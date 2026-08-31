// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { WorkflowRunOutcome } from "@/api/workflows";
import { RUN_STATUS_LEGEND, RunHistoryPanel } from "@/views/workflows/RunHistoryPanel";

/**
 * Issue #1798: a run row's at-a-glance signals — the coloured status dot, the
 * "not delivered" badge — carried no definition, so an operator could not act on
 * a status they could not read. Each now explains itself on hover, and the panel
 * header carries a standing legend for the operator who does not know a badge is
 * hoverable.
 *
 * A jsdom render because the claim is about what the panel PAINTS: a `title`
 * attribute on the right element and a legend affordance in the header. The
 * definitions themselves are a plain map and could be unit-tested directly, but
 * the wiring — which element carries which title — is the part that regresses.
 */

function baseRun(over: Partial<WorkflowRunOutcome> = {}): WorkflowRunOutcome {
  return {
    seq: 1,
    atMillis: 1_700_000_000_000,
    workflowId: "daily_digest",
    scheduled: false,
    deliveries: [],
    pendingApprovals: [],
    ...over,
  };
}

/** A run an operator stopped: `runTone` reads this as `stopped`. */
function stoppedRun(): WorkflowRunOutcome {
  return baseRun({ seq: 2, cancelled: true });
}

/** A run that failed before any node ran at all — a graph that would not
 * compile, or a capability that could not be built (`graph.ts`'s
 * `failureLocation`/`failedNodeOf` name this case explicitly: `nodes` is
 * empty and no node is at fault). `verdictOf` reads a run with `error` set as
 * `failed` regardless of whether any node ran. */
function failedRun(): WorkflowRunOutcome {
  return baseRun({ seq: 4, error: "graph failed to compile" });
}

/** A run blocked on a gated call that never got a card at all: `unparkable` is
 * set and `approvalIds` is absent, the shape `WorkflowBlockedNode.approvalIds`
 * documents as "Absent when every park failed." `isBlocked` reads `true` off
 * `blockedNodes.length`, and nothing here promotes the run to `stranded`
 * (that reading only folds `pendingApprovals`/`strandedApprovals` — the
 * gate shape, not this one — see `run-verdict.md`). */
function blockedUnparkableRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 5,
    blockedNodes: [{ nodeId: "notify_ops", tools: ["send_slack_message"], unparkable: 1 }],
  });
}

/** A blocked run whose one unparkable call was `discarded` — the per-turn
 * approval cap dropped it before the queue ever saw it (`caps/mod.rs`'s
 * `park_gated_calls`), never refused by the queue. `blockedUnparkableRun()`
 * above carries no `approvals` rows at all, so it cannot exercise this
 * outcome; `run.approvals` is the only place `discarded` and `parkFailed`
 * are told apart (issue #1821, twelfth pass). */
function discardedOnlyBlockedRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 14,
    blockedNodes: [{ nodeId: "notify_ops", tools: ["send_slack_message"], unparkable: 1 }],
    approvals: [{ outcome: "discarded" }],
  });
}

/** A blocked run whose two unparkable calls mix a `discarded` overflow and a
 * genuine `parkFailed` — both causes are real for this run at once. */
function mixedUnparkableBlockedRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 15,
    blockedNodes: [{ nodeId: "notify_ops", tools: ["send_slack_message"], unparkable: 2 }],
    approvals: [
      { outcome: "discarded" },
      { outcome: "parkFailed", nodeId: "notify_ops", tool: "send_slack_message" },
    ],
  });
}

/** A finished run whose one output report was refused — `undeliveredCount` is 1,
 * so the row falls to the delivery block and badges "1 not delivered". */
function undeliveredRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 3,
    deliveries: [
      {
        node: "digest",
        kind: "channel",
        target: "engineering",
        status: "failed",
        detail: "channel not wired",
      },
    ],
  });
}

/** A dry run: its one delivery is `skipped` with reason `dry-run` —
 * `isUndelivered` (`run-health.ts`) deliberately exempts that reason, so
 * `undeliveredCount` is 0 and `verdictOf` reads this run as `ok`, the same as
 * a run that actually sent something. Nothing was attempted, on purpose. */
function dryRunOkRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 6,
    deliveries: [
      {
        node: "digest",
        kind: "channel",
        target: "engineering",
        status: "skipped",
        reason: "dry-run",
        detail: "dry run — nothing sent",
      },
    ],
  });
}

/** A run parked one approval and it is still on the Approvals page:
 * `liveParkedApprovalCount` (`decidableApprovalCount` minus the stranded
 * count, both 0 here) is 1, so the row's "parked" badge renders. */
function parkedRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 7,
    approvals: [
      {
        outcome: "parked",
        approvalId: "a1",
        nodeId: "notify_ops",
        tool: "send_slack_message",
      },
    ],
  });
}

let container: HTMLDivElement;
let root: Root;

async function renderHistory(run: WorkflowRunOutcome) {
  await act(async () => {
    root.render(
      createElement(RunHistoryPanel, {
        runs: [run],
        graph: null,
        workflowName: "Daily digest",
        onClose: () => {},
        selectedRunSeq: null,
        onSelectRun: () => {},
      }),
    );
  });
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the run-status legend affordance", () => {
  it("renders an accessible legend trigger in the panel header", async () => {
    await renderHistory(baseRun());
    const legend = container.querySelector(
      '[data-testid="workflow-run-legend"]',
    );
    expect(legend).not.toBeNull();
    // It must carry a name — the whole point is a discoverable key, and an icon
    // with no accessible name is not discoverable to a screen reader.
    expect(legend?.getAttribute("aria-label")).toBeTruthy();
  });

  // Codex review on #1821 (eleventh pass): the trigger was a bare `<button>`
  // with no padding, height or width, so its hit box was only its `size-3.5`
  // (~14×14px) `Info` child — well below a touch-reliable target, even though
  // switching to `Popover` (fifth pass, above) made a tap capable of opening
  // it at all. `icon-xs` is the smallest sized hit-area the button scale
  // already defines (`size-6`, 24×24px) — the same convention
  // `TaskDetailView`'s redact trigger and the styleguide's own Popover
  // example already use for an icon-only trigger, rather than a one-off pixel
  // value invented here.
  it("gives the legend trigger a touch-sized hit area, not just its icon's box", async () => {
    await renderHistory(baseRun());
    const legend = container.querySelector(
      '[data-testid="workflow-run-legend"]',
    );
    expect(legend?.className ?? "").toMatch(/\bsize-6\b/);
  });
});

describe("the status dot defines the run's verdict on hover", () => {
  it("titles a stopped run's dot with the word and its meaning", async () => {
    await renderHistory(stoppedRun());
    const dot = container.querySelector(
      '[data-testid="workflow-run-status-dot"]',
    );
    const title = dot?.getAttribute("title") ?? "";
    // The word an operator sees the colour for…
    expect(title).toContain("stopped");
    // …and a plain-English definition, not just the word again.
    expect(title).toContain("stopped this run");
  });

  // Codex review on #1821 (fifth pass): `title` is a mouse-hover affordance.
  // The dot was a non-focusable, unlabelled span, so a keyboard, touch or
  // screen-reader user had nothing that named THIS row's verdict — the header
  // legend defines the terms but never says which one applies here. `role="img"`
  // plus `aria-label` puts the same one-liner in the accessibility tree.
  it("gives the dot an accessible name matching its hover title", async () => {
    await renderHistory(stoppedRun());
    const dot = container.querySelector(
      '[data-testid="workflow-run-status-dot"]',
    );
    expect(dot?.getAttribute("role")).toBe("img");
    expect(dot?.getAttribute("aria-label")).toBe(dot?.getAttribute("title"));
  });

  // Codex review on #1821: `RunCancel` (`src/ports/workflow_runner.rs`) stops
  // a run at the next node boundary — the node already executing normally
  // finishes and is journaled. Only a node wedged past the hard-abort grace
  // period is actually dropped mid-flight. The old wording claimed the
  // mid-flight step was *always* dropped, which misleads an operator into
  // thinking its work or side effects never completed.
  it("does not claim the mid-flight step was unconditionally dropped", async () => {
    await renderHistory(stoppedRun());
    const dot = container.querySelector(
      '[data-testid="workflow-run-status-dot"]',
    );
    const title = dot?.getAttribute("title") ?? "";
    expect(title).toContain("normally ran to completion");
    expect(title).not.toContain("was dropped where it was");
  });

  // Codex review on #1821 (eleventh pass): the fix above stopped claiming the
  // mid-flight step was unconditionally dropped, but still spoke of "the step
  // that was mid-flight" as a step every stopped run has. `stoppedRun()` here
  // carries no `nodes` and no `startedNodes` — the exact shape `runner.rs`'s
  // `a_run_cancelled_before_it_starts_does_not_walk_the_graph` proves happens
  // when the cancel signal is already fired before the graph is ever walked —
  // so there was no mid-flight step for this run at all. Unlike the row
  // body's "every step that had started completed", which is vacuously true
  // over an empty set, a definite "the step that was mid-flight" presupposes
  // one exists and is simply false here.
  it("does not presuppose a mid-flight step for a run stopped before any step began", async () => {
    await renderHistory(stoppedRun());
    const dot = container.querySelector(
      '[data-testid="workflow-run-status-dot"]',
    );
    const title = dot?.getAttribute("title") ?? "";
    expect(title).toContain("no such step at all");
  });

  // Codex review on #1821 (thirteenth pass): `WorkflowNodeFinished` is
  // appended best-effort (`runner.rs`'s progress collector logs a failed
  // append and lets the run proceed) — the same fire-and-forget semantics
  // the row body's `midFlightNode` hedge (tenth pass) already accounts for.
  // This definition still told every reader the mid-flight step's own
  // completion "was recorded", unconditionally — true only when that node's
  // finish append happened to succeed.
  it("does not promise the mid-flight step's completion was recorded", async () => {
    await renderHistory(stoppedRun());
    const dot = container.querySelector(
      '[data-testid="workflow-run-status-dot"]',
    );
    const title = dot?.getAttribute("title") ?? "";
    expect(title).not.toContain("normally ran to completion and was recorded");
    expect(title).toContain("completion record can go missing");
  });

  // Codex review on #1821: `failureLocation`/`failedNodeOf` (`graph.ts`)
  // explicitly preserve the case where a run's `error` names no node at all —
  // a graph that would not compile, a capability that could not be built, or
  // an interrupted run the boot sweep recorded. The old wording said
  // unconditionally "a step errored", misdiagnosing those runs.
  it("does not claim a step errored when the run failed before any step ran", async () => {
    await renderHistory(failedRun());
    const dot = container.querySelector(
      '[data-testid="workflow-run-status-dot"]',
    );
    const title = dot?.getAttribute("title") ?? "";
    expect(title).toContain("failed");
    expect(title).not.toContain("A step errored");
  });

  // Codex review on #1821 (second pass): the previous fix above stopped
  // claiming a step errored when the run failed with no node at fault, but
  // left the remedy — "correct the workflow" — unconditional. `workflow_outcome.rs`'s
  // `INTERRUPTED_BY_RESTART` is exactly this case and is deliberately worded
  // as a host fact, not a workflow fault ("nothing about the graph went
  // wrong, the process holding it went away... an operator reading this
  // should go looking at the deployment, not at their nodes"). Telling that
  // operator to correct the workflow sends them to fix something that was
  // never broken.
  it("does not unconditionally tell the operator to correct the workflow", async () => {
    await renderHistory(failedRun());
    const dot = container.querySelector(
      '[data-testid="workflow-run-status-dot"]',
    );
    const title = dot?.getAttribute("title") ?? "";
    expect(title).toContain("failed");
    expect(title).not.toContain("correct the workflow, and run it again");
    // The hedge names a case where the failure isn't the workflow's fault.
    expect(title).toContain("host restart");
  });

  // Codex review on #1821: a blocked node whose gated call could not be
  // queued for approval at all (`unparkable`, not `stranded`) never gets a
  // card in Approvals — `BlockedNodeApprovals`/the row's own body text
  // already say so ("could not be queued for approval at all, so you will
  // not be asked about it"). The old wording unconditionally told the
  // operator to "decide it in Approvals" for every blocked run, which sends
  // this one to a queue with nothing in it.
  it("does not unconditionally promise a card in Approvals for a blocked run", async () => {
    await renderHistory(blockedUnparkableRun());
    const dot = container.querySelector(
      '[data-testid="workflow-run-status-dot"]',
    );
    const title = dot?.getAttribute("title") ?? "";
    expect(title).toContain("blocked");
    expect(title).not.toContain("decide it in Approvals");
    // The hedge names the case that has no card, and what to do about it.
    expect(title).toContain("nothing there to decide");
  });

  // Codex review on #1821 (third pass, same site): `unparkable` is set both
  // when the workflow never wired an approvals queue AND when the store
  // itself refused the write (`docs/modules/server/workflow-routes.md`'s
  // `parkFailed`: "the store refused the write, or no approvals queue is
  // wired"). The frontend has no field naming which one happened, so telling
  // the operator this "needs a workflow or policy change" is only true for
  // one of the two causes and misdirects them for the other.
  it("does not unconditionally prescribe a workflow change for a call that could not be queued", async () => {
    await renderHistory(blockedUnparkableRun());
    const dot = container.querySelector(
      '[data-testid="workflow-run-status-dot"]',
    );
    const title = dot?.getAttribute("title") ?? "";
    expect(title).not.toContain("that case needs a workflow or policy change");
    // The hedge names the infra cause a workflow edit can't fix.
    expect(title).toContain("approvals queue itself can refuse the write");
  });

  // Codex review on #1821: `isUndelivered` (`run-health.ts`) exempts a
  // `skipped` row whose reason is `dry-run` — a test run attempted nothing,
  // on purpose — so `verdictOf` reads it as `ok` the same as a run that
  // actually sent something. The old wording claimed "every report reached
  // its destination", which is false for a report that was never attempted.
  it("does not claim every report was delivered for a dry run read as ok", async () => {
    await renderHistory(dryRunOkRun());
    const dot = container.querySelector(
      '[data-testid="workflow-run-status-dot"]',
    );
    const title = dot?.getAttribute("title") ?? "";
    expect(title).toContain("ok");
    expect(title).not.toContain("every report reached its destination");
    // The hedge covers the report that was never attempted, not just refused.
    expect(title).toContain("didn't need to");
  });
});

describe("the parked badge's definition is reachable without a mouse", () => {
  // Codex review on #1821: the parked badge's definition lives only in its
  // native `title` (a non-focusable span). The panel's ONE keyboard- and
  // touch-reachable affordance is the header `RunStatusLegend` tooltip
  // button, which lists only `RUN_STATUS_LEGEND` — and that array used to
  // omit "parked", so a keyboard or touch user could see the badge but never
  // learn what it means. Asserted against the exported array rather than the
  // rendered tooltip popup, which portals to `document.body` and would not
  // be reachable off `container` anyway.
  it("lists parked in the keyboard-accessible header legend", () => {
    expect(RUN_STATUS_LEGEND).toContain("parked");
  });

  it("still carries the definition on the badge itself, for the mouse case", async () => {
    await renderHistory(parkedRun());
    const badge = Array.from(container.querySelectorAll("[title]")).find(
      (el) => el.textContent?.includes("parked"),
    );
    expect(badge).toBeTruthy();
    expect(badge?.getAttribute("title")).toBe(
      "The run filed this into the Approvals queue for you to decide.",
    );
  });
});

describe("the 'not delivered' delivery badge explains itself", () => {
  it("carries a title defining a report that did not go out", async () => {
    await renderHistory(undeliveredRun());
    const badge = Array.from(container.querySelectorAll("[title]")).find((el) =>
      el.textContent?.includes("not delivered"),
    );
    expect(badge).toBeTruthy();
    expect(badge?.getAttribute("title")).toContain(
      "never reached its destination",
    );
  });
});

describe("the legend trigger opens without hover or focus", () => {
  // Codex review on #1821 (fifth pass): Base UI's `Tooltip.Trigger` only
  // opens on hover or keyboard focus — by design, the same split Base UI
  // documents between Tooltip (glance-only) and Popover (press-activatable).
  // A tap on a touch-only device produces neither: `useHoverReferenceInteraction`
  // is `mouseOnly`, and the trigger's own `onClick` handler *cancels* a
  // pending hover-open rather than starting one. So the legend button — the
  // affordance an earlier fix on this same PR called "the panel's one
  // keyboard- and touch-reachable affordance" — was itself unreachable by
  // touch. This fires a bare `click` with no prior `pointerenter`/`focus`,
  // the same shape a touch tap dispatches, and proves the popup opens off
  // that alone.
  it("opens the popup on a bare click, the shape a touch tap dispatches", async () => {
    await renderHistory(baseRun());
    const trigger = container.querySelector<HTMLElement>(
      '[data-testid="workflow-run-legend"]',
    );
    expect(trigger).not.toBeNull();
    expect(document.body.textContent).not.toContain("What these statuses mean");

    await act(async () => {
      trigger?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(document.body.textContent).toContain("What these statuses mean");
    // Every entry in the source-of-truth array actually rendered, not just
    // the heading.
    for (const term of RUN_STATUS_LEGEND) {
      expect(document.body.textContent).toContain(term);
    }
  });
});

describe("the legend is limited to terms RUN_STATUS_DEFINITIONS actually defines", () => {
  // CodeRabbit review on PR #1821: RUN_STATUS_DEFINITIONS was typed
  // Record<string, string> and RUN_STATUS_LEGEND readonly string[], so
  // TypeScript accepted a legend entry with no matching definition — and the
  // legend's own render site (`RUN_STATUS_DEFINITIONS[term]`, no fallback)
  // has no guard against one landing there silently. This is a type-only
  // regression, proven the same way `tsconfig.e2e.json`'s docblock describes
  // for its suite: `noUnusedLocals` turns an `@ts-expect-error` that suppresses
  // nothing into a compile error, so `npm run typecheck:unit` goes red the
  // moment the legend's element type widens back to bare `string`.
  it("rejects a legend entry that is not a defined term (type-level)", () => {
    type LegendTerm = (typeof RUN_STATUS_LEGEND)[number];
    // @ts-expect-error - "not-a-real-status" has no entry in RUN_STATUS_DEFINITIONS
    const bogus: LegendTerm = "not-a-real-status";
    expect(bogus).toBe("not-a-real-status");
  });
});

describe("the legend popup names itself for assistive technology", () => {
  // Codex review on #1821 (sixth pass): `Popover.Popup` renders `role="dialog"`
  // and only sets its own `aria-labelledby` when a `Popover.Title` supplied the
  // id via the shared store — a heading rendered as a plain element supplies
  // nothing, and React drops an `undefined` attribute rather than emitting an
  // empty one. So a screen-reader user who opened this dialog heard "dialog"
  // with no name at all. Proven against the popup Base UI actually renders
  // (not the heading text alone, which was already present either way) —
  // reverting the `PopoverTitle` swap in `RunHistoryPanel.tsx` fails this on
  // the `aria-labelledby` assertion while leaving the "opens on click" test
  // above green, which is exactly the gap the earlier test didn't catch.
  it("labels the dialog via aria-labelledby pointing at the heading", async () => {
    await renderHistory(baseRun());
    const trigger = container.querySelector<HTMLElement>(
      '[data-testid="workflow-run-legend"]',
    );
    await act(async () => {
      trigger?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    const dialog = document.body.querySelector('[role="dialog"]');
    expect(dialog).not.toBeNull();

    const labelledBy = dialog?.getAttribute("aria-labelledby");
    expect(labelledBy).toBeTruthy();

    const heading = labelledBy
      ? document.getElementById(labelledBy)
      : null;
    expect(heading).not.toBeNull();
    expect(heading?.textContent).toBe("What these statuses mean");
  });
});

/**
 * Codex review on #1821 (eighth pass): three definitions above were reworded,
 * over three earlier rounds, to stop presuming a cause the data doesn't
 * support — but the ROW BODY text a few hundred lines further down
 * `RunHistoryPanel.tsx` (the paragraph under the failed card, the cancelled
 * sentence, the blocked-unparkable remedy) still spoke the old, unconditional
 * story for the very same run, so a single row could show an accurate
 * definition on hover and a contradicting remedy in the body at once. These
 * tests pin the row body, not the `title` attribute the earlier rounds
 * already cover above.
 */

/** A run whose error traces to a specific node — the ordinary failure case,
 * kept as the positive control: the workflow-fault remedy and the copilot fix
 * must both still show here. */
function failedAtNodeRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 8,
    runId: "run-failed-8",
    error: "the writer agent has no model",
    nodes: [{ nodeId: "n_3", status: "error", elapsedMs: 12 }],
  });
}

describe("the failed-run remedy matches whether a node was actually at fault", () => {
  it("still tells the operator to correct the workflow when a node errored", async () => {
    await renderHistory(failedAtNodeRun());
    const row = container.querySelector('[data-testid="workflow-run-row"]');
    expect(row?.textContent).toContain(
      "Review the error details, then correct the workflow and run it again.",
    );
  });

  // Codex review on #1821 (eighth pass): `failedRun()` traces to no node at
  // all — a graph that would not compile, a capability that could not be
  // built, or a host restart — exactly what the legend definition (fixed two
  // rounds ago, tested above) already says is "rather than anything wrong
  // with the workflow". This paragraph still told the operator to correct the
  // workflow unconditionally.
  //
  // Codex review on #1821 (twelfth pass): "nothing in the graph got the
  // chance to run" itself overclaimed — `WorkflowNodeFinished` is appended
  // best-effort, so an empty `nodes` can mean a node's own finish row simply
  // failed to journal, not that execution never began (the same fact the
  // tenth-pass fix already established for the `nodes.length > 0` sibling
  // arm). The assertion below on the removed phrase is the regression proof:
  // it fails against the pre-fix string, which asserted exactly that.
  it("does not tell the operator to correct the workflow, and does not claim nothing ran, when no node was at fault", async () => {
    await renderHistory(failedRun());
    const row = container.querySelector('[data-testid="workflow-run-row"]');
    expect(row?.textContent).not.toContain(
      "correct the workflow and run it again",
    );
    expect(row?.textContent).not.toContain(
      "nothing in the graph got the chance to run",
    );
    expect(row?.textContent).toContain("isn't proof nothing ran");
    expect(row?.textContent).toContain("host/capability problem");
  });

  it("offers Fix with copilot for a node-level failure", async () => {
    await act(async () => {
      root.render(
        createElement(RunHistoryPanel, {
          runs: [failedAtNodeRun()],
          graph: null,
          workflowName: "Daily digest",
          onClose: () => {},
          selectedRunSeq: null,
          onSelectRun: () => {},
          onFixWithCopilot: () => {},
        }),
      );
    });
    expect(
      container.querySelector('[data-testid="workflow-run-fix-with-copilot"]'),
    ).not.toBeNull();
  });

  // Codex review on #1821 (eighth pass): the copilot re-wires the workflow, so
  // offering it here dangles the same wrong remedy as the sentence above for a
  // run with no evidence the workflow was ever at fault. Gated the button on
  // `failedNode`.
  //
  // Codex review on #1821 (twelfth pass): that premise doesn't hold —
  // `failedNode` is null in the SAME "finish row missing" case as the test
  // above, not only in the genuine no-node-ran case, and the backend endpoint
  // this button drives never required a node id (`resolve_fix_error` in
  // `workflows.rs` only needs `run.error`; the request this callback sends
  // carries no node id at all). The copilot itself reads the error text and
  // replies `NotAutomatable` when it genuinely cannot help — the frontend
  // gating pre-empted that classification with a guess a lost per-node
  // record could falsify. The button now shows whenever there is a run to
  // fix from; this test proves the flip against the exact pre-fix
  // expectation.
  it("offers Fix with copilot even when no node is named — the backend classifies automatable-or-not", async () => {
    // `runId` set explicitly: `failedRun()` omits it, and the button's OTHER
    // guard (`run.runId`) would then hide it for a reason unrelated to this
    // test.
    await act(async () => {
      root.render(
        createElement(RunHistoryPanel, {
          runs: [{ ...failedRun(), runId: "run-failed-no-node" }],
          graph: null,
          workflowName: "Daily digest",
          onClose: () => {},
          selectedRunSeq: null,
          onSelectRun: () => {},
          onFixWithCopilot: () => {},
        }),
      );
    });
    expect(
      container.querySelector('[data-testid="workflow-run-fix-with-copilot"]'),
    ).not.toBeNull();
  });
});

/** A run interrupted by a host restart after some nodes had already
 * completed: none of those finish rows carries an error — the synthetic
 * outcome the boot sweep writes belongs to no node — so `failedNodeOf` is
 * null the same as `failedRun()`, but this run did NOT fail "before any node
 * ran". `failureLocation` in `graph.ts` already distinguishes the two; the
 * row body must too. */
function failedAfterNodesRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 11,
    error: "harness error: host restarted mid-run",
    nodes: [
      { nodeId: "start", status: "ok", elapsedMs: 5 },
      { nodeId: "n_2", status: "ok", elapsedMs: 800 },
    ],
  });
}

// Codex review on #1821 (ninth pass): the eighth-pass fix above gated the
// remedy sentence on `failedNode` alone, so a run interrupted after nodes
// already completed collapsed into the same "nothing in the graph got the
// chance to run" claim as a run that never started at all — contradicting
// the finish rows sitting right above it in the same card.
//
// Codex review on #1821 (tenth pass): the ninth-pass fix then named a
// specific alternate cause ("a host or capability problem") for that same
// case — but `WorkflowNodeFinished` is appended best-effort (`runner.rs`),
// so a missing finish row is not proof the culprit node's own failure never
// happened; its append can silently drop while `run.error` still lands. The
// sentence now stops naming a cause it cannot actually rule in or out.
describe("the failed-run remedy distinguishes an interrupted run from one that never started", () => {
  it("names the steps that completed instead of claiming nothing ran", async () => {
    await renderHistory(failedAfterNodesRun());
    const row = container.querySelector('[data-testid="workflow-run-row"]');
    expect(row?.textContent).not.toContain(
      "nothing in the graph got the chance to run",
    );
    expect(row?.textContent).toContain("2 steps completed before this run ended");
    // Does not commit to a specific alternate cause a missing row cannot prove.
    expect(row?.textContent).not.toContain("host or capability problem");
    expect(row?.textContent).toContain("may not be fully recorded here");
  });

  // Codex review on #1821 (twelfth pass): this test's own premise was the
  // overclaim — an empty `nodes` is not proof "nothing ran", it can equally
  // mean the FIRST node's own `WorkflowNodeFinished` silently failed to
  // journal (the exact best-effort gap the tenth-pass comment above already
  // names). The `nodes.length === 0` arm now hedges the same way the
  // `nodes.length > 0` arm above already does, instead of asserting nothing
  // ran.
  it("hedges rather than asserting nothing ran for a run that failed with no nodes recorded", async () => {
    await renderHistory(failedRun());
    const row = container.querySelector('[data-testid="workflow-run-row"]');
    expect(row?.textContent).not.toContain(
      "nothing in the graph got the chance to run",
    );
    expect(row?.textContent).toContain("isn't proof nothing ran");
  });
});

/** A run cancelled cleanly at a node boundary: every node it started also
 * finished, so `RunCancel`'s own contract — the active step normally
 * completes and is journaled — held, and nothing was actually cut off. */
function cancelledAtBoundaryRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 9,
    cancelled: true,
    startedNodes: ["n_3"],
    nodes: [{ nodeId: "n_3", status: "ok", elapsedMs: 40 }],
  });
}

/** A run cancelled while a node was genuinely mid-flight: `n_3` started but
 * never finished, so it IS the node the stop cut off — the positive control
 * for the mid-flight branch (hedged, tenth pass — see below). */
function cancelledMidFlightRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 10,
    cancelled: true,
    startedNodes: ["n_3"],
    nodes: [],
  });
}

describe("the cancelled-run sentence matches whether a step was actually cut off", () => {
  it("names the mid-flight step as a possible cut-off when one genuinely was mid-flight", async () => {
    await renderHistory(cancelledMidFlightRun());
    const row = container.querySelector('[data-testid="workflow-run-cancelled"]');
    expect(row?.textContent).toContain("stopped where it was");
    // Codex review on #1821 (tenth pass): `WorkflowNodeFinished` is appended
    // best-effort, so an unmatched `startedNodes` entry is equally
    // consistent with a node that finished normally but whose own record
    // silently failed to journal — a definitive "was stopped where it was"
    // overclaims what a missing row alone can prove.
    expect(row?.textContent).toContain(
      "finished without its own record being saved",
    );
    expect(row?.textContent).toContain("isn't confirmed here");
  });

  // Codex review on #1821 (eighth pass): the legend definition (fixed earlier
  // this round, tested above) says the mid-flight step "normally ran to
  // completion and was recorded" — true for `cancelledAtBoundaryRun`, whose
  // one started node also finished. This sentence claimed the opposite for
  // the very same run.
  it("does not claim a step was cut off when every started node also finished", async () => {
    await renderHistory(cancelledAtBoundaryRun());
    const row = container.querySelector('[data-testid="workflow-run-cancelled"]');
    expect(row?.textContent).not.toContain("was stopped where it was");
    expect(row?.textContent).toContain(
      "Every step recorded as started also finished and was recorded before the stop took effect",
    );
    // The approvals sentence is unconditional and must survive either branch.
    expect(row?.textContent).toContain(
      "Any approvals it had already raised are still waiting for you.",
    );
  });

  // Codex review on #1821 (thirteenth pass): `WorkflowNodeStarted` is ALSO
  // appended best-effort (`runner.rs`'s progress collector) — the same
  // fire-and-forget semantics the tenth-pass fix already established for
  // `WorkflowNodeFinished`. A node whose own start silently failed to
  // journal never appears in `startedNodes` at all, so it can never become
  // `midFlightNode` even if it was genuinely running when the stop landed.
  // The unconditional "every step that had started completed" therefore only
  // speaks for the steps the record actually captured, not for every step
  // that in fact started — this sentence must not promise the wider claim.
  it("hedges the known-complete sentence instead of promising nothing else was cut off unseen", async () => {
    await renderHistory(cancelledAtBoundaryRun());
    const row = container.querySelector('[data-testid="workflow-run-cancelled"]');
    expect(row?.textContent).not.toContain(
      "Every step that had started completed and was recorded before the stop took effect.",
    );
    expect(row?.textContent).toContain("can't rule out one being cut off unseen");
  });
});

/** A run cancelled on a host predating #1010/#382: no `startedNodes` field at
 * all, so whether the step in progress when the stop landed finished is
 * genuinely unknowable — `WorkflowRunOutcome.startedNodes`'s own doc comment
 * says absence must read as "no start trail", never as "nothing started". */
function cancelledLegacyRun(): WorkflowRunOutcome {
  return baseRun({
    seq: 12,
    cancelled: true,
    nodes: [{ nodeId: "n_3", status: "ok", elapsedMs: 40 }],
  });
}

// Codex review on #1821 (ninth pass): `midFlightNode` folded `run.startedNodes
// ?? []` into "no node was mid-flight" for BOTH a settled run whose receipt
// confirms every started step finished AND a legacy run with no receipt at
// all — collapsing "known complete" and "unknown" into the same unconditional
// completion claim.
describe("the cancelled-run sentence hedges when the start trail itself is missing", () => {
  it("does not claim every started step completed when startedNodes is absent", async () => {
    await renderHistory(cancelledLegacyRun());
    const row = container.querySelector('[data-testid="workflow-run-cancelled"]');
    expect(row?.textContent).not.toContain(
      "Every step that had started completed",
    );
    expect(row?.textContent).not.toContain("stopped where it was");
    expect(row?.textContent).toContain("is not recorded for this run");
  });

  it("still asserts completion when startedNodes is present and empty (known: nothing had started)", async () => {
    await renderHistory(
      baseRun({ seq: 13, cancelled: true, startedNodes: [] }),
    );
    const row = container.querySelector('[data-testid="workflow-run-cancelled"]');
    expect(row?.textContent).toContain(
      "Every step recorded as started also finished and was recorded before the stop took effect",
    );
  });
});

describe("the unparkable-only blocked remedy stops naming a policy cause", () => {
  // Codex review on #1821 (eighth pass, same site as the sixth): `parkFailed`
  // fires both when the approvals queue refused the write and when this
  // runtime never wired one at all (`docs/modules/server/workflow-routes.md`)
  // — neither is a policy-content problem, and the frontend has no field
  // naming which happened, exactly as the legend definition's own hedge
  // (tested above) already accounts for. The row body still told the operator
  // to "change the policy" for the very same run.
  it("does not tell the operator to change the policy", async () => {
    await renderHistory(blockedUnparkableRun());
    const row = container.querySelector('[data-testid="workflow-run-blocked"]');
    expect(row?.textContent).not.toContain("change the policy");
    expect(row?.textContent).toContain("approvals queue itself may have refused it");
  });
});

describe("the unparkable-only blocked remedy distinguishes a discarded overflow from a park failure", () => {
  // Codex review on #1821 (twelfth pass): `discarded` (the per-turn approval
  // cap dropped the excess before the queue ever saw it) and `parkFailed`
  // (the queue itself refused the write, or none is wired) were both told as
  // "the approvals queue itself may have refused it" — true for `parkFailed`,
  // false for `discarded`, which the queue never saw at all. `run.approvals`'
  // `outcome` is what lets the row tell them apart
  // (`blockedUnparkableRun()`, used above, carries no `approvals` rows and so
  // cannot exercise this).
  it("names the per-turn cap, not the queue, when every unparkable call was discarded", async () => {
    await renderHistory(discardedOnlyBlockedRun());
    const row = container.querySelector('[data-testid="workflow-run-blocked"]');
    expect(row?.textContent).not.toContain(
      "approvals queue itself may have refused it",
    );
    expect(row?.textContent).toContain("more approvals than one batch may raise");
  });

  it("names both causes when the unparkable calls are a mix of discarded and parkFailed", async () => {
    await renderHistory(mixedUnparkableBlockedRun());
    const row = container.querySelector('[data-testid="workflow-run-blocked"]');
    expect(row?.textContent).toContain("more approvals than one batch may raise");
    expect(row?.textContent).toContain("approvals queue itself may have refused them");
  });
});
