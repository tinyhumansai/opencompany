# Roadmap

Product stages (what a user can do) map onto engineering phases (what ships in
the crate). Each phase is independently shippable and leaves `main` releasable.
Terms: [glossary.md](glossary.md).

## Stage 0 — Host (today)

The current state: a thin Axum host (`/healthz`, `/spec`, `/tiny`), a clap CLI,
a cargo shell-out launcher for OpenHuman, and 18 example companies whose
`agents.toml` manifests are printed but not executed.

## Stage 1 — Company of One

An Operator boots a real company from a manifest and works with it daily.

- **Phase 0 — Spec + manifest.** This spec; `src/company/manifest.rs` parses
  `company.toml`/`agents.toml` with validation; `opencompany check <dir>`
  lints manifests. Examples stop merely printing TOML.
- **Phase 1 — Kernel + fs store + stub brain.** `src/ports/` traits, file-based
  default stores, `CompanyRuntime`/`CycleRunner`, event log, operator chat
  route backed by a single-call stub `Brain` (via TinyAgents) so the plumbing
  ships and is testable offline.
- **Phase 2 — Hosted Medulla brain.** `HostedMedullaBrain` speaking the
  [/orchestration/v1 wire contract](integrations/medulla.md): HTTP event
  ingestion plus the Socket.IO effect/device-tool channel. Compressed traces
  land in `MemoryStore`; budget ledger starts. Requires a TinyHumans
  credential ([runtime/config.md](runtime/config.md)).
- **Phase 3 — Tools, channels, approvals via OpenHuman.** `ToolProvider` and
  `ChannelAdapter` over JSON-RPC to `openhuman-core serve`; `ApprovalGate`
  mapped to OpenHuman policy tiers; cron schedules; the
  [feedback loop](feedback-loop/README.md) files its first GitHub issues.

## Stage 2 — Public Company

The company earns: it is discoverable and hireable on tiny.place.

- **Phase 4 — tiny.place economy.** `TinyplaceEconomy` adapter (crate
  `tinyplace`): keypair identity, handle claim, Agent Card publish, inbound
  `/a2a/{handle}` with SIWX verification and x402-priced skills, outbound
  hiring under `[budget]` caps, delegated signers.

## Stage 3 — Learning Company

The product improves itself and companies remember.

- **Phase 5 — Platform mode.** Multi-company registry, `POST
  /api/v1/companies` provisioning, per-company auth, sqlite store, the
  operator-pluggable store guide, export/import migration between local and
  hosted.
- **Phase 6 — Memory maturity + alternate brains.** TinyCortex `MemoryStore` /
  `ContextStore` implementations; `SidecarBrain`; feedback triage agent;
  Signals and the Opportunity Engine arrive as a venture-studio Template, not
  kernel code.
- **Phase 7 — Agentic setup + Manager.** The [agentic company](agentic/README.md):
  the Architect's Blueprint flow generalizes the onboarding interview (its
  conversational core can ship as early as Phase 2, since it only needs the
  hosted brain); the Manager tick and the Change Proposal pipeline land on
  top of approvals and the event log. Templates become the Architect's
  priors and the offline fallback.

## Stage 4 — Venture Factory

The [AVI vision](vision/README.md): autonomous opportunity discovery, venture
spawning, compounding knowledge graph. Horizon, not commitment.

## Candidate upstream workstreams

Documented here, executed as PRs against the owning repos — never forked
locally:

- **TinyHumans backend**: API-key authentication for headless hosts (today:
  session JWT only); company-scoped orchestration v2 (multi-company routing,
  richer effects, tool namespacing) — see
  [integrations/medulla.md](integrations/medulla.md).
- **OpenHuman**: headless multi-workspace mode; library-crate split of the
  tool/channel/credential domains; external approval hook; namespaced
  credentials; documented `/events` schema — see
  [integrations/openhuman.md](integrations/openhuman.md).

## Non-goals

- **Not a model host.** Medulla and all model routing are hosted by
  TinyHumans; no local-LLM or BYO-model support.
- **Not a general agent framework.** TinyAgents is the harness; OpenCompany
  grows no graph engine of its own.
- **Not a fork of OpenHuman.** Gaps go upstream as PRs.
- **Not multi-human companies.** Exactly one Operator per Company.
- **Not the AVI venture factory (yet).** No autonomous opportunity discovery
  or venture spawning; `vision/` only.
- **Not custodial finance.** No fiat, no custody beyond the delegated-signer
  model; x402 USDC only.
- **Not a legal-entity service.** Incorporation, tax, and compliance stay
  with the human.
- **No private feedback backend.** Feedback goes to public GitHub issues or
  stays local; there is no telemetry side channel.
- **No prosumer-visible runtime internals.** UI or product text exposing
  "agent graph", tiers, or dispatch is a spec violation
  ([glossary](glossary.md), translation table).
