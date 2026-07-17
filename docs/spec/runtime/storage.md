# Storage backends

The storage ports (see [ports.md](ports.md)) are the entire persistence
contract. The five core ports — `CompanyStore`, `EventLog`, `MemoryStore`,
`ContextStore`, `SecretStore` — plus the six console-surface stores added in
WS3 — `TaskStore`, `WorkspaceStore`, `FactStore`, `UsageMeter`,
`SkillStateStore`, `InboxStore` — are all that a backend must implement. The
kernel never names an engine; a backend is anything that implements those
traits and passes the conformance suite (`src/store/conformance.rs`). This file
documents the shipped backends and how one is selected at boot.

## Selection

`OPENCOMPANY_STORAGE` picks the backend once per process; `serve` and
platform provisioning inject the same opened handles into every company's
`RuntimeBuilder` (`src/store/select.rs`). A selected-but-unavailable backend
aborts boot — there is never a silent fallback to the filesystem.

| Value | Backend | Feature flag | Notes |
|---|---|---|---|
| `fs` (default) | Per-company bundle directories | — | Human-inspectable; no external service |
| `sqlite` | One SQLite file under the data dir | `sqlite` | Single-file, offline |
| `mongodb` | A MongoDB database on a shared cluster | `mongodb` | The multi-tenant platform backend |

Each backend implements **all fourteen** ports. The fs backend keeps the core
records as inspectable TOML/JSONL bundles and the WS3 console-surface stores
under a sibling `ops/` layout (`src/store/fs_ops.rs`); sqlite and mongodb add
one collection/table per store.

Three of those fourteen — `UserStore`, `SessionStore`, `LoginCodeStore` — back
[human user authentication](users.md). Sessions and login codes are credential
material: they hold **hashes only**, and they must never be added to the
export path below.

MongoDB settings:

- `OPENCOMPANY_MONGODB_URI` — connection string (required for `mongodb`).
- `OPENCOMPANY_MONGODB_DB` — database name (default `opencompany`).

## MongoDB backend (`src/store/mongodb.rs`)

One `MongoStore` wraps a single database and implements all five ports.
Payloads are stored as the same JSON strings the fs/sqlite backends persist,
so records round-trip byte-identically across backends and `export`/`import`
migrate between any two backends unchanged. Monotonic 0-based sequences come
from a `counters` collection via atomic `findOneAndUpdate {$inc}`.

Collections (all uniquely indexed on `company_id` + their key):
`companies`, `ledger`, `events`, `memory_traces`, `memory_tasks`,
`context_chunks`, `secrets`, plus `counters` and `owners`; and the WS3
console-surface collections `tasks`, `workspace`, `facts`, `usage`, `skills`,
and `inboxes`. The `usage` collection is trimmed to the 90-day retention window
on each `record` (see [ports.md](ports.md), `UsageMeter`).

### Multi-tenant isolation (two layers)

1. **Database per tenant (recommended).** The hosting layer (the
   opencompany-manager control plane) runs one shared MongoDB but creates one
   logical database per tenant (`oc-<slug>`) and one database-level user
   whose only role is `readWrite` on that database. The credentials are
   injected as `OPENCOMPANY_MONGODB_URI`/`OPENCOMPANY_MONGODB_DB` when the
   tenant workload is created. A compromised tenant container cannot address
   any other tenant's data — isolation is MongoDB auth, not an application
   filter.
2. **Company scoping inside a database.** Mirroring the sqlite backend, every
   document carries `company_id` and every query filters on it, so one
   database can also host multiple companies (platform mode). The `owners`
   collection makes the company → tenant map durable: `serve` hydrates the
   in-memory `AppState` ownership map from it at boot, and provisioning
   updates it — closing the previous restart-loses-ownership stub.

### Adding another backend (e.g. DynamoDB)

Implement the five traits in a new `src/store/<engine>.rs` behind a feature
flag, key everything on `company_id`, run the conformance suite against it,
and add a `StorageKind` arm in `src/store/select.rs`. No business logic
changes.

## Conformance coverage

`src/store/conformance.rs` is the backend-agnostic suite every backend runs.
Beyond the core assertions (per-company isolation, append-only event/ledger,
monotonic event sequence, export totality) it exercises each WS3 store —
`assert_task_store`, `assert_workspace_store`, `assert_fact_store`,
`assert_skill_state_store`, `assert_inbox_store`, `assert_usage_meter` — plus a
dedicated `assert_usage_retention` that verifies samples older than the 90-day
window are evicted on write. A new backend passes only when all of these hold.

## Testing

`cargo test --features mongodb,sqlite` runs everything; the MongoDB
conformance tests are env-gated and skip unless
`OPENCOMPANY_TEST_MONGODB_URI` points at a live server:

```sh
OPENCOMPANY_TEST_MONGODB_URI=mongodb://127.0.0.1:27017 \
  cargo test --features mongodb
```

Each test creates (and drops) a uniquely named throwaway database.
