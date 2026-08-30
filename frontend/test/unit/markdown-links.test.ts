import { describe, expect, it } from "vitest";

import { isExternalHref } from "@/components/markdown";

/**
 * Which links leave the console, and which stay in it.
 *
 * `components/markdown.tsx` is one renderer behind chat, memory, workspace and
 * workflows. An external link that takes over the tab costs the operator the
 * view they were reading — and in chat, the scroll position in a long thread.
 * An *internal* link that opens a tab is the same mistake in the other
 * direction, which is why this is a decision and not a blanket `target`.
 */
describe("isExternalHref", () => {
  it("treats absolute web URLs as external", () => {
    expect(isExternalHref("https://example.com/docs")).toBe(true);
    expect(isExternalHref("http://example.com")).toBe(true);
    expect(isExternalHref("HTTPS://EXAMPLE.COM")).toBe(true);
    // Protocol-relative: the browser resolves it to http(s), so it leaves too.
    expect(isExternalHref("//example.com/asset.png")).toBe(true);
    expect(isExternalHref("  https://example.com  ")).toBe(true);
  });

  it("keeps in-app navigation in the same tab", () => {
    expect(isExternalHref("#/chat/ceo")).toBe(false);
    expect(isExternalHref("/workspace")).toBe(false);
    expect(isExternalHref("./note.md")).toBe(false);
    expect(isExternalHref("note.md")).toBe(false);
  });

  it("leaves non-web schemes alone", () => {
    // A new tab for a mail client is a blank tab left behind.
    expect(isExternalHref("mailto:team@example.com")).toBe(false);
    expect(isExternalHref("tel:+3110000000")).toBe(false);
  });

  it("is safe on a link the markdown left without a target", () => {
    expect(isExternalHref(undefined)).toBe(false);
    expect(isExternalHref("")).toBe(false);
  });
});
