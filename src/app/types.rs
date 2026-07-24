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
    /// and provisioning/suspension require the `platform` scope.
    ///
    /// When `None` there are no machine credentials at all, and every request
    /// must carry a human's session — see `server::users`. Provisioning over
    /// HTTP is then unavailable by construction; self-hosters load companies
    /// with `serve --company <dir>`.
    pub platform_auth: Option<PlatformAuthConfig>,
    /// Global cap on the number of provisioned companies. `None` = unlimited.
    pub max_companies: Option<usize>,
    /// Per-tenant cap on provisioned companies. `None` = unlimited.
    pub max_companies_per_tenant: Option<usize>,
    /// Outbound webhook delivery configuration. `None` disables webhooks.
    pub webhook: Option<WebhookConfig>,
    /// Tenant namespace for shared-single-DB deployments
    /// (`OPENCOMPANY_TENANT_ID`). When set, provisioned/booted company ids are
    /// prefixed with `<tenant>--` via [`Self::namespaced_company_id`] so many
    /// tenants sharing one logical database never collide on the `companies`
    /// unique index. `None` (the default) is a no-op: db-per-tenant and
    /// single-tenant deployments are unaffected.
    pub tenant_namespace: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".to_string(),
            openhuman_root: None,
            api_url: crate::app::config::DEFAULT_API_URL.to_string(),
            brain_mode: BrainMode::Hosted,
            tinyplace_api_url: crate::app::config::DEFAULT_TINYPLACE_API_URL.to_string(),
            public_url: None,
            tinyhumans_credential: None,
            platform_auth: None,
            max_companies: None,
            max_companies_per_tenant: None,
            webhook: None,
            tenant_namespace: None,
        }
    }
}

/// Prefixes `id` with `<tenant>--` for shared-single-DB namespacing.
///
/// Idempotent: an id already carrying the `<tenant>--` prefix is returned
/// unchanged, so applying it more than once — or to an id read back from a
/// shared DB — never double-prefixes. Both the boot path and API provisioning
/// use the workload's own [`AppConfig::tenant_namespace`], so ids stay
/// workload-local regardless of which tenant a provisioning request acts for.
pub fn namespace_company_id(tenant: &str, id: CompanyId) -> CompanyId {
    let prefix = format!("{tenant}--");
    if id.as_ref().starts_with(&prefix) {
        id
    } else {
        CompanyId::new(format!("{prefix}{}", id.as_ref()))
    }
}

/// The canonical form of a tenant identifier for ownership: the bare slug, with
/// any leading `tenant:` prefix stripped.
///
/// The two representations of the *same* tenant must compare equal. A verified
/// token's [`PlatformClaims::tenant`](crate::server::platform_auth::PlatformClaims)
/// carries the platform-issued `tenant:acme` form, while the workload's injected
/// `OPENCOMPANY_TENANT_ID` (and thus [`AppConfig::tenant_namespace`], the id
/// prefix, and shared-DB `owners` rows) is the bare slug `acme`. Recording
/// ownership under one form and authorizing against the other would lock a
/// tenant out of its own companies. Every site that stores, counts, hydrates, or
/// compares an owning tenant funnels through this one helper so `acme` and
/// `tenant:acme` are one identity end-to-end.
pub fn canonical_tenant(tenant: &str) -> &str {
    tenant.strip_prefix("tenant:").unwrap_or(tenant)
}

impl AppConfig {
    /// True when hosted cognition can run: hosted mode plus a credential.
    pub fn cycles_available(&self) -> bool {
        self.brain_mode == BrainMode::Hosted && self.tinyhumans_credential.is_some()
    }

    /// Namespaces a company id for shared-single-DB mode.
    ///
    /// Returns `<tenant>--<id>` when [`Self::tenant_namespace`] is set and `id`
    /// is not already prefixed; returns `id` unchanged when the namespace is
    /// unset (the no-op that keeps db-per-tenant deployments identical).
    /// Idempotent: an already-prefixed id passes through untouched, so applying
    /// it twice — or to an id read back from a shared DB — never double-prefixes.
    pub fn namespaced_company_id(&self, id: CompanyId) -> CompanyId {
        match &self.tenant_namespace {
            Some(tenant) => namespace_company_id(tenant, id),
            None => id,
        }
    }

    /// The host base URL to embed in published Agent Card endpoints: the
    /// configured [`Self::public_url`] when set, otherwise `http://{bind}`.
    pub fn host_base_url(&self) -> String {
        match &self.public_url {
            Some(url) => url.clone(),
            None => format!("http://{}", self.bind),
        }
    }

    /// Whether this host is reachable only from this machine.
    ///
    /// Gates behavior that is safe on a developer's laptop and unsafe anywhere
    /// else — chiefly echoing a login code in an HTTP response when no mail
    /// transport is configured (see the user-auth routes).
    ///
    /// Fails **closed**: a host it cannot prove is loopback (a DNS name, an
    /// empty host, a malformed bind, or any configured `public_url`) is treated
    /// as routable. A `public_url` means someone expects to reach this from
    /// elsewhere, which settles it regardless of the bind.
    pub fn is_local_only(&self) -> bool {
        if self.public_url.is_some() {
            return false;
        }
        bind_is_loopback(&self.bind)
    }
}

/// The host portion of a `host:port` bind string, handling the bracketed IPv6
/// form (`[::1]:8080`).
fn bind_host(bind: &str) -> &str {
    if let Some(rest) = bind.strip_prefix('[')
        && let Some((host, _)) = rest.split_once(']')
    {
        return host;
    }
    match bind.rsplit_once(':') {
        Some((host, _)) => host,
        None => bind,
    }
}

/// Whether a bind address accepts connections only from this machine.
fn bind_is_loopback(bind: &str) -> bool {
    let host = bind_host(bind);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppConfig")
            .field("bind", &self.bind)
            .field("openhuman_root", &self.openhuman_root)
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
            .field("tenant_namespace", &self.tenant_namespace)
            .finish()
    }
}

/// Shared application state passed to Axum handlers.
#[derive(Clone)]
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
    /// The memory engine overlay selected by `OPENCOMPANY_MEMORY`, when it is
    /// not the base store's own memory. Provisioning and boot apply it after
    /// `stores` so a dedicated engine (TinyCortex) backs recall on top of any
    /// base backend. `None` means the base backend's memory is used unchanged.
    memory_overlay: Option<crate::store::MemoryOverlay>,
    /// The repo-level shared skill library directory (`skills/`), set on the
    /// serve path. `None` in platform-provisioned mode (no repo checkout), where
    /// the `skillRegistry` query degrades to empty.
    skills_root: Option<std::path::PathBuf>,
    /// Cache of the repo-level shared skill registry (`skills/*/SKILL.md`).
    /// Populated on first read via [`AppState::skill_registry`]; never
    /// invalidated because the repo's skill library is immutable at runtime.
    skill_registry: Arc<OnceLock<Arc<[SkillDoc]>>>,
    /// The GraphQL read-plane schema, built once at construction and reused for
    /// every `/graphql` request (per-request auth is injected as request data).
    schema: crate::server::graphql::OcSchema,
    /// Injected network seams for the credential surfaces (DNS resolver, mail
    /// sender). Empty by default so the build stays offline.
    connections: crate::server::ops::ConnectionsRuntime,
    /// Cross-origin allowlist. Empty (the default) means CORS is off, which is
    /// correct for every same-origin deployment.
    cors: crate::server::cors::CorsConfig,
    /// Host-global replay-protection cache shared across every inbound A2A
    /// request. Gated behind `tinyplace` so the default build links no crypto.
    #[cfg(feature = "tinyplace")]
    nonce: std::sync::Arc<crate::economy::NonceCache>,
    /// In-flight console MCP OAuth flows, keyed by the opaque `state` the browser
    /// round-trips (issue #90). The `/mcp/servers/{name}/oauth/start` route parks
    /// a [`PendingOAuth`](crate::company::mcp_oauth::PendingOAuth) here; the
    /// unauthenticated `/oauth/mcp/callback` route takes it back out by `state`.
    /// Gated behind `mcp` so the default build links none of the OAuth path.
    /// Each entry carries the [`Instant`](std::time::Instant) it was parked so
    /// abandoned flows (closed tab, double-click, pre-callback error) can be
    /// swept — they hold a `client_secret` + `code_verifier` that must not live
    /// in memory forever.
    #[cfg(feature = "mcp")]
    oauth_pending: Arc<
        std::sync::Mutex<
            HashMap<String, (std::time::Instant, crate::company::mcp_oauth::PendingOAuth)>,
        >,
    >,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The GraphQL schema carries no useful debug state and is not `Debug`.
        f.debug_struct("AppState")
            .field("config", &self.config)
            .field("registry", &self.registry)
            .field("home", &self.home)
            .field("stores", &self.stores)
            .finish_non_exhaustive()
    }
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
            memory_overlay: None,
            skills_root: None,
            skill_registry: Arc::new(OnceLock::new()),
            schema: crate::server::graphql::build_schema(),
            connections: crate::server::ops::ConnectionsRuntime::new(),
            cors: crate::server::cors::CorsConfig::default(),
            #[cfg(feature = "tinyplace")]
            nonce: std::sync::Arc::new(crate::economy::NonceCache::new()),
            #[cfg(feature = "mcp")]
            oauth_pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Sets the OpenCompany home root used to resolve company identities.
    pub fn with_home(mut self, home: impl Into<std::path::PathBuf>) -> Self {
        self.home = home.into();
        self
    }

    /// Sets the repo-level shared skill library directory (`skills/`) backing the
    /// top-level `skillRegistry` query. Set on the serve path; unset in
    /// platform-provisioned mode.
    pub fn with_skills_root(mut self, skills_root: impl Into<std::path::PathBuf>) -> Self {
        self.skills_root = Some(skills_root.into());
        self
    }

    /// The repo-level shared skill library directory, when set.
    pub fn skills_root(&self) -> Option<&std::path::Path> {
        self.skills_root.as_deref()
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

    /// Installs the memory engine overlay selected by `OPENCOMPANY_MEMORY`.
    pub fn with_memory_overlay(mut self, overlay: crate::store::MemoryOverlay) -> Self {
        self.memory_overlay = Some(overlay);
        self
    }

    /// The memory engine overlay, if one is selected (`OPENCOMPANY_MEMORY`).
    pub fn memory_overlay(&self) -> Option<&crate::store::MemoryOverlay> {
        self.memory_overlay.as_ref()
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

    /// Sets the cross-origin allowlist. Empty (the default) leaves CORS off.
    pub fn with_cors(mut self, cors: crate::server::cors::CorsConfig) -> Self {
        self.cors = cors;
        self
    }

    /// The cross-origin allowlist.
    pub fn cors(&self) -> &crate::server::cors::CorsConfig {
        &self.cors
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
    ///
    /// The tenant is stored in [`canonical_tenant`] form so the map always keys
    /// ownership by the bare slug, whatever representation the caller passes
    /// (a token's `tenant:acme` claim or the workload's bare `acme` namespace).
    pub fn set_owner(&self, id: CompanyId, tenant: impl Into<String>) {
        let tenant = canonical_tenant(&tenant.into()).to_string();
        self.ownership
            .write()
            .expect("ownership poisoned")
            .insert(id, tenant);
    }

    /// Forgets the ownership record for `id` (used by archive).
    pub fn remove_owner(&self, id: &CompanyId) {
        self.ownership
            .write()
            .expect("ownership poisoned")
            .remove(id);
    }

    /// The number of companies owned by `tenant`.
    ///
    /// Both the stored owners and the query are compared in [`canonical_tenant`]
    /// form, so a `tenant:acme` claim and a bare `acme` namespace count the same
    /// tenant's companies.
    pub fn tenant_company_count(&self, tenant: &str) -> usize {
        let tenant = canonical_tenant(tenant);
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

    /// The prebuilt GraphQL read-plane schema.
    pub fn schema(&self) -> &crate::server::graphql::OcSchema {
        &self.schema
    }

    /// The host-global A2A replay-protection nonce cache.
    #[cfg(feature = "tinyplace")]
    pub fn nonce(&self) -> &std::sync::Arc<crate::economy::NonceCache> {
        &self.nonce
    }

    /// How long a parked OAuth flow stays reclaimable before it's swept. Longer
    /// than any realistic operator round-trip through the authorization server,
    /// short enough that an abandoned flow's secrets don't linger.
    #[cfg(feature = "mcp")]
    const OAUTH_PENDING_TTL: std::time::Duration = std::time::Duration::from_secs(600);

    /// Parks an in-flight console MCP OAuth flow keyed by its opaque `state`, to
    /// be reclaimed by the callback route. See issue #90. Sweeps flows older than
    /// [`OAUTH_PENDING_TTL`](Self::OAUTH_PENDING_TTL) on every park so an
    /// abandoned sign-in (closed tab, double-click, pre-callback error) can't
    /// retain its `client_secret`/`code_verifier` for the life of the process.
    #[cfg(feature = "mcp")]
    pub fn park_oauth(&self, state: String, pending: crate::company::mcp_oauth::PendingOAuth) {
        let mut guard = self.oauth_pending.lock().expect("oauth pending poisoned");
        guard.retain(|_, (parked_at, _)| parked_at.elapsed() < Self::OAUTH_PENDING_TTL);
        guard.insert(state, (std::time::Instant::now(), pending));
    }

    /// Takes (removes) a parked console MCP OAuth flow by its `state`. `None` when
    /// the state is unknown, already consumed (single-use, so a replayed
    /// callback can't re-exchange), or swept as stale past
    /// [`OAUTH_PENDING_TTL`](Self::OAUTH_PENDING_TTL).
    #[cfg(feature = "mcp")]
    pub fn take_oauth(&self, state: &str) -> Option<crate::company::mcp_oauth::PendingOAuth> {
        let mut guard = self.oauth_pending.lock().expect("oauth pending poisoned");
        let entry = guard.remove(state)?;
        let (parked_at, pending) = entry;
        // A flow that outlived its TTL is treated as expired, not reclaimable.
        if parked_at.elapsed() >= Self::OAUTH_PENDING_TTL {
            return None;
        }
        Some(pending)
    }

    /// Test-only: park a flow with an explicit parked-at instant so the TTL
    /// expiry + sweep paths can be exercised without waiting real time.
    #[cfg(all(test, feature = "mcp"))]
    fn park_oauth_at(
        &self,
        state: String,
        pending: crate::company::mcp_oauth::PendingOAuth,
        parked_at: std::time::Instant,
    ) {
        self.oauth_pending
            .lock()
            .expect("oauth pending poisoned")
            .insert(state, (parked_at, pending));
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

    fn bound_to(bind: &str) -> AppConfig {
        AppConfig {
            bind: bind.to_string(),
            ..AppConfig::default()
        }
    }

    #[test]
    fn loopback_binds_are_local_only() {
        for bind in [
            "127.0.0.1:8080",
            "127.0.0.53:8080",
            "localhost:8080",
            "LocalHost:8080",
            "[::1]:8080",
        ] {
            assert!(bound_to(bind).is_local_only(), "{bind} is loopback");
        }
    }

    #[test]
    fn routable_binds_are_not_local_only() {
        for bind in ["0.0.0.0:8080", "192.168.1.10:8080", "[::]:8080"] {
            assert!(!bound_to(bind).is_local_only(), "{bind} is routable");
        }
    }

    #[test]
    fn an_unprovable_bind_host_fails_closed() {
        // A DNS name could resolve anywhere, and a malformed bind is not
        // evidence of safety. Neither may unlock loopback-only behavior.
        for bind in ["example.com:8080", ":8080", "garbage", ""] {
            assert!(
                !bound_to(bind).is_local_only(),
                "{bind:?} is not provably loopback and must fail closed"
            );
        }
    }

    #[test]
    fn a_public_url_settles_it_regardless_of_bind() {
        // Someone expects to reach this from elsewhere. Whatever the bind says,
        // this host is not a private laptop.
        let config = AppConfig {
            public_url: Some("https://acme.example".into()),
            ..bound_to("127.0.0.1:8080")
        };
        assert!(!config.is_local_only());
    }

    #[test]
    fn spec_reports_axum_framework() {
        let spec = AppState::new(AppConfig::default()).spec();

        assert_eq!(spec.framework, "axum");
        assert!(spec.modules.contains(&"server"));
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn parked_oauth_flow_is_single_use() {
        use crate::company::mcp_oauth::PendingOAuth;
        use crate::ports::types::CompanyId;

        let state = AppState::new(AppConfig::default());
        let pending = PendingOAuth {
            company_id: CompanyId::new("acme"),
            server_name: "notion".into(),
            code_verifier: "verifier".into(),
            client_id: "cid".into(),
            client_secret: Some("secret".into()),
            token_endpoint: "https://as.example/token".into(),
            redirect_uri: "https://acme.example/oauth/mcp/callback".into(),
        };

        state.park_oauth("state-1".into(), pending.clone());
        // First take reclaims it; a replayed callback finds nothing (single-use).
        assert!(state.take_oauth("state-1").is_some());
        assert!(state.take_oauth("state-1").is_none());
        // An unknown state is always None.
        assert!(state.take_oauth("never-parked").is_none());
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn parked_oauth_flow_expires_and_is_swept() {
        use crate::company::mcp_oauth::PendingOAuth;
        use crate::ports::types::CompanyId;
        use std::time::{Duration, Instant};

        let state = AppState::new(AppConfig::default());
        let pending = |server: &str| PendingOAuth {
            company_id: CompanyId::new("acme"),
            server_name: server.into(),
            code_verifier: "verifier".into(),
            client_id: "cid".into(),
            client_secret: Some("secret".into()),
            token_endpoint: "https://as.example/token".into(),
            redirect_uri: "https://acme.example/oauth/mcp/callback".into(),
        };
        let stale_at = Instant::now() - (AppState::OAUTH_PENDING_TTL + Duration::from_secs(1));

        // Stale-on-read: an entry parked past its TTL is rejected (and removed).
        state.park_oauth_at("expired".into(), pending("notion"), stale_at);
        assert!(state.take_oauth("expired").is_none());

        // Sweep-on-park: parking a fresh flow evicts any stale sibling first, so
        // an abandoned flow's secrets can't outlive the TTL even if never taken.
        state.park_oauth_at("stale".into(), pending("slack"), stale_at);
        state.park_oauth("fresh".into(), pending("github"));
        assert!(state.take_oauth("stale").is_none());
        assert!(state.take_oauth("fresh").is_some());
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
    fn namespaced_company_id_is_noop_when_unset() {
        let config = AppConfig::default();
        assert!(config.tenant_namespace.is_none());
        let id = CompanyId::new("agentic-software-company");
        assert_eq!(config.namespaced_company_id(id.clone()), id);
    }

    #[test]
    fn namespaced_company_id_prefixes_when_set() {
        let config = AppConfig {
            tenant_namespace: Some("acme".into()),
            ..AppConfig::default()
        };
        assert_eq!(
            config.namespaced_company_id(CompanyId::new("agentic-software-company")),
            CompanyId::new("acme--agentic-software-company")
        );
    }

    #[test]
    fn namespaced_company_id_is_idempotent() {
        let config = AppConfig {
            tenant_namespace: Some("acme".into()),
            ..AppConfig::default()
        };
        let once = config.namespaced_company_id(CompanyId::new("agentic-software-company"));
        let twice = config.namespaced_company_id(once.clone());
        assert_eq!(once, twice);
        assert_eq!(once, CompanyId::new("acme--agentic-software-company"));
    }

    #[test]
    fn canonical_tenant_strips_prefix() {
        assert_eq!(canonical_tenant("tenant:acme"), "acme");
        assert_eq!(canonical_tenant("acme"), "acme");
        // Only the leading `tenant:` is stripped, and only once.
        assert_eq!(canonical_tenant("company:acme"), "company:acme");
        assert_eq!(canonical_tenant("tenant:tenant:x"), "tenant:x");
    }

    #[test]
    fn ownership_is_keyed_canonically_across_representations() {
        let state = AppState::new(AppConfig::default());
        let id = CompanyId::new("acme--acme");

        // A row stored in the claim shape (as hydration would set it) is keyed by
        // the bare slug, so a query in either representation finds it.
        state.set_owner(id.clone(), "tenant:acme");
        assert_eq!(state.owner_of(&id).as_deref(), Some("acme"));
        assert_eq!(state.tenant_company_count("acme"), 1);
        assert_eq!(state.tenant_company_count("tenant:acme"), 1);
        assert_eq!(state.tenant_company_count("tenant:globex"), 0);

        // Re-recording under the bare form is the same identity, not a second.
        state.set_owner(id.clone(), "acme");
        assert_eq!(state.tenant_company_count("tenant:acme"), 1);
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
