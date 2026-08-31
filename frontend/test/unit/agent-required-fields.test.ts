// @vitest-environment jsdom
//
// A required field says so, and a blank one is marked (issue #1776).
//
// The rule — name and role are required — was never wrong. It was invisible: a
// manifest teammate carries no name of its own (every card renders
// `name || role`), so the edit form opened with Role filled, Name blank, and
// Save already disabled, with nothing on screen naming the field responsible.
// These pin the visibility, not the rule.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { AgentFields } from "@/views/team/AgentFields";
import { draftIsValid, emptyDraft, missingRequired } from "@/lib/agent";
import type { AgentDetailDto } from "@/api/types";

let container: HTMLDivElement;
let root: Root;

function render(element: ReturnType<typeof createElement>) {
  act(() => root.render(element));
}

function field(key: string): HTMLElement | null {
  return container.querySelector(`[data-testid="agent-field-${key}"]`);
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

/** A manifest teammate: named by its role, with no name of its own. */
function manifestAgent(): AgentDetailDto {
  return {
    id: "qa_engineer",
    name: null,
    role: "QA Engineer",
    description: "Test features and catch regressions.",
    source: "manifest",
    editable: ["name", "role", "description", "instructions"],
  } as unknown as AgentDetailDto;
}

describe("required fields are visible, not just enforced", () => {
  it("names the blank required field instead of leaving a dead button unexplained", () => {
    const draft = { ...emptyDraft(), role: "QA Engineer" };
    const blocked = missingRequired(draft);

    expect(blocked.map((f) => f.key)).toEqual(["name"]);
    expect(blocked[0].label).toBe("Name");
    // The boolean the Save button reads still agrees with the list beside it.
    expect(draftIsValid(manifestAgent(), draft)).toBe(false);
  });

  /// The edit form opens on an existing teammate, so a blank Name is a real gap
  /// the moment it appears — that is the answer to "why is Save dead?".
  it("marks a blank required field once the form holds anything", () => {
    render(
      createElement(AgentFields, {
        idPrefix: "t",
        draft: { ...emptyDraft(), role: "QA Engineer" },
        onChange: () => {},
      }),
    );

    expect(field("name")?.getAttribute("aria-invalid")).toBe("true");
    expect(field("role")?.getAttribute("aria-invalid")).toBe("false");
  });

  /// A fresh Add form is blank everywhere. Painting every box red before a
  /// single keystroke is nagging, not help — the fields still say "Required".
  it("stays quiet on a form nobody has typed in yet", () => {
    render(
      createElement(AgentFields, {
        idPrefix: "t",
        draft: emptyDraft(),
        onChange: () => {},
      }),
    );

    expect(field("name")?.getAttribute("aria-invalid")).toBe("false");
    expect(
      container.querySelector('[data-testid="agent-field-required-name"]')?.textContent,
    ).toBe("Required");
  });

  /// A field this host will not accept cannot block a save it is not part of —
  /// the pre-#1530 behaviour, when `name` was overlay-only.
  it("does not require a field this host will not let you edit", () => {
    const readOnlyName = { ...manifestAgent(), editable: ["description", "instructions"] };
    const draft = { ...emptyDraft(), role: "QA Engineer" };

    expect(draftIsValid(readOnlyName as AgentDetailDto, draft)).toBe(true);

    render(
      createElement(AgentFields, {
        idPrefix: "t",
        draft,
        onChange: () => {},
        readOnly: (key: string) => key === "name",
      }),
    );
    expect(field("name")?.getAttribute("aria-invalid")).toBe("false");
    expect(container.querySelector('[data-testid="agent-field-required-name"]')).toBeNull();
  });
});
