//! End-to-end tests for the three sign-in modes.
//!
//! The mode is configuration, and configuration that silently does not take
//! effect is the failure this file exists to prevent. Three properties matter
//! more than the happy paths:
//!
//! 1. **A mode's routes are the only ones that answer.** A wallet company must
//!    not also accept a magic link, or the roster has a second door nobody
//!    configured. A `none` company must not accept either.
//! 2. **`none` really admits somebody.** A mode that turns the login off and
//!    then leaves every request unauthenticated is not "no sign-in", it is a
//!    bricked console — and it would look identical from the outside.
//! 3. **A wallet signature is checked against the challenge the host issued**,
//!    not against anything the caller supplied.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use ed25519_dalek::{Signer as _, SigningKey};
use tower::ServiceExt;

use crate::app::config::AuthMode;
use crate::company::CompanyManifest;
use crate::ports::CompanyStore;
use crate::ports::types::{CompanyId, CompanyRecord, SecretValue};
use crate::runtime::RuntimeBuilder;
use crate::server::ops::ConnectionsRuntime;
use crate::server::ops::mailer::{MailCredentials, RecordingMailSender};
use crate::server::ops::smtp::{SmtpCredentials, SmtpSecurity};
use crate::server::router;
use crate::server::users::token;
use crate::server::users::wallet::{self, VerifyRequest};
use crate::{AppConfig, AppState};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oc-authmode-")
        .tempdir()
        .expect("tempdir")
}

/// A wallet keypair, from a fixed seed so a failure is reproducible.
fn wallet(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn address(key: &SigningKey) -> String {
    bs58::encode(key.verifying_key().to_bytes()).into_string()
}

/// Builds a host serving one company in `mode`, bootstrapping `bootstrap` as an
/// admin through whichever manifest list that mode reads.
async fn state_in_mode(
    home: &std::path::Path,
    mode: AuthMode,
    bootstrap: Option<&str>,
) -> AppState {
    state_in_mode_on(
        home,
        mode,
        bootstrap,
        AppConfig::default(),
        ConnectionsRuntime::new(),
    )
    .await
}

/// The same host, over an explicit config and connection set — for the
/// questions whose answer is a property of the *deployment* rather than the
/// mode: whether the bind is routable, and whether mail is wired.
async fn state_in_mode_on(
    home: &std::path::Path,
    mode: AuthMode,
    bootstrap: Option<&str>,
    config: AppConfig,
    connections: ConnectionsRuntime,
) -> AppState {
    let toml_src = match (mode, bootstrap) {
        (AuthMode::Email, Some(who)) => {
            format!("[company]\nname = \"Acme\"\n[users]\nmode = \"email\"\nadmins = [\"{who}\"]\n")
        }
        (AuthMode::Wallet, Some(who)) => {
            format!(
                "[company]\nname = \"Acme\"\n[users]\nmode = \"wallet\"\nwallets = [\"{who}\"]\n"
            )
        }
        (mode, _) => format!(
            "[company]\nname = \"Acme\"\n[users]\nmode = \"{}\"\n",
            mode.as_str()
        ),
    };
    let manifest: CompanyManifest = toml::from_str(&toml_src).expect("valid manifest");
    assert!(
        manifest.validate().is_empty(),
        "the test manifest must be valid: {:?}",
        manifest.validate()
    );

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

/// A routable bind: nothing is echoed back to the caller here, so a magic link
/// is only usable if it can genuinely be mailed.
fn routable() -> AppConfig {
    AppConfig {
        bind: "0.0.0.0:8080".to_string(),
        ..AppConfig::default()
    }
}

/// A wired mail transport, so a link is actually sent.
fn mail_connections() -> ConnectionsRuntime {
    ConnectionsRuntime::new()
        .with_mail(Arc::new(RecordingMailSender::new()))
        .with_mail_credentials(MailCredentials::Smtp(SmtpCredentials {
            host: "smtp.test".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "u".into(),
            password: SecretValue("p".into()),
            from_name: "Acme".into(),
            from_email: "noreply@acme.test".into(),
        }))
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
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

// ---------------------------------------------------------------------------
// What the console is told
// ---------------------------------------------------------------------------

/// The console cannot draw a sign-in screen without this, and it must be able to
/// ask before it has any credential.
#[tokio::test]
async fn auth_config_publishes_the_mode_to_an_anonymous_caller() {
    for (mode, passwords) in [
        (AuthMode::Email, true),
        (AuthMode::Wallet, false),
        (AuthMode::None, false),
    ] {
        let dir = home();
        let state = state_in_mode(dir.path(), mode, None).await;
        let response = router(state)
            .oneshot(get("/api/v1/company/auth/config"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{mode}");
        let body = body_json(response).await;
        assert_eq!(body["mode"], mode.as_str(), "{mode}");
        assert_eq!(body["passwords"], passwords, "{mode}");
    }
}

/// The sign-in screen is the one place a person confirms *what* they are
/// signing in to before handing over a credential, and on the hosted platform
/// every tenant is a separate company on its own URL. The console cannot ask
/// anything else for the name — every other route that reports it is behind the
/// very sign-in being drawn — so it has to come back here (issue #1334).
#[tokio::test]
async fn auth_config_names_the_company_to_an_anonymous_caller() {
    for mode in [AuthMode::Email, AuthMode::Wallet, AuthMode::None] {
        let dir = home();
        let state = state_in_mode(dir.path(), mode, None).await;
        let response = router(state)
            .oneshot(get("/api/v1/company/auth/config"))
            .await
            .unwrap();
        let body = body_json(response).await;
        // The manifest's display name, not the id it is stored under — the
        // fixture spells them differently ("Acme" vs `acme`) precisely so a
        // fallback to the id cannot pass this.
        assert_eq!(
            body["name"], "Acme",
            "every mode draws a heading, so every mode needs the name: {body}"
        );
    }
}

/// A manifest that names the company nothing still has to produce a heading.
/// The id is what every other surface calls it in that case — `status` makes
/// the same substitution — and a blank `h1` is the bug this field exists to
/// remove, so it must not be reachable by writing `name = ""`.
#[tokio::test]
async fn auth_config_falls_back_to_the_company_id_when_the_manifest_has_no_name() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::Email, None).await;
    let id = CompanyId::new("acme");
    let store = crate::store::FsCompanyStore::new(dir.path().to_path_buf());
    let mut record = store.load(&id).await.unwrap().expect("the fixture record");
    record.manifest.company.name = "   ".to_string();
    store.save(&record).await.unwrap();

    let response = router(state)
        .oneshot(get("/api/v1/company/auth/config"))
        .await
        .unwrap();
    let body = body_json(response).await;
    assert_eq!(
        body["name"], "acme",
        "a blank name is not a heading; the id is: {body}"
    );
}

/// The record the name comes from is not there, or is not readable.
///
/// Both are the same statement: a heading is decoration on a route whose real
/// payload is the mode, and a console that cannot learn the mode draws the
/// wrong screen entirely. So neither case may fail the request — they fall back
/// to the id, exactly as a blank name does.
///
/// The two are separate paths in `display_name`: a missing bundle is `Ok(None)`
/// from the store, an unreadable one is `Err`, and the `Err` arm is the one that
/// would take the route down if it were propagated.
#[tokio::test]
async fn auth_config_falls_back_to_the_company_id_when_the_record_cannot_be_read() {
    for (case, contents) in [("gone", None), ("unreadable", Some("}} not toml {{"))] {
        let dir = home();
        let state = state_in_mode(dir.path(), AuthMode::Email, None).await;
        let manifest_path =
            crate::store::paths::Bundle::new(dir.path(), &CompanyId::new("acme")).company_toml();
        match contents {
            Some(garbage) => std::fs::write(&manifest_path, garbage).unwrap(),
            None => std::fs::remove_file(&manifest_path).unwrap(),
        }

        let response = router(state)
            .oneshot(get("/api/v1/company/auth/config"))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a missing name must not cost the console the mode ({case})"
        );
        let body = body_json(response).await;
        assert_eq!(body["name"], "acme", "({case}) {body}");
        assert_eq!(body["mode"], "email", "({case}) {body}");
    }
}

/// A routable host with no transport cannot deliver a magic link and will not
/// echo the code either, so the form is a dead end. The console has to be told
/// that in the payload — from the outside a link request there answers `sent`
/// exactly like one that worked.
#[tokio::test]
async fn auth_config_reports_a_magic_link_that_cannot_arrive() {
    let dir = home();
    let state = state_in_mode_on(
        dir.path(),
        AuthMode::Email,
        None,
        routable(),
        ConnectionsRuntime::new(),
    )
    .await;
    let response = router(state)
        .oneshot(get("/api/v1/company/auth/config"))
        .await
        .unwrap();
    let body = body_json(response).await;

    assert_eq!(
        body["magicLink"], false,
        "no transport and no echo is a dead end: {body}"
    );
}

/// The two ways a link does reach the person: mailed, or — on a loopback host —
/// handed straight back in the response. The second is the laptop case, and
/// treating it as "no magic link" would take the form away from the only host
/// where it needs no configuration at all.
#[tokio::test]
async fn auth_config_reports_a_magic_link_that_is_mailed_or_echoed() {
    let dir = home();
    let mailed = state_in_mode_on(
        dir.path(),
        AuthMode::Email,
        None,
        routable(),
        mail_connections(),
    )
    .await;
    let response = router(mailed)
        .oneshot(get("/api/v1/company/auth/config"))
        .await
        .unwrap();
    let body = body_json(response).await;
    assert_eq!(
        body["magicLink"], true,
        "a wired transport sends it: {body}"
    );

    let echoed = home();
    let loopback = state_in_mode(echoed.path(), AuthMode::Email, None).await;
    let response = router(loopback)
        .oneshot(get("/api/v1/company/auth/config"))
        .await
        .unwrap();
    let body = body_json(response).await;
    assert_eq!(
        body["magicLink"], true,
        "a loopback host hands the code back: {body}"
    );
}

// ---------------------------------------------------------------------------
// One mode, one door
// ---------------------------------------------------------------------------

/// A wallet company has no magic link, no password login, and no hub buttons.
/// Each would be a second way onto the roster that nobody configured.
#[tokio::test]
async fn a_wallet_company_refuses_every_email_route() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::Wallet, None).await;
    let app = router(state);

    for request in [
        post(
            "/api/v1/company/auth/request",
            serde_json::json!({"email": "ada@example.com"}),
        ),
        post(
            "/api/v1/company/auth/verify",
            serde_json::json!({"code": "x"}),
        ),
        post(
            "/api/v1/company/auth/login",
            serde_json::json!({"email": "ada@example.com", "password": "hunter2hunter2"}),
        ),
    ] {
        let uri = request.uri().to_string();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT, "{uri}");
        let body = body_json(response).await;
        assert_eq!(body["code"], "auth_mode", "{uri}");
        // The refusal names the mode, so a console that got here can correct
        // itself rather than telling somebody their address was wrong.
        assert_eq!(body["mode"], "wallet", "{uri}");
    }

    // No ecosystem buttons either — a hub sign-in resolves to an email address
    // and would apply an email roster this company does not have.
    let response = app.oneshot(get("/api/v1/company/auth/hub")).await.unwrap();
    assert_eq!(
        body_json(response).await["providers"],
        serde_json::json!([])
    );
}

/// An email company has no wallet door.
#[tokio::test]
async fn an_email_company_refuses_the_wallet_routes() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::Email, None).await;
    let response = router(state)
        .oneshot(post(
            "/api/v1/company/auth/wallet/challenge",
            serde_json::json!({"address": address(&wallet(1))}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(response).await["mode"], "email");
}

// ---------------------------------------------------------------------------
// Wallet sign-in
// ---------------------------------------------------------------------------

/// The whole flow: challenge, sign, session. The signature is produced by a real
/// Ed25519 key over the exact bytes the host handed back, which is what a
/// browser wallet does.
#[tokio::test]
async fn a_bootstrapped_wallet_signs_in() {
    let dir = home();
    let key = wallet(3);
    let addr = address(&key);
    let app = router(state_in_mode(dir.path(), AuthMode::Wallet, Some(&addr)).await);

    let challenge = body_json(
        app.clone()
            .oneshot(post(
                "/api/v1/company/auth/wallet/challenge",
                serde_json::json!({"address": addr}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let message = challenge["message"].as_str().unwrap();
    // The layout is the host's and is versioned by its first line; a console
    // must sign it verbatim rather than rebuilding it.
    assert!(
        message.starts_with("opencompany-wallet-login-v1\nacme\n"),
        "{message}"
    );
    assert!(message.contains(&addr), "the address is bound: {message}");

    let signature = bs58::encode(key.sign(message.as_bytes()).to_bytes()).into_string();
    let response = app
        .oneshot(post(
            "/api/v1/company/auth/wallet/verify",
            serde_json::json!({"nonce": challenge["nonce"], "signature": signature}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let set_cookie = response
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        set_cookie.contains("oc_session_acme=") && set_cookie.contains("HttpOnly"),
        "a wallet sign-in mints the same ordinary session a link does: {set_cookie}"
    );
    let me = body_json(response).await;
    // The identity is stored scheme-prefixed, so it can never collide with an
    // email in the same column.
    assert_eq!(me["email"], format!("wallet:{addr}"));
    assert_eq!(me["role"], "admin");
}

/// A nonce is good exactly once. The store consumes it atomically, so a captured
/// request cannot be replayed.
#[tokio::test]
async fn a_challenge_cannot_be_answered_twice() {
    let dir = home();
    let key = wallet(4);
    let addr = address(&key);
    let app = router(state_in_mode(dir.path(), AuthMode::Wallet, Some(&addr)).await);

    let challenge = body_json(
        app.clone()
            .oneshot(post(
                "/api/v1/company/auth/wallet/challenge",
                serde_json::json!({"address": addr}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let signature = bs58::encode(
        key.sign(challenge["message"].as_str().unwrap().as_bytes())
            .to_bytes(),
    )
    .into_string();
    let answer = serde_json::json!({"nonce": challenge["nonce"], "signature": signature});

    let first = app
        .clone()
        .oneshot(post("/api/v1/company/auth/wallet/verify", answer.clone()))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let replay = app
        .oneshot(post("/api/v1/company/auth/wallet/verify", answer))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(replay).await["code"], "invalid_login");
}

/// Requesting a second challenge for the same wallet invalidates the first: the
/// route must not accumulate one durable `LoginCodeRecord` per request, which
/// would let an unauthenticated caller who keeps naming an eligible wallet grow
/// the challenge table without bound.
#[tokio::test]
async fn a_second_challenge_inside_the_throttle_window_does_not_invalidate_the_first() {
    let dir = home();
    let key = wallet(9);
    let addr = address(&key);
    let app = router(state_in_mode(dir.path(), AuthMode::Wallet, Some(&addr)).await);

    let first = body_json(
        app.clone()
            .oneshot(post(
                "/api/v1/company/auth/wallet/challenge",
                serde_json::json!({"address": addr}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let second = body_json(
        app.clone()
            .oneshot(post(
                "/api/v1/company/auth/wallet/challenge",
                serde_json::json!({"address": addr}),
            ))
            .await
            .unwrap(),
    )
    .await;

    // The second request landed inside the throttle window, so it answered
    // with a decoy rather than replacing the pending challenge — the first
    // nonce is still exactly what the owner should sign.
    let first_signature = bs58::encode(
        key.sign(first["message"].as_str().unwrap().as_bytes())
            .to_bytes(),
    )
    .into_string();
    let answered = app
        .clone()
        .oneshot(post(
            "/api/v1/company/auth/wallet/verify",
            serde_json::json!({"nonce": first["nonce"], "signature": first_signature}),
        ))
        .await
        .unwrap();
    assert_eq!(
        answered.status(),
        StatusCode::OK,
        "a throttled replacement must not invalidate the pending challenge"
    );

    // The decoy the second request returned is not a real challenge — it was
    // never persisted — so signing it answers nothing.
    let decoy_signature = bs58::encode(
        key.sign(second["message"].as_str().unwrap().as_bytes())
            .to_bytes(),
    )
    .into_string();
    let decoy_answer = app
        .oneshot(post(
            "/api/v1/company/auth/wallet/verify",
            serde_json::json!({"nonce": second["nonce"], "signature": decoy_signature}),
        ))
        .await
        .unwrap();
    assert_eq!(decoy_answer.status(), StatusCode::UNAUTHORIZED);
}

/// Once the throttle window has passed, a replacement challenge really does
/// invalidate the one it replaces — the throttle is a delay, not a
/// prohibition, so the roster's own admin still gets a working "one live
/// challenge" invariant once the window clears.
#[tokio::test]
async fn a_challenge_replaces_the_previous_one_once_the_throttle_window_passes() {
    let dir = home();
    let key = wallet(14);
    let addr = address(&key);
    let state = state_in_mode(dir.path(), AuthMode::Wallet, Some(&addr)).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    let t0 = 1_000_000_u64;
    let first = wallet::issue_challenge(&runtime, &token::OsTokens, &addr, t0)
        .await
        .unwrap();
    let t1 = t0 + wallet::CHALLENGE_RESEND_INTERVAL_MILLIS + 1;
    let second = wallet::issue_challenge(&runtime, &token::OsTokens, &addr, t1)
        .await
        .unwrap();
    assert_ne!(first.nonce, second.nonce);

    let stale_body = VerifyRequest {
        nonce: first.nonce,
        signature: bs58::encode(key.sign(first.message.as_bytes()).to_bytes()).into_string(),
    };
    assert!(
        wallet::verify_challenge(&runtime, &stale_body, t1)
            .await
            .is_none(),
        "the replaced challenge must no longer redeem"
    );

    let fresh_body = VerifyRequest {
        nonce: second.nonce,
        signature: bs58::encode(key.sign(second.message.as_bytes()).to_bytes()).into_string(),
    };
    assert!(
        wallet::verify_challenge(&runtime, &fresh_body, t1)
            .await
            .is_some()
    );
}

/// Inviting a wallet identity has no mailbox to write to, and the invite route
/// must say so — `no_mailbox`, not `no_transport` or a silent `sent` — since the
/// console renders this delivery status for an admin who typed the address in.
#[tokio::test]
async fn inviting_a_wallet_reports_no_mailbox_delivery() {
    let dir = home();
    let admin_key = wallet(11);
    let admin_addr = address(&admin_key);
    let app = router(state_in_mode(dir.path(), AuthMode::Wallet, Some(&admin_addr)).await);

    let challenge = body_json(
        app.clone()
            .oneshot(post(
                "/api/v1/company/auth/wallet/challenge",
                serde_json::json!({"address": admin_addr}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let signature = bs58::encode(
        admin_key
            .sign(challenge["message"].as_str().unwrap().as_bytes())
            .to_bytes(),
    )
    .into_string();
    let verify = app
        .clone()
        .oneshot(post(
            "/api/v1/company/auth/wallet/verify",
            serde_json::json!({"nonce": challenge["nonce"], "signature": signature}),
        ))
        .await
        .unwrap();
    assert_eq!(verify.status(), StatusCode::OK);
    let cookie = session_cookie(&verify);

    let invitee_addr = address(&wallet(12));
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/company/users/invites",
            serde_json::json!({"wallet": invitee_addr}),
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["delivery"], "no_mailbox",
        "a wallet invite has no mailbox to write to: {body}"
    );
    assert_eq!(body["email"], format!("wallet:{invitee_addr}"));
}

/// An invite naming the field the company's mode does not read is refused
/// rather than silently ignored — an admin who fills in the wrong field, or
/// both, must not believe they invited something they did not.
#[tokio::test]
async fn inviting_with_the_wrong_identity_field_is_refused() {
    let dir = home();
    let admin_key = wallet(13);
    let admin_addr = address(&admin_key);
    let app = router(state_in_mode(dir.path(), AuthMode::Wallet, Some(&admin_addr)).await);

    let challenge = body_json(
        app.clone()
            .oneshot(post(
                "/api/v1/company/auth/wallet/challenge",
                serde_json::json!({"address": admin_addr}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let signature = bs58::encode(
        admin_key
            .sign(challenge["message"].as_str().unwrap().as_bytes())
            .to_bytes(),
    )
    .into_string();
    let verify = app
        .clone()
        .oneshot(post(
            "/api/v1/company/auth/wallet/verify",
            serde_json::json!({"nonce": challenge["nonce"], "signature": signature}),
        ))
        .await
        .unwrap();
    let cookie = session_cookie(&verify);

    // An `email` field on a wallet company is refused, whether or not `wallet`
    // is also set.
    let response = app
        .oneshot(post_with_cookie(
            "/api/v1/company/users/invites",
            serde_json::json!({"email": "bob@example.com"}),
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// A wallet that is not on the roster gets a challenge shaped exactly like a
/// real one — the route must not be a membership oracle — and it verifies as
/// nothing.
#[tokio::test]
async fn an_uninvited_wallet_gets_a_challenge_that_does_not_work() {
    let dir = home();
    let invited = wallet(5);
    let stranger = wallet(6);
    let app = router(state_in_mode(dir.path(), AuthMode::Wallet, Some(&address(&invited))).await);

    let challenge = body_json(
        app.clone()
            .oneshot(post(
                "/api/v1/company/auth/wallet/challenge",
                serde_json::json!({"address": address(&stranger)}),
            ))
            .await
            .unwrap(),
    )
    .await;
    // Indistinguishable from an invited wallet's challenge.
    assert!(challenge["nonce"].as_str().is_some_and(|n| !n.is_empty()));
    assert!(
        challenge["message"]
            .as_str()
            .unwrap()
            .contains(&address(&stranger))
    );

    let signature = bs58::encode(
        stranger
            .sign(challenge["message"].as_str().unwrap().as_bytes())
            .to_bytes(),
    )
    .into_string();
    let response = app
        .oneshot(post(
            "/api/v1/company/auth/wallet/verify",
            serde_json::json!({"nonce": challenge["nonce"], "signature": signature}),
        ))
        .await
        .unwrap();
    // The same failure a forged signature gets. Nothing distinguishes "not on
    // the roster" from "that is not your key".
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await["code"], "invalid_login");
}

/// The address is taken from the stored challenge, never from the request, so a
/// wallet cannot answer a challenge issued to another one.
#[tokio::test]
async fn another_wallets_signature_does_not_answer_the_challenge() {
    let dir = home();
    let invited = wallet(7);
    let impostor = wallet(8);
    let addr = address(&invited);
    let app = router(state_in_mode(dir.path(), AuthMode::Wallet, Some(&addr)).await);

    let challenge = body_json(
        app.clone()
            .oneshot(post(
                "/api/v1/company/auth/wallet/challenge",
                serde_json::json!({"address": addr}),
            ))
            .await
            .unwrap(),
    )
    .await;
    // A perfectly valid signature — over the right bytes, by the wrong key.
    let signature = bs58::encode(
        impostor
            .sign(challenge["message"].as_str().unwrap().as_bytes())
            .to_bytes(),
    )
    .into_string();

    let response = app
        .oneshot(post(
            "/api/v1/company/auth/wallet/verify",
            serde_json::json!({"nonce": challenge["nonce"], "signature": signature}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// No sign-in at all
// ---------------------------------------------------------------------------

/// The point of the mode: a request carrying no credential is the owner. If this
/// failed, `none` would not be "no sign-in", it would be a console nobody can
/// use — and the two look identical from outside.
#[tokio::test]
async fn none_mode_serves_the_local_owner_with_no_credential() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::None, None).await;
    let response = router(state)
        .oneshot(get("/api/v1/company/auth/me"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let me = body_json(response).await;
    assert_eq!(me["email"], "local:owner");
    // The person at the machine owns the company; there is nobody for a lesser
    // role to be distinguished from.
    assert_eq!(me["role"], "admin");
}

/// `none` mode's local-owner resolution is peer-gated everywhere `MaybePeer`
/// reaches a handler — `CompanyAuth`, the GraphQL handler, and every REST route
/// that resolves a principal through `current_user` or `chat_actor` — as a
/// second, independent check alongside the bind-time refusal, see
/// `crate::server::graphql::auth::local_owner`. A loopback peer, or no peer at
/// all (an embedded caller or a router exercised directly, as this test itself
/// does for everything else), still resolves the owner; a non-loopback peer
/// does not, even though the same company would otherwise admit it.
#[tokio::test]
async fn none_mode_local_owner_resolution_refuses_a_non_loopback_peer() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::None, None).await;
    let app = router(state);

    // No peer info at all — an embedded caller, or this very test harness for
    // any route it does not explicitly wire below. Still resolves: this is not
    // itself a refusal, only a positive non-loopback finding is.
    let no_peer = app
        .clone()
        .oneshot(get("/api/v1/company/feedback"))
        .await
        .unwrap();
    assert_eq!(no_peer.status(), StatusCode::OK);

    // A loopback peer resolves the owner.
    let mut loopback_req = get("/api/v1/company/feedback");
    loopback_req.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:54321".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let loopback = app.clone().oneshot(loopback_req).await.unwrap();
    assert_eq!(loopback.status(), StatusCode::OK);

    // A non-loopback peer does not, even for the same company and route.
    let mut remote_req = get("/api/v1/company/feedback");
    remote_req.extensions_mut().insert(ConnectInfo(
        "203.0.113.7:54321".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let remote = app.oneshot(remote_req).await.unwrap();
    assert_eq!(
        remote.status(),
        StatusCode::UNAUTHORIZED,
        "a non-loopback peer must not resolve the none-mode local owner"
    );
}

/// The peer gate applies to `current_user`'s REST call sites too, not just
/// `CompanyAuth`/GraphQL — `/auth/me` is the simplest of them.
#[tokio::test]
async fn none_mode_auth_me_refuses_a_non_loopback_peer() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::None, None).await;
    let app = router(state);

    let mut req = get("/api/v1/company/auth/me");
    req.extensions_mut().insert(ConnectInfo(
        "203.0.113.7:1".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a non-loopback peer must not resolve the none-mode local owner through /auth/me either"
    );
}

/// A valid platform bearer is not a way past the peer/header gates on a
/// `none`-mode company: falling through to it once `local_owner` refuses
/// would make the gates decorative, since a bearer is just another credential
/// a remote caller could hold. The identical bearer must keep working
/// normally on an `email`-mode company — the refusal is specific to `none`
/// mode's local-only contract, not a blanket rule against platform auth.
#[tokio::test]
async fn none_mode_refuses_a_platform_bearer_from_a_non_loopback_peer() {
    use crate::server::platform_auth::{
        PlatformAuthConfig, PlatformClaims, UnsignedTenantVerifier,
    };

    let secret = "top-secret";
    let verifier = std::sync::Arc::new(UnsignedTenantVerifier::new(secret));
    let token = UnsignedTenantVerifier::tenant_token(&PlatformClaims {
        tenant: "tenant:acme".to_string(),
        scopes: std::collections::HashSet::from(["platform".to_string()]),
        companies: None,
    });
    let remote = ConnectInfo("203.0.113.7:1".parse::<std::net::SocketAddr>().unwrap());
    let auth_header = format!("Bearer {token}");

    let dir = home();
    let none_state = state_in_mode(dir.path(), AuthMode::None, None)
        .await
        .with_platform_auth(PlatformAuthConfig::new(verifier.clone()));
    let mut none_req = get("/api/v1/company/feedback");
    none_req.extensions_mut().insert(remote);
    none_req
        .headers_mut()
        .insert("authorization", auth_header.parse().unwrap());
    let none_response = router(none_state).oneshot(none_req).await.unwrap();
    assert_eq!(
        none_response.status(),
        StatusCode::UNAUTHORIZED,
        "a platform bearer must not stand in for the local owner on a none-mode company"
    );

    // The same bearer, unchanged, still works on an ordinary email-mode
    // company — this isn't a blanket refusal of platform auth.
    let email_state = state_in_mode(dir.path(), AuthMode::Email, None)
        .await
        .with_platform_auth(PlatformAuthConfig::new(verifier));
    let mut email_req = get("/api/v1/company/feedback");
    email_req.extensions_mut().insert(remote);
    email_req
        .headers_mut()
        .insert("authorization", auth_header.parse().unwrap());
    let email_response = router(email_state).oneshot(email_req).await.unwrap();
    assert_eq!(email_response.status(), StatusCode::OK);
}

/// A same-host reverse proxy connects to a loopback-bound listener over
/// loopback too, so the peer this process sees always reads as loopback
/// regardless of where the proxy's own caller actually was — the peer check
/// alone cannot see an undeclared proxy. A `Forwarded`/`X-Forwarded-*` header
/// is the signal that one is there, and `local_owner` must refuse on it even
/// when the peer itself looks perfectly local.
#[tokio::test]
async fn none_mode_local_owner_resolution_refuses_a_forwarded_request_even_from_a_loopback_peer() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::None, None).await;
    let app = router(state);

    let mut req = get("/api/v1/company/feedback");
    req.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:54321".parse::<std::net::SocketAddr>().unwrap(),
    ));
    req.headers_mut()
        .insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a proxy-forwarding header must refuse the none-mode local owner even from a loopback peer"
    );
}

/// A session minted while a company was `email` mode must not still
/// authenticate after the company is rebuilt into `none` mode — nothing purges
/// a company's session store on a manifest edit, so without this the peer and
/// forwarding-header gates on `none` mode's implicit owner would be moot: a
/// caller who already held (or stole) an old session could fall through to it
/// instead.
#[tokio::test]
async fn a_session_from_before_a_mode_flip_does_not_survive_it() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::Email, Some("ada@example.com")).await;
    let app = router(state.clone());

    let requested = body_json(
        app.clone()
            .oneshot(post(
                "/api/v1/company/auth/request",
                serde_json::json!({"email": "ada@example.com"}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let code = requested["dev_code"]
        .as_str()
        .expect("a loopback host with no mail transport echoes the code");
    let verify = app
        .oneshot(post(
            "/api/v1/company/auth/verify",
            serde_json::json!({"code": code}),
        ))
        .await
        .unwrap();
    assert_eq!(verify.status(), StatusCode::OK);
    let cookie = session_cookie(&verify);

    // The manifest edit + rebuild a mode flip is: the same company id, now
    // built with `[users].mode = "none"`. The session store is untouched.
    let none_mode = state_in_mode(dir.path(), AuthMode::None, None).await;
    state
        .registry()
        .insert(CompanyId::new("acme"), none_mode.registry().sole().unwrap());

    // A non-loopback peer refuses the implicit local owner, so this exercises
    // the fallback path — and the old session must not be what it falls
    // through to.
    let mut req = get("/api/v1/company/feedback");
    req.extensions_mut().insert(ConnectInfo(
        "203.0.113.9:1".parse::<std::net::SocketAddr>().unwrap(),
    ));
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    let response = router(state).oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a session minted before the mode flip must not authenticate a none-mode company"
    );
}

/// The owner is one durable record, not a principal invented per request —
/// chat attribution and the task board key off the user id.
#[tokio::test]
async fn the_local_owner_is_the_same_person_on_every_request() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::None, None).await;
    let app = router(state);

    let first = body_json(
        app.clone()
            .oneshot(get("/api/v1/company/auth/me"))
            .await
            .unwrap(),
    )
    .await;
    let second = body_json(app.oneshot(get("/api/v1/company/auth/me")).await.unwrap()).await;
    assert_eq!(first["id"], second["id"]);
    assert!(first["id"].as_str().is_some_and(|id| !id.is_empty()));
}

/// The owner of a company with no sign-in is still a person with a name and a
/// face — and on the desktop they are the *only* person, so if the profile route
/// did not serve `none` mode it would not serve the case it matters most in.
#[tokio::test]
async fn the_local_owner_can_name_themselves_and_pick_a_face() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::None, None).await;
    let app = router(state);

    let request = Request::builder()
        .method("PATCH")
        .uri("/api/v1/company/auth/me")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"displayName": "Steven", "avatar": "tiny:clay"}).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let saved = body_json(response).await;
    assert_eq!(saved["displayName"], "Steven", "{saved}");
    assert_eq!(saved["avatar"], "tiny:clay", "{saved}");

    // The same durable owner record, so the choice survives the next request
    // rather than living on a principal invented per call.
    let reread = body_json(app.oneshot(get("/api/v1/company/auth/me")).await.unwrap()).await;
    assert_eq!(reread["id"], saved["id"], "{reread}");
    assert_eq!(reread["avatar"], "tiny:clay", "{reread}");
}

/// `none` cannot add users. An invite would grant an account nobody could ever
/// reach, because there is no sign-in to reach it through.
#[tokio::test]
async fn none_mode_admits_nobody_else() {
    let dir = home();
    let state = state_in_mode(dir.path(), AuthMode::None, None).await;
    let app = router(state);

    let response = app
        .clone()
        .oneshot(post(
            "/api/v1/company/users/invites",
            serde_json::json!({"email": "ada@example.com", "role": "member"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = body_json(response).await;
    assert_eq!(body["code"], "auth_mode");
    assert_eq!(body["mode"], "none");

    // And no login route to reach such an account through, had one existed.
    for request in [
        post(
            "/api/v1/company/auth/request",
            serde_json::json!({"email": "ada@example.com"}),
        ),
        post(
            "/api/v1/company/auth/wallet/challenge",
            serde_json::json!({"address": address(&wallet(9))}),
        ),
        post("/api/v1/company/auth/logout", serde_json::json!({})),
    ] {
        let uri = request.uri().to_string();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT, "{uri}");
    }
}

/// The host's own answer beats the manifest's. A packaged desktop build and a
/// hosting platform both need to guarantee a mode whatever a company says.
#[tokio::test]
async fn the_host_override_beats_the_manifest() {
    let dir = home();
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[users]\nmode = \"email\"\n").unwrap();
    let runtime = RuntimeBuilder::new(dir.path().to_path_buf(), manifest)
        .with_id(CompanyId::new("acme"))
        .with_auth_mode_override(Some(AuthMode::None))
        .build()
        .await
        .unwrap();
    assert_eq!(runtime.auth_mode(), AuthMode::None);
}
