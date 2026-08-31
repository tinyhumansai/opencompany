import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import type { OperatorChannelDto } from "@/api/types";
import { operatorThread } from "@/lib/threads";

/**
 * The legacy `#/conversation` route's own copy of the pinned Operator row
 * (issue #1781 review, Codex P2).
 *
 * `ChatView`'s channel model already appends the Operator feed through
 * `operatorSection` (`views/chat/model.ts`). `Conversation` reads a plain
 * `Thread[]` instead, and app-shell.tsx's hydration pass fetched and
 * hydrated the Operator channel's identity without ever folding it into that
 * list — so `#/conversation` never received an Operator thread at all, and
 * `Conversation.tsx`'s existing `readOnly={thread.readOnly}` forward
 * (`Conversation.tsx:232`) had nothing to gate: workflow reports could not
 * be opened there.
 */
describe("operatorThread (issue #1781 review, Codex P2)", () => {
  const dto: OperatorChannelDto = {
    id: "operator",
    name: "Operator",
    description: "Workflow reports and notifications — what happened and what needs you",
  };

  it("builds a read-only thread from the Operator channel DTO", () => {
    const thread = operatorThread(dto);
    expect(thread).toEqual({
      id: "operator",
      contact: { name: "Operator", kind: "company" },
      blurb: "Workflow reports and notifications — what happened and what needs you",
      messages: [],
      readOnly: true,
    });
  });

  it("carries the grandfathered collision-fallback id unchanged", () => {
    // `operator_feed_channel()` diverts a grandfathered company onto
    // `operator-feed` (src/ports/types.rs) — this helper does not re-derive
    // that address, it only projects whatever id the host already resolved.
    const diverted = operatorThread({ ...dto, id: "operator-feed" });
    expect(diverted.id).toBe("operator-feed");
  });
});

/**
 * The scan above proves the builder. This proves the shell actually calls
 * it into the thread list `Conversation` reads — the same source-wiring
 * pattern `chat-general-channel.test.ts` and `chat-realtime-poll.test.ts`
 * use for app-shell.tsx's async hydration effect, which has no render-test
 * harness in this repo.
 */
describe("the shell folds the Operator channel into Conversation's own thread list", () => {
  const here = dirname(fileURLToPath(import.meta.url));
  const shell = readFileSync(resolve(here, "../../src/components/app-shell.tsx"), "utf8").replace(
    /\s+/g,
    " ",
  );

  it("appends an operatorThread to the resolved thread list", () => {
    expect(shell).toContain(
      "...(operatorChannel ? [operatorThread(operatorChannel)] : []),",
    );
  });

  it("Conversation still reads the same threads state this appends to", () => {
    expect(shell).toContain("<Conversation client={client} company={company} threads={threads}");
  });
});
