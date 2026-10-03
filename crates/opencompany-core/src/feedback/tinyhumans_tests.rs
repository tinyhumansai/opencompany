use super::*;

fn request(category: FeedbackCategory) -> IngestRequest {
    IngestRequest {
        category,
        title: "[bug] it broke".to_string(),
        body: "**Category:** bug\n\nit broke\n\n— filed by @acme".to_string(),
        origin: "acme".to_string(),
        external_ref: "item-1".to_string(),
    }
}

#[test]
fn maps_categories_onto_the_hub_type_pair() {
    // Something the product did wrong.
    for category in [FeedbackCategory::Bug, FeedbackCategory::WrongOutput] {
        assert_eq!(request(category).wire_type(), "bug", "{category:?}");
    }
    // Something the product does not do yet.
    for category in [
        FeedbackCategory::MissingCapability,
        FeedbackCategory::TemplateGap,
        FeedbackCategory::ApprovalFriction,
        FeedbackCategory::Docs,
    ] {
        assert_eq!(request(category).wire_type(), "feature", "{category:?}");
    }
}

#[test]
fn product_is_always_opencompany() {
    assert_eq!(PRODUCT, "opencompany");
}

#[tokio::test]
async fn mock_records_the_forwarded_request() {
    let client = MockTinyHumansClient::new();
    let outcome = client
        .ingest(&request(FeedbackCategory::Bug))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        IngestOutcome::Accepted {
            remote_id: Some("hub-1".to_string())
        }
    );
    let forwarded = client.forwarded();
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].external_ref, "item-1");
    assert_eq!(forwarded[0].origin, "acme");
}

#[tokio::test]
async fn mock_can_reject_and_fail() {
    let rejected = MockTinyHumansClient::new().with_outcome(IngestOutcome::Rejected {
        reason: "spam".to_string(),
    });
    assert_eq!(
        rejected
            .ingest(&request(FeedbackCategory::Bug))
            .await
            .unwrap(),
        IngestOutcome::Rejected {
            reason: "spam".to_string()
        }
    );

    let failing = MockTinyHumansClient::new().with_failure("connection refused");
    assert!(
        failing
            .ingest(&request(FeedbackCategory::Bug))
            .await
            .is_err()
    );
    // The attempt is still recorded, so a test can assert what we tried to send.
    assert_eq!(failing.forwarded().len(), 1);
}

/// The real client against a local stub of the hub: the route, the bearer, the
/// product header and the wire body are what the hub reads, and a 429 is a
/// reported rate limit rather than an error.
#[cfg(feature = "tinyhumans")]
#[tokio::test]
async fn the_http_client_posts_the_hub_shape_and_maps_a_rate_limit() {
    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;

    let seen: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>> = Arc::default();
    let limited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app = axum::Router::new().route(
        "/feedback/ingest",
        post({
            let seen = seen.clone();
            let limited = limited.clone();
            move |headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                seen.lock().unwrap().push((headers, body));
                if limited.load(std::sync::atomic::Ordering::SeqCst) {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        Json(serde_json::json!({ "success": false, "error": "slow down" })),
                    );
                }
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "success": true,
                        "data": { "accepted": true, "feedback": { "id": "hub-1" } }
                    })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = HttpTinyHumansClient::new(
        format!("http://{addr}/"),
        crate::ports::types::SecretValue("sk-test".to_string()),
    );
    let outcome = client
        .ingest(&request(FeedbackCategory::Bug))
        .await
        .unwrap();
    assert!(
        matches!(outcome, IngestOutcome::Accepted { remote_id: Some(ref id) } if id == "hub-1")
    );

    let (headers, body) = seen.lock().unwrap()[0].clone();
    assert_eq!(headers["authorization"], "Bearer sk-test");
    let (name, value) = crate::product::product_identity_header();
    assert_eq!(headers[name], value);
    assert_eq!(body["type"], "bug");
    assert_eq!(body["product"], PRODUCT);
    assert_eq!(body["externalRef"], "item-1");
    assert_eq!(body["origin"], "acme");

    limited.store(true, std::sync::atomic::Ordering::SeqCst);
    let outcome = client
        .ingest(&request(FeedbackCategory::Bug))
        .await
        .unwrap();
    assert!(
        matches!(outcome, IngestOutcome::RateLimited { ref reason } if reason == "slow down"),
        "{outcome:?}"
    );
}
