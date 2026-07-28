// Product-tour first-run state.
//
// The console has no per-user preferences field on the backend yet
// (`UserRecord` carries none), so — exactly like the workspace and inbox
// surfaces (`lib/workspace.ts`, `lib/inbox.ts`) — we persist "has seen the
// tour" in localStorage, keyed per company. A backend `UserRecord` flag for
// cross-device memory is a named follow-up, out of this v1's scope.

const KEY = (company: string | null): string => `oc-tour:${company ?? "single"}`;

export interface TourState {
  completed?: boolean;
  skipped?: boolean;
  seenAt?: number;
}

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
