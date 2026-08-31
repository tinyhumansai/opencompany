import { expect, test, type Page } from "@playwright/test";

/**
 * End-to-end proof for the mention badge.
 *
 * The badge is the **durable** half of mentions: the company SSE feed only
 * reaches a browser that is open, so a mention that landed overnight is visible
 * here and nowhere else. Two properties matter, and neither is provable from a
 * unit test of the counting function alone, because both are about what the
 * console *does* with the feed:
 *
 * 1. A mention badge is not the unread badge. It is a separate row element with
 *    its own meaning, and it shows even on a channel you have open — having a
 *    channel open is not an answer to a question somebody asked you.
 * 2. Opening a channel clears **only that channel's** mentions. A bare
 *    "mark all read" would silently clear a summons waiting elsewhere, which is
 *    exactly the message somebody would then never answer, with nothing left on
 *    screen to notice it by.
 *
 * Mocks the operator API rather than driving a live host, following
 * `chat-channel-membership.spec.ts`: the interesting inputs are a feed with
 * mentions in two channels at once and a `PUT` whose body has to be inspected,
 * and only a stub produces those on demand.
 */

const COMPANY = "acme";

const DESKS = [
  // A third channel with no mentions of its own, so a test can open *somewhere*
  // without the act of looking clearing a badge it is about to assert on. The
  // console resolves an unknown channel id by falling back to the first desk,
  // so a `general` that is not in this list would silently open Engineering —
  // and clear exactly the badge under test.
  { id: "general", name: "General", description: "The main line", members: ["ceo"] },
  { id: "engineering", name: "Engineering", description: "Ships it", members: ["ceo"] },
  { id: "design", name: "Design", description: "Draws it", members: ["ceo"] },
];

const ROSTER = [{ id: "ceo", name: "Rae", role: "Chief Executive" }];

type Note = {
  id: string;
  kind: string;
  subjectKind: string;
  subjectId: string;
  title: string;
  createdAt: number;
  context?: string;
  readAt?: number;
};

/** Every mark-read body the console sent, so the clearing rule can be asserted. */
let marked: Array<{ ids?: string[] }> = [];

function seedFeed(): Note[] {
  return [
    {
      id: "eng-1",
      kind: "mention",
      subjectKind: "message",
      subjectId: "10",
      title: "Ada mentioned you in engineering",
      createdAt: 3,
      context: "engineering",
    },
    {
      id: "eng-2",
      kind: "mention",
      subjectKind: "message",
      subjectId: "11",
      title: "Ada mentioned you in engineering",
      createdAt: 2,
      context: "engineering",
    },
    {
      id: "design-1",
      kind: "mention",
      subjectKind: "message",
      subjectId: "12",
      title: "Ada mentioned you in design",
      createdAt: 1,
      context: "design",
    },
  ];
}

async function mockApi(
  page: Page,
  feed: Note[],
  options: {
    historyGates?: Record<string, Promise<void>>;
    /** Per-desk transcripts to serve; a desk with no entry gets `[]`. */
    history?: Record<string, unknown[]>;
  } = {},
) {
  // The first-run tour renders a modal over the console and swallows clicks.
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });

  await page.route("**/api/v1/**", async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname;
    const json = (body: unknown, status = 200) =>
      route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
    const status = { id: COMPANY, name: "Acme", lifecycle: "running", pending_approvals: 0 };

    if (path === "/api/v1/companies") return json([status]);
    if (path === `/api/v1/companies/${COMPANY}`) return json(status);
    if (path.endsWith("/desks")) return json(DESKS);
    if (path.endsWith("/team")) return json(ROSTER);

    if (path.endsWith("/notifications")) {
      if (route.request().method() === "PUT") {
        const body = (route.request().postDataJSON() ?? {}) as { ids?: string[] };
        marked.push(body);
        // Mark exactly what was named, so the next poll reflects the host's
        // real answer rather than the console's optimism.
        for (const n of feed) {
          if (body.ids?.includes(n.id)) n.readAt = 99;
        }
        return json({ unread: feed.filter((n) => n.readAt === undefined).length });
      }
      return json({
        notifications: feed,
        unread: feed.filter((n) => n.readAt === undefined).length,
      });
    }

    if (path.endsWith("/chat/read-state")) return json({ markers: [] });
    if (path.endsWith("/chat/history")) {
      // A test proving the race holds one channel's response open until it
      // says so — the console must not have marked that channel's mentions
      // read while its own history is still on the wire.
      const desk = url.searchParams.get("desk") ?? "";
      const gate = options.historyGates?.[desk];
      if (gate) await gate;
      return json(options.history?.[desk] ?? []);
    }
    if (path.endsWith("/events")) {
      return route.fulfill({ status: 200, contentType: "text/event-stream", body: "" });
    }
    if (path.endsWith("/me")) return json({ id: "op", email: "op@example.com", role: "member" });
    return json([]);
  });
}

async function openChannel(page: Page, channelId: string) {
  await page.goto(`/#/chat/${channelId}`);
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible({ timeout: 30_000 });
}

/** The mention badge on one rail row, by the channel's visible name. */
function mentionBadge(page: Page, channelName: string) {
  return page
    .getByRole("button", { name: new RegExp(channelName, "i") })
    .getByTestId("channel-mentions");
}

test.beforeEach(() => {
  marked = [];
});

test("a mention badge shows the count, per channel, and is not the unread badge", async ({
  page,
}) => {
  await mockApi(page, seedFeed());
  // Open a channel with no mentions of its own, so neither badge under test is
  // cleared by the act of looking.
  await openChannel(page, "general");

  await expect(mentionBadge(page, "Engineering")).toHaveText("@2");
  await expect(mentionBadge(page, "Design")).toHaveText("@1");

  // Distinct from unread, and distinctly labelled: the mention badge carries no
  // "only in this tab" caveat, because the host recorded who was named.
  await expect(mentionBadge(page, "Engineering")).toHaveAttribute(
    "title",
    /mentions of you here/i,
  );
});

test("opening a channel clears only its own mentions", async ({ page }) => {
  // The seed mentions name messages 10 and 11 (`subjectId`), so the mocked
  // history has to carry them — `mentionsToClear` only clears a mention whose
  // summoning text is actually in the loaded transcript (codex P1).
  await mockApi(page, seedFeed(), {
    history: {
      engineering: [
        { id: "10", channel: "engineering", author: "ceo", text: "please review the invoice", atMillis: 3, mine: false },
        { id: "11", channel: "engineering", author: "ceo", text: "and the contract", atMillis: 2, mine: false },
      ],
    },
  });
  await openChannel(page, "general");
  await expect(mentionBadge(page, "Engineering")).toBeVisible();

  await openChannel(page, "engineering");

  // Engineering's badge goes...
  await expect(mentionBadge(page, "Engineering")).toHaveCount(0);
  // ...and Design's stays. This is the assertion the whole spec exists for: a
  // bare "mark all read" passes every other line here and fails this one.
  await expect(mentionBadge(page, "Design")).toHaveText("@1");

  // And the console named ids rather than asking for everything. An `ids`-less
  // PUT means "mark all" to the host, so sending one here would clear Design
  // server-side even though the badge above still renders — the badge would
  // come back wrong on the next reload rather than immediately.
  // The PUT is fire-and-forget on the app side (the optimistic clear already
  // took the badge off the rail), so wait for it to actually land rather than
  // racing the network against the DOM assertions above.
  await expect
    .poll(() => marked.filter((m) => m.ids && m.ids.length > 0).length)
    .toBeGreaterThan(0);
  const clearing = marked.filter((m) => m.ids && m.ids.length > 0);
  for (const call of clearing) {
    expect(call.ids).not.toContain("design-1");
  }
  expect(clearing.flatMap((c) => c.ids ?? []).sort()).toContain("eng-1");
});

test("opening a channel does not clear its mentions before history has loaded", async ({
  page,
}) => {
  // The Codex P1 finding: `ChatView` used to report a channel "viewed" on the
  // same tick it switched to it, with no regard for whether that channel's
  // own `chat/history` had come back yet. A mention is durable and there is
  // no older-history pagination to recover one, so clearing it in that window
  // could lose the summons for good — worse than the local-only unread
  // estimate this shares a code path with.
  //
  // Reproducing the race needs the mention feed already loaded *before* the
  // switch (it is a poll independent of the active channel, and was already
  // sitting in memory by the time the bug fired in the field) and Engineering's
  // own history held open at the moment of the switch — which is why this
  // opens General first, ungated, and only gates Engineering's history.
  let releaseHistory: () => void = () => {};
  const historyGate = new Promise<void>((resolve) => {
    releaseHistory = resolve;
  });
  await mockApi(page, seedFeed(), {
    historyGates: { engineering: historyGate },
    // The message `eng-1` names (`subjectId: "10"`); a mention whose summoning
    // text is absent from the loaded transcript never clears (codex P1).
    history: {
      engineering: [
        { id: "10", channel: "engineering", author: "ceo", text: "please review the invoice", atMillis: 3, mine: false },
      ],
    },
  });

  await openChannel(page, "general");
  await expect(mentionBadge(page, "Engineering")).toHaveText("@2");

  await page.getByRole("button", { name: /engineering/i }).click();
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible();
  // Engineering's `chat/history` is still on the wire at this point.
  await page.waitForTimeout(200);
  expect(marked.some((m) => m.ids?.includes("eng-1"))).toBe(false);

  releaseHistory();
  await expect
    .poll(() => marked.some((m) => m.ids?.includes("eng-1")))
    .toBe(true);
});

test("a mention inside a thread reply stays unread until its thread is opened", async ({
  page,
}) => {
  // The mention lives in a reply, and replies are folded out of the main
  // timeline (`buildTimeline`) — they render only inside the thread panel that
  // opens on their parent. Opening the channel alone therefore shows nothing
  // of the summons, so the badge must not clear; it clears the moment the
  // thread panel makes the reply visible. The notification names the reply by
  // its host sequence (`subjectId: "21"`), and the loaded transcript's reply
  // map keys by the console id the same sequence prefixes (`h21`).
  await mockApi(page, [
    {
      id: "eng-thread-1",
      kind: "mention",
      subjectKind: "message",
      subjectId: "21",
      title: "Ada mentioned you in engineering",
      createdAt: 2,
      context: "engineering",
    },
  ], {
    history: {
      engineering: [
        {
          id: "20",
          channel: "engineering",
          author: "ceo",
          text: "the launch plan",
          atMillis: 1,
          mine: false,
        },
        {
          id: "21",
          channel: "engineering",
          author: "ceo",
          text: "@Rae can you sanity-check?",
          atMillis: 2,
          mine: false,
          parentId: "20",
        },
      ],
    },
  });
  await openChannel(page, "general");
  await expect(mentionBadge(page, "Engineering")).toHaveText("@1");

  // The channel is open and its history is on screen, but the reply itself is
  // still hidden inside the collapsed thread — the badge has to stay.
  await openChannel(page, "engineering");
  await expect(mentionBadge(page, "Engineering")).toHaveText("@1");
  expect(marked.some((m) => m.ids?.includes("eng-thread-1"))).toBe(false);

  // Opening the thread renders the reply, and only then is the summons
  // answered.
  await page.getByRole("button", { name: /1 reply/i }).click();
  await expect
    .poll(() => marked.some((m) => m.ids?.includes("eng-thread-1")))
    .toBe(true);
});

test("a collapsed section aggregates its hidden mentions, same as unread", async ({ page }) => {
  await mockApi(page, seedFeed());
  await openChannel(page, "general");
  await expect(mentionBadge(page, "Engineering")).toHaveText("@2");

  // Engineering and Design both live in the "Channels" section. Collapsing it
  // hides both rows, and with them the per-row badges under test above — the
  // only place those three mentions can still be seen is the header. Exact, so
  // the desktop rail's "Collapse channels" button (whose name contains the
  // section's) is not mistaken for the section toggle.
  await page.getByRole("button", { name: "Channels", exact: true }).click();

  await expect(mentionBadge(page, "Engineering")).toHaveCount(0);
  // Both rails (mobile, desktop) render the collapsed-section badge now, but
  // only the `lg+` one is on screen at this viewport — and a CSS `section`
  // locator matches hidden DOM, where the role-based locator above did not. So
  // the section is narrowed to the visible rail before reading its badge.
  const sectionMentions = page
    .locator("section")
    .filter({ hasText: "Channels" })
    .filter({ visible: true })
    .getByTestId("section-mentions");
  await expect(sectionMentions).toHaveText("@3");
});

test("a host with no notification route simply shows no mention badges", async ({ page }) => {
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
    // The pre-feature host.
    if (path.endsWith("/notifications")) return json({ error: "not_found" }, 404);
    if (path.endsWith("/chat/read-state")) return json({ markers: [] });
    if (path.endsWith("/chat/history")) return json([]);
    if (path.endsWith("/events")) {
      return route.fulfill({ status: 200, contentType: "text/event-stream", body: "" });
    }
    if (path.endsWith("/me")) return json({ id: "op", email: "op@example.com", role: "member" });
    return json([]);
  });

  await openChannel(page, "general");
  // Degrades to the previous console rather than to an error or an empty badge.
  await expect(page.getByTestId("channel-mentions")).toHaveCount(0);
  await expect(page.getByPlaceholder(/^Message /)).toBeVisible();
});
