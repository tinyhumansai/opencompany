import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

import { bubbles, openChannel } from "./chat-helpers";

/**
 * End-to-end proof for issue #2029 — a chat attachment may be any file in the
 * company's workspace, not only one the chat upload route stamped a mime onto.
 *
 * The Files tab stores a UTF-8 text upload as a prose note, and so does every
 * note an agent or the console writes. Those nodes are files, they are in the
 * tree, and attaching one to a message was refused as "not a file in this
 * company's workspace" — the first two files a human tried.
 *
 * What each test drives through the real console, and what it cannot:
 *
 * - The upload is the Files tab's own control, and the download is the chip's
 *   own click; both are the UI.
 * - The **attach** of an existing workspace node has no affordance in v1: the
 *   paperclip uploads a fresh copy through `…/chat/upload` and never references
 *   a node already in the tree. So the send that carries a workspace node id is
 *   posted over REST, the way the operator who found this did.
 * - `extracted_text` is deliberately absent from the console's wire shape
 *   (`ChatAttachmentDto`), so the "an over-cap note carries no extracted text"
 *   half is only assertable in the Rust suite. The observable half — it still
 *   attaches — is here.
 *
 * Like the rest of `test/e2e` this drives a running host; see
 * `playwright.config.ts` for how one is brought up.
 */

/** The single-company alias the host answers on, as the other chat specs use. */
const SCOPE = "/api/v1/company";

/** The harness manifest's engineering desk. Its id is its channel id. */
const DESK = "engineering";

/** The first-run tour renders over the console and swallows clicks beneath it. */
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

async function dismissOnboarding(page: Page) {
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip.waitFor({ state: "visible", timeout: 5_000 }).catch(() => {});
  if (await skip.isVisible()) {
    await skip.click();
    await expect(skip).toBeHidden();
  }
}

/**
 * Uploads one file through the Files tab's own Upload control and returns the
 * stored node's id.
 *
 * The picker's input is hidden behind the toolbar button, which is how a
 * browser file picker always is — `setInputFiles` is the supported way to drive
 * one, and the change handler it fires is the console's real upload path.
 */
async function uploadThroughFilesTab(
  page: Page,
  request: APIRequestContext,
  file: { name: string; mimeType: string; buffer: Buffer },
): Promise<string> {
  await page.goto("/#/workspace");
  await page.reload();
  await dismissOnboarding(page);
  await expect(page.getByTestId("workspace-tree")).toBeVisible({ timeout: 30_000 });

  await page.locator('input[type="file"]').setInputFiles(file);
  await expect(page.getByTestId("workspace-tree")).toContainText(file.name.replace(/\.\w+$/, ""), {
    timeout: 30_000,
  });

  return await nodeIdByName(request, file.name);
}

/** The id the host stored a node under, by the name it stored it under. */
async function nodeIdByName(request: APIRequestContext, name: string): Promise<string> {
  const tree = (await (await request.get(`${SCOPE}/workspace`)).json()) as {
    id: string;
    name: string;
  }[];
  const node = tree.find((n) => n.name === name);
  expect(node, `the host must hold a node named ${name}`).toBeTruthy();
  return node!.id;
}

/**
 * Sends a message carrying workspace node ids, from outside the browser.
 *
 * The step the console has no control for — see the header. Everything either
 * side of it is the UI.
 */
async function attachFromElsewhere(
  request: APIRequestContext,
  text: string,
  attachments: string[],
) {
  return await request.post(`${SCOPE}/chat`, {
    data: { text, chat: DESK, attachments },
  });
}

/**
 * The download chip for `name` in the open transcript.
 *
 * Located by the chip's own `title`, not its accessible name: the button's name
 * is its visible text — the filename and the rendered size — which would tie
 * every assertion here to how bytes happen to be formatted.
 */
function chip(page: Page, name: string) {
  return bubbles(page).locator(`button[title="Download ${name}"]`);
}

/**
 * Clicks a chip and returns what the blob route served for it.
 *
 * The click's own fetch is the assertion: the chip resolves the node through
 * the authenticated blob route and hands the bytes to the browser, so a chip
 * that renders over a 404 is caught here rather than looking fine on screen.
 */
async function downloadThroughChip(page: Page, name: string, nodeId: string): Promise<Buffer> {
  const served = page.waitForResponse(
    (response) => response.url().includes(`/workspace/blob/${nodeId}`),
    { timeout: 30_000 },
  );
  await chip(page, name).click();
  const response = await served;
  expect(
    response.ok(),
    `the chip's download failed: ${response.status()} ${await response.text()}`,
  ).toBeTruthy();
  return await response.body();
}

test("a note uploaded through the Files tab attaches to a message and downloads", async ({
  page,
  request,
}) => {
  const stamp = Date.now().toString(36);
  const name = `q3-notes-${stamp}.md`;
  const content = `# Q3 notes ${stamp}\n\nRevenue grew 12% year over year.\n`;

  const nodeId = await uploadThroughFilesTab(page, request, {
    name,
    mimeType: "text/markdown",
    buffer: Buffer.from(content, "utf8"),
  });

  // The upload was stored as a prose note — the shape the resolver used to
  // refuse. Asserted so a change in how the route classifies text does not
  // leave this test passing while covering nothing.
  const listed = (await (await request.get(`${SCOPE}/workspace`)).json()) as {
    id: string;
    mime?: string | null;
  }[];
  expect(listed.find((n) => n.id === nodeId)?.mime ?? null).toBeNull();

  const marker = `note-attach-${stamp}`;
  const posted = await attachFromElsewhere(request, marker, [nodeId]);
  expect(
    posted.ok(),
    `attaching a workspace note failed: ${posted.status()} ${await posted.text()}`,
  ).toBeTruthy();

  await openChannel(page, DESK);
  await expect(bubbles(page).filter({ hasText: marker }).first()).toBeVisible({
    timeout: 60_000,
  });
  await expect(chip(page, name)).toBeVisible({ timeout: 30_000 });

  const bytes = await downloadThroughChip(page, name, nodeId);
  expect(bytes.toString("utf8")).toBe(content);

  await request.delete(`${SCOPE}/workspace/${nodeId}`);
});

test("a note already in the workspace attaches on the same terms", async ({ page, request }) => {
  const stamp = Date.now().toString(36);
  const name = `roadmap-${stamp}.md`;
  const content = `# Roadmap ${stamp}\n\nShip the thing.`;

  // Written the way an agent's `workspace_write` or the company's seed does.
  const created = await request.post(`${SCOPE}/workspace`, {
    data: { name, kind: "file", content },
  });
  expect(created.ok()).toBeTruthy();
  const nodeId = ((await created.json()) as { id: string }).id;

  const marker = `seeded-attach-${stamp}`;
  const posted = await attachFromElsewhere(request, marker, [nodeId]);
  expect(
    posted.ok(),
    `attaching a seeded note failed: ${posted.status()} ${await posted.text()}`,
  ).toBeTruthy();

  await openChannel(page, DESK);
  await expect(bubbles(page).filter({ hasText: marker }).first()).toBeVisible({
    timeout: 60_000,
  });
  const bytes = await downloadThroughChip(page, name, nodeId);
  expect(bytes.toString("utf8")).toBe(content);

  await request.delete(`${SCOPE}/workspace/${nodeId}`);
});

test("a file picked with the paperclip still attaches, previews and downloads", async ({
  page,
}) => {
  const stamp = Date.now().toString(36);
  const name = `hero-${stamp}.png`;
  // A one-pixel PNG: real enough for the console to preview and for the host to
  // keep as bytes rather than prose.
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
    "base64",
  );

  await openChannel(page, DESK);
  await page.locator('input[type="file"]').setInputFiles({
    name,
    mimeType: "image/png",
    buffer: png,
  });
  // The staged chip in the composer, before the send.
  await expect(page.getByRole("button", { name: `Remove ${name}` })).toBeVisible({
    timeout: 30_000,
  });

  const marker = `paperclip-attach-${stamp}`;
  await page.getByPlaceholder(/^Message /).fill(marker);
  await page.getByRole("button", { name: "Send" }).click();

  await expect(bubbles(page).filter({ hasText: marker }).first()).toBeVisible({
    timeout: 60_000,
  });
  await expect(chip(page, name)).toBeVisible({ timeout: 30_000 });

  // The node id is not on the page, so this one matches the route by name-free
  // suffix: any blob fetch the click causes must be this chip's.
  const served = page.waitForResponse((r) => r.url().includes("/workspace/blob/"), {
    timeout: 30_000,
  });
  await chip(page, name).click();
  const response = await served;
  expect(response.ok()).toBeTruthy();
  expect((await response.body()).equals(png)).toBeTruthy();
});

test("a folder is refused, and the refusal names the reason", async ({ page, request }) => {
  const stamp = Date.now().toString(36);
  const created = await request.post(`${SCOPE}/workspace`, {
    data: { name: `designs-${stamp}`, kind: "folder" },
  });
  expect(created.ok()).toBeTruthy();
  const folderId = ((await created.json()) as { id: string }).id;

  const marker = `folder-attach-${stamp}`;
  const posted = await attachFromElsewhere(request, marker, [folderId]);
  expect(posted.status()).toBe(400);
  expect((await posted.text()).toLowerCase()).toContain("folder");

  // And it never reached the transcript: the refusal is before the journal.
  await openChannel(page, DESK);
  await expect(bubbles(page).filter({ hasText: marker })).toHaveCount(0);

  await request.delete(`${SCOPE}/workspace/${folderId}`);
});

test("a note past the extraction cap still attaches", async ({ page, request }) => {
  const stamp = Date.now().toString(36);
  const name = `huge-${stamp}.md`;
  // Past the 4 MiB the send route will read for extraction. The reference is
  // what the operator asked for, so it attaches either way; that it carries no
  // extracted text is asserted in the Rust suite, which can see the field.
  const content = "x".repeat(5 * 1024 * 1024);

  const nodeId = await uploadThroughFilesTab(page, request, {
    name,
    mimeType: "text/markdown",
    buffer: Buffer.from(content, "utf8"),
  });

  const marker = `huge-attach-${stamp}`;
  const posted = await attachFromElsewhere(request, marker, [nodeId]);
  expect(
    posted.ok(),
    `attaching an over-cap note failed: ${posted.status()} ${await posted.text()}`,
  ).toBeTruthy();

  await openChannel(page, DESK);
  await expect(bubbles(page).filter({ hasText: marker }).first()).toBeVisible({
    timeout: 60_000,
  });
  await expect(chip(page, name)).toBeVisible({ timeout: 30_000 });

  await request.delete(`${SCOPE}/workspace/${nodeId}`);
});
