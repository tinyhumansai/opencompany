// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { ComposioCredentialSource, ComposioStatus } from "@/api/composio";
import { IntegrationStep } from "@/onboarding/IntegrationStep";
import { COMPOSIO_MANAGED_HIDDEN } from "@/product-scope";

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

  it("retries a failed credential read instead of withholding the waiver forever", async () => {
    // Codex review, PR #2046: the rejection handler was a permanent no-op, so
    // one transient failure left `credentialConfirmed` false — and with it the
    // durable waiver, the only escape a credential-less founder has — withheld
    // for the whole life of the mount, long after the outage ended.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      let calls = 0;
      const client = {
        scopeFor: () => "/api/v1/company",
        get: (path: string) => {
          if (!path.includes("/composio")) throw new Error(`unexpected path: ${path}`);
          calls += 1;
          if (calls === 1) return Promise.reject(new Error("network blip"));
          return Promise.resolve(status("none"));
        },
      } as unknown as OpenCompanyClient;

      await act(async () => {
        root.render(
          createElement(IntegrationStep, {
            client,
            company: null,
            onOpenApps: () => {},
            onWaive: () => {},
          }),
        );
        await vi.advanceTimersByTimeAsync(0);
      });

      expect(
        container.querySelector('[data-testid="gate-integration-waive"]'),
        "a failed read is not a confirmed 'no credential' — the waiver stays withheld for now",
      ).toBeNull();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(3000);
      });

      expect(calls, "the failed read must be retried").toBeGreaterThan(1);
      expect(
        container.querySelector('[data-testid="gate-integration-waive"]'),
        "once a read succeeds the escape must become available",
      ).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps reading after a success, so a credential cleared elsewhere is noticed", async () => {
    // Codex review, PR #2046, round 4: the mount read only ever scheduled
    // another read from its REJECTION handler, so the first successful read
    // was the last one this card did — `client` and `company` never change.
    // A credential cleared in another tab therefore left the card asserting
    // one exists, which hides the waive button (`!hasCredential`) and so puts
    // `waive`'s own click-time re-read out of reach, while activation still
    // reports `integrationConnected: false`. Neither finish nor skip, until a
    // reload.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      let source: ComposioCredentialSource = "company";
      let calls = 0;
      const client = {
        scopeFor: () => "/api/v1/company",
        get: (path: string) => {
          if (!path.includes("/composio")) throw new Error(`unexpected path: ${path}`);
          calls += 1;
          return Promise.resolve(status(source));
        },
      } as unknown as OpenCompanyClient;

      await act(async () => {
        root.render(
          createElement(IntegrationStep, {
            client,
            company: null,
            onOpenApps: () => {},
            onWaive: () => {},
          }),
        );
        await vi.advanceTimersByTimeAsync(0);
      });

      expect(container.querySelector('[data-testid="gate-integration-has-credential"]')).toBeTruthy();
      expect(container.querySelector('[data-testid="gate-integration-waive"]')).toBeNull();

      // Another tab clears the company's last credential, with no provider
      // ever connected. Nothing tells this card; only its own next read can.
      source = "none";
      const readsBefore = calls;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5000);
      });

      expect(calls, "a successful read must not be the last read of the mount").toBeGreaterThan(
        readsBefore,
      );
      expect(
        container.querySelector('[data-testid="gate-integration-has-credential"]'),
        "the card must stop claiming a credential the company no longer has",
      ).toBeNull();
      expect(
        container.querySelector('[data-testid="gate-integration-waive"]'),
        "and the escape hidden behind that claim must come back",
      ).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("re-reads on returning to the tab rather than waiting out a poll tick", async () => {
    // The finding's scenario is literally a second tab, so the moment the
    // answer is known to be stale is the moment this one comes back to the
    // front. `startVisiblePolling` reads on that hidden -> visible edge (and
    // stops the timer while hidden, issue #581), which is why the poll goes
    // through it rather than through a bare timer chain.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let visibility: DocumentVisibilityState = "visible";
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => visibility,
    });
    try {
      let source: ComposioCredentialSource = "company";
      const client = {
        scopeFor: () => "/api/v1/company",
        get: (path: string) => {
          if (!path.includes("/composio")) throw new Error(`unexpected path: ${path}`);
          return Promise.resolve(status(source));
        },
      } as unknown as OpenCompanyClient;

      await act(async () => {
        root.render(
          createElement(IntegrationStep, {
            client,
            company: null,
            onOpenApps: () => {},
            onWaive: () => {},
          }),
        );
        await vi.advanceTimersByTimeAsync(0);
      });
      expect(container.querySelector('[data-testid="gate-integration-has-credential"]')).toBeTruthy();

      source = "none";
      await act(async () => {
        visibility = "hidden";
        document.dispatchEvent(new Event("visibilitychange"));
        visibility = "visible";
        document.dispatchEvent(new Event("visibilitychange"));
        await vi.advanceTimersByTimeAsync(0);
      });

      expect(
        container.querySelector('[data-testid="gate-integration-waive"]'),
        "coming back to the tab must not need a full poll tick to tell the truth",
      ).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("names every credential route the Apps page it links to will actually take", async () => {
    // This sentence is the first-run instruction, and its whole job is to send
    // the founder after a credential the page it links to accepts. While
    // `COMPOSIO_MANAGED_HIDDEN` was set, that was a Composio API key and
    // nothing else — naming a TinyHumans account key sent them after one the
    // Apps page had no surface for.
    //
    // The flag is off. The managed route is a selectable row again and the
    // company-credential card is on the page above it, so the account key is
    // now the ONE-CLICK way to finish this step and must be named first. The
    // branch is kept rather than collapsed to the current value, because it is
    // what makes re-hiding the route the single edit `product-scope.ts`
    // promises — and reading the flag here is what pins the copy to it.
    await render("none");
    const copy = container.textContent ?? "";
    expect(copy).toContain("needs a credential to connect it with");
    if (COMPOSIO_MANAGED_HIDDEN) {
      expect(copy, "the managed route is hidden — do not offer it").not.toContain(
        "TinyHumans account key",
      );
      expect(copy).toContain("Composio API key of your own");
    } else {
      // Both, and in this order: the one-click credential, then the escape
      // hatch for a company that wants its own Composio account. A first run
      // that only heard about the second is the errand the grant removed.
      expect(copy, "the managed route is offered — name its credential").toContain(
        "TinyHumans account key",
      );
      expect(copy).toContain("Composio token of your own");
      expect(copy.indexOf("TinyHumans account key")).toBeLessThan(
        copy.indexOf("Composio token of your own"),
      );
    }
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
