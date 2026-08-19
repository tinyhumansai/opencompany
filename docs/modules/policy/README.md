# Policy Module

The policy module is the default `ApprovalGate`: it evaluates an `Effect`
against the manifest `[policy]` block into `Allow`, `RequireApproval`, or
`Deny`, and owns the approval queue (`park` / `resolve`). Semantics follow
[`docs/spec/company-brain/approvals.md`](../../spec/company-brain/approvals.md):
`readonly`/`supervised`/`full` modes, `always_approve` effect kinds, and
`auto_approve_under_usd`.

The gate is consulted by the `CycleRunner` before any effect crosses the trust
boundary; parked effects surface in the operator's approvals inbox.

There are two ways onto that queue, both landing in `CycleHostImpl::park` (so a
parked effect is journaled one way and survives a restart with its original id):

- `CycleHost::emit_effect` — an effect the brain submits for a **decision**. The
  gate evaluates it and parks only on `RequireApproval`.
- `CycleHost::park_effect` — an effect whose verdict the brain **already**
  reached, parked as-is. This is the harness brain's path: its openhuman
  `ApprovalPolicy` blocks a gated tool call inside the agent turn, and the
  projected call is then held for the operator rather than re-decided (issue
  #172). Re-evaluating it here would `Allow` — and so silently "execute" as a
  no-op — anything in the `Other` group, which is most gated tool calls. See
  [the OpenHuman module](../openhuman/README.md#approval-parking).

Effects are classified into the checkpoint groups (Spend / Send / Sign /
Publish / Hire / Identity) with per-group supervised defaults. Parked approvals
**default-deny on silence**: they expire to `deny` after a configurable window
(`[policy].approval_ttl_hours`, default 24 hours) measured against an
injectable clock. The window is enforced at resolution time by the gate itself;
draining the queue is the `MaintenanceTicker`'s job (issue #971). The operator may **edit**
a parked effect's payload and approve the amended version; the follow-up cycle
shows the brain both the original and the edit.

## Emergency stop

`ManifestApprovalGate` carries an `AtomicBool` kill switch, checked by
`evaluate` **before** every policy rule including `always_approve`. While it is
engaged, any effect outside `EffectGroup::Other` is `Deny` — not
`RequireApproval`, so the approval queue cannot be used to work around the
switch. `Other` is exempt so chat keeps working.

The durable state is the event log, not a record field: `replayed_emergency`
scans for the last `CompanyEvent::EmergencyPauseChanged` at boot and
`CompanyRuntime::hydrate_emergency` seeds the flag from it, **failing safe to
stopped** if the log cannot be read. The switch is untouched by `sweep_expired`
— it has no TTL and never auto-releases.

Full normative rules, including the asymmetric confirmation on the two REST
routes, are in
[`docs/spec/company-brain/approvals.md`](../../spec/company-brain/approvals.md#emergency-stop-the-governance-kill-switch).
