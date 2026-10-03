use super::*;

/// The exact string the hub's `GET /auth/key` receives, pinned whole.
///
/// Every value percent-encoded the same way, `scopes` included: the hub
/// reads it off the query string like any other parameter, and an encoder
/// applied to three of four values is the one that surprises somebody.
#[test]
fn a_key_grant_query_asks_for_its_scopes_beside_the_challenge() {
    assert_eq!(
        key_grant_query("https://acme.example.com/?company=acme", "chal-abc", "Acme"),
        "callback_url=https%3A%2F%2Facme.example.com%2F%3Fcompany%3Dacme\
         &code_challenge=chal-abc\
         &code_challenge_method=S256\
         &name=Acme\
         &scopes=connections"
    );
}

/// Without this name on the ask, a key minted to anything but a provisioned
/// tenant cannot drive `/agent-integrations/composio/*` at all.
#[test]
fn the_scopes_asked_for_name_connections() {
    assert!(
        KEY_GRANT_SCOPES.contains(&"connections"),
        "managed Composio 403s without it: {KEY_GRANT_SCOPES:?}"
    );
}

/// Both readers get the builder's output verbatim.
///
/// The property the split exists for: neither path may append a parameter
/// of its own, because a grant whose reach depends on which page the
/// browser went through is one nobody can reason about from the consent
/// screen.
#[test]
fn both_readers_carry_the_same_built_query() {
    let query = key_grant_query("http://127.0.0.1:5173/?key=link", "chal-abc", "Acme");

    let api = key_grant_url(
        "https://api.example.com",
        "http://127.0.0.1:5173/?key=link",
        "chal-abc",
        "Acme",
    );
    let site = crate::server::hub_account::connect_url("https://example.com", &query);

    assert_eq!(api, format!("https://api.example.com/auth/key?{query}"));
    assert_eq!(site, format!("https://example.com/connect?{query}"));
}

/// The real exchange against a local stub of the hub: redemption carries no
/// bearer and the PKCE pair in the body; the summary carries the redeemed key
/// as its bearer; both carry the product header; a failure names the status.
#[cfg(feature = "tinyhumans")]
#[tokio::test]
async fn the_http_exchange_redeems_then_reads_the_summary() {
    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};

    let seen: Arc<Mutex<Vec<(String, HeaderMap, serde_json::Value)>>> = Arc::default();
    let app = axum::Router::new()
        .route(
            "/auth/keys",
            post({
                let seen = seen.clone();
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                    seen.lock().unwrap().push(("keys".into(), headers, body));
                    Json(serde_json::json!({ "success": true, "data": { "key": "sk-minted" } }))
                }
            }),
        )
        .route(
            "/payments/summary",
            get({
                let seen = seen.clone();
                move |headers: HeaderMap| async move {
                    seen.lock()
                        .unwrap()
                        .push(("summary".into(), headers, serde_json::Value::Null));
                    Json(serde_json::json!({
                        "success": true,
                        "data": {
                            "credits": { "totalUsd": 12.5 },
                            "plan": { "hasActiveSubscription": false },
                            "links": {}
                        }
                    }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let exchange = HttpHubIdentityExchange::new(format!("http://{addr}/"));
    let key = exchange
        .redeem_key_grant("code-1", "verifier-1")
        .await
        .unwrap();
    assert_eq!(key, "sk-minted");
    let summary = exchange.billing_summary(&key).await.unwrap();
    assert_eq!(summary.balance_usd, 12.5);
    assert_eq!(summary.plan, "free");

    let seen = seen.lock().unwrap();
    let (name, value) = crate::product::product_identity_header();
    let (_, redeem_headers, redeem_body) = &seen[0];
    assert!(
        redeem_headers.get("authorization").is_none(),
        "redemption is unauthenticated"
    );
    assert_eq!(redeem_headers[name], value);
    assert_eq!(redeem_body["code"], "code-1");
    assert_eq!(redeem_body["code_verifier"], "verifier-1");
    let (_, summary_headers, _) = &seen[1];
    assert_eq!(summary_headers["authorization"], "Bearer sk-minted");
    assert_eq!(summary_headers[name], value);
}
