//! End-to-end tests for `GET`/`POST …/auth/hub`.
//!
//! Every case runs against [`MockHubIdentityExchange`], so the route's whole
//! decision tree — ask the hub, check eligibility, mint a session, and every
//! refusal — is exercised in a build that links no HTTP crate.
//!
//! The distinction these tests exist to protect is between the three ways a
//! sign-in can fail: the *token* is dead (say so, tell them to sign in again),
//! the *person* is not on this company's roster (say so, tell them to ask for
//! an invite), and the *hub* is down (say nothing about either, and do not tell
//! them to click again when clicking again cannot work). Collapsing any two of
//! those into one status is the regression worth catching.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::ports::{CompanyStore, UserRecord, UserRole, UserStatus, generate_id, now_millis};
use crate::runtime::RuntimeBuilder;
use crate::server::hub_identity::MockHubIdentityExchange;
use crate::server::router;
use crate::{AppConfig, AppState};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oc-hub-auth-")
        .tempdir()
        .expect("tempdir")
}

/// `Ada` is a manifest-bootstrapped admin, spelled with capitals so the route's
/// email normalization is exercised against a hub that returns a different case.
fn manifest() -> CompanyManifest {
    toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [users]\nadmins = [\"Ada@Example.com\"]\n",
    )
    .unwrap()
}

/// A host with `exchange` wired, bound to loopback as a local console is.
async fn state_with(
    home: &std::path::Path,
    exchange: Option<Arc<MockHubIdentityExchange>>,
) -> AppState {
    state_with_public_url(home, exchange, None).await
}

/// The same host, with `public_url` set as the platform sets it on a tenant —
/// which is the only difference between a local console and a hosted one.
async fn state_with_public_url(
    home: &std::path::Path,
    exchange: Option<Arc<MockHubIdentityExchange>>,
    public_url: Option<&str>,
) -> AppState {
    let store = crate::store::FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let mut state = AppState::new(AppConfig {
        bind: "127.0.0.1:8080".to_string(),
        api_url: "https://hub.example.com".to_string(),
        public_url: public_url.map(str::to_string),
        ..AppConfig::default()
    })
    .with_home(home.to_path_buf());
    if let Some(exchange) = exchange {
        state = state.with_hub_identity(exchange);
    }
    state.registry().insert(id, Arc::new(runtime));
    state
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get_with_cookie(uri: &str, cookie: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn session_cookie(response: &axum::response::Response) -> String {
    let set = response
        .headers()
        .get("set-cookie")
        .expect("a session response must set a cookie")
        .to_str()
        .unwrap();
    set.split(';').next().unwrap().to_string()
}

async fn sign_in(state: &AppState, token: &str) -> axum::response::Response {
    router(state.clone())
        .oneshot(post(
            "/api/v1/companies/acme/auth/hub",
            serde_json::json!({ "token": token }),
        ))
        .await
        .unwrap()
}

async fn providers(state: &AppState) -> serde_json::Value {
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/companies/acme/auth/hub")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await
}

/// `providers`, with the console's own `?…` query appended to the request.
async fn providers_from(state: &AppState, query: &str) -> serde_json::Value {
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/companies/acme/auth/hub?{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await
}

// ---------------------------------------------------------------------------
// The buttons
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_host_with_a_hub_offers_the_three_openhuman_providers() {
    let home = home();
    let state = state_with(home.path(), Some(Arc::new(MockHubIdentityExchange::new()))).await;

    let body = providers(&state).await;
    let listed = body["providers"].as_array().unwrap();

    // Same three, same order, as OpenHuman's welcome screen. Discord is a
    // registered login provider at the hub but hidden there, so it stays hidden.
    let ids: Vec<&str> = listed
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["google", "github", "twitter"]);
    assert_eq!(listed[2]["label"], "X");
}

#[tokio::test]
async fn the_start_url_points_at_the_hub_and_returns_to_this_console() {
    let home = home();
    let state = state_with(home.path(), Some(Arc::new(MockHubIdentityExchange::new()))).await;

    let body = providers(&state).await;
    let start = body["providers"][0]["startUrl"].as_str().unwrap();

    // The hub's own OAuth start, not something this crate invented.
    assert!(
        start.starts_with("https://hub.example.com/auth/google/login?redirectUri="),
        "unexpected start URL: {start}"
    );
    // The return origin is derived from the bind, so a local console is an RFC
    // 8252 loopback URI — the one shape the hub's redirectUri check accepts
    // today. A hosted origin arrives here by configuring OPENCOMPANY_PUBLIC_URL,
    // with no change to this code.
    assert!(
        start.ends_with("redirectUri=http%3A%2F%2F127.0.0.1%3A8080%2F%3Fcompany%3Dacme"),
        "the redirect must be a percent-encoded loopback URI: {start}"
    );
    // Encoded as one value: unescaped, the console's own `?company=` would be
    // read by the hub as one of its own query parameters.
    assert!(!start.contains("redirectUri=http://"));
}

#[tokio::test]
async fn a_setup_handoff_asks_the_hub_to_return_to_the_setup_destination() {
    // A dead setup link that falls back to the form and continues through an
    // ecosystem button has to arrive on the same roster the link promised. The
    // destination rides in the return URI as a query parameter (`&from=setup`):
    // the console's own fragment cannot cross the OAuth round trip — the hub
    // appends `token=…&key=auth` to whatever it was given, and anything after a
    // `#` there would swallow them.
    let home = home();
    let state = state_with(home.path(), Some(Arc::new(MockHubIdentityExchange::new()))).await;

    let body = providers_from(&state, "from=setup").await;
    let start = body["providers"][0]["startUrl"].as_str().unwrap();

    assert!(
        start.ends_with(
            "redirectUri=http%3A%2F%2F127.0.0.1%3A8080%2F%3Fcompany%3Dacme%26from%3Dsetup"
        ),
        "the redirect must carry the setup destination: {start}"
    );
}

#[tokio::test]
async fn a_malformed_from_is_dropped_not_obeyed() {
    // `from` rides the return URI through an external service and back into the
    // address bar, so a value that could escape it — a fragment that would
    // capture the hub's token, say — must be ignored, and the buttons must
    // still come back.
    let home = home();
    let state = state_with(home.path(), Some(Arc::new(MockHubIdentityExchange::new()))).await;

    let body = providers_from(&state, "from=https%3A%2F%2Fevil.example%0A").await;
    let start = body["providers"][0]["startUrl"].as_str().unwrap();

    assert!(
        start.ends_with("redirectUri=http%3A%2F%2F127.0.0.1%3A8080%2F%3Fcompany%3Dacme"),
        "the malformed destination must be dropped: {start}"
    );
}

#[tokio::test]
async fn a_hosted_console_offers_no_providers_while_the_hub_refuses_its_origin() {
    let home = home();
    let state = state_with_public_url(
        home.path(),
        Some(Arc::new(MockHubIdentityExchange::new())),
        Some("https://smoke1.example.com"),
    )
    .await;

    let body = providers(&state).await;

    // Issue #512: the hub's redirect gate accepts loopback origins only, so
    // every one of these buttons answers 400 before Google is ever reached.
    // Empty is the same "no honest button to offer" the no-exchange case takes,
    // and it leaves the magic-link form — which does work hosted — standing
    // alone rather than under three that cannot.
    //
    // Fails when `tinyhumansai/backend#1243` lands and the guard comes out,
    // which is the point: this test is the reminder to delete it.
    assert_eq!(body["providers"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_self_hosted_host_offers_no_providers_at_all() {
    let home = home();
    let state = state_with(home.path(), None).await;

    let body = providers(&state).await;

    // Empty rather than a 404, so the console has one code path — and empty
    // rather than three dead buttons, because a host with no exchange could not
    // check a token that came back.
    assert_eq!(body["providers"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Signing in
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_token_for_an_eligible_address_mints_a_session() {
    let home = home();
    let exchange =
        Arc::new(MockHubIdentityExchange::new().with_token("platform-jwt", "Ada@Example.com"));
    let state = state_with(home.path(), Some(exchange)).await;

    let response = sign_in(&state, "platform-jwt").await;

    assert_eq!(response.status(), StatusCode::OK);
    let cookie = session_cookie(&response);
    assert!(
        cookie.starts_with("oc_session_acme="),
        "the cookie must be named for this company: {cookie}"
    );
    let body = body_json(response).await;
    // The hub's casing is normalized on the way in, so this address cannot end
    // up holding a second account alongside a magic-link sign-in.
    assert_eq!(body["email"], "ada@example.com");
    assert_eq!(body["role"], "admin");
    assert_eq!(body["company"], "acme");

    // And the session actually works.
    let me = router(state.clone())
        .oneshot(get_with_cookie("/api/v1/companies/acme/auth/me", &cookie))
        .await
        .unwrap();
    assert_eq!(me.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_same_token_may_be_presented_twice() {
    let home = home();
    let exchange =
        Arc::new(MockHubIdentityExchange::new().with_token("platform-jwt", "Ada@Example.com"));
    let state = state_with(home.path(), Some(exchange)).await;

    assert_eq!(
        sign_in(&state, "platform-jwt").await.status(),
        StatusCode::OK
    );

    // Unlike a single-use handoff code, a platform token is a bearer credential
    // with a lifetime. A reload that re-posts it must not look like an attack.
    assert_eq!(
        sign_in(&state, "platform-jwt").await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_address_this_company_does_not_know_is_refused() {
    let home = home();
    let exchange =
        Arc::new(MockHubIdentityExchange::new().with_token("platform-jwt", "stranger@example.com"));
    let state = state_with(home.path(), Some(exchange)).await;

    let response = sign_in(&state, "platform-jwt").await;

    // Signed in to the ecosystem, and still refused here: the hub says who they
    // are, this company's roster says whether they may in.
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(body_json(response).await["code"], "not_a_member");
}

#[tokio::test]
async fn an_address_removed_from_the_roster_is_refused() {
    let home = home();
    let exchange =
        Arc::new(MockHubIdentityExchange::new().with_token("platform-jwt", "bob@example.com"));
    let state = state_with(home.path(), Some(exchange)).await;

    // Bob was a member and is suspended. He is still a perfectly good ecosystem
    // identity, which is exactly why the hub cannot be the one to decide this.
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let now = now_millis();
    runtime
        .users()
        .upsert_user(
            &id,
            &UserRecord {
                id: generate_id(),
                email: "bob@example.com".to_string(),
                display_name: None,
                avatar: None,
                role: UserRole::Member,
                status: UserStatus::Suspended,
                password_hash: None,
                must_change_password: false,
                created_at_millis: now,
                last_seen_at_millis: None,
                updated_at_millis: now,
            },
        )
        .await
        .unwrap();

    let response = sign_in(&state, "platform-jwt").await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(body_json(response).await["code"], "not_a_member");
}

#[tokio::test]
async fn a_token_the_hub_rejects_is_a_client_error_not_a_bad_gateway() {
    let home = home();
    let exchange = Arc::new(MockHubIdentityExchange::new());
    let state = state_with(home.path(), Some(exchange)).await;

    let response = sign_in(&state, "forged-or-expired").await;

    // The hub's 401 must not surface as the 502 the error type otherwise maps
    // to: a dead token is the caller's problem to fix by signing in again, and
    // telling them the ecosystem is down is the wrong instruction.
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(body_json(response).await["code"], "hub_rejected");
}

#[tokio::test]
async fn an_unreachable_hub_is_not_reported_as_a_dead_token() {
    let home = home();
    let exchange = Arc::new(MockHubIdentityExchange::unreachable());
    let state = state_with(home.path(), Some(exchange)).await;

    let response = sign_in(&state, "platform-jwt").await;

    // An outage keeps its 5xx. Reporting it as `hub_rejected` would send
    // someone round the OAuth loop forever against a hub that cannot answer.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.headers().get("set-cookie").is_none());
    assert_ne!(body_json(response).await["code"], "hub_rejected");
}

#[tokio::test]
async fn a_host_with_no_hub_refuses_every_token() {
    let home = home();
    let state = state_with(home.path(), None).await;

    let response = sign_in(&state, "platform-jwt").await;

    // Without an exchange there is no way to check the token, and accepting it
    // on trust would make an unverifiable JWT a bearer credential here.
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(body_json(response).await["code"], "hub_unavailable");
}
