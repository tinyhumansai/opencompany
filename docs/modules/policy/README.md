# Policy Module

The policy module owns the default `ApprovalGate` and durable approval queue.
Policy-generated HITL is currently disabled: production evaluation allows
effects that the historical taxonomy would park, while `readonly` and the
emergency stop remain hard denials.

Approval cards come from explicit `park_effect` calls, including the intrinsic
`request_approval` tool and specialized tools that openly stage a concrete
approval. Ordinary tool calls are not silently converted into prompts.

## The operator overlay and the live gate

The gate is built from the seed manifest's `[policy]` alone, then reconciled
with the operator's console override once the persisted record is read.
`CompanyRecord::effective_policy` resolves the merge (per field, `None` meaning
"not overridden"), and the runtime applies it to the live gate with
`ManifestApprovalGate::apply_effective_policy` at boot/rebuild and at the start
of every cycle (issue #1455). The swap keeps the parked queue and the emergency
switch; only the evaluation snapshot and the derived deadline move.

The two halves move on different timings. The deadline (`[policy].approval_ttl_hours`)
is **immediate**: a policy `PUT`/`DELETE` calls `ManifestApprovalGate::apply_effective_ttl`
right after the write persists, because a parked card's deadline is re-evaluated
against the current TTL each time it is displayed, swept or resolved — waiting
for the next cycle would let approvals parked under a longer TTL outlive the
deadline the console just reported. The evaluation snapshot (mode,
`always_approve`, spend cap) moves at the next safe turn boundary instead: an
in-flight turn must finish under the policy snapshot it started with, and a
failed rebuild must not leave the still-live runtime enforcing a policy its
record does not describe. A test-injected gate is exempt — it carries its own
policy/TTL on purpose.

There are two ways onto that queue, both landing in `CycleHostImpl::park` (so a
parked effect is journaled one way and survives a restart with its original id):

- `CycleHost::emit_effect` still evaluates an effect, but production policy no
  longer returns `RequireApproval`.
- `CycleHost::park_effect` — an effect whose verdict the brain **already**
  reached, parked as-is. This is the harness brain's path: its openhuman
  `ApprovalPolicy` blocks a gated tool call inside the agent turn, and the
  projected call is then held for the operator rather than re-decided (issue
  #172). Re-evaluating it here would `Allow` — and so silently "execute" as a
  no-op — anything in the `Other` group, which is most gated tool calls. See
  [the OpenHuman module](../openhuman/README.md#approval-parking).

The historical checkpoint classification remains for audit and future policy
modes, but no longer creates HITL. Explicitly parked approvals
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
