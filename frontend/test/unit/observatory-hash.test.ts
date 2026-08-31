// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from "vitest";

import {
  observatoryHref,
  readObservatoryHash,
  writeObservatoryQuery,
} from "@/views/observatory/hash";

function go(hash: string): void {
  window.history.replaceState(null, "", hash);
}

beforeEach(() => go("#/observatory"));

describe("readObservatoryHash", () => {
  it("reads the bare index", () => {
    expect(readObservatoryHash()).toMatchObject({
      onObservatory: true,
      runId: null,
      tab: "runs",
    });
  });

  it("reads a run and its selection", () => {
    go("#/observatory/wr-1?agent=theorist&turn=att-9&step=7&node=solve");
    expect(readObservatoryHash()).toMatchObject({
      onObservatory: true,
      runId: "wr-1",
      agent: "theorist",
      turn: "att-9",
      step: 7,
      node: "solve",
    });
  });

  it("knows when another view owns the hash", () => {
    go("#/workflows/greet?run=wr-1");
    expect(readObservatoryHash().onObservatory).toBe(false);
  });

  it("decodes a run id that needed escaping", () => {
    go(`#/observatory/${encodeURIComponent("run/with slash")}`);
    expect(readObservatoryHash().runId).toBe("run/with slash");
  });

  it("survives a truncated percent escape", () => {
    // A hand-typed or clipped address. `decodeURIComponent` throws on a lone
    // `%`, and an exception here would blank the view rather than land it on
    // the index.
    go("#/observatory/broken%");
    expect(() => readObservatoryHash()).not.toThrow();
    expect(readObservatoryHash().runId).toBe("broken%");
  });

  it("rejects a step that is not a non-negative integer", () => {
    for (const raw of ["-1", "abc", "1.5", ""]) {
      go(`#/observatory/wr-1?step=${raw}`);
      expect(readObservatoryHash().step).toBeNull();
    }
    go("#/observatory/wr-1?step=0");
    expect(readObservatoryHash().step).toBe(0);
  });

  it("treats an unknown tab as the run list", () => {
    go("#/observatory?tab=nonsense");
    expect(readObservatoryHash().tab).toBe("runs");
    go("#/observatory?tab=analytics");
    expect(readObservatoryHash().tab).toBe("analytics");
  });
});

describe("writeObservatoryQuery", () => {
  it("moves the selection without pushing history", () => {
    // Opening an agent's thread is a selection, not a navigation: pushing each
    // one would make Back walk through every row an operator clicked while
    // reading a single run.
    go("#/observatory/wr-1");
    const before = window.history.length;
    writeObservatoryQuery({ agent: "theorist" });
    expect(window.history.length).toBe(before);
    expect(readObservatoryHash().agent).toBe("theorist");
  });

  it("preserves the host scope", () => {
    // A `replaceState` fires no `hashchange`, so a scope dropped here has
    // nothing to put it back.
    go("#/observatory/wr-1?host=remote-a");
    writeObservatoryQuery({ agent: "scribe" });
    expect(window.location.hash).toContain("host=remote-a");
    expect(readObservatoryHash().agent).toBe("scribe");
  });

  it("clears one key without disturbing another", () => {
    go("#/observatory/wr-1?agent=theorist&turn=att-9");
    writeObservatoryQuery({ turn: null });
    expect(readObservatoryHash().turn).toBeNull();
    expect(readObservatoryHash().agent).toBe("theorist");
  });

  it("keeps the run id in the path", () => {
    go("#/observatory/wr-1");
    writeObservatoryQuery({ agent: "scribe" });
    expect(readObservatoryHash().runId).toBe("wr-1");
  });

  it("is silent when another view owns the hash", () => {
    // A write racing a company switch must not drag the operator back here.
    go("#/workflows/greet");
    writeObservatoryQuery({ agent: "theorist" });
    expect(window.location.hash).toBe("#/workflows/greet");
  });
});

describe("observatoryHref", () => {
  it("addresses the index and a run", () => {
    expect(observatoryHref()).toBe("#/observatory");
    expect(observatoryHref("wr-1")).toBe("#/observatory/wr-1");
  });

  it("carries the host scope so a link stays on its host", () => {
    go("#/workflows?host=remote-a");
    expect(observatoryHref("wr-1")).toContain("host=remote-a");
  });

  it("escapes a run id with a slash", () => {
    expect(observatoryHref("a/b")).toBe("#/observatory/a%2Fb");
  });

  it("omits a default tab and includes a real one", () => {
    expect(observatoryHref(null, { tab: "runs" })).toBe("#/observatory");
    expect(observatoryHref(null, { tab: "analytics" })).toBe("#/observatory?tab=analytics");
  });
});
