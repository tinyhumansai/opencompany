// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { AppSpec, CompanyStatus } from "@/api/types";
import { HostsProvider } from "@/connections/HostsContext";

/**
 * These exercise the create/reset flow itself, so they need the build where the
 * product offers it.
 *
 * `canCreateCompanies` is `!COMPANY_SWITCHING_HIDDEN && carriesPlatformBearer`
 * (b8a3e2e97), and `COMPANY_SWITCHING_HIDDEN` ships `true` — so in the shipped
 * tree every trigger below is hidden and the dialog's own preflight never runs.
 * Left unmocked, these files would assert against controls the product
 * deliberately does not render, which is what broke them: they would be pinning
 * the flag rather than the flow.
 *
 * The *hide* is not weakened by this. It has its own coverage, deliberately
 * unmocked, in `product-scope-hidden-surfaces.test.ts` ("company creation is
 * gone from every trigger, not just the switcher"), which is where a regression
 * in the gate belongs. What is left here is the flow underneath it — including
 * the #1894 pre-archive guard, whose whole job is to keep a reset from
 * archiving a company before it knows the replacement has a usable admin. That
 * logic is still in the tree and still worth failing a build over the day the
 * flag flips back.
 */
vi.mock("@/product-scope", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/product-scope")>()),
  COMPANY_SWITCHING_HIDDEN: false,
}));


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

/**
 * Retired with the trigger it drove.
 *
 * It covered the landing after a create: the operator ends up inside the
 * fresh company, and the console does not re-fetch a status the create call
 * already returned — the point being that a follow-up lookup which fails must
 * not strand them outside a company that exists.
 *
 * It reached that through the no-company screen's "New company" button, and
 * no company-creation trigger renders while the product does not offer it
 * (`src/product-scope.ts`). The picker's own button is gated the same way, so
 * there is no second way in to re-point this at.
 *
 * Deliberately not re-pointed at the handler directly: driving
 * `onCompanyCreated` below the trigger would keep this green while the shipped
 * path renders nothing to reach it. The handler is untouched; turning the flag
 * off restores the trigger and this case.
 */

describe("the no-company screen while the product offers no company creation", () => {
  it("shows no way to create one", async () => {
    // What replaces the retired case above: the trigger it drove is the thing
    // under test now, and its absence is asserted where the case used to press
    // it. A platform-scoped client is used deliberately — the caller *could*
    // create a company, and it is product scope that withholds the control.
    const { client } = stubClient(company("co-fresh1", "Fresh Co"));

    await show(client);
    await settle();

    expect(container.querySelector('[data-testid="no-company-new"]')).toBeNull();
  });
});
