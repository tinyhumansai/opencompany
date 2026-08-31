// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";

import type { LocalScope } from "@/connections/types";

const ACME: LocalScope = { connection: "local-a", company: "acme" };
const OTHER_CONNECTION: LocalScope = { connection: "local-b", company: "acme" };
const OTHER_COMPANY: LocalScope = { connection: "local-a", company: "globex" };

/**
 * A fresh module registry is a fresh page load.
 *
 * The since-visit boundary settles once per page load (issue #1700) and the
 * mechanism holding it is module state, so "reload the browser" and "reset the
 * module registry" are the same event. Every test that needs a second visit
 * asks for a second load here rather than reaching into the module.
 */
async function load() {
  vi.resetModules();
  return await import("@/lib/overview-visit");
}

describe("the operator overview visit boundary (#1321)", () => {
  beforeEach(() => window.localStorage.clear());

  it("survives a reload for the same connection and company", async () => {
    const { readOverviewVisit, writeOverviewVisit } = await load();
    writeOverviewVisit(ACME, 1_700_000_000_000);

    expect(readOverviewVisit(ACME)).toBe(1_700_000_000_000);
  });

  it("does not share a browser-local boundary between connections", async () => {
    const { readOverviewVisit, writeOverviewVisit } = await load();
    writeOverviewVisit(ACME, 1_700_000_000_000);

    expect(readOverviewVisit(OTHER_CONNECTION)).toBeNull();
  });

  it("ignores malformed stored values rather than inventing a boundary", async () => {
    const { readOverviewVisit } = await load();
    window.localStorage.setItem("oc.overview.last-visit:local-a::acme", "yesterday");

    expect(readOverviewVisit(ACME)).toBeNull();
  });
});

describe("opening the overview settles the boundary for one page load (#1700)", () => {
  beforeEach(() => window.localStorage.clear());

  it("hands back the previous open, and records this one on commit", async () => {
    const { openOverviewVisit, commitOverviewVisit, readOverviewVisit } = await load();
    window.localStorage.setItem("oc.overview.last-visit:local-a::acme", "1700000000000");

    expect(openOverviewVisit(ACME)).toBe(1_700_000_000_000);
    commitOverviewVisit(ACME, 1_700_000_500_000);
    expect(readOverviewVisit(ACME)).toBe(1_700_000_500_000);
  });

  it("records nothing for a render that never commits", async () => {
    // Review of PR #1752. `openOverviewVisit` used to write straight from the
    // render initializer. A render React starts and throws away — a descendant
    // throwing, the operator reloading out of the error boundary — therefore
    // left a durable timestamp for an Overview nobody ever saw, and the next
    // page load hid every failure recorded before it.
    const abandoned = await load();
    window.localStorage.setItem("oc.overview.last-visit:local-a::acme", "1700000000000");

    abandoned.openOverviewVisit(ACME);
    expect(abandoned.readOverviewVisit(ACME)).toBe(1_700_000_000_000);

    // The operator reloads. The real last visit is still the boundary.
    const reloaded = await load();
    expect(reloaded.openOverviewVisit(ACME)).toBe(1_700_000_000_000);
  });

  it("does not push the recorded visit forward when the view remounts", async () => {
    // The commit half needs the same once-per-load rule as the read half: a
    // trip to Chat and back is a second mount, and a second write would leave
    // the NEXT page load comparing against a moment ago.
    const { openOverviewVisit, commitOverviewVisit, readOverviewVisit } = await load();
    window.localStorage.setItem("oc.overview.last-visit:local-a::acme", "1700000000000");

    openOverviewVisit(ACME);
    commitOverviewVisit(ACME, 1_700_000_500_000);
    commitOverviewVisit(ACME, 1_700_000_900_000);

    expect(readOverviewVisit(ACME)).toBe(1_700_000_500_000);
  });

  it("keeps answering with the same boundary for the rest of the load", async () => {
    // This is the whole of #1700. Every navigation back to the overview opens
    // it again; when each open advanced the boundary, the panel compared
    // against a moment ago and reported that nothing had failed since.
    const { openOverviewVisit, commitOverviewVisit, readOverviewVisit } = await load();
    window.localStorage.setItem("oc.overview.last-visit:local-a::acme", "1700000000000");

    openOverviewVisit(ACME);
    commitOverviewVisit(ACME, 1_700_000_500_000);

    expect(openOverviewVisit(ACME)).toBe(1_700_000_000_000);
    expect(openOverviewVisit(ACME)).toBe(1_700_000_000_000);
    // …and the recorded visit stays where the first commit put it, so the
    // *next* page load compares against when this one began.
    expect(readOverviewVisit(ACME)).toBe(1_700_000_500_000);
  });

  it("advances on the next page load, which is the event the heading names", async () => {
    const first = await load();
    window.localStorage.setItem("oc.overview.last-visit:local-a::acme", "1700000000000");
    first.openOverviewVisit(ACME);
    first.commitOverviewVisit(ACME, 1_700_000_500_000);

    const second = await load();

    expect(second.openOverviewVisit(ACME)).toBe(1_700_000_500_000);
    second.commitOverviewVisit(ACME, 1_700_000_900_000);
    expect(second.readOverviewVisit(ACME)).toBe(1_700_000_900_000);
  });

  it("settles each scope separately, so a company switch gets its own boundary", async () => {
    const { openOverviewVisit, commitOverviewVisit, readOverviewVisit } = await load();
    window.localStorage.setItem("oc.overview.last-visit:local-a::acme", "1700000000000");
    window.localStorage.setItem("oc.overview.last-visit:local-a::globex", "1600000000000");

    expect(openOverviewVisit(ACME)).toBe(1_700_000_000_000);
    commitOverviewVisit(ACME, 1_700_000_500_000);
    expect(openOverviewVisit(OTHER_COMPANY)).toBe(1_600_000_000_000);
    commitOverviewVisit(OTHER_COMPANY, 1_700_000_500_000);
    // Switching back does not re-open the first: within one load each scope is
    // opened exactly once.
    expect(openOverviewVisit(ACME)).toBe(1_700_000_000_000);
    expect(readOverviewVisit(ACME)).toBe(1_700_000_500_000);
  });

  it("reports no earlier visit the first time a browser opens a company", async () => {
    const { openOverviewVisit, commitOverviewVisit, readOverviewVisit } = await load();

    expect(openOverviewVisit(ACME)).toBeNull();
    commitOverviewVisit(ACME, 1_700_000_500_000);
    // A settled `null` must still count as settled rather than as never opened.
    expect(openOverviewVisit(ACME)).toBeNull();
    expect(readOverviewVisit(ACME)).toBe(1_700_000_500_000);
  });
});
