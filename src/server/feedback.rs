//! Operator HTTP surface for the feedback loop.
//!
//! `POST /api/v1/companies/{id}/feedback` (and the single-company alias
//! `POST /api/v1/company/feedback`) captures a feedback item and runs the
//! scrub-then-preview gate. The response never leaks a blocked value: a scrub
//! abort returns `{ blocked: true, reason }`, a `preview` returns the byte-exact
//! final body, and a satisfied consent returns the filed issue URL (or a
//! prefilled manual link when no token is configured).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::feedback::service::FeedbackResponse;
use crate::feedback::types::FeedbackInput;
use crate::ports::types::CompanyId;
use crate::server::error::ApiError;
use crate::server::operator::OperatorAuth;
use crate::server::platform_auth::{PlatformOrOperatorAuth, authorize_address};

/// Builds the feedback route fragment, merged into the main router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/companies/{id}/feedback", post(submit))
        .route("/api/v1/company/feedback", post(submit_single))
}

/// The feedback submission body: the capture input plus a `preview` flag.
#[derive(Debug, Deserialize)]
struct FeedbackRequest {
    /// The capture fields (category, note, work_ref, template).
    #[serde(flatten)]
    input: FeedbackInput,
    /// When true, return the exact final body instead of filing.
    #[serde(default)]
    preview: bool,
}

fn lookup(state: &AppState, id: &str) -> Result<Arc<CompanyRuntime>, ApiError> {
    state
        .registry()
        .get(&CompanyId::new(id))
        .ok_or_else(|| ApiError(OpenCompanyError::CompanyNotFound(id.to_string())))
}

fn sole(state: &AppState) -> Result<Arc<CompanyRuntime>, ApiError> {
    state.registry().sole().ok_or_else(|| {
        ApiError(OpenCompanyError::CompanyNotFound(
            "single-company".to_string(),
        ))
    })
}

async fn run(
    runtime: Arc<CompanyRuntime>,
    body: FeedbackRequest,
) -> Result<Json<FeedbackResponse>, ApiError> {
    runtime.ensure_running().await?;
    let response = runtime.submit_feedback(body.input, body.preview).await?;
    Ok(Json(response))
}

/// `POST /api/v1/companies/{id}/feedback`.
///
/// A per-company route: like every other `/companies/{id}/…` handler it takes
/// platform-or-operator auth and enforces tenant ownership, so one tenant can
/// never file feedback (or trigger issue-filing) against another's company.
async fn submit(
    PlatformOrOperatorAuth(claims): PlatformOrOperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<FeedbackRequest>,
) -> Result<Json<FeedbackResponse>, Response> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &claims, &company) {
        return Err(resp);
    }
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    run(runtime, body)
        .await
        .map_err(IntoResponse::into_response)
}

/// `POST /api/v1/company/feedback` (single-company alias).
async fn submit_single(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Json(body): Json<FeedbackRequest>,
) -> Result<Json<FeedbackResponse>, ApiError> {
    run(sole(&state)?, body).await
}

#[cfg(test)]
mod test {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::company::CompanyManifest;
    use crate::feedback::MockGitHubClient;
    use crate::feedback::types::ConsentMode;
    use crate::ports::SecretStore;
    use crate::ports::types::SecretValue;
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsSecretStore;
    use crate::{AppConfig, AppState};

    fn home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "opencompany-feedback-{}",
            crate::ports::generate_id()
        ))
    }

    fn manifest() -> CompanyManifest {
        toml::from_str(
            r#"
            [company]
            name = "Acme"
            handle = "acme"
            [[agent]]
            id = "dana_roe"
            role = "Analyst"
            [policy]
            mode = "full"
            "#,
        )
        .unwrap()
    }

    /// Builds state with a company wired for `auto` consent and a mock GitHub
    /// client, plus a seeded secret to exercise the scrub-abort path.
    async fn state_with_company(home: &std::path::Path, github: Arc<MockGitHubClient>) -> AppState {
        let id = CompanyId::new("acme");
        // Seed a secret whose value the scrubber must abort on if it appears.
        let secrets = FsSecretStore::new(home.to_path_buf());
        secrets
            .set(&id, "github_token", SecretValue("ghp_LEAKEDSECRET".into()))
            .await
            .unwrap();

        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(id.clone())
            .with_github(github)
            .with_feedback_consent(ConsentMode::Auto)
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, Arc::new(runtime));
        state
    }

    async fn post_json(app: &Router, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    // The offline end-to-end path: capture → scrub (secret aborts, email
    // redacted) → preview → mock-file with dedupe.
    #[tokio::test]
    async fn capture_scrub_preview_file_and_dedupe() {
        let home = home();
        let github = Arc::new(MockGitHubClient::new());
        let state = state_with_company(&home, github.clone()).await;
        let app = router(state);

        // 1. A report containing the seeded secret is blocked (scrub fail-closed)
        //    and nothing is filed.
        let (status, value) = post_json(
            &app,
            "/api/v1/company/feedback",
            r#"{"category":"bug","note":"token ghp_LEAKEDSECRET broke the run"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["blocked"], true);
        assert_eq!(value["filed"], false);
        assert!(github.created().is_empty());

        // 2. A clean report with an email, in preview mode, returns the byte-exact
        //    final body with the email redacted and nothing filed.
        let (status, value) = post_json(
            &app,
            "/api/v1/company/feedback",
            r#"{"category":"wrong-output","note":"email dana@acme.co bounced","preview":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["filed"], false);
        let preview = value["preview_body"].as_str().expect("preview body");
        assert!(preview.contains("⟨redacted:email⟩"), "got {preview}");
        assert!(!preview.contains("dana@acme.co"));
        // Signed with the company @handle for provenance.
        assert!(preview.contains("— filed by @acme"));
        assert!(github.created().is_empty());

        // 3. Filing (auto consent) creates one issue with the agent-filed label.
        let (status, value) = post_json(
            &app,
            "/api/v1/company/feedback",
            r#"{"category":"wrong-output","note":"the invoice total was wrong"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["filed"], true);
        assert!(value["issue_url"].is_string());
        let created = github.created();
        assert_eq!(created.len(), 1);
        assert!(
            created[0]
                .labels
                .contains(&"source/agent-filed".to_string())
        );

        // 4. A second filing with the same title dedupes: it comments, does not
        //    create a duplicate.
        let (status, value) = post_json(
            &app,
            "/api/v1/company/feedback",
            r#"{"category":"wrong-output","note":"the invoice total was wrong"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["deduped"], true);
        assert_eq!(github.created().len(), 1);
        assert_eq!(github.comments().len(), 1);

        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn feedback_for_unknown_company_is_404() {
        let home = home();
        let github = Arc::new(MockGitHubClient::new());
        let state = state_with_company(&home, github).await;
        let app = router(state);

        let (status, value) = post_json(
            &app,
            "/api/v1/companies/ghost/feedback",
            r#"{"category":"bug","note":"x"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(value["code"], "company_not_found");
        tokio::fs::remove_dir_all(&home).await.ok();
    }
}
