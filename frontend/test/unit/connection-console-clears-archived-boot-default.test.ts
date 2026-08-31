// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import type { AppSpec, CompanyStatus } from "@/api/types";
import { addConnection, getConnection, resetConnections } from "@/connections/registry";
import { HostsProvider } from "@/connections/HostsContext";
import type { ConnectionId } from "@/connections/types";

/**
 * Codex review on #1828 (PR comment 3864885215): when a reset on an
 * explicit-company connection archives successfully, provisioning then
 * fails, and the operator cancels out of the dialog, `ConnectionConsole`'s
 * `onClose` used to only refresh the in-memory roster (`backToPicker`). The
 * connection's persisted `defaultCompany` and any `?company=` link still
 * named the now-archived id. The next reload's boot effect takes the
 * explicit-company branch (`ConnectionConsole.tsx`'s boot `useEffect`)
 * straight into `client.status(archivedId)`, which the host answers
 * `company_not_found`, landing the operator on a connection error instead of
 * back in their own console.
 *
 * The fix clears both persisted sources — `clearDefaultCompany` and
 * `retargetCompanyUrlParam(archivedId, null)` — before refreshing the
 * roster, so a reload falls through to the ordinary multi-company/no-company
 * boot path instead of retrying a dead id.
 *
 * `AppShell` is stubbed to a `Reset` trigger wired to the real
 * `onResetCompany` callback — same reasoning as
 * `connection-console-create-retargets-url.test.ts` for staying out of a
 * full mount.
 *
 * Follow-up (codex review on #1828, PR comment 3865563560): the URL clear
 * this pins is now gated on `ConnectionConsole`'s `isBootstrap` prop — see
 * `connection-console-create-retargets-url.test.ts` for the restored,
 * non-bootstrap counter-case. `show()` below passes `isBootstrap: true`
 * because the connection it renders is scoped to the same company the page
 * lands on (`?company=acme`), i.e. it IS the bootstrap connection.
 */

vi.mock("@/components/app-shell", () => ({
  AppShell: (props: {
    company: string | null;
    initialStatus?: CompanyStatus;
    onResetCompany: (id: string, name: string) => void;
  }) =>
    createElement(
      "div",
      {
        "data-testid": "console-phase",
        "data-company": props.company ?? "",
        "data-status-id": props.initialStatus?.id ?? "",
      },
      createElement(
        "button",
        {
          "data-testid": "app-shell-reset",
          onClick: () => props.onResetCompany(props.company!, props.initialStatus?.name ?? ""),
        },
        "Reset",
      ),
    ),
}));

const { ConnectionConsole } = await import("@/views/ConnectionConsole");

function spec(over: Partial<AppSpec> = {}): AppSpec {
  return {
    name: "opencompany",
    version: "0.0.0",
    api_url: "",
    cycles_available: false,
    setup_complete: true,
    ...over,
  } as AppSpec;
}

function company(id: string, name: string): CompanyStatus {
  return { id, name, lifecycle: "running", pending_approvals: 0 };
}

function stubClient(acme: CompanyStatus) {
  const lifecycle = vi.fn(() => Promise.resolve());
  const provisionCompany = vi.fn(() =>
    Promise.reject(new ApiError(500, "internal_error", "internal error", true)),
  );
  const listCompanies = vi.fn(() => Promise.resolve([] as CompanyStatus[]));
  const client = {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    spec: async () => spec(),
    listCompanies,
    status: vi.fn((id: string | null) =>
      id === acme.id ? Promise.resolve(acme) : Promise.reject(new Error(`unexpected status(${id ?? "null"})`)),
    ),
    lifecycle,
    provisionCompany,
  } as unknown as OpenCompanyClient;
  return { client, lifecycle, provisionCompany, listCompanies };
}

let container: HTMLDivElement;
let root: Root;

function land(search: string): void {
  window.history.replaceState({}, "", `/${search}`);
}

async function show(connectionId: ConnectionId, client: OpenCompanyClient) {
  await act(async () => {
    root.render(
      createElement(HostsProvider, {
        value: {
          connections: [],
          selected: null,
          onSelect: () => {},
          onAdd: () => {},
          onEditHost: () => {},
          onRemoveHost: () => {},
          localInstances: [],
          hub: false,
        },
        children: createElement(ConnectionConsole, {
          connectionId,
          client,
          defaultCompany: "acme",
          // This connection's `defaultCompany` matches the `?company=acme`
          // the page lands on in `beforeEach` below — it is the bootstrap
          // connection, so the abandon-path URL clear this test pins is
          // expected to fire (issue #1828 comment 3865563560; see
          // `ConnectionConsole`'s `isBootstrap` prop).
          isBootstrap: true,
        }),
      }),
    );
  });
}

async function settle() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

async function fillAdminEmail() {
  const input = document.querySelector<HTMLInputElement>("#create-company-admin");
  expect(input, "no admin-email field").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  await act(async () => {
    setter.call(input, "ceo@acme.test");
    input!.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  resetConnections();
  window.localStorage.clear();
  land("?company=acme");
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  resetConnections();
  window.localStorage.clear();
  land("");
});

describe("cancelling a reset after the archive landed but the create failed", () => {
  it("clears the persisted default company and the ?company= URL instead of leaving them stale", async () => {
    const acme = company("acme", "Acme Robotics");
    const connectionId = addConnection({ baseUrl: "https://acme.test", defaultCompany: "acme" });
    const { client, lifecycle, provisionCompany, listCompanies } = stubClient(acme);

    await show(connectionId, client);
    await settle();

    // Explicit company wins at boot — straight into "acme"'s console.
    let phase = container.querySelector('[data-testid="console-phase"]');
    expect(phase, "console phase never rendered").toBeTruthy();
    expect(phase!.getAttribute("data-company")).toBe("acme");
    expect(window.location.search).toBe("?company=acme");
    expect(getConnection(connectionId)?.defaultCompany).toBe("acme");

    const resetTrigger = container.querySelector<HTMLButtonElement>(
      '[data-testid="app-shell-reset"]',
    );
    expect(resetTrigger, "Reset trigger not found").toBeTruthy();
    await act(async () => {
      resetTrigger!.click();
    });

    await fillAdminEmail();

    const submit = Array.from(
      document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] button'),
    ).find((b) => b.textContent?.trim().startsWith("Archive & start clean"));
    expect(submit, 'no "Archive & start clean" button found').toBeTruthy();
    await act(async () => {
      submit!.click();
    });
    await settle();

    // The archive landed; the create failed. The dialog is back to idle with
    // an error shown — the starting point this bug requires.
    expect(lifecycle).toHaveBeenCalledWith("archive", "acme");
    expect(provisionCompany).toHaveBeenCalledTimes(1);
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeTruthy();

    const cancel = Array.from(
      document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] button'),
    ).find((b) => b.textContent?.trim() === "Cancel");
    expect(cancel, 'no "Cancel" button found').toBeTruthy();
    await act(async () => {
      cancel!.click();
    });
    await settle();

    // The bug this pins: the old code refreshed only the in-memory roster
    // (`listCompanies`) and left the persisted profile and the URL still
    // naming "acme" — a reload would have retried the now-archived id.
    expect(getConnection(connectionId)?.defaultCompany).toBeNull();
    expect(window.location.search).toBe("?company=");
    expect(listCompanies).toHaveBeenCalledTimes(1);

    // The roster came back empty (the only company was just archived), so
    // `backToPicker` landed the console on the (empty) picker rather than a
    // connection error or a stale console still showing "acme".
    phase = container.querySelector('[data-testid="console-phase"]');
    expect(phase, "console phase should no longer be rendered").toBeFalsy();
    expect(container.querySelector('[data-testid="connection-error"]')).toBeNull();
    expect(container.querySelector('[data-testid="picker-new-company"]')).toBeTruthy();
  });
});
