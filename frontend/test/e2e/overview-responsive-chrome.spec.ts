import { expect, test, type Locator } from "@playwright/test";

/**
 * Issue #1385: the Overview legend was wider than a phone viewport and its
 * side paddles remained 128px by 48px at every width. These checks measure the
 * browser's boxes rather than the Tailwind class names: the canvas is clipped,
 * so an in-bounds legend is only useful if its right edge is truly reachable.
 */

const VIEWPORTS = [
  { width: 390, height: 844, paddle: { width: 32, height: 56, inset: 8 } },
  { width: 768, height: 900, paddle: { width: 40, height: 80, inset: 12 } },
] as const;

test.beforeEach(async ({ page }) => {
  // The first-run tour is unrelated chrome that can cover the graph.
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

for (const viewport of VIEWPORTS) {
  test(`keeps graph chrome reachable at ${viewport.width}px`, async ({ page }) => {
    await page.setViewportSize(viewport);
    await page.goto("/#/company/graph");

    const legend = page.getByTestId("kg-legend");
    const previous = page.getByRole("button", { name: "Previous desk" });
    const next = page.getByRole("button", { name: "Next desk" });
    await expect(legend).toBeVisible();
    await expect(previous).toBeVisible();
    await expect(next).toBeVisible();

    const [legendBox, previousBox, nextBox, canvasBox] = await Promise.all([
      legend.boundingBox(),
      previous.boundingBox(),
      next.boundingBox(),
      legend.evaluate((el) => {
        const canvas = el.parentElement;
        if (!canvas) return null;
        const { left, right } = canvas.getBoundingClientRect();
        return { left, right };
      }),
    ]);
    expect(legendBox, "the legend must have a rendered box").not.toBeNull();
    expect(previousBox, "the previous paddle must have a rendered box").not.toBeNull();
    expect(nextBox, "the next paddle must have a rendered box").not.toBeNull();
    expect(canvasBox, "the legend must be placed in the graph canvas").not.toBeNull();

    // The legend starts at the canvas inset and finishes inside the canvas —
    // in particular, none of its kinds are hidden past the clipped right edge.
    expect(legendBox!.x).toBeGreaterThanOrEqual(canvasBox!.left);
    expect(legendBox!.x + legendBox!.width).toBeLessThanOrEqual(canvasBox!.right + 1);

    for (const paddle of [previousBox!, nextBox!]) {
      expect(paddle.width).toBeLessThanOrEqual(viewport.paddle.width);
      expect(paddle.height).toBeLessThanOrEqual(viewport.paddle.height);
    }
    expect(previousBox!.x - canvasBox!.left).toBeGreaterThanOrEqual(viewport.paddle.inset - 1);
    expect(canvasBox!.right - (nextBox!.x + nextBox!.width)).toBeGreaterThanOrEqual(viewport.paddle.inset - 1);
  });
}

/**
 * The element's box once it has stopped moving.
 *
 * The legend animates between its two placements (`transition-[bottom]`), so a
 * box read straight after a click is a frame of the journey rather than the
 * result — the first run of this spec failed at three widths on a legend that
 * was mid-flight and 140px from where it settles. Two identical reads in a row
 * is the end of the transition without hard-coding its duration.
 */
async function settled(locator: Locator) {
  let previous = JSON.stringify(await locator.boundingBox());
  for (let attempt = 0; attempt < 40; attempt++) {
    await locator.page().waitForTimeout(50);
    const current = JSON.stringify(await locator.boundingBox());
    if (current === previous) return JSON.parse(current) as NonNullable<Awaited<ReturnType<Locator["boundingBox"]>>>;
    previous = current;
  }
  throw new Error("the element never stopped moving");
}

/**
 * Issue #1664: with a node selected, the detail panel covered the legend.
 *
 * Below 820px the panel is a full-width bottom sheet anchored to the same edge
 * the legend sits on, and it is `z-30` against the legend's `z-10` — so every
 * kind label and the workflow-placement caveat vanished outright. Above 820px
 * the same panel is a 300px right rail that a wide legend ran underneath.
 *
 * Class names cannot answer this: the sheet's height is a percentage of a card
 * that is shorter than the window, and the first attempt at the fix carried the
 * right class and was outranked by a `sm:` variant at 700px. So this measures
 * boxes, at both sides of the breakpoint, with the caveat both shut and open —
 * open is the legend's real cap, and the state #1318 exists to reach.
 */
const WITH_DETAIL = [
  { width: 390, height: 844, panel: "sheet" },
  { width: 700, height: 400, panel: "sheet" },
  { width: 800, height: 420, panel: "sheet" },
  { width: 700, height: 500, panel: "sheet" },
  { width: 700, height: 600, panel: "sheet" },
  { width: 700, height: 800, panel: "sheet" },
  { width: 819, height: 800, panel: "sheet" },
  { width: 900, height: 800, panel: "rail" },
  { width: 1280, height: 900, panel: "rail" },
] as const;

for (const viewport of WITH_DETAIL) {
  test(`keeps the legend clear of the detail ${viewport.panel} at ${viewport.width}x${viewport.height}`, async ({ page }) => {
    await page.setViewportSize({ width: viewport.width, height: viewport.height });
    await page.goto("/#/company/graph");

    const legend = page.getByTestId("kg-legend");
    await expect(legend).toBeVisible();

    // Any node with a detail card will do; a teammate is present in every
    // company the harness can boot.
    await page.getByRole("button", { name: /^AI teammates: / }).first().click({ force: true });
    const panel = page.locator("aside").first();
    await expect(panel).toBeVisible();

    const caveat = page.locator('[aria-label="Graph legend"] details');
    for (const openCaveat of [false, true]) {
      if (openCaveat) await caveat.locator("summary").click();
      await expect(legend).toBeVisible();

      const [legendBox, panelBox] = await Promise.all([settled(legend), settled(panel)]);
      // Not one pixel of overlap, in either axis — being partly under an opaque
      // panel is the same failure as being wholly under one.
      const overlapX = Math.min(legendBox.x + legendBox.width, panelBox.x + panelBox.width) - Math.max(legendBox.x, panelBox.x);
      const overlapY = Math.min(legendBox.y + legendBox.height, panelBox.y + panelBox.height) - Math.max(legendBox.y, panelBox.y);
      expect(
        overlapX <= 0 || overlapY <= 0,
        `legend ${JSON.stringify(legendBox)} overlaps the detail panel ${JSON.stringify(panelBox)} with the caveat ${openCaveat ? "open" : "shut"}`,
      ).toBe(true);

      // Nor may lifting it push it under the chrome above: the desk selector
      // names every desk, and the paddles are how you step between them. The
      // legend is `z-40` and both of those are below it, so an overlap here is
      // the legend drawing OVER the two controls the graph is steered with —
      // the same failure as being covered, with the roles swapped.
      const chrome: [string, Awaited<ReturnType<Locator["boundingBox"]>>][] = [
        ["Previous desk", await page.getByRole("button", { name: "Previous desk", exact: true }).boundingBox()],
        ["Next desk", await page.getByRole("button", { name: "Next desk", exact: true }).boundingBox()],
        ["the desk selector", await page.getByTestId("kg-desk-selector").boundingBox()],
      ];
      // …and the paddles must not sit on the desk selector either. They are
      // `z-40` over its `z-20`, so an overlap covers the first desk chip and
      // takes its clicks — the paddle wins the hit test.
      const deskSelector = await page.getByTestId("kg-desk-selector").boundingBox();
      if (deskSelector) {
        for (const name of ["Previous desk", "Next desk"]) {
          const paddle = await page.getByRole("button", { name, exact: true }).boundingBox();
          if (!paddle) continue;
          const px = Math.min(paddle.x + paddle.width, deskSelector.x + deskSelector.width) - Math.max(paddle.x, deskSelector.x);
          const py = Math.min(paddle.y + paddle.height, deskSelector.y + deskSelector.height) - Math.max(paddle.y, deskSelector.y);
          expect(
            px <= 0 || py <= 0,
            `${name} ${JSON.stringify(paddle)} overlaps the desk selector ${JSON.stringify(deskSelector)}`,
          ).toBe(true);
        }
      }

      for (const [name, other] of chrome) {
        if (!other) continue;
        const ox = Math.min(legendBox.x + legendBox.width, other.x + other.width) - Math.max(legendBox.x, other.x);
        const oy = Math.min(legendBox.y + legendBox.height, other.y + other.height) - Math.max(legendBox.y, other.y);
        expect(
          ox <= 0 || oy <= 0,
          `legend ${JSON.stringify(legendBox)} overlaps ${name} ${JSON.stringify(other)} with the caveat ${openCaveat ? "open" : "shut"}`,
        ).toBe(true);
      }
    }
  });
}
