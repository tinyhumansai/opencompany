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
use crate::server::platform_auth::{CompanyAuth, authorize_address, refuse_until_password_changed};

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
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<FeedbackRequest>,
) -> Result<Json<FeedbackResponse>, Response> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp);
    }
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    run(runtime, body)
        .await
        .map_err(IntoResponse::into_response)
}

/// `POST /api/v1/company/feedback` (single-company alias).
async fn submit_single(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Json(body): Json<FeedbackRequest>,
) -> Result<Json<FeedbackResponse>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    // The sole company IS the addressed one, so the principal is checked
    // against it exactly as on the `{id}` form.
    if let Some(resp) = authorize_address(&state, &auth, runtime.id()) {
        return Err(resp);
    }
    if let Some(resp) = refuse_until_password_changed(&auth) {
        return Err(resp);
    }
    run(runtime, body)
        .await
        .map_err(IntoResponse::into_response)
}

#[cfg(test)]
mod test {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::company::CompanyManifest;
    use crate::feedback::tinyhumans::IngestOutcome;
    use crate::feedback::types::ConsentMode;
    use crate::feedback::{MockGitHubClient, MockTinyHumansClient};
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
        state_with_clients(home, github, None).await
    }

    /// As [`state_with_company`], but optionally provisioned with a TinyHumans
    /// hub — the "instance has a credential" case.
    async fn state_with_clients(
        home: &std::path::Path,
        github: Arc<MockGitHubClient>,
        hub: Option<Arc<MockTinyHumansClient>>,
    ) -> AppState {
        let id = CompanyId::new("acme");
        // Seed a secret whose value the scrubber must abort on if it appears.
        let secrets = FsSecretStore::new(home.to_path_buf());
        secrets
            .set(&id, "github_token", SecretValue("ghp_LEAKEDSECRET".into()))
            .await
            .unwrap();

        let mut builder = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(id.clone())
            .with_github(github)
            .with_feedback_consent(ConsentMode::Auto);
        if let Some(hub) = hub {
            builder = builder.with_tinyhumans_feedback(hub);
        }
        let runtime = builder.build().await.unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
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
                    // Every route needs a principal now; sign in as the
                    // harness admin so these assert feedback behavior.
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
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

        // 3. Filing (auto consent) creates one issue. The `POST .../feedback`
        //    route is operator-driven, so it carries the `source/operator`
        //    label from the four-axis triage taxonomy.
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
        assert!(created[0].labels.contains(&"source/operator".to_string()));
        assert!(created[0].labels.contains(&"sev/annoyance".to_string()));
        assert!(created[0].labels.contains(&"type/wrong-output".to_string()));

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

    // A provisioned instance forwards to the hub instead of filing, and what
    // crosses the boundary is the scrubbed body — not the operator's raw words.
    #[tokio::test]
    async fn provisioned_instance_forwards_scrubbed_body_and_files_nothing() {
        let home = home();
        let github = Arc::new(MockGitHubClient::new());
        let hub = Arc::new(MockTinyHumansClient::new());
        let state = state_with_clients(&home, github.clone(), Some(hub.clone())).await;
        let app = router(state);

        let (status, value) = post_json(
            &app,
            "/api/v1/company/feedback",
            r#"{"category":"wrong-output","note":"email dana@acme.co bounced"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["filed"], true);
        assert_eq!(value["destination"], "tinyhumans");
        // The hub decides whether an issue exists, so we report no URL.
        assert!(value["issue_url"].is_null());

        // Nothing was filed from here.
        assert!(github.created().is_empty());
        assert!(github.comments().is_empty());

        let forwarded = hub.forwarded();
        assert_eq!(forwarded.len(), 1);
        let sent = &forwarded[0];
        assert!(sent.body.contains("⟨redacted:email⟩"), "got {}", sent.body);
        assert!(!sent.body.contains("dana@acme.co"));
        assert!(sent.body.contains("— filed by @acme"));
        assert_eq!(sent.origin, "acme");
        assert_eq!(sent.wire_type(), "bug");
        assert!(!sent.external_ref.is_empty());

        tokio::fs::remove_dir_all(&home).await.ok();
    }

    // The scrub gate runs before the destination choice, so a secret is blocked
    // rather than forwarded.
    #[tokio::test]
    async fn scrub_abort_blocks_before_forwarding() {
        let home = home();
        let github = Arc::new(MockGitHubClient::new());
        let hub = Arc::new(MockTinyHumansClient::new());
        let state = state_with_clients(&home, github, Some(hub.clone())).await;
        let app = router(state);

        let (status, value) = post_json(
            &app,
            "/api/v1/company/feedback",
            r#"{"category":"bug","note":"token ghp_LEAKEDSECRET broke the run"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["blocked"], true);
        assert_eq!(value["destination"], "local");
        assert!(
            hub.forwarded().is_empty(),
            "a blocked report must not leave"
        );

        tokio::fs::remove_dir_all(&home).await.ok();
    }

    // An unreachable hub is a degraded success: the note is already stored, so
    // the operator gets a reason rather than a failed request.
    #[tokio::test]
    async fn unreachable_hub_degrades_to_local() {
        let home = home();
        let github = Arc::new(MockGitHubClient::new());
        let hub = Arc::new(MockTinyHumansClient::new().with_failure("connection refused"));
        let state = state_with_clients(&home, github.clone(), Some(hub)).await;
        let app = router(state);

        let (status, value) = post_json(
            &app,
            "/api/v1/company/feedback",
            r#"{"category":"bug","note":"the run crashed"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["filed"], false);
        assert_eq!(value["destination"], "local");
        assert!(value["reason"].is_string());
        // It must not silently fall back to filing an issue instead.
        assert!(github.created().is_empty());

        tokio::fs::remove_dir_all(&home).await.ok();
    }

    // Moderation rejection is reported as such, not as a transport failure.
    #[tokio::test]
    async fn hub_moderation_rejection_is_reported() {
        let home = home();
        let github = Arc::new(MockGitHubClient::new());
        let hub = Arc::new(
            MockTinyHumansClient::new().with_outcome(IngestOutcome::Rejected {
                reason: "off-topic".to_string(),
            }),
        );
        let state = state_with_clients(&home, github, Some(hub)).await;
        let app = router(state);

        let (status, value) = post_json(
            &app,
            "/api/v1/company/feedback",
            r#"{"category":"docs","note":"the docs are thin"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["filed"], false);
        assert_eq!(value["destination"], "tinyhumans");
        assert_eq!(value["reason"], "off-topic");

        tokio::fs::remove_dir_all(&home).await.ok();
    }

    // Without a credential the original GitHub path is untouched.
    #[tokio::test]
    async fn unprovisioned_instance_still_files_to_github() {
        let home = home();
        let github = Arc::new(MockGitHubClient::new());
        let state = state_with_company(&home, github.clone()).await;
        let app = router(state);

        let (status, value) = post_json(
            &app,
            "/api/v1/company/feedback",
            r#"{"category":"bug","note":"the run crashed"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["filed"], true);
        assert_eq!(value["destination"], "github");
        assert_eq!(github.created().len(), 1);

        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn feedback_for_unknown_company_is_404() {
        let home = home();
        let github = Arc::new(MockGitHubClient::new());
        let state = state_with_company(&home, github).await;
        let app = router(state);

        let (status, _value) = post_json(
            &app,
            "/api/v1/companies/ghost/feedback",
            r#"{"category":"bug","note":"x"}"#,
        )
        .await;
        // 401, not 404: authentication precedes existence, so an unauthenticated
        // caller cannot enumerate which companies this host runs.
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        tokio::fs::remove_dir_all(&home).await.ok();
    }
}
