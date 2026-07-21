//! Workflow execution: `POST /workflows/{wid}/run` under both scope forms.
//!
//! Runs the company's saved workflow graph `wid` on the embedded `tinyflows`
//! engine, with agent nodes executing on the harness pool (see
//! [`crate::ports::WorkflowRunner`]). The graph is loaded from the company's
//! on-disk source directory (`companies/<name>/workflows/<wid>.toml`).
//!
//! Execution is dependency-inverted behind the [`WorkflowRunner`] port: when no
//! runner is wired (the default build, or a runtime built without a harness) the
//! route reports `not_wired` — the same 404 seam the DNS/SMTP surfaces use — so
//! the default build stays inert.

use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;
use crate::company::load_company_workflows;
use crate::error::OpenCompanyError;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the workflow-run route fragment.
pub fn router() -> Router<AppState> {
    scoped("/workflows/{wid}/run", post(run_workflow))
}

/// The sub-resource path (`wid`); the scope `id` is consumed by the extractor.
#[derive(Debug, Deserialize)]
struct WorkflowPath {
    wid: String,
}

/// The run body: an optional trigger `input` payload seeded as the trigger
/// node's item. An empty object (`{}`) runs with a null input.
#[derive(Debug, Default, Deserialize)]
struct RunWorkflowBody {
    #[serde(default)]
    input: Value,
}

/// The run response: the engine's final state plus any nodes left pending
/// approval.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunWorkflowResponse {
    output: Value,
    pending_approvals: Vec<String>,
}

/// `POST …/workflows/{wid}/run` (both scope forms).
async fn run_workflow(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
    body: Option<Json<RunWorkflowBody>>,
) -> Result<Json<RunWorkflowResponse>, Response> {
    // No runner wired (default build / no harness) → the same "not wired" seam
    // the networked surfaces use.
    let Some(runner) = company.runtime.workflow_runner() else {
        return Err(super::not_wired("workflow execution"));
    };

    // Load the saved graph from the company's on-disk source directory. Without
    // one (platform-provisioned mode) there is nothing to run.
    let source_dir = company.runtime.source_dir().ok_or_else(|| {
        super::not_wired("workflow source (no company definition directory on this deployment)")
    })?;
    let file = load_company_workflows(source_dir, std::slice::from_ref(&wid))
        .map_err(|e| ApiError(e).into_response())?
        .into_iter()
        .next()
        .ok_or_else(|| {
            ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))).into_response()
        })?;

    let input = body.map(|Json(b)| b.input).unwrap_or(Value::Null);
    let run = runner
        .run(company.id(), &file, input)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    Ok(Json(RunWorkflowResponse {
        output: run.output,
        pending_approvals: run.pending_approvals,
    }))
}
