import { expect, test, type Page, type Route } from "@playwright/test";

/**
 * The blocking first-run gate, end to end (issue #1844).
 *
 * A unit test pins the pure gating decision
 * (`test/unit/onboarding-gate-logic.test.ts`); this pins that a real mount
 * renders the gate over the shell and lets it go once the funnel completes —
 * and, the regression this spec exists to catch, that it stays gone across a
 * reload rather than re-deriving the answer from scratch each time.
 *
 * # Why this mocks `GET .../activation` instead of driving a real company
 *
 * `companies/e2e_harness` boots this suite serves on every run comes up with
 * `OPENCOMPANY_SKIP_ACTIVATION_GATE=1` (`test/e2e/host.sh`) — without it,
 * every one of this suite's other ~100 specs would hit this gate on their very
 * first navigation, since none of them know the funnel exists. That makes the
 * shared company always-activated by construction, so this spec drives the
 * gate through response mocking instead of trying to talk the real company out
 * of the flag the rest of the suite depends on it holding.
 *
 * # Why the paths below are patterns, not one literal string
 *
 * `OpenCompanyClient.scope()` (`src/api/client.ts`) answers `/api/v1/company`
 * only when it has no company id at all; the moment a connection carries one —
 * which every real connection does once it has discovered the single company
 * on this host, `e2e-harness-co` — every request goes to the scoped
 * `/api/v1/companies/{id}/…` form instead. A route registered only for the
 * unscoped shorthand never matches here, so the mock silently misses and the
 * page falls through to the *real* `GET …/activation` — which this host
 * answers `isActivated: true` (the `OPENCOMPANY_SKIP_ACTIVATION_GATE` stamp
 * above), not the `INCOMPLETE` this spec means to drive. The gate never
 * renders, "blocks the shell…" and "'skip for now'…" both fail waiting on a
 * step that never mounts, and only "does not reappear on reload…" happens to
 * pass — it wants `isActivated: true` anyway, which the real route already
 * answers on its own. Matching both shapes is what `scopeFor` itself
 * promises callers, so the mock now honours the same contract the app does.
 */

const ACTIVATION_PATH = /\/api\/v1\/(?:companies\/[^/]+|company)\/activation$/;
const COMPANY_PATCH_PATH = /\/api\/v1\/(?:companies\/[^/]+|company)$/;

const INCOMPLETE = {
  nameConfirmed: false,
  integrationConnected: false,
  workflowRunSucceeded: false,
  isActivated: false,
};

const COMPLETE = {
  nameConfirmed: true,
  integrationConnected: true,
  workflowRunSucceeded: true,
  isActivated: true,
  activationCompletedAtMillis: 1_700_000_000_000,
};

/** Serves `body` for every `GET .../activation` this page makes. */
async function mockActivation(page: Page, body: () => unknown) {
  await page.route(ACTIVATION_PATH, async (route: Route) => {
    await route.fulfill({ json: body() });
  });
}

/** The shell's own nav — present only once `AppShell` renders past the gate. */
function shellNav(page: Page) {
  return page.locator('[data-tour="nav-overview"]');
}

function gateStep(page: Page, id: "name" | "integration" | "workflow") {
  return page.getByTestId(`gate-step-${id}`);
}

test.describe("onboarding gate", () => {
  test("blocks the shell while the funnel is incomplete, and lets it through once complete", async ({
    page,
  }) => {
    let activated = false;
    await mockActivation(page, () => (activated ? COMPLETE : INCOMPLETE));
    // The name step's own write — the one action this spec drives through the
    // real UI rather than through a mock, so the gate's "re-poll after an
    // action" contract is exercised, not assumed.
    await page.route(COMPANY_PATCH_PATH, async (route: Route) => {
      if (route.request().method() !== "PATCH") return route.fallback();
      activated = true;
      await route.fulfill({ json: { name: "Real Name", nameConfirmed: true } });
    });

    await page.goto("/");

    // The gate, not the shell: every step visible, none of the nav.
    await expect(gateStep(page, "name")).toBeVisible();
    await expect(gateStep(page, "integration")).toBeVisible();
    await expect(gateStep(page, "workflow")).toBeVisible();
    await expect(shellNav(page)).toHaveCount(0);

    // Complete the name step through the real form. `OnboardingGate` opens
    // the first incomplete step itself (`firstOpen`) — "name" here, since
    // `INCOMPLETE.nameConfirmed` is false — so the toggle button already
    // reads `aria-expanded="true"` and clicking it unconditionally would
    // *close* it instead of opening it. Only click when it is not already
    // open, so this still opens the step on its own if a future change to
    // `firstOpen`'s ordering ever stops auto-opening it.
    const nameToggle = gateStep(page, "name").getByRole("button", { name: "Name your company" });
    if ((await nameToggle.getAttribute("aria-expanded")) !== "true") {
      await nameToggle.click();
    }
    // The mocked PATCH above flips `activated`, so the gate's next poll
    // (`onRefresh`, fired immediately after the write — not the 5s interval)
    // reads `isActivated` and the shell takes over.
    await page.getByLabel("Company name").fill("Real Name");
    await page.getByRole("button", { name: "Confirm name" }).click();

    await expect(shellNav(page)).toBeVisible();
    await expect(gateStep(page, "name")).toHaveCount(0);
  });

  test("does not reappear on reload once the funnel is already complete", async ({ page }) => {
    await mockActivation(page, () => COMPLETE);

    await page.goto("/");
    await expect(shellNav(page)).toBeVisible();

    await page.reload();
    await expect(shellNav(page)).toBeVisible();
    await expect(gateStep(page, "name")).toHaveCount(0);
  });

  test('"skip for now" lets the operator into the shell without completing the funnel', async ({
    page,
  }) => {
    await mockActivation(page, () => INCOMPLETE);

    await page.goto("/");
    await expect(gateStep(page, "name")).toBeVisible();

    await page.getByTestId("gate-skip").click();
    await expect(shellNav(page)).toBeVisible();
  });
});
