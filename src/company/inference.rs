//! Per-tenant Bring-Your-Own-Key inference (issue #56): the inert data model
//! plus the async secret-resolution used to materialize a company's *effective*
//! inference configuration.
//!
//! A company's effective inference config is the highest-precedence of three
//! sources:
//!
//! 1. **Runtime** — a config the operator sets through the console, persisted as
//!    a single JSON blob in the [`SecretStore`](crate::ports::SecretStore) under
//!    [`RUNTIME_CONFIG_KEY`]. Highest precedence, so a console switch takes
//!    effect on the agents' next turn with no rebuild.
//! 2. **Manifest** — the `[inference]` section committed in `company.toml`
//!    ([`Inference`]). Declarative intent; never a credential.
//! 3. **Default** — the platform-injected managed brain
//!    (`TINYHUMANS_API_KEY` / `OPENCOMPANY_INFERENCE_*`), passed in as an
//!    [`EnvDefault`]. Lowest precedence.
//!
//! Credentials live apart from the declarations. The outbound key is written to
//! its own [`KEY_KEY`] secret (write-only via the console) — never inline in the
//! runtime config blob or the manifest — and is attached to the
//! [`InferenceDecl`] as a [`Credential`] by [`resolve_effective`], then read on
//! the request path by [`InferenceDecl::bearer`]. Deferring the read is what lets
//! the managed tier be a *rotating* platform token rather than a value captured
//! once at boot. Nothing here ever serializes a credential into an API response,
//! log line, or agent-visible output: [`InferenceDecl`] derives no `Serialize`
//! and its `Debug` redacts the credential.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::company::credentials::Credential;
use crate::company::types::{INFERENCE_PROVIDERS, Inference};
use crate::error::OpenCompanyError;
use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, SecretValue};

/// The [`SecretStore`](crate::ports::SecretStore) key holding the JSON runtime
/// inference override (a [`RuntimeInference`] the console writes).
pub const RUNTIME_CONFIG_KEY: &str = "inference/config";

/// The canonical per-company inference credential key. The outbound token is
/// stored here (write-only via the console); the value is the raw token string.
pub const KEY_KEY: &str = "inference/key";

/// The [`SecretStore`](crate::ports::SecretStore) key holding the runtime
/// inference override for `harness_id`.
///
/// The **default** harness keeps the flat legacy key. That is not cosmetic: a
/// tenant's stored console override and credential already live at
/// [`RUNTIME_CONFIG_KEY`] / [`KEY_KEY`], and the store has no rename — so
/// namespacing every harness would silently orphan the config of every company
/// already running, which is the one migration this design cannot afford.
///
/// Non-default harnesses namespace under `harness/<id>/`, so two `built_in`
/// harnesses can hold two different OpenRouter accounts.
pub fn runtime_config_key(harness_id: &str, is_default: bool) -> String {
    if is_default {
        return RUNTIME_CONFIG_KEY.to_string();
    }
    format!("harness/{harness_id}/{RUNTIME_CONFIG_KEY}")
}

/// The credential key for `harness_id`. Same default-harness rule as
/// [`runtime_config_key`].
pub fn harness_key_key(harness_id: &str, is_default: bool) -> String {
    if is_default {
        return KEY_KEY.to_string();
    }
    format!("harness/{harness_id}/{KEY_KEY}")
}

/// Which harness's secrets a resolution reads, and whether that harness is the
/// company default (which keeps the flat legacy keys).
///
/// Passed as one value rather than two loose arguments because the pair is only
/// ever meaningful together — an id without the default flag cannot name a key.
#[derive(Clone, Debug)]
pub struct HarnessScope {
    /// The harness id.
    pub id: String,
    /// Whether it is the company's default harness.
    pub is_default: bool,
}

impl HarnessScope {
    /// The scope for a company's default harness — what every pre-existing
    /// caller means, and what keeps them reading the flat keys.
    pub fn default_harness(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            is_default: true,
        }
    }

    /// A named, non-default harness.
    pub fn named(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            is_default: false,
        }
    }

    /// This scope's runtime-config secret key.
    pub fn config_key(&self) -> String {
        runtime_config_key(&self.id, self.is_default)
    }

    /// This scope's credential secret key.
    pub fn key_key(&self) -> String {
        harness_key_key(&self.id, self.is_default)
    }
}

impl Default for HarnessScope {
    fn default() -> Self {
        Self::default_harness(crate::company::types::IMPLICIT_HARNESS_ID)
    }
}

/// The platform's OpenAI-compatible endpoint — the subscription proxy an
/// `openrouter` company with **no** key of its own resolves against.
///
/// The proxy fronts OpenRouter upstream and meters the spend against the
/// tenant's subscription, so from the workload's point of view this and
/// [`OPENROUTER_BASE_URL`] serve the same catalogue; only who pays differs.
pub const PLATFORM_BASE_URL: &str = "https://api.tinyhumans.ai/openai/v1";

/// The provider kind removed when OpenCompany stopped exposing its own model
/// SKUs. A manifest or stored runtime blob still naming it aliases to
/// [`DEFAULT_PROVIDER`] rather than failing: a runtime blob is data an operator
/// cannot hand-edit, so hard-failing on it would strand a tenant whose console
/// wrote a value that used to be valid.
pub const LEGACY_MANAGED: &str = "managed";

/// The provider a company gets when nothing names one.
pub const DEFAULT_PROVIDER: &str = "openrouter";

/// Default concrete OpenRouter model id per abstract tier.
///
/// Used on the **direct** path only. A tier names a workload, and something has
/// to turn it into a model id before the request leaves this process — but only
/// when the endpoint would not do it. See [`model_for_tier`].
///
/// The slugs mirror the platform's own OpenRouter bindings, so proxied and
/// direct resolve to the same models by default.
pub const DEFAULT_TIER_MODELS: &[(&str, &str)] = &[
    ("chat-v1", "anthropic/claude-sonnet-5"),
    ("reasoning-v1", "openai/gpt-5.6-sol-pro"),
    ("agentic-v1", "anthropic/claude-opus-5"),
    ("vision-v1", "qwen/qwen3.8-max"),
];

/// The concrete model id to put on the wire for `tier`.
///
/// **The two paths need different answers, and sending the wrong one fails.**
///
/// * **Proxied** — the platform endpoint resolves tier names itself, against a
///   curated registry that pins each tier to a sub-provider so its rate card
///   stays exact. A bare tier is exactly what it wants. It also accepts a
///   concrete model, but only under its own `openrouter/<author>/<slug>`
///   namespace, and only when passthrough is switched on there — which it is
///   not by default. So a tier is the only thing that always works.
/// * **Direct** — OpenRouter has never heard of `chat-v1`, so a tier must be
///   resolved here or the request 400s.
///
/// An operator's own `models` entry is honoured verbatim on both paths: they
/// named a specific model and it is not this function's place to rewrite it
/// (on the proxied path they can write the `openrouter/…` form themselves).
pub fn model_for_tier(tier: &str, overrides: &BTreeMap<String, String>, proxied: bool) -> String {
    if let Some(mapped) = overrides.get(tier) {
        return mapped.clone();
    }
    if proxied {
        // Let the platform resolve it. Substituting a concrete slug here would
        // bypass its per-tier provider pinning and, with passthrough off, be
        // rejected outright.
        return tier.to_string();
    }
    DEFAULT_TIER_MODELS
        .iter()
        .find(|(name, _)| *name == tier)
        .map(|(_, model)| (*model).to_string())
        .unwrap_or_else(|| tier.to_string())
}

/// Normalizes a provider kind: blank and the legacy `managed` both become
/// [`DEFAULT_PROVIDER`]; anything else passes through for validation to judge.
pub fn normalize_provider(provider: &str) -> &str {
    match provider.trim() {
        "" | LEGACY_MANAGED => DEFAULT_PROVIDER,
        other => other,
    }
}

/// OpenRouter's OpenAI-compatible base URL — used when the `openrouter`
/// provider names no explicit `base_url`.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// A local Ollama server's OpenAI-compatible surface — the convenience default
/// for the `ollama` provider (validation still requires an explicit `base_url`
/// in the manifest; this only backstops an empty resolved value).
pub const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

/// Where an effective inference config came from — drives the console's source
/// badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceSource {
    /// The platform-injected managed default (`env`).
    Default,
    /// Declared in `company.toml`'s `[inference]`.
    Manifest,
    /// Set at runtime through the console.
    Runtime,
}

/// The platform-injected managed default (from `harness_inference_from_env`):
/// the base URL + credential the manager supplies. Passed to
/// [`resolve_effective`] as the lowest-precedence source.
///
/// The credential is a [`Credential`], not a `String`: on the hosted platform it
/// is a projected token that rotates in place, so it is resolved per request
/// rather than captured here.
#[derive(Clone, Debug)]
pub struct EnvDefault {
    /// Managed base URL (env `OPENCOMPANY_INFERENCE_URL` or the default).
    pub base_url: String,
    /// Managed credential — a projected platform token source, or the static
    /// `OPENCOMPANY_INFERENCE_KEY` / `TINYHUMANS_API_KEY` value.
    pub credential: Credential,
}

/// The on-disk runtime inference override stored under [`RUNTIME_CONFIG_KEY`].
/// Carries no credential — the token lives apart under [`KEY_KEY`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeInference {
    /// Provider kind — one of [`INFERENCE_PROVIDERS`].
    pub provider: String,
    /// Optional OpenAI-compatible base URL override.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Abstract-tier → concrete model id.
    #[serde(default)]
    pub models: BTreeMap<String, String>,
}

/// One company's *effective* inference configuration — the highest-precedence
/// of runtime / manifest / env, carrying the credential as a resolvable
/// [`Credential`] rather than a captured value.
///
/// Derives **no** `Serialize` (the key must never cross a wire) and its `Debug`
/// redacts the credential.
#[derive(Clone, Debug)]
pub struct InferenceDecl {
    /// Provider slug — one of [`INFERENCE_PROVIDERS`].
    pub provider: String,
    /// Resolved OpenAI-compatible base URL (never empty for a valid config).
    pub base_url: String,
    /// Abstract-tier → concrete model id. Empty means every tier passes
    /// through to the provider verbatim.
    pub models: BTreeMap<String, String>,
    /// Provenance badge for the console.
    pub source: InferenceSource,
    /// The outbound credential. Private — read only through
    /// [`bearer`](Self::bearer); never serialized.
    credential: Credential,
    /// Whether this rides the platform's subscription proxy. Read through
    /// [`is_proxied`](Self::is_proxied).
    proxied: bool,
}

impl InferenceDecl {
    /// The outbound credential, unresolved. Callers on the request path want
    /// [`bearer`](Self::bearer); this is for status and fingerprinting.
    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// The bearer to present on **this** request, or `None` to omit the header
    /// (the keyless Ollama case).
    ///
    /// Resolved per call rather than captured at build time: a hosted tenant's
    /// managed credential is a projected token the platform rotates in place, so
    /// a value captured once would go stale within minutes.
    pub async fn bearer(&self) -> Result<Option<String>> {
        self.credential.current().await
    }

    /// Whether an outbound credential is configured — the non-secret status the
    /// read APIs surface. Never returns the value, and never reads the token.
    pub fn key_configured(&self) -> bool {
        self.credential.configured()
    }

    /// Whether this config rides the platform's subscription proxy rather than
    /// a credential the tenant supplied.
    ///
    /// True only for `openrouter` with no tenant key — the default a company
    /// starts on. It is what separates "the subscription pays" from "the tenant
    /// pays", which is why it is recorded here rather than re-derived from the
    /// base URL by every caller that cares.
    pub fn is_proxied(&self) -> bool {
        self.proxied
    }

    /// The stable telemetry slug for this config
    /// (`subscription` / `openrouter` / `byok` / `ollama`).
    ///
    /// Distinguishes proxied from direct OpenRouter, because those are two
    /// different payers and a Usage view that merged them would be telling the
    /// operator nothing.
    pub fn telemetry_slug(&self) -> &'static str {
        if self.proxied {
            return "subscription";
        }
        provider_slug(&self.provider)
    }
}

/// The stable telemetry slug for a provider kind.
///
/// An unknown kind reports `unknown` rather than being folded into a real
/// provider's attribution: [`resolve_effective`] rejects one outright, so
/// reaching here with one means something upstream is wrong, and quietly
/// billing it to a provider that was never called would hide that.
///
/// This answers on the *kind* alone. Proxied OpenRouter slugs as
/// `subscription`, which needs the credential too — see
/// [`InferenceDecl::telemetry_slug`].
pub fn provider_slug(provider: &str) -> &'static str {
    match normalize_provider(provider) {
        "openrouter" => "openrouter",
        "ollama" => "ollama",
        "openai_compatible" => "byok",
        _ => "unknown",
    }
}

/// Resolves the effective `(base_url, credential, proxied)` for a provider.
///
/// **`openrouter` is dual-mode**, and that is the whole shape of the product's
/// first-run story:
///
/// * **No tenant key** — the config inherits the platform endpoint and the
///   platform credential, and the subscription pays. This is where a company
///   starts, with nothing configured and nobody asked for a card.
/// * **A tenant `sk-or-…`** — the config goes direct to OpenRouter on the
///   tenant's own account.
///
/// The inheritance branch is the one `managed` used to own, and it moved here
/// rather than being deleted for the same reason it existed: a config that named
/// a provider but dropped the platform key would 401 rather than fall back.
///
/// The one exception is a keyless `openrouter` that also sets its own
/// `base_url`: that endpoint is not the platform's, so the platform credential
/// is withheld and the config goes direct (keyless) instead — sending the
/// platform token to an arbitrary override would leak it.
///
/// Every other kind uses its own configured base URL and key verbatim — those
/// are third-party endpoints we hold no credential for.
fn resolve_endpoint(
    provider: &str,
    base_url_override: Option<&str>,
    key: String,
    env_default: Option<&EnvDefault>,
) -> (String, Credential, bool) {
    let base_url_override = base_url_override.map(str::trim).filter(|s| !s.is_empty());
    let has_key = !key.trim().is_empty();

    if normalize_provider(provider) == "openrouter" && !has_key {
        // The platform credential rides only the platform's own endpoint. A
        // tenant-supplied base URL override with no key is a direct (keyless)
        // config, not the inheritance branch: pairing the platform token with
        // an arbitrary endpoint would leak it to wherever the override points.
        if let Some(base_url) = base_url_override {
            return (base_url.to_string(), Credential::None, false);
        }
        let base_url = env_default
            .map(|e| e.base_url.clone())
            .unwrap_or_else(|| PLATFORM_BASE_URL.to_string());
        let credential = env_default
            .map(|e| e.credential.clone())
            .unwrap_or(Credential::None);
        return (base_url, credential, true);
    }

    (
        effective_base_url(provider, base_url_override),
        Credential::from_value(key),
        false,
    )
}

/// A declaration for the **first-run** connection test, before any company
/// exists to resolve one from.
///
/// [`resolve_effective`] cannot serve the wizard: it reads a company's secret
/// store and manifest, and during first-run setup there is neither. This builds
/// the same shape from what the operator has just typed, through the *same*
/// [`resolve_endpoint`] — so a probe defaults its URL and treats a blank key
/// exactly as the running system will, rather than by a second set of rules that
/// agree today and drift later. A test that passes under different rules than
/// the runtime uses is worse than no test.
///
/// `env_default` is what the host already has. Passing it is what makes "leave
/// the key blank and press Test" mean *test the credential this host was given*
/// — the hosted case, where the operator has no key of their own and the control
/// plane injected one. A blank key with no env default resolves to
/// [`Credential::None`]: correct for keyless Ollama, and honestly
/// unauthenticated everywhere else, which is what the probe should then report.
///
/// [`InferenceSource::Runtime`] because that is what this is — a value an
/// operator supplied, with nothing persisted anywhere yet.
pub fn decl_for_probe(
    provider: &str,
    base_url: Option<&str>,
    key: Option<&str>,
    env_default: Option<&EnvDefault>,
) -> InferenceDecl {
    let provider = provider.trim().to_string();
    let (base_url, credential, proxied) = resolve_endpoint(
        &provider,
        base_url,
        key.unwrap_or_default().trim().to_string(),
        env_default,
    );
    InferenceDecl {
        provider,
        base_url,
        models: BTreeMap::new(),
        source: InferenceSource::Runtime,
        credential,
        proxied,
    }
}

/// The effective base URL for a provider kind, given an optional override.
///
/// This is the **direct** endpoint for each kind. Proxied `openrouter` does not
/// come through here — it inherits the platform endpoint in
/// [`resolve_endpoint`], which is the only place that distinction is made.
///
/// `ollama` backstops to a local default; `openai_compatible` has no default
/// (validation requires an explicit URL).
pub fn effective_base_url(provider: &str, override_url: Option<&str>) -> String {
    let override_url = override_url.map(str::trim).filter(|s| !s.is_empty());
    match normalize_provider(provider) {
        "ollama" => override_url.unwrap_or(OLLAMA_DEFAULT_BASE_URL).to_string(),
        "openai_compatible" => override_url.unwrap_or_default().to_string(),
        // openrouter, and any unknown kind (which `resolve_effective` rejects).
        _ => override_url.unwrap_or(OPENROUTER_BASE_URL).to_string(),
    }
}

/// Normalizes an endpoint typed by a person during setup.
///
/// Local model applications commonly advertise themselves as `localhost:1234`
/// even though the OpenAI-compatible client needs
/// `http://localhost:1234/v1`. Accept that familiar spelling while preserving
/// explicit schemes and non-root paths.
pub fn normalize_setup_base_url(provider: &str, raw: Option<&str>) -> Option<String> {
    let raw = raw.map(str::trim).filter(|value| !value.is_empty())?;
    if !matches!(normalize_provider(provider), "ollama" | "openai_compatible") {
        return Some(raw.trim_end_matches('/').to_string());
    }

    let mut url = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    url = url.trim_end_matches('/').to_string();
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    if !after_scheme.contains('/') {
        url.push_str("/v1");
    }
    Some(url)
}

/// Loads the runtime inference override, or `None` when unset/blank. A malformed
/// blob is a store error (surfaced, not silently dropped).
pub async fn load_runtime_config(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<Option<RuntimeInference>> {
    load_runtime_config_scoped(company, secrets, &HarnessScope::default()).await
}

/// [`load_runtime_config`] for one harness's own slot.
pub async fn load_runtime_config_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    scope: &HarnessScope,
) -> Result<Option<RuntimeInference>> {
    let Some(SecretValue(raw)) = secrets.get(company, &scope.config_key()).await? else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let config: RuntimeInference = serde_json::from_str(&raw).map_err(|e| {
        OpenCompanyError::Store(format!("inference runtime config is not valid JSON: {e}"))
    })?;
    if config.provider.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(config))
}

/// Persists the runtime inference override (console `PUT`).
pub async fn save_runtime_config(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    config: &RuntimeInference,
) -> Result<()> {
    save_runtime_config_scoped(company, secrets, config, &HarnessScope::default()).await
}

/// [`save_runtime_config`] for one harness's own slot.
pub async fn save_runtime_config_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    config: &RuntimeInference,
    scope: &HarnessScope,
) -> Result<()> {
    let raw = serde_json::to_string(config)
        .map_err(|e| OpenCompanyError::Store(format!("serializing inference config: {e}")))?;
    secrets
        .set(company, &scope.config_key(), SecretValue(raw))
        .await
}

/// Clears the runtime inference override (console `DELETE` → revert to
/// manifest/managed). Best-effort — the store has no delete, so an empty value
/// reads back as unset.
pub async fn clear_runtime_config(company: &CompanyId, secrets: &dyn SecretStore) -> Result<()> {
    clear_runtime_config_scoped(company, secrets, &HarnessScope::default()).await
}

/// [`clear_runtime_config`] for one harness's own slot.
pub async fn clear_runtime_config_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    scope: &HarnessScope,
) -> Result<()> {
    secrets
        .set(company, &scope.config_key(), SecretValue(String::new()))
        .await
}

/// Reads the effective outbound credential.
///
/// The canonical [`KEY_KEY`] (`inference/key`) is tried first — the console
/// writes rotated tokens there. When it is empty/missing, `override_key` (a
/// manifest section's `api_key_secret`) is the fallback for a commit-time key.
/// Returns an empty string when neither holds a value.
pub async fn load_key(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    override_key: Option<&str>,
) -> Result<String> {
    load_key_scoped(company, secrets, override_key, &HarnessScope::default()).await
}

/// [`load_key`] for one harness's own credential slot.
pub async fn load_key_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    override_key: Option<&str>,
    scope: &HarnessScope,
) -> Result<String> {
    if let Some(SecretValue(raw)) = secrets.get(company, &scope.key_key()).await?
        && !raw.trim().is_empty()
    {
        return Ok(raw);
    }
    if let Some(key) = override_key.map(str::trim).filter(|s| !s.is_empty())
        && let Some(SecretValue(raw)) = secrets.get(company, key).await?
        && !raw.trim().is_empty()
    {
        return Ok(raw);
    }
    Ok(String::new())
}

/// Writes the company's outbound inference credential (write-only intake).
pub async fn store_key(company: &CompanyId, secrets: &dyn SecretStore, key: &str) -> Result<()> {
    store_key_scoped(company, secrets, key, &HarnessScope::default()).await
}

/// [`store_key`] for one harness's own credential slot.
pub async fn store_key_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    key: &str,
    scope: &HarnessScope,
) -> Result<()> {
    secrets
        .set(company, &scope.key_key(), SecretValue(key.to_string()))
        .await
}

/// Clears the stored credential (best-effort — the store has no delete, so an
/// empty value reads back as "not configured").
pub async fn clear_key(company: &CompanyId, secrets: &dyn SecretStore) -> Result<()> {
    secrets
        .set(company, KEY_KEY, SecretValue(String::new()))
        .await
}

/// Whether the company currently has an outbound inference credential — the
/// non-secret status surfaced by the read APIs. Never returns the value.
pub async fn key_configured(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    override_key: Option<&str>,
) -> Result<bool> {
    Ok(!load_key(company, secrets, override_key)
        .await?
        .trim()
        .is_empty())
}

/// Resolves a company's *effective* inference configuration.
///
/// Precedence is **runtime > manifest > env-default**. Returns `None` when no
/// source configures inference at all — the caller then keeps the managed/echo
/// brain. The single seam the harness builder and the ops route both use so the
/// agent-facing resolution and the console's status view stay identical.
///
/// This re-reads the secret store on every call, which is what makes a console
/// switch take effect on the agents' next turn with no rebuild.
pub async fn resolve_effective(
    company: &CompanyId,
    manifest: &Inference,
    env_default: Option<&EnvDefault>,
    secrets: &dyn SecretStore,
) -> Result<Option<InferenceDecl>> {
    resolve_effective_scoped(
        company,
        manifest,
        env_default,
        secrets,
        &HarnessScope::default(),
    )
    .await
}

/// [`resolve_effective`] against one harness's own config and credential slots.
///
/// `manifest` is that harness's `[harness.inference]`, or the company-level
/// `[inference]` when it declares none — the caller picks, because only it knows
/// which fallback applies.
///
/// The precedence within a harness is unchanged (runtime > manifest > env
/// default); what differs is only *which* secret keys the runtime and credential
/// tiers read. Two `built_in` harnesses therefore resolve independently, which
/// is what lets one run on the subscription while the other runs on a key.
pub async fn resolve_effective_scoped(
    company: &CompanyId,
    manifest: &Inference,
    env_default: Option<&EnvDefault>,
    secrets: &dyn SecretStore,
    scope: &HarnessScope,
) -> Result<Option<InferenceDecl>> {
    // 1. Runtime override (console) wins.
    if let Some(runtime) = load_runtime_config_scoped(company, secrets, scope).await? {
        let provider = normalize_provider(&runtime.provider).to_string();
        reject_unknown_provider(&provider, "the stored runtime inference config")?;
        let key = load_key_scoped(company, secrets, None, scope).await?;
        let (base_url, credential, proxied) =
            resolve_endpoint(&provider, runtime.base_url.as_deref(), key, env_default);
        return Ok(Some(InferenceDecl {
            provider,
            base_url,
            models: runtime.models,
            source: InferenceSource::Runtime,
            credential,
            proxied,
        }));
    }

    // 2. Manifest `[inference]`.
    if manifest.is_set() {
        let provider =
            normalize_provider(manifest.provider.as_deref().unwrap_or_default()).to_string();
        reject_unknown_provider(&provider, "`[inference].provider`")?;
        let key =
            load_key_scoped(company, secrets, manifest.api_key_secret.as_deref(), scope).await?;
        let (base_url, credential, proxied) =
            resolve_endpoint(&provider, manifest.base_url.as_deref(), key, env_default);
        return Ok(Some(InferenceDecl {
            provider,
            base_url,
            models: manifest.models.clone(),
            source: InferenceSource::Manifest,
            credential,
            proxied,
        }));
    }

    // 3. The platform-injected default: OpenRouter, proxied on the subscription.
    //    A company that has configured nothing lands here, which is why it must
    //    be a working config and not a prompt for a credential.
    //
    //    It still runs through `resolve_endpoint` rather than assuming proxied,
    //    because a console-set key is a configuration act even when the operator
    //    never named a provider: the console's key field on a fresh company
    //    writes `inference/key` and nothing else. Assuming proxied here would
    //    take that key, store it, report it as configured — and then never send
    //    it anywhere.
    if let Some(env) = env_default {
        let key = load_key_scoped(company, secrets, None, scope).await?;
        let (base_url, credential, proxied) =
            resolve_endpoint(DEFAULT_PROVIDER, None, key, Some(env));
        return Ok(Some(InferenceDecl {
            provider: DEFAULT_PROVIDER.to_string(),
            base_url,
            models: BTreeMap::new(),
            source: InferenceSource::Default,
            credential,
            proxied,
        }));
    }

    Ok(None)
}

/// Fails a provider kind that is not in [`INFERENCE_PROVIDERS`].
///
/// The manifest validator already rejects one, but a **stored runtime blob**
/// never passes through it: the console wrote it, possibly under an older build
/// whose vocabulary differed. Resolving one silently would attribute its spend to
/// whatever the fallback happened to be, so it fails loudly here instead — the
/// one place both sources converge.
fn reject_unknown_provider(provider: &str, whence: &str) -> Result<()> {
    if crate::company::types::INFERENCE_PROVIDERS.contains(&provider) {
        return Ok(());
    }
    Err(OpenCompanyError::Config(format!(
        "{whence} names an unknown inference provider `{provider}` — expected one of {}.",
        crate::company::types::INFERENCE_PROVIDERS.join(", ")
    )))
}

/// Validates the manifest `[inference]` section, returning every problem in
/// prosumer language. An absent section (`provider = None`) is inert. Shared by
/// manifest validation and the ops `PUT` route (via [`validate_runtime`]).
pub fn validate_inference(inference: &Inference) -> Vec<String> {
    let Some(provider_raw) = inference.provider.as_deref() else {
        return Vec::new();
    };
    let provider = provider_raw.trim();
    if provider.is_empty() {
        return Vec::new();
    }
    validate_parts(
        provider,
        inference.base_url.as_deref(),
        inference.api_key_secret.as_deref(),
    )
}

/// Validates a runtime override (console `PUT`) — same rules as the manifest,
/// but a runtime override never names a secret key (the console writes the
/// canonical `inference/key`), so `api_key_secret` is not part of the shape.
pub fn validate_runtime(config: &RuntimeInference) -> Vec<String> {
    validate_parts(config.provider.trim(), config.base_url.as_deref(), None)
}

/// The shared validation rules for an inference declaration.
fn validate_parts(
    provider: &str,
    base_url: Option<&str>,
    api_key_secret: Option<&str>,
) -> Vec<String> {
    let mut problems = Vec::new();

    // `managed` aliases rather than failing. It named a real thing until
    // OpenCompany stopped exposing its own SKUs, and a committed manifest that
    // still says it means "the platform's brain" — which is now proxied
    // OpenRouter. Rejecting it would break bundles that were valid when written,
    // to no purpose: the intent still resolves.
    let provider = normalize_provider(provider);

    if !INFERENCE_PROVIDERS.contains(&provider) {
        problems.push(format!(
            "`[inference].provider` must be one of {} — you wrote `{provider}`.",
            INFERENCE_PROVIDERS.join(", ")
        ));
    }

    let base_url = base_url.map(str::trim).filter(|s| !s.is_empty());
    match provider {
        "ollama" | "openai_compatible" => match base_url {
            None => problems.push(format!(
                "`[inference].base_url` is required for provider `{provider}` — give the OpenAI-compatible endpoint URL."
            )),
            Some(url) if !is_http_url(url) => problems.push(format!(
                "`[inference].base_url` must be an `http://` or `https://` URL — you wrote `{url}`."
            )),
            _ => {}
        },
        _ => {
            if let Some(url) = base_url
                && !is_http_url(url)
            {
                problems.push(format!(
                    "`[inference].base_url` must be an `http://` or `https://` URL — you wrote `{url}`."
                ));
            }
        }
    }

    // The credential must be a *key name*, not the token itself. Reject values
    // that look like a pasted credential so a secret never lands in the manifest.
    if let Some(secret) = api_key_secret.map(str::trim).filter(|s| !s.is_empty())
        && looks_like_inline_credential(secret)
    {
        problems.push(
            "`[inference].api_key_secret` names a secret-store key, not the secret itself — you appear to have pasted a credential. Set the token through the console instead.".to_string(),
        );
    }

    problems
}

/// True when `url` is an absolute `http://` or `https://` URL.
fn is_http_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Heuristic: does this string look like a pasted credential rather than a
/// secret-store *key name*? Catches the common provider token prefixes and any
/// long, opaque, single-token value.
fn looks_like_inline_credential(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "sk-", "sk_", "pk-", "pk_", "rk-", "or-v1-", "xai-", "gsk_", "bearer ",
    ];
    if PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    // A long, opaque token with no path separator or whitespace — key names are
    // short and structured (`inference/openrouter`), tokens are long and dense.
    value.len() >= 40 && !value.contains('/') && !value.contains(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;

    /// The resolved bearer for a decl, for the assertions below.
    async fn bearer(decl: &InferenceDecl) -> Option<String> {
        decl.bearer().await.expect("credential resolves")
    }

    fn inference(provider: &str) -> Inference {
        Inference {
            provider: Some(provider.to_string()),
            base_url: None,
            api_key_secret: None,
            models: BTreeMap::new(),
        }
    }

    #[derive(Default)]
    struct MemSecrets {
        map: Mutex<HashMap<String, String>>,
    }

    #[async_trait]
    impl SecretStore for MemSecrets {
        async fn get(&self, _c: &CompanyId, key: &str) -> Result<Option<SecretValue>> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(key)
                .map(|v| SecretValue(v.clone())))
        }
        async fn set(&self, _c: &CompanyId, key: &str, value: SecretValue) -> Result<()> {
            self.map.lock().unwrap().insert(key.to_string(), value.0);
            Ok(())
        }
    }

    // ---- precedence matrix -------------------------------------------------

    #[tokio::test]
    async fn runtime_beats_manifest_beats_env() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/v1".into(),
            credential: Credential::from_value("env-key"),
        };
        let mut manifest = inference("openai_compatible");
        manifest.base_url = Some("https://manifest.example/v1".into());

        // Env only.
        let decl = resolve_effective(&company, &Inference::default(), Some(&env), &secrets)
            .await
            .unwrap()
            .expect("env default resolves");
        assert_eq!(decl.source, InferenceSource::Default);
        assert_eq!(decl.provider, DEFAULT_PROVIDER);
        assert!(decl.is_proxied(), "the default rides the subscription");
        assert_eq!(decl.telemetry_slug(), "subscription");
        assert_eq!(bearer(&decl).await.as_deref(), Some("env-key"));

        // Manifest beats env.
        let decl = resolve_effective(&company, &manifest, Some(&env), &secrets)
            .await
            .unwrap()
            .expect("manifest resolves");
        assert_eq!(decl.source, InferenceSource::Manifest);
        assert_eq!(decl.provider, "openai_compatible");
        assert_eq!(decl.base_url, "https://manifest.example/v1");

        // Runtime beats manifest.
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "openrouter".into(),
                base_url: None,
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        store_key(&company, &secrets, "or-secret").await.unwrap();
        let decl = resolve_effective(&company, &manifest, Some(&env), &secrets)
            .await
            .unwrap()
            .expect("runtime resolves");
        assert_eq!(decl.source, InferenceSource::Runtime);
        assert_eq!(decl.provider, "openrouter");
        assert_eq!(decl.base_url, OPENROUTER_BASE_URL);
        assert!(!decl.is_proxied(), "a tenant key goes direct");
        assert_eq!(decl.telemetry_slug(), "openrouter");
        assert_eq!(bearer(&decl).await.as_deref(), Some("or-secret"));
        assert!(decl.key_configured());
    }

    /// A keyless `openrouter` inherits the platform endpoint and credential
    /// rather than dropping them — the branch `managed` used to own. Without it a
    /// company that names its provider but holds no key of its own would 401
    /// instead of riding the subscription.
    #[tokio::test]
    async fn keyless_openrouter_inherits_the_platform_endpoint_and_credential() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        let decl = resolve_effective(&company, &inference("openrouter"), Some(&env), &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.source, InferenceSource::Manifest);
        assert_eq!(decl.provider, "openrouter");
        assert_eq!(decl.base_url, "https://env.example/openai/v1");
        assert!(decl.is_proxied());
        assert_eq!(bearer(&decl).await.as_deref(), Some("platform-key"));
    }

    /// A keyless `openrouter` with a tenant-supplied `base_url` override goes
    /// direct with **no** credential — the platform token must not ride an
    /// arbitrary endpoint the operator pointed it at.
    #[tokio::test]
    async fn keyless_openrouter_never_sends_the_platform_credential_to_an_override() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        let mut manifest = inference("openrouter");
        manifest.base_url = Some("https://attacker.example/v1".into());
        let decl = resolve_effective(&company, &manifest, Some(&env), &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.base_url, "https://attacker.example/v1");
        assert!(
            !decl.is_proxied(),
            "an arbitrary endpoint is not the subscription"
        );
        assert!(
            !decl.key_configured(),
            "a keyless config holds no credential to send"
        );
        assert_eq!(decl.telemetry_slug(), "openrouter");
        assert_eq!(
            bearer(&decl).await,
            None,
            "the platform credential stays home"
        );
    }

    /// A committed manifest still saying `provider = "managed"` resolves as
    /// proxied OpenRouter rather than failing. It was valid when written, and the
    /// intent — "the platform's brain" — is exactly what proxied OpenRouter is.
    #[tokio::test]
    async fn a_legacy_managed_manifest_aliases_to_proxied_openrouter() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        let decl = resolve_effective(&company, &inference(LEGACY_MANAGED), Some(&env), &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.provider, DEFAULT_PROVIDER);
        assert!(decl.is_proxied());
        assert_eq!(decl.base_url, "https://env.example/openai/v1");
        assert_eq!(bearer(&decl).await.as_deref(), Some("platform-key"));
        assert!(
            validate_inference(&inference(LEGACY_MANAGED)).is_empty(),
            "and it still validates"
        );

        // The same alias applies to a stored runtime blob, which an operator
        // cannot hand-edit — the case that would otherwise strand a tenant.
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: LEGACY_MANAGED.into(),
                base_url: None,
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        let decl = resolve_effective(&company, &Inference::default(), Some(&env), &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.source, InferenceSource::Runtime);
        assert_eq!(decl.provider, DEFAULT_PROVIDER);
        assert!(decl.is_proxied());
    }

    /// A stored runtime blob naming a provider this build does not know fails
    /// loudly rather than resolving to whatever the fallback happened to be.
    #[tokio::test]
    async fn an_unknown_stored_provider_is_an_error_not_a_silent_fallback() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "telepathy".into(),
                base_url: None,
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        let err = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .expect_err("unknown provider must fail");
        let msg = err.to_string();
        assert!(msg.contains("telepathy"), "{msg}");
        assert!(msg.contains("openrouter"), "names what is valid: {msg}");
    }

    /// Issue #585: the company's own key is the admin's to set, and a key stored
    /// through the console wins over the deploy-time env credential — otherwise
    /// the only way to pay for a tenant is an environment variable the admin
    /// cannot reach.
    ///
    /// **What changed with `managed`'s removal.** Under `managed`, a console key
    /// kept the *platform* endpoint, so an admin could bill their own account
    /// through the proxy. `openrouter` is dual-mode instead: a key means an
    /// OpenRouter key, so it goes direct to OpenRouter — sending an `sk-or-…` to
    /// the platform proxy would simply be rejected. An admin who wants the
    /// platform endpoint with a credential of their own now names
    /// `openai_compatible` with that `base_url`.
    #[tokio::test]
    async fn a_console_key_wins_over_the_env_credential_and_goes_direct() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "openrouter".into(),
                base_url: None,
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        store_key(&company, &secrets, "company-key").await.unwrap();

        let decl = resolve_effective(&company, &Inference::default(), Some(&env), &secrets)
            .await
            .unwrap()
            .expect("runtime openrouter resolves");
        assert_eq!(decl.source, InferenceSource::Runtime);
        assert_eq!(decl.provider, "openrouter");
        assert_eq!(bearer(&decl).await.as_deref(), Some("company-key"));
        assert!(decl.key_configured());
        assert!(!decl.is_proxied(), "the tenant's own account pays");
        assert_eq!(decl.base_url, OPENROUTER_BASE_URL);

        // Clearing it falls back to the subscription rather than 401ing — the
        // property that makes a key genuinely optional in both directions.
        clear_key(&company, &secrets).await.unwrap();
        let decl = resolve_effective(&company, &Inference::default(), Some(&env), &secrets)
            .await
            .unwrap()
            .expect("still resolves with no key");
        assert!(decl.is_proxied());
        assert_eq!(decl.base_url, "https://env.example/openai/v1");
        assert_eq!(bearer(&decl).await.as_deref(), Some("platform-key"));
    }

    /// Issue #585 adds a second writer to `key_configured` — an admin setting the
    /// company's key from the console — alongside the platform default the
    /// manager injects. #636's `effective_status` split exists precisely to keep
    /// the console's `keyConfigured` reporting *tenant* config and never the
    /// platform token. Nothing asserted the two stay distinguishable, so this
    /// does: on one company, the same call answers differently depending on
    /// which source is in play.
    #[tokio::test]
    async fn a_console_key_and_the_platform_default_are_distinguishable() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let platform = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "managed".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Nothing tenant-scoped is stored yet. The platform-aware resolve is
        // credentialled — that is the value the console must NOT surface — while
        // the tenant-only resolve the read route uses reports "no key".
        let with_platform =
            resolve_effective(&company, &Inference::default(), Some(&platform), &secrets)
                .await
                .unwrap()
                .expect("platform default resolves");
        let tenant_only = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap()
            .expect("runtime config resolves");
        assert!(
            with_platform.key_configured(),
            "the platform token is a real credential"
        );
        assert!(
            !tenant_only.key_configured(),
            "an injected platform token must never light up the console's `keyConfigured`"
        );

        // An admin sets the company's key. Now both agree — and the bearer the
        // agents present is the tenant's, not the platform's.
        store_key(&company, &secrets, "company-key").await.unwrap();
        let with_platform =
            resolve_effective(&company, &Inference::default(), Some(&platform), &secrets)
                .await
                .unwrap()
                .expect("platform default resolves");
        let tenant_only = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap()
            .expect("runtime config resolves");
        assert!(
            tenant_only.key_configured(),
            "a console-set key is tenant config"
        );
        assert_eq!(bearer(&with_platform).await.as_deref(), Some("company-key"));
    }

    #[tokio::test]
    async fn no_source_resolves_to_none() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let decl = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap();
        assert!(
            decl.is_none(),
            "no source means the managed/echo brain stays"
        );
    }

    #[tokio::test]
    async fn clearing_runtime_reverts_to_manifest() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let manifest = inference("openrouter");
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "ollama".into(),
                base_url: Some("http://localhost:11434/v1".into()),
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            resolve_effective(&company, &manifest, None, &secrets)
                .await
                .unwrap()
                .unwrap()
                .provider,
            "ollama"
        );
        clear_runtime_config(&company, &secrets).await.unwrap();
        assert_eq!(
            resolve_effective(&company, &manifest, None, &secrets)
                .await
                .unwrap()
                .unwrap()
                .provider,
            "openrouter"
        );
    }

    // ---- write-only key ----------------------------------------------------

    #[tokio::test]
    async fn key_is_write_only_and_never_serialized() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        store_key(&company, &secrets, "sk-super-secret")
            .await
            .unwrap();
        let decl = resolve_effective(&company, &inference("openrouter"), None, &secrets)
            .await
            .unwrap()
            .unwrap();
        // The key resolves for request building…
        assert_eq!(bearer(&decl).await.as_deref(), Some("sk-super-secret"));
        // …but never appears in the Debug rendering.
        let debug = format!("{decl:?}");
        assert!(!debug.contains("sk-super-secret"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[tokio::test]
    async fn cleared_key_reads_back_unconfigured() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        store_key(&company, &secrets, "tok").await.unwrap();
        assert!(key_configured(&company, &secrets, None).await.unwrap());
        clear_key(&company, &secrets).await.unwrap();
        assert!(!key_configured(&company, &secrets, None).await.unwrap());
    }

    #[tokio::test]
    async fn manifest_api_key_secret_is_the_fallback_key() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        // Only the manifest-named key holds a token; canonical key is cold.
        secrets
            .set(
                &company,
                "byo/openrouter",
                SecretValue("named-secret".into()),
            )
            .await
            .unwrap();
        let mut manifest = inference("openrouter");
        manifest.api_key_secret = Some("byo/openrouter".into());
        let decl = resolve_effective(&company, &manifest, None, &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bearer(&decl).await.as_deref(), Some("named-secret"));
    }

    // ---- validation --------------------------------------------------------

    #[test]
    fn absent_section_is_inert() {
        assert!(validate_inference(&Inference::default()).is_empty());
    }

    #[test]
    fn valid_configs_pass() {
        assert!(validate_inference(&inference("managed")).is_empty());
        assert!(validate_inference(&inference("openrouter")).is_empty());
        let mut ollama = inference("ollama");
        ollama.base_url = Some("http://localhost:11434/v1".into());
        assert!(
            validate_inference(&ollama).is_empty(),
            "{:?}",
            validate_inference(&ollama)
        );
    }

    #[test]
    fn unknown_provider_is_rejected() {
        let problems = validate_inference(&inference("gpt5"));
        assert!(
            problems.iter().any(|p| p.contains("provider")),
            "{problems:?}"
        );
    }

    #[test]
    fn ollama_and_openai_compatible_require_base_url() {
        let ollama = validate_inference(&inference("ollama"));
        assert!(
            ollama
                .iter()
                .any(|p| p.contains("base_url") && p.contains("required"))
        );
        let compat = validate_inference(&inference("openai_compatible"));
        assert!(
            compat
                .iter()
                .any(|p| p.contains("base_url") && p.contains("required"))
        );
    }

    #[test]
    fn non_http_base_url_is_rejected() {
        let mut m = inference("openai_compatible");
        m.base_url = Some("ftp://x/v1".into());
        let problems = validate_inference(&m);
        assert!(problems.iter().any(|p| p.contains("http")), "{problems:?}");
    }

    #[test]
    fn inline_credential_in_key_name_is_rejected() {
        let mut m = inference("openrouter");
        m.api_key_secret = Some("sk-or-v1-abcdef0123456789".into());
        let problems = validate_inference(&m);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("names a secret-store key")),
            "{problems:?}"
        );

        // A long opaque token with no separators is also caught.
        let mut m2 = inference("openrouter");
        m2.api_key_secret = Some("abcdefghijklmnopqrstuvwxyz0123456789ABCDEF".into());
        assert!(!validate_inference(&m2).is_empty());

        // A structured key name is accepted.
        let mut ok = inference("openrouter");
        ok.api_key_secret = Some("byo/openrouter".into());
        assert!(
            validate_inference(&ok).is_empty(),
            "{:?}",
            validate_inference(&ok)
        );
    }

    /// The isolation property named harnesses exist for: two `built_in`
    /// harnesses on one company resolve independently, so one can ride the
    /// subscription while the other runs on a key of its own.
    #[tokio::test]
    async fn two_harnesses_on_one_company_resolve_independently() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        let embedded = HarnessScope::default_harness("embedded");
        let deep = HarnessScope::named("deep");

        // Only `deep` gets a key.
        store_key_scoped(&company, &secrets, "sk-or-deep", &deep)
            .await
            .unwrap();

        let d = resolve_effective_scoped(
            &company,
            &inference("openrouter"),
            Some(&env),
            &secrets,
            &deep,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!d.is_proxied(), "deep pays its own way");
        assert_eq!(d.base_url, OPENROUTER_BASE_URL);
        assert_eq!(bearer(&d).await.as_deref(), Some("sk-or-deep"));

        let e = resolve_effective_scoped(
            &company,
            &inference("openrouter"),
            Some(&env),
            &secrets,
            &embedded,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(e.is_proxied(), "embedded is untouched by deep's key");
        assert_eq!(e.base_url, "https://env.example/v1");
        assert_eq!(bearer(&e).await.as_deref(), Some("platform-key"));
    }

    /// The default harness keeps the flat legacy keys, so a tenant whose console
    /// already wrote `inference/key` keeps working with no migration — the store
    /// has no rename, so getting this wrong would orphan every running company.
    #[tokio::test]
    async fn the_default_harness_reads_the_legacy_flat_keys() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();

        // Written the pre-harness way.
        store_key(&company, &secrets, "legacy-key").await.unwrap();
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "openrouter".into(),
                base_url: None,
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();

        // Read back through the scoped path, as the default harness.
        let scope = HarnessScope::default_harness("embedded");
        assert_eq!(scope.key_key(), KEY_KEY);
        assert_eq!(scope.config_key(), RUNTIME_CONFIG_KEY);

        let decl =
            resolve_effective_scoped(&company, &Inference::default(), None, &secrets, &scope)
                .await
                .unwrap()
                .expect("the legacy config resolves");
        assert_eq!(decl.source, InferenceSource::Runtime);
        assert_eq!(bearer(&decl).await.as_deref(), Some("legacy-key"));

        // A named harness namespaces instead, and sees none of it.
        let named = HarnessScope::named("deep");
        assert_eq!(named.key_key(), "harness/deep/inference/key");
        assert_eq!(named.config_key(), "harness/deep/inference/config");
        assert!(
            load_runtime_config_scoped(&company, &secrets, &named)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Every declared tier resolves to a concrete OpenRouter slug, and a tier
    /// left unmapped by the harness still does.
    ///
    /// This is what makes the DIRECT path work at all: OpenRouter has never
    /// heard of `chat-v1`, so a bare tier on a tenant's own key would 400.
    #[test]
    fn every_tier_resolves_to_a_concrete_model_id_on_the_direct_path() {
        let none = BTreeMap::new();
        for tier in crate::company::types::INFERENCE_TIERS {
            let resolved = model_for_tier(tier, &none, false);
            assert_ne!(
                &resolved, tier,
                "`{tier}` must map to a concrete slug, not pass through"
            );
            assert!(
                resolved.contains('/'),
                "`{tier}` resolved to `{resolved}`, which is not an OpenRouter slug"
            );
        }
    }

    /// `every_tier_resolves_to_a_concrete_model_id_on_the_direct_path` above
    /// only proves each tier resolves to *some* concrete slug — it would still
    /// pass if `chat-v1` and `agentic-v1` were swapped. These expected ids are
    /// hardcoded rather than read back from [`DEFAULT_TIER_MODELS`]: asserting
    /// a table against itself would pass no matter what the table said, so an
    /// incorrect tier assignment needs a second, independent source of truth
    /// to fail against.
    #[test]
    fn every_tier_resolves_to_its_documented_default_model() {
        let none = BTreeMap::new();
        for (tier, expected) in [
            ("chat-v1", "anthropic/claude-sonnet-5"),
            ("reasoning-v1", "openai/gpt-5.6-sol-pro"),
            ("agentic-v1", "anthropic/claude-opus-5"),
            ("vision-v1", "qwen/qwen3.8-max"),
        ] {
            assert_eq!(
                model_for_tier(tier, &none, false),
                expected,
                "`{tier}` must resolve to the documented default `{expected}`"
            );
        }
    }

    #[test]
    fn a_harness_override_beats_the_default_and_a_concrete_slug_passes_through() {
        let overrides =
            BTreeMap::from([("chat-v1".to_string(), "anthropic/claude-haiku".to_string())]);
        // An operator's own entry is honoured verbatim on BOTH paths — they
        // named a specific model, and rewriting it is not this function's call.
        for proxied in [false, true] {
            assert_eq!(
                model_for_tier("chat-v1", &overrides, proxied),
                "anthropic/claude-haiku"
            );
        }
        // An unmapped tier still takes the shipped default on the direct path.
        assert_eq!(
            model_for_tier("reasoning-v1", &overrides, false),
            "openai/gpt-5.6-sol-pro"
        );
        // A caller naming a concrete slug is not treated as an unknown tier.
        assert_eq!(
            model_for_tier("anthropic/claude-sonnet-4.5", &BTreeMap::new(), false),
            "anthropic/claude-sonnet-4.5"
        );
    }

    /// The proxied path keeps the tier name. The platform's registry routes on
    /// it and pins each tier to a sub-provider; substituting a concrete slug
    /// would bypass that pinning, and its passthrough namespace is opt-in and
    /// off by default, so the slug would simply be rejected.
    #[test]
    fn the_proxied_path_keeps_the_tier_name() {
        let none = BTreeMap::new();
        for tier in crate::company::types::INFERENCE_TIERS {
            assert_eq!(&model_for_tier(tier, &none, true), tier);
        }
    }

    #[test]
    fn provider_slugs_map_as_documented() {
        assert_eq!(provider_slug("openrouter"), "openrouter");
        assert_eq!(provider_slug("openai_compatible"), "byok");
        assert_eq!(provider_slug("ollama"), "ollama");
        // The legacy kind slugs as what it now is, so historical usage rows and
        // new ones aggregate together.
        assert_eq!(provider_slug(LEGACY_MANAGED), "openrouter");
        // An unknown kind is never folded into a real provider's attribution.
        assert_eq!(provider_slug("mystery"), "unknown");
    }

    #[test]
    fn effective_base_url_defaults_per_provider() {
        assert_eq!(
            effective_base_url(LEGACY_MANAGED, None),
            OPENROUTER_BASE_URL
        );
        assert_eq!(effective_base_url("openrouter", None), OPENROUTER_BASE_URL);
        assert_eq!(effective_base_url("ollama", None), OLLAMA_DEFAULT_BASE_URL);
        assert_eq!(
            effective_base_url("openrouter", Some("https://proxy/v1")),
            "https://proxy/v1"
        );
    }

    #[test]
    fn setup_accepts_the_localhost_spelling_local_model_apps_display() {
        assert_eq!(
            normalize_setup_base_url("ollama", Some("localhost:6969")),
            Some("http://localhost:6969/v1".to_string())
        );
        assert_eq!(
            normalize_setup_base_url("openai_compatible", Some("http://127.0.0.1:1234/v1/")),
            Some("http://127.0.0.1:1234/v1".to_string())
        );
        assert_eq!(
            normalize_setup_base_url("openai_compatible", Some("https://llm.test/api")),
            Some("https://llm.test/api".to_string())
        );
    }
}
