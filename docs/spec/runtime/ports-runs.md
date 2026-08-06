# RunStore

The attempt record behind the Task Detail **Attempts** tab. Part of the port
contracts indexed by [ports.md](ports.md); the console-surface stores it sits
alongside are in [ports-console.md](ports-console.md).

One attempt at a task, and its trace (`src/ports/runs.rs`). A `RunRecord`
carries the task and agent it belongs to, its 1-based `attempt` ordinal, a
status, the cost it accrued, and — on failure — why. A `RunStepRecord` is one
entry of its trace, keyed `(run_id, step_seq)` on a run-scoped dense counter
rather than an `EventSeq`.

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

## Reading runs back

Three surfaces, all in `src/server/ops/runs.rs`, under both scope forms:

| Route | Answers |
|---|---|
| `GET …/runs?task=&status=&limit=` | the company's attempts, newest first |
| `GET …/runs/{run_id}` | one attempt plus its full persisted step trace |
| `GET …/tasks/{task_id}` → `runs[]` | the card's attempts, additive on the task detail read |

Each hands its predicates to `RunStore::list_runs` as a `RunFilter`. **No route
here folds the journal** — that is the whole reason a run is state rather than
an event, and the sibling `GET …/workflows/runs` (which does fold, and says so)
is the cost being avoided. `?status=` takes a comma-separated list and refuses
an unknown word with a `400`, because a typo'd filter answering `[]` is
indistinguishable from "nothing matched".

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
