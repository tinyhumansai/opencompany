//! SQLite-backed implementations of the storage ports.
//!
//! One [`SqliteStore`] opens a single bundled-SQLite connection and implements
//! every durable port — [`CompanyStore`], [`EventLog`], [`MemoryStore`],
//! [`ContextStore`], and [`SecretStore`] — sharing that connection behind an
//! `Arc<Mutex<_>>`. The same `Arc<SqliteStore>` can therefore be injected into
//! all four `RuntimeBuilder::with_*` setters so one database file serves the
//! whole company.
//!
//! ## Isolation and append-only semantics
//!
//! Every table is keyed on `company_id` first and every query filters
//! `WHERE company_id = ?`, so company A can never read company B's rows. Event
//! and ledger tables are append-only: sequence and index columns are assigned
//! `COALESCE(MAX(..)+1, 0)` under the connection lock, reproducing the fs
//! stores' 0-based, monotonic-per-company semantics.
//!
//! ## Concurrency
//!
//! `rusqlite` is synchronous. Each async method locks the `std::sync::Mutex`,
//! does its work, and releases the guard *without* crossing an `.await`, so the
//! non-`Send` guard never escapes a suspension point and the boxed futures stay
//! `Send`.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use async_trait::async_trait;
use futures::stream::BoxStream;
use rusqlite::{Connection, OptionalExtension, params};
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

/// Schema for every port table. Idempotent: safe to run on each `open`.
const MIGRATIONS: &str = r#"
CREATE TABLE IF NOT EXISTS company (
    company_id    TEXT PRIMARY KEY,
    manifest_toml TEXT NOT NULL,
    lifecycle     TEXT NOT NULL,
    overlay_json  TEXT NOT NULL DEFAULT '[]',
    updated_ms    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ledger (
    company_id TEXT NOT NULL,
    idx        INTEGER NOT NULL,
    entry_json TEXT NOT NULL,
    at_ms      INTEGER NOT NULL,
    PRIMARY KEY (company_id, idx)
);
CREATE TABLE IF NOT EXISTS events (
    company_id TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    event_json TEXT NOT NULL,
    at_ms      INTEGER NOT NULL,
    PRIMARY KEY (company_id, seq)
);
CREATE TABLE IF NOT EXISTS memory_traces (
    company_id TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    trace_json TEXT NOT NULL,
    at_ms      INTEGER NOT NULL,
    PRIMARY KEY (company_id, seq)
);
CREATE TABLE IF NOT EXISTS memory_tasks (
    company_id  TEXT NOT NULL,
    id          TEXT NOT NULL,
    result_json TEXT NOT NULL,
    at_ms       INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE TABLE IF NOT EXISTS context_chunks (
    company_id TEXT NOT NULL,
    addr       TEXT NOT NULL,
    label      TEXT NOT NULL,
    body       TEXT NOT NULL,
    len        INTEGER NOT NULL,
    PRIMARY KEY (company_id, addr)
);
CREATE TABLE IF NOT EXISTS secrets (
    company_id TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (company_id, key)
);
CREATE TABLE IF NOT EXISTS inbox (
    company_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    inbox       TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY (company_id, seq)
);
CREATE TABLE IF NOT EXISTS inbox_meta (
    company_id TEXT NOT NULL,
    key        TEXT NOT NULL,
    meta_json  TEXT NOT NULL,
    PRIMARY KEY (company_id, key)
);
CREATE TABLE IF NOT EXISTS tasks (
    company_id TEXT NOT NULL,
    id         TEXT NOT NULL,
    task_json  TEXT NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE TABLE IF NOT EXISTS facts (
    company_id TEXT NOT NULL,
    id         TEXT NOT NULL,
    fact_json  TEXT NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE TABLE IF NOT EXISTS usage_samples (
    company_id TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    at_ms      INTEGER NOT NULL,
    sample_json TEXT NOT NULL,
    PRIMARY KEY (company_id, seq)
);
CREATE TABLE IF NOT EXISTS skill_state (
    company_id TEXT NOT NULL,
    slug       TEXT NOT NULL,
    state_json TEXT NOT NULL,
    PRIMARY KEY (company_id, slug)
);
CREATE TABLE IF NOT EXISTS workspace_nodes (
    company_id TEXT NOT NULL,
    id         TEXT NOT NULL,
    node_json  TEXT NOT NULL,
    content    TEXT NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
"#;

/// Maps a `rusqlite` failure onto the crate error type without a bare `?` on
/// I/O (which would collide with the existing `#[from] io::Error` mapping).
fn sql_err(e: rusqlite::Error) -> OpenCompanyError {
    OpenCompanyError::Store(format!("sqlite error: {e}"))
}

/// Translates a `usize` limit into a SQLite `LIMIT` value. `usize::MAX` (the
/// "read everything" sentinel used by export/replay) maps to `-1`, SQLite's
/// unbounded-limit encoding.
fn sql_limit(limit: usize) -> i64 {
    if limit > i64::MAX as usize {
        -1
    } else {
        limit as i64
    }
}

/// A single SQLite database implementing all five storage ports.
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<StdMutex<Connection>>,
    senders: Arc<StdMutex<HashMap<CompanyId, broadcast::Sender<StoredEvent>>>>,
}

impl SqliteStore {
    /// Opens (creating if absent) the database at `path` and runs migrations.
    /// Pass `":memory:"` for an ephemeral database, or use [`Self::open_in_memory`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).map_err(sql_err)?;
        Self::from_conn(conn)
    }

    /// Opens a private in-memory database (migrated), primarily for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(sql_err)?;
        Self::from_conn(conn)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(MIGRATIONS).map_err(sql_err)?;
        Ok(Self {
            conn: Arc::new(StdMutex::new(conn)),
            senders: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("sqlite connection mutex poisoned")
    }

    fn sender_for(&self, id: &CompanyId) -> broadcast::Sender<StoredEvent> {
        let mut map = self.senders.lock().expect("sender map poisoned");
        map.entry(id.clone())
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }
}

// ---------------------------------------------------------------------------
// CompanyStore
// ---------------------------------------------------------------------------

#[async_trait]
impl CompanyStore for SqliteStore {
    async fn load(&self, id: &CompanyId) -> Result<Option<CompanyRecord>> {
        let conn = self.conn();
        let row: Option<(String, String, String)> = conn
            .query_row(
                "SELECT manifest_toml, lifecycle, overlay_json FROM company WHERE company_id = ?1",
                params![id.as_ref()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((manifest_toml, lifecycle, overlay_json)) = row else {
            return Ok(None);
        };
        let manifest: CompanyManifest = toml::from_str(&manifest_toml)
            .map_err(|e| OpenCompanyError::Store(format!("invalid company.toml: {e}")))?;
        let overlay_agents = serde_json::from_str(&overlay_json)?;

        let mut stmt = conn
            .prepare("SELECT entry_json FROM ledger WHERE company_id = ?1 ORDER BY idx")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![id.as_ref()], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut ledger = Vec::new();
        for row in rows {
            let json = row.map_err(sql_err)?;
            ledger.push(serde_json::from_str::<LedgerEntry>(&json)?);
        }

        Ok(Some(CompanyRecord {
            id: id.clone(),
            manifest,
            ledger,
            lifecycle,
            overlay_agents,
        }))
    }

    async fn save(&self, record: &CompanyRecord) -> Result<()> {
        let manifest_toml = toml::to_string(&record.manifest)
            .map_err(|e| OpenCompanyError::Store(format!("cannot serialize manifest: {e}")))?;
        let overlay_json = serde_json::to_string(&record.overlay_agents)?;
        let conn = self.conn();
        // Append-only: `save` upserts the company row and never touches ledger.
        conn.execute(
            "INSERT INTO company (company_id, manifest_toml, lifecycle, overlay_json, updated_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(company_id) DO UPDATE SET \
               manifest_toml = excluded.manifest_toml, \
               lifecycle = excluded.lifecycle, \
               overlay_json = excluded.overlay_json, \
               updated_ms = excluded.updated_ms",
            params![
                record.id.as_ref(),
                manifest_toml,
                record.lifecycle,
                overlay_json,
                now_millis() as i64
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<CompanySummary>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT company_id, manifest_toml, lifecycle FROM company ORDER BY company_id")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, manifest_toml, lifecycle) = row.map_err(sql_err)?;
            let Ok(manifest) = toml::from_str::<CompanyManifest>(&manifest_toml) else {
                continue;
            };
            out.push(CompanySummary {
                id: CompanyId::new(id),
                name: manifest.company.name,
                lifecycle,
            });
        }
        Ok(out)
    }

    async fn append_ledger(&self, id: &CompanyId, entry: LedgerEntry) -> Result<()> {
        let entry_json = serde_json::to_string(&entry)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO ledger (company_id, idx, entry_json, at_ms) VALUES \
             (?1, (SELECT COALESCE(MAX(idx) + 1, 0) FROM ledger WHERE company_id = ?1), ?2, ?3)",
            params![id.as_ref(), entry_json, entry.at_millis as i64],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EventLog
// ---------------------------------------------------------------------------

#[async_trait]
impl EventLog for SqliteStore {
    async fn append(&self, id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
        let event_json = serde_json::to_string(&event)?;
        let at_millis = now_millis();
        let seq = {
            let conn = self.conn();
            let seq: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(seq) + 1, 0) FROM events WHERE company_id = ?1",
                    params![id.as_ref()],
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            conn.execute(
                "INSERT INTO events (company_id, seq, event_json, at_ms) VALUES (?1, ?2, ?3, ?4)",
                params![id.as_ref(), seq, event_json, at_millis as i64],
            )
            .map_err(sql_err)?;
            seq as u64
        };

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
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT seq, event_json, at_ms FROM events \
                 WHERE company_id = ?1 AND seq >= ?2 ORDER BY seq LIMIT ?3",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(
                params![id.as_ref(), seq.value() as i64, sql_limit(limit)],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, event_json, at_ms) = row.map_err(sql_err)?;
            out.push(StoredEvent {
                seq: EventSeq::new(seq as u64),
                company: id.clone(),
                event: serde_json::from_str(&event_json)?,
                at_millis: at_ms as u64,
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
impl MemoryStore for SqliteStore {
    async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()> {
        let trace_json = serde_json::to_string(&trace)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO memory_traces (company_id, seq, trace_json, at_ms) VALUES \
             (?1, (SELECT COALESCE(MAX(seq) + 1, 0) FROM memory_traces WHERE company_id = ?1), ?2, ?3)",
            params![id.as_ref(), trace_json, trace.at_millis as i64],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn recent_traces(&self, id: &CompanyId, limit: usize) -> Result<Vec<CompressedTrace>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT trace_json FROM memory_traces WHERE company_id = ?1 \
                 ORDER BY seq DESC LIMIT ?2",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![id.as_ref(), sql_limit(limit)], |r| {
                r.get::<_, String>(0)
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let json = row.map_err(sql_err)?;
            out.push(serde_json::from_str::<CompressedTrace>(&json)?);
        }
        // Query returned newest-first; the port contract is newest-last.
        out.reverse();
        Ok(out)
    }

    async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()> {
        let result_json = serde_json::to_string(&result)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO memory_tasks (company_id, id, result_json, at_ms) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(company_id, id) DO UPDATE SET \
               result_json = excluded.result_json, at_ms = excluded.at_ms",
            params![
                id.as_ref(),
                result.task_id,
                result_json,
                now_millis() as i64
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64> {
        let conn = self.conn();
        let removed = match policy {
            EvictionPolicy::KeepRecent { n } => conn
                .execute(
                    "DELETE FROM memory_traces WHERE company_id = ?1 AND seq NOT IN \
                     (SELECT seq FROM memory_traces WHERE company_id = ?1 ORDER BY seq DESC LIMIT ?2)",
                    params![id.as_ref(), sql_limit(n)],
                )
                .map_err(sql_err)?,
            EvictionPolicy::OlderThan { before_millis } => conn
                .execute(
                    "DELETE FROM memory_traces WHERE company_id = ?1 AND at_ms < ?2",
                    params![id.as_ref(), before_millis as i64],
                )
                .map_err(sql_err)?,
        };
        Ok(removed as u64)
    }
}

// ---------------------------------------------------------------------------
// ContextStore
// ---------------------------------------------------------------------------

#[async_trait]
impl ContextStore for SqliteStore {
    async fn put(&self, id: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr> {
        let addr = content_address(&chunk.body);
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO context_chunks (company_id, addr, label, body, len) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id.as_ref(),
                addr,
                chunk.label,
                chunk.body,
                chunk.body.len() as i64
            ],
        )
        .map_err(sql_err)?;
        Ok(ChunkAddr::new(addr))
    }

    async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT addr, label, len FROM context_chunks WHERE company_id = ?1 ORDER BY rowid",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![id.as_ref()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (addr, label, len) = row.map_err(sql_err)?;
            if label.starts_with(prefix) {
                out.push(ChunkMeta {
                    addr: ChunkAddr::new(addr),
                    label,
                    len: len as usize,
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
        let conn = self.conn();
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM context_chunks WHERE company_id = ?1 AND addr = ?2",
                params![id.as_ref(), addr.as_ref()],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let body = body.ok_or_else(|| {
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
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT addr, body FROM context_chunks WHERE company_id = ?1 ORDER BY rowid")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![id.as_ref()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(sql_err)?;
        let mut hits = Vec::new();
        for row in rows {
            if hits.len() >= limit {
                break;
            }
            let (addr, body) = row.map_err(sql_err)?;
            if let Some(pos) = body.find(query) {
                let start = pos.saturating_sub(24);
                let end = (pos + query.len() + 24).min(body.len());
                hits.push(ChunkHit {
                    addr: ChunkAddr::new(addr),
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
impl SecretStore for SqliteStore {
    async fn get(&self, company: &CompanyId, key: &str) -> Result<Option<SecretValue>> {
        let conn = self.conn();
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM secrets WHERE company_id = ?1 AND key = ?2",
                params![company.as_ref(), key],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        Ok(value.map(SecretValue))
    }

    async fn set(&self, company: &CompanyId, key: &str, value: SecretValue) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO secrets (company_id, key, value) VALUES (?1, ?2, ?3) \
             ON CONFLICT(company_id, key) DO UPDATE SET value = excluded.value",
            params![company.as_ref(), key, value.expose()],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// InboxStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::inbox::InboxStore for SqliteStore {
    async fn inboxes(&self, company: &CompanyId) -> Result<Vec<crate::ports::inbox::InboxMeta>> {
        use std::collections::BTreeMap;
        let conn = self.conn();
        let mut out: BTreeMap<String, crate::ports::inbox::InboxMeta> = BTreeMap::new();
        // Explicit metadata first.
        let mut stmt = conn
            .prepare("SELECT meta_json FROM inbox_meta WHERE company_id = ?1")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref()], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        for row in rows {
            let meta: crate::ports::inbox::InboxMeta =
                serde_json::from_str(&row.map_err(sql_err)?)?;
            out.insert(meta.key.clone(), meta);
        }
        // Synthesize a default enabled meta for message-only inboxes.
        let mut stmt = conn
            .prepare("SELECT DISTINCT inbox FROM inbox WHERE company_id = ?1")
            .map_err(sql_err)?;
        let names = stmt
            .query_map(params![company.as_ref()], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        for name in names {
            let key = name.map_err(sql_err)?;
            out.entry(key.clone())
                .or_insert_with(|| crate::ports::inbox::InboxMeta {
                    key: key.clone(),
                    name: key.clone(),
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
        let json = serde_json::to_string(meta)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO inbox_meta (company_id, key, meta_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(company_id, key) DO UPDATE SET meta_json = excluded.meta_json",
            params![company.as_ref(), key, json],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn append(
        &self,
        company: &CompanyId,
        msg: &crate::ports::inbox::EmailRecord,
    ) -> Result<()> {
        let json = serde_json::to_string(msg)?;
        let conn = self.conn();
        // Monotonic per-company append seq preserves insertion order.
        let next: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq) + 1, 0) FROM inbox WHERE company_id = ?1",
                params![company.as_ref()],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        conn.execute(
            "INSERT INTO inbox (company_id, seq, inbox, record_json) VALUES (?1, ?2, ?3, ?4)",
            params![company.as_ref(), next, msg.inbox, json],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn messages(
        &self,
        company: &CompanyId,
        key: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::ports::inbox::EmailRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT record_json FROM inbox WHERE company_id = ?1 AND inbox = ?2 \
                 ORDER BY seq LIMIT ?3 OFFSET ?4",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(
                params![company.as_ref(), key, sql_limit(limit), offset as i64],
                |r| r.get::<_, String>(0),
            )
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
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
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT seq, record_json FROM inbox WHERE company_id = ?1 AND inbox = ?2")
            .map_err(sql_err)?;
        let rows: Vec<(i64, String)> = stmt
            .query_map(params![company.as_ref(), key], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(sql_err)?
            .collect::<std::result::Result<_, _>>()
            .map_err(sql_err)?;
        let mut unread = 0u64;
        for (seq, json) in rows {
            let mut record: EmailRecord = serde_json::from_str(&json)?;
            let hit = match ids {
                Some(ids) => ids.iter().any(|id| id == &record.id),
                None => true,
            };
            if hit && !record.read {
                record.read = true;
                conn.execute(
                    "UPDATE inbox SET record_json = ?1 WHERE company_id = ?2 AND seq = ?3",
                    params![serde_json::to_string(&record)?, company.as_ref(), seq],
                )
                .map_err(sql_err)?;
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
impl crate::ports::tasks::TaskStore for SqliteStore {
    async fn list(&self, company: &CompanyId) -> Result<Vec<crate::ports::tasks::TaskRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT task_json FROM tasks WHERE company_id = ?1 ORDER BY updated_ms DESC")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref()], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
        }
        Ok(out)
    }

    async fn upsert(
        &self,
        company: &CompanyId,
        task: &crate::ports::tasks::TaskRecord,
    ) -> Result<()> {
        let json = serde_json::to_string(task)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO tasks (company_id, id, task_json, updated_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(company_id, id) DO UPDATE SET task_json = excluded.task_json, \
             updated_ms = excluded.updated_ms",
            params![
                company.as_ref(),
                task.id,
                json,
                task.updated_at_millis as i64
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM tasks WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// FactStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::facts::FactStore for SqliteStore {
    async fn list(
        &self,
        company: &CompanyId,
        query: Option<&str>,
        kind: Option<crate::ports::facts::FactKind>,
    ) -> Result<Vec<crate::ports::facts::FactRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT fact_json FROM facts WHERE company_id = ?1 ORDER BY updated_ms DESC")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref()], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut out: Vec<crate::ports::facts::FactRecord> = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
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
        let json = serde_json::to_string(fact)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO facts (company_id, id, fact_json, updated_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(company_id, id) DO UPDATE SET fact_json = excluded.fact_json, \
             updated_ms = excluded.updated_ms",
            params![
                company.as_ref(),
                fact.id,
                json,
                fact.updated_at_millis as i64
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM facts WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// UsageMeter
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::usage::UsageMeter for SqliteStore {
    async fn record(
        &self,
        company: &CompanyId,
        sample: &crate::ports::usage::UsageSample,
    ) -> Result<()> {
        let json = serde_json::to_string(sample)?;
        let conn = self.conn();
        let next: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq) + 1, 0) FROM usage_samples WHERE company_id = ?1",
                params![company.as_ref()],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        conn.execute(
            "INSERT INTO usage_samples (company_id, seq, at_ms, sample_json) VALUES (?1, ?2, ?3, ?4)",
            params![company.as_ref(), next, sample.at_millis as i64, json],
        )
        .map_err(sql_err)?;
        // Retention: drop samples older than the 90-day window, anchored to the
        // newest sample just written.
        let cutoff = crate::ports::usage::retention_cutoff(sample.at_millis);
        conn.execute(
            "DELETE FROM usage_samples WHERE company_id = ?1 AND at_ms < ?2",
            params![company.as_ref(), cutoff as i64],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn query(
        &self,
        company: &CompanyId,
        since_millis: u64,
    ) -> Result<Vec<crate::ports::usage::UsageSample>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT sample_json FROM usage_samples WHERE company_id = ?1 AND at_ms >= ?2 \
                 ORDER BY at_ms, seq",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref(), since_millis as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// SkillStateStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::skills_state::SkillStateStore for SqliteStore {
    async fn list(
        &self,
        company: &CompanyId,
    ) -> Result<Vec<crate::ports::skills_state::SkillState>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT state_json FROM skill_state WHERE company_id = ?1 ORDER BY slug")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref()], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
        }
        Ok(out)
    }

    async fn set(
        &self,
        company: &CompanyId,
        state: &crate::ports::skills_state::SkillState,
    ) -> Result<()> {
        let json = serde_json::to_string(state)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO skill_state (company_id, slug, state_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(company_id, slug) DO UPDATE SET state_json = excluded.state_json",
            params![company.as_ref(), state.slug, json],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn remove(&self, company: &CompanyId, slug: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM skill_state WHERE company_id = ?1 AND slug = ?2",
                params![company.as_ref(), slug],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// WorkspaceStore
// ---------------------------------------------------------------------------

impl SqliteStore {
    /// Loads every workspace node for a company into an id-keyed map.
    fn workspace_nodes(
        &self,
        conn: &Connection,
        company: &CompanyId,
    ) -> Result<HashMap<String, crate::ports::workspace::WorkspaceNode>> {
        let mut stmt = conn
            .prepare("SELECT node_json FROM workspace_nodes WHERE company_id = ?1")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref()], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut out = HashMap::new();
        for row in rows {
            let node: crate::ports::workspace::WorkspaceNode =
                serde_json::from_str(&row.map_err(sql_err)?)?;
            out.insert(node.id.clone(), node);
        }
        Ok(out)
    }
}

#[async_trait]
impl crate::ports::workspace::WorkspaceStore for SqliteStore {
    async fn tree(
        &self,
        company: &CompanyId,
    ) -> Result<Vec<crate::ports::workspace::WorkspaceNode>> {
        let conn = self.conn();
        Ok(self
            .workspace_nodes(&conn, company)?
            .into_values()
            .collect())
    }

    async fn read(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<(crate::ports::workspace::WorkspaceNode, String)>> {
        let conn = self.conn();
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT node_json, content FROM workspace_nodes WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        match row {
            Some((node_json, content)) => Ok(Some((serde_json::from_str(&node_json)?, content))),
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
        let conn = self.conn();
        let node_json: Option<String> = conn
            .query_row(
                "SELECT node_json FROM workspace_nodes WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(node_json) = node_json else {
            return Err(OpenCompanyError::CompanyNotFound(format!(
                "workspace node {id}"
            )));
        };
        let mut node: crate::ports::workspace::WorkspaceNode = serde_json::from_str(&node_json)?;
        if node.kind != NodeKind::File {
            return Err(OpenCompanyError::InvalidRequest(
                "cannot write content to a folder".to_string(),
            ));
        }
        node.updated_at_millis = now_millis();
        conn.execute(
            "UPDATE workspace_nodes SET node_json = ?1, content = ?2, updated_ms = ?3 \
             WHERE company_id = ?4 AND id = ?5",
            params![
                serde_json::to_string(&node)?,
                content,
                node.updated_at_millis as i64,
                company.as_ref(),
                id
            ],
        )
        .map_err(sql_err)?;
        Ok(node)
    }

    async fn create(
        &self,
        company: &CompanyId,
        node: &crate::ports::workspace::WorkspaceNode,
        content: Option<&str>,
    ) -> Result<()> {
        use crate::ports::workspace::NodeKind;
        let conn = self.conn();
        let nodes = self.workspace_nodes(&conn, company)?;
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
        conn.execute(
            "INSERT INTO workspace_nodes (company_id, id, node_json, content, updated_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                company.as_ref(),
                node.id,
                serde_json::to_string(node)?,
                content.unwrap_or(""),
                node.updated_at_millis as i64
            ],
        )
        .map_err(sql_err)?;
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
        let conn = self.conn();
        let nodes = self.workspace_nodes(&conn, company)?;
        if !nodes.contains_key(id) {
            return Err(OpenCompanyError::CompanyNotFound(format!(
                "workspace node {id}"
            )));
        }
        // A move to root (`Some(None)`) never forms a cycle.
        if let Some(Some(parent)) = parent {
            if parent == id || workspace_descendants(&nodes, id).contains(parent) {
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
        conn.execute(
            "UPDATE workspace_nodes SET node_json = ?1, updated_ms = ?2 \
             WHERE company_id = ?3 AND id = ?4",
            params![
                serde_json::to_string(&node)?,
                node.updated_at_millis as i64,
                company.as_ref(),
                id
            ],
        )
        .map_err(sql_err)?;
        Ok(node)
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let conn = self.conn();
        let nodes = self.workspace_nodes(&conn, company)?;
        if !nodes.contains_key(id) {
            return Ok(false);
        }
        let mut to_remove = workspace_descendants(&nodes, id);
        to_remove.insert(id.to_string());
        for node_id in &to_remove {
            conn.execute(
                "DELETE FROM workspace_nodes WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), node_id],
            )
            .map_err(sql_err)?;
        }
        Ok(true)
    }

    async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
        let conn = self.conn();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM workspace_nodes WHERE company_id = ?1",
                params![company.as_ref()],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        Ok(count == 0)
    }
}

/// Collects the ids of every descendant of `id` (excluding `id`), shared by the
/// sqlite workspace's cycle check and recursive delete.
fn workspace_descendants(
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::store::conformance;
    use futures::StreamExt;

    fn store() -> Arc<SqliteStore> {
        Arc::new(SqliteStore::open_in_memory().expect("open in-memory sqlite"))
    }

    #[tokio::test]
    async fn conformance_isolation_by_company() {
        let s = store();
        conformance::assert_isolation_by_company(s.clone(), s.clone(), s.clone(), s).await;
    }

    #[tokio::test]
    async fn conformance_append_only_event_and_ledger() {
        let s = store();
        conformance::assert_append_only_event_and_ledger(s.clone(), s).await;
    }

    #[tokio::test]
    async fn conformance_monotonic_event_seq() {
        let s = store();
        conformance::assert_monotonic_event_seq(s).await;
    }

    #[tokio::test]
    async fn conformance_export_totality() {
        let s = store();
        conformance::assert_export_totality(s.clone(), s.clone(), s.clone(), s).await;
    }

    #[tokio::test]
    async fn conformance_inbox_store() {
        conformance::assert_inbox_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_task_store() {
        conformance::assert_task_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_fact_store() {
        conformance::assert_fact_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_usage_meter() {
        conformance::assert_usage_meter(store()).await;
    }

    #[tokio::test]
    async fn conformance_usage_retention() {
        conformance::assert_usage_retention(store()).await;
    }

    #[tokio::test]
    async fn conformance_skill_state_store() {
        conformance::assert_skill_state_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_workspace_store() {
        conformance::assert_workspace_store(store()).await;
    }

    #[tokio::test]
    async fn one_store_serves_every_port_through_arc() {
        // A single Arc<SqliteStore> satisfies all five port trait objects — the
        // shape a platform-mode `build_runtime` injects into every `with_*`.
        let s = store();
        let company: Arc<dyn CompanyStore> = s.clone();
        let events: Arc<dyn EventLog> = s.clone();
        let memory: Arc<dyn MemoryStore> = s.clone();
        let context: Arc<dyn ContextStore> = s.clone();
        let secrets: Arc<dyn SecretStore> = s.clone();

        let id = CompanyId::new("acme");
        company
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: toml::from_str(
                    "[company]\nname=\"Acme\"\noutput=\"widgets\"\n[[agent]]\nid=\"ceo\"\nrole=\"Chief\"\n[policy]\nmode=\"supervised\"\n",
                )
                .unwrap(),
                ledger: Vec::new(),
                lifecycle: "running".into(),
                overlay_agents: Vec::new(),
            })
            .await
            .unwrap();
        events
            .append(&id, CompanyEvent::OperatorMessage { text: "hi".into() })
            .await
            .unwrap();
        memory
            .save_trace(&id, CompressedTrace::now("c0", "s0"))
            .await
            .unwrap();
        context
            .put(
                &id,
                ContextChunk {
                    label: "notes".into(),
                    body: "body".into(),
                },
            )
            .await
            .unwrap();
        secrets
            .set(&id, "token", SecretValue("secret".into()))
            .await
            .unwrap();

        assert!(company.load(&id).await.unwrap().is_some());
        assert_eq!(
            events
                .read_from(&id, EventSeq::new(0), 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(memory.recent_traces(&id, 10).await.unwrap().len(), 1);
        assert_eq!(context.list(&id, "").await.unwrap().len(), 1);
        assert_eq!(
            secrets.get(&id, "token").await.unwrap(),
            Some(SecretValue("secret".into()))
        );
    }

    #[tokio::test]
    async fn subscribe_delivers_new_event() {
        let s = store();
        let id = CompanyId::new("acme");
        let mut stream = s.subscribe(&id);
        s.append(&id, CompanyEvent::OperatorMessage { text: "hi".into() })
            .await
            .unwrap();
        let received = stream.next().await.expect("event delivered");
        assert_eq!(
            received.event,
            CompanyEvent::OperatorMessage { text: "hi".into() }
        );
    }

    #[tokio::test]
    async fn task_result_upserts_by_id() {
        let s = store();
        let id = CompanyId::new("acme");
        s.save_task_result(
            &id,
            TaskResult {
                task_id: "t1".into(),
                ok: false,
                output: serde_json::json!({"v": 1}),
            },
        )
        .await
        .unwrap();
        // Same id again overwrites rather than duplicating.
        s.save_task_result(
            &id,
            TaskResult {
                task_id: "t1".into(),
                ok: true,
                output: serde_json::json!({"v": 2}),
            },
        )
        .await
        .unwrap();
        let count: i64 = s
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM memory_tasks WHERE company_id = ?1 AND id = ?2",
                params![id.as_ref(), "t1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn evict_keep_recent_and_older_than() {
        let s = store();
        let id = CompanyId::new("acme");
        for i in 0..5 {
            s.save_trace(&id, CompressedTrace::now(format!("c{i}"), format!("s{i}")))
                .await
                .unwrap();
        }
        let removed = s
            .evict(&id, EvictionPolicy::KeepRecent { n: 2 })
            .await
            .unwrap();
        assert_eq!(removed, 3);
        let kept = s.recent_traces(&id, 10).await.unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[1].cycle_id, "c4");

        // A cutoff comfortably in the future evicts every remaining trace.
        let removed = s
            .evict(
                &id,
                EvictionPolicy::OlderThan {
                    before_millis: now_millis() + 60_000,
                },
            )
            .await
            .unwrap();
        assert_eq!(removed, 2);
        assert!(s.recent_traces(&id, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn data_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("company.db");
        let id = CompanyId::new("acme");
        {
            let s = SqliteStore::open(&path).unwrap();
            s.append(
                &id,
                CompanyEvent::OperatorMessage {
                    text: "persist".into(),
                },
            )
            .await
            .unwrap();
        }
        // A fresh handle over the same file sees the durable event.
        let s = SqliteStore::open(&path).unwrap();
        let events = s.read_from(&id, EventSeq::new(0), 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event,
            CompanyEvent::OperatorMessage {
                text: "persist".into()
            }
        );
    }
}
