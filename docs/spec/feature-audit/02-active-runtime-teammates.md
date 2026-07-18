# Active Runtime Teammates

## Outcome

Every teammate accepted by the Operator becomes a real, addressable,
policy-bound worker with an inbox, workspace, memory namespace, tools, skills,
budget, and execution history.

## Why this matters

Manifest teammates are constructed through the embedded OpenHuman harness, but
operator-added roster overlays are currently presentation-only. The Team view
can therefore describe a larger organization than the runtime can actually
execute. That mismatch will become more visible once onboarding and continuous
company review can propose new roles.

## Proposed capability

- Promote an accepted roster overlay into a durable runtime teammate.
- Assign a stable identity that survives restarts and safe configuration edits.
- Materialize an isolated workspace and memory namespace.
- Attach only the tools, skills, channels, and credentials granted to the role.
- Apply company policy plus narrower per-teammate restrictions.
- Enforce daily and per-task spend limits beneath the company budget.
- Address teammates directly from desks, tasks, schedules, and workflows.
- Support graceful activation, suspension, retirement, and replacement.
- Drain or reassign in-flight work before retirement.
- Preserve historical attribution after a teammate is removed.
- Rebuild the effective roster deterministically from manifest plus approved
  overlays without rewriting `company.toml`.

## Lifecycle

Suggested states are `draft`, `activating`, `active`, `suspended`, `retiring`,
`retired`, and `failed`. State transitions should be journaled and idempotent.
Activation failure must leave a diagnosable draft rather than a phantom active
teammate.

## Acceptance boundary

- Adding a teammate does not mutate the version-controlled manifest.
- An active teammate can receive and complete a real task.
- Tool, credential, and budget scopes are enforced at execution time.
- Cross-company memory, workspace, and secret access is impossible.
- Restart reconstructs the same effective active roster.
- Retirement retains history and prevents new work from being assigned.
- Failed activation is visible and safely retryable.
- Every storage backend passes the same teammate lifecycle contract.

## Likely implementation seams

- `src/server/ops/team.rs` for write behavior
- `src/runtime/builder.rs` and `src/harness/` for dynamic construction
- `src/company/types.rs` for overlay and effective-roster models
- `src/ports/` and `src/store/conformance.rs` for durable lifecycle state
- GraphQL team projections and the frontend Team view
- task, inbox, schedule, and workflow assignment paths

## Open questions

- Whether each teammate owns one long-lived OpenHuman agent or a reconstructable
  execution profile.
- How mandate edits affect cached state and ongoing work.
- Whether retirement can be reversed or always creates a new identity.
- How template upgrades reconcile with Operator-created teammates.
