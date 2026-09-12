// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { SearchView } from "@/views/SearchView";

/**
 * Connections → Search always has a Managed row.
 *
 * The row is not a record and is not conditional on one: managed search is what
 * the absence of everything else means, so it is the one thing on this page that
 * is true of every deployment. It was nonetheless unreachable on exactly the
 * deployment that most needed it — a fresh company on a host with no managed
 * credential — because the list short-circuited to an empty state whenever
 * nothing else resolved, and the sentence explaining managed search was printed
 * as that empty state's sub-line instead of on the row it describes.
 *
 * What the operator saw there was "No search providers connected. No managed
 * credential on this deployment." and an Add button, with no indication that
 * Managed is a thing this page has at all.
 *
 * So the four combinations below are asserted against the rendered page rather
 * than against `managedSubline`, which was always correct. The defect was never
 * in the sentence; it was in whether anything rendered it.
 */

interface Fixture {
  providers: unknown[];
  managedConfigured: boolean;
  inBuild?: boolean;
}

const BRAVE = {
  slug: "brave",
  label: "Brave Search",
  category: "account",
  enabled: true,
  keyConfigured: true,
  takesKey: true,
  takesEndpoint: false,
  endpoint: null,
  complete: true,
  isDefault: true,
};

function status({ providers, managedConfigured, inBuild = true }: Fixture) {
  return {
    provider: "brave",
    effectiveProvider: providers.length > 0 ? "brave" : "managed",
    providers,
    endpoint: null,
    apiKeyConfigured: providers.length > 0,
    needsApiKey: false,
    needsEndpoint: false,
    granted: true,
    inBuild,
    managedConfigured,
    managedDailyCallCap: 250,
    supportedProviders: ["managed", "brave", "exa", "querit", "searxng"],
  };
}

/** An admin client answering the page's own read, and `/auth/me` as `admin`. */
function clientFor(fixture: Fixture): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get: (path: string) =>
      path.endsWith("/auth/me")
        ? Promise.resolve({
            id: "u1",
            email: "a@b.c",
            role: "admin",
            company: "acme",
            hasPassword: true,
          })
        : Promise.resolve(status(fixture)),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function render(fixture: Fixture) {
  await act(async () => {
    root.render(
      createElement(SearchView, {
        client: clientFor(fixture),
        company: "acme",
      }),
    );
  });
}

function at(testid: string): HTMLElement | null {
  return container.querySelector<HTMLElement>(`[data-testid="${testid}"]`);
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

describe("the Managed row on Connections → Search", () => {
  it("renders with nothing connected and no managed credential", async () => {
    // The defect, in the state it was reachable in. This is the deployment an
    // operator meets on day one of a self-hosted install.
    await render({ providers: [], managedConfigured: false });

    const managed = at("search-provider-managed");
    expect(managed).not.toBeNull();
    expect(managed?.textContent).toContain("Managed");
    expect(managed?.textContent).toContain(
      "No managed credential on this deployment",
    );
    // The claim that managed search *works* is a different claim from the row
    // existing, and only the first of them is false here.
    expect(at("search-provider-managed-on")).toBeNull();
  });

  it("keeps the CTA a new operator needs beside that row", async () => {
    // The empty state folded into the list rather than replacing it, so both
    // are on the page at once: what you have, and how to add to it.
    await render({ providers: [], managedConfigured: false });

    expect(at("search-provider-list")?.getAttribute("data-state")).toBe(
      "empty",
    );
    expect(at("search-provider-empty")?.textContent).toContain(
      "No search providers connected",
    );
    const add = at("search-provider-empty")?.querySelector("button");
    expect(add?.textContent).toContain("Add a provider");
    expect(add?.hasAttribute("disabled")).toBe(false);
    // Said only where it is true — with no records and nothing behind the
    // Managed row, no teammate can search at all.
    expect(at("search-provider-dead-end")).not.toBeNull();
  });

  it("renders with nothing connected and a managed credential", async () => {
    await render({ providers: [], managedConfigured: true });

    const managed = at("search-provider-managed");
    expect(managed?.textContent).toContain("Metered — up to 250 searches a day");
    expect(at("search-provider-managed-on")).not.toBeNull();
    // Still no records of this company's own, so the notice and its CTA stay —
    // but managed answers, so the dead end is not claimed.
    expect(at("search-provider-list")?.getAttribute("data-state")).toBe(
      "empty",
    );
    expect(at("search-provider-empty")).not.toBeNull();
    expect(at("search-provider-dead-end")).toBeNull();
  });

  it("renders above a connected provider, and drops the notice", async () => {
    await render({ providers: [BRAVE], managedConfigured: false });

    expect(at("search-provider-list")?.getAttribute("data-state")).toBe(
      "populated",
    );
    expect(at("search-provider-managed")).not.toBeNull();
    expect(at("search-provider-brave")).not.toBeNull();
    expect(at("search-provider-empty")).toBeNull();
    expect(at("search-provider-dead-end")).toBeNull();

    // Managed first: it is the fallback the rest of the list is measured
    // against, so it is not sorted in among the records.
    const rows = Array.from(
      container.querySelectorAll<HTMLElement>(
        '[data-testid="search-provider-list"] > li',
      ),
    ).map((li) => li.getAttribute("data-testid"));
    expect(rows[0]).toBe("search-provider-managed");
  });

  it("says so on a build with no search tools at all", async () => {
    // The third state of the sub-line, and the one where the row is the only
    // thing that says it inside the list.
    await render({
      providers: [],
      managedConfigured: false,
      inBuild: false,
    });

    expect(at("search-provider-managed")?.textContent).toContain(
      "This build has no search tools",
    );
    expect(at("search-provider-managed-on")).toBeNull();
  });
});
