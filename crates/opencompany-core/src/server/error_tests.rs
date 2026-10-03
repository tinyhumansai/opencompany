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

    let roster_cap = ApiError(OpenCompanyError::RosterProposalRateLimit {
        limit: 5,
        window_secs: 60,
    });
    assert_eq!(roster_cap.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(roster_cap.0.code(), "roster_proposal_rate_limited");

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

/// Keys rework (#2306): an `InUse` refusal is a 409 with the stable code
/// `in_use`, and its envelope additively carries `usedBy` — the same
/// one-off pattern `WorkflowInvalid` uses for `problems` above — so a
/// client can render exactly what a confirmed retry would break.
#[tokio::test]
async fn in_use_envelope_carries_used_by() {
    use crate::error::{UsedBy, UsedByAgent};
    use axum::body::to_bytes;

    let err = ApiError(OpenCompanyError::InUse {
        message: "Anthropic is used by the company default and 2 agents: \
                  Researcher, Web search."
            .to_string(),
        used_by: UsedBy {
            default: true,
            agents: vec![
                UsedByAgent {
                    id: "researcher".to_string(),
                    name: "Researcher".to_string(),
                },
                UsedByAgent {
                    id: "web_search".to_string(),
                    name: "Web search".to_string(),
                },
            ],
            surfaces: Vec::new(),
        },
    });
    assert_eq!(err.status(), StatusCode::CONFLICT);
    assert_eq!(err.0.code(), "in_use");

    let body = err.into_response().into_body();
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["code"], "in_use");
    assert!(json["error"].as_str().unwrap().contains("Anthropic"));
    assert_eq!(json["usedBy"]["default"], true);
    assert_eq!(json["usedBy"]["agents"][0]["id"], "researcher");
    assert!(
        json["usedBy"].get("surfaces").is_none(),
        "empty surfaces are omitted"
    );
    assert!(json.get("problems").is_none(), "{json}");
}

/// [`crate::error::UsedBy`]'s own contract: every field omitted, never
/// `false`/`[]`, when there is nothing to say.
#[test]
fn used_by_omits_every_empty_field() {
    use crate::error::UsedBy;

    let value = serde_json::to_value(UsedBy::default()).unwrap();
    assert_eq!(value, serde_json::json!({}), "{value}");
    assert!(UsedBy::default().is_empty());
}
