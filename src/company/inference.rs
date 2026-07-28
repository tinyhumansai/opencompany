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
//! runtime config blob or the manifest — and is resolved into the
//! [`InferenceDecl`]'s private field only at build/turn time by
//! [`resolve_effective`]. Nothing here ever serializes a credential into an API
//! response, log line, or agent-visible output: [`InferenceDecl`] derives no
//! `Serialize` and its `Debug` redacts the key.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Result;
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

/// Default managed base URL (the hosted TinyHumans / Medulla OpenAI-compatible
/// surface) when the managed provider names no `base_url`.
pub const MANAGED_BASE_URL: &str = "https://api.tinyhumans.ai/openai/v1";

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
/// [`resolve_effective`] as the lowest-precedence source. Carries no `Debug`
/// derive so the credential never lands in a trace.
#[derive(Clone)]
pub struct EnvDefault {
    /// Managed base URL (env `OPENCOMPANY_INFERENCE_URL` or the default).
    pub base_url: String,
    /// Managed credential (`OPENCOMPANY_INFERENCE_KEY` / `TINYHUMANS_API_KEY`).
    pub api_key: String,
}

impl std::fmt::Debug for EnvDefault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvDefault")
            .field("base_url", &self.base_url)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .finish()
    }
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
/// of runtime / manifest / env, with the credential resolved to a private field
/// at build/turn time.
///
/// Derives **no** `Serialize` (the key must never cross a wire) and its `Debug`
/// redacts the key.
#[derive(Clone)]
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
    /// Resolved outbound credential. Empty means "no bearer" (e.g. Ollama).
    /// Private — read only through [`api_key`](Self::api_key); never serialized.
    api_key: String,
}

impl InferenceDecl {
    /// The resolved outbound credential (empty = omit the bearer header). Kept
    /// crate-internal so the request builder can read it; never serialized.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Whether an outbound credential is configured — the non-secret status the
    /// read APIs surface. Never returns the value.
    pub fn key_configured(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    /// The stable telemetry slug for this provider (`managed` / `openrouter` /
    /// `byok` / `ollama`).
    pub fn telemetry_slug(&self) -> &'static str {
        provider_slug(&self.provider)
    }
}

impl std::fmt::Debug for InferenceDecl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceDecl")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("models", &self.models)
            .field("source", &self.source)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .finish()
    }
}

/// The stable telemetry slug for a provider kind. Unknown kinds fall back to
/// `managed` (the safe default attribution).
pub fn provider_slug(provider: &str) -> &'static str {
    match provider.trim() {
        "openrouter" => "openrouter",
        "ollama" => "ollama",
        "openai_compatible" => "byok",
        _ => "managed",
    }
}

/// Resolves the effective `(base_url, api_key)` for a provider.
///
/// The `managed` kind is special: it is the *platform* brain, so it inherits the
/// env default's base URL and credential when a source (manifest/runtime) names
/// `managed` without supplying its own — otherwise a hand-written
/// `provider = "managed"` would drop the platform key and 401. Every other kind
/// uses its own configured base URL + key verbatim.
fn resolve_endpoint(
    provider: &str,
    base_url_override: Option<&str>,
    key: String,
    env_default: Option<&EnvDefault>,
) -> (String, String) {
    let base_url_override = base_url_override.map(str::trim).filter(|s| !s.is_empty());
    if provider.trim() == "managed" {
        let base_url = base_url_override
            .map(str::to_string)
            .or_else(|| env_default.map(|e| e.base_url.clone()))
            .unwrap_or_else(|| MANAGED_BASE_URL.to_string());
        let api_key = if !key.trim().is_empty() {
            key
        } else {
            env_default.map(|e| e.api_key.clone()).unwrap_or_default()
        };
        (base_url, api_key)
    } else {
        (effective_base_url(provider, base_url_override), key)
    }
}

/// The effective base URL for a provider kind, given an optional override.
///
/// `managed`/`openrouter` default to their well-known endpoints; `ollama`
/// backstops to a local default; `openai_compatible` has no default (validation
/// requires an explicit URL).
pub fn effective_base_url(provider: &str, override_url: Option<&str>) -> String {
    let override_url = override_url.map(str::trim).filter(|s| !s.is_empty());
    match provider.trim() {
        "openrouter" => override_url.unwrap_or(OPENROUTER_BASE_URL).to_string(),
        "ollama" => override_url.unwrap_or(OLLAMA_DEFAULT_BASE_URL).to_string(),
        "openai_compatible" => override_url.unwrap_or_default().to_string(),
        // managed (and any unknown kind, which validation rejects separately).
        _ => override_url.unwrap_or(MANAGED_BASE_URL).to_string(),
    }
}

/// Loads the runtime inference override, or `None` when unset/blank. A malformed
/// blob is a store error (surfaced, not silently dropped).
pub async fn load_runtime_config(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<Option<RuntimeInference>> {
    let Some(SecretValue(raw)) = secrets.get(company, RUNTIME_CONFIG_KEY).await? else {
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
    let raw = serde_json::to_string(config)
        .map_err(|e| OpenCompanyError::Store(format!("serializing inference config: {e}")))?;
    secrets
        .set(company, RUNTIME_CONFIG_KEY, SecretValue(raw))
        .await
}

/// Clears the runtime inference override (console `DELETE` → revert to
/// manifest/managed). Best-effort — the store has no delete, so an empty value
/// reads back as unset.
pub async fn clear_runtime_config(company: &CompanyId, secrets: &dyn SecretStore) -> Result<()> {
    secrets
        .set(company, RUNTIME_CONFIG_KEY, SecretValue(String::new()))
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
    if let Some(SecretValue(raw)) = secrets.get(company, KEY_KEY).await?
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
    secrets
        .set(company, KEY_KEY, SecretValue(key.to_string()))
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
    // 1. Runtime override (console) wins.
    if let Some(runtime) = load_runtime_config(company, secrets).await? {
        let provider = runtime.provider.trim().to_string();
        let key = load_key(company, secrets, None).await?;
        let (base_url, api_key) =
            resolve_endpoint(&provider, runtime.base_url.as_deref(), key, env_default);
        return Ok(Some(InferenceDecl {
            provider,
            base_url,
            models: runtime.models,
            source: InferenceSource::Runtime,
            api_key,
        }));
    }

    // 2. Manifest `[inference]`.
    if manifest.is_set() {
        let provider = manifest
            .provider
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        let key = load_key(company, secrets, manifest.api_key_secret.as_deref()).await?;
        let (base_url, api_key) =
            resolve_endpoint(&provider, manifest.base_url.as_deref(), key, env_default);
        return Ok(Some(InferenceDecl {
            provider,
            base_url,
            models: manifest.models.clone(),
            source: InferenceSource::Manifest,
            api_key,
        }));
    }

    // 3. Platform-injected managed default.
    if let Some(env) = env_default {
        return Ok(Some(InferenceDecl {
            provider: "managed".to_string(),
            base_url: env.base_url.clone(),
            models: BTreeMap::new(),
            source: InferenceSource::Default,
            api_key: env.api_key.clone(),
        }));
    }

    Ok(None)
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
            api_key: "env-key".into(),
        };
        let mut manifest = inference("openai_compatible");
        manifest.base_url = Some("https://manifest.example/v1".into());

        // Env only.
        let decl = resolve_effective(&company, &Inference::default(), Some(&env), &secrets)
            .await
            .unwrap()
            .expect("env default resolves");
        assert_eq!(decl.source, InferenceSource::Default);
        assert_eq!(decl.provider, "managed");
        assert_eq!(decl.api_key(), "env-key");

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
        assert_eq!(decl.api_key(), "or-secret");
        assert!(decl.key_configured());
    }

    #[tokio::test]
    async fn manifest_managed_inherits_env_credential() {
        // A hand-written `provider = "managed"` must still use the platform
        // env key + base URL rather than dropping the credential.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            api_key: "platform-key".into(),
        };
        let decl = resolve_effective(&company, &inference("managed"), Some(&env), &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.source, InferenceSource::Manifest);
        assert_eq!(decl.provider, "managed");
        assert_eq!(decl.base_url, "https://env.example/openai/v1");
        assert_eq!(decl.api_key(), "platform-key");
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
        assert_eq!(decl.api_key(), "sk-super-secret");
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
        assert_eq!(decl.api_key(), "named-secret");
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

    #[test]
    fn provider_slugs_map_as_documented() {
        assert_eq!(provider_slug("managed"), "managed");
        assert_eq!(provider_slug("openrouter"), "openrouter");
        assert_eq!(provider_slug("openai_compatible"), "byok");
        assert_eq!(provider_slug("ollama"), "ollama");
        assert_eq!(provider_slug("mystery"), "managed");
    }

    #[test]
    fn effective_base_url_defaults_per_provider() {
        assert_eq!(effective_base_url("managed", None), MANAGED_BASE_URL);
        assert_eq!(effective_base_url("openrouter", None), OPENROUTER_BASE_URL);
        assert_eq!(effective_base_url("ollama", None), OLLAMA_DEFAULT_BASE_URL);
        assert_eq!(
            effective_base_url("openrouter", Some("https://proxy/v1")),
            "https://proxy/v1"
        );
    }
}
