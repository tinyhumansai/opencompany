import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

/**
 * A completed background task must not have its toast suppressed by a
 * transcript the operator cannot actually see (#1768 codex review).
 *
 * Below `lg`, selecting the channel rail hides `ChatView`'s transcript
 * (`chatPaneVisible === false`) while leaving it mounted — but
 * `activeChatChannelRef` in the shell only updates from `onChannelViewed`,
 * which stops firing the moment the pane hides. So the ref keeps naming
 * whichever channel was on screen right before the rail was opened, and a
 * `desk_task_completed` event from that channel makes `isViewingTaskOrigin`
 * report "still watching it" even though the inline marker it is deferring
 * to is off screen. The operator gets no toast and no visible marker.
 *
 * A jsdom render of `app-shell` cannot prove this — it needs the whole
 * client and every hook, the same reason `chat-rail-focus.test.ts` and
 * `responsive-two-rail-band.test.ts` fall back to a source-contract check.
 * This guards the wiring the fix rests on: ChatView reports pane visibility
 * on its own channel, separate from `onChannelViewed`'s channel-identity
 * report, and the shell's origin check consults it before trusting the
 * remembered channel id.
 */

const here = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(here, "../../src", rel), "utf8");

describe("a hidden mobile chat pane cannot suppress a completion toast (#1768)", () => {
  const chatView = read("views/ChatView.tsx");
  const appShell = read("components/app-shell.tsx");

  it("ChatView reports chatPaneVisible on its own dedicated channel", () => {
    // Not folded into `onChannelViewed`, which only fires — and only ever
    // fired — while the pane is visible, so it cannot report the hide edge.
    expect(chatView).toContain("onChatPaneVisibilityChange?.(chatPaneVisible);");
  });

  it("the shell tracks pane visibility separately from the remembered channel", () => {
    // Distinct from `activeChatChannelRef`: that ref has a second job
    // (addressing an unaddressed system line after a walk to Approvals) that
    // must keep using the last-known channel even while the rail is showing,
    // so visibility cannot be folded into clearing it.
    expect(appShell).toContain("const chatPaneVisibleRef = useRef(true);");
    expect(appShell).toContain(
      "const onChatPaneVisibilityChange = useCallback((visible: boolean) => {\n    chatPaneVisibleRef.current = visible;\n  }, []);",
    );
  });

  it("wires the shell's tracker to ChatView's report", () => {
    expect(appShell).toContain("onChatPaneVisibilityChange={onChatPaneVisibilityChange}");
  });

  it("isViewingTaskOrigin refuses to claim visibility while the pane is hidden", () => {
    const idx = appShell.indexOf("isViewingTaskOrigin: useCallback(");
    expect(idx).toBeGreaterThan(-1);
    const body = appShell.slice(idx, idx + 900);
    expect(body).toContain('if (!chatPaneVisibleRef.current) return false;');
    // The visibility check must come before the origin channel is trusted.
    expect(body.indexOf("chatPaneVisibleRef.current")).toBeLessThan(
      body.indexOf("activeChatChannelRef.current === origin"),
    );
  });
});
