// The Task Detail **Artifacts** tab (#306): the console half of the versioned
// artifacts + human-edit diff backend (#187).
//
// What the tab is for is the diff, not the list. A completed dispatch records
// its output as an agent-authored version; when an operator fixes it up before
// approving, that edit lands as a *new version by a different author*, and the
// gap between the two is the highest-signal quality datum the product produces
// — heavy churn means the agent's instructions need work. So this tab both
// shows that story and is where the operator edit is made, because nothing else
// in the console can author an operator version.
//
// Self-fetching by design: the tab panel mounts when the tab is activated, so
// it owns its own read + poll rather than widening the parent's single
// `GET …/tasks/{id}` (#185) with a payload three of the four tabs never use.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowLeft,
  FileText,
  History,
  Image as ImageIcon,
  Loader2,
  Paperclip,
  Pencil,
  UserRound,
} from "lucide-react";

import {
  appendArtifactVersion,
  diffArtifact,
  listTaskArtifacts,
  type ArtifactDiff,
  type ArtifactKind,
  type ArtifactVersion,
  type ArtifactView,
} from "@/api/artifacts";
import type { OpenCompanyClient } from "@/api/client";
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
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { Markdown } from "@/components/markdown";
import { cn } from "@/lib/utils";
import { toast } from "sonner";

/** Matches the parent screen's poll cadence; visibility-gated the same way. */
const POLL_MS = 4000;

/** Kinds whose body is text an operator can meaningfully edit in a textarea. */
const EDITABLE_KINDS: ReadonlySet<ArtifactKind> = new Set<ArtifactKind>(["text", "markdown"]);

const KIND_ICON: Record<ArtifactKind, typeof FileText> = {
  text: FileText,
  markdown: FileText,
  image: ImageIcon,
  file: Paperclip,
};

function whenOf(at: number): string {
  return new Date(at).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

/** `34%` — churn is a 0–1 fraction of the agent draft's lines the human changed. */
function churnPercent(churn: number): string {
  return `${Math.round(churn * 100)}%`;
}

export function ArtifactsTab({
  client,
  company,
  taskId,
}: {
  client: OpenCompanyClient;
  company: string | null;
  taskId: string;
}) {
  const [artifacts, setArtifacts] = useState<ArtifactView[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);

  // A poll that lands mid-edit would swap the textarea's seed out from under
  // the operator, so the editor freezes the refresh while it is open. The ref
  // (rather than an effect dependency) keeps the timer itself untouched, so
  // closing the editor resumes on the next tick instead of restarting the poll.
  const editingRef = useRef(false);

  const load = useCallback(
    async (isActive: () => boolean = () => true) => {
      try {
        const rows = await listTaskArtifacts(client, company, taskId);
        if (!isActive()) return;
        setArtifacts(rows);
        setError(null);
      } catch (e) {
        if (!isActive()) return;
        setError(e instanceof Error ? e.message : "could not load this task's artifacts");
      } finally {
        if (isActive()) setLoading(false);
      }
    },
    [client, company, taskId],
  );

  // Mirrors TaskDetailView's poll: 4s, paused while the tab is hidden, resumed
  // with an immediate fetch when it comes back. `isActive` is a per-run token
  // so a superseded run can never apply a previous task's rows.
  useEffect(() => {
    let cancelled = false;
    const isActive = () => !cancelled;
    setLoading(true);
    setArtifacts(null);
    setOpenId(null);
    void load(isActive);
    let timer: number | undefined;
    const stop = () => {
      if (timer !== undefined) {
        window.clearInterval(timer);
        timer = undefined;
      }
    };
    const start = () => {
      if (timer === undefined) {
        timer = window.setInterval(() => {
          if (!editingRef.current) void load(isActive);
        }, POLL_MS);
      }
    };
    const onVisibility = () => {
      if (document.visibilityState === "hidden") {
        stop();
      } else {
        if (!editingRef.current) void load(isActive);
        start();
      }
    };
    if (document.visibilityState !== "hidden") start();
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      cancelled = true;
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [load]);

  const open = useMemo(
    () => (openId ? (artifacts?.find((a) => a.id === openId) ?? null) : null),
    [artifacts, openId],
  );

  // Replace one row in place after an append, so the freshly returned artifact
  // (with its now-present `humanEditDiff`) renders before the poll comes round.
  const replace = useCallback((next: ArtifactView) => {
    setArtifacts((rows) => (rows ?? []).map((a) => (a.id === next.id ? next : a)));
  }, []);

  // Stable identity: the detail panel reports its editor state through an
  // effect, and an inline closure here would re-run that effect on every
  // render — flipping the pause flag off and on again mid-edit.
  const setEditing = useCallback((editing: boolean) => {
    editingRef.current = editing;
  }, []);
  const refresh = useCallback(() => void load(), [load]);

  if (loading && artifacts === null) {
    return (
      <div className="space-y-2">
        <Skeleton className="h-16 rounded-xl" />
        <Skeleton className="h-16 rounded-xl" />
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {error && (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {open ? (
        <ArtifactDetail
          client={client}
          company={company}
          artifact={open}
          onBack={() => setOpenId(null)}
          onAppended={replace}
          onRefresh={refresh}
          onEditingChange={setEditing}
        />
      ) : artifacts && artifacts.length > 0 ? (
        <ul className="space-y-1.5">
          {artifacts.map((a) => (
            <ArtifactRow key={a.id} artifact={a} onOpen={() => setOpenId(a.id)} />
          ))}
        </ul>
      ) : (
        !error && (
          <div className="rounded-xl border border-dashed py-10 text-center">
            <p className="text-sm font-medium">No artifacts for this task yet</p>
            <p className="mx-auto mt-1 max-w-sm text-xs text-muted-foreground">
              A dispatch that finishes successfully records what the agent produced here, as a
              version you can browse and edit. Files an agent writes into the workspace are not
              captured yet — that producer is tracked separately (#244).
            </p>
          </div>
        )
      )}
    </div>
  );
}

/** One artifact in the list: title, kind, how many versions, and the edit signal. */
function ArtifactRow({ artifact, onOpen }: { artifact: ArtifactView; onOpen: () => void }) {
  const Icon = KIND_ICON[artifact.kind];
  const diff = artifact.humanEditDiff;
  return (
    <li>
      <button
        className="flex w-full items-center gap-2 rounded-lg border bg-card px-3 py-2 text-left text-xs transition-colors hover:bg-accent"
        onClick={onOpen}
      >
        <Icon className="size-3.5 shrink-0 text-muted-foreground" />
        <span className="min-w-0 flex-1 truncate font-medium">{artifact.title}</span>
        {diff && <EditedBadge diff={diff} />}
        <Badge variant="outline" className="shrink-0 font-normal capitalize">
          {artifact.kind}
        </Badge>
        <span className="shrink-0 text-[11px] text-muted-foreground">
          {artifact.versions.length} {artifact.versions.length === 1 ? "version" : "versions"}
        </span>
        <span className="shrink-0 text-[11px] tabular-nums text-muted-foreground">
          {whenOf(artifact.updatedAtMillis)}
        </span>
      </button>
    </li>
  );
}

/** The "a human changed this before approving" signal: +added / −removed and churn. */
function EditedBadge({ diff }: { diff: ArtifactDiff }) {
  return (
    <span
      className="inline-flex shrink-0 items-center gap-1.5 rounded-full bg-muted px-2 py-0.5 text-[11px] font-medium"
      title={`An operator edited v${diff.fromVersion} into v${diff.toVersion} — ${churnPercent(diff.churn)} of its lines changed`}
    >
      <UserRound className="size-3" />
      <span className="tabular-nums text-emerald-600 dark:text-emerald-400">+{diff.added}</span>
      <span className="tabular-nums text-rose-600 dark:text-rose-400">−{diff.removed}</span>
    </span>
  );
}

function ArtifactDetail({
  client,
  company,
  artifact,
  onBack,
  onAppended,
  onRefresh,
  onEditingChange,
}: {
  client: OpenCompanyClient;
  company: string | null;
  artifact: ArtifactView;
  onBack: () => void;
  onAppended: (next: ArtifactView) => void;
  onRefresh: () => void;
  onEditingChange: (editing: boolean) => void;
}) {
  const latest: ArtifactVersion | undefined = artifact.versions[artifact.versions.length - 1];
  const [selected, setSelected] = useState<number | null>(null);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const [saving, setSaving] = useState(false);

  // Follow the newest version unless the operator has pinned an older one, so a
  // re-run that appends while the panel is open shows its result rather than
  // silently leaving the reader on a stale draft.
  const shown = useMemo(() => {
    if (selected !== null) {
      const pinned = artifact.versions.find((v) => v.version === selected);
      if (pinned) return pinned;
    }
    return latest;
  }, [artifact.versions, latest, selected]);

  useEffect(() => {
    onEditingChange(editing);
    return () => onEditingChange(false);
  }, [editing, onEditingChange]);

  const editable = EDITABLE_KINDS.has(artifact.kind) && latest !== undefined;

  function startEditing() {
    setDraft(latest?.body ?? "");
    setEditing(true);
  }

  function stopEditing() {
    setEditing(false);
    setDraft("");
  }

  async function save() {
    if (!latest) return;
    setSaving(true);
    try {
      const next = await appendArtifactVersion(client, company, artifact.id, { body: draft });
      onAppended(next);
      // Pin nothing: the newly appended version is now the latest, so the
      // follow-the-newest rule lands the reader on exactly what they saved.
      setSelected(null);
      stopEditing();
      onRefresh();
      toast.success("Saved as a new version — the diff against the agent's draft is below.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not save the edit");
    } finally {
      setSaving(false);
    }
  }

  const dirty = editing && draft !== (latest?.body ?? "");
  const Icon = KIND_ICON[artifact.kind];

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2">
        <Button variant="ghost" size="sm" className="-ml-2 h-8 px-2" onClick={onBack}>
          <ArrowLeft className="mr-1.5 size-4" />
          Artifacts
        </Button>
      </div>

      <div className="rounded-xl border bg-card p-4">
        <div className="flex items-start justify-between gap-3">
          <h3 className="inline-flex min-w-0 items-center gap-2 text-sm font-semibold">
            <Icon className="size-4 shrink-0 text-muted-foreground" />
            <span className="truncate">{artifact.title}</span>
          </h3>
          <Badge variant="outline" className="shrink-0 capitalize">
            {artifact.kind}
          </Badge>
        </div>
        <p className="mt-2 text-[11px] text-muted-foreground">
          Created {whenOf(artifact.createdAtMillis)} · updated {whenOf(artifact.updatedAtMillis)}
        </p>
      </div>

      <VersionRail
        versions={artifact.versions}
        shown={shown?.version ?? null}
        onSelect={(v) => setSelected(v)}
        disabled={editing}
      />

      {editing ? (
        <div className="rounded-xl border bg-card p-3">
          <p className="mb-2 text-[11px] text-muted-foreground">
            Saving appends a new version authored by you. The agent's version is never
            overwritten — that is what keeps the diff answerable later.
          </p>
          <Textarea
            autoFocus
            value={draft}
            rows={14}
            className="font-mono text-xs"
            disabled={saving}
            onChange={(e) => setDraft(e.target.value)}
          />
          <div className="mt-2 flex items-center gap-2">
            <Button size="sm" className="h-8" disabled={saving || !dirty} onClick={() => void save()}>
              {saving && <Loader2 className="mr-1.5 size-3.5 animate-spin" />}
              Save as new version
            </Button>
            {dirty ? (
              <AlertDialog>
                <AlertDialogTrigger
                  render={
                    <Button variant="ghost" size="sm" className="h-8" disabled={saving}>
                      Cancel
                    </Button>
                  }
                />
                <AlertDialogContent>
                  <AlertDialogHeader>
                    <AlertDialogTitle>Discard this edit?</AlertDialogTitle>
                    <AlertDialogDescription>
                      Nothing has been saved yet, so your changes will be lost.
                    </AlertDialogDescription>
                  </AlertDialogHeader>
                  <AlertDialogFooter>
                    <AlertDialogCancel>Keep editing</AlertDialogCancel>
                    <AlertDialogAction
                      className="bg-destructive text-white hover:bg-destructive/90"
                      onClick={stopEditing}
                    >
                      Discard
                    </AlertDialogAction>
                  </AlertDialogFooter>
                </AlertDialogContent>
              </AlertDialog>
            ) : (
              <Button variant="ghost" size="sm" className="h-8" disabled={saving} onClick={stopEditing}>
                Cancel
              </Button>
            )}
          </div>
        </div>
      ) : (
        shown && (
          <VersionBody
            artifact={artifact}
            version={shown}
            canEdit={editable && shown.version === latest?.version}
            onEdit={startEditing}
          />
        )
      )}

      {artifact.humanEditDiff && (
        <DiffPanel
          title="What the operator changed"
          note={`v${artifact.humanEditDiff.fromVersion} (agent) → v${artifact.humanEditDiff.toVersion} (operator)`}
          diff={artifact.humanEditDiff}
        />
      )}

      {artifact.versions.length > 1 && (
        <ComparePanel client={client} company={company} artifact={artifact} />
      )}
    </div>
  );
}

/** The version browser: who wrote each revision, when, and why. */
function VersionRail({
  versions,
  shown,
  onSelect,
  disabled,
}: {
  versions: ArtifactVersion[];
  shown: number | null;
  onSelect: (version: number) => void;
  disabled: boolean;
}) {
  if (versions.length === 0) {
    return (
      <div className="rounded-xl border border-dashed py-6 text-center text-xs text-muted-foreground">
        This artifact has no versions.
      </div>
    );
  }
  return (
    <div className="rounded-xl border bg-card/40 p-3">
      <p className="mb-2 inline-flex items-center gap-1.5 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
        <History className="size-3.5" />
        Versions
      </p>
      <ol className="space-y-1.5">
        {versions.map((v) => (
          <li key={v.version}>
            <button
              className={cn(
                "flex w-full items-center gap-2 rounded-lg border bg-card px-2.5 py-1.5 text-left text-xs transition-colors",
                v.version === shown ? "border-foreground/30 bg-accent" : "hover:bg-accent",
                disabled && "cursor-not-allowed opacity-60",
              )}
              disabled={disabled}
              onClick={() => onSelect(v.version)}
            >
              <span className="shrink-0 font-medium tabular-nums">v{v.version}</span>
              <Badge
                variant="outline"
                className={cn(
                  "shrink-0 font-normal capitalize",
                  v.author === "operator" && "border-amber-500/40 text-amber-600 dark:text-amber-400",
                )}
              >
                {v.author}
              </Badge>
              <span className="min-w-0 flex-1 truncate text-muted-foreground">
                {v.note ?? v.authorId}
              </span>
              <span className="shrink-0 text-[11px] tabular-nums text-muted-foreground">
                {whenOf(v.createdAtMillis)}
              </span>
            </button>
          </li>
        ))}
      </ol>
    </div>
  );
}

/**
 * One version's content.
 *
 * `image` and `file` carry a *reference* — a URL or a workspace path — not
 * bytes, so they render as the reference plus a caption saying so rather than
 * pretending to be a preview. Neither is editable: there is no text to edit.
 */
function VersionBody({
  artifact,
  version,
  canEdit,
  onEdit,
}: {
  artifact: ArtifactView;
  version: ArtifactVersion;
  canEdit: boolean;
  onEdit: () => void;
}) {
  const reference = artifact.kind === "image" || artifact.kind === "file";
  return (
    <div className="rounded-xl border bg-card p-3">
      <div className="mb-2 flex items-center gap-2">
        <span className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
          v{version.version} · {version.author}
        </span>
        {canEdit && (
          <Button variant="outline" size="sm" className="ml-auto h-7" onClick={onEdit}>
            <Pencil className="mr-1.5 size-3.5" />
            Edit as operator
          </Button>
        )}
      </div>
      {reference ? (
        <div className="space-y-1">
          <p className="break-all rounded-lg bg-muted px-2.5 py-2 font-mono text-[11px]">
            {version.body}
          </p>
          <p className="text-[11px] text-muted-foreground">
            {artifact.kind === "image" ? "An image" : "A file"} reference — the host stores the
            location, not the bytes.
          </p>
        </div>
      ) : artifact.kind === "markdown" ? (
        <Markdown className="text-sm">{version.body}</Markdown>
      ) : (
        <pre className="whitespace-pre-wrap break-words font-mono text-xs">{version.body}</pre>
      )}
    </div>
  );
}

/** Compare any two versions — the agent-draft-vs-agent-draft case a re-run creates. */
function ComparePanel({
  client,
  company,
  artifact,
}: {
  client: OpenCompanyClient;
  company: string | null;
  artifact: ArtifactView;
}) {
  const numbers = artifact.versions.map((v) => v.version);
  const [from, setFrom] = useState(() => String(numbers[0]));
  const [to, setTo] = useState(() => String(numbers[numbers.length - 1]));
  const [diff, setDiff] = useState<ArtifactDiff | null>(null);
  const [busy, setBusy] = useState(false);

  const same = from === to;

  async function compare() {
    setBusy(true);
    try {
      setDiff(await diffArtifact(client, company, artifact.id, Number(from), Number(to)));
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not diff those versions");
    } finally {
      setBusy(false);
    }
  }

  const labels = useMemo(() => {
    const out: Record<string, string> = {};
    for (const v of artifact.versions) out[String(v.version)] = `v${v.version} · ${v.author}`;
    return out;
  }, [artifact.versions]);

  return (
    <div className="rounded-xl border bg-card/40 p-3">
      <p className="mb-2 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
        Compare versions
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <Select value={from} onValueChange={(v) => v && setFrom(String(v))} items={labels}>
          <SelectTrigger className="w-36" size="sm">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {numbers.map((n) => (
              <SelectItem key={n} value={String(n)}>
                {labels[String(n)]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <span className="text-xs text-muted-foreground">→</span>
        <Select value={to} onValueChange={(v) => v && setTo(String(v))} items={labels}>
          <SelectTrigger className="w-36" size="sm">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {numbers.map((n) => (
              <SelectItem key={n} value={String(n)}>
                {labels[String(n)]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Button size="sm" className="h-7" disabled={busy || same} onClick={() => void compare()}>
          {busy && <Loader2 className="mr-1.5 size-3.5 animate-spin" />}
          Compare
        </Button>
        {same && <span className="text-[11px] text-muted-foreground">Pick two different versions.</span>}
      </div>
      {diff && (
        <div className="mt-3">
          <DiffPanel
            title="Selected comparison"
            note={`v${diff.fromVersion} → v${diff.toVersion}`}
            diff={diff}
          />
        </div>
      )}
    </div>
  );
}

/** A rendered diff: the churn header, then the op-coloured lines. */
function DiffPanel({ title, note, diff }: { title: string; note: string; diff: ArtifactDiff }) {
  return (
    <div className="rounded-xl border bg-card">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b px-3 py-2">
        <span className="text-xs font-medium">{title}</span>
        <span className="text-[11px] text-muted-foreground">{note}</span>
        <span className="ml-auto inline-flex items-center gap-2 text-[11px] tabular-nums">
          <span className="text-emerald-600 dark:text-emerald-400">+{diff.added}</span>
          <span className="text-rose-600 dark:text-rose-400">−{diff.removed}</span>
          <span className="text-muted-foreground">{churnPercent(diff.churn)} churn</span>
        </span>
      </div>
      {diff.lines.length === 0 ? (
        <p className="px-3 py-3 text-[11px] text-muted-foreground">Both versions are empty.</p>
      ) : (
        <div className="overflow-x-auto">
          <pre className="min-w-full py-1 font-mono text-[11px] leading-relaxed">
            {diff.lines.map((line, i) => (
              <div
                key={i}
                className={cn(
                  "px-3 whitespace-pre-wrap break-words",
                  line.op === "insert" && "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300",
                  line.op === "delete" && "bg-rose-500/10 text-rose-700 dark:text-rose-300",
                  line.op === "equal" && "text-muted-foreground",
                )}
              >
                <span className="select-none opacity-60">
                  {line.op === "insert" ? "+" : line.op === "delete" ? "−" : " "}{" "}
                </span>
                {line.text}
              </div>
            ))}
          </pre>
        </div>
      )}
    </div>
  );
}
