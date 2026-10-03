# The memory engine overlay

`OPENCOMPANY_MEMORY` and the TinyMemory v2 engine seam: what each mode does,
and why a hosted engine refuses to boot rather than silently losing memory.

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
| `remote` | A hosted TinyMemory v2 engine | `tinymemory` | `OPENCOMPANY_MEMORY_DRIVER` names it; a credential is required, the URL is optional |
| `null` | Nothing (host-side `NullEngine`) | `tinymemory` | Writes accepted and discarded, reads empty |

The in-pod `embedded`/`tinycortex` engine and its `namespace` mode were removed
in #1568; a deployment that still sets `OPENCOMPANY_MEMORY=embedded`,
`tinycortex` or `cortex` is refused at boot, naming the value.

## Choosing a hosted engine (`remote`)

The engines are TinyMemory v2's registry (`tinymemory::list_engines`), so the
set is whatever the pinned crate ships; the console catalog
(`src/server/ops/memory_engine.rs`) is generated from it, with each engine's
`accepts_url` and `default_url`.

| Engine id | What it is | Default endpoint |
|---|---|---|
| `cortexdb` | CortexDB, called directly over its own API | `https://api-v1.cortexdb.ai` |
| `tinyhumans` | CortexDB hosted by the TinyHumans backend | `https://api.tinyhumans.ai` |

`cortex` is accepted as an alias of `cortexdb` (the manager injects that id
at provision). `supermemory`, `mem0` and `cognee` are
**retired**: they are refused by name with a hint to migrate off them, never
silently mapped onto another engine (`RETIRED_ENGINES` in
`src/store/memory/driver.rs`). An unknown id is refused naming the ids this
build can bind.

| Env var | Required | Notes |
|---|---|---|
| `OPENCOMPANY_MEMORY_DRIVER` | yes | An engine id above. No default — see below. |
| `OPENCOMPANY_MEMORY_URL` | no | Overrides the engine's default endpoint (a self-hosted CortexDB, a local instance). A credentialed endpoint must be `https`, or `http` to a loopback host; that rule is enforced upstream in `tinymemory::build_engine`. |
| `OPENCOMPANY_MEMORY_API_KEY` | yes | The outbound credential. |

`OPENCOMPANY_MEMORY_DEPLOYMENT` and `OPENCOMPANY_MEMORY_ACTOR` (the old
`X-Cortex-Actor` header value) are gone: the cortex engine learns the actor
from CortexDB's `whoami` for the credential it holds. Setting them has no
effect.

**A missing driver or key refuses at boot, naming the knob.** There is
deliberately no fall back to the base store's memory. A company that believes it is
writing to its hosted memory and is not is worse off than one that fails to
start: the second failure is visible immediately, and the first is invisible
until the memory is needed and turns out not to be there.

There is no default driver id for the same reason. Guessing which hosted service
an operator meant would write a company's memory somewhere it cannot be read
back from, and that is not a recoverable mistake.

### The credential is a secret; the endpoint is topology

Neither appears in logs, `/healthz`, `/spec`, status output, or an export.
`StorageSettings` and `MemoryDriverConfig` both carry hand-written `Debug` impls
rendering `<set>` rather than the value, because both types are reachable from
boot logging where a bare `{:?}` is one keystroke away.

The engine id and the retrieval modes it serves (`keyword`, `vector`,
`hybrid`) are surfaced by the authenticated memory-engine endpoint, so an
operator can see how search will rank rather than discover it from a poor
answer.

## CortexDB

CortexDB is the memory service both hosted engines speak to: `cortexdb`
directly, `tinyhumans` through the TinyHumans backend. This repository no
longer carries an adapter for it, nor a separate dialect: the engine is
TinyMemory's own `tinymemory-cortex` crate, pinned through the `vendor/openhuman`
submodule at `vendor/openhuman/vendor/tinymemory/crates/tinymemory-cortex`.
Its wire behaviour, the scoping CortexDB enforces server-side, and its measured
costs are documented there, not here. The earlier investigation pages this
repo kept for the deleted host adapter were removed with it.

What this host relies on is only the engine contract: a `Document` item carries
`meta.workspace`, `folder` and `source.id`, and a listing or fetch can be
narrowed on them (the next sections).

### Running CortexDB locally

`scripts/cortexdb-up.sh` starts (or reuses) a local `cortexdb/cortexdb` Docker
instance on `127.0.0.1:3141` with enrichment, layers and graph extraction off by
default, so writes cost one embedding call and nothing else; it prints the exact
`OPENCOMPANY_MEMORY_*` exports this build needs:

```bash
./scripts/cortexdb-up.sh
export OPENCOMPANY_MEMORY=remote
export OPENCOMPANY_MEMORY_DRIVER=cortexdb
export OPENCOMPANY_MEMORY_URL=http://127.0.0.1:3141
export OPENCOMPANY_MEMORY_API_KEY=<printed by the script>
cargo run --bin opencompany -- serve
```

## The boot probe

An engine can answer `health()` from local state while its credential is
revoked, so `store::select::probe_engine` runs two calls concurrently, each
under its own deadline: `health()`, and one bounded, read-only `list` against a
workspace no tenant owns (`__host_probe__`; company workspaces are `oc/`-rooted
and hash-suffixed, so the read is a miss and touches nobody's records). `list`
is the one call every engine must serve and the one every port read is built on.

Results land on the `MemoryDescriptor` and the route, split by **consequence**:

| descriptor field | route field | what it holds | effect |
| --- | --- | --- | --- |
| `unreachable_families` | `unreachableFamilies` | `"list"` when the read errored | apply refuses the bind |
| `degraded_families` | `degradedFamilies` | the engine's own reason when `health()` says `Degraded` | reported; the engine binds |
| `slow_families` | `slowFamilies` | `"health"` / `"list"` when it missed the budget | reported; the engine binds |

The field names are the v1 "capability family" spelling, kept so the console and
`/spec` consumers do not break; they now carry **operation names** and the
engine's reason, not families. `Down` health maps to `healthy: false`;
`Degraded` is reachable and serving, so it stays `healthy: true` with the reason
recorded.

At boot the probe is advisory: it warns loudly and does not refuse, because a
transient vendor outage must not crash-loop a tenant. The console apply route
*does* refuse on a failed health report or a refused read, matching what an
operator who is present would want — the previous engine stays in force.

**An empty answer is success.** A freshly provisioned engine holds nothing, so
reading "no rows" as "broken" would refuse it on day one; only an error or a
timeout counts. That also bounds what this catches: an engine answering
`Ok(empty)` forever while storing nothing is indistinguishable from a new one.
Tracked in issue #1968.

The read path (`GET …/memory/engine`) reuses an answer younger than fifteen
seconds instead of asking again, since the probe costs real calls against an
engine that may meter each one. `test` and `apply` never reuse: an operator who
has just fixed a credential must not be shown the verdict from before the fix.

## Which contract this binds

`tinymemory`, at `vendor/openhuman/vendor/tinymemory/crates/tinymemory` — the
same path `vendor/openhuman` itself path-depends on, which keeps the
`MemoryEngine` trait identity single across the process. The contract is
engine-neutral: `store`, `fetch`, `list`, `forget`, `recall`, `health`, with
scoping by item metadata (`MemoryMeta::workspace`, `folder`, `MetaFilter`) and
no tenant argument and no namespace. The v1 `MemoryProvider` contract, its
capability families and the `tinymemory-remote` dialect are gone.

OpenHuman's **own** memory domain is switched off on the shared runtime
(`host_domains` in `src/harness/openhuman_runtime.rs`). That domain is one
engine for the whole process, which under multi-tenant-in-one-process would
serve one company's memory to another; company memory belongs to
`store::memory::BoundMemory` alone.

## Tenant isolation across the seam

The three memory ports take `&CompanyId` as an explicit first argument — a
compiler-enforced isolation invariant. `MemoryEngine` has no tenant argument,
and a missing filter would be a silent cross-tenant leak with no type-level
guard. With a hosted engine it is worse: the workspace string is the only thing
separating tenants inside somebody else's database.

`store::memory::BoundMemory` is therefore the only public way to get a memory
port out of an engine. Its `Namespace` type has no public constructor and is
derived from the company id through an injective sanitize-plus-hash — sanitizing
alone would collapse `acme:1`, `acme/1` and `acme_1` onto one namespace. A
namespace looks like `oc/<company>-<hash>/<scope>`, the scope being a closed
enum (facts, traces, archive, scratch, …).

### How a port record becomes an item

The ports are keyed (a fact id, a chunk address, a cycle id); the v2 contract is
not — `store` mints a content fingerprint and there is no `get(key)`. So each
port record is one `Document` item whose metadata carries the addressing:
`meta.workspace` and `folder` are the tenant namespace, and `meta.source.id` is
the port key. The body is a versioned JSON envelope (MIME
`application/vnd.opencompany.memory+json`). A rewrite is **store-then-forget**:
the new body is stored first and the old items forgotten second, so a crash
between the two leaves a duplicate the next read reconciles (newest
`observed_at` wins), never a hole.

The namespace is derived **per call**, from the `&CompanyId` the port method was
given, not fixed when the facade is built. One overlay is opened per process and
shared by every company on the host, so a namespace fixed at construction would
be one tenant's namespace serving all of them.

Every read **forces** the workspace into the filter and then re-checks each hit
against the namespace it asked for; hits reported outside it are dropped with a
warning. That re-check should never fire; if it does, the alternative was
serving one tenant another's memory. Search uses `fetch` — keyword mode when the
engine offers it, else hybrid.

## What the host owns, because the contract does not

The contract deliberately carries no policy, which leaves these host-side:

- **Archive on evict.** `evict` *moves* traces to an archive namespace rather
  than forgetting them, because the contract has no archive tier and
  `docs/spec/company-brain/memory.md` makes archiving normative. The archive
  write is ordered **before** the live delete: there is no transaction spanning
  two engine calls, so a crash in between leaves a duplicate the next read
  reconciles rather than a hole.
- **The scratch firewall.** Provisional working-out lives in its own namespace,
  a sibling of every durable scope, so durable recall cannot reach it even if an
  engine ignores the workspace filter.
- **Provenance.** v1 had a `MemoryTaint` enum the engine stored; v2 has none.
  Inbound-channel writes now carry `SourceKind::Link` plus the tag
  `oc:provenance:external` (`BoundMemory::inbound_context`,
  `facades::EXTERNAL_TAG`), which this host stamps and owns. The tag is on the
  item, so an operator filtering the engine's own console sees it and a future
  reader can refuse it by `tags_any`.
- **Per-agent and per-desk scoping**, which neither cognition port has.

`open_memory_overlay` (`src/store/select.rs`) builds the overlay once per boot
and `RuntimeBuilder::with_memory_overlay` applies it to each company's
`RuntimeBuilder`, **after** `with_stores`, so the engine's ports win while the
base keeps the rest. A selected-but-unavailable engine (feature disabled)
aborts boot, same as the storage backend.

The hosted modes write to their provider and the `store` default reuses the
base backend, so neither has an ephemeral-`/data` hazard; the
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
  candidate, probes it (see "The boot probe" above), and refuses on a failed
  health report or a refused read, leaving the previous
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

## Depth: provenance, deliberate memory, and what is deliberately not wired

Four determinations from the depth pass (issue #1113), recorded so nobody
re-derives them:

- **Provenance routing is by trigger, at the cycle.** A cycle triggered by
  `WebhookReceived` or `A2aTaskReceived` — outside content: a channel
  message, an email, a third-party callback, a remote agent's payload —
  writes its brain-chosen context puts through the overlay's inbound port,
  which stamps `SourceKind::Link` and the `oc:provenance:external` tag;
  everything else (`OperatorMessage`, `FeedbackFiled`, `PaymentReceived`, the
  company's own machinery) writes through the ordinary port, unmarked.
  Coarse by design — the host cannot see which put quoted the payload, and
  over-marking is safe where under-marking is the leak. `OperatorMessage`
  turns are deliberately internal: operator speech is the company writing
  about itself, the same authorship precedent that treats operator facts as
  internal. Read-side *filtering* on the mark is a separate, larger change
  (the ports' `ChunkMeta`/`ChunkHit` carry no provenance field); until it
  lands, the mark is honest at the engine and invisible to port readers.
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
   a live cycle writes to the source *after* its page was listed is lost to
   the target, and a hosted listing cursor can skip or repeat rows against a
   store that keeps changing underneath it. A paused company loses nothing:
   chat still parks, and the whole procedure is one restart long anyway.
1. **Move the data.** `opencompany memory migrate --to <engine>` copies every
   item under the folder root `oc` — every company this host wrote, and
   nothing other products wrote into the same engine — from the env-selected
   engine (the source: you have not flipped the environment yet, so it still
   names the old one) into the target, `list` → `store`, page by page. Items
   cross with workspace, folder, key, tags and provenance untouched. `store` is
   idempotent by content fingerprint, so re-running a page cannot duplicate: an
   item the target already holds is reported as replayed and writes nothing.
   `--dry-run` counts first; a stopped run prints the `--resume-cursor` to
   re-enter at; `--page-size` defaults to 500. Targets are `cortexdb` (alias
   `cortex`) or `tinyhumans`; `--to-url` is required only for an engine without
   a default endpoint (all current engines have one). The source must be
   `OPENCOMPANY_MEMORY=remote`: the `store` default has no engine seam and is
   refused by name — use `opencompany export`, which reads the live engine (base
   backend plus memory overlay, operator facts included). `null`, shared-DB
   tenant mode (`OPENCOMPANY_TENANT_ID`) and a copy of an engine onto itself are
   refused too. A retired source engine (`supermemory`, `mem0`, `cognee`) cannot
   be opened by this build: run the migration from a build that still has it.

   Two hosted-deployment cautions. The copy is **engine-level**: every
   workspace the source credential can see under `oc` crosses, which is exactly
   right when each tenant has its own hosted account and credential — and
   exactly wrong if two tenants ever shared one, so keep hosted memory
   credentials per-tenant. And pass the target credential through
   `OPENCOMPANY_MEMORY_TARGET_API_KEY`, not `--to-api-key`: a flag sits in
   `/proc/<pid>/cmdline`, world-readable for the whole (possibly long) run. The
   flag remains only for compatibility. On completion the command re-counts the
   **target's own** listing as a receipt, so the evidence is the target's answer
   rather than the migration's own counters.
2. **Set the variables** for the target engine (the `deploy/.env.example` block
   names all of them). A hosted engine needs the build to carry the
   `tinymemory` feature (on by default); a feature-less build refuses at boot
   naming the missing feature.
3. **Restart.** Selection is read once at boot; a running process never
   re-reads it.
4. **Verify through the authenticated `GET /api/v1/company/memory/engine`**:
   `active` names what is bound, `capabilities` lists the retrieval modes it
   serves, and `healthy` is re-probed for the read (reusing an answer taken in
   the last fifteen seconds). `false` means bound-but-unreachable (bad endpoint
   or credential); absent means "not probed" (the `store` default).
   `unreachableFamilies`, `degradedFamilies` and `slowFamilies` carry the probe
   operation names and the engine's own degraded reason.

Misconfiguration never falls back: an unknown mode, a missing driver or key, a
retired engine, or a missing cargo feature is a boot refusal naming the knob to
change.
