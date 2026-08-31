import { beforeEach, describe, expect, it, vi } from "vitest";

import type { WorkflowGraph } from "@/api/workflows";
import { foldLiveRun } from "@/views/workflows/graph";

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));

vi.mock("sonner", () => {
  const toast = Object.assign(toasts.base, {
    success: toasts.success,
    error: toasts.error,
    warning: toasts.warning,
    info: toasts.info,
  });
  return { toast };
});

const { handleEvent } = await import("@/hooks/use-events");
type Ev = import("@/hooks/use-events").CompanyStreamEvent;
type Subs = import("@/hooks/use-events").Subscribers;

/**
 * Issue #382: "currently executing" is now REPORTED by the host, not derived.
 *
 * The engine gained an `on_step_start` hook, so a run brackets each non-trigger
 * node with a `workflow_node_started` frame ahead of its `workflow_node_finished`.
 * Two contracts carry that on the console:
 *
 * - `foldLiveRun` marks a node `running` when its start frame arrives and settles
 *   it on the finish frame — no more topology-derived frontier that over-marked
 *   both arms of a branch; and
 * - the frame reaches the run subscriber rather than falling through `default:`
 *   and vanishing (the trap this file has been bitten by three times).
 */

/** `start → ceo → done` — one trigger, one agent, one output. */
const GRAPH: WorkflowGraph = {
  id: "greet",
  name: "Greet",
  version: null,
  nodes: [
    { id: "start", kind: "trigger", name: "Start" },
    { id: "ceo", kind: "agent", name: "CEO", agent: "ceo" },
    { id: "done", kind: "output", name: "Done" },
  ],
  edges: [
    { from: "start", to: "ceo" },
    { from: "ceo", to: "done" },
  ],
};

const start = (runId: string): Ev => ({
  type: "workflow_run_started",
  seq: 1,
  atMillis: 1,
  workflowId: "greet",
  runId,
  scheduled: false,
  startedBy: "operator",
});
const nodeStarted = (runId: string, nodeId: string): Ev => ({
  type: "workflow_node_started",
  seq: 2,
  atMillis: 2,
  workflowId: "greet",
  runId,
  nodeId,
});
const nodeFinished = (runId: string, nodeId: string, status: string): Ev => ({
  type: "workflow_node_finished",
  seq: 3,
  atMillis: 3,
  workflowId: "greet",
  runId,
  nodeId,
  status,
  elapsedMs: 5,
});
const finished = (runId: string): Ev => ({
  type: "workflow_run_finished",
  seq: 4,
  atMillis: 4,
  workflowId: "greet",
  scheduled: false,
  deliveries: [],
  pendingApprovals: [],
  runId,
});

describe("foldLiveRun reports running from node-started frames", () => {
  it("only the trigger is marked before any node-started frame arrives", () => {
    // The old fold lit up the trigger's successors as a guessed frontier. Now
    // nothing but the trigger is marked until the host says a node started.
    const live = foldLiveRun([start("r1")], "greet", GRAPH);
    expect(live?.states).toEqual({ start: "ok" });
  });

  it("marks a node running on its started frame, then settles it on finish", () => {
    const live = foldLiveRun(
      [start("r1"), nodeStarted("r1", "ceo")],
      "greet",
      GRAPH,
    );
    expect(live?.states.ceo).toBe("running");
    // `done` is NOT lit — nothing derives it any more; it waits for its own
    // started frame.
    expect(live?.states.done).toBeUndefined();

    const settled = foldLiveRun(
      [start("r1"), nodeStarted("r1", "ceo"), nodeFinished("r1", "ceo", "ok")],
      "greet",
      GRAPH,
    );
    expect(settled?.states.ceo).toBe("ok");
    expect(settled?.active).toBe(true);
  });

  it("a finished frame cannot be downgraded by a stray later start", () => {
    // Ordering guarantees start precedes finish, but the guard must hold even if
    // a frame arrives out of order: a settled node stays settled.
    const live = foldLiveRun(
      [start("r1"), nodeFinished("r1", "ceo", "ok"), nodeStarted("r1", "ceo")],
      "greet",
      GRAPH,
    );
    expect(live?.states.ceo).toBe("ok");
  });

  it("clears an orphaned running mark once the run settles (cancel/crash)", () => {
    // ceo started but never finished — the run ended on it. The settled sweep
    // drops the orphan so the canvas does not pulse "running" forever.
    const live = foldLiveRun(
      [start("r1"), nodeStarted("r1", "ceo"), finished("r1")],
      "greet",
      GRAPH,
    );
    expect(live?.active).toBe(false);
    expect(live?.states.ceo).toBeUndefined();
    // The trigger's reported "ok" is not a frontier guess, so it stays.
    expect(live?.states.start).toBe("ok");
  });

  it("ignores node-started frames from a different run on the shared stream", () => {
    // One SSE connection carries every run in the company; a concurrent run's
    // start frame must not light a node on the run being watched.
    const live = foldLiveRun(
      [start("r1"), nodeStarted("r2", "ceo")],
      "greet",
      GRAPH,
    );
    expect(live?.states.ceo).toBeUndefined();
  });
});

/**
 * Issue #863: a console that did not see the run start.
 *
 * The frame window only holds what arrived since this console connected, so a
 * run already walking when the tab was opened — a cron fire, a run started from
 * chat, a reload, an `EventSource` reconnect — has no `workflow_run_started` in
 * it. The fold used to return `null` there, which painted NOTHING for the whole
 * run: not a partial trail, nothing. The host serves that run on
 * `…/workflows/runs` with `running: true` and the nodes it has finished, and
 * that read is what the fold now adopts.
 */
describe("foldLiveRun joins a run it did not see start", () => {
  const seed = (runId: string, states: Record<string, "ok" | "error" | "running">) => ({
    runId,
    states,
    elapsed: { ceo: 12 },
    scheduled: false,
  });

  it("paints nothing without a seed — the regression #863 reports", () => {
    // No start frame in the window: every later frame is stranded behind the
    // one thing the fold used to need.
    expect(foldLiveRun([nodeStarted("r1", "ceo")], "greet", GRAPH)).toBeNull();
  });

  it("adopts the host's in-flight run and keeps its recorded trail", () => {
    const live = foldLiveRun([], "greet", GRAPH, seed("r1", { ceo: "ok" }));
    expect(live?.runId).toBe("r1");
    expect(live?.active).toBe(true);
    expect(live?.states.ceo).toBe("ok");
    expect(live?.elapsed.ceo).toBe(12);
    // Still seeded from the graph, so the trigger reads as fired.
    expect(live?.states.start).toBe("ok");
  });

  it("keeps painting from the frames that arrive after it joins", () => {
    const live = foldLiveRun(
      [nodeStarted("r1", "done"), nodeFinished("r1", "done", "ok")],
      "greet",
      GRAPH,
      seed("r1", { ceo: "ok" }),
    );
    expect(live?.states).toMatchObject({ start: "ok", ceo: "ok", done: "ok" });
  });

  it("settles when the run it adopted finishes", () => {
    const live = foldLiveRun(
      [nodeStarted("r1", "done"), finished("r1")],
      "greet",
      GRAPH,
      seed("r1", { ceo: "ok" }),
    );
    expect(live?.active).toBe(false);
    // The orphan sweep applies to an adopted run exactly as it does to a
    // watched one: `done` started and never finished.
    expect(live?.states.done).toBeUndefined();
    expect(live?.states.ceo).toBe("ok");
  });

  it("ignores frames belonging to another run while it is adopted", () => {
    const live = foldLiveRun(
      [nodeStarted("r2", "done"), finished("r2")],
      "greet",
      GRAPH,
      seed("r1", { ceo: "ok" }),
    );
    expect(live?.active).toBe(true);
    expect(live?.states.done).toBeUndefined();
  });

  it("a live start frame outranks the seed — a rerun supersedes the run before", () => {
    const live = foldLiveRun(
      [start("r2"), nodeStarted("r2", "ceo")],
      "greet",
      GRAPH,
      seed("r1", { ceo: "ok", done: "ok" }),
    );
    expect(live?.runId).toBe("r2");
    // The older run's finished nodes are gone: this is a different run, and
    // carrying its predecessor's marks over would be a lie about this one.
    expect(live?.states.done).toBeUndefined();
    expect(live?.states.ceo).toBe("running");
  });
});

describe("workflow_node_started routing", () => {
  function subscribers() {
    return {
      onAgentReply: vi.fn(),
      onTaskEvent: vi.fn(),
      onWorkspaceEvent: vi.fn(),
      onTurnEvent: vi.fn(),
      onWorkflowRunEvent: vi.fn(),
      onWorkflowChanged: vi.fn(),
      onApprovalEvent: vi.fn(),
    } satisfies Subs;
  }

  beforeEach(() => {
    for (const fn of Object.values(toasts)) fn.mockClear();
  });

  it("reaches the run subscriber and raises no toast", () => {
    const subs = subscribers();
    handleEvent(nodeStarted("r1", "ceo"), subs);

    expect(subs.onWorkflowRunEvent).toHaveBeenCalledTimes(1);
    expect(subs.onWorkflowRunEvent).toHaveBeenCalledWith(nodeStarted("r1", "ceo"));
    // Progress is not an attention signal — a node lighting up on the canvas is
    // the feedback, not a toast.
    for (const fn of Object.values(toasts)) expect(fn).not.toHaveBeenCalled();
    // And no cross-wire to an unrelated surface.
    expect(subs.onTaskEvent).not.toHaveBeenCalled();
    expect(subs.onWorkflowChanged).not.toHaveBeenCalled();
    expect(subs.onWorkspaceEvent).not.toHaveBeenCalled();
  });
});

/**
 * Issue #921: a finished run still reads as running.
 *
 * The fold clears `active` on one signal only — a `workflow_run_finished` frame
 * for the run it is following. When the stream dies mid-run that frame never
 * arrives, so the canvas keeps a node pulsing and the header keeps saying
 * "running" long after the host is done; three QA reports were filed as engine
 * hangs on the strength of it, and only a reload ever corrected the view.
 *
 * The host's run history is the authority that survives a dead stream: a run it
 * no longer lists as `running` IS settled, whatever the frame window shows. The
 * console polls that list, so this is reachable without the broken stream.
 */
describe("a run settles from the host's history when its stream dies", () => {
  /** The window a dropped stream leaves behind: the run started, the agent node
   * started, and then nothing — no finish for the node, none for the run. */
  const truncated: Ev[] = [start("r1"), nodeStarted("r1", "ceo")];

  it("stays active on the frames alone — the state the bug reports", () => {
    const live = foldLiveRun(truncated, "greet", GRAPH);
    expect(live?.active).toBe(true);
    expect(live?.states.ceo).toBe("running");
  });

  it("settles once the host lists the run as no longer running", () => {
    const live = foldLiveRun(truncated, "greet", GRAPH, null, new Set(["r1"]));
    expect(live?.active).toBe(false);
  });

  it("clears the orphaned running mark but keeps what was reported", () => {
    const window: Ev[] = [
      start("r1"),
      nodeStarted("r1", "ceo"),
      nodeFinished("r1", "ceo", "ok"),
      nodeStarted("r1", "done"),
    ];
    const live = foldLiveRun(window, "greet", GRAPH, null, new Set(["r1"]));
    expect(live?.active).toBe(false);
    // The node the run died on stops pulsing…
    expect(live?.states.done).toBeUndefined();
    // …and the honest "how far did it get?" answer survives.
    expect(live?.states.ceo).toBe("ok");
  });

  it("a run the host still lists as running is left alone", () => {
    const live = foldLiveRun(truncated, "greet", GRAPH, null, new Set(["other"]));
    expect(live?.active).toBe(true);
    expect(live?.states.ceo).toBe("running");
  });

  // The dispatch's other half: a fix that repairs "finished" but not "failed"
  // is half a fix. The host settles a run the same way whatever became of it —
  // failed, cancelled or denied all stop being `running` — so the reconciliation
  // must not be keyed on a successful outcome.
  it("settles a run that ended badly, not only a clean one", () => {
    const failed: Ev[] = [
      start("r1"),
      nodeStarted("r1", "ceo"),
      nodeFinished("r1", "ceo", "error"),
      nodeStarted("r1", "done"),
    ];
    const live = foldLiveRun(failed, "greet", GRAPH, null, new Set(["r1"]));
    expect(live?.active).toBe(false);
    expect(live?.states.ceo).toBe("error");
    expect(live?.states.done).toBeUndefined();
  });

  it("settles a run adopted from history rather than from a start frame", () => {
    // No start frame in the window at all (issue #863's path) — a console that
    // joined mid-run and then lost the stream is the worst case, because it has
    // neither a start frame nor a finish one.
    const seed = {
      runId: "r1",
      states: { ceo: "ok" as const },
      elapsed: { ceo: 5 },
      scheduled: false,
    };
    const stillRunning = foldLiveRun([], "greet", GRAPH, seed);
    expect(stillRunning?.active).toBe(true);

    const live = foldLiveRun([], "greet", GRAPH, seed, new Set(["r1"]));
    expect(live?.active).toBe(false);
    expect(live?.states.ceo).toBe("ok");
  });
});
