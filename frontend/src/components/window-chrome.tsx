// The desktop window's own chrome: what replaces the native title bar once the
// window stops drawing one.
//
// `src-tauri/tauri.conf.json` runs the main window with `titleBarStyle:
// "Overlay"` and `hiddenTitle: true`, which is macOS's transparent title bar:
// the bar itself is gone, the traffic lights float over the web content, and
// the window keeps its rounded corners and its resize edges. Two things stop
// working the moment that happens, and this file is both of them.
//
// **Dragging.** With no title bar there is nothing to grab. macOS does not make
// the top of the content draggable on its own — the webview captures the
// pointer — so a band has to opt back in with `data-tauri-drag-region`. See
// {@link WindowDragBar}.
//
// **The lights' backdrop.** They are drawn over whatever is at the window's
// top-left, which in this console is the sidebar's company switcher. A control
// under three floating circles is a control you cannot click. See
// {@link WindowControlsInset}, which reserves the strip they land in.
//
// Both are macOS-and-desktop only. Windows and Linux keep their native
// decorated title bar (`Overlay` is a no-op there), so reserving a band would
// waste vertical space, and in a browser there is no window to drag at all.
// This mirrors `WindowDragBar.tsx` and `AppSidebar.tsx` in the vendored
// OpenHuman checkout, which solved the same two problems for the same reason.

import { isDesktopRuntime } from "@/api/transport";

/**
 * Height of the reserved strip, in px, and the height of the drag band.
 *
 * It is the macOS traffic-light zone: 28px clears the three buttons at their
 * standard size with a hair of margin. `trafficLightPosition.y` in
 * `tauri.conf.json` is tuned against this number — see the note there — so the
 * two move together or the lights sit off-centre in their own strip.
 */
export const WINDOW_CHROME_HEIGHT = 28;

/**
 * Whether this build is drawing its own window chrome.
 *
 * Deliberately a runtime check rather than a build-time one: the same bundle is
 * served by `opencompany serve` to a browser and loaded by the Tauri shell, so
 * there is no compile step that could tell them apart. `navigator.platform` is
 * deprecated but is what a webview still answers reliably for the OS; the Tauri
 * check is the load-bearing half, and a non-mac desktop simply keeps its native
 * title bar.
 */
export function usesOverlayTitleBar(): boolean {
  if (!isDesktopRuntime()) return false;
  if (typeof navigator === "undefined") return false;
  const platform =
    (navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData?.platform ??
    navigator.platform ??
    "";
  return /mac/i.test(platform);
}

/**
 * The transparent band that makes the top of the window draggable again.
 *
 * Absolutely positioned over the top of whatever it is placed in, so it
 * reserves no vertical space and adds no inherited inset to the page below it.
 * `aria-hidden` because it is window chrome: there is nothing here for a screen
 * reader, and a landmark-free empty div would otherwise be announced as one
 * more thing to skip.
 *
 * `pointer-events-none` is deliberately NOT set. The band has to receive the
 * press for macOS to start a drag, which means it also swallows clicks in the
 * strip it covers — that is why it is only ever placed over space nothing else
 * is using.
 */
export function WindowDragBar({ className }: { className?: string }) {
  if (!usesOverlayTitleBar()) return null;
  return (
    <div
      data-tauri-drag-region
      data-testid="window-drag-bar"
      aria-hidden="true"
      className={`absolute inset-x-0 top-0 z-20 ${className ?? ""}`}
      style={{ height: WINDOW_CHROME_HEIGHT }}
    />
  );
}

/**
 * The vertical space the traffic lights sit in, at the top of the sidebar.
 *
 * OpenHuman's expanded sidebar dodges the lights by right-aligning its header
 * icons, leaving the top-left empty. This console cannot: the top-left is the
 * company switcher, a 48px nameplate that names where you are and is the first
 * thing the column should answer. So the strip is reserved instead and the
 * header starts below it — which is exactly what OpenHuman's own *collapsed*
 * rail does for the same reason, its narrow column having no empty left space
 * to give away either.
 *
 * It is draggable as well as reserved. The alternative is a 28px band of dead
 * window above a control an operator will try to grab, and a title bar you
 * cannot drag is stranger than no title bar at all.
 */
export function WindowControlsInset() {
  if (!usesOverlayTitleBar()) return null;
  return (
    <div
      data-tauri-drag-region
      data-testid="window-controls-inset"
      aria-hidden="true"
      className="w-full flex-none"
      style={{ height: WINDOW_CHROME_HEIGHT }}
    />
  );
}
