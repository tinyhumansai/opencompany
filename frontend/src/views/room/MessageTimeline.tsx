import { useMemo } from "react";
import { Bot, CircleDot, Hash, Lock, Send, UserPlus } from "lucide-react";

import type { ApprovalSummary, CognitionState, DecideApproval, TurnStep, Verdict } from "@/api/types";
import type { TaskStatus } from "@/api/tasks";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import { ApprovalRow } from "./ApprovalRow";
import { ChatLiveReceipt, type ChatReceipt } from "./ChatLiveReceipt";
import { EpisodeCompleteMarker } from "./EpisodeCompleteMarker";
import { EpisodeWaitingMarker } from "./EpisodeWaitingMarker";
import { RoundBand } from "./RoundBand";
import { MessageRow } from "./MessageRow";
import { StepTimeline } from "./StepTimeline";
import { WorkingIndicator } from "./WorkingIndicator";
import {
  channelIntroSentence,
  channelTitle,
  dmFace,
  type Channel,
  type TimelineItem,
} from "./model";
import { JumpToLatest } from "./JumpToLatest";
import { useBottomAnchor } from "./useBottomAnchor";

interface Props {
  channel: Channel;
  /**
   * The channel's rows: messages, and the approvals raised in this channel
   * interleaved among them by time (#379). A card is its own kind rather than a
   * message — see {@link TimelineItem}.
   */
  items: TimelineItem[];
  /**
   * This channel's persisted history has not arrived yet, so the absence of
   * rows is not evidence of anything (issue #934).
   */
  historyPending?: boolean;
  /** The message whose thread is open, if any — that row stays highlighted. */
  openThreadId: string | null;
  /** Someone on the company side is composing a reply. */
  typing: boolean;
  /**
   * The turn is accepted but has not started — queued on the per-company serial
   * lock rather than working (issue #983). Words the row honestly instead of
   * showing a spinner that implies progress.
   */
  queued?: boolean;
  /**
   * The tool rows of a turn running *right now* on this channel's thread, off
   * the transient `tool_call` / `tool_result` / `thinking` frames. Present
   * whether or not this console started the turn, which is the point — a turn
   * an inbound message kicked off shows its work here too (issue #367).
   */
  liveSteps?: TurnStep[];
  /**
   * Live rows per **query**, keyed by the asking message's console id — the
   * per-turn half of `liveSteps` above, which is the per-thread strip.
   *
   * Both exist because a frame only knows which query it belongs to when the
   * host stamps `messageSeq` on it. One that does files under its query; one
   * that does not (a relay, a dispatched card, an older host) falls back to
   * the thread. They are filled exclusively — never both.
   *
   * The query decides which **bucket** the rows land in, never where they
   * render: the live pair is pinned to the foot of the pane either way. A
   * "happening now" row placed back at the asking message claims the work
   * finished before every line beneath it, which is false the moment anything
   * is journaled in between — a hive episode posts a seat's line per turn, so
   * by convergence the pulsing row sits several messages up while everything
   * below it has already happened.
   */
  liveStepsByMessage?: Record<string, TurnStep[]>;
  /**
   * Who last reported on each open turn, keyed exactly as its rows are.
   *
   * The live answer to "who is working", read in preference to
   * {@link turnAgentId} — which names whoever the host started the turn on and
   * never revises, so it cannot follow a desk hand-off or a room passing the
   * floor between seats.
   */
  liveAgentByTurn?: Record<string, string>;
  /**
   * The live receipt for a synchronous chat turn this console just sent (issue
   * #1934). When present it supersedes {@link TypingRow} — it says "Sent →
   * Picked up → on step" with a ticking clock instead of bare typing dots — and
   * folds the same {@link StepTimeline} below the line when steps exist. Absent
   * for an inbound turn this console never started, which still falls to the
   * `liveSteps`/`typing` rows below.
   */
  receipt?: ChatReceipt;
  /**
   * Who the host recorded as answering the open turn, as a roster id.
   *
   * Only the reload leg needs it. A console that sent the turn itself has a
   * {@link receipt}, which names the teammate off the first live frame and
   * supersedes the rows below; a console that reloaded has neither, and this is
   * what lets its re-armed row say who rather than a bare "Working…".
   * Resolved through {@link agentNames} here, never rendered raw.
   */
  turnAgentId?: string;
  /** Roster agent id → display name, so the receipt never shows a raw id. */
  agentNames?: Record<string, string>;
  onOpenThread: (messageId: string) => void;
  onReact: (messageId: string, emoji: string) => void;
  /** Deletes the board card a line opened, and drops its chip (issue #984). */
  onDismissCard: (taskId: string) => void;
  /** The card whose delete is in flight, if any. */
  dismissingCardId: string | null;
  /**
   * Settles the in-review card a finished card's settle pill links to — the
   * Approve control on that pill. Absent where review is not wired.
   */
  onReviewCard?: (taskId: string, decision: "approve" | "revise") => void;
  /** Every card whose review verdict is in flight, if any. */
  reviewingCardIds?: ReadonlySet<string>;
  /**
   * Resolves a stored attachment's bytes to an object URL for the transcript
   * (issue #1682). Threaded from the shell, which holds the authenticated
   * client the blob route needs. Absent where nothing renders attachments.
   */
  resolveAttachmentUrl?: (nodeId: string) => Promise<string>;
  /** Board task id -> live state for card-linked background turns (#1758). */
  taskStatusByTaskId?: Readonly<Record<string, TaskStatus>>;
  /** Sends a line whose POST never completed again (B-099), by its id. */
  onRetrySend?: (messageId: string) => void;
  /**
   * Places a first brief into the composer on an empty channel.
   * Optional so the thread panel — which renders no intro — need not pass it.
   */
  onStartBrief?: () => void;
  /**
   * Opens the members pane, for the "Add people" card on an empty channel.
   * Optional so the thread panel — which renders no intro — need not pass it.
   */
  onAddPeople?: () => void;
  /** Now, for the cards' "waiting N minutes" line. Owned by the shell's feed. */
  now?: number;
  /** Agent id → display name, for a card's "Asked by" line. */
  askerNames?: Map<string, string>;
  /** Host thread id → console channel id, for a card's origin link. */
  chatChannelByThread?: Readonly<Record<string, string>>;
  /** The verdict each inline card is currently waiting on. */
  decidingApprovals?: ReadonlyMap<string, Verdict>;
  /** Decisions that did not land, per approval id (#842) — see `ApprovalRow`. */
  failedApprovals?: Record<string, string>;
  onDecideApproval?: DecideApproval;
  /**
   * Whether this company's teammates can think (issue #1735). On either echo
   * state every company-side row below is a canned line rather than a
   * teammate's answer (issue #1734). Passed straight through to `MessageRow`,
   * which explains why this is a company-level fact and not a per-message one,
   * and why it carries the cause rather than a boolean.
   */
  cognition?: CognitionState | null;
  /**
   * The Add-Credits CTA (issue #1846). Passed straight through to
   * `MessageRow`, which is why the signature carries the clicked notice's
   * own `message.id` alongside the agent id (issue #1846 review, Codex
   * #3868962374) — see `MessageRow`'s doc.
   */
  onRedeemBudgetPause?: (agentId: string, noticeMessageId: string) => void;
  redeemingBudgetPauseAgent?: string | null;
  /**
   * The message id of the MOST RECENT budget-pause notice per agent,
   * COMPANY-WIDE (issue #1846 review, Codex #3865395879).
   *
   * Computed by the caller from every channel's transcript, not just this
   * one — the backend parks at most one marker per agent regardless of which
   * channel the pause happened in, so this has to match that scope. Passed
   * straight through to `MessageRow`, the same way `redeemingBudgetPauseAgent`
   * is.
   */
  latestBudgetPauseMessageIdByAgent?: Map<string, string>;
}

/**
 * The scrolling body of a channel.
 *
 * Rows are bottom-anchored: the view sticks to the newest message, which is
 * what a chat log wants and what a plain scroll container does not do on its
 * own. Day dividers ride along as sticky pills so the date stays legible while
 * you scroll through it.
 *
 * Anchoring is two rules, not one (issue #757):
 *
 * 1. **Arriving at a channel jumps, it does not travel.** Opening a channel
 *    anchors instantly in a layout effect, before the browser paints, so the
 *    first frame the operator sees is already the newest message. Animating
 *    here would be animating from a position that was never theirs — and the
 *    longer the transcript, the longer the slide.
 * 2. **Growth follows, but only if they are already at the bottom.** A tool row
 *    or reply arriving while they watch should glide into view; the same row
 *    arriving while they have scrolled up to read history must not yank the
 *    viewport away from what they are reading.
 */
export function MessageTimeline({
  channel,
  items,
  historyPending = false,
  openThreadId,
  typing,
  queued,
  liveSteps,
  liveStepsByMessage,
  liveAgentByTurn,
  receipt,
  turnAgentId,
  agentNames,
  onOpenThread,
  onReact,
  onDismissCard,
  dismissingCardId,
  onReviewCard,
  reviewingCardIds,
  resolveAttachmentUrl,
  taskStatusByTaskId,
  onRetrySend,
  onStartBrief,
  onAddPeople,
  now,
  askerNames,
  chatChannelByThread,
  decidingApprovals,
  failedApprovals,
  onDecideApproval,
  cognition,
  onRedeemBudgetPause,
  redeemingBudgetPauseAgent,
  latestBudgetPauseMessageIdByAgent,
}: Props) {
  /**
   * The open turn's rows, whichever bucket the host's stamping filed them in.
   *
   * `liveStepsByMessage` and `liveStepsByThread` are filled **exclusively** — a
   * frame carrying `messageSeq` files under its query, one without files under
   * the thread (`AppShell.onTurnEvent`, whose own comment is "never both") — so
   * one turn's rows live in exactly one of them and reading the union cannot
   * double-count.
   *
   * Only half of it was reaching the live rows. `ChatLiveReceipt` reads these
   * to reach its third state, "On step <label>"; given the thread half alone, a
   * turn the host stamped left its bucket empty, so the receipt sat on "Picked
   * up by <name>" for the whole turn while the steps surfaced somewhere else
   * entirely.
   *
   * Newest first: a channel can hold rows for more than one query, and the open
   * turn is the most recent that has any. Walking `items` rather than the map's
   * own key order because only the timeline knows which question came last.
   */
  const openTurn = useMemo(() => {
    if (liveSteps?.length) return { steps: liveSteps, key: undefined as string | undefined };
    if (!liveStepsByMessage) return undefined;
    for (let i = items.length - 1; i >= 0; i -= 1) {
      const item = items[i];
      if (item.kind !== "message") continue;
      const id = item.entry.message.id;
      const rows = liveStepsByMessage[id];
      if (rows?.length) return { steps: rows, key: id };
    }
    return undefined;
  }, [liveSteps, liveStepsByMessage, items]);
  const openTurnSteps = openTurn?.steps;
  const liveStepCount = openTurnSteps?.length ?? 0;
  // Resolved once, for both live rows below. Kept here rather than inside them
  // so the receipt's "never a raw id" rule holds in one place: an id this map
  // does not know yields no name, and the row says "Working…" as it always did.
  // The live agent wins: `turnAgentId` names whoever the host started the turn
  // on and is never revised, so on its own the row kept naming the opening
  // responder through a hand-off and through every seat of a room. The
  // fallback still covers the reload leg, whose re-armed row has no frames yet.
  const liveAgentId = openTurn?.key ? liveAgentByTurn?.[openTurn.key] : undefined;
  const resolvedTurnAgentId = liveAgentId ?? turnAgentId;
  const turnAgentName = resolvedTurnAgentId ? agentNames?.[resolvedTurnAgentId] : undefined;
  // Rows that arrived locally — a message sent before hydration landed — are
  // still worth showing while the rest of the history is in flight. It is only
  // the *claim of emptiness* that has to wait.
  const loading = historyPending && items.length === 0;
  /**
   * The channel has answered and has nothing in it.
   *
   * Distinct from `loading`: both have no rows, but only this one is a *claim*
   * that there are none. It drives the intro's copy, its action cards, and —
   * since #1323 — which end of the pane the whole block settles against.
   */
  const empty = items.length === 0 && !loading;
  const { scroller, content, onScroll, atBottom, jumpToLatest } = useBottomAnchor({
    key: channel.id,
    pending: historyPending,
    growth: [items.length, typing, liveStepCount],
  });

  /**
   * One timeline row.
   *
   * Extracted from the `items.map` it used to be inlined in so a
   * {@link RoundBand} can render the very same rows inside itself. A round's
   * utterances are ordinary messages — same avatar gutter, same hover actions,
   * same thread affordances — and a second renderer for them would be a second
   * place for those to drift.
   */
  const renderRow = (item: TimelineItem): React.ReactNode => {
    if (item.kind === "round") {
      return (
        <RoundBand
          key={item.key}
          episode={item.episode}
          round={item.round}
          items={item.items}
          renderRow={renderRow}
          agentNames={agentNames}
        />
      );
    }
    if (item.kind === "episode_complete") {
      return <EpisodeCompleteMarker key={item.key} episode={item.episode} agentNames={agentNames} />;
    }
    if (item.kind === "episode_waiting") {
      return (
        <EpisodeWaitingMarker key={item.key} episode={item.episode} seats={item.seats} agentNames={agentNames} />
      );
    }
    if (item.kind === "message") {
      return (
        <div key={item.key}>
          {item.entry.dayLabel && <DayDivider label={item.entry.dayLabel} />}
          <MessageRow
            entry={item.entry}
            threadOpen={item.entry.message.id === openThreadId}
            onOpenThread={onOpenThread}
            onReact={onReact}
            onDismissCard={onDismissCard}
            dismissingCardId={dismissingCardId}
            onReviewCard={onReviewCard}
            reviewingCardIds={reviewingCardIds}
            resolveAttachmentUrl={resolveAttachmentUrl}
            taskStatusByTaskId={taskStatusByTaskId}
            onRetrySend={onRetrySend}
            now={now ?? Date.now()}
            cognition={cognition}
            onRedeemBudgetPause={onRedeemBudgetPause}
            redeemingBudgetPauseAgent={redeemingBudgetPauseAgent}
            latestBudgetPauseMessageIdByAgent={latestBudgetPauseMessageIdByAgent}
            // Issue #1986: read off `channel.system` here rather than threaded
            // down from `RoomView`, because this component already holds the
            // channel and that flag *is* the predicate `RoomView` derives its
            // own `readOnly` from — a second prop carrying the same fact through
            // the same tree is one more thing that can disagree with it. See
            // `MessageRow`'s `readOnly` doc for what it takes away (adding a
            // reaction) and what it deliberately leaves (reactions already
            // there, and the way into a thread).
            readOnly={Boolean(channel.system)}
            agentNames={agentNames}
          />
        </div>
      );
    }
    return (
      <ApprovalRow
        key={item.key}
        approvals={item.approvals}
        now={now ?? Date.now()}
        askerNames={askerNames ?? EMPTY_NAMES}
        chatChannelByThread={chatChannelByThread}
        variant="compact"
        thread={
          item.approvals[0]?.thread
            ? { channelId: channel.id, label: channelTitle(channel) }
            : null
        }
        /* Narrowed to this card's own items (#842): a decision in flight on
           another turn's batch is not this card's business, which is the same
           rule #373 established one level down. */
        deciding={decidingIn(item.approvals, decidingApprovals)}
        decided={item.decided}
        failed={failedApprovals ?? EMPTY_FAILURES}
        onDecide={(approval, verdict, scope) => onDecideApproval?.(approval, verdict, scope)}
      />
    );
  };

  return (
    <div className="relative flex min-h-0 flex-1 flex-col">
      <div ref={scroller}
        onScroll={onScroll}
        data-testid="channel-transcript"
        className="min-h-0 flex-1 overflow-y-auto"
      >
        {/*
         * Which end short content settles against (issue #1323).
         *
         * `justify-end` is right for a *transcript* shorter than the viewport:
         * three messages should sit above the composer the way every chat client
         * puts them, not float in the middle of the pane. It is wrong for a
         * channel with no transcript at all, because then the only thing being
         * bottom-pinned is the intro — a heading, a sentence, and the two action
         * cards that are the whole point of an empty channel — and they end up
         * crushed against the composer under most of a screen of dead canvas.
         * The cards are the primary invitation and they were the last thing the
         * eye reached.
         *
         * So an empty channel reads downward from the top, as the design
         * reference draws it. `empty` is the same value `ChannelIntro` gets, and
         * it is lifted here rather than recomputed so the two cannot disagree
         * about what "empty" means — a channel whose intro claimed emptiness
         * while the wrapper anchored for content would jump on every load.
         */}
        <div
          ref={content}
          className={cn("flex min-h-full flex-col pb-4", empty ? "justify-start" : "justify-end")}
        >
          {/* `empty` only drives the top padding, and the skeleton fills the
              same space real rows will — so a loading channel is spaced like a
              full one and the intro does not jump down and back up. That is also
              why `loading` keeps the *bottom* anchor above: flipping to the top
              while history is in flight would move the intro up and then drop it
              back down the moment the rows land. */}
          <ChannelIntro
            channel={channel}
            empty={empty}
            loading={loading}
            onStartBrief={onStartBrief}
            onAddPeople={onAddPeople}
          />
          {loading && <HistorySkeleton />}
          {items.map(renderRow)}
          {receipt ? (
            // The receipt for our own in-flight send (issue #1934) supersedes the
            // typing dots and carries the live steps itself. It now rides a
            // detached turn past its 202 into the queued/working window too (issue
            // #2021), so `queued` words its base line and stills its pulse rather
            // than dropping it back to the bare "Queued…"/step row.
            <ChatLiveReceipt
              channel={channel}
              receipt={receipt}
              agentNames={agentNames}
              steps={openTurnSteps ?? []}
              queued={queued}
            />
          ) : liveStepCount > 0 && !queued ? (
            <LiveTurnRow
              channel={channel}
              steps={openTurnSteps ?? []}
              name={turnAgentName}
            />
          ) : (
            typing && (
              <TypingRow
                channel={channel}
                queued={queued}
                name={turnAgentName}
              />
            )
          )}
        </div>
      </div>
      {!atBottom && <JumpToLatest onClick={jumpToLatest} />}
    </div>
  );
}

/** Stable identity so a missing `askerNames` cannot churn the card's props. */
const EMPTY_NAMES: Map<string, string> = new Map();

/** Stable identity, for the same reason as {@link EMPTY_NAMES}. */
const EMPTY_DECIDING: ReadonlyMap<string, Verdict> = new Map();

/** Ditto — no failure has been recorded on any card. */
const EMPTY_FAILURES: Record<string, string> = {};

/**
 * The in-flight verdicts belonging to one card's items (#842).
 *
 * The shell keeps one map for the whole console, and a batch card must not
 * spin — or disable its buttons — because a different turn's approval is being
 * decided. Returns the shared empty map when nothing of this card's is in
 * flight, so the common case allocates nothing and the props stay identical
 * between renders.
 */
function decidingIn(
  approvals: ApprovalSummary[],
  deciding: ReadonlyMap<string, Verdict> | undefined,
): ReadonlyMap<string, Verdict> {
  if (!deciding?.size) return EMPTY_DECIDING;
  const mine = new Map<string, Verdict>();
  for (const a of approvals) {
    const verdict = deciding.get(a.id);
    if (verdict) mine.set(a.id, verdict);
  }
  return mine.size ? mine : EMPTY_DECIDING;
}

function DayDivider({ label }: { label: string }) {
  return (
    <div
      aria-label={label}
      className="pointer-events-none sticky top-2 z-20 flex items-center gap-3 px-4 py-2"
    >
      <span className="h-px flex-1 bg-border" aria-hidden />
      <p className="rounded-full border bg-background px-3 py-1 text-2xs font-medium tracking-wide text-muted-foreground">
        {label}
      </p>
      <span className="h-px flex-1 bg-border" aria-hidden />
    </div>
  );
}

/**
 * The block at the very top of a channel, explaining what it is for. It stays
 * above the first message rather than only showing when the channel is empty —
 * scrolling to the beginning of a channel should tell you where you are.
 */
function ChannelIntro({
  channel,
  empty,
  loading,
  onStartBrief,
  onAddPeople,
}: {
  channel: Channel;
  empty: boolean;
  loading: boolean;
  onStartBrief?: () => void;
  onAddPeople?: () => void;
}) {
  return (
    // `pt-8` on an empty channel, not `pt-16`. The taller lead-in was there to
    // push the intro down into a pane with nothing under it — but the
    // transcript grows from the bottom, so the moment a channel has one message
    // the intro is pushed up by the message anyway, and on a brand new one 64px
    // of nothing above the title read as the pane failing to load rather than
    // as breathing room. Still more than the `pt-6` a channel with history
    // gets, because on an empty channel the intro IS the content.
    <div className={cn("px-4 pb-3", empty ? "pt-8" : "pt-6")}>
      <IntroMark channel={channel} />
      <h2 className="text-xl font-semibold tracking-tight">{channelTitle(channel)}</h2>
      {/* Both of these sentences are positive claims that the channel has no
          history — "the start of", "the very beginning of". Neither may render
          until the host has answered, or a reload of a busy DM reads as lost
          conversation (issue #934). The identity block above is not a claim
          and stays either way, so the pane still says where you are. */}
      <p className="mt-1 max-w-prose text-sm text-muted-foreground">
        {channelIntroSentence(channel, loading)}
      </p>
      {/* The two openings a new channel actually has. Held back until the
          history has answered, for the same reason the sentence above is:
          offering "add an agent here" over a channel that turns out to be full
          of conversation reads as data loss.

          Not on a read-only channel (`channel.system`, the same
          predicate `RoomView` derives `readOnly` from). Neither opening exists
          there: "Give the team a brief" prefills a composer that channel does
          not render, and "Add people" opens a members pane `RoomView` gates
          off on the same flag — so both were controls offering an action that
          could not happen, under a notice saying nothing can be posted
          here. */}
      {empty && !loading && channel.kind === "channel" && !channel.system && (
        <ActionCards onStartBrief={onStartBrief} onAddPeople={onAddPeople} />
      )}
    </div>
  );
}

/**
 * What the intro draws above the channel's name (issue #1327).
 *
 * The same rule the header settled in #1170, at the intro's larger size: a DM
 * has exactly one person on the other end and wears their face; a channel has
 * nobody behind it and wears its kind.
 *
 * Before this, every channel but `main` fell through to `TeammateAvatar` seeded
 * on the channel *name*, so `#engineering` grew an arbitrary mascot — a face
 * belonging to no one, at the largest avatar size on the surface, as the first
 * thing in the pane — while the header eighteen pixels above drew `#` for the
 * same channel. Two marks for one thing, disagreeing on screen.
 *
 * `dmFace` is the shared seed, so the mark here and the rail row and the header
 * cannot drift about who a DM is with.
 */
function IntroMark({ channel }: { channel: Channel }) {
  // The geometry is fixed across all three branches so the copy beneath never
  // shifts with the kind of channel being opened.
  const box = "mb-3 size-12 rounded-lg";

  if (channel.kind === "dm") {
    const face = dmFace(channel);
    // A DM with no roster entry has nobody to draw. The header falls back to a
    // glyph rather than inventing a mascot for a stranger; so does this.
    return face ? (
      <TeammateAvatar {...face} className={cn(box, "text-base")} />
    ) : (
      <MarkTile icon={CircleDot} className={box} />
    );
  }

  // The company's own line keeps the brand mark it has always had.
  if (channel.id === "main") {
    return (
      <TeammateAvatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
        company
        className={cn(box, "text-base")}
      />
    );
  }

  return <MarkTile icon={channel.private ? Lock : Hash} className={box} />;
}

/**
 * A channel's kind on a tile, matching the treatment `ActionCard` gives its own
 * icon — `--surface-icon`, the rung the brand guide names for an icon ground —
 * so the two blocks on an empty channel read as one system rather than two.
 */
function MarkTile({ icon: Icon, className }: { icon: typeof Hash; className?: string }) {
  return (
    <span
      className={cn(
        "flex items-center justify-center bg-surface-icon text-muted-foreground",
        className,
      )}
      aria-hidden
    >
      <Icon className="size-5" />
    </span>
  );
}

/**
 * The pair of starting moves on an empty channel.
 *
 * Cards rather than buttons in a row: an empty channel is mostly empty space,
 * and the two things worth doing there deserve to be the largest objects on
 * it. The icon sits on `--surface-icon` — the rung the brand guide names for
 * exactly this, an icon circle — rather than on `muted`, which is the ground
 * for recessed *fills*.
 */
function ActionCards({
  onStartBrief,
  onAddPeople,
}: {
  onStartBrief?: () => void;
  onAddPeople?: () => void;
}) {
  return (
    <div className="mt-5 flex flex-wrap gap-4">
      <ActionCard
        icon={Send}
        title="Give the team a brief"
        hint="Start with a first request."
        onClick={onStartBrief}
      />
      <ActionCard
        icon={UserPlus}
        title="Add people"
        hint="Invite members."
        onClick={onAddPeople}
      />
    </div>
  );
}

function ActionCard({
  icon: Icon,
  title,
  hint,
  href,
  onClick,
}: {
  icon: typeof Bot;
  title: string;
  hint: string;
  href?: string;
  onClick?: () => void;
}) {
  const body = (
    <>
      <span className="flex size-9 items-center justify-center rounded-lg bg-surface-icon text-muted-foreground">
        <Icon className="size-4.5" aria-hidden />
      </span>
      <span className="mt-4 block">
        <span className="block text-lg font-semibold tracking-tight">{title}</span>
        <span className="mt-0.5 block text-2xs text-muted-foreground">{hint}</span>
      </span>
    </>
  );
  // `bg-glow-brand-card` is a background *image* and `bg-card` a background
  // *colour*, so the two compose rather than collide: the glow sits over the
  // card's fill and under its content, and `hover:bg-accent` still swaps the
  // fill beneath it. See `--glow-brand-card` in `index.css` for why the tint is
  // a token.
  const cls =
    "flex h-33 w-60 flex-col items-start rounded-xl border bg-card bg-glow-brand-card p-4 text-left transition-colors hover:bg-accent focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none";

  // A navigation is an anchor and an in-page action is a button, so the card
  // keeps the affordance its behaviour actually has.
  if (href) {
    return (
      <a href={href} className={cls}>
        {body}
      </a>
    );
  }
  return (
    <button type="button" onClick={onClick} className={cls} disabled={!onClick}>
      {body}
    </button>
  );
}

/**
 * Placeholder rows while a channel's history is on the wire.
 *
 * Shaped like the message rows it stands in for — avatar gutter, a name line,
 * two lines of body — so the pane does not jump when the real transcript
 * replaces it. `role="status"` is what makes the wait legible to a screen
 * reader, which cannot see that anything is pulsing.
 */
function HistorySkeleton() {
  return (
    <div role="status" aria-busy="true" className="space-y-1">
      <span className="sr-only">Loading messages…</span>
      {[0, 1, 2].map((row) => (
        <div key={row} className="flex items-start gap-2.5 px-4 py-1">
          <Skeleton className="size-9 shrink-0 rounded-full" />
          <div className="min-w-0 flex-1 space-y-2 py-1">
            <Skeleton className="h-3 w-28" />
            <Skeleton className="h-3 w-full max-w-prose" />
            <Skeleton className={cn("h-3", row === 1 ? "w-1/2" : "w-3/4")} />
          </div>
        </div>
      ))}
    </div>
  );
}

/**
 * What the company is doing right now, in place of the typing dots.
 *
 * Same avatar gutter as a message row so the work reads as coming from the
 * voice that will answer, and the same {@link StepTimeline} the finished reply
 * renders — so the rows do not re-draw differently the instant the turn ends.
 */
function LiveTurnRow({
  channel,
  steps,
  name,
  label,
}: {
  channel: Channel;
  steps: TurnStep[];
  /**
   * The answering teammate's display name, when the host recorded one.
   *
   * Ranks below a running step, which {@link WorkingIndicator} enforces: the
   * step is both more specific and more current. This is what the line says in
   * the gaps — before the first step, and between a settled step and the next.
   */
  name?: string;
  /** A complete line, when a name cannot describe the work — see the prop. */
  label?: string;
}) {
  return (
    <div className="flex items-start gap-2.5 px-4 py-1">
      <TeammateAvatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
        avatar={channel.member?.avatar}
        company={channel.kind === "channel" && channel.id === "main"}
        className="size-9 shrink-0"
      />
      <div className="min-w-0 flex-1 space-y-1.5">
        {/* The line says who and what; the timeline below says how far. */}
        <WorkingIndicator srLabel="Working…" steps={steps} name={name} label={label} />
        {/* The steps of a turn **still running**, collapsed to "N steps" the
            way a finished reply's are, and auto-opening on a failed or parked
            one so a silent MCP failure is visible rather than buried (#411).

            Raw turns still owns "what the agent saw" — the stored rows of a
            settled turn, in one renderer, because a claim that reads
            differently per screen is two claims. These are not that claim:
            they exist only while the turn is open and they are gone the moment
            it settles, replaced by the reply's own durable steps. Without them
            chat could say a turn was running and never what it had done. */}
        {!!steps.length && <StepTimeline steps={steps} />}
      </div>
    </div>
  );
}

function TypingRow({
  channel,
  queued,
  name,
  label,
}: {
  channel: Channel;
  queued?: boolean;
  /** The answering teammate's display name, when the host recorded one. */
  name?: string;
  /** A complete line, when a name cannot describe the work — see the prop. */
  label?: string;
}) {
  return (
    <div className="flex items-center gap-2.5 px-4 py-1">
      <TeammateAvatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
        avatar={channel.member?.avatar}
        company={channel.kind === "channel" && channel.id === "main"}
        className="size-9"
      />
      <WorkingIndicator srLabel="Replying…" queued={queued} name={name} label={label} />
    </div>
  );
}
