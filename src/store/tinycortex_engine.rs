//! In-pod, persistent TinyCortex memory engine (degraded-embedding mode).
//!
//! [`EngineCortex`] is the real, engine-backed [`CortexClient`] that replaces the
//! former inert HTTP client. It runs the OpenHuman `tinycortex` engine **inside
//! the pod** with durable local storage: every company gets its own workspace
//! under `<data_root>/memory/<company>/`, and the engine's canonical per-workspace
//! SQLite database (opened + migrated through the crate's own
//! [`chunks::shared_connection`]) holds that company's traces, task results, and
//! context chunks. The engine "never makes a network call".
//!
//! ## Two-tier recall — vector first, lexical fallback (188c2)
//!
//! Issue #188 slice **188c1** lit up the persistent engine in degraded
//! lexical/recency mode with **zero** embedding compute. Slice **188c2** adds
//! the meaning tier: when a [`HostedEmbeddings`](crate::harness::embeddings)
//! backend is injected (a data-dir build with a hosted credential), each stored
//! chunk is embedded into a per-company
//! [`VectorStore`](tinycortex::memory::store::vectors::VectorStore) (a SQLite
//! `vectors.db` beside the KV database), and [`search_chunks`](EngineCortex) runs
//! cosine recall **first**, then tops the result up with the existing lexical
//! [`score_chunks`] scorer (excluding duplicate addresses) up to the caller's
//! limit. When no backend is injected — or on any embedding/search outage — the
//! engine falls back to the **pure lexical** path unchanged, so recall degrades
//! gracefully and the store never fails on an embeddings outage. The KV tier
//! stays the 1:1 addr↔body source of truth: vector hits are mapped back through
//! it for their snippet and dangling ids are pruned.
//!
//! This slice deliberately wires **only** the `VectorStore` store+search path
//! (dimension-agnostic, native 1024-dim). The retrieval-scorer `Embedder` /
//! summary-tree seal (the hard-768 path) and a full-corpus reconcile are
//! deferred to #198.
//!
//! The engine's `MemoryConfig` still sets `embedding.strict = false` — the
//! crate's own summary-tree embedder stays inert; the meaning tier here is a
//! separate, explicitly-injected `VectorStore` over the same workspace.
//!
//! ## Why chunks persist through the KV tier, not `ingest`/retrieval primitives
//!
//! The plan first mapped `put_chunk` onto the crate's `ingest` pipeline and
//! `search_chunks` onto its hybrid-retrieval primitives. Two crate realities make
//! that the wrong fit for this slice, so chunks are persisted through the engine's
//! **KV tier** (on the engine's shared workspace connection) instead:
//!
//! 1. **Retrieval can't rank lexically in degraded mode.** The crate's own
//!    `retrieval` module documents that its primitives currently rank by *stored
//!    admission score, semantic-cosine rerank, or recency* — the keyword/graph
//!    scorers are "defined, not yet wired". With an inert embedder there is no
//!    semantic signal, so a `search_chunks` built on them would not produce the
//!    `[0, 1]` token-overlap relevance the [`ContextStore`](crate::ports::context::ContextStore)
//!    contract (and the degraded-mode search test) requires.
//! 2. **`ingest` fragments the 1:1 addr↔body + label contract.** OpenCompany's
//!    context store addresses each chunk by a content hash and lists by a
//!    label prefix; the crate's ingest path re-chunks/scores a document into its
//!    own sha-256 chunk ids, which cannot round-trip OpenCompany's `addr`, its
//!    `peek(addr)`, or its `list(prefix)` semantics.
//!
//! So chunk bodies + metadata live as content-addressed KV entries in the same
//! per-company engine database, and `search_chunks` ranks them with the same
//! lexical token-overlap the offline [`InMemoryCortex`](crate::store::tinycortex::InMemoryCortex)
//! backend already defines. Semantic recall over the summary tree arrives with
//! embeddings in 188c2.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use tinycortex::memory::chunks::shared_connection;
use tinycortex::memory::config::MemoryConfig;
use tinycortex::memory::store::KvStore;
use tinycortex::memory::store::vectors::embedding::EmbeddingBackend;
use tinycortex::memory::store::vectors::{SearchResult, VectorStore};

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::now_millis;
use crate::ports::types::{
    ChunkAddr, ChunkHit, ChunkMeta, CompressedTrace, ContextChunk, EvictionPolicy, TaskResult,
};
use crate::store::tinycortex::{CortexClient, CortexContextStore, CortexMemoryStore};

// ---------------------------------------------------------------------------
// KV layout
// ---------------------------------------------------------------------------

/// Key holding a company's live traces, as a JSON array (newest last).
const KEY_TRACES: &str = "oc:traces";
/// Key holding a company's archived traces, as a JSON array.
const KEY_ARCHIVE: &str = "oc:archive";
/// Key prefix for one task result per completed background task.
const KEY_TASK_PREFIX: &str = "oc:task:";
/// Key prefix for one context chunk per content address.
const KEY_CHUNK_PREFIX: &str = "oc:chunk:";

/// Max chunks embedded in a single boot's best-effort backfill, so a cold open
/// of a large pre-embeddings corpus stays bounded (the rest is covered by live
/// writes; a full reconcile is deferred to #198).
const BACKFILL_LIMIT: usize = 256;
/// Backfill sub-batch size, aligned with the hosted client's per-request cap.
const BACKFILL_BATCH: usize = 64;

/// The text embedded for a chunk: its label and body, so a semantic query can
/// match either the topic label or the content.
fn embed_text(label: &str, body: &str) -> String {
    format!("{label}\n{body}")
}

/// A persisted context chunk: its label(s), body, and first-stored wall-clock.
#[derive(Clone, Serialize, Deserialize)]
struct StoredChunk {
    /// The first label to claim this address — kept meaningful on its own so a
    /// binary from before `labels` existed still reads what it always read.
    label: String,
    body: String,
    stored_at_millis: u64,
    /// Every label claiming this address (#1300) — content addressing means a
    /// byte-identical body stored under a second label lands here rather than
    /// as a second record. Records written before the field existed decode to
    /// an empty vec; [`labels_of`] unions the scalar back in.
    #[serde(default)]
    labels: Vec<String>,
}

/// Every label claiming `chunk`, deduped, scalar (first-stored) label first.
fn labels_of(chunk: &StoredChunk) -> Vec<String> {
    let mut labels = vec![chunk.label.clone()];
    for label in &chunk.labels {
        if !labels.iter().any(|have| have == label) {
            labels.push(label.clone());
        }
    }
    labels
}

// ---------------------------------------------------------------------------
// Per-company engine handle
// ---------------------------------------------------------------------------

/// One company's opened engine: the KV tier over its per-workspace SQLite DB,
/// plus an optional [`VectorStore`] meaning tier when an embeddings backend is
/// injected (188c2).
struct CompanyEngine {
    kv: KvStore,
    /// Serializes read-modify-write sequences over this company's shared
    /// aggregate keys (`oc:traces` / `oc:archive`). The KV tier upserts a whole
    /// JSON array per key, so an unguarded `append`/`archive`/`hard_delete`/
    /// `redact` reads the array, mutates it in memory, and writes it back — two
    /// concurrent calls would both read N entries and both write N+1, silently
    /// dropping one. Holding this async mutex across each such sequence makes the
    /// aggregate updates serializable per company. It is an async
    /// [`tokio::sync::Mutex`] (not [`std::sync::Mutex`]) because it is held across
    /// `.await` points in the trait methods.
    write_lock: tokio::sync::Mutex<()>,
    /// The per-company vector index (`<workspace>/vectors.db`), present only
    /// when an [`EmbeddingBackend`] was injected. `None` ⇒ pure lexical recall.
    vectors: Option<VectorStore>,
}

impl CompanyEngine {
    /// Opens (creating on first use) the engine for a company workspace.
    ///
    /// Builds the crate's `MemoryConfig` directly (`strict = false` — the crate's
    /// own summary-tree embedder stays inert), then rides a [`KvStore`] on the
    /// engine's shared per-workspace SQLite connection. When `embeddings` is
    /// `Some`, a [`VectorStore`] is also opened at `<workspace>/vectors.db` for
    /// the meaning tier; only a confirmed dimension/signature drift against a
    /// pre-existing index is recovered by clearing and rebuilding it (the
    /// backfill then re-populates) — any other open failure degrades to
    /// lexical-only *without deleting the existing index files*, and never
    /// fails the whole open. The workspace directory is created if absent.
    fn open(workspace: PathBuf, embeddings: Option<Arc<dyn EmbeddingBackend>>) -> Result<Self> {
        std::fs::create_dir_all(&workspace).map_err(|e| {
            OpenCompanyError::Store(format!(
                "create engine workspace {}: {e}",
                workspace.display()
            ))
        })?;

        let mut config = MemoryConfig::new(workspace.clone());
        // The crate's own summary-tree embedder stays inert; the 188c2 meaning
        // tier is the separately-injected `VectorStore` below.
        config.embedding.strict = false;
        config
            .validate()
            .map_err(|e| OpenCompanyError::Store(format!("invalid memory config: {e}")))?;

        let conn = shared_connection(&config)
            .map_err(|e| OpenCompanyError::Store(format!("open engine connection: {e}")))?;
        let kv = KvStore::from_shared_connection(conn)
            .map_err(|e| OpenCompanyError::Store(format!("open engine kv store: {e}")))?;

        let vectors = embeddings.and_then(|backend| open_vector_store(&workspace, backend));

        Ok(Self {
            kv,
            write_lock: tokio::sync::Mutex::new(()),
            vectors,
        })
    }

    /// Best-effort, bounded one-time backfill of the meaning tier: when the
    /// vector index for this company is **empty** but KV chunks exist (a pod that
    /// ran 188c1 lexical-only, or a just-cleared index after a drift), embed up
    /// to [`BACKFILL_LIMIT`] chunks so semantic recall covers the existing
    /// corpus. Runs once per process after a fresh open. An embedding-outage
    /// error leaves the affected chunks lexically findable — the engine never
    /// fails on the backend. A partially-populated index is left to live writes;
    /// a full-corpus sweep is deferred to #198.
    async fn backfill_vectors(&self, company: &str) {
        let Some(vectors) = &self.vectors else {
            return;
        };
        // Trigger only on an empty index (no per-id existence probe in this cut).
        match vectors.count(Some(company)) {
            Ok(0) => {}
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(company, error = %e, "[tinycortex] vector count failed; skipping backfill");
                return;
            }
        }
        let chunks = match self.all_chunks() {
            Ok(chunks) => chunks,
            Err(e) => {
                tracing::warn!(company, error = %e, "[tinycortex] listing chunks for backfill failed");
                return;
            }
        };
        if chunks.is_empty() {
            return;
        }
        // `all_chunks()` returns KV listing order, which is not meaningful
        // (implementation detail of the underlying store) — sort newest-first by
        // `stored_at_millis` before truncating so the bounded backfill
        // deterministically favors the most recent chunks and is reproducible
        // across boots, instead of picking an arbitrary `BACKFILL_LIMIT`-sized
        // slice of listing order. `addr` (a content hash, unique) breaks ties
        // between chunks stored in the same millisecond.
        let mut chunks = chunks;
        chunks.sort_by(|(addr_a, a), (addr_b, b)| {
            b.stored_at_millis
                .cmp(&a.stored_at_millis)
                .then_with(|| addr_a.cmp(addr_b))
        });
        let pending: Vec<(String, String)> = chunks
            .into_iter()
            .take(BACKFILL_LIMIT)
            .map(|(addr, chunk)| (addr, embed_text(&chunk.label, &chunk.body)))
            .collect();
        let mut embedded = 0usize;
        for batch in pending.chunks(BACKFILL_BATCH) {
            let entries: Vec<(&str, &str, serde_json::Value)> = batch
                .iter()
                .map(|(id, text)| (id.as_str(), text.as_str(), serde_json::json!({})))
                .collect();
            match vectors.insert_batch(company, &entries).await {
                Ok(()) => embedded += entries.len(),
                Err(e) => tracing::warn!(
                    company,
                    error = %e,
                    "[tinycortex] backfill batch embed failed; chunks remain lexically findable"
                ),
            }
        }
        if embedded > 0 {
            tracing::debug!(company, embedded, "[tinycortex] backfilled chunk vectors");
        }
    }

    /// Reads and JSON-decodes the value at `key`, `None` when absent.
    fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self
            .kv
            .get_global(key)
            .map_err(|e| OpenCompanyError::Store(format!("kv get {key}: {e}")))?
        {
            None => Ok(None),
            Some(value) => Ok(Some(serde_json::from_value(value)?)),
        }
    }

    /// JSON-encodes `value` and upserts it at `key`.
    fn put_json<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        // Store as a *structured* JSON value, never a stringified blob. The
        // engine's KV `set_global` runs a write-time safety guard over string
        // values (a checksum-gated national-ID PII scrubber + secret redaction);
        // a stringified payload would expose its integer fields — chiefly the
        // epoch-millis timestamps — to that scrubber, which can rewrite the digits
        // and corrupt the JSON. Stored as structured JSON, numbers pass through
        // the guard untouched and only string *content* is scrubbed, so
        // timestamps round-trip intact.
        let encoded = serde_json::to_value(value)?;
        self.kv
            .set_global(key, &encoded)
            .map_err(|e| OpenCompanyError::Store(format!("kv set {key}: {e}")))
    }

    /// Returns `(addr, chunk)` for every stored context chunk.
    fn all_chunks(&self) -> Result<Vec<(String, StoredChunk)>> {
        let records = self
            .kv
            .records_global()
            .map_err(|e| OpenCompanyError::Store(format!("kv list chunks: {e}")))?;
        let mut out = Vec::new();
        for record in records {
            let Some(addr) = record
                .key
                .strip_prefix(KEY_CHUNK_PREFIX)
                .map(str::to_string)
            else {
                continue;
            };
            let chunk: StoredChunk = serde_json::from_value(record.value)?;
            out.push((addr, chunk));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// EngineCortex
// ---------------------------------------------------------------------------

/// A persistent, in-pod [`CortexClient`] over the OpenHuman `tinycortex` engine.
///
/// Per-company engine handles are opened lazily and cached, each rooted at
/// `<memory_root>/<workspace_name(company)>/`. Isolation is physical: two
/// companies never share a workspace or a database file, so company A cannot
/// observe company B's data. That guarantee rests on [`workspace_name`] being
/// *injective* — distinct company ids must map to distinct directories (see it
/// for why a sanitized prefix alone is not enough).
///
/// [`workspace_name`]: EngineCortex::workspace_name
pub struct EngineCortex {
    memory_root: PathBuf,
    companies: StdMutex<HashMap<String, Arc<CompanyEngine>>>,
    /// The embeddings backend injected into every company's [`VectorStore`], or
    /// `None` for pure lexical recall (no-data-dir / no-credential / default
    /// build). Shared across all per-company vector indices.
    embeddings: Option<Arc<dyn EmbeddingBackend>>,
}

impl EngineCortex {
    /// Builds a **lexical-only** engine rooted at `memory_root` (typically
    /// `<data_dir>/memory`). Equivalent to [`with_embeddings`](Self::with_embeddings)
    /// with `None`.
    ///
    /// Opening is lazy: the first call touching a company creates + migrates that
    /// company's workspace database.
    pub fn new(memory_root: impl Into<PathBuf>) -> Self {
        Self::with_embeddings(memory_root, None)
    }

    /// Builds an engine rooted at `memory_root`, injecting `embeddings` as the
    /// meaning tier for every company's [`VectorStore`] (188c2). `None` keeps the
    /// pure lexical path.
    pub fn with_embeddings(
        memory_root: impl Into<PathBuf>,
        embeddings: Option<Arc<dyn EmbeddingBackend>>,
    ) -> Self {
        Self {
            memory_root: memory_root.into(),
            companies: StdMutex::new(HashMap::new()),
            embeddings,
        }
    }

    /// Maps a company id to its path-safe, **injective** workspace directory name.
    ///
    /// A readable sanitized prefix alone is *not* injective: mapping every char
    /// outside `[A-Za-z0-9-_]` to `_` collapses distinct ids like `acme:1`,
    /// `acme/1`, and `acme_1` onto the same `acme_1` directory — so those
    /// companies would share one workspace and one SQLite DB and read each other's
    /// traces and chunks, breaking the physical-isolation contract this type
    /// promises. To keep the name injective, a suffix derived from a stable hash
    /// of the **full raw** id is always appended: even when two sanitized prefixes
    /// collide, their raw ids differ and so do their hashes, yielding distinct
    /// directories. The same id is always stable across calls (durability rests on
    /// a company's directory not moving across restarts).
    fn workspace_name(company: &str) -> String {
        let prefix: String = company
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let suffix = stable_hash_hex(company);
        if prefix.is_empty() {
            format!("h-{suffix}")
        } else {
            format!("{prefix}-{suffix}")
        }
    }

    /// Returns the (lazily opened, cached) engine handle for `company`.
    ///
    /// The first touch opens + migrates the workspace under the cache lock (as
    /// before), then — **outside** the lock, since embedding is async + does
    /// I/O — runs a one-time best-effort vector backfill. Concurrent first
    /// touches see the cached handle and skip the backfill, so it runs exactly
    /// once per company per process.
    async fn engine(&self, company: &str) -> Result<Arc<CompanyEngine>> {
        let (engine, fresh) = {
            let mut map = self.companies.lock().expect("engine cortex mutex poisoned");
            if let Some(existing) = map.get(company) {
                return Ok(existing.clone());
            }
            let workspace = self.memory_root.join(Self::workspace_name(company));
            let engine = Arc::new(CompanyEngine::open(workspace, self.embeddings.clone())?);
            map.insert(company.to_string(), engine.clone());
            (engine, true)
        };
        if fresh {
            engine.backfill_vectors(company).await;
        }
        Ok(engine)
    }
}

/// Substring [`VectorStore::open`] emits only for a dimension mismatch between
/// the persisted `store_meta` and the runtime backend (see the vendored
/// `check_or_store_meta` in `tinycortex::memory::store::vectors::store`) —
/// `"vector store dimension mismatch: database was created with {stored}-dim
/// embeddings but the current backend ({name}) uses {runtime} dims. Delete the
/// database or reconfigure the backend."`. Matching on this exact, stable
/// substring is what distinguishes a recoverable embedding-space drift from any
/// other open failure (I/O error, permissions, corrupt sqlite file, corrupt
/// `embed_dims` metadata) — none of which should destroy a healthy index.
const DIMENSION_DRIFT_MARKER: &str = "vector store dimension mismatch";

/// Opens (or rebuilds) the per-company [`VectorStore`] at `<workspace>/vectors.db`,
/// or `None` to degrade to lexical-only.
///
/// Only a confirmed dimension/signature drift against an existing index (the
/// [`DIMENSION_DRIFT_MARKER`] error [`VectorStore::open`] emits when the
/// persisted `store_meta` disagrees with the runtime backend's dimensionality)
/// clears the index files and rebuilds once (the backfill then re-populates
/// against the current backend), so a model change never wedges the engine.
/// Any other open error (I/O, permissions, corrupt sqlite file, corrupt
/// `embed_dims` metadata) is logged and returns `None` immediately **without**
/// deleting anything — a transient or unrelated failure must never destroy a
/// healthy vector index.
fn open_vector_store(
    workspace: &std::path::Path,
    backend: Arc<dyn EmbeddingBackend>,
) -> Option<VectorStore> {
    let db = workspace.join("vectors.db");
    match VectorStore::open(&db, backend.clone()) {
        Ok(store) => Some(store),
        Err(first) => {
            if !first.to_string().contains(DIMENSION_DRIFT_MARKER) {
                tracing::warn!(
                    db = %db.display(),
                    error = %first,
                    "[tinycortex] vector store open failed (not a dimension drift); lexical-only, index left untouched"
                );
                return None;
            }
            tracing::warn!(
                db = %db.display(),
                error = %first,
                "[tinycortex] vector store open failed (embedding-space dimension drift); clearing + rebuilding"
            );
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(workspace.join(format!("vectors.db{suffix}")));
            }
            match VectorStore::open(&db, backend) {
                Ok(store) => Some(store),
                Err(second) => {
                    tracing::warn!(error = %second, "[tinycortex] vector store rebuild failed; lexical-only");
                    None
                }
            }
        }
    }
}

#[async_trait]
impl CortexClient for EngineCortex {
    async fn append_trace(&self, company: &str, trace: CompressedTrace) -> Result<()> {
        let engine = self.engine(company).await?;
        // Serialize the read-modify-write against this company's trace array so
        // concurrent appends cannot both read N and both write N+1 (dropping one).
        let _guard = engine.write_lock.lock().await;
        let mut traces: Vec<CompressedTrace> = engine.get_json(KEY_TRACES)?.unwrap_or_default();
        traces.push(trace);
        engine.put_json(KEY_TRACES, &traces)
    }

    async fn recent_traces(&self, company: &str, limit: usize) -> Result<Vec<CompressedTrace>> {
        let engine = self.engine(company).await?;
        let traces: Vec<CompressedTrace> = engine.get_json(KEY_TRACES)?.unwrap_or_default();
        let start = traces.len().saturating_sub(limit);
        Ok(traces[start..].to_vec())
    }

    async fn put_task_result(&self, company: &str, result: TaskResult) -> Result<()> {
        let engine = self.engine(company).await?;
        let key = format!("{KEY_TASK_PREFIX}{}", result.task_id);
        engine.put_json(&key, &result)
    }

    async fn archive_traces(&self, company: &str, policy: EvictionPolicy) -> Result<u64> {
        let engine = self.engine(company).await?;
        // Serialize the move-between-arrays against concurrent trace mutations.
        let _guard = engine.write_lock.lock().await;
        let mut traces: Vec<CompressedTrace> = engine.get_json(KEY_TRACES)?.unwrap_or_default();
        let mut archive: Vec<CompressedTrace> = engine.get_json(KEY_ARCHIVE)?.unwrap_or_default();

        // Eviction archives rather than destroys — same policy as InMemoryCortex.
        let moved: Vec<CompressedTrace> = match policy {
            EvictionPolicy::KeepRecent { n } => {
                let cut = traces.len().saturating_sub(n);
                traces.drain(..cut).collect()
            }
            EvictionPolicy::OlderThan { before_millis } => {
                let mut kept = Vec::with_capacity(traces.len());
                let mut moved = Vec::new();
                for trace in traces.drain(..) {
                    if trace.at_millis < before_millis {
                        moved.push(trace);
                    } else {
                        kept.push(trace);
                    }
                }
                traces = kept;
                moved
            }
        };

        let count = moved.len() as u64;
        archive.extend(moved);
        engine.put_json(KEY_TRACES, &traces)?;
        engine.put_json(KEY_ARCHIVE, &archive)?;
        Ok(count)
    }

    async fn put_chunk(&self, company: &str, addr: &str, chunk: ContextChunk) -> Result<()> {
        let engine = self.engine(company).await?;
        // Serialize the read-merge-write on the chunk record: two concurrent
        // puts of one body under different labels must both land their claim
        // (#1300), not both read the same record and drop one.
        let _guard = engine.write_lock.lock().await;
        let key = format!("{KEY_CHUNK_PREFIX}{addr}");
        // Content-addressed: an identical body is stored once, and each label
        // claiming it is folded into the record's label set. No re-embed on a
        // label-only fold: the vector row was built from the first label and
        // the body, and every label stays lexically findable through the KV
        // record either way.
        if let Some(existing) = engine.get_json::<StoredChunk>(&key)? {
            let labels = labels_of(&existing);
            if labels.iter().any(|have| have == &chunk.label) {
                return Ok(());
            }
            let mut updated = existing;
            updated.labels = labels;
            updated.labels.push(chunk.label);
            return engine.put_json(&key, &updated);
        }
        // The text used for the meaning tier, captured before the fields move.
        let text = embed_text(&chunk.label, &chunk.body);
        engine.put_json(
            &key,
            &StoredChunk {
                labels: vec![chunk.label.clone()],
                label: chunk.label,
                body: chunk.body,
                stored_at_millis: now_millis(),
            },
        )?;
        // Meaning tier: embed the newly-stored chunk. An embeddings outage must
        // NOT fail the store — the KV write above already landed, so the chunk
        // stays lexically findable and the next backfill can pick it up.
        if let Some(vectors) = &engine.vectors
            && let Err(e) = vectors
                .insert(addr, company, &text, serde_json::json!({}))
                .await
        {
            tracing::warn!(company, addr, error = %e, "[tinycortex] embedding chunk failed; stored lexically only");
        }
        Ok(())
    }

    async fn list_chunks(&self, company: &str, prefix: &str) -> Result<Vec<ChunkMeta>> {
        let engine = self.engine(company).await?;
        Ok(engine
            .all_chunks()?
            .into_iter()
            .flat_map(|(addr, chunk)| {
                // One meta per label claiming the address (#1300). The stamp
                // is the address's first write: the record is one row however
                // many labels claim it, so per-label stamps (an fs/sqlite
                // refinement) are not kept here.
                let len = chunk.body.len();
                let stored_at_millis = chunk.stored_at_millis;
                labels_of(&chunk)
                    .into_iter()
                    .filter(|label| label.starts_with(prefix))
                    .map(move |label| ChunkMeta {
                        addr: ChunkAddr::new(addr.clone()),
                        len,
                        label,
                        stored_at_millis,
                    })
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    async fn peek_chunk(&self, company: &str, addr: &str) -> Result<Option<String>> {
        let engine = self.engine(company).await?;
        let key = format!("{KEY_CHUNK_PREFIX}{addr}");
        Ok(engine.get_json::<StoredChunk>(&key)?.map(|c| c.body))
    }

    async fn search_chunks(
        &self,
        company: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ChunkHit>> {
        let engine = self.engine(company).await?;
        let chunks = engine.all_chunks()?;
        // Meaning tier first: cosine recall, mapped back through the KV chunks
        // for the snippet + dangling-id prune, then topped up with the lexical
        // scorer. On no backend, or any embed/search outage, fall through to the
        // pure lexical path unchanged.
        if let Some(vectors) = &engine.vectors {
            match vectors.search(company, query, limit).await {
                Ok(results) => return Ok(merge_hits(results, &chunks, query, limit)),
                Err(e) => tracing::warn!(
                    company,
                    error = %e,
                    "[tinycortex] vector search failed; falling back to lexical"
                ),
            }
        }
        let mut hits = score_chunks(&chunks, query);
        hits.truncate(limit);
        Ok(hits)
    }

    async fn hard_delete_trace(&self, company: &str, cycle_id: &str) -> Result<bool> {
        let engine = self.engine(company).await?;
        // Serialize the delete-across-both-arrays against concurrent mutations.
        let _guard = engine.write_lock.lock().await;
        let mut traces: Vec<CompressedTrace> = engine.get_json(KEY_TRACES)?.unwrap_or_default();
        let mut archive: Vec<CompressedTrace> = engine.get_json(KEY_ARCHIVE)?.unwrap_or_default();
        let before = traces.len() + archive.len();
        traces.retain(|t| t.cycle_id != cycle_id);
        archive.retain(|t| t.cycle_id != cycle_id);
        let changed = before != traces.len() + archive.len();
        if changed {
            engine.put_json(KEY_TRACES, &traces)?;
            engine.put_json(KEY_ARCHIVE, &archive)?;
        }
        Ok(changed)
    }

    async fn hard_delete_chunk(&self, company: &str, addr: &str) -> Result<bool> {
        let engine = self.engine(company).await?;
        let key = format!("{KEY_CHUNK_PREFIX}{addr}");
        let deleted = engine
            .kv
            .delete_global(&key)
            .map_err(|e| OpenCompanyError::Store(format!("kv delete {key}: {e}")))?;
        // Drop the vector row too, so a hard-deleted chunk can never resurface
        // through semantic recall. Best-effort — a vector-delete hiccup must not
        // fail the KV delete that already succeeded.
        if let Some(vectors) = &engine.vectors
            && let Err(e) = vectors.delete(company, addr)
        {
            tracing::warn!(company, addr, error = %e, "[tinycortex] deleting vector row failed");
        }
        Ok(deleted)
    }

    async fn delete_chunk_label(&self, company: &str, addr: &str, label: &str) -> Result<bool> {
        let engine = self.engine(company).await?;
        // Serialize against concurrent puts of the same address: the reap
        // decision (last label gone → record goes) must see every claim a
        // committed put landed (#1300).
        let _guard = engine.write_lock.lock().await;
        let key = format!("{KEY_CHUNK_PREFIX}{addr}");
        let Some(chunk) = engine.get_json::<StoredChunk>(&key)? else {
            return Ok(false);
        };
        let mut labels = labels_of(&chunk);
        let before = labels.len();
        labels.retain(|have| have != label);
        if labels.len() == before {
            return Ok(false);
        }
        if labels.is_empty() {
            // Last claim gone: reap the record and its vector row, exactly as
            // `hard_delete_chunk` does.
            engine
                .kv
                .delete_global(&key)
                .map_err(|e| OpenCompanyError::Store(format!("kv delete {key}: {e}")))?;
            if let Some(vectors) = &engine.vectors
                && let Err(e) = vectors.delete(company, addr)
            {
                tracing::warn!(company, addr, error = %e, "[tinycortex] deleting vector row failed");
            }
            return Ok(true);
        }
        let updated = StoredChunk {
            // The scalar stays a real label so a pre-`labels` binary keeps
            // reading a claim that still exists.
            label: labels[0].clone(),
            labels,
            body: chunk.body,
            stored_at_millis: chunk.stored_at_millis,
        };
        engine.put_json(&key, &updated)?;
        Ok(true)
    }

    async fn redact(&self, company: &str, needle: &str, replacement: &str) -> Result<u64> {
        if needle.is_empty() {
            return Ok(0);
        }
        let engine = self.engine(company).await?;
        // Serialize the sweep-and-rewrite over traces, archive, and chunks against
        // concurrent trace/chunk mutations for this company.
        let _guard = engine.write_lock.lock().await;
        let mut replaced = 0u64;

        let mut traces: Vec<CompressedTrace> = engine.get_json(KEY_TRACES)?.unwrap_or_default();
        let mut archive: Vec<CompressedTrace> = engine.get_json(KEY_ARCHIVE)?.unwrap_or_default();
        for trace in traces.iter_mut().chain(archive.iter_mut()) {
            replaced += replace_in_place(&mut trace.summary, needle, replacement);
        }
        engine.put_json(KEY_TRACES, &traces)?;
        engine.put_json(KEY_ARCHIVE, &archive)?;

        for (addr, mut chunk) in engine.all_chunks()? {
            let hits = replace_in_place(&mut chunk.body, needle, replacement);
            if hits > 0 {
                replaced += hits;
                engine.put_json(&format!("{KEY_CHUNK_PREFIX}{addr}"), &chunk)?;
            }
        }
        Ok(replaced)
    }
}

// ---------------------------------------------------------------------------
// Injection helper
// ---------------------------------------------------------------------------

/// Builds a [`MemoryStore`](crate::ports::memory::MemoryStore) +
/// [`ContextStore`](crate::ports::context::ContextStore) pair over one shared
/// [`EngineCortex`] rooted at `memory_root`, so both ports read and write the
/// same persistent per-company engine databases.
///
/// This is the injection shape [`open_tinycortex`](crate::store::select) uses
/// when a data directory is present: feed the returned stores into
/// `RuntimeBuilder::with_memory_overlay`.
pub fn engine(
    memory_root: impl Into<PathBuf>,
) -> (Arc<CortexMemoryStore>, Arc<CortexContextStore>) {
    engine_with_embeddings(memory_root, None)
}

/// Like [`engine`], but injects `embeddings` as the meaning tier (188c2). `None`
/// is byte-identical to [`engine`] (pure lexical recall). This is the shape
/// [`open_tinycortex`](crate::store::select) uses when it can resolve a hosted
/// embeddings backend from the environment.
pub fn engine_with_embeddings(
    memory_root: impl Into<PathBuf>,
    embeddings: Option<Arc<dyn EmbeddingBackend>>,
) -> (Arc<CortexMemoryStore>, Arc<CortexContextStore>) {
    let client: Arc<dyn CortexClient> =
        Arc::new(EngineCortex::with_embeddings(memory_root, embeddings));
    (
        Arc::new(CortexMemoryStore::new(client.clone())),
        Arc::new(CortexContextStore::new(client)),
    )
}

// ---------------------------------------------------------------------------
// Two-tier hit merge (vector recall + lexical top-up)
// ---------------------------------------------------------------------------

/// Merges a vector-store result set with the lexical scorer into the final
/// [`ChunkHit`] list, capped at `limit`.
///
/// Vector hits come first (already sorted by descending cosine). Each is mapped
/// back through the KV `chunks` for its snippet, and a vector row whose KV chunk
/// no longer exists (a dangling id) is pruned. The list is then topped up with
/// lexical hits, skipping any address the vector half already carried, so an
/// address never appears twice. Cosine scores are clamped to the `[0, 1]`
/// [`ChunkHit::score`] contract.
fn merge_hits(
    results: Vec<SearchResult>,
    chunks: &[(String, StoredChunk)],
    query: &str,
    limit: usize,
) -> Vec<ChunkHit> {
    // Build the addr → chunk index once so each vector result is an O(1)
    // lookup instead of a linear scan over `chunks` (was O(results × chunks)).
    let by_addr: HashMap<&str, &StoredChunk> = chunks
        .iter()
        .map(|(addr, chunk)| (addr.as_str(), chunk))
        .collect();
    let mut hits: Vec<ChunkHit> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for result in results {
        if hits.len() >= limit {
            break;
        }
        // Prune dangling ids: a vector row with no live KV chunk is skipped.
        let Some(chunk) = by_addr.get(result.id.as_str()).copied() else {
            continue;
        };
        if !seen.insert(result.id.clone()) {
            continue;
        }
        hits.push(ChunkHit {
            addr: ChunkAddr::new(result.id.clone()),
            snippet: semantic_snippet(&chunk.body, query),
            score: result.score.clamp(0.0, 1.0),
        });
    }
    if hits.len() < limit {
        for hit in score_chunks(chunks, query) {
            if hits.len() >= limit {
                break;
            }
            if seen.contains(hit.addr.as_ref()) {
                continue;
            }
            hits.push(hit);
        }
    }
    hits
}

/// A snippet for a semantic hit: anchored on the first matching query term when
/// one is lexically present, else a char-boundary-safe leading window (a
/// semantic match need not share any surface token with the query).
fn semantic_snippet(body: &str, query: &str) -> String {
    let body_lower = body.to_lowercase();
    let anchor = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|t| !t.is_empty())
        .filter_map(|t| body_lower.find(&t).map(|p| (p, t.len())))
        .min_by_key(|(p, _)| *p);
    match anchor {
        Some((p, len)) => snippet_around(body, p, len),
        None => leading_snippet(body),
    }
}

/// A char-boundary-safe leading window of `body` (~48 chars), for a semantic hit
/// with no lexical anchor.
fn leading_snippet(body: &str) -> String {
    crate::store::text::slice_on_char_boundaries(body, 0..48)
}

// ---------------------------------------------------------------------------
// Degraded lexical scoring (mirrors InMemoryCortex)
// ---------------------------------------------------------------------------

/// A stable 64-bit FNV-1a hash of `s`, hex-encoded (16 lowercase digits).
///
/// Used to derive the injective suffix of a company's [`workspace_name`]. FNV-1a
/// is chosen deliberately over [`std::hash::DefaultHasher`]: its algorithm is
/// fixed by the two constants below, so a given id hashes identically across Rust
/// versions and process restarts. That determinism is a durability requirement —
/// a company's workspace directory must not move (and orphan its SQLite DB) just
/// because the binary was rebuilt with a newer toolchain.
///
/// [`workspace_name`]: EngineCortex::workspace_name
fn stable_hash_hex(s: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// Replaces every occurrence of `needle` in `s`, returning the count replaced.
fn replace_in_place(s: &mut String, needle: &str, replacement: &str) -> u64 {
    let count = s.matches(needle).count() as u64;
    if count > 0 {
        *s = s.replace(needle, replacement);
    }
    count
}

/// Ranks chunks by distinct-query-token overlap against their bodies, best
/// first, dropping zero-overlap chunks. Lexical and offline — the degraded-mode
/// recall this slice ships; semantic recall lands with embeddings in 188c2.
fn score_chunks(chunks: &[(String, StoredChunk)], query: &str) -> Vec<ChunkHit> {
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
    for (addr, chunk) in chunks {
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
            addr: ChunkAddr::new(addr.clone()),
            snippet,
            score,
        });
    }
    scored.sort_by(|a, b| b.score.total_cmp(&a.score));
    scored
}

/// Extracts a char-boundary-safe window around `pos` of a matched term.
///
/// Cuts the window with the same shared helper every other `ContextStore`
/// backend uses, so identical content snippets identically whichever store
/// answered. This copy used to *ceil* the start where the shared helper
/// *floors* it, which dropped the leading character of a non-ASCII window here
/// and nowhere else — a difference no `.contains(term)` assertion can see.
fn snippet_around(body: &str, pos: usize, term_len: usize) -> String {
    crate::store::text::slice_on_char_boundaries(body, pos.saturating_sub(24)..pos + term_len + 24)
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::Path;

    use crate::ports::context::ContextStore;
    use crate::ports::events::EventLog;
    use crate::ports::memory::MemoryStore;
    use crate::ports::store::CompanyStore;
    use crate::ports::types::CompanyId;
    use crate::store::conformance;
    use crate::store::{FsCompanyStore, FsEventLog};

    /// The four port trait objects the conformance suite drives: fs company and
    /// event stores paired with the engine-backed cortex memory + context stores.
    type ConformanceStores = (
        Arc<dyn CompanyStore>,
        Arc<dyn EventLog>,
        Arc<CortexMemoryStore>,
        Arc<CortexContextStore>,
    );

    /// Builds engine-backed cortex memory+context stores rooted under a tempdir,
    /// plus fs company/event stores (over the same tempdir) for the two
    /// conformance slots the cortex backend does not implement.
    fn stores(dir: &Path) -> ConformanceStores {
        let (mem, ctx) = engine(dir.join("memory"));
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

    #[tokio::test]
    async fn conformance_context_chunk_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let (_store, _events, _mem, ctx) = stores(dir.path());
        conformance::assert_context_chunk_stamps(ctx).await;
    }

    #[tokio::test]
    async fn conformance_context_identical_body_two_labels() {
        let dir = tempfile::tempdir().unwrap();
        let (_store, _events, _mem, ctx) = stores(dir.path());
        conformance::assert_identical_body_two_labels(ctx).await;
    }

    #[tokio::test]
    async fn conformance_context_delete_label_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let (_store, _events, _mem, ctx) = stores(dir.path());
        conformance::assert_delete_label_scoped(ctx).await;
    }

    #[tokio::test]
    async fn conformance_context_delete_label_survives_a_concurrent_identical_put() {
        let dir = tempfile::tempdir().unwrap();
        let (_store, _events, _mem, ctx) = stores(dir.path());
        conformance::assert_delete_label_survives_a_concurrent_identical_put(ctx).await;
    }

    /// A chunk record persisted before the `labels` set existed (scalar label
    /// only) keeps its claim through the union read, folds a second label in
    /// beside it, and stays label-deletable — the mixed-version story the
    /// serde default alone does not prove.
    #[tokio::test]
    async fn legacy_scalar_label_records_fold_and_label_delete() {
        let dir = tempfile::tempdir().unwrap();
        let cortex = EngineCortex::new(dir.path().to_path_buf());
        let body = "written before the labels set";
        let addr = crate::store::content_address(body);
        // Seed the pre-#1300 record shape directly at the KV tier — as a
        // structured value, the same rule `put_json` documents.
        {
            let engine = cortex.engine("acme").await.unwrap();
            engine
                .kv
                .set_global(
                    &format!("{KEY_CHUNK_PREFIX}{addr}"),
                    &serde_json::json!({
                        "label": "agent/ceo",
                        "body": body,
                        "stored_at_millis": 7,
                    }),
                )
                .unwrap();
        }

        let labels =
            |metas: Vec<ChunkMeta>| -> Vec<String> { metas.into_iter().map(|m| m.label).collect() };
        assert_eq!(
            labels(cortex.list_chunks("acme", "").await.unwrap()),
            ["agent/ceo"],
            "the scalar label is a claim"
        );

        cortex
            .put_chunk(
                "acme",
                &addr,
                ContextChunk {
                    label: "agent/ops".to_string(),
                    body: body.to_string(),
                },
            )
            .await
            .unwrap();
        let mut both = labels(cortex.list_chunks("acme", "").await.unwrap());
        both.sort();
        assert_eq!(both, ["agent/ceo", "agent/ops"]);

        assert!(
            cortex
                .delete_chunk_label("acme", &addr, "agent/ceo")
                .await
                .unwrap()
        );
        assert_eq!(
            labels(cortex.list_chunks("acme", "").await.unwrap()),
            ["agent/ops"]
        );
        assert_eq!(
            cortex.peek_chunk("acme", &addr).await.unwrap().as_deref(),
            Some(body),
            "the body survives under the remaining claim"
        );
        assert!(
            cortex
                .delete_chunk_label("acme", &addr, "agent/ops")
                .await
                .unwrap()
        );
        assert_eq!(
            cortex.peek_chunk("acme", &addr).await.unwrap(),
            None,
            "the record goes with its last claim"
        );
    }

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    /// `workspace_name` is injective and path-safe: ids whose sanitized prefixes
    /// collide (`acme:1`, `acme/1`, `acme_1`) still map to DISTINCT directories,
    /// so distinct companies never share a workspace/DB — and the mapping is
    /// stable across calls (a company's directory must not move across restarts).
    #[test]
    fn workspace_name_is_injective_and_stable() {
        let colon = EngineCortex::workspace_name("acme:1");
        let slash = EngineCortex::workspace_name("acme/1");
        let under = EngineCortex::workspace_name("acme_1"); // the literal id

        // The three distinct ids get three distinct directories, even though a
        // sanitize-only scheme would collapse all of them onto `acme_1`.
        assert_ne!(colon, slash);
        assert_ne!(colon, under);
        assert_ne!(slash, under);

        // Stable across calls for the same id (durability depends on it).
        assert_eq!(colon, EngineCortex::workspace_name("acme:1"));
        assert_eq!(under, EngineCortex::workspace_name("acme_1"));

        // Every produced name is a single path-safe segment.
        for name in [&colon, &slash, &under] {
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
                "workspace name must be path-safe: {name}"
            );
        }

        // The empty id still yields a non-empty, path-safe name.
        let empty = EngineCortex::workspace_name("");
        assert!(!empty.is_empty());
    }

    /// Data written through one engine instance survives dropping it and
    /// reopening a fresh engine at the same root — the durability contract, plus
    /// a real on-disk SQLite artifact under the company workspace.
    #[tokio::test]
    async fn persistence_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        let id = company();

        let addr = {
            let (mem, ctx) = engine(root.clone());
            mem.save_trace(&id, CompressedTrace::now("c0", "the q3 report shipped"))
                .await
                .unwrap();
            ctx.put(
                &id,
                ContextChunk {
                    label: "notes/q3".into(),
                    body: "revenue grew in the q3 report".into(),
                },
            )
            .await
            .unwrap()
        };

        // The engine wrote a real on-disk database under the company workspace.
        let db = root
            .join(EngineCortex::workspace_name(id.as_ref()))
            .join("memory_tree")
            .join("chunks.db");
        assert!(
            db.exists(),
            "engine must persist a SQLite db on disk: {db:?}"
        );

        // A fresh engine at the same root recalls both the trace and the chunk.
        let (mem2, ctx2) = engine(root);
        let traces = mem2.recent_traces(&id, 10).await.unwrap();
        assert_eq!(traces.len(), 1, "trace survived reopen");
        assert_eq!(traces[0].cycle_id, "c0");
        let body = ctx2.peek(&id, &addr, None).await.unwrap();
        assert_eq!(
            body, "revenue grew in the q3 report",
            "chunk survived reopen"
        );
        let metas = ctx2.list(&id, "notes/").await.unwrap();
        assert_eq!(metas.len(), 1, "chunk metadata survived reopen");
    }

    /// Two companies never observe each other's data — physical isolation by
    /// separate per-company workspaces/databases.
    #[tokio::test]
    async fn two_company_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let (mem, ctx) = engine(dir.path().join("memory"));
        let alpha = CompanyId::new("alpha");
        let beta = CompanyId::new("beta");

        mem.save_trace(&alpha, CompressedTrace::now("c0", "alpha only"))
            .await
            .unwrap();
        ctx.put(
            &alpha,
            ContextChunk {
                label: "doc/a".into(),
                body: "alpha secret body".into(),
            },
        )
        .await
        .unwrap();

        // beta was never written: every port reads empty for it.
        assert!(mem.recent_traces(&beta, 10).await.unwrap().is_empty());
        assert!(ctx.list(&beta, "").await.unwrap().is_empty());
        assert!(
            ctx.search(&beta, "alpha secret", 10)
                .await
                .unwrap()
                .is_empty(),
            "cross-company recall must not bleed"
        );

        // alpha still sees its own data.
        assert_eq!(mem.recent_traces(&alpha, 10).await.unwrap().len(), 1);
        assert_eq!(ctx.list(&alpha, "").await.unwrap().len(), 1);
    }

    /// The overlay selector picks the persistent engine when a data dir is
    /// present, and the offline in-memory backend otherwise. Proven by
    /// durability: only the engine path recalls data across a fresh overlay.
    #[tokio::test]
    async fn overlay_selection_engine_when_data_dir_set() {
        use crate::store::{MemoryBackend, StorageSettings, open_memory_overlay};

        let dir = tempfile::tempdir().unwrap();
        let id = company();
        let settings = StorageSettings {
            memory_backend: MemoryBackend::Tinycortex,
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };

        // First overlay writes a chunk, then is dropped.
        let addr = {
            let overlay = open_memory_overlay(&settings).unwrap().expect("overlay");
            overlay
                .context
                .put(
                    &id,
                    ContextChunk {
                        label: "notes/q3".into(),
                        body: "revenue grew in the q3 report".into(),
                    },
                )
                .await
                .unwrap()
        };

        // A fresh overlay from the same settings recalls it — proving the engine
        // (not the ephemeral in-memory backend) was selected.
        let overlay2 = open_memory_overlay(&settings).unwrap().expect("overlay");
        let body = overlay2.context.peek(&id, &addr, None).await.unwrap();
        assert_eq!(body, "revenue grew in the q3 report");

        // With no data dir, the in-memory backend is selected: a fresh overlay
        // starts empty (nothing persisted across instances).
        let ephemeral = StorageSettings {
            memory_backend: MemoryBackend::Tinycortex,
            data_dir: None,
            ..Default::default()
        };
        let a = open_memory_overlay(&ephemeral).unwrap().expect("overlay");
        a.context
            .put(
                &id,
                ContextChunk {
                    label: "notes/x".into(),
                    body: "ephemeral".into(),
                },
            )
            .await
            .unwrap();
        let b = open_memory_overlay(&ephemeral).unwrap().expect("overlay");
        assert!(
            b.context.list(&id, "").await.unwrap().is_empty(),
            "in-memory backend must not persist across instances"
        );
    }

    /// Degraded-mode search ranks chunks by lexical token-overlap, dropping
    /// zero-overlap chunks and ordering better matches first — no embeddings.
    #[tokio::test]
    async fn degraded_search_ranks_lexically() {
        let dir = tempfile::tempdir().unwrap();
        let (_mem, ctx) = engine(dir.path().join("memory"));
        let id = company();

        for (label, body) in [
            ("doc/a", "quarterly revenue growth strategy"),
            ("doc/b", "revenue only"),
            ("doc/c", "unrelated note"),
        ] {
            ctx.put(
                &id,
                ContextChunk {
                    label: label.into(),
                    body: body.into(),
                },
            )
            .await
            .unwrap();
        }

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
    }

    #[tokio::test]
    async fn evict_archives_rather_than_destroys() {
        use crate::ports::types::EvictionPolicy;

        let dir = tempfile::tempdir().unwrap();
        let (mem, _ctx) = engine(dir.path().join("memory"));
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

        // Hard delete reaches the archive, not just the live set.
        assert!(mem.hard_delete_trace(&id, "c0").await.unwrap());
        assert!(!mem.hard_delete_trace(&id, "missing").await.unwrap());
    }

    /// Concurrent `append_trace` calls for one company never lose a write. The
    /// KV tier upserts the whole trace array per key, so without the per-company
    /// `write_lock` two racing appends both read N and both write N+1, dropping
    /// one. A multi-thread runtime with many concurrent appends over one shared
    /// [`EngineCortex`] asserts all of them land.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_append_trace_never_loses_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let cortex = Arc::new(EngineCortex::new(dir.path().join("memory")));
        let company = "acme";
        let n = 64usize;

        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let cortex = cortex.clone();
            handles.push(tokio::spawn(async move {
                cortex
                    .append_trace(
                        company,
                        CompressedTrace::now(format!("c{i}"), format!("s{i}")),
                    )
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let traces = cortex.recent_traces(company, n * 2).await.unwrap();
        assert_eq!(
            traces.len(),
            n,
            "every concurrent append must land under the per-company write lock"
        );
        // No trace was clobbered: all cycle ids c0..cN are present.
        let mut ids: Vec<String> = traces.into_iter().map(|t| t.cycle_id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(
            ids.len(),
            n,
            "each concurrent append persisted a distinct trace"
        );
    }

    #[tokio::test]
    async fn redact_rewrites_traces_and_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let (mem, ctx) = engine(dir.path().join("memory"));
        let id = company();

        mem.save_trace(&id, CompressedTrace::now("c0", "contact bob-9 noted"))
            .await
            .unwrap();
        let addr = ctx
            .put(
                &id,
                ContextChunk {
                    label: "doc/a".into(),
                    body: "bob-9 twice: bob-9".into(),
                },
            )
            .await
            .unwrap();

        let replaced = mem.redact_all(&id, "bob-9", "[redacted]").await.unwrap();
        assert_eq!(replaced, 3, "one in the trace, two in the chunk");
        let body = ctx.peek(&id, &addr, None).await.unwrap();
        assert!(!body.contains("bob-9"));
        assert!(body.contains("[redacted]"));
        let trace = mem.recent_traces(&id, 1).await.unwrap();
        assert!(!trace[0].summary.contains("bob-9"));
    }

    // -----------------------------------------------------------------------
    // Meaning tier (188c2): VectorStore-backed semantic recall
    // -----------------------------------------------------------------------

    use std::collections::HashMap as StdHashMap;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    /// A deterministic, offline embedding backend for engine tests. Crafted
    /// texts map to explicit vectors (so a test can force a semantic match with
    /// different surface wording); every other text hashes to a distinct unit
    /// vector on an axis `≥ 1`, keeping it near-orthogonal to the crafted axis 0.
    /// `failing` errors on every embed, modelling an embeddings outage.
    #[derive(Clone)]
    struct FakeEmbedding {
        dim: usize,
        table: StdHashMap<String, Vec<f32>>,
        failing: bool,
    }

    impl FakeEmbedding {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                table: StdHashMap::new(),
                failing: false,
            }
        }

        fn failing(dim: usize) -> Self {
            Self {
                dim,
                table: StdHashMap::new(),
                failing: true,
            }
        }

        /// Registers a crafted `text → vector` mapping.
        fn with(mut self, text: &str, vector: Vec<f32>) -> Self {
            self.table.insert(text.to_string(), vector);
            self
        }

        fn vector_for(&self, text: &str) -> Vec<f32> {
            if let Some(vector) = self.table.get(text) {
                return vector.clone();
            }
            let mut v = vec![0.0_f32; self.dim];
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            // Reserve axis 0 for crafted matches; unknowns land on axes ≥ 1.
            let idx = 1 + (hasher.finish() as usize % (self.dim - 1));
            v[idx] = 1.0;
            v
        }
    }

    #[async_trait]
    impl EmbeddingBackend for FakeEmbedding {
        fn name(&self) -> &str {
            "fake"
        }
        fn model_id(&self) -> &str {
            "fake"
        }
        fn dimensions(&self) -> usize {
            self.dim
        }
        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            if self.failing {
                anyhow::bail!("fake embedding outage");
            }
            Ok(texts.iter().map(|t| self.vector_for(t)).collect())
        }
    }

    /// A crafted unit vector: `1.0` on `axis`, `0.0` elsewhere.
    fn unit(axis: usize, dim: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; dim];
        v[axis] = 1.0;
        v
    }

    /// A stored chunk gets a real vector row, and a semantic query with *no*
    /// shared surface tokens still ranks the matching chunk first — proof the
    /// meaning tier (not lexical overlap) drove the result.
    #[tokio::test]
    async fn semantic_hit_outranks_lexical_nonmatch() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FakeEmbedding::new(8)
                .with(
                    "notes/finance\nquarterly earnings climbed sharply",
                    unit(0, 8),
                )
                .with("revenue growth", unit(0, 8))
                .with("notes/weather\ntoday will be sunny and warm", unit(1, 8)),
        );
        let (_mem, ctx) = engine_with_embeddings(dir.path().join("memory"), Some(backend));
        let id = company();

        let finance = ctx
            .put(
                &id,
                ContextChunk {
                    label: "notes/finance".into(),
                    body: "quarterly earnings climbed sharply".into(),
                },
            )
            .await
            .unwrap();
        ctx.put(
            &id,
            ContextChunk {
                label: "notes/weather".into(),
                body: "today will be sunny and warm".into(),
            },
        )
        .await
        .unwrap();

        // "revenue growth" shares no surface token with the finance body, so a
        // lexical scorer alone could never rank it first.
        let hits = ctx.search(&id, "revenue growth", 5).await.unwrap();
        assert!(!hits.is_empty(), "semantic recall found nothing");
        assert_eq!(hits[0].addr, finance, "semantic match must rank first");
    }

    /// With no embeddings backend the engine is byte-identical to the 188c1
    /// lexical path — a guard that `vectors = None` never changes degraded recall.
    #[tokio::test]
    async fn no_backend_stays_lexical() {
        let dir = tempfile::tempdir().unwrap();
        let (_mem, ctx) = engine_with_embeddings(dir.path().join("memory"), None);
        let id = company();
        for (label, body) in [
            ("doc/a", "quarterly revenue growth strategy"),
            ("doc/b", "unrelated weather note"),
        ] {
            ctx.put(
                &id,
                ContextChunk {
                    label: label.into(),
                    body: body.into(),
                },
            )
            .await
            .unwrap();
        }
        let hits = ctx.search(&id, "revenue growth", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "only the lexical overlap matches");
        assert!(hits[0].snippet.contains("revenue"));
    }

    /// An embeddings outage mid-store must not fail the write, and the chunk
    /// stays lexically findable (vector search also degrades to lexical).
    #[tokio::test]
    async fn embed_failure_keeps_chunk_lexically_findable() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakeEmbedding::failing(8));
        let (_mem, ctx) = engine_with_embeddings(dir.path().join("memory"), Some(backend));
        let id = company();

        // Store succeeds even though the embed call errors.
        ctx.put(
            &id,
            ContextChunk {
                label: "doc/a".into(),
                body: "revenue growth strategy".into(),
            },
        )
        .await
        .unwrap();

        // Query embed fails too → lexical fallback still finds it.
        let hits = ctx.search(&id, "revenue growth", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("revenue"));
    }

    /// A hard-deleted chunk drops both its KV row and its vector row, so it can
    /// never resurface through semantic recall.
    #[tokio::test]
    async fn hard_delete_removes_vector_row() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FakeEmbedding::new(8)
                .with("doc/secret\ntop secret alpha initiative", unit(0, 8))
                .with("alpha initiative", unit(0, 8)),
        );
        let (_mem, ctx) = engine_with_embeddings(dir.path().join("memory"), Some(backend));
        let id = company();

        let addr = ctx
            .put(
                &id,
                ContextChunk {
                    label: "doc/secret".into(),
                    body: "top secret alpha initiative".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            ctx.search(&id, "alpha initiative", 5)
                .await
                .unwrap()
                .iter()
                .any(|h| h.addr == addr),
            "chunk must be semantically recallable before deletion"
        );

        assert!(ctx.hard_delete_chunk(&id, &addr).await.unwrap());

        let after = ctx.search(&id, "alpha initiative", 5).await.unwrap();
        assert!(
            after.is_empty(),
            "deleted chunk must not resurface: {after:?}"
        );
    }

    /// A chunk written in lexical-only mode (188c1) is embedded on the next open
    /// with a backend present — the migration backfill — so semantic recall with
    /// different wording then finds it.
    #[tokio::test]
    async fn backfill_embeds_preexisting_chunks_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        let id = company();

        // 188c1-style: lexical-only engine writes a chunk, no vectors.
        {
            let (_mem, ctx) = engine_with_embeddings(root.clone(), None);
            ctx.put(
                &id,
                ContextChunk {
                    label: "doc/a".into(),
                    body: "quarterly earnings climbed".into(),
                },
            )
            .await
            .unwrap();
        }

        // Reopen WITH a backend → first touch backfills the pre-existing chunk.
        let backend = Arc::new(
            FakeEmbedding::new(8)
                .with("doc/a\nquarterly earnings climbed", unit(0, 8))
                .with("revenue", unit(0, 8)),
        );
        let (_mem, ctx) = engine_with_embeddings(root, Some(backend));

        // "revenue" is absent from the body, so only the backfilled vector can
        // surface it.
        let hits = ctx.search(&id, "revenue", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "backfill must make the old chunk semantic");
    }

    /// The two-tier merge never exceeds the limit and never repeats an address,
    /// even when a chunk matches both the vector and lexical halves.
    #[tokio::test]
    async fn merge_respects_limit_without_duplicate_addrs() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FakeEmbedding::new(8)
                .with("doc/a\nrevenue growth strategy", unit(0, 8))
                .with("doc/b\nrevenue only", unit(1, 8))
                .with("revenue growth", unit(0, 8)),
        );
        let (_mem, ctx) = engine_with_embeddings(dir.path().join("memory"), Some(backend));
        let id = company();
        ctx.put(
            &id,
            ContextChunk {
                label: "doc/a".into(),
                body: "revenue growth strategy".into(),
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

        let hits = ctx.search(&id, "revenue growth", 5).await.unwrap();
        let addrs: HashSet<ChunkAddr> = hits.iter().map(|h| h.addr.clone()).collect();
        assert_eq!(addrs.len(), hits.len(), "no address may appear twice");

        let one = ctx.search(&id, "revenue growth", 1).await.unwrap();
        assert_eq!(one.len(), 1, "the limit is respected across the merge");
    }
}
