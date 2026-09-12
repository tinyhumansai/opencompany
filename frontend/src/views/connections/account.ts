// The decisions the Account page makes, none of which needs React, a host or a
// browser to exercise.
//
// The organising rule is the inference rework's (`docs/modules/inference/
// architecture.md`): what the page *decides* — which tier answers, whether
// there is a key of this company's own to remove, what the balance line says —
// lives here with a unit test each, and what is left in the component is
// layout. A decision written inline in JSX can only be checked by mounting the
// page, and the ones below are exactly the ones that have been got wrong.

import type { CompanyBilling, CompanyCredentialStatus } from "@/api/credential";

/** The name the one row carries, and the two letters its mark is drawn from. */
export const ACCOUNT_LABEL = "TinyHumans";

/**
 * How much the page knows about the account right now.
 *
 * `error` is the host refusing to answer — a secret store it could not read —
 * and it is deliberately a value of its own rather than folded into "nothing is
 * set". See {@link accountShape}.
 */
export type AccountLoad = "loading" | "ready" | "error";

/**
 * What the account row is.
 *
 * Three, not two, and the third is the point. `company_key::resolve` propagates
 * a store read error rather than falling through to the instance identity,
 * because "we cannot read the store" and "no key is set" are different answers
 * that call for opposite actions — and a console that renders the first as the
 * second would tell an admin to set a key they have already set. The host went
 * to some trouble to keep them apart; the page has to spend a state on it.
 */
export type AccountShape = "unknown" | "empty" | "connected";

/**
 * Which of the three states the row is in.
 *
 * Keyed on `source`, which is what
 * [`resolve`](../../../../src/company/company_key.rs) returned — not on
 * `configured`, which is `key_configured` and answers the narrower question
 * "has this company pasted one". The two differ in exactly the case this page
 * exists to describe: a hosted tenant with no key of its own still has a
 * working identity, and a row built on `configured` would call that
 * "not configured" while the server's account quietly pays for every turn.
 */
export function accountShape(load: AccountLoad, status: CompanyCredentialStatus | null): AccountShape {
  if (load !== "ready" || status === null) return "unknown";
  return status.source === "none" ? "empty" : "connected";
}

/**
 * The one sub-line the account row shows: which tier actually answers.
 *
 * One fact, not three stacked — an operator is scanning for the row rather than
 * reading it. The two "working" states are kept apart because that is the
 * decision somebody is on this page to make, and a row that says only
 * "connected" hides which account is standing behind the company.
 *
 * **Identity, not a billing verdict.** These lines said "Billed to …", and
 * `source` cannot carry that: it comes from `company_key::resolve`, which
 * reports only which TinyHumans identity won. What an agent's *thinking* costs
 * is decided by `inference/config` and `inference/key`, which are set
 * independently on the LLM page — so a company with `source: "company"` can
 * think on its own OpenRouter key and be billed nothing here, and one on the
 * instance's identity can be paying a provider directly. Naming a payer from
 * this value would point an operator investigating spend at the wrong account,
 * which is the same overclaim this page's pass exists to remove, one row down.
 * The billing move is stated where it is conditional and true: on the header
 * card beside the Connect button, and in {@link REMOVAL_AND_THINKING}.
 */
export function accountSubline(load: AccountLoad, status: CompanyCredentialStatus | null): string {
  if (load !== "ready" || status === null) {
    return "The host could not say — this is not the same as having no key";
  }
  switch (status.source) {
    case "company":
      return "Acting as this company's own TinyHumans account";
    case "attested":
    case "static":
      return "Acting as the account of whoever runs this server";
    case "none":
      // Deliberately narrow. "Agents cannot think" is what this page used to
      // say here, and it is **false** on a company whose LLM page holds a key
      // of its own: that one outranks this credential in the managed chain, and
      // a provider of its own never consults it — so such a company thinks
      // perfectly well with no TinyHumans account at all. What is always true
      // is the absence itself.
      return "No TinyHumans account for this company";
    default:
      // An older or newer host naming a tier this build does not know. Saying
      // what the row *is* beats claiming a state nobody established.
      return "The account this company acts and spends through";
  }
}

/**
 * Whether there is a key of **this company's own** to take away.
 *
 * The instance's platform identity is not this row's to remove, and offering a
 * Remove that would clear nothing is the control-that-cannot-act the LLM page's
 * pass deleted a toggle over. Gated on the resolved tier rather than on
 * `configured` for the reason {@link accountShape} gives.
 */
export function canRemoveKey(status: CompanyCredentialStatus | null): boolean {
  return status?.source === "company";
}

/**
 * The single action the header card offers, or `null` for none.
 *
 * `connect` is the short path and wins wherever the host can complete a grant.
 * `key` is the paste dialog, which is the only route on a host with no hub
 * wired — so the slot holds whichever action is actually live rather than a
 * primary button that renders nothing and leaves a heading over empty space.
 */
export function headerAction(
  status: CompanyCredentialStatus | null,
  canManage: boolean,
): "connect" | "key" | null {
  if (!canManage) return null;
  // No status is not "no key". `refresh` drops `status` to `null` both while
  // the read is in flight and when it fails, and in the failed case the row
  // beneath this button is already saying the host could not answer. Offering
  // "Add a key" under that sentence is the control-that-cannot-act rule
  // pointing the other way: the write it opens is a blind overwrite of a
  // write-only credential the console has just admitted it cannot see, and the
  // value it replaces cannot be read back from anywhere. Wait for a known
  // state — {@link accountShape} spends one on this for the same reason.
  if (status === null) return null;
  return status.hubLink === true ? "connect" : "key";
}

/**
 * What removing this company's key actually does — first paragraph of the
 * confirmation.
 *
 * It offers **both** fallbacks rather than picking one, because the console
 * genuinely cannot tell which applies: `GET …/credential` reports the tier that
 * *won*, and while this company's own key is set that is always `company`,
 * whether or not an instance identity sits behind it.
 *
 * What it no longer claims is that the billing stops — that half is
 * {@link REMOVAL_AND_THINKING}'s, and it is not a flat "it stops" either.
 * `set_key("")` clears `tinyhumans/key` and nothing else; what happens to the
 * turns depends on what the managed chain finds next
 * (`src/server/ops/company_key.rs`, `src/company/inference.rs`).
 */
export const REMOVAL_CONSEQUENCE =
  "Apps connected as this company stop being reachable, and the identity the platform presents " +
  "on its behalf is gone. The company falls back to the identity of whoever runs this server, if " +
  "this instance carries one — and to no account at all if it does not.";

/**
 * What the removal costs the company's *thinking*, said before the press rather
 * than found when its agents stop answering.
 *
 * This sentence has been wrong in both directions, and #2266 is why. The grant
 * used to copy the key into `inference/key` as well, so removal here genuinely
 * did leave a second copy thinking on the same account. It no longer copies:
 * the key lands only in `tinyhumans/key`, and a managed turn *resolves* through
 * it — `provider/tinyhumans/key`, then the legacy `inference/key`, then this,
 * then the instance identity, then nothing. So removing it takes the rung a
 * managed company was standing on, and the honest sentence is the fallback,
 * not a reassurance.
 *
 * Still conditional at the top: a TinyHumans key pasted on the LLM page
 * outranks this one and goes on working, which is the state an operator most
 * needs to be able to tell from the others.
 */
export const REMOVAL_AND_THINKING =
  "Thinking goes with it where this company's models are set to TinyHumans: those turns resolve " +
  "through this same key, so removing it falls back to the identity of whoever runs this server " +
  "— and to nothing at all if this instance carries none. A TinyHumans key set on the LLM page " +
  "outranks this one and keeps working.";

/** The balance row, once there is an account of this company's own to ask about. */
export interface BalanceLine {
  /** The figure as drawn, or `null` when the hub would not answer. */
  amount: string | null;
  /** The one sub-line: the plan, or why there are no figures. */
  detail: string;
  /** Whether the figure should read as a warning rather than a fact. */
  low: boolean;
}

/**
 * What the balance row says, or `null` for no row at all.
 *
 * No row unless the company has an account of its own: `$0.00` under a company
 * that never set a key would be a made-up fact about a wallet that does not
 * exist. The host keys `configured` here off `load` rather than `resolve` for
 * the same reason — the instance's balance is not this company's to show.
 *
 * `unavailable` is **not** a zero balance. They look identical on a row and
 * mean opposite things, one "top up" and one "try again", so the figure is
 * dropped rather than invented.
 */
export function balanceLine(billing: CompanyBilling | null): BalanceLine | null {
  if (billing?.configured !== true) return null;

  if (billing.unavailable !== undefined) {
    return {
      amount: null,
      detail: `The key is set; the hub said: ${billing.unavailable}`,
      low: false,
    };
  }

  const summary = billing.summary;
  // Zero is a number worth showing, so the empty test is on `null` and never on
  // falsiness — `!balanceUsd` would hide exactly the figure somebody needs.
  const usd = typeof summary?.balanceUsd === "number" ? summary.balanceUsd : null;
  return {
    amount: usd === null ? "—" : `$${usd.toFixed(2)}`,
    detail: `on the ${summary?.plan ?? "free"} plan${
      summary?.activeSubscription ? " · subscription active" : ""
    }`,
    low: usd !== null && usd <= 0,
  };
}
