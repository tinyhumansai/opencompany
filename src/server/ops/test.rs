//! HTTP-level tests for the ops write plane (domain, SMTP, inbox ingest).
//!
//! Every networked seam is exercised offline through injected mocks: a
//! [`StaticDnsResolver`](crate::company::dns::StaticDnsResolver) for domain
//! verify and a [`RecordingMailSender`](super::smtp::RecordingMailSender) for
//! the SMTP test send. The default build links no network crate.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::company::dns::StaticDnsResolver;
use crate::ports::CompanyStore;
use crate::ports::types::{CompanyId, CompanyRecord, SecretValue};
use crate::runtime::RuntimeBuilder;
use crate::server::ops::ConnectionsRuntime;
use crate::server::ops::mailer::RecordingMailSender;
use crate::server::router;
#[cfg(not(feature = "webhooks"))]
use crate::server::webhook::DefaultHashSigner;
use crate::server::webhook::WebhookSigner;
use crate::{AppConfig, AppState};

fn home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("opencompany-ops-{}", crate::ports::generate_id()))
}

fn manifest() -> CompanyManifest {
    toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
}

/// Builds state holding one running company `acme`, with `connections` injected.
async fn state_with(home: &std::path::Path, connections: ConnectionsRuntime) -> AppState {
    let store = crate::store::FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default())
        .with_home(home.to_path_buf())
        .with_connections(connections);
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn put_domain_returns_records() {
    let home = home();
    let state = state_with(&home, ConnectionsRuntime::new()).await;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/company/domain")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"domain":"acme.com"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["domain"], "acme.com");
    assert_eq!(value["verified"], false);
    assert_eq!(value["records"].as_array().unwrap().len(), 5);
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn verify_without_resolver_is_404_not_wired() {
    let home = home();
    let state = state_with(&home, ConnectionsRuntime::new()).await;
    let app = router(state);

    // Configure a domain first.
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/company/domain")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"domain":"acme.com"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/domain/verify")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let value = body_json(response).await;
    assert_eq!(value["code"], "not_wired");
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn verify_with_resolver_marks_verified() {
    let home = home();
    let resolver = Arc::new(StaticDnsResolver::fully_verifying("acme.com"));
    let state = state_with(&home, ConnectionsRuntime::new().with_dns(resolver)).await;
    let app = router(state);

    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/company/domain")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"domain":"acme.com"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/domain/verify")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["verified"], true);
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn put_smtp_hides_password() {
    let home = home();
    let state = state_with(&home, ConnectionsRuntime::new()).await;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/company/smtp")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"host":"smtp.acme.test","port":587,"security":"starttls","username":"u","password":"top-secret","from_email":"ceo@acme.test"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(!text.contains("top-secret"), "password leaked: {text}");
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["configured"], true);
    assert_eq!(value["host"], "smtp.acme.test");
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn smtp_test_without_sender_is_404() {
    let home = home();
    let state = state_with(&home, ConnectionsRuntime::new()).await;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/smtp/test")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn smtp_test_sends_and_records_outbound() {
    let home = home();
    let sender = Arc::new(RecordingMailSender::new());
    let state = state_with(&home, ConnectionsRuntime::new().with_mail(sender.clone())).await;
    let app = router(state);

    // Store credentials.
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/company/smtp")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"host":"smtp.acme.test","port":587,"username":"u","password":"pw","from_email":"ceo@acme.test"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/smtp/test")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"to":"ops@acme.test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["ok"], true);
    assert_eq!(sender.sent().len(), 1);
    assert_eq!(sender.sent()[0].1.to, "ops@acme.test");
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn ingest_bad_hmac_is_401_and_no_mail() {
    let home = home();
    let state = state_with(&home, ConnectionsRuntime::new()).await;
    // Seed the ingest secret.
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    runtime
        .secrets()
        .set(
            runtime.id(),
            super::INGEST_SECRET_KEY,
            SecretValue("s3cret".into()),
        )
        .await
        .unwrap();
    let app = router(state.clone());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/inboxes/ingest")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .header("x-opencompany-signature", "kh1=deadbeef")
                .body(Body::from(
                    r#"{"from":"a@x.test","to":"ceo@acme.test","subject":"hi","body":"yo"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // No mail was filed.
    assert!(
        runtime
            .inbox()
            .messages(runtime.id(), "ceo", usize::MAX, 0)
            .await
            .unwrap()
            .is_empty()
    );
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn ingest_good_hmac_files_mail() {
    let home = home();
    let state = state_with(&home, ConnectionsRuntime::new()).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    runtime
        .secrets()
        .set(
            runtime.id(),
            super::INGEST_SECRET_KEY,
            SecretValue("s3cret".into()),
        )
        .await
        .unwrap();
    let app = router(state.clone());

    let payload = r#"{"from":"a@x.test","to":"ceo@acme.test","subject":"hi","body":"yo"}"#;
    // Sign with whatever signer this build actually verifies with, mirroring
    // `inbox::signer()`. Hardcoding DefaultHashSigner made this test pass only
    // in the default build and 401 under `--features webhooks`, where the route
    // verifies with HmacSha256Signer.
    #[cfg(feature = "webhooks")]
    let signature = crate::server::webhook::HmacSha256Signer.sign("s3cret", payload.as_bytes());
    #[cfg(not(feature = "webhooks"))]
    let signature = DefaultHashSigner.sign("s3cret", payload.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/inboxes/ingest")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .header("x-opencompany-signature", signature)
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let value = body_json(response).await;
    assert_eq!(value["inbox"], "ceo");
    let mail = runtime
        .inbox()
        .messages(runtime.id(), "ceo", usize::MAX, 0)
        .await
        .unwrap();
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].from_email, "a@x.test");
    assert!(!mail[0].outbound);
    tokio::fs::remove_dir_all(&home).await.ok();
}
