// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { staticAvatarSrc } from "@/lib/avatar";
import { avatarFor, type TeamMember } from "@/lib/team";
import { MessageTimeline } from "@/views/chat/MessageTimeline";
import type { Channel } from "@/views/chat/model";

/**
 * What the channel intro draws above a channel's name (issue #1327).
 *
 * The rule the header settled in #1170, applied one block lower: a DM has
 * exactly one person on the other end and wears their face; a channel has
 * nobody behind it and wears its kind.
 *
 * The bug this pins is silent in exactly the way #1170's was. Every channel but
 * `main` fell through to `TeammateAvatar` seeded on the channel *name*, so
 * `#engineering` grew a mascot belonging to no one — at the largest avatar size
 * on the surface, as the first thing in the pane — while the header a few
 * pixels above drew `#` for the same channel. Nothing throws and nothing fails
 * to render; the two marks simply disagree on screen, and only an assertion
 * about *which* mark was drawn catches it.
 *
 * The marks are told apart by what they are made of rather than by a class
 * name: only `TeammateAvatar`'s mascot branch renders an `<img>`, so its
 * presence is the whole question here.
 */

let container: HTMLDivElement;
let root: Root;

function member(id: string, name: string): TeamMember {
  return {
    id,
    name,
    role: "Engineer",
    description: "",
    tone: "sky",
    avatar: avatarFor(id),
    inboxEnabled: false,
    effectiveTools: [],
    desks: [],
  };
}

// `createElement` rather than JSX because the unit suite's vitest `include` is
// `*.test.ts` — a `.tsx` file is silently not collected, which reads as a
// passing suite.
function render(channel: Channel) {
  act(() => {
    root.render(
      createElement(MessageTimeline, {
        channel,
        items: [],
        historyPending: false,
        openThreadId: null,
        typing: false,
        onOpenThread: () => {},
        onReact: () => {},
        onDismissCard: () => {},
        dismissingCardId: null,
      }),
    );
  });
}

/**
 * The intro's mark: the first element inside the intro block.
 *
 * Reached structurally rather than by test id because the point of the change
 * is *which element type* is rendered there — a query that named one of them
 * would pass by construction.
 */
function mark(): HTMLElement {
  const intro = container.firstElementChild!.firstElementChild!.firstElementChild!;
  return intro.firstElementChild as HTMLElement;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the channel intro's mark", () => {
  it("draws a DM's teammate, face and all", () => {
    render({
      id: "dm:agent_ada",
      name: "Ada",
      kind: "dm",
      purpose: "",
      tone: "violet",
      member: member("agent_ada", "Ada"),
    });

    const img = mark().querySelector("img");
    expect(img).not.toBeNull();
    // Seeded through `dmFace`, so the intro cannot disagree with the rail row
    // or the header about who this DM is with.
    expect(img!.getAttribute("src")).toBe(staticAvatarSrc(avatarFor("agent_ada")));
  });

  it("draws no face for a channel — there is no one person behind it", () => {
    render({ id: "engineering", name: "engineering", kind: "channel", purpose: "" });

    expect(mark().querySelector("img")).toBeNull();
    // The kind mark instead, on the icon ground the action cards below it use.
    expect(mark().className).toContain("bg-surface-icon");
    expect(mark().querySelector("svg")).not.toBeNull();
  });

  it("draws no face for a private channel either — the lock speaks for it", () => {
    render({ id: "ops", name: "ops", kind: "channel", purpose: "", private: true });

    expect(mark().querySelector("img")).toBeNull();
    expect(mark().className).toContain("bg-surface-icon");
  });

  it("keeps the company's own brand mark on the main line", () => {
    // `main` is the one channel that legitimately has a voice behind it, and it
    // wears the company mark rather than a mascot or a hash.
    render({ id: "main", name: "general", voice: "Acme", kind: "channel", purpose: "" });

    expect(mark().querySelector("img")).toBeNull();
    expect(mark().className).toContain("bg-primary");
  });

  it("falls back to a glyph for a DM the roster cannot name", () => {
    // No `member`, so `dmFace` answers null. Inventing a mascot for a stranger
    // is exactly what the header refuses to do here.
    render({ id: "dm:gone", name: "Gone", kind: "dm", purpose: "" });

    expect(mark().querySelector("img")).toBeNull();
    expect(mark().querySelector("svg")).not.toBeNull();
  });
});
