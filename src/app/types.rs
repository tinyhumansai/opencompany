use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use serde::Serialize;

use crate::app::config::{BrainMode, redacted};
use crate::company::{SkillDoc, load_dir_skills};
use crate::ports::types::{CompanyId, SecretValue};
use crate::runtime::CompanyRegistry;
use crate::server::platform_auth::PlatformAuthConfig;
use crate::server::webhook::WebhookConfig;
use crate::{VERSION, tiny::RuntimeModuleStatus};

/// Runtime configuration for OpenCompany.
///
/// `Debug` is implemented by hand so the TinyHumans credential is redacted to
/// `set`/`missing` and can never reach a log line or panic message.
#[derive(Clone)]
pub struct AppConfig {
    /// Address for the Axum HTTP server.
    pub bind: String,
    /// Optional sibling OpenHuman checkout used by launcher commands.
    pub openhuman_root: Option<PathBuf>,
    /// Bearer token required on operator routes. When `None`, Phase-1 dev mode
    /// allows local operator calls without authentication.
    pub operator_token: Option<String>,
    /// TinyHumans orchestration API base URL.
    pub api_url: String,
    /// Which brain the runtime drives.
    pub brain_mode: BrainMode,
    /// tiny.place economy API base URL.
    pub tinyplace_api_url: String,
    /// Public host base URL advertised in published Agent Cards. When `None`,
    /// the card endpoint falls back to `http://{bind}`.
    pub public_url: Option<String>,
    /// TinyHumans hosted-brain credential, if configured. Redacted in `Debug`.
    pub tinyhumans_credential: Option<SecretValue>,
    /// Platform (multi-tenant) auth. When set, `{id}` routes honor tenant scopes
    /// and provisioning/suspension require the `platform` scope. When `None`, the
    /// prosumer `operator_token` path is used.
    pub platform_auth: Option<PlatformAuthConfig>,
    /// Global cap on the number of provisioned companies. `None` = unlimited.
    pub max_companies: Option<usize>,
    /// Per-tenant cap on provisioned companies. `None` = unlimited.
    pub max_companies_per_tenant: Option<usize>,
    /// Outbound webhook delivery configuration. `None` disables webhooks.
    pub webhook: Option<WebhookConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".to_string(),
            openhuman_root: None,
            operator_token: None,
            api_url: crate::app::config::DEFAULT_API_URL.to_string(),
            brain_mode: BrainMode::Hosted,
            tinyplace_api_url: crate::app::config::DEFAULT_TINYPLACE_API_URL.to_string(),
            public_url: None,
            tinyhumans_credential: None,
            platform_auth: None,
            max_companies: None,
            max_companies_per_tenant: None,
            webhook: None,
        }
    }
}

impl AppConfig {
    /// True when hosted cognition can run: hosted mode plus a credential.
    pub fn cycles_available(&self) -> bool {
        self.brain_mode == BrainMode::Hosted && self.tinyhumans_credential.is_some()
    }

    /// The host base URL to embed in published Agent Card endpoints: the
    /// configured [`Self::public_url`] when set, otherwise `http://{bind}`.
    pub fn host_base_url(&self) -> String {
        match &self.public_url {
            Some(url) => url.clone(),
            None => format!("http://{}", self.bind),
        }
    }
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppConfig")
            .field("bind", &self.bind)
            .field("openhuman_root", &self.openhuman_root)
            .field(
                "operator_token",
                &self.operator_token.as_ref().map(|_| "set"),
            )
            .field("api_url", &self.api_url)
            .field("brain_mode", &self.brain_mode)
            .field("tinyplace_api_url", &self.tinyplace_api_url)
            .field("public_url", &self.public_url)
            .field(
                "tinyhumans_credential",
                &redacted(&self.tinyhumans_credential),
            )
            .field("platform_auth", &self.platform_auth)
            .field("max_companies", &self.max_companies)
            .field("max_companies_per_tenant", &self.max_companies_per_tenant)
            .field("webhook", &self.webhook)
            .finish()
    }
}

/// Shared application state passed to Axum handlers.
#[derive(Clone, Debug)]
pub struct AppState {
    config: AppConfig,
    registry: CompanyRegistry,
    /// OpenCompany home root holding company bundles. Used by the tiny.place
    /// A2A inbound routes to resolve a company's Ed25519 identity.
    home: std::path::PathBuf,
    /// Company → owning-tenant map, populated when a company is provisioned in
    /// platform mode. Drives per-tenant quotas and cross-tenant isolation.
    ///
    /// Batch-1 durability is a documented stub: this map is in-memory and resets
    /// on restart until a durable `tenant_id` slot exists on the company record.
    ownership: Arc<RwLock<HashMap<CompanyId, String>>>,
    /// The opened storage backend's port handles, when a non-fs backend is
    /// selected (`OPENCOMPANY_STORAGE`). Provisioning injects these into each
    /// new company's builder; `None` means fs defaults.
    stores: Option<crate::store::StorageHandles>,
    /// Cache of the repo-level shared skill registry (`skills/*/SKILL.md`).
    /// Populated on first read via [`AppState::skill_registry`]; never
    /// invalidated because the repo's skill library is immutable at runtime.
    skill_registry: Arc<OnceLock<Arc<[SkillDoc]>>>,
    /// Injected network seams for the credential surfaces (DNS resolver, mail
    /// sender). Empty by default so the build stays offline.
    connections: crate::server::ops::ConnectionsRuntime,
    /// Host-global replay-protection cache shared across every inbound A2A
    /// request. Gated behind `tinyplace` so the default build links no crypto.
    #[cfg(feature = "tinyplace")]
    nonce: std::sync::Arc<crate::economy::NonceCache>,
}

impl AppState {
    /// Builds state from runtime configuration with an empty company registry.
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            registry: CompanyRegistry::new(),
            home: std::path::PathBuf::from("."),
            ownership: Arc::new(RwLock::new(HashMap::new())),
            stores: None,
            skill_registry: Arc::new(OnceLock::new()),
            connections: crate::server::ops::ConnectionsRuntime::new(),
            #[cfg(feature = "tinyplace")]
            nonce: std::sync::Arc::new(crate::economy::NonceCache::new()),
        }
    }

    /// Sets the OpenCompany home root used to resolve company identities.
    pub fn with_home(mut self, home: impl Into<std::path::PathBuf>) -> Self {
        self.home = home.into();
        self
    }

    /// Installs the opened storage backend's port handles (non-fs backends).
    pub fn with_stores(mut self, stores: crate::store::StorageHandles) -> Self {
        self.stores = Some(stores);
        self
    }

    /// The opened storage backend's handles, if a non-fs backend is selected.
    pub fn stores(&self) -> Option<&crate::store::StorageHandles> {
        self.stores.as_ref()
    }

    /// The repo-level shared skill registry, loaded from `dir` and cached.
    ///
    /// The first successful call parses `dir/*/SKILL.md` and caches the result;
    /// later calls return the cached registry and ignore `dir`, since the
    /// repo's skill library is immutable at runtime.
    pub fn skill_registry(&self, dir: &Path) -> crate::Result<Arc<[SkillDoc]>> {
        if let Some(cached) = self.skill_registry.get() {
            return Ok(cached.clone());
        }
        let registry: Arc<[SkillDoc]> = load_dir_skills(dir)?.into();
        // A concurrent caller may have set it first; keep whichever won.
        let _ = self.skill_registry.set(registry.clone());
        Ok(self.skill_registry.get().cloned().unwrap_or(registry))
    }

    /// Installs the injected connection seams (DNS resolver, mail sender).
    pub fn with_connections(mut self, connections: crate::server::ops::ConnectionsRuntime) -> Self {
        self.connections = connections;
        self
    }

    /// The injected connection seams for the credential surfaces.
    pub fn connections(&self) -> &crate::server::ops::ConnectionsRuntime {
        &self.connections
    }

    /// Installs platform (multi-tenant) auth. Mirrors [`Self::with_home`].
    pub fn with_platform_auth(mut self, platform_auth: PlatformAuthConfig) -> Self {
        self.config.platform_auth = Some(platform_auth);
        self
    }

    /// Installs an outbound webhook sink configuration.
    pub fn with_webhook(mut self, webhook: WebhookConfig) -> Self {
        self.config.webhook = Some(webhook);
        self
    }

    /// Sets provisioning quotas: a global cap and a per-tenant cap.
    pub fn with_quota(
        mut self,
        max_companies: Option<usize>,
        max_companies_per_tenant: Option<usize>,
    ) -> Self {
        self.config.max_companies = max_companies;
        self.config.max_companies_per_tenant = max_companies_per_tenant;
        self
    }

    /// The tenant that owns `id`, if it was provisioned in platform mode.
    pub fn owner_of(&self, id: &CompanyId) -> Option<String> {
        self.ownership
            .read()
            .expect("ownership poisoned")
            .get(id)
            .cloned()
    }

    /// Records that `tenant` owns `id`.
    pub fn set_owner(&self, id: CompanyId, tenant: impl Into<String>) {
        self.ownership
            .write()
            .expect("ownership poisoned")
            .insert(id, tenant.into());
    }

    /// Forgets the ownership record for `id` (used by archive).
    pub fn remove_owner(&self, id: &CompanyId) {
        self.ownership
            .write()
            .expect("ownership poisoned")
            .remove(id);
    }

    /// The number of companies owned by `tenant`.
    pub fn tenant_company_count(&self, tenant: &str) -> usize {
        self.ownership
            .read()
            .expect("ownership poisoned")
            .values()
            .filter(|owner| owner.as_str() == tenant)
            .count()
    }

    /// The configured webhook delivery, if any.
    pub fn webhook(&self) -> Option<&WebhookConfig> {
        self.config.webhook.as_ref()
    }

    /// Returns runtime configuration.
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// The OpenCompany home root holding company bundles.
    pub fn home(&self) -> &std::path::Path {
        &self.home
    }

    /// The registry of running companies served by this host.
    pub fn registry(&self) -> &CompanyRegistry {
        &self.registry
    }

    /// The host-global A2A replay-protection nonce cache.
    #[cfg(feature = "tinyplace")]
    pub fn nonce(&self) -> &std::sync::Arc<crate::economy::NonceCache> {
        &self.nonce
    }

    /// Returns a serializable system specification snapshot.
    pub fn spec(&self) -> AppSpec {
        AppSpec {
            name: "opencompany",
            version: VERSION,
            framework: "axum",
            modules: vec![
                "app",
                "company",
                "ports",
                "store",
                "policy",
                "brain",
                "runtime",
                "server",
                "openhuman",
                "tiny",
            ],
            runtime_modules: RuntimeModuleStatus::all(),
            openhuman_root: self
                .config
                .openhuman_root
                .as_ref()
                .map(|path| path.display().to_string()),
            api_url: self.config.api_url.clone(),
            cycles_available: self.config.cycles_available(),
        }
    }
}

/// Serializable OpenCompany runtime specification.
#[derive(Clone, Debug, Serialize)]
pub struct AppSpec {
    /// Crate name.
    pub name: &'static str,
    /// Crate version.
    pub version: &'static str,
    /// HTTP framework used by this host.
    pub framework: &'static str,
    /// First-class source modules.
    pub modules: Vec<&'static str>,
    /// Runtime module integration status.
    pub runtime_modules: Vec<RuntimeModuleStatus>,
    /// Configured OpenHuman checkout path, if any.
    pub openhuman_root: Option<String>,
    /// TinyHumans orchestration API base URL.
    pub api_url: String,
    /// Whether hosted cognition can run (hosted brain plus a credential). No
    /// secret bytes are surfaced.
    pub cycles_available: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_binds_locally() {
        assert_eq!(AppConfig::default().bind, "127.0.0.1:8080");
    }

    #[test]
    fn spec_reports_axum_framework() {
        let spec = AppState::new(AppConfig::default()).spec();

        assert_eq!(spec.framework, "axum");
        assert!(spec.modules.contains(&"server"));
    }

    #[test]
    fn host_base_url_falls_back_to_bind() {
        let config = AppConfig::default();
        assert_eq!(config.host_base_url(), "http://127.0.0.1:8080");

        let public = AppConfig {
            public_url: Some("https://acme.example".into()),
            ..AppConfig::default()
        };
        assert_eq!(public.host_base_url(), "https://acme.example");
    }

    #[test]
    fn debug_redacts_the_credential() {
        let config = AppConfig {
            tinyhumans_credential: Some(SecretValue("th_super_secret_value".into())),
            ..AppConfig::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("th_super_secret_value"));
        assert!(rendered.contains("set"));
    }

    #[test]
    fn default_config_cannot_run_cycles() {
        assert!(!AppConfig::default().cycles_available());
    }

    #[test]
    fn skill_registry_loads_the_repo_library_and_caches() {
        let state = AppState::new(AppConfig::default());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");

        let first = state.skill_registry(&dir).expect("registry loads");
        assert!(first.iter().any(|skill| skill.slug == "web-research"));
        assert!(first.iter().any(|skill| skill.slug == "weekly-report"));

        // A second call returns the same cached allocation, ignoring the path.
        let second = state
            .skill_registry(std::path::Path::new("/nonexistent"))
            .expect("cached registry");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn hosted_with_credential_can_run_cycles() {
        let config = AppConfig {
            tinyhumans_credential: Some(SecretValue("th_secret".into())),
            ..AppConfig::default()
        };
        assert!(config.cycles_available());
        assert!(AppState::new(config).spec().cycles_available);
    }
}
