//! HTTP error mapping.
//!
//! [`OpenCompanyError`] stays axum-free (it is the shared crate error), so the
//! `IntoResponse` mapping lives here in a thin server-local newtype. Every error
//! renders the api.md envelope `{ "error": <message>, "code": <stable_code> }`
//! with a status derived from the variant.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::error::OpenCompanyError;

/// A server-local wrapper that renders an [`OpenCompanyError`] as an HTTP
/// response. Handlers return `Result<T, ApiError>`.
#[derive(Debug)]
pub struct ApiError(pub OpenCompanyError);

/// A compact wrapper for a response that has already been rendered by a
/// handler. Axum cannot use `Box<Response>` directly because it does not
/// implement [`IntoResponse`]; this wrapper preserves that response unchanged.
#[derive(Debug)]
pub struct Rejection(Box<Response>);

impl From<Response> for Rejection {
    fn from(response: Response) -> Self {
        Self(Box::new(response))
    }
}

impl From<ApiError> for Rejection {
    fn from(error: ApiError) -> Self {
        Self(Box::new(error.into_response()))
    }
}

impl From<OpenCompanyError> for Rejection {
    fn from(error: OpenCompanyError) -> Self {
        Self(Box::new(ApiError(error).into_response()))
    }
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response {
        *self.0
    }
}

impl From<OpenCompanyError> for ApiError {
    fn from(error: OpenCompanyError) -> Self {
        Self(error)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self(OpenCompanyError::from(error))
    }
}

impl ApiError {
    /// The HTTP status this error maps to.
    pub fn status(&self) -> StatusCode {
        // Issue #1008: a failed workflow run wraps its cause so it can carry the
        // partial run back for the journal. Classify the cause — a graph that
        // failed validation is still a 400 whether or not partial rows rode home
        // with it.
        match self.0.unwrapped() {
            OpenCompanyError::CompanyNotFound(_) | OpenCompanyError::NotFound(_) => {
                StatusCode::NOT_FOUND
            }
            #[cfg(any(feature = "openhuman", feature = "mcp"))]
            OpenCompanyError::McpServerNotFound(_) => StatusCode::NOT_FOUND,
            OpenCompanyError::ManifestInvalid { .. }
            | OpenCompanyError::ManifestParse(_, _)
            | OpenCompanyError::MissingManifest(_)
            | OpenCompanyError::InvalidRequest(_)
            | OpenCompanyError::WorkflowInvalid { .. } => StatusCode::BAD_REQUEST,
            // Issue #1017: a stored company data file that no longer parses is
            // the caller's bad input, not a server fault — 400, so a route like
            // get_workflow (and the render `?` in update_company_workflow)
            // surfaces the parse message instead of a blank 500. One central
            // mapping covers every current and future caller that lets a
            // DataParse escape as an ApiError.
            OpenCompanyError::DataParse { .. } => StatusCode::BAD_REQUEST,
            // A file that parses but fails validation is a semantically bad
            // payload the caller can correct — 422.
            OpenCompanyError::DataInvalid { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            OpenCompanyError::LifecycleConflict(_)
            | OpenCompanyError::Conflict(_)
            | OpenCompanyError::NotInBuild(_)
            | OpenCompanyError::NotConfigured(_)
            | OpenCompanyError::EmergencyStop(_)
            // Keys rework (#2306): a removal/clear/disable/switch that would
            // strand a dependent (the default, an agent pair, a workload) is
            // a conflict with existing config, same status as the other
            // durable-invariant conflicts above; the `in_use` code and the
            // additive `usedBy` envelope key (below) are what a client
            // branches on to retry with `confirmInUse: true`.
            | OpenCompanyError::InUse { .. } => StatusCode::CONFLICT,
            // A runtime swap is in progress and clears itself within a turn, so
            // this is a retry-me, not a refusal (issue #290).
            OpenCompanyError::Quiescing(_) => StatusCode::SERVICE_UNAVAILABLE,
            OpenCompanyError::ToolNotGranted(_) | OpenCompanyError::Forbidden(_) => {
                StatusCode::FORBIDDEN
            }
            OpenCompanyError::BudgetExceeded(_) => StatusCode::PAYMENT_REQUIRED,
            // 413 — for both ways an upload can be too big. The store's per-file
            // cap raises this variant with the file and the limit named; the
            // upload route's `DefaultBodyLimit` backstop raises it too, after
            // classifying the parse failure axum reports (issue #647). The two
            // limits are deliberately *different* numbers, but they are one
            // cause as far as a caller is concerned, so they share this status
            // and the `workspace_quota_exceeded` code rather than splitting into
            // two vocabularies for "too big".
            OpenCompanyError::WorkspaceQuota(_) => StatusCode::PAYLOAD_TOO_LARGE,
            // The company is at its concurrent-run ceiling (issue #401). The run
            // was never started; the operator waits for a slot or raises the cap,
            // so this is a rate-limit refusal — 429, the same status
            // `provision.rs` already answers when a tenant asks for too much at
            // once — rather than a 4xx that reads as a bad request.
            OpenCompanyError::WorkflowRunLimit { .. } => StatusCode::TOO_MANY_REQUESTS,
            // The company is at its roster-proposal burst cap: each call runs
            // a real, metered model pass, so this is a rate-limit refusal too.
            OpenCompanyError::RosterProposalRateLimit { .. } => StatusCode::TOO_MANY_REQUESTS,
            // The TinyHumans hub: an unreachable backend degrades to 503 so
            // callers retry; any other protocol failure is an upstream 502. In
            // practice feedback forwarding swallows these (the report is
            // already stored locally), so this mapping only applies if a future
            // caller propagates one.
            OpenCompanyError::TinyHumans { code, .. } if code == "unreachable" => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            // The shared feedback board needs a TinyHumans account; an instance
            // provisioned without one simply has no board. That is a missing
            // surface, not an upstream fault, so it is a 404 the console can
            // treat as "hide the board" without special-casing a 502.
            OpenCompanyError::TinyHumans { code, .. } if code == "no_board" => {
                StatusCode::NOT_FOUND
            }
            OpenCompanyError::TinyHumans { .. } => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        // Every error renders the api.md envelope `{ "error", "code" }`. A
        // `WorkflowInvalid` additively carries a `problems` array (issue #1016)
        // so the console can highlight the exact node + field; the key is present
        // ONLY for that variant, so every other error stays byte-for-byte as
        // before and no existing client sees a new field.
        let body = match &self.0 {
            OpenCompanyError::WorkflowInvalid { problems } => Json(json!({
                "error": self.0.to_string(),
                "code": self.0.code(),
                "problems": problems,
            })),
            // Keys rework (#2306): additively carries `usedBy` so the console
            // can render exactly what an `in_use` refusal would break, the
            // same way `WorkflowInvalid` above carries `problems`. See
            // `docs/key-reworks/in-use-guards.md`.
            OpenCompanyError::InUse { used_by, .. } => Json(json!({
                "error": self.0.to_string(),
                "code": self.0.code(),
                "usedBy": used_by,
            })),
            _ => Json(json!({
                "error": self.0.to_string(),
                "code": self.0.code(),
            })),
        };
        (status, body).into_response()
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
