//! The hosting configuration write-plane: store a company's hosting provider
//! API key — **write-only** — and report what is configured without echoing it.
//!
//! `GET …/hosting` returns only [`HostingStatus`]: booleans, the provider slug,
//! and the team scope. The API key is never serialized into any response, by
//! construction. It lives in [`SecretStore`](crate::ports::SecretStore) and this
//! module reads it back only to report *whether* it is there.
//!
//! # Why the team is stored beside the key but is not confidential
//!
//! A team scope is not a secret, and it *is* returned by `GET` — a settings form
//! has to show which account it deploys to, and "Connected ✓" beside the wrong
//! team is exactly the confusion this avoids. It shares the secret store with
//! the key only because the pair is used together.
//!
//! # Three things can each be missing, and they fail differently
//!
//! [`HostingStatus`] reports them separately rather than as one "connected"
//! flag, because the remedies differ: no key means the agents have no hosting
//! tools at all; a missing `hosting` grant means the key is configured and still
//! nothing reaches an agent; and a build without the `openhuman` feature has no
//! hosting tools to wire in the first place. A single boolean would send an
//! operator looking in the wrong place for two of those three.

use axum::extract::State;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::hosting::{API_KEY_SECRET, DEFAULT_PROVIDER, PROVIDER_SECRET, TEAM_SECRET};
use crate::company::runtime::CompanyRuntime;
use crate::ports::types::SecretValue;
use crate::server::error::ApiError;
use crate::server::ops::scope::{AdminScopedCompany, ScopedCompany, scoped};

/// The providers this build can deploy to.
///
/// Kept here rather than derived from the crate so a settings form can render a
/// picker in a build with no hosting tools compiled in at all.
pub const SUPPORTED_PROVIDERS: [&str; 1] = ["vercel"];

/// The non-secret view of a company's hosting configuration.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostingStatus {
    /// Whether an API key is stored and non-empty. Never the key itself.
    pub api_key_configured: bool,
    /// The provider the key belongs to — shown so a settings form can say
    /// *which* provider it is connected to rather than only that it is.
    pub provider: String,
    /// The team, organization, or account scope, when one is set. `None` means
    /// the provider's personal account.
    pub team: Option<String>,
    /// Whether this company's manifest **explicitly** grants `hosting`. A key
    /// can be present and still wire no tools without it.
    pub granted: bool,
    /// Whether this build has the hosting tools compiled in at all.
    pub in_build: bool,
    /// The providers a key can be stored for.
    pub supported_providers: Vec<String>,
}

/// The ops router for hosting settings.
pub fn router() -> Router<AppState> {
    scoped("/hosting", get(get_hosting).put(put_hosting))
        .merge(scoped("/hosting/key", delete(delete_hosting)))
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
/// provider would leave the key stored behind an error response — a company
/// whose settings page reads "not connected" while an agent deploys with the
/// key anyway.
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
                        "[hosting] a credential write failed and could not be rolled back; this \
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
async fn status_of(runtime: &CompanyRuntime) -> Result<HostingStatus, ApiError> {
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
        .map(|record| crate::company::grants_hosting_explicit(&record.manifest.tools.allow))
        .unwrap_or(false);

    Ok(HostingStatus {
        api_key_configured: read(runtime, API_KEY_SECRET).await?.is_some(),
        provider: read(runtime, PROVIDER_SECRET)
            .await?
            .unwrap_or_else(|| DEFAULT_PROVIDER.to_string()),
        team: read(runtime, TEAM_SECRET).await?,
        granted,
        in_build: cfg!(feature = "openhuman"),
        supported_providers: SUPPORTED_PROVIDERS
            .iter()
            .map(|p| (*p).to_string())
            .collect(),
    })
}

/// `GET …/hosting` — non-secret status only.
async fn get_hosting(company: ScopedCompany) -> Result<Json<HostingStatus>, ApiError> {
    Ok(Json(status_of(&company.runtime).await?))
}

/// The write-only config body. Every field is optional; only fields present and
/// non-empty are applied, so the team can be corrected without re-entering the
/// key.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostingConfigBody {
    /// The provider API key (write-only). Omit to leave it unchanged.
    #[serde(default)]
    api_key: Option<String>,
    /// The provider slug. Omit to leave it unchanged.
    #[serde(default)]
    provider: Option<String>,
    /// The team, organization, or account scope. Omit to leave it unchanged.
    #[serde(default)]
    team: Option<String>,
}

/// `PUT …/hosting` — store any supplied credentials, return status.
///
/// Requires authority over the company: this key deploys the company's files to
/// the public internet under its own account and can provision a database it is
/// billed for, so pointing it at a different hosting account is not an ordinary
/// member's edit.
async fn put_hosting(
    company: AdminScopedCompany,
    State(_state): State<AppState>,
    Json(body): Json<HostingConfigBody>,
) -> Result<Json<HostingStatus>, ApiError> {
    let runtime = &company.runtime;
    let supplied = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };

    let mut writes: Vec<(&str, String)> = Vec::new();
    if let Some(api_key) = supplied(body.api_key.as_deref()) {
        writes.push((API_KEY_SECRET, api_key));
    }
    if let Some(provider) = supplied(body.provider.as_deref()) {
        let provider = provider.to_ascii_lowercase();
        // Refused here rather than stored and discovered later: a slug this
        // build cannot connect to would wire no tools at all, and the settings
        // page would still read "Connected".
        if !SUPPORTED_PROVIDERS.contains(&provider.as_str()) {
            return Err(ApiError(crate::error::OpenCompanyError::InvalidRequest(
                format!(
                    "`{provider}` is not a hosting provider this build supports — one of: {}",
                    SUPPORTED_PROVIDERS.join(", ")
                ),
            )));
        }
        writes.push((PROVIDER_SECRET, provider));
    }
    if let Some(team) = supplied(body.team.as_deref()) {
        writes.push((TEAM_SECRET, team));
    }
    write_all(runtime, &writes).await?;

    Ok(Json(status_of(runtime).await?))
}

/// `DELETE …/hosting/key` — clear every stored hosting credential.
///
/// The [`SecretStore`](crate::ports::SecretStore) port has no delete, so a
/// cleared credential is stored as the empty string; every read site treats an
/// empty value as unset, and the tools then fail closed.
async fn delete_hosting(
    company: AdminScopedCompany,
    State(_state): State<AppState>,
) -> Result<Json<HostingStatus>, ApiError> {
    let runtime = &company.runtime;
    // Together, and including the team: a clear that dropped the key but left a
    // team behind would report the integration as disconnected while a later
    // key, saved without a team, silently inherited the old account scope.
    let cleared: Vec<(&str, String)> = [API_KEY_SECRET, PROVIDER_SECRET, TEAM_SECRET]
        .into_iter()
        .map(|key| (key, String::new()))
        .collect();
    write_all(runtime, &cleared).await?;
    Ok(Json(status_of(runtime).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // These drive the real router rather than the helpers, because the
    // properties worth holding are route-level: that a credential goes in and
    // never comes back out, that clearing clears all of it, and that a provider
    // this build cannot use is refused at the door rather than stored.

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::ports::types::CompanyId;

    /// A running company with a manifest that grants `hosting`.
    async fn state_with_company(home: &std::path::Path, grant_hosting: bool) -> AppState {
        use crate::ports::CompanyStore;
        use crate::ports::types::CompanyRecord;

        let id = CompanyId::new("acme");
        let allow = if grant_hosting {
            "\n[tools]\nallow = [\"hosting\"]\n"
        } else {
            ""
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
    async fn a_saved_token_is_reported_as_configured_and_never_returned() {
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        let (status, before) = call(
            &state,
            "GET",
            "/api/v1/companies/acme/hosting",
            &admin,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{before}");
        assert_eq!(before["apiKeyConfigured"], false);
        // The default provider is reported before anything is stored, so a
        // settings form can render its picker.
        assert_eq!(before["provider"], "vercel");
        assert_eq!(before["granted"], true);

        let (status, saved) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/hosting",
            &admin,
            Some(json!({
                "apiKey": "vercel_live_supersecret",
                "provider": "Vercel",
                "team": " team_abc ",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");

        let (_, after) = call(
            &state,
            "GET",
            "/api/v1/companies/acme/hosting",
            &admin,
            None,
        )
        .await;
        assert_eq!(after["apiKeyConfigured"], true);
        // The provider is normalised, and the team — the one non-secret field —
        // comes back trimmed.
        assert_eq!(after["provider"], "vercel");
        assert_eq!(after["team"], "team_abc");

        // The whole contract of this surface: it reports WHETHER a token is
        // stored, never what it is. A response that echoed it back would put it
        // in the browser, the network log, and any screen share.
        for rendered in [saved.to_string(), after.to_string()] {
            assert!(!rendered.contains("supersecret"), "{rendered}");
        }
    }

    #[tokio::test]
    async fn a_provider_this_build_cannot_use_is_refused_rather_than_stored() {
        // Stored, it would leave the settings page reading "Connected" over a
        // slug that wires no tools at all.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        let (status, body) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/hosting",
            &admin,
            Some(json!({"apiKey": "k", "provider": "heroku"})),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (_, after) = call(
            &state,
            "GET",
            "/api/v1/companies/acme/hosting",
            &admin,
            None,
        )
        .await;
        // The rejected request stored nothing at all — not even the key that
        // came with it.
        assert_eq!(after["apiKeyConfigured"], false, "{after}");
        assert_eq!(after["provider"], "vercel");
    }

    #[tokio::test]
    async fn clearing_removes_the_team_too_not_just_the_token() {
        // The route is named `…/key`, which reads as if it clears only the
        // token. A team left behind would be silently inherited by the next
        // token saved without one — deploying into the previous account's scope.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        call(
            &state,
            "PUT",
            "/api/v1/companies/acme/hosting",
            &admin,
            Some(json!({"apiKey": "vercel_key", "team": "team_abc"})),
        )
        .await;

        let (status, cleared) = call(
            &state,
            "DELETE",
            "/api/v1/companies/acme/hosting/key",
            &admin,
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{cleared}");
        assert_eq!(cleared["apiKeyConfigured"], false);
        assert_eq!(cleared["team"], Value::Null, "{cleared}");
        assert_eq!(cleared["provider"], "vercel", "{cleared}");
    }

    #[tokio::test]
    async fn a_stored_token_without_the_grant_reports_that_it_reaches_nobody() {
        // Both halves can be right and still nothing happens. The status says so
        // separately, because the fix is the manifest rather than this page.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), false).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        call(
            &state,
            "PUT",
            "/api/v1/companies/acme/hosting",
            &admin,
            Some(json!({"apiKey": "vercel_key"})),
        )
        .await;

        let (_, after) = call(
            &state,
            "GET",
            "/api/v1/companies/acme/hosting",
            &admin,
            None,
        )
        .await;

        assert_eq!(after["apiKeyConfigured"], true);
        assert_eq!(after["granted"], false, "{after}");
    }

    #[tokio::test]
    async fn saving_a_team_alone_leaves_the_token_alone() {
        // A patch, not a replace: an operator correcting a team must not have to
        // re-enter a token they can never see again.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        call(
            &state,
            "PUT",
            "/api/v1/companies/acme/hosting",
            &admin,
            Some(json!({"apiKey": "vercel_key"})),
        )
        .await;
        let (_, after) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/hosting",
            &admin,
            Some(json!({"team": "team_xyz"})),
        )
        .await;

        assert_eq!(after["apiKeyConfigured"], true, "{after}");
        assert_eq!(after["team"], "team_xyz");
    }
}
