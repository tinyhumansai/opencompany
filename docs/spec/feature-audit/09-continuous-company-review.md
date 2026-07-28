# Continuous Company Review

## Outcome

The company periodically identifies evidence-backed improvements and presents
them as bounded, reversible proposals that the Operator may accept, edit, or
reject.

## Why this matters

A company assembled once will drift away from its Operator as volume,
preferences, costs, tools, and markets change. The target Manager design
already defines a continuous-fit loop and a Change Proposal fence. The product
feature must connect those designs to real signals, validation, approvals,
application, rollback, and suppression behavior.

## Proposed capability

- Run a low-frequency review on a configurable schedule and explicit demand.
- Read approval outcomes, denials, feedback, budget use, teammate activity,
  workflow outcomes, repeated failures, schedule results, and template updates.
- Produce no output when evidence is insufficient.
- File typed proposals for roster, mandate, policy, charter, schedule, workflow,
  skill, and budget-allocation changes.
- Include evidence, expected benefit, risk, affected surfaces, and rollback
  behavior in plain language.
- Validate proposals against hard runtime fences before showing them.
- Limit open proposals and suppress repeated denied or ignored suggestions.
- Apply accepted changes through versioned overlay patches.
- Revert an applied proposal without erasing its provenance.
- Measure whether accepted changes improved the stated outcome and surface a
  follow-up if they did not.

## Acceptance boundary

- The review process has no effect-producing tools.
- It cannot approve, apply, bundle, or resubmit its own proposals.
- It cannot raise the company budget ceiling, weaken mandatory approvals,
  remove `never_do` constraints, publish the company, or change its own fence.
- Every proposal cites durable evidence and a target configuration version.
- Concurrent configuration changes cause revalidation rather than blind apply.
- Denied and expired suggestions follow documented suppression windows.
- Disabling continuous review leaves normal company execution unchanged.
- Accepted changes and reverts remain fully auditable.

## Likely implementation seams

- `docs/spec/agentic/manager.md` and `agentic/proposals.md`
- reserved schedules and the existing Brain port
- EventLog, approvals, ledger, UsageMeter, feedback, and workflow outcomes
- an overlay/proposal store with optimistic concurrency
- Approvals, Settings, and change-history console surfaces

## Open questions

- Which evidence thresholds are fixed versus template-specific.
- How outcome attribution avoids crediting unrelated changes.
- Whether proposal batches may share evidence while remaining independently
  resolvable.
- The default cadence and budget for very small companies.
