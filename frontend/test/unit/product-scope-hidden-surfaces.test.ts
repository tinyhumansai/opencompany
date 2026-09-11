// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { ComposioStatus } from "@/api/composio";
import { INFERENCE_PROVIDERS, SETUP_INFERENCE_OPTIONS } from "@/api/setup";
import { HostSwitcher, hostSwitcherMenu } from "@/components/host-switcher";
import { canCreateCompanies, offersCompanyCreation } from "@/components/create-company-dialog";
import { ComposioSection } from "@/views/connections/ComposioSection";
import { HostsProvider, type HostsValue } from "@/connections/HostsContext";
import type { Connection, ConnectionId } from "@/connections/types";
import { SidebarProvider } from "@/components/ui/sidebar";

/**
 * One company, and nothing else selectable.
 *
 * These pin the *absence* of controls, which is the only thing a hide can be
 * checked by. Each fails against a tree where the matching flag in
 * `product-scope.ts` is false — that is what makes them a test of the hide
 * rather than of the layout that happens to be on screen.
 */

const CONNECTION: Connection = {
  id: "c1" as ConnectionId,
  defaultCompany: null,
  label: "This computer",
  baseUrl: "",
  credential: { kind: "cookie" },
  status: "live",
  identity: null,
  companies: [],
  connector: { kind: "local" },
};

const SECOND: Connection = { ...CONNECTION, id: "c2" as ConnectionId, label: "Acme" };

function hosts(connections: Connection[]): HostsValue {
  return {
    connections,
    selected: connections[0]?.id ?? null,
    onSelect: () => {},
    onAdd: () => {},
    localInstances: [],
    onEditHost: () => {},
    onRemoveHost: () => {},
    hub: false,
  };
}

let container: HTMLDivElement;
let root: Root;

/** jsdom ships no `matchMedia`, and `SidebarProvider` reaches for it unguarded. */
function stubMatchMedia() {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }),
  });
}

beforeEach(() => {
  stubMatchMedia();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function show(value: HostsValue, props: Record<string, unknown> = {}) {
  await act(async () => {
    root.render(
      createElement(
        SidebarProvider,
        null,
        createElement(
          HostsProvider,
          { value, children: null } as never,
          createElement(HostSwitcher, {
            // What the app shell actually renders (`app-shell.tsx`): the window
            // title row, which is where an operator sees this.
            variant: "titlebar",
            companyName: "Acme",
            companies: [
              { id: "a", name: "Acme" },
              { id: "b", name: "Other" },
            ],
            activeCompany: "a",
            onSwitchCompany: () => {},
            onCreateCompany: () => {},
            canCreateCompany: true,
            ...props,
          } as never),
        ),
      ),
    );
  });
}

const find = (testId: string) =>
  document.querySelector(`[data-testid="${testId}"]`) as HTMLElement | null;

/**
 * Press whatever the switcher put on screen, then look.
 *
 * Menu content is portalled and only mounts once the trigger is pressed, so an
 * assertion made without this is vacuous — it passes against a tree that still
 * has the whole roster, because the roster simply had not been opened yet.
 */
async function openWhateverExists() {
  const trigger = container.querySelector("button");
  if (!trigger) return;
  await act(async () => {
    trigger.click();
    trigger.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
    trigger.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    trigger.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
  });
}

describe("the switcher carries hosts, and only hosts", () => {
  /**
   * The asymmetry these pin.
   *
   * `HOSTS_HIDDEN` is off and `COMPANY_SWITCHING_HIDDEN` is still on, so the
   * one control in the title row holds exactly one of its two groups. That is
   * the arrangement most likely to be broken by accident: the switcher derives
   * `menu` from both flags at once, so turning either one back on has to leave
   * the other's group exactly where it was.
   */
  it("opens a menu, and is a real button for a keyboard to land on", async () => {
    await show(hosts([CONNECTION, SECOND]));

    // The trigger still names the company, and now it also opens.
    expect(container.textContent).toContain("Acme");
    const trigger = container.querySelector("button");
    expect(trigger).not.toBeNull();
    expect(trigger?.getAttribute("aria-haspopup")).toBe("menu");
  });

  it("offers the host roster, a way to add one and a way to manage one", async () => {
    await show(hosts([CONNECTION, SECOND]));
    await openWhateverExists();

    expect(find("host-row-c1")).not.toBeNull();
    expect(find("host-row-c2")).not.toBeNull();
    expect(find("host-switcher-add")).not.toBeNull();
    expect(find("host-switcher-manage")).not.toBeNull();
  });

  it("offers no company switching and no way to make another company", async () => {
    await show(hosts([CONNECTION]));
    await openWhateverExists();

    expect(find("company-row-a")).toBeNull();
    expect(find("company-row-b")).toBeNull();
    expect(find("switcher-new-company")).toBeNull();
    expect(container.textContent).not.toContain("All companies");
  });

  it("opens the roster on a single host, not just on two", async () => {
    // The rule `hostSwitcherMenu` states — any host at all opens a menu —
    // reaches the rendered tree rather than stopping at the predicate. One
    // host is the ordinary web console, and "Manage hosts" is the only way to
    // rename or re-address the one it has.
    expect(hostSwitcherMenu(1)).toBe(true);

    await show(hosts([CONNECTION]));
    await openWhateverExists();

    expect(document.querySelector("[role='menu']")).not.toBeNull();
    expect(find("host-switcher-add")).not.toBeNull();
  });
});

/**
 * BYOK only, on both credential surfaces.
 *
 * The pair that matters: the managed route must not be *selectable*, and a
 * company already on it must still be *legible*. Hiding a route by deleting its
 * descriptor would satisfy the first and break the second — the label tables
 * keep every route for exactly that reason.
 */

function composioStatus(over: Partial<ComposioStatus> = {}): ComposioStatus {
  return {
    inBuild: true,
    granted: true,
    credentialSource: "company",
    mode: "managed",
    backendUrl: "https://api.tinyhumans.ai",
    toolkits: [],
    openMode: true,
    effectiveToolkits: [],
    effectiveCatalog: [],
    catalogSource: "manifest",
    catalogNotice: null,
    ...over,
  } as ComposioStatus;
}

function composioClient(status: ComposioStatus) {
  return {
    scopeFor: (company: string | null) =>
      company ? `/api/v1/companies/${company}` : "/api/v1/company",
    get: async () => status,
    put: async () => ({ status, note: "" }),
    post: async () => ({ status, note: "" }),
    del: async () => ({ status, note: "" }),
  } as unknown as OpenCompanyClient;
}

async function mountComposio(status: ComposioStatus) {
  await act(async () => {
    root.render(
      createElement(ComposioSection, {
        client: composioClient(status),
        company: "acme",
        canManage: true,
        onChanged: () => {},
      }),
    );
  });
}

describe("Composio offers this company's own account and nothing else", () => {
  it("offers this company's own Composio key, and no route to pick between", async () => {
    // With one route left there is nothing to choose, so the picker goes and the
    // credential field for that route is what the operator lands on. A picker of
    // one is not a choice; it is a click between the operator and the task.
    await mountComposio(composioStatus({ mode: "managed", credentialSource: "none" }));

    expect(document.querySelector("#composio-api-key")).not.toBeNull();
    expect(document.querySelectorAll('[role="radiogroup"]')).toHaveLength(0);
    expect(find("composio-mode-managed")).toBeNull();
  });

  it("cannot leave a radiogroup with nothing checked, because there is none", async () => {
    // The a11y break this replaces: managed filtered out of the order array with
    // a company still on it left `active` false for every tile, so the whole
    // group reported aria-checked="false". Removing the group removes the state.
    await mountComposio(composioStatus({ mode: "managed", credentialSource: "none" }));

    const radios = document.querySelectorAll('[role="radio"]');
    const checked = document.querySelectorAll('[role="radio"][aria-checked="true"]');
    expect(radios.length === 0 || checked.length === 1).toBe(true);
  });

  it("names nothing about the hidden route anywhere on the panel", async () => {
    await mountComposio(composioStatus({ mode: "managed", credentialSource: "none" }));

    expect(container.textContent).not.toContain("OpenHuman");
    expect(container.textContent).not.toContain("TinyHumans");
    expect(container.textContent).not.toContain("api.tinyhumans.ai");
  });
  it("offers a BYOK company no control that would move it off its own account", async () => {
    // Clearing a key is not "the key goes away". The host derives the route from
    // whether one exists, so an empty write puts the company back on the route
    // this console no longer offers — and a company with any credential there
    // resumes acting through it, a different account billed differently.
    //
    // A button here could only say that, naming the route, or not say it, which
    // is the switch happening silently. Rotating stays; removing does not.
    await mountComposio(composioStatus({ mode: "byok", credentialSource: "company" }));

    expect(find("composio-clear-key")).toBeNull();
    expect(container.textContent).not.toContain("Clear key");
    expect(container.textContent).not.toContain("use OpenHuman-managed");
    // Rotation is still reachable, so a compromised key is still replaceable.
    expect(container.textContent).toContain("Rotate key");
  });
});


// The settings half of "offer the managed route" used to be pinned by mounting
// the single-provider form and opening its Provider select. **That control is
// retired**: a select over a closed three-element list is what the provider list
// replaces, and "is Managed offered" is no longer a question about a dropdown —
// Managed is an unremovable row on the Connected list, always present and always
// on, so it cannot be hidden by a flag.
//
// What the hide could still affect is the wizard, which has its own list and is
// the first screen of a first run. That half is below and is unchanged.

describe("the wizard's model step offers the managed endpoint too", () => {
  it("offers the managed endpoint as a thing to think with", () => {
    // The wizard has its own provider list, and it is the FIRST screen of a
    // first run. Offering the route only on the settings card would hide the
    // one-click option at the one moment every operator passes through.
    const offered = SETUP_INFERENCE_OPTIONS.map((option) => option.id);

    expect(offered).toContain("managed");
    expect(offered).toContain("openrouter");
    expect(INFERENCE_PROVIDERS.find((p) => p.id === "managed")?.label).toBe("TinyHumans");
  });
});

describe("the roster's keyboard shortcuts go with the roster", () => {
  it("selects a host on Cmd-2 now that the roster is on screen", async () => {
    // The listener is installed on `window` by the provider, not by the menu,
    // and it is gated on the same flag as the roster it drives. The pairing is
    // the point: a shortcut that switched hosts with no roster on screen would
    // swallow the browser's own Cmd-2 and act invisibly, and a roster whose
    // printed `⌘2` did nothing would be furniture.
    const picked: string[] = [];
    const value = { ...hosts([CONNECTION, SECOND]), onSelect: (id: string) => picked.push(id) };
    await show(value as HostsValue);

    await act(async () => {
      window.dispatchEvent(
        new KeyboardEvent("keydown", { key: "2", metaKey: true, bubbles: true }),
      );
    });

    expect(picked).toEqual(["c2"]);
  });
});

describe("company creation is gone from every trigger, not just the switcher", () => {
  /**
   * Four triggers reach one dialog: the switcher's "New company", the picker's
   * own button, the picker's per-card Reset, the no-company screen, and
   * Settings' "Reset / Start clean" — which archives and re-provisions through
   * the same flow. Gating them one at a time is how three stayed live after the
   * first was hidden, so the question is asked once, where they all ask it.
   */
  it("answers no at the funnel every trigger goes through", () => {
    const platform = { carriesPlatformBearer: true } as unknown as OpenCompanyClient;
    const person = { carriesPlatformBearer: false } as unknown as OpenCompanyClient;

    expect(offersCompanyCreation(platform)).toBe(false);
    expect(offersCompanyCreation(person)).toBe(false);
  });

  it("leaves the caller's own capability alone", () => {
    // Product scope decides whether an entry point renders. Whether this
    // principal may create a company is a different question, and the dialog's
    // preflight and submit read it — so a scope flag reaching in there would
    // make the shipped path inert while its tests passed against logic nothing
    // could run.
    const platform = { carriesPlatformBearer: true } as unknown as OpenCompanyClient;
    const person = { carriesPlatformBearer: false } as unknown as OpenCompanyClient;

    expect(canCreateCompanies(platform)).toBe(true);
    expect(canCreateCompanies(person)).toBe(false);
  });
});
