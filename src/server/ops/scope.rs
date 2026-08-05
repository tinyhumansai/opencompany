//! Dual-scope routing for the write plane.
//!
//! Every write route is registered under **both** addressing forms — the
//! platform `…/companies/{id}/…` form and the prosumer single-company alias
//! `…/company/…` — by [`scoped`]. A [`ScopedCompany`] extractor resolves the
//! target [`CompanyRuntime`] and enforces authorization for whichever form the
//! request used:
//!
//! - `…/companies/{id}` → [`CompanyAuth`] + `authorize_address`
//!   (a tenant token may only address a company it owns).
//! - `…/company` → [`OperatorAuth`] + [`CompanyRegistry::sole`].
//!
//! ## Addressing is not authority
//!
//! [`ScopedCompany`] answers "may this principal talk to this company at all",
//! and nothing more. It is the right guard for the many writes this product
//! deliberately leaves open to **any** member — sending a chat message, opening
//! a task, adding a teammate. It is the wrong guard, on its own, for a write
//! that decides something *on behalf of* the company. For those,
//! [`AdminScopedCompany`] puts the role check in the handler's signature rather
//! than leaving it to a call the handler has to remember to make (issue #403).

use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRequestParts, RawPathParams};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::{Actor, ActorKind, CompanyId};
use crate::server::error::ApiError;
use crate::server::graphql::auth::{GqlAuth, UserPrincipal};
use crate::server::platform_auth::{CompanyAuth, authorize_address, refuse_until_password_changed};
use crate::server::users::admin::require_admin;

/// Registers `mr` under both the `{id}` platform form and the single-company
/// alias. `suffix` is the path after the scope prefix (e.g. `"/tasks"` or
/// `"/tasks/{task_id}"`).
pub(crate) fn scoped(suffix: &str, mr: MethodRouter<AppState>) -> Router<AppState> {
    Router::new()
        .route(&format!("/api/v1/companies/{{id}}{suffix}"), mr.clone())
        .route(&format!("/api/v1/company{suffix}"), mr)
}

/// The company a write targets, resolved from the request's scope form with
/// authorization already enforced.
pub(crate) struct ScopedCompany {
    /// The resolved runtime for the addressed company.
    pub(crate) runtime: Arc<CompanyRuntime>,
    /// The signed-in human behind the request, when there is one (issue #335).
    ///
    /// The extractor already resolves the principal to authorize the call; this
    /// carries the person half of it forward so a route that journals an
    /// operator action can attribute it, instead of every write landing as an
    /// anonymous `by: None`. A machine credential (the platform scope) is
    /// `None` — there is no person behind it to name, which is exactly the
    /// distinction [`CompanyEvent::OperatorMessage`](crate::ports::types::CompanyEvent::OperatorMessage)'s
    /// `by` already draws.
    pub(crate) actor: Option<Actor>,
}

impl ScopedCompany {
    /// The addressed company's id.
    pub(crate) fn id(&self) -> &CompanyId {
        self.runtime.id()
    }
}

impl FromRequestParts<AppState> for ScopedCompany {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Detect the `{id}` path param without consuming it (handlers may still
        // extract sub-resource ids). Its presence selects the scope form.
        let id = RawPathParams::from_request_parts(parts, state)
            .await
            .ok()
            .and_then(|params| {
                params
                    .iter()
                    .find(|(key, _)| *key == "id")
                    .map(|(_, value)| value.to_string())
            });

        // Resolve the company first: on the alias form the sole registered
        // company IS the addressed one, and the principal must be checked
        // against it just the same.
        let runtime = match &id {
            Some(id) => state.registry().get(&CompanyId::new(id)).ok_or_else(|| {
                ApiError(OpenCompanyError::CompanyNotFound(id.clone())).into_response()
            })?,
            None => state.registry().sole().ok_or_else(|| {
                ApiError(OpenCompanyError::CompanyNotFound(
                    "single-company".to_string(),
                ))
                .into_response()
            })?,
        };
        let company = runtime.id().clone();

        let CompanyAuth(auth) = CompanyAuth::from_request_parts(parts, state).await?;
        if let Some(resp) = authorize_address(state, &auth, &company) {
            return Err(resp);
        }
        // A temporary password is a boundary, not a suggestion.
        if let Some(resp) = refuse_until_password_changed(&auth) {
            return Err(resp);
        }
        // Keep the person, drop the credential: only a human principal names an
        // actor, and only the user id travels — never the email or the role.
        let actor = match auth {
            GqlAuth::User(user) => Some(Actor {
                kind: ActorKind::User,
                id: user.user_id,
            }),
            GqlAuth::Platform(_) => None,
        };
        Ok(ScopedCompany { runtime, actor })
    }
}

/// The company a write targets, resolved exactly as [`ScopedCompany`] does and
/// then narrowed to a principal who **administers** that company (issue #403).
///
/// Use this for a write that settles something on the company's behalf — what
/// credential its agents present, which third-party account they act through —
/// as opposed to a write a member makes for themselves. Stating the requirement
/// in the extractor is the point: a route declares its authority in its
/// signature, so the guard cannot be lost by editing a handler body, and a new
/// route cannot acquire this class of gap by simply forgetting a call.
///
/// It composes with [`require_admin`] rather than restating it, so "who may
/// administer this company" stays decided in exactly one place. Note what that
/// inherits: [`require_admin`] resolves through a **human** session only, so a
/// machine credential (the platform scope) is refused here as unauthenticated.
/// That is deliberate — this extractor's whole purpose is to name the person
/// accountable for the change, and a token names nobody.
pub(crate) struct AdminScopedCompany {
    /// The resolved runtime for the addressed company.
    pub(crate) runtime: Arc<CompanyRuntime>,
    /// The admin behind the request. Always a real person — see the type docs.
    pub(crate) admin: UserPrincipal,
}

impl AdminScopedCompany {
    /// The addressed company's id.
    pub(crate) fn id(&self) -> &CompanyId {
        self.runtime.id()
    }

    /// The admin as a journal [`Actor`], for attributing the write they made.
    pub(crate) fn actor(&self) -> Actor {
        Actor {
            kind: ActorKind::User,
            id: self.admin.user_id.clone(),
        }
    }
}

impl FromRequestParts<AppState> for AdminScopedCompany {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ScopedCompany { runtime, .. } = ScopedCompany::from_request_parts(parts, state).await?;
        let admin = require_admin(&parts.headers, state, &runtime).await?;
        Ok(AdminScopedCompany { runtime, admin })
    }
}
