import { expect, test } from "@playwright/test";

/**
 * End-to-end proof for B-096 — which conversation is open is **routed** state,
 * never derived state.
 *
 * The founder bug: a message composed in `#general` was delivered to a private
 * DM. The console had changed conversation underneath the draft with no
 * navigation behind it. Two facts made that possible, and only one is wrong.
 *
 * The right one is that the composer is a single instance shared by every
 * channel, whose draft deliberately survives a channel change — see
 * `MessageComposer`'s `suppressed` doc; unmounting it is what discards a
 * half-written message, and PR #1984 restored that on purpose.
 *
 * The wrong one is that a bare `#/chat` — where the magic-link landing route
 * puts you, because `useHashView` canonicalises the *view* and knows nothing
 * about chat's channels — left the open channel as the value of an expression
 * over `members`, `desks`, `transcripts` and `operator`. Each lands
 * asynchronously, so any re-derivation could answer differently from the
 * channel the founder was shown, and `send` addresses whatever that expression
 * currently names.
 *
 * # Why this is an e2e spec and not a unit test
 *
 * The defect *is* the URL, and only a browser has one. The behaviour spans
 * `useHashView`'s canonicalisation, `ChatView`'s channel resolution and the
 * `hashchange` round trip between them; a unit test of any one of the three
 * passes with the bug fully present, which is how it survived.
 *
 * Like the rest of `test/e2e` this drives a running host — see
 * `playwright.config.ts`.
 */

/** The hash a resolved chat address has: `#/chat/<channel id>`. */
const RESOLVED = /^#\/chat\/[^/]+$/;

/** Issue #370's notice, raised when an address names no channel this company has. */
const UNKNOWN_CHANNEL = /isn't a channel here/;

/**
 * The first-run product tour renders a modal over the console and swallows
 * every click beneath it. Answer "already skipped" for whatever company id the
 * host resolves to rather than hard-coding the harness's.
 */
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

/**
 * The landing case: nothing is remembered, so there is no memory to restore and
 * the address has to be written from the channel that was actually resolved.
 *
 * This is the exact state of a founder arriving on a magic link — a fresh
 * profile with an empty `oc.chat.last-channel` — and it is the one that left
 * the hash bare indefinitely, because the console reported the *derived*
 * channel as viewed and remembered that, so memory could never disagree with
 * the fallback and never triggered a navigation of its own.
 */
test("a bare #/chat resolves into the address with nothing remembered", async ({ page }) => {
  await page.addInitScript(() => {
    for (const key of Object.keys(localStorage)) {
      if (key.startsWith("oc.chat.last-channel")) localStorage.removeItem(key);
    }
  });

  await page.goto("/#/chat");
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible({ timeout: 30_000 });

  await expect.poll(() => new URL(page.url()).hash, { timeout: 15_000 }).toMatch(RESOLVED);
  // A real channel, not merely a syntactically resolved address: an id this
  // company does not have would raise issue #370's notice over the fallback,
  // which is the same silent-substitution bug wearing a URL.
  await expect(page.getByText(UNKNOWN_CHANNEL)).toHaveCount(0);
});

/**
 * The two properties the normalisation must not cost: a deep link is honoured
 * verbatim, and a bare re-entry returns to what was last read (issue #412).
 *
 * Guards the fix from collapsing into "always navigate to `firstChannel`",
 * which would pass the test above while silently retiring re-entry memory.
 */
test("a deep link is honoured verbatim and a bare re-entry restores it", async ({ page }) => {
  await page.goto("/#/chat");
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible({ timeout: 30_000 });
  await expect.poll(() => new URL(page.url()).hash, { timeout: 15_000 }).toMatch(RESOLVED);
  const deepLink = new URL(page.url()).hash;

  await page.goto(`/${deepLink}`);
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible({ timeout: 30_000 });
  // Long enough for a stray normalisation to have fired had one been armed.
  await page.waitForTimeout(1_500);
  expect(new URL(page.url()).hash).toBe(deepLink);

  await page.goto("/#/chat");
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible({ timeout: 30_000 });
  await expect.poll(() => new URL(page.url()).hash, { timeout: 15_000 }).toBe(deepLink);
});
