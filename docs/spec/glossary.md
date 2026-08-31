# Glossary

This file is the authoritative vocabulary for every document under
`docs/spec/`. Other docs link here on first use instead of redefining terms.

## Core nouns

| Term | Definition |
| --- | --- |
| **Company** | One running instance of a one-person business: Charter + Roster + Brain + Memory + Ledger, hosted by the OpenCompany runtime. |
| **Operator** | The one human who owns a Company. Provides capital, taste, and irreversible decisions. Exactly one per Company. |
| **Platform Operator** | The persona that embeds the `opencompany` crate or hosts many Companies behind a provisioning API. |
| **Brain** | A Company's cognition and the durable state it runs over. Cognition is provided by Medulla through the `Brain` port; the runtime's core job is keeping the brain state consistent and durable. |
| **Company Brain state** | The seven state families: identity, charter, roster, memory, context, world (event log + ledger + approvals), and feedback inbox. |
| **Cycle** | One brain iteration over a batch of events: orchestrate → refine / delegate / dispatch → responses out, memory written back. Medulla term, adopted as-is. Internal-only; never shown to prosumers. |
| **Pass** | One orchestrate↔execute round trip inside a Cycle (Medulla caps at 12). Internal-only. |
| **Dispatch** | The orchestrator's instruction to compile output for one surface. Internal-only. |
| **Teammate** | A Roster member with a mandate (internally: an agent). *Teammate* is the prosumer-facing word. |
| **Roster** | The set of Teammates a Company employs, declared in the manifest (`[[agent]]` entries). |
| **Charter** | The Company's constitution: name, mission, output, services and prices, tone, never-do policies, spend caps, checkpoint overrides. Extends the manifest's `[company]` table. |
| **Template** | A packaged company definition ready to launch — the productized form of the 18 `examples/*` manifests. |
| **Manifest** | The on-disk company definition (`company.toml`, with `agents.toml` accepted for compatibility). |
| **Checkpoint** | A moment that requires human sign-off before an effect executes (spend, send, sign, publish, hire, identity change). |
| **Approval** | The Operator's decision at a Checkpoint: approve, deny, or edit. |
| **Effect** | Any action that touches the world (send a message, call a paid API, spend money, publish). Effects pass the `ApprovalGate` before execution. |
| **Event** | A normalized stimulus entering a Company: operator message, webhook, schedule firing, A2A task, approval resolution, feedback filing. |
| **Engagement** | A paid job between Companies delivered over A2A. |
| **Feedback Item** | A captured "this was wrong" (or thumbs-down) with scrubbed context, optionally filed as a public GitHub issue. |
| **Work Feed** | The prosumer surface listing what the team did, in plain language. |
| **Architect** | The setup-time cognition job that turns an Operator conversation into a tailored Blueprint; invocable post-launch to propose reshaping. Internal-only name. See [agentic/setup.md](agentic/setup.md). |
| **Blueprint** | The Architect's artifact: a complete draft company (manifest + charter + per-decision rationale + provenance), validated before the Operator reviews it at launch. |
| **Manager** | The scheduled cognition job that watches how a Company actually runs and files Change Proposals. Internal-only name; its output surfaces as the company's own suggestions. See [agentic/manager.md](agentic/manager.md). |
| **Change Proposal** | A typed, evidenced, Operator-approvable diff against a Company's effective configuration — the only way any agent changes a running company. See [agentic/proposals.md](agentic/proposals.md). |

## Brain and cycle terms (Medulla mapping)

Medulla vocabulary is adopted unchanged where it appears; see
[integrations/medulla.md](integrations/medulla.md) for the wire contract.

| Medulla term | Meaning here |
| --- | --- |
| **Tier** | A named cognition class (`orchestrator`, `reasoning`, `frontend`, `compress`, `subconscious`). The client only names a tier; the TinyHumans backend maps tier → model SKU. OpenCompany never selects models. |
| **Compressed trace** | One record per cycle, persisted through `MemoryStore`. Named for the ~20:1 working memory it is meant to become; today the `summary` is a constant string and no cycle reads it back. Maintenance retains the newest 32 for inspection (issue #1175). |
| **World-state diff** | Append-only notes about the world uploaded between cycles (`POST /orchestration/v1/world-diff`). |
| **Steering** | A directive synthesized by the subconscious tick that biases future cycles. |
| **Device tool** | A tool the client registers over Socket.IO that the hosted brain calls back into (`orch:tool_call` → `orch:tool_result`). |
| **ContextStore** | The RLM environment: addressable chunks the brain queries lazily (`put`/`list`/`peek`/`search`). |

## Economy terms (tiny.place mapping)

| Term | Meaning here |
| --- | --- |
| **Handle** | The Company's paid `@name` on tiny.place, claimed via `POST /registry/names`. |
| **Wallet** | The Company's Ed25519 keypair; the base58 Solana address of its public key is its `agentId`. The wallet *is* the identity — there are no API keys on tiny.place. |
| **Agent Card** | The public directory listing (skills, capabilities, endpoint, payment requirements) published with `PUT /directory/agents/{id}`. Generated from the Charter's service catalog. |
| **Skill** | A sellable capability priced in x402 USDC on the Agent Card. |
| **A2A** | Agent-to-agent JSON-RPC task delegation (`POST /a2a/{id}`, discovery via `GET /a2a/{id}/skill.md`). |
| **x402** | The HTTP-402 micropayment protocol (USDC on Solana) used to price and settle Skills. |
| **Delegated Signer** | A budget-capped, expiring session key minted from the master wallet so agents can spend without holding the master key. |
| **Ledger** | The Company's money and usage journal: every payment in/out, token spend, signer used, engagement link. Append-only. |
| **SIWX** | Per-action wallet-signature authentication used by tiny.place (no bearer tokens). |

## Orchestration terms

How a many-agent Company converges. See
[runtime/orchestration/](runtime/orchestration/README.md).

> **Naming.** **Ledger** unqualified always means the money and usage journal
> above. The orchestration files below are **derived ledgers** and MUST always
> be named with their qualifier — *demand ledger*, *claim ledger*, *direction
> ledger* — in both prose and code. An unqualified "the ledger" in an
> orchestration context is a defect.

| Term | Meaning |
| --- | --- |
| **Derived ledger** | A Markdown file written by code, never by an agent, re-rendered whole from its sources on every relevant write. A source is either one file per item (`threads.md`) or a fenced block embedded in a workspace note, where one note may hold several items (`claims.md`) — either way the ledger addresses one item at a time, never a whole source file. It cannot drift, because it is a projection rather than a summary somebody maintains. There are three: the claim ledger, the direction ledger, and the demand ledger. |
| **Claim ledger** | The derived ledger of `claim` blocks (`claims.md`): one statement each, with its conditions, whether it holds here, and how well it is established. What closes a Demand. |
| **Direction ledger** | The derived ledger of `thread` blocks (`threads.md`): one open question or dead end each, with its status, what it rests on, and what blocks it. `claims.md` and `threads.md` are what [alignment.md](runtime/orchestration/alignment.md#what-ships) means by "two ledgers" — the two that ship under the alignment layer, distinct from the demand ledger below. |
| **Claim** | One statement the Company holds to be true, with `status` recording how well: `verified`, `sourced`, `asserted`, `heuristic`. A claim citing a Demand id is what closes that Demand. |
| **Demand** | A stated need: what is missing, what the asker would do with it, and what would show their current belief wrong. Stated by whoever is blocked, deduped against what the Company already knows, and closed only by a Claim that cites it. Replaces the board card as the work model. |
| **Demand ledger** | The derived ledger of Demands. The Company's work model; the board columns are a projection of Demand state. Two demands with the same [`DemandId`](runtime/orchestration/demand-ledger.md#demandid-canonicalization), or two differently-worded demands [semantically deduped](runtime/orchestration/demand-ledger.md#semantic-dedup-against-open-demands) against each other, collapse to a single row rather than appearing as two. |
| **Brief** | The single budgeted document (`brief.md`) nearly every reasoning role is given, holding what the Company established, ruled out, and recalled. Exactly one writer. An unreadable brief measures as empty rather than erroring — a deliberate narrow exception to the routed-document hard-error rule, since the brief is wholly machine-derived and machine-consumed. Internal-only. |
| **Context routing** | The per-role decision about which workspace documents enter that role's system prompt. Context is authority; exclusions are deliberate. |
| **Assertion board** | Where a Teammate tells the others something it cannot yet establish — a dead end, a lesson, a hunch. Never an input to a derived ledger. Distinct from the work board. |
| **Attempt** | One try at a Demand. Followed by a concurrent evaluation fan-out — `judge`, `verify`, `critique`, `completeness` — then a routing decision. |
| **Critique** | The evaluation arm that asks what is wrong with or missing from an Attempt's result *as reasoning*, independent of whether verify finds it right: contradictions with the claim ledger, unsupported assertions, insufficient reasoning for the conclusion. Cannot itself route the loop; its objections fold into the judge's `Steer` guidance for the next Attempt. See [loop.md](runtime/orchestration/loop.md#four-questions-one-merge). |
| **Completeness** | The evaluation arm that asks whether an Attempt's result actually answers the Demand's `falsifies`, not merely its topic. A verified result that completeness judges partial routes to `Diversify`, not `Answered`. See [loop.md](runtime/orchestration/loop.md#four-questions-one-merge). |
| **Verdict** | The judge's reading of how an Attempt was *conducted*: `Proceed`, `Steer`, `Restart`. Distinct from verification, which reads whether the result is *right* and alone can end the loop. |
| **Directive** | An Operator instruction queued for a run already in flight. Delivered verbatim into the next Attempt, never blocking. Cannot force a restart or make unverified work count as answered. Internal-only. |

## Legacy / AVI term bridge

The [vision doc](vision/README.md) predates this spec. Its terms map as:

| AVI term | Current term |
| --- | --- |
| Venture | Company |
| Assigned humans | Operator |
| Agent team | Roster of Teammates |
| Venture Orchestrator | Brain (Medulla) |
| Governance / approvals | Checkpoints and Approvals |
| Learning loop | Feedback Loop |
| Knowledge Graph | Memory (future evolution) |
| Signal / Opportunity | Reserved for Stage 3+ features; defined now, unused in the kernel |

## Prosumer translation table (normative)

Prosumer-facing docs and UI text MUST use the right-hand column and MUST NOT
use the left-hand column:

| Internal term | What the Operator sees |
| --- | --- |
| agent, agent graph | your team / a teammate |
| dispatch, pass, cycle | (never shown; describe the work itself) |
| tier, SKU, model routing | (never shown) |
| checkpoint raised | "needs your approval" |
| A2A engagement | "a job from/for another company" |
| effect denied by policy | "blocked by your rules" |
| event log replay | (never shown) |
| Architect, Blueprint | "we'll build your company" / "your company plan" |
| Manager, manager tick | (never shown; suggestions come from "your company") |
| change proposal | "a suggestion from your company" |
