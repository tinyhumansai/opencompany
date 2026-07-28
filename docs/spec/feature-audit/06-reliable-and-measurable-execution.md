# Reliable and Measurable Execution

## Outcome

Company work is durable enough to recover from failure, observable enough to
debug, and measurable enough to determine whether teammates and workflows are
producing useful outcomes within budget.

## Why this matters

The runtime already journals cycles and committed effects, uses idempotency
keys, retains events, and projects usage and finances. Production operation
also requires explicit retry semantics, stalled-work detection, diagnostic
traces, outcome measures, and safe replay tools.

## Proposed capability

- Give every inbound event, cycle, task, workflow run, tool call, approval, and
  payment a correlated execution identity.
- Persist state transitions and attempts with structured error categories.
- Define bounded retries, backoff, deadlines, and nonretryable failures.
- Detect stalled work and surface it as an actionable feed item.
- Route exhausted work into a durable recovery queue with retry, amend, cancel,
  and inspect controls.
- Expose execution timelines without revealing secrets or unnecessary private
  reasoning.
- Measure latency, completion, failure, approval wait, token use, model cost,
  tool cost, revenue, and budget variance.
- Let templates define outcome checks for recurring work.
- Capture Operator corrections and ratings as evaluation data.
- Run regression scenarios against updated prompts, tools, policies, workflows,
  and templates before rollout.
- Provide health and readiness signals suitable for hosted wake-on-request.

## Acceptance boundary

- Recovery never repeats an effect whose commit is already journaled.
- Retry policy is explicit and inspectable for every effect category.
- The Operator can distinguish waiting, stalled, failed, and canceled work.
- Metrics are company-scoped and do not create a hidden telemetry channel.
- Evaluation datasets remain local unless the Operator explicitly exports
  them.
- Cost totals reconcile with ledger entries within documented rounding rules.
- Traces redact credentials and bounded private context before display/export.
- Restore and replay behavior is covered by restart-focused integration tests.

## Likely implementation seams

- `src/runtime/cycle.rs`, `src/runtime/journal.rs`, and scheduler maintenance
- `EventLog`, `UsageMeter`, CompanyStore ledger, and new recovery state
- `src/server/provision.rs` health and webhook events
- GraphQL execution/metrics projections and console timelines
- `tests/e2e.rs` restart, retry, and at-most-once scenarios

## Open questions

- Which outcomes are generic versus template-authored.
- Retention limits for traces, failed payloads, and evaluation examples.
- Whether recovery actions create new cycles or resume the original identity.
- How much model-generated explanation is useful without storing hidden
  reasoning.
