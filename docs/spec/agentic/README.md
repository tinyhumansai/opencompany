# The Agentic Company

This section specifies how OpenCompany becomes agentic end to end: an agent
**designs** the company, agents **run** it, and an agent **evolves** it — so
every company is tailored to its Operator's specific needs rather than being
a static template instance. Terms: [glossary](../glossary.md).

```text
Design    the Architect      turns a conversation into a tailored Blueprint
Run       the Brain + Roster the existing cycle (company-brain/)
Evolve    the Manager        watches how the company actually runs and
                             proposes changes as Change Proposals
```

The Operator remains the only decider. One principle binds all three roles:

> **Agents propose; the Operator disposes.** No agent — Architect, Manager,
> or Teammate — ever mutates the Charter, Roster, policy, or budget
> directly. Every change is a [Change Proposal](proposals.md) resolved in
> the Approvals Inbox, exactly like any other
> [Checkpoint](../company-brain/approvals.md).

## Why

Today's flow is *pick a template → answer a short interview → go live*. That
gets a company running, but the fit is coarse: the Roster, checkpoints, and
schedules are whatever the template author guessed, and they never change
unless the Operator hand-edits settings. Two consequences:

1. **Setup fits the template, not the person.** A solo wedding photographer
   and a three-city photo studio both get the "design studio" roster.
2. **Drift goes unnoticed.** The Operator approves the same $3 stock-photo
   purchase forty times, a teammate sits idle for a month, feedback repeats
   the same complaint — and nothing in the product reacts.

The agentic setup and agentic manager close both gaps without weakening the
control model: tailoring happens through the same manifest, charter,
provenance, and approval machinery that already exists.

## The three roles

| Role | Runs when | Reads | Produces | Spec |
| --- | --- | --- | --- | --- |
| **Architect** | onboarding; on demand later ("reshape my company") | operator conversation, Template library | a [Blueprint](setup.md) (pre-launch) or a proposal batch (post-launch) | [setup.md](setup.md) |
| **Brain + Roster** | every cycle | events, memory, context | work, effects | [company-brain/](../company-brain/README.md) (unchanged) |
| **Manager** | scheduled tick (default weekly) | feedback inbox, approval history, ledger, memory traces | [Change Proposals](proposals.md) | [manager.md](manager.md) |

Neither the Architect nor the Manager is a new kind of runtime. Both are
cognition jobs executed through the existing
[`Brain` port](../runtime/ports.md) (hosted Medulla), consuming the same
tiers, budgets, and callbacks as a normal cycle. They add **no new external
dependency** and respect the one-key invariant.

## One voice, still

The Architect and Manager are internal names. Prosumers never meet them as
characters: setup is *"tell us about your business — we'll build your
company"*, and Manager output surfaces as **the company itself** making a
suggestion in the Approvals Inbox ("Want to stop being asked about purchases
under $5? You've approved 14 like this."). Exposing "the Architect agent" or
"the Manager agent" in product text violates the
[prosumer translation table](../glossary.md).

## Invariants (normative)

- **MUST route through approvals.** Architect launches, Architect reshapes,
  and Manager suggestions are all Checkpoints. Silence expires to "no".
- **MUST NOT self-deal.** The Manager cannot approve, auto-approve, or
  batch-bundle its own proposals; the Architect cannot launch a company the
  Operator has not reviewed.
- **MUST NOT escalate.** No proposal may raise `[budget].monthly_usd`,
  flip `[place].discoverable`, remove an `always_approve` entry for
  money/publish/filing effects, or alter the Manager's own tick, limits, or
  proposal quota. Those changes exist, but only as Operator-initiated edits.
- **MUST preserve provenance.** Every applied change records its origin
  layer ([proposals.md](proposals.md)), so the Charter can always answer
  "who set this and why".
- **MUST validate before proposing.** Anything the Architect or Manager
  emits is machine-checked (`opencompany check` semantics) before the
  Operator ever sees it; an invalid draft is a bug, not a prompt.
- **MUST degrade gracefully.** With the brain unreachable, setup falls back
  to the static Template Gallery + classic interview, and the Manager tick
  simply skips. A company never blocks on either role.

## Index

| Doc | Purpose |
| --- | --- |
| [setup.md](setup.md) | Agentic setup: the Architect, Blueprints, the launch review |
| [manager.md](manager.md) | The Manager: signals, tick, suggestion surfaces, fences |
| [proposals.md](proposals.md) | Change Proposal schema, lifecycle, provenance, rollback (normative) |

Related: [product/templates.md](../product/templates.md) (templates become
the Architect's priors), [company-brain/charter.md](../company-brain/charter.md)
(provenance layers), [roadmap.md](../roadmap.md) (phasing).
