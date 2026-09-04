// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { AppShell } from "@/components/app-shell";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import type { ConnectionId, LocalScope } from "@/connections/types";
import { markGateStepWaived, waivedGateSteps } from "@/onboarding/state";

/**
 * CodeRabbit review, PR #2046.
 *
 * `useActivationGate` resets its `status` to `null` for a newly-selected
 * company only from its OWN effect — which fires in the same commit as
 * `AppShell`'s waiver-cleanup effect below but is not guaranteed to run
 * first, and even when it does the reset does not take effect until the
 * NEXT render. So the very first commit after switching companies can still
 * pair the FORMER company's `isActivated: true` with the NEW `scope`, and
 * the waiver-cleanup effect — gated only on
 * `[activationGate.status?.isActivated, scope]` — would read that
 * combination and durably wipe the newly-selected company's own waiver
 * before that company's own activation read has ever landed.
 *
 * This is not reachable from `onboarding-gate-waiver.test.ts` (a pure
 * `localStorage` roundtrip) or `onboarding-gate-logic.test.ts` (pure
 * predicates) — the race is in which company's `activationGate.status`
 * `AppShell`'s effect reads on the commit right after a company switch, which
 * only rendering `AppShell` itself through that switch can observe.
 */

const CONNECTION = "conn-1" as ConnectionId;
const SCOPE_A: LocalScope = { connection: CONNECTION, company: "acme-a" };
const SCOPE_B: LocalScope = { connection: CONNECTION, company: "acme-b" };

const STATUS_A: CompanyStatus = {
  id: "acme-a",
  name: "Acme A",
  lifecycle: "running",
  pending_approvals: 0,
};

const STATUS_B: CompanyStatus = {
  id: "acme-b",
  name: "Acme B",
  lifecycle: "running",
  pending_approvals: 0,
};

/** A promise that never settles — reads this test does not care about. */
function hang(): Promise<never> {
  return new Promise<never>(() => {});
}

/**
 * A minimal `OpenCompanyClient` double that answers company A's `/activation`
 * read immediately as activated, and leaves every other read (including
 * company B's `/activation`) hanging — the shape of a real switch, where the
 * newly-selected company's own funnel read has not landed yet.
 */
function buildClient(): OpenCompanyClient {
  const known = {
    baseUrl: "",
    scopeFor: (company: string | null) => `/api/v1/companies/${company ?? ""}`,
    subscribeToEvents: () => () => {},
    get: (path: string) => {
      if (path === "/api/v1/companies/acme-a/activation") {
        return Promise.resolve({
          nameConfirmed: true,
          integrationConnected: true,
          workflowRunSucceeded: true,
          isActivated: true,
        });
      }
      return hang();
    },
    status: hang,
    approvals: hang,
    listDesks: hang,
    listTeam: hang,
  };
  return new Proxy(known, {
    get(target, prop, receiver) {
      if (prop in target) return Reflect.get(target, prop, receiver);
      return hang;
    },
  }) as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  window.matchMedia = ((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
  window.location.hash = "#/overview";
  localStorage.clear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  localStorage.clear();
});

describe("AppShell's waiver cleanup does not cross a company switch", () => {
  it("preserves the newly-selected company's waiver when switching away from an activated one", async () => {
    markGateStepWaived(SCOPE_B, "integration");
    expect(waivedGateSteps(SCOPE_B)).toEqual(["integration"]);

    const client = buildClient();

    await act(async () => {
      root.render(
        createElement(ConnectionScopeProvider, {
          scope: SCOPE_A,
          children: createElement(AppShell, {
            client,
            company: STATUS_A.id,
            initialStatus: STATUS_A,
            companies: [STATUS_A, STATUS_B],
            onSwitchCompany: () => {},
          }),
        }),
      );
      // Flush the microtasks `getActivation`'s promise chain needs to land.
      await Promise.resolve();
      await Promise.resolve();
    });

    // Switch to company B — the commit under test. `activationGate`'s own
    // reset effect for B has not resolved anything yet (its `/activation`
    // read hangs by design above), so on this commit `AppShell` still holds
    // company A's `isActivated: true` in `activationGate.status`.
    await act(async () => {
      root.render(
        createElement(ConnectionScopeProvider, {
          scope: SCOPE_B,
          children: createElement(AppShell, {
            client,
            company: STATUS_B.id,
            initialStatus: STATUS_B,
            companies: [STATUS_A, STATUS_B],
            onSwitchCompany: () => {},
          }),
        }),
      );
    });

    expect(
      waivedGateSteps(SCOPE_B),
      "switching to company B must not wipe the waiver B already had, just because A was activated",
    ).toEqual(["integration"]);
  });
});
