// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { TaskColumn } from "@/lib/board-columns";
import {
  BOARD_GAP,
  COLUMN_PX,
  COLUMN_WIDTH,
  GUTTER_PX,
  LedgerBoard,
} from "@/views/LedgerBoard";

/**
 * Issue #1101 — a board whose work is all in later columns reads as empty.
 *
 * The report: To-do, Planning and In progress are the three columns that fit an
 * ordinary window, and on a company that has actually shipped something they
 * are the three that are empty. The operator gets three confident zeros and no
 * hint that 101 cards are two columns off the right edge.
 *
 * The fix collapses an empty column to a rail so the populated ones fit, and
 * the whole risk of it is the board's main gesture: **empty columns are exactly
 * the columns you drag into.** To-do is where returned work lands, In progress
 * is where a card is handed to its assignee. So the claims that matter here are
 * not "it looks narrower" — they are that a rail is still a drop target, that it
 * opens under a drag, and that a column the operator pinned open stays open.
 *
 * This suite is normally for pure functions (see `vitest.config.ts`), and it
 * earns the exception the same way `task-blocked-card.test.ts` does: every
 * claim above only exists at the rendered board. A helper returning
 * `collapsed: true` would prove nothing about whether the element under the
 * pointer still takes a drop.
 */

const COLUMNS: TaskColumn[] = [
  { id: "todo", label: "To-do", closed: false },
  { id: "planning", label: "Planning", closed: false },
  { id: "in_progress", label: "In progress", closed: false },
  { id: "paused", label: "Paused", closed: false },
  { id: "in_review", label: "In review", closed: false },
  { id: "done", label: "Done", closed: true },
];

interface Row {
  id: string;
  status: string;
}

/** The board as the issue found it: everything parked in the later columns. */
function laterColumnsOnly(): Row[] {
  return [
    ...Array.from({ length: 47 }, (_, n) => ({
      id: `p${n}`,
      status: "paused",
    })),
    ...Array.from({ length: 54 }, (_, n) => ({
      id: `r${n}`,
      status: "in_review",
    })),
  ];
}

/**
 * Pins what the board's viewport measures, for the width half of the rule.
 *
 * jsdom lays nothing out, so every element reports `clientWidth: 0` — which the
 * board reads as *nothing is known to fit*, the conservative answer, and is why
 * every case below that does not touch this still exercises the collapse path
 * exactly as it did before the width gate existed.
 */
function widenViewportTo(px: number) {
  Object.defineProperty(HTMLElement.prototype, "clientWidth", {
    configurable: true,
    get: () => px,
  });
}

function restoreViewport() {
  delete (HTMLElement.prototype as unknown as Record<string, unknown>)
    .clientWidth;
}

let container: HTMLDivElement;
let root: Root;
let moves: Array<{ id: string; status: string }>;

async function render(rows: Row[], extra: { columnHeader?: boolean } = {}) {
  moves = [];
  await act(async () => {
    root.render(
      createElement(LedgerBoard<Row>, {
        columns: COLUMNS,
        rows,
        statusOf: (row) => row.status,
        renderCard: (row) => createElement("span", null, row.id),
        onMove: (row, status) => {
          moves.push({ id: row.id, status });
        },
        onMiss: () => {},
        columnHeader: extra.columnHeader
          ? (column) =>
              column.id === "todo" ? createElement("button", null, "+") : null
          : undefined,
      }),
    );
  });
}

function column(id: string): HTMLElement {
  const found = container.querySelector<HTMLElement>(`[data-column="${id}"]`);
  if (!found) throw new Error(`no ${id} column in:\n${container.innerHTML}`);
  return found;
}

const isCollapsed = (id: string) => column(id).dataset.collapsed === "true";

/** Dispatches a bare DOM event React will wrap. jsdom has no `DragEvent`. */
async function fire(target: Element, type: string) {
  await act(async () => {
    target.dispatchEvent(new Event(type, { bubbles: true, cancelable: true }));
  });
}

/** Picks a card up, so the board's `dragId` fallback holds it like a real drag. */
async function pickUp(id: string) {
  const card = Array.from(
    container.querySelectorAll<HTMLElement>("[draggable=true]"),
  ).find((held) => held.textContent === id);
  if (!card) throw new Error(`no card ${id} to pick up`);
  await fire(card, "dragstart");
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  restoreViewport();
});

/**
 * The width half of the rule (the audit's finding on #1101).
 *
 * #1101's premise is a claim about *width* — "the 101 cards you came for are
 * two columns off the right edge" — but the guard shipped as "some other column
 * has cards", which is a claim about content. A three-stage list holding one
 * row satisfied the second and not the first, and rendered as two rails of
 * rotated text beside eight hundred pixels of empty page.
 */
describe("a board with room for every column", () => {
  it("collapses nothing, however the work is distributed", async () => {
    // Six columns at the reference geometry, and then some.
    widenViewportTo(COLUMNS.length * (COLUMN_PX + GUTTER_PX) + 200);
    await render(laterColumnsOnly());

    for (const held of COLUMNS) expect(isCollapsed(held.id)).toBe(false);
  });

  it("still collapses once one column too many is declared", async () => {
    // A viewport that fits five of the six. The board has somewhere off the
    // right edge to rescue again, so the rails earn their cost.
    widenViewportTo(5 * (COLUMN_PX + GUTTER_PX));
    await render(laterColumnsOnly());

    expect(isCollapsed("todo")).toBe(true);
    expect(isCollapsed("paused")).toBe(false);
  });

  it("keeps the measured geometry and the painted geometry in step", () => {
    // `fits` does arithmetic in pixels; the columns are laid out by Tailwind
    // spacing classes. Nothing else would notice the two drifting apart —
    // the board would simply start collapsing at the wrong width.
    expect(COLUMN_WIDTH).toBe(`w-${COLUMN_PX / 4}`);
    expect(BOARD_GAP).toBe(`gap-${GUTTER_PX / 4}`);
  });
});

describe("a board whose work has moved to the later columns", () => {
  it("opens on its first populated column", async () => {
    widenViewportTo(5 * (COLUMN_PX + GUTTER_PX));
    const originalRect = HTMLElement.prototype.getBoundingClientRect;
    HTMLElement.prototype.getBoundingClientRect = function () {
      const left = this.dataset.column === "paused" ? 164 : 24;
      return {
        x: left,
        y: 0,
        width: 0,
        height: 0,
        top: 0,
        right: left,
        bottom: 0,
        left,
        toJSON: () => ({}),
      };
    };

    try {
      await render(laterColumnsOnly());

      expect(
        container.querySelector<HTMLElement>("[data-testid=ledger-board]")
          ?.scrollLeft,
      ).toBe(140);
    } finally {
      HTMLElement.prototype.getBoundingClientRect = originalRect;
    }
  });

  it("collapses the empty columns and leaves the populated ones alone", async () => {
    await render(laterColumnsOnly());

    expect(isCollapsed("todo")).toBe(true);
    expect(isCollapsed("planning")).toBe(true);
    expect(isCollapsed("in_progress")).toBe(true);
    expect(isCollapsed("done")).toBe(true);
    expect(isCollapsed("paused")).toBe(false);
    expect(isCollapsed("in_review")).toBe(false);
  });

  it("gives a rail an accessible name carrying the column and its count", async () => {
    await render(laterColumnsOnly());

    // The label is painted sideways and the count sits under it, which a screen
    // reader cannot see and would otherwise read as "To-do0". The name is the
    // one place both facts are actually available.
    const rail = column("todo").querySelector("button");
    expect(rail).not.toBeNull();
    expect(rail?.getAttribute("aria-label")).toBe("Expand To-do, 0 cards");
  });

  it("collapses nothing when the board is empty everywhere", async () => {
    // Six rails and no board is a worse answer to "show me the work" than the
    // three honest zeros this issue is about.
    widenViewportTo(5 * (COLUMN_PX + GUTTER_PX));
    await render([]);

    for (const held of COLUMNS) expect(isCollapsed(held.id)).toBe(false);
    expect(
      container.querySelector<HTMLElement>("[data-testid=ledger-board]")
        ?.scrollLeft,
    ).toBe(0);
  });

  it("keeps a column open when its header slot holds a control", async () => {
    // A rail has nowhere to put the intake `+`, so collapsing To-do would hide
    // a control rather than some whitespace.
    await render(laterColumnsOnly(), { columnHeader: true });

    expect(isCollapsed("todo")).toBe(false);
    expect(isCollapsed("planning")).toBe(true);
  });
});

describe("dragging a card into a collapsed column", () => {
  it("makes a populated target visibly ready for the drop", async () => {
    await render(laterColumnsOnly());
    await pickUp("p0");

    await fire(column("in_review"), "dragover");

    expect(column("in_review").className).toContain("border-primary");
    expect(column("in_review").className).toContain("from-accent/80");
    expect(column("in_review").className).toContain("ring-2");
  });

  it("opens the rail under the drag and takes the drop", async () => {
    await render(laterColumnsOnly());
    await pickUp("p0");

    await fire(column("in_progress"), "dragover");
    expect(isCollapsed("in_progress")).toBe(false);
    // And it reads as a landing spot rather than the same "nothing here" it
    // shows at rest.
    expect(column("in_progress").textContent).toContain("Drop it here");

    await fire(column("in_progress"), "drop");
    expect(moves).toEqual([{ id: "p0", status: "in_progress" }]);
  });

  it("folds the rail back once the drag moves on, without pinning it", async () => {
    await render(laterColumnsOnly());
    await pickUp("p0");

    await fire(column("todo"), "dragover");
    expect(isCollapsed("todo")).toBe(false);

    await fire(column("todo"), "dragleave");
    expect(isCollapsed("todo")).toBe(true);
  });

  it("leaves the other empty columns collapsed while one is hovered", async () => {
    await render(laterColumnsOnly());
    await pickUp("p0");

    await fire(column("in_progress"), "dragover");
    expect(isCollapsed("todo")).toBe(true);
    expect(isCollapsed("planning")).toBe(true);
  });
});

describe("pinning a collapsed column open", () => {
  it("opens on a click and stays open", async () => {
    await render(laterColumnsOnly());

    const rail = column("todo").querySelector("button");
    await act(async () => rail?.click());
    expect(isCollapsed("todo")).toBe(false);

    // Nothing but another click may fold it: a column that re-collapsed itself
    // while somebody was reading it would be this bug's mirror image. A drag
    // over its neighbour is the cheapest re-render to prove that with.
    await pickUp("p0");
    await fire(column("planning"), "dragover");
    await fire(column("planning"), "dragleave");
    expect(isCollapsed("todo")).toBe(false);
  });

  it("offers a control that folds it back up", async () => {
    await render(laterColumnsOnly());

    await act(async () => column("todo").querySelector("button")?.click());
    const fold = column("todo").querySelector<HTMLButtonElement>(
      'button[aria-label="Collapse To-do"]',
    );
    expect(fold).not.toBeNull();

    await act(async () => fold?.click());
    expect(isCollapsed("todo")).toBe(true);
  });

  it("offers no fold control on a column that holds work", async () => {
    await render(laterColumnsOnly());

    expect(
      column("paused").querySelector('button[aria-label="Collapse Paused"]'),
    ).toBeNull();
  });
});
