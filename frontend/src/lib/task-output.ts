// The card's link to what it produced (issue #339, epic #183 §6).
//
// A task that finished used to end as a chat message: prose in a conversation,
// with no durable thing to open, share, or hand to someone else. The host now
// stamps every successful attempt with a `TaskOutput`; this module turns that
// record into the one link a card shows, and into the addresses that link
// resolves to.
//
// # The precedence lives here and nowhere else
//
// A card shows **one** primary link — artifact, else workflow, else the run
// trace — and reaches the rest through the task detail. That primary is
// *derived* from the stamp on every render rather than persisted alongside it,
// so it can never contradict the list it came from. This module is the single
// site of that rule; the host deliberately stores the facts and not the choice.
//
// # Why the routes are query strings and not path segments
//
// `hooks/use-hash-view.ts` is the app's hash router, and its `canonicalize()`
// runs on **every** `hashchange`, rewriting the URL to at most `head/sub` — two
// segments. A third segment (`#/tasks/t-1/artifacts/a-1/3`) is therefore
// silently replaced away before anything can read it, which would make these
// links appear to work and then not.
//
// `readSegments()` strips everything after `?` before that comparison, and
// `canonicalize` early-returns when the two-segment path already matches — so a
// hash **query string** survives untouched. That is load-bearing, not a style
// choice: do not "tidy" these into path segments.

import type { Task, TaskOutput } from "@/api/tasks";

/** Where a card's link points, and what to call it. */
export interface TaskLink {
  /** What is at the other end — drives the icon and the label's voice. */
  kind: "artifact" | "workflow" | "trace" | "card";
  /** The `#/…` address, ready for an anchor's `href`. */
  href: string;
  /** The operator-facing label. */
  label: string;
  /** A longer explanation for the anchor's `title`, when one helps. */
  hint?: string;
}

/** `#/tasks/<id>` — the card itself, and the link of last resort. */
export function cardHref(taskId: string): string {
  return `#/tasks/${encodeURIComponent(taskId)}`;
}

/**
 * `#/tasks/<id>?artifact=<artifactId>&v=<version>` — the task's Artifacts tab,
 * with that deliverable open at the revision the run wrote.
 */
export function artifactHref(
  taskId: string,
  artifactId: string,
  version: number,
): string {
  return `${cardHref(taskId)}?artifact=${encodeURIComponent(artifactId)}&v=${version}`;
}

/**
 * `#/tasks/<id>?run=<runId>` — the task's Attempts tab, with that attempt's
 * trace open. This is what "no artifact" resolves to, which is why it is a
 * first-class address rather than a fallback with no URL.
 */
export function traceHref(taskId: string, runId: string): string {
  return `${cardHref(taskId)}?run=${encodeURIComponent(runId)}`;
}

/**
 * `#/workflows/<id>`, plus `?run=<runId>` when the attempt actually executed
 * it — the canvas, showing what ran rather than only what the graph says now.
 */
export function workflowHref(workflowId: string, runId?: string): string {
  const base = `#/workflows/${encodeURIComponent(workflowId)}`;
  return runId ? `${base}?run=${encodeURIComponent(runId)}` : base;
}

/** Everything the stamp points at, in precedence order. */
function linksFor(taskId: string, output: TaskOutput): TaskLink[] {
  const links: TaskLink[] = [];
  for (const artifact of output.artifacts ?? []) {
    links.push({
      kind: "artifact",
      href: artifactHref(taskId, artifact.artifactId, artifact.version),
      label: `Open ${artifact.title}`,
      hint: `Opens v${artifact.version} — the version this run produced.`,
    });
  }
  for (const workflow of output.workflows ?? []) {
    links.push({
      kind: "workflow",
      href: workflowHref(workflow.workflowId, workflow.runId),
      label: `Open workflow ${workflow.workflowId}`,
      hint:
        workflow.action === "ran"
          ? "Opens the workflow on its canvas, showing this run."
          : "Opens the workflow this task built. It has not been run yet.",
    });
  }
  // Always last, and always present: the attempt is the deliverable when
  // nothing else is, and the fallback when a published artifact is later
  // deleted. This is what stops "no artifact" degrading into "no link".
  links.push({
    kind: "trace",
    href: traceHref(taskId, output.runId),
    label: output.attempt
      ? `View run trace · attempt ${output.attempt}`
      : "View run trace",
    hint: "Opens what this attempt actually did, step by step.",
  });
  return links;
}

/**
 * The single link a card shows: artifact, else workflow, else the trace.
 *
 * A card with no stamp — never succeeded, dragged to Done by hand, or settled
 * before #339 — falls back to the card itself. That is deliberate and it is
 * where the epic's *"every card in Done has a link"* honestly stops: nothing
 * recorded an attempt for those, and synthesizing one would be a lie about
 * identity. A link to the card is at least true.
 */
export function primaryLink(task: Task): TaskLink {
  if (!task.output) {
    return {
      kind: "card",
      href: cardHref(task.id),
      label: "Open this task",
      hint: "This card recorded no attempt, so there is nothing else to open.",
    };
  }
  return linksFor(task.id, task.output)[0];
}

/**
 * How many further things this card produced beyond the primary link.
 *
 * The trace is not counted: it is always reachable and always last, so
 * counting it would put a `+1 more` on every single stamped card and mean
 * nothing. This counts only additional *deliverables*.
 */
export function extraOutputCount(task: Task): number {
  const output = task.output;
  if (!output) return 0;
  const deliverables =
    (output.artifacts?.length ?? 0) + (output.workflows?.length ?? 0);
  return Math.max(0, deliverables - 1);
}

/** What a `#/tasks/<id>?…` address asks the detail screen to open. */
export interface TaskFocus {
  /** Open this artifact on the Artifacts tab… */
  artifactId?: string;
  /** …pinned at this revision, when the address named one. */
  version?: number;
  /** Or open this attempt's trace on the Attempts tab. */
  runId?: string;
}

/**
 * Reads the focus out of a `#/tasks/<id>?…` hash.
 *
 * Tolerant by construction: a malformed or unknown query yields an empty focus
 * and the detail screen opens on its default tab. A link that has gone stale
 * should land somewhere sensible, never on an error.
 */
export function readTaskFocus(hash: string): TaskFocus {
  const query = hash.split("?")[1];
  if (!query) return {};
  let params: URLSearchParams;
  try {
    params = new URLSearchParams(query);
  } catch {
    return {};
  }
  const focus: TaskFocus = {};
  const artifactId = params.get("artifact");
  if (artifactId) {
    focus.artifactId = artifactId;
    const version = Number.parseInt(params.get("v") ?? "", 10);
    // A version that is not a positive integer is dropped rather than
    // guessed at, which lands the reader on the newest revision — the same
    // place they would have arrived with no pin at all.
    if (Number.isInteger(version) && version > 0) focus.version = version;
  }
  const runId = params.get("run");
  if (runId) focus.runId = runId;
  return focus;
}

/** Whether a focus asks for anything at all. */
export function hasFocus(focus: TaskFocus): boolean {
  return Boolean(focus.artifactId || focus.runId);
}
