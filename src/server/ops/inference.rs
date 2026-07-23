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
//! the per-tenant provider re-resolves this config every turn.

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
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// The reminder attached to every mutating response: a per-tenant provider
/// re-resolves its config each turn, so a switch reaches agents on the next
/// turn with no restart.
const SWITCH_NOTE: &str =
    "Agents use the new inference provider on their next turn — no restart needed.";

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

/// Resolves the effective status DTO. The ops layer resolves *tenant* config
/// only (no env default), so a company with nothing configured reports the
/// managed default rather than a synthesized env decl.
async fn effective_status(runtime: &CompanyRuntime) -> Result<InferenceStatusDto, ApiError> {
    let manifest = manifest_inference(runtime).await?;
    let decl = resolve_effective(runtime.id(), &manifest, None, runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    Ok(match decl {
        Some(d) => InferenceStatusDto {
            provider: d.provider.clone(),
            slug: d.telemetry_slug().to_string(),
            base_url: d.base_url.clone(),
            models: d.models.clone(),
            source: source_label(d.source).to_string(),
            key_configured: d.key_configured(),
        },
        None => InferenceStatusDto {
            provider: "managed".to_string(),
            slug: "managed".to_string(),
            base_url: inference::MANAGED_BASE_URL.to_string(),
            models: BTreeMap::new(),
            source: "managed".to_string(),
            key_configured: false,
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

    Ok(Json(MutationResponse {
        status: effective_status(runtime).await?,
        note: SWITCH_NOTE.to_string(),
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

    fn home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("oc-inference-{}", crate::ports::generate_id()))
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
        let home = home();
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

        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test]
    async fn revert_clears_the_runtime_override() {
        let home = home();
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

        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test]
    async fn invalid_provider_config_is_rejected() {
        let home = home();
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

        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test]
    async fn key_never_leaks_across_any_response() {
        let home = home();
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

        std::fs::remove_dir_all(&home).ok();
    }
}
