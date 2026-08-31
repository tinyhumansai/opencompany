//! The tool-catalog read surface: `GET {scope}/tools/catalog`.
//!
//! One list of everything this company can grant an agent — built-in tool
//! families, per-tenant MCP servers, and Composio toolkits — in a single
//! vocabulary, each row carrying the exact grant token an operator would write.
//!
//! Read-only, and open to any member who may address the company. Nothing here
//! decides anything: the catalog is a projection of the manifest through the
//! same matcher the roster build uses (see [`crate::company::tool_catalog`]), so
//! this route can only ever describe grants the gate already honours.

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde::Serialize;

use crate::AppState;
use crate::company::tool_catalog::{CatalogEntry, catalog};
use crate::error::OpenCompanyError;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the tool-catalog route fragment.
pub fn router() -> Router<AppState> {
    scoped("/tools/catalog", get(get_catalog))
}

/// The company's tool catalog as the console renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolCatalogDto {
    /// The company-wide `[tools].allow` ceiling, echoed so a console can render
    /// the catalog beside the grant that produced each row's `granted` flag
    /// without a second request.
    company_allow: Vec<String>,
    /// Every grantable entry, built-ins first.
    entries: Vec<CatalogEntry>,
}

async fn get_catalog(company: ScopedCompany) -> Result<Json<ToolCatalogDto>, ApiError> {
    let record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;

    Ok(Json(ToolCatalogDto {
        company_allow: record.manifest.tools.allow.clone(),
        entries: catalog(&record.manifest),
    }))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::CompanyStore;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    const MANIFEST: &str = r#"
[company]
name = "Acme"
[policy]
mode = "full"
[tools]
allow = ["*", "search"]

[[agent]]
id = "ceo"
role = "Chief Executive"

[[mcp_server]]
name = "notion"
endpoint = "https://example.com/mcp"
description = "Notion pages."
"#;

    async fn state() -> (tempfile::TempDir, AppState) {
        let home = tempfile::Builder::new()
            .prefix("oc-tool-catalog-")
            .tempdir()
            .expect("tempdir");
        let manifest: CompanyManifest = toml::from_str(MANIFEST).expect("valid manifest");
        let store = FsCompanyStore::new(home.path().to_path_buf());
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
            .expect("save");
        let runtime = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .expect("runtime");
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        (home, state)
    }

    async fn get(state: &AppState, path: &str) -> (StatusCode, Value) {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    fn entry<'a>(body: &'a Value, key: &str) -> &'a Value {
        body["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .find(|e| e["key"] == key)
            .unwrap_or_else(|| panic!("no catalog entry `{key}` in {body}"))
    }

    #[tokio::test]
    async fn the_catalog_is_served_under_both_scope_forms() {
        let (_home, state) = state().await;
        for path in [
            "/api/v1/company/tools/catalog",
            "/api/v1/companies/acme/tools/catalog",
        ] {
            let (status, _) = get(&state, path).await;
            assert_eq!(status, StatusCode::OK, "{path}");
        }
    }

    /// The row an operator most needs to read correctly: `*` is granted, but it
    /// does not confer the opt-in namespaces, and the response has to say so.
    #[tokio::test]
    async fn the_wildcard_grant_is_reported_honestly() {
        let (_home, state) = state().await;
        let (_, body) = get(&state, "/api/v1/company/tools/catalog").await;

        assert_eq!(entry(&body, "builtin:shell")["granted"], true, "{body}");
        assert_eq!(
            entry(&body, "builtin:media")["granted"],
            false,
            "`*` must not read as granting real-money media: {body}"
        );
        assert_eq!(
            entry(&body, "builtin:media")["coveredByWildcard"],
            false,
            "{body}"
        );
        // `search` is granted here because the manifest names it explicitly.
        assert_eq!(entry(&body, "builtin:search")["granted"], true, "{body}");
    }

    #[tokio::test]
    async fn a_declared_mcp_server_is_a_catalog_row_with_its_grant_token() {
        let (_home, state) = state().await;
        let (_, body) = get(&state, "/api/v1/company/tools/catalog").await;

        let notion = entry(&body, "mcp:notion");
        assert_eq!(notion["grant"], "mcp:notion", "{body}");
        assert_eq!(notion["kind"], "mcp", "{body}");
        assert_eq!(notion["server"], "notion", "{body}");
        assert_eq!(notion["description"], "Notion pages.", "{body}");
    }

    #[tokio::test]
    async fn the_response_echoes_the_company_allow_list() {
        let (_home, state) = state().await;
        let (_, body) = get(&state, "/api/v1/company/tools/catalog").await;
        assert_eq!(body["companyAllow"], serde_json::json!(["*", "search"]));
    }
}
