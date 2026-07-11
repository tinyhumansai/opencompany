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

## Brain and cycle terms (Medulla mapping)

Medulla vocabulary is adopted unchanged where it appears; see
[integrations/medulla.md](integrations/medulla.md) for the wire contract.

| Medulla term | Meaning here |
| --- | --- |
| **Tier** | A named cognition class (`orchestrator`, `reasoning`, `frontend`, `compress`, `subconscious`). The client only names a tier; the TinyHumans backend maps tier → model SKU. OpenCompany never selects models. |
| **Compressed history** | ~20:1 summaries of cycle traces — the Company's working memory, persisted through `MemoryStore`. |
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
