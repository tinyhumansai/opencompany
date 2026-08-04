//! End-to-end tests for the login and admin routes.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::ports::CompanyStore;
use crate::ports::types::{CompanyId, CompanyRecord};
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
            template_provenance: None,
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
            password: "p".into(),
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
            template_provenance: None,
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
                password: "p".into(),
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
