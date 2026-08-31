//! Authenticated operator controls for the unredacted deep-trace store.
//!
//! Deep trace bodies can contain credentials and other sensitive model/tool
//! material. This surface therefore uses [`AdminScopedCompany`], rather than
//! the ordinary company-scope extractor: members may read the scrubbed trace,
//! but only an authenticated administrator (or the hosting platform principal)
//! may destroy its unredacted bodies.
//!
//! Routes are available under both the platform company scope and the
//! single-company operator alias:
//!
//! * `DELETE …/deep-trace` purges every deep-trace record for the company.
//! * `DELETE …/deep-trace/{run_id}` purges one run's detail.

use axum::Router;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::delete;

use crate::AppState;
use crate::server::error::ApiError;
use crate::server::ops::{AdminScopedCompany, scoped};

/// Builds the authenticated deep-trace purge routes.
pub fn router() -> Router<AppState> {
    scoped("/deep-trace", delete(purge_all))
        .merge(scoped("/deep-trace/{run_id}", delete(purge_run)))
}

#[derive(Debug, serde::Deserialize)]
struct RunPath {
    run_id: String,
}

/// `DELETE …/deep-trace` — destroy all unredacted details for this company.
async fn purge_all(company: AdminScopedCompany) -> Result<StatusCode, ApiError> {
    company
        .runtime
        .deep_trace()
        .purge_deep_trace(company.id(), None)
        .await
        .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE …/deep-trace/{run_id}` — destroy one attempt's unredacted details.
async fn purge_run(
    company: AdminScopedCompany,
    Path(RunPath { run_id }): Path<RunPath>,
) -> Result<StatusCode, ApiError> {
    company
        .runtime
        .deep_trace()
        .purge_deep_trace(company.id(), Some(&run_id))
        .await
        .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}
