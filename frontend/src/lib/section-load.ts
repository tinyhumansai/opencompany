// How a connections-section fetch failure should be read (issues #1470, #2081).
//
// Five sections on the old Connections page treated ANY fetch failure as "this host
// has no such thing" and unmounted themselves, so a transient 500 or a dropped
// session was indistinguishable from a feature the host genuinely does not have
// — the operator concluded the feature was missing and went looking for a
// rebuild. `CompanyCredentialCard` already draws the right distinction one
// directory over; this is that rule, extracted so every section routes through
// the same decision.
//
// #1470 fixed that in one direction and left the other open: with only a 404
// counted as "not served here", every *other* permanent refusal fell into the
// transient bucket and asked the operator to reload something no reload could
// fill. The host says which is which — it has said so all along, in the `code`
// field of its error envelope — so this reads the code rather than guessing
// from the status.

import { ApiError } from "@/api/types";

/**
 * How a section should read a failed read.
 *
 * - `"unavailable"` — the surface is not on this host at all: a 404, or the
 *   host's own `not_in_build`. A fact about the binary; the section may hide.
 * - `"unconfigured"` — the surface is here, but this company has not set it up
 *   (`not_configured`). Reloading never clears it; an operator action
 *   elsewhere does, and the host's message names that control.
 * - `"error"` — anything else (5xx, offline, an expired session, a body that
 *   wasn't the shape the route promises): the current state is UNKNOWN, which
 *   is not the same as absent. The section must stay on the page and say so.
 */
export type SectionLoad = "unavailable" | "unconfigured" | "error";

/**
 * The host codes that mean a permanent capability state rather than a failure.
 *
 * Keyed on the structured `code` ONLY, never the prose and never the bare
 * status. `409` cannot carry this decision: the same status answers a lost
 * publish race, an optimistic-lock version skew and a duplicate desk id, all of
 * which a caller clears by retrying or by sending something else. Reading 409
 * as permanent would tell an operator that a workflow they can still save is
 * gone. `run-error.ts` states the same rule for the run-failure codes.
 */
const PERMANENT: Record<string, SectionLoad> = {
  not_in_build: "unavailable",
  not_configured: "unconfigured",
};

/**
 * Splits a genuine "not served here" from "the host could not answer".
 *
 * A 404 stays `"unavailable"` on status alone: it predates the code vocabulary,
 * and a host old enough to answer 404 for an absent route is exactly the host
 * that sends no useful code.
 */
export function classifyLoadFailure(err: unknown): SectionLoad {
  if (!(err instanceof ApiError)) return "error";
  if (err.status === 404) return "unavailable";
  // `fromHost` is the guard that keeps a proxy from voting. A gateway that
  // synthesises a status has no idea whether the capability exists, and a
  // synthesised code that happened to collide would retire a section the host
  // still serves.
  return (err.fromHost && PERMANENT[err.code]) || "error";
}
