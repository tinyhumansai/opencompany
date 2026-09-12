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
 * Both routes, on both credential surfaces.
 *
 * `COMPOSIO_MANAGED_HIDDEN` and `INFERENCE_MANAGED_HIDDEN` are now both off, so
 * these pin the *presence* of the managed route rather than its absence — and
 * the pair that always mattered is unchanged: a route must be selectable, and a
 * company already on one must be legible. The hide satisfied the second and
 * broke the first; what replaced it has to do both.
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

describe("Composio offers both routes, and says which one is live", () => {
  it("offers a real choice between the two accounts", async () => {
    // The point of the flag flip. While it was set, `MODE_ORDER` filtered
    // `managed` out and the picker collapsed to a single credential field —
    // there was nothing to choose because only one route was on offer.
    await mountComposio(composioStatus({ mode: "managed", credentialSource: "none" }));

    expect(find("composio-row-managed")).not.toBeNull();
    expect(find("composio-row-byok")).not.toBeNull();
    expect(document.querySelectorAll('[role="radiogroup"]')).toHaveLength(1);
  });

  it("checks exactly one route, never none", async () => {
    // The a11y break the hide caused: managed filtered out of the order array
    // with a company still on it left `active` false for every tile, so the
    // whole group reported aria-checked="false" — a control claiming the
    // company had chosen nothing, which is a different and wrong statement from
    // "it is on a route not offered here".
    await mountComposio(composioStatus({ mode: "managed", credentialSource: "none" }));

    expect(document.querySelectorAll('[role="radio"][aria-checked="true"]')).toHaveLength(1);
    expect(
      find("composio-row-managed-select")!.getAttribute("aria-checked"),
      "the company is on managed",
    ).toBe("true");
  });

  it("names the managed route, because it is a route again", async () => {
    await mountComposio(composioStatus({ mode: "managed", credentialSource: "company" }));

    expect(container.textContent).toContain("TinyHumans-managed");
    // ...and says which account pays, which is the decision the row exists for.
    expect(find("composio-row-managed-subline")!.textContent).toContain(
      "Billed to this company's TinyHumans account",
    );
  });

  it("gives a BYOK company the route back, and still no second name for it", async () => {
    // Clearing a BYOK key and switching to managed are ONE host call, because
    // the host derives the route from whether a key exists. While managed was
    // hidden, the only control that made that call could not name where it
    // landed, so there was no honest button and the section offered none.
    //
    // With the route on screen the call has a name — the managed row's
    // "Use this" — and the own-account row must NOT also offer it as a
    // "Remove key", which would be one action wearing two labels.
    await mountComposio(composioStatus({ mode: "byok", credentialSource: "static" }));

    expect(find("composio-row-managed-select")).not.toBeNull();
    expect(find("composio-row-managed-select")!.textContent).toContain("Use this");
    expect(find("composio-row-byok-remove")).toBeNull();
    // Rotation is still reachable, so a compromised key is still replaceable.
    expect(find("composio-row-byok-replace")).not.toBeNull();
  });

  it("hides the way back when the managed chain resolves to nothing", async () => {
    // Offering a switch into an outage is worse than offering no switch. The
    // row still reports why, in its sub-line — which is the whole of what the
    // operator gets here now that the company-credential card is off this page
    // (it is on the API Key page), and is why the sub-line has to say which
    // payer failed to resolve rather than only that one did not.
    await mountComposio(
      composioStatus({
        mode: "byok",
        credentialSource: "static",
        managedCredentialSource: "none",
      }),
    );

    expect(find("composio-row-managed-select")).toBeNull();
    expect(find("composio-row-managed-subline")!.textContent).toContain("No credential resolves");
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
