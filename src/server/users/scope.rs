//! Resolving the addressed company on the *unauthenticated* login routes.
//!
//! [`ScopedCompany`](crate::server::ops::ScopedCompany) cannot serve these.
//! It resolves the company *and* enforces operator/platform auth in one step,
//! which is right for the write plane and exactly wrong here: asking for a
//! magic link is something a person does precisely because they have no
//! credential yet. Using it would demand an operator token to log in — and
//! would appear to work in dev only because of the no-token escape hatch, which
//! is the worst failure mode available: green tests, 401 in production.
//!
//! [`PublicCompany`] therefore mirrors `ScopedCompany`'s dual-addressing
//! (`/api/v1/companies/{id}/…` and the single-company `/api/v1/company/…`
//! alias) and does no auth at all. Every route mounted on it must be safe to
//! call anonymously.

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

/// Registers `mr` under both the `{id}` platform form and the single-company
/// alias, mirroring [`scoped`](crate::server::ops::scoped) for public routes.
pub(crate) fn public_scoped(suffix: &str, mr: MethodRouter<AppState>) -> Router<AppState> {
    Router::new()
        .route(&format!("/api/v1/companies/{{id}}{suffix}"), mr.clone())
        .route(&format!("/api/v1/company{suffix}"), mr)
}

/// The addressed company, resolved with **no authentication**.
pub(crate) struct PublicCompany {
    /// The resolved runtime for the addressed company.
    pub(crate) runtime: Arc<CompanyRuntime>,
}

impl FromRequestParts<AppState> for PublicCompany {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Sniff `{id}` without consuming it, exactly as ScopedCompany does, so
        // handlers may still extract their own path params.
        let id = RawPathParams::from_request_parts(parts, state)
            .await
            .ok()
            .and_then(|params| {
                params
                    .iter()
                    .find(|(key, _)| *key == "id")
                    .map(|(_, value)| value.to_string())
            });

        let runtime = match id {
            Some(id) => state
                .registry()
                .get(&CompanyId::new(&id))
                .ok_or_else(|| ApiError(OpenCompanyError::CompanyNotFound(id)).into_response())?,
            None => state.registry().sole().ok_or_else(|| {
                ApiError(OpenCompanyError::CompanyNotFound(
                    "single-company".to_string(),
                ))
                .into_response()
            })?,
        };
        Ok(PublicCompany { runtime })
    }
}
