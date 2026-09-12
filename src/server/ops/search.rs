//! The search configuration write-plane: which providers a company has
//! connected, which one its agents search through, and the credential behind
//! each — **write-only**.
//!
//! `GET …/search` returns only [`SearchStatus`]: slugs, booleans, and the
//! non-secret endpoints. No API key is ever serialized into any response, by
//! construction. Keys live in [`SecretStore`](crate::ports::SecretStore) and
//! this module reads them back only to report *whether* they are there.
//!
//! # Why this is a list and not a field
//!
//! There used to be one `search/api_key` for the whole company and a separate
//! `search/provider` field selecting which API it was presented to. Switching
//! provider without re-pasting the key left the old key authenticating against
//! the new provider — and this route, the console badge and the harness all
//! agreed the company was correctly configured until an agent's first search
//! came back 401. One credential per provider slug makes that unrepresentable.
//! See [`crate::company::search::store`] and
//! `docs/modules/search/current-state.md`.
//!
//! # Why "not configured" is a working state and not an error
//!
//! Leaving this page alone is a legitimate choice: a company with no provider
//! configured searches through the platform's managed surface, which is metered
//! and daily-capped and needs no credential from the company at all. Connecting
//! a provider here moves those calls onto the company's own account — a change
//! of who is billed, and of which index answers, not a change of whether the
//! agents can search. [`SearchStatus::effective_provider`] is the field that
//! says which of the two is live, because "I connected Exa but pasted no key"
//! and "I connected Exa" must not read identically.
//!
//! # Three things can each be missing, and they fail differently
//!
//! A provider connected with no key; a `search` grant the manifest never made; a
//! build with no agent harness compiled in. The remedies are on three different
//! pages, so the status reports them separately rather than as one "connected"
//! flag.
//!
//! # Authority
//!
//! Reads are [`ScopedCompany`]; **every write and the probe are
//! [`AdminScopedCompany`]**. A search key is billed to whoever's account it
//! belongs to, and the provider choice decides which index — and which retention
//! policy — every agent's queries are handed to. The probe is an admin action
//! for two further reasons: it spends the company's money (no search provider
//! offers a free credential validator) and, for a self-hosted instance, it
//! fetches an operator-supplied address that is allowed to be on a private
//! network.

use axum::extract::{Path, State};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::company::search::catalogue::{self, SearchProviderInfo};
use crate::company::search::probe::{self, ProbeClass};
use crate::company::search::resolve::Candidate;
use crate::company::search::store::{self, SearchProvider};
use crate::company::search::{
    API_KEY_SECRET, ENDPOINT_SECRET, MANAGED_PROVIDER, PROVIDER_SECRET, SUPPORTED_PROVIDERS,
    provider_requires_endpoint, provider_requires_key, resolve,
};
use crate::ports::types::SecretValue;
use crate::server::error::ApiError;
use crate::server::ops::scope::{AdminScopedCompany, ScopedCompany, scoped};

/// One connected provider, as the console sees it.
///
/// Carries `key_configured` and **never** a key. The boolean is derived by
/// asking the store whether a non-empty value exists, never by reading a flag
/// that could go stale against a cleared secret.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SearchProviderView {
    /// The catalogue slug. Identity and address at once.
    pub slug: String,
    /// Display name, from the catalogue.
    pub label: String,
    /// `account` or `self-hosted` — which of the two questions it answers.
    pub category: String,
    /// Whether it is eligible to be the provider agents search through.
    pub enabled: bool,
    /// Whether a credential is stored. **Never the credential.**
    pub key_configured: bool,
    /// Whether this kind of provider takes a key at all. `false` for SearXNG,
    /// which is how the console knows not to offer "Remove key" on a row that
    /// has none.
    pub takes_key: bool,
    /// Whether this kind of provider takes an instance address.
    pub takes_endpoint: bool,
    /// The instance address, for a self-hosted provider. Not a secret.
    pub endpoint: Option<String>,
    /// Whether this provider has everything it needs to answer a search.
    pub complete: bool,
    /// Whether this is the provider agents search through.
    ///
    /// The **resolved** answer rather than the raw marker, so a row can say
    /// "Default" without the console knowing whether it was chosen or inherited
    /// — the operator sees the same answer either way.
    pub is_default: bool,
}

/// The non-secret view of a company's search configuration.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SearchStatus {
    /// The provider the company **selected**, kept for compatibility with the
    /// single-slot shape: the active provider's slug, or `managed`.
    pub provider: String,
    /// The provider the agents actually search through. Differs from `provider`
    /// exactly when the active one is missing its credential.
    pub effective_provider: String,
    /// Every connected provider.
    pub providers: Vec<SearchProviderView>,
    /// Whether a key is stored for the active provider. Never the key.
    pub api_key_configured: bool,
    /// The active provider's instance URL, where it has one.
    pub endpoint: Option<String>,
    /// Whether the active provider still needs a key.
    pub needs_api_key: bool,
    /// Whether the active provider still needs an endpoint.
    pub needs_endpoint: bool,
    /// Whether this company's manifest **explicitly** grants `search`.
    pub granted: bool,
    /// Whether this build has the agent search tools compiled in at all.
    pub in_build: bool,
    /// Whether the platform's own managed search credential resolves on this
    /// deployment.
    ///
    /// The Managed row is rendered from this rather than from a permanent
    /// "Always on" badge. Managed search is always the *fallback*, which is a
    /// different claim from always *working*: a self-hosted deployment with no
    /// platform credential falls back to a surface that answers nothing, and
    /// that badge would be the one claim on this page an operator most needs to
    /// be true.
    pub managed_configured: bool,
    /// The company's daily managed-search ceiling.
    pub managed_daily_call_cap: u32,
    /// The providers a company can connect.
    pub supported_providers: Vec<String>,
}

/// The ops router for search settings.
pub fn router() -> Router<AppState> {
    scoped("/search", get(get_search).put(put_search))
        .merge(scoped("/search/key", delete(delete_search)))
        .merge(scoped("/search/providers", post(connect_provider)))
        .merge(scoped(
            "/search/providers/{slug}",
            put(update_provider).delete(remove_provider),
        ))
        .merge(scoped("/search/providers/{slug}/key", put(replace_key)))
        .merge(scoped("/search/default", put(set_default)))
        .merge(scoped("/search/test", post(test_provider)))
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
                        company = %runtime.id(),                        key = done,
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

/// Whether the platform's own managed search credential resolves here.
fn managed_configured() -> bool {
    #[cfg(feature = "openhuman")]
    {
        use crate::app::config::ProcessEnv;
        crate::harness::built_in::provider::search_backend_from_env(&ProcessEnv).is_some()
    }
    #[cfg(not(feature = "openhuman"))]
    {
        false
    }
}

/// A bad-request error with `message`.
fn invalid(message: impl Into<String>) -> ApiError {
    ApiError(crate::error::OpenCompanyError::InvalidRequest(
        message.into(),
    ))
}

/// The longest instance address this will store.
///
/// An address is an operator-supplied value that becomes a stored secret and is
/// rendered back on every page load. A sibling surface learned the hard way that
/// an unbounded operator-supplied string reaching the store is a way to leave a
/// row that cannot be deleted; a cap costs nothing and closes the class.
const MAX_ENDPOINT_LEN: usize = 2048;

/// Whether `slug` is safe to build a secret-store key out of.
///
/// Credential addresses are `search/provider/<slug>/key`, so a slug carrying a
/// slash, a control character or an unbounded run of text is a slug that writes
/// somewhere other than where it claims. Every route that *adds* something
/// checks catalogue membership, which is stricter; this exists for the routes
/// that address an existing row, where refusing a slug the catalogue no longer
/// knows would leave the operator unable to delete it.
fn slug_is_addressable(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 32
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The catalogue entry for `slug`, or a refusal naming what this build knows.
///
/// Refused here rather than stored and discovered later: a slug this build
/// cannot search through wires no tools at all, and the settings page would
/// still read as connected.
fn catalogue_entry(slug: &str) -> Result<&'static SearchProviderInfo, ApiError> {
    catalogue::entry(slug).ok_or_else(|| {
        invalid(format!(
            "`{slug}` is not a search provider this build supports — one of: {}",
            SUPPORTED_PROVIDERS.join(", ")
        ))
    })
}

/// Assembles the non-secret status.
async fn status_of(runtime: &CompanyRuntime) -> Result<SearchStatus, ApiError> {
    // The grant lives in the stored manifest, not on the runtime handle. A
    // company that cannot be loaded reports `granted: false` rather than failing
    // the whole status: the operator still needs to see what IS configured, and
    // a settings page that 500s tells them nothing.
    let record = runtime.store().load(runtime.id()).await.ok().flatten();
    let granted = record
        .as_ref()
        .map(|record| crate::company::grants_search_explicit(&record.manifest.tools.allow))
        .unwrap_or(false);
    let managed_daily_call_cap = record
        .as_ref()
        .and_then(|record| record.manifest.tools.search_daily_calls)
        .unwrap_or(crate::company::DEFAULT_SEARCH_DAILY_CALLS);

    let candidates =
        crate::company::search::candidates(runtime.id(), runtime.secrets().as_ref()).await?;
    let marked = store::load_default_slug(runtime.id(), runtime.secrets().as_ref()).await?;
    let active = resolve::active(&candidates, marked.as_deref());
    let active_slug = active.map(|candidate| candidate.provider.slug.clone());

    let providers = candidates
        .iter()
        .map(|candidate| view_of(candidate, active_slug.as_deref()))
        .collect();

    let effective = resolve::effective_slug(active).to_string();

    // `provider` is the SELECTION and `effective_provider` is what answers, and
    // the two must not be collapsed: "I connected Exa and pasted no key" has to
    // read differently from "I connected nothing". So the selection is the
    // marked slug when there is one — even when it resolves to nothing — and
    // only falls back to the active provider for a company that never marked
    // one.
    let selected = marked
        .clone()
        .filter(|slug| candidates.iter().any(|c| &c.provider.slug == slug))
        .or_else(|| active_slug.clone());
    let selected_candidate = selected
        .as_deref()
        .and_then(|slug| candidates.iter().find(|c| c.provider.slug == slug));

    let (api_key_configured, endpoint, needs_api_key, needs_endpoint) = match selected_candidate {
        Some(candidate) => (
            candidate.has_key,
            candidate.provider.endpoint.clone(),
            provider_requires_key(&candidate.provider.slug) && !candidate.has_key,
            provider_requires_endpoint(&candidate.provider.slug)
                && candidate.provider.endpoint.is_none(),
        ),
        None => (false, None, false, false),
    };

    Ok(SearchStatus {
        provider: selected.unwrap_or_else(|| MANAGED_PROVIDER.to_string()),
        effective_provider: effective,
        providers,
        api_key_configured,
        endpoint,
        needs_api_key,
        needs_endpoint,
        granted,
        in_build: cfg!(feature = "openhuman"),
        managed_configured: managed_configured(),
        managed_daily_call_cap,
        supported_providers: SUPPORTED_PROVIDERS
            .iter()
            .map(|provider| (*provider).to_string())
            .collect(),
    })
}

/// One row, from a candidate.
fn view_of(candidate: &Candidate, active_slug: Option<&str>) -> SearchProviderView {
    let slug = candidate.provider.slug.clone();
    let info = catalogue::entry(&slug);
    SearchProviderView {
        label: info
            .map(|info| info.label.to_string())
            .unwrap_or_else(|| slug.clone()),
        category: info
            .map(|info| info.category.as_str().to_string())
            .unwrap_or_else(|| "account".to_string()),
        enabled: candidate.provider.enabled,
        key_configured: candidate.has_key,
        takes_key: provider_requires_key(&slug),
        takes_endpoint: provider_requires_endpoint(&slug),
        endpoint: candidate.provider.endpoint.clone(),
        complete: candidate.is_complete(),
        is_default: active_slug == Some(slug.as_str()),
        slug,
    }
}

/// `GET …/search` — non-secret status only.
async fn get_search(company: ScopedCompany) -> Result<Json<SearchStatus>, ApiError> {
    Ok(Json(status_of(&company.runtime).await?))
}

/// What a connect or test attempt came back with.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectOutcome {
    /// Whether the provider answered.
    pub ok: bool,
    /// The probe class when it did not. `null` on success.
    pub probe_class: Option<String>,
    /// One sentence for the operator. **Never carries the upstream body**, which
    /// can echo request material including fragments of the credential.
    pub message: Option<String>,
    /// Whether the record and the credential were kept. Only an auth-class
    /// failure is destructive.
    pub saved: bool,
    /// The status after the attempt.
    pub status: SearchStatus,
}

/// The body for connecting a provider.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectBody {
    /// The catalogue slug.
    slug: String,
    /// The API key (write-only). Required for an account provider.
    #[serde(default)]
    api_key: Option<String>,
    /// The instance URL. Required for a self-hosted provider.
    #[serde(default)]
    endpoint: Option<String>,
}

/// Trims a supplied value and treats empty as absent.
fn supplied(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Validates a draft against what its kind needs, returning the endpoint to use.
async fn validate_draft(
    info: &SearchProviderInfo,
    api_key: Option<&str>,
    endpoint: Option<&str>,
) -> Result<Option<String>, ApiError> {
    if info.needs_key() && api_key.is_none() {
        return Err(invalid(format!("{} needs an API key", info.label)));
    }
    if !info.needs_endpoint() {
        return Ok(None);
    }
    let endpoint =
        endpoint.ok_or_else(|| invalid(format!("{} needs an instance address", info.label)))?;
    validate_endpoint(endpoint).await?;
    Ok(Some(endpoint.to_string()))
}

/// Every check an operator-supplied instance address must pass before it is
/// stored or fetched.
///
/// **One function, because there are four write paths and they disagreed.**
/// `POST …/search/providers` ran all four of these; the two halves of the
/// legacy `PUT …/search` ran the last two, and `POST …/search/test` ran only
/// the last. So the compatibility route stored an address the modern route
/// refuses — including one past [`MAX_ENDPOINT_LEN`], which is the case that
/// comment was written about: an unbounded operator-supplied string reaching
/// the store is how a row that cannot be deleted gets made.
///
/// A validator that is a habit rather than a function is one that gets applied
/// unevenly, and unevenly is how this one was.
async fn validate_endpoint(endpoint: &str) -> Result<(), ApiError> {
    if endpoint.len() > MAX_ENDPOINT_LEN {
        return Err(invalid("that instance address is too long"));
    }
    if endpoint.chars().any(char::is_control) {
        return Err(invalid(
            "that instance address contains a control character",
        ));
    }
    // Checked at the door rather than turned into a connection error the
    // operator has to read a log to find.
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err(invalid(format!("`{endpoint}` is not an http(s) URL")));
    }
    probe::guard_instance_url(endpoint).map_err(invalid)?;
    // And what the name resolves to, not just how it is spelled. The literal
    // guard above judges `http://169.254.169.254/`; only this judges
    // `http://metadata.example/`, and this route may STORE the address without
    // ever probing it — after which the search tool resolves it at agent-turn
    // time and fetches whatever it points at. A name that does not resolve is
    // deliberately not refused; see `guard_resolved_host`.
    probe::guard_resolved_host(endpoint).await.map_err(invalid)
}

/// Runs the check and reports what it means, without touching the store.
async fn check(
    info: &SearchProviderInfo,
    api_key: Option<&str>,
    endpoint: Option<&str>,
) -> Option<(ProbeClass, String)> {
    match probe::probe(info, api_key, endpoint).await {
        Ok(()) => None,
        Err(failure) => {
            let class = probe::classify(info.slug, &failure);
            // Neither the response NOR the log gets the raw failure. The body
            // can echo request material including fragments of the credential,
            // and a log is a second durable copy of it — kept longer and read
            // by more people than the banner that reasoning was written about.
            // `log_detail` keeps the part a person acts on and withholds the
            // body; `class` beside it is what the body was read for.
            tracing::info!(
                provider = info.slug,
                class = class.as_str(),
                detail = %probe::log_detail(&failure),
                "[search] connectivity check failed"
            );
            Some((class, probe::describe(class, info.label)))
        }
    }
}

/// `POST …/search/providers` — connect one provider.
///
/// The ordering is not arbitrary. Local validation first, so nothing is written
/// for a draft that cannot work. Then the record, claimed test-and-set under
/// the store's lock so that two admins connecting the same provider at once
/// cannot both proceed — and before the credential, so the one that is refused
/// has written nothing to overwrite the winner's key with. Then the credential,
/// then the probe, which is handed the key rather than reading it back. Only an
/// auth-class failure rolls both back.
async fn connect_provider(
    company: AdminScopedCompany,
    State(_state): State<AppState>,
    Json(body): Json<ConnectBody>,
) -> Result<Json<ConnectOutcome>, ApiError> {
    let runtime = &company.runtime;
    let slug = body.slug.trim().to_ascii_lowercase();
    let info = catalogue_entry(&slug)?;

    let api_key = supplied(body.api_key.as_deref());
    let endpoint = validate_draft(
        info,
        api_key.as_deref(),
        supplied(body.endpoint.as_deref()).as_deref(),
    )
    .await?;

    // Test-and-set under the store's own lock, and **before** the credential.
    //
    // It used to be a read here and a write three awaits later, so two admins
    // connecting the same provider at once both got past it. The loser then did
    // real damage rather than duplicating work: an `Auth` probe failure rolls
    // back by deleting the row *and* the credential, so a request whose key was
    // rejected deleted the row and the working key the other request had just
    // stored — while that request answered `saved: true` from its own
    // request-local copy.
    //
    // Before the credential, because a loser must not have written anything by
    // the time it is refused. The order the comment above describes is
    // unaffected: the probe is handed the key rather than reading it back.
    if !store::claim_provider(
        runtime.id(),
        runtime.secrets().as_ref(),
        SearchProvider {
            slug: slug.clone(),
            enabled: true,
            endpoint: endpoint.clone(),
        },
    )
    .await?
    {
        return Err(invalid(format!(
            "{} is already connected — replace its key instead",
            info.label
        )));
    }

    if let Some(key) = api_key.as_deref() {
        store::store_provider_key(runtime.id(), runtime.secrets().as_ref(), &slug, key).await?;
    }

    let failure = check(info, api_key.as_deref(), endpoint.as_deref()).await;
    let (ok, probe_class, message, saved) = match failure {
        None => (true, None, None, true),
        Some((class, message)) if probe::destroys_credential(class) => {
            // Roll back both stores. A failed rollback is logged rather than
            // swallowed: a credential left behind for a provider with no record
            // is an orphaned secret.
            if let Err(err) =
                store::delete_provider(runtime.id(), runtime.secrets().as_ref(), &slug).await
            {
                tracing::error!(
                    company = %runtime.id(),                    provider = %slug,
                    "[search] a rejected credential could not be rolled back: {err}"
                );
            }
            (false, Some(class), Some(message), false)
        }
        Some((class, message)) => (false, Some(class), Some(message), true),
    };

    Ok(Json(ConnectOutcome {
        ok,
        probe_class: probe_class.map(|class| class.as_str().to_string()),
        message,
        saved,
        status: status_of(runtime).await?,
    }))
}

/// The `{slug}` capture, as a named struct.
///
/// **Not `Path<String>`.** Every route here is registered by
/// [`scoped`](super::scope::scoped), which serves both the platform form
/// (`…/companies/{id}/search/providers/{slug}`) and the single-company alias
/// (`…/company/search/providers/{slug}`). The platform form therefore captures
/// *two* parameters, and a `Path<String>` under it fails extraction with "wrong
/// number of path parameters" — a 400 on every call, from the console as well
/// as from a test. A named struct deserializes by key and works under both
/// shapes, which is why every other ops module with a path parameter uses one.
#[derive(Debug, Deserialize)]
struct SlugPath {
    slug: String,
}

/// The body for changing one provider.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateBody {
    /// Turn the provider on or off. Omit to leave it unchanged.
    #[serde(default)]
    enabled: Option<bool>,
    /// A new instance address, for a self-hosted provider.
    #[serde(default)]
    endpoint: Option<String>,
}

/// `PUT …/search/providers/{slug}` — enable, disable, or re-address.
async fn update_provider(
    company: AdminScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
    State(_state): State<AppState>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<SearchStatus>, ApiError> {
    let runtime = &company.runtime;
    let slug = slug.trim().to_ascii_lowercase();
    let info = catalogue_entry(&slug)?;

    if !store::list_providers(runtime.id(), runtime.secrets().as_ref())
        .await?
        .iter()
        .any(|provider| provider.slug == slug)
    {
        return Err(invalid(format!("{} is not connected", info.label)));
    }

    if let Some(endpoint) = supplied(body.endpoint.as_deref()) {
        let endpoint = validate_draft(info, Some("unused"), Some(&endpoint)).await?;
        // The read of the current row and the write back are one critical
        // section in the store. Split — as they were here, with the `enabled`
        // flag read above and written below — a removal landing between them
        // made this **recreate** the row: a provider the operator had just
        // disconnected came back enabled with a fresh address and started
        // receiving agent searches again, after the removal had answered 200.
        if !store::update_endpoint_if_present(
            runtime.id(),
            runtime.secrets().as_ref(),
            &slug,
            endpoint,
        )
        .await?
        {
            return Err(invalid(format!("{} is not connected", info.label)));
        }
    }
    if let Some(enabled) = body.enabled {
        store::set_enabled(runtime.id(), runtime.secrets().as_ref(), &slug, enabled).await?;
    }

    Ok(Json(status_of(runtime).await?))
}

/// `DELETE …/search/providers/{slug}` — remove a provider and its credential.
async fn remove_provider(
    company: AdminScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
    State(_state): State<AppState>,
) -> Result<Json<SearchStatus>, ApiError> {
    let runtime = &company.runtime;
    let slug = slug.trim().to_ascii_lowercase();
    // Checked rather than looked up: a row whose slug this build no longer has
    // in its catalogue must still be removable, but a slug that could address
    // something other than its own credential must not reach the store.
    if !slug_is_addressable(&slug) {
        return Err(invalid("that is not a provider slug"));
    }
    store::delete_provider(runtime.id(), runtime.secrets().as_ref(), &slug).await?;
    Ok(Json(status_of(runtime).await?))
}

/// The body for replacing one provider's credential.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyBody {
    /// The new key (write-only). An empty value clears it.
    #[serde(default)]
    api_key: Option<String>,
}

/// `PUT …/search/providers/{slug}/key` — replace or clear one credential.
///
/// Clearing is a write of the empty string: the store has no delete, and every
/// read site treats an empty value as unset.
async fn replace_key(
    company: AdminScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
    State(_state): State<AppState>,
    Json(body): Json<KeyBody>,
) -> Result<Json<SearchStatus>, ApiError> {
    let runtime = &company.runtime;
    let slug = slug.trim().to_ascii_lowercase();
    let info = catalogue_entry(&slug)?;
    if !info.needs_key() {
        return Err(invalid(format!("{} does not take an API key", info.label)));
    }
    // **Replace**, so there has to be something to replace. Without this a
    // direct `PUT …/search/providers/exa/key` for a provider with no row wrote
    // a credential to `search/provider/exa/key` that the status route never
    // reports and `DELETE …/search/key` never clears — its loop visits indexed
    // providers only. An invisible credential the operator cannot see and
    // cannot delete is the orphaned-secret shape this module keeps refusing
    // elsewhere, and it does not get an exception here.
    //
    // Checked and written in one critical section rather than two: a removal
    // landing between them would leave the credential at an address absent from
    // the index, which the status route never reports and `DELETE …/search/key`
    // never clears, because both walk the index.
    let key = supplied(body.api_key.as_deref()).unwrap_or_default();
    if !store::store_key_if_connected(runtime.id(), runtime.secrets().as_ref(), &slug, &key).await?
    {
        return Err(invalid(format!(
            "{} is not connected — connect it instead",
            info.label
        )));
    }
    Ok(Json(status_of(runtime).await?))
}

/// The body for marking the default provider.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DefaultBody {
    /// The slug to mark, or `null`/empty to unmark and fall back to the first
    /// enabled provider.
    #[serde(default)]
    slug: Option<String>,
}

/// `PUT …/search/default` — mark which provider the agents search through.
async fn set_default(
    company: AdminScopedCompany,
    State(_state): State<AppState>,
    Json(body): Json<DefaultBody>,
) -> Result<Json<SearchStatus>, ApiError> {
    let runtime = &company.runtime;
    match supplied(body.slug.as_deref()) {
        Some(slug) => {
            let slug = slug.to_ascii_lowercase();
            let info = catalogue_entry(&slug)?;
            if !store::list_providers(runtime.id(), runtime.secrets().as_ref())
                .await?
                .iter()
                .any(|provider| provider.slug == slug)
            {
                return Err(invalid(format!("{} is not connected", info.label)));
            }
            store::set_default_slug(runtime.id(), runtime.secrets().as_ref(), &slug).await?;
        }
        None => store::clear_default_slug(runtime.id(), runtime.secrets().as_ref()).await?,
    }
    Ok(Json(status_of(runtime).await?))
}

/// The body for a connectivity check.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestBody {
    /// The provider to check.
    slug: String,
    /// A key to check **without storing it** — testing a credential and
    /// committing to it are separate acts. Omit to check the stored one.
    #[serde(default)]
    api_key: Option<String>,
    /// An address to check without storing it.
    #[serde(default)]
    endpoint: Option<String>,
}

/// `POST …/search/test` — check a draft or a stored provider.
///
/// `AdminScopedCompany`, not `ScopedCompany`: the check spends the company's
/// money — no search provider publishes a free credential validator — and for a
/// self-hosted provider it fetches an operator-supplied address that is allowed
/// to be on a private network.
async fn test_provider(
    company: AdminScopedCompany,
    State(_state): State<AppState>,
    Json(body): Json<TestBody>,
) -> Result<Json<ConnectOutcome>, ApiError> {
    let runtime = &company.runtime;
    let slug = body.slug.trim().to_ascii_lowercase();
    let info = catalogue_entry(&slug)?;

    let api_key = match supplied(body.api_key.as_deref()) {
        Some(key) => Some(key),
        None => store::load_provider_key(runtime.id(), runtime.secrets().as_ref(), &slug).await?,
    };
    let endpoint = match supplied(body.endpoint.as_deref()) {
        Some(endpoint) => Some(endpoint),
        None => store::list_providers(runtime.id(), runtime.secrets().as_ref())
            .await?
            .into_iter()
            .find(|provider| provider.slug == slug)
            .and_then(|provider| provider.endpoint),
    };
    if let Some(endpoint) = endpoint.as_deref() {
        validate_endpoint(endpoint).await?;
    }

    let failure = check(info, api_key.as_deref(), endpoint.as_deref()).await;
    Ok(Json(ConnectOutcome {
        ok: failure.is_none(),
        probe_class: failure
            .as_ref()
            .map(|(class, _)| class.as_str().to_string()),
        // A test never changes the store, so the save-flavoured sentences
        // `describe` produces would be wrong here. The class is what the console
        // renders from; this is the fallback for anything that does not.
        message: failure.as_ref().map(|(_, message)| message.clone()),
        saved: true,
        status: status_of(runtime).await?,
    }))
}

/// The write-only config body for the single-slot route. Every field is
/// optional; only fields present and non-empty are applied.
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

/// `PUT …/search` — the single-slot route, kept and re-expressed over the list.
///
/// It predates the provider list and other callers may still hold it, so it
/// stays. What it must **not** do any more is write the flat keys directly: that
/// is the write half of the bug this rework exists to fix, and it would also
/// fight the convergence rule in [`store`]. So it is now sugar over the list —
/// connect-or-update the named provider, store its key at *its own* address, and
/// mark it as the default, which is what selecting a provider always meant.
async fn put_search(
    company: AdminScopedCompany,
    State(_state): State<AppState>,
    Json(body): Json<SearchConfigBody>,
) -> Result<Json<SearchStatus>, ApiError> {
    let runtime = &company.runtime;

    let Some(provider) = supplied(body.provider.as_deref()).map(|p| p.to_ascii_lowercase()) else {
        // No provider named: apply the key or address to the SELECTED provider,
        // not to the effective one. They differ precisely in the case this
        // branch exists to serve — "I picked SearXNG, now here is its address" —
        // where the selection resolves to nothing yet and the effective provider
        // is still managed. Reading the effective one here would refuse the
        // request that completes the configuration.
        let selected =
            match store::load_default_slug(runtime.id(), runtime.secrets().as_ref()).await? {
                Some(slug) => Some(slug),
                None => store::list_providers(runtime.id(), runtime.secrets().as_ref())
                    .await?
                    .into_iter()
                    .next()
                    .map(|provider| provider.slug),
            };
        let Some(selected) = selected else {
            return Err(invalid("no provider is connected to apply that to"));
        };
        return apply_to(runtime, &selected, &body).await;
    };

    if provider == MANAGED_PROVIDER {
        // Selecting managed has always meant "stop using my own account", and
        // the honest expression of that is still NOT to store `managed` as
        // though it were a connection.
        //
        // Unmarking the default is not it either, though, which is what this
        // branch used to do alone. [`resolve::active`] reads an absent marker as
        // "the first usable provider", so a company with any usable connection
        // kept searching through it while this route answered 200 — and worst
        // for exactly the configurations this route exists to serve, since an
        // upgraded legacy company has no marker for the clear to remove.
        //
        // So the connections are switched off. Managed search is what the
        // absence of everything else means, and leaving nothing in the way is
        // the only representation of that this model has. Nothing is destroyed:
        // every credential and address stays where it is, the rows stay on the
        // page reading as off, and naming one of them again turns it back on.
        for connected in store::list_providers(runtime.id(), runtime.secrets().as_ref()).await? {
            if connected.enabled {
                store::set_enabled(
                    runtime.id(),
                    runtime.secrets().as_ref(),
                    &connected.slug,
                    false,
                )
                .await?;
            }
        }
        store::clear_default_slug(runtime.id(), runtime.secrets().as_ref()).await?;
        return Ok(Json(status_of(runtime).await?));
    }

    let info = catalogue_entry(&provider)?;
    let connected = store::list_providers(runtime.id(), runtime.secrets().as_ref())
        .await?
        .into_iter()
        .find(|existing| existing.slug == provider);

    let endpoint = supplied(body.endpoint.as_deref());
    if let Some(endpoint) = endpoint.as_deref()
        && info.needs_endpoint()
    {
        validate_endpoint(endpoint).await?;
    }

    store::put_provider(
        runtime.id(),
        runtime.secrets().as_ref(),
        SearchProvider {
            slug: provider.clone(),
            // Naming a provider on this route is selecting it, and a selected
            // provider that is switched off resolves to something else — which
            // would make the route answer 200 and change nothing, the same
            // failure as the managed branch above. It is also what makes the
            // round trip work: managed, then back to this provider.
            enabled: true,
            endpoint: endpoint.or_else(|| connected.and_then(|p| p.endpoint)),
        },
    )
    .await?;
    if let Some(key) = supplied(body.api_key.as_deref()) {
        store::store_provider_key(runtime.id(), runtime.secrets().as_ref(), &provider, &key)
            .await?;
    }
    store::set_default_slug(runtime.id(), runtime.secrets().as_ref(), &provider).await?;

    Ok(Json(status_of(runtime).await?))
}

/// [`put_search`]'s tail, for a provider that was not named in the body.
async fn apply_to(
    runtime: &CompanyRuntime,
    slug: &str,
    body: &SearchConfigBody,
) -> Result<Json<SearchStatus>, ApiError> {
    if let Some(key) = supplied(body.api_key.as_deref()) {
        store::store_provider_key(runtime.id(), runtime.secrets().as_ref(), slug, &key).await?;
    }
    if let Some(endpoint) = supplied(body.endpoint.as_deref()) {
        validate_endpoint(&endpoint).await?;
        let enabled = store::list_providers(runtime.id(), runtime.secrets().as_ref())
            .await?
            .into_iter()
            .find(|provider| provider.slug == slug)
            .map(|provider| provider.enabled)
            .unwrap_or(true);
        store::put_provider(
            runtime.id(),
            runtime.secrets().as_ref(),
            SearchProvider {
                slug: slug.to_string(),
                enabled,
                endpoint: Some(endpoint),
            },
        )
        .await?;
    }
    Ok(Json(status_of(runtime).await?))
}

/// `DELETE …/search/key` — clear every connection and fall back to managed.
///
/// The [`SecretStore`](crate::ports::SecretStore) port has no delete, so a
/// cleared credential is stored as the empty string; every read site treats an
/// empty value as unset, and resolution then falls back to managed search.
async fn delete_search(
    company: AdminScopedCompany,
    State(_state): State<AppState>,
) -> Result<Json<SearchStatus>, ApiError> {
    let runtime = &company.runtime;
    // Every provider goes, not just the active one: this route has always meant
    // "disconnect my own search", and leaving a second account's key behind
    // under a page that now says "managed" would be storing a credential the
    // operator believes they deleted.
    for provider in store::list_providers(runtime.id(), runtime.secrets().as_ref()).await? {
        store::delete_provider(runtime.id(), runtime.secrets().as_ref(), &provider.slug).await?;
    }
    let cleared: Vec<(&str, String)> = [API_KEY_SECRET, PROVIDER_SECRET, ENDPOINT_SECRET]
        .into_iter()
        .map(|key| (key, String::new()))
        .collect();
    write_all(runtime, &cleared).await?;
    store::clear_default_slug(runtime.id(), runtime.secrets().as_ref()).await?;
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
                overlay_desk_hive: Vec::new(),
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

    /// `status_of`'s own comment says a company record that cannot be loaded
    /// reports `granted: false` rather than failing the whole status — "the
    /// operator still needs to see what IS configured, and a settings page
    /// that 500s tells them nothing." That fallback only runs when
    /// `store().load()` actually errors, which an absent record does not do
    /// (`Ok(None)`, not `Err`) — so this corrupts the on-disk manifest after
    /// the company has already booted, forcing a real `FsCompanyStore::load`
    /// failure on the route's own re-read rather than mocking the store.
    #[tokio::test]
    async fn a_company_that_fails_to_load_reports_ungranted_instead_of_500() {
        let home_dir = ::tempfile::tempdir().expect("tempdir");
        let home = home_dir.path();
        let state = state_with_company(home, true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        // Baseline: the manifest grants `search`, so the route reports it.
        let (status, body) =
            call(&state, "GET", "/api/v1/companies/acme/search", &admin, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["granted"], true);

        let toml_path = crate::store::Bundle::new(home, &CompanyId::new("acme")).company_toml();
        tokio::fs::write(&toml_path, b"not valid toml [[[")
            .await
            .expect("corrupt company.toml");

        let (status, body) =
            call(&state, "GET", "/api/v1/companies/acme/search", &admin, None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a store-load failure must still answer with a status, not a 500: {body}"
        );
        assert_eq!(
            body["granted"], false,
            "an unreadable record must fall back to ungranted rather than keep reporting the \
             last-known grant: {body}"
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
    async fn the_legacy_route_refuses_what_the_modern_one_refuses() {
        // Four checks guard an operator-supplied address, and only one of the
        // four write paths ran all four. `POST …/search/providers` did;
        // `PUT …/search` ran the http(s) prefix and the metadata guard and
        // skipped the length cap and the control-character check, so the
        // compatibility route stored addresses the modern route refuses.
        //
        // The length one is the case `MAX_ENDPOINT_LEN`'s own comment was
        // written about: an unbounded operator-supplied string reaching the
        // store is how a row that cannot be deleted gets made.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        let too_long = format!(
            "http://search.acme.internal/{}",
            "a".repeat(MAX_ENDPOINT_LEN)
        );
        let (status, _) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "searxng", "endpoint": too_long})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "the cap applies here too");

        let (status, _) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "searxng", "endpoint": "http://search.acme.internal/\u{7}"})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "so does the control-character check"
        );

        // Nothing was stored on the way to either refusal.
        let (_, after) = call(&state, "GET", "/api/v1/companies/acme/search", &admin, None).await;
        assert_eq!(after["effectiveProvider"], "managed", "{after}");
        assert!(
            after["providers"].as_array().expect("providers").is_empty(),
            "{after}"
        );
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
    async fn switching_providers_does_not_hand_one_providers_key_to_another() {
        // This test used to assert the opposite, and that assertion WAS the bug.
        //
        // With one `search/api_key` for the whole company, switching to Querit
        // without pasting a key left Exa's key in the slot — and every layer
        // then agreed the company was correctly configured, because a key was
        // present: `configuration_complete` said yes, the badge said Querit, and
        // the harness wired Querit's tools around Exa's credential. The first
        // agent to search got a 401 that nothing on the page could explain.
        //
        // Each provider holds its own credential now, so the switch reports
        // Querit as selected with no key, and searches stay on managed until one
        // is pasted. Exa keeps its key and its row.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "exa", "apiKey": "exa-not-a-real-key"})),
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
        assert_eq!(
            after["apiKeyConfigured"], false,
            "querit must not inherit exa's key: {after}"
        );
        assert_eq!(
            after["effectiveProvider"], "managed",
            "a keyless selection searches through managed, not through a borrowed key: {after}"
        );

        let rows = after["providers"].as_array().expect("providers");
        let exa = rows
            .iter()
            .find(|row| row["slug"] == "exa")
            .expect("exa row survives the switch");
        assert_eq!(
            exa["keyConfigured"], true,
            "exa keeps its own credential: {after}"
        );
        let querit = rows
            .iter()
            .find(|row| row["slug"] == "querit")
            .expect("querit row");
        assert_eq!(querit["keyConfigured"], false, "{after}");
    }

    #[tokio::test]
    async fn selecting_managed_actually_stops_searching_through_the_account() {
        // The compatibility route answered 200 for `{"provider":"managed"}` and
        // changed nothing an agent could feel. It cleared the default marker,
        // and `resolve::active` reads an absent marker as "the first usable
        // provider" — so a company with a working Exa connection kept searching
        // through Exa, billed to Exa, after explicitly asking to stop.
        //
        // Worse where it matters most: an upgraded legacy company has no marker
        // at all, so the clear was already a no-op there and the route was pure
        // theatre.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        let (_, connected) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "exa", "apiKey": "exa-not-a-real-key"})),
        )
        .await;
        assert_eq!(
            connected["effectiveProvider"], "exa",
            "the setup has to actually be searching through exa: {connected}"
        );

        let (status, after) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "managed"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            after["effectiveProvider"], "managed",
            "asking for managed and being told 200 has to mean it: {after}"
        );

        // Switched off, not destroyed. The credential is write-only and is
        // never shown back, so an operator who lost one here could not retype it
        // from the screen.
        let exa = after["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .find(|row| row["slug"] == "exa")
            .expect("the exa row survives")
            .clone();
        assert_eq!(exa["keyConfigured"], true, "the key is kept: {after}");
        assert_eq!(exa["enabled"], false, "the connection is off: {after}");

        // And naming it again turns it back on, rather than marking a default
        // that resolves to nothing.
        let (_, back) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "exa"})),
        )
        .await;
        assert_eq!(
            back["effectiveProvider"], "exa",
            "the round trip has to come back: {back}"
        );
    }

    #[tokio::test]
    async fn two_providers_hold_two_independent_credentials() {
        // The central claim of the rework, asserted end to end through the
        // routes rather than only against the store.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        for (slug, key) in [
            ("exa", "exa-not-a-real-key"),
            ("brave", "brave-not-a-real-key"),
        ] {
            call(
                &state,
                "PUT",
                "/api/v1/companies/acme/search",
                &admin,
                Some(json!({"provider": slug, "apiKey": key})),
            )
            .await;
        }

        let (_, body) = call(&state, "GET", "/api/v1/companies/acme/search", &admin, None).await;
        let rows = body["providers"].as_array().expect("providers");
        assert_eq!(rows.len(), 2, "{body}");
        for row in rows {
            assert_eq!(row["keyConfigured"], true, "{body}");
        }

        // Clearing one leaves the other untouched. Under the old single slot
        // this was not expressible at all.
        //
        // The status is asserted because it has already hidden a bug once: with
        // `Path<String>` under `scoped`, the platform form captures `{id}` too
        // and every one of these returned 400 while the assertions below still
        // read as "the key was not cleared".
        let (cleared_status, cleared_body) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search/providers/exa/key",
            &admin,
            Some(json!({"apiKey": ""})),
        )
        .await;
        assert_eq!(cleared_status, StatusCode::OK, "{cleared_body}");

        let (_, after) = call(&state, "GET", "/api/v1/companies/acme/search", &admin, None).await;
        let rows = after["providers"].as_array().expect("providers");
        let keyed: Vec<&str> = rows
            .iter()
            .filter(|row| row["keyConfigured"] == true)
            .map(|row| row["slug"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(keyed, vec!["brave"], "{after}");
    }

    #[tokio::test]
    async fn a_key_cannot_be_replaced_on_a_provider_that_is_not_connected() {
        // `PUT …/search/providers/{slug}/key` is **replace**, so there has to be
        // something to replace. Without the check it wrote a credential to
        // `search/provider/exa/key` that the status route never reports and
        // `DELETE …/search/key` never clears — its loop visits indexed
        // providers only. An invisible credential the operator can neither see
        // nor delete is the orphaned-secret shape this module refuses
        // everywhere else.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        let (status, _) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search/providers/exa/key",
            &admin,
            Some(json!({"apiKey": "exa-not-a-real-key"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (_, after) = call(&state, "GET", "/api/v1/companies/acme/search", &admin, None).await;
        assert!(
            after["providers"].as_array().expect("providers").is_empty(),
            "{after}"
        );

        // And it still works for one that IS connected — the refusal must not
        // have cost the ordinary path.
        call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search",
            &admin,
            Some(json!({"provider": "exa", "apiKey": "exa-not-a-real-key"})),
        )
        .await;
        let (status, after) = call(
            &state,
            "PUT",
            "/api/v1/companies/acme/search/providers/exa/key",
            &admin,
            Some(json!({"apiKey": "exa-also-not-a-real-key"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{after}");
    }

    #[tokio::test]
    async fn a_hostile_slug_never_reaches_the_secret_store() {
        // Credential addresses are `search/provider/<slug>/key`, so a slug
        // carrying a slash or an unbounded run of text is a slug that writes
        // somewhere other than where it claims.
        let home = ::tempfile::tempdir().expect("tempdir");
        let state = state_with_company(home.path(), true).await;
        let admin = crate::server::test_support::seed_admin(&state, "acme").await;

        for slug in ["%2e%2e%2fexa", &"x".repeat(300), "%E6%A4%9C%E7%B4%A2"] {
            let (status, body) = call(
                &state,
                "DELETE",
                &format!("/api/v1/companies/acme/search/providers/{slug}"),
                &admin,
                None,
            )
            .await;
            assert!(
                !status.is_success(),
                "a slug that could address another provider's credential must never \
                 succeed — axum may refuse it at the router before the handler, which is \
                 equally fine: {slug}: {body}"
            );
        }
    }
}
