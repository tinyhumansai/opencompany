//! Security tests for the user principal.
//!
//! These exist to pin the properties that make session cookies safe to accept
//! at all. Each one is a thing that, if it broke, would be a vulnerability
//! rather than a bug: a user reaching the operator write plane, a session
//! working against the wrong company, a suspended user still being served.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::ports::{CompanyStore, SessionRecord, UserRecord, UserRole, UserStatus};
use crate::runtime::RuntimeBuilder;
use crate::server::graphql::auth::{GqlAuth, resolve_principal};
use crate::server::router;
use crate::server::users::cookie::session_cookie_name;
use crate::server::users::token::{OsTokens, mint_session_token, sha256_hex};
use crate::{AppConfig, AppState};

fn home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("oc-userauth-{}", crate::ports::generate_id()))
}

fn manifest() -> CompanyManifest {
    toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
}

/// Builds state holding the named running companies.
async fn state_with(home: &std::path::Path, companies: &[&str]) -> AppState {
    let store = crate::store::FsCompanyStore::new(home.to_path_buf());
    let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
    for name in companies {
        let id = CompanyId::new(*name);
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
        state.registry().insert(id, Arc::new(runtime));
    }
    state
}

/// Seeds an active user with a live session in `company`, returning the
/// plaintext session token the browser would hold.
async fn seed_session(
    state: &AppState,
    company: &str,
    role: UserRole,
    status: UserStatus,
) -> String {
    let id = CompanyId::new(company);
    let runtime = state.registry().get(&id).unwrap();
    let now = crate::ports::now_millis();
    runtime
        .users()
        .upsert_user(
            &id,
            &UserRecord {
                id: "u1".into(),
                email: "ada@example.com".into(),
                display_name: None,
                role,
                status,
                created_at_millis: now,
                last_seen_at_millis: None,
                updated_at_millis: now,
            },
        )
        .await
        .unwrap();
    let token = mint_session_token(&OsTokens);
    runtime
        .sessions()
        .create(
            &id,
            &SessionRecord {
                id: "s1".into(),
                // Only the hash is stored — the plaintext goes to the browser.
                token_hash: sha256_hex(&token),
                user_id: "u1".into(),
                created_at_millis: now,
                expires_at_millis: now + 60_000,
                last_seen_at_millis: now,
                user_agent: None,
            },
        )
        .await
        .unwrap();
    token
}

fn cookie_header(company: &str, token: &str) -> String {
    format!(
        "{}={token}",
        session_cookie_name(&CompanyId::new(company)).unwrap()
    )
}

fn headers_with_cookie(company: &str, token: &str) -> axum::http::HeaderMap {
    let mut h = axum::http::HeaderMap::new();
    h.insert(
        axum::http::header::COOKIE,
        cookie_header(company, token).parse().unwrap(),
    );
    h
}

#[tokio::test]
async fn a_session_cookie_resolves_to_a_user_of_that_company() {
    let home = home();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let acme = CompanyId::new("acme");
    let auth = resolve_principal(&headers_with_cookie("acme", &token), &state, Some(&acme))
        .await
        .unwrap();
    match auth {
        GqlAuth::User(user) => {
            assert_eq!(user.company, acme);
            assert_eq!(user.user_id, "u1");
            assert_eq!(user.email, "ada@example.com");
            assert_eq!(user.role, UserRole::Member);
        }
        other => panic!("expected a user principal, got {other:?}"),
    }
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn a_session_for_one_company_is_refused_for_another() {
    let home = home();
    let state = state_with(&home, &["acme", "globex"]).await;
    let token = seed_session(&state, "acme", UserRole::Admin, UserStatus::Active).await;

    let globex = CompanyId::new("globex");
    // Presenting acme's cookie while addressing globex: the cookie name does
    // not match globex's, so no user resolves at all.
    let auth = resolve_principal(&headers_with_cookie("acme", &token), &state, Some(&globex))
        .await
        .unwrap();
    assert!(
        !matches!(auth, GqlAuth::User(_)),
        "acme's session must not authenticate against globex"
    );

    // And even renaming the cookie to globex's does not work: the token hash
    // lives in acme's storage partition, so globex has no such row.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        cookie_header("globex", &token).parse().unwrap(),
    );
    let auth = resolve_principal(&headers, &state, Some(&globex))
        .await
        .unwrap();
    assert!(
        !matches!(auth, GqlAuth::User(_)),
        "a token from another company's partition must not resolve"
    );
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn a_user_may_address_only_their_own_company() {
    let home = home();
    let state = state_with(&home, &["acme", "globex"]).await;
    let token = seed_session(&state, "acme", UserRole::Admin, UserStatus::Active).await;

    let acme = CompanyId::new("acme");
    let auth = resolve_principal(&headers_with_cookie("acme", &token), &state, Some(&acme))
        .await
        .unwrap();

    assert!(auth.authorize(&state, &acme).is_ok());
    assert!(
        auth.authorize(&state, &CompanyId::new("globex")).is_err(),
        "authorize() is the second line of defense and must reject cross-company"
    );
    // A user cannot even learn that other companies exist on this host.
    assert_eq!(auth.visible_companies(&state), vec![acme]);
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn a_suspended_users_live_session_stops_working_immediately() {
    let home = home();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;
    let acme = CompanyId::new("acme");

    // Suspend the user, leaving their session row untouched and unexpired.
    let runtime = state.registry().get(&acme).unwrap();
    let mut user = runtime
        .users()
        .get_user(&acme, "u1")
        .await
        .unwrap()
        .unwrap();
    user.status = UserStatus::Suspended;
    runtime.users().upsert_user(&acme, &user).await.unwrap();

    let auth = resolve_principal(&headers_with_cookie("acme", &token), &state, Some(&acme))
        .await
        .unwrap();
    assert!(
        !matches!(auth, GqlAuth::User(_)),
        "suspension must take effect on the next request, not at cookie expiry"
    );
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn an_expired_session_does_not_resolve() {
    let home = home();
    let state = state_with(&home, &["acme"]).await;
    let acme = CompanyId::new("acme");
    let runtime = state.registry().get(&acme).unwrap();
    let now = crate::ports::now_millis();
    runtime
        .users()
        .upsert_user(
            &acme,
            &UserRecord {
                id: "u1".into(),
                email: "ada@example.com".into(),
                display_name: None,
                role: UserRole::Member,
                status: UserStatus::Active,
                created_at_millis: now,
                last_seen_at_millis: None,
                updated_at_millis: now,
            },
        )
        .await
        .unwrap();
    let token = mint_session_token(&OsTokens);
    runtime
        .sessions()
        .create(
            &acme,
            &SessionRecord {
                id: "s1".into(),
                token_hash: sha256_hex(&token),
                user_id: "u1".into(),
                created_at_millis: 0,
                expires_at_millis: now - 1, // already dead
                last_seen_at_millis: 0,
                user_agent: None,
            },
        )
        .await
        .unwrap();

    let auth = resolve_principal(&headers_with_cookie("acme", &token), &state, Some(&acme))
        .await
        .unwrap();
    assert!(!matches!(auth, GqlAuth::User(_)));
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn a_stale_or_garbage_cookie_falls_through_to_the_bearer_path() {
    let home = home();
    // Prosumer mode with a configured operator token, so the bearer path is
    // genuinely guarded rather than open via the dev-mode escape hatch.
    let state = AppState::new(AppConfig {
        operator_token: Some("s3cret".into()),
        ..AppConfig::default()
    })
    .with_home(home.clone());
    let id = CompanyId::new("acme");
    let runtime = RuntimeBuilder::new(home.clone(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    state.registry().insert(id.clone(), Arc::new(runtime));

    // A junk session cookie alongside a valid operator bearer must not 401 the
    // operator — otherwise one stale cookie bricks the console on this origin.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        "oc_session_acme=not-a-real-token".parse().unwrap(),
    );
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer s3cret".parse().unwrap(),
    );
    let auth = resolve_principal(&headers, &state, Some(&id))
        .await
        .unwrap();
    assert!(
        matches!(auth, GqlAuth::Operator),
        "a bad cookie must degrade to the bearer path, not fail the request"
    );
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn a_session_cookie_cannot_reach_an_operator_write_route() {
    // THE ESCALATION TEST. Operator/platform write routes resolve through
    // `resolve_claims`, which cannot produce a User. A session must therefore
    // buy nothing on the write plane, even for an admin user.
    let home = home();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Admin, UserStatus::Active).await;

    // Prosumer mode with a token configured, so the route is genuinely guarded
    // rather than open by the dev-mode escape hatch.
    let guarded = AppState::new(AppConfig {
        operator_token: Some("s3cret".into()),
        ..AppConfig::default()
    })
    .with_home(home.clone());
    let acme = CompanyId::new("acme");
    guarded
        .registry()
        .insert(acme.clone(), state.registry().get(&acme).unwrap());

    let app = router(guarded);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/companies/acme/tasks")
                .header("content-type", "application/json")
                .header("cookie", cookie_header("acme", &token))
                .body(Body::from(r#"{"title":"pwn"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "an admin user's session must not authorize an operator write route"
    );
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn without_an_addressed_company_a_lone_cookie_selects_its_own() {
    // The GraphQL path: the company lives in the request body, so the cookie
    // name is the only signal.
    let home = home();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let auth = resolve_principal(&headers_with_cookie("acme", &token), &state, None)
        .await
        .unwrap();
    match auth {
        GqlAuth::User(u) => assert_eq!(u.company, CompanyId::new("acme")),
        other => panic!("expected a user, got {other:?}"),
    }
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn without_an_addressed_company_ambiguous_cookies_resolve_no_user() {
    // Two companies' sessions in one jar (only reachable in local dev). Picking
    // one would be a guess; degrade instead.
    let home = home();
    let state = state_with(&home, &["acme", "globex"]).await;
    let acme_token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;
    let globex_token = seed_session(&state, "globex", UserRole::Member, UserStatus::Active).await;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        format!(
            "{}; {}",
            cookie_header("acme", &acme_token),
            cookie_header("globex", &globex_token)
        )
        .parse()
        .unwrap(),
    );
    let auth = resolve_principal(&headers, &state, None).await.unwrap();
    assert!(
        !matches!(auth, GqlAuth::User(_)),
        "an ambiguous jar must not silently pick a company"
    );
    tokio::fs::remove_dir_all(&home).await.ok();
}
