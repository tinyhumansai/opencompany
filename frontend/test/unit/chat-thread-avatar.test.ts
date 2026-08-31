// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { hashedFlavour, tinySrc } from "@/lib/avatar";
import type { ChatMessage } from "@/lib/chat";
import type { TeamMember } from "@/lib/team";
import { ThreadPanel } from "@/views/chat/ThreadPanel";
import type { Channel } from "@/views/chat/model";

/**
 * Issue #1729: the thread panel drew the agent's mascot for the current user.
 *
 * `senderOf` seeds a face off the sender's *name* when it is handed none, and a
 * "you" line's name is the literal string "You" — so the panel drew whatever
 * `avatarFor("You")` hashes to for every reader, regardless of the face they
 * had actually chosen. Both participants ended up with the same mascot and the
 * thread could not be read at all.
 *
 * The main timeline never had this: `buildTimeline` has always taken a
 * `youAvatar`. Only this panel resolved its senders without one.
 */

const CHANNEL: Channel = {
  id: "engineering",
  name: "engineering",
  voice: "Engineering",
  kind: "channel",
  purpose: "",
};

/** The agent's own face — deliberately the one `avatarFor("You")` collides with. */
const AGENT_AVATAR = `tiny:${hashedFlavour("You")}`;

const MEMBERS: TeamMember[] = [
  {
    id: "ada",
    name: "Ada",
    role: "Engineer",
    description: "",
    tone: "amber",
    avatar: AGENT_AVATAR,
    inboxEnabled: true,
    effectiveTools: [],
    desks: [],
  },
];

/** The face the signed-in operator actually chose. */
const YOU_AVATAR = "tiny:rose";

const YOURS: ChatMessage = { id: "p", from: "you", text: "any update?", at: 0 };
const THEIRS: ChatMessage = {
  id: "r",
  parentId: "p",
  from: "company",
  channel: "ada",
  text: "shipped it",
  at: 1,
};

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

async function render(over: Partial<Parameters<typeof ThreadPanel>[0]> = {}) {
  await act(async () => {
    root.render(
      createElement(ThreadPanel, {
        channel: CHANNEL,
        members: MEMBERS,
        parent: YOURS,
        replies: [THEIRS],
        sending: false,
        onSend: vi.fn(),
        onClose: vi.fn(),
        ...over,
      }),
    );
  });
  await act(async () => {
    await Promise.resolve();
  });
}

/** The `src` of the avatar on the line whose sender is `kind`. */
function face(kind: "you" | "agent"): string {
  const tile = container.querySelector(`[data-testid="thread-avatar-${kind}"]`);
  return tile?.querySelector("img")?.getAttribute("src") ?? "";
}

describe("ThreadPanel avatars (issue #1729)", () => {
  it("draws your own face on your own line", async () => {
    await render({ youAvatar: YOU_AVATAR });
    const mine = face("you");
    const theirs = face("agent");
    expect(mine).toBe(tinySrc("rose"));
    expect(theirs).toBe(tinySrc(hashedFlavour("You")));
    expect(mine).not.toBe(theirs);
  });

  it("is the same face the main timeline draws for you", async () => {
    // The bug, stated as the property that was violated: with a `youAvatar` in
    // hand the panel must never fall back to seeding on the name "You" — which
    // is what produced the agent's mascot.
    await render({ youAvatar: YOU_AVATAR });
    expect(face("you")).not.toBe(tinySrc(hashedFlavour("You")));
  });

  it("falls back to the name-seeded mascot only before the viewer resolves", async () => {
    // `loadViewer` has not answered yet, so there is no face to draw and the
    // tile seeds on the name, exactly as the main timeline does for that first
    // render. Degrading is fine; degrading forever was the bug.
    await render({ youAvatar: undefined });
    expect(face("you")).toBe(tinySrc(hashedFlavour("You")));
  });
});
