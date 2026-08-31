# A run's verdict — `WorkflowRunVerdict` (issue #981)

The rows on `WorkflowRun.deliveries` say *why* a report did not go out.
`verdict` says what the whole run adds up to, in one word, on both run DTOs —
the synchronous `POST …/workflows/{wid}/run` body and every row of
`GET …/workflows/runs`:

```text
running | failed | stopped | stranded | blocked | undelivered | awaiting-approval | degraded | ok
```

**Always serialized**, unlike the optional fields around it. Its whole purpose
is to be the field a client reads *instead of* re-deriving the reading, and an
omitted verdict pushes every reader straight back into the six-field ladder it
replaces.

## Why the host owns it

A run's outcome was spread across `running`, `error`, `cancelled`,
`blockedNodes`, `deliveries` and `pendingApprovals`, and nothing said what they
added up to. The only place that answered "did this run succeed?" was the
console's TypeScript, so every other reader re-derived it — and the obvious
derivation is wrong in exactly the case that matters. Delivery is **host-side
and post-engine** (`src/workflows/delivery.rs`): by the time a destination is
refused the engine has already returned, so the graph's nodes all report `ok`
and nothing about a node's status moves.

The 2026-08-18 QA pass watched one run paint its `output` node `DONE`, green,
list it as `ok` in the Steps panel, and score PASS in a harness folding
`nodes[].status` — while the run's own delivery row read `channel-not-wired`
and the report was gone. Three readers, three transcriptions of the same
ladder, and the one fact that mattered in none of them.

## The order is the check

Each arm below the first exists because the state it names had been scoring
green on some surface:

| verdict | read from | why it sits here |
| --- | --- | --- |
| `running` | `running` | an unsettled run has no error, no cancel and no deliveries yet, so without this it falls to `ok` |
| `failed` | `error` | **the more serious fact first** — a run that broke mid-graph *and* dropped a report reads `failed`, with its delivery rows still on the body |
| `stopped` | `cancelled` | issue #383: a stop somebody asked for is not a fault, and a cancelled run has no deliveries to weigh |
| `stranded` | `pendingApprovals` **and** `strandedApprovals` | issue #1189: every gate has lost its card, so the two readings below — both of which say "go and decide it" — are the one thing that is no longer true |
| `blocked` | `blockedNodes` | issue #881: carries no error, is not cancelled, is not running and routed no report — the shape that fell through every check |
| `undelivered` | `deliveries` | issue #981: a report that will not go out without a change outranks one waiting on a human |
| `awaiting-approval` | `pendingApprovals` **and** `pending` delivery rows | issue #846: a run that paused at a gate reached no `output` node, so a delivery-only read scored the gated case clean |
| `degraded` | `nodes[].status` | issue #1865: no failure, stop, stranding, block or dropped report, but at least one `on_error: continue\|route` node errored and the graph kept going past it — ranked last, immediately above `ok`, since every other non-`ok` verdict is more actionable and none may be hidden behind this one |
| `ok` | — | finished, delivered what it routed, waiting on nobody |

## A run nobody can act on any more (issue #1189)

`stranded` fires when the run stopped for somebody, **every** one of those gates
has no live card left in the queue, and no report is parked either. A run only
*partly* stranded keeps its old verdict — something there really is still
decidable, and the per-node `blockedNodes[].stranded` count carries the loss.

### Why it needed a word of its own

A run has two ways to stop for a person and only one of them was ever
reconciled against the live queue:

| shape | what it records | join key |
| --- | --- | --- |
| **blocked node** | `blockedNodes[].approvalIds` — the ids its gated calls parked. The cards are ordinary tool-call effects and carry no node id. | approval id (issue #1143) |
| **gate** | nothing but a node id on `pendingApprovals`. `park_pending_gates` writes no approval-row receipt and no blocked-node row; the parked `workflow.approve` card is the only thing that knows the pair. | `(run_id, node_id)` (issue #1189) |

#1143's join is keyed on ids the gate shape does not have, so it structurally
could not reach it — and the gate shape is the larger half. On the marketing
tenant, 34 of 60 runs are `{"verdict":"awaiting-approval","pendingApprovals":
["fetch_bbc","fetch_espn","fetch_guardian"],"blockedNodes":[],"approvals":[]}`
against an **empty** queue: a third of the tenant's history claiming to wait on
a person who had nothing to answer.

Without this arm there was no honest verdict for that state, so it kept the
dishonest one. `blocked` and `awaiting-approval` both tell an operator to go and
decide something; `stranded` is precisely the state in which there is nothing to
decide, which is why it outranks them rather than sitting below.

### It claims nothing about why

Approving a gate does not continue the parent run: `resume_run` →
`spawn_continuation` starts a **new** run with a new id and records no link back
to the one that paused. So a run whose gates were all *approved* is
indistinguishable, from this end, from one whose cards were *lost*. The join is
still right — no decision left can move either run — but the copy must not claim
data loss. Every surface says only what is observable ("nothing here is waiting
on you any more; this run cannot be continued") and offers a re-run as an
option, never as a remedy for a stated cause.

The console paints it `idle`, not the amber `blocked`/`awaiting-approval` share:
amber is the "needs your attention, go and decide this" state, and reprinting
that in colour is the claim this reading exists to remove. Not red either — the
graph did not break. See `docs/design-system/color.md`.

### Derived after the reconciliation, not before it

`strandedApprovals` is computed on each read of `GET …/workflows/runs`, and the
verdict pass now runs **after** that join. Until #1189 the pass sat above
`reverse()`/`truncate()` and above the join, so even the shape #1143 *did*
reconcile got a verdict read from pre-join data — a blocked-node list saying
"cannot be continued" beside a run verdict saying `blocked`.

`reverse` and `truncate` moved above the pass to make room. Neither touches a
single field of a row — they reorder and drop whole rows — so the invariant the
pass is placed on ("derive after everything that can still change its inputs")
is not weakened. It is extended: the reconciliation genuinely does change an
input.

The synchronous `POST …/workflows/{wid}/run` path deliberately passes a literal
`0`. Its body is written microseconds after `park_pending_gates` minted the
cards, so joining against the queue there would be a guaranteed-zero query on
the hot path; the reconciliation is a fact about a run somebody comes back to.

An empty `error` string is not a failure. No producer writes one, and the
console's `if (run.error)` has always read it as falsy — the host agreeing costs
nothing and removes a way for the two to disagree.

## Undelivered is its own reading, not a failure

A delivery failure does **not** populate the run's `error` and does **not** flip
any `nodes[].status`. The nodes really did run and their work is valid; the fix
is a destination or a runtime wiring, not a node. Marking the run failed would

- point the copilot's fix-from-run at a graph that was fine,
- inflate the failure count and hide real breaks among them, and
- collapse the three terminal readings issue #383 keeps apart.

So `undelivered` sits between them: not `ok`, not `failed`, and named for what
happened. Every existing consumer of `error`, `cancelled`, `running` and
`nodes[].status` sees exactly what it saw before.

## Derived, never stored

`CompanyEvent::WorkflowRunFinished` gains no field. `GET …/workflows/runs`
computes each row's verdict in a single pass **after** the fold has settled every
open row and after the issue-#1009 cross-check has flipped the dead ones — the
position is the correctness argument, since every input the verdict reads is
written by the settle arm *after* its row was pushed.

Three things follow:

- **No migration.** Every run already in a company's journal re-scores on
  deploy, including rows written before this existed.
- **No third state to keep in sync.** The read-side settle (issue #1081)
  rewrites `running` and `error`; a stored verdict would have to be rewritten
  alongside them, and the one that was forgotten would be the bug.
- **No new failure mode.** A verdict cannot disagree with the rows it was read
  from, because there is only ever one reading.

One consequence worth stating out loud: anyone counting successful runs off this
endpoint sees their rate drop, with no change in behaviour. The dropped reports
were always there.

## Consumers

`WorkflowRunVerdict` lives in `src/ports/workflow_verdict.rs`, beside the
`DeliveryReport` rows it reads. The console's `runTone`
(`frontend/src/views/workflows/run-health.ts`) is a lookup on `verdictOf`, which
takes the host's word when there is one and falls back to the same ladder for a
host predating this — the fallback is what keeps a run's meaning stable across
hosts, not legacy tolerance for its own sake. `qa/oc-qa.js` reads it the same
way, and `frontend/test/unit/qa-harness.test.ts` pins the two together.

The orchestrator's `run_workflow` tool summary
(`harness::orchestrator::summarize_run`) reports the dropped reports too, using
`DeliveryReason` and never `detail` — the closed set is the log-safe half of the
pair (issue #248), and a summary rides wherever the model's turn rides.

## What counts as undelivered

A report is undelivered when it **did not reach a destination and will not
without a change**. `crate::ports::is_undelivered` is the one rung, and every
surface stands on it: the verdict above, the scheduler's alert number, the
sidecar's and the orchestrator's run summaries, the console's "N not delivered"
badge, the SSE toast, and the per-node marker the console paints on the `output`
node itself.

`sent` landed. `pending` is a report parked for an operator's approval, counted
by `awaiting_count` instead — counting it here would score a working approvals
queue as a failure. `denied` and `failed` always count.

`skipped` is the interesting one, because the delivery path writes it for three
genuinely different situations. The axis that separates them is **whether the
report's fate is accounted for** (issue #981):

| reason | counts? | why |
| --- | --- | --- |
| `already-delivered` | no | an earlier run in this approval lineage **sent it** (issue #438). Approving a gate re-runs the graph from the trigger, so every upstream `output` node is reached again; the report is at its destination. |
| `dry-run` | no | a test run (issue #542). Nothing was attempted, on purpose, in a mode the operator chose. |
| `no-destination-configured` | **yes** | the report was produced and then **lost**, with nothing accounting for it (issue #925). |

The third is the deliberate non-move, and it is the reason the other two could
move at all. That row exists precisely so "the author routed nothing on purpose"
and "the author never configured a destination" stop being the same observation;
excusing it here would restore the silence issues #947 and #963 were filed
about. The earlier framing — that all three "describe a report that was never
owed to an address" — reads the wrong axis: a report with nowhere to go was owed
to nobody *because the graph is wrong*, which is the finding, not the excuse.

The match on `DeliveryStatus` is exhaustive and only the `skipped` arm reads a
reason, so a new status cannot be added without classifying it. A row carrying
`unspecified` — the only reachable value on a `WorkflowRunFinished` journaled
before issue #248 added the field — counts, which is the safe direction.

## The node cell answers a different question

`nodes[].status` stays `ok | error | blocked` and a delivery failure still does
not move it. The node cell answers **"did the engine run this step?"**, and the
honest answer for a dropped report is yes: delivery is post-engine, the node ran,
its work is valid, and the fix is a destination or a runtime wiring.

Issue #981's second report was that the console then showed `ok` on the output
node of a run reading `undelivered` — two readings contradicting each other on
one screen. The resolution is **not** a fourth status word:

* `frontend/src/views/workflows/graph.ts`'s `nodeStateFrom` fail-safes any status
  it does not recognise to `error`, so a new word would make every console
  predating it paint a node that ran perfectly as failed;
* one field would then answer two questions decided by two subsystems at two
  different times — the collapse issues #383 and #881 each had to undo once.

Instead the node cell stops being alone. `DeliveryReport.node` carries the
`output` node's id, so the console joins the delivery rows to the nodes locally —
**no new wire field, and no third notion of "this run did not fully work"**. Three
surfaces render the join beside the node's own state, each explicitly labelled so
the two facts read as different questions rather than as a contradiction:

* the canvas node keeps its green ring and `DONE` badge and gains a
  `report not delivered` strip (and a tooltip that says "This step ran. Its
  report did not go out.");
* the run drawer's **Steps** row keeps its `ok` badge and gains a
  `not delivered` badge beside it;
* the history panel's node chip keeps its green dot and gains a red
  `not delivered` segment.

`undeliveredNodes` in `frontend/src/views/workflows/run-health.ts` is that join,
folding the same `isUndelivered` the counts use.

## Not in scope

The SSE `workflow_run_finished` frame carries no verdict. It already reads the
delivery rows correctly and toasts an undelivered report, so nothing there
scores a dropped report green — it now toasts through the shared predicate, so a
test run no longer warns that a report it never attempted "didn't go out".
