import { useCallback, useEffect, useLayoutEffect, useRef } from "react";
import { Bot, UserPlus } from "lucide-react";

import type { ApprovalSummary, GrantScope, TurnStep, Verdict } from "@/api/types";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import { ApprovalRow } from "./ApprovalRow";
import { Avatar } from "./Avatar";
import { MessageRow } from "./MessageRow";
import { StepTimeline } from "./StepTimeline";
import { WorkingIndicator } from "./WorkingIndicator";
import { channelTitle, type Channel, type TimelineItem } from "./model";

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
   * The tool rows of a turn running *right now* on this channel's thread, off
   * the transient `tool_call` / `tool_result` / `thinking` frames. Present
   * whether or not this console started the turn, which is the point — a turn
   * an inbound message kicked off shows its work here too (issue #367).
   */
  liveSteps?: TurnStep[];
  onOpenThread: (messageId: string) => void;
  onReact: (messageId: string, emoji: string) => void;
  /**
   * Opens the members pane, for the "Add people" card on an empty channel.
   * Optional so the thread panel — which renders no intro — need not pass it.
   */
  onAddPeople?: () => void;
  /** Now, for the cards' "waiting N minutes" line. Owned by the shell's feed. */
  now?: number;
  /** Agent id → display name, for a card's "Asked by" line. */
  askerNames?: Map<string, string>;
  /** The verdict each inline card is currently waiting on. */
  decidingApprovals?: ReadonlyMap<string, Verdict>;
  /** Decisions that did not land, per approval id (#842) — see `ApprovalRow`. */
  failedApprovals?: Record<string, string>;
  onDecideApproval?: (approval: ApprovalSummary, verdict: Verdict, scope: GrantScope) => void;
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
  liveSteps,
  onOpenThread,
  onReact,
  onAddPeople,
  now,
  askerNames,
  decidingApprovals,
  failedApprovals,
  onDecideApproval,
}: Props) {
  const scroller = useRef<HTMLDivElement>(null);
  const liveStepCount = liveSteps?.length ?? 0;
  // Rows that arrived locally — a message sent before hydration landed — are
  // still worth showing while the rest of the history is in flight. It is only
  // the *claim of emptiness* that has to wait.
  const loading = historyPending && items.length === 0;
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
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
    following.current = true;
  }, [channel.id]);

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
    if (!following.current) return;
    el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
  }, [channel.id, items.length, typing, liveStepCount]);

  return (
    <div ref={scroller} onScroll={trackFollowing} className="flex-1 overflow-y-auto">
      <div className="flex min-h-full flex-col justify-end pb-4">
        {/* `empty` only drives the top padding, and the skeleton fills the
            same space real rows will — so a loading channel is spaced like a
            full one and the intro does not jump down and back up. */}
        <ChannelIntro
          channel={channel}
          empty={items.length === 0 && !loading}
          loading={loading}
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
              />
            </div>
          ) : (
            <ApprovalRow
              key={item.key}
              approvals={item.approvals}
              now={now ?? Date.now()}
              askerNames={askerNames ?? EMPTY_NAMES}
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
        {liveStepCount > 0 ? (
          <LiveTurnRow channel={channel} steps={liveSteps ?? []} />
        ) : (
          typing && <TypingRow channel={channel} />
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
  onAddPeople,
}: {
  channel: Channel;
  empty: boolean;
  loading: boolean;
  onAddPeople?: () => void;
}) {
  return (
    <div className={cn("px-4 pb-3", empty ? "pt-16" : "pt-6")}>
      <Avatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
        company={channel.kind === "channel" && channel.id === "main"}
        className="mb-3 size-12 rounded-lg text-base"
      />
      <h2 className="text-xl font-semibold tracking-tight">{channelTitle(channel)}</h2>
      {/* Both of these sentences are positive claims that the channel has no
          history — "the start of", "the very beginning of". Neither may render
          until the host has answered, or a reload of a busy DM reads as lost
          conversation (issue #934). The identity block above is not a claim
          and stays either way, so the pane still says where you are. */}
      <p className="mt-1 max-w-prose text-sm text-muted-foreground">
        {loading
          ? sentence(channel.purpose)
          : channel.kind === "dm"
            ? `This is the start of your direct message with ${channel.name} — ${lower(channel.purpose)}.`
            : `This is the very beginning of ${channelTitle(channel)}. ${sentence(channel.purpose)}`}
      </p>
      {/* The two openings a new channel actually has. Held back until the
          history has answered, for the same reason the sentence above is:
          offering "add an agent here" over a channel that turns out to be full
          of conversation reads as data loss. */}
      {empty && !loading && channel.kind === "channel" && (
        <ActionCards onAddPeople={onAddPeople} />
      )}
    </div>
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
function ActionCards({ onAddPeople }: { onAddPeople?: () => void }) {
  return (
    <div className="mt-5 flex flex-wrap gap-4">
      <ActionCard
        icon={Bot}
        title="Create agent"
        hint="Add an agent here."
        href="#/company"
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
  const cls =
    "flex h-33 w-60 flex-col items-start rounded-xl border bg-card p-4 text-left transition-colors hover:bg-accent focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none";

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
      <Avatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
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

function TypingRow({ channel }: { channel: Channel }) {
  return (
    <div className="flex items-center gap-2.5 px-4 py-1">
      <Avatar
        name={channel.voice ?? channel.name}
        tone={channel.tone}
        company={channel.kind === "channel" && channel.id === "main"}
        className="size-9"
      />
      <WorkingIndicator srLabel="Replying…" />
    </div>
  );
}

function lower(s: string): string {
  return s.charAt(0).toLowerCase() + s.slice(1);
}

function sentence(s: string): string {
  const t = s.trim();
  return /[.!?]$/.test(t) ? t : `${t}.`;
}
