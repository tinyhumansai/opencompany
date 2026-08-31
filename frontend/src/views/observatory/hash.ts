/**
 * The Observatory's own address grammar.
 *
 * `useHashView` carries exactly two segments — a head and a sub — and its
 * `navigate` drops every query key except `host`. This view needs four more
 * (`tab`, `agent`, `turn`, `step`), so it reads and writes the hash itself, the
 * way `WorkflowsView`'s `readWorkflowHash` does for `?run=`.
 *
 * ```
 * #/observatory                        every run
 * #/observatory?tab=analytics          cross-run analytics
 * #/observatory/<runId>                one run
 * #/observatory/<runId>?agent=theorist&turn=<agentRunId>&step=7
 * ```
 *
 * Every write goes through {@link writeObservatoryQuery}, which preserves the
 * `host` scope. That is not defensive tidiness: a `replaceState` fires no
 * `hashchange`, so a connection scope dropped by one of these writes has nothing
 * to put it back (`use-host-route.ts`).
 */

import { HOST_PARAM } from "@/hooks/use-host-route";

/** The keys this view owns, beyond the two the router carries. */
export interface ObservatoryQuery {
  /** Which top-level lens: the run list, or cross-run analytics. */
  tab: "runs" | "analytics";
  /** The agent whose thread is open, when one is. */
  agent: string | null;
  /** The attempt whose turn is expanded, when one is. */
  turn: string | null;
  /** The step scrolled to within that turn, when one is. */
  step: number | null;
  /** The DAG node selected, when one is. */
  node: string | null;
}

/** Everything the current address says about this view. */
export interface ObservatoryHash extends ObservatoryQuery {
  /** Whether the hash names this view at all. */
  onObservatory: boolean;
  /** The workflow run being inspected, or `null` on the index. */
  runId: string | null;
}

/**
 * A percent-decode that cannot throw.
 *
 * A hand-typed or truncated address can carry a lone `%`, which
 * `decodeURIComponent` rejects — and an exception here would blank the view
 * rather than land it on the index.
 */
function safeDecode(raw: string): string {
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

/** Parses `step`, rejecting anything that is not a non-negative integer. */
function readStep(raw: string | null): number | null {
  // `Number("")` is 0, so a bare `?step=` would otherwise select step zero
  // rather than nothing — a selection the operator never made.
  if (raw === null || raw.trim() === "") return null;
  const n = Number(raw);
  return Number.isInteger(n) && n >= 0 ? n : null;
}

/**
 * Reads the live address.
 *
 * From `window.location` rather than the router's props, for the reason
 * `readWorkflowHash` documents: the query is invisible to the router, and a
 * writer needs the *current* URL — not a copy that lags a `replaceState` it
 * never heard about.
 */
export function readObservatoryHash(): ObservatoryHash {
  const [path = "", query = ""] = window.location.hash.replace(/^#\/?/, "").split("?");
  const [head, runId] = path.split("/").filter(Boolean);
  const params = new URLSearchParams(query);
  return {
    onObservatory: head === "observatory",
    runId: runId ? safeDecode(runId) : null,
    tab: params.get("tab") === "analytics" ? "analytics" : "runs",
    agent: params.get("agent"),
    turn: params.get("turn"),
    step: readStep(params.get("step")),
    node: params.get("node"),
  };
}

/** Serialises this view's keys, dropping the ones at their default. */
function writeQuery(params: URLSearchParams, patch: Partial<ObservatoryQuery>): void {
  const set = (key: string, value: string | number | null | undefined) => {
    if (value === null || value === undefined || value === "") params.delete(key);
    else params.set(key, String(value));
  };
  if ("tab" in patch) set("tab", patch.tab === "analytics" ? "analytics" : null);
  if ("agent" in patch) set("agent", patch.agent);
  if ("turn" in patch) set("turn", patch.turn);
  if ("step" in patch) set("step", patch.step);
  if ("node" in patch) set("node", patch.node);
}

/**
 * The address for a run and an optional selection within it.
 *
 * For `href`s — a row in the index, an `Inspect` link from the workflow run
 * history. Carries the `host` scope, so a link followed from another host's
 * console stays on that host.
 */
export function observatoryHref(
  runId?: string | null,
  patch: Partial<ObservatoryQuery> = {},
): string {
  const params = new URLSearchParams();
  const host = new URLSearchParams(
    window.location.hash.replace(/^#/, "").split("?")[1] ?? "",
  ).get(HOST_PARAM);
  if (host) params.set(HOST_PARAM, host);
  writeQuery(params, patch);
  const path = runId ? `observatory/${encodeURIComponent(runId)}` : "observatory";
  const query = params.toString();
  return `#/${path}${query ? `?${query}` : ""}`;
}

/**
 * Moves the selection **within** the current run, in place.
 *
 * `replaceState`, never a push. Opening an agent's thread or expanding a turn is
 * a selection, not a navigation: pushing each one would make Back walk backwards
 * through every row an operator clicked while reading a single run, which is
 * exactly the Back button nobody wants. Choosing a *different run* is a
 * navigation and goes through an `href` instead.
 *
 * Silent when the hash does not name this view, so a write racing a company
 * switch cannot drag the operator back here.
 */
export function writeObservatoryQuery(patch: Partial<ObservatoryQuery>): void {
  const [path = "", query = ""] = window.location.hash.replace(/^#/, "").split("?");
  if (!path.replace(/^\/?/, "").startsWith("observatory")) return;
  const params = new URLSearchParams(query);
  writeQuery(params, patch);
  const next = params.toString();
  window.history.replaceState(null, "", `#${path}${next ? `?${next}` : ""}`);
}
