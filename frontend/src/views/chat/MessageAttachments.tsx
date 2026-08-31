import { useEffect, useRef, useState } from "react";
import { Download, FileText, Loader2 } from "lucide-react";

import { formatBytes } from "@/api/workspace";
import type { AttachmentDto } from "@/api/types";
import { cn } from "@/lib/utils";

interface Props {
  attachments: AttachmentDto[];
  /**
   * Resolves a stored attachment's bytes to an object URL (issue #1682). The
   * blob route needs the client's bearer, which no `<img>` or bare link can
   * carry, so both the inline preview and the download go through this. Absent
   * when the surface cannot reach the client — the chips then render without a
   * working download rather than crashing.
   *
   * An optional `signal` cancels the transfer: the preview aborts its fetch
   * when the image scrolls out of view, instead of letting a multi-megabyte
   * download run to completion only to be discarded (codex review finding).
   */
  resolveUrl?: (nodeId: string, signal?: AbortSignal) => Promise<string>;
}

/** Whether a stored mime renders as an inline image preview (issue #1682).
 *
 * `image/*` minus SVG: an SVG is an XML document whose `<script>` executes, so
 * the blob route already serves it as an attachment (issue #667) and v1 keeps
 * it download-only rather than inlining it. Every other `image/*` is decoded to
 * pixels with no script context and is safe to preview. */
function isPreviewableImage(mime: string): boolean {
  return mime.startsWith("image/") && mime !== "image/svg+xml";
}

/**
 * The attached files on one message (issue #1682).
 *
 * v1 renders a download chip per file — filename, size, and a click that
 * fetches the bytes through the authenticated blob route and hands them to the
 * browser — plus an inline `<img>` preview for a non-SVG image. Multi-file,
 * drag-drop and rich previews (PDF/video) are deliberately out of scope; the
 * shape already loops so more chips need no change here.
 */
export function MessageAttachments({ attachments, resolveUrl }: Props) {
  if (attachments.length === 0) return null;
  return (
    <div className="mt-1.5 flex flex-col gap-1.5">
      {attachments.map((attachment) => (
        <AttachmentItem key={attachment.nodeId} attachment={attachment} resolveUrl={resolveUrl} />
      ))}
    </div>
  );
}

function AttachmentItem({
  attachment,
  resolveUrl,
}: {
  attachment: AttachmentDto;
  resolveUrl?: (nodeId: string, signal?: AbortSignal) => Promise<string>;
}) {
  const image = isPreviewableImage(attachment.mime);
  const [previewUrl, setPreviewUrl] = useState<string>();
  const [downloading, setDownloading] = useState(false);
  const [downloadError, setDownloadError] = useState<string>();
  const container = useRef<HTMLDivElement>(null);
  // Whether this chip has scrolled near the viewport — an image preview only
  // fetches once this flips true (codex review finding). History can
  // materialize hundreds of messages at once, and each workspace blob can run
  // tens of megabytes, so fetching every attachment's bytes the instant it
  // mounts — most of them off-screen — could pull down gigabytes for one
  // channel open.
  const [inView, setInView] = useState(false);

  // Arms `inView` once an image chip nears the viewport, and disarms it when
  // the chip scrolls back out (codex review finding): visibility is two-way,
  // not a latch, so the preview's object URL is revoked on exit and refetched
  // on re-entry. Without the exit half, an image-heavy history could
  // accumulate every blob it ever scrolled past — rows stay mounted, and a
  // one-way latch means the full multi-megabyte attachment set for the whole
  // visible span stays resident. Without an IntersectionObserver (a test
  // environment, an old embedded webview) this falls back to eager — the
  // pre-existing behavior — rather than a preview that can never load.
  useEffect(() => {
    if (!image) return;
    const el = container.current;
    if (!el || typeof IntersectionObserver === "undefined") {
      setInView(true);
      return;
    }
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) {
            setInView(true);
          } else {
            // Drop the fetched URL while the chip is off-screen so the
            // `<img>` does not keep pointing at a blob the preview effect's
            // cleanup is about to revoke.
            setInView(false);
            setPreviewUrl(undefined);
          }
        }
      },
      // A little ahead of the viewport, so a fast scroll finds the preview
      // already loading rather than popping in after the fact.
      { rootMargin: "200px" },
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, [image]);

  // Fetches the bytes for an inline image once it is in view, and revokes the
  // object URL on unmount so it does not stay resident for the life of the
  // document. Scrolling the chip back out mid-fetch aborts the download — the
  // whole payload must not transfer to the last byte for a preview nothing
  // will show, and the next re-entry must not overlap it with a second
  // concurrent request for the same node (codex review finding).
  useEffect(() => {
    if (!image || !inView || !resolveUrl) return;
    const controller = new AbortController();
    let url: string | undefined;
    let alive = true;
    resolveUrl(attachment.nodeId, controller.signal)
      .then((got) => {
        if (alive) {
          url = got;
          setPreviewUrl(got);
        } else {
          URL.revokeObjectURL(got);
        }
      })
      .catch(() => {
        // A preview is a nicety the download chip below does not depend on
        // (codex review finding — a rejection here must not go unhandled),
        // so a failed fetch stays silent here rather than duplicating the
        // download button's own error state for a fetch the operator never
        // asked for directly. An abort — the chip scrolled away mid-fetch —
        // lands in the same place and is equally silent.
      });
    return () => {
      alive = false;
      controller.abort();
      if (url) URL.revokeObjectURL(url);
    };
  }, [image, inView, resolveUrl, attachment.nodeId]);

  async function download() {
    if (!resolveUrl || downloading) return;
    setDownloading(true);
    setDownloadError(undefined);
    try {
      const url = await resolveUrl(attachment.nodeId);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = attachment.name;
      document.body.appendChild(anchor);
      anchor.click();
      anchor.remove();
      // Revoke after the click has been handed off, not synchronously — the
      // browser needs the URL to still resolve when it starts the download.
      setTimeout(() => URL.revokeObjectURL(url), 10_000);
    } catch (err) {
      // Codex review finding: an unhandled rejection here left the button
      // looking like it simply did nothing — the node was deleted, the
      // session expired, or the blob request otherwise failed, and the
      // operator had no way to tell.
      setDownloadError(
        err instanceof Error ? err.message : "Couldn't download this file.",
      );
    } finally {
      setDownloading(false);
    }
  }

  return (
    <div ref={container} className="flex w-fit max-w-full flex-col gap-1.5">
      {image && previewUrl && (
        <img
          src={previewUrl}
          alt={attachment.name}
          className="max-h-64 max-w-full rounded-md border object-contain"
        />
      )}
      <button
        type="button"
        onClick={() => void download()}
        disabled={!resolveUrl || downloading}
        className={cn(
          "flex w-fit max-w-full items-center gap-2 rounded-md border bg-card px-2.5 py-1.5",
          "text-left text-xs transition-colors hover:bg-accent disabled:opacity-60",
        )}
        title={`Download ${attachment.name}`}
      >
        {downloading ? (
          <Loader2 className="size-4 shrink-0 animate-spin text-muted-foreground" aria-hidden />
        ) : (
          <FileText className="size-4 shrink-0 text-muted-foreground" aria-hidden />
        )}
        <span className="min-w-0 truncate font-medium">{attachment.name}</span>
        <span className="shrink-0 text-2xs text-muted-foreground">
          {formatBytes(attachment.size)}
        </span>
        <Download className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
      </button>
      {downloadError && (
        <p role="alert" className="text-2xs text-destructive">
          {downloadError}
        </p>
      )}
    </div>
  );
}
