# OpenHuman

OpenHuman (vendored at `vendor/openhuman`, written against v0.58.x) is the
local-first personal-AI product: a Rust core with a Tauri/React shell. For
OpenCompany it is the **preferred backend for tools, channels, credentials,
and policy** — roughly 60 mature domains (memory, threads, channels,
subconscious, routing, providers, tools, skills, cron, workflows, wallet,
security, people, embeddings, …) that the kernel should consume, not copy.

## Integration: embedded as a library (current)

OpenHuman is now consumed as an **embeddable Rust library**, not an
out-of-process daemon. The `src/harness/` module links `openhuman_core`
directly and, under `feature = "openhuman"`, builds one openhuman
[`Agent`] per manifest `[[agent]]` through
[`AgentBuilder`](../../modules/openhuman/README.md). The builder's seams are
wired to OpenCompany's own ports:

- **Memory** → `OcMemory`, an openhuman `Memory` implemented over the
  OpenCompany [`ContextStore`](../runtime/ports.md).
- **Inference provider** → the hosted Medulla `Provider` (a `MockProvider`
  stands in for offline tests).
- **Approval policy** → `ApprovalPolicy` maps the manifest `[policy].mode`
  onto openhuman's `ToolPolicy`; the security-tier words
  (readonly/supervised/full) line up 1:1, which is why the manifest reuses
  them.
- **Tools / skills** → injected through the builder's tool/skill seams from the
  company's manifest grants.

The default build links **none** of this and keeps its offline, echo-brained
behaviour. When the `openhuman` feature is off, tool/channel behaviour degrades
to built-ins and the operator channel — never a boot failure
([runtime/config.md](../runtime/config.md)).

**Realized upstream candidate #2 (library-crate split).** Embedding
`openhuman_core` directly is exactly the "expose the domains as an embeddable
crate so co-located hosts can link instead of RPC" workstream below —
delivered, not pending.

### Cost metering seam (partial — pending openhuman#4940)

openhuman surfaces a completed turn's token/cost totals only through a
`pub(crate)` accessor (`Agent::take_last_turn_usage_totals`), so a host crate
cannot read the real `TurnCost` after `turn()`. The harness cost mapping
(`TurnCost` → ledger + [`UsageMeter`](../runtime/ports.md)) is complete and
tested, but until the **public turn-usage accessor**
(tinyhumansai/openhuman#4940) lands, `HarnessPool::run` records a **zero-usage
turn** — which, per the cost contract, writes nothing. Usage/Finances token and
cost numbers are therefore structurally correct but empty of real inference
cost until that PR is the seam for real metering.

### Group-chat / desk routing

openhuman is single-agent; desk (group-chat) routing is OpenCompany's job. v1
is single-responder. The full ops `chat` handler that resolves a desk's members
and journals the reply — including approval **resume** on a follow-up cycle —
lives in the WS3 chat handler, not inline in the harness.

## Legacy: JSON-RPC launcher/wire path

The former out-of-process seam is retained for one release behind
`feature = "openhuman-rpc"` and is then removed:

- **Process**: the launcher (`opencompany open-human [--dry-run]`) shells out
  through Cargo to `openhuman-core` (Core) or the Tauri app (Desktop);
  `OPENCOMPANY_OPENHUMAN_URL` attaches to a running `openhuman-core serve`.
- **Wire**: JSON-RPC at `http://127.0.0.1:<port>/rpc` (methods
  `openhuman.<namespace>_<function>`, per-launch bearer) plus REST
  `GET /health`, `GET /schema`, `GET /events`; `OpenHumanToolProvider` /
  `OpenHumanChannelAdapter` adapt it to the `ToolProvider`/`ChannelAdapter`
  ports.

New work targets the embedded library; the RPC path takes no new features.

## Desktop story

The Tauri app is a natural prosumer install path: OpenHuman as the shell,
OpenCompany as the company runtime behind it. Whether the prosumer UI ships
as an OpenHuman mode or a separate frontend is an open product question
([product/prosumer.md](../product/prosumer.md)); the runtime API is the same
either way.

## Upstreaming policy

Glue that adapts OpenHuman's RPC to kernel ports lives in OpenCompany.
Anything that changes OpenHuman behavior goes upstream. Candidate PRs
identified so far:

1. **Headless multi-workspace mode** — `openhuman-core serve` today serves
   one local persona; a `--workspace <id>` scope (or workspace param on RPC
   methods) would let one daemon serve N companies.
2. **Library-crate split** — *realized.* The tool/channel/credential/policy
   domains are consumed as the embeddable `openhuman_core` crate; the harness
   links them instead of speaking RPC.
3. **Public turn-usage accessor** (tinyhumansai/openhuman#4940) — expose the
   completed turn's token/cost totals publicly (today `pub(crate)`) so a host
   crate can read the real `TurnCost` after `turn()` and feed real cost
   metering. This is the seam the harness cost hook waits on.
4. **External approval hook** — policy tiers currently resolve in-app; a
   webhook/RPC callback would let OpenCompany's `ApprovalGate` be the
   resolver of record.
5. **Namespaced credentials** — per-workspace credential scoping so company
   A's secrets are invisible to company B.
6. **Documented `/events` schema** — the REST event stream exists but has no
   stable documented schema for external consumers.
7. **Brain-protocol port** — make OpenHuman's own orchestration loop
   pluggable so an OpenHuman instance could delegate cognition to a hosted
   Medulla brain (the inverse of our integration).
