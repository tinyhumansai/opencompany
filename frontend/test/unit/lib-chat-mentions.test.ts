import { describe, expect, it } from "vitest";

import { makeMessage, type Mention } from "@/lib/chat";

/**
 * The message construction half of @-mentions: `makeMessage` now carries
 * resolved mentions through so the renderer can chip them on every path —
 * the optimistic POST response, the SSE stream, and history rehydration.
 */
const sampleMentions: Mention[] = [
  { text: "@engineer", offset: 0, label: "engineer", mine: false },
  { text: "@jane", offset: 9, label: "Jane Doe", mine: true },
];

describe("makeMessage with mentions", () => {
  it("carries mentions when passed", () => {
    const msg = makeMessage("company", "hello @engineer @jane", {
      mentions: sampleMentions,
    });
    expect(msg.mentions).toEqual(sampleMentions);
  });

  it("omits mentions when absent", () => {
    const msg = makeMessage("company", "hello");
    expect(msg.mentions).toBeUndefined();
  });

  it("omits mentions when the array is empty", () => {
    const msg = makeMessage("company", "hello", { mentions: [] });
    expect(msg.mentions).toBeUndefined();
  });

  it("carries channel, steps, taskId alongside mentions", () => {
    const msg = makeMessage("company", "reply", {
      channel: "operator",
      steps: [],
      taskId: "t1",
      mentions: sampleMentions,
    });
    expect(msg.channel).toBe("operator");
    expect(msg.steps).toEqual([]);
    expect(msg.taskId).toBe("t1");
    expect(msg.mentions).toEqual(sampleMentions);
  });

  it("renders a quiet mention through", () => {
    const quiet: Mention[] = [
      { text: "@former", offset: 0, label: "Former Teammate", mine: false, quiet: true },
    ];
    const msg = makeMessage("company", "@former", { mentions: quiet });
    expect(msg.mentions?.[0].quiet).toBe(true);
  });
});
