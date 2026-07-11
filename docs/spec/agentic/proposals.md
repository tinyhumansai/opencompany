# Change Proposals

A Change Proposal is a typed, evidenced, Operator-approvable diff against a
company's effective configuration. It is the single mechanism by which any
agent — the [Manager](manager.md), the post-launch
[Architect](setup.md), or a future source — changes a running company.
This doc is normative.

## Schema

```text
ChangeProposal
├── id           ULID
├── source       manager | architect | operator   (operator: Settings edits
│                may reuse the same pipeline for uniform audit)
├── kind         roster.add | roster.retire | roster.reshape
│                policy.rule | policy.threshold | policy.always_approve.add
│                charter.tone | charter.services | charter.mission
│                charter.never_do.add
│                schedule.add | schedule.move | schedule.remove
│                budget.rebalance
│                template.update
├── diff         machine-applicable change against effective config
│                (JSON-patch-style ops over the config tree; for
│                template.update, the upstream diff artifact)
├── summary      one prosumer-language sentence (the inbox headline)
├── rationale    plain-language explanation shown in the approval card
├── evidence     links into the EventLog / ledger / feedback items that
│                justify it ("14 approvals matching …")
├── expires_at   standard approval expiry; silence resolves to denied
└── status       open | approved | edited+approved | denied | expired |
                 rolled_back
```

## Validation at filing (normative)

Before a proposal reaches the Approvals Inbox, the runtime MUST:

1. **Apply the diff to a copy** of the effective configuration and run full
   manifest/charter validation (`opencompany check` semantics) on the
   result. Invalid → rejected, internal error event, never shown.
2. **Enforce the fence** for agent-sourced proposals
   ([manager.md](manager.md)): budget ceiling, discoverability, identity,
   `always_approve` removals, `never_do` relaxation, `[manager]`/`[brain]`
   self-modification. Fence violations → rejected at filing.
3. **Deduplicate**: a proposal semantically equal to one currently open,
   denied within 90 days, or twice-expired is suppressed.
4. **Check the quota** (per source; the Manager's is 3 open).

## Lifecycle

```text
draft ──validate──▶ open ──▶ approved ────────▶ applied
                     │  └──▶ edited+approved ─▶ applied (with operator diff)
                     ├─────▶ denied    (suppresses re-filing 90 days)
                     └─────▶ expired   (2× same proposal ⇒ 90-day suppress)
applied ──inverse proposal, one click──▶ rolled_back
```

- **Open** proposals live in the ordinary Approvals Inbox and follow every
  approval rule in [approvals.md](../company-brain/approvals.md), including
  expiry-to-no. They are Checkpoints; nothing about their handling is
  special-cased in the gate.
- **Edit** lets the Operator adjust the diff before approving (e.g. change
  a $5 threshold to $2). The applied artifact records both the agent's
  original and the Operator's edit.
- **Applied** changes are written to the `CompanyStore` live record — never
  back into the manifest file — as the next provenance layer (below).
- **Rollback** is first-class: every applied proposal stores its inverse
  diff. Reverting files the inverse as a pre-approved Operator action,
  applied immediately, logged like any change.

## Provenance

The [charter layering](../runtime/manifest.md) gains one layer:

```text
template defaults ⟵ manifest ⟵ interview/Blueprint ⟵ applied proposals ⟵ operator edits
```

Each applied proposal records `{proposal_id, source, evidence, approved_at,
operator_edit?}` on every field it touched, so Settings can answer *"who
set this and why"* end to end: "The company suggested this on 12 Jun
(you'd approved 14 similar purchases); you set the limit to $2." A later
direct Operator edit wins over an applied proposal, as usual.

## Audit events

All land in the `EventLog` (append-only):

| Event | When |
| --- | --- |
| `ProposalFiled` | passed validation, entered the inbox |
| `ProposalRejectedAtFiling` | failed validation/fence/dedup/quota (internal) |
| `ProposalResolved` | approved / edited / denied / expired, with the resolution |
| `ProposalApplied` | diff applied to the live record |
| `ProposalRolledBack` | inverse applied |

## Relationship to plain approvals

An ordinary [Checkpoint](../company-brain/approvals.md) asks *"may I do
this action once?"*; a Change Proposal asks *"may I change how the company
is configured from now on?"*. They share the inbox, the expiry rules, and
the one-voice presentation; they differ in payload (an Effect vs. a config
diff) and in consequence (execution vs. a new provenance layer). A standing
rule created by approving a `policy.rule` proposal is exactly the same
object as one created through the "loosen the fence" growth moment — the
proposal pipeline is a producer of standing rules, not a parallel policy
system.
