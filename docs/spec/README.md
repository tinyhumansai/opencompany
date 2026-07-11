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
L1  Adapters        hosted-medulla | openhuman-rpc | tinyagents | tinycortex |
                    tinyplace | fs (default)
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
  [company-as-agent/](company-as-agent/README.md) →
  [feedback-loop/](feedback-loop/README.md)
- **Runtime engineering**: [runtime/](runtime/README.md) →
  [company-brain/](company-brain/README.md) →
  [integrations/](integrations/README.md)
- **Where this is going**: [roadmap.md](roadmap.md) →
  [vision/](vision/README.md)

## Index

| Doc | Purpose |
| --- | --- |
| [glossary.md](glossary.md) | Authoritative vocabulary and term bridges |
| [roadmap.md](roadmap.md) | Stages 0–4, phase mapping, non-goals |
| [product/README.md](product/README.md) | Product thesis, personas, surfaces, one-key promise |
| [product/prosumer.md](product/prosumer.md) | Non-technical operator journey end to end |
| [product/platform.md](product/platform.md) | Embed mode and hosted multi-tenant mode |
| [product/templates.md](product/templates.md) | Templates: the productized company manifests |
| [company-brain/README.md](company-brain/README.md) | What the company brain is; the cycle |
| [company-brain/charter.md](company-brain/charter.md) | The company constitution |
| [company-brain/approvals.md](company-brain/approvals.md) | Checkpoints and the approval model |
| [company-brain/memory.md](company-brain/memory.md) | Long-term memory and retention |
| [runtime/README.md](runtime/README.md) | Kernel architecture and crate layout |
| [runtime/ports.md](runtime/ports.md) | Port trait contracts (normative) |
| [runtime/manifest.md](runtime/manifest.md) | `company.toml` schema, `agents.toml` compatibility |
| [runtime/lifecycle.md](runtime/lifecycle.md) | Company state machine and durability |
| [runtime/api.md](runtime/api.md) | HTTP surface and auth model |
| [runtime/config.md](runtime/config.md) | Configuration and the one-key story |
| [company-as-agent/README.md](company-as-agent/README.md) | Companies as economy citizens |
| [company-as-agent/identity.md](company-as-agent/identity.md) | Wallet, handle, Agent Card |
| [company-as-agent/commerce.md](company-as-agent/commerce.md) | Selling, hiring, delegated signers, ledger |
| [integrations/README.md](integrations/README.md) | Reuse-first rule, dependency matrix |
| [integrations/medulla.md](integrations/medulla.md) | Brain contract and the hosted wire protocol |
| [integrations/openhuman.md](integrations/openhuman.md) | OpenHuman seams and upstream PR list |
| [integrations/tinyagents.md](integrations/tinyagents.md) | TinyAgents harness usage |
| [integrations/tinycortex.md](integrations/tinycortex.md) | TinyCortex expectations behind the memory port |
| [integrations/tinyplace.md](integrations/tinyplace.md) | tiny.place protocol integration |
| [feedback-loop/README.md](feedback-loop/README.md) | Feedback capture → GitHub issue → release loop |
| [feedback-loop/privacy.md](feedback-loop/privacy.md) | Redaction rules (normative) |
| [feedback-loop/triage.md](feedback-loop/triage.md) | Labels, triage, closing the loop |
| [vision/README.md](vision/README.md) | The AVI north star (aspirational) |

Module docs under [`docs/modules/`](../modules/) describe the code as it
exists today; this spec describes the target design. When they disagree, the
spec wins for new work.

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
