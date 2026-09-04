// @vitest-environment jsdom

import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

/**
 * Cross-module wiring for the settled-response reconciliation
 * (`PendingSyncPosts.ended`'s own suite proves the reconciliation logic
 * itself; Codex review, PR #2052).
 *
 * The bug this exists to catch cannot be seen from either module alone: it
 * is `ChatView` computing `responseTexts` but never handing them to
 * `onSendEnd`, or `app-shell` receiving them but never forwarding them to
 * `ended()`, or `ended()`'s released frames never reaching `renderAgentReply`.
 * Any one of those silently regresses back to the double-render bug (a
 * `system_notice` fallback shown as both a live system pill and a settled
 * `"company"` bubble) or the original swallowed-note bug (B-101's
 * mention-ambiguity note lost to a blanket discard), with nothing but a
 * live screenshot mid-send to catch it.
 *
 * `AppShell` is too large to mount in a unit test (SSE, the authenticated
 * client, routing — see `chat-receipt-scope-reset.test.ts`'s own doc for the
 * precedent this file follows: read the source, assert the wiring is real
 * rather than merely present somewhere in the file).
 *
 * `onSendEnd`'s call site also pins an ordering fix (Codex review round 2,
 * PR #2052): it fires from the try block, before `append` renders the
 * settled response's own replies — not from `finally`, which used to run it
 * strictly after. B-101's mention-ambiguity note is always journaled on the
 * host *before* the reply it is about; releasing the held note after
 * `append` rendered the reply first put them on screen in the reverse of
 * `chat/history`'s own order, so the note visibly jumped backward past the
 * answer on the very next reload.
 */

const here = dirname(fileURLToPath(import.meta.url));
const chatView = readFileSync(resolve(here, "../../src/views/ChatView.tsx"), "utf8");
const appShell = readFileSync(resolve(here, "../../src/components/app-shell.tsx"), "utf8");

describe("ChatView computes and forwards the settled response's own texts", () => {
  it("captures every response line's text before onSendEnd can see it", () => {
    expect(chatView).toMatch(/responseTexts = reply\.responses\.map\(\(r\) => r\.text\);/);
  });

  it("fires onSendEnd before append renders the response's own replies", () => {
    const marker = "if (stateKey) onSendEnd?.(stateKey, gen, responseTexts);";
    const onSendEndAt = chatView.indexOf(marker);
    expect(onSendEndAt, "the responseTexts-carrying onSendEnd call").toBeGreaterThan(-1);
    const appendAt = chatView.indexOf("append(target, ...replies);", onSendEndAt);
    expect(appendAt, "append(target, ...replies) after that call").toBeGreaterThan(onSendEndAt);
  });

  it("no longer fires onSendEnd a second time for the resolved outcome, from finally", () => {
    // The bug this pins: firing it twice would release (and render) the same
    // held frame twice, on top of getting the order wrong either way.
    const finallyAt = chatView.indexOf("} finally {");
    expect(finallyAt, "the send() finally block").toBeGreaterThan(-1);
    const finallyBody = chatView.slice(finallyAt, chatView.indexOf("\n  }\n", finallyAt));
    expect(finallyBody).not.toMatch(/onSendEnd\?\.\(stateKey, gen, responseTexts\)/);
  });
});

describe("app-shell forwards responseTexts to ended() and renders what it releases", () => {
  function onSendEndBody(): string {
    const start = appShell.indexOf("const onSendEnd = useCallback(");
    expect(start, "onSendEnd's declaration").toBeGreaterThan(-1);
    const end = appShell.indexOf("[clearReceipt, renderAgentReply],", start);
    expect(end, "onSendEnd's dependency array").toBeGreaterThan(start);
    return appShell.slice(start, end);
  }

  it("threads responseTexts into ended(), rather than calling it bare", () => {
    const body = onSendEndBody();
    expect(body).toMatch(/pendingPostThreadsRef\.current\.ended\(threadId, responseTexts\)/);
    // The old bare call this replaced discarded every held frame
    // unconditionally — its absence here is the fix, not merely the new
    // call's presence.
    expect(body).not.toMatch(/pendingPostThreadsRef\.current\.ended\(threadId\);/);
  });

  it("renders every frame ended() releases, rather than dropping them on the floor", () => {
    const body = onSendEndBody();
    expect(body).toMatch(/const released = pendingPostThreadsRef\.current\.ended\(/);
    expect(body).toMatch(/released\.forEach\(\(frame\) => renderAgentReply\(frame\)\);/);
  });
});
