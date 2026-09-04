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
            | OpenCompanyError::NotConfigured(_) => StatusCode::CONFLICT,
            // A runtime swap is in progress and clears itself within a turn, so
            // this is a retry-me, not a refusal (issue #290).
            OpenCompanyError::Quiescing(_) => StatusCode::SERVICE_UNAVAILABLE,
            OpenCompanyError::ToolNotGranted(_) => StatusCode::FORBIDDEN,
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
            // tiny.place transport: an unreachable backend degrades to 503 so
            // callers retry; any other protocol failure is an upstream 502.
            OpenCompanyError::Tinyplace { code, .. } if code == "unreachable" => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            OpenCompanyError::Tinyplace { .. } => StatusCode::BAD_GATEWAY,
            // The TinyHumans hub degrades the same way. In practice feedback
            // forwarding swallows these (the report is already stored locally),
            // so this mapping only applies if a future caller propagates one.
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
            _ => Json(json!({
                "error": self.0.to_string(),
                "code": self.0.code(),
            })),
        };
        (status, body).into_response()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::header::HeaderName;
    use std::path::PathBuf;

    #[tokio::test]
    async fn rejection_preserves_a_pre_rendered_response() {
        let response = Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(HeaderName::from_static("set-cookie"), "session=token")
            .body("redirect".into_response().into_body())
            .unwrap();

        let response = Rejection::from(response).into_response();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()["set-cookie"], "session=token");
        assert_eq!(
            &to_bytes(response.into_body(), usize::MAX).await.unwrap()[..],
            b"redirect"
        );
    }

    #[test]
    fn maps_variants_to_status_and_code() {
        let not_found = ApiError(OpenCompanyError::CompanyNotFound("acme".into()));
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        assert_eq!(not_found.0.code(), "company_not_found");

        let conflict = ApiError(OpenCompanyError::LifecycleConflict("paused".into()));
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        let invalid = ApiError(OpenCompanyError::ManifestInvalid {
            path: PathBuf::from("company.toml"),
            problems: vec!["missing name".into()],
        });
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        let tool = ApiError(OpenCompanyError::ToolNotGranted("payment.send".into()));
        assert_eq!(tool.status(), StatusCode::FORBIDDEN);

        // Issue #401: the concurrent-run ceiling renders as 429, and its stable
        // code is what the console (and the orchestrator tool) branch on.
        let run_cap = ApiError(OpenCompanyError::WorkflowRunLimit { limit: 8 });
        assert_eq!(run_cap.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(run_cap.0.code(), "workflow_run_limit");

        let other = ApiError(OpenCompanyError::Store("disk full".into()));
        assert_eq!(other.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// Issue #2081: the two permanent capability states share `409` with every
    /// ordinary conflict, so the **code** is the only thing that tells them
    /// apart. A console that cannot make that distinction can only offer the
    /// recoverable reading of both, and asks operators to reload a section no
    /// reload can fill.
    #[test]
    fn capability_refusals_keep_409_and_carry_their_own_codes() {
        let not_in_build = ApiError(OpenCompanyError::NotInBuild(
            "Composio is not compiled into this build".into(),
        ));
        assert_eq!(not_in_build.status(), StatusCode::CONFLICT);
        assert_eq!(not_in_build.0.code(), "not_in_build");

        let not_configured = ApiError(OpenCompanyError::NotConfigured(
            "no Composio credential is available for this company".into(),
        ));
        assert_eq!(not_configured.status(), StatusCode::CONFLICT);
        assert_eq!(not_configured.0.code(), "not_configured");

        // The neighbours they must stay distinguishable from: same status,
        // opposite advice.
        let ordinary = ApiError(OpenCompanyError::Conflict(
            "a desk with that id exists".into(),
        ));
        assert_eq!(ordinary.status(), StatusCode::CONFLICT);
        assert_eq!(ordinary.0.code(), "conflict");

        let lifecycle = ApiError(OpenCompanyError::LifecycleConflict("paused".into()));
        assert_eq!(lifecycle.status(), StatusCode::CONFLICT);
        assert_eq!(lifecycle.0.code(), "lifecycle_conflict");
    }

    /// The message must reach the operator unprefixed. `Conflict` renders as
    /// `conflict: {0}`, and these carry prose that already names the control to
    /// go and use — a `conflict: ` in front of it is noise the console shows.
    #[tokio::test]
    async fn capability_refusals_render_their_message_verbatim() {
        let response = ApiError(OpenCompanyError::NotInBuild(
            "Composio is not compiled into this build".into(),
        ))
        .into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "Composio is not compiled into this build");
        assert_eq!(json["code"], "not_in_build");
    }

    #[test]
    fn maps_company_data_errors_to_400_and_422() {
        // Issue #1017: an unparseable company data file (e.g. a workflow whose
        // stored body is no longer valid TOML) is the caller's bad input, not a
        // server fault — 400 with the stable `data_parse` code, so a route like
        // get_workflow surfaces the parse message instead of a blank 500.
        let parse = ApiError(OpenCompanyError::DataParse {
            path: PathBuf::from("workflows/weekly-digest.toml"),
            message: "expected `=` after key".into(),
        });
        assert_eq!(parse.status(), StatusCode::BAD_REQUEST);
        assert_eq!(parse.0.code(), "data_parse");

        // A file that parses but fails validation is a semantically bad payload —
        // 422, matching how the render `?` in update_company_workflow should
        // report a graph the caller can fix.
        let invalid = ApiError(OpenCompanyError::DataInvalid {
            path: PathBuf::from("workflows/weekly-digest.toml"),
            problems: vec!["missing a trigger".into()],
        });
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(invalid.0.code(), "data_invalid");
    }

    #[test]
    fn maps_tinyplace_transport_to_503_and_502() {
        let unreachable = ApiError(OpenCompanyError::tinyplace("unreachable", "offline"));
        assert_eq!(unreachable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(unreachable.0.code(), "tinyplace_unreachable");

        let upstream = ApiError(OpenCompanyError::tinyplace("http_500", "boom"));
        assert_eq!(upstream.status(), StatusCode::BAD_GATEWAY);
    }

    /// Issue #1016: a `WorkflowInvalid` renders a `400` whose envelope additively
    /// carries a `problems` array, each entry naming its node + field.
    #[tokio::test]
    async fn workflow_invalid_envelope_carries_problems() {
        use crate::error::WorkflowProblem;
        use axum::body::to_bytes;

        let err = ApiError(OpenCompanyError::WorkflowInvalid {
            problems: vec![WorkflowProblem::node_field(
                "greet",
                "config.url",
                "greet has a bad url.",
            )],
        });
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.0.code(), "workflow_invalid");

        let body = err.into_response().into_body();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["code"], "workflow_invalid");
        assert_eq!(json["problems"][0]["node_id"], "greet");
        assert_eq!(json["problems"][0]["field"], "config.url");
        assert!(
            json["error"].as_str().unwrap().contains("bad url"),
            "{json}"
        );
    }

    /// Every other error keeps the plain `{ error, code }` envelope with NO
    /// `problems` key — the new field is additive and scoped to `WorkflowInvalid`.
    #[tokio::test]
    async fn non_workflow_error_has_no_problems_key() {
        use axum::body::to_bytes;

        let err = ApiError(OpenCompanyError::CompanyNotFound("acme".into()));
        let body = err.into_response().into_body();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["code"], "company_not_found");
        assert!(json.get("problems").is_none(), "{json}");
    }
}
