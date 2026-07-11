//! End-to-end axum tests for provisioning, per-tenant auth, lifecycle controls,
//! quotas, and webhook emission. All offline (default build, no features).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::ports::Brain;
use crate::ports::types::{
    CompanyEvent, CompanyId, CompressedTrace, CycleRequest, CycleResult, Effect, EffectGroup,
    EventSeq, OutboundMessage, TokenUsage,
};
use crate::ports::{CycleHost, EventLog};
use crate::runtime::RuntimeBuilder;
use crate::server::platform_auth::{PlatformAuthConfig, PlatformClaims, StaticPlatformVerifier};
use crate::server::router;
use crate::server::webhook::{WebhookConfig, WebhookKind};
use crate::store::FsEventLog;
use crate::{AppConfig, AppState};

const PLATFORM_SECRET: &str = "plat-secret";

const ACME_TOML: &str = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n";

fn home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("oc-provision-{}", crate::ports::generate_id()))
}

fn platform_state(home: &std::path::Path, max_per_tenant: Option<usize>) -> AppState {
    let verifier = Arc::new(StaticPlatformVerifier::new(PLATFORM_SECRET));
    AppState::new(AppConfig::default())
        .with_home(home.to_path_buf())
        .with_platform_auth(PlatformAuthConfig::new(verifier))
        .with_quota(None, max_per_tenant)
}

fn tenant_token(tenant: &str, scopes: &[&str]) -> String {
    StaticPlatformVerifier::tenant_token(&PlatformClaims {
        tenant: tenant.to_string(),
        scopes: scopes.iter().map(|s| s.to_string()).collect::<HashSet<_>>(),
        companies: None,
    })
}

fn provision_req(token: Option<&str>, toml: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v1/companies")
        .header("content-type", "text/plain");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(toml.to_string())).unwrap()
}

fn get_req(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

fn post_req(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("POST").uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

fn chat_req(uri: &str, token: Option<&str>, text: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder
        .body(Body::from(format!(r#"{{"text":"{text}"}}"#)))
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------------------
// Provisioning + status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provision_then_list_then_status() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    // Provision with a platform-scope token.
    let response = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = json_body(response).await;
    assert_eq!(body["id"], "acme");
    assert_eq!(body["lifecycle"], "running");

    // List shows it.
    let list = app
        .clone()
        .oneshot(get_req("/api/v1/companies", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let list_body = json_body(list).await;
    assert_eq!(list_body.as_array().unwrap().len(), 1);

    // Status by id.
    let status = app
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(json_body(status).await["id"], "acme");
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn provision_accepts_json_envelope_with_explicit_id() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    let body = serde_json::json!({ "manifest_toml": ACME_TOML, "id": "custom-id" }).to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/companies")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {PLATFORM_SECRET}"))
        .body(Body::from(body))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(json_body(response).await["id"], "custom-id");
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn provision_requires_platform_scope() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    // No token → 401.
    let unauthorized = app
        .clone()
        .oneshot(provision_req(None, ACME_TOML))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    // Tenant-only token (no platform scope) → 403.
    let token = tenant_token("tenant:acme", &["operator"]);
    let forbidden = app
        .oneshot(provision_req(Some(&token), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(forbidden).await["code"], "forbidden");
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn invalid_manifest_is_400() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    // Empty company name fails validation.
    let bad = "[company]\nname = \"\"\n";
    let response = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), bad))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(response).await["code"], "manifest_invalid");
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn quota_rejects_when_exceeded() {
    let home = home();
    let state = platform_state(&home, Some(1));
    let app = router(state);

    let first = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let globex = "[company]\nname = \"Globex\"\n";
    let second = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), globex))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json_body(second).await["code"], "quota_exceeded");
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn duplicate_id_conflicts() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    let first = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let dup = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(dup.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(dup).await["code"], "company_exists");
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pause_toggles_and_chat_409() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // Pause → paused.
    let paused = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/pause",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    assert_eq!(json_body(paused).await["lifecycle"], "paused");

    // Chat is 409 while paused.
    let conflict = app
        .clone()
        .oneshot(chat_req(
            "/api/v1/companies/acme/chat",
            Some(PLATFORM_SECRET),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);

    // Resume → running, chat 200.
    let resumed = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/resume",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    assert_eq!(json_body(resumed).await["lifecycle"], "running");

    let ok = app
        .oneshot(chat_req(
            "/api/v1/companies/acme/chat",
            Some(PLATFORM_SECRET),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn suspend_requires_platform_scope_and_blocks_chat() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // A tenant-only token cannot suspend.
    let tenant = tenant_token("tenant:platform", &["operator"]);
    let forbidden = app
        .clone()
        .oneshot(post_req("/api/v1/companies/acme/suspend", Some(&tenant)))
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    // Platform scope suspends.
    let suspended = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/suspend",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(suspended.status(), StatusCode::OK);
    assert_eq!(json_body(suspended).await["lifecycle"], "suspended");

    // Chat is blocked.
    let conflict = app
        .oneshot(chat_req(
            "/api/v1/companies/acme/chat",
            Some(PLATFORM_SECRET),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn foreign_tenant_cannot_file_feedback() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    // acme is owned by tenant:platform.
    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // A different tenant's token must not reach acme's feedback route.
    let other = tenant_token("tenant:other", &["operator"]);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/companies/acme/feedback")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {other}"))
        .body(Body::from(r#"{"category":"bug","note":"not yours"}"#))
        .unwrap();
    let denied = app.oneshot(req).await.unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn owner_cannot_resume_a_platform_suspension() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // Platform suspends the tenant.
    let suspended = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/suspend",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(suspended.status(), StatusCode::OK);

    // The owner's tenant token must NOT be able to lift the suspension.
    let tenant = tenant_token("tenant:platform", &["operator"]);
    let denied = app
        .clone()
        .oneshot(post_req("/api/v1/companies/acme/resume", Some(&tenant)))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    // Platform scope can lift it.
    let resumed = app
        .oneshot(post_req(
            "/api/v1/companies/acme/resume",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    assert_eq!(json_body(resumed).await["lifecycle"], "running");
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn archive_removes_from_registry() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    let archived = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/archive",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(archived.status(), StatusCode::OK);
    assert_eq!(json_body(archived).await["lifecycle"], "archived");

    // Now unaddressable: status 404, chat 404.
    let status = app
        .clone()
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::NOT_FOUND);

    let chat = app
        .oneshot(chat_req(
            "/api/v1/companies/acme/chat",
            Some(PLATFORM_SECRET),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn cross_tenant_access_forbidden() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    // Tenant B provisions (its token carries the platform scope).
    let b_platform = tenant_token("tenant:b", &["platform", "operator"]);
    let created = app
        .clone()
        .oneshot(provision_req(Some(&b_platform), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    // Tenant A (no platform scope, different tenant) cannot address it.
    let a_token = tenant_token("tenant:a", &["operator"]);
    let forbidden = app
        .oneshot(get_req("/api/v1/companies/acme", Some(&a_token)))
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    std::fs::remove_dir_all(&home).ok();
}

#[tokio::test]
async fn lifecycle_transition_recorded_as_event() {
    let home = home();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    app.oneshot(post_req(
        "/api/v1/companies/acme/pause",
        Some(PLATFORM_SECRET),
    ))
    .await
    .unwrap();

    // The audit trail carries a LifecycleChanged running -> paused.
    let events = FsEventLog::new(home.clone());
    let stored = events
        .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    let found = stored.iter().any(|e| {
        matches!(
            &e.event,
            CompanyEvent::LifecycleChanged { from, to, .. } if from == "running" && to == "paused"
        )
    });
    assert!(found, "expected a LifecycleChanged event, got {stored:?}");
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// Webhooks
// ---------------------------------------------------------------------------

/// A brain that emits one supervised effect per operator message (parks under a
/// supervised policy), so a cycle produces an `approval.requested` webhook.
struct EffectBrain {
    effect: Effect,
}

#[async_trait]
impl Brain for EffectBrain {
    async fn run_cycle(
        &self,
        req: CycleRequest,
        host: &dyn CycleHost,
    ) -> crate::Result<CycleResult> {
        let mut responses = Vec::new();
        for event in &req.events {
            if let CompanyEvent::OperatorMessage { text } = event {
                host.emit_effect(self.effect.clone()).await?;
                responses.push(OutboundMessage {
                    channel: "operator".into(),
                    text: format!("handled: {text}"),
                });
            }
        }
        Ok(CycleResult {
            channel_responses: responses,
            new_traces: vec![CompressedTrace::now(&req.cycle_id, "effect cycle")],
            ledger_deltas: Vec::new(),
            token_usage: TokenUsage::default(),
        })
    }
}

#[tokio::test]
async fn webhook_emitted_on_approval_requested() {
    let home = home();
    // Prosumer mode (no platform_auth) plus a recording webhook sink.
    let (webhook, sink) = WebhookConfig::recording("tenant-secret");
    let state = AppState::new(AppConfig::default())
        .with_home(home.clone())
        .with_webhook(webhook);

    // A supervised company whose brain parks a filing.submit effect.
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"supervised\"\n").unwrap();
    let sign_effect = Effect {
        kind: "filing.submit".into(),
        group: EffectGroup::Sign,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: false,
        payload: serde_json::Value::Null,
    };
    let runtime = RuntimeBuilder::new(home.clone(), manifest)
        .with_id(CompanyId::new("acme"))
        .with_brain(Arc::new(EffectBrain {
            effect: sign_effect,
        }))
        .build()
        .await
        .unwrap();
    state
        .registry()
        .insert(CompanyId::new("acme"), Arc::new(runtime));

    let app = router(state);
    let chat = app
        .oneshot(chat_req("/api/v1/companies/acme/chat", None, "file it"))
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::OK);

    let delivered = sink.delivered();
    let approval = delivered
        .iter()
        .find(|(event, _)| event.kind == WebhookKind::ApprovalRequested)
        .expect("an approval_requested webhook was delivered");
    // The delivery carries a non-empty signature header value.
    assert!(!approval.1.is_empty());
    assert!(approval.1.starts_with("kh1="));
    std::fs::remove_dir_all(&home).ok();
}
