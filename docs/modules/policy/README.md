# Policy Module

The policy module is the default `ApprovalGate`: it evaluates an `Effect`
against the manifest `[policy]` block into `Allow`, `RequireApproval`, or
`Deny`, and owns the approval queue (`park` / `resolve`). Semantics follow
[`docs/spec/company-brain/approvals.md`](../../spec/company-brain/approvals.md):
`readonly`/`supervised`/`full` modes, `always_approve` effect kinds, and
`auto_approve_under_usd`.

The gate is consulted by the `CycleRunner` before any effect crosses the trust
boundary; parked effects surface in the operator's approvals inbox.

Effects are classified into the checkpoint groups (Spend / Send / Sign /
Publish / Hire / Identity) with per-group supervised defaults. Parked approvals
**default-deny on silence**: they expire to `deny` after a configurable window
(default 7 days) measured against an injectable clock. The operator may **edit**
a parked effect's payload and approve the amended version; the follow-up cycle
shows the brain both the original and the edit.
