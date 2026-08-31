// The Ledgers section: the company's own record, whatever shape it takes.
//
// # Nothing here knows what a ledger holds
//
// Every read carries the ledger's own `fields`, `statuses` and `sections`, and
// the form and the table below are built from that. So a ledger a teammate
// declared this morning — a hiring pipeline, a customer promise, an experiment
// — renders correctly this afternoon with no console release. A screen that
// hard-coded the goals columns would have made "declare your own axis" a
// promise the UI quietly broke.
//
// # Two asymmetries this screen has to show, not hide
//
// **Only a person deletes.** Teammates open, amend and close rows; the delete
// control exists here and nowhere an agent can reach. Closing is offered first
// and framed as the ordinary way to be finished with something, because a
// closed row keeps its reason and a deleted one keeps nothing.
//
// **The task board writes differently, not less.** The board IS the `tasks`
// ledger and renders here as columns, because its statuses are a lifecycle and
// that is what a lifecycle looks like. Since issue #1140 this is the *only*
// place it renders: there was a Tasks page beside Ledgers showing the same
// records through the same component, and an operator who met their work twice
// had to learn which of the two was real.
//
// What that page did differently is what this screen had to grow, and all of it
// comes from the same fact — entering a column *fires work*: the dispatch edge,
// the planning pass, the settle.
//
// * **A drop goes through `patchTask`**, not `record_entry`.
// * **Creation keeps its own dialog** (`CreateTaskDialog`, `POST …/tasks`)
//   rather than this screen's compose form, because `record_entry` is refused
//   for this ledger — so the generic form would produce a save the host will
//   not take. It is offered on the board and nowhere else, and it lands a card
//   in To-do, which is the one column new work may enter.
// * **A native board card is a `Task`, not a row.** Priority, assignee, cost,
//   plan badge, output link, and what a paused card is stopped behind are none
//   of them ledger fields — the ledger's projection deliberately carries none
//   of it — so this screen reads `…/tasks` alongside the ledger and hands
//   `TaskCard` the record. The ledger supplies the *shape* of the board; the
//   task store supplies what is *on* it.
//
// Clicking a card still leaves for `#/tasks/<id>`, which carries the timeline,
// plan, discussion and attempts this screen does not try to reproduce.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useDebouncedValue } from "@/hooks/use-debounced-value";
import { useHashFlag } from "@/hooks/use-hash-flag";
import { useLedgerViewMode, type LedgerViewMode } from "@/hooks/use-ledger-view-mode";
import { withHostParam } from "@/hooks/use-host-route";
import { DeclareListWizard } from "@/views/company/DeclareListWizard";
import {
  AlertTriangle,
  Check,
  CheckCircle2,
  ChevronDown,
  FileText,
  Columns3,
  Loader2,
  Lock,
  Rows3,
  Plus,
  RefreshCw,
  Search,
  Trash2,
} from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { listTasks, patchTask, type Task } from "@/api/tasks";
import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import { CreateTaskDialog } from "@/views/CreateTaskDialog";
import { LedgerBoard } from "@/views/LedgerBoard";
import { useAskerNames } from "@/components/approval-card";
import type { DecidedApproval } from "@/views/chat/model";
import { TaskItem } from "@/views/TaskCard";
import {
  BOARD_LEDGER,
  BOARD_WORKING,
  columnsOf,
  labelFor,
} from "@/lib/board-columns";
import { taskApprovalRows } from "@/lib/task-approvals";
import {
  byline,
  composableFields,
  composeDialogDescription,
  composeDialogTitle,
  deleteEntry,
  EVERY_STATUS,
  filteredEmptyNotice,
  isClosingStatus,
  isWritable,
  readLedger,
  recordEntry,
  renderedLedger,
  statusField,
  statusFilterLabel,
  statusNeedsReason,
  type LedgerEntry,
  type LedgerRead,
  type LedgerSummary,
} from "@/api/ledgers";
import { inlineCode } from "@/lib/inline-code";
import { Markdown } from "@/components/markdown";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
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
  /**
   * Every list this company holds, read once by `app-shell.tsx`'s
   * `useLedgerNav` and shared with the sidebar's own rows (issue #1284) —
   * this screen no longer reads `listLedgers()` on its own.
   */
  ledgers: LedgerSummary[];
  /** Whether `ledgers`'s first read is still in flight. */
  ledgersLoading: boolean;
  /** How many more lists this company may declare (`useLedgerNav`). */
  remaining: number;
  /** The list named in `#/ledgers/<slug>`, when the address carries one. */
  sub?: string | null;
  /**
   * Navigates to `#/ledgers/<slug>`.
   *
   * Only used for the one case this screen still resolves on its own: a bare
   * `#/ledgers` (a hand-typed or bookmarked address with no slug) lands on the
   * first list rather than nothing, since there is no in-page picker left to
   * choose one from (issue #1284 removed it — every list is a sidebar row
   * now). Absent, this falls back to rendering nothing rather than guessing.
   */
  onOpenLedger?: (slug: string | null) => void;
  /**
   * Opens a task card's detail screen (`#/tasks/<id>`).
   *
   * The board is the `tasks` ledger and renders here, but a *card* carries far
   * more than a ledger row — a timeline, a plan brief, a discussion, attempts, a
   * workflow proposal, steer controls. Reproducing any of that here would be a
   * worse second copy, so the board's cards link out to the screen that already
   * does it. Absent when this view is rendered with nowhere to link to, in which
   * case a card opens the ordinary amend form.
   */
  onOpenCard?: (id: string, mode: LedgerViewMode) => void;
  /**
   * A counter the shell bumps on every task event off the company SSE stream
   * (issue #464) — a card opened, moved, settled, dispatched or steered.
   *
   * The board re-reads itself whenever it changes, which is what makes *ask for
   * something in chat, watch it become work on the board* true without a
   * reload. It arrived here with the board (issue #1140); before that, the only
   * screen that could see work appear was the page this one replaced.
   *
   * There is deliberately **no interval poll** beside it. The retired screen
   * kept one at 4s as the degradation path for a host with no SSE, and a board
   * that re-renders on a timer is exactly what `board-columns.spec.ts`'s
   * shared-`DataTransfer` note warns about straddling a drag. Refresh is the
   * manual fallback, and it is on this screen already.
   */
  taskEventTick?: number;
  /**
   * The company's parked approvals (issue #883).
   *
   * A paused card is blocked until every approval its turn parked has been
   * decided, and neither the ledger's rows nor the `…/tasks` read carries them.
   * Defaults to empty, which renders exactly the pre-#883 card.
   */
  approvals?: readonly ApprovalSummary[];
  /** The feed's clock, for "blocked for 4m". Falls back to the browser's. */
  now?: number;
  /**
   * The console-wide decision state a blocked card decides through (#1891).
   *
   * The same four the shell already hands `WorkflowsView` for the run drawer,
   * owned there for the same reason: an operator who decides on the board,
   * steps over to Approvals and comes back must not find a card that forgot
   * what they did. `decided` is fed by the `approval_resolved` frame as well as
   * by this console's own resolves, which is what makes a decision taken on the
   * page settle on the board with no reload.
   *
   * All optional, and absent means a read-only board: `onDecideApproval` is
   * what a card checks before offering buttons at all.
   */
  decidingApprovals?: ReadonlyMap<string, Verdict>;
  decidedApprovals?: Readonly<Record<string, DecidedApproval>>;
  failedApprovals?: Record<string, string>;
  /** Whether a detached approval continuation is still running for a card. */
  onDecideApproval?: (
    approval: ApprovalSummary,
    verdict: Verdict,
    scope: GrantScope,
  ) => void;
  /**
   * Re-reads the shared list (`useLedgerNav.refresh`) — called after the
   * switcher's in-place wizard declares a new one, so it shows up in the
   * menu (and Manage Lists, which reads the same instance) with no reload.
   */
  onListsChanged?: () => Promise<void>;
}

/**
 * The reserved `sub` segment for Manage Lists (issue #1284): `#/ledgers/manage`,
 * checked by `app-shell.tsx` *before* it ever mounts this component, not
 * inside it — `LedgersView`'s own hooks read and write real list rows keyed
 * on `sub`, and running that machinery against a slug that names no list
 * would be all cost and no ledger. See the doc comment on `MANAGE_SEGMENT`'s
 * use in `app-shell.tsx`.
 *
 * Cheaply reserved the same way `CompanyView.DESKS_SEGMENT` is: a company
 * could in principle declare a list whose slug collides with it, which is why
 * every caller that derives a slug for a *new* list (`DeclareListWizard`)
 * excludes this word (and `NEW_SEGMENT` below) from what it will derive —
 * see `RESERVED_SEGMENTS`.
 */
export const MANAGE_SEGMENT = "manage";

/**
 * The reserved query flag for the in-place declare wizard (issue #1284):
 * `#/ledgers/<slug>?new`. Unlike `MANAGE_SEGMENT` this never reaches the
 * router at all — `useHashView`'s segment parsing strips everything from `?`
 * onward — so it coexists with whichever list is on screen underneath the
 * wizard rather than replacing it. See `hooks/use-hash-flag.ts`.
 */
export const NEW_LIST_FLAG = "new";

/** Slugs a declared list may not take — reserved for routing, not data. */
export const RESERVED_SEGMENTS: readonly string[] = [
  MANAGE_SEGMENT,
  NEW_LIST_FLAG,
];

/**
 * A stable empty default for `approvals`.
 *
 * A `[]` literal in the parameter list is a new array identity on every render,
 * and this screen re-renders on every keystroke in its search box.
 */
const EMPTY_APPROVALS: readonly ApprovalSummary[] = [];
/** Stable defaults, so a board rendered without the decide bundle does not
 *  churn every card's props on each poll (#1891). */
const EMPTY_DECIDING: ReadonlyMap<string, Verdict> = new Map();
const EMPTY_DECIDED: Readonly<Record<string, DecidedApproval>> = {};
const EMPTY_FAILED: Record<string, string> = {};

/** The task ledger is operated as a board; declared ledgers are read as rows. */
export function defaultLedgerMode(
  ledger: LedgerSummary | null,
): "board" | "list" {
  return ledger?.source === "native" && ledger.slug === BOARD_LEDGER
    ? "board"
    : "list";
}

/** A row is either being opened fresh or amended; the form differs only in id. */
interface Composing {
  /** The row this edits, or empty for a new one. */
  id: string;
  fields: Record<string, string>;
  status: string;
  /** True when the form was opened by "Close" rather than "Record". */
  closing: boolean;
}

export function LedgersView({
  client,
  company,
  ledgers,
  ledgersLoading,
  remaining,
  sub,
  onOpenLedger,
  onOpenCard,
  taskEventTick,
  approvals = EMPTY_APPROVALS,
  now,
  decidingApprovals = EMPTY_DECIDING,
  decidedApprovals = EMPTY_DECIDED,
  failedApprovals = EMPTY_FAILED,
  onDecideApproval,
  onListsChanged,
}: Props) {
  // The declare wizard, opened in place over whatever list is already on
  // screen (issue #1284): riding the hash's query suffix rather than plain
  // `useState` is what makes the browser Back button close it, instead of
  // bouncing two levels out past both the wizard and Manage Lists.
  const [declaring, setDeclaring] = useHashFlag(NEW_LIST_FLAG);
  const [read, setRead] = useState<LedgerRead | null>(null);
  const [reading, setReading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  // Issue #1696: the read effect keys on `refreshRead`, whose identity turns
  // over on every `query` change. Feeding the raw box to it fired one network
  // read per keystroke; feeding this settled copy fires one per typing pause.
  // The <Input> stays bound to raw `query` so typing still echoes instantly.
  const debouncedQuery = useDebouncedValue(query, 300);
  const [statusFilter, setStatusFilter] = useState(EVERY_STATUS);
  const [composing, setComposing] = useState<Composing | null>(null);
  const [saving, setSaving] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<LedgerEntry | null>(null);
  const [rendered, setRendered] = useState<string | null>(null);
  /**
   * The board's own create dialog, for the native `tasks` ledger only.
   *
   * Every other ledger takes `record_entry`, which this screen's compose form
   * already is. The board does not — entering a column fires work — so opening
   * a card keeps its own `POST …/tasks` path and its own dialog, which is the
   * one lifted out of the retired board screen rather than rewritten here.
   */
  const [creatingCard, setCreatingCard] = useState(false);
  /**
   * The `Task` behind each row of the native board (issue #1140).
   *
   * The ledger read says which cards exist and which column each is in; this
   * says what is *on* them — the priority, the assignee, the cost, the plan, the
   * output, and whether the card is paused and what it is stopped behind. The
   * ledger's projection carries none of that by design
   * (`src/ledger/native.rs`), so the two reads are joined by id here rather
   * than the projection being widened to carry a second copy of the task store.
   *
   * Empty for every other ledger, whose rows have no `Task` behind them at all.
   */
  const [tasks, setTasks] = useState<Task[]>([]);

  const ledger = useMemo(
    () => (sub ? (ledgers.find((held) => held.slug === sub) ?? null) : null),
    [ledgers, sub],
  );

  /**
   * Columns or rows.
   *
   * The task ledger is operated as a board; declared ledgers read as rows
   * (`defaultLedgerMode`). The choice is navigation state (`?view=list`) so a
   * browser Back returns to the rendering the operator left — and the
   * per-ledger default above applies only when the address names no view, which
   * is what the fallback argument to `useLedgerViewMode` means here.
   */
  const [mode, setMode] = useLedgerViewMode(defaultLedgerMode(ledger));

  // A bare `#/ledgers` (no slug — the nav row's own address, a hand-typed
  // one, or a bookmark) lands on Tasks, not "whichever list happened to load
  // first" — Tasks is the hero content this screen defaults to (issue
  // #1284), and the switcher on the title is what reaches everything else.
  useEffect(() => {
    if (!sub && !ledgersLoading && ledgers.length > 0) {
      const board = ledgers.find((held) => held.slug === BOARD_LEDGER);
      onOpenLedger?.(board?.slug ?? ledgers[0].slug);
    }
  }, [sub, ledgersLoading, ledgers, onOpenLedger]);

  /** The task board, as opposed to a ledger a company declared. */
  const isBoard = ledger?.source === "native" && ledger.slug === BOARD_LEDGER;

  const taskById = useMemo(
    () => new Map(tasks.map((task) => [task.id, task])),
    [tasks],
  );

  /**
   * Ids the compose dialog can currently confirm exist (issue #1264).
   *
   * `read.entries` is server-filtered by the search box and status filter, so
   * this can under-report — a row hidden by the current filter or truncated
   * past the fetched page reads as "does not exist" here. Accepted: this is a
   * display-copy fix for the dialog title, not a data-fetching change, and the
   * common case (a brand-new row's id typed for the first time) never matches
   * anything anyway.
   */
  const existingIds = useMemo(
    () => new Set((read?.entries ?? []).map((entry) => entry.id)),
    [read],
  );

  /** The clock the "blocked since" labels measure against (issue #883). */
  const clock = now ?? Date.now();

  /**
   * Who asked for each parked approval, for the blocked cards (#1891).
   *
   * At the view rather than per card: `useAskerNames` keys on the *set* of
   * asker ids, so one call over the company's queue is a single roster read
   * however many columns hold blocked cards — and a card that mounts mid-poll
   * finds the names already resolved instead of starting its own fetch.
   */
  const askerNames = useAskerNames(client, company, [...approvals]);

  /**
   * Why this ledger is showing nothing, when a filter is the reason (#1217).
   * `null` when nothing is filtering, which is the only case where "Nothing
   * recorded here yet" is a true sentence.
   */
  const emptyNotice = filteredEmptyNotice(ledger, query, statusFilter);

  /** Put the ledger back to showing everything it holds. */
  const clearFilters = useCallback(() => {
    setQuery("");
    setStatusFilter(EVERY_STATUS);
  }, []);

  /**
   * Re-reads the open list.
   *
   * `quiet` suppresses the "Reading…" line, and exists for the re-reads the
   * operator did not ask for — the ones an SSE event triggers. Flashing a
   * spinner every time an agent somewhere moved a card would report the
   * host's activity as this screen's slowness.
   */
  const refreshRead = useCallback(
    async (quiet = false) => {
      if (!company || !sub) {
        setRead(null);
        return;
      }
      if (!quiet) setReading(true);
      try {
        const next = await readLedger(client, company, sub, {
          q: debouncedQuery.trim() || undefined,
          status: statusFilter === EVERY_STATUS ? undefined : statusFilter,
          limit: 100,
        });
        setRead(next);
        setError(null);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        if (!quiet) setReading(false);
      }
    },
    [client, company, sub, debouncedQuery, statusFilter],
  );

  useEffect(() => {
    void refreshRead();
  }, [refreshRead]);

  /**
   * Reads the `Task` records behind the native board's rows.
   *
   * Deliberately silent about its own failure. The rows are already on screen —
   * they come from the ledger read, which has its own error surface — so a
   * `…/tasks` read that fails costs the cards their decoration, not their
   * existence. Raising a second error banner for it would report the board as
   * broken when it is merely plainer.
   */
  const refreshTasks = useCallback(async () => {
    if (!company || !isBoard) {
      setTasks([]);
      return;
    }
    try {
      setTasks(await listTasks(client, company));
    } catch {
      // Leave whatever is already held: a stale decoration beats none.
    }
  }, [client, company, isBoard]);

  useEffect(() => {
    void refreshTasks();
  }, [refreshTasks]);

  // Issue #464: the push half. Re-read the moment the host says something on
  // the board moved, rather than not at all — this screen has no timer.
  //
  // The tick is not consumed while the tab is hidden: marking it seen there
  // would drop the event, since this effect does not re-run on a visibility
  // change. A tab coming back re-reads on the next one.
  const seenTaskTick = useRef(taskEventTick);
  useEffect(() => {
    if (taskEventTick === undefined || taskEventTick === seenTaskTick.current) {
      return;
    }
    if (document.visibilityState === "hidden") return;
    seenTaskTick.current = taskEventTick;
    void refreshRead(true);
    void refreshTasks();
  }, [taskEventTick, refreshRead, refreshTasks]);

  // The status filter is per list, so switching lists must clear it —
  // otherwise the new list reads as empty under a filter it does not declare.
  // `ledger` is derived straight from `sub` (no local "selected" state to
  // follow it any more — issue #1284 removed the in-page picker that used to
  // own that), so this keys on `sub` directly.
  useEffect(() => {
    setStatusFilter(EVERY_STATUS);
    setRendered(null);
  }, [sub]);

  const save = async () => {
    if (!company || !ledger || !composing) return;
    const id = composing.id.trim();
    if (!id) {
      toast.error("A row needs an id.");
      return;
    }
    setSaving(true);
    try {
      await recordEntry(client, company, ledger.slug, {
        id,
        fields: composing.fields,
        status: composing.status || undefined,
      });
      setComposing(null);
      await refreshRead();
      toast.success(`Recorded ${id}.`);
    } catch (e) {
      // The host's refusals are written to be read — an unknown status names
      // the real ones, a silent close names the missing reason — so they are
      // shown verbatim rather than replaced with a generic failure.
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  /**
   * Moves a row into `status` — the drop half of the board.
   *
   * Two writes, one gesture, and the split is the whole reason the board can
   * live here at all. A native ledger's rows are the task board's, and entering
   * a column *fires work*: the dispatch edge, the planning pass, the settle. So
   * that write goes through `patchTask`, exactly as dragging on the old board
   * did. Every other ledger takes the ordinary `record_entry` merge.
   *
   * A status that demands a reason does not write at all — it opens the compose
   * form pre-filled instead, because the host refuses a silent close and asking
   * first is the same rule met before it bites.
   *
   * **Optimistic, and reverted out loud.** The card moves under the pointer and
   * snaps back if the host refuses, and the message then carries the whole
   * story: which card, where it was going, and the host's own words for why it
   * would not go. This screen validates no status itself — the declaration is
   * the host's — so the reason for a refusal only ever exists in the response,
   * and a card that silently returned to where it started is the failure issue
   * #334 was reported as.
   *
   * **A dispatch says so.** Dropping a card into In progress hands it to its
   * assignee and spends an agent turn, and the same drop out of Paused resumes
   * a run rather than starting one — two different things happening, neither of
   * which looks different from an ordinary reordering unless the console says
   * which. The retired board screen said it, and this one lost the words on the
   * way over (issue #1140).
   */
  const move = async (entry: LedgerEntry, status: string) => {
    if (!company || !ledger || entry.status === status) return;
    if (statusNeedsReason(ledger, status) && !entry.fields.reason?.trim()) {
      setComposing({
        id: entry.id,
        fields: { ...entry.fields },
        status,
        closing: true,
      });
      return;
    }
    const was = entry.status;
    const restage = (to: string) =>
      setRead((current) =>
        current
          ? {
              ...current,
              entries: current.entries.map((held) =>
                held.id === entry.id
                  ? { ...held, status: to, closed: isClosingStatus(ledger, to) }
                  : held,
              ),
            }
          : current,
      );
    restage(status);
    try {
      if (ledger.source === "native") {
        await patchTask(client, company, entry.id, { column: status });
        // `working` is the phase word the host resolves to `in_progress`, which
        // is what dispatches (issue #1512). The stage the card was in — paused,
        // or never started — comes off the `Task` record rather than off the
        // row's status, which is now the phase and therefore says nothing about
        // which of the two happened.
        if (status === BOARD_WORKING) {
          toast.success(
            taskById.get(entry.id)?.stage === "paused"
              ? "Resumed — the assignee is working on it."
              : "Dispatched — the assignee is working on it.",
          );
          // The turn runs server-side. The SSE tick brings its result, but a
          // host with no push has only this, so read once more after the turn
          // has had a moment rather than leaving the card looking untouched.
          setTimeout(() => {
            void refreshRead(true);
            void refreshTasks();
          }, 1500);
        }
      } else {
        await recordEntry(client, company, ledger.slug, {
          id: entry.id,
          status,
        });
      }
      await Promise.all([refreshRead(), refreshTasks()]);
    } catch (e) {
      restage(was);
      const label = labelFor(columnsOf(ledger), status);
      toast.error(`Could not move "${entry.title || entry.id}" to ${label}.`, {
        description:
          e instanceof Error ? e.message : "the host refused the move",
      });
    }
  };

  /**
   * Re-dispatch a paused card (issue #1512).
   *
   * Not `move`, which is right for a drag and wrong here: a paused card is
   * already in the `working` phase, so `move` would see the row's status
   * unchanged and return without doing anything. The gesture is not "put this
   * in a different column" — it is "run it again", and the write that means
   * that is the same PATCH a drop into Working sends.
   */
  const resume = async (entry: LedgerEntry) => {
    if (!company || !ledger) return;
    try {
      await patchTask(client, company, entry.id, { column: BOARD_WORKING });
      toast.success("Resumed — the assignee is working on it.");
      await Promise.all([refreshRead(), refreshTasks()]);
    } catch (e) {
      toast.error(`Could not resume "${entry.title || entry.id}".`, {
        description:
          e instanceof Error ? e.message : "the host refused the move",
      });
    }
  };

  const remove = async () => {
    if (!company || !ledger || !confirmDelete) return;
    try {
      await deleteEntry(client, company, ledger.slug, confirmDelete.id);
      setConfirmDelete(null);
      await refreshRead();
      toast.success(`Deleted ${confirmDelete.id}.`);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  const showRendered = async () => {
    if (!company || !ledger) return;
    try {
      setRendered(await renderedLedger(client, company, ledger.slug));
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  /*
    The page's name before its switcher can exist (codex review, #1785). Both
    guards below used to return above the header, so a fresh `#/ledgers/<slug>`
    and a console with no company selected each rendered with no `h1` at all.

    A plain title rather than the real header: the loaded one *is* the list
    switcher, and a switcher with no list to switch between is a control that
    lies. `ledger?.title` is used when it is already known — the second guard
    fires while the rows load, by which point the list itself has resolved —
    and "Lists" when it is not, which is the honest name for a page that does
    not yet know which one it is showing.
  */
  // Same gutter as the loaded header below, so the title does not shift 8px
  // sideways the moment the board arrives.
  const loadingHeader = <PageHeader gutter="px-6" title={ledger?.title ?? "Lists"} />;

  if (!company) {
    return (
      <div className="flex h-full min-h-0 flex-col">
        {loadingHeader}
        <div className="p-6 text-sm text-muted-foreground">
          Pick a company to see its lists.
        </div>
      </div>
    );
  }

  // Loading covers two different waits — this screen's own row read, and (only
  // while no `ledger` has resolved yet) the shared list read `app-shell.tsx`
  // owns — so a fresh `#/ledgers/<slug>` load shows the same skeleton it always
  // did rather than a flash of "not found" before the list catches up.
  if ((ledgersLoading && !ledger) || (reading && !read)) {
    return (
      <div className="flex h-full min-h-0 flex-col">
        {loadingHeader}
        <div className="space-y-3 p-6">
          <Skeleton className="h-8 w-48" />
          <Skeleton className="h-40 w-full" />
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/*
        Issue #1763: the page header, with the list switcher as the title.

        The title is still a real level-one heading — the interactive affordance is the
        `<button>` inside it, not the heading (issue #1284) — and the count
        rides beside it, which is the shape `PageHeader` gives every page.

        "About this list" goes to `children` rather than `description`: it is
        a `<details>`, and the description renders into a `<p>`.
      */}
      <PageHeader
        // The body below is `p-6`, so the header's default `px-4` put the title
        // and actions 8px outside the board content they belong to. Before this
        // page had a `PageHeader` at all its heading lived inside that same
        // wrapper and lined up; `ManageListsView` passes the same gutter for the
        // same reason.
        gutter="px-6"
        title={
          <DropdownMenu>
              <DropdownMenuTrigger
                render={
                  <button
                    type="button"
                    data-testid="list-switcher-trigger"
                    className="group -ml-1 flex items-center gap-1 rounded-md px-1 py-0.5 outline-none hover:bg-accent/50 focus-visible:ring-2 focus-visible:ring-ring"
                  >
                    {ledger?.title ?? "Not found"}
                    <ChevronDown className="size-4 shrink-0 text-muted-foreground transition-transform group-data-[popup-open]:rotate-180" />
                  </button>
                }
              />
              <DropdownMenuContent
                align="start"
                className="max-h-[60vh] w-56 overflow-y-auto"
              >
                {ledgers.map((held) => (
                  <DropdownMenuItem
                    key={held.slug}
                    data-testid={`list-switcher-${held.slug}`}
                    onClick={() => onOpenLedger?.(held.slug)}
                  >
                    <Check
                      className={cn(
                        "size-4",
                        held.slug === ledger?.slug
                          ? "opacity-100"
                          : "opacity-0",
                      )}
                    />
                    <span className="flex-1 truncate">{held.title}</span>
                    {/* Always rendered, zero included (issue #1284): a count
                        that only shows up for a positive number reads as "no
                        count information" for the zero case, not as "zero" —
                        the same ambiguity `filteredEmptyNotice` exists to
                        rule out elsewhere in this file. */}
                    <span className="text-xs text-muted-foreground">
                      {held.open}
                    </span>
                  </DropdownMenuItem>
                ))}
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  data-testid="list-switcher-new"
                  disabled={remaining <= 0}
                  title={
                    remaining <= 0
                      ? "This company is at the list cap. Retire one nothing reads first."
                      : undefined
                  }
                  onClick={() => {
                    if (remaining > 0) setDeclaring(true);
                  }}
                >
                  <Plus className="size-4" />
                  New list
                </DropdownMenuItem>
                <DropdownMenuItem
                  data-testid="list-switcher-manage"
                  onClick={() => onOpenLedger?.(MANAGE_SEGMENT)}
                >
                  Manage lists
                </DropdownMenuItem>
              </DropdownMenuContent>
          </DropdownMenu>
        }
        /* How much is live here, beside the name — the one fact about a list
           that changes, and the one the switcher already shows for every
           *other* list while saying nothing about the open one. */
        trailing={
          ledger && (
            <span
              title={`${ledger.open} open`}
              className="rounded-full bg-muted px-2 py-0.5 text-xs font-medium tabular-nums text-muted-foreground"
            >
              {ledger.open}
            </span>
          )
        }
        actions={
          <>
          <Button
            variant="outline"
            size="sm"
            onClick={() => {
              void refreshRead();
              // The board's cards are decorated from a second read, and this
              // button is the whole manual fallback on a screen with no timer.
              void refreshTasks();
            }}
          >
            <RefreshCw className="mr-2 size-4" />
            Refresh
          </Button>
          {/* The primary action sits on the title row, not in the filter bar
              below it (issue #1349). Down there it was the fourth of four
              buttons in one flat row, at the same weight as a view toggle and
              a file viewer — three different kinds of control, one of which
              opens work and two of which change how you are looking at it. The
              reference puts it here, beside the name and the count, and it is
              right: this is the row about the list, that one is about the
              view. */}
          {ledger &&
            (isWritable(ledger) ? (
              <Button
                size="sm"
                onClick={() =>
                  setComposing({
                    id: "",
                    fields: {},
                    status: ledger.statuses[0]?.name ?? "",
                    closing: false,
                  })
                }
              >
                <Plus className="mr-2 size-4" />
                Record
              </Button>
            ) : (
              ledger.slug === BOARD_LEDGER && (
                <Button size="sm" onClick={() => setCreatingCard(true)}>
                  <Plus className="mr-2 size-4" />
                  Add task
                </Button>
              )
            ))}
          </>
        }
      >
          {!ledger && (
            <p className="text-sm text-muted-foreground">
              This list does not exist, or was retired. Pick another from the
              title menu.
            </p>
          )}
          {ledger && (
            // The purpose lives in here now (issue #1349). It is a fixed
            // sentence about the engine — "this ledger is written through the
            // board, not with `record_entry`" — written for whoever declares a
            // list, not for the operator working one, and it was holding two
            // permanent lines above the board on the console's most-visited
            // screen. Read once, then never again; a disclosure is what that
            // shape of text is for.
            <details className="text-xs text-muted-foreground">
              <summary className="w-fit cursor-pointer select-none rounded px-1 py-0.5 hover:bg-accent/50 hover:text-foreground">
                About this list
              </summary>
              <div className="mt-1 max-w-prose space-y-1 px-1">
                <p>{inlineCode(ledger.purpose)}</p>
                <p>
                  Renders into <code>{ledger.derived}</code>
                </p>
                {!isWritable(ledger) && (
                  <p className="flex items-start gap-1.5">
                    <Lock className="mt-0.5 size-3 shrink-0" />
                    <span>
                      Rows here are opened elsewhere:{" "}
                      {inlineCode(ledger.writtenBy)}. You can still move one
                      between columns — that goes through the board, which is
                      what makes it start work.
                    </span>
                  </p>
                )}
              </div>
            </details>
          )}
      </PageHeader>
      <div className="flex min-h-0 flex-1 flex-col gap-4 p-6">
      {error && (
        <Alert variant="destructive">
          <AlertTriangle className="size-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <div className="flex min-h-0 flex-1 gap-4">
        <section className="flex min-h-0 min-w-0 flex-1 flex-col gap-3">
          {!ledger ? null : (
            <>
              <div className="flex flex-wrap items-center gap-2">
                <div className="relative min-w-[12rem] flex-1">
                  <Search className="pointer-events-none absolute left-2 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
                  <Input
                    className="pl-8"
                    aria-label="Search ledger entries"
                    placeholder="Search every field"
                    value={query}
                    onChange={(e) => setQuery(e.target.value)}
                  />
                </div>
                <Select
                  value={statusFilter}
                  onValueChange={(value) =>
                    setStatusFilter(value ?? EVERY_STATUS)
                  }
                >
                  {/* Explicit trigger text, for the reason issue #813 gave the
                      destination picker its own: base-ui renders the stored
                      value in the collapsed control unless told otherwise, so
                      this read `all` while the list under it read "Every
                      status", and a chosen board column read `in_progress`
                      beside columns headed "In progress". */}
                  <SelectTrigger className="w-[12rem]" aria-label="Filter by status">
                    <SelectValue>
                      {statusFilterLabel(ledger, statusFilter)}
                    </SelectValue>
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value={EVERY_STATUS}>Every status</SelectItem>
                    {ledger.statuses.map((status) => (
                      <SelectItem key={status.name} value={status.name}>
                        {statusFilterLabel(ledger, status.name)}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setMode(mode === "board" ? "list" : "board")}
                  title={
                    mode === "board"
                      ? "Show every field on each row"
                      : "Group rows by status"
                  }
                >
                  {mode === "board" ? (
                    <Rows3 className="mr-2 size-4" />
                  ) : (
                    <Columns3 className="mr-2 size-4" />
                  )}
                  {mode === "board" ? "List" : "Board"}
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => void showRendered()}
                >
                  <FileText className="mr-2 size-4" />
                  Rendered file
                </Button>
                {/* The primary action moved up to the title row (issue
                    #1349): this row is the view, that one is the list. */}
                {/* Retiring a list moved to Manage Lists (issue #1284),
                    reached from the Company page — the same place it is
                    declared. This screen is about a list's rows now, never
                    about whether the list itself continues to exist. */}
              </div>

              {reading && (
                <p className="flex items-center gap-2 text-sm text-muted-foreground">
                  <Loader2 className="size-4 animate-spin" />
                  Reading…
                </p>
              )}

              {/* Issue #1217: zero rows back means "no matches" and "no rows"
                  indistinguishably — the read is filtered server-side. The two
                  are not the same claim, and saying the stronger one over a
                  ledger the nav is counting 14 rows for is simply false. */}
              {mode === "list" &&
                read &&
                read.entries.length === 0 &&
                !reading &&
                (emptyNotice ? (
                  <div
                    className="flex flex-wrap items-center gap-2 text-sm text-muted-foreground"
                    data-testid="ledger-filtered-empty"
                  >
                    <span>{emptyNotice}</span>
                    <Button size="sm" variant="outline" onClick={clearFilters}>
                      Clear filters
                    </Button>
                  </div>
                ) : (
                  <p
                    className="text-sm text-muted-foreground"
                    data-testid="ledger-empty"
                  >
                    Nothing recorded here yet.
                  </p>
                ))}

              {mode === "board" && read ? (
                <BoardMode
                  ledger={ledger}
                  entries={read.entries}
                  onMove={(entry, status) => void move(entry, status)}
                  // A drop on dead board pixels. Saying so is the whole fix:
                  // silence here is indistinguishable from a frozen app.
                  onMiss={() =>
                    toast.error("Drop the card on a column to move it.")
                  }
                  onOpen={(entry) =>
                    ledger.source === "native" && onOpenCard
                      ? onOpenCard(entry.id, mode)
                      : setComposing({
                          id: entry.id,
                          fields: { ...entry.fields },
                          status: entry.status,
                          closing: false,
                        })
                  }
                  // The `Task` behind a native board row, when there is one.
                  // Absent for every declared ledger, whose rows are rows.
                  taskFor={
                    isBoard ? (entry) => taskById.get(entry.id) : undefined
                  }
                  approvals={approvals}
                  now={clock}
                  // #1891: a blocked card decides in place, through the shell's
                  // one resolve. The same four the run drawer already receives.
                  askerNames={askerNames}
                  deciding={decidingApprovals}
                  decided={decidedApprovals}
                  failed={failedApprovals}
                  onDecide={onDecideApproval}
                  // Resume has its own write since #1512: a paused card is
                  // already in the `working` phase, so routing it through
                  // `move` would be a no-op the operator could not see fail.
                  onResume={(entry) => void resume(entry)}
                />
              ) : (
                <div className="min-h-0 flex-1 space-y-2 overflow-y-auto">
                  {read?.entries.map((entry) => (
                    <EntryCard
                      key={entry.id}
                      entry={entry}
                      ledger={ledger}
                      onOpen={
                        ledger.source === "native" && onOpenCard
                          ? () => onOpenCard(entry.id, mode)
                          : undefined
                      }
                      detailHref={
                        ledger.source === "native"
                          ? withHostParam(`tasks/${encodeURIComponent(entry.id)}`, {
                              view: "list",
                            })
                          : undefined
                      }
                      onAmend={() =>
                        setComposing({
                          id: entry.id,
                          fields: { ...entry.fields },
                          status: entry.status,
                          closing: false,
                        })
                      }
                      onClose={() =>
                        setComposing({
                          id: entry.id,
                          fields: { ...entry.fields },
                          status:
                            ledger.statuses.find((s) => s.closed)?.name ?? "",
                          closing: true,
                        })
                      }
                      onDelete={() => setConfirmDelete(entry)}
                    />
                  ))}
                </div>
              )}

              {read && read.matched > read.entries.length && (
                <p className="text-xs text-muted-foreground">
                  Showing {read.entries.length} of {read.matched}. Narrow with
                  the search or the status filter — the rest are not gone.
                </p>
              )}

              {/* One row, not one per fault (issue #1347).
                  
                  A ledger whose rows are written by agents produces faults in
                  batches — a required field one pass forgot is missing from
                  every row that pass wrote — so this was seven full-width
                  alerts, 364px of the same sentence with a different id in it,
                  stacked below the board and out-weighing the content they are
                  about. The count is the whole headline; the ids are what you
                  open when you go looking. */}
              {read?.faults && read.faults.length > 0 && (
                <Alert data-testid="ledger-faults">
                  <AlertTriangle className="size-4" />
                  <AlertDescription>
                    <details>
                      <summary className="w-fit cursor-pointer select-none">
                        {read.faults.length === 1
                          ? "1 row could not be read"
                          : `${read.faults.length} rows could not be read`}
                      </summary>
                      <ul className="mt-2 space-y-1">
                        {read.faults.map((fault) => (
                          <li key={fault}>{inlineCode(fault)}</li>
                        ))}
                      </ul>
                    </details>
                  </AlertDescription>
                </Alert>
              )}
            </>
          )}
        </section>
      </div>
      </div>

      {composing && ledger && (
        <ComposeDialog
          ledger={ledger}
          composing={composing}
          existingIds={existingIds}
          saving={saving}
          onChange={setComposing}
          onCancel={() => setComposing(null)}
          onSave={() => void save()}
        />
      )}

      {confirmDelete && (
        <Dialog open onOpenChange={() => setConfirmDelete(null)}>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>Delete {confirmDelete.id}?</DialogTitle>
              <DialogDescription>
                This removes the row and everything ever recorded against it.
                Nothing can bring it back, and no teammate can do this — they
                close a row instead, which keeps the reason. Close it rather
                than delete it unless it should never have existed.
              </DialogDescription>
            </DialogHeader>
            <DialogFooter>
              <Button variant="ghost" onClick={() => setConfirmDelete(null)}>
                Cancel
              </Button>
              <Button variant="destructive" onClick={() => void remove()}>
                <Trash2 className="mr-2 size-4" />
                Delete permanently
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}

      {ledger && (
        <CreateTaskDialog
          open={creatingCard}
          onClose={() => setCreatingCard(false)}
          onCreated={() => {
            setCreatingCard(false);
            void refreshRead();
            // …and the `Task` behind the row that just appeared, or the new
            // card renders as a bare ledger row until something else re-reads.
            void refreshTasks();
          }}
          client={client}
          company={company}
        />
      )}

      {declaring && company && (
        <DeclareListWizard
          client={client}
          company={company}
          existingSlugs={[...ledgers.map((l) => l.slug), ...RESERVED_SEGMENTS]}
          remaining={remaining}
          onCancel={() => setDeclaring(false)}
          onCreated={async (created) => {
            setDeclaring(false);
            await onListsChanged?.();
            toast.success(`Declared ${created.title}.`);
            onOpenLedger?.(created.slug);
          }}
        />
      )}

      {rendered !== null && ledger && (
        <Dialog open onOpenChange={() => setRendered(null)}>
          <DialogContent className="max-h-[80vh] sm:max-w-3xl overflow-y-auto">
            <DialogHeader>
              <DialogTitle>{ledger.derived}</DialogTitle>
              <DialogDescription>
                Written by the runtime on every write to this ledger. Editing it
                in the workspace is refused — the next write would erase the
                edit without saying so.
              </DialogDescription>
            </DialogHeader>
            <Markdown>{rendered}</Markdown>
          </DialogContent>
        </Dialog>
      )}
    </div>
  );
}

/**
 * A ledger as columns, over the shared board.
 *
 * Everything about the *gesture* — the columns, the counts, the edge scroll, the
 * miss, the gutter — is [`LedgerBoard`](./LedgerBoard)'s. What is here is only
 * the **card**, and the card is where the two kinds of board genuinely differ:
 * a task carries a priority, an assignee, a cost, a plan badge and a Resume
 * button off its `Task` record, and a ledger row carries whatever fields its
 * declaration named. So there are two, chosen by whether `taskFor` finds a
 * record — never one renderer built from field roles, which was tried and is
 * wrong for the reasons `LedgerBoard`'s header sets out.
 *
 * This is the second attempt at the board itself. The first re-implemented it
 * inline and lost all three of issue #334's fixes on the way, which is exactly
 * the failure a shared component exists to prevent.
 */
function BoardMode({
  ledger,
  entries,
  onMove,
  onMiss,
  onOpen,
  taskFor,
  approvals,
  now,
  askerNames,
  deciding,
  decided,
  failed,
  onDecide,
  onResume,
}: {
  ledger: LedgerSummary;
  entries: LedgerEntry[];
  onMove: (entry: LedgerEntry, status: string) => void;
  onMiss: () => void;
  onOpen: (entry: LedgerEntry) => void;
  /**
   * The `Task` behind a row, on the native board (issue #1140).
   *
   * Absent for a declared ledger, and allowed to return nothing for a row on
   * the native board too: the ledger read and the `…/tasks` read land
   * separately, so a card exists here for a moment before its record does. It
   * renders as a ledger row until then, which is thinner than it will be but
   * never absent — a card that waited for its decoration would flicker in and
   * out of the column it is in.
   */
  taskFor?: (entry: LedgerEntry) => Task | undefined;
  /** The company's parked approvals, for the paused card's reason (issue #883). */
  approvals?: readonly ApprovalSummary[];
  /** The clock those "blocked for 4m" labels measure against. */
  now?: number;
  /**
   * Roster ids → names, resolved once for the whole board (#1891).
   *
   * Lifted to the view rather than run per card: `useAskerNames` keys on the
   * set of asker ids, so one call over the company's queue is one roster read
   * for every blocked column — and a card that mounts mid-poll finds the names
   * already in hand instead of starting its own fetch.
   */
  askerNames: Map<string, string>;
  /** The console-wide decision state a blocked card decides through (#1891). */
  deciding: ReadonlyMap<string, Verdict>;
  decided: Readonly<Record<string, DecidedApproval>>;
  failed: Record<string, string>;
  /** The shell's one resolve — absent when this board is read-only. */
  onDecide?: (approval: ApprovalSummary, verdict: Verdict, scope: GrantScope) => void;
  /** Re-dispatch a paused card. */
  onResume?: (entry: LedgerEntry) => void;
}) {
  return (
    <LedgerBoard
      columns={columnsOf(ledger)}
      rows={entries}
      statusOf={(entry) => entry.status}
      onMove={onMove}
      onMiss={onMiss}
      renderCard={(entry, dragging) => {
        const task = taskFor?.(entry);
        if (task) {
          return (
            <TaskItem
              task={task}
              dragging={dragging}
              rows={taskApprovalRows(approvals ?? [], decided, task.id)}
              now={now ?? Date.now()}
              askerNames={askerNames}
              deciding={deciding}
              failed={failed}
              onDecide={onDecide}
              onOpen={() => onOpen(entry)}
              onResume={() => onResume?.(entry)}
            />
          );
        }
        return (
          <button
            type="button"
            onClick={() => onOpen(entry)}
            className={cn(
              "w-full cursor-grab rounded-lg border bg-card p-3 text-left shadow-sm transition-shadow hover:shadow active:cursor-grabbing",
              dragging && "opacity-50",
            )}
          >
            <span className="line-clamp-2 text-sm font-medium leading-snug">
              {entry.title || entry.id}
            </span>
            {/* One line beneath the title, not every field: a board is scanned,
                and a card carrying all of them is a list with extra steps. The
                list mode is where a row is read.

                `data-owner` is set only when the ledger declares an owner field
                AND this row filled it in — so "unassigned" is machine-checkable
                rather than inferred from the absence of a span, which the id
                fallback would otherwise occupy. */}
            <span
              className="mt-1 block truncate text-xs text-muted-foreground"
              {...(ownerOf(entry, ledger)
                ? { "data-owner": ownerOf(entry, ledger) }
                : {})}
            >
              {ownerOf(entry, ledger) || entry.id}
            </span>
            {entry.closed && entry.fields.reason?.trim() && (
              <span className="mt-2 block line-clamp-3 text-xs leading-relaxed text-muted-foreground">
                {entry.fields.reason}
              </span>
            )}
          </button>
        );
      }}
    />
  );
}

/**
 * Who owns a row, if the ledger declares an owner field and the row filled it
 * in. Empty otherwise — the card then falls back to its id.
 *
 * Its **id**, and not the first prose field. A board card has to be
 * identifiable at a glance so somebody can say which one they mean, and a
 * truncated sentence of detail identifies nothing.
 */
function ownerOf(entry: LedgerEntry, ledger: LedgerSummary): string {
  const owner = ledger.fields.find((field) => field.role === "owner");
  return (owner ? entry.fields[owner.name]?.trim() : "") ?? "";
}

function EntryCard({
  entry,
  ledger,
  onOpen,
  detailHref,
  onAmend,
  onClose,
  onDelete,
}: {
  entry: LedgerEntry;
  ledger: LedgerSummary;
  /** Opens the row's own screen, when it has one. Only native Tasks do. */
  onOpen?: () => void;
  /** Native task-detail address, including the List return context. */
  detailHref?: string;
  onAmend: () => void;
  onClose: () => void;
  onDelete: () => void;
}) {
  const writable = isWritable(ledger);
  const columns = columnsOf(ledger);
  return (
    <Card
      className={cn(
        onOpen && "cursor-pointer transition-colors hover:bg-accent/50",
        entry.closed && "opacity-75",
      )}
      onClick={(event) => {
        if (!(event.target as HTMLElement).closest("a, button")) onOpen?.();
      }}
    >
      <CardContent
        className="space-y-2"
        data-testid={`ledger-entry-${entry.id}`}
      >
        <div className="flex flex-wrap items-start gap-2">
          {onOpen ? (
            <a
              href={detailHref}
              className="min-w-0 flex-1 text-left font-medium hover:underline"
              data-testid="ledger-entry-title"
            >
              {entry.title}
            </a>
          ) : (
            <span
              className="min-w-0 flex-1 font-medium"
              data-testid="ledger-entry-title"
            >
              {entry.title}
            </span>
          )}
          {entry.status && (
            <Badge variant="secondary" data-testid="ledger-entry-status">
              {entry.closed && <CheckCircle2 className="mr-1 size-3" />}
              {labelFor(columns, entry.status)}
            </Badge>
          )}
          <code
            className="text-xs text-muted-foreground"
            data-testid="ledger-entry-id"
          >
            {entry.id}
          </code>
        </div>

        <dl className="grid max-w-3xl gap-x-4 gap-y-1 text-sm sm:grid-cols-[8rem_minmax(0,1fr)]">
          {ledger.fields
            .filter(
              (field) =>
                field.role !== "id" &&
                field.role !== "title" &&
                field.role !== "status",
            )
            .map((field) => {
              const value = entry.fields[field.name];
              if (!value) return null;
              return (
                <div key={field.name} className="contents">
                  <dt className="text-muted-foreground">{field.name}</dt>
                  <dd className="whitespace-pre-wrap">
                    {compactFieldValue(value)}
                  </dd>
                </div>
              );
            })}
        </dl>

        <p className="text-xs text-muted-foreground">
          Last recorded by {byline(entry.updatedBy)} · {entry.events}{" "}
          {entry.events === 1 ? "write" : "writes"}
        </p>

        {writable && (
          <div className="flex flex-wrap gap-2">
            <Button variant="outline" size="sm" onClick={onAmend}>
              Amend
            </Button>
            {!entry.closed && ledger.statuses.some((s) => s.closed) && (
              <Button variant="outline" size="sm" onClick={onClose}>
                Close
              </Button>
            )}
            {/* Last, and quiet. Closing is the ordinary way to be finished
                with a row; this is the one that keeps nothing. */}
            <Button
              variant="ghost"
              size="sm"
              className="text-destructive"
              onClick={onDelete}
            >
              <Trash2 className="size-4" />
            </Button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/** Keep an empty paragraph from turning a compact list row into a document.
    Only the blank lines go: indentation and aligned columns inside a line are
    part of what was recorded, so the row still renders with `pre-wrap`. */
function compactFieldValue(value: string): string {
  return value
    .split(/\r?\n/)
    .filter((line) => line.trim())
    .join("\n");
}

function ComposeDialog({
  ledger,
  composing,
  existingIds,
  saving,
  onChange,
  onCancel,
  onSave,
}: {
  ledger: LedgerSummary;
  composing: Composing;
  existingIds: ReadonlySet<string>;
  saving: boolean;
  onChange: (next: Composing) => void;
  onCancel: () => void;
  onSave: () => void;
}) {
  const status = statusField(ledger);
  // Asked for here rather than let the save fail: the host refuses a silent
  // close, and meeting that rule in the form is the same rule met earlier.
  const needsReason = statusNeedsReason(ledger, composing.status);
  const reasonMissing = needsReason && !composing.fields.reason?.trim();

  return (
    <Dialog open onOpenChange={onCancel}>
      <DialogContent className="max-h-[85vh] sm:max-w-2xl overflow-y-auto">
        <DialogHeader>
          <DialogTitle>
            {composeDialogTitle(ledger, composing, existingIds)}
          </DialogTitle>
          <DialogDescription>
            {composeDialogDescription(composing, existingIds)}
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <div className="space-y-1">
            <Label htmlFor="ledger-entry-id">id</Label>
            <Input
              id="ledger-entry-id"
              value={composing.id}
              disabled={Boolean(composing.id) && composing.closing}
              placeholder="vendor-slip"
              onChange={(e) => onChange({ ...composing, id: e.target.value })}
            />
          </div>

          {status && (
            <div className="space-y-1">
              <Label>{status.name}</Label>
              <Select
                value={composing.status}
                onValueChange={(value) =>
                  onChange({ ...composing, status: value ?? "" })
                }
              >
                <SelectTrigger>
                  <SelectValue placeholder="Pick a status" />
                </SelectTrigger>
                <SelectContent>
                  {ledger.statuses.map((declared) => (
                    <SelectItem key={declared.name} value={declared.name}>
                      {declared.name}
                      {declared.closed ? " (closes the row)" : ""}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          )}

          {composableFields(ledger).map((field) => (
            <div key={field.name} className="space-y-1">
              <Label htmlFor={`ledger-field-${field.name}`}>
                {field.name}
                {field.required && " *"}
                {field.name === "reason" &&
                  needsReason &&
                  " — required to close"}
              </Label>
              {field.role === "prose" ? (
                <Textarea
                  id={`ledger-field-${field.name}`}
                  rows={3}
                  value={composing.fields[field.name] ?? ""}
                  onChange={(e) =>
                    onChange({
                      ...composing,
                      fields: {
                        ...composing.fields,
                        [field.name]: e.target.value,
                      },
                    })
                  }
                />
              ) : (
                <Input
                  id={`ledger-field-${field.name}`}
                  value={composing.fields[field.name] ?? ""}
                  onChange={(e) =>
                    onChange({
                      ...composing,
                      fields: {
                        ...composing.fields,
                        [field.name]: e.target.value,
                      },
                    })
                  }
                />
              )}
              {field.description && (
                <p className="text-xs text-muted-foreground">
                  {field.description}
                </p>
              )}
            </div>
          ))}

          {reasonMissing && (
            <Alert>
              <AlertTriangle className="size-4" />
              <AlertDescription>
                Closing into <code>{composing.status}</code> needs a reason. A
                row that does not say why it closed is worth nothing to whoever
                reads it next.
              </AlertDescription>
            </Alert>
          )}
        </div>

        <DialogFooter>
          <Button variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
          <Button onClick={onSave} disabled={saving || reasonMissing}>
            {saving && <Loader2 className="mr-2 size-4 animate-spin" />}
            {composing.closing ? "Close row" : "Record"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
