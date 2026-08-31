// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { AgentDetailDto } from "@/api/types";
import { AgentDetailView } from "@/views/team/AgentDetailView";

/**
 * The instructions editor on the agent detail view (issue #1530).
 *
 * A unit render, earned the same way `provider-detail-render` earns it: the
 * claim under test *is* what reaches the operator's eye, and no pure helper can
 * hold it. Four facts: the editor and its Edit button appear for a blueprint
 * (manifest) agent too — the old dead button is gone; a persona change saves as
 * an `instructions`-only patch; a manifest agent's name stays read-only while
 * its instructions do not; and Reset-to-blueprint sends `instructions: null`.
 */

function overlay(over: Partial<AgentDetailDto> = {}): AgentDetailDto {
  return {
    id: "agent-overlay",
    name: "Nova",
    role: "Growth Marketer",
    source: "overlay",
    editable: ["name", "role", "description", "tools", "instructions"],
    isOrchestrator: false,
    tools: { requested: [], companyAllow: [], deskAllow: [], deskCeilingActive: false, effective: [] },
    desks: [],
    inboxEnabled: false,
    description: "Runs paid acquisition.",
    instructions: "Always confirm the budget before launching.",
    instructionsOverridden: false,
    ...over,
  };
}

function manifest(over: Partial<AgentDetailDto> = {}): AgentDetailDto {
  return overlay({
    id: "agent-manifest",
    name: undefined,
    source: "manifest",
    editable: ["instructions"],
    blueprintInstructions: "Ship weekly. Keep the changelog current.",
    ...over,
  });
}

function makeClient(detail: AgentDetailDto, updateAgent = vi.fn(async () => detail)) {
  return {
    getAgent: vi.fn(async () => detail),
    updateAgent,
    // The detail view reads the signed-in role and, best-effort, the board.
    // A member role skips the people directory; a company of `null` skips the
    // board read entirely, so neither needs a real answer here.
    get: vi.fn(async () => ({ role: "member" })),
    // Since `feat/external-acp` merged, the detail view also reads the
    // company's declared harnesses on mount (issue #1245's harness picker).
    // These tests predate that and care about none of it — an empty list is
    // the honest answer for a fixture that declares no `[[harness]]`, and it
    // keeps the picker off the screen so the instructions assertions below
    // still address the control they mean.
    listHarnesses: vi.fn(async () => []),
    scopeFor: () => "",
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function mount(client: OpenCompanyClient, agentId: string) {
  await act(async () => {
    root.render(
      createElement(AgentDetailView, {
        client,
        company: null,
        agentId,
        onBack: () => {},
      }),
    );
  });
  // Flush the boot() read so the view settles into `ready`.
  await act(async () => {});
}

async function click(testid: string) {
  await act(async () => {
    (document.querySelector(`[data-testid="${testid}"]`) as HTMLElement | null)?.click();
  });
}

/** Set a controlled input/textarea's value the way React's onChange expects. */
async function type(testid: string, value: string) {
  const el = document.querySelector(`[data-testid="${testid}"]`) as
    | HTMLTextAreaElement
    | HTMLInputElement
    | null;
  if (!el) throw new Error(`no element ${testid}`);
  const proto =
    el instanceof HTMLTextAreaElement
      ? window.HTMLTextAreaElement.prototype
      : window.HTMLInputElement.prototype;
  const setter = Object.getOwnPropertyDescriptor(proto, "value")!.set!;
  await act(async () => {
    setter.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

function el(testid: string): HTMLElement | null {
  return document.querySelector(`[data-testid="${testid}"]`);
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

describe("the agent detail instructions editor", () => {
  it("saves an overlay agent's instructions as an instructions-only patch", async () => {
    const update = vi.fn(async () => overlay());
    const client = makeClient(overlay(), update);
    await mount(client, "agent-overlay");

    await click("agent-edit");
    expect(el("agent-field-instructions"), "the editor renders for an overlay agent").not.toBeNull();

    await type("agent-field-instructions", "Report ROAS every Friday.");
    await click("agent-save");

    expect(update).toHaveBeenCalledWith(
      "agent-overlay",
      { instructions: "Report ROAS every Friday." },
      null,
    );
  });

  it("shows an enabled Edit button and a working editor for a blueprint agent", async () => {
    const update = vi.fn(async () => manifest());
    const client = makeClient(manifest(), update);
    await mount(client, "agent-manifest");

    // The old dead button: a manifest agent used to have `editable: []` and the
    // Edit button sat disabled. It is enabled now.
    const edit = el("agent-edit") as HTMLButtonElement | null;
    expect(edit).not.toBeNull();
    expect(edit!.disabled).toBe(false);

    await click("agent-edit");
    // Name is a manifest-native field and stays read-only; instructions do not.
    expect((el("agent-field-name") as HTMLInputElement).readOnly).toBe(true);
    expect((el("agent-field-instructions") as HTMLTextAreaElement).readOnly).toBe(false);

    await type("agent-field-instructions", "Tone: terse. Escalate blockers same day.");
    await click("agent-save");
    expect(update).toHaveBeenCalledWith(
      "agent-manifest",
      { instructions: "Tone: terse. Escalate blockers same day." },
      null,
    );
  });

  it("resets to blueprint by sending instructions: null", async () => {
    const update = vi.fn(async () => manifest({ instructionsOverridden: false }));
    const client = makeClient(manifest({ instructionsOverridden: true }), update);
    await mount(client, "agent-manifest");

    await click("agent-instructions-reset");
    expect(update).toHaveBeenCalledWith("agent-manifest", { instructions: null }, null);
  });

  it("offers no reset control when the blueprint is not overridden", async () => {
    const client = makeClient(manifest({ instructionsOverridden: false }));
    await mount(client, "agent-manifest");
    expect(el("agent-instructions-reset")).toBeNull();
  });
});
