//! MongoDB-backed implementations of the storage ports.
//!
//! One [`MongoStore`] wraps a single [`mongodb::Database`] and implements
//! every durable port — [`CompanyStore`], [`EventLog`], [`MemoryStore`],
//! [`ContextStore`], and [`SecretStore`] — so the same `Arc<MongoStore>` can
//! be injected into all of the `RuntimeBuilder::with_*` setters.
//!
//! ## Multi-tenancy
//!
//! This is the platform backend for hosting many companies on one shared
//! MongoDB cluster (the same cluster the platform's Node backend uses for
//! users/teams/billing). Isolation is layered:
//!
//! - **Database per tenant** (recommended): the hosting layer points each
//!   tenant workload at its own database (`OPENCOMPANY_MONGODB_DB`, e.g.
//!   `oc-<tenant-slug>`), so tenants can never address each other's data and
//!   per-tenant export/drop is a database-level operation.
//! - **Company scoping inside a database**: mirroring the sqlite backend,
//!   every document carries `company_id` and every query filters on it, so a
//!   single database can also host multiple companies (platform mode). The
//!   `owners` collection additionally records the durable company → tenant
//!   mapping for shared-database deployments.
//!
//! ## Semantics
//!
//! Payloads are stored as the same JSON strings the fs/sqlite backends
//! persist, so records round-trip byte-identically across backends and the
//! export/import bundle path works unchanged. Monotonic 0-based sequences are
//! allocated from a `counters` collection with an atomic
//! `findOneAndUpdate {$inc}` per `(company, kind)` key.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::stream::TryStreamExt;
use mongodb::bson::{Document, doc};
use mongodb::options::{
    CollectionOptions, FindOneAndUpdateOptions, IndexOptions, ReturnDocument, UpdateOptions,
    WriteConcern,
};
use mongodb::{Client, Collection, Database, IndexModel};
use tokio::sync::broadcast;

use crate::Result;
use crate::company::CompanyManifest;
use crate::error::OpenCompanyError;
use crate::ports::context::ContextStore;
use crate::ports::events::{EventLog, EventStreamItem, PruneReport, RetentionPolicy, plan_prune};
use crate::ports::login_codes::LoginCodeRecord;
use crate::ports::memory::MemoryStore;
use crate::ports::now_millis;
use crate::ports::secrets::SecretStore;
use crate::ports::sessions::SessionRecord;
use crate::ports::store::CompanyStore;
use crate::ports::types::{
    ChunkAddr, ChunkHit, ChunkMeta, CompanyEvent, CompanyId, CompanyRecord, CompanySummary,
    CompressedTrace, ContextChunk, EventSeq, EvictionPolicy, LedgerEntry, OverlayBlob, SecretValue,
    StoredEvent, TaskResult,
};
use crate::ports::users::{InviteRecord, UserRecord};
use crate::store::content_address;
use crate::store::text::slice_on_char_boundaries;

fn mongo_err(e: impl std::fmt::Display) -> OpenCompanyError {
    OpenCompanyError::Store(format!("mongodb error: {e}"))
}

/// Whether the driver classified this error as worth trying again.
///
/// Asks the driver rather than matching on the message, because the driver is
/// the thing that knows: it attaches these labels from the server's own reply
/// and its view of the topology. `SystemOverloadedError` is a shared cluster
/// asking to be called back shortly, which is precisely the case that used to
/// kill a booting tenant (#1716).
#[cfg(feature = "mongodb")]
fn is_retryable(err: &mongodb::error::Error) -> bool {
    use mongodb::error::{RETRYABLE_ERROR, RETRYABLE_WRITE_ERROR, SYSTEM_OVERLOADED_ERROR};
    err.contains_label(RETRYABLE_ERROR)
        || err.contains_label(RETRYABLE_WRITE_ERROR)
        || err.contains_label(SYSTEM_OVERLOADED_ERROR)
}

/// How a port-contract limit maps onto a MongoDB `find`.
///
/// An enum with a distinct `Empty` arm, rather than the `Option<i64>` this used
/// to be, because the two vocabularies disagree about ZERO and the disagreement
/// is silent. `find().limit(0)` is MongoDB's *no limit* sentinel; every port in
/// `crate::ports` means "an empty page" by a limit of zero, as the fs and sqlite
/// backends implement it. Passing the number straight through therefore returned
/// the WHOLE collection at the exact input where the caller asked for nothing.
///
/// Issue #555: that is how `list_runs` drifted from the other two backends, and
/// it went unseen because `conformance.rs` — which asserts precisely this case —
/// had never run against MongoDB in CI. `read_events_from`, `recent_traces` and
/// `list_inbox` carried the same latent inversion; only the eviction path
/// happened to guard it with an `if n > 0`.
///
/// So the zero case is a variant the compiler makes every call site, present and
/// future, decide about — it cannot be forgotten into the driver's meaning again.
enum FindLimit {
    /// The caller asked for nothing. MUST NOT reach `find().limit()`.
    Empty,
    /// The port contract's "read everything" sentinel (`usize::MAX`), or any
    /// value too large for the driver's `i64`.
    Unlimited,
    /// At most this many documents.
    AtMost(i64),
}

fn find_limit(limit: usize) -> FindLimit {
    match limit {
        0 => FindLimit::Empty,
        n if n > i64::MAX as usize => FindLimit::Unlimited,
        n => FindLimit::AtMost(n as i64),
    }
}

/// Saturates a `u64` byte cap to Mongo's bindable `i64` range, so a cap
/// above `i64::MAX` compares as effectively unlimited instead of wrapping
/// negative.
fn clamp_max_bytes(max_bytes: u64) -> i64 {
    max_bytes.min(i64::MAX as u64) as i64
}

fn get_str(doc: &Document, key: &str) -> Result<String> {
    doc.get_str(key)
        .map(str::to_owned)
        .map_err(|e| mongo_err(format!("missing field {key}: {e}")))
}

fn get_i64(doc: &Document, key: &str) -> Result<i64> {
    doc.get_i64(key)
        .map_err(|e| mongo_err(format!("missing field {key}: {e}")))
}

/// Every label claiming a context document (issue #1300): the `labels` set,
/// plus the legacy scalar `label` field — documents written before the set
/// existed carry only the scalar, and new writes keep it as the first label so
/// a downgraded binary still reads what it always read. Deduped, scalar first.
/// Wraps an array-valued aggregation expression in a `$reduce` that drops
/// duplicates while **preserving order** — `$setUnion` dedupes but reorders,
/// and the context-chunk label list is read in order (the first claim is the
/// one the legacy scalar `label` names).
fn dedupe_preserving_order(input: Document) -> Document {
    doc! {"$reduce": {
        "input": input,
        "initialValue": [],
        "in": {"$cond": [
            {"$in": ["$$this", "$$value"]},
            "$$value",
            {"$concatArrays": ["$$value", ["$$this"]]},
        ]},
    }}
}

fn doc_labels(doc: &Document) -> Vec<String> {
    let mut labels = Vec::new();
    if let Ok(scalar) = doc.get_str("label") {
        labels.push(scalar.to_string());
    }
    if let Ok(set) = doc.get_array("labels") {
        for value in set {
            if let Some(label) = value.as_str()
                && !labels.iter().any(|have| have == label)
            {
                labels.push(label.to_string());
            }
        }
    }
    labels
}

/// A unique index restricted to the documents that carry `present` at all
/// (issue #697).
///
/// MongoDB is the one backend with no transaction to decide a first publish
/// under, so the uniqueness of a file's path has to be the database's own
/// invariant rather than a read followed by a write.
///
/// PARTIAL, and that is what makes it safe to add to a live tenant. A plain
/// unique index is built over every existing document, so a company that
/// already lost this race — carrying the duplicate pair issue #697 is about —
/// would fail index creation, and that failure happens during
/// [`MongoStore::ensure_indexes`], which means the store's startup goes down
/// for that tenant. Restricted to documents that *have* the field, the index
/// covers everything this code writes from now on and ignores history it cannot
/// repair. Legacy duplicates keep being caught one level up, where
/// `resolve_file` already answers them with `Conflict`.
///
/// # `present` is a runtime value, not a literal
///
/// `doc! {present: ...}` uses the **value** of `present`, not the string
/// `"present"`. The `doc!`/`bson!` key arms end at
/// `$object.insert::<_, Bson>(($($key)+), $value)` — the accumulated key tokens
/// are passed to `insert` as an *expression*, and `insert<KT: Into<String>>`
/// takes this `&str` by value. There is no `stringify!` anywhere on that path;
/// the macro only treats a key as a literal when one is written literally.
///
/// This is worth saying out loud because the failure it would cause is
/// invisible: a partial filter keyed on a field no document has would match
/// nothing, the index would be built over an empty set, and it would reject no
/// insert — silently removing the guarantee this function exists to provide,
/// while every test that does not actually race still passed.
/// `the_partial_filter_is_keyed_on_the_field_not_the_parameter_name` pins it.
fn unique_partial(keys: Document, present: &str) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(
            IndexOptions::builder()
                .unique(true)
                .partial_filter_expression(doc! {present: {"$exists": true}})
                .build(),
        )
        .build()
}

/// The uniqueness key for a **file's** path inside one company (issue #697).
///
/// Stored beside the node document so the partial unique index in
/// [`MongoStore::ensure_indexes`] can enforce "one file per path" as a database
/// invariant. That is what lets a first publish be a real compare-and-swap on
/// this backend: fs and SQLite decide the race under a lock and a transaction
/// respectively, and MongoDB has neither across two documents, so the insert
/// either violates the index or it does not.
///
/// Files only. Folders carry [`folder_path_key`] in a separate field instead
/// (issue #759), so the two guards never make a folder and a file contend for
/// one name. The key encoding is shared — see [`path_key`].
fn file_path_key(parent_id: Option<&str>, name: &str) -> String {
    path_key(parent_id, name)
}

/// The uniqueness key for a **folder's** path inside one company (issue #759).
///
/// The same `{parent}\0{name}` string as [`file_path_key`], stored in a
/// **different field** (`folder_path_key`) behind its own partial unique index.
/// Two fields rather than one shared key on purpose: a folder and a file are
/// still allowed to share a name, exactly as they were before either guard
/// existed. This is a race fix, not a new tree rule, and one key would quietly
/// make it the latter.
///
/// Stamped only by [`adopt_or_create_folder`], never by plain `create` — so the
/// console's own folder creation is unaffected and a legacy tenant carrying
/// duplicates keeps booting (the index is partial; see [`unique_partial`]).
///
/// [`adopt_or_create_folder`]: crate::ports::workspace::WorkspaceStore::adopt_or_create_folder
fn folder_path_key(parent_id: Option<&str>, name: &str) -> String {
    path_key(parent_id, name)
}

/// The shared `{parent}\0{name}` encoding behind both path keys.
///
/// NUL is the separator because [`reject_unsafe_name`](crate::store::fs_ops)
/// keeps it out of every node name, so no name can forge a different pair's key.
/// The root is the empty string; a node parented at the root and one parented
/// under a folder therefore never collide.
fn path_key(parent_id: Option<&str>, name: &str) -> String {
    format!("{}\u{0}{}", parent_id.unwrap_or(""), name)
}

/// The key a node should carry, or `None` when it is not a file.
fn node_path_key(node: &crate::ports::workspace::WorkspaceNode) -> Option<String> {
    (node.kind == crate::ports::workspace::NodeKind::File)
        .then(|| file_path_key(node.parent_id.as_deref(), &node.name))
}

/// The runtime journal's collection: one document per record (issue #726).
const JOURNAL: &str = "journal";

/// One document per company recording that its filesystem journal has been
/// imported — the gate that makes the import happen exactly once.
const JOURNAL_IMPORTS: &str = "journal_imports";

/// How many legacy run rows [`MongoStore::backfill_run_agent_ids`] migrates in
/// one pass. Bounds the memory a first boot's migration holds and gives the
/// server's own run writes room to interleave between passes.
const BACKFILL_BATCH_SIZE: usize = 200;

/// A single MongoDB database implementing all five storage ports.
#[derive(Clone)]
pub struct MongoStore {
    db: Database,
    senders: Arc<StdMutex<HashMap<CompanyId, broadcast::Sender<StoredEvent>>>>,
}

impl MongoStore {
    /// Connects to `uri` and opens `db_name`, creating the port indexes.
    pub async fn connect(uri: &str, db_name: &str) -> Result<Self> {
        let client = Client::with_uri_str(uri).await.map_err(mongo_err)?;
        Self::from_database(client.database(db_name)).await
    }

    /// Wraps an existing database handle (e.g. for tests), creating indexes.
    pub async fn from_database(db: Database) -> Result<Self> {
        let store = Self {
            db,
            senders: Arc::new(StdMutex::new(HashMap::new())),
        };
        store.ensure_indexes().await?;
        // Issue #1573: give run documents written before the `agent_id` mirror
        // existed one, so a per-teammate history is the whole history. Spawned
        // rather than awaited: on a first boot the migration can touch every
        // legacy run one row at a time, and storage initialization must not sit
        // in front of `/healthz` (AGENTS.md:170). The sqlite backend heals
        // synchronously because its migration is a single UPDATE; Mongo's desk
        // lives inside a JSON string, so there is no equivalent and the cost is
        // a bounded background churn. Best-effort for the same reason the
        // orphan-blob sweep is — a store that will not boot is worse than one
        // whose oldest run rows are not yet filterable by desk.
        let backfill_store = store.clone();
        tokio::spawn(async move {
            match backfill_store.backfill_run_agent_ids().await {
                Ok(0) => {}
                Ok(filled) => tracing::info!(
                    filled,
                    "backfilled the agent id on run rows written before the column existed"
                ),
                Err(err) => tracing::warn!(
                    error = %err,
                    "could not backfill run agent ids; a per-teammate run history will \
                     under-report until the next boot"
                ),
            }
        });
        // Reclaim workspace payloads whose node document never landed (issue
        // #553). Best-effort by design: the cost of skipping it is disk that is
        // already unreachable, and the cost of failing boot over it would be a
        // company that will not start. See `sweep_orphan_blobs`.
        match store.sweep_orphan_blobs().await {
            Ok(0) => {}
            Ok(removed) => tracing::info!(
                removed,
                "reclaimed orphaned workspace blobs left by an interrupted write"
            ),
            Err(err) => tracing::warn!(
                error = %err,
                "could not sweep orphaned workspace blobs; they remain until the next boot"
            ),
        }
        Ok(store)
    }

    /// Copies `agentId` out of `run_json` into the indexed `agent_id` field for
    /// every run document that predates it (issue #1573), returning how many
    /// were filled.
    ///
    /// The MongoDB half of sqlite's `heal_runs_agent_id`, and it exists for the
    /// same reason: a per-teammate run list that silently omitted every attempt
    /// written before the upgrade would read as "this teammate has never run",
    /// which is a wrong answer rather than a missing feature. Mongo cannot do
    /// this server-side — the desk lives inside a **string** of JSON, not a
    /// sub-document — so the rows are read back and rewritten one by one.
    ///
    /// The `$exists: false` probe finds rows written before the mirror column
    /// existed and matches nothing once the backfill has run — but a first
    /// boot can still hold a whole legacy collection's worth of work, so the
    /// migration is batched. Each pass resumes from the previous batch's last
    /// `_id`, so it never re-reads rows it has already migrated; each pass
    /// collects at most [`BACKFILL_BATCH_SIZE`] rows, keeps them in memory
    /// only long enough to fill them, then moves on. Bounded memory, and the
    /// server's own run writes interleave between passes. Best-effort at the
    /// call site for the same reason the orphan blob sweep is — a company
    /// that will not start is worse than one whose oldest run rows are not
    /// yet filterable by desk.
    async fn backfill_run_agent_ids(&self) -> Result<usize> {
        let runs = self.collection("runs");
        let mut filled = 0usize;
        // Resume where the last pass stopped. The probe has no leading
        // `company_id`, so no compound index can serve it — restarting the
        // scan per batch would re-examine every already-migrated row to reach
        // the next legacy one. `_id` is the one index every document has, and
        // the cursor advances in `_id` order, so the `$gt` bound is both cheap
        // and lossless: everything at or before the watermark has been filled.
        let mut after: Option<mongodb::bson::Bson> = None;
        loop {
            let mut filter = doc! {"agent_id": {"$exists": false}};
            if let Some(id) = after.clone() {
                filter.insert("_id", doc! {"$gt": id});
            }
            let mut cursor = runs
                .find(filter)
                .sort(doc! {"_id": 1})
                .limit(BACKFILL_BATCH_SIZE as i64)
                .await
                .map_err(mongo_err)?;
            let mut batch: Vec<(String, String, String)> = Vec::new();
            while let Some(document) = cursor.try_next().await.map_err(mongo_err)? {
                let record: crate::ports::runs::RunRecord =
                    serde_json::from_str(&get_str(&document, "run_json")?)?;
                batch.push((
                    get_str(&document, "company_id")?,
                    get_str(&document, "run_id")?,
                    record.agent_id,
                ));
                after = document.get("_id").cloned();
            }
            if batch.is_empty() {
                return Ok(filled);
            }
            for (company_id, run_id, agent_id) in &batch {
                runs.update_one(
                    doc! {"company_id": company_id.as_str(), "run_id": run_id.as_str()},
                    doc! {"$set": {"agent_id": agent_id.as_str()}},
                )
                .await
                .map_err(mongo_err)?;
            }
            filled += batch.len();
        }
    }

    /// Idempotent index creation — the MongoDB equivalent of the sqlite
    /// backend's `CREATE TABLE IF NOT EXISTS` migrations.
    async fn ensure_indexes(&self) -> Result<()> {
        let unique = |keys: Document| {
            IndexModel::builder()
                .keys(keys)
                .options(IndexOptions::builder().unique(true).build())
                .build()
        };
        // Not every index can be unique: a user holds many sessions, and an
        // address may have several login codes over time.
        let nonunique = |keys: Document| IndexModel::builder().keys(keys).build();
        // See `unique_partial`.
        let plans: [(&str, IndexModel); 37] = [
            ("companies", unique(doc! {"company_id": 1})),
            ("ledger", unique(doc! {"company_id": 1, "idx": 1})),
            ("events", unique(doc! {"company_id": 1, "seq": 1})),
            ("memory_traces", unique(doc! {"company_id": 1, "seq": 1})),
            ("memory_tasks", unique(doc! {"company_id": 1, "task_id": 1})),
            ("context_chunks", unique(doc! {"company_id": 1, "addr": 1})),
            ("secrets", unique(doc! {"company_id": 1, "key": 1})),
            ("inbox", unique(doc! {"company_id": 1, "seq": 1})),
            ("inbox_meta", unique(doc! {"company_id": 1, "key": 1})),
            ("tasks", unique(doc! {"company_id": 1, "task_id": 1})),
            ("facts", unique(doc! {"company_id": 1, "fact_id": 1})),
            ("ledger_specs", unique(doc! {"company_id": 1, "slug": 1})),
            // Not unique on the entry: many events fold into one row.
            // `seq` is the fold's ordering, so it is unique per company.
            ("ledger_events", unique(doc! {"company_id": 1, "seq": 1})),
            (
                "ledger_events",
                nonunique(doc! {"company_id": 1, "ledger": 1, "seq": 1}),
            ),
            ("usage_samples", unique(doc! {"company_id": 1, "seq": 1})),
            ("skill_state", unique(doc! {"company_id": 1, "slug": 1})),
            (
                "workspace_nodes",
                unique(doc! {"company_id": 1, "node_id": 1}),
            ),
            // One file per path. This is the compare-and-swap a first publish
            // wins or loses (issue #697); see `file_path_key`.
            (
                "workspace_nodes",
                unique_partial(doc! {"company_id": 1, "file_path_key": 1}, "file_path_key"),
            ),
            // One folder per path, for the folders the publish walk claims
            // (issue #759). A separate field from the file key on purpose; see
            // `folder_path_key`. Partial for the same boot-safety reason: a
            // tenant that already lost this race must keep starting.
            (
                "workspace_nodes",
                unique_partial(
                    doc! {"company_id": 1, "folder_path_key": 1},
                    "folder_path_key",
                ),
            ),
            ("users", unique(doc! {"company_id": 1, "user_id": 1})),
            // Enforces one account per address per company, and backs the login
            // lookup.
            ("users", unique(doc! {"company_id": 1, "email": 1})),
            (
                "user_invites",
                unique(doc! {"company_id": 1, "invite_id": 1}),
            ),
            ("user_invites", unique(doc! {"company_id": 1, "email": 1})),
            (
                "user_sessions",
                unique(doc! {"company_id": 1, "session_id": 1}),
            ),
            // Backs session resolution on every authenticated request.
            (
                "user_sessions",
                unique(doc! {"company_id": 1, "token_hash": 1}),
            ),
            (
                "user_sessions",
                nonunique(doc! {"company_id": 1, "user_id": 1}),
            ),
            ("login_codes", unique(doc! {"company_id": 1, "code_id": 1})),
            (
                "login_codes",
                unique(doc! {"company_id": 1, "code_hash": 1}),
            ),
            ("login_codes", nonunique(doc! {"company_id": 1, "email": 1})),
            ("runs", unique(doc! {"company_id": 1, "run_id": 1})),
            // A card has many attempts, and many attempts share a status.
            ("runs", nonunique(doc! {"company_id": 1, "task_id": 1})),
            // Issue #1573: and many attempts share a desk — the console's
            // per-teammate run history. See `backfill_run_agent_ids` for why
            // the field is on every document by the time this index matters.
            ("runs", nonunique(doc! {"company_id": 1, "agent_id": 1})),
            ("runs", nonunique(doc! {"company_id": 1, "status": 1})),
            // The workflow-run join: a node's attempt has no card and no
            // conversation, so this is the only handle on it.
            (
                "runs",
                nonunique(doc! {"company_id": 1, "workflow_run_id": 1}),
            ),
            (
                "run_steps",
                unique(doc! {"company_id": 1, "run_id": 1, "step_seq": 1}),
            ),
            // Issue #274: one row per snapshot; the compound index backs both the
            // per-workflow list (newest-first) and the prune.
            (
                "workflow_revisions",
                unique(doc! {"company_id": 1, "revision_id": 1}),
            ),
            (
                "workflow_revisions",
                nonunique(doc! {"company_id": 1, "workflow_id": 1, "created_ms": -1}),
            ),
        ];
        // Every index below is issued CONCURRENTLY rather than one at a time.
        //
        // Each `create_index` is a round trip, and against a remote cluster the
        // round trips are the whole cost: measured on staging (MongoDB Atlas),
        // boot spent ~3.5s in this function — most of an ~8s cold start — and it
        // was paid on every single boot to re-create indexes that already
        // existed.
        //
        // Safe to parallelise: index creation is independent per index and
        // idempotent, there is no ordering between any two of these, and
        // re-creating an existing index is a server-side no-op. Nothing here
        // reads what another one writes.
        //
        // Bounded rather than unbounded because the driver's connection pool is
        // finite (10 by default); more in flight than that would queue inside
        // the pool anyway, while making the burst look less deliberate than it
        // is.
        const INDEX_CONCURRENCY: usize = 10;

        /// How many times one index creation is retried before giving up.
        ///
        /// Only errors the driver itself labels retryable are retried; a wrong
        /// key spec fails on the first attempt as it always did.
        const INDEX_RETRIES: u32 = 4;

        /// Base backoff between retries, doubled each attempt: 100ms, 200ms,
        /// 400ms, 800ms — 1.5s in total, which is short against a 60s startup
        /// budget and long enough to outlast the load-shedding actually
        /// observed.
        const INDEX_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

        let extra: [(&str, IndexModel); 11] = [
            ("owners", unique(doc! {"company_id": 1})),
            // Issue #241: the cross-replica arbiter. This unique compound index
            // is what turns two replicas racing one schedule minute into one
            // winning `insert_one` and one `E11000` — the case that actually
            // matters, since hosted replicas share the tenant database.
            (
                "schedule_fires",
                unique(doc! {"company_id": 1, "schedule_id": 1, "scheduled_for": 1}),
            ),
            // Issue #596: one durable per-node output snapshot per run. The
            // unique `(company_id, run_id)` index makes the upsert
            // last-write-wins per run; the recency index backs the newest-N
            // prune.
            ("run_outputs", unique(doc! {"company_id": 1, "run_id": 1})),
            (
                "run_outputs",
                nonunique(doc! {"company_id": 1, "at_ms": -1}),
            ),
            // The unredacted step detail. `(company_id, run_id, step_seq)` is
            // unique because that pair IS the key — a reasoning run flushes
            // twice under the same ordinal and must converge rather than
            // duplicate. The recency index backs the newest-N-runs prune.
            (
                "run_step_details",
                unique(doc! {"company_id": 1, "run_id": 1, "step_seq": 1}),
            ),
            (
                "run_step_details",
                nonunique(doc! {"company_id": 1, "at_ms": -1}),
            ),
            // Issue #726: the runtime journal. `(company_id, seq)` is unique
            // because two records sharing a sequence would order arbitrarily on
            // replay, and an at-most-once key replayed before the resolution
            // that consumed it is the failure the journal exists to prevent. The
            // import receipt is one document per company.
            (JOURNAL, unique(doc! {"company_id": 1, "seq": 1})),
            (JOURNAL_IMPORTS, unique(doc! {"company_id": 1})),
            // Issue #749: durable notifications. The recency index backs the
            // newest-first feed `NotificationStore::list` returns.
            (
                "notifications",
                nonunique(doc! {"company_id": 1, "created_ms": -1, "id": -1}),
            ),
            // Unique per (company_id, id): makes `append` idempotent — a
            // duplicate id within a company is rejected, matching the sqlite
            // primary key.
            ("notifications", unique(doc! {"company_id": 1, "id": 1})),
            // The per-person read marker is unique on
            // `(company_id, user_id, notification_id)` so a re-mark is a no-op
            // `E11000` and read stays a latch.
            (
                "notification_reads",
                unique(doc! {"company_id": 1, "user_id": 1, "notification_id": 1}),
            ),
        ];

        // Scoped import: the file deliberately avoids a blanket `StreamExt`,
        // since `map` would then be ambiguous with `Iterator::map`.
        use futures::StreamExt as _;

        // Every operation is allowed to finish before any error is reported.
        //
        // `try_collect` would short-circuit, which matches what the old
        // sequential loop did — but only in appearance. Sequentially there was
        // never an operation in flight to abandon; here a short-circuit drops up
        // to `INDEX_CONCURRENCY - 1` futures mid-await, and a driver operation
        // cancelled after its request is sent but before its reply is read
        // leaves a connection the pool cannot safely reuse. That hazard is new,
        // introduced by the concurrency, so it is handled here rather than
        // inherited.
        //
        // Letting them all land is also simply better: a transient failure on
        // one index no longer decides whether the other forty-three exist.
        let results: Vec<Result<()>> = futures::stream::iter(plans.into_iter().chain(extra))
            .map(|(name, index)| async move {
                let mut backoff = INDEX_RETRY_BACKOFF;
                let mut attempt = 0;
                loop {
                    match self.collection(name).create_index(index.clone()).await {
                        Ok(_) => return Ok(()),
                        // The driver classifies its own errors, so retry exactly
                        // what it says is retryable rather than guessing from the
                        // message. `SystemOverloadedError` in particular is the
                        // server asking to be called back shortly — treating it
                        // as fatal is reading "try again" as "give up".
                        Err(err) if attempt < INDEX_RETRIES && is_retryable(&err) => {
                            tracing::warn!(
                                collection = name, attempt = attempt + 1, %err,
                                "index creation returned a retryable error; retrying"
                            );
                            tokio::time::sleep(backoff).await;
                            backoff *= 2;
                            attempt += 1;
                        }
                        Err(err) => {
                            return Err(mongo_err(format!("index on `{name}`: {err}")));
                        }
                    }
                }
            })
            .buffer_unordered(INDEX_CONCURRENCY)
            .collect()
            .await;

        let total = results.len();
        let failures: Vec<_> = results
            .into_iter()
            .filter_map(std::result::Result::err)
            .collect();
        let failed = failures.len();
        let Some(first) = failures.into_iter().next() else {
            return Ok(());
        };

        // An index that could not be created is LOUD but not fatal.
        //
        // This used to return `Err`, which failed the store's construction and
        // took the process down with it. Under Kubernetes that was a crash-loop
        // and the next attempt usually succeeded. Under the microVM runtime the
        // workload is PID 1, so its exit panics the guest kernel and the tenant
        // is simply gone — five of two hundred tenants died that way in one
        // ramp, because a shared Atlas cluster shed load while they all booted
        // at once (#1716).
        //
        // Indexes are a performance property, not a correctness precondition
        // for the process existing: every query here is correct without them,
        // only slower, and the next boot re-runs this. Refusing to start is the
        // strictly worse failure, and it is the same judgement this file already
        // makes one function below — "a store that will not boot is worse than
        // one whose oldest run rows are not yet filterable by desk".
        //
        // The count matters and is kept: one failing index is a different
        // problem from every index failing, and the first error alone cannot
        // tell them apart.
        if failed > 1 {
            tracing::error!(
                failed, total, first = %first,
                "some indexes could not be created; serving without them"
            );
        } else {
            tracing::error!(
                failed, total, error = %first,
                "an index could not be created; serving without it"
            );
        }
        Ok(())
    }

    fn collection(&self, name: &str) -> Collection<Document> {
        self.db.collection::<Document>(name)
    }

    /// A collection handle whose writes are acknowledged only once the server
    /// has committed them to its on-disk journal (`j:true`).
    ///
    /// Scoped to the runtime-journal collections rather than set database-wide,
    /// and within them to the records that ask for it
    /// ([`Durability::Host`](crate::ports::journal::Durability::Host)), because
    /// it is bought for one specific contract and costs a disk flush per write:
    /// the at-most-once guarantee is that an effect's key is durable *before* the
    /// side effect runs, and the default acknowledgement returns as soon as the
    /// primary has the write in memory — which a primary crash in the wrong
    /// millisecond loses, re-arming an effect that already fired.
    fn journaled(&self, name: &str) -> Collection<Document> {
        self.db.collection_with_options::<Document>(
            name,
            CollectionOptions::builder()
                .write_concern(WriteConcern::builder().journal(true).build())
                .build(),
        )
    }

    /// Atomically allocates the next 0-based sequence for `(company, kind)`.
    async fn next_seq(&self, id: &CompanyId, kind: &str) -> Result<u64> {
        let counters = self.collection("counters");
        let key = format!("{}:{kind}", id.as_ref());
        let doc = counters
            .find_one_and_update(doc! {"_id": &key}, doc! {"$inc": {"next": 1_i64}})
            .with_options(
                FindOneAndUpdateOptions::builder()
                    .upsert(true)
                    .return_document(ReturnDocument::Before)
                    .build(),
            )
            .await
            .map_err(mongo_err)?;
        // Before the first allocation there is no document: the seq is 0.
        Ok(doc.and_then(|d| d.get_i64("next").ok()).unwrap_or_default() as u64)
    }

    fn sender_for(&self, id: &CompanyId) -> broadcast::Sender<StoredEvent> {
        let mut map = self.senders.lock().expect("sender map poisoned");
        map.entry(id.clone())
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }

    // -- Durable tenant ownership (shared-database platform mode) ----------

    /// Records the owning tenant of a company. Used by platform mode to make
    /// the company → tenant map survive restarts (the in-memory `AppState`
    /// ownership map is hydrated from this at boot).
    pub async fn set_owner(&self, id: &CompanyId, tenant: &str) -> Result<()> {
        self.collection("owners")
            .update_one(
                doc! {"company_id": id.as_ref()},
                doc! {"$set": {"tenant_id": tenant, "updated_ms": now_millis() as i64}},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    /// Removes the ownership record (company deleted).
    pub async fn remove_owner(&self, id: &CompanyId) -> Result<()> {
        self.collection("owners")
            .delete_one(doc! {"company_id": id.as_ref()})
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    /// Every durable company → tenant mapping in this database.
    pub async fn owners(&self) -> Result<Vec<(CompanyId, String)>> {
        let mut cursor = self
            .collection("owners")
            .find(doc! {})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push((
                CompanyId::new(get_str(&doc, "company_id")?),
                get_str(&doc, "tenant_id")?,
            ));
        }
        Ok(out)
    }

    /// The shared body of `save` and `save_importing`: upserts the company
    /// document, stamping `overlay_json`'s `activation_gate_seen` with
    /// whatever the caller passes rather than always `true`. See
    /// `CompanyStore::save_importing`'s doc comment for why the two callers
    /// need different values.
    async fn save_gated(&self, record: &CompanyRecord, activation_gate_seen: bool) -> Result<()> {
        let manifest_toml = toml::to_string(&record.manifest)
            .map_err(|e| OpenCompanyError::Store(format!("cannot serialize manifest: {e}")))?;
        let overlay_json = serde_json::to_string(&OverlayBlob::from_record_gated(
            record,
            activation_gate_seen,
        ))?;
        // Append-only: `save` upserts the company document, never the ledger.
        self.collection("companies")
            .update_one(
                doc! {"company_id": record.id.as_ref()},
                doc! {"$set": {
                    "manifest_toml": manifest_toml,
                    "lifecycle": &record.lifecycle,
                    "overlay_json": overlay_json,
                    "updated_ms": now_millis() as i64,
                }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CompanyStore
// ---------------------------------------------------------------------------

#[async_trait]
impl CompanyStore for MongoStore {
    async fn load(&self, id: &CompanyId) -> Result<Option<CompanyRecord>> {
        let Some(company) = self
            .collection("companies")
            .find_one(doc! {"company_id": id.as_ref()})
            .await
            .map_err(mongo_err)?
        else {
            return Ok(None);
        };
        // `from_stored_toml` applies the global baseline — see
        // `CompanyManifest::apply_globals`.
        let manifest = CompanyManifest::from_stored_toml(&get_str(&company, "manifest_toml")?)
            .map_err(|e| OpenCompanyError::Store(format!("invalid company.toml: {e}")))?;

        let mut cursor = self
            .collection("ledger")
            .find(doc! {"company_id": id.as_ref()})
            .sort(doc! {"idx": 1})
            .await
            .map_err(mongo_err)?;
        let mut ledger = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            ledger.push(serde_json::from_str::<LedgerEntry>(&get_str(
                &doc,
                "entry_json",
            )?)?);
        }

        let overlay = match company.get_str("overlay_json") {
            Ok(json) => OverlayBlob::parse(json)?,
            Err(_) => OverlayBlob::default(),
        };
        Ok(Some(CompanyRecord {
            id: id.clone(),
            manifest,
            ledger,
            lifecycle: get_str(&company, "lifecycle")?,
            overlay_agents: overlay.agents,
            overlay_desk_members: overlay.desk_members,
            overlay_desk_order: overlay.desk_order,
            overlay_desks: overlay.desks,
            overlay_workflows: overlay.workflows,
            overlay_budgets: overlay.budgets,
            overlay_agent_edits: overlay.agent_edits,
            overlay_retired_agents: overlay.retired_agents,
            overlay_policy: overlay.policy,
            overlay_tool_grants: overlay.tool_grants,
            overlay_desk_tools: overlay.desk_tools,
            disabled_workflows: overlay.disabled_workflows,
            template_provenance: overlay.provenance,
            setup: overlay.setup,
            name_confirmed: overlay.name_confirmed,
            activation_completed_at: overlay.activation_completed_at,
            created_at_millis: overlay.created_at_millis,
        }))
    }

    async fn save(&self, record: &CompanyRecord) -> Result<()> {
        // Every ordinary `save` call against a `running` company is, by
        // definition, made by code that understands the activation funnel —
        // see `CompanyStore::activation_gate_seen`'s doc comment. That
        // reasoning does NOT extend to a `paused`/`archived` record still
        // waiting on its own first `running` boot to decide the marker (PR
        // #1875 review finding, second round): `RuntimeBuilder::build`'s
        // "existing but not running" arm carries the marker forward
        // untouched for exactly this reason, but a write that reaches this
        // method directly — bypassing `build` entirely, e.g.
        // `company_logo::put_logo`'s plain load-modify-save, which never
        // checks lifecycle — would stamp `true` regardless and poison the
        // grandfather arm's `!gate_already_seen` guard before the record's
        // own migration boot ever runs. So: stamp `true` only once the
        // record itself says `running`; otherwise preserve whatever is
        // already on file, same as `build`'s own "not running" arm does.
        if record.lifecycle == "running" {
            self.save_gated(record, true).await
        } else {
            let gate_seen = self.activation_gate_seen(&record.id).await?;
            self.save_gated(record, gate_seen).await
        }
    }

    async fn save_importing(&self, record: &CompanyRecord, gate_seen: bool) -> Result<()> {
        self.save_gated(record, gate_seen).await
    }

    /// PR #1875 review finding: the hosted platform selects this backend for
    /// tenant storage (`OPENCOMPANY_STORAGE=mongodb`), so inheriting the
    /// trait's always-`false` default here — rather than reading the same
    /// `overlay_json` field `save` above always stamps `activation_gate_seen:
    /// true` into (via `OverlayBlob::from_record`) — would let a genuinely
    /// new tenant's *second* boot go unnoticed as a legacy pre-#1843 record
    /// and get silently auto-activated, defeating the fix `save` above
    /// exists to carry. A company with no document at all reads `false`,
    /// matching `FsCompanyStore::activation_gate_seen`'s own "no bundle
    /// written yet" case.
    async fn activation_gate_seen(&self, id: &CompanyId) -> Result<bool> {
        let Some(company) = self
            .collection("companies")
            .find_one(doc! {"company_id": id.as_ref()})
            .await
            .map_err(mongo_err)?
        else {
            return Ok(false);
        };
        match company.get_str("overlay_json") {
            Ok(json) => Ok(OverlayBlob::parse(json)?.activation_gate_seen),
            Err(_) => Ok(false),
        }
    }

    async fn list(&self) -> Result<Vec<CompanySummary>> {
        let mut cursor = self
            .collection("companies")
            .find(doc! {})
            .sort(doc! {"company_id": 1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            let Ok(manifest) = toml::from_str::<CompanyManifest>(&get_str(&doc, "manifest_toml")?)
            else {
                continue;
            };
            out.push(CompanySummary {
                id: CompanyId::new(get_str(&doc, "company_id")?),
                name: manifest.company.name,
                lifecycle: get_str(&doc, "lifecycle")?,
            });
        }
        Ok(out)
    }

    async fn append_ledger(&self, id: &CompanyId, entry: LedgerEntry) -> Result<()> {
        let entry_json = serde_json::to_string(&entry)?;
        let idx = self.next_seq(id, "ledger").await?;
        self.collection("ledger")
            .insert_one(doc! {
                "company_id": id.as_ref(),
                "idx": idx as i64,
                "entry_json": entry_json,
                "at_ms": entry.at_millis as i64,
            })
            .await
            .map_err(mongo_err)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EventLog
// ---------------------------------------------------------------------------

#[async_trait]
impl EventLog for MongoStore {
    async fn append(&self, id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
        let event_json = serde_json::to_string(&event)?;
        let at_millis = now_millis();
        let seq = self.next_seq(id, "events").await?;
        self.collection("events")
            .insert_one(doc! {
                "company_id": id.as_ref(),
                "seq": seq as i64,
                "event_json": event_json,
                "at_ms": at_millis as i64,
            })
            .await
            .map_err(mongo_err)?;

        let stored = StoredEvent {
            seq: EventSeq::new(seq),
            company: id.clone(),
            event,
            at_millis,
        };
        // Best-effort fan-out; an error only means no live subscribers.
        let _ = self.sender_for(id).send(stored);
        Ok(EventSeq::new(seq))
    }

    async fn read_from(
        &self,
        id: &CompanyId,
        seq: EventSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let events = self.collection("events");
        let mut find = events
            .find(doc! {
                "company_id": id.as_ref(),
                "seq": {"$gte": seq.value() as i64},
            })
            .sort(doc! {"seq": 1});
        match find_limit(limit) {
            FindLimit::Empty => return Ok(Vec::new()),
            FindLimit::Unlimited => {}
            FindLimit::AtMost(n) => find = find.limit(n),
        }
        let mut cursor = find.await.map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(StoredEvent {
                seq: EventSeq::new(get_i64(&doc, "seq")? as u64),
                company: id.clone(),
                event: serde_json::from_str(&get_str(&doc, "event_json")?)?,
                at_millis: get_i64(&doc, "at_ms")? as u64,
            });
        }
        Ok(out)
    }

    async fn read_before(
        &self,
        id: &CompanyId,
        before: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let events = self.collection("events");
        let mut filter = doc! { "company_id": id.as_ref() };
        if let Some(cursor) = before {
            filter.insert("seq", doc! { "$lt": cursor.value() as i64 });
        }
        let mut find = events.find(filter).sort(doc! { "seq": -1 });
        match find_limit(limit) {
            FindLimit::Empty => return Ok(Vec::new()),
            FindLimit::Unlimited => {}
            FindLimit::AtMost(n) => find = find.limit(n),
        }
        let mut cursor = find.await.map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(StoredEvent {
                seq: EventSeq::new(get_i64(&doc, "seq")? as u64),
                company: id.clone(),
                event: serde_json::from_str(&get_str(&doc, "event_json")?)?,
                at_millis: get_i64(&doc, "at_ms")? as u64,
            });
        }
        Ok(out)
    }

    fn subscribe(&self, id: &CompanyId) -> BoxStream<'static, EventStreamItem> {
        let rx = self.sender_for(id).subscribe();
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            // Each call to this closure produces exactly one item and hands the
            // receiver back as continuation state, so there is no loop here.
            match rx.recv().await {
                Ok(event) => Some((EventStreamItem::Event(event), rx)),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    Some((EventStreamItem::Gap { missed }, rx))
                }
                Err(broadcast::error::RecvError::Closed) => None,
            }
        });
        Box::pin(stream)
    }

    async fn prune(&self, id: &CompanyId, policy: &RetentionPolicy) -> Result<PruneReport> {
        // Same whole-log read as the sqlite backend, and for the same reason:
        // the decision belongs to `plan_prune` so the three backends cannot
        // disagree about what a policy means.
        let all = self.read_from(id, EventSeq::new(0), usize::MAX).await?;
        let doomed = plan_prune(&all, policy);

        let mut report = PruneReport {
            scanned: all.len(),
            removed: 0,
            oldest_retained: all.iter().map(|e| e.seq).min(),
        };
        if doomed.is_empty() {
            return Ok(report);
        }

        let seqs: Vec<i64> = doomed.iter().map(|s| s.value() as i64).collect();
        self.collection("events")
            .delete_many(doc! {
                "company_id": id.as_ref(),
                "seq": {"$in": seqs},
            })
            .await
            .map_err(mongo_err)?;

        report.removed = doomed.len();
        report.oldest_retained = all
            .iter()
            .map(|e| e.seq)
            .filter(|seq| doomed.binary_search(seq).is_err())
            .min();
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

#[async_trait]
impl MemoryStore for MongoStore {
    async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()> {
        let trace_json = serde_json::to_string(&trace)?;
        let seq = self.next_seq(id, "memory_traces").await?;
        self.collection("memory_traces")
            .insert_one(doc! {
                "company_id": id.as_ref(),
                "seq": seq as i64,
                "trace_json": trace_json,
                "at_ms": trace.at_millis as i64,
            })
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn recent_traces(&self, id: &CompanyId, limit: usize) -> Result<Vec<CompressedTrace>> {
        let traces = self.collection("memory_traces");
        let mut find = traces
            .find(doc! {"company_id": id.as_ref()})
            .sort(doc! {"seq": -1});
        match find_limit(limit) {
            FindLimit::Empty => return Ok(Vec::new()),
            FindLimit::Unlimited => {}
            FindLimit::AtMost(n) => find = find.limit(n),
        }
        let mut cursor = find.await.map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str::<CompressedTrace>(&get_str(
                &doc,
                "trace_json",
            )?)?);
        }
        // Query returned newest-first; the port contract is newest-last.
        out.reverse();
        Ok(out)
    }

    async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()> {
        let result_json = serde_json::to_string(&result)?;
        self.collection("memory_tasks")
            .update_one(
                doc! {"company_id": id.as_ref(), "task_id": &result.task_id},
                doc! {"$set": {
                    "result_json": result_json,
                    "at_ms": now_millis() as i64,
                }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64> {
        let traces = self.collection("memory_traces");
        let removed = match policy {
            EvictionPolicy::KeepRecent { n } => {
                // `KeepRecent { n: 0 }` keeps nothing, so there is no query to
                // run — and must never become `find().limit(0)`, which would
                // keep EVERYTHING and evict none of it. This arm is the old
                // `if n > 0` guard, now stated in the shared vocabulary.
                match find_limit(n) {
                    FindLimit::Empty => {
                        traces
                            .delete_many(doc! {"company_id": id.as_ref()})
                            .await
                            .map_err(mongo_err)?
                            .deleted_count
                    }
                    limit => {
                        let mut find = traces
                            .find(doc! {"company_id": id.as_ref()})
                            .sort(doc! {"seq": -1});
                        if let FindLimit::AtMost(n) = limit {
                            find = find.limit(n);
                        }
                        let mut cursor = find.await.map_err(mongo_err)?;
                        // The n-th newest seq is the eviction cutoff, and the
                        // delete is `seq < cutoff` rather than `$nin` of the
                        // snapshot. `next_seq` hands out strictly increasing
                        // sequences per company, so a trace saved AFTER this
                        // read has a seq larger than every seq seen here and
                        // can never satisfy `seq < cutoff`. The `$nin` form
                        // deleted any doc whose seq was not in the snapshot, so
                        // a `save_trace` landing between the find and the
                        // delete was evicted the same pass it was written — and
                        // the sweep runs every minute, silently dropping the
                        // newest completed cycle from inspection and export.
                        let mut cutoff: Option<i64> = None;
                        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
                            let seq = get_i64(&doc, "seq")?;
                            cutoff = Some(cutoff.map_or(seq, |smallest| smallest.min(seq)));
                        }
                        let Some(cutoff) = cutoff else {
                            return Ok(0); // the company has no traces to evict
                        };
                        traces
                            .delete_many(doc! {
                                "company_id": id.as_ref(),
                                "seq": {"$lt": cutoff},
                            })
                            .await
                            .map_err(mongo_err)?
                            .deleted_count
                    }
                }
            }
            EvictionPolicy::OlderThan { before_millis } => {
                traces
                    .delete_many(doc! {
                        "company_id": id.as_ref(),
                        "at_ms": {"$lt": before_millis as i64},
                    })
                    .await
                    .map_err(mongo_err)?
                    .deleted_count
            }
        };
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// ContextStore
// ---------------------------------------------------------------------------

#[async_trait]
impl ContextStore for MongoStore {
    async fn put(&self, id: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr> {
        let addr = content_address(&chunk.body);
        // Insertion order stands in for the sqlite backend's rowid ordering.
        let ord = self.next_seq(id, "context_ord").await?;
        // Body + metadata land once and never move (first-write-wins per
        // address); every put folds its label into the `labels` set — one
        // claim per (addr, label), #1300 — so a byte-identical body stored
        // under a second label keeps both claims.
        //
        // A pipeline rather than `$setOnInsert` + `$addToSet`, for the scalar
        // `label`: `$setOnInsert` is skipped on a document that already
        // exists, so a document that has somehow lost its scalar would gain a
        // claim while staying scalar-less — and a pre-#1300 `list` reads that
        // field with a hard error on absence, so a rollback would meet a
        // document it cannot read. `$ifNull` restores it from this write
        // instead, and keeps the existing one untouched when there is one.
        let claims = dedupe_preserving_order(doc! {"$concatArrays": [
            {"$ifNull": ["$labels", []]},
            [chunk.label.as_str()],
        ]});
        let result = self
            .collection("context_chunks")
            .update_one(
                doc! {"company_id": id.as_ref(), "addr": &addr},
                vec![doc! {"$set": {
                    "labels": claims,
                    // `$ifNull` IS the first-write-wins rule: on an upsert
                    // insert every field is missing and takes this write's
                    // value; on an existing document each keeps what it has.
                    "label": {"$ifNull": ["$label", chunk.label.as_str()]},
                    "body": {"$ifNull": ["$body", chunk.body.as_str()]},
                    "len": {"$ifNull": ["$len", chunk.body.len() as i64]},
                    "ord": {"$ifNull": ["$ord", ord as i64]},
                    "stored_ms": {"$ifNull": ["$stored_ms", now_millis() as i64]},
                }}],
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await;
        match result {
            Ok(_) => Ok(ChunkAddr::new(addr)),
            Err(e) => Err(mongo_err(e)),
        }
    }

    async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>> {
        let mut cursor = self
            .collection("context_chunks")
            .find(doc! {"company_id": id.as_ref()})
            .sort(doc! {"ord": 1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            let addr = get_str(&doc, "addr")?;
            let len = get_i64(&doc, "len")? as usize;
            // Absent on documents written before the field existed; those
            // read as an unknown (`0`) store time rather than failing the
            // whole list. One document carries every label claiming its
            // address, so the stamp is the address's first write — per-label
            // stamps are an fs/sqlite refinement this backend does not keep.
            let stored_at_millis = doc.get_i64("stored_ms").unwrap_or(0).max(0) as u64;
            for label in doc_labels(&doc) {
                if label.starts_with(prefix) {
                    out.push(ChunkMeta {
                        addr: ChunkAddr::new(addr.clone()),
                        label,
                        len,
                        stored_at_millis,
                    });
                }
            }
        }
        Ok(out)
    }

    async fn delete(&self, id: &CompanyId, addr: &ChunkAddr) -> Result<bool> {
        // Address-level: the one document carries the body and every label
        // claim, so they go together.
        let result = self
            .collection("context_chunks")
            .delete_one(doc! {"company_id": id.as_ref(), "addr": addr.as_ref()})
            .await
            .map_err(mongo_err)?;
        Ok(result.deleted_count > 0)
    }

    async fn delete_label(&self, id: &CompanyId, addr: &ChunkAddr, label: &str) -> Result<bool> {
        let chunks = self.collection("context_chunks");
        // Every label claiming this document, as an expression over both
        // shapes a claim lives in: the legacy scalar `label` first, then the
        // `labels` set.
        let claim_union = doc! {"$concatArrays": [
            {"$cond": [{"$ifNull": ["$label", false]}, ["$label"], []]},
            {"$ifNull": ["$labels", []]},
        ]};

        // Label-scoped (#1300), as ONE atomic document operation per outcome
        // — never a read followed by a write, and never an intermediate state
        // a concurrent reader or writer can observe.
        //
        // First outcome: this label is the ONLY claim, so the whole document
        // goes. `$setEquals` ignores order and duplicates, and the condition
        // is evaluated by the server as part of the delete, so a concurrent
        // put that adds a second claim first simply makes this match nothing
        // — the check-then-delete race the callers' old snapshot guards
        // carried, closed by construction rather than by a snapshot.
        let reaped = chunks
            .delete_one(doc! {
                "company_id": id.as_ref(),
                "addr": addr.as_ref(),
                "$expr": {"$setEquals": [claim_union.clone(), [label]]},
            })
            .await
            .map_err(mongo_err)?;
        if reaped.deleted_count > 0 {
            return Ok(true);
        }

        // Second outcome: other claims remain, so only this one is removed.
        // The scalar is re-pointed at a survivor in the SAME update, and this
        // branch can only run while a survivor exists — so the document is
        // never left without a scalar `label`, which a pre-#1300 `list` reads
        // with a hard error on absence. Deduped by `$reduce` rather than
        // `$setUnion`, which would not preserve read order.
        let survivors = dedupe_preserving_order(doc! {"$filter": {
            "input": claim_union,
            "cond": {"$ne": ["$$this", label]},
        }});
        let removed = chunks
            .update_one(
                doc! {
                    "company_id": id.as_ref(),
                    "addr": addr.as_ref(),
                    // Only a document this label actually claims, so a match
                    // IS the answer to "did the pairing exist" — and so the
                    // pipeline can never touch a document it has no claim on.
                    "$or": [{"label": label}, {"labels": label}],
                },
                vec![doc! {"$set": {
                    "labels": survivors.clone(),
                    "label": {"$first": survivors},
                }}],
            )
            .await
            .map_err(mongo_err)?;
        Ok(removed.matched_count > 0)
    }

    async fn peek(
        &self,
        id: &CompanyId,
        addr: &ChunkAddr,
        range: Option<Range<usize>>,
    ) -> Result<String> {
        let doc = self
            .collection("context_chunks")
            .find_one(doc! {"company_id": id.as_ref(), "addr": addr.as_ref()})
            .await
            .map_err(mongo_err)?
            .ok_or_else(|| {
                OpenCompanyError::Store(format!("context chunk not found: {}", addr.as_ref()))
            })?;
        let body = get_str(&doc, "body")?;
        match range {
            None => Ok(body),
            // Byte offsets from the caller can land mid-codepoint; widen to
            // the boundary rather than panic the slice.
            Some(r) => Ok(slice_on_char_boundaries(&body, r)),
        }
    }

    async fn peek_many(&self, id: &CompanyId, addrs: &[ChunkAddr]) -> Result<Vec<Option<String>>> {
        // One `$in` find for the whole batch, instead of the default's
        // round-trip-per-chunk loop.
        let wanted: Vec<&str> = addrs.iter().map(|a| a.as_ref()).collect();
        let mut cursor = self
            .collection("context_chunks")
            .find(doc! {"company_id": id.as_ref(), "addr": {"$in": wanted}})
            .await
            .map_err(mongo_err)?;
        let mut by_addr = HashMap::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            // Per-addr degrade, per the port contract: a document missing its
            // fields answers `None` for its addr rather than failing the
            // batch — only the find/cursor itself is batch-level.
            match (get_str(&doc, "addr"), get_str(&doc, "body")) {
                (Ok(addr), Ok(body)) => {
                    by_addr.insert(addr, body);
                }
                (addr, body) => {
                    let error = addr.err().or(body.err()).expect("one side failed");
                    tracing::warn!(%error, "malformed context document skipped in bulk read");
                }
            }
        }
        Ok(addrs
            .iter()
            .map(|addr| by_addr.get(addr.as_ref()).cloned())
            .collect())
    }

    /// Weighted token overlap rather than `body.find(query)` — see
    /// [`crate::store::lexical`] for what was wrong and why the weighting is the
    /// way it is.
    ///
    /// The cursor keeps streaming: each body goes through the [`Ranker`] and is
    /// then dropped, so a company with a filled history does not put its whole
    /// `context_chunks` collection in the service's memory. What has to be given
    /// up is the early `break`: ranking is only possible once every candidate has
    /// been seen — and that is the bug, not the price of it.
    ///
    /// [`Ranker`]: crate::store::lexical::Ranker
    async fn search(&self, id: &CompanyId, query: &str, limit: usize) -> Result<Vec<ChunkHit>> {
        let mut ranker = crate::store::lexical::Ranker::new(query);
        if ranker.matches_nothing() {
            return Ok(Vec::new());
        }
        let mut cursor = self
            .collection("context_chunks")
            .find(doc! {"company_id": id.as_ref()})
            .sort(doc! {"ord": 1})
            .await
            .map_err(mongo_err)?;
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            ranker.offer(&get_str(&doc, "addr")?, &get_str(&doc, "body")?);
        }
        Ok(ranker.best(limit))
    }
}

// ---------------------------------------------------------------------------
// SecretStore
// ---------------------------------------------------------------------------

#[async_trait]
impl SecretStore for MongoStore {
    async fn get(&self, company: &CompanyId, key: &str) -> Result<Option<SecretValue>> {
        let doc = self
            .collection("secrets")
            .find_one(doc! {"company_id": company.as_ref(), "key": key})
            .await
            .map_err(mongo_err)?;
        doc.map(|d| get_str(&d, "value").map(SecretValue))
            .transpose()
    }

    async fn set(&self, company: &CompanyId, key: &str, value: SecretValue) -> Result<()> {
        self.collection("secrets")
            .update_one(
                doc! {"company_id": company.as_ref(), "key": key},
                doc! {"$set": {"value": value.expose()}},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// InboxStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::inbox::InboxStore for MongoStore {
    async fn inboxes(&self, company: &CompanyId) -> Result<Vec<crate::ports::inbox::InboxMeta>> {
        use std::collections::BTreeMap;
        let mut out: BTreeMap<String, crate::ports::inbox::InboxMeta> = BTreeMap::new();
        // Explicit metadata first.
        let mut cursor = self
            .collection("inbox_meta")
            .find(doc! {"company_id": company.as_ref()})
            .await
            .map_err(mongo_err)?;
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            let meta: crate::ports::inbox::InboxMeta =
                serde_json::from_str(&get_str(&doc, "meta_json")?)?;
            out.insert(meta.key.clone(), meta);
        }
        // Synthesize a default enabled meta for message-only inboxes.
        let names = self
            .collection("inbox")
            .distinct("inbox", doc! {"company_id": company.as_ref()})
            .await
            .map_err(mongo_err)?;
        for name in names
            .into_iter()
            .filter_map(|b| b.as_str().map(str::to_string))
        {
            out.entry(name.clone())
                .or_insert_with(|| crate::ports::inbox::InboxMeta {
                    key: name.clone(),
                    name: name.clone(),
                    address: String::new(),
                    enabled: true,
                });
        }
        Ok(out.into_values().collect())
    }

    async fn set_enabled(
        &self,
        company: &CompanyId,
        key: &str,
        meta: &crate::ports::inbox::InboxMeta,
    ) -> Result<()> {
        let meta_json = serde_json::to_string(meta)?;
        self.collection("inbox_meta")
            .update_one(
                doc! {"company_id": company.as_ref(), "key": key},
                doc! {"$set": {"meta_json": meta_json}},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn append(
        &self,
        company: &CompanyId,
        msg: &crate::ports::inbox::EmailRecord,
    ) -> Result<()> {
        let record_json = serde_json::to_string(msg)?;
        let seq = self.next_seq(company, "inbox").await?;
        self.collection("inbox")
            .insert_one(doc! {
                "company_id": company.as_ref(),
                "seq": seq as i64,
                "inbox": &msg.inbox,
                "record_json": record_json,
            })
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn messages(
        &self,
        company: &CompanyId,
        key: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::ports::inbox::EmailRecord>> {
        let collection = self.collection("inbox");
        let mut find = collection
            .find(doc! {"company_id": company.as_ref(), "inbox": key})
            .sort(doc! {"seq": 1})
            .skip(offset as u64);
        match find_limit(limit) {
            FindLimit::Empty => return Ok(Vec::new()),
            FindLimit::Unlimited => {}
            FindLimit::AtMost(n) => find = find.limit(n),
        }
        let mut cursor = find.await.map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "record_json")?)?);
        }
        Ok(out)
    }

    async fn mark_read(
        &self,
        company: &CompanyId,
        key: &str,
        ids: Option<&[String]>,
    ) -> Result<u64> {
        use crate::ports::inbox::EmailRecord;
        let coll = self.collection("inbox");
        let mut cursor = coll
            .find(doc! {"company_id": company.as_ref(), "inbox": key})
            .await
            .map_err(mongo_err)?;
        let mut unread = 0u64;
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            let seq = get_i64(&doc, "seq")?;
            let mut record: EmailRecord = serde_json::from_str(&get_str(&doc, "record_json")?)?;
            let hit = match ids {
                Some(ids) => ids.iter().any(|id| id == &record.id),
                None => true,
            };
            if hit && !record.read {
                record.read = true;
                coll.update_one(
                    doc! {"company_id": company.as_ref(), "seq": seq},
                    doc! {"$set": {"record_json": serde_json::to_string(&record)?}},
                )
                .await
                .map_err(mongo_err)?;
            }
            if !record.read {
                unread += 1;
            }
        }
        Ok(unread)
    }
}

// ---------------------------------------------------------------------------
// TaskStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::tasks::TaskStore for MongoStore {
    async fn list(&self, company: &CompanyId) -> Result<Vec<crate::ports::tasks::TaskRecord>> {
        let mut cursor = self
            .collection("tasks")
            .find(doc! {"company_id": company.as_ref()})
            .sort(doc! {"updated_ms": -1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "task_json")?)?);
        }
        Ok(out)
    }

    async fn upsert(
        &self,
        company: &CompanyId,
        task: &crate::ports::tasks::TaskRecord,
    ) -> Result<()> {
        self.collection("tasks")
            .update_one(
                doc! {"company_id": company.as_ref(), "task_id": &task.id},
                doc! {"$set": {
                    "task_json": serde_json::to_string(task)?,
                    "updated_ms": task.updated_at_millis as i64,
                }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let res = self
            .collection("tasks")
            .delete_one(doc! {"company_id": company.as_ref(), "task_id": id})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count > 0)
    }
}

// ---------------------------------------------------------------------------
// LedgerStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::ledgers::LedgerStore for MongoStore {
    async fn list_specs(&self, company: &CompanyId) -> Result<Vec<crate::ledger::LedgerSpec>> {
        let mut cursor = self
            .collection("ledger_specs")
            .find(doc! {"company_id": company.as_ref()})
            .sort(doc! {"slug": 1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "spec_json")?)?);
        }
        Ok(out)
    }

    async fn put_spec(&self, company: &CompanyId, spec: &crate::ledger::LedgerSpec) -> Result<()> {
        self.collection("ledger_specs")
            .update_one(
                doc! {"company_id": company.as_ref(), "slug": &spec.slug},
                doc! {"$set": { "spec_json": serde_json::to_string(spec)? }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn delete_spec(&self, company: &CompanyId, slug: &str) -> Result<bool> {
        // The events stay. See `LedgerStore::delete_spec`.
        let res = self
            .collection("ledger_specs")
            .delete_one(doc! {"company_id": company.as_ref(), "slug": slug})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count > 0)
    }

    async fn append(&self, company: &CompanyId, event: &crate::ledger::LedgerEvent) -> Result<()> {
        // A company-wide counter rather than a per-ledger one. The fold only
        // needs the order *within* a ledger, and one counter per company is one
        // document to contend on instead of one per axis — while still giving a
        // total order the console can read across ledgers.
        let seq = self.next_seq(company, "ledger_events").await?;
        self.collection("ledger_events")
            .insert_one(doc! {
                "company_id": company.as_ref(),
                "seq": seq as i64,
                "ledger": &event.ledger,
                "entry_id": &event.id,
                "event_json": serde_json::to_string(event)?,
            })
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn events(
        &self,
        company: &CompanyId,
        ledger: &str,
    ) -> Result<Vec<crate::ledger::LedgerEvent>> {
        let mut cursor = self
            .collection("ledger_events")
            .find(doc! {"company_id": company.as_ref(), "ledger": ledger})
            .sort(doc! {"seq": 1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "event_json")?)?);
        }
        Ok(out)
    }

    async fn purge_entry(&self, company: &CompanyId, ledger: &str, entry: &str) -> Result<bool> {
        let res = self
            .collection("ledger_events")
            .delete_many(doc! {
                "company_id": company.as_ref(),
                "ledger": ledger,
                "entry_id": entry,
            })
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count > 0)
    }

    async fn purge_ledger(&self, company: &CompanyId, ledger: &str) -> Result<bool> {
        let res = self
            .collection("ledger_events")
            .delete_many(doc! {"company_id": company.as_ref(), "ledger": ledger})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count > 0)
    }
}

// ---------------------------------------------------------------------------
// UserStore
// ---------------------------------------------------------------------------

/// Whether a driver failure is a duplicate-key violation (E11000).
///
/// The unique email/token indexes are the real enforcement; this maps the
/// driver's error onto the crate's `409 Conflict` so every backend reports a
/// clash identically.
fn is_duplicate_key(e: &mongodb::error::Error) -> bool {
    matches!(
        *e.kind,
        mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(ref we))
            if we.code == 11000
    )
}

#[async_trait]
impl crate::ports::users::UserStore for MongoStore {
    async fn list_users(&self, company: &CompanyId) -> Result<Vec<UserRecord>> {
        let mut cursor = self
            .collection("users")
            .find(doc! {"company_id": company.as_ref()})
            .sort(doc! {"created_ms": -1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "user_json")?)?);
        }
        Ok(out)
    }

    async fn get_user(&self, company: &CompanyId, id: &str) -> Result<Option<UserRecord>> {
        let found = self
            .collection("users")
            .find_one(doc! {"company_id": company.as_ref(), "user_id": id})
            .await
            .map_err(mongo_err)?;
        match found {
            Some(doc) => Ok(Some(serde_json::from_str(&get_str(&doc, "user_json")?)?)),
            None => Ok(None),
        }
    }

    async fn find_user_by_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<UserRecord>> {
        // Exact match on the unique email index. Normalization is the caller's
        // job, so a store never matches an address it was not asked for.
        let found = self
            .collection("users")
            .find_one(doc! {"company_id": company.as_ref(), "email": email})
            .await
            .map_err(mongo_err)?;
        match found {
            Some(doc) => Ok(Some(serde_json::from_str(&get_str(&doc, "user_json")?)?)),
            None => Ok(None),
        }
    }

    async fn upsert_user(&self, company: &CompanyId, user: &UserRecord) -> Result<()> {
        self.collection("users")
            .update_one(
                doc! {"company_id": company.as_ref(), "user_id": &user.id},
                doc! {"$set": {
                    "email": &user.email,
                    "user_json": serde_json::to_string(user)?,
                    "created_ms": user.created_at_millis as i64,
                }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(|e| {
                if is_duplicate_key(&e) {
                    OpenCompanyError::Conflict(format!(
                        "another user already has the email {}",
                        user.email
                    ))
                } else {
                    mongo_err(e)
                }
            })?;
        Ok(())
    }

    async fn delete_user(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let res = self
            .collection("users")
            .delete_one(doc! {"company_id": company.as_ref(), "user_id": id})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count > 0)
    }

    async fn list_invites(&self, company: &CompanyId) -> Result<Vec<InviteRecord>> {
        let mut cursor = self
            .collection("user_invites")
            .find(doc! {"company_id": company.as_ref()})
            .sort(doc! {"created_ms": -1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "invite_json")?)?);
        }
        Ok(out)
    }

    async fn find_invite_by_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<InviteRecord>> {
        let found = self
            .collection("user_invites")
            .find_one(doc! {"company_id": company.as_ref(), "email": email})
            .await
            .map_err(mongo_err)?;
        match found {
            Some(doc) => Ok(Some(serde_json::from_str(&get_str(&doc, "invite_json")?)?)),
            None => Ok(None),
        }
    }

    async fn upsert_invite(&self, company: &CompanyId, invite: &InviteRecord) -> Result<()> {
        self.collection("user_invites")
            .update_one(
                doc! {"company_id": company.as_ref(), "invite_id": &invite.id},
                doc! {"$set": {
                    "email": &invite.email,
                    "invite_json": serde_json::to_string(invite)?,
                    "created_ms": invite.created_at_millis as i64,
                }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(|e| {
                if is_duplicate_key(&e) {
                    OpenCompanyError::Conflict(format!("{} is already invited", invite.email))
                } else {
                    mongo_err(e)
                }
            })?;
        Ok(())
    }

    async fn mark_invite_notified(
        &self,
        company: &CompanyId,
        id: &str,
        at_millis: u64,
    ) -> Result<bool> {
        let found = self
            .collection("user_invites")
            .find_one(doc! {"company_id": company.as_ref(), "invite_id": id})
            .await
            .map_err(mongo_err)?;
        let Some(found) = found else {
            return Ok(false);
        };
        let current = get_str(&found, "invite_json")?;
        let mut invite: InviteRecord = serde_json::from_str(&current)?;
        invite.notified_at_millis = Some(at_millis);
        // No `upsert` option, and the filter pins the document we just read: a
        // revocation that lands between the two calls leaves nothing to match,
        // so the stamp becomes a no-op instead of resurrecting the row.
        let res = self
            .collection("user_invites")
            .update_one(
                doc! {
                    "company_id": company.as_ref(),
                    "invite_id": id,
                    "invite_json": &current,
                },
                doc! {"$set": {"invite_json": serde_json::to_string(&invite)?}},
            )
            .await
            .map_err(mongo_err)?;
        Ok(res.matched_count > 0)
    }

    async fn delete_invite(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let res = self
            .collection("user_invites")
            .delete_one(doc! {"company_id": company.as_ref(), "invite_id": id})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count > 0)
    }
}

// ---------------------------------------------------------------------------
// SessionStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::sessions::SessionStore for MongoStore {
    async fn create(&self, company: &CompanyId, session: &SessionRecord) -> Result<()> {
        self.collection("user_sessions")
            .insert_one(doc! {
                "company_id": company.as_ref(),
                "session_id": &session.id,
                "token_hash": &session.token_hash,
                "user_id": &session.user_id,
                "session_json": serde_json::to_string(session)?,
                "created_ms": session.created_at_millis as i64,
                "expires_ms": session.expires_at_millis as i64,
            })
            .await
            .map_err(|e| {
                if is_duplicate_key(&e) {
                    OpenCompanyError::Conflict("that session token already exists".to_string())
                } else {
                    mongo_err(e)
                }
            })?;
        Ok(())
    }

    async fn find_by_token_hash(
        &self,
        company: &CompanyId,
        token_hash: &str,
    ) -> Result<Option<SessionRecord>> {
        let found = self
            .collection("user_sessions")
            .find_one(doc! {"company_id": company.as_ref(), "token_hash": token_hash})
            .await
            .map_err(mongo_err)?;
        match found {
            Some(doc) => Ok(Some(serde_json::from_str(&get_str(&doc, "session_json")?)?)),
            None => Ok(None),
        }
    }

    async fn list_for_user(
        &self,
        company: &CompanyId,
        user_id: &str,
    ) -> Result<Vec<SessionRecord>> {
        let mut cursor = self
            .collection("user_sessions")
            .find(doc! {"company_id": company.as_ref(), "user_id": user_id})
            .sort(doc! {"created_ms": -1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "session_json")?)?);
        }
        Ok(out)
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let res = self
            .collection("user_sessions")
            .delete_one(doc! {"company_id": company.as_ref(), "session_id": id})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count > 0)
    }

    async fn delete_for_user(&self, company: &CompanyId, user_id: &str) -> Result<u64> {
        let res = self
            .collection("user_sessions")
            .delete_many(doc! {"company_id": company.as_ref(), "user_id": user_id})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count)
    }

    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64> {
        // Expiry is exclusive: a session expiring exactly at `now` is dead.
        let res = self
            .collection("user_sessions")
            .delete_many(doc! {
                "company_id": company.as_ref(),
                "expires_ms": {"$lte": now_millis as i64},
            })
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count)
    }
}

// ---------------------------------------------------------------------------
// LoginCodeStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::login_codes::LoginCodeStore for MongoStore {
    async fn create(&self, company: &CompanyId, code: &LoginCodeRecord) -> Result<()> {
        self.collection("login_codes")
            .insert_one(doc! {
                "company_id": company.as_ref(),
                "code_id": &code.id,
                "code_hash": &code.code_hash,
                "email": &code.email,
                "code_json": serde_json::to_string(code)?,
                "expires_ms": code.expires_at_millis as i64,
                // Promoted so redeemability is expressible as a query filter,
                // which is what makes the claim below atomic.
                "consumed_ms": Option::<i64>::None,
            })
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn latest_for_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<LoginCodeRecord>> {
        // Served by the non-unique (company_id, email) index. `expires_ms`
        // orders by mint time, since every code for one address shares a TTL.
        let found = self
            .collection("login_codes")
            .find_one(doc! {"company_id": company.as_ref(), "email": email})
            .sort(doc! {"expires_ms": -1})
            .await
            .map_err(mongo_err)?;
        match found {
            Some(doc) => Ok(Some(serde_json::from_str(&get_str(&doc, "code_json")?)?)),
            None => Ok(None),
        }
    }

    async fn consume(
        &self,
        company: &CompanyId,
        code_hash: &str,
        now_millis: u64,
    ) -> Result<Option<LoginCodeRecord>> {
        // Single-use lives or dies here. `findOneAndUpdate` matches and marks in
        // one atomic server-side operation, so of two requests racing on the
        // same code exactly one can match `consumed_ms: null` — the loser's
        // filter no longer matches and it gets `None`.
        //
        // `consumed_ms` is promoted out of the payload purely so the filter can
        // express "unconsumed"; the payload is still the record, and it is
        // brought back into agreement immediately below.
        let claimed = self
            .collection("login_codes")
            .find_one_and_update(
                doc! {
                    "company_id": company.as_ref(),
                    "code_hash": code_hash,
                    "consumed_ms": Option::<i64>::None,
                    "expires_ms": {"$gt": now_millis as i64},
                },
                doc! {"$set": {"consumed_ms": now_millis as i64}},
            )
            .with_options(
                FindOneAndUpdateOptions::builder()
                    .return_document(ReturnDocument::Before)
                    .build(),
            )
            .await
            .map_err(mongo_err)?;
        let Some(doc) = claimed else {
            return Ok(None);
        };
        let mut code: LoginCodeRecord = serde_json::from_str(&get_str(&doc, "code_json")?)?;
        code.consumed_at_millis = Some(now_millis);
        // Payload fidelity: the claim above already guaranteed single use, so a
        // failure here cannot hand out a second session — the code is spent
        // either way.
        self.collection("login_codes")
            .update_one(
                doc! {"company_id": company.as_ref(), "code_hash": code_hash},
                doc! {"$set": {"code_json": serde_json::to_string(&code)?}},
            )
            .await
            .map_err(mongo_err)?;
        Ok(Some(code))
    }

    async fn delete_for_email(&self, company: &CompanyId, email: &str) -> Result<u64> {
        let res = self
            .collection("login_codes")
            .delete_many(doc! {"company_id": company.as_ref(), "email": email})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count)
    }

    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64> {
        let res = self
            .collection("login_codes")
            .delete_many(doc! {
                "company_id": company.as_ref(),
                "expires_ms": {"$lte": now_millis as i64},
            })
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count)
    }
}

// ---------------------------------------------------------------------------
// FactStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::facts::FactStore for MongoStore {
    async fn list(
        &self,
        company: &CompanyId,
        query: Option<&str>,
        kind: Option<crate::ports::facts::FactKind>,
    ) -> Result<Vec<crate::ports::facts::FactRecord>> {
        let mut cursor = self
            .collection("facts")
            .find(doc! {"company_id": company.as_ref()})
            .sort(doc! {"updated_ms": -1})
            .await
            .map_err(mongo_err)?;
        let mut out: Vec<crate::ports::facts::FactRecord> = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "fact_json")?)?);
        }
        if let Some(kind) = kind {
            out.retain(|f| f.kind == kind);
        }
        if let Some(q) = query.map(str::to_lowercase).filter(|q| !q.is_empty()) {
            out.retain(|f| {
                f.title.to_lowercase().contains(&q) || f.body.to_lowercase().contains(&q)
            });
        }
        Ok(out)
    }

    async fn upsert(
        &self,
        company: &CompanyId,
        fact: &crate::ports::facts::FactRecord,
    ) -> Result<()> {
        self.collection("facts")
            .update_one(
                doc! {"company_id": company.as_ref(), "fact_id": &fact.id},
                doc! {"$set": {
                    "fact_json": serde_json::to_string(fact)?,
                    "updated_ms": fact.updated_at_millis as i64,
                }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let res = self
            .collection("facts")
            .delete_one(doc! {"company_id": company.as_ref(), "fact_id": id})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count > 0)
    }
}

// ---------------------------------------------------------------------------
// ArtifactStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::artifacts::ArtifactStore for MongoStore {
    async fn list(
        &self,
        company: &CompanyId,
        task_id: Option<&str>,
    ) -> Result<Vec<crate::ports::artifacts::ArtifactRecord>> {
        // `task_id` narrows the query itself rather than filtering after the
        // fetch, so one task's Artifacts tab does not pull the whole company.
        let mut filter = doc! {"company_id": company.as_ref()};
        if let Some(task_id) = task_id {
            filter.insert("task_id", task_id);
        }
        let mut cursor = self
            .collection("artifacts")
            .find(filter)
            .sort(doc! {"updated_ms": -1})
            .await
            .map_err(mongo_err)?;
        let mut out: Vec<crate::ports::artifacts::ArtifactRecord> = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "artifact_json")?)?);
        }
        Ok(out)
    }

    async fn get(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<crate::ports::artifacts::ArtifactRecord>> {
        let found = self
            .collection("artifacts")
            .find_one(doc! {"company_id": company.as_ref(), "artifact_id": id})
            .await
            .map_err(mongo_err)?;
        match found {
            Some(doc) => Ok(Some(serde_json::from_str(&get_str(
                &doc,
                "artifact_json",
            )?)?)),
            None => Ok(None),
        }
    }

    async fn upsert(
        &self,
        company: &CompanyId,
        artifact: &crate::ports::artifacts::ArtifactRecord,
    ) -> Result<()> {
        self.collection("artifacts")
            .update_one(
                doc! {"company_id": company.as_ref(), "artifact_id": &artifact.id},
                doc! {"$set": {
                    "task_id": &artifact.task_id,
                    "artifact_json": serde_json::to_string(artifact)?,
                    "updated_ms": artifact.updated_at_millis as i64,
                }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let res = self
            .collection("artifacts")
            .delete_one(doc! {"company_id": company.as_ref(), "artifact_id": id})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count > 0)
    }
}

// ---------------------------------------------------------------------------
// WorkflowRevisionStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::workflow_revisions::WorkflowRevisionStore for MongoStore {
    async fn push_revision(
        &self,
        company: &CompanyId,
        revision: &crate::ports::workflow_revisions::WorkflowRevisionRecord,
    ) -> Result<()> {
        use crate::ports::workflow_revisions::MAX_WORKFLOW_REVISIONS;
        let coll = self.collection("workflow_revisions");
        coll.insert_one(doc! {
            "company_id": company.as_ref(),
            "revision_id": &revision.id,
            "workflow_id": &revision.workflow_id,
            "revision_json": serde_json::to_string(revision)?,
            "created_ms": revision.created_at_millis as i64,
        })
        .await
        .map_err(mongo_err)?;

        // Prune to the cap: collect this workflow's revision ids newest-first and
        // delete everything past `MAX`. Two statements rather than one because
        // MongoDB has no "delete all but the newest N" operator; the compound
        // index keeps the read cheap, and an interleaved second push only ever
        // trims further, never resurrects a pruned row.
        let mut cursor = coll
            .find(doc! {"company_id": company.as_ref(), "workflow_id": &revision.workflow_id})
            .sort(doc! {"created_ms": -1, "revision_id": -1})
            .await
            .map_err(mongo_err)?;
        let mut ids: Vec<String> = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            ids.push(get_str(&doc, "revision_id")?);
        }
        if ids.len() > MAX_WORKFLOW_REVISIONS {
            let stale: Vec<&String> = ids.iter().skip(MAX_WORKFLOW_REVISIONS).collect();
            coll.delete_many(doc! {
                "company_id": company.as_ref(),
                "workflow_id": &revision.workflow_id,
                "revision_id": {"$in": stale},
            })
            .await
            .map_err(mongo_err)?;
        }
        Ok(())
    }

    async fn list_revisions(
        &self,
        company: &CompanyId,
        workflow_id: &str,
    ) -> Result<Vec<crate::ports::workflow_revisions::WorkflowRevisionRecord>> {
        let mut cursor = self
            .collection("workflow_revisions")
            .find(doc! {"company_id": company.as_ref(), "workflow_id": workflow_id})
            .sort(doc! {"created_ms": -1, "revision_id": -1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "revision_json")?)?);
        }
        Ok(out)
    }

    async fn get_revision(
        &self,
        company: &CompanyId,
        workflow_id: &str,
        revision_id: &str,
    ) -> Result<Option<crate::ports::workflow_revisions::WorkflowRevisionRecord>> {
        let found = self
            .collection("workflow_revisions")
            .find_one(doc! {
                "company_id": company.as_ref(),
                "workflow_id": workflow_id,
                "revision_id": revision_id,
            })
            .await
            .map_err(mongo_err)?;
        match found {
            Some(doc) => Ok(Some(serde_json::from_str(&get_str(
                &doc,
                "revision_json",
            )?)?)),
            None => Ok(None),
        }
    }

    async fn delete_revisions(&self, company: &CompanyId, workflow_id: &str) -> Result<u64> {
        let res = self
            .collection("workflow_revisions")
            .delete_many(doc! {"company_id": company.as_ref(), "workflow_id": workflow_id})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count)
    }
}

// ---------------------------------------------------------------------------
// WorkflowRunOutputStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::run_output::WorkflowRunOutputStore for MongoStore {
    async fn put_run_output(
        &self,
        company: &CompanyId,
        record: &crate::ports::run_output::WorkflowRunOutputRecord,
    ) -> Result<()> {
        use crate::ports::run_output::MAX_RUN_OUTPUTS_PER_COMPANY;
        let coll = self.collection("run_outputs");
        // Upsert: last-write-wins per `(company, run_id)`, so a re-run overwrites
        // rather than stacking. The unique index is what makes the filter a key.
        coll.update_one(
            doc! {"company_id": company.as_ref(), "run_id": &record.run_id},
            doc! {"$set": {
                "company_id": company.as_ref(),
                "run_id": &record.run_id,
                "workflow_id": &record.workflow_id,
                "at_ms": record.at_millis as i64,
                "output_json": serde_json::to_string(record)?,
            }},
        )
        .with_options(UpdateOptions::builder().upsert(true).build())
        .await
        .map_err(mongo_err)?;

        // Prune to the cap: collect this company's run ids newest-first and delete
        // everything past `MAX`. Two statements, like the revision prune — Mongo
        // has no "delete all but the newest N" operator; the recency index keeps
        // the read cheap. Each run owns at most one row here (this method
        // upserts by run id), so a row a concurrent writer refreshes mid-scan
        // carries a newer `at_ms` than the cap row — conditioning the delete on
        // `at_ms <= cutoff` spares exactly the refreshed outputs, closing the
        // scan-to-delete race the deep prune beside it re-verifies in full.
        let mut cursor = coll
            .find(doc! {"company_id": company.as_ref()})
            .sort(doc! {"at_ms": -1, "run_id": -1})
            .await
            .map_err(mongo_err)?;
        let mut ranked: Vec<(String, i64)> = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            ranked.push((get_str(&doc, "run_id")?, get_i64(&doc, "at_ms")?));
        }
        if ranked.len() > MAX_RUN_OUTPUTS_PER_COMPANY {
            let cutoff = ranked[MAX_RUN_OUTPUTS_PER_COMPANY - 1].1;
            let stale: Vec<&str> = ranked
                .iter()
                .skip(MAX_RUN_OUTPUTS_PER_COMPANY)
                .map(|(id, _)| id.as_str())
                .collect();
            coll.delete_many(doc! {
                "company_id": company.as_ref(),
                "run_id": {"$in": stale},
                "at_ms": {"$lte": cutoff},
            })
            .await
            .map_err(mongo_err)?;
        }
        Ok(())
    }

    async fn get_run_output(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Option<crate::ports::run_output::WorkflowRunOutputRecord>> {
        let found = self
            .collection("run_outputs")
            .find_one(doc! {"company_id": company.as_ref(), "run_id": run_id})
            .await
            .map_err(mongo_err)?;
        match found {
            Some(doc) => Ok(Some(serde_json::from_str(&get_str(&doc, "output_json")?)?)),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// DeepTraceStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::deep_trace::DeepTraceStore for MongoStore {
    async fn append_step_detail(
        &self,
        company: &CompanyId,
        record: &crate::ports::deep_trace::RunStepDetailRecord,
    ) -> Result<()> {
        use crate::ports::deep_trace::MAX_DEEP_RUNS_PER_COMPANY;
        let coll = self.collection("run_step_details");
        // Upsert: last-write-wins per `(company, run_id, step_seq)`, so a
        // reasoning run that flushes partway and again at close converges.
        coll.update_one(
            doc! {
                "company_id": company.as_ref(),
                "run_id": &record.run_id,
                "step_seq": record.step_seq as i64,
            },
            doc! {"$set": {
                "company_id": company.as_ref(),
                "run_id": &record.run_id,
                "step_seq": record.step_seq as i64,
                "at_ms": record.at_millis as i64,
                "detail_json": serde_json::to_string(&record.detail)?,
            }},
        )
        .with_options(UpdateOptions::builder().upsert(true).build())
        .await
        .map_err(mongo_err)?;

        // Prune to the cap by RUN, not by row: ranking rows would leave a
        // surviving run holding a torn half of its own trace. A run's recency is
        // the newest `at_ms` any of its rows carries, so a long run is ranked by
        // when it last wrote. Ranking and deleting are two statements (Mongo has
        // no "delete all but the newest N" operator), and a run a concurrent
        // writer refreshes between them must survive — so the delete is
        // conditional on the recency the ranking observed, not on the run id
        // alone. `at_ms` is monotone per run, so "still the recency we saw" is
        // exactly "still ranks past the cap"; a run written to since is spared.
        let mut cursor = coll
            .aggregate(vec![
                doc! {"$match": {"company_id": company.as_ref()}},
                doc! {"$group": {"_id": "$run_id", "newest": {"$max": "$at_ms"}}},
                doc! {"$sort": {"newest": -1, "_id": -1}},
                doc! {"$skip": MAX_DEEP_RUNS_PER_COMPANY as i64},
            ])
            .await
            .map_err(mongo_err)?;
        let mut stale: Vec<(String, i64)> = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            stale.push((get_str(&doc, "_id")?, get_i64(&doc, "newest")?));
        }
        if !stale.is_empty() {
            let candidate_ids: Vec<&str> = stale.iter().map(|(id, _)| id.as_str()).collect();
            // Re-verify in one pass: the newest `at_ms` each candidate carries
            // *now*, then delete only the candidates that did not move since
            // the ranking read them. A candidate another prune already removed
            // has no current row, so deleting it is a no-op.
            let mut verify = coll
                .aggregate(vec![
                    doc! {
                        "$match": {
                            "company_id": company.as_ref(),
                            "run_id": {"$in": candidate_ids},
                        }
                    },
                    doc! {"$group": {"_id": "$run_id", "newest": {"$max": "$at_ms"}}},
                ])
                .await
                .map_err(mongo_err)?;
            let mut newest: std::collections::HashMap<String, i64> = Default::default();
            while let Some(doc) = verify.try_next().await.map_err(mongo_err)? {
                newest.insert(get_str(&doc, "_id")?, get_i64(&doc, "newest")?);
            }
            let doomed: Vec<(&str, i64)> = stale
                .iter()
                .filter(|(id, seen)| newest.get(id.as_str()).is_none_or(|now| now <= seen))
                .map(|(id, seen)| (id.as_str(), *seen))
                .collect();
            if !doomed.is_empty() {
                // The ranking promised a run written to since is spared WHOLE,
                // so recency is re-checked one final time, at the delete: a
                // candidate holding any row newer than the `seen` the ranking
                // recorded was refreshed after the verification pass, and it
                // survives entire — deleting its older rows while keeping the
                // new one would leave exactly the torn trace the run-level
                // ranking exists to prevent. The delete predicate below still
                // excludes rows newer than `seen`, so a write that lands after
                // this check cannot lose the data it just wrote either.
                let mut live = coll
                    .aggregate(vec![
                        doc! {
                            "$match": {
                                "company_id": company.as_ref(),
                                "$or": doomed
                                    .iter()
                                    .map(|(id, seen)| {
                                        doc! {
                                            "run_id": id,
                                            "at_ms": {"$gt": seen},
                                        }
                                    })
                                    .collect::<Vec<_>>(),
                            }
                        },
                        doc! {"$group": {"_id": "$run_id"}},
                    ])
                    .await
                    .map_err(mongo_err)?;
                let mut refreshed: std::collections::HashSet<String> = Default::default();
                while let Some(row) = live.try_next().await.map_err(mongo_err)? {
                    refreshed.insert(get_str(&row, "_id")?.to_string());
                }
                let doomed: Vec<(&str, i64)> = doomed
                    .into_iter()
                    .filter(|(id, _)| !refreshed.contains(*id))
                    .collect();
                if doomed.is_empty() {
                    return Ok(());
                }
                // The delete is itself conditional on the recency the ranking
                // observed, not just on the run id: a writer that refreshes one
                // of these runs after the freshness check above but before this
                // delete would otherwise have its brand-new detail rows removed
                // by a `delete_many` keyed on `run_id` alone. `at_ms` is
                // monotone per run, so "row is at or below the recency we saw"
                // is exactly "this row was already stale when we ranked" — a
                // row written since has a larger `at_ms` and survives.
                let stale_row = doomed
                    .iter()
                    .map(|(id, seen)| {
                        doc! {
                            "run_id": id,
                            "at_ms": {"$lte": seen},
                        }
                    })
                    .collect::<Vec<_>>();
                coll.delete_many(doc! {
                    "company_id": company.as_ref(),
                    "$or": stale_row,
                })
                .await
                .map_err(mongo_err)?;
            }
        }
        Ok(())
    }

    async fn list_step_details(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Vec<crate::ports::deep_trace::RunStepDetailRecord>> {
        let mut cursor = self
            .collection("run_step_details")
            .find(doc! {"company_id": company.as_ref(), "run_id": run_id})
            .sort(doc! {"step_seq": 1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(crate::ports::deep_trace::RunStepDetailRecord {
                run_id: run_id.to_string(),
                step_seq: get_i64(&doc, "step_seq")? as u32,
                at_millis: get_i64(&doc, "at_ms")? as u64,
                detail: serde_json::from_str(&get_str(&doc, "detail_json")?)?,
            });
        }
        Ok(out)
    }

    async fn purge_deep_trace(&self, company: &CompanyId, run_id: Option<&str>) -> Result<u64> {
        let filter = match run_id {
            Some(id) => doc! {"company_id": company.as_ref(), "run_id": id},
            None => doc! {"company_id": company.as_ref()},
        };
        let result = self
            .collection("run_step_details")
            .delete_many(filter)
            .await
            .map_err(mongo_err)?;
        Ok(result.deleted_count)
    }
}

// ---------------------------------------------------------------------------
// RunStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::runs::RunStore for MongoStore {
    async fn create_run(
        &self,
        company: &CompanyId,
        spec: crate::ports::runs::NewRun,
    ) -> Result<crate::ports::runs::RunRecord> {
        use crate::ports::runs::{RunRecord, RunStatus};

        // The ordinal comes from the same atomic `$inc` counter the event and
        // usage sequences use, keyed per card — so concurrent creates cannot
        // collide even across processes. `next_seq` is 0-based; attempts are
        // 1-based (`Attempt 1` is the first).
        // A card-less run (issue #983) is always attempt 1 — see the fs
        // backend's `create_run` for why the ordinal is not shared across them,
        // and note that a shared counter here would be worse still: it is
        // durable, so every chat turn a company ever ran would keep counting up.
        let attempt = match &spec.task_id {
            Some(task_id) => self
                .next_seq(company, &format!("run:{task_id}"))
                .await?
                .saturating_add(1),
            None => 1,
        };
        let run = RunRecord {
            id: spec.id,
            company: company.clone(),
            task_id: spec.task_id,
            agent_id: spec.agent_id,
            chat_id: spec.chat_id,
            thread_root: spec.thread_root,
            workflow_run_id: spec.workflow_run_id,
            node_id: spec.node_id,
            attempt: attempt as u32,
            status: RunStatus::Pending,
            trigger_event_seq: None,
            created_at_millis: now_millis(),
            started_at_millis: None,
            finished_at_millis: None,
            error: None,
            usage: Default::default(),
            step_count: 0,
        };
        // A plain insert, not an upsert: the unique `(company_id, run_id)`
        // index is what turns a repeated id into the port's documented
        // conflict instead of a silently overwritten attempt.
        let existing = self
            .collection("runs")
            .find_one(doc! {"company_id": company.as_ref(), "run_id": &run.id})
            .await
            .map_err(mongo_err)?;
        if existing.is_some() {
            return Err(OpenCompanyError::Conflict(format!(
                "run '{}' already exists",
                run.id
            )));
        }
        self.collection("runs")
            .insert_one(doc! {
                "company_id": company.as_ref(),
                "run_id": &run.id,
                "task_id": run.task_id.as_deref(),
                "workflow_run_id": run.workflow_run_id.as_deref(),
                "agent_id": run.agent_id.as_str(),
                "status": run.status.as_str(),
                "attempt": run.attempt as i64,
                "created_ms": run.created_at_millis as i64,
                "run_json": serde_json::to_string(&run)?,
            })
            .await
            .map_err(mongo_err)?;
        Ok(run)
    }

    async fn get_run(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<crate::ports::runs::RunRecord>> {
        let found = self
            .collection("runs")
            .find_one(doc! {"company_id": company.as_ref(), "run_id": id})
            .await
            .map_err(mongo_err)?;
        match found {
            Some(doc) => Ok(Some(serde_json::from_str(&get_str(&doc, "run_json")?)?)),
            None => Ok(None),
        }
    }

    async fn put_run(
        &self,
        company: &CompanyId,
        run: &crate::ports::runs::RunRecord,
    ) -> Result<()> {
        self.collection("runs")
            .update_one(
                doc! {"company_id": company.as_ref(), "run_id": &run.id},
                doc! {"$set": {
                    "task_id": run.task_id.as_deref(),
                    "workflow_run_id": run.workflow_run_id.as_deref(),
                    "agent_id": run.agent_id.as_str(),
                    "status": run.status.as_str(),
                    "attempt": run.attempt as i64,
                    "created_ms": run.created_at_millis as i64,
                    "run_json": serde_json::to_string(run)?,
                }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn list_runs(
        &self,
        company: &CompanyId,
        filter: &crate::ports::runs::RunFilter,
    ) -> Result<Vec<crate::ports::runs::RunRecord>> {
        let mut query = doc! {"company_id": company.as_ref()};
        if let Some(task_id) = &filter.task_id {
            query.insert("task_id", task_id.as_str());
        }
        if let Some(workflow_run_id) = &filter.workflow_run_id {
            query.insert("workflow_run_id", workflow_run_id.as_str());
        }
        if let Some(agent_id) = &filter.agent_id {
            query.insert("agent_id", agent_id.as_str());
        }
        if !filter.statuses.is_empty() {
            let statuses: Vec<&str> = filter.statuses.iter().map(|s| s.as_str()).collect();
            query.insert("status", doc! {"$in": statuses});
        }
        // The canonical port ordering (see `runs::sort_newest_first`), pushed
        // into the query so the limit truncates the right end.
        let runs = self.collection("runs");
        let mut find = runs
            .find(query)
            .sort(doc! {"created_ms": -1, "attempt": -1, "run_id": -1});
        if let Some(limit) = filter.limit {
            match find_limit(limit) {
                FindLimit::Empty => return Ok(Vec::new()),
                FindLimit::Unlimited => {}
                FindLimit::AtMost(n) => find = find.limit(n),
            }
        }
        let mut cursor = find.await.map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "run_json")?)?);
        }
        Ok(out)
    }

    async fn append_run_step(
        &self,
        company: &CompanyId,
        step: &crate::ports::runs::RunStepRecord,
    ) -> Result<()> {
        // Upsert on `(run_id, step_seq)`: a replayed append overwrites rather
        // than duplicating, matching the other two backends.
        self.collection("run_steps")
            .update_one(
                doc! {
                    "company_id": company.as_ref(),
                    "run_id": &step.run_id,
                    "step_seq": step.step_seq as i64,
                },
                doc! {"$set": {
                    "at_ms": step.at_millis as i64,
                    "step_json": serde_json::to_string(step)?,
                }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn list_run_steps(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Vec<crate::ports::runs::RunStepRecord>> {
        let mut cursor = self
            .collection("run_steps")
            .find(doc! {"company_id": company.as_ref(), "run_id": run_id})
            .sort(doc! {"step_seq": 1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "step_json")?)?);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// ScheduleFireStore (issue #241)
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::schedule_fires::ScheduleFireStore for MongoStore {
    async fn claim_fire(
        &self,
        company: &CompanyId,
        schedule_id: &str,
        minute: u64,
    ) -> Result<bool> {
        // A plain `insert_one` against the unique `(company_id, schedule_id,
        // scheduled_for)` index. Success means this caller won the race; an
        // `E11000` duplicate-key means a peer — another replica, or this process
        // before a restart — already claimed the instant, so the caller lost and
        // must skip. Every OTHER driver error propagates: a claim store that
        // cannot answer must fail closed at the scheduler, never be read as a win.
        let result = self
            .collection("schedule_fires")
            .insert_one(doc! {
                "company_id": company.as_ref(),
                "schedule_id": schedule_id,
                "scheduled_for": minute as i64,
                "claimed_at_ms": now_millis() as i64,
            })
            .await;
        match result {
            Ok(_) => Ok(true),
            Err(e) if is_duplicate_key(&e) => Ok(false),
            Err(e) => Err(mongo_err(e)),
        }
    }

    async fn latest_fire(&self, company: &CompanyId, schedule_id: &str) -> Result<Option<u64>> {
        // The single newest row for this schedule, straight off the compound
        // index — no aggregation needed. A schedule that never fired matches
        // nothing and yields `None`, the no-anchor case.
        let found = self
            .collection("schedule_fires")
            .find_one(doc! {"company_id": company.as_ref(), "schedule_id": schedule_id})
            .sort(doc! {"scheduled_for": -1})
            .await
            .map_err(mongo_err)?;
        match found {
            Some(doc) => Ok(Some(get_i64(&doc, "scheduled_for")? as u64)),
            None => Ok(None),
        }
    }

    async fn prune_fires_before(&self, company: &CompanyId, cutoff_minute: u64) -> Result<usize> {
        let result = self
            .collection("schedule_fires")
            .delete_many(doc! {
                "company_id": company.as_ref(),
                "scheduled_for": {"$lt": cutoff_minute as i64},
            })
            .await
            .map_err(mongo_err)?;
        Ok(result.deleted_count as usize)
    }

    async fn delete_schedule_fires(&self, company: &CompanyId, schedule_id: &str) -> Result<usize> {
        // Every row for one schedule, whatever its minute — the delete-time
        // purge (#708). A never-fired id matches nothing, so `deleted_count`
        // is 0.
        let result = self
            .collection("schedule_fires")
            .delete_many(doc! {
                "company_id": company.as_ref(),
                "schedule_id": schedule_id,
            })
            .await
            .map_err(mongo_err)?;
        Ok(result.deleted_count as usize)
    }
}

// ---------------------------------------------------------------------------
// JournalStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::journal::JournalStore for MongoStore {
    /// One journaled `insert_one` per record, sequenced by the same atomic
    /// `counters` allocator [`EventLog`] uses.
    ///
    /// A document insert is atomic server-side, so the torn/merged-line class
    /// the filesystem backend had to be fixed for (#386) cannot arise here at
    /// all. The seq comes from the server rather than from a read-then-write, so
    /// two hosts of one tenant overlapping across a rolling deploy interleave
    /// without colliding — the same physics as `O_APPEND`, and the reason a
    /// cross-process lock is not the missing piece.
    ///
    /// Fail-closed: an errored or timed-out insert returns `Err` before the
    /// caller runs the side effect. The residual case — a timeout on a write the
    /// server did commit — leaves a committed key with no effect, which is the
    /// at-most-once contract's documented safe direction.
    ///
    /// Both durability levels are honoured (issue #392):
    /// [`Durability::Host`](crate::ports::journal::Durability::Host) writes with
    /// `j:true`, so the server has committed the insert to its own on-disk
    /// journal before it acknowledges; `Process` takes the default
    /// acknowledgement, which returns once the primary holds the write in
    /// memory. Flattening these together would either tax the journal's
    /// highest-volume records with a disk flush each, or drop the guarantee that
    /// keeps an already-fired effect from firing again after a primary crash.
    async fn append_journal(
        &self,
        company: &CompanyId,
        line: &str,
        durability: crate::ports::journal::Durability,
    ) -> Result<()> {
        use crate::ports::journal::Durability;

        let seq = self.next_seq(company, JOURNAL).await?;
        let collection = match durability {
            Durability::Host => self.journaled(JOURNAL),
            Durability::Process => self.collection(JOURNAL),
        };
        collection
            .insert_one(doc! {
                "company_id": company.as_ref(),
                "seq": seq as i64,
                "line": line,
            })
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    /// One ordered scan of this company's records, cursor-batched by the driver.
    ///
    /// Deliberately unbounded, and **not** a capped collection: capping would
    /// silently drop the oldest records, and the oldest records are exactly the
    /// at-most-once keys an effect from months ago committed. Un-committing one
    /// re-arms that effect. Bounding the journal is a retention question with a
    /// correctness argument attached (#575/#275 territory), not something a
    /// storage-shape default gets to decide.
    async fn read_journal(&self, company: &CompanyId) -> Result<Vec<String>> {
        let mut cursor = self
            .collection(JOURNAL)
            .find(doc! {"company_id": company.as_ref()})
            .sort(doc! {"seq": 1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(get_str(&doc, "line")?);
        }
        Ok(out)
    }

    async fn journal_imported(&self, company: &CompanyId) -> Result<bool> {
        Ok(self
            .collection(JOURNAL_IMPORTS)
            .find_one(doc! {"company_id": company.as_ref()})
            .await
            .map_err(mongo_err)?
            .is_some())
    }

    /// Clear, copy, receipt — in that order, and the order carries the whole
    /// safety argument.
    ///
    /// MongoDB gives no transaction across three collections without a replica
    /// set, so this cannot be atomic the way the sqlite backend's import is.
    /// It does not need to be: the receipt is written **last**, so an import
    /// interrupted anywhere leaves the gate open and the next boot re-runs the
    /// clear-and-copy from the top. What must never happen is a *partial* copy
    /// behind a *closed* gate — a journal missing its tail is a set of
    /// at-most-once keys that quietly went missing — and writing the receipt
    /// last is what makes that unreachable.
    ///
    /// The copy is one **ordered** `insert_many`, so the records land in the
    /// order they were read and the sequence numbers mean what they say.
    async fn complete_import(&self, company: &CompanyId, lines: Vec<String>) -> Result<()> {
        let journal = self.journaled(JOURNAL);
        journal
            .delete_many(doc! {"company_id": company.as_ref()})
            .await
            .map_err(mongo_err)?;
        if !lines.is_empty() {
            let docs: Vec<Document> = lines
                .iter()
                .enumerate()
                .map(|(seq, line)| {
                    doc! {
                        "company_id": company.as_ref(),
                        "seq": seq as i64,
                        "line": line.as_str(),
                    }
                })
                .collect();
            journal.insert_many(docs).await.map_err(mongo_err)?;
        }
        // The counter has to move too, or the first append after an import would
        // hand out seq 0 and collide with the copy's first row on the unique
        // index. `$max` rather than `$set`, so a counter that is somehow already
        // ahead is never wound back.
        self.collection("counters")
            .update_one(
                doc! {"_id": format!("{}:{JOURNAL}", company.as_ref())},
                doc! {"$max": {"next": lines.len() as i64}},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        self.journaled(JOURNAL_IMPORTS)
            .update_one(
                doc! {"company_id": company.as_ref()},
                doc! {"$set": {"at_ms": now_millis() as i64, "lines": lines.len() as i64}},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// UsageMeter
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::usage::UsageMeter for MongoStore {
    async fn record(
        &self,
        company: &CompanyId,
        sample: &crate::ports::usage::UsageSample,
    ) -> Result<()> {
        let seq = self.next_seq(company, "usage").await?;
        self.collection("usage_samples")
            .insert_one(doc! {
                "company_id": company.as_ref(),
                "seq": seq as i64,
                "at_ms": sample.at_millis as i64,
                "sample_json": serde_json::to_string(sample)?,
            })
            .await
            .map_err(mongo_err)?;
        // Retention: drop samples older than the 90-day window, anchored to the
        // newest sample just written.
        let cutoff = crate::ports::usage::retention_cutoff(sample.at_millis);
        self.collection("usage_samples")
            .delete_many(doc! {
                "company_id": company.as_ref(),
                "at_ms": {"$lt": cutoff as i64},
            })
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn query(
        &self,
        company: &CompanyId,
        since_millis: u64,
    ) -> Result<Vec<crate::ports::usage::UsageSample>> {
        let mut cursor = self
            .collection("usage_samples")
            .find(doc! {"company_id": company.as_ref(), "at_ms": {"$gte": since_millis as i64}})
            .sort(doc! {"at_ms": 1, "seq": 1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "sample_json")?)?);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// SkillStateStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::skills_state::SkillStateStore for MongoStore {
    async fn list(
        &self,
        company: &CompanyId,
    ) -> Result<Vec<crate::ports::skills_state::SkillState>> {
        let mut cursor = self
            .collection("skill_state")
            .find(doc! {"company_id": company.as_ref()})
            .sort(doc! {"slug": 1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(serde_json::from_str(&get_str(&doc, "state_json")?)?);
        }
        Ok(out)
    }

    async fn set(
        &self,
        company: &CompanyId,
        state: &crate::ports::skills_state::SkillState,
    ) -> Result<()> {
        self.collection("skill_state")
            .update_one(
                doc! {"company_id": company.as_ref(), "slug": &state.slug},
                doc! {"$set": {"state_json": serde_json::to_string(state)?}},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn remove(&self, company: &CompanyId, slug: &str) -> Result<bool> {
        let res = self
            .collection("skill_state")
            .delete_one(doc! {"company_id": company.as_ref(), "slug": slug})
            .await
            .map_err(mongo_err)?;
        Ok(res.deleted_count > 0)
    }
}

// ---------------------------------------------------------------------------
// ReadStateStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::read_state::ReadStateStore for MongoStore {
    async fn list(
        &self,
        company: &CompanyId,
        user: &str,
    ) -> Result<Vec<crate::ports::read_state::ChannelRead>> {
        let mut cursor = self
            .collection("channel_read_state")
            .find(doc! {"company_id": company.as_ref(), "user_id": user})
            .sort(doc! {"channel_id": 1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(d) = cursor.try_next().await.map_err(mongo_err)? {
            out.push(crate::ports::read_state::ChannelRead {
                channel_id: get_str(&d, "channel_id")?,
                last_read_at: d.get_i64("last_read_ms").unwrap_or_default(),
            });
        }
        Ok(out)
    }

    async fn mark(
        &self,
        company: &CompanyId,
        user: &str,
        channel_id: &str,
        at: i64,
    ) -> Result<crate::ports::read_state::ChannelRead> {
        let key = doc! {
            "company_id": company.as_ref(),
            "user_id": user,
            "channel_id": channel_id,
        };
        // `$max` rather than `$set`, for the monotonicity the port promises —
        // and on an upsert it seeds the field, so no `$setOnInsert` twin (which
        // Mongo would reject as a conflicting path anyway).
        self.collection("channel_read_state")
            .update_one(key.clone(), doc! {"$max": {"last_read_ms": at}})
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        let settled = self
            .collection("channel_read_state")
            .find_one(key)
            .await
            .map_err(mongo_err)?
            .and_then(|d| d.get_i64("last_read_ms").ok())
            .unwrap_or(at);
        Ok(crate::ports::read_state::ChannelRead {
            channel_id: channel_id.to_string(),
            last_read_at: settled,
        })
    }
}

// ---------------------------------------------------------------------------
// NotificationStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::notifications::NotificationStore for MongoStore {
    async fn append(
        &self,
        company: &CompanyId,
        notification: &crate::ports::notifications::Notification,
    ) -> Result<()> {
        // Idempotent by id (first write wins): `$setOnInsert` seeds the record
        // only when it does not yet exist, so a retried or replayed append is a
        // no-op rather than a duplicate. The unique (company_id, id) index in
        // `ensure_indexes` is the backstop against a concurrent double insert.
        // All three backends agree: no duplicate id within a company.
        self.collection("notifications")
            .update_one(
                doc! {"company_id": company.as_ref(), "id": notification.id.as_str()},
                doc! {"$setOnInsert": {
                    "kind": notification.kind.as_str(),
                    "subject_kind": notification.subject.kind.as_str(),
                    "subject_id": notification.subject.id.as_str(),
                    "title": notification.title.as_str(),
                    "created_ms": notification.created_at as i64,
                    // Absent (not null) for a company-wide row, so a document
                    // written before this field and one written after it read
                    // back identically.
                    "audience": notification.audience.as_ref().map(|a| {
                        a.iter().map(|id| mongodb::bson::Bson::String(id.clone())).collect::<Vec<_>>()
                    }),
                    "context": notification.context.as_deref(),
                }},
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    async fn list(
        &self,
        company: &CompanyId,
        user: &str,
    ) -> Result<Vec<crate::ports::notifications::NotificationView>> {
        // This person's read markers first, into an id → read_ms map.
        let mut reads: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        let mut read_cursor = self
            .collection("notification_reads")
            .find(doc! {"company_id": company.as_ref(), "user_id": user})
            .await
            .map_err(mongo_err)?;
        while let Some(d) = read_cursor.try_next().await.map_err(mongo_err)? {
            reads.insert(
                get_str(&d, "notification_id")?,
                d.get_i64("read_ms").unwrap_or_default(),
            );
        }
        // The records, newest first; the sort is the trait's contract.
        let mut cursor = self
            .collection("notifications")
            .find(doc! {"company_id": company.as_ref()})
            .sort(doc! {"created_ms": -1, "id": -1})
            .await
            .map_err(mongo_err)?;
        let mut out = Vec::new();
        while let Some(d) = cursor.try_next().await.map_err(mongo_err)? {
            let id = get_str(&d, "id")?;
            let subject_kind = get_str(&d, "subject_kind")?;
            let subject_kind = crate::ports::notifications::SubjectKind::from_token(&subject_kind)
                .ok_or_else(|| {
                    crate::error::OpenCompanyError::Store(format!(
                        "notification {id} has an unknown subject kind {subject_kind:?}"
                    ))
                })?;
            let read_at = reads.get(&id).map(|v| *v as u64);
            out.push(crate::ports::notifications::NotificationView {
                notification: crate::ports::notifications::Notification {
                    id,
                    kind: get_str(&d, "kind")?,
                    subject: crate::ports::notifications::Subject {
                        kind: subject_kind,
                        id: get_str(&d, "subject_id")?,
                    },
                    created_at: d.get_i64("created_ms").unwrap_or_default() as u64,
                    title: get_str(&d, "title")?,
                    audience: d.get_array("audience").ok().map(|a| {
                        a.iter()
                            .filter_map(|b| b.as_str().map(str::to_string))
                            .collect()
                    }),
                    context: d.get_str("context").ok().map(str::to_string),
                },
                read_at,
            });
        }
        // Filtered here rather than in the query, so the audience rule lives in
        // exactly one place (`Notification::visible_to`) across all three
        // backends. A company's feed is small; if that stops being true, the
        // server-side form is `{$or: [{audience: {$exists: false}}, {audience: user}]}`
        // and must be introduced *with* a conformance case, not instead of one.
        out.retain(|view| view.notification.visible_to(user));
        Ok(out)
    }

    async fn mark_read(
        &self,
        company: &CompanyId,
        user: &str,
        ids: Option<&[String]>,
    ) -> Result<u64> {
        let now = crate::ports::now_millis() as i64;
        // Which notifications to mark: the named ids that actually exist in this
        // company, or every notification when `None`.
        let targets: Vec<String> = match ids {
            Some(ids) => {
                let mut present = Vec::new();
                for id in ids {
                    let found = self
                        .collection("notifications")
                        .find_one(doc! {"company_id": company.as_ref(), "id": id.as_str()})
                        .await
                        .map_err(mongo_err)?;
                    if found.is_some() {
                        present.push(id.clone());
                    }
                }
                present
            }
            // Only what this person can see. Reuses the same projection `list`
            // does, so "mark all read" and "what is listed" cannot disagree.
            None => crate::ports::notifications::NotificationStore::list(self, company, user)
                .await?
                .into_iter()
                .map(|v| v.notification.id)
                .collect(),
        };
        for id in &targets {
            // `$setOnInsert` is the latch: it seeds `read_ms` only when the
            // marker is first created, so a re-mark leaves the original instant.
            self.collection("notification_reads")
                .update_one(
                    doc! {
                        "company_id": company.as_ref(),
                        "user_id": user,
                        "notification_id": id.as_str(),
                    },
                    doc! {"$setOnInsert": {"read_ms": now}},
                )
                .with_options(UpdateOptions::builder().upsert(true).build())
                .await
                .map_err(mongo_err)?;
        }
        // Still-unread count for this person, derived from the same projection
        // `list` uses so the two never disagree. Fully qualified because
        // `MongoStore` implements several ports that each expose a `list`.
        let unread = crate::ports::notifications::NotificationStore::list(self, company, user)
            .await?
            .iter()
            .filter(|v| v.read_at.is_none())
            .count() as u64;
        Ok(unread)
    }
}

// ---------------------------------------------------------------------------
// WorkspaceStore
// ---------------------------------------------------------------------------

/// The GridFS bucket every workspace payload lives in.
///
/// One bucket for the whole database rather than one per company: GridFS
/// buckets are a pair of collections, and a per-company bucket would mint two
/// collections per tenant in the shared-database mode this backend exists to
/// serve. Isolation is where it is everywhere else in this file — a
/// `company_id` on every document and a filter on every query — see
/// [`MongoStore::blob_filter`].
const BLOB_BUCKET: &str = "workspace_blobs";

/// How long a blob must have existed before [`MongoStore::sweep_orphan_blobs`]
/// will treat "no node document" as *abandoned* rather than *in flight*
/// (issue #664).
///
/// The window the sweep must not delete into is between `put_blob` returning
/// and the `workspace_nodes` insert landing — normally milliseconds, but a
/// concurrent boot only has to land inside it once to destroy a live upload,
/// and the damage is permanent: a node whose download 404s and whose `size`
/// still counts against the quota.
///
/// An hour is far longer than that window and deliberately so. The cost of
/// being generous is bounded and dull — a genuinely abandoned blob occupies
/// disk until the first boot after it becomes old enough, which may be much
/// later on a long-lived deployment — while the cost of being tight is
/// destroying a tenant's upload. The asymmetry is the whole argument, so do not
/// tune this down without one.
const ORPHAN_BLOB_MIN_AGE_MS: i64 = 60 * 60 * 1000;

impl MongoStore {
    /// The workspace payload bucket.
    fn blobs(&self) -> mongodb::gridfs::GridFsBucket {
        self.db.gridfs_bucket(
            mongodb::options::GridFsBucketOptions::builder()
                .bucket_name(BLOB_BUCKET.to_string())
                .build(),
        )
    }

    /// The tenancy-scoped filter naming exactly one node's payload.
    ///
    /// **Both keys, every time.** A filter on `node_id` alone would be a
    /// cross-tenant read the moment two companies in one database minted the
    /// same node id — and node ids come from a shared generator, so that is a
    /// collision away rather than an attack away. Every read, every delete and
    /// the boot sweep go through this function so there is no call site that
    /// could have forgotten the company half.
    fn blob_filter(company: &CompanyId, node_id: &str) -> Document {
        doc! {
            "metadata.company_id": company.as_ref(),
            "metadata.node_id": node_id,
        }
    }

    /// Uploads `bytes` as this node's payload and returns the GridFS file id.
    ///
    /// The blob is written **before** the node document that names it, on both
    /// the create and the replace path. See
    /// [`create_binary`](crate::ports::workspace::WorkspaceStore::create_binary)
    /// on this type for why that direction. The id names exactly the upload
    /// this call made, so a caller that must reclaim its own payload on a
    /// losing insert can do so without touching a concurrent writer's blob for
    /// the same node.
    async fn put_blob(
        &self,
        company: &CompanyId,
        node_id: &str,
        filename: &str,
        bytes: &[u8],
    ) -> Result<mongodb::bson::Bson> {
        use futures::io::AsyncWriteExt;
        let mut upload = self
            .blobs()
            .open_upload_stream(filename)
            .metadata(doc! {
                "company_id": company.as_ref(),
                "node_id": node_id,
            })
            .await
            .map_err(mongo_err)?;
        // The id is minted when the stream opens and is the files-collection
        // `_id` the close commits — capture it here, before close, because the
        // caller may need to delete exactly this upload.
        let id = upload.id().clone();
        upload
            .write_all(bytes)
            .await
            .map_err(|e| mongo_err(format!("writing a workspace blob failed: {e}")))?;
        // `close` is what commits the final chunk and the files-collection
        // document. Without it the upload is a partial write that no reader can
        // see — so its failure is the write's failure.
        upload
            .close()
            .await
            .map_err(|e| mongo_err(format!("closing a workspace blob failed: {e}")))?;
        Ok(id)
    }

    /// Removes exactly the payload one call uploaded, not every payload for
    /// the node id.
    ///
    /// The conflict path of a same-id `create_binary` race must not reclaim
    /// the winning call's bytes (issue #1694): both racers uploaded under the
    /// same fresh node id, so a sweep over the id would take the winner's
    /// payload down with the loser's. The tenancy+node filter is the same
    /// isolation boundary as [`blob_filter`](MongoStore::blob_filter) — a
    /// stray id must never be deletable without proving which node owns it.
    async fn delete_blob(
        &self,
        company: &CompanyId,
        node_id: &str,
        file_id: &mongodb::bson::Bson,
    ) -> Result<()> {
        let bucket = self.blobs();
        let file = bucket
            .find_one(doc! {
                "_id": file_id,
                "metadata.company_id": company.as_ref(),
                "metadata.node_id": node_id,
            })
            .await
            .map_err(mongo_err)?;
        if let Some(file) = file {
            bucket.delete(file.id).await.map_err(mongo_err)?;
        }
        Ok(())
    }

    /// Removes every payload registered to `node_id`, except optionally the one
    /// just written.
    ///
    /// `keep` exists for the replace path: the new blob is uploaded before the
    /// node document is updated, so at that moment the filter matches both
    /// generations and the older one is the one to drop.
    async fn drop_blobs(
        &self,
        company: &CompanyId,
        node_id: &str,
        keep: Option<&mongodb::bson::Bson>,
    ) -> Result<()> {
        let bucket = self.blobs();
        let mut cursor = bucket
            .find(Self::blob_filter(company, node_id))
            .await
            .map_err(mongo_err)?;
        while let Some(file) = cursor.try_next().await.map_err(mongo_err)? {
            if keep == Some(&file.id) {
                continue;
            }
            bucket.delete(file.id).await.map_err(mongo_err)?;
        }
        Ok(())
    }

    /// Deletes blobs whose node document is gone (issue #553).
    ///
    /// The crash window this closes is the one the write ordering deliberately
    /// leaves open: a payload uploaded, then the process dies before the node
    /// document lands. That leaves a blob nothing references — invisible to
    /// every reader, but occupying the tenant's quota forever. The opposite
    /// ordering would have left a node whose download 404s, which is worse, so
    /// this sweep is the price of choosing the survivable direction.
    ///
    /// Runs at construction, where it is cheap (one scan of the files
    /// collection against one projection of the node ids) and where a partial
    /// failure is safe: a sweep that cannot complete is logged and the store is
    /// returned anyway, because refusing to open a company's storage over
    /// reclaimable disk would trade an invisible cost for a total outage.
    ///
    /// ## Only blobs older than [`ORPHAN_BLOB_MIN_AGE_MS`] (issue #664)
    ///
    /// "No node document" describes an abandoned blob **and** a blob whose
    /// writer is still running, and the two are indistinguishable by reference
    /// alone. Without an age bound this sweep deletes live uploads: a second
    /// process — a rolling deploy, a restarted replica, another tenant's
    /// container on a shared database — boots between a peer's `put_blob` and
    /// its node insert, sees a blob with no node, and reclaims it. The peer's
    /// insert then lands, and the company keeps a node whose download 404s
    /// forever while its recorded `size` still counts against `tree_quota_gb`.
    /// Neither half self-heals, because this sweep only ever deletes blobs
    /// without nodes and never nodes without blobs.
    ///
    /// That is the failure the write ordering exists to avoid, reintroduced by
    /// the thing meant to clean up after it — and the *shared-single-DB* mode
    /// makes it cross-tenant, since this is the one query in this backend that
    /// carries no `company_id` (see the multi-tenancy note at the top of this
    /// module). An age bound closes both: an in-flight upload is young, an
    /// abandoned one is not, and the filter runs server-side so a young blob is
    /// never even read.
    ///
    /// **Not fixed by reordering the writes.** Node-first would replace an
    /// invisible orphan with a node whose bytes are absent — the outcome
    /// `fs_ops::create_binary` rejects in as many words ("only one of those is
    /// survivable"). The ordering is right; the sweep was wrong.
    async fn sweep_orphan_blobs(&self) -> Result<u64> {
        let live: std::collections::HashSet<(String, String)> = {
            let mut cursor = self
                .collection("workspace_nodes")
                .find(doc! {})
                .await
                .map_err(mongo_err)?;
            let mut out = std::collections::HashSet::new();
            while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
                if let (Ok(company), Ok(node)) = (doc.get_str("company_id"), doc.get_str("node_id"))
                {
                    out.insert((company.to_string(), node.to_string()));
                }
            }
            out
        };
        let bucket = self.blobs();
        // Server-side, so a blob younger than the bound is never even read —
        // and so the bound cannot be defeated by a `continue` added later to
        // the loop below. `uploadDate` is GridFS's own field on the files
        // document, written when the upload completes.
        let cutoff = mongodb::bson::DateTime::from_millis(
            (now_millis() as i64).saturating_sub(ORPHAN_BLOB_MIN_AGE_MS),
        );
        let mut cursor = bucket
            .find(doc! { "uploadDate": { "$lt": cutoff } })
            .await
            .map_err(mongo_err)?;
        let mut removed = 0;
        while let Some(file) = cursor.try_next().await.map_err(mongo_err)? {
            let key = file.metadata.as_ref().and_then(|m| {
                Some((
                    m.get_str("company_id").ok()?.to_string(),
                    m.get_str("node_id").ok()?.to_string(),
                ))
            });
            // A blob with no usable metadata cannot be matched to a node and is
            // therefore an orphan by definition — it predates this scheme or was
            // written by something that is not this store.
            let orphan = key.map(|k| !live.contains(&k)).unwrap_or(true);
            if orphan {
                bucket.delete(file.id).await.map_err(mongo_err)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Loads every workspace node for a company into an id-keyed map.
    async fn workspace_nodes(
        &self,
        company: &CompanyId,
    ) -> Result<HashMap<String, crate::ports::workspace::WorkspaceNode>> {
        let mut cursor = self
            .collection("workspace_nodes")
            .find(doc! {"company_id": company.as_ref()})
            .await
            .map_err(mongo_err)?;
        let mut out = HashMap::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            let node: crate::ports::workspace::WorkspaceNode =
                serde_json::from_str(&get_str(&doc, "node_json")?)?;
            out.insert(node.id.clone(), node);
        }
        Ok(out)
    }
}

#[async_trait]
impl crate::ports::workspace::WorkspaceStore for MongoStore {
    async fn tree(
        &self,
        company: &CompanyId,
    ) -> Result<Vec<crate::ports::workspace::WorkspaceNode>> {
        Ok(self.workspace_nodes(company).await?.into_values().collect())
    }

    async fn read(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<(crate::ports::workspace::WorkspaceNode, String)>> {
        let doc = self
            .collection("workspace_nodes")
            .find_one(doc! {"company_id": company.as_ref(), "node_id": id})
            .await
            .map_err(mongo_err)?;
        match doc {
            Some(doc) => {
                let node: crate::ports::workspace::WorkspaceNode =
                    serde_json::from_str(&get_str(&doc, "node_json")?)?;
                // A binary node reads as an empty body, like a folder — the port
                // contract that keeps every prose-shaped caller correct.
                let content = if node.is_binary() {
                    String::new()
                } else {
                    get_str(&doc, "content")?
                };
                Ok(Some((node, content)))
            }
            None => Ok(None),
        }
    }

    async fn read_capped(
        &self,
        company: &CompanyId,
        id: &str,
        max_bytes: u64,
    ) -> Result<Option<(crate::ports::workspace::WorkspaceNode, String, u64)>> {
        // Projected server-side, so an over-cap body stays in the server: the
        // length is computed there and the content field is replaced with an
        // empty string unless it fits. `$strLenBytes` is the same unit the cap
        // is expressed in — `$strLenCP` would admit a body up to four times it.
        let len_bytes = doc! {"$strLenBytes": {"$ifNull": ["$content", ""]}};
        let mut cursor = self
            .collection("workspace_nodes")
            .aggregate(vec![
                doc! {"$match": {"company_id": company.as_ref(), "node_id": id}},
                doc! {"$limit": 1},
                doc! {"$project": {
                    "node_json": 1,
                    "content_len": len_bytes.clone(),
                    "content": {"$cond": [
                        {"$lte": [len_bytes, clamp_max_bytes(max_bytes)]},
                        {"$ifNull": ["$content", ""]},
                        "",
                    ]},
                }},
            ])
            .await
            .map_err(mongo_err)?;
        let Some(doc) = cursor.try_next().await.map_err(mongo_err)? else {
            return Ok(None);
        };
        let node: crate::ports::workspace::WorkspaceNode =
            serde_json::from_str(&get_str(&doc, "node_json")?)?;
        if node.kind != crate::ports::workspace::NodeKind::File || node.is_binary() {
            return Ok(Some((node, String::new(), 0)));
        }
        let len = doc.get_i64("content_len").unwrap_or_else(|_| {
            doc.get_i32("content_len")
                .map(i64::from)
                .unwrap_or_default()
        });
        Ok(Some((node, get_str(&doc, "content")?, len.max(0) as u64)))
    }

    async fn write(
        &self,
        company: &CompanyId,
        id: &str,
        content: &str,
        author: crate::ports::workspace::WorkspaceOrigin,
    ) -> Result<crate::ports::workspace::WorkspaceNode> {
        use crate::ports::workspace::NodeKind;
        let doc = self
            .collection("workspace_nodes")
            .find_one(doc! {"company_id": company.as_ref(), "node_id": id})
            .await
            .map_err(mongo_err)?;
        let Some(doc) = doc else {
            return Err(OpenCompanyError::CompanyNotFound(format!(
                "workspace node {id}"
            )));
        };
        let mut node: crate::ports::workspace::WorkspaceNode =
            serde_json::from_str(&get_str(&doc, "node_json")?)?;
        if node.kind != NodeKind::File {
            return Err(OpenCompanyError::InvalidRequest(
                "cannot write content to a folder".to_string(),
            ));
        }
        if let Some(mime) = node.mime.clone() {
            return Err(OpenCompanyError::InvalidRequest(
                crate::ports::workspace::binary_write_refusal(&node.name, &mime),
            ));
        }
        node.updated_at_millis = now_millis();
        // Authorship rides the same stamp as the timestamp. The node is stored
        // as opaque JSON in `node_json`, so this needs no schema change.
        node.updated_by = author;
        self.collection("workspace_nodes")
            .update_one(
                doc! {"company_id": company.as_ref(), "node_id": id},
                doc! {"$set": {
                    "node_json": serde_json::to_string(&node)?,
                    "content": content,
                    "updated_ms": node.updated_at_millis as i64,
                }},
            )
            .await
            .map_err(mongo_err)?;
        Ok(node)
    }

    async fn create(
        &self,
        company: &CompanyId,
        node: &crate::ports::workspace::WorkspaceNode,
        content: Option<&str>,
    ) -> Result<()> {
        use crate::ports::workspace::NodeKind;
        let nodes = self.workspace_nodes(company).await?;
        if nodes.contains_key(&node.id) {
            return Err(OpenCompanyError::Conflict(format!(
                "workspace node {} already exists",
                node.id
            )));
        }
        if let Some(parent) = &node.parent_id {
            match nodes.get(parent) {
                Some(p) if p.kind == NodeKind::Folder => {}
                Some(_) => {
                    return Err(OpenCompanyError::InvalidRequest(
                        "parent is not a folder".to_string(),
                    ));
                }
                None => {
                    return Err(OpenCompanyError::InvalidRequest(
                        "parent folder does not exist".to_string(),
                    ));
                }
            }
        }
        let mut document = doc! {
            "company_id": company.as_ref(),
            "node_id": &node.id,
            "node_json": serde_json::to_string(node)?,
            "content": content.unwrap_or(""),
            "updated_ms": node.updated_at_millis as i64,
        };
        // Issue #697: claims this file's path against the partial unique index.
        if let Some(key) = node_path_key(node) {
            document.insert("file_path_key", key);
        }
        self.collection("workspace_nodes")
            .insert_one(document)
            .await
            // Issue #894: the index refusing a second file at one path is a
            // *contract* outcome, not a storage fault. It reached callers as
            // `Store("mongodb error: E11000 …")` — a 500 carrying a driver
            // string — where fs and SQLite both answer `Conflict`. The users
            // -email index has been mapped this way since it was added.
            .map_err(|e| {
                if is_duplicate_key(&e) {
                    OpenCompanyError::Conflict(crate::ports::workspace::duplicate_file_refusal(
                        &node.name,
                    ))
                } else {
                    mongo_err(e)
                }
            })?;
        Ok(())
    }

    /// Find-then-insert, with the **partial unique index deciding the race**
    /// and a re-read adopting the winner when it loses (issue #759).
    ///
    /// This is the only backend that cannot serialize a read-then-insert: there
    /// is no per-company lock and no transaction it can require of its
    /// deployment, so a find that says "free" is a statement about an instant
    /// and the insert acts on it later. The answer is the one issue #697 already
    /// established for files — let the database hold the line — applied to a
    /// second field.
    ///
    /// The re-read is what makes the loser *adopt* rather than fail. That is the
    /// whole difference from `swap_files`: two publishers wanting one folder want
    /// the same thing, so the one the index refuses must come back, find the
    /// winner's folder and use it.
    ///
    /// The retry is bounded. Each pass either adopts, inserts, or loses to a
    /// duplicate key — and a duplicate key means a competing folder now exists,
    /// which the next find must see. More than a couple of passes therefore
    /// means something is deleting folders as fast as they are created, and
    /// looping forever on that would hang a publish rather than fail it.
    async fn adopt_or_create_folder(
        &self,
        company: &CompanyId,
        parent: Option<&str>,
        name: &str,
        origin: crate::ports::workspace::WorkspaceOrigin,
    ) -> Result<crate::ports::workspace::FolderClaim> {
        use crate::ports::workspace::{FolderClaim, NodeKind, existing_folder_claim, new_folder};
        /// Enough to absorb a burst of concurrent claimers; see the doc above
        /// for why an unbounded loop would be worse than a refusal.
        const ATTEMPTS: usize = 8;

        for _ in 0..ATTEMPTS {
            let nodes = self.workspace_nodes(company).await?;
            if let Some(parent) = parent {
                match nodes.get(parent) {
                    Some(p) if p.kind == NodeKind::Folder => {}
                    Some(_) => {
                        return Err(OpenCompanyError::InvalidRequest(
                            "parent is not a folder".to_string(),
                        ));
                    }
                    None => {
                        return Err(OpenCompanyError::InvalidRequest(
                            "parent folder does not exist".to_string(),
                        ));
                    }
                }
            }
            if let Some(mut existing) = existing_folder_claim(nodes.values(), parent, name)? {
                // Stamp the adoption lease (issue #1839) via a `$set` on the
                // node document. This backend has no per-company lock, so the
                // flag narrows Race 1 to #1801's documented residual window
                // rather than closing it — a `delete_if_empty` racing this
                // `$set` can still miss it, and the loser's bounded retry above
                // re-mints if it does. Authorship is untouched.
                if !existing.adopted {
                    existing.adopted = true;
                    self.collection("workspace_nodes")
                        .update_one(
                            doc! {"company_id": company.as_ref(), "node_id": &existing.id},
                            doc! {"$set": {"node_json": serde_json::to_string(&existing)?}},
                        )
                        .await
                        .map_err(mongo_err)?;
                }
                return Ok(FolderClaim::Adopted(existing));
            }
            let node = new_folder(name, parent, origin.clone());
            let mut document = doc! {
                "company_id": company.as_ref(),
                "node_id": &node.id,
                "node_json": serde_json::to_string(&node)?,
                "content": "",
                "updated_ms": node.updated_at_millis as i64,
            };
            document.insert("folder_path_key", folder_path_key(parent, name));
            match self
                .collection("workspace_nodes")
                .insert_one(document)
                .await
            {
                Ok(_) => return Ok(FolderClaim::Created(node)),
                // Somebody else claimed the path between the find and the
                // insert. Nothing was written — this backend's create is one
                // document — so the next pass simply adopts their folder.
                Err(err) if is_duplicate_key(&err) => continue,
                Err(err) => return Err(mongo_err(err)),
            }
        }
        Err(OpenCompanyError::Conflict(format!(
            "the folder `{name}` could not be claimed after {ATTEMPTS} attempts; something is \
             creating and removing it concurrently"
        )))
    }

    /// GridFS — the only backend where the payload cannot ride in the record.
    ///
    /// A BSON document caps at 16 MB, and the artifacts this issue exists for
    /// are generated images and video that routinely exceed it, so the bytes go
    /// into a bucket and the node document keeps only the metadata.
    ///
    /// # Blob first, document second
    ///
    /// The two writes cannot be made atomic without a transaction this backend
    /// does not require of its deployment, so the ordering is chosen by which
    /// failure is survivable. A crash between them leaves a blob with no node:
    /// invisible to every reader, costing disk until
    /// [`sweep_orphan_blobs`](MongoStore::sweep_orphan_blobs) runs at the next
    /// boot. The reverse would leave a node the tree shows and the download
    /// 404s on — a file the operator can see and cannot fetch, with nothing to
    /// repair it. Deletion runs the mirror image (document first, blob second)
    /// for the same reason.
    async fn create_binary(
        &self,
        company: &CompanyId,
        node: &crate::ports::workspace::WorkspaceNode,
        bytes: &[u8],
    ) -> Result<crate::ports::workspace::WorkspaceNode> {
        use crate::ports::workspace::NodeKind;
        let node = crate::ports::workspace::stamped_binary(node, bytes)?;
        let nodes = self.workspace_nodes(company).await?;
        if nodes.contains_key(&node.id) {
            return Err(OpenCompanyError::Conflict(format!(
                "workspace node {} already exists",
                node.id
            )));
        }
        if let Some(parent) = &node.parent_id {
            match nodes.get(parent) {
                Some(p) if p.kind == NodeKind::Folder => {}
                Some(_) => {
                    return Err(OpenCompanyError::InvalidRequest(
                        "parent is not a folder".to_string(),
                    ));
                }
                None => {
                    return Err(OpenCompanyError::InvalidRequest(
                        "parent folder does not exist".to_string(),
                    ));
                }
            }
        }
        let file_id = self.put_blob(company, &node.id, &node.name, bytes).await?;
        let mut document = doc! {
            "company_id": company.as_ref(),
            "node_id": &node.id,
            "node_json": serde_json::to_string(&node)?,
            "content": "",
            "updated_ms": node.updated_at_millis as i64,
        };
        // Issue #697, as above.
        if let Some(key) = node_path_key(&node) {
            document.insert("file_path_key", key);
        }
        if let Err(err) = self
            .collection("workspace_nodes")
            .insert_one(document)
            .await
        {
            if !is_duplicate_key(&err) {
                return Err(mongo_err(err));
            }
            // Issue #894, as in `create` — but the blob uploaded above now has
            // no node document, which on this backend is the exact state the
            // boot sweep exists to reclaim. The sweep only runs at store
            // construction and spares blobs younger than an hour, so a repeated
            // name conflict (the chat-attachment flow's retry under a fresh id)
            // would otherwise strand the full payload as invisible GridFS disk
            // until some later restart. Reclaim it here instead, but ONLY the
            // upload this losing call made: a concurrent `create_binary` racing
            // the same fresh node id has uploaded its own payload under that id
            // too (issue #1694), and a sweep over the id would take the
            // winner's bytes down with the loser's. Best-effort: a drop failure
            // still leaves the sweep to finish the job, and the conflict is the
            // outcome the caller is owed either way.
            if let Err(drop_err) = self.delete_blob(company, &node.id, &file_id).await {
                tracing::warn!(
                    company = %company,
                    node_id = %node.id,
                    error = %drop_err,
                    "workspace create_binary conflict left its GridFS payload for the boot sweep"
                );
            }
            return Err(OpenCompanyError::Conflict(
                crate::ports::workspace::duplicate_file_refusal(&node.name),
            ));
        }
        // The stamped node, so the digest a caller records can only have come
        // from the store (issue #668).
        Ok(node)
    }

    async fn write_binary(
        &self,
        company: &CompanyId,
        id: &str,
        bytes: &[u8],
        mime: Option<&str>,
        author: crate::ports::workspace::WorkspaceOrigin,
    ) -> Result<crate::ports::workspace::WorkspaceNode> {
        let doc = self
            .collection("workspace_nodes")
            .find_one(doc! {"company_id": company.as_ref(), "node_id": id})
            .await
            .map_err(mongo_err)?;
        let Some(doc) = doc else {
            return Err(OpenCompanyError::CompanyNotFound(format!(
                "workspace node {id}"
            )));
        };
        let mut node: crate::ports::workspace::WorkspaceNode =
            serde_json::from_str(&get_str(&doc, "node_json")?)?;
        crate::ports::workspace::rebind_binary(&mut node, bytes, mime, author)?;
        // New blob, then the document, then the old blob — the same "a reader
        // never sees a node without its bytes" ordering as the create path. The
        // worst crash outcome is a superseded blob left behind for the boot
        // sweep.
        let superseded: Vec<mongodb::bson::Bson> = {
            let mut cursor = self
                .blobs()
                .find(Self::blob_filter(company, id))
                .await
                .map_err(mongo_err)?;
            let mut ids = Vec::new();
            while let Some(file) = cursor.try_next().await.map_err(mongo_err)? {
                ids.push(file.id);
            }
            ids
        };
        self.put_blob(company, id, &node.name, bytes).await?;
        self.collection("workspace_nodes")
            .update_one(
                doc! {"company_id": company.as_ref(), "node_id": id},
                doc! {"$set": {
                    "node_json": serde_json::to_string(&node)?,
                    "content": "",
                    "updated_ms": node.updated_at_millis as i64,
                }},
            )
            .await
            .map_err(mongo_err)?;
        let bucket = self.blobs();
        for old in superseded {
            bucket.delete(old).await.map_err(mongo_err)?;
        }
        Ok(node)
    }

    /// Streams straight out of GridFS — a video is never resident, which is the
    /// reason this backend needed a bucket rather than a bigger field.
    async fn read_bytes(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<
        Option<(
            crate::ports::workspace::WorkspaceNode,
            crate::ports::workspace::BlobStream,
        )>,
    > {
        let Some(doc) = self
            .collection("workspace_nodes")
            .find_one(doc! {"company_id": company.as_ref(), "node_id": id})
            .await
            .map_err(mongo_err)?
        else {
            return Ok(None);
        };
        let node: crate::ports::workspace::WorkspaceNode =
            serde_json::from_str(&get_str(&doc, "node_json")?)?;
        if !node.is_binary() {
            return Ok(None);
        }
        let bucket = self.blobs();
        // Located by the tenancy-scoped metadata filter rather than by an id
        // carried on the node document: the bucket is shared across companies,
        // so the company half of the filter is the isolation boundary and must
        // be applied by the query that finds the file, not checked afterwards.
        let Some(file) = bucket
            .find_one(Self::blob_filter(company, id))
            .await
            .map_err(mongo_err)?
        else {
            return Ok(None);
        };
        let stream = bucket
            .open_download_stream(file.id)
            .await
            .map_err(mongo_err)?;
        use tokio_util::compat::FuturesAsyncReadCompatExt;
        let reader = tokio_util::io::ReaderStream::new(stream.compat());
        Ok(Some((
            node,
            Box::pin(futures::StreamExt::map(reader, |chunk| {
                chunk.map_err(|e| {
                    OpenCompanyError::Store(format!("reading a workspace blob failed: {e}"))
                })
            })),
        )))
    }

    async fn rename_move(
        &self,
        company: &CompanyId,
        id: &str,
        name: Option<&str>,
        parent: Option<Option<&str>>,
    ) -> Result<crate::ports::workspace::WorkspaceNode> {
        use crate::ports::workspace::NodeKind;
        let nodes = self.workspace_nodes(company).await?;
        if !nodes.contains_key(id) {
            return Err(OpenCompanyError::CompanyNotFound(format!(
                "workspace node {id}"
            )));
        }
        // A move to root (`Some(None)`) never forms a cycle.
        if let Some(Some(parent)) = parent {
            if parent == id || mongo_workspace_descendants(&nodes, id).contains(parent) {
                return Err(OpenCompanyError::InvalidRequest(
                    "cannot move a folder into its own subtree".to_string(),
                ));
            }
            if nodes.get(parent).map(|p| p.kind) != Some(NodeKind::Folder) {
                return Err(OpenCompanyError::InvalidRequest(
                    "target parent is not a folder".to_string(),
                ));
            }
        }
        let mut node = nodes.get(id).cloned().expect("node present");
        if let Some(name) = name {
            node.name = name.to_string();
        }
        if let Some(parent) = parent {
            node.parent_id = parent.map(str::to_string);
        }
        node.updated_at_millis = now_millis();
        // A rename or a reparent moves the file to a different path, so its
        // claim has to move with it (issue #697) or the index would keep
        // guarding the path it left and ignore the one it took.
        let mut set = doc! {
            "node_json": serde_json::to_string(&node)?,
            "updated_ms": node.updated_at_millis as i64,
        };
        let mut unset = Document::new();
        match node_path_key(&node) {
            Some(key) => {
                set.insert("file_path_key", key);
            }
            None => {
                unset.insert("file_path_key", "");
            }
        }
        // A moved folder **drops** its claim rather than carrying it (issue
        // #759). The claim exists to decide a race between two publishers on the
        // publish walk; an operator who moved the folder by hand has taken it
        // out of that walk's reach, and a key that travelled with it would keep
        // guarding the path it left — refusing that path to every later publish
        // forever, which is the very outage this primitive exists to prevent.
        // Demoting to unguarded is what every console-made folder already is.
        if node.kind == NodeKind::Folder {
            unset.insert("folder_path_key", "");
        }
        let mut update = doc! {"$set": set};
        if !unset.is_empty() {
            update.insert("$unset", unset);
        }
        self.collection("workspace_nodes")
            .update_one(doc! {"company_id": company.as_ref(), "node_id": id}, update)
            .await
            .map_err(mongo_err)?;
        Ok(node)
    }

    async fn swap_files(
        &self,
        company: &CompanyId,
        expected_id: Option<&str>,
        replacement_id: &str,
        name: &str,
    ) -> Result<Option<crate::ports::workspace::WorkspaceNode>> {
        use crate::ports::workspace::NodeKind;

        let nodes = self.collection("workspace_nodes");
        let Some(staged_doc) = nodes
            .find_one(doc! {
                "company_id": company.as_ref(),
                "node_id": replacement_id,
            })
            .await
            .map_err(mongo_err)?
        else {
            return Err(OpenCompanyError::CompanyNotFound(format!(
                "workspace node {replacement_id}"
            )));
        };
        let staged_json = get_str(&staged_doc, "node_json")?.to_string();
        let replacement: crate::ports::workspace::WorkspaceNode =
            serde_json::from_str(&staged_json)?;
        if replacement.kind != NodeKind::File {
            return Err(OpenCompanyError::InvalidRequest(
                "only files can be promoted from a staging path".to_string(),
            ));
        }

        let expected_doc = match expected_id {
            Some(id) => nodes
                .find_one(doc! {
                    "company_id": company.as_ref(),
                    "node_id": id,
                })
                .await
                .map_err(mongo_err)?,
            None => None,
        };
        let expected = expected_doc
            .as_ref()
            .map(
                |document| -> Result<crate::ports::workspace::WorkspaceNode> {
                    let json = get_str(document, "node_json")?;
                    Ok(serde_json::from_str(&json)?)
                },
            )
            .transpose()?;
        // Only the replace arm can be decided by reading. The create arm is
        // decided below, by the unique index, because a read here would be a
        // check with a window after it.
        let still_current = expected_id.is_none()
            || expected.as_ref().is_some_and(|node| {
                node.kind == NodeKind::File
                    && node.name == name
                    && node.parent_id == replacement.parent_id
            });

        // Detach the private staging document before changing the expected
        // document's id: `(company_id, node_id)` is unique. The GridFS payload
        // deliberately stays in place, already keyed by `replacement_id`.
        // A crash here leaves the old deliverable intact and, at worst, an
        // orphan the boot sweep reclaims.
        let detached = nodes
            .delete_one(doc! {
                "company_id": company.as_ref(),
                "node_id": replacement_id,
                "node_json": &staged_json,
            })
            .await
            .map_err(mongo_err)?;
        if detached.deleted_count != 1 {
            return Err(OpenCompanyError::Conflict(
                "the staged workspace replacement changed before it could be promoted".to_string(),
            ));
        }

        if !still_current {
            self.drop_blobs(company, replacement_id, None).await?;
            return Ok(None);
        }

        let mut promoted = replacement.clone();
        promoted.name = name.to_string();
        promoted.updated_at_millis = now_millis();

        // Issue #697, the first-publish arm. The staged document has just been
        // detached, so re-inserting it under the final name is the whole
        // operation — and the partial unique index on `file_path_key` is what
        // decides it. Two publishers reaching here concurrently both attempt
        // the insert; exactly one satisfies the index and the other is refused
        // by the server, which is the only place this backend can hold that
        // line without a transaction.
        if expected_id.is_none() {
            let staged_content = staged_doc
                .get("content")
                .cloned()
                .unwrap_or_else(|| mongodb::bson::Bson::String(String::new()));
            let mut document = doc! {
                "company_id": company.as_ref(),
                "node_id": replacement_id,
                "node_json": serde_json::to_string(&promoted)?,
                "content": staged_content,
                "updated_ms": promoted.updated_at_millis as i64,
            };
            document.insert(
                "file_path_key",
                file_path_key(promoted.parent_id.as_deref(), &promoted.name),
            );
            return match nodes.insert_one(document).await {
                Ok(_) => Ok(Some(promoted)),
                Err(err) if is_duplicate_key(&err) => {
                    // Lost the race: somebody else holds this path. The staged
                    // node is already detached, so only its payload remains to
                    // reclaim — the same cleanup the replace arm owes. A
                    // cleanup failure must not turn the clean loss into a
                    // reported publish failure: the compare-and-swap already
                    // lost, and the boot orphan sweep finishes this later.
                    if let Err(err) = self.drop_blobs(company, replacement_id, None).await {
                        tracing::warn!(
                            company = %company,
                            node_id = %replacement_id,
                            error = %err,
                            "workspace first publish lost the race but staged GridFS payload \
                             cleanup was deferred"
                        );
                    }
                    Ok(None)
                }
                Err(err) => Err(mongo_err(err)),
            };
        }

        let expected_doc = expected_doc.expect("still_current requires an expected document");
        let expected_json = get_str(&expected_doc, "node_json")?.to_string();
        let expected_content = expected_doc
            .get("content")
            .cloned()
            .unwrap_or_else(|| mongodb::bson::Bson::String(String::new()));
        let staged_content = staged_doc
            .get("content")
            .cloned()
            .unwrap_or_else(|| mongodb::bson::Bson::String(String::new()));

        // One document update is the compare-and-swap. Including the complete
        // old JSON and content makes an in-place writer count as a competing
        // revision even if two timestamps happen to share a millisecond.
        let swapped = nodes
            .update_one(
                doc! {
                    "company_id": company.as_ref(),
                    "node_id": expected_id,
                    "node_json": &expected_json,
                    "content": expected_content,
                },
                doc! {"$set": {
                    "node_id": replacement_id,
                    "node_json": serde_json::to_string(&promoted)?,
                    "content": staged_content,
                    "updated_ms": promoted.updated_at_millis as i64,
                    // The promoted node now holds the path, so it takes the
                    // claim with it (issue #697). The superseded document is
                    // this same document, so there is no stale key left behind.
                    "file_path_key": file_path_key(
                        promoted.parent_id.as_deref(),
                        &promoted.name,
                    ),
                }},
            )
            .await
            .map_err(mongo_err)?;
        if swapped.matched_count == 0 {
            self.drop_blobs(company, replacement_id, None).await?;
            return Ok(None);
        }

        // The new document already points at durable replacement bytes. Old
        // bytes are now unreachable; a cleanup failure must not turn a
        // successful atomic swap into a reported publish failure, because the
        // boot orphan sweep can safely finish this work later.
        let superseded = expected_id.expect("the replace arm has an expected id");
        if let Err(err) = self.drop_blobs(company, superseded, None).await {
            tracing::warn!(
                company = %company,
                node_id = %superseded,
                error = %err,
                "workspace file swap succeeded but old GridFS payload cleanup was deferred"
            );
        }
        Ok(Some(promoted))
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let nodes = self.workspace_nodes(company).await?;
        if !nodes.contains_key(id) {
            return Ok(false);
        }
        let mut to_remove = mongo_workspace_descendants(&nodes, id);
        to_remove.insert(id.to_string());
        let ids: Vec<&String> = to_remove.iter().collect();
        // Documents first, blobs second — the mirror of the write ordering. A
        // crash between them leaves a payload with no node, which the boot
        // sweep reclaims; the reverse would leave a node whose download 404s.
        self.collection("workspace_nodes")
            .delete_many(doc! {"company_id": company.as_ref(), "node_id": {"$in": &ids}})
            .await
            .map_err(mongo_err)?;
        for node_id in &to_remove {
            self.drop_blobs(company, node_id, None).await?;
        }
        Ok(true)
    }

    /// Re-checks emptiness immediately before deleting, rather than trusting
    /// a caller's earlier [`tree`](Self::tree) read. This store keeps no
    /// cross-request lock — nothing here does — so it cannot close the window
    /// the way [`FsOps`](crate::store::FsOps) does; it only narrows it to the
    /// single round trip between this method's own fresh read and its
    /// `delete_one`, instead of the whole of a caller's request.
    async fn delete_if_empty(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let nodes = self.workspace_nodes(company).await?;
        let Some(node) = nodes.get(id) else {
            return Ok(false);
        };
        // An adopted folder has a second writer (issue #1839). This backend has
        // no per-company lock, so the flag only narrows Race 1 to #1801's
        // documented residual window rather than closing it: an adoption that
        // has not yet committed its `$set` is invisible here, and the loser's
        // bounded retry re-mints if it still loses. What the flag does close is
        // the common case where the adoption *has* landed.
        if node.adopted {
            return Ok(false);
        }
        if nodes
            .values()
            .any(|node| node.parent_id.as_deref() == Some(id))
        {
            return Ok(false);
        }
        // Single document, non-recursive — the check above just proved this
        // id has no children, so there is nothing else to remove with it.
        let deleted = self
            .collection("workspace_nodes")
            .delete_one(doc! {"company_id": company.as_ref(), "node_id": id})
            .await
            .map_err(mongo_err)?;
        if deleted.deleted_count == 0 {
            return Ok(false);
        }
        self.drop_blobs(company, id, None).await?;
        Ok(true)
    }

    async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
        let count = self
            .collection("workspace_nodes")
            .count_documents(doc! {"company_id": company.as_ref()})
            .await
            .map_err(mongo_err)?;
        Ok(count == 0)
    }
}

/// Collects the ids of every descendant of `id` (excluding `id`).
fn mongo_workspace_descendants(
    nodes: &HashMap<String, crate::ports::workspace::WorkspaceNode>,
    id: &str,
) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let mut frontier = vec![id.to_string()];
    while let Some(current) = frontier.pop() {
        for (child_id, node) in nodes {
            if node.parent_id.as_deref() == Some(current.as_str()) && out.insert(child_id.clone()) {
                frontier.push(child_id.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — env-gated conformance against a real MongoDB
// ---------------------------------------------------------------------------

/// The conformance suite needs a live server; there is no in-process MongoDB.
/// Set `OPENCOMPANY_TEST_MONGODB_URI` (e.g. `mongodb://localhost:27017`) to
/// run these; without it every test is a skip, keeping `cargo test` offline.
///
/// CI additionally sets `OPENCOMPANY_TEST_MONGODB_REQUIRED=1`, which turns that
/// skip into a failure — see `required()` below.
#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::store::conformance;

    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Whether a missing server must FAIL rather than skip. Issue #555.
    ///
    /// The `OPENCOMPANY_TEST_MONGODB_URI` skip above is right for a laptop with
    /// no MongoDB — it keeps a default `cargo test` offline — and wrong for the
    /// CI lane whose entire purpose is running this suite. There, an unset URI
    /// is a misconfigured job, and the skip would report it as a pass: the
    /// whole suite silently absent behind a green tick, which is the exact
    /// defect this lane was added to fix, reintroduced one layer down.
    ///
    /// So CI sets this second variable and nothing else does. Set = the caller
    /// has promised a reachable server, so not finding one is an error.
    ///
    /// `0` and the empty string read as unset, so the variable can be threaded
    /// through a workflow matrix or a shell wrapper that always defines it.
    fn required() -> bool {
        std::env::var("OPENCOMPANY_TEST_MONGODB_REQUIRED")
            .is_ok_and(|value| !value.is_empty() && value != "0")
    }

    /// Issue #697. The partial filter must be keyed on the **field name passed
    /// in**, never on the literal `"present"` — the parameter's own name.
    ///
    /// Raised in review of #733 as a HIGH finding: that `doc!` stringifies
    /// identifier keys, so `doc! {present: ...}` would build
    /// `{"present": {"$exists": true}}`, a filter no document matches. The
    /// index would then be built over an empty set and reject no insert,
    /// silently removing the one-file-per-path guarantee on this backend.
    ///
    /// It does not: the `bson!` key arms end at
    /// `insert::<_, Bson>(($($key)+), $value)`, passing the key tokens as an
    /// expression rather than through `stringify!`. But that is an argument
    /// about a macro's expansion, and the cost of being wrong is a guard that
    /// looks present and enforces nothing — so this asserts the built artifact
    /// instead of the reasoning.
    ///
    /// Needs no server: it inspects the `IndexModel` this code constructs, so
    /// it runs on the `mongodb` feature alone and cannot pass vacuously the way
    /// the URI-gated tests can.
    #[test]
    fn the_partial_filter_is_keyed_on_the_field_not_the_parameter_name() {
        let model = unique_partial(doc! {"company_id": 1, "file_path_key": 1}, "file_path_key");
        let filter = model
            .options
            .as_ref()
            .and_then(|options| options.partial_filter_expression.as_ref())
            .expect("the index is partial");

        assert!(
            filter.contains_key("file_path_key"),
            "the filter must name the field it guards: {filter:?}"
        );
        assert!(
            !filter.contains_key("present"),
            "a filter keyed on the parameter's own name would match no document, so the \
             index would be built over an empty set and reject nothing: {filter:?}"
        );
        assert_eq!(
            filter.get_document("file_path_key").expect("the condition"),
            &doc! {"$exists": true},
            "and the condition is existence of that field: {filter:?}"
        );
        assert!(
            model
                .options
                .as_ref()
                .and_then(|options| options.unique)
                .unwrap_or(false),
            "a partial filter without uniqueness would guard nothing at all"
        );
    }

    /// Issue #759's index, asserted the same way and for the same reason.
    ///
    /// The folder guard is a second `unique_partial`, and a partial filter that
    /// named the wrong field would be the identical silent failure: an index
    /// built over an empty set, rejecting nothing, while every sequential test
    /// still passed. Asserting the constructed `IndexModel` catches that with no
    /// server, so it cannot pass vacuously.
    ///
    /// It also pins the field **name**: the folder key must be its own field,
    /// not `file_path_key`. Sharing one field would make a folder and a file
    /// contend for a single name — a new tree rule this change explicitly does
    /// not introduce.
    #[test]
    fn the_folder_claim_index_is_partial_unique_on_its_own_field() {
        let model = unique_partial(
            doc! {"company_id": 1, "folder_path_key": 1},
            "folder_path_key",
        );
        let filter = model
            .options
            .as_ref()
            .and_then(|options| options.partial_filter_expression.as_ref())
            .expect("the index is partial");

        assert!(
            filter.contains_key("folder_path_key"),
            "the filter must name the field it guards: {filter:?}"
        );
        assert!(
            !filter.contains_key("file_path_key"),
            "the folder guard must not key on the file field, or a folder and a note would \
             contend for one name: {filter:?}"
        );
        assert_eq!(
            filter
                .get_document("folder_path_key")
                .expect("the condition"),
            &doc! {"$exists": true},
        );
        assert!(
            model
                .options
                .as_ref()
                .and_then(|options| options.unique)
                .unwrap_or(false),
            "a partial filter without uniqueness would guard nothing at all"
        );
        // The two keys share an encoding, which is what lets one `path_key`
        // serve both — pinned so a future edit cannot make them silently differ.
        assert_eq!(
            folder_path_key(Some("p"), "task-42"),
            file_path_key(Some("p"), "task-42")
        );
    }

    /// Issue #759, the subtle half: a folder that is **moved** drops its claim.
    ///
    /// `rename_move` has to `$unset` `folder_path_key`, and a missing unset is
    /// invisible until somebody needs the vacated path again. The moved
    /// document would keep guarding the path it left, so the next publish that
    /// wanted `agents/cmo/task-42/` would be refused by an index entry
    /// describing a folder that is no longer there — the permanent outage this
    /// primitive exists to prevent, reintroduced by the fix itself.
    ///
    /// Asserted by reclaiming the old path and checking a *new* folder was
    /// minted there, rather than by reading the document: the claim is only
    /// worth what the next claimer observes.
    #[tokio::test]
    async fn a_moved_folder_releases_its_claim_on_the_path_it_left() {
        use crate::ports::workspace::WorkspaceStore;
        let Some(s) = store().await else { return };
        let company = CompanyId::new("mover");
        let origin = crate::ports::workspace::WorkspaceOrigin::Seed;

        let parent = s
            .adopt_or_create_folder(&company, None, "Agents", origin.clone())
            .await
            .expect("the root")
            .into_node()
            .id;
        let moved = s
            .adopt_or_create_folder(&company, Some(&parent), "task-42", origin.clone())
            .await
            .expect("the folder")
            .into_node()
            .id;

        // The operator renames it out of the way.
        s.rename_move(&company, &moved, Some("task-42-archived"), None)
            .await
            .expect("rename to the workspace root");

        // The vacated path must be claimable again, by a genuinely new folder.
        let reclaimed = s
            .adopt_or_create_folder(&company, Some(&parent), "task-42", origin)
            .await
            .expect("the path the moved folder left must be free");
        assert!(
            reclaimed.was_created(),
            "a stale claim would have made this adopt a folder that is not there"
        );
        assert_ne!(reclaimed.node().id, moved);

        drop_db(&s).await;
    }

    /// **Issue #392 through the port**: the host-durable append asks the server
    /// for `j:true`, and the process-durable one does not.
    ///
    /// `assert_journal_store` cannot catch this — a backend that ignored the
    /// `Durability` argument stores and orders every record identically and
    /// passes the whole suite, silently dropping the guarantee that keeps an
    /// already-fired effect from firing again after a primary crash. So the
    /// constructed handles are asserted directly, the same way
    /// `the_partial_filter_is_keyed_on_the_field_not_the_parameter_name` asserts
    /// a built `IndexModel` rather than the reasoning behind it.
    ///
    /// Needs no server: `Client::with_options` resolves lazily and
    /// `collection_with_options` builds a handle locally, so this runs on the
    /// `mongodb` feature alone and cannot pass vacuously the way the URI-gated
    /// tests can. (It is a `tokio::test` only because the driver's constructor
    /// spawns a cleanup task, not because anything here awaits the network.)
    #[tokio::test]
    async fn only_the_host_durable_journal_write_asks_for_j_true() {
        // A client handle, not a connection: `with_options` resolves lazily and
        // never touches the network, so this stays a pure shape assertion.
        let client = Client::with_options(
            mongodb::options::ClientOptions::builder()
                .hosts(vec![mongodb::options::ServerAddress::Tcp {
                    host: "localhost".into(),
                    port: Some(27017),
                }])
                .build(),
        )
        .expect("build a client handle");
        let store = MongoStore {
            db: client.database("oc_test_shape"),
            senders: Arc::new(StdMutex::new(HashMap::new())),
        };

        let host = store.journaled(JOURNAL);
        assert_eq!(
            host.write_concern().and_then(|concern| concern.journal),
            Some(true),
            "a host-durable record must be committed to the server's journal \
             before the insert is acknowledged"
        );

        let process = store.collection(JOURNAL);
        assert!(
            process
                .write_concern()
                .and_then(|concern| concern.journal)
                .is_none(),
            "the process-durable level must NOT pay a disk flush: these are the \
             journal's highest-volume records, and losing one makes the runtime \
             re-ask rather than re-fire"
        );
    }

    /// The URI with any `user:password@` replaced by `***@`, for the panic
    /// message below.
    ///
    /// The unreachable-server panic names the URI so the failure says *which*
    /// server it could not reach — a bare "connection refused" in a CI log is
    /// most of a debugging session. But a connection string carries its
    /// credentials inline, and a panic lands in the CI log, the terminal
    /// scrollback and any artifact that captures either. CI points at an
    /// unauthenticated localhost, so nothing leaks there; a developer pointing
    /// this suite at a real cluster is the case that would, and that is exactly
    /// when the message is most useful. Redacting keeps the host and port,
    /// which is the part worth printing.
    fn redact_credentials(uri: &str) -> String {
        let Some((scheme, rest)) = uri.split_once("://") else {
            return uri.to_string();
        };
        // Userinfo, when present, precedes the first `/` of the path — so only
        // an `@` before that boundary delimits it. A password may itself
        // contain `@`, so split at the LAST one within the authority.
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(authority_end);
        match authority.rfind('@') {
            Some(at) => format!("{scheme}://***{}{tail}", &authority[at..]),
            None => uri.to_string(),
        }
    }

    #[test]
    fn redaction_keeps_the_host_and_drops_the_credentials() {
        // The CI shape: nothing to redact, nothing changed.
        assert_eq!(
            redact_credentials("mongodb://localhost:27017"),
            "mongodb://localhost:27017"
        );
        // The shape that would leak.
        assert_eq!(
            redact_credentials("mongodb://user:hunter2@cluster.example:27017"),
            "mongodb://***@cluster.example:27017"
        );
        // A password containing `@` — splitting at the FIRST one would leave
        // the tail of the password in the message.
        assert_eq!(
            redact_credentials("mongodb://user:p@ss@cluster.example:27017"),
            "mongodb://***@cluster.example:27017"
        );
        // An `@` in the path or query must not be mistaken for userinfo.
        assert_eq!(
            redact_credentials("mongodb://localhost:27017/db?replicaSet=a@b"),
            "mongodb://localhost:27017/db?replicaSet=a@b"
        );
        // A credentialed URI that also carries a path keeps the path.
        assert_eq!(
            redact_credentials("mongodb+srv://u:p@host/admin?retryWrites=true"),
            "mongodb+srv://***@host/admin?retryWrites=true"
        );
        // Not a URI at all: returned untouched rather than mangled.
        assert_eq!(redact_credentials("localhost:27017"), "localhost:27017");
    }

    async fn store() -> Option<Arc<MongoStore>> {
        let uri = match std::env::var("OPENCOMPANY_TEST_MONGODB_URI") {
            Ok(uri) => uri,
            Err(_) => {
                assert!(
                    !required(),
                    "OPENCOMPANY_TEST_MONGODB_REQUIRED is set but \
                     OPENCOMPANY_TEST_MONGODB_URI is not. This lane exists to run the \
                     MongoDB conformance suite against a real server, so a skip here is \
                     a misconfigured job rather than a pass — point the URI at the \
                     service container."
                );
                eprintln!("skipping: OPENCOMPANY_TEST_MONGODB_URI is not set");
                return None;
            }
        };
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis();
        let db = format!(
            "oc_test_{}_{}_{}",
            std::process::id(),
            nonce,
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        // `connect` creates indexes, so it round-trips to the server rather
        // than resolving lazily: an unreachable host fails HERE, after the
        // driver's server-selection timeout, instead of much later inside
        // whichever assertion happened to touch the database first.
        let store = MongoStore::connect(&uri, &db).await.unwrap_or_else(|err| {
            panic!(
                "could not reach the MongoDB server at {}: {err}",
                redact_credentials(&uri)
            )
        });
        Some(Arc::new(store))
    }

    /// One failing index must not stop the other forty-three from being
    /// created, **nor stop the store from opening at all**.
    ///
    /// Two properties in one test, because they have the same setup.
    ///
    /// `ensure_indexes` runs concurrently, so a short-circuit would drop
    /// in-flight driver operations mid-await — and an operation cancelled after
    /// its request is sent but before its reply is read leaves a connection the
    /// pool cannot safely reuse. That hazard does not exist in a sequential
    /// loop; it arrived with the concurrency, so it is pinned here.
    ///
    /// And the failure is now reported by logging rather than by refusing to
    /// construct the store (#1716). Returning `Err` here took the whole process
    /// down; under the microVM runtime the workload is PID 1, so that panicked
    /// the guest kernel and the tenant was gone. Indexes are a performance
    /// property, not a precondition for the process existing.
    #[tokio::test]
    async fn a_failing_index_stops_neither_the_other_indexes_nor_the_boot() {
        let Some(store) = store().await else { return };

        // `store()` has already run `ensure_indexes` once, so the assertion has
        // to be about something this run RE-creates. Drop a known index first;
        // if the pipeline short-circuits before reaching it, it stays missing.
        store
            .collection("notifications")
            .drop_index("company_id_1_id_1")
            .await
            .expect("drop the index whose return proves the run continued");

        // Induce the failure the way MongoDB actually produces one: an existing
        // index on the same keys with conflicting options. Replacing the unique
        // `owners` index with a non-unique one makes `ensure_indexes` collide.
        store
            .collection("owners")
            .drop_index("company_id_1")
            .await
            .expect("drop owners index");
        store
            .collection("owners")
            .create_index(IndexModel::builder().keys(doc! {"company_id": 1}).build())
            .await
            .expect("seed the conflicting index");

        // The store must still open. A tenant that cannot boot because one
        // index conflicts is strictly worse than one serving without it.
        store
            .ensure_indexes()
            .await
            .expect("a conflicting index must not stop the store from opening");

        // The point: the run continued past the failure and recreated the index
        // dropped above. A short-circuit would leave it absent.
        let mut names = Vec::new();
        let mut cursor = store
            .collection("notifications")
            .list_indexes()
            .await
            .expect("list notifications indexes");
        while cursor.advance().await.expect("advance") {
            if let Some(name) = cursor
                .deserialize_current()
                .expect("model")
                .options
                .and_then(|o| o.name)
            {
                names.push(name);
            }
        }
        assert!(
            names.iter().any(|n| n == "company_id_1_id_1"),
            "a failure on `owners` must not stop other indexes being created; got {names:?}"
        );

        drop_db(&store).await;
    }

    /// Issue #1573: the backfill copies `agentId` out of `run_json` for rows
    /// written before the mirror column existed, and does so in bounded batches
    /// that re-probe between passes rather than holding the whole collection.
    ///
    /// Seeded directly into the `runs` collection with no `agent_id` field —
    /// the exact shape a row predating the upgrade has — because it is not
    /// reachable through the port: `create_run`/`put_run` always write the
    /// mirror. The store is built as a bare struct, not through `connect`, so
    /// no background backfill task shares the database with this one's
    /// assertions.
    #[tokio::test]
    async fn backfill_fills_legacy_run_rows_in_bounded_batches() {
        let uri = match std::env::var("OPENCOMPANY_TEST_MONGODB_URI") {
            Ok(uri) => uri,
            Err(_) => {
                assert!(
                    !required(),
                    "OPENCOMPANY_TEST_MONGODB_REQUIRED is set but \
                     OPENCOMPANY_TEST_MONGODB_URI is not"
                );
                eprintln!("skipping: OPENCOMPANY_TEST_MONGODB_URI is not set");
                return;
            }
        };
        let client = Client::with_uri_str(&uri).await.unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis();
        let db_name = format!(
            "oc_test_{}_{}_{}",
            std::process::id(),
            nonce,
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let store = MongoStore {
            db: client.database(&db_name),
            senders: Arc::new(StdMutex::new(HashMap::new())),
        };

        let company = CompanyId::new("legacy-co");
        let runs = store.collection("runs");
        // More than one batch, so the re-probe loop is exercised — the rows past
        // the first `BACKFILL_BATCH_SIZE` must be picked up by a later pass.
        for i in 0..BACKFILL_BATCH_SIZE + 3 {
            let agent = if i % 2 == 0 { "engineer" } else { "designer" };
            let record = crate::ports::runs::RunRecord {
                id: format!("legacy-{i}"),
                company: company.clone(),
                task_id: None,
                agent_id: agent.to_string(),
                chat_id: None,
                thread_root: None,
                workflow_run_id: None,
                node_id: None,
                attempt: 1,
                status: crate::ports::runs::RunStatus::Succeeded,
                trigger_event_seq: None,
                created_at_millis: 1_700_000_000_000,
                started_at_millis: None,
                finished_at_millis: None,
                error: None,
                usage: Default::default(),
                step_count: 1,
            };
            runs.insert_one(doc! {
                "company_id": company.as_ref(),
                "run_id": &record.id,
                "run_json": serde_json::to_string(&record).unwrap(),
            })
            .await
            .unwrap();
        }
        // A row that already carries the mirror (written through the port after
        // the upgrade) must be neither touched nor counted.
        let fresh = crate::ports::runs::RunRecord {
            id: "fresh".to_string(),
            company: company.clone(),
            task_id: None,
            agent_id: "engineer".to_string(),
            chat_id: None,
            thread_root: None,
            workflow_run_id: None,
            node_id: None,
            attempt: 1,
            status: crate::ports::runs::RunStatus::Pending,
            trigger_event_seq: None,
            created_at_millis: 1_700_000_000_000,
            started_at_millis: None,
            finished_at_millis: None,
            error: None,
            usage: Default::default(),
            step_count: 0,
        };
        runs.insert_one(doc! {
            "company_id": company.as_ref(),
            "run_id": &fresh.id,
            "agent_id": "engineer",
            "status": "pending",
            "attempt": 1i64,
            "created_ms": 1_700_000_000_000i64,
            "run_json": serde_json::to_string(&fresh).unwrap(),
        })
        .await
        .unwrap();

        let filled = store.backfill_run_agent_ids().await.unwrap();
        assert_eq!(
            filled,
            BACKFILL_BATCH_SIZE + 3,
            "every legacy row is filled; the fresh row is not counted"
        );

        // The mirror landed on disk, not just in the return value — one row from
        // each batch's worth of desks.
        let migrated = runs
            .find_one(doc! {"run_id": "legacy-0"})
            .await
            .unwrap()
            .unwrap();
        assert_eq!(get_str(&migrated, "agent_id").unwrap(), "engineer");
        let later = runs
            .find_one(doc! {"run_id": "legacy-1"})
            .await
            .unwrap()
            .unwrap();
        assert_eq!(get_str(&later, "agent_id").unwrap(), "designer");

        // A second pass has nothing left to do — the `$exists: false` probe is
        // exhausted.
        assert_eq!(store.backfill_run_agent_ids().await.unwrap(), 0);

        drop_db(&store).await;
    }

    async fn drop_db(store: &MongoStore) {
        let _ = store.db.drop().await;
    }

    /// Ages every staged blob past [`ORPHAN_BLOB_MIN_AGE_MS`] (issue #664).
    ///
    /// The sweep only reclaims blobs old enough to be abandoned, so a test that
    /// uploads bytes and reboots in the same millisecond is staging an
    /// *in-flight* upload, not an orphaned one. Rewriting `uploadDate` is how a
    /// test says "and then an hour passed" without sleeping for one.
    async fn age_blobs_past_the_sweep_threshold(store: &MongoStore) {
        let old = mongodb::bson::DateTime::from_millis(
            now_millis() as i64 - ORPHAN_BLOB_MIN_AGE_MS - 60_000,
        );
        store
            .db
            .collection::<Document>(&format!("{BLOB_BUCKET}.files"))
            .update_many(doc! {}, doc! { "$set": { "uploadDate": old } })
            .await
            .expect("backdate staged blobs");
    }

    /// The boot sweep reclaims a payload whose node document never landed.
    ///
    /// This is the crash the write ordering deliberately allows: blob first,
    /// document second, so an interrupted `create_binary` leaves bytes nothing
    /// references. Seeded here directly — uploading to the bucket without ever
    /// inserting the node — because that is precisely the state a crash between
    /// the two writes produces, and it is not reachable through the port.
    ///
    /// The node-backed blob beside it is the half that must be left alone: a
    /// sweep that reclaimed live payloads would be far worse than the leak it
    /// fixes.
    #[tokio::test]
    async fn the_boot_sweep_reclaims_orphaned_blobs_and_spares_live_ones() {
        let Some(s) = store().await else { return };
        let company = CompanyId::new("sweep-co");

        // A live binary node, written through the port.
        let node = crate::ports::workspace::WorkspaceNode {
            id: "keep".to_string(),
            name: "keep.png".to_string(),
            kind: crate::ports::workspace::NodeKind::File,
            parent_id: None,
            updated_at_millis: now_millis(),
            created_by: crate::ports::workspace::WorkspaceOrigin::Operator,
            updated_by: crate::ports::workspace::WorkspaceOrigin::Operator,
            mime: Some("image/png".to_string()),
            size: None,
            sha256: None,
            adopted: false,
        };
        crate::ports::workspace::WorkspaceStore::create_binary(&*s, &company, &node, b"live-bytes")
            .await
            .unwrap();

        // …and a dangling one: the bytes of an interrupted create.
        s.put_blob(&company, "vanished", "ghost.png", b"orphan-bytes")
            .await
            .unwrap();
        // A blob with no metadata at all — a shape this store never writes, and
        // therefore unmatchable to any node, so it is an orphan by definition.
        {
            use futures::io::AsyncWriteExt;
            let mut up = s
                .blobs()
                .open_upload_stream("nometa.bin")
                .await
                .expect("upload");
            up.write_all(b"no-metadata").await.unwrap();
            up.close().await.unwrap();
        }

        let before = s.blobs().find(doc! {}).await.unwrap();
        assert_eq!(
            before.try_collect::<Vec<_>>().await.unwrap().len(),
            3,
            "two orphans and one live payload are staged"
        );

        // The orphans have to be *old* to be orphans (issue #664). Staged
        // seconds ago they are indistinguishable from a peer's in-flight
        // upload, and the sweep now spares them for that reason — see
        // `a_recent_orphan_is_left_alone_because_it_may_be_an_in_flight_upload`.
        age_blobs_past_the_sweep_threshold(&s).await;

        // Constructing a store over the same database runs the sweep.
        let rebooted = MongoStore::from_database(s.db.clone()).await.unwrap();

        let files = rebooted
            .blobs()
            .find(doc! {})
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(files.len(), 1, "both orphans are reclaimed");
        assert_eq!(files[0].filename.as_deref(), Some("keep.png"));

        // The live node still serves its bytes — the sweep did not touch it.
        let (_, stream) =
            crate::ports::workspace::WorkspaceStore::read_bytes(&rebooted, &company, "keep")
                .await
                .unwrap()
                .expect("the live payload survives the sweep");
        let mut got = Vec::new();
        {
            use futures::StreamExt;
            let mut stream = stream;
            while let Some(chunk) = stream.next().await {
                got.extend_from_slice(&chunk.unwrap());
            }
        }
        assert_eq!(got, b"live-bytes".to_vec());

        drop_db(&s).await;
    }

    /// Issue #664: the boot sweep must not delete a concurrent writer's upload.
    ///
    /// The state staged here is not a crash — it is the perfectly ordinary
    /// instant *inside* a healthy `create_binary`, after `put_blob` has
    /// returned and before the node insert lands. A second process booting then
    /// (a rolling deploy, a restarted replica, another tenant's container on a
    /// shared database) sees a blob with no node and, before this fix, reclaimed
    /// it. The writer's insert would then land on top, leaving a node whose
    /// download 404s forever and whose `size` still counts against the quota —
    /// unreclaimable, because the sweep only deletes blobs without nodes and
    /// never nodes without blobs.
    ///
    /// Deliberately *not* backdated: recency is the whole signal.
    #[tokio::test]
    async fn a_recent_orphan_is_left_alone_because_it_may_be_an_in_flight_upload() {
        let Some(s) = store().await else { return };
        let company = CompanyId::new("inflight-co");

        // Exactly what a concurrent `create_binary` has written so far.
        s.put_blob(&company, "arriving", "arriving.png", b"in-flight-bytes")
            .await
            .unwrap();

        // A second process boots against the same database and sweeps.
        let rebooted = MongoStore::from_database(s.db.clone()).await.unwrap();

        let files = rebooted
            .blobs()
            .find(doc! {})
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            files.len(),
            1,
            "a blob younger than the threshold is an upload in progress, not an orphan"
        );
        assert_eq!(files[0].filename.as_deref(), Some("arriving.png"));

        // And the writer's insert, landing after the sweep, yields a node whose
        // bytes are actually there — the whole point.
        let node = crate::ports::workspace::WorkspaceNode {
            id: "arriving".to_string(),
            name: "arriving.png".to_string(),
            kind: crate::ports::workspace::NodeKind::File,
            parent_id: None,
            updated_at_millis: now_millis(),
            created_by: crate::ports::workspace::WorkspaceOrigin::Operator,
            updated_by: crate::ports::workspace::WorkspaceOrigin::Operator,
            mime: Some("image/png".to_string()),
            size: None,
            sha256: None,
            adopted: false,
        };
        rebooted
            .collection("workspace_nodes")
            .insert_one(doc! {
                "company_id": company.as_ref(),
                "node_id": &node.id,
                "node_json": serde_json::to_string(&node).unwrap(),
                "content": "",
                "updated_ms": node.updated_at_millis as i64,
            })
            .await
            .unwrap();

        let (_, stream) =
            crate::ports::workspace::WorkspaceStore::read_bytes(&rebooted, &company, "arriving")
                .await
                .unwrap()
                .expect("the upload that was in flight during the sweep still has its bytes");
        let mut got = Vec::new();
        {
            use futures::StreamExt;
            let mut stream = stream;
            while let Some(chunk) = stream.next().await {
                got.extend_from_slice(&chunk.unwrap());
            }
        }
        assert_eq!(got, b"in-flight-bytes".to_vec());

        drop_db(&s).await;
    }

    /// A `create_binary` name conflict must not strand the payload it just
    /// uploaded.
    ///
    /// This backend writes blob-first (issue #894), so a sibling-name collision
    /// is detected only when the node-document insert fails — by which time the
    /// bytes are already in GridFS. Before the conflict path reclaimed them, the
    /// error returned with that blob still present: no node document referenced
    /// it, and the boot sweep (which runs only at store construction, and only
    /// for blobs older than an hour) was the sole reclaim path. The
    /// chat-attachment flow reaches this branch every time a repeated filename
    /// is disambiguated and retried, so a long-lived tenant would accumulate
    /// invisible GridFS copies. The conflict path must own the payload it
    /// uploaded before the caller learns of the conflict.
    #[tokio::test]
    async fn a_name_conflict_reclaims_the_blob_it_just_uploaded() {
        let Some(s) = store().await else { return };
        let company = CompanyId::new("conflict-co");

        let first = crate::ports::workspace::WorkspaceNode {
            id: "winner".to_string(),
            name: "image.png".to_string(),
            kind: crate::ports::workspace::NodeKind::File,
            parent_id: None,
            updated_at_millis: now_millis(),
            created_by: crate::ports::workspace::WorkspaceOrigin::Operator,
            updated_by: crate::ports::workspace::WorkspaceOrigin::Operator,
            mime: Some("image/png".to_string()),
            size: None,
            sha256: None,
            adopted: false,
        };
        crate::ports::workspace::WorkspaceStore::create_binary(&*s, &company, &first, b"first")
            .await
            .unwrap();

        let loser = crate::ports::workspace::WorkspaceNode {
            id: "loser".to_string(),
            ..first.clone()
        };
        let err = crate::ports::workspace::WorkspaceStore::create_binary(
            &*s, &company, &loser, b"second",
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, crate::error::OpenCompanyError::Conflict(_)),
            "a taken sibling name is a Conflict, not a storage fault: {err:?}"
        );

        // The loser's payload must not survive the conflict as an orphan.
        let files = s
            .blobs()
            .find(MongoStore::blob_filter(&company, "loser"))
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            files.len(),
            0,
            "the conflicted upload leaves no orphan blob for the sweep to find later"
        );

        // The winner still serves its bytes.
        let (_, stream) =
            crate::ports::workspace::WorkspaceStore::read_bytes(&*s, &company, "winner")
                .await
                .unwrap()
                .expect("the winner's payload is untouched by the refusal");
        let mut got = Vec::new();
        {
            use futures::StreamExt;
            let mut stream = stream;
            while let Some(chunk) = stream.next().await {
                got.extend_from_slice(&chunk.unwrap());
            }
        }
        assert_eq!(got, b"first".to_vec());

        drop_db(&s).await;
    }

    /// A same-id `create_binary` race must not reclaim the winning payload.
    ///
    /// Two callers racing the same fresh node id both pass the `contains_key`
    /// pre-check (neither document is visible when the reads land), both upload
    /// a GridFS payload under that id, and the node-document insert then loses
    /// on the unique `(company_id, node_id)` index for exactly one of them.
    /// The loser's conflict cleanup used to sweep *every* blob for the id —
    /// including the winner's, the bytes its live node now points at — leaving
    /// the download irrecoverable on hosted MongoDB deployments. The cleanup
    /// must name and delete only the upload this losing call made.
    ///
    /// Spawned rather than staged: the whole point is the interleaving, and the
    /// runtime guarantees it here — each racer awaits a database read before
    /// either inserts, so both `contains_key` checks necessarily see the empty
    /// tree regardless of which document insert finally wins.
    #[tokio::test]
    async fn a_same_id_race_preserves_the_winning_payload() {
        let Some(s) = store().await else { return };
        let company = CompanyId::new("dupe-race-co");

        let node = crate::ports::workspace::WorkspaceNode {
            id: "dupe".to_string(),
            name: "dupe.png".to_string(),
            kind: crate::ports::workspace::NodeKind::File,
            parent_id: None,
            updated_at_millis: now_millis(),
            created_by: crate::ports::workspace::WorkspaceOrigin::Operator,
            updated_by: crate::ports::workspace::WorkspaceOrigin::Operator,
            mime: Some("image/png".to_string()),
            size: None,
            sha256: None,
            adopted: false,
        };

        let racer_a = {
            let s = s.clone();
            let company = company.clone();
            let node = node.clone();
            tokio::spawn(async move {
                crate::ports::workspace::WorkspaceStore::create_binary(
                    &*s,
                    &company,
                    &node,
                    b"payload-a",
                )
                .await
            })
        };
        let racer_b = {
            let s = s.clone();
            let company = company.clone();
            let node = node.clone();
            tokio::spawn(async move {
                crate::ports::workspace::WorkspaceStore::create_binary(
                    &*s,
                    &company,
                    &node,
                    b"payload-b",
                )
                .await
            })
        };

        let (outcome_a, outcome_b) = (racer_a.await.unwrap(), racer_b.await.unwrap());
        assert_eq!(
            outcome_a.is_ok() as u8 + outcome_b.is_ok() as u8,
            1,
            "exactly one same-id caller wins the insert; the other must be a Conflict"
        );

        // The survivor's node still serves its bytes: the loser's cleanup
        // deleted only its own upload, never the winner's.
        let (_, stream) =
            crate::ports::workspace::WorkspaceStore::read_bytes(&*s, &company, "dupe")
                .await
                .unwrap()
                .expect("the winning payload survives the same-id race");
        let mut got = Vec::new();
        {
            use futures::StreamExt;
            let mut stream = stream;
            while let Some(chunk) = stream.next().await {
                got.extend_from_slice(&chunk.unwrap());
            }
        }
        assert!(
            got == b"payload-a" || got == b"payload-b",
            "the surviving payload is exactly one racer's, not a mix: {got:?}"
        );

        // And exactly one blob remains for the id — the losing upload is gone,
        // the winner's is untouched.
        let files = s
            .blobs()
            .find(MongoStore::blob_filter(&company, "dupe"))
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            files.len(),
            1,
            "the losing upload is reclaimed without touching the winner's"
        );

        drop_db(&s).await;
    }

    /// Issue #1077: the orphan report composes `list()` and `owners()`
    /// correctly against a real server.
    ///
    /// The pure set difference is unit-tested in `app::orphans`. What only a
    /// live backend can prove is that the two reads are *comparable* — that
    /// `owners()` keys on the same id string `list()` returns. They do
    /// (`company_id` in both collections), but nothing in the type system says
    /// so: both sides are `CompanyId`, and if one had been namespaced and the
    /// other bare, the report would have called every company on the platform
    /// an orphan while still type-checking and still passing every unit test.
    ///
    /// Namespaced ids specifically, because that is the only mode in which the
    /// `owners` collection is load-bearing at all.
    #[tokio::test]
    async fn orphaned_companies_are_found_through_the_real_ports() {
        let Some(s) = store().await else { return };

        let manifest: CompanyManifest = toml::from_str("[company]\nname = \"Acme\"\n").unwrap();
        let owned = crate::app::namespace_company_id("tenant-a", CompanyId::new("owned"));
        let orphan = crate::app::namespace_company_id("tenant-a", CompanyId::new("orphan"));

        for id in [&owned, &orphan] {
            let record = CompanyRecord {
                overlay_retired_agents: Vec::new(),
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".into(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_agent_edits: Vec::new(),
                overlay_policy: None,
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            };
            s.save(&record).await.expect("save company");
        }
        // Only one of the two gets an owner row. The other is exactly the state
        // a failed `set_owner` used to leave behind before #1050 was fixed.
        s.set_owner(&owned, "tenant-a").await.expect("record owner");
        // ...plus a row naming a company that was never saved, which is the
        // benign direction #1073 deliberately prefers on a rolled-back provision.
        let ghost = crate::app::namespace_company_id("tenant-a", CompanyId::new("ghost"));
        s.set_owner(&ghost, "tenant-a").await.expect("record ghost");

        let companies = CompanyStore::list(s.as_ref()).await.expect("list");
        let owners = s.owners().await.expect("owners");
        let report = crate::app::find_orphans(&companies, &owners);

        let unowned: Vec<&str> = report.unowned.iter().map(|c| c.id.as_ref()).collect();
        assert!(
            unowned.contains(&orphan.as_ref()),
            "the company with no owner row must be reported: {report:?}"
        );
        assert!(
            !unowned.contains(&owned.as_ref()),
            "the company WITH an owner row must not be: {report:?}"
        );
        let dangling: Vec<&str> = report.dangling.iter().map(|r| r.id.as_ref()).collect();
        assert!(
            dangling.contains(&ghost.as_ref()),
            "the owner row naming no company must be reported: {report:?}"
        );
        assert!(
            !dangling.contains(&owned.as_ref()),
            "a row whose company exists must not be: {report:?}"
        );

        drop_db(&s).await;
    }

    /// Shared-single-DB namespacing: two tenants registering the same template
    /// name land distinct namespaced ids in one database, so the `companies`
    /// unique index never conflicts, and the `owners` rows carry the right
    /// tenant for each. Mirrors what the workload does when
    /// `OPENCOMPANY_TENANT_ID` is set (see `AppConfig::namespaced_company_id`).
    #[tokio::test]
    async fn shared_db_namespaced_companies_do_not_conflict() {
        let Some(s) = store().await else { return };

        let manifest: CompanyManifest = toml::from_str("[company]\nname = \"Acme\"\n").unwrap();
        let id_a = crate::app::namespace_company_id(
            "tenant-a",
            crate::runtime::company_id_from_name(&manifest.company.name),
        );
        let id_b = crate::app::namespace_company_id(
            "tenant-b",
            crate::runtime::company_id_from_name(&manifest.company.name),
        );
        assert_eq!(id_a.as_ref(), "tenant-a--acme");
        assert_eq!(id_b.as_ref(), "tenant-b--acme");

        for (id, tenant) in [(&id_a, "tenant-a"), (&id_b, "tenant-b")] {
            let record = CompanyRecord {
                overlay_retired_agents: Vec::new(),
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".into(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_agent_edits: Vec::new(),
                overlay_policy: None,
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            };
            // Same template name under two tenants: distinct namespaced ids, no
            // `companies` unique-index conflict.
            s.save(&record).await.expect("save namespaced company");
            s.set_owner(id, tenant).await.expect("record owner");
        }

        let mut owners = s.owners().await.unwrap();
        owners.sort_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));
        assert_eq!(
            owners,
            vec![
                (id_a.clone(), "tenant-a".to_string()),
                (id_b.clone(), "tenant-b".to_string()),
            ]
        );

        // Both companies remain addressable and carry the shared template name.
        assert_eq!(
            s.load(&id_a).await.unwrap().unwrap().manifest.company.name,
            "Acme"
        );
        assert_eq!(
            s.load(&id_b).await.unwrap().unwrap().manifest.company.name,
            "Acme"
        );

        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_isolation_by_company() {
        let Some(s) = store().await else { return };
        conformance::assert_isolation_by_company(s.clone(), s.clone(), s.clone(), s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_paused_ordinary_save_preserves_activation_gate() {
        let Some(s) = store().await else { return };
        conformance::assert_paused_ordinary_save_preserves_activation_gate(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_user_store() {
        let Some(s) = store().await else { return };
        conformance::assert_user_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_session_store() {
        let Some(s) = store().await else { return };
        conformance::assert_session_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_login_code_store() {
        let Some(s) = store().await else { return };
        conformance::assert_login_code_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_append_only_event_and_ledger() {
        let Some(s) = store().await else { return };
        conformance::assert_append_only_event_and_ledger(s.clone(), s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_monotonic_event_seq() {
        let Some(s) = store().await else { return };
        conformance::assert_monotonic_event_seq(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_event_subscription_surfaces_gap() {
        let Some(s) = store().await else { return };
        conformance::assert_event_subscription_surfaces_gap(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_event_read_before() {
        let Some(s) = store().await else { return };
        conformance::assert_event_read_before(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_event_retention() {
        let Some(s) = store().await else { return };
        conformance::assert_event_retention(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_export_totality() {
        let Some(s) = store().await else { return };
        conformance::assert_export_totality(s.clone(), s.clone(), s.clone(), s.clone()).await;
        drop_db(&s).await;
    }

    /// KeepRecent eviction deletes strictly-older-than-the-n-th-newest traces,
    /// so a trace saved after the eviction snapshot can never be evicted by the
    /// sweep that means to keep it. The old `$nin` predicate deleted any doc
    /// whose seq was not in the keep set, so a `save_trace` landing between
    /// evict's find and delete was dropped the same pass it was written — the
    /// race this delete-by-cutoff form removes.
    #[tokio::test]
    async fn evict_keep_recent_spares_a_trace_saved_after_its_snapshot() {
        let Some(s) = store().await else { return };
        let id = CompanyId::new("acme");
        // 33 traces: `next_seq` hands out 0..=32 in save order.
        for i in 0..=32 {
            s.save_trace(&id, CompressedTrace::now(format!("c{i}"), format!("s{i}")))
                .await
                .unwrap();
        }
        // Keeps the newest 32 (seqs 1..=32), evicting exactly seq 0.
        let removed = s
            .evict(&id, EvictionPolicy::KeepRecent { n: 32 })
            .await
            .unwrap();
        assert_eq!(removed, 1);
        let kept = s.recent_traces(&id, usize::MAX).await.unwrap();
        assert_eq!(kept.len(), 32);
        assert_eq!(kept.first().unwrap().cycle_id, "c1");
        assert_eq!(kept.last().unwrap().cycle_id, "c32");

        // A trace written after the sweep's snapshot has seq 33, above the
        // cutoff (1): it must survive the retention policy. This is the write
        // the `$nin` predicate deleted when it landed between evict's find and
        // delete.
        s.save_trace(&id, CompressedTrace::now("c33", "s33"))
            .await
            .unwrap();
        let after = s.recent_traces(&id, usize::MAX).await.unwrap();
        assert_eq!(after.len(), 33);
        assert_eq!(after.last().unwrap().cycle_id, "c33");

        // KeepRecent{0} keeps nothing: every trace goes.
        let removed = s
            .evict(&id, EvictionPolicy::KeepRecent { n: 0 })
            .await
            .unwrap();
        assert_eq!(removed, 33);
        assert!(s.recent_traces(&id, usize::MAX).await.unwrap().is_empty());

        // KeepRecent above the current count is a no-op.
        s.save_trace(&id, CompressedTrace::now("c34", "s34"))
            .await
            .unwrap();
        let removed = s
            .evict(&id, EvictionPolicy::KeepRecent { n: 10 })
            .await
            .unwrap();
        assert_eq!(removed, 0);

        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_inbox_store() {
        let Some(s) = store().await else { return };
        conformance::assert_inbox_store(s.clone()).await;
        drop_db(&s).await;
    }

    /// Issue #1505. The port holds this tenant's inference credential, its MCP
    /// OAuth tokens and its SMTP password, and had no conformance case on any
    /// backend until this one — on the backend a hosted tenant actually runs,
    /// where the company scope in the query IS the tenant boundary.
    #[tokio::test]
    async fn conformance_secret_store() {
        let Some(s) = store().await else { return };
        conformance::assert_secret_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_task_store() {
        let Some(s) = store().await else { return };
        conformance::assert_task_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_fact_store() {
        let Some(s) = store().await else { return };
        conformance::assert_fact_store(s.clone()).await;
        conformance::assert_artifact_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_context_chunk_stamps() {
        let Some(s) = store().await else { return };
        conformance::assert_context_chunk_stamps(s.clone()).await;
        drop_db(&s).await;
    }

    // Exercises this backend's single-`$in`-find `peek_many` override against
    // the same positional contract the default implementation gives.
    #[tokio::test]
    async fn conformance_context_peek_many() {
        let Some(s) = store().await else { return };
        conformance::assert_context_peek_many_answers_positionally(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_context_multibyte_bodies() {
        let Some(s) = store().await else { return };
        conformance::assert_multibyte_bodies_survive_search_and_ranged_peek(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_context_identical_body_two_labels() {
        let Some(s) = store().await else { return };
        conformance::assert_identical_body_two_labels(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_context_delete_label_scoped() {
        let Some(s) = store().await else { return };
        conformance::assert_delete_label_scoped(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_context_delete_label_survives_a_concurrent_identical_put() {
        let Some(s) = store().await else { return };
        conformance::assert_delete_label_survives_a_concurrent_identical_put(s.clone()).await;
        drop_db(&s).await;
    }

    /// The legacy scalar-`label` document shape (written before the `labels`
    /// set existed) keeps working through every read and through the
    /// label-scoped delete — `doc_labels` unions the two shapes, and the
    /// `$unset` leg of `delete_label` is what removes a scalar claim.
    #[tokio::test]
    async fn legacy_scalar_label_documents_list_and_label_delete() {
        let Some(s) = store().await else { return };
        let id = CompanyId::new("acme");
        let body = "written before the labels set";
        // Seed the pre-#1300 shape directly: scalar label, no labels array —
        // at the body's real content address, so a later put folds into it.
        s.collection("context_chunks")
            .insert_one(doc! {
                "company_id": id.as_ref(),
                "addr": content_address(body),
                "label": "agent/ceo",
                "body": body,
                "len": body.len() as i64,
                "ord": 1_i64,
                "stored_ms": 7_i64,
            })
            .await
            .expect("seed a legacy document");

        let metas = ContextStore::list(s.as_ref(), &id, "").await.expect("list");
        assert_eq!(
            metas.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            ["agent/ceo"],
            "the scalar label is a claim"
        );

        // A second label on the same body folds into the set beside it.
        let addr = s
            .put(
                &id,
                ContextChunk {
                    label: "agent/ops".to_string(),
                    body: body.to_string(),
                },
            )
            .await
            .expect("put an identical body under a new label");
        let mut labels: Vec<String> = ContextStore::list(s.as_ref(), &id, "")
            .await
            .unwrap()
            .into_iter()
            .filter(|m| m.addr == addr)
            .map(|m| m.label)
            .collect();
        labels.sort();
        assert_eq!(labels, ["agent/ceo", "agent/ops"]);

        // Deleting the scalar claim leaves the set claim and the body.
        assert!(
            s.delete_label(&id, &addr, "agent/ceo")
                .await
                .expect("delete the scalar claim")
        );
        let after: Vec<String> = ContextStore::list(s.as_ref(), &id, "")
            .await
            .unwrap()
            .into_iter()
            .filter(|m| m.addr == addr)
            .map(|m| m.label)
            .collect();
        assert_eq!(after, ["agent/ops"]);
        s.peek(&id, &addr, None).await.expect("the body survives");

        // The rollback contract, read from the raw document: while any claim
        // remains the scalar `label` is present AND names a live claim. A
        // pre-#1300 binary reads that field with a hard error on absence, so
        // a document left without one would fail its whole `list`.
        let raw = s
            .collection("context_chunks")
            .find_one(doc! {"company_id": id.as_ref(), "addr": addr.as_ref()})
            .await
            .unwrap()
            .expect("the document survives");
        assert_eq!(
            raw.get_str("label").ok(),
            Some("agent/ops"),
            "the scalar must re-point at a surviving claim: {raw:?}"
        );

        // And the last claim takes the document with it.
        assert!(s.delete_label(&id, &addr, "agent/ops").await.unwrap());
        assert!(s.peek(&id, &addr, None).await.is_err());
        drop_db(&s).await;
    }

    /// A document carrying claims but no scalar `label` heals on the next
    /// `put`, rather than gaining a claim and staying unreadable to a
    /// pre-#1300 `list` (which reads that field with a hard error on
    /// absence). `$setOnInsert` could not do this — it is skipped entirely on
    /// a document that already exists — which is why `put` is a pipeline.
    ///
    /// Seeded directly rather than raced for: `delete_label` no longer leaves
    /// this state (it either deletes the document atomically or re-points the
    /// scalar in the same update), so the only honest way to test the healing
    /// is to construct the state a rollback or an older build could leave.
    #[tokio::test]
    async fn a_put_restores_a_missing_scalar_label_and_keeps_first_write_wins() {
        let Some(s) = store().await else { return };
        let id = CompanyId::new("acme");
        let body = "a document that lost its scalar label";
        s.collection("context_chunks")
            .insert_one(doc! {
                "company_id": id.as_ref(),
                "addr": content_address(body),
                "body": body,
                "len": body.len() as i64,
                "ord": 1_i64,
                "stored_ms": 7_i64,
                "labels": [],
            })
            .await
            .expect("seed a scalar-less document");

        let addr = s
            .put(
                &id,
                ContextChunk {
                    label: "agent/ops".to_string(),
                    body: body.to_string(),
                },
            )
            .await
            .expect("put onto the scalar-less document");

        let raw = s
            .collection("context_chunks")
            .find_one(doc! {"company_id": id.as_ref(), "addr": addr.as_ref()})
            .await
            .unwrap()
            .expect("the document is still there");
        assert_eq!(
            raw.get_str("label").ok(),
            Some("agent/ops"),
            "the write restores the scalar it found missing: {raw:?}"
        );
        assert_eq!(doc_labels(&raw), ["agent/ops"], "and claims it once");
        assert_eq!(
            raw.get_i64("stored_ms").ok(),
            Some(7),
            "first-write-wins still holds for the fields that were present"
        );
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_deep_trace_store() {
        let Some(s) = store().await else { return };
        conformance::assert_deep_trace_store(s.clone()).await;
        drop_db(&s).await;
    }

    /// The prune ranks by RUN and keeps the newest `MAX` runs whole — and a run
    /// that fell past the cap survives once it is written to again. The
    /// conformance suite never crosses the cap, so the aggregation the prune
    /// uses to rank, and the re-verification that spares a refreshed run, are
    /// exercised here against the real server.
    #[tokio::test]
    async fn deep_trace_prune_keeps_the_newest_runs_and_spares_a_refreshed_one() {
        use crate::ports::deep_trace::DeepTraceStore;
        use crate::ports::deep_trace::{
            MAX_DEEP_RUNS_PER_COMPANY, RunStepDetailRecord, TurnStepDetail,
        };
        let Some(s) = store().await else { return };
        let company = CompanyId::new("pruner");
        let detail = |run: &str, seq: u32, at: u64, reasoning: &str| RunStepDetailRecord {
            run_id: run.to_string(),
            step_seq: seq,
            at_millis: at,
            detail: TurnStepDetail {
                reasoning: Some(reasoning.to_string()),
                ..TurnStepDetail::default()
            },
        };
        // Fill past the cap with two rows per run, so the prune must drop whole
        // runs rather than tear them.
        let cap = MAX_DEEP_RUNS_PER_COMPANY;
        for i in 0..cap + 2 {
            let run = format!("r{i:03}");
            s.append_step_detail(&company, &detail(&run, 0, (i * 2) as u64, "first"))
                .await
                .unwrap();
            s.append_step_detail(&company, &detail(&run, 1, (i * 2 + 1) as u64, "second"))
                .await
                .unwrap();
        }
        // The two oldest runs fell past the cap and are gone whole…
        assert!(
            s.list_step_details(&company, "r000")
                .await
                .unwrap()
                .is_empty(),
            "r000 ranked oldest and must be pruned"
        );
        assert!(
            s.list_step_details(&company, "r001")
                .await
                .unwrap()
                .is_empty()
        );
        // …and every surviving run kept both rows.
        for i in 2..cap + 2 {
            let run = format!("r{i:03}");
            assert_eq!(
                s.list_step_details(&company, &run).await.unwrap().len(),
                2,
                "{run} must survive whole"
            );
        }
        // A run that already ranked stale is written to again — the concurrent
        // refresh the delete's re-verification exists to protect. The next
        // append's prune must keep it, not delete what it just received.
        s.append_step_detail(
            &company,
            &detail("r000", 2, 1_000_000, "refreshed after ranking stale"),
        )
        .await
        .unwrap();
        let refreshed = s.list_step_details(&company, "r000").await.unwrap();
        assert_eq!(
            refreshed.len(),
            1,
            "a refreshed run is not deleted by the prune that follows"
        );
        assert_eq!(
            refreshed[0].detail.reasoning.as_deref(),
            Some("refreshed after ranking stale"),
            "the refreshed detail is the one that survives"
        );
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_run_store_workflow_join() {
        let Some(s) = store().await else { return };
        conformance::assert_run_store_workflow_join(s.clone()).await;
        drop_db(&s).await;
    }

    /// The same search semantics as every other backend. This is the production
    /// backend, so this is the row that matters most.
    #[tokio::test]
    async fn conformance_context_search_ranking() {
        let Some(s) = store().await else { return };
        conformance::assert_context_search_ranking(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_journal_store() {
        let Some(s) = store().await else { return };
        conformance::assert_journal_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_journal_import() {
        let Some(s) = store().await else { return };
        conformance::assert_journal_import(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_run_store() {
        let Some(s) = store().await else { return };
        conformance::assert_run_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_workflow_revision_store() {
        let Some(s) = store().await else { return };
        conformance::assert_workflow_revision_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_run_reaper() {
        let Some(s) = store().await else { return };
        conformance::assert_run_reaper(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_schedule_fire_store() {
        let Some(s) = store().await else { return };
        conformance::assert_schedule_fire_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_usage_meter() {
        let Some(s) = store().await else { return };
        conformance::assert_usage_meter(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_usage_retention() {
        let Some(s) = store().await else { return };
        conformance::assert_usage_retention(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_skill_state_store() {
        let Some(s) = store().await else { return };
        conformance::assert_skill_state_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_read_state_store() {
        let Some(s) = store().await else { return };
        conformance::assert_read_state_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_notification_store() {
        let Some(s) = store().await else { return };
        conformance::assert_notification_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_workspace_store() {
        let Some(s) = store().await else { return };
        conformance::assert_workspace_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_workspace_sibling_names() {
        let Some(s) = store().await else { return };
        conformance::assert_workspace_sibling_names(s.clone()).await;
        drop_db(&s).await;
    }

    /// The reason issue #553 could not defer the Mongo backend: hosted tenants
    /// run MongoDB, and this is the only lane where the GridFS path — and the
    /// 17 MiB case that proves the 16 MB BSON document cap is not in play —
    /// actually executes.
    #[tokio::test]
    async fn conformance_workspace_binary_store() {
        let Some(s) = store().await else { return };
        conformance::assert_workspace_binary_store(s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_workspace_read_capped() {
        let Some(s) = store().await else { return };
        conformance::assert_workspace_read_capped(s.clone()).await;
        drop_db(&s).await;
    }

    /// Issue #759. The only lane where the folder-claim primitive's contention
    /// case runs against the partial unique index that actually decides it —
    /// this backend has neither a lock nor a transaction to fall back on.
    #[tokio::test]
    async fn conformance_workspace_folder_claims() {
        let Some(s) = store().await else { return };
        conformance::assert_workspace_folder_claims(s.clone()).await;
        drop_db(&s).await;
    }

    /// Issue #1839: the adoption lease, on the backend that can only *narrow*
    /// Race 1 — the `$set` an adoption writes is what a later `delete_if_empty`
    /// reads, so the sequential contract (mark, then refuse) still holds even
    /// though a truly concurrent delete can still miss an in-flight `$set`.
    #[tokio::test]
    async fn conformance_workspace_adoption_lease() {
        let Some(s) = store().await else { return };
        conformance::assert_workspace_adoption_lease(s.clone()).await;
        drop_db(&s).await;
    }

    /// Issue #887's no-torn-read contract, on the other backend hosted tenants
    /// run. A document replace is atomic, so this passed before the `fs` fix
    /// and passes after — the contract is the port's, not one backend's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn conformance_workspace_read_never_tears() {
        let Some(s) = store().await else { return };
        conformance::assert_workspace_read_never_tears(s.clone()).await;
        drop_db(&s).await;
    }

    /// The stat-then-open race fixed in the `fs` backend's `read_capped`.
    /// This backend measures and reads under one aggregation and passes on
    /// both sides of that fix — the contract is the port's, not one backend's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn conformance_workspace_read_capped_race() {
        let Some(s) = store().await else { return };
        conformance::assert_workspace_read_capped_race(s.clone()).await;
        drop_db(&s).await;
    }

    /// The sqlite pin's mirror, on the other backend hosted tenants run
    /// (issue #700).
    ///
    /// Both are needed: the guard exists because a backend accepts a node name
    /// carrying a path separator, and "accepts it" is a fact about each backend
    /// rather than about the port. A folder holding only such a child has no
    /// renderable path to it, reads as empty by every path-shaped measure, and
    /// would still lose it to the port's recursive `delete` (issue #671) — so
    /// the sweep must leave that folder standing here too.
    #[tokio::test]
    async fn workspace_sweep_keeps_a_folder_whose_only_child_has_no_renderable_path() {
        use crate::company::workspace_sweep::sweep_empty_agent_folders;
        use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

        let Some(s) = store().await else { return };
        let company = CompanyId::new("acme");
        let node = |id: &str, name: &str, kind, parent: Option<&str>| WorkspaceNode {
            id: id.to_string(),
            name: name.to_string(),
            kind,
            parent_id: parent.map(str::to_string),
            updated_at_millis: 1,
            created_by: WorkspaceOrigin::Seed,
            updated_by: WorkspaceOrigin::Seed,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        };

        for (id, name, kind, parent, body) in [
            ("root", "Agents", NodeKind::Folder, None, None),
            ("ghost", "ceo", NodeKind::Folder, Some("root"), None),
            ("empty", "cto", NodeKind::Folder, Some("root"), None),
            (
                "hidden",
                "q/r.md",
                NodeKind::File,
                Some("ghost"),
                Some("# quarterly"),
            ),
        ] {
            WorkspaceStore::create(s.as_ref(), &company, &node(id, name, kind, parent), body)
                .await
                .expect("mongodb accepts these names — that is why the guard exists");
        }

        let removed = sweep_empty_agent_folders(s.as_ref(), &company, false)
            .await
            .unwrap();

        assert_eq!(
            removed.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            vec!["empty"],
            "only the genuinely empty folder may go"
        );
        let ids: Vec<String> = WorkspaceStore::tree(s.as_ref(), &company)
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect();
        assert!(
            ids.contains(&"ghost".to_string()) && ids.contains(&"hidden".to_string()),
            "the folder and its unaddressable child must both survive, got {ids:?}"
        );
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn durable_ownership_round_trip() {
        let Some(s) = store().await else { return };
        let id = CompanyId::new("acme");
        s.set_owner(&id, "tenant-a").await.expect("set owner");
        s.set_owner(&id, "tenant-b").await.expect("update owner");
        let owners = s.owners().await.expect("owners");
        assert_eq!(owners, vec![(id.clone(), "tenant-b".to_string())]);
        s.remove_owner(&id).await.expect("remove owner");
        assert!(s.owners().await.expect("owners").is_empty());
        drop_db(&s).await;
    }
}
