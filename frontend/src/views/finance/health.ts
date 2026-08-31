/**
 * The one-line verdict a connection panel shows when it is collapsed.
 *
 * Four things can each be wrong, they fail differently, and only two of them
 * are fixable from the form the panel holds:
 *
 *   - not in build   — the host was compiled without the feature. Nothing on
 *                      this page will ever help.
 *   - not granted    — the credentials are stored and STILL nothing reaches an
 *                      agent, because the company does not grant the tool.
 *   - not configured — no credential. This is the one the form is for.
 *   - connected      — working.
 *
 * `not_granted` is now fixable from here too (issue #1796): the grant used to
 * be reachable only by editing `company.toml` — impossible on a hosted tenant,
 * where the manifest is a read-only boot snapshot — so the panel said so and
 * stopped. `grantNamespace` names what the panel should offer to grant.
 *
 * # The precedence bug this file shipped with (issue #1796)
 *
 * `not_granted` outranking `not_configured` is right, but only for a company
 * that has actually connected. The guards used to test `!granted` **before**
 * asking whether any credential existed, so a company that had never touched
 * Chargebee fell into the `not_granted` arm — which asserts "Connected" and
 * interpolates the site into the label. With no site stored that rendered, on a
 * live tenant, as:
 *
 *     Chargebee · Connected to null — but no teammate can use it
 *
 * Two claims in one line, both false: nothing was connected, and `null` was not
 * an account. The definition above already said what the guard did not — "the
 * credentials are stored and STILL nothing reaches an agent" — so the fix is to
 * make the guard say it: a missing grant is only the worst problem once there
 * is something for it to be blocking.
 *
 * `BillingView` reported all four as separate alerts, which is right for a
 * settings page an operator reads top to bottom. A panel that collapses to one
 * line has to pick, so it picks **the worst**: "Connected ✓" over a missing
 * grant is exactly the green tick that sends someone hunting through a form for
 * a problem that is not in it.
 *
 * Pure, and separate from the components, so the precedence is testable without
 * rendering anything.
 */

import type { BillingStatus, PaypalStatus } from "@/api/billing";

/** A provider's overall state, worst-first. */
export interface Health {
  /** Ordered worst to best; the panel styles from this. */
  state: "not_in_build" | "not_granted" | "not_configured" | "connected";
  /** The collapsed line's summary, e.g. `Connected · acme-test`. */
  label: string;
  /** What to do about it, or `null` when there is nothing to do. */
  remedy: string | null;
  /** Whether the panel's own form can fix it. Drives whether it opens itself. */
  fixableHere: boolean;
  /**
   * The `[tools].allow` namespace this panel should offer to grant, when a
   * missing grant is what is wrong (issue #1796).
   *
   * `undefined` for every other state — including `not_in_build`, where the
   * grant would be real and still useless because the host has no such tools to
   * hand out. Offering a button there would trade one dead end for a subtler
   * one: a grant that succeeds and changes nothing.
   */
  grantNamespace?: string;
}

/** Chargebee's verdict. */
export function chargebeeHealth(status: BillingStatus): Health {
  if (!status.inBuild)
    return {
      state: "not_in_build",
      label: "Not supported by this host",
      remedy:
        "This host was built without Chargebee support. Credentials saved here will be stored and have no effect.",
      fixableHere: false,
    };
  // The configured-check comes first deliberately — see the precedence note at
  // the top. `status.site` is interpolated below, so reaching this arm without
  // one is what produced "Connected to null".
  if (status.apiKeyConfigured && status.site && !status.granted)
    return {
      state: "not_granted",
      label: `Connected to ${status.site} — but no teammate can use it`,
      remedy:
        "This company does not grant `chargebee`, so billing tools reach no teammate even with the key stored.",
      fixableHere: false,
      grantNamespace: "chargebee",
    };
  if (!status.apiKeyConfigured || !status.site)
    return {
      state: "not_configured",
      label: "Not connected",
      remedy: "Add the Chargebee site and API key to start raising invoices.",
      fixableHere: true,
    };
  return {
    state: "connected",
    label: `Connected · ${status.site}`,
    // Not an error, so not a remedy — but worth saying, because invoicing
    // works fine without it and the silence is the symptom.
    remedy: status.webhookConfigured
      ? null
      : "No webhook credential, so nobody is told when a customer pays. Invoicing itself is unaffected.",
    fixableHere: false,
  };
}

/** PayPal's verdict. */
export function paypalHealth(status: PaypalStatus): Health {
  if (!status.inBuild)
    return {
      state: "not_in_build",
      label: "Not supported by this host",
      remedy:
        "This host was built without PayPal support. Credentials saved here will be stored and have no effect.",
      fixableHere: false,
    };
  // Configured first, for the reason chargebeeHealth gives: an unconnected
  // company reporting "Connected — but no teammate can use it" is the same
  // false claim without the `null` to make it obvious.
  if (
    status.clientIdConfigured &&
    status.clientSecretConfigured &&
    !status.granted
  )
    return {
      state: "not_granted",
      label: "Connected — but no teammate can use it",
      remedy:
        "This company does not grant `paypal`, so wallet tools reach no teammate.",
      fixableHere: false,
      grantNamespace: "paypal",
    };
  if (!status.clientIdConfigured || !status.clientSecretConfigured)
    return {
      state: "not_configured",
      label: "Not connected",
      remedy: "Add the REST app client ID and secret to read the wallet.",
      fixableHere: true,
    };
  return {
    state: "connected",
    label: `Connected · ${status.environment}`,
    remedy: null,
    fixableHere: false,
  };
}

/**
 * Whether the panel should start expanded.
 *
 * Open when the operator can do something about it here, closed otherwise — a
 * working integration should not tax every visit with a credential form, and a
 * broken one that this form cannot fix should not pretend it can.
 */
export function startsExpanded(health: Health): boolean {
  return health.fixableHere;
}
