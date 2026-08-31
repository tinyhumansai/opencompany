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

  it("every terminal send callback accepts and forwards a generation", () => {
    expect(appShell).toMatch(/const onSendEnd = useCallback\(\s*\n\s*\(threadId: string, gen\?: number\) =>/);
    expect(appShell).toMatch(/const onSendStale = useCallback\(\s*\n\s*\(threadId: string, gen\?: number\) =>/);
    expect(appShell).toMatch(
      /const onSendDetached = useCallback\(\s*\n\s*\(threadId: string, turnId\?: string, gen\?: number\) =>/,
    );
    expect(appShell).toMatch(/const onSendFailed = useCallback\(\s*\n\s*\(threadId: string, gen\?: number\) =>/);
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
 * `Conversation`'s own send surface (issue #1935 review, codex 3892702774).
 *
 * `#/conversation` is a second, still-routable surface — `ROUTABLE.
 * conversation` in `console-routes.ts` — that shares `AppShell`'s
 * `onSendStart`/`onSendEnd`/`onSendDetached`/`onSendFailed` and therefore the
 * same `receiptByThread` map `ChatView` writes into. Its send lives in
 * `useConversationRuntime` (`views/conversation/runtime.ts`), which — like
 * `AppShell` — is a hook wired into a large host component (`ChatPane`,
 * itself inside `Conversation`), not something worth mounting whole for this.
 *
 * `shouldClearReceipt`'s own suite in `chat-live-receipt.test.ts` proves the
 * *semantics* directly, including a case modelled explicitly on this
 * Conversation → ChatView cross-surface race. What this block locks down is
 * that `runtime.ts` actually *wires* into those semantics — captures
 * `onSendStart`'s return value and forwards it to every terminal call —
 * rather than the fix living only on the `ChatView` side of the same map.
 */
const runtimeTs = readFileSync(
  resolve(here, "../../src/views/conversation/runtime.ts"),
  "utf8",
);

describe("Conversation's send surface generation-tags its receipt clears too", () => {
  it("captures onSendStart's return value instead of discarding it", () => {
    // The pre-fix shape was a bare `onSendStart?.(thread.id);` statement —
    // the call happened, but nothing captured what it returned, so every
    // terminal callback below had nothing to forward and fell through to
    // `shouldClearReceipt`'s undefined-generation branch on every single
    // send from this surface.
    expect(runtimeTs).not.toMatch(/^\s*onSendStart\?\.\(thread\.id\);\s*$/m);
    expect(runtimeTs).toMatch(/const gen = onSendStart\?\.\(thread\.id\);/);
  });

  it("forwards that generation to all three terminal outcomes", () => {
    expect(runtimeTs).toMatch(/onSendDetached\?\.\(thread\.id, answer\.turnId, gen\);/);
    expect(runtimeTs).toMatch(/onSendEnd\?\.\(thread\.id, gen\);/);
    expect(runtimeTs).toMatch(/onSendFailed\?\.\(thread\.id, gen\);/);
  });

  it("declares onSendStart as returning a generation, not void", () => {
    // A `void`-typed signature would silently defeat the capture above by
    // letting a caller assign `gen` and never mean it — TypeScript would
    // accept `const gen = onSendStart?.(thread.id)` either way, so the
    // capture line alone does not prove the *type* was updated to promise a
    // value exists to capture.
    expect(runtimeTs).toMatch(
      /onSendStart\?: \(threadId: string\) => number \| undefined;/,
    );
  });
});
