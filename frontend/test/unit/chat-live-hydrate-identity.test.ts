import { describe, expect, it } from "vitest";

import { fromHistory, liveReplyIdentity, makeMessage } from "@/lib/chat";
import type { ChatHistoryMessageDto } from "@/api/types";

/**
 * Issue #483 — a reply that arrived live was rendered a second time when its
 * channel was later hydrated from `chat/history`.
 *
 * The cause was an identity gap, not a race. Two writers put a company reply
 * into a transcript, and they deduped on different keys that could never agree:
 *
 * - the live stream injection deduped on **content**, against the recent tail;
 * - `hydrateChannel` deduped on **id**.
 *
 * A live line used to be built with an ephemeral `m<seq>` counter, so hydration
 * — which prepends `h`-prefixed ids — never recognised it, called it fresh, and
 * inserted the same reply twice. The content check could not cover it from the
 * other side either, because hydration prepends rather than appending to the
 * tail that check scans.
 *
 * The fix is that the live line now carries the host's own id. This file pins
 * the property that makes that work: the stream frame's `seq` and the history
 * entry's `id` are the same host identifier — both projected from one
 * `StoredEvent.seq` — so stamping the live line with it yields *the same
 * console id* the rehydrated twin will get.
 *
 * These are string-level assertions on purpose. If the namespacing of either
 * side ever changes independently, the duplicate returns silently, and the only
 * symptom is a reply appearing twice in a channel the operator had closed.
 */

function historyEntry(over: Partial<ChatHistoryMessageDto> = {}): ChatHistoryMessageDto {
  return {
    id: "4211",
    channel: "engineering",
    author: "engineering",
    text: "You said: quarterly numbers",
    atMillis: 1_700_000_000_000,
    mine: false,
    ...over,
  };
}

describe("a live reply and its rehydrated twin", () => {
  it("resolve to one console id, so hydration can recognise the line it already has", () => {
    const seq = 4211;

    // Built exactly the way the stream injection builds it — through
    // `liveReplyIdentity`, not by re-deriving the id here. Asserting against a
    // hand-written `messageId` would pass even if the injection stopped
    // stamping one, which is the failure this test exists to prevent.
    const live = makeMessage("company", "You said: quarterly numbers", {
      channel: "engineering",
      ...liveReplyIdentity({ seq }),
    });

    // What `chat/history` yields for the same message.
    const [rehydrated] = fromHistory([historyEntry({ id: String(seq) })]);

    expect(live.id).toBe(rehydrated.id);
  });

  it("is deduped by the id check hydration actually runs", () => {
    const seq = 4211;
    const live = makeMessage("company", "You said: quarterly numbers", {
      channel: "engineering",
      ...liveReplyIdentity({ seq }),
    });
    const hydrated = fromHistory([historyEntry({ id: String(seq) })]);

    // `hydrateChannel`'s filter, verbatim in shape: anything already known by
    // id is not fresh. Before the fix `live.id` was `m<counter>` and this set
    // never contained the hydrated id, so the reply was inserted a second time.
    const known = new Set([live].map((m) => m.id));
    const fresh = hydrated.filter((m) => !known.has(m.id));

    expect(fresh).toHaveLength(0);
  });

  it("still separates two genuinely different messages", () => {
    const first = makeMessage("company", "same words", { messageId: "1" });
    const second = makeMessage("company", "same words", { messageId: "2" });

    // Identical text, different host messages. Collapsing these would be the
    // opposite defect — a real reply silently swallowed — so the id must
    // distinguish them even though a content check could not.
    expect(first.id).not.toBe(second.id);

    const known = new Set([first.id]);
    expect(fromHistory([historyEntry({ id: "2", text: "same words" })]).filter(
      (m) => !known.has(m.id),
    )).toHaveLength(1);
  });

  it("leaves a line with no host id in the browser-local namespace", () => {
    // The operator's own optimistic send has no host id yet. It must NOT
    // collide with a host id, or reconciliation would swap the wrong line.
    const optimistic = makeMessage("you", "quarterly numbers", {});
    const hosted = makeMessage("company", "…", { messageId: "1" });

    expect(optimistic.id).not.toBe(hosted.id);
    expect(hosted.id.startsWith("h")).toBe(true);
  });
});
