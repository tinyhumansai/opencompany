// @vitest-environment jsdom
//
// The reduced Add-teammate dialog, and the redirect that completes it
// (issue #1989).
//
// # Why this is a component test as well as a pure one
//
// `team-add-surface.test.ts` proves the branch decision for every input. What
// it cannot prove is that the component actually *asks* — a dialog that
// hard-rendered the full form would pass every one of those cases. That is
// exactly the silent failure this redesign is exposed to: the full form looks
// precisely like the dialog did before the change, so nothing reports it.
//
// The redirect needs a component test for a second reason. The reduced dialog
// collects a name and a sentence and nothing else, so the description, the
// persona, the budget and the inbox are all still to be written — on the page
// this redirect opens, beside the copilot that drafts two of them. A create
// that lands nowhere is not a smaller dialog, it is a teammate abandoned
// half-written.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { TeamMemberDto } from "@/api/types";

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));

vi.mock("sonner", () => {
  const toast = Object.assign(toasts.base, {
    success: toasts.success,
    error: toasts.error,
    warning: toasts.warning,
    info: toasts.info,
  });
  return { toast };
});

const api = vi.hoisted(() => ({
  listTasks: vi.fn(),
  fetchBoardColumns: vi.fn(),
  fetchMe: vi.fn(),
  listPeople: vi.fn(),
  setInboxEnabled: vi.fn(),
  getInferenceStatus: vi.fn(),
}));

vi.mock("@/api/tasks", () => ({ listTasks: api.listTasks }));
vi.mock("@/lib/board-columns", () => ({
  fetchBoardColumns: api.fetchBoardColumns,
  IN_FLIGHT_COLUMNS: ["planning", "in_progress"],
}));
vi.mock("@/api/auth", () => ({ me: api.fetchMe, listPeople: api.listPeople }));
vi.mock("@/api/inbox", () => ({ setInboxEnabled: api.setInboxEnabled }));
vi.mock("@/api/inference", () => ({ getInferenceStatus: api.getInferenceStatus }));

const { TeamView } = await import("@/views/TeamView");

const ROSTER: TeamMemberDto[] = [
  { id: "maya", name: "Maya", role: "Research Lead", description: "Tracks competitors." },
];

let container: HTMLDivElement;
let root: Root;
let added: Array<Record<string, unknown>>;
let opened: Array<[string | null, { edit?: boolean } | undefined]>;

function fakeClient(): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    listTeam: async () => ROSTER,
    addTeamMember: async (input: Record<string, unknown>) => {
      added.push(input);
      return { id: "nova", name: "Nova", role: "Runs paid acquisition" } as TeamMemberDto;
    },
  } as unknown as OpenCompanyClient;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  added = [];
  opened = [];
  vi.clearAllMocks();
  api.listTasks.mockResolvedValue([]);
  api.fetchBoardColumns.mockResolvedValue([]);
  api.fetchMe.mockResolvedValue({ id: "u1", role: "admin" });
  api.listPeople.mockResolvedValue([]);
  api.getInferenceStatus.mockResolvedValue({ cognition: "harness" });
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  container.remove();
});

function byText(tag: string, text: string): HTMLElement | undefined {
  return Array.from(document.querySelectorAll<HTMLElement>(tag)).find(
    (el) => el.textContent?.trim() === text,
  );
}

/** Types into a controlled input/textarea the way React sees it. */
function type(testId: string, value: string) {
  const el = document.querySelector<HTMLInputElement | HTMLTextAreaElement>(
    `[data-testid="${testId}"]`,
  );
  if (!el) throw new Error(`no field [data-testid="${testId}"]`);
  const proto =
    el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
  const setter = Object.getOwnPropertyDescriptor(proto, "value")!.set!;
  act(() => {
    setter.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function mount() {
  await act(async () => {
    root.render(
      createElement(TeamView, {
        client: fakeClient(),
        company: "acme",
        sub: null,
        onOpenAgent: (agentId: string | null, options?: { edit?: boolean }) => {
          opened.push([agentId, options]);
        },
        refreshKey: 0,
        onRunSetup: vi.fn(),
        onManageDesks: vi.fn(),
        onNavigateToDesk: vi.fn(),
      }),
    );
  });
}

/** Opens the dialog and lets its cognition read land. */
async function openDialog() {
  await act(async () => {
    byText("button", "Add teammate")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
  await act(async () => {});
}

/** The footer's Add teammate — the dialog is open, so it is the last one. */
async function pressCreate() {
  const buttons = Array.from(document.querySelectorAll<HTMLElement>("button")).filter(
    (el) => el.textContent?.trim() === "Add teammate",
  );
  await act(async () => {
    buttons[buttons.length - 1].dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

const box = '[data-testid="team-describe-box"]';
const roleField = '[data-testid="agent-field-role"]';

describe("the reduced Add-teammate dialog (issue #1989)", () => {
  it("renders one box and no Role, Instructions, budget or inbox field", async () => {
    await mount();
    await openDialog();

    expect(document.querySelector(box), "the description box must be on screen").not.toBeNull();
    expect(
      document.querySelector('[data-testid="team-describe-name"]'),
      "a name is asked for, because nothing can derive one",
    ).not.toBeNull();

    // The fields the reduction removes. Each is either derived, or waiting on
    // the detail page this create lands on.
    expect(document.querySelector(roleField), "Role is derived, not asked for").toBeNull();
    expect(
      document.querySelector('[data-testid="agent-field-instructions"]'),
      "the persona is drafted by the copilot on the detail page",
    ).toBeNull();
    expect(
      document.querySelector('[data-testid="agent-field-description"]'),
      "the description IS the box",
    ).toBeNull();
    expect(
      document.querySelector('[data-testid="team-add-budget"]'),
      "the budget is set on the detail page",
    ).toBeNull();
    expect(
      byText("span", "Give this teammate an inbox"),
      "the inbox is toggled on the detail page",
    ).toBeUndefined();
  });

  it("creates from the box, deriving a role, and lands on the teammate's edit form", async () => {
    await mount();
    await openDialog();

    type("team-describe-name", "Nova");
    type("team-describe-box", "Runs paid acquisition, and reports on ROAS weekly.");
    await pressCreate();

    expect(added).toHaveLength(1);
    expect(added[0].name).toBe("Nova");
    // Derived from the first clause. Never blank: a blank role breaks the
    // teammate's own system prompt and switches off the copilot on the page
    // this create is about to open.
    expect(added[0].role).toBe("Runs paid acquisition");
    expect(added[0].description).toBe("Runs paid acquisition, and reports on ROAS weekly.");
    // Not collected here — the copilot drafts it on the detail page, grounded
    // in a teammate the host has actually stored.
    expect(added[0].instructions).toBeUndefined();

    // The redirect is the other half of the flow, not a flourish at the end of
    // it: `edit` is what puts the copilot on screen when the operator arrives.
    expect(opened).toEqual([["nova", { edit: true }]]);
  });

  it("hands over the full form when the description yields no role", async () => {
    await mount();
    await openDialog();

    type("team-describe-name", "Nova");
    type("team-describe-box", "...");
    await pressCreate();

    // Nothing was written. A teammate with a blank role would be a first in
    // this codebase — every other write path refuses one — and `POST /team` is
    // the one route that would have accepted it.
    expect(added).toHaveLength(0);
    expect(opened).toHaveLength(0);

    // The full form instead, carrying what was typed, so the operator can name
    // the role themselves rather than meeting a Create that cannot work.
    expect(document.querySelector(roleField), "the full form must be on screen").not.toBeNull();
    expect(document.querySelector(box), "the reduced dialog is retired").toBeNull();
    expect(
      document.querySelector<HTMLInputElement>('[data-testid="agent-field-name"]')!.value,
    ).toBe("Nova");
    expect(
      document.querySelector<HTMLTextAreaElement>('[data-testid="agent-field-description"]')!.value,
    ).toBe("...");
    expect(document.querySelector('[data-testid="team-add-handover"]')).not.toBeNull();
  });

  it("refuses to create until both the name and the box hold something", async () => {
    await mount();
    await openDialog();

    type("team-describe-box", "Runs paid acquisition.");
    await pressCreate();
    expect(added, "a nameless teammate has no id to mint").toHaveLength(0);

    type("team-describe-name", "Nova");
    type("team-describe-box", "");
    await pressCreate();
    expect(added, "an empty box derives no role").toHaveLength(0);
  });
});

describe("the full Add-teammate form on a company that cannot draft", () => {
  beforeEach(() => {
    // The operator's screenshot: "No model is configured, so the copilot can't
    // draft yet." That path keeps today's form, unchanged — hidden, never
    // deleted — so a company with no model is not locked out of writing the
    // fields nothing downstream could draft for it.
    api.getInferenceStatus.mockResolvedValue({ cognition: "echo" });
  });

  it("renders every field the dialog has always had", async () => {
    await mount();
    await openDialog();

    expect(document.querySelector(box), "the reduced dialog must NOT be on screen").toBeNull();
    for (const field of ["name", "role", "description", "instructions"]) {
      expect(
        document.querySelector(`[data-testid="agent-field-${field}"]`),
        `the full form must still render ${field}`,
      ).not.toBeNull();
    }
    expect(document.querySelector('[data-testid="team-add-budget"]')).not.toBeNull();
    expect(byText("span", "Give this teammate an inbox")).toBeDefined();
    // The hand-over note belongs to the reduced dialog's dead end. This form is
    // simply what the dialog IS here, so there is nothing to explain.
    expect(document.querySelector('[data-testid="team-add-handover"]')).toBeNull();
  });

  it("creates from the typed fields and stays on the roster", async () => {
    await mount();
    await openDialog();

    type("agent-field-name", "Nova");
    type("agent-field-role", "Growth Marketer");
    await pressCreate();

    expect(added).toHaveLength(1);
    expect(added[0].role).toBe("Growth Marketer");
    // No redirect on this path: the operator filled the fields in here, so
    // there is nothing waiting for them on the detail page.
    expect(opened).toHaveLength(0);
  });
});
