# RunStore

The attempt record behind the Task Detail **Attempts** tab. Part of the port
contracts indexed by [ports.md](ports.md); the console-surface stores it sits
alongside are in [ports-console.md](ports-console.md).

One attempt at work, and its trace (`src/ports/runs.rs`). A `RunRecord`
carries the agent it belongs to, its 1-based `attempt` ordinal, a status, the
cost it accrued, and — on failure — why. A `RunStepRecord` is one entry of its
trace, keyed `(run_id, step_seq)` on a run-scoped dense counter rather than an
`EventSeq`.

Since issue #983 the **card is optional** (`task_id: Option<String>`) and a
`chat_id` sits beside it, because a chat turn is an attempt at work that
frequently opens no card. A card-less run is always `attempt` 1 — with no card
there is nothing for a second attempt to be the second of — and it never answers
a per-card filter, which is what keeps the Attempts tab honest (see below). Both
fields are `#[serde(default, skip_serializing_if = "Option::is_none")]`, so a row
written before them loads and a dispatch row serializes byte-identically; the
sqlite mirror column needed a table rebuild to drop its `NOT NULL`, run
idempotently on open.

```rust
pub trait RunStore: Send + Sync {
    // storage verbs
    async fn create_run(&self, company: &CompanyId, spec: NewRun) -> Result<RunRecord>;
    async fn get_run(&self, company: &CompanyId, id: &str) -> Result<Option<RunRecord>>;
    async fn put_run(&self, company: &CompanyId, run: &RunRecord) -> Result<()>;
    async fn list_runs(&self, company: &CompanyId, filter: &RunFilter)
        -> Result<Vec<RunRecord>>;
    async fn append_run_step(&self, company: &CompanyId, step: &RunStepRecord) -> Result<()>;
    async fn list_run_steps(&self, company: &CompanyId, run_id: &str)
        -> Result<Vec<RunStepRecord>>;

    // transitions — provided methods; legality is enforced here, not per backend
    async fn begin_run(&self, /* company, id, trigger_event_seq */) -> Result<RunRecord>;
    async fn finish_run(&self, /* company, id, outcome */) -> Result<RunRecord>;
}
```

**The transitions are the API; the storage verbs are the seam.** `create_run`
mints `Pending` and allocates the attempt ordinal, `begin_run` moves
`Pending → Running`, and `finish_run` settles a run into a parked or terminal
status. Legality lives in the provided methods, so no backend can re-derive the
state machine and drift from the others. `put_run` writes a row verbatim with no
check at all — it exists because Rust cannot hide a trait method from a `dyn`
caller, and it is documented as the backend seam rather than something to call.

`RunStatus` is `pending · running · waiting_approval · paused · succeeded ·
failed · cancelled`. The two parked statuses are separated by **who unblocks
them**: `waiting_approval` means a *person* must act; `paused` means anything
else must — a dependency, a rate limit, a missing credential, a retry, an
operator steer. Defining the split by who resolves it, rather than by cause,
keeps it correct as new blocking reasons appear. `waiting_approval` is
re-enterable: approval grants are single-use and argument-exact, so a run that
could only stop once would force approvals to be batched into one prompt.

**Boot reaping.** `reap_orphaned_runs` runs at startup, before dispatch and the
scheduler spawn, and settles every `pending`/`running` row as `failed` with an
orphan reason. This is a proof rather than a timeout heuristic, resting on three
invariants held elsewhere in the runtime: cycles are process-local
`tokio::spawn`s, exactly one process may write a given company's journal, and
cycles serialise on a per-company mutex. Any active row present at boot is
therefore necessarily dead. The two parked statuses are never reaped — parked is
not orphaned.

The fs backend stores runs in `runs.jsonl` (last-write-wins per id) and steps in
`run-steps.jsonl` (a true append, folded per `(run_id, step_seq)` on read).
Deliberately not one file per run: that would make a run id a path component,
and a store must never let an id it did not mint address the filesystem.

## Who writes a run, and when

A run wraps a cycle; it never replaces one. Four writers, in order:

1. **`CompanyRuntime::dispatch_task`** — the single choke point every dispatch
   passes through — mints the `Pending` row *before* the cycle is spawned, and
   puts its id on the `TaskDispatched` event so the journal is self-describing.
   If the row cannot be written the dispatch proceeds anyway with
   `run_id: None`: record-keeping never fails the work it records.
2. **`CycleRunner::run_locked`** calls `begin_run` right after the event's
   append yields its seq — the serial lock is held and the seq now exists, so
   the row can name the exact log line that drove it. After the brain returns
   (`Ok` *or* `Err`) a **terminality backstop** settles any row still claiming
   to be live, so a brain that ignores `TaskDispatched` or errors out cannot
   strand one. Only a panic escapes it, which is the boot reaper's job.
3. **`HarnessBrain::run_task`** does the rich settle: the `TaskRunEnd` the steer
   loop yielded maps to a `RunStatus` (`lifecycle::run_status_for`), the folded
   cost and step count ride along, and a failure carries its reason. It returns
   before the backstop runs, so the rich settle always wins.
4. **The trace sink** (`harness::run_trace::RunTraceSink`) writes each step
   **during** the turn, from the collector task that already drains the harness
   progress stream. A tool call's start persists as `running` and its completion
   re-writes the same `step_seq` finalized — which is why killing the host
   mid-run leaves the prefix behind instead of nothing. The await lives in the
   collector, never the model loop, so a slow store slows only trace
   persistence. One sink spans every turn of the attempt (redirect re-runs, and
   a delegate's turn), so ordinals stay dense and cost folds across all of them.

The explicit price is write amplification: one row per step plus roughly three
status writes per run, against one event before. Affordable because cycles
serialise per company.

**Review vs paused at the settle.** A run that otherwise succeeded while parking
at least one approval finishes `waiting_approval`, not `succeeded` — a person
must act. A failed, cancelled or paused run keeps the reason it stopped;
relabelling it "waiting on you" would hide that reason.

**…and its card parks rather than landing in review (issue #465).** The status
answers *who unblocks this*; the column answers *has the work happened yet*.
`column_for_settled_run` therefore maps `waiting_approval` to `paused`: a run
stopped at a call it was not allowed to make has produced nothing to accept —
in the reported case, a desk whose *first* tool call parked, nothing at all.
The teeth are that `in_review` is defined by the verdict consuming it
(`review_landing_column(Approve)` writes `done`, the only route there), so a
parked card sat one gesture from being filed as finished; #337 closed the
automatic route to that state and this closes the manual one. The
parked-vs-review distinction survives on `RunStatus`, which is where the console
reads it ("Waiting on you"). This *narrows* epic #183 decision 2 rather than
overturning it: that rule splits waiting-on-a-person from waiting-on-a-system,
and never claimed the work had finished.

**A runtime being replaced is a fifth writer.** Writer 1 mints the row before
writer 2 can start it, so a runtime swapped in between refuses the cycle and
settles the row itself — and the boot reaper is *not* a safe fallback mid-life.
See [rebuild.md](rebuild.md#attempt-rows-242).

## Correlation fields elsewhere

Four additive `Option<String>` fields point back at a run. All are
`#[serde(default, skip_serializing_if = "Option::is_none")]`, so **no migration
and no backfill**: a record written before them loads with `None`, and an
untagged one serializes byte-identically to how it did before.

| Carrier | Meaning when set |
|---|---|
| `CompanyEvent::TaskDispatched.run_id` | the attempt this dispatch opened |
| `Effect.run_id` | the attempt whose turn parked this approval — stamped at the dispatch boundary, never in `ApprovalPolicy::effect_for` (a policy is per-agent and outlives runs), so a chat-parked effect stays `None` |
| `ArtifactVersion.run_id` | the attempt that wrote *this revision* — per version, so a card dispatched twice keeps both links |
| `UsageSample.run_id` | the attempt a turn's tokens were spent under; attribution only, no ledger semantics change |

Old `RunRecord`s are never synthesised from historical `AgentReply` events:
fabricating identity for attempts nobody recorded would be worse than a
pre-existing card honestly showing zero of them.

### A chat turn does get a row — and still not a card's attempt (#806, #983)

Issue #806 refused to synthesise a run **for a card**, so that a turn which
authored something inline (`create_workflow`, say) could hang a `TaskOutput` off
it. That would have made the Attempts tab claim work was attempted at a card
when none was. `TaskOutput` names *what produced it* as a closed set instead —
`TaskOutputSource::Run` or `TaskOutputSource::ChatTurn` — which keeps "every card
in Done links to what it produced" (#339) true without weakening what a run
means.

Issue #983 mints a row for the **turn itself**, prospectively, and that is a
different claim: the turn *is* a work attempt, it has a status worth reading, and
before this nothing durable recorded that one was owed — so a turn killed with
the pod left no trace at all. The two coexist because the row names no card:
`RunFilter::for_task` does not match an absent `task_id`, so a chat turn never
appears in `GET …/tasks/{task_id}` → `runs[]` and the Attempts tab is exactly
what #806 left it as. The turn is reachable through the company-wide
`GET …/runs`, and by desk through its `chat_id`.

`create_run` at accept, `begin_run` once the cycle holds the per-company serial
lock, `finish_run` when the turn settles. That placement is deliberate: `Pending`
therefore means *queued behind other turns* and `Running` means *owns the lock*,
which is the distinction an operator on a busy company needs. Reusing this store
is what makes the feature small — transition legality, the step trace,
`list_stale_active` and `reap_orphaned_runs` all apply unchanged, and the boot
reaper's proof (a turn is a process-local spawn, one process owns the journal,
turns serialise on one mutex) holds for a chat turn verbatim.

Old `RunRecord`s are still never synthesised from historical events. See
[ports-state.md](ports-state.md) for the task record itself; a card a chat turn
opens now carries that turn's id in `origin_run_id`.

## Reading runs back

Three surfaces, all in `src/server/ops/runs.rs`, under both scope forms:

| Route | Answers |
|---|---|
| `GET …/runs?task=&agent=&status=&limit=` | the company's attempts, newest first |
| `GET …/runs/{run_id}` | one attempt plus its full persisted step trace |
| `GET …/tasks/{task_id}` → `runs[]` | the card's attempts, additive on the task detail read |

Each hands its predicates to `RunStore::list_runs` as a `RunFilter`. **No route
here folds the journal** — that is the whole reason a run is state rather than
an event, and the sibling `GET …/workflows/runs` (which does fold, and says so)
is the cost being avoided. `?status=` takes a comma-separated list and refuses
an unknown word with a `400`, because a typo'd filter answering `[]` is
indistinguishable from "nothing matched".

`?agent=` (issue #1573) narrows to one desk, and is what the console's
per-teammate run history reads. It is a **store** predicate rather than a slice
of a fetched page for the reason `limit` makes unavoidable: filtering in the
console would make `limit` mean "the newest N attempts in the *company*, of
which some happen to be this desk's", so a teammate who had been quiet while
the rest of the company was busy would render as one who had never run. Unlike
`?status=` it is not validated against the roster — a teammate can be removed
while its attempts remain, and refusing to show that history would erase the
record of work that did happen; an id nobody ran simply answers `[]`.

Both indexed backends push it down, and both had to **backfill** to do so
honestly. `agent_id` is a mirror of a field that has always been inside the
stored record, so every row written before the column existed carries the desk
in its blob and `NULL`/absent in its index — and a `NULL` never matches. Left
alone, the new filter would have silently omitted precisely the history an
operator opening a teammate for the first time most wants. sqlite copies it
across in `heal_runs_agent_id` (an `ALTER`, one `UPDATE … WHERE agent_id IS
NULL`, then the index — idempotent, and matching nothing on every open after
the first); MongoDB does the same in `backfill_run_agent_ids` at connect,
document by document, because the desk lives inside a *string* of JSON there
and the server cannot reach it. The two differ in how a failure lands: sqlite
runs the heal synchronously in `SqliteStore::from_conn` with a propagated
error, so a backfill that cannot run prevents store initialization; MongoDB's
is best-effort at its call site — a company that will not start is worse than
one whose oldest rows are not yet filterable by desk, and the migration is
spawned so a slow first boot never sits in front of `/healthz`.

Three things the wire shape refuses to imply, each a state the write path really
produces:

- **`phase`** (`active` · `parked` · `terminal`), projected from
  `RunStatus::phase`, is how a reader decides liveness. `finishedAtMillis` is
  absent for a *parked* run exactly as for a running one — a parked run can
  resume — so inferring liveness from the timestamp renders an attempt waiting
  on a person as running forever.
- **`stepCount` / `stepCountCapped`.** The count is the high-water ordinal
  persisted, capped at `run_trace::MAX_RUN_STEPS`, and written on the settle —
  so it reads `0` throughout a live run and stops meaning "steps the agent
  took" once capped. `usage` settles alongside it and is provisional until then.
- **A step's `status`** (`ok` · `error` · `running`) rides beside its `kind`. A
  host killed mid-tool-call leaves that call recorded `running` — the point of
  an incremental trace — meaning in-flight-when-the-trace-stopped, never failed.

Steps project into the console's existing `TimelineEntry` contract (`seq` /
`atMillis` / `kind` / `label` / `detail`, plus `status` and `elapsedMs`), so
`kind` widens additively to include `tool_call` · `thinking` · `note` and the
grouped-timeline renderer is reused rather than reinvented. `usage` is re-cased
to camelCase by a local DTO — `TokenUsage` carries no `rename_all` because its
field names are the decode contract for already-journaled events.

Run detail is **refresh-on-read**: steps persist incrementally, so re-reading a
live attempt shows the progress since. Streaming would widen the harness turn
stream for something a re-read already answers.

## The workflow-run join

`RunRecord` carries `workflow_run_id` and `node_id`, both optional.

A workflow `agent` node's turn has **neither a card nor a conversation**, so
before these existed `RunStore` — keyed on exactly those two — could not name the
attempt at all. It was not that the join column was missing; the *row* was
missing, because `run_background` took no `RunTraceSink` and the node therefore
minted nothing. A node was green or red and that was the whole of what could be
known about it.

Both fields are additive in the same shape `task_id`/`chat_id` took: a row
written before they existed loads with `None` and re-serializes byte-identically.
**There is no backfill**, and that is not a shortcut — a pre-existing run
genuinely belongs to no workflow, so `None` is true rather than tolerated.

`RunFilter::for_workflow_run` selects one run's nodes; `GET {scope}/runs?workflow_run=`
exposes it over REST, and `Company.agentRuns(workflowRunId:)` over GraphQL.

### `begin_run_untriggered`

`begin_run` stamps the seq of the journal event that drove the attempt. A
workflow node is activated by the engine walking a graph, not by a
`TaskDispatched` the journal recorded, so there is no seq to stamp.
`trigger_event_seq` is already `Option`, so leaving it `None` is the record's own
way of saying "nothing in the journal drove this" — passing a made-up seq (or
`0`) would point every workflow attempt at an unrelated event and quietly corrupt
any reader that follows it. Transition legality is identical.
