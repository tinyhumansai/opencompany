import { useEffect, useMemo, useRef, useState } from "react";
import {
  ChevronRight,
  FilePlus2,
  FileText,
  Folder,
  FolderPlus,
  Home,
  MoreHorizontal,
  Upload,
  X,
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import {
  addFile,
  addFolder,
  childrenOf,
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
} from "@/lib/workspace";

interface Props {
  company: string | null;
}

/** A Drive/Notion-style workspace: folders + markdown files, organized locally. */
export function WorkspaceView({ company }: Props) {
  const [nodes, setNodes] = useState<FsNode[]>(() => loadWorkspace(company));
  const [folderId, setFolderId] = useState<string | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  const [prompt, setPrompt] = useState<PromptState | null>(null);
  const [moving, setMoving] = useState<FsNode | null>(null);
  const uploadRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    saveWorkspace(company, nodes);
  }, [company, nodes]);

  const items = useMemo(() => childrenOf(nodes, folderId), [nodes, folderId]);
  const crumbs = useMemo(() => pathOf(nodes, folderId), [nodes, folderId]);
  const openFile = nodeById(nodes, openId);

  function createFolder(name: string) {
    setNodes((n) => addFolder(n, folderId, name));
  }
  function createFile(name: string) {
    setNodes((n) => {
      const res = addFile(n, folderId, name);
      setOpenId(res.id);
      return res.nodes;
    });
  }

  async function onUpload(files: FileList | null) {
    if (!files?.length) return;
    const reads = await Promise.all(
      Array.from(files).map(async (f) => ({ name: f.name, text: await f.text().catch(() => "") })),
    );
    setNodes((n) => {
      let next = n;
      for (const r of reads) next = addFile(next, folderId, r.name, r.text).nodes;
      return next;
    });
  }

  return (
    <div className="flex flex-1 flex-col overflow-hidden">
      {/* Toolbar */}
      <div className="flex flex-wrap items-center gap-2 border-b px-4 py-2.5">
        <Breadcrumbs crumbs={crumbs} onNavigate={setFolderId} />
        <div className="ml-auto flex items-center gap-1.5">
          <Button variant="outline" size="sm" onClick={() => setPrompt({ mode: "folder" })}>
            <FolderPlus className="size-4" /> Folder
          </Button>
          <Button variant="outline" size="sm" onClick={() => setPrompt({ mode: "file" })}>
            <FilePlus2 className="size-4" /> File
          </Button>
          <Button variant="outline" size="sm" onClick={() => uploadRef.current?.click()}>
            <Upload className="size-4" /> Upload
          </Button>
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
      </div>

      {/* Body */}
      <div className="flex flex-1 overflow-hidden">
        <div className={cn("flex-1 overflow-y-auto p-4", openFile && "hidden lg:block lg:max-w-md lg:border-r")}>
          {items.length === 0 ? (
            <EmptyFolder />
          ) : (
            <div
              className={cn(
                "grid gap-3",
                openFile ? "grid-cols-1" : "sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4",
              )}
            >
              {items.map((node) => (
                <FsItem
                  key={node.id}
                  node={node}
                  active={node.id === openId}
                  onOpen={() => (node.kind === "folder" ? setFolderId(node.id) : setOpenId(node.id))}
                  onRename={() => setPrompt({ mode: "rename", node })}
                  onMove={() => setMoving(node)}
                  onDelete={() => {
                    setNodes((n) => removeNode(n, node.id));
                    if (openId && subtreeIds(nodes, node.id).has(openId)) setOpenId(null);
                  }}
                />
              ))}
            </div>
          )}
        </div>

        {openFile && openFile.kind === "file" && (
          <Editor
            file={openFile}
            onChange={(content) => setNodes((n) => setContent(n, openFile.id, content))}
            onClose={() => setOpenId(null)}
          />
        )}
      </div>

      <NamePrompt
        state={prompt}
        onClose={() => setPrompt(null)}
        onSubmit={(name) => {
          if (prompt?.mode === "folder") createFolder(name);
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

/* ---- breadcrumbs ---- */

function Breadcrumbs({ crumbs, onNavigate }: { crumbs: FsNode[]; onNavigate: (id: string | null) => void }) {
  return (
    <nav className="flex min-w-0 items-center gap-1 text-sm">
      <button
        onClick={() => onNavigate(null)}
        className="flex items-center gap-1 rounded px-1.5 py-1 font-medium hover:bg-accent"
      >
        <Home className="size-3.5" /> Workspace
      </button>
      {crumbs.map((c) => (
        <span key={c.id} className="flex min-w-0 items-center gap-1">
          <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
          <button onClick={() => onNavigate(c.id)} className="truncate rounded px-1.5 py-1 hover:bg-accent">
            {c.name}
          </button>
        </span>
      ))}
    </nav>
  );
}

/* ---- items ---- */

function FsItem({
  node,
  active,
  onOpen,
  onRename,
  onMove,
  onDelete,
}: {
  node: FsNode;
  active: boolean;
  onOpen: () => void;
  onRename: () => void;
  onMove: () => void;
  onDelete: () => void;
}) {
  const Icon = node.kind === "folder" ? Folder : FileText;
  return (
    <div
      className={cn(
        "group flex items-center gap-2.5 rounded-lg border bg-card p-3 transition-colors hover:bg-accent/40",
        active && "border-primary/40 bg-accent/40",
      )}
    >
      <button onClick={onOpen} className="flex min-w-0 flex-1 items-center gap-2.5 text-left">
        <Icon className={cn("size-5 shrink-0", node.kind === "folder" ? "text-sky-500" : "text-muted-foreground")} />
        <span className="truncate text-sm font-medium">{node.name}</span>
      </button>
      <DropdownMenu>
        <DropdownMenuTrigger
          render={<Button variant="ghost" size="icon" className="size-7 opacity-0 group-hover:opacity-100 data-[popup-open]:opacity-100" aria-label="Actions" />}
        >
          <MoreHorizontal className="size-4" />
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem onClick={onRename}>Rename</DropdownMenuItem>
          <DropdownMenuItem onClick={onMove}>Move to…</DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem variant="destructive" onClick={onDelete}>
            Delete
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}

function EmptyFolder() {
  return (
    <div className="mt-16 flex flex-col items-center gap-2 text-center text-muted-foreground">
      <Folder className="size-8" />
      <p className="text-sm">This folder is empty.</p>
      <p className="text-xs">Add a file or folder, or drop files with Upload.</p>
    </div>
  );
}

/* ---- editor ---- */

function Editor({
  file,
  onChange,
  onClose,
}: {
  file: FsNode;
  onChange: (content: string) => void;
  onClose: () => void;
}) {
  return (
    <div className="flex flex-1 flex-col overflow-hidden">
      <div className="flex items-center gap-2 border-b px-4 py-2">
        <FileText className="size-4 text-muted-foreground" />
        <span className="truncate text-sm font-medium">{file.name}</span>
        <Button variant="ghost" size="icon" className="ml-auto size-7" onClick={onClose} aria-label="Close">
          <X className="size-4" />
        </Button>
      </div>
      <Tabs defaultValue="edit" className="flex flex-1 flex-col overflow-hidden">
        <div className="px-4 pt-2">
          <TabsList>
            <TabsTrigger value="edit">Edit</TabsTrigger>
            <TabsTrigger value="preview">Preview</TabsTrigger>
          </TabsList>
        </div>
        <TabsContent value="edit" className="flex-1 overflow-hidden px-4 pb-4">
          <Textarea
            value={file.content ?? ""}
            onChange={(e) => onChange(e.target.value)}
            placeholder="Write in Markdown…"
            className="h-full min-h-0 resize-none font-mono text-sm"
          />
        </TabsContent>
        <TabsContent value="preview" className="flex-1 overflow-y-auto px-4 pb-4">
          <Markdown source={file.content ?? ""} />
        </TabsContent>
      </Tabs>
    </div>
  );
}

function Markdown({ source }: { source: string }) {
  if (!source.trim()) {
    return <p className="text-sm text-muted-foreground">Nothing to preview yet.</p>;
  }
  return (
    <div className="prose prose-sm max-w-none dark:prose-invert">
      <ReactMarkdown remarkPlugins={[remarkGfm]}>{source}</ReactMarkdown>
    </div>
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

  const title =
    state?.mode === "folder" ? "New folder" : state?.mode === "file" ? "New file" : "Rename";

  return (
    <Dialog open={Boolean(state)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            {state?.mode === "file" ? "Markdown files get a .md extension automatically." : "Give it a name."}
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
          <DialogTitle>Move “{moving?.name}”</DialogTitle>
          <DialogDescription>Pick a destination folder.</DialogDescription>
        </DialogHeader>
        <div className="max-h-72 space-y-1 overflow-y-auto">
          <DestRow label="Workspace" icon={<Home className="size-4" />} disabled={moving?.parentId === null} onClick={() => onMove(null)} />
          {folders.map((f) => (
            <DestRow
              key={f.id}
              label={f.name}
              icon={<Folder className="size-4 text-sky-500" />}
              disabled={moving?.parentId === f.id}
              onClick={() => onMove(f.id)}
            />
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
}

function DestRow({
  label,
  icon,
  disabled,
  onClick,
}: {
  label: string;
  icon: React.ReactNode;
  disabled?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      disabled={disabled}
      onClick={onClick}
      className="flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-sm hover:bg-accent disabled:pointer-events-none disabled:opacity-40"
    >
      {icon}
      <span className="truncate">{label}</span>
      {disabled && <span className="ml-auto text-xs text-muted-foreground">Here</span>}
    </button>
  );
}
