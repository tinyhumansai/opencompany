// One card on a board, when the row behind it is a `Task`.
//
// # Why this is a file of its own
//
// It was the task board screen's card, and the screen is gone (issue #1140):
// the board an operator works is the `tasks` ledger's columns inside the
// Ledgers section, and meeting the same records twice — once on a Tasks page,
// once on a Ledgers page — was the duplication that retired the page.
//
// The card did not go with it. `LedgerBoard`'s `renderCard` slot exists exactly
// so one board can serve both a ledger a company declared this morning and the
// native board, and this is the second half of that pair: a priority, an
// assignee, a cost, a workflow chip, a plan badge, an output link and — the one
// that matters most — what a paused card is stopped behind and whether Resume
// is the right click. None of that is a ledger field; all of it comes off the
// `Task` record, which is why a role-driven renderer was tried here and was
// wrong. See `LedgerBoard`'s header for that argument in full.
//
// Nothing changed on the way over. That was deliberate: the move was verified
// by `test/unit/task-blocked-card.test.ts` passing with only its import line
// touched.
//
// # The blocked card decides (#1891)
//
// What a paused card used to say was that approvals existed — one action name,
// or a count, and a link somewhere else. Everything needed to say more was
// already in hand and thrown away: which URL, which command, how much money,
// who asked, and how long before the deadline default-denies it. So the card
// renders `ApprovalRow`, the same component the Approvals page, the chat
// transcript and the workflow run drawer decide through, in a stacked variant
// sized for a column. It resolves; it does not re-implement resolving.
//
// This is the run drawer's fix (#1002) reaching the surface an operator
// actually works from.

import { useMemo } from "react";

import {
  AlertTriangle,
  CircleHelp,
  ClipboardList,
  FileText,
  ListTree,
  Paperclip,
  Play,
  ScrollText,
} from "lucide-react";

import type { Task, TaskPlan } from "@/api/tasks";
import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { withHostParam } from "@/hooks/use-host-route";
import { PRIORITY_STYLES } from "@/lib/board-columns";
import { formatUsdCost } from "@/lib/cost";
import {
  decidingForTask,
  taskApprovalVerdicts,
  type TaskApprovalRow,
} from "@/lib/task-approvals";
import { extraOutputCount, primaryLink, type TaskLink } from "@/lib/task-output";
import { cn } from "@/lib/utils";
import { ApprovalRow } from "@/views/chat/ApprovalRow";
import { approvalBatchKey } from "@/views/chat/model";
import { tallyPrerequisites } from "./TaskPlanBrief";

function priorityStyle(priority: string): string {
  return PRIORITY_STYLES[priority as keyof typeof PRIORITY_STYLES] ?? PRIORITY_STYLES.low;
}

/**
 * What a card's note says, once the runtime's own bookkeeping is out of it.
 *
 * `note` is not prose with the occasional machine line in it — **the whole
 * field is an attributed journal**. `append_result`
 * (`src/runtime/advance.rs:71`) writes every outcome as `[<who>] <what>` and
 * joins the blocks with a blank line, never overwriting: *"the note is the
 * card's history"*. `<who>` is `system` for the host's own paths and the
 * teammate's id for everything else, so a live board shows both
 * `[system] the dispatch cycle ended without settling this attempt` and
 * `[frontend_engineer] __MOCK_LLM__ mock inference backend reply.`
 *
 * Reading that field as text has three consequences on a card face, which has
 * room for exactly one secondary line:
 *
 * 1. **The bookkeeping reads as the work.** Three of eight To-do cards on a
 *    healthy seeded board reported an error that had not happened.
 * 2. **The attribution is said twice.** The card already carries the assignee
 *    as an avatar and a name; `[frontend_engineer]` in the body is the same
 *    fact again, in the noisiest possible place.
 * 3. **`line-clamp-2` shows the *oldest* two lines.** The journal is
 *    append-only, so a clamped note freezes on the first thing that ever
 *    happened to the card and never moves again — the exact opposite of what a
 *    running history is for.
 *
 * So: split the journal into its blocks, drop the host's own (`[system]`),
 * take the **most recent** of what is left, and strip its `[<who>]` prefix.
 * A block with no prefix is a note somebody typed, and is shown as-is.
 *
 * Only the *preview* is derived this way. The note itself is untouched, and the
 * whole of it — system blocks included — is still on the detail screen's
 * timeline, which is where a journal belongs and where somebody looking for one
 * went.
 *
 * Returns `null` when nothing is left, so the card renders no line at all
 * rather than an empty one holding space.
 */
export function notePreview(note: string | undefined): string | null {
  if (!note) return null;
  const blocks = note
    .split(/\n\s*\n/)
    .map((block) => block.trim())
    .filter(Boolean)
    .filter((block) => !block.startsWith("[system]"));
  const latest = blocks[blocks.length - 1];
  if (!latest) return null;
  return latest.replace(/^\[[^\]\n]*\]\s*/, "").trim() || null;
}

/**
 * One card on the task board.
 *
 * It no longer carries the drag handlers: [`LedgerBoard`](./LedgerBoard) wraps
 * every card in the draggable element and owns the gesture, so this is purely
 * what a *task* looks like. That split is what lets one board serve both this
 * and a ledger a company declared — see that module's docs for why the card is
 * a slot rather than something built from field roles.
 *
 * Exported for `test/unit/task-blocked-card.test.ts` (issue #883). The
 * paused card's central claim — Resume is *disabled* while the card's own
 * approvals are undecided, because pressing it re-runs work that parks again —
 * exists only at the rendered button, so a pure test of the derivation cannot
 * reach it. Same exception `approval-batch-card.test.ts` earns, on the same
 * grounds: the thing under test is what reaches the operator's hand.
 */
export function TaskItem({
  task,
  dragging,
  rows,
  now,
  askerNames,
  deciding,
  failed,
  onDecide,
  onOpen,
  onResume,
}: {
  task: Task;
  dragging: boolean;
  /**
   * Every approval this card is (or was just) stopped behind (#883, #1891) —
   * empty when nothing is. Both the still-parked ones and any this console has
   * witnessed a verdict for, so a decision settles in place across the gap
   * between the resolve's answer and the feed's next poll.
   */
  rows: readonly TaskApprovalRow[];
  /** The clock `rows` was derived against, for their relative labels. */
  now: number;
  /** Roster ids → names, for naming who asked (#1891). */
  askerNames: Map<string, string>;
  /** Decisions in flight across the console; narrowed to this card below. */
  deciding: ReadonlyMap<string, Verdict>;
  /** Decisions that did not land, keyed by approval id. */
  failed: Record<string, string>;
  /** Whether a detached approval continuation is still running for this card. */
  /**
   * The shell's one resolve, per approval id (#1891).
   *
   * Optional on `RunResultPanel`'s precedent, and gating the row the same way:
   * a surface with no handler renders no decide controls rather than live
   * buttons that do nothing. Every board in this console is handed one.
   */
  onDecide?: (approval: ApprovalSummary, verdict: Verdict, scope: GrantScope) => void;
  onOpen: () => void;
  onResume: () => void;
}) {
  // One group per turn, never one card-wide batch (#1895 review). `ApprovalRow`
  // consolidates because #842's premise is that a batch is *one piece of work*
  // — one turn's parked calls, interrupting once. A paused card can hold more
  // than one turn's parks (an overlapping re-dispatch) or several the host
  // never keyed at all, and handing those to a single row put one Approve over
  // unrelated requests. `approvalBatchKey` is the transcript's own rule, shared
  // rather than restated so the two surfaces cannot answer it differently.
  const groups = useMemo(() => {
    const byKey = new Map<string, TaskApprovalRow[]>();
    for (const row of rows) {
      const key = approvalBatchKey(row.approval);
      const bucket = byKey.get(key);
      if (bucket) bucket.push(row);
      else byKey.set(key, [row]);
    }
    // Only the groups still asking something. A batch whose every row has been
    // decided is settled; the card simply stops showing it.
    return [...byKey.entries()].filter(([, group]) =>
      group.some((row) => row.verdict === null),
    );
  }, [rows]);
  return (
    <div
      className={cn(
        "cursor-grab rounded-lg border bg-card p-3 shadow-sm transition-[transform,box-shadow] hover:shadow active:cursor-grabbing",
        // A card being carried needs to read as being in the operator's hand,
        // not as unavailable. The small rise, rotation, and shadow make that
        // state distinct from a disabled card without changing the gesture.
        dragging && "-translate-y-1 rotate-1 shadow-xl",
      )}
    >
      <div className="flex items-start justify-between gap-2">
        <button
          type="button"
          onClick={onOpen}
          className="-m-1 min-w-0 rounded-sm p-1 text-left text-sm font-medium leading-snug hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          data-testid="task-card-open"
        >
          {task.title}
        </button>
        {/* Only when something is being asked. `low` is what a card takes when
            nobody chose a priority, so a badge for it is a pill on a third of a
            real board announcing the default — noise competing with the title
            for the one line of the card that has to be read first.
            `PRIORITY_STYLES` already makes the same call about colour, keeping
            `low` neutral "for the same reason `idle` does: nothing is being
            asked of anyone". This finishes that thought. */}
        {task.priority !== "low" && (
          <Badge variant="outline" className={cn("shrink-0 capitalize", priorityStyle(task.priority))}>
            {task.priority}
          </Badge>
        )}
      </div>
      {notePreview(task.note) && (
        <p className="mt-1 line-clamp-2 whitespace-pre-wrap text-xs text-muted-foreground">
          {notePreview(task.note)}
        </p>
      )}
      {task.assignee && (
        <div className="mt-3 flex items-center gap-2">
          <span
            className="flex size-6 items-center justify-center rounded-full bg-muted text-3xs font-semibold text-muted-foreground"
            aria-hidden
          >
            {initials(task.assignee)}
          </span>
          <span className="truncate text-xs text-muted-foreground">{task.assignee}</span>
        </div>
      )}
      {formatUsdCost(task.cost, "total") && (
        <div className="mt-2 text-2xs font-medium tabular-nums text-foreground">
          {formatUsdCost(task.cost, "total")}
        </div>
      )}
      {task.deliverable === "workflow" && (
        <div className="mt-2 inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-2xs text-muted-foreground">
          <ListTree className="size-3 shrink-0" />
          Workflow
        </div>
      )}
      {/* Issue #1865 (Codex review): the task API converts a stored `todo`
          card to the wire phase `column: "pending"` (issue #1512) and never
          serializes `column: "todo"` — `stage_of` only fills `stage` for the
          `working` phase's four columns, so a To-do card carries no `stage`
          either. `"todo"` here matched no real card; `"pending"` is the wire
          value a bounced card — always in the store's `todo` column — is
          actually seen at. */}
      {task.column === "pending" && task.bounced && (
        <BouncedBadgeRow reason={task.bounced} />
      )}
      {task.plan && <PlanBadgeRow plan={task.plan} />}
      {showsOutputLink(task) && <OutputLinkRow task={task} />}
      {task.stage === "paused" && (
        <>
          {groups.length > 0 && onDecide && (
            // Deciding, not just reporting (#1891). Rendered only while
            // something is actually blocking: once every row has a verdict this
            // component's own settled receipt would take the card's place
            // permanently, and a board card is not where a decision's paperwork
            // belongs — the card simply stops being blocked.
            <div
              // The rows hold buttons and links. Without this, every click
              // inside them would also reach the card's own open handler and
              // drop the operator into task detail as their decision landed.
              onClick={(e) => e.stopPropagation()}
            >
              {groups.map(([key, group]) => (
                <ApprovalRow
                  key={key}
                  variant="card"
                  // The group's whole set, not only its undecided rows: the row
                  // subtracts what is already decided itself and says so — "1
                  // of 3 decided — 2 still waiting on you" is what an operator
                  // part-way through a batch needs, and passing the remainder
                  // would silently renumber the work under them.
                  approvals={group.map((r) => r.approval)}
                  now={now}
                  askerNames={askerNames}
                  // This card's rows on the Approvals page, not the flat queue —
                  // built with `withHostParam` because a raw hash href drops the
                  // host scope, and an operator on a second host would be sent to
                  // another console's queue.
                  detailsHref={withHostParam(`approvals/${encodeURIComponent(task.id)}`)}
                  // Narrowed to this group: a decision in flight on another of
                  // this card's turns must not freeze it.
                  deciding={decidingForTask(group, deciding)}
                  decided={taskApprovalVerdicts(group)}
                  failed={failed}
                  onDecide={onDecide}
                />
              ))}
            </div>
          )}
          <Button
            variant="outline"
            size="sm"
            className={cn("h-7 w-full", groups.length > 0 ? "mt-2" : "mt-3")}
            // Issue #883: the button is disabled rather than hidden while the
            // card is blocked. Hiding it would leave the card looking like it
            // had no next action at all, which is the ambiguity being fixed —
            // the operator has to be able to see that Resume is the wrong click
            // right now, not wonder where it went. `title` carries the reason
            // for a pointer; the row above carries it for everyone else.
            //
            // Still disabled now that the decision is on the card, and more
            // clearly right for it: since #469 the last verdict continues the
            // turn on its own, so Approve *is* the resume. Pressing this
            // instead would re-run the work from the start and park it again.
            //
            // Keyed on `rows`, not on `blocking` (#1895 review). An Approve
            // this console has just witnessed empties `blocking` at once, while
            // the host is only *starting* the continuation the verdict
            // released — the resolve detaches (#391), so its answer comes back
            // before the follow-up cycle runs. Re-enabling Resume there would
            // put a live re-dispatch under the operator's finger at the exact
            // moment they had finished deciding, duplicating the work the
            // decision had already set going. The queue still holds the row
            // until the host drops it, so this stays down across that window
            // and lifts on its own.
            //
            // A `continuationInFlight` flag was tried here and removed: the
            // shell set it before the resolve and cleared it in the `finally`,
            // so it was true only while the POST was in flight — exactly the
            // window `rows` already covers, since the queue still holds the row
            // then. It read as closing the gap after the feed refreshes and did
            // not, which is worse than the gap being documented: that one needs
            // a host-side "continuation running" signal the board projection
            // does not carry, and it is tracked on #1891.
            disabled={rows.length > 0}
            title={
              rows.length > 0
                ? "Blocked — decide its approvals first; resuming re-runs the work from the start."
                : undefined
            }
            onClick={(e) => {
              // Don't let the click bubble to the card's open handler.
              e.stopPropagation();
              onResume();
            }}
          >
            <Play className="mr-1.5 size-3.5" />
            Resume
          </Button>
        </>
      )}
    </div>
  );
}

/**
 * The columns whose cards show what they produced (issue #339).
 *
 * Done **and In review**, which is a correction to how the epic is worded. A
 * clean success no longer lands in Done — it stops in In review, and Done is
 * reached only when a person accepts it. So a card that has produced something
 * spends most of its visible life in In review, and showing the link only in
 * Done would hide it during exactly the stretch where somebody is deciding
 * whether to accept the work and needs to read it.
 *
 * Not the earlier columns: a card in To-do or In progress either has no output
 * yet or has one from a superseded attempt, and advertising that mid-run would
 * suggest the work in flight is already finished.
 */
const SHOWS_OUTPUT_LINK = new Set(["in_review", "done"]);

/**
 * Whether this card advertises its output.
 *
 * Reads the **stage** and falls back to the phase, because since issue #1512 a
 * card waiting on a verdict is `column: "working", stage: "in_review"` while a
 * finished one is `column: "done"` with no stage at all. Matching on `column`
 * alone would put the link on every working card, including the three that
 * have not produced anything to look at yet.
 */
function showsOutputLink(task: Task): boolean {
  return SHOWS_OUTPUT_LINK.has(task.stage ?? task.column);
}

/**
 * What a planned card carries, in one line on the board (issue #337).
 *
 * Shown on **every** column a plan survives into rather than a chosen set, and
 * that is the difference from {@link SHOWS_OUTPUT_LINK} above. An output is
 * only meaningful once there is one, so it earns a column filter; a plan is
 * only ever present because a person deliberately asked for one, so hiding it
 * anywhere would be second-guessing that request.
 *
 * The blocked case is the one that has to be loud. A pass that could not clear
 * a card returns it to To-do, where it sits looking exactly like work nobody
 * has picked up — and the difference between "not started" and "cannot start"
 * is the whole point of having planned it. So blockers get the destructive
 * treatment and a count; a clear plan gets a quiet step count and stays out of
 * the way.
 *
 * `needsApproval` and `unknown` are deliberately not counted here. Neither
 * stops the card host-side, and a badge that counted them would tell an
 * operator to go fix something that is not blocking anything.
 */
/**
 * The board's bounce chip (issue #1865): a card in `todo` because a run
 * FAILED, distinct from one nobody has touched yet.
 *
 * Before this, a card returned to `todo` after a failed dispatch looked
 * identical to a fresh one — `todo` was both the failure state and the
 * unstarted state, so an operator had to open every card in the column to
 * tell a bounced retry candidate apart from work nobody had picked up. The
 * host clears `task.bounced` the moment the card re-enters `in_progress`, so
 * this reads as stale for exactly as long as the card sits untouched — never
 * once a fresh attempt is under way.
 *
 * `AlertTriangle` + destructive styling, matching {@link PlanBadgeRow}'s
 * blocking-prerequisite row: both name a reason the card needs a human look
 * rather than the machine simply finishing it.
 */
function BouncedBadgeRow({ reason }: { reason: string }) {
  return (
    <div className="mt-2 flex items-start gap-1.5 text-2xs font-medium text-destructive">
      <AlertTriangle className="mt-0.5 size-3 shrink-0" />
      <span className="line-clamp-2">{'bounced: '}{reason}</span>
    </div>
  );
}

function PlanBadgeRow({ plan }: { plan: TaskPlan }) {
  const { blocking, approval, unchecked } = tallyPrerequisites(plan);
  if (blocking > 0) {
    return (
      <div className="mt-2 flex items-center gap-1.5 text-2xs font-medium text-destructive">
        <AlertTriangle className="size-3 shrink-0" />
        <span>
          Planned — needs {blocking} thing{blocking === 1 ? "" : "s"}
        </span>
      </div>
    );
  }
  // Nothing blocking, but not necessarily all-clear either — the same three-way
  // distinction the brief's headline makes, kept in step with it so the board
  // and the card can never disagree about whether a plan is settled. A count
  // here is a prompt to open the card, where the rows say which is which.
  const unresolved = approval + unchecked;
  if (unresolved > 0) {
    return (
      <div className="mt-2 flex items-center gap-1.5 text-2xs text-status-blocked-text">
        <CircleHelp className="size-3 shrink-0" />
        <span>
          Planned — {unresolved} to be aware of
        </span>
      </div>
    );
  }
  return (
    <div className="mt-2 flex items-center gap-1.5 text-2xs text-muted-foreground">
      <ClipboardList className="size-3 shrink-0" />
      <span>
        Planned
        {plan.steps.length > 0 && ` · ${plan.steps.length} step${plan.steps.length === 1 ? "" : "s"}`}
      </span>
    </div>
  );
}

function LinkIcon({ kind }: { kind: TaskLink["kind"] }) {
  const className = "size-3.5 shrink-0";
  if (kind === "artifact") return <Paperclip className={className} />;
  if (kind === "workflow") return <ListTree className={className} />;
  if (kind === "trace") return <ScrollText className={className} />;
  return <FileText className={className} />;
}

/**
 * One line on a finished card: *here is the thing this task produced*.
 *
 * A card that produced no file still gets one, because for those the link opens
 * the attempt's trace — which is the deliverable when there is no document. A
 * card that recorded no attempt at all gets no row: `primaryLink` returns the
 * `card` kind there, a link back to the card itself, and the card is already
 * that click. See the guard below.
 *
 * It is independent of the title button that opens the task detail screen, so
 * following an output never also opens the task itself.
 */
function OutputLinkRow({ task }: { task: Task }) {
  const link = primaryLink(task);
  const extra = extraOutputCount(task);
  // A card that produced nothing links to itself, labelled "Open this task" —
  // which is what the title button already does (`onOpen`). A
  // second copy of the card's own action, given a divider and a row of its own,
  // is most of why two cards for the same kind of object came out different
  // heights and different shapes depending only on which column they sat in.
  // The row still appears the moment there is a real deliverable behind it.
  if (link.kind === "card") return null;
  return (
    <div className="mt-3 flex items-center gap-2 border-t pt-2 text-xs">
      <a
        href={link.href}
        title={link.hint}
        className="flex min-w-0 items-center gap-1.5 text-muted-foreground hover:text-foreground hover:underline"
      >
        <LinkIcon kind={link.kind} />
        <span className="truncate">{link.label}</span>
      </a>
      {extra > 0 && (
        <a
          href={`#/tasks/${encodeURIComponent(task.id)}`}
          title="Open the task to see everything it produced."
          className="shrink-0 text-muted-foreground hover:text-foreground hover:underline"
        >
          +{extra} more
        </a>
      )}
    </div>
  );
}

/**
 * The two letters on a card's avatar.
 *
 * Splits on underscores and hyphens as well as whitespace, because a teammate
 * id is snake_case and holds no whitespace at all — so this returned a
 * **single** letter for every agent on the board, and `docs_writer`, `devrel`
 * and `designer` all rendered the same "D". An avatar that cannot tell three
 * teammates apart is decoration.
 *
 * Exported for `test/unit/task-card-face.test.ts`.
 */
export function initials(name: string): string {
  return name
    .trim()
    .split(/[\s_-]+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((p) => p.charAt(0).toUpperCase())
    .join("");
}
