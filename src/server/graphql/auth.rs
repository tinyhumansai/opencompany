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
use crate::ports::{UserRole, UserStatus};
use crate::server::platform_auth::{PlatformClaims, bearer};

/// A human collaborator, authenticated by a session cookie.
///
/// Scoped to exactly one company: unlike a platform tenant, which may own
/// several, a user *is* a member of one company and a session is minted for
/// that company alone.
#[derive(Clone, Debug)]
pub struct UserPrincipal {
    /// The company this session was minted in. Valid nowhere else.
    pub company: CompanyId,
    /// The authenticated [`UserRecord::id`](crate::ports::UserRecord).
    pub user_id: String,
    /// The user's normalized email.
    pub email: String,
    /// What the user may do inside their company.
    pub role: UserRole,
    /// The session's token hash, so logout can revoke exactly this session.
    pub session_token_hash: String,
}

impl UserPrincipal {
    /// Whether this user may invite, revoke, and remove other users.
    pub fn may_administer(&self) -> bool {
        self.role.may_administer()
    }
}

/// The authenticated principal for a request.
///
/// - `Dev`: prosumer mode with no `operator_token` configured (local dev).
/// - `Operator`: prosumer mode, the configured operator token matched.
/// - `Platform`: platform mode, a verified platform/tenant token.
/// - `User`: a human collaborator with a session cookie, scoped to one company.
///
/// The first three are *machine* credentials and are what
/// [`resolve_claims`] returns. `User` can only come from
/// [`resolve_principal`], which is the property that keeps humans off the
/// operator write plane — see [`resolve_claims`].
#[derive(Clone, Debug)]
pub enum GqlAuth {
    /// Prosumer dev mode: no operator token configured, every call allowed.
    Dev,
    /// Prosumer mode with a matching operator token.
    Operator,
    /// Platform mode with verified tenant claims.
    Platform(PlatformClaims),
    /// A human collaborator of exactly one company. Never an operator.
    User(UserPrincipal),
}

/// The one failure [`resolve_claims`] can return: an unauthenticated request.
/// The REST extractors map this to `401`; the GraphQL handler to an error.
#[derive(Clone, Copy, Debug)]
pub struct Unauthorized;

/// Turns an `Authorization` header into a **machine** [`GqlAuth`] principal.
///
/// This is the single claims-resolution path for machine credentials, shared by
/// the REST extractors and the GraphQL handler:
///
/// - Platform mode (`platform_auth` configured): the bearer must verify, else
///   [`Unauthorized`]. Any valid token (platform-scope or tenant) yields
///   [`GqlAuth::Platform`].
/// - Prosumer mode: no `operator_token` → [`GqlAuth::Dev`]; a matching token →
///   [`GqlAuth::Operator`]; a wrong/missing token when one is configured →
///   [`Unauthorized`].
///
/// **It can never return [`GqlAuth::User`], and that is load-bearing.** Every
/// operator/platform write route reaches auth through this function (via
/// `PlatformOrOperatorAuth`, `PlatformScope`, and `ScopedCompany`), so a
/// session cookie cannot reach any of them no matter what it contains — not
/// because each route checks, but because the type it receives cannot represent
/// a human. User-facing routes opt in explicitly by calling
/// [`resolve_principal`] instead.
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

/// Resolves the full principal: a valid session cookie wins, else the machine
/// credentials from [`resolve_claims`].
///
/// `company` is the addressed company when the caller knows it (the REST
/// routes, from the path or the sole registered company). Pass `None` when it
/// is not knowable at resolution time — the GraphQL handler's company argument
/// lives in the request body — and the cookie *name* selects it. With several
/// session cookies present and no addressed company (only reachable in local
/// dev, where one origin serves many companies) no user is resolved, because
/// guessing which one the request meant would be worse than degrading.
///
/// A present-but-invalid session cookie falls through to the bearer path rather
/// than failing the request: a stale cookie must not brick an operator sharing
/// the origin.
pub async fn resolve_principal(
    headers: &HeaderMap,
    state: &AppState,
    company: Option<&CompanyId>,
) -> Result<GqlAuth, Unauthorized> {
    if let Some(user) = resolve_session(headers, state, company).await {
        return Ok(GqlAuth::User(user));
    }
    resolve_claims(headers, state)
}

/// Resolves a session cookie to a live user, or `None`.
///
/// Returns `None` — never an error — for every failure: no cookie, unknown
/// token, expired session, vanished user, suspended user. Callers fall back to
/// machine credentials.
async fn resolve_session(
    headers: &HeaderMap,
    state: &AppState,
    company: Option<&CompanyId>,
) -> Option<UserPrincipal> {
    use crate::server::users::cookie::{company_from_cookie_name, parse_cookies};

    let cookies = parse_cookies(headers);
    // Resolve which company's cookie to read: the addressed one when known,
    // else the sole session cookie present.
    let (company, token) = match company {
        Some(id) => {
            let name = crate::server::users::cookie::session_cookie_name(id)?;
            (id.clone(), cookies.get(&name)?.clone())
        }
        None => {
            let mut sessions = cookies
                .iter()
                .filter_map(|(name, value)| {
                    company_from_cookie_name(name).map(|id| (CompanyId::new(id), value.clone()))
                })
                .collect::<Vec<_>>();
            // Exactly one, or we cannot know which company was meant.
            if sessions.len() != 1 {
                return None;
            }
            sessions.remove(0)
        }
    };

    let runtime = state.registry().get(&company)?;
    let token_hash = crate::server::users::token::sha256_hex(&token);
    // Lookup is *by* hash and scoped to the company: a session minted for
    // another company simply is not in this partition.
    let session = runtime
        .sessions()
        .find_by_token_hash(&company, &token_hash)
        .await
        .ok()??;
    let now = crate::ports::now_millis();
    if !session.is_live(now) {
        return None;
    }
    // Re-read the user on every request. This is what makes suspension and
    // removal take effect immediately rather than whenever the cookie happens
    // to expire — the cost is a second store read per authenticated request.
    let user = runtime
        .users()
        .get_user(&company, &session.user_id)
        .await
        .ok()??;
    if user.status != UserStatus::Active {
        return None;
    }
    Some(UserPrincipal {
        company,
        user_id: user.id,
        email: user.email,
        role: user.role,
        session_token_hash: token_hash,
    })
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
            // A user belongs to one company and may address that one only. The
            // storage partition already makes a cross-company session
            // unresolvable; this is the second, explicit line of defense.
            GqlAuth::User(user) => {
                if user.company == *company {
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
            // A user sees their own company and nothing else — not even that
            // other companies exist on this host.
            GqlAuth::User(user) => all.into_iter().filter(|id| *id == user.company).collect(),
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
