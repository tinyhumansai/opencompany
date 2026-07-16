//! Dual-scope routing for the write plane.
//!
//! Every write route is registered under **both** addressing forms — the
//! platform `…/companies/{id}/…` form and the prosumer single-company alias
//! `…/company/…` — by [`scoped`]. A [`ScopedCompany`] extractor resolves the
//! target [`CompanyRuntime`] and enforces authorization for whichever form the
//! request used:
//!
//! - `…/companies/{id}` → [`PlatformOrOperatorAuth`] + `authorize_address`
//!   (a tenant token may only address a company it owns).
//! - `…/company` → [`OperatorAuth`] + [`CompanyRegistry::sole`].

use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRequestParts, RawPathParams};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;
use crate::server::error::ApiError;
use crate::server::operator::OperatorAuth;
use crate::server::platform_auth::{PlatformOrOperatorAuth, authorize_address};

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

        match id {
            Some(id) => {
                let PlatformOrOperatorAuth(claims) =
                    PlatformOrOperatorAuth::from_request_parts(parts, state).await?;
                let company = CompanyId::new(&id);
                if let Some(resp) = authorize_address(state, &claims, &company) {
                    return Err(resp);
                }
                let runtime = state.registry().get(&company).ok_or_else(|| {
                    ApiError(OpenCompanyError::CompanyNotFound(id.clone())).into_response()
                })?;
                Ok(ScopedCompany { runtime })
            }
            None => {
                OperatorAuth::from_request_parts(parts, state).await?;
                let runtime = state.registry().sole().ok_or_else(|| {
                    ApiError(OpenCompanyError::CompanyNotFound(
                        "single-company".to_string(),
                    ))
                    .into_response()
                })?;
                Ok(ScopedCompany { runtime })
            }
        }
    }
}
