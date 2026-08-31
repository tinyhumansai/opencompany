// @vitest-environment jsdom
//
// The Add-teammate dialog must send the persona it collected (issue #1776).
//
// `AGENT_FIELDS` has rendered an Instructions box in this dialog since #264,
// and the host has accepted `instructions` at creation since #1530 — but
// `AddMemberFields` never carried the value between them, so an operator who
// wrote a persona here watched it disappear on Add. Nothing failed and nothing
// warned; the teammate was simply created without it.
//
// # Why this is a component test
//
// The gap was in the wiring, not in a function: every piece was individually
// correct, and only the path from the box to the request was missing. A test of
// any one helper would have passed before the fix. This drives the dialog the
// way an operator does and asserts on what left for the host — which is the
// only place the omission was ever visible.
//
// It matters more now than it did: #1776 puts a copilot under that box, and a
// drafted persona thrown away on Add would be a worse failure than a typed one.

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

function fakeClient(): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    listTeam: async () => ROSTER,
    addTeamMember: async (input: Record<string, unknown>) => {
      added.push(input);
      return { id: "growth", name: "Growth", role: "Growth Marketer" } as TeamMemberDto;
    },
  } as unknown as OpenCompanyClient;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  added = [];
  vi.clearAllMocks();
  api.listTasks.mockResolvedValue([]);
  api.fetchBoardColumns.mockResolvedValue([]);
  api.fetchMe.mockResolvedValue({ id: "u1", role: "admin" });
  api.listPeople.mockResolvedValue([]);
  // The dialog reads this to gate the copilot; an offline brain keeps the
  // control disabled and out of the way of what this file is about.
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

describe("adding a teammate (issue #1776)", () => {
  it("sends the persona the dialog collected", async () => {
    await act(async () => {
      root.render(
        createElement(TeamView, {
          client: fakeClient(),
          company: "acme",
          sub: null,
          onOpenAgent: vi.fn(),
          refreshKey: 0,
          onRunSetup: vi.fn(),
          onManageDesks: vi.fn(),
          onNavigateToDesk: vi.fn(),
        }),
      );
    });

    click(byText("button", "Add teammate"));
    type("member-name", "Growth");
    type("member-role", "Growth Marketer");
    type("member-description", "Owns paid acquisition and reports on ROAS.");
    type(
      "member-instructions",
      "Confirm the budget before launching a campaign. Flag anything under 2x.",
    );

    // The footer's Add teammate — the dialog is open, so it is the last one.
    const buttons = Array.from(document.querySelectorAll<HTMLElement>("button")).filter(
      (el) => el.textContent?.trim() === "Add teammate",
    );
    await act(async () => {
      buttons[buttons.length - 1].dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(added).toHaveLength(1);
    expect(added[0].instructions).toBe(
      "Confirm the budget before launching a campaign. Flag anything under 2x.",
    );
    expect(added[0].role).toBe("Growth Marketer");
  });

  /// At creation there is no blueprint to override, so an untouched box means
  /// "no persona" — not an empty one stored as an override.
  it("leaves the persona off the wire when the box was never filled in", async () => {
    await act(async () => {
      root.render(
        createElement(TeamView, {
          client: fakeClient(),
          company: "acme",
          sub: null,
          onOpenAgent: vi.fn(),
          refreshKey: 0,
          onRunSetup: vi.fn(),
          onManageDesks: vi.fn(),
          onNavigateToDesk: vi.fn(),
        }),
      );
    });

    click(byText("button", "Add teammate"));
    type("member-name", "Growth");
    type("member-role", "Growth Marketer");

    const buttons = Array.from(document.querySelectorAll<HTMLElement>("button")).filter(
      (el) => el.textContent?.trim() === "Add teammate",
    );
    await act(async () => {
      buttons[buttons.length - 1].dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(added).toHaveLength(1);
    expect(added[0].instructions).toBeUndefined();
  });
});
