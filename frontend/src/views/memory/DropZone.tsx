/**
 * The Brain page's drop target: files, whole folders, and links.
 *
 * ## Folders are the reason this is not an `<input type="file">`
 *
 * A file input can be told to accept a directory, but a *drag* of one only
 * arrives as a traversable tree through `DataTransferItem.webkitGetAsEntry`,
 * and that tree is what the feature is for — dropping `Contracts/` and having
 * the company remember all of it. So the drop path walks entries recursively
 * and keeps each file's path relative to the dropped folder, which is what the
 * host stores as the document's name.
 *
 * A picker is still offered beside it, because a drop is undiscoverable and
 * unavailable to anyone driving the console from a keyboard.
 *
 * ## Links are a drop too
 *
 * Dragging a link or a tab onto the page hands over `text/uri-list`, so the
 * same gesture that remembers a file remembers a page. The host fetches it —
 * the browser cannot, cross-origin — which is why the URL path is guarded
 * server-side.
 */

import { useCallback, useRef, useState } from "react";
import { FileUp, Link2, Loader2, Upload } from "lucide-react";
import { toast } from "sonner";

import {
  ingestDocuments,
  ingestLinks,
  type DroppedFile,
  type Ingested,
  type IngestedItem,
} from "@/api/memory";
import type { OpenCompanyClient } from "@/api/client";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** Called after a drop lands, so the page can re-read memory. */
  onIngested: () => void;
  /** Whether the bound engine keeps anything; a null engine takes no drops. */
  discarding: boolean;
}

/**
 * How many files go in one request.
 *
 * A folder can be thousands of files, and one request carrying all of them
 * would hit the host's body limit and lose the whole drop. Batching also gives
 * the operator moving progress on a big folder instead of one long stall.
 */
const BATCH = 20;

/** Files a folder drop should never carry into memory. */
const IGNORED = new Set([".DS_Store", "Thumbs.db", ".gitkeep"]);

/**
 * Whether to skip a path outright, before it costs an upload.
 *
 * Exported for its own tests: this is the rule that decides what a dropped
 * folder does *not* put in a company's memory, and getting it wrong is either
 * a repository's `.git` objects uploaded one refusal at a time or a real
 * document silently skipped.
 */
export function ignored(path: string): boolean {
  const name = path.split("/").pop() ?? path;
  if (IGNORED.has(name)) return true;
  // Version-control and dependency directories: dropping a repository folder
  // is a natural thing to try, and `.git/objects` is megabytes of binary that
  // extraction would refuse one file at a time.
  return /(^|\/)(\.git|node_modules|target|dist|\.next|__pycache__)(\/|$)/.test(path);
}

/** Recursively collects every file under one dropped entry. */
async function walk(entry: FileSystemEntry, prefix: string): Promise<DroppedFile[]> {
  if (entry.isFile) {
    const file = await new Promise<File | null>((resolve) => {
      (entry as FileSystemFileEntry).file(resolve, () => resolve(null));
    });
    if (!file) return [];
    const path = prefix ? `${prefix}/${entry.name}` : entry.name;
    return ignored(path) ? [] : [{ path, file }];
  }
  if (!entry.isDirectory) return [];
  const reader = (entry as FileSystemDirectoryEntry).createReader();
  const children: FileSystemEntry[] = [];
  // `readEntries` returns at most 100 at a time and signals the end with an
  // empty batch — reading once silently truncates any folder past 100 files,
  // which is exactly the size where a folder drop starts to matter.
  for (;;) {
    const batch = await new Promise<FileSystemEntry[]>((resolve) => {
      reader.readEntries(resolve, () => resolve([]));
    });
    if (batch.length === 0) break;
    children.push(...batch);
  }
  const path = prefix ? `${prefix}/${entry.name}` : entry.name;
  if (ignored(`${path}/`)) return [];
  const nested = await Promise.all(children.map((child) => walk(child, path)));
  return nested.flat();
}

/** Pulls files and links out of a drop. */
async function readDrop(transfer: DataTransfer): Promise<{ files: DroppedFile[]; urls: string[] }> {
  const entries: FileSystemEntry[] = [];
  const urls: string[] = [];
  // `items` is live and empties as soon as the handler yields, so every entry
  // has to be taken out of it synchronously, before the first `await`.
  for (const item of Array.from(transfer.items)) {
    if (item.kind === "file") {
      const entry = item.webkitGetAsEntry?.();
      if (entry) entries.push(entry);
    }
  }
  const list = transfer.getData("text/uri-list") || transfer.getData("text/plain");
  for (const line of list.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (/^https?:\/\//i.test(trimmed)) urls.push(trimmed);
  }

  if (entries.length > 0) {
    const walked = await Promise.all(entries.map((entry) => walk(entry, "")));
    return { files: walked.flat(), urls };
  }
  // A browser with no entry API still gives plain files, without their paths.
  const files = Array.from(transfer.files)
    .map((file) => ({ path: file.name, file }))
    .filter((f) => !ignored(f.path));
  return { files, urls };
}

export function DropZone({ client, company, onIngested, discarding }: Props) {
  const [over, setOver] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [report, setReport] = useState<IngestedItem[] | null>(null);
  const picker = useRef<HTMLInputElement>(null);
  // Drag events fire for every child element, so a plain boolean flickers the
  // highlight off as the pointer crosses the inner text. Counting enter/leave
  // pairs is what keeps it steady.
  const depth = useRef(0);

  const send = useCallback(
    async (files: DroppedFile[], urls: string[]) => {
      if (files.length === 0 && urls.length === 0) {
        toast.error("nothing in that drop could be read");
        return;
      }
      const items: IngestedItem[] = [];
      try {
        for (let i = 0; i < files.length; i += BATCH) {
          const batch = files.slice(i, i + BATCH);
          setBusy(
            files.length > BATCH
              ? `Reading ${Math.min(i + BATCH, files.length)} of ${files.length} files…`
              : `Reading ${files.length} file${files.length === 1 ? "" : "s"}…`,
          );
          const result: Ingested = await ingestDocuments(client, company, batch);
          items.push(...result.items);
        }
        if (urls.length > 0) {
          setBusy(`Fetching ${urls.length} link${urls.length === 1 ? "" : "s"}…`);
          const result = await ingestLinks(client, company, urls);
          items.push(...result.items);
        }
      } catch (e) {
        toast.error(e instanceof Error ? e.message : "the drop could not be read");
      } finally {
        setBusy(null);
      }
      setReport(items);
      const stored = items.filter((i) => i.status === "stored").length;
      if (stored > 0) {
        toast.success(`Remembered ${stored} of ${items.length}.`);
        onIngested();
      } else if (items.length > 0) {
        toast.error("nothing in that drop could be turned into memory");
      }
    },
    [client, company, onIngested],
  );

  return (
    <Card
      onDragEnter={(e) => {
        e.preventDefault();
        depth.current += 1;
        setOver(true);
      }}
      onDragOver={(e) => e.preventDefault()}
      onDragLeave={() => {
        depth.current = Math.max(0, depth.current - 1);
        if (depth.current === 0) setOver(false);
      }}
      onDrop={(e) => {
        e.preventDefault();
        depth.current = 0;
        setOver(false);
        if (discarding) {
          toast.error("this engine discards every write — nothing dropped here would be kept");
          return;
        }
        const transfer = e.dataTransfer;
        void readDrop(transfer).then(({ files, urls }) => send(files, urls));
      }}
      className={cn(
        "border-dashed transition-colors",
        over && "border-primary bg-primary/5",
        discarding && "opacity-60",
      )}
      data-testid="memory-dropzone"
    >
      <CardContent className="space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex items-center gap-3">
            {busy ? (
              <Loader2 className="size-5 shrink-0 animate-spin text-muted-foreground" />
            ) : (
              <Upload className="size-5 shrink-0 text-muted-foreground" />
            )}
            <div className="space-y-0.5">
              <p className="text-sm font-medium">
                {busy ?? "Drop files, folders or links here to remember them"}
              </p>
              <p className="text-xs text-muted-foreground">
                PDFs, Word, Excel, PowerPoint, Markdown, text and web pages. Memory keeps what
                the document says — the file itself is not stored.
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              disabled={busy !== null || discarding}
              onClick={() => picker.current?.click()}
            >
              <FileUp className="size-4" /> Choose files
            </Button>
            <Button
              variant="outline"
              size="sm"
              disabled={busy !== null || discarding}
              onClick={() => {
                const typed = window.prompt("Link to remember");
                const url = typed?.trim();
                if (!url) return;
                void send([], [url]);
              }}
            >
              <Link2 className="size-4" /> Add link
            </Button>
          </div>
        </div>

        <input
          ref={picker}
          type="file"
          multiple
          className="hidden"
          onChange={(e) => {
            const files = Array.from(e.target.files ?? [])
              .map((file) => ({
                // A picked file carries `webkitRelativePath` only for a
                // directory pick; a plain multi-select has just the name.
                path: (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name,
                file,
              }))
              .filter((f) => !ignored(f.path));
            // Reset first: picking the same file twice in a row fires no
            // change event otherwise, and the second attempt looks broken.
            e.target.value = "";
            void send(files, []);
          }}
        />

        {report && <DropReport items={report} onDismiss={() => setReport(null)} />}
      </CardContent>
    </Card>
  );
}

/**
 * What became of each dropped file.
 *
 * Shown for every drop, not only failures: a folder always contains something
 * that could not be read, and an operator who is not told which files those
 * were will believe the whole folder is in memory.
 */
function DropReport({ items, onDismiss }: { items: IngestedItem[]; onDismiss: () => void }) {
  const failed = items.filter((i) => i.status !== "stored");
  const stored = items.length - failed.length;
  return (
    <div className="space-y-2 rounded-lg border bg-muted/30 p-3" data-testid="memory-drop-report">
      <div className="flex items-center justify-between gap-2">
        <p className="text-xs font-medium">
          {stored} remembered
          {failed.length > 0 && ` · ${failed.length} skipped`}
        </p>
        <Button variant="ghost" size="sm" className="h-6 text-xs" onClick={onDismiss}>
          Dismiss
        </Button>
      </div>
      {failed.length > 0 && (
        <ul className="max-h-40 space-y-1 overflow-y-auto text-xs text-muted-foreground">
          {failed.map((item) => (
            <li key={item.source} className="flex gap-2">
              <span className="truncate font-mono">{item.source}</span>
              <span className="shrink-0">— {item.detail ?? item.status}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
