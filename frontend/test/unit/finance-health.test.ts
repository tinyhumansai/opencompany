import { describe, expect, it } from "vitest";

import type { BillingStatus, PaypalStatus } from "@/api/billing";
import { chargebeeHealth, paypalHealth, startsExpanded } from "@/views/finance/health";

/**
 * The four-state precedence behind each provider panel's collapsed line.
 *
 * This is the surviving half of `billing-view-branches.test.ts`, which drove the
 * retired `BillingView` through jsdom to assert which of four alert cards it
 * rendered. The alerts became one line and a badge when the form moved into a
 * collapsible panel (docs/spec/runtime/finance-console.md), and the decision
 * behind them moved into `health.ts` — a pure function, testable without a
 * document, and testable at every combination rather than the handful a render
 * test could afford.
 *
 * The property is unchanged and is the reason the module exists: a panel that
 * shows one line has to pick which problem to report, and it must pick the
 * WORST. "Connected ✓" over a missing manifest grant is the green tick that
 * sends an operator hunting through a form for a problem that is not in it.
 *
 * One case that used to live here is gone because it can no longer happen: a
 * PayPal read failing used to blank the Chargebee form, because the two loaded
 * together under `Promise.all` on one page. They are now two pages with two
 * loads, so there is no shared failure to guard against — see
 * `finance-invoicing.test.ts` for the surviving version of that concern.
 */

const CHARGEBEE_OK: BillingStatus = {
  apiKeyConfigured: true,
  site: "acme-test",
  webhookConfigured: true,
  webhookUrl: "https://oc.example/hooks/acme/chargebee",
  granted: true,
  inBuild: true,
};

const PAYPAL_OK: PaypalStatus = {
  clientIdConfigured: true,
  clientSecretConfigured: true,
  environment: "sandbox",
  granted: true,
  inBuild: true,
};

describe("chargebeeHealth", () => {
  it("reports connected, naming the site", () => {
    const health = chargebeeHealth(CHARGEBEE_OK);
    expect(health.state).toBe("connected");
    // The site is in the label because "Connected" against the WRONG site is
    // the confusion this surface exists to remove.
    expect(health.label).toContain("acme-test");
    expect(health.remedy).toBeNull();
  });

  it("names the grant, not the form, when the company does not grant chargebee", () => {
    // Both credentials stored and still nothing reaches an agent. Saying "not
    // connected" would send the operator back through a form already correct.
    const health = chargebeeHealth({ ...CHARGEBEE_OK, granted: false });
    expect(health.state).toBe("not_granted");
    expect(health.remedy).toContain("chargebee");
    // Issue #1796: the remedy used to end in "it cannot be fixed from this
    // page" and stop. It now names the namespace the panel offers to grant, so
    // the panel can render the control that ends the dead end.
    expect(health.grantNamespace).toBe("chargebee");
    // The credential form still is not the fix, so the panel stays collapsed.
    expect(startsExpanded(health)).toBe(false);
  });

  it("does not claim a connection the company never made (issue #1796)", () => {
    // The reported bug, exactly: a company with no Chargebee credential at all
    // fell into the not-granted arm, which asserts "Connected" and interpolates
    // the site — rendering, on a live tenant, "Connected to null — but no
    // teammate can use it". Two claims in one line, both false.
    const health = chargebeeHealth({
      ...CHARGEBEE_OK,
      granted: false,
      apiKeyConfigured: false,
      site: null,
    });
    expect(health.state).toBe("not_configured");
    expect(health.label).toBe("Not connected");
    expect(health.label).not.toContain("null");
    expect(health.label).not.toContain("Connected to");
    // And the form IS the fix now, so the panel opens itself on arrival.
    expect(startsExpanded(health)).toBe(true);
  });

  it("still reports the missing grant once a credential exists", () => {
    // The half of the precedence that must survive the fix above: a company
    // that HAS connected and lacks the grant must not be told to re-enter a
    // credential it already stored.
    const configuredButUngranted = chargebeeHealth({ ...CHARGEBEE_OK, granted: false });
    expect(configuredButUngranted.state).toBe("not_granted");
    // A half credential is not one, so it reports the form, not the grant.
    expect(
      chargebeeHealth({ ...CHARGEBEE_OK, granted: false, site: null }).state,
    ).toBe("not_configured");
    expect(
      chargebeeHealth({ ...CHARGEBEE_OK, granted: false, apiKeyConfigured: false }).state,
    ).toBe("not_configured");
  });

  it("puts not-in-build above not-granted", () => {
    // Granting `chargebee` in the manifest fixes nothing on a host compiled
    // without it, so reporting the grant would hand over the wrong remedy.
    const health = chargebeeHealth({ ...CHARGEBEE_OK, granted: false, inBuild: false });
    expect(health.state).toBe("not_in_build");
    // And no grant control either: the grant would succeed and change nothing,
    // because this host has no billing tools to hand out (issue #1796).
    expect(health.grantNamespace).toBeUndefined();
  });

  it("never reports connected without BOTH the key and the site", () => {
    // A key with no site produces requests against no host, and a site with no
    // key produces unauthenticated ones. Either alone is not a connection.
    expect(chargebeeHealth({ ...CHARGEBEE_OK, site: null }).state).toBe("not_configured");
    expect(chargebeeHealth({ ...CHARGEBEE_OK, apiKeyConfigured: false }).state).toBe(
      "not_configured",
    );
  });

  it("opens the panel only when its own form is the fix", () => {
    // Unconfigured is the one state this form addresses, so it is the one that
    // earns an expanded panel on arrival.
    expect(startsExpanded(chargebeeHealth({ ...CHARGEBEE_OK, apiKeyConfigured: false }))).toBe(
      true,
    );
    expect(startsExpanded(chargebeeHealth(CHARGEBEE_OK))).toBe(false);
    expect(startsExpanded(chargebeeHealth({ ...CHARGEBEE_OK, inBuild: false }))).toBe(false);
  });

  it("mentions a missing webhook without calling the connection broken", () => {
    // Invoicing works fine without it — nobody is just told when a customer
    // pays. That is a note on a working connection, not a failure.
    const health = chargebeeHealth({ ...CHARGEBEE_OK, webhookConfigured: false });
    expect(health.state).toBe("connected");
    expect(health.remedy).toContain("webhook");
  });
});

describe("paypalHealth", () => {
  it("names the environment when connected", () => {
    // Reading a sandbox balance and believing it is real money is the failure
    // the environment exists in the label to prevent.
    expect(paypalHealth(PAYPAL_OK).label).toContain("sandbox");
    expect(paypalHealth({ ...PAYPAL_OK, environment: "live" }).label).toContain("live");
  });

  it("never reports connected with only half a credential", () => {
    expect(paypalHealth({ ...PAYPAL_OK, clientIdConfigured: false }).state).toBe("not_configured");
    expect(paypalHealth({ ...PAYPAL_OK, clientSecretConfigured: false }).state).toBe(
      "not_configured",
    );
  });

  it("applies the same precedence as chargebee", () => {
    expect(paypalHealth({ ...PAYPAL_OK, granted: false }).state).toBe("not_granted");
    expect(paypalHealth({ ...PAYPAL_OK, granted: false, inBuild: false }).state).toBe(
      "not_in_build",
    );
  });

  it("offers the grant, and only where it would help", () => {
    // Issue #1796: the Wallet panel said "Add `paypal` to [tools].allow" and
    // stopped, which on a hosted tenant named a file the operator cannot edit.
    expect(paypalHealth({ ...PAYPAL_OK, granted: false }).grantNamespace).toBe("paypal");
    expect(paypalHealth(PAYPAL_OK).grantNamespace).toBeUndefined();
    expect(
      paypalHealth({ ...PAYPAL_OK, granted: false, inBuild: false }).grantNamespace,
    ).toBeUndefined();
  });

  it("does not claim a connection the company never made (issue #1796)", () => {
    // The Wallet half of the reported bug. Chargebee's showed the `null`;
    // PayPal's said a bare "Connected — but no teammate can use it" over a
    // company that had never entered a client id, which is the same false claim
    // with nothing in it to give the falsehood away.
    const health = paypalHealth({
      ...PAYPAL_OK,
      granted: false,
      clientIdConfigured: false,
      clientSecretConfigured: false,
    });
    expect(health.state).toBe("not_configured");
    expect(health.label).toBe("Not connected");
    expect(startsExpanded(health)).toBe(true);
  });
});
