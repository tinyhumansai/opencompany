//! Filesystem-backed implementations of the persistence ports.
//!
//! All state lives in per-company [`Bundle`] directories (TOML for the
//! manifest, JSONL for append-only logs, content-addressed blobs for context).
//! Appends are the hot path and never rewrite the whole file; per-path
//! `tokio::sync::Mutex` locks serialize concurrent writers within a process.
//! Those locks live in one process-wide registry (`path_lock`) rather than on
//! each store, so two instances over one bundle actually meet (issue #388).

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::{Mutex as TokioMutex, broadcast};

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::context::ContextStore;
use crate::ports::events::EventLog;
use crate::ports::inbox::{EmailRecord, InboxMeta, InboxStore};
use crate::ports::memory::MemoryStore;
use crate::ports::secrets::SecretStore;
use crate::ports::store::CompanyStore;
use crate::ports::types::{
    ChunkAddr, ChunkHit, ChunkMeta, CompanyEvent, CompanyId, CompanyRecord, CompanySummary,
    CompressedTrace, ContextChunk, EventSeq, EvictionPolicy, LedgerEntry, SecretValue, StoredEvent,
    TaskResult,
};
use crate::ports::{generate_id, now_millis};
use crate::store::content_address;
use crate::store::paths::Bundle;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(crate) fn io_err(path: &Path, source: std::io::Error) -> OpenCompanyError {
    OpenCompanyError::StoreIo {
        path: path.to_path_buf(),
        source,
    }
}

/// A registry of per-path async locks, so appends to the same file serialize
/// while distinct files stay concurrent.
#[derive(Clone, Default)]
pub(crate) struct PathLocks {
    inner: Arc<StdMutex<HashMap<PathBuf, Arc<TokioMutex<()>>>>>,
}

impl PathLocks {
    pub(crate) fn get(&self, path: &Path) -> Arc<TokioMutex<()>> {
        let mut map = self.inner.lock().expect("path-lock map poisoned");
        map.entry(path.to_path_buf()).or_default().clone()
    }
}

/// Every filesystem store's write locks, shared by every instance in the
/// process (issue #388).
///
/// The locks these replaced were **fields** on `FsCompanyStore`, `FsEventLog`,
/// `FsMemoryStore`, `FsContextStore`, `FsInboxStore` and `FsOps` — so two stores
/// over one bundle serialised against nothing, which is the state those types
/// have always been in and which nothing stopped a caller reaching: each
/// constructor takes a root and builds a fresh registry. A `static` is the only
/// thing two independently-constructed instances can share.
///
/// The damage that let through was not theoretical. `FsEventLog::append`
/// derives the next sequence from the current line count and then appends, so
/// two unsynchronised instances hand out the **same** `seq` — breaking every
/// consumer that treats it as an identity. And the read-modify-write sites
/// (`FsMemoryStore::evict`, `FsInboxStore::mark_read`, and the whole-file
/// rewrites in [`FsOps`](crate::store::fs_ops::FsOps)) replace the file with a
/// snapshot, so an append that raced one of them was simply erased.
///
/// Mirrors the precedent
/// [`JOURNAL_WRITE_LOCKS`](crate::runtime::journal) set for the runtime journal
/// in issue #386, deliberately including its caveats — see [`path_lock`].
static FS_WRITE_LOCKS: std::sync::LazyLock<PathLocks> =
    std::sync::LazyLock::new(PathLocks::default);

/// The process-wide write lock for `path`.
///
/// Keyed on the **absolutised** path, so a relative and an absolute spelling of
/// one file meet on the same lock instead of racing.
///
/// Two limits, both by construction rather than oversight:
///
/// * **Absolutising is not canonicalising.** `std::path::absolute` is purely
///   lexical: it never touches the filesystem, so it does not resolve symlinks
///   and it does not collapse `..` across one. Two spellings that differ by a
///   symlinked ancestor therefore land on two different locks. Resolving that
///   would mean a `canonicalize` syscall on every append — on the hot path, for
///   a case no caller in this crate produces (every path is built by
///   [`Bundle`] from one configured root). What keeps the missed case from
///   tearing is [`append_line`]'s single `O_APPEND` write, not this lock.
/// * **In-process only.** A second *process* over the same
///   `OPENCOMPANY_DATA_DIR` is outside any in-process lock's reach, and no
///   amount of `static` fixes that. Write atomicity is what keeps that case
///   safe: appends are one `O_APPEND` write and rewrites go through
///   [`write_atomic`]'s temp-file-plus-rename. That bounds the damage to a lost
///   update, never a torn file. Real cross-process exclusion would need file
///   locking, which is a separate change with a separate portability argument.
pub(crate) fn path_lock(path: &Path) -> Arc<TokioMutex<()>> {
    FS_WRITE_LOCKS.get(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// Appends one line (a `\n` is added) to `path`, creating the file if absent.
///
/// The line and its terminating newline are written in a **single** blocking
/// `write_all` inside `spawn_blocking` (via `std::fs::OpenOptions` with
/// `O_APPEND`), so the whole record lands as one atomic OS-level write.
/// Tokio's async `File` buffers internally and can return before the kernel
/// write completes, which makes concurrent-appends tests unreliable; this
/// version always waits for the write syscall to finish before returning.
pub(crate) async fn append_line(path: &Path, line: &str) -> Result<()> {
    let owned_path = path.to_path_buf();
    let mut record = String::with_capacity(line.len() + 1);
    record.push_str(line);
    record.push('\n');
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&owned_path)
            .map_err(|e| io_err(&owned_path, e))?;
        file.write_all(record.as_bytes())
            .map_err(|e| io_err(&owned_path, e))?;
        Ok::<_, OpenCompanyError>(())
    })
    .await
    .map_err(|e| OpenCompanyError::Store(format!("spawn_blocking failed: {e}")))?
}

/// Reads a file to a string, returning an empty string if it does not exist.
pub(crate) async fn read_optional(path: &Path) -> Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => Ok(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(io_err(path, e)),
    }
}

/// Parses every non-empty JSONL line of `path` into `T`, skipping absent files.
///
/// **Strict**: one unparseable line fails the whole read. That is the right
/// default for everything that still uses it, and it is a decision rather than
/// an oversight (issue #387):
///
/// * **Read-modify-write callers must stay strict.** `evict` and `mark_read`
///   read a file here and then [`write_atomic`] it back. A tolerant read would
///   drop the damaged line from the in-memory vector, and the rewrite would then
///   erase it from disk — turning recoverable damage into permanent deletion,
///   silently. See [`read_jsonl_lenient`] for the same boundary stated from the
///   other side.
/// * **Request-time read-only callers stay strict too.** A failed inbox or
///   context read surfaces as one failed request the operator can retry and
///   report; it does not cost a company its boot. Making those tolerant is a
///   separate judgement about each surface's error budget, and it is not made
///   here.
///
/// Only the *boot* path — the ledger read in [`FsCompanyStore::load`] — is
/// tolerant, because there the failure is not one request but the whole
/// company, and the console that would repair the file is behind the boot.
pub(crate) async fn read_jsonl<T>(path: &Path) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let contents = read_optional(path).await?;
    let mut out = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

/// A JSONL line [`read_jsonl_lenient`] could not parse: quarantined in place,
/// never deleted.
///
/// Carries where the line is, how big it is, and what the parser rejected —
/// and **never the line's contents**. A ledger memo is free text a person or an
/// agent wrote; echoing it into a log to explain a parse failure would put
/// arbitrary tenant prose into the container log. The 1-based line number and
/// the byte length are enough for an operator to find the line by hand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkippedLine {
    /// The line's 1-based number in the file.
    pub(crate) line: usize,
    /// The line's length in bytes, as it sits on disk.
    pub(crate) bytes: usize,
    /// What the parse rejected. `serde_json`'s message is positional
    /// ("EOF while parsing a string at line 1 column 74") and quotes no input.
    pub(crate) message: String,
}

/// Parses every non-empty JSONL line of `path` into `T`, **skipping** the lines
/// that will not parse and reporting them rather than failing the read.
///
/// Returns the values that did parse, oldest first, plus one [`SkippedLine`]
/// per line that did not. An absent file is an empty read, as in [`read_jsonl`].
///
/// Bytes are read and decoded per line, not through `read_to_string`. A torn
/// write can split a multi-byte codepoint, and a whole-file UTF-8 decode fails
/// on that one bad byte — losing the entire file to damage confined to a single
/// line. Decoding lossily per line keeps the damage where it is. This mirrors
/// [`RuntimeJournal::load`](crate::runtime::journal::RuntimeJournal::load),
/// which reached the same conclusion for the journal (issue #386).
///
/// # Tolerance boundary — do not widen it
///
/// **A read-modify-write consumer must never call this.** Skipping is only safe
/// because the bytes stay on disk: the caller drops the line from its in-memory
/// view and a person can still repair the file. A caller that reads here and
/// then rewrites the file atomically would write back exactly the lines that
/// parsed, deleting the damaged one for good — converting a recoverable fault
/// into silent, permanent data loss, which is strictly worse than the failed
/// boot this function exists to prevent. Rewriters ([`FsMemoryStore::evict`],
/// [`FsInboxStore::mark_read`]) stay on strict [`read_jsonl`], where a damaged
/// line aborts the rewrite instead of laundering it.
///
/// The one caller today is the ledger read in [`FsCompanyStore::load`]. A ledger
/// entry is descriptive accounting: dropping one from a boot-time view
/// misreports spend until the file is repaired, which is recoverable and
/// visible. It is not an at-most-once safety key — the journal's
/// `EffectExecuted` records are, and they are handled separately and more
/// carefully for exactly that reason.
pub(crate) async fn read_jsonl_lenient<T>(path: &Path) -> Result<(Vec<T>, Vec<SkippedLine>)>
where
    T: serde::de::DeserializeOwned,
{
    let contents = match tokio::fs::read(path).await {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(io_err(path, e)),
    };

    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for (index, raw) in contents.split(|b| *b == b'\n').enumerate() {
        // Lossy on purpose: an invalid byte becomes U+FFFD, the line then fails
        // to parse as JSON, and it lands on the same skip-and-report path as any
        // other unreadable line instead of aborting the whole read.
        let line = String::from_utf8_lossy(raw);
        let line = line.as_ref();
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(value) => out.push(value),
            Err(error) => skipped.push(SkippedLine {
                line: index + 1,
                // The on-disk length, not the lossily-decoded one: U+FFFD is
                // three bytes and would inflate the count for exactly the lines
                // an operator is trying to locate.
                bytes: raw.len(),
                message: error.to_string(),
            }),
        }
    }
    Ok((out, skipped))
}

/// Atomically writes `contents` to `path` via a temp file + rename.
pub(crate) async fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| io_err(parent, e))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", generate_id()));
    tokio::fs::write(&tmp, contents)
        .await
        .map_err(|e| io_err(&tmp, e))?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| io_err(path, e))?;
    Ok(())
}

/// Bundle metadata persisted alongside the manifest.
#[derive(serde::Serialize, serde::Deserialize)]
struct Meta {
    lifecycle: String,
    /// The operator team overlay (teammates added outside the manifest).
    #[serde(default)]
    overlay_agents: Vec<crate::ports::types::OverlayAgent>,
    /// The operator desk-membership overlay (agents added to desks at runtime).
    #[serde(default)]
    overlay_desk_members: Vec<crate::ports::types::OverlayDeskMember>,
    /// The operator per-desk member-ordering overlay (desk hierarchy).
    #[serde(default)]
    overlay_desk_order: Vec<crate::ports::types::OverlayDeskOrder>,
    /// The operator desk-creation overlay (desks created at runtime).
    #[serde(default)]
    overlay_desks: Vec<crate::ports::types::OverlayDesk>,
    /// The operator workflow-authoring overlay (graphs created at runtime).
    /// Absent on meta files written before runtime workflow bodies persisted
    /// through the store, so `#[serde(default)]` keeps those loading.
    #[serde(default)]
    overlay_workflows: Vec<crate::ports::types::OverlayWorkflow>,
    /// The operator-set per-teammate daily spend caps (issue #343). Absent on
    /// meta files written before the console could write a budget, so
    /// `#[serde(default)]` keeps those loading with the manifest in charge.
    #[serde(default)]
    overlay_budgets: Vec<crate::ports::types::BudgetOverride>,
    /// The source-template provenance stamped at launch. `None` for companies
    /// provisioned from a raw manifest and for legacy meta files written before
    /// provenance existed (the `#[serde(default)]` keeps those loading).
    #[serde(default)]
    template_provenance: Option<crate::ports::types::TemplateProvenance>,
}

impl Default for Meta {
    /// A bundle whose meta file has not been written yet: a running company
    /// with no operator overlays and no recorded provenance. `lifecycle` is
    /// `"running"` rather than `String::default()` — an empty lifecycle is not
    /// a state this runtime has.
    fn default() -> Self {
        Self {
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        }
    }
}

// ---------------------------------------------------------------------------
// CompanyStore
// ---------------------------------------------------------------------------

/// Filesystem [`CompanyStore`]: the manifest as TOML, lifecycle as JSON, and an
/// append-only ledger.
#[derive(Clone)]
pub struct FsCompanyStore {
    root: PathBuf,
}

impl FsCompanyStore {
    /// Creates a store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }
}

#[async_trait]
impl CompanyStore for FsCompanyStore {
    async fn load(&self, id: &CompanyId) -> Result<Option<CompanyRecord>> {
        let bundle = self.bundle(id);
        let toml_path = bundle.company_toml();
        let toml_src = match tokio::fs::read_to_string(&toml_path).await {
            Ok(src) => src,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&toml_path, e)),
        };
        let manifest = toml::from_str(&toml_src)
            .map_err(|e| OpenCompanyError::Store(format!("invalid company.toml: {e}")))?;

        let meta_src = read_optional(&bundle.meta_json()).await?;
        // A bundle with no meta file yet is a running company with no overlays —
        // which is exactly `Meta::default()`. Carrying that through one value
        // rather than a positional tuple keeps adding an overlay collection a
        // one-line change here instead of a four-place one.
        let meta: Meta = if meta_src.trim().is_empty() {
            Meta::default()
        } else {
            serde_json::from_str(&meta_src)?
        };

        // The ledger is read leniently, and this is the only place in the fs
        // backend that is (issue #387). Every other read here is either a
        // rewriter — where skipping would delete the damaged line on write-back
        // — or a request-time reader, where a failure costs one request. This
        // one is neither: it is on the boot path, so a single malformed
        // accounting line used to take the whole company down, and the repair
        // console an operator would use sits behind the boot it killed.
        //
        // The skipped lines stay on disk untouched. Nothing in `load` writes,
        // and `append_ledger` only appends, so the file after a tolerated boot
        // is byte-identical to the file before it.
        let ledger_path = bundle.ledger_jsonl();
        let (ledger, skipped) = read_jsonl_lenient::<LedgerEntry>(&ledger_path).await?;
        if let Some(first) = skipped.first() {
            // `error!`, not `warn!`: the company is running on an incomplete
            // ledger, so its reported spend is wrong until someone repairs the
            // file. Loud once per load, naming the file and the first bad line —
            // never the line's contents, which are operator/agent free text.
            tracing::error!(
                company = %id,
                ledger = %ledger_path.display(),
                skipped = skipped.len(),
                first_line = first.line,
                first_bytes = first.bytes,
                error = %first.message,
                "[store] ledger lines could not be parsed; they were skipped so the company can still boot, and left on disk for repair — reported spend is incomplete until they are fixed"
            );
        }

        Ok(Some(CompanyRecord {
            id: id.clone(),
            manifest,
            ledger,
            lifecycle: meta.lifecycle,
            overlay_agents: meta.overlay_agents,
            overlay_desk_members: meta.overlay_desk_members,
            overlay_desk_order: meta.overlay_desk_order,
            overlay_desks: meta.overlay_desks,
            overlay_workflows: meta.overlay_workflows,
            overlay_budgets: meta.overlay_budgets,
            template_provenance: meta.template_provenance,
        }))
    }

    async fn save(&self, record: &CompanyRecord) -> Result<()> {
        let bundle = self.bundle(&record.id);
        bundle.ensure_dirs().await?;

        let toml_src = toml::to_string(&record.manifest)
            .map_err(|e| OpenCompanyError::Store(format!("cannot serialize manifest: {e}")))?;
        write_atomic(&bundle.company_toml(), &toml_src).await?;

        let meta = Meta {
            lifecycle: record.lifecycle.clone(),
            overlay_agents: record.overlay_agents.clone(),
            overlay_desk_members: record.overlay_desk_members.clone(),
            overlay_desk_order: record.overlay_desk_order.clone(),
            overlay_desks: record.overlay_desks.clone(),
            overlay_workflows: record.overlay_workflows.clone(),
            overlay_budgets: record.overlay_budgets.clone(),
            template_provenance: record.template_provenance.clone(),
        };
        write_atomic(&bundle.meta_json(), &serde_json::to_string(&meta)?).await?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<CompanySummary>> {
        let companies_dir = self.root.join("companies");
        let mut entries = match tokio::fs::read_dir(&companies_dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&companies_dir, e)),
        };

        let mut out = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| io_err(&companies_dir, e))?
        {
            let dir = entry.path();
            let toml_path = dir.join("company.toml");
            let Ok(toml_src) = tokio::fs::read_to_string(&toml_path).await else {
                continue;
            };
            let manifest: crate::company::CompanyManifest = match toml::from_str(&toml_src) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let meta_src = read_optional(&dir.join("meta.json")).await?;
            let lifecycle = if meta_src.trim().is_empty() {
                "running".to_string()
            } else {
                serde_json::from_str::<Meta>(&meta_src)?.lifecycle
            };
            let id = entry.file_name().to_string_lossy().into_owned();
            out.push(CompanySummary {
                id: CompanyId::new(id),
                name: manifest.company.name,
                lifecycle,
            });
        }
        out.sort_by(|a, b| a.id.as_ref().cmp(b.id.as_ref()));
        Ok(out)
    }

    async fn append_ledger(&self, id: &CompanyId, entry: LedgerEntry) -> Result<()> {
        let bundle = self.bundle(id);
        bundle.ensure_dirs().await?;
        let path = bundle.ledger_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        append_line(&path, &serde_json::to_string(&entry)?).await
    }
}

// ---------------------------------------------------------------------------
// EventLog
// ---------------------------------------------------------------------------

/// Filesystem [`EventLog`]: append-only JSONL with a live broadcast fan-out for
/// subscribers.
#[derive(Clone)]
pub struct FsEventLog {
    root: PathBuf,
    /// Live subscribers, keyed by company.
    ///
    /// **Deliberately per-instance, and not part of issue #388's fix.** The
    /// write locks moved to a process-wide registry because two instances over
    /// one file must exclude each other. This map is the opposite kind of state:
    /// it is a fan-out to the subscribers *this* instance handed streams to, and
    /// nothing about durability or ordering depends on two instances sharing it.
    /// A subscriber that misses an event because it subscribed through the other
    /// instance is a delivery gap in a best-effort live feed, recoverable by
    /// [`read_from`](EventLog::read_from) against the durable log — not the
    /// silent write loss the lock change fixes. Making it global would give one
    /// company's stream process-global lifetime for a much weaker reason.
    senders: Arc<StdMutex<HashMap<CompanyId, broadcast::Sender<StoredEvent>>>>,
}

impl FsEventLog {
    /// Creates an event log rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            senders: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }

    fn sender_for(&self, id: &CompanyId) -> broadcast::Sender<StoredEvent> {
        let mut map = self.senders.lock().expect("sender map poisoned");
        map.entry(id.clone())
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }
}

#[async_trait]
impl EventLog for FsEventLog {
    async fn append(&self, id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
        let bundle = self.bundle(id);
        bundle.ensure_dirs().await?;
        let path = bundle.events_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;

        // The next sequence is the current line count; held under the lock so
        // concurrent appends never collide on a seq.
        let existing = read_optional(&path).await?;
        let seq = existing.lines().filter(|l| !l.trim().is_empty()).count() as u64;

        let stored = StoredEvent {
            seq: EventSeq::new(seq),
            company: id.clone(),
            event,
            at_millis: now_millis(),
        };
        append_line(&path, &serde_json::to_string(&stored)?).await?;

        // Best-effort fan-out; a send error only means there are no live
        // subscribers, which is fine.
        let _ = self.sender_for(id).send(stored);
        Ok(EventSeq::new(seq))
    }

    async fn read_from(
        &self,
        id: &CompanyId,
        seq: EventSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let all = read_jsonl::<StoredEvent>(&self.bundle(id).events_jsonl()).await?;
        Ok(all
            .into_iter()
            .filter(|ev| ev.seq >= seq)
            .take(limit)
            .collect())
    }

    fn subscribe(&self, id: &CompanyId) -> BoxStream<'static, StoredEvent> {
        let rx = self.sender_for(id).subscribe();
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(event) => return Some((event, rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        Box::pin(stream)
    }
}

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

/// Filesystem [`MemoryStore`]: compressed traces and task results as JSONL.
#[derive(Clone)]
pub struct FsMemoryStore {
    root: PathBuf,
}

impl FsMemoryStore {
    /// Creates a memory store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }
}

#[async_trait]
impl MemoryStore for FsMemoryStore {
    async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()> {
        let bundle = self.bundle(id);
        bundle.ensure_dirs().await?;
        let path = bundle.traces_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        append_line(&path, &serde_json::to_string(&trace)?).await
    }

    async fn recent_traces(&self, id: &CompanyId, limit: usize) -> Result<Vec<CompressedTrace>> {
        let mut all = read_jsonl::<CompressedTrace>(&self.bundle(id).traces_jsonl()).await?;
        if all.len() > limit {
            all.drain(0..all.len() - limit);
        }
        Ok(all)
    }

    async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()> {
        let bundle = self.bundle(id);
        bundle.ensure_dirs().await?;
        let path = bundle.tasks_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        append_line(&path, &serde_json::to_string(&result)?).await
    }

    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64> {
        let bundle = self.bundle(id);
        let path = bundle.traces_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;

        let all = read_jsonl::<CompressedTrace>(&path).await?;
        let before = all.len();
        let kept: Vec<CompressedTrace> = match policy {
            EvictionPolicy::KeepRecent { n } => {
                if all.len() > n {
                    all[all.len() - n..].to_vec()
                } else {
                    all
                }
            }
            EvictionPolicy::OlderThan { before_millis } => all
                .into_iter()
                .filter(|t| t.at_millis >= before_millis)
                .collect(),
        };
        let removed = (before - kept.len()) as u64;
        if removed > 0 {
            let body: String = kept
                .iter()
                .map(|t| serde_json::to_string(t).map(|s| s + "\n"))
                .collect::<std::result::Result<String, _>>()?;
            write_atomic(&path, &body).await?;
        }
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// ContextStore
// ---------------------------------------------------------------------------

/// A context index line pairing an address with its label, length, and the
/// epoch-millis it was first stored.
///
/// `stored_at_millis` defaults to `0` so index lines written before the field
/// existed still deserialize — they simply report an unknown store time.
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexEntry {
    addr: String,
    label: String,
    len: usize,
    #[serde(default)]
    stored_at_millis: u64,
}

/// Filesystem [`ContextStore`]: content-addressed blobs plus a JSONL index.
///
/// Phase 1 uses a non-cryptographic [`DefaultHasher`] content id; a real
/// content hash (sha-256) is a documented follow-up.
#[derive(Clone)]
pub struct FsContextStore {
    root: PathBuf,
}

impl FsContextStore {
    /// Creates a context store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }
}

#[async_trait]
impl ContextStore for FsContextStore {
    async fn put(&self, id: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr> {
        let bundle = self.bundle(id);
        bundle.ensure_dirs().await?;
        let addr = content_address(&chunk.body);

        let blob_path = bundle.context_blob(&addr);
        tokio::fs::write(&blob_path, &chunk.body)
            .await
            .map_err(|e| io_err(&blob_path, e))?;

        let index_path = bundle.context_index_jsonl();
        let lock = path_lock(&index_path);
        let _guard = lock.lock().await;
        let entry = IndexEntry {
            addr: addr.clone(),
            label: chunk.label,
            len: chunk.body.len(),
            stored_at_millis: now_millis(),
        };
        append_line(&index_path, &serde_json::to_string(&entry)?).await?;
        Ok(ChunkAddr::new(addr))
    }

    async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>> {
        let index = read_jsonl::<IndexEntry>(&self.bundle(id).context_index_jsonl()).await?;
        Ok(index
            .into_iter()
            .filter(|e| e.label.starts_with(prefix))
            .map(|e| ChunkMeta {
                addr: ChunkAddr::new(e.addr),
                label: e.label,
                len: e.len,
                stored_at_millis: e.stored_at_millis,
            })
            .collect())
    }

    async fn peek(
        &self,
        id: &CompanyId,
        addr: &ChunkAddr,
        range: Option<Range<usize>>,
    ) -> Result<String> {
        let path = self.bundle(id).context_blob(addr.as_ref());
        let body = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| io_err(&path, e))?;
        match range {
            None => Ok(body),
            Some(r) => {
                let start = r.start.min(body.len());
                let end = r.end.min(body.len());
                if start >= end {
                    return Ok(String::new());
                }
                Ok(body[start..end].to_string())
            }
        }
    }

    async fn search(&self, id: &CompanyId, query: &str, limit: usize) -> Result<Vec<ChunkHit>> {
        let bundle = self.bundle(id);
        let index = read_jsonl::<IndexEntry>(&bundle.context_index_jsonl()).await?;
        let mut hits = Vec::new();
        for entry in index {
            if hits.len() >= limit {
                break;
            }
            let blob_path = bundle.context_blob(&entry.addr);
            let Ok(body) = tokio::fs::read_to_string(&blob_path).await else {
                continue;
            };
            if let Some(pos) = body.find(query) {
                let start = pos.saturating_sub(24);
                let end = (pos + query.len() + 24).min(body.len());
                hits.push(ChunkHit {
                    addr: ChunkAddr::new(entry.addr),
                    snippet: body[start..end].to_string(),
                    score: 1.0,
                });
            }
        }
        Ok(hits)
    }
}

// ---------------------------------------------------------------------------
// SecretStore
// ---------------------------------------------------------------------------

/// Filesystem [`SecretStore`]: one file per key under the company's isolated
/// `secrets/` directory.
///
/// Isolation is structural: a secret path is always under the requesting
/// company's bundle, so company B cannot address company A's directory.
/// Encryption-at-rest is a documented follow-up; Phase 1 stores plaintext with
/// `0700` directory permissions on unix.
#[derive(Clone)]
pub struct FsSecretStore {
    root: PathBuf,
}

impl FsSecretStore {
    /// Creates a secret store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }
}

#[async_trait]
impl SecretStore for FsSecretStore {
    async fn get(&self, company: &CompanyId, key: &str) -> Result<Option<SecretValue>> {
        let path = self.bundle(company).secret(key);
        match tokio::fs::read_to_string(&path).await {
            Ok(value) => Ok(Some(SecretValue(value))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(&path, e)),
        }
    }

    async fn set(&self, company: &CompanyId, key: &str, value: SecretValue) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.secret(key);
        tokio::fs::write(&path, value.expose())
            .await
            .map_err(|e| io_err(&path, e))
    }
}

// ---------------------------------------------------------------------------
// InboxStore
// ---------------------------------------------------------------------------

/// Filesystem [`InboxStore`]: one append-only `inbox.jsonl` per company holding
/// every inbox's mail interleaved. Reads filter by inbox in memory; the volumes
/// (a teammate's mail) stay well within a single-file scan.
#[derive(Clone)]
pub struct FsInboxStore {
    root: PathBuf,
}

impl FsInboxStore {
    /// Creates an inbox store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }
}

impl FsInboxStore {
    /// Loads the `key` → [`InboxMeta`] map, defaulting to empty.
    async fn load_meta(&self, company: &CompanyId) -> Result<HashMap<String, InboxMeta>> {
        let path = self.bundle(company).inbox_meta_json();
        let contents = read_optional(&path).await?;
        if contents.trim().is_empty() {
            return Ok(HashMap::new());
        }
        Ok(serde_json::from_str(&contents)?)
    }
}

#[async_trait]
impl InboxStore for FsInboxStore {
    async fn inboxes(&self, company: &CompanyId) -> Result<Vec<InboxMeta>> {
        let meta = self.load_meta(company).await?;
        let all = read_jsonl::<EmailRecord>(&self.bundle(company).inbox_jsonl()).await?;
        // Start from explicit metadata, then synthesize a default enabled meta
        // for any inbox that only has messages.
        let mut out: HashMap<String, InboxMeta> = meta;
        for record in all {
            out.entry(record.inbox.clone())
                .or_insert_with(|| InboxMeta {
                    key: record.inbox.clone(),
                    name: record.inbox.clone(),
                    address: String::new(),
                    enabled: true,
                });
        }
        let mut list: Vec<InboxMeta> = out.into_values().collect();
        list.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(list)
    }

    async fn set_enabled(&self, company: &CompanyId, key: &str, meta: &InboxMeta) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.inbox_meta_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut map = self.load_meta(company).await?;
        map.insert(key.to_string(), meta.clone());
        write_atomic(&path, &serde_json::to_string(&map)?).await
    }

    async fn messages(
        &self,
        company: &CompanyId,
        key: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<EmailRecord>> {
        let all = read_jsonl::<EmailRecord>(&self.bundle(company).inbox_jsonl()).await?;
        Ok(all
            .into_iter()
            .filter(|r| r.inbox == key)
            .skip(offset)
            .take(limit)
            .collect())
    }

    async fn append(&self, company: &CompanyId, msg: &EmailRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.inbox_jsonl();
        let line = serde_json::to_string(msg)?;
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        append_line(&path, &line).await
    }

    async fn mark_read(
        &self,
        company: &CompanyId,
        key: &str,
        ids: Option<&[String]>,
    ) -> Result<u64> {
        let path = self.bundle(company).inbox_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut all = read_jsonl::<EmailRecord>(&path).await?;
        for record in all.iter_mut() {
            if record.inbox != key {
                continue;
            }
            let hit = match ids {
                Some(ids) => ids.iter().any(|id| id == &record.id),
                None => true,
            };
            if hit {
                record.read = true;
            }
        }
        let unread = all.iter().filter(|r| r.inbox == key && !r.read).count() as u64;
        let body: String = all
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n");
        let body = if body.is_empty() {
            String::new()
        } else {
            format!("{body}\n")
        };
        write_atomic(&path, &body).await?;
        Ok(unread)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::store::conformance;
    use futures::StreamExt;

    fn tmp_root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-test-")
            .tempdir()
            .expect("tempdir")
    }

    #[tokio::test]
    async fn concurrent_appends_stay_one_record_per_line() {
        // Many tasks appending to the same JSONL file must never interleave a
        // record with another's newline (the `{a}{b}\n\n` corruption that
        // `read_jsonl` reports as a "trailing characters" parse error). The
        // single-write `append_line` makes each record one atomic O_APPEND
        // write, so this holds deterministically.
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let path = root.join("log.jsonl");

        const N: u64 = 64;
        let mut set = tokio::task::JoinSet::new();
        for i in 0..N {
            let path = path.clone();
            set.spawn(async move {
                let line = serde_json::to_string(&serde_json::json!({ "i": i })).unwrap();
                append_line(&path, &line).await.unwrap();
            });
        }
        while let Some(res) = set.join_next().await {
            res.unwrap();
        }

        // Every record parses (no merged lines) and all N are present once.
        let rows: Vec<serde_json::Value> = read_jsonl(&path).await.expect("no corrupt lines");
        assert_eq!(rows.len() as u64, N, "every append is its own line");
        let mut seen: Vec<u64> = rows.iter().map(|r| r["i"].as_u64().unwrap()).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..N).collect::<Vec<_>>(), "all records intact");
    }

    // The fs backend runs the identical port-conformance suite the sqlite
    // backend runs under `--features sqlite`. Each test gets a fresh root so the
    // stores start empty.
    #[tokio::test]
    async fn conformance_isolation_by_company() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_isolation_by_company(
            Arc::new(FsCompanyStore::new(&root)),
            Arc::new(FsEventLog::new(&root)),
            Arc::new(FsMemoryStore::new(&root)),
            Arc::new(FsContextStore::new(&root)),
        )
        .await;
    }

    #[tokio::test]
    async fn conformance_append_only_event_and_ledger() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_append_only_event_and_ledger(
            Arc::new(FsCompanyStore::new(&root)),
            Arc::new(FsEventLog::new(&root)),
        )
        .await;
    }

    #[tokio::test]
    async fn conformance_monotonic_event_seq() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_monotonic_event_seq(Arc::new(FsEventLog::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_inbox_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_inbox_store(Arc::new(FsInboxStore::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_context_chunk_stamps() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_context_chunk_stamps(Arc::new(FsContextStore::new(&root))).await;
    }

    /// Two event logs over one data root must not hand out the same sequence
    /// number (issue #388).
    ///
    /// `EventLog::append` computes the next `seq` by counting the lines already
    /// in the file, then appends. That read-then-append is only atomic under a
    /// lock, and the lock used to be a **field** on `FsEventLog` — so two
    /// instances over one bundle serialised against nothing, both read the same
    /// count, and both wrote the same `seq`. A duplicate sequence number breaks
    /// every consumer that treats it as an identity: `read_from`'s `seq >=`
    /// cursor silently replays, and the console's resume-from-seq skips.
    ///
    /// Nothing stops a second instance being constructed — `FsEventLog::new`
    /// takes a root and is called wherever one is needed — so this is reachable
    /// without any exotic setup, which is exactly what makes it worth a lock in
    /// the registry rather than a convention.
    #[tokio::test]
    async fn two_event_logs_over_one_root_never_reuse_a_sequence() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        // Two independently-constructed logs over the same data root — the shape
        // a second runtime, a maintenance task, or an export job produces.
        let first = Arc::new(FsEventLog::new(&root));
        let second = Arc::new(FsEventLog::new(&root));
        let id = CompanyId::new("acme");

        const N: u64 = 32;
        let mut set = tokio::task::JoinSet::new();
        for i in 0..N {
            let log = if i % 2 == 0 {
                first.clone()
            } else {
                second.clone()
            };
            let id = id.clone();
            set.spawn(async move {
                log.append(
                    &id,
                    CompanyEvent::OperatorMessage {
                        parent: None,
                        text: format!("event {i}"),
                        by: None,
                        chat: None,
                    },
                )
                .await
                .expect("append succeeds")
            });
        }
        let mut handed_out = Vec::new();
        while let Some(res) = set.join_next().await {
            handed_out.push(res.expect("task joins").value());
        }

        handed_out.sort_unstable();
        assert_eq!(
            handed_out,
            (0..N).collect::<Vec<_>>(),
            "the sequences handed to callers must be unique and dense — a repeat \
             means two instances read the same line count before either appended"
        );

        // And the same must hold for what actually landed on disk.
        let stored = first.read_from(&id, EventSeq::new(0), 1024).await.unwrap();
        assert_eq!(stored.len() as u64, N, "every append is on disk");
        let mut persisted: Vec<u64> = stored.iter().map(|e| e.seq.value()).collect();
        persisted.sort_unstable();
        assert_eq!(
            persisted,
            (0..N).collect::<Vec<_>>(),
            "the persisted sequences must be unique and dense too"
        );
    }

    /// The fs backend's migration path: index lines written before
    /// `stored_at_millis` existed carry no such field, and must still
    /// deserialize — reporting an unknown (`0`) store time rather than failing
    /// the read and blanking the whole Brain list.
    #[test]
    fn legacy_context_index_line_without_a_stamp_still_parses() {
        let legacy = r#"{"addr":"abc123","label":"agent/ceo","len":24}"#;
        let entry: IndexEntry = serde_json::from_str(legacy).expect("legacy index line parses");
        assert_eq!(entry.addr, "abc123");
        assert_eq!(entry.label, "agent/ceo");
        assert_eq!(entry.len, 24);
        assert_eq!(entry.stored_at_millis, 0);
    }

    #[tokio::test]
    async fn conformance_export_totality() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_export_totality(
            Arc::new(FsCompanyStore::new(&root)),
            Arc::new(FsEventLog::new(&root)),
            Arc::new(FsMemoryStore::new(&root)),
            Arc::new(FsContextStore::new(&root)),
        )
        .await;
    }

    fn sample_manifest() -> crate::company::CompanyManifest {
        let toml_src = r#"
            [company]
            name = "Acme"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "supervised"
        "#;
        toml::from_str(toml_src).expect("parse manifest")
    }

    #[tokio::test]
    async fn company_store_saves_and_loads() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let record = CompanyRecord {
            id: id.clone(),
            manifest: sample_manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        };
        store.save(&record).await.unwrap();

        let loaded = store.load(&id).await.unwrap().expect("record exists");
        assert_eq!(loaded.manifest.company.name, "Acme");
        assert_eq!(loaded.lifecycle, "running");
        assert_eq!(loaded.manifest.agents.len(), 1);

        let summaries = store.list().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "Acme");

        assert!(
            store
                .load(&CompanyId::new("ghost"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn append_ledger_grows_without_rewrite() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: sample_manifest(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();

        for i in 0..3 {
            store
                .append_ledger(
                    &id,
                    LedgerEntry {
                        at_millis: now_millis(),
                        kind: "inference.spend".to_string(),
                        amount_usd: i as f64,
                        memo: format!("entry {i}"),
                    },
                )
                .await
                .unwrap();
        }
        let loaded = store.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded.ledger.len(), 3);
        assert_eq!(loaded.ledger[2].memo, "entry 2");
    }

    /// A company with a damaged ledger still boots, and the damage is
    /// quarantined rather than deleted (issue #387).
    ///
    /// Before this, `read_jsonl` parsed inside its loop, so the first bad line
    /// returned `Err` from `FsCompanyStore::load`, the builder propagated it,
    /// and one torn accounting line made the company unbootable — with the
    /// console that would repair it sitting behind the boot it killed.
    ///
    /// Four lines, two of them damaged in the two ways a torn write actually
    /// produces: JSON that stops mid-value, and a byte sequence that is not
    /// UTF-8 at all. The second is the reason this reads bytes rather than a
    /// `String`: a whole-file decode would fail on that one byte and lose all
    /// four lines to damage confined to one.
    #[tokio::test]
    async fn a_damaged_ledger_line_is_skipped_and_left_on_disk() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: sample_manifest(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();

        // The memo text the report must never echo. Distinctive enough that a
        // substring search cannot pass by accident.
        const MEMO_TWO: &str = "acquire-northwind-holdings";
        const MEMO_THREE: &str = "settle-quarterly-invoice";

        let mut bytes: Vec<u8> = Vec::new();
        // 1. Intact.
        bytes.extend_from_slice(
            br#"{"at_millis":1,"kind":"inference.spend","amount_usd":1.0,"memo":"first entry"}"#,
        );
        bytes.push(b'\n');
        // 2. A torn write: the JSON stops in the middle of the memo string.
        bytes.extend_from_slice(
            format!(
                r#"{{"at_millis":2,"kind":"inference.spend","amount_usd":2.0,"memo":"{MEMO_TWO}"#
            )
            .as_bytes(),
        );
        bytes.push(b'\n');
        // 3. Invalid UTF-8 in a structural position, so the lossy U+FFFD lands
        //    where a key is expected and the line cannot parse. (Damage *inside*
        //    a string would decode to a valid — if mangled — record, which is
        //    the recoverable case and deliberately not skipped.)
        bytes.extend_from_slice(br#"{"at_millis":3,"#);
        bytes.push(0xFF);
        bytes.extend_from_slice(
            format!(r#""kind":"inference.spend","amount_usd":3.0,"memo":"{MEMO_THREE}"}}"#)
                .as_bytes(),
        );
        bytes.push(b'\n');
        // 4. Intact.
        bytes.extend_from_slice(
            br#"{"at_millis":4,"kind":"inference.spend","amount_usd":4.0,"memo":"fourth entry"}"#,
        );
        bytes.push(b'\n');

        let path = Bundle::new(root.clone(), &id).ledger_jsonl();
        tokio::fs::write(&path, &bytes).await.unwrap();

        // The company boots, carrying the entries that survived.
        let loaded = store
            .load(&id)
            .await
            .expect("a damaged ledger line must not fail the load")
            .expect("the company exists");
        assert_eq!(
            loaded.ledger.len(),
            2,
            "the two intact entries load; the two damaged ones are skipped"
        );
        assert_eq!(loaded.ledger[0].memo, "first entry");
        assert_eq!(loaded.ledger[1].memo, "fourth entry");

        // The report locates the damage without quoting it.
        let (entries, skipped) = read_jsonl_lenient::<LedgerEntry>(&path).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            skipped.iter().map(|s| s.line).collect::<Vec<_>>(),
            vec![2, 3],
            "the report names the 1-based line numbers of the damaged lines"
        );
        for entry in &skipped {
            assert!(
                entry.bytes > 0,
                "the report carries the on-disk line length"
            );
            assert!(
                !entry.message.is_empty(),
                "the report says what was rejected"
            );
            for memo in [MEMO_TWO, MEMO_THREE] {
                assert!(
                    !entry.message.contains(memo),
                    "a memo is free text and must never reach the report: {:?}",
                    entry.message
                );
            }
        }

        // Quarantine, not repair: the file is untouched, so an operator can
        // still recover the damaged lines by hand.
        let after = tokio::fs::read(&path).await.unwrap();
        assert_eq!(
            after, bytes,
            "loading must not rewrite the ledger — skipping a line the reader \
             could not parse must never become deleting it"
        );
    }

    #[tokio::test]
    async fn event_log_assigns_monotonic_seqs_and_resumes() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let log = FsEventLog::new(&root);
        let id = CompanyId::new("acme");

        let s0 = log
            .append(
                &id,
                CompanyEvent::OperatorMessage {
                    parent: None,
                    text: "a".into(),
                    by: None,
                    chat: None,
                },
            )
            .await
            .unwrap();
        let s1 = log
            .append(
                &id,
                CompanyEvent::OperatorMessage {
                    parent: None,
                    text: "b".into(),
                    by: None,
                    chat: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(s0, EventSeq::new(0));
        assert_eq!(s1, EventSeq::new(1));

        let from_start = log.read_from(&id, EventSeq::new(0), 10).await.unwrap();
        assert_eq!(from_start.len(), 2);
        let from_one = log.read_from(&id, EventSeq::new(1), 10).await.unwrap();
        assert_eq!(from_one.len(), 1);
        assert_eq!(from_one[0].seq, EventSeq::new(1));
    }

    #[tokio::test]
    async fn event_log_subscribe_delivers_new_event() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let log = FsEventLog::new(&root);
        let id = CompanyId::new("acme");
        let mut stream = log.subscribe(&id);

        log.append(
            &id,
            CompanyEvent::OperatorMessage {
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None,
            },
        )
        .await
        .unwrap();
        let received = stream.next().await.expect("event delivered");
        assert_eq!(
            received.event,
            CompanyEvent::OperatorMessage {
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None
            }
        );
    }

    #[tokio::test]
    async fn memory_store_traces_tail_and_evict() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let mem = FsMemoryStore::new(&root);
        let id = CompanyId::new("acme");
        for i in 0..5 {
            mem.save_trace(&id, CompressedTrace::now(format!("c{i}"), format!("s{i}")))
                .await
                .unwrap();
        }
        let recent = mem.recent_traces(&id, 2).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[1].cycle_id, "c4");

        let removed = mem
            .evict(&id, EvictionPolicy::KeepRecent { n: 1 })
            .await
            .unwrap();
        assert_eq!(removed, 4);
        assert_eq!(mem.recent_traces(&id, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn context_store_put_peek_search() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let ctx = FsContextStore::new(&root);
        let id = CompanyId::new("acme");
        let addr = ctx
            .put(
                &id,
                ContextChunk {
                    label: "notes/intro".into(),
                    body: "the quick brown fox jumps".into(),
                },
            )
            .await
            .unwrap();

        let full = ctx.peek(&id, &addr, None).await.unwrap();
        assert_eq!(full, "the quick brown fox jumps");
        let ranged = ctx.peek(&id, &addr, Some(4..9)).await.unwrap();
        assert_eq!(ranged, "quick");

        let listed = ctx.list(&id, "notes/").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].label, "notes/intro");

        let hits = ctx.search(&id, "brown", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("brown"));
    }

    #[tokio::test]
    async fn secret_store_isolates_companies() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let secrets = FsSecretStore::new(&root);
        let a = CompanyId::new("company-a");
        let b = CompanyId::new("company-b");

        secrets
            .set(&a, "github_token", SecretValue("ghp_secret".into()))
            .await
            .unwrap();
        assert_eq!(
            secrets.get(&a, "github_token").await.unwrap(),
            Some(SecretValue("ghp_secret".into()))
        );
        // Company B cannot see company A's secret.
        assert_eq!(secrets.get(&b, "github_token").await.unwrap(), None);
    }
}
