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
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
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
    stored_ms  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (company_id, addr)
);
-- The context index: one row per (addr, label) claim, the shape the fs
-- backend's JSONL index always had (issue #1300). `context_chunks` keeps one
-- body row per address (its `label` column stays the first write's label so a
-- downgraded binary keeps reading what it always read); this table is what
-- `list` and the label-scoped delete read and mutate.
CREATE TABLE IF NOT EXISTS context_chunk_labels (
    company_id TEXT NOT NULL,
    addr       TEXT NOT NULL,
    label      TEXT NOT NULL,
    stored_ms  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (company_id, addr, label)
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
CREATE TABLE IF NOT EXISTS ledger_specs (
    company_id TEXT NOT NULL,
    slug       TEXT NOT NULL,
    spec_json  TEXT NOT NULL,
    PRIMARY KEY (company_id, slug)
);
-- Append-only: `seq` is the fold's ordering and the only thing it may rely on.
-- A timestamp would not do — it is written by whichever replica is running, and
-- a clock that steps backwards would reorder the board.
CREATE TABLE IF NOT EXISTS ledger_events (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    company_id TEXT NOT NULL,
    ledger     TEXT NOT NULL,
    entry_id   TEXT NOT NULL,
    event_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS ledger_events_by_ledger
    ON ledger_events (company_id, ledger, seq);
CREATE TABLE IF NOT EXISTS facts (
    company_id TEXT NOT NULL,
    id         TEXT NOT NULL,
    fact_json  TEXT NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE TABLE IF NOT EXISTS artifacts (
    company_id    TEXT NOT NULL,
    id            TEXT NOT NULL,
    task_id       TEXT NOT NULL,
    artifact_json TEXT NOT NULL,
    updated_ms    INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE INDEX IF NOT EXISTS artifacts_by_task ON artifacts (company_id, task_id);
CREATE TABLE IF NOT EXISTS runs (
    company_id TEXT NOT NULL,
    id         TEXT NOT NULL,
    -- Nullable since issue #983: a chat turn is a recorded attempt at work that
    -- opens no card. This column is only the index mirror of `run_json`'s own
    -- `taskId`, so NULL here reads as "no card" in exactly the way the record
    -- does, and `task_id = ?` never matches it — which is what a per-card
    -- filter wants.
    task_id    TEXT,
    -- The index mirror of `run_json`'s `workflowRunId`, on the same terms as
    -- `task_id` above: NULL reads as "belongs to no workflow", and
    -- `workflow_run_id = ?` never matches it.
    workflow_run_id TEXT,
    -- Issue #1573: the desk the attempt was dispatched to, mirrored out of
    -- `run_json` so the console's per-teammate history is an indexed read
    -- rather than a scan. Nullable only so the additive `ALTER` on an existing
    -- database has something to write before the backfill runs; every row the
    -- store itself writes carries one.
    agent_id   TEXT,
    status     TEXT NOT NULL,
    attempt    INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    run_json   TEXT NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE INDEX IF NOT EXISTS runs_by_task ON runs (company_id, task_id);
CREATE INDEX IF NOT EXISTS runs_by_status ON runs (company_id, status);
-- NOTE: `runs_by_workflow_run` is deliberately NOT here. `CREATE TABLE IF NOT
-- EXISTS` is a no-op on a database that predates `workflow_run_id`, so an index
-- over that column would fail outright on exactly the legacy databases the
-- additive step below exists for. It is created in `from_conn`, after
-- `add_column_if_missing` has guaranteed the column.
-- `runs_by_agent` is deliberately NOT here; see `heal_runs_agent_id`.
CREATE TABLE IF NOT EXISTS run_steps (
    company_id TEXT NOT NULL,
    run_id     TEXT NOT NULL,
    step_seq   INTEGER NOT NULL,
    at_ms      INTEGER NOT NULL,
    step_json  TEXT NOT NULL,
    PRIMARY KEY (company_id, run_id, step_seq)
);
CREATE TABLE IF NOT EXISTS run_step_details (
    company_id  TEXT NOT NULL,
    run_id      TEXT NOT NULL,
    step_seq    INTEGER NOT NULL,
    at_ms       INTEGER NOT NULL,
    detail_json TEXT NOT NULL,
    PRIMARY KEY (company_id, run_id, step_seq)
);
CREATE INDEX IF NOT EXISTS run_step_details_by_recency
    ON run_step_details (company_id, at_ms);
CREATE TABLE IF NOT EXISTS workflow_revisions (
    company_id    TEXT NOT NULL,
    id            TEXT NOT NULL,
    workflow_id   TEXT NOT NULL,
    revision_json TEXT NOT NULL,
    created_ms    INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE INDEX IF NOT EXISTS workflow_revisions_by_workflow
    ON workflow_revisions (company_id, workflow_id, created_ms);
CREATE TABLE IF NOT EXISTS run_outputs (
    company_id  TEXT NOT NULL,
    run_id      TEXT NOT NULL,
    workflow_id TEXT NOT NULL,
    at_ms       INTEGER NOT NULL,
    output_json TEXT NOT NULL,
    PRIMARY KEY (company_id, run_id)
);
CREATE INDEX IF NOT EXISTS run_outputs_by_recency
    ON run_outputs (company_id, at_ms);
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
CREATE TABLE IF NOT EXISTS channel_read_state (
    company_id   TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    channel_id   TEXT NOT NULL,
    last_read_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, user_id, channel_id)
);
CREATE TABLE IF NOT EXISTS notifications (
    company_id   TEXT NOT NULL,
    id           TEXT NOT NULL,
    kind         TEXT NOT NULL,
    subject_kind TEXT NOT NULL,
    subject_id   TEXT NOT NULL,
    title        TEXT NOT NULL,
    created_ms   INTEGER NOT NULL,
    -- Who the row is for, as a JSON array of user ids. NULL means the whole
    -- company, which is what every row written before this column meant.
    audience     TEXT,
    -- The console channel id the subject lives in, for placing a badge without
    -- the browser having loaded that transcript.
    context      TEXT,
    PRIMARY KEY (company_id, id)
);
-- Backs the documented newest-first feed (`NotificationStore::list`):
-- `WHERE company_id = ? ORDER BY created_ms DESC, id DESC`, so a company's feed
-- reads straight off the index instead of sorting at read time.
CREATE INDEX IF NOT EXISTS notifications_feed
    ON notifications (company_id, created_ms DESC, id DESC);
CREATE TABLE IF NOT EXISTS notification_reads (
    company_id      TEXT NOT NULL,
    user_id         TEXT NOT NULL,
    notification_id TEXT NOT NULL,
    read_ms         INTEGER NOT NULL,
    PRIMARY KEY (company_id, user_id, notification_id)
);
CREATE TABLE IF NOT EXISTS workspace_nodes (
    company_id TEXT NOT NULL,
    id         TEXT NOT NULL,
    node_json  TEXT NOT NULL,
    content    TEXT NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE TABLE IF NOT EXISTS users (
    company_id TEXT NOT NULL,
    id         TEXT NOT NULL,
    email      TEXT NOT NULL,
    user_json  TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE UNIQUE INDEX IF NOT EXISTS users_email
    ON users (company_id, email);
CREATE TABLE IF NOT EXISTS user_invites (
    company_id  TEXT NOT NULL,
    id          TEXT NOT NULL,
    email       TEXT NOT NULL,
    invite_json TEXT NOT NULL,
    created_ms  INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE UNIQUE INDEX IF NOT EXISTS user_invites_email
    ON user_invites (company_id, email);
CREATE TABLE IF NOT EXISTS user_sessions (
    company_id   TEXT NOT NULL,
    id           TEXT NOT NULL,
    token_hash   TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    session_json TEXT NOT NULL,
    created_ms   INTEGER NOT NULL,
    expires_ms   INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE UNIQUE INDEX IF NOT EXISTS user_sessions_token
    ON user_sessions (company_id, token_hash);
CREATE INDEX IF NOT EXISTS user_sessions_user
    ON user_sessions (company_id, user_id);
CREATE TABLE IF NOT EXISTS login_codes (
    company_id TEXT NOT NULL,
    id         TEXT NOT NULL,
    code_hash  TEXT NOT NULL,
    email      TEXT NOT NULL,
    code_json  TEXT NOT NULL,
    expires_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, id)
);
CREATE UNIQUE INDEX IF NOT EXISTS login_codes_hash
    ON login_codes (company_id, code_hash);
CREATE INDEX IF NOT EXISTS login_codes_email
    ON login_codes (company_id, email);
CREATE TABLE IF NOT EXISTS schedule_fires (
    company_id    TEXT NOT NULL,
    schedule_id   TEXT NOT NULL,
    scheduled_for INTEGER NOT NULL,
    claimed_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, schedule_id, scheduled_for)
);
CREATE INDEX IF NOT EXISTS schedule_fires_by_schedule
    ON schedule_fires (company_id, schedule_id, scheduled_for);
CREATE TABLE IF NOT EXISTS journal (
    company_id TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    line       TEXT NOT NULL,
    PRIMARY KEY (company_id, seq)
);
CREATE TABLE IF NOT EXISTS journal_imports (
    company_id TEXT PRIMARY KEY,
    at_ms      INTEGER NOT NULL,
    lines      INTEGER NOT NULL
);
"#;

/// Maps a `rusqlite` failure onto the crate error type without a bare `?` on
/// I/O (which would collide with the existing `#[from] io::Error` mapping).
fn sql_err(e: rusqlite::Error) -> OpenCompanyError {
    OpenCompanyError::Store(format!("sqlite error: {e}"))
}

/// Adds `column` to `table` when it is absent, so an additive schema change
/// reaches databases created before the column existed.
///
/// [`MIGRATIONS`] is a bundle of `CREATE TABLE IF NOT EXISTS` statements, which
/// silently skip an already-created table — new columns would therefore never
/// land on an existing file. `PRAGMA table_info` is the check rather than
/// swallowing the `ALTER`'s "duplicate column name" error, so a genuine `ALTER`
/// failure still surfaces.
/// Decodes a stored audience and applies the shared visibility predicate.
///
/// Invalid JSON preserves the fail-open migration behavior used by the read
/// path: the notification remains company-wide rather than disappearing.
fn audience_admits(audience: Option<&str>, user: &str) -> bool {
    let decoded: Option<Vec<String>> = audience.and_then(|raw| serde_json::from_str(raw).ok());
    // Decode once, then defer to the one shared rule on `Notification` so this
    // cannot drift from `list()` in this file or the other two backends.
    crate::ports::notifications::audience_admits(decoded.as_deref(), user)
}

/// Notification ids in this company that `user` is allowed to see.
fn notification_ids_visible_to(
    conn: &Connection,
    company: &CompanyId,
    user: &str,
) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT id, audience FROM notifications WHERE company_id = ?1")
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![company.as_ref()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, audience) = row.map_err(sql_err)?;
        if audience_admits(audience.as_deref(), user) {
            out.push(id);
        }
    }
    Ok(out)
}

fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let present = {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(sql_err)?;
        // `table_info` column 1 is the column name.
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(sql_err)?;
        let mut found = false;
        for row in rows {
            if row.map_err(sql_err)? == column {
                found = true;
                break;
            }
        }
        found
    };
    if present {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))
        .map_err(sql_err)
}

/// Gives `runs` its `agent_id` mirror column, backfills it, and indexes it
/// (issue #1573).
///
/// Three steps that have to happen in this order and cannot be expressed in
/// [`MIGRATIONS`]:
///
/// 1. **The column.** `CREATE TABLE IF NOT EXISTS` silently skips a table that
///    already exists, so an additive column only reaches an existing database
///    through an `ALTER` — [`add_column_if_missing`].
/// 2. **The backfill.** Every run ever written already carries its desk inside
///    `run_json`; this copies it into the column so a per-teammate read answers
///    with the *whole* history rather than only the attempts written since the
///    upgrade. Without it the new filter would silently under-report, which is
///    worse than not offering it — an empty history reads as "this teammate has
///    never run", not as "this database has not been migrated".
/// 3. **The index.** It cannot sit in [`MIGRATIONS`], because that batch is
///    executed *before* the `ALTER` above and would fail on a column the old
///    table does not have yet.
///
/// Idempotent, and cheap on every open after the first: the `ALTER` is skipped
/// once the column exists, the `UPDATE` matches nothing once no row is `NULL`
/// (and the index makes that a lookup rather than a scan), and re-creating an
/// existing index is a no-op.
fn heal_runs_agent_id(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "runs", "agent_id", "TEXT")?;
    conn.execute(
        "UPDATE runs SET agent_id = json_extract(run_json, '$.agentId') WHERE agent_id IS NULL",
        [],
    )
    .map_err(sql_err)?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS runs_by_agent ON runs (company_id, agent_id)")
        .map_err(sql_err)
}

/// Drops the `NOT NULL` constraint on `runs.task_id` (issue #983).
///
/// A column constraint cannot be altered in place in SQLite, so this is the
/// documented rebuild: create the new shape, copy, drop, rename, recreate the
/// indexes — all inside one transaction, so a database is never left with two
/// half-populated tables. The rows themselves need no rewriting: their
/// `task_id` values are already non-`NULL` strings, and the record's `task_id`
/// is `#[serde(default)]`, so the blob column loads unchanged either way.
///
/// Gated on `PRAGMA table_info`'s `notnull` flag rather than run unconditionally
/// — the rebuild would otherwise fire on every open of every database forever,
/// rewriting the whole run history each time.
fn relax_runs_task_id_nullability(conn: &Connection) -> Result<()> {
    let not_null = {
        let mut stmt = conn.prepare("PRAGMA table_info(runs)").map_err(sql_err)?;
        // `table_info` column 1 is the name and column 3 is the `notnull` flag.
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?)))
            .map_err(sql_err)?;
        let mut flagged = false;
        for row in rows {
            let (name, notnull) = row.map_err(sql_err)?;
            if name == "task_id" && notnull != 0 {
                flagged = true;
                break;
            }
        }
        flagged
    };
    if !not_null {
        return Ok(());
    }
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE runs_rebuilt (
             company_id TEXT NOT NULL,
             id         TEXT NOT NULL,
             task_id    TEXT,
             agent_id   TEXT,
             status     TEXT NOT NULL,
             attempt    INTEGER NOT NULL,
             created_ms INTEGER NOT NULL,
             run_json   TEXT NOT NULL,
             PRIMARY KEY (company_id, id)
         );
         -- `agent_id` is read straight out of the blob rather than copied from
         -- a column, because this rebuild also runs on a database that predates
         -- the column entirely — selecting it there would fail on a name the
         -- old table does not have.
         INSERT INTO runs_rebuilt
             SELECT company_id, id, task_id,
                    json_extract(run_json, '$.agentId'),
                    status, attempt, created_ms, run_json FROM runs;
         DROP TABLE runs;
         ALTER TABLE runs_rebuilt RENAME TO runs;
         CREATE INDEX IF NOT EXISTS runs_by_task ON runs (company_id, task_id);
         CREATE INDEX IF NOT EXISTS runs_by_agent ON runs (company_id, agent_id);
         CREATE INDEX IF NOT EXISTS runs_by_status ON runs (company_id, status);
         COMMIT;",
    )
    .map_err(sql_err)
}

/// Heals `context_chunk_labels` against `context_chunks` at open (issue #1300).
///
/// Two idempotent steps, one transaction, covering every mixed-version
/// history a database can have:
///
/// - **Backfill**: a body row whose (addr, label) has no index row — a
///   database from before the labels table existed, or a row an older binary
///   wrote since — gets one, carrying the body row's stamp. `INSERT OR
///   IGNORE` on the full primary key makes re-running a no-op, and a
///   label-scoped delete can never be undone by it: when the last label goes
///   the body row goes in the same transaction, so there is nothing left to
///   backfill from.
/// - **Orphan sweep**: an index row whose body row is gone — an older
///   binary's address-level delete removed only `context_chunks` — is
///   dropped, so `list` never names a chunk `peek` cannot read.
///
/// Both run on every open rather than behind a version flag, matching the
/// other heals in this file, which read the live schema instead of trusting a
/// stamp. The cost is two anti-joins against a primary-key index, once per
/// process, and neither writes anything on an already-healed database.
fn sync_context_chunk_labels(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN;
         INSERT OR IGNORE INTO context_chunk_labels (company_id, addr, label, stored_ms)
             SELECT company_id, addr, label, stored_ms FROM context_chunks;
         DELETE FROM context_chunk_labels WHERE NOT EXISTS (
             SELECT 1 FROM context_chunks c
             WHERE c.company_id = context_chunk_labels.company_id
               AND c.addr = context_chunk_labels.addr);
         COMMIT;",
    )
    .map_err(sql_err)
}

/// Runs `write` with `synchronous=FULL`, so its commit is fsynced, and restores
/// `NORMAL` afterwards no matter how `write` ended.
///
/// The restore is unconditional on purpose. Leaving `FULL` set would silently
/// buy a flush for every later write on this connection — a permanent cost from
/// a per-record decision — and restoring only on success would leave it set
/// exactly when something has already gone wrong. A failure to restore is
/// reported, but never masks the write's own error: the write's result is what
/// the caller acts on.
fn with_full_sync<T>(conn: &Connection, write: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(sql_err)?;
    let written = write(conn);
    let restored = conn.pragma_update(None, "synchronous", "NORMAL");
    let value = written?;
    restored.map_err(sql_err)?;
    Ok(value)
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

/// Saturates a `u64` byte cap to SQLite's bindable `i64` range, so a cap
/// above `i64::MAX` compares as effectively unlimited instead of wrapping
/// negative.
fn clamp_max_bytes(max_bytes: u64) -> i64 {
    max_bytes.min(i64::MAX as u64) as i64
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
        Self::apply_pragmas(&conn)?;
        conn.execute_batch(MIGRATIONS).map_err(sql_err)?;
        // `CREATE TABLE IF NOT EXISTS` is a no-op on a database that predates a
        // column, so additive columns need their own idempotent step.
        add_column_if_missing(
            &conn,
            "context_chunks",
            "stored_ms",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        // Issue #553: a workspace node may hold bytes. Nullable and with no
        // default, so every existing prose note keeps `blob IS NULL` — which is
        // exactly the "this node is not binary" test the reads use, and needs no
        // backfill.
        add_column_if_missing(&conn, "workspace_nodes", "blob", "BLOB")?;
        // Targeted notifications. Both nullable with no default, so every row
        // written before them keeps `audience IS NULL` — which is exactly the
        // "for the whole company" test the read uses, and needs no backfill.
        add_column_if_missing(&conn, "notifications", "audience", "TEXT")?;
        add_column_if_missing(&conn, "notifications", "context", "TEXT")?;
        // Issue #983: `runs.task_id` was `NOT NULL`, and SQLite has no way to
        // drop a column constraint — so a database created before this needs the
        // twelve-step table rebuild, or the first card-less chat turn fails its
        // insert on a constraint the record no longer has. Idempotent: it reads
        // the live schema and does nothing once the constraint is gone.
        relax_runs_task_id_nullability(&conn)?;
        // The workflow-run join. Nullable with no default, so every existing row
        // keeps `workflow_run_id IS NULL` — which is TRUE for them, not a
        // placeholder: a run minted before workflow nodes recorded anything
        // genuinely belongs to no workflow. No backfill.
        //
        // ORDER IS LOAD-BEARING: it runs AFTER the rebuild above, which recreates
        // `runs` from a fixed column list and would drop this column (and its
        // index) on any database old enough to need that rebuild.
        add_column_if_missing(&conn, "runs", "workflow_run_id", "TEXT")?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS runs_by_workflow_run \
             ON runs (company_id, workflow_run_id);",
        )
        .map_err(sql_err)?;
        // Issue #1573: the `agent_id` mirror column, its backfill and its index.
        // After the rebuild above, which owns the table's shape on the one path
        // that replaces it wholesale.
        heal_runs_agent_id(&conn)?;
        // Issue #1300: the context index moved into `context_chunk_labels`
        // (one row per (addr, label) claim). Runs after the `stored_ms`
        // column heal above, whose column it reads.
        sync_context_chunk_labels(&conn)?;
        Ok(Self {
            conn: Arc::new(StdMutex::new(conn)),
            senders: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    /// Connection settings applied before the first migration statement runs.
    ///
    /// Ordering is load-bearing: `journal_mode` cannot change inside a
    /// transaction, and [`MIGRATIONS`] is an `execute_batch` that opens one. Set
    /// here, not in `open`, so the in-memory constructor takes the identical
    /// path — a divergence between the two would mean the test suite exercises
    /// settings production never gets.
    ///
    /// `journal_mode=WAL` is a no-op on an in-memory database (SQLite answers
    /// `memory` and reports no error), which is why this is safe to share.
    ///
    /// What each one is actually for, given this store holds exactly ONE
    /// `Connection` behind a `StdMutex`:
    ///
    /// - `synchronous=NORMAL` is the real win, and only sound under WAL: it
    ///   drops the per-commit fsync to a per-checkpoint one. A desktop instance
    ///   commits an event per turn step, and `FULL` makes each of those a disk
    ///   flush. Under WAL the failure mode it trades away is losing the last
    ///   commits to an OS/power loss, not corruption.
    /// - `busy_timeout` buys nothing against our own single connection — it is
    ///   the guard for a SECOND one: an operator with the `sqlite3` CLI open
    ///   against the same file, or a second process that got past the home lock.
    ///   Five seconds of blocking beats an immediate `SQLITE_BUSY` surfacing as
    ///   a 500.
    /// - `foreign_keys=ON` is stated rather than assumed. SQLite defaults it
    ///   OFF per-connection, so any future `REFERENCES` clause in [`MIGRATIONS`]
    ///   would be inert decoration, and would stay inert silently.
    fn apply_pragmas(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             PRAGMA foreign_keys=ON;",
        )
        .map_err(sql_err)
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

    /// The shared body of `save` and `save_importing`: upserts the company
    /// row, stamping `overlay_json`'s `activation_gate_seen` with whatever
    /// the caller passes rather than always `true`. See
    /// `CompanyStore::save_importing`'s doc comment for why the two callers
    /// need different values.
    async fn save_gated(&self, record: &CompanyRecord, activation_gate_seen: bool) -> Result<()> {
        let manifest_toml = toml::to_string(&record.manifest)
            .map_err(|e| OpenCompanyError::Store(format!("cannot serialize manifest: {e}")))?;
        let overlay_json = serde_json::to_string(&OverlayBlob::from_record_gated(
            record,
            activation_gate_seen,
        ))?;
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
        // `from_stored_toml` applies the global baseline — see
        // `CompanyManifest::apply_globals`.
        let manifest = CompanyManifest::from_stored_toml(&manifest_toml)
            .map_err(|e| OpenCompanyError::Store(format!("invalid company.toml: {e}")))?;
        let overlay = OverlayBlob::parse(&overlay_json)?;

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

    /// PR #1875 review finding: `overlay_json` is the same blob `save` above
    /// always stamps `activation_gate_seen: true` into (via
    /// `OverlayBlob::from_record`), so reading it back here — rather than
    /// inheriting the trait's always-`false` default — is what lets a
    /// sqlite-backed company's second boot tell itself apart from a genuine
    /// pre-#1843 legacy record. A row that has never been saved at all reads
    /// `false`, matching `FsCompanyStore::activation_gate_seen`'s own "no
    /// bundle written yet" case.
    async fn activation_gate_seen(&self, id: &CompanyId) -> Result<bool> {
        let conn = self.conn();
        let overlay_json: Option<String> = conn
            .query_row(
                "SELECT overlay_json FROM company WHERE company_id = ?1",
                params![id.as_ref()],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(overlay_json) = overlay_json else {
            return Ok(false);
        };
        Ok(OverlayBlob::parse(&overlay_json)?.activation_gate_seen)
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

    async fn read_before(
        &self,
        id: &CompanyId,
        before: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn();
        let (sql, cursor) = match before {
            Some(cursor) => (
                "SELECT seq, event_json, at_ms FROM events WHERE company_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
                Some(cursor.value() as i64),
            ),
            None => (
                "SELECT seq, event_json, at_ms FROM events WHERE company_id = ?1 ORDER BY seq DESC LIMIT ?2",
                None,
            ),
        };
        let mut stmt = conn.prepare(sql).map_err(sql_err)?;
        let mut out = Vec::new();
        if let Some(cursor) = cursor {
            let rows = stmt
                .query_map(params![id.as_ref(), cursor, sql_limit(limit)], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map_err(sql_err)?;
            for row in rows {
                let (seq, event_json, at_ms) = row.map_err(sql_err)?;
                out.push(StoredEvent {
                    seq: EventSeq::new(seq as u64),
                    company: id.clone(),
                    event: serde_json::from_str(&event_json)?,
                    at_millis: at_ms as u64,
                });
            }
        } else {
            let rows = stmt
                .query_map(params![id.as_ref(), sql_limit(limit)], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map_err(sql_err)?;
            for row in rows {
                let (seq, event_json, at_ms) = row.map_err(sql_err)?;
                out.push(StoredEvent {
                    seq: EventSeq::new(seq as u64),
                    company: id.clone(),
                    event: serde_json::from_str(&event_json)?,
                    at_millis: at_ms as u64,
                });
            }
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
        // Whole-log read rather than a `DELETE … WHERE at_ms < ?`: the policy's
        // per-kind bound is a property of the decoded event, and routing every
        // backend through `plan_prune` is what keeps fs, sqlite and mongodb
        // from drifting on the one operation that cannot be undone. Journals
        // are bounded by this very feature, so the read is affordable.
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

        {
            let mut conn = self.conn();
            let tx = conn.transaction().map_err(sql_err)?;
            {
                let mut stmt = tx
                    .prepare("DELETE FROM events WHERE company_id = ?1 AND seq = ?2")
                    .map_err(sql_err)?;
                for seq in &doomed {
                    stmt.execute(params![id.as_ref(), seq.value() as i64])
                        .map_err(sql_err)?;
                }
            }
            tx.commit().map_err(sql_err)?;
        }

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
        // One clock read for both rows: a body row and the claim that lands
        // with it describe the same write and must not disagree by a
        // millisecond the caller can observe through `list`.
        let stored_ms = now_millis() as i64;
        let mut conn = self.conn();
        // Body row first-write-wins per address; the labels index accumulates
        // one row per (addr, label) — set semantics, so a byte-identical body
        // stored under a second label keeps both claims, and a re-put of an
        // identical (body, label) is a no-op (#1300). One transaction, so a
        // body row can never land without its index row.
        let tx = conn.transaction().map_err(sql_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO context_chunks (company_id, addr, label, body, len, stored_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.as_ref(),
                addr,
                chunk.label,
                chunk.body,
                chunk.body.len() as i64,
                stored_ms
            ],
        )
        .map_err(sql_err)?;
        tx.execute(
            "INSERT OR IGNORE INTO context_chunk_labels (company_id, addr, label, stored_ms) \
             VALUES (?1, ?2, ?3, ?4)",
            params![id.as_ref(), addr, chunk.label, stored_ms],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(ChunkAddr::new(addr))
    }

    async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>> {
        let conn = self.conn();
        // The labels table is the index (#1300); the join carries each claim's
        // body length from the one body row. `l.rowid` keeps insertion order,
        // the same order the single-table `rowid` scan used to give.
        let mut stmt = conn
            .prepare(
                "SELECT l.addr, l.label, c.len, l.stored_ms \
                 FROM context_chunk_labels l \
                 JOIN context_chunks c ON c.company_id = l.company_id AND c.addr = l.addr \
                 WHERE l.company_id = ?1 ORDER BY l.rowid",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![id.as_ref()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (addr, label, len, stored_ms) = row.map_err(sql_err)?;
            if label.starts_with(prefix) {
                out.push(ChunkMeta {
                    addr: ChunkAddr::new(addr),
                    label,
                    len: len as usize,
                    stored_at_millis: stored_ms.max(0) as u64,
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
            // Byte offsets from the caller can land mid-codepoint; widen to
            // the boundary rather than panic the slice.
            Some(r) => Ok(slice_on_char_boundaries(&body, r)),
        }
    }

    async fn peek_many(&self, id: &CompanyId, addrs: &[ChunkAddr]) -> Result<Vec<Option<String>>> {
        // One connection acquisition and one prepared statement for the whole
        // batch, instead of the default's lock-per-chunk loop.
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT body FROM context_chunks WHERE company_id = ?1 AND addr = ?2")
            .map_err(sql_err)?;
        let mut bodies = Vec::with_capacity(addrs.len());
        for addr in addrs {
            // Per-addr degrade, per the port contract: a row that fails to
            // read (corrupt decode included) answers `None` like a missing
            // one — only failing to prepare the statement fails the batch.
            let body: Option<String> = match stmt
                .query_row(params![id.as_ref(), addr.as_ref()], |r| r.get(0))
                .optional()
            {
                Ok(body) => body,
                Err(error) => {
                    tracing::warn!(addr = %addr.as_ref(), %error, "context row failed to read");
                    None
                }
            };
            bodies.push(body);
        }
        Ok(bodies)
    }

    async fn delete(&self, id: &CompanyId, addr: &ChunkAddr) -> Result<bool> {
        let mut conn = self.conn();
        // Address-level: the body row and every label claim go together, in
        // one transaction so no half can survive the other.
        let tx = conn.transaction().map_err(sql_err)?;
        let removed_labels = tx
            .execute(
                "DELETE FROM context_chunk_labels WHERE company_id = ?1 AND addr = ?2",
                params![id.as_ref(), addr.as_ref()],
            )
            .map_err(sql_err)?;
        let removed = tx
            .execute(
                "DELETE FROM context_chunks WHERE company_id = ?1 AND addr = ?2",
                params![id.as_ref(), addr.as_ref()],
            )
            .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(removed > 0 || removed_labels > 0)
    }

    async fn delete_label(&self, id: &CompanyId, addr: &ChunkAddr, label: &str) -> Result<bool> {
        let mut conn = self.conn();
        // Label-scoped (#1300): remove exactly one (addr, label) claim, and
        // reap the body row when — and only when — no claim remains. The
        // check and both deletes are one transaction, so a concurrent put of
        // identical content under another label either commits its claim
        // before this transaction (and keeps the body) or after it.
        let tx = conn.transaction().map_err(sql_err)?;
        let removed = tx
            .execute(
                "DELETE FROM context_chunk_labels \
                 WHERE company_id = ?1 AND addr = ?2 AND label = ?3",
                params![id.as_ref(), addr.as_ref(), label],
            )
            .map_err(sql_err)?;
        if removed > 0 {
            let remaining: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM context_chunk_labels \
                     WHERE company_id = ?1 AND addr = ?2",
                    params![id.as_ref(), addr.as_ref()],
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            if remaining == 0 {
                tx.execute(
                    "DELETE FROM context_chunks WHERE company_id = ?1 AND addr = ?2",
                    params![id.as_ref(), addr.as_ref()],
                )
                .map_err(sql_err)?;
            }
        }
        tx.commit().map_err(sql_err)?;
        Ok(removed > 0)
    }

    /// Weighted token overlap rather than `body.find(query)` — see
    /// [`crate::store::lexical`]. The same ranker as fs and mongo, so the
    /// conformance suite can demand one search semantics from all three.
    async fn search(&self, id: &CompanyId, query: &str, limit: usize) -> Result<Vec<ChunkHit>> {
        let mut ranker = crate::store::lexical::Ranker::new(query);
        if ranker.matches_nothing() {
            return Ok(Vec::new());
        }
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT addr, body FROM context_chunks WHERE company_id = ?1 ORDER BY rowid")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![id.as_ref()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(sql_err)?;
        for row in rows {
            let (addr, body) = row.map_err(sql_err)?;
            ranker.offer(&addr, &body);
        }
        Ok(ranker.best(limit))
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
// LedgerStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::ledgers::LedgerStore for SqliteStore {
    async fn list_specs(&self, company: &CompanyId) -> Result<Vec<crate::ledger::LedgerSpec>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT spec_json FROM ledger_specs WHERE company_id = ?1 ORDER BY slug")
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

    async fn put_spec(&self, company: &CompanyId, spec: &crate::ledger::LedgerSpec) -> Result<()> {
        let json = serde_json::to_string(spec)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO ledger_specs (company_id, slug, spec_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(company_id, slug) DO UPDATE SET spec_json = excluded.spec_json",
            params![company.as_ref(), spec.slug, json],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn delete_spec(&self, company: &CompanyId, slug: &str) -> Result<bool> {
        let conn = self.conn();
        // The events stay. See `LedgerStore::delete_spec`.
        let n = conn
            .execute(
                "DELETE FROM ledger_specs WHERE company_id = ?1 AND slug = ?2",
                params![company.as_ref(), slug],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    async fn append(&self, company: &CompanyId, event: &crate::ledger::LedgerEvent) -> Result<()> {
        let json = serde_json::to_string(event)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO ledger_events (company_id, ledger, entry_id, event_json) \
             VALUES (?1, ?2, ?3, ?4)",
            params![company.as_ref(), event.ledger, event.id, json],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn events(
        &self,
        company: &CompanyId,
        ledger: &str,
    ) -> Result<Vec<crate::ledger::LedgerEvent>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT event_json FROM ledger_events WHERE company_id = ?1 AND ledger = ?2 \
                 ORDER BY seq",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref(), ledger], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
        }
        Ok(out)
    }

    async fn purge_entry(&self, company: &CompanyId, ledger: &str, entry: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM ledger_events WHERE company_id = ?1 AND ledger = ?2 AND entry_id = ?3",
                params![company.as_ref(), ledger, entry],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    async fn purge_ledger(&self, company: &CompanyId, ledger: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM ledger_events WHERE company_id = ?1 AND ledger = ?2",
                params![company.as_ref(), ledger],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// UserStore
// ---------------------------------------------------------------------------

/// Whether a `rusqlite` failure is a uniqueness violation.
///
/// The email and token-hash unique indexes are the real enforcement of "one
/// account per address"; this turns the raw sqlite failure into the crate's
/// `409 Conflict` so callers see the same error the fs backend produces.
fn is_unique_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

#[async_trait]
impl crate::ports::users::UserStore for SqliteStore {
    async fn list_users(&self, company: &CompanyId) -> Result<Vec<UserRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT user_json FROM users WHERE company_id = ?1 ORDER BY created_ms DESC")
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

    async fn get_user(&self, company: &CompanyId, id: &str) -> Result<Option<UserRecord>> {
        let conn = self.conn();
        let json: Option<String> = conn
            .query_row(
                "SELECT user_json FROM users WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        json.map(|j| serde_json::from_str(&j).map_err(Into::into))
            .transpose()
    }

    async fn find_user_by_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<UserRecord>> {
        let conn = self.conn();
        // Served by the `users_email` unique index: the login hot path must not
        // scan. Comparison is exact — normalization is the caller's job.
        let json: Option<String> = conn
            .query_row(
                "SELECT user_json FROM users WHERE company_id = ?1 AND email = ?2",
                params![company.as_ref(), email],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        json.map(|j| serde_json::from_str(&j).map_err(Into::into))
            .transpose()
    }

    async fn upsert_user(&self, company: &CompanyId, user: &UserRecord) -> Result<()> {
        let json = serde_json::to_string(user)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO users (company_id, id, email, user_json, created_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(company_id, id) DO UPDATE SET email = excluded.email, \
             user_json = excluded.user_json",
            params![
                company.as_ref(),
                user.id,
                user.email,
                json,
                user.created_at_millis as i64
            ],
        )
        .map_err(|e| {
            if is_unique_violation(&e) {
                OpenCompanyError::Conflict(format!(
                    "another user already has the email {}",
                    user.email
                ))
            } else {
                sql_err(e)
            }
        })?;
        Ok(())
    }

    async fn delete_user(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM users WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    async fn list_invites(&self, company: &CompanyId) -> Result<Vec<InviteRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT invite_json FROM user_invites WHERE company_id = ?1 \
                 ORDER BY created_ms DESC",
            )
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

    async fn find_invite_by_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<InviteRecord>> {
        let conn = self.conn();
        let json: Option<String> = conn
            .query_row(
                "SELECT invite_json FROM user_invites WHERE company_id = ?1 AND email = ?2",
                params![company.as_ref(), email],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        json.map(|j| serde_json::from_str(&j).map_err(Into::into))
            .transpose()
    }

    async fn upsert_invite(&self, company: &CompanyId, invite: &InviteRecord) -> Result<()> {
        let json = serde_json::to_string(invite)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO user_invites (company_id, id, email, invite_json, created_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(company_id, id) DO UPDATE SET email = excluded.email, \
             invite_json = excluded.invite_json",
            params![
                company.as_ref(),
                invite.id,
                invite.email,
                json,
                invite.created_at_millis as i64
            ],
        )
        .map_err(|e| {
            if is_unique_violation(&e) {
                OpenCompanyError::Conflict(format!("{} is already invited", invite.email))
            } else {
                sql_err(e)
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
        let conn = self.conn();
        let current: Option<String> = conn
            .query_row(
                "SELECT invite_json FROM user_invites WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(current) = current else {
            return Ok(false);
        };
        let mut invite: InviteRecord = serde_json::from_str(&current)?;
        invite.notified_at_millis = Some(at_millis);
        // UPDATE, never INSERT, and matched against the row we just read: a
        // revocation that lands between the two statements leaves nothing to
        // match, so the stamp becomes a no-op instead of resurrecting the row.
        let n = conn
            .execute(
                "UPDATE user_invites SET invite_json = ?3 \
                 WHERE company_id = ?1 AND id = ?2 AND invite_json = ?4",
                params![
                    company.as_ref(),
                    id,
                    serde_json::to_string(&invite)?,
                    current
                ],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    async fn delete_invite(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM user_invites WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// SessionStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::sessions::SessionStore for SqliteStore {
    async fn create(&self, company: &CompanyId, session: &SessionRecord) -> Result<()> {
        let json = serde_json::to_string(session)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO user_sessions \
             (company_id, id, token_hash, user_id, session_json, created_ms, expires_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                company.as_ref(),
                session.id,
                session.token_hash,
                session.user_id,
                json,
                session.created_at_millis as i64,
                session.expires_at_millis as i64
            ],
        )
        .map_err(|e| {
            if is_unique_violation(&e) {
                OpenCompanyError::Conflict("that session token already exists".to_string())
            } else {
                sql_err(e)
            }
        })?;
        Ok(())
    }

    async fn find_by_token_hash(
        &self,
        company: &CompanyId,
        token_hash: &str,
    ) -> Result<Option<SessionRecord>> {
        let conn = self.conn();
        // Served by the `user_sessions_token` unique index: this runs on every
        // authenticated request.
        let json: Option<String> = conn
            .query_row(
                "SELECT session_json FROM user_sessions \
                 WHERE company_id = ?1 AND token_hash = ?2",
                params![company.as_ref(), token_hash],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        json.map(|j| serde_json::from_str(&j).map_err(Into::into))
            .transpose()
    }

    async fn list_for_user(
        &self,
        company: &CompanyId,
        user_id: &str,
    ) -> Result<Vec<SessionRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT session_json FROM user_sessions WHERE company_id = ?1 AND user_id = ?2 \
                 ORDER BY created_ms DESC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref(), user_id], |r| {
                r.get::<_, String>(0)
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
        }
        Ok(out)
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM user_sessions WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    async fn delete_for_user(&self, company: &CompanyId, user_id: &str) -> Result<u64> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM user_sessions WHERE company_id = ?1 AND user_id = ?2",
                params![company.as_ref(), user_id],
            )
            .map_err(sql_err)?;
        Ok(n as u64)
    }

    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64> {
        let conn = self.conn();
        // Expiry is exclusive: a session expiring exactly at `now` is dead.
        let n = conn
            .execute(
                "DELETE FROM user_sessions WHERE company_id = ?1 AND expires_ms <= ?2",
                params![company.as_ref(), now_millis as i64],
            )
            .map_err(sql_err)?;
        Ok(n as u64)
    }
}

// ---------------------------------------------------------------------------
// LoginCodeStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::login_codes::LoginCodeStore for SqliteStore {
    async fn create(&self, company: &CompanyId, code: &LoginCodeRecord) -> Result<()> {
        let json = serde_json::to_string(code)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO login_codes \
             (company_id, id, code_hash, email, code_json, expires_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                company.as_ref(),
                code.id,
                code.code_hash,
                code.email,
                json,
                code.expires_at_millis as i64
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn latest_for_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<LoginCodeRecord>> {
        let conn = self.conn();
        // Served by the `login_codes_email` index. `expires_ms` orders by mint
        // time, since every code for one address shares a TTL.
        let json: Option<String> = conn
            .query_row(
                "SELECT code_json FROM login_codes WHERE company_id = ?1 AND email = ?2 \
                 ORDER BY expires_ms DESC LIMIT 1",
                params![company.as_ref(), email],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        json.map(|j| serde_json::from_str(&j).map_err(Into::into))
            .transpose()
    }

    async fn consume(
        &self,
        company: &CompanyId,
        code_hash: &str,
        now_millis: u64,
    ) -> Result<Option<LoginCodeRecord>> {
        // Single-use lives or dies here. The read, the redeemability check, and
        // the mark are one transaction, so two requests racing on the same code
        // cannot both come away with a record — the loser either sees the code
        // already consumed or blocks until the winner commits.
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(sql_err)?;
        let json: Option<String> = tx
            .query_row(
                "SELECT code_json FROM login_codes WHERE company_id = ?1 AND code_hash = ?2",
                params![company.as_ref(), code_hash],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(json) = json else {
            return Ok(None);
        };
        let mut code: LoginCodeRecord = serde_json::from_str(&json)?;
        if !code.is_redeemable(now_millis) {
            return Ok(None);
        }
        code.consumed_at_millis = Some(now_millis);
        tx.execute(
            "UPDATE login_codes SET code_json = ?3 WHERE company_id = ?1 AND code_hash = ?2",
            params![company.as_ref(), code_hash, serde_json::to_string(&code)?],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(Some(code))
    }

    async fn delete_for_email(&self, company: &CompanyId, email: &str) -> Result<u64> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM login_codes WHERE company_id = ?1 AND email = ?2",
                params![company.as_ref(), email],
            )
            .map_err(sql_err)?;
        Ok(n as u64)
    }

    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM login_codes WHERE company_id = ?1 AND expires_ms <= ?2",
                params![company.as_ref(), now_millis as i64],
            )
            .map_err(sql_err)?;
        Ok(n as u64)
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
// ArtifactStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::artifacts::ArtifactStore for SqliteStore {
    async fn list(
        &self,
        company: &CompanyId,
        task_id: Option<&str>,
    ) -> Result<Vec<crate::ports::artifacts::ArtifactRecord>> {
        let conn = self.conn();
        // `task_id` is filtered in SQL (it has an index) rather than in Rust,
        // so a company with many artifacts does not deserialize the whole set
        // to answer one task's Artifacts tab.
        let mut out: Vec<crate::ports::artifacts::ArtifactRecord> = Vec::new();
        match task_id {
            Some(task_id) => {
                let mut stmt = conn
                    .prepare(
                        "SELECT artifact_json FROM artifacts WHERE company_id = ?1 \
                         AND task_id = ?2 ORDER BY updated_ms DESC",
                    )
                    .map_err(sql_err)?;
                let rows = stmt
                    .query_map(params![company.as_ref(), task_id], |r| {
                        r.get::<_, String>(0)
                    })
                    .map_err(sql_err)?;
                for row in rows {
                    out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
                }
            }
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT artifact_json FROM artifacts WHERE company_id = ?1 \
                         ORDER BY updated_ms DESC",
                    )
                    .map_err(sql_err)?;
                let rows = stmt
                    .query_map(params![company.as_ref()], |r| r.get::<_, String>(0))
                    .map_err(sql_err)?;
                for row in rows {
                    out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
                }
            }
        }
        Ok(out)
    }

    async fn get(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<crate::ports::artifacts::ArtifactRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT artifact_json FROM artifacts WHERE company_id = ?1 AND id = ?2")
            .map_err(sql_err)?;
        let mut rows = stmt
            .query_map(params![company.as_ref(), id], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        match rows.next() {
            Some(row) => Ok(Some(serde_json::from_str(&row.map_err(sql_err)?)?)),
            None => Ok(None),
        }
    }

    async fn upsert(
        &self,
        company: &CompanyId,
        artifact: &crate::ports::artifacts::ArtifactRecord,
    ) -> Result<()> {
        let json = serde_json::to_string(artifact)?;
        let conn = self.conn();
        conn.execute(
            "INSERT INTO artifacts (company_id, id, task_id, artifact_json, updated_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(company_id, id) DO UPDATE SET task_id = excluded.task_id, \
             artifact_json = excluded.artifact_json, updated_ms = excluded.updated_ms",
            params![
                company.as_ref(),
                artifact.id,
                artifact.task_id,
                json,
                artifact.updated_at_millis as i64
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM artifacts WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }
}

// ---------------------------------------------------------------------------
// WorkflowRevisionStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::workflow_revisions::WorkflowRevisionStore for SqliteStore {
    async fn push_revision(
        &self,
        company: &CompanyId,
        revision: &crate::ports::workflow_revisions::WorkflowRevisionRecord,
    ) -> Result<()> {
        use crate::ports::workflow_revisions::MAX_WORKFLOW_REVISIONS;
        let json = serde_json::to_string(revision)?;
        let mut guard = self.conn();
        // Insert + prune in ONE transaction, so a reader never observes a
        // 21-deep ring. The prune keeps the newest `MAX` rows for THIS workflow
        // (by `created_ms DESC, id DESC`, matching `sort_newest_first`) and
        // deletes the rest — the same shape OpenHuman's `flow_revisions` prune
        // uses.
        let tx = guard.transaction().map_err(sql_err)?;
        tx.execute(
            "INSERT INTO workflow_revisions (company_id, id, workflow_id, revision_json, created_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                company.as_ref(),
                revision.id,
                revision.workflow_id,
                json,
                revision.created_at_millis as i64,
            ],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM workflow_revisions \
             WHERE company_id = ?1 AND workflow_id = ?2 AND id NOT IN (\
                 SELECT id FROM workflow_revisions \
                 WHERE company_id = ?1 AND workflow_id = ?2 \
                 ORDER BY created_ms DESC, id DESC LIMIT ?3)",
            params![
                company.as_ref(),
                revision.workflow_id,
                MAX_WORKFLOW_REVISIONS as i64,
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    async fn list_revisions(
        &self,
        company: &CompanyId,
        workflow_id: &str,
    ) -> Result<Vec<crate::ports::workflow_revisions::WorkflowRevisionRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT revision_json FROM workflow_revisions \
                 WHERE company_id = ?1 AND workflow_id = ?2 \
                 ORDER BY created_ms DESC, id DESC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref(), workflow_id], |r| {
                r.get::<_, String>(0)
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
        }
        Ok(out)
    }

    async fn get_revision(
        &self,
        company: &CompanyId,
        workflow_id: &str,
        revision_id: &str,
    ) -> Result<Option<crate::ports::workflow_revisions::WorkflowRevisionRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT revision_json FROM workflow_revisions \
                 WHERE company_id = ?1 AND workflow_id = ?2 AND id = ?3",
            )
            .map_err(sql_err)?;
        let mut rows = stmt
            .query_map(params![company.as_ref(), workflow_id, revision_id], |r| {
                r.get::<_, String>(0)
            })
            .map_err(sql_err)?;
        match rows.next() {
            Some(row) => Ok(Some(serde_json::from_str(&row.map_err(sql_err)?)?)),
            None => Ok(None),
        }
    }

    async fn delete_revisions(&self, company: &CompanyId, workflow_id: &str) -> Result<u64> {
        let conn = self.conn();
        let n = conn
            .execute(
                "DELETE FROM workflow_revisions WHERE company_id = ?1 AND workflow_id = ?2",
                params![company.as_ref(), workflow_id],
            )
            .map_err(sql_err)?;
        Ok(n as u64)
    }
}

// ---------------------------------------------------------------------------
// WorkflowRunOutputStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::run_output::WorkflowRunOutputStore for SqliteStore {
    async fn put_run_output(
        &self,
        company: &CompanyId,
        record: &crate::ports::run_output::WorkflowRunOutputRecord,
    ) -> Result<()> {
        use crate::ports::run_output::MAX_RUN_OUTPUTS_PER_COMPANY;
        let json = serde_json::to_string(record)?;
        let mut guard = self.conn();
        // Upsert + prune in ONE transaction, so a reader never observes an
        // over-cap set. `INSERT OR REPLACE` keeps the write last-write-wins per
        // `(company, run_id)`; the prune keeps the newest `MAX` rows for the
        // company (by `at_ms DESC, run_id DESC`, matching `sort_newest_first`).
        let tx = guard.transaction().map_err(sql_err)?;
        tx.execute(
            "INSERT OR REPLACE INTO run_outputs \
             (company_id, run_id, workflow_id, at_ms, output_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                company.as_ref(),
                record.run_id,
                record.workflow_id,
                record.at_millis as i64,
                json,
            ],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM run_outputs \
             WHERE company_id = ?1 AND run_id NOT IN (\
                 SELECT run_id FROM run_outputs \
                 WHERE company_id = ?1 \
                 ORDER BY at_ms DESC, run_id DESC LIMIT ?2)",
            params![company.as_ref(), MAX_RUN_OUTPUTS_PER_COMPANY as i64],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    async fn get_run_output(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Option<crate::ports::run_output::WorkflowRunOutputRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT output_json FROM run_outputs WHERE company_id = ?1 AND run_id = ?2")
            .map_err(sql_err)?;
        let mut rows = stmt
            .query_map(params![company.as_ref(), run_id], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        match rows.next() {
            Some(row) => Ok(Some(serde_json::from_str(&row.map_err(sql_err)?)?)),
            None => Ok(None),
        }
    }
}

#[async_trait]
impl crate::ports::deep_trace::DeepTraceStore for SqliteStore {
    async fn append_step_detail(
        &self,
        company: &CompanyId,
        record: &crate::ports::deep_trace::RunStepDetailRecord,
    ) -> Result<()> {
        use crate::ports::deep_trace::MAX_DEEP_RUNS_PER_COMPANY;
        let json = serde_json::to_string(&record.detail)?;
        let mut guard = self.conn();
        // Upsert + prune in ONE transaction, so a reader never observes an
        // over-cap set. The prune keeps whole RUNS, not the newest rows: ranking
        // rows would leave a surviving run holding a torn half of its own trace,
        // which reads as the agent stopping mid-thought rather than as a prune.
        // A run's recency is the newest `at_ms` any of its rows carries.
        let tx = guard.transaction().map_err(sql_err)?;
        tx.execute(
            "INSERT OR REPLACE INTO run_step_details \
             (company_id, run_id, step_seq, at_ms, detail_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                company.as_ref(),
                record.run_id,
                record.step_seq as i64,
                record.at_millis as i64,
                json,
            ],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM run_step_details \
             WHERE company_id = ?1 AND run_id NOT IN (\
                 SELECT run_id FROM run_step_details \
                 WHERE company_id = ?1 \
                 GROUP BY run_id \
                 ORDER BY MAX(at_ms) DESC, run_id DESC LIMIT ?2)",
            params![company.as_ref(), MAX_DEEP_RUNS_PER_COMPANY as i64],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    async fn list_step_details(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Vec<crate::ports::deep_trace::RunStepDetailRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT step_seq, at_ms, detail_json FROM run_step_details \
                 WHERE company_id = ?1 AND run_id = ?2 ORDER BY step_seq ASC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref(), run_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (step_seq, at_ms, json) = row.map_err(sql_err)?;
            out.push(crate::ports::deep_trace::RunStepDetailRecord {
                run_id: run_id.to_string(),
                step_seq: step_seq as u32,
                at_millis: at_ms as u64,
                detail: serde_json::from_str(&json)?,
            });
        }
        Ok(out)
    }

    async fn purge_deep_trace(&self, company: &CompanyId, run_id: Option<&str>) -> Result<u64> {
        let conn = self.conn();
        let removed = match run_id {
            Some(id) => conn
                .execute(
                    "DELETE FROM run_step_details WHERE company_id = ?1 AND run_id = ?2",
                    params![company.as_ref(), id],
                )
                .map_err(sql_err)?,
            None => conn
                .execute(
                    "DELETE FROM run_step_details WHERE company_id = ?1",
                    params![company.as_ref()],
                )
                .map_err(sql_err)?,
        };
        Ok(removed as u64)
    }
}

// ---------------------------------------------------------------------------
// RunStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::runs::RunStore for SqliteStore {
    async fn create_run(
        &self,
        company: &CompanyId,
        spec: crate::ports::runs::NewRun,
    ) -> Result<crate::ports::runs::RunRecord> {
        use crate::ports::runs::{RunRecord, RunStatus};

        let mut guard = self.conn();
        // Read-max-then-insert inside one transaction, so two concurrent
        // creates on one card cannot mint the same ordinal. This is the
        // "transactional where the backend can do so cheaply" half of the
        // port's `create_run` concurrency contract.
        let tx = guard.transaction().map_err(sql_err)?;
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM runs WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), spec.id],
                |_| Ok(true),
            )
            .optional()
            .map_err(sql_err)?
            .unwrap_or(false);
        if exists {
            return Err(OpenCompanyError::Conflict(format!(
                "run '{}' already exists",
                spec.id
            )));
        }
        // A card-less run (issue #983) is always attempt 1 — see the fs
        // backend's `create_run` for why the ordinal is not shared across them.
        let attempt: i64 = match &spec.task_id {
            Some(task_id) => tx
                .query_row(
                    "SELECT COALESCE(MAX(attempt), 0) + 1 FROM runs \
                     WHERE company_id = ?1 AND task_id = ?2",
                    params![company.as_ref(), task_id],
                    |r| r.get(0),
                )
                .map_err(sql_err)?,
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
        tx.execute(
            "INSERT INTO runs \
             (company_id, id, task_id, workflow_run_id, agent_id, status, attempt, created_ms, run_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                company.as_ref(),
                run.id,
                run.task_id,
                run.workflow_run_id,
                run.agent_id,
                run.status.as_str(),
                run.attempt as i64,
                run.created_at_millis as i64,
                serde_json::to_string(&run)?,
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(run)
    }

    async fn get_run(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<crate::ports::runs::RunRecord>> {
        let conn = self.conn();
        let json: Option<String> = conn
            .query_row(
                "SELECT run_json FROM runs WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match json {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    async fn put_run(
        &self,
        company: &CompanyId,
        run: &crate::ports::runs::RunRecord,
    ) -> Result<()> {
        let json = serde_json::to_string(run)?;
        let conn = self.conn();
        // `status`, `task_id` and `workflow_run_id` are mirrored out of the blob
        // so the indexes can answer a filtered list without deserializing every
        // row.
        conn.execute(
            "INSERT INTO runs \
             (company_id, id, task_id, workflow_run_id, agent_id, status, attempt, created_ms, run_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(company_id, id) DO UPDATE SET task_id = excluded.task_id, \
             workflow_run_id = excluded.workflow_run_id, \
             agent_id = excluded.agent_id, \
             status = excluded.status, attempt = excluded.attempt, \
             created_ms = excluded.created_ms, run_json = excluded.run_json",
            params![
                company.as_ref(),
                run.id,
                run.task_id,
                run.workflow_run_id,
                run.agent_id,
                run.status.as_str(),
                run.attempt as i64,
                run.created_at_millis as i64,
                json,
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn list_runs(
        &self,
        company: &CompanyId,
        filter: &crate::ports::runs::RunFilter,
    ) -> Result<Vec<crate::ports::runs::RunRecord>> {
        // Every predicate is pushed into SQL (all three columns are indexed) so a
        // long-lived company does not deserialize its whole run history to
        // answer one card's Attempts list.
        let mut sql = String::from("SELECT run_json FROM runs WHERE company_id = ?1");
        let mut args: Vec<String> = vec![company.as_ref().to_string()];
        if let Some(task_id) = &filter.task_id {
            args.push(task_id.clone());
            sql.push_str(&format!(" AND task_id = ?{}", args.len()));
        }
        if let Some(workflow_run_id) = &filter.workflow_run_id {
            args.push(workflow_run_id.clone());
            sql.push_str(&format!(" AND workflow_run_id = ?{}", args.len()));
        }
        if let Some(agent_id) = &filter.agent_id {
            args.push(agent_id.clone());
            sql.push_str(&format!(" AND agent_id = ?{}", args.len()));
        }
        if !filter.statuses.is_empty() {
            let placeholders: Vec<String> = filter
                .statuses
                .iter()
                .map(|status| {
                    args.push(status.as_str().to_string());
                    format!("?{}", args.len())
                })
                .collect();
            sql.push_str(&format!(" AND status IN ({})", placeholders.join(", ")));
        }
        // The canonical port ordering, in SQL: see `runs::sort_newest_first`.
        sql.push_str(" ORDER BY created_ms DESC, attempt DESC, id DESC");
        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {}", sql_limit(limit)));
        }
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                r.get::<_, String>(0)
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
        }
        Ok(out)
    }

    async fn append_run_step(
        &self,
        company: &CompanyId,
        step: &crate::ports::runs::RunStepRecord,
    ) -> Result<()> {
        let json = serde_json::to_string(step)?;
        let conn = self.conn();
        // `(run_id, step_seq)` is the primary key, so a replayed append
        // overwrites rather than duplicating — the same idempotence the fs
        // backend gets by folding its JSONL.
        conn.execute(
            "INSERT INTO run_steps (company_id, run_id, step_seq, at_ms, step_json) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(company_id, run_id, step_seq) DO UPDATE SET \
             at_ms = excluded.at_ms, step_json = excluded.step_json",
            params![
                company.as_ref(),
                step.run_id,
                step.step_seq as i64,
                step.at_millis as i64,
                json,
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn list_run_steps(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Vec<crate::ports::runs::RunStepRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT step_json FROM run_steps WHERE company_id = ?1 AND run_id = ?2 \
                 ORDER BY step_seq",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref(), run_id], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// ScheduleFireStore (issue #241)
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::schedule_fires::ScheduleFireStore for SqliteStore {
    async fn claim_fire(
        &self,
        company: &CompanyId,
        schedule_id: &str,
        minute: u64,
    ) -> Result<bool> {
        let conn = self.conn();
        // `INSERT OR IGNORE` against the `(company_id, schedule_id,
        // scheduled_for)` primary key: the row lands for the first caller and is
        // silently skipped for every later one. `changes()` (rows-affected) is
        // therefore exactly "did I win the claim" — 1 = won, 0 = a peer already
        // held it.
        let changed = conn
            .execute(
                "INSERT OR IGNORE INTO schedule_fires \
                 (company_id, schedule_id, scheduled_for, claimed_at_ms) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    company.as_ref(),
                    schedule_id,
                    minute as i64,
                    now_millis() as i64,
                ],
            )
            .map_err(sql_err)?;
        Ok(changed == 1)
    }

    async fn latest_fire(&self, company: &CompanyId, schedule_id: &str) -> Result<Option<u64>> {
        let conn = self.conn();
        // `MAX(scheduled_for)` over one schedule; a company that never fired it
        // returns SQL `NULL`, mapped to `None` — the no-anchor / fresh-install
        // case the schedulers read as "no catch-up".
        let max: Option<i64> = conn
            .query_row(
                "SELECT MAX(scheduled_for) FROM schedule_fires \
                 WHERE company_id = ?1 AND schedule_id = ?2",
                params![company.as_ref(), schedule_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        Ok(max.map(|m| m as u64))
    }

    async fn prune_fires_before(&self, company: &CompanyId, cutoff_minute: u64) -> Result<usize> {
        let conn = self.conn();
        let removed = conn
            .execute(
                "DELETE FROM schedule_fires \
                 WHERE company_id = ?1 AND scheduled_for < ?2",
                params![company.as_ref(), cutoff_minute as i64],
            )
            .map_err(sql_err)?;
        Ok(removed)
    }

    async fn delete_schedule_fires(&self, company: &CompanyId, schedule_id: &str) -> Result<usize> {
        let conn = self.conn();
        // Every row for one schedule, whatever its minute — the delete-time
        // purge (#708). A never-fired id matches nothing, so `changes()` is 0.
        let removed = conn
            .execute(
                "DELETE FROM schedule_fires \
                 WHERE company_id = ?1 AND schedule_id = ?2",
                params![company.as_ref(), schedule_id],
            )
            .map_err(sql_err)?;
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// JournalStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::journal::JournalStore for SqliteStore {
    /// One row per record, ordered by a per-company sequence.
    ///
    /// Sequence and insert are one statement against the `(company_id, seq)`
    /// primary key, so the allocation cannot be observed half-done and a
    /// duplicate seq is a constraint violation rather than a silently
    /// overwritten record — losing a row here would un-commit an at-most-once
    /// key. Same `COALESCE(MAX(..)+1, 0)` shape the event and ledger tables use.
    ///
    /// Both durability levels are honoured (issue #392), because flattening them
    /// here would silently weaken the contract for a company moved onto sqlite:
    ///
    /// * [`Durability::Process`](crate::ports::journal::Durability::Process) is
    ///   the connection's standing `synchronous=NORMAL` under WAL (see
    ///   [`apply_pragmas`](SqliteStore::apply_pragmas)) — the write is in the WAL
    ///   and survives process death, and a power loss can still take the last
    ///   commits. Exactly what the filesystem backend's unflushed append gives.
    /// * [`Durability::Host`](crate::ports::journal::Durability::Host) raises the
    ///   pragma to `FULL` for this one statement, which fsyncs the WAL on commit.
    ///   The pragma is restored **whatever the insert did** — leaving `FULL` set
    ///   would silently fsync every later write on this connection, and leaving
    ///   it set only on the error path would be worse still.
    ///
    /// `synchronous` is settable at any time and takes effect at the next commit;
    /// only `journal_mode` is transaction-bound. The whole sequence runs under the
    /// connection mutex, so no other statement can commit between the raise and
    /// the restore.
    async fn append_journal(
        &self,
        company: &CompanyId,
        line: &str,
        durability: crate::ports::journal::Durability,
    ) -> Result<()> {
        use crate::ports::journal::Durability;

        let conn = self.conn();
        let insert = |conn: &Connection| {
            conn.execute(
                "INSERT INTO journal (company_id, seq, line) VALUES \
                 (?1, COALESCE((SELECT MAX(seq) + 1 FROM journal WHERE company_id = ?1), 0), ?2)",
                params![company.as_ref(), line],
            )
            .map(|_| ())
            .map_err(sql_err)
        };
        match durability {
            Durability::Process => insert(&conn),
            Durability::Host => with_full_sync(&conn, insert),
        }
    }

    async fn read_journal(&self, company: &CompanyId) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT line FROM journal WHERE company_id = ?1 ORDER BY seq ASC")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref()], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(sql_err)?);
        }
        Ok(out)
    }

    async fn journal_imported(&self, company: &CompanyId) -> Result<bool> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT 1 FROM journal_imports WHERE company_id = ?1",
                params![company.as_ref()],
                |_| Ok(true),
            )
            .optional()
            .map_err(sql_err)?
            .unwrap_or(false))
    }

    /// Clear, copy, receipt — in **one transaction**, so the gate and the rows
    /// it guards can never disagree.
    ///
    /// An interrupted import therefore leaves no rows and no receipt, and the
    /// next boot re-runs the whole copy. The alternative — a partial copy behind
    /// a written receipt — is the failure this port exists to prevent: a journal
    /// missing its tail is a set of at-most-once keys that quietly went missing.
    async fn complete_import(&self, company: &CompanyId, lines: Vec<String>) -> Result<()> {
        let mut guard = self.conn();
        // Host-durable, and by the same reasoning `EffectExecuted` is: what is
        // being copied *is* the at-most-once key set, and a migration a power
        // loss can take back has not migrated anything. Raised and restored
        // exactly as `with_full_sync` does it — inline because the transaction
        // needs `&mut Connection` and that helper hands out `&Connection`.
        guard
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_err)?;
        let result = (|| {
            let tx = guard
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_err)?;
            tx.execute(
                "DELETE FROM journal WHERE company_id = ?1",
                params![company.as_ref()],
            )
            .map_err(sql_err)?;
            for (seq, line) in lines.iter().enumerate() {
                tx.execute(
                    "INSERT INTO journal (company_id, seq, line) VALUES (?1, ?2, ?3)",
                    params![company.as_ref(), seq as i64, line],
                )
                .map_err(sql_err)?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO journal_imports (company_id, at_ms, lines) \
                 VALUES (?1, ?2, ?3)",
                params![company.as_ref(), now_millis() as i64, lines.len() as i64],
            )
            .map_err(sql_err)?;
            tx.commit().map_err(sql_err)
        })();
        let restored = guard.pragma_update(None, "synchronous", "NORMAL");
        result?;
        restored.map_err(sql_err)?;
        Ok(())
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
// ReadStateStore
// ---------------------------------------------------------------------------

#[async_trait]
impl crate::ports::read_state::ReadStateStore for SqliteStore {
    async fn list(
        &self,
        company: &CompanyId,
        user: &str,
    ) -> Result<Vec<crate::ports::read_state::ChannelRead>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT channel_id, last_read_ms FROM channel_read_state \
                 WHERE company_id = ?1 AND user_id = ?2 ORDER BY channel_id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref(), user], |r| {
                Ok(crate::ports::read_state::ChannelRead {
                    channel_id: r.get::<_, String>(0)?,
                    last_read_at: r.get::<_, i64>(1)?,
                })
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(sql_err)?);
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
        let conn = self.conn();
        // `max(excluded, existing)` in the conflict arm is the monotonicity the
        // port promises: a late request carrying an earlier instant must not
        // move the marker back and resurrect messages already read.
        conn.execute(
            "INSERT INTO channel_read_state (company_id, user_id, channel_id, last_read_ms) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(company_id, user_id, channel_id) DO UPDATE SET \
             last_read_ms = max(excluded.last_read_ms, channel_read_state.last_read_ms)",
            params![company.as_ref(), user, channel_id, at],
        )
        .map_err(sql_err)?;
        let settled: i64 = conn
            .query_row(
                "SELECT last_read_ms FROM channel_read_state \
                 WHERE company_id = ?1 AND user_id = ?2 AND channel_id = ?3",
                params![company.as_ref(), user, channel_id],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
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
impl crate::ports::notifications::NotificationStore for SqliteStore {
    async fn append(
        &self,
        company: &CompanyId,
        notification: &crate::ports::notifications::Notification,
    ) -> Result<()> {
        let conn = self.conn();
        // `INSERT OR IGNORE` makes append idempotent by id (first write wins): a
        // retried or replayed append is a no-op on the primary key rather than an
        // error, matching the fs and mongo backends.
        conn.execute(
            "INSERT OR IGNORE INTO notifications \
                 (company_id, id, kind, subject_kind, subject_id, title, created_ms, \
                  audience, context) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                company.as_ref(),
                notification.id.as_str(),
                notification.kind.as_str(),
                notification.subject.kind.as_str(),
                notification.subject.id.as_str(),
                notification.title.as_str(),
                notification.created_at as i64,
                notification
                    .audience
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| crate::error::OpenCompanyError::Store(format!(
                        "notification audience is not serializable: {e}"
                    )))?,
                notification.context.as_deref(),
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    async fn list(
        &self,
        company: &CompanyId,
        user: &str,
    ) -> Result<Vec<crate::ports::notifications::NotificationView>> {
        let conn = self.conn();
        // Read state is projected per person with a LEFT JOIN: a notification
        // with no marker of this user's comes back with a NULL `read_ms`.
        let mut stmt = conn
            .prepare(
                "SELECT n.id, n.kind, n.subject_kind, n.subject_id, n.title, n.created_ms, \
                        r.read_ms, n.audience, n.context \
                 FROM notifications n \
                 LEFT JOIN notification_reads r \
                     ON r.company_id = n.company_id \
                    AND r.notification_id = n.id \
                    AND r.user_id = ?2 \
                 WHERE n.company_id = ?1 \
                 ORDER BY n.created_ms DESC, n.id DESC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref(), user], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, kind, subject_kind, subject_id, title, created_ms, read_ms, audience, context) =
                row.map_err(sql_err)?;
            let subject_kind = crate::ports::notifications::SubjectKind::from_token(&subject_kind)
                .ok_or_else(|| {
                    crate::error::OpenCompanyError::Store(format!(
                        "notification {id} has an unknown subject kind {subject_kind:?}"
                    ))
                })?;
            out.push(crate::ports::notifications::NotificationView {
                notification: crate::ports::notifications::Notification {
                    id,
                    kind,
                    subject: crate::ports::notifications::Subject {
                        kind: subject_kind,
                        id: subject_id,
                    },
                    created_at: created_ms as u64,
                    title,
                    // A row whose audience will not parse is treated as
                    // company-wide rather than dropped: losing a notification
                    // is worse than showing one person one extra line, and a
                    // corrupt column is a bug to see, not to hide.
                    audience: audience
                        .as_deref()
                        .and_then(|a| serde_json::from_str(a).ok()),
                    context,
                },
                read_at: read_ms.map(|v| v as u64),
            });
        }
        // Filtered here rather than in SQL: SQLite's JSON support is a
        // compile-time option this crate does not require, and a company's feed
        // is small enough that the index-ordered scan plus a predicate is
        // cheaper than depending on it. The rule itself lives on
        // `Notification::visible_to`, so all three backends read it from one
        // place.
        out.retain(|view| view.notification.visible_to(user));
        Ok(out)
    }

    async fn mark_read(
        &self,
        company: &CompanyId,
        user: &str,
        ids: Option<&[String]>,
    ) -> Result<u64> {
        let conn = self.conn();
        let now = crate::ports::now_millis() as i64;
        // `INSERT OR IGNORE` is the latch: a PK conflict on an already-read row
        // is skipped, so the original `read_ms` survives a re-mark. Only ids
        // that name an existing notification in this company are inserted.
        match ids {
            Some(ids) => {
                for id in ids {
                    conn.execute(
                        "INSERT OR IGNORE INTO notification_reads \
                             (company_id, user_id, notification_id, read_ms) \
                         SELECT ?1, ?2, ?3, ?4 \
                         WHERE EXISTS \
                             (SELECT 1 FROM notifications WHERE company_id = ?1 AND id = ?3)",
                        params![company.as_ref(), user, id.as_str(), now],
                    )
                    .map_err(sql_err)?;
                }
            }
            None => {
                // Only what this person can actually see. Marking a colleague's
                // targeted row read is inert, but writing markers for rows they
                // will never be shown makes "mark all read" mean something
                // different per backend, which is exactly what the conformance
                // suite exists to prevent.
                for id in notification_ids_visible_to(&conn, company, user)? {
                    conn.execute(
                        "INSERT OR IGNORE INTO notification_reads \
                             (company_id, user_id, notification_id, read_ms) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![company.as_ref(), user, id.as_str(), now],
                    )
                    .map_err(sql_err)?;
                }
            }
        }
        // Counted in Rust rather than in SQL: the audience is a JSON array, and
        // reading it in SQLite needs the `json1` extension, which this crate
        // does not require of its host. A company's feed is small, and the
        // predicate lives on `Notification::visible_to` so all three backends
        // agree by construction rather than by three careful queries.
        let mut stmt = conn
            .prepare(
                "SELECT n.audience FROM notifications n \
                 WHERE n.company_id = ?1 \
                   AND NOT EXISTS \
                       (SELECT 1 FROM notification_reads r \
                        WHERE r.company_id = n.company_id \
                          AND r.notification_id = n.id \
                          AND r.user_id = ?2)",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![company.as_ref(), user], |r| {
                r.get::<_, Option<String>>(0)
            })
            .map_err(sql_err)?;
        let mut unread = 0u64;
        for row in rows {
            let audience = row.map_err(sql_err)?;
            let decoded: Option<Vec<String>> = audience
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok());
            if crate::ports::notifications::audience_admits(decoded.as_deref(), user) {
                unread += 1;
            }
        }
        Ok(unread)
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
            Some((node_json, content)) => {
                let node: crate::ports::workspace::WorkspaceNode =
                    serde_json::from_str(&node_json)?;
                // A binary node reads as an empty body, like a folder — the port
                // contract that keeps every prose-shaped caller correct without
                // teaching it that bytes exist.
                let content = if node.is_binary() {
                    String::new()
                } else {
                    content
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
        let conn = self.conn();
        // One statement, so the length and the body it admits describe the same
        // row: the cap is applied by SQLite, and an over-cap body is never
        // carried across the boundary into a Rust `String`.
        let row: Option<(String, i64, String)> = conn
            .query_row(
                "SELECT node_json, \
                        length(CAST(content AS BLOB)), \
                        CASE WHEN length(CAST(content AS BLOB)) <= ?3 THEN content ELSE '' END \
                 FROM workspace_nodes WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id, clamp_max_bytes(max_bytes)],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_err)?;
        match row {
            Some((node_json, len, content)) => {
                let node: crate::ports::workspace::WorkspaceNode =
                    serde_json::from_str(&node_json)?;
                if node.kind != crate::ports::workspace::NodeKind::File || node.is_binary() {
                    return Ok(Some((node, String::new(), 0)));
                }
                Ok(Some((node, content, len.max(0) as u64)))
            }
            None => Ok(None),
        }
    }

    async fn write(
        &self,
        company: &CompanyId,
        id: &str,
        content: &str,
        author: crate::ports::workspace::WorkspaceOrigin,
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
        if let Some(mime) = &node.mime {
            return Err(OpenCompanyError::InvalidRequest(
                crate::ports::workspace::binary_write_refusal(&node.name, mime),
            ));
        }
        node.updated_at_millis = now_millis();
        // Authorship rides the same stamp as the timestamp. The node is stored
        // as opaque JSON, so this needs no column and no migration.
        node.updated_by = author;
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
        use crate::ports::workspace::{NodeKind, duplicate_file_refusal, file_name_taken};
        let mut conn = self.conn();
        // Issue #894: the write reservation is taken BEFORE the read, exactly as
        // `adopt_or_create_folder` and `swap_files` already do on this backend.
        //
        // The `StdMutex` around the connection makes this function atomic within
        // one `SqliteStore`, and that is not the race: two `SqliteStore`
        // instances can point at one database file, and against a second
        // instance a plain read-then-`INSERT` has both callers see the name free
        // and both insert. `IMMEDIATE` is what makes the loser wait for the
        // winner's commit and then observe it — `busy_timeout` (see
        // `apply_pragmas`) turns the contention into a bounded block rather than
        // an immediate `SQLITE_BUSY`.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let nodes = self.workspace_nodes(&tx, company)?;
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
        // The sibling-name guard this backend never had. Files only — a folder
        // sharing a name with a file stays legal, and folder-vs-folder is
        // `adopt_or_create_folder`'s to decide.
        if node.kind == NodeKind::File
            && file_name_taken(nodes.values(), node.parent_id.as_deref(), &node.name)
        {
            return Err(OpenCompanyError::Conflict(duplicate_file_refusal(
                &node.name,
            )));
        }
        tx.execute(
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
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    /// Read-siblings then adopt-or-`INSERT`, inside an **immediate**
    /// transaction (issue #759).
    ///
    /// The same device this backend's [`swap_files`] already uses, and for the
    /// same reason: two `SqliteStore` instances can point at one database file,
    /// so the write reservation has to be taken *before* the read or both
    /// callers see the name free and both insert. Nothing else here would stop
    /// them — plain [`create`] checks only that the node **id** is fresh and has
    /// never had a sibling-name guard at all.
    ///
    /// [`swap_files`]: crate::ports::workspace::WorkspaceStore::swap_files
    /// [`create`]: crate::ports::workspace::WorkspaceStore::create
    async fn adopt_or_create_folder(
        &self,
        company: &CompanyId,
        parent: Option<&str>,
        name: &str,
        origin: crate::ports::workspace::WorkspaceOrigin,
    ) -> Result<crate::ports::workspace::FolderClaim> {
        use crate::ports::workspace::{FolderClaim, NodeKind, existing_folder_claim, new_folder};
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let nodes = self.workspace_nodes(&tx, company)?;
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
        // Adoption stamps the lease flag (issue #1839) and commits it. The write
        // rides `node_json` via serde, inside the immediate transaction this
        // method already holds, so the flag lands atomically against a
        // concurrent `delete_if_empty` and Race 1 is closed on this backend.
        // Authorship is untouched — only `adopted` changes.
        if let Some(mut existing) = existing_folder_claim(nodes.values(), parent, name)? {
            if !existing.adopted {
                existing.adopted = true;
                tx.execute(
                    "UPDATE workspace_nodes SET node_json = ?3 \
                     WHERE company_id = ?1 AND id = ?2",
                    params![
                        company.as_ref(),
                        existing.id,
                        serde_json::to_string(&existing)?
                    ],
                )
                .map_err(sql_err)?;
                tx.commit().map_err(sql_err)?;
            }
            return Ok(FolderClaim::Adopted(existing));
        }
        let node = new_folder(name, parent, origin);
        tx.execute(
            "INSERT INTO workspace_nodes (company_id, id, node_json, content, updated_ms) \
             VALUES (?1, ?2, ?3, '', ?4)",
            params![
                company.as_ref(),
                node.id,
                serde_json::to_string(&node)?,
                node.updated_at_millis as i64
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(FolderClaim::Created(node))
    }

    /// One `INSERT` carrying the node and its payload together.
    ///
    /// **sqlite cannot orphan a blob**, and that is a property of this statement
    /// rather than of care taken around it: the bytes live in the same row as
    /// the node, so there is no ordering between two writes to get wrong and no
    /// crash window to sweep afterwards. The other two backends have to work for
    /// what this gets for free.
    async fn create_binary(
        &self,
        company: &CompanyId,
        node: &crate::ports::workspace::WorkspaceNode,
        bytes: &[u8],
    ) -> Result<crate::ports::workspace::WorkspaceNode> {
        use crate::ports::workspace::{NodeKind, duplicate_file_refusal, file_name_taken};
        let node = crate::ports::workspace::stamped_binary(node, bytes)?;
        let mut conn = self.conn();
        // Issue #894, the same reservation-before-read as `create`. A binary
        // node is a file, so it contends for a name exactly like a note does —
        // MongoDB's `create_binary` stamps the same path key for this reason.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let nodes = self.workspace_nodes(&tx, company)?;
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
        if node.kind == NodeKind::File
            && file_name_taken(nodes.values(), node.parent_id.as_deref(), &node.name)
        {
            return Err(OpenCompanyError::Conflict(duplicate_file_refusal(
                &node.name,
            )));
        }
        tx.execute(
            "INSERT INTO workspace_nodes (company_id, id, node_json, content, updated_ms, blob) \
             VALUES (?1, ?2, ?3, '', ?4, ?5)",
            params![
                company.as_ref(),
                node.id,
                serde_json::to_string(&node)?,
                node.updated_at_millis as i64,
                bytes
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
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
        crate::ports::workspace::rebind_binary(&mut node, bytes, mime, author)?;
        conn.execute(
            "UPDATE workspace_nodes SET node_json = ?1, blob = ?2, updated_ms = ?3 \
             WHERE company_id = ?4 AND id = ?5",
            params![
                serde_json::to_string(&node)?,
                bytes,
                node.updated_at_millis as i64,
                company.as_ref(),
                id
            ],
        )
        .map_err(sql_err)?;
        Ok(node)
    }

    /// Buffers the row's blob once, under the connection mutex, and yields it as
    /// a single chunk.
    ///
    /// The one backend that cannot genuinely stream: the payload is a `BLOB` in
    /// a row behind a `StdMutex<Connection>`, and holding that lock across an
    /// `await` while a client drains a slow download would stall every other
    /// query in the process. So the bytes are resident for the length of the
    /// read, bounded by the per-file cap the quota decorator enforces — stated
    /// here rather than left to be discovered by whoever raises that cap.
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
        let conn = self.conn();
        let row: Option<(String, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT node_json, blob FROM workspace_nodes WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        drop(conn);
        let Some((node_json, Some(blob))) = row else {
            return Ok(None);
        };
        let node: crate::ports::workspace::WorkspaceNode = serde_json::from_str(&node_json)?;
        if !node.is_binary() {
            return Ok(None);
        }
        Ok(Some((node, crate::ports::workspace::one_chunk(blob))))
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

    async fn swap_files(
        &self,
        company: &CompanyId,
        expected_id: Option<&str>,
        replacement_id: &str,
        name: &str,
    ) -> Result<Option<crate::ports::workspace::WorkspaceNode>> {
        use crate::ports::workspace::NodeKind;

        let mut conn = self.conn();
        // Acquire the write reservation before reading either row. Two
        // SqliteStore instances can point at the same database; an immediate
        // transaction serializes their compare-and-swap decisions rather than
        // letting both read the same expected node first.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let nodes = self.workspace_nodes(&tx, company)?;
        let Some(replacement) = nodes.get(replacement_id).cloned() else {
            return Err(OpenCompanyError::CompanyNotFound(format!(
                "workspace node {replacement_id}"
            )));
        };
        if replacement.kind != NodeKind::File {
            return Err(OpenCompanyError::InvalidRequest(
                "only files can be promoted from a staging path".to_string(),
            ));
        }

        // Both readings of `expected_id`, decided inside the immediate
        // transaction opened above so two writers cannot both see the name free.
        let still_current = match expected_id {
            Some(id) => nodes.get(id).is_some_and(|node| {
                node.kind == NodeKind::File
                    && node.name == name
                    && node.parent_id == replacement.parent_id
            }),
            // Issue #697: `None` asserts the name is unoccupied. Any node
            // already at it loses this caller the compare-and-swap; the staged
            // node itself is excluded, since it is what is being installed.
            None => !nodes.values().any(|node| {
                node.id != replacement.id
                    && node.name == name
                    && node.parent_id == replacement.parent_id
            }),
        };
        if !still_current {
            tx.execute(
                "DELETE FROM workspace_nodes WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), replacement_id],
            )
            .map_err(sql_err)?;
            tx.commit().map_err(sql_err)?;
            return Ok(None);
        }

        let mut promoted = replacement;
        promoted.name = name.to_string();
        promoted.updated_at_millis = now_millis();
        // Nothing to retire on a first publish: the name was free, which is
        // exactly what the guard above established.
        if let Some(id) = expected_id {
            tx.execute(
                "DELETE FROM workspace_nodes WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
            )
            .map_err(sql_err)?;
        }
        tx.execute(
            "UPDATE workspace_nodes SET node_json = ?1, updated_ms = ?2 \
             WHERE company_id = ?3 AND id = ?4",
            params![
                serde_json::to_string(&promoted)?,
                promoted.updated_at_millis as i64,
                company.as_ref(),
                replacement_id
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(Some(promoted))
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

    async fn delete_if_empty(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        // `parent_id` and `adopted` live inside `node_json`, not as their own
        // columns (see the `workspace_nodes` table def), so both the existence
        // check and the has-child check go through the same deserializing
        // helper the rest of this backend's workspace methods use — a raw SQL
        // `WHERE parent_id = ?` against this table is a "no such column" error,
        // not a narrowing filter.
        let nodes = self.workspace_nodes(&tx, company)?;
        let Some(node) = nodes.get(id) else {
            return Ok(false);
        };
        // An adopted folder has a second writer whose create has not landed yet
        // (issue #1839). The flag-read here and the flag-write in
        // `adopt_or_create_folder` both happen under an IMMEDIATE transaction on
        // this same table, so they serialize: once an adoption has stamped
        // `adopted`, this refuses — closing the race on this backend rather than
        // narrowing it, matching `FsOps`'s guard.
        if node.adopted {
            return Ok(false);
        }
        if nodes.values().any(|n| n.parent_id.as_deref() == Some(id)) {
            return Ok(false);
        }
        // Single document, non-recursive — the reads above, taken under this
        // same IMMEDIATE transaction, already proved `id` exists, is unadopted
        // and is childless, so a plain delete-by-key is enough.
        let removed = tx
            .execute(
                "DELETE FROM workspace_nodes WHERE company_id = ?1 AND id = ?2",
                params![company.as_ref(), id],
            )
            .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(removed > 0)
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

    /// [`SqliteStore::apply_pragmas`] settles for the connection the ports use.
    ///
    /// Asserted against a FILE-backed database on purpose. Every other test here
    /// runs in memory, where `journal_mode=WAL` legitimately answers `memory` —
    /// so an in-memory assertion could not distinguish "WAL was applied" from
    /// "the pragma was never issued", which is the regression worth catching.
    /// `synchronous=NORMAL` is only sound under WAL, so the two are checked
    /// together rather than separately.
    #[tokio::test]
    async fn file_backed_store_runs_in_wal_with_relaxed_sync() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SqliteStore::open(dir.path().join("opencompany.db")).expect("open sqlite file");

        let conn = store.conn();
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read journal_mode");
        // SQLite reports the mode lowercased regardless of how it was set.
        assert_eq!(journal.to_ascii_lowercase(), "wal");

        // 1 == NORMAL. The numeric form is what `PRAGMA synchronous` reads back;
        // there is no symbolic getter.
        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("read synchronous");
        assert_eq!(synchronous, 1, "expected synchronous=NORMAL");

        let foreign_keys: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("read foreign_keys");
        assert_eq!(foreign_keys, 1, "expected foreign_keys=ON");
    }

    #[tokio::test]
    async fn conformance_isolation_by_company() {
        let s = store();
        conformance::assert_isolation_by_company(s.clone(), s.clone(), s.clone(), s).await;
    }

    #[tokio::test]
    async fn conformance_paused_ordinary_save_preserves_activation_gate() {
        let s = store();
        conformance::assert_paused_ordinary_save_preserves_activation_gate(s).await;
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
    async fn conformance_event_subscription_surfaces_gap() {
        conformance::assert_event_subscription_surfaces_gap(store()).await;
    }

    #[tokio::test]
    async fn conformance_event_read_before() {
        conformance::assert_event_read_before(store()).await;
    }

    #[tokio::test]
    async fn conformance_event_retention() {
        let s = store();
        conformance::assert_event_retention(s).await;
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

    /// Issue #1505. The port holds this company's inference credential, its MCP
    /// OAuth tokens and its SMTP password, and had no conformance case on any
    /// backend until this one.
    #[tokio::test]
    async fn conformance_secret_store() {
        conformance::assert_secret_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_task_store() {
        conformance::assert_task_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_user_store() {
        conformance::assert_user_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_session_store() {
        conformance::assert_session_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_login_code_store() {
        conformance::assert_login_code_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_fact_store() {
        conformance::assert_fact_store(store()).await;
        conformance::assert_artifact_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_workflow_revision_store() {
        conformance::assert_workflow_revision_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_workflow_run_output_store() {
        conformance::assert_workflow_run_output_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_context_chunk_stamps() {
        conformance::assert_context_chunk_stamps(store()).await;
    }

    // Exercises this backend's single-connection `peek_many` override against
    // the same positional contract the default implementation gives.
    #[tokio::test]
    async fn conformance_context_peek_many() {
        conformance::assert_context_peek_many_answers_positionally(store()).await;
    }

    #[tokio::test]
    async fn conformance_context_multibyte_bodies() {
        conformance::assert_multibyte_bodies_survive_search_and_ranged_peek(store()).await;
    }

    #[tokio::test]
    async fn conformance_context_identical_body_two_labels() {
        conformance::assert_identical_body_two_labels(store()).await;
    }

    #[tokio::test]
    async fn conformance_context_delete_label_scoped() {
        conformance::assert_delete_label_scoped(store()).await;
    }

    #[tokio::test]
    async fn conformance_context_delete_label_survives_a_concurrent_identical_put() {
        conformance::assert_delete_label_survives_a_concurrent_identical_put(store()).await;
    }

    /// The same search semantics as every other backend.
    #[tokio::test]
    async fn conformance_context_search_ranking() {
        conformance::assert_context_search_ranking(store()).await;
    }

    /// The migration path a fresh database never exercises.
    ///
    /// [`MIGRATIONS`] is all `CREATE TABLE IF NOT EXISTS`, which is a no-op
    /// against a database that already has `context_chunks` — so on an existing
    /// deployment only [`add_column_if_missing`] can add `stored_ms`. Without
    /// it, every read of the column would fail with "no such column" and the
    /// whole Brain list/stats surface would 500.
    #[tokio::test]
    async fn legacy_context_chunks_table_gains_stored_ms_on_open() {
        // A database as it looked before the column existed, with a row in it.
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE context_chunks (
                 company_id TEXT NOT NULL,
                 addr       TEXT NOT NULL,
                 label      TEXT NOT NULL,
                 body       TEXT NOT NULL,
                 len        INTEGER NOT NULL,
                 PRIMARY KEY (company_id, addr)
             );
             INSERT INTO context_chunks (company_id, addr, label, body, len)
             VALUES ('acme', 'legacy-addr', 'agent/ceo', 'remembered before stamps', 24);",
        )
        .expect("seed a pre-`stored_ms` database");

        let store = SqliteStore::from_conn(conn).expect("migrations run on a legacy database");
        let id = CompanyId::new("acme");

        // The legacy row survives the migration and reports an *unknown* store
        // time — not the epoch, and not a misleading "migrated just now".
        // Fully qualified: `SqliteStore` implements both `ContextStore::list`
        // and `CompanyStore::list`, and method resolution matches on the name
        // alone — arity does not disambiguate, so the bare call is `E0034`.
        let metas = ContextStore::list(&store, &id, "")
            .await
            .expect("list after migration");
        assert_eq!(metas.len(), 1, "the legacy row must not be dropped");
        assert_eq!(metas[0].label, "agent/ceo");
        assert_eq!(
            metas[0].stored_at_millis, 0,
            "a row written before stamps existed has no store time to report"
        );

        // A write after the migration is stamped for real.
        let before = now_millis();
        store
            .put(
                &id,
                ContextChunk {
                    label: "agent/ops".to_string(),
                    body: "remembered after the migration".to_string(),
                },
            )
            .await
            .expect("put into a migrated database");

        let metas = ContextStore::list(&store, &id, "")
            .await
            .expect("list after put");
        assert_eq!(metas.len(), 2);
        let fresh = metas
            .iter()
            .find(|m| m.label == "agent/ops")
            .expect("the new chunk is listed");
        assert!(
            fresh.stored_at_millis >= before,
            "a post-migration write must carry a real stamp, got {}",
            fresh.stored_at_millis
        );

        // Reopening an already-migrated database must not try to add the column
        // a second time — the `ALTER` would fail with "duplicate column name".
        add_column_if_missing(
            &store.conn(),
            "context_chunks",
            "stored_ms",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .expect("adding an existing column is a no-op, not an error");
    }

    /// Issue #1300's open-time heals on a mixed-version database: the
    /// backfill gives a row an older binary wrote its label claim (so the
    /// label-scoped delete can reach it), and the orphan sweep drops an index
    /// row whose body row an older binary's address-level delete removed.
    #[tokio::test]
    async fn legacy_context_rows_gain_label_claims_and_orphans_are_swept_on_open() {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE context_chunks (
                 company_id TEXT NOT NULL,
                 addr       TEXT NOT NULL,
                 label      TEXT NOT NULL,
                 body       TEXT NOT NULL,
                 len        INTEGER NOT NULL,
                 stored_ms  INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (company_id, addr)
             );
             CREATE TABLE context_chunk_labels (
                 company_id TEXT NOT NULL,
                 addr       TEXT NOT NULL,
                 label      TEXT NOT NULL,
                 stored_ms  INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (company_id, addr, label)
             );
             -- A row an older binary wrote after the labels table existed: the
             -- body row is there, its claim is not.
             INSERT INTO context_chunks (company_id, addr, label, body, len, stored_ms)
             VALUES ('acme', 'old-binary-addr', 'agent/ceo', 'written by an old binary', 24, 7);
             -- And the reverse: a claim whose body row an older binary's
             -- address-level delete removed.
             INSERT INTO context_chunk_labels (company_id, addr, label, stored_ms)
             VALUES ('acme', 'reaped-addr', 'agent/ops', 9);",
        )
        .expect("seed a mixed-version database");

        let store = SqliteStore::from_conn(conn).expect("migrations run");
        let id = CompanyId::new("acme");

        let metas = ContextStore::list(&store, &id, "")
            .await
            .expect("list after the heals");
        assert_eq!(
            metas.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            ["agent/ceo"],
            "the old binary's row gains its claim; the orphaned claim is swept"
        );
        assert_eq!(
            metas[0].stored_at_millis, 7,
            "the backfilled claim carries the body row's stamp"
        );

        // The backfilled claim is label-deletable, and takes the body with it
        // as the last claim.
        let addr = ChunkAddr::new("old-binary-addr".to_string());
        assert!(
            store
                .delete_label(&id, &addr, "agent/ceo")
                .await
                .expect("label-scoped delete on a backfilled claim")
        );
        assert!(
            ContextStore::list(&store, &id, "")
                .await
                .unwrap()
                .is_empty(),
            "no claim may remain"
        );
        assert!(
            store.peek(&id, &addr, None).await.is_err(),
            "the body row went with its last claim"
        );
    }

    /// Issue #983: a database created while `runs.task_id` was `NOT NULL` must
    /// end up able to hold a card-less run.
    ///
    /// `MIGRATIONS` is all `CREATE TABLE IF NOT EXISTS`, so the relaxed DDL is a
    /// no-op against an existing deployment — and SQLite cannot drop a column
    /// constraint in place. Without the rebuild the *first chat turn* on any
    /// upgraded self-hosted install fails its insert, which is a failure mode
    /// that appears only in production and never in a test that starts fresh.
    #[tokio::test]
    async fn a_legacy_runs_table_learns_to_hold_a_card_less_run() {
        use crate::ports::runs::{NewRun, RunStore};

        // The table exactly as it shipped before #983, with an attempt in it.
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE runs (
                 company_id TEXT NOT NULL,
                 id         TEXT NOT NULL,
                 task_id    TEXT NOT NULL,
                 status     TEXT NOT NULL,
                 attempt    INTEGER NOT NULL,
                 created_ms INTEGER NOT NULL,
                 run_json   TEXT NOT NULL,
                 PRIMARY KEY (company_id, id)
             );
             INSERT INTO runs (company_id, id, task_id, status, attempt, created_ms, run_json)
             VALUES ('acme', 'old-run', 'card-7', 'succeeded', 1, 1700000000000,
                     '{\"id\":\"old-run\",\"company\":\"acme\",\"taskId\":\"card-7\",\
                       \"agentId\":\"ceo\",\"attempt\":1,\"status\":\"succeeded\",\
                       \"createdAtMillis\":1700000000000}');",
        )
        .expect("seed a pre-#983 database");

        let store = SqliteStore::from_conn(conn).expect("migrations run on a legacy database");
        let id = CompanyId::new("acme");

        // The rebuild copied the history rather than dropping it.
        let old = store
            .get_run(&id, "old-run")
            .await
            .expect("read after the rebuild")
            .expect("the legacy attempt survived");
        assert_eq!(old.task_id.as_deref(), Some("card-7"));
        assert_eq!(old.attempt, 1);

        // …and the constraint is gone, so a chat turn can be recorded at all.
        let turn = store
            .create_run(&id, NewRun::for_chat("turn-1", "general", "general"))
            .await
            .expect("a card-less run must be insertable after the rebuild");
        assert_eq!(turn.task_id, None);
        assert_eq!(
            store.get_run(&id, "turn-1").await.unwrap().as_ref(),
            Some(&turn)
        );

        // The per-card filter still works over the rebuilt indexes, and the
        // card-less row does not answer it.
        let for_card = store
            .list_runs(&id, &crate::ports::runs::RunFilter::for_task("card-7"))
            .await
            .expect("list by card");
        assert_eq!(
            for_card.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["old-run"]
        );

        // Re-running the migration is a no-op: the constraint check is what
        // stops it rewriting the whole run history on every single open.
        relax_runs_task_id_nullability(&store.conn())
            .expect("the rebuild is idempotent once the constraint is gone");
        assert_eq!(
            store
                .list_runs(&id, &Default::default())
                .await
                .unwrap()
                .len(),
            2,
            "a second pass duplicated or dropped rows"
        );
    }

    /// Issue #1573: a database created before the `agent_id` mirror column must
    /// end up able to answer "what has this desk run" over its **whole**
    /// history, not just the attempts written since the upgrade.
    ///
    /// The column reaches an existing deployment through an additive `ALTER`,
    /// which leaves every stored row `NULL` — and a `NULL` never matches
    /// `agent_id = ?`. So the rows that would silently disappear from the new
    /// filter are precisely the ones an operator opening a teammate for the
    /// first time most wants to see. That is a wrong answer rather than a
    /// missing feature: an empty history reads as "this teammate has never
    /// run".
    ///
    /// Seeded with the **post-#983** shape on purpose, so this exercises the
    /// plain `ALTER` + backfill path rather than riding along on the table
    /// rebuild that `a_legacy_runs_table_learns_to_hold_a_card_less_run`
    /// already covers.
    #[tokio::test]
    async fn a_legacy_runs_table_learns_which_desk_ran_each_attempt() {
        use crate::ports::runs::{NewRun, RunFilter, RunStore};

        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE runs (
                 company_id TEXT NOT NULL,
                 id         TEXT NOT NULL,
                 task_id    TEXT,
                 status     TEXT NOT NULL,
                 attempt    INTEGER NOT NULL,
                 created_ms INTEGER NOT NULL,
                 run_json   TEXT NOT NULL,
                 PRIMARY KEY (company_id, id)
             );
             INSERT INTO runs (company_id, id, task_id, status, attempt, created_ms, run_json)
             VALUES ('acme', 'old-run', 'card-7', 'succeeded', 1, 1700000000000,
                     '{\"id\":\"old-run\",\"company\":\"acme\",\"taskId\":\"card-7\",\
                       \"agentId\":\"engineer\",\"attempt\":1,\"status\":\"succeeded\",\
                       \"createdAtMillis\":1700000000000}');",
        )
        .expect("seed a pre-#1573 database");

        let store = SqliteStore::from_conn(conn).expect("migrations run on a legacy database");
        let id = CompanyId::new("acme");

        // The desk was only ever inside the blob; the backfill is what makes it
        // a predicate.
        assert_eq!(
            store
                .list_runs(&id, &RunFilter::for_agent("engineer"))
                .await
                .expect("list by desk")
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["old-run"],
            "an attempt written before the column existed is still this desk's"
        );
        assert!(
            store
                .list_runs(&id, &RunFilter::for_agent("ceo"))
                .await
                .expect("list by desk")
                .is_empty(),
            "and it is not somebody else's"
        );

        // A run written after the upgrade is filed by the same predicate.
        store
            .create_run(&id, NewRun::for_chat("turn-1", "general", "engineer"))
            .await
            .expect("mint a run on the migrated table");
        let mut ids = store
            .list_runs(&id, &RunFilter::for_agent("engineer"))
            .await
            .expect("list by desk")
            .into_iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, ["old-run", "turn-1"]);

        // The heal is idempotent — it runs on every open, and must not rewrite
        // the run history each time. Re-running it leaves the same answer, and
        // nothing is left `NULL` for it to touch.
        heal_runs_agent_id(&store.conn()).expect("the heal is idempotent");
        assert_eq!(
            store
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM runs WHERE agent_id IS NULL",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .expect("count"),
            0,
            "the backfill left nothing behind for a later open to find"
        );
    }

    /// **Issue #392 through the port**: the host-durable append really does
    /// commit under `synchronous=FULL`, and really does put it back.
    ///
    /// `assert_journal_store` cannot see this — a backend that ignored the
    /// `Durability` argument entirely stores and orders every record identically
    /// and passes the whole suite. So the raise and the restore are asserted
    /// here, from inside and outside the closure.
    ///
    /// The restore matters as much as the raise. `synchronous` is connection
    /// state, not statement state: leaving `FULL` set would silently buy an
    /// fsync for every later write on this connection — a permanent cost from a
    /// per-record decision — and restoring only on success would leave it set
    /// exactly when something has already gone wrong.
    #[test]
    fn the_host_durable_write_runs_under_full_sync_and_restores_normal() {
        /// `PRAGMA synchronous`: 1 = NORMAL, 2 = FULL.
        fn synchronous(conn: &Connection) -> i64 {
            conn.query_row("PRAGMA synchronous", [], |r| r.get(0))
                .expect("read the pragma")
        }

        let store = SqliteStore::open_in_memory().expect("open");
        let conn = store.conn();
        assert_eq!(synchronous(&conn), 1, "the standing setting is NORMAL");

        let seen = with_full_sync(&conn, |c| Ok(synchronous(c))).expect("the write runs");
        assert_eq!(
            seen, 2,
            "the write must commit under FULL, or the host-durable level is a no-op"
        );
        assert_eq!(
            synchronous(&conn),
            1,
            "and the connection must be left as it was found"
        );

        // A failing write restores it too, and reports its own error rather than
        // the restore's.
        let err = with_full_sync(&conn, |_| {
            Err::<(), _>(OpenCompanyError::Store("the write failed".into()))
        })
        .expect_err("the write's error reaches the caller");
        assert!(err.to_string().contains("the write failed"));
        assert_eq!(
            synchronous(&conn),
            1,
            "a failed write must not strand the connection on FULL"
        );
    }

    #[tokio::test]
    async fn conformance_journal_store() {
        conformance::assert_journal_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_journal_import() {
        conformance::assert_journal_import(store()).await;
    }

    #[tokio::test]
    async fn conformance_run_store() {
        conformance::assert_run_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_run_reaper() {
        conformance::assert_run_reaper(store()).await;
    }

    #[tokio::test]
    async fn conformance_deep_trace_store() {
        conformance::assert_deep_trace_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_run_store_workflow_join() {
        conformance::assert_run_store_workflow_join(store()).await;
    }

    #[tokio::test]
    async fn conformance_schedule_fire_store() {
        conformance::assert_schedule_fire_store(store()).await;
    }

    /// A fire claim written to a file-backed database survives a reopen (issue
    /// #241) — the restart durability the whole port exists for.
    #[tokio::test]
    async fn schedule_fire_claim_survives_reopen() {
        use crate::ports::schedule_fires::ScheduleFireStore;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("company.db");
        let id = CompanyId::new("acme");
        {
            let s = SqliteStore::open(&path).unwrap();
            assert!(s.claim_fire(&id, "workflow-x", 42).await.unwrap());
        }
        // A fresh handle over the same file loses the repeat and reads the anchor.
        let s = SqliteStore::open(&path).unwrap();
        assert!(
            !s.claim_fire(&id, "workflow-x", 42).await.unwrap(),
            "a reopened database must see the earlier claim and lose the repeat"
        );
        assert_eq!(
            s.latest_fire(&id, "workflow-x").await.unwrap(),
            Some(42),
            "the anchor is durable across a reopen"
        );
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
    async fn conformance_read_state_store() {
        conformance::assert_read_state_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_notification_store() {
        conformance::assert_notification_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_workspace_store() {
        conformance::assert_workspace_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_workspace_binary_store() {
        conformance::assert_workspace_binary_store(store()).await;
    }

    #[tokio::test]
    async fn conformance_workspace_read_capped() {
        conformance::assert_workspace_read_capped(store()).await;
    }

    /// Issue #759: the folder-claim primitive, including the eight-way
    /// contention case an immediate transaction is what decides here.
    #[tokio::test]
    async fn conformance_workspace_folder_claims() {
        conformance::assert_workspace_folder_claims(store()).await;
    }

    #[tokio::test]
    async fn conformance_workspace_sibling_names() {
        conformance::assert_workspace_sibling_names(store()).await;
    }

    /// Issue #1839: the adoption lease — a second claim marks the folder, and
    /// `delete_if_empty` refuses it while a minted-unadopted twin still deletes.
    /// SQLite inherits the trait-default `delete_if_empty`, so this pins that the
    /// flag it persists in `node_json` under the adopt transaction is the flag
    /// the default reads back.
    #[tokio::test]
    async fn conformance_workspace_adoption_lease() {
        conformance::assert_workspace_adoption_lease(store()).await;
    }

    /// **The race issue #894 is actually about**, and the reason the guard is an
    /// `IMMEDIATE` transaction rather than a check in `create`'s caller.
    ///
    /// One `SqliteStore` cannot reproduce it: `conn()` holds a `StdMutex` across
    /// the read and the `INSERT`, so two tasks on one store serialize and the
    /// old code passes. The reachable shape is **two stores over one file** —
    /// the case `apply_pragmas`' `busy_timeout` note and
    /// `adopt_or_create_folder`'s header both name. Before the fix both callers
    /// read "free" and both inserted; after it the loser waits on the winner's
    /// commit, then sees the name taken.
    ///
    /// A barrier makes the overlap deterministic rather than hoping the
    /// scheduler interleaves: neither task may enter `create` until both have
    /// arrived.
    #[tokio::test]
    async fn two_stores_racing_one_name_have_one_winner() {
        use crate::ports::workspace::{NodeKind, WorkspaceOrigin, WorkspaceStore};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencompany.db");
        let one: Arc<dyn WorkspaceStore> =
            Arc::new(SqliteStore::open(&path).expect("first store over the file"));
        let two: Arc<dyn WorkspaceStore> =
            Arc::new(SqliteStore::open(&path).expect("second store over the SAME file"));

        let company = CompanyId::new("alpha");
        let origin = WorkspaceOrigin::Agent {
            id: "ceo".to_string(),
        };
        let node = |id: &str| crate::ports::workspace::WorkspaceNode {
            id: id.to_string(),
            name: "report.md".to_string(),
            kind: NodeKind::File,
            parent_id: None,
            created_by: origin.clone(),
            updated_by: origin.clone(),
            updated_at_millis: crate::ports::now_millis(),
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        };

        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let mut tasks = Vec::new();
        for (store, id) in [(one.clone(), "racer-a"), (two.clone(), "racer-b")] {
            let gate = gate.clone();
            let node = node(id);
            let company = company.clone();
            tasks.push(tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("racer runtime");
                rt.block_on(async move {
                    gate.wait().await;
                    store.create(&company, &node, Some("payload")).await
                })
            }));
        }
        let mut outcomes = Vec::new();
        for task in tasks {
            outcomes.push(task.await.expect("racer did not panic"));
        }

        let winners = outcomes.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            winners, 1,
            "exactly one create may take the name, got {outcomes:?}"
        );
        let loser = outcomes
            .iter()
            .find_map(|r| r.as_ref().err())
            .expect("the other create must fail");
        assert!(
            matches!(loser, OpenCompanyError::Conflict(_)),
            "the loser is refused, not broken: {loser:?}"
        );

        // The durable state is the real assertion: a passing error code with two
        // rows in the table would still be the bug.
        let tree = one
            .tree(&CompanyId::new("alpha"))
            .await
            .expect("tree reads");
        let named: Vec<_> = tree.iter().filter(|n| n.name == "report.md").collect();
        assert_eq!(
            named.len(),
            1,
            "one name, one node — a second row is the poisoned path #894 describes: {named:?}"
        );
    }

    /// Issue #887's no-torn-read contract. This backend passed it before the
    /// `fs` fix and passes it after — which is the point of stating it on the
    /// port: the guarantee is owed by every backend a tenant can run, not by
    /// whichever one happened to have it for free.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn conformance_workspace_read_never_tears() {
        conformance::assert_workspace_read_never_tears(store()).await;
    }

    /// The stat-then-open race fixed in the `fs` backend's `read_capped`.
    /// This backend measures and reads under one query and passes on both
    /// sides of that fix — the contract is the port's, not one backend's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn conformance_workspace_read_capped_race() {
        conformance::assert_workspace_read_capped_race(store()).await;
    }

    /// A cap above `i64::MAX` must still admit a small body. SQLite binds the
    /// comparison as a signed integer, so an unclamped cast wraps negative and
    /// withholds every note regardless of size.
    #[tokio::test]
    async fn read_capped_clamps_oversized_max_bytes() {
        use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

        let store = store();
        let company = CompanyId::new("clamp-co");
        let origin = WorkspaceOrigin::Operator;
        let node = WorkspaceNode {
            id: "clamp-note".to_string(),
            name: "clamp.md".to_string(),
            kind: NodeKind::File,
            parent_id: None,
            updated_at_millis: crate::ports::now_millis(),
            created_by: origin.clone(),
            updated_by: origin,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        };
        store
            .create(&company, &node, Some("small body"))
            .await
            .expect("create the note");

        let (_, body, len) = store
            .read_capped(&company, "clamp-note", u64::MAX)
            .await
            .expect("read with an oversized cap")
            .expect("the note exists");
        assert_eq!(
            body, "small body",
            "a cap above i64::MAX must not withhold a body far under it"
        );
        assert_eq!(len, "small body".len() as u64);
    }

    /// Issue #700's emptiness predicate, against the backend that can actually
    /// hold the shape it has to survive.
    ///
    /// The sweep refuses a folder holding an unaddressable child — a name
    /// carrying a path separator, which every path-shaped index drops and the
    /// port's recursive `delete` would still take (issue #671). Its own module
    /// tests pin that on a hand-built node list, because `FsOps` rejects such a
    /// name at creation (`reject_unsafe_name`) and cannot produce one.
    ///
    /// This backend does not reject it, and hosted tenants run this backend — so
    /// the shape is reachable data rather than a thought experiment, and that is
    /// what this asserts: sqlite stores `q/r.md` under `agents/ghost/`, and the
    /// sweep leaves `ghost` alone. It lives here because
    /// `cargo test --features sqlite --lib store::sqlite` is the only lane that
    /// runs it.
    #[tokio::test]
    async fn workspace_sweep_keeps_a_folder_whose_only_child_has_no_renderable_path() {
        use crate::company::workspace_sweep::sweep_empty_agent_folders;
        use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

        let store = store();
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

        WorkspaceStore::create(
            store.as_ref(),
            &company,
            &node("root", "Agents", NodeKind::Folder, None),
            None,
        )
        .await
        .unwrap();
        WorkspaceStore::create(
            store.as_ref(),
            &company,
            &node("ghost", "ceo", NodeKind::Folder, Some("root")),
            None,
        )
        .await
        .unwrap();
        WorkspaceStore::create(
            store.as_ref(),
            &company,
            &node("empty", "cto", NodeKind::Folder, Some("root")),
            None,
        )
        .await
        .unwrap();
        // The name `fs` would have refused. If this create ever starts failing,
        // the premise of the whole guard has changed and this test should say so
        // rather than the guard quietly becoming untested.
        WorkspaceStore::create(
            store.as_ref(),
            &company,
            &node("hidden", "q/r.md", NodeKind::File, Some("ghost")),
            Some("# quarterly"),
        )
        .await
        .expect("sqlite accepts a separator-carrying name — that is why the guard exists");

        let removed = sweep_empty_agent_folders(store.as_ref(), &company, false)
            .await
            .unwrap();

        assert_eq!(
            removed.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            vec!["empty"],
            "only the genuinely empty folder may go"
        );
        let ids: Vec<String> = WorkspaceStore::tree(store.as_ref(), &company)
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect();
        assert!(
            ids.contains(&"ghost".to_string()) && ids.contains(&"hidden".to_string()),
            "the folder and its unaddressable child must both survive, got {ids:?}"
        );
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
                overlay_retired_agents: Vec::new(),
                id: id.clone(),
                manifest: toml::from_str(
                    "[company]\nname=\"Acme\"\noutput=\"widgets\"\n[[agent]]\nid=\"ceo\"\nrole=\"Chief\"\n[policy]\nmode=\"supervised\"\n",
                )
                .unwrap(),
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
            })
            .await
            .unwrap();
        events
            .append(
                &id,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "hi".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                },
            )
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
        s.append(
            &id,
            CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();
        let received = stream.next().await.expect("event delivered");
        let EventStreamItem::Event(received) = received else {
            panic!("subscription unexpectedly reported a gap");
        };
        assert_eq!(
            received.event,
            CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }
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
                    mentions: Vec::new(),
                    parent: None,
                    text: "persist".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
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
                mentions: Vec::new(),
                parent: None,
                text: "persist".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }
        );
    }
}
