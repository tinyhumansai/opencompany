//! GraphQL request context: the shared auth principal and its authorization
//! helpers.
//!
//! [`GqlAuth`] is the single principal type both the REST extractors in
//! [`platform_auth`](crate::server::platform_auth) and the GraphQL handler
//! resolve to. [`resolve_claims`] is the one place a bearer header is turned
//! into a principal; the REST extractors ([`PlatformOrOperatorAuth`],
//! [`PlatformScope`]) map its output onto their existing `Option<PlatformClaims>`
//! shape so their behavior is byte-for-byte unchanged.

use async_graphql::ErrorExtensions;
use axum::http::HeaderMap;

use crate::AppState;
use crate::ports::types::CompanyId;
use crate::server::platform_auth::{PlatformClaims, bearer};

/// The authenticated principal for a request.
///
/// - `Dev`: prosumer mode with no `operator_token` configured (local dev).
/// - `Operator`: prosumer mode, the configured operator token matched.
/// - `Platform`: platform mode, a verified platform/tenant token.
#[derive(Clone, Debug)]
pub enum GqlAuth {
    /// Prosumer dev mode: no operator token configured, every call allowed.
    Dev,
    /// Prosumer mode with a matching operator token.
    Operator,
    /// Platform mode with verified tenant claims.
    Platform(PlatformClaims),
}

/// The one failure [`resolve_claims`] can return: an unauthenticated request.
/// The REST extractors map this to `401`; the GraphQL handler to an error.
#[derive(Clone, Copy, Debug)]
pub struct Unauthorized;

/// Turns an `Authorization` header into a [`GqlAuth`] principal.
///
/// This is the single claims-resolution path shared by the REST extractors and
/// the GraphQL handler:
///
/// - Platform mode (`platform_auth` configured): the bearer must verify, else
///   [`Unauthorized`]. Any valid token (platform-scope or tenant) yields
///   [`GqlAuth::Platform`].
/// - Prosumer mode: no `operator_token` → [`GqlAuth::Dev`]; a matching token →
///   [`GqlAuth::Operator`]; a wrong/missing token when one is configured →
///   [`Unauthorized`].
pub fn resolve_claims(headers: &HeaderMap, state: &AppState) -> Result<GqlAuth, Unauthorized> {
    if let Some(platform) = state.config().platform_auth.as_ref() {
        let token = bearer(headers).ok_or(Unauthorized)?;
        let claims = platform.verifier.verify(token).map_err(|_| Unauthorized)?;
        return Ok(GqlAuth::Platform(claims));
    }
    match state.config().operator_token.as_deref() {
        None => Ok(GqlAuth::Dev),
        Some(expected) if bearer(headers) == Some(expected) => Ok(GqlAuth::Operator),
        Some(_) => Err(Unauthorized),
    }
}

impl GqlAuth {
    /// Authorizes addressing a specific company under this principal.
    ///
    /// Dev/operator and platform-scope principals may address any company; a
    /// tenant principal may address a company only when it owns it (the registry
    /// ownership map records `id -> tenant`) and its own allow-list permits it.
    /// Mirrors [`authorize_address`](crate::server::platform_auth::authorize_address).
    pub fn authorize(&self, state: &AppState, company: &CompanyId) -> async_graphql::Result<()> {
        match self {
            GqlAuth::Dev | GqlAuth::Operator => Ok(()),
            GqlAuth::Platform(claims) => {
                if claims.has_platform_scope() {
                    return Ok(());
                }
                let owner = state.owner_of(company);
                if owner.as_deref() == Some(claims.tenant.as_str()) && claims.may_address(company) {
                    Ok(())
                } else {
                    Err(forbidden())
                }
            }
        }
    }

    /// The companies this principal may see, filtered from the registry.
    ///
    /// Dev/operator and platform-scope principals see every registered company;
    /// a tenant principal sees only the companies it owns and may address.
    pub fn visible_companies(&self, state: &AppState) -> Vec<CompanyId> {
        let all = state.registry().list();
        match self {
            GqlAuth::Dev | GqlAuth::Operator => all,
            GqlAuth::Platform(claims) if claims.has_platform_scope() => all,
            GqlAuth::Platform(claims) => all
                .into_iter()
                .filter(|id| {
                    state.owner_of(id).as_deref() == Some(claims.tenant.as_str())
                        && claims.may_address(id)
                })
                .collect(),
        }
    }
}

/// A `403`-equivalent GraphQL error carrying `extensions.code = "forbidden"`.
pub fn forbidden() -> async_graphql::Error {
    async_graphql::Error::new("forbidden").extend_with(|_, e| e.set("code", "forbidden"))
}

/// A GraphQL error marking a resolver whose backing surface is not yet wired,
/// carrying `extensions.code = "NOT_WIRED"`. The console's bare-catch treats it
/// like a `404` and falls back to its sample.
pub fn not_wired(field: &str) -> async_graphql::Error {
    async_graphql::Error::new(format!("{field} is not wired"))
        .extend_with(|_, e| e.set("code", "NOT_WIRED"))
}
