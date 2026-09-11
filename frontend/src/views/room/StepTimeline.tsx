import { useState } from "react";
import {
  AlertTriangle,
  Brain,
  ChevronDown,
  ChevronRight,
  CornerUpLeft,
  Hourglass,
  Loader2,
  Scissors,
  SquareKanban,
  Wrench,
  X,
  type LucideIcon,
} from "lucide-react";

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

import { TeammateAvatar } from "@/components/teammate-avatar";

import type { AsideConversationDto, ReferralConversationDto } from "@/api/types";

import {
  AWAITING_APPROVAL_LABEL,
  STEP_FAILURE_LABEL,
  isFailedStep,
  type TurnStep,
  type TurnStepKind,
} from "@/api/types";
import { consoleHref } from "@/lib/console-paths";
import { cn } from "@/lib/utils";

/**
 * A private aside, collapsed onto the move it rode under.
 *
 * Same idiom as {@link ReferralConversation}, deliberately — both are detail
 * behind a line rather than part of the desk's own conversation. What differs is
 * what the collapse means. A referral's rows are dropped host-side, so expanding
 * is the only way to read them at all. An aside is withheld from *agents*
 * outside it and never from a person: `Audience::admits` admits every operator
 * unconditionally, because privacy here is a deliberation device and not a
 * security boundary. So this collapse is tidiness — it stops a two-seat sidebar
 * reading like the room's own voice — and expanding reveals nothing the reader
 * was not already entitled to.
 *
 * Closed by default, and the count is the point of the closed state: it says how
 * much was said without saying it.
 */
export function AsideConversation({ aside }: { aside: AsideConversationDto }) {
  const [open, setOpen] = useState(false);
  const count = aside.lines.length;
  if (count === 0) return null;
  // Everyone but the author, who is this row's own speaker and already named.
  const addressed = aside.members.slice(1);

  return (
    <div className="mt-1 w-full max-w-[85%] sm:max-w-[75%]">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className="flex items-center gap-1 rounded-md px-1.5 py-0.5 text-2xs font-medium text-muted-foreground transition-colors hover:bg-accent/60"
      >
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        <span>
          aside {addressed.map((id) => `@${id}`).join(", ")} · {count} message
          {count === 1 ? "" : "s"}
        </span>
      </button>
      {open && (
        // Dashed, where a referral's is solid: the border is the one cue that
        // this was said beside the room rather than across a desk boundary.
        <ol className="mt-0.5 flex flex-col gap-2 rounded-lg border border-dashed bg-card/60 px-2.5 py-2">
          {aside.lines.map((line, i) => (
            // `AsideLineDto` carries no id of its own (issue: tinysweeper
            // review), so the index alone is not a stable key if the lines
            // are ever reordered or merged. Pairing it with the author keeps
            // a reordered author's rows from swapping component instances
            // with an unrelated author's.
            <li key={`${line.authorId}-${i}`} className="flex gap-2">
              <TeammateAvatar name={line.authorId} className="mt-0.5 size-5 shrink-0" />
              <div className="flex min-w-0 flex-col gap-0.5">
                <span className="text-2xs leading-none font-semibold">{line.authorId}</span>
                <span className="text-2xs leading-relaxed whitespace-pre-wrap text-muted-foreground">
                  {line.text}
                </span>
              </div>
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

/**
 * The scrubbed processing steps behind a company reply, rendered above its
 * bubble. Collapsed by default to a one-line "N steps · M failed" summary;
 * auto-expands when any step failed so a silent MCP failure is visible, not
 * buried. Renders nothing when there are no steps (a memory-served / tool-less
 * reply). Ported from the retired Conversation page (issue #246) so the chat
 * workspace keeps the same tool-call visibility it had.
 *
 * `defaultOpen` is for the *live* timeline of a turn still running (issue
 * #367): there the rows are the content — they are what says the company is
 * working and on what — so they start open rather than behind a count. A
 * finished reply's steps stay collapsed, where they are supporting detail.
 * Either way the operator's own toggle wins from the first click.
 */
export function StepTimeline({
  steps,
  defaultOpen = false,
}: {
  steps: TurnStep[];
  defaultOpen?: boolean;
}) {
  const failed = steps.filter((s) => isFailedStep(s.status)).length;
  // A parked step is counted and surfaced separately — it is not a failure, and
  // it is the most actionable thing in the list, so it must not hide behind a
  // collapsed summary either (#411).
  const parked = steps.filter((s) => s.status === "awaiting_approval").length;
  const hasError = failed > 0;
  const [open, setOpen] = useState(defaultOpen || hasError || parked > 0);

  if (steps.length === 0) return null;

  return (
    <div className="mt-1 w-full max-w-[85%] sm:max-w-[75%]">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className={cn(
          "flex items-center gap-1 rounded-md px-1.5 py-0.5 text-2xs font-medium transition-colors hover:bg-accent/60",
          hasError
            ? "text-destructive"
            : parked > 0
              ? "text-status-blocked-text"
              : "text-muted-foreground",
        )}
      >
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        <span>
          {steps.length} step{steps.length === 1 ? "" : "s"}
          {failed > 0 && ` · ${failed} failed`}
          {parked > 0 && ` · ${parked} awaiting approval`}
        </span>
      </button>
      {open && (
        <ol className="mt-0.5 flex flex-col gap-1 rounded-lg border bg-card/60 px-2.5 py-1.5">
          {steps.map((step, i) => (
            <StepRow key={i} step={step} />
          ))}
        </ol>
      )}
    </div>
  );
}

/**
 * A crossing with somebody outside this desk, as one collapsed line.
 *
 * Same idiom as {@link StepTimeline} and for the same reason: the exchange is
 * detail behind a report, not part of the desk's own conversation. The relayed
 * rows are dropped host-side — an agent who does not work here did not speak
 * here — so this is the only place an operator can read what was actually asked
 * and answered rather than the asker's paraphrase of it.
 *
 * Closed by default. The count is the whole point of the collapsed state: it
 * says how much was said without saying it.
 */
export function ReferralConversation({ crossing }: { crossing: ReferralConversationDto }) {
  const [open, setOpen] = useState(false);
  const count = crossing.lines.length;
  if (count === 0) return null;

  return (
    <div className="mt-1 w-full max-w-[85%] sm:max-w-[75%]">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className="flex items-center gap-1 rounded-md px-1.5 py-0.5 text-2xs font-medium text-muted-foreground transition-colors hover:bg-accent/60"
      >
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        <span>
          {/* Who was asked, in the form they were asked in: `@name` went to a
              person, `#desk` was put to a room. Naming the answerer's desk
              alongside their name read as though the desk had been asked, which
              for a `@name` crossing is the one thing that did not happen. */}
          asked {crossing.direct ? `@${crossing.otherId}` : `#${crossing.otherDeskId}`} ·{" "}
          {count} message{count === 1 ? "" : "s"}
        </span>
      </button>
      {open && (
        // Rendered as a conversation, because that is what it is. The same
        // gutter-avatar-then-author-then-body shape a message uses in the
        // transcript above, one size down: an operator reading this is reading
        // a chat between two desks, and a label-over-paragraph list made them
        // translate it back into one.
        <ol className="mt-0.5 flex flex-col gap-2 rounded-lg border bg-card/60 px-2.5 py-2">
          {crossing.lines.map((line, i) => {
            const who = line.outbound
              ? crossing.askerId
              : line.authorLabel || line.authorId;
            // The desk each side is speaking from — the asker's is this one, so
            // it goes unsaid; the answer comes from somewhere the reader may not
            // have open.
            return (
              <li key={i} className="flex gap-2">
                <TeammateAvatar name={who} className="mt-0.5 size-5 shrink-0" />
                <div className="flex min-w-0 flex-col gap-0.5">
                  {/* The name alone. The header already says whether this was
                      a person or a desk, and repeating the answerer's desk on
                      their line was what made a `@name` crossing read as though
                      the desk had been asked. */}
                  <span className="text-2xs leading-none font-semibold">{who}</span>
                  <span className="text-2xs leading-relaxed whitespace-pre-wrap text-muted-foreground">
                    {line.text}
                  </span>
                </div>
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}

function StepRow({ step }: { step: TurnStep }) {
  const error = isFailedStep(step.status);
  const parked = step.status === "awaiting_approval";
  const Icon = parked ? Hourglass : stepIcon(step.kind);
  return (
    <li
      className={cn(
        "flex flex-col gap-0.5 text-2xs leading-relaxed",
        error
          ? "text-destructive"
          : parked
            ? "text-status-blocked-text"
            : "text-muted-foreground",
      )}
    >
      <div className="flex items-center gap-1.5">
        <Icon className={cn("size-3 shrink-0", step.status === "running" && "animate-pulse")} />
        <span className={cn("font-medium", !error && !parked && "text-foreground/80")}>
          {step.label}
        </span>
        {/* The typed state, rendered by lookup — never by reading `result`. */}
        {parked && <StepChip tone="amber">{AWAITING_APPROVAL_LABEL}</StepChip>}
        {step.failure && <StepChip tone="rose">{STEP_FAILURE_LABEL[step.failure]}</StepChip>}
        {step.truncated && (
          <StepChip tone="amber">
            <Scissors className="size-2.5 shrink-0" aria-hidden />
            Result cut
          </StepChip>
        )}
        {step.detail && <span className="min-w-0 truncate">— {step.detail}</span>}
        <span className="ml-auto shrink-0 tabular-nums opacity-70">
          {formatElapsed(step.elapsedMs, step.status)}
        </span>
      </div>
      {/* What came back, on its own line: it is the answer to "how far did we
          get", and inlining it would push the arguments off the row. */}
      {step.result && (
        <span className="min-w-0 truncate pl-[18px] opacity-80">{step.result}</span>
      )}
    </li>
  );
}

function StepChip({
  tone,
  children,
}: {
  tone: "amber" | "rose";
  children: React.ReactNode;
}) {
  return (
    <span
      className={cn(
        "flex shrink-0 items-center gap-1 rounded px-1 py-px text-3xs font-medium",
        tone === "amber"
          ? "bg-status-blocked-soft text-status-blocked-text"
          : "bg-status-failed-soft text-status-failed-text",
      )}
    >
      {children}
    </span>
  );
}

function stepIcon(kind: TurnStepKind): LucideIcon {
  switch (kind) {
    case "tool_call":
      return Wrench;
    case "thinking":
      return Brain;
    case "note":
      return AlertTriangle;
    default:
      return Wrench;
  }
}

/**
 * A step's duration.
 *
 * A gated call never ran, so it reports `0ms` — which read identically to a
 * fast success and was one of the things #411 called out. Say "didn't run"
 * instead: a duration of zero on a step that never left the process is not a
 * measurement, it is the absence of one.
 */
function formatElapsed(ms: number | undefined, status: TurnStep["status"]): string {
  if (status === "awaiting_approval") return "didn't run";
  if (typeof ms !== "number") return "";
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
}

/**
 * The "a card opened from this reply" chip (issue #246) — links straight to
 * the board card a turn opened, or the one it dispatched to, and dismisses it
 * (issue #984).
 *
 * The dismissal is the half #442 promised and never shipped on this surface:
 * it allowed a turn to open a card from an ordinary message on the grounds
 * that *"a spurious card can be dismissed in one click"*, while this chip was
 * a bare link to the card's detail screen. `onDismiss` is optional so a caller
 * that has no card-delete route — the thread panel, a future read-only
 * transcript — still renders the link half rather than a control that throws.
 */
/**
 * Where a crossing referral came from, or went (tinyhivemind P15).
 *
 * Modelled on {@link CardChip} deliberately: both are provenance on the bubble
 * — "this message is connected to something not on this screen" — and a reader
 * should not have to learn two shapes for one idea. It is NOT a centred system
 * pill; a pill is a line the runtime wrote *about* the conversation, and this
 * is a fact about the message under it.
 *
 * Two directions, two words, because the reader's question differs. In the
 * desk that was asked, the question is "why is this here?" — `Asked by
 * Engineering`. In the desk that asked, an answer has arrived from a teammate
 * who is not on this desk, and without saying so it reads as local work —
 * `Answered by Product & Design`.
 *
 * The label is whatever the host captured with the row; this never resolves a
 * desk id at render, and never shows one — an id in operator copy is the thing
 * `DiscussionMessage.author` refuses for the same reason.
 */
export function ReferralChip({
  deskId,
  deskName,
  askerId,
  sequence,
  direction,
  direct = false,
}: {
  deskId: string;
  deskName: string;
  askerId: string;
  sequence: number;
  direction: "asked" | "answered";
  direct?: boolean;
}) {
  // Whoever was actually addressed. A crossing put to a PERSON never reached
  // their desk — that desk holds none of the exchange and its other members had
  // no part in it — so naming the desk here credited a room that was never asked.
  const who = direct ? `@${askerId}` : deskName;
  const label = direction === "asked" ? `Asked by ${who}` : `Answered by ${who}`;
  const body = (
    <>
      <CornerUpLeft className="size-3 shrink-0" />
      {label}
    </>
  );
  return (
    <span className="mt-1.5 flex w-fit items-center rounded-full bg-accent text-accent-foreground">
      {/* A DESK crossing ran on that desk, so its transcript is where the
          question at `sequence` is and the link reaches it.

          A DIRECT one did not: both sides are held in the pair's own thread and
          the target's desk holds none of it, so this link would open an
          unrelated conversation and land on a sequence that is not there.
          Nothing to link to until a pair thread is a surface an operator can
          open — and the exchange itself is already one click away, on the
          message this chip sits under. So it reads as a label rather than
          offering a way somewhere wrong. */}
      {direct ? (
        <span className="flex items-center gap-1 px-2 py-0.5 text-2xs font-medium">{body}</span>
      ) : (
        <a
          href={`#/chat?desk=${encodeURIComponent(deskId)}&at=${sequence}`}
          className="flex items-center gap-1 px-2 py-0.5 text-2xs font-medium transition-opacity hover:opacity-80"
          title={`Open the conversation that ${direction === "asked" ? "asked" : "answered"}`}
        >
          {body}
        </a>
      )}
    </span>
  );
}

export function CardChip({
  taskId,
  busy = false,
  disabled = false,
  onDismiss,
}: {
  taskId: string;
  busy?: boolean;
  disabled?: boolean;
  onDismiss?: (taskId: string) => void;
}) {
  const link = (
    <a
      href={consoleHref("tasks", taskId)}
      className={cn(
        "flex items-center gap-1 py-0.5 text-2xs font-medium transition-opacity hover:opacity-80",
        onDismiss ? "pl-2 pr-1" : "px-2",
      )}
    >
      <SquareKanban className="size-3 shrink-0" />
      Card opened
    </a>
  );
  return (
    <span className="mt-1.5 flex w-fit items-center rounded-full bg-accent text-accent-foreground">
      {link}
      {onDismiss && (
        <AlertDialog>
          <AlertDialogTrigger
            render={
              <button
                type="button"
                // Always in the DOM and focusable rather than hover-revealed:
                // this chip is the only place the card can be dismissed from
                // the channel, and a hover-only control is unreachable by
                // keyboard and on touch.
                className="flex items-center rounded-full py-0.5 pl-0.5 pr-1.5 transition-opacity hover:opacity-80 disabled:opacity-50"
                disabled={busy || disabled}
                title="Dismiss this card"
                aria-label="Dismiss this card"
              >
                {busy ? (
                  <Loader2 className="size-3 shrink-0 animate-spin" />
                ) : (
                  <X className="size-3 shrink-0" />
                )}
              </button>
            }
          />
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Dismiss this card?</AlertDialogTitle>
              <AlertDialogDescription>
                This deletes the card from the board and can’t be undone. The
                message stays in the channel.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Keep card</AlertDialogCancel>
              <AlertDialogAction
                onClick={() => onDismiss(taskId)}
                className="bg-destructive text-white hover:bg-destructive/90"
              >
                Dismiss card
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      )}
    </span>
  );
}
