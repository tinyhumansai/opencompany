// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { InvoicingView } from "@/views/finance/InvoicingView";
import { resolveFinancePage } from "@/views/finance/FinanceSection";

/**
 * What Invoicing does when the data read fails — which, on a fresh company, is
 * always.
 *
 * The credential status and the invoice list are separate reads and the list
 * *cannot* succeed before the credential exists. A page that treated the list's
 * failure as fatal would therefore hide the very form that fixes it on exactly
 * the company that needs it most. This is the surviving form of the concern the
 * retired `BillingView` suite tested as "one provider failing must not blank the
 * other".
 *
 * The three host states are also distinguished here rather than collapsed into
 * one error, because `src/server/ops/finance.rs` goes to the trouble of keeping
 * them apart and the remedies differ: fill in this form / get a different build
 * / go and fix something at Chargebee.
 */

const STATUS = {
  apiKeyConfigured: true,
  site: "acme-test",
  webhookConfigured: true,
  webhookUrl: "https://oc.example/hooks/acme/chargebee",
  granted: true,
  inBuild: true,
};

function clientRejectingListWith(error: unknown): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get: async (path: string) => {
      if (path.endsWith("/billing/chargebee")) return STATUS;
      throw error;
    },
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(InvoicingView, { client, company: "acme" }));
  });
}

function at(testid: string): HTMLElement | null {
  return container.querySelector<HTMLElement>(`[data-testid="${testid}"]`);
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("Invoicing when the invoice list cannot be read", () => {
  it("keeps the connection panel on the page", async () => {
    await show(clientRejectingListWith(new ApiError(409, "not_configured", "no credentials", true)));
    // The whole page is still there, and so is the panel that fixes it.
    expect(at("invoicing-view")).not.toBeNull();
    expect(at("chargebee-panel")).not.toBeNull();
    expect(at("invoicing-status-error")).toBeNull();
  });

  it("tells an unconfigured company to connect, not that something broke", async () => {
    await show(clientRejectingListWith(new ApiError(409, "not_configured", "no credentials", true)));
    expect(at("invoice-list-error")?.textContent).toContain("Connect Chargebee");
  });

  it("tells a featureless host that no build change is coming from this page", async () => {
    await show(
      clientRejectingListWith(new ApiError(501, "not_in_build", "no chargebee support", true)),
    );
    expect(at("invoice-list-error")?.textContent).toContain("without Chargebee support");
  });

  it("passes a provider refusal through in Chargebee's own words", async () => {
    // The provider's message names the setting to change. Paraphrasing it into
    // "could not load invoices" throws away the only actionable part.
    await show(
      clientRejectingListWith(
        new ApiError(502, "provider_error", "Currency GBP is not enabled for this site", true),
      ),
    );
    expect(at("invoice-list-error")?.textContent).toContain("GBP");
  });
});

describe("resolveFinancePage", () => {
  it("lands on Overview for anything that does not name a sub-page", () => {
    // Overview is the ledger fold, which has something to show on a host where
    // no provider is connected — so the section is never an empty shell.
    for (const sub of [null, "", "nonsense", "billing"]) {
      expect(resolveFinancePage(sub)).toBe("overview");
    }
  });

  it("resolves the two provider pages by name", () => {
    expect(resolveFinancePage("invoicing")).toBe("invoicing");
    expect(resolveFinancePage("wallet")).toBe("wallet");
  });
});

/**
 * The Grant control on the Chargebee panel (issue #1796).
 *
 * `PUT …/tools/grants` is admin-only, and both finance pages always supply
 * `onGrant`, so the panel — not the page — is where the viewer's role has to be
 * honoured. Offering a member a button whose only possible outcome is a 403
 * toast replaces the "cannot be fixed from this page" dead end with a subtler
 * one, which is the opposite of what this change is for.
 */
describe("the Chargebee panel's grant control", () => {
  /** Configured, but the company does not grant `chargebee`. */
  const UNGRANTED = { ...STATUS, granted: false };

  function client(role: "admin" | "member"): OpenCompanyClient {
    return {
      scopeFor: () => "/api/v1/companies/acme",
      get: async (path: string) => {
        if (path.endsWith("/auth/me")) {
          return { id: "u1", email: "a@b.c", role, company: "acme", hasPassword: true };
        }
        if (path.endsWith("/billing/chargebee")) return UNGRANTED;
        throw new ApiError(404, "not_found", "no invoices");
      },
    } as unknown as OpenCompanyClient;
  }

  it("offers an admin the grant, next to the remedy", async () => {
    await show(client("admin"));

    const remedy = at("chargebee-remedy");
    expect(remedy).not.toBeNull();
    // The dead-end sentence is gone from the product.
    expect(remedy?.textContent).not.toContain("cannot be fixed from this page");
    const grant = at("chargebee-grant");
    expect(grant).not.toBeNull();
    expect(grant?.textContent).toContain("Grant chargebee");
  });

  it("withholds the control from a member, and keeps the remedy", async () => {
    await show(client("member"));

    // The member still needs to know why invoicing reaches no teammate.
    expect(at("chargebee-remedy")).not.toBeNull();
    expect(at("chargebee-grant")).toBeNull();
  });

  it("offers nothing on a host built without Chargebee", async () => {
    // The grant would succeed and change nothing: this build has no billing
    // tools to hand out, so the remedy is a different binary.
    const notInBuild = {
      scopeFor: () => "/api/v1/companies/acme",
      get: async (path: string) => {
        if (path.endsWith("/auth/me")) {
          return { id: "u1", email: "a@b.c", role: "admin", company: "acme", hasPassword: true };
        }
        if (path.endsWith("/billing/chargebee")) return { ...UNGRANTED, inBuild: false };
        throw new ApiError(404, "not_found", "no invoices");
      },
    } as unknown as OpenCompanyClient;
    await show(notInBuild);

    expect(at("chargebee-remedy")).not.toBeNull();
    expect(at("chargebee-grant")).toBeNull();
  });
});
