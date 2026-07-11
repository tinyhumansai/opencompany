//! Operator HTTP surface: chat with a company and resolve its approvals.
//!
//! Phase 1 ships synchronous JSON chat: a `POST .../chat` enqueues an
//! `OperatorMessage`, runs exactly one cycle, and returns the channel
//! responses. SSE streaming (`/chat` streaming plus a `GET /events` work feed)
//! is the first follow-up.
//!
//! Both addressing forms are served by one router: the platform `{id}` form and
//! the prosumer single-company aliases (`/api/v1/company/...`) resolved through
//! [`CompanyRegistry::sole`](crate::runtime::CompanyRegistry::sole).
//!
//! Auth is a bearer operator token from [`AppConfig`](crate::AppConfig). When
//! no token is configured, Phase-1 dev mode allows local operator calls;
//! platform JWT is out of scope for this batch.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, OutboundMessage, Verdict,
};
use crate::runtime::types::{ApprovalSummary, CompanyStatus};
use crate::server::error::ApiError;

/// Builds the operator route fragment, merged into the main router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/companies", get(list_companies))
        .route("/api/v1/companies/{id}", get(company_status))
        .route("/api/v1/companies/{id}/chat", post(operator_chat))
        .route("/api/v1/companies/{id}/approvals", get(list_approvals))
        .route(
            "/api/v1/companies/{id}/approvals/{aid}",
            post(resolve_approval),
        )
        // Single-company aliases (no id; resolved via the sole registered company).
        .route("/api/v1/company/chat", post(operator_chat_single))
        .route("/api/v1/company/approvals", get(list_approvals_single))
        .route(
            "/api/v1/company/approvals/{aid}",
            post(resolve_approval_single),
        )
}

/// A bearer-token guard for operator routes.
///
/// In dev mode (`operator_token` unset) every request is allowed. When a token
/// is configured, the request must carry `Authorization: Bearer <token>`.
pub struct OperatorAuth;

impl FromRequestParts<AppState> for OperatorAuth {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(expected) = state.config().operator_token.as_deref() else {
            return Ok(OperatorAuth); // dev mode: no token required
        };
        let provided = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if provided == Some(expected) {
            Ok(OperatorAuth)
        } else {
            let body = Json(json!({ "error": "unauthorized", "code": "unauthorized" }));
            Err((StatusCode::UNAUTHORIZED, body).into_response())
        }
    }
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

/// `GET /api/v1/companies` — status of every registered company.
async fn list_companies(
    _auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<CompanyStatus>>, ApiError> {
    let mut out = Vec::new();
    for id in state.registry().list() {
        if let Some(runtime) = state.registry().get(&id) {
            out.push(runtime.status().await?);
        }
    }
    Ok(Json(out))
}

/// `GET /api/v1/companies/{id}` — one company's status.
async fn company_status(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CompanyStatus>, ApiError> {
    let runtime = lookup(&state, &id)?;
    Ok(Json(runtime.status().await?))
}

/// The operator's chat request body.
#[derive(Debug, Deserialize)]
struct ChatMessage {
    /// The operator's message text.
    text: String,
}

/// A chat or approval-resolution response: the company's channel replies.
#[derive(Debug, Serialize)]
struct ChatResponse {
    /// Channel responses produced by the cycle.
    responses: Vec<OutboundMessage>,
}

async fn run_chat(
    runtime: Arc<CompanyRuntime>,
    message: ChatMessage,
) -> Result<Json<ChatResponse>, ApiError> {
    runtime.ensure_running().await?;
    // Operator-chat feedback intent: a complaint phrase ("that was wrong — flag
    // it") captures a feedback item alongside the normal cycle. Neutral chat
    // carries no intent, so ordinary messages are untouched.
    if let Some(category) = crate::feedback::detect_chat_intent(&message.text) {
        runtime
            .capture_feedback(crate::feedback::FeedbackInput {
                category,
                note: message.text.clone(),
                work_ref: None,
                template_name: None,
                template_version: None,
            })
            .await?;
    }
    let report = runtime
        .run_cycle(vec![CompanyEvent::OperatorMessage { text: message.text }])
        .await?;
    Ok(Json(ChatResponse {
        responses: report.responses,
    }))
}

/// `POST /api/v1/companies/{id}/chat`.
async fn operator_chat(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(message): Json<ChatMessage>,
) -> Result<Json<ChatResponse>, ApiError> {
    run_chat(lookup(&state, &id)?, message).await
}

/// `POST /api/v1/company/chat` (single-company alias).
async fn operator_chat_single(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Json(message): Json<ChatMessage>,
) -> Result<Json<ChatResponse>, ApiError> {
    run_chat(sole(&state)?, message).await
}

/// `GET /api/v1/companies/{id}/approvals`.
async fn list_approvals(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ApprovalSummary>>, ApiError> {
    Ok(Json(lookup(&state, &id)?.pending_approvals()))
}

/// `GET /api/v1/company/approvals` (single-company alias).
async fn list_approvals_single(
    _auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<ApprovalSummary>>, ApiError> {
    Ok(Json(sole(&state)?.pending_approvals()))
}

/// The operator's resolution of a parked approval.
///
/// `verdict` stays `approve`/`deny`; the api.md wire enum gains no `edit`
/// verdict. Instead, an optional `amended_payload` paired with an `approve`
/// verdict routes to the approve-with-edit path. Pairing `amended_payload` with
/// `deny` is a contradiction and is rejected as a 400.
#[derive(Debug, Deserialize)]
struct ResolveApproval {
    /// `approve` or `deny`.
    verdict: Verdict,
    /// An optional operator note (reserved; not yet surfaced to the brain).
    #[allow(dead_code)]
    #[serde(default)]
    note: Option<String>,
    /// An optional payload edit; overlaid onto the parked effect on `approve`.
    #[serde(default)]
    amended_payload: Option<serde_json::Value>,
}

async fn run_resolve(
    runtime: Arc<CompanyRuntime>,
    approval_id: String,
    body: ResolveApproval,
) -> Result<Json<ChatResponse>, ApiError> {
    runtime.ensure_running().await?;
    let actor = Actor {
        kind: ActorKind::Operator,
        id: "operator".to_string(),
    };
    let id = ApprovalId::new(approval_id);
    let report = match (body.verdict, body.amended_payload) {
        (Verdict::Approve, Some(payload)) => {
            runtime
                .resolve_approval_amended(&id, payload, actor)
                .await?
        }
        (Verdict::Deny, Some(_)) => {
            return Err(ApiError(OpenCompanyError::InvalidRequest(
                "amended_payload cannot accompany a deny verdict".to_string(),
            )));
        }
        (verdict, None) => runtime.resolve_approval(&id, verdict, actor).await?,
    };
    Ok(Json(ChatResponse {
        responses: report.responses,
    }))
}

/// `POST /api/v1/companies/{id}/approvals/{aid}`.
async fn resolve_approval(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path((id, aid)): Path<(String, String)>,
    Json(body): Json<ResolveApproval>,
) -> Result<Json<ChatResponse>, ApiError> {
    run_resolve(lookup(&state, &id)?, aid, body).await
}

/// `POST /api/v1/company/approvals/{aid}` (single-company alias).
async fn resolve_approval_single(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path(aid): Path<String>,
    Json(body): Json<ResolveApproval>,
) -> Result<Json<ChatResponse>, ApiError> {
    run_resolve(sole(&state)?, aid, body).await
}

#[cfg(test)]
mod test {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;
    use crate::company::CompanyManifest;
    use crate::ports::types::CompanyRecord;
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("opencompany-http-{}", crate::ports::generate_id()))
    }

    fn manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
    }

    async fn state_with_company(home: &std::path::Path, lifecycle: &str) -> AppState {
        build_state(home, lifecycle, AppConfig::default()).await
    }

    async fn build_state(home: &std::path::Path, lifecycle: &str, config: AppConfig) -> AppState {
        // Pre-seed a record so the builder preserves the requested lifecycle.
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        use crate::ports::CompanyStore;
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest(),
                ledger: Vec::new(),
                lifecycle: lifecycle.to_string(),
            })
            .await
            .unwrap();

        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(config);
        state.registry().insert(id, Arc::new(runtime));
        state
    }

    #[tokio::test]
    async fn chat_returns_echoed_response() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["responses"][0]["text"], "You said: hi");
        assert_eq!(value["responses"][0]["channel"], "operator");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn chat_by_id_matches_registered_company() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/companies/acme/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"yo"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn unknown_company_is_404() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/companies/ghost/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["code"], "company_not_found");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn paused_company_chat_is_409() {
        let home = home();
        let state = state_with_company(&home, "paused").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn list_and_status_routes_report_the_company() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let list = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list.status(), StatusCode::OK);
        let bytes = to_bytes(list.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 1);
        assert_eq!(value[0]["id"], "acme");

        let status = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies/acme")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let bytes = to_bytes(status.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["id"], "acme");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn approvals_list_is_empty_before_any_park() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/approvals")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 0);
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn amended_approve_resolves_and_returns_responses() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        // An `approve` verdict carrying an amended payload routes to the
        // approve-with-edit path. Even against an unknown id it resolves
        // cleanly (nothing to execute) and the follow-up cycle replies.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/approvals/missing")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"verdict":"approve","amended_payload":{"text":"edited"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value["responses"].is_array());
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn deny_with_amended_payload_is_400() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/approvals/missing")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"verdict":"deny","amended_payload":{"text":"edited"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["code"], "invalid_request");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn operator_token_guards_routes() {
        let home = home();
        let config = AppConfig {
            operator_token: Some("s3cret".to_string()),
            ..AppConfig::default()
        };
        let state = build_state(&home, "running", config).await;
        let app = router(state);

        // Missing bearer token is rejected.
        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        // Wrong token is rejected.
        let wrong = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .header("authorization", "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        // Correct token is accepted.
        let ok = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .header("authorization", "Bearer s3cret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        tokio::fs::remove_dir_all(&home).await.ok();
    }
}
