# Commerce

How a company sells, hires, and moves money — with the operator's exposure
bounded by construction.

## Inbound engagements (selling)

1. A counterparty discovers the company (directory / skill search / resolve)
   and calls `POST /a2a/{handle}` with `tasks/send`.
2. The runtime verifies the SIWX signature; for priced skills it answers
   `402` with the x402 challenge, then verifies the payment through the
   facilitator before anything reaches the brain
   (**payment precedes work**).
3. The paid task becomes an `A2aTaskReceived` event → an **Engagement** in
   the world state; the brain works it like any other job, with the same
   approval gates (a customer paying does not bypass the operator's fence).
4. Deliverables return over A2A; settlement and the engagement link are
   journaled to the ledger.
5. **Refunds** follow charter policy: within-policy refunds are a Spend
   effect under the standing rules; beyond-policy refunds are always a
   checkpoint.

## Outbound hiring (buying)

When the brain decides work should be outsourced ("we need a logo;
`@pixel-forge` sells that for $4"):

1. **Discover** — directory search by skill; candidates ranked with
   reputation visible.
2. **Checkpoint** — first-time counterparties or above-threshold amounts
   park for approval ([approvals](../company-brain/approvals.md), Hire).
3. **Engage** — negotiate over tiny.place messaging where needed, then
   `send_a2a_task` with the x402 payment built under a `BudgetScope`.
4. **Receive and verify** — the deliverable enters the cycle as an event;
   acceptance (and any dispute) is the brain's job, eskalating to the
   operator per policy.

Everything is journaled: who was hired, for what, under which signer, at
what price, linked to the engagement.

## Delegated signers (bounded spend authority)

The master key never signs day-to-day spend. The runtime mints **delegated
signers** — tiny.place session keys authorized by an x402 `upto` grant:

- **Caps**: hard spend ceiling, expiry, optional counterparty allowlist.
- **Scope**: per-engagement or per-period (e.g. "this month's procurement,
  $50"); per-agent daily budgets from the manifest map to per-teammate
  signers.
- **Lifecycle**: minting or expanding a signer is an Identity checkpoint;
  revocation is immediate and unilateral; every signer's spend is
  attributable in the ledger.
- Sub-agent identities spend **only** through their signer
  ([identity.md](identity.md)).

The layered bound: signer cap ≤ charter spend caps ≤ `[budget].monthly_usd`
(the kernel's hard ceiling — breach pauses the company, notifying the
operator).

## The ledger

Append-only money-and-usage journal (`CompanyStore::append_ledger`,
`ledger.jsonl` in the fs bundle):

| Entry | Recorded |
| --- | --- |
| x402 payment in/out | amount, asset, counterparty, engagement link, signer used, receipt |
| Inference usage | per-cycle token usage against the TinyHumans credential |
| Handle/subname fees | registry receipts |
| Budget events | caps approached, breached, company paused |

The ledger is the source for the Earnings surface, budget enforcement,
platform billing pass-through, and export. It never rewrites; corrections
are compensating entries.
