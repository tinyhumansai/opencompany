// Product-tour first-run state.
//
// The console has no per-user preferences field on the backend yet
// (`UserRecord` carries none), so — exactly like the workspace surface
// (`lib/workspace.ts`) — we persist "has seen the
// tour" in localStorage, keyed per company. A backend `UserRecord` flag for
// cross-device memory is a named follow-up, out of this v1's scope.

const KEY = (company: string | null): string => `oc-tour:${company ?? "single"}`;

export interface TourState {
  completed?: boolean;
  skipped?: boolean;
  seenAt?: number;
  /**
   * Set when a tour stop hands the browser to a third party (the OAuth connect
   * flow navigates the whole document away), so the controller can pick the
   * tour back up on the stop the operator left. See `pendingResume` helpers.
   */
  pendingResume?: TourResume;
}

/**
 * Where to resume the tour after a full-page round trip.
 *
 * The stop is identified by its **view**, not its index: issue #302 deleted a
 * stop, and a stored index would have resumed on the wrong one — or past the
 * end, where the controller's `if (!stop) return` guard swallows it silently.
 */
export interface TourResume {
  view: string;
  at: number;
}

/**
 * How long a resume marker stays good. Deliberately longer than the OAuth
 * `state` nonce (10 minutes host-side) so a handshake that only just made it
 * still resumes, but short enough that a marker abandoned mid-flow cannot
 * hijack an unrelated visit hours later.
 */
const RESUME_TTL_MS = 15 * 60 * 1000;

/** Fired by `restartTour` so a mounted controller can replay from the top. */
export const RESTART_EVENT = "oc-tour:restart";

export function readTourState(company: string | null): TourState {
  try {
    const raw = localStorage.getItem(KEY(company));
    return raw ? (JSON.parse(raw) as TourState) : {};
  } catch {
    return {};
  }
}

export function writeTourState(company: string | null, next: TourState): void {
  try {
    localStorage.setItem(KEY(company), JSON.stringify({ ...next, seenAt: Date.now() }));
  } catch {
    /* private mode / quota — the tour simply re-offers on next load */
  }
}

/** Has the operator already finished or skipped the tour for this company? */
export function tourSeen(company: string | null): boolean {
  const s = readTourState(company);
  return Boolean(s.completed || s.skipped);
}

/**
 * Record that the tour was on `view` when the page handed off to a third party,
 * so the next mount resumes there instead of re-offering the tour from step 1.
 *
 * Read-modify-write, and in the **same per-company key** as the seen flags: a
 * marker written under one company must never resume that company's tour inside
 * another (the contamination the controller's company-switch teardown guards
 * against).
 */
export function markTourResume(company: string | null, view: string): void {
  const current = readTourState(company);
  writeTourState(company, { ...current, pendingResume: { view, at: Date.now() } });
}

/**
 * The view to resume on, or `null` when there is no marker or it has aged out.
 * A stale marker is treated as absent — the caller clears it either way.
 */
export function readTourResume(company: string | null): string | null {
  const { pendingResume } = readTourState(company);
  if (!pendingResume) return null;
  if (Date.now() - pendingResume.at > RESUME_TTL_MS) return null;
  return pendingResume.view;
}

/**
 * The stop a running tour is currently on, or `null` when no tour is running.
 *
 * Module-level rather than persisted: it changes on every step, and only the
 * one moment that hands the browser away needs it to survive. `armTourResume`
 * is what turns this live position into a stored marker.
 */
let activeStopView: string | null = null;

/** Called by the controller as each stop renders; `null` when the tour ends. */
export function setActiveTourStop(view: string | null): void {
  activeStopView = view;
}

/**
 * Arm a resume marker for wherever the tour currently is, so a full-page
 * navigation (the OAuth connect flow) can be picked back up.
 *
 * A **no-op when no tour is running** — that is what lets a caller arm
 * unconditionally without first asking whether it is inside onboarding.
 */
export function armTourResume(company: string | null): void {
  if (activeStopView === null) return;
  markTourResume(company, activeStopView);
}

/** Drop the resume marker, keeping the completed/skipped flags intact. */
export function clearTourResume(company: string | null): void {
  const current = readTourState(company);
  if (!current.pendingResume) return;
  const { pendingResume: _dropped, ...rest } = current;
  writeTourState(company, rest);
}

/** Clear the seen flag and ask any mounted controller to replay from step 1. */
export function restartTour(company: string | null): void {
  try {
    localStorage.removeItem(KEY(company));
  } catch {
    /* ignore */
  }
  window.dispatchEvent(new CustomEvent(RESTART_EVENT));
}

/**
 * Dev-only force flag (mirrors OpenHuman's `VITE_DEV_FORCE_ONBOARDING`): set
 * `VITE_DEV_FORCE_TOUR=true` in a dev build to reopen the tour every load.
 * Read defensively so it needs no addition to the Vite env type surface.
 */
export function tourForced(): boolean {
  const env = import.meta.env as Record<string, string | boolean | undefined>;
  return Boolean(env.DEV) && env.VITE_DEV_FORCE_TOUR === "true";
}
