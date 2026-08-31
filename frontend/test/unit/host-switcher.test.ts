import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  hostSwitcherInteractive,
  hostSwitcherMenu,
  statusCopy,
  worstStatus,
} from "@/components/host-switcher";
import { hostShortcutLabel, HOST_SHORTCUT_LIMIT } from "@/connections/HostsContext";
import type { Connection, ConnectionStatus } from "@/connections/types";

/**
 * The switcher's trigger is the only place cross-host health survives.
 *
 * The rail it replaced (issue #1142) showed one dot per host, permanently. A
 * dropdown hides its rows, so an operator running three hosts learns nothing
 * about the two they are not looking at unless the trigger says so — and what
 * it says is this function. Getting the ordering wrong is silent: a console
 * that reports "Connected" while a host is unreachable looks exactly like a
 * console where everything is fine.
 */
function host(status: ConnectionStatus, id: string = status): Connection {
  return {
    id,
    defaultCompany: null,
    label: id,
    baseUrl: "",
    credential: { kind: "cookie" },
    status,
    identity: null,
    companies: [],
    connector: { kind: "remote" },
  };
}

describe("worstStatus", () => {
  it("has nothing to report with no hosts", () => {
    expect(worstStatus([])).toBeNull();
  });

  it("stays quiet while every host is live", () => {
    expect(worstStatus([host("live", "a"), host("live", "b")])).toBe("live");
  });

  it("reports the unreachable host, not the one on screen", () => {
    // THE case. The host being viewed is fine; the trigger must still say that
    // something, somewhere, is not.
    expect(worstStatus([host("live"), host("down")])).toBe("down");
  });

  it("prefers a host that is gone over one that is merely refusing", () => {
    expect(worstStatus([host("unauthenticated"), host("down")])).toBe("down");
    expect(worstStatus([host("degraded"), host("unauthenticated")])).toBe("unauthenticated");
  });

  it("does not claim everything is fine while a roster is still settling", () => {
    expect(worstStatus([host("live"), host("connecting")])).toBe("connecting");
  });
});

describe("statusCopy", () => {
  it("says what an ordinary host is doing", () => {
    expect(statusCopy(host("connecting")).label).toBe("Connecting…");
    expect(statusCopy(host("down")).label).toBe("Unreachable");
  });

  it("says a hibernating tenant is being started, not merely contacted", () => {
    // "Connecting…" for the seconds a cold tenant takes to boot reads as a
    // hang, and the operator's next move is to go looking for a fault that is
    // not there.
    const waking = { ...host("connecting"), waking: true };
    expect(statusCopy(waking).label).toBe("Waking…");
    // Same dot: it is a connecting host, and ranks as one on the trigger.
    expect(statusCopy(waking).dot).toBe(statusCopy(host("connecting")).dot);
  });
});

describe("hostSwitcherInteractive", () => {
  it("has nothing to say on an ordinary single-host browser console", () => {
    // The dot and the standalone chrome still sit this one out: a permanently
    // green dot says nothing, and that console's host being unreachable is a
    // full-screen error rather than a corner to notice.
    expect(hostSwitcherInteractive(1, false)).toBe(false);
  });

  it("becomes a control as soon as there is a choice", () => {
    expect(hostSwitcherInteractive(2, false)).toBe(true);
  });

  it("opens at any count on a hub, which has no bootstrap host to fall back on", () => {
    expect(hostSwitcherInteractive(0, true)).toBe(true);
  });
});

describe("hostSwitcherMenu", () => {
  it("opens on a single-host browser console, which is the only way to manage it", () => {
    // "Manage hosts" lives in this menu and nowhere else, and that console's
    // one host is a plain `remote` connector — renameable, re-addressable and
    // forgettable. A nameplate there is a host nobody can fix.
    expect(hostSwitcherMenu(1, false)).toBe(true);
  });

  it("stays a nameplate only when there is no host at all to manage", () => {
    expect(hostSwitcherMenu(0, false)).toBe(false);
  });

  it("still opens at zero on a hub and wherever the switcher is already a control", () => {
    expect(hostSwitcherMenu(0, true)).toBe(true);
    expect(hostSwitcherMenu(2, false)).toBe(true);
  });
});

describe("hostShortcutLabel", () => {
  it("numbers the first nine hosts from one", () => {
    expect(hostShortcutLabel(0)).toMatch(/1$/);
    expect(hostShortcutLabel(HOST_SHORTCUT_LIMIT - 1)).toMatch(/9$/);
  });

  it("prints nothing past the number row, so no row promises a key that does nothing", () => {
    expect(hostShortcutLabel(HOST_SHORTCUT_LIMIT)).toBeNull();
  });
});

describe("the collapsed rail keeps the lifecycle signal (issue #1931 review)", () => {
  // The collapsed sidebar shrinks `SidebarMenuButton` to the 32px glyph alone
  // (`group-data-[collapsible=icon]:size-8!` in `ui/sidebar.tsx`), clipping the
  // nameplate's two text lines out of the visible box — so without a rescue, a
  // paused, suspended, archived, or emergency-stopped company loses the one
  // place that fact was surfaced. `SidebarMenuButton`'s own `tooltip` prop
  // already exists for this: it renders only while the rail is collapsed, so
  // wiring the switcher's lifecycle-aware tooltip through it keeps the signal
  // reachable on hover instead of losing it outright.
  //
  // A jsdom render of the full switcher needs `HostsContext`, which this file
  // does not stand up; the source-contract idiom (`responsive-two-rail-band
  // .test.ts` uses the same one for a media-query fact jsdom cannot evaluate)
  // pins the wiring instead.
  const here = dirname(fileURLToPath(import.meta.url));
  const source = readFileSync(resolve(here, "../../src/components/host-switcher.tsx"), "utf8");

  it("derives the collapsed-rail tooltip from the same lifecycle line as the nameplate", () => {
    expect(source).toContain(
      'const switcherTooltip = lifecycleLine ? `${primary} — ${lifecycleLine}` : primary;',
    );
  });

  it("wires the tooltip onto both the plain nameplate and the dropdown trigger", () => {
    // The plain (non-menu) trigger passes it directly; the menu trigger goes
    // through `SidebarHeaderTrigger`, which takes it as its own `tooltip` prop
    // (split out because `useSidebar` throws outside a `SidebarProvider`) and
    // forwards it onto its `SidebarMenuButton`.
    expect(source).toContain("tooltip={switcherTooltip}");
    expect(source).toContain("tooltip={tooltip}");
    expect(source).toMatch(/tooltip: string;/);
  });
});
