# Company Commerce

## Outcome

A company can publish what it offers, receive qualified work, agree on scope,
deliver through its normal runtime, get paid, hire another company or agent,
and retain a complete Operator-readable record.

## Why this matters

OpenCompany already has the AgentEconomy port, discoverable Agent Cards, A2A
task handling, x402 authorization, budget scopes, delegated signer concepts,
and ledger entries. The remaining product work is to join these primitives
into a trustworthy engagement lifecycle rather than isolated protocol calls.

## Proposed capability

- Publish a versioned service catalog with descriptions, inputs, outputs,
  availability, base pricing, and policy constraints.
- Discover providers by capability and inspect their identity and terms.
- Convert inbound requests into qualified leads, quotes, or rejected requests.
- Represent an engagement with counterparties, scope, price, deadlines,
  deliverables, approval requirements, and status.
- Require Operator approval for exposure outside standing rules.
- Authorize bounded payment through delegated signers without exposing the
  company wallet key.
- Link A2A tasks, work artifacts, approvals, receipts, disputes, refunds, and
  ledger entries to the engagement.
- Verify delivery before releasing payment where the payment rail supports it.
- Track revenue, spend, margin, and counterparty history in Finances.
- Allow a company to hire external capability as a normal tool-like effect.
- Pause public intake without deleting identity or history.

## Acceptance boundary

- Public discovery is opt-in and reversible.
- The company cannot spend above its budget or signer grant.
- Every payment has an engagement, counterparty, authorization, and receipt.
- Duplicate tasks or callbacks cannot cause duplicate settlement.
- Inbound work passes authentication, pricing, policy, and capacity checks
  before execution.
- Private workspace or memory content is never published by default.
- The Operator can inspect and export the complete commercial audit trail.
- Degraded economy connectivity does not prevent private company operation.

## Likely implementation seams

- `AgentEconomy` in `src/ports/` and tiny.place adapters
- `src/server/a2a.rs`, company Agent Card projection, and skill dispatch
- approval policy, `BudgetScope`, delegated signers, and ledger entries
- engagement storage port with fs/sqlite/mongodb conformance coverage
- inbox, tasks, workspace deliverables, usage, and finances projections

## Open questions

- The minimum engagement state machine for the first release.
- Whether quotes and disputes are protocol extensions or local projections.
- How capacity and availability are advertised without leaking operations.
- Which receipts and deliverables belong in portable exports.
