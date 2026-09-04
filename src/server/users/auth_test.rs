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
use crate::ports::{CompanyStore, SessionKind, SessionRecord, UserRecord, UserRole, UserStatus};
use crate::runtime::RuntimeBuilder;
use crate::server::graphql::auth::{GqlAuth, resolve_principal};
use crate::server::router;
use crate::server::users::cookie::session_cookie_name;
use crate::server::users::token::{OsTokens, mint_session_token, sha256_hex};
use crate::{AppConfig, AppState};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oc-userauth-")
        .tempdir()
        .expect("tempdir")
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
                avatar: None,
                role,
                status,
                password_hash: None,
                must_change_password: false,
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
                user_agent: None,
                kind: SessionKind::Browser,
                label: None,
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
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let acme = CompanyId::new("acme");
    let auth = resolve_principal(
        &headers_with_cookie("acme", &token),
        &state,
        Some(&acme),
        None,
    )
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
}

#[tokio::test]
async fn a_session_for_one_company_is_refused_for_another() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme", "globex"]).await;
    let token = seed_session(&state, "acme", UserRole::Admin, UserStatus::Active).await;

    let globex = CompanyId::new("globex");
    // Presenting acme's cookie while addressing globex: the cookie name does
    // not match globex's, so no user resolves at all.
    assert!(
        resolve_principal(
            &headers_with_cookie("acme", &token),
            &state,
            Some(&globex),
            None
        )
        .await
        .is_err(),
        "acme's session must not authenticate against globex"
    );

    // And even renaming the cookie to globex's does not work: the token hash
    // lives in acme's storage partition, so globex has no such row.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        cookie_header("globex", &token).parse().unwrap(),
    );
    assert!(
        resolve_principal(&headers, &state, Some(&globex), None)
            .await
            .is_err(),
        "a token from another company's partition must not resolve"
    );
}

#[tokio::test]
async fn a_user_may_address_only_their_own_company() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme", "globex"]).await;
    let token = seed_session(&state, "acme", UserRole::Admin, UserStatus::Active).await;

    let acme = CompanyId::new("acme");
    let auth = resolve_principal(
        &headers_with_cookie("acme", &token),
        &state,
        Some(&acme),
        None,
    )
    .await
    .unwrap();

    assert!(auth.authorize(&state, &acme).is_ok());
    assert!(
        auth.authorize(&state, &CompanyId::new("globex")).is_err(),
        "authorize() is the second line of defense and must reject cross-company"
    );
    // A user cannot even learn that other companies exist on this host.
    assert_eq!(auth.visible_companies(&state), vec![acme]);
}

#[tokio::test]
async fn a_suspended_users_live_session_stops_working_immediately() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
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

    assert!(
        resolve_principal(
            &headers_with_cookie("acme", &token),
            &state,
            Some(&acme),
            None
        )
        .await
        .is_err(),
        "suspension must take effect on the next request, not at cookie expiry"
    );
}

#[tokio::test]
async fn an_expired_session_does_not_resolve() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
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
                avatar: None,
                role: UserRole::Member,
                status: UserStatus::Active,
                password_hash: None,
                must_change_password: false,
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
                user_agent: None,
                kind: SessionKind::Browser,
                label: None,
            },
        )
        .await
        .unwrap();

    assert!(
        resolve_principal(
            &headers_with_cookie("acme", &token),
            &state,
            Some(&acme),
            None
        )
        .await
        .is_err(),
        "an expired session must not resolve"
    );
}

#[tokio::test]
async fn a_stale_or_garbage_cookie_falls_through_to_the_platform_bearer() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // Platform mode: the hosting layer's machine credential.
    let state = AppState::new(AppConfig {
        platform_auth: Some(crate::server::platform_auth::PlatformAuthConfig::new(
            Arc::new(crate::server::platform_auth::StaticPlatformVerifier::new(
                "s3cret",
            )),
        )),
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

    // A junk session cookie alongside a valid platform bearer must not fail the
    // request — one stale cookie must not brick the hosting layer on an origin
    // it shares with the console.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        "oc_session_acme=not-a-real-token".parse().unwrap(),
    );
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer s3cret".parse().unwrap(),
    );
    let auth = resolve_principal(&headers, &state, Some(&id), None)
        .await
        .unwrap();
    assert!(
        matches!(auth, GqlAuth::Platform(_)),
        "a bad cookie must degrade to the bearer path, not fail the request"
    );
}

#[tokio::test]
async fn an_anonymous_request_reaches_nothing() {
    // What used to be dev mode. With no principal at all, a write route is
    // simply closed — previously this was a 200 on every deployment, because
    // the operator token that would have guarded it could not be set.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;

    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/companies/acme/tasks")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"anon"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_users_session_now_reaches_their_own_companys_write_plane() {
    // The point of the change: humans are the prosumer auth story, so a member
    // of the company can drive its console surfaces.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/companies/acme/tasks")
                .header("content-type", "application/json")
                .header("cookie", cookie_header("acme", &token))
                .body(Body::from(r#"{"title":"real work"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "a member must be able to use their own company, got {}",
        response.status()
    );
}

#[tokio::test]
async fn a_session_cookie_cannot_reach_the_platform_plane() {
    // THE ESCALATION TEST, now aimed where it still matters. Provisioning and
    // suspension resolve through `resolve_claims`, which cannot produce a User,
    // so no session — however admin — can create or destroy companies across
    // tenants.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Admin, UserStatus::Active).await;

    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/companies")
                .header("content-type", "application/toml")
                .header("cookie", cookie_header("acme", &token))
                .body(Body::from("[company]\nname = \"Pwned\"\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "an admin user's session must not provision companies"
    );

    // And suspension, the other platform-scoped lever.
    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/companies/acme/suspend")
                .header("cookie", cookie_header("acme", &token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "an admin user's session must not suspend a company"
    );
}

#[tokio::test]
async fn a_second_session_cookie_does_not_hide_the_addressed_one() {
    // A browser attaches every cookie for an origin, and cookies ignore port,
    // so one browser holds a session per company ever signed into on that host
    // — including companies this host has never heard of. None of that changes
    // which company the request addresses.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        format!(
            "{}; oc_session_ghost=stale; theme=dark",
            cookie_header("acme", &token)
        )
        .parse()
        .unwrap(),
    );

    // Unaddressed: the registry's sole company is the addressed one.
    match resolve_principal(&headers, &state, None, None)
        .await
        .expect("a decoy cookie must not refuse the request")
    {
        GqlAuth::User(u) => assert_eq!(u.company, CompanyId::new("acme")),
        other => panic!("expected a user, got {other:?}"),
    }

    // Addressed explicitly: same answer, by the same lookup.
    match resolve_principal(&headers, &state, Some(&CompanyId::new("acme")), None)
        .await
        .expect("a decoy cookie must not refuse an addressed request")
    {
        GqlAuth::User(u) => assert_eq!(u.company, CompanyId::new("acme")),
        other => panic!("expected a user, got {other:?}"),
    }
}

#[tokio::test]
async fn an_expired_cookie_beside_a_live_one_still_reads_as_ambiguous() {
    // Both name companies this host serves, so both are candidates. Liveness is
    // not consulted to break the tie: that would mean authenticating every
    // cookie in the jar to decide which one the request meant, and the answer
    // would still be a guess. The refusal is the conservative direction, and
    // the company-scoped routes resolve it — see the test below.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme", "globex"]).await;
    let live = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;
    let expired = seed_session(&state, "globex", UserRole::Member, UserStatus::Active).await;
    let globex = CompanyId::new("globex");
    let runtime = state.registry().get(&globex).unwrap();
    let mut record = runtime
        .sessions()
        .find_by_token_hash(&globex, &sha256_hex(&expired))
        .await
        .unwrap()
        .unwrap();
    runtime
        .sessions()
        .delete(&globex, &record.id)
        .await
        .unwrap();
    record.id = "s1-expired".into();
    record.expires_at_millis = crate::ports::now_millis() - 1;
    runtime.sessions().create(&globex, &record).await.unwrap();

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        format!(
            "{}; {}",
            cookie_header("acme", &live),
            cookie_header("globex", &expired)
        )
        .parse()
        .unwrap(),
    );
    assert!(
        resolve_principal(&headers, &state, None, None)
            .await
            .is_err(),
        "two served companies in the jar stay ambiguous whatever their liveness"
    );
    // Naming the company is what resolves it, and the dead cookie is irrelevant.
    match resolve_principal(&headers, &state, Some(&CompanyId::new("acme")), None)
        .await
        .unwrap()
    {
        GqlAuth::User(u) => assert_eq!(u.company, CompanyId::new("acme")),
        other => panic!("expected a user, got {other:?}"),
    }
    // And the expired one authenticates nobody even when it is addressed.
    assert!(
        resolve_principal(&headers, &state, Some(&globex), None)
            .await
            .is_err(),
        "an expired session must not authenticate its own company either"
    );
}

#[tokio::test]
async fn a_foreign_cookie_does_not_make_one_login_ambiguous() {
    // Several companies served here, the caller signed into one of them, plus a
    // cookie left by a company this host does not serve — a neighbouring host
    // on the same hostname, or one since deleted. Only one candidate is real,
    // so the request is not ambiguous and must still resolve.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme", "globex"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        format!(
            "{}; oc_session_elsewhere=stale",
            cookie_header("acme", &token)
        )
        .parse()
        .unwrap(),
    );
    match resolve_principal(&headers, &state, None, None)
        .await
        .expect("one served company plus a foreign cookie is not ambiguous")
    {
        GqlAuth::User(u) => assert_eq!(u.company, CompanyId::new("acme")),
        other => panic!("expected a user, got {other:?}"),
    }
}

#[tokio::test]
async fn the_addressed_company_picks_its_cookie_out_of_a_shared_jar() {
    // Two real companies on one origin, a live session for each. The address
    // decides which one answers — never the jar's contents or its order.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme", "globex"]).await;
    let acme = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;
    let globex = seed_session(&state, "globex", UserRole::Member, UserStatus::Active).await;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        format!(
            "{}; {}",
            cookie_header("acme", &acme),
            cookie_header("globex", &globex)
        )
        .parse()
        .unwrap(),
    );

    for name in ["acme", "globex"] {
        match resolve_principal(&headers, &state, Some(&CompanyId::new(name)), None)
            .await
            .unwrap()
        {
            GqlAuth::User(u) => assert_eq!(u.company, CompanyId::new(name)),
            other => panic!("expected a user, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_cookie_for_another_company_never_answers_for_the_addressed_one() {
    // The jar holds a perfectly good session — for somewhere else. Reading it
    // would answer from a company the request did not name.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme", "globex"]).await;
    let globex = seed_session(&state, "globex", UserRole::Member, UserStatus::Active).await;

    assert!(
        resolve_principal(
            &headers_with_cookie("globex", &globex),
            &state,
            Some(&CompanyId::new("acme")),
            None,
        )
        .await
        .is_err(),
        "globex's session must not authenticate a request addressing acme"
    );
}

#[tokio::test]
async fn without_an_addressed_company_a_lone_cookie_selects_its_own() {
    // The unaddressed forms — bare `/graphql` and the `/api/v1/company`
    // aliases. The host serves one company, so that is the one addressed.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let auth = resolve_principal(&headers_with_cookie("acme", &token), &state, None, None)
        .await
        .unwrap();
    match auth {
        GqlAuth::User(u) => assert_eq!(u.company, CompanyId::new("acme")),
        other => panic!("expected a user, got {other:?}"),
    }
}

#[tokio::test]
async fn without_an_addressed_company_ambiguous_cookies_resolve_no_user() {
    // Several companies registered, so no unaddressed request can name one.
    // Picking from the jar would be a guess; degrade instead. The console
    // addresses these hosts through the `{id}` routes.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
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
    assert!(
        resolve_principal(&headers, &state, None, None)
            .await
            .is_err(),
        "an ambiguous jar must not silently pick a company"
    );
}

// ---------------------------------------------------------------------------
// The header carrier
//
// A desktop client is cross-site with every server it talks to, and a
// `SameSite=Lax` cookie is never sent cross-site — so the same session has to
// be presentable as a header. These mirror the cookie tests above one for one:
// the carrier changed, so every property the cookie tests pin has to be
// re-pinned rather than assumed to carry over. The dangerous outcome is not the
// header failing, it is the header succeeding somewhere the cookie would not.
// ---------------------------------------------------------------------------

fn session_header_value(company: &str, token: &str) -> String {
    format!("{company}.{token}")
}

fn headers_with_session_header(company: &str, token: &str) -> axum::http::HeaderMap {
    let mut h = axum::http::HeaderMap::new();
    h.insert(
        crate::server::users::cookie::SESSION_HEADER,
        session_header_value(company, token).parse().unwrap(),
    );
    h
}

#[tokio::test]
async fn a_session_header_resolves_to_a_user_of_that_company() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let acme = CompanyId::new("acme");
    let auth = resolve_principal(
        &headers_with_session_header("acme", &token),
        &state,
        Some(&acme),
        None,
    )
    .await
    .unwrap();
    match auth {
        // Same principal as the cookie path, including the token hash logout
        // revokes by: one session, two envelopes.
        GqlAuth::User(user) => {
            assert_eq!(user.company, acme);
            assert_eq!(user.user_id, "u1");
            assert_eq!(user.session_token_hash, sha256_hex(&token));
        }
        other => panic!("expected a user, got {other:?}"),
    }
}

#[tokio::test]
async fn without_an_addressed_company_the_header_names_its_own() {
    // The property the cookie gets from its *name* and the header has to get
    // from its value. Without this the header form would work on the REST
    // routes and silently not on GraphQL, where the company is in the body.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let auth = resolve_principal(
        &headers_with_session_header("acme", &token),
        &state,
        None,
        None,
    )
    .await
    .unwrap();
    match auth {
        GqlAuth::User(u) => assert_eq!(u.company, CompanyId::new("acme")),
        other => panic!("expected a user, got {other:?}"),
    }
}

#[tokio::test]
async fn a_session_header_does_not_work_against_another_company() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme", "globex"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let globex = CompanyId::new("globex");
    // Addressed as globex while naming acme: the carrier is ignored.
    assert!(
        resolve_principal(
            &headers_with_session_header("acme", &token),
            &state,
            Some(&globex),
            None
        )
        .await
        .is_err(),
        "an acme session must not authenticate a globex request"
    );
    // And claiming to be globex does not help: the token is not in globex's
    // storage partition.
    assert!(
        resolve_principal(
            &headers_with_session_header("globex", &token),
            &state,
            Some(&globex),
            None
        )
        .await
        .is_err(),
        "relabelling the company must not move a session between partitions"
    );
}

#[tokio::test]
async fn a_suspended_users_session_header_stops_working_immediately() {
    // The per-request user re-read must apply to this carrier too. If the
    // header path skipped it, suspension would take effect for the console and
    // not for the desktop.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Suspended).await;

    let acme = CompanyId::new("acme");
    assert!(
        resolve_principal(
            &headers_with_session_header("acme", &token),
            &state,
            Some(&acme),
            None
        )
        .await
        .is_err(),
        "a suspended user must not resolve through the header"
    );
}

#[tokio::test]
async fn a_session_header_cannot_reach_the_platform_write_plane() {
    // THE load-bearing property. `resolve_claims` cannot return a human, so
    // provisioning and suspension are unreachable by any human credential. A
    // new carrier is exactly the kind of change that could quietly route around
    // that, so it is asserted against the real router rather than the resolver.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Admin, UserStatus::Active).await;

    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/companies")
                .header("content-type", "application/toml")
                .header(
                    crate::server::users::cookie::SESSION_HEADER,
                    session_header_value("acme", &token),
                )
                .body(Body::from("[company]\nname = \"Pwned\"\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "an admin's session header must not provision companies"
    );

    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/companies/acme/suspend")
                .header(
                    crate::server::users::cookie::SESSION_HEADER,
                    session_header_value("acme", &token),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "an admin's session header must not suspend a company"
    );
}

#[tokio::test]
async fn a_malformed_session_header_resolves_no_user() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme"]).await;
    let token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;
    let acme = CompanyId::new("acme");

    for hostile in [
        // No separator at all: the whole value would otherwise be read as a
        // company with no token, or a token with no company.
        token.clone(),
        "acme".to_string(),
        // Empty on either side of the separator.
        format!(".{token}"),
        "acme.".to_string(),
        ".".to_string(),
        String::new(),
        // A company id that could not name a cookie must not be able to name a
        // header either — otherwise the two carriers disagree about which
        // companies can hold a session at all.
        format!("ac.me.{token}"),
        format!("evil;Path=/.{token}"),
    ] {
        let mut headers = axum::http::HeaderMap::new();
        let Ok(value) = hostile.parse() else {
            continue; // Unrepresentable as a header value; nothing to test.
        };
        headers.insert(crate::server::users::cookie::SESSION_HEADER, value);
        assert!(
            resolve_principal(&headers, &state, Some(&acme), None)
                .await
                .is_err(),
            "{hostile:?} must not authenticate"
        );
    }
}

#[tokio::test]
async fn a_header_naming_another_company_falls_through_to_a_valid_cookie() {
    // Same degrade-rather-than-fail policy the cookie path has: a credential
    // for somewhere else must not brick a request that carried a good one.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with(&home, &["acme", "globex"]).await;
    let acme_token = seed_session(&state, "acme", UserRole::Member, UserStatus::Active).await;

    let acme = CompanyId::new("acme");
    let mut headers = headers_with_session_header("globex", "irrelevant");
    headers.insert(
        axum::http::header::COOKIE,
        cookie_header("acme", &acme_token).parse().unwrap(),
    );

    let auth = resolve_principal(&headers, &state, Some(&acme), None)
        .await
        .unwrap();
    match auth {
        GqlAuth::User(u) => assert_eq!(u.company, acme),
        other => panic!("expected the cookie to still win, got {other:?}"),
    }
}
