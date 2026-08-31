// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  addConnection,
  getConnection,
  resetConnections,
  retargetDefaultCompany,
} from "@/connections/registry";
import { findProfile } from "@/connections/profileStore";

/**
 * Codex review on #1828 (PR comment 3862711351): `defaultCompany` is what
 * `ConnectionConsole`'s boot effect reads on every mount to take the
 * "explicit company wins" path straight to `client.status(defaultCompany)`.
 * A reset provisions the replacement into the *current* session directly
 * (`switchCompany`) but never touched this — so a connection opened through
 * `?company=acme` or a single-company profile would, after a reset, boot
 * clean on the current tab but fail on the next reload: it would still ask
 * the host for the just-archived `acme`, get `company_not_found`, and land
 * on a connection error instead of reopening into the replacement.
 *
 * `retargetDefaultCompany` is what `ConnectionConsole`'s `onCompanyCreated`
 * now calls right after a create/reset succeeds. Tested at the registry
 * level, in the style of `connection-registry.test.ts` — the callsite wiring
 * itself is exercised by reading, not a full `AppShell` mount, since reaching
 * the reset control lives four layers into a component tree with its own
 * heavy provider stack (workspace, chat, connection scope).
 */

beforeEach(() => {
  resetConnections();
  window.localStorage.clear();
});

afterEach(() => {
  resetConnections();
  window.localStorage.clear();
});

describe("retargetDefaultCompany", () => {
  it("points an explicit-company connection at the reset's replacement", () => {
    const id = addConnection({ baseUrl: "https://acme.test", defaultCompany: "acme" });

    retargetDefaultCompany(id, "acme-x7f2a91c");

    expect(getConnection(id)?.defaultCompany).toBe("acme-x7f2a91c");
    // Persisted, not just held in the in-memory entry — a reload must see it
    // too, since that is exactly the boot path this guards.
    expect(findProfile("https://acme.test", "acme-x7f2a91c")?.id).toBe(id);
  });

  it("survives a reload — a fresh registry reads the retargeted id back", () => {
    const id = addConnection({ baseUrl: "https://acme.test", defaultCompany: "acme" });
    retargetDefaultCompany(id, "acme-x7f2a91c");

    // The in-memory registry is gone; only `localStorage` persists.
    resetConnections();

    expect(findProfile("https://acme.test", "acme-x7f2a91c")).toBeTruthy();
    expect(findProfile("https://acme.test", "acme")).toBeFalsy();
  });

  it("does nothing to a connection that was never company-scoped", () => {
    const id = addConnection({ baseUrl: "https://acme.test", defaultCompany: null });

    retargetDefaultCompany(id, "whatever");

    // Forcing a multi-company connection onto one company would narrow it to
    // an address it was never opened as.
    expect(getConnection(id)?.defaultCompany).toBeNull();
  });

  it("is a no-op when the connection is already pointed at that company", () => {
    const id = addConnection({ baseUrl: "https://acme.test", defaultCompany: "acme" });

    retargetDefaultCompany(id, "acme");

    expect(getConnection(id)?.defaultCompany).toBe("acme");
  });
});
