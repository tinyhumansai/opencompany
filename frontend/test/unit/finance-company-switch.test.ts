// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { InvoicingView } from "@/views/finance/InvoicingView";

/**
 * A provider's credentials must not survive a company switch.
 *
 * This suite is normally for pure functions. The exception is earned the same
 * way it was when this file tested `SettingsSection`: the behaviour is not a
 * function anybody can call, it is *composition* — a parent giving a
 * credential-holding page a `key` that changes with the company. A unit test of
 * either alone would pass with the key removed, which is exactly the regression
 * that happened once: clearing fields by hand covered the ones somebody
 * remembered and left the API key, webhook secret and both PayPal halves
 * behind, so an operator who typed a key, switched company, and pressed Save
 * wrote that credential into the wrong company's secret store.
 *
 * The forms moved from Settings → Billing to Finance → Invoicing / Wallet
 * (docs/spec/runtime/finance-console.md), and `FinanceSection` carries the same
 * `key={company ?? "self"}`. What is driven here is the page itself, keyed the
 * way the section keys it — `FinanceSection` lazily imports the Recharts-backed
 * Overview, which this runner has no reason to stand up.
 */

/** A client that answers the Chargebee status read and fails the data read. */
function clientFor(company: string): OpenCompanyClient {
  return {
    scopeFor: () => `/api/v1/companies/${company}`,
    get: async (path: string) => {
      if (path.endsWith("/billing/chargebee"))
        return {
          apiKeyConfigured: false,
          site: null,
          webhookConfigured: false,
          webhookUrl: null,
          granted: true,
          inBuild: true,
        };
      // Unconfigured, so there are no invoices to list. The page must still
      // render the form — that is the point of the state being a 409.
      throw Object.assign(new Error("not configured"), { code: "not_configured" });
    },
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

/** Renders the page exactly as `FinanceSection` does: keyed by company. */
async function showInvoicing(company: string) {
  await act(async () => {
    root.render(
      createElement(ConnectionScopeProvider, {
        scope: { connection: "local", company },
        children: createElement(InvoicingView, {
          key: company,
          client: clientFor(company),
          company,
        }),
      }),
    );
  });
}

/**
 * Opens the credential form.
 *
 * An unconfigured company now opens on `ChargebeePitch` — the case for
 * migrating — with the form one click behind "I already have a Chargebee site",
 * so every test below has to make that click before it has a field to type in.
 * The switch behaviour this suite is actually about is unchanged; only the way
 * in is, and a helper is where that belongs so a later redesign of the entry
 * point touches one line rather than three tests.
 */
async function openCredentialForm() {
  const already = container.querySelector<HTMLButtonElement>(
    '[data-testid="chargebee-already-have"]',
  );
  // Absent on a company that is already connected — there the panel is the
  // page's own surface and there is nothing to open.
  if (!already) return;
  await act(async () => {
    already.click();
  });
}

function apiKeyBox(): HTMLInputElement {
  const box = container.querySelector<HTMLInputElement>('[data-testid="billing-api-key"]');
  if (!box) throw new Error("the API key input is not on the page");
  return box;
}

/** Types into the field the way an operator does, so React's state updates. */
async function type(box: HTMLInputElement, value: string) {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLInputElement.prototype,
      "value",
    )?.set;
    setter?.call(box, value);
    box.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  window.localStorage.clear();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("invoicing credentials across a company switch", () => {
  it("offers the pitch first on an unconfigured company, with the form one click behind it", async () => {
    // The precondition every assertion below rests on, and a behaviour in its
    // own right. It used to be "the panel arrives expanded rather than making
    // the operator find it" — right while this page opened on a credential
    // form, and wrong once it opens on the case for migrating: a panel that
    // expanded itself under the pitch would put two competing starting points
    // on one screen and pre-empt the button that is the pitch's own way in.
    // What must stay true is that the form is never more than that one click
    // away for somebody who already has a site and came here to paste a key.
    await showInvoicing("acme");
    expect(container.querySelector('[data-testid="chargebee-pitch"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="billing-api-key"]')).toBeNull();

    await openCredentialForm();
    expect(apiKeyBox()).not.toBeNull();
  });

  it("drops a typed-but-unsaved credential when the company changes", async () => {
    await showInvoicing("acme");
    await openCredentialForm();
    await type(apiKeyBox(), "cb_live_for_acme");
    expect(apiKeyBox().value).toBe("cb_live_for_acme");

    // The operator switches company without saving.
    await showInvoicing("globex");
    await openCredentialForm();

    // The key must NOT still be sitting in the box, where the next Save would
    // send it to globex.
    expect(apiKeyBox().value).toBe("");
  });

  it("drops it again on a switch back, not just the first time", async () => {
    // A `key` that only changed once — or a clear that ran on mount only —
    // would pass the test above and fail this one.
    await showInvoicing("acme");
    await openCredentialForm();
    await type(apiKeyBox(), "cb_live_for_acme");
    await showInvoicing("globex");
    await openCredentialForm();
    await type(apiKeyBox(), "cb_live_for_globex");
    expect(apiKeyBox().value).toBe("cb_live_for_globex");

    await showInvoicing("acme");
    await openCredentialForm();
    expect(apiKeyBox().value).toBe("");
  });
});
