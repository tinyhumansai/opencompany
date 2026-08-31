# OpenCompany System Specification

OpenCompany is the open-source runtime that turns one person into a whole
company. A single human operator brings capital, taste, and judgment; a roster
of AI teammates does every functional job. The runtime keeps each company's
**brain** — its charter, roster, memory, ledger, and pending approvals —
durable and consistent, drives it with **Medulla** (TinyHumans' hosted
orchestrator-first model), and makes every company a first-class, discoverable
citizen of the **tiny.place** agent economy.

Two personas are served by the same crate:

- **Prosumer operator** — a non-technical person running a one-person
  business. Installs one binary, pastes one key (`TINYHUMANS_API_KEY`), picks
  a template, and goes live.
- **Platform operator** — a builder embedding the crate or hosting fleets of
  one-person companies behind a provisioning API.

One invariant binds everything: **the only mandatory external dependency is
the TinyHumans API key.** Storage is DB-agnostic behind ports, tiny.place is
opt-in, and every integration degrades gracefully.

## Layered Architecture

Dependencies point strictly downward. OpenCompany owns the kernel; every
neighbor sits behind a Rust trait ("port") and is swappable.

```text
L4  Surfaces        Axum HTTP (operator API, A2A, webhooks), CLI, future UI
L3  Company Brain   cycle loop, approvals, effect routing, feedback loop
L2  Kernel ports    Brain, CompanyStore, EventLog, MemoryStore, ContextStore,
                    ChannelAdapter, ToolProvider, AgentEconomy, ApprovalGate
L1  Adapters        hosted-medulla | openhuman-rpc | tinyagents |
                    hosted-memory | tinyplace | fs (default)
L0  Substrate       api.tinyhumans.ai, openhuman-core, tiny.place, filesystem
```

| Concern | Owner | OpenCompany's role |
| --- | --- | --- |
| Cognition (orchestrate / delegate / dispatch) | Medulla | called via the `Brain` port; never reimplemented |
| Model access, tier→SKU mapping, billing | TinyHumans backend | sends tier names + credential; never sees SKUs |
| Tools, channels, credentials, policy tiers | OpenHuman | consumed via JSON-RPC; gaps go upstream as PRs |
| In-process LLM sub-work | TinyAgents | embedded library behind `ToolProvider` |
| Long-term memory | TinyCortex (candidate) | behind `MemoryStore`; default is file-based |
| Identity, discovery, payments, A2A | tiny.place | behind `AgentEconomy` |
| Company definition, brain state, lifecycle, approvals, HTTP surface | **OpenCompany** | owned outright |

## Reading Paths

- **Product / UX**: [product/](product/README.md) →
  [agentic/](agentic/README.md) →
  [company-as-agent/](company-as-agent/README.md) →
  [feedback-loop/](feedback-loop/README.md)
- **Runtime engineering**: [runtime/](runtime/README.md) →
  [company-brain/](company-brain/README.md) →
  [integrations/](integrations/README.md)
- **Security**: [security/agent-isolation.md](security/agent-isolation.md) —
  read this before assuming any agent capability is contained
- **Where this is going**: [roadmap.md](roadmap.md) →
  [feature audit](feature-audit/README.md) →
  [vision/](vision/README.md)

## Index

| Doc | Purpose |
| --- | --- |
| [glossary.md](glossary.md) | Authoritative vocabulary and term bridges |
| [roadmap.md](roadmap.md) | Stages 0–4, phase mapping, non-goals |
| [feature-audit/README.md](feature-audit/README.md) | Draft feature families to enrich into future implementation specs |
| [product/README.md](product/README.md) | Product thesis, personas, surfaces, one-key promise |
| [product/prosumer.md](product/prosumer.md) | Non-technical operator journey end to end |
| [product/platform.md](product/platform.md) | Embed mode and hosted multi-tenant mode |
| [product/templates.md](product/templates.md) | Templates: the productized company manifests |
| [agentic/README.md](agentic/README.md) | The agentic company: design, run, evolve — agents propose, the Operator disposes |
| [agentic/setup.md](agentic/setup.md) | Agentic setup: the Architect and Blueprints |
| [agentic/manager.md](agentic/manager.md) | The Manager: continuous-fit loop and its fence |
| [agentic/proposals.md](agentic/proposals.md) | Change Proposal schema, lifecycle, provenance (normative) |
| [company-brain/README.md](company-brain/README.md) | What the company brain is; the cycle |
| [company-brain/charter.md](company-brain/charter.md) | The company constitution |
| [company-brain/approvals.md](company-brain/approvals.md) | Checkpoints and the approval model |
| [company-brain/grants.md](company-brain/grants.md) | Grants and the tool gate: single-use, standing, tiers, precedence |
| [company-brain/per-call-judgement.md](company-brain/per-call-judgement.md) | Which calls warrant a human, per call (step 7 of the gate) |
| [company-brain/memory.md](company-brain/memory.md) | Long-term memory and retention |
| [runtime/README.md](runtime/README.md) | Kernel architecture and crate layout |
| [runtime/ports.md](runtime/ports.md) | Port trait contracts (normative) — index, assembly, defaults |
| [runtime/ports-cognition.md](runtime/ports-cognition.md) | `Brain`, `CycleHost`, `ChannelAdapter`, `TurnStep` |
| [runtime/ports-state.md](runtime/ports-state.md) | `CompanyStore`, `EventLog`, memory/context, secrets, identity |
| [runtime/ports-effects.md](runtime/ports-effects.md) | `ToolProvider`, `AgentEconomy`, `ApprovalGate` |
| [runtime/ports-console.md](runtime/ports-console.md) | The WS3 console-surface stores |
| [runtime/ports-runs.md](runtime/ports-runs.md) | `RunStore`: attempts and their traces |
| [runtime/events.md](runtime/events.md) | `CompanyEvent` vocabulary + journal correlation rules |
| [runtime/manifest.md](runtime/manifest.md) | `company.toml` schema, `agents.toml` compatibility |
| [runtime/harnesses.md](runtime/harnesses.md) | Named execution engines: `built_in` vs `acp`, transports, per-agent binding |
| [runtime/harnesses-acp.md](runtime/harnesses-acp.md) | The ACP transports in detail: `local` vs `runner`, readiness probing, resuming a teammate's session across a restart, and streaming its execution state while the turn runs |
| [runtime/providers.md](runtime/providers.md) | Inference providers, dual-mode OpenRouter, per-harness credentials |
| [runtime/globals.md](runtime/globals.md) | The global baseline every company gets: agents, workflows, skills, the starting tool belt, and `[globals].disable` |
| [runtime/lifecycle.md](runtime/lifecycle.md) | Company state machine and durability |
| [runtime/planning.md](runtime/planning.md) | The Planning station: pass contract, prerequisite verdicts, boot sweep |
| [runtime/ledgers.md](runtime/ledgers.md) | Dynamic ledgers: declared record shapes, the append-only fold, who may delete, the `derived/` folder |
| [runtime/ledger-statuses.md](runtime/ledger-statuses.md) | How many statuses a ledger may declare, the board's phase/stage split, and how a retired status word heals |
| [runtime/ledgers-console-ia.md](runtime/ledgers-console-ia.md) | The console surface over ledgers: naming ("ledger" is internal-only), per-list sidebar rows, Manage Lists, the declare wizard |
| [runtime/pages.md](runtime/pages.md) | Agent-authored internal dashboard pages: the `pages/<slug>/` convention, the compile-on-write contract, and the two-part isolation model |
| [runtime/orchestration/README.md](runtime/orchestration/README.md) | Making a many-agent company converge: the three collapses, the three principles, phasing |
| [runtime/orchestration/memory.md](runtime/orchestration/memory.md) | One memory contract: `MemoryProvider` replaces three ports, and the host decorator that keeps tenants apart |
| [runtime/orchestration/context-routing.md](runtime/orchestration/context-routing.md) | Which workspace documents reach which role's prompt, the load-bearing exclusions, and assembly order |
| [runtime/orchestration/alignment.md](runtime/orchestration/alignment.md) | The budgeted brief, the derived ledgers, the assertion board |
| [runtime/orchestration/demand-ledger.md](runtime/orchestration/demand-ledger.md) | The demand ledger as the work model (normative): dedup, closure by evidence, the column projection |
| [runtime/orchestration/loop.md](runtime/orchestration/loop.md) | The attempt loop: the evaluation fan-out, judge vs verify, routing, and the mandatory parity sweep |
| [runtime/orchestration/delegation.md](runtime/orchestration/delegation.md) | The join primitive, operator directives, and collapsing desks into workflows |
| [runtime/orchestration/sandbox.md](runtime/orchestration/sandbox.md) | Containerised programming tools: posture, placement, the code library, checkpointing |
| [runtime/api.md](runtime/api.md) | HTTP surface and auth model |
| [runtime/config.md](runtime/config.md) | Configuration and the one-key story |
| [runtime/search.md](runtime/search.md) | Web search: the managed surface, a company's own provider, the gates, and who is billed |
| [runtime/data-root.md](runtime/data-root.md) | Data-root resolution, the single-writer lock, instance identity |
| [runtime/desktop.md](runtime/desktop.md) | The desktop client: connections, transport seam, embedded host |
| [runtime/desktop-instances.md](runtime/desktop-instances.md) | Several local hosts on one machine: the roster, onboarding, dev runs |
| [runtime/connectors.md](runtime/connectors.md) | Connectors: choosing where the runtime runs — this computer, TinyHumans Cloud, a remote gateway, or over SSH |
| [runtime/offline.md](runtime/offline.md) | Running with no network: the configuration, what is not local, and the CI lane that proves it |
| [runtime/analytics.md](runtime/analytics.md) | Product analytics: hosted tenants only, opaque identity, shape-not-content payloads, and the switch that turns it off |
| [runtime/hub-console.md](runtime/hub-console.md) | One console deployment operating many hosts on other origins |
| [security/agent-isolation.md](security/agent-isolation.md) | What confines an agent and what does not — enforced controls, the gaps, and the capability that survives every planned control |
| [company-as-agent/README.md](company-as-agent/README.md) | Companies as economy citizens |
| [company-as-agent/identity.md](company-as-agent/identity.md) | Wallet, handle, Agent Card |
| [company-as-agent/commerce.md](company-as-agent/commerce.md) | Selling, hiring, delegated signers, ledger |
| [integrations/README.md](integrations/README.md) | Reuse-first rule, dependency matrix |
| [integrations/medulla.md](integrations/medulla.md) | Brain contract and the hosted wire protocol |
| [integrations/openhuman.md](integrations/openhuman.md) | OpenHuman seams and upstream PR list |
| [integrations/tinyagents.md](integrations/tinyagents.md) | TinyAgents harness usage |
| [integrations/tinyplace.md](integrations/tinyplace.md) | tiny.place protocol integration |
| [feedback-loop/README.md](feedback-loop/README.md) | Feedback capture → GitHub issue → release loop |
| [feedback-loop/privacy.md](feedback-loop/privacy.md) | Redaction rules (normative) |
| [feedback-loop/triage.md](feedback-loop/triage.md) | Labels, triage, closing the loop |
| [vision/README.md](vision/README.md) | The AVI north star (aspirational) |
| [../brand/README.md](../brand/README.md) | Brand guideline: positioning, voice, colour, form |
| [../design-system/README.md](../design-system/README.md) | Design system: tokens, type, components (normative for the console) |

Module docs under [`docs/modules/`](../modules/) describe the code as it
exists today; this spec describes the target design. When they disagree, the
spec wins for new work.

The brand and design-system docs are the exception to that split: they describe
what the console ships *today*, because their source of truth is a stylesheet
(`frontend/src/index.css`) and a page that renders it (`#/styleguide`).

## Conventions

- Every Markdown file stays at 500 lines or fewer; topics that outgrow a file
  split into a directory with a `README.md` entrypoint.
- MUST / SHOULD / MAY carry their RFC 2119 meanings in normative sections.
- [glossary.md](glossary.md) is authoritative for vocabulary; docs link terms
  on first use rather than redefining them.
- Prosumer-facing language rules in the glossary are normative: product docs
  and UI text never expose runtime internals ("agent graph", "tier",
  "dispatch", "cycle").

## Design Goals

- Make simple company workflows concise; make complex workflows explicit,
  inspectable, and testable.
- Reuse Medulla, OpenHuman, TinyAgents, TinyCortex, and tiny.place instead of
  reimplementing them; changes those layers need go upstream as PRs.
- Keep the default build small; deeper integrations are feature-gated.
- One required credential; everything else optional and gracefully degrading.
- Keep docs, examples, and public APIs aligned.
