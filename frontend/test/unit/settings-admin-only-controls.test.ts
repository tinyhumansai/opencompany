// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { HostingView } from "@/views/HostingView";
import { SearchView } from "@/views/SearchView";

/**
 * Settings must not offer a member a control whose only outcome is a refusal.
 *
 * Every write behind these two pages is `AdminScopedCompany`, so a member's
 * Save answers `403 only an admin can do that`. Both pages already resolved the
 * viewer's role before this suite existed — and wired the answer into the tool-
 * grant banner alone, leaving the credential field, Save and Disconnect
 * rendering enabled. Knowing the role and using it were separate acts, and the
 * pages did the first.
 *
 * The assertion that matters most is the credential field's absence rather than
 * its disabled-ness. A disabled password box is still somewhere to aim a paste,
 * and a member who pastes a live key learns they were not allowed only after
 * the key has left their password manager.
 */

const HOSTING = {
  apiKeyConfigured: true,
  provider: "vercel",
  team: "team_abc",
  granted: true,
  inBuild: true,
  supportedProviders: ["vercel"],
};

const SEARCH = {
  provider: "brave",
  effectiveProvider: "brave",
  endpoint: null,
  apiKeyConfigured: true,
  needsApiKey: false,
  needsEndpoint: false,
  granted: true,
  inBuild: true,
  supportedProviders: ["managed", "brave", "searxng"],
};

/** A client answering the page's own read, and `/auth/me` as `role`. */
function clientAs(role: "admin" | "member", answer: unknown): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get: (path: string) =>
      path.endsWith("/auth/me")
        ? Promise.resolve({ id: "u1", email: "a@b.c", role, company: "acme", hasPassword: true })
        : Promise.resolve(answer),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(element: React.ReactElement) {
  await act(async () => {
    root.render(element);
  });
}

function at(testid: string): HTMLElement | null {
  return container.querySelector<HTMLElement>(`[data-testid="${testid}"]`);
}

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

describe("Settings → Hosting, by role", () => {
  it("offers a member no credential field, no Save and no Disconnect", async () => {
    const client = clientAs("member", HOSTING);
    await show(createElement(HostingView, { client, company: "acme" }));

    expect(at("hosting-api-key")).toBeNull();
    expect(at("hosting-save")).toBeNull();
    expect(at("hosting-clear")).toBeNull();
  });

  it("tells a member why, rather than leaving the page silently plainer", async () => {
    // Withholding the controls without saying so would trade a dishonest
    // button for a missing one: the member would go looking for a control that
    // is not theirs and conclude the page is broken.
    const client = clientAs("member", HOSTING);
    await show(createElement(HostingView, { client, company: "acme" }));

    const notice = at("hosting-read-only");
    expect(notice).not.toBeNull();
    expect(notice?.textContent).toContain("Only an admin");
  });

  it("still shows a member what is connected", async () => {
    // The read is not admin-only, and a member has a reason to want it: it is
    // what explains why a teammate can deploy at all.
    const client = clientAs("member", HOSTING);
    await show(createElement(HostingView, { client, company: "acme" }));

    expect(at("hosting-connected")).not.toBeNull();
    expect(at("hosting-team")).not.toBeNull();
  });

  it("offers an admin the whole form and no notice", async () => {
    // The control. Every assertion above is only worth having if the admin
    // path still renders what the member's does not.
    const client = clientAs("admin", HOSTING);
    await show(createElement(HostingView, { client, company: "acme" }));

    expect(at("hosting-read-only")).toBeNull();
    expect(at("hosting-api-key")).not.toBeNull();
    expect(at("hosting-save")).not.toBeNull();
    expect(at("hosting-clear")).not.toBeNull();
  });
});

describe("Settings → Search, by role", () => {
  it("offers a member no key field and no way to change the provider", async () => {
    const client = clientAs("member", SEARCH);
    await show(createElement(SearchView, { client, company: "acme" }));

    expect(at("search-api-key")).toBeNull();
    expect(at("search-save")).toBeNull();
    expect(at("search-clear")).toBeNull();
  });

  it("leaves the provider picker visible to a member but not operable", async () => {
    // Which index answers a teammate's search is worth reading even when it is
    // not yours to change — the page's own footnote is about exactly that.
    const client = clientAs("member", SEARCH);
    await show(createElement(SearchView, { client, company: "acme" }));

    const picker = at("search-provider");
    expect(picker).not.toBeNull();
    expect(picker?.hasAttribute("disabled") || picker?.getAttribute("aria-disabled") === "true").toBe(
      true,
    );
  });

  it("states the rule the page's own footnote has always asserted", async () => {
    // The page has always ended with "the choice is an administrator's and not
    // a teammate's". Until this gate existed it printed that under an enabled
    // picker and an enabled Save.
    const client = clientAs("member", SEARCH);
    await show(createElement(SearchView, { client, company: "acme" }));

    const notice = at("search-read-only");
    expect(notice).not.toBeNull();
    expect(notice?.textContent).toContain("Only an admin");
    // The footnote's own characters, typographic apostrophe included — this is
    // the sentence the page was printing under an enabled picker.
    expect(container.textContent).toContain("an administrator’s and not a teammate’s");
  });

  it("offers an admin the whole form and no notice", async () => {
    const client = clientAs("admin", SEARCH);
    await show(createElement(SearchView, { client, company: "acme" }));

    expect(at("search-read-only")).toBeNull();
    expect(at("search-api-key")).not.toBeNull();
    expect(at("search-save")).not.toBeNull();
    expect(at("search-clear")).not.toBeNull();
  });
});
