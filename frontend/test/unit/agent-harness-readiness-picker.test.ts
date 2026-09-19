// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { AgentDetailDto, HarnessDto } from "@/api/types";
import { AgentDetailView } from "@/views/team/AgentDetailView";

/**
 * Readiness in the agent page's harness picker (issue #2394).
 *
 * Before this, the picker built each option from the manifest alone, so an
 * operator could bind a teammate to `claude` with no way to know whether
 * Claude Code was installed, signed in, or absent — the turn simply failed
 * later, somewhere else.
 *
 * The rule the options must never break is that an unready one is still
 * pickable. A browser cannot see a local CLI at all, so "we did not look" and
 * "not installed" are different facts, and disabling on the first would be a
 * guess dressed as a verdict.
 */

let container: HTMLDivElement;
let root: Root;

function detail(over: Partial<AgentDetailDto> = {}): AgentDetailDto {
  return {
    id: "researcher",
    name: "Researcher",
    role: "Researcher",
    source: "overlay",
    editable: ["harness", "model"],
    isOrchestrator: false,
    tools: { requested: null, companyAllow: ["*"], deskAllow: [], deskCeilingActive: false, effective: [] },
    desks: [],
    inboxEnabled: false,
    ...over,
  };
}

const HARNESSES: HarnessDto[] = [
  { id: "claude", kind: "acp", default: true, detected: true, agent: "claude", transport: "local", runsHere: true },
  { id: "codex", kind: "acp", default: false, detected: true, agent: "codex", transport: "local", runsHere: true },
];

/**
 * A desktop shell whose survey reports both CLIs present but neither adapter
 * installed — the ordinary state of a machine with Claude Code and Codex on it
 * and no `@agentclientprotocol/*` global.
 */
function installBridge(
  readiness: Record<string, unknown>,
  models: Record<string, unknown[]> = {},
  onInstall?: (id: string) => void,
): { invocations: string[] } {
  const invocations: string[] = [];
  (window as unknown as { __TAURI__: unknown }).__TAURI__ = {
    core: {
      invoke: async (command: string, args?: { id?: string }) => {
        invocations.push(command);
        const id = args?.id ?? "";
        if (command === "oc_acp_harnesses") {
          return Object.entries(readiness).map(([key, state]) => ({
            id: key,
            label: key === "claude" ? "Claude Code" : "Codex",
            readiness: state,
          }));
        }
        if (command === "oc_acp_confirm_harness") {
          return { readiness: readiness[id] ?? { state: "notInstalled" }, models: models[id] ?? [] };
        }
        if (command === "oc_acp_install_harness") {
          onInstall?.(id);
          return undefined;
        }
        return undefined;
      },
      Channel: class {
        onmessage: ((message: string) => void) | null = null;
      },
    },
  };
  return { invocations };
}

function uninstallBridge(): void {
  delete (window as unknown as { __TAURI__?: unknown }).__TAURI__;
}

function client(harnesses: HarnessDto[] = HARNESSES): OpenCompanyClient {
  return {
    getAgent: vi.fn(async () => detail()),
    listTeam: vi.fn(async () => []),
    listHarnesses: vi.fn(async () => harnesses),
    get: vi.fn(async () => ({ role: "admin" })),
    updateAgent: vi.fn(),
    put: vi.fn(),
    scopeFor: () => "/api/v1/company",
  } as unknown as OpenCompanyClient;
}

async function mount(c: OpenCompanyClient) {
  // Harness & model lives on the Model tab, and the tab rides the hash.
  window.location.hash = "#/team/researcher?tab=model";
  await act(async () => {
    root.render(
      createElement(AgentDetailView, { client: c, company: "acme", agentId: "researcher", onBack: () => {} }),
    );
  });
  await act(async () => {});
  await act(async () => {});
}

async function openEditor(c: OpenCompanyClient) {
  await mount(c);
  const pencil = container.querySelector('[data-testid="agent-harness-edit"]') as HTMLElement | null;
  if (!pencil) throw new Error(`no harness edit affordance; saw: ${container.innerHTML}`);
  await act(async () => {
    pencil.click();
  });
  // The survey and each per-row confirmation settle in microtasks.
  await act(async () => {});
  await act(async () => {});
}

async function openHarnessOptions() {
  const trigger = container.querySelector('[data-testid="agent-harness-select"]') as HTMLElement;
  await act(async () => {
    trigger.click();
  });
  return Array.from(document.querySelectorAll('[role="option"]')) as HTMLElement[];
}

/** The option for one harness, as distinct from the "Company default" row that also names it. */
function option(options: HTMLElement[], starts: string): HTMLElement {
  const found = options.find((o) => o.textContent?.trim().startsWith(starts));
  if (!found) {
    throw new Error(`no “${starts}” option; saw: ${options.map((o) => o.textContent).join(" | ")}`);
  }
  return found;
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
  uninstallBridge();
  vi.restoreAllMocks();
});

describe("the harness picker, on a desktop that can see the local CLIs", () => {
  it("badges an installed CLI whose adapter is missing, and offers to install it", async () => {
    installBridge({
      claude: { state: "adapterMissing", cli: "/usr/local/bin/claude" },
      codex: { state: "adapterMissing", cli: "/usr/local/bin/codex" },
    });
    await openEditor(client());

    const options = await openHarnessOptions();
    // "Add-on needed", never "Not installed": the CLI *is* there, and telling
    // someone otherwise sends them to reinstall software they already have.
    const claude = option(options, "claude — claude");
    expect(claude.textContent).toContain("Add-on needed");
    expect(claude.textContent).not.toContain("Not installed");
    expect(option(options, "codex — codex").textContent).toContain("Add-on needed");
    // The "Company default" row is a binding to whichever harness is default,
    // so it carries that harness's readiness rather than none at all.
    expect(option(options, "Company default").textContent).toContain("Add-on needed");

    // And the drafted harness carries the one action this app can actually
    // take about it.
    const action = container.querySelector('[data-testid="agent-harness-install"]');
    expect(action?.textContent).toContain("Install add-on");
    expect(container.querySelector('[data-testid="agent-harness-readiness"]')?.textContent).toContain(
      "needs a small add-on",
    );
  });

  it("refreshes the model list after installing, rather than leaving it empty", async () => {
    // The machine starts adapter-less and gains the adapter — with the models
    // it advertises — when the install runs.
    const state: Record<string, unknown> = {
      claude: { state: "adapterMissing", cli: "/usr/local/bin/claude" },
      codex: { state: "adapterMissing", cli: "/usr/local/bin/codex" },
    };
    const models: Record<string, unknown[]> = { claude: [], codex: [] };
    installBridge(state, models, (id) => {
      state[id] = { state: "ready" };
      models[id] = [{ value: "opus-5", name: "Opus 5", current: true }];
    });
    await openEditor(client());

    // Before: nothing to pick from, so the field is free text rather than an
    // empty dropdown.
    expect(container.querySelector('[data-testid="agent-model-input"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="agent-model-select"]')).toBeNull();

    const button = container.querySelector('[data-testid="agent-harness-install"]') as HTMLElement;
    await act(async () => {
      button.click();
    });
    await act(async () => {});
    await act(async () => {});

    // `installAcpHarness` evicts the cached confirmation and none of the model
    // effect's dependencies change, so without an explicit re-read the picker
    // would still be free text until the operator toggled the harness away and
    // back.
    expect(
      container.querySelector('[data-testid="agent-model-select"]'),
      "the model picker must appear once the harness can report its models",
    ).not.toBeNull();
    expect(container.querySelector('[data-testid="agent-harness-install"]')).toBeNull();
  });

  it("never renders an option the operator cannot choose", async () => {
    installBridge({
      claude: { state: "adapterMissing", cli: "/usr/local/bin/claude" },
      codex: { state: "nodeMissing" },
    });
    await openEditor(client());

    const options = await openHarnessOptions();
    expect(options.length).toBeGreaterThan(0);
    for (const option of options) {
      expect(
        option.getAttribute("aria-disabled"),
        `“${option.textContent}” was rendered unpickable`,
      ).not.toBe("true");
      expect(option.hasAttribute("data-disabled")).toBe(false);
    }
  });
});

describe("the harness picker, in a browser that cannot see a local CLI", () => {
  it("says it did not look, rather than guessing in either direction", async () => {
    uninstallBridge();
    await openEditor(client());

    const options = await openHarnessOptions();
    const claude = option(options, "claude — claude");
    expect(claude.textContent).toContain("Desktop only");
    expect(claude.textContent).not.toContain("Not installed");
    expect(claude.textContent).not.toContain("Ready");

    // Nothing to install, because nothing was established — an Install button
    // here would be acting on a fact nobody has.
    expect(container.querySelector('[data-testid="agent-harness-install"]')).toBeNull();
    for (const option of options) {
      expect(option.getAttribute("aria-disabled")).not.toBe("true");
    }
  });

  it("does not probe this machine until the editor is actually open", async () => {
    const bridge = installBridge({ claude: { state: "ready" }, codex: { state: "ready" } });
    await mount(client());

    // Viewing a teammate must not start a subprocess per harness. The probe is
    // the editor's, the same trigger the model list already uses.
    expect(bridge.invocations).not.toContain("oc_acp_confirm_harness");
    expect(bridge.invocations).not.toContain("oc_acp_harnesses");
  });
});
