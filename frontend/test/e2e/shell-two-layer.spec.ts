import { expect, test, type Page } from "@playwright/test";

import { openFirstWorkflow } from "./workflows";

/**
 * The console's shell is two layers, not two panes (issue #1178).
 *
 * The window *chrome* is painted exactly once, on the shell root. Both the
 * sidebar column and the margin around the content card are that one surface
 * showing through, and the routed page sits on a single inset, rounded card —
 * the only opaque sheet in the shell.
 *
 * # Why these assertions and not a screenshot
 *
 * The first attempt at this issue (PR #1188) inset the frame on three sides
 * over a flat tint and shipped a hairline sliver: structurally a two-layer
 * shell, visually nothing. Every geometric assertion below therefore names a
 * quantity rather than a class — a margin of at least 8px on *all four* sides,
 * a real corner radius, a card fill measurably different from the chrome —
 * because "the class is present" is exactly what that change would have passed.
 *
 * # The single-scrim property
 *
 * The seam this layout removes comes back the moment the sidebar paints a fill
 * of its own: two separately-tinted columns meeting at a line. So the test is
 * not "the sidebar and the frame are the same colour" — two different tokens
 * that happen to resolve alike would pass that and then drift. It is "the
 * sidebar's fill and the frame's fill are the *same painted element*", which is
 * a structural fact that cannot drift.
 *
 * Both themes, because the chrome steps *down* in lightness from the sheet in
 * light and *up* in dark — the canvas is already the darkest value in the dark
 * theme, so "further back" cannot mean darker there. A regression that only
 * flattens one theme is the likely one.
 */

/** The first-run tour opens a modal over a fresh console and eats every click. */
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

/** Pin the theme before the app boots, so the first paint is the one under test. */
async function open(page: Page, theme: "dark" | "light", hash: string) {
  await page.addInitScript((value) => {
    window.localStorage.setItem("theme", value);
  }, theme);
  await page.goto(hash);
  await expect(page.locator('[data-testid="content-surface"]')).toBeVisible({ timeout: 30_000 });
  await expect(page.locator("html")).toHaveClass(new RegExp(`\\b${theme}\\b`));
}

/**
 * What the shell actually paints, read off the live document.
 *
 * `paintedBy` walks up from an element until it finds one with a non-transparent
 * `background-color` — the element whose fill the viewer is really looking at
 * through everything above it. That is what makes the single-scrim assertion
 * structural: two panes read as one surface exactly when they are painted by
 * the same element.
 */
async function shell(page: Page) {
  return page.evaluate(() => {
    const q = (selector: string) => document.querySelector<HTMLElement>(selector);
    const transparent = (color: string) =>
      color === "transparent" || /^rgba\(\s*0,\s*0,\s*0,\s*0\s*\)$/.test(color);
    const name = (el: Element | null) =>
      el === null
        ? null
        : (el.getAttribute("data-testid") ?? el.getAttribute("data-slot") ?? el.tagName.toLowerCase());
    const paintedBy = (el: Element | null) => {
      for (let node: Element | null = el; node !== null; node = node.parentElement) {
        const color = getComputedStyle(node).backgroundColor;
        if (!transparent(color)) return { by: name(node), color };
      }
      return { by: null, color: null };
    };
    const box = (el: Element | null) => {
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height };
    };

    const root = q('[data-slot="sidebar-wrapper"]');
    const sidebar = q('[data-slot="sidebar-inner"]');
    const sidebarColumn = q('[data-slot="sidebar-container"]');
    const inset = q('[data-slot="sidebar-inset"]');
    const card = q('[data-testid="content-surface"]');
    const style = card ? getComputedStyle(card) : null;

    return {
      rootName: name(root),
      // Where the sidebar's fill comes from, and where the frame's does.
      sidebarPaint: paintedBy(sidebar),
      framePaint: paintedBy(inset),
      cardColor: card ? getComputedStyle(card).backgroundColor : null,
      cardRadius: style ? Number.parseFloat(style.borderTopLeftRadius) : null,
      cardBorderWidth: style ? Number.parseFloat(style.borderTopWidth) : null,
      // The seam the old shell drew between the two panes.
      sidebarBorderRight: sidebarColumn
        ? Number.parseFloat(getComputedStyle(sidebarColumn).borderRightWidth)
        : null,
      unframed: card?.getAttribute("data-unframed") ?? null,
      insetBox: box(inset),
      cardBox: box(card),
    };
  });
}

for (const theme of ["light", "dark"] as const) {
  test(`the shell is chrome plus one inset card, with no seam (${theme})`, async ({ page }) => {
    await open(page, theme, "/#/settings");
    const s = await shell(page);

    // One surface, painted once. Both the sidebar column and the frame around
    // the card resolve to the SAME painting element — the shell root — which is
    // what makes them read as continuous rather than as two tinted panes.
    expect(s.sidebarPaint.by).toBe("sidebar-wrapper");
    expect(s.framePaint.by).toBe("sidebar-wrapper");
    expect(s.sidebarPaint.color).toBe(s.framePaint.color);

    // The card is the other layer: opaque, and a different fill from the chrome.
    // Equal fills would mean the "card" is only a rounded rectangle of nothing.
    expect(s.cardColor).not.toBeNull();
    expect(s.cardColor).not.toBe(s.sidebarPaint.color);

    // No divider between the panes. The border the sidebar used to draw is the
    // seam this layout replaces with fill contrast.
    expect(s.sidebarBorderRight).toBe(0);
  });

  test(`the content card is inset on all four sides and rounded (${theme})`, async ({ page }) => {
    await open(page, theme, "/#/settings");
    const s = await shell(page);

    expect(s.unframed).toBeNull();
    const { insetBox: frame, cardBox: card } = s;
    expect(frame).not.toBeNull();
    expect(card).not.toBeNull();

    // A frame, not a sliver, and an EVEN one: the four sides come from a single
    // `--frame-inset`, so they are equal by construction rather than by four
    // numbers that happen to agree. The closed first attempt at this issue
    // inset three sides and left the fourth flush, which is what produced a
    // sliver instead of a frame.
    const insets = {
      left: card!.left - frame!.left,
      top: card!.top - frame!.top,
      right: frame!.right - card!.right,
      bottom: frame!.bottom - card!.bottom,
    };
    for (const [side, gap] of Object.entries(insets)) {
      expect(gap, `${side} inset`).toBeGreaterThanOrEqual(8);
      expect(gap, `${side} inset matches the left`).toBeCloseTo(insets.left, 0);
    }

    expect(s.cardRadius).toBeGreaterThanOrEqual(8);
    expect(s.cardBorderWidth).toBeGreaterThan(0);
  });

  test(`the knowledge graph fills the card without spilling out of it (${theme})`, async ({
    page,
  }) => {
    await open(page, theme, "/#/company/graph");

    const graph = await page.evaluate(() => {
      const card = document.querySelector('[data-testid="content-surface"]')!;
      const kg = document.querySelector(".oc-kg");
      const box = card.getBoundingClientRect();
      // The card's INNER box. Its 1px hairline is part of its border box, so the
      // content starts one pixel in on every side — `clientLeft`/`clientTop` are
      // exactly that border, and `clientWidth`/`clientHeight` the box inside it.
      const inner = {
        left: box.left + card.clientLeft,
        top: box.top + card.clientTop,
        right: box.left + card.clientLeft + card.clientWidth,
        bottom: box.top + card.clientTop + card.clientHeight,
      };
      const rect = (el: Element | null) => {
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
      };
      return { inner, kg: rect(kg) };
    });

    // Every page is framed now, the graph included. What matters is that it
    // takes the height the card actually has rather than the height of the
    // window: it used to claim `h-svh`, which inside a card shorter than the
    // viewport lays the graph out taller than the box that clips it and crops
    // the bottom band — the legend with it.
    expect(graph.kg).not.toBeNull();
    expect(graph.kg!.left).toBeCloseTo(graph.inner.left, 0);
    expect(graph.kg!.top).toBeCloseTo(graph.inner.top, 0);
    expect(graph.kg!.right).toBeCloseTo(graph.inner.right, 0);
    expect(graph.kg!.bottom).toBeCloseTo(graph.inner.bottom, 0);
  });
}

test("the workflow canvas fills the card and keeps its minimap inside it", async ({ page }) => {
  await open(page, "light", "/#/workflows");
  await openFirstWorkflow(page);

  // Issue #1683 opens the History rail on select. That rail is real,
  // in-flow layout — it is what this spec's crop-bug class (#1259/#1261) is
  // NOT about — so close it and measure the canvas against the bare card it
  // was written against.
  const historyToggle = page.getByTestId("workflow-history-toggle");
  if (await historyToggle.isVisible().catch(() => false)) {
    await historyToggle.click();
    await expect(page.getByTestId("workflow-run-history")).toBeHidden();
  }

  const canvas = await page.evaluate(() => {
    const card = document.querySelector('[data-testid="content-surface"]')!;
    const flow = document.querySelector(".react-flow");
    const mini = document.querySelector(".react-flow__minimap");
    const box = card.getBoundingClientRect();
    // The card's inner box — its hairline is part of the border box, so the
    // content it clips starts one pixel in on every side.
    const inner = {
      left: box.left + card.clientLeft,
      right: box.left + card.clientLeft + card.clientWidth,
      bottom: box.top + card.clientTop + card.clientHeight,
    };
    const rect = (el: Element | null) => {
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return { left: r.left, right: r.right, bottom: r.bottom, width: r.width, height: r.height };
    };
    return { inner, flow: rect(flow), mini: rect(mini) };
  });

  // React Flow computes its viewport transform and its minimap viewbox from the
  // container's measured rect, so the thing to prove is that the container it
  // measures is the card — not a box wider than the one it is clipped to. That
  // is the crop class of bug #1259 and #1261 were filed for.
  expect(canvas.flow).not.toBeNull();
  expect(canvas.flow!.left).toBeCloseTo(canvas.inner.left, 0);
  expect(canvas.flow!.right).toBeCloseTo(canvas.inner.right, 0);

  // And the minimap, pinned to the canvas's bottom-right, is inside the card
  // rather than under its rounded corner or past its edge.
  expect(canvas.mini).not.toBeNull();
  expect(canvas.mini!.right).toBeLessThanOrEqual(canvas.inner.right);
  expect(canvas.mini!.bottom).toBeLessThanOrEqual(canvas.inner.bottom);
  expect(canvas.mini!.width).toBeGreaterThan(0);
  expect(canvas.mini!.height).toBeGreaterThan(0);
});
