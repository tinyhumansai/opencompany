import { MessageSquareReply } from "lucide-react";

import type { TaskStatus } from "@/api/tasks";
import type { CognitionState } from "@/api/types";
import { AgentAvatarButton, useAgentProfileOpener } from "@/components/agent-profile-sheet";
import { Markdown } from "@/components/markdown";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Button } from "@/components/ui/button";
import { IN_FLIGHT_COLUMNS } from "@/lib/board-columns";
import { isHostMessageId, type ChatMessage } from "@/lib/chat";
import { isBudgetPauseNotice } from "@/hooks/use-events";
import { timeAgo } from "@/lib/language";
import { BudgetPauseNoticeCard } from "./BudgetPauseNoticeCard";
import { MessageAttachments } from "./MessageAttachments";
import { cn } from "@/lib/utils";
import {
  formatTime,
  hasReacted,
  QUICK_REACTIONS,
  reactionChips,
  type ReactionChip,
  type Sender,
  type TimelineEntry,
} from "./model";
import { EchoPlaceholder, echoMarkerFor } from "./EchoPlaceholder";
import { CardChip, StepTimeline } from "./StepTimeline";
import { WorkingIndicator } from "./WorkingIndicator";

interface Props {
  entry: TimelineEntry;
  /** True when the thread panel is showing this row's replies. */
  threadOpen: boolean;
  onOpenThread: (messageId: string) => void;
  onReact: (messageId: string, emoji: string) => void;
  /** Deletes the board card this line opened, and drops its chip (issue #984). */
  onDismissCard: (taskId: string) => void;
  /** The card whose delete is in flight, if any. */
  dismissingCardId: string | null;
  /**
   * Resolves an attachment's bytes to an object URL for preview/download
   * (issue #1682). Threaded from the shell, which holds the authenticated
   * client the blob route needs.
   */
  resolveAttachmentUrl?: (nodeId: string) => Promise<string>;
  /** Board task id -> live state for card-linked background turns (#1758). */
  taskStatusByTaskId?: Readonly<Record<string, TaskStatus>>;
  /** Shared shell clock for elapsed background-work copy. */
  now?: number;
  /**
   * Whether this company's teammates can think, as the host reported it (issue
   * #1735). On either echo state nothing on the company side of this transcript
   * was written by the teammate it appears under, and every such row is marked
   * (issue #1734).
   *
   * The **discriminated state** rather than a boolean, because the marker's
   * tooltip has to name the cause: `unconfigured` and `unavailable` have
   * different remedies, and a chip that says "no model configured" on a host
   * with no harness contradicts the banner directly above it. Collapsing them
   * here is what made that contradiction possible (CodeRabbit and codex both
   * caught it on PR #1740).
   *
   * A company-level fact rather than a per-message one, because that is the
   * only shape the truth has here: `ChatMessage` carries no provenance, so a
   * canned line and a considered one are byte-for-byte identical at this layer.
   * The alternatives were worse. Matching the text (`"You said: …"`) would hide
   * a genuine reply that happens to start that way, and would miss the echo
   * brain's other two lines (`"Acknowledged."`, `"webhook on …"`) entirely.
   * Suppressing the row would leave the operator's message with no answer at
   * all, which reads as "still working" — one lie traded for another, and it
   * would put the transcript out of step with the journal, which did record a
   * reply.
   *
   * The known imprecision runs both ways, and both are the price of having no
   * per-message provenance: a company on the echo brain *now* may hold replies
   * from a boot when it was configured and those get marked too, and a company
   * configured *since* keeps historical echoes unmarked. Marking by the
   * company's current state is what the console can actually know; stamping
   * provenance at write time is a host change (issue #1792).
   *
   * `undefined` is unknown — an older host — and marks nothing.
   */
  cognition?: CognitionState | null;
  /**
   * The Add-Credits CTA (issue #1846): redeems the parked re-issue marker for
   * a budget-paused teammate. `undefined` when the shell has not wired
   * redemption — the notice still renders, just without a working button.
   *
   * Carries the clicked notice's own `message.id` alongside the agent id
   * (issue #1846 review, Codex #3868962374) — the caller binds the redeem to
   * the marker THIS card was rendered from, rather than whatever is live at
   * click time.
   */
  onRedeemBudgetPause?: (agentId: string, noticeMessageId: string) => void;
  /** The agent id whose redeem is currently in flight, so only that row's
   * button shows a busy state and the others stay clickable. */
  redeemingBudgetPauseAgent?: string | null;
  /**
   * Agent id -> the message id of that agent's most recently parked
   * budget-pause notice (issue #1846 review, Codex #3864988184).
   *
   * The backend keeps at most one marker per agent — a fresh pause overwrites
   * the last — so a notice that is not this row disables the CTA rather than
   * offering to redeem a marker that belongs to a different, newer pause than
   * the one on screen. Computed once in `MessageTimeline` (the only place
   * with the whole channel's history) and passed straight through.
   */
  latestBudgetPauseMessageIdByAgent?: Map<string, string>;
}

/**
 * Whether a card-linked reply still represents background work (#1758).
 *
 * An in-flight row wins over a briefly stale board read. Otherwise the board
 * is authoritative, and {@link IN_FLIGHT_COLUMNS} is the one place that says
 * which stages are actually active (`board-columns.ts`) — reused rather than
 * re-derived here so a card back in `pending` (a planning failure, a cancel,
 * or a revision) reads as stopped, the same as review, done or paused, rather
 * than defaulting to "working" for anything that isn't a known terminal word.
 */
export function isTaskWorking(status: TaskStatus | undefined): boolean {
  if (!status) return false;
  return status.startedAt !== undefined || IN_FLIGHT_COLUMNS.includes(status.column);
}

/** The requested stable elapsed sentence, or nothing without a run clock. */
export function taskElapsedLabel(
  startedAt: number | undefined,
  now: number,
): string | null {
  if (startedAt === undefined || !Number.isFinite(startedAt) || !Number.isFinite(now)) {
    return null;
  }
  const minutes = Math.max(0, Math.floor((now - startedAt) / 60_000));
  return `${minutes} min elapsed, still working`;
}

/**
 * Why replying and reacting are unavailable on a row, or `undefined` when they
 * are not (issue #364).
 *
 * Derived from the row's own id rather than passed down, because the id *is*
 * the answer: a thread reply and a reaction both have to name a message the
 * host has journaled, and only an `h`-prefixed id names one. A row still
 * carrying its optimistic counter (the POST has not landed) or one from a host
 * with no durable ids at all cannot be named by either.
 *
 * The reason is a sentence, not a boolean, because a control that just stops
 * working reads as a bug. This is the on-screen label for the one thing about
 * this feature that is genuinely conditional on the host.
 */
function actionsUnavailableFor(message: ChatMessage): string | undefined {
  if (isHostMessageId(message.id)) return undefined;
  return message.from === "system"
    ? "Console-only line — there is nothing saved to reply to."
    : "Not saved yet — a reply or a reaction needs a message this company has stored.";
}

/**
 * One line in the channel.
 *
 * The layout is a fixed avatar gutter plus a flexible body, which is what
 * makes a run of messages from one voice line up: the first row shows the
 * avatar and the author, and every continuation row leaves the gutter empty
 * and reveals its timestamp there on hover instead. The action bar floats over
 * the top-right corner rather than taking layout space.
 */
export function MessageRow({
  entry,
  threadOpen,
  onOpenThread,
  onReact,
  onDismissCard,
  dismissingCardId,
  resolveAttachmentUrl,
  taskStatusByTaskId,
  now = Date.now(),
  cognition,
  onRedeemBudgetPause,
  redeemingBudgetPauseAgent,
  latestBudgetPauseMessageIdByAgent,
}: Props) {
  const { message, sender, continuation, replies } = entry;
  const chips = reactionChips(message.reactions);
  const actionsUnavailable = actionsUnavailableFor(message);
  const taskStatus = message.taskId ? taskStatusByTaskId?.[message.taskId] : undefined;
  const taskWorking = isTaskWorking(taskStatus);
  const elapsed = taskWorking ? taskElapsedLabel(taskStatus?.startedAt, now) : null;

  if (sender.kind === "system") {
    return (
      <SystemPill
        message={message}
        onRedeemBudgetPause={onRedeemBudgetPause}
        redeemingBudgetPauseAgent={redeemingBudgetPauseAgent}
        latestBudgetPauseMessageIdByAgent={latestBudgetPauseMessageIdByAgent}
      />
    );
  }

  return (
    <article
      data-message-id={message.id}
      className={cn(
        "group/message relative flex gap-2.5 px-4 transition-colors hover:bg-muted/40",
        continuation ? "py-0.5" : "pb-0.5 pt-2",
        threadOpen && "bg-muted/60",
      )}
    >
      <div className="w-9 shrink-0">
        {continuation ? (
          <span className="hidden pt-0.5 text-right text-3xs leading-5 text-muted-foreground tabular-nums group-hover/message:block">
            {formatTime(message.at)}
          </span>
        ) : (
          // The face in the gutter is the way into who is talking (issue
          // #1653) — for a voice that resolves to a roster teammate. A desk,
          // the company and "you" have no profile behind them and stay plain.
          <AgentAvatarButton agentId={sender.agentId} name={sender.name}>
            <TeammateAvatar
              name={sender.name}
              tone={sender.tone}
              avatar={sender.avatar}
              company={sender.kind === "company"}
              className="size-9"
            />
          </AgentAvatarButton>
        )}
      </div>

      <div className="flex min-w-0 flex-1 flex-col">
        {!continuation && (
          <AuthorLine
            sender={sender}
            at={message.at}
            // One predicate, shared with `ThreadPanel`: neither the reader's
            // own line nor another signed-in person's is the echo brain's, and
            // both arrive as `from: "company"`. `system` never reaches here.
            placeholder={echoMarkerFor(message, sender, cognition)}
          />
        )}
        <Markdown mentions={message.mentions} className="text-sm leading-6 break-words prose-p:my-0 prose-pre:my-1.5 prose-ul:my-1 prose-ol:my-1 prose-headings:my-1">{message.text}</Markdown>

        {message.attachments && message.attachments.length > 0 && (
          <MessageAttachments
            attachments={message.attachments}
            resolveUrl={resolveAttachmentUrl}
          />
        )}

        {message.steps && message.steps.length > 0 && <StepTimeline steps={message.steps} />}
        {message.taskId && (
          <div className="flex flex-wrap items-center gap-2">
            <CardChip
              taskId={message.taskId}
              busy={dismissingCardId === message.taskId}
              disabled={dismissingCardId !== null && dismissingCardId !== message.taskId}
              onDismiss={onDismissCard}
            />
            {taskWorking && (
              <div className="mt-1.5 flex min-w-0 items-center gap-2">
                <WorkingIndicator
                  srLabel="This task is still working."
                  className="shrink-0 px-2 py-0.5 text-2xs"
                />
                {elapsed && (
                  <span className="text-2xs text-muted-foreground tabular-nums">
                    {elapsed}
                  </span>
                )}
              </div>
            )}
          </div>
        )}

        {chips.length > 0 && (
          <Reactions
            chips={chips}
            disabledReason={actionsUnavailable}
            onReact={(e) => onReact(message.id, e)}
          />
        )}

        {replies.length > 0 && (
          <button
            type="button"
            onClick={() => onOpenThread(message.id)}
            className="mt-1 flex w-fit items-center gap-2 rounded-md px-1.5 py-1 text-xs font-medium text-primary transition-colors hover:bg-accent"
          >
            <ReplyFacepile senders={entry.replySenders} />
            {replies.length} {replies.length === 1 ? "reply" : "replies"}
            {/* "Last reply" asks how long a thread has been quiet, and a
                wall-clock time is the one form that does not answer it: on a
                transcript older than a day it is ambiguous without the date,
                and inside one day it makes the reader do the subtraction. The
                day divider above already carries "Today", so the absolute time
                was doubly redundant on the common case (issue #1328). */}
            <span className="font-normal text-muted-foreground">
              · Last reply {timeAgo(replies[replies.length - 1].at, Date.now())}
            </span>
          </button>
        )}
      </div>

      <ActionBar
        onReply={() => onOpenThread(message.id)}
        onReact={(emoji) => onReact(message.id, emoji)}
        reacted={(emoji) => hasReacted(message.reactions, emoji)}
        disabledReason={actionsUnavailable}
      />
    </article>
  );
}

/**
 * A centred system line — an approval decision, or a dispatch marker (issue
 * #377).
 *
 * The pill becomes a **link to the card** when the line names one. That is what
 * makes a marker useful rather than merely informative: `finished → Paused`
 * tells a reader the run stopped, and the next thing they want is the card
 * itself. Without the link they would have to find it on the board by title.
 *
 * A system line with no card — every approval line — renders exactly as it
 * always has, as plain text. There is no icon and no chip here on purpose: this
 * is one short sentence, and dressing it up would give a status line more visual
 * weight than the messages around it.
 *
 * **Except one** (issue #1846): a budget-pause notice is a terminal state the
 * operator has exactly one lever for, and a plain sentence buries that lever.
 * It renders as a highlighted card with an "Add credits" button instead of the
 * plain pill every other system line still gets.
 */
function SystemPill({
  message,
  onRedeemBudgetPause,
  redeemingBudgetPauseAgent,
  latestBudgetPauseMessageIdByAgent,
}: {
  message: ChatMessage;
  // Issue #1846 review (Codex #3868962374): carries `message.id` alongside
  // the agent id, so the caller can bind the redeem to the SPECIFIC marker
  // this card was rendered from — see `ChatView.redeemBudgetPause`'s doc for
  // why a live re-read at click time cannot do that on its own.
  onRedeemBudgetPause?: (agentId: string, noticeMessageId: string) => void;
  redeemingBudgetPauseAgent?: string | null;
  latestBudgetPauseMessageIdByAgent?: Map<string, string>;
}) {
  const className =
    "rounded-full bg-muted px-3 py-1 text-center text-xs text-muted-foreground";

  // Issue #1846 review (Codex #3870168372): extracted to `BudgetPauseNoticeCard`
  // so `ThreadPanel` can render the SAME card for a notice that answered a
  // thread reply — see that component's own doc.
  if (isBudgetPauseNotice(message.text)) {
    return (
      <BudgetPauseNoticeCard
        message={message}
        onRedeemBudgetPause={onRedeemBudgetPause}
        redeemingBudgetPauseAgent={redeemingBudgetPauseAgent}
        latestBudgetPauseMessageIdByAgent={latestBudgetPauseMessageIdByAgent}
      />
    );
  }

  return (
    <div className="flex justify-center px-4 py-1">
      {message.taskId ? (
        <a
          href={`#/tasks/${encodeURIComponent(message.taskId)}`}
          className={cn(className, "transition-opacity hover:opacity-80 hover:underline")}
        >
          {message.text}
        </a>
      ) : (
        <p className={className}>{message.text}</p>
      )}
    </div>
  );
}

function AuthorLine({
  sender,
  at,
  placeholder,
}: {
  sender: Sender;
  at: number;
  /**
   * Why this line is not authored by the voice above it (issue #1734), or
   * `null` when it is. Carries the cause rather than a flag so the chip's
   * tooltip can name it — see `EchoPlaceholder`.
   */
  placeholder?: CognitionState | null;
}) {
  const openProfile = useAgentProfileOpener();
  const { agentId } = sender;
  // The name is the other half of the same target as the avatar beside it: a
  // reader who wants to know who this is aims at whichever of the two their
  // eye landed on.
  const name = agentId && openProfile ? (
    <button
      type="button"
      onClick={() => openProfile(agentId)}
      className="truncate rounded-sm text-sm font-semibold tracking-tight hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring"
    >
      {sender.name}
    </button>
  ) : (
    <span className="truncate text-sm font-semibold tracking-tight">{sender.name}</span>
  );
  return (
    <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 leading-5">
      {name}
      {placeholder && <EchoPlaceholder author={sender.name} cause={placeholder} />}
      <span className="shrink-0 text-2xs text-muted-foreground tabular-nums">
        {formatTime(at)}
      </span>
    </div>
  );
}

/**
 * The reaction chips under a line.
 *
 * Each chip carries a count *and* names everyone behind it in its tooltip —
 * "who reacted" is most of what a reaction is for, and a bare count answers
 * none of it. The reader's own chip is highlighted, which is what makes it
 * legible as a toggle rather than a tally that only goes up.
 */
function Reactions({
  chips,
  disabledReason,
  onReact,
}: {
  chips: ReactionChip[];
  disabledReason?: string;
  onReact: (emoji: string) => void;
}) {
  return (
    <div className="mt-1 flex flex-wrap gap-1">
      {chips.map((chip) => (
        <button
          key={chip.emoji}
          type="button"
          disabled={!!disabledReason}
          onClick={() => onReact(chip.emoji)}
          title={disabledReason ?? `${chip.by.join(", ")} reacted with ${chip.emoji}`}
          className={cn(
            "flex min-h-6 items-center gap-1 rounded-full border px-2 py-0.5 text-xs transition-colors md:min-h-0",
            chip.mine
              ? "border-primary/40 bg-primary/10 hover:bg-primary/20"
              : "border-border bg-muted/60 hover:bg-muted",
            disabledReason && "cursor-not-allowed opacity-60 hover:bg-inherit",
          )}
          aria-pressed={chip.mine}
          aria-label={`${chip.emoji} — ${chip.by.join(", ")}`}
        >
          <span aria-hidden>{chip.emoji}</span>
          <span className="tabular-nums text-2xs font-medium">{chip.count}</span>
        </button>
      ))}
    </div>
  );
}

/** The hover-revealed react/reply strip, pinned to the row's top-right. */
function ActionBar({
  onReply,
  onReact,
  reacted,
  disabledReason,
}: {
  onReply: () => void;
  onReact: (emoji: string) => void;
  /** Whether the reader has already reacted with an emoji, to mark the button. */
  reacted: (emoji: string) => boolean;
  /** Why both actions are unavailable, shown as the tooltip when they are. */
  disabledReason?: string;
}) {
  const disabled = !!disabledReason;
  return (
    <div className="absolute -top-3 right-4 z-10 hidden items-center gap-0.5 rounded-lg border bg-popover p-0.5 shadow-sm group-hover/message:flex group-focus-within/message:flex">
      {QUICK_REACTIONS.map((emoji) => (
        <button
          key={emoji}
          type="button"
          disabled={disabled}
          onClick={() => onReact(emoji)}
          title={disabledReason}
          aria-pressed={reacted(emoji)}
          className={cn(
            "flex size-7 items-center justify-center rounded-md text-sm transition-colors hover:bg-accent",
            reacted(emoji) && "bg-primary/10",
            disabled && "cursor-not-allowed opacity-50 hover:bg-transparent",
          )}
          aria-label={`React with ${emoji}`}
        >
          <span aria-hidden>{emoji}</span>
        </button>
      ))}
      <span className="mx-0.5 h-4 w-px bg-border" aria-hidden />
      <Button
        variant="ghost"
        size="icon"
        className="size-7"
        disabled={disabled}
        onClick={onReply}
        aria-label="Reply in thread"
        title={disabledReason ?? "Reply in thread"}
      >
        <MessageSquareReply className="size-4" />
      </Button>
    </div>
  );
}

/**
 * Who is in this thread, as faces (issue #1324).
 *
 * This used to seed each tile on `message.channel` and draw it `markOnly` —
 * and both halves defeated it. Every reply in a thread carries the same
 * channel, so all three tiles hashed to one tone; and `markOnly` renders an
 * *empty* tile by design, because 16px is below the size at which initials or
 * a mascot can be read. The result was three identical featureless grey
 * squares that read as a loading skeleton, carrying no information at all.
 *
 * Both are fixed by the same change. The senders arrive already resolved and
 * deduped from {@link TimelineEntry.replySenders}, so the tiles are genuinely
 * different people; and at 20px — the size the rail already draws a DM's face
 * at, legibly — the real mascot fits, so there is nothing left for `markOnly`
 * to apologise for.
 */
function ReplyFacepile({ senders }: { senders: Sender[] }) {
  // At most three faces — beyond that the count carries the information.
  const shown = senders.slice(0, 3);
  if (shown.length === 0) return null;
  return (
    <span className="flex -space-x-1.5" aria-hidden>
      {shown.map((s) => (
        <TeammateAvatar
          key={s.key}
          name={s.name}
          tone={s.tone}
          avatar={s.avatar}
          company={s.kind === "company"}
          className="size-5 rounded-[4px] text-3xs ring-1 ring-background"
        />
      ))}
    </span>
  );
}
