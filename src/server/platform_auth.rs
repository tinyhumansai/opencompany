//! Platform authentication: the hosting layer's tenant-scoped bearer.
//!
//! This is the only *machine* credential. A platform-issued bearer's verified
//! [`PlatformClaims`] carry a `tenant`, a set of `scopes`, and an optional
//! company allow-list. Provisioning and suspension require the `platform`
//! scope; every tenant token is confined to the companies it owns, so it can
//! never cross tenants.
//!
//! Humans do not use this surface — they sign in and carry a session cookie
//! (see [`server::users`](crate::server::users)). Without `platform_auth`
//! configured there is no machine credential at all, and a session is the only
//! way in.
//!
//! The verification seam is [`PlatformVerifier`]. The default build ships an
//! offline [`StaticPlatformVerifier`] (a shared platform secret plus unsigned,
//! structured tenant tokens) so the scope-gate and cross-tenant logic are fully
//! covered by `cargo test`. Real signed-JWT verification is added under the
//! `platform-jwt` feature; the gate logic here is verifier-agnostic.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{FromRequestParts, RawPathParams};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::{Json, http::HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;
use crate::server::graphql::auth::GqlAuth;

/// The `platform` scope, required for provisioning and suspension.
pub const SCOPE_PLATFORM: &str = "platform";

/// Claims a verified platform token carries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlatformClaims {
    /// The owning tenant, e.g. `tenant:acme`.
    pub tenant: String,
    /// The granted scopes, e.g. `{"operator", "platform"}`.
    #[serde(default)]
    pub scopes: HashSet<String>,
    /// An explicit company allow-list. `None` means "any company this tenant
    /// owns" (ownership is enforced separately against the registry map).
    #[serde(default)]
    pub companies: Option<HashSet<String>>,
}

impl PlatformClaims {
    /// Whether these claims carry the `platform` scope (provisioning/suspension).
    pub fn has_platform_scope(&self) -> bool {
        self.scopes.contains(SCOPE_PLATFORM)
    }

    /// Whether the token's own allow-list permits addressing `id`. `None`
    /// allow-list permits any id (ownership is checked separately).
    pub fn may_address(&self, id: &CompanyId) -> bool {
        match &self.companies {
            Some(allow) => allow.contains(id.as_ref()),
            None => true,
        }
    }
}

/// The verification seam: turns a bearer string into [`PlatformClaims`] or an
/// error. Implementations are offline-testable; only real JWT signature
/// verification is feature-gated.
pub trait PlatformVerifier: Send + Sync {
    /// Verifies `bearer` and returns its claims, or an error if invalid.
    fn verify(&self, bearer: &str) -> crate::Result<PlatformClaims>;
}

/// An offline/dev verifier. A bearer equal to `platform_secret` is a
/// platform-scope token; a `oc_tenant.<base64url(json)>` bearer carries
/// structured (UNSIGNED) tenant claims. Clearly insecure — for local use and
/// tests. Real signed verification is `platform-jwt`.
#[derive(Clone)]
pub struct StaticPlatformVerifier {
    /// The shared secret that grants a full platform-scope token.
    pub platform_secret: String,
}

impl StaticPlatformVerifier {
    /// The prefix marking an unsigned structured tenant token.
    pub const TENANT_PREFIX: &'static str = "oc_tenant.";

    /// Builds a verifier around a shared platform secret.
    pub fn new(platform_secret: impl Into<String>) -> Self {
        Self {
            platform_secret: platform_secret.into(),
        }
    }

    /// Encodes structured claims into a dev tenant token (test helper).
    pub fn tenant_token(claims: &PlatformClaims) -> String {
        let json = serde_json::to_vec(claims).expect("claims serialize");
        format!("{}{}", Self::TENANT_PREFIX, b64url_encode(&json))
    }
}

impl PlatformVerifier for StaticPlatformVerifier {
    fn verify(&self, bearer: &str) -> crate::Result<PlatformClaims> {
        if bearer == self.platform_secret {
            return Ok(PlatformClaims {
                tenant: "tenant:platform".to_string(),
                scopes: HashSet::from([SCOPE_PLATFORM.to_string(), "operator".to_string()]),
                companies: None,
            });
        }
        if let Some(encoded) = bearer.strip_prefix(Self::TENANT_PREFIX) {
            let bytes = b64url_decode(encoded).ok_or_else(|| {
                OpenCompanyError::InvalidRequest("malformed platform token".to_string())
            })?;
            let claims: PlatformClaims = serde_json::from_slice(&bytes)?;
            return Ok(claims);
        }
        Err(OpenCompanyError::InvalidRequest(
            "unrecognized token".to_string(),
        ))
    }
}

/// A real signed-JWT verifier (HS256), gated behind `platform-jwt`. The claim
/// shape mirrors [`PlatformClaims`] (`tenant`, `scopes`, `companies`); `exp` is
/// honored when present. The offline [`StaticPlatformVerifier`] covers the
/// scope-gate logic without this feature.
#[cfg(feature = "platform-jwt")]
pub struct JwtPlatformVerifier {
    secret: String,
}

#[cfg(feature = "platform-jwt")]
impl JwtPlatformVerifier {
    /// Builds an HS256 verifier around a shared signing secret.
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
        }
    }
}

#[cfg(feature = "platform-jwt")]
impl PlatformVerifier for JwtPlatformVerifier {
    fn verify(&self, bearer: &str) -> crate::Result<PlatformClaims> {
        use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};

        let mut validation = Validation::new(Algorithm::HS256);
        // Callers may omit registered claims; only signature (and `exp` when
        // present) matter for the platform gate.
        validation.required_spec_claims.clear();
        validation.validate_exp = true;

        let token = decode::<PlatformClaims>(
            bearer,
            &DecodingKey::from_secret(self.secret.as_bytes()),
            &validation,
        )
        .map_err(|e| OpenCompanyError::InvalidRequest(format!("invalid jwt: {e}")))?;
        Ok(token.claims)
    }
}

/// Platform auth configuration held on [`AppConfig`](crate::AppConfig): the
/// verifier plus optional expected issuer/audience (used by the JWT verifier).
#[derive(Clone)]
pub struct PlatformAuthConfig {
    /// The token verifier.
    pub verifier: Arc<dyn PlatformVerifier>,
}

impl PlatformAuthConfig {
    /// Builds a config around a verifier.
    pub fn new(verifier: Arc<dyn PlatformVerifier>) -> Self {
        Self { verifier }
    }
}

impl std::fmt::Debug for PlatformAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformAuthConfig").finish_non_exhaustive()
    }
}

/// Extracts the bearer token from the `Authorization` header.
///
/// Shared with [`resolve_claims`](crate::server::graphql::auth::resolve_claims)
/// so REST and GraphQL parse the credential identically.
pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// Refuses a user who is still carrying an admin-issued temporary password.
///
/// An admin who resets a password knows it, and conveys it over some channel
/// they do not control. So a session opened with one is only good for replacing
/// it: this returns `403 password_change_required` everywhere except the auth
/// routes (set-password, logout, me), which deliberately do not call this so
/// the user can always resolve the situation.
///
/// Checked at the extractors rather than surfaced to the console, so it holds
/// against a client that would rather not honor it.
pub(crate) fn refuse_until_password_changed(auth: &GqlAuth) -> Option<Response> {
    match auth {
        GqlAuth::User(user) if user.must_change_password => Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "set a new password before continuing",
                    "code": "password_change_required",
                })),
            )
                .into_response(),
        ),
        _ => None,
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized", "code": "unauthorized" })),
    )
        .into_response()
}

pub(crate) fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "forbidden", "code": "forbidden" })),
    )
        .into_response()
}

/// An extractor for any principal entitled to address a company: a platform
/// token, or a human's session cookie.
///
/// Replaces the old `PlatformOrOperatorAuth`, whose `Option<PlatformClaims>`
/// could not represent a human and whose `None` meant "dev mode, allow
/// everything". There is no such state now — an unauthenticated request is
/// `401`.
///
/// The extractor resolves the addressed company from the `{id}` path param when
/// present so a session cookie can be matched to it; on the single-company
/// alias the sole session cookie selects itself.
pub struct CompanyAuth(pub GqlAuth);

impl FromRequestParts<AppState> for CompanyAuth {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        use crate::server::graphql::auth::resolve_principal;

        // Sniff `{id}` without consuming it; handlers still extract their own.
        let company = RawPathParams::from_request_parts(parts, state)
            .await
            .ok()
            .and_then(|params| {
                params
                    .iter()
                    .find(|(key, _)| *key == "id")
                    .map(|(_, value)| CompanyId::new(value))
            });
        match resolve_principal(&parts.headers, state, company.as_ref()).await {
            Ok(auth) => Ok(Self(auth)),
            Err(_) => Err(unauthorized()),
        }
    }
}

/// An extractor requiring the `platform` scope: the hosting layer only.
///
/// This gates provisioning and suspension — creating and destroying companies
/// across tenants. It resolves through [`resolve_claims`], which cannot return
/// a human, so a session cookie can never reach these routes whatever it
/// contains.
///
/// Without `platform_auth` configured nobody holds the scope, so a self-hosted
/// deployment has no HTTP provisioning at all and loads companies with
/// `serve --company <dir>`. That is the intended shape: a prosumer host has no
/// machine credential to hand out.
pub struct PlatformScope(pub PlatformClaims);

impl FromRequestParts<AppState> for PlatformScope {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        use crate::server::graphql::auth::{GqlAuth, resolve_claims};
        match resolve_claims(&parts.headers, state) {
            Ok(GqlAuth::Platform(claims)) if claims.has_platform_scope() => Ok(Self(claims)),
            Ok(GqlAuth::Platform(_)) => Err(forbidden()),
            // Unreachable: resolve_claims cannot construct a User. Stated
            // rather than wildcarded so that if it ever could, this refuses.
            Ok(GqlAuth::User(_)) => Err(forbidden()),
            Err(_) => Err(unauthorized()),
        }
    }
}

/// Authorizes addressing a specific company under the given claims.
///
/// - Platform scope may address any company.
/// - A tenant token may address a company only when it owns it (the registry
///   ownership map records `id -> tenant`) and its own allow-list permits it.
/// - `None` claims (prosumer/dev) are always allowed.
///
/// Returns `Some(403 forbidden)` on a cross-tenant or out-of-allow-list attempt,
/// or `None` when the caller is allowed to address `id`.
pub fn authorize_address(state: &AppState, auth: &GqlAuth, id: &CompanyId) -> Option<Response> {
    match auth {
        GqlAuth::Platform(claims) => {
            if claims.has_platform_scope() {
                return None;
            }
            let owner = state.owner_of(id);
            if owner.as_deref() == Some(crate::app::canonical_tenant(&claims.tenant))
                && claims.may_address(id)
            {
                None
            } else {
                Some(forbidden())
            }
        }
        // A user belongs to one company. The storage partition already makes a
        // cross-company session unresolvable; this is the explicit check.
        GqlAuth::User(user) => {
            if user.company == *id {
                None
            } else {
                Some(forbidden())
            }
        }
    }
}

/// The tenant a token acts as, for ownership recording. Platform-scope and dev
/// callers act as the `tenant:platform` account.
pub fn acting_tenant(auth: &GqlAuth) -> String {
    match auth {
        GqlAuth::Platform(claims) => claims.tenant.clone(),
        // A human acts for the company they belong to. Ownership records a
        // tenant, and a self-hosted company's tenant is itself.
        GqlAuth::User(user) => format!("company:{}", user.company),
    }
}

// ---------------------------------------------------------------------------
// base64url (no padding) — std-only, used by the offline dev token codec.
// ---------------------------------------------------------------------------

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encodes bytes as unpadded base64url.
pub(crate) fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64URL[(n >> 18) as usize & 0x3f] as char);
        out.push(B64URL[(n >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(B64URL[(n >> 6) as usize & 0x3f] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL[n as usize & 0x3f] as char);
        }
    }
    out
}

/// Decodes unpadded base64url, returning `None` on any invalid input.
fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod test {
    use super::*;

    fn tenant_claims(tenant: &str, scopes: &[&str]) -> PlatformClaims {
        PlatformClaims {
            tenant: tenant.to_string(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            companies: None,
        }
    }

    #[test]
    fn b64url_round_trips() {
        for sample in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"hello world!"] {
            let encoded = b64url_encode(sample);
            assert_eq!(b64url_decode(&encoded).unwrap(), sample);
        }
    }

    #[test]
    fn platform_secret_grants_platform_scope() {
        let verifier = StaticPlatformVerifier::new("top-secret");
        let claims = verifier.verify("top-secret").unwrap();
        assert!(claims.has_platform_scope());
        assert_eq!(claims.tenant, "tenant:platform");
    }

    #[test]
    fn tenant_token_carries_structured_claims_without_platform_scope() {
        let verifier = StaticPlatformVerifier::new("top-secret");
        let token =
            StaticPlatformVerifier::tenant_token(&tenant_claims("tenant:acme", &["operator"]));
        let claims = verifier.verify(&token).unwrap();
        assert_eq!(claims.tenant, "tenant:acme");
        assert!(!claims.has_platform_scope());
    }

    #[test]
    fn unrecognized_token_is_rejected() {
        let verifier = StaticPlatformVerifier::new("top-secret");
        assert!(verifier.verify("nope").is_err());
        assert!(verifier.verify("oc_tenant.@@@not-base64@@@").is_err());
    }

    #[test]
    fn may_address_honors_allow_list() {
        let mut claims = tenant_claims("tenant:acme", &["operator"]);
        assert!(claims.may_address(&CompanyId::new("anything")));
        claims.companies = Some(HashSet::from(["acme".to_string()]));
        assert!(claims.may_address(&CompanyId::new("acme")));
        assert!(!claims.may_address(&CompanyId::new("globex")));
    }

    #[cfg(feature = "platform-jwt")]
    #[test]
    fn jwt_verifier_round_trips_signed_claims() {
        use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

        let secret = "signing-secret";
        let claims = tenant_claims("tenant:acme", &["platform", "operator"]);
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();

        let verifier = JwtPlatformVerifier::new(secret);
        let verified = verifier.verify(&token).unwrap();
        assert_eq!(verified.tenant, "tenant:acme");
        assert!(verified.has_platform_scope());

        // A token signed with the wrong secret is rejected.
        let wrong = JwtPlatformVerifier::new("other-secret");
        assert!(wrong.verify(&token).is_err());
    }
}
