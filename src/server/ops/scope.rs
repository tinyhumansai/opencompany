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
//! [`AdminScopedCompany`] puts the authority check in the handler's signature
//! rather than leaving it to a call the handler has to remember to make (issue
//! #403).
//!
//! Note what the pair is **not**: a read-scope / write-scope split. "Write" and
//! "requires authority" are different questions here, and this product answers
//! them differently on purpose — a member may open a task, post to a
//! discussion, or add a teammate, and gating every write on a role would take
//! capabilities away that are deliberately theirs. The axis that matters is
//! whether the write decides something *for the company*, which is why the
//! second extractor is named for the authority it demands rather than for the
//! HTTP verb it happens to sit behind.

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
    /// Whether this principal may read the *money and arguments* an effect
    /// carries — issue #618's rule, answered once here for routes that project
    /// such a field (issue #705).
    ///
    /// **The decision, not the credential.** The comment below on `actor` is
    /// the rule for this struct: the role does not travel. So this carries what
    /// [`may_read_approval_contents`](crate::server::approval_visibility::may_read_approval_contents)
    /// *decided*, resolved at the edge where the principal still exists, rather
    /// than the role that decided it. Nothing downstream can re-derive the
    /// answer differently, because nothing downstream has the input.
    ///
    /// Deliberately the same predicate the Approvals read uses rather than a
    /// second one. Two role checks that can disagree about what an operator may
    /// see is a worse outcome than the leak either was written to close.
    pub(crate) may_read_contents: bool,
    /// Whether this principal may see an `admin_only` row — an owner-fallback
    /// report journaled under
    /// [`OWNER_FALLBACK_REPORT_AUTHOR`](crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR)
    /// (issue #1781 review, Codex P1).
    ///
    /// `GqlAuth::Platform` (no person behind the credential) gets `true` here,
    /// matching [`Viewer::Operator`](crate::server::chat_history::Viewer::Operator)'s
    /// existing, already unrestricted access to the rest of a company's
    /// history — an admin-only row is not a narrower case than that. Mirrors
    /// the same resolution the GraphQL `Chat.history` resolver makes
    /// (`company.rs`), so the SSE feed and a reload can never disagree about
    /// which viewer sees one.
    pub(crate) is_admin: bool,
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
        // Resolved here, while the principal still exists, because the role is
        // about to be dropped on purpose (issue #705). This is the answer, not
        // the input to it.
        let may_read_contents =
            crate::server::approval_visibility::may_read_approval_contents(&auth);
        // Same "resolved while the principal still exists" reasoning as
        // `may_read_contents` above, for the admin-only-row gate (issue #1781
        // review, Codex P1). Unlike `may_read_contents`, the machine principal
        // is unrestricted here — see the field's doc.
        let is_admin = match &auth {
            GqlAuth::User(user) => user.may_administer(),
            GqlAuth::Platform(_) => true,
        };
        // Keep the person, drop the credential: only a human principal names an
        // actor, and only the user id travels — never the email or the role.
        let actor = match auth {
            GqlAuth::User(user) => Some(Actor {
                kind: ActorKind::User,
                id: user.user_id,
            }),
            GqlAuth::Platform(_) => None,
        };
        Ok(ScopedCompany {
            runtime,
            actor,
            may_read_contents,
            is_admin,
        })
    }
}

/// The company a write targets, resolved exactly as [`ScopedCompany`] does and
/// then narrowed to a principal entitled to decide **on that company's behalf**
/// (issue #403).
///
/// Use this for a write that settles something for the company — what
/// credential its agents present, which third-party account they act through —
/// as opposed to a write a member makes for themselves. Stating the requirement
/// in the extractor is the point: a route declares its authority in its
/// signature, so the guard cannot be lost by editing a handler body, and a new
/// route cannot acquire this class of gap by simply forgetting a call.
///
/// ## Who qualifies
///
/// Exactly the two principals `docs/spec/runtime/config.md` defines, each held
/// to what it actually is:
///
/// * a **human** whose session says they administer this company. Resolved
///   through [`require_admin`] rather than restating the rule, so "who may
///   administer this company" stays decided in one place. A member is `403`.
/// * the **machine** principal that may address this company — the hosting
///   control plane. It is *not* refused: it provisions the tenant and holds its
///   database credentials, so denying it a route it already sits above would be
///   ceremony rather than a boundary, and it would break the documented
///   platform principal. What it does not get is anonymity — see below.
///
/// ## Attribution is not optional
///
/// [`Self::actor`] is infallible by construction: whichever principal got here
/// is named. A human is [`ActorKind::User`] plus their user id; the machine is
/// [`ActorKind::System`] plus its tenant. The gap this closes is as much about
/// unattributed writes as unauthorized ones — a change to what a company
/// connects through that nobody's name is on is one nobody can review later.
pub(crate) struct AdminScopedCompany {
    /// The resolved runtime for the addressed company.
    pub(crate) runtime: Arc<CompanyRuntime>,
    /// The human admin behind the request, when the principal is a person.
    /// `None` for the machine principal, which is a tenant and not a user.
    #[allow(dead_code, reason = "carried for handlers that need the person")]
    pub(crate) admin: Option<UserPrincipal>,
    /// Who made this change. Always identified — never anonymous.
    actor: Actor,
}

impl AdminScopedCompany {
    /// The addressed company's id.
    pub(crate) fn id(&self) -> &CompanyId {
        self.runtime.id()
    }

    /// Who made the change, for attributing it in the journal.
    pub(crate) fn actor(&self) -> Actor {
        self.actor.clone()
    }
}

impl FromRequestParts<AppState> for AdminScopedCompany {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Addressing, company resolution and the temporary-password boundary
        // are already exactly right; this only adds the authority question on
        // top of them.
        let ScopedCompany {
            runtime,
            actor,
            // Not carried on: every principal that reaches this extractor may
            // read contents by construction (an admin, or the platform bearer
            // which this extractor admits and #618 refuses contents to anyway).
            may_read_contents: _,
            // Not carried on, for the same reason: every principal that
            // reaches this extractor already IS an admin by construction (see
            // above), so this extractor's own callers have no narrower
            // question to ask than `ScopedCompany` already answered.
            is_admin: _,
        } = ScopedCompany::from_request_parts(parts, state).await?;
        match actor {
            // A human. `ScopedCompany` keeps the person but drops the role, so
            // the role has to be re-resolved — which is the whole reason this
            // class of gap existed.
            Some(actor) => {
                let peer = parts
                    .extensions
                    .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                    .map(|info| info.0);
                let admin = require_admin(&parts.headers, state, &runtime, peer)
                    .await
                    .map_err(IntoResponse::into_response)?;
                Ok(AdminScopedCompany {
                    runtime,
                    admin: Some(admin),
                    actor,
                })
            }
            // The machine principal. It already passed `authorize_address`, so
            // it owns this company (or holds the platform scope). Name it.
            None => {
                let CompanyAuth(auth) = CompanyAuth::from_request_parts(parts, state).await?;
                let actor = Actor {
                    kind: ActorKind::System,
                    id: crate::server::platform_auth::acting_tenant(&auth),
                };
                Ok(AdminScopedCompany {
                    runtime,
                    admin: None,
                    actor,
                })
            }
        }
    }
}
