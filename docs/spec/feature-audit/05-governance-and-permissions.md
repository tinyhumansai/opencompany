# Governance and Permissions

## Outcome

The Operator can understand, test, and change what the company may do, while
the runtime enforces the same decision consistently across chat, schedules,
workflows, teammates, integrations, and commerce.

## Why this matters

OpenCompany already has manifest-driven approvals, security-tier mapping,
budget ceilings, a SecretStore, and an append-only journal. The next step is to
turn those primitives into a coherent governance product rather than a set of
configuration fields.

## Proposed capability

- Express standing rules by action, tool, integration, counterparty, amount,
  teammate, workflow, destination, and time window.
- Compose company-wide policy with narrower teammate and workflow grants.
- Explain every decision in plain language: allowed, denied, or awaiting
  approval, including the rule and configuration layer responsible.
- Preview how a proposed policy change would affect representative actions.
- Detect shadowed, contradictory, unsafe, or unreachable rules before save.
- Provide time-limited and single-use grants without exposing credentials.
- Require stronger confirmation for changes that expand autonomy.
- Record approvals, denials, amendments, policy changes, and grant use in one
  immutable audit trail.
- Support emergency pause and credential revocation.
- Export a redacted governance report for review or incident response.

## Acceptance boundary

- Deny wins when rules conflict unless an explicit, narrower Operator grant is
  defined by the normative policy model.
- Policy is evaluated at the moment of effect, not only during planning.
- A policy edit cannot retroactively authorize an already denied effect.
- Every authorization decision is attributable to a versioned rule set.
- Simulation performs no external effects and uses the production evaluator.
- Secret values never appear in rules, logs, explanations, or exports.
- Platform administrators cannot silently broaden a tenant company's policy.

## Likely implementation seams

- `src/policy/`, `src/runtime/journal.rs`, and approval resolution
- manifest policy plus Operator-owned overlay storage
- OpenHuman tool and credential injection in `src/harness/`
- `SecretStore`, connection lifecycle, and delegated signer scopes
- Settings and Approvals console surfaces
- webhook/event projections for security-relevant changes

## Open questions

- The minimal rule language that remains understandable to a nontechnical
  Operator.
- Whether policy versions are full snapshots or append-only patches.
- Which autonomy-expanding edits require step-up authentication.
- How emergency pause interacts with already committed effects.
