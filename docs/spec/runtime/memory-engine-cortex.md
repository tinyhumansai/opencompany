# Hosting Cortex behind the memory seam

A design record for [#1936](https://github.com/tinyhumansai/opencompany/issues/1936):
can we host [`tinycortex`](https://github.com/tinyhumansai/tinycortex) ourselves and
let tenants bind to it through the `remote` seam, on equal footing with
`supermemory`, `mem0` and `cognee`?

Companion to [`memory-engine.md`](memory-engine.md), which specifies the seam this
would bind through. **That document describes what ships; this one describes a
proposal and the measurements behind it.**

`cortex` is a **selectable** engine as of #2065 — the driver from
[tinymemory#128](https://github.com/tinyhumansai/tinymemory/pull/128) is
registered, and both live suites pass against a real CortexDB. Selectable is not
selected: `OPENCOMPANY_MEMORY` defaults exactly as it did, and choosing Cortex is
the decision this record informs. It argues against it — with **one leg of that
argument retracted**; see [Correction](#correction-2026-09-04) below.

## Correction (2026-09-04)

**Finding 3 below is overturned, not merely withdrawn. The derived fact and
belief tier works.** It was measured on a server with enrichment switched off,
and on v0.9.8 nothing on the wire said so.

CortexDB v0.9.9 (released 2026-09-03, after this record was written) adds
`embeddings`, `enrichment` and `content_processors` checks to
`GET /v1/admin/ready`, precisely because a server could report
`{"status":"healthy"}` and `degraded: false` while underdelivering. Ours
answered `enrichment: { "enabled": false, "mode": "off" }`.

Three prerequisites were unset, none discoverable on v0.9.8:

1. `CORTEX_ENRICHMENT_URL` / `_API_KEY` — the enrichment router is a separate
   lane from `CORTEX_LLM_*`. `_MODEL` alone gives it a model with no endpoint.
2. `CORTEX_ENTITY_API_KEY` — the binary is explicit that "both the primary
   entity LLM and enrichment router are required". Without it, boot logs
   `LLM router not configured` even with `CORTEX_LLM_URL`/`_MODEL` both set.
3. `CORTEX_ENRICHMENT_DELAY_SECONDS` — **the on/off gate**. Unset means
   enrichment is off entirely: "events/episodes/recall only".

A first re-run still indexed 0 facts, because the embedding provider was
answering `402 Insufficient credits`. With that fixed — 120 embedding calls,
0 failures — **10 events produced 19 facts and 9 beliefs**, as
subject/predicate/object triples with resolved entities and confidence:

```text
ent_Aniketh                  prefers  "evidence over product-page claims"
ent_tinymemory_cortex_driver folds    "newest-per-key on read"     conf 1.0
```

So the commercial argument against Cortex — that it offers nothing the
incumbents do not — does not hold. It offers this.

**What replaces it is narrower and sharper: beliefs build but never revise.**
Three events establishing "Priya Raman owns billing", then two handing it to
Marcus Webb, leaves *both* owners live at `confidence: 1.0` with `valid_to:
null`, and `/v1/recall` returns them undifferentiated. The section below was
right about the consequence and wrong about the cause (its heading is now
corrected too): a `FactStore` consumer
reads live and superseded claims with nothing marking which is which. Filed as
[#2089](https://github.com/tinyhumansai/opencompany/issues/2089). Understanding
stays empty and concepts stay broken.

Finding 2 is untouched, and it *forces* instance-per-tenant rather than blocking
it: it removes the shared-instance-with-per-tenant-credentials row from the
topology table, while instance-per-tenant needs no token scoping at all. The
decision taken on [#2072](https://github.com/tinyhumansai/opencompany/issues/2072)
is to adopt at that topology.

## Findings first

A CortexDB instance was deployed and exercised to answer this from evidence
rather than from the product page. Five results change the shape of the
question, and the recommendation follows from them.

1. **`tinyhumansai/tinycortex` is not the deployable artifact.** It is a Rust
   *library* crate — no binary target, no server, no Dockerfile. The server is
   **closed-source**, distributed only as prebuilt artifacts (`cortexdbai/cortexdb-releases`,
   Docker Hub `cortexdb/cortexdb`). Whatever we build treats it as an opaque
   upstream binary we cannot patch.
2. **The one token minter a self-hosted operator can reach does not confine a
   token to its scope.** `POST /v1/auth/tokens` answers `NOT_CONFIGURED` by
   default, but `CORTEX_V1_MINTER_ENABLE=1` turns it on and it mints correctly
   — `subject`, `scope`, TTL, capability narrowing, and working revocation. The
   scope does not hold. A token minted *for* scope A, pointed at scope B: `POST
   /v1/recall` is refused `403 POLICY_DENIED`, but `GET /v1/events?scope=B`
   returns B's records and `POST /v1/experience` into B is accepted. Reproduced
   three times, including with narrowed `capabilities`. The vendor documents
   this minter as dev-only (a minted token reports `tenant: dev`) and says
   production presets expect an external OIDC provider or the separate
   `cortex-auth-ref` issuer — which is absent from the v0.9.8 assets, has no
   public repository, and no published contract. So the reachable minter does
   not isolate, and the isolating one is not reachable.
3. ~~**The derived fact and belief tier does not work.**~~ **Overturned — see
   [Correction](#correction-2026-09-04).** The run below had enrichment off and
   no way to report it; configured, and on a funded provider account, Facts and
   Beliefs do build. What still holds is the structural half: these are Cortex
   *layers*, not contract capability families, so the audit cannot fire on them
   however empty they are.
4. **Retrieval quality is real, and comes from embeddings alone.** Ranked recall
   over the Events layer is good and needs no LLM lanes at all.
5. **The contract's upsert has no direct mapping, but a conformant driver is
   reachable.** `(namespace, key)` replacement cannot be expressed against an
   append-only log with immutable keys. It can be *reconstructed*: append every
   write under a fresh idempotency key carrying the logical key, and fold to
   newest-per-key on read. That driver exists and passes. Its cost — set out in
   [the driver notes](memory-engine-cortex-driver.md) — is what should decide
   this, not impossibility.

## The isolation choice collapses

The issue frames a choice between Cortex-DB-per-tenant and one shared Cortex
with namespace-only separation, and says to default to the stronger. Finding 2
removes the middle option:

| Option | Isolation tier | Reachable self-hosted? |
|---|---|---|
| One shared instance, one bootstrap credential | Namespace-only — the **weak** tier | Yes |
| **One instance per tenant**, own key and own data dir | Credential *and* storage isolation | **Yes** |
| Shared instance, real per-tenant credentials | Strong | **No** — the reachable minter does not confine a token to its scope (finding 2) |

`memory-engine.md` is unambiguous about why the weak tier is not acceptable as a
default: with a hosted engine "the namespace string is the only thing separating
tenants inside somebody else's database", and the engine-level `memory migrate`
copies *every namespace a source credential can see*, which is "exactly wrong if
two tenants ever shared one".

**Recommendation: instance-per-tenant, co-located on shared infrastructure.** It
is the only self-hosted route to the tier the seam already assumes. Note what
this does to the issue's own framing: "shared hosting" becomes shared
*infrastructure*, not a shared engine process. The per-tenant Cortex credential
then slots into `OPENCOMPANY_MEMORY_API_KEY` exactly like any other hosted
engine's, and the migrate caution above is satisfied by construction.

## What Cortex would and would not replace

Worth stating plainly, because the two knobs are independent and it is easy to
read one as the other.

| Knob | Scope |
|---|---|
| `OPENCOMPANY_STORAGE` | the durable base for **all fourteen ports** — `fs`, `sqlite`, or `mongodb` |
| `OPENCOMPANY_MEMORY` | an optional overlay for **three of them** — `MemoryStore`, `ContextStore`, `FactStore` |

Where memory actually lives therefore depends on the overlay, not the base
alone:

- **`store` (default)** — memory reuses the base backend. Under
  `OPENCOMPANY_STORAGE=mongodb`, memory *is* in MongoDB. This is what tenants
  run today.
- **`embedded`/tinycortex (removed)** — memory lived in the engine's own
  filesystem workspace under `<OPENCOMPANY_DATA_DIR>/memory/`, **not** MongoDB.
  Selecting it alongside `OPENCOMPANY_STORAGE=mongodb` was a **boot refusal**,
  because `/data` is scratch in that mode and the memory would have been
  silently lost on restart.
- **`remote`** — memory lives in the hosted provider; the base backend keeps
  the other eleven ports.

So adopting Cortex is **not** replacing MongoDB. It carves three ports out of
the base backend and leaves the other eleven — companies, events, secrets,
tasks and the rest — exactly where they are. A tenant would run Mongo *and*
Cortex, each owning a disjoint set of ports.

That also bounds the blast radius of everything in this document: a Cortex
outage costs a tenant its knowledge ports, not its company records.

## Layers are not capability families

The empty layers are real, but they are **not** a capability-audit finding, and
it is worth separating the two because conflating them points a driver plan at
the wrong thing.

Measured on the deployment, with the LLM lanes believed configured and the
server reporting healthy — see [Correction](#correction-2026-09-04) for why that
belief was wrong:

| Cortex layer | Endpoint | Contents |
|---|---|---|
| Events | `/v1/events` | populated |
| Episodes | `/v1/episodes` | populated |
| Facts | `/v1/facts` | **empty** |
| Beliefs | `/v1/beliefs` | **empty** |
| Understanding | `/v1/understanding` | **empty**; errors every scheduler tick |

The contract's `Capability` enum is a closed set — deliberately not
`#[non_exhaustive]`, so adding a variant is a compile error rather than a config
change — and it contains **no** Facts, Beliefs or Understanding variant. Cortex's
five layers are an engine-internal model, not families a driver advertises, so
`audit_provider` cannot fire on them however empty they are. If a hosted Cortex
driver carries an over-claim risk it lives in `Ingest`, `Entities`, `Tree` or
`Retrieval`, and a driver plan should name which of those it intends to
advertise and on what evidence.

What the empty layers *would* mean is commercial rather than structural: the
derived fact/belief tier is the reason to prefer Cortex over
`supermemory`/`mem0`/`cognee` at all. Whether it works is now open — the
measurement above did not test it.

### The audit gap that is real, and is not about Cortex

Separately — and this one holds regardless of engine — `audit_provider` compares
`capabilities()` against `provides()`, and **both are properties of the
adapter**. `provides()` is a defaulted trait method with a fixed body:
`Core | Recall | Portability` hardcoded `true`, everything else
`self.as_x().is_some()`. It is a structural Rust-type question, and its own doc
calls it "the implementation-side truth". Neither side asks whether the engine
answers.

So the three **mandatory** families can never fail the audit. Two of them,
`Core` and `Recall`, are what this host's knowledge ports are built on;
`Portability` is mandatory to the *contract* without being exercised by them. The lever is a live
probe or a conformance case, **not** `provides()` — asking `provides()` to
consult the engine would make the audit compare two runtime opinions instead of a
claim against a structure. Tracked as
[#1968](https://github.com/tinyhumansai/opencompany/issues/1968); a boot-time
probe of the mandatory families is proposed in
[#1973](https://github.com/tinyhumansai/opencompany/pull/1973).

## The upsert gap, and the driver mechanics

The contract's `(namespace, key)` upsert has no direct mapping onto an
append-only log, and reconstructing it is most of what a driver does. That
argument, the measured table of failed direct mappings, what the workaround costs
at every call, and the engine behaviours that only appear against a running
instance are in
[`memory-engine-cortex-driver.md`](memory-engine-cortex-driver.md).

Two conclusions from it are load-bearing here. **A conformant driver is
reachable** — appending under a fresh idempotency key and folding to
newest-per-key on read passes the suite against a live engine. And **an upsert
alone would not make Cortex cheap**: keyed reads would still scan and writes
would still wait, because those need a metadata filter and a readiness signal
that are separate asks.

## Belief revision is reachable, and silently wrong

Worth stating separately because it is much of what would justify preferring
Cortex over the drivers we already have. Tested directly: three events
establishing an owner, then two contradicting them.

```text
POST /v1/beliefs/build
{"built":0,"facts_scanned":0,"events_scanned":5,"belief_events_found":0,
 "reasons":{"no_belief_shaped_events":1,"no_facts_in_scope":1}}
```

`no_facts_in_scope` is exactly what an enrichment-off server reports, so this run
tested the [misconfiguration](#correction-2026-09-04), not belief revision. Re-run
correctly, beliefs **do** build — and still do not revise: the contradicted owner
stays live at equal confidence beside the correction, `valid_to: null` on both.
The conclusion below therefore stands, and is worse than "not reachable": it is
reachable, silently wrong, and filed as #2089.

What recall does with the contradiction is adequate *by accident*: the correction
ranks first on semantic similarity, while the superseded claim is still returned
with nothing marking it stale. A `FactStore` consumer would be reading a pack
mixing live and superseded claims with no provenance distinction between them.

## What does work

Ranked retrieval is genuinely good, and it is available from embeddings alone.
In a fresh scope, twelve events, queried twelve seconds after writing and before
any derived layer had built:

- *"who runs mobile releases?"* → `Mobile releases ship every second Wednesday`,
  then `Kai Tanaka manages the mobile release train`
- *"how long do contract reviews take?"* → `Contract reviews take about five
  business days`

Tenant separation also holds *within Cortex*: writes to one scope never
surfaced in another across every test run.

**But the namespace formats are incompatible, and a driver must translate.**
Cortex scopes are slash-delimited `type:id` segments
(`^[a-z][a-z0-9_]{0,31}:[A-Za-z0-9_-]{1,128}(/[a-z][a-z0-9_]{0,31}:[A-Za-z0-9_-]{1,128}){0,31}$`).
Host namespaces are nothing of the sort: `Namespace::company_root` emits
`oc/<slug>-<32 hex>`, children append a plain segment (`…/context`, `…/facts`,
`…/agent/<member>`), and `sanitize_segment` maps every character outside
`[A-Za-z0-9-_]` to `_` — so a colon can never appear and no segment is ever
`type:id`. Passed through unchanged, every store and recall would be rejected.

A `cortex` driver therefore needs an explicit and **reversible** translation.
Reversible is not a nicety: `Bound::recall` re-checks every returned entry with
`Namespace::contains` and drops mismatches, so a return the driver failed to map
back yields zero hits *silently* rather than an error.

So Cortex can back all three knowledge ports today — including `FactStore`.
That needs saying precisely, because the obvious reading is wrong.
`MemoryStore`, `ContextStore` and `FactStore` are **host ports**, not driver
families; a driver can neither advertise nor withhold them. All three are
facades over the same `Bound` helper using only `store`/`get`/`list`/`forget`/
`recall` — `Core` plus `Recall`, both mandatory supertraits.
`FactStore::upsert` is `self.bound.put(company, &fact.id, fact, "fact")`, which
writes operator-curated records into `oc/<slug>-<32 hex>/facts` — the full host
namespace derived from the `CompanyId` — through `MemoryCore::store`. It never
reads a derived facts layer, so Cortex's empty `/v1/facts` does not touch it.

The true statement is narrower, and still supports the conclusion: Cortex cannot
deliver the *derived* fact and belief tier. That is a reason it offers nothing
over `supermemory`/`mem0`/`cognee`, not a reason a port fails.

## Fitting the seam's invariants

`memory-engine.md` makes several properties load-bearing. Three need work
host-side, because Cortex does not provide them:

- **Boot refuses rather than silently degrades.** Cortex does the opposite. With
  no embedding credential it does not refuse: it logs a warning, falls back to
  **mock embeddings**, pins the data directory to `mock::1536`, and reports
  `{"status":"healthy"}`. Recall then returns confident, meaningless results —
  the precise failure the seam's no-fallback rule exists to prevent. **A `cortex`
  driver must probe for the mock provider and refuse the bind itself.**
- **Class is decided by the host.** Unaffected — `remote` pins `External`, and
  nothing about Cortex self-reports class.
- **Credential and endpoint never logged.** Compatible: Cortex's own config
  surface redacts, and the host's `Debug` impls already handle this.
- **Host-owned policy** — archive-on-evict ordering, the scratch firewall,
  `ExternalSync` taint stamping, per-agent and per-desk scoping — all remain
  host-side and are unaffected by the engine choice. Cortex has its own
  provenance/taint notion; it is not the host's and should not be relied on.

## Infrastructure

**Deliberately incomplete.** #1936 asked for HA, backup/restore and an upgrade
path alongside sizing. They are absent by decision: they describe operating a
deployment this record recommends against, and belong with the Phase 0 decision.

Per-instance footprint is the open number. The binary's own config lint projects
**~18 GiB steady-state RAM** on a 3.9 GiB box, and that estimate did not move
under any remediation it suggests — probed across `CORTEX_VECTOR_RESIDENT_MAX`
at 150000 / 20000 / 5000, tenant shards on and off, and `CORTEX_VECTOR_SHARDS` at
8 and 2, all returning ~18 GiB. Idle RSS is ~27 MiB, so the projection is a
ceiling model rather than a floor, but **the real per-instance figure has to come
from Cortex before any capacity plan is credible**. Under instance-per-tenant
that number is multiplied by tenant count, which makes it the dominant cost term.

Other operational notes:

- `CORTEX_VECTOR_TENANT_SHARDS` latches on disk at pool creation
  (`pool_manifest.json`) — it must be set before real data lands.
- Changing embedding provider or dimension re-pins the data directory and
  requires wiping it. Vectors from two providers cannot be mixed.
- Per-call cost is negligible at evaluation scale: embeddings plus LLM lanes
  across ~50 events and several hours of scheduler ticks totalled **$0.000022**.
  Capability, not cost, is the constraint.

## Licensing permits this

Checked against the license text rather than the release README's one-line
summary, which is lossy in a way that matters. **CortexDB Community License
v1.0 clause 2** explicitly allows what this design does:

> The Software may be used to power internal or commercial applications,
> including those sold to third parties, provided that: (a) the third party
> does not access the Software directly as a general-purpose memory database
> (i.e., you may build products on top of CortexDB and sell those products; you
> may not resell CortexDB itself as a service); and (b) attribution to
> "CortexDB" appears in product documentation where reasonable.

Selling OpenCompany with Cortex behind the memory seam is building a product on
top of CortexDB, not reselling CortexDB. Condition (a) is satisfied **by
construction**: tenants reach memory only through the `MemoryProvider` seam, and
the credential and endpoint "never appear in logs, `/healthz`, `/spec`, status
output, or an export" (`storage.md`; `memory-engine.md` states the same rule in
its own words). A tenant cannot address the engine
directly.

Two obligations follow, and both of them are gates:

- **Never hand a tenant its own Cortex endpoint or credential.** Under
  instance-per-tenant that would be easy to do casually — a BYO-engine feature,
  or exposing the URL in a console — and it is precisely what (a) forbids. The
  seam's existing redaction already prevents it, so this is a **provisioning
  gate**: no surface that returns a tenant its own engine URL or key ships
  without re-reading clause 2. Phase 2 owns enforcing it.
- **Attribute CortexDB in product documentation** (clause 2b). A **release
  gate**, not a nicety: the attribution has to exist before the first tenant is
  served, not after someone remembers.

Clause 3 permits mirroring the binary on an internal artifact store for our own
use, which instance-per-tenant provisioning needs. Clause 5 points source access
and cloud-hosted offerings at sales@cortexdb.ai — neither is required here.

## Phased plan

**Phase 0 — decide.** Ratify instance-per-tenant, or accept the weak tier
explicitly and write down why. Licensing is settled (clause 2 permits it); what
remains is the topology decision, which gates everything below.

**Phase 1 — the driver. Done, and unbound.**
A `cortex` driver over `tinymemory-api` implementing the mandatory three —
`Core`, `Recall` and `Portability`. All three are non-negotiable:
`MemoryProvider` declares them as supertraits
(`MemoryCore + MemoryRecall + MemoryPortability`), so a driver missing
`export_page`/`import_records` does not compile, and
`advertised_capabilities()` returns the set unconditionally.

Only `Core` and `Recall` are exercised by the knowledge ports — that is the
whole of what `MemoryStore`, `ContextStore` and `FactStore` need, since all
three are host ports over the same `Bound` helper and no driver advertises or
withholds them individually. `Portability` is not optional scope to defer,
though: it is what Phase 3 runs migration over, and implementing it against an
append-only event log is its own problem rather than a line of glue. What Phase 1
leaves out is Cortex's derived fact and belief tier, which no host port reads and
which Phase 4 revisits.

Merged as [tinymemory#128](https://github.com/tinyhumansai/tinymemory/pull/128)
with a live-engine test lane, and registered here in #2065:
`SUPPORTED_REMOTE_DRIVERS`, `remote_provider()` and the console catalog all carry
`cortex`. Selecting it remains a decision this record argues against; the default
is untouched.

Acceptance, against the list this record set before the work started:

- the full driver conformance suite (tinymemory#18 §E1);
- failure-path tests for error mapping and malformed responses, as the existing
  remote adapters carry;
- **a bind refusal when the engine reports the mock embedding provider**;
- **a live reachability probe** for the mandatory families, with a freshly
  provisioned empty instance probing clean. Note this cannot be expressed
  through `provides()`, which is a fixed-body structural check — the lever is a
  probe or a conformance case (#1968, #1973);
- **key-exact deletion**, verified — and the `deleted.events: 2` alarm (see the
  [driver notes](memory-engine-cortex-driver.md)) was ours. Being precise about what was blocked and by whom, since only one of these
  is a guardrail:

  | Request | Enforced by | Result |
  |---|---|---|
  | `event_ids` (not a schema field) **+ `confirm_all: true`** | nothing | **the scope was wiped** — observed, not simulated |
  | `event_ids` alone | engine | `422 EMPTY_SELECTOR_WITHOUT_CONFIRMATION` |
  | `memory_ids` + `confirm_all: true` | engine | `400 AMBIGUOUS_SELECTOR_CONFIRM_ALL` |
  | `memory_ids` alone | — | exact: `requested: 1, matched: 1, deleted: 1` |

  An unrecognised selector field deserialises as an *empty* selector, meaning the
  whole scope. The engine refuses that on its own, but it cannot refuse it when
  `confirm_all` is also present, because that combination is a valid wipe request.
  So the only thing standing between a typo and a destroyed tenant is driver-side:
  it names `memory_ids` and never sends `confirm_all` anywhere.

Two operational notes. Select it with `OPENCOMPANY_MEMORY=remote` plus
`OPENCOMPANY_MEMORY_DRIVER=cortex` — `OPENCOMPANY_MEMORY=cortex` is still a hard
boot refusal as a **mode** value, left over from #1568. And registering the
driver needed no change to tinymemory's reserved table: `admit` takes an
unreserved id when the host declares the class, and this host declares every
remote driver `External` with `TRUSTED`, so the class stays host-decided rather
than self-reported.

**Phase 2 — provisioning.** Per-tenant instance lifecycle through
opencompany-manager: create, inject `OPENCOMPANY_MEMORY_*` alongside the existing
`OPENCOMPANY_MONGODB_URI`/`_DB` injection, health-probe, back up, destroy. Sizing
waits on a real per-instance figure from Cortex.

**Phase 3 — migration.** `opencompany memory migrate --to cortex` over the
Portability family. The existing runbook in `memory-engine.md` applies unchanged;
its per-tenant-credential caution is satisfied by the instance-per-tenant
topology. Hosted-target enumeration cost still applies.

**Phase 4 — revisit the derived layers.** Previously gated on upstream defects
being fixed; per the [Correction](#correction-2026-09-04) it is now gated on
re-measuring them on a correctly configured server, since whether there is an
upstream defect at all is unproven. That is the point at which Cortex would
offer something the incumbent drivers do not.

## Open questions

- Does CortexDB agree with our reading of clause 2? Worth confirming in writing
  when we contact them, though the text is not ambiguous.
- Is the v1 minter's `scope` advisory rather than enforcing, or is the gap in
  finding 2 a defect? And what should a self-hosted multi-tenant deployment use
  instead — is `cortex-auth-ref` published, or is an external OIDC provider
  expected, against what contract? This decides whether a *shared* instance can
  ever reach the strong tier; instance-per-tenant reaches it without the minter.
  Adoption was decided by the derived-layer finding, which is now withdrawn.
- What is the true per-instance memory floor, from Cortex rather than the lint?
- Will the two filed defects be accepted? The release tracker is scoped to
  binary/packaging issues, with source bugs directed to Cortex Cloud support —
  so a self-hosted deployment's support path is itself unproven.
- Is there an undocumented prerequisite for fact extraction that we missed?
  **Yes — three.** See [Correction](#correction-2026-09-04).
- Will Cortex add an upsert path? **Answered enough to decide on.** No stateful
  key index is needed — append-and-fold works and passes. The question is no
  longer whether a driver is possible but whether its cost is worth paying: a
  scan per keyed read, seconds per write, and superseded values left in the
  ranking corpus.
- Is the embedded engine a fallback? `tinymemory-tinycortex` still exists and
  still passes the full suite, so it is technically viable today with no
  upstream dependency. #1568's PR body records only the mechanical removal and
  no rationale, so whether this is a real option or a closed door is unresolved.
- Self-hosted deployments have no support channel we can reach: the public
  tracker is packaging-only and Cortex Cloud support presumes a customer
  relationship. Worth settling when we contact them about the upsert gap.
- If Facts and Beliefs stay unreachable, does Cortex beat `supermemory` / `mem0`
  / `cognee` on retrieval alone? **No — but the premise is now unproven**
  ([Correction](#correction-2026-09-04)). Its ranked recall is vector search over
  the event log — confirmed on a fresh scope queried before any derived layer had
  built — which is what all three incumbents already provide through this seam,
  without a scan per read or a multi-second write.
