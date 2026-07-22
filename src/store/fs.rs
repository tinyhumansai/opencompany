//! Filesystem-backed implementations of the persistence ports.
//!
//! All state lives in per-company [`Bundle`] directories (TOML for the
//! manifest, JSONL for append-only logs, content-addressed blobs for context).
//! Appends are the hot path and never rewrite the whole file; per-path
//! `tokio::sync::Mutex` locks serialize concurrent writers within a process.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::io::AsyncWriteExt;
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

/// Appends one line (a `\n` is added) to `path`, creating the file if absent.
pub(crate) async fn append_line(path: &Path, line: &str) -> Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| io_err(path, e))?;
    file.write_all(line.as_bytes())
        .await
        .map_err(|e| io_err(path, e))?;
    file.write_all(b"\n").await.map_err(|e| io_err(path, e))?;
    Ok(())
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
}

// ---------------------------------------------------------------------------
// CompanyStore
// ---------------------------------------------------------------------------

/// Filesystem [`CompanyStore`]: the manifest as TOML, lifecycle as JSON, and an
/// append-only ledger.
#[derive(Clone)]
pub struct FsCompanyStore {
    root: PathBuf,
    locks: PathLocks,
}

impl FsCompanyStore {
    /// Creates a store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: PathLocks::default(),
        }
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
        let (lifecycle, overlay_agents) = if meta_src.trim().is_empty() {
            ("running".to_string(), Vec::new())
        } else {
            let meta: Meta = serde_json::from_str(&meta_src)?;
            (meta.lifecycle, meta.overlay_agents)
        };

        let ledger = read_jsonl::<LedgerEntry>(&bundle.ledger_jsonl()).await?;

        Ok(Some(CompanyRecord {
            id: id.clone(),
            manifest,
            ledger,
            lifecycle,
            overlay_agents,
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
        let lock = self.locks.get(&path);
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
    locks: PathLocks,
    senders: Arc<StdMutex<HashMap<CompanyId, broadcast::Sender<StoredEvent>>>>,
}

impl FsEventLog {
    /// Creates an event log rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: PathLocks::default(),
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
        let lock = self.locks.get(&path);
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
    locks: PathLocks,
}

impl FsMemoryStore {
    /// Creates a memory store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: PathLocks::default(),
        }
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
        let lock = self.locks.get(&path);
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
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        append_line(&path, &serde_json::to_string(&result)?).await
    }

    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64> {
        let bundle = self.bundle(id);
        let path = bundle.traces_jsonl();
        let lock = self.locks.get(&path);
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

/// A context index line pairing an address with its label and length.
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexEntry {
    addr: String,
    label: String,
    len: usize,
}

/// Filesystem [`ContextStore`]: content-addressed blobs plus a JSONL index.
///
/// Phase 1 uses a non-cryptographic [`DefaultHasher`] content id; a real
/// content hash (sha-256) is a documented follow-up.
#[derive(Clone)]
pub struct FsContextStore {
    root: PathBuf,
    locks: PathLocks,
}

impl FsContextStore {
    /// Creates a context store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: PathLocks::default(),
        }
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
        let lock = self.locks.get(&index_path);
        let _guard = lock.lock().await;
        let entry = IndexEntry {
            addr: addr.clone(),
            label: chunk.label,
            len: chunk.body.len(),
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
    locks: PathLocks,
}

impl FsInboxStore {
    /// Creates an inbox store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: PathLocks::default(),
        }
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
        let lock = self.locks.get(&path);
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
        let lock = self.locks.get(&path);
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
        let lock = self.locks.get(&path);
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

    fn tmp_root() -> PathBuf {
        std::env::temp_dir().join(format!("opencompany-test-{}", generate_id()))
    }

    // The fs backend runs the identical port-conformance suite the sqlite
    // backend runs under `--features sqlite`. Each test gets a fresh root so the
    // stores start empty.
    #[tokio::test]
    async fn conformance_isolation_by_company() {
        let root = tmp_root();
        conformance::assert_isolation_by_company(
            Arc::new(FsCompanyStore::new(&root)),
            Arc::new(FsEventLog::new(&root)),
            Arc::new(FsMemoryStore::new(&root)),
            Arc::new(FsContextStore::new(&root)),
        )
        .await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_append_only_event_and_ledger() {
        let root = tmp_root();
        conformance::assert_append_only_event_and_ledger(
            Arc::new(FsCompanyStore::new(&root)),
            Arc::new(FsEventLog::new(&root)),
        )
        .await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_monotonic_event_seq() {
        let root = tmp_root();
        conformance::assert_monotonic_event_seq(Arc::new(FsEventLog::new(&root))).await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_inbox_store() {
        let root = tmp_root();
        conformance::assert_inbox_store(Arc::new(FsInboxStore::new(&root))).await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_export_totality() {
        let root = tmp_root();
        conformance::assert_export_totality(
            Arc::new(FsCompanyStore::new(&root)),
            Arc::new(FsEventLog::new(&root)),
            Arc::new(FsMemoryStore::new(&root)),
            Arc::new(FsContextStore::new(&root)),
        )
        .await;
        tokio::fs::remove_dir_all(&root).await.ok();
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
        let root = tmp_root();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let record = CompanyRecord {
            id: id.clone(),
            manifest: sample_manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
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
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn append_ledger_grows_without_rewrite() {
        let root = tmp_root();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: sample_manifest(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
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
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn event_log_assigns_monotonic_seqs_and_resumes() {
        let root = tmp_root();
        let log = FsEventLog::new(&root);
        let id = CompanyId::new("acme");

        let s0 = log
            .append(
                &id,
                CompanyEvent::OperatorMessage {
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
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn event_log_subscribe_delivers_new_event() {
        let root = tmp_root();
        let log = FsEventLog::new(&root);
        let id = CompanyId::new("acme");
        let mut stream = log.subscribe(&id);

        log.append(
            &id,
            CompanyEvent::OperatorMessage {
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
                text: "hi".into(),
                by: None,
                chat: None
            }
        );
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn memory_store_traces_tail_and_evict() {
        let root = tmp_root();
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
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn context_store_put_peek_search() {
        let root = tmp_root();
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
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn secret_store_isolates_companies() {
        let root = tmp_root();
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
        tokio::fs::remove_dir_all(&root).await.ok();
    }
}
