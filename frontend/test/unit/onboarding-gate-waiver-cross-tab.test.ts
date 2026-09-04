// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { AppShell } from "@/components/app-shell";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { HostsProvider, type HostsValue } from "@/connections/HostsContext";
import type { Connection, ConnectionId, LocalScope } from "@/connections/types";
import { clearGateStepWaivers, markGateStepWaived } from "@/onboarding/state";

/**
 * Codex review, PR #2046.
 *
 * `AppShell`'s `gateWaived` state only re-read `localStorage` when `scope`
 * changed — never on an update written by a DIFFERENT tab already open on
 * the same company. A waiver is durably scoped and meant to survive a fresh
 * tab (`markGateStepWaived`'s own doc), but a tab that was already open when
 * another tab waived the last outstanding step kept the gate up regardless,
 * until reloaded or the founder repeated the waive in that tab too.
 *
 * The fix listens for the native `storage` event, which the browser already
 * fires in every OTHER same-origin tab (never the one that wrote) — this
 * test dispatches that event by hand to stand in for the other tab.
 */

const CONNECTION_ID = "conn-1" as ConnectionId;
const SCOPE: LocalScope = { connection: CONNECTION_ID, company: "acme" };

const STATUS: CompanyStatus = {
  id: "acme",
  name: "Acme",
  lifecycle: "running",
  pending_approvals: 0,
};

function hang(): Promise<never> {
  return new Promise<never>(() => {});
}

/**
 * name and workflow are already done; only `integration` is outstanding —
 * the one step this test waives from "another tab".
 */
function buildClient(): OpenCompanyClient {
  const known = {
    baseUrl: "",
    scopeFor: (company: string | null) => `/api/v1/companies/${company ?? ""}`,
    subscribeToEvents: () => () => {},
    get: (path: string) => {
      if (path.endsWith("/auth/me")) return Promise.resolve({ role: "admin" });
      if (path.endsWith("/activation")) {
        return Promise.resolve({
          nameConfirmed: true,
          integrationConnected: false,
          workflowRunSucceeded: true,
          isActivated: false,
        });
      }
      return hang();
    },
    status: hang,
    approvals: hang,
    listDesks: hang,
    // Must resolve, not hang: `shouldHoldShellPending` also gates on
    // `SetupController`'s own `setupChecked`, which only settles once this
    // read lands (see the round-13/14 findings in
    // `onboarding-gate-setup-controller-mount.test.ts`).
    listTeam: async () => [
      { id: "operations", role: "Analyst", inboxEnabled: false, global: true },
      { id: "ada", role: "Operations", inboxEnabled: false },
    ],
  };
  return new Proxy(known, {
    get(target, prop, receiver) {
      if (prop in target) return Reflect.get(target, prop, receiver);
      return hang;
    },
  }) as unknown as OpenCompanyClient;
}

const CONNECTION: Connection = {
  id: CONNECTION_ID,
  defaultCompany: null,
  label: "test",
  baseUrl: "",
  credential: { kind: "cookie" },
  status: "live",
  identity: null,
  companies: [],
  connector: { kind: "remote" },
};

const HOSTS: HostsValue = {
  connections: [CONNECTION],
  selected: CONNECTION_ID,
  onSelect: () => {},
  onAdd: () => {},
  localInstances: [],
  onEditHost: () => {},
  onRemoveHost: () => {},
  hub: false,
};

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

describe("AppShell notices a waiver written by another tab", () => {
  it("closes the gate on a storage event, without a reload or a local waive click", async () => {
    const client = buildClient();

    await act(async () => {
      root.render(
        createElement(HostsProvider, {
          value: HOSTS,
          children: createElement(ConnectionScopeProvider, {
            scope: SCOPE,
            children: createElement(AppShell, {
              client,
              company: STATUS.id,
              initialStatus: STATUS,
              companies: [STATUS],
              onSwitchCompany: () => {},
            }),
          }),
        }),
      );
      // The activation and admin reads route through `withReadTimeout`'s own
      // `Promise.race` on top of the client's plain `Promise.resolve`, so a
      // couple of bare microtask ticks is not always enough to settle both
      // and let the resulting re-render commit — flush generously.
      for (let i = 0; i < 20; i++) await Promise.resolve();
    });

    // Precondition: the gate is up (integration outstanding), not the shell.
    expect(
      container.querySelector("#main-content"),
      "the gate must be showing before the cross-tab waiver lands",
    ).toBeNull();
    expect(container.querySelector('[data-testid="gate-integration-step"]')).toBeTruthy();

    // Another tab durably waives the last outstanding step — written directly
    // to storage, exactly as `markGateStepWaived` does, WITHOUT going through
    // this tab's own `waiveGateStep`.
    await act(async () => {
      markGateStepWaived(SCOPE, "integration");
      window.dispatchEvent(new StorageEvent("storage", { key: "irrelevant-to-the-browser" }));
      await Promise.resolve();
    });

    expect(
      container.querySelector("#main-content"),
      "the gate must close once the storage event is noticed, with no reload",
    ).toBeTruthy();
  });

  /**
   * Codex review, PR #2046, round 2.
   *
   * `clearGateStepWaivers` fires from ANOTHER tab too, the moment THAT tab's
   * own poll confirms `isActivated` — and every `removeItem` it makes is a
   * deletion `storage` event here. This tab's `/activation` read never
   * settles `isActivated: true` (stands in for an outage, or simply this
   * tab's own poll not having caught up yet), so applying the removal
   * immediately would reopen the gate over a step the founder already
   * answered. The fix must defer the removal — leave this tab's own waiver
   * state untouched — until this tab's OWN activation read independently
   * confirms it, rather than trusting the other tab's word.
   */
  it("does not reopen the gate on a removal-type storage event while this tab's own activation is still stale", async () => {
    const client = buildClient();

    await act(async () => {
      root.render(
        createElement(HostsProvider, {
          value: HOSTS,
          children: createElement(ConnectionScopeProvider, {
            scope: SCOPE,
            children: createElement(AppShell, {
              client,
              company: STATUS.id,
              initialStatus: STATUS,
              companies: [STATUS],
              onSwitchCompany: () => {},
            }),
          }),
        }),
      );
      for (let i = 0; i < 20; i++) await Promise.resolve();
    });

    // Precondition: the gate is up (integration outstanding).
    expect(container.querySelector("#main-content")).toBeNull();

    // This tab waives the outstanding step (standing in for either a local
    // click or another tab's addition, already covered above) so the gate
    // closes.
    await act(async () => {
      markGateStepWaived(SCOPE, "integration");
      window.dispatchEvent(new StorageEvent("storage", { key: "irrelevant-to-the-browser" }));
      await Promise.resolve();
    });
    expect(
      container.querySelector("#main-content"),
      "the gate must be closed before the removal arrives, or this test proves nothing",
    ).toBeTruthy();

    // Another tab's OWN poll confirms activation and clears every waiver —
    // but this tab's `/activation` mock keeps answering `isActivated: false`
    // forever, standing in for an outage or a poll that has not caught up.
    // The resulting deletion `storage` event must not reopen the gate.
    await act(async () => {
      clearGateStepWaivers(SCOPE);
      window.dispatchEvent(new StorageEvent("storage", { key: "irrelevant-to-the-browser" }));
      for (let i = 0; i < 20; i++) await Promise.resolve();
    });

    expect(
      container.querySelector("#main-content"),
      "a removal must not reopen the gate before this tab's own activation independently confirms it",
    ).toBeTruthy();
  });
});
