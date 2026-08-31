// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { resolveConfig } from "@/config";
import {
  addConnection,
  clearDefaultCompany,
  getConnection,
  resetConnections,
  restoreConnections,
  retargetCompanyUrlParam,
  retargetDefaultCompany,
} from "@/connections/registry";

/**
 * Codex review on #1828 (PR comment 3863028385): `retargetDefaultCompany`
 * fixes the persisted profile a reset leaves behind, but a connection opened
 * from a `?company=<id>` link is re-minted from that URL on every reload —
 * `resolveConfig()` reads it fresh from `window.location.search`, and a
 * reset never touches the address bar. Left stale, the next reload's
 * bootstrap `addConnection` call looks up `findProfile(baseUrl, archivedId)`,
 * which no longer matches the retargeted profile (its `defaultCompany` is now
 * the replacement's id), mints a fresh duplicate connection still scoped to
 * the archived id, and that connection's boot effect asks the host for an id
 * that no longer exists — a connection error instead of the replacement.
 *
 * `retargetCompanyUrlParam` is what `ConnectionConsole`'s `onCompanyCreated`
 * now calls beside `retargetDefaultCompany` on every reset. These tests drive
 * the real bootstrap sequence App.tsx runs on load (`resolveConfig` +
 * `restoreConnections` + the bootstrap `addConnection` call) against a real
 * `window.location`, in the style of `magic-link-scope.test.ts`.
 */

function land(search: string): void {
  window.history.replaceState({}, "", `/${search}`);
}

beforeEach(() => {
  resetConnections();
  window.localStorage.clear();
  land("");
});

afterEach(() => {
  resetConnections();
  window.localStorage.clear();
  land("");
});

describe("retargetCompanyUrlParam", () => {
  it("keeps a ?company= bootstrap reusing the same connection across a reload after reset", () => {
    land("?company=acme");
    const bootConfig = resolveConfig();
    expect(bootConfig.company).toBe("acme");
    const id = addConnection({ baseUrl: "https://acme.test", defaultCompany: bootConfig.company });

    // The reset: registry and URL both retargeted, as ConnectionConsole's
    // onCompanyCreated now does.
    retargetDefaultCompany(id, "acme-x7f2a91c");
    retargetCompanyUrlParam("acme", "acme-x7f2a91c");

    expect(window.location.search).toBe("?company=acme-x7f2a91c");

    // A reload: the in-memory registry is gone; the URL and localStorage
    // persist. Replay exactly what App.tsx's bootstrap does.
    resetConnections();
    const reloadedConfig = resolveConfig();
    expect(reloadedConfig.company).toBe("acme-x7f2a91c");

    restoreConnections();
    const reloadedId = addConnection({
      baseUrl: "https://acme.test",
      defaultCompany: reloadedConfig.company,
    });

    // The same connection is reused — no orphaned duplicate left pointed at
    // the archived company.
    expect(reloadedId).toBe(id);
    expect(getConnection(reloadedId)?.defaultCompany).toBe("acme-x7f2a91c");
    expect(getConnection(reloadedId)?.status).not.toBe("unauthenticated");
  });

  it("without it, a reload mints a duplicate connection still scoped to the archived id", () => {
    land("?company=acme");
    const bootConfig = resolveConfig();
    const id = addConnection({ baseUrl: "https://acme.test", defaultCompany: bootConfig.company });

    // The registry is retargeted, but the URL param is deliberately left
    // alone — this is the state the previous fix (retargetDefaultCompany
    // alone) left a reload in.
    retargetDefaultCompany(id, "acme-x7f2a91c");

    resetConnections();
    const reloadedConfig = resolveConfig();
    expect(reloadedConfig.company).toBe("acme"); // still the archived id

    restoreConnections();
    const reloadedId = addConnection({
      baseUrl: "https://acme.test",
      defaultCompany: reloadedConfig.company,
    });

    // A different connection, still scoped to the id the reset archived —
    // the exact orphaning `retargetCompanyUrlParam` exists to prevent.
    expect(reloadedId).not.toBe(id);
    expect(getConnection(reloadedId)?.defaultCompany).toBe("acme");
  });

  it("is a no-op when the URL never named the archived id", () => {
    land("?company=acme");

    retargetCompanyUrlParam("someone-else", "acme-x7f2a91c");

    expect(window.location.search).toBe("?company=acme");
  });

  it("preserves the hash and other query params", () => {
    land("?api=https%3A%2F%2Facme.test&company=acme&hub=1#/overview");

    retargetCompanyUrlParam("acme", "acme-x7f2a91c");

    expect(window.location.search).toContain("company=acme-x7f2a91c");
    expect(window.location.search).toContain("hub=1");
    expect(window.location.hash).toBe("#/overview");
  });

  /**
   * Codex review on #1828 (PR comment 3864885209): a connection's explicit
   * company can come from `window.OPENCOMPANY_CONFIG` or `VITE_OC_COMPANY`,
   * not just a `?company=` link — `resolveConfig()` merges all three, with
   * the query layer outranking both. The original guard
   * (`url.searchParams.get("company") !== archivedId`) bailed whenever the
   * URL didn't already name `archivedId`, which is exactly what happens when
   * the id came from one of the other two sources: the URL never carried it
   * in the first place. `window.OPENCOMPANY_CONFIG` stands in for both here
   * — the fix never inspects which lower layer supplied the id, only
   * whether the URL currently conflicts with it, so this exercises the same
   * branch `VITE_OC_COMPANY` would.
   */
  it("writes an override even when the URL never carried a ?company= param — a window.OPENCOMPANY_CONFIG-sourced explicit company", () => {
    land("");
    window.OPENCOMPANY_CONFIG = { company: "acme" };
    const bootConfig = resolveConfig();
    expect(bootConfig.company).toBe("acme");
    expect(window.location.search).toBe("");

    retargetCompanyUrlParam("acme", "acme-x7f2a91c");

    // The bug this guards: the old code saw no `?company=` param to correct
    // and did nothing, so a reload's `resolveConfig()` would still resolve
    // "acme" from `window.OPENCOMPANY_CONFIG` — this override is the only
    // way to outrank that on the next load.
    expect(window.location.search).toBe("?company=acme-x7f2a91c");
    const reloadedConfig = resolveConfig();
    expect(reloadedConfig.company).toBe("acme-x7f2a91c");

    delete window.OPENCOMPANY_CONFIG;
  });

  /**
   * Codex review on #1828 (PR comment 3864885215): cancelling a reset after
   * the archive already landed has no replacement id to retarget to. Passing
   * `null` must still neutralize a config/env-sourced explicit company on
   * the next reload, not merely no-op — an empty `?company=` still outranks
   * `window.OPENCOMPANY_CONFIG`/`VITE_OC_COMPANY` in `resolveConfig`'s
   * merge, and resolves to `""`, which the boot effect's `if (defaultCompany)`
   * treats as "no explicit company" the same as `null`.
   */
  it("clears to an empty override when newId is null, neutralizing a config-sourced company on reload", () => {
    land("");
    window.OPENCOMPANY_CONFIG = { company: "acme" };
    expect(resolveConfig().company).toBe("acme");

    retargetCompanyUrlParam("acme", null);

    expect(window.location.search).toBe("?company=");
    const reloadedConfig = resolveConfig();
    // Falsy — not "acme" — is what matters: the boot effect's
    // `if (defaultCompany)` check treats both `null` and `""` the same way.
    expect(reloadedConfig.company).toBeFalsy();

    delete window.OPENCOMPANY_CONFIG;
  });

  it("still no-ops when the URL already names some other company", () => {
    land("?company=beta");

    retargetCompanyUrlParam("acme", null);

    expect(window.location.search).toBe("?company=beta");
  });

  /**
   * Codex review on #1828 (PR comment 3865190492): the previous test proves
   * `resolveConfig().company` is falsy after a clear, but the bootstrap in
   * `App.tsx` never compares that value for truthiness — it feeds it
   * straight into `addConnection`'s `findProfile(baseUrl, defaultCompany)`
   * lookup, which matches by STRICT equality against the persisted profile's
   * `defaultCompany`. `clearDefaultCompany` sets that to `null`; an
   * unnormalized empty `?company=` resolves to `""`, not `null`, and
   * `"" !== null` — so the reload that was supposed to land back on the same
   * cleared connection instead mints a fresh, orphaned duplicate still
   * carrying the old browser-scoped state on the original.
   */
  it("keeps a cleared connection's profile matching itself across a reload, not just its config falsy", () => {
    land("?company=acme");
    const bootConfig = resolveConfig();
    const id = addConnection({ baseUrl: "https://acme.test", defaultCompany: bootConfig.company });

    // The abandon path: archive landed, nothing replaced it — the same two
    // calls `ConnectionConsole`'s `onClose` handler makes today.
    clearDefaultCompany(id);
    retargetCompanyUrlParam("acme", null);

    expect(getConnection(id)?.defaultCompany).toBeNull();
    expect(window.location.search).toBe("?company=");

    // A reload: registry gone, URL + localStorage persist. Replay App.tsx's
    // bootstrap exactly.
    resetConnections();
    const reloadedConfig = resolveConfig();

    restoreConnections();
    const reloadedId = addConnection({
      baseUrl: "https://acme.test",
      defaultCompany: reloadedConfig.company,
    });

    // The same connection is reused, not an orphaned duplicate scoped to "".
    expect(reloadedId).toBe(id);
    expect(getConnection(reloadedId)?.defaultCompany).toBeNull();
  });
});
