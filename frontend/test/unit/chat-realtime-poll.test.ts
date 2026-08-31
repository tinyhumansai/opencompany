// @vitest-environment jsdom

import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import type { ChatHistoryMessageDto } from "@/api/types";
import { fromHistory, mergeHistoryInOrder, type ChatMessage } from "@/lib/chat";

const here = dirname(fileURLToPath(import.meta.url));
const appShell = readFileSync(resolve(here, "../../src/components/app-shell.tsx"), "utf8");
const chatView = readFileSync(resolve(here, "../../src/views/ChatView.tsx"), "utf8");

describe("chat channel history polling", () => {
  it("wires each resolved channel fan-out to a disposable 5s visible-tab poll", () => {
    expect(appShell.match(/startVisiblePolling\(rehydrateAll, 5000\)/g)).toHaveLength(2);
    expect(appShell).toContain("disposeRehydratePolling?.();");
  });

  /**
   * Issue #1781 review (Codex P2): `ChatView` fetches the Operator channel's
   * identity independently of this hydration pass, for rendering its pinned
   * row. A single dropped request here — while `ChatView`'s own, later call
   * succeeds — used to render the row but permanently omit its id from the
   * rehydration targets and this 5s poll, since this pass had already given
   * up on it. `fetchWithOneRetry` closes the common transient case.
   */
  it("retries the Operator channel fetch instead of giving up on the first miss", () => {
    expect(appShell).toContain(
      "fetchWithOneRetry(() => client.getOperatorChannel(company))",
    );
    expect(appShell).not.toContain("client.getOperatorChannel(company).catch(() => null)");
  });

  /**
   * PR #1781 review (Codex P2, comment 3878524727): `ChatView`'s own
   * render-side lookup of the same identity had the twin gap — a single
   * dropped request there, while `app-shell.tsx`'s independent, now-retried
   * lookup succeeded, left history hydrating with `operator` stuck `null`
   * until the client/company changed or the page reloaded. Same wrapper,
   * same fix, source-wiring-pinned the same way the shell's call site above
   * is (no render harness for either component in this repo).
   */
  it("ChatView also retries the Operator channel fetch instead of giving up on the first miss", () => {
    expect(chatView).toContain("fetchWithOneRetry(() => client.getOperatorChannel(company))");
    expect(chatView).not.toContain("client.getOperatorChannel(company)\n      .then(");
  });

  /**
   * PR #1781 review (Codex, comment 3878749061): the `.then` callback checked
   * `isOperatorChannelDto(dto)` but had no `else` — a 2xx response that is not
   * `OperatorChannelDto`-shaped (schema drift, not a fetch failure) was
   * silently indistinguishable from the ordinary offline/older-host miss.
   * Source-wiring-pinned the same way the retry above is (no render harness
   * for this component in this repo): the mismatched-shape arm must be
   * logged, and only the `dto !== null` (i.e. not the already-collapsed
   * fetch-failure) case must log.
   */
  it("ChatView logs a mismatched Operator channel shape instead of dropping it silently", () => {
    expect(chatView).toContain(
      'console.debug("[ChatView] getOperatorChannel returned an unexpected shape", dto)',
    );
    expect(chatView).toContain("} else if (dto !== null) {");
  });

  // The polling merge is `mergeHistoryInOrder` — the same reconstruction rule
  // the cold mount and every 5s tick share, which is what the source-wiring
  // test above arms but cannot itself observe. These exercise the real
  // function against the endpoint's oldest-first contract (issue #1690).

  const dto = (id: string, text: string, mine = false): ChatHistoryMessageDto => ({
    id,
    channel: "engineering",
    author: mine ? "operator" : "workflow",
    text,
    atMillis: 1_700_000_000_000 + Number(id),
    mine,
  });

  it("folds a first history read in and never duplicates it on a later tick", () => {
    const hydrated = fromHistory([dto("1686", "The workflow finished.")]);
    const first = mergeHistoryInOrder([], hydrated);
    expect(first.map((message) => message.text)).toEqual(["The workflow finished."]);

    // Same durable row on the next tick is id-seen: the identical array
    // reference comes back, so a caller can skip the state write (and React
    // the re-render).
    expect(mergeHistoryInOrder(first, hydrated)).toBe(first);
    expect(first).toHaveLength(1);
  });

  it("places a message recovered by a later tick after the transcript it follows", () => {
    const earlier = fromHistory([dto("1", "Kicking off the deploy.", true)])[0];
    // Oldest-first, matching the real endpoint's ordering.
    const recovered = fromHistory([dto("1", "Kicking off the deploy.", true), dto("2", "Deploy finished.")]);

    const merged = mergeHistoryInOrder([earlier], recovered);
    expect(merged.map((message) => message.text)).toEqual([
      "Kicking off the deploy.",
      "Deploy finished.",
    ]);
    // The transcript's own copy of the already-seen row survives the re-fetch
    // (reactions and other local decoration are kept), only the new row is new.
    expect(merged[0]).toBe(earlier);
  });

  it("fills a gap the live path left at the host's own position, not the tail", () => {
    // The SSE frame for 2 was missed while 1 and 3 landed: a plain append or
    // prepend rule would merge to `[1, 3, 2]` or `[2, 1, 3]`. The persisted
    // rows must take the history's order — `[1, 2, 3]`.
    const one = fromHistory([dto("1", "First", true)])[0];
    const three = fromHistory([dto("3", "Third")])[0];
    const hydrated = fromHistory([dto("1", "First", true), dto("2", "Second"), dto("3", "Third")]);

    const merged = mergeHistoryInOrder([one, three], hydrated);
    expect(merged.map((message) => message.text)).toEqual(["First", "Second", "Third"]);
  });

  it("keeps durable rows evicted from the newest history page in order", () => {
    const old = fromHistory([dto("1", "Oldest")])[0];
    const current = fromHistory([dto("2", "Current"), dto("3", "Newest")]);

    // The endpoint's default page contains only the newest 200 rows. The
    // durable row missing from that page is not an optimistic send and must
    // remain before the returned page on every poll.
    const merged = mergeHistoryInOrder([old, current[0]], current);
    expect(merged.map((message) => message.text)).toEqual(["Oldest", "Current", "Newest"]);
    expect(merged[0]).toBe(old);
  });

  it("does not retain an optimistic send after its persisted echo appears", () => {
    const optimistic: ChatMessage = {
      id: "m42",
      from: "you",
      text: "slow synchronous send",
      at: 1_700_000_010_000,
    };
    const persisted = fromHistory([dto("42", optimistic.text, true)])[0];

    // The POST is still awaiting its response, but the host has already
    // journaled the operator row. Match the echo by its stable fields and
    // replace the local row with the durable projection rather than showing
    // both copies.
    const merged = mergeHistoryInOrder([optimistic], [persisted]);
    expect(merged).toEqual([persisted]);
    expect(merged).not.toContain(optimistic);
  });

  it("does not consume an older identical operator row as a new send echo", () => {
    const old = fromHistory([dto("1", "repeat", true)])[0];
    const boundary = fromHistory([dto("2", "newer", true)])[0];
    const optimistic: ChatMessage = {
      id: "m42",
      from: "you",
      text: "repeat",
      parentId: old.parentId,
      at: old.at + 10_000,
    };

    // The page predates the new send. The old durable row must remain and the
    // new local bubble must stay visible until its own echo arrives.
    const merged = mergeHistoryInOrder(
      [old, optimistic],
      [old, boundary],
    );
    expect(merged).toEqual([old, boundary, optimistic]);
  });

  it("keeps a newest identical operator send after a one-row snapshot", () => {
    const old = fromHistory([dto("1", "repeat", true)])[0];
    const optimistic: ChatMessage = {
      id: "m42",
      from: "you",
      text: "repeat",
      parentId: old.parentId,
      at: old.at + 10_000,
    };

    // The response was captured before the second send. Even though the old
    // row is the page's only (and newest) item, it is not the new send's echo.
    expect(mergeHistoryInOrder([old, optimistic], [old])).toEqual([old, optimistic]);
  });

  it("keeps optimistic rows before a durable live tail", () => {
    const durable = fromHistory([dto("2", "durable")])[0];
    const optimistic: ChatMessage = {
      id: "m42",
      from: "you",
      text: "optimistic",
      at: durable.at + 1,
    };
    const hydrated = fromHistory([dto("1", "before"), dto("3", "after")]);

    // The SSE durable row arrived after the send, but before the snapshot was
    // applied. Preserve the live order [optimistic, durable] while placing the
    // durable row at its sequence position within history.
    expect(mergeHistoryInOrder([optimistic, durable], hydrated).map((m) => m.text)).toEqual([
      "before",
      "optimistic",
      "durable",
      "after",
    ]);
  });

  it("reconciles a legacy company reply without a message id", () => {
    const optimistic: ChatMessage = {
      id: "m42",
      from: "company",
      text: "legacy reply",
      at: 1_700_000_000_002,
      parentId: "h10",
      channel: "engineer",
    };
    const persisted = fromHistory([dto("11", optimistic.text)]);
    persisted[0].parentId = optimistic.parentId;
    persisted[0].channel = optimistic.channel;

    expect(mergeHistoryInOrder([optimistic], persisted)).toEqual(persisted);
  });

  it("uses durable sequence bounds when timestamps are tied", () => {
    const first = fromHistory([dto("10", "first")])[0];
    const last = fromHistory([dto("12", "last")])[0];
    const live = fromHistory([dto("11", "live")])[0];
    const hydrated = fromHistory([dto("10", "first"), dto("12", "last")]);

    // Sequence 11 arrived after the snapshot but shares its millisecond with
    // the page's newest row. It is still a post-snapshot durable tail row.
    live.at = last.at;
    expect(mergeHistoryInOrder([first, live], hydrated).map((m) => m.text)).toEqual([
      "first",
      "live",
      "last",
    ]);
  });

  it("preserves relative order between live durable and optimistic rows", () => {
    const durable = fromHistory([dto("2", "durable")])[0];
    const optimistic: ChatMessage = {
      id: "m42",
      from: "you",
      text: "optimistic",
      at: durable.at + 1,
    };
    const hydrated = fromHistory([dto("1", "before")]);

    // Both rows were created after the snapshot; their live order is durable,
    // then optimistic, and folding must not reorder them by category.
    expect(mergeHistoryInOrder([durable, optimistic], hydrated).map((m) => m.text)).toEqual([
      "before",
      "durable",
      "optimistic",
    ]);
  });

  it("keeps rows the host has not persisted yet at the tail", () => {
    // The operator's optimistic bubble is minted with a browser-local `m<seq>`
    // id the host does not know, so history does not name it. It must survive
    // the fold as the newest line, after every persisted row.
    const persisted = fromHistory([dto("1", "Kicking off the deploy.", true)])[0];
    const optimistic: ChatMessage = {
      id: "m42",
      from: "you",
      text: "unacked local bubble",
      at: 1_700_000_010_000,
    };

    const merged = mergeHistoryInOrder([persisted, optimistic], fromHistory([dto("1", "Kicking off the deploy.", true)]));
    expect(merged).toEqual([persisted, optimistic]);
    expect(merged[1]).toBe(optimistic);
  });
});
