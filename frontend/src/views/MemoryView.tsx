// Brain lives under Settings (issue #1416): the memory browser and its engine
// controls belong together, while the sidebar keeps its scarce permanent rows
// for surfaces an operator works from every day.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Brain, Loader2, Plus, Search, Trash2 } from "lucide-react";
import { toast } from "sonner";

import {
  CONTEXT_ORIGINS,
  createMemory,
  deleteMemory,
  documentSlug,
  forgetDocument,
  KIND_STYLES,
  listMemory,
  MEMORY_KINDS,
  memoryStats,
  ORIGIN_LABELS,
  ORIGIN_STYLES,
  type MemoryEngineState,
  type MemoryEntry,
  type MemoryKind,
  type MemoryStats,
} from "@/api/memory";
import type { OpenCompanyClient } from "@/api/client";
import { DropZone } from "@/views/memory/DropZone";
import { EngineSection } from "@/views/memory/EngineSection";
import { Markdown } from "@/components/markdown";
import { PageHeader } from "@/components/page-header";
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

/**
 * The value the type filter matches a row on: a fact matches on its `kind`, a
 * read-only context row matches on its `origin` (agent-memory / task-outcome).
 * Keeps the original per-kind filtering while extending it to the new sources.
 */
function entryType(e: MemoryEntry): string {
  return e.origin === "fact" ? (e.kind ?? "fact") : e.origin;
}

/** The badge label + style for a row, from its kind (facts) or origin (context). */
function entryBadge(e: MemoryEntry): { label: string; style: string } {
  if (e.origin === "fact") {
    const kind = e.kind ?? "fact";
    return { label: kind, style: KIND_STYLES[kind] };
  }
  return { label: ORIGIN_LABELS[e.origin], style: ORIGIN_STYLES[e.origin] };
}

/** The type-filter options in display order: fact kinds, then context origins. */
const TYPE_FILTERS: string[] = [...MEMORY_KINDS, ...CONTEXT_ORIGINS];

/** Labels for every type-filter value (including `all`), for the Select. */
const TYPE_FILTER_LABELS: Record<string, string> = {
  all: "All types",
  ...Object.fromEntries(MEMORY_KINDS.map((k) => [k, KIND_LABELS[k]])),
  ...Object.fromEntries(CONTEXT_ORIGINS.map((o) => [o, ORIGIN_LABELS[o]])),
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
  // The truncation metadata that rode in with the last list read, kept beside
  // `entries` because the banner's "newest N of M" must describe the SAME read
  // as the rows it counts — a write between two requests would let N and M
  // silently disagree.
  const [totalContext, setTotalContext] = useState(0);
  const [contextTruncated, setContextTruncated] = useState(false);
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
        const [list, s] = await Promise.all([
          listMemory(client, company),
          memoryStats(client, company),
        ]);
        if (mine !== gen.current) return;
        setEntries(list.items);
        setTotalContext(list.totalContext);
        setContextTruncated(list.contextTruncated);
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
    setTotalContext(0);
    setContextTruncated(false);
    setStats(null);
    void load();
    return () => {
      gen.current++;
    };
  }, [load]);

  // The bound memory engine, from the engine route rather than `/spec`.
  //
  // One source, because the two can now disagree: `/spec`'s snapshot is what
  // boot bound, and an operator who switches engines from the section below
  // changes what is bound without restarting. A header badge naming the
  // previous engine would be the most confusing possible answer to "did my
  // change take".
  const [engine, setEngine] = useState<MemoryEngineState | null>(null);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return entries
      .filter((e) => kind === "all" || entryType(e) === kind)
      .filter((e) => !q || e.title.toLowerCase().includes(q) || e.body.toLowerCase().includes(q));
  }, [entries, query, kind]);

  // Per-type counts for the health badges, keyed by the same value the filter
  // matches on (fact kind or context origin) so every source shows a count.
  const perType = useMemo(() => {
    const counts: Record<string, number> = {};
    for (const e of entries) {
      const t = entryType(e);
      counts[t] = (counts[t] ?? 0) + 1;
    }
    return counts;
  }, [entries]);

  const listedContextItems = useMemo(
    () => entries.filter((entry) => entry.origin !== "fact").length,
    [entries],
  );

  // The one engine state the *writing* half of this page has to respect: the
  // null engine takes every write and throws it away, so a live "New memory"
  // button beside that warning invites work the host will silently drop
  // (issue #1410). The panel's health dot already refuses to go green here for
  // the same reason.
  const discarding = engine?.active === "null";

  async function add(fields: { kind: MemoryKind; title: string; body: string }) {
    await createMemory(client, company, fields);
    // Close the moment the write is confirmed, then reload in the background.
    // The dialog's catch owns the "could not save the memory" toast, so only
    // createMemory — an actual save failure — may reach it. Awaiting the reload
    // here instead would route a reload failure into that same catch (a false
    // save error) and skip this close, stranding the dialog open so the operator
    // retries and writes a duplicate. `void load` is fire-and-forget: load
    // handles its own errors via the page banner and never leaks a rejection.
    setAddOpen(false);
    void load({ silent: true });
  }

  async function remove(entry: MemoryEntry) {
    // Optimistic: drop the card immediately, then reconcile counts from the host.
    setEntries((all) => all.filter((x) => x.id !== entry.id));
    try {
      if (entry.origin === "document") {
        // A document is many chunks under one slug, so forgetting it is one
        // call against the document — deleting the card's own chunk would
        // leave the rest of the file in memory, which is worse than not
        // offering a delete at all.
        await forgetDocument(client, company, documentSlug(entry.source));
      } else {
        await deleteMemory(client, company, entry.id);
      }
      await load({ silent: true });
    } catch (e) {
      // Re-insert only this entry on failure (no whole-list rollback).
      setEntries((all) => (all.some((x) => x.id === entry.id) ? all : [entry, ...all]));
      toast.error(e instanceof Error ? e.message : "could not delete the memory");
    }
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="Brain"
        width="5xl"
        description={
          <>
            What your company remembers — facts, people, projects, and preferences your
            teammates can recall.
          </>
        }
        actions={
          <>
            {engine && (
              <span
                className={cn(
                  "rounded-full border px-3 py-1 text-xs",
                  // A capability count in the calm register, inches above an
                  // alert saying every write is discarded, reads as a fact
                  // about a working engine. Amber, like the dot (issue #1410).
                  discarding
                    ? "border-status-blocked/40 text-status-blocked-text"
                    : "text-muted-foreground",
                )}
                title={
                  discarding
                    ? "This engine discards every write — nothing saved here is retained."
                    : engine.capabilities.length
                      ? `Capability families: ${engine.capabilities.join(", ")}`
                      : "Capabilities not negotiated"
                }
                data-testid="memory-engine-badge"
              >
                engine: {engine.active}
                {engine.capabilities.length > 0 && (
                  <> · {engine.capabilities.length} families</>
                )}
              </span>
            )}
            {/*
              The reason rides on the wrapper, not the button: `Button` carries
              `disabled:pointer-events-none`, so a `title` on a disabled button
              never surfaces — the span still takes the hover and shows it.
            */}
            <span
              title={
                discarding
                  ? "This engine discards every write — nothing saved here is retained."
                  : undefined
              }
            >
              <Button
                onClick={() => setAddOpen(true)}
                disabled={discarding}
                // Rendered, not hidden: the operator should see that writing is
                // the thing this engine cannot do, not find the control missing.
                data-testid="memory-add"
              >
                <Plus className="size-4" /> New memory
              </Button>
            </span>
          </>
        }
      />
      <div className="mx-auto min-h-0 w-full max-w-5xl flex-1 space-y-5 overflow-y-auto px-4 py-6">

        <EngineSection
          client={client}
          company={company}
          onApplied={(next) => {
            setEngine(next);
            // The new engine's memory is a different set of rows — often an
            // empty one, since nothing migrates between engines — so the list
            // has to be re-read rather than left showing the old engine's.
            void load({ silent: true });
          }}
        />

        <DropZone
          client={client}
          company={company}
          discarding={discarding}
          onIngested={() => void load({ silent: true })}
        />

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <HealthStrip loading={loading} stats={stats} perType={perType} />
        {contextTruncated && (
          <Alert>
            <AlertDescription>
              Showing the newest {listedContextItems} of {totalContext} context memory items.
            </AlertDescription>
          </Alert>
        )}

        <div className="flex flex-wrap items-center gap-2">
          <div className="relative flex-1 sm:max-w-xs">
            <Search className="absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              aria-label="Search memory"
              placeholder="Search memory…"
              className="pl-8"
            />
          </div>
          <Select value={kind} onValueChange={(v) => v && setKind(v)} items={TYPE_FILTER_LABELS}>
            <SelectTrigger className="w-40" aria-label="Filter by memory type">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">All types</SelectItem>
              {TYPE_FILTERS.map((t) => (
                <SelectItem key={t} value={t}>
                  {TYPE_FILTER_LABELS[t]}
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
  perType,
}: {
  loading: boolean;
  stats: MemoryStats | null;
  perType: Record<string, number>;
}) {
  if (loading && !stats) {
    return <Skeleton className="h-16 rounded-xl" />;
  }
  const tiles: { label: string; value: string }[] = [
    { label: "Total items", value: String(stats?.totalItems ?? 0) },
    { label: "Operator facts", value: String(stats?.facts ?? 0) },
    { label: "Teammate memory", value: String(stats?.teammateMemory ?? 0) },
    { label: "Document chunks", value: String(stats?.documentMemory ?? 0) },
    { label: "Task outcomes", value: String(stats?.taskOutcomes ?? 0) },
    // Across every memory source, not just operator facts — teammates write only
    // context chunks, so a facts-only figure left this stat at "—" forever.
    { label: "Last updated", value: formatUpdated(stats?.lastUpdatedAtMillis ?? 0) },
  ];
  return (
    <Card data-testid="memory-health">
      <CardContent className="flex flex-wrap items-center gap-x-8 gap-y-3">
        {tiles.map((t) => (
          <div key={t.label} className="space-y-0.5">
            <p className="text-xs text-muted-foreground">{t.label}</p>
            <p className="text-lg font-semibold tabular-nums">{t.value}</p>
          </div>
        ))}
        <div className="flex flex-wrap items-center gap-1.5">
          {MEMORY_KINDS.filter((k) => perType[k]).map((k) => (
            <Badge key={k} variant="outline" className={cn("capitalize", KIND_STYLES[k])}>
              {k} · {perType[k]}
            </Badge>
          ))}
          {CONTEXT_ORIGINS.filter((o) => perType[o]).map((o) => (
            <Badge key={o} variant="outline" className={ORIGIN_STYLES[o]}>
              {ORIGIN_LABELS[o]} · {perType[o]}
            </Badge>
          ))}
        </div>
      </CardContent>
    </Card>
  );
}

function MemoryCard({ entry, onDelete }: { entry: MemoryEntry; onDelete: () => void }) {
  const badge = entryBadge(entry);
  return (
    <Card className="group" data-testid="memory-card">
      <CardContent className="space-y-2">
        <div className="flex items-start justify-between gap-2">
          <p className="font-medium leading-snug">{entry.title}</p>
          <Badge variant="outline" className={cn("shrink-0 capitalize", badge.style)}>
            {badge.label}
          </Badge>
        </div>
        {entry.body && (
          // Render markdown so **bold**/lists in memory bodies format instead
          // of showing raw markup. Force muted-foreground on every descendant so
          // prose's own palette doesn't override the card's muted body styling.
          <Markdown className="text-muted-foreground [&_*]:text-muted-foreground [&>:first-child]:mt-0 [&>:last-child]:mb-0">
            {entry.body}
          </Markdown>
        )}
        <div className="flex items-center justify-between pt-1">
          <span className="text-xs text-muted-foreground">via {entry.source}</span>
          {/* Delete is only offered on operator facts; agent memory and task
              outcomes are read-only, so they show no delete affordance. */}
          {entry.editable && (
            <Button
              variant="ghost"
              size="icon"
              className="size-7 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100 hover:text-destructive"
              onClick={onDelete}
              aria-label="Delete memory"
            >
              <Trash2 className="size-4" />
            </Button>
          )}
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
