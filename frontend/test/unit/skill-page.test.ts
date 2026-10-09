// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { Skill, SkillAgentScope } from "@/api/skills";
import type { TeamMemberDto } from "@/api/types";
import { SkillPage } from "@/views/skills/SkillPage";

/**
 * The skill panel's write, which is the part of it that can lose data.
 *
 * Scoping one skill to N teammates is N `PATCH …/team/{agentId}` calls, each
 * carrying that teammate's WHOLE skill list. Every failure this suite pins is a
 * write the host accepts happily:
 *
 *  1. a body of `[thisSkill]` is a legal narrowing, so ticking one teammate can
 *     strip every other skill it has while the toast says saved;
 *  2. a call for a teammate nobody touched is a scope pinned by accident —
 *     `null` becomes an explicit list and the teammate stops inheriting;
 *  3. a partial failure is permanent, so the count in the message has to come
 *     from what actually returned 2xx rather than from what was attempted.
 *
 * The call-count assertions therefore assert the **bodies** too. "One call per
 * changed teammate" passes while every body is wrong.
 */

const CEILING = ["brand-voice", "invoicing", "web-research"];

function scope(
  id: string,
  state: SkillAgentScope["state"],
  holds = state !== "excluded",
) {
  return { id, state, holds } satisfies SkillAgentScope;
}

function skill(
  agents: SkillAgentScope[] | undefined,
  over: Partial<Skill> = {},
): Skill {
  return {
    id: "brand-voice",
    name: "Brand Voice",
    description: "How we sound.",
    category: "Content",
    source: "custom",
    enabled: true,
    agents,
    ...over,
  };
}

/** A roster row carrying that teammate's stored list. */
function member(id: string, requested: string[] | null): TeamMemberDto {
  return {
    id,
    role: "Worker",
    skills: {
      requested,
      companyAvailable: CEILING,
      effective: requested ?? CEILING,
      overridden: false,
    },
  } as TeamMemberDto;
}

let container: HTMLDivElement;
let root: Root;

function clientWith(updateAgent: unknown = vi.fn(() => Promise.resolve({}))) {
  return {
    updateAgent,
    // The page's playbook panel reads the document on mount. Answered so these
    // tests exercise the scope picker rather than the panel's failure state.
    scopeFor: () => "/api/v1/companies/acme",
    get: () =>
      Promise.resolve({
        slug: "brand-voice",
        markdown:
          "---\nname: Brand Voice\ndescription: How we sound.\n---\nStep one.\n",
        editable: true,
      }),
  } as unknown as OpenCompanyClient;
}

async function open(
  subject: Skill | null,
  team: TeamMemberDto[] | null,
  canManage = true,
  client: OpenCompanyClient = clientWith(),
  handlers: { onClose?: () => void; onSaved?: () => void } = {},
) {
  await act(async () => {
    root.render(
      createElement(SkillPage, {
        client,
        company: null,
        skill: subject,
        team,
        canManage,
        onClose: handlers.onClose ?? (() => {}),
        onSaved: handlers.onSaved ?? (() => {}),
      }),
    );
  });
}

/** Looked up on `document`, so the same helper reads a portal or a page. */
function node(testid: string): HTMLElement {
  const el = document.querySelector(`[data-testid="${testid}"]`);
  if (!el) throw new Error(`no \`${testid}\` in the rendered panel`);
  return el as HTMLElement;
}

function has(testid: string): boolean {
  return document.querySelector(`[data-testid="${testid}"]`) !== null;
}

async function click(testid: string) {
  await act(async () => {
    node(testid).dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

/** A real click, so React's own change tracking sees the box move. */
async function tick(agentId: string, on: boolean) {
  const box = node(`skill-agent-toggle-${agentId}`) as HTMLInputElement;
  if (box.checked === on)
    throw new Error(`\`${agentId}\` is already ${on ? "on" : "off"}`);
  await act(async () => {
    box.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

/** Reveals the per-teammate list, which an all-ticked skill hides behind "All agents". */
async function reveal() {
  await click("skill-detail-mode-selected");
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

describe("all agents means inheriting, not a list naming everybody", () => {
  /** A roster row whose display name is its id, so a warning naming one is readable. */
  function named(id: string, requested: string[] | null): TeamMemberDto {
    return { ...member(id, requested), name: id } as TeamMemberDto;
  }

  /** Picks "All agents", which is a choice rather than a state here. */
  async function chooseAll() {
    await act(async () => {
      node("skill-detail-mode-all").dispatchEvent(
        new MouseEvent("click", { bubbles: true }),
      );
    });
  }

  it("hands every pinned teammate back to inheriting", async () => {
    // The defect: a teammate holding a fixed list keeps missing whatever is
    // installed next. `null` is the only body that restores the dynamic state.
    const updateAgent = vi.fn(() => Promise.resolve({}));
    await open(
      skill([
        scope("ceo", "inherited"),
        scope("writer", "included"),
        scope("analyst", "excluded"),
      ]),
      [
        named("ceo", null),
        named("writer", ["brand-voice"]),
        named("analyst", []),
      ],
      true,
      clientWith(updateAgent),
    );

    await chooseAll();
    await click("skill-detail-save");

    expect(updateAgent).toHaveBeenCalledTimes(2);
    expect(updateAgent).toHaveBeenCalledWith("writer", { skills: null }, null);
    expect(updateAgent).toHaveBeenCalledWith("analyst", { skills: null }, null);
  });

  it("writes nothing for a teammate that already inherits", async () => {
    const updateAgent = vi.fn(() => Promise.resolve({}));
    await open(
      skill([scope("ceo", "inherited"), scope("writer", "inherited")]),
      [member("ceo", null), member("writer", null)],
      true,
      clientWith(updateAgent),
    );

    // Nothing to unpin, so Save has no work and is refused.
    expect(
      (node("skill-detail-save") as HTMLButtonElement).disabled,
      "nothing to do",
    ).toBe(true);
    expect(updateAgent).not.toHaveBeenCalled();
  });

  it("offers Save although no checkbox moved", async () => {
    // Every box is already ticked in all-mode, so a gate keyed on a moved
    // checkbox would disable Save exactly when there is work to do.
    await open(
      skill([scope("ceo", "inherited"), scope("writer", "included")]),
      [member("ceo", null), member("writer", ["brand-voice"])],
      true,
      clientWith(),
    );
    expect((node("skill-detail-save") as HTMLButtonElement).disabled).toBe(
      false,
    );
  });

  it("names each teammate and what it gains before the save", async () => {
    await open(
      skill([scope("ceo", "inherited"), scope("writer", "included")]),
      [named("ceo", null), named("writer", ["brand-voice"])],
      true,
      clientWith(),
    );

    const warning = node("skill-detail-widening-warning");
    expect(warning.textContent).toContain("writer");
    // `writer` stores only brand-voice, so inheriting hands it the rest.
    expect(warning.textContent).toContain("invoicing");
    expect(warning.textContent).toContain("web-research");
    // The teammate that already inherits gains nothing and is not listed.
    expect(warning.textContent).not.toContain("ceo");
  });

  it("says nothing about widening when no teammate gains anything", async () => {
    await open(
      skill([scope("ceo", "inherited"), scope("writer", "included")]),
      [named("ceo", null), named("writer", CEILING)],
      true,
      clientWith(),
    );
    expect(has("skill-detail-widening-warning")).toBe(false);
  });

  it("still narrows one teammate at a time once the list is revealed", async () => {
    // Revealing drops out of all-mode, so the per-teammate arithmetic is
    // unchanged and a tick still writes a list rather than `null`.
    const updateAgent = vi.fn(() => Promise.resolve({}));
    await open(
      skill([scope("ceo", "inherited"), scope("writer", "included")]),
      [member("ceo", null), member("writer", ["brand-voice"])],
      true,
      clientWith(updateAgent),
    );

    await reveal();
    await tick("writer", false);
    await click("skill-detail-save");

    expect(updateAgent).toHaveBeenCalledTimes(1);
    const [id, body] = updateAgent.mock.calls[0] as unknown as [
      string,
      { skills: string[] | null },
    ];
    expect(id).toBe("writer");
    expect(body.skills, "a list, not a reset").not.toBeNull();
    expect(body.skills).not.toContain("brand-voice");
  });
});

describe("the write the panel sends", () => {
  it("adds the skill to a teammate's stored list rather than replacing it", async () => {
    // THE data-losing bug. `analyst` stores `["invoicing","web-research"]`, and
    // a body of `["brand-voice"]` is a narrowing the host stores without
    // complaint — the teammate loses two skills, the toast says saved, and the
    // panel looks right afterwards.
    const updateAgent = vi.fn(() => Promise.resolve({}));
    await open(
      skill([scope("analyst", "excluded")]),
      [member("analyst", ["invoicing", "web-research"])],
      true,
      clientWith(updateAgent),
    );

    await tick("analyst", true);
    await click("skill-detail-save");

    expect(updateAgent).toHaveBeenCalledTimes(1);
    expect(updateAgent).toHaveBeenCalledWith(
      "analyst",
      { skills: ["invoicing", "web-research", "brand-voice"] },
      null,
    );
  });

  it("materialises an inherited teammate to the ceiling minus this skill", async () => {
    const updateAgent = vi.fn(() => Promise.resolve({}));
    await open(
      skill([scope("ceo", "inherited")]),
      [member("ceo", null)],
      true,
      clientWith(updateAgent),
    );

    await reveal();
    await tick("ceo", false);
    await click("skill-detail-save");

    expect(updateAgent).toHaveBeenCalledWith(
      "ceo",
      { skills: ["invoicing", "web-research"] },
      null,
    );
  });

  it("writes one teammate per change and nothing for the rest", async () => {
    const updateAgent = vi.fn(() => Promise.resolve({}));
    await open(
      skill([
        scope("ceo", "inherited"),
        scope("writer", "included"),
        scope("analyst", "excluded"),
      ]),
      [
        member("ceo", null),
        member("writer", ["brand-voice"]),
        member("analyst", ["invoicing"]),
      ],
      true,
      clientWith(updateAgent),
    );

    await reveal();
    await tick("analyst", true);
    await click("skill-detail-save");

    expect(updateAgent).toHaveBeenCalledTimes(1);
    const [id, body] = updateAgent.mock.calls[0] as unknown as [
      string,
      { skills: string[] },
    ];
    expect(id, "the two teammates nobody touched are not written").toBe(
      "analyst",
    );
    expect(body.skills).toEqual(["invoicing", "brand-voice"]);
  });

  it("offers no save until a checkbox actually moves", async () => {
    const updateAgent = vi.fn(() => Promise.resolve({}));
    await open(
      skill([scope("ceo", "inherited")]),
      [member("ceo", null)],
      true,
      clientWith(updateAgent),
    );

    await reveal();
    // Revealing the list and saving it unchanged would pin every inheriting
    // teammate to today's set on a click that changed nothing on screen.
    expect((node("skill-detail-save") as HTMLButtonElement).disabled).toBe(
      true,
    );
    await click("skill-detail-save");
    expect(updateAgent).not.toHaveBeenCalled();
  });

  it("offers no save once a checkbox is moved back to where it started", async () => {
    const updateAgent = vi.fn(() => Promise.resolve({}));
    await open(
      skill([scope("ceo", "inherited")]),
      [member("ceo", null)],
      true,
      clientWith(updateAgent),
    );

    await reveal();
    await tick("ceo", false);
    await tick("ceo", true);
    await click("skill-detail-save");
    expect(updateAgent).not.toHaveBeenCalled();
  });
});

describe("a partial failure", () => {
  it("stops at the first refusal, keeps the unsent change ticked, and counts honestly", async () => {
    // Three changes, the second refused. Pressing on would multiply the damage
    // and the third is unsent — so the message has to say 1, not 3, and the
    // panel has to stay open with the two that did not land still ticked.
    const updateAgent = vi.fn((id: string) =>
      id === "writer"
        ? Promise.reject(new Error("forbidden"))
        : Promise.resolve({}),
    );
    const onClose = vi.fn();
    const onSaved = vi.fn();
    await open(
      skill([
        scope("ceo", "inherited"),
        scope("writer", "inherited"),
        scope("analyst", "inherited"),
      ]),
      [
        member("ceo", null),
        { ...member("writer", null), name: "Wanda Writer" } as TeamMemberDto,
        member("analyst", null),
      ],
      true,
      clientWith(updateAgent),
      { onClose, onSaved },
    );

    await reveal();
    await tick("ceo", false);
    await tick("writer", false);
    await tick("analyst", false);
    await click("skill-detail-save");

    expect(
      updateAgent.mock.calls.map(([id]) => id),
      "the third is never sent",
    ).toEqual(["ceo", "writer"]);
    const message = node("skill-detail-problem").textContent ?? "";
    expect(
      message,
      "the count is what returned 2xx, not what was attempted",
    ).toContain("Scoped 1 of 3");
    expect(message).toContain("Wanda Writer");
    expect(message).toContain("forbidden");
    expect(message).toContain("The other 1 was not changed");
    expect(
      onClose,
      "the panel stays open so the operator can save again",
    ).not.toHaveBeenCalled();
    expect(
      onSaved,
      "both reads refetch, so what is ticked is what is stored",
    ).toHaveBeenCalled();
    // The two that did not land are still ticked off where the operator put them.
    expect(
      (node("skill-agent-toggle-writer") as HTMLInputElement).checked,
    ).toBe(false);
    expect(
      (node("skill-agent-toggle-analyst") as HTMLInputElement).checked,
    ).toBe(false);
  });

  it("names the refused teammate rather than its id", async () => {
    const updateAgent = vi.fn(() => Promise.reject(new Error("forbidden")));
    await open(
      skill([scope("writer", "inherited")]),
      [{ ...member("writer", null), name: "Wanda Writer" } as TeamMemberDto],
      true,
      clientWith(updateAgent),
    );

    await reveal();
    await tick("writer", false);
    await click("skill-detail-save");

    const message = node("skill-detail-problem").textContent ?? "";
    expect(message).toContain("Wanda Writer");
    expect(message).not.toContain("writer:");
  });

  it("closes and reports nothing when every write lands", async () => {
    const onClose = vi.fn();
    await open(
      skill([scope("ceo", "inherited")]),
      [member("ceo", null)],
      true,
      clientWith(),
      { onClose },
    );

    await reveal();
    await tick("ceo", false);
    await click("skill-detail-save");

    expect(onClose).toHaveBeenCalled();
    expect(has("skill-detail-problem")).toBe(false);
  });
});

describe("the four states the picker has to tell apart", () => {
  it("says the host cannot report the scope rather than showing an empty picker", async () => {
    await open(skill(undefined), [member("ceo", null)]);
    expect(has("skill-detail-no-scope")).toBe(true);
    expect(has("skill-detail-agents"), "no picker at all").toBe(false);
  });

  it("says the company has no teammates rather than that nobody is scoped", async () => {
    await open(skill([]), []);
    expect(has("skill-detail-empty-roster")).toBe(true);
    expect(has("skill-detail-agents")).toBe(false);
  });

  it("renders read-only for a member, with no checkboxes and no save", async () => {
    await open(
      skill([scope("ceo", "inherited")]),
      [member("ceo", null)],
      false,
    );
    expect(has("skill-detail-read-only")).toBe(true);
    expect(has("skill-detail-agents"), "the scope is still readable").toBe(
      true,
    );
    expect(has("skill-agent-toggle-ceo")).toBe(false);
    expect(has("skill-detail-save")).toBe(false);
  });

  it("says a switched-off skill reaches nobody while keeping the scope editable", async () => {
    await open(skill([scope("ceo", "inherited", false)], { enabled: false }), [
      member("ceo", null),
    ]);
    expect(has("skill-detail-disabled-note")).toBe(true);
    expect(
      has("skill-detail-mode"),
      "the scope is still real and still editable",
    ).toBe(true);
  });

  it("refuses the write when the host does not report the stored lists", async () => {
    // The panel can display the scope from `agents` alone and cannot change one:
    // without a teammate's stored list the next list is unknowable, and guessing
    // it is exactly the data-losing write.
    const updateAgent = vi.fn(() => Promise.resolve({}));
    await open(
      skill([scope("ceo", "inherited")]),
      null,
      true,
      clientWith(updateAgent),
    );

    expect(has("skill-detail-no-stored-lists")).toBe(true);
    await reveal();
    await tick("ceo", false);
    expect((node("skill-detail-save") as HTMLButtonElement).disabled).toBe(
      true,
    );
    await click("skill-detail-save");
    expect(updateAgent).not.toHaveBeenCalled();
  });
});

describe("what the panel says before it pins a scope", () => {
  it("warns when unticking a teammate that inherits every skill", async () => {
    await open(skill([scope("ceo", "inherited")]), [member("ceo", null)]);
    await reveal();
    expect(has("skill-detail-pin-warning"), "nothing has moved yet").toBe(
      false,
    );
    await tick("ceo", false);
    const warning = node("skill-detail-pin-warning").textContent ?? "";
    expect(warning).toContain("pin its list to today's set");
  });

  it("does not warn about a teammate that already carries a list", async () => {
    await open(skill([scope("writer", "included")]), [
      member("writer", ["brand-voice"]),
    ]);
    await reveal();
    await tick("writer", false);
    expect(has("skill-detail-pin-warning")).toBe(false);
  });

  it("offers no reset-to-inherit, only the link to the teammate's page", async () => {
    // Expressing "reset to inherit" from one skill would decide on the
    // operator's behalf about every slug this panel is not showing. A deliberate
    // asymmetry with the teammate page, not an omission.
    await open(skill([scope("ceo", "inherited")]), [member("ceo", null)]);
    await reveal();
    expect(document.body.textContent ?? "").not.toContain(
      "Reset to every skill",
    );
    expect(node("skill-agent-link-ceo").getAttribute("href")).toContain("ceo");
  });
});

describe("what a teammate's row says", () => {
  /** A roster row that actually carries a display name. */
  function named(id: string, name: string): TeamMemberDto {
    return { ...member(id, null), name } as TeamMemberDto;
  }

  it("names the teammate rather than printing its id", async () => {
    await open(skill([scope("writer", "inherited")]), [
      named("writer", "Wanda"),
    ]);
    await reveal();
    const row = node("skill-agent-toggle-writer").closest("label");
    expect(row?.textContent).toContain("Wanda");
    expect(row?.textContent, "the id is not the label").not.toContain("writer");
  });

  it("carries the mascot hashed from the teammate's id", async () => {
    await open(skill([scope("writer", "inherited")]), [
      named("writer", "Wanda"),
    ]);
    await reveal();
    const face = node("skill-agent-toggle-writer")
      .closest("label")
      ?.querySelector("img");
    expect(face?.getAttribute("src") ?? "").toContain("blob-");
  });

  it("reads the reach verdict off what is stored, not off the tick", async () => {
    // A tick is an intent until it is saved. Labelling it `Not reached` the
    // instant the box moves would report a scope the host has not been told
    // about — and the faces in the list behind this page would disagree.
    await open(skill([scope("writer", "inherited")]), [
      named("writer", "Wanda"),
    ]);
    await reveal();
    expect(node("skill-agent-reach-writer").textContent).toBe("Reached");
    await tick("writer", false);
    expect(
      node("skill-agent-reach-writer").textContent,
      "still reached until the write lands",
    ).toBe("Reached");
  });

  it("says not reached for a teammate the scope excludes", async () => {
    await open(skill([scope("writer", "excluded")]), [
      named("writer", "Wanda"),
    ]);
    expect(node("skill-agent-reach-writer").textContent).toBe("Not reached");
  });
});
