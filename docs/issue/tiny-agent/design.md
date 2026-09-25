# Design: Tiny, a built-in system-health DM

## The problem

The operator has no single place the *instance itself* tells them something is
wrong: a connection quietly failing, a budget about to run out, a workflow
failing on repeat, an approval queue backing up. Today each of these is a
separate screen the operator has to remember to check. This doc designs a
fixed DM — "Tiny" — that surfaces these proactively, is present in every
company by construction, and cannot be removed the way an operator-created
desk or teammate can be.

## The core decision: separate identity from cognition

The instinct is to build Tiny as a roster `[[agent]]` — give it a role, a
tier, a prompt, and let it "decide" what to say. That is the wrong default,
for a structural reason: a real agent is a heavy, disable-able, deletable
thing by design, and every one of those properties is a problem for something
meant to be a fixed, always-on, trustworthy voice.

Concretely, a manifest agent (`docs/spec/runtime/agents.md`):

- needs a `role` and goes through full prompt assembly,
- needs a tier→model mapping and burns model-call budget on every turn,
- is subject to `[globals].disable` if it comes from the global baseline
  (`docs/spec/runtime/globals.md`), and
- can be deleted through the Team API's `remove_member`
  (`crates/opencompany-core/src/server/ops/team.rs:798`) — the only existing
  refusal there is a "can't have zero teammates" guard
  (`team.rs:821`, `roster_ids(&record).count() <= 1`), not a per-agent
  protection.

None of that is a defect in the agent system — it's correct for teammates,
which are supposed to be things an operator fully owns and can reshape or
remove. Tiny needs the opposite guarantee, so it should not be a member of the
thing operators are allowed to reshape and remove.

### What to build instead

This codebase already has the pattern needed, used for a different purpose:
**host-authored chat rows with no LLM turn behind them, under a reserved id no
teammate can ever hold.**

- `SYSTEM_AUTHOR = "system"` (`crates/opencompany-core/src/ports/ids.rs:116`)
  — the host journals a chat row under this id today (e.g. mention-ambiguity
  notices), with no model call. It renders as a centered system pill in the
  console, not a teammate bubble.
- `WORKFLOW_REPLY_AUTHOR = "workflow-report"`
  (`crates/opencompany-core/src/runtime/channel.rs:52`) — a workflow's own
  report author. Hyphenated deliberately: both id-minting paths (console
  `agent_slug`, manifest `is_snake_case`) reject a hyphen, so no
  operator-created or manifest-declared teammate can ever collide with it.
- `CONFINED_AGENT_ID = "workflow-copilot"`
  (`crates/opencompany-core/src/ports/ids.rs:130`) — a *real* model turn, but
  explicitly not a roster id: not addressable, not in the Team API, not
  subject to `[globals].disable`. This is the precedent for "a real LLM call
  that still isn't a teammate," useful for Tiny's later phases.
- `RESERVED_AGENT_IDS` (`crates/opencompany-core/src/ports/types.rs:4592`) —
  the list `manifest.rs` checks so no operator/manifest id can collide with a
  built-in one (the operator channel, the two General spellings, `system`,
  the workspace roots). This is where a `tiny` id belongs.

So the design is:

1. **Identity**: mint `tiny` into `RESERVED_AGENT_IDS`, hyphen-free is fine
   here since it's never user-mintable by construction once it's in that
   list. Give it its own name/avatar and a **pinned DM rail entry** in the
   console (the same treatment the Operator feed row already gets — a fixed
   entry outside the normal desk/DM list, not read from `company.toml`).
   Tiny never appears in the Team/roster page, because it was never a
   teammate — that is *why* it can't be removed, not a rule enforced on top
   of a thing that otherwise could be.
2. **Phase 1 behavior — no LLM at all.** A process-wide ticker (see below)
   runs deterministic checks and, on a state transition, host-composes a
   templated string and journals `CompanyEvent::AgentReply { agent_id: "tiny",
   chat_id: "dm:tiny" }` directly, the same primitive `system` and
   `workflow-report` already use.
3. **Phase 2+ — a confined turn, still not a roster member**, patterned on
   `workflow-copilot`, for when templated text stops being enough (answering
   "why did you say that," reasoning over usage data for a skill suggestion).
4. **Any write action** (retry a connection, install a skill) is an ordinary
   tool call, classified by consequence and gated through the same
   three-level grant (`[tools].allow ∩ desk.tools ∩ agent.tools`,
   `docs/spec/runtime/tools.md`) and `ApprovalGate`
   (`docs/spec/company-brain/approvals.md`) every teammate's effects cross.
   There is no per-agent bypass anywhere in the existing approval precedence
   chain (`never_do` → single-use grant → standing grant → `always_approve` →
   cap → mode → per-call judgement, `docs/spec/company-brain/grants.md`), and
   there should not be a first one built for this.

## Why not an LLM from day one

Two independent risks, not one:

- **Self-triggering budget spiral.** If the health check itself runs through
  `Brain::run_cycle`, a company already low on budget could have the very
  check meant to warn it be what spends the last of it. A host-side
  conditional cannot do this; a model turn can.
- **Trust-asymmetry as a social-engineering surface.** Nothing in
  `docs/spec/security/agent-isolation.md` currently treats a channel's trust
  level as a variable — but an operator will trust "the system talking" more
  than an ordinary teammate's DM, by design (that trust is the entire point
  of building this). Any text this agent ever ingests (a connection's raw
  error string, a Composio trigger payload) carries the same
  prompt-injection surface any agent's context does
  (`agent-isolation.md`: "the agent that reads them is the one holding the
  grants"), and an injected false claim landing in *this specific channel*
  ("your key is compromised") is higher-leverage than the same injection in a
  normal teammate's DM. A deterministic template cannot be injected into; an
  LLM turn can be.

Both point the same direction: Phase 1 should be tool-less and model-less.

## The trigger loop

Four background tickers already exist and are wired into boot/shutdown
(`docs/spec/runtime/lifecycle.md`, boot step 4 starts "the cron scheduler, the
feedback poller"; shutdown stops them before drain):

| Existing ticker | Shape | Fit for Tiny |
| --- | --- | --- |
| `MaintenanceTicker` (`crates/opencompany-core/src/runtime/maintenance.rs:87`) | Process-wide, one loop, re-reads `CompanyRegistry` (`crates/opencompany-core/src/runtime/registry.rs:18`) on an interval, sweeps every company's expired approvals/grants | **This is the template.** Nobody has to remember to wire it up per company |
| `CompanyScheduler` (`crates/opencompany-core/src/runtime/scheduler.rs:179`) | Per-company, driven by manifest `[[schedule]]` | Wrong template — an operator could edit the trigger away, which defeats "always on" |
| `WorkflowScheduler`, `mailbox_poller.rs` | Similar tokio-interval shapes, per saved workflow / IMAP polling | Same family, confirms the pattern is normal here, not directly reusable |

Recommended: a fifth ticker, `SystemHealthTicker`, process-wide like
`MaintenanceTicker`, ticking on an interval, running independent checks per
company, and appending a new `CompanyEvent` variant into that company's own
serial cycle queue **only on a state transition** (OK → problem), never on
every tick — level-triggering a channel that cannot be muted is how operators
learn to ignore it.

```text
 every N minutes
        |
        v
+-----------------------+  reads only, no model call  +------------------------+
| SystemHealthTicker     | ---------------------------> | per-company probes    |
| (process-wide, like    |                               | - connection status   |
| MaintenanceTicker)     |                               | - budget / ledger     |
+-----------------------+                               | - usage meter          |
        |                                                +------------------------+
        | state changed? (OK -> PROBLEM only)
        v
+------------------------+
| templated message,      |  deterministic string, not an LLM
| composed host-side      |
+------------------------+
        |
        v
+---------------------------------------+
| CompanyEvent::AgentReply                |
|   agent_id: "tiny"                      |
|   chat_id:  "dm:tiny"                   |
+---------------------------------------+
        |
        v
   EventLog (durable) -> operator SSE feed -> console renders the DM bubble
```

Reactive precedent already exists for one of the three signals:
`BudgetPauseMarker` (`crates/opencompany-core/src/runtime/grants.rs:1169`)
already parks a durable, redeemable marker when a turn hits the budget wall
(`GET/POST …/agents/{id}/budget-pause`). What's missing is the *proactive*
warning before that wall is hit — which is exactly Tiny's job.

## What each signal actually costs to wire up

| Signal | Status | What's needed |
| --- | --- | --- |
| **Connections failing** | Ready today. `project_connections` (`crates/opencompany-core/src/server/ops/connections_read.rs:321`) already reconciles native OAuth and a live Composio probe into one row per provider. | A read-only binding to the existing projection. Nothing new. |
| **Credits running low** | Data exists but "credits" isn't a single defined concept yet. `metering::daily_budget` (`crates/opencompany-core/src/metering/daily_budget.rs`) computes spend-today against the manifest `budget_usd_daily` cap — a budget/ledger model, not a prepaid-credits balance. | A product decision: define "low" off the existing ledger/budget data (cheap, available now), or add a new read against the actual TinyHumans account balance (more accurate, new plumbing). |
| **Usage → skill suggestion** | Weakest link, highest effort. Product analytics (`docs/spec/runtime/analytics.md`) is one-way/external by design for privacy — not queryable back in-process. The in-process usage meter (`metering::usage`) is queryable, but no recommender exists anywhere, and the skills routes (`POST …/skills`, `GET …/skills/registry`) are install/list only. | Genuinely new logic on top of partially-existing data. Scope as its own later phase, not the MVP. |

## Relationship to `NotificationStore`

A separate, already-built durable notification system exists:
`NotificationStore` (`crates/opencompany-core/src/ports/notifications.rs`) —
per-person read state, `SubjectKind` (Task/Run/Approval/Workflow/Message),
audience targeting, `GET/PUT …/notifications`. Its own doc comment defers
"what's worth notifying" to a future epic (issue #558) so that vocabulary
isn't decided twice.

This needs a decision, not an assumption: does Tiny become the chat-facing
delivery surface for what that system decides is worth notifying, or is it a
second, independent decision-maker? Building both without reconciling them
risks Tiny's DM and the notification bell disagreeing about whether credits
are actually low. See [`edge-cases.md`](edge-cases.md).

## Full menu of what this could eventually cover

- Credit/budget threshold warnings, proactively, ahead of the existing
  reactive `BudgetPauseMarker`
- Connection/OAuth health — token expiry or failure, surfaced before a tool
  call fails silently mid-turn
- Workflow run failure digests, rolling up `WorkflowRunFinished` events
  instead of the operator checking every desk
- Approval-queue backlog nudges, reusing the TTL sweep `MaintenanceTicker`
  already computes
- Usage-based skill/tool suggestions (highest effort — see table above)
- Onboarding nudges ("2 agents still need email connected")
- Security notices (a manifest edit that collided with a reserved id, a grant
  nearing expiry)
- Self-hosted update/release notices (should arguably stay opt-out-able even
  though the agent itself isn't)
- Phase 2+ writes: retry a connection, redeem a budget pause, install a
  suggested skill — all approval-gated, no exceptions

See [`edge-cases.md`](edge-cases.md) for what needs deciding before any of
this is built.
