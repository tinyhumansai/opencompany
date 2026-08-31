// First-run setup's one piece of browser-local state: did this operator say
// "I'll do this later"?
//
// Keyed per (connection, company) exactly like `tour/state.ts`, so two hosts
// serving a company of the same name never share one operator's decision.
//
// ## Why a browser flag is safe here and would not be for "has setup run"
//
// `tour/state.ts` explains that first-run state lives in `localStorage` because
// `UserRecord` carries no per-user field. For the tour that is a small cost:
// cleared storage re-offers a walkthrough.
//
// Setup *creates things*, so the same trade would be unacceptable for the
// question "has setup already run?" — cleared storage would build a second team
// on top of the first. That question is therefore answered by the host instead:
// `shouldOfferSetup` asks whether the roster is empty (see
// `lib/company-setup.ts`).
//
// What lives here is only the *skip*, and skipping can do exactly one thing:
// hide an offer. Losing it re-offers setup to a company that still has nobody on
// it, which is the correct outcome anyway. So the fragile store holds the
// harmless half, and the durable store holds the half that matters.

import { type LocalScope, scopedKey } from "@/connections/types";

const KEY = (scope: LocalScope): string => scopedKey("oc-setup", scope);

interface SetupState {
  skipped?: boolean;
  /**
   * The operator left this flow to wire a model and has not been brought back.
   *
   * The exact opposite of [`skipped`] despite sharing a record: skipping hides
   * the offer, this one owes the operator a re-offer. It is here rather than in
   * a ref because wiring a provider can ask for a restart, and a console
   * reloaded on the Connections page would otherwise lose the debt — which is
   * the whole failure this flag exists to prevent.
   *
   * Losing it anyway (private mode, cleared storage) costs nothing worse than
   * the operator reaching setup through the Company page's prompt, exactly as
   * they did before. It can only ever *offer* something.
   */
  resuming?: boolean;
  /**
   * The operator finished a fallback team, was sent to Settings to wire a
   * model, and is owed a redesign on return.
   *
   * Set by the completion screen's "Add a model in Settings" action. Distinct
   * from [`resuming`] because the two returns differ: a resume reopens the
   * questions over an **unstaffed** company, while a redesign reopens over the
   * standard team the first pass just created — and must replace it rather than
   * stack a second one.
   */
  redesign?: boolean;
  /**
   * The host ids of the fallback team the first pass created, captured when the
   * operator left to wire a model.
   *
   * The redesign's replacement may only remove **these** rows. Re-deriving the
   * list from the roster on return would sweep up teammates other operators
   * added while model settings were open — those rows belong to someone else's
   * work and must survive the replacement. Ids survive a reload of the settings
   * page, which is the whole point of storing them here rather than in a ref.
   */
  fallbackIds?: string[];
  at?: number;
}

function read(scope: LocalScope): SetupState {
  try {
    const raw = localStorage.getItem(KEY(scope));
    return raw ? (JSON.parse(raw) as SetupState) : {};
  } catch {
    return {};
  }
}

/** Has this operator dismissed the setup offer for this company? */
export function setupSkipped(scope: LocalScope): boolean {
  return Boolean(read(scope).skipped);
}

/**
 * Record "I'll do this later", so the dialog stops opening by itself.
 *
 * Writes the record whole, which drops any pending resume: an operator who went
 * to wire a model and then said "later" has said "later", and honouring the
 * older debt would reopen the dialog they just dismissed.
 */
export function markSetupSkipped(scope: LocalScope): void {
  try {
    localStorage.setItem(KEY(scope), JSON.stringify({ skipped: true, at: Date.now() }));
  } catch {
    /* private mode / quota — setup simply re-offers on the next load */
  }
}

/** Is setup owed a re-offer because the operator left to wire a model? */
export function setupResuming(scope: LocalScope): boolean {
  return Boolean(read(scope).resuming);
}

/**
 * Record that the operator left for model settings mid-setup.
 *
 * Merged onto whatever is stored rather than written whole: a company that had
 * been skipped before can be forced open again from the Team page, and clearing
 * the skip here would silently re-enable the unprompted offer.
 */
export function markSetupResuming(scope: LocalScope): void {
  try {
    const next: SetupState = { ...read(scope), resuming: true, at: Date.now() };
    localStorage.setItem(KEY(scope), JSON.stringify(next));
  } catch {
    /* private mode / quota — the Team page's prompt is still the way back */
  }
}

/** Forget the debt, once it has been paid by reopening the dialog. */
export function clearSetupResuming(scope: LocalScope): void {
  try {
    const { resuming: _dropped, ...rest } = read(scope);
    localStorage.setItem(KEY(scope), JSON.stringify(rest));
  } catch {
    /* nothing to clear */
  }
}

/** Is a redesign owed because the operator left the completion screen to wire a model? */
export function setupRedesign(scope: LocalScope): boolean {
  return Boolean(read(scope).redesign);
}

/**
 * Record that the operator left the completion screen to wire a model and
 * wants the standard team redesigned on their return.
 *
 * Merged rather than written whole, for the same reason as
 * [`markSetupResuming`]: a company that had been skipped before can be forced
 * open again, and clearing the skip here would silently re-enable the
 * unprompted offer.
 *
 * `fallbackIds` is the host ids of the team the first pass created, so the
 * redesign replaces exactly that team and nothing another operator added while
 * model settings were open. Captured at leave time — the roster on return is a
 * snapshot of everyone's work since, and must not be the boundary of what a
 * redesign may remove.
 */
export function markSetupRedesign(scope: LocalScope, fallbackIds?: string[]): void {
  try {
    const next: SetupState = {
      ...read(scope),
      redesign: true,
      ...(fallbackIds?.length ? { fallbackIds } : {}),
      at: Date.now(),
    };
    localStorage.setItem(KEY(scope), JSON.stringify(next));
  } catch {
    /* private mode / quota — the Company page's prompt is still the way back */
  }
}

/** The fallback team's host ids a pending redesign may replace, if any. */
export function setupRedesignIds(scope: LocalScope): string[] {
  return read(scope).fallbackIds ?? [];
}

/**
 * Forget the redesign debt — both the owed flag and the rows it names.
 *
 * Called when the redesign is resolved (its build-out completed, which also
 * clears the whole state) or explicitly declined ("I'll do this later"), never
 * when the return merely reopens the dialog: a reload between the reopen and
 * the build-out must still find the debt, or the fallback team would leave the
 * ordinary gate reporting staffed and the owed redesign would be unreachable.
 */
export function clearSetupRedesign(scope: LocalScope): void {
  try {
    const { redesign: _dropped, fallbackIds: _ids, ...rest } = read(scope);
    localStorage.setItem(KEY(scope), JSON.stringify(rest));
  } catch {
    /* nothing to clear */
  }
}

/**
 * Forget the skip.
 *
 * Called when setup completes, so the flag cannot outlive the thing it was
 * suppressing: an operator who skips, later runs setup, and then removes every
 * agent should be offered setup again rather than silently left on an empty
 * team page.
 */
export function clearSetupSkipped(scope: LocalScope): void {
  try {
    localStorage.removeItem(KEY(scope));
  } catch {
    /* nothing to clear */
  }
}

// ---------------------------------------------------------------------------
// The sign-in hand-off marker
// ---------------------------------------------------------------------------

/**
 * The hash-query key a setup hand-off link carries, so a sign-in that
 * navigates the whole document away (setup's button sets `window.location.href`)
 * still lands knowing setup just finished: `…code=…#/company?from=setup`.
 *
 * `useHashView`'s segment parsing strips everything from `?` onward, so the
 * flag never reaches the router; AppShell consumes it on the landing mount to
 * apply the same welcome suppression a same-mount completion gets, then removes
 * it so a reload or a copied link cannot re-apply it.
 */
export const SETUP_HANDOFF_FLAG = "from";

/**
 * The landing fragment a setup hand-off link carries. The wizard hands this to
 * the host so a *mailed* link lands the same way the loopback link does.
 */
export const SETUP_HANDOFF_FRAGMENT = `#/company?${SETUP_HANDOFF_FLAG}=setup`;

/** A fragment marker scoped to one connection and company. */
export function setupHandoffFragment(scope: SetupHandoffScope): string {
  const company = scope.company ?? "single";
  return `#/company?${SETUP_HANDOFF_FLAG}=setup&connection=${encodeURIComponent(scope.connection)}&company=${encodeURIComponent(company)}`;
}

export interface SetupHandoffScope {
  connection: string;
  company: string | null;
}

/** Whether the current address arrived from setup's sign-in hand-off. */
export function arrivedViaSetupHandoff(scope?: SetupHandoffScope): boolean {
  const [, query = ""] = window.location.hash.split("?");
  const params = new URLSearchParams(query);
  if (params.get(SETUP_HANDOFF_FLAG) !== "setup") return false;
  if (!scope) return true;
  return (
    params.get("connection") === scope.connection &&
    params.get("company") === (scope.company ?? "single")
  );
}

/**
 * Whether the hand-off marker is scoped to a connection and company.
 *
 * `setupHandoffFragment` encodes the scope so a marker addressed to one company
 * cannot be consumed by another; the setup wizard and magic-link flow leave it
 * out because their scope may not survive the full-page hand-off. Telling the
 * two apart is what lets AppShell accept the unscoped form on whatever company
 * it lands on while still refusing a marker scoped somewhere else.
 */
export function setupHandoffHasScope(): boolean {
  const [, query = ""] = window.location.hash.split("?");
  const params = new URLSearchParams(query);
  return params.has("connection") || params.has("company");
}

/**
 * Whether the current address rode in on a hub sign-in that was asked to land
 * on setup's destination.
 *
 * The host puts the destination on the hub's return URI as a *query* parameter
 * (`?company=…&from=setup`), because a fragment cannot cross the OAuth round
 * trip — the hub appends its own `token=` to whatever it was given, and
 * anything after a `#` there would swallow it.
 */
export function arrivedViaHubSetupHandoff(scope?: SetupHandoffScope): boolean {
  const params = new URLSearchParams(window.location.search);
  if (params.get(SETUP_HANDOFF_FLAG) !== "setup") return false;
  if (!scope) return true;
  return (
    params.get("connection") === scope.connection &&
    params.get("company") === (scope.company ?? "single")
  );
}

/**
 * Consumes a hub-carried setup destination, translating it into the same
 * one-shot hash marker a setup hand-off link carries.
 *
 * An ecosystem sign-in returns to `/?company=…&from=setup`; the token
 * redemption strips the hub's own params but leaves `from`. This reads it,
 * takes it out of the query, and writes `#/company?from=setup` — the exact
 * landing a setup link would have produced — so the shell applies the same
 * welcome suppression and route, then clears the marker like any other
 * hand-off. A reload after the conversion has neither the query flag nor the
 * hash marker, so it cannot re-apply either.
 */
export function absorbHubSetupHandoff(scope?: SetupHandoffScope): void {
  if (!arrivedViaHubSetupHandoff(scope)) return;
  const params = new URLSearchParams(window.location.search);
  params.delete(SETUP_HANDOFF_FLAG);
  const qs = params.toString();
  window.history.replaceState(
    {},
    "",
    window.location.pathname + (qs ? `?${qs}` : "") + window.location.hash,
  );
  window.location.hash = scope ? setupHandoffFragment(scope) : SETUP_HANDOFF_FRAGMENT;
}

/**
 * Removes the hand-off flag from the address.
 *
 * One-shot: the suppression it enables belongs to the arrival it rode in on,
 * not to a later reload. Other hash-query keys (`?host=`, for instance) are
 * preserved.
 */
export function clearSetupHandoff(): void {
  const [path, query = ""] = window.location.hash.replace(/^#/, "").split("?");
  const params = new URLSearchParams(query);
  if (!params.has(SETUP_HANDOFF_FLAG)) return;
  params.delete(SETUP_HANDOFF_FLAG);
  const qs = params.toString().replace(/=(?=&|$)/g, "");
  const next = `#${path}${qs ? `?${qs}` : ""}`;
  if (next !== window.location.hash) window.history.replaceState(null, "", next);
}
