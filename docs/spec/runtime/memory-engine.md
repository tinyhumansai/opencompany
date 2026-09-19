# The memory engine overlay

`OPENCOMPANY_MEMORY` and the provider seam: what each mode does, and why a
hosted engine refuses to boot rather than silently losing memory.

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
| `remote` | A hosted memory service | `tinymemory` | Bound through the `MemoryProvider` contract; needs a URL and a credential |
| `null` | Nothing | `tinymemory` | Writes accepted and discarded, reads empty |

The in-pod `embedded`/`tinycortex` engine and its `namespace` provider-store
mode were removed in #1568; a deployment that still sets
`OPENCOMPANY_MEMORY=embedded`, `tinycortex` or `cortex` is refused at boot,
naming the removed value. Only the `store` default, the hosted `remote` modes
and `null` remain.

Whether Cortex could return as a *hosted* engine under `remote` — the opposite
question from #1568 — is investigated in
[`memory-engine-cortex.md`](memory-engine-cortex.md), which records what a
deployed CortexDB instance actually provides. The driver is now registered and
`cortex` is selectable, but that record argues against choosing it and its open
decisions still stand. A companion,
[`memory-engine-cortex-driver.md`](memory-engine-cortex-driver.md), records what a
driver against v0.9.8 has to do and what each call costs.

## Choosing a hosted engine (`remote`)

| Env var | Required | Notes |
|---|---|---|
| `OPENCOMPANY_MEMORY_DRIVER` | yes | `supermemory`, `mem0`, `cognee`, `cortexdb`, or `cortex`. No default — see below. `cortexdb` and `cortex` are two adapters for the same CortexDB service: `cortexdb` is this repo's own and sends `X-Cortex-Actor` (see `OPENCOMPANY_MEMORY_ACTOR`), `cortex` is the dialect `tinymemory-remote` ships. Read [`memory-engine-cortex.md`](memory-engine-cortex.md) before choosing either. |
| `OPENCOMPANY_MEMORY_URL` | yes | The engine's endpoint. |
| `OPENCOMPANY_MEMORY_API_KEY` | yes | The outbound credential. |
| `OPENCOMPANY_MEMORY_ACTOR` | no (`cortexdb` only) | The `X-Cortex-Actor` header value, `type:id` (type one of `user`, `agent`, `service`, `system` — a bare id is refused). CortexDB hard-refuses a request whose actor does not match the bearer token's subject; unset falls back to the token's own `sub` JWT claim, then to `service:opencompany`. |

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
deliberately no fall back to the base store's memory. A company that believes it is
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

`driver_id()` is surfaced only by the authenticated memory-engine endpoint,
alongside the capability families the driver negotiated at bind time — a hosted engine
typically has no summary tree, no graph and no taint tier, and an operator
should be able to read that rather than discover it from a failed cycle.

### Class is decided by the host, never by the driver

`OPENCOMPANY_MEMORY=remote` pins the driver class to `External` and cross-checks
it against the registry's reserved table, so naming an embedded-class driver
under the remote mode is refused rather than quietly resolved. The contract crate
excludes driver class on purpose: a driver that self-reported it could claim to
be embedded and skip the egress and trust checks that class gates.

### A driver that over-claims its capabilities is refused at bind

`capabilities()` is a claim the driver writes by hand; `provides()` is derived
from the accessors it actually returns. The host compares the two once, at bind
(`audit_provider`, called from `src/store/memory/driver.rs`), because it registers RPC
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

### What the audit cannot catch, and the boot probe that does

Both sides of that comparison are properties of the **adapter**: `provides()` is
`self.as_x().is_some()`, a Rust-object check. Neither asks whether the engine
behind the adapter answers. An engine that exposes a family's surface, reports
itself healthy, and serves nothing passes the bind and then returns empty on
every read — the same harm the audit exists to prevent, arriving by a route the
audit cannot see.

The three **mandatory** families are the sharp case: `provides()` returns `true`
for Core, Recall and Portability unconditionally, so the audit cannot fail them
by construction — and they are exactly the three this host binds `MemoryStore`,
`ContextStore` and `FactStore` to.

`MemoryOverlay::refresh_health` therefore reads once against every family the
driver advertises that has a cheap read of its own, concurrently under one
deadline, and records what did not answer on the descriptor. At boot it is
advisory, like the health probe beside it: it warns loudly and does not refuse,
because a transient vendor outage must not crash-loop a tenant. The console
apply route *does* refuse, matching what it already does for a failed health
probe — an operator applying a change is present, and the previous engine stays
in force.

The results are split by **consequence**, not by confidence:

| descriptor field | route field | what it holds | effect |
| --- | --- | --- | --- |
| `unreachable_families` | `unreachableFamilies` | mandatory families that refused | apply refuses the bind |
| `degraded_families` | `degradedFamilies` | advertised optional families that refused | reported; the engine binds |
| `slow_families` | `slowFamilies` | any family that did not answer in the budget | reported; the engine binds |

Core and Recall are what `MemoryStore`, `ContextStore` and `FactStore` are built
on, so an engine refusing one cannot serve a cycle at all. An engine refusing
`people` serves every cycle and fails one agent tool — refusing to bind over
that would take a mostly-working engine away from an operator who has no other
one. Both are reported; only the first is a refusal.

`store::select::family_leg` is the exhaustive table of which read stands for
which family. Every probed leg is a **required** trait method, read-only, and
either keyed to a name no company can produce or limited to one record — a
defaulted method would answer `Unsupported` for a driver that implements the
family perfectly well, and reporting that as a refusal would break working
engines. The match has no `_` arm on purpose: `Capability` is not
`#[non_exhaustive]`, so a family added upstream fails to compile here until
somebody decides how, or whether, it is probed.

Nine families are deliberately **not** probed, because none of them has such a
read:

- `Ingest`, `Sources`, `DocumentIngest`, `ConversationIngest`, `LearningIngest`
  and `EventIngest` — every required method writes, and `forget_source`
  deletes. Probing them would store something in a tenant's engine on every
  bind.
- `Maintenance` — its required methods are `reembed`, `compact`, `consolidate`
  and `doctor`: whole-store jobs. The cheap reads beside them are defaulted.
- `Answer` — grounded synthesis, an inference call, metered and measured in
  seconds.
- `Portability` — mandatory, and the one that most looks like an omission. Its
  only read is `export_page`, which enumerates every namespace and then lists
  one in full, so on a hosted engine it is tens of sequential round trips that
  grow with the corpus. It would time out and report a working engine broken.

A family the driver does not advertise is never probed: absence is a legitimate
answer, and reading a family nobody claims would report every minimal driver
broken.

The read path (`GET …/memory/engine`) reuses an answer younger than fifteen
seconds instead of asking again. The probe is one read per advertised family
plus health, so re-running it per request charged a console page load — and
every re-render behind it — a full round against an engine that may meter each
call. `test` and `apply` never reuse: they act on the answer, and an operator
who has just fixed a credential must not be shown the verdict from before the
fix.

**An empty answer is success.** A freshly provisioned engine holds nothing, so
reading "no rows" as "not implemented" would refuse every family on day one;
only an error or a timeout counts. That also bounds what this catches: an engine
answering `Ok(empty)` forever while storing nothing is indistinguishable from a
new one without an engine-specific signal, which belongs in the adapter and its
conformance suite rather than here — a live lane against a real engine, not an
offline double written from the same vendor documentation the adapter was.
Tracked in issue #1968.

## Which contract this binds

`tinymemory-api`, at `vendor/openhuman/vendor/tinymemory/crates/tinymemory-api`
— the same path
`vendor/openhuman` itself path-depends on, which is what keeps the
`MemoryProvider` trait identity single across the process. The historical
`tinycortex-api` re-export and the in-pod `tinycortex` engine that used to back
the `embedded` mode are removed; only the hosted drivers and `null` bind
through this seam.

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

`open_memory_overlay` (`src/store/select.rs`) builds the overlay once per boot
and `RuntimeBuilder::with_memory_overlay` applies it to each company's
`RuntimeBuilder`, **after** `with_stores`, so the engine's ports win while the
base keeps the rest. A selected-but-unavailable engine (feature disabled)
aborts boot, same as the storage backend.

The removed in-pod engine wrote durable state to `<OPENCOMPANY_DATA_DIR>/memory/`
and was refused under `OPENCOMPANY_STORAGE=mongodb` (the `/data`-is-scratch
caveat). The hosted modes write to their provider and the `store` default
reuses the base backend, so neither has an ephemeral-`/data` hazard; the
`OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL` flag is retained as a no-op for deployment
compatibility.

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
  task outcomes and operator facts are not an agent's to delete. Chunks are
  content-addressed, and since #1300 every backend keeps one claim per
  (addr, label) with a **label-scoped** `ContextStore::delete_label`: a
  forget whose identical content is indexed under other labels (another
  agent's byte-identical memory, a task outcome with the same text) removes
  exactly the agent's own claims, the other labels keep the body, and the
  body is reaped — atomically inside the port — only with its last claim.
  (Before #1300 this case refused outright, which let anyone make an
  agent's memory permanently un-forgettable by storing identical text.) The
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
   their enumeration-based write cost. The `store` default has no provider
   seam and is refused by name — for those, `opencompany export` reads the
   live engine (base backend plus memory overlay, operator facts included)
   and is the capture tool. Target drivers are the hosted engines
   `supermemory`, `mem0`, `cognee`, `cortexdb` and `cortex`. Hosted tenants run
   **`cortex`** — the manager injects that id at provision — so a migration onto
   a provisioned engine names `cortex`, not `cortexdb`.

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
2. **Set the variables** for the target engine (the `deploy/.env.example` block names
   all five). A hosted engine needs the build to carry the `tinymemory`
   feature; a feature-less build refuses at boot naming the missing feature.
3. **Restart.** Selection is read once at boot; a running process never
   re-reads it.
4. **Verify through the authenticated `GET /api/v1/company/memory/engine`**:
   `active` names what is bound, `capabilities` lists what it negotiated, and
   `healthy` is re-probed for the read (reusing an answer taken in the last
   fifteen seconds). `false` means bound-but-unreachable (bad endpoint or
   credential); absent means "not probed" (the `store` default).
   `unreachableFamilies` and `degradedFamilies` are the part `capabilities`
   cannot tell you: what the engine *answered*, mandatory and optional, as
   against what the driver claims.

Misconfiguration never falls back: an unknown mode, a missing driver, URL or
key, or a missing cargo feature is a boot refusal naming the knob to change.

## Running CortexDB locally

`cortexdb` (`src/store/memory/cortexdb.rs`) speaks to a standalone
[CortexDB](https://github.com/tinyhumansai) instance (`cortexdb/cortexdb`
Docker image) over its own HTTP API — it is not the removed in-pod
`tinycortex` engine, and does not reintroduce it. `scripts/cortexdb-up.sh`
starts (or reuses) a local instance on `127.0.0.1:3141` with enrichment,
layers and graph extraction off by default, so writes cost one embedding call
and nothing else; it prints the exact `OPENCOMPANY_MEMORY_*` exports this
build needs:

```bash
./scripts/cortexdb-up.sh
export OPENCOMPANY_MEMORY=remote
export OPENCOMPANY_MEMORY_DRIVER=cortexdb
export OPENCOMPANY_MEMORY_URL=http://127.0.0.1:3141
export OPENCOMPANY_MEMORY_API_KEY=<printed by the script>
cargo run --bin opencompany -- serve
```

CortexDB namespaces every write under `org:opencompany/ns:<hash>`, one scope
per tinymemory namespace (which already carries this host's own per-company
isolation — see "Tenant isolation across the seam" above), so two companies
never share a CortexDB scope and therefore never share recall. Two headers
travel on every request: `Authorization: Bearer <key>` and
`X-Cortex-Actor: <actor>` — CortexDB hard-401s a mismatch between them, which
is why `OPENCOMPANY_MEMORY_ACTOR` exists.
