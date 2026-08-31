import type { Step } from "react-joyride";

import type { View } from "@/components/app-shell";

/**
 * One stop on the guided tour: a spotlight target plus the console view it
 * belongs to, so the controller can switch panes before highlighting.
 *
 * Anchoring strategy: most stops target an always-mounted **sidebar nav**
 * element (`[data-tour="nav-<view>"]`), which sidesteps the lazy/Suspense race
 * entirely — navigating still swaps the main pane so the operator sees the
 * view, but the spotlight never waits on a code-split chunk. Only the two
 * richest moments (the operator overview, the chat composer) anchor to content on
 * non-lazy views.
 */
export interface TourStop {
  view: View;
  /** A section's sub-page, when the stop lives inside one (`#/settings/…`). */
  sub?: string;
  target: string;
  title: string;
  body: string;
  placement?: Step["placement"];
}

export const TOUR: TourStop[] = [
  {
    view: "overview",
    target: '[data-tour="sidebar"]',
    placement: "right",
    title: "Welcome to your company",
    body: "Everything your company can do lives in this sidebar. Let's take a quick look around.",
  },
  {
    view: "overview",
    target: '[data-tour="overview-graph"]',
    placement: "top",
    title: "Start here each day",
    body: "Your company at a glance: its departments, the work in flight, who's doing it, and what it remembers. Pull the graph around to explore it.",
  },
  {
    view: "chat",
    target: '[data-tour="chat-composer"]',
    placement: "top",
    title: "Talk to your company",
    body: "Ask for an update or hand off a task in plain language — like messaging a teammate.",
  },
  {
    view: "chat",
    target: '[data-tour="nav-chat"]',
    placement: "right",
    title: "Your AI staff",
    body: "The teammates that actually do the work each have a channel and a direct message here. Open the team pane to see the whole roster.",
  },
  {
    view: "workflows",
    target: '[data-tour="nav-workflows"]',
    placement: "right",
    title: "Workflows",
    body: "Turn recurring work into a repeatable workflow — a graph of steps your teammates run end to end.",
  },
  {
    view: "approvals",
    target: '[data-tour="nav-approvals"]',
    placement: "right",
    title: "Approvals",
    body: "Anything that needs your sign-off before it happens waits here. Nothing risky runs without you.",
  },
  {
    // The accounts page is `#/settings/oauth` since the Connections split, so
    // the stop navigates there and spotlights the nav row that leads to it.
    view: "settings",
    sub: "oauth",
    target: '[data-tour="nav-settings"]',
    placement: "right",
    title: "Connect your tools",
    body: "Plug in the tools your company already uses — Gmail, Slack, Notion — so your teammates can act for real.",
  },
  {
    view: "chat",
    target: '[data-tour="chat-composer"]',
    placement: "top",
    title: "You're all set",
    body: "That's the tour. Say hi to your company to get going — you can replay this anytime from Settings.",
  },
];

/**
 * Poll for a step's target to mount. Route swaps and lazy Suspense chunks
 * resolve a tick or two after navigation, so a spotlight can't anchor on the
 * same frame it navigates. Resolves `false` on timeout so a missing anchor
 * degrades to a skipped step instead of wedging the tour. Mirrors OpenHuman's
 * `waitForTarget` (walkthroughSteps.ts).
 */
export function waitForTarget(selector: string, timeoutMs = 5000): Promise<boolean> {
  return new Promise((resolve) => {
    const start = Date.now();
    const tick = () => {
      if (document.querySelector(selector)) {
        resolve(true);
        return;
      }
      if (Date.now() - start > timeoutMs) {
        resolve(false);
        return;
      }
      window.setTimeout(tick, 50);
    };
    tick();
  });
}
