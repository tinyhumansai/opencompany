import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

/**
 * A top-level send's live-turn state must key on the **channel**, never on
 * whatever thread the panel happens to have open beside it (codex P2, PR
 * #2052 fresh review round).
 *
 * `send`'s `stateKey` used to be `turnStateKey(chatId,
 * threadRootOf(openThreadId ?? undefined))` unconditionally — derived from
 * the currently-open thread panel, not from the send's own `parentId`. Two
 * ingresses can carry `parentId: undefined` (a genuine top-level send) while
 * `openThreadId` still names some *other* thread in the same channel:
 *
 * - the channel composer itself, which always sends `parentId: undefined`
 *   regardless of whether a thread panel happens to be open beside it
 *   (`onSend={(text, intent, attachments, mentions) => send(text, intent,
 *   undefined, attachments, mentions)}`);
 * - a Retry click on a top-level failed send's row, whose saved payload
 *   (`failedSends`) never carried a `parentId` either, and which can be
 *   clicked from the channel timeline while any other thread in the channel
 *   is open in the panel.
 *
 * Either one, before this fix, armed the send's receipt / open-turn recovery
 * under `chatId#<the-open-thread's-root>` instead of the bare `chatId`. The
 * thread panel for the unrelated thread then showed work it never started
 * (its own read, `threadTurnKey`, keys on exactly that same
 * `turnStateKey(activeThreadId, threadRootOf(openThreadId))`), while the
 * channel's own row excludes anything matching `threadTurnKey` (`key !==
 * threadTurnKey`) and so showed nothing for a turn that was, in fact, its
 * own.
 *
 * Asserted against the source, matching this file's established style for
 * `ChatView.send` (see `chat-send-state-key-symmetry.test.ts`,
 * `chat-receipt-scope-reset.test.ts`): `send` sits inside a large host
 * component this suite declines to mount for a wiring/branching assertion,
 * and the failure mode here is exactly "the key formula used the wrong
 * input", which a source read pins precisely.
 */
const here = dirname(fileURLToPath(import.meta.url));
const chatView = readFileSync(resolve(here, "../../src/views/ChatView.tsx"), "utf8");

describe("send's stateKey does not borrow the open thread for a top-level send", () => {
  it("keys an unthreaded send on the bare channel, not on openThreadId", () => {
    expect(chatView).toMatch(
      /const stateKey = chatId\s*\n\s*\? parentId === undefined\s*\n\s*\? turnStateKey\(chatId\)\s*\n\s*: turnStateKey\(chatId, threadRootOf\(openThreadId \?\? undefined\)\)\s*\n\s*: undefined;/,
    );
  });

  it("no longer derives every send's key from openThreadId unconditionally", () => {
    // The pre-fix shape: one branch, always consulting `openThreadId`, with no
    // read of `parentId` at all.
    expect(chatView).not.toMatch(
      /const stateKey = chatId\s*\n\s*\? turnStateKey\(chatId, threadRootOf\(openThreadId \?\? undefined\)\)\s*\n\s*: undefined;/,
    );
  });

  it("still keys a threaded send off openThreadId, not off its own parentId", () => {
    // The case the original shape was right about: a review reply's
    // `parentId` is an anchor *reply*, not the thread root, so the threaded
    // branch must keep consulting `openThreadId` — the same value the panel's
    // own `threadTurnKey` read uses — rather than switching to
    // `threadRootOf(parentId)`.
    expect(chatView).toMatch(/turnStateKey\(chatId, threadRootOf\(openThreadId \?\? undefined\)\)/);
    expect(chatView).not.toMatch(/turnStateKey\(chatId, threadRootOf\(parentId/);
  });

  it("retry replays the failed payload's own parentId, so an unthreaded retry keeps parentId undefined", () => {
    // `retrySend` must still forward `payload.parentId` verbatim into `send` —
    // this fix depends on that being `undefined` for a top-level failure, not
    // on `retrySend` inferring scope from the UI.
    expect(chatView).toMatch(
      /void send\(payload\.text, payload\.intent, payload\.parentId, payload\.attachments, payload\.mentions\);/,
    );
  });
});
