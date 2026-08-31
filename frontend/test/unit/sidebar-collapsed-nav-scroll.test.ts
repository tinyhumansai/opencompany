import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

/**
 * The collapsed rail's nav list must stay reachable, even when it does not
 * fit (issue #1931 review).
 *
 * `WindowControlsInset` (the macOS traffic-light strip) and `SidebarUtilityBar`
 * (Settings, Feedback, Discord, Collapse) both stack vertically above the nav
 * list in the collapsed rail. At the desktop's supported minimum window
 * height, that stack plus a full ten-row nav list can exceed the rail's
 * height. `SidebarContent` used to answer overflow there with
 * `overflow-hidden`, which clipped the last row(s) out of reach with no way
 * to get to them — not just visually cut off, but not pointer-reachable
 * either.
 *
 * jsdom does not evaluate `group-data-[collapsible=icon]:*` selectors (they
 * are plain CSS, keyed off a `data-*` attribute this suite never triggers a
 * real layout pass for), so this pins the source contract instead, the same
 * idiom `responsive-two-rail-band.test.ts` uses for a media-query fact jsdom
 * cannot evaluate either.
 */

const here = dirname(fileURLToPath(import.meta.url));
const sidebar = readFileSync(resolve(here, "../../src/components/ui/sidebar.tsx"), "utf8");

describe("SidebarContent stays scrollable when collapsed", () => {
  it("scrolls (auto) rather than clips (hidden) in the collapsed rail", () => {
    expect(sidebar).toContain("group-data-[collapsible=icon]:overflow-y-auto");
    expect(sidebar).not.toContain("group-data-[collapsible=icon]:overflow-hidden");
  });

  it("keeps the scrollbar itself invisible, matching the expanded rail's own look", () => {
    // `no-scrollbar` only hides the bar (see `index.css`); it does not disable
    // scrolling. Losing it here would be a visual regression on collapse, not
    // a functional one, but the two are meant to travel together.
    const idx = sidebar.indexOf("group-data-[collapsible=icon]:overflow-y-auto");
    expect(idx).toBeGreaterThan(-1);
    const classAttr = sidebar.slice(Math.max(0, idx - 80), idx);
    expect(classAttr).toContain("no-scrollbar");
  });
});
