import { describe, expect, it } from "vitest";

import { PendingSyncPosts } from "@/lib/live-reply";

/**
 * A held system-attributed live `agent_reply` frame (`SYSTEM_AUTHOR`, B-101's
 * mention-ambiguity notice among others) must survive `ended()` exactly when
 * the settled response's own body never carried it — and must NOT survive
 * when the response did (CodeRabbit review, PR #2052).
 *
 * `ended` used to discard every held frame unconditionally on the assumption
 * that the awaited response already rendered it — true for the operator's
 * own reply, which is always in the response, but false for a system frame:
 * some (`system_notice`'s approval-overflow / `"Acknowledged."` fallback) ARE
 * folded into the same response body a normal reply is; B-101's
 * mention-ambiguity note deliberately never is
 * (`post_mention_ambiguity_note`'s own doc: "Journaled, not returned in the
 * POST response"). An earlier fix here exempted every system frame from
 * suppression at *capture* time instead, which fixed the ambiguity note but
 * then double-rendered a `system_notice` fallback the response DOES carry —
 * once from the unsuppressed live frame, once from `ChatView`'s own append
 * of that same response text. The fix that does not trade one bug for the
 * other belongs at *release*, where the response's own text is available:
 * `ended(threadId, responseTexts)` discards a held system frame only when
 * its text is present in `responseTexts`, and releases it — for the caller
 * to render — otherwise.
 */
describe("PendingSyncPosts.ended reconciles held system frames against the response", () => {
  it("still holds a system-attributed frame while the thread's POST is in flight", () => {
    const pending = new PendingSyncPosts();
    pending.started("main");

    // `true` means held, exactly like any other frame — the fix is not at
    // capture time (see the class's own `capture` doc for why that was tried
    // and reverted).
    expect(pending.capture({ chatId: "main", agentId: "system", text: "note" })).toBe(true);
  });

  it("releases a held system frame the settled response never carried", () => {
    const pending = new PendingSyncPosts();
    pending.started("main");
    const note = { chatId: "main", agentId: "system", text: "you meant one of two teammates" };
    pending.capture(note);

    // The response body carried only the operator's own reply text — never
    // this note, which is B-101's whole point.
    expect(pending.ended("main", ["here is your answer"])).toEqual([note]);
  });

  it("discards a held system frame the settled response DID carry", () => {
    const pending = new PendingSyncPosts();
    pending.started("main");
    const fallback = { chatId: "main", agentId: "system", text: "Acknowledged." };
    pending.capture(fallback);

    // `system_notice`'s fallback IS folded into `channel_responses` the same
    // way any reply is — releasing it here would double it against
    // `ChatView`'s own append of the identical response text.
    expect(pending.ended("main", ["Acknowledged."])).toEqual([]);
  });

  it("still discards an ordinary company frame unconditionally, regardless of the response", () => {
    const pending = new PendingSyncPosts();
    pending.started("main");
    const reply = { chatId: "main", agentId: "engineer", text: "here is your answer" };
    pending.capture(reply);

    // The operator's own reply is always in the response — no responseTexts
    // needed to know that, and none given here.
    expect(pending.ended("main")).toEqual([]);
  });

  it("defaults to an empty response, releasing every held system frame", () => {
    // A caller with no response text to reconcile against (the legacy
    // Conversation surface's `onSendEnd?.(threadId, gen)`, which passes only
    // two arguments) must not silently swallow the notice either — the safe
    // default is "the response carried nothing", same as before any system
    // frame existed to reconcile.
    const pending = new PendingSyncPosts();
    pending.started("main");
    const note = { chatId: "main", agentId: "system", text: "note" };
    pending.capture(note);

    expect(pending.ended("main")).toEqual([note]);
  });

  it("still releases a held system frame on detached() and failed(), unfiltered", () => {
    // Neither outcome has a response body to reconcile against — `detached`
    // and `failed` are untouched by this fix and keep releasing everything.
    const detachedCase = new PendingSyncPosts();
    detachedCase.started("main");
    const note = { chatId: "main", agentId: "system", text: "note" };
    detachedCase.capture(note);
    expect(detachedCase.detached("main")).toEqual([note]);

    const failedCase = new PendingSyncPosts();
    failedCase.started("main");
    failedCase.capture(note);
    expect(failedCase.failed("main")).toEqual([note]);
  });
});
