//! Per-tenant Bring-Your-Own-Key inference management (issue #56): read the
//! company's effective inference status, set a runtime provider override, revert
//! it, and live-probe the configured provider.
//!
//! The effective config is the highest-precedence of a runtime override (a JSON
//! blob the console writes under `inference/config`), the committed manifest
//! `[inference]` section, and the platform managed default. The outbound
//! credential lives apart under `inference/key` and is **write-only** over the
//! API: it is set through the `key` field, stored in the secret store, and never
//! echoed — the read shape carries only a `keyConfigured` bool.
//!
//! A runtime switch takes effect on the agents' **next turn** with no restart:
//! the per-tenant provider re-resolves this config every turn — *once the
//! company is already on the harness cognition path*. Which brain a company runs
//! is chosen once, at build time, so a company that resolved **no** inference
//! source at boot is on the offline echo brain and stays there until the process
//! restarts, no matter what is saved here. That one transition is reported as
//! [`InferenceStatusDto::restart_required`] rather than papered over with a
//! "next turn" promise the runtime cannot keep (issue #266).

use std::collections::BTreeMap;

use axum::Router;
use axum::routing::{get, post};
use axum::{Json, response::Response};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::Inference;
use crate::company::inference::{
    self, InferenceSource, RuntimeInference, clear_runtime_config, resolve_effective,
    save_runtime_config, store_key, validate_runtime,
};
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::UsageMetering;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// The reminder attached to a mutating response that the running brain can act
/// on: a per-tenant provider re-resolves its config each turn, so a switch
/// reaches agents on the next turn with no restart.
const SWITCH_NOTE: &str =
    "Agents use the new inference provider on their next turn — no restart needed.";

/// The reminder attached instead when [`restart_pending`] holds — the save
/// landed, but the running brain predates it (issue #266). Says the thing the
/// operator has to do, and names the two surfaces that stay broken until they do
/// it, because "agents still echo" and "scheduled workflows never fire" are how
/// this is actually noticed.
const RESTART_NOTE: &str = "Saved — but this company started with no inference source, so it is \
     running the offline echo brain and its workflow runner is unwired. The brain is chosen at \
     startup: restart the company for agents to think with this provider and for scheduled \
     workflows to fire.";

/// Builds the inference management route fragment.
pub fn router() -> Router<AppState> {
    scoped(
        "/inference",
        get(get_status).put(set_config).delete(revert_config),
    )
    .merge(scoped("/inference/test", post(test_config)))
}

/// The company's effective inference status as the console renders it. **Never**
/// carries a credential — only a non-secret `keyConfigured` flag.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InferenceStatusDto {
    /// Provider kind (`managed` / `openrouter` / `openai_compatible` / `ollama`).
    provider: String,
    /// The stable telemetry slug (`managed` / `openrouter` / `byok` / `ollama`).
    slug: String,
    /// Resolved OpenAI-compatible base URL.
    base_url: String,
    /// Abstract-tier → concrete model id.
    models: BTreeMap<String, String>,
    /// Where the effective config came from: `default` / `manifest` / `runtime`,
    /// or `managed` when nothing tenant-specific is configured.
    source: String,
    /// Whether an outbound credential is stored — never the credential itself.
    key_configured: bool,
    /// The cognition path this company actually booted onto: `harness` (live
    /// local inference), `hosted` (Medulla), `sidecar`, `echo` (offline — no
    /// inference at all), or `custom`.
    ///
    /// Config resolving to a provider does **not** guarantee the harness path: a
    /// build without the `openhuman` feature, or a config that fails to resolve at
    /// boot, silently falls back to the hosted/echo brain. Reporting what the
    /// runtime actually holds is the only honest answer (issue #174).
    cognition: String,
    /// Where this path's inference usage is metered: `perTurn` (the harness meters
    /// each turn), `perCycle` (the runtime meters what the cycle reports), or
    /// `none` (nothing to meter — the echo path runs no model, so a zero Usage
    /// reading is the truth rather than a missing hook).
    usage_metering: UsageMetering,
    /// Whether a stored inference config resolves but the **running** brain
    /// predates it, so only a restart puts it to work (issue #266).
    ///
    /// See [`restart_pending`] for the exact predicate. `false` covers both "the
    /// config is already live" and "no restart would help either" — the console
    /// tells the second apart from `cognition` on its own, and this flag never
    /// promises a restart that would change nothing.
    restart_required: bool,
}

/// A mutating response: the resulting status plus the switch reminder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationResponse {
    status: InferenceStatusDto,
    note: String,
}

/// Set-config body. `key` is write-only intake (never returned).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetInference {
    provider: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    models: Option<BTreeMap<String, String>>,
    /// The outbound credential, stored write-only. Omit to leave it unchanged;
    /// send an empty string to clear it.
    #[serde(default)]
    key: Option<String>,
}

/// Loads the company's committed `[inference]` section from its record.
async fn manifest_inference(runtime: &CompanyRuntime) -> Result<Inference, ApiError> {
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    Ok(record.map(|r| r.manifest.inference).unwrap_or_default())
}

/// The console-facing source label for a resolved source badge.
fn source_label(source: InferenceSource) -> &'static str {
    match source {
        InferenceSource::Default => "default",
        InferenceSource::Manifest => "manifest",
        InferenceSource::Runtime => "runtime",
    }
}

/// Whether the harness cognition path is reachable on this host at all: the
/// `openhuman` feature compiled in **and** a harness pool attached at boot.
///
/// Without both, no restart can move this company off the echo/hosted brain, so
/// telling the operator to restart would just be a second false promise. The
/// pool is attached whenever the serve path ran `attach_harness`, independently
/// of which brain arm won — which is exactly the "this host could have run the
/// harness, and didn't" signal we need.
#[cfg(feature = "openhuman")]
fn harness_reachable(runtime: &CompanyRuntime) -> bool {
    runtime.harness().is_some()
}

#[cfg(not(feature = "openhuman"))]
fn harness_reachable(_runtime: &CompanyRuntime) -> bool {
    false
}

/// Whether a saved inference config is stranded behind a boot-time decision.
///
/// Brain selection happens once, in `RuntimeBuilder::build`: a company whose
/// inference resolved to nothing at boot gets the offline echo brain **and** an
/// unwired workflow runner, and a later credential write reaches neither. The
/// per-tenant provider does re-resolve every turn, so swapping a model or
/// rotating a key on a company already on the harness path is genuinely live —
/// this is only about the not-configured → configured transition (issue #266).
///
/// True requires all three:
/// 1. a tenant config resolves *now* (the same predicate `build` tests),
/// 2. the company is **not** on the harness path, and
/// 3. the harness path is reachable here, so a restart would actually change it.
fn restart_pending(runtime: &CompanyRuntime, configured: bool) -> bool {
    configured
        && runtime.cognition().path != crate::ports::brain::HARNESS_PATH
        && harness_reachable(runtime)
}

/// [`restart_pending`] resolved from scratch, for callers outside this module
/// that hold only a runtime.
///
/// The workflow-run route uses it to tell "this build has no workflow execution"
/// apart from "this *boot* has none, and a restart would fix it" — the two look
/// identical from `workflow_runner() == None`, and the operator's next step is
/// completely different. Degrades to `false` on a resolve error: a config we
/// cannot read is not evidence that a restart would help.
pub(crate) async fn restart_pending_for(runtime: &CompanyRuntime) -> bool {
    let Ok(manifest) = manifest_inference(runtime).await else {
        return false;
    };
    let configured = resolve_effective(runtime.id(), &manifest, None, runtime.secrets().as_ref())
        .await
        .is_ok_and(|decl| decl.is_some());
    restart_pending(runtime, configured)
}

/// Resolves the effective status DTO. The ops layer resolves *tenant* config
/// only (no env default), so a company with nothing configured reports the
/// managed default rather than a synthesized env decl.
async fn effective_status(runtime: &CompanyRuntime) -> Result<InferenceStatusDto, ApiError> {
    let manifest = manifest_inference(runtime).await?;
    let decl = resolve_effective(runtime.id(), &manifest, None, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    // What the company actually booted onto, not what the config implies.
    let cognition = runtime.cognition();
    let restart_required = restart_pending(runtime, decl.is_some());
    Ok(match decl {
        Some(d) => InferenceStatusDto {
            provider: d.provider.clone(),
            slug: d.telemetry_slug().to_string(),
            base_url: d.base_url.clone(),
            models: d.models.clone(),
            source: source_label(d.source).to_string(),
            key_configured: d.key_configured(),
            cognition: cognition.path.to_string(),
            usage_metering: cognition.metering,
            restart_required,
        },
        None => InferenceStatusDto {
            provider: "managed".to_string(),
            slug: "managed".to_string(),
            base_url: inference::MANAGED_BASE_URL.to_string(),
            models: BTreeMap::new(),
            source: "managed".to_string(),
            key_configured: false,
            cognition: cognition.path.to_string(),
            usage_metering: cognition.metering,
            // `decl` is `None`, so `restart_pending` is `false` here by
            // construction — nothing tenant-specific is configured to be
            // stranded. Threaded rather than hardcoded so the two arms cannot
            // drift apart.
            restart_required,
        },
    })
}

/// `GET …/inference` — the company's effective inference status.
async fn get_status(company: ScopedCompany) -> Result<Json<InferenceStatusDto>, ApiError> {
    Ok(Json(effective_status(company.runtime.as_ref()).await?))
}

/// `PUT …/inference` — set (or replace) the runtime provider override, and
/// optionally rotate the write-only outbound credential.
async fn set_config(
    company: ScopedCompany,
    Json(body): Json<SetInference>,
) -> Result<Json<MutationResponse>, ApiError> {
    let runtime = company.runtime.as_ref();

    let config = RuntimeInference {
        provider: body.provider.trim().to_string(),
        base_url: body
            .base_url
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty()),
        models: body.models.unwrap_or_default(),
    };
    let problems = validate_runtime(&config);
    if !problems.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            problems.join(" "),
        )));
    }

    save_runtime_config(runtime.id(), runtime.secrets().as_ref(), &config)
        .await
        .map_err(ApiError)?;

    // The key is write-only: a non-empty value rotates it, an explicit empty
    // string clears it, and an omitted field leaves it untouched.
    if let Some(key) = body.key {
        store_key(runtime.id(), runtime.secrets().as_ref(), key.trim())
            .await
            .map_err(ApiError)?;
    }

    let status = effective_status(runtime).await?;
    Ok(Json(MutationResponse {
        // The note follows the *resulting* status, so the response can never
        // promise "next turn" to a company whose brain cannot honour it.
        note: if status.restart_required {
            RESTART_NOTE
        } else {
            SWITCH_NOTE
        }
        .to_string(),
        status,
    }))
}

/// `DELETE …/inference` — clear the runtime override, reverting to the committed
/// manifest `[inference]` (or the managed default). The stored credential is
/// left in place (harmless for the managed default; still resolves for a
/// manifest provider) — clear it explicitly with `PUT { key: "" }`.
async fn revert_config(company: ScopedCompany) -> Result<Json<MutationResponse>, ApiError> {
    let runtime = company.runtime.as_ref();
    clear_runtime_config(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    Ok(Json(MutationResponse {
        status: effective_status(runtime).await?,
        note: "Reverted to the committed manifest (or managed) configuration.".to_string(),
    }))
}

/// `POST …/inference/test` — a live one-message probe of the resolved provider.
///
/// Gated on the `openhuman` feature (the HTTP provider lives there); without it
/// the route reports `not_wired` so the console falls back gracefully. The probe
/// error is scrubbed of the credential by the provider layer.
#[cfg(feature = "openhuman")]
async fn test_config(company: ScopedCompany) -> Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let runtime = company.runtime.as_ref();
    let manifest = match manifest_inference(runtime).await {
        Ok(m) => m,
        Err(err) => return err.into_response(),
    };
    let decl =
        match resolve_effective(runtime.id(), &manifest, None, runtime.secrets().as_ref()).await {
            Ok(d) => d,
            Err(err) => return ApiError(err).into_response(),
        };
    match decl {
        None => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "error": "No custom inference is configured — this company is on the managed brain.",
                "code": "not_configured",
            })),
        )
            .into_response(),
        Some(decl) => match crate::harness::provider::probe(&decl).await {
            Ok(()) => Json(serde_json::json!({
                "ok": true,
                "provider": decl.provider,
                "note": "Reached the provider and got a reply.",
            }))
            .into_response(),
            Err(err) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("Inference probe failed: {err}"),
                    "code": "probe_failed",
                })),
            )
                .into_response(),
        },
    }
}

/// Without the `openhuman` feature there is no HTTP provider, so the live probe
/// is "not wired" (the console falls back to the stored status).
#[cfg(not(feature = "openhuman"))]
async fn test_config(company: ScopedCompany) -> Response {
    let _ = company;
    crate::server::ops::not_wired("inference test")
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    const TOKEN: &str = "sk-super-secret-inference-token-XYZ";

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-inference-")
            .tempdir()
            .expect("tempdir")
    }

    fn manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
    }

    async fn state_with_company(home: &std::path::Path) -> AppState {
        use crate::ports::CompanyStore;
        let store = FsCompanyStore::new(home.to_path_buf());
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
                template_provenance: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn send(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, String) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", crate::server::test_support::fixed_cookie("acme"));
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let raw = String::from_utf8_lossy(&bytes).to_string();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value, raw)
    }

    #[tokio::test]
    async fn status_defaults_to_managed_then_switches_to_runtime() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home).await;

        // A company with no manifest/runtime inference reports the managed default.
        let (status, dto, _) = send(&state, "GET", "/api/v1/company/inference", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["provider"], "managed");
        assert_eq!(dto["source"], "managed");
        assert_eq!(dto["keyConfigured"], false);
        assert!(dto.get("key").is_none(), "status DTO must not carry a key");

        // Switch to OpenRouter with a write-only key + a tier→model map.
        let (status, resp, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/inference",
            Some(json!({
                "provider": "openrouter",
                "models": { "chat-v1": "deepseek/deepseek-chat", "reasoning-v1": "deepseek/deepseek-r1" },
                "key": TOKEN,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["provider"], "openrouter");
        assert_eq!(resp["status"]["slug"], "openrouter");
        assert_eq!(resp["status"]["source"], "runtime");
        assert_eq!(resp["status"]["keyConfigured"], true);
        assert_eq!(
            resp["status"]["models"]["chat-v1"],
            "deepseek/deepseek-chat"
        );
        // The token must NEVER appear in the mutation response body.
        assert!(!raw.contains(TOKEN), "PUT response leaked the token: {raw}");

        // GET reflects the switch and still never carries the token.
        let (_, dto, raw) = send(&state, "GET", "/api/v1/company/inference", None).await;
        assert_eq!(dto["provider"], "openrouter");
        assert_eq!(dto["source"], "runtime");
        assert_eq!(dto["keyConfigured"], true);
        assert!(!raw.contains(TOKEN), "GET status leaked the token: {raw}");
    }

    #[tokio::test]
    async fn revert_clears_the_runtime_override() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home).await;

        send(
            &state,
            "PUT",
            "/api/v1/company/inference",
            Some(json!({ "provider": "openrouter", "key": TOKEN })),
        )
        .await;

        let (status, resp, _) = send(&state, "DELETE", "/api/v1/company/inference", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["status"]["provider"], "managed");
        assert_eq!(resp["status"]["source"], "managed");
    }

    #[tokio::test]
    async fn invalid_provider_config_is_rejected() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home).await;

        // Ollama requires a base_url.
        let (status, err, _) = send(
            &state,
            "PUT",
            "/api/v1/company/inference",
            Some(json!({ "provider": "ollama" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            err["error"]
                .as_str()
                .unwrap_or_default()
                .contains("base_url"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn key_never_leaks_across_any_response() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home).await;

        let (_, _, put_raw) = send(
            &state,
            "PUT",
            "/api/v1/company/inference",
            Some(json!({ "provider": "openai_compatible", "baseUrl": "https://byo.example/v1", "key": TOKEN })),
        )
        .await;
        let (_, _, get_raw) = send(&state, "GET", "/api/v1/company/inference", None).await;
        // The live probe path returns an error (unreachable host) — assert the
        // scrubbed error body still never contains the token.
        let (_, _, test_raw) = send(&state, "POST", "/api/v1/company/inference/test", None).await;

        for raw in [put_raw, get_raw, test_raw] {
            assert!(!raw.contains(TOKEN), "a response leaked the token: {raw}");
        }
    }

    // --- Issue #266: a save the running brain cannot honour --------------------

    /// On a host with no harness reachable, a saved config is *never* "restart
    /// pending" — the echo brain is where this build ends up no matter how many
    /// times it is restarted, and telling the operator otherwise would send them
    /// bouncing a process for nothing.
    ///
    /// This is the default build, so it is also the guard that keeps the flag
    /// from firing on every self-hosted instance that simply has no local
    /// inference compiled in.
    #[tokio::test]
    async fn a_save_is_not_restart_pending_when_no_restart_would_help() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home).await;

        let (status, resp, _) = send(
            &state,
            "PUT",
            "/api/v1/company/inference",
            Some(json!({ "provider": "openrouter", "key": TOKEN })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // The config landed...
        assert_eq!(resp["status"]["source"], "runtime");
        assert_eq!(resp["status"]["keyConfigured"], true);
        // ...and the runtime is on the echo brain, but no harness is reachable
        // here, so there is nothing a restart would change.
        assert_eq!(resp["status"]["cognition"], "echo");
        assert_eq!(resp["status"]["restartRequired"], false);
        assert!(
            resp["note"]
                .as_str()
                .unwrap_or_default()
                .contains("no restart needed"),
            "{}",
            resp["note"]
        );

        let (_, dto, _) = send(&state, "GET", "/api/v1/company/inference", None).await;
        assert_eq!(dto["restartRequired"], false);
    }

    /// A company with nothing configured has nothing stranded, so the flag is
    /// off even before any save.
    #[tokio::test]
    async fn an_unconfigured_company_is_not_restart_pending() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_company(&home).await;

        let (_, dto, _) = send(&state, "GET", "/api/v1/company/inference", None).await;
        assert_eq!(dto["source"], "managed");
        assert_eq!(dto["restartRequired"], false);
    }

    /// Issue #266, reproduced at the route: a company built with a harness pool
    /// but **no** inference source boots onto the echo brain with an unwired
    /// workflow runner. Storing a credential afterwards updates the secret store
    /// and nothing else — the brain is chosen in `RuntimeBuilder::build` and this
    /// one already ran.
    ///
    /// So the save must report `restartRequired`, the note must say restart
    /// rather than "next turn", and `POST …/workflows/{id}/run` must stop
    /// claiming the deployment lacks workflow execution when what it lacks is a
    /// restart.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn configuring_inference_after_boot_reports_restart_required() {
        use crate::harness::HarnessPool;

        let home_dir = home();
        let home = home_dir.path().to_path_buf();

        // Build the company the way the serve path does — with a harness pool
        // attached — but with no inference source of any kind. That is the boot
        // this issue is about.
        let id = CompanyId::new("acme");
        let runtime = RuntimeBuilder::new(home.clone(), manifest())
            .with_id(id.clone())
            .with_harness(std::sync::Arc::new(HarnessPool::new()))
            .build()
            .await
            .unwrap();
        // The bug's precondition, asserted rather than assumed: the harness was
        // available and the company still landed on the offline brain.
        assert_eq!(
            runtime.cognition().path,
            "echo",
            "expected the no-inference boot to select the echo brain"
        );
        assert!(
            runtime.workflow_runner().is_none(),
            "expected the no-inference boot to leave the workflow runner unwired"
        );

        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;

        // Configure inference, exactly as the console does.
        let (status, resp, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/inference",
            Some(json!({
                "provider": "openai_compatible",
                "baseUrl": "https://stub.invalid/v1",
                "key": TOKEN,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["source"], "runtime");
        assert_eq!(resp["status"]["keyConfigured"], true);
        // The config resolves now; the running brain still does not know it.
        assert_eq!(resp["status"]["cognition"], "echo");
        assert_eq!(resp["status"]["restartRequired"], true, "{raw}");

        let note = resp["note"].as_str().unwrap_or_default();
        assert!(note.contains("restart"), "note must say restart: {note}");
        assert!(
            !note.contains("next turn"),
            "note must not promise the next turn: {note}"
        );
        assert!(!raw.contains(TOKEN), "PUT response leaked the token: {raw}");

        // The flag is a property of the runtime, not of the mutation, so a plain
        // read reports it too — this is what keeps the warning on screen after
        // the toast is gone.
        let (_, dto, _) = send(&state, "GET", "/api/v1/company/inference", None).await;
        assert_eq!(dto["restartRequired"], true);

        // Second surface: the run route no longer blames the deployment.
        let (status, err, _) = send(
            &state,
            "POST",
            "/api/v1/company/workflows/daily/run",
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(err["code"], "restart_required");
        assert!(
            err["error"]
                .as_str()
                .unwrap_or_default()
                .contains("Restart"),
            "{err}"
        );

        // Reverting to managed un-strands the company: there is no longer a
        // saved config waiting on a restart, so the flag clears.
        let (_, resp, _) = send(&state, "DELETE", "/api/v1/company/inference", None).await;
        assert_eq!(resp["status"]["restartRequired"], false);

        // ...and with nothing pending, the run route is back to the honest
        // "this deployment has no workflow execution" 404.
        let (status, err, _) = send(
            &state,
            "POST",
            "/api/v1/company/workflows/daily/run",
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(err["code"], "not_wired");
    }
}
