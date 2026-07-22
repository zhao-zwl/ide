//! Authentication & SSO (T12 — enterprise GA).
//!
//! Provides a pluggable [`Authenticator`] trait that derives a [`Principal`]
//! from a bearer token at the gRPC boundary. Two implementations ship:
//!   * [`NoopAuthenticator`] — dev / no-SSO mode; ignores the token and returns
//!     the configured (server) principal. Selected when `sso_enabled = false`.
//!   * [`Hs256Authenticator`] — minimal HS256 shared-secret verification of a
//!     `Authorization: Bearer <jwt>` token; claims (`tenant_id` / `user_id` /
//!     `perm_mask`) map onto a `Principal`. Wrong secret / expired / missing
//!     token (when SSO is on) / issuer / audience mismatch -> `AuthError::Unauthorized`.
//!
//! OIDC (RS256 / JWKS), LDAP and SAML are intentionally **not** implemented
//! here. HS256 with a shared secret is the minimal viable enterprise SSO for the
//! slice (see README §v1.0B "SSO 扩展说明"). The transport-layer bearer
//! extraction lives in `agent.rs`; this module is pure and unit-testable with
//! no network or heavyweight dependencies (only `hmac` + `sha2` + `base64`).

use crate::principal::Principal;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// HMAC-SHA256 MAC type alias.
type HmacSha256 = Hmac<Sha256>;

/// Authentication failure returned by an [`Authenticator`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    /// The presented credential is missing, malformed, or rejected.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
}

/// Pluggable identity source. The gRPC server holds an `Arc<dyn Authenticator>`
/// and calls [`Authenticator::authenticate`] per request to obtain the
/// [`Principal`] that is then threaded through the governance chain.
pub trait Authenticator: Send + Sync {
    /// Resolve a bearer token (already stripped of the `Bearer ` prefix, or
    /// `None` when absent) into a [`Principal`].
    ///
    /// Returns [`AuthError::Unauthorized`] when the credential is missing or
    /// invalid (e.g. bad signature, expired, issuer/audience mismatch).
    fn authenticate(&self, token: Option<&str>) -> Result<Principal, AuthError>;
}

/// Dev / no-SSO authenticator. Always returns the configured server principal,
/// ignoring the token. Selected when `sso_enabled = false` (the default), so a
/// deployment without SSO keeps working exactly as in v1.0 Stage A.
#[derive(Debug, Clone)]
pub struct NoopAuthenticator {
    fallback: Principal,
}

impl NoopAuthenticator {
    /// Build a no-op authenticator that returns `fallback` for every request.
    pub fn new(fallback: Principal) -> Self {
        Self { fallback }
    }
}

impl Authenticator for NoopAuthenticator {
    fn authenticate(&self, _token: Option<&str>) -> Result<Principal, AuthError> {
        Ok(self.fallback.clone())
    }
}

/// Minimal HS256 bearer-token verifier (T12 SSO). The Core shares a secret with
/// the identity provider; tokens are standard `header.payload.signature` JWTs
/// signed with HMAC-SHA256 over `<header>.<payload>`.
///
/// Claims consumed:
///   * `tenant_id` (string) — single-tenant tenant id.
///   * `user_id`   (string) — acting user / service identity.
///   * `perm_mask` (u32)    — six-bit capability mask (0..63), clamped on map.
///   * `exp`       (i64)    — expiry (Unix seconds); expired tokens are rejected.
///   * `iss` / `aud`        — optionally pinned to the configured issuer/client.
#[derive(Debug, Clone)]
pub struct Hs256Authenticator {
    /// Shared HMAC-SHA256 secret.
    secret: String,
    /// Expected `iss` claim; empty string disables the check.
    issuer: String,
    /// Expected `aud`/`client_id` claim; empty string disables the check.
    client_id: String,
}

impl Hs256Authenticator {
    /// Build a verifier pinning `secret` (shared with the IdP). `issuer` /
    /// `client_id` are optional strictness pins; pass `""` to skip each check.
    pub fn new(
        secret: impl Into<String>,
        issuer: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Self {
        Self {
            secret: secret.into(),
            issuer: issuer.into(),
            client_id: client_id.into(),
        }
    }
}

impl Authenticator for Hs256Authenticator {
    fn authenticate(&self, token: Option<&str>) -> Result<Principal, AuthError> {
        let token = token.ok_or_else(|| AuthError::Unauthorized("missing bearer token".into()))?;
        let claims = verify_hs256(token, &self.secret).map_err(AuthError::Unauthorized)?;

        // Optional strictness pins (skipped when the configured value is empty).
        if !self.issuer.is_empty() {
            match &claims.iss {
                Some(iss) if iss == &self.issuer => {}
                _ => return Err(AuthError::Unauthorized("issuer mismatch".into())),
            }
        }
        if !self.client_id.is_empty() {
            match &claims.aud {
                Some(aud) if aud == &self.client_id => {}
                _ => return Err(AuthError::Unauthorized("audience mismatch".into())),
            }
        }

        Ok(Principal::from_mask(
            claims.tenant_id,
            claims.user_id,
            claims.perm_mask,
        ))
    }
}

/// Decoded JWT claims consumed by [`Hs256Authenticator`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HsClaims {
    #[serde(default)]
    tenant_id: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    perm_mask: u32,
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    iss: Option<String>,
    #[serde(default)]
    aud: Option<String>,
}

/// Compute `base64url(HMAC-SHA256(signing_input))` (no padding), the JWT
/// signature for an HS256 token.
fn sign_hs256(signing_input: &str, secret: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts keys of any size");
    mac.update(signing_input.as_bytes());
    let tag = mac.finalize().into_bytes();
    URL_SAFE_NO_PAD.encode(tag)
}

/// Verify an HS256 JWT and return its decoded [`HsClaims`]. Returns a
/// human-readable error on malformed shape, bad signature, expiry, or
/// non-JSON payload.
fn verify_hs256(token: &str, secret: &str) -> Result<HsClaims, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("malformed JWT: expected 3 segments".into());
    }
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let expected = sign_hs256(&signing_input, secret);
    if !constant_time_eq(expected.as_bytes(), parts[2].as_bytes()) {
        return Err("signature verification failed".into());
    }
    let payload_json = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|e| format!("payload is not base64url: {e}"))?;
    let claims: HsClaims = serde_json::from_slice(&payload_json)
        .map_err(|e| format!("payload is not valid claims JSON: {e}"))?;
    if let Some(exp) = claims.exp {
        let now = now_secs();
        if now > exp {
            return Err(format!("token expired at {exp}, now {now}"));
        }
    }
    Ok(claims)
}

/// Length-safe constant-time byte comparison (timing-attack hygiene).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Current Unix epoch seconds.
fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Mint a signed HS256 JWT from raw claim parts. Used by tests and (optionally)
/// by an IdP-facing token issuer; not required by the verification path.
pub fn mint_hs256_token(
    secret: &str,
    tenant_id: &str,
    user_id: &str,
    perm_mask: u32,
    exp: Option<i64>,
    iss: Option<&str>,
    aud: Option<&str>,
) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = HsClaims {
        tenant_id: tenant_id.to_string(),
        user_id: user_id.to_string(),
        perm_mask,
        exp,
        iss: iss.map(|s| s.to_string()),
        aud: aud.map(|s| s.to_string()),
    };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims serialize"));
    let signing_input = format!("{header}.{payload}");
    let sig = sign_hs256(&signing_input, secret);
    format!("{signing_input}.{sig}")
}

/// Strip the `Bearer ` scheme (case-insensitive) from an `Authorization` header
/// value, returning the raw token. Returns `None` for missing/empty values. A
/// bare token with no scheme is tolerated (returned as-is).
pub fn extract_bearer(authorization: Option<&str>) -> Option<String> {
    let v = authorization?;
    let v = v.trim();
    if let Some(tok) = v.strip_prefix("Bearer ") {
        Some(tok.trim().to_string())
    } else if let Some(tok) = v.strip_prefix("bearer ") {
        Some(tok.trim().to_string())
    } else if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Permission;

    const SECRET: &str = "test-shared-secret";

    #[test]
    fn noop_returns_config_principal_regardless_of_token() {
        let p = Principal::from_mask("t1", "u1", 35);
        let authn = NoopAuthenticator::new(p.clone());
        // Any token (or none) yields the configured principal.
        let got = authn.authenticate(Some("garbage.token.here")).unwrap();
        assert_eq!(got.tenant_id, "t1");
        assert_eq!(got.perm_mask(), 35);
        assert!(authn.authenticate(None).is_ok());
    }

    #[test]
    fn hs256_roundtrip_ok() {
        let tok = mint_hs256_token(
            SECRET,
            "acme",
            "alice",
            63,
            Some(now_secs() + 3600),
            Some("aidea"),
            Some("cli"),
        );
        let authn = Hs256Authenticator::new(SECRET, "aidea", "cli");
        let p = authn.authenticate(Some(&tok)).unwrap();
        assert_eq!(p.tenant_id, "acme");
        assert_eq!(p.user_id, "alice");
        assert_eq!(p.perm_mask(), 63);
        assert!(p.perms.has(Permission::Commit));
    }

    #[test]
    fn hs256_wrong_secret_rejected() {
        let tok = mint_hs256_token(SECRET, "acme", "alice", 63, Some(now_secs() + 3600), None, None);
        let authn = Hs256Authenticator::new("wrong-secret", "", "");
        assert!(matches!(
            authn.authenticate(Some(&tok)),
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[test]
    fn hs256_missing_token_rejected() {
        let authn = Hs256Authenticator::new(SECRET, "", "");
        assert!(matches!(
            authn.authenticate(None),
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[test]
    fn hs256_expired_rejected() {
        let tok = mint_hs256_token(SECRET, "acme", "alice", 63, Some(now_secs() - 10), None, None);
        let authn = Hs256Authenticator::new(SECRET, "", "");
        assert!(matches!(
            authn.authenticate(Some(&tok)),
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[test]
    fn hs256_issuer_mismatch_rejected() {
        let tok = mint_hs256_token(
            SECRET,
            "acme",
            "alice",
            63,
            Some(now_secs() + 3600),
            Some("other-iss"),
            None,
        );
        let authn = Hs256Authenticator::new(SECRET, "aidea", "");
        assert!(matches!(
            authn.authenticate(Some(&tok)),
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[test]
    fn hs256_audience_mismatch_rejected() {
        let tok = mint_hs256_token(
            SECRET,
            "acme",
            "alice",
            63,
            Some(now_secs() + 3600),
            None,
            Some("other-aud"),
        );
        let authn = Hs256Authenticator::new(SECRET, "", "cli");
        assert!(matches!(
            authn.authenticate(Some(&tok)),
            Err(AuthError::Unauthorized(_))
        ));
    }

    #[test]
    fn hs256_claims_map_to_principal() {
        // 0b100011 == Read | Generate | Commit.
        let tok =
            mint_hs256_token(SECRET, "t", "u", 0b100011, Some(now_secs() + 3600), None, None);
        let authn = Hs256Authenticator::new(SECRET, "", "");
        let p = authn.authenticate(Some(&tok)).unwrap();
        assert_eq!(p.perm_mask(), 0b100011);
        assert!(p.perms.has(Permission::Read));
        assert!(p.perms.has(Permission::Generate));
        assert!(p.perms.has(Permission::Commit));
        assert!(!p.perms.has(Permission::Modify));
    }

    #[test]
    fn extract_bearer_strips_scheme_and_tolerates_bare() {
        assert_eq!(
            extract_bearer(Some("Bearer abc.def.ghi")),
            Some("abc.def.ghi".to_string())
        );
        assert_eq!(
            extract_bearer(Some("bearer xyz")),
            Some("xyz".to_string())
        );
        assert_eq!(
            extract_bearer(Some("  Bearer tok ")),
            Some("tok".to_string())
        );
        // Bare token (no scheme) is tolerated.
        assert_eq!(extract_bearer(Some("raw.token")), Some("raw.token".to_string()));
        assert_eq!(extract_bearer(None), None);
        assert_eq!(extract_bearer(Some("")), None);
    }
}
