# Workflow run progress (issue #371)

Part of the [Company Events](events.md) contract, split out on this repo's
500-line Markdown cap. This file owns how a workflow run brackets itself on the
journal — its start/node/finish variants, run-id correlation, the ordering
guarantee, the interrupted-run sweep, and operator stop/cancel semantics
(issues #382/#383/#398).

A workflow run used to journal exactly one line, `WorkflowRunFinished`, written
after the run returned. Between pressing Run and that line there was no record
at all: a long run was indistinguishable from a wedged one, and a run that died
at the fourth of six nodes recorded only that it died.

Four variants now bracket a run:

| Variant | Written by | When |
| --- | --- | --- |
| `WorkflowRunStarted` | the workflow runner | before the engine call |
| `WorkflowNodeStarted` | the runner's `RunObserver` (issue #382) | as each non-trigger node begins |
| `WorkflowNodeFinished` | the runner's `RunObserver` | as each non-trigger node finishes |
| `WorkflowRunFinished` | the **caller**, via `record_run_finished` | after the run returns, on both arms |

Each node's `WorkflowNodeStarted` and `WorkflowNodeFinished` ride the **same**
unbounded observer channel, so the collector drains them in order and a node's
start is always journaled ahead of its finish.

## Run-start sender

`WorkflowRunStarted` also carries `startedBy`, identifying the source of the
run: `"operator"`, `"schedule"`, or `{ "agent": "<teammate-id>" }`. The field
is serialized in the same mixed shape as the runtime event and is available on
the operator SSE projection so consumers can attribute a run without guessing
from `scheduled` alone.

`startedBy` is optional on the wire, not a guaranteed member: a journal line
written before this field existed carries none, and replaying it must not fail
to parse, so the key is omitted rather than sent as `null`. `None` is also the
honest reading for any future entry point that genuinely has no identity to
give. A consumer — SSE or otherwise — must treat a missing `startedBy` as
"unattributed," not assume `scheduled` alone can stand in for it.


A chat turn emits dozens of steps per message; a workflow run emits **one event
per non-trigger node** — roughly eight for a six-node graph. At that volume the
journal is the right carrier, and it already feeds the operator SSE
projection — so **one append serves both the live half and the durable half**
through a single write path. A dedicated store would need a second writer and a
second read path to deliver the same two answers.

The alternative considered and rejected was adopting issue #242's `RunRecord` /
`RunStepRecord`. `RunRecord` requires a `task_id`, an `agent_id` and a per-task
attempt ordinal; a workflow run has none of those, so it would need a synthetic
task id — a lie leaking into every `RunStore` consumer, all of which assume a
run keys to a board card. `RunStepRecord`'s payload is a `TurnStep`, which has
no node-id field at all.

The accepted cost is journal growth of ~(N+1) lines per run, and a
`GET …/workflows/runs` fold that is already O(journal). A dedicated store stays
the known escape hatch if cron volume ever demands it.

### What these events deliberately do not carry

**No node output and no error text.** The engine's `ExecutionStep` carries the
node's output items; `WorkflowNodeFinished` carries a node id, a three-valued
status (`ok` / `error` / `blocked`, issue #881) and a duration, and every
`WorkflowNodeStatus` arm is a **unit** variant — so there is no `format!` that
could put a node's own words on the journal. #881 added a reading and kept that
invariant: what a blocked node is blocked *on* travels structurally on
`WorkflowRunFinished.blocked_nodes` (node id, tool names, approval ids), never
as prose.
`WorkflowNodeStarted` (issue #382) is even thinner: the node has not run, so it
carries the ids alone — no status, no duration, and never any input. This is the
same stance the live turn-progress frames take on tool args, and it matters
because the journal is read by the operator SSE projection *and* wired out to
the inference sidecar.

Nothing is lost: the run-level failure reason already lands on
`WorkflowRunFinished.error`, which is a tenant-scoped surface.

### Run-id correlation

`WorkflowRunStarted`, `WorkflowNodeStarted` and `WorkflowNodeFinished`
**require** a `run_id`; `WorkflowRunFinished` has always carried an optional one
and now populates it.
That shared id is what lets a reader group a run's node rows with its outcome,
and what lets the console overlay one past run's states onto the canvas.

**The entry point mints it, not the runner.** On the error arm the runner
returns nothing that could carry an id, and that is exactly the run whose
per-node trail is worth correlating — so the id is minted above the call, in a
`WorkflowRunContext` threaded through `WorkflowRunner::run`. Every entry point
(the console's run route, the cron scheduler, the orchestrator's `run_workflow`
tool) therefore stamps one id across both halves on every path.

`WorkflowRunContext` is a crate-internal port type with no serde impl. Putting
it in the trait signature is deliberate: it makes the compiler enumerate every
entry point, so one cannot quietly journal an uncorrelatable run.

### Ordering guarantee

A run's lines are always `Started < Node… < Finished`. The first part is
ordering by construction; the last is enforced — the runner drops its progress
channel and **joins the collector before returning**, so every node line is
durable before the caller writes the outcome. Without that join the two would
race and a fold could attach a node to a run it had already rendered as
finished.

Lines of *different* runs may interleave (two workflows can run at once), which
is why the read-side fold groups on run id rather than on adjacency.

### Interrupted runs (issue #371)

Because the start is written before the engine call, a host that dies mid-run
leaves a start with no finish. At boot that is provably dead work, on three
invariants: every entry point drives its run future **in this process**, exactly
one process owns the journal, and no entry point has run yet. So
`sweep_interrupted_runs` settles each unmatched start with a synthetic
`WorkflowRunFinished` carrying an "interrupted by a host restart" error — the
start's own `workflow_id`, `scheduled` flag and `run_id` are carried over, so
the nodes that did complete still group under it.

This is what keeps the read side honest: `GET …/workflows/runs` folds a start
without a finish as `running: true`, and that claim is only true because runs
that will never finish are settled at the next boot.

**It must not run on a rebuild.** The argument above holds at boot and is false
once a company has been serving: a scheduler-spawned run survives a live runtime
swap, so sweeping mid-life would stamp "interrupted" on a run still walking its
graph, whose real outcome would then land afterwards — two contradictory
finishes for one run id. The call site is gated on the handover being absent,
exactly like `reap_orphaned_runs`. Same lesson as #290.

### Stopping a run (issue #383)

A run an operator stops is a **third terminal reading**, not a variant of the
other two. `WorkflowRunFinished` gained a `cancelled` flag; a cancelled run
carries that flag and **no `error` at all**.

| Reading | `error` | `cancelled` |
| --- | --- | --- |
| finished | absent | absent |
| failed | the reason, naming the node when the trail names one | absent |
| interrupted by a host restart | the boot sweep's synthetic reason | absent |
| stopped by an operator | absent | `true` |

Keeping them apart is the whole point. Folding a stop into `error` would put
every deliberate cancel in the failure count and make "this run failed" the
console's answer to a button the operator just pressed; folding it into a clean
finish — which is what any reader that only checks `error` does — would report
a stopped run as a success. Both fields are `serde(default)` +
`skip_serializing_if`, so every line written before #383 decodes as not
cancelled and every non-cancelled line stays byte-identical.

A cancelled run journals a **real finish**, which is what keeps
`sweep_interrupted_runs` out of it: there is nothing left open to sweep, so the
two never write contradictory outcomes for one run id.

**The trail is truthful about how far it got.** The runner drains and joins its
progress collector on the cancel path exactly as on the completion path, so the
nodes that finished before the stop are durable ahead of the outcome. The node
that was *executing* contributes no row — it never finished, and inventing one
would answer "how far did it get" wrongly.

**How the stop works, and what it costs (issue #398).** The runner drives the
engine through `run_cancellable_with_observer`, which takes a `CancellationToken`
**and** a `RunObserver`, so a cancellable run keeps the per-node progress trail
above instead of trading it away. When an operator stops a run, the runner flips
that token: the engine checks it before each node, so a node already executing
runs to completion and is journaled, then the run winds down at the next
**boundary** carrying a real (partial) `RunOutcome` with `cancelled` set. That is
the clean path — nothing is dropped mid-await, and the collected node rows ride
back on the run response.

A node **wedged** mid-await on a stalled external call never reaches the next
boundary, so the runner bounds the wait with `CANCEL_HARD_ABORT_GRACE` and, past
it, falls back to the pre-#398 **hard abort**: it drops the engine future, which
stops the run mid-await — a node part-way through a side effect stays part-way
through it, the same class of outcome as a host `SIGKILL`, which the boot sweep
already handles. Keeping this bounded fallback is what guarantees a wedged run
stays killable. On that arm the run settles with an empty body (`cancelled_run()`)
and its trail is the journal; on the clean arm the body carries the node rows.

On the hard-abort arm the engine future must be dropped **before** the observer,
because it owns observer `Arc` clones inside its per-node handlers; dropping the
observer alone would leave the progress channel open and stall the cancel until
the drain timeout, *on top of* the grace. Today the borrow checker enforces that
ordering, but only incidentally (the engine borrows the observer), so a
cancel-latency bound is asserted as well.

### The node-started bracket (issue #382)

tinyflows' `RunObserver` gained an `on_step_start` hook, so the runner now
journals a `WorkflowNodeStarted` immediately before each non-trigger node's first
attempt. A console showing which node is *currently* executing therefore reads it
straight off the stream rather than deriving a frontier from graph topology — the
pre-#382 guess marked both arms of a branch until the real finishes corrected it.

A failing node **is** reported exactly too: a node that dies under the default
`stop` policy still emits a finish step with `Error` status before the run ends,
so failure attribution is exact rather than inferred.

That is also true of a **blocked** node (issue #881), and it is why the journal
line says `error` there. A node whose agent turn had a tool call parked halts its
branch by returning a capability error, so the observer reports `Error` — the
engine's own honest account of what it saw. The host knows *why*, and
reclassifies on the way out: `WorkflowRun` marks the node `blocked` and the run
settles with no error at all, and `GET …/workflows/runs` relabels the folded node
row against the finish's `blocked_nodes` list so the history's chips agree with
its terminal reading. The durable node row is deliberately left as the engine
wrote it — rewriting it would make the live progress frames disagree with the
engine that produced them.

## `agent_run_id` on the node events

`WorkflowNodeFinished` carries `agent_run_id`: the attempt the node's agent ran
as, when it opened one.

This does not weaken the events' "structural ids, never payloads" stance — a run
id is a structural id, no more revealing than the `node_id` beside it, and it is
reachable by the same operator through `GET {scope}/runs`. What it buys is the
join without a second round trip: a console watching the canvas can open a node's
step trace directly instead of searching for which attempt was its.

Absent for a non-agent node, for a host that records no attempts, and on every
line written before the field existed (`serde(default)` + `skip_serializing_if`,
for the reason `diagnostics` beside it has them — the journal is replayed at
boot, so a field without a default turns pre-existing lines into silent history
loss).

The id travels from the node to the journal through `RunAttempts`, a shared
side-channel in the `RunNotices`/`RunBoard`/`RunBlocks` family. It cannot ride
the node's output: `run_agent`'s return value *becomes* the node's output, so a
non-output fact placed there lands in the next node's `=items` binding — the
mistake `RunBlocks` exists to document.
