import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

/**
 * The 768–1023px two-rail band (issue #1383).
 *
 * The app sidebar comes on at `md` (≥768). Chat's channel rail and Settings'
 * sub-rail used to come on *earlier or at the same* breakpoint, so from
 * 768–1023px the window carried two rails plus content and the working pane
 * collapsed to ~290px. In Chat that stranded the composer's Send button off
 * the right edge with no scroll to reach it; in Settings it clipped the SMTP
 * card on both sides. The fix pushes both second rails to `lg` (≥1024) — so
 * 768–1023 is single-rail — and lets the composer's action row wrap so Send
 * can never leave the flow.
 *
 * A jsdom render cannot prove this: the whole failure is a media query, and
 * jsdom does not evaluate them. So this guards the *class contract* the fix
 * rests on — the same source-contract idiom as `shell-chrome-tokens.test.ts`.
 * The pixel-accurate proof (Send clickable, SMTP legible across 768–1024px)
 * is a manual/e2e concern noted in the PR.
 */

const here = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(here, "../../src", rel), "utf8");

describe("chat rails collapse to single-rail below lg (issue #1383)", () => {
  const chatView = read("views/ChatView.tsx");
  const chatHeader = read("views/chat/ChatHeader.tsx");

  it("shows the full channel rail below lg only when toggled", () => {
    // The mobile rail stays full-width and is governed by the shared-pane toggle.
    expect(chatView).toContain('cn("lg:hidden", mobilePane === "rail" ? "flex" : "hidden")');
    // Regression guard: the `md` rail is what produced the two-rail band.
    expect(chatView).not.toContain('cn("md:hidden", mobilePane === "rail" ? "flex" : "hidden")');
  });

  it("keeps a separate desktop rail from lg onward", () => {
    // A desktop-local compact rail must not affect the 768–1023px mobile-pane flow.
    expect(chatView).toContain('className="hidden lg:flex"');
    expect(chatView).not.toContain('className="hidden md:flex"');
  });

  it("shows the chat pane full-width below lg (single pane), split at lg", () => {
    expect(chatView).toContain('mobilePane === "chat" ? "flex" : "hidden lg:flex"');
    expect(chatView).not.toContain('mobilePane === "chat" ? "flex" : "hidden md:flex"');
  });

  it("keeps the header's rail toggle visible across the single-rail band", () => {
    // The "Show channels" affordance must reach up to lg, matching the rail.
    expect(chatHeader).toContain("size-8 lg:hidden");
    expect(chatHeader).not.toContain("size-8 md:hidden");
  });

  it("offers the desktop channel collapse separately from the mobile rail toggle", () => {
    expect(chatHeader).toContain('className="hidden size-8 lg:inline-flex"');
    expect(chatHeader).toContain('aria-label={channelsCollapsed ? "Expand channels" : "Collapse channels"}');
  });
});

describe("settings sub-rail collapses to chips below lg (issue #1383)", () => {
  const settings = read("views/SettingsSection.tsx");

  it("shows the sub-rail only from lg", () => {
    expect(settings).toContain(
      "hidden w-60 shrink-0 flex-col gap-0.5 overflow-y-auto border-r p-3 lg:flex",
    );
    // Regression guard: the `sm` rail overlapped the app sidebar at 768–1023.
    expect(settings).not.toContain(
      "hidden w-60 shrink-0 flex-col gap-0.5 overflow-y-auto border-r p-3 sm:flex",
    );
  });

  it("shows the chip-row fallback below lg, so the pane gets full width", () => {
    expect(settings).toContain("border-b lg:hidden");
    expect(settings).toContain("flex gap-1 overflow-x-auto p-2");
    expect(settings).not.toContain("border-b sm:hidden");
  });

  /**
   * Codex review on PR #1931: `ContentSurface` overlays every page's top 28px
   * with an absolutely-positioned, pointer-events-enabled drag band
   * (`WindowDragBar`, z-20) on the macOS desktop so the window stays movable
   * without a native title bar. This chip row is the one place in the console
   * that puts real, clickable navigation into that exact strip below `lg` —
   * so without a higher stacking order than the drag band, its links are
   * unreachable at 880–1023px window widths on macOS.
   *
   * jsdom cannot evaluate the actual overlap (that is the drag band's own
   * media query and stacking order in a real compositor), so this pins the
   * source contract the fix rests on: the chip row's wrapper carries its own
   * `relative z-30` stacking context, above the drag band's `z-20`.
   */
  it("keeps the chip row above the macOS drag band (z-30 over the band's z-20)", () => {
    const idx = settings.indexOf('border-b lg:hidden');
    expect(idx).toBeGreaterThan(-1);
    const wrapper = settings.slice(Math.max(0, idx - 60), idx);
    expect(wrapper).toContain("relative z-30");
  });
});

describe("composer keeps Send in-flow in a narrow pane (issue #1383)", () => {
  const composer = read("views/chat/MessageComposer.tsx");

  it("lets the action row wrap instead of overflowing", () => {
    expect(composer).toContain('className="flex flex-wrap items-center gap-0.5 px-2 pb-1.5"');
    // Regression guard: the non-wrapping row pushed Send off-screen.
    expect(composer).not.toContain('className="flex items-center gap-0.5 px-2 pb-1.5"');
  });

  it("keeps Send in normal flow (ml-auto), never absolutely positioned", () => {
    // Anchor on the Send button and read the className just above its
    // `aria-label`: it must right-align with `ml-auto` and carry no out-of-flow
    // escape. If Send were pulled from the flow it could clip again exactly the
    // way #1383 describes.
    const idx = composer.indexOf('aria-label="Send"');
    expect(idx).toBeGreaterThan(-1);
    const sendButton = composer.slice(Math.max(0, idx - 300), idx);
    expect(sendButton).toContain("ml-auto");
    expect(sendButton).not.toMatch(/\babsolute\b|\bfixed\b/);
  });
});

describe("mention clearing is gated on the transcript being visible (codex P1)", () => {
  const chatView = read("views/ChatView.tsx");

  it("only reports a channel viewed while the chat pane is actually on screen", () => {
    // The view-report effect that clears mentions must not fire while a sub-`lg`
    // pane shows only the rail — a mention landing then would be marked read
    // behind the operator's back. A jsdom render cannot prove it (the whole
    // failure is the `lg` media query), so this pins the gate to the same
    // `mobilePane` toggle and `lg` breakpoint the pane's class contract above
    // uses: a future move of the rail off `lg` trips that class test too.
    expect(chatView).toMatch(/if \(channel && chatPaneVisible\)/);
    expect(chatView).toContain(
      'const chatPaneVisible = mobilePane === "chat" || isDesktop;',
    );
    // The visibility flag is a dependency, so re-opening the pane from the rail
    // re-runs the report and clears whatever is newly visible.
    expect(chatView).toContain("chatPaneVisible,\n  ]);");
  });
});
