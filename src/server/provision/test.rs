//! End-to-end axum tests for provisioning, per-tenant auth, lifecycle controls,
//! quotas, and webhook emission. All offline (default build, no features).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::app::config::AuthMode;
use crate::company::CompanyManifest;
use crate::ports::Brain;
use crate::ports::types::{
    CompanyEvent, CompanyId, CompanyRecord, CompanySummary, CompressedTrace, CycleRequest,
    CycleResult, Effect, EffectGroup, EventSeq, LedgerEntry, OutboundMessage, TokenUsage,
};
use crate::ports::{CompanyStore, CycleHost, EventLog};
use crate::runtime::RuntimeBuilder;
use crate::server::graphql::auth::GqlAuth;
use crate::server::platform_auth::{PlatformAuthConfig, PlatformClaims, UnsignedTenantVerifier};
use crate::server::router;
use crate::server::webhook::{WebhookConfig, WebhookKind};
use crate::store::{FsCompanyStore, FsEventLog};
use crate::{AppConfig, AppState};

const PLATFORM_SECRET: &str = "plat-secret";

const ACME_TOML: &str = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n";

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oc-provision-")
        .tempdir()
        .expect("tempdir")
}

fn platform_state(home: &std::path::Path, max_per_tenant: Option<usize>) -> AppState {
    let verifier = Arc::new(UnsignedTenantVerifier::new(PLATFORM_SECRET));
    AppState::new(AppConfig::default())
        .with_home(home.to_path_buf())
        .with_platform_auth(PlatformAuthConfig::new(verifier))
        .with_quota(None, max_per_tenant)
}

/// A platform state bound to a routable address rather than loopback, for
/// exercising the same none-mode refusal `serve --company` applies at boot.
fn routable_platform_state(home: &std::path::Path) -> AppState {
    let verifier = Arc::new(UnsignedTenantVerifier::new(PLATFORM_SECRET));
    AppState::new(AppConfig {
        bind: "0.0.0.0:8080".to_string(),
        ..AppConfig::default()
    })
    .with_home(home.to_path_buf())
    .with_platform_auth(PlatformAuthConfig::new(verifier))
}

/// A platform state in shared-single-DB mode for the workload tenant
/// `namespace` (its `OPENCOMPANY_TENANT_ID`). The configured namespace — not the
/// request's acting tenant — is authoritative for the id prefix and the
/// ownership record, so ids and owners stay workload-local and survive boot
/// hydration, which filters the `owners` rows by this same value.
fn namespaced_state(home: &std::path::Path, namespace: &str) -> AppState {
    let verifier = Arc::new(UnsignedTenantVerifier::new(PLATFORM_SECRET));
    AppState::new(AppConfig {
        tenant_namespace: Some(namespace.to_string()),
        ..AppConfig::default()
    })
    .with_home(home.to_path_buf())
    .with_platform_auth(PlatformAuthConfig::new(verifier))
}

/// Mints a tenant principal through the `cfg(test)` unsigned codec.
///
/// What these tests are about is what a *verified* tenant token may reach —
/// scopes, the allow-list, cross-tenant ownership — which is independent of how
/// the bearer was authenticated. The codec keeps them running with no signing
/// machinery; a shipped build accepts this shape from nobody.
fn tenant_token(tenant: &str, scopes: &[&str]) -> String {
    UnsignedTenantVerifier::tenant_token(&PlatformClaims {
        tenant: tenant.to_string(),
        scopes: scopes.iter().map(|s| s.to_string()).collect::<HashSet<_>>(),
        companies: None,
    })
}

fn provision_req(token: Option<&str>, toml: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v1/companies")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header("content-type", "text/plain");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(toml.to_string())).unwrap()
}

fn get_req(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

fn post_req(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("POST").uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

fn chat_req(uri: &str, token: Option<&str>, text: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    } else {
        // No explicit credential: sign in as the harness admin, since chat now
        // requires a principal like everything else.
        builder = builder.header("cookie", crate::server::test_support::fixed_cookie("acme"));
    }
    builder
        .body(Body::from(format!(r#"{{"text":"{text}"}}"#)))
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------------------
// Provisioning + status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provision_then_list_then_status() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    // Provision with a platform-scope token.
    let response = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = json_body(response).await;
    assert_eq!(body["id"], "acme");
    assert_eq!(body["lifecycle"], "running");

    // List shows it.
    let list = app
        .clone()
        .oneshot(get_req("/api/v1/companies", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let list_body = json_body(list).await;
    assert_eq!(list_body.as_array().unwrap().len(), 1);

    // Status by id.
    let status = app
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(json_body(status).await["id"], "acme");
}

/// The same refusal boot applies to a `none`-mode company on a routable bind:
/// a company with no sign-in reachable from anywhere is an unauthenticated
/// admin console. A tenant's manifest can request `[users].mode = "none"`, but
/// this host must not silently serve it, regardless of which path created the
/// runtime.
#[tokio::test]
async fn provisioning_a_none_mode_company_on_a_routable_bind_is_refused() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = routable_platform_state(&home);
    let app = router(state);

    let toml = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[users]\nmode = \"none\"\n";
    let response = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), toml))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["code"], "auth_mode_none_not_allowed");

    // Refused, so nothing was registered.
    let response = app
        .clone()
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// The error above tells the caller to fix the manifest (`email` or `wallet`)
/// and retry. Before this durable-store duplicate check existed,
/// `RuntimeBuilder::build` had already saved a `CompanyRecord` for `id` when
/// the refusal fired, so that recovery hit `company_exists` forever — the id
/// was reserved by a provision that never succeeded (issue #1828 comment
/// 3866012835). The auth-mode check now runs before `id` is even resolved, so
/// a rejected `none`-mode request must never reach the store at all, and the
/// exact same id must provision cleanly right after.
#[tokio::test]
async fn retrying_after_a_none_mode_refusal_with_a_valid_mode_succeeds() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = routable_platform_state(&home);
    let app = router(state);

    let rejected =
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[users]\nmode = \"none\"\n";
    let response = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), rejected))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["code"], "auth_mode_none_not_allowed");

    // Same company name, so the same id — corrected to a mode with sign-in.
    let corrected =
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[users]\nmode = \"email\"\n";
    let response = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), corrected))
        .await
        .unwrap();
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "retry with a valid auth mode must provision the id the rejected \
         request never should have reserved, got: {body:?}"
    );
    assert_eq!(body["id"], "acme");
}

/// A manifest built the way the console's create/reset dialog always builds
/// one — `[users].admins` only, never `[users].mode` or `[users].wallets`
/// (`buildManifestToml`, `frontend/src/lib/company-manifest.ts`) — must be
/// refused on a host whose auth override forces `wallet`: the manifest's own
/// admin bootstrap is read in `email` mode only, and unlike `email` there is
/// no deployment-wide `OPENCOMPANY_ADMIN_EMAIL`-style fallback for `wallet`
/// (`manifest_wallets`, `server/users/wallet.rs` — "there is deliberately no
/// environment counterpart"). Provisioning this manifest as-is would create a
/// company nobody, ever, can sign in to.
///
/// `manifest.validate()` alone cannot catch this: the manifest's own
/// `[users].mode` defaults to `email`, which is perfectly self-consistent
/// with a non-empty `admins` list. The mismatch only exists against the
/// host's override, which is why this is checked against
/// `effective_auth_mode` in the handler rather than in `CompanyManifest`
/// itself (issue #1828 comment 3866132491).
#[tokio::test]
async fn provisioning_admins_only_manifest_on_a_wallet_mode_host_is_refused() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    state.set_auth_mode_override(Some(AuthMode::Wallet));
    let app = router(state.clone());

    // Exactly the shape `buildManifestToml` emits: a name and an admin email,
    // no `[users].mode`, no `[users].wallets`.
    let toml = "[company]\nname = \"Acme\"\n[users]\nadmins = [\"ada@example.com\"]\n";
    let response = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), toml))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["code"], "auth_mode_wallet_no_wallets");

    // Refused, so nothing was registered and the id was not reserved.
    let response = app
        .clone()
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// The refusal above must not permanently burn the id the way a post-build
/// refusal would (see `retrying_after_a_none_mode_refusal_with_a_valid_mode_
/// succeeds` for the same property on the `none`-mode check): it runs before
/// `id` is resolved, so a caller who adds a wallet address and retries with
/// the exact same company name must provision cleanly.
#[tokio::test]
async fn retrying_after_a_wallet_mode_refusal_with_a_wallet_listed_succeeds() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    state.set_auth_mode_override(Some(AuthMode::Wallet));
    let app = router(state.clone());

    let rejected = "[company]\nname = \"Acme\"\n[users]\nadmins = [\"ada@example.com\"]\n";
    let response = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), rejected))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["code"], "auth_mode_wallet_no_wallets");

    // Same company name, so the same id — corrected to carry a wallet AND
    // declare `mode = "wallet"`: `manifest.validate()`'s own self-consistency
    // check (`validate_users`) reads `[users].wallets` only when the manifest
    // itself says `mode = "wallet"` — its default is `email` — and refuses a
    // wallets-with-no-mode manifest on that unrelated ground before this
    // request would ever reach the effective-mode check under test. The wallet
    // address itself is built rather than pasted, like `CompanyManifest`'s own
    // `wallet_address()` test helper, so it cannot drift from what the decoder
    // accepts (a base58 32-byte Ed25519 public key).
    let wallet_address = bs58::encode([9u8; 32]).into_string();
    let corrected = format!(
        "[company]\nname = \"Acme\"\n[users]\nmode = \"wallet\"\nwallets = [\"{wallet_address}\"]\n"
    );
    let response = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), &corrected))
        .await
        .unwrap();
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "retry with a wallet listed must provision the id the rejected \
         request never should have reserved, got: {body:?}"
    );
    assert_eq!(body["id"], "acme");
}

/// With no host-wide auth override, the preflight reports `email` — each
/// company's own `[users].mode` decides, and the console builds an `email`
/// manifest by default.
#[tokio::test]
async fn provisioning_info_reports_email_by_default() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    let response = app
        .oneshot(get_req(
            "/api/v1/companies/provisioning",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["auth_mode"], "email");
    assert_eq!(body["wallets_required"], false);
}

/// A host whose override forces `wallet` reports it, so the create/reset dialog
/// collects wallet addresses before it builds a manifest the backend would
/// otherwise refuse with `auth_mode_wallet_no_wallets`.
#[tokio::test]
async fn provisioning_info_reports_wallet_when_overridden() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    state.set_auth_mode_override(Some(AuthMode::Wallet));
    let app = router(state);

    let response = app
        .oneshot(get_req(
            "/api/v1/companies/provisioning",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["auth_mode"], "wallet");
    assert_eq!(body["wallets_required"], true);
}

/// The preflight is a `PlatformScope` route: a session cookie can never reach
/// it (401), and a tenant token without the platform scope is refused (403).
#[tokio::test]
async fn provisioning_info_requires_platform_scope() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    let unauthorized = app
        .clone()
        .oneshot(get_req("/api/v1/companies/provisioning", None))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let token = tenant_token("tenant:acme", &["operator"]);
    let forbidden = app
        .oneshot(get_req("/api/v1/companies/provisioning", Some(&token)))
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
}

/// A wallet-mode host provisions a manifest that lists `[users].wallets` (and
/// declares `mode = "wallet"`, which `manifest.validate()` requires before it
/// reads the wallets) — the positive counterpart to the empty-wallets refusal.
#[tokio::test]
async fn provisioning_a_wallet_manifest_on_a_wallet_mode_host_succeeds() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    state.set_auth_mode_override(Some(AuthMode::Wallet));
    let app = router(state);

    let wallet_address = bs58::encode([7u8; 32]).into_string();
    let toml = format!(
        "[company]\nname = \"Acme\"\n[users]\nmode = \"wallet\"\nwallets = [\"{wallet_address}\"]\n"
    );
    let response = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), &toml))
        .await
        .unwrap();
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(status, StatusCode::CREATED, "got: {body:?}");
    assert_eq!(body["id"], "acme");
    assert_eq!(body["lifecycle"], "running");
}

/// Builds a JSON-envelope provision request naming an explicit id.
fn provision_req_json(token: Option<&str>, toml: &str, id: &str) -> Request<Body> {
    let body = serde_json::json!({ "manifest_toml": toml, "id": id }).to_string();
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v1/companies")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(body)).unwrap()
}

/// An archived company's durable record must block ANY later provision that
/// asks for its id — not just a reset of that same company reusing its own
/// id, but a wholly unrelated company typing the archived id into Advanced.
///
/// Archive removes a company from the live registry, which is all the old
/// duplicate-id check consulted, but never deletes its durable record — and
/// `RuntimeBuilder::build` loads any existing durable record for an id before
/// building over it. So a registry-only check let a second, unrelated
/// "clean" company come back carrying the archived company's old lifecycle,
/// ledger and overlays (issue #1828 comment 3865803905). This proves the
/// server refuses regardless of which company is asking, not just the one
/// that owned the id originally.
#[tokio::test]
async fn archived_company_id_rejected_for_unrelated_provision() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    // Provision and then archive "acme".
    let created = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(json_body(created).await["id"], "acme");

    let archived = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/archive",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(archived.status(), StatusCode::OK);

    // "acme" is gone from the live registry ...
    let missing = app
        .clone()
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    // ... but a totally unrelated new company ("Beta") asking for that same
    // id must still be refused, not silently built over the archived record.
    const BETA_TOML: &str = "[company]\nname = \"Beta\"\n[policy]\nmode = \"full\"\n";
    let collision = app
        .clone()
        .oneshot(provision_req_json(Some(PLATFORM_SECRET), BETA_TOML, "acme"))
        .await
        .unwrap();
    assert_eq!(collision.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(collision).await["code"], "company_exists");

    // And the archived record's own history was not disturbed: a fresh
    // provision of "acme" cleanly denied above means nothing overwrote it, so
    // the id is still not addressable as a live company.
    let still_missing = app
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(still_missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn provision_accepts_json_envelope_with_explicit_id() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    let body = serde_json::json!({ "manifest_toml": ACME_TOML, "id": "custom-id" }).to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/companies")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {PLATFORM_SECRET}"))
        .body(Body::from(body))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(json_body(response).await["id"], "custom-id");
}

#[tokio::test]
async fn provision_requires_platform_scope() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    // No token → 401.
    let unauthorized = app
        .clone()
        .oneshot(provision_req(None, ACME_TOML))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    // Tenant-only token (no platform scope) → 403.
    let token = tenant_token("tenant:acme", &["operator"]);
    let forbidden = app
        .oneshot(provision_req(Some(&token), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(forbidden).await["code"], "forbidden");
}

#[tokio::test]
async fn invalid_manifest_is_400() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    // Empty company name fails validation.
    let bad = "[company]\nname = \"\"\n";
    let response = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), bad))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(response).await["code"], "manifest_invalid");
}

#[tokio::test]
async fn quota_rejects_when_exceeded() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, Some(1));
    let app = router(state);

    let first = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let globex = "[company]\nname = \"Globex\"\n";
    let second = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), globex))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json_body(second).await["code"], "quota_exceeded");
}

#[tokio::test]
async fn duplicate_id_conflicts() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    let first = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let dup = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(dup.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(dup).await["code"], "company_exists");
}

#[tokio::test]
async fn provision_namespaces_id_by_workload_tenant() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // Workload tenant is `tenant-a`.
    let state = namespaced_state(&home, "tenant-a");
    // Keep a handle on the shared ownership map to inspect what boot hydration
    // (which filters `owners` rows by the configured namespace) would reload.
    let observed = state.clone();
    let app = router(state);

    // A *full-platform* token provisions the Acme template. Its acting tenant is
    // `tenant:platform`, not `tenant-a` — yet the id and owner must be keyed to
    // the workload tenant, or the company is orphaned at reboot.
    let response = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    // The derived id `acme` is namespaced with the workload tenant, not the
    // acting `tenant:platform`.
    assert_eq!(json_body(response).await["id"], "tenant-a--acme");
    // The ownership row records the workload tenant — exactly what boot
    // hydration filters on — so the company survives a restart.
    let id = CompanyId::new("tenant-a--acme");
    assert_eq!(observed.owner_of(&id).as_deref(), Some("tenant-a"));
}

#[tokio::test]
async fn same_template_under_two_tenant_workloads_does_not_conflict() {
    // Two tenants are two separate workloads (containers), each with its own
    // `OPENCOMPANY_TENANT_ID`, writing to one shared logical database. In a
    // shared DB the derived id `acme` used to collide; per-workload namespacing
    // keeps them distinct.
    let home_a_dir = home();
    let home_a = home_a_dir.path().to_path_buf();
    let app_a = router(namespaced_state(&home_a, "tenant-a"));
    let a = tenant_token("tenant-a", &["platform", "operator"]);
    let first = app_a
        .oneshot(provision_req(Some(&a), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(json_body(first).await["id"], "tenant-a--acme");

    let home_b_dir = home();
    let home_b = home_b_dir.path().to_path_buf();
    let app_b = router(namespaced_state(&home_b, "tenant-b"));
    let b = tenant_token("tenant-b", &["platform", "operator"]);
    let second = app_b
        .oneshot(provision_req(Some(&b), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);
    assert_eq!(json_body(second).await["id"], "tenant-b--acme");
}

#[tokio::test]
async fn claim_shaped_tenant_manages_namespaced_company() {
    // Shared-single-DB workload for tenant slug `acme` (its bare
    // `OPENCOMPANY_TENANT_ID`). A full-platform token provisions the company; it
    // is namespaced `acme--acme` and its owner is recorded under the bare slug.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = namespaced_state(&home, "acme");
    let observed = state.clone();
    let app = router(state);

    let created = app
        .clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(json_body(created).await["id"], "acme--acme");
    // Ownership is recorded canonically (bare slug), matching the namespace.
    let id = CompanyId::new("acme--acme");
    assert_eq!(observed.owner_of(&id).as_deref(), Some("acme"));

    // The tenant's own token carries the platform-issued *claim* shape
    // `tenant:acme`, which differs textually from the bare `acme` owner. It must
    // still be authorized to address and manage its own company.
    let claim_shaped = tenant_token("tenant:acme", &["operator"]);
    let status = app
        .clone()
        .oneshot(get_req("/api/v1/companies/acme--acme", Some(&claim_shaped)))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(json_body(status).await["id"], "acme--acme");

    let paused = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme--acme/pause",
            Some(&claim_shaped),
        ))
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    assert_eq!(json_body(paused).await["lifecycle"], "paused");

    // A different tenant — whatever its representation — is still denied.
    let intruder = tenant_token("tenant:globex", &["operator"]);
    let denied = app
        .oneshot(get_req("/api/v1/companies/acme--acme", Some(&intruder)))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pause_toggles_and_chat_409() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // Pause → paused.
    let paused = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/pause",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    assert_eq!(json_body(paused).await["lifecycle"], "paused");

    // Chat is 409 while paused.
    let conflict = app
        .clone()
        .oneshot(chat_req(
            "/api/v1/companies/acme/chat",
            Some(PLATFORM_SECRET),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);

    // Resume → running, chat 200.
    let resumed = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/resume",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    assert_eq!(json_body(resumed).await["lifecycle"], "running");

    let ok = app
        .oneshot(chat_req(
            "/api/v1/companies/acme/chat",
            Some(PLATFORM_SECRET),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Emergency stop (issue #86)
// ---------------------------------------------------------------------------

/// A `POST` carrying a JSON body, for the step-up-confirmed emergency routes.
fn json_post_req(uri: &str, token: Option<&str>, body: serde_json::Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

/// The happy path end to end: stop, observe it in `status`, release it.
///
/// `ACME_TOML` sets `mode = "full"`, so this also pins that the stop overrides
/// the most permissive policy the manifest can ask for.
#[tokio::test]
async fn emergency_pause_shows_in_status_and_resume_clears_it() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    let paused = app
        .clone()
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-pause",
            Some(PLATFORM_SECRET),
            serde_json::json!({ "confirm": "EMERGENCY-PAUSE", "reason": "runaway loop" }),
        ))
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    let body = json_body(paused).await;
    assert_eq!(body["emergency_paused"], true);
    assert_eq!(body["changed"], true);
    // Orthogonal to lifecycle: the company is still running, so chat still works.
    assert_eq!(body["lifecycle"], "running");

    let ok = app
        .clone()
        .oneshot(chat_req(
            "/api/v1/companies/acme/chat",
            Some(PLATFORM_SECRET),
            "what are you doing?",
        ))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);

    // Release requires the company id, not the fixed phrase.
    let resumed = app
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-resume",
            Some(PLATFORM_SECRET),
            serde_json::json!({ "confirm": "acme" }),
        ))
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    let body = json_body(resumed).await;
    assert_eq!(body["emergency_paused"], false);
    assert_eq!(body["changed"], true);
}

/// The failure path that matters most: a request with no confirmation, or the
/// wrong one, must not move the switch.
#[tokio::test]
async fn emergency_routes_refuse_without_the_right_confirmation() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // Empty body → 400, and nothing changed.
    let bare = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/emergency-pause",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(bare.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(bare).await["code"], "confirmation_required");

    // A declared JSON body that is empty (or malformed) must reach the handler
    // and read as "no step-up supplied" — the same envelope, not an opaque
    // `Json` rejection. (With `Option<Json<_>>` this request would have been
    // rejected by the extractor before the handler got to answer; the
    // error-aware arm keeps the panic button able to say *what* to send.)
    let empty_json = Request::builder()
        .method("POST")
        .uri("/api/v1/companies/acme/emergency-pause")
        .header("authorization", format!("Bearer {PLATFORM_SECRET}"))
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();
    let empty_json = app.clone().oneshot(empty_json).await.unwrap();
    assert_eq!(empty_json.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(empty_json).await["code"], "confirmation_required");

    // Wrong phrase → 400.
    let wrong = app
        .clone()
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-pause",
            Some(PLATFORM_SECRET),
            serde_json::json!({ "confirm": "emergency pause please" }),
        ))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);

    // The company is still running normally.
    let status = app
        .clone()
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(json_body(status).await["emergency_paused"], false);

    // Engage it, then try to release with the *pause* phrase rather than the id.
    app.clone()
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-pause",
            Some(PLATFORM_SECRET),
            serde_json::json!({ "confirm": "EMERGENCY-PAUSE" }),
        ))
        .await
        .unwrap();

    let wrong_resume = app
        .clone()
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-resume",
            Some(PLATFORM_SECRET),
            serde_json::json!({ "confirm": "EMERGENCY-PAUSE" }),
        ))
        .await
        .unwrap();
    assert_eq!(wrong_resume.status(), StatusCode::BAD_REQUEST);

    // Still stopped — a failed release must never be a release.
    let status = app
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(json_body(status).await["emergency_paused"], true);
}

/// Pressing the panic button twice is not an error, and the second press
/// reports that it changed nothing.
#[tokio::test]
async fn emergency_pause_is_idempotent() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    let body = serde_json::json!({ "confirm": "EMERGENCY-PAUSE" });
    let first = app
        .clone()
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-pause",
            Some(PLATFORM_SECRET),
            body.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(json_body(first).await["changed"], true);

    let second = app
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-pause",
            Some(PLATFORM_SECRET),
            body,
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let body = json_body(second).await;
    assert_eq!(body["changed"], false);
    assert_eq!(body["emergency_paused"], true);
}

/// The mirror idempotency case: releasing a company that is not stopped is not
/// an error, and reports that it changed nothing. The early return exists so a
/// stray release cannot journal a spurious `engaged: false` event against a
/// company that never stopped — the exact failure the engage-side guard guards
/// in reverse.
#[tokio::test]
async fn emergency_resume_when_not_stopped_is_idempotent() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // The correct confirmation (the company id) on a company that never stopped.
    let release = app
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-resume",
            Some(PLATFORM_SECRET),
            serde_json::json!({ "confirm": "acme" }),
        ))
        .await
        .unwrap();
    assert_eq!(release.status(), StatusCode::OK);
    let body = json_body(release).await;
    assert_eq!(body["changed"], false);
    assert_eq!(body["emergency_paused"], false);
}

/// Unauthenticated callers cannot reach either route — checked before the
/// confirmation, so a correct phrase is never a substitute for a credential.
#[tokio::test]
async fn emergency_routes_require_auth() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    let anon = app
        .clone()
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-pause",
            None,
            serde_json::json!({ "confirm": "EMERGENCY-PAUSE" }),
        ))
        .await
        .unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);

    let anon_resume = app
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-resume",
            None,
            serde_json::json!({ "confirm": "acme" }),
        ))
        .await
        .unwrap();
    assert_eq!(anon_resume.status(), StatusCode::UNAUTHORIZED);
}

/// The kill switch is journaled with the acting operator, both directions.
#[tokio::test]
async fn emergency_transitions_are_journaled_with_the_actor() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state.clone());

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    app.clone()
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-pause",
            Some(PLATFORM_SECRET),
            serde_json::json!({ "confirm": "EMERGENCY-PAUSE", "reason": "burning budget" }),
        ))
        .await
        .unwrap();
    app.oneshot(json_post_req(
        "/api/v1/companies/acme/emergency-resume",
        Some(PLATFORM_SECRET),
        serde_json::json!({ "confirm": "acme" }),
    ))
    .await
    .unwrap();

    let runtime = state
        .registry()
        .get(&CompanyId::new("acme"))
        .expect("company registered");
    let events = runtime
        .events()
        .read_from(runtime.id(), EventSeq::new(0), 1000)
        .await
        .unwrap();
    let changes: Vec<_> = events
        .iter()
        .filter_map(|stored| match &stored.event {
            CompanyEvent::EmergencyPauseChanged {
                engaged,
                by,
                reason,
            } => Some((*engaged, by.clone(), reason.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(changes.len(), 2, "expected an engage and a release");
    assert!(changes[0].0, "first event should be the engage");
    assert_eq!(changes[0].2.as_deref(), Some("burning budget"));
    assert!(!changes[1].0, "second event should be the release");
    // Both carry an identified actor rather than an anonymous one.
    assert!(!changes[0].1.id.is_empty());
    assert!(!changes[1].1.id.is_empty());
}

#[tokio::test]
async fn emergency_stop_survives_a_cold_boot_and_release_does_not_stick() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // Engage the stop over the route.
    let paused = app
        .clone()
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-pause",
            Some(PLATFORM_SECRET),
            serde_json::json!({ "confirm": "EMERGENCY-PAUSE", "reason": "pre-restart" }),
        ))
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);

    // A fresh boot on the same home — a second CompanyRuntime with no handover,
    // so the flag must come from the journal, not from live memory — comes up
    // stopped.
    let manifest: CompanyManifest = toml::from_str(ACME_TOML).unwrap();
    let rebooted = RuntimeBuilder::new(home.clone(), manifest.clone())
        .with_id(CompanyId::new("acme"))
        .build()
        .await
        .unwrap();
    assert!(
        rebooted.is_emergency_paused(),
        "a company stopped before a restart must boot stopped"
    );

    // Release the stop on the live runtime, then boot cold once more: the
    // switch must not be sticky.
    let resumed = app
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-resume",
            Some(PLATFORM_SECRET),
            serde_json::json!({ "confirm": "acme" }),
        ))
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);

    let released = RuntimeBuilder::new(home, manifest)
        .with_id(CompanyId::new("acme"))
        .build()
        .await
        .unwrap();
    assert!(
        !released.is_emergency_paused(),
        "a company released before a restart must boot running"
    );
}

#[tokio::test]
async fn suspend_requires_platform_scope_and_blocks_chat() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // A tenant-only token cannot suspend.
    let tenant = tenant_token("tenant:platform", &["operator"]);
    let forbidden = app
        .clone()
        .oneshot(post_req("/api/v1/companies/acme/suspend", Some(&tenant)))
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    // Platform scope suspends.
    let suspended = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/suspend",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(suspended.status(), StatusCode::OK);
    assert_eq!(json_body(suspended).await["lifecycle"], "suspended");

    // Chat is blocked.
    let conflict = app
        .oneshot(chat_req(
            "/api/v1/companies/acme/chat",
            Some(PLATFORM_SECRET),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn foreign_tenant_cannot_file_feedback() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    // acme is owned by tenant:platform.
    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // A different tenant's token must not reach acme's feedback route.
    let other = tenant_token("tenant:other", &["operator"]);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/companies/acme/feedback")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {other}"))
        .body(Body::from(r#"{"category":"bug","note":"not yours"}"#))
        .unwrap();
    let denied = app.oneshot(req).await.unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn owner_cannot_resume_a_platform_suspension() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    // Platform suspends the tenant.
    let suspended = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/suspend",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(suspended.status(), StatusCode::OK);

    // The owner's tenant token must NOT be able to lift the suspension.
    let tenant = tenant_token("tenant:platform", &["operator"]);
    let denied = app
        .clone()
        .oneshot(post_req("/api/v1/companies/acme/resume", Some(&tenant)))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    // Platform scope can lift it.
    let resumed = app
        .oneshot(post_req(
            "/api/v1/companies/acme/resume",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    assert_eq!(json_body(resumed).await["lifecycle"], "running");
}

#[tokio::test]
async fn archive_removes_from_registry() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();

    let archived = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/archive",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(archived.status(), StatusCode::OK);
    assert_eq!(json_body(archived).await["lifecycle"], "archived");

    // Now unaddressable: status 404, chat 404.
    let status = app
        .clone()
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::NOT_FOUND);

    let chat = app
        .oneshot(chat_req(
            "/api/v1/companies/acme/chat",
            Some(PLATFORM_SECRET),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cross_tenant_access_forbidden() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    // Tenant B provisions (its token carries the platform scope).
    let b_platform = tenant_token("tenant:b", &["platform", "operator"]);
    let created = app
        .clone()
        .oneshot(provision_req(Some(&b_platform), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    // Tenant A (no platform scope, different tenant) cannot address it.
    let a_token = tenant_token("tenant:a", &["operator"]);
    let forbidden = app
        .oneshot(get_req("/api/v1/companies/acme", Some(&a_token)))
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn lifecycle_transition_recorded_as_event() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = platform_state(&home, None);
    let app = router(state);

    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    app.oneshot(post_req(
        "/api/v1/companies/acme/pause",
        Some(PLATFORM_SECRET),
    ))
    .await
    .unwrap();

    // The audit trail carries a LifecycleChanged running -> paused.
    let events = FsEventLog::new(home.clone());
    let stored = events
        .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    let found = stored.iter().any(|e| {
        matches!(
            &e.event,
            CompanyEvent::LifecycleChanged { from, to, .. } if from == "running" && to == "paused"
        )
    });
    assert!(found, "expected a LifecycleChanged event, got {stored:?}");
}

// ---------------------------------------------------------------------------
// Webhooks
// ---------------------------------------------------------------------------

/// A brain that emits one supervised effect per operator message (parks under a
/// explicit request), so a cycle produces an `approval.requested` webhook.
struct EffectBrain {
    effect: Effect,
}

#[async_trait]
impl Brain for EffectBrain {
    async fn run_cycle(
        &self,
        req: CycleRequest,
        host: &dyn CycleHost,
    ) -> crate::Result<CycleResult> {
        let mut responses = Vec::new();
        for event in &req.events {
            if let CompanyEvent::OperatorMessage { text, .. } = event {
                host.park_effect(self.effect.clone()).await?;
                responses.push(OutboundMessage {
                    message_id: None,
                    task_id: None,
                    channel: "operator".into(),
                    agent: None,
                    text: format!("handled: {text}"),
                    steps: Vec::new(),
                    reply_to: None,
                    mentions: Vec::new(),
                });
            }
        }
        Ok(CycleResult {
            channel_responses: responses,
            new_traces: vec![CompressedTrace::now(&req.cycle_id, "effect cycle")],
            ledger_deltas: Vec::new(),
            token_usage: TokenUsage::default(),
        })
    }
}

#[tokio::test]
async fn webhook_emitted_on_approval_requested() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // Prosumer mode (no platform_auth) plus a recording webhook sink.
    let (webhook, sink) = WebhookConfig::recording("tenant-secret");
    let state = AppState::new(AppConfig::default())
        .with_home(home.clone())
        .with_webhook(webhook);

    // A company whose agent explicitly asks the operator for approval.
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"supervised\"\n").unwrap();
    let sign_effect = Effect {
        kind: crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND.into(),
        group: EffectGroup::Other,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: false,
        payload: serde_json::json!({
            "title": "Submit the filing",
            "question": "May I submit it?"
        }),
        agent: Some("ceo".into()),
        run_id: None,
    };
    let runtime = RuntimeBuilder::new(home.clone(), manifest)
        .with_id(CompanyId::new("acme"))
        .with_brain(Arc::new(EffectBrain {
            effect: sign_effect,
        }))
        .build()
        .await
        .unwrap();
    state
        .registry()
        .insert(CompanyId::new("acme"), Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    let app = router(state);
    let chat = app
        .oneshot(chat_req("/api/v1/companies/acme/chat", None, "file it"))
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::OK);

    let delivered = sink.delivered();
    let approval = delivered
        .iter()
        .find(|(event, _)| event.kind == WebhookKind::ApprovalRequested)
        .expect("an approval_requested webhook was delivered");
    // The delivery carries a non-empty signature header value.
    assert!(!approval.1.is_empty());
    assert!(approval.1.starts_with("kh1="));
}

// ---------------------------------------------------------------------------
// Issue #605 — the tier a provisioned company is recorded on
// ---------------------------------------------------------------------------

/// The tier `id` was persisted with, read back off the stored record rather
/// than off the response.
///
/// The record is what matters here: it is the manifest a rebuild re-reads and
/// the only place a platform-provisioned tenant's tier is written down at all,
/// since it has no `company.toml` anywhere on disk.
async fn recorded_mode(state: &AppState, id: &str) -> String {
    let id = CompanyId::new(id);
    let runtime = state.registry().get(&id).expect("company is registered");
    runtime
        .store()
        .load(&id)
        .await
        .expect("store readable")
        .expect("record exists")
        .manifest
        .policy
        .mode
}

/// Issue #605: a company provisioned from a manifest that names no tier is
/// recorded on `auto`, explicitly.
///
/// This is the one creation path with no template behind it — `serve` and the
/// desktop app both read a `companies/*/company.toml`, and every shipped preset
/// declares `mode`. So this is where the "new companies get `auto`" half of
/// #605 is actually delivered.
///
/// Asserting `auto` is also what pins the change as *doing something*: the serde
/// default is still `supervised`, deliberately (see `Policy::mode`), so a
/// regression that dropped the provisioning write would record `supervised`
/// here and fail rather than quietly reverting the feature.
#[tokio::test]
async fn a_provisioned_company_with_no_stated_tier_is_recorded_on_auto() {
    let home_dir = home();
    let state = platform_state(home_dir.path(), None);
    let app = router(state.clone());

    let response = app
        .oneshot(provision_req(
            Some(PLATFORM_SECRET),
            "[company]\nname = \"Acme\"\n",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    assert_eq!(
        recorded_mode(&state, "acme").await,
        crate::company::PROVISIONED_POLICY_MODE,
        "a manifest that states no tier must be recorded on the provisioning \
         default, explicitly — not left to the serde default"
    );
    assert_ne!(
        crate::company::PROVISIONED_POLICY_MODE,
        crate::company::Policy::default().mode,
        "if these ever coincide this test proves nothing — it would pass with \
         the provisioning write deleted"
    );
}

/// ...and a manifest that *does* state a tier keeps it, whichever tier it is.
///
/// **Preserve, never widen**, which is the property the whole of #605 turns on.
/// Walked over `POLICY_MODES` rather than spot-checked, so a fifth tier cannot
/// silently escape the guarantee the way `auto` escaped the prose tier lists in
/// #660: the day someone adds one, this covers it without being edited.
///
/// `supervised` is the sharp case and the reason this is a walk and not a single
/// `readonly` assertion — it is the value the serde default *also* produces, so
/// a broken "did the author declare a mode?" check is invisible on every other
/// tier and caught only here.
#[tokio::test]
async fn a_provisioned_company_keeps_whatever_tier_it_states() {
    let home_dir = home();
    let state = platform_state(home_dir.path(), None);
    let app = router(state.clone());

    let mut checked = 0;
    for mode in crate::company::POLICY_MODES {
        let name = format!("Acme {mode}");
        let manifest = format!("[company]\nname = \"{name}\"\n[policy]\nmode = \"{mode}\"\n");
        let response = app
            .clone()
            .oneshot(provision_req(Some(PLATFORM_SECRET), &manifest))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "provisioning `{mode}` failed"
        );

        let id = json_body(response).await["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            recorded_mode(&state, &id).await,
            *mode,
            "`{mode}` was stated in the manifest and must survive provisioning \
             untouched"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        crate::company::POLICY_MODES.len(),
        "the walk skipped a tier"
    );
}

// ---------------------------------------------------------------------------
// Host-wide auth mode override
// ---------------------------------------------------------------------------

/// A host-wide sign-in override set before provisioning (by setup, or flipped
/// live afterward) must reach a company provisioned *after* the change, the
/// same way it reaches every company built at boot — see
/// `AppState::auth_mode_override`. Provisioning built the runtime without
/// threading it through, so an operator who locked the host to `email` after
/// setup still got a provisioned tenant honoring its own manifest mode.
#[tokio::test]
async fn a_host_wide_auth_override_reaches_a_company_provisioned_after_it_is_set() {
    let home_dir = home();
    let state = platform_state(home_dir.path(), None);
    state.set_auth_mode_override(Some(AuthMode::Email));
    let app = router(state.clone());

    let manifest = "[company]\nname = \"Acme\"\n[users]\nmode = \"wallet\"\n";
    let response = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), manifest))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).expect("company is registered");
    assert_eq!(
        runtime.auth_mode(),
        AuthMode::Email,
        "the host-wide override set before provisioning must beat the \
         manifest's own mode, exactly as it does for a company built at boot"
    );
}

/// A request must validate against, and build with, ONE snapshot of the
/// host-wide auth override — not two independent reads of the shared
/// `RwLock` straddling this request's own `.await`s.
///
/// `provision` used to call `state.auth_mode_override()` twice: once to
/// resolve `effective_auth_mode` for the wallet-no-wallets preflight check,
/// and again, later, to build `RuntimeBuilder::with_auth_mode_override`. A
/// concurrent `setup.rs` request can flip the override at any point via
/// `AppState::set_auth_mode_override` — including during the first read's
/// duplicate-id `company_store.load` await, which is exactly where this test
/// flips it. Before the fix, the preflight check validated an admins-only
/// manifest against the override's ORIGINAL value (`email`, which the
/// manifest satisfies), but the builder then read the override's NEW value
/// (`wallet`) and built a runtime in wallet mode with an empty
/// `[users].wallets` — a company nobody could sign into, and on a reset, the
/// only copy left once the old one was archived (issue #1828 comment
/// 3873451846). The fix reads the override once and reuses that snapshot for
/// both, so the built runtime can never disagree with what was validated.
#[tokio::test]
async fn a_concurrent_override_flip_mid_request_cannot_desync_the_check_from_the_build() {
    let home_dir = home();
    let state = platform_state(home_dir.path(), None);
    state.set_auth_mode_override(Some(AuthMode::Email));
    let app = router(state.clone());

    // Shaped exactly like `buildManifestToml` (frontend/src/lib/company-manifest.ts):
    // no `[users].mode` (defaults to `email`), admins only, no wallets.
    let manifest = "[company]\nname = \"Acme\"\n\n[users]\nadmins = [\"admin@example.com\"]\n";

    // Flips the override to `wallet` in a tight loop for the lifetime of the
    // request. The request's only `.await` before the builder reads the
    // override is the duplicate-id `company_store.load` — a real
    // `tokio::fs::read_to_string` that suspends the request task on the
    // blocking pool, giving this loop many scheduler turns to land the flip
    // inside that window before the request resumes.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flip_state = state.clone();
    let flip_stop = stop.clone();
    let flipper = tokio::spawn(async move {
        while !flip_stop.load(std::sync::atomic::Ordering::Relaxed) {
            flip_state.set_auth_mode_override(Some(AuthMode::Wallet));
            tokio::task::yield_now().await;
        }
    });

    let response = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), manifest))
        .await
        .unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    flipper.await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "the manifest was valid against the mode actually checked (`email`) and must provision \
         — a build that silently switched modes underneath a passed check must not surface as \
         a rejection either, it must surface as the desync this test is about"
    );
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).expect("company is registered");
    assert_eq!(
        runtime.auth_mode(),
        AuthMode::Email,
        "the auth mode the runtime was actually built with must match the one the \
         wallet-no-wallets preflight check validated against, regardless of how the shared \
         override changed mid-request — two reads of the same `RwLock` must not be able to \
         disagree with each other"
    );
}

// ── issue #1050: the durable ownership write ────────────────────────────────

/// An [`OwnershipStore`](crate::store::select::OwnershipStore) that fails its
/// first `fail_first` `set_owner` calls, then succeeds — the transient blip
/// (mongo election, timeout) issue #1050 names as the cause.
struct FlakyOwnership {
    fail_first: std::sync::Mutex<usize>,
    attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FlakyOwnership {
    fn new(fail_first: usize) -> (Self, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Self {
                fail_first: std::sync::Mutex::new(fail_first),
                attempts: attempts.clone(),
            },
            attempts,
        )
    }
}

#[async_trait::async_trait]
impl crate::store::select::OwnershipStore for FlakyOwnership {
    async fn set_owner(&self, _id: &CompanyId, _tenant: &str) -> crate::Result<()> {
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut left = self.fail_first.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return Err(crate::error::OpenCompanyError::Config(
                "transient ownership write failure".into(),
            ));
        }
        Ok(())
    }
    async fn remove_owner(&self, _id: &CompanyId) -> crate::Result<()> {
        Ok(())
    }
    async fn owners(&self) -> crate::Result<Vec<(CompanyId, String)>> {
        Ok(Vec::new())
    }
}

/// A transient failure is retried and the write succeeds, so a mongo blip does
/// not turn into a refused provision.
#[tokio::test]
async fn a_transient_ownership_failure_is_retried_and_succeeds() {
    let (store, attempts) = FlakyOwnership::new(2);
    let result = super::persist_owner_with_retry(&store, &CompanyId::new("acme"), "tenant-a").await;
    assert!(result.is_ok(), "the third attempt succeeds: {result:?}");
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "it retried rather than giving up on the first failure"
    );
}

/// A backend that is genuinely down returns the error, which is what the route
/// turns into a refusal. The bound matters: this must not retry forever with a
/// caller waiting on the request.
#[tokio::test]
async fn a_persistent_ownership_failure_gives_up_and_reports_it() {
    let (store, attempts) = FlakyOwnership::new(usize::MAX);
    let result = super::persist_owner_with_retry(&store, &CompanyId::new("acme"), "tenant-a").await;
    assert!(
        result.is_err(),
        "a write that never succeeds must be reported, not swallowed — swallowing it is \
         issue #1050"
    );
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        super::OWNERSHIP_WRITE_ATTEMPTS,
        "bounded: a caller is waiting on this request"
    );
}

/// The happy path costs exactly one write — the retry must not multiply the
/// normal case.
#[tokio::test]
async fn a_successful_ownership_write_is_attempted_once() {
    let (store, attempts) = FlakyOwnership::new(0);
    super::persist_owner_with_retry(&store, &CompanyId::new("acme"), "tenant-a")
        .await
        .expect("writes first time");
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// ── issue #1828 comment 3866132497: register before the status() read ──────

/// A `CompanyStore` that fails its `fail_on`-th `load` call (1-indexed) and
/// otherwise delegates to `inner` for everything, including every `save`.
/// Models a transient read blip (a mongo hiccup) landing right after the
/// write it would be reading back already succeeded.
struct FlakyLoadStore {
    inner: Arc<dyn CompanyStore>,
    fail_on: usize,
    load_calls: std::sync::atomic::AtomicUsize,
}

impl FlakyLoadStore {
    fn new(inner: Arc<dyn CompanyStore>, fail_on: usize) -> Self {
        Self {
            inner,
            fail_on,
            load_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl CompanyStore for FlakyLoadStore {
    async fn load(&self, id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
        let n = self
            .load_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if n == self.fail_on {
            return Err(crate::error::OpenCompanyError::Config(
                "transient store read failure".into(),
            ));
        }
        self.inner.load(id).await
    }
    async fn save(&self, record: &CompanyRecord) -> crate::Result<()> {
        self.inner.save(record).await
    }
    async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
        self.inner.list().await
    }
    async fn append_ledger(&self, id: &CompanyId, entry: LedgerEntry) -> crate::Result<()> {
        self.inner.append_ledger(id, entry).await
    }
}

/// Builds a runtime the same way `provision`'s `builder.build()` does, over a
/// store whose SECOND `load` call fails. The first `load` is `build()`'s own
/// "is this a rebuild" check (empty registry here, so it finds nothing and
/// proceeds as a fresh boot); the second is whichever caller reads next —
/// in production that is `register_and_report_status`'s `runtime.status()`.
/// `build()` itself must still succeed: its own durable `store.save` is
/// unaffected, so by the time this returns the `CompanyRecord` is already on
/// disk regardless of what a later read does.
async fn build_runtime_with_status_read_failing(
    home: &std::path::Path,
    id: &CompanyId,
) -> crate::runtime::CompanyRuntime {
    let inner = Arc::new(FsCompanyStore::new(home.to_path_buf()));
    let store = Arc::new(FlakyLoadStore::new(inner, 2));
    let manifest: CompanyManifest = toml::from_str(ACME_TOML).unwrap();
    RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .with_store(store)
        .build()
        .await
        .expect("build succeeds — its own save is unaffected by a later load failing")
}

/// The property this whole helper exists for: a `status()` failure right
/// after a successful build must not leave the company unregistered.
///
/// Before the fix, `provision` read `status()` BEFORE calling
/// `state.registry().insert(...)` — so on this exact failure, the runtime a
/// successful `build()` had just constructed was discarded, never reaching
/// the registry. The company's `CompanyRecord` was already durably saved
/// (`build()`'s own `store.save`, unaffected by a later `load` failing), so a
/// retry's duplicate-id check (`company_store.load(&id)` in `provision.rs`)
/// would find that record and refuse with `company_exists` — forever, for an
/// id nothing had ever registered and no request could ever create or reach
/// again (issue #1828 comment 3866132497). This test calls the extracted
/// `register_and_report_status` directly — the exact function `provision`
/// calls — so it exercises the real ordering, not a re-implementation of it.
#[tokio::test]
async fn register_and_report_status_registers_even_when_the_status_read_fails() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");
    let runtime = build_runtime_with_status_read_failing(&home, &id).await;

    let state = platform_state(&home, None);
    let result = super::register_and_report_status(&state, &id, "tenant-a", runtime).await;

    assert!(
        result.is_err(),
        "the status() read was made to fail — the function must report that, not paper over it"
    );
    assert!(
        state.registry().get(&id).is_some(),
        "the company was fully built (its record is durably saved) before the failing \
         status() read ran, so it must already be registered and addressable — a status() \
         failure is a response-body problem, not proof the company doesn't exist"
    );
}

/// The direct, HTTP-level consequence of the property above: a caller that
/// gets an error back from a status()-read failure right after a successful
/// build can still address the company through the ordinary status route —
/// it is not the permanent, unrecoverable lockout a pre-fix retry would hit.
#[tokio::test]
async fn a_company_registered_despite_a_failed_status_read_is_addressable() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");
    let runtime = build_runtime_with_status_read_failing(&home, &id).await;

    let state = platform_state(&home, None);
    let result = super::register_and_report_status(&state, &id, "tenant-a", runtime).await;
    assert!(result.is_err());

    let app = router(state);
    let response = app
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "registered despite the failed status() read during provisioning, so a plain \
         status lookup afterward must succeed"
    );
}

// ── issue #1828 comment 3875046440: finish archive cleanup even when the ──
// ── post-write status() read fails ─────────────────────────────────────

/// A runtime that is already registered (as if provisioned normally), then
/// rebuilt over the SAME durable record with a store whose THIRD `load` call
/// fails. The first `load` is `build()`'s own rebuild check (finds the
/// already-provisioned record and inherits it); the second is inside
/// `set_lifecycle` (`CompanyRuntime::set_lifecycle`, `src/company/runtime.rs`)
/// reading the current record before it flips `lifecycle` to `"archived"` and
/// saves it; the third is `transition`'s own post-`set_lifecycle`
/// `runtime.status()` read. `set_lifecycle`'s `store.save` — like `build()`'s
/// — is unaffected by a later `load` failing, so by the time this third read
/// fails, `lifecycle: "archived"` is already durably on disk.
async fn build_runtime_with_archive_status_read_failing(
    home: &std::path::Path,
    id: &CompanyId,
) -> crate::runtime::CompanyRuntime {
    let inner = Arc::new(FsCompanyStore::new(home.to_path_buf()));
    let store = Arc::new(FlakyLoadStore::new(inner, 3));
    let manifest: CompanyManifest = toml::from_str(ACME_TOML).unwrap();
    RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .with_store(store)
        .build()
        .await
        .expect("build succeeds — its own save is unaffected by a later load failing")
}

/// `archive`'s registry/owner cleanup (`src/server/provision.rs`) is gated on
/// `transition`'s whole response being `StatusCode::OK`. `set_lifecycle`
/// persists `lifecycle: "archived"` to the store BEFORE it appends the
/// `LifecycleChanged` audit event, and `transition` then re-reads `status()`
/// after that — so a failure in either the event append or that re-read
/// surfaces as a non-200 response even though the archive genuinely landed.
/// Before the fix, that left the runtime (and its owner record) still
/// registered: on a host at its per-tenant or global company quota
/// (`provision.rs` quota checks) or one that just re-lists companies, an
/// already-archived company kept occupying its slot and appearing in
/// `listCompanies()` — exactly the gap a create/reset dialog's own
/// reconciliation (`create-company-dialog.tsx`, commit 1191ad67e) cannot see
/// or fix from the client, because the client has no visibility into the
/// server's registry (issue #1828 comment 3875046440). This test provisions
/// normally, then swaps in a runtime whose post-persist status() read fails on
/// `archive`, and proves the registry entry does not survive that failure.
#[tokio::test]
async fn archive_removes_from_registry_even_when_the_status_read_fails() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    // Provision normally first, so the durable record exists exactly as it
    // would for a real company (matching `build()`'s rebuild-inherit path
    // the flaky-load helper below relies on for its first `load` call).
    let state = platform_state(&home, None);
    let app = router(state.clone());
    let provisioned = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(provisioned.status(), StatusCode::CREATED);

    // Swap the registered runtime for one whose post-set_lifecycle status()
    // read is made to fail on `archive`, modeling a read blip landing right
    // after the archive write it would be reading back already succeeded.
    let flaky_runtime = build_runtime_with_archive_status_read_failing(&home, &id).await;
    state.registry().insert(id.clone(), Arc::new(flaky_runtime));

    let app = router(state.clone());
    let archived = app
        .oneshot(post_req(
            "/api/v1/companies/acme/archive",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_ne!(
        archived.status(),
        StatusCode::OK,
        "the status() read was made to fail — the response itself must report that"
    );

    assert!(
        state.registry().get(&id).is_none(),
        "set_lifecycle's own store.save already persisted lifecycle:\"archived\" before the \
         status() read failed — the registry entry must not survive a response-body problem \
         after the archive write already landed"
    );
}

// ── issue #1828 comment 3875203599: the response's own OK is proof enough ──

/// Same rebuild-over-the-existing-record shape as
/// `build_runtime_with_archive_status_read_failing` above, but with a store
/// whose FOURTH `load` call fails instead of its third. The first three
/// loads are unchanged (`build()`'s rebuild check, `set_lifecycle`'s own
/// load, `transition`'s post-`set_lifecycle` `status()`) and all SUCCEED
/// here, so `transition` returns an ordinary `200` whose body already
/// confirms `lifecycle: "archived"`. The fourth load is `archive`'s own
/// extra, redundant `runtime.status()` re-read on top of that.
async fn build_runtime_with_redundant_archive_read_failing(
    home: &std::path::Path,
    id: &CompanyId,
) -> crate::runtime::CompanyRuntime {
    let inner = Arc::new(FsCompanyStore::new(home.to_path_buf()));
    let store = Arc::new(FlakyLoadStore::new(inner, 4));
    let manifest: CompanyManifest = toml::from_str(ACME_TOML).unwrap();
    RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .with_store(store)
        .build()
        .await
        .expect("build succeeds — its own save is unaffected by a later load failing")
}

/// The regression codex flagged in 890aac128 itself (PR #1828 comment
/// 3875203599): cleanup was rewritten to depend EXCLUSIVELY on a second,
/// redundant `runtime.status()` re-read — even on the ordinary path where
/// `transition`'s response already came back `200`, meaning its OWN
/// `status()` read (the third `load` above) already succeeded and already
/// confirmed `lifecycle: "archived"`. Re-reading a fourth time to reconfirm
/// what the response body already proved was pure downside: a transient
/// failure on that redundant read flipped `archived` to `false` via
/// `unwrap_or(false)`, so the handler still returned the original successful
/// `200` while leaving the archived runtime and its owner registered — a
/// reset at quota then could not provision its replacement because the
/// retired company still occupied the slot. `response.status() == OK` must
/// be sufficient on its own; the extra read exists only to reconcile
/// non-`OK` responses (the `archive_removes_from_registry_even_when_the_
/// status_read_fails` test above).
#[tokio::test]
async fn archive_removes_from_registry_when_the_response_already_confirms_it_even_if_the_redundant_read_fails()
 {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    // Provision normally first, so the durable record exists exactly as it
    // would for a real company (matching `build()`'s rebuild-inherit path
    // the flaky-load helper below relies on for its first `load` call).
    let state = platform_state(&home, None);
    let app = router(state.clone());
    let provisioned = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(provisioned.status(), StatusCode::CREATED);

    // Swap the registered runtime for one whose FOURTH load — the extra
    // read `archive` performs on top of `transition`'s own status() read —
    // is made to fail. The third load (transition's) still succeeds.
    let flaky_runtime = build_runtime_with_redundant_archive_read_failing(&home, &id).await;
    state.registry().insert(id.clone(), Arc::new(flaky_runtime));

    let app = router(state.clone());
    let archived = app
        .oneshot(post_req(
            "/api/v1/companies/acme/archive",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(
        archived.status(),
        StatusCode::OK,
        "transition's own status() read (the third load) succeeded and already confirmed \
         lifecycle: \"archived\" in the response body — only the redundant fourth read was \
         made to fail"
    );

    assert!(
        state.registry().get(&id).is_none(),
        "the response itself already proved the archive landed — a transient failure on the \
         extra, redundant status() re-read must not leave an already-archived company still \
         registered and occupying its quota slot"
    );
}

// ── issue #1828 comment 3875297944: retry the reconciliation read itself ──
// ── instead of treating one blip on it as proof the archive never landed ──

/// A `CompanyStore` that fails every `load` call whose 1-indexed call number
/// is in `fail_on` and otherwise delegates to `inner`. Unlike `FlakyLoadStore`
/// above (which fails exactly one call), this can make two calls in a row
/// fail — modeling `transition`'s own post-`set_lifecycle` status() read AND
/// one or more attempts of `archive`'s retrying reconciliation read landing
/// back to back.
struct FlakyLoadStoreOnCalls {
    inner: Arc<dyn CompanyStore>,
    fail_on: HashSet<usize>,
    load_calls: std::sync::atomic::AtomicUsize,
}

impl FlakyLoadStoreOnCalls {
    fn new(inner: Arc<dyn CompanyStore>, fail_on: impl IntoIterator<Item = usize>) -> Self {
        Self {
            inner,
            fail_on: fail_on.into_iter().collect(),
            load_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl CompanyStore for FlakyLoadStoreOnCalls {
    async fn load(&self, id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
        let n = self
            .load_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if self.fail_on.contains(&n) {
            return Err(crate::error::OpenCompanyError::Config(
                "transient store read failure".into(),
            ));
        }
        self.inner.load(id).await
    }
    async fn save(&self, record: &CompanyRecord) -> crate::Result<()> {
        self.inner.save(record).await
    }
    async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
        self.inner.list().await
    }
    async fn append_ledger(&self, id: &CompanyId, entry: LedgerEntry) -> crate::Result<()> {
        self.inner.append_ledger(id, entry).await
    }
}

/// Builds a runtime the same way `build_runtime_with_archive_status_read_failing`
/// does, but over a store whose 3rd AND 4th `load` calls both fail. The 3rd is
/// `transition`'s own post-`set_lifecycle` status() read (so its response
/// still comes back non-`OK`, same as the single-failure case above); the 4th
/// is the FIRST attempt of `archive`'s retrying reconciliation read
/// (`archive_reconcile_status`) — made to fail too, so only the retry's
/// SECOND attempt (the 5th load) succeeds. Proves the fix is an actual retry,
/// not a lone lucky first try landing where the old single read used to.
async fn build_runtime_with_archive_status_read_failing_twice(
    home: &std::path::Path,
    id: &CompanyId,
) -> crate::runtime::CompanyRuntime {
    let inner = Arc::new(FsCompanyStore::new(home.to_path_buf()));
    let store = Arc::new(FlakyLoadStoreOnCalls::new(inner, [3, 4]));
    let manifest: CompanyManifest = toml::from_str(ACME_TOML).unwrap();
    RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .with_store(store)
        .build()
        .await
        .expect("build succeeds — its own save is unaffected by a later load failing")
}

/// The retry `archive_reconcile_status` adds (`src/server/provision.rs`)
/// closes the gap the single-attempt `unwrap_or(false)` left: a transient
/// failure on `archive`'s non-`OK` reconciliation read used to be
/// indistinguishable from "not archived", even though `set_lifecycle`'s own
/// `store.save` already persisted `lifecycle: "archived"` before that read
/// ever ran. Left uncorrected, the registry/owner cleanup this branch is the
/// LAST chance to run was skipped for good: the create/reset dialog's own
/// client-side reconciliation has no visibility into the server registry, so
/// if its own later status lookup succeeds it reports the reset as done and
/// never calls `archive` again (issue #1828 comment 3875297944). This test
/// makes BOTH `transition`'s own read and the reconciliation branch's first
/// attempt fail, and proves cleanup still lands once the retry's second
/// attempt succeeds.
#[tokio::test]
async fn archive_removes_from_registry_when_the_reconciliation_read_retries_past_one_blip() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    let state = platform_state(&home, None);
    let app = router(state.clone());
    let provisioned = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(provisioned.status(), StatusCode::CREATED);

    let flaky_runtime = build_runtime_with_archive_status_read_failing_twice(&home, &id).await;
    state.registry().insert(id.clone(), Arc::new(flaky_runtime));

    let app = router(state.clone());
    let archived = app
        .oneshot(post_req(
            "/api/v1/companies/acme/archive",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_ne!(
        archived.status(),
        StatusCode::OK,
        "transition's own status() read (the 3rd load) was made to fail — the response itself \
         must still report that"
    );

    assert!(
        state.registry().get(&id).is_none(),
        "set_lifecycle's own store.save already persisted lifecycle:\"archived\" before either \
         read failed; the reconciliation branch's retry must recover from a single blip on its \
         first attempt (the 4th load) and still find archived on its second (the 5th), so \
         cleanup must not be permanently skipped"
    );
}

/// The retry is bounded, not unconditional trust: when the reconciliation
/// read fails on EVERY attempt (`ARCHIVE_RECONCILE_READ_ATTEMPTS` of them),
/// `archive_reconcile_status` must still report failure rather than looping
/// forever or defaulting to "archived", and the registry entry must survive
/// exactly like the pre-existing single-failure case — proving the fix adds
/// resilience to a genuine blip without papering over a store that is really
/// down.
#[tokio::test]
async fn archive_reconciliation_read_gives_up_after_its_attempt_budget_and_leaves_registry_intact()
{
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    let state = platform_state(&home, None);
    let app = router(state.clone());
    let provisioned = app
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    assert_eq!(provisioned.status(), StatusCode::CREATED);

    // 3rd load = transition's own read (fails, so response is non-OK); 4th,
    // 5th, 6th = every attempt archive_reconcile_status's retry budget makes.
    let inner = Arc::new(FsCompanyStore::new(home.clone()));
    let store = Arc::new(FlakyLoadStoreOnCalls::new(inner, [3, 4, 5, 6]));
    let manifest: CompanyManifest = toml::from_str(ACME_TOML).unwrap();
    let flaky_runtime = RuntimeBuilder::new(home.clone(), manifest)
        .with_id(id.clone())
        .with_store(store)
        .build()
        .await
        .expect("build succeeds — its own save is unaffected by a later load failing");
    state.registry().insert(id.clone(), Arc::new(flaky_runtime));

    let app = router(state.clone());
    let archived = app
        .oneshot(post_req(
            "/api/v1/companies/acme/archive",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_ne!(archived.status(), StatusCode::OK);

    assert!(
        state.registry().get(&id).is_some(),
        "every reconciliation attempt was made to fail — a request this inconclusive must not \
         run cleanup (the registry entry stays), but must also return promptly rather than \
         retrying without bound"
    );
}

// ── codex review on #1943, PR comment 3894439358: durable ownership before ─
// ── the irreversible registry removal ────────────────────────────────────
// ── codex review on #1943, PR comment 3894439351: conditional eviction ────
// ── against a runtime that has since replaced the one confirmed archived ──
//
// Both tests below call `evict_registry_and_ownership` directly — the
// sequencing `evict_archived_company` delegates to — rather than through the
// HTTP `archive` route. Constructing the exact race each finding describes
// (a persisted-store failure landing mid-eviction; a rebuild swap landing in
// the window between a caller observing `"archived"` and eviction actually
// running) through the router would mean synchronizing two concurrent
// requests around one specific await point — the direct call lets the test
// construct each "the race already happened" state deterministically instead.

/// An [`OwnershipStore`](crate::store::select::OwnershipStore) whose
/// `remove_owner` always fails — models a persisted-store hiccup landing
/// exactly on eviction's durable ownership cleanup step.
struct FailingOwnershipRemoval {
    remove_owner_calls: std::sync::atomic::AtomicUsize,
}

impl FailingOwnershipRemoval {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            remove_owner_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl crate::store::select::OwnershipStore for FailingOwnershipRemoval {
    async fn set_owner(&self, _id: &CompanyId, _tenant: &str) -> crate::Result<()> {
        Ok(())
    }
    async fn remove_owner(&self, _id: &CompanyId) -> crate::Result<()> {
        self.remove_owner_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(crate::error::OpenCompanyError::Config(
            "transient ownership removal failure".into(),
        ))
    }
    async fn owners(&self) -> crate::Result<Vec<(CompanyId, String)>> {
        Ok(Vec::new())
    }
}

/// A bare, registered `CompanyRuntime` for the eviction unit tests below —
/// its own tempdir so two calls for the "same" id never share a durable
/// record, which would blur two instances that must stay distinct `Arc`s.
async fn evict_test_runtime(id: &CompanyId) -> Arc<crate::runtime::CompanyRuntime> {
    let home_dir = home();
    let manifest: CompanyManifest = toml::from_str(ACME_TOML).unwrap();
    Arc::new(
        RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .expect("runtime builds"),
    )
}

/// #3894439358: a failed persisted-ownership removal must leave the company
/// registered, not de-registered-with-an-orphaned-row. `MaintenanceTicker::tick`
/// (`src/runtime/maintenance.rs`) only retries a company it can still see in
/// `CompanyRegistry::list` — removing the registry entry BEFORE confirming
/// the durable ownership row is actually gone strands that row the moment the
/// removal fails, with nothing left registered to ever trigger a retry.
#[tokio::test]
async fn a_failed_persisted_ownership_removal_leaves_the_company_registered_for_retry() {
    let id = CompanyId::new("acme");
    let runtime = evict_test_runtime(&id).await;

    let registry = crate::runtime::CompanyRegistry::new();
    registry.insert(id.clone(), runtime.clone());

    let failing = FailingOwnershipRemoval::new();
    let ownership: Option<Arc<dyn crate::store::select::OwnershipStore>> =
        Some(failing.clone() as Arc<dyn crate::store::select::OwnershipStore>);

    let removed = super::evict_registry_and_ownership(&registry, &ownership, &id, &runtime).await;

    assert!(
        !removed,
        "the persisted ownership removal was made to fail — eviction must report that it did \
         not complete"
    );
    assert!(
        registry.get(&id).is_some(),
        "a failed durable ownership removal must leave the company registered so the next \
         maintenance tick can retry the whole eviction — removing it here would orphan the \
         ownership row with nothing left registered to ever revisit it"
    );
    assert_eq!(
        failing
            .remove_owner_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "exactly one removal attempt for this call"
    );
}

/// #3894439351: eviction must not remove a runtime that has since replaced
/// the one it confirmed archived. `CompanyRegistry::insert` — the one choke
/// point every registration goes through, including a rebuild swap
/// (`runtime::rebuild::rebuild_company`) — can land a fresh runtime under the
/// same id in the window between a caller observing `"archived"` and the
/// eviction call actually running. This constructs that race deterministically:
/// `expected` names the ORIGINAL runtime, but the registry now holds a
/// DIFFERENT one under the same id, as if the swap had already landed.
#[tokio::test]
async fn eviction_preserves_a_runtime_that_replaced_the_one_confirmed_archived() {
    let id = CompanyId::new("acme");
    let original = evict_test_runtime(&id).await;
    let replacement = evict_test_runtime(&id).await;
    assert!(
        !Arc::ptr_eq(&original, &replacement),
        "sanity: these must be two distinct runtime instances"
    );

    let registry = crate::runtime::CompanyRegistry::new();
    // The replacement is what's actually registered — as if a rebuild swap
    // landed after `original` was observed archived but before eviction ran.
    registry.insert(id.clone(), replacement.clone());

    let ownership: Option<Arc<dyn crate::store::select::OwnershipStore>> = None;
    let removed = super::evict_registry_and_ownership(&registry, &ownership, &id, &original).await;

    assert!(
        removed,
        "no persisted ownership store was configured, so the durable half trivially succeeds \
         — only the registry conditional is under test here"
    );
    let still_registered = registry.get(&id);
    assert!(
        still_registered.is_some(),
        "the id must still be registered — eviction must not have removed it outright"
    );
    assert!(
        Arc::ptr_eq(still_registered.as_ref().unwrap(), &replacement),
        "eviction confirmed `original` archived, but `replacement` is what was actually \
         registered by the time it ran — removing by id alone would have deregistered the live \
         replacement instead of doing nothing"
    );
}

/// The ordinary case, for contrast with the two failure-mode tests above:
/// nothing raced, ownership removal succeeds (trivially — no store
/// configured), and eviction actually removes the matching registered
/// runtime.
#[tokio::test]
async fn eviction_removes_the_registered_runtime_when_nothing_raced() {
    let id = CompanyId::new("acme");
    let runtime = evict_test_runtime(&id).await;

    let registry = crate::runtime::CompanyRegistry::new();
    registry.insert(id.clone(), runtime.clone());

    let ownership: Option<Arc<dyn crate::store::select::OwnershipStore>> = None;
    let removed = super::evict_registry_and_ownership(&registry, &ownership, &id, &runtime).await;

    assert!(removed);
    assert!(
        registry.get(&id).is_none(),
        "the runtime confirmed archived is the one actually registered, so eviction must \
         remove it"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle authority: who may move a company, not merely who may address it
// ---------------------------------------------------------------------------

/// A `POST` carrying a signed-in human's session cookie.
fn post_req_as(uri: &str, cookie: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap()
}

/// A `POST` carrying a session cookie and a JSON step-up body.
fn json_post_req_as(uri: &str, cookie: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Provisions `acme` and returns the router plus a seeded session of `role`.
async fn company_with_session(
    home: &std::path::Path,
    role: crate::ports::UserRole,
) -> (axum::Router, String) {
    let state = platform_state(home, None);
    let app = router(state.clone());
    app.clone()
        .oneshot(provision_req(Some(PLATFORM_SECRET), ACME_TOML))
        .await
        .unwrap();
    let cookie = crate::server::test_support::seed_session(&state, "acme", role).await;
    (app, cookie)
}

/// The gap this section pins: `pause` stops the whole company, and it used to
/// accept any signed-in member because addressing the company was mistaken for
/// authority over it. Every other admin write on the same session refuses.
#[tokio::test]
async fn a_member_may_not_pause_the_company() {
    let home_dir = home();
    let (app, cookie) = company_with_session(home_dir.path(), crate::ports::UserRole::Member).await;

    let denied = app
        .clone()
        .oneshot(post_req_as("/api/v1/companies/acme/pause", &cookie))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    // The refusal is real, not merely reported: the company is still running.
    let status = app
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(json_body(status).await["lifecycle"], "running");
}

#[tokio::test]
async fn a_member_may_not_resume_the_company() {
    let home_dir = home();
    let (app, cookie) = company_with_session(home_dir.path(), crate::ports::UserRole::Member).await;

    app.clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/pause",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();

    let denied = app
        .clone()
        .oneshot(post_req_as("/api/v1/companies/acme/resume", &cookie))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let status = app
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(json_body(status).await["lifecycle"], "paused");
}

/// The same gap on the kill switch. The confirmation phrase is a step-up
/// against a stray click, never a role check — a member knows it as well as an
/// admin does, and `emergency-resume`'s stronger confirmation is the company's
/// own id, which a member necessarily knows.
#[tokio::test]
async fn a_member_may_not_engage_the_emergency_stop() {
    let home_dir = home();
    let (app, cookie) = company_with_session(home_dir.path(), crate::ports::UserRole::Member).await;

    let denied = app
        .clone()
        .oneshot(json_post_req_as(
            "/api/v1/companies/acme/emergency-pause",
            &cookie,
            serde_json::json!({ "confirm": "EMERGENCY-PAUSE" }),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let status = app
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(json_body(status).await["emergency_paused"], false);
}

#[tokio::test]
async fn a_member_may_not_release_the_emergency_stop() {
    let home_dir = home();
    let (app, cookie) = company_with_session(home_dir.path(), crate::ports::UserRole::Member).await;

    app.clone()
        .oneshot(json_post_req(
            "/api/v1/companies/acme/emergency-pause",
            Some(PLATFORM_SECRET),
            serde_json::json!({ "confirm": "EMERGENCY-PAUSE" }),
        ))
        .await
        .unwrap();

    let denied = app
        .clone()
        .oneshot(json_post_req_as(
            "/api/v1/companies/acme/emergency-resume",
            &cookie,
            serde_json::json!({ "confirm": "acme" }),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    // A refused release must leave the stop engaged.
    let status = app
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(json_body(status).await["emergency_paused"], true);
}

/// The other half of the guard: it must refuse a member without also refusing
/// the admin the routes exist for.
#[tokio::test]
async fn an_admin_may_still_pause_and_resume() {
    let home_dir = home();
    let (app, cookie) = company_with_session(home_dir.path(), crate::ports::UserRole::Admin).await;

    let paused = app
        .clone()
        .oneshot(post_req_as("/api/v1/companies/acme/pause", &cookie))
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    assert_eq!(json_body(paused).await["lifecycle"], "paused");

    let resumed = app
        .oneshot(post_req_as("/api/v1/companies/acme/resume", &cookie))
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    assert_eq!(json_body(resumed).await["lifecycle"], "running");
}

#[tokio::test]
async fn an_admin_may_still_work_the_emergency_stop() {
    let home_dir = home();
    let (app, cookie) = company_with_session(home_dir.path(), crate::ports::UserRole::Admin).await;

    let stopped = app
        .clone()
        .oneshot(json_post_req_as(
            "/api/v1/companies/acme/emergency-pause",
            &cookie,
            serde_json::json!({ "confirm": "EMERGENCY-PAUSE" }),
        ))
        .await
        .unwrap();
    assert_eq!(stopped.status(), StatusCode::OK);
    assert_eq!(json_body(stopped).await["emergency_paused"], true);

    let released = app
        .oneshot(json_post_req_as(
            "/api/v1/companies/acme/emergency-resume",
            &cookie,
            serde_json::json!({ "confirm": "acme" }),
        ))
        .await
        .unwrap();
    assert_eq!(released.status(), StatusCode::OK);
    assert_eq!(json_body(released).await["emergency_paused"], false);
}

/// The narrower platform rule the admin guard must not swallow: a company's own
/// admin still cannot lift a platform-forced suspension.
#[tokio::test]
async fn an_admin_may_not_resume_a_platform_suspended_company() {
    let home_dir = home();
    let (app, cookie) = company_with_session(home_dir.path(), crate::ports::UserRole::Admin).await;

    let suspended = app
        .clone()
        .oneshot(post_req(
            "/api/v1/companies/acme/suspend",
            Some(PLATFORM_SECRET),
        ))
        .await
        .unwrap();
    assert_eq!(suspended.status(), StatusCode::OK);

    let denied = app
        .clone()
        .oneshot(post_req_as("/api/v1/companies/acme/resume", &cookie))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let status = app
        .oneshot(get_req("/api/v1/companies/acme", Some(PLATFORM_SECRET)))
        .await
        .unwrap();
    assert_eq!(json_body(status).await["lifecycle"], "suspended");
}

/// The runtime handle `AdminScopedCompany` hands `pause`/`resume` must be the
/// one that actually moves — not whatever `CompanyRegistry::get` returns for
/// the same id at the moment the handler body runs. `CompanyRegistry::insert`
/// replaces an occupied slot unconditionally ("a rebuild swap is the
/// production case"), so a second, independent lookup by id can return a
/// runtime nobody authorized this request against.
///
/// A live request race through the router can't be driven deterministically —
/// nothing suspends a request mid-flight between the extractor resolving
/// `admin.runtime` and the handler body running. This pins the invariant the
/// same way `eviction_preserves_a_runtime_that_replaced_the_one_confirmed_archived`
/// does above: construct the swap directly, and prove the function lifecycle
/// routes now call acts only on the runtime it was handed and never reaches
/// back into the registry for one sharing its id.
#[tokio::test]
async fn transition_runtime_ignores_a_registry_entry_that_replaced_it() {
    let id = CompanyId::new("acme");
    // Each runtime needs its own store that outlives the build call — unlike
    // `evict_test_runtime`, whose caller never round-trips through the store
    // and so never notices its `home` directory going away with it.
    let authorized_home = home();
    let manifest: CompanyManifest = toml::from_str(ACME_TOML).unwrap();
    let authorized = Arc::new(
        RuntimeBuilder::new(authorized_home.path().to_path_buf(), manifest.clone())
            .with_id(id.clone())
            .build()
            .await
            .expect("runtime builds"),
    );
    let swapped_in_home = home();
    let swapped_in = Arc::new(
        RuntimeBuilder::new(swapped_in_home.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .expect("runtime builds"),
    );
    assert!(
        !Arc::ptr_eq(&authorized, &swapped_in),
        "sanity: these must be two distinct runtime instances"
    );

    // As if a rebuild landed a fresh runtime under the same id after
    // `AdminScopedCompany` resolved and authorized `authorized`.
    let registry = crate::runtime::CompanyRegistry::new();
    registry.insert(id.clone(), swapped_in.clone());

    let auth = GqlAuth::Platform(PlatformClaims {
        tenant: "tenant:acme".to_string(),
        scopes: HashSet::new(),
        companies: None,
    });
    let response = super::transition_runtime(&authorized, &auth, "paused").await;
    assert_eq!(response.status(), StatusCode::OK);

    let authorized_status = authorized.status().await.expect("status reads");
    assert_eq!(
        authorized_status.lifecycle, "paused",
        "the runtime the caller was actually authorized against must be the one that moved"
    );

    let swapped_in_status = swapped_in.status().await.expect("status reads");
    assert_eq!(
        swapped_in_status.lifecycle, "running",
        "the runtime that merely shares the id — and replaced `authorized` in the registry \
         after authorization — must be left alone: a lifecycle route may not silently act on \
         whatever a registry lookup happens to return"
    );
}
