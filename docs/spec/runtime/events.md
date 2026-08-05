# Company Events

The `CompanyEvent` vocabulary carried by the [`EventLog`](ports.md#eventlog)
port, and the correlation rules that let a reader fold one company's
append-only journal back into per-task, per-approval and per-run views.

Split out of [`ports.md`](ports.md) (issue #371), which had grown past this
repo's 500-line cap for a Markdown file. That file still owns the *port
contracts* — the traits and their method signatures; this one owns the *event
vocabulary* those traits carry, which is the half that keeps growing.

## The journal's shape

Append-only, replayable, **single-writer**, and company-scoped. Boot replays the
tail to rebuild in-flight state. Every variant is serialized internally-tagged
under `kind`, so each JSONL line is self-describing.

Two properties are load-bearing everywhere below:

* **Additive-only.** A new variant, or a new field carrying
  `#[serde(default, skip_serializing_if = …)]`, cannot change how an
  already-persisted line loads or how an existing event serializes. That is
  what lets the vocabulary grow with no journal migration and no break in the
  cross-backend export/import round-trip. The cost is accepted and stated: an
  **older binary cannot decode a newer journal's variants** — the same posture
  every variant addition has shipped with.
* **One writer per company.** The journal is opened by exactly one process, and
  a rebuild inherits the open handle rather than reopening it (two handles
  interleave a record and its newline onto one line and brick the next replay).
  Several boot-time sweeps rest on this; see
  [Interrupted runs](#interrupted-runs-issue-371).

## Variants

`CompanyEvent` variants: `OperatorMessage`, `WebhookReceived`,
`ScheduleFired`, `A2aTaskReceived`, `ApprovalParked` (issue #379 — an effect
is now waiting on the operator; see [In-conversation
approvals](#in-conversation-approvals-issue-379)), `ApprovalResolved`,
`FeedbackFiled`,
`PaymentReceived`, `LifecycleChanged`, `AgentReply`, `MemoryFactDeleted`,
`TaskDispatched`, `McpCallFailed`, `WorkflowCreated` (a new saved workflow
graph was authored + enabled via the console `POST …/workflows` route or the
orchestrator's `create_workflow` tool; journaled best-effort after persist),
`TaskSteered` (an operator paused, cancelled, or redirected an in-flight task
or delegation), `DeskTaskCompleted` (a dispatched board task finished its run —
the terminal anchor a per-task timeline ends on; "completed" means the run
stopped, not that it succeeded, and `column` carries where the card landed),
`TaskDiscussionPosted` (a human posted to a card's discussion thread, issue
#335 — the Discussion tab's whole store, folded back out by
`GET …/tasks/{task_id}` beside that card's timeline), `WorkflowUpdated` /
`WorkflowDeleted` (issue #259 — a saved graph was replaced wholesale or removed;
neither carries the TOML body, deliberately, since the journal reaches readers
that have no business holding agent prompts or destination addresses),
`WorkflowRunFinished` (issue #228 — the durable record of what a run did, from
every entry point) and, from issue #371, `WorkflowRunStarted` /
`WorkflowNodeFinished` (the per-node progress trail; see
[Workflow run progress](#workflow-run-progress-issue-371)).

### Per-task event correlation (issue #185)

The journal is company-scoped, so the events a dispatch *produces* cannot be
filtered back to their task by shape alone. `AgentReply` and `McpCallFailed`
therefore carry an optional `task_id`, stamped by the harness when the
producing turn ran inside a `TaskDispatched` cycle and absent for an ordinary
chat turn. Together with the `TaskDispatched` / `DeskTaskCompleted` anchors,
that is what `GET …/tasks/{task_id}` filters on to assemble a task's timeline.

Both fields are additive — `#[serde(default, skip_serializing_if = …)]` — so
every already-persisted event loads unchanged and an untagged event serializes
byte-for-byte as it did before the field existed. No stored log needs
migrating, and the cross-backend export/import round-trip is unaffected.

`TaskRecord` gains `parent_task_id` on the same contract, recording the
task-to-task edge that `origin_chat_id` (a *conversation*, shared by every
sibling spawned in that thread, and absent entirely on a board-native card)
cannot express. It is the parent half of the Task Detail screen's lineage.

`OutboundMessage` gains `task_id` on the same contract (issue #246): the card a
chat turn **opened**, so the console can say a card exists instead of leaving an
operator to notice it on the board. It is journaled onto that turn's
`AgentReply.task_id`, which widens that field's meaning from "the dispatch that
produced this reply" to "the card this reply is about" — a card-creating reply
now also appears on that card's timeline, which is the lineage an operator
wants and costs no schema change. A turn that opens several cards reports the
**first**: the journal field is a single optional id, and widening it would
break the byte-identical round-trip, so the claim is incomplete but never wrong.
Both `chat/history` surfaces (REST and GraphQL) project it from the shared
`MessageView`, so the chip survives a transcript reload on either.

### Per-task approval correlation (issue #333)

`ApprovalResolved` carries an id, a verdict and an actor — never a task — so
the same problem reaches the approval queue, and worse: a *parked* approval has
no event at all. A task's Approvals tab could therefore only filter the
timeline for resolutions that happened to fall inside the card's run window,
which showed nothing while an approval was actually waiting and let a second
card worked in that window absorb the first's sign-offs.

The link is recorded where the approval is: the runtime journal's
`ApprovalParked` record gains a `task` field, stamped by the cycle that parked
the effect. A cycle knows which card it is working from its own trigger
events — a `TaskDispatched`, or an `ApprovalResolved` whose approval was itself
parked for a card, which is how a run needing two sign-offs keeps the link
through the first.

`task` is a two-armed link, not an optional id, and the distinction is the
whole correctness of the feature:

| On disk | Means | Read side |
| --- | --- | --- |
| `{"link":"task","id":"t-1"}` | that card owns it | shows on `t-1`, and only there |
| `{"link":"unlinked"}` | no card owns it | shows on no card |
| *absent* | written before #333 | falls back to the run window |

An optional id collapses the middle row into the last one, and the middle row
is not an edge case: every workflow delivery, operator-chat turn and scheduler
tick parks unlinked. Treating those as "unknown" sends each of them to whatever
card happened to be running, along with that card's `waitingSince` — the exact
misattribution this issue exists to end. So a host from #333 onward always
writes one of the first two, and absence means one thing only.

#### Which key is authoritative

Two keys correlate an approval to work, and they are kept **both**:
`Effect.run_id` (issue #242) is **attempt-level**, and `ApprovalParked.task` is
**card-level**. The read side resolves them in this order:

```text
card = if let Some(run_id) = effect.run_id { run_store.get(run_id).task_id }  // authoritative
       else { approval_parked.task }                                          // fallback
// neither recorded → genuinely unlinked, and NOT the run-window fallback
```

`run_id` wins wherever it is present because a `RunRecord` names its card, so a
run id resolves to a task — while a task id can never say which *attempt*
parked an approval. #183 settled that repeat trips through review are normal,
so two attempts on one card is the expected case, and only `run_id` separates
them.

It cannot be the only key, though: `run_id` is `None` by design for every park
with no attempt behind it — a chat turn, a workflow delivery, a scheduler tick,
and the hosted brain's own gate — whereas `task` is stamped in
`CycleHostImpl::park`, which *every* park path passes through. Neither key is a
superset of the other, so "pick one" is not available. The card-level key also
inherits through a resolution (an `ApprovalResolved` whose approval was itself
parked for a card keeps that card), which the attempt-level one does not do.

This is why the three-state link above is load-bearing rather than pedantic:
with two keys, "unlinked because chat-parked — no run and no card" has to stay
distinguishable from "not stamped because it predates #333", and only the
second may fall back to the run window.

A batch is ambiguous — and stamps nothing — when it names two different cards,
or when it carries a card's dispatch alongside a turn that is its own work (an
operator message, a webhook, a schedule tick, an inbound A2A task, or a
resolution of an approval known to belong to no card). A cycle is a unit of
batching, not of work, so "the card this cycle is for" is only well defined
when nothing else rides along. Issue #357 guards the same seam per *attempt*
with a queue-position boundary; this rule only has to stop the cross-turn leak.

The journal keeps a per-approval origin index (park instant, effect kind,
task link) because the parked effect is dropped from the queue on resolution
and nothing else can answer what a resolved approval was.
`GET …/tasks/{task_id}` returns `approvals[]` from it, joined by id.

**That index is unbounded.** It holds one entry per approval ever parked, for
the life of the process, and is never pruned — resolution and expiry remove the
queue entry but deliberately not the origin. #333 widens each entry from a
`u64` to a `u64` plus two `String`s (the effect kind, and the task id when
linked). No journal rotation exists today, so replaying every `ApprovalParked`
line on `load` is the only path to rebuild it, and it is the correct one. If
rotation is ever added, this index is the first thing that must survive it: a
rotated-away park line silently makes its approval unreadable.

The field is additive on the same contract as the rest — a pre-#333 line
replays with no link and keeps the old run-window correlation, so existing
history still renders.

### In-conversation approvals (issue #379)

A parked approval had no event at all — parking was journal-only — so a console
learned about a new request when its approvals feed next polled. That is far too
late to raise the request *inside the conversation that produced it*, which is
where an operator is actually looking when their agent stops to ask.

Two additions, both on the pattern above.

**`CompanyEvent::ApprovalParked`**, appended best-effort at the single park
choke point (`CycleHostImpl::park`) immediately after the journal write
succeeds. The journal is the binding record of what is parked; the event is an
advisory nudge, so a failed log write never undoes a park that already
happened — the same division `sweep_expired_approvals` draws for expiry.

It carries **an id, a dotted kind, and a thread — nothing else**. No payload and
no asker, deliberately: the parked effect's arguments are redacted and bounded
in exactly one place (`pending_approvals`, issue #372), and a payload-bearing
durable event would open a second surface that has to redact, and eventually
will not. A reader reacts by re-reading the approvals feed and renders from the
redacted `ApprovalSummary`. That costs one round trip between the frame and the
card, and buys one redaction surface instead of two.

**`ApprovalParked.thread` on the journal record**, stamped at park time from the
cycle's own trigger events by `cycle_thread_id` — the sibling of #333's
`cycle_task_id`, same exhaustive-match discipline, same refusal to guess. It
also surfaces on `PendingApproval`, `ApprovalOrigin` and `ApprovalSummary`, and
is copied onto `GrantedCall.origin_thread` when an approval mints a grant.

The id is read off `OperatorMessage.chat`, which is the only field that can do
this job. `Effect.agent` cannot: a desk channel and a direct message to that
desk's lead are answered by the same teammate, so placing a card by asker raises
one conversation's request inside the other. `chat` carries a desk id for a
channel and a roster agent id for a DM — **different strings even when the same
agent answers both**.

It inherits through a resolution, exactly as the task link does: an
`ApprovalResolved` whose approval was itself raised in a thread keeps that
thread, so a follow-up turn that needs a *second* sign-off re-parks in the
channel the first one was asked in rather than falling out of the conversation.

A plain `Option<String>`, not a two-armed link like `task`, and for a precise
reason: nothing downstream falls back to a heuristic when it is absent. An
approval with no thread matches no channel filter and stays Approvals-page-only,
which is exactly today's behaviour. So "no conversation produced this" and
"written before #379" need not be told apart — both are correct as the same
answer, which is the condition #333's enum exists to handle and this does not
have.

A batch is ambiguous — and stamps nothing — when it names two different threads,
or when it carries an addressed chat turn alongside work that is its own (a task
dispatch, a webhook, a schedule tick, an inbound A2A task). An **unaddressed**
operator message (`chat: None`) is itself a rival rather than a neutral
pass-through: it went to the orchestrator with no conversation of its own, so a
batch holding one cannot say which conversation a parked effect came from.

**The redemption reply is routed by this thread too**, and that is a bug fix
rather than a new capability. `redispatch_granted_call` journaled the
continuation `AgentReply` with `chat_id: grant.agent` — correct for a DM by
coincidence, and wrong for a desk channel, where the agent's continuation landed
in the desk lead's private line instead of the channel the operator asked in. It
now uses `grant.origin_thread`, falling back to the agent when there is none,
which is the previous behaviour kept for exactly the cases it was already right
for.

### What a retry would repeat (issue #351)

Re-entering a run re-runs its effects, and the two facts needed to warn about
that already existed separately: the gate classifies `Sign` / `Publish` /
`Identity` / capped `Spend` / first-contact `Send` as the effects it refuses to
wave through, and the journal's executed-key set records what was committed to
run. Neither reached the operator, because the key is opaque — it answers "has
this run?" and nothing else.

`EffectExecuted` therefore carries an optional `ExecutedEffect` alongside the
key: the effect kind, its amount, the board task it ran for, and whether the
gate called it irreversible. The classification is made **at execution time**,
by `ManifestApprovalGate::is_irreversible` (which delegates to the supervised
taxonomy, so there is one copy of the rules), and it is deliberately
mode-independent: a `full`-mode company executes a filing without ever parking
it, which is precisely when a retry dialog is the only warning anyone gets.

There is **no payload**. The record is read back onto an operator's screen
through `GET …/tasks/{task_id}`, which scrubs by construction, so recipients and
message bodies are never retained in the first place.

The task attribution comes from the cycle that ran the effect. Under
`supervised` an irreversible effect never executes in the cycle that emitted it
— it parks, and the operator's approval opens a fresh cycle carrying only
`ApprovalResolved` — so `ApprovalParked` also gains an optional `task_id`, and
the approved execution reads the card back off it. Without that, every effect
that went through the approval gate the way the policy intends would be
attributed to nothing.

**Committed, not completed.** The record is written *before* the effect is
performed — that ordering is the at-most-once guarantee — and a failed perform
leaves it standing. So an entry means "this was committed, and the runtime will
never re-attempt it", which is what the warning needs: the operator has to
assume it happened, because nothing will finish it and nothing will retry it. It
does not mean the effect is known to have completed, and the dialog's wording
says so rather than rendering the list as flat fact.

**Approved tool calls are described at redemption.** An approved effect carrying
an `agent` is settled by minting a single-use grant, not by
`execute_effect_once` — the tool then runs inside the agent's next turn — so it
writes no `EffectExecuted` line at all. `GrantConsumed` therefore carries the
same optional `ExecutedEffect`, built from the park record (retained past
resolution, payload scrubbed, superseded by an approve-with-edit) joined to the
gate's classification. It is attached at **redemption** rather than at minting,
because a grant that expires unredeemed is a call that never ran and must not
appear on a warning.

**A journal that cannot describe itself says so.** Both fields are additive: a
line written before #351 replays as a committed key with no description, which
keeps the at-most-once guarantee exact and simply contributes no warning. That
makes an empty list ambiguous on an upgraded company, so replay raises a
company-wide flag when it reads an undescribed executed key, surfaced as
`historyIncomplete` on `GET …/tasks/{task_id}`. With it set the console confirms
a retry regardless and says earlier activity cannot be described, instead of
presenting the gap as an all-clear. The flag is company-wide rather than
per-task by necessity — an undescribed record carries no card either. The
related pre-#351 case it cannot detect directly: an approval parked before the
upgrade has no `task_id`, and that record is byte-identical to a legitimately
card-less park written today, so flagging it would misreport every company that
has ever parked an approval from operator chat.

**Scope.** Task Detail only. The board's own re-dispatch — dragging a card back
into `in_progress` (`company/runtime.rs`, `upsert_task` → `dispatch_task`) — has
the same shape and now has this read available to it, and is deliberately left
for a follow-up rather than half-gated here.

## Workflow run progress (issue #371)

A workflow run used to journal exactly one line, `WorkflowRunFinished`, written
after the run returned. Between pressing Run and that line there was no record
at all: a long run was indistinguishable from a wedged one, and a run that died
at the fourth of six nodes recorded only that it died.

Three variants now bracket a run:

| Variant | Written by | When |
| --- | --- | --- |
| `WorkflowRunStarted` | the workflow runner | before the engine call |
| `WorkflowNodeFinished` | the runner's `RunObserver` | as each non-trigger node finishes |
| `WorkflowRunFinished` | the **caller**, via `record_run_finished` | after the run returns, on both arms |

### Why the journal rather than a dedicated store

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
node's output items; `WorkflowNodeFinished` carries a node id, a two-valued
status and a duration, and `WorkflowNodeStatus` has no `String` arm — so there
is no `format!` that could put a node's own words on the journal. This is the
same stance the live turn-progress frames take on tool args, and it matters
because the journal is read by the operator SSE projection *and* wired out to
the inference sidecar.

Nothing is lost: the run-level failure reason already lands on
`WorkflowRunFinished.error`, which is a tenant-scoped surface.

### Run-id correlation

`WorkflowRunStarted` and `WorkflowNodeFinished` **require** a `run_id`;
`WorkflowRunFinished` has always carried an optional one and now populates it.
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

### An engine constraint worth knowing

tinyflows' `RunObserver` has `on_step_finish` and **no `on_step_start`**, so
there is no "node started" event to journal. A console showing which node is
*currently* executing derives it from the graph topology it already holds
(mark the successors of a finished node), which is a good-faith frontier rather
than ground truth — after a branch point it briefly marks both arms. A true
start hook needs a vendored tinyflows change and is tracked separately.

A failing node, by contrast, **is** reported exactly: a node that dies under the
default `stop` policy still emits a step with `Error` status before the run
ends, so failure attribution is exact rather than inferred.
