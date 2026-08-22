# The memory engine overlay

`OPENCOMPANY_MEMORY` and the in-pod engine: what each mode does, and why an
ephemeral data root refuses to boot rather than silently losing memory.

Split out of [`storage.md`](storage.md), which was over the repository's 500-line
ceiling.

## Memory engine overlay (`OPENCOMPANY_MEMORY`)

Memory is a separable concern. `OPENCOMPANY_STORAGE` picks the durable base for
all fourteen ports; `OPENCOMPANY_MEMORY` optionally swaps the three
knowledge ports — `MemoryStore`, `ContextStore` and `FactStore` — onto a
dedicated memory engine layered on top of that base. The base still owns every other port
(companies, events, secrets, tasks, …).

| Value | Engine | Feature flag | Notes |
|---|---|---|---|
| `store` (default) | The base backend's own memory | — | fs substring recall, or sqlite/mongodb |
| `embedded` (or `tinycortex`) | In-pod TinyCortex engine | `tinycortex` | Persistent per-company store; vector-first recall with lexical/recency fallback when no embeddings backend resolves |
| `embedded` + `OPENCOMPANY_MEMORY_DRIVER=namespace` | In-pod contract store | `tinymemory-embedded` | `tinymemory-core`'s durable `UnifiedMemory`, bound through the `MemoryProvider` contract; no network call |
| `embedded` + `OPENCOMPANY_MEMORY_DRIVER=module` | The loadable TinyMemory module | `tinymemory-module` | The separately compiled `cdylib` over TinyBus (issue #1524): digest-verified, loaded eagerly at boot (refuse, never degrade), store at `<data_dir>/memory-module` — a **different engine and directory** than `namespace`, never an alias. Preflight: `opencompany modules check` |
| `remote` | A hosted memory service | `tinymemory` | Bound through the `MemoryProvider` contract; needs a URL and a credential |
| `null` | Nothing | `tinymemory` | Writes accepted and discarded, reads empty |

`embedded` and `tinycortex` are the **same value**. Issue #914 introduced the
first spelling; the second keeps parsing indefinitely, because renaming it would
break every deployment that already sets it — including hosted tenants whose
environment the control plane injects — for a cosmetic gain. The same applies to
`cortex`, and to `mongo` on `OPENCOMPANY_STORAGE`. Only one name is reported
back out (`/spec` says `embedded`), so a client never has to know both.

## Choosing a hosted engine (`remote`)

| Env var | Required | Notes |
|---|---|---|
| `OPENCOMPANY_MEMORY_DRIVER` | yes | `supermemory`, `mem0`, or `cognee`. No default — see below. |
| `OPENCOMPANY_MEMORY_URL` | yes | The engine's endpoint. |
| `OPENCOMPANY_MEMORY_API_KEY` | yes | The outbound credential. |

### `remote` is conformance-backed

The unproven-remote acceptance flag that used to guard this mode is retired,
exactly as its own text promised: it was a gate on *confidence*, meant to be
deleted rather than lived with, and its premise — no driver conformance suite
(tinymemory#18 §E1) — stopped being true when the vendored tinymemory gained
one. The suite now runs against every driver, the remote adapters carry
failure-path tests (error mapping, malformed responses), and the bind-time
capability audit asserts the advertised families match the reachable surface
on every boot. `remote` is an ordinary choice: a driver, a URL, a credential.

**Every one of these refuses at boot when missing, naming the knob.** There is
deliberately no fall back to the embedded engine. A company that believes it is
writing to its hosted memory and is not is worse off than one that fails to
start: the second failure is visible immediately, and the first is invisible
until the memory is needed and turns out not to be there.

There is no default driver id for the same reason. Guessing which hosted service
an operator meant would write a company's memory somewhere it cannot be read
back from, and that is not a recoverable mistake.

### The credential is a secret; the endpoint is topology

Neither appears in logs, `/healthz`, `/spec`, status output, or an export.
`StorageSettings` and the driver config both carry hand-written `Debug` impls
rendering `<set>` rather than the value, because both types are reachable from
boot logging where a bare `{:?}` is one keystroke away.

`driver_id()` **is** safe to surface, and `/spec` reports it alongside the
capability families the driver negotiated at bind time — a hosted engine
typically has no summary tree, no graph and no taint tier, and an operator
should be able to read that rather than discover it from a failed cycle.

### Class is decided by the host, never by the driver

`OPENCOMPANY_MEMORY=remote` pins the driver class to `External` and cross-checks
it against the registry's reserved table, so naming the embedded engine under
the remote mode is refused rather than quietly resolved. The contract crate
excludes driver class on purpose: a driver that self-reported it could claim to
be embedded and skip the egress and trust checks that class gates.

### A driver that over-claims its capabilities is refused at bind

`capabilities()` is a claim the driver writes by hand; `provides()` is derived
from the accessors it actually returns. The host compares the two once, at bind
(`audit_capabilities` in `src/store/memory/driver.rs`), because it registers RPC
methods and assembles agent tools from the *claim* and never re-checks. A driver
advertising a family it does not implement would otherwise produce a surface
that exists, is offered to an agent, and fails on its first call — inside a
tenant, at the moment the memory is needed.

The two directions of mismatch are not the same failure and are not treated
alike:

- **Advertised but absent** refuses the bind, naming the families. There is no
  opt-in flag: this is an adapter bug, not a deployment choice, so no
  environment variable lifts it.
- **Present but unadvertised** logs a warning and boots. The family works but
  nothing routes to it, because routing follows the claim. That is dead surface
  from a forgotten `capabilities()` entry — refusing a boot over it would turn
  an upstream oversight into a tenant outage.

Structurally neither should fire: every adapter reachable from here is composed
through `MemoryTraitProvider`, which derives its advertisement from its
accessors. The check runs anyway because that guarantee lives upstream, in a
submodule this repository pins by gitlink, and a gitlink bump is exactly when it
would quietly stop holding.

## Which contract this binds

`tinymemory-api`, at `vendor/openhuman/vendor/tinymemory/api` — the same path
`vendor/openhuman` itself path-depends on, which is what keeps the
`MemoryProvider` trait identity single across the process.

**Not `tinycortex-api`.** They are distinct crates on incompatible contract
majors (`(1, 0)` against `(2, 0)`, and `is_compatible` is major-equality only),
and OpenHuman's own inlined contract documents `tinycortex-api` as a deprecated
re-export. The `tinycortex` crate remains pinned as the *engine* behind the
embedded mode; only the contract moved.

## `embedded` through the provider seam (`namespace`)

`remote` and `null` bind a provider. Plain `embedded` keeps the `EngineCortex`
overlay it has always had — today's companies have their data in those tables,
and swapping the default out from under them would strand it. But the mode is
no longer *confined* to that overlay: `OPENCOMPANY_MEMORY_DRIVER=namespace`
binds `tinymemory-core`'s `UnifiedMemory` — the contract's own durable SQLite
store, whose `Memory` implementation reports `name() == "namespace"` — through
the same seam as the hosted engines. Same registry admission (the id is
host-reserved at class `Embedded`), same bind-time capability audit, same
`BoundMemory` tenant-namespace facades, full three-port overlay with the
inbound-taint and scratch partitions. No network call; the store persists
under `<OPENCOMPANY_DATA_DIR>/memory-namespace/` — beside, never inside, the
incumbent engine's `memory/`, and nothing migrates between the two. An
operator who switches starts that store empty.

It rides the `tinymemory-embedded` feature (which implies `tinymemory`),
separate on purpose: the store lives in `tinymemory-core`, which pulls the
in-pod engine weight — `tinycortex` and a bundled SQLite — that a
hosted-memory tenant deliberately builds without (tinymemory#18 §D). Selecting
the driver without the feature refuses at boot, naming the feature; naming any
other driver id under `embedded` refuses too, never a silent fallback to the
engine the operator did not ask for. The `/data`-is-scratch durability refusal
below applies to this store exactly as it does to `EngineCortex`, and there is
no in-memory fallback when the data dir is missing.

Recall honesty: no embedding backend is injected, so every chunk is stored
vector-less and recall runs on the store's graph and keyword tiers. That is
the same loud degraded-mode contract `EngineCortex` ships under.

The earlier form of this section said the seam needed "a durable `Memory`
implementation over the engine's KV tier first". `UnifiedMemory` is that
implementation — it was the store, not the engine's KV tier, that supplied
it.

## Tenant isolation across the seam

The three memory ports take `&CompanyId` as an explicit first argument — a
compiler-enforced isolation invariant. `MemoryProvider` has only
`namespace: &str`, and a missing prefix would be a silent cross-tenant leak with
no type-level guard. With a hosted engine it is worse: the namespace string is
the only thing separating tenants inside somebody else's database.

`store::memory::BoundMemory` is therefore the only public way to get a memory
port out of a provider. Its `Namespace` type has no public constructor and is
derived from the company id through an injective sanitize-plus-hash — sanitizing
alone would collapse `acme:1`, `acme/1` and `acme_1` onto one namespace.

The namespace is derived **per call**, from the `&CompanyId` the port method was
given, not fixed when the facade is built. One overlay is opened per process and
shared by every company on the host, so a namespace fixed at construction would
be one tenant's namespace serving all of them. Deriving per call also makes the
namespace a pure function of the argument the port contract already requires, so
it cannot be stale or mismatched with the caller's intent.

Every read is additionally re-checked against the namespace it asked for, and
entries reported outside it are dropped with a warning. That filter should never
fire; if it does, the alternative was serving one tenant another's memory.

## What the host owns, because the contract does not

The contract deliberately carries no policy, which leaves these host-side:

- **Archive on evict.** `evict` *moves* traces to an archive namespace rather
  than forgetting them, because the contract has no archive tier and
  `docs/spec/company-brain/memory.md` makes archiving normative. The archive
  write is ordered **before** the live delete: there is no transaction spanning
  two provider calls, so a crash in between leaves a duplicate the next read
  reconciles rather than a hole.
- **The scratch firewall.** Provisional working-out lives in its own namespace,
  a sibling of every durable scope, so durable recall cannot reach it even if a
  driver ignores the namespace filter.
- **Taint.** Inbound-channel writes are stamped `ExternalSync`. Note the
  contract's `MemoryCore::store` *requires* taint on every call and has no
  dropping default — the defaulted `store_with_taint` is on the engine-side
  `Memory` trait, which is why nothing here wraps a bare `Memory`.
- **Per-agent and per-desk scoping**, which neither cognition port has.

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
gets its own workspace at `<OPENCOMPANY_DATA_DIR>/memory/<workspace-name>/` — the
path-safe, stable name derived from the full company ID (`EngineCortex::workspace_name`
sanitizes the id and appends a stable hash), the same `<workspace>` the config
examples below render — and the engine's canonical per-workspace SQLite database
(opened + migrated through the crate's own shared connection) holds that
company's traces, task results, and context chunks. The local workspace
persistence layer does not make network calls; a configured hosted embeddings
backend may make outbound requests during embedding and recall. When no data
directory is present (tests, no-data-dir callers) the overlay selects the
offline in-memory backend (`InMemoryCortex`). An error opening a company
workspace propagates to the caller rather than silently switching to in-memory.

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
compute, which is dimension-agnostic and runs at the configured embedding
dimension (1024 by default).

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

## Choosing an engine from the console

Engine selection stays **instance-wide** — one engine per host, every company
on it sharing that engine — but it is no longer environment-only. `config.toml`
gained a `[memory]` section, and `…/memory/engine` is the surface that writes
it:

```text
GET  …/memory/engine        what is bound, what is saved, what may be picked
POST …/memory/engine/test   probe a candidate without saving it
PUT  …/memory/engine        save it, bind it, and put it in force
```

Three properties make this safe to hand an admin, and each is a refusal rather
than a convention:

- **The environment still wins.** `OPENCOMPANY_MEMORY` set at all makes the
  file layer inert, the console read-only, and a `PUT` a `409` naming the
  variable. A hosted tenant's control plane injects those variables, so a
  console that accepted the edit would write a file, report success, and change
  nothing at the next boot.
- **An engine that does not answer is not bound.** The route opens the
  candidate, probes it, and refuses on a failed probe, leaving the previous
  overlay in force — the opposite of boot, which binds and warns because a
  transient vendor outage must not crash-loop a tenant. `?force=true` is the
  escape hatch.
- **It applies live, or says which companies it did not reach.** The new
  overlay is swapped onto the `AppState` and every registered company is
  rebuilt through `RuntimeRebuilder`; a company that cannot be rebuilt is
  *named* in `restartRequiredFor` rather than covered by a blanket "restart
  required". The credential is never read back out — the route reports whether
  a key is set, never its bytes.

What has **not** changed, and is still a decision rather than a gap: there is
no per-company and no per-agent selection. Memory is storage, and nothing
model-shaped may repoint it — this deliberately does not follow the
per-company `[inference]` model, for the reason recorded at the selection site
in `src/store/select.rs`. Splitting workloads across engines (traces local,
facts hosted) remains a possible refinement of *routing*, not of selection.

**Switching still moves no data.** A new engine starts empty; see the runbook
below, whose migration step is the only thing that moves records.

## Depth: taint, deliberate memory, and what is deliberately not wired

Four determinations from the depth pass (issue #1113), recorded so nobody
re-derives them:

- **Taint routing is by trigger, at the cycle.** A cycle triggered by
  `WebhookReceived` or `A2aTaskReceived` — outside content: a channel
  message, an email, a third-party callback, a remote agent's payload —
  writes its brain-chosen context puts through the overlay's inbound port,
  which stamps `ExternalSync`; everything else (`OperatorMessage`,
  `FeedbackFiled`, `PaymentReceived`, the company's own machinery) stamps
  `Internal`. Coarse by design — the host cannot see which put quoted the
  payload, and over-tainting is safe where under-tainting is the leak.
  `OperatorMessage` turns are deliberately `Internal`: operator speech is the
  company writing about itself, the same authorship precedent that stamps
  operator facts `Internal`. Read-side taint *filtering* is a separate,
  larger change (a `taint` field on `ChunkMeta`/`ChunkHit` and every
  backend); until it lands, the stamp is honest at the engine and invisible
  to readers.
- **Deliberate agent memory is three oc-authored tools** — `memory_store`,
  `memory_recall`, `memory_forget` — over the company's own `ContextStore`,
  company and agent captured at build time, never a model-supplied
  namespace. Forget reaches only the agent's own `agent-memory/<id>/` rows;
  task outcomes and operator facts are not an agent's to delete. And because
  chunks are content-addressed with an ADDRESS-level `ContextStore::delete`,
  a forget whose identical content is indexed under any other label (another
  agent's byte-identical memory, a task outcome with the same text) refuses
  rather than deleting theirs too — store a correction instead. The
  vendored upstream memory tools stay unwired: they resolve their store
  ambiently, which under multi-tenant-in-one-process is a cross-company
  leak (`src/harness/built_in/build.rs`, `memory_tools`).
- **Scratch stays on the overlay, unwired, until its first consumer.**
  Carrying it into the harness with zero consumers would recreate the dead
  seam this pass existed to remove.
- **Hybrid routing (traces local, facts hosted) is deferred, not rejected** —
  it would sidestep the hosted enumeration-cost cliff without waiting on
  upstream keyed CRUD, but it is a refinement of *routing* under the P3
  selection decision, and it waits for real usage data to say which
  workloads actually hurt.

## Switching engines — the operator runbook

Whether the switch is a console apply or an env flip plus a restart, **the
switch alone moves no data** — a switched engine starts empty until something
puts records in it — so the migration below is the step that moves it, and it
comes first.

0. **Stop the writes.** Pause the workload (or scale the tenant to zero)
   before migrating: the copy is page-by-page with no dual-write, so anything
   a live cycle writes to the source *after* its page was exported is lost to
   the target. The export cursor is also the source driver's own — against a
   store that keeps changing underneath it, a hosted cursor can skip or repeat
   rows. A paused company loses nothing: chat still parks, and the whole
   procedure is one restart long anyway.
1. **Move the data.** `opencompany memory migrate --to <driver>` copies every
   record from the env-selected engine (the source — you have not flipped the
   environment yet, so it still names the old engine) into the target, over
   the contract's Portability family: namespaces, record kinds and provenance
   taint round-trip untouched. `--dry-run` counts first; a stopped run prints
   the `--resume-cursor` to re-enter at (import is idempotent by
   `(namespace, key)`, so re-running a failed page cannot duplicate — drivers
   that detect presence report `skipped`, the rest overwrite in place). Hosted targets warn about
   their enumeration-based write cost. The `store` default and the
   EngineCortex overlay have no provider seam and are refused by name — for
   those, `opencompany export` now reads the live engine (base backend plus
   memory overlay, operator facts included) and is the capture tool.

   Two hosted-deployment cautions. The copy is **engine-level**: every
   namespace the source credential can see crosses, which is exactly right
   when each tenant has its own hosted account and credential — and exactly
   wrong if two tenants ever shared one, so keep hosted memory credentials
   per-tenant. And pass the target credential through
   `OPENCOMPANY_MEMORY_TARGET_API_KEY`, not `--to-api-key`: a flag sits in
   `/proc/<pid>/cmdline`, world-readable for the whole (possibly long) run,
   which no shell-history hygiene fixes. The flag remains only for
   compatibility. On completion the command re-counts the **target's own**
   export as a receipt, so the evidence is the target's answer rather than
   the migration's own counters.
2. **Set the variables** for the target engine (the `.env.example` block names
   all five). A hosted engine needs the build to carry the `tinymemory`
   feature; `namespace` needs `tinymemory-embedded`. A feature-less build
   refuses at boot naming the missing feature.
3. **Restart.** Selection is read once at boot; a running process never
   re-reads it.
4. **Verify on `GET /spec`**: `memory.backend` and `memory.driver_id` name
   what you selected, `memory.capabilities` lists what it negotiated, and
   `memory.healthy` reports the boot-time reachability probe — `false` means
   bound-but-unreachable (bad endpoint or credential); absent means "not
   probed" (the `store` default or the direct engine overlay).

Misconfiguration never falls back: an unknown mode, a missing driver, URL or
key, or a missing cargo feature is a boot refusal naming the knob to change.

> **`namespace` caveat (2026-08-20):** #1201 (writes corrupted by the PII
> scrubber redacting Luhn-valid digit runs — most often `at_millis` timestamps
> — into broken JSON) is fixed in this change's own stack: the scrubber now
> corroborates before redacting, and two regression nets pin it (the
> Luhn-timestamp round-trip, and the survival contract in
> `store::memory::upstream_conformance_test`). One defect remains open —
> #1238 (a dropped context chunk and reordered traces under the port
> conformance suite). Until it lands, prefer the incumbent `embedded` engine
> overlay or a hosted engine for anything real.
