// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { FinancesView } from "@/views/FinancesView";
import { HostingView } from "@/views/HostingView";
import { InvoicingView } from "@/views/finance/InvoicingView";
import { PeopleView } from "@/views/PeopleView";
import { SearchView } from "@/views/SearchView";
import { TaskDetailView } from "@/views/TaskDetailView";
import { WalletView } from "@/views/finance/WalletView";

/**
 * A page keeps its name while it loads and when it fails (codex review on
 * #1785).
 *
 * # Why this cannot be the source scan
 *
 * `page-header-adoption.test.ts` reads files. A file that contains the string
 * `<PageHeader` satisfies it — and both of these did, while returning early
 * past that header in two states. **A source scan cannot see control flow.**
 * `SearchView` rendered no `h1` at all while its read was in flight, and none
 * ever once the read failed, because nothing retries it: a screen reader got a
 * page with no accessible name and no way out of it.
 *
 * So this renders the component and asks the DOM, which is the only thing that
 * can answer the question. Four pages rather than one because they are a
 * copy-paste family and the defect was in all of them — a sweep of
 * `src/views/**` for an early `return` above a `PageHeader` found this shape in
 * Search, Hosting, Wallet and Invoicing, which is why fixing only the one that
 * was reported would have been the wrong answer.
 *
 * # What a failure here means
 *
 * Someone added an early `return` above the page's header. Move the header
 * above the conditionals — read it into a const and render it in each branch —
 * rather than deleting the branch.
 */

/** A client whose one read resolves, rejects, or never settles. */
function clientWith(answer: unknown): OpenCompanyClient {
  // One answer for every read these pages make. `finances` is its own client
  // method rather than a bare `get`, so a stub with only `get` reported
  // "client.finances is not a function" and told us nothing about headings.
  const reply = (url?: string) =>
    answer instanceof Error
      ? Promise.reject(answer)
      : answer === "pending"
        ? new Promise(() => {})
        : Promise.resolve(typeof answer === "function" ? answer(url ?? "") : answer);
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get: (url?: string) => reply(url),
    post: (url?: string) => reply(url),
    finances: () => reply(),
  } as unknown as OpenCompanyClient;
}

const HOSTING_OK = {
  apiKeyConfigured: true,
  provider: "vercel",
  team: "team_abc",
  granted: true,
  inBuild: true,
  supportedProviders: ["vercel"],
};

const PAYPAL_OK = {
  connected: true,
  granted: true,
  inBuild: true,
  environment: "sandbox",
  credentialConfigured: true,
};

const CHARGEBEE_OK = {
  connected: true,
  granted: true,
  inBuild: true,
  site: "acme-test",
  credentialConfigured: true,
};

const FINANCES_OK = {
  balanceUsd: 0,
  revenueUsd: 0,
  spentUsd: 0,
  netUsd: 0,
  budgetUsd: null,
  currency: "USD",
  byCategory: [],
  transactions: [],
};

/**
 * People makes three reads with different shapes, so its fixture answers by
 * URL. It has to be an *admin*, or the view takes its members-only branch and
 * the loaded case has nothing to assert on — which is precisely what the
 * ready-state assertion caught when this fixture was a bare `[]`.
 */
const PEOPLE_OK = (url: string): unknown =>
  url.endsWith("/auth/me")
    ? { id: "u1", email: "admin@example.test", role: "admin" }
    : [];

const SEARCH_OK = {
  provider: "managed",
  effectiveProvider: "managed",
  endpoint: null,
  apiKeyConfigured: false,
  granted: true,
  inBuild: true,
  supportedProviders: ["managed", "brave", "exa"],
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
  vi.restoreAllMocks();
});

/** The page's one accessible name, or null when it has none. */
type Page =
  | typeof SearchView
  | typeof HostingView
  | typeof WalletView
  | typeof InvoicingView
  | typeof FinancesView
  | typeof PeopleView;

async function nameOf(view: Page, answer: unknown): Promise<string | null> {
  await act(async () => {
    root.render(createElement(view, { client: clientWith(answer), company: "acme" }));
  });
  const headings = container.querySelectorAll("h1");
  expect(headings.length, "a page has exactly one h1").toBeLessThan(2);
  return headings[0]?.textContent?.trim() ?? null;
}

describe.each([
  ["Search", SearchView, SEARCH_OK, "search-load-error", "search-view"] as const,
  ["Hosting", HostingView, HOSTING_OK, "hosting-load-error", "hosting-view"] as const,
  ["Wallet", WalletView, PAYPAL_OK, "wallet-status-error", "wallet-view"] as const,
  ["Invoicing", InvoicingView, CHARGEBEE_OK, "invoicing-status-error", "invoicing-view"] as const,
])("%s is named in every state", (title, view, ok, errorTestId, readyTestId) => {
  /*
    Every state now renders the same title, which is the fix — and which
    hollowed out an assertion on the title alone: an invalid fixture, or a
    "success" that actually errored, would render the error branch and still
    say "Search" (coderabbit review). So each case pins the *state* as well as
    the name, using the testid that exists only on the loaded return.
  */
  const at = (testid: string) => container.querySelector(`[data-testid="${testid}"]`);

  it("names the page once it has loaded, and is really loaded", async () => {
    expect(await nameOf(view, ok)).toBe(title);
    expect(at(readyTestId), "the loaded fixture must reach the loaded branch").not.toBeNull();
    expect(at(errorTestId), "and must not be quietly erroring").toBeNull();
  });

  it("names the page while the read is still in flight", async () => {
    expect(await nameOf(view, "pending")).toBe(title);
    expect(at(readyTestId)).toBeNull();
    expect(at(errorTestId)).toBeNull();
  });

  it("names the page when the read failed, which is a state it never leaves", async () => {
    expect(await nameOf(view, new Error("store unreachable"))).toBe(title);
    // And the failure is still reported — a header that swallowed the alert
    // would pass the assertion above and be worse than the defect.
    expect(at(errorTestId)?.textContent).toContain("store unreachable");
    expect(at(readyTestId)).toBeNull();
  });
});

/**
 * The two pages the sweep found outside the settings family.
 *
 * Separate `describe` because their failure shapes differ from the four above:
 * `FinancesView` has three load states rather than two (it tells a host with no
 * finances route apart from a read that failed), and `PeopleView`'s header
 * changes with the reader's role, so the state worth pinning is that nothing
 * offers `Invite` before the role is known. Once it is known the button is
 * correct even while the member list is still arriving — verified in a browser
 * on 2026-08-26, where `me` settles before the list and the button appears with
 * the skeletons still on screen.
 */
describe.each([
  // Neither has a loaded-only testid, so the ready signal is content only the
  // loaded branch renders: Finances' KPI labels, People's members section.
  ["Finances", FinancesView, FINANCES_OK, "Wallet balance"] as const,
  ["People", PeopleView, PEOPLE_OK, "Members"] as const,
])("%s is named in every state", (title, view, ok, readySignal) => {
  it("names the page once it has loaded, and is really loaded", async () => {
    expect(await nameOf(view, ok)).toBe(title);
    expect(
      container.textContent,
      "the loaded fixture must reach the loaded branch",
    ).toContain(readySignal);
  });

  it("names the page while the read is still in flight", async () => {
    expect(await nameOf(view, "pending")).toBe(title);
    expect(container.textContent).not.toContain(readySignal);
  });

  it("names the page when the read failed", async () => {
    expect(await nameOf(view, new Error("ledger unreachable"))).toBe(title);
    expect(container.textContent).not.toContain(readySignal);
  });
});

describe("People offers no Invite before it knows the reader is an admin", () => {
  it("has no actions in the loading state", async () => {
    await act(async () => {
      root.render(
        createElement(PeopleView, { client: clientWith("pending"), company: "acme" }),
      );
    });
    expect(container.querySelector("h1")?.textContent).toBe("People");
    const invite = [...container.querySelectorAll("button")].find((b) =>
      b.textContent?.includes("Invite"),
    );
    expect(invite, "Invite must not be offered before the role is known").toBeUndefined();
  });
});

/**
 * The card detail pane, which is not a settings page but has the same shape
 * and was the fifth finding of this class (codex review on #1785).
 *
 * Its heading is the card's own title, inside `DetailHeader`, which needs a
 * loaded record. So a cold `#/tasks/<id>` was unnamed while the read was in
 * flight and stayed unnamed after a non-404 failure — `detail` is left null and
 * nothing retries. The `notFound` branch had already been given a heading; this
 * is the branch a per-file check could not see, because the file already
 * contained one.
 */
describe("the task detail pane is named before the card is", () => {
  async function taskName(answer: unknown): Promise<string | null> {
    await act(async () => {
      root.render(
        createElement(TaskDetailView, {
          client: clientWith(answer) as never,
          company: "acme",
          taskId: "task-1",
          onBack: () => {},
        } as never),
      );
    });
    const headings = container.querySelectorAll("h1");
    expect(headings.length, "a page has exactly one h1").toBeLessThan(2);
    return headings[0]?.textContent?.trim() ?? null;
  }

  it("names the pane while the record is still loading", async () => {
    expect(await taskName("pending")).toBe("Task");
  });

  it("names the pane after a read that leaves no record", async () => {
    expect(await taskName(new Error("board unreachable"))).toBe("Task");
  });
});
