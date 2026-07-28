# Executable Workflows

## Outcome

An Operator can design, validate, run, schedule, pause, resume, and inspect a
company workflow whose nodes execute through the same teammates, tools,
approvals, budgets, and journal as conversational work.

## Why this matters

Workflow TOML is parsed and displayed today, but the graph is primarily a
read-only description. A company runtime needs workflows to be durable
operating procedures rather than diagrams. Building a separate automation
engine would violate the reuse-first design, so execution should compile into
existing runtime and harness primitives.

## Proposed capability

- Edit workflow metadata, nodes, edges, assignments, inputs, and enablement.
- Validate cycles, unreachable nodes, missing assignments, unavailable tools,
  unsafe outputs, and incompatible input/output contracts before publishing.
- Version published workflows while allowing drafts to change independently.
- Trigger runs manually, on a schedule, from inbound events, or from another
  approved workflow.
- Execute teammate, tool, HTTP, condition, approval, and output nodes.
- Persist run state and node attempts so work resumes safely after restart.
- Pause naturally at approvals or missing input.
- Retry transient failures with bounded policy and route exhausted work to a
  visible failed state.
- Support cancellation and compensation guidance for completed side effects.
- Show a run timeline with inputs, outputs, cost, approvals, and errors.
- Provide a test mode using mocks and fixture inputs without external effects.

## Acceptance boundary

- Published definitions are immutable; edits create a new version.
- Every side effect uses the existing approval gate and idempotency journal.
- A restart can resume a run without repeating committed effects.
- Workflow runs respect company and teammate budgets.
- Secrets are referenced by handle and never embedded in workflow TOML.
- Test mode proves routing and output shape without performing real effects.
- The console can distinguish draft, active, paused, completed, failed, and
  canceled runs.

## Likely implementation seams

- `src/company/workflow_file.rs` for definition and validation
- an execution adapter over the owning workflow/runtime crate rather than a new
  graph engine inside OpenCompany
- `src/runtime/journal.rs`, approvals, schedules, and event ingestion
- new workflow-run storage ports plus backend conformance tests
- REST writes, GraphQL reads, and `frontend/src/views/WorkflowsView.tsx`

## Open questions

- Which workflow engine owns graph scheduling and checkpoint persistence.
- Whether loops are permitted in v1 or require explicit bounded iteration.
- How schema evolution handles runs pinned to older workflow versions.
- What compensation metadata is required for nonreversible tools.
