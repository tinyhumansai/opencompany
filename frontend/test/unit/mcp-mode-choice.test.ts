// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ApprovalMode } from "@/api/mcp-tool-policy";
import { MODE_LABELS, ModeChoice } from "@/views/mcp/McpToolPermissionsControl";

let container: HTMLDivElement;
let root: Root;

function segment(mode: ApprovalMode): HTMLButtonElement {
  const found = container.querySelector<HTMLButtonElement>(
    `[data-testid="mcp-mode-${mode}"]`,
  );
  if (!found) throw new Error(`no segment for ${mode}`);
  return found;
}

async function mount(
  value: ApprovalMode,
  onChange = vi.fn(),
  disabled = false,
) {
  await act(async () => {
    root.render(
      createElement(ModeChoice, {
        value,
        label: "What happens when delete_page is called",
        disabled,
        onChange,
      }),
    );
  });
  return onChange;
}

async function arrow(from: ApprovalMode, key: string) {
  await act(async () => {
    segment(from).dispatchEvent(
      new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }),
    );
  });
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
});

describe("the three approval modes as one control", () => {
  it("labels the segments in the words the brief chose", async () => {
    await mount("needs_approval");
    expect(segment("always_allow").textContent).toBe("Allow");
    expect(segment("needs_approval").textContent).toBe("Needs approval");
    expect(segment("blocked").textContent).toBe("Block");
  });

  it("names the group so a screen reader says which tool it decides", async () => {
    await mount("needs_approval");
    const group = container.querySelector('[role="radiogroup"]');
    expect(group?.getAttribute("aria-label")).toBe(
      "What happens when delete_page is called",
    );
  });

  it("checks exactly the mode it was given", async () => {
    await mount("blocked");
    expect(segment("always_allow").getAttribute("aria-checked")).toBe("false");
    expect(segment("needs_approval").getAttribute("aria-checked")).toBe(
      "false",
    );
    expect(segment("blocked").getAttribute("aria-checked")).toBe("true");
  });

  it("keeps one Tab stop for the group, on the checked segment", async () => {
    await mount("needs_approval");
    expect(segment("always_allow").tabIndex).toBe(-1);
    expect(segment("needs_approval").tabIndex).toBe(0);
    expect(segment("blocked").tabIndex).toBe(-1);
  });

  it("reports a click on an unchecked segment", async () => {
    const onChange = await mount("needs_approval");
    await act(async () => segment("blocked").click());
    expect(onChange).toHaveBeenCalledWith("blocked");
  });

  it("treats a click on the checked segment as no change", async () => {
    const onChange = await mount("blocked");
    await act(async () => segment("blocked").click());
    expect(onChange).not.toHaveBeenCalled();
  });

  it("moves and selects in one step, the way a native radio group does", async () => {
    const onChange = await mount("needs_approval");
    await arrow("needs_approval", "ArrowRight");
    expect(onChange).toHaveBeenCalledWith("blocked");
  });

  it("wraps at both ends rather than dead-ending", async () => {
    const forward = await mount("blocked");
    await arrow("blocked", "ArrowRight");
    expect(forward).toHaveBeenCalledWith("always_allow");

    const back = await mount("always_allow");
    await arrow("always_allow", "ArrowLeft");
    expect(back).toHaveBeenCalledWith("blocked");
  });

  it("ignores keys that are not arrows", async () => {
    const onChange = await mount("needs_approval");
    await arrow("needs_approval", "Enter");
    expect(onChange).not.toHaveBeenCalled();
  });

  it("answers nothing at all when writes are not this viewer's", async () => {
    const onChange = await mount("needs_approval", vi.fn(), true);
    expect(segment("blocked").disabled).toBe(true);
    await act(async () => segment("blocked").click());
    await arrow("needs_approval", "ArrowRight");
    expect(onChange).not.toHaveBeenCalled();
  });

  it("carries one label set, so a row and a tier default cannot drift apart", () => {
    expect(Object.keys(MODE_LABELS)).toEqual([
      "always_allow",
      "needs_approval",
      "blocked",
    ]);
  });
});
