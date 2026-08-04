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
use crate::error::OpenCompanyError;
use crate::ports::artifacts::ArtifactStore;
use crate::ports::context::ContextStore;
use crate::ports::events::EventLog;
use crate::ports::facts::FactStore;
use crate::ports::inbox::InboxStore;
use crate::ports::login_codes::LoginCodeStore;
use crate::ports::memory::MemoryStore;
use crate::ports::runs::RunStore;
use crate::ports::secrets::SecretStore;
use crate::ports::sessions::SessionStore;
use crate::ports::skills_state::SkillStateStore;
use crate::ports::store::CompanyStore;
use crate::ports::tasks::TaskStore;
use crate::ports::types::CompanyId;
use crate::ports::usage::UsageMeter;
use crate::ports::users::UserStore;
use crate::ports::workspace::WorkspaceStore;

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
/// (companies, events, secrets, …) while `OPENCOMPANY_MEMORY` can swap just the
/// two knowledge ports onto a dedicated memory engine. This is why TinyCortex
/// is *not* a [`StorageKind`] — it only implements memory + context, not the
/// other durable ports, so it layers on top rather than replacing the base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MemoryBackend {
    /// Memory + context come from the base [`StorageKind`] (the default; fs
    /// substring recall, or the sqlite/mongodb store).
    #[default]
    Store,
    /// TinyCortex backs memory + context: ranked token-overlap recall over a
    /// compounding chunk store, on top of whatever base backend is selected
    /// (`tinycortex` feature).
    Tinycortex,
}

impl std::str::FromStr for MemoryBackend {
    type Err = OpenCompanyError;
    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "store" | "" => Ok(Self::Store),
            "tinycortex" | "cortex" => Ok(Self::Tinycortex),
            other => Err(OpenCompanyError::Config(format!(
                "OPENCOMPANY_MEMORY must be 'store' or 'tinycortex', got '{other}'"
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
}

impl std::fmt::Debug for MemoryOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryOverlay").finish_non_exhaustive()
    }
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
    pub workspace: Arc<dyn WorkspaceStore>,
    pub facts: Arc<dyn FactStore>,
    pub artifacts: Arc<dyn ArtifactStore>,
    /// First-class task-run records and their step traces (#242).
    pub runs: Arc<dyn RunStore>,
    pub usage: Arc<dyn UsageMeter>,
    pub skills: Arc<dyn SkillStateStore>,
    pub users: Arc<dyn UserStore>,
    pub sessions: Arc<dyn SessionStore>,
    pub login_codes: Arc<dyn LoginCodeStore>,
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

/// Connection settings for [`open_storage`]. `fs` needs nothing beyond the
/// runtime's home directory (handled by the builder's defaults), so it yields
/// `None` handles.
#[derive(Clone, Debug, Default)]
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
    /// (`OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL`). The in-pod TinyCortex engine is
    /// refused by default under `OPENCOMPANY_STORAGE=mongodb`, because the hosted
    /// model treats `/data` as ephemeral scratch there and engine memory would be
    /// silently lost on restart. Setting this flag is the operator asserting that
    /// they have mounted a genuinely persistent volume at the data dir, which
    /// lifts the refusal for the mongodb+tinycortex combination. `false` (the
    /// [`Default`], and the safe default) keeps the silent-memory-loss guard.
    pub allow_ephemeral_memory: bool,
}

/// Parses env var `key` into `T`. Absent → `Ok(None)` (the caller applies its
/// default); a set-but-non-UTF-8 value is a hard [`OpenCompanyError::Config`]
/// rather than a silent fallback to the default.
fn parse_env<T>(key: &str) -> Result<Option<T>>
where
    T: std::str::FromStr<Err = OpenCompanyError>,
{
    match std::env::var(key) {
        Ok(raw) => Ok(Some(raw.parse()?)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(OpenCompanyError::Config(format!(
            "{key} is set but is not valid UTF-8"
        ))),
    }
}

/// Reads a boolean opt-in env flag. Truthy values (case-insensitive, trimmed):
/// `1`, `true`, `yes`, `on`. Anything else — including unset — is `false`.
fn env_flag(key: &str) -> bool {
    std::env::var(key)
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
        let kind: StorageKind = parse_env("OPENCOMPANY_STORAGE")?.unwrap_or_default();
        let memory_backend: MemoryBackend = parse_env("OPENCOMPANY_MEMORY")?.unwrap_or_default();
        let non_empty = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        Ok(Self {
            kind,
            mongodb_uri: non_empty("OPENCOMPANY_MONGODB_URI"),
            mongodb_db: non_empty("OPENCOMPANY_MONGODB_DB"),
            tenant_id: non_empty("OPENCOMPANY_TENANT_ID"),
            memory_backend,
            data_dir: Some(crate::app::config::data_dir_from_env()),
            allow_ephemeral_memory: env_flag("OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL"),
        })
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

/// Opens the memory + context overlay selected by `OPENCOMPANY_MEMORY`.
///
/// `Ok(None)` means [`MemoryBackend::Store`] — the base backend keeps its own
/// memory, no overlay. A selected-but-unavailable engine (feature disabled) is
/// an error, never a silent fallback, mirroring [`open_storage`].
pub fn open_memory_overlay(settings: &StorageSettings) -> Result<Option<MemoryOverlay>> {
    match settings.memory_backend {
        MemoryBackend::Store => Ok(None),
        MemoryBackend::Tinycortex => open_tinycortex(settings),
    }
}

/// Opens the TinyCortex overlay. With a `data_dir` present it is the persistent,
/// in-pod [`EngineCortex`](crate::store::tinycortex_engine::EngineCortex) rooted
/// at `<data_dir>/memory/`; without one (tests, no-data-dir callers) it is the
/// offline in-memory backend.
///
/// Two boot-time contracts are enforced here rather than left to silently
/// surprise an operator at runtime:
///
/// 1. **Refuse-to-open on ephemeral `/data`, unless durability is asserted.**
///    `OPENCOMPANY_STORAGE=mongodb` makes the container's data dir ephemeral
///    scratch (the database is the durable base), so an in-pod engine rooted
///    there would lose *all* memory on every restart. That is silent data loss,
///    so by default this combination is a hard [`OpenCompanyError::Config`] — we
///    never open a doomed engine. But storage-kind is only a *proxy* for
///    "ephemeral `/data`": a mongodb deployment that HAS mounted a persistent
///    volume at the data dir is perfectly safe. So the refusal is an explicit
///    durability contract, not a hard-coded storage-kind rejection: an operator
///    who has mounted a durable volume sets
///    `OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL=1` (surfaced as
///    [`StorageSettings::allow_ephemeral_memory`]) to assert it, and the engine
///    opens. Unset (the safe default) still refuses the mongodb+tinycortex combo.
/// 2. **Meaning tier, with a loud degraded-mode fallback.** A hosted embeddings
///    backend is resolved from the environment (188c2); when one is present each
///    stored chunk is embedded and recall runs vector-first (cosine) with a
///    lexical top-up. When **no** backend resolves, recall degrades to *lexical*
///    (substring/recency token-overlap) — **not** the vector/semantic recall the
///    `tinycortex` name implies — and that is announced once, loudly, at open so
///    it is never mistaken for real embedding recall.
#[cfg(feature = "tinycortex")]
fn open_tinycortex(settings: &StorageSettings) -> Result<Option<MemoryOverlay>> {
    let (memory, context) = match &settings.data_dir {
        Some(dir) => {
            // Refuse-to-open contract: the engine persists to `<data_dir>/memory`
            // on the local container filesystem. Under `OPENCOMPANY_STORAGE=mongodb`
            // the hosting model treats `/data` as ephemeral scratch (the durable
            // base is the database), so engine memory would be silently lost on
            // every restart. Refusing to open beats warning-then-losing-data: the
            // failure mode we are guarding against is exactly a quiet memory wipe on
            // restart. But storage-kind is only a proxy for "ephemeral /data" — a
            // mongodb deploy with a genuinely persistent volume is safe — so the
            // operator can lift the refusal by explicitly asserting durability via
            // OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL. See docs/spec/runtime/storage.md.
            if settings.kind == StorageKind::Mongodb && !settings.allow_ephemeral_memory {
                return Err(OpenCompanyError::Config(
                    "OPENCOMPANY_MEMORY=tinycortex needs a persistent volume at the data dir, but \
                     OPENCOMPANY_STORAGE=mongodb makes /data ephemeral scratch by default, so \
                     in-pod memory would be silently lost on restart. If you have mounted a \
                     genuinely persistent volume at OPENCOMPANY_DATA_DIR, set \
                     OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL=1 to assert its durability and open the \
                     engine anyway. Otherwise use OPENCOMPANY_STORAGE=fs or sqlite (durable /data), \
                     or keep memory on the base store with OPENCOMPANY_MEMORY=store."
                        .into(),
                ));
            }
            // Meaning tier (188c2): resolve a hosted embeddings backend from the
            // environment when one is configured, so recall is vector-first
            // (semantic) rather than lexical-only. `None` (no hosted credential, or
            // a default build without the `openhuman` harness) keeps the lexical
            // path — the embeddings client lives in the openhuman-gated harness, so
            // the type is only reachable there.
            let embeddings = hosted_embeddings_backend();
            // Loud, one-time degraded-mode contract: with no embeddings backend
            // recall is lexical (substring/recency token-overlap), NOT the
            // vector/semantic recall the name implies. Announce it once at open so
            // it is never mistaken for real embedding recall.
            if embeddings.is_none() {
                tracing::warn!(
                    data_dir = %dir.display(),
                    "OPENCOMPANY_MEMORY=tinycortex is running in DEGRADED lexical fallback mode: no \
                     embeddings backend resolved, so recall is substring/recency token-overlap, \
                     NOT vector/semantic recall. Configure a hosted embeddings backend for \
                     semantic recall.",
                );
            }
            crate::store::tinycortex_engine::engine_with_embeddings(dir.join("memory"), embeddings)
        }
        None => crate::store::tinycortex::in_memory(),
    };
    Ok(Some(MemoryOverlay { memory, context }))
}

/// Resolves the hosted embeddings backend for the memory meaning tier from the
/// process environment, as an `Arc<dyn EmbeddingBackend>` the vector store
/// consumes. Only the `openhuman` build can build one (that is where the hosted
/// embeddings client lives); every other build gets `None` and lexical recall.
#[cfg(feature = "tinycortex")]
fn hosted_embeddings_backend()
-> Option<Arc<dyn tinycortex::memory::store::vectors::embedding::EmbeddingBackend>> {
    #[cfg(feature = "openhuman")]
    {
        use tinycortex::memory::store::vectors::embedding::EmbeddingBackend;
        crate::harness::embeddings::hosted_embeddings_from_env(&crate::app::config::ProcessEnv)
            .map(|backend| Arc::new(backend) as Arc<dyn EmbeddingBackend>)
    }
    #[cfg(not(feature = "openhuman"))]
    {
        None
    }
}

#[cfg(not(feature = "tinycortex"))]
fn open_tinycortex(_settings: &StorageSettings) -> Result<Option<MemoryOverlay>> {
    Err(OpenCompanyError::Config(
        "OPENCOMPANY_MEMORY=tinycortex requires a build with the `tinycortex` feature".into(),
    ))
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
        workspace: store.clone(),
        facts: store.clone(),
        artifacts: store.clone(),
        runs: store.clone(),
        usage: store.clone(),
        skills: store.clone(),
        users: store.clone(),
        sessions: store.clone(),
        login_codes: store,
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
        workspace: store.clone(),
        facts: store.clone(),
        artifacts: store.clone(),
        runs: store.clone(),
        usage: store.clone(),
        skills: store.clone(),
        users: store.clone(),
        sessions: store.clone(),
        login_codes: store.clone(),
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
            "TinyCortex".parse::<MemoryBackend>().unwrap(),
            MemoryBackend::Tinycortex
        );
        assert_eq!(
            "cortex".parse::<MemoryBackend>().unwrap(),
            MemoryBackend::Tinycortex
        );
        assert!("redis".parse::<MemoryBackend>().is_err());
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
    fn from_env_reads_memory_backend() {
        // SAFETY: single-threaded test; restores prior state.
        let prev = std::env::var("OPENCOMPANY_MEMORY").ok();

        unsafe { std::env::set_var("OPENCOMPANY_MEMORY", "tinycortex") };
        assert_eq!(
            StorageSettings::from_env().unwrap().memory_backend,
            MemoryBackend::Tinycortex
        );

        unsafe { std::env::remove_var("OPENCOMPANY_MEMORY") };
        assert_eq!(
            StorageSettings::from_env().unwrap().memory_backend,
            MemoryBackend::Store
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("OPENCOMPANY_MEMORY", v) },
            None => unsafe { std::env::remove_var("OPENCOMPANY_MEMORY") },
        }
    }

    #[cfg(feature = "tinycortex")]
    #[tokio::test]
    async fn tinycortex_overlay_recalls_stored_chunks() {
        use crate::ports::types::{CompanyId, ContextChunk};

        let settings = StorageSettings {
            memory_backend: MemoryBackend::Tinycortex,
            ..Default::default()
        };
        let overlay = open_memory_overlay(&settings).unwrap().expect("overlay");

        let company = CompanyId::new("acme");
        overlay
            .context
            .put(
                &company,
                ContextChunk {
                    label: "notes/q3".into(),
                    body: "revenue grew in the q3 report".into(),
                },
            )
            .await
            .unwrap();

        // The chunk is recallable by content — the compounding-memory contract.
        let hits = overlay
            .context
            .search(&company, "q3 revenue", 5)
            .await
            .unwrap();
        assert!(hits.iter().any(|h| h.score > 0.0), "expected a ranked hit");

        // Isolation: another company never sees acme's chunk.
        let other = CompanyId::new("globex");
        let leaked = overlay
            .context
            .search(&other, "q3 revenue", 5)
            .await
            .unwrap();
        assert!(leaked.is_empty(), "cross-company recall must not bleed");
    }

    /// Refuse-to-open contract: `OPENCOMPANY_STORAGE=mongodb` makes `/data`
    /// ephemeral, so opening the in-pod engine there would silently lose memory
    /// on restart. That combination must be a hard error, not a warning.
    #[cfg(feature = "tinycortex")]
    #[test]
    fn tinycortex_refuses_ephemeral_mongodb_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let settings = StorageSettings {
            kind: StorageKind::Mongodb,
            memory_backend: MemoryBackend::Tinycortex,
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let err = open_memory_overlay(&settings).expect_err("mongodb /data must refuse to open");
        let msg = err.to_string();
        assert!(
            msg.contains("silently lost on restart"),
            "error must name the silent-memory-loss failure mode, got: {msg}"
        );
    }

    /// The refusal is an explicit durability *contract*, not a hard storage-kind
    /// reject: an operator who has mounted a persistent volume under a mongodb
    /// deployment asserts it via `OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL` (surfaced as
    /// `allow_ephemeral_memory`), and the engine then opens instead of refusing.
    #[cfg(feature = "tinycortex")]
    #[test]
    fn tinycortex_opens_ephemeral_mongodb_when_durability_asserted() {
        let dir = tempfile::tempdir().unwrap();
        let settings = StorageSettings {
            kind: StorageKind::Mongodb,
            memory_backend: MemoryBackend::Tinycortex,
            data_dir: Some(dir.path().to_path_buf()),
            allow_ephemeral_memory: true,
            ..Default::default()
        };
        assert!(
            open_memory_overlay(&settings)
                .unwrap_or_else(|e| panic!("durability-asserted mongodb must open: {e}"))
                .is_some(),
            "asserting durability must lift the mongodb+tinycortex refusal"
        );
    }

    /// The refuse is scoped to the ephemeral-`/data` combination only: durable
    /// base backends (fs, sqlite) still open the engine overlay normally.
    #[cfg(feature = "tinycortex")]
    #[test]
    fn tinycortex_opens_on_durable_fs_and_sqlite() {
        for kind in [StorageKind::Fs, StorageKind::Sqlite] {
            let dir = tempfile::tempdir().unwrap();
            let settings = StorageSettings {
                kind,
                memory_backend: MemoryBackend::Tinycortex,
                data_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            };
            assert!(
                open_memory_overlay(&settings)
                    .unwrap_or_else(|e| panic!("durable {kind:?} base must open: {e}"))
                    .is_some(),
                "durable {kind:?} base must yield an engine overlay"
            );
        }
    }

    #[cfg(not(feature = "tinycortex"))]
    #[test]
    fn tinycortex_overlay_requires_feature() {
        let settings = StorageSettings {
            memory_backend: MemoryBackend::Tinycortex,
            ..Default::default()
        };
        assert!(open_memory_overlay(&settings).is_err());
    }

    #[test]
    fn from_env_reads_tenant_id() {
        // SAFETY: single-threaded test; restores prior state.
        let prev = std::env::var("OPENCOMPANY_TENANT_ID").ok();

        unsafe { std::env::set_var("OPENCOMPANY_TENANT_ID", "acme") };
        assert_eq!(
            StorageSettings::from_env().unwrap().tenant_id.as_deref(),
            Some("acme")
        );

        // An empty value is filtered out, same as the mongodb vars.
        unsafe { std::env::set_var("OPENCOMPANY_TENANT_ID", "") };
        assert_eq!(StorageSettings::from_env().unwrap().tenant_id, None);

        // Unset leaves it `None` (the id-namespacing no-op).
        unsafe { std::env::remove_var("OPENCOMPANY_TENANT_ID") };
        assert_eq!(StorageSettings::from_env().unwrap().tenant_id, None);

        match prev {
            Some(v) => unsafe { std::env::set_var("OPENCOMPANY_TENANT_ID", v) },
            None => unsafe { std::env::remove_var("OPENCOMPANY_TENANT_ID") },
        }
    }

    #[test]
    fn from_env_reads_data_dir() {
        // SAFETY: single-threaded test; restores prior state.
        let prev = std::env::var("OPENCOMPANY_DATA_DIR").ok();

        // An explicit data dir is threaded straight through into settings.
        unsafe { std::env::set_var("OPENCOMPANY_DATA_DIR", "/srv/oc-data") };
        assert_eq!(
            StorageSettings::from_env().unwrap().data_dir,
            Some(PathBuf::from("/srv/oc-data")),
            "OPENCOMPANY_DATA_DIR must be read into StorageSettings::data_dir"
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("OPENCOMPANY_DATA_DIR", v) },
            None => unsafe { std::env::remove_var("OPENCOMPANY_DATA_DIR") },
        }
    }

    #[test]
    fn from_env_reads_allow_ephemeral_memory() {
        // SAFETY: single-threaded test; restores prior state.
        let prev = std::env::var("OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL").ok();

        // Unset → the safe default: refuse (flag false).
        unsafe { std::env::remove_var("OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL") };
        assert!(!StorageSettings::from_env().unwrap().allow_ephemeral_memory);

        // Truthy values set the durability assertion.
        for truthy in ["1", "true", "YES", "On"] {
            unsafe { std::env::set_var("OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL", truthy) };
            assert!(
                StorageSettings::from_env().unwrap().allow_ephemeral_memory,
                "{truthy:?} must read as durability asserted"
            );
        }

        // Any non-truthy value stays false (fails safe toward refusal).
        for falsy in ["0", "false", "no", ""] {
            unsafe { std::env::set_var("OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL", falsy) };
            assert!(
                !StorageSettings::from_env().unwrap().allow_ephemeral_memory,
                "{falsy:?} must read as not asserted"
            );
        }

        match prev {
            Some(v) => unsafe { std::env::set_var("OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL", v) },
            None => unsafe { std::env::remove_var("OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL") },
        }
    }

    #[cfg(feature = "mongodb")]
    #[tokio::test]
    async fn mongodb_selection_requires_uri() {
        let settings = StorageSettings {
            kind: StorageKind::Mongodb,
            ..Default::default()
        };
        assert!(open_storage(&settings, Path::new("/tmp")).await.is_err());
    }
}
