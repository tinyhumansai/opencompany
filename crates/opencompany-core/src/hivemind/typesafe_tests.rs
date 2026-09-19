//! Tests for [`super::typesafe`]: the live System One transport.
//!
//! Each round-trip test serves one canned response on an ephemeral port, the
//! pattern `chargebee::client_tests` uses, so the transport is exercised over a
//! real socket rather than against a hand-built future.

use super::{DEFAULT_BASE_URL, TypeSafeTransport};
use tinyhivemind_typesafe::{Error, SystemOneTransport};

/// Serves one canned response on an ephemeral port, as
/// `chargebee::client_tests` does, so the transport is exercised over a real
/// socket rather than against a hand-built future.
async fn stub(status: axum::http::StatusCode, body: &'static str) -> String {
    let app = axum::Router::new().fallback(axum::routing::any(move || async move {
        (status, [("content-type", "application/json")], body)
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// One well-formed routing Choice, built through the library's own types.
///
/// `SystemOneRequest` is `Serialize` only — it is a question this host asks,
/// never one it is told — so the fixture is constructed rather than decoded.
fn question() -> tinyhivemind_typesafe::SystemOneRequest {
    use std::collections::BTreeMap;
    use tinyhivemind_typesafe::{Question, SystemOneRequest};

    let mut criteria = BTreeMap::new();
    criteria.insert("checker".to_owned(), None);
    criteria.insert("theory".to_owned(), None);

    let mut questions = BTreeMap::new();
    questions.insert(
        "responder".to_owned(),
        Question::Choice {
            instructions: serde_json::json!("Which teammate should take this?"),
            criteria,
        },
    );

    SystemOneRequest {
        state: serde_json::json!({ "message": "verify the degree-9 identity" }),
        model: "test-model".to_owned(),
        questions,
    }
}

#[tokio::test]
async fn a_failing_status_carries_the_providers_own_diagnostic() {
    let base = stub(
        axum::http::StatusCode::BAD_REQUEST,
        r#"{"error":"roster_version is stale"}"#,
    )
    .await;
    let transport = TypeSafeTransport::with_base_url("k".to_owned(), base).expect("builds");

    let error = transport
        .evaluate(&question())
        .await
        .expect_err("400 is a transport error");

    match error {
        Error::Transport { status, message } => {
            assert_eq!(status, Some(400));
            assert!(
                message.contains("roster_version is stale"),
                "the provider's diagnostic was swallowed: {message}"
            );
        }
        other => panic!("expected a transport error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_success_this_host_cannot_decode_is_a_schema_drift_not_an_outage() {
    let base = stub(axum::http::StatusCode::OK, r#"{"unexpected":true}"#).await;
    let transport = TypeSafeTransport::with_base_url("k".to_owned(), base).expect("builds");

    let error = transport
        .evaluate(&question())
        .await
        .expect_err("an undecodable body is an error");

    match error {
        // The status rides along precisely so a 200-that-did-not-decode is
        // distinguishable from an outage in a log line.
        Error::Transport { status, message } => {
            assert_eq!(status, Some(200));
            assert!(
                message.contains("did not decode"),
                "schema drift was reported as something else: {message}"
            );
        }
        other => panic!("expected a transport error, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unreachable_endpoint_reports_no_status() {
    // Port 9 is `discard`; nothing listens, so this is a connect failure
    // rather than an HTTP one and there is no status to report.
    let transport =
        TypeSafeTransport::with_base_url("k".to_owned(), "http://127.0.0.1:9/so".to_owned())
            .expect("builds");

    match transport.evaluate(&question()).await {
        Err(Error::Transport { status, .. }) => assert_eq!(status, None),
        other => panic!("expected a statusless transport error, got {other:?}"),
    }
}

#[test]
fn debug_redacts_the_api_key() {
    let transport =
        TypeSafeTransport::new("sk-live-should-never-appear".to_owned()).expect("builds");
    let rendered = format!("{transport:?}");
    assert!(
        !rendered.contains("sk-live-should-never-appear"),
        "the API key leaked into Debug: {rendered}"
    );
    assert!(
        rendered.contains(DEFAULT_BASE_URL),
        "the endpoint should still be visible: {rendered}"
    );
}

#[test]
fn an_explicit_base_url_is_kept_verbatim() {
    let transport =
        TypeSafeTransport::with_base_url("k".to_owned(), "http://127.0.0.1:9/so".to_owned())
            .expect("builds");
    assert_eq!(transport.base_url(), "http://127.0.0.1:9/so");
}

#[test]
fn no_credential_is_routing_off_rather_than_a_failure() {
    // Serialised against the other env tests by reading only what this one
    // sets; `from_env` treats an absent OR blank key identically so the
    // ordering between them cannot matter.
    unsafe {
        std::env::remove_var(super::API_KEY_ENV);
    }
    assert!(
        TypeSafeTransport::from_env()
            .expect("an absent key is not an error")
            .is_none(),
        "a company with no routing credential must still boot"
    );
}

#[test]
fn the_routing_model_is_pinned_unless_the_environment_moves_it() {
    unsafe {
        std::env::remove_var(super::MODEL_ENV);
    }
    assert_eq!(super::routing_model(), super::DEFAULT_MODEL);
    // The alias, not a version: `jev-1.13` was refused live with
    // `Unknown model`, and a refused pin fails closed as a silent fallback.
    assert_eq!(super::DEFAULT_MODEL, "jev-latest");
}
