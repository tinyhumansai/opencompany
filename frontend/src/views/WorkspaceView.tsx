import { useCallback, useEffect, useRef, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  FilePlus2,
  FileText,
  Folder,
  FolderOpen,
  FolderPlus,
  Link2,
  Loader2,
  MoreHorizontal,
  PanelLeft,
  RefreshCw,
  Upload,
} from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { toast } from "sonner";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import {
  createNode,
  deleteNode as deleteNodeApi,
  fetchFile,
  fetchTree,
  renameMoveNode,
  writeFile,
  type WorkspaceFile,
} from "@/api/workspace";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import {
  childrenOf,
  clearLegacyLocal,
  ensureMdExt,
  fileByTitle,
  type FsNode,
  hasLegacyLocal,
  nodeById,
  pathOf,
  readLegacyLocalNodes,
  subtreeIds,
  titleOf,
} from "@/lib/workspace";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/** How long typing settles before the editor pushes a save to the host. */
const AUTOSAVE_DELAY_MS = 800;

/** The folder created to hold notes rescued from the retired local scratchpad. */
const IMPORT_FOLDER_NAME = "Imported from this browser";

/** What the editor's status line is currently reporting. */
type SaveState = "idle" | "saving" | "saved" | "error";

/** Formats an epoch-millis instant for the open note's header. */
function formatUpdated(ms: number): string {
  if (!ms) return "—";
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

/** Whether a failed request was a 404 (the note is gone on the host). */
function isNotFound(e: unknown): boolean {
  return e instanceof ApiError && e.status === 404;
}

function message(e: unknown, fallback: string): string {
  return e instanceof Error ? e.message : fallback;
}

/**
 * An Obsidian-style workspace: a file-tree explorer, a markdown note pane with
 * `[[wiki links]]`, and a backlinks panel — all of it the company's **real**
 * workspace, read and written on the host over `…/workspace` (issue #177).
 *
 * This used to be a localStorage scratchpad seeded with invented marketing
 * notes, so the operator's workspace and the one the agents read and write
 * (their `workspace_*` tools, issue #237) were two unrelated trees. Now there is
 * one: a note an agent writes shows up here on the next refresh, and a note
 * typed here is readable by an agent on its next turn.
 *
 * Writes are **apply-on-ack**, never optimistic — every mutation returns the
 * authoritative node, so local state is patched from the response and a failure
 * simply leaves the tree as it was. The editor is the one exception: it keeps a
 * local dirty buffer so typing is never blocked on (or lost to) the network.
 *
 * Two gaps are deliberate and tracked rather than worked around: notes carry no
 * authorship, so an agent's note is indistinguishable from the operator's
 * (#326), and there is no live push, so a write that lands while the tab is open
 * appears on refresh/refocus rather than instantly (#327).
 */
export function WorkspaceView({ client, company }: Props) {
  const [nodes, setNodes] = useState<FsNode[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [openId, setOpenId] = useState<string | null>(null);
  const [openFile, setOpenFile] = useState<WorkspaceFile | null>(null);
  const [fileError, setFileError] = useState<string | null>(null);

  const [mode, setMode] = useState<"read" | "edit">("read");
  const [draft, setDraft] = useState<string | null>(null);
  const [saveState, setSaveState] = useState<SaveState>("idle");

  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  const [prompt, setPrompt] = useState<PromptState | null>(null);
  const [moving, setMoving] = useState<FsNode | null>(null);
  const [showExplorer, setShowExplorer] = useState(true);
  const [legacy, setLegacy] = useState<FsNode[]>([]);
  const [importing, setImporting] = useState(false);
  const uploadRef = useRef<HTMLInputElement>(null);

  // Generation tokens so a response from a previous company scope (or from a
  // file that has since been closed) can never overwrite the current one.
  const treeGen = useRef(0);
  const fileGen = useRef(0);
  // Whether the explorer's initial folder expansion has happened yet, so a later
  // refresh never re-opens folders the operator collapsed.
  const expandedSeeded = useRef(false);

  /* ---- tree ---- */

  const loadTree = useCallback(
    async (opts?: { silent?: boolean }) => {
      const mine = ++treeGen.current;
      if (opts?.silent) setRefreshing(true);
      else setLoading(true);
      try {
        const tree = await fetchTree(client, company);
        if (mine !== treeGen.current) return;
        setNodes(tree);
        setError(null);
        if (!expandedSeeded.current) {
          expandedSeeded.current = true;
          setExpanded(
            new Set(
              childrenOf(tree, null)
                .filter((n) => n.kind === "folder")
                .map((n) => n.id),
            ),
          );
        }
      } catch (e) {
        if (mine !== treeGen.current) return;
        setError(message(e, "could not load the workspace"));
      } finally {
        if (mine === treeGen.current) {
          setLoading(false);
          setRefreshing(false);
        }
      }
    },
    [client, company],
  );

  // Mount / company change: reset every scoped piece of state, then load.
  useEffect(() => {
    expandedSeeded.current = false;
    setNodes([]);
    setOpenId(null);
    setOpenFile(null);
    setDraft(null);
    setSaveState("idle");
    setExpanded(new Set());
    void loadTree();
    return () => {
      treeGen.current++;
      fileGen.current++;
    };
  }, [loadTree]);

  /* ---- the open note ---- */

  const loadFile = useCallback(
    async (id: string) => {
      const mine = ++fileGen.current;
      try {
        const file = await fetchFile(client, company, id);
        if (mine !== fileGen.current) return file;
        setOpenFile(file);
        setFileError(null);
        return file;
      } catch (e) {
        if (mine !== fileGen.current) return null;
        setOpenFile(null);
        setFileError(message(e, "could not open this note"));
        return null;
      }
    },
    [client, company],
  );

  /* ---- the editor's dirty buffer ---- */

  // The pending save, held in a ref so the debounce timer and the unmount
  // cleanup both see the latest value without re-subscribing on every keystroke.
  const pending = useRef<{ id: string; content: string } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const flush = useCallback(async () => {
    if (timer.current) {
      clearTimeout(timer.current);
      timer.current = null;
    }
    const job = pending.current;
    if (!job) return;
    pending.current = null;
    setSaveState("saving");
    try {
      const ack = await writeFile(client, company, job.id, job.content);
      setSaveState("saved");
      // Patch the authoritative stamp onto both the open file and the tree row,
      // so "last updated" is the host's answer and not a guess.
      setOpenFile((f) =>
        f && f.id === job.id ? { ...f, content: job.content, updatedAt: ack.updatedAt } : f,
      );
      setNodes((all) =>
        all.map((n) => (n.id === job.id ? { ...n, updatedAt: ack.updatedAt } : n)),
      );
    } catch (e) {
      // Keep the buffer: the operator's text is never dropped because a save
      // failed. A 404 means the note is gone on the host, which needs a decision
      // rather than a retry, so say so explicitly.
      //
      // Only restore when nothing newer arrived. The operator can keep typing
      // during the await above, and that typing writes a fresher job into
      // `pending.current` — overwriting it with the job we just failed to save
      // would silently discard every keystroke made while the request was in
      // flight, which is the exact loss this buffer exists to prevent.
      if (!pending.current) pending.current = job;
      setSaveState("error");
      if (isNotFound(e)) {
        toast.error("This note no longer exists on the host.", {
          description: "Someone deleted it. Your text is still here — save it as a new note.",
        });
      } else {
        toast.error(message(e, "could not save this note"));
      }
    }
  }, [client, company]);

  // Always flush through the newest closure, including from cleanup callbacks
  // that captured an older one.
  const flushRef = useRef(flush);
  useEffect(() => {
    flushRef.current = flush;
  }, [flush]);

  // Unmounting (tab switch, sign-out) must not silently drop buffered typing.
  useEffect(() => {
    return () => {
      void flushRef.current();
    };
  }, []);

  function onEdit(id: string, content: string) {
    setDraft(content);
    setSaveState("idle");
    pending.current = { id, content };
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => void flushRef.current(), AUTOSAVE_DELAY_MS);
  }

  /* ---- refresh on refocus ---- */

  // No live push (#327): a note written by an agent or another browser is
  // invisible until we ask again. Coming back to the tab is the moment an
  // operator most expects to see current state, so refetch quietly there.
  useEffect(() => {
    const refresh = () => {
      if (document.visibilityState === "visible") void loadTree({ silent: true });
    };
    window.addEventListener("focus", refresh);
    document.addEventListener("visibilitychange", refresh);
    return () => {
      window.removeEventListener("focus", refresh);
      document.removeEventListener("visibilitychange", refresh);
    };
  }, [loadTree]);

  /* ---- migration off the retired localStorage scratchpad ---- */

  useEffect(() => {
    const mine = readLegacyLocalNodes(company);
    if (mine.length > 0) {
      setLegacy(mine);
      return;
    }
    // A key holding nothing but the bundled seed is app-shipped bytes with zero
    // user information — sweep it silently rather than offering to import
    // invented marketing copy into a real company's workspace.
    if (hasLegacyLocal(company)) clearLegacyLocal(company);
    setLegacy([]);
  }, [company]);

  async function importLegacy() {
    setImporting(true);
    try {
      const root = await createNode(client, company, {
        name: IMPORT_FOLDER_NAME,
        kind: "folder",
      });
      // Folders first, so a child's remapped parent id always exists by the time
      // it is created. Old ids are local-only and meaningless to the host.
      const remap = new Map<string, string>();
      const parentFor = (node: FsNode) =>
        node.parentId ? (remap.get(node.parentId) ?? root.id) : root.id;
      for (const folder of legacy.filter((n) => n.kind === "folder")) {
        const created = await createNode(client, company, {
          name: folder.name,
          kind: "folder",
          parentId: parentFor(folder),
        });
        remap.set(folder.id, created.id);
      }
      for (const file of legacy.filter((n) => n.kind === "file")) {
        await createNode(client, company, {
          name: ensureMdExt(file.name),
          kind: "file",
          parentId: parentFor(file),
          content: file.content ?? "",
        });
      }
      clearLegacyLocal(company);
      setLegacy([]);
      toast.success(`Imported ${legacy.length} note${legacy.length === 1 ? "" : "s"}.`);
      await loadTree({ silent: true });
    } catch (e) {
      // The key is left intact on failure, so the banner comes back and nothing
      // the operator wrote is lost to a half-finished import.
      toast.error(message(e, "could not import your local notes"));
    } finally {
      setImporting(false);
    }
  }

  function discardLegacy() {
    clearLegacyLocal(company);
    setLegacy([]);
  }

  /* ---- navigation ---- */

  async function open(id: string) {
    await flush();
    setOpenId(id);
    setMode("read");
    setDraft(null);
    setSaveState("idle");
    setOpenFile(null);
    setFileError(null);
    setExpanded((prev) => {
      const next = new Set(prev);
      for (const a of pathOf(nodes, id)) if (a.kind === "folder") next.add(a.id);
      return next;
    });
    await loadFile(id);
  }

  function toggle(id: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  // Switching to Edit refetches first, so the operator edits what the host
  // currently holds rather than a copy that may be minutes stale — the cheapest
  // mitigation available for the no-CAS overwrite window.
  async function changeMode(next: "read" | "edit") {
    await flush();
    if (next === "edit" && openId) {
      const fresh = await loadFile(openId);
      setDraft(fresh?.content ?? null);
    } else {
      setDraft(null);
    }
    setSaveState("idle");
    setMode(next);
  }

  /* ---- mutations (apply-on-ack) ---- */

  async function createAndOpen(name: string, content?: string) {
    try {
      const created = await createNode(client, company, {
        name: ensureMdExt(name.trim() || "Untitled"),
        kind: "file",
        parentId: null,
        content: content ?? "",
      });
      setNodes((all) => [...all, created]);
      setOpenId(created.id);
      setOpenFile({
        id: created.id,
        name: created.name,
        content: content ?? "",
        updatedAt: created.updatedAt,
        backlinks: [],
      });
      setFileError(null);
      setDraft(content ?? "");
      setSaveState("idle");
      setMode("edit");
    } catch (e) {
      toast.error(message(e, "could not create the note"));
    }
  }

  async function createFolder(name: string) {
    try {
      const created = await createNode(client, company, {
        name: name.trim() || "New folder",
        kind: "folder",
        parentId: null,
      });
      setNodes((all) => [...all, created]);
    } catch (e) {
      toast.error(message(e, "could not create the folder"));
    }
  }

  async function rename(node: FsNode, name: string) {
    const next = (node.kind === "file" ? ensureMdExt(name.trim()) : name.trim()) || node.name;
    try {
      const updated = await renameMoveNode(client, company, node.id, { name: next });
      setNodes((all) => all.map((n) => (n.id === updated.id ? updated : n)));
      setOpenFile((f) => (f && f.id === updated.id ? { ...f, name: updated.name } : f));
    } catch (e) {
      toast.error(message(e, "could not rename this item"));
    }
  }

  async function move(node: FsNode, destId: string | null) {
    try {
      // The move-cycle guard is the host's: it answers 400 for a folder moved
      // under its own descendant, which surfaces here as a toast.
      const updated = await renameMoveNode(client, company, node.id, { parentId: destId });
      setNodes((all) => all.map((n) => (n.id === updated.id ? updated : n)));
    } catch (e) {
      toast.error(message(e, "could not move this item"));
    }
  }

  async function remove(node: FsNode) {
    const removed = subtreeIds(nodes, node.id);
    try {
      await deleteNodeApi(client, company, node.id);
      setNodes((all) => all.filter((n) => !removed.has(n.id)));
      if (openId && removed.has(openId)) {
        pending.current = null;
        setOpenId(null);
        setOpenFile(null);
        setDraft(null);
      }
    } catch (e) {
      toast.error(message(e, "could not delete this item"));
    }
  }

  async function onWiki(target: string) {
    const existing = fileByTitle(nodes, target);
    if (existing) {
      await open(existing.id);
      return;
    }
    await flush();
    await createAndOpen(target, `# ${target}\n`);
  }

  async function onUpload(files: FileList | null) {
    if (!files?.length) return;
    const reads = await Promise.all(
      Array.from(files).map(async (f) => ({ name: f.name, text: await f.text().catch(() => "") })),
    );
    for (const r of reads) {
      try {
        const created = await createNode(client, company, {
          name: ensureMdExt(r.name),
          kind: "file",
          parentId: null,
          content: r.text,
        });
        setNodes((all) => [...all, created]);
      } catch (e) {
        toast.error(`${r.name}: ${message(e, "upload failed")}`);
      }
    }
  }

  const body = draft ?? openFile?.content ?? "";
  const openNode = nodeById(nodes, openId);

  return (
    <div className="flex flex-1 overflow-hidden">
      {/* Explorer */}
      <aside
        className={cn(
          "w-64 shrink-0 flex-col border-r bg-card/40 md:flex",
          showExplorer ? "flex" : "hidden",
        )}
      >
        <div className="flex items-center gap-1 border-b px-2 py-2">
          <span className="flex-1 px-1 text-xs font-semibold tracking-wide text-muted-foreground uppercase">
            Explorer
          </span>
          <IconBtn
            label="Refresh"
            onClick={() => void loadTree({ silent: true })}
            data-testid="workspace-refresh"
          >
            <RefreshCw className={cn("size-4", refreshing && "animate-spin")} />
          </IconBtn>
          <IconBtn label="New file" onClick={() => setPrompt({ mode: "file" })}>
            <FilePlus2 className="size-4" />
          </IconBtn>
          <IconBtn label="New folder" onClick={() => setPrompt({ mode: "folder" })}>
            <FolderPlus className="size-4" />
          </IconBtn>
          <IconBtn label="Upload" onClick={() => uploadRef.current?.click()}>
            <Upload className="size-4" />
          </IconBtn>
          <input
            ref={uploadRef}
            type="file"
            accept=".md,.markdown,.txt"
            multiple
            hidden
            onChange={(e) => {
              void onUpload(e.target.files);
              e.target.value = "";
            }}
          />
        </div>
        <div className="flex-1 overflow-y-auto py-1" data-testid="workspace-tree">
          {loading ? (
            <div className="space-y-2 px-2 py-2">
              <Skeleton className="h-5 w-4/5" />
              <Skeleton className="h-5 w-3/5" />
              <Skeleton className="h-5 w-2/3" />
            </div>
          ) : error ? (
            <div className="px-2 py-2">
              <Alert variant="destructive">
                <AlertDescription data-testid="workspace-error">{error}</AlertDescription>
              </Alert>
              <Button
                variant="outline"
                size="sm"
                className="mt-2 w-full"
                onClick={() => void loadTree()}
              >
                Try again
              </Button>
            </div>
          ) : nodes.length === 0 ? (
            <p className="px-3 py-2 text-xs text-muted-foreground">
              This workspace is empty. Create a note to start.
            </p>
          ) : (
            <Tree
              nodes={nodes}
              parentId={null}
              depth={0}
              expanded={expanded}
              openId={openId}
              onToggle={toggle}
              onOpen={(id) => void open(id)}
              onRename={(node) => setPrompt({ mode: "rename", node })}
              onMove={(node) => setMoving(node)}
              onDelete={(node) => void remove(node)}
            />
          )}
        </div>
      </aside>

      {/* Note pane */}
      <section className={cn("flex-1 flex-col overflow-hidden", showExplorer ? "hidden md:flex" : "flex")}>
        {legacy.length > 0 && (
          <Alert className="m-3 mb-0 w-auto" data-testid="workspace-migration-banner">
            <AlertDescription className="flex flex-wrap items-center gap-3">
              <span className="flex-1">
                {legacy.length} note{legacy.length === 1 ? "" : "s"} from this browser's old
                scratchpad {legacy.length === 1 ? "is" : "are"} not in the company workspace yet.
              </span>
              <Button
                size="sm"
                disabled={importing}
                onClick={() => void importLegacy()}
                data-testid="workspace-migration-import"
              >
                {importing && <Loader2 className="size-3.5 animate-spin" />} Import
              </Button>
              <Button size="sm" variant="ghost" disabled={importing} onClick={discardLegacy}>
                Discard
              </Button>
            </AlertDescription>
          </Alert>
        )}
        {openNode && openNode.kind === "file" ? (
          <>
            <div className="flex items-center gap-2 border-b px-3 py-2">
              <IconBtn label="Toggle explorer" onClick={() => setShowExplorer((s) => !s)}>
                <PanelLeft className="size-4" />
              </IconBtn>
              <span className="truncate text-sm font-medium">{titleOf(openNode)}</span>
              {/* No authorship on a node (#326), so "when" is the only signal an
                  agent has been in here — worth showing plainly. */}
              <span className="hidden shrink-0 text-xs text-muted-foreground sm:inline">
                {formatUpdated(openFile?.updatedAt ?? openNode.updatedAt)}
              </span>
              <SaveStatus state={saveState} />
              <Tabs
                value={mode}
                onValueChange={(v) => void changeMode(v as "read" | "edit")}
                className="ml-auto"
              >
                <TabsList>
                  <TabsTrigger value="read">Reading</TabsTrigger>
                  <TabsTrigger value="edit">Edit</TabsTrigger>
                </TabsList>
              </Tabs>
            </div>
            <div className="flex flex-1 overflow-hidden">
              <div className="flex-1 overflow-y-auto">
                {fileError ? (
                  <div className="p-6">
                    <Alert variant="destructive">
                      <AlertDescription>{fileError}</AlertDescription>
                    </Alert>
                  </div>
                ) : !openFile ? (
                  <div className="mx-auto max-w-3xl space-y-3 px-6 py-6">
                    <Skeleton className="h-6 w-1/3" />
                    <Skeleton className="h-4 w-full" />
                    <Skeleton className="h-4 w-5/6" />
                  </div>
                ) : mode === "edit" ? (
                  <Textarea
                    value={body}
                    onChange={(e) => onEdit(openFile.id, e.target.value)}
                    onBlur={() => void flush()}
                    placeholder="Write in Markdown… link with [[Note name]]"
                    data-testid="workspace-editor"
                    className="h-full min-h-0 resize-none rounded-none border-0 p-6 font-mono text-sm shadow-none focus-visible:ring-0"
                  />
                ) : (
                  <div className="mx-auto max-w-3xl px-6 py-6" data-testid="workspace-note">
                    <NoteMarkdown source={body} nodes={nodes} onWiki={(t) => void onWiki(t)} />
                  </div>
                )}
              </div>
              {/* Backlinks — computed by the host, not derived here: the tree
                  read carries no bodies, so the client has nothing to scan. */}
              <aside className="hidden w-56 shrink-0 flex-col border-l bg-card/30 xl:flex">
                <div className="border-b px-3 py-2 text-xs font-semibold tracking-wide text-muted-foreground uppercase">
                  Backlinks
                </div>
                <div className="flex-1 overflow-y-auto p-2">
                  {!openFile || openFile.backlinks.length === 0 ? (
                    <p className="px-1 py-2 text-xs text-muted-foreground">No backlinks yet.</p>
                  ) : (
                    openFile.backlinks.map((b) => (
                      <button
                        key={b.id}
                        onClick={() => void open(b.id)}
                        className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-accent"
                      >
                        <Link2 className="size-3.5 shrink-0 text-muted-foreground" />
                        <span className="truncate">{titleOf(b)}</span>
                      </button>
                    ))
                  )}
                </div>
              </aside>
            </div>
          </>
        ) : (
          <EmptyNote onNew={() => setPrompt({ mode: "file" })} onToggleExplorer={() => setShowExplorer((s) => !s)} />
        )}
      </section>

      <NamePrompt
        state={prompt}
        onClose={() => setPrompt(null)}
        onSubmit={(name) => {
          if (prompt?.mode === "folder") void createFolder(name);
          else if (prompt?.mode === "file") void createAndOpen(name);
          else if (prompt?.mode === "rename" && prompt.node) void rename(prompt.node, name);
          setPrompt(null);
        }}
      />
      <MoveDialog
        nodes={nodes}
        moving={moving}
        onClose={() => setMoving(null)}
        onMove={(destId) => {
          if (moving) void move(moving, destId);
          setMoving(null);
        }}
      />
    </div>
  );
}

/** The editor's save indicator: quiet when idle, explicit when it matters. */
function SaveStatus({ state }: { state: SaveState }) {
  if (state === "idle") return null;
  const label =
    state === "saving" ? "Saving…" : state === "saved" ? "Saved" : "Not saved — retrying on edit";
  return (
    <span
      data-testid="workspace-save-state"
      data-state={state}
      className={cn(
        "shrink-0 text-xs",
        state === "error" ? "text-destructive" : "text-muted-foreground",
      )}
    >
      {label}
    </span>
  );
}

/* ---- explorer tree ---- */

interface TreeProps {
  nodes: FsNode[];
  parentId: string | null;
  depth: number;
  expanded: Set<string>;
  openId: string | null;
  onToggle: (id: string) => void;
  onOpen: (id: string) => void;
  onRename: (node: FsNode) => void;
  onMove: (node: FsNode) => void;
  onDelete: (node: FsNode) => void;
}

function Tree(props: TreeProps) {
  const items = childrenOf(props.nodes, props.parentId);
  return (
    <>
      {items.map((node) => (
        <TreeRow key={node.id} node={node} {...props} />
      ))}
    </>
  );
}

function TreeRow({ node, ...props }: TreeProps & { node: FsNode }) {
  const { depth, expanded, openId, onToggle, onOpen } = props;
  const isFolder = node.kind === "folder";
  const isOpen = expanded.has(node.id);
  const active = node.id === openId;

  return (
    <>
      <div
        className={cn(
          "group flex items-center gap-1 rounded-md px-1.5 py-1 text-sm",
          active ? "bg-accent font-medium" : "hover:bg-accent/50",
        )}
        style={{ paddingLeft: 6 + depth * 12 }}
      >
        <button
          onClick={() => (isFolder ? onToggle(node.id) : onOpen(node.id))}
          className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
        >
          {isFolder ? (
            <>
              {isOpen ? <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" /> : <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />}
              {isOpen ? <FolderOpen className="size-4 shrink-0 text-sky-500" /> : <Folder className="size-4 shrink-0 text-sky-500" />}
            </>
          ) : (
            <FileText className="ml-3.5 size-4 shrink-0 text-muted-foreground" />
          )}
          <span className="truncate">{isFolder ? node.name : titleOf(node)}</span>
        </button>
        <DropdownMenu>
          <DropdownMenuTrigger
            render={<Button variant="ghost" size="icon" className="size-6 opacity-0 group-hover:opacity-100 data-[popup-open]:opacity-100" aria-label="Actions" />}
          >
            <MoreHorizontal className="size-3.5" />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem onClick={() => props.onRename(node)}>Rename</DropdownMenuItem>
            <DropdownMenuItem onClick={() => props.onMove(node)}>Move to…</DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem variant="destructive" onClick={() => props.onDelete(node)}>
              Delete
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
      {isFolder && isOpen && <Tree {...props} parentId={node.id} depth={depth + 1} />}
    </>
  );
}

/* ---- note markdown with wiki links ---- */

function NoteMarkdown({
  source,
  nodes,
  onWiki,
}: {
  source: string;
  nodes: FsNode[];
  onWiki: (target: string) => void;
}) {
  if (!source.trim()) {
    return <p className="text-sm text-muted-foreground">This note is empty. Switch to Edit to write.</p>;
  }
  // Rewrite [[target]] / [[target|alias]] into links the renderer can style —
  // but leave fenced and inline code untouched (so `[[…]]` examples survive).
  const rewritten = source.replace(
    /(```[\s\S]*?```|~~~[\s\S]*?~~~|`[^`\n]*`)|\[\[([^\]|]+)(?:\|([^\]]+))?\]\]/g,
    (_m, code: string | undefined, target: string, alias?: string) =>
      code
        ? code
        : `[${(alias ?? target).trim()}](#wiki:${encodeURIComponent(target.trim())})`,
  );
  return (
    <div className="prose prose-sm max-w-none dark:prose-invert">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          a({ href, children }) {
            if (href?.startsWith("#wiki:")) {
              const target = decodeURIComponent(href.slice("#wiki:".length));
              const exists = Boolean(fileByTitle(nodes, target));
              return (
                <button
                  type="button"
                  onClick={() => onWiki(target)}
                  className={cn(
                    "rounded px-0.5 font-medium no-underline",
                    exists
                      ? "text-primary hover:underline"
                      : "text-muted-foreground underline decoration-dashed underline-offset-2",
                  )}
                >
                  {children}
                </button>
              );
            }
            return (
              <a href={href} target="_blank" rel="noreferrer">
                {children}
              </a>
            );
          },
        }}
      >
        {rewritten}
      </ReactMarkdown>
    </div>
  );
}

function EmptyNote({ onNew, onToggleExplorer }: { onNew: () => void; onToggleExplorer: () => void }) {
  return (
    <div className="flex flex-1 flex-col">
      <div className="flex items-center border-b px-3 py-2 md:hidden">
        <IconBtn label="Toggle explorer" onClick={onToggleExplorer}>
          <PanelLeft className="size-4" />
        </IconBtn>
      </div>
      <div className="flex flex-1 flex-col items-center justify-center gap-3 text-center">
        <FileText className="size-8 text-muted-foreground" />
        <div className="space-y-1">
          <p className="font-medium">No note open</p>
          <p className="text-sm text-muted-foreground">Pick a note from the explorer, or create one.</p>
        </div>
        <Button variant="outline" size="sm" onClick={onNew}>
          <FilePlus2 className="size-4" /> New note
        </Button>
      </div>
    </div>
  );
}

function IconBtn({
  label,
  onClick,
  children,
  ...rest
}: {
  label: string;
  onClick: () => void;
  children: React.ReactNode;
} & Omit<React.ComponentProps<typeof Button>, "onClick" | "children">) {
  return (
    <Button
      variant="ghost"
      size="icon"
      className="size-7 text-muted-foreground"
      aria-label={label}
      onClick={onClick}
      {...rest}
    >
      {children}
    </Button>
  );
}

/* ---- dialogs ---- */

interface PromptState {
  mode: "folder" | "file" | "rename";
  node?: FsNode;
}

function NamePrompt({
  state,
  onClose,
  onSubmit,
}: {
  state: PromptState | null;
  onClose: () => void;
  onSubmit: (name: string) => void;
}) {
  const [name, setName] = useState("");

  useEffect(() => {
    setName(state?.mode === "rename" ? (state.node?.name ?? "") : "");
  }, [state]);

  const title = state?.mode === "folder" ? "New folder" : state?.mode === "file" ? "New note" : "Rename";

  return (
    <Dialog open={Boolean(state)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            {state?.mode === "file" ? "Notes get a .md extension automatically." : "Give it a name."}
          </DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <Label htmlFor="fs-name">Name</Label>
          <Input
            id="fs-name"
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && name.trim()) onSubmit(name);
            }}
            placeholder={state?.mode === "folder" ? "e.g. Campaigns" : "e.g. Notes"}
          />
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button disabled={!name.trim()} onClick={() => onSubmit(name)}>
            {state?.mode === "rename" ? "Rename" : "Create"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function MoveDialog({
  nodes,
  moving,
  onClose,
  onMove,
}: {
  nodes: FsNode[];
  moving: FsNode | null;
  onClose: () => void;
  onMove: (destId: string | null) => void;
}) {
  const blocked = moving ? subtreeIds(nodes, moving.id) : new Set<string>();
  const folders = nodes.filter((x) => x.kind === "folder" && !blocked.has(x.id));

  return (
    <Dialog open={Boolean(moving)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>Move “{moving ? titleOf(moving) : ""}”</DialogTitle>
          <DialogDescription>Pick a destination folder.</DialogDescription>
        </DialogHeader>
        <div className="max-h-72 space-y-1 overflow-y-auto">
          <DestRow label="Workspace root" disabled={moving?.parentId === null} onClick={() => onMove(null)} />
          {folders.map((f) => (
            <DestRow key={f.id} label={f.name} disabled={moving?.parentId === f.id} onClick={() => onMove(f.id)} />
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
}

function DestRow({ label, disabled, onClick }: { label: string; disabled?: boolean; onClick: () => void }) {
  return (
    <button
      disabled={disabled}
      onClick={onClick}
      className="flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-sm hover:bg-accent disabled:pointer-events-none disabled:opacity-40"
    >
      <Folder className="size-4 text-sky-500" />
      <span className="truncate">{label}</span>
      {disabled && <span className="ml-auto text-xs text-muted-foreground">Here</span>}
    </button>
  );
}
