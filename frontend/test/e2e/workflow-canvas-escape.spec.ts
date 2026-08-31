import { expect, test, type Locator, type Page } from "@playwright/test";

import { openWorkflow, workflowDetailName } from "./workflows";

/** The source-defined chain whose last node opens under the inspector. */
const FIXTURE = "Committed flow";

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

async function box(locator: Locator) {
  const result = await locator.boundingBox();
  expect(result, "element has no box").not.toBeNull();
  return result!;
}

async function openCanvas(page: Page) {
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/#/workflows");
  await dismissTour(page);
  await openWorkflow(page, FIXTURE);
  await expect(page.locator(".react-flow__node").first()).toBeVisible({
    timeout: 30_000,
  });
}

/**
 * Issue #1683 opens the Copilot on select. Copilot and the node inspector
 * share the canvas's right edge and Copilot wins while open (#303), so a spec
 * driving the inspector has to close it first — same as an operator would.
 */
async function closeCopilotIfOpen(page: Page) {
  const toggle = page.getByTestId("workflow-copilot-toggle");
  if (await toggle.isVisible().catch(() => false)) {
    await toggle.click();
    await expect(page.getByTestId("workflow-copilot")).toBeHidden();
  }
}

async function rightmostNode(page: Page) {
  const nodes = page.locator(".react-flow__node");
  const count = await nodes.count();
  let rightmost: Locator | null = null;
  let rightEdge = -Infinity;
  for (let index = 0; index < count; index++) {
    const node = nodes.nth(index);
    const nodeBox = await box(node);
    if (nodeBox.x + nodeBox.width > rightEdge) {
      rightmost = node;
      rightEdge = nodeBox.x + nodeBox.width;
    }
  }
  expect(rightmost, "the canvas rendered no nodes").not.toBeNull();
  return rightmost!;
}

test("Escape closes the node inspector and restores the canvas", async ({ page }) => {
  await openCanvas(page);
  await closeCopilotIfOpen(page);
  const node = await rightmostNode(page);
  const before = await box(node);

  await node.click();
  const inspector = page.getByTestId("workflow-node-detail");
  await expect(inspector).toBeVisible();
  await expect(async () => {
    expect((await box(node)).x).toBeLessThan(before.x - 1);
  }).toPass({ timeout: 5_000 });

  await page.keyboard.press("Escape");
  await expect(inspector).toBeHidden();
  await expect(async () => {
    const restored = await box(node);
    expect(Math.abs(restored.x - before.x)).toBeLessThan(2);
    expect(Math.abs(restored.y - before.y)).toBeLessThan(2);
  }).toPass({ timeout: 5_000 });
});

test("Escape closes the copilot and does nothing once canvas overlays are gone", async ({
  page,
}) => {
  await openCanvas(page);

  // Issue #1683 opens the Copilot on select — it is already up by the time
  // the canvas is.
  const copilot = page.getByTestId("workflow-copilot");
  await expect(copilot).toBeVisible();

  await page.keyboard.press("Escape");
  await expect(copilot).toBeHidden();
  await expect(workflowDetailName(page)).toHaveText(FIXTURE);

  await page.keyboard.press("Escape");
  await expect(workflowDetailName(page)).toHaveText(FIXTURE);
  await expect(page.getByTestId("workflow-node-detail")).toBeHidden();
});

test("the copilot leads with its purpose and mutes the unavailable composer", async ({ page }) => {
  await openCanvas(page);

  // Issue #1683 opens the Copilot on select — it is already up by the time
  // the canvas is.
  const copilot = page.getByTestId("workflow-copilot");
  await expect(copilot).toBeVisible();
  await expect(page.getByTestId("workflow-copilot-introduction")).toContainText(
    "Ask what Committed flow does, why a run failed, or what to change.",
  );

  const boundaries = copilot.getByText("How this copilot works", { exact: true });
  await expect(copilot.getByText("Answers are grounded in", { exact: false })).toBeHidden();
  await boundaries.click();
  await expect(copilot.getByText("Answers are grounded in", { exact: false })).toBeVisible();

  // The default e2e host runs the echo brain. The unavailable affordances must
  // be disabled, and the Ask button must opt out of primary-button paint.
  const send = page.getByTestId("workflow-copilot-send");
  await expect(send).toBeDisabled();
  await expect(send).toHaveClass(/disabled:bg-muted/);
  await expect(send).toHaveClass(/disabled:opacity-100/);
});
