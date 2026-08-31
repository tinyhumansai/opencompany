// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { CompanyStatus } from "@/api/types";
import type { Transport, TransportRequest, TransportResponse } from "@/api/transport";
import { HostsProvider } from "@/connections/HostsContext";
import { addConnection, clientFor, resetConnections, useConnection } from "@/connections/registry";
import type { ConnectionId } from "@/connections/types";

/**
 * Codex review on #1828, PR comment 3865401542: an explicit-company
 * connection's reset (or plain create) calls `retargetDefaultCompany`
 * (`onCompanyCreated`), which `reseat`s the connection — a brand new
 * `OpenCompanyClient` and a new `defaultCompany`. `App` reads both off the
 * registry and passes them straight through as `ConnectionConsole`'s props,
 * so that reseat is a prop change on `client` and `defaultCompany` — exactly
 * the boot effect's dependency array. The effect re-runs and, since
 * `defaultCompany` is now set, takes the "explicit company wins" path
 * straight into a *second* `client.status(id)` call for the company
 * `switchCompany` already entered with a known-good `CompanyStatus`
 * (`connection-console-switch-known-status.test.ts` fixed the *first*
 * redundant call `switchCompany` itself made; this is a second, independent
 * one the reseat-driven reboot makes on top of that fix). A transient
 * failure on this reboot call — nothing needed it — replaces a fully
 * succeeded reset with the generic connection-error screen.
 *
 * Driven through the real registry (`addConnection` / `retargetDefaultCompany`
 * via `reseat`) rather than manually-supplied props, because the bug is in
 * how a *prop change* re-enters the boot effect — a harness that hands
 * `ConnectionConsole` static props would never reproduce it. Only
 * `AppShell` is stubbed, for the same reason `connection-console-switch-known-
 * status.test.ts` stubs it: reaching "console" (and staying there) is the
 * proof, and the real component pulls in a workspace/chat/presence surface
 * this fix has nothing to do with.
 */

vi.mock("@/components/app-shell", () => ({
  AppShell: (props: {
    company: string | null;
    initialStatus?: CompanyStatus;
    onResetCompany: (id: string, name: string) => void;
  }) =>
    createElement(
      "div",
      { "data-testid": "console-phase", "data-company": props.company ?? "" },
      createElement(
        "button",
        {
          "data-testid": "trigger-reset",
          onClick: () => props.onResetCompany(props.company!, props.initialStatus?.name ?? ""),
        },
        "Reset",
      ),
    ),
}));

const { ConnectionConsole } = await import("@/views/ConnectionConsole");

type Handler = (req: TransportRequest) => TransportResponse | null;

/**
 * A transport backed by a handler list rather than a single big switch, so
 * each test can read intent off the list instead of a nested conditional.
 * Rejects (transport-level, not an HTTP status) when nothing matches —
 * `OpenCompanyClient.request` turns that into `network_error`, the same
 * ambiguity a dropped connection produces for real.
 */
function fakeTransport(handlers: Handler[], onRequest?: (req: TransportRequest) => void): Transport {
  return {
    request: vi.fn(async (req: TransportRequest): Promise<TransportResponse> => {
      onRequest?.(req);
      for (const h of handlers) {
        const res = h(req);
        if (res) return res;
      }
      throw new Error(`unhandled request: ${req.method} ${req.url}`);
    }),
    stream: vi.fn(() => () => {}),
  } as unknown as Transport;
}

function json(status: number, body: unknown): TransportResponse {
  return {
    status,
    statusText: "",
    url: "",
    text: JSON.stringify(body),
    header: () => null,
  };
}

function path(req: TransportRequest): string {
  return new URL(req.url).pathname;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  resetConnections();
  window.localStorage.clear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  resetConnections();
  window.localStorage.clear();
});

async function settle() {
  await act(async () => {
    for (let i = 0; i < 8; i += 1) await Promise.resolve();
  });
}

describe("an explicit-company connection resetting its company", () => {
  it("does not re-fetch the freshly-reset company's status after the reseat-driven reboot", async () => {
    const oldStatus: CompanyStatus = {
      id: "acme",
      name: "Acme Robotics",
      lifecycle: "running",
      pending_approvals: 0,
    } as CompanyStatus;

    let redundantStatusCalls = 0;

    const handlers: Handler[] = [
      (req) =>
        req.method === "GET" && path(req) === "/spec"
          ? json(200, {
              name: "opencompany",
              version: "0.0.0",
              api_url: "",
              cycles_available: false,
              setup_complete: true,
            })
          : null,
      // The create/reset dialog's auth-mode preflight, read on open.
      (req) =>
        req.method === "GET" && path(req) === "/api/v1/companies/provisioning"
          ? json(200, { auth_mode: "email", wallets_required: false })
          : null,
      // The initial boot lookup for the connection's own default company —
      // legitimate, happens once before anything else.
      (req) =>
        req.method === "GET" && path(req) === "/api/v1/companies/acme" ? json(200, oldStatus) : null,
      (req) =>
        req.method === "POST" && path(req) === "/api/v1/companies/acme/archive"
          ? json(200, oldStatus)
          : null,
      (req) => {
        if (req.method !== "POST" || path(req) !== "/api/v1/companies") return null;
        const sent = JSON.parse(req.body ?? "{}") as { id?: string };
        const id = sent.id ?? "acme-replacement";
        return json(201, {
          id,
          name: "Acme Robotics",
          lifecycle: "running",
          pending_approvals: 0,
        });
      },
      // Any OTHER GET to a scoped company (i.e. the freshly reset id) is the
      // reboot's redundant lookup — `switchCompany` already has this status
      // via `onCompanyCreated`'s `knownStatus`, so nothing should ever ask
      // for it again. Always fails, the same way a dropped connection would.
      (req) => {
        if (req.method !== "GET") return null;
        const p = path(req);
        if (p.startsWith("/api/v1/companies/") && p !== "/api/v1/companies/acme") {
          redundantStatusCalls += 1;
          throw new Error("transient network failure");
        }
        return null;
      },
    ];

    const transport = fakeTransport(handlers);
    const id: ConnectionId = addConnection({
      baseUrl: "https://acme.test",
      defaultCompany: "acme",
      credential: { kind: "platform", token: "test-platform-token" },
      transport,
    });

    function Harness() {
      const connection = useConnection(id);
      const client = clientFor(id);
      if (!connection || !client) return null;
      return createElement(ConnectionConsole, {
        connectionId: id,
        client,
        defaultCompany: connection.defaultCompany,
      });
    }

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
          children: createElement(Harness),
        }),
      );
    });
    await settle();

    // Boots straight into the console for the explicit default company.
    expect(container.querySelector('[data-testid="console-phase"]')?.getAttribute("data-company")).toBe(
      "acme",
    );

    const resetTrigger = container.querySelector<HTMLButtonElement>('[data-testid="trigger-reset"]');
    expect(resetTrigger, "reset trigger not found").toBeTruthy();
    await act(async () => {
      resetTrigger!.click();
    });

    // The dialog renders through a Radix portal into `document.body`.
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    const adminInput = document.querySelector<HTMLInputElement>("#create-company-admin");
    expect(adminInput, "create-company-admin field not found").toBeTruthy();
    await act(async () => {
      setValue.call(adminInput, "ceo@acme.test");
      adminInput!.dispatchEvent(new Event("input", { bubbles: true }));
    });

    const submit = Array.from(
      document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] button'),
    ).find((b) => b.textContent?.trim().startsWith("Archive & start clean"));
    expect(submit, 'no "Archive & start clean" button found').toBeTruthy();
    await act(async () => {
      submit!.click();
    });
    await settle();
    // Reseat is asynchronous relative to the click (registry emit → new
    // props → effect re-run); give the boot effect's re-entry a chance to
    // fully resolve (or, pre-fix, to fail) before asserting.
    await settle();

    // Never the generic connection-error screen — a transient failure on a
    // lookup nothing needed should not surface at all.
    expect(container.querySelector('[data-testid="connection-error"]')).toBeNull();

    const phase = container.querySelector('[data-testid="console-phase"]');
    expect(phase, "console phase never rendered after reset").toBeTruthy();
    expect(phase!.getAttribute("data-company")).not.toBe("acme");
    expect(phase!.getAttribute("data-company")).not.toBe("");

    // The heart of the regression: the reseat-driven reboot must not re-ask
    // for a status `switchCompany` already holds.
    expect(redundantStatusCalls).toBe(0);
  });
});
