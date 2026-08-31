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

A fifteenth, `JournalStore`, joined them in issue #726. The runtime journal was
built on the filesystem unconditionally until then, so on a mongodb tenant the
at-most-once effect set and the parked-approval queue lived on `/data` —
ephemeral scratch, discarded on every container replacement. It is now selected
from the same handles as every other store, with a one-time receipt-gated import
off the old file: [journal.md](journal.md).

Three of those fourteen — `UserStore`, `SessionStore`, `LoginCodeStore` — back
[human user authentication](users.md). Sessions and login codes are credential
material: they hold **hashes only**, and they must never be added to the
export path below.

MongoDB settings:

- `OPENCOMPANY_MONGODB_URI` — connection string (required for `mongodb`).
- `OPENCOMPANY_MONGODB_DB` — database name (default `opencompany`).
- `OPENCOMPANY_TENANT_ID` — tenant identity for **shared-single-DB** mode
  (default unset). See [Shared single database](#shared-single-database-mode).

## Secrets at rest, and what that costs (issue #752)

`fs` writes one **plaintext** file per secret under
`<data-dir>/companies/<slug>/secrets/`; `sqlite` puts the same bytes in a
database file on the same disk. Both are readable by the uid the server runs
as — which is the uid an agent's `shell` tool runs as, in the same container.
There is no boundary in between; see
[../security/agent-isolation.md](../security/agent-isolation.md). `mongodb` is
the only backend that keeps secrets out of the container, in the tenant
database.

**Repository credentials therefore require `OPENCOMPANY_STORAGE=mongodb`.**
This is enforced, not advised, in three places:

| Where | What happens |
| --- | --- |
| `POST …/repos` (bind) | `409 Conflict` with the refusal message; nothing is stored |
| Company boot / rebuild | The company does not come up — `OpenCompanyError::Config` — when its **effective roster** grants `repo`, including an agent naming `repo` under a wildcard company allow-list |
| Agent build | Repo tools are withheld (fail-closed, with a warning), which covers a teammate added through the console on a live runtime |

The refusal names both remedies: set `OPENCOMPANY_STORAGE=mongodb` with
`OPENCOMPANY_MONGODB_URI`, or drop the `repo` grant.

**This is a breaking change for `fs` and `sqlite` deployments that have already
bound a repository.** That is deliberate. Those are precisely the deployments
carrying a plaintext repository token on a disk their agents can read, so a
warning would leave the exposure in place and call it handled. Migration is one
of the two remedies above; a bound credential on an fs host should also be
**revoked at the forge**, since it has been readable for as long as it has been
installed.

Every *other* secret on an `fs` or `sqlite` host is still plaintext next to it.
This gate closes one credential on one path; it does not make the filesystem
safe.

## The root itself

How the data root is resolved, why only one process may write it, and what
`instance-id` is: [`data-root.md`](data-root.md). This file describes the layout
*inside* the root.

## Workspace layout (`src/store/layout.rs`)

Moved to [`workspace-layout.md`](workspace-layout.md) — this file was over the repository's 500-line limit. See that page for the full detail.

## Memory engine overlay (`OPENCOMPANY_MEMORY`)

Moved to [`memory-engine.md`](memory-engine.md) — this file was over the repository's 500-line limit. See that page for the full detail.

`OPENCOMPANY_MEMORY` selects `store` (default), `remote`, or `null`. A hosted engine additionally needs
`OPENCOMPANY_MEMORY_DRIVER`, `OPENCOMPANY_MEMORY_URL` and
`OPENCOMPANY_MEMORY_API_KEY`; each refuses at boot when missing, naming the
knob, and never falls back to the base store's memory. The in-pod
`embedded`/`tinycortex` engine and its `namespace` provider-store mode were
removed in #1568 and refuse at boot if still selected. The credential and the
endpoint never appear in logs, `/healthz`, `/spec`, status output, or an export
— `/spec` reports the engine's `driver_id` and negotiated capabilities only.

## MongoDB backend (`src/store/mongodb.rs`)

One `MongoStore` wraps a single database and implements all five ports.
Payloads are stored as the same JSON strings the fs/sqlite backends persist,
so records round-trip byte-identically across backends and `export`/`import`
migrate between any two backends unchanged. Monotonic 0-based sequences come
from a `counters` collection via atomic `findOneAndUpdate {$inc}`.

Collections (all uniquely indexed on `company_id` + their key):
`companies`, `ledger`, `events`, `memory_traces`, `memory_tasks`,
`context_chunks`, `secrets`, `journal` and `journal_imports`, plus `counters`
and `owners`; and the WS3 console-surface collections `tasks`, `workspace`,
`facts`, `usage`, `skills`, and `inboxes`. The `usage` collection is trimmed to the 90-day retention window
on each `record` (see [`UsageMeter`](ports-console.md#usagemeter)).

**Path-uniqueness indexes on `workspace_nodes`.** This backend has neither a
per-company lock nor a transaction it can require of its deployment, so "one node
per path" cannot be a read followed by a write; it is the database's own
invariant. Two **partial unique** indexes carry it: `(company_id,
file_path_key)` for files (issue #697) and `(company_id, folder_path_key)` for
the folders `adopt_or_create_folder` claims (issue #759). Both keys encode
`{parent_id}\0{name}` — NUL, because no node name may contain one — and both are
separate fields on purpose, so a folder and a note may still share a name exactly
as they always could. This is a race fix, not a new tree rule.

*Partial* is what makes them safe to add to a live tenant: a plain unique index
is built over every existing document, so a company that already lost one of
these races would fail index creation, and that failure happens during
`ensure_indexes` — taking the tenant's startup down. Restricted to documents that
*have* the field, each index covers everything written from now on and ignores
history it cannot repair; legacy duplicates keep being refused one layer up.

Only the primitives stamp the keys. Plain `create` does not stamp
`folder_path_key`, so console-made folders are unguarded (and unaffected). A
folder that `rename_move` touches **drops** its key: the claim exists to decide
contention on the publish walk, and a key that travelled with a hand-moved folder
would keep guarding the path it left — refusing that path forever, which is the
outage the primitive exists to prevent.

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
`assert_skill_state_store`, `assert_inbox_store`, `assert_usage_meter`,
`assert_secret_store` — plus a dedicated `assert_usage_retention` that verifies
samples older than the 90-day window are evicted on write. A new backend passes
only when all of these hold.

`assert_secret_store` (issue #1505) covers the port holding a tenant's inference
credential, MCP OAuth tokens, Composio account tokens and SMTP password:
read-back, absence, per-key independence, overwrite, "cleared is an empty value
and not absence", and — the property with security consequences — that a secret
written for company A is unreadable as company B, in both directions. The port
has no `delete`; callers clear by writing an empty value, which is why the
empty-value case stands in for a deletion case.

It also asserts that two distinct keys stay distinct — issue #1510. The
filesystem backend encodes each key into an injective filename (percent-encoded
with a `%` prefix the legacy slug layout can never produce, and truncated with a
digest suffix for long keys), and the old slugged file is kept readable as a
migration fallback. Upper-case letters are percent-encoded rather than passed
through (so filenames stay distinct on case-insensitive volumes — the macOS and
Windows default), and a trailing `.` is encoded as `%2E` (Windows strips
trailing periods), so distinct keys map to distinct files on every supported
filesystem. `set` keeps the legacy file for non-empty rotations, because one
slug can name several distinct keys and it may still hold a colliding alias's
value that an un-migrated alias reads through the fallback. Clears are
different: writing an empty value is a revocation, so the shared legacy file is
removed rather than allowing an un-migrated alias to resurrect the revoked
credential. `get` prefers the canonical file, so a rotated key is shadowed while
a cleared ambiguous legacy value is unavailable to every alias. The suite covers
both the space-vs-underscore keys the old slug conflated, two keys differing only
in letter case, a key ending in a period, and a key shaped like a legacy filename
(`key-foo`) reading or deleting a different key's value.

**Fixtures in this suite are non-empty on purpose.** An empty vec, map or `None`
survives every possible bug, including a backend that never persisted the field
at all, so seeding one certifies the gap it was meant to close. Issue #1504 was
exactly that: `CompanyRecord::overlay_agents` was seeded as `Vec::new()` and
never read back, so a backend that dropped every console-created teammate passed
the whole suite. The fixture now seeds `overlay_agents`, `overlay_desks` and
`overlay_desk_members` with their optional fields populated, and both
`assert_isolation_by_company` and `assert_export_totality` assert them.

`assert_workspace_folder_claims` (issue #759) additionally drives eight
concurrent callers at one `(parent, name)`: all must succeed, all must come away
holding the same folder, and exactly one may report having created it. That case
is the point of the function — a naive read-then-create passes every sequential
assertion beside it and fails only there, which is the defect's exact shape.

## Testing

`cargo test --features mongodb,sqlite` runs everything; the MongoDB
conformance tests are env-gated and skip unless
`OPENCOMPANY_TEST_MONGODB_URI` points at a live server:

```sh
OPENCOMPANY_TEST_MONGODB_URI=mongodb://127.0.0.1:27017 \
  cargo test --features mongodb
```

Each test creates (and drops) a uniquely named throwaway database.

## Deep trace

Each backend gains one store for the unredacted companion of a run's steps —
fs `deep-trace.jsonl`, sqlite `run_step_details`, one mongodb collection of the
same name — all keyed `(company_id, run_id, step_seq)` and held to
`assert_deep_trace_store` in the shared conformance suite. See
[deep-trace.md](deep-trace.md) for what it holds, its caps, and why it is a
sibling of the run store rather than part of it.

`runs` also gains a `workflow_run_id` mirror for the workflow-node join
(see [ports-runs.md](ports-runs.md)). On sqlite the column is added by
`add_column_if_missing` and its index created **after** the `#983` table rebuild
— that rebuild recreates `runs` from a fixed column list, so an index declared in
`MIGRATIONS` would fail outright on exactly the legacy databases the additive
step exists for.
