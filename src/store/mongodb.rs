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
use mongodb::options::{FindOneAndUpdateOptions, IndexOptions, ReturnDocument, UpdateOptions};
use mongodb::{Client, Collection, Database, IndexModel};
use tokio::sync::broadcast;

use crate::Result;
use crate::company::CompanyManifest;
use crate::error::OpenCompanyError;
use crate::ports::context::ContextStore;
use crate::ports::events::EventLog;
use crate::ports::memory::MemoryStore;
use crate::ports::now_millis;
use crate::ports::secrets::SecretStore;
use crate::ports::store::CompanyStore;
use crate::ports::types::{
    ChunkAddr, ChunkHit, ChunkMeta, CompanyEvent, CompanyId, CompanyRecord, CompanySummary,
    CompressedTrace, ContextChunk, EventSeq, EvictionPolicy, LedgerEntry, SecretValue, StoredEvent,
    TaskResult,
};
use crate::store::content_address;

fn mongo_err(e: impl std::fmt::Display) -> OpenCompanyError {
    OpenCompanyError::Store(format!("mongodb error: {e}"))
}

/// The port contract's "read everything" sentinel (`usize::MAX`) means no
/// limit; everything else maps onto the driver's `i64` limit.
fn find_limit(limit: usize) -> Option<i64> {
    if limit > i64::MAX as usize {
        None
    } else {
        Some(limit as i64)
    }
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
        Ok(store)
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
        let plans: [(&str, IndexModel); 14] = [
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
            ("usage_samples", unique(doc! {"company_id": 1, "seq": 1})),
            ("skill_state", unique(doc! {"company_id": 1, "slug": 1})),
            (
                "workspace_nodes",
                unique(doc! {"company_id": 1, "node_id": 1}),
            ),
        ];
        for (name, index) in plans {
            self.collection(name)
                .create_index(index)
                .await
                .map_err(mongo_err)?;
        }
        self.collection("owners")
            .create_index(unique(doc! {"company_id": 1}))
            .await
            .map_err(mongo_err)?;
        Ok(())
    }

    fn collection(&self, name: &str) -> Collection<Document> {
        self.db.collection::<Document>(name)
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
        let manifest: CompanyManifest = toml::from_str(&get_str(&company, "manifest_toml")?)
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

        let overlay_agents = match company.get_str("overlay_json") {
            Ok(json) => serde_json::from_str(json)?,
            Err(_) => Vec::new(),
        };
        Ok(Some(CompanyRecord {
            id: id.clone(),
            manifest,
            ledger,
            lifecycle: get_str(&company, "lifecycle")?,
            overlay_agents,
        }))
    }

    async fn save(&self, record: &CompanyRecord) -> Result<()> {
        let manifest_toml = toml::to_string(&record.manifest)
            .map_err(|e| OpenCompanyError::Store(format!("cannot serialize manifest: {e}")))?;
        let overlay_json = serde_json::to_string(&record.overlay_agents)?;
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
        if let Some(limit) = find_limit(limit) {
            find = find.limit(limit);
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
        if let Some(limit) = find_limit(limit) {
            find = find.limit(limit);
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
                // Collect the seqs to keep (newest n), delete the rest.
                let mut keep = Vec::new();
                if n > 0 {
                    let mut find = traces
                        .find(doc! {"company_id": id.as_ref()})
                        .sort(doc! {"seq": -1});
                    if let Some(limit) = find_limit(n) {
                        find = find.limit(limit);
                    }
                    let mut cursor = find.await.map_err(mongo_err)?;
                    while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
                        keep.push(get_i64(&doc, "seq")?);
                    }
                }
                traces
                    .delete_many(doc! {
                        "company_id": id.as_ref(),
                        "seq": {"$nin": keep},
                    })
                    .await
                    .map_err(mongo_err)?
                    .deleted_count
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
        let result = self
            .collection("context_chunks")
            .update_one(
                doc! {"company_id": id.as_ref(), "addr": &addr},
                doc! {"$setOnInsert": {
                    "label": &chunk.label,
                    "body": &chunk.body,
                    "len": chunk.body.len() as i64,
                    "ord": ord as i64,
                }},
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
            let label = get_str(&doc, "label")?;
            if label.starts_with(prefix) {
                out.push(ChunkMeta {
                    addr: ChunkAddr::new(get_str(&doc, "addr")?),
                    label,
                    len: get_i64(&doc, "len")? as usize,
                });
            }
        }
        Ok(out)
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
        let mut cursor = self
            .collection("context_chunks")
            .find(doc! {"company_id": id.as_ref()})
            .sort(doc! {"ord": 1})
            .await
            .map_err(mongo_err)?;
        let mut hits = Vec::new();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_err)? {
            if hits.len() >= limit {
                break;
            }
            let body = get_str(&doc, "body")?;
            if let Some(pos) = body.find(query) {
                let start = pos.saturating_sub(24);
                let end = (pos + query.len() + 24).min(body.len());
                hits.push(ChunkHit {
                    addr: ChunkAddr::new(get_str(&doc, "addr")?),
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
        if let Some(limit) = find_limit(limit) {
            find = find.limit(limit);
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
// WorkspaceStore
// ---------------------------------------------------------------------------

impl MongoStore {
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
            Some(doc) => Ok(Some((
                serde_json::from_str(&get_str(&doc, "node_json")?)?,
                get_str(&doc, "content")?,
            ))),
            None => Ok(None),
        }
    }

    async fn write(
        &self,
        company: &CompanyId,
        id: &str,
        content: &str,
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
        node.updated_at_millis = now_millis();
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
        self.collection("workspace_nodes")
            .insert_one(doc! {
                "company_id": company.as_ref(),
                "node_id": &node.id,
                "node_json": serde_json::to_string(node)?,
                "content": content.unwrap_or(""),
                "updated_ms": node.updated_at_millis as i64,
            })
            .await
            .map_err(mongo_err)?;
        Ok(())
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
        self.collection("workspace_nodes")
            .update_one(
                doc! {"company_id": company.as_ref(), "node_id": id},
                doc! {"$set": {
                    "node_json": serde_json::to_string(&node)?,
                    "updated_ms": node.updated_at_millis as i64,
                }},
            )
            .await
            .map_err(mongo_err)?;
        Ok(node)
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let nodes = self.workspace_nodes(company).await?;
        if !nodes.contains_key(id) {
            return Ok(false);
        }
        let mut to_remove = mongo_workspace_descendants(&nodes, id);
        to_remove.insert(id.to_string());
        let ids: Vec<&String> = to_remove.iter().collect();
        self.collection("workspace_nodes")
            .delete_many(doc! {"company_id": company.as_ref(), "node_id": {"$in": ids}})
            .await
            .map_err(mongo_err)?;
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
#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::store::conformance;

    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    async fn store() -> Option<Arc<MongoStore>> {
        let Ok(uri) = std::env::var("OPENCOMPANY_TEST_MONGODB_URI") else {
            eprintln!("skipping: OPENCOMPANY_TEST_MONGODB_URI is not set");
            return None;
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
        let store = MongoStore::connect(&uri, &db).await.expect("connect");
        Some(Arc::new(store))
    }

    async fn drop_db(store: &MongoStore) {
        let _ = store.db.drop().await;
    }

    #[tokio::test]
    async fn conformance_isolation_by_company() {
        let Some(s) = store().await else { return };
        conformance::assert_isolation_by_company(s.clone(), s.clone(), s.clone(), s.clone()).await;
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
    async fn conformance_export_totality() {
        let Some(s) = store().await else { return };
        conformance::assert_export_totality(s.clone(), s.clone(), s.clone(), s.clone()).await;
        drop_db(&s).await;
    }

    #[tokio::test]
    async fn conformance_inbox_store() {
        let Some(s) = store().await else { return };
        conformance::assert_inbox_store(s.clone()).await;
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
    async fn conformance_workspace_store() {
        let Some(s) = store().await else { return };
        conformance::assert_workspace_store(s.clone()).await;
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
