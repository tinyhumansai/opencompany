// @vitest-environment jsdom
//
// The Add-agent dialog collects three things and gets out of the way.
//
// # What it replaced, and why this file changed shape
//
// This used to pin that the dialog's Instructions box reached the host (issue
// #1776), over a dialog that collected name, role, description, persona, an
// inbox switch and a daily cap — with a copilot that would design all of it
// from one sentence (#1989) and a hand-over to the long form when that design
// was refused.
//
// The dialog asks for a name, a face and a post now. An agent is not finished
// at the moment it is created, and this dialog was the only place pretending
// otherwise: the description and the persona are written on the agent's own
// page, next to the copilot that drafts them and the record it is grounded in.
// So there is no Instructions box left to pin, and the contract worth pinning
// moved.
//
// # What is worth pinning instead
//
// Three things, each of which fails silently if it breaks:
//
//   1. **The dialog asks for exactly three things.** A regression that put the
//      long form back would look correct on screen — it did, for months — and
//      nothing would report it.
//   2. **The avatar is a second write.** `addTeamMember` takes no avatar, so a
//      chosen face has to be sent as its own `updateAgent` call against the id
//      the host answers with. Miss it and the agent is created wearing the
//      hashed mascot, which looks like a face nobody chose rather than a
//      dropped write.
//   3. **It lands on the agent's page.** The dialog collects three of the
//      fields an agent has; a create that stayed on the roster would leave the
//      rest unwritten with nothing pointing at where to write them.

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
  getInferenceStatus: vi.fn(),
}));

vi.mock("@/api/tasks", () => ({ listTasks: api.listTasks }));
vi.mock("@/lib/board-columns", () => ({
  fetchBoardColumns: api.fetchBoardColumns,
  IN_FLIGHT_COLUMNS: ["planning", "in_progress"],
}));
vi.mock("@/api/auth", () => ({ me: api.fetchMe, listPeople: api.listPeople }));
vi.mock("@/api/inference", () => ({ getInferenceStatus: api.getInferenceStatus }));

const { TeamView } = await import("@/views/TeamView");

const ROSTER: TeamMemberDto[] = [
  { id: "maya", name: "Maya", role: "Research Lead", description: "Tracks competitors." },
];

let container: HTMLDivElement;
let root: Root;
let added: Array<Record<string, unknown>>;
let patched: Array<{ id: string; patch: Record<string, unknown> }>;

/**
 * jsdom ships no `matchMedia`, and the mascot avatar swatch this dialog's
 * picker now renders (`AvatarPicker`'s flavour grid,
 * `rendering-strategy.md`'s "12th tile") pulls in `@rive-app/react-canvas`,
 * whose own `useDevicePixelRatio` reaches for `matchMedia` unguarded — so
 * without this the dialog fails to mount at all. Same stub as
 * `chat-cognition-banner.test.ts` / `working-indicator.test.ts`.
 */
function stubMatchMedia() {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
      onchange: null,
    }),
  });
}

function fakeClient(): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    listTeam: async () => ROSTER,
    addTeamMember: async (input: Record<string, unknown>) => {
      added.push(input);
      return { id: "growth", name: "Growth", role: "Growth Marketer" } as TeamMemberDto;
    },
    // The avatar's own write, recorded separately because that is the point:
    // `addTeamMember` has no avatar field, so a face has to arrive here.
    updateAgent: async (id: string, patch: Record<string, unknown>) => {
      patched.push({ id, patch });
      return { id } as unknown as TeamMemberDto;
    },
  } as unknown as OpenCompanyClient;
}

beforeEach(() => {
  stubMatchMedia();
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  added = [];
  patched = [];
  vi.clearAllMocks();
  api.listTasks.mockResolvedValue([]);
  api.fetchBoardColumns.mockResolvedValue([]);
  api.fetchMe.mockResolvedValue({ id: "u1", role: "admin" });
  api.listPeople.mockResolvedValue([]);
  // Still mocked because the roster reads it, but it no longer decides which
  // dialog renders: there is only one dialog now, and it asks nothing a model
  // could draft.
  api.getInferenceStatus.mockResolvedValue({ cognition: "echo" });
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

function click(el: HTMLElement | undefined | null) {
  if (!el) throw new Error("no such element");
  act(() => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

/** Opens the dialog and flushes the roster's pending reads. */
async function openDialog() {
  click(byText("button", "Add agent"));
  await act(async () => {});
}

/** Types into a controlled input/textarea the way React sees it. */
function type(id: string, value: string) {
  const el = document.querySelector<HTMLInputElement | HTMLTextAreaElement>(`#${id}`);
  if (!el) throw new Error(`no field #${id}`);
  const proto =
    el instanceof HTMLTextAreaElement
      ? HTMLTextAreaElement.prototype
      : HTMLInputElement.prototype;
  const setter = Object.getOwnPropertyDescriptor(proto, "value")!.set!;
  act(() => {
    setter.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

/** The footer's own Add agent, which is the last one on screen while open. */
function submit() {
  const buttons = Array.from(document.querySelectorAll<HTMLElement>("button")).filter(
    (el) => el.textContent?.trim() === "Add agent",
  );
  return buttons[buttons.length - 1];
}

async function mount(onOpenAgent = vi.fn()) {
  await act(async () => {
    root.render(
      createElement(TeamView, {
        client: fakeClient(),
        company: "acme",
        sub: null,
        onOpenAgent,
        refreshKey: 0,
        onRunSetup: vi.fn(),
        onManageDesks: vi.fn(),
        onNavigateToDesk: vi.fn(),
      }),
    );
  });
  return onOpenAgent;
}

describe("the Add-agent dialog", () => {
  it("asks for a name and a post, and nothing the agent's own page owns", async () => {
    await mount();
    await openDialog();

    expect(document.querySelector("#agent-add-name")).not.toBeNull();
    expect(document.querySelector("#agent-add-role")).not.toBeNull();
    // The long form's fields, each of which now lives on the agent's page. Named
    // individually rather than counted: a count passes if one is swapped for
    // another, and the failure this guards is the long form coming back.
    expect(document.querySelector("#member-description")).toBeNull();
    expect(document.querySelector("#member-instructions")).toBeNull();
    expect(document.querySelector("#member-budget-new")).toBeNull();
  });

  it("holds Create until it has both a name and a post", async () => {
    await mount();
    await openDialog();

    expect(submit().hasAttribute("disabled")).toBe(true);
    type("agent-add-name", "Growth");
    // A name alone is not enough: the host derives the starting tool belt from
    // the post, so an agent created without one starts with nothing.
    expect(submit().hasAttribute("disabled")).toBe(true);
    type("agent-add-role", "Growth Marketer");
    expect(submit().hasAttribute("disabled")).toBe(false);
  });

  it("sends the name and the post, and nothing it no longer collects", async () => {
    await mount();
    await openDialog();
    type("agent-add-name", "Growth");
    type("agent-add-role", "Growth Marketer");
    await act(async () => {
      submit().dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(added).toHaveLength(1);
    expect(added[0].name).toBe("Growth");
    expect(added[0].role).toBe("Growth Marketer");
    // At creation there is no blueprint to override, so an unwritten field is
    // "no persona" rather than an empty one stored as an override.
    expect(added[0].instructions).toBeUndefined();
    expect(added[0].description).toBeUndefined();
    // Both retired from the console entirely, not merely from this dialog.
    expect(added[0].budgetUsdDaily).toBeUndefined();
    expect(added[0].inbox).toBeUndefined();
  });

  it("lands the operator on the new agent's page", async () => {
    const onOpenAgent = await mount();
    await openDialog();
    type("agent-add-name", "Growth");
    type("agent-add-role", "Growth Marketer");
    await act(async () => {
      submit().dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    // Against the id the host answered with, never the typed name: the roster
    // key is the host's, and navigating to a guess is a 404 on an agent that
    // was created successfully.
    expect(onOpenAgent).toHaveBeenCalledWith("growth", { edit: true });
  });

  it("writes an unchosen face as no write at all", async () => {
    await mount();
    await openDialog();
    type("agent-add-name", "Growth");
    type("agent-add-role", "Growth Marketer");
    await act(async () => {
      submit().dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    // Nobody picked one, so there is nothing to send. The roster hashes a
    // mascot from the id, and sending that back would turn "nobody chose" into
    // a stored choice — the distinction `avatarRef` exists to keep.
    expect(patched).toHaveLength(0);
  });
});
