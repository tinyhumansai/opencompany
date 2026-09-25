// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { AgentDetailDto } from "@/api/types";
import { AgentDetailView } from "@/views/team/AgentDetailView";

/**
 * The Skills card's two claims that only hold in a document.
 *
 * The first is the whole reason the card carries a `touched` flag beside the
 * stored scope. An inherited scope (`requested: null`) renders every switch on
 * while storing nothing, so the list the card starts a narrowing from cannot be
 * the stored one — it has to be the company's enabled set. Get that wrong and
 * the operator's first flick, which reads on screen as "all but this one",
 * writes "only the ones I flicked", and a teammate silently loses every skill
 * the operator never touched. The host accepts that write happily: it is a
 * legal narrowing.
 *
 * The second is the dropped-slug line. A scope entry the company does not have
 * enabled is stored rather than refused, so the only thing standing between an
 * operator and a scope that quietly confers nothing is the card saying so.
 */

const ENABLED = ["brand-voice", "invoicing", "web-research"];

function detail(skills: Partial<AgentDetailDto["skills"]> = {}): AgentDetailDto {
  return {
    id: "jamie",
    name: "Jamie",
    role: "Growth",
    source: "overlay",
    editable: ["name", "role", "description", "tools", "skills"],
    isOrchestrator: false,
    tools: {
      requested: [],
      companyAllow: [],
      deskAllow: [],
      deskCeilingActive: false,
      effective: [],
    },
    skills: {
      requested: null,
      companyAvailable: ENABLED,
      effective: ENABLED,
      overridden: false,
      ...skills,
    },
    desks: [],
    inboxEnabled: false,
  };
}

function clientFor(agent: AgentDetailDto, updateAgent = vi.fn()) {
  return {
    getAgent: vi.fn(() => Promise.resolve(agent)),
    listTeam: vi.fn(() => Promise.resolve([])),
    updateAgent,
    get: vi.fn(() => Promise.resolve({ role: "admin" })),
    listHarnesses: vi.fn(() => Promise.resolve([])),
    put: vi.fn(),
    scopeFor: () => "/api/v1/company",
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

function node(testid: string) {
  const el = container.querySelector(`[data-testid="${testid}"]`);
  if (!el) throw new Error(`no \`${testid}\` in the rendered page`);
  return el;
}

async function click(testid: string) {
  await act(async () => {
    node(testid).dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

/** Base UI's `Switch.Root` renders a `<span role="switch">`, so `aria-checked`
 *  is where its state actually lives. */
function switchIsOn(slug: string) {
  return node(`agent-skill-toggle-${slug}`).getAttribute("aria-checked") === "true";
}

async function show(client: OpenCompanyClient) {
  // The Skills card lives on the Tools tab, and `PageTabPanel` renders nothing
  // for an inactive tab, so the address has to open that tab before the mount.
  window.location.hash = "#/team/jamie?tab=tools";
  await act(async () => {
    root.render(
      createElement(AgentDetailView, {
        client,
        company: null,
        agentId: "jamie",
        onBack: () => {},
      }),
    );
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
  window.location.hash = "";
  vi.restoreAllMocks();
});

describe("narrowing an inherited skill scope", () => {
  it("saves every other enabled skill when the first switch is turned off, not just the ones flicked", async () => {
    const agent = detail();
    const updateAgent = vi.fn((_id: string, body: { skills: string[] | null }) =>
      Promise.resolve(
        detail({ requested: body.skills, effective: body.skills ?? ENABLED }),
      ),
    );
    await show(clientFor(agent, updateAgent));

    await click("agent-skills-edit");
    // Inheriting: every switch reads on, though the stored scope is `null`.
    for (const slug of ENABLED) {
      expect(switchIsOn(slug), `${slug} reads on while the scope is inherited`).toBe(true);
    }

    await click("agent-skill-toggle-invoicing");
    expect(switchIsOn("invoicing")).toBe(false);
    expect(switchIsOn("brand-voice"), "the untouched switches stay on").toBe(true);
    expect(switchIsOn("web-research")).toBe(true);

    await click("agent-skills-save");
    expect(updateAgent).toHaveBeenCalledWith(
      "jamie",
      { skills: ["brand-voice", "web-research"] },
      null,
    );
  });

  it("keeps narrowing from the draft once the first switch has been touched", async () => {
    const agent = detail();
    const updateAgent = vi.fn((_id: string, body: { skills: string[] | null }) =>
      Promise.resolve(
        detail({ requested: body.skills, effective: body.skills ?? ENABLED }),
      ),
    );
    await show(clientFor(agent, updateAgent));

    await click("agent-skills-edit");
    await click("agent-skill-toggle-invoicing");
    // The second flick must narrow the draft the first one produced. Falling
    // back to the enabled set here would put `invoicing` straight back.
    await click("agent-skill-toggle-brand-voice");
    expect(switchIsOn("invoicing")).toBe(false);
    expect(switchIsOn("brand-voice")).toBe(false);

    await click("agent-skills-save");
    expect(updateAgent).toHaveBeenCalledWith("jamie", { skills: ["web-research"] }, null);
  });

  it("offers no save until a switch has actually been touched", async () => {
    // Opening the editor on an inherited scope shows every switch on; saving
    // that unchanged view would store the whole enabled set as an explicit
    // list and turn an inheriting teammate into a pinned one.
    const updateAgent = vi.fn();
    await show(clientFor(detail(), updateAgent));

    await click("agent-skills-edit");
    await click("agent-skills-save");
    expect(updateAgent).not.toHaveBeenCalled();
  });
});

describe("the dropped-slug line", () => {
  it("names a stored slug the company does not have enabled", async () => {
    await show(
      clientFor(
        detail({
          requested: ["brand-voice", "retired-playbook"],
          effective: ["brand-voice"],
        }),
      ),
    );

    const dropped = node("agent-skills-dropped").textContent ?? "";
    expect(dropped).toContain("retired-playbook");
    expect(dropped, "a slug the teammate does hold is not a dropped one").not.toContain(
      "brand-voice",
    );
  });

  it("is absent when every stored slug resolves", async () => {
    await show(clientFor(detail({ requested: ["brand-voice"], effective: ["brand-voice"] })));
    expect(container.querySelector('[data-testid="agent-skills-dropped"]')).toBeNull();
  });
});

describe("restoring a scope to what was already stored", () => {
  it("offers no save once an inherited scope is flicked off and back on", async () => {
    // `touched` stays set after the second flick, so a Save gated on it alone
    // would write the enabled set out as an explicit list. That reads as a
    // no-op on screen and is not one: the teammate stops inheriting, so every
    // skill the company enables afterwards passes it by.
    const updateAgent = vi.fn();
    await show(clientFor(detail(), updateAgent));

    await click("agent-skills-edit");
    await click("agent-skill-toggle-invoicing");
    await click("agent-skill-toggle-invoicing");
    for (const slug of ENABLED) {
      expect(switchIsOn(slug), `${slug} is back on`).toBe(true);
    }

    await click("agent-skills-save");
    expect(
      updateAgent,
      "the scope on screen is the inherited one, so there is nothing to store",
    ).not.toHaveBeenCalled();
  });

  it("offers no save once an explicit scope is flicked back to its stored shape", async () => {
    const updateAgent = vi.fn();
    await show(
      clientFor(
        detail({ requested: ["brand-voice"], effective: ["brand-voice"] }),
        updateAgent,
      ),
    );

    await click("agent-skills-edit");
    await click("agent-skill-toggle-invoicing");
    await click("agent-skill-toggle-invoicing");

    await click("agent-skills-save");
    expect(updateAgent).not.toHaveBeenCalled();
  });

  it("still offers the save when the flick lands somewhere else", async () => {
    const updateAgent = vi.fn(
      (_id: string, _body: { skills: string[] | null }, _scope: null) =>
        Promise.resolve(detail()),
    );
    await show(clientFor(detail(), updateAgent));

    await click("agent-skills-edit");
    await click("agent-skill-toggle-invoicing");
    await click("agent-skill-toggle-brand-voice");
    await click("agent-skill-toggle-brand-voice");

    await click("agent-skills-save");
    const [, body] = updateAgent.mock.calls[0];
    expect([...(body.skills ?? [])].sort(), "invoicing is the only one left off").toEqual([
      "brand-voice",
      "web-research",
    ]);
  });
});

describe("the line shown when a teammate reads nothing", () => {
  it("separates a scope the company disabled from a deliberately empty one", async () => {
    await show(clientFor(detail({ requested: ["retired-playbook"], effective: [] })));
    const text = node("agent-skills-empty").textContent ?? "";
    expect(
      text,
      "the teammate asked for a skill, so calling its scope empty misreports what is stored",
    ).not.toContain("explicit empty scope");
    expect(text).toContain("none of the skills it asks for");
  });

  it("still calls a stored empty list what it is", async () => {
    await show(clientFor(detail({ requested: [], effective: [] })));
    expect(node("agent-skills-empty").textContent).toContain("explicit empty scope");
  });

  it("names the company when an inheriting teammate reads nothing", async () => {
    await show(
      clientFor(detail({ requested: null, companyAvailable: [], effective: [] })),
    );
    expect(node("agent-skills-empty").textContent).toContain("company has none enabled");
  });
});
