import { expect, test, type Page } from "@playwright/test";

/**
 * A note opened at phone width has to be readable.
 *
 * The workspace is two panes — explorer, then the note — and below `md` only
 * one of them can have the column. Which one is a piece of state that opening a
 * note has to move: the explorer keeps the column until a note is picked, and
 * the note takes it from there. When that state never moves, tapping a note
 * plays an animation and leaves the operator on the list, with the note itself
 * laid out at zero by zero.
 *
 * Measured rather than asserted on class names: `toBeVisible()` alone passes on
 * a `display:none` ancestor's child in some arrangements, and the failure here
 * is precisely a box with no area.
 */

const PHONE = { width: 386, height: 800 };
const DESKTOP = { width: 1280, height: 900 };

test.beforeEach(async ({ page }) => {
  // The first-run tour renders over the console and swallows clicks.
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

async function openWorkspace(page: Page) {
  await page.goto("/#/workspace");
  await page.reload();
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip.waitFor({ state: "visible", timeout: 15_000 }).catch(() => {});
  if (await skip.isVisible()) await skip.click();
  await expect(page.getByTestId("workspace-tree")).toBeVisible({ timeout: 30_000 });
}

/** Tap the seeded `standards` note in the tree. */
async function openSeededNote(page: Page) {
  await page
    .getByTestId("workspace-tree")
    .getByTestId("workspace-tree-name")
    .filter({ hasText: "standards" })
    .first()
    .click();
}

/** The note body's rendered box, or `null` where it has none. */
async function noteBox(page: Page) {
  return page.getByTestId("workspace-note").boundingBox();
}

test("a note opened at phone width renders with area", async ({ page }) => {
  await page.setViewportSize(PHONE);
  await openWorkspace(page);
  await openSeededNote(page);

  const box = await noteBox(page);
  expect(box, "the note pane must lay out at phone width").not.toBeNull();
  expect(box!.width).toBeGreaterThan(200);
  expect(box!.height).toBeGreaterThan(40);
  await expect(page.getByTestId("workspace-note")).toContainText("House conventions");
});

test("the note survives a width round trip", async ({ page }) => {
  // Narrow, wide, narrow again. A one-shot layout race would recover on the
  // second narrow; a collapse does not.
  await page.setViewportSize(PHONE);
  await openWorkspace(page);
  await openSeededNote(page);
  const first = await noteBox(page);

  await page.setViewportSize(DESKTOP);
  const wide = await noteBox(page);

  await page.setViewportSize(PHONE);
  const again = await noteBox(page);

  for (const [label, box] of [
    ["first narrow", first],
    ["wide", wide],
    ["narrow again", again],
  ] as const) {
    expect(box, `${label}: the note must have a box`).not.toBeNull();
    expect(box!.width, `${label}: the note must have width`).toBeGreaterThan(200);
    expect(box!.height, `${label}: the note must have height`).toBeGreaterThan(40);
  }
});

test("the explorer is reachable again after opening a note at phone width", async ({ page }) => {
  // Taking the column is only correct if it can be handed back — otherwise the
  // fix trades an unreadable note for an unreachable list.
  await page.setViewportSize(PHONE);
  await openWorkspace(page);
  await openSeededNote(page);

  await page.getByRole("button", { name: "Toggle explorer" }).click();
  await expect(page.getByTestId("workspace-tree")).toBeVisible();
});

test("both panes still share the column on a desktop viewport", async ({ page }) => {
  await page.setViewportSize(DESKTOP);
  await openWorkspace(page);
  await openSeededNote(page);

  await expect(page.getByTestId("workspace-tree")).toBeVisible();
  const box = await noteBox(page);
  expect(box).not.toBeNull();
  expect(box!.width).toBeGreaterThan(200);
});
