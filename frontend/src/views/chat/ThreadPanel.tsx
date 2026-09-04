import { TriangleAlert, X } from "lucide-react";

import { Markdown } from "@/components/markdown";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Button } from "@/components/ui/button";
import type { MessageIntent } from "@/api/tasks";
import type { AttachmentDto, CognitionState, TurnStep } from "@/api/types";
import type { ChatMessage } from "@/lib/chat";
import type { TeamMember } from "@/lib/team";
import { cn } from "@/lib/utils";
import { BudgetPauseNoticeCard } from "./BudgetPauseNoticeCard";
import { EchoPlaceholder, echoMarkerFor } from "./EchoPlaceholder";
import { FailedSendNotice } from "./MessageRow";
import { MessageAttachments } from "./MessageAttachments";
import { StepTimeline } from "./StepTimeline";
import { MessageComposer } from "./MessageComposer";
import { TypingLine } from "./TypingLine";
import { WorkingIndicator } from "./WorkingIndicator";
import { channelTitle, formatTime, senderOf, type Channel } from "./model";
import { type Mention, type Mentionable } from "./mentions";

interface Props {
  channel: Channel;
  /**
   * The roster, so a reply's own line can resolve its sender's mascot the
   * same way the main timeline does — `senderOf` needs it to look a named
   * agent up by id (issue #1185).
   */
  members: TeamMember[];
  /** The message the thread hangs off. */
  parent: ChatMessage;
  replies: ChatMessage[];
  /**
   * The subset of `replies` already laid out inline in the channel, from
   * {@link inlineReplyIds} — excluded from the count above the list, never
   * from the list itself.
   *
   * The two are different questions and were being answered by one number.
   * The channel's chip counts what is left to open (`buildTimeline` filters
   * the promoted reply out of `replies`, deliberately: "a reader seeing the
   * answer must not be told there is one more thing to open"), while this
   * panel counted every descendant `repliesInThread` walked to. A capped turn
   * emits two replies — the agent's write-up and the host's pause notice — so
   * the chip said "1 reply", the header said "2 replies", and the same thread
   * reported two sizes on screen at once.
   *
   * Counting the same set the chip counts settles that. The promoted reply
   * still *renders* here, because the notice under it opens "The reply above
   * is a pause" — drop the reply above and that sentence points at nothing.
   * Shown but not counted is the honest reading: it is context the reader has
   * already seen in the channel, not a further thing to open.
   */
  inlineReplyIds?: ReadonlySet<string>;
  /**
   * Live rows per query, keyed by the asking message's id.
   *
   * The panel needs its own copy because `buildTimeline` keeps every parented
   * line out of the channel timeline: a query typed into an open thread renders
   * here or nowhere. Passing this only to `MessageTimeline` left such a turn
   * with no render path at all (Codex on #2069).
   */
  liveStepsByMessage?: Record<string, TurnStep[]>;
  sending: boolean;
  /**
   * Everything an `@` can name here (issue #1645). Drawn from the parent
   * ChatView's directory so the thread composer shares the same roster.
   * Absent when the host predates the route, or when the directory has not
   * loaded — the composer degrades to plain-text typing.
   */
  mentionables?: Mentionable[];
  /**
   * The ids of the teammates on the channel this thread belongs to, for the
   * composer's outside-channel warning. Absent when membership is unknown.
   */
  channelMemberIds?: string[];
  /**
   * Whether the channel this thread belongs to is read-only (issue #1757's
   * Operator channel, `Boolean(channel?.system)` in `ChatView`). The main
   * composer is not rendered on such a channel, but a thread has its own
   * composer — so without this a durable Operator report could still be
   * opened as a thread and replied to there, only for the server's read-only
   * guard to reject it after the text was written. Absent means "no such
   * channel is open", the same as the main composer's default.
   *
   * The panel answers it the way the channel does: **no composer at all**,
   * and a notice in its place saying why. See the render site.
   */
  readOnly?: boolean;
  /**
   * Whether this thread hangs off a settled `in_review` dispatch card's review
   * surface — its settle pill or the relay bubble that followed it. When set, a
   * reply here is review feedback that re-runs the card, so the composer says
   * so instead of reading like an ordinary reply.
   */
  reviewing?: boolean;
  /**
   * The `reviewing` card's id, so the panel can offer Approve beside its own
   * notice (CodeRabbit #3905116857) — a card settled inside an already-open
   * thread folds its settle pill into a plain reply line with no room for
   * `MessageRow`'s Approve button, so without this the ONLY way to approve
   * such a card was to close the thread and find the pill in the channel.
   * Absent exactly when `reviewing` is, since the anchor is what makes it so.
   */
  reviewTaskId?: string;
  /** Approves or revises {@link reviewTaskId}. Mirrors `MessageRow`'s prop of the same name. */
  onReviewCard?: (taskId: string, decision: "approve" | "revise") => void;
  /** Whether {@link reviewTaskId}'s verdict is already in flight. */
  reviewInFlight?: boolean;
  /**
   * Every OTHER in-review card this thread anchors to, besides
   * {@link reviewTaskId} — `reviewAnchorsForThread`'s entries after its
   * newest (CodeRabbit review on #1981). A card dispatched into an
   * already-open thread before an earlier one settles leaves both live at
   * once, and the newest already owns the notice + Approve above the
   * composer; without a control of its own, the older card was only
   * reachable by first settling the newer one. Each entry here gets its own
   * notice row with its own Approve button instead.
   */
  additionalReviewAnchors?: { taskId: string; anchorId: string }[];
  /**
   * Every task id currently mid-verdict — `ChatView`'s own
   * `reviewingCardIds`. {@link reviewInFlight} already covers
   * {@link reviewTaskId}; this is the same signal for each entry in
   * {@link additionalReviewAnchors}, which has no scalar prop of its own to
   * carry it.
   */
  reviewingTaskId?: ReadonlySet<string>;
  /**
   * Your own avatar reference, so your lines in this thread wear your face
   * (issue #1729).
   *
   * `senderOf` seeds a face off the sender's *name* when it is given none, and
   * a "you" line's name is the literal string "You" — so without this the panel
   * drew whatever mascot `avatarFor("You")` hashes to, which is the agent's
   * green one. Both participants then had the same face and a thread could not
   * be read at all. The main timeline has always passed it (`buildTimeline`);
   * only this panel resolved its senders without it.
   *
   * Absent until `loadViewer` has resolved who you are, exactly as in the main
   * timeline — the tile falls back to the name-seeded mascot for that first
   * render rather than showing nothing.
   */
  youAvatar?: string;
  /**
   * Resolves an attachment's bytes to an object URL for preview/download
   * (issue #1682). Threaded from the parent ChatView like the main timeline's
   * `MessageRow` gets it, so a thread line can render the same chips — a
   * reply with an attachment is legal on the wire and history preserves it,
   * and it was invisible without this path.
   */
  resolveAttachmentUrl?: (nodeId: string) => Promise<string>;
  onSend: (
    text: string,
    intent?: MessageIntent,
    // The thread composer never attaches (no `uploadAttachment`), so this is
    // always absent — kept in the signature so arg alignment with the merged
    // `MessageComposer.onSend` holds and mentions land in the right slot.
    _attachments?: AttachmentDto[],
    mentions?: Mention[],
  ) => void;
  onClose: () => void;
  /**
   * Who is typing *in this thread* — scoped by the parent's own id, never the
   * channel's. Without this the thread panel had no typing signal at all: the
   * wire and `useTyping` already carry `parentId`, but nothing upstream of
   * this component filtered by it or rendered a line for it.
   */
  typingNames?: string[];
  /**
   * A turn open in **this thread**, when one is (issue #1934's other half).
   *
   * The panel used to carry no live-turn state at all, and the channel behind
   * it blanked its own the moment a thread opened — so a turn running in the
   * thread you were reading was visible nowhere. It could only be wired once
   * the shell keyed its open turns per thread rather than per channel; before
   * that there was no way to ask "is *this* thread working".
   */
  openTurn?: { queued: boolean };
  /** This console is typing here. Distinct from the main composer's callback
   * so the ping this thread sends carries the thread's own `parentId`. */
  onTyping?: () => void;
  /**
   * Whether this company's teammates can think (issue #1735), threaded here for
   * the same reason `MessageTimeline` gets it: on either echo state the company
   * side of this panel is the offline echo brain's output, not the teammate's.
   *
   * A thread is not a lesser transcript. The first cut of #1734 marked only the
   * channel timeline, and this panel's own `Line` kept rendering echoed replies
   * under the teammate's name and avatar with nothing to say otherwise — the
   * exact false attribution the fix exists to remove, one click away from where
   * it had been removed (CodeRabbit and codex both caught it on PR #1740).
   */
  cognition?: CognitionState | null;
  /**
   * The Add-Credits CTA (issue #1846 review, Codex #3870168372). A
   * budget-pause notice is journaled with the thread's `parent` set when the
   * exhausted turn was answering a reply, and `buildTimelineItems` routes any
   * message with a `parentId` out of the main channel timeline and into this
   * thread — so a thread-parented notice's CTA has to render HERE, not just
   * in `MessageTimeline`, or it is unreachable no matter how the operator
   * scrolls the main channel.
   */
  onRedeemBudgetPause?: (agentId: string, noticeMessageId: string) => void;
  redeemingBudgetPauseAgent?: string | null;
  latestBudgetPauseMessageIdByAgent?: Map<string, string>;
  /**
   * Retries a reply that failed to send from *this thread's* composer
   * (B-099). `MessageRow` gets the same prop of the same name; without it a
   * threaded reply's `sendFailed` field had nowhere to go, because `Line` is
   * its own renderer and never received it — a failed reply read as
   * delivered, with no way to send it again (Codex review, PR #2052).
   */
  onRetrySend?: (messageId: string) => void;
}

/**
 * The thread panel.
 *
 * Replies live here rather than inline, so a busy exchange never pushes the
 * channel apart. The parent message sits at the top under a rule, and the
 * panel carries its own composer scoped to the thread.
 */
export function ThreadPanel({
  channel,
  members,
  parent,
  replies,
  inlineReplyIds,
  liveStepsByMessage,
  sending,
  mentionables,
  channelMemberIds,
  readOnly,
  youAvatar,
  resolveAttachmentUrl,
  onSend,
  reviewing,
  reviewTaskId,
  onReviewCard,
  reviewInFlight,
  additionalReviewAnchors,
  reviewingTaskId,
  onClose,
  typingNames = [],
  openTurn,
  onTyping,
  cognition,
  onRedeemBudgetPause,
  redeemingBudgetPauseAgent,
  latestBudgetPauseMessageIdByAgent,
  onRetrySend,
}: Props) {
  // Absent `inlineReplyIds` counts everything, which is what every caller did
  // before the prop existed: a panel that cannot know what the channel
  // promoted must not silently under-count.
  const countedReplies = inlineReplyIds
    ? replies.reduce((n, r) => (inlineReplyIds.has(r.id) ? n : n + 1), 0)
    : replies.length;
  return (
    <aside className="flex w-96 shrink-0 flex-col border-l bg-background">
      <header className="flex h-13 shrink-0 items-center gap-2 border-b px-3">
        <div className="min-w-0 flex-1">
          <h2 className="text-sm font-semibold tracking-tight">Thread</h2>
          <p className="truncate text-xs text-muted-foreground">{channelTitle(channel)}</p>
        </div>
        <Button variant="ghost" size="icon" className="size-8" onClick={onClose} aria-label="Close thread">
          <X className="size-4" />
        </Button>
      </header>

      <div className="flex-1 overflow-y-auto">
        <Line
          channel={channel}
          members={members}
          message={parent}
          liveSteps={liveStepsByMessage?.[parent.id]}
          youAvatar={youAvatar}
          resolveAttachmentUrl={resolveAttachmentUrl}
          cognition={cognition}
          onRedeemBudgetPause={onRedeemBudgetPause}
          redeemingBudgetPauseAgent={redeemingBudgetPauseAgent}
          latestBudgetPauseMessageIdByAgent={latestBudgetPauseMessageIdByAgent}
          onRetrySend={onRetrySend}
        />
        <div className="flex items-center gap-2 px-4 py-2">
          <span className="text-xs font-medium text-muted-foreground">
            {countedReplies} {countedReplies === 1 ? "reply" : "replies"}
          </span>
          <span className="h-px flex-1 bg-border" aria-hidden />
        </div>
        {replies.map((r) => (
          <Line
            key={r.id}
            channel={channel}
            members={members}
            message={r}
            liveSteps={liveStepsByMessage?.[r.id]}
            youAvatar={youAvatar}
            resolveAttachmentUrl={resolveAttachmentUrl}
            cognition={cognition}
            onRedeemBudgetPause={onRedeemBudgetPause}
            redeemingBudgetPauseAgent={redeemingBudgetPauseAgent}
            onRetrySend={onRetrySend}
            latestBudgetPauseMessageIdByAgent={latestBudgetPauseMessageIdByAgent}
          />
        ))}
      </div>

      {/* A read-only thread gets the notice and no composer, the way its
          channel does. The panel used to render a *disabled* composer with the
          placeholder "This channel is read-only" — but a disabled reply box is
          still a claim that replying is a thing you do here, and it was the
          only thing this panel said on the subject. The explanation is what
          should occupy the space; the affordance should not be there at all.

          `noopSend` went with it: with no composer there is nothing left to
          wire a no-op to. The belt that mattered is the server's read-only
          guard (issue #1757), which is untouched, plus `ChatView`'s own
          `if (readOnly) return;` before it calls `client.chat`. */}
      {readOnly ? (
        <p
          role="status"
          data-testid="thread-read-only-notice"
          className="flex shrink-0 items-center gap-1.5 border-t bg-muted/50 px-3 py-1.5 text-xs text-muted-foreground"
        >
          <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
          <span className="min-w-0">
            The <span className="font-medium text-foreground">Operator</span> channel is a
            read-only feed of workflow reports and notifications. There is nothing to reply to
            here.
          </span>
        </p>
      ) : (
        <>
          {openTurn && (
            <div className="px-4 py-2">
              <WorkingIndicator
                srLabel={openTurn.queued ? "Queued…" : "Replying…"}
                queued={openTurn.queued}
              />
            </div>
          )}
          <TypingLine names={typingNames} />
          {reviewing && (
            <div className="flex items-center justify-between gap-2 border-t bg-muted/40 px-4 py-1.5">
              <p className="text-xs text-muted-foreground">
                This card is ready for review. A reply sends it back for another pass
                with your notes.
              </p>
              {reviewTaskId !== undefined && onReviewCard !== undefined && (
                <Button
                  size="sm"
                  variant="outline"
                  className="h-6 shrink-0 px-2 text-xs"
                  disabled={reviewInFlight}
                  onClick={() => onReviewCard(reviewTaskId, "approve")}
                >
                  {reviewInFlight ? "Approving…" : "Approve"}
                </Button>
              )}
            </div>
          )}
          {additionalReviewAnchors?.map((anchor) => (
            <div
              key={anchor.taskId}
              className="flex items-center justify-between gap-2 border-t bg-muted/40 px-4 py-1.5"
            >
              <p className="text-xs text-muted-foreground">
                Another card in this thread is also ready for review.
              </p>
              {onReviewCard !== undefined && (
                <Button
                  size="sm"
                  variant="outline"
                  className="h-6 shrink-0 px-2 text-xs"
                  disabled={reviewingTaskId?.has(anchor.taskId) ?? false}
                  onClick={() => onReviewCard(anchor.taskId, "approve")}
                >
                  {reviewingTaskId?.has(anchor.taskId) ? "Approving…" : "Approve"}
                </Button>
              )}
            </div>
          ))}
          <MessageComposer
            compact
            placeholder={reviewing ? "Send for another pass…" : "Reply…"}
            disabled={sending}
            mentionables={mentionables}
            channelMemberIds={channelMemberIds}
            onSend={onSend}
            onTyping={onTyping}
          />
        </>
      )}
    </aside>
  );
}

function Line({
  channel,
  members,
  message,
  liveSteps,
  youAvatar,
  resolveAttachmentUrl,
  cognition,
  onRedeemBudgetPause,
  redeemingBudgetPauseAgent,
  latestBudgetPauseMessageIdByAgent,
  onRetrySend,
}: {
  channel: Channel;
  members: TeamMember[];
  message: ChatMessage;
  /** This message's in-flight turn rows, if one is running (see `MessageRow`). */
  liveSteps?: readonly TurnStep[];
  youAvatar?: string;
  resolveAttachmentUrl?: (nodeId: string) => Promise<string>;
  cognition?: CognitionState | null;
  onRedeemBudgetPause?: (agentId: string, noticeMessageId: string) => void;
  redeemingBudgetPauseAgent?: string | null;
  latestBudgetPauseMessageIdByAgent?: Map<string, string>;
  onRetrySend?: (messageId: string) => void;
}) {
  // Four arguments, not three: `youAvatar` is the last parameter, and omitting
  // it left your own line with no avatar to seed from but the name "You" —
  // which collided with the agent's face (issue #1729).
  const sender = senderOf(message, channel, members, youAvatar);
  // Literally the same predicate `MessageRow` uses, not a second copy of it —
  // two spellings of "which rows are the echo brain's" is how this panel came
  // to be missing the marker in the first place.
  const placeholder = echoMarkerFor(message, sender, cognition);

  if (sender.kind === "system") {
    // Issue #1846 review (Codex #3870168372): a budget-pause notice gets the
    // SAME highlighted card + CTA `MessageRow` renders in the main timeline
    // — see `BudgetPauseNoticeCard`'s own doc for why this thread panel is
    // sometimes the ONLY place such a notice is visible at all. Falls back
    // to the plain system line for every other system message.
    // Called as a plain function, not as JSX: `BudgetPauseNoticeCard` is a
    // pure "maybe-render" component (no hooks, no state) that returns `null`
    // for a non-notice message — JSX-invoking it (`<BudgetPauseNoticeCard
    // .../>`) would always produce a truthy element descriptor regardless of
    // what it renders internally, defeating the `??` fallback below.
    const budgetPauseCard = BudgetPauseNoticeCard({
      message,
      onRedeemBudgetPause,
      redeemingBudgetPauseAgent,
      latestBudgetPauseMessageIdByAgent,
    });
    return (
      budgetPauseCard ?? (
        <p className="px-4 py-1 text-center text-xs text-muted-foreground">{message.text}</p>
      )
    );
  }

  return (
    <div className="flex gap-2.5 px-4 py-2">
      <TeammateAvatar
        name={sender.name}
        tone={sender.tone}
        avatar={sender.avatar}
        company={sender.kind === "company"}
        className="size-8"
        // Named by whose line it is, so a spec can assert that your face and
        // the agent's are two different faces (issue #1729) rather than
        // counting `img` elements in DOM order.
        data-testid={`thread-avatar-${sender.kind}`}
      />
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-baseline gap-x-2">
          <span className="truncate text-sm font-semibold tracking-tight">{sender.name}</span>
          {placeholder && <EchoPlaceholder author={sender.name} cause={placeholder} />}
          <span className="shrink-0 text-2xs text-muted-foreground tabular-nums">
            {formatTime(message.at)}
          </span>
        </div>
        <Markdown
          mentions={message.mentions}
          className={cn(
            "text-sm leading-6 break-words prose-p:my-0 prose-pre:my-1.5 prose-ul:my-1 prose-ol:my-1 prose-headings:my-1",
            // Same rule and the same reason as `MessageRow` (B-099): a threaded
            // reply that never left the browser must not read as delivered just
            // because this panel draws replies with its own renderer.
            message.sendFailed !== undefined && "text-muted-foreground",
          )}
        >
          {message.text}
        </Markdown>
        {message.sendFailed !== undefined && (
          <FailedSendNotice
            reason={message.sendFailed || "something went wrong"}
            onRetry={onRetrySend ? () => onRetrySend(message.id) : undefined}
          />
        )}
        {message.attachments && message.attachments.length > 0 && (
          <MessageAttachments
            attachments={message.attachments}
            resolveUrl={resolveAttachmentUrl}
          />
        )}
        {/* The same two step blocks `MessageRow` renders, because a message
            asked or answered inside a thread is not a lesser message.
            `buildTimeline` keeps every parented line OUT of the channel
            timeline, so this panel is the only surface a threaded query has —
            without these, a turn started from an open thread showed no account
            of itself anywhere, even after the panel was closed (Codex on
            #2069). */}
        {message.steps && message.steps.length > 0 && <StepTimeline steps={message.steps} />}
        {!!liveSteps?.length && <StepTimeline steps={[...liveSteps]} defaultOpen />}
      </div>
    </div>
  );
}
