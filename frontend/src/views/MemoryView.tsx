import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Brain, Loader2, Plus, Search, Trash2 } from "lucide-react";
import { toast } from "sonner";

import {
  createMemory,
  deleteMemory,
  KIND_STYLES,
  listMemory,
  MEMORY_KINDS,
  memoryStats,
  type MemoryEntry,
  type MemoryKind,
  type MemoryStats,
} from "@/api/memory";
import type { OpenCompanyClient } from "@/api/client";
import { Alert, AlertDescription } from "@/components/ui/alert";
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
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
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

/** Formats an epoch-millis instant as a short absolute date, or a dash when 0. */
function formatUpdated(ms: number): string {
  if (!ms) return "—";
  return new Date(ms).toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
  });
}

/**
 * The company's Brain: its durable memory, read live from the host (`…/memory`)
 * with a health strip proving the store is real (fact + agent-context counts).
 * Operators add and delete facts; a create is mirrored server-side into the
 * agents' recallable context so a note reaches an agent on its next turn.
 */
export function MemoryView({ client, company }: Props) {
  const [entries, setEntries] = useState<MemoryEntry[]>([]);
  const [stats, setStats] = useState<MemoryStats | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [kind, setKind] = useState<string>("all");
  const [addOpen, setAddOpen] = useState(false);
  // A generation token so a response from a previous company scope (or after
  // unmount) can't overwrite the current one.
  const gen = useRef(0);

  const load = useCallback(
    async (opts?: { silent?: boolean }) => {
      const mine = ++gen.current;
      if (!opts?.silent) setLoading(true);
      try {
        const [rows, s] = await Promise.all([
          listMemory(client, company),
          memoryStats(client, company),
        ]);
        if (mine !== gen.current) return;
        setEntries(rows);
        setStats(s);
        setError(null);
      } catch (e) {
        if (mine !== gen.current) return;
        setError(e instanceof Error ? e.message : "could not load memory");
      } finally {
        if (mine === gen.current && !opts?.silent) setLoading(false);
      }
    },
    [client, company],
  );

  useEffect(() => {
    setEntries([]);
    setStats(null);
    void load();
    return () => {
      gen.current++;
    };
  }, [load]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return entries
      .filter((e) => kind === "all" || e.kind === kind)
      .filter((e) => !q || e.title.toLowerCase().includes(q) || e.body.toLowerCase().includes(q));
  }, [entries, query, kind]);

  const perKind = useMemo(() => {
    const counts: Record<string, number> = {};
    for (const e of entries) counts[e.kind] = (counts[e.kind] ?? 0) + 1;
    return counts;
  }, [entries]);

  async function add(fields: { kind: MemoryKind; title: string; body: string }) {
    await createMemory(client, company, fields);
    await load({ silent: true });
    setAddOpen(false);
  }

  async function remove(entry: MemoryEntry) {
    // Optimistic: drop the card immediately, then reconcile counts from the host.
    setEntries((all) => all.filter((x) => x.id !== entry.id));
    try {
      await deleteMemory(client, company, entry.id);
      await load({ silent: true });
    } catch (e) {
      // Re-insert only this entry on failure (no whole-list rollback).
      setEntries((all) => (all.some((x) => x.id === entry.id) ? all : [entry, ...all]));
      toast.error(e instanceof Error ? e.message : "could not delete the memory");
    }
  }

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-5xl space-y-5 px-4 py-6">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="space-y-1">
            <h2 className="text-2xl font-semibold tracking-tight">Brain</h2>
            <p className="text-sm text-muted-foreground">
              What your company remembers — facts, people, projects, and preferences your agents can
              recall.
            </p>
          </div>
          <Button onClick={() => setAddOpen(true)} data-testid="memory-add">
            <Plus className="size-4" /> New memory
          </Button>
        </div>

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <HealthStrip loading={loading} stats={stats} total={entries.length} perKind={perKind} />

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

        {loading ? (
          <div className="grid gap-3 sm:grid-cols-2">
            <Skeleton className="h-28 rounded-xl" />
            <Skeleton className="h-28 rounded-xl" />
          </div>
        ) : filtered.length === 0 ? (
          <EmptyMemory hasEntries={entries.length > 0} />
        ) : (
          <div className="grid gap-3 sm:grid-cols-2">
            {filtered.map((e) => (
              <MemoryCard key={e.id} entry={e} onDelete={() => void remove(e)} />
            ))}
          </div>
        )}
      </div>

      <AddMemoryDialog open={addOpen} onOpenChange={setAddOpen} onAdd={add} />
    </div>
  );
}

function HealthStrip({
  loading,
  stats,
  total,
  perKind,
}: {
  loading: boolean;
  stats: MemoryStats | null;
  total: number;
  perKind: Record<string, number>;
}) {
  if (loading && !stats) {
    return <Skeleton className="h-16 rounded-xl" />;
  }
  const tiles: { label: string; value: string }[] = [
    { label: "Total items", value: String(total) },
    { label: "Agent memory", value: String(stats?.agentChunks ?? 0) },
    { label: "Task outcomes", value: String(stats?.taskOutcomes ?? 0) },
    { label: "Last updated", value: formatUpdated(stats?.factsUpdatedAtMillis ?? 0) },
  ];
  return (
    <Card data-testid="memory-health">
      <CardContent className="flex flex-wrap items-center gap-x-8 gap-y-3 py-4">
        {tiles.map((t) => (
          <div key={t.label} className="space-y-0.5">
            <p className="text-xs text-muted-foreground">{t.label}</p>
            <p className="text-lg font-semibold tabular-nums">{t.value}</p>
          </div>
        ))}
        <div className="flex flex-wrap items-center gap-1.5">
          {MEMORY_KINDS.filter((k) => perKind[k]).map((k) => (
            <Badge key={k} variant="outline" className={cn("capitalize", KIND_STYLES[k])}>
              {k} · {perKind[k]}
            </Badge>
          ))}
        </div>
      </CardContent>
    </Card>
  );
}

function MemoryCard({ entry, onDelete }: { entry: MemoryEntry; onDelete: () => void }) {
  return (
    <Card className="group" data-testid="memory-card">
      <CardContent className="space-y-2 py-4">
        <div className="flex items-start justify-between gap-2">
          <p className="font-medium leading-snug">{entry.title}</p>
          <Badge variant="outline" className={cn("shrink-0 capitalize", KIND_STYLES[entry.kind])}>
            {entry.kind}
          </Badge>
        </div>
        {entry.body && <p className="text-sm text-muted-foreground">{entry.body}</p>}
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
  onAdd: (fields: { kind: MemoryKind; title: string; body: string }) => Promise<void>;
}) {
  const [kind, setKind] = useState<MemoryKind>("fact");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);

  function reset() {
    setKind("fact");
    setTitle("");
    setBody("");
  }

  async function submit() {
    if (!title.trim()) return;
    setBusy(true);
    try {
      await onAdd({ kind, title: title.trim(), body: body.trim() });
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not save the memory");
    } finally {
      setBusy(false);
    }
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
          <Input
            id="mem-title"
            data-testid="memory-title"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            placeholder="e.g. Client prefers Friday reviews"
          />
        </div>
        <div className="grid gap-2">
          <Label htmlFor="mem-body">Details</Label>
          <Textarea
            id="mem-body"
            data-testid="memory-body"
            rows={3}
            value={body}
            onChange={(e) => setBody(e.target.value)}
            placeholder="The detail your company should recall."
          />
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={busy}>
            Cancel
          </Button>
          <Button
            disabled={!title.trim() || busy}
            onClick={() => void submit()}
            data-testid="memory-save"
          >
            {busy && <Loader2 className="mr-1.5 size-4 animate-spin" />}
            Save memory
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
