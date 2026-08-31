# Observatory

What a company's agents actually did, run by run.

## Why it is its own view

`WorkflowsView` is an *authoring and operating* surface — create, edit, arm, run,
cancel, decide approvals — and it is already 3000+ lines. This one is read-only,
cross-run and **agent-centric**: its subject is who did what, and the graph is
one lens on that rather than the thing being edited. A fourth lens inside that
view would have been a merge conflict, not a design.

## The address

```
#/observatory                        every attempt, newest first
#/observatory?tab=analytics          cross-run analytics
#/observatory/<workflowRunId>        one run's attempts
#/observatory/<workflowRunId>?agent=theorist&turn=<attemptId>&step=7
```

`useHashView` carries only head/sub and drops every query key except `host`, so
`hash.ts` reads and writes the rest — the same thing `readWorkflowHash` does for
`?run=`. Every write is a `replaceState`: opening an agent's thread is a
*selection*, not a navigation, and pushing each one would make Back walk through
every row an operator clicked while reading one run.

## Live and replayable

The fetched snapshot is authority. It is re-read on a visible poll (4 s while
anything is running, 30 s when nothing is) and on an event tick the shell
derives from SSE.

A frame is **never merged into the snapshot** — it only triggers a re-read. That
is the discipline `views/workflows/graph.ts` documents: two frames collapsing
inside one React batch still mean "re-read" exactly once, whereas two *payloads*
collapsing loses one.

This is also why `app-shell.tsx`'s `onTurnEvent` gained a counter. A workflow
`agent` node's turn belongs to no chat, so every frame it emitted used to hit
`if (!threadId) return` and vanish.

## Files

| File | Role |
| --- | --- |
| `hash.ts` | the address grammar — **pure** |
| `waterfall.ts` | lane packing, placement, concurrency — **pure** |
| `model.ts` | attempts → spans, totals, per-agent and per-node rollups — **pure** |
| `clamp.ts` | bounding what a step body renders — **pure** |
| `ObservatoryView.tsx` | fetch, poll, tab and agent selection |
| `WaterfallLens.tsx` | the timeline and the concurrency strip |
| `AttemptCard.tsx` | one attempt, collapsed until opened |
| `StepRow.tsx` | one step, with its unredacted half behind a fold |
| `AnalyticsLens.tsx` | the four cross-run charts |

Everything worth arguing about is in the pure half, which is why the unit lane
covers it without a browser.

## Two things the UI must keep saying honestly

**`stepCount` is null while an attempt is live.** The host writes it on the
settle and refuses to invent one before, so `stepTotal` counts `steps` instead.
Rendering the stored `0` would under-report exactly the run somebody is watching.

**Blocked is not failed.** An attempt waiting on a person has not gone wrong.
The two get different tones and are counted separately in `byNode`, because
folding them sends an operator hunting a bug in the node that most often just
needs a click.

**Declined is not succeeded.** A by-design compiler refusal (issue #1809) is a
clean terminal outcome, not a failure — but it did not succeed either. It gets
the `idle` tone (the closed vocabulary's neutral word — never `done`'s green,
never `failed`'s red) in `AttemptCard`, the waterfall span and a workflow run's
summary dot, and its own `declined` column in `byNode`, apart from `succeeded`.

## Redacted vs raw

A step carries both halves and the panes label which is which. `detail` and
`result` are the redacted projection — the same one an approval card renders,
always safe. `deep` is raw arguments and raw output. Note that redaction is by
**key name**: `runtime::approval_display`'s own docs say an unlisted key holding
a secret is not masked. See `docs/spec/runtime/deep-trace.md`.
