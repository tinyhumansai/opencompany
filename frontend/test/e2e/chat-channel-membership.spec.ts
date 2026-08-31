import { expect, test, type Page } from "@playwright/test";

/**
 * End-to-end proof for issues #369 and #370 — a channel says who is in *it*,
 * and never shows you a different channel without saying so.
 *
 * #369: the header's count and the member pane both read the flat company
 * roster, so every channel claimed the same members — a two-person desk in a
 * seventeen-person company said 17, and so did a one-to-one DM.
 *
 * #370: `desks` was seeded with the static fallback set rather than treated as
 * unloaded, so a deep link resolved against fabricated channels and flashed
 * `#general` before swapping; a blanket `catch` made that permanent when
 * `/desks` failed; and an unresolvable channel id opened a different channel
 * in silence while the address bar kept naming the missing one.
 *
 * Unlike the live-host specs beside it, this one **mocks the operator API**
 * with `page.route`. Both defects are in what the console does with the host's
 * answer — per-desk membership it was discarding, and load states it was
 * collapsing — so the interesting inputs are a desk whose members differ from
 * the roster, a slow `/desks`, and a broken `/desks`. Only a stub can produce
 * those on demand. The page itself still comes from whatever `PW_BASE_URL`
 * serves the console, like the rest of the suite.
 *
 * Verified to fail against the pre-fix build: five of these seven go red.
 */

const COMPANY = "acme";

/** A roster deliberately much larger than any one desk — that gap is the bug. */
const ROSTER = Array.from({ length: 17 }, (_, i) => ({
  id: `agent-${i + 1}`,
  name: `Agent ${i + 1}`,
  role: `Role ${i + 1}`,
}));

const DESKS = [
  {
    id: "engineering",
    name: "Engineering",
    description: "Ships the product",
    // Lead first, and NOT in roster order: `members[0]` is the lead, so a fix
    // that sorts or filters the roster instead of walking these ids would
    // badge the wrong person here.
    members: ["agent-3", "agent-1"],
  },
  {
    id: "content",
    name: "Content",
    description: "Writes things",
    // `agent-99` is not on the roster — a teammate removed since the desks were
    // fetched. It must drop out rather than render a row for nobody.
    members: ["agent-5", "agent-6", "agent-7", "agent-99"],
  },
];

type DesksMode = "ok" | "404" | "500" | "slow";

/** Every chat POST the console made, so the send path can be asserted on. */
const sentChats: { text: string; chat?: string; detach?: boolean }[] = [];

/**
 * Stub the operator API.
 *
 * `mode` is read through a getter rather than captured, so a test can flip a
 * failing `/desks` to a healthy one and exercise the Retry button.
 */
async function mockApi(page: Page, mode: () => DesksMode) {
  // The first-run product tour renders a modal over the console and swallows
  // every click beneath it. Answer "already skipped" for any company id.
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

    if (path.endsWith("/desks")) {
      switch (mode()) {
        case "404":
          return json({ error: "not_found", message: "no desks surface" }, 404);
        case "500":
          return json({ error: "internal", message: "desks are broken" }, 500);
        case "slow":
          await new Promise((r) => setTimeout(r, 2500));
          return json(DESKS);
        default:
          return json(DESKS);
      }
    }

    if (path.endsWith("/team")) return json(ROSTER);
    if (path.endsWith("/chat")) {
      const body = route.request().postDataJSON() as { text: string; chat?: string; detach?: boolean };
      sentChats.push(body);
      return json({ responses: [{ text: `echo: ${body.text}`, channel: body.chat }] });
    }
    if (path.endsWith("/events")) {
      return route.fulfill({ status: 200, contentType: "text/event-stream", body: "" });
    }
    if (path.endsWith("/me")) return json({ id: "op", email: "op@example.com", role: "member" });
    return json([]);
  });
}

/** The header's member toggle — its label ends in "members". */
const membersToggle = (page: Page) => page.getByRole("button", { name: /teammates$/i });

/** The member pane; the channel rail is the other `complementary` on screen. */
const pane = (page: Page) => page.getByRole("complementary").last();

/**
 * Open the pane if it is shut.
 *
 * Not a plain click: the pane's open state lives in `ChatView`, and switching
 * channels is a hash-only navigation that does not remount it — so a second
 * click would close what the first opened.
 */
async function openPane(page: Page) {
  if ((await membersToggle(page).getAttribute("aria-pressed")) !== "true") {
    await membersToggle(page).click();
  }
  await expect(page.getByRole("heading", { name: "Team" })).toBeVisible();
}

async function openChannel(page: Page, channelId: string) {
  await page.goto(`/#/chat/${channelId}`);
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible({ timeout: 30_000 });
}

test("#369 each desk counts and lists its own members, not the company", async ({ page }) => {
  await mockApi(page, () => "ok");

  await openChannel(page, "engineering");
  await expect(membersToggle(page)).toHaveText(/2/);
  await openPane(page);
  await expect(pane(page)).toContainText("2 in this channel · 17 in the company");
  await expect(pane(page).getByRole("heading", { name: "In this channel" })).toBeVisible();
  await expect(pane(page).getByRole("heading", { name: "Everyone else" })).toBeVisible();

  // The desk's own order survives: the lead is `members[0]`, not whoever the
  // roster happens to list first.
  const inChannel = pane(page).locator("ul").first();
  await expect(inChannel.locator("li")).toHaveCount(2);
  await expect(inChannel.locator("li").first()).toContainText("Agent 3");
  await expect(inChannel.locator("li").first()).toContainText("Lead");
  await expect(inChannel.locator("li").nth(1)).toContainText("Agent 1");
  await expect(inChannel.locator("li").nth(1)).not.toContainText("Lead");
  // The rest of the company is still reachable, one section down.
  await expect(pane(page).locator("ul").nth(1).locator("li")).toHaveCount(15);

  // A second desk reads differently — and the id with no roster row is gone.
  await openChannel(page, "content");
  await expect(membersToggle(page)).toHaveText(/3/);
  await openPane(page);
  await expect(pane(page)).toContainText("3 in this channel · 17 in the company");
  await expect(pane(page).locator("ul").first().locator("li")).toHaveCount(3);
  await expect(pane(page)).not.toContainText("Agent 99");
});

test("#369 a DM reads as two people, not as the whole company", async ({ page }) => {
  await mockApi(page, () => "ok");
  await openChannel(page, "engineering");
  await openPane(page);

  // Agent 4 is on no desk, so this row is in "Everyone else" — which proves
  // that section's actions still work as well as opening the DM.
  await pane(page).getByText("Agent 4", { exact: true }).click();
  await expect(page.getByPlaceholder("Message Agent 4")).toBeVisible();
  // Two: the teammate and the operator, who has no roster row of their own.
  await expect(membersToggle(page)).toHaveText(/2/);
  await expect(pane(page).locator("ul").first().locator("li")).toHaveCount(1);
});

test("#369 a host with no desks surface still shows the whole roster", async ({ page }) => {
  await mockApi(page, () => "404");
  // `strategy`, not `general` (issue #1743): `#general` is no longer one of the
  // static fallback desks — it is derived from the roster and every teammate is
  // in it, which is what the test below this one pins. `strategy` is still a
  // fallback desk with `members` absent, so it is the fixture this test is
  // actually about: membership *unknown*, as distinct from membership empty.
  await openChannel(page, "strategy");

  // The fallback desks have no membership to scope to, so this is unchanged
  // behaviour on purpose — one plain roster, no "in this channel" claim.
  await expect(membersToggle(page)).toHaveText(/17/);
  await openPane(page);
  await expect(pane(page)).toContainText("17 teammates");
  await expect(pane(page).getByRole("heading", { name: "In this channel" })).toHaveCount(0);
  await expect(pane(page).locator("ul").first().locator("li")).toHaveCount(17);
});

test("#1743 #general is everyone, even with no desks surface", async ({ page }) => {
  // The other half of the case above, and the reason it had to move off
  // `general`. `#general` is not a desk and never comes from `/desks` — it is
  // built from the roster this render was handed — so its membership is known
  // even when `/desks` 404s, and it is the whole company by construction.
  await mockApi(page, () => "404");
  await openChannel(page, "general");

  await expect(membersToggle(page)).toHaveText(/17/);
  await openPane(page);
  await expect(pane(page)).toContainText("17 in this channel · 17 in the company");
  await expect(pane(page).getByRole("heading", { name: "In this channel" })).toBeVisible();
  await expect(pane(page).locator("ul").first().locator("li")).toHaveCount(17);
});

test("#370 a deep link never flashes a channel the company doesn't have", async ({ page }) => {
  // Record every composer placeholder the page renders, in-page and on every
  // frame. An out-of-process poll misses a flash that lasts a single paint,
  // which is exactly the length of the one under test.
  await page.addInitScript(() => {
    (window as unknown as { __placeholders: string[] }).__placeholders = [];
    const tick = () => {
      const seen = (window as unknown as { __placeholders: string[] }).__placeholders;
      const p = document
        .querySelector("textarea[placeholder], input[placeholder]")
        ?.getAttribute("placeholder");
      if (p && seen[seen.length - 1] !== p) seen.push(p);
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
  await mockApi(page, () => "slow");

  await page.goto("/#/chat/engineering");
  await expect(page.getByPlaceholder("Message #engineering")).toBeVisible({ timeout: 30_000 });

  const seen = await page.evaluate(
    () => (window as unknown as { __placeholders: string[] }).__placeholders,
  );
  // Before the fix this read ["Message #general", "Message #engineering"].
  expect(seen).toEqual(["Message #engineering"]);
});

test("#370 an unknown channel opens the first one and says so", async ({ page }) => {
  await mockApi(page, () => "ok");
  await openChannel(page, "does-not-exist");

  // Scoped to the notice rather than "the page's one status region": the
  // timeline raises a second one while a channel's history is still loading
  // (issue #934), and an unqualified `getByRole("status")` matches both.
  const notice = page.getByRole("status").filter({ hasText: /isn't a channel here/ });
  await expect(notice).toContainText("#does-not-exist");
  // `#general` rather than `#engineering` since issue #1743: the built-in
  // company-wide channel is prepended to every company's list, so "the first
  // one" is now `#general` in every company rather than whichever desk the
  // host happened to return first. The property under test is unchanged — the
  // notice names the channel you actually landed in.
  await expect(notice).toContainText("#general");
  // The hash is left alone deliberately — rewriting it needs replace-semantics
  // the shell does not thread through yet, and a push would fight the back
  // button. The notice is what closes the gap between URL and content.
  expect(page.url()).toContain("#/chat/does-not-exist");

  // It is derived from the hash, so navigating clears it with no dismiss.
  await page.getByRole("complementary").first().getByRole("button", { name: /content/i }).click();
  await expect(notice).toHaveCount(0);
});

test("#370 a broken /desks is a retryable error, not invented channels", async ({ page }) => {
  let mode: DesksMode = "500";
  await mockApi(page, () => mode);

  await page.goto("/#/chat/engineering");
  await expect(page.getByText("Couldn't load this company's channels")).toBeVisible({
    timeout: 30_000,
  });
  // The old blanket catch pinned the fabricated desks here, so the view showed
  // #general while the hash claimed a real desk, permanently.
  await expect(page.getByText("#general")).toHaveCount(0);

  mode = "ok";
  await page.getByRole("button", { name: "Retry" }).click();
  await expect(page.getByPlaceholder("Message #engineering")).toBeVisible({ timeout: 30_000 });
});

/** The way out to the org chart, added by #485. */
const manageLink = (page: Page) =>
  pane(page).getByRole("button", { name: "Manage on the org chart" });

const chart = (page: Page) => page.getByRole("tree", { name: "Company org chart" });

test("#485 a desk channel links out to its own desk on the org chart", async ({ page }) => {
  await mockApi(page, () => "ok");
  await openChannel(page, "engineering");
  await openPane(page);

  await expect(manageLink(page)).toBeVisible();
  await manageLink(page).click();

  // A desk's channel id *is* its desk id (`deskFromDto`), so the link needs no
  // mapping — and the address it lands on names the desk, not just the chart.
  await expect.poll(() => page.url()).toContain("#/company/engineering");
  await expect(chart(page)).toBeVisible({ timeout: 30_000 });

  const desk = chart(page)
    .locator('[role="treeitem"][aria-level="2"]')
    .filter({ hasText: "Engineering" });
  await expect(desk).toBeVisible();
  // Landed *on the desk*, not merely on the chart: the arrival ring and DOM
  // focus are what make a link into a nine-desk company useful.
  await expect(desk).toHaveAttribute("data-desk-focused", "true");
  await expect(desk).toBeFocused();
});

test("#485 a DM has no desk to manage", async ({ page }) => {
  await mockApi(page, () => "ok");
  await openChannel(page, "engineering");
  await openPane(page);
  // The positive control, in the same run: the link is offered here, so its
  // absence below is about DMs and not about the link never rendering.
  await expect(manageLink(page)).toBeVisible();

  await pane(page).getByText("Agent 4", { exact: true }).click();
  await expect(page.getByPlaceholder("Message Agent 4")).toBeVisible();

  // A DM still gets the "In this channel" section — it has a membership of
  // exactly one — but it is not a desk, so the chart has nothing to open.
  await expect(pane(page).getByRole("heading", { name: "In this channel" })).toBeVisible();
  await expect(manageLink(page)).toHaveCount(0);
});

test("#485 a fallback desk offers no link to a desk the host doesn't have", async ({ page }) => {
  await mockApi(page, () => "404");
  // `strategy`, not `general`, for the same reason as the #369 case above:
  // since issue #1743 `#general` is derived from the roster rather than being
  // one of the static fallback desks, so it is no longer an example of one.
  await openChannel(page, "strategy");
  await openPane(page);

  // No `.../desks` route at all, so these channels come from `lib/desks.ts` and
  // name desks the org chart cannot draw. Linking to one would land on an empty
  // chart, so no link is offered — the same reasoning that makes this pane fall
  // back to the whole roster here.
  await expect(pane(page).getByRole("heading", { name: "In this channel" })).toHaveCount(0);
  await expect(manageLink(page)).toHaveCount(0);
});

test("the send path and the thread id it addresses are undisturbed", async ({ page }) => {
  sentChats.length = 0;
  await mockApi(page, () => "ok");
  await openChannel(page, "content");

  await page.getByPlaceholder("Message #content").fill("ping");
  await page.getByPlaceholder("Message #content").press("Enter");
  await expect(page.getByText("echo: ping")).toBeVisible({ timeout: 30_000 });
  // A desk's channel id doubles as its host thread id; none of the above may
  // change what the composer addresses. `detach: true` rides along on every
  // send since issue #983 (the console always asks for the accept-and-poll
  // shape now), which is orthogonal to what this test guards — the identity
  // of `chat` — so it is asserted for rather than making the match partial.
  expect(sentChats.at(-1)).toEqual({ text: "ping", chat: "content", detach: true });
});
