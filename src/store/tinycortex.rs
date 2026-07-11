//! TinyCortex-backed implementations of the memory and context ports.
//!
//! TinyCortex is the TinyHumans managed memory service. It is **not** checked
//! out in this repository, so this module depends on nothing external: the
//! storage seam is the [`CortexClient`] trait, and the compiled-and-tested
//! backend is [`InMemoryCortex`], an offline in-memory implementation. A real
//! HTTP client ([`HttpCortexClient`]) is present but inert — it returns
//! [`OpenCompanyError::Unimplemented`] until the service is reachable through
//! the OpenHuman seam. Nothing here touches the network.
//!
//! [`CortexMemoryStore`] implements [`MemoryStore`] and [`CortexContextStore`]
//! implements [`ContextStore`], both by translating each port call into a
//! company-scoped [`CortexClient`] call. Every method takes the company id as
//! its first argument so the adapter enforces isolation regardless of how the
//! backend namespaces tenants.
//!
//! ## Isolation
//!
//! [`InMemoryCortex`] keys everything on the company string, so company A can
//! never read company B's traces, tasks, or chunks.
//!
//! ## Eviction archives rather than destroys
//!
//! [`MemoryStore::evict`] routes to [`CortexClient::archive_traces`], which
//! moves live traces into a separate archive rather than dropping them. Only the
//! inherent operator-rights methods ([`CortexMemoryStore::hard_delete_trace`],
//! [`CortexContextStore::hard_delete_chunk`], [`CortexMemoryStore::redact_all`])
//! mutate or remove data for good, and they propagate to the backing store, not
//! just to an index.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::context::ContextStore;
use crate::ports::memory::MemoryStore;
use crate::ports::types::{
    ChunkAddr, ChunkHit, ChunkMeta, CompanyId, CompressedTrace, ContextChunk, EvictionPolicy,
    TaskResult,
};
use crate::store::content_address;

// ---------------------------------------------------------------------------
// The storage seam
// ---------------------------------------------------------------------------

/// The company-scoped storage seam the cortex stores translate port calls into.
///
/// Every method takes the company id as a `&str` first argument so the adapter
/// enforces tenant isolation regardless of how a backend namespaces companies.
/// The offline [`InMemoryCortex`] is the compiled-and-tested implementation; a
/// networked implementation would reach TinyCortex through the OpenHuman seam.
#[async_trait]
pub trait CortexClient: Send + Sync {
    /// Appends one compressed trace for a company.
    async fn append_trace(&self, company: &str, trace: CompressedTrace) -> Result<()>;
    /// Returns up to `limit` most-recent live traces, newest last.
    async fn recent_traces(&self, company: &str, limit: usize) -> Result<Vec<CompressedTrace>>;
    /// Upserts a completed background task's result.
    async fn put_task_result(&self, company: &str, result: TaskResult) -> Result<()>;
    /// Moves live traces matching `policy` into the archive, returning how many
    /// were archived. Archiving retains the data; it never destroys it.
    async fn archive_traces(&self, company: &str, policy: EvictionPolicy) -> Result<u64>;

    /// Stores a context chunk under its content `addr`.
    async fn put_chunk(&self, company: &str, addr: &str, chunk: ContextChunk) -> Result<()>;
    /// Lists metadata for chunks whose label starts with `prefix`.
    async fn list_chunks(&self, company: &str, prefix: &str) -> Result<Vec<ChunkMeta>>;
    /// Reads a chunk body by address, `None` if absent.
    async fn peek_chunk(&self, company: &str, addr: &str) -> Result<Option<String>>;
    /// Searches chunk bodies for `query`, returning up to `limit` ranked hits.
    async fn search_chunks(
        &self,
        company: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ChunkHit>>;

    /// Permanently removes every trace (live or archived) with `cycle_id`,
    /// returning whether anything was removed. Propagates to the backing store.
    async fn hard_delete_trace(&self, company: &str, cycle_id: &str) -> Result<bool>;
    /// Permanently removes the chunk at `addr`, returning whether it existed.
    /// Propagates to the backing store.
    async fn hard_delete_chunk(&self, company: &str, addr: &str) -> Result<bool>;
    /// Rewrites every occurrence of `needle` with `replacement` across a
    /// company's traces (live and archived) and chunk bodies, returning the
    /// number of occurrences replaced. Propagates to the backing store.
    async fn redact(&self, company: &str, needle: &str, replacement: &str) -> Result<u64>;
}

// ---------------------------------------------------------------------------
// Offline in-memory backend
// ---------------------------------------------------------------------------

/// One company's live traces, archived traces, task results, and chunks.
#[derive(Default)]
struct CompanyCells {
    /// Live traces in insertion order (newest last).
    traces: Vec<CompressedTrace>,
    /// Traces archived by [`CortexClient::archive_traces`].
    archive: Vec<CompressedTrace>,
    /// Task results keyed by task id.
    tasks: HashMap<String, TaskResult>,
    /// Context chunks in insertion order.
    chunks: Vec<StoredChunk>,
}

/// A stored context chunk: its content address, label, and body.
#[derive(Clone)]
struct StoredChunk {
    addr: String,
    label: String,
    body: String,
}

/// An offline, in-memory [`CortexClient`] backing the cortex stores.
///
/// Per-company data lives in a `HashMap<String, CompanyCells>` behind a
/// [`StdMutex`]. Archived traces are held separately from live ones so
/// [`Self`]'s eviction archives rather than destroys, while the `hard_delete_*`
/// methods remove data outright.
#[derive(Default)]
pub struct InMemoryCortex {
    cells: StdMutex<HashMap<String, CompanyCells>>,
}

impl InMemoryCortex {
    /// Builds an empty in-memory cortex.
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `f` against the (created-on-demand) cells for `company`.
    fn with_company<R>(&self, company: &str, f: impl FnOnce(&mut CompanyCells) -> R) -> R {
        let mut map = self.cells.lock().expect("cortex mutex poisoned");
        let cells = map.entry(company.to_string()).or_default();
        f(cells)
    }
}

#[async_trait]
impl CortexClient for InMemoryCortex {
    async fn append_trace(&self, company: &str, trace: CompressedTrace) -> Result<()> {
        self.with_company(company, |c| c.traces.push(trace));
        Ok(())
    }

    async fn recent_traces(&self, company: &str, limit: usize) -> Result<Vec<CompressedTrace>> {
        Ok(self.with_company(company, |c| {
            let start = c.traces.len().saturating_sub(limit);
            c.traces[start..].to_vec()
        }))
    }

    async fn put_task_result(&self, company: &str, result: TaskResult) -> Result<()> {
        self.with_company(company, |c| {
            c.tasks.insert(result.task_id.clone(), result);
        });
        Ok(())
    }

    async fn archive_traces(&self, company: &str, policy: EvictionPolicy) -> Result<u64> {
        Ok(self.with_company(company, |c| {
            let archived: Vec<CompressedTrace> = match policy {
                EvictionPolicy::KeepRecent { n } => {
                    let cut = c.traces.len().saturating_sub(n);
                    c.traces.drain(..cut).collect()
                }
                EvictionPolicy::OlderThan { before_millis } => {
                    let mut kept = Vec::with_capacity(c.traces.len());
                    let mut moved = Vec::new();
                    for t in c.traces.drain(..) {
                        if t.at_millis < before_millis {
                            moved.push(t);
                        } else {
                            kept.push(t);
                        }
                    }
                    c.traces = kept;
                    moved
                }
            };
            let count = archived.len() as u64;
            c.archive.extend(archived);
            count
        }))
    }

    async fn put_chunk(&self, company: &str, addr: &str, chunk: ContextChunk) -> Result<()> {
        self.with_company(company, |c| {
            // Content-addressed: an identical body is stored once.
            if !c.chunks.iter().any(|s| s.addr == addr) {
                c.chunks.push(StoredChunk {
                    addr: addr.to_string(),
                    label: chunk.label,
                    body: chunk.body,
                });
            }
        });
        Ok(())
    }

    async fn list_chunks(&self, company: &str, prefix: &str) -> Result<Vec<ChunkMeta>> {
        Ok(self.with_company(company, |c| {
            c.chunks
                .iter()
                .filter(|s| s.label.starts_with(prefix))
                .map(|s| ChunkMeta {
                    addr: ChunkAddr::new(s.addr.clone()),
                    label: s.label.clone(),
                    len: s.body.len(),
                })
                .collect()
        }))
    }

    async fn peek_chunk(&self, company: &str, addr: &str) -> Result<Option<String>> {
        Ok(self.with_company(company, |c| {
            c.chunks
                .iter()
                .find(|s| s.addr == addr)
                .map(|s| s.body.clone())
        }))
    }

    async fn search_chunks(
        &self,
        company: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ChunkHit>> {
        let chunks = self.with_company(company, |c| c.chunks.clone());
        let mut hits = score_chunks(&chunks, query);
        hits.truncate(limit);
        Ok(hits)
    }

    async fn hard_delete_trace(&self, company: &str, cycle_id: &str) -> Result<bool> {
        Ok(self.with_company(company, |c| {
            let before = c.traces.len() + c.archive.len();
            c.traces.retain(|t| t.cycle_id != cycle_id);
            c.archive.retain(|t| t.cycle_id != cycle_id);
            before != c.traces.len() + c.archive.len()
        }))
    }

    async fn hard_delete_chunk(&self, company: &str, addr: &str) -> Result<bool> {
        Ok(self.with_company(company, |c| {
            let before = c.chunks.len();
            c.chunks.retain(|s| s.addr != addr);
            before != c.chunks.len()
        }))
    }

    async fn redact(&self, company: &str, needle: &str, replacement: &str) -> Result<u64> {
        if needle.is_empty() {
            return Ok(0);
        }
        Ok(self.with_company(company, |c| {
            let mut replaced = 0u64;
            for t in c.traces.iter_mut().chain(c.archive.iter_mut()) {
                replaced += replace_in_place(&mut t.summary, needle, replacement);
            }
            for s in c.chunks.iter_mut() {
                replaced += replace_in_place(&mut s.body, needle, replacement);
            }
            replaced
        }))
    }
}

/// Replaces every occurrence of `needle` in `s`, returning the count replaced.
fn replace_in_place(s: &mut String, needle: &str, replacement: &str) -> u64 {
    let count = s.matches(needle).count() as u64;
    if count > 0 {
        *s = s.replace(needle, replacement);
    }
    count
}

/// Ranks chunks by token-overlap against `query`, best first.
///
/// A chunk's score is the fraction of distinct query tokens that appear in its
/// body (case-insensitive). Chunks with no overlap are dropped. This is
/// strictly better than a raw substring scan yet stays lexical and offline; the
/// seam allows a networked backend to answer with semantic recall instead.
fn score_chunks(chunks: &[StoredChunk], query: &str) -> Vec<ChunkHit> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let mut distinct: Vec<&String> = Vec::new();
    for t in &terms {
        if !distinct.contains(&t) {
            distinct.push(t);
        }
    }

    let mut scored: Vec<ChunkHit> = Vec::new();
    for chunk in chunks {
        let body_lower = chunk.body.to_lowercase();
        let matched = distinct
            .iter()
            .filter(|t| body_lower.contains(t.as_str()))
            .count();
        if matched == 0 {
            continue;
        }
        let score = matched as f64 / distinct.len() as f64;
        // Anchor the snippet on the first matching term.
        let pos = distinct
            .iter()
            .filter_map(|t| body_lower.find(t.as_str()).map(|p| (p, t.len())))
            .min_by_key(|(p, _)| *p);
        let snippet = match pos {
            Some((p, len)) => snippet_around(&chunk.body, p, len),
            None => chunk.body.clone(),
        };
        scored.push(ChunkHit {
            addr: ChunkAddr::new(chunk.addr.clone()),
            snippet,
            score,
        });
    }
    scored.sort_by(|a, b| b.score.total_cmp(&a.score));
    scored
}

/// Extracts a char-boundary-safe window around `pos` of a matched term.
fn snippet_around(body: &str, pos: usize, term_len: usize) -> String {
    let raw_start = pos.saturating_sub(24);
    let raw_end = (pos + term_len + 24).min(body.len());
    let start = (raw_start..=pos)
        .find(|&i| body.is_char_boundary(i))
        .unwrap_or(pos);
    let end = (raw_end..=body.len())
        .find(|&i| body.is_char_boundary(i))
        .unwrap_or(body.len());
    body[start..end].to_string()
}

// ---------------------------------------------------------------------------
// Inert real client
// ---------------------------------------------------------------------------

/// A networked [`CortexClient`] that reaches TinyCortex through the OpenHuman
/// seam.
///
/// TinyCortex is not checked out in this repository, so every method is inert
/// and returns [`OpenCompanyError::Unimplemented`]. It ships compiled-but-inert
/// so the feature graph is real; wiring it to the service — once
/// `vendor/openhuman/vendor/tinycortex` is present — is a documented follow-up.
/// No network dependency is added.
#[derive(Default)]
pub struct HttpCortexClient {
    _private: (),
}

impl HttpCortexClient {
    /// Builds the inert client.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl CortexClient for HttpCortexClient {
    async fn append_trace(&self, _company: &str, _trace: CompressedTrace) -> Result<()> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
    async fn recent_traces(&self, _company: &str, _limit: usize) -> Result<Vec<CompressedTrace>> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
    async fn put_task_result(&self, _company: &str, _result: TaskResult) -> Result<()> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
    async fn archive_traces(&self, _company: &str, _policy: EvictionPolicy) -> Result<u64> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
    async fn put_chunk(&self, _company: &str, _addr: &str, _chunk: ContextChunk) -> Result<()> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
    async fn list_chunks(&self, _company: &str, _prefix: &str) -> Result<Vec<ChunkMeta>> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
    async fn peek_chunk(&self, _company: &str, _addr: &str) -> Result<Option<String>> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
    async fn search_chunks(
        &self,
        _company: &str,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<ChunkHit>> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
    async fn hard_delete_trace(&self, _company: &str, _cycle_id: &str) -> Result<bool> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
    async fn hard_delete_chunk(&self, _company: &str, _addr: &str) -> Result<bool> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
    async fn redact(&self, _company: &str, _needle: &str, _replacement: &str) -> Result<u64> {
        Err(OpenCompanyError::Unimplemented("tinycortex http client"))
    }
}

// ---------------------------------------------------------------------------
// Port adapters
// ---------------------------------------------------------------------------

/// A [`MemoryStore`] backed by a [`CortexClient`].
#[derive(Clone)]
pub struct CortexMemoryStore {
    client: Arc<dyn CortexClient>,
}

impl CortexMemoryStore {
    /// Wraps a client as a memory store.
    pub fn new(client: Arc<dyn CortexClient>) -> Self {
        Self { client }
    }

    /// Operator right: permanently removes every trace with `cycle_id` (live or
    /// archived), returning whether anything was removed. Not a port method —
    /// this propagates a hard delete to the backing store.
    pub async fn hard_delete_trace(&self, id: &CompanyId, cycle_id: &str) -> Result<bool> {
        self.client.hard_delete_trace(id.as_ref(), cycle_id).await
    }

    /// Operator right: rewrites every occurrence of `needle` across the
    /// company's traces and chunks, returning the number of occurrences
    /// replaced. Propagates to the backing store, not just an index.
    pub async fn redact_all(&self, id: &CompanyId, needle: &str, replacement: &str) -> Result<u64> {
        self.client.redact(id.as_ref(), needle, replacement).await
    }
}

#[async_trait]
impl MemoryStore for CortexMemoryStore {
    async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()> {
        self.client.append_trace(id.as_ref(), trace).await
    }

    async fn recent_traces(&self, id: &CompanyId, limit: usize) -> Result<Vec<CompressedTrace>> {
        self.client.recent_traces(id.as_ref(), limit).await
    }

    async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()> {
        self.client.put_task_result(id.as_ref(), result).await
    }

    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64> {
        // Eviction archives rather than destroys.
        self.client.archive_traces(id.as_ref(), policy).await
    }
}

/// A [`ContextStore`] backed by a [`CortexClient`].
#[derive(Clone)]
pub struct CortexContextStore {
    client: Arc<dyn CortexClient>,
}

impl CortexContextStore {
    /// Wraps a client as a context store.
    pub fn new(client: Arc<dyn CortexClient>) -> Self {
        Self { client }
    }

    /// Operator right: permanently removes the chunk at `addr`, returning
    /// whether it existed. Propagates the hard delete to the backing store.
    pub async fn hard_delete_chunk(&self, id: &CompanyId, addr: &ChunkAddr) -> Result<bool> {
        self.client
            .hard_delete_chunk(id.as_ref(), addr.as_ref())
            .await
    }

    /// Operator right: rewrites every occurrence of `needle` across the
    /// company's chunks and traces, returning the number replaced. Propagates
    /// to the backing store.
    pub async fn redact_all(&self, id: &CompanyId, needle: &str, replacement: &str) -> Result<u64> {
        self.client.redact(id.as_ref(), needle, replacement).await
    }
}

#[async_trait]
impl ContextStore for CortexContextStore {
    async fn put(&self, id: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr> {
        // Mint the same content address the fs/sqlite backends mint.
        let addr = content_address(&chunk.body);
        self.client.put_chunk(id.as_ref(), &addr, chunk).await?;
        Ok(ChunkAddr::new(addr))
    }

    async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>> {
        self.client.list_chunks(id.as_ref(), prefix).await
    }

    async fn peek(
        &self,
        id: &CompanyId,
        addr: &ChunkAddr,
        range: Option<Range<usize>>,
    ) -> Result<String> {
        let body = self
            .client
            .peek_chunk(id.as_ref(), addr.as_ref())
            .await?
            .ok_or_else(|| {
                OpenCompanyError::Store(format!("context chunk not found: {}", addr.as_ref()))
            })?;
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
        self.client.search_chunks(id.as_ref(), query, limit).await
    }
}

/// Builds a [`MemoryStore`] + [`ContextStore`] pair over one shared
/// [`InMemoryCortex`], so both ports read and write the same offline backend.
///
/// This is the injection shape a platform operator uses: feed the returned
/// stores into `RuntimeBuilder::with_memory` / `with_context`.
pub fn in_memory() -> (Arc<CortexMemoryStore>, Arc<CortexContextStore>) {
    let client: Arc<dyn CortexClient> = Arc::new(InMemoryCortex::new());
    (
        Arc::new(CortexMemoryStore::new(client.clone())),
        Arc::new(CortexContextStore::new(client)),
    )
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::events::EventLog;
    use crate::ports::now_millis;
    use crate::ports::store::CompanyStore;
    use crate::store::conformance;
    use crate::store::{FsCompanyStore, FsEventLog};

    /// The four port trait objects the conformance suite drives: fs company and
    /// event stores paired with the cortex memory and context stores.
    type ConformanceStores = (
        Arc<dyn CompanyStore>,
        Arc<dyn EventLog>,
        Arc<CortexMemoryStore>,
        Arc<CortexContextStore>,
    );

    /// Builds cortex memory+context stores over one shared client, plus fs
    /// company/event stores (over a tempdir) for the two conformance slots the
    /// cortex backend does not implement.
    fn stores(dir: &std::path::Path) -> ConformanceStores {
        let (mem, ctx) = in_memory();
        (
            Arc::new(FsCompanyStore::new(dir.to_path_buf())),
            Arc::new(FsEventLog::new(dir.to_path_buf())),
            mem,
            ctx,
        )
    }

    #[tokio::test]
    async fn conformance_isolation_by_company() {
        let dir = tempfile::tempdir().unwrap();
        let (store, events, mem, ctx) = stores(dir.path());
        conformance::assert_isolation_by_company(store, events, mem, ctx).await;
    }

    #[tokio::test]
    async fn conformance_export_totality() {
        let dir = tempfile::tempdir().unwrap();
        let (store, events, mem, ctx) = stores(dir.path());
        conformance::assert_export_totality(store, events, mem, ctx).await;
    }

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    #[tokio::test]
    async fn recent_traces_newest_last() {
        let (mem, _ctx) = in_memory();
        let id = company();
        for i in 0..4 {
            mem.save_trace(&id, CompressedTrace::now(format!("c{i}"), format!("s{i}")))
                .await
                .unwrap();
        }
        let recent = mem.recent_traces(&id, 2).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].cycle_id, "c2");
        assert_eq!(
            recent[1].cycle_id, "c3",
            "recent_traces must be newest last"
        );
    }

    #[tokio::test]
    async fn evict_keep_recent_and_older_than_archives_not_destroys() {
        // Hold the shared client directly so the test can inspect the archive.
        let client: Arc<InMemoryCortex> = Arc::new(InMemoryCortex::new());
        let mem = CortexMemoryStore::new(client.clone());
        let id = company();
        for i in 0..5 {
            mem.save_trace(&id, CompressedTrace::now(format!("c{i}"), format!("s{i}")))
                .await
                .unwrap();
        }

        let archived = mem
            .evict(&id, EvictionPolicy::KeepRecent { n: 2 })
            .await
            .unwrap();
        assert_eq!(archived, 3, "three of five traces archived");

        let kept = mem.recent_traces(&id, 10).await.unwrap();
        assert_eq!(kept.len(), 2, "live set shrank to the recent two");
        assert_eq!(kept[1].cycle_id, "c4");

        // The archive retained the evicted traces rather than destroying them.
        let archive_len = client.with_company(id.as_ref(), |c| c.archive.len());
        assert_eq!(archive_len, 3, "evicted traces live on in the archive");

        // OlderThan with a future cutoff archives the remaining live traces.
        let archived = mem
            .evict(
                &id,
                EvictionPolicy::OlderThan {
                    before_millis: now_millis() + 60_000,
                },
            )
            .await
            .unwrap();
        assert_eq!(archived, 2);
        assert!(mem.recent_traces(&id, 10).await.unwrap().is_empty());
        let archive_len = client.with_company(id.as_ref(), |c| c.archive.len());
        assert_eq!(archive_len, 5, "every evicted trace is archived, none lost");
    }

    #[tokio::test]
    async fn hard_delete_trace_and_chunk_propagate() {
        let client: Arc<InMemoryCortex> = Arc::new(InMemoryCortex::new());
        let mem = CortexMemoryStore::new(client.clone());
        let ctx = CortexContextStore::new(client.clone());
        let id = company();

        mem.save_trace(&id, CompressedTrace::now("c0", "keep"))
            .await
            .unwrap();
        mem.save_trace(&id, CompressedTrace::now("c1", "drop"))
            .await
            .unwrap();
        // Archive c0 so the hard delete must reach the archive, not just live.
        mem.evict(&id, EvictionPolicy::KeepRecent { n: 1 })
            .await
            .unwrap();

        assert!(mem.hard_delete_trace(&id, "c0").await.unwrap());
        assert!(!mem.hard_delete_trace(&id, "missing").await.unwrap());
        // Gone from live and archive alike.
        assert!(
            mem.recent_traces(&id, 10)
                .await
                .unwrap()
                .iter()
                .all(|t| t.cycle_id != "c0")
        );
        let archive_has_c0 = client.with_company(id.as_ref(), |c| {
            c.archive.iter().any(|t| t.cycle_id == "c0")
        });
        assert!(!archive_has_c0, "hard delete reached the backing archive");

        let addr = ctx
            .put(
                &id,
                ContextChunk {
                    label: "doc/a".into(),
                    body: "secret body".into(),
                },
            )
            .await
            .unwrap();
        assert!(ctx.hard_delete_chunk(&id, &addr).await.unwrap());
        assert!(ctx.list(&id, "").await.unwrap().is_empty());
        assert!(ctx.peek(&id, &addr, None).await.is_err());
    }

    #[tokio::test]
    async fn redact_rewrites_traces_and_chunks() {
        let client: Arc<InMemoryCortex> = Arc::new(InMemoryCortex::new());
        let mem = CortexMemoryStore::new(client.clone());
        let ctx = CortexContextStore::new(client);
        let id = company();

        mem.save_trace(
            &id,
            CompressedTrace::now("c0", "email alice@example.com noted"),
        )
        .await
        .unwrap();
        let addr = ctx
            .put(
                &id,
                ContextChunk {
                    label: "doc/a".into(),
                    body: "contact alice@example.com twice: alice@example.com".into(),
                },
            )
            .await
            .unwrap();

        let replaced = mem
            .redact_all(&id, "alice@example.com", "[redacted]")
            .await
            .unwrap();
        assert_eq!(replaced, 3, "one in the trace, two in the chunk");

        let trace = mem.recent_traces(&id, 1).await.unwrap();
        assert!(!trace[0].summary.contains("alice@example.com"));
        let body = ctx.peek(&id, &addr, None).await.unwrap();
        assert!(!body.contains("alice@example.com"));
        assert!(body.contains("[redacted]"));
    }

    #[tokio::test]
    async fn search_ranks_by_relevance() {
        let (_mem, ctx) = in_memory();
        let id = company();
        ctx.put(
            &id,
            ContextChunk {
                label: "doc/a".into(),
                body: "quarterly revenue growth strategy".into(),
            },
        )
        .await
        .unwrap();
        ctx.put(
            &id,
            ContextChunk {
                label: "doc/b".into(),
                body: "revenue only".into(),
            },
        )
        .await
        .unwrap();
        ctx.put(
            &id,
            ContextChunk {
                label: "doc/c".into(),
                body: "unrelated note".into(),
            },
        )
        .await
        .unwrap();

        let hits = ctx.search(&id, "revenue growth", 10).await.unwrap();
        assert_eq!(
            hits.len(),
            2,
            "the unrelated chunk scores zero and is dropped"
        );
        // The chunk matching both query terms outranks the one matching one.
        assert!(hits[0].score > hits[1].score);
        assert!(hits[0].snippet.contains("revenue"));

        // A limit truncates the ranked list.
        let one = ctx.search(&id, "revenue growth", 1).await.unwrap();
        assert_eq!(one.len(), 1);
        assert!(one[0].score >= hits[1].score);
    }

    #[tokio::test]
    async fn task_result_round_trips() {
        let client: Arc<InMemoryCortex> = Arc::new(InMemoryCortex::new());
        let mem = CortexMemoryStore::new(client.clone());
        let id = company();
        mem.save_task_result(
            &id,
            TaskResult {
                task_id: "t1".into(),
                ok: false,
                output: serde_json::json!({"v": 1}),
            },
        )
        .await
        .unwrap();
        // Same id overwrites rather than duplicating.
        mem.save_task_result(
            &id,
            TaskResult {
                task_id: "t1".into(),
                ok: true,
                output: serde_json::json!({"v": 2}),
            },
        )
        .await
        .unwrap();
        let (ok, count) = client.with_company(id.as_ref(), |c| {
            (c.tasks.get("t1").map(|t| t.ok), c.tasks.len())
        });
        assert_eq!(count, 1);
        assert_eq!(ok, Some(true));
    }

    #[tokio::test]
    async fn http_client_is_inert() {
        let client = HttpCortexClient::new();
        let err = client
            .recent_traces("acme", 1)
            .await
            .expect_err("http client is not implemented");
        assert!(matches!(err, OpenCompanyError::Unimplemented(_)));
    }
}
