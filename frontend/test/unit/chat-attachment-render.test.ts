// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { AttachmentDto } from "@/api/types";
import { MessageAttachments } from "@/views/chat/MessageAttachments";

/**
 * Issue #1682: how an attachment renders in the transcript. v1 is a download
 * chip for every file and an inline preview for a non-SVG image. SVG is
 * download-only — it is an XML document whose script would execute, so the blob
 * route serves it as an attachment and the console never inlines it.
 */

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  // jsdom does not implement the object-URL lifecycle the component revokes;
  // patch just the two static methods, leaving the `URL` constructor intact.
  URL.createObjectURL = vi.fn(() => "blob:mock");
  URL.revokeObjectURL = vi.fn();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

async function render(attachments: AttachmentDto[], resolveUrl?: (id: string) => Promise<string>) {
  await act(async () => {
    root.render(createElement(MessageAttachments, { attachments, resolveUrl }));
  });
  // Flush the preview-fetch effect's microtasks.
  await act(async () => {
    await Promise.resolve();
  });
}

const png: AttachmentDto = { nodeId: "n1", name: "chart.png", mime: "image/png", size: 4096 };
const svg: AttachmentDto = { nodeId: "n2", name: "logo.svg", mime: "image/svg+xml", size: 512 };
const pdf: AttachmentDto = { nodeId: "n3", name: "report.pdf", mime: "application/pdf", size: 8192 };

describe("MessageAttachments (issue #1682)", () => {
  it("renders a download chip with the file's name and size", async () => {
    await render([pdf]);
    expect(container.textContent).toContain("report.pdf");
    expect(container.textContent).toContain("8.0 KB");
    expect(container.querySelector('[title="Download report.pdf"]')).not.toBeNull();
  });

  it("previews a non-SVG image inline, fetched through the resolver", async () => {
    const resolveUrl = vi.fn(async () => "blob:the-image");
    await render([png], resolveUrl);
    expect(resolveUrl).toHaveBeenCalledWith("n1", expect.any(AbortSignal));
    const img = container.querySelector("img");
    expect(img).not.toBeNull();
    expect(img!.getAttribute("src")).toBe("blob:the-image");
  });

  it("never inlines an SVG — download-only", async () => {
    const resolveUrl = vi.fn(async () => "blob:the-svg");
    await render([svg], resolveUrl);
    // The chip is there; the inline preview is not.
    expect(container.textContent).toContain("logo.svg");
    expect(container.querySelector("img")).toBeNull();
  });

  it("clicking the chip resolves the bytes for download", async () => {
    const resolveUrl = vi.fn(async () => "blob:the-pdf");
    await render([pdf], resolveUrl);
    await act(async () => {
      (container.querySelector('[title="Download report.pdf"]') as HTMLButtonElement).click();
    });
    expect(resolveUrl).toHaveBeenCalledWith("n3");
  });

  // Codex review finding: a failed `resolveUrl` used to leave the download
  // button looking like it simply did nothing — an unhandled rejection with
  // no feedback to the operator.
  it("reports a failed download instead of leaving the button silent", async () => {
    const resolveUrl = vi.fn(async () => {
      throw new Error("node not found");
    });
    await render([pdf], resolveUrl);
    await act(async () => {
      (container.querySelector('[title="Download report.pdf"]') as HTMLButtonElement).click();
    });
    await act(async () => {
      await Promise.resolve();
    });

    expect(container.querySelector('[role="alert"]')?.textContent).toBe("node not found");
  });

  // Codex review finding: the preview fetch had no rejection handling either
  // — this pins that a failed preview does not throw an unhandled rejection
  // or crash the row; the download chip is still there as the fallback.
  it("does not crash when the preview fetch fails", async () => {
    const resolveUrl = vi.fn(async () => {
      throw new Error("gone");
    });
    await render([png], resolveUrl);

    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector('[title="Download chart.png"]')).not.toBeNull();
  });

  describe("lazy preview loading (codex review finding)", () => {
    let observe: ReturnType<typeof vi.fn>;
    let disconnect: ReturnType<typeof vi.fn>;
    let intersectCallback: (entries: { isIntersecting: boolean }[]) => void;
    let originalIO: typeof IntersectionObserver | undefined;

    beforeEach(() => {
      originalIO = globalThis.IntersectionObserver;
      observe = vi.fn();
      disconnect = vi.fn();
      class FakeIntersectionObserver {
        constructor(cb: (entries: { isIntersecting: boolean }[]) => void) {
          intersectCallback = cb;
        }
        observe = observe;
        disconnect = disconnect;
        unobserve = vi.fn();
      }
      // @ts-expect-error -- a minimal stand-in, not the full DOM interface
      globalThis.IntersectionObserver = FakeIntersectionObserver;
    });

    afterEach(() => {
      globalThis.IntersectionObserver = originalIO as typeof IntersectionObserver;
    });

    it("does not fetch an image preview until it scrolls into view", async () => {
      const resolveUrl = vi.fn(async () => "blob:the-image");
      await render([png], resolveUrl);

      expect(observe).toHaveBeenCalledOnce();
      expect(resolveUrl).not.toHaveBeenCalled();

      await act(async () => {
        intersectCallback([{ isIntersecting: true }]);
        await Promise.resolve();
      });

      expect(resolveUrl).toHaveBeenCalledWith("n1", expect.any(AbortSignal));
      expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:the-image");
    });

    it("never observes a non-image attachment — nothing to defer", async () => {
      await render([pdf]);
      expect(observe).not.toHaveBeenCalled();
    });

    // Codex review finding: leaving the viewport mid-fetch used to only mark
    // the request dead — the download still ran to completion, and a quick
    // re-enter started a second full transfer of the same (potentially 64 MiB)
    // payload, so rapid scrolling could keep several concurrent downloads of
    // one attachment alive with all but the newest discarded. The exit must
    // cancel the in-flight fetch through the signal.
    it("cancels an in-flight preview fetch when the image leaves the viewport", async () => {
      const settle = { aborted: false };
      const resolveUrl = vi.fn((_id: string, signal?: AbortSignal) => {
        return new Promise<string>((_resolve, reject) => {
          signal?.addEventListener("abort", () => {
            settle.aborted = true;
            reject(new DOMException("aborted", "AbortError"));
          });
        });
      });
      await render([png], resolveUrl);

      await act(async () => {
        intersectCallback([{ isIntersecting: true }]);
      });
      // The preview fetch carries the signal it can be cancelled with.
      expect(resolveUrl).toHaveBeenCalledWith("n1", expect.any(AbortSignal));

      // Scrolls back out while the fetch is still pending: the download is
      // cancelled rather than left to transfer bytes no preview will show.
      await act(async () => {
        intersectCallback([{ isIntersecting: false }]);
        await Promise.resolve();
      });
      expect(settle.aborted).toBe(true);

      // Re-entry fetches again — but only after the previous download was
      // cancelled, so the two never run concurrently.
      await act(async () => {
        intersectCallback([{ isIntersecting: true }]);
        await Promise.resolve();
      });
      expect(resolveUrl).toHaveBeenCalledTimes(2);
    });
  });
});
