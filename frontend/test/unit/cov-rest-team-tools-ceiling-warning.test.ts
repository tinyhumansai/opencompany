// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { AgentDetailDto } from "@/api/types";
import { AgentDetailView } from "@/views/team/AgentDetailView";

/**
 * The tools editor's ceiling warning (`agent-tools-uncovered`) is,
 * deliberately, a soft guard: a glob outside `grantCeiling` "can only ever
 * take capability away — never add to it" (`lib/agent.ts` doc), because
 * `agent_scoped_grants` on the host narrows to the same ceiling this warning
 * is drawn from. Saving anyway is therefore safe by construction, not a
 * bypass — but nothing had verified that this is the actual, current
 * behaviour: that the warning appears AND that Save stays live through it,
 * rather than the warning silently doing nothing or Save silently blocking.
 */

function overlay(over: Partial<AgentDetailDto> = {}): AgentDetailDto {
  return {
    id: "agent-overlay",
    name: "Nova",
    role: "Growth Marketer",
    source: "overlay",
    editable: ["name", "role", "description", "tools", "instructions"],
    isOrchestrator: false,
    tools: { requested: [], companyAllow: ["docs.*"], deskAllow: [], deskCeilingActive: false, effective: [] },
    skills: { requested: null, companyAvailable: [], effective: [], overridden: false },
    desks: [],
    inboxEnabled: false,
    description: "Runs paid acquisition.",
    instructions: "Always confirm the budget before launching.",
    instructionsOverridden: false,
    ...over,
  };
}

function clientAs(agent: AgentDetailDto, updateAgent = vi.fn(async () => agent)): OpenCompanyClient {
  return {
    getAgent: vi.fn(async () => agent),
    updateAgent,
    get: vi.fn(async () => ({ role: "admin" })),
    listHarnesses: vi.fn(async () => []),
    scopeFor: () => "",
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

function at(testid: string): HTMLElement | null {
  return container.querySelector<HTMLElement>(`[data-testid="${testid}"]`);
}

/**
 * Open Tools, start editing, and reveal the raw glob field.
 *
 * Three steps where there used to be one. Tools is a tab on the agent's page
 * now (Overview leads), and the editor's primary surface is a switch per grant
 * in the company ceiling — switches cannot spell a wildcard like `docs.*`, so
 * the field this file drives moved behind an "Edit globs" disclosure. It is
 * still the only way to type a pattern, which is exactly why it survived.
 */
async function openGlobField() {
  await act(async () => at("agent-tab-tools")?.click());
  await act(async () => at("agent-tools-edit")?.click());
  await act(async () => at("agent-tools-advanced")?.click());
}

async function type(text: string) {
  const el = at("agent-tools-field") as HTMLInputElement;
  const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
  await act(async () => {
    setValue?.call(el, text);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

describe("agent tools editor, a grant outside the company ceiling", () => {
  it("warns, but still leaves Save live — the write is harmless, the host ignores what it does not allow", async () => {
    const client = clientAs(overlay());
    await act(async () => {
      root.render(
        createElement(AgentDetailView, { client, company: "acme", agentId: "agent-overlay", onBack: vi.fn() }),
      );
    });
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    await openGlobField();
    await type("docs.*, media.*");

    const warning = at("agent-tools-uncovered");
    expect(warning).not.toBeNull();
    expect(warning?.textContent).toContain("media.*");

    const saveButtons = Array.from(container.querySelectorAll("button")).filter(
      (b) => b.textContent === "Save",
    );
    expect(saveButtons).toHaveLength(1);
    expect(saveButtons[0].disabled).toBe(false);
  });

  it("saves the over-ceiling glob byte for byte — the console never silently drops what the operator typed", async () => {
    const updateAgent = vi.fn(async () => overlay());
    const client = clientAs(overlay(), updateAgent);
    await act(async () => {
      root.render(
        createElement(AgentDetailView, { client, company: "acme", agentId: "agent-overlay", onBack: vi.fn() }),
      );
    });
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    await openGlobField();
    await type("docs.*, media.*");
    const saveButton = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Save");
    await act(async () => saveButton?.click());

    expect(updateAgent).toHaveBeenCalledWith(
      "agent-overlay",
      { tools: ["docs.*", "media.*"] },
      "acme",
    );
  });

  it("shows no ceiling warning for a grant the company already covers", async () => {
    const client = clientAs(overlay());
    await act(async () => {
      root.render(
        createElement(AgentDetailView, { client, company: "acme", agentId: "agent-overlay", onBack: vi.fn() }),
      );
    });
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    await openGlobField();
    await type("docs.read");

    expect(at("agent-tools-uncovered")).toBeNull();
  });
});
