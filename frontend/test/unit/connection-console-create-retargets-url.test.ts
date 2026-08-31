// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { AppSpec, CompanyStatus } from "@/api/types";
import { addConnection, getConnection, resetConnections } from "@/connections/registry";
import { HostsProvider } from "@/connections/HostsContext";
import type { ConnectionId } from "@/connections/types";

/**
 * Codex review on #1828 (PR comment 3864628310): the `?company=` URL retarget
 * `onCompanyCreated` performs after a create/reset was gated on `archived`
 * (only ever set for a reset), but `retargetDefaultCompany` — the sibling fix
 * it runs beside — does not distinguish create from reset at all: it
 * retargets any company-scoped connection's persisted profile regardless.
 * A plain "New company" triggered from inside an explicit-company console
 * hits that same retarget, so the profile moved while the `?company=` link
 * did not. The next reload's bootstrap `addConnection` call then looks up
 * `findProfile(baseUrl, archivedId)` — the id the URL still names — which no
 * longer matches the retargeted profile, and mints a duplicate connection
 * still scoped to the company the operator just left.
 *
 * The fix reads `connection?.defaultCompany` *before* `retargetDefaultCompany`
 * overwrites it, and retargets the URL on that (`priorDefaultCompany`)
 * instead of on `archived` — covering a plain create the same way a reset
 * already was.
 *
 * `AppShell` is stubbed to a `New company` trigger wired to the real
 * `onCreateCompany` callback — same reasoning as
 * `connection-console-switch-known-status.test.ts` for staying out of a full
 * mount. `retargetCompanyUrlParam` itself (the primitive) already has
 * dedicated coverage in `connection-retarget-company-url-param.test.ts`; this
 * file is the wiring gap that left it — the one that previously only ever
 * called with `archived` — never called for a plain create.
 *
 * Follow-up (codex review on #1828, PR comment 3865563560): that fix over-
 * corrected. `retargetCompanyUrlParam`'s own guard only refuses to write when
 * the URL already names some OTHER company — an absent `?company=` param
 * passes it, so the retarget wrote one in even for a restored, non-bootstrap
 * connection whose `defaultCompany` came from `profileStore`, not from the
 * page's URL. `App`'s bootstrap connection is the only one the live URL
 * actually describes; `ConnectionConsole` now takes an `isBootstrap` prop
 * (`App`'s `active.id === bootstrapId`) and gates both `retargetCompanyUrlParam`
 * call sites on it. The suite below covers both directions: the "first" test
 * models the bootstrap connection (`isBootstrap: true`, matching the URL it
 * lands on) and still expects the retarget; the "second" models a restored
 * non-bootstrap profile (`isBootstrap: false`) with its own `defaultCompany`
 * and an empty URL, and expects the retarget to be skipped.
 */

vi.mock("@/components/app-shell", () => ({
  AppShell: (props: {
    company: string | null;
    initialStatus?: CompanyStatus;
    onCreateCompany: () => void;
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
        { "data-testid": "app-shell-new-company", onClick: props.onCreateCompany },
        "New company",
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

function stubClient(opts: { initial: CompanyStatus; provisioned: CompanyStatus }) {
  const provisionCompany = vi.fn(() => Promise.resolve(opts.provisioned));
  const client = {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    spec: async () => spec(),
    listCompanies: async () => [opts.initial],
    status: vi.fn((id: string | null) =>
      id === opts.initial.id
        ? Promise.resolve(opts.initial)
        : Promise.reject(new Error(`unexpected status(${id ?? "null"})`)),
    ),
    provisionCompany,
    lifecycle: vi.fn(() => Promise.resolve()),
  } as unknown as OpenCompanyClient;
  return { client, provisionCompany };
}

let container: HTMLDivElement;
let root: Root;

function land(search: string): void {
  window.history.replaceState({}, "", `/${search}`);
}

async function show(
  connectionId: ConnectionId,
  client: OpenCompanyClient,
  isBootstrap = true,
) {
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
          isBootstrap,
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

describe("a plain create from the bootstrap connection's explicit-company console", () => {
  it("retargets the ?company= URL param the same way a reset does", async () => {
    const acme = company("acme", "Acme Robotics");
    const beta = company("co-beta1", "Beta Co");
    const connectionId = addConnection({ baseUrl: "https://acme.test", defaultCompany: "acme" });
    const { client, provisionCompany } = stubClient({ initial: acme, provisioned: beta });

    await show(connectionId, client);
    await settle();

    // Explicit company wins at boot — straight into "acme"'s console, never
    // the picker. Confirms the starting point this bug requires.
    let phase = container.querySelector('[data-testid="console-phase"]');
    expect(phase, "console phase never rendered").toBeTruthy();
    expect(phase!.getAttribute("data-company")).toBe("acme");
    expect(window.location.search).toBe("?company=acme");

    const trigger = container.querySelector<HTMLButtonElement>(
      '[data-testid="app-shell-new-company"]',
    );
    expect(trigger, "New company trigger not found").toBeTruthy();
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
      setValue.call(nameInput, "Beta Co");
      nameInput!.dispatchEvent(new Event("input", { bubbles: true }));
    });

    // Required after codex review comment 3864885200 — unrelated to what
    // this file pins (the ?company= URL retarget), so the submit needs one
    // filled in first or validation blocks it before provisioning runs.
    const adminInput = document.querySelector<HTMLInputElement>("#create-company-admin");
    expect(adminInput, "create-company-admin field not found").toBeTruthy();
    await act(async () => {
      setValue.call(adminInput, "ceo@beta.test");
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

    expect(provisionCompany).toHaveBeenCalledTimes(1);

    // The bug this pins: the old code only rewrote the URL when `archived`
    // was set (reset-only). A plain create left it naming "acme" — the
    // company the operator just left — while the persisted profile below had
    // already moved on.
    expect(window.location.search).toBe("?company=co-beta1");
    expect(getConnection(connectionId)?.defaultCompany).toBe("co-beta1");

    // The console itself followed the create into the new company too.
    phase = container.querySelector('[data-testid="console-phase"]');
    expect(phase!.getAttribute("data-company")).toBe("co-beta1");
  });
});

describe("a plain create from a restored, non-bootstrap console", () => {
  it("does not rewrite the bootstrap connection's URL", async () => {
    // No `?company=` on the page at all — this models a second, restored
    // connection sitting beside whatever the bootstrap connection's own URL
    // describes (or nothing, in the single-connection default). Either way
    // the live URL does not belong to `connectionId` below.
    land("");

    const acme = company("acme", "Acme Robotics");
    const beta = company("co-beta1", "Beta Co");
    const connectionId = addConnection({ baseUrl: "https://acme.test", defaultCompany: "acme" });
    const { client, provisionCompany } = stubClient({ initial: acme, provisioned: beta });

    // `isBootstrap: false` is the point of this test — everything else
    // mirrors the bootstrap case above.
    await show(connectionId, client, false);
    await settle();

    let phase = container.querySelector('[data-testid="console-phase"]');
    expect(phase, "console phase never rendered").toBeTruthy();
    expect(phase!.getAttribute("data-company")).toBe("acme");
    // Pre-condition: `retargetCompanyUrlParam`'s own no-op guard only refuses
    // to write when the URL already names some OTHER company. An absent
    // `?company=` is not that, so without the `isBootstrap` gate this test
    // would still see the write happen below.
    expect(window.location.search).toBe("");

    const trigger = container.querySelector<HTMLButtonElement>(
      '[data-testid="app-shell-new-company"]',
    );
    expect(trigger, "New company trigger not found").toBeTruthy();
    await act(async () => {
      trigger!.click();
    });

    const nameInput = document.querySelector<HTMLInputElement>("#create-company-name");
    expect(nameInput, "create-company-name field not found").toBeTruthy();
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    await act(async () => {
      setValue.call(nameInput, "Beta Co");
      nameInput!.dispatchEvent(new Event("input", { bubbles: true }));
    });

    const adminInput = document.querySelector<HTMLInputElement>("#create-company-admin");
    expect(adminInput, "create-company-admin field not found").toBeTruthy();
    await act(async () => {
      setValue.call(adminInput, "ceo@beta.test");
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

    expect(provisionCompany).toHaveBeenCalledTimes(1);

    // The regression this pins (issue #1828 comment 3865563560): a restored,
    // non-bootstrap profile's create must NOT touch the live URL — only the
    // bootstrap connection's own `?company=` link is safe to rewrite.
    expect(window.location.search).toBe("");
    // The persisted profile still moves — `retargetDefaultCompany` is
    // ungated, and correctly so; only the URL write is bootstrap-only.
    expect(getConnection(connectionId)?.defaultCompany).toBe("co-beta1");

    phase = container.querySelector('[data-testid="console-phase"]');
    expect(phase!.getAttribute("data-company")).toBe("co-beta1");
  });
});
