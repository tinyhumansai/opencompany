//! Config-driven storage backend selection.
//!
//! The five storage ports are the entire persistence contract; this module is
//! the one place that maps a backend *name* onto concrete port
//! implementations. `serve` (and platform provisioning) resolve a
//! [`StorageKind`] from `OPENCOMPANY_STORAGE`, open the backend once, and
//! inject the same [`StorageHandles`] into every company's `RuntimeBuilder` —
//! the kernel itself never names an engine.
//!
//! Backends behind disabled cargo features fail loudly at open time rather
//! than silently falling back to the filesystem.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::app::config::{EnvSource, ProcessEnv, data_dir_from_source};
use crate::error::OpenCompanyError;
use crate::ports::artifacts::ArtifactStore;
use crate::ports::context::ContextStore;
use crate::ports::events::EventLog;
use crate::ports::facts::FactStore;
use crate::ports::inbox::InboxStore;
use crate::ports::journal::JournalStore;
use crate::ports::ledgers::LedgerStore;
use crate::ports::login_codes::LoginCodeStore;
use crate::ports::memory::MemoryStore;
use crate::ports::notifications::NotificationStore;
use crate::ports::read_state::ReadStateStore;
use crate::ports::run_output::WorkflowRunOutputStore;
use crate::ports::runs::RunStore;
use crate::ports::schedule_fires::ScheduleFireStore;
use crate::ports::secrets::SecretStore;
use crate::ports::sessions::SessionStore;
use crate::ports::skills_state::SkillStateStore;
use crate::ports::store::CompanyStore;
use crate::ports::tasks::TaskStore;
use crate::ports::types::CompanyId;
use crate::ports::usage::UsageMeter;
use crate::ports::users::UserStore;
use crate::ports::workflow_revisions::WorkflowRevisionStore;
use crate::ports::workspace::WorkspaceStore;

/// Safe access to the provider-only context partitions.
///
/// This is deliberately a facade, not the underlying `MemoryProvider`: every
/// method still derives the company namespace from a [`CompanyId`], so wiring
/// it onto a runtime cannot reopen the raw-namespace escape hatch.
#[async_trait]
pub trait MemoryScopes: Send + Sync {
    /// One agent's private context partition.
    fn agent_context(&self, agent_id: &str) -> Arc<dyn ContextStore>;
    /// One desk's shared context partition.
    fn desk_context(&self, desk_id: &str) -> Arc<dyn ContextStore>;
    /// Traces retained when normal trace eviction archives them.
    async fn archived_traces(
        &self,
        company: &CompanyId,
    ) -> Result<Vec<crate::ports::CompressedTrace>>;
    /// Restores traces directly into the archive tier during bundle import.
    ///
    /// Implementations that expose only the inspection surface reject this
    /// operation rather than silently moving retained traces back into the
    /// live window.
    async fn restore_archived_traces(
        &self,
        _company: &CompanyId,
        traces: &[crate::ports::CompressedTrace],
    ) -> Result<()> {
        if traces.is_empty() {
            return Ok(());
        }
        Err(OpenCompanyError::Store(
            "the selected memory engine cannot restore archived traces".into(),
        ))
    }
}

/// Which storage backend hosts the durable ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StorageKind {
    /// Per-company filesystem bundles (the default; no external service).
    #[default]
    Fs,
    /// One SQLite database file under the data dir (`sqlite` feature).
    Sqlite,
    /// A MongoDB database on a shared cluster (`mongodb` feature) — the
    /// multi-tenant platform backend.
    Mongodb,
}

impl StorageKind {
    /// The backend's name, for `/spec`. Stable wire strings — a client keys
    /// behaviour off these, so they are not `Debug` output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fs => "fs",
            Self::Sqlite => "sqlite",
            Self::Mongodb => "mongodb",
        }
    }

    /// Whether this backend keeps [`SecretStore`] material as **plaintext on
    /// the container's own filesystem** (issue #752).
    ///
    /// `fs` writes one plaintext file per secret under
    /// `<data-dir>/companies/<slug>/secrets/` — [`FsSecretStore`] says so in its
    /// own doc comment, and `sqlite` puts the same bytes in a database file on
    /// the same disk. `mongodb` is the only backend that keeps them out of the
    /// container, in the tenant database.
    ///
    /// This matters because of who else is on that filesystem. An agent holding
    /// `shell` runs as the same uid as the server process, in the same
    /// container, so "plaintext on disk" means "readable by a prompt-injected
    /// agent" — there is no boundary in between, and
    /// `docs/spec/security/agent-isolation.md` is explicit that none is planned
    /// inside a tenant. A repository credential parked there is a credential the
    /// agent can read and use directly, without going through any tool the host
    /// gates.
    ///
    /// New backends default to the safe answer by being added to the `true` arm
    /// unless they demonstrably keep secrets off the local disk.
    ///
    /// [`FsSecretStore`]: crate::store::FsSecretStore
    /// [`SecretStore`]: crate::ports::SecretStore
    pub fn secrets_are_plaintext_on_disk(self) -> bool {
        match self {
            Self::Fs | Self::Sqlite => true,
            Self::Mongodb => false,
        }
    }
}

/// The refusal every repository-credential gate raises on a backend that keeps
/// secrets as plaintext on the container's disk (issue #752).
///
/// One function rather than a message per call site: the bind route, the boot
/// check and the agent-build gate all refuse the *same* deployment condition,
/// and an operator who reads it in the console then reads it again in the boot
/// log should not have to work out whether they are two problems.
///
/// Written to be self-service — it names the condition, the risk in one clause,
/// and both ways out — because the operator hitting it is mid-task with a token
/// in their clipboard, and "storage backend not supported" would send them to
/// the issue tracker instead of to a fix.
pub fn plaintext_secret_refusal(kind: StorageKind) -> String {
    format!(
        "this host keeps secrets on its own filesystem (OPENCOMPANY_STORAGE={}), so a \
         repository credential would sit there in plaintext — readable by the same uid the \
         agent shell runs as, which is not a boundary this deployment has. Repository \
         credentials are refused here. Either point this host at MongoDB \
         (OPENCOMPANY_STORAGE=mongodb plus OPENCOMPANY_MONGODB_URI, which keeps secrets in \
         the tenant database), or drop the `repo` grant from the company's [tools] allow \
         list and from every agent that names it. See docs/spec/runtime/storage.md.",
        kind.as_str()
    )
}

impl std::str::FromStr for StorageKind {
    type Err = OpenCompanyError;
    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "fs" | "" => Ok(Self::Fs),
            "sqlite" => Ok(Self::Sqlite),
            "mongodb" | "mongo" => Ok(Self::Mongodb),
            other => Err(OpenCompanyError::Config(format!(
                "OPENCOMPANY_STORAGE must be 'fs', 'sqlite', or 'mongodb', got '{other}'"
            ))),
        }
    }
}

/// Which engine backs the memory + context ports, independent of the base
/// [`StorageKind`].
///
/// Memory is a separable concern: `OPENCOMPANY_STORAGE` picks the durable base
/// (companies, events, secrets, …) while `OPENCOMPANY_MEMORY` can swap the
/// knowledge ports onto a hosted provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MemoryBackend {
    /// Memory + context come from the base [`StorageKind`] (the default; fs
    /// substring recall, or the sqlite/mongodb store).
    #[default]
    Store,
    /// A hosted memory service behind a URL and a credential, bound through the
    /// `MemoryProvider` contract (`tinymemory` feature).
    ///
    /// Missing credentials refuse at boot: a company that believes it is writing
    /// to hosted memory and is not is worse off than one that fails to start.
    Remote,
    /// Writes accepted and discarded, reads empty (`tinymemory` feature).
    Null,
}

impl MemoryBackend {
    /// The stable wire string for status output.
    ///
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Remote => "remote",
            Self::Null => "null",
        }
    }
}

impl std::str::FromStr for MemoryBackend {
    type Err = OpenCompanyError;
    /// Parses `OPENCOMPANY_MEMORY`.
    ///
    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "store" | "" => Ok(Self::Store),
            "remote" => Ok(Self::Remote),
            "null" => Ok(Self::Null),
            other => Err(OpenCompanyError::Config(format!(
                "OPENCOMPANY_MEMORY must be 'store', 'remote', or 'null', got '{other}'"
            ))),
        }
    }
}

/// The memory + context ports of a selected memory engine, ready to overlay
/// onto a company's builder after the base [`StorageHandles`] via
/// [`RuntimeBuilder::with_memory_overlay`](crate::runtime::RuntimeBuilder::with_memory_overlay).
#[derive(Clone)]
pub struct MemoryOverlay {
    pub memory: Arc<dyn MemoryStore>,
    pub context: Arc<dyn ContextStore>,
    /// The operator's facts, when the selected engine serves them too.
    ///
    /// A provider-backed engine covers all three ports, so this is populated
    /// whenever an overlay is active.
    pub facts: Option<Arc<dyn FactStore>>,
    /// The inbound-content partition: writes land taint-stamped
    /// `ExternalSync`, so third-party content can never launder into
    /// internal-trust memory. Carried on the overlay so the runtime can route
    /// channel/web ingestion through it the day such a path exists — no
    /// production writer yet, and that absence is tracked in #1113.
    pub inbound_context: Option<Arc<dyn ContextStore>>,
    /// The scratch firewall: working-out that durable recall can never reach.
    pub scratch: Option<Arc<dyn ContextStore>>,
    /// Provider-only scoped partitions and archive reads, with no raw
    /// provider exposed to runtime consumers.
    pub scopes: Option<Arc<dyn MemoryScopes>>,
    /// What is bound, for status output.
    pub descriptor: MemoryDescriptor,
    /// The bound provider, kept solely so [`Self::refresh_health`] can probe
    /// it at boot and for operator status reads. `None` on a bare test overlay,
    /// which has no provider to ask.
    ///
    /// Private on purpose: `MemoryCore` is a supertrait of `MemoryProvider`,
    /// so a public handle here would let anything holding an `AppState` call
    /// `store("<any namespace>", …)` directly — re-opening exactly the
    /// raw-namespace door `store::memory`'s module docs promise is closed by
    /// construction. The ports above are the only data path.
    #[cfg(feature = "tinymemory")]
    probe: Option<Arc<dyn tinymemory_api::provider::MemoryProvider>>,
}

impl MemoryOverlay {
    /// Bare overlay for wiring tests: the given ports, no facts, no scratch,
    /// no probe. Lives here because `probe` is private by design (see the
    /// field doc) — tests outside this module cannot construct the struct.
    #[cfg(test)]
    pub(crate) fn test_with_ports(
        memory: Arc<dyn MemoryStore>,
        context: Arc<dyn ContextStore>,
        inbound_context: Option<Arc<dyn ContextStore>>,
    ) -> Self {
        Self {
            memory,
            context,
            facts: None,
            inbound_context,
            scratch: None,
            scopes: None,
            descriptor: MemoryDescriptor {
                backend: MemoryBackend::Store,
                driver_id: "test".into(),
                capabilities: Vec::new(),
                healthy: None,
            },
            #[cfg(feature = "tinymemory")]
            probe: None,
        }
    }

    /// Probes the bound engine and records the answer on the descriptor, so the
    /// authenticated engine endpoint can tell an operator "bound but unreachable"
    /// before the first cycle finds out — until this ran, a hosted engine with a dead
    /// endpoint or a revoked key bound cleanly and failed days later, on a
    /// path nobody was watching.
    ///
    /// Bounded by `timeout`, and advisory by design: a probe
    /// failure logs loudly and records `healthy: Some(false)`, it never
    /// refuses the boot. Configuration errors already refuse at open; a
    /// transient vendor outage must not crash-loop a tenant that could serve
    /// everything else. `healthy: None` means "not probed" — the engine
    /// overlay path, or a build without the provider seam.
    #[cfg(feature = "tinymemory")]
    pub async fn refresh_health(&mut self, timeout: std::time::Duration) {
        let Some(probe) = &self.probe else {
            return;
        };
        let answer = tokio::time::timeout(timeout, probe.health()).await.ok();
        let healthy = probe_answer_is_healthy(&answer);
        if !healthy {
            tracing::warn!(
                driver_id = %self.descriptor.driver_id,
                status = ?answer,
                timeout_secs = timeout.as_secs(),
                "memory engine bound but its health probe did not answer Ready or Degraded; \
                 cycles that need memory will fail until the endpoint or credential is fixed"
            );
        }
        self.descriptor.healthy = Some(healthy);
    }

    /// Without the provider seam there is nothing to probe; `healthy` stays
    /// `None` ("not probed"), which is the truth.
    #[cfg(not(feature = "tinymemory"))]
    pub async fn refresh_health(&mut self, _timeout: std::time::Duration) {}
}

/// Maps a probe outcome (`None` = timed out) onto the `healthy` bit.
///
/// `Ready` AND `Degraded` are healthy: a degraded engine is still serving —
/// reduced, not absent — and reporting it as down would tell an operator to
/// fix an endpoint that is answering. Only `Down` and a timeout mean the
/// first cycle that needs memory will fail.
#[cfg(feature = "tinymemory")]
fn probe_answer_is_healthy(answer: &Option<tinymemory_api::health::MemoryHealth>) -> bool {
    use tinymemory_api::health::MemoryHealth;
    matches!(
        answer,
        Some(MemoryHealth::Ready) | Some(MemoryHealth::Degraded { .. })
    )
}

impl std::fmt::Debug for MemoryOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryOverlay")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

/// What memory engine is live, in terms safe to show an operator.
///
/// Deliberately carries no endpoint and no credential. `driver_id` is safe to
/// surface — the contract's own docs treat it as an identity, not a secret —
/// while the URL and the key are not, and this type is what reaches `/spec`,
/// which is unauthenticated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryDescriptor {
    /// The selected mode (`store`, `embedded`, `remote`, `null`).
    pub backend: MemoryBackend,
    /// The bound engine's own name, when one is bound.
    pub driver_id: String,
    /// The capability families the bound driver negotiated, so an operator can
    /// see what the engine does *not* support before a cycle finds out.
    pub capabilities: Vec<String>,
    /// Whether the boot-time reachability probe found the engine usable:
    /// `Ready`, or `Degraded` — reachable and serving, possibly reduced.
    /// Only `Down` maps to `Some(false)`.
    ///
    /// `None` means the engine was never probed — a boot path that skipped
    /// [`MemoryOverlay::refresh_health`]. `Some(false)` is a bound engine
    /// whose probe failed: still bound and loudly warned.
    pub healthy: Option<bool>,
}

/// Durable company → tenant ownership, for shared-database platform mode.
/// Backends that can persist ownership (MongoDB today) expose it here so the
/// in-memory `AppState` map can be hydrated at boot and updated on provision.
#[async_trait]
pub trait OwnershipStore: Send + Sync {
    async fn set_owner(&self, id: &CompanyId, tenant: &str) -> Result<()>;
    async fn remove_owner(&self, id: &CompanyId) -> Result<()>;
    async fn owners(&self) -> Result<Vec<(CompanyId, String)>>;
}

/// One opened backend's implementations of every durable port, ready to be
/// injected into `RuntimeBuilder::with_stores`.
#[derive(Clone)]
pub struct StorageHandles {
    pub company: Arc<dyn CompanyStore>,
    pub events: Arc<dyn EventLog>,
    pub memory: Arc<dyn MemoryStore>,
    pub context: Arc<dyn ContextStore>,
    pub secrets: Arc<dyn SecretStore>,
    pub inbox: Arc<dyn InboxStore>,
    pub tasks: Arc<dyn TaskStore>,
    /// The company's declared ledgers and their append-only event logs.
    pub ledgers: Arc<dyn LedgerStore>,
    pub workspace: Arc<dyn WorkspaceStore>,
    pub facts: Arc<dyn FactStore>,
    pub artifacts: Arc<dyn ArtifactStore>,
    /// First-class task-run records and their step traces (#242).
    pub runs: Arc<dyn RunStore>,
    /// Per-workflow edit history for rollback (#274).
    pub workflow_revisions: Arc<dyn WorkflowRevisionStore>,
    /// Durable cross-replica scheduler fire claims (#241).
    pub schedule_fires: Arc<dyn ScheduleFireStore>,
    /// Durable, console-facing per-node run output snapshots (#596).
    pub run_outputs: Arc<dyn WorkflowRunOutputStore>,
    /// The unredacted companion of a turn's steps — reasoning text, raw tool
    /// arguments and raw tool output. Holds secrets by design; see
    /// [`crate::ports::deep_trace`].
    pub deep_trace: Arc<dyn crate::ports::deep_trace::DeepTraceStore>,
    pub usage: Arc<dyn UsageMeter>,
    pub skills: Arc<dyn SkillStateStore>,
    /// Per-person, per-channel read markers (#755).
    pub read_state: Arc<dyn ReadStateStore>,
    /// Durable notifications with per-person read state (#749).
    pub notifications: Arc<dyn NotificationStore>,
    pub users: Arc<dyn UserStore>,
    pub sessions: Arc<dyn SessionStore>,
    pub login_codes: Arc<dyn LoginCodeStore>,
    /// The runtime journal's durable sink (#726): at-most-once effect keys, the
    /// parked-approval queue, grants, and cycle brackets.
    ///
    /// Not `Option`, unlike [`ownership`](Self::ownership): a backend that
    /// cannot hold the journal cannot host a company at all, and a `None` here
    /// would be an invitation to fall back to the filesystem — which is exactly
    /// the bug (#726). On a mongodb tenant `/data` is ephemeral scratch, so a
    /// silent fs journal there loses every committed key and every parked
    /// approval the next time the container is replaced.
    pub journal: Arc<dyn JournalStore>,
    /// Present when the backend persists company → tenant ownership.
    pub ownership: Option<Arc<dyn OwnershipStore>>,
}

impl std::fmt::Debug for StorageHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageHandles")
            .field("ownership", &self.ownership.is_some())
            .finish_non_exhaustive()
    }
}

/// Which deployment of a hosted engine the credential belongs to.
///
/// Not cosmetic, and not inferable from the URL. Mem0 and Cognee each expose
/// two products that speak *different protocols* under the same driver id:
/// Mem0's platform authenticates with `Authorization: Token` and serves v3/v1
/// paths while its open-source server uses `X-API-Key` and un-prefixed ones;
/// Cognee Cloud uses `X-Api-Key` where an authenticated self-hosted instance
/// takes a bearer token. Pointing the wrong one at a live service fails at the
/// first request — a 404 on paths that do not exist there, or a 401 whose body
/// says `Invalid header` — and neither error names the real cause.
///
/// Supermemory is the exception that made this easy to miss: it serves the
/// same API with the same bearer credential either way, so the single
/// constructor OpenCompany used worked against it and against nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemoteDeployment {
    /// The vendor's managed platform. The default, because that is what
    /// `remote` mode is for; a self-hosted engine is the deliberate case.
    #[default]
    Managed,
    /// An instance the operator runs themselves.
    SelfHosted,
}

impl RemoteDeployment {
    /// Parses the wire value, or `None` when it names neither deployment.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "managed" | "cloud" | "hosted" | "platform" => Some(Self::Managed),
            "self-hosted" | "selfhosted" | "self" => Some(Self::SelfHosted),
            _ => None,
        }
    }
}

/// Connection settings for [`open_storage`]. `fs` needs nothing beyond the
/// runtime's home directory (handled by the builder's defaults), so it yields
/// `None` handles.
#[derive(Clone, Default)]
pub struct StorageSettings {
    pub kind: StorageKind,
    /// MongoDB connection string (`OPENCOMPANY_MONGODB_URI`).
    pub mongodb_uri: Option<String>,
    /// MongoDB database name (`OPENCOMPANY_MONGODB_DB`); the hosting layer
    /// sets a per-tenant name (e.g. `oc-<tenant>`) on a shared cluster.
    pub mongodb_db: Option<String>,
    /// Tenant identity for shared-single-DB deployments
    /// (`OPENCOMPANY_TENANT_ID`). When set, company ids are namespaced with
    /// this value so that many tenants sharing one logical database never
    /// collide on the `companies` unique index. Unset means the id-namespacing
    /// no-op: single-tenant / db-per-tenant behavior is unchanged.
    pub tenant_id: Option<String>,
    /// Which engine backs the memory + context ports (`OPENCOMPANY_MEMORY`),
    /// overlaid on top of `kind`. Defaults to [`MemoryBackend::Store`] (the base
    /// backend's own memory), so unset changes nothing.
    pub memory_backend: MemoryBackend,
    /// The instance workspace root (`OPENCOMPANY_DATA_DIR`), when known. Threaded
    /// through so a persistent memory engine can root each company's storage
    /// under `<data_dir>/memory/`. `None` (the [`Default`]) selects the offline
    /// in-memory engine — the shape tests and no-data-dir callers get.
    pub data_dir: Option<PathBuf>,
    /// Operator's explicit durability assertion for the data dir
    /// (`OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL`). Retained from the embedded-engine
    /// era, when the in-pod engine was refused by default under
    /// `OPENCOMPANY_STORAGE=mongodb` because the hosted model treats `/data` as
    /// ephemeral scratch. The in-pod engine is gone, so the flag is currently a
    /// no-op kept for deployment compatibility; `false` (the [`Default`]) stays
    /// the safe default.
    pub allow_ephemeral_memory: bool,
    /// Which engine to bind for `OPENCOMPANY_MEMORY=remote`
    /// (`OPENCOMPANY_MEMORY_DRIVER`): `supermemory`, `mem0`, `cognee`.
    ///
    /// Instance-level, never per-company: one engine per instance, like
    /// `OPENCOMPANY_STORAGE`, while manifests are per-company — a
    /// company-scoped knob for an instance-wide choice would be incoherent.
    /// This deliberately differs from `[inference].provider`, which *is*
    /// per-company and rightly lives in the manifest.
    ///
    /// Two channels, in the usual order: the environment, and — when the
    /// deployment names no engine — the `[memory]` section of `config.toml`,
    /// which is what lets an operator choose one from the console
    /// ([`MemorySelection`], [`StorageSettings::with_memory_config`]).
    pub memory_driver: Option<String>,
    pub memory_url: Option<String>,
    /// The hosted engine's credential (`OPENCOMPANY_MEMORY_API_KEY`).
    ///
    /// A raw credential, so it is kept out of [`Debug`] — see the impl below.
    ///
    /// Env is the only supported channel. A
    /// [`SecretStore`](crate::ports::SecretStore) key — the convention every
    /// other integration follows, and the one that would keep this out of the
    /// process environment — is deliberately *not* accepted here: the store is
    /// per-company and opened from the storage layer this setting is used to
    /// build, so reading the memory credential out of it would be circular.
    /// The hosted manager injects environment rather than manifests, which is
    /// what makes env sufficient.
    pub memory_api_key: Option<String>,
    /// Which deployment of the named remote engine the credential belongs to
    /// (`OPENCOMPANY_MEMORY_DEPLOYMENT`: `managed` or `self-hosted`).
    ///
    /// Defaults to managed. Mem0 and Cognee serve different protocols to their
    /// platform and their self-hosted server under one driver id, so this is
    /// not inferable from the URL, and getting it wrong fails at the first
    /// request with an error that names neither the cause nor this setting.
    pub memory_deployment: RemoteDeployment,
}

impl std::fmt::Debug for StorageSettings {
    /// Renders everything except the two credentials.
    ///
    /// `StorageSettings` is printed at boot (`src/bin/opencompany.rs`), so a
    /// derived `Debug` would put a memory credential and a MongoDB connection
    /// string into the startup log of every tenant container.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageSettings")
            .field("kind", &self.kind)
            .field("mongodb_uri", &self.mongodb_uri.as_ref().map(|_| "<set>"))
            .field("mongodb_db", &self.mongodb_db)
            .field("tenant_id", &self.tenant_id)
            .field("memory_backend", &self.memory_backend)
            .field("data_dir", &self.data_dir)
            .field("allow_ephemeral_memory", &self.allow_ephemeral_memory)
            .field("memory_driver", &self.memory_driver)
            .field("memory_url", &self.memory_url.as_ref().map(|_| "<set>"))
            .field(
                "memory_api_key",
                &self.memory_api_key.as_ref().map(|_| "<set>"),
            )
            .finish()
    }
}

/// Parses env var `key` into `T`. Absent → `Ok(None)` (the caller applies its
/// default); a set-but-non-UTF-8 value is a hard [`OpenCompanyError::Config`]
/// rather than a silent fallback to the default.
fn parse_env<T>(env: &dyn EnvSource, key: &str) -> Result<Option<T>>
where
    T: std::str::FromStr<Err = OpenCompanyError>,
{
    match env.get_os(key) {
        Some(raw) => match raw.into_string() {
            Ok(raw) => Ok(Some(raw.parse()?)),
            Err(_) => Err(OpenCompanyError::Config(format!(
                "{key} is set but is not valid UTF-8"
            ))),
        },
        None => Ok(None),
    }
}

/// Reads a boolean opt-in env flag. Truthy values (case-insensitive, trimmed):
/// `1`, `true`, `yes`, `on`. Anything else — including unset — is `false`.
fn env_flag(env: &dyn EnvSource, key: &str) -> bool {
    env.get(key)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

impl StorageSettings {
    /// Reads the CLI-surface storage env vars (`OPENCOMPANY_STORAGE`,
    /// `OPENCOMPANY_MONGODB_URI`, `OPENCOMPANY_MONGODB_DB`,
    /// `OPENCOMPANY_TENANT_ID`, `OPENCOMPANY_MEMORY`, `OPENCOMPANY_DATA_DIR`,
    /// `OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL`).
    pub fn from_env() -> Result<Self> {
        Self::from_env_source(&ProcessEnv)
    }

    /// Resolves storage settings from an injected environment source.
    pub fn from_env_source(env: &dyn EnvSource) -> Result<Self> {
        let kind: StorageKind = parse_env(env, "OPENCOMPANY_STORAGE")?.unwrap_or_default();
        let memory_backend: MemoryBackend =
            parse_env(env, "OPENCOMPANY_MEMORY")?.unwrap_or_default();
        let non_empty = |key: &str| env.get(key);
        Ok(Self {
            kind,
            mongodb_uri: non_empty("OPENCOMPANY_MONGODB_URI"),
            mongodb_db: non_empty("OPENCOMPANY_MONGODB_DB"),
            tenant_id: non_empty("OPENCOMPANY_TENANT_ID"),
            memory_backend,
            data_dir: Some(data_dir_from_source(env)),
            allow_ephemeral_memory: env_flag(env, "OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL"),
            memory_driver: non_empty("OPENCOMPANY_MEMORY_DRIVER"),
            memory_url: non_empty("OPENCOMPANY_MEMORY_URL"),
            memory_api_key: non_empty("OPENCOMPANY_MEMORY_API_KEY"),
            memory_deployment: non_empty("OPENCOMPANY_MEMORY_DEPLOYMENT")
                .as_deref()
                .and_then(RemoteDeployment::parse)
                .unwrap_or_default(),
        })
    }

    /// Whether the *deployment* named the memory engine
    /// (`OPENCOMPANY_MEMORY`), in which case the `[memory]` section of
    /// `config.toml` is inert and the console renders the engine read-only.
    ///
    /// Presence, not value: a control plane that injects `OPENCOMPANY_MEMORY`
    /// owns the choice whichever engine it names, including `store`.
    pub fn memory_is_env_owned() -> bool {
        Self::memory_is_env_owned_by(&ProcessEnv)
    }

    /// Whether `env` explicitly owns the memory-engine choice.
    pub fn memory_is_env_owned_by(env: &dyn EnvSource) -> bool {
        env.get("OPENCOMPANY_MEMORY")
            .is_some_and(|value| !value.trim().is_empty())
    }

    /// Layers a `config.toml` `[memory]` section under the process
    /// environment.
    ///
    /// A no-op when [`Self::memory_is_env_owned`] — see [`MemorySection`] for
    /// why the env layer wins rather than the more recent write.
    ///
    /// [`MemorySection`]: crate::app::config::MemorySection
    pub fn with_memory_config(self, section: &crate::app::config::MemorySection) -> Result<Self> {
        self.with_memory_config_from(&ProcessEnv, section)
    }

    /// Like [`Self::with_memory_config`], but resolves the ownership check from
    /// the injected `env` source rather than the ambient process environment.
    ///
    /// A caller that built the settings with [`Self::from_env_source`] must
    /// layer through here: the plain variant reads `ProcessEnv`, so a `MapEnv`
    /// carrying `OPENCOMPANY_MEMORY` would be overridden by the file, and an
    /// absent injected value would wrongly appear env-owned when the ambient
    /// process sets it.
    pub fn with_memory_config_from(
        mut self,
        env: &dyn EnvSource,
        section: &crate::app::config::MemorySection,
    ) -> Result<Self> {
        if Self::memory_is_env_owned_by(env) {
            return Ok(self);
        }
        let selection = MemorySelection::from_section(section)?;
        self.memory_backend = selection.backend;
        self.memory_driver = selection.driver;
        self.memory_url = selection.url;
        self.memory_api_key = selection.api_key;
        Ok(self)
    }

    /// Replaces the memory selection with `selection`, leaving the base
    /// backend, data dir and durability assertion untouched.
    ///
    /// This is what a live engine swap rebinds through
    /// ([`crate::server::ops::memory_engine`]): the rest of a running
    /// instance's storage settings must survive a memory change unchanged.
    pub fn with_memory_selection(mut self, selection: MemorySelection) -> Self {
        self.memory_backend = selection.backend;
        self.memory_driver = selection.driver;
        self.memory_url = selection.url;
        self.memory_api_key = selection.api_key;
        self
    }
}

/// One instance's memory-engine choice: everything `OPENCOMPANY_MEMORY*`
/// names, parsed and validated, independent of where it came from (the
/// environment, `config.toml`, or a console write that has not been persisted
/// yet).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct MemorySelection {
    pub backend: MemoryBackend,
    pub driver: Option<String>,
    pub url: Option<String>,
    pub api_key: Option<String>,
}

impl MemorySelection {
    /// Parses a `config.toml` `[memory]` section.
    ///
    /// Blank strings are treated as absent — a TOML key written and then
    /// emptied means "not set", the same rule `non_empty` applies to the
    /// environment, and routing on bare presence would send `""` down a path
    /// that binds nothing.
    pub fn from_section(section: &crate::app::config::MemorySection) -> Result<Self> {
        let trimmed = |value: &Option<String>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let backend = match trimmed(&section.backend) {
            Some(raw) => raw.parse()?,
            None => MemoryBackend::default(),
        };
        Ok(Self {
            backend,
            driver: trimmed(&section.driver),
            url: trimmed(&section.url),
            api_key: trimmed(&section.api_key),
        })
    }
}

/// Renders the credential as `<set>`, never its bytes — the same hand-written
/// `Debug` [`StorageSettings`] carries, and for the same reason: this type is
/// reachable from boot logging and from a route's error path, where a bare
/// `{:?}` is one keystroke away.
impl std::fmt::Debug for MemorySelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySelection")
            .field("backend", &self.backend.as_str())
            .field("driver", &self.driver)
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

/// Opens the selected backend once. `Ok(None)` means "use the builder's fs
/// defaults"; a selected-but-unavailable backend is an error, never a silent
/// fs fallback.
pub async fn open_storage(
    settings: &StorageSettings,
    data_dir: &Path,
) -> Result<Option<StorageHandles>> {
    match settings.kind {
        StorageKind::Fs => Ok(None),
        StorageKind::Sqlite => open_sqlite(data_dir),
        StorageKind::Mongodb => open_mongodb(settings).await,
    }
}

/// The bundle-command environment refusals (`export` / `import`), extracted
/// from the bin's `live_ports` so they execute under this module's tests —
/// the first cut left them in the binary, where no CI lane runs tests and a
/// mutation (`if false &&`) went green (the #1279 review's finding).
///
/// One deployment per bundle: with a non-default environment an explicit
/// `--home` is refused rather than mixed in; `null` is refused in both
/// directions; shared-single-DB tenant mode is refused (bundle ops write no
/// owner rows). Under the fs+store default every check passes and `--home`
/// means exactly what it always has.
pub fn refuse_bundle_env(settings: &StorageSettings, home_was_flagged: bool) -> crate::Result<()> {
    let live = settings.kind != StorageKind::Fs || settings.memory_backend != MemoryBackend::Store;
    if settings.memory_backend == MemoryBackend::Null {
        return Err(crate::error::OpenCompanyError::Config(
            "OPENCOMPANY_MEMORY=null retains nothing: an export would capture no memory and an \
             import would discard every record while reporting success. Unset OPENCOMPANY_MEMORY \
             for bundle operations."
                .into(),
        ));
    }
    if live && home_was_flagged {
        return Err(crate::error::OpenCompanyError::Config(format!(
            "--home names an fs data set, but this environment selects storage `{}` and memory \
             `{}` — the bundle would mix two deployments. Unset OPENCOMPANY_STORAGE and \
             OPENCOMPANY_MEMORY* to operate on the fs home, or drop --home to operate on the \
             live deployment.",
            settings.kind.as_str(),
            settings.memory_backend.as_str()
        )));
    }
    if live
        && settings
            .tenant_id
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
    {
        return Err(crate::error::OpenCompanyError::Config(
            "shared-single-DB tenant mode (OPENCOMPANY_TENANT_ID) namespaces company ids and \
             owner rows at the app layer; bundle operations write neither. Run them without \
             tenant mode, from the manager path."
                .into(),
        ));
    }
    Ok(())
}

/// Opens the memory + context overlay selected by `OPENCOMPANY_MEMORY`.
///
/// `Ok(None)` means [`MemoryBackend::Store`] — the base backend keeps its own
/// memory, no overlay. A selected-but-unavailable engine (feature disabled) is
/// an error, never a silent fallback, mirroring [`open_storage`].
pub fn open_memory_overlay(settings: &StorageSettings) -> Result<Option<MemoryOverlay>> {
    match settings.memory_backend {
        MemoryBackend::Store => Ok(None),
        MemoryBackend::Remote | MemoryBackend::Null => open_provider(settings),
    }
}

/// Opens a [`MemoryProvider`](tinymemory_api::provider::MemoryProvider)-backed
/// overlay: the `remote` and `null` modes. The provider contract covers all
/// three memory ports, so a company never splits its memory across engines.
#[cfg(feature = "tinymemory")]
fn open_provider(settings: &StorageSettings) -> Result<Option<MemoryOverlay>> {
    use crate::store::memory::{BoundMemory, MemoryDriverConfig, MemoryMode, open_driver};

    // The unproven-remote acceptance flag retired here: its premise — "no
    // driver conformance suite (tinymemory#18 §E1)" — stopped being true when
    // the vendored tinymemory gained one (a shared suite run against all four
    // drivers, plus failure-path tests on the remote adapters). The bind-time
    // capability audit below is the live safeguard.
    let mode = match settings.memory_backend {
        MemoryBackend::Remote => MemoryMode::Remote,
        MemoryBackend::Null => MemoryMode::Null,
        // Unreachable: the caller never routes `store` here.
        MemoryBackend::Store => return Ok(None),
    };
    let config = MemoryDriverConfig {
        mode,
        driver_id: settings.memory_driver.clone(),
        url: settings.memory_url.clone(),
        api_key: settings.memory_api_key.clone(),
        data_dir: settings.data_dir.clone(),
        deployment: settings.memory_deployment,
    };
    let Some((provider, class)) = open_driver(&config)? else {
        return Err(OpenCompanyError::Config(
            "the selected memory mode did not bind a provider".into(),
        ));
    };
    // Kept aside for the boot-time health probe; `bind` consumes its argument
    // and deliberately exposes no provider accessor (the ports are the only
    // data path). Clone is an `Arc` bump.
    let probe = provider.clone();
    let bound = BoundMemory::bind(provider, class)?;
    // Announce the bind: which engine, and — the part an operator cannot infer —
    // the class the *host* assigned it, since that is what decides whether the
    // egress and external-trust checks apply. Names the engine and its
    // capabilities, never the endpoint or the credential.
    tracing::info!(
        driver_id = bound.driver_id(),
        class = bound.class().as_str(),
        capabilities = ?bound.capability_names(),
        "memory engine bound"
    );
    if settings.memory_backend == MemoryBackend::Null {
        // Loud, once, at open: `null` is a legitimate choice but a surprising
        // one to inherit from a stale environment, and every read returning
        // empty is indistinguishable from a company that has not learned
        // anything yet.
        tracing::warn!(
            "OPENCOMPANY_MEMORY=null is bound: memory writes are accepted and discarded, and \
             every read is empty. Nothing this company is told will be remembered."
        );
    }
    Ok(Some(MemoryOverlay {
        memory: bound.memory(),
        context: bound.context(),
        facts: Some(bound.facts()),
        inbound_context: Some(bound.inbound_context()),
        scratch: Some(bound.scratch()),
        scopes: Some(Arc::new(bound.clone())),
        descriptor: MemoryDescriptor {
            backend: settings.memory_backend,
            driver_id: bound.driver_id().to_string(),
            capabilities: bound
                .capability_names()
                .into_iter()
                .map(str::to_string)
                .collect(),
            // Not probed yet: binding is offline by design, and the probe is
            // the caller's boot-time step (`refresh_health`).
            healthy: None,
        },
        probe: Some(probe),
    }))
}

/// Without the `tinymemory` feature the two provider-backed modes cannot be
/// served, so they refuse rather than silently resolving to something else.
#[cfg(not(feature = "tinymemory"))]
fn open_provider(settings: &StorageSettings) -> Result<Option<MemoryOverlay>> {
    Err(OpenCompanyError::Config(format!(
        "OPENCOMPANY_MEMORY={} requires a build with the `tinymemory` feature",
        settings.memory_backend.as_str()
    )))
}

#[cfg(feature = "sqlite")]
fn open_sqlite(data_dir: &Path) -> Result<Option<StorageHandles>> {
    let store = Arc::new(crate::store::SqliteStore::open(
        data_dir.join("opencompany.db"),
    )?);
    Ok(Some(StorageHandles {
        company: store.clone(),
        events: store.clone(),
        memory: store.clone(),
        context: store.clone(),
        secrets: store.clone(),
        inbox: store.clone(),
        tasks: store.clone(),
        ledgers: store.clone(),
        workspace: store.clone(),
        facts: store.clone(),
        artifacts: store.clone(),
        runs: store.clone(),
        workflow_revisions: store.clone(),
        schedule_fires: store.clone(),
        run_outputs: store.clone(),
        deep_trace: store.clone(),
        usage: store.clone(),
        skills: store.clone(),
        read_state: store.clone(),
        notifications: store.clone(),
        users: store.clone(),
        sessions: store.clone(),
        login_codes: store.clone(),
        journal: store,
        ownership: None,
    }))
}

#[cfg(not(feature = "sqlite"))]
fn open_sqlite(_data_dir: &Path) -> Result<Option<StorageHandles>> {
    Err(OpenCompanyError::Config(
        "OPENCOMPANY_STORAGE=sqlite requires a build with the `sqlite` feature".into(),
    ))
}

#[cfg(feature = "mongodb")]
async fn open_mongodb(settings: &StorageSettings) -> Result<Option<StorageHandles>> {
    let uri = settings.mongodb_uri.as_deref().ok_or_else(|| {
        OpenCompanyError::Config(
            "OPENCOMPANY_STORAGE=mongodb requires OPENCOMPANY_MONGODB_URI".into(),
        )
    })?;
    let db = settings.mongodb_db.as_deref().unwrap_or("opencompany");
    let store = Arc::new(crate::store::MongoStore::connect(uri, db).await?);
    Ok(Some(StorageHandles {
        company: store.clone(),
        events: store.clone(),
        memory: store.clone(),
        context: store.clone(),
        secrets: store.clone(),
        inbox: store.clone(),
        tasks: store.clone(),
        ledgers: store.clone(),
        workspace: store.clone(),
        facts: store.clone(),
        artifacts: store.clone(),
        runs: store.clone(),
        workflow_revisions: store.clone(),
        schedule_fires: store.clone(),
        run_outputs: store.clone(),
        deep_trace: store.clone(),
        usage: store.clone(),
        skills: store.clone(),
        read_state: store.clone(),
        notifications: store.clone(),
        users: store.clone(),
        sessions: store.clone(),
        login_codes: store.clone(),
        journal: store.clone(),
        ownership: Some(store),
    }))
}

#[cfg(not(feature = "mongodb"))]
async fn open_mongodb(_settings: &StorageSettings) -> Result<Option<StorageHandles>> {
    Err(OpenCompanyError::Config(
        "OPENCOMPANY_STORAGE=mongodb requires a build with the `mongodb` feature".into(),
    ))
}

#[cfg(feature = "mongodb")]
#[async_trait]
impl OwnershipStore for crate::store::MongoStore {
    async fn set_owner(&self, id: &CompanyId, tenant: &str) -> Result<()> {
        crate::store::MongoStore::set_owner(self, id, tenant).await
    }
    async fn remove_owner(&self, id: &CompanyId) -> Result<()> {
        crate::store::MongoStore::remove_owner(self, id).await
    }
    async fn owners(&self) -> Result<Vec<(CompanyId, String)>> {
        crate::store::MongoStore::owners(self).await
    }
}
#[cfg(test)]
mod test {
    use super::*;

    use crate::app::config::MapEnv;

    #[test]
    fn parses_storage_kinds() {
        assert_eq!("fs".parse::<StorageKind>().unwrap(), StorageKind::Fs);
        assert_eq!(
            "sqlite".parse::<StorageKind>().unwrap(),
            StorageKind::Sqlite
        );
        assert_eq!(
            "MongoDB".parse::<StorageKind>().unwrap(),
            StorageKind::Mongodb
        );
        assert!("postgres".parse::<StorageKind>().is_err());
    }

    /// Issue #752: only MongoDB keeps secret material off the container's own
    /// disk, so only MongoDB clears the repository-credential gates.
    #[test]
    fn only_mongodb_keeps_secrets_off_the_local_disk() {
        assert!(StorageKind::Fs.secrets_are_plaintext_on_disk());
        assert!(StorageKind::Sqlite.secrets_are_plaintext_on_disk());
        assert!(!StorageKind::Mongodb.secrets_are_plaintext_on_disk());
        // The default is the refusing side: a host that never resolved a
        // backend must not be treated as one that keeps secrets safely.
        assert!(StorageKind::default().secrets_are_plaintext_on_disk());
    }

    /// The refusal has to be actionable on its own — an operator reading it in
    /// a console toast has nothing else to go on.
    #[test]
    fn the_refusal_names_the_condition_and_both_remedies() {
        let message = plaintext_secret_refusal(StorageKind::Fs);
        assert!(message.contains("OPENCOMPANY_STORAGE=fs"), "{message}");
        assert!(message.contains("OPENCOMPANY_STORAGE=mongodb"), "{message}");
        assert!(message.contains("OPENCOMPANY_MONGODB_URI"), "{message}");
        assert!(message.contains("`repo` grant"), "{message}");
        assert!(message.contains("plaintext"), "{message}");
        // The named kind is the one actually in force, not a hard-coded "fs".
        assert!(
            plaintext_secret_refusal(StorageKind::Sqlite).contains("OPENCOMPANY_STORAGE=sqlite"),
        );
    }

    #[tokio::test]
    async fn fs_selection_uses_builder_defaults() {
        let settings = StorageSettings::default();
        let handles = open_storage(&settings, Path::new("/tmp")).await.unwrap();
        assert!(handles.is_none());
    }

    #[test]
    fn parses_memory_backends() {
        assert_eq!(
            "store".parse::<MemoryBackend>().unwrap(),
            MemoryBackend::Store
        );
        assert_eq!("".parse::<MemoryBackend>().unwrap(), MemoryBackend::Store);
        assert_eq!(
            "remote".parse::<MemoryBackend>().unwrap(),
            MemoryBackend::Remote
        );
        assert_eq!(
            "NULL".parse::<MemoryBackend>().unwrap(),
            MemoryBackend::Null
        );
        assert!("redis".parse::<MemoryBackend>().is_err());
    }

    #[test]
    fn the_removed_embedded_spellings_refuse_to_parse() {
        // The in-pod engines (`tinycortex`/`cortex`/`embedded`) were removed
        // with the `tinycortex` feature. A deployment still setting
        // `OPENCOMPANY_MEMORY=tinycortex` must fail loudly at boot, not be
        // quietly reinterpreted as a mode that no longer exists.
        for value in ["tinycortex", "cortex", "embedded"] {
            assert!(
                value.parse::<MemoryBackend>().is_err(),
                "{value} must no longer parse as a memory backend"
            );
        }
    }

    #[test]
    fn each_backend_reports_its_wire_name() {
        // A client reading status should never have to know both names.
        assert_eq!(MemoryBackend::Store.as_str(), "store");
        assert_eq!(MemoryBackend::Remote.as_str(), "remote");
        assert_eq!(MemoryBackend::Null.as_str(), "null");
    }

    #[test]
    fn the_parse_refusal_names_every_accepted_value() {
        let error = "redis".parse::<MemoryBackend>().err().unwrap().to_string();
        for value in ["store", "remote", "null"] {
            assert!(error.contains(value), "{value} missing from: {error}");
        }
    }

    #[test]
    fn settings_debug_never_renders_a_credential() {
        // `StorageSettings` is printed at boot, so a derived `Debug` would put a
        // memory credential and a MongoDB connection string in the startup log
        // of every tenant container.
        let settings = StorageSettings {
            mongodb_uri: Some("mongodb://user:hunter2@cluster.example/db".into()),
            memory_url: Some("https://memory.internal.example".into()),
            memory_api_key: Some("sk-memory-super-secret".into()),
            ..StorageSettings::default()
        };
        let rendered = format!("{settings:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("sk-memory-super-secret"), "{rendered}");
        assert!(!rendered.contains("memory.internal.example"), "{rendered}");
        // Still useful: it says the values are configured.
        assert!(rendered.contains("<set>"), "{rendered}");
    }

    #[cfg(feature = "tinymemory")]
    #[test]
    fn remote_without_a_url_or_key_refuses_at_open() {
        let settings = StorageSettings {
            memory_backend: MemoryBackend::Remote,
            memory_driver: Some("supermemory".into()),
            // Past the confidence gate, so this asserts the *configuration*
            // refusal rather than tripping over the one before it.
            ..StorageSettings::default()
        };
        let error = open_memory_overlay(&settings)
            .expect_err("remote without an endpoint must refuse")
            .to_string();
        assert!(error.contains("OPENCOMPANY_MEMORY_URL"), "{error}");
    }

    #[cfg(feature = "tinymemory")]
    #[test]
    fn remote_with_full_config_proceeds_to_the_driver_without_an_acceptance_flag() {
        // The unproven-remote acceptance flag is retired: the vendored
        // tinymemory now ships a driver conformance suite that runs against
        // all the hosted adapters, so the flag's premise is gone. A fully
        // configured remote proceeds to driver construction — the error here
        // is the driver failing to reach the (nonexistent) endpoint or an
        // admission refusal, never a demand for a deleted knob.
        let settings = StorageSettings {
            memory_backend: MemoryBackend::Remote,
            memory_driver: Some("supermemory".into()),
            memory_url: Some("https://memory.invalid".into()),
            memory_api_key: Some("k".into()),
            ..StorageSettings::default()
        };
        match open_memory_overlay(&settings) {
            // `Ok(None)` is the trap this arm exists to close: it is how a
            // silently skipped remote overlay would look, and an
            // error-message-only assertion would pass straight through it.
            Ok(overlay) => assert!(
                overlay.is_some(),
                "a fully configured remote must bind an overlay, not skip one"
            ),
            Err(error) => {
                let error = error.to_string();
                assert!(
                    !error.contains("ALLOW_UNPROVEN_REMOTE"),
                    "the retired knob must not be demanded: {error}"
                );
            }
        }
    }

    #[cfg(feature = "tinymemory")]
    #[test]
    fn remote_binds_and_reports_its_driver() {
        // The success half of the pair above: a complete configuration binds,
        // and the descriptor it reports back names the driver that was asked
        // for rather than a fallback. No acceptance step is involved — that
        // knob is retired.
        let settings = StorageSettings {
            memory_backend: MemoryBackend::Remote,
            memory_driver: Some("supermemory".into()),
            memory_url: Some("https://memory.example".into()),
            memory_api_key: Some("k".into()),
            ..StorageSettings::default()
        };
        let overlay = open_memory_overlay(&settings)
            .expect("a fully configured remote engine binds")
            .expect("remote yields an overlay");
        assert_eq!(overlay.descriptor.backend, MemoryBackend::Remote);
        assert_eq!(overlay.descriptor.driver_id, "supermemory");
        // Binding is offline: nothing has probed yet, and claiming health
        // before a probe would be the same lie in the other direction.
        assert_eq!(
            overlay.descriptor.healthy, None,
            "bind must not pre-claim health"
        );
        assert!(
            overlay.probe.is_some(),
            "the provider-seam overlay must carry a probe handle for the boot health check"
        );
    }

    #[cfg(feature = "tinymemory")]
    #[tokio::test]
    async fn refresh_health_records_the_probe_answer() {
        // `null` is the one driver whose health is deterministic offline —
        // its `health()` is `Ready` by contract — so it proves the probe
        // path end-to-end with no network: probe handle → `refresh_health`
        // → `descriptor.healthy`, the value `/spec` serves.
        let settings = StorageSettings {
            memory_backend: MemoryBackend::Null,
            ..StorageSettings::default()
        };
        let mut overlay = open_memory_overlay(&settings)
            .expect("null binds")
            .expect("null yields an overlay");
        assert_eq!(overlay.descriptor.healthy, None);
        overlay
            .refresh_health(std::time::Duration::from_secs(5))
            .await;
        assert_eq!(
            overlay.descriptor.healthy,
            Some(true),
            "the probe's answer must land on the descriptor"
        );
    }

    /// The whole health vocabulary, pinned per outcome: `Degraded` is still
    /// serving — reduced, not absent — so it must read healthy; only `Down`
    /// and a timeout mean the next memory-needing cycle fails.
    #[cfg(feature = "tinymemory")]
    #[test]
    fn probe_mapping_counts_degraded_as_healthy_and_down_or_timeout_as_not() {
        use tinymemory_api::health::MemoryHealth;
        assert!(super::probe_answer_is_healthy(&Some(MemoryHealth::Ready)));
        assert!(super::probe_answer_is_healthy(&Some(
            MemoryHealth::Degraded {
                reason: "index rebuilding".into()
            }
        )));
        assert!(!super::probe_answer_is_healthy(&Some(MemoryHealth::Down {
            reason: "connection refused".into()
        })));
        assert!(
            !super::probe_answer_is_healthy(&None),
            "a timed-out probe must read unhealthy, not unknown"
        );
    }

    #[cfg(feature = "tinymemory")]
    #[test]
    fn the_gate_applies_only_to_remote() {
        // `null` retains nothing by design, so it is not routing memory at an
        // unproven third party and is not behind this gate.
        let settings = StorageSettings {
            memory_backend: MemoryBackend::Null,
            ..StorageSettings::default()
        };
        assert!(
            open_memory_overlay(&settings).is_ok(),
            "null must not be gated on the remote-adapter assertion"
        );
    }

    #[cfg(feature = "tinymemory")]
    #[test]
    fn null_opens_and_reports_itself() {
        let settings = StorageSettings {
            memory_backend: MemoryBackend::Null,
            ..StorageSettings::default()
        };
        let overlay = open_memory_overlay(&settings)
            .unwrap()
            .expect("null binds an overlay");
        assert_eq!(overlay.descriptor.backend, MemoryBackend::Null);
        assert_eq!(overlay.descriptor.driver_id, "null");
        // A bound provider serves all three seam partitions, not just facts.
        // Asserting each one separately is what catches a partition that is
        // wired to `None` at the construction site while the others are not —
        // which reads downstream as "this engine has no scratch", not as a bug.
        assert!(overlay.facts.is_some(), "a provider serves facts too");
        assert!(
            overlay.inbound_context.is_some(),
            "a provider serves the inbound-context partition"
        );
        assert!(
            overlay.scratch.is_some(),
            "a provider serves the scratch partition"
        );
    }

    #[cfg(not(feature = "tinymemory"))]
    #[test]
    fn the_provider_modes_require_the_feature() {
        for backend in [MemoryBackend::Remote, MemoryBackend::Null] {
            let settings = StorageSettings {
                memory_backend: backend,
                ..StorageSettings::default()
            };
            let error = open_memory_overlay(&settings).err().unwrap().to_string();
            assert!(error.contains("`tinymemory` feature"), "{error}");
        }
    }

    #[test]
    fn default_memory_backend_is_store() {
        assert_eq!(
            StorageSettings::default().memory_backend,
            MemoryBackend::Store
        );
        // Store is the no-op: no overlay, base backend keeps its own memory.
        assert!(
            open_memory_overlay(&StorageSettings::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn from_env_reads_the_remote_memory_knobs() {
        // The four knobs `remote` needs. Without this, a rename in `from_env`
        // would surface as "the engine refuses and names a variable you did
        // set", which reads as a broken deployment rather than a broken parse.
        const KEYS: [&str; 3] = [
            "OPENCOMPANY_MEMORY_DRIVER",
            "OPENCOMPANY_MEMORY_URL",
            "OPENCOMPANY_MEMORY_API_KEY",
        ];
        let env = MapEnv::new([
            (KEYS[0], "supermemory"),
            (KEYS[1], "https://memory.example"),
            (KEYS[2], "sk-test"),
        ]);
        let settings = StorageSettings::from_env_source(&env).unwrap();
        assert_eq!(settings.memory_driver.as_deref(), Some("supermemory"));
        assert_eq!(
            settings.memory_url.as_deref(),
            Some("https://memory.example")
        );
        assert_eq!(settings.memory_api_key.as_deref(), Some("sk-test"));

        // Empty is absent, not an empty credential: `require` would otherwise
        // accept a blank key and defer the failure to the first call.
        let blank =
            StorageSettings::from_env_source(&MapEnv::new([(KEYS[0], ""), (KEYS[2], "")])).unwrap();
        assert_eq!(blank.memory_driver, None);
        assert_eq!(blank.memory_api_key, None);

        let unset = StorageSettings::from_env_source(&MapEnv::default()).unwrap();
        assert_eq!(unset.memory_driver, None);
        assert_eq!(unset.memory_url, None);
        assert_eq!(unset.memory_api_key, None);
    }

    #[test]
    fn from_env_reads_memory_backend() {
        let env = MapEnv::new([("OPENCOMPANY_MEMORY", "remote")]);
        assert_eq!(
            StorageSettings::from_env_source(&env)
                .unwrap()
                .memory_backend,
            MemoryBackend::Remote
        );

        assert_eq!(
            StorageSettings::from_env_source(&MapEnv::default())
                .unwrap()
                .memory_backend,
            MemoryBackend::Store
        );
    }

    #[test]
    fn from_env_reads_tenant_id() {
        let env = MapEnv::new([("OPENCOMPANY_TENANT_ID", "acme")]);
        assert_eq!(
            StorageSettings::from_env_source(&env)
                .unwrap()
                .tenant_id
                .as_deref(),
            Some("acme")
        );

        // An empty value is filtered out, same as the mongodb vars.
        assert_eq!(
            StorageSettings::from_env_source(&MapEnv::new([("OPENCOMPANY_TENANT_ID", "")]))
                .unwrap()
                .tenant_id,
            None
        );

        // Unset leaves it `None` (the id-namespacing no-op).
        assert_eq!(
            StorageSettings::from_env_source(&MapEnv::default())
                .unwrap()
                .tenant_id,
            None
        );
    }

    #[test]
    fn from_env_reads_data_dir() {
        let env = MapEnv::new([("OPENCOMPANY_DATA_DIR", "/srv/oc-data")]);
        assert_eq!(
            StorageSettings::from_env_source(&env).unwrap().data_dir,
            Some(PathBuf::from("/srv/oc-data")),
            "OPENCOMPANY_DATA_DIR must be read into StorageSettings::data_dir"
        );
    }

    #[test]
    fn from_env_reads_allow_ephemeral_memory() {
        const KEY: &str = "OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL";
        assert!(
            !StorageSettings::from_env_source(&MapEnv::default())
                .unwrap()
                .allow_ephemeral_memory
        );

        // Truthy values set the durability assertion.
        for truthy in ["1", "true", "YES", "On"] {
            assert!(
                StorageSettings::from_env_source(&MapEnv::new([(KEY, truthy)]))
                    .unwrap()
                    .allow_ephemeral_memory,
                "{truthy:?} must read as durability asserted"
            );
        }

        // Any non-truthy value stays false (fails safe toward refusal).
        for falsy in ["0", "false", "no", ""] {
            assert!(
                !StorageSettings::from_env_source(&MapEnv::new([(KEY, falsy)]))
                    .unwrap()
                    .allow_ephemeral_memory,
                "{falsy:?} must read as not asserted"
            );
        }
    }

    #[test]
    fn with_memory_config_from_resolves_ownership_from_the_injected_source() {
        let section = crate::app::config::MemorySection {
            backend: Some("remote".into()),
            driver: Some("supermemory".into()),
            url: Some("https://memory.example".into()),
            ..Default::default()
        };

        // The injected source owns the choice: the `config.toml` layer is
        // inert, exactly as it is for a deployment env naming an engine.
        let env = MapEnv::new([("OPENCOMPANY_MEMORY", "store")]);
        let settings = StorageSettings::from_env_source(&env)
            .unwrap()
            .with_memory_config_from(&env, &section)
            .unwrap();
        assert_eq!(settings.memory_backend, MemoryBackend::Store);
        assert_eq!(settings.memory_driver, None);
        assert_eq!(settings.memory_url, None);

        // No injected ownership: the file layer is applied.
        let unset = StorageSettings::from_env_source(&MapEnv::default())
            .unwrap()
            .with_memory_config_from(&MapEnv::default(), &section)
            .unwrap();
        assert_eq!(unset.memory_backend, MemoryBackend::Remote);
        assert_eq!(unset.memory_driver.as_deref(), Some("supermemory"));
        assert_eq!(unset.memory_url.as_deref(), Some("https://memory.example"));
    }

    #[cfg(feature = "mongodb")]
    #[tokio::test]
    async fn mongodb_selection_requires_uri() {
        let settings = StorageSettings {
            kind: StorageKind::Mongodb,
            ..Default::default()
        };
        let error = open_storage(&settings, std::path::Path::new("/tmp"))
            .await
            .expect_err("mongodb without a URI must refuse")
            .to_string();
        assert!(error.contains("OPENCOMPANY_MONGODB_URI"), "{error}");
    }
    /// The bundle-command refusals, executed — the #1279 review neutralised
    /// the bin-resident versions with `if false &&` and nothing went red;
    /// these are the tests that make that mutation fail.
    #[test]
    fn bundle_env_refusals_fire_and_the_fs_default_passes() {
        // fs+store default: both flag spellings pass — no regression.
        let default = StorageSettings::default();
        refuse_bundle_env(&default, false).expect("default env, no flag");
        refuse_bundle_env(&default, true).expect("default env, explicit --home");

        // null refuses in both directions regardless of the flag.
        let null = StorageSettings {
            memory_backend: MemoryBackend::Null,
            ..StorageSettings::default()
        };
        for flagged in [false, true] {
            let err = refuse_bundle_env(&null, flagged)
                .expect_err("null must refuse")
                .to_string();
            assert!(err.contains("OPENCOMPANY_MEMORY=null"), "{err}");
        }

        // A live environment refuses an explicit --home (two deployments in
        // one bundle) but proceeds without the flag.
        let live = StorageSettings {
            kind: StorageKind::Mongodb,
            ..StorageSettings::default()
        };
        let err = refuse_bundle_env(&live, true)
            .expect_err("live env + --home must refuse")
            .to_string();
        assert!(err.contains("--home"), "{err}");
        refuse_bundle_env(&live, false).expect("live env without the flag proceeds");

        // Tenant mode refuses on a live env; whitespace does not count as set.
        let tenant = StorageSettings {
            kind: StorageKind::Mongodb,
            tenant_id: Some("acme".into()),
            ..StorageSettings::default()
        };
        let err = refuse_bundle_env(&tenant, false)
            .expect_err("tenant mode must refuse")
            .to_string();
        assert!(err.contains("OPENCOMPANY_TENANT_ID"), "{err}");
        let blank_tenant = StorageSettings {
            kind: StorageKind::Mongodb,
            tenant_id: Some("  ".into()),
            ..StorageSettings::default()
        };
        refuse_bundle_env(&blank_tenant, false).expect("blank tenant id is unset");
    }
}
