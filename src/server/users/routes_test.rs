//! End-to-end tests for the login and admin routes.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::ports::CompanyStore;
use crate::ports::types::{CompanyId, CompanyRecord, SecretValue};
use crate::runtime::RuntimeBuilder;
use crate::server::ops::ConnectionsRuntime;
use crate::server::ops::mailer::{MailCredentials, RecordingMailSender};
use crate::server::ops::smtp::{SmtpCredentials, SmtpSecurity};
use crate::server::router;
use crate::{AppConfig, AppState};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oc-routes-")
        .tempdir()
        .expect("tempdir")
}

/// A manifest whose `[users] admins` bootstraps `ada` — deliberately spelled
/// with capitals, so normalization is exercised end to end.
fn manifest() -> CompanyManifest {
    toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [users]\nadmins = [\"Ada@Example.com\"]\n",
    )
    .unwrap()
}

/// A manifest with **no** `[users] admins` — the shape a company the platform
/// provisions boots with, and the reason issue #321 exists: nobody is eligible
/// and there is no operator token to send the first invite with.
fn manifest_without_admins() -> CompanyManifest {
    toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
}

async fn state_with(home: &std::path::Path, connections: ConnectionsRuntime) -> AppState {
    state_bound_to(home, &AppConfig::default().bind, connections).await
}

/// State on an explicit bind, for the tests that turn on whether the host looks
/// reachable from anywhere but this machine.
async fn state_bound_to(
    home: &std::path::Path,
    bind: &str,
    connections: ConnectionsRuntime,
) -> AppState {
    state_from(
        home,
        manifest(),
        AppConfig {
            bind: bind.to_string(),
            ..AppConfig::default()
        },
        connections,
    )
    .await
}

/// State over an explicit manifest and config — the seam the bootstrap-admin
/// tests need, since they turn on both.
async fn state_from(
    home: &std::path::Path,
    manifest: CompanyManifest,
    config: AppConfig,
    connections: ConnectionsRuntime,
) -> AppState {
    let store = crate::store::FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest.clone(),
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
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(config)
        .with_home(home.to_path_buf())
        .with_connections(connections);
    state.registry().insert(id, Arc::new(runtime));
    state
}

/// A recording mail sender wired as a transport, so links are "delivered".
fn mail_connections() -> (ConnectionsRuntime, RecordingMailSender) {
    let sender = RecordingMailSender::new();
    let connections = ConnectionsRuntime::new()
        .with_mail(Arc::new(sender.clone()))
        .with_mail_credentials(MailCredentials::Smtp(SmtpCredentials {
            host: "smtp.test".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "u".into(),
            password: SecretValue("p".into()),
            from_name: "Acme".into(),
            from_email: "noreply@acme.test".into(),
        }));
    (connections, sender)
}

/// State with a recording mail sender wired, so links are "delivered".
async fn state_with_mail(home: &std::path::Path) -> (AppState, RecordingMailSender) {
    let (connections, sender) = mail_connections();
    (state_with(home, connections).await, sender)
}

/// State with mail wired over an explicit manifest and `OPENCOMPANY_ADMIN_EMAIL`
/// value — `None` being the pre-#321 deployment.
async fn state_with_admin_email(
    home: &std::path::Path,
    manifest: CompanyManifest,
    admin_email: Option<&str>,
) -> (AppState, RecordingMailSender) {
    let (connections, sender) = mail_connections();
    let config = AppConfig {
        admin_email: admin_email.map(str::to_string),
        ..AppConfig::default()
    };
    (
        state_from(home, manifest, config, connections).await,
        sender,
    )
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn post_with_cookie(uri: &str, body: serde_json::Value, cookie: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("cookie", cookie)
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

/// Extracts the session cookie's `name=value` pair from a `Set-Cookie` header.
fn session_cookie(response: &axum::response::Response) -> String {
    let set = response
        .headers()
        .get("set-cookie")
        .expect("a session response must set a cookie")
        .to_str()
        .unwrap();
    set.split(';').next().unwrap().to_string()
}

/// Requests a link for `email` and returns the dev-echoed code, if any.
async fn request_dev_code(state: &AppState, email: &str) -> Option<String> {
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/request",
            serde_json::json!({ "email": email }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["sent"], true, "the response must never vary");
    json["dev_code"].as_str().map(str::to_string)
}

/// The login code from the most recent mail the recorder captured.
///
/// With a transport wired the code is deliberately *not* echoed in the
/// response, so tests read it the way a user would: out of the mail.
fn code_from_last_mail(sender: &RecordingMailSender) -> String {
    let sent = sender.sent();
    let body = &sent.last().expect("no mail was sent").1.body;
    body.split("code=")
        .nth(1)
        .expect("the mail must contain a login link")
        .split_whitespace()
        .next()
        .expect("the link must carry a code")
        .to_string()
}

/// Requests a link for `email` and returns the code, read out of the mail.
async fn request_code(state: &AppState, sender: &RecordingMailSender, email: &str) -> String {
    let echoed = request_dev_code(state, email).await;
    assert_eq!(
        echoed, None,
        "a host with mail wired must never echo the code"
    );
    code_from_last_mail(sender)
}

/// Logs `email` in via the magic link, returning the session cookie.
async fn login_via_link(state: &AppState, sender: &RecordingMailSender, email: &str) -> String {
    let code = request_code(state, sender, email).await;
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": code }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    session_cookie(&response)
}

// ---------------------------------------------------------------------------
// The header carrier — a hub console that cannot receive a cookie
// ---------------------------------------------------------------------------

/// A login request that asks for a session the client will carry itself.
fn post_wanting_header_carrier(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header(super::cookie::SESSION_CARRIER_HEADER, "header")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get_with_session_header(uri: &str, session: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(super::cookie::SESSION_HEADER, session)
        .body(Body::empty())
        .unwrap()
}

/// Signs `email` in asking for the header carrier, returning the whole body.
async fn login_wanting_header_carrier(
    state: &AppState,
    sender: &RecordingMailSender,
    email: &str,
) -> (axum::http::HeaderMap, serde_json::Value) {
    let code = request_code(state, sender, email).await;
    let app = router(state.clone());
    let response = app
        .oneshot(post_wanting_header_carrier(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": code }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    (headers, body_json(response).await)
}

#[tokio::test]
async fn the_header_carrier_returns_a_session_that_authenticates() {
    // The whole point of the carrier: a console on another origin gets a
    // credential it can actually present, because a cookie would never be sent.
    let home = home();
    let (state, sender) = state_with_mail(home.path()).await;
    let (_, json) = login_wanting_header_carrier(&state, &sender, "ada@example.com").await;

    let session = json["session"]
        .as_str()
        .expect("the header carrier must return a session")
        .to_string();
    assert!(
        session.starts_with("acme."),
        "the value must name its company so a client need not know how the \
         addressed company was resolved: {session}"
    );

    let app = router(state.clone());
    let response = app
        .oneshot(get_with_session_header(
            "/api/v1/companies/acme/auth/me",
            &session,
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the returned session must authenticate the next request"
    );
    assert_eq!(body_json(response).await["email"], "ada@example.com");
}

#[tokio::test]
async fn the_header_carrier_sets_no_cookie() {
    // One session, one carrier. Setting both would leave the cookie half as a
    // third-party cookie some browsers keep and others drop, so whether logging
    // out actually ended the session would vary by browser.
    let home = home();
    let (state, sender) = state_with_mail(home.path()).await;
    let (headers, _) = login_wanting_header_carrier(&state, &sender, "ada@example.com").await;
    assert!(
        headers.get("set-cookie").is_none(),
        "a client that asked to carry the session must not also be given a cookie"
    );
}

#[tokio::test]
async fn a_login_that_asks_for_nothing_is_unchanged() {
    // The carrier is opt-in, and every existing console is the opt-out case:
    // it must still get its HttpOnly cookie and must never see a token in the
    // body, which is precisely what it has no way to store safely.
    let home = home();
    let (state, sender) = state_with_mail(home.path()).await;
    let code = request_code(&state, &sender, "ada@example.com").await;
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": code }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let set = response
        .headers()
        .get("set-cookie")
        .expect("the default carrier is still the cookie")
        .to_str()
        .unwrap()
        .to_string();
    assert!(set.contains("HttpOnly"), "{set}");
    let json = body_json(response).await;
    assert!(
        json.get("session").is_none(),
        "a cookie client must never be handed the raw token: {json}"
    );
    // And the body it always returned is still there, unflattened by the change.
    assert_eq!(json["email"], "ada@example.com");
}

#[tokio::test]
async fn a_setup_link_carries_the_requested_landing_fragment() {
    // Setup's hand-off asks the mailed link to land on the roster, so a
    // production operator who finishes setup and follows the email reaches the
    // company the wizard just built rather than the Overview graph. The
    // fragment is appended after the code so the magic-link landing strips the
    // credential and keeps the destination.
    let home = home();
    let (state, sender) = state_with_mail(home.path()).await;
    let app = router(state.clone());
    app.oneshot(post(
        "/api/v1/companies/acme/auth/request",
        serde_json::json!({
            "email": "ada@example.com",
            "redirect": "#/company?from=setup",
        }),
    ))
    .await
    .unwrap();
    let sent = sender.sent();
    let body = &sent.last().expect("no mail was sent").1.body;
    assert!(
        body.contains("/login?company=acme&code=") && body.contains("#/company?from=setup"),
        "the mailed link must carry the setup destination: {body}"
    );
}

#[tokio::test]
async fn a_malformed_redirect_is_dropped_not_obeyed() {
    // The fragment is mailed, so a value that could break the link out of the
    // body — or name something that is not a console route — must be ignored,
    // and it must not stop the sign-in mail from going out at all.
    let home = home();
    let (state, sender) = state_with_mail(home.path()).await;
    let app = router(state.clone());
    app.oneshot(post(
        "/api/v1/companies/acme/auth/request",
        serde_json::json!({
            "email": "ada@example.com",
            "redirect": "https://evil.example\n#/company",
        }),
    ))
    .await
    .unwrap();
    let sent = sender.sent();
    let body = &sent.last().expect("no mail was sent").1.body;
    assert!(!body.contains("evil.example"), "{body}");
    assert!(
        body.contains("/login?company=acme&code="),
        "the sign-in link itself must survive: {body}"
    );
}

#[tokio::test]
async fn a_header_carried_session_can_be_logged_out() {
    // Revocation has to reach the session however it was carried, or a hub
    // console's "sign out" would clear its own storage and leave a live token
    // on the server for the rest of its TTL.
    let home = home();
    let (state, sender) = state_with_mail(home.path()).await;
    let (_, json) = login_wanting_header_carrier(&state, &sender, "ada@example.com").await;
    let session = json["session"].as_str().unwrap().to_string();

    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/companies/acme/auth/logout")
                .header(super::cookie::SESSION_HEADER, &session)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let app = router(state.clone());
    let after = app
        .oneshot(get_with_session_header(
            "/api/v1/companies/acme/auth/me",
            &session,
        ))
        .await
        .unwrap();
    assert_eq!(
        after.status(),
        StatusCode::UNAUTHORIZED,
        "the token must be dead server-side, not merely dropped by the client"
    );
}

/// Looks a user's id up through the admin roster.
async fn user_id(state: &AppState, admin: &str, email: &str) -> String {
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie("/api/v1/companies/acme/users", admin))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response)
        .await
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == email)
        .unwrap_or_else(|| panic!("no user {email}"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// The generic-failure rule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_request_answers_identically_for_everyone() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;

    // An eligible admin, a stranger, and malformed input must be
    // indistinguishable from outside. Anything else is a membership oracle.
    for email in ["ada@example.com", "nobody@example.com", "not-an-email", ""] {
        let app = router(state.clone());
        let response = app
            .oneshot(post(
                "/api/v1/companies/acme/auth/request",
                serde_json::json!({ "email": email }),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "status varied for {email:?}"
        );
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "sent": true }),
            "the body varied for {email:?} — that is an enumeration oracle"
        );
    }

    // Only the eligible address actually got mail.
    let sent = sender.sent();
    assert_eq!(sent.len(), 1, "mail went to someone it shouldn't have");
    assert_eq!(sent[0].1.to, "ada@example.com");
}

#[tokio::test]
async fn every_verify_failure_is_the_same_401() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, _) = state_with_mail(&home).await;

    let mut seen = Vec::new();
    for code in [
        "",
        "not-a-real-code",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        let app = router(state.clone());
        let response = app
            .oneshot(post(
                "/api/v1/companies/acme/auth/verify",
                serde_json::json!({ "code": code }),
            ))
            .await
            .unwrap();
        let status = response.status();
        seen.push((status, body_json(response).await));
    }
    let first = seen[0].clone();
    for entry in &seen {
        assert_eq!(entry.0, StatusCode::UNAUTHORIZED);
        assert_eq!(*entry, first, "verify failures must be byte-identical");
        assert_eq!(entry.1["code"], "invalid_login");
    }
}

#[tokio::test]
async fn every_password_login_failure_is_the_same_401() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    // Give ada an account and a password first.
    let cookie = login_via_link(&state, &sender, "ada@example.com").await;
    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/auth/password",
            serde_json::json!({ "password": "correct horse battery" }),
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut seen = Vec::new();
    for (email, pw) in [
        // Wrong password for a real account.
        ("ada@example.com", "wrong password here"),
        // Unknown address entirely.
        ("nobody@example.com", "correct horse battery"),
        // Empty address.
        ("", "correct horse battery"),
    ] {
        let app = router(state.clone());
        let response = app
            .oneshot(post(
                "/api/v1/companies/acme/auth/login",
                serde_json::json!({ "email": email, "password": pw }),
            ))
            .await
            .unwrap();
        let status = response.status();
        seen.push((status, body_json(response).await));
    }
    let first = seen[0].clone();
    for entry in &seen {
        assert_eq!(entry.0, StatusCode::UNAUTHORIZED);
        assert_eq!(*entry, first, "login failures must be byte-identical");
    }
}

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_manifest_admin_can_log_in_and_is_an_admin() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    // The manifest spells it "Ada@Example.com"; normalization must match.
    let cookie = login_via_link(&state, &sender, "ada@example.com").await;

    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie("/api/v1/companies/acme/auth/me", &cookie))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let me = body_json(response).await;
    assert_eq!(me["email"], "ada@example.com");
    assert_eq!(me["role"], "admin", "the manifest bootstraps an admin");
    assert_eq!(me["company"], "acme");
    assert_eq!(me["hasPassword"], false);
}

#[tokio::test]
async fn a_link_is_single_use() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let code = request_code(&state, &sender, "ada@example.com").await;

    let app = router(state.clone());
    let first = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": code }),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    // The same link again buys nothing — a forwarded mail is not a credential.
    let app = router(state.clone());
    let second = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": code }),
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_second_link_within_the_throttle_window_is_not_sent() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let first_code = request_code(&state, &sender, "ada@example.com").await;

    // Immediately ask again: same acknowledgement, no second mail. Otherwise
    // this route is a mail cannon pointed at an invited mailbox.
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/request",
            serde_json::json!({ "email": "ada@example.com" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        serde_json::json!({ "sent": true }),
        "a throttled request must answer exactly like a sent one"
    );
    assert_eq!(sender.sent().len(), 1, "a second mail went out");

    // The live link still works: throttling must not let anyone invalidate
    // someone else's link on demand.
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": first_code }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn once_the_window_passes_a_new_link_invalidates_the_previous_one() {
    use crate::ports::LoginCodeRecord;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let now = crate::ports::now_millis();

    // Seed a code minted well outside the throttle window, standing in for
    // "they asked a few minutes ago". Clock control is not available here, so
    // the elapsed time is expressed in the record rather than by waiting.
    let old_plaintext =
        crate::server::users::token::mint_login_code(&crate::server::users::token::OsTokens);
    runtime
        .login_codes()
        .create(
            &id,
            &LoginCodeRecord {
                id: "old".into(),
                code_hash: crate::server::users::token::sha256_hex(&old_plaintext),
                email: "ada@example.com".into(),
                created_at_millis: now - 5 * 60 * 1000,
                expires_at_millis: now + 5 * 60 * 1000,
                consumed_at_millis: None,
            },
        )
        .await
        .unwrap();

    // Past the window, so a fresh link is minted and mailed.
    let new_code = request_code(&state, &sender, "ada@example.com").await;
    assert_ne!(new_code, old_plaintext);

    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": old_plaintext }),
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "an abandoned link must not work once a newer one exists"
    );

    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": new_code }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn setting_a_password_enables_password_login() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let cookie = login_via_link(&state, &sender, "ada@example.com").await;

    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/auth/password",
            serde_json::json!({ "password": "correct horse battery" }),
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["hasPassword"], true);

    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/login",
            serde_json::json!({
                "email": "ada@example.com",
                "password": "correct horse battery",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!session_cookie(&response).is_empty());
}

#[tokio::test]
async fn a_weak_password_is_refused() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let cookie = login_via_link(&state, &sender, "ada@example.com").await;

    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/auth/password",
            serde_json::json!({ "password": "short" }),
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn setting_a_password_requires_a_session() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, _) = state_with_mail(&home).await;
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/password",
            serde_json::json!({ "password": "correct horse battery" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_session_cookie_is_defended() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let code = request_code(&state, &sender, "ada@example.com").await;
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": code }),
        ))
        .await
        .unwrap();
    let set = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(set.starts_with("oc_session_acme="), "{set}");
    assert!(set.contains("HttpOnly"), "{set}");
    assert!(set.contains("SameSite=Lax"), "{set}");
    assert!(set.contains("Path=/"), "{set}");
    // Default config has no https public_url, so this is loopback dev.
    assert!(
        !set.contains("Secure"),
        "http dev must not set Secure or the cookie is dropped: {set}"
    );
}

#[tokio::test]
async fn a_https_deployment_marks_the_cookie_secure() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let sender = RecordingMailSender::new();
    let store = crate::store::FsCompanyStore::new(home.clone());
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
    let runtime = RuntimeBuilder::new(home.clone(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    // The hosted shape: the manager injects an https public URL.
    let state = AppState::new(AppConfig {
        public_url: Some("https://acme.example".into()),
        ..AppConfig::default()
    })
    .with_home(home.clone())
    .with_connections(
        ConnectionsRuntime::new()
            .with_mail(Arc::new(sender.clone()))
            .with_mail_credentials(MailCredentials::Smtp(SmtpCredentials {
                host: "smtp.test".into(),
                port: 587,
                security: SmtpSecurity::Starttls,
                username: "u".into(),
                password: SecretValue("p".into()),
                from_name: "Acme".into(),
                from_email: "noreply@acme.test".into(),
            })),
    );
    state.registry().insert(id, Arc::new(runtime));

    let code = request_code(&state, &sender, "ada@example.com").await;
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": code }),
        ))
        .await
        .unwrap();
    let set = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        set.contains("Secure"),
        "an https host must set Secure: {set}"
    );
}

#[tokio::test]
async fn logout_revokes_the_session_not_just_the_cookie() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let cookie = login_via_link(&state, &sender, "ada@example.com").await;

    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/auth/logout",
            serde_json::json!({}),
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );

    // The token must be dead server-side: clearing a cookie does nothing to a
    // copy of the token held anywhere else.
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie("/api/v1/companies/acme/auth/me", &cookie))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Admin routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_an_admin_can_invite() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let admin = login_via_link(&state, &sender, "ada@example.com").await;

    // Anonymous.
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/users/invites",
            serde_json::json!({ "email": "bob@example.com" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // The admin can, and the address is normalized on the way in.
    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/users/invites",
            serde_json::json!({ "email": "Bob@Example.com" }),
            &admin,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["email"], "bob@example.com");

    // Bob logs in as a member, and cannot invite.
    let bob = login_via_link(&state, &sender, "bob@example.com").await;
    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/users/invites",
            serde_json::json!({ "email": "eve@example.com" }),
            &bob,
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "a member must not be able to invite"
    );
}

#[tokio::test]
async fn an_uninvited_address_cannot_log_in() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, _) = state_with_mail(&home).await;
    // Not invited, not in the manifest: no code is minted at all.
    assert_eq!(
        request_dev_code(&state, "eve@example.com").await,
        None,
        "an uninvited address must not receive a code"
    );
}

#[tokio::test]
async fn suspending_a_user_kills_their_session_at_once() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let admin = login_via_link(&state, &sender, "ada@example.com").await;

    let app = router(state.clone());
    app.oneshot(post_with_cookie(
        "/api/v1/companies/acme/users/invites",
        serde_json::json!({ "email": "bob@example.com" }),
        &admin,
    ))
    .await
    .unwrap();
    let bob_cookie = login_via_link(&state, &sender, "bob@example.com").await;
    let bob_id = user_id(&state, &admin, "bob@example.com").await;

    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/companies/acme/users/{bob_id}"))
                .header("content-type", "application/json")
                .header("cookie", &admin)
                .body(Body::from(r#"{"status":"suspended"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // His live cookie stops working immediately, not at expiry.
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie(
            "/api/v1/companies/acme/auth/me",
            &bob_cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // And he cannot get a new link either.
    assert_eq!(request_dev_code(&state, "bob@example.com").await, None);
}

/// The same bound the self-service route enforces, on the admin route too: an
/// over-long name written for somebody else would render on every surface that
/// shows them and ride in every roster payload.
#[tokio::test]
async fn an_admin_cannot_set_an_over_long_display_name() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let admin = login_via_link(&state, &sender, "ada@example.com").await;

    let app = router(state.clone());
    app.oneshot(post_with_cookie(
        "/api/v1/companies/acme/users/invites",
        serde_json::json!({ "email": "bob@example.com" }),
        &admin,
    ))
    .await
    .unwrap();
    login_via_link(&state, &sender, "bob@example.com").await;
    let bob_id = user_id(&state, &admin, "bob@example.com").await;

    let long = "A".repeat(crate::server::users::MAX_DISPLAY_NAME_CHARS + 1);
    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/companies/acme/users/{bob_id}"))
                .header("content-type", "application/json")
                .header("cookie", &admin)
                .body(Body::from(
                    serde_json::json!({ "display_name": long }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_last_admin_cannot_be_demoted() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let admin = login_via_link(&state, &sender, "ada@example.com").await;
    let ada_id = user_id(&state, &admin, "ada@example.com").await;

    // Demoting the only admin would lock the company out of its own directory,
    // and there is no operator token to recover with.
    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/companies/acme/users/{ada_id}"))
                .header("content-type", "application/json")
                .header("cookie", &admin)
                .body(Body::from(r#"{"role":"member"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_admin_reset_forces_a_change_and_kills_sessions() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let admin = login_via_link(&state, &sender, "ada@example.com").await;

    let app = router(state.clone());
    app.oneshot(post_with_cookie(
        "/api/v1/companies/acme/users/invites",
        serde_json::json!({ "email": "bob@example.com" }),
        &admin,
    ))
    .await
    .unwrap();
    let bob_cookie = login_via_link(&state, &sender, "bob@example.com").await;
    let bob_id = user_id(&state, &admin, "bob@example.com").await;

    // The admin issues a temporary password.
    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            &format!("/api/v1/companies/acme/users/{bob_id}/password"),
            serde_json::json!({ "password": "temporary pass phrase" }),
            &admin,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let summary = body_json(response).await;
    assert_eq!(summary["mustChangePassword"], true);
    assert_eq!(summary["hasPassword"], true);
    assert!(
        summary.get("passwordHash").is_none(),
        "a response must never carry the hash"
    );

    // Bob's old session is gone: a reset is what you do when you believe the
    // account is compromised.
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie(
            "/api/v1/companies/acme/auth/me",
            &bob_cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // He logs in with the temporary password and is told to replace it.
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/login",
            serde_json::json!({
                "email": "bob@example.com",
                "password": "temporary pass phrase",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let new_cookie = session_cookie(&response);
    assert_eq!(body_json(response).await["mustChangePassword"], true);

    // Setting his own password clears the flag.
    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/auth/password",
            serde_json::json!({ "password": "his own long secret" }),
            &new_cookie,
        ))
        .await
        .unwrap();
    assert_eq!(body_json(response).await["mustChangePassword"], false);
}

#[tokio::test]
async fn a_temporary_password_is_a_boundary_not_a_suggestion() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let admin = login_via_link(&state, &sender, "ada@example.com").await;

    let app = router(state.clone());
    app.oneshot(post_with_cookie(
        "/api/v1/companies/acme/users/invites",
        serde_json::json!({ "email": "bob@example.com" }),
        &admin,
    ))
    .await
    .unwrap();
    login_via_link(&state, &sender, "bob@example.com").await;
    let bob_id = user_id(&state, &admin, "bob@example.com").await;

    let app = router(state.clone());
    app.oneshot(post_with_cookie(
        &format!("/api/v1/companies/acme/users/{bob_id}/password"),
        serde_json::json!({ "password": "temporary pass phrase" }),
        &admin,
    ))
    .await
    .unwrap();

    // Bob signs in with the temporary password the admin chose — and knows.
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/login",
            serde_json::json!({
                "email": "bob@example.com",
                "password": "temporary pass phrase",
            }),
        ))
        .await
        .unwrap();
    let temp_cookie = session_cookie(&response);

    // That session is good for exactly one thing: replacing the password. The
    // admin knows this secret and conveyed it over some channel they do not
    // control, so it must not be a working session for anything else.
    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/tasks",
            serde_json::json!({ "title": "work" }),
            &temp_cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(response).await["code"],
        "password_change_required"
    );

    // Chat too — this is enforced at the extractors, not per-route.
    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/chat",
            serde_json::json!({ "message": "hi" }),
            &temp_cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // But `me` and set-password stay open, or the user could never escape.
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie(
            "/api/v1/companies/acme/auth/me",
            &temp_cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The read stays open, the write does not: a temporary password must not be
    // spendable on the account's public name or face before it is replaced —
    // the admin who reset it knows the value and conveyed it over a channel
    // they do not control.
    let app = router(state.clone());
    let response = app
        .oneshot(patch_with_cookie(
            "/api/v1/companies/acme/auth/me",
            serde_json::json!({ "displayName": "Bob the Temp" }),
            &temp_cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(response).await["code"],
        "password_change_required"
    );

    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/auth/password",
            serde_json::json!({ "password": "his own long secret" }),
            &temp_cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Once replaced, the same session works normally.
    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/tasks",
            serde_json::json!({ "title": "work" }),
            &temp_cookie,
        ))
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "the flag must clear once the password is replaced, got {}",
        response.status()
    );
}

#[tokio::test]
async fn a_manifest_admin_invite_cannot_be_revoked_through_the_api() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let admin = login_via_link(&state, &sender, "ada@example.com").await;

    // Revoking a synthetic manifest invite must say so rather than silently
    // succeed — the manifest would re-grant on the next login anyway.
    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/companies/acme/users/invites/manifest:ada@example.com")
                .header("cookie", &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_mail_transport_still_returns_202_and_echoes_for_dev() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // No mail wired at all — the default offline build, on the default
    // loopback bind.
    let state = state_with(&home, ConnectionsRuntime::new()).await;
    assert!(
        request_dev_code(&state, "ada@example.com").await.is_some(),
        "without a transport the code must be echoed so local dev works"
    );
}

#[tokio::test]
async fn a_routable_host_never_echoes_the_code_even_with_no_mail() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // Routable bind, no mail transport: the code cannot be delivered — and it
    // must NOT come back in the response instead. Returning a credential to
    // whoever asked is worse than nobody being able to sign in.
    let state = state_bound_to(&home, "0.0.0.0:8080", ConnectionsRuntime::new()).await;

    assert_eq!(
        request_dev_code(&state, "ada@example.com").await,
        None,
        "a routable host must never echo a login code"
    );
}

#[tokio::test]
async fn a_loopback_host_with_no_mail_reissues_inside_the_resend_window() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, ConnectionsRuntime::new()).await;

    // Two sign-ins back to back, well inside the 60s resend window. Nothing is
    // mailed here — the code is echoed — so there is no mailbox to spare, and
    // the only thing a throttle could achieve is locking the sole local sign-in
    // path for a minute after each use. That is issue #271: the console's
    // Playwright bootstrap re-authenticates on every run and would fail on the
    // second one within a minute, reporting a broken host.
    let first = request_dev_code(&state, "ada@example.com")
        .await
        .expect("the first request must echo a code");
    let second = request_dev_code(&state, "ada@example.com")
        .await
        .expect("a second request inside the window must still echo a code");
    assert_ne!(first, second, "the second request must mint a fresh code");

    // And the fresh one is the live one: one live code per address still holds,
    // so the reissue invalidated its predecessor rather than leaving two open.
    let app = router(state.clone());
    let stale = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": first }),
        ))
        .await
        .unwrap();
    assert_eq!(
        stale.status(),
        StatusCode::UNAUTHORIZED,
        "reissuing must invalidate the previous code"
    );

    let app = router(state.clone());
    let live = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": second }),
        ))
        .await
        .unwrap();
    assert_eq!(live.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_routable_host_still_throttles_even_with_no_mail() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // No transport wired, but the host looks reachable from elsewhere. The
    // reissue exemption is for the echo path only: this host echoes nothing, so
    // it keeps the throttle and keeps the live code alone.
    let state = state_bound_to(&home, "0.0.0.0:8080", ConnectionsRuntime::new()).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();

    assert_eq!(request_dev_code(&state, "ada@example.com").await, None);
    let minted = runtime
        .login_codes()
        .latest_for_email(&id, "ada@example.com")
        .await
        .unwrap()
        .expect("the first request must have minted a code");

    assert_eq!(request_dev_code(&state, "ada@example.com").await, None);
    let after = runtime
        .login_codes()
        .latest_for_email(&id, "ada@example.com")
        .await
        .unwrap()
        .expect("the throttled request must leave the live code in place");
    assert_eq!(
        minted.code_hash, after.code_hash,
        "a throttled request must not replace the live code"
    );
}

#[tokio::test]
async fn with_mail_wired_the_code_is_never_echoed() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    assert_eq!(
        request_dev_code(&state, "ada@example.com").await,
        None,
        "a host that can send mail must never return the code in the response"
    );
    // It went to the mailbox instead.
    let sent = sender.sent();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1.body.contains("/login?company=acme&code="));
}

// ---------------------------------------------------------------------------
// The deployment bootstrap admin (`OPENCOMPANY_ADMIN_EMAIL`, issue #321)
// ---------------------------------------------------------------------------

/// The bug this fixes: a platform-provisioned company's manifest names nobody,
/// so before the variable existed *no address at all* could get in. The
/// injected address is the only one that can, and it comes out an admin — the
/// same grant a manifest entry gives, minted only on redemption.
#[tokio::test]
async fn the_env_admin_can_sign_in_to_a_company_whose_manifest_names_nobody() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) =
        state_with_admin_email(&home, manifest_without_admins(), Some("zoe@example.com")).await;

    let cookie = login_via_link(&state, &sender, "zoe@example.com").await;

    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie("/api/v1/companies/acme/auth/me", &cookie))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let me = body_json(response).await;
    assert_eq!(me["email"], "zoe@example.com");
    assert_eq!(
        me["role"], "admin",
        "the injected address bootstraps as an admin, exactly like a manifest entry"
    );
}

/// Unset, empty, and whitespace-only are one behaviour: the company as it was
/// before #321. Empty matters on its own — the platform renders the variable
/// for every tenant, so a tenant with no recorded creator gets an empty value
/// rather than no variable, and that must not grant anyone anything.
#[tokio::test]
async fn no_env_admin_leaves_a_provisioned_company_refusing_everyone() {
    for admin_email in [None, Some(""), Some("   ")] {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let (state, sender) =
            state_with_admin_email(&home, manifest_without_admins(), admin_email).await;

        assert_eq!(request_dev_code(&state, "zoe@example.com").await, None);
        assert!(
            sender.sent().is_empty(),
            "{admin_email:?} must grant no eligibility, so no link is ever sent"
        );
    }
}

/// The injected address admits exactly one address, not "anyone the platform
/// vouches for". Everyone else meets the same silence as before.
#[tokio::test]
async fn a_different_address_is_still_refused() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) =
        state_with_admin_email(&home, manifest_without_admins(), Some("zoe@example.com")).await;

    assert_eq!(request_dev_code(&state, "eve@example.com").await, None);
    assert!(
        sender.sent().is_empty(),
        "an address the platform did not name must get no link"
    );
}

/// Case and surrounding whitespace are normalized the way the manifest path
/// normalizes them. A value that only matched with the right capitalization
/// would be a lockout that reads as a typo.
#[tokio::test]
async fn the_env_admin_is_normalized_like_a_manifest_admin() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_admin_email(
        &home,
        manifest_without_admins(),
        Some("  ZOE@Example.COM  "),
    )
    .await;

    let cookie = login_via_link(&state, &sender, "zoe@example.com").await;
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie("/api/v1/companies/acme/auth/me", &cookie))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["email"], "zoe@example.com");
}

/// An address named in both places is one standing invite, not two. It stays a
/// `manifest:` row: that grant outlives the deployment's variable, so it is the
/// one the operator has to withdraw.
#[tokio::test]
async fn an_env_admin_already_in_the_manifest_is_not_invited_twice() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let manifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [users]\nadmins = [\"Ada@Example.com\", \"Bob@Example.com\"]\n",
    )
    .unwrap();
    let (state, sender) = state_with_admin_email(&home, manifest, Some("Bob@Example.com")).await;

    // Ada signs in so there is an admin to read the invite page with; she
    // becomes a user, which is why only bob's synthetic row is left.
    let admin = login_via_link(&state, &sender, "ada@example.com").await;
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie(
            "/api/v1/companies/acme/users/invites",
            &admin,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let invites = body_json(response).await;
    let rows = invites.as_array().expect("invite list");
    assert_eq!(
        rows.len(),
        1,
        "bob is named twice but is one invite: {invites}"
    );
    assert_eq!(rows[0]["email"], "bob@example.com");
    assert_eq!(rows[0]["id"], "manifest:bob@example.com");
    assert_eq!(rows[0]["role"], "admin", "the role must not change");
    assert_eq!(rows[0]["invitedBy"], "manifest");
}

/// An injected address the manifest does not name renders as its own
/// `platform:` row, so the invite page does not contradict who can log in — and
/// revoking it is refused, pointing at the variable rather than the manifest.
#[tokio::test]
async fn the_env_admin_shows_on_the_invite_page_and_cannot_be_revoked() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_admin_email(&home, manifest(), Some("zoe@example.com")).await;
    let admin = login_via_link(&state, &sender, "ada@example.com").await;

    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie(
            "/api/v1/companies/acme/users/invites",
            &admin,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let invites = body_json(response).await;
    let rows = invites.as_array().expect("invite list");
    assert_eq!(rows.len(), 1, "expected one synthetic row: {invites}");
    assert_eq!(rows[0]["id"], "platform:zoe@example.com");
    assert_eq!(rows[0]["invitedBy"], "platform");
    assert_eq!(rows[0]["role"], "admin");

    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/companies/acme/users/invites/platform:zoe@example.com")
                .header("cookie", &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "revoking would be a lie: the variable re-grants on the next login"
    );
    assert!(
        body_json(response).await["error"]
            .as_str()
            .unwrap()
            .contains("OPENCOMPANY_ADMIN_EMAIL")
    );
}

// ---------------------------------------------------------------------------
// Invite mail (issue #584)
//
// Adding a person used to write a record and mail nobody, while the console
// reported unconditional success. These turn on both halves: that a mail
// actually goes out, and that when it does not the caller is told so rather
// than being congratulated.
// ---------------------------------------------------------------------------

/// A wired transport that refuses mail to one address and delivers the rest.
///
/// "No mail configured" is not the only way an invite fails to arrive, and it
/// is the less dangerous one — it is at least visible in configuration. A
/// wired transport that rejects the message is the case where a success toast
/// is a lie, so it needs a mock of its own.
///
/// Refusal is per-recipient rather than global, for a reason that is not
/// convenience: a sender that failed everything would also fail the admin's own
/// login mail, leaving no way to reach the admin-authenticated route under
/// test. It is also the truer model — a transport rejects a *message*, not a
/// process.
#[derive(Clone)]
struct RefusingMailSender {
    refuse: String,
    accepted: RecordingMailSender,
}

#[async_trait::async_trait]
impl crate::server::ops::mailer::MailSender for RefusingMailSender {
    async fn send(
        &self,
        creds: &MailCredentials,
        email: &crate::server::ops::mailer::OutboundEmail,
    ) -> Result<(), crate::error::OpenCompanyError> {
        if email.to == self.refuse {
            // The same variant the real SMTP sender reports a rejected send
            // with, so this exercises the branch production actually takes.
            return Err(crate::error::OpenCompanyError::Store(
                "smtp send: the transport refused the message".to_string(),
            ));
        }
        self.accepted.send(creds, email).await
    }
}

/// State whose transport works except for mail addressed to `refuse`.
///
/// Returns the recorder of everything it *did* accept, so a test can assert
/// that the refused message is genuinely absent rather than merely unreported.
async fn state_refusing_mail_to(
    home: &std::path::Path,
    refuse: &str,
) -> (AppState, RecordingMailSender) {
    let accepted = RecordingMailSender::new();
    let sender = RefusingMailSender {
        refuse: refuse.to_string(),
        accepted: accepted.clone(),
    };
    let connections = ConnectionsRuntime::new()
        .with_mail(Arc::new(sender))
        .with_mail_credentials(MailCredentials::Smtp(SmtpCredentials {
            host: "smtp.test".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "u".into(),
            password: SecretValue("p".into()),
            from_name: "Acme".into(),
            from_email: "noreply@acme.test".into(),
        }));
    (state_with(home, connections).await, accepted)
}

/// A transport that revokes the invite it is delivering, mid-send.
///
/// The race being modelled is an admin pressing Revoke while the SMTP round
/// trip is still in flight — the exact window the route's post-send stamp sits
/// in. Revoking from inside `send` reproduces that window deterministically:
/// no sleep, no second task, and the route is provably still holding its
/// pre-send copy of the record when the revocation lands.
#[derive(Clone)]
struct RevokingMailSender {
    /// Filled once the state exists, since the runtime this revokes through is
    /// the one the state owns — and the state cannot be built until the sender
    /// it borrows is already wired into its connections.
    runtime: Arc<std::sync::OnceLock<Arc<crate::runtime::CompanyRuntime>>>,
    revoke_for: String,
    accepted: RecordingMailSender,
}

#[async_trait::async_trait]
impl crate::server::ops::mailer::MailSender for RevokingMailSender {
    async fn send(
        &self,
        creds: &MailCredentials,
        email: &crate::server::ops::mailer::OutboundEmail,
    ) -> Result<(), crate::error::OpenCompanyError> {
        if email.to == self.revoke_for {
            let runtime = self
                .runtime
                .get()
                .expect("the runtime is wired before any invite is sent");
            let invite = runtime
                .users()
                .find_invite_by_email(runtime.id(), &self.revoke_for)
                .await
                .unwrap()
                .expect("the grant lands before the mail goes out");
            assert!(
                runtime
                    .users()
                    .delete_invite(runtime.id(), &invite.id)
                    .await
                    .unwrap(),
                "the revocation this models must actually remove the invite"
            );
        }
        self.accepted.send(creds, email).await
    }
}

/// Signs an admin in on a host with no mail transport, via the dev echo.
async fn login_via_dev_code(state: &AppState, email: &str) -> String {
    let code = request_dev_code(state, email)
        .await
        .expect("a loopback host with no transport echoes the code");
    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": code }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    session_cookie(&response)
}

/// Invites `email` as `admin`, returning the status and decoded body.
async fn invite_as(state: &AppState, admin: &str, email: &str) -> (StatusCode, serde_json::Value) {
    let app = router(state.clone());
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/companies/acme/users/invites",
            serde_json::json!({ "email": email }),
            admin,
        ))
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

#[tokio::test]
async fn inviting_someone_mails_them_a_credential_free_invitation() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let admin = login_via_link(&state, &sender, "ada@example.com").await;
    let before = sender.sent().len();

    let (status, body) = invite_as(&state, &admin, "bob@example.com").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["delivery"], "sent");
    // The record's own fields stay top level: the response is additive.
    assert_eq!(body["email"], "bob@example.com");
    assert!(
        body["notifiedAtMillis"].is_number(),
        "a sent invite must record when: {body}"
    );

    let sent = sender.sent();
    assert_eq!(
        sent.len() - before,
        1,
        "inviting once must send exactly one mail"
    );
    let mail = &sent.last().unwrap().1;
    assert_eq!(mail.to, "bob@example.com");
    assert!(
        mail.subject.contains("Acme"),
        "the subject must name the company: {}",
        mail.subject
    );
    assert!(
        mail.body.contains("/login?company=acme"),
        "the mail must say where to sign in: {}",
        mail.body
    );

    // The property the issue's acceptance criteria turn on: this is a
    // notification, not a credential. The allowlist plus the magic link stays
    // the only way in, so nothing redeemable may appear in the body.
    let invite_id = body["id"].as_str().expect("an invite id");
    assert!(
        !mail.body.contains(invite_id),
        "the invite id must not travel in the mail: {}",
        mail.body
    );
    assert!(
        !mail.body.contains("code="),
        "the mail must carry no login code: {}",
        mail.body
    );
    // The inviter is named from the local part, never by full address — and
    // through `UserRecord::display_label`, so the name in this mail is the one
    // the invitee will meet in the console a minute later.
    assert!(
        mail.body.contains("Ada"),
        "the mail must name who invited them: {}",
        mail.body
    );
    assert!(
        !mail.body.contains("ada@example.com"),
        "the inviter's full address must not be disclosed: {}",
        mail.body
    );

    // And the stamp is durable, not just echoed in the response.
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie(
            "/api/v1/companies/acme/users/invites",
            &admin,
        ))
        .await
        .unwrap();
    let invites = body_json(response).await;
    let row = invites
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["email"] == "bob@example.com")
        .expect("the invite must be listed");
    assert!(
        row["notifiedAtMillis"].is_number(),
        "the mailed stamp must survive the store: {row}"
    );
}

#[tokio::test]
async fn inviting_on_a_host_with_no_mail_says_so_instead_of_reporting_success() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // No transport at all — the shape the issue calls out as "fails quietly".
    let state = state_with(&home, ConnectionsRuntime::new()).await;
    let admin = login_via_dev_code(&state, "ada@example.com").await;

    let (status, body) = invite_as(&state, &admin, "bob@example.com").await;
    assert_eq!(status, StatusCode::OK, "the grant still succeeds");
    assert_eq!(
        body["delivery"], "no_transport",
        "the operator must be told nothing was mailed: {body}"
    );
    assert!(
        body["notifiedAtMillis"].is_null(),
        "nothing was mailed, so nothing may be stamped: {body}"
    );

    // The invite itself is real — the person can sign in, they just have to be
    // told out of band. Reporting no_transport must not have skipped the grant.
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie(
            "/api/v1/companies/acme/users/invites",
            &admin,
        ))
        .await
        .unwrap();
    let invites = body_json(response).await;
    assert!(
        invites
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["email"] == "bob@example.com"),
        "the invite must exist regardless of mail: {invites}"
    );
}

#[tokio::test]
async fn a_failing_transport_is_reported_and_never_rolls_back_the_invite() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, accepted) = state_refusing_mail_to(&home, "bob@example.com").await;
    let admin = login_via_link(&state, &accepted, "ada@example.com").await;
    let before = accepted.sent().len();

    let (status, body) = invite_as(&state, &admin, "bob@example.com").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a mail failure must not fail the grant"
    );
    assert_eq!(
        body["delivery"], "failed",
        "a refused message must be reported, not swallowed: {body}"
    );
    assert!(
        body["notifiedAtMillis"].is_null(),
        "nothing arrived, so nothing may be stamped as sent: {body}"
    );
    assert_eq!(
        accepted.sent().len(),
        before,
        "the refused message must not appear as delivered"
    );

    // The grant survives the failed send. This is the half that must not
    // regress: rolling the invite back would turn a mail outage into a silent
    // refusal to add people, and re-inviting would then 409 against a record
    // the operator was told did not exist.
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie(
            "/api/v1/companies/acme/users/invites",
            &admin,
        ))
        .await
        .unwrap();
    let invites = body_json(response).await;
    assert!(
        invites
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["email"] == "bob@example.com"),
        "the invite must survive a failed send: {invites}"
    );
}

#[tokio::test]
async fn an_invite_revoked_while_its_mail_is_in_flight_stays_revoked() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let accepted = RecordingMailSender::new();
    let runtime_cell = Arc::new(std::sync::OnceLock::new());
    let connections = ConnectionsRuntime::new()
        .with_mail(Arc::new(RevokingMailSender {
            runtime: runtime_cell.clone(),
            revoke_for: "bob@example.com".to_string(),
            accepted: accepted.clone(),
        }))
        .with_mail_credentials(MailCredentials::Smtp(SmtpCredentials {
            host: "smtp.test".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "u".into(),
            password: SecretValue("p".into()),
            from_name: "Acme".into(),
            from_email: "noreply@acme.test".into(),
        }));
    let state = state_with(&home, connections).await;
    let _ = runtime_cell.set(
        state
            .registry()
            .get(&CompanyId::new("acme"))
            .expect("the company is registered"),
    );
    let admin = login_via_link(&state, &accepted, "ada@example.com").await;

    let (status, body) = invite_as(&state, &admin, "bob@example.com").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["delivery"], "sent",
        "the message really did leave, and reporting otherwise would be a lie: {body}"
    );
    assert!(
        body["notifiedAtMillis"].is_null(),
        "an invite revoked mid-send has nothing left to stamp: {body}"
    );

    // The property this test exists for. The stamp is written from a record
    // read before the send, so an upsert would put the revoked invite back —
    // silently returning an address to the allowlist after an admin removed it,
    // with nothing on screen to say so.
    let app = router(state.clone());
    let response = app
        .oneshot(get_with_cookie(
            "/api/v1/companies/acme/users/invites",
            &admin,
        ))
        .await
        .unwrap();
    let invites = body_json(response).await;
    assert!(
        !invites
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["email"] == "bob@example.com"),
        "the mailed stamp must not restore a revoked invite: {invites}"
    );
}

#[tokio::test]
async fn a_refused_invite_mails_nobody() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, sender) = state_with_mail(&home).await;
    let admin = login_via_link(&state, &sender, "ada@example.com").await;

    // Already a member: Ada bootstraps from the manifest and has signed in.
    let before = sender.sent().len();
    let (status, _) = invite_as(&state, &admin, "ada@example.com").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        sender.sent().len(),
        before,
        "an invite refused as already-a-member must mail nobody"
    );

    // And a duplicate outstanding invite is refused by the store, also silently
    // as far as the mailbox is concerned — one invitation per address, not one
    // per click. Without this, the button is a mail cannon aimed at whoever an
    // admin most recently typed.
    let (status, _) = invite_as(&state, &admin, "bob@example.com").await;
    assert_eq!(status, StatusCode::OK);
    let after_first = sender.sent().len();

    let (status, _) = invite_as(&state, &admin, "bob@example.com").await;
    assert_eq!(status, StatusCode::CONFLICT, "one invite per address");
    assert_eq!(
        sender.sent().len(),
        after_first,
        "a duplicate invite must not re-mail"
    );

    // A malformed address never reaches a transport either.
    let (status, _) = invite_as(&state, &admin, "not-an-email").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sender.sent().len(),
        after_first,
        "a rejected address must mail nobody"
    );
}

// ---------------------------------------------------------------------------
// The profile: naming yourself and choosing your own face
// (docs/spec/runtime/avatars.md)
// ---------------------------------------------------------------------------

fn patch_with_cookie(uri: &str, body: serde_json::Value, cookie: &str) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("content-type", "application/json")
        .header("cookie", cookie)
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn patch_me(
    state: &AppState,
    cookie: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = router(state.clone())
        .oneshot(patch_with_cookie(
            "/api/v1/companies/acme/auth/me",
            body,
            cookie,
        ))
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

/// A person names themselves and picks a face, without an admin in the loop.
/// The whole reason this route exists beside the admin one: your own identity
/// in a company should not be something you have to ask for.
#[tokio::test]
async fn a_person_can_name_themselves_and_pick_a_face() {
    let home = home();
    let (state, sender) = state_with_mail(home.path()).await;
    let cookie = login_via_link(&state, &sender, "ada@example.com").await;

    let (status, me) = patch_me(
        &state,
        &cookie,
        serde_json::json!({"displayName": "Ada L.", "avatar": "tiny:violet"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["displayName"], "Ada L.", "{me}");
    assert_eq!(me["avatar"], "tiny:violet", "{me}");

    // Persisted, not just echoed.
    let response = router(state.clone())
        .oneshot(get_with_cookie("/api/v1/companies/acme/auth/me", &cookie))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let reread = body_json(response).await;
    assert_eq!(reread["displayName"], "Ada L.", "{reread}");
    assert_eq!(reread["avatar"], "tiny:violet", "{reread}");
}

/// A partial save leaves the field it did not mention alone — the reason both
/// fields are double options. Without it, saving a name wipes the face.
#[tokio::test]
async fn editing_one_field_of_a_profile_leaves_the_other() {
    let home = home();
    let (state, sender) = state_with_mail(home.path()).await;
    let cookie = login_via_link(&state, &sender, "ada@example.com").await;

    patch_me(&state, &cookie, serde_json::json!({"avatar": "tiny:rose"})).await;
    let (_, named) = patch_me(&state, &cookie, serde_json::json!({"displayName": "Ada"})).await;
    assert_eq!(named["avatar"], "tiny:rose", "{named}");

    // And each is individually resettable: `null` — or a blanked input, which is
    // the same intent typed — goes back to the default.
    let (_, unnamed) = patch_me(&state, &cookie, serde_json::json!({"displayName": "  "})).await;
    assert!(
        unnamed.get("displayName").is_none(),
        "a blank name is not a name: {unnamed}"
    );
    assert_eq!(unnamed["avatar"], "tiny:rose", "{unnamed}");
    let (_, bare) = patch_me(&state, &cookie, serde_json::json!({"avatar": null})).await;
    assert!(
        bare.get("avatar").is_none(),
        "a reset is absent, not empty: {bare}"
    );
}

/// The grammar's rule, on this route too: an avatar names something this host
/// holds, never a URL the console would fetch on this person's behalf.
#[tokio::test]
async fn a_profile_avatar_may_not_be_a_url() {
    let home = home();
    let (state, sender) = state_with_mail(home.path()).await;
    let cookie = login_via_link(&state, &sender, "ada@example.com").await;

    for hostile in [
        "https://tracker.example/beacon.gif",
        "javascript:alert(1)",
        "blob:01NOSUCHNODE",
    ] {
        let (status, refused) =
            patch_me(&state, &cookie, serde_json::json!({"avatar": hostile})).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{hostile} was accepted: {refused}"
        );
    }
}

/// A display name is a bounded field: it renders on every surface that shows a
/// person and rides in every roster payload, so a page of text parked in it
/// would be served to everyone. The bound is a `400`, not a truncation.
#[tokio::test]
async fn a_profile_name_may_not_exceed_the_bound() {
    let home = home();
    let (state, sender) = state_with_mail(home.path()).await;
    let cookie = login_via_link(&state, &sender, "ada@example.com").await;

    let long = "A".repeat(crate::server::users::MAX_DISPLAY_NAME_CHARS + 1);
    let (status, refused) =
        patch_me(&state, &cookie, serde_json::json!({"displayName": long})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    // Nothing was persisted, and a subsequent normal save still works.
    let (status, _) = patch_me(
        &state,
        &cookie,
        serde_json::json!({"displayName": "Ada L."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// No session, no profile. There is no `user_id` in the path to point at
/// somebody else, so this is the whole of the route's authority check.
#[tokio::test]
async fn a_profile_edit_needs_a_session() {
    let home = home();
    let (state, _sender) = state_with_mail(home.path()).await;
    let response = router(state.clone())
        .oneshot(patch_with_cookie(
            "/api/v1/companies/acme/auth/me",
            serde_json::json!({"displayName": "Nobody"}),
            "oc_session_acme=not-a-session",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Materialization races (issue #1833)
// ---------------------------------------------------------------------------

/// A runtime over a company with no `[users] admins`, which is all these need:
/// `local_owner_record` answers a mode question nobody asks it, so the manifest
/// only has to produce a store.
async fn users_runtime(home: &std::path::Path) -> Arc<crate::CompanyRuntime> {
    let (connections, _sender) = mail_connections();
    let state = state_from(
        home,
        manifest_without_admins(),
        AppConfig::default(),
        connections,
    )
    .await;
    state.registry().get(&CompanyId::new("acme")).unwrap()
}

/// The loser of a race adopts the winner rather than being refused.
///
/// Deterministic where the race itself is not: it stages the exact state a
/// losing caller finds itself in — an address already held, by an id that is not
/// the one this caller minted — and asserts the outcome is the winner's record
/// rather than a `Conflict`.
///
/// Before #1833 this returned `Err(Conflict)`, `graphql::auth` turned that into
/// `GatesRefused`, and the desktop console reported the healthy host it was
/// talking to as "Unreachable".
#[tokio::test]
async fn a_lost_materialization_race_adopts_the_winner() {
    use crate::ports::users::{UserRecord, UserRole, UserStatus};

    let home = home();
    let runtime = users_runtime(home.path()).await;
    let id = runtime.id();
    let email = crate::ports::users::LoginIdentity::Local.key();

    let winner = UserRecord {
        id: "winner-id".to_string(),
        email: email.clone(),
        display_name: None,
        avatar: None,
        role: UserRole::Admin,
        status: UserStatus::Active,
        password_hash: None,
        must_change_password: false,
        created_at_millis: 1,
        last_seen_at_millis: Some(1),
        updated_at_millis: 1,
    };
    runtime.users().upsert_user(id, &winner).await.unwrap();

    // The loser: same address, its own freshly generated id — which is exactly
    // what makes the store refuse it.
    let loser = UserRecord {
        id: "loser-id".to_string(),
        created_at_millis: 2,
        ..winner.clone()
    };

    let adopted = crate::server::users::routes::insert_or_adopt(&runtime, loser)
        .await
        .expect("a lost race is not an error — the owner exists");

    assert_eq!(
        adopted.id, "winner-id",
        "the loser must return the record that won, not its own"
    );

    // And the store still holds exactly one owner: adopting must not have
    // written the loser's id alongside the winner's.
    let held = runtime.users().list_users(id).await.unwrap();
    let owners: Vec<_> = held.iter().filter(|u| u.email == email).collect();
    assert_eq!(owners.len(), 1, "exactly one owner record: {held:?}");
    assert_eq!(owners[0].id, "winner-id");
}

/// Concurrent callers converge on one owner.
///
/// The shape of the original bug: N requests arrive together on a cold store,
/// all miss `find_user_by_email`, and each presents a different `generate_id()`
/// for one address. On the desktop's first boot three of them raced; one won and
/// two were refused 16ms later.
///
/// Timing-dependent by nature — it cannot *guarantee* an interleaving — so it is
/// the companion to the deterministic test above rather than the proof. What it
/// does catch is a regression that reintroduces the shape, and it fails reliably
/// against the pre-#1833 code at this width.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_local_owner_materialization_yields_one_record() {
    let home = home();
    let runtime = users_runtime(home.path()).await;

    let racers = 16;
    let mut tasks = Vec::with_capacity(racers);
    for _ in 0..racers {
        let runtime = Arc::clone(&runtime);
        tasks.push(tokio::spawn(async move {
            crate::server::users::routes::local_owner_record(&runtime).await
        }));
    }

    let mut ids = Vec::with_capacity(racers);
    for task in tasks {
        let record = task
            .await
            .expect("no racer panics")
            .expect("no racer is refused its own company's owner");
        ids.push(record.id);
    }

    let first = &ids[0];
    assert!(
        ids.iter().all(|id| id == first),
        "every racer must see one owner, got {ids:?}"
    );

    let held = runtime.users().list_users(runtime.id()).await.unwrap();
    let key = crate::ports::users::LoginIdentity::Local.key();
    assert_eq!(
        held.iter().filter(|u| u.email == key).count(),
        1,
        "exactly one owner record survives the race: {held:?}"
    );
}
