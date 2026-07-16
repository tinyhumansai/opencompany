import { useEffect, useMemo, useState } from "react";
import { Brain, Plus, Search, Trash2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import {
  KIND_STYLES,
  loadMemory,
  MEMORY_KINDS,
  type MemoryEntry,
  type MemoryKind,
  newMemory,
  saveMemory,
} from "@/lib/memory";

interface Props {
  company: string | null;
}

const KIND_LABELS: Record<string, string> = {
  all: "All types",
  fact: "Facts",
  preference: "Preferences",
  person: "People",
  project: "Projects",
  reference: "References",
};

/** The company's durable memory — searchable, filterable, and editable. */
export function MemoryView({ company }: Props) {
  const [entries, setEntries] = useState<MemoryEntry[]>(() => loadMemory(company));
  const [query, setQuery] = useState("");
  const [kind, setKind] = useState<string>("all");
  const [addOpen, setAddOpen] = useState(false);

  useEffect(() => {
    saveMemory(company, entries);
  }, [company, entries]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return entries
      .filter((e) => kind === "all" || e.kind === kind)
      .filter((e) => !q || e.title.toLowerCase().includes(q) || e.body.toLowerCase().includes(q))
      .sort((a, b) => b.updatedAt - a.updatedAt);
  }, [entries, query, kind]);

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-5xl space-y-5 px-4 py-6">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="space-y-1">
            <h2 className="text-2xl font-semibold tracking-tight">Memory</h2>
            <p className="text-sm text-muted-foreground">
              What your company remembers — facts, people, projects, and preferences.
            </p>
          </div>
          <Button onClick={() => setAddOpen(true)}>
            <Plus className="size-4" /> New memory
          </Button>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <div className="relative flex-1 sm:max-w-xs">
            <Search className="absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search memory…"
              className="pl-8"
            />
          </div>
          <Select value={kind} onValueChange={(v) => v && setKind(v)} items={KIND_LABELS}>
            <SelectTrigger className="w-40">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">All types</SelectItem>
              {MEMORY_KINDS.map((k) => (
                <SelectItem key={k} value={k}>
                  {KIND_LABELS[k]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        {filtered.length === 0 ? (
          <EmptyMemory hasEntries={entries.length > 0} />
        ) : (
          <div className="grid gap-3 sm:grid-cols-2">
            {filtered.map((e) => (
              <MemoryCard
                key={e.id}
                entry={e}
                onDelete={() => setEntries((all) => all.filter((x) => x.id !== e.id))}
              />
            ))}
          </div>
        )}
      </div>

      <AddMemoryDialog
        open={addOpen}
        onOpenChange={setAddOpen}
        onAdd={(fields) => {
          setEntries((all) => [newMemory(fields), ...all]);
          setAddOpen(false);
        }}
      />
    </div>
  );
}

function MemoryCard({ entry, onDelete }: { entry: MemoryEntry; onDelete: () => void }) {
  return (
    <Card className="group">
      <CardContent className="space-y-2 py-4">
        <div className="flex items-start justify-between gap-2">
          <p className="font-medium leading-snug">{entry.title}</p>
          <Badge variant="outline" className={cn("shrink-0 capitalize", KIND_STYLES[entry.kind])}>
            {entry.kind}
          </Badge>
        </div>
        <p className="text-sm text-muted-foreground">{entry.body}</p>
        <div className="flex items-center justify-between pt-1">
          <span className="text-xs text-muted-foreground">via {entry.source}</span>
          <Button
            variant="ghost"
            size="icon"
            className="size-7 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100 hover:text-destructive"
            onClick={onDelete}
            aria-label="Delete memory"
          >
            <Trash2 className="size-4" />
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}

function EmptyMemory({ hasEntries }: { hasEntries: boolean }) {
  return (
    <div className="mt-16 flex flex-col items-center gap-2 text-center text-muted-foreground">
      <Brain className="size-8" />
      <p className="text-sm">{hasEntries ? "No memories match your search." : "No memories yet."}</p>
    </div>
  );
}

function AddMemoryDialog({
  open,
  onOpenChange,
  onAdd,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onAdd: (fields: { kind: MemoryKind; title: string; body: string }) => void;
}) {
  const [kind, setKind] = useState<MemoryKind>("fact");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");

  function reset() {
    setKind("fact");
    setTitle("");
    setBody("");
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) reset();
      }}
    >
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>New memory</DialogTitle>
          <DialogDescription>Capture something your company should remember.</DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <Label htmlFor="mem-kind">Type</Label>
          <Select
            value={kind}
            onValueChange={(v) => v && setKind(v as MemoryKind)}
            items={Object.fromEntries(MEMORY_KINDS.map((k) => [k, KIND_LABELS[k]]))}
          >
            <SelectTrigger id="mem-kind" className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {MEMORY_KINDS.map((k) => (
                <SelectItem key={k} value={k}>
                  {KIND_LABELS[k]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="grid gap-2">
          <Label htmlFor="mem-title">Title</Label>
          <Input id="mem-title" value={title} onChange={(e) => setTitle(e.target.value)} placeholder="e.g. Client prefers Friday reviews" />
        </div>
        <div className="grid gap-2">
          <Label htmlFor="mem-body">Details</Label>
          <Textarea id="mem-body" rows={3} value={body} onChange={(e) => setBody(e.target.value)} placeholder="The detail your company should recall." />
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button disabled={!title.trim()} onClick={() => onAdd({ kind, title, body })}>
            Save memory
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
