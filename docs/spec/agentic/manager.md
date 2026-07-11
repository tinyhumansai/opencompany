# The Agentic Manager

The Manager is the company's continuous-fit loop: it watches how the
company *actually* runs — what the Operator approves and denies, where
money goes, what feedback repeats, which teammates idle — and turns
mismatches into [Change Proposals](proposals.md). It is how a launched
company keeps tailoring itself to the Operator instead of freezing at its
Blueprint. Overview and invariants: [README.md](README.md).

The Manager is not a chat persona, not a Roster member, and not a new
runtime. It is a scheduled cognition job on the
[`Brain` port](../runtime/ports.md), and its entire output surface is the
Approvals Inbox — where its suggestions speak in the company's one voice
([company-brain](../company-brain/README.md)).

## Signals

Each tick, the Manager reads (read-only, through the existing ports):

| Signal | Source | Example inference |
| --- | --- | --- |
| Approval history | `EventLog` | 14 approvals of near-identical sub-$3 spends → propose a standing rule |
| Denial patterns | `EventLog` | 3 denials of outreach emails → propose tightening tone / adding a checkpoint |
| Feedback inbox | brain state, [feedback loop](../feedback-loop/README.md) | repeated "too formal" thumbs-downs → propose a Charter `tone` edit |
| Ledger | `CompanyStore` | one teammate consumes 80% of budget for 5% of output → propose rebalancing per-agent caps |
| Memory traces | `MemoryStore` | recurring task type with no owning teammate → propose a hire; a teammate idle for a month → propose retiring it |
| Schedule outcomes | `EventLog` | weekly digest always read Monday night → propose moving the cron |
| Template registry | Template library | upstream template improved → surface the diff ([templates.md](../product/templates.md)) |

The Manager reasons over evidence that already exists in the brain state; it
adds no new telemetry, calls no external services, and (like the Architect)
registers no effect-producing tools.

## The tick

- **Cadence**: default weekly; configurable between daily and monthly via a
  reserved `[manager]` manifest table (`[manager].cadence = "weekly"`,
  `enabled = true` by default). The tick rides the existing `[[schedule]]`
  machinery — it is a `ScheduleFired` event with a reserved prompt, run as
  its own cycle with the standard pass ceiling and a per-tick budget cap
  counted against `[budget].monthly_usd`.
- **Output**: zero or more Change Proposals, each independently validated
  before filing ([proposals.md](proposals.md)). A tick that finds nothing
  files nothing — no "all good!" noise.
- **Quota**: at most **3 open Manager proposals** at any time (constant,
  not configurable upward by the Manager itself). At quota, new findings
  are journaled and reconsidered next tick. Proposals expire like any
  approval; expiry is a signal too — two expiries of the same suggestion
  MUST suppress it for 90 days.

## What it may propose

All of these are ordinary Change Proposals — Operator-approved, provenance
recorded, reversible:

- **Roster**: add a teammate (id, role, description, tier hint, tool
  grants, per-agent budget), retire one, reshape one's mandate.
- **Policy**: a new standing rule ("auto-approve stock photos under $5"),
  raising/lowering `auto_approve_under_usd`, *adding* effects to
  `always_approve`.
- **Charter**: `tone` and `services` edits driven by feedback; `mission`
  refinements. `never_do` may only be **added to**, never relaxed.
- **Schedules**: add, move, or drop `[[schedule]]` entries.
- **Budget rebalance**: shifting per-agent daily caps *within* the existing
  company ceiling.
- **Template updates**: adopting an upstream template diff, presented per
  the [customization-without-forking rule](../product/templates.md).

## The Manager's fence (normative)

Restating the [section invariants](README.md) as hard rules enforced by the
runtime — not by prompting — when a proposal is filed:

- MUST NOT raise `[budget].monthly_usd` or any cap above the company
  ceiling.
- MUST NOT flip `[place].discoverable`, claim handles, or touch identity.
- MUST NOT remove money/publish/filing effects from `always_approve`, and
  MUST NOT relax `never_do`.
- MUST NOT modify the `[manager]` table (its own cadence, quota, or
  enablement) or the `[brain]` table.
- MUST NOT approve, bundle, or resubmit-verbatim its own denied proposals
  (a denial suppresses that suggestion for 90 days, same as double expiry).
- Proposals violating the fence are rejected at filing with an internal
  error event — the Operator never sees them.

The Operator can do all of the fenced things by hand; the fence bounds the
Manager, not the human.

## How suggestions read

Prosumer language rules apply ([glossary](../glossary.md)): the suggestion
is from *the company*, with its evidence inline and plain controls:

> **Stop asking about small stock-photo purchases?**
> You've approved 14 purchases like this in the last month, all under $3.
> If you agree, we'll stop asking for anything under $5 from these sites.
> You can change this any time in Settings.
> [ Yes, stop asking ] [ Keep asking me ] [ Edit the limit ]

Approving applies the proposal ([proposals.md](proposals.md)); the Work
Feed logs the applied change; Settings shows it with full provenance and a
one-click revert.

## Failure and degradation

- Brain unreachable at tick time → the tick is skipped and rescheduled;
  never queued into a burst.
- Budget ceiling reached → Manager ticks pause with the rest of the company.
- `[manager].enabled = false` → no ticks, no proposals; the company still
  runs exactly as configured. The Manager is an enhancement, never a
  dependency.
