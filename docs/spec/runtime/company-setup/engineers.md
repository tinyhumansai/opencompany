# First-run company setup — engineering notes

The intent lives in [overview.md](overview.md). This page is where it meets
code and is the part most likely to go stale — trust the code over this list.

## Phase 1 needs

| Need | Where it lives | Status |
| --- | --- | --- |
| Create an agent | `POST …/team` (`add_member`, `src/server/ops/team.rs`) | exists |
| Set an agent's role/description | `PATCH …/team/{agent_id}` (`src/server/ops/team_agent.rs`) | exists |
| Read the roster (first-run check, D4) | `GET …/team` (`list_team`) | exists |
| Tell baseline teammates from staffed ones | `TeamMemberDto.global` (`list_team`, `team_agent::is_global`) | exists |
| Desk and agent folders | `ensure_workspace_scaffold`, `ensure_agent_folder`, `ensure_desk_folder` (`src/company/workspace_scaffold.rs`) | exists |
| Talk to an agent afterwards | existing chat surface | exists |
| Answers → proposed roster | — | **new** |
| The three-question dialog + build-out screen | `frontend/src/` | **new** |
| Storing the raw answers for Phase 2 | — | **new** |

### Phase 2 needs (not now)

| Need | Where it lives |
| --- | --- |
| Create a workflow | `POST …/workflows` (`create_workflow`, `src/server/ops/workflows.rs`) |
| Scheduled runs | `[[schedule]]` / cron, `src/runtime/cron.rs`, `src/runtime/workflow_scheduler.rs` |
| Intent → workflow graph | `run_workflow_build_pass` (`src/harness/workflow_build.rs`), see [workflow-build.md](../workflow-build.md) |
| Connections | `src/server/ops/connections.rs` (`oauth` feature) |

### Notes on the decisions

- D1 maps to `[policy].mode`, whose values are `readonly`, `supervised`
  (default) and `full` — see [manifest.md](../manifest.md) and
  `Reach` in `src/policy/consequence.rs`. Because we are not asking, setup
  inherits the default and needs no route to change it. A settings-page control
  is a follow-up, and matters more in Phase 2.
- D4 is a deliberate departure from `frontend/src/tour/state.ts`, which keeps
  first-run state in `localStorage` and documents why (no per-user field on
  `UserRecord`). Fine for a tour, wrong for anything that creates records.
  Derive from an empty roster instead.
- **Setup must not call `POST /api/v1/companies`** (`src/server/provision.rs`).
  That route requires the `platform` scope and belongs to the control plane. A
  company can arrive two ways — provisioned by the platform before anyone signs
  in, or run locally by its owner — and setup has to work in both. So it
  *populates the company the session is already scoped to*, through the
  operator-scoped routes in `src/server/ops/`.
- The build-out screen wants progress it can render as it happens. The event
  vocabulary in [events.md](../events.md) already carries per-entity events and
  the console's existing stream (`frontend/src/hooks/use-events.ts`) is the
  natural transport, rather than a bespoke polling endpoint.

**Undecided:** where the stored answers live. They need to outlive the setup
session so Phase 2 can read them, and they are company-scoped rather than
user-scoped. Worth settling in Phase 1 rather than retrofitting — getting this
wrong is the one Phase 1 decision that would force rework in Phase 2.
