//! Cognitum OAuth access-token verification (ADR-271).
//!
//! The accept-rule is ported from `meta-llm/src/auth/oauthBearer.ts` (ADR-045),
//! the only other resource-server-side verifier of these tokens in the org.
//! Divergence from it would be a bug, not a preference — a token meta-llm
//! rejects must not be one RuView accepts.
//!
//! ## The trust chain, narrowly
//!
//! 1. Only identity's ES256 key — fetched from the published JWKS by `kid` —
//!    can sign an accepted token. No shared secret, no static PEM to leak.
//! 2. **The algorithm is fixed to ES256 by this code.** The token header's `alg`
//!    is only ever *compared against* that allowlist, never used to *select* an
//!    algorithm. That is what makes `alg: none` and RSA-substitution
//!    non-starters rather than things we defend against case by case.
//! 3. Signature math is `jsonwebtoken`'s. This module owns claim policy only.
//!
//! ## Why `setup` and `workload` tokens are refused outright
//!
//! Identity also issues long-lived *setup* (365-day) and *workload* credentials.
//! Their revocation lives in identity's `oauth_setup_tokens` table, and RuView —
//! like meta-llm — has **no database and no way to check it**. A 15-minute
//! access token needs no revocation round-trip because it expires faster than
//! any realistic revocation propagates; a 365-day one does. Accepting one would
//! mean honouring a credential that may already have been revoked, so we don't.
//!
//! ## There is no `aud` claim
//!
//! Cognitum access tokens carry no audience. Cross-product *identity* is
//! intended — one account, every Cognitum product — so this is by design, not a
//! defect. It does mean **scope is the only capability boundary**: `client_id`
//! cannot serve as one, because clients borrow each other's registrations
//! (musica ships `DEFAULT_CLIENT_ID = "meta-proxy"`). Hence `required_scope`
//! below is not optional garnish; it is the boundary.

use jsonwebtoken::{decode, decode_header, Algorithm, Validation};
use serde::Deserialize;

use crate::jwks::{JwksCache, JwksError};
use crate::principal::Principal;

/// The `typ` identity stamps on ordinary interactive access tokens
/// (`jwt.rs`'s `TOKEN_TYP_ACCESS`).
const TYP_ACCESS: &str = "access";

/// Clock leeway for `exp`/`iat`.
///
/// Deliberately small. Against a 15-minute token a generous window is a real
/// extension of a revoked credential's life, so this absorbs ordinary NTP jitter
/// and nothing more. Hosts without a battery-backed clock (Pi-class) need real
/// time sync — see [`VerifyError::ExpiredOrNotYetValid`], which is reported
/// distinctly so "your clock is wrong" is diagnosable rather than presenting as
/// a generic 401.
const CLOCK_LEEWAY_SECS: u64 = 30;

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("authorization header is missing or not a Bearer token")]
    MissingBearer,
    #[error("token is not a well-formed JWT: {0}")]
    Malformed(String),
    #[error("token algorithm is not ES256")]
    WrongAlgorithm,
    #[error("could not resolve a verification key: {0}")]
    Jwks(#[from] JwksError),
    #[error("token signature is not valid for identity's published key")]
    BadSignature,
    /// `exp`/`iat` outside the accepted window. Distinct from `BadSignature` on
    /// purpose: on an RTC-less host this is usually a clock-sync problem, not an
    /// attack, and an operator needs to be able to tell those apart.
    #[error("token is expired or not yet valid (check host clock sync)")]
    ExpiredOrNotYetValid,
    #[error("token issuer is not {expected}")]
    WrongIssuer { expected: String },
    #[error("token type {found:?} is not an interactive access token")]
    WrongTokenType { found: Option<String> },
    #[error("long-lived setup/workload credentials are not accepted (unverifiable revocation)")]
    LongLivedCredential,
    #[error("token carries no account_id and cannot be attributed")]
    MissingAccountId,
    #[error("token does not carry the required scope {required:?}")]
    MissingScope { required: String },
}

/// Identity's access-token claims. Mirrors `AccessTokenClaims` in
/// `dashboard/services/identity/src/jwt.rs`.
///
/// `typ` is `Option` because identity types it that way — absence must be
/// treated as "not an access token", never as a default.
#[derive(Debug, Deserialize)]
struct AccessTokenClaims {
    #[serde(default)]
    typ: Option<String>,
    sub: String,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    org_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    jti: String,
    exp: i64,
    /// Long-lived, non-rotating setup credential. Absent on older tokens.
    #[serde(default)]
    setup: bool,
    /// Machine workload credential. Absent on older tokens.
    #[serde(default)]
    workload: bool,
}

/// Verifier configuration.
pub struct VerifierConfig {
    /// Expected `iss`, compared **verbatim**. RFC 8414 §2 requires the issuer to
    /// match the origin exactly — a trailing slash or an http/https mismatch is
    /// a failure, not a near-miss to be normalised away.
    pub issuer: String,
    /// The scope a caller must hold for the route being served.
    pub required_scope: String,
}

/// Verify a raw JWT and produce a [`Principal`].
///
/// Every rejection path returns a typed error; none of them return a partially
/// trusted principal.
pub fn verify_access_token(
    token: &str,
    jwks: &JwksCache,
    config: &VerifierConfig,
) -> Result<Principal, VerifyError> {
    let header = decode_header(token).map_err(|e| VerifyError::Malformed(e.to_string()))?;

    // Compared, never selected. `decode` below independently enforces the same
    // allowlist; this early check exists so the failure is legible.
    if header.alg != Algorithm::ES256 {
        return Err(VerifyError::WrongAlgorithm);
    }
    let kid = header.kid.ok_or(JwksError::MissingKid)?;
    let key = jwks.decoding_key_for(&kid)?;

    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_issuer(&[config.issuer.as_str()]);
    validation.leeway = CLOCK_LEEWAY_SECS;
    validation.validate_exp = true;
    // No audience validation: these tokens carry no `aud` (see module docs).
    validation.validate_aud = false;
    validation.set_required_spec_claims(&["exp", "iss"]);

    let data = decode::<AccessTokenClaims>(token, &key, &validation).map_err(map_jwt_error)?;
    let claims = data.claims;

    // ---- Claim policy. Mirrors meta-llm's oauthBearer.ts accept-rule. ----

    if claims.typ.as_deref() != Some(TYP_ACCESS) {
        return Err(VerifyError::WrongTokenType { found: claims.typ });
    }
    if claims.setup || claims.workload {
        // Belt and braces alongside the `typ` check: identity stamps these as
        // booleans as well, and a credential that sets either must never be
        // honoured here regardless of how it types itself.
        return Err(VerifyError::LongLivedCredential);
    }

    let account_id = claims.account_id.filter(|a| !a.is_empty());
    let Some(account_id) = account_id else {
        // meta-llm requires this so a token cannot bill an account it doesn't
        // belong to. RuView's reason is attribution: an unattributable principal
        // cannot appear in an audit trail, which is most of the point of moving
        // off a shared static bearer.
        return Err(VerifyError::MissingAccountId);
    };

    let principal = Principal::new(
        claims.sub,
        account_id,
        claims.org_id,
        claims.workspace_id,
        claims.client_id,
        claims.jti,
        &claims.scope,
        claims.exp,
    );

    if !principal.has_scope(&config.required_scope) {
        return Err(VerifyError::MissingScope {
            required: config.required_scope.clone(),
        });
    }

    Ok(principal)
}

/// Extract a bearer token from an `Authorization` header value.
pub fn extract_bearer(header_value: &str) -> Result<&str, VerifyError> {
    let token = header_value
        .strip_prefix("Bearer ")
        .ok_or(VerifyError::MissingBearer)?
        .trim();
    if token.is_empty() {
        return Err(VerifyError::MissingBearer);
    }
    Ok(token)
}

fn map_jwt_error(e: jsonwebtoken::errors::Error) -> VerifyError {
    use jsonwebtoken::errors::ErrorKind;
    match e.kind() {
        ErrorKind::InvalidSignature => VerifyError::BadSignature,
        ErrorKind::ExpiredSignature | ErrorKind::ImmatureSignature => {
            VerifyError::ExpiredOrNotYetValid
        }
        ErrorKind::InvalidIssuer => VerifyError::WrongIssuer {
            expected: String::new(),
        },
        ErrorKind::InvalidAlgorithm | ErrorKind::InvalidAlgorithmName => {
            VerifyError::WrongAlgorithm
        }
        _ => VerifyError::Malformed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_bearer_accepts_a_well_formed_header() {
        assert_eq!(extract_bearer("Bearer abc.def.ghi").unwrap(), "abc.def.ghi");
    }

    #[test]
    fn extract_bearer_rejects_a_missing_prefix() {
        assert!(matches!(
            extract_bearer("abc.def.ghi"),
            Err(VerifyError::MissingBearer)
        ));
    }

    #[test]
    fn extract_bearer_rejects_a_lowercase_scheme() {
        // RFC 7235 makes the scheme case-insensitive, but every Cognitum client
        // sends "Bearer". Accepting variants would widen the surface for no
        // real-world caller, so this is a deliberate strictness.
        assert!(matches!(
            extract_bearer("bearer abc.def.ghi"),
            Err(VerifyError::MissingBearer)
        ));
    }

    #[test]
    fn extract_bearer_rejects_an_empty_token() {
        assert!(matches!(
            extract_bearer("Bearer   "),
            Err(VerifyError::MissingBearer)
        ));
    }
}
