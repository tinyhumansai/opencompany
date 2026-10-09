// Brain: the company's memory, served by OpenHuman's memory v2 engine.
//
// Learnings live at the company root, each teammate's conversations under its
// own agent node, and dropped documents under one node per source kind. The
// engine is OpenHuman's `[memory]` config, not a console choice, so this page
// reports whether memory is on rather than offering a picker.
import { useCallback, useEffect, useRef, useState } from "react";
import { Brain, FileText, Loader2, Plus, Search, Trash2 } from "lucide-react";
import { toast } from "sonner";

import {
  brainSources,
  createMemory,
  deleteMemory,
  forgetAgentMemory,
  forgetDocument,
  isMemoryOff,
  ITEM_KIND_LABELS,
  ITEM_KIND_STYLES,
  ITEM_KINDS,
  LEARNING_KIND_LABELS,
  LEARNING_KINDS,
  listMemory,
  memoryAgents,
  memoryStatus,
  type BrainSources,
  type ItemKind,
  type LearningKind,
  type MemoryAgent,
  type MemoryEntry,
  type MemoryStatus,
} from "@/api/memory";
import type { OpenCompanyClient } from "@/api/client";
import { VirtualList } from "@/components/virtual-list";
import { useMediaQuery } from "@/hooks/use-media-query";
import { DropZone } from "@/views/memory/DropZone";
import { BRAIN_PAGES, resolveBrainPage, type BrainPage } from "@/views/memory/brain-pages";
import { PageTabPanel, PageTabs } from "@/components/page-tabs";
import { consoleHref } from "@/lib/console-paths";
import { Markdown } from "@/components/markdown";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
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
  /**
   * The third hash segment — `upload` in `#/company/brain/upload`.
   *
   * Unvalidated here, as every sub-dispatching view takes it: only this view
   * knows which of its pages exist, so `resolveBrainPage` does that check.
   */
  sub?: string | null;
}

/** How long the search box waits after the last keystroke before asking the host. */
const SEARCH_DEBOUNCE_MS = 300;

/** Labels for the kind filter, including `all`. */
const KIND_FILTER_LABELS: Record<string, string> = {
  all: "All kinds",
  ...Object.fromEntries(ITEM_KINDS.map((k) => [k, `${ITEM_KIND_LABELS[k]}s`])),
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

/** The badge a row wears: its learning kind for learnings, else its item kind. */
function entryBadge(e: MemoryEntry): { label: string; style: string } {
  const style = ITEM_KIND_STYLES[e.kind] ?? ITEM_KIND_STYLES.learning;
  if (e.kind === "learning" && e.learningKind) {
    return { label: LEARNING_KIND_LABELS[e.learningKind], style };
  }
  return { label: ITEM_KIND_LABELS[e.kind] ?? e.kind, style };
}

/** Where a row came from, for the card footer. */
function entryOrigin(e: MemoryEntry): string {
  if (e.agentId) return e.agentId;
  if (e.source) return e.source;
  return e.namespace;
}

/**
 * The company's Brain: its memory read live from the host (`…/memory`), with
 * the engine's status, the teammates that remember conversations, and the
 * brain's documents by source. Operators add learnings and forget items,
 * whole teammates, or whole document sources.
 */
export function MemoryView({ client, company, sub }: Props) {
  // Overview for a bare `#/company/brain` and for any segment that names
  // nothing — a stale `#/company/brain/settings` bookmark lands on the page the
  // section is for rather than on an error.
  const page = resolveBrainPage(sub ?? null);
  // Brain's tabs ride the path segment they already owned rather than `?tab=`,
  // so every `#/company/brain/upload` ever linked still opens Upload.
  const openPage = (next: BrainPage) => {
    window.location.hash = consoleHref("brain", next === "overview" ? null : next);
  };
  const [status, setStatus] = useState<MemoryStatus | null>(null);
  const [agents, setAgents] = useState<MemoryAgent[]>([]);
  const [brain, setBrain] = useState<BrainSources | null>(null);
  const [entries, setEntries] = useState<MemoryEntry[]>([]);
  const [nextCursor, setNextCursor] = useState<string | undefined>(undefined);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [appliedQuery, setAppliedQuery] = useState("");
  const [kind, setKind] = useState<string>("all");
  const [agent, setAgent] = useState<string>("all");
  const [scrollEl, setScrollEl] = useState<HTMLDivElement | null>(null);
  const lanes = useMediaQuery("(min-width: 640px)") ? 2 : 1;
  // A generation token so a response from a previous scope or filter (or after
  // unmount) can't overwrite the current one.
  const gen = useRef(0);

  useEffect(() => {
    const t = setTimeout(() => setAppliedQuery(query.trim()), SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [query]);

  const filters = useCallback(
    () => ({
      query: appliedQuery || undefined,
      kind: kind === "all" ? undefined : (kind as ItemKind),
      agent: agent === "all" ? undefined : agent,
    }),
    [appliedQuery, kind, agent],
  );

  const load = useCallback(
    async (opts?: { silent?: boolean }) => {
      const mine = ++gen.current;
      if (!opts?.silent) setLoading(true);
      try {
        // Status first: it is the one route that answers when memory is off,
        // and every other route would only say `409 not_configured`.
        const s = await memoryStatus(client, company);
        if (mine !== gen.current) return;
        setStatus(s);
        if (!s.on) {
          setEntries([]);
          setNextCursor(undefined);
          setAgents([]);
          setBrain(null);
          setError(null);
          return;
        }
        const [list, a, b] = await Promise.all([
          listMemory(client, company, filters()),
          memoryAgents(client, company),
          brainSources(client, company),
        ]);
        if (mine !== gen.current) return;
        setEntries(list.items);
        setNextCursor(list.nextCursor);
        setAgents(a.agents);
        setBrain(b);
        setError(null);
      } catch (e) {
        if (mine !== gen.current) return;
        if (isMemoryOff(e)) {
          // Memory went off between the status read and the rest: re-read
          // status so the page explains why instead of showing the refusal.
          void memoryStatus(client, company)
            .then((s) => {
              if (mine === gen.current) setStatus(s);
            })
            .catch(() => {});
          setEntries([]);
          setNextCursor(undefined);
          setError(null);
          return;
        }
        setError(e instanceof Error ? e.message : "could not load memory");
      } finally {
        if (mine === gen.current && !opts?.silent) setLoading(false);
      }
    },
    [client, company, filters],
  );

  // A new company scope starts from a blank page rather than the last one's rows.
  useEffect(() => {
    setStatus(null);
    setEntries([]);
    setNextCursor(undefined);
    setAgents([]);
    setBrain(null);
    setAgent("all");
  }, [client, company]);

  useEffect(() => {
    void load();
    return () => {
      gen.current++;
    };
  }, [load]);

  async function loadMore() {
    if (!nextCursor) return;
    const mine = gen.current;
    setLoadingMore(true);
    try {
      const list = await listMemory(client, company, { ...filters(), cursor: nextCursor });
      if (mine !== gen.current) return;
      setEntries((all) => {
        const seen = new Set(all.map((e) => e.id));
        return [...all, ...list.items.filter((e) => !seen.has(e.id))];
      });
      setNextCursor(list.nextCursor);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not load more memory");
    } finally {
      setLoadingMore(false);
    }
  }

  const off = status !== null && !status.on;

  async function add(fields: { text: string; kind: LearningKind }) {
    await createMemory(client, company, fields);
    // Fire-and-forget: a reload failure must not reach the panel's "could not
    // save" toast and report a save that did happen as one that did not.
    void load({ silent: true });
  }

  async function remove(entry: MemoryEntry) {
    // Optimistic: drop the card immediately, then reconcile from the host.
    setEntries((all) => all.filter((x) => x.id !== entry.id));
    try {
      await deleteMemory(client, company, entry.id);
      void load({ silent: true });
    } catch (e) {
      setEntries((all) => (all.some((x) => x.id === entry.id) ? all : [entry, ...all]));
      toast.error(e instanceof Error ? e.message : "could not forget the memory");
    }
  }

  async function forgetAgent(agentId: string) {
    try {
      const { forgotten } = await forgetAgentMemory(client, company, agentId);
      toast.success(`Forgot ${forgotten} item${forgotten === 1 ? "" : "s"} from ${agentId}.`);
      setAgent("all");
      void load({ silent: true });
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not forget the agent's memory");
    }
  }

  async function forgetSource(source: string) {
    try {
      const { forgotten } = await forgetDocument(client, company, source);
      toast.success(`Forgot ${forgotten} ${source} document${forgotten === 1 ? "" : "s"}.`);
      void load({ silent: true });
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not forget the documents");
    }
  }

  const filtering = appliedQuery !== "" || kind !== "all" || agent !== "all";

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="Brain"
        width="full"
        description={
          <>
            What your company remembers — learnings, conversations, and documents your agents can
            recall.
          </>
        }
        tabs={
          <PageTabs
            tabs={BRAIN_PAGES}
            value={page}
            onChange={openPage}
            idBase="brain"
            aria-label="Brain views"
          />
        }
        actions={
          status && (
            <span
              className={cn(
                "rounded-full border px-3 py-1 text-xs",
                status.on ? "text-muted-foreground" : "border-status-blocked/40 text-status-blocked-text",
              )}
              title={status.on ? status.endpoint : status.reason}
              data-testid="memory-status-badge"
            >
              {status.on ? `memory: ${status.engine ?? "on"}` : "memory off"}
            </span>
          )
        }
      />
      <div
        ref={setScrollEl}
        className="min-h-0 w-full flex-1 space-y-5 overflow-y-auto px-4 py-6"
      >
        {off && (
          <Alert data-testid="memory-off">
            <AlertDescription>
              Memory is off{status?.reason ? ` — ${status.reason}` : ""}. Configure OpenHuman's{" "}
              <code className="text-xs">[memory]</code> engine to turn it on.
            </AlertDescription>
          </Alert>
        )}
        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        {/* Upload. Its own page rather than a target above the list: dropping a
            document is something an operator does when one arrives, not on the
            way to reading what is already remembered. */}
        <PageTabPanel idBase="brain" id="upload" value={page} className="space-y-5">
          <DropZone
            client={client}
            company={company}
            off={off}
            onIngested={() => void load({ silent: true })}
          />
          <AddLearningPanel off={off} onAdd={add} />
        </PageTabPanel>

        {/* Overview: status, sources, filters and the list — what the section
            is for, and what a bare `#/company/brain` lands on. */}
        <PageTabPanel idBase="brain" id="overview" value={page} className="space-y-5">
          <StatusStrip loading={loading} status={status} agents={agents} brain={brain} />
          {brain && brain.sources.length > 0 && (
            <BrainSourcesCard brain={brain} onForget={(s) => void forgetSource(s)} />
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
                disabled={off}
              />
            </div>
            <Select value={kind} onValueChange={(v) => v && setKind(v)} items={KIND_FILTER_LABELS}>
              <SelectTrigger className="w-40" aria-label="Filter by memory kind">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="all">All kinds</SelectItem>
                {ITEM_KINDS.map((k) => (
                  <SelectItem key={k} value={k}>
                    {KIND_FILTER_LABELS[k]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Select
              value={agent}
              onValueChange={(v) => v && setAgent(v)}
              items={{
                all: "All agents",
                ...Object.fromEntries(agents.map((a) => [a.agentId, a.agentId])),
              }}
            >
              <SelectTrigger className="w-44" aria-label="Filter by agent">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="all">All agents</SelectItem>
                {agents.map((a) => (
                  <SelectItem key={a.agentId} value={a.agentId}>
                    {a.agentId} · {a.turns}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {agent !== "all" && (
              <ConfirmAction
                trigger={
                  <Button variant="outline" size="sm" data-testid="memory-forget-agent">
                    <Trash2 className="mr-1.5 size-4" /> Forget {agent}'s memory
                  </Button>
                }
                title={`Forget everything ${agent} remembers?`}
                description="Every conversation this teammate has in memory is forgotten. Learnings and documents are kept. This cannot be undone."
                confirmLabel="Forget"
                onConfirm={() => void forgetAgent(agent)}
              />
            )}
          </div>

          {loading ? (
            <div className="grid gap-3 sm:grid-cols-2">
              <Skeleton className="h-28 rounded-xl" />
              <Skeleton className="h-28 rounded-xl" />
            </div>
          ) : entries.length === 0 ? (
            <EmptyMemory filtering={filtering} off={off} />
          ) : (
            <>
              <VirtualList
                items={entries}
                scrollElement={scrollEl}
                lanes={lanes}
                estimateRowHeight={132}
                getKey={(e) => e.id}
                renderItem={(e) => <MemoryCard entry={e} onDelete={() => void remove(e)} />}
                data-testid="memory-list"
              />
              {nextCursor && (
                <div className="flex justify-center">
                  <Button
                    variant="outline"
                    onClick={() => void loadMore()}
                    disabled={loadingMore}
                    data-testid="memory-load-more"
                  >
                    {loadingMore && <Loader2 className="mr-1.5 size-4 animate-spin" />}
                    Load more
                  </Button>
                </div>
              )}
            </>
          )}
        </PageTabPanel>
      </div>
    </div>
  );
}

function StatusStrip({
  loading,
  status,
  agents,
  brain,
}: {
  loading: boolean;
  status: MemoryStatus | null;
  agents: MemoryAgent[];
  brain: BrainSources | null;
}) {
  if (loading && !status) {
    return <Skeleton className="h-16 rounded-xl" />;
  }
  const documents =
    (brain?.sources.reduce((n, s) => n + s.documents, 0) ?? 0) + (brain?.unfiled ?? 0);
  const tiles: { label: string; value: string }[] = [
    { label: "Memory", value: status?.on ? "On" : "Off" },
    { label: "Engine", value: status?.engine ?? "—" },
    { label: "Agents", value: String(agents.length) },
    { label: "Conversation turns", value: String(agents.reduce((n, a) => n + a.turns, 0)) },
    { label: "Documents", value: String(documents) },
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
        {status?.root && (
          <span className="font-mono text-xs text-muted-foreground" title="Memory root">
            {status.root}
          </span>
        )}
      </CardContent>
    </Card>
  );
}

function BrainSourcesCard({
  brain,
  onForget,
}: {
  brain: BrainSources;
  onForget: (source: string) => void;
}) {
  return (
    <Card data-testid="memory-brain-sources">
      <CardContent className="space-y-2">
        <h3 className="flex items-center gap-1.5 text-sm font-medium">
          <FileText className="size-4" /> Documents by source
        </h3>
        <ul className="divide-y">
          {brain.sources.map((s) => (
            <li
              key={s.source}
              className="flex items-center justify-between gap-2 py-2"
              data-testid="memory-brain-source"
            >
              <span className="text-sm">
                <span className="font-mono">{s.source}</span>{" "}
                <span className="text-muted-foreground">
                  · {s.documents} document{s.documents === 1 ? "" : "s"}
                </span>
              </span>
              <ConfirmAction
                trigger={
                  <Button
                    variant="ghost"
                    size="icon"
                    className="size-7 text-muted-foreground hover:text-destructive"
                    aria-label={`Forget ${s.source} documents`}
                  >
                    <Trash2 className="size-4" />
                  </Button>
                }
                title={`Forget every ${s.source} document?`}
                description={`All ${s.documents} ${s.source} document${s.documents === 1 ? "" : "s"} in the brain are forgotten. This cannot be undone.`}
                confirmLabel="Forget"
                onConfirm={() => onForget(s.source)}
              />
            </li>
          ))}
        </ul>
        {brain.unfiled > 0 && (
          <p className="text-xs text-muted-foreground">
            {brain.unfiled} document{brain.unfiled === 1 ? "" : "s"} with no source.
          </p>
        )}
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
        {entry.body && entry.body !== entry.title && (
          // Render markdown so **bold**/lists format instead of showing raw
          // markup, muted on every descendant to keep the card's body styling.
          <Markdown className="text-muted-foreground [&_*]:text-muted-foreground [&>:first-child]:mt-0 [&>:last-child]:mb-0">
            {entry.body}
          </Markdown>
        )}
        <div className="flex items-center justify-between pt-1">
          <span className="text-xs text-muted-foreground">
            via {entryOrigin(entry)} · {formatUpdated(entry.updatedAt)}
          </span>
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

function EmptyMemory({ filtering, off }: { filtering: boolean; off: boolean }) {
  return (
    <div className="mt-16 flex flex-col items-center gap-2 text-center text-muted-foreground">
      <Brain className="size-8" />
      <p className="text-sm">
        {off ? "Memory is off." : filtering ? "No memories match." : "No memories yet."}
      </p>
    </div>
  );
}

function ConfirmAction({
  trigger,
  title,
  description,
  confirmLabel,
  onConfirm,
}: {
  trigger: React.ReactElement;
  title: string;
  description: string;
  confirmLabel: string;
  onConfirm: () => void;
}) {
  return (
    <AlertDialog>
      <AlertDialogTrigger render={trigger} />
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>{description}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction
            onClick={onConfirm}
            className="bg-destructive text-white hover:bg-destructive/90"
          >
            {confirmLabel}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/**
 * Add one learning by hand — inline, on the Upload page beside the drop zone,
 * so the two ways in (drop a document, type a learning) sit together. The host
 * stores it at the company root, where every teammate recalls it.
 */
function AddLearningPanel({
  off,
  onAdd,
}: {
  off: boolean;
  onAdd: (fields: { text: string; kind: LearningKind }) => Promise<void>;
}) {
  const [kind, setKind] = useState<LearningKind>("fact");
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const kindRevision = useRef(0);
  const textRevision = useRef(0);

  async function submit() {
    if (!text.trim()) return;
    setBusy(true);
    const submittedKindRevision = kindRevision.current;
    const submittedTextRevision = textRevision.current;
    try {
      await onAdd({ kind, text: text.trim() });
      // Nothing closes, so the form resets explicitly — text left standing
      // after a successful save reads as work that has not been saved yet.
      // Clear each saved field only when nothing newer was typed into it
      // during the write; never erase edits made while the save was pending.
      if (kindRevision.current === submittedKindRevision) {
        setKind("fact");
      }
      if (textRevision.current === submittedTextRevision) {
        setText("");
      }
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not save the learning");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card data-testid="memory-add">
      <CardContent className="grid gap-4">
        <div className="grid gap-1">
          <h3 className="flex items-center gap-1.5 text-sm font-medium">
            <Plus className="size-4" /> Add learning
          </h3>
          <p className="text-xs text-muted-foreground">
            Something every teammate should know and recall.
          </p>
        </div>
        <div className="grid gap-2">
          <Label htmlFor="mem-kind">Kind</Label>
          <Select
            value={kind}
            onValueChange={(v) => {
              if (v) {
                kindRevision.current++;
                setKind(v as LearningKind);
              }
            }}
            items={LEARNING_KIND_LABELS}
          >
            <SelectTrigger id="mem-kind" className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {LEARNING_KINDS.map((k) => (
                <SelectItem key={k} value={k}>
                  {LEARNING_KIND_LABELS[k]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="grid gap-2">
          <Label htmlFor="mem-text">Learning</Label>
          <Textarea
            id="mem-text"
            data-testid="memory-text"
            rows={3}
            value={text}
            onChange={(e) => {
              textRevision.current++;
              setText(e.target.value);
            }}
            placeholder="e.g. The client prefers Friday reviews."
          />
        </div>
        <div className="flex justify-end">
          {/* The reason rides on the wrapper: a `title` on a disabled button
              never surfaces, since `Button` drops pointer events. */}
          <span title={off ? "Memory is off — nothing saved here would be kept." : undefined}>
            <Button
              disabled={off || !text.trim() || busy}
              onClick={() => void submit()}
              data-testid="memory-save"
            >
              {busy && <Loader2 className="mr-1.5 size-4 animate-spin" />}
              Save learning
            </Button>
          </span>
        </div>
      </CardContent>
    </Card>
  );
}
