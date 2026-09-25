# Approval correlation in the journal

How a reader folds the company journal back into per-approval views: which card
an approval belongs to, which of the two correlation keys wins, and the event
that lets a conversation raise the request while it is still waiting.

Split out of [`events.md`](events.md) on this repo's 500-line cap for a Markdown
file. That page owns the `CompanyEvent` vocabulary and the journal's shape; this
one owns the approval half of the correlation rules.

## Per-task approval correlation (issue #333)

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

### Which key is authoritative

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

## In-conversation approvals (issue #379)

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
