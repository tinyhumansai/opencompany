// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useDebouncedValue } from "@/hooks/use-debounced-value";

let container: HTMLDivElement;
let root: Root;
let lastValue: string | null;

function Probe({ value, delayMs }: { value: string; delayMs: number }) {
  lastValue = useDebouncedValue(value, delayMs);
  return null;
}

async function render(value: string, delayMs: number) {
  await act(async () => {
    root.render(createElement(Probe, { value, delayMs }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  lastValue = null;
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  container.remove();
  vi.useRealTimers();
});

describe("useDebouncedValue", () => {
  it("returns the initial value immediately", async () => {
    await render("", 300);
    expect(lastValue).toBe("");
  });

  it("swallows keystrokes made inside the window and only lands the final value once", async () => {
    vi.useFakeTimers();
    await render("", 300);

    // Type "a" -> "ab" -> "abc" with no time advancing between changes: every
    // change restarts the window, so nothing settles yet.
    await render("a", 300);
    await render("ab", 300);
    await render("abc", 300);
    expect(lastValue).toBe("");

    // The pause the user finally takes — the debounced value catches up to the
    // last thing typed, and only the last thing.
    await act(async () => {
      vi.advanceTimersByTime(300);
    });
    expect(lastValue).toBe("abc");
  });

  it("restarts the window when the value changes mid-flight", async () => {
    vi.useFakeTimers();
    await render("", 300);

    await render("a", 300);
    // Almost there, but not quite — then a fresh keystroke lands.
    await act(async () => {
      vi.advanceTimersByTime(200);
    });
    expect(lastValue).toBe("");

    await render("ab", 300);
    // The earlier window would have fired at 300ms total; because "ab"
    // restarted it, 200ms more is not enough.
    await act(async () => {
      vi.advanceTimersByTime(200);
    });
    expect(lastValue).toBe("");

    // Only once the *new* window completes does the settled value appear.
    await act(async () => {
      vi.advanceTimersByTime(100);
    });
    expect(lastValue).toBe("ab");
  });
});
