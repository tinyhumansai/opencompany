import { expect, test, type Page } from "@playwright/test";

import { expectWorkflowIndex, openFirstWorkflow } from "./workflows";

/**
 * Issue #1107: run history is a LEFT RAIL beside the canvas on a wide viewport,
 * and the bottom strip it has always been below `xl` (1280px).
 *
 * Pinned as geometry rather than as class names, because the thing that broke
 * is the thing an operator sees: run rows are tall, so a horizontal strip shows
 * two of them while spending the full width of the screen, and it eats the
 * canvas height that a workflow graph needs most — the same canvas the selected
 * run is overlaid onto (issue #371).
 *
 * The second assertion is the one that keeps the fix honest. A rail is only an
 * improvement while the canvas beside it stays usable: the rail is in-flow, so
 * every pixel it takes is a pixel the graph loses, and the right-hand overlays
 * (`CopilotPanel`, `NodeDetailPanel`) then cover ~396px more of what is left.
 * Below `xl` the rail is therefore not in-flow at all — hence the narrow case.
 *
 * Issue #1110: the rail lives in the DETAIL view, so every case here opens a
 * workflow first — and the last case pins the other half of that, which is that
 * the index must not carry per-workflow run chrome at all. A list of workflows
 * has no single run to show history for, so there is nothing for a rail to be
 * beside.
 */

/** Dismisses the first-run tour if it is up; see `workflow-run-history.spec.ts`. */
async function dismissTour(page: Page) {
  const skip = page.getByRole("button", { name: "Skip for now" });
  try {
    await skip.waitFor({ state: "visible", timeout: 10_000 });
  } catch {
    return;
  }
  await skip.click();
  await expect(skip).toBeHidden();
}

/**
 * Opens run history on a workflow's detail view, skipping on a host that does
 * not serve `…/workflows/runs`.
 *
 * Issue #1110: the caller must have opened a workflow already. Waiting for the
 * toggle on the index would time out and report it as an unsupported host,
 * which would be a lie about the host to cover a spec that never navigated.
 */
async function openHistory(page: Page) {
  const toggle = page.getByTestId("workflow-history-toggle");
  try {
    await toggle.waitFor({ state: "visible", timeout: 30_000 });
  } catch {
    test.skip(true, "this host does not serve …/workflows/runs");
  }
  // Issue #1683: the toggle no longer needs a click to open the rail — index
  // select already did, so clicking it here would close it instead.
  const panel = page.getByTestId("workflow-run-history");
  await expect(panel).toBeVisible();
  return panel;
}

/** The canvas ReactFlow paints into — the surface the rail is competing with. */
function canvas(page: Page) {
  return page.locator(".react-flow").first();
}

test("at xl the run history is a left rail, and the canvas keeps its width", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/#/workflows");
  await dismissTour(page);
  await openFirstWorkflow(page);

  const panel = await openHistory(page);
  const flow = canvas(page);
  await expect(flow).toBeVisible();

  const rail = (await panel.boundingBox())!;
  const graph = (await flow.boundingBox())!;

  // Left of the canvas, not under it.
  expect(rail.x + rail.width, "the rail must sit left of the canvas").toBeLessThanOrEqual(
    graph.x + 1,
  );
  // Full height of the canvas region, which is what lets it show more than two
  // runs. Compared with a tolerance because they are siblings in a flex row,
  // not the same box.
  expect(
    Math.abs(rail.height - graph.height),
    "the rail runs the full height of the canvas region",
  ).toBeLessThan(4);
  // A rail that costs the canvas more than it is worth is the failure mode this
  // whole change has to avoid. 640px is a floor, not a target: the graph must
  // stay a graph, not a keyhole.
  expect(graph.width, "the canvas keeps a usable width beside the rail").toBeGreaterThan(
    640,
  );
});

test("below xl the run history falls back to a strip under the canvas", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1024, height: 800 });
  await page.goto("/#/workflows");
  await dismissTour(page);
  await openFirstWorkflow(page);

  const panel = await openHistory(page);
  const flow = canvas(page);
  await expect(flow).toBeVisible();

  const strip = (await panel.boundingBox())!;
  const graph = (await flow.boundingBox())!;

  // Under the canvas, full width — a fixed rail plus a canvas does not fit here,
  // and squeezing the graph to a sliver would be worse than the strip.
  expect(strip.y, "the strip must sit below the canvas").toBeGreaterThanOrEqual(
    graph.y + graph.height - 1,
  );
  expect(
    Math.abs(strip.width - graph.width),
    "the strip spans the same width as the canvas",
  ).toBeLessThan(4);
});

test("the index carries no run chrome, and leaving a workflow takes the rail with it", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/#/workflows");
  await dismissTour(page);

  // Issue #1110: the tab's front door. Nothing here is about one run — no
  // History toggle to open a rail with, and no rail already open.
  await expectWorkflowIndex(page);
  await expect(page.getByTestId("workflow-history-toggle")).toHaveCount(0);
  await expect(page.getByTestId("workflow-run-history")).toHaveCount(0);

  // Open one, put the rail up, and go back. The rail belongs to the workflow,
  // so it must not survive the trip: it is not merely hidden on the index, the
  // slot it mounts in does not exist there.
  await openFirstWorkflow(page);
  await openHistory(page);
  await page.getByTestId("workflow-back-to-index").click();
  await expectWorkflowIndex(page);
  await expect(page.getByTestId("workflow-run-history")).toHaveCount(0);
  await expect(page.getByTestId("workflow-history-toggle")).toHaveCount(0);
});
