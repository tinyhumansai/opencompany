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

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::context::ContextStore;
use crate::ports::events::EventLog;
use crate::ports::facts::FactStore;
use crate::ports::inbox::InboxStore;
use crate::ports::login_codes::LoginCodeStore;
use crate::ports::memory::MemoryStore;
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

impl StorageSettings {
    /// Reads the CLI-surface storage env vars (`OPENCOMPANY_STORAGE`,
    /// `OPENCOMPANY_MONGODB_URI`, `OPENCOMPANY_MONGODB_DB`,
    /// `OPENCOMPANY_TENANT_ID`, `OPENCOMPANY_MEMORY`).
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
        MemoryBackend::Tinycortex => open_tinycortex(),
    }
}

#[cfg(feature = "tinycortex")]
fn open_tinycortex() -> Result<Option<MemoryOverlay>> {
    let (memory, context) = crate::store::tinycortex::in_memory();
    Ok(Some(MemoryOverlay { memory, context }))
}

#[cfg(not(feature = "tinycortex"))]
fn open_tinycortex() -> Result<Option<MemoryOverlay>> {
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

    /// Serializes the tests that mutate process-global env vars, so they never
    /// race each other (or `from_env`) under the parallel test harness.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK against other env-mutating tests;
        // restores prior state.
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
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by ENV_LOCK against other env-mutating tests;
        // restores prior state.
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
