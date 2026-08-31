// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { useStableList, type StableList } from "@/hooks/use-stable-list";

/**
 * Issue #1414: the approvals queue must not reflow under the pointer on the 5s
 * poll. `useStableList` freezes the rendered order — and holds removals — while
 * the operator is interacting with the queue, so a poll that reorders or drops
 * a card cannot slide a different card's button under an in-flight click.
 *
 * A jsdom render rather than a pure test because the invariant is "the rendered
 * order does not change across a prop update", which only a rendered list has.
 * The interaction handlers are called directly rather than dispatched as DOM
 * events — they are plain callbacks, and calling them avoids jsdom's synthetic
 * pointer-event gaps while exercising exactly the freeze/thaw the view wires up.
 */

let captured: StableList<string>;

function Harness({ items }: { items: string[] }) {
  const stable = useStableList(items);
  captured = stable;
  return createElement(
    "div",
    { "data-testid": "queue", ...stable.containerProps },
    stable.items.map((id) =>
      createElement("div", { key: id, "data-id": id }, id),
    ),
  );
}

function FocusHarness({
  items,
  disabled,
}: {
  items: string[];
  disabled: boolean;
}) {
  const stable = useStableList(items);
  captured = stable;
  return createElement(
    "div",
    { "data-testid": "queue", ...stable.containerProps },
    createElement(
      "button",
      { disabled, "data-testid": "focus-target" },
      "Decide",
    ),
    stable.items.map((id) =>
      createElement("div", { key: id, "data-id": id }, id),
    ),
  );
}

let container: HTMLDivElement;
let root: Root;

async function renderItems(items: string[]) {
  await act(async () => {
    root.render(createElement(Harness, { items }));
  });
}

/** The ids the DOM is actually showing, top to bottom. */
function domOrder(): string[] {
  return Array.from(container.querySelectorAll("[data-id]")).map((el) =>
    el.getAttribute("data-id"),
  ) as string[];
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

describe("useStableList (#1414)", () => {
  it("holds the rendered order while the pointer is over the queue", async () => {
    await renderItems(["a", "b", "c"]);
    expect(domOrder()).toEqual(["a", "b", "c"]);

    // Pointer enters the queue — the operator is now aiming a click.
    await act(async () => captured.containerProps.onPointerEnter());

    // A poll arrives that removes the top card and reorders the rest. Left to
    // React this would pull every card up under the pointer.
    await renderItems(["c", "b"]);

    // Frozen: the DOM must still show exactly what it did before the poll.
    expect(domOrder()).toEqual(["a", "b", "c"]);
  });

  it("reconciles to the latest poll once the pointer leaves", async () => {
    await renderItems(["a", "b", "c"]);
    await act(async () => captured.containerProps.onPointerEnter());
    await renderItems(["c", "b"]);
    expect(domOrder()).toEqual(["a", "b", "c"]);

    // Pointer leaves — nothing is being aimed at, so the held order is dropped
    // and the newest poll takes over.
    await act(async () => captured.containerProps.onPointerLeave());
    expect(domOrder()).toEqual(["c", "b"]);
  });

  it("thaws after a focused control is disabled without a blur event", async () => {
    await act(async () => {
      root.render(
        createElement(FocusHarness, { items: ["a", "b"], disabled: false }),
      );
    });

    const target = container.querySelector<HTMLButtonElement>(
      "[data-testid=focus-target]",
    )!;
    await act(async () => target.focus());
    expect(captured.holding).toBe(true);

    // Chromium clears activeElement when a focused button becomes disabled but
    // does not dispatch blur. The hook's post-commit check must still release
    // the hold so a later poll can reconcile.
    await act(async () => {
      root.render(
        createElement(FocusHarness, { items: ["a", "b"], disabled: true }),
      );
    });
    expect(captured.holding).toBe(false);
  });

  it("does not freeze an empty queue, so new approvals appear while the pointer rests over it", async () => {
    // The queue is empty; the operator's cursor sits over the (large) empty
    // state. A stationary cursor must not freeze the empty snapshot, or a
    // just-arrived approval would stay invisible — until its deadline — while
    // the page claims nothing needs approval.
    await renderItems([]);
    await act(async () => captured.containerProps.onPointerEnter());
    expect(captured.holding).toBe(true);
    expect(captured.items).toEqual([]);

    // A poll brings a deadline-bound approval in. It must render immediately,
    // even though the pointer never moved.
    await renderItems(["a"]);
    expect(captured.items).toEqual(["a"]);
  });

  it("freezes the instant rows arrive while the pointer is already inside", async () => {
    await renderItems([]);
    await act(async () => captured.containerProps.onPointerEnter());

    // Rows arrive under a stationary cursor — the operator is now aiming at a
    // real card, so a poll that reorders it must not slide another card's
    // button under the pointer (#1414).
    await renderItems(["a", "b"]);
    expect(domOrder()).toEqual(["a", "b"]);

    // The card the operator was aiming at is removed by a poll. Frozen: the
    // rendered order is unchanged until the pointer leaves.
    await renderItems(["b"]);
    expect(domOrder()).toEqual(["a", "b"]);

    await act(async () => captured.containerProps.onPointerLeave());
    expect(domOrder()).toEqual(["b"]);
  });
});
