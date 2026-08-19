# Master QA — the release parity pass

What a release check consists of, in one pass. Run it against a tenant **rolled
to the commit under test** (see [`README.md`](README.md) — this is not optional;
a stale tenant reports bugs `main` has already fixed).

Three parts, in this order:

1. [`oc-qa.js`](oc-qa.js) — 22 read-only checks (~10s) and 5 probes, 4 of them live.
2. [The seven UI checks](#the-seven-ui-checks) a script cannot judge.
3. [The five persona tasks](#the-five-persona-tasks) and their rubric.

A pass is written up as three tallies, and **`SKIP` is reported separately from
`PASS`**. Untested is not passed — three checks in the 2026-08-18 pass were
written up as green having never run, which is the single reason this document
exists in the repo rather than in someone's notes.

## Part 1 — the script

Open the console on a signed-in tenant, paste [`oc-qa.js`](oc-qa.js), then:

```js
OCQA.read()      // 22 read-only checks, ~10s, spends nothing
OCQA.probe()     // 5 checks, 4 of them live — spends tokens, may park approvals
OCQA.report()    // the last run as Markdown, for the issue
```

`probe()` acts on the tenant the tab is signed in to: real chat turns to the
orchestrator and to up to five desks, and a real board card created and deleted.
It prints the host and company it is about to act on before it starts.

**It will not choose a workflow for you.** A real run fires real deliveries, so
bare `OCQA.probe()` reports `probe-workflow-run` as `SKIP`. Name the workflow to
run one:

```js
OCQA.probe({ workflow: "<id>" })              // runs it FOR REAL
OCQA.probe({ workflow: "<id>", dryRun: true })// rehearses it over stubbed effects
```

Read the verdict back off the response rather than trusting the flag: a host
that predates test mode ignores it and runs for real, and the script warns when
that happens.

After `probe()`, check `probe-approval-delta`. The probes can park effects at the
approvals gate; leaving them there freezes whatever sits behind them.

`OCQA.report()` withholds the tenant message text the probes collected, because
its output is written to be pasted into an issue. `OCQA.report({ raw: true })`
includes it — keep that out of anything public.

## The seven UI checks

None of these are visible to an API client. Each is written as a claim to
falsify, not a thing to look at.

| # | Check | Passes when | Fails as |
| --- | --- | --- | --- |
| U1 | **Cold load in a browser that has been here before** | A hard reload and then a *normal* reload both render. Do not clear the cache first — the returning visitor is the case that breaks. | A blank `#root` with `Failed to fetch dynamically imported module` in the console (#979). |
| U2 | **Board drag** | A card dragged between columns stays in the new column after a full page reload. | It springs back, or the move is local-only and the server never heard it. |
| U3 | **Approvals resolve** | Approve and deny each remove their card and the effect visibly happens (or visibly does not). | The card stays, or vanishes with nothing behind it. Needs a queued approval — see [Making U3 testable](#making-u3-testable). |
| U4 | **Run drawer delivery rows** | Opening a run whose report was dropped shows the delivery row and its reason, not a clean green run. | The drawer reads as a success (#981). |
| U5 | **Live timeline** | A message in flight shows steps arriving, and a reload mid-turn still shows the question. | The reload shows an empty transcript — the turn is invisible (#983). |
| U6 | **Theme and layout** | Both themes render every view with no clipped text, no invisible-on-invisible, no horizontal body scroll. | Anything unreadable in one theme only. |
| U7 | **Error honesty** | Disconnect the network and use the app: every surface says what failed. | A spinner forever, or a surface that renders empty as though the answer were "none". |

### Making U3 testable

The 2026-08-18 pass could not run U3: the queue was empty, and the only way to
fill it was to change the tenant's approval tier — a change to the thing under
test. Two ways to get a row without that:

- Run a workflow with an `output` node whose destination needs approval; the
  delivery parks as `pending`.
- Ask a desk for something with an outward effect (send mail, post to a channel).
  Under `supervised` and `auto` this parks.

If neither is available, U3 is `SKIP`. Write it up as untested.

## The five persona tasks

Five real requests, one per persona, sent as ordinary chat messages. **Send them
one at a time.** Five concurrent messages become a serial train behind the
per-company cycle lock and every one of them times out at the edge (#983) — the
work still happens, but you learn nothing about quality from a 504.

| # | Persona | Task |
| --- | --- | --- |
| P1 | Operator, first day | "What does this company do, and what is it working on right now?" |
| P2 | Marketer | "Draft a short launch announcement for our newest deliverable and save it to the workspace." |
| P3 | Analyst | "What have we spent this month, and on what?" |
| P4 | Ops | "Something in our last workflow run didn't go out. What was it and why?" |
| P5 | Founder | "Propose one workflow that would save us the most time, and build it." |

### Rubric

Score each turn on four axes, one point each. A turn scores 4 or it is not done.

- **Answered** — it responded to the question asked, not an adjacent one.
- **Grounded** — every specific claim (a number, a name, a date, a file) traces
  to something in the tenant. An invented figure fails this axis outright.
- **Honest** — where it could not know, it said so. A confident answer to an
  unanswerable question fails even when the answer happens to be right.
- **Delivered** — the artefact it says it produced exists and can be opened. Check
  the workspace, the board card, the workflow — do not take the sentence for it.

Score the transcript **after every turn has settled**, not while they run. Four of
five turns in the 2026-08-18 pass were still running when the pass ended, and the
partial scores were worth nothing.

## Writing the pass up

Three tallies, kept separate:

```
read:   N pass / N warn / N fail / N untested
probe:  N pass / N warn / N fail / N untested
UI:     U1..U7 each pass / fail / skip
persona: P1..P5 each scored /4
```

Then, for every `FAIL`: the check name, the value it judged, and either an issue
number or a new issue. `OCQA.report()` emits the first two as a Markdown table.

**And one more step, which is the one that gets skipped.** For every bug found by
something *other* than this harness, ask why the harness missed it and change the
check that should have caught it. The workflow probe scored a delivery-failure run
as `PASS` because it judged a run by node status alone — the same mistake the
product made in #981. Adding a delivery check afterwards was not the fix; changing
the run check to read `deliveries` was.

## Handing the pass to an agent

An agent with browser access can run parts 1–3. Paste this:

> You are running the OpenCompany release parity pass against `<TENANT URL>`,
> which is rolled to commit `<SHA>`. Follow `qa/MASTER-QA.md` in the repo.
>
> 1. Confirm the tenant is on `<SHA>` before anything else. If you cannot
>    confirm it, stop and say so — a pass against a stale tenant is worse than
>    no pass, because it reports fixed bugs as live.
> 2. Sign in, open the console, paste `qa/oc-qa.js`, run `OCQA.read()`. Report
>    every row, verdict beside the value it judged. Do not summarise away the
>    values.
> 3. Run `OCQA.probe()`. This spends real tokens and writes to the tenant, so
>    confirm the host it names in the console is `<TENANT URL>` before going on.
>    It reports `probe-workflow-run` as SKIP by design — a real run fires real
>    deliveries, so re-run it as `OCQA.probe({ workflow: "<id>" })` only with a
>    workflow you were told to run, and prefer `dryRun: true` unless a real
>    delivery is what is being checked. Then check `probe-approval-delta` and
>    resolve anything the probes parked.
> 4. Walk U1–U7 in the browser. For each, state the claim you falsified and what
>    you actually saw. If you could not create the conditions for a check, mark
>    it SKIP — never PASS.
> 5. Send P1–P5 **one at a time**, waiting for each to settle. Score each on
>    answered / grounded / honest / delivered, and verify every deliverable by
>    opening it.
> 6. Report three separate tallies, keeping SKIP out of PASS. For every FAIL,
>    name the check, the value judged, and the issue — existing or new.
> 7. Finally: for any bug you found outside the script, say which check should
>    have caught it and what that check must become.
>
> Do not fix anything. This pass reports.
