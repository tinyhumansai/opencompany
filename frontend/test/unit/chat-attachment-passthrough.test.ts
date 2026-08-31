import { describe, expect, it } from "vitest";

import type { AttachmentDto, ChatHistoryMessageDto } from "@/api/types";
import { fromHistory, makeMessage, reconcileIds } from "@/lib/chat";

/**
 * Issue #1682: an attachment must survive every hop the console model puts a
 * message through — the optimistic build, the reload from history, and the
 * optimistic→durable id swap — or a file that showed live vanishes on the next
 * repaint. These pin the passthrough at each seam.
 */

const attachment: AttachmentDto = {
  nodeId: "node-1",
  name: "diagram.png",
  mime: "image/png",
  size: 2048,
};

describe("makeMessage — attachments", () => {
  it("carries a supplied attachment onto the line", () => {
    const message = makeMessage("you", "see this", { attachments: [attachment] });
    expect(message.attachments).toEqual([attachment]);
  });

  it("leaves the field absent when there is none — the pre-#1682 shape", () => {
    expect(makeMessage("you", "hi").attachments).toBeUndefined();
    expect(makeMessage("you", "hi", { attachments: [] }).attachments).toBeUndefined();
  });
});

describe("fromHistory — attachments", () => {
  it("rehydrates a persisted attachment onto the operator's line", () => {
    const entry: ChatHistoryMessageDto = {
      id: "42",
      channel: "operator",
      author: "operator",
      text: "here it is",
      atMillis: 1,
      mine: true,
      attachments: [attachment],
    };
    const [message] = fromHistory([entry]);
    expect(message.attachments).toEqual([attachment]);
  });

  it("leaves it absent when the history entry carries none", () => {
    const entry: ChatHistoryMessageDto = {
      id: "43",
      channel: "operator",
      author: "operator",
      text: "no file",
      atMillis: 1,
      mine: true,
    };
    expect(fromHistory([entry])[0].attachments).toBeUndefined();
  });
});

describe("reconcileIds — attachments", () => {
  it("preserves attachments when the optimistic id is swapped for the durable one", () => {
    const local = makeMessage("you", "see this", { attachments: [attachment] });
    const [reconciled] = reconcileIds([local], local.id, "77");
    expect(reconciled.id).toBe("h77");
    expect(reconciled.attachments).toEqual([attachment]);
  });
});
