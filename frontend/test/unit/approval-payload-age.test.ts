import { describe, expect, it } from "vitest";

import type { ApprovalSummary } from "@/api/types";
import { leavesTheCompany, payloadAge } from "@/lib/language";

/**
 * Issue #1024: a `GMAIL_SEND_EMAIL` gate sat parked for days, and the moment an
 * operator cleared a backlog it mailed a digest built from 13 Aug under the
 * heading "Weekly Digest — 18 Aug 2026".
 *
 * The age was already on the card. It rendered as a bare "5d ago" in the footer
 * between "Asked by Maya" and "Open the card" — routing metadata — where it
 * reads as how long the QUEUE has held the item. What decides an outbound send
 * is that the PAYLOAD is five days old. Same integer, different fact, and only
 * the second one should stop a send. Hence the report's "from the operator's
 * side it looked like a routine send".
 *
 * These pin the label, not the number: asserting a time string alone would pass
 * against the pre-fix card too.
 */
const NOW = 1_755_500_000_000;
const FIVE_DAYS = 5 * 24 * 60 * 60 * 1000;

function approval(over: Partial<ApprovalSummary> = {}): ApprovalSummary {
  return {
    id: "a-1",
    kind: "composio_execute",
    amount_usd: null,
    at_millis: NOW - FIVE_DAYS,
    ...over,
  } as ApprovalSummary;
}

describe("how old the parked payload is", () => {
  it("says what the age is OF on an effect that leaves the company", () => {
    // The #1024 case exactly: a composio send, five days parked.
    const age = payloadAge(approval({ group: "send" }), NOW);
    expect(age.text).toBe("Composed 5d ago");
    expect(age.emphasise).toBe(true);
  });

  it("leaves an internal effect's age unlabelled", () => {
    // On a card that sends nothing outward the age genuinely IS queue latency,
    // and labelling it "Composed" everywhere would spend the emphasis where it
    // does not matter.
    const age = payloadAge(approval({ group: "other" }), NOW);
    expect(age.text).toBe("5d ago");
    expect(age.emphasise).toBe(false);
  });

  it("labels every outward group, not just sends", () => {
    for (const group of [
      "spend",
      "send",
      "sign",
      "publish",
      "hire",
      "identity",
    ] as const) {
      expect(payloadAge(approval({ group }), NOW).text).toBe("Composed 5d ago");
    }
  });

  it("claims nothing when the host does not classify the effect", () => {
    // An older host omits `group`. Silence is honest: the card renders exactly
    // as it did before #1024 rather than labelling something this console
    // cannot actually classify.
    const age = payloadAge(approval(), NOW);
    expect(age.text).toBe("5d ago");
    expect(age.emphasise).toBe(false);
  });

  it("reads outbound-ness from the host's group, never from the effect kind", () => {
    // The trap this exists to stop. For a harness tool call `kind` is the TOOL
    // NAME, so a predicate keyed on `kind` would miss the composio send that
    // #1024 was reported for while matching native effects like "email.send".
    expect(
      leavesTheCompany(approval({ kind: "composio_execute", group: "send" })),
    ).toBe(true);
    expect(
      leavesTheCompany(approval({ kind: "email.send", group: "other" })),
    ).toBe(false);
  });
});
