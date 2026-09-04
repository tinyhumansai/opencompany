// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Me } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { ProfileRow } from "@/components/profile-row";
import { SidebarProvider } from "@/components/ui/sidebar";

/**
 * Ending your own session from the console.
 *
 * The failure this pins is not the happy path — it is the console deciding on
 * its own that the session is over. A host that refuses the revocation, or one
 * that cannot be reached at all, must leave the operator signed in and say so;
 * a login screen over a session that is still live is worse than a sign-out
 * that visibly failed, because the operator walks away from a shared machine
 * believing the opposite of the truth.
 */

const ME: Me = {
  id: "me",
  email: "me@example.test",
  displayName: "Me",
  role: "member",
  company: "alpha",
  hasPassword: false,
  mustChangePassword: false,
};

/** A client whose `/auth/logout` resolves or rejects on demand. */
function host(logout: () => Promise<unknown>) {
  const calls: string[] = [];
  const client = {
    scopeFor: () => "/api/v1/companies/alpha",
    get: async (path: string) => {
      calls.push(`GET ${path}`);
      if (path.endsWith("/auth/me")) return ME;
      throw new Error(`unexpected GET ${path}`);
    },
    post: async (path: string) => {
      calls.push(`POST ${path}`);
      if (path.endsWith("/auth/logout")) return logout();
      throw new Error(`unexpected POST ${path}`);
    },
  } as unknown as OpenCompanyClient;
  return { client, calls };
}

/**
 * jsdom ships no `matchMedia`, and `useIsMobile` — which `SidebarProvider`
 * calls — reaches for it unguarded. Always reports "not matching", the
 * desktop case.
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

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  stubMatchMedia();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  document.body.innerHTML = "";
});

async function show(client: OpenCompanyClient, onSignedOut?: () => void) {
  await act(async () => {
    root.render(
      createElement(
        SidebarProvider,
        null,
        createElement(ProfileRow, { client, company: "alpha", onSignedOut }),
      ),
    );
  });
}

/** Open the account menu. */
async function openMenu() {
  const row = container.querySelector('[data-testid="profile-row"]');
  if (!row) throw new Error(`no profile row in:\n${container.innerHTML}`);
  await act(async () => {
    (row as HTMLElement).click();
  });
}

function signOutItem(): HTMLElement | null {
  return document.body.querySelector('[data-testid="profile-sign-out"]');
}

describe("signing yourself out", () => {
  it("offers sign-out from the account menu", async () => {
    const { client } = host(async () => undefined);
    await show(client, () => {});
    await openMenu();

    expect(signOutItem()).toBeTruthy();
  });

  it("tells the connection only after the host has revoked the session", async () => {
    const { client, calls } = host(async () => undefined);
    const signedOut = vi.fn();
    await show(client, signedOut);
    await openMenu();

    await act(async () => {
      signOutItem()!.click();
    });

    expect(calls).toContain("POST /api/v1/companies/alpha/auth/logout");
    expect(signedOut).toHaveBeenCalledTimes(1);
  });

  it("keeps the operator signed in when the host refuses the revocation", async () => {
    const { client } = host(async () => {
      throw new Error("logout is not available on this host");
    });
    const signedOut = vi.fn();
    await show(client, signedOut);
    await openMenu();

    await act(async () => {
      signOutItem()!.click();
    });

    // The connection is never told. Reporting a sign-out the host refused
    // would drop the console to a login screen while the session still works.
    expect(signedOut).not.toHaveBeenCalled();
  });

  it("keeps the operator signed in when the host cannot be reached", async () => {
    const { client } = host(async () => {
      throw new TypeError("Failed to fetch");
    });
    const signedOut = vi.fn();
    await show(client, signedOut);
    await openMenu();

    await act(async () => {
      signOutItem()!.click();
    });

    expect(signedOut).not.toHaveBeenCalled();
  });

  it("offers no sign-out where nothing owns the connection's state", async () => {
    const { client } = host(async () => undefined);
    await show(client, undefined);
    await openMenu();

    // A control that ends nowhere is worse than no control: it would revoke
    // the session server-side and leave the console showing a live one.
    expect(signOutItem()).toBeNull();
  });
});
