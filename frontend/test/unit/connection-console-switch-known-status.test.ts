// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { AppSpec, CompanyStatus } from "@/api/types";
import { HostsProvider } from "@/connections/HostsContext";

/**
 * Codex review on #1828 (PR comment 3864628314): `switchCompany`'s only
 * caller that already holds fresh data for the company it is entering —
 * `onCompanyCreated`, right after a create or reset provisions the
 * replacement — still re-fetched it with a second, redundant
 * `client.status(id)` call. On a reset the old company is already archived
 * by that point, so a transient failure on *this* second lookup alone
 * dropped a fully-succeeded create to a connection error, and could just as
 * easily undo a successful ambiguous-provision reconciliation
 * (`create-company-dialog.tsx`, PR comment 3863028397) by failing its own
 * follow-up lookup right after.
 *
 * `switchCompany` now takes an optional `knownStatus` and only falls back to
 * `client.status` when the caller doesn't already hold one; `onCompanyCreated`
 * passes the status the create/reset call already returned.
 *
 * `AppShell` is stubbed out: reaching "console" *is* the proof here (a
 * redundant, failing second lookup used to keep the console from ever
 * getting there), but the real component pulls in workspace/chat/presence
 * hooks with their own heavy client surface this fix has nothing to do
 * with — same reasoning `connection-console-no-company.test.ts` and
 * `create-company-reset-cancel-reports-archived.test.ts` give for staying
 * out of a full mount.
 */

vi.mock("@/components/app-shell", () => ({
  AppShell: (props: { company: string | null; initialStatus?: CompanyStatus }) =>
    createElement("div", {
      "data-testid": "console-phase",
      "data-company": props.company ?? "",
      "data-status-id": props.initialStatus?.id ?? "",
    }),
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

function stubClient(provisioned: CompanyStatus) {
  // A transient failure on the *follow-up* lookup — exactly the shape PR
  // comment 3864628314 describes: the create/reset itself fully succeeded,
  // only the redundant re-fetch after it failed.
  const statusSpy = vi.fn((id: string | null) =>
    Promise.reject(new Error(`transient status failure for ${id ?? "null"}`)),
  );
  const client = {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    spec: async () => spec(),
    listCompanies: async () => [],
    status: statusSpy,
    provisionCompany: vi.fn(() => Promise.resolve(provisioned)),
    lifecycle: vi.fn(() => Promise.resolve()),
  } as unknown as OpenCompanyClient;
  return { client, statusSpy };
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient) {
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
          connectionId: "test-connection",
          client,
          defaultCompany: null,
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

describe("a create provisioning a company whose follow-up status lookup would fail", () => {
  it("still lands the operator in the fresh company, without re-fetching what the create already returned", async () => {
    const provisioned = company("co-fresh1", "Fresh Co");
    const { client, statusSpy } = stubClient(provisioned);

    await show(client);
    await settle();

    // A configured host with zero companies lands on "no-company" — the
    // lightest phase with a reachable "New company" trigger.
    const trigger = container.querySelector<HTMLButtonElement>('[data-testid="no-company-new"]');
    expect(trigger, "no-company-new trigger not found").toBeTruthy();
    await act(async () => {
      trigger!.click();
    });

    // The dialog renders through a Radix portal into `document.body`, not
    // into `container` — matching `create-company-reset-fresh-id.test.ts`'s
    // `[data-slot="dialog-content"]` queries.
    const nameInput = document.querySelector<HTMLInputElement>("#create-company-name");
    expect(nameInput, "create-company-name field not found").toBeTruthy();
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    await act(async () => {
      setValue.call(nameInput, "Fresh Co");
      nameInput!.dispatchEvent(new Event("input", { bubbles: true }));
    });

    // Required after codex review comment 3864885200 — unrelated to what
    // this file pins (skipping the redundant status lookup), so the submit
    // needs one filled in first or validation blocks it before provisioning.
    const adminInput = document.querySelector<HTMLInputElement>("#create-company-admin");
    expect(adminInput, "create-company-admin field not found").toBeTruthy();
    await act(async () => {
      setValue.call(adminInput, "ceo@fresh.test");
      adminInput!.dispatchEvent(new Event("input", { bubbles: true }));
    });

    const submit = Array.from(
      document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] button'),
    ).find((b) => b.textContent?.trim() === "Create company");
    expect(submit, 'no "Create company" button found').toBeTruthy();
    await act(async () => {
      submit!.click();
    });
    await settle();

    // Never the generic connection-error screen — a transient failure on a
    // lookup nothing needed should not surface at all.
    expect(container.querySelector('[data-testid="connection-error"]')).toBeNull();

    const phase = container.querySelector('[data-testid="console-phase"]');
    expect(phase, "console phase never rendered").toBeTruthy();
    expect(phase!.getAttribute("data-company")).toBe("co-fresh1");
    // Populated from the status the create call already returned, not a
    // second fetch — which is exactly what the assertion below proves.
    expect(phase!.getAttribute("data-status-id")).toBe("co-fresh1");
    expect(statusSpy).not.toHaveBeenCalled();
  });
});
