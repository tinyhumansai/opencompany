// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { TurnStep } from "@/api/types";
import { WorkingIndicator } from "@/views/room/WorkingIndicator";

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  // Same declaration the other jsdom specs make: without it React warns on
  // every `act` and the real assertions get lost in the noise.
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  // tinysweeper: `act` returns a Promise in React 18; awaiting it is what
  // flushes an unmount's own effects before the next test's `beforeEach`
  // creates a fresh root, matching this repo's other jsdom specs (e.g.
  // `chat-thread-avatar.test.ts`).
  await act(() => root.unmount());
  container.remove();
});

async function render(props: Parameters<typeof WorkingIndicator>[0]) {
  // tinysweeper: unawaited, a render's own effects (the reduced-motion
  // listener setup) are not guaranteed to have flushed before the assertion
  // below reads `textContent`, which is exactly the flakiness this file's
  // own regression story warns about.
  await act(() => root.render(createElement(WorkingIndicator, props)));
  return container.querySelector("[data-testid='working-indicator']")?.textContent ?? "";
}

const step = (status: TurnStep["status"]): TurnStep => ({
  kind: "tool_call",
  label: "Reading the ledger",
  status,
});

/**
 * The line a reloaded console shows while a turn it never sent is running.
 *
 * Written against a real miss. `name` was added to this component's prop *type*
 * but not to its destructured parameters, so the identifier resolved to the
 * global `window.name` — a string, so it type-checked, and empty, so it was
 * falsy and the line silently stayed "Working…". Nothing failed; the feature
 * just did not appear. A rendering test is the only thing that catches that.
 */
describe("the working line names the teammate", () => {
  it("names whoever the host recorded", async () => {
    expect(await render({ srLabel: "Replying…", name: "Amendments" })).toContain(
      "Amendments is working…",
    );
  });

  it("says the generic line when nobody was recorded", async () => {
    expect(await render({ srLabel: "Replying…" })).toContain("Working…");
  });

  /**
   * The regression above, stated as itself: an unbound `name` reads the global,
   * which is `""`. Pinning the empty string keeps the fallback honest whichever
   * way the value arrives.
   */
  it("says the generic line for an empty name rather than a naked verb", async () => {
    const text = await render({ srLabel: "Replying…", name: "" });
    expect(text).toContain("Working…");
    expect(text).not.toContain(" is working…");
  });

  /**
   * A running step is more specific and more current than the seat's name, so
   * it wins. The name is what fills the gaps — before the first step, and
   * between one settling and the next starting.
   */
  it("lets a running step outrank the name", async () => {
    const text = await render({
      srLabel: "Replying…",
      name: "Amendments",
      steps: [step("running")],
    });
    expect(text).toContain("Reading the ledger");
    expect(text).not.toContain("Amendments is working…");
  });

  it("falls back to the name once every step has settled", async () => {
    expect(
      await render({ srLabel: "Replying…", name: "Amendments", steps: [step("ok")] }),
    ).toContain("Amendments is working…");
  });

  /**
   * Queued outranks everything: the turn is waiting on the company's serial
   * lock, so naming a seat that has not started would imply progress that is
   * not happening — the same reason `queued` already outranks a step label.
   */
  it("keeps the queued wording even when a seat is named", async () => {
    const text = await render({ srLabel: "Replying…", name: "Amendments", queued: true });
    expect(text).not.toContain("Amendments is working…");
  });

  /**
   * CodeRabbit: the visible line names the teammate; the assistive text must
   * too, or a screen-reader user never learns who is answering — only sighted
   * readers did. Checked on the `sr-only` node specifically, so a future
   * `textContent` match against the aria-hidden twin cannot paper over a
   * regression here the way a combined-text assertion could.
   */
  it("names the teammate in the accessible label too, not only the visible one", async () => {
    await render({ srLabel: "Replying…", name: "Amendments" });
    const srOnly = container.querySelector(".sr-only")?.textContent ?? "";
    expect(srOnly).toBe("Amendments is working…");
  });

  /**
   * The sr-only text is deliberately stable across step transitions (module
   * doc); it must not start following the name mid-step and read
   * "Amendments is working…" while the visible line — and the live step
   * timeline beside it — are naming a specific step instead.
   */
  it("keeps the accessible label on the running step, not the name, while one runs", async () => {
    await render({
      srLabel: "Replying…",
      name: "Amendments",
      steps: [step("running")],
    });
    const srOnly = container.querySelector(".sr-only")?.textContent ?? "";
    expect(srOnly).toBe("Replying…");
  });
});
