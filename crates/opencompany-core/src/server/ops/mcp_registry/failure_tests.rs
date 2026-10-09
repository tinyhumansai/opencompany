use super::*;

use axum::body::to_bytes;

async fn body_of(response: Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn classifies_timeouts_apart_from_other_failures() {
    assert_eq!(
        RegistryFailure::classify("operation timed out"),
        RegistryFailure::Timeout
    );
    assert_eq!(
        RegistryFailure::classify("request Timeout after 15s"),
        RegistryFailure::Timeout
    );
    assert_eq!(
        RegistryFailure::classify(
            "mcp transport failure for `https://registry.modelcontextprotocol.io`: error sending \
             request"
        ),
        RegistryFailure::Unavailable
    );
}

#[test]
fn maps_each_failure_to_its_status_and_code() {
    assert_eq!(
        RegistryFailure::Timeout.status(),
        StatusCode::GATEWAY_TIMEOUT
    );
    assert_eq!(RegistryFailure::Timeout.code(), "registry_timeout");
    assert_eq!(
        RegistryFailure::Unavailable.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(RegistryFailure::Unavailable.code(), "registry_unavailable");
}

#[tokio::test]
async fn the_response_body_carries_no_upstream_detail() {
    for failure in [RegistryFailure::Timeout, RegistryFailure::Unavailable] {
        for read in [RegistryRead::Search, RegistryRead::Lookup] {
            let response = failure.response(read);
            assert_eq!(response.status(), failure.status());
            let body = body_of(response).await;
            assert_eq!(body["code"], failure.code());
            let text = body["error"].as_str().unwrap();
            assert!(!text.contains("http"), "{text}");
            assert!(!text.contains("harness"), "{text}");
            assert!(!text.contains("transport"), "{text}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_read_past_its_budget_is_a_timeout() {
    let slow = async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok::<u32, String>(1)
    };
    let outcome = bounded(RegistryRead::Search, Some(SEARCH_BUDGET), slow).await;
    assert_eq!(outcome, Err(RegistryFailure::Timeout));
}

#[tokio::test(start_paused = true)]
async fn a_read_inside_its_budget_passes_through() {
    let quick = async { Ok::<u32, String>(7) };
    assert_eq!(
        bounded(RegistryRead::Search, Some(SEARCH_BUDGET), quick).await,
        Ok(7)
    );
}

#[tokio::test]
async fn an_upstream_error_is_classified_not_forwarded() {
    let failing = async { Err::<u32, String>("error sending request".to_string()) };
    assert_eq!(
        bounded(RegistryRead::Lookup, None, failing).await,
        Err(RegistryFailure::Unavailable)
    );
}
