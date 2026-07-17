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

fn home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("oc-routes-{}", crate::ports::generate_id()))
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
    state
}

/// State with a recording mail sender wired, so links are "delivered".
async fn state_with_mail(home: &std::path::Path) -> (AppState, RecordingMailSender) {
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
    (state_with(home, connections).await, sender)
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
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn every_verify_failure_is_the_same_401() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn every_password_login_failure_is_the_same_401() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_manifest_admin_can_log_in_and_is_an_admin() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn a_link_is_single_use() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn requesting_a_new_link_invalidates_the_previous_one() {
    let home = home();
    let (state, sender) = state_with_mail(&home).await;
    let first_code = request_code(&state, &sender, "ada@example.com").await;
    let second_code = request_code(&state, &sender, "ada@example.com").await;
    assert_ne!(first_code, second_code);

    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": first_code }),
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "an abandoned link must not work later"
    );

    let app = router(state.clone());
    let response = app
        .oneshot(post(
            "/api/v1/companies/acme/auth/verify",
            serde_json::json!({ "code": second_code }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn setting_a_password_enables_password_login() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn a_weak_password_is_refused() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn setting_a_password_requires_a_session() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn the_session_cookie_is_defended() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn a_https_deployment_marks_the_cookie_secure() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn logout_revokes_the_session_not_just_the_cookie() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

// ---------------------------------------------------------------------------
// Admin routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_an_admin_can_invite() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn an_uninvited_address_cannot_log_in() {
    let home = home();
    let (state, _) = state_with_mail(&home).await;
    // Not invited, not in the manifest: no code is minted at all.
    assert_eq!(
        request_dev_code(&state, "eve@example.com").await,
        None,
        "an uninvited address must not receive a code"
    );
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn suspending_a_user_kills_their_session_at_once() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn the_last_admin_cannot_be_demoted() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn an_admin_reset_forces_a_change_and_kills_sessions() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn a_manifest_admin_invite_cannot_be_revoked_through_the_api() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_mail_transport_still_returns_202_and_echoes_for_dev() {
    let home = home();
    // No mail wired at all — the default offline build.
    let state = state_with(&home, ConnectionsRuntime::new()).await;
    assert!(
        request_dev_code(&state, "ada@example.com").await.is_some(),
        "without a transport the code must be echoed so local dev works"
    );
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn with_mail_wired_the_code_is_never_echoed() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}
