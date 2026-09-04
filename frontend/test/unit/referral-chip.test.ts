import { describe, expect, it } from "vitest";
import { fromHistory } from "@/lib/chat";

describe("referredFrom", () => {
  it("carries a crossing referral's provenance through history", () => {
    const [msg] = fromHistory([
      {
        id: "9",
        channel: "product_designer",
        author: "product_designer",
        text: "here is the login mock",
        atMillis: 1,
        mine: false,
        referredFrom: {
          deskId: "engineering",
          deskName: "Engineering",
          askerId: "software_engineer",
          askerLabel: "10x engineer",
          sequence: 4821,
        },
      } as never,
    ]);
    expect(msg.referredFrom?.deskName).toBe("Engineering");
    expect(msg.referredFrom?.sequence).toBe(4821);
  });

  it("carries the host's own word for which leg this is", () => {
    // Both legs of a referral are agent-authored `company` lines, so nothing
    // on the message distinguishes them. The console used to guess from
    // `from`, which made every returning answer read "Asked by" — the chip
    // claiming design had asked a question design had in fact answered.
    const [asked, answered] = fromHistory([
      {
        id: "9",
        channel: "software_engineer",
        author: "software_engineer",
        text: "@#design what would you change about the error messages?",
        atMillis: 1,
        mine: false,
        referredFrom: {
          deskId: "engineering",
          deskName: "Engineering",
          askerId: "software_engineer",
          askerLabel: "10x engineer",
          sequence: 4821,
          direction: "asked",
        },
      } as never,
      {
        id: "11",
        channel: "product_designer",
        author: "product_designer",
        text: "error messages are one of the things that look like a copy task",
        atMillis: 2,
        mine: false,
        referredFrom: {
          deskId: "design",
          deskName: "Design",
          askerId: "product_designer",
          askerLabel: "Product Designer",
          sequence: 4830,
          direction: "answered",
        },
      } as never,
    ]);
    expect(asked.referredFrom?.direction).toBe("asked");
    expect(answered.referredFrom?.direction).toBe("answered");
    // Identical on the terms the old guess used — which is the whole point.
    expect(asked.from).toBe(answered.from);
  });

  it("is absent on an ordinary message", () => {
    const [msg] = fromHistory([
      { id: "1", channel: "ceo", author: "ceo", text: "hi", atMillis: 1, mine: false } as never,
    ]);
    expect(msg.referredFrom).toBeUndefined();
  });
});
