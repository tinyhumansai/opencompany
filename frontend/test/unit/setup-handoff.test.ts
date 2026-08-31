// @vitest-environment jsdom

import { describe, expect, it } from "vitest";

import {
  absorbHubSetupHandoff,
  arrivedViaHubSetupHandoff,
  arrivedViaSetupHandoff,
  clearSetupHandoff,
  SETUP_HANDOFF_FRAGMENT,
  setupHandoffHasScope,
} from "@/setup/state";
import type { SetupHandoffScope } from "@/setup/state";

/**
 * The one-shot marker a setup hand-off link carries, and how the landing
 * mount consumes it.
 *
 * The marker is a hash-query flag (`#/company?from=setup`) rather than a state
 * write, because it has to survive a full-page navigation: setup's sign-in
 * button sets `window.location.href`, so component state dies at the boundary.
 * What survives is the URL — and `useHashView`'s segment parsing strips the
 * query from the route, so the flag is visible to the shell without ever
 * reaching the router.
 */

const hash = () => window.location.hash;

/** A landing, as the console's own routing sees it (pathname + search + hash). */
const land = (pathname: string, search: string, hash = "") =>
  window.history.replaceState({}, "", `${pathname}${search}${hash}`);

describe("the setup hand-off marker", () => {
  it("is a fragment whose route the router ignores", () => {
    expect(SETUP_HANDOFF_FRAGMENT).toBe("#/company?from=setup");
    // The view segment is still `company` — the query is the marker, not the
    // route, so `#/company?from=setup` resolves to the same view as `#/company`.
    const [path] = SETUP_HANDOFF_FRAGMENT.replace(/^#/, "").split("?");
    expect(path).toBe("/company");
  });

  it("reads true only when the address carries the marker", () => {
    window.location.hash = "#/company";
    expect(arrivedViaSetupHandoff()).toBe(false);

    window.location.hash = SETUP_HANDOFF_FRAGMENT;
    expect(arrivedViaSetupHandoff()).toBe(true);

    // A different origin value for the same key is not this marker.
    window.location.hash = "#/company?from=elsewhere";
    expect(arrivedViaSetupHandoff()).toBe(false);
  });

  it("consumes the marker without touching the route or other keys", () => {
    window.location.hash = "#/company?from=setup&host=conn-a";
    clearSetupHandoff();
    expect(hash()).toBe("#/company?host=conn-a");
    expect(arrivedViaSetupHandoff()).toBe(false);
  });

  it("leaves an address without the marker alone", () => {
    window.location.hash = "#/overview";
    clearSetupHandoff();
    expect(hash()).toBe("#/overview");
  });

  it("tells a scoped marker from the plain unscoped one", () => {
    window.location.hash = SETUP_HANDOFF_FRAGMENT;
    expect(setupHandoffHasScope()).toBe(false);

    window.location.hash = "#/company?from=setup&connection=conn-a&company=acme";
    expect(setupHandoffHasScope()).toBe(true);
  });

  // The shell's consume decision: a scoped marker is accepted only for the
  // connection and company it names; the plain form is accepted on whatever
  // company it lands on, because the wizard and magic-link flows may not know
  // their scope yet.
  const accept = (scope: SetupHandoffScope): boolean =>
    arrivedViaSetupHandoff(scope) ||
    (!setupHandoffHasScope() && arrivedViaSetupHandoff());

  it("accepts the plain marker on whatever company it lands on", () => {
    window.location.hash = SETUP_HANDOFF_FRAGMENT;
    expect(accept({ connection: "conn-b", company: "beta" })).toBe(true);
  });

  it("accepts a scoped marker only for the connection and company it names", () => {
    window.location.hash = "#/company?from=setup&connection=conn-a&company=acme";
    expect(accept({ connection: "conn-a", company: "acme" })).toBe(true);
    expect(accept({ connection: "conn-b", company: "acme" })).toBe(false);
    expect(accept({ connection: "conn-a", company: "other" })).toBe(false);
  });
});

describe("a hub-carried setup destination", () => {
  it("reads true only when the *query* carries it", () => {
    land("/", "?company=acme");
    expect(arrivedViaHubSetupHandoff()).toBe(false);

    land("/", "?company=acme&from=setup");
    expect(arrivedViaHubSetupHandoff()).toBe(true);

    // A different value for the same key is not this marker.
    land("/", "?company=acme&from=elsewhere");
    expect(arrivedViaHubSetupHandoff()).toBe(false);
  });

  it("translates into the hash marker and takes the flag out of the query", () => {
    land("/", "?company=acme&from=setup");

    absorbHubSetupHandoff();

    // The query flag is gone — the shell's own one-shot marker took its place,
    // and a reload has neither to re-apply.
    expect(window.location.search).toBe("?company=acme");
    expect(hash()).toBe(SETUP_HANDOFF_FRAGMENT);
    expect(arrivedViaHubSetupHandoff()).toBe(false);
    expect(arrivedViaSetupHandoff()).toBe(true);
  });

  it("leaves an address without the marker alone", () => {
    land("/", "?company=acme");

    absorbHubSetupHandoff();

    expect(window.location.search).toBe("?company=acme");
    expect(hash()).toBe("");
  });

  it("recognizes the marker only for its originating connection and company", () => {
    land("/", "?company=acme&from=setup&connection=conn-a");

    expect(arrivedViaHubSetupHandoff({ connection: "conn-a", company: "acme" })).toBe(true);
    expect(arrivedViaHubSetupHandoff({ connection: "conn-b", company: "acme" })).toBe(false);
    expect(arrivedViaHubSetupHandoff({ connection: "conn-a", company: "other" })).toBe(false);
  });
});
