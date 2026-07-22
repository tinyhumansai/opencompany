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
- `OPENCOMPANY_TENANT_ID` — tenant identity for **shared-single-DB** mode
  (default unset). See [Shared single database](#shared-single-database-mode).

## Memory engine overlay (`OPENCOMPANY_MEMORY`)

Memory is a separable concern. `OPENCOMPANY_STORAGE` picks the durable base for
all fourteen ports; `OPENCOMPANY_MEMORY` optionally swaps **just** the two
knowledge ports — `MemoryStore` + `ContextStore` — onto a dedicated memory
engine layered on top of that base. The base still owns every other port
(companies, events, secrets, tasks, …).

| Value | Engine | Feature flag | Notes |
|---|---|---|---|
| `store` (default) | The base backend's own memory | — | fs substring recall, or sqlite/mongodb |
| `tinycortex` | TinyCortex chunk store | `tinycortex` | Ranked token-overlap recall over a compounding store |

This is why TinyCortex is not a `StorageKind`: it implements only memory +
context, so it cannot be a full backend — it overlays. `serve` and platform
provisioning build the overlay once (`open_memory_overlay`,
`src/store/select.rs`) and apply it to each company's `RuntimeBuilder` via
`with_memory_overlay`, **after** `with_stores`, so the engine's ports win while
the base keeps the rest. A selected-but-unavailable engine (feature disabled)
aborts boot, same as the storage backend.

The compiled TinyCortex backend is the offline in-memory client
(`src/store/tinycortex.rs`); a networked client for true semantic recall is an
inert seam until the service is reachable through the OpenHuman integration.

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

### Shared single database (`OPENCOMPANY_TENANT_ID`) mode

An operator may run every tenant workload against **one** logical MongoDB
database instead of one database per tenant (e.g. to stay under a managed
cluster's database/namespace limits). In this mode the manager injects
`OPENCOMPANY_TENANT_ID=<tenant-slug>` (alongside `OPENCOMPANY_MONGODB_DB`
pointing all tenants at the shared database name) so the workload can keep its
records apart:

- **Id namespacing.** Company ids are prefixed with `<tenant>--` before they
  reach the store (`AppConfig::namespaced_company_id`). Both the boot path and
  the API provisioning path prefix with the workload's own
  `OPENCOMPANY_TENANT_ID` — config, not the request's acting tenant, is
  authoritative for this workload's data scope. So even a full-platform token
  provisioning on behalf of another tenant yields a workload-local id rather
  than one prefixed with a foreign tenant. This keeps the same boot template
  (`OPENCOMPANY_COMPANY=agentic_software_company` for every tenant) from
  colliding on the `companies` collection's unique `company_id` index. The
  prefix is idempotent — an already-prefixed id passes through unchanged.
- **Ownership.** A provisioned or boot company's `company_id -> tenant_id`
  mapping is written to the `owners` collection (best-effort) with the
  workload's own `OPENCOMPANY_TENANT_ID`, so a shared-DB manager can enumerate
  and purge a tenant's companies later. Recording the same value the id is
  namespaced with is what lets owners hydration reload it: hydration at boot
  filters to rows whose `tenant_id` equals this workload's
  `OPENCOMPANY_TENANT_ID`, so the in-memory ownership map never carries other
  tenants' companies and no API-provisioned company is orphaned across a
  restart.

Everything is backwards compatible: with `OPENCOMPANY_TENANT_ID` unset, id
derivation, ownership recording, and owners hydration behave exactly as before
(the db-per-tenant and single-tenant paths are unchanged).

#### Isolation tradeoff — read this before enabling shared-single-DB mode

In shared-single-DB mode all tenant workloads hold credentials to the **same**
logical database. Isolation is **application-layer only** — the `<tenant>--`
id namespace, the `company_id` filter on every query, and the registry serving
only locally-loaded companies. A compromised or malicious tenant container that
reaches the database directly can read and write **every** tenant's documents;
nothing at the MongoDB auth layer stops it. Database-per-tenant (layer 1 below)
remains the security-recommended mode and stays the manager default; enable
shared-single-DB mode only where the operational constraint outweighs this
weaker isolation.

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
