// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { ComposioCredentialSource, ComposioStatus } from "@/api/composio";
import { IntegrationStep } from "@/onboarding/IntegrationStep";

/**
 * Codex review, PR #2046. `src/company/activation.rs` derives
 * `integrationConnected` from whether an active Composio CONNECTION exists —
 * not from whether a CREDENTIAL exists. Before this fix, `IntegrationStep`
 * had no way to tell those apart: it always said "this company needs a
 * credential" and offered to waive the step, even for a hosted founder who
 * already has an `attested`/`company`/`static` credential and simply hasn't
 * connected a provider yet — an ordinary, always-completable action, not the
 * "no lever at all" case the waiver exists for.
 */

function status(credentialSource: ComposioCredentialSource): ComposioStatus {
  return {
    inBuild: true,
    granted: true,
    credentialSource,
    backendUrl: "",
    toolkits: [],
    openMode: true,
    effectiveToolkits: [],
    effectiveCatalog: [],
    catalogSource: "backend",
    catalogNotice: null,
  };
}

function fakeClient(credentialSource: ComposioCredentialSource | "hang"): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/company",
    get: (path: string) => {
      if (path.includes("/composio")) {
        if (credentialSource === "hang") return new Promise(() => {});
        return Promise.resolve(status(credentialSource));
      }
      throw new Error(`unexpected path: ${path}`);
    },
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function render(credentialSource: ComposioCredentialSource | "hang") {
  await act(async () => {
    root.render(
      createElement(IntegrationStep, {
        client: fakeClient(credentialSource),
        company: null,
        onOpenApps: () => {},
        onWaive: () => {},
      }),
    );
    await Promise.resolve();
    await Promise.resolve();
  });
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

describe("IntegrationStep distinguishes a missing connection from a missing credential", () => {
  it("keeps the original copy and waiver when there is genuinely no credential", async () => {
    await render("none");
    expect(container.querySelector('[data-testid="gate-integration-has-credential"]')).toBeNull();
    expect(container.textContent).toContain("needs a credential to connect it with");
    expect(container.querySelector('[data-testid="gate-integration-waive"]')).toBeTruthy();
    expect(container.querySelector('[data-testid="gate-integration-open-apps"]')?.textContent).toContain(
      "Enter a credential",
    );
  });

  it("does not claim a credential is missing when one is attested", async () => {
    await render("attested");
    expect(container.querySelector('[data-testid="gate-integration-has-credential"]')).toBeTruthy();
    expect(container.textContent).not.toContain("needs a credential to connect it with");
  });

  it("does not offer to waive a step that a credential makes completable", async () => {
    await render("company");
    expect(
      container.querySelector('[data-testid="gate-integration-waive"]'),
      "waiving is for builds with no credential lever at all, not this case",
    ).toBeNull();
    expect(container.querySelector('[data-testid="gate-integration-open-apps"]')?.textContent).toContain(
      "Connect a provider",
    );
  });

  it("also recognizes a static/BYOK credential", async () => {
    await render("static");
    expect(container.querySelector('[data-testid="gate-integration-has-credential"]')).toBeTruthy();
  });

  it("defaults to the safe no-credential copy while the read is still in flight", async () => {
    await render("hang");
    expect(container.querySelector('[data-testid="gate-integration-has-credential"]')).toBeNull();
  });

  it("withholds the durable waiver until the credential read actually settles", async () => {
    // Codex review, PR #2046: `hasCredential` starts `false` so the COPY is
    // safe by default, but the waive button used to key off that same flag —
    // so it was visible for the entire loading window too. A click there
    // could permanently mark the step skipped before a slow read went on to
    // report a credential DID exist. The waive button (and its footer) now
    // also require `credentialConfirmed`, which only a settled SUCCESSFUL
    // read sets.
    await render("hang");
    expect(
      container.querySelector('[data-testid="gate-integration-waive"]'),
      "the waiver must stay hidden while the read is still pending",
    ).toBeNull();
    expect(
      container.textContent,
      "the footer explaining the (not-yet-offered) waiver must also stay hidden",
    ).not.toContain("Skipping is remembered");
  });

  it("withholds the durable waiver when the credential read fails outright", async () => {
    const client = {
      scopeFor: () => "/api/v1/company",
      get: (path: string) => {
        if (path.includes("/composio")) return Promise.reject(new Error("network blip"));
        throw new Error(`unexpected path: ${path}`);
      },
    } as unknown as OpenCompanyClient;
    await act(async () => {
      root.render(
        createElement(IntegrationStep, { client, company: null, onOpenApps: () => {}, onWaive: () => {} }),
      );
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(
      container.querySelector('[data-testid="gate-integration-waive"]'),
      "a failed read is not a confirmed 'no credential' — the waiver must stay withheld",
    ).toBeNull();
  });
});
