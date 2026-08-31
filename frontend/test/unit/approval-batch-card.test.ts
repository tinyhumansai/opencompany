// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import type { ApprovalThreadLink } from "@/components/approval-card";
import { money } from "@/lib/language";
import { ApprovalRow } from "@/views/chat/ApprovalRow";

/**
 * The consolidated card's decisions (issue #842).
 *
 * This suite is normally for pure functions — see `vitest.config.ts` — and the
 * exception is earned the same way `provider-detail-render` earns it: the thing
 * under test *is* what reaches the operator's hand. The issue's whole claim is
 * that one click can answer three gated calls **without** widening what any of
 * them buys, and three of those claims are only true at the click:
 *
 *  1. one Approve resolves every item, each on its own id — so each approved
 *     call still mints its own host-scoped grant (#739) rather than one grant
 *     spanning the batch, and three fetches still produce three independently
 *     revocable standing permissions;
 *  2. the card is **all-or-nothing** — it answers every item it is still asking
 *     about, because the turn stays blocked until each parked call has a
 *     verdict (#469), so a decision that left one open would hold the turn
 *     while looking as though it had resolved the card;
 *  3. an item decided elsewhere — the Approvals page, another tab — is not
 *     re-resolved, and the card stops listing it as pending.
 *
 * A pure test of the grouping cannot reach any of them: it can see the card is
 * built, not what pressing it sends.
 */

const T0 = new Date("2026-03-02T10:00:00Z").getTime();

function approval(id: string, url: string): ApprovalSummary {
  return {
    id,
    kind: "web_fetch",
    amount_usd: null,
    at_millis: T0,
    agent: "seo",
    thread: "desk-marketing",
    batch: "turn-1",
    broadly_grantable: true,
    payload: { url },
  };
}

const ESPN = approval("a1", "https://espn.com/nba");
const BBC = approval("a2", "https://bbc.com/sport");
const GUARDIAN = approval("a3", "https://theguardian.com/uk");

function payment(id: string, to: string, amountUsd: number): ApprovalSummary {
  return {
    id,
    kind: "payment.send",
    amount_usd: amountUsd,
    at_millis: T0,
    agent: "seo",
    thread: "desk-marketing",
    batch: "turn-1",
    // A spend stays a per-call decision, so the full card offers no standing
    // scope — and the compact row must render the same way.
    broadly_grantable: false,
    payload: { to, amount_usd: amountUsd },
  };
}

const VENDOR = payment("p1", "vendor@example.test", 42.5);
const SUPPLIER = payment("p2", "supplier@example.test", 12);

function request(id: string, url: string, method: string, body?: unknown): ApprovalSummary {
  return {
    id,
    kind: "http_request",
    amount_usd: null,
    at_millis: T0,
    agent: "seo",
    thread: "desk-marketing",
    batch: "turn-1",
    broadly_grantable: true,
    payload: body === undefined ? { url, method } : { url, method, body },
  };
}

const GET_ITEMS = request("h1", "https://example.com/items", "GET");
const DELETE_ITEMS = request("h2", "https://example.com/items", "DELETE");

interface Decision {
  id: string;
  verdict: Verdict;
  scope: GrantScope;
}

let container: HTMLDivElement;
let root: Root;
let decisions: Decision[];

async function render(
  approvals: ApprovalSummary[],
  decided: Record<string, Verdict> = {},
  failed: Record<string, string> = {},
  deciding: ReadonlyMap<string, Verdict> = new Map(),
  compact = false,
  thread?: ApprovalThreadLink | null,
) {
  await act(async () => {
    root.render(
      createElement(ApprovalRow, {
        approvals,
        now: T0 + 60_000,
        askerNames: new Map([["seo", "SEO Specialist"]]),
        variant: compact ? ("compact" as const) : ("full" as const),
        thread,
        deciding,
        decided,
        failed,
        onDecide: (approval: ApprovalSummary, verdict: Verdict, scope: GrantScope) =>
          decisions.push({ id: approval.id, verdict, scope }),
      }),
    );
  });
}

/** Every item line on the card, in render order. */
function items(): HTMLElement[] {
  return [...container.querySelectorAll<HTMLElement>("[data-approval-item]")];
}

/** The one compact transcript row a fully settled turn leaves behind (#970). */
function receipts(): HTMLElement[] {
  return [...container.querySelectorAll<HTMLElement>("[data-approval-receipt]")];
}

function button(label: string): HTMLButtonElement {
  const match = [...container.querySelectorAll("button")].find((b) =>
    (b.textContent ?? "").includes(label),
  );
  if (!match) throw new Error(`no "${label}" button on the card: ${container.textContent}`);
  return match as HTMLButtonElement;
}

async function click(el: HTMLElement) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

/**
 * Toggle a checkbox or radio the way a person does — by clicking it.
 *
 * Deliberately **not** by assigning `.checked` first: React tracks an input's
 * last-rendered value to decide whether a click changed anything, so setting it
 * by hand makes the click look like a no-op and `onChange` never fires. The
 * click's own activation behaviour flips the box, which is both what a browser
 * does and what React is watching for.
 */
async function toggle(input: HTMLInputElement) {
  await act(async () => {
    input.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  decisions = [];
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the consolidated approval card", () => {
  it("lets actions wrap below a readable headline in single and batch cards (#1384)", async () => {
    // The chat column is narrower than the viewport in both reported cases, so
    // this has to be a width floor and wrapping contract on each headline — not
    // a viewport breakpoint. A 12rem title plus the icon and action pair cannot
    // fit in the narrow transcript, moving the pair to its own line instead of
    // reducing the title to one word per line. The floor is capped at the
    // card's own width so a column narrower than the icon plus a 12rem title
    // wraps rather than overflowing the card.
    await render([ESPN]);
    const singleActions = container.querySelector<HTMLElement>("[data-approval-actions]");
    expect(singleActions).not.toBeNull();
    expect(singleActions!.parentElement?.className).toContain("flex-wrap");
    expect(singleActions!.previousElementSibling?.className).toContain("min-w-[min(12rem,100%)]");

    await render([ESPN, BBC]);
    const batchActions = container.querySelector<HTMLElement>("[data-approval-actions]");
    expect(batchActions).not.toBeNull();
    expect(batchActions!.parentElement?.className).toContain("flex-wrap");
    expect(batchActions!.previousElementSibling?.className).toContain("min-w-[min(12rem,100%)]");
  });

  it("asks once for a turn's three gated calls, naming each of them", async () => {
    await render([ESPN, BBC, GUARDIAN]);

    const text = container.textContent ?? "";
    expect(text).toContain("SEO Specialist");
    expect(text).toContain("3 sign-offs");
    for (const url of [
      "https://espn.com/nba",
      "https://bbc.com/sport",
      "https://theguardian.com/uk",
    ]) {
      expect(text).toContain(url);
    }
    // One decision to make, not three — and no per-item control beside it.
    // Granularity is the Approvals page's job; offering it here too would be a
    // second copy of the same state to keep in step.
    expect(container.querySelectorAll("button")).toHaveLength(2);
    expect(items()).toHaveLength(3);
    expect(container.querySelectorAll('input[type="checkbox"]')).toHaveLength(0);
  });

  it("resolves every item on its own id, so each mints its own grant", async () => {
    await render([ESPN, BBC, GUARDIAN]);
    await click(button("Approve"));

    // Three resolves, not one batch resolve. The host has no batch to decide —
    // the park is the unit of truth, and a grant is minted per approval from
    // that approval's own arguments.
    expect(decisions).toEqual([
      { id: "a1", verdict: "approve", scope: { kind: "once" } },
      { id: "a2", verdict: "approve", scope: { kind: "once" } },
      { id: "a3", verdict: "approve", scope: { kind: "once" } },
    ]);
  });

  it("carries the chosen scope to every item, so each gets its own standing grant", async () => {
    await render([ESPN, BBC]);
    // The broader option. One choice on the card, one standing permission per
    // item — each scoped to that item's own host when the host mints it (#739),
    // which is why approving three fetches leaves three independently revocable
    // rows under Standing permissions rather than one that spans them.
    const forAPeriod = [...container.querySelectorAll<HTMLInputElement>('input[type="radio"]')][1];
    await toggle(forAPeriod);
    await click(button("Approve"));

    expect(decisions).toEqual([
      { id: "a1", verdict: "approve", scope: { kind: "tool", expiresInMillis: 60 * 60 * 1000 } },
      { id: "a2", verdict: "approve", scope: { kind: "tool", expiresInMillis: 60 * 60 * 1000 } },
    ]);
  });

  it("declines the whole batch with one Decline, granting nothing", async () => {
    await render([ESPN, BBC]);
    // Even with the broader scope selected: a decline has nothing to grant, so
    // it must not carry a duration the operator picked for a yes.
    const forAPeriod = [...container.querySelectorAll<HTMLInputElement>('input[type="radio"]')][1];
    await toggle(forAPeriod);
    await click(button("Decline"));

    expect(decisions).toEqual([
      { id: "a1", verdict: "deny", scope: { kind: "once" } },
      { id: "a2", verdict: "deny", scope: { kind: "once" } },
    ]);
  });

  it("stops listing an item decided on the Approvals page, and says how many are left", async () => {
    // The drift case: both surfaces open, one row approved over there. The card
    // must not go on claiming three things are pending.
    await render([ESPN, BBC, GUARDIAN], { a1: "approve" });

    const text = container.textContent ?? "";
    expect(text).toContain("Approved");
    expect(text).toContain("1 of 3 decided");
    // Still listed — the operator has to see their own decision land — but
    // shown as settled rather than as something still being asked about.
    expect(items()).toHaveLength(3);
    expect(items()[0].textContent).toContain("Approved");

    await click(button("Approve"));
    // And an approve here covers only what is still open. Re-resolving a1 would
    // be a second decision on an approval the host has already dropped.
    expect(decisions.map((d) => d.id)).toEqual(["a2", "a3"]);
  });

  it("names the item whose decision did not land, and does not call it pending", async () => {
    // The failure consolidation makes worse. One click, three resolves, and the
    // third fails: two effects are authorised and one is not. An item that
    // simply dropped back to its pending look would read as "still working",
    // and the operator's honest conclusion would be that they got all three.
    await render([ESPN, BBC, GUARDIAN], { a1: "approve", a2: "approve" }, { a3: "host is away" });

    const text = container.textContent ?? "";
    expect(text).toContain("Not recorded");
    expect(text).toContain("host is away");
    // Which one, on the row itself — a toast says a decision failed without
    // saying which, and is gone by the time the operator looks back.
    const failedRow = container.querySelector('[data-approval-failed="true"]');
    expect(failedRow?.getAttribute("data-approval-item")).toBe("a3");
    expect(failedRow?.textContent).toContain("https://theguardian.com/uk");
  });

  it("counts the failures honestly rather than claiming nothing was recorded", async () => {
    await render([ESPN, BBC, GUARDIAN], { a1: "approve", a2: "approve" }, { a3: "host is away" });

    // Two of the three DID take. "Nothing was recorded" would be a fresh lie in
    // place of the silence this replaces.
    const text = container.textContent ?? "";
    expect(text).toContain("1 of 3 weren't recorded");
    expect(text).not.toContain("None of the 3");
  });

  it("shows the settled verdict, not a stale failure, once the item resolves elsewhere", async () => {
    // Failed here, then resolved on the Approvals page or in another tab: the
    // item carries both a failure and a verdict. A failure describes one
    // *attempt*; the verdict describes the approval, and the host has already
    // acted on it. Saying "not recorded" over that would be the card
    // contradicting the queue — the drift this whole change exists to remove.
    await render([ESPN, BBC], { a2: "approve" }, { a2: "host is away" });

    const settled = container.querySelector('[data-approval-item="a2"]');
    expect(settled?.textContent).toContain("Approved");
    expect(settled?.textContent).not.toContain("Not recorded");
    expect(container.querySelector('[data-approval-failed="true"]')).toBeNull();
    // And the card counts only what is still open: a2 is decided, so it is not
    // one of the failures still waiting on anybody.
    expect(container.textContent ?? "").not.toContain("weren't recorded");
  });

  it("leaves the buttons live after a failure, because a retry is the way out", async () => {
    await render([ESPN, BBC, GUARDIAN], { a1: "approve", a2: "approve" }, { a3: "host is away" });

    expect(button("Approve").disabled).toBe(false);
    await click(button("Approve"));
    // Only the item that never landed is retried — the two that did are settled
    // and re-resolving them would be a second decision on approvals the host
    // has already dropped.
    expect(decisions.map((d) => d.id)).toEqual(["a3"]);
  });

  it("leaves one expandable release receipt for a fully approved turn", async () => {
    await render([ESPN, BBC, GUARDIAN], { a1: "approve", a2: "approve", a3: "approve" });

    // Three decisions from one parked turn do not become three permanent
    // transcript rows. The receipt is about the one release, not the clicks.
    expect(receipts()).toHaveLength(1);
    expect(receipts()[0].textContent).toContain(
      "Approved 3 actions — the teammate is picking it up now",
    );
    expect(container.querySelectorAll("button")).toHaveLength(0);

    // The individual verdicts remain inspectable, but do not flood the channel
    // until the operator asks for them.
    const disclosure = receipts()[0].querySelector("details");
    expect(disclosure?.open).toBe(false);
    expect(disclosure?.textContent).toContain("Show individual decisions");
    expect(items()).toHaveLength(3);
    expect(items().map((item) => item.textContent)).toEqual(
      expect.arrayContaining([
        expect.stringContaining("https://espn.com/nba"),
        expect.stringContaining("https://bbc.com/sport"),
        expect.stringContaining("https://theguardian.com/uk"),
      ]),
    );
  });

  it("summarizes mixed verdicts honestly once the turn releases", async () => {
    await render([ESPN, BBC], { a1: "approve", a2: "deny" });

    expect(receipts()).toHaveLength(1);
    expect(receipts()[0].textContent).toContain(
      "Approved 1 action and declined 1 action — the teammate is picking it up now",
    );
    // Nothing left to decide, so nothing left to press.
    expect(container.querySelectorAll("button")).toHaveLength(0);
  });

  it("says plainly when every action in a settled turn was declined", async () => {
    await render([ESPN, BBC], { a1: "deny", a2: "deny" });

    expect(receipts()).toHaveLength(1);
    expect(receipts()[0].textContent).toContain(
      "Declined 2 actions — the teammate will not take them",
    );
    expect(receipts()[0].querySelector("details")?.open).toBe(false);
  });

  it("keeps a settled single approval natural", async () => {
    await render([ESPN], { a1: "approve" });

    // Unlike the multi-item receipts above, a single-item card cannot know the
    // turn's stillAwaiting count is zero — #561's neutral "recorded" wording,
    // not a "picking it up now" claim this card has no basis for.
    expect(receipts()).toHaveLength(1);
    expect(receipts()[0].textContent).toBe("Approved — recorded");
    expect(receipts()[0].querySelector("details")).toBeNull();
  });

  it("renders a single-call turn exactly as it did before batching", async () => {
    await render([ESPN]);

    // No item list, no counts — the consolidation earns its furniture only when
    // there is something to consolidate.
    expect(items()).toHaveLength(0);
    expect(container.textContent ?? "").not.toContain("sign-offs");
    await click(button("Approve"));
    expect(decisions).toEqual([{ id: "a1", verdict: "approve", scope: { kind: "once" } }]);
  });

  it("keeps a chat approval to a quiet, differentiating two-line interruption", async () => {
    await render([ESPN], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row).not.toBeNull();
    expect(row?.textContent).toContain("Fetch a web page — https://espn.com/nba");
    expect(row?.textContent).toContain("Asked by SEO Specialist");
    expect(row?.querySelector('a[href="#/approvals"]')?.textContent).toBe("View details");
    // Payload and grant scope are detailed decisions, so they stay on the
    // Approvals page instead of making every interruption a full card.
    expect(row?.querySelectorAll('input[type="radio"]')).toHaveLength(0);
    expect(row?.className).not.toContain("border");

    const approve = button("Approve");
    expect(approve.className).toContain("hover:bg-primary");
    expect(approve.className.split(" ")).not.toContain("bg-primary");
  });

  it("links the compact chat row back to its conversation when the thread resolves", async () => {
    // The compact branch used to return before the `ApprovalMeta` call that
    // receives `thread`, so the value MessageTimeline constructs for it was
    // discarded and an inline card never said where the request was asked
    // (#1419). Forwarding it keeps the compact row linked too.
    await render(
      [ESPN],
      {},
      {},
      new Map(),
      true,
      { channelId: "marketing", label: "#marketing" },
    );

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row?.textContent).toContain("Asked in");
    const link = row?.querySelector<HTMLAnchorElement>('a[href="#/chat/marketing"]');
    expect(link?.textContent).toBe("#marketing");
  });

  it("shows the amount beside a monetary approval in the compact chat row", async () => {
    const PAYMENT: ApprovalSummary = {
      id: "a4",
      kind: "payment.send",
      amount_usd: 42.5,
      at_millis: T0,
      agent: "seo",
      thread: "desk-marketing",
      batch: "turn-1",
      // A spend stays a per-call decision, so the full card offers no standing
      // scope — and the compact row must render the same way.
      broadly_grantable: false,
      payload: { to: "vendor@example.test", amount_usd: 42.5 },
    };
    await render([PAYMENT], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    // An operator approving a payment must see its value beside the Approve
    // button, not just the recipient the first payload line happens to name.
    expect(row?.textContent).toContain(`Send a payment — vendor@example.test · ${money(42.5)}`);
  });

  it("names every call and amount in a same-kind compact batch, not just the lead's", async () => {
    // Two payments from one turn: the lead's line already shows its own value,
    // and the second's has to appear too — one Approve authorizes both.
    await render([VENDOR, SUPPLIER], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row?.textContent).toContain(
      `Send a payment — vendor@example.test, supplier@example.test · ${money(42.5)} · ${money(12)}`,
    );
  });

  it("names every same-kind call in a compact batch, not just the lead's", async () => {
    // Three fetches from one turn: the second and third URLs are the
    // consequential ones — one Approve authorizes all three. "+ 2 more" would
    // hide a sketchy destination behind a harmless first fetch.
    await render([ESPN, BBC, GUARDIAN], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row?.textContent).toContain(
      `Fetch a web page — https://espn.com/nba, https://bbc.com/sport, https://theguardian.com/uk`,
    );
    expect(row?.textContent).not.toContain("+ 2 more");
  });

  it("names every action, call and amount in a mixed compact batch", async () => {
    // A fetch and a payment in one turn: "Fetch a web page + 1 more" would
    // hide the payment (and its amount) behind the lead. The line must say
    // what the one Approve actually covers — the distinct actions, and each
    // call's own detail, so the operator sees which page and which recipient
    // before clicking Approve.
    await render([ESPN, VENDOR], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row?.textContent).toContain(
      `2 actions need your sign-off — Fetch a web page and Send a payment — https://espn.com/nba, vendor@example.test · ${money(42.5)}`,
    );
  });

  it("counts a mixed compact batch with duplicate actions honestly", async () => {
    // Two fetches and a payment: the distinct actions are named once each, the
    // count says there are three of them, and every call's own URL or
    // recipient is named — the one Approve covers all of it.
    await render([ESPN, BBC, VENDOR], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row?.textContent).toContain(
      `3 actions need your sign-off — Fetch a web page and Send a payment — https://espn.com/nba, https://bbc.com/sport, vendor@example.test · ${money(42.5)}`,
    );
    // The duplicate fetch is not repeated as a bare action on the line.
    expect(row?.textContent).not.toContain("Fetch a web page, Fetch a web page");
  });

  it("keeps a role-hidden call's warning in a mixed compact batch", async () => {
    // A hidden payment beside a fetch: #618's flag must survive the mixed
    // summary, or the operator would approve a call whose payload says nothing
    // about what it does.
    const HIDDEN: ApprovalSummary = {
      id: "a6",
      kind: "payment.send",
      amount_usd: null,
      at_millis: T0,
      agent: "seo",
      thread: "desk-marketing",
      batch: "turn-1",
      broadly_grantable: false,
      payload: null,
      contents_hidden: true,
    };
    await render([ESPN, HIDDEN], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row?.textContent).toContain(
      `2 actions need your sign-off — Fetch a web page and Send a payment — https://espn.com/nba, Send a payment — details hidden by your role`,
    );
  });

  it("names a hidden approval once in the compact chat row", async () => {
    const HIDDEN: ApprovalSummary = {
      id: "a5",
      kind: "payment.send",
      amount_usd: null,
      at_millis: T0,
      agent: "seo",
      thread: "desk-marketing",
      batch: "turn-1",
      broadly_grantable: false,
      payload: null,
      contents_hidden: true,
    };
    await render([HIDDEN], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    // #618's flag reads as "not shown to you", never as an empty card — and the
    // action must not be printed twice.
    expect(row?.textContent).toContain("Send a payment — details hidden by your role");
    expect(row?.textContent ?? "").not.toContain("Send a payment — Send a payment");
  });

  it("summarizes only what the compact row's buttons still decide", async () => {
    // The drift case in the compact row: one of three already approved on the
    // page, and the row must not go on claiming all three are on the table.
    // The Approve button left here authorizes only the two still open, so the
    // line names them and lets the status say what was already decided.
    await render([ESPN, BBC, GUARDIAN], { a1: "approve" }, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    const text = row?.textContent ?? "";
    expect(text).toContain(
      "Fetch a web page — https://bbc.com/sport, https://theguardian.com/uk",
    );
    // The status still tells the operator one has been settled elsewhere.
    expect(text).toContain("1 of 3 decided — 2 still waiting on you");
    // The settled item is not named as something the buttons here will touch.
    expect(text).not.toContain("https://espn.com/nba");

    await click(button("Approve"));
    // And an approve here covers only what the label named — re-resolving a1
    // would be a second decision on an approval the host has already dropped.
    expect(decisions.map((d) => d.id)).toEqual(["a2", "a3"]);
  });

  it("names the HTTP method beside a request URL in the compact chat row", async () => {
    // The method is the difference between a read and a delete on the same URL:
    // a row that showed only the address would render GET and DELETE
    // identically even though approving them has very different effects.
    await render([DELETE_ITEMS], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row?.textContent).toContain(
      "Make a request to a web address — DELETE https://example.com/items",
    );
  });

  it("names each request's method in a compact batch, not just the lead's", async () => {
    // Same URL, opposite effects: the second item's method is the consequential
    // half of the one Approve, so it has to be on the line, not hidden behind
    // a count or a shared address.
    await render([GET_ITEMS, DELETE_ITEMS], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row?.textContent).toContain(
      "Make a request to a web address — GET https://example.com/items, DELETE https://example.com/items",
    );
  });

  it("shows what a write sends, not just where it goes, in the compact row", async () => {
    // Two POSTs to the same address are only the same decision if the payload
    // is the same: the body is what the one Approve authorizes. A row that
    // showed only "POST https://example.com/items" would render a write that
    // ships a document and a body-less read identically.
    const WRITE = request("h3", "https://example.com/items", "POST", {
      message: "ship it",
    });
    await render([WRITE], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row?.textContent).toContain(
      'Make a request to a web address — POST {"message":"ship it"} https://example.com/items',
    );
  });

  it("previews a long request body rather than flooding the compact row", async () => {
    // The row's job is to distinguish two requests, not to carry the file; a
    // giant body is previewed with an ellipsis, and the detailed view still
    // shows the full payload.
    const body = { message: "x".repeat(120) };
    const bodyJson = JSON.stringify(body);
    expect(bodyJson.length).toBeGreaterThan(60);
    const WRITE = request("h4", "https://example.com/items", "POST", body);
    await render([WRITE], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    expect(row?.textContent).toContain(
      `Make a request to a web address — POST ${bodyJson.slice(0, 60)}… https://example.com/items`,
    );
    // And the preview is not the whole body.
    expect(row?.textContent).not.toContain(bodyJson);
  });

  it("gates the inline Approve when a body preview was cut (#1330 review)", async () => {
    // A preview is not the payload: two POSTs to the same URL whose bodies share
    // the first 60 code units render identically even when the cut-off suffix
    // changes an amount or recipient. The compact row must not one-click
    // Approve on that — the operator has to see the complete host-bounded
    // payload (the detailed view) first, so the inline Approve is replaced by a
    // path there. Decline is always safe and stays inline.
    const body = { recipient: "vendor@example.test", message: "x".repeat(120) };
    const bodyJson = JSON.stringify(body);
    expect(bodyJson.length).toBeGreaterThan(60);
    const WRITE = request("h5", "https://example.com/items", "POST", body);
    await render([WRITE], {}, {}, new Map(), true);

    const row = container.querySelector<HTMLElement>('[data-approval-inline="compact"]');
    // The cut label names the row and admits it is cut…
    expect(row?.textContent).toContain("…");
    expect(row?.textContent).toContain("Review in Approvals");
    // …but there is no live inline Approve button on the truncated preview, and
    // a decline is still one press.
    expect(() => button("Approve")).toThrow();
    expect(() => button("Decline")).not.toThrow();
    // Nothing was decided: the row only offered the detailed view.
    expect(decisions).toEqual([]);
  });
});
