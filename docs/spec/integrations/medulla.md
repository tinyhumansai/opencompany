# Medulla (the hosted brain)

Medulla is TinyHumans' orchestrator-first cognitive model: a closed-loop
cycle (orchestrate → refine / delegate / dispatch) with guaranteed
termination and at least one response per cycle. The library itself is
TypeScript and import-only; OpenCompany consumes it **as a hosted service**
through the TinyHumans backend, which mounts it at `/orchestration/v1/*` on
`api.tinyhumans.ai`. The Rust runtime plays the same role the OpenHuman
desktop app plays today: the **device client** that ingests events, receives
effects, and services device-tool callbacks.

`HostedMedullaBrain` (`src/brain/hosted.rs`) implements the
[`Brain` port](../runtime/ports.md) over this contract.

## Cognition contract (what the brain guarantees)

- One cycle per wake: orchestrator tier reviews everything, loops
  refine/delegate/dispatch, pass ceiling 12.
- Tiers name cognition classes only — `orchestrator`, `reasoning`,
  `frontend`, `compress`, `subconscious`. The **server** maps tier → SKU
  (today: frontend→`chat-v1`, reasoning→`agentic-v1`, compress→`burst-v1`,
  subconscious→`reasoning-v1`; billed against the opaque `orchestrator-v1`
  SKU). The client can never select a model: any request body containing a
  `model` field is rejected with `400 ORCH_MODEL_NOT_ALLOWED`.
- Working memory is compressed ~20:1 server-side per session; steering
  directives from the subconscious tick bias future wakes.

## Wire contract, v1 (as implemented today)

### Envelope

- Every request body carries `"protocol": 1` (server supports range [1,1];
  mismatch → `409 ORCH_PROTOCOL_MISMATCH` with `{min,max}`).
- Success: `{ "success": true, "data": … }`. Error:
  `{ "success": false, "error", "errorCode"?, "details"? }`.
- Error codes: `ORCH_PROTOCOL_MISMATCH`, `ORCH_MODEL_NOT_ALLOWED`,
  `ORCH_VALIDATION_ERROR`, `ORCH_INSUFFICIENT_BALANCE`, `ORCH_RATE_LIMITED`,
  `ORCH_UPSTREAM_MODEL_ERROR`, `ORCH_INVALID_STATE`, `ORCH_DEVICE_OFFLINE`,
  `ORCH_EXECUTE_TIMEOUT`.
- Auth: `Authorization: Bearer <credential>` on HTTP; the same credential as
  the Socket.IO handshake `auth.token`
  ([runtime/config.md](../runtime/config.md) — session JWT today, API key
  as the contract).

### HTTP endpoints

**`POST /orchestration/v1/events`** — ingest an event; the wake trigger.

```json
{
  "protocol": 1,
  "counterpartAgentId": "string(1..256)",
  "sessionId": "string(1..256)",
  "event": {
    "seq": 0,
    "role": "user|assistant|system",
    "sender": "string(1..256)",
    "body": "string(≤200000, plaintext)",
    "ts": 0,
    "kind": "string(1..64)"
  }
}
```

→ `202 { "data": { "accepted": true, "cycleId": "cyc:<counterpart>:<session>:<seq>" } }`.
Idempotent on `(user, counterpartAgentId, sessionId, seq)`. The reply is
**not** in the HTTP response — the wake fires out-of-band and results arrive
over Socket.IO.

**`POST /orchestration/v1/world-diff`** — upload world-state notes (feeds
the subconscious tick):
`{ protocol, sessionId, entries: [{seq, note(≤8000), ts}] (1..500) }` →
`202 { accepted, duplicates, tickScheduled }`.

**Read surface** (all `{ success, data }`):

```text
GET /orchestration/v1/sessions                      SessionSummary[]
GET /orchestration/v1/sessions/:id/messages?after=  MessageView[] (seq asc, page 200)
GET /orchestration/v1/sessions/:id/state            { sessionId, status, lastSeq, lastCycleId? }
GET /orchestration/v1/steering                      { active|null, history }
GET /orchestration/v1/world-diff?session=<id>       WorldDiffView[]
```

### Socket.IO channel (effects + device tools)

Default namespace, credential in handshake `auth.token`.

- **Effects out**: events named `orch:effect:<kind>` (e.g.
  `orch:effect:send_dm`), frame `{ cycleId, callId, …payload }`. Delivery is
  **at-least-once** — pending effects replay on reconnect; the client MUST
  dedupe on `callId` (deterministic `{cycleId}:{kind}:{index}`).
- **Acks**: client emits `orch:effect:result`
  `{ callId, ok, error?, result? }`.
- **Device tools** (client-provided tools the brain can call):
  1. On connect, register the manifest: `orch:register_tools`
     `{ tools: [{ name, description?, inputSchema? }] }`.
  2. Mid-cycle the server emits `orch:tool_call`
     `{ cycleId, callId, name, args, timeoutMs }` (~30 s timeout).
  3. Client answers `orch:tool_result` `{ callId, ok, result?, error? }`.
  Cloud tools (server-side) win over device tools on name collision; unknown
  tools are rejected.
- **Usage out** (issue #174): `orch:usage`
  `{ cycleId, callId, inputTokens, outputTokens, cachedInputTokens?, costUsd? }`,
  one frame per model pass. The host never touches the model on this path, so
  this frame is the **only** way it learns what a cycle cost; without it the
  hosted path is structurally unmetered and the console's Usage view reads zero
  however much inference ran. Nothing is acked — usage is a report, not a
  request. Delivery is at-least-once like every other frame, so `callId` is
  deterministic (`{cycleId}:usage:{index}`) and the client dedupes on it rather
  than double-charging the meter.
  The frame is **advisory and additive**: a backend that does not emit it yet is
  unaffected — the client folds whatever arrives (nothing ⇒ a zero total, metered
  as nothing rather than guessed at), `cachedInputTokens` defaults to `0`, and an
  omitted `costUsd` means "tokens moved, billing happens off the wire", which
  still records tokens but posts no `$0.00` spend line to Finances.

## How `HostedMedullaBrain` maps the kernel onto v1

| Kernel concept | v1 mapping |
| --- | --- |
| `CompanyId` | one session per company: `sessionId = company ULID`, `counterpartAgentId = "opencompany:<slug>"`. One TinyHumans account may host many companies as many sessions. |
| `CycleRequest.events` | one `POST /v1/events` per normalized event, seq from the `EventLog` sequence |
| `CycleResult` channel responses | `orch:effect:send_dm` frames, acked then routed to `ChannelAdapter`s |
| `CycleHost::call_tool` | device-tool round-trip: the runtime registers the company's granted tool catalog via `orch:register_tools`; `orch:tool_call` → `ToolProvider::invoke` → `orch:tool_result` |
| `CycleHost::context_op` | exposed as device tools (`context_put`, `context_search`, …) until v2 adds first-class context ops |
| `CycleHost::emit_effect` | every effect frame passes the `ApprovalGate` **before** acking `ok`; parked effects ack `ok: false` with a "pending approval" error so the brain hears the gate |
| `CycleResult.token_usage` | `orch:usage` frames folded into the cycle total, then metered by `CycleRunner` onto the Usage surface (`SampleKind::Inference`, provider `medulla`) and, when the frame carries USD, onto Finances as an `inference.spend` ledger entry. Per-teammate attribution is not on the wire, so hosted usage is charged to the whole-company bucket (`company`) |
| `MemoryStore` | mirrors the read surface (`/sessions/:id/messages`) plus locally-journaled cycle summaries; server keeps its own compressed state |
| World state | `POST /v1/world-diff` after notable local effects (approvals resolved, payments, feedback) |

Constraints inherited from v1: the 30 s device-tool timeout means long tools
must return a handle and complete asynchronously (result uploaded as a
follow-up event); effect delivery requires a live socket, so the runtime
maintains a persistent reconnecting connection per account and marks the
company `paused` with an operator notice if the backend is unreachable for
long periods.

## Proposed v2 (candidate backend workstream)

v1 is per-user and chat-session-shaped. A company-scoped v2 would add — as
PRs against the backend, tracked in [roadmap.md](../roadmap.md):

1. **API-key auth** for headless hosts (issuance + scoping), fulfilling the
   `TINYHUMANS_API_KEY` contract.
2. **Company-scoped routing**: first-class `companyId` on events, sessions,
   and effect frames; server-side multiplexing of many companies over one
   socket with per-company ordering.
3. **Richer effect vocabulary**: beyond `send_dm` — typed effects for
   publish, payment intents, and approval-aware dispositions so the gate
   result is a first-class protocol state rather than an error-shaped ack.
4. **First-class context ops**: `context_put/list/peek/search` as protocol
   frames instead of device tools, aligning the wire with Medulla's
   `ContextStore` port.
5. **Tool namespacing + long-running tools**: per-company tool manifests,
   call handles that outlive the 30 s window.
6. **Orchestrator-tier events**: expose pass/dispatch telemetry so the work
   feed can narrate progress without scraping messages.

## Modes other than hosted

- **`SidecarBrain`** (feature `sidecar`): a thin Node process embedding
  `@tinyhumansai/medulla-v1` directly, speaking the same frames over local
  HTTP/stdio; its `InferenceClient` calls back into the Rust host, which
  proxies to any provider through the TinyAgents harness. For self-hosters
  who bring their own inference.
- **`NativeBrain`** (far future): a TinyAgents graph port of `runCycle`.
  Interface only; no commitment.

## Degraded mode

Without a TinyHumans credential the brain is unavailable; the runtime still
builds, validates, inspects, and serves read-only routes
([runtime/config.md](../runtime/config.md)).
