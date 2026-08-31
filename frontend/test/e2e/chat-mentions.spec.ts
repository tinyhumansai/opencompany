import { expect, test, type Page } from "@playwright/test";

/**
 * End-to-end proof for the `@`-mention composer.
 *
 * Three things a unit test of the pure module cannot show, because all three
 * are about the composer's *keyboard*, which only exists in a browser:
 *
 * 1. **Enter picks and does not send.** Somebody mid-`@name` is choosing a
 *    person, not finishing a message. Sending there is unrecoverable — the
 *    half-typed message is already in the channel — and it is the single most
 *    likely way to get this feature wrong.
 * 2. The picker opens on a real `@` and stays shut inside an email address.
 * 3. What the console *sends* carries structured mentions, so the host resolves
 *    what the person actually picked rather than re-guessing from the text.
 *
 * Mocks the operator API, following `chat-channel-membership.spec.ts`: the
 * inputs that matter are a known directory and an inspectable chat POST body,
 * and only a stub produces those on demand.
 */

const COMPANY = "acme";

const DESKS = [
  { id: "engineering", name: "Engineering", description: "Ships it", members: ["engineer", "ceo"] },
];

const ROSTER = [
  { id: "engineer", name: "Ada", role: "Backend Engineer" },
  { id: "ceo", name: "Rae", role: "Chief Executive" },
];

const MENTIONABLES = {
  agents: ROSTER,
  people: [{ id: "u1", label: "Jane Doe", slug: "jane-doe" }],
  desks: [{ id: "engineering", name: "Engineering", memberIds: ["engineer", "ceo"] }],
  everyone: { label: "everyone", aliases: ["everyone", "channel", "here"] },
};

type SentChat = {
  text: string;
  chat?: string;
  mentions?: Array<{ target: { kind: string; id?: string }; text: string; offset: number }>;
};

let sent: SentChat[] = [];

async function mockApi(page: Page, overrides: { mentionables?: "missing" } = {}) {
  // The first-run tour renders a modal over the console and swallows clicks.
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });

  await page.route("**/api/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    const json = (body: unknown, status = 200) =>
      route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
    const status = { id: COMPANY, name: "Acme", lifecycle: "running", pending_approvals: 0 };

    if (path === "/api/v1/companies") return json([status]);
    if (path === `/api/v1/companies/${COMPANY}`) return json(status);
    if (path.endsWith("/desks")) return json(DESKS);
    if (path.endsWith("/team")) return json(ROSTER);
    if (path.endsWith("/chat/mentionables")) {
      return overrides.mentionables === "missing"
        ? json({ error: "not_found" }, 404)
        : json(MENTIONABLES);
    }
    if (path.endsWith("/chat/read-state")) return json({ markers: [] });
    if (path.endsWith("/chat/history")) return json([]);
    if (path.endsWith("/chat")) {
      const body = route.request().postDataJSON() as SentChat;
      sent.push(body);
      return json({ responses: [{ text: `echo: ${body.text}`, channel: body.chat }] });
    }
    if (path.endsWith("/events")) {
      return route.fulfill({ status: 200, contentType: "text/event-stream", body: "" });
    }
    if (path.endsWith("/me")) return json({ id: "op", email: "op@example.com", role: "member" });
    return json([]);
  });
}

const composer = (page: Page) => page.getByPlaceholder(/^Message /);
const picker = (page: Page) => page.getByTestId("mention-picker");
const options = (page: Page) => page.getByTestId("mention-option");

async function openChannel(page: Page, channelId: string) {
  await page.goto(`/#/chat/${channelId}`);
  await expect(composer(page)).toBeVisible({ timeout: 30_000 });
}

test.beforeEach(() => {
  sent = [];
});

test("typing @ opens the picker and filters as you type", async ({ page }) => {
  await mockApi(page);
  await openChannel(page, "engineering");

  await composer(page).click();
  await composer(page).pressSequentially("hey @");
  await expect(picker(page)).toBeVisible();

  // Everything is offered before a query narrows it.
  await expect(options(page).first()).toBeVisible();

  await composer(page).pressSequentially("eng");
  // The teammate wins the exact query even though a desk also starts with it.
  await expect(options(page).first()).toContainText("Ada");
});

/**
 * The assertion this whole spec exists for. Enter with the picker open must
 * pick, never send — the message is half-typed and sending it cannot be undone.
 */
test("Enter picks the highlighted row and does not send", async ({ page }) => {
  await mockApi(page);
  await openChannel(page, "engineering");

  await composer(page).click();
  await composer(page).pressSequentially("hey @eng");
  await expect(picker(page)).toBeVisible();
  await composer(page).press("Enter");

  // Picked: the draft now holds the full handle, and the picker is shut.
  await expect(composer(page)).toHaveValue(/@Ada\s$/);
  await expect(picker(page)).toHaveCount(0);
  // And crucially, nothing was sent. Poll briefly so an in-flight request
  // cannot make this negative assertion pass by timing alone.
  await expect.poll(() => sent.length, { timeout: 1_000, intervals: [100] }).toBe(0);
});

test("Enter with the picker shut sends, and carries the resolved mention", async ({ page }) => {
  await mockApi(page);
  await openChannel(page, "engineering");

  await composer(page).click();
  await composer(page).pressSequentially("hey @eng");
  await composer(page).press("Enter"); // picks
  await composer(page).pressSequentially("what is the build status?");
  await composer(page).press("Enter"); // sends

  await expect.poll(() => sent.length).toBe(1);
  const body = sent[0];
  expect(body.text).toContain("@Ada");
  // Structured, not re-guessed from the text: the host is told exactly who the
  // person picked.
  expect(body.mentions).toHaveLength(1);
  expect(body.mentions?.[0].target).toEqual({ kind: "agent", id: "engineer" });
  expect(body.mentions?.[0].text).toBe("@Ada");
  // The span has to actually sit at the recorded offset, or the chip renders
  // over the wrong characters.
  const { offset, text } = body.mentions![0];
  expect(body.text.slice(offset, offset + text.length)).toBe("@Ada");
});

test("Escape closes the picker and leaves the draft alone", async ({ page }) => {
  await mockApi(page);
  await openChannel(page, "engineering");

  await composer(page).click();
  await composer(page).pressSequentially("hey @eng");
  await expect(picker(page)).toBeVisible();
  await composer(page).press("Escape");

  await expect(picker(page)).toHaveCount(0);
  await expect(composer(page)).toHaveValue("hey @eng");
  await expect.poll(() => sent.length, { timeout: 1_000, intervals: [100] }).toBe(0);
});

/** An email address is the case a naive `@` trigger gets wrong every time. */
test("the picker does not open inside an email address", async ({ page }) => {
  await mockApi(page);
  await openChannel(page, "engineering");

  await composer(page).click();
  await composer(page).pressSequentially("write to jane@eng");
  await expect(picker(page)).toHaveCount(0);
});

test("a two-word person is reachable, space and all", async ({ page }) => {
  await mockApi(page);
  await openChannel(page, "engineering");

  await composer(page).click();
  await composer(page).pressSequentially("thanks @Jane Do");
  // The query survives the space — without that, a display name with one in it
  // can never be picked.
  await expect(picker(page)).toBeVisible();
  await expect(options(page).first()).toContainText("Jane Doe");

  await composer(page).press("Enter");
  await expect(composer(page)).toHaveValue(/@Jane Doe\s$/);
});

test("backspacing through a chip un-mentions it", async ({ page }) => {
  await mockApi(page);
  await openChannel(page, "engineering");

  await composer(page).click();
  await composer(page).pressSequentially("hey @eng");
  await composer(page).press("Enter"); // picks `@Ada `
  // Delete the trailing space and one character of the handle.
  await composer(page).press("Backspace");
  await composer(page).press("Backspace");
  await composer(page).pressSequentially(" hello");
  await composer(page).press("Enter");

  await expect.poll(() => sent.length).toBe(1);
  // The text no longer contains the handle the mention recorded, so the mention
  // must go with it rather than pinging somebody who is not named any more.
  expect(sent[0].mentions ?? []).toHaveLength(0);
});

test("a host with no mention directory still sends, with no picker", async ({ page }) => {
  await mockApi(page, { mentionables: "missing" });
  await openChannel(page, "engineering");
  await composer(page).click();
  await composer(page).pressSequentially("hey @engineer");
  // No picker, and Enter therefore sends — the composer's behaviour before the
  // feature existed, which is what an older host must keep getting.
  await expect(picker(page)).toHaveCount(0);
  await composer(page).press("Enter");

  await expect.poll(() => sent.length).toBe(1);
  expect(sent[0].text).toContain("@engineer");
  // Nothing structured to send; the host extracts from the text instead.
  expect(sent[0].mentions).toBeUndefined();
});
