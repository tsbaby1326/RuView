//! Opt-in bearer-token auth for the sensing-server HTTP API (#443).
//!
//! When the `RUVIEW_API_TOKEN` environment variable is set, every request
//! whose path begins with `/api/v1/` must carry a matching
//! `Authorization: Bearer <token>` header, otherwise the server responds with
//! `401 Unauthorized`. When the env var is unset (or empty), the middleware is
//! a no-op and the API stays unauthenticated — preserving the long-standing
//! LAN-only deployment posture documented in the issue. This is a binary,
//! deployment-time switch with **no default authentication change**.
//!
//! Endpoints outside `/api/v1/*` (`/health*`, `/ws/sensing`, the static `/ui/*`
//! mount, `/`) are intentionally **not** gated:
//! * `/health*` is the liveness/readiness probe that orchestrators hit
//!   anonymously;
//! * `/ws/sensing` and `/ui/*` are served to local browsers that can't easily
//!   inject headers — the sensitive control plane is the `/api/v1/*` tree, and
//!   that is what this layer protects.
//!
//! The header check uses a length-then-byte constant-time compare to avoid
//! leaking the token through timing.
//!
//! # Cognitum OAuth (ADR-271)
//!
//! A second, **additive** credential is supported: a Cognitum OAuth access
//! token, verified offline against `auth.cognitum.one`'s published JWKS. It is
//! enabled by setting [`OAUTH_ISSUER_ENV`], and the two schemes layer:
//!
//! 1. If `RUVIEW_API_TOKEN` is set and the presented bearer matches it exactly,
//!    the request is allowed — byte-for-byte today's behaviour.
//! 2. Otherwise, if OAuth is configured, the bearer is verified as a JWT and
//!    must carry the scope the route requires.
//! 3. Otherwise `401`.
//!
//! Order matters for compatibility, not for security: a static token that
//! matches is not a JWT, and a JWT never matches the static token. Trying the
//! static compare first means an existing deployment's behaviour is unchanged
//! even with OAuth switched on.
//!
//! **Nothing here weakens the unset case.** With neither variable set the
//! middleware is the same no-op it has always been.
//!
//! ## Scope gating
//!
//! Not every route carries the same blast radius, so a single "authenticated"
//! bit is too coarse once we have scopes. [`required_scope_for`] maps a request
//! to `sensing:read` or `sensing:admin` — see its docs for the split and why it
//! is drawn where it is.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use ruview_auth::{scope, verify_access_token, JwksCache, UreqFetcher, VerifierConfig};

/// Environment variable that gates the middleware. Unset / empty ⇒ auth off.
pub const API_TOKEN_ENV: &str = "RUVIEW_API_TOKEN";

/// Issuer origin of the Cognitum authorization server. Setting this enables
/// OAuth verification; unset ⇒ OAuth off and behaviour is unchanged.
pub const OAUTH_ISSUER_ENV: &str = "RUVIEW_OAUTH_ISSUER";

/// Optional JWKS override. Defaults to `<issuer>/.well-known/jwks.json`, which
/// is where RFC 8414 metadata points for `auth.cognitum.one`. Overridable so a
/// staging issuer or an air-gapped mirror can be pointed at without a rebuild.
pub const OAUTH_JWKS_URL_ENV: &str = "RUVIEW_OAUTH_JWKS_URL";

/// The production Cognitum issuer, for operators who just want it on.
pub const COGNITUM_ISSUER: &str = "https://auth.cognitum.one";

/// Path prefix the middleware protects when auth is enabled.
pub const PROTECTED_PREFIX: &str = "/api/v1/";

/// `/api/v1/stream/pose` is a WebSocket upgrade endpoint reachable from
/// browser code. Unlike a plain fetch(), the browser `WebSocket` constructor
/// cannot attach an `Authorization` header to the handshake request, so this
/// path can never carry a bearer token from a stock browser client — the
/// same reasoning that already exempts `/ws/sensing` (see module docs).
/// Exempted here rather than moved out of `/api/v1/*` to avoid an API
/// surface change for existing clients.
const EXEMPT_PATHS: &[&str] = &["/api/v1/stream/pose"];

/// Cognitum OAuth verification state. Built once at boot and shared.
pub struct OAuthState {
    jwks: JwksCache,
    issuer: String,
}

impl std::fmt::Debug for OAuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthState")
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

/// Why OAuth could not be configured. Every variant is fatal at boot — see
/// [`AuthState::from_env`]'s contract about failing closed.
#[derive(Debug, thiserror::Error)]
pub enum OAuthConfigError {
    #[error("{OAUTH_ISSUER_ENV} is set but empty")]
    EmptyIssuer,
    #[error("JWKS at {url} is unreachable, so no token could ever be verified: {source}")]
    JwksUnreachable {
        url: String,
        #[source]
        source: ruview_auth::JwksError,
    },
}

/// Cheap, cloneable handle to the configured credentials.
#[derive(Debug, Clone, Default)]
pub struct AuthState {
    /// The expected static bearer token, if any.
    token: Option<Arc<String>>,
    /// Cognitum OAuth verification, if enabled.
    oauth: Option<Arc<OAuthState>>,
}

impl AuthState {
    /// Build an [`AuthState`] from an explicit string. Empty ⇒ disabled.
    pub fn from_token(t: impl Into<String>) -> Self {
        let s = t.into();
        if s.is_empty() {
            AuthState::default()
        } else {
            AuthState {
                token: Some(Arc::new(s)),
                oauth: None,
            }
        }
    }

    /// Read the auth configuration from the process environment.
    ///
    /// **Fails closed.** If OAuth is requested but cannot be made to work — the
    /// issuer is empty, or the JWKS cannot be fetched at boot — this returns
    /// `Err` and the caller must refuse to serve `/api/v1/*`. Starting anyway
    /// would mean an operator who asked for OAuth silently gets either an open
    /// API or a single-shared-secret one, which is precisely the failure mode
    /// that makes people distrust an auth switch.
    ///
    /// The JWKS is fetched eagerly for the same reason: a misconfigured
    /// `jwks_uri` should fail at boot with a legible message, not as a puzzling
    /// 401 on some user's first request an hour later.
    pub fn from_env() -> Result<Self, OAuthConfigError> {
        let token = match std::env::var(API_TOKEN_ENV) {
            Ok(s) if !s.is_empty() => Some(Arc::new(s)),
            _ => None,
        };

        let oauth = match std::env::var(OAUTH_ISSUER_ENV) {
            Ok(issuer) if !issuer.trim().is_empty() => {
                let issuer = issuer.trim().trim_end_matches('/').to_string();
                let jwks_url = std::env::var(OAUTH_JWKS_URL_ENV)
                    .ok()
                    .filter(|u| !u.trim().is_empty())
                    .unwrap_or_else(|| format!("{issuer}/.well-known/jwks.json"));

                let jwks = JwksCache::new(jwks_url.clone(), Box::new(UreqFetcher::new()));
                let key_count =
                    jwks.warm()
                        .map_err(|source| OAuthConfigError::JwksUnreachable {
                            url: jwks_url.clone(),
                            source,
                        })?;
                tracing::info!(
                    issuer = %issuer,
                    jwks_url = %jwks_url,
                    key_count,
                    "Cognitum OAuth enabled for /api/v1/*"
                );
                Some(Arc::new(OAuthState { jwks, issuer }))
            }
            Ok(_) => return Err(OAuthConfigError::EmptyIssuer),
            Err(_) => None,
        };

        Ok(AuthState { token, oauth })
    }

    /// Whether the middleware will enforce auth on `/api/v1/*` requests.
    pub fn is_enabled(&self) -> bool {
        self.token.is_some() || self.oauth.is_some()
    }

    /// Whether Cognitum OAuth verification is active.
    pub fn oauth_enabled(&self) -> bool {
        self.oauth.is_some()
    }

    /// Whether the legacy static `RUVIEW_API_TOKEN` is configured.
    pub fn static_token_enabled(&self) -> bool {
        self.token.is_some()
    }
}

/// The scope a request must carry, split by **blast radius** (ADR-060): can
/// this call destroy something, or only observe?
///
/// `sensing:admin` covers exactly three things:
/// * `/api/v1/train/*` — burns hours of CPU on a Pi and writes models;
/// * `DELETE /api/v1/models/{id}` — irreversible loss of a trained model;
/// * `DELETE /api/v1/recording/{id}` — irreversible loss of a labelled capture.
///
/// Everything else is `sensing:read`. Note what is deliberately *not* admin:
/// loading/unloading a model and starting a recording both mutate server state
/// but destroy nothing, and putting them behind the destructive scope would
/// push routine dashboard use into asking for a capability it does not need —
/// the opposite of least privilege.
///
/// `sensing:read` is not "harmless": for a presence and vital-signs sensor,
/// read access tells the holder who is home. It is *non-destructive*, which is
/// a weaker claim.
pub fn required_scope_for(method: &Method, path: &str) -> &'static str {
    if path.starts_with("/api/v1/train/") {
        return scope::SENSING_ADMIN;
    }
    if method == Method::DELETE
        && (path.starts_with("/api/v1/models/") || path.starts_with("/api/v1/recording/"))
    {
        return scope::SENSING_ADMIN;
    }
    scope::SENSING_READ
}

/// Constant-time byte slice equality. Returns `false` immediately on length
/// mismatch (lengths are not secret here — both sides are fixed tokens).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Axum middleware: enforces `Authorization: Bearer <token>` on `/api/v1/*`
/// requests when [`AuthState::is_enabled`] returns `true`. Wires up via
/// [`axum::middleware::from_fn_with_state`].
pub async fn require_bearer(
    State(auth): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    if !auth.is_enabled() {
        return next.run(request).await;
    }
    let path = request.uri().path();
    if !path.starts_with(PROTECTED_PREFIX) || EXEMPT_PATHS.contains(&path) {
        return next.run(request).await;
    }

    let Some(supplied) = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        // RFC 6750 §2.1 / RFC 7235 §2.1: the auth-scheme ("Bearer") is
        // case-insensitive. Match it as such (and tolerate extra leading
        // whitespace before the token) so a correct token isn't rejected
        // just because a client sent `bearer`/`BEARER`. The token compare
        // below stays exact + constant-time.
        .and_then(|s| {
            let (scheme, token) = s.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("Bearer")
                .then(|| token.trim_start())
        })
    else {
        return unauthorized(&auth);
    };

    // 1. Legacy static token. Unchanged, and tried first so an existing
    //    deployment behaves identically even with OAuth switched on.
    if let Some(expected) = auth.token.as_ref() {
        if ct_eq(supplied.as_bytes(), expected.as_bytes()) {
            return next.run(request).await;
        }
    }

    // 2. Cognitum OAuth (ADR-271).
    if let Some(oauth) = auth.oauth.as_ref() {
        let required = required_scope_for(request.method(), path);
        let config = VerifierConfig {
            issuer: oauth.issuer.clone(),
            required_scope: required.to_string(),
        };
        match verify_access_token(supplied, &oauth.jwks, &config) {
            Ok(principal) => {
                tracing::debug!(
                    sub = %principal.subject,
                    account_id = %principal.account_id,
                    client_id = %principal.client_id,
                    jti = %principal.token_id,
                    scope = %required,
                    path = %path,
                    "OAuth request authorized"
                );
                // Downstream handlers can attribute the request without
                // re-parsing the token.
                request.extensions_mut().insert(principal);
                return next.run(request).await;
            }
            Err(e) => {
                // Logged, never returned: the reason a token failed is useful
                // to an operator and useful to an attacker probing for which
                // claim to forge next. The response stays a flat 401.
                tracing::debug!(error = %e, path = %path, required_scope = %required, "OAuth verification failed");
                return unauthorized(&auth);
            }
        }
    }

    unauthorized(&auth)
}

/// A uniform 401. The hint names whichever credentials are actually accepted,
/// so an operator is not told to set a variable this server ignores — but it
/// never says *why* a presented token failed.
fn unauthorized(auth: &AuthState) -> Response {
    let body = match (auth.token.is_some(), auth.oauth.is_some()) {
        (true, true) => concat!(
            "missing or invalid bearer token\n",
            "accepted: Authorization: Bearer <RUVIEW_API_TOKEN>, ",
            "or a Cognitum OAuth access token with the scope this route requires\n"
        ),
        (false, true) => concat!(
            "missing or invalid bearer token\n",
            "accepted: a Cognitum OAuth access token with the scope this route requires\n"
        ),
        _ => "missing or invalid bearer token (set Authorization: Bearer <RUVIEW_API_TOKEN>)\n",
    };
    (StatusCode::UNAUTHORIZED, body).into_response()
}

/// Convenience re-export so handlers can name the type they pull out of
/// request extensions without depending on `ruview-auth` directly.
pub use ruview_auth::Principal as AuthenticatedPrincipal;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        routing::get,
        Router,
    };
    use tower::ServiceExt;

    fn ok_handler() -> Router {
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/api/v1/info", get(|| async { "ok" }))
            .route("/api/v1/sensitive", axum::routing::post(|| async { "ok" }))
            .route("/api/v1/stream/pose", get(|| async { "ok" }))
            .route("/ui/index.html", get(|| async { "<html/>" }))
    }

    fn wrap(auth: AuthState) -> Router {
        ok_handler().layer(axum::middleware::from_fn_with_state(auth, require_bearer))
    }

    async fn status(router: Router, method: &str, path: &str, auth: Option<&str>) -> StatusCode {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        if let Some(t) = auth {
            req.headers_mut()
                .insert(AUTHORIZATION, format!("Bearer {t}").parse().unwrap());
        }
        router.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn middleware_is_no_op_when_token_unset() {
        let r = wrap(AuthState::default());
        assert_eq!(
            status(r.clone(), "GET", "/api/v1/info", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(r.clone(), "POST", "/api/v1/sensitive", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(r.clone(), "GET", "/health", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(r, "GET", "/ui/index.html", None).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn enabled_blocks_api_without_bearer() {
        let r = wrap(AuthState::from_token("s3cr3t"));
        assert_eq!(
            status(r.clone(), "GET", "/api/v1/info", None).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(r, "POST", "/api/v1/sensitive", None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn accepts_case_insensitive_bearer_scheme() {
        // RFC 6750 §2.1 / RFC 7235 §2.1: the auth-scheme is case-insensitive.
        // A correct token must authenticate regardless of scheme casing or
        // extra whitespace; a wrong token must still be rejected.
        async fn req_status(auth_value: &str) -> StatusCode {
            let r = wrap(AuthState::from_token("s3cr3t"));
            let mut req = Request::builder()
                .method("GET")
                .uri("/api/v1/info")
                .body(Body::empty())
                .unwrap();
            req.headers_mut()
                .insert(AUTHORIZATION, auth_value.parse().unwrap());
            r.oneshot(req).await.unwrap().status()
        }
        assert_eq!(req_status("Bearer s3cr3t").await, StatusCode::OK);
        assert_eq!(req_status("bearer s3cr3t").await, StatusCode::OK);
        assert_eq!(req_status("BEARER s3cr3t").await, StatusCode::OK);
        assert_eq!(req_status("Bearer  s3cr3t").await, StatusCode::OK); // extra space
        // Scheme leniency must NOT weaken the token check.
        assert_eq!(req_status("bearer nope").await, StatusCode::UNAUTHORIZED);
        assert_eq!(req_status("Basic s3cr3t").await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn enabled_blocks_api_with_wrong_bearer() {
        let r = wrap(AuthState::from_token("s3cr3t"));
        assert_eq!(
            status(r.clone(), "GET", "/api/v1/info", Some("nope")).await,
            StatusCode::UNAUTHORIZED
        );
        // Wrong scheme (Basic / token) — only "Bearer <token>" is accepted.
        let mut req = Request::builder()
            .method("GET")
            .uri("/api/v1/info")
            .body(Body::empty())
            .unwrap();
        req.headers_mut()
            .insert(AUTHORIZATION, "Basic s3cr3t".parse().unwrap());
        assert_eq!(
            r.oneshot(req).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn enabled_allows_api_with_correct_bearer() {
        let r = wrap(AuthState::from_token("s3cr3t"));
        assert_eq!(
            status(r.clone(), "GET", "/api/v1/info", Some("s3cr3t")).await,
            StatusCode::OK
        );
        assert_eq!(
            status(r, "POST", "/api/v1/sensitive", Some("s3cr3t")).await,
            StatusCode::OK
        );
    }

    /// REGRESSION (ADR-080 #3, CWE-598 — token in URL query string).
    ///
    /// ADR-080 flagged "JWT in URL" as a HIGH finding (tokens in query strings
    /// leak into logs, proxies, browser history, `Referer`). The current
    /// sensing-server only ever reads the token from the `Authorization: Bearer`
    /// header — there is no `?token=` / `?access_token=` query path in
    /// `require_bearer` (see [`require_bearer`] above, which only inspects the
    /// `AUTHORIZATION` header). This test pins that: a request carrying the
    /// correct token *only* in the query string is still `401`, while the same
    /// token in the header is `200`. If anyone ever re-introduces a query-string
    /// token path, this fails.
    #[tokio::test]
    async fn query_string_token_is_never_accepted() {
        let r = wrap(AuthState::from_token("s3cr3t"));
        // Correct token, but supplied only in the URL — must NOT authenticate.
        assert_eq!(
            status(r.clone(), "GET", "/api/v1/info?token=s3cr3t", None).await,
            StatusCode::UNAUTHORIZED,
            "?token= in the query string must not authenticate (CWE-598)"
        );
        assert_eq!(
            status(
                r.clone(),
                "GET",
                "/api/v1/info?access_token=s3cr3t",
                None
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "?access_token= in the query string must not authenticate (CWE-598)"
        );
        // A query token must not "help" a request that also lacks the header,
        // even combined with an unrelated param.
        assert_eq!(
            status(
                r.clone(),
                "GET",
                "/api/v1/info?foo=bar&token=s3cr3t",
                None
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
        // The header path is the only accepted channel — same token, header,
        // succeeds. (Proves we didn't just break auth entirely.)
        assert_eq!(
            status(r, "GET", "/api/v1/info?token=s3cr3t", Some("s3cr3t")).await,
            StatusCode::OK,
            "the Authorization: Bearer header is the supported channel"
        );
    }

    /// REGRESSION (ADR-080 #1 — X-Forwarded-For spoofing).
    ///
    /// The bearer middleware authenticates on the token alone and must be
    /// completely insensitive to a client-supplied `X-Forwarded-For` header:
    /// an attacker cannot flip an auth decision by spoofing XFF. A wrong token
    /// stays `401` and a right token stays `200` regardless of XFF. (The
    /// sensing-server has no IP-based rate-limit / allowlist that XFF could
    /// bypass; this locks in that auth itself never consults XFF.)
    #[tokio::test]
    async fn xff_header_never_affects_auth_decision() {
        let r = wrap(AuthState::from_token("s3cr3t"));
        async fn with_xff(router: Router, token: Option<&str>, xff: &str) -> StatusCode {
            let mut req = Request::builder()
                .method("GET")
                .uri("/api/v1/info")
                .header("X-Forwarded-For", xff)
                .body(Body::empty())
                .unwrap();
            if let Some(t) = token {
                req.headers_mut()
                    .insert(AUTHORIZATION, format!("Bearer {t}").parse().unwrap());
            }
            router.oneshot(req).await.unwrap().status()
        }
        // Spoofed XFF + no/ wrong token ⇒ still rejected.
        assert_eq!(
            with_xff(r.clone(), None, "127.0.0.1").await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            with_xff(r.clone(), Some("nope"), "10.0.0.1, 127.0.0.1").await,
            StatusCode::UNAUTHORIZED
        );
        // Spoofed XFF + correct token ⇒ still accepted (XFF is irrelevant).
        assert_eq!(
            with_xff(r, Some("s3cr3t"), "evil-proxy").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn enabled_never_gates_paths_outside_api_v1() {
        let r = wrap(AuthState::from_token("s3cr3t"));
        // Even with auth ON, `/health` and `/ui/*` are reachable without a token:
        // orchestrator probes and the local UI need to load unchallenged.
        assert_eq!(
            status(r.clone(), "GET", "/health", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(r, "GET", "/ui/index.html", None).await,
            StatusCode::OK
        );
    }

    /// `/api/v1/stream/pose` is a WebSocket upgrade the browser `WebSocket`
    /// constructor drives directly — it cannot attach an `Authorization`
    /// header, so this path must stay reachable even with auth ON (mirrors
    /// the existing `/ws/sensing` exemption, just inside the `/api/v1/*`
    /// prefix this time).
    #[tokio::test]
    async fn enabled_exempts_pose_stream_websocket() {
        let r = wrap(AuthState::from_token("s3cr3t"));
        assert_eq!(
            status(r.clone(), "GET", "/api/v1/stream/pose", None).await,
            StatusCode::OK,
            "pose stream WS must stay reachable without a bearer token"
        );
        // The exemption is narrow: it must not leak to other /api/v1/* paths.
        assert_eq!(
            status(r, "GET", "/api/v1/info", None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn ct_eq_basics() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab")); // length mismatch
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn from_env_treats_empty_as_disabled() {
        // Avoid touching the real env in a thread-shared test — exercise the
        // string ctor directly with the same trim logic.
        assert!(!AuthState::from_token("").is_enabled());
        assert!(AuthState::from_token("x").is_enabled());
    }

    #[test]
    fn protected_prefix_and_env_constants_are_stable() {
        // These are documented in the issue body and the README; keep them locked.
        assert_eq!(API_TOKEN_ENV, "RUVIEW_API_TOKEN");
        assert_eq!(PROTECTED_PREFIX, "/api/v1/");
    }
}

/// ADR-271 — the OAuth path and the scope gate, exercised end to end through a
/// real Router: request → middleware → `ruview-auth` verifier → handler.
///
/// Tokens are real ES256 JWTs signed with a key generated at test runtime; no
/// key material is committed. The verifier's own accept/reject matrix lives in
/// `ruview-auth`; what is tested here is the wiring — layering with the legacy
/// static token, which scope each route demands, and that a rejected token
/// never reaches a handler.
#[cfg(test)]
mod oauth_tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        routing::{delete, get, post},
        Router,
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use jsonwebtoken::{encode, EncodingKey, Header};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};
    use ruview_auth::jwks::{JwksError, JwksFetcher};
    use std::sync::OnceLock;
    use tower::ServiceExt;

    const KID: &str = "test-kid";
    const ISSUER: &str = "https://auth.test.local";

    struct TestKey {
        pem: String,
        x: String,
        y: String,
    }

    fn key() -> &'static TestKey {
        static K: OnceLock<TestKey> = OnceLock::new();
        K.get_or_init(|| {
            let sk = SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
            let point = sk.verifying_key().to_encoded_point(false);
            TestKey {
                pem: sk.to_pkcs8_pem(LineEnding::LF).unwrap().to_string(),
                x: URL_SAFE_NO_PAD.encode(point.x().unwrap()),
                y: URL_SAFE_NO_PAD.encode(point.y().unwrap()),
            }
        })
    }

    struct StaticJwks(String);
    impl JwksFetcher for StaticJwks {
        fn fetch(&self, _url: &str) -> Result<String, JwksError> {
            Ok(self.0.clone())
        }
    }

    fn oauth_state() -> Arc<OAuthState> {
        let k = key();
        let doc = format!(
            r#"{{"keys":[{{"kty":"EC","crv":"P-256","alg":"ES256","use":"sig","kid":"{KID}","x":"{}","y":"{}"}}]}}"#,
            k.x, k.y
        );
        Arc::new(OAuthState {
            jwks: JwksCache::new("https://stub/jwks.json", Box::new(StaticJwks(doc))),
            issuer: ISSUER.to_string(),
        })
    }

    fn token_with_scope(scope_claim: &str) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let claims = serde_json::json!({
            "typ": "access",
            "sub": "user-1",
            "account_id": "acct-1",
            "org_id": "org-1",
            "workspace_id": "ws-1",
            "client_id": "ruview",
            "scope": scope_claim,
            "jti": "jti-1",
            "iat": now - 10,
            "exp": now + 900,
            "setup": false,
            "workload": false,
            "iss": ISSUER,
        });
        let mut header = Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(KID.to_string());
        encode(
            &header,
            &claims,
            &EncodingKey::from_ec_pem(key().pem.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    /// Mirrors the real route shapes the scope gate keys off.
    fn app(auth: AuthState) -> Router {
        Router::new()
            .route("/api/v1/info", get(|| async { "ok" }))
            .route("/api/v1/models", get(|| async { "ok" }))
            .route("/api/v1/models/m1", delete(|| async { "deleted" }))
            .route("/api/v1/recording/r1", delete(|| async { "deleted" }))
            .route("/api/v1/train/start", post(|| async { "training" }))
            .layer(axum::middleware::from_fn_with_state(auth, require_bearer))
    }

    async fn call(auth: AuthState, method: &str, path: &str, bearer: Option<&str>) -> StatusCode {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(b) = bearer {
            req = req.header(AUTHORIZATION, format!("Bearer {b}"));
        }
        app(auth)
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    fn oauth_only() -> AuthState {
        AuthState {
            token: None,
            oauth: Some(oauth_state()),
        }
    }

    // ── scope policy (pure) ───────────────────────────────────────────

    #[test]
    fn training_requires_the_admin_scope() {
        assert_eq!(
            required_scope_for(&Method::POST, "/api/v1/train/start"),
            scope::SENSING_ADMIN
        );
    }

    #[test]
    fn deleting_a_model_or_recording_requires_the_admin_scope() {
        assert_eq!(
            required_scope_for(&Method::DELETE, "/api/v1/models/m1"),
            scope::SENSING_ADMIN
        );
        assert_eq!(
            required_scope_for(&Method::DELETE, "/api/v1/recording/r1"),
            scope::SENSING_ADMIN
        );
    }

    #[test]
    fn reading_models_is_not_admin_merely_because_the_path_matches() {
        // The gate is (method, path), not path alone — GET on the same prefix
        // must stay a read.
        assert_eq!(
            required_scope_for(&Method::GET, "/api/v1/models/m1"),
            scope::SENSING_READ
        );
    }

    #[test]
    fn non_destructive_mutations_stay_read_scoped() {
        // Loading a model changes server state but destroys nothing. Putting it
        // behind the destructive scope would push routine dashboard use into
        // asking for delete capability — the opposite of least privilege.
        assert_eq!(
            required_scope_for(&Method::POST, "/api/v1/models/load"),
            scope::SENSING_READ
        );
        assert_eq!(
            required_scope_for(&Method::POST, "/api/v1/recording/start"),
            scope::SENSING_READ
        );
    }

    // ── wiring ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_read_scoped_token_reaches_a_read_route() {
        let t = token_with_scope(scope::SENSING_READ);
        assert_eq!(
            call(oauth_only(), "GET", "/api/v1/info", Some(&t)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_read_scoped_token_cannot_delete_a_model() {
        // The whole point of the split: a dashboard session streaming poses
        // must not be able to destroy the model it streams through.
        let t = token_with_scope(scope::SENSING_READ);
        assert_eq!(
            call(oauth_only(), "DELETE", "/api/v1/models/m1", Some(&t)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_read_scoped_token_cannot_start_training() {
        let t = token_with_scope(scope::SENSING_READ);
        assert_eq!(
            call(oauth_only(), "POST", "/api/v1/train/start", Some(&t)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn an_admin_scoped_token_may_delete_and_train() {
        let t = token_with_scope("sensing:read sensing:admin");
        assert_eq!(
            call(oauth_only(), "DELETE", "/api/v1/models/m1", Some(&t)).await,
            StatusCode::OK
        );
        assert_eq!(
            call(oauth_only(), "POST", "/api/v1/train/start", Some(&t)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn an_inference_token_from_another_cognitum_product_is_refused_everywhere() {
        // The cross-product case. Correctly signed, unexpired, right issuer —
        // only the scope stops it. Asserted here at the middleware layer too,
        // because this is where a wiring mistake would actually let it through.
        let t = token_with_scope("inference");
        for (m, p) in [
            ("GET", "/api/v1/info"),
            ("DELETE", "/api/v1/models/m1"),
            ("POST", "/api/v1/train/start"),
        ] {
            assert_eq!(
                call(oauth_only(), m, p, Some(&t)).await,
                StatusCode::UNAUTHORIZED,
                "{m} {p} must reject an inference-only token"
            );
        }
    }

    #[tokio::test]
    async fn a_garbage_bearer_is_refused_when_only_oauth_is_configured() {
        assert_eq!(
            call(oauth_only(), "GET", "/api/v1/info", Some("not-a-jwt")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn no_credential_is_refused_when_only_oauth_is_configured() {
        assert_eq!(
            call(oauth_only(), "GET", "/api/v1/info", None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    // ── layering with the legacy static token ─────────────────────────

    fn both() -> AuthState {
        AuthState {
            token: Some(Arc::new("legacy-secret".to_string())),
            oauth: Some(oauth_state()),
        }
    }

    #[tokio::test]
    async fn the_legacy_static_token_still_works_with_oauth_enabled() {
        // Backward compatibility: turning OAuth on must not break a deployment
        // that has been using RUVIEW_API_TOKEN.
        assert_eq!(
            call(both(), "GET", "/api/v1/info", Some("legacy-secret")).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn the_legacy_static_token_is_not_scope_gated() {
        // It predates scopes and carries no claims, so it keeps the full
        // access it has always had. Narrowing it here would be a silent
        // breaking change to existing deployments; migrating to OAuth is how
        // an operator opts into the finer split.
        assert_eq!(
            call(both(), "POST", "/api/v1/train/start", Some("legacy-secret")).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn an_oauth_token_works_alongside_a_configured_static_token() {
        let t = token_with_scope(scope::SENSING_READ);
        assert_eq!(
            call(both(), "GET", "/api/v1/info", Some(&t)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_wrong_static_token_falls_through_to_oauth_and_is_refused() {
        assert_eq!(
            call(both(), "GET", "/api/v1/info", Some("wrong-secret")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    // ── attribution ───────────────────────────────────────────────────

    #[tokio::test]
    async fn the_verified_principal_is_available_to_handlers() {
        // The reason for moving off a shared secret: requests become
        // attributable. If the principal is not in extensions, no handler and
        // no audit log can name who called.
        async fn echo(req: Request<Body>) -> String {
            match req.extensions().get::<ruview_auth::Principal>() {
                Some(p) => format!("{}|{}|{}", p.subject, p.account_id, p.client_id),
                None => "none".to_string(),
            }
        }
        let router = Router::new()
            .route("/api/v1/whoami", get(echo))
            .layer(axum::middleware::from_fn_with_state(
                oauth_only(),
                require_bearer,
            ));
        let t = token_with_scope(scope::SENSING_READ);
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/whoami")
                    .header(AUTHORIZATION, format!("Bearer {t}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "user-1|acct-1|ruview");
    }

    // ── the unset case must stay untouched ────────────────────────────

    #[tokio::test]
    async fn with_neither_credential_configured_the_middleware_is_still_a_no_op() {
        assert_eq!(
            call(AuthState::default(), "POST", "/api/v1/train/start", None).await,
            StatusCode::OK
        );
    }
}
