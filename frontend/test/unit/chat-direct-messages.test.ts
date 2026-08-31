import { describe, expect, it } from "vitest";

import type { TeamMember } from "@/lib/team";
import { buildChannels, directMessageForId } from "@/views/chat/model";

function member(id: string, name: string): TeamMember {
  return {
    id,
    name,
    role: "Engineer",
    description: "",
    tone: "sky",
    avatar: "green",
    inboxEnabled: false,
    effectiveTools: [],
    desks: [],
  };
}

describe("direct-message channels", () => {
  it("shows only conversations with messages, newest first", () => {
    const ada = member("ada", "Ada");
    const ben = member("ben", "Ben");
    const cy = member("cy", "Cy");

    const dms = buildChannels([ada, ben, cy], [], {
      "dm:ada": [{ id: "a", from: "you", text: "Earlier", at: 10 }],
      "dm:ben": [
        { id: "b1", from: "you", text: "Old", at: 5 },
        { id: "b2", from: "company", text: "Newest", at: 20 },
      ],
    }).find((section) => section.id === "dms")?.channels;

    expect(dms?.map((channel) => channel.id)).toEqual(["dm:ben", "dm:ada"]);
  });

  it("resolves an unused DM by its stable id for the picker and saved links", () => {
    const ada = member("ada", "Ada");
    expect(directMessageForId([ada], "dm:ada")?.name).toBe("Ada");
  });
});
