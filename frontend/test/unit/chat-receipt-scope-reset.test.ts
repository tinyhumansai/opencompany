// @vitest-environment jsdom

import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

/**
 * Source-wiring coverage for the receipt/agent-name company-scope reset
 * (issue #1935 review, codex 3892523790).
 *
 * `AppShell` is too large, and pulls in too much (SSE, the authenticated
 * client, routing) to mount in a unit test — `chat-realtime-poll.test.ts`
 * settles the same way for its own wiring, reading the source and asserting
 * on the literal reset calls rather than mounting the component. The
 * behaviour those calls produce (a receipt/name-map object reference change)
 * is exercised directly and exhaustively by `shouldClearReceipt`'s own suite
 * in `chat-live-receipt.test.ts`; what this file locks down is that the reset
 * calls are actually *wired into the scope-change effect*, not merely
 * defined somewhere in the file.
 */

const here = dirname(fileURLToPath(import.meta.url));
const appShell = readFileSync(resolve(here, "../../src/components/app-shell.tsx"), "utf8");

/**
 * The body of the `useEffect` keyed on `[client, company]` that resets every
 * other company-scoped map (`chatChannelByThread`, `transcripts`,
 * `decidedApprovals`, …) on a switch — the same effect `receiptByThread` and
 * `agentNames` belong in. Sliced out so the assertions below cannot pass by
 * matching a `setReceiptByThread`/`setAgentNames` call anywhere else in the
 * file (both names appear elsewhere too: the send-outcome callbacks and the
 * roster fetch's own async reset).
 */
function scopeChangeEffectBody(): string {
  const start = appShell.indexOf("const requestCompany = company;");
  expect(start, "the [client, company] effect's marker line").toBeGreaterThan(-1);
  const end = appShell.indexOf("}, [client, company]);", start);
  expect(end, "the effect's closing dependency array").toBeGreaterThan(start);
  return appShell.slice(start, end);
}

describe("company-switch reset wires receiptByThread and agentNames", () => {
  it("clears receiptByThread synchronously in the scope-change effect", () => {
    const body = scopeChangeEffectBody();
    expect(body).toMatch(/setReceiptByThread\(\(prev\) =>/);
  });

  it("clears agentNames synchronously in the scope-change effect", () => {
    const body = scopeChangeEffectBody();
    expect(body).toMatch(/setAgentNames\(\(prev\) =>/);
  });

  it("does not merely reference the setters without calling them", () => {
    // A regex match against an identifier alone (e.g. a comment mentioning
    // `setAgentNames`) would pass the two tests above without the reset
    // actually being wired. Confirms both are real call expressions inside
    // the slice, immediately preceding the rest-of-map reset idiom
    // (`Object.keys(prev).length === 0 ? prev : {}`) every sibling reset in
    // this same effect already uses.
    const body = scopeChangeEffectBody();
    expect(body).toMatch(
      /setReceiptByThread\(\(prev\) => \(Object\.keys\(prev\)\.length === 0 \? prev : \{\}\)\);/,
    );
    expect(body).toMatch(
      /setAgentNames\(\(prev\) => \(Object\.keys\(prev\)\.length === 0 \? prev : \{\}\)\);/,
    );
  });
});

describe("clearReceipt is generation-guarded (issue #1935 review)", () => {
  it("routes every clear through shouldClearReceipt rather than deleting unconditionally", () => {
    expect(appShell).toContain(
      'import { shouldClearReceipt, type ChatReceipt } from "@/views/chat/ChatLiveReceipt";',
    );
    // The old body deleted whenever `prev[threadId]` was truthy, with no
    // generation check at all — this is the shape that let a stale
    // `onSendStale` from an old company delete a newer company's receipt.
    expect(appShell).not.toMatch(/if \(!prev\[threadId\]\) return prev;\s*\n\s*const next = \{ \.\.\.prev \};\s*\n\s*delete next\[threadId\];/);
    expect(appShell).toMatch(/if \(!shouldClearReceipt\(prev\[threadId\], gen\)\) return prev;/);
  });

  it("every terminal send callback that clears the receipt accepts and forwards a generation", () => {
    // The three callbacks that still clear the receipt keep the #1935 guard:
    // they take the generation their own `onSendStart` returned and hand it to
    // `clearReceipt`, so a stale cross-company clear is a no-op.
    //
    // `onSendEnd` also gained a third, unrelated parameter (issue #101 review,
    // PR #2052) — the settled response's own reply text(s), which `ended`
    // needs to tell a held system frame the response duplicates from one it
    // never will. The regex tolerates it (`(?:, responseTexts\?: readonly
    // string\[\])?`) rather than pinning its exact name: this test's whole
    // job is the generation guard, and a future rename of that third param
    // should not have to touch this file.
    expect(appShell).toMatch(
      /const onSendEnd = useCallback\(\s*\n\s*\(threadId: string, gen\?: number(?:, responseTexts\?: readonly string\[\])?\) =>/,
    );
    expect(appShell).toMatch(/const onSendStale = useCallback\(\s*\n\s*\(threadId: string, gen\?: number\) =>/);
    expect(appShell).toMatch(/const onSendFailed = useCallback\(\s*\n\s*\(threadId: string, gen\?: number\) =>/);
  });

  it("onSendDetached no longer clears the receipt — it rides the turn into the open-turn window (issue #2021)", () => {
    // The 202 handoff used to `clearReceipt` and hand the turn to a bare
    // open-turn row, dropping every #1934 affordance (elapsed, picked-up-by,
    // 30s stall). It now keeps the receipt alive; the poll's terminal settle
    // clears it (see `reReadSettledThread`). So the generation it once forwarded
    // to `clearReceipt` is now unused — the param is present but underscored,
    // and no `clearReceipt` call survives inside the callback body.
    expect(appShell).toMatch(
      /const onSendDetached = useCallback\(\s*\n\s*\(threadId: string, turnId\?: string, _gen\?: number, chatId\?: string\) =>/,
    );
    const start = appShell.indexOf("const onSendDetached = useCallback(");
    expect(start, "onSendDetached must be present").toBeGreaterThan(-1);
    const end = appShell.indexOf("[renderAgentReply],", start);
    expect(end, "onSendDetached's dependency array").toBeGreaterThan(start);
    expect(appShell.slice(start, end)).not.toContain("clearReceipt(");
  });

  it("onSendStart mints and returns a fresh generation per armed receipt", () => {
    expect(appShell).toContain("const receiptGenRef = useRef(0);");
    expect(appShell).toMatch(/const gen = \+\+receiptGenRef\.current;/);
    // Stamped onto the receipt it arms, and handed back to the caller so
    // `ChatView.send` can thread it through whichever terminal outcome fires.
    expect(appShell).toMatch(/\[threadId\]: \{ startedAt: now, lastFrameAt: now, gen \},/);
    expect(appShell).toMatch(/return gen;\s*\n\s*\}, \[\]\);/);
  });
});

/**
 * `ChatView`'s send surface (issue #1935 review, codex 3892702774).
 *
 * `AppShell` owns `receiptByThread` and hands `ChatView` the
 * `onSendStart`/`onSendEnd`/`onSendDetached`/`onSendFailed` callbacks that
 * write it. The send itself lives in `ChatView`'s `send` callback, which —
 * like `AppShell` — sits inside a large host component this suite declines to
 * mount for a wiring assertion.
 *
 * `shouldClearReceipt`'s own suite in `chat-live-receipt.test.ts` proves the
 * *semantics* directly. What this block locks down is that `ChatView` actually
 * *wires* into them: captures `onSendStart`'s return value and forwards it to
 * every terminal call, so a settle arriving after a company switch is refused
 * by generation rather than deleting the new company's receipt.
 */
const chatViewTsx = readFileSync(resolve(here, "../../src/views/ChatView.tsx"), "utf8");

describe("ChatView's send surface generation-tags its receipt clears", () => {
  it("captures onSendStart's return value instead of discarding it", () => {
    // The pre-fix shape was a bare `onSendStart?.(stateKey);` statement — the
    // call happened, but nothing captured what it returned, so every terminal
    // callback below had nothing to forward and fell through to
    // `shouldClearReceipt`'s undefined-generation branch on every send.
    expect(chatViewTsx).not.toMatch(/^\s*onSendStart\?\.\(stateKey\);\s*$/m);
    expect(chatViewTsx).toMatch(
      /const gen = stateKey \? onSendStart\?\.\(stateKey\) : undefined;/,
    );
  });

  it("forwards that generation to all three terminal outcomes", () => {
    // `gen` pinned by position, with the argument list left open: #2044 added
    // a fourth (`chatId`) to `onSendDetached`, and `onSendEnd` gained a third
    // of its own (`responseTexts`, issue #101 review, PR #2052 — see the
    // `appShell` signature check above for why). The generation being second
    // is the whole of what this asserts.
    expect(chatViewTsx).toMatch(/onSendDetached\?\.\(stateKey, answer\.turnId, gen[,)]/);
    expect(chatViewTsx).toMatch(/onSendEnd\?\.\(stateKey, gen(?:, responseTexts)?\);/);
    expect(chatViewTsx).toMatch(/onSendFailed\?\.\(stateKey, gen\);/);
  });

  it("declares onSendStart as returning a generation, not void", () => {
    // A `void`-typed signature would silently defeat the capture above by
    // letting a caller assign `gen` and never mean it — TypeScript would
    // accept `const gen = onSendStart?.(stateKey)` either way, so the capture
    // line alone does not prove the *type* was updated to promise a value
    // exists to capture.
    expect(chatViewTsx).toMatch(/onSendStart\?: \(threadId: string\) => number \| undefined;/);
  });
});
