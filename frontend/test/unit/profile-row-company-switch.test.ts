// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { Me } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { ProfileRow } from "@/components/profile-row";
import { SidebarProvider } from "@/components/ui/sidebar";

/**
 * Which person the sidebar footer shows while the operator moves between
 * companies (issue #1676 review).
 *
 * `me` is a read keyed by the scope it was fetched for. The failure this pins:
 * when the company prop changes, the previous company's record used to stay on
 * screen — and, worse, was the one a save through the profile dialog would have
 * written to the new company — until the new fetch happened to resolve. A pure
 * test cannot reach that: the bug is the *gap* between the prop change and the
 * new fetch landing, which only exists once the component is mounted and
 * re-rendered.
 */

function deferred<T>() {
  let settle!: (value: T) => void;
  const promise = new Promise<T>((resolve) => {
    settle = resolve;
  });
  return { promise, settle };
}

function meFor(company: string): Me {
  return {
    id: `${company}-me`,
    email: `me@${company}.test`,
    displayName: `${company} user`,
    role: "member",
    company,
    hasPassword: false,
    mustChangePassword: false,
  };
}

function host() {
  const reads: string[] = [];
  const patches: { path: string; body: unknown }[] = [];
  const beta = deferred<Me>();
  const client = {
    scopeFor: (company: string | null) =>
      company === null ? "/api/v1/company" : `/api/v1/companies/${company}`,
    get: async (path: string) => {
      const company = path.match(/companies\/([^/]+)\//)?.[1] ?? "";
      reads.push(company);
      // Only the second company's read is held open, so the test can look at
      // the row between "the scope changed" and "the new identity arrived".
      if (company === "beta") return beta.promise;
      // A company with no sign-in has no `me` to read; the console answers 404
      // and the row is expected to stay empty.
      if (company === "ghost") throw new Error("no sign-in");
      return meFor(company);
    },
    patch: async (path: string, body: unknown) => {
      patches.push({ path, body });
      const company = path.match(/companies\/([^/]+)\//)?.[1] ?? "";
      return meFor(company);
    },
  } as unknown as OpenCompanyClient;
  return { client, reads, patches, releaseBeta: (who: Me) => beta.settle(who) };
}

let container: HTMLDivElement;
let root: Root;

/**
 * jsdom ships no `matchMedia`, and `useIsMobile` — which `SidebarProvider`
 * calls — reaches for it unguarded. Same stub `sidebar-collapse-button`
 * installs, always reporting "not matching", which is the desktop case.
 */
function stubMatchMedia() {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
      onchange: null,
    }),
  });
}

async function show(client: OpenCompanyClient, company: string | null) {
  await act(async () => {
    root.render(
      createElement(
        SidebarProvider,
        null,
        createElement(ProfileRow, { client, company }),
      ),
    );
  });
}

function text(): string {
  return container.textContent ?? "";
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  stubMatchMedia();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the sidebar identity while the operator changes company", () => {
  it("drops the previous company's identity the moment the scope changes", async () => {
    const { client, reads, releaseBeta } = host();
    await show(client, "alpha");
    expect(text()).toContain("alpha user");
    expect(reads).toEqual(["alpha"]);

    // Move to another company and leave the new fetch in flight.
    await show(client, "beta");

    // The row must not keep showing alpha's identity while beta's fetch is
    // pending — that is the bug: the sidebar, and a save through the dialog,
    // would be operating on the previous company's person.
    expect(text()).not.toContain("alpha user");

    // The beta identity lands when the fetch resolves.
    await act(async () => {
      releaseBeta(meFor("beta"));
    });
    expect(text()).toContain("beta user");
  });

  it("shows no identity for a company with no sign-in", async () => {
    const { client, reads } = host();
    await show(client, "alpha");
    expect(text()).toContain("alpha user");

    // Moving to a company whose fetch rejects (no sign-in) must leave the row
    // empty rather than restoring the previous company's identity.
    await act(async () => {
      root.render(
        createElement(
          SidebarProvider,
          null,
          createElement(ProfileRow, { client, company: "ghost" }),
        ),
      );
    });
    expect(reads).toEqual(["alpha", "ghost"]);
    expect(text()).not.toContain("alpha user");
  });

  it("leaves an untouched display name off an avatar-only save", async () => {
    const { client, patches } = host();
    await show(client, "alpha");

    // Open the account menu, then the profile dialog from it. Both render
    // through a portal, so their controls live under `document.body`, not the
    // mount container.
    const row = container.querySelector('[data-testid="profile-row"]');
    expect(row).toBeTruthy();
    await act(async () => {
      (row as HTMLElement).click();
    });
    const openProfile = document.body.querySelector('[data-testid="profile-open"]');
    expect(openProfile).toBeTruthy();
    await act(async () => {
      (openProfile as HTMLElement).click();
    });

    // Save with no edits at all. The name in the box equals the stored
    // `me.displayName`, so the payload must omit it — a stale echo would
    // revert a display name another client changed while the dialog was open.
    const save = document.body.querySelector('[data-testid="profile-save"]');
    expect(save).toBeTruthy();
    await act(async () => {
      (save as HTMLButtonElement).click();
    });

    expect(patches).toHaveLength(1);
    const body = patches[0].body as Record<string, unknown>;
    expect(body).not.toHaveProperty("displayName");
    expect(body).not.toHaveProperty("avatar");
  });
});
