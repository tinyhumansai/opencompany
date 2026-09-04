# Runtime

The runtime is the part of OpenCompany that is owned outright: the kernel
that keeps each [Company](../glossary.md)'s brain state durable, runs cycles
against the [Brain](../integrations/medulla.md), gates effects through
approvals, and serves the HTTP surface. Everything that touches a neighbor
system sits behind a port trait.

Supporting docs:

- [ports.md](ports.md) — the port trait contracts (normative): the index over
  the five files below, plus the runtime assembly and the default
  implementation of each port
  - [ports-cognition.md](ports-cognition.md) — `Brain`, `CycleHost`,
    `ChannelAdapter` and the `TurnStep` activity trace
  - [ports-state.md](ports-state.md) — `CompanyStore`, `EventLog`,
    `MemoryStore`, `ContextStore`, `SecretStore`, and the identity trio
  - [ports-effects.md](ports-effects.md) — `ToolProvider`, `AgentEconomy`,
    `ApprovalGate`
  - [ports-console.md](ports-console.md) — the WS3 console-surface stores
    - [ports-console-workspace.md](ports-console-workspace.md) — `WorkspaceStore`,
      the Obsidian-style note tree, its binary half, folder claims, and the
      system workspace roots
  - [ports-runs.md](ports-runs.md) — `RunStore`: one attempt at a task, its
    trace, and who writes it
  - [journal.md](journal.md) — `JournalStore`: the runtime journal's durable
    sink (at-most-once effect keys, parked approvals, grants, cycle brackets),
    the per-backend shapes, and the one-time receipt-gated import off the old
    `journal.jsonl` (issue #726)
- [storage.md](storage.md) — how a backend is chosen at boot, the shipped
  backends, and the conformance suite that pins them to identical answers
  - [workspace-layout.md](workspace-layout.md) — the on-disk layout inside the
    data root: the embedded runtime's root, the agent sandboxes, choosing the
    root, and migrating a legacy doubled install
  - [workspace-names.md](workspace-names.md) — the one naming rule for
    everything the runtime puts in a workspace (lowercase, dashed), where it is
    applied, and how a company created before it keeps working
  - [memory-engine.md](memory-engine.md) — the `OPENCOMPANY_MEMORY` overlay and
    why an ephemeral data root refuses to boot
  - [data-root.md](data-root.md) — the root itself: resolution order, ownership,
    and two processes wanting the same directory
  - [offline.md](offline.md) — running with no network at all: the documented
    manifest, what stays hosted (Medulla, Composio, the hub identity exchange),
    and the CI lane that executes the claim inside a network namespace
  - [analytics.md](analytics.md) — what the product reports about its own use:
    hosted tenants only, an opaque id, shape-and-outcome payloads that cannot
    structurally carry content, and how to turn it off
- [events.md](events.md) — the `CompanyEvent` vocabulary those ports carry, and
  the run/task/approval correlation rules a journal reader folds on
  - [workflow-events.md](workflow-events.md) — the workflow-run progress
    brackets (`WorkflowRunStarted` / `WorkflowNodeStarted` /
    `WorkflowNodeFinished` / `WorkflowRunFinished`), run-id correlation, the
    interrupted-run sweep, and operator stop/cancel semantics (issues
    #371/#382/#383/#398)
  - [events-settle-marker.md](events-settle-marker.md) — the card-linked
    marker a settled dispatch leaves in the conversation that raised it: the
    captured origin (channel and, since #1890, thread), why `None` means no
    conversation rather than the General desk, and the identity dedupe that
    keeps the live line and its rehydrated twin one line (issues #377/#1890)
- [artifacts.md](artifacts.md) — what makes something a deliverable: the
  explicit-publish rule, `(task, source)` identity, body caps and reference
  bodies, and the single follow-up nudge
- [manifest.md](manifest.md) — `company.toml` schema,
  with [manifest-semantics.md](manifest-semantics.md) for each key's behaviour
- [globals.md](globals.md) — the global baseline: the agents, workflows, skills
  and starting tool belt every company gets whichever vertical it started from,
  how a company supersedes or disables one, and why provenance is persisted
- [agents.md](agents.md) — how a teammate is declared: the inline `[[agent]]`
  form and the one-file-per-teammate `agents/<id>.toml` bundle form, custom
  prompts, checked-in briefing documents versus routed workspace documents, and
  the `classes` routing exclusions
- [tools.md](tools.md) — the three-level tool grant
  (`[tools].allow ∩ desk.tools ∩ agent.tools`), why an absent grant means
  "inherit" rather than "nothing" (and why an explicit empty agent grant is a
  deny-all since #1804), the four namespaces `*` never confers, the
  unified tool catalog, and the seed-wins rule for console desk overrides
- [lifecycle.md](lifecycle.md) — company state machine and durability
- [planning.md](planning.md) — the board's Planning station: one tool-less model
  call per card, the host-gathered evidence pack, the prerequisite verdict
  taxonomy, and the no-run/no-lock concurrency argument
- [orchestration/](orchestration/README.md) — how a many-agent company
  *converges*: per-role context routing, a budgeted shared brief, code-derived
  ledgers, the demand ledger that replaces the board, the attempt loop, the join
  primitive, and containerised code tools. Also the three entities this removes
  — the kanban board as the work model, desks, and two of the three memory
  backends
  - [orchestration/memory.md](orchestration/memory.md) — `MemoryProvider`
    replacing the bespoke `CortexClient` backend, with `MemoryStore`,
    `ContextStore` and `FactStore` kept as typed facades over the one
    provider rather than as three independent backends, and why the host
    decorator is the only safe constructor
  - [orchestration/context-routing.md](orchestration/context-routing.md) — what
    each role is told, why the exclusions matter as much as the entries, and
    why assembly order is a prompt-cache decision
  - [orchestration/alignment.md](orchestration/alignment.md) — the budgeted
    brief, and the ledgers that are derived rather than asserted
  - [orchestration/demand-ledger.md](orchestration/demand-ledger.md) — work
    stated by whoever is blocked, deduped, and closed by evidence that cites it
    (normative)
  - [orchestration/loop.md](orchestration/loop.md) — attempt → evaluate → route,
    and the parity sweep that holds the Rust ladder and its jq translation
    together
  - [orchestration/delegation.md](orchestration/delegation.md) — awaiting
    delegated work, directing a run in flight, and desks as workflows
  - [orchestration/sandbox.md](orchestration/sandbox.md) — the container
    posture, write-path placement a shell cannot bypass, and the code library
- [workflow-build.md](workflow-build.md) — the plan → workflow bridge: a
  `workflow`-deliverable card builds a proposed graph that lands In Review for
  approval before it exists, then apply/reject; host-authority conversion and
  the one authoring path
- [workflow-vocabulary.md](workflow-vocabulary.md) — the node-kind authoring
  contract: the 12 kinds an author may write and what each lowers to, the
  engine-only kinds (`code` / `memory` / `dedup` / `loop`) OpenCompany refuses
  at parse and why, and the builder ⊂ parser ⊂ engine nesting
- [rebuild.md](rebuild.md) — replacing a registered runtime in place (quiesce →
  hand over → swap), so a first-time inference config needs no restart
- [api.md](api.md) — the map of the API surface: which planes exist and where
  each is documented
  - [Console write plane](api-write-plane.md) — every write the console makes,
    route by route
    - [api-team-drafting.md](api-team-drafting.md) — the two draft routes behind
      the teammate copilot, and why a model may write into a persona at all
    - [api-tool-grants.md](api-tool-grants.md) — the three tool-grant routes
      that widen `[tools].allow` from a connect page, and when a grant takes
      effect
  - [api-graphql.md](api-graphql.md) — the `/graphql` read plane
- [credentials.md](credentials.md) — the company's own TinyHumans key: the one
  seam a brokered surface resolves through (Composio today), why rotating it
  reaches every surface wired to it, and which surfaces are deliberately outside
  it
- [config.md](config.md) — configuration and the one-key story
- [setup.md](setup.md) — the first-run setup flow that writes it
- [../security/agent-isolation.md](../security/agent-isolation.md) — the threat
  model behind that limit: what confines an agent today, what does not, and what
  a prompt-injected agent with `shell` can still do after every planned control
  lands
- [users.md](users.md) — human collaborators: magic-link/password sign-in,
  sessions, invites, and chat attribution
- [auth-modes.md](auth-modes.md) — the configured sign-in mode: `email`,
  `wallet`, or `none` (no sign-in, for the desktop app), and what each changes
- [avatars.md](avatars.md) — which icon a teammate wears and which one you do:
  the closed `tiny:`/`blob:` reference grammar and why a URL is not an avatar,
  the upload route (GIFs included, SVG refused), and why a person's name is
  guessed at render time rather than written into the directory
- [hub-console.md](hub-console.md) — one console deployment operating many hosts
  on other origins: the carried session, CORS, and what it costs
- [finance-console.md](finance-console.md) — the Finance section: Invoicing
  (Chargebee) and Wallet (PayPal) as sub-pages, the host read plane that makes
  provider data reachable from the console at all, and how an operator tests a
  connection without billing a real customer
- [connectors.md](connectors.md) — where the runtime runs: the four connectors
  (this computer, TinyHumans Cloud, a remote gateway, over SSH), why the choice
  is per host rather than per application, and what each one costs
- [company-setup/overview.md](company-setup/overview.md) — first-run **company**
  setup: three questions asked once, turned into a real roster of agents, with
  fallback and resume flows. Distinct from [setup.md](setup.md), which
  configures the *instance*
  - [company-setup-guarantees.md](company-setup-guarantees.md) — the four things
    the host *enforces* rather than asks a prompt for: job coverage checked
    against its own list, a tool belt asked for rather than inherited, a copy of
    the reference team refused the name "designed", and a fallback that says
    which fallback it is

## Responsibilities

The kernel owns:

- **Manifest parsing and validation** with prosumer-friendly errors.
- **The cycle loop**: normalize stimuli into events, batch them per company,
  invoke the `Brain`, service its callbacks (tools, context ops), route its
  effects through the `ApprovalGate`, persist the results.
- **Durability**: append-only event log, replay on boot, checkpointed
  drain on shutdown, tar export/import of the whole company bundle.
- **Multi-company hosting**: a registry of running `CompanyRuntime`s with
  per-company isolation (one serial cycle queue each; companies run
  concurrently).
- **The HTTP surface**: operator API, agent-facing A2A endpoint, webhooks.

The kernel explicitly does **not** own cognition (Medulla), model routing
(TinyHumans backend), tool implementations (OpenHuman / TinyAgents), memory
internals (TinyCortex or any store), or the agent economy (tiny.place).

## Crate layout (target)

Today's modules (`src/app`, `src/server`, `src/openhuman`, `src/tiny` — see
[docs/modules/](../../modules/)) remain; the spec adds:

```text
src/ports/      one file per port trait (brain, store, events, memory,
                context, channel, tools, economy, approvals, secrets)
src/company/    manifest.rs, runtime.rs (CompanyRuntime, RuntimeBuilder),
                cycle.rs (CycleRunner), registry.rs (CompanyRegistry)
src/brain/      hosted.rs (HostedMedullaBrain), stub.rs, sidecar.rs (gated)
src/economy/    tinyplace adapter, card generation, signer management
src/store/      fs (default), sqlite (gated)
src/feedback/   capture, scrubber, github filing
```

`AppState` grows a `CompanyRegistry`; `src/error.rs` grows variants
(`Manifest`, `Store`, `Brain`, `Economy`, `PolicyDenied`, `Http`) so every
port returns the crate `Result<T>`.

## Feature flags

| Feature | Adds |
| --- | --- |
| *(default)* | kernel, fs store, hosted brain client, operator API |
| `tiny` | TinyAgents embedding (existing flag; used by stub brain and local workers) |
| `sqlite` | SQLite store implementations |
| `tinymemory` | Hosted/null memory engine seam (`MemoryProvider` contract) |
| `tinyplace` | tiny.place economy adapter and A2A routes |
| `sidecar` | Node sidecar brain for self-hosters |

The default build MUST stay small and compile offline; every feature degrades
to a stub or a clear "not enabled" error, never a panic.

## DB-agnosticism

No storage engine appears in the kernel. The four storage ports
(`CompanyStore`, `EventLog`, `MemoryStore`, `ContextStore`) each ship a
file-based default (a human-inspectable bundle under
`~/.opencompany/companies/<slug>/` — see [lifecycle.md](lifecycle.md)), and a
platform operator implements the same traits over Postgres, S3, or anything
else. Export is defined as "read everything through the ports"; import is the
inverse — so migration between backends is total by construction.
