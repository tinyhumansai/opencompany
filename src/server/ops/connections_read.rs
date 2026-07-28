//! Read-only connection status (`GET …/connections`).
//!
//! Deliberately **un-gated**: connection *status* is readable even when the
//! token-exchanging OAuth write routes ([`ops::connections`](super::connections),
//! the `oauth` feature) are not compiled. Without this route the console's
//! `GET …/connections` 404s and the page falls back to the read-only
//! "connections unavailable" banner; with it the console renders real
//! per-provider state and lights up the Connect buttons.
//!
//! Every field here is a **non-secret projection** — the provider id, a
//! `connected` boolean, and an optional account label — mirroring the GraphQL
//! `Company.connections` resolver
//! ([`resolve_connections`](crate::server::graphql::connections::resolve_connections)).
//! The stored OAuth token material never appears in the response or any log.

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde::Serialize;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// One connection's non-secret status, matching the console `ConnectionState`
/// wire type (`frontend/src/api/types.ts`): `{ provider, connected, account? }`.
#[derive(Debug, Serialize)]
struct ConnectionStateDto {
    /// The provider id (e.g. `github`, `slack`, `gmail`).
    provider: String,
    /// Whether a non-empty OAuth token is stored for this provider.
    connected: bool,
    /// The connected account label, when known — never token material. Omitted
    /// when not connected or when the stored blob carries no account.
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

/// Builds the connection-status route fragment (both scope forms).
pub fn router() -> Router<AppState> {
    scoped("/connections", get(list))
}

/// Projects each manifest connection into its non-secret status by reading the
/// `oauth/{provider}` secret. Mirrors
/// [`resolve_connections`](crate::server::graphql::connections::resolve_connections):
/// only `provider` / `connected` / `account` ever leave this function — the
/// token blob stays in the [`SecretStore`](crate::ports::SecretStore).
async fn project(runtime: &CompanyRuntime) -> Result<Vec<ConnectionStateDto>, ApiError> {
    let Some(record) = runtime.store().load(runtime.id()).await.map_err(ApiError)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(record.manifest.connections.len());
    for connection in &record.manifest.connections {
        let key = format!("oauth/{}", connection.provider);
        let (connected, account) = match runtime
            .secrets()
            .get(runtime.id(), &key)
            .await
            .map_err(ApiError)?
        {
            Some(value) if !value.expose().trim().is_empty() => {
                // Read only the `account` label out of the stored blob; the
                // `token` field is intentionally never touched.
                let account = serde_json::from_str::<serde_json::Value>(value.expose())
                    .ok()
                    .and_then(|json| {
                        json.get("account")
                            .and_then(|a| a.as_str())
                            .map(str::to_string)
                    });
                (true, account)
            }
            _ => (false, None),
        };
        out.push(ConnectionStateDto {
            provider: connection.provider.clone(),
            connected,
            account,
        });
    }
    Ok(out)
}

/// `GET …/connections` — the company's non-secret connection status list.
async fn list(company: ScopedCompany) -> Result<Json<Vec<ConnectionStateDto>>, ApiError> {
    Ok(Json(project(company.runtime.as_ref()).await?))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, CompanyRecord, SecretValue};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("oc-connections-{}", crate::ports::generate_id()))
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
                overlay_desks: Vec::new(),
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

    async fn get_connections(state: &AppState) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri("/api/v1/company/connections")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// The list route projects a connected provider (token stored) and a
    /// not-connected one (no token) side by side — the shape the console needs
    /// to flip from "unavailable" to live buttons.
    #[tokio::test]
    async fn projects_connected_and_not_connected() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[connection]]\nprovider = \"github\"\n\
             [[connection]]\nprovider = \"slack\"\n\
             [[connection]]\nprovider = \"gmail\"\n",
        )
        .await;

        // Store a GitHub token blob (github connected, with an account label);
        // a Gmail token blob with NO account label (connected, account omitted);
        // slack stays untouched (not connected).
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        runtime
            .secrets()
            .set(
                &id,
                "oauth/github",
                SecretValue(
                    serde_json::json!({
                        "token": { "access_token": "gho_secret_should_never_leak" },
                        "account": "octocat"
                    })
                    .to_string(),
                ),
            )
            .await
            .unwrap();
        runtime
            .secrets()
            .set(
                &id,
                "oauth/gmail",
                SecretValue(
                    serde_json::json!({
                        "token": { "access_token": "ya29_secret_should_never_leak" }
                    })
                    .to_string(),
                ),
            )
            .await
            .unwrap();

        let (status, body) = get_connections(&state).await;
        assert_eq!(status, StatusCode::OK);
        let list = body.as_array().expect("array body");
        assert_eq!(list.len(), 3, "one row per manifest connection: {body}");

        let github = list
            .iter()
            .find(|c| c["provider"] == "github")
            .expect("github row");
        assert_eq!(github["connected"], true);
        assert_eq!(github["account"], "octocat");

        let slack = list
            .iter()
            .find(|c| c["provider"] == "slack")
            .expect("slack row");
        assert_eq!(slack["connected"], false);
        assert!(
            slack.get("account").is_none(),
            "no account when not connected: {slack}"
        );

        // A connected provider whose stored blob carries no `account` label is
        // still `connected: true`, and the `account` field is omitted entirely
        // (never serialized as null) — the `skip_serializing_if` path.
        let gmail = list
            .iter()
            .find(|c| c["provider"] == "gmail")
            .expect("gmail row");
        assert_eq!(gmail["connected"], true);
        assert!(
            gmail.get("account").is_none(),
            "no account when the stored blob carries no label: {gmail}"
        );

        // SECURITY: no token material may appear anywhere in the response.
        assert!(
            !body.to_string().contains("gho_secret_should_never_leak"),
            "token material leaked into the connections response: {body}"
        );
        assert!(
            !body.to_string().contains("ya29_secret_should_never_leak"),
            "token material leaked into the connections response: {body}"
        );
        assert!(
            !body.to_string().contains("access_token"),
            "token field leaked into the connections response: {body}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// A company with no `[[connection]]` entries returns an empty list (200),
    /// not a 404 — so the console renders "ready" with an empty catalog rather
    /// than the "unavailable" fallback.
    #[tokio::test]
    async fn empty_when_no_connections_declared() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        let (status, body) = get_connections(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!([]), "empty array: {body}");

        std::fs::remove_dir_all(&home).ok();
    }
}
