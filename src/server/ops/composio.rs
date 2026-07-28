//! Per-tenant Composio connection management (issue #110, epic #26 Cell D): read
//! the company's Composio status and set its write-only OAuth bearer token.
//!
//! The token is the entire tenant-isolation lever — the backend derives the
//! Composio entity from it — so it is **write-only** over the API: set through
//! the `token` field, stored in the secret store under
//! [`TOKEN_KEY`](crate::company::composio::TOKEN_KEY), and **never** echoed. The
//! read shape carries only a `tokenConfigured` boolean plus non-secret routing
//! (backend URL, toolkit allowlist). A token set/rotate/clear takes effect on
//! the agents' **next turn** with no restart (the harness re-resolves it each
//! turn and rebuilds the roster when it changes).

use axum::Json;
use axum::Router;
use axum::routing::{get, put};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::composio::{backend_url_or_default, store_token, token_configured};
use crate::company::runtime::CompanyRuntime;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// The reminder attached to every mutating response.
const SWITCH_NOTE: &str =
    "Agents pick up the new Composio token on their next turn — no restart needed.";

/// Builds the Composio management route fragment.
pub fn router() -> Router<AppState> {
    scoped("/composio", get(get_status)).merge(scoped("/composio/token", put(set_token)))
}

/// The company's Composio status as the console renders it. **Never** carries the
/// token — only a non-secret `tokenConfigured` flag plus routing.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ComposioStatusDto {
    /// Whether the `composio` feature is compiled into this build at all (the
    /// tools only exist under it). `false` lets the console show a "not in this
    /// build" state rather than implying a missing token.
    in_build: bool,
    /// Whether the company **explicitly** grants the `composio` namespace (a `*`
    /// wildcard does NOT count).
    granted: bool,
    /// Whether a non-empty per-tenant token is stored — never the token itself.
    token_configured: bool,
    /// The effective Composio backend URL (env override or default). Non-secret.
    backend_url: String,
    /// The manifest toolkit allowlist (empty = defer to the backend allowlist).
    toolkits: Vec<String>,
}

/// A mutating response: the resulting status plus the switch reminder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationResponse {
    status: ComposioStatusDto,
    note: String,
}

/// Set-token body. `token` is write-only intake (never returned): a non-empty
/// value rotates it, an explicit empty string clears it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetToken {
    token: String,
}

/// Resolves the Composio status DTO for a company.
async fn effective_status(runtime: &CompanyRuntime) -> Result<ComposioStatusDto, ApiError> {
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let (granted, toolkits) = match record {
        Some(record) => (
            crate::company::grants_composio_explicit(&record.manifest.tools.allow),
            record.manifest.tools.composio.toolkits.clone(),
        ),
        None => (false, Vec::new()),
    };
    let configured = token_configured(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    let env_url = {
        use crate::app::config::EnvSource;
        crate::app::config::ProcessEnv.get(crate::company::composio::COMPOSIO_BACKEND_URL_ENV)
    };
    Ok(ComposioStatusDto {
        in_build: cfg!(feature = "composio"),
        granted,
        token_configured: configured,
        backend_url: backend_url_or_default(env_url),
        toolkits,
    })
}

/// `GET …/composio` — the company's Composio status.
async fn get_status(company: ScopedCompany) -> Result<Json<ComposioStatusDto>, ApiError> {
    Ok(Json(effective_status(company.runtime.as_ref()).await?))
}

/// `PUT …/composio/token` — set / rotate / clear the write-only per-tenant token.
async fn set_token(
    company: ScopedCompany,
    Json(body): Json<SetToken>,
) -> Result<Json<MutationResponse>, ApiError> {
    let runtime = company.runtime.as_ref();
    store_token(runtime.id(), runtime.secrets().as_ref(), &body.token)
        .await
        .map_err(ApiError)?;
    Ok(Json(MutationResponse {
        status: effective_status(runtime).await?,
        note: SWITCH_NOTE.to_string(),
    }))
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

    const TOKEN: &str = "composio-tenant-bearer-SECRET-xyz";

    fn home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("oc-composio-{}", crate::ports::generate_id()))
    }

    async fn state_with_manifest(home: &std::path::Path, manifest_toml: &str) -> AppState {
        use crate::ports::CompanyStore;
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
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
    async fn status_reports_grant_and_toolkits_then_token_round_trips_write_only() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\", \"github\"]\n",
        )
        .await;

        // Initial status: granted, toolkits surfaced, no token yet.
        let (status, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["granted"], true);
        assert_eq!(dto["tokenConfigured"], false);
        assert_eq!(dto["toolkits"], json!(["gmail", "github"]));
        assert!(dto.get("backendUrl").is_some());
        assert!(
            dto.get("token").is_none(),
            "status must never carry a token"
        );

        // Set the write-only token.
        let (status, resp, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": TOKEN })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["tokenConfigured"], true);
        assert!(!raw.contains(TOKEN), "PUT response leaked the token: {raw}");

        // GET reflects it and still never carries the token.
        let (_, dto, raw) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(dto["tokenConfigured"], true);
        assert!(!raw.contains(TOKEN), "GET status leaked the token: {raw}");

        // Clearing with "" removes it.
        let (_, resp, _) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": "" })),
        )
        .await;
        assert_eq!(resp["status"]["tokenConfigured"], false);

        std::fs::remove_dir_all(&home).ok();
    }

    /// A `*` wildcard grant must NOT count as a composio grant on the status
    /// route (mirrors the harness build gate).
    #[tokio::test]
    async fn wildcard_grant_does_not_count_as_composio() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"*\"]\n",
        )
        .await;
        let (_, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(dto["granted"], false, "{dto}");
        std::fs::remove_dir_all(&home).ok();
    }
}
