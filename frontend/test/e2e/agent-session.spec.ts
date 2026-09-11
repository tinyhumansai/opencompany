import { expect, test, type Page } from "@playwright/test";

/**
 * Proof for the one-agent-one-session change: a teammate's session is reachable
 * against the live host, at an address.
 *
 * The claim spans the whole stack and no unit render can make it. The console
 * asks `GET {scope}/agents/{id}/session`; the host answers by resolving which
 * channels that agent can read — through the *same* function that decides the
 * agent's own context — and stamps every row with the channel it was said on.
 * A stubbed client proves the component; only a host proves the projection.
 *
 * The tab is an address (`#/company/agent/<id>?tab=session` — `#/team/<id>`
 * rewrites onto it and drops the query, so the canonical form is the one used
 * here), so the walk is a deep link
 * rather than a click path: that is the property that lets one operator paste a
 * teammate's session to another, and it is the one a click path would not test.
 *
 * Either populated or empty is correct — whether the harness company's data
 * directory holds any transcript decides which. What must never happen is the
 * panel failing to render, or holding a spinner forever.
 */

/**
 * Clears both things that can stand in front of a deep link.
 *
 * "Skip setup" is the activation gate's control and "Skip for now" is the
 * guided tour's — two different surfaces with two different labels, and either
 * can be up on a fresh data directory. Both are tried because which one appears
 * depends on state this test does not own.
 */
async function dismissOnboarding(page: Page) {
  for (const name of ["Skip setup", "Skip for now"]) {
    const skip = page.getByRole("button", { name });
    for (let attempt = 0; attempt < 5; attempt += 1) {
      if (!(await skip.isVisible().catch(() => false))) break;
      await skip.click({ force: true }).catch(() => {});
      await page.waitForTimeout(300);
    }
  }
}

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const seen = JSON.stringify({ skipped: true, seenAt: Date.now() });
    for (const key of ["oc-tour:single", "oc-tour:e2e-harness-co", "oc-tour:null"]) {
      window.localStorage.setItem(key, seen);
    }
  });
});

test("a teammate's session opens from its own address", async ({ page }) => {
  // Clear the gate FIRST, on a neutral address. Dismissing it navigates, and a
  // navigation rewrites the hash — so opening the deep link before the gate is
  // gone would test the gate's redirect rather than the link.
  await page.goto("/");
  await dismissOnboarding(page);

  await page.goto("/#/company/agent/engineer?tab=session");
  await dismissOnboarding(page);

  // The tab resolved from the hash rather than defaulting to Overview — the
  // whole point of `useHashTab`, and what makes the link shareable.
  // Addressed by test id and asserted on the attribute rather than through
  // `getByRole("tab", …)`: the strip's buttons carry `tabindex="-1"` (roving
  // focus), which keeps them out of that role query on this build.
  const tab = page.getByTestId("agent-tab-session");
  await expect(tab).toBeVisible({ timeout: 30_000 });
  // The tab resolved FROM THE HASH rather than defaulting to Overview. This is
  // the assertion the whole spec exists for — a click path would pass without
  // the address ever working.
  await expect(tab).toHaveAttribute("aria-selected", "true");

  // One of the three honest states, never a spinner that never settles: the
  // stream, the "nothing yet" line, or a host that keeps no session.
  const stream = page.getByTestId("agent-session");
  const settled = stream
    .or(page.getByText(/has not said or heard anything yet/))
    .or(page.getByText(/does not keep a per-agent session yet/));
  await expect(settled.first()).toBeVisible({ timeout: 30_000 });

  // If there are rows at all, every one of them says which channel it came
  // from. A merged stream whose rows do not is unreadable — two teammates
  // answering in two desks interleave with nothing to tell them apart.
  if (await stream.isVisible().catch(() => false)) {
    const rows = await page.getByTestId("agent-session-row").count();
    if (rows > 0) {
      expect(await page.getByTestId("agent-session-channel").count()).toBe(rows);
    }
  }
});

/**
 * The raw view is reachable at its own address.
 *
 * `?raw` is a deep link on purpose — "look at what it actually saw" is a thing
 * one operator sends another — so the walk opens it cold rather than clicking
 * the toggle. A click path would pass with the address broken, which is the one
 * failure that matters here.
 */
test("a teammate's raw turns open from their own address", async ({ page }) => {
  await page.goto("/");
  await dismissOnboarding(page);

  await page.goto("/#/company/agent/engineer?tab=session&raw");
  await dismissOnboarding(page);

  const tab = page.getByTestId("agent-tab-session");
  await expect(tab).toBeVisible({ timeout: 30_000 });
  await expect(tab).toHaveAttribute("aria-selected", "true");

  // Either the raw stream, or one of the two honest empties. Whether this
  // harness company's data directory holds a transcript decides which, and the
  // spec must not depend on that — what it asserts is that the address settles.
  const rawStream = page.getByTestId("agent-session-raw");
  const settled = rawStream
    .or(page.getByText(/has not said or heard anything yet/))
    .or(page.getByText(/does not keep a per-agent session yet/));
  await expect(settled.first()).toBeVisible({ timeout: 30_000 });

  if (await rawStream.isVisible().catch(() => false)) {
    // The address won: the raw control is the pressed one, and the rendered
    // stream is the raw one rather than the chat bubbles.
    await expect(page.getByTestId("agent-session-view-raw")).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expect(page.getByTestId("agent-session")).toHaveCount(0);

    // Every raw turn says whether the teammate said it or was given it. That
    // distinction is the reason to read this view at all.
    const turns = page.getByTestId("agent-session-raw-turn");
    const count = await turns.count();
    for (let index = 0; index < count; index += 1) {
      await expect(turns.nth(index)).toHaveAttribute(
        "data-direction",
        /^(said|heard)$/,
      );
    }

    // And the toggle goes back, without needing a reload.
    await page.getByTestId("agent-session-view-chat").click();
    await expect(page.getByTestId("agent-session")).toBeVisible();
    await expect(rawStream).toHaveCount(0);
  }
});

/**
 * The same raw view, reached from the DM — which is where the question gets
 * asked.
 *
 * `#/chat/dm:<id>?raw` is an address for the same reason the tab's is, so the
 * walk is a deep link rather than a click on the header control. What it proves
 * that a unit render cannot: the flag survives the chat router (which strips
 * everything from `?` onward before resolving a segment), and the host answers
 * the per-agent route for a teammate reached this way.
 */
test("a DM opens on the teammate's raw turns when the address asks", async ({
  page,
}) => {
  await page.goto("/");
  await dismissOnboarding(page);

  await page.goto("/#/chat/dm:engineer?raw");
  await dismissOnboarding(page);

  // The control is present at all — which is the half of this the operator
  // complained about. It exists only in a DM; the channel case is below.
  const toggle = page.getByTestId("chat-raw-toggle");
  await expect(toggle).toBeVisible({ timeout: 30_000 });
  await expect(toggle).toHaveAttribute("aria-pressed", "true");

  // One of the honest states, never a spinner that never settles.
  const stream = page.getByTestId("agent-session-raw");
  const settled = stream
    .or(page.getByText(/Nothing has been said in this conversation yet/))
    .or(page.getByText(/does not keep a per-agent session yet/));
  await expect(settled.first()).toBeVisible({ timeout: 30_000 });

  // The composer stays. The toggle changes how the conversation is drawn, not
  // whether you can still talk in it. Asserted against the composer itself,
  // not a second look at the toggle already checked above (coderabbit
  // review) — this is the actual claim the comment makes.
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible();

  // And it goes back without a reload, dropping the flag from the address.
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-pressed", "false");
  await expect(stream).toHaveCount(0);
  expect(await page.evaluate(() => window.location.hash)).not.toContain("raw");
});

/**
 * Not offered on a `#channel`. Several agents speak there, so "the raw turns"
 * would have to pick one for you — worse than not offering it at all.
 */
test("a channel offers no raw-turns toggle", async ({ page }) => {
  await page.goto("/");
  await dismissOnboarding(page);

  await page.goto("/#/chat/general");
  await dismissOnboarding(page);

  // Wait for the header to exist before asserting a control is absent from it,
  // or this passes against a page that simply had not rendered yet.
  await expect(
    page.getByRole("heading", { level: 1, name: "general" }),
  ).toBeVisible({ timeout: 30_000 });
  await expect(page.getByTestId("chat-raw-toggle")).toHaveCount(0);
});
