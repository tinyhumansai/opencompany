//! Tests for the memory-engine selection route.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::company::CompanyManifest;
use crate::ports::store::CompanyStore;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::runtime::RuntimeBuilder;
use crate::server::router;
use crate::store::FsCompanyStore;
use crate::test_support::EnvVarGuard;
use crate::{AppConfig, AppState};

/// Every environment key the engine surface reads, so a guarded test restores
/// all of them however it fails.
const MEMORY_ENV: [&str; 5] = [
    "OPENCOMPANY_MEMORY",
    "OPENCOMPANY_MEMORY_DRIVER",
    "OPENCOMPANY_MEMORY_URL",
    "OPENCOMPANY_MEMORY_API_KEY",
    "OPENCOMPANY_DATA_DIR",
];

/// A rebuilder that rebuilds a company over its live handover, so the apply
/// path's live swap is exercised rather than reported as needing a restart.
struct TestRebuilder {
    home: std::path::PathBuf,
}

#[async_trait::async_trait]
impl crate::runtime::RuntimeRebuilder for TestRebuilder {
    async fn rebuild(
        &self,
        _state: &AppState,
        request: crate::runtime::RebuildRequest,
    ) -> crate::Result<crate::CompanyRuntime> {
        RuntimeBuilder::new(self.home.clone(), request.manifest)
            .with_id(request.id)
            .with_handover(request.handover)
            .build()
            .await
    }
}

/// A one-company host whose `config.toml` lives under `dir`.
async fn state_at(dir: &std::path::Path) -> AppState {
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
    let store = FsCompanyStore::new(dir.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            overlay_agent_edits: Vec::new(),
            overlay_retired_agents: Vec::new(),
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
    let runtime = RuntimeBuilder::new(dir.to_path_buf(), manifest)
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default())
        .with_home(dir.to_path_buf())
        .with_config_root(dir.to_path_buf())
        .with_rebuilder(Arc::new(TestRebuilder {
            home: dir.to_path_buf(),
        }));
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

/// Issues one authenticated request against the whole router.
async fn call(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", crate::server::test_support::fixed_cookie("acme"));
    let request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The catalog is the console's whole menu, so an id it offers must be one the
/// runtime can resolve. A tile whose id maps to nothing would render, accept a
/// click, and fail the apply.
#[test]
fn every_offered_engine_resolves_to_a_runtime_selection() {
    for option in catalog() {
        assert!(
            split_engine(option.id).is_some(),
            "`{}` is offered but has no (backend, driver) mapping",
            option.id
        );
    }
}

/// And the round trip holds: the id a selection renders as is the id that
/// produced it. Without this the console could apply `mem0` and be shown
/// `remote` afterwards, which reads as a failed save.
#[test]
fn engine_ids_round_trip_through_a_selection() {
    for option in catalog() {
        let (backend, driver) = split_engine(option.id).unwrap();
        let selection = MemorySelection {
            backend,
            driver: driver.map(str::to_string),
            url: None,
            api_key: None,
        };
        assert_eq!(engine_id(&selection), option.id);
    }
}

/// The remote catalog must not drift from the driver ids the host can actually
/// construct — the duplication this module documents.
#[cfg(feature = "tinymemory")]
#[test]
fn catalog_matches_driver_registry() {
    let offered: Vec<&str> = catalog()
        .iter()
        .filter(|o| split_engine(o.id).is_some_and(|(b, _)| b == MemoryBackend::Remote))
        .map(|o| o.id)
        .collect();
    assert_eq!(
        offered,
        crate::store::memory::driver::SUPPORTED_REMOTE_DRIVERS.to_vec()
    );
}

/// The hosted engines are available in a **default** build, not only in one
/// somebody remembered to pass `--features tinymemory` to.
///
/// This is a regression pin with a support case behind it: the memory-engine
/// catalog is a console surface in every build, and without `tinymemory` four
/// of its tiles ("Supermemory", "Mem0", "Cognee", "No memory") render disabled
/// with "this build was compiled without the `tinymemory` feature" — an
/// instruction to go and find a differently compiled binary. For the desktop
/// app and for anyone running the shipped container that is not an instruction
/// they can follow, so the feature ships in the default set (see `Cargo.toml`)
/// and this asserts it from a lane that passes no features at all.
///
/// Deliberately NOT quantified over the whole catalog: the in-pod engines
/// (`embedded`, `namespace`) do cost a bundled SQLite build and stay opt-in.
/// The desktop keeps those off too — it offers no in-pod memory surface — and
/// enables the hosted drivers itself via `tinymemory` in `src-tauri/Cargo.toml`.
#[test]
fn the_hosted_memory_engines_ship_in_the_default_build() {
    let hosted = ["supermemory", "mem0", "cognee", "null"];
    let entries: Vec<_> = catalog()
        .into_iter()
        .filter(|option| hosted.contains(&option.id))
        .collect();
    // Assert that the catalog still offers every required ID, so a regression
    // that drops one of them fails here rather than passing silently because
    // the entry never reached the disabled list.
    let offered: Vec<_> = entries.iter().map(|option| option.id).collect();
    assert_eq!(offered, hosted);
    let disabled: Vec<String> = entries
        .into_iter()
        .filter(|option| !option.available)
        .map(|option| {
            format!(
                "{}: {}",
                option.id,
                option.unavailable_reason.unwrap_or_default()
            )
        })
        .collect();
    assert!(
        disabled.is_empty(),
        "a default build must offer the hosted memory engines: {disabled:?}"
    );
}

/// Whether this build can bind an engine is answered separately from whether
/// the operator filled the form in — so a build without the feature refuses
/// with the reason that actually blocks it.
#[test]
fn an_engine_this_build_cannot_bind_is_refused_by_availability() {
    let refused = ensure_available("supermemory");
    if cfg!(feature = "tinymemory") {
        assert!(refused.is_ok());
    } else {
        let error = refused.unwrap_err().to_string();
        assert!(error.contains("tinymemory"), "unexpected message: {error}");
    }
    assert!(
        ensure_available("pinecone")
            .unwrap_err()
            .to_string()
            .contains("supermemory"),
        "an unknown id lists the catalog"
    );
}

/// A hosted engine with no endpoint is refused before anything is written,
/// with a message naming the field rather than an environment variable the
/// operator never set.
#[test]
fn a_hosted_engine_without_an_endpoint_is_refused() {
    let error = selection_from(
        &EngineRequest {
            engine: "supermemory".to_string(),
            url: None,
            api_key: Some("k".to_string()),
        },
        &MemorySelection::default(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("endpoint"),
        "unexpected message: {error}"
    );
}

/// An omitted key keeps the stored one. A console showing a redacted key must
/// be able to save an endpoint change without resending the credential — and
/// must not store the redaction if it does.
#[test]
fn an_omitted_key_keeps_the_stored_credential() {
    let stored = MemorySelection {
        backend: MemoryBackend::Remote,
        driver: Some("mem0".to_string()),
        url: Some("https://old.example".to_string()),
        api_key: Some("secret".to_string()),
    };
    let selection = selection_from(
        &EngineRequest {
            engine: "mem0".to_string(),
            url: Some("https://new.example".to_string()),
            api_key: None,
        },
        &stored,
    )
    .unwrap();
    assert_eq!(selection.api_key.as_deref(), Some("secret"));
    assert_eq!(selection.url.as_deref(), Some("https://new.example"));
}

/// Switching to an engine that needs neither drops both — a stale endpoint and
/// a stale credential left in `config.toml` are dead keys the next reader of
/// the file has to work out are inert.
#[test]
fn switching_to_the_built_in_store_drops_endpoint_and_credential() {
    let stored = MemorySelection {
        backend: MemoryBackend::Remote,
        driver: Some("mem0".to_string()),
        url: Some("https://old.example".to_string()),
        api_key: Some("secret".to_string()),
    };
    let selection = selection_from(
        &EngineRequest {
            engine: "store".to_string(),
            url: None,
            api_key: None,
        },
        &stored,
    )
    .unwrap();
    assert_eq!(selection.backend, MemoryBackend::Store);
    assert!(selection.url.is_none());
    assert!(selection.api_key.is_none());
}

/// The default host: nothing selected, the base store serving memory, and the
/// choice editable from here.
#[tokio::test]
async fn the_default_host_reports_the_built_in_store_as_editable() {
    let dir = tempfile::tempdir().unwrap();
    let guard = EnvVarGuard::capture(&MEMORY_ENV);
    guard.remove("OPENCOMPANY_MEMORY");
    let state = state_at(dir.path()).await;

    let (status, body) = call(&state, "GET", "/api/v1/company/memory/engine", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], "store");
    assert_eq!(body["selected"], "store");
    assert_eq!(body["layer"], "default");
    assert_eq!(body["editable"], true);
    assert_eq!(body["apiKeySet"], false);
    assert!(
        body["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["id"] == "supermemory"),
        "the catalog is offered whether or not this build can bind it"
    );
}

#[test]
fn an_env_owned_engine_is_detected_from_an_injected_source() {
    let env = crate::app::config::MapEnv::new([("OPENCOMPANY_MEMORY", "store")]);
    assert!(crate::store::StorageSettings::memory_is_env_owned_by(&env));
}

/// A health answer is useful only if it is current. The engine route is
/// operator-authenticated and re-probes its overlay for each read, while the
/// unauthenticated `/spec` handshake carries no memory block at all.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn read_reprobes_the_live_memory_engine() {
    let dir = tempfile::tempdir().unwrap();
    let overlay = crate::store::open_memory_overlay(&crate::store::StorageSettings {
        memory_backend: MemoryBackend::Null,
        ..Default::default()
    })
    .expect("null binds")
    .expect("null yields an overlay");
    assert_eq!(
        overlay.descriptor.healthy, None,
        "the overlay starts unprobed"
    );

    let state = state_at(dir.path()).await.with_memory_overlay(overlay);
    let (status, body) = call(&state, "GET", "/api/v1/company/memory/engine", None).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], "null");
    assert_eq!(body["healthy"], true, "GET must return its fresh probe");
}

/// The refusal this surface exists for: a deployment that injects
/// `OPENCOMPANY_MEMORY` owns the engine, so the console renders read-only and
/// a write is refused rather than accepted-and-dropped.
#[tokio::test]
async fn an_env_owned_engine_is_read_only_and_refuses_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let guard = EnvVarGuard::capture(&MEMORY_ENV);
    guard.set("OPENCOMPANY_MEMORY", "store");
    let state = state_at(dir.path()).await;

    let (status, body) = call(&state, "GET", "/api/v1/company/memory/engine", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["layer"], "env");
    assert_eq!(body["editable"], false);

    let (status, body) = call(
        &state,
        "PUT",
        "/api/v1/company/memory/engine",
        Some(json!({ "engine": "store" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("OPENCOMPANY_MEMORY"),
        "the refusal must name the variable that outranks the file: {body}"
    );
}

/// An apply writes the file, and the read that follows reports the new
/// selection — the console's own proof the choice landed.
#[tokio::test]
async fn applying_an_engine_persists_it_to_config_toml() {
    let dir = tempfile::tempdir().unwrap();
    let guard = EnvVarGuard::capture(&MEMORY_ENV);
    guard.remove("OPENCOMPANY_MEMORY");
    guard.set("OPENCOMPANY_DATA_DIR", dir.path().to_str().unwrap());
    let state = state_at(dir.path()).await;

    let (status, body) = call(
        &state,
        "PUT",
        "/api/v1/company/memory/engine",
        Some(json!({ "engine": "store" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["engine"], "store");
    assert_eq!(
        body["restartRequiredFor"].as_array().unwrap().len(),
        0,
        "a host with a rebuilder wired applies the swap live: {body}"
    );

    let written = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(written.contains("[memory]"), "written file: {written}");
    assert!(written.contains("backend = \"store\""), "{written}");

    let (_, body) = call(&state, "GET", "/api/v1/company/memory/engine", None).await;
    assert_eq!(body["selected"], "store");
    assert_eq!(body["layer"], "config.toml");
}

/// An engine id nothing in the catalog carries is a bad request, and the
/// message lists what may be picked instead.
#[tokio::test]
async fn an_unknown_engine_is_refused_with_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let guard = EnvVarGuard::capture(&MEMORY_ENV);
    guard.remove("OPENCOMPANY_MEMORY");
    let state = state_at(dir.path()).await;

    let (status, body) = call(
        &state,
        "PUT",
        "/api/v1/company/memory/engine",
        Some(json!({ "engine": "pinecone" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("supermemory"),
        "{body}"
    );
}
