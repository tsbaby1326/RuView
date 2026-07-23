//! Browser sign-in: `GET /oauth/start` → Cognitum → `GET /oauth/callback`
//! → a signed session cookie (ADR-271, browser half).
//!
//! # Why this exists
//!
//! `wifi-densepose login` writes `~/.ruview/credentials.json`. **A browser
//! cannot read that file.** So until now the UI had no way to obtain a Cognitum
//! token at all — the WebSocket ticket mechanism ADR-272 built "for browsers"
//! was only exercisable with the legacy static shared secret that OAuth was
//! meant to replace. An adversarial review found the gap; this closes it.
//!
//! # The pattern, ported from `cognitum-one/freetokens`
//!
//! freetokens (`src/auth/oauth.ts`, live at `freetokens.cognitum.one`) solves
//! exactly this, and the shape is worth stating because it is not the obvious
//! one:
//!
//! **The browser never holds an OAuth token.** The server generates the PKCE
//! verifier and state, keeps them in a signed cookie, performs the code
//! exchange itself, verifies the token, and then issues *its own* session
//! cookie. The access token never reaches page JavaScript, so it cannot be
//! read by an XSS, stored in `localStorage`, or leaked through a URL.
//!
//! # Deviation from freetokens, and why
//!
//! freetokens uses the `__Host-` cookie prefix, which **requires** the `Secure`
//! attribute. It is served only over HTTPS, so that is free. RuView is
//! routinely reached over plain HTTP on a LAN or at `http://localhost`, where a
//! `__Host-`/`Secure` cookie is simply never sent and sign-in would silently
//! fail. So the names carry no prefix and `Secure` is set only when the request
//! arrived over TLS. Every other attribute — `HttpOnly`, `SameSite=Lax`,
//! `Path=/` — matches, and the signature is what actually protects the value.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Signing key for both cookies. Absent ⇒ browser sign-in is unavailable and
/// `/oauth/start` answers 503, rather than issuing cookies nobody can verify.
pub const SESSION_SECRET_ENV: &str = "RUVIEW_SESSION_SECRET";

/// Public origin this server is reached at, used to build `redirect_uri`.
/// Must match the value registered for the `ruview` OAuth client.
pub const PUBLIC_BASE_URL_ENV: &str = "RUVIEW_PUBLIC_BASE_URL";

const TXN_COOKIE: &str = "ruview_oauth_txn";
const SESSION_COOKIE: &str = "ruview_session";

/// The OAuth round-trip is a page load or two. Ten minutes is generous.
const TXN_TTL_SECS: i64 = 600;
/// How long a browser stays signed in before repeating the redirect.
const SESSION_TTL_SECS: i64 = 12 * 3600;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn b64(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.decode(s).ok()
}

/// `<payload-b64>.<hmac-b64>`.
fn sign(payload: &[u8], secret: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac accepts any key size");
    let body = b64(payload);
    mac.update(body.as_bytes());
    format!("{body}.{}", b64(&mac.finalize().into_bytes()))
}

/// Verify and unwrap. Constant-time tag comparison — a byte-at-a-time compare
/// on a MAC is a forgery oracle.
fn unsign(value: &str, secret: &str) -> Option<Vec<u8>> {
    let sep = value.rfind('.')?;
    let (body, tag) = (&value[..sep], &value[sep + 1..]);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(body.as_bytes());
    let expected = b64(&mac.finalize().into_bytes());
    if expected.as_bytes().ct_eq(tag.as_bytes()).into() {
        unb64(body)
    } else {
        None
    }
}

/// What the transaction cookie carries between `/oauth/start` and the callback.
#[derive(serde::Serialize, serde::Deserialize)]
struct Transaction {
    state: String,
    verifier: String,
    exp: i64,
}

/// What the session cookie carries after a successful sign-in.
///
/// Deliberately NOT the access token. The browser gets an assertion that this
/// server already verified one — nothing replayable elsewhere.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BrowserSession {
    pub subject: String,
    pub account_id: String,
    pub scope: String,
    pub exp: i64,
}

impl BrowserSession {
    pub fn is_live(&self) -> bool {
        self.exp > now()
    }
    pub fn has_scope(&self, want: &str) -> bool {
        self.scope.split_whitespace().any(|s| s == want)
    }
}

fn cookie(name: &str, value: &str, max_age: i64, secure: bool) -> String {
    format!(
        "{name}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}

/// Read one cookie from a raw `Cookie:` header.
pub fn read_cookie(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("browser sign-in is not configured on this server")]
    NotConfigured,
    #[error("the sign-in request is missing or has expired")]
    InvalidTransaction,
    #[error("the sign-in state did not match — this response did not come from the flow that started")]
    StateMismatch,
    #[error("Cognitum sign-in could not be completed: {0}")]
    ExchangeFailed(String),
    #[error("Cognitum returned a token this server will not accept: {0}")]
    InvalidToken(String),
}

/// Process-wide secret, resolved once.
static SECRET: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Resolve the signing secret: env first, then a persisted file, then generate.
///
/// Requiring an operator to invent a secret before browser sign-in works is a
/// footgun — they set `RUVIEW_OAUTH_ISSUER`, expect sign-in, and get a 503 that
/// names an env var they have never heard of. A single-host appliance has no
/// reason to need that step, so we generate one and persist it `0600` next to
/// the server's other state.
///
/// Persisted rather than in-memory so a restart does not silently sign everyone
/// out. The env var still wins, which is what a multi-instance deployment needs
/// — several servers must share a secret or a session issued by one is
/// rejected by the next.
pub fn init_secret(data_dir: &Path) {
    let resolved = std::env::var(SESSION_SECRET_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            tracing::info!("browser session secret: from {SESSION_SECRET_ENV}");
            s
        })
        .or_else(|| load_or_create_secret(data_dir));
    let _ = SECRET.set(resolved);
}

fn load_or_create_secret(data_dir: &Path) -> Option<String> {
    let path = data_dir.join("session-secret");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            tracing::info!(path = %path.display(), "browser session secret: loaded");
            return Some(trimmed);
        }
    }
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let generated = b64(&bytes);
    if let Err(e) = write_secret(&path, &generated) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "could not persist a browser session secret; sessions will not survive a restart. \
             Set {SESSION_SECRET_ENV} to fix this permanently."
        );
        // Still usable this run — better than refusing sign-in outright.
        return Some(generated);
    }
    tracing::info!(path = %path.display(), "browser session secret: generated");
    Some(generated)
}

fn write_secret(path: &Path, value: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    // Created 0600, not written-then-chmodded. `fs::write` creates at
    // `0666 & !umask`, so this file — the HMAC key for EVERY browser session —
    // was world-readable for the window before the chmod. Anyone who read it
    // could forge a session cookie for any account with any scope, including
    // `sensing:admin`, which is strictly worse than stealing one session.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(value.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, value)?;
    std::fs::rename(&tmp, path)
}

fn secret() -> Result<String, SessionError> {
    SECRET
        .get()
        .and_then(|s| s.clone())
        .ok_or(SessionError::NotConfigured)
}

/// Is browser sign-in usable on this server?
pub fn is_configured() -> bool {
    secret().is_ok()
}

/// Begin sign-in: where to redirect, and the cookie to set.
pub fn begin(issuer: &str, client_id: &str, scope: &str, secure: bool) -> Result<(String, String), SessionError> {
    let secret = secret()?;
    let req = ruview_auth::pkce::generate();
    let txn = Transaction {
        state: req.state.clone(),
        verifier: req.code_verifier,
        exp: now() + TXN_TTL_SECS,
    };
    let payload = serde_json::to_vec(&txn).expect("transaction serializes");

    let mut url = url_encode_authorize(issuer, client_id, scope, &req.state, &req.code_challenge);
    url.push_str(""); // no-op; keeps the builder readable

    Ok((
        url,
        cookie(TXN_COOKIE, &sign(&payload, &secret), TXN_TTL_SECS, secure),
    ))
}

fn url_encode_authorize(
    issuer: &str,
    client_id: &str,
    scope: &str,
    state: &str,
    challenge: &str,
) -> String {
    // Percent-encode every value: `scope` legitimately contains a space
    // ("sensing:read sensing:admin") and hand-formatting silently truncates it.
    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    format!(
        "{}/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        issuer.trim_end_matches('/'),
        enc(client_id),
        enc(&redirect_uri()),
        enc(scope),
        enc(state),
        enc(challenge),
    )
}

pub fn public_base_url() -> String {
    std::env::var(PUBLIC_BASE_URL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string())
}

pub fn redirect_uri() -> String {
    format!("{}/oauth/callback", public_base_url())
}

/// Cookie that clears the transaction.
pub fn clear_transaction(secure: bool) -> String {
    cookie(TXN_COOKIE, "", 0, secure)
}

/// Cookie that ends the session.
pub fn clear_session(secure: bool) -> String {
    cookie(SESSION_COOKIE, "", 0, secure)
}

/// Validate the callback's `state` against the transaction cookie and return
/// the PKCE verifier needed for the exchange.
pub fn verifier_for_callback(cookie_header: &str, state: &str) -> Result<String, SessionError> {
    let secret = secret()?;
    let raw = read_cookie(cookie_header, TXN_COOKIE).ok_or(SessionError::InvalidTransaction)?;
    let bytes = unsign(&raw, &secret).ok_or(SessionError::InvalidTransaction)?;
    let txn: Transaction =
        serde_json::from_slice(&bytes).map_err(|_| SessionError::InvalidTransaction)?;
    if txn.exp < now() {
        return Err(SessionError::InvalidTransaction);
    }
    // CSRF: constant-time, and BEFORE the code is spent.
    let ok: bool = txn.state.as_bytes().ct_eq(state.as_bytes()).into();
    if !ok {
        return Err(SessionError::StateMismatch);
    }
    Ok(txn.verifier)
}

/// Issue the session cookie for a verified principal.
pub fn issue(principal: &ruview_auth::Principal, secure: bool) -> Result<String, SessionError> {
    let secret = secret()?;
    let session = BrowserSession {
        subject: principal.subject.clone(),
        account_id: principal.account_id.clone(),
        scope: principal.scopes().collect::<Vec<_>>().join(" "),
        // Never outlive our own ceiling, and never inherit the access token's
        // 15 minutes either — this is a browser session, not the token.
        exp: now() + SESSION_TTL_SECS,
    };
    let payload = serde_json::to_vec(&session).expect("session serializes");
    Ok(cookie(
        SESSION_COOKIE,
        &sign(&payload, &secret),
        SESSION_TTL_SECS,
        secure,
    ))
}

/// Recover a live session from a request's `Cookie:` header.
pub fn from_cookie_header(cookie_header: &str) -> Option<BrowserSession> {
    let secret = secret().ok()?;
    let raw = read_cookie(cookie_header, SESSION_COOKIE)?;
    let bytes = unsign(&raw, &secret)?;
    let session: BrowserSession = serde_json::from_slice(&bytes).ok()?;
    session.is_live().then_some(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-secret-value";

    fn session(exp: i64) -> BrowserSession {
        BrowserSession {
            subject: "user-1".into(),
            account_id: "acct-1".into(),
            scope: "sensing:read".into(),
            exp,
        }
    }

    #[test]
    fn a_signed_value_round_trips() {
        let signed = sign(b"hello", SECRET);
        assert_eq!(unsign(&signed, SECRET).as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        // The whole point of signing: the browser holds this value and can edit
        // it. Flipping a byte must invalidate the tag.
        let signed = sign(b"hello", SECRET);
        let (body, tag) = signed.split_once('.').unwrap();
        let mut bad = body.to_string();
        bad.push('x');
        assert!(unsign(&format!("{bad}.{tag}"), SECRET).is_none());
    }

    #[test]
    fn a_value_signed_with_another_secret_is_rejected() {
        let signed = sign(b"hello", "a-different-secret");
        assert!(unsign(&signed, SECRET).is_none());
    }

    #[test]
    fn a_malformed_cookie_value_is_rejected_rather_than_panicking() {
        for bad in ["", ".", "no-separator", "!!!.!!!", "a.b.c"] {
            assert!(unsign(bad, SECRET).is_none(), "{bad:?} must not verify");
        }
    }

    #[test]
    fn cookies_are_httponly_and_samesite_lax() {
        // HttpOnly is what keeps page JavaScript — and therefore an XSS — away
        // from the session.
        let c = cookie("n", "v", 600, false);
        assert!(c.contains("HttpOnly"), "{c}");
        assert!(c.contains("SameSite=Lax"), "{c}");
        assert!(c.contains("Path=/"), "{c}");
        assert!(!c.contains("Secure"), "plain HTTP must not set Secure: {c}");
    }

    #[test]
    fn secure_is_set_only_over_tls() {
        assert!(cookie("n", "v", 600, true).contains("; Secure"));
    }

    #[test]
    fn a_session_cookie_never_contains_the_access_token() {
        // The core property of this design: the browser holds an assertion,
        // not a credential it could replay against Cognitum or another service.
        let payload = serde_json::to_vec(&session(now() + 3600)).unwrap();
        let rendered = sign(&payload, SECRET);
        let decoded = String::from_utf8(unsign(&rendered, SECRET).unwrap()).unwrap();
        assert!(!decoded.contains("eyJ"), "looks like a JWT: {decoded}");
        assert!(decoded.contains("user-1") && decoded.contains("sensing:read"));
    }

    #[test]
    fn an_expired_session_is_not_live() {
        assert!(!session(now() - 1).is_live());
        assert!(session(now() + 60).is_live());
    }

    #[test]
    fn session_scope_matching_is_exact() {
        let s = session(now() + 60);
        assert!(s.has_scope("sensing:read"));
        assert!(!s.has_scope("sensing:admin"), "no implied escalation");
        assert!(!s.has_scope("sensing"), "prefixes must not match");
    }

    #[test]
    fn reads_a_named_cookie_out_of_a_header() {
        let h = "foo=1; ruview_session=abc.def; bar=2";
        assert_eq!(read_cookie(h, "ruview_session").as_deref(), Some("abc.def"));
        assert_eq!(read_cookie(h, "absent"), None);
    }

    #[test]
    fn a_cookie_name_that_merely_ends_with_the_target_is_not_matched() {
        // `xruview_session=` must not be read as `ruview_session=`.
        assert_eq!(read_cookie("xruview_session=v", "ruview_session"), None);
    }

    #[test]
    fn the_authorize_url_encodes_a_multi_scope_request() {
        let u = url_encode_authorize(
            "https://auth.cognitum.one",
            "ruview",
            "sensing:read sensing:admin",
            "st",
            "ch",
        );
        assert!(u.starts_with("https://auth.cognitum.one/oauth/authorize"));
        assert!(u.contains("client_id=ruview"));
        assert!(u.contains("code_challenge_method=S256"));
        assert!(
            u.contains("scope=sensing%3Aread%20sensing%3Aadmin"),
            "space must be encoded, not truncated: {u}"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_issuer_does_not_double_up() {
        let u = url_encode_authorize("https://auth.cognitum.one/", "ruview", "s", "st", "ch");
        assert!(!u.contains(".one//oauth"), "{u}");
    }
}

/// Regression guard for a response-shape mistake that silently broke sign-in.
#[cfg(test)]
mod response_shape_tests {
    use axum::response::IntoResponse;

    /// Axum's array-of-tuples form REPLACES same-name headers. Two `Set-Cookie`
    /// entries collapse to one — which, on the sign-in callback, dropped the
    /// session cookie and made a successful OAuth round-trip a no-op. Only the
    /// last cookie survived.
    #[test]
    fn an_array_of_headers_silently_drops_a_duplicate_set_cookie() {
        let resp = (
            axum::http::StatusCode::FOUND,
            [
                (axum::http::header::SET_COOKIE, "a=1".to_string()),
                (axum::http::header::SET_COOKIE, "b=2".to_string()),
            ],
        )
            .into_response();
        assert_eq!(
            resp.headers()
                .get_all(axum::http::header::SET_COOKIE)
                .iter()
                .count(),
            1,
            "documenting the footgun: the array form replaces, it does not append"
        );
    }

    /// `AppendHeaders` is what actually emits both.
    #[test]
    fn append_headers_emits_every_set_cookie() {
        let resp = (
            axum::http::StatusCode::FOUND,
            axum::response::AppendHeaders([
                (axum::http::header::LOCATION, "/ui/".to_string()),
                (axum::http::header::SET_COOKIE, "a=1".to_string()),
                (axum::http::header::SET_COOKIE, "b=2".to_string()),
            ]),
        )
            .into_response();
        let cookies: Vec<_> = resp
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(cookies.len(), 2, "both cookies must reach the browser");
        assert!(cookies.contains(&"a=1") && cookies.contains(&"b=2"));
        assert!(resp.headers().get(axum::http::header::LOCATION).is_some());
    }
}
