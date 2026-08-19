# `qa/` — the release parity harness

What a release is checked with, in one pass, against a real deployed tenant.

| File | What it is |
| --- | --- |
| [`oc-qa.js`](oc-qa.js) | A zero-dependency browser-console script: 22 read-only checks and 5 probes, 4 of them live. |
| [`MASTER-QA.md`](MASTER-QA.md) | The seven UI checks a script cannot judge, five persona tasks with a rubric, and the prompt that hands the pass to an agent. |

A release checklist points here. Nothing else has to be found first.

## Before anything else: roll the tenant

**The tenant must be on the commit under test.** A tenant running an older image
reports bugs `main` has already fixed, and the report is worse than no report —
it costs a re-investigation of closed issues. The first attempt at the
2026-08-18 pass was misleading for exactly this reason.

`oc-qa.js` prints the build identity in its `repo-binding` row (`name@version`
plus the company's source-template provenance). No endpoint reports a git SHA,
so that row stays `SKIP` until a human compares it against the commit under
test. Confirm it, then start.

## Running it

Open the operator console on a **signed-in** tenant, paste the whole of
[`oc-qa.js`](oc-qa.js) into the browser console, then:

```js
OCQA.read()      // 22 read-only checks, ~10s. Spends nothing.
OCQA.probe()     // 5 checks, 4 of them live. Spends tokens; may park approvals.
OCQA.report()    // the last run as a Markdown table for an issue
```

Options: `OCQA.read({ company: "acme" })` pins a company on a multi-company host
(the scope is auto-detected otherwise).

### What `probe()` actually does to the tenant

`probe()` is not a read. It sends real chat turns to the orchestrator and to up
to five desks, and creates and deletes a real board card — on whatever tenant the
tab is signed in to. It prints that host and company before it starts anything,
so a probe fired at the wrong tab is visible in the transcript rather than only
in the consequences.

**It will not choose a workflow for you.** A real run fires real deliveries — a
report into a channel, mail to a real address — so `probe()` on its own reports
`probe-workflow-run` as `SKIP` and says so. Name the one you meant:

```js
OCQA.probe({ workflow: "daily-release-readiness" })              // runs it FOR REAL
OCQA.probe({ workflow: "daily-release-readiness", dryRun: true })// rehearses it
```

Read `dryRun` back off the response rather than trusting the flag: a host that
predates test mode ignores it and runs for real, and the script raises a `WARN`
when the response comes back without it.

After any `probe()`, check `probe-approval-delta` and resolve whatever the probes
parked — leaving effects at the gate freezes whatever sits behind them.

### What `report()` leaves out

`OCQA.report()` is written to be pasted into an issue, so it withholds the tenant
message text a probe collected — `probe-chat` judges the reply's shape in the
table and keeps the reply itself out of it. The table says how many rows were
withheld. `OCQA.report({ raw: true })` includes them, for a write-up that is not
going anywhere public.

Ids are not redacted — company, desk, workflow and delivery targets are what make
a `FAIL` actionable, and a report without them names no defect. Read the table
before pasting it somewhere public.

It rides the session cookie of the tab it is pasted into, so there is no token to
mint or distribute — which is also why it is a console script rather than a
CI job.

## What the verdicts mean

Each row reports `PASS` / `WARN` / `FAIL` / `SKIP` **next to the value it judged**,
so a verdict can be checked rather than trusted.

The `approval-tier` row reads `{scope}/policy` (`src/server/ops/policy.rs`, #562) —
`read_policy` is a GET that needs no admin token — and reports the tier actually
in force next to the manifest's tier and any override producing it. On a build
before that read surface, the row reports what the gate is holding and `SKIP`s
with the tier unverified. With the tier in hand an empty approvals queue stops
reading the same for a `supervised` tenant (nothing pending) as for a `full`
tenant (nothing will ever park).

`SKIP` means the surface could not be read — a 404, an error, a feature absent
from this build, or a precondition that did not exist. **`SKIP` is untested, not
passed**, and the summary counts it separately. Writing a skipped check up as
green is the failure mode this harness is shaped against.

A build that answers `{"code": "not_wired"}` — a feature-gated surface saying
"this deployment has no such machinery" — is `SKIP`, not `FAIL`. A host with no
workflow runner cannot run a workflow, and scoring that red sends somebody
chasing a graph that is fine. The typed `code` is what is matched, never the
prose (issue #248).

`read()` reports **22 checks**. On a host that serves a built console the cache
check expands into three rows — the shell, the SPA fallback and a hashed asset
are three separate responses from three code paths — so a full run prints 24.

## The two checks worth keeping even if the rest is deleted

- **`console-cache-headers`** — caught [#979](https://github.com/tinyhumansai/opencompany/issues/979).
  A cacheable `index.html` white-screens every returning user after a deploy: the
  stale shell names an entry bundle whose lazy chunks are gone, the SPA fallback
  answers them with HTML, the dynamic import throws, and the page is blank. There
  is nothing in the UI to see, because the UI is the thing that failed to load.

  The fix has landed (`src/server/routes.rs` sets `no-cache` on the shell and
  every SPA fallback, `immutable` on `/assets/*`), so this check is now a
  **regression guard**: it should read PASS/PASS/PASS on a current build, and a
  FAIL means a deploy has reintroduced the highest-impact defect the
  2026-08-18 pass found.

- **`workflow-deliveries`** — caught [#981](https://github.com/tinyhumansai/opencompany/issues/981).
  A run whose nodes are all `ok` can still have delivered nothing. A delivery
  failure deliberately does not populate `error` or flip `nodes[].status`, so
  folding node status scores a dropped report as green.

## Reviewing the harness against what it missed

Extending the harness after a miss is not enough — **the check that missed has to
change.** The workflow probe originally scored a delivery-failure run as `PASS`
because it judged a run by node status alone, which is the same mistake #981
filed against the product. The fix was not "add a delivery check"; it was to make
the run check read `deliveries`.

`runVerdict` in `oc-qa.js` is a transcription of the console's own
`frontend/src/views/workflows/run-health.ts`, and `frontend/test/unit/qa-harness.test.ts`
pins the two together — so a change to how the console reads a run breaks this
file loudly rather than silently re-greening a bad run.

## Known gaps

Closed when they become testable, and reported as `SKIP` until then:

- **Board drag** (`U2`) and **approvals resolve** (`U3`) are browser-only; U3 also
  needs a queued approval, which the 2026-08-18 pass could not create without
  changing the tenant's approval tier. See MASTER-QA for two ways to get one.
- **Persona quality** was only partly scored in that pass, because four of five
  turns were still running when it ended.
  [#983](https://github.com/tinyhumansai/opencompany/issues/983) makes this
  properly measurable for the first time.
