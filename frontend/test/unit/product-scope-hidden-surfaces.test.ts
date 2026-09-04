// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { ComposioStatus } from "@/api/composio";
import type { InferenceStatus } from "@/api/inference";
import { INFERENCE_PROVIDERS, SETUP_INFERENCE_OPTIONS } from "@/api/setup";
import { HostSwitcher, hostSwitcherMenu } from "@/components/host-switcher";
import { canCreateCompanies, offersCompanyCreation } from "@/components/create-company-dialog";
import { ComposioSection } from "@/views/connections/ComposioSection";
import { InferenceSection } from "@/views/connections/InferenceSection";
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

describe("the company switcher is a label, not a menu", () => {
  it("opens nothing, even with two hosts and two companies to offer", async () => {
    await show(hosts([CONNECTION, SECOND]));

    // The trigger still names the company — that is the whole surface now.
    expect(container.textContent).toContain("Acme");
    // Nothing to click at all: not a disabled control, not a chevron over an
    // empty popup, no button element for a keyboard to land on.
    expect(container.querySelector("button")).toBeNull();
    expect(container.querySelector("[aria-haspopup]")).toBeNull();
    expect(container.querySelector("[aria-expanded]")).toBeNull();
  });

  it("offers no host roster, no way to add one and no way to manage one", async () => {
    await show(hosts([CONNECTION, SECOND]));
    await openWhateverExists();

    expect(find("host-row-c1")).toBeNull();
    expect(find("host-row-c2")).toBeNull();
    expect(find("host-switcher-add")).toBeNull();
    expect(find("host-switcher-manage")).toBeNull();
  });

  it("offers no company switching and no way to make another company", async () => {
    await show(hosts([CONNECTION]));
    await openWhateverExists();

    expect(find("company-row-a")).toBeNull();
    expect(find("company-row-b")).toBeNull();
    expect(find("switcher-new-company")).toBeNull();
    expect(container.textContent).not.toContain("All companies");
  });

  it("keeps the trigger a nameplate rather than a chevron over an empty popup", async () => {
    // The trap this guards: `hostSwitcherMenu` still answers "any host at all
    // opens a menu", and with every group hidden that would be a chevron over a
    // popup with nothing in it. The switcher must not consult it alone.
    expect(hostSwitcherMenu(1)).toBe(true);

    await show(hosts([CONNECTION]));
    await openWhateverExists();

    // Nothing was openable, so nothing opened.
    expect(document.querySelector("[role='menu']")).toBeNull();
    expect(find("host-switcher-add")).toBeNull();
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

function inferenceClient(status: InferenceStatus) {
  return {
    scopeFor: (company: string | null) =>
      company ? `/api/v1/companies/${company}` : "/api/v1/company",
    get: async (path: string) => (path.endsWith("/inference/models") ? [] : status),
    put: async () => ({ status, note: "" }),
    del: async () => ({ status, note: "" }),
    post: async () => ({ status, note: "" }),
  } as unknown as OpenCompanyClient;
}

function inferenceStatus(over: Partial<InferenceStatus> = {}): InferenceStatus {
  return {
    provider: "openrouter",
    slug: "openrouter",
    baseUrl: "https://openrouter.ai/api/v1",
    models: {},
    defaultTierModels: {},
    source: "runtime",
    keyConfigured: true,
    cognition: "echo",
    usageMetering: "none",
    restartRequired: false,
    harnessReachable: true,
    canRebuildInPlace: true,
    ...over,
  };
}

async function mountInference(status: InferenceStatus) {
  await act(async () => {
    root.render(
      createElement(InferenceSection, {
        client: inferenceClient(status),
        company: "acme",
        canManage: true,
      }),
    );
  });
}

describe("inference asks the operator to name a provider", () => {
  it("does not offer the managed provider in the list", async () => {
    await mountInference(inferenceStatus());

    // The list is portalled and only mounts once the select is opened — asserted
    // without this the test passes against a tree that still offers managed.
    const trigger = document.querySelector("#inference-provider") as HTMLElement | null;
    expect(trigger, "no provider select").toBeTruthy();
    await act(async () => {
      trigger!.click();
      trigger!.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
      trigger!.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
      trigger!.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
    });

    const options = Array.from(document.querySelectorAll("[role='option']")).map((o) =>
      o.textContent?.trim(),
    );
    expect(options.length, "the provider list did not open").toBeGreaterThan(0);
    expect(options).not.toContain("Managed (TinyHumans)");
    expect(options).toContain("OpenRouter");
  });

  it("shows a value that is a real member of its own option set", async () => {
    // The bug in one line: the trigger rendered a label out of the full
    // descriptor table while the list was filtered, so the control displayed a
    // provider none of its options matched.
    await mountInference(inferenceStatus({ provider: "managed", slug: "managed" }));

    const trigger = document.querySelector("#inference-provider") as HTMLElement;
    const shown = trigger.textContent ?? "";
    expect(shown).not.toContain("Managed (TinyHumans)");

    await act(async () => {
      trigger.click();
      trigger.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
      trigger.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
      trigger.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
    });

    const options = Array.from(document.querySelectorAll("[role='option']")).map(
      (o) => o.textContent?.trim() ?? "",
    );
    expect(options.length, "the list did not open").toBeGreaterThan(0);
    expect(options).not.toContain("Managed (TinyHumans)");
    // Whatever the trigger shows has to be one of the rows below it.
    expect(options.some((label) => shown.includes(label))).toBe(true);
  });
  it("brands nothing on the card for a route it does not offer", async () => {
    await mountInference(inferenceStatus({ provider: "managed", slug: "managed" }));

    expect(find("inference-current-provider")!.textContent).toBe("Not configured");
    expect(find("inference-not-configured")).not.toBeNull();
    // No brand, no source word naming the route, and not the platform endpoint.
    expect(container.textContent).not.toContain("Managed (TinyHumans)");
    expect(container.textContent).not.toContain("api.tinyhumans.ai");
  });

  it("leads an unconfigured company into a provider it can actually finish", async () => {
    // Not an inert placeholder: the resting state has to be completable, or
    // onboarding dead-ends on a row nobody can act on. The header still reports
    // the company's real state, so proposing a provider in the form is an offer
    // rather than a claim about what is configured.
    await mountInference(inferenceStatus({ provider: "managed", slug: "managed" }));

    expect(find("inference-current-provider")!.textContent).toBe("Not configured");
    expect(document.querySelector("#inference-provider")?.textContent).toContain("OpenRouter");
    expect(document.querySelector("#inference-key")).not.toBeNull();
    expect((find("inference-save") as HTMLButtonElement).disabled).toBe(false);
  });
});

describe("the wizard's model step asks the operator to name a provider too", () => {
  it("does not offer the managed endpoint as the thing to think with", () => {
    // The wizard has its own provider list, and it is the FIRST screen of a
    // first run — hiding the option only on the settings card would leave the
    // managed route selectable at the one moment every operator passes through.
    const offered = SETUP_INFERENCE_OPTIONS.map((option) => option.id);

    expect(offered).not.toContain("managed");
    expect(offered).toContain("openrouter");
    // Still resolvable, so a host already reporting it keeps its label.
    expect(INFERENCE_PROVIDERS.find((p) => p.id === "managed")?.label).toBe("TinyHumans");
  });
});

describe("the roster's keyboard shortcuts go with the roster", () => {
  it("does not select a host on Cmd-1 when there is no roster to see", async () => {
    // The listener is installed on `window` by the provider, not by the menu, so
    // hiding the switcher does not remove it. Left live it would swallow the
    // browser's own Cmd-1 and switch hosts with nothing on screen saying so.
    const picked: string[] = [];
    const value = { ...hosts([CONNECTION, SECOND]), onSelect: (id: string) => picked.push(id) };
    await show(value as HostsValue);

    await act(async () => {
      window.dispatchEvent(
        new KeyboardEvent("keydown", { key: "2", metaKey: true, bubbles: true }),
      );
    });

    expect(picked).toEqual([]);
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
