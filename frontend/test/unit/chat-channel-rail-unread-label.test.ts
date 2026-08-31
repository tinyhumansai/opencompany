// @vitest-environment jsdom

import { act, createElement, Fragment, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { ChannelRail } from "@/views/chat/ChannelRail";
import type { ChannelSection } from "@/views/chat/model";

/**
 * The compact rail keeps an unread channel's count in its accessible name
 * (issue #364, P2 review).
 *
 * The expanded row announces unread because the count is text inside the
 * button; the collapsed row draws it as a bare dot, which is invisible to
 * screen readers. The fix puts the same count in the compact button's
 * `aria-label`, so collapsing the rail does not strip the fact from the
 * accessibility tree. These pin that label directly.
 */

const SECTIONS: ChannelSection[] = [
  {
    id: "s1",
    label: "Company",
    channels: [
      { id: "front-desk", name: "Front desk", kind: "channel", purpose: "The front line." },
      { id: "ops", name: "Ops", kind: "channel", purpose: "Where work lands." },
    ],
  },
];

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
});

const channelButtons = () =>
  [...container.querySelectorAll<HTMLButtonElement>('nav[aria-label="Channels"] button')].filter(
    (b) => b.getAttribute("aria-label") !== "Expand channels",
  );

describe("collapsed ChannelRail unread labels", () => {
  it("names an unread channel with its count", () => {
    act(() =>
      root.render(
        createElement(ChannelRail, {
          sections: SECTIONS,
          activeId: null,
          unread: { "front-desk": 3 },
          onSelect: () => {},
          collapsed: true,
        }),
      ),
    );

    const buttons = channelButtons();
    expect(buttons.map((b) => b.getAttribute("aria-label"))).toEqual([
      "Front desk, 3 unread",
      "Ops",
    ]);
  });

  it("includes mention and unread counts in the compact accessible name", () => {
    act(() =>
      root.render(
        createElement(ChannelRail, {
          sections: SECTIONS,
          activeId: null,
          unread: { "front-desk": 3 },
          mentions: { "front-desk": 2 },
          onSelect: () => {},
          collapsed: true,
        }),
      ),
    );

    expect(channelButtons()[0].getAttribute("aria-label")).toBe("Front desk, 2 mentions, 3 unread");
    expect(container.querySelectorAll('[data-testid="channel-mentions"]')).toHaveLength(1);
  });
  it("caps a huge count the way the expanded badge does", () => {
    act(() =>
      root.render(
        createElement(ChannelRail, {
          sections: SECTIONS,
          activeId: null,
          unread: { "front-desk": 142 },
          onSelect: () => {},
          collapsed: true,
        }),
      ),
    );

    expect(channelButtons()[0].getAttribute("aria-label")).toBe("Front desk, 99+ unread");
  });

  it("keeps the active channel's label bare even when unread", () => {
    act(() =>
      root.render(
        createElement(ChannelRail, {
          sections: SECTIONS,
          activeId: "front-desk",
          unread: { "front-desk": 7 },
          onSelect: () => {},
          collapsed: true,
        }),
      ),
    );

    // The unread dot does not render on the channel you are already reading,
    // so the label must not claim unread either.
    expect(channelButtons()[0].getAttribute("aria-label")).toBe("Front desk");
  });
});

describe("section folds survive a rail collapse/expand (P2 review)", () => {
  const sectionToggle = () =>
    container.querySelector<HTMLButtonElement>('section button[aria-expanded]');

  it("does not reopen a folded section when the rail is collapsed and expanded again", () => {
    act(() =>
      root.render(
        createElement(ChannelRail, {
          sections: SECTIONS,
          activeId: null,
          unread: {},
          onSelect: () => {},
        }),
      ),
    );

    expect(sectionToggle()?.getAttribute("aria-expanded")).toBe("true");
    act(() => {
      sectionToggle()?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(sectionToggle()?.getAttribute("aria-expanded")).toBe("false");

    // Collapsing unmounts every `Section`; expanding must recreate them still
    // folded rather than resetting the operator's organization.
    act(() =>
      root.render(
        createElement(ChannelRail, {
          sections: SECTIONS,
          activeId: null,
          unread: {},
          onSelect: () => {},
          collapsed: true,
        }),
      ),
    );
    act(() =>
      root.render(
        createElement(ChannelRail, {
          sections: SECTIONS,
          activeId: null,
          unread: {},
          onSelect: () => {},
          collapsed: false,
        }),
      ),
    );

    expect(sectionToggle()?.getAttribute("aria-expanded")).toBe("false");
  });

  it("shares one fold set across the desktop and sub-lg rail instances", () => {
    // `ChatView` renders two `ChannelRail`s (sub-`lg` and desktop) and hands
    // both the same controlled disclosure state so crossing the breakpoint
    // keeps the operator's folds (codex P2 review). This harness mirrors that
    // wiring; folding on one rail must fold the same section on the other.
    const SharedRails = () => {
      const [folds, setFolds] = useState<Record<string, boolean>>({});
      const toggle = (id: string) =>
        setFolds((prev) => ({ ...prev, [id]: !(prev[id] ?? true) }));
      return createElement(
        Fragment,
        null,
        createElement(ChannelRail, {
          sections: SECTIONS,
          activeId: null,
          unread: {},
          onSelect: () => {},
          openSections: folds,
          onToggleSection: toggle,
        }),
        createElement(ChannelRail, {
          sections: SECTIONS,
          activeId: null,
          unread: {},
          onSelect: () => {},
          openSections: folds,
          onToggleSection: toggle,
        }),
      );
    };

    act(() => root.render(createElement(SharedRails)));
    const toggles = () =>
      [...container.querySelectorAll<HTMLButtonElement>("section button[aria-expanded]")];
    expect(toggles()).toHaveLength(2);
    expect(toggles()[0].getAttribute("aria-expanded")).toBe("true");
    expect(toggles()[1].getAttribute("aria-expanded")).toBe("true");

    act(() => {
      toggles()[0].dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(toggles()[0].getAttribute("aria-expanded")).toBe("false");
    expect(toggles()[1].getAttribute("aria-expanded")).toBe("false");
  });
});

/**
 * The pinned Operator row shows unread the same as any other channel
 * (PR #1781 review, Codex P2).
 *
 * A workflow report can land in the Operator feed while another channel is
 * open, same as any other channel — the collapsed rail's `CompactChannelRow`
 * already surfaced that (it flat-maps every section, this one included), but
 * `PinnedOperatorRow` in the expanded rail never received the `unread` map at
 * all, so folding the rail changed whether the pinned row could tell you
 * something was waiting.
 */
const OPERATOR_SECTIONS: ChannelSection[] = [
  {
    id: "operator",
    label: "Operator",
    channels: [
      {
        id: "operator",
        name: "Operator",
        kind: "channel",
        purpose: "Workflow reports.",
        system: true,
      },
    ],
  },
];

describe("expanded ChannelRail pinned Operator row unread", () => {
  const pinnedRow = () =>
    container.querySelector<HTMLButtonElement>('aside button[aria-current], aside button');

  it("shows the unread count on the pinned row when a report lands unseen", () => {
    act(() =>
      root.render(
        createElement(ChannelRail, {
          sections: OPERATOR_SECTIONS,
          activeId: null,
          unread: { operator: 2 },
          onSelect: () => {},
        }),
      ),
    );

    const badge = container.querySelector('[data-testid="channel-unread"]');
    expect(badge?.textContent).toBe("2");
  });

  it("renders no badge when the Operator feed has nothing unread", () => {
    act(() =>
      root.render(
        createElement(ChannelRail, {
          sections: OPERATOR_SECTIONS,
          activeId: null,
          unread: {},
          onSelect: () => {},
        }),
      ),
    );

    expect(container.querySelector('[data-testid="channel-unread"]')).toBeNull();
  });

  it("suppresses the badge while the Operator feed is the active channel", () => {
    act(() =>
      root.render(
        createElement(ChannelRail, {
          sections: OPERATOR_SECTIONS,
          activeId: "operator",
          unread: { operator: 5 },
          onSelect: () => {},
        }),
      ),
    );

    expect(container.querySelector('[data-testid="channel-unread"]')).toBeNull();
    expect(pinnedRow()?.getAttribute("aria-current")).toBe("page");
  });
});
