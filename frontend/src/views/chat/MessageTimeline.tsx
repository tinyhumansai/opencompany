import { useCallback, useEffect, useLayoutEffect, useRef } from "react";
import { Bot, CircleDot, Hash, Lock, Send, UserPlus } from "lucide-react";

import type { ApprovalSummary, CognitionState, GrantScope, TurnStep, Verdict } from "@/api/types";
import type { TaskStatus } from "@/api/tasks";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import { ApprovalRow } from "./ApprovalRow";
import { ChatLiveReceipt, type ChatReceipt } from "./ChatLiveReceipt";
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
   * The live receipt for a synchronous chat turn this console just sent (issue
   * #1934). When present it supersedes {@link TypingRow} — it says "Sent →
   * Picked up → on step" with a ticking clock instead of bare typing dots — and
   * folds the same {@link StepTimeline} below the line when steps exist. Absent
   * for an inbound turn this console never started, which still falls to the
   * `liveSteps`/`typing` rows below.
   */
  receipt?: ChatReceipt;
  /** Roster agent id → display name, so the receipt never shows a raw id. */
  agentNames?: Record<string, string>;
  onOpenThread: (messageId: string) => void;
  onReact: (messageId: string, emoji: string) => void;
  /** Deletes the board card a line opened, and drops its chip (issue #984). */
  onDismissCard: (taskId: string) => void;
  /** The card whose delete is in flight, if any. */
  dismissingCardId: string | null;
  /**
   * Resolves a stored attachment's bytes to an object URL for the transcript
   * (issue #1682). Threaded from the shell, which holds the authenticated
   * client the blob route needs. Absent where nothing renders attachments.
   */
  resolveAttachmentUrl?: (nodeId: string) => Promise<string>;
  /** Board task id -> live state for card-linked background turns (#1758). */
  taskStatusByTaskId?: Readonly<Record<string, TaskStatus>>;
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
  onDecideApproval?: (approval: ApprovalSummary, verdict: Verdict, scope: GrantScope) => void;
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
 * How close to the bottom still counts as "parked at the bottom", in CSS
 * pixels. Sub-pixel layout and a fractional `clientHeight` mean the arithmetic
 * rarely lands on exactly zero, so a strict test would read a view that is
 * visibly at the bottom as scrolled away and stop following.
 */
const BOTTOM_SLACK_PX = 32;

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
  receipt,
  agentNames,
  onOpenThread,
  onReact,
  onDismissCard,
  dismissingCardId,
  resolveAttachmentUrl,
  taskStatusByTaskId,
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
  const scroller = useRef<HTMLDivElement>(null);
  /** The inner column whose own height rule 2b's `ResizeObserver` watches. */
  const content = useRef<HTMLDivElement>(null);
  const liveStepCount = liveSteps?.length ?? 0;
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
  /**
   * Is the view parked at the bottom, and therefore still following?
   *
   * A ref rather than state on purpose: it is read inside effects and written
   * from a scroll handler that fires at frame rate. Making it state would
   * re-render the whole transcript on every wheel tick to compute a value no
   * rendered output depends on.
   */
  const following = useRef(true);
  /** The channel the growth effect has already settled on. See rule 2. */
  const settledOn = useRef<string | null>(null);

  const trackFollowing = useCallback(() => {
    const el = scroller.current;
    if (!el) return;
    const fromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    following.current = fromBottom <= BOTTOM_SLACK_PX;
  }, []);

  // Rule 1 — arriving at a channel. `useLayoutEffect` so the jump happens
  // before paint: with `useEffect` the browser paints the un-anchored position
  // first, which is the flash this issue is about. `channel.id` is the
  // dependency, not `items.length` — two channels can hold the same number of
  // rows, and an effect keyed on the count would not fire for that switch at
  // all, leaving the new channel wearing the old one's scroll offset.
  //
  // `historyPending` is the second dependency, and it is what makes the rule
  // true rather than merely well-intentioned (issue #1224). A cold load mounts
  // this component *before* the transcript exists: history is still on the wire
  // (`historyPending`), the box is one screen tall, and "scroll to the bottom"
  // is a no-op against content that has not arrived. Keyed on the channel
  // alone, this effect then never ran again, and the operator was left at the
  // top of a transcript that appeared under them a hundred milliseconds later.
  // Re-anchoring as the history lands is the same jump, against the real
  // transcript this time.
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
    following.current = true;
  }, [channel.id, historyPending]);

  // Rule 2 — growth while the channel is open. Each new tool row grows the
  // block at the bottom, so the scroll has to follow it as the turn works, not
  // only when the reply lands. A card arriving counts too — it is the thing the
  // operator has to act on. Skipped entirely when they have scrolled away.
  //
  // `channel.id` is a dependency so the first pass after a switch can *defer*:
  // the layout effect above has already anchored this channel, and animating on
  // top of that is the very travel rule 1 removes.
  useEffect(() => {
    const el = scroller.current;
    if (!el) return;
    if (settledOn.current !== channel.id) {
      settledOn.current = channel.id;
      return;
    }
    // Nothing to follow while the transcript is still on the wire (#1224).
    // `scrollTo` captures a **pixel offset**, not the idea of "the bottom", so
    // an animation started against a one-screen box eases to a number the
    // arriving history makes meaningless — and the scroll events it emits on
    // the way there are indistinguishable from a person scrolling, so
    // `trackFollowing` reads the grown transcript as "they scrolled away" and
    // the channel stops following for the rest of the session. Rule 1 above
    // owns the anchor until the history has landed.
    if (historyPending) return;
    if (!following.current) return;
    el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
  }, [channel.id, historyPending, items.length, typing, liveStepCount]);

  // Rule 3 — the *viewport* shrinking underneath (issue #1325).
  //
  // Rules 1 and 2 both watch the content. Neither watches the box, and the box
  // moves: the composer below this pane grows with the draft (`field-sizing-
  // content`, up to `max-h-48`), which takes its height out of this scroller's
  // `clientHeight`. `scrollTop` is untouched by that, so the transcript slides
  // up behind the composer — measured at 96px on a two-line draft and up to
  // ~150px at the cap, which is often the very message being replied to,
  // hidden for exactly as long as the draft is long.
  //
  // It could not be fixed by adding a dependency to rule 2: the composer is a
  // sibling component and its height is not a value this one is given. The
  // element's own size is, through `ResizeObserver` — and observing the box
  // covers the window resizing and the thread panel opening as well, which want
  // the same answer.
  //
  // `following.current` is the same gate rule 2 uses, so a reader who has
  // deliberately scrolled up is left alone. Instant rather than smooth,
  // unlike rule 2: this fires as the composer grows a line at a time, and an
  // animation per keystroke would be a permanent wobble rather than a glide.
  // Setting `scrollTop` does not resize anything, so there is no feedback loop.
  useEffect(() => {
    const el = scroller.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      if (!following.current) return;
      el.scrollTop = el.scrollHeight;
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  // Rule 2b — content that grows without moving any of rule 2's dependencies
  // (issue #1935 review, coderabbit 3892517543). `ChatLiveReceipt`'s 30s
  // "still waiting" note is timed by a clock entirely internal to that
  // component: nothing here re-renders when it appears, so rule 2 never fires
  // and the note can land under the fold with no follow-scroll to reveal it.
  // A live receipt is the concrete case, but the same gap exists for any
  // in-place child growth this component was not told about.
  //
  // Rule 3's `ResizeObserver` cannot double as this one — it watches the
  // *scroller's own border box*, which content overflowing inside an
  // `overflow-y-auto` container never changes; that is the whole reason the
  // container scrolls instead of growing. This one watches the *content*
  // column instead — the inner wrapper whose height the rows and receipt
  // actually determine — so it fires on exactly the growth rule 3 cannot see,
  // and stays silent on the box-only resizes (composer growing, window
  // resizing) rule 3 exists for, which do not move this column's own height.
  useEffect(() => {
    const contentEl = content.current;
    const scrollerEl = scroller.current;
    if (!contentEl || !scrollerEl || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      // Nothing to follow while the transcript is still on the wire, same as
      // rule 2 — a cold load's content grows repeatedly as history lands, and
      // rule 1 owns the anchor until it has.
      if (historyPending || !following.current) return;
      scrollerEl.scrollTo({ top: scrollerEl.scrollHeight, behavior: "smooth" });
    });
    observer.observe(contentEl);
    return () => observer.disconnect();
  }, [historyPending]);

  return (
    <div ref={scroller} onScroll={trackFollowing} className="flex-1 overflow-y-auto">
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
        {items.map((item) =>
          item.kind === "message" ? (
            <div key={item.key}>
              {item.entry.dayLabel && <DayDivider label={item.entry.dayLabel} />}
              <MessageRow
                entry={item.entry}
                threadOpen={item.entry.message.id === openThreadId}
                onOpenThread={onOpenThread}
                onReact={onReact}
                onDismissCard={onDismissCard}
                dismissingCardId={dismissingCardId}
                resolveAttachmentUrl={resolveAttachmentUrl}
                taskStatusByTaskId={taskStatusByTaskId}
                now={now ?? Date.now()}
                cognition={cognition}
                onRedeemBudgetPause={onRedeemBudgetPause}
                redeemingBudgetPauseAgent={redeemingBudgetPauseAgent}
                latestBudgetPauseMessageIdByAgent={latestBudgetPauseMessageIdByAgent}
              />
            </div>
          ) : (
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
              /* Narrowed to this card's own items (#842): a decision in flight
                 on another turn's batch is not this card's business, which is
                 the same rule #373 established one level down. */
              deciding={decidingIn(item.approvals, decidingApprovals)}
              decided={item.decided}
              failed={failedApprovals ?? EMPTY_FAILURES}
              onDecide={(approval, verdict, scope) =>
                onDecideApproval?.(approval, verdict, scope)
              }
            />
          ),
        )}
        {receipt && !queued ? (
          // The receipt for our own in-flight send (issue #1934) supersedes the
          // typing dots and carries the live steps itself. A queued turn keeps
          // its honest "Queued…" row instead — the receipt is a `!queued` state.
          <ChatLiveReceipt
            channel={channel}
            receipt={receipt}
            agentNames={agentNames}
            steps={liveSteps ?? []}
          />
        ) : liveStepCount > 0 && !queued ? (
          <LiveTurnRow channel={channel} steps={liveSteps ?? []} />
        ) : (
          typing && <TypingRow channel={channel} queued={queued} />
        )}
      </div>
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
    <div className={cn("px-4 pb-3", empty ? "pt-16" : "pt-6")}>
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
          offering "add a teammate here" over a channel that turns out to be full
          of conversation reads as data loss. */}
      {empty && !loading && channel.kind === "channel" && (
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
function LiveTurnRow({ channel, steps }: { channel: Channel; steps: TurnStep[] }) {
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
        {/* The line names the step actually in flight (#787), above the
            timeline that details every step. Same source, one phrasing. */}
        <WorkingIndicator srLabel="Working…" steps={steps} />
        <StepTimeline steps={steps} defaultOpen />
      </div>
    </div>
  );
}

function TypingRow({ channel, queued }: { channel: Channel; queued?: boolean }) {
  return (
    <div className="flex items-center gap-2.5 px-4 py-1">
      <TeammateAvatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
        avatar={channel.member?.avatar}
        company={channel.kind === "channel" && channel.id === "main"}
        className="size-9"
      />
      <WorkingIndicator srLabel="Replying…" queued={queued} />
    </div>
  );
}
