import { expect, test, type Locator, type Page } from "@playwright/test";

import { LIVE_BRAIN } from "./capabilities";
import { bubbles, openChannel } from "./chat-helpers";

/**
 * End-to-end proof for issue #1729 — the thread panel drew the agent's face on
 * the current user's own line.
 *
 * This needs a browser, and specifically needs the *signed-in* console: the
 * face on a "you" line comes from `auth/me`, and the bug was `ThreadPanel`
 * resolving its senders without it, so every reader's own line fell back to the
 * mascot hashed from the literal string "You". A unit test pins the resolution
 * (`test/unit/chat-thread-avatar.test.ts`); this pins that the value the panel
 * needs actually reaches it through the real shell, against a real host, for a
 * real viewer.
 *
 * The assertion is deliberately "the thread shows the same face the timeline
 * shows", not "the face is such-and-such flavour" — that is the property the
 * issue is about (a reader must be able to tell their own lines from the
 * agent's, in both places), and it cannot be passed by coincidence the way an
 * assertion on one hashed flavour could.
 */

const ENGINEERING = "engineering";

test.beforeEach(async ({ page }) => {
  // The first-run tour opens a modal over the console and swallows every click
  // beneath it (the pattern every chat spec here uses).
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

/** The `src` of the avatar image inside one bubble or thread line. */
async function faceOf(row: Locator): Promise<string> {
  return (await row.locator("img").first().getAttribute("src")) ?? "";
}

/** The thread panel's avatar tile for your own line (issue #1729). */
function yourThreadTile(page: Page): Locator {
  return page.getByTestId("thread-avatar-you").first();
}

/**
 * The thread panel's avatar tile for the other voice.
 *
 * Matched by "not yours" rather than by `thread-avatar-agent`, because who the
 * company side reads as depends on the channel: a desk channel with a tone is
 * an `agent` and wears a mascot, while the harness company's desks read as the
 * `company` and wear the brand mark — which has no `img` at all. So this is
 * only ever asked "are you there and are you not me", never for a `src`.
 */
function otherThreadTile(page: Page): Locator {
  return page
    .locator('[data-testid^="thread-avatar-"]:not([data-testid="thread-avatar-you"])')
    .first();
}

/** The thread panel, once it is open. */
function panel(page: Page): Locator {
  return page.locator("aside").filter({ has: page.getByRole("heading", { name: "Thread" }) });
}

test("your own line wears your own face in the thread panel too", async ({ page }, testInfo) => {
  // Same guard, and the same one-line reason, as the three other specs that
  // locate the company's reply by the offline echo brain's `You said: <text>`
  // (`chat-detached-post-failure`, `chat-detached-sse-unavailable`,
  // `chat-live-events`). The live-brain lane runs a real turn against
  // `test/e2e/mock-brain.mjs`, whose plain reply quotes nothing back
  // deliberately — so the wait below can never be satisfied there. Not a
  // flake and not a lane to paper over: the assertion this spec exists for
  // (your face and the agent's are two different faces) is about the console,
  // and the default `Console E2E` lane — the one the CI comments name as the
  // gate on a console change — runs it in full.
  test.skip(LIVE_BRAIN, "asserts the offline echo brain's `You said: <text>` reply.");

  await openChannel(page, ENGINEERING);

  const marker = `thread-face-${Date.now()}`;
  await page.getByPlaceholder(/^Message /).fill(marker);
  await page.getByPlaceholder(/^Message /).press("Enter");

  // Your own line in the main timeline, and the reply beneath it. The offline
  // echo brain answers every message, so there is always a second voice.
  // `hasNotText` matters: the echo brain's reply quotes the marker back, so a
  // bare `hasText: marker` matches both bubbles.
  const mine = bubbles(page)
    .filter({ hasText: marker })
    .filter({ hasNotText: "You said:" })
    .first();
  await expect(mine).toBeVisible({ timeout: 30_000 });
  const theirs = bubbles(page).filter({ hasText: `You said: ${marker}` }).first();
  await expect(theirs).toBeVisible({ timeout: 30_000 });

  const timelineFace = await faceOf(mine);
  expect(timelineFace, "the main timeline already drew your own face").not.toBe("");

  // Open a thread off your own message and reply in it, so the panel holds
  // both voices: your line and the agent's answer to it.
  await mine.hover();
  const reply = mine.getByRole("button", { name: "Reply in thread" });
  await expect(reply).toBeEnabled({ timeout: 30_000 });
  await reply.click();
  const thread = panel(page);
  await expect(thread).toBeVisible();

  // The panel now holds your own message as the thread's parent. The echo
  // brain answers the reply below, so both voices end up in it.
  await thread.getByPlaceholder("Reply…").fill("and here is a reply");
  await thread.getByPlaceholder("Reply…").press("Enter");
  // `exact` matters here for the same reason `hasNotText` does above: the echo
  // brain quotes the reply back as `You said: and here is a reply`, so a
  // substring match resolves to two lines the moment that answer lands, and the
  // assertion dies of a strict-mode violation instead of waiting. Which of the
  // two the console paints first is a race the runner's load decides, which is
  // why the substring form passed here and failed on a busy CI runner.
  await expect(thread.getByText("and here is a reply", { exact: true })).toBeVisible({
    timeout: 30_000,
  });
  await expect(otherThreadTile(page)).toBeVisible({ timeout: 30_000 });

  // The bug: your line's face was the mascot hashed from the literal string
  // "You", not the face you actually have — so it could (and on the reported
  // company did) come out as the agent's, and the thread could not be read.
  //
  // Equality with the timeline is the assertion, not "the face is flavour X":
  // it states the property the issue is about — the same person is drawn the
  // same way in both places — and it cannot pass by hash coincidence.
  const mineInThread = await faceOf(yourThreadTile(page));
  expect(mineInThread, "the thread must show the same face the timeline does").toBe(timelineFace);
  expect(mineInThread, "and it must actually be a face").not.toBe("");

  // Evidence for the issue: the panel with a human author beside an agent one,
  // in both themes.
  for (const theme of ["light", "dark"] as const) {
    // `next-themes` is configured `attribute="class"`, so the live switch is
    // the class on `<html>`; `localStorage` is only what it reads at boot.
    await page.evaluate((value) => {
      window.localStorage.setItem("theme", value);
      document.documentElement.classList.remove("light", "dark");
      document.documentElement.classList.add(value);
      document.documentElement.style.colorScheme = value;
    }, theme);
    await testInfo.attach(`thread-panel-${theme}`, {
      body: await thread.screenshot(),
      contentType: "image/png",
    });
  }
});
