# Pausing a workflow, and the disarm rule (issue #276)

`PUT …/workflows/{wid}/enabled` with `{"enabled": false}` stops a workflow's
schedule without deleting it. **Pausing stops the schedule, not the workflow**:
a paused workflow keeps its graph, stays in the picker and still runs from the
Run button. `WorkflowScheduler::tick` is the only execution gate that uses the
flag to skip scheduled runs.

The switch is `CompanyRecord.disabled_workflows`, **not** the manifest's
`[workflows].enabled`. That list is a declaration of what this company was
provisioned with, and `merge_enabled_workflows` (issue #208) rebuilds it at boot
from seed ids ∪ surviving overlay ids — so "off" expressed as absence from it
would re-arm itself on the next restart, which is the one failure mode a safety
switch may not have. Read `workflow_enabled`, write `set_workflow_enabled`;
never touch the field directly.

Toggling is a **wider** set than editing: any id that resolves to a real graph,
including a seed-defined one that `PUT`/`DELETE` refuse with a `409`. Pausing
does not touch the source tree and can only remove capability, so it cannot let
a runtime write outlive a seed rollback. A manifest-`enabled` id with no graph
is still a `409` — no graph, no schedule to stop. There is no `expectedVersion`:
a switch carries no content to overwrite, and requiring a token would make a
seed-backed workflow untoggleable since only overlay bodies have one.

**An edit disarms; nothing ever silently arms.** OpenHuman's `flows_update`
forces `enabled = false` when an edit turns a manual or absent trigger into an
automatic one, after a flow of its own started running on an unreviewed 8am
schedule. This host adopts that rule and widens it to **create**, because
`create_company_workflow` is also the orchestrator's `create_workflow` tool — so
an agent cannot arm a cron, and an operator cannot route around the rule by
authoring the graph fresh instead of editing it. Changing an already-armed
workflow's cron does **not** disarm: the reviewed decision is "automatic at
all", and putting a re-enable click behind every typo fix is how an operator
learns to click through it. `a_switched_off_workflow_does_not_fire` and
`an_edit_that_adds_a_schedule_switches_the_workflow_off` pin the two halves.

**A schedule that cannot run is refused arming (issue #976).** `set_company_workflow_enabled`
rejects `enabled = true` when the graph carries a trigger schedule and has **no
node other than its trigger**. Such a graph fires on time, runs nothing and
reports nothing: on staging `campaign` was one resume away from exactly that.

**Refused at arming, not at save**, and the distinction is the fix. Saving a stub
mid-authoring is legitimate — the console drops a Start node first and adds
stages after, `parse_workflow` was made lenient on purpose (issue #661) to allow
it, and refusing at save would also refuse every existing seed and legacy body on
its next edit. Saving promises nothing; switching a schedule on promises that
something happens. Arming is also already the human gate the disarm rule above
forces a scheduled graph through, and it has the parsed graph in hand for the
journal name — so the check costs no extra load.

Switching such a workflow **off** stays allowed, on the same principle as the
unparseable-body case: an operator must always be able to stop a thing. A manual
(unscheduled) stub is left alone — running one by hand is the author's business,
and the run says so itself (below).

**A run of a stage-less graph records a notice.** The engine runs such a graph
happily — no stage fails, because there is no stage — so before this it settled
as an ordinary finished run, and `QA Test Pipeline` banked six of them. It now
carries `WorkflowRun.notices` (the issue #638 channel) saying it had nothing to
run. Deliberately a notice and **not** an error: nothing broke and nothing was
attempted, so marking it failed would put a half-authored stub into the failure
count beside runs that genuinely went wrong — the same call issue #925 makes one
level down with `NoDestinationConfigured`. `WorkflowFile::has_runnable_node` is
the single predicate behind both halves, for the reason `trigger_schedule` is
single: two copies that disagreed would let a graph be refused a schedule and
still run silently, or the reverse.

**In the console.** Each row carries a switch, rendered from `enabled` on the
list read. Like `editable`, only an explicit `false` counts — a host predating
issue `#276` sends no field, and `undefined` must not read as paused. A row the
disarm rule just switched off reports `enabled: false` on the create/update
response
itself, so the console learns about it from the write it made.