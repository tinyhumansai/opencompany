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

## Workspace layout (`src/store/layout.rs`)

`OPENCOMPANY_DATA_DIR` (default `$HOME/.opencompany`; `/data` in a hosted tenant
container) is the per-instance **workspace root** — everything one running
instance owns. [`DataLayout`](../../../src/store/layout.rs) names the canonical
subdirectories under it so stores, agents, and tools resolve well-known
locations instead of ad-hoc paths:

```text
<OPENCOMPANY_DATA_DIR>/
  companies/   ← per-company bundles (companies/<slug>/, owned by the fs store)
  memory/      ← instance-shared memory artifacts
  store/       ← instance-shared durable-store artifacts
  files/       ← instance-shared files (exports, attachments)
  logs/        ← instance logs
  tmp/         ← ephemeral scratch, cleared on startup by default
```

Per-company state (each bundle's own `memory/`/`context/`) lives under
`companies/<slug>/`; the top-level `memory/`/`store/`/`files/` are the shared,
instance-level locations. `serve` calls `DataLayout::ensure` at boot: it creates
the shared subdirectories and — unless `[workspace].clear_tmp_on_startup` is
`false` — empties `tmp/` so no stale scratch survives a restart. Because the hosting model runs **one container per tenant** with its
own `OPENCOMPANY_DATA_DIR`, this root *is* the per-tenant workspace — no separate
per-tenant path prefix is needed.

### Choosing the root (`src/store/paths.rs`)

`OPENCOMPANY_DATA_DIR` is the **only** environment knob that places an instance.
`opencompany serve` (and `export` / `import`) resolve the root every company
bundle hangs off through `store::resolve_home`, in this order:

| Precedence | Source | Resolves to |
| --- | --- | --- |
| 1 | `--home <DIR>` | `<DIR>` verbatim — an explicit flag is never overridden by the environment |
| 2 | `OPENCOMPANY_DATA_DIR` | its value verbatim, so bundles land at `<root>/companies/<slug>` — exactly the layout above |
| 3 | neither | `$HOME/.opencompany` (a relative `.opencompany` when `$HOME` is unset) |

An empty `OPENCOMPANY_DATA_DIR` counts as unset — it would otherwise root the
instance at the process working directory.

All three branches resolve the home to the **workspace root**, so `Bundle`'s own
`companies/` segment puts bundles at `<root>/companies/<slug>` in every case —
exactly the layout above, and exactly `DataLayout::companies_dir()`.

One consequence worth knowing:

- **`--home` moves the bundles but not the workspace.** It places company
  bundles only; `memory/`, `store/`, `files/`, `logs/` and `tmp/` always follow
  `OPENCOMPANY_DATA_DIR`. So two hosts isolated by `--home` alone still share one
  workspace. `serve` prints an operator-visible warning naming both roots
  whenever they are not aligned. Prefer `OPENCOMPANY_DATA_DIR`, which moves the
  whole instance. A hosted tenant sets both to the same value
  (`docker/entrypoint.sh` passes `--home "$OPENCOMPANY_DATA_DIR"`), so it never
  warns — nor does the local default, whose home and data root are now the same
  path. Passing `--home ~/.opencompany/companies` by hand recreates the legacy
  doubled shape below and does warn, correctly.

#### Migrating a legacy doubled install (`src/store/migrate.rs`)

The default home used to append a `companies` leaf of its own, so a default local
install's bundles were nested one level too deep at
`~/.opencompany/companies/companies/<slug>` while `DataLayout` materialized
`~/.opencompany/{memory,store,files,logs,tmp}` beside the *first* `companies/`. A
local sqlite database was orphaned the same way, at
`~/.opencompany/companies/opencompany.db`, because `serve` hands the resolved
home to `open_storage`. So were the two runtime trees that hang off the home
rather than off a bundle: the harness agent workspaces (`<home>/harness`) and the
MCP runtime registry (`<home>/mcp`, whose persisted installs and stored
environment values are reconnected on boot).

Dropping the leaf without moving that data would leave every existing local
company invisible, so `serve`, `export`, and `import` all run
`store::migrate::migrate_legacy_nest` against the resolved home before reading
anything:

- No `<home>/companies/companies` directory is a no-op. A hosted tenant takes
  this branch on every boot: two `stat`s that find nothing.
- A nest that is **bundle-shaped** — holding any of the top-level files
  (`company.toml`, `meta.json`, `events.jsonl`, `ledger.jsonl`, `tasks.json`, …)
  or subdirectories (`keys/`, `secrets/`, `memory/`, `context/`, …) that only a
  company owns — is a real bundle slugged `companies` and is left exactly as it
  is. A manifest is deliberately *not* the test: `Bundle::ensure_dirs` creates a
  bundle with neither marker at ~20 call sites, and under
  `OPENCOMPANY_STORAGE=sqlite|mongodb` the manifest never reaches the filesystem
  at all while the keys, secrets and task board still do — so a marker test would
  have dissolved exactly the installs that have no manifest to find.
- Only entries that are **themselves bundle-shaped directories** are relocated,
  the same test one level down. Anything else stays where it is, silently: the
  legacy nest holds nothing but bundles, so an entry that does not look like one
  is not something the migration knows where to put.
- Any `opencompany.db` (with its `-wal`/`-shm` siblings, as a set) moves from
  `<home>/companies/` to `<home>/`. Only **regular files** count as the database:
  a company slugged `opencompany.db` owns the directory at that exact path, and
  relocating it would delete the company.
- `<home>/companies/{harness,mcp}` move up to `<home>/{harness,mcp}` under the
  same shape guard — a company really can be slugged `harness`, and its canonical
  bundle sits at exactly the path the legacy tree occupied.
- An occupied destination is **skipped**, never merged: two copies of one company
  hold two event logs and two signing keys, which cannot be interleaved. Both
  copies stay put and a warning names both paths.
- Files move by `link`+`unlink`, never by `rename`. A rename replaces a regular
  file silently, and a "is the destination free?" check taken beforehand is stale
  the instant it is read — a `serve` that has already migrated is writing a live
  `-wal` at that path, and a rename over it drops every committed transaction the
  log still holds. A hard link fails when the destination exists, so the check and
  the move are one indivisible step. A crash between the link and the unlink
  leaves one file reachable under both names, which the next run recognises by
  device and inode and finishes rather than reporting as two databases.
  Directories keep `rename`, which cannot replace a populated directory
  (`ENOTEMPTY`) or a regular file (`ENOTDIR`) at all.
- The nest directory is removed only once emptied, so a crash mid-migration
  resumes on the next boot. Re-running a migrated install is silent.
- The database set resumes the same way. It is detected from **any** surviving
  member, not from `opencompany.db` alone, so a run that moved the database and
  then died is finished by the next boot rather than being read as complete —
  which would have paired a relocated database with a stranded write-ahead log
  and lost whatever that log still held.
- A source another process moved first is a success, not a failure. Running
  `opencompany export` against a home a `serve` process is booting is ordinary
  and both migrate; the loser of that race must not abort on a `NotFound` that
  means "already done". Note that this is race *tolerance*, not a concurrency
  guarantee: two processes sharing one home is unsupported for the same reason
  the runtime journal is single-writer, and this migration does not change that.
  What the no-replace moves above do guarantee is that losing such a race can
  never cost data, whatever the interleaving.

An install whose migration genuinely cannot complete — `EXDEV` because
`companies/` is a mount point, a root-owned or read-only nest — still boots:
`--home ~/.opencompany/companies` resolves every bundle exactly where it already
sits and finds no nest beneath it to migrate. That shape warns about the split
workspace, correctly, and is the supported way to run an install this migration
cannot move.

Moves are printed on stderr rather than logged through `warn!`, which the default
`EnvFilter` drops unless `RUST_LOG` is set.

`OPENCOMPANY_HOME` is **not** a synonym and is **not supported**. It was never
wired to anything, so setting it used to be ignored silently. The resolver now
reads it solely to reject it: `serve`, `export`, and `import` abort with an error
naming `OPENCOMPANY_DATA_DIR` instead. The rejection is checked before `--home`,
so passing the flag does not suppress it — a stale entrypoint that still exports
the variable fails loudly rather than half-placing a store.

#### Running two hosts side by side

Because a bundle store is shared by every process that resolves the same root,
two `serve` processes on different ports with no isolation write to one another's
companies — teammates and desks created on one appear on the other. Give each its
own root:

```sh
OPENCOMPANY_DATA_DIR=/tmp/oc-a opencompany serve \
  --company companies/e2e_harness --bind 127.0.0.1:8095 &
OPENCOMPANY_DATA_DIR=/tmp/oc-b opencompany serve \
  --company companies/e2e_harness --bind 127.0.0.1:8096 &
```

`--home /tmp/oc-a` places the bundles the same way and takes precedence, but it
does **not** move the shared workspace — prefer the variable for side-by-side
hosts.

The `[workspace]` section of `config.toml` (in the data dir) tunes the lifecycle:

```toml
[workspace]
clear_tmp_on_startup = true   # default; set false to preserve tmp/ across restarts
storage_quota_gb = 5          # soft whole-workspace quota; omit or <= 0 = unlimited
tmp_quota_gb = 1              # soft tmp/ quota; omit or <= 0 = unlimited
```

**Quotas are soft/advisory in the binary.** At boot `serve` measures the
workspace (and `tmp/`) and emits an operator-visible `tracing::warn` when either
exceeds its configured quota. **Hard enforcement** — blocking writes at the
limit — is the container/StorageClass layer's job (an EFS access point cap or a
k8s `ResourceQuota`), which is where the deploy manifests wire it; the binary
surfaces the condition rather than intercepting every write.

Large-file S3 offload remains a follow-up (needs an S3 client + credentials).

## Memory engine overlay (`OPENCOMPANY_MEMORY`)

Memory is a separable concern. `OPENCOMPANY_STORAGE` picks the durable base for
all fourteen ports; `OPENCOMPANY_MEMORY` optionally swaps **just** the two
knowledge ports — `MemoryStore` + `ContextStore` — onto a dedicated memory
engine layered on top of that base. The base still owns every other port
(companies, events, secrets, tasks, …).

| Value | Engine | Feature flag | Notes |
|---|---|---|---|
| `store` (default) | The base backend's own memory | — | fs substring recall, or sqlite/mongodb |
| `tinycortex` | In-pod TinyCortex engine | `tinycortex` | Persistent per-company store; vector-first recall with lexical/recency fallback when no embeddings backend resolves |

This is why TinyCortex is not a `StorageKind`: it implements only memory +
context, so it cannot be a full backend — it overlays. `serve` and platform
provisioning build the overlay once (`open_memory_overlay`,
`src/store/select.rs`) and apply it to each company's `RuntimeBuilder` via
`with_memory_overlay`, **after** `with_stores`, so the engine's ports win while
the base keeps the rest. A selected-but-unavailable engine (feature disabled)
aborts boot, same as the storage backend.

### In-pod engine (`EngineCortex`)

With the `tinycortex` feature and a data directory present, the overlay is
`EngineCortex` (`src/store/tinycortex_engine.rs`): the OpenHuman `tinycortex`
engine crate running **inside the pod** with durable local storage. Each company
gets its own workspace at `<OPENCOMPANY_DATA_DIR>/memory/<company>/`, and the
engine's canonical per-workspace SQLite database (opened + migrated through the
crate's own shared connection) holds that company's traces, task results, and
context chunks. The engine never makes a network call. When no data directory is
present (tests, no-data-dir callers) the overlay falls back to the offline
in-memory backend (`InMemoryCortex`), which is also the compiled fallback when a
company workspace cannot be opened.

**Vector-first recall, with a loud lexical/recency fallback (188c2).** This
slice builds the engine's `MemoryConfig` directly with `embedding.strict =
false`, so the crate's own summary-tree embedder stays inert regardless — but
when a hosted embeddings backend resolves from the environment (see
"Embeddings configuration" below), each stored chunk is separately embedded
into a per-company [`VectorStore`], and `search_chunks` runs cosine recall
**first**, topped up with the existing lexical token-overlap scorer (the same
`[0, 1]`-scored, snippet-bearing contract the in-memory backend defines) up to
the caller's limit — see the two-tier recall in
`src/store/tinycortex_engine.rs`. When **no** embeddings backend resolves — or
on any embedding/search outage — recall degrades to **pure lexical**
(substring/recency token-overlap), **not** the vector/semantic recall the
`tinycortex` name implies, so the overlay announces the degraded mode once,
loudly, at open (`tracing::warn` in `src/store/select.rs`). Because the
crate's retrieval primitives rank only by admission-score/recency in fully
degraded mode (their keyword/graph scorers are defined but not yet wired), and
its `ingest` path re-chunks documents under its own ids — which cannot
round-trip OpenCompany's content-address / label-prefix / peek contract —
chunk bodies and metadata are persisted through the engine's **KV tier** (on
the same per-company workspace database) rather than the crate's
ingest/retrieval primitives, with the vector index layered beside it. Wiring
the crate's own retrieval-scorer `Embedder` / summary-tree seal path (the
hard-768-dim path, plus a full-corpus reconcile beyond the bounded backfill) is
deferred to #198 — this slice injects only the `VectorStore` store+search
compute, which is dimension-agnostic and runs at the backend's native 1024.

#### Embeddings configuration

The hosted embeddings backend (`src/harness/embeddings.rs`, `openhuman`-gated
harness build only) shares its credential + base URL with the chat inference
client and layers two overrides on top:

| Env var | Default | Notes |
|---|---|---|
| `OPENCOMPANY_EMBEDDINGS_MODEL` | `embedding-v1` | The managed embeddings model id. `embedding-v1` is 1024-dim and rejects the OpenAI `dimensions` request param. |
| `OPENCOMPANY_EMBEDDINGS_DIM` | `1024` | The model's native dimensionality. Must parse as a positive integer; only meaningful alongside a model whose native dim differs from 1024. |

Every returned vector is validated against the configured dimensionality; a
wrong length is an error, never silently truncated.

### Durability contract & the `/data`-is-scratch caveat

`EngineCortex` is durable **only to the extent the data directory is durable**.
On a host with a persistent `OPENCOMPANY_DATA_DIR` (a mounted volume, or the
default `$HOME/.opencompany`), engine memory survives restarts. But under the
hosted multi-tenant model with `OPENCOMPANY_STORAGE=mongodb`, the durable base is
the database and the container's `/data` is treated as **ephemeral scratch** — so
engine memory written to `<data_dir>/memory` would **not** survive a container
restart. Because that failure mode is *silent* memory loss on restart, selecting
`OPENCOMPANY_MEMORY=tinycortex` together with `OPENCOMPANY_STORAGE=mongodb` is by
default a hard **refuse-to-open** error at boot (`src/store/select.rs`), not a
warning: the overlay never opens a doomed engine.

Storage-kind is only a *proxy* for "ephemeral `/data`", though — a mongodb
deployment that HAS mounted a persistent volume at the data dir is perfectly
safe to run the in-pod engine on. So the refusal is an explicit **durability
contract**, not a hard storage-kind rejection. To run the in-pod engine you can:

- mount a persistent volume at `OPENCOMPANY_DATA_DIR` and use
  `OPENCOMPANY_STORAGE=fs` or `sqlite` (durable `/data`); or
- keep memory on the base store (`OPENCOMPANY_MEMORY=store`); or
- under `OPENCOMPANY_STORAGE=mongodb`, if you have mounted a genuinely durable
  volume at `OPENCOMPANY_DATA_DIR`, set **`OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL=1`**
  to assert that durability and lift the refusal. Unset (or any non-truthy value)
  keeps the safe default: refuse. Truthy values are `1`/`true`/`yes`/`on`.

#### Config examples

**(a) Supported persistent config** — durable base + in-pod engine. The data dir
is a real mounted volume, so engine memory survives restarts and no override is
needed:

```sh
OPENCOMPANY_STORAGE=sqlite            # durable /data (single SQLite file)
OPENCOMPANY_MEMORY=tinycortex         # in-pod engine overlay
OPENCOMPANY_DATA_DIR=/data            # a persistent volume mount
# → boots; per-company workspaces persist under /data/memory/<workspace>/
```

**(b) MongoDB config — the boot-time refusal and how the opt-in changes it.**
With mongodb as the durable base, `/data` is treated as ephemeral scratch, so the
engine is refused by default:

```sh
OPENCOMPANY_STORAGE=mongodb           # durable base is the database; /data is scratch
OPENCOMPANY_MEMORY=tinycortex
OPENCOMPANY_DATA_DIR=/data
OPENCOMPANY_MONGODB_URI=mongodb://…   # (tenant-scoped)
# → REFUSES to boot: hard OpenCompanyError::Config. The operator-visible result is
#   a boot abort naming the silent-memory-loss risk and the OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL
#   opt-in — the engine never opens, so no memory is written to a doomed /data.
```

If — and only if — the operator has actually mounted a durable volume at
`/data`, asserting it lifts the refusal:

```sh
OPENCOMPANY_STORAGE=mongodb
OPENCOMPANY_MEMORY=tinycortex
OPENCOMPANY_DATA_DIR=/data            # a genuinely persistent volume
OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL=1  # operator asserts /data is durable
OPENCOMPANY_MONGODB_URI=mongodb://…
# → boots; engine memory persists under /data/memory/<workspace>/ as usual.
```

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
