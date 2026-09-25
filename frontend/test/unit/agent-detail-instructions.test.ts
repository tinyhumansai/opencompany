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
    skills: { requested: null, companyAvailable: [], effective: [], overridden: false },
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
  // The agent's definition is tabbed, and Overview leads — it is what the page
  // is opened to answer. Everything this file drives lives on the Instructions
  // tab, so open it before asserting, the way an operator does.
  await click("agent-tab-instructions");
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

/**
 * What the Instructions card actually shows (issue #1989).
 *
 * The card holds two different fields and used to label neither. In read mode
 * it rendered the *description* as a bare paragraph under a heading reading
 * "Instructions" and a subtitle reading "What this teammate was defined to do.
 * It frames every turn they take." — which describes the persona, not the
 * mandate. For a teammate with no persona that paragraph was the whole card, so
 * the mislabelling was the default rather than an edge, and nothing on screen
 * said the persona was empty at all.
 *
 * The operator who found it asked, of a teammate created through the reduced
 * dialog: "is this how a Role, What they do, and Instructions should be
 * surfaced?" These are the two facts that answer that.
 */
describe("what the Instructions card names", () => {
  function labels(): string[] {
    return Array.from(document.querySelectorAll("p.text-xs.font-medium")).map(
      (node) => node.textContent?.trim() ?? "",
    );
  }

  it("labels the mandate and the persona apart", async () => {
    const client = makeClient(
      overlay({
        description: "Owns the stockist pipeline.",
        instructions: "Escalate anything under 40% margin.",
      }),
    );
    await mount(client, "agent-overlay");

    expect(el("agent-description")!.textContent).toContain("Owns the stockist pipeline.");
    expect(el("agent-instructions")!.textContent).toContain("Escalate anything under 40% margin.");
    // Both named, so neither can be read as the other.
    expect(labels()).toEqual(
      expect.arrayContaining(["What they do", expect.stringContaining("Persona instructions")]),
    );
  });

  it("says so when there is no persona, instead of showing the mandate alone", async () => {
    const client = makeClient(
      overlay({ description: "Owns the stockist pipeline.", instructions: undefined }),
    );
    await mount(client, "agent-overlay");

    // The mandate is still there and still labelled as the mandate.
    expect(el("agent-description")!.textContent).toContain("Owns the stockist pipeline.");
    expect(el("agent-instructions"), "there is no persona to render").toBeNull();
    // And the empty persona is stated. An operator could previously only find
    // out by opening the edit form.
    const empty = el("agent-instructions-empty");
    expect(empty, "an absent persona is said, not left blank").not.toBeNull();
    expect(empty!.textContent).toContain("default wording");
  });
});
