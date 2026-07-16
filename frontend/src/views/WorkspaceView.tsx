import { useEffect, useMemo, useRef, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  FilePlus2,
  FileText,
  Folder,
  FolderOpen,
  FolderPlus,
  Link2,
  MoreHorizontal,
  PanelLeft,
  Upload,
} from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

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
import { cn } from "@/lib/utils";
import {
  addFile,
  addFolder,
  backlinksTo,
  childrenOf,
  fileByTitle,
  type FsNode,
  loadWorkspace,
  moveNode,
  nodeById,
  pathOf,
  removeNode,
  renameNode,
  saveWorkspace,
  setContent,
  subtreeIds,
  titleOf,
} from "@/lib/workspace";

interface Props {
  company: string | null;
}

/** An Obsidian-style workspace: a file-tree explorer, a markdown note pane with
 *  `[[wiki links]]`, and a backlinks panel. */
export function WorkspaceView({ company }: Props) {
  const [nodes, setNodes] = useState<FsNode[]>(() => loadWorkspace(company));
  const [openId, setOpenId] = useState<string | null>(null);
  const [mode, setMode] = useState<"read" | "edit">("read");
  const [expanded, setExpanded] = useState<Set<string>>(
    () => new Set(childrenOf(loadWorkspace(company), null).filter((n) => n.kind === "folder").map((n) => n.id)),
  );
  const [prompt, setPrompt] = useState<PromptState | null>(null);
  const [moving, setMoving] = useState<FsNode | null>(null);
  const [showExplorer, setShowExplorer] = useState(true);
  const uploadRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    saveWorkspace(company, nodes);
  }, [company, nodes]);

  const openFile = nodeById(nodes, openId);
  const backlinks = useMemo(
    () => (openFile ? backlinksTo(nodes, openFile) : []),
    [nodes, openFile],
  );

  function open(id: string) {
    setOpenId(id);
    setMode("read");
    setExpanded((prev) => {
      const next = new Set(prev);
      for (const a of pathOf(nodes, id)) if (a.kind === "folder") next.add(a.id);
      return next;
    });
  }

  function toggle(id: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function onWiki(target: string) {
    const existing = fileByTitle(nodes, target);
    if (existing) {
      open(existing.id);
      return;
    }
    const res = addFile(nodes, null, target, `# ${target}\n`);
    setNodes(res.nodes);
    setOpenId(res.id);
    setMode("edit");
  }

  async function onUpload(files: FileList | null) {
    if (!files?.length) return;
    const reads = await Promise.all(
      Array.from(files).map(async (f) => ({ name: f.name, text: await f.text().catch(() => "") })),
    );
    setNodes((n) => {
      let next = n;
      for (const r of reads) next = addFile(next, null, r.name, r.text).nodes;
      return next;
    });
  }

  function createFile(name: string) {
    setNodes((n) => {
      const res = addFile(n, null, name);
      setOpenId(res.id);
      setMode("edit");
      return res.nodes;
    });
  }

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
        <div className="flex-1 overflow-y-auto py-1">
          <Tree
            nodes={nodes}
            parentId={null}
            depth={0}
            expanded={expanded}
            openId={openId}
            onToggle={toggle}
            onOpen={open}
            onRename={(node) => setPrompt({ mode: "rename", node })}
            onMove={(node) => setMoving(node)}
            onDelete={(node) => {
              setNodes((n) => removeNode(n, node.id));
              if (openId && subtreeIds(nodes, node.id).has(openId)) setOpenId(null);
            }}
          />
        </div>
      </aside>

      {/* Note pane */}
      <section className={cn("flex-1 flex-col overflow-hidden", showExplorer ? "hidden md:flex" : "flex")}>
        {openFile && openFile.kind === "file" ? (
          <>
            <div className="flex items-center gap-2 border-b px-3 py-2">
              <IconBtn label="Toggle explorer" onClick={() => setShowExplorer((s) => !s)}>
                <PanelLeft className="size-4" />
              </IconBtn>
              <span className="truncate text-sm font-medium">{titleOf(openFile)}</span>
              <Tabs value={mode} onValueChange={(v) => setMode(v as "read" | "edit")} className="ml-auto">
                <TabsList>
                  <TabsTrigger value="read">Reading</TabsTrigger>
                  <TabsTrigger value="edit">Edit</TabsTrigger>
                </TabsList>
              </Tabs>
            </div>
            <div className="flex flex-1 overflow-hidden">
              <div className="flex-1 overflow-y-auto">
                {mode === "edit" ? (
                  <Textarea
                    value={openFile.content ?? ""}
                    onChange={(e) => setNodes((n) => setContent(n, openFile.id, e.target.value))}
                    placeholder="Write in Markdown… link with [[Note name]]"
                    className="h-full min-h-0 resize-none rounded-none border-0 p-6 font-mono text-sm shadow-none focus-visible:ring-0"
                  />
                ) : (
                  <div className="mx-auto max-w-3xl px-6 py-6">
                    <NoteMarkdown source={openFile.content ?? ""} nodes={nodes} onWiki={onWiki} />
                  </div>
                )}
              </div>
              {/* Backlinks */}
              <aside className="hidden w-56 shrink-0 flex-col border-l bg-card/30 xl:flex">
                <div className="border-b px-3 py-2 text-xs font-semibold tracking-wide text-muted-foreground uppercase">
                  Backlinks
                </div>
                <div className="flex-1 overflow-y-auto p-2">
                  {backlinks.length === 0 ? (
                    <p className="px-1 py-2 text-xs text-muted-foreground">No backlinks yet.</p>
                  ) : (
                    backlinks.map((b) => (
                      <button
                        key={b.id}
                        onClick={() => open(b.id)}
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
          if (prompt?.mode === "folder") setNodes((n) => addFolder(n, null, name));
          else if (prompt?.mode === "file") createFile(name);
          else if (prompt?.mode === "rename" && prompt.node)
            setNodes((n) => renameNode(n, prompt.node!.id, name));
          setPrompt(null);
        }}
      />
      <MoveDialog
        nodes={nodes}
        moving={moving}
        onClose={() => setMoving(null)}
        onMove={(destId) => {
          if (moving) setNodes((n) => moveNode(n, moving.id, destId));
          setMoving(null);
        }}
      />
    </div>
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
}: {
  label: string;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <Button variant="ghost" size="icon" className="size-7 text-muted-foreground" aria-label={label} onClick={onClick}>
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
