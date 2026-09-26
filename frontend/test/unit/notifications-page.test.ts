// @vitest-environment jsdom

/**
 * The Notifications page's two decidable halves: which tab an address resolves
 * to, and where a row on the Activity list sends you.
 *
 * The page's *appearance* is not pinned here — `routed-views.ts` already holds
 * it to drawing a `PageHeader`, and the approvals queue inside it is covered by
 * the specs it already had. What is pinned is the two rules that are easy to
 * get subtly wrong and impossible to see in a screenshot:
 *
 *   - `#/approvals` must land on the queue **whatever `?tab=` says**, because
 *     it is the address six in-tree links and every bookmark point at. A stale
 *     `?tab=activity` left in the hash from a previous visit must not hijack a
 *     link to a blocked card's approvals.
 *   - a row must never link somewhere that is not about it. The host's `kind`
 *     and `subjectKind` are free-form by design, so an unknown subject has to
 *     render inert rather than be guessed at.
 *
 * The first of those is asserted by **rendering the page at each address** and
 * reading `aria-selected` off the strip, not by membership in `VIEWS`. Both
 * names can stay in that union while the forced tab, the `?tab=` read or the
 * task id is dropped — a routing regression the registry check cannot see
 * (CodeRabbit).
 */

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { NotificationDto, ApprovalSummary } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyFeed } from "@/hooks/use-company";
import { byNewestFirst, notificationHref } from "@/lib/notification-links";
import { VIEWS } from "@/lib/console-routes";
import { NotificationsView } from "@/views/NotificationsView";

const CHANNELS = {
  rendered: new Set(["desk-design", "desk-ops"]),
  mainChannelId: "desk-design",
};

function row(over: Partial<NotificationDto> = {}): NotificationDto {
  return {
    id: "n1",
    kind: "mention",
    subjectKind: "message",
    subjectId: "m1",
    title: "Priya mentioned you",
    createdAt: 1_000,
    context: "desk-ops",
    ...over,
  };
}

const NOW = new Date("2026-09-11T10:00:00Z").getTime();

const approval: ApprovalSummary = {
  id: "a1",
  kind: "web_fetch",
  amount_usd: null,
  at_millis: NOW,
};

const client = {
  get: async <T>(path: string): Promise<T> => (path.endsWith("/users") ? [] : null) as T,
  listGrants: async () => [],
  listTeam: async () => [],
  revokeGrant: async () => undefined,
  scopeFor: () => "/api/v1/company",
} as unknown as OpenCompanyClient;

const feed: CompanyFeed = {
  status: { pending_approvals: 1 } as CompanyFeed["status"],
  approvals: [approval],
  queue: "ready",
  now: NOW,
  refresh: async () => undefined,
};

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  window.location.hash = "";
});

/** Render the page as the shell would, at whatever hash is currently set. */
function renderPage(over: { forceApprovalsTab?: boolean; sub?: string | null } = {}) {
  act(() => {
    root.render(
      createElement(NotificationsView, {
        client,
        company: "acme",
        feed,
        notifications: [],
        channels: CHANNELS,
        onNotificationsRead: () => undefined,
        onResolved: () => undefined,
        onGoToConversation: () => undefined,
        ...over,
      }),
    );
  });
}

/** Which tab the strip reports as selected — the page's own answer, not ours. */
function selectedTab(): string | undefined {
  const tab = container.querySelector("[role=tab][aria-selected=true]");
  return tab?.getAttribute("data-testid")?.replace("notifications-tab-", "");
}

function tabTrigger(id: string): HTMLButtonElement {
  return container.querySelector(`[data-testid=notifications-tab-${id}]`) as HTMLButtonElement;
}

describe("both addresses the page answers", () => {
  it("routes #/notifications and keeps #/approvals alive beside it", () => {
    // Retiring `#/approvals` was the obvious move and is the wrong one:
    // `REWRITE_RETIRED` maps `[head, sub] -> [View, sub]` with no query
    // channel, so `#/approvals/<taskId>` could not have been rewritten onto
    // `?task=` without dropping the id. Both heads stay routable and the shell
    // renders one page for them.
    expect(VIEWS).toContain("notifications");
    expect(VIEWS).toContain("approvals");
  });

  it("opens on Approvals when the address names no tab", () => {
    window.location.hash = "#/notifications";
    renderPage();
    expect(selectedTab()).toBe("approvals");
  });

  it("opens on Activity when the address asks for it", () => {
    window.location.hash = "#/notifications?tab=activity";
    renderPage();
    expect(selectedTab()).toBe("activity");
  });

  it("ignores a stale ?tab= on the legacy approvals head", () => {
    // The rule this file's header states, now actually exercised: a
    // `?tab=activity` left over from a previous visit must not hijack a link
    // minted at the queue.
    window.location.hash = "#/approvals?tab=activity";
    renderPage({ forceApprovalsTab: true });
    expect(selectedTab()).toBe("approvals");
  });
});

describe("choosing a tab from the legacy approvals head", () => {
  it("moves to #/notifications rather than writing ?tab= onto a forced hash", () => {
    // `tab` is forced to the queue on this head, so the hash setter alone
    // changed the address and left the queue on screen — the operator clicked
    // Activity and nothing happened (Codex).
    window.location.hash = "#/approvals";
    renderPage({ forceApprovalsTab: true });
    act(() => tabTrigger("activity").click());
    expect(window.location.hash).toBe("#/notifications?tab=activity");
  });

  it("carries the rest of the address across the move", () => {
    // `?host=` rides every navigation (`use-host-route.ts`); dropping it here
    // would strand the console rendering one host under an address naming none.
    window.location.hash = "#/approvals?host=alpha";
    renderPage({ forceApprovalsTab: true });
    act(() => tabTrigger("activity").click());
    expect(window.location.hash).toBe("#/notifications?host=alpha&tab=activity");
  });

  it("leaves #/approvals/<taskId> exactly where it is when Approvals is chosen", () => {
    // Re-selecting the tab already on screen must not navigate: `#/approvals`
    // is the only head that can carry a board task id (#883), and moving to
    // `#/notifications` would drop it.
    window.location.hash = "#/approvals/task-7";
    renderPage({ forceApprovalsTab: true, sub: "task-7" });
    act(() => tabTrigger("approvals").click());
    expect(window.location.hash).toBe("#/approvals/task-7");
    expect(selectedTab()).toBe("approvals");
  });
});

describe("where a row sends you", () => {
  it("opens the card a task row is about, not the board", () => {
    expect(notificationHref(row({ subjectKind: "task", subjectId: "t 1" }), CHANNELS)).toBe(
      "#/tasks/t%201",
    );
  });

  it("opens the run a run row is about", () => {
    expect(notificationHref(row({ subjectKind: "run", subjectId: "r1" }), CHANNELS)).toBe(
      "#/observatory/r1",
    );
  });

  it("sends an approval row to the whole queue, never to a narrowed one", () => {
    // `#/approvals/<id>` narrows on a BOARD TASK id (#883). An approval id is
    // not one, so a narrowed queue would match nothing and render "this card is
    // clear" — a lie about the row that sent the operator there.
    expect(notificationHref(row({ subjectKind: "approval", subjectId: "a1" }), CHANNELS)).toBe(
      "#/approvals",
    );
  });

  it("opens an ordinary workflow row on the workflows list", () => {
    expect(
      notificationHref(
        row({ kind: "mention", subjectKind: "workflow", subjectId: "wf1", context: "desk-ops" }),
        CHANNELS,
      ),
    ).toBe("#/workflows");
  });

  it("opens a workflow-report row on the report's DM, not the workflows list", () => {
    // `report_to_operator` journals the report into the responsible agent's
    // DM and files a `workflow_report` notification with that DM as
    // `context` — the general `workflow` case ignores `context` entirely, so
    // this row needs its own branch or it opens a list with no report in it.
    const dm = "dm:product_manager";
    expect(
      notificationHref(
        row({ kind: "workflow_report", subjectKind: "workflow", subjectId: "wf1", context: dm }),
        { rendered: new Set([dm]), mainChannelId: "desk-design" },
      ),
    ).toBe(`#/chat/${dm}`);
  });

  it("renders a workflow-report row inert if it names no DM", () => {
    expect(
      notificationHref(
        row({ kind: "workflow_report", subjectKind: "workflow", subjectId: "wf1", context: undefined }),
        CHANNELS,
      ),
    ).toBeNull();
  });

  it("resolves a message row through the channel the host recorded", () => {
    expect(notificationHref(row({ context: "desk-ops", subjectId: "412" }), CHANNELS)).toBe(
      "#/chat/desk-ops?m=h412",
    );
  });

  it("names the line, not just the room", () => {
    // A mention's `subject.id` is the host's own message sequence
    // (`company/runtime.rs`), and `MessageRow` keys its rows on the console id
    // — `h`-prefixed. Linking the bare sequence lands in the channel and finds
    // nothing to scroll to, which is what `search/sources.ts` documents having
    // learned. Without the anchor at all, a mention above the fold in a busy
    // channel opens the room and leaves the operator to hunt (Codex).
    expect(notificationHref(row({ context: "desk-ops", subjectId: "412" }), CHANNELS)).toContain(
      "?m=h412",
    );
    // A row carrying no subject is still a link to the room. `RoomView` gives
    // up quietly on an anchor it cannot find, so the degraded case here is the
    // behaviour this had before the anchor existed, not a broken link.
    expect(notificationHref(row({ context: "desk-ops", subjectId: "" }), CHANNELS)).toBe(
      "#/chat/desk-ops",
    );
  });

  it("leaves a DM channel id unescaped, because the router never decodes", () => {
    // A DM id is `dm:<member-id>` (`room/channels.ts`) and `readSegments`
    // splits the hash on `/` with no decoding anywhere downstream, so
    // percent-encoding the `:` produces `dm%3A…` — a segment that matches no
    // channel. Every DM mention's row would have been a link to nothing, and
    // the other cases in this suite could not see it: `desk-ops` and
    // `desk-design` have no character encoding touches (tinysweeper).
    //
    // The `?m=` value on the end stays encoded, and the contrast is the rule:
    // `RoomView` reads it through `URLSearchParams`, which decodes; the path
    // segment reaches `readSegments`, which does not.
    const dm = "dm:ada-1f3k";
    expect(
      notificationHref(row({ context: dm, subjectId: "9" }), {
        rendered: new Set([dm]),
        mainChannelId: "desk-design",
      }),
    ).toBe(`#/chat/${dm}?m=h9`);
  });

  it("lands a legacy general-chat context on the rendered main channel", () => {
    // The same resolution the mention badge and the shell's thread re-read
    // share (issue #65) — not a second copy of the rule.
    expect(notificationHref(row({ context: "general", subjectId: "7" }), CHANNELS)).toBe(
      "#/chat/desk-design?m=h7",
    );
  });

  it("renders inert rather than guessing, for a subject it does not know", () => {
    // The host has no kind allowlist on purpose, so a future producer can write
    // a subject this console has never seen. The row still shows its title; it
    // simply is not a link.
    expect(notificationHref(row({ subjectKind: "moonbase" }), CHANNELS)).toBeNull();
    expect(notificationHref(row({ subjectKind: "task", subjectId: "" }), CHANNELS)).toBeNull();
    expect(notificationHref(row({ context: undefined }), CHANNELS)).toBeNull();
  });
});

describe("the order the list reads in", () => {
  it("is newest first, without mutating the shell's own array", () => {
    // The array handed to the list is React state in `app-shell`. Sorting it in
    // place would mutate that behind React's back.
    const feed = [row({ id: "old", createdAt: 1 }), row({ id: "new", createdAt: 9 })];
    expect(byNewestFirst(feed).map((n) => n.id)).toEqual(["new", "old"]);
    expect(feed.map((n) => n.id)).toEqual(["old", "new"]);
  });
});
