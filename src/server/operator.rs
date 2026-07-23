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
//! Auth is a platform token (hosting layer) or a human's session cookie; there
//! is no unauthenticated path. See [`server::users`](crate::server::users).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, OutboundMessage, Verdict,
};
use crate::runtime::types::{ApprovalSummary, CompanyStatus, CycleReport};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};
use crate::server::platform_auth::{CompanyAuth, authorize_address, refuse_until_password_changed};
use crate::server::provision::{emit_cycle_webhooks, emit_feedback_webhook};

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
        // The company's desks (group chats), under both scope forms — the
        // console builds its chat threads from these (issue #53).
        .merge(scoped("/desks", get(list_desks)))
}

/// One desk (group chat) as the console renders it. Mirrors `DeskDto` in
/// `frontend/src/api/types.ts`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeskDto {
    /// The desk id (the group-chat id; used as the chat thread id).
    id: String,
    /// The desk's display name.
    name: String,
    /// An optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// The teammate ids on this desk; the first is its lead.
    members: Vec<String>,
}

/// `GET {scope}/desks` — the company's desks, built from its manifest group
/// chats. Empty when the company defines none (the console then falls back to
/// its static default threads).
async fn list_desks(scope: ScopedCompany) -> Result<Json<Vec<DeskDto>>, Response> {
    let record = scope
        .runtime
        .store()
        .load(scope.id())
        .await
        .map_err(|e| ApiError(e).into_response())?;
    let desks = record
        .map(|record| {
            record
                .manifest
                .group_chats
                .iter()
                .map(|chat| DeskDto {
                    id: chat.id.clone(),
                    name: chat.name.clone(),
                    description: chat.description.clone(),
                    members: chat.members.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(desks))
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

/// `GET /api/v1/companies` — status of every company this principal may see.
///
/// A platform-scope token sees all of them; a tenant token sees only the
/// companies it owns; a user sees their own company and nothing else — not even
/// that others exist on this host.
async fn list_companies(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<CompanyStatus>>, ApiError> {
    let mut out = Vec::new();
    // `visible_companies` is the one place this filter lives, shared with the
    // GraphQL root, so REST and GraphQL cannot disagree about who sees what.
    for id in auth.visible_companies(&state) {
        if let Some(runtime) = state.registry().get(&id) {
            out.push(runtime.status().await?);
        }
    }
    Ok(Json(out))
}

/// `GET /api/v1/companies/{id}` — one company's status.
async fn company_status(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CompanyStatus>, Response> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp);
    }
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    runtime
        .status()
        .await
        .map(Json)
        .map_err(|e| ApiError(e).into_response())
}

/// The operator's chat request body.
///
/// WS3 extends the Phase-1 `{text}` body with an optional `chat` desk id
/// (single-responder in v1): replies are journaled against that desk so the
/// GraphQL `Chat.history` resolver can read them back. The field is accepted
/// under either `text` (Phase-1) or `message` (the console) key.
#[derive(Debug, Deserialize)]
struct ChatMessage {
    /// The operator's message text.
    #[serde(alias = "message")]
    text: String,
    /// The desk the message is addressed to. Defaults to the "General" desk.
    #[serde(default)]
    chat: Option<String>,
}

/// A chat or approval-resolution response: the company's channel replies.
#[derive(Debug, Serialize)]
struct ChatResponse {
    /// Channel responses produced by the cycle.
    responses: Vec<OutboundMessage>,
}

/// Runs one operator-chat cycle, returning the report and, when a complaint
/// intent captured feedback, the note that was captured (so the caller can emit
/// the `feedback.created` webhook).
async fn run_chat(
    runtime: Arc<CompanyRuntime>,
    message: ChatMessage,
    by: Option<Actor>,
) -> Result<(CycleReport, Option<String>), ApiError> {
    runtime.ensure_running().await?;
    // Operator-chat feedback intent: a complaint phrase ("that was wrong — flag
    // it") captures a feedback item alongside the normal cycle. Neutral chat
    // carries no intent, so ordinary messages are untouched.
    let feedback_note = if let Some(category) = crate::feedback::detect_chat_intent(&message.text) {
        runtime
            .capture_feedback(crate::feedback::FeedbackInput {
                category,
                note: message.text.clone(),
                work_ref: None,
                template_name: None,
                template_version: None,
            })
            .await?;
        Some(message.text.clone())
    } else {
        None
    };
    let report = runtime
        .run_cycle(vec![CompanyEvent::OperatorMessage {
            text: message.text,
            by,
            // Thread the addressed desk through so the orchestrator brain can
            // route to that desk's lead member (issue #53).
            chat: message.chat,
        }])
        .await?;
    Ok((report, feedback_note))
}

/// Runs a chat cycle and emits any implied webhooks, rendering the responses.
async fn chat_and_emit(
    state: &AppState,
    id: &CompanyId,
    runtime: Arc<CompanyRuntime>,
    message: ChatMessage,
    by: Option<Actor>,
) -> Result<Json<ChatResponse>, ApiError> {
    // The default desk for an unaddressed message.
    let desk = message
        .chat
        .clone()
        .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string());
    let (report, feedback_note) = run_chat(runtime.clone(), message, by).await?;
    emit_cycle_webhooks(state, id, &report).await;
    if let Some(note) = feedback_note {
        emit_feedback_webhook(state, id, &note).await;
    }
    // Journal each reply against the addressed desk so desk history can be read
    // back (GraphQL `Chat.history`, WS2c). Single-responder in v1.
    for response in &report.responses {
        let _ = runtime
            .events()
            .append(
                id,
                CompanyEvent::AgentReply {
                    chat_id: desk.clone(),
                    agent_id: response.channel.clone(),
                    text: response.text.clone(),
                },
            )
            .await;
    }
    Ok(Json(ChatResponse {
        responses: report.responses,
    }))
}

/// Resolves who is sending a chat message.
///
/// Chat is the one surface both machines and humans drive, so it accepts
/// either. A signed-in user is attributed to themselves; a platform credential
/// yields `None`, which reads back as "operator" — there is no person behind it
/// to name.
async fn chat_actor(
    headers: &HeaderMap,
    state: &AppState,
    company: &CompanyId,
) -> Result<Option<Actor>, Response> {
    use crate::server::graphql::auth::{GqlAuth, resolve_principal};

    let auth = resolve_principal(headers, state, Some(company))
        .await
        .map_err(|_| unauthorized_response())?;
    if let Some(resp) = authorize_address(state, &auth, company) {
        return Err(resp);
    }
    if let Some(resp) = refuse_until_password_changed(&auth) {
        return Err(resp);
    }
    Ok(match auth {
        GqlAuth::User(user) => Some(Actor {
            kind: ActorKind::User,
            id: user.user_id,
        }),
        GqlAuth::Platform(_) => None,
    })
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "unauthorized", "code": "unauthorized" })),
    )
        .into_response()
}

/// `POST /api/v1/companies/{id}/chat`.
async fn operator_chat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(message): Json<ChatMessage>,
) -> Result<Json<ChatResponse>, Response> {
    let company = CompanyId::new(&id);
    let by = chat_actor(&headers, &state, &company).await?;
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    chat_and_emit(&state, &company, runtime, message, by)
        .await
        .map_err(IntoResponse::into_response)
}

/// `POST /api/v1/company/chat` (single-company alias).
async fn operator_chat_single(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(message): Json<ChatMessage>,
) -> Result<Json<ChatResponse>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    let id = runtime.id().clone();
    let by = chat_actor(&headers, &state, &id).await?;
    chat_and_emit(&state, &id, runtime, message, by)
        .await
        .map_err(IntoResponse::into_response)
}

/// `GET /api/v1/companies/{id}/approvals`.
async fn list_approvals(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ApprovalSummary>>, Response> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp);
    }
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    Ok(Json(runtime.pending_approvals()))
}

/// `GET /api/v1/company/approvals` (single-company alias).
async fn list_approvals_single(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<ApprovalSummary>>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    // The sole company IS the addressed one, so the principal is checked
    // against it exactly as on the `{id}` form.
    if let Some(resp) = authorize_address(&state, &auth, runtime.id()) {
        return Err(resp);
    }
    Ok(Json(runtime.pending_approvals()))
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
    state: &AppState,
    company: &CompanyId,
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
    emit_cycle_webhooks(state, company, &report).await;
    Ok(Json(ChatResponse {
        responses: report.responses,
    }))
}

/// `POST /api/v1/companies/{id}/approvals/{aid}`.
async fn resolve_approval(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path((id, aid)): Path<(String, String)>,
    Json(body): Json<ResolveApproval>,
) -> Result<Json<ChatResponse>, Response> {
    let company = CompanyId::new(&id);
    if let Some(resp) = authorize_address(&state, &auth, &company) {
        return Err(resp);
    }
    let runtime = lookup(&state, &id).map_err(IntoResponse::into_response)?;
    run_resolve(&state, &company, runtime, aid, body)
        .await
        .map_err(IntoResponse::into_response)
}

/// `POST /api/v1/company/approvals/{aid}` (single-company alias).
async fn resolve_approval_single(
    CompanyAuth(auth): CompanyAuth,
    State(state): State<AppState>,
    Path(aid): Path<String>,
    Json(body): Json<ResolveApproval>,
) -> Result<Json<ChatResponse>, Response> {
    let runtime = sole(&state).map_err(IntoResponse::into_response)?;
    let id = runtime.id().clone();
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return Err(resp);
    }
    if let Some(resp) = refuse_until_password_changed(&auth) {
        return Err(resp);
    }
    run_resolve(&state, &id, runtime, aid, body)
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
                overlay_agents: Vec::new(),
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
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
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
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
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

    /// End-to-end proof of the WS4 wire: with a [`HarnessBrain`] as the runtime's
    /// cognition, `POST /company/chat` returns the **agent's** reply rather than
    /// the echo brain's `"You said: …"`. The mock provider prefixes the routed
    /// message, so `"mock: hi"` proves the operator message reached an openhuman
    /// agent turn through the HTTP handler → `run_cycle` → brain path.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn chat_routes_through_the_harness_brain() {
        use crate::harness::provider::MockProvider;
        use crate::harness::{HarnessBrain, HarnessDeps, HarnessPool};
        use crate::ports::CompanyStore;
        use crate::store::{FsContextStore, FsOps};

        let home = home();
        let id = CompanyId::new("acme");
        let manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief Executive\"\n",
        )
        .unwrap();

        let record = CompanyRecord {
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
        };
        FsCompanyStore::new(home.to_path_buf())
            .save(&record)
            .await
            .unwrap();

        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(home.to_path_buf())),
            store: Arc::new(FsCompanyStore::new(home.to_path_buf())),
            meter: Some(Arc::new(FsOps::new(home.to_path_buf()))),
            workspace_root: home.to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: None,
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: crate::harness::orchestrator::DelegationQueue::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            secrets: None,
        };
        let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record);

        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .with_brain(Arc::new(brain))
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let text = value["responses"][0]["text"].as_str().unwrap();
        // The mock provider's `mock: ` prefix proves the message went through an
        // openhuman agent turn; the trailing `hi` is the operator message the
        // agent forwarded (the agent prepends a date/time context line). Crucially
        // it is NOT the echo brain's `"You said: hi"`.
        assert!(text.starts_with("mock: "), "not an agent reply: {text:?}");
        assert!(
            text.trim_end().ends_with("hi"),
            "message not forwarded: {text:?}"
        );
        assert_ne!(text, "You said: hi", "still routing through the echo brain");
        assert_eq!(value["responses"][0]["channel"], "operator");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn desks_route_returns_the_company_desks() {
        // The default test manifest defines no group chats, so the route answers
        // 200 with an empty list (the console then falls back to its defaults).
        let home = home();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/desks")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
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
    async fn chat_by_id_matches_registered_company() {
        let home = home();
        let state = state_with_company(&home, "running").await;
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/companies/acme/chat")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
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
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // 401, not 404: the caller holds no credential for `ghost`, and
        // authentication precedes existence. Answering "no such company" to an
        // unauthenticated caller would let anyone enumerate which companies a
        // host runs. A user of `ghost` gets a real 404.
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
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
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
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
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
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
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
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
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
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
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
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
    async fn a_session_is_required_and_sufficient() {
        // Replaces `operator_token_guards_routes`. That token could never be
        // set, so the test only ever proved the guard worked in a state no
        // deployment could reach; every real host served this route to anyone.
        let home = home();
        let state = build_state(&home, "running", AppConfig::default()).await;

        // No credential at all: closed.
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // A garbage bearer buys nothing either — there is no bearer path in
        // prosumer mode at all now.
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .header("authorization", "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // A signed-in human gets their own company.
        let cookie = crate::server::test_support::seed_admin(&state, "acme").await;
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        tokio::fs::remove_dir_all(&home).await.ok();
    }
}
