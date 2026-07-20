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
    /// Whether an admin issued a temporary password this user must replace.
    ///
    /// Carried on the principal so the check is a boundary at the extractor
    /// rather than a convention the console is trusted to honor.
    pub must_change_password: bool,
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
/// - `Platform`: a verified platform/tenant token — the *machine* credential,
///   used by the hosting layer.
/// - `User`: a human collaborator with a session cookie, scoped to one company.
///
/// There is no unauthenticated principal. There used to be `Dev` (no operator
/// token configured ⇒ allow everything) and `Operator` (a shared bearer), but
/// the operator token could never actually be set, so `Dev` was the only
/// reachable state and every deployment served every route to anyone. Humans
/// authenticate as themselves now; nothing is open.
#[derive(Clone, Debug)]
pub enum GqlAuth {
    /// Platform mode with verified tenant claims.
    Platform(PlatformClaims),
    /// A human collaborator of exactly one company.
    User(UserPrincipal),
}

/// The one failure [`resolve_claims`] can return: an unauthenticated request.
/// The REST extractors map this to `401`; the GraphQL handler to an error.
#[derive(Clone, Copy, Debug)]
pub struct Unauthorized;

/// Turns an `Authorization` header into a **machine** [`GqlAuth`] principal.
///
/// Only platform mode has machine credentials: the bearer must verify, else
/// [`Unauthorized`]. Without `platform_auth` configured there is no machine
/// credential to present, so this always fails and callers must come in as a
/// human via [`resolve_principal`].
///
/// **It can never return [`GqlAuth::User`], and that is load-bearing.** Routes
/// that mean to serve only the hosting layer (provisioning, suspension) resolve
/// through this and therefore cannot be reached by a session cookie, no matter
/// what it contains — not because each route checks, but because the type it
/// receives cannot represent a human.
pub fn resolve_claims(headers: &HeaderMap, state: &AppState) -> Result<GqlAuth, Unauthorized> {
    let platform = state.config().platform_auth.as_ref().ok_or(Unauthorized)?;
    let token = bearer(headers).ok_or(Unauthorized)?;
    let claims = platform.verifier.verify(token).map_err(|_| Unauthorized)?;
    Ok(GqlAuth::Platform(claims))
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
        must_change_password: user.must_change_password,
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
            GqlAuth::Platform(claims) => {
                if claims.has_platform_scope() {
                    return Ok(());
                }
                let owner = state.owner_of(company);
                if owner.as_deref() == Some(crate::app::canonical_tenant(&claims.tenant))
                    && claims.may_address(company)
                {
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
            GqlAuth::Platform(claims) if claims.has_platform_scope() => all,
            GqlAuth::Platform(claims) => all
                .into_iter()
                .filter(|id| {
                    state.owner_of(id).as_deref()
                        == Some(crate::app::canonical_tenant(&claims.tenant))
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
