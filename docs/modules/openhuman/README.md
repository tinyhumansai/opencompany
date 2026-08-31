# OpenHuman Module

OpenHuman is the tenant harness, embedded as a **library**. The
`src/harness/` module links `openhuman_core` (`vendor/openhuman`) directly and,
under `feature = "openhuman"`, builds one openhuman `Agent` per manifest
`[[agent]]` through `AgentBuilder`. The default build links none of it and
keeps its offline, echo-brained behaviour.

The builder seams are wired to OpenCompany's own ports:

- **Persona** → each agent gets a system prompt framing it as its manifest
  `role` at the company, built with `SystemPromptBuilder::for_subagent` and
  `omit_identity` so it speaks as that role rather than openhuman's own
  assistant identity.
- **Memory** → `harness::memory::OcMemory`, an openhuman `Memory` over the
  OpenCompany `ContextStore`.
- **Inference provider** → `harness::provider::HostedProvider`, an
  OpenAI-compatible client for the hosted TinyHumans brain (`chat()` sends the
  full history and parses token/cost usage back out), with a `MockProvider`
  for offline tests.
- **Tool policy** → `harness::policy::ApprovalPolicy` still enforces hard
  denials such as `readonly`, but policy-generated HITL is disabled. Approvals
  are raised deliberately through the intrinsic `request_approval` tool.
- **Tools / skills** → injected from the company's manifest grants.

See [`docs/modules/runtime/README.md`](../runtime/README.md) for `HarnessPool`
and [`docs/spec/integrations/openhuman.md`](../../spec/integrations/openhuman.md)
for the full integration contract.

## `HarnessBrain` — cognition on the embedded runtime

`harness::brain::HarnessBrain` implements the `Brain` cognition port over a
`HarnessPool`: each operator message runs one openhuman agent turn and returns
the agent's reply, in place of the offline `EchoBrain`'s `"You said: …"`. A
company routes through it when the `RuntimeBuilder` has both a harness pool
(`with_harness`) and any inference source that resolves at build time, and no
explicit brain — brain precedence is `with_brain` > harness > hosted/echo. The
`opencompany` binary's `attach_harness` resolves the managed default from the
environment (below).

Which brain a company runs is chosen once, when its runtime is built. A company
that resolved **no** inference source at boot is on the offline echo brain and
stays there for as long as that runtime lives, no matter what the console saves
afterwards — a company runtime is built once and cached in the
`CompanyRegistry`. That transition is reported honestly as `restartRequired`
(issue #266) and cleared by rebuilding the runtime in place (issue #290, see
[`docs/spec/runtime/rebuild.md`](../../spec/runtime/rebuild.md)) rather than by
a process restart.

Everything *after* that first transition is live: once a company is on the
harness path, `TenantProvider` re-resolves the effective config — console
runtime override > manifest `[inference]` > managed env default — on every turn,
so a provider switch or key rotation reaches agents on the next turn with no
rebuild at all.

## Explicit approval requests

Every roster agent receives `request_approval` as an intrinsic tool. It takes a
short `title`, a precise yes/no `question`, and optional `context`. Calling it
pushes one `request_approval` effect onto the shared `ApprovalRequestQueue`;
`HarnessBrain` drains and journals that request through the existing approval
inbox. The tool tells the agent to stop the turn and wait.

Resolving the card starts a continuation turn for the requesting agent. Approve
and deny are both delivered as decisions; approval does **not** re-run the
`request_approval` tool. The agent continues (or stops) based on the answer.

Ordinary tools no longer enter HITL because of `[policy].mode`,
`always_approve`, budget thresholds, or per-call judgement. Existing parked
tool-call approvals and their grants remain redeemable during migration. The
`readonly` brake stays a hard denial, not a prompt the operator can override.

## Inference config (environment)

`harness::provider::harness_inference_from_env` resolves the endpoint, key, and
default model, most specific first:

| Value | Source | Fallback |
| --- | --- | --- |
| key | `OPENCOMPANY_INFERENCE_KEY` | `TINYHUMANS_API_KEY` — **no key ⇒ echo brain** |
| url | `OPENCOMPANY_INFERENCE_URL` | `https://api.tinyhumans.ai/openai/v1` |
| model | `OPENCOMPANY_INFERENCE_MODEL` | `chat-v1` |
| window | `OPENCOMPANY_CONTEXT_WINDOW` | `240000` — context window advertised on the managed profile; `off`/`0` disables compression and trimming. Lower it for a smaller model — see [history protection](../runtime/providers.md#history-protection) |

The two key names keep a per-tenant override distinct from the platform-wide
credential the hosting manager injects.

This is the **lowest**-precedence source. A company's own key, set write-only
through the console (`PUT …/inference` with `key`, stored under the
`inference/key` secret), wins over both env names — including on the `managed`
provider, where only the credential changes and the platform endpoint is kept.
Clearing it (`PUT …/inference` with `key: ""`, the console's **Remove key**)
falls back to the env credential rather than 401ing.

## Cost metering

`harness::cost` maps a completed turn's usage onto the ledger and the
`UsageMeter`. `HarnessPool::run` reads the real per-turn token/cost totals from
openhuman's public `Agent::last_turn_usage()` accessor
(tinyhumansai/openhuman#4940), so metering is **live**. Gating differs by
surface: a usage sample is recorded whenever tokens moved (the `/openai/v1`
passthrough reports tokens but bills backend-side, echoing no USD), while a
ledger `inference.spend` entry is written only when the turn actually cost USD —
so a token-bearing zero-cost turn meters usage without a `$0.00` spend line. An
offline provider that reports no usage yields a zero turn, which writes nothing.

## `src/openhuman/` — legacy JSON-RPC path (behind `openhuman-rpc`)

The former out-of-process seam is retained for one release and then removed.
`src/openhuman/` still hosts the launcher (`opencompany open-human
[--mode core|desktop] [--release] [--dry-run]` — Core shells out through Cargo
to `openhuman-core`, Desktop calls `cargo tauri dev`/`build` directly and ports
OpenHuman's `dev:app`/`dev:wry`/`macos:build:release`/`tauri:build:ui` preflight
into Rust: vendored CEF-aware `tauri-cli` install, `CEF_PATH`, `.env` load
(seeded from `.env.example` only in Desktop mode when absent), and macOS
keychain + signing) and the JSON-RPC adapters —
`rpc.rs` (the `OpenHumanRpc` transport trait + `MockOpenHumanRpc`),
`http_client.rs` (the `reqwest` client behind `openhuman-rpc`), `tools.rs`
(`OpenHumanToolProvider`, catalog filtered by manifest grants, ungranted calls
rejected), and `channel.rs` (`OpenHumanChannelAdapter`). It degrades to
built-in tools and the operator channel with a boot warning when OpenHuman is
unreachable — never a boot failure. New work targets the embedded library, not
this path.
