use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use serde::Serialize;

use crate::app::config::{AuthMode, BrainMode, EnvSource, redacted};
use crate::company::{CredentialSource, SkillDoc, TinyhumansTokenSource, load_dir_skills};
use crate::ports::normalize_email;
use crate::ports::types::{CompanyId, SecretValue};
use crate::runtime::CompanyRegistry;
use crate::server::platform_auth::PlatformAuthConfig;
use crate::server::webhook::WebhookConfig;
use crate::{BUILD_COMMIT, VERSION, tiny::RuntimeModuleStatus};

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
    /// An operator-set display name for this instance
    /// (`OPENCOMPANY_INSTANCE_NAME`), surfaced by `/spec` so a client holding
    /// several connections can show something friendlier than a URL. Purely
    /// cosmetic and untrusted — it never selects, authorizes, or routes.
    pub instance_name: Option<String>,
    /// Which brain the runtime drives.
    pub brain_mode: BrainMode,
    /// tiny.place economy API base URL.
    pub tinyplace_api_url: String,
    /// Public host base URL advertised in published Agent Cards. When `None`,
    /// the card endpoint falls back to `http://{bind}`.
    pub public_url: Option<String>,
    /// A **static** TinyHumans hosted-brain credential, if configured. Redacted
    /// in `Debug`. This is only the static tier: a hosted tenant instead reads a
    /// platform-projected token file, so ask
    /// [`credential_available`](Self::credential_available) — not this field —
    /// whether a credential can be obtained.
    pub tinyhumans_credential: Option<SecretValue>,
    /// Install-wide default MCP servers (issue #527) — the normalized
    /// `[[default_mcp_server]]` list from this instance's `config.toml`, handed
    /// to every company this host builds so a fresh install has working tools
    /// with no user setup.
    ///
    /// Normalized by
    /// [`normalize_default_servers`](crate::company::mcp::normalize_default_servers)
    /// at whichever boundary populates it — the same function
    /// [`RuntimeConfig`](crate::app::config::RuntimeConfig) uses, so the two
    /// config structs cannot disagree about what is shippable. Empty is the
    /// default and means "ship no defaults", never "use a built-in set".
    pub default_mcp_servers: Vec<crate::company::McpServer>,
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
    /// The byte limits every company's workspace tree is held to (issue #553),
    /// resolved from the `[workspace]` section of `config.toml`. Threaded onto
    /// each company's builder so the store-level quota decorator is configured
    /// from one place rather than re-read per company.
    pub workspace_quota: crate::runtime::WorkspaceQuota,
    /// Whether each agent's private filesystem workspace is Git-backed and
    /// automatically checkpointed after tool calls.
    pub workspace_git_enabled: bool,
    /// Tenant namespace for shared-single-DB deployments
    /// (`OPENCOMPANY_TENANT_ID`). When set, provisioned/booted company ids are
    /// prefixed with `<tenant>--` via [`Self::namespaced_company_id`] so many
    /// tenants sharing one logical database never collide on the `companies`
    /// unique index. `None` (the default) is a no-op: db-per-tenant and
    /// single-tenant deployments are unaffected.
    pub tenant_namespace: Option<String>,
    /// One address the deployment bootstraps as an admin of every company it
    /// serves (`OPENCOMPANY_ADMIN_EMAIL`), on top of each manifest's
    /// `[users] admins`.
    ///
    /// A company the *platform* provisions has no one in its manifest: the
    /// person who asked for it is recorded on the control plane's tenant row,
    /// which the manifest never sees. With an empty `[users] admins` and no
    /// invite outstanding nobody is eligible, and there is no operator token to
    /// send the first invite with — the company is unreachable by the human who
    /// created it (issue #321). This is the seam the platform injects that
    /// address through.
    ///
    /// It is a *standing invite*, not an account: exactly like a manifest
    /// admin, listing the address makes it eligible to log in, and only
    /// redeeming a link mints the user. Unsetting it stops future bootstrapping
    /// and does not delete an account it already created. `None` — and an empty
    /// or whitespace-only value — is a clean no-op.
    pub admin_email: Option<String>,
    /// A host-wide override of every company's `[users].mode`
    /// (`OPENCOMPANY_AUTH_MODE`, or `auth_mode` in `config.toml`).
    ///
    /// `None` — the normal case — leaves each company to name its own sign-in
    /// mode in its manifest. It exists because two deployments own the answer
    /// rather than the company definition does: the packaged desktop app, which
    /// forces [`AuthMode::None`](crate::app::config::AuthMode::None) because
    /// there is only ever the person at the machine, and a hosting platform,
    /// which must be able to guarantee a mode across every tenant it builds
    /// regardless of what a tenant wrote.
    ///
    /// Carried to each company by
    /// [`RuntimeBuilder::with_auth_mode_override`](crate::runtime::RuntimeBuilder::with_auth_mode_override),
    /// which is where it beats the manifest.
    pub auth_mode_override: Option<AuthMode>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".to_string(),
            openhuman_root: None,
            api_url: crate::app::config::DEFAULT_API_URL.to_string(),
            instance_name: None,
            brain_mode: BrainMode::Hosted,
            tinyplace_api_url: crate::app::config::DEFAULT_TINYPLACE_API_URL.to_string(),
            public_url: None,
            tinyhumans_credential: None,
            default_mcp_servers: Vec::new(),
            platform_auth: None,
            max_companies: None,
            max_companies_per_tenant: None,
            workspace_quota: crate::runtime::WorkspaceQuota::default(),
            workspace_git_enabled: false,
            webhook: None,
            tenant_namespace: None,
            admin_email: None,
            auth_mode_override: None,
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

/// A tenant namespace must not contain the `--` id delimiter.
///
/// [`namespace_company_id`] and `app::orphans::filter_to_tenant` both encode a
/// tenant as the `<tenant>--` prefix, so a namespace containing `--` makes the
/// encoding ambiguous: `acme` namespacing `other--company` collides with
/// `acme--other` namespacing `company`, and the shorter tenant's filter then
/// claims the longer tenant's ids. Reject the delimiter at the boundary that
/// reads `OPENCOMPANY_TENANT_ID` so a malformed namespace fails loudly instead
/// of silently misattributing another tenant's companies.
pub fn validate_tenant_namespace(tenant: &str) -> Result<(), String> {
    if tenant.contains("--") {
        Err(format!(
            "tenant namespace `{tenant}` contains `--`, which is the company-id \
             delimiter; a namespace may not contain it"
        ))
    } else {
        Ok(())
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
    /// True when hosted cognition can run: hosted brain mode plus a credential
    /// this instance can **obtain** — see [`Self::credential_available`].
    pub fn cycles_available(&self) -> bool {
        self.cycles_available_in(&crate::app::config::ProcessEnv)
    }

    /// [`Self::cycles_available`] against an explicit environment seam.
    pub fn cycles_available_in(&self, env: &dyn EnvSource) -> bool {
        self.brain_mode == BrainMode::Hosted && self.credential_available_in(env)
    }

    /// Whether a TinyHumans credential can be obtained at all — a static one
    /// resolved into [`Self::tinyhumans_credential`], **or** a platform-projected
    /// token source ([`TinyhumansTokenSource::from_env`]).
    ///
    /// The question changed from "do I hold a secret?" to "can I get a token?".
    /// A hosted tenant holds nothing: the platform projects a short-lived,
    /// audience-bound token into a file that rotates in place. Asking about a
    /// stored secret would report such an instance as unable to think.
    ///
    /// The projected file is read from the **environment** rather than threaded
    /// through a config layer on purpose: the platform injects it into the pod and
    /// an operator never configures it, so there is nothing to resolve by
    /// precedence. [`Self::credential_available_in`] takes the seam explicitly for
    /// tests.
    pub fn credential_available(&self) -> bool {
        self.credential_available_in(&crate::app::config::ProcessEnv)
    }

    /// [`Self::credential_available`] against an explicit environment seam.
    pub fn credential_available_in(&self, env: &dyn EnvSource) -> bool {
        self.tinyhumans_credential.is_some() || TinyhumansTokenSource::from_env(env).is_some()
    }

    /// Which tier the credential comes from, for operator-facing output. The
    /// projected file wins, mirroring [`TinyhumansTokenSource::from_env`].
    pub fn credential_source_in(&self, env: &dyn EnvSource) -> CredentialSource {
        match TinyhumansTokenSource::from_env(env) {
            Some(source) => source.credential_source(),
            None if self.tinyhumans_credential.is_some() => CredentialSource::Static,
            None => CredentialSource::None,
        }
    }

    /// The deployment-wide bootstrap admin address, normalized, or `None`.
    ///
    /// Normalization goes through the same [`normalize_email`] the manifest and
    /// login paths use, so `Ada@Example.com ` and `ada@example.com` name one
    /// address here exactly as they do there — an injected value that only
    /// matched with the right capitalization would be a lockout that looks like
    /// a typo. A value that is empty or trims to nothing is `None`: the
    /// platform renders this variable for every tenant, and a tenant with no
    /// recorded creator must be indistinguishable from one deployed before the
    /// variable existed.
    pub fn bootstrap_admin(&self) -> Option<String> {
        self.admin_email
            .as_deref()
            .map(normalize_email)
            .filter(|email| !email.is_empty())
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

    /// The base URL a third party can deliver an inbound webhook to, if there
    /// is one — otherwise `None`.
    ///
    /// Distinct from [`Self::host_base_url`], which always answers *something*
    /// (falling back to `http://{bind}`) because an Agent Card must carry an
    /// endpoint. A webhook URL has no such fallback: a provider that cannot
    /// reach the URL simply never delivers. So this is `Some` only when an
    /// explicit `public_url` is configured **and** it is `https` — Telegram
    /// (issue #203) refuses any other scheme for `setWebhook`, and the
    /// `http://127.0.0.1:<port>` bind fallback is unreachable from the internet
    /// by construction. Callers use it to decide whether to offer a webhook at
    /// all, rather than showing an operator a URL that can never work.
    pub fn public_webhook_base_url(&self) -> Option<&str> {
        self.public_url
            .as_deref()
            .map(str::trim)
            .map(|url| url.trim_end_matches('/'))
            .filter(|url| {
                url.len() > "https://".len()
                    && url
                        .get(.."https://".len())
                        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
            })
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
    /// The root `config.toml` — and everything the first-run setup flow
    /// (`crate::server::setup`) reads and writes — resolves under.
    ///
    /// `None` means "same as [`Self::home`]", which is the aligned shape
    /// [`crate::store::home_divergence_warning`] treats as silent: a hosted
    /// tenant (`--home "$OPENCOMPANY_DATA_DIR"`), `OPENCOMPANY_DATA_DIR` alone,
    /// and the untouched local default all resolve `home` and the instance's
    /// `data_dir_from_env()` to the same path.
    ///
    /// Set explicitly whenever a deployment's `--home` (company bundles) and
    /// data root (the instance workspace `config.toml` lives beside) diverge —
    /// see `serve` in `src/bin/opencompany.rs`. Without this, startup resolves
    /// `setup_completed_at` from the data root while setup itself resolved
    /// `state.home()`, so a completed setup could read back as incomplete
    /// (and vice versa) on exactly the deployments the divergence warning
    /// already flags.
    config_root: Option<std::path::PathBuf>,
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
    /// `stores` so a dedicated provider can back recall on top of any
    /// base backend. `None` means the base backend's memory is used unchanged.
    ///
    /// Behind a lock because the engine is no longer decided only at boot: the
    /// console's engine route opens a replacement overlay and swaps it in, then
    /// rebuilds each registered company so the new ports are actually in force
    /// (`crate::server::ops::memory_engine`). Same shape, and the same reason,
    /// as [`Self::auth_mode_override`] — a choice an operator makes while the
    /// process is running, which telling them to restart for would defeat.
    memory_overlay: Arc<RwLock<Option<crate::store::MemoryOverlay>>>,
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
    /// The hub exchange backing `…/auth/hub`. `None` (the default, and every
    /// self-hosted host) means the console offers no ecosystem sign-in at all,
    /// rather than offering a button that leads nowhere.
    hub_identity: Option<Arc<dyn crate::server::hub_identity::HubIdentityExchange>>,
    /// Cross-origin allowlist. Empty (the default) means CORS is off, which is
    /// correct for every same-origin deployment.
    cors: crate::server::cors::CorsConfig,
    /// This host's stable public identity, minted on first boot and cached for
    /// the process. Lazy because it is a disk read that only `/spec` needs, and
    /// `AppState::new` is deliberately IO-free.
    instance_id: Arc<OnceLock<String>>,
    /// Who is currently present, per company.
    ///
    /// Host-global and in-memory, like the live turn bus it publishes
    /// alongside — presence is a lease, not a record, so it has no port and no
    /// backend. See [`crate::server::presence`] for the TTL contract and for
    /// why a second replica knowing nothing about this one is acceptable.
    presence: Arc<crate::server::presence::PresenceRegistry>,
    /// Which storage backend is serving the durable ports. Reported by `/spec`
    /// as a kind only — never a path or a connection string.
    storage_kind: crate::store::StorageKind,
    /// Whether the first-run setup flow (`crate::server::setup`) has already run
    /// against this data root.
    ///
    /// Seeded at boot from `config.toml`'s `setup_completed_at` and flipped by
    /// `POST /api/v1/setup`. Held in memory rather than re-read per request: the
    /// setup route is the only writer, and `/spec` — which reports it so an
    /// unauthenticated console can decide between the wizard and the sign-in
    /// form — would otherwise take a disk read on every poll. Shared so the
    /// clone every handler holds observes the flip.
    setup_complete: Arc<AtomicBool>,
    /// The **live** host-wide sign-in mode, seeded at construction from
    /// [`AppConfig::auth_mode_override`].
    ///
    /// Separate from the `AppConfig` field because that one is the value boot
    /// resolved and can never change, and this one has to: the first-run setup
    /// flow writes `auth_mode` and then rebuilds the affected companies in
    /// place, so the mode the rebuild reads must be the one just chosen rather
    /// than the one the process started with. Reading the frozen field there
    /// made "no sign-in" apply only after the operator restarted the host by
    /// hand — a setting that appeared to save and then did nothing.
    ///
    /// A lock rather than an atomic because [`AuthMode`] is not a primitive;
    /// it is read once per company build, never on a request path.
    auth_mode_override: Arc<RwLock<Option<AuthMode>>>,
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
    /// Issue #290: this host's ability to rebuild a registered company's runtime
    /// in place, so a first-time inference config takes effect without a process
    /// restart.
    ///
    /// Wired by the binary, which is where the builder inputs (harness pool,
    /// OpenHuman RPC, managed backends, per-tenant mailbox) are assembled.
    /// `None` — every test host, and any embedder that does not wire one — keeps
    /// the pre-#290 behaviour: the inference status still reports
    /// `restartRequired` and the console still says so, which is the honest
    /// answer when a rebuild is genuinely unavailable.
    rebuilder: Option<Arc<dyn crate::runtime::RuntimeRebuilder>>,
    /// Where this host reports product analytics, if anywhere (issue #1739).
    ///
    /// Held here because it is a **process-wide** decision — one deployment
    /// kind, one identity, one destination — that every company's builder then
    /// inherits, and threading it separately to boot, to provisioning and to
    /// the rebuilder is how one of the three comes to be missed. The default is
    /// [`NullTracker`](crate::analytics::NullTracker): a state nobody wired
    /// reports nothing, which is what every test, every desktop build and every
    /// self-hosted install gets.
    analytics: Arc<dyn crate::analytics::Tracker>,
    /// Builds the engine for a `transport = "local"` `acp` harness (issue
    /// #1245). `None` — every test host, and any embedder that does not wire
    /// one — leaves every such harness `unavailable`. Only the desktop shell
    /// has an implementation to give this; it lives at
    /// [`crate::ports::acp::AcpAgentFactory`], ungated, for the same reason
    /// [`rebuilder`](Self::rebuilder) above is: the desktop supplies it, this
    /// crate only defines the seam.
    acp_agents: Option<Arc<dyn crate::ports::acp::AcpAgentFactory>>,
    /// Live inbound ACP sessions. Kept on the host, rather than on a company,
    /// because one ACP connection may open sessions for several companies.
    #[cfg(feature = "acp")]
    acp_sessions: Arc<crate::server::acp::SessionRegistry>,
    /// The boot-only builder inputs recorded per company at registration, so a
    /// rebuild configures the successor exactly as boot configured its
    /// predecessor. See [`BootInputs`](crate::runtime::BootInputs) for why
    /// `--discoverable` in particular cannot be recovered any other way.
    boot_inputs: Arc<RwLock<HashMap<CompanyId, crate::runtime::BootInputs>>>,
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
        let auth_mode_override = Arc::new(RwLock::new(config.auth_mode_override));
        Self {
            config,
            auth_mode_override,
            registry: CompanyRegistry::new(),
            home: std::path::PathBuf::from("."),
            config_root: None,
            ownership: Arc::new(RwLock::new(HashMap::new())),
            stores: None,
            memory_overlay: Arc::new(RwLock::new(None)),
            skills_root: None,
            skill_registry: Arc::new(OnceLock::new()),
            instance_id: Arc::new(OnceLock::new()),
            presence: Arc::new(crate::server::presence::PresenceRegistry::new()),
            storage_kind: crate::store::StorageKind::default(),
            // Fails "not set up", so a host that never calls `with_setup_complete`
            // — every test fixture — presents the wizard rather than silently
            // claiming a configuration it does not have.
            setup_complete: Arc::new(AtomicBool::new(false)),
            schema: crate::server::graphql::build_schema(),
            connections: crate::server::ops::ConnectionsRuntime::new(),
            hub_identity: None,
            cors: crate::server::cors::CorsConfig::default(),
            #[cfg(feature = "tinyplace")]
            nonce: std::sync::Arc::new(crate::economy::NonceCache::new()),
            #[cfg(feature = "mcp")]
            oauth_pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            analytics: crate::analytics::null_tracker(),
            rebuilder: None,
            acp_agents: None,
            #[cfg(feature = "acp")]
            acp_sessions: Arc::new(crate::server::acp::SessionRegistry::new()),
            boot_inputs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Wires this host's analytics tracker (issue #1739).
    pub fn with_analytics(mut self, analytics: Arc<dyn crate::analytics::Tracker>) -> Self {
        self.analytics = analytics;
        self
    }

    /// Where this host reports analytics. A [`NullTracker`](crate::analytics::NullTracker)
    /// unless something wired one, which is every build but a hosted tenant's.
    pub fn analytics(&self) -> Arc<dyn crate::analytics::Tracker> {
        self.analytics.clone()
    }

    /// Wires this host's in-place runtime rebuilder (issue #290).
    pub fn with_rebuilder(mut self, rebuilder: Arc<dyn crate::runtime::RuntimeRebuilder>) -> Self {
        self.rebuilder = Some(rebuilder);
        self
    }

    /// Wires this host's local-transport ACP agent factory (issue #1245).
    pub fn with_acp_agents(mut self, factory: Arc<dyn crate::ports::acp::AcpAgentFactory>) -> Self {
        self.acp_agents = Some(factory);
        self
    }

    /// This host's local-transport ACP agent factory, when one is wired.
    pub fn acp_agents(&self) -> Option<Arc<dyn crate::ports::acp::AcpAgentFactory>> {
        self.acp_agents.clone()
    }

    /// Whether a `transport = "local"` ACP harness can actually run here.
    ///
    /// Issue #1814. Two callers — the harness picker and the teammate `PATCH`
    /// validator — used to ask [`acp_agents`](Self::acp_agents) directly, which
    /// answers a different question: whether a factory was HANDED OVER, not
    /// whether this build can use one. Those coincide on every server build and
    /// on a desktop compiled with `acp`, and diverge on exactly one
    /// configuration — a desktop compiled WITHOUT it:
    ///
    /// * [`with_acp_agents`](Self::with_acp_agents) is deliberately ungated, so
    ///   that `src-tauri` can hand over a factory without pulling the whole
    ///   embedded harness in behind `crate::harness` (`crate::ports::acp` exists
    ///   for that reason). The desktop shell calls it unconditionally.
    /// * The runtime cannot use what it was given: `RuntimeBuilder` forces
    ///   `acp_agents = None` under `cfg(not(feature = "acp"))`, and
    ///   `lanes::resolve_acp_engine` is an unconditional `Err` there — its
    ///   factory parameter is typed `Infallible`, so `Some` is uninhabited.
    ///
    /// The result was a picker that offered `claude` and `codex`, a `PATCH`
    /// that accepted the binding, and then every turn failing with `lanes.rs`'s
    /// "run it from the desktop app" — advice for somebody already in it.
    ///
    /// One method rather than the same conjunct at both call sites: they were
    /// already copy-paste siblings, comments included, and a predicate the two
    /// can state differently is what opened the gap in the first place.
    pub fn can_run_local_acp(&self) -> bool {
        cfg!(feature = "acp") && self.acp_agents.is_some()
    }

    /// Sessions opened through the host's ACP HTTP transport.
    #[cfg(feature = "acp")]
    pub fn acp_sessions(&self) -> Arc<crate::server::acp::SessionRegistry> {
        Arc::clone(&self.acp_sessions)
    }

    /// This host's in-place runtime rebuilder, when one is wired.
    pub fn rebuilder(&self) -> Option<Arc<dyn crate::runtime::RuntimeRebuilder>> {
        self.rebuilder.clone()
    }

    /// Whether this host can rebuild a registered company's runtime in place
    /// (issue #290) — the capability behind every surface that offers to apply
    /// a configuration change without a process restart.
    ///
    /// [`crate::server::setup`] and [`crate::server::ops::memory_engine`]
    /// establish the same fact by *attempting* a rebuild and reporting the
    /// failure. That is the right shape for an action already under way, and
    /// the wrong one for a surface deciding whether to *offer* the action at
    /// all: a console that cannot ask up front renders a control whose only
    /// possible outcome is the `Config` error [`rebuild_company`] returns
    /// (issue #1736). Asking is what lets it say "not on this host" instead.
    ///
    /// [`rebuild_company`]: crate::runtime::rebuild_company
    pub fn can_rebuild_in_place(&self) -> bool {
        self.rebuilder.is_some()
    }

    /// Records the boot-only builder inputs for `id`, at registration.
    ///
    /// Cloned state shares this map (it is `Arc`-backed), so a company
    /// registered after the router was built is still reachable by a rebuild.
    pub fn set_boot_inputs(&self, id: CompanyId, inputs: crate::runtime::BootInputs) {
        self.boot_inputs
            .write()
            .expect("boot inputs poisoned")
            .insert(id, inputs);
    }

    /// The boot-only builder inputs recorded for `id`, or the defaults when the
    /// company was registered without any (a platform-provisioned tenant has no
    /// source directory and was never `--discoverable`).
    pub fn boot_inputs(&self, id: &CompanyId) -> crate::runtime::BootInputs {
        self.boot_inputs
            .read()
            .expect("boot inputs poisoned")
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Sets the OpenCompany home root used to resolve company identities.
    pub fn with_home(mut self, home: impl Into<std::path::PathBuf>) -> Self {
        self.home = home.into();
        self
    }

    /// Sets the root `config.toml` — and the first-run setup flow — resolves
    /// under, when it differs from [`Self::with_home`]. See the `config_root`
    /// field doc for when that happens and why it matters.
    pub fn with_config_root(mut self, config_root: impl Into<std::path::PathBuf>) -> Self {
        self.config_root = Some(config_root.into());
        self
    }

    /// The root `config.toml` resolves under: [`Self::with_config_root`] when
    /// set, else [`Self::home`]. Every reader of `config.toml` — startup and
    /// `crate::server::setup` alike — must resolve through this, not through
    /// [`Self::home`] directly, so the two can never read or write two
    /// different files for the same instance.
    pub fn config_root(&self) -> &std::path::Path {
        self.config_root.as_deref().unwrap_or(&self.home)
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

    /// Records which storage backend was opened, for `/spec` to report.
    pub fn with_storage_kind(mut self, kind: crate::store::StorageKind) -> Self {
        self.storage_kind = kind;
        self
    }

    /// Which storage backend is serving the durable ports.
    ///
    /// Read by `/spec` and — since issue #752 — by every company builder, which
    /// passes it down to the repository-credential gates. Those refuse on `fs`
    /// and `sqlite`, so this is a security-relevant value and not only a
    /// reporting one: a host that forgets to set it is treated as the refusing
    /// case, which is the safe direction.
    pub fn storage_kind(&self) -> crate::store::StorageKind {
        self.storage_kind
    }

    /// This host's stable public identity, minted on first use.
    ///
    /// See [`crate::app::instance`] for why it is random rather than derived
    /// from anything about the deployment.
    pub fn instance_id(&self) -> &str {
        self.instance_id
            .get_or_init(|| crate::app::instance::load_or_create(&self.home))
    }

    /// Installs the memory engine overlay selected at boot
    /// (`OPENCOMPANY_MEMORY`, or `[memory]` in `config.toml`).
    pub fn with_memory_overlay(self, overlay: crate::store::MemoryOverlay) -> Self {
        self.set_memory_overlay(Some(overlay));
        self
    }

    /// The bound memory engine overlay, if one is selected.
    ///
    /// Returns a clone rather than a borrow: the overlay can be replaced while
    /// the process runs (see [`Self::set_memory_overlay`]), and every field of
    /// it is an `Arc`, so the clone costs a handful of refcount bumps and
    /// cannot observe a half-applied swap.
    pub fn memory_overlay(&self) -> Option<crate::store::MemoryOverlay> {
        self.memory_overlay
            .read()
            .expect("memory overlay poisoned")
            .clone()
    }

    /// Replaces the bound memory engine overlay.
    ///
    /// `None` returns memory to the base storage backend's own ports. This
    /// only changes what a company built *after* it will bind — companies
    /// already in the registry hold the previous ports on their cached
    /// runtime, so a caller that wants the swap in force must rebuild them
    /// ([`crate::runtime::rebuild_company`]).
    pub fn set_memory_overlay(&self, overlay: Option<crate::store::MemoryOverlay>) {
        *self
            .memory_overlay
            .write()
            .expect("memory overlay poisoned") = overlay;
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
        // A *configured* library that is missing or not a directory is a host
        // misconfiguration, not a parse failure `load_dir_skills` would flag —
        // it returns `Ok(empty)` for a nonexistent `dir`, which would silently
        // downgrade a server-authoritative install to a client-authored one
        // (the exact invariant `shared_skill_registry`'s doc forbids). Reject it
        // as `Config` (a 500 / failed boot) before the load can flatten it away.
        if !dir.is_dir() {
            return Err(crate::OpenCompanyError::Config(format!(
                "shared skill library at {} is not a directory",
                dir.display()
            )));
        }
        // `load_dir_skills` reports a parse/validation failure via the same
        // `DataParse`/`DataInvalid` variants a per-company workflow file uses,
        // where the HTTP mapping (issue #1017) treats them as the *caller's*
        // bad input (400/422). Here the "file" is the operator-provisioned
        // shared library, not anything a caller submitted, so that mapping
        // would misreport a host misconfiguration as a client error. Recast
        // as `Config` — already the crate's "runtime setup is broken" variant
        // (see `app/config.rs`) — so it renders the 500 documented above.
        let registry: Arc<[SkillDoc]> = load_dir_skills(dir)
            .map_err(|error| {
                crate::OpenCompanyError::Config(format!(
                    "shared skill library at {} failed to load: {error}",
                    dir.display()
                ))
            })?
            .into();
        // A concurrent caller may have set it first; keep whichever won.
        let _ = self.skill_registry.set(registry.clone());
        Ok(self.skill_registry.get().cloned().unwrap_or(registry))
    }

    /// The repo-level shared skill registry, empty when nothing backs it.
    ///
    /// Empty means exactly one thing: no [`skills_root`](Self::skills_root) is
    /// configured, so this host serves no shared library (platform-provisioned
    /// mode). Callers read that as "there is nothing to resolve against" and
    /// fall back accordingly — the install route, for one, then accepts the
    /// client's own metadata.
    ///
    /// A *configured* root that cannot load is therefore never flattened to
    /// empty: doing so would silently downgrade a server-authoritative install
    /// into a client-authored one whenever a `SKILL.md` is malformed or the
    /// directory is unreadable. The load error propagates instead, and callers
    /// surface it (a server error on the HTTP surfaces, a failed boot on the
    /// serve path).
    pub fn shared_skill_registry(&self) -> crate::Result<Arc<[SkillDoc]>> {
        let Some(dir) = self.skills_root() else {
            return Ok(Arc::from([]));
        };
        self.skill_registry(dir)
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

    /// Installs the hub identity exchange backing `…/auth/hub`.
    ///
    /// An injected seam rather than a client built per request, so the route's
    /// refusals — rejected token, unreachable hub, address not on this
    /// company's roster — are testable offline against
    /// [`MockHubIdentityExchange`](crate::server::hub_identity::MockHubIdentityExchange)
    /// in a build that links no HTTP crate at all.
    pub fn with_hub_identity(
        mut self,
        exchange: Arc<dyn crate::server::hub_identity::HubIdentityExchange>,
    ) -> Self {
        self.hub_identity = Some(exchange);
        self
    }

    /// The hub identity exchange, when one is wired.
    ///
    /// `None` means this host has no ecosystem to sign in against, which is the
    /// correct default: a host that cannot ask the hub whose token it is
    /// holding has no way to check one, and accepting it on trust would make an
    /// unverifiable JWT a bearer credential for this company.
    pub fn hub_identity(
        &self,
    ) -> Option<&Arc<dyn crate::server::hub_identity::HubIdentityExchange>> {
        self.hub_identity.as_ref()
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

    /// Marks this host as already set up, seeded at boot from `config.toml`'s
    /// `setup_completed_at`. Mirrors the other `with_*` builders.
    pub fn with_setup_complete(self, complete: bool) -> Self {
        self.setup_complete.store(complete, Ordering::Relaxed);
        self
    }

    /// Whether the first-run setup flow has run against this data root.
    ///
    /// This is the raw `setup_completed_at` stamp, and it is deliberately *not*
    /// what [`AppSpec::setup_complete`] reports: a host serving a company named
    /// on the command line has never been through setup, yet needs no wizard.
    /// The spec answers "does the console need to offer setup", this answers
    /// "did setup run", and only the authorization gate wants the latter — see
    /// `server::setup::authorize`, where an empty registry is what makes the
    /// call anonymous, because there is then no admin to authorize against.
    pub fn setup_complete(&self) -> bool {
        self.setup_complete.load(Ordering::Relaxed)
    }

    /// Records that setup has just completed, so the flip is visible to every
    /// clone of this state without a restart.
    pub fn mark_setup_complete(&self) {
        self.setup_complete.store(true, Ordering::Relaxed);
    }

    /// The host-wide sign-in mode currently in force, or `None` when each
    /// company's own `[users].mode` decides.
    ///
    /// Every company build must read this rather than
    /// [`AppConfig::auth_mode_override`], so a mode changed after boot reaches
    /// the next build or rebuild. See the field docs.
    pub fn auth_mode_override(&self) -> Option<AuthMode> {
        *self.auth_mode_override.read().expect("auth mode poisoned")
    }

    /// Sets the host-wide sign-in mode for companies built from now on.
    ///
    /// Does **not** touch companies already built: the mode is resolved once,
    /// at build, and cached on the runtime. A caller changing it is expected to
    /// rebuild or re-register whatever is already registered — which is what
    /// makes the change take effect without restarting the process.
    pub fn set_auth_mode_override(&self, mode: Option<AuthMode>) {
        *self.auth_mode_override.write().expect("auth mode poisoned") = mode;
    }

    /// The registry of running companies served by this host.
    pub fn registry(&self) -> &CompanyRegistry {
        &self.registry
    }

    /// Who is currently present, per company.
    pub fn presence(&self) -> &crate::server::presence::PresenceRegistry {
        &self.presence
    }

    /// A cloned handle to the same host-global registry [`Self::presence`]
    /// borrows from, for a background task (the periodic sweep) that must
    /// outlive any single request's borrow of `self`.
    pub fn presence_handle(&self) -> std::sync::Arc<crate::server::presence::PresenceRegistry> {
        self.presence.clone()
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
            build_commit: BUILD_COMMIT,
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
            instance_id: self.instance_id().to_string(),
            display_name: self.config.instance_name.clone(),
            capabilities: self.capabilities(),
            storage: self.storage_kind.as_str(),
            // Not the raw stamp: a host already serving a company is set up as
            // far as the console is concerned, whether or not it was this flow
            // that got it there. `--company` predates setup, so every existing
            // deployment has companies and no `setup_completed_at`, and
            // reporting the bare stamp would send all of them through the
            // wizard on their next console load. The wizard exists to fix a
            // host with nothing to open; a host with something to open does not
            // need it.
            //
            // The authorization gate deliberately reads the raw
            // [`Self::setup_complete`] instead — see `server::setup::authorize`.
            // There, "has companies" is what supplies an admin to check
            // against, so the two questions come apart.
            setup_complete: self.setup_complete() || !self.registry().is_empty(),
        }
    }

    /// What this host can do, as flat feature names a client can test for.
    ///
    /// Additive and open-ended by design. A client reading an **older** host's
    /// `/spec` finds no `capabilities` field at all, and must treat that as
    /// "assume nothing beyond REST" — which is why every entry here names a
    /// capability rather than a version number. Growing the list must never
    /// break a client that has not heard of the new entry.
    fn capabilities(&self) -> Vec<&'static str> {
        let mut out = vec!["rest", "graphql", "sse", "approvals"];
        if self.hub_identity.is_some() {
            out.push("hub-identity");
        }
        if self.config.platform_auth.is_some() {
            out.push("platform-auth");
        }
        out
    }
}

/// Serializable OpenCompany runtime specification.
#[derive(Clone, Debug, Serialize)]
pub struct AppSpec {
    /// Crate name.
    pub name: &'static str,
    /// Crate version.
    pub version: &'static str,
    /// The Git commit this host was built from: a short object id, suffixed
    /// `-dirty` when the tree carried uncommitted changes, or `"unknown"`
    /// when the build could not determine one.
    ///
    /// On the unauthenticated handshake, beside [`Self::version`], because the
    /// line this surface polices is **build facts versus deployment facts**. A
    /// build fact is identical for every instance compiled from the same
    /// artifact and says nothing about *this* host. A deployment fact — the
    /// storage path two fields below, a connection string, a data root — is
    /// unique to this host and directly actionable, which is why
    /// [`Self::storage`] reports a kind and not a location. A revision id is
    /// the first kind: it is `version` at usable precision, and `version` has
    /// always been served here.
    ///
    /// That line survives this repository going private, which is the case
    /// worth stating explicitly. A commit id is an opaque hash; without the
    /// repository it maps to nothing, so closing the source *narrows* what
    /// this field discloses rather than widening it. The residual risk is the
    /// public case — an unauthenticated caller can look the revision up and
    /// read off which fixes are missing — and it is accepted deliberately.
    /// Answering "which build is this host actually running?" without a shell
    /// on the box is the entire reason the field exists: an operator served a
    /// three-day-old binary on 2026-08-25 had to compare `strings` output to
    /// work that out.
    pub build_commit: &'static str,
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
    /// Whether hosted cognition can run (hosted brain plus a credential this
    /// instance can obtain, from either tier). No secret bytes are surfaced.
    pub cycles_available: bool,
    /// This host's stable identity, so a client holding several connections can
    /// tell one server from another across URL changes. Random, not derived —
    /// see [`crate::app::instance`].
    pub instance_id: String,
    /// An operator-set name for this instance (`OPENCOMPANY_INSTANCE_NAME`),
    /// shown in a client's connection list. Untrusted, display-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Flat, additive feature names. Absent on a host predating this field,
    /// which a client must read as "REST only".
    pub capabilities: Vec<&'static str>,
    /// The storage backend kind. Deliberately the kind alone: `/spec` is
    /// unauthenticated, so a path or connection string here would be a gift.
    pub storage: &'static str,
    /// Whether the first-run setup flow has been completed on this instance.
    ///
    /// Reported here, on the unauthenticated handshake the console already
    /// fetches before sign-in, because a host that has never been set up has
    /// nobody who *can* sign in — gating this behind auth would make the wizard
    /// unreachable exactly when it is needed. A bare boolean is the whole
    /// disclosure: the configuration itself lives behind `/api/v1/setup`.
    pub setup_complete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_binds_locally() {
        assert_eq!(AppConfig::default().bind, "127.0.0.1:8080");
    }

    /// Automatic Git checkpoints in agent workspaces are opt-in: the host
    /// default is off, preserving the pre-checkpoint behavior exactly.
    #[test]
    fn workspace_git_checkpoints_default_off() {
        assert!(!AppConfig::default().workspace_git_enabled);
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

    /// A host with nothing to open is the only one the wizard is for.
    ///
    /// The registered-company half of this lives in `server::setup::test`,
    /// beside the helper that can build a real runtime:
    /// `spec_reports_setup_complete_once_a_company_is_registered`.
    #[test]
    fn spec_reports_setup_incomplete_for_an_empty_unstamped_host() {
        let spec = AppState::new(AppConfig::default()).spec();

        assert!(
            !spec.setup_complete,
            "no stamp and no companies is exactly the first-run case"
        );
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

    /// Issue #203: unlike `host_base_url`, this has no bind fallback — a URL a
    /// provider cannot reach is worse than none, because it silently swallows
    /// every inbound delivery.
    #[test]
    fn public_webhook_base_url_requires_an_explicit_https_url() {
        // The default (loopback bind, no public_url) offers no webhook — this
        // is exactly the `http://127.0.0.1:8080/hooks/...` URL of issue #203.
        assert_eq!(AppConfig::default().public_webhook_base_url(), None);

        let with = |url: &str| AppConfig {
            public_url: Some(url.into()),
            ..AppConfig::default()
        };
        // Plain http never qualifies: Telegram's setWebhook refuses it, and a
        // public-looking http URL is no more deliverable than a loopback one.
        assert_eq!(with("http://acme.example").public_webhook_base_url(), None);
        // Neither does a scheme-less or empty value.
        assert_eq!(with("acme.example").public_webhook_base_url(), None);
        assert_eq!(with("   ").public_webhook_base_url(), None);
        assert_eq!(with("https://").public_webhook_base_url(), None);

        assert_eq!(
            with("https://acme.example").public_webhook_base_url(),
            Some("https://acme.example")
        );
        // Surrounding whitespace and a trailing slash are normalized away, so
        // callers can join a `/hooks/...` path without doubling the separator.
        assert_eq!(
            with("  https://acme.example/  ").public_webhook_base_url(),
            Some("https://acme.example")
        );
        // The scheme is case-insensitive, as in any URL.
        assert_eq!(
            with("HTTPS://acme.example").public_webhook_base_url(),
            Some("HTTPS://acme.example")
        );
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

    /// With neither tier configured there is nothing to obtain, so no cycles.
    /// Driven through the env seam so an ambient `TINYHUMANS_*` in a developer's
    /// shell cannot decide the result.
    #[test]
    fn default_config_cannot_run_cycles() {
        use crate::app::config::MapEnv;
        let empty = MapEnv::default();
        assert!(!AppConfig::default().cycles_available_in(&empty));
        assert!(!AppConfig::default().credential_available_in(&empty));
        assert_eq!(
            AppConfig::default().credential_source_in(&empty),
            CredentialSource::None
        );
    }

    /// A temp directory holding a stand-in for the platform's projected token
    /// file (mounted at `/var/run/secrets/tinyhumans.ai/token` in production).
    /// The tier is only selected when the path exists, so the test needs a real
    /// one.
    /// Returns the directory handle alongside the path: dropping it removes the
    /// fixture, and holding it is what keeps the file alive for the assertions.
    fn projected_token_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("oc-appcfg-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "projected-token").unwrap();
        (dir, path)
    }

    /// The hosted shape: no static secret at all, just a projected token file.
    /// Cognition must be considered available, and the source reads `attested`.
    #[test]
    fn a_projected_token_file_alone_enables_cycles() {
        use crate::app::config::MapEnv;
        let (_dir, path) = projected_token_file();
        let env = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            path.display().to_string(),
        )]);
        let config = AppConfig::default();
        assert!(config.tinyhumans_credential.is_none(), "nothing stored");
        assert!(config.credential_available_in(&env));
        assert!(config.cycles_available_in(&env));
        assert_eq!(
            config.credential_source_in(&env),
            CredentialSource::Attested
        );

        // A sidecar brain still cannot run hosted cycles, credential or not.
        let sidecar = AppConfig {
            brain_mode: crate::app::config::BrainMode::Sidecar,
            ..AppConfig::default()
        };
        assert!(!sidecar.cycles_available_in(&env));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Docker development is unaffected: the static tier still answers yes, and
    /// a projected file present alongside it outranks it as the source.
    #[test]
    fn static_tier_still_answers_and_is_outranked_by_a_projected_file() {
        use crate::app::config::MapEnv;
        let config = AppConfig {
            tinyhumans_credential: Some(SecretValue("th_static".into())),
            ..AppConfig::default()
        };
        let empty = MapEnv::default();
        assert!(config.cycles_available_in(&empty));
        assert_eq!(
            config.credential_source_in(&empty),
            CredentialSource::Static
        );

        let (_dir, path) = projected_token_file();
        let projected = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            path.display().to_string(),
        )]);
        assert_eq!(
            config.credential_source_in(&projected),
            CredentialSource::Attested
        );

        // A leftover variable pointing at a path the runtime never mounted (the
        // docker case) degrades to the static tier rather than breaking cycles.
        let stale = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            "/nonexistent/oc/token",
        )]);
        assert_eq!(
            config.credential_source_in(&stale),
            CredentialSource::Static
        );
        assert!(config.cycles_available_in(&stale));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn skill_registry_loads_the_repo_library_and_caches() {
        let state = AppState::new(AppConfig::default());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");

        let first = state.skill_registry(&dir).expect("registry loads");
        assert!(first.iter().any(|skill| skill.slug == "web-research"));
        assert!(first.iter().any(|skill| skill.slug == "weekly-report"));
        // The post-call half of the meeting pair (#240): its body must carry the
        // full contract, not just the frontmatter description.
        let debrief = first
            .iter()
            .find(|skill| skill.slug == "call-debrief")
            .expect("call-debrief is in the shared library");
        assert_eq!(debrief.name, "Call Debrief");
        assert_eq!(debrief.category.as_deref(), Some("Ops"));
        assert!(debrief.body.contains("## Steps"), "{}", debrief.body);
        assert!(debrief.body.contains("## Output"), "{}", debrief.body);

        // A second call returns the same cached allocation, ignoring the path.
        let second = state
            .skill_registry(std::path::Path::new("/nonexistent"))
            .expect("cached registry");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn skill_registry_rejects_a_configured_but_missing_library() {
        // A configured `skills_root` that does not exist is a host
        // misconfiguration. `load_dir_skills` returns `Ok(empty)` for a missing
        // dir, so without the `is_dir` guard the registry would silently flatten
        // to empty — downgrading a server-authoritative install to a
        // client-authored one, the invariant `shared_skill_registry` forbids.
        let state = AppState::new(AppConfig::default());
        let err = state
            .skill_registry(std::path::Path::new("/nonexistent"))
            .expect_err("a missing configured library must fail, not load empty");
        assert!(
            matches!(err, crate::OpenCompanyError::Config(_)),
            "expected a Config error for a missing library, got {err:?}"
        );
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
    fn tenant_namespace_rejects_the_id_delimiter() {
        // A namespace containing `--` makes the `<tenant>--` id prefix
        // ambiguous between tenants, so the boundary that reads
        // `OPENCOMPANY_TENANT_ID` rejects it.
        assert!(validate_tenant_namespace("acme").is_ok());
        assert!(validate_tenant_namespace("acme-corp").is_ok());
        assert_eq!(
            validate_tenant_namespace("acme--other").unwrap_err(),
            "tenant namespace `acme--other` contains `--`, which is the company-id \
             delimiter; a namespace may not contain it"
        );
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

    /// `config_root` defaults to `home` — the aligned shape every deployment
    /// but an explicit, diverging `--home` takes (see the field doc and
    /// `store::home_divergence_warning`).
    #[test]
    fn config_root_defaults_to_home() {
        let state = AppState::new(AppConfig::default()).with_home("/data/companies");
        assert_eq!(state.config_root(), std::path::Path::new("/data/companies"));
    }

    /// Once set explicitly, `config_root` diverges from `home` — this is the
    /// fix for #908's review: `server::setup` must resolve `config.toml`
    /// through this, not through `home`, or it reads and writes a different
    /// file than startup does on a deployment where the two differ.
    #[test]
    fn config_root_can_diverge_from_home() {
        let state = AppState::new(AppConfig::default())
            .with_home("/data/companies")
            .with_config_root("/data");
        assert_eq!(state.home(), std::path::Path::new("/data/companies"));
        assert_eq!(state.config_root(), std::path::Path::new("/data"));
    }
}
