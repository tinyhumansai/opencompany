//! The search configuration write-plane: choose which provider a company's
//! agents search through, and store the key behind it — **write-only**.
//!
//! `GET …/search` returns only [`SearchStatus`]: the provider slug, booleans,
//! and the non-secret endpoint. The API key is never serialized into any
//! response, by construction. It lives in
//! [`SecretStore`](crate::ports::SecretStore) and this module reads it back only
//! to report *whether* it is there. The same shape, and the same reasoning, as
//! [`hosting`](super::hosting).
//!
//! # Why "not configured" is a working state and not an error
//!
//! Leaving this page alone is a legitimate choice: a company with no provider
//! configured searches through the platform's managed surface, which is metered
//! and daily-capped and needs no credential from the company at all. Saving a
//! provider here moves those calls onto the company's own account — a change of
//! who is billed, and of which index answers, not a change of whether the agents
//! can search. [`SearchStatus::effective_provider`] is the field that says which
//! of the two is live, because "I picked Exa but pasted no key" and "I picked
//! Exa" must not read identically.
//!
//! # Three things can each be missing, and they fail differently
//!
//! A provider selected with no key; a `search` grant the manifest never made; a
//! build with no agent harness compiled in. The remedies are on three different
//! pages, so the status reports them separately rather than as one "connected"
//! flag.

use axum::extract::State;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::company::search::{
    API_KEY_SECRET, ENDPOINT_SECRET, MANAGED_PROVIDER, PROVIDER_SECRET, SUPPORTED_PROVIDERS,
    effective_provider, provider_requires_endpoint, provider_requires_key, provider_supported,
};
use crate::ports::types::SecretValue;
use crate::server::error::ApiError;
use crate::server::ops::scope::{AdminScopedCompany, ScopedCompany, scoped};

/// The non-secret view of a company's search configuration.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SearchStatus {
    /// The provider the company **selected**. `managed` when it selected
    /// nothing.
    pub provider: String,
    /// The provider the agents actually search through, which differs from
    /// `provider` exactly when the selected one is missing its credential:
    /// selecting Exa and pasting no key searches through `managed`, and a page
    /// that showed only the selection would be reporting a connection that does
    /// not exist.
    pub effective_provider: String,
    /// Whether an API key is stored and non-empty. Never the key itself.
    pub api_key_configured: bool,
    /// The instance URL, for the one provider that is an address rather than an
    /// account (SearXNG). Not a secret — a settings form has to show which
    /// instance it queries.
    pub endpoint: Option<String>,
    /// Whether the selected provider still needs a key before it does anything.
    pub needs_api_key: bool,
    /// Whether the selected provider still needs an endpoint.
    pub needs_endpoint: bool,
    /// Whether this company's manifest **explicitly** grants `search`. A
    /// provider can be fully configured and still reach no agent without it.
    pub granted: bool,
    /// Whether this build has the agent search tools compiled in at all.
    pub in_build: bool,
    /// The providers a company can select.
    pub supported_providers: Vec<String>,
}

/// The ops router for search settings.
pub fn router() -> Router<AppState> {
    scoped("/search", get(get_search).put(put_search))
        .merge(scoped("/search/key", delete(delete_search)))
}

/// Reads a stored secret, treating empty as absent.
async fn read(runtime: &CompanyRuntime, key: &str) -> Result<Option<String>, ApiError> {
    Ok(runtime
        .secrets()
        .get(runtime.id(), key)
        .await?
        .map(|value| value.expose().to_string())
        .filter(|value| !value.trim().is_empty()))
}

/// Writes several secrets, rolling back what already landed if one fails.
///
/// Written one `?` at a time, a store that took the key and then failed on the
/// provider would leave a company searching through one provider's index with
/// another provider's key — an authentication failure whose cause is invisible
/// from the settings page that caused it.
async fn write_all(runtime: &CompanyRuntime, writes: &[(&str, String)]) -> Result<(), ApiError> {
    let mut prior: Vec<(&str, String)> = Vec::new();
    for (key, value) in writes {
        let before = read(runtime, key).await?.unwrap_or_default();
        if let Err(err) = runtime
            .secrets()
            .set(runtime.id(), key, SecretValue(value.clone()))
            .await
        {
            for (done, restore) in &prior {
                if let Err(undo) = runtime
                    .secrets()
                    .set(runtime.id(), done, SecretValue(restore.clone()))
                    .await
                {
                    tracing::error!(
                        company = %runtime.id(),
                        key = done,
                        "[search] a credential write failed and could not be rolled back; this \
                         company is now half configured: {undo}"
                    );
                }
            }
            return Err(ApiError(err));
        }
        prior.push((key, before));
    }
    Ok(())
}

/// Assembles the non-secret status.
async fn status_of(runtime: &CompanyRuntime) -> Result<SearchStatus, ApiError> {
    // The grant lives in the stored manifest, not on the runtime handle. A
    // company that cannot be loaded reports `granted: false` rather than failing
    // the whole status: the operator still needs to see what IS configured, and
    // a settings page that 500s tells them nothing.
    let granted = runtime
        .store()
        .load(runtime.id())
        .await
        .ok()
        .flatten()
        .map(|record| crate::company::grants_search_explicit(&record.manifest.tools.allow))
        .unwrap_or(false);

    let provider = read(runtime, PROVIDER_SECRET)
        .await?
        .unwrap_or_else(|| MANAGED_PROVIDER.to_string());
    let api_key_configured = read(runtime, API_KEY_SECRET).await?.is_some();
    let endpoint = read(runtime, ENDPOINT_SECRET).await?;

    Ok(SearchStatus {
        effective_provider: effective_provider(&provider, api_key_configured, endpoint.is_some())
            .to_string(),
        needs_api_key: provider_requires_key(&provider) && !api_key_configured,
        needs_endpoint: provider_requires_endpoint(&provider) && endpoint.is_none(),
        provider,
        api_key_configured,
        endpoint,
        granted,
        in_build: cfg!(feature = "openhuman"),
        supported_providers: SUPPORTED_PROVIDERS
            .iter()
            .map(|provider| (*provider).to_string())
            .collect(),
    })
}

/// `GET …/search` — non-secret status only.
async fn get_search(company: ScopedCompany) -> Result<Json<SearchStatus>, ApiError> {
    Ok(Json(status_of(&company.runtime).await?))
}

/// The write-only config body. Every field is optional; only fields present and
/// non-empty are applied, so the provider can be switched without re-entering a
/// key that is already stored for it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchConfigBody {
    /// The provider slug. Omit to leave it unchanged.
    #[serde(default)]
    provider: Option<String>,
    /// The provider API key (write-only). Omit to leave it unchanged.
    #[serde(default)]
    api_key: Option<String>,
    /// The instance URL, for SearXNG. Omit to leave it unchanged.
    #[serde(default)]
    endpoint: Option<String>,
}

/// `PUT …/search` — store any supplied settings, return status.
///
/// Requires authority over the company, like every other credential write here.
/// A search key is billed to whoever's account it belongs to, and the provider
/// choice decides which index — and which retention policy — every agent's
/// queries are handed to; neither is an ordinary member's edit.
async fn put_search(
    company: AdminScopedCompany,
    State(_state): State<AppState>,
    Json(body): Json<SearchConfigBody>,
) -> Result<Json<SearchStatus>, ApiError> {
    let runtime = &company.runtime;
    let supplied = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };

    let mut writes: Vec<(&str, String)> = Vec::new();
    if let Some(provider) = supplied(body.provider.as_deref()) {
        let provider = provider.to_ascii_lowercase();
        // Refused here rather than stored and discovered later: a slug this
        // build cannot search through wires no tools at all, and the settings
        // page would still read as configured.
        if !provider_supported(&provider) {
            return Err(ApiError(crate::error::OpenCompanyError::InvalidRequest(
                format!(
                    "`{provider}` is not a search provider this build supports — one of: {}",
                    SUPPORTED_PROVIDERS.join(", ")
                ),
            )));
        }
        writes.push((PROVIDER_SECRET, provider));
    }
    if let Some(api_key) = supplied(body.api_key.as_deref()) {
        writes.push((API_KEY_SECRET, api_key));
    }
    if let Some(endpoint) = supplied(body.endpoint.as_deref()) {
        // A SearXNG instance is queried over the network by every agent that
        // searches, so the address is checked at the door rather than turned
        // into a connection error the operator has to read a log to find.
        if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
            return Err(ApiError(crate::error::OpenCompanyError::InvalidRequest(
                format!("`{endpoint}` is not an http(s) URL"),
            )));
        }
        writes.push((ENDPOINT_SECRET, endpoint));
    }
    write_all(runtime, &writes).await?;

    Ok(Json(status_of(runtime).await?))
}

/// `DELETE …/search/key` — clear the whole connection and fall back to managed.
///
/// The [`SecretStore`](crate::ports::SecretStore) port has no delete, so a
/// cleared credential is stored as the empty string; every read site treats an
/// empty value as unset, and resolution then falls back to managed search.
async fn delete_search(
    company: AdminScopedCompany,
    State(_state): State<AppState>,
) -> Result<Json<SearchStatus>, ApiError> {
    let runtime = &company.runtime;
    // The provider goes with the key, and so does the endpoint. Clearing only
    // the key would leave a selected provider that no longer works, reported as
    // "needs a key" forever; and a stale endpoint would be silently inherited by
    // the next instance URL-less save.
    let cleared: Vec<(&str, String)> = [API_KEY_SECRET, PROVIDER_SECRET, ENDPOINT_SECRET]
        .into_iter()
        .map(|key| (key, String::new()))
        .collect();
    write_all(runtime, &cleared).await?;
    Ok(Json(status_of(runtime).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Route-level, like the hosting tests beside them: the properties worth
    // holding are that a key goes in and never comes back out, that an
    // incomplete selection reports itself as still on managed search, and that
    // an unsupported provider is refused at the door rather than stored.

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::ports::types::CompanyId;

    /// A running company whose manifest grants `search` (or does not).
    async fn state_with_company(home: &std::path::Path, grant_search: bool) -> AppState {
        use crate::ports::CompanyStore;
        use crate::ports::types::CompanyRecord;

        let id = CompanyId::new("acme");
        // Both arms state `[tools]` explicitly. Leaving the ungranted arm
        // empty used to mean "no grant", but the global default belt carries
        // `search` now, so an absent section is a company that *does* grant it
        // — and the ungranted test would have been asserting the opposite of
        // what it set up.
        let allow = if grant_search {
            "\n[tools]\nallow = [\"search\"]\n"
        } else {
            "\n[tools]\nallow = [\"*\"]\n"
        };
        let manifest: crate::company::CompanyManifest = ::toml::from_str(&format!(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n{allow}"
        ))
        .expect("manifest");
        crate::store::FsCompanyStore::new(home.to_path_buf())
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
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .expect("save");

        let runtime = crate::runtime::RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .expect("runtime");
        let state = AppState::new(crate::AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        state
    }

    async fn call(
        state: &AppState,
        method: &str,
        uri: &str,
        cookie: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie);
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .expect("request");
        let response = crate::server::router(state.clone())
            .oneshot(request)
            .await
            .expect("routed");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn an_unconfigured_company_reports_managed_search() {
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        let (status, body) =
            call(&state, "GET", "/api/v1/companies/acme/search", &admin, None).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["provider"], "managed");
        assert_eq!(body["effectiveProvider"], "managed");
        assert_eq!(body["apiKeyConfigured"], false);
        assert_eq!(body["needsApiKey"], false);
        assert_eq!(body["granted"], true);
        assert!(
            body["supportedProviders"]
                .as_array()
                .expect("providers")
                .contains(&json!("exa")),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_saved_key_is_reported_as_configured_and_never_returned() {
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        let (status, saved) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "Exa", "apiKey": "exa_supersecret"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");

        let (_, after) = call(&state, "GET", "/api/v1/companies/acme/search", &admin, None).await;
        // The slug is normalised, and the company now searches through its own
        // account rather than the platform's.
        assert_eq!(after["provider"], "exa");
        assert_eq!(after["effectiveProvider"], "exa");
        assert_eq!(after["apiKeyConfigured"], true);

        // The whole contract of this surface: it reports WHETHER a key is
        // stored, never what it is.
        for rendered in [saved.to_string(), after.to_string()] {
            assert!(!rendered.contains("supersecret"), "{rendered}");
        }
    }

    /// The distinction the page exists to make: a selected provider with no key
    /// is not a connection, and the agents are still on managed search.
    #[tokio::test]
    async fn a_provider_selected_without_its_key_still_reports_managed_as_effective() {
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        let (_, body) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "brave"})),
        )
        .await;

        assert_eq!(body["provider"], "brave", "{body}");
        assert_eq!(body["effectiveProvider"], "managed", "{body}");
        assert_eq!(body["needsApiKey"], true, "{body}");
    }

    #[tokio::test]
    async fn searxng_needs_an_endpoint_and_the_endpoint_must_be_a_url() {
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        let (_, selected) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "searxng"})),
        )
        .await;
        assert_eq!(selected["needsEndpoint"], true, "{selected}");
        assert_eq!(selected["needsApiKey"], false, "{selected}");

        let (status, refused) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"endpoint": "searx.example"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

        let (_, saved) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"endpoint": "https://searx.example"})),
        )
        .await;
        assert_eq!(saved["effectiveProvider"], "searxng", "{saved}");
        assert_eq!(saved["endpoint"], "https://searx.example", "{saved}");
    }

    #[tokio::test]
    async fn a_provider_this_build_cannot_use_is_refused_rather_than_stored() {
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        let (status, body) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "google", "apiKey": "k"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (_, after) = call(&state, "GET", "/api/v1/companies/acme/search", &admin, None).await;
        // The rejected request stored nothing at all — not even the key that
        // came with it.
        assert_eq!(after["provider"], "managed", "{after}");
        assert_eq!(after["apiKeyConfigured"], false, "{after}");
    }

    #[tokio::test]
    async fn clearing_drops_the_provider_and_the_endpoint_too_not_just_the_key() {
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({
                "provider": "searxng",
                "endpoint": "https://searx.example",
            })),
        )
        .await;

        let (status, cleared) = call(
            &state,
            "DELETE",
            "/api/v1/companies/acme/search/key",
            &admin,
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{cleared}");
        assert_eq!(cleared["provider"], "managed", "{cleared}");
        assert_eq!(cleared["endpoint"], Value::Null, "{cleared}");
        assert_eq!(cleared["apiKeyConfigured"], false, "{cleared}");
    }

    #[tokio::test]
    async fn a_configured_provider_without_the_grant_reports_that_it_reaches_nobody() {
        // Both halves can be right and still nothing happens. The status says so
        // separately, because the fix is the manifest rather than this page.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), false).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "exa", "apiKey": "k"})),
        )
        .await;

        let (_, after) = call(&state, "GET", "/api/v1/companies/acme/search", &admin, None).await;

        assert_eq!(after["apiKeyConfigured"], true, "{after}");
        assert_eq!(after["granted"], false, "{after}");
    }

    #[tokio::test]
    async fn switching_providers_leaves_a_stored_key_alone() {
        // A patch, not a replace: an operator switching provider must not have
        // to re-enter a key they can never see again.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "exa", "apiKey": "exa_key"})),
        )
        .await;
        let (_, after) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "querit"})),
        )
        .await;

        assert_eq!(after["provider"], "querit", "{after}");
        assert_eq!(after["apiKeyConfigured"], true, "{after}");
    }
}
