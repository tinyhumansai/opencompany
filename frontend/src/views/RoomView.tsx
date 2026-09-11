import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type Dispatch,
  type ReactNode,
  type RefObject,
  type SetStateAction,
} from "react";
import { createPortal } from "react-dom";
import { Loader2, MessageSquare, TriangleAlert } from "lucide-react";
import { toast } from "sonner";

import { me as fetchMe } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { deleteTask, type InflightRun, type MessageIntent, type TaskStatus } from "@/api/tasks";
import { turnStateKey } from "@/lib/live-reply";
import { uploadChatAttachment } from "@/api/chat";
import { deleteNode, fetchBlobUrl } from "@/api/workspace";
import { fetchWithOneRetry } from "@/lib/fetch-with-retry";
import {
  ApiError,
  type ApprovalSummary,
  type AttachmentDto,
  type CognitionState,
  type DecideApproval,
  type AgentSessionMessageDto,
  type OperatorChannelDto,
  type TeamMemberDto,
  type Verdict,
  isDetachedChat,
} from "@/api/types";
import { useHashFlag } from "@/hooks/use-hash-flag";
import { Button } from "@/components/ui/button";
import { PageHeader } from "@/components/page-header";
import { Skeleton } from "@/components/ui/skeleton";
import {
  fromHistory,
  isGeneralChannel,
  makeMessage,
  markSendFailed,
  reconcileIds,
  replyVoice,
  toHostMessageId,
  type ChatMessage,
} from "@/lib/chat";
import { defaultDesks, type Desk } from "@/lib/desks";
import { readLastChannel } from "@/lib/last-channel";
import { connectionsHref } from "@/views/connection-pages";
import {
  addMemberFailure,
  reportAddMember,
  type AddMemberOutcome,
} from "@/lib/member-feedback";
import { fromDto, newMember, type TeamMember } from "@/lib/team";
import { personAvatar } from "@/lib/person";
import { useAskerNames } from "@/components/approval-card";
import { useRoomRailSlot } from "@/components/room-rail";
import { AddMemberDialog, type NewMemberFields } from "./room/AddMemberDialog";
import { ChannelCreateDialog } from "./room/ChannelCreateDialog";
import { ChannelRail } from "./room/ChannelRail";
import { ChatHeader } from "./room/ChatHeader";
import { MembersPane } from "./room/MembersPane";
import { TypingLine } from "./room/TypingLine";
import { InflightRunBar } from "./room/InflightRunBar";
import { MessageComposer } from "./room/MessageComposer";
import {
  mentionablesFor,
  sameTarget,
  mentionsOutsideChannel,
  utf8ByteLength,
  type Mention,
  type Mentionable,
} from "./room/mentions";
import { echoCause } from "./room/EchoPlaceholder";
import { MessageTimeline } from "./room/MessageTimeline";
import { RawTurns } from "./room/RawTurns";
import { ThreadPanel } from "./room/ThreadPanel";
import { useLocalScope } from "@/connections/ConnectionContext";
import * as room from "@/room/store";
import { foldEpisodes, type EpisodeTurn } from "@/lib/hive/episode";
import {
  buildChannels,
  buildTimeline,
  buildTimelineItems,
  budgetPauseRedeemId,
  canSubmitReview,
  channelIdFromSegment,
  channelMembers,
  channelTitle,
  deskFromDto,
  dmChannelId,
  dmThreadId,
  findChannel,
  firstChannel,
  generalChannelId,
  historyReady,
  HISTORY_UNTRACKED,
  clearTaskCardEverywhere,
  directMessageChannels,
  directMessageForId,
  inlineReplyIds,
  isOperatorChannelDto,
  latestBudgetPauseMessageIdByAgent,
  mergeBudgetPauseMarkerRead,
  offersDeliverableChoice,
  operatorSection,
  repliesInThread,
  resolveDmChannelId,
  reviewAnchorsForThread,
  toggleReaction,
  type DecidedApproval,
  type HistoryHydration,
  type Transcripts,
} from "./room/model";

/**
 * The stable empty transcript fallback.
 *
 * `transcripts[channel.id]` can be absent for a channel with no history yet —
 * a newly opened DM, or a desk whose history came back empty. Falling back to a
 * fresh `[]` would give `messages` a new identity on every render, which
 * recomputes the `replyParents`/`loadedMessageIds` memos and re-runs the
 * channel-view effect on every render — and that effect's state write
 * re-renders the shell, closing a render loop. One shared empty array keeps the
 * identity stable until a transcript entry actually lands.
 */
const EMPTY_MESSAGES: ChatMessage[] = [];

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** The hash's second segment — the channel id, e.g. `main` in `#/chat/main`. */
  sub: string | null;
  /**
   * Whether `#/chat` is the address on screen.
   *
   * The shell keeps this view mounted on **every** route since issue #2130,
   * because the sidebar's channel rail is portalled out of here and is pinned
   * there on every section — see `components/room-rail.tsx` for the two ways
   * that could have been done and why this is the one taken.
   *
   * So the rail renders unconditionally and everything that belongs to the Room
   * *route* — the transcript, the header, the members pane — renders only when
   * this is true. It also gates `chatPaneVisible`: a transcript that is mounted
   * but not routed must not mark a mention read, for the same reason the phone's
   * covering sheet must not.
   *
   * Optional, defaulting to **true**: a caller that renders this view as a page
   * — every unit test, a future embed — means the transcript, and should not
   * have to say so. It is the shell keeping it mounted off-route that is the
   * unusual case, and the shell is the one that says it. Same rule
   * `useRoomRailSlot` follows when there is no provider: the standalone shape
   * is the one that works with nothing configured.
   */
  routeOpen?: boolean;
  onNavigate: (channelId: string) => void;
  /**
   * Leave chat for a teammate's detail page, with `edit` opening its edit form
   * too (issue #1989).
   *
   * The one navigation out of this view, and it exists for one reason: the
   * reduced Add-teammate dialog collects a name and a sentence, and the copilot
   * that drafts the description and the persona lives in that form. Creating a
   * teammate here and staying in chat would leave them half-written with
   * nothing pointing at where to finish them.
   *
   * Optional, so `RoomView` still mounts standalone in tests — but a mount
   * without it turns the reduced dialog's create into a dead end, so the shell
   * always passes it.
   */
  onOpenAgent?: (agentId: string, options?: { edit?: boolean }) => void;
  /** Called after a reply lands, so the shell can refresh approvals/status. */
  onReply?: () => void;
  /**
   * Every channel's transcript, keyed by channel id, and its setter — owned by
   * `AppShell` rather than here so a transcript survives this component
   * unmounting when the operator navigates to another view and back (the shell
   * mounts and unmounts `RoomView` per route; component-local state would be
   * discarded on every trip away from Chat).
   */
  transcripts: Transcripts;
  setTranscripts: Dispatch<SetStateAction<Transcripts>>;
  /**
   * How far the shell's rehydration of each channel's history has got, so the
   * timeline can hold a loading state instead of claiming a channel is empty
   * while its history is still on the wire (issue #934). Optional, and absent
   * means "nothing is pending" — a mount with no shell behind it renders as it
   * always did rather than spinning forever.
   */
  hydration?: HistoryHydration;
  /**
   * Called around the awaited chat POST with the **host thread id** it was sent
   * on, so the shell can suppress the SSE echo of our own turn while it is in
   * flight. Without this bracket the shell's live injection and the awaited
   * reply below both render and the bubble doubles — the exact duplicate-bubble
   * race the Conversation surface already brackets against.
   *
   * Returns the generation the shell stamped this send's receipt with (issue
   * #1935 review). `send` threads it back through whichever terminal callback
   * this POST reaches, so the shell can tell "my own armed receipt settling"
   * apart from "a newer send already re-armed this reused thread id" — see
   * `shouldClearReceipt`. `undefined` when the shell has nothing to say (no
   * handler wired), which callers must treat the same as "clear unconditionally".
   */
  onSendStart?: (threadId: string) => number | undefined;
  /**
   * Who is present right now, keyed by user id. Empty when the host has no
   * presence route, or when nobody else is connected to this replica.
   */
  presence?: ReadonlyMap<string, { status: "online" | "away" | "offline" }>;
  /**
   * The autonomy control, rendered on the composer's toolbar row.
   *
   * A node, not the policy: `AppShell` owns the tier and the admin check, and
   * handing the rendered pill down keeps every fact about policy in one place.
   */
  autonomy?: ReactNode;
  /**
   * The company's people, for the members pane's People section.
   *
   * Separate from `members` (teammates) on purpose: desk membership is a
   * teammate concept, and every signed-in person can already see every desk,
   * so people are never "in" or "outside" a channel.
   */
  companyPeople?: Array<{ id: string; label: string }>;
  /**
   * Display names for the typing line, in a stable order — resolved on
   * demand rather than a single precomputed array, because this view needs
   * two independent lines: the main composer's (no `parentId`) and, when a
   * thread is open, that thread's own (`parentId` set). A single `string[]`
   * could only ever answer one of them.
   */
  resolveTypingNames?: (chatId: string, parentId?: string) => string[];
  /** Called as a composer is typed in; the caller throttles. */
  onTyping?: (chatId: string, parentId?: string) => void;
  /**
   * `responseTexts` is every reply line this settled POST's own body carried
   * (issue #101 review, PR #2052) — what `PendingSyncPosts.ended` needs to
   * tell a held **system**-attributed live frame (never the operator's own
   * reply, which is always in the response) apart from one the response never
   * carried: a `system_notice` fallback (approval overflow, "Acknowledged.")
   * is folded into this same response body, but B-101's mention-ambiguity
   * note deliberately never is. Without it, either every held system frame
   * had to be discarded (silently losing the ambiguity note) or none did
   * (double-rendering the ones the response does carry).
   */
  onSendEnd?: (threadId: string, gen?: number, responseTexts?: readonly string[]) => void;
  /**
   * The host accepted the turn and answered `202` instead of the reply
   * (issue #983). Distinct from `onSendEnd`, which says the turn is *over*:
   * this one says the POST is over and the turn is not, so the shell keeps the
   * working row up and stops suppressing the live reply frame.
   */
  onSendDetached?: (threadId: string, turnId?: string, gen?: number, chatId?: string) => void;
  /**
   * The chat POST **threw** rather than answering (issue #1000).
   *
   * The third outcome, and distinct from `onSendEnd` in the way that matters:
   * `onSendEnd` promises the shell that the reply is already on screen, so the
   * shell may drop the live frame it was holding. A throw promises the
   * opposite — nothing was rendered, and since #983 the turn usually outlives
   * the request that started it, so that held frame is the only copy of the
   * answer anyone is going to get.
   */
  onSendFailed?: (threadId: string, gen?: number) => void;
  /** Called when a delayed response belongs to a previous company scope. */
  onSendStale?: (threadId: string, gen?: number) => void;
  /**
   * The shell's live company ref, so the stale-response check keeps observing
   * company switches after this view unmounts.
   *
   * A component-local ref would freeze at the last render's company once this
   * subtree disappears — exactly when the operator can walk to another view
   * and switch companies mid-POST. The shell owns `companyRef` and updates it
   * on every company change whether or not Chat is mounted, so a send that
   * resolves or rejects after the switch still sees the new scope and is
   * declared stale rather than writing the old company's reply into the new
   * company's transcript.
   */
  /**
   * The latest connection and company scope, updated by the shell while mounted.
   *
   * `client` is part of the scope for a reason codex flagged (P1): the registry's
   * `reseat` path edits a host address by replacing the `OpenCompanyClient` while
   * deliberately preserving the connection id, so `connection` + `company` alone
   * do not move when the host underneath a send changes. Comparing the client
   * instance catches the old host's late completion after reseat.
   */
  scopeRef: RefObject<{ connection: string; company: string | null; client: OpenCompanyClient }>;
  /**
   * Roster agent id → display name, captured by the shell's desks/roster read
   * (issue #1934). Lets the receipt name whoever picked the turn up rather than
   * rendering a raw id; a miss falls back to the channel voice.
   */
  agentNames?: Record<string, string>;
  /** Channel id → unread count, for the rail's badges. Owned by the shell. */
  unread?: Record<string, number>;
  /**
   * Channel id → unread mentions of this person there.
   *
   * A separate badge from `unread`, not a subset of it: unread is derived in
   * this browser, a mention is a durable host-side fact about *you*. See
   * `ChannelRail`'s prop docs for why merging them would be a loss.
   */
  mentions?: Record<string, number>;
  mentionFeedRevision?: number;
  /**
   * Reports the channel actually on screen — which the hash need not name,
   * since it may have been resolved by the first-channel fallback. The shell
   * clears that channel's unread count and remembers it as where an
   * unaddressed line belongs after this view is gone (issue #368).
   *
   * The second argument is whether *this* channel's history is still on the
   * wire. A mention is durable and there is no older-history pagination to
   * recover one — so the shell must not clear a mention for a message it
   * cannot yet prove is on screen, which is exactly the case where this is
   * `true`.
   */
  onChannelViewed?: (
    channelId: string,
    historyPending: boolean,
    mentionFeedRevision?: number,
    /**
     * The loaded transcript's thread replies (`reply id → parent id`), so the
     * reader can defer a thread-reply mention until its thread is open.
     */
    replyParents?: ReadonlyMap<string, string>,
    /** The thread panel currently open, or `null`. */
    openThreadId?: string | null,
    /** All loaded message ids for this channel, so the reader can defer a
     * mention whose subject is outside the history window. */
    loadedMessageIds?: ReadonlySet<string>,
  ) => void;
  /**
   * Reports whether the transcript is actually on screen right now — on a
   * phone the sidebar holding the channel list is a sheet over the whole
   * screen, and it hides the transcript even though `onChannelViewed`'s last
   * report still names that channel.
   * Distinct from `onChannelViewed`'s own channel memory (which the shell
   * also uses to address an unaddressed system line after the operator walks
   * off to Approvals, and must keep doing even while the rail is showing):
   * this is only for "is a completion's inline marker visible right now",
   * so a stale-but-correct channel name does not suppress its toast for a
   * transcript the operator cannot see (#1768 codex review).
   */
  onChatPaneVisibilityChange?: (visible: boolean) => void;
  /**
   * Every approval currently awaiting the operator, straight off the shell's
   * feed, plus the host thread → channel map that places them (#379).
   *
   * Passed whole and filtered here rather than pre-filtered upstream, because
   * the filter needs the channel actually on screen — which this view resolves,
   * not the shell. An approval whose `thread` names no channel this company has
   * (a workflow delivery, a scheduler tick, anything parked before #379) simply
   * matches nothing and stays on the Approvals page, which is the whole
   * additive contract.
   */
  approvals?: ApprovalSummary[];
  /** Board task id -> live state for card-linked background turns (#1758). */
  taskStatusByTaskId?: Readonly<Record<string, TaskStatus>>;
  /**
   * The company's steerable runs, whole. Separate from `taskStatusByTaskId`
   * because that map is card-keyed and a delegation has no card, so the runs
   * that most need a control here are exactly the ones it cannot carry.
   */
  inflightRuns?: readonly InflightRun[];
  /** Re-read the in-flight list after a steer lands. */
  onInflightSteered?: () => void | Promise<void>;
  /** Now, for a card's "waiting N minutes" line. */
  now?: number;
  /**
   * Decide an approval from inside the conversation. Owned by the shell so the
   * witnessed verdict survives this view unmounting — the operator can walk to
   * Approvals and back mid-turn.
   */
  onDecideApproval?: DecideApproval;
  /** The verdict each card is waiting on, and the ones already witnessed. */
  decidingApprovals?: ReadonlyMap<string, Verdict>;
  decidedApprovals?: Record<string, DecidedApproval>;
  /**
   * Decisions that did not land, per approval id (#842) — the message to show
   * on that item. Owned by the shell, like the two maps above, because a
   * failure has to outlive this view unmounting: the operator's next move after
   * one is often to open the Approvals page and come back.
   */
  failedApprovals?: Record<string, string>;
  /**
   * The coarse "near your credit limit" warning (issue #1846), off the live
   * `budget_proximity` frame. Owned by the shell — it can fire mid-turn on any
   * channel, and the shell is what outlives a channel switch. `null`/absent
   * renders no banner.
   */
  budgetProximity?: { message: string; atMillis: number } | null;
  /** Clears the banner above — the shell's own state, this view only asks. */
  onDismissBudgetProximity?: () => void;
}

const FIRST_TEAM_BRIEF =
  "Help us get started: propose the first three priorities for our company and who should own each one.";

/**
 * The chat workspace.
 *
 * One screen replaces what used to be three: the Conversation page's thread
 * list, the Team page's roster, and the desks those two shared without ever
 * being connected. Here the desks are channels, every teammate has a DM, and
 * the roster sits in a pane you can open beside the transcript.
 *
 * Every channel posts to the same company chat endpoint — a channel scopes a
 * transcript and fixes the company side's identity, it is not a separate
 * backend. Threads and reactions are console-local for the same reason: the
 * host has no surface for either yet.
 */
/**
 * The host seq a console message id names, for keying live-turn state.
 *
 * `undefined` for an unthreaded send and for a local id the host has not
 * reconciled yet — both of which key at the channel, which is what they are.
 */
function threadRootOf(parentId: string | undefined): number | undefined {
  const seq = toHostMessageId(parentId);
  if (seq === null) return undefined;
  const n = Number(seq);
  return Number.isFinite(n) ? n : undefined;
}

export function RoomView({
  client,
  company,
  sub,
  routeOpen = true,
  onNavigate,
  onOpenAgent,
  autonomy,
  onReply,
  transcripts,
  setTranscripts,
  hydration = HISTORY_UNTRACKED,
  onSendStart,
  presence,
  companyPeople,
  resolveTypingNames,
  onTyping,
  onSendEnd,
  onSendDetached,
  onSendFailed,
  onSendStale,
  scopeRef,
  agentNames,
  unread,
  mentions,
  mentionFeedRevision,
  onChannelViewed,
  onChatPaneVisibilityChange,
  approvals,
  taskStatusByTaskId,
  inflightRuns,
  onInflightSteered,
  now,
  onDecideApproval,
  decidingApprovals,
  decidedApprovals,
  failedApprovals,
  budgetProximity,
  onDismissBudgetProximity,
}: Props) {
  /*
   * Read straight from the Room store rather than taken as props.
   *
   * All five used to be threaded down from `app-shell.tsx`, which held them in
   * `useState` only because this component unmounts on every trip away from the
   * Room. They live in `room/store.ts` now, so the shell has nothing to hand
   * over and this reads them where they are.
   *
   * `transcripts` and `hydration` deliberately stay props: `hydration` defaults
   * to `HISTORY_UNTRACKED` for a `RoomView` mounted with no shell behind it —
   * resolving every channel to "ready" so a standalone mount does not spin on a
   * pass that is never coming — and reading the store would replace that with
   * `HISTORY_UNSTARTED`, which spins forever.
   */
  const openTurns = room.useOpenTurns();
  const liveStepsByThread = room.useLiveStepsByThread();
  const liveStepsByMessage = room.useLiveStepsByMessage();
  const receiptByThread = room.useReceiptByThread();
  const chatChannelByThread = room.useChatChannelByThread();
  // Which (connection, company) this subtree's browser-local state belongs to.
  const scope = useLocalScope();
  /**
   * How many times Room has been entered — the mount this view no longer has
   * (Codex P2 review on #2130).
   *
   * Every read below was keyed on `[client, company]` alone, and that was
   * sufficient while navigating away unmounted the view: coming back was a
   * mount, and a mount re-read everything. It is not sufficient now. Add a
   * teammate on Company, delete a desk on the org chart, and the pinned rail
   * would go on showing the roster and channels it loaded once — until a reload
   * or a company switch, and in plain sight, because the rail is on screen the
   * whole time.
   *
   * A counter incremented on ENTRY, rather than `routeOpen` in each dependency
   * list, for two reasons. `routeOpen` moves in both directions, so leaving Room
   * would spend a second round of reads on a section the operator has just left.
   * And gating the reads on `routeOpen` instead would leave the rail empty on a
   * console loaded straight onto `#/company`, which is the one thing this whole
   * change exists to prevent — the reads still run on mount, wherever that is.
   *
   * The cognition read is in the set too, and it was not at first: it already
   * refreshes on `visibilitychange`, which sounded like enough and is not. That
   * event is about the *tab*, not the route — an admin who follows the Room
   * warning to Connections → Inference, configures a provider and comes back has
   * never hidden the tab, so the stale warning and its echo placeholders would
   * have stayed (Codex P2 review).
   */
  const [roomVisits, setRoomVisits] = useState(0);
  const wasRouteOpen = useRef(routeOpen);
  useEffect(() => {
    // A `false → true` transition, and only after the mount (Codex P2 review).
    // Bumping on `routeOpen` being true at all counted the ordinary startup —
    // the console opens on `#/chat` — so every read below ran twice in a row,
    // and `loadDesks` clears the desk list on its way, dropping the pane back to
    // its loading state a frame after it had arrived. The mount's own pass is
    // the first visit; this counts the ones after it.
    const entered = routeOpen && !wasRouteOpen.current;
    wasRouteOpen.current = routeOpen;
    if (entered) setRoomVisits((n) => n + 1);
  }, [routeOpen]);

  const [members, setMembers] = useState<TeamMember[]>([]);
  const [loadingTeam, setLoadingTeam] = useState(true);
  /**
   * Whether this company's teammates can actually think (issue #1734/#1735),
   * **stamped with the scope that produced it**.
   *
   * `null` until the host has answered, and the `state` inside stays `null` on
   * a host that has no such field — an older one, or one that could not answer.
   * That silence is *not* evidence of an echo, so the banner stays down and
   * every row renders exactly as it did before. The failure this fixes is the
   * console asserting something it was never told; asserting the opposite would
   * be the same bug pointed the other way.
   *
   * The `client`/`company` stamp is what keeps that promise across a company
   * switch. This view stays mounted when `company` changes, and effects run
   * *after* the render that changed it — so a bare `CognitionState` would still
   * be holding the previous company's answer on the first render of the next
   * one, showing its banner and its Placeholder chips over a transcript they
   * are not about. Clearing inside the effect cannot fix that; it runs too
   * late. Deriving through the stamp below makes the stale value unreadable
   * rather than merely short-lived (CodeRabbit review of PR #1740).
   */
  /**
   * Monotonic ticket for the cognition read, so only the newest one commits.
   * A ref rather than state: it must be readable and bumped synchronously by a
   * read that is already in flight, and changing it must not re-render.
   */
  const cognitionRead = useRef(0);
  const [loadedCognition, setLoadedCognition] = useState<{
    client: OpenCompanyClient;
    company: string | null;
    state: CognitionState | null;
  } | null>(null);
  const [fromHost, setFromHost] = useState(false);
  /**
   * The company's channels — `null` until `/desks` has answered.
   *
   * Seeding this with `defaultDesks()` is what made every deep link flash
   * `#general`: the first render of every mount resolved the hash against the
   * fabricated `main`/`strategy`/`creative`/`frontdesk` set, then swapped under
   * the operator once the real desks landed (issue #370). `null` means "not
   * answered yet", so nothing resolves against a set the company doesn't have.
   */
  const [desks, setDesks] = useState<Desk[] | null>(null);
  /** Set when `/desks` failed for a reason that isn't "this host has none". */
  const [desksError, setDesksError] = useState<string | null>(null);
  /**
   * The identity of the always-present Operator feed (issue #1757 rework) —
   * fetched separately from `desks`, since it is its own surface now rather
   * than an entry `list_desks` returns. `null` until `/operator-channel` has
   * answered; a fetch failure leaves it `null` rather than surfacing an
   * error, since the pinned row degrading to absent is a much smaller loss
   * than blocking the rest of Chat on it.
   */
  const [operator, setOperator] = useState<OperatorChannelDto | null>(null);
  const [sending, setSending] = useState(false);
  const [composerPrefill, setComposerPrefill] = useState<{
    text: string;
    revision: number;
  } | null>(null);
  const [openThreadId, setOpenThreadId] = useState<string | null>(null);
  const [dismissingCardId, setDismissingCardId] = useState<string | null>(null);
  /** Every card whose review verdict is currently in flight — one entry per
   * task, not a single global slot, so a click on one card's Approve/Revise
   * control never gets silently dropped by a DIFFERENT card's in-flight
   * verdict (Codex #3906779123). See {@link canSubmitReview}. */
  const [reviewingCardIds, setReviewingCardIds] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  /** Issue #1846: which teammate's budget-pause redeem is in flight, if any —
   * so only that notice's button shows a busy state. */
  const [redeemingBudgetPauseAgent, setRedeemingBudgetPauseAgent] = useState<string | null>(
    null,
  );
  const [membersOpen, setMembersOpen] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  // The rail's "+" (issue #1835) — chat's own door for creating a channel.
  const [channelCreateOpen, setChannelCreateOpen] = useState(false);
  // The channel list is a section of the app sidebar now, so the sidebar owns
  // where it is, how dense it is, and whether it is covering the transcript.
  // See `components/room-rail.tsx`.
  const roomRail = useRoomRailSlot();
  // Whether the transcript is actually on screen. Two ways for it not to be,
  // and both have to be said here. The operator may be on another section
  // entirely — this view stays mounted to keep feeding the sidebar's rail
  // (#2130), so being mounted is no longer evidence of being visible. Or the
  // sidebar may be a sheet covering the whole screen, which is the phone.
  //
  // Mention clearing is gated on this so a mention cannot be marked read while
  // the operator is looking at the channel list (codex P1 review) — or at
  // Company, which would be the same defect one route further away.
  const chatPaneVisible = routeOpen && !roomRail.covering;
  const channelsCollapsed = roomRail.collapsed;
  // Section disclosure is shared by the desktop and sub-`lg` rail instances
  // (codex P2 review): each instance would otherwise keep its own fold state,
  // so dropping below `lg` reopened every section the operator had folded.
  const [railOpenSections, setRailOpenSections] = useState<Record<string, boolean>>({});
  const toggleRailSection = (id: string) =>
    setRailOpenSections((prev) => ({ ...prev, [id]: !(prev[id] ?? true) }));
  /** Your own avatar reference, once `loadViewer` has resolved who you are. */
  const [youAvatar, setYouAvatar] = useState<string | undefined>(undefined);
  const [effectiveHive, setEffectiveHive] = useState<{
    quorum: number;
    turnBudget: number;
  } | null>(null);

  /**
   * Ask the host whether this company can think (issues #1734, #1735).
   *
   * There is no other way to tell. A company with no inference configured
   * answers `200` with `"You said: <your message>"` from the offline echo
   * brain, and that reply reaches the transcript with the same shape as a
   * considered one — same avatar, same name, same timestamp. The runtime knows
   * the difference and, until #1735, never said so.
   *
   * Re-read on every company switch, and every answer is stamped with the
   * scope it came from — the read is what is scoped, not just when it is
   * cleared. On failure the stamped state is `null`: see the state declaration
   * for why silence must not become a claim.
   *
   * Also re-read whenever the tab comes back to the foreground, because this
   * answer can go stale under a console that is doing nothing at all: another
   * admin, or this operator in a second window, can configure inference and
   * rebuild the runtime while this chat sits open (codex, PR #1740). The
   * operator's *own* trip to Connections → Inference already re-reads — the shell
   * mounts and unmounts `RoomView` per route, so coming back remounts it — but
   * nothing covered the cross-session case, and a standing banner insisting
   * that a company which now thinks perfectly well cannot is the same class of
   * wrong claim as the one this surface exists to remove.
   *
   * A visibility hook rather than a poll: it re-asks exactly when someone is
   * about to read the answer, costs nothing while the tab is hidden, and adds
   * no host concept. It does not close the window for an operator who never
   * leaves the tab; a runtime revision on the wire is the complete answer, and
   * it belongs with the host rather than in a console-honesty fix.
   */
  useEffect(() => {
    let live = true;
    const read = async () => {
      // Which read this is. Two can be in flight at once — the mount's and a
      // visibility refresh's — and they are not guaranteed to settle in the
      // order they were issued, so a slow *older* one could otherwise land last
      // and put back the state the newer one had just corrected (codex, PR
      // #1740). The scope stamp cannot catch that: both carry the same scope.
      // Only the newest read may commit, in either direction, including its
      // failure path — a stale rejection overwriting a fresh success is the
      // same bug with the sign flipped.
      const ticket = ++cognitionRead.current;
      const isCurrent = () => live && ticket === cognitionRead.current;
      try {
        const capabilities = await client.capabilityStatus(company);
        if (isCurrent()) {
          setLoadedCognition({ client, company, state: capabilities.cognition ?? null });
        }
      } catch (e) {
        // An older host, or one that could not answer. Nothing is claimed
        // either way, and chat renders exactly as it did before the banner
        // existed.
        console.debug("[RoomView] cognition state unavailable", e);
        if (isCurrent()) setLoadedCognition({ client, company, state: null });
      }
    };
    void read();
    const onVisible = () => {
      if (document.visibilityState === "visible") void read();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      live = false;
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [client, company, roomVisits]);

  /**
   * The host's answer *for the company on screen right now*, or `null` while
   * this company's own read is still in flight. A value stamped with another
   * scope is not an answer about this one.
   */
  const cognition =
    loadedCognition &&
    loadedCognition.client === client &&
    loadedCognition.company === company
      ? loadedCognition.state
      : null;

  /**
   * The company is on the offline echo brain, whichever of the two reasons it
   * is for. Both mean the same thing to a reader of the transcript: the lines
   * on the company side were not written by the teammates they appear under.
   * The *cause* still travels separately, because the two have different
   * remedies and the banner and the chips both have to name the right one.
   */
  const echoing = echoCause(cognition) !== null;

  function toggleChannels() {
    const expanding = channelsCollapsed;
    roomRail.expand();
    // Expanding unmounts the compact rail's own expand button while a keyboard
    // user is still on it, dropping them at the document (issue #1340). Hand
    // focus to the sidebar's collapse control, which is the one control mounted
    // on BOTH density states now that the chat header no longer carries a
    // duplicate of it. Queried by its test id rather than threaded as a ref:
    // it is rendered by the shell, two components above this one, and that id
    // is already the contract `sidebar-toggle-reachable.spec.ts` pins it by.
    if (expanding) {
      window.requestAnimationFrame(() =>
        document.querySelector<HTMLElement>('[data-testid="sidebar-collapse"]')?.focus(),
      );
    }
  }

  /**
   * Which roster read this is, so only the newest may write — the same guard
   * `loadDesks`, `viewerRun` and `directoryEpoch` each keep, and the one this
   * read was missing (Codex P2 review on #2130).
   *
   * Two of these could not overlap while the read happened once per mount. They
   * can now: a console that loads off Room and is taken *into* Room before the
   * first `listTeam` settles starts a second one beside it. And the failure path
   * is the dangerous half — it writes unconditionally, so a slow older rejection
   * landing after a newer success replaces a real roster with `[]` and
   * `fromHost: false`. Direct messages vanish and "New channel" goes with them,
   * until some later entry to Room happens to fix it.
   */
  const rosterRead = useRef(0);
  const boot = useCallback(async () => {
    const ticket = ++rosterRead.current;
    const isCurrent = () => ticket === rosterRead.current;
    try {
      const roster = await client.listTeam(company);
      if (!isCurrent()) return;
      if (roster.length) {
        setMembers(roster.map(fromDto));
        setFromHost(true);
      } else {
        // Nobody, rather than a fabricated roster. A DM list of twelve invented
        // teammates offers conversations with agents the host has never heard
        // of, and the first message to one goes nowhere
        // (`docs/spec/runtime/company-setup.md`).
        setMembers([]);
        setFromHost(false);
      }
    } catch {
      // The roster read failed, so we do not know who works here. Still nobody:
      // guessing a team is what this change exists to stop. Guarded in both
      // directions — a stale rejection overwriting a fresh success is the same
      // bug with the sign flipped, which is the rule the cognition read already
      // states.
      if (!isCurrent()) return;
      setMembers([]);
      setFromHost(false);
    } finally {
      if (isCurrent()) setLoadingTeam(false);
    }
  }, [client, company, roomVisits]);

  /**
   * Hiding the budget controls from a non-admin is **courtesy, not
   * enforcement**. The host refuses the write with a 403 whatever this says;
   * showing an operator a control they cannot use is the only thing this
   * prevents.
   */
  // Only the newest load may write, exactly as `loadDesks` guards its own runs.
  // A scope change while a request is merely slow would otherwise let a stale
  // answer land last and wear the previous company's face on your own lines.
  // The face is cleared *before* the fetch so a slow request can never pin an
  // old avatar across a scope change; the timeline falls back to the
  // name-seeded mascot meanwhile.
  //
  // It used to read the user directory too, to attribute who set a teammate's
  // daily cap. There are no caps in the console any more, so the second request
  // went with them and this is one call again.
  const viewerRun = useRef(0);
  const loadViewer = useCallback(async () => {
    const run = ++viewerRun.current;
    setYouAvatar(undefined);
    try {
      const who = await fetchMe(client, company);
      if (run !== viewerRun.current) return;
      // Your own face, so your lines in a busy channel are yours at a glance.
      setYouAvatar(personAvatar(who));
    } catch {
      // No user plane on this host, or not signed in — leave the composer's
      // own lines on the name-seeded fallback.
    }
  }, [client, company, roomVisits]);

  useEffect(() => {
    setLoadingTeam(true);
    void boot();
    void loadViewer();
  }, [boot, loadViewer]);






  // Only the newest load may write. Two loads can be in flight at once — a
  // company switch, or a Retry over a request that is merely slow rather than
  // dead — and a stale answer landing last would replace the current company's
  // channels with the previous one's.
  const desksRun = useRef(0);
  /**
   * Whether the current `desks` state is `defaultDesks()` — the fabricated
   * starter set shown when the host exposes no desks — rather than the host's
   * own list. `onCreated` below needs the distinction (codex on #1872):
   * appending the company's first real channel *beside* the fallback would
   * leave nonexistent channels in the rail until reload, and a channel named
   * "Strategy" would collide with the fallback row of the same id, so
   * navigation could land on the fabrication instead of the real thing. The
   * moment one real desk exists the fallback set has no business rendering —
   * that is the fallback's own contract (`lib/desks.ts`).
   */
  const desksAreFallback = useRef(false);

  /**
   * The company's real desks, when the host exposes them — a company with its
   * own desks gets its own channels instead of the generic strategy/creative/
   * front-desk trio.
   *
   * Three outcomes, and they are three different facts:
   *
   * **A list, empty or not** — the host answered. An empty list means this
   * company has no desks, which is a fact about the company and is rendered as
   * itself: `#general` and the DMs, and no channels beside them. It used to
   * fall back to the fabricated trio, so a company that had never declared a
   * `[[group_chat]]` showed a Strategy desk, a Creative studio and a Front desk
   * that did not exist and could not be opened — while the overview graph, which
   * has no such fallback, correctly said the company had no desks. Two surfaces
   * disagreeing about the same read is how the fabrication was finally noticed;
   * the graph was right. This is the same rule as issue #370, applied to the
   * answer rather than to the failure: console-side desk invention is
   * indistinguishable from a real desk, so it does not happen.
   *
   * **404** — the host has no `.../desks` route at all (the pre-#53 shape the
   * Conversation path also tolerates). That is not an answer about the company,
   * so the static defaults still stand in: an old host's rail would otherwise
   * be empty of everything it once had.
   *
   * **Anything else** — a 500, a timeout, an offline tab — is a genuine
   * failure, and pinning the fabricated desks on top of it is what made a
   * broken `/desks` permanently show `#general` while the URL claimed a real
   * desk (issue #370). Those surface as an error the operator can retry.
   */
  /**
   * The `(client, company)` the desks on screen were loaded for.
   *
   * What decides whether a reload may blank the list. See `loadDesks`.
   */
  const desksLoadedFor = useRef<{ client: unknown; company: string | null } | null>(null);

  const loadDesks = useCallback(async () => {
    const run = ++desksRun.current;
    // Blank the list only when the SCOPE changed — a different host or a
    // different company, where the desks on screen belong to somebody else and
    // showing them for another moment would be a lie.
    //
    // A plain revisit is the other caller, and it must not blank anything.
    // `roomVisits` re-runs this every time an operator returns to Room, and
    // `setDesks(null)` sent `RoomView` down its `if (!desks)` branch — which
    // renders a loading pane and, crucially, no rail. The rail is portalled
    // into the app sidebar, so a refetch of data the operator already had tore
    // the channel list out of the sidebar and put it back a frame later,
    // resetting the sidebar's scroll position to the top every time they walked
    // back into Room. The list is re-rendered from the answer either way; what
    // is removed here is the empty frame in between.
    const scope = desksLoadedFor.current;
    if (!scope || scope.client !== client || scope.company !== company) setDesks(null);
    setDesksError(null);
    try {
      const dtos = await client.listDesks(company);
      if (run !== desksRun.current) return;
      // An answered read is never the fallback set, empty or not.
      desksAreFallback.current = false;
      desksLoadedFor.current = { client, company };
      setDesks(dtos.map(deskFromDto));
    } catch (error) {
      if (run !== desksRun.current) return;
      if (error instanceof ApiError && error.status === 404) {
        desksAreFallback.current = true;
        desksLoadedFor.current = { client, company };
        setDesks(defaultDesks());
        return;
      }
      setDesksError(
        error instanceof Error ? error.message : "Couldn't load this company's channels.",
      );
    }
  }, [client, company, roomVisits]);

  useEffect(() => {
    void loadDesks();
  }, [loadDesks]);

  /**
   * The always-present Operator feed's identity (issue #1757 rework),
   * fetched in parallel with `loadDesks` rather than derived from it — it is
   * its own surface now, not an entry `list_desks` returns. A failure is
   * swallowed rather than surfacing `desksError`: losing the pinned row is a
   * much smaller degradation than blocking the whole channel list on it, and
   * the fetch is retried on every company switch same as desks are.
   *
   * One bounded retry (issue #1781 review, Codex P2), the same
   * `fetchWithOneRetry` wrapper `app-shell.tsx`'s independent hydration pass
   * already uses for this identity: without it, a single dropped request
   * here — while the shell's own, retried lookup succeeds — left `operator`
   * `null` even though history kept hydrating, so the pinned row stayed
   * absent until the client/company changed or the page reloaded. See
   * `fetchWithOneRetry`'s doc for why the retry itself lives there rather
   * than inline.
   *
   * `fetchWithOneRetry` already collapses a genuine fetch failure to `null`
   * (issue #1781 review, tinysweeper): that and a 2xx response that simply
   * is not `OperatorChannelDto`-shaped both degrade to no pinned row here,
   * on purpose — see `isOperatorChannelDto`'s doc comment. But a non-`null`
   * value that still fails the shape check is a schema drift the fetch
   * itself did not report as an error, so it is logged (not surfaced —
   * still the same silent degrade) to keep that distinct from an ordinary
   * offline/older-host miss.
   */
  const operatorRun = useRef(0);
  useEffect(() => {
    const run = ++operatorRun.current;
    setOperator(null);
    void fetchWithOneRetry(() => client.getOperatorChannel(company)).then((dto) => {
      if (run !== operatorRun.current) return;
      if (isOperatorChannelDto(dto)) {
        setOperator(dto);
      } else if (dto !== null) {
        console.debug("[RoomView] getOperatorChannel returned an unexpected shape", dto);
      }
    });
  }, [client, company, roomVisits]);

  /** One attempt per bare-hash entry; see the effect below `channel`, which
   * is the single owner of what a bare `#/chat` resolves to. */
  const restoredFor = useRef<string | null | undefined>(undefined);

  /**
   * What a failed send needs to be sent again (B-099), by the optimistic id of
   * the row it failed on.
   *
   * A ref, not state: nothing rendered depends on it — `message.sendFailed` is
   * what draws the row — and a re-render per failed send would be a re-render
   * for a value only a click ever reads.
   *
   * It is deliberately not stored on the `ChatMessage`. Two of these five
   * fields are not on the bubble and cannot be recovered from it: the wire
   * `mentions` carry a target the rendered chip does not, and an `attachment`
   * is a workspace node reference rather than the projection the row shows. A
   * Retry rebuilt from the rendered message would quietly send a *different*
   * message — chip-less, or without its file — which is a worse failure than
   * the one being fixed.
   *
   * Declared **here**, beside the other long-lived refs, rather than beside
   * `retrySend` where it is read: three early returns sit between the two
   * (`desksError`, `!desks`, `!channel`), and a hook below them runs on some
   * renders and not others. That is React error 310 — a blank console with no
   * composer at all, on exactly the first paint, where `desks` is still `null`.
   * Every hook in this component belongs above those returns.
   */
  const failedSends = useRef<
    Map<
      string,
      {
        target: string;
        text: string;
        intent?: MessageIntent;
        parentId?: string;
        attachments?: AttachmentDto[];
        mentions?: Mention[];
      }
    >
  >(new Map());

  // No channels exist until the host has answered. Resolving against a
  // half-built list is exactly the first-paint swap issue #370 describes.
  //
  // The shell's live scope ref, not a local one: a local ref would freeze at
  // the last render's scope once this subtree unmounts, and a `client.chat`
  // still in flight from before the switch would then pass its stale check and
  // write the old company's reply into the new company's transcript. The shell
  // keeps updating its ref on every connection/company change, mounted or not,
  // so the comparison in `send` stays honest after Chat is gone (codex P1).

  // The pinned Operator row is appended *last* (issue #1757 rework) — after
  // every desk/DM section `buildChannels` produces — so `firstChannel` below
  // still defaults to a writable desk rather than the read-only feed.
  const sections = useMemo(() => {
    const base = desks ? buildChannels(members, desks, transcripts) : [];
    return operator ? [...base, operatorSection(operator)] : base;
  }, [members, desks, transcripts, operator]);
  // The hash's channel, else the first one that exists. There used to be a
  // literal "main" between the two — an id only the *fallback* desks carry, so
  // it matched nothing once a company's real desks loaded and matched the same
  // channel `firstChannel` returns when they hadn't. It never selected anything
  // the line below wouldn't; it only made "main" look like a real channel id
  // (issue #368).
  /**
   * A `#/chat/dm:…` link minted before issue #364 re-keyed DMs onto the
   * teammate's id, mapped onto the id that channel has now.
   *
   * One release of grace for a bookmarked or pasted link. Resolution only —
   * nothing is ever addressed or stored under the old id, so this shim can be
   * deleted without leaving anything stranded.
   */
  const decodedSub = channelIdFromSegment(sub);
  const resolvedSub =
    decodedSub && !findChannel(sections, decodedSub)
      ? resolveDmChannelId(decodedSub, members)
      : null;
  /**
   * A General *spelling* in the hash, mapped onto the channel that actually
   * renders the company-wide line.
   *
   * The host folds four addresses into one conversation — `""`, `main`,
   * `general` and `General`, case-insensitively (`isGeneralChannel`, mirroring
   * `is_general_chat`) — and everything downstream of a live frame already
   * applies that fold. Routing did not, so which of the four opened the channel
   * depended on how the company was declared: the built-in channel is `main`,
   * while a blueprint `[[group_chat]] id = "general"` is grandfathered onto the
   * line and the built-in steps aside for it ({@link generalChannelId}). One
   * spelling therefore worked and the other raised issue #370's "isn't a channel
   * here" — for the same conversation, in the same company.
   *
   * Only ever a *fallback*: the exact id is asked first, so a real desk whose id
   * happens to be a General spelling still wins its own channel, and this cannot
   * reroute anything that already resolves. It takes precedence over
   * `resolvedSub` for the reason `channelForThread` gives — a teammate whose id
   * is a General spelling does not inherit the company's line.
   *
   * The guided tour depends on it (PR #1984): its two composer stops address
   * `#/chat/main` explicitly so they cannot land on the read-only Operator feed,
   * which renders no composer and would silently skip both stops.
   */
  const generalSub =
    desks && decodedSub && isGeneralChannel(decodedSub) && !findChannel(sections, decodedSub)
      ? generalChannelId(desks)
      : null;
  /**
   * The channel the hash names, else the first one that exists.
   *
   * The rail only carries DMs with a transcript (issue #1335), so `findChannel`
   * answers `null` for an inactive DM — but `dm:<teammate-id>` is still a
   * valid, directly-addressable conversation. `directMessageForId` is the
   * all-roster resolver that keeps such a deep link (and the New message
   * picker's selection) landing on the DM before the first-channel fallback
   * takes over, without ever adding the inactive DM to the rail.
   */
  const channel = desks
    ? (findChannel(sections, generalSub ?? resolvedSub ?? decodedSub) ??
      directMessageForId(members, generalSub ?? resolvedSub ?? decodedSub) ??
      firstChannel(sections))
    : null;

  /**
   * The teammate whose raw turns this conversation can show, if any.
   *
   * A DM has exactly one agent on the other end, so "the raw turns" names
   * something. A `#channel` has several and the Operator feed has none, so
   * there is no such control there — a toggle that has to pick one of four
   * agents for you is worse than no toggle.
   */
  const rawAgentId = channel?.kind === "dm" ? (channel.member?.id ?? null) : null;
  /**
   * Whether the transcript is showing raw turns instead of chat.
   *
   * An address (`#/chat/<id>?raw`), not component state, for the reason `?edit`
   * is one on the agent page: "look at what it actually saw" is a link one
   * operator sends another, and Back closes it. `useHashView` strips everything
   * from `?` onward before it resolves a segment, so this rides the chat route
   * without the router ever seeing it.
   */
  const [rawRequested, setRawRequested] = useHashFlag("raw");
  const showRaw = rawRequested && !!rawAgentId;
  const [rawRows, setRawRows] = useState<AgentSessionMessageDto[]>([]);
  const [rawLoad, setRawLoad] = useState<RawLoad>("loading");
  // The same stale-response guard the Session tab carries (issue #1671): a read
  // started before the operator switched DMs must not commit its rows under the
  // next teammate's name.
  const rawGenerationRef = useRef(0);
  useEffect(() => {
    if (!showRaw || !rawAgentId) return;
    const generation = (rawGenerationRef.current += 1);
    setRawLoad("loading");
    void (async () => {
      try {
        const rows = await fetchDmRawTurns(client, rawAgentId, company);
        if (generation !== rawGenerationRef.current) return;
        setRawRows(rows);
        setRawLoad("ready");
      } catch (error) {
        if (generation !== rawGenerationRef.current) return;
        // A host without the per-agent session route is a host without this
        // surface, not a failure — saying so invites no debugging, and a red
        // error box does.
        const status = (error as { status?: number } | null)?.status;
        setRawLoad(status === 404 ? "unsupported" : "error");
      }
    })();
  }, [showRaw, rawAgentId, client, company]);

  /**
   * A bare `#/chat` is resolved **into the hash**, so which conversation is open
   * is routed state rather than derived state (B-096).
   *
   * Two facts made a draft escape into somebody else's DM. The first is that a
   * bare hash never named a channel: the magic-link landing route puts the
   * console on `#/chat` with no second segment, `useHashView` canonicalises the
   * *view* and knows nothing about chat's channels, and nothing else wrote one —
   * so `channel` above stayed the value of an expression over `members`,
   * `desks`, `transcripts` and `operator`, every one of which lands
   * asynchronously and can re-order what `firstChannel` answers. The second is
   * that the composer is deliberately ONE instance shared by every channel, and
   * its draft deliberately survives a channel change (see `MessageComposer`'s
   * `suppressed` doc, PR #1984) — so when the derived channel moved, the
   * half-written message moved with it and `send` addressed whatever `channel`
   * had become. The founder watched a message they wrote in `#general` post into
   * a private DM with a teammate.
   *
   * Keeping a draft across a switch is right and stays. What is wrong is a
   * conversation that can change with no navigation behind it, so this closes
   * the gap at that end: the moment there is a channel to name, its id goes in
   * the hash, and from then on `decodedSub` pins it. A deep link outranks
   * everything (`sub` short-circuits), so this can never fight one.
   *
   * It also subsumes issue #412's restore, which used to be its own effect
   * above. That is not a merge of convenience — two effects both writing the
   * hash for a bare entry raced, and the loser silently won: memory would
   * navigate to the remembered channel and the normaliser would immediately
   * replace it with `firstChannel`. One effect, one decision, memory first.
   *
   * `restoredFor` keeps this to one attempt per bare-hash entry per scope, so a
   * remembered channel the operator then navigates away from is not yanked back;
   * `sub` becoming truthy re-arms it for the next re-entry. The `!channel` guard
   * sits BEFORE the ref is stamped, so an entry that arrives before `/desks` has
   * answered waits for a real channel instead of burning its one attempt on
   * nothing.
   *
   * **Only while Chat is the open route** — `routeOpen`, PR #2134. This view is
   * mounted on *every* route since #2130, and a route that names no second
   * segment (`#/workflows`, `#/connections`) reaches this effect with a bare
   * `sub` and no channel in the hash, indistinguishable from a bare `#/chat`.
   * Writing a channel into the hash there restores nothing: it navigates the
   * operator straight OUT of the section they just opened, which is what
   * clicking Flows did before that guard — it landed on `#/chat/main`.
   *
   * That guard matters *more* here than it did before this change, not less.
   * The old effect only navigated when there was something remembered
   * (`if (remembered)`), so an operator who had never opened a channel was
   * accidentally spared; this one always resolves — memory, else `channel.id` —
   * which is the entire point of routing the decision instead of deriving it,
   * and would bounce every such operator out of Flows on the first paint.
   */
  useEffect(() => {
    if (!routeOpen) return;
    if (sub) {
      // A channel is named, so the next bare `#/chat` is a fresh re-entry.
      restoredFor.current = undefined;
      return;
    }
    // Nothing to normalise *to* yet. `channel` is null until `/desks` answers.
    if (!channel) return;
    // Scoped like `readLastChannel(scope)`: two connections serving the same
    // company must each restore their own remembered channel, so a host switch
    // cannot be mistaken for a re-entry into the previous host's state.
    const scopeKey = `${scope.connection}::${scope.company ?? "single"}`;
    if (restoredFor.current === scopeKey) return;
    restoredFor.current = scopeKey;
    // Memory outranks the fallback, and is written into the hash rather than
    // held here (issue #412): the channel on screen stays shareable, survives a
    // reload, and a remembered channel that has since been removed falls through
    // the same stale-id path as a bad deep link — raising issue #370's
    // unknown-channel notice rather than landing somewhere else in silence.
    onNavigate(readLastChannel(scope) ?? channel.id);
  }, [routeOpen, scope, sub, channel, onNavigate]);

  /**
   * The hash named a channel this company doesn't have, and the first-channel
   * fallback answered instead.
   *
   * Only meaningful once the desks are in: before that, *every* id looks
   * unknown. Derived rather than stored, so it clears itself the moment the
   * hash changes — there is no stale banner to dismiss. A legacy DM link that
   * the shim above resolved is not unknown; it found its channel.
   *
   * An inactive DM is not unknown either: the rail only carries DMs with a
   * transcript (issue #1335), so `findChannel` answers `null` for a DM the
   * picker just opened, but `directMessageForId` still resolves it against the
   * whole roster. Check that resolver explicitly rather than leaning on
   * `resolvedSub`, whose legacy-id shim is meant to be deletable.
   *
   * Nor is a General spelling the company renders under another id: `generalSub`
   * resolved it to a real channel, so naming it unknown would put a notice over
   * the conversation the operator actually asked for.
   */
  const unknownChannel =
    desks &&
    decodedSub &&
    !resolvedSub &&
    !generalSub &&
    !findChannel(sections, decodedSub) &&
    !directMessageForId(members, decodedSub)
      ? decodedSub
      : null;

  /**
   * Who is in the channel on screen — `null` when it names no membership, in
   * which case the pane falls back to the whole roster (issue #369).
   *
   * A desk's membership comes from the host. A DM's is the one teammate on the
   * other end: it has no `memberIds` (nothing in the model claims a DM has a
   * roster), so the two-person case is stated here rather than faked upstream.
   */
  const inChannel = useMemo(() => {
    if (!channel) return null;
    if (channel.kind === "dm") return channel.member ? [channel.member] : null;
    return channelMembers(channel, members);
  }, [channel, members]);

  /**
   * Everything an `@` can name in this company.
   *
   * Fetched once per company rather than per channel: the directory is
   * company-wide, and only the `inChannel` ranking below is per channel.
   *
   * A host that predates the route answers 404, which lands here as `null` —
   * read as "no picker", so typing an `@` stays plain text and the host still
   * extracts what it can. An older host therefore degrades to the composer's
   * previous behaviour rather than to a broken one.
   */
  const [directory, setDirectory] = useState<Mentionable[] | null>(null);
  /**
   * One epoch token shared by the mount fetch and `reloadDirectory`. Every
   * fetch bumps it and applies its response only while the token is still
   * current, so a fetch superseded by a company switch (or by a second roster
   * write) cannot land after the newer directory and hand the picker stale —
   * possibly cross-company — rows. Selecting a row the server will demote is a
   * bad row, so advertising it in the first place is what the guard prevents.
   */
  const directoryEpoch = useRef(0);
  /**
   * Re-read the mention directory.
   *
   * Called on mount and after a roster write, so a teammate added here appears
   * in the picker at once and one removed does not stay selectable until the
   * next reload (server revalidation would demote it, but offering a row that
   * can only fail is a bad row).
   */
  const reloadDirectory = useCallback(() => {
    const epoch = ++directoryEpoch.current;
    void client
      .mentionables(company)
      .then((d) => {
        if (epoch === directoryEpoch.current) setDirectory(mentionablesFor(d));
      })
      .catch(() => {
        if (epoch === directoryEpoch.current) setDirectory(null);
      });
  }, [client, company]);
  useEffect(() => {
    const epoch = ++directoryEpoch.current;
    setDirectory(null);
    void client
      .mentionables(company)
      .then((d) => {
        if (epoch === directoryEpoch.current) setDirectory(mentionablesFor(d));
      })
      .catch(() => {
        if (epoch === directoryEpoch.current) setDirectory(null);
      });
    return () => {
      directoryEpoch.current += 1;
    };
  }, [client, company, roomVisits]);

  /**
   * The directory with this channel's teammates marked, so they rank first.
   *
   * Re-marked rather than re-fetched on a channel switch — the rows are the
   * same, only their ordering hint changes.
   */
  const mentionables = useMemo(() => {
    if (!directory) return undefined;
    const inside = new Set((inChannel ?? []).map((m) => m.id));
    return directory.map((entry) =>
      entry.target.kind === "agent"
        ? { ...entry, inChannel: inside.has(entry.target.id) }
        : entry,
    );
  }, [directory, inChannel]);

  const outsideChannel = useMemo(() => {
    if (!inChannel) return members;
    const inside = new Set(inChannel.map((m) => m.id));
    return members.filter((m) => !inside.has(m.id));
  }, [inChannel, members]);

  const messages = useMemo(
    () => (channel ? (transcripts[channel.id] ?? EMPTY_MESSAGES) : EMPTY_MESSAGES),
    [transcripts, channel?.id],
  );
  /**
   * Whether this channel's history is still on the wire.
   *
   * `messages` cannot tell you — the `?? []` above collapses "never fetched"
   * into "empty", which is why the timeline could claim a reloaded DM was brand
   * new (issue #934). The roster is part of the answer too: a DM's channel only
   * exists once `members` lands, and until then nothing has even asked the shell
   * to hydrate it.
   */
  const historyPending = channel
    ? loadingTeam || !historyReady(hydration, channel.id)
    : false;
  const entries = useMemo(
    () => (channel ? buildTimeline(messages, channel, members, youAvatar) : []),
    [messages, channel, members, youAvatar],
  );
  /**
   * The open channel's thread replies, for the mention-clearing gate: a reply
   * is folded out of the main timeline (`buildTimeline`), so a mention inside
   * one must not clear on channel-open alone — only once the thread panel
   * actually renders it. Keyed by the console's `h<seq>` id, the namespace
   * `subjectId` on a mention notification meets through `hostMessageId`.
   */
  const replyParents = useMemo(() => {
    // Issue #1890 D: only the **folded** replies belong here. Since part 2 a
    // thread's first reply can render inline, and an inline reply is on screen
    // the moment the channel is — deferring its mention would leave a badge
    // that opening the channel cannot clear and no thread panel exists to.
    // Asked of `buildTimeline`'s own rule rather than re-derived, so the two
    // surfaces cannot drift about what is visible.
    const inline = inlineReplyIds(messages);
    const map = new Map<string, string>();
    for (const m of messages) {
      if (m.parentId && !inline.has(m.id)) map.set(m.id, m.parentId);
    }
    return map;
  }, [messages]);

  /** All loaded message ids in this channel, for the mention-clearing gate. */
  const loadedMessageIds = useMemo(() => new Set(messages.map((m) => m.id)), [messages]);

  /**
   * The approvals raised in the channel on screen (#379).
   *
   * **Derived, never appended.** The cards come from server state on every
   * render, so a pending one survives a reload — better than the transcripts it
   * sits among, which are still console-local (#364), and deliberately not
   * dependent on that being fixed.
   *
   * An approval with no `thread`, or one naming a thread this company has no
   * channel for, resolves to `null` and matches nothing. That is how a workflow
   * delivery or a scheduler tick stays Approvals-page-only.
   *
   * `desks` gates the derivation for the same reason it gates the channel list
   * (#393): before `/desks` answers, every thread id looks unknown, and placing
   * cards against a half-built channel set is the first-paint swap #370
   * describes one surface over.
   */
  const channelApprovals = useMemo(() => {
    if (!desks || !channel || !approvals?.length) return [];
    const byThread = chatChannelByThread ?? {};
    return approvals.filter((a) => a.thread && byThread[a.thread] === channel.id);
  }, [desks, channel, approvals, chatChannelByThread]);

  /**
   * Cards the operator has already decided, which the feed no longer carries.
   *
   * The host drops a resolved approval from `GET …/approvals` at once, so these
   * cannot be re-derived from `approvals` — they come from the shell's witnessed
   * map, which keeps the last-seen summary precisely so a decided card can
   * settle in place rather than blinking out of the thread.
   */
  const settledApprovals = useMemo(() => {
    if (!decidedApprovals || !channel || !desks) return [];
    const live = new Set(channelApprovals.map((a) => a.id));
    const byThread = chatChannelByThread ?? {};
    return Object.values(decidedApprovals)
      .map((d) => d.approval)
      .filter((a) => !live.has(a.id))
      .filter((a) => a.thread && byThread[a.thread] === channel.id);
  }, [decidedApprovals, channel, desks, channelApprovals, chatChannelByThread]);

  const askerNames = useAskerNames(client, company, channelApprovals);

  /**
   * The rooms this channel held, folded out of its own transcript.
   *
   * Derived rather than fetched: a deliberating desk journals nothing but its
   * turns, so the transcript **is** the episode and there is no episode endpoint
   * to ask. See `lib/hive/episode.ts`.
   *
   * `[]` for every DM, `#general`, the Operator feed and every desk that
   * answered with one ordinary turn — the fold looks for marker lines and the
   * reserved `hive-report` author and finds neither. Nothing here consults the
   * channel's kind, which is what keeps the surface unchanged for every
   * conversation that is not a room.
   */
  useEffect(() => {
    let live = true;
    setEffectiveHive(null);
    // Lightweight room-test clients and older hosts do not expose this optional
    // grammar read. The fold retains its derived policy in that case.
    if (!channel?.memberIds || typeof client.getDeskHive !== "function") return () => {
      live = false;
    };
    client
      .getDeskHive(channel.id, company)
      .then((hive) => {
        if (live) {
          setEffectiveHive({
            quorum: hive.effective.quorum,
            turnBudget: hive.effective.turnBudget,
          });
        }
      })
      // DMs and system channels have no desk grammar endpoint.
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [client, company, channel?.id, channel?.memberIds]);

  const episodes = useMemo(
    () =>
      foldEpisodes(
        // The complete transcript, not `entries` — `buildTimeline` folds
        // thread replies out of the main timeline, but hive turns and
        // `hive-report` messages can themselves be replies to the triggering
        // operator message, and `entries` would then miss those rows and
        // render no episode or an incomplete one.
        messages,
        // The seat count the host derives its quorum and turn budget from. Only
        // a hint: with no membership the fold falls back to its own default and
        // reports the number as derived rather than asserting one it cannot know.
        {
          members: channel?.memberIds?.length,
          quorum: effectiveHive?.quorum,
          turnBudget: effectiveHive?.turnBudget,
        },
      ),
    [messages, channel?.memberIds, effectiveHive],
  );

  const items = useMemo(
    () =>
      buildTimelineItems(
        entries,
        [...channelApprovals, ...settledApprovals],
        decidedApprovals ?? {},
        episodes,
      ),
    [entries, channelApprovals, settledApprovals, decidedApprovals, episodes],
  );

  /** Each deliberation turn by the message that carried it, for the rows. */
  const episodeTurn = useMemo(() => {
    const out: Record<string, EpisodeTurn> = {};
    for (const episode of episodes)
      for (const turn of [...episode.turns, ...episode.referrals])
        out[turn.messageId] = turn;
    return out;
  }, [episodes]);

  // Company-wide, not scoped to the open channel — see the function's own
  // doc for why a per-channel version silently redeemed the wrong marker
  // (issue #1846 review, Codex #3865395879).
  const budgetPauseMessageIdByAgent = useMemo(
    () => latestBudgetPauseMessageIdByAgent(transcripts),
    [transcripts],
  );

  /**
   * The marker id `GET …/budget-pause` returned for each budget-pause
   * notice, keyed by the notice's OWN `message.id` (issue #1846 review,
   * Codex #3868962374) — read back once, at the moment a notice becomes the
   * latest for its agent, rather than re-read live at click time.
   *
   * `redeemBudgetPause` used to re-read the live marker in its own click
   * handler and send THAT id as `?id=` — which sounds like it binds the
   * click to a specific marker, but the read happens at click time, so it is
   * always comparing "whatever is live right now" against itself. A
   * background turn (a workflow node, an unstreamed task) that re-parks the
   * SAME agent's marker with no chat destination BEFORE the click — which
   * `isBudgetPauseNoticeSuperseded` cannot see, since a chat-less park never
   * touches the transcript it watches — would have its id picked up by that
   * live read and redeemed instead, silently, under the operator's "add
   * credits" intent for the card they actually clicked.
   *
   * Reading the marker at RENDER time instead — the moment this becomes the
   * latest notice for its agent — narrows that window from "however long the
   * operator takes to notice the card" down to the round-trip of the one GET
   * fired here, the same "narrow, not eliminate" shape the server's own
   * `?id=` 409 guard already accepts for the GET→POST race it closes.
   */
  const [budgetPauseMarkerByNotice, setBudgetPauseMarkerByNotice] = useState<
    Map<string, string>
  >(new Map());
  // Issue #1846 review (Codex #3870014951 / #3870092746): company-scoped,
  // like `workflowRunEvents`/`openTurns`/`budgetProximity` above — host
  // message ids (`h<seq>`) are a per-company sequence, so a marker id cached
  // under company A's message id must not answer for company B's
  // identically-numbered one. `RoomView` is not remounted on a company
  // switch, so nothing else clears this map: `transcripts` resetting (in
  // `AppShell`) does not reach a `RoomView`-local `useState`.
  useEffect(() => {
    setBudgetPauseMarkerByNotice((prev) => (prev.size === 0 ? prev : new Map()));
  }, [client, company]);
  useEffect(() => {
    let live = true;
    // Issue #1906: `budgetPauseMessageIdByAgent` now also holds NO-RESEND
    // notices, so this can read back a marker for a notice that will never
    // draw a CTA to spend it. That is one wasted GET on a rare path, and the
    // alternative — filtering to redeemable notices here — would rebuild the
    // very blind spot the widened scan exists to remove, since "which notice
    // is latest" and "which notice gets a button" have to be answered by the
    // same map or an older notice's CTA goes stale-but-enabled again.
    for (const [agentId, messageId] of budgetPauseMessageIdByAgent) {
      if (budgetPauseMarkerByNotice.has(messageId)) continue;
      void client
        .getBudgetPause(agentId, company)
        .then((marker) => {
          if (!live || marker == null) return;
          setBudgetPauseMarkerByNotice((prev) =>
            mergeBudgetPauseMarkerRead(prev, messageId, marker.id),
          );
        })
        .catch(() => {
          // Issue #1846 review (Codex #3870092746): best-effort read-back —
          // every other `client.*` call in this file that is not inside a
          // try/catch ends with one. `redeemBudgetPause`'s live-read
          // fallback still covers this notice at click time if the cache
          // never gets populated (a host that lacks this route, a transient
          // network failure), so a swallowed rejection here degrades to no
          // worse than the pre-fix always-live-at-click-time behaviour
          // rather than an unhandled promise rejection on every effect run.
        });
    }
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `client`/`company`
    // intentionally excluded: this effect's job is "a new notice appeared",
    // not "the client changed". A client/company change is handled by the
    // reset effect above, which empties this map; `transcripts` resetting in
    // `AppShell` then re-derives `budgetPauseMessageIdByAgent`, so this effect
    // re-runs with the new scope's notices.
  }, [budgetPauseMessageIdByAgent]);

  /**
   * The `?thread=<id>` the address names right now, tracked reactively.
   *
   * `useHashView` deliberately parses only the path segments, so
   * `#/chat/general` → `#/chat/general?thread=h41` changes neither `sub` nor
   * `channel.id` and re-renders nothing an effect keyed on those would see. A
   * subscription of its own is the answer, and it is the one `useHashFlag`
   * already uses for `?new` — same event, same reason.
   *
   * `hashchange` is enough on its own. Every navigation in this console reaches
   * the address through `window.location.hash = …` (`useHashView`'s `navigate`,
   * `useHashFlag`'s setter) or through Back/Forward, and all of those fire it.
   * The one write that does not is `replaceState`, which is exactly how the
   * query below is *consumed* — so consuming leaves this value where it is and
   * the effect cannot loop on its own write.
   *
   * Carried with a **nonce**, not as a bare string, and that is not decoration:
   * consuming `?thread=h41` strips it from the address, so opening h41 a second
   * time is a real hash change whose parsed value is the one already held.
   * React would bail out of the re-render and the panel would not reopen —
   * verified in a browser, where the third of three `?thread=` links was the
   * one that did nothing. The nonce makes every hash change distinct; what
   * stops the effect acting on the ones that are not about threads is
   * `threadResolvedFor` below.
   */
  const [threadQuery, setThreadQuery] = useState<{ value: string | null; nonce: number }>({
    value: null,
    nonce: 0,
  });
  useEffect(() => {
    const read = () => {
      const [, query = ""] = window.location.hash.split("?");
      return new URLSearchParams(query).get("thread");
    };
    const apply = () => setThreadQuery((prev) => ({ value: read(), nonce: prev.nonce + 1 }));
    apply();
    window.addEventListener("hashchange", apply);
    return () => window.removeEventListener("hashchange", apply);
  }, []);

  /**
   * Which channel the thread panel was last resolved for, so "you arrived here"
   * can be told apart from "the address changed while you were already here".
   *
   * Only an arrival closes an open thread. A hash change that names no thread is
   * some other surface's query moving — or this effect's own `replaceState`
   * consuming the one it just opened — and neither is a reason to shut a panel
   * the operator is reading.
   */
  const threadResolvedFor = useRef<string | null>(null);

  /**
   * The nonce of the `?thread=` already acted on, so one link opens one thread
   * once (Codex P2 review on #2130).
   *
   * Consuming is a `replaceState`, which fires no `hashchange` — so the query
   * state keeps naming the thread it just opened until the address next moves.
   * `useHashView.navigate` writes `window.location.hash` and calls `setRoute`
   * **synchronously**, and `hashchange` arrives a task later: so picking another
   * channel re-runs this effect for the new `channel.id` while the query still
   * says `h41`. Without this ref that reopened h41 on the channel the operator
   * had just switched to, and the correcting pass could not undo it — by then
   * `threadResolvedFor` held the new channel, so `arrived` was false and the
   * stale panel stayed, suppressing that channel's live steps and receipt.
   *
   * A ref rather than clearing the state: clearing is a second render for a
   * fact that is not rendered. And a nonce rather than a boolean, because the
   * same thread id can legitimately arrive twice.
   */
  const consumedThreadNonce = useRef<number | null>(null);

  // An open thread only makes sense while its parent is on screen; arriving at
  // a channel closes whatever was open rather than leaving a panel pointing at
  // nothing. `?thread=<id>` on the hash opens straight into that thread
  // instead, and is consumed (stripped via `replaceState`) so it does not
  // reopen on a later switch back to this channel.
  //
  // `routeOpen` is both a guard and a dependency, and it has to be both since
  // #2130 (Codex P2 review on that PR).
  //
  // As a **dependency**, because arriving on Room no longer remounts this view.
  // A task card's "Opened from chat" link goes `#/tasks/<id>` →
  // `#/chat/<channel>?thread=<id>`, and this view can already be sitting on that
  // very channel — the shell replays the last chat segment while the address
  // belongs to another section. Only `routeOpen` and the query then change, so
  // keyed on `channel?.id` alone this never fired: the thread did not open and
  // the query was never consumed, so it lay in wait for a later switch.
  //
  // As a **guard**, because a view mounted off its own route must not rewrite
  // another section's address. This calls `replaceState` on whatever hash it
  // finds, and the hash it finds off Room belongs to Company or Flows.
  //
  // `threadQuery` is the third dependency, and it is what makes a same-channel
  // link work: `#/chat/general` → `#/chat/general?thread=h41` moves nothing else
  // (CodeRabbit review on #2130). That gap predates the rail work — the effect
  // was keyed on `channel?.id` alone before it — and is closed here rather than
  // carried, because pinning the rail made the same-channel case the *common*
  // one: this view now sits on a channel far more often than it used to.
  useEffect(() => {
    if (!routeOpen || !channel?.id) return;
    const arrived = threadResolvedFor.current !== channel.id;
    threadResolvedFor.current = channel.id;
    if (threadQuery.value !== null && consumedThreadNonce.current !== threadQuery.nonce) {
      consumedThreadNonce.current = threadQuery.nonce;
      setOpenThreadId(threadQuery.value);
      const [path, query = ""] = window.location.hash.replace(/^#/, "").split("?");
      const params = new URLSearchParams(query);
      params.delete("thread");
      const qs = params.toString();
      window.history.replaceState(null, "", `#${path}${qs ? `?${qs}` : ""}`);
      return;
    }
    if (arrived) setOpenThreadId(null);
  }, [routeOpen, channel?.id, threadQuery]);

  /**
   * `?m=<messageId>` — the line a search result names, brought into view.
   *
   * Carried with a nonce for the same reason `?thread=` is: consuming strips it
   * from the address, so opening the same message twice is a real hash change
   * whose parsed value is the one already held, and React would bail out of the
   * re-render.
   *
   * The id is the **console** id (`hostMessageId`, `h`-prefixed), which is what
   * `MessageRow` puts in `data-message-id`. The bare host id the history route
   * answers with matches no row.
   */
  const [messageQuery, setMessageQuery] = useState<{ value: string | null; nonce: number }>({
    value: null,
    nonce: 0,
  });
  useEffect(() => {
    const read = () => {
      const [, query = ""] = window.location.hash.split("?");
      return new URLSearchParams(query).get("m");
    };
    const apply = () => setMessageQuery((prev) => ({ value: read(), nonce: prev.nonce + 1 }));
    apply();
    window.addEventListener("hashchange", apply);
    return () => window.removeEventListener("hashchange", apply);
  }, []);

  const consumedMessageNonce = useRef(0);
  useEffect(() => {
    const wanted = messageQuery.value;
    if (!wanted || consumedMessageNonce.current === messageQuery.nonce) return;

    // The transcript for a channel just arrived at is still loading, and the row
    // cannot be scrolled to before it exists. Poll briefly rather than waiting
    // on a load signal this effect has no access to, and give up rather than
    // spin: a message that never appears is one the history did not carry.
    let attempts = 0;
    const find = () =>
      document.querySelector<HTMLElement>(`[data-message-id="${CSS.escape(wanted)}"]`);
    const timer = window.setInterval(() => {
      attempts += 1;
      const row = find();
      if (!row && attempts < 40) return;
      window.clearInterval(timer);
      if (!row) return;

      consumedMessageNonce.current = messageQuery.nonce;
      row.scrollIntoView({ block: "center", behavior: "smooth" });
      // Marked on the element rather than in React state: the transcript owns
      // that state and re-renders on every arriving message, and a highlight
      // that survives one is a highlight that has to be cleared by something.
      row.setAttribute("data-found", "true");
      window.setTimeout(() => row.removeAttribute("data-found"), 2600);

      const [path, query = ""] = window.location.hash.replace(/^#/, "").split("?");
      const params = new URLSearchParams(query);
      params.delete("m");
      const qs = params.toString();
      window.history.replaceState(null, "", `#${path}${qs ? `?${qs}` : ""}`);
    }, 100);
    return () => window.clearInterval(timer);
  }, [messageQuery, channel?.id]);

  // Whoever owns the unread counts needs to know what is actually being looked
  // at. Re-runs as the open channel's transcript grows, not only on a switch:
  // a reply that lands while you are reading the channel is read, and should
  // not leave a badge on the channel you are sitting in. It also re-runs when
  // a thread opens or closes: opening a thread renders its replies, so the
  // replies' mentions — which the channel-open alone must not clear — clear
  // the moment the thread makes them visible.
  //
  // Gated on the transcript actually being on screen: on a phone the sidebar
  // holding the channel list is a sheet over the whole screen, and a mention
  // that lands while the operator is only looking at that list must not be
  // marked read behind their back. The gate itself is a dependency, so closing
  // the sheet re-runs the report and clears whatever is newly visible.
  useEffect(() => {
    if (channel && chatPaneVisible)
      onChannelViewed?.(
        channel.id,
        historyPending,
        mentionFeedRevision,
        replyParents,
        openThreadId,
        loadedMessageIds,
      );
  }, [
    channel?.id,
    messages.length,
    historyPending,
    mentionFeedRevision,
    onChannelViewed,
    replyParents,
    openThreadId,
    loadedMessageIds,
    chatPaneVisible,
  ]);

  // The visibility half of the report above: `onChannelViewed` only ever says
  // *which* channel, and only while it is visible, so nothing tells the shell
  // the moment that stops being true — its "last channel seen" memory keeps
  // naming whatever was visible before the operator dropped to the rail. A
  // plain mirror of `chatPaneVisible`, not folded into that report, because
  // the two callbacks answer different questions the shell must not conflate
  // (see the prop doc).
  useEffect(() => {
    onChatPaneVisibilityChange?.(chatPaneVisible);
  }, [chatPaneVisible, onChatPaneVisibilityChange]);

  /**
   * Close the dialogs that belong to the Room *route* when the operator leaves
   * it (Codex P2 review on #2130).
   *
   * Both of these open from the members pane, which is inside the `routeOpen`
   * gate — but the dialogs themselves are deliberately outside it, because two
   * of their siblings have triggers painted in the sidebar and must open from
   * any section. That is right for those two and wrong for these: leaving Room
   * on Back, Forward or a typed hash used to unmount this whole view, and now
   * leaves an "Add teammate" sheet standing over Company. `BudgetDialog` is
   * worse than untidy — it holds the member it was opened for, so it would come
   * back later still pointing at them.
   *
   * Closed rather than unmounted, so returning to Room does not find them
   * reopened by state nobody cleared.
   */
  useEffect(() => {
    if (routeOpen) return;
    setAddOpen(false);
    // And the thread panel, which used to close because leaving Room unmounted
    // the whole view. Clearing the marker with it makes the next arrival an
    // arrival, so Room opens on the channel rather than on a panel the operator
    // left behind two sections ago.
    setOpenThreadId(null);
    threadResolvedFor.current = null;
  }, [routeOpen]);

  // Upload one attachment's bytes for the composer (issue #1682). Bound to the
  // active connection's client/company so the composer stays agnostic of both.
  // Must live above the early returns: a hook after them is skipped on the
  // loading render and present on the next, which is a Rules-of-Hooks crash
  // ("Rendered more hooks than during the previous render").
  const uploadAttachment = useCallback(
    (file: File) => uploadChatAttachment(client, company, file),
    [client, company],
  );

  // Fetch a stored attachment's bytes as an object URL for the transcript
  // (issue #1682). The blob route needs the client's bearer, which an `<img>`
  // or a bare link cannot carry — so the row resolves through this and the
  // caller revokes the URL when done. Reuses the hardened `/workspace/blob`
  // serve untouched. The optional `signal` lets a preview that scrolls out of
  // view cancel its in-flight download (codex review finding).
  const resolveAttachmentUrl = useCallback(
    (nodeId: string, signal?: AbortSignal) =>
      fetchBlobUrl(client, company, nodeId, signal),
    [client, company],
  );

  /**
   * Delete an uploaded-but-never-sent attachment's workspace node (issue
   * #1682, codex review finding).
   *
   * A staged file is uploaded (and charged against the workspace quota) the
   * moment it lands, before the operator has sent anything — replacing it,
   * removing it, or leaving the composer used to just drop the local
   * reference, leaving the binary node on the server forever. Bound the same
   * way `uploadAttachment` is; best-effort, since a failed cleanup here must
   * never block the operator from continuing to compose.
   */
  const deleteAttachment = useCallback(
    (nodeId: string) => {
      void deleteNode(client, company, nodeId).catch(() => {
        // Best-effort: an orphaned node here is a quota nuisance, not a
        // correctness bug, and the operator has already moved on.
      });
    },
    [client, company],
  );

  /*
    Chat is its own content (`components/page-header.tsx`'s `hidden` variant):
    the channel it opens on already carries its own visible title
    (`ChatHeader`'s own `h1`), so the page keeps only an accessible name and
    paints nothing over it.

    Read once into a const rather than duplicated into every early return —
    `SearchView` and `FinancesView` take the same shape. Without it, the two
    states below rendered nothing before `ChatHeader` mounts: a company still
    loading its desks, or one with no channel to open at all, so a screen
    reader got a page with no accessible name until a channel existed
    (issue #1781 review, Codex P2; `page-header-precedes-every-return.test.ts`
    covers every routed view, this one included).
  */
  const header = <PageHeader hidden title="Chat" />;

  // Off Room this view exists only to keep the channel rail alive in the
  // sidebar, and in all three states below there is no rail to render — no
  // desks, or none that loaded. Whatever section the operator actually is in
  // owns the content area, so contribute nothing to it rather than painting a
  // chat empty-state over Company.
  if (!routeOpen && (desksError || !desks || !channel)) return null;

  // Three ways to have no channel on screen, which used to be one blank pane.
  // Which one it is, is the whole point: "still loading" and "this company has
  // nothing" are different facts and only one of them is worth acting on.
  if (desksError) {
    return (
      <>
        {header}
        <EmptyPane
          title="Couldn't load this company's channels"
          body={desksError}
          action={{ label: "Retry", onClick: () => void loadDesks() }}
        />
      </>
    );
  }
  if (!desks) {
    return (
      <>
        {header}
        <LoadingPane />
      </>
    );
  }
  if (!channel) {
    return (
      <>
        {header}
        <EmptyPane
          title="No channels yet"
          body="This company has no desks and nobody on its roster, so there is nothing to talk to. Add an agent and their direct message shows up here."
          action={{ label: "Add an agent", onClick: () => setAddOpen(true) }}
          after={
            <AddMemberDialog
              open={addOpen}
              onOpenChange={setAddOpen}
              onAdd={addMember}
              client={client}
              company={company}
            />
          }
        />
      </>
    );
  }
  // A local the closures below can capture as non-null: TypeScript hoists
  // function declarations, so the guard above does not narrow inside them.
  const active = channel;
  // Whether the open channel is a real, host-backed desk — as opposed to the
  // built-in `#general` channel, a DM, or a fallback desk (`lib/desks.ts`,
  // used before `/desks` answers). The built-in channel is `kind: "channel"`
  // and carries `memberIds` exactly like a desk does, so neither alone tells
  // them apart; asking the desk list is what keeps the lead badge and the
  // org-chart link off a channel the host does not list under `GET .../desks`.
  const activeIsDesk = active.kind === "channel" && (desks ?? []).some((d) => d.id === active.id);
  // Issue #1757: the Operator channel is a read-only "what happened" feed. Its
  // composer is disabled and the host also refuses a send to it, so this is UX,
  // not the enforcement.
  const readOnly = Boolean(channel?.system);
  // The host thread this channel is addressed on. A real desk channel's id
  // doubles as its thread id (`deskFromDto`), so addressing by it routes to
  // that desk's lead. A DM's id is console-local (`dmChannelId`), not a host
  // thread — but `chat` also accepts a roster teammate id directly
  // (`responder_for` in `src/harness/brain.rs`), which is exactly what a DM's
  // `member.id` is, so a DM addresses that teammate the same way a desk
  // addresses its lead. It is also the id every live turn frame carries.
  const activeThreadId = active.system
    ? undefined
    : active.kind === "channel"
      ? active.id
      : active.member
        ? dmThreadId(active.member)
        : undefined;
  const liveSteps = activeThreadId ? liveStepsByThread?.[activeThreadId] : undefined;
  // The live receipt for this channel's thread (issue #1934), resolved exactly
  // as `liveSteps` above — same host thread id, same open-thread exclusion at
  // the render site below.
  const receipt = activeThreadId ? receiptByThread?.[activeThreadId] : undefined;
  /**
   * The turn this channel is waiting on, if any (issue #983).
   *
   * Sourced from the shell rather than from local `sending`, which is the whole
   * point: `sending` only knows about a POST *this* component made, so it went
   * false on every reload and on every walk to another view. An open turn is a
   * fact about the company, so the indicator survives both.
   */
  /**
   * The turns open in this channel, split by whether they belong to the thread
   * the panel is showing.
   *
   * They used to be one lookup on the channel id, which could not tell the two
   * apart — so `RoomView` suppressed the channel's indicator whenever any
   * thread was open, and a turn the host was actively running showed nowhere at
   * all. The shell now keys them per thread (`turnStateKey`), which is what
   * makes this split expressible.
   *
   * `channelTurn` deliberately spans *every other* thread in the channel rather
   * than only channel-rooted turns: from the channel timeline, a turn running in
   * a thread you are not reading is still this channel's work, and saying
   * nothing about it is the failure this replaces.
   */
  // Only when a thread is actually open. Without the `openThreadId` guard this
  // collapses to the channel key for an unthreaded view, and `openTurn` below —
  // which excludes it — would then hide the channel's own turn: the exact
  // silence this change exists to remove, reintroduced one line down.
  const threadTurnKey =
    activeThreadId && openThreadId
      ? turnStateKey(activeThreadId, threadRootOf(openThreadId))
      : undefined;
  const threadTurn = threadTurnKey ? openTurns?.[threadTurnKey]?.[0] : undefined;
  const openTurn = (() => {
    if (!activeThreadId) return undefined;
    const candidates = Object.entries(openTurns ?? {})
      .filter(
        ([key, turns]) =>
          key !== threadTurnKey &&
          (key === activeThreadId || key.startsWith(`${activeThreadId}#`)) &&
          turns.length > 0,
      )
      // Every turn, not each list's head. `mergeOpenTurns` appends rather than
      // re-sorts, so a reload re-arm racing a detached POST can leave a running
      // row *behind* a queued one in the same list — and a search over heads
      // alone would never see it, which is the same "Queued…" over live work
      // this is here to prevent (Codex review on #2044).
      .flatMap(([, turns]) => turns);
    // A running turn outranks a queued one. Taking the first match instead
    // would let map order decide the wording, and map order follows `/runs`,
    // which is newest-first — so the ordinary serialized case (an older turn
    // working while a newer one waits on the company lock) rendered "Queued…"
    // over live work (Codex review on #2042).
    return candidates.find((t) => !t.queued) ?? candidates[0];
  })();
  /**
   * The count beside the channel title.
   *
   * A DM is stated as 2 rather than derived: it is a two-person conversation,
   * but the operator has no roster row, so counting rows would say 1 and
   * inventing a "You" row to make the arithmetic work would be worse. A desk
   * counts its own members; a channel with no membership of its own still
   * counts the company, which is all it can honestly claim.
   */
  const headerCount = active.kind === "dm" ? 2 : (inChannel?.length ?? members.length);
  /**
   * The teammate on the other end of this DM exists only in the console (issue
   * #364) — a starter-roster row, or one added while the host had no team write
   * plane.
   *
   * Worth saying out loud, and worth being precise about *what* is local. The
   * transcript is not: the DM is addressed by a reload-stable id now, so the
   * host journals it and gives it back. What is missing is the other half of the
   * conversation — there is no such agent on the company, so nothing on the
   * roster answers. Claiming the whole channel was console-local would be the
   * old, wrong story; saying nothing would leave an operator waiting for a reply
   * that is never coming.
   */
  const consoleOnlyMember =
    active.kind === "dm" && !fromHost && active.member ? active.member.name : null;

  const append = (channelId: string, ...added: ChatMessage[]) =>
    setTranscripts((t) => ({ ...t, [channelId]: [...(t[channelId] ?? []), ...added] }));


  /**
   * Send a failed line again (B-099).
   *
   * The failed row is dropped first, because `send` appends its own optimistic
   * bubble: keeping both would show the operator their message twice, one of
   * them a corpse. Its retry payload goes with it, so a Retry cannot be
   * replayed twice from one click; a second failure re-registers a fresh one
   * under the new row's id.
   *
   * Nothing is retried automatically. A throw is ambiguous — the host may have
   * journaled the message before the request died (see `send`'s doc) — so an
   * automatic resend would risk posting the same instruction twice. Whether
   * that is worth it is the operator's call, which is exactly what a button is.
   */
  const retrySend = (messageId: string) => {
    // `send` itself no-ops while a POST is already in flight (`if (sending)
    // return false`). Checked here too, before the payload and failed row are
    // consumed, because otherwise a Retry clicked mid-send would drop both —
    // silently losing the only copy of the retry (CodeRabbit review) — and
    // `send`'s own guard would have nothing left to return `false` about.
    if (sending) return;
    const payload = failedSends.current.get(messageId);
    if (!payload) return;
    failedSends.current.delete(messageId);
    setTranscripts((t) => ({
      ...t,
      [payload.target]: (t[payload.target] ?? []).filter((m) => m.id !== messageId),
    }));
    void send(payload.text, payload.intent, payload.parentId, payload.attachments, payload.mentions);
  };

  /**
   * Post a line and thread the company's answer back into the same place.
   * `parentId` set means the exchange stays inside the thread panel.
   *
   * The thread is now a fact about the transcript, not about this browser: the
   * parent goes to the host with the message, and both halves come back under
   * it on the next reload (issue #364). A parent that has no durable id yet is
   * dropped from the request rather than sent as a local counter the host
   * cannot resolve — the row's own actions are disabled in that window, so this
   * is the belt to that brace.
   *
   * Returns whether the POST reached the host and journaled (codex review
   * round 4, on top of round 2's naive version): `true` for every outcome
   * where `client.chat` itself resolved (a normal reply, a detached turn, or
   * a stale response the caller discards) — the host answered, so the
   * journal write is a fact. `undefined` when the request THREW, because a
   * throw is genuinely ambiguous: `accept_chat_turn` journals the message
   * before the turn's cycle is spawned onto its own task, and a synchronous
   * (non-detached) send then awaits that task — so a failure surfacing from
   * deep in cycle execution reaches this `catch` looking identical to one
   * that never reached the journal at all. There is no `false` this function
   * ever returns: nothing observable here tells "refused before journal"
   * and "journaled, then the turn itself failed" apart. The composer treats
   * `undefined` as "unknown — leave it alone", never as "not sent"; see
   * `deleteAttachment` and `MessageComposer.send`.
   */
  async function send(
    text: string,
    intent?: MessageIntent,
    parentId?: string,
    attachments?: AttachmentDto[],
    mentions?: Mention[],
  ): Promise<boolean | undefined> {
    // The one genuinely safe `false`: another send is already in flight, so
    // this call's own text/attachments were never handed to `client.chat` at
    // all — no server round trip happened for them, no ambiguity possible.
    if (sending) return false;
    const scopeAtSend = {
      connection: scope.connection,
      company: scope.company,
      client,
    };
    const target = active.id;
    const chatId = activeThreadId;
    // The optimistic bubble carries the attachments too, so the operator sees
    // the file on their own message the instant they send (issue #1682) — the
    // SSE echo / reload copy then matches it, projected from the same durable
    // references the host resolved.

    // Warn about @-mentioning a teammate who is not on this channel.
    if (mentions?.length && inChannel) {
      const channelIds = inChannel.map((m) => m.id);
      const outside = mentionsOutsideChannel(mentions, channelIds);
      if (outside.length) {
        toast.warning(
          outside.length === 1
            ? "An agent you @-mentioned is not on this channel — they won't see the message."
            : `${outside.length} agents you @-mentioned are not on this channel — they won't see the message.`,
        );
      }
    }

    // Chips need a label and a `mine` flag, which the wire mention (target +
    // span) does not carry; the directory supplies the label. The optimistic
    // row is never replaced by history — the id reconcile below makes the
    // durable row "known", so `hydrateChannel` skips it — which means the
    // metadata has to land on this row or the just-sent line renders without
    // chips until reload.
    const localMentions = mentions?.map((m) => {
      // `sameTarget` rather than an inline comparison: narrowing one operand
      // says nothing about the other, so the inline form does not typecheck —
      // and the rule belongs in one place regardless.
      const row = mentionables?.find((e) => sameTarget(e.target, m.target));
      return {
        text: m.text,
        // Incoming/rendered mention offsets use the host's UTF-8 byte contract;
        // the composer keeps UTF-16 offsets only while editing.
        offset: utf8ByteLength(text.slice(0, m.offset)),
        label: row?.label ?? m.text,
        // `@everyone` addresses the room, the author included; a pick of a
        // teammate or person names somebody else.
        mine: m.target.kind === "everyone",
      };
    });
    const local = makeMessage("you", text, {
      parentId,
      attachments,
      mentions: localMentions?.length ? localMentions : undefined,
    });
    append(target, local);
    setSending(true);
    // Claim the thread for the duration of the POST. The backend journals an
    // `AgentReply` for our own turn too and pushes it over SSE mid-await, so
    // without this the shell injects that echo *and* the awaited reply lands
    // below — two bubbles for one turn.
    //
    // The generation the shell stamped this send's receipt with, if any
    // (issue #1935 review). Threaded through to whichever terminal callback
    // this POST reaches below, so a clear this send triggers can never delete
    // a receipt a *later* send has since armed for the same (possibly
    // cross-company-reused) thread id — see `shouldClearReceipt`.
    // Armed under the same key the reload leg folds runs into, or the two
    // legs describe the same turn under two names and the indicator that
    // survives a reload is not the one the POST armed.
    //
    // **Unthreaded (`parentId` unset) always keys the channel outright** —
    // `turnStateKey(chatId)` with no root — regardless of what thread happens
    // to be open in the panel beside it. The channel composer always sends
    // `parentId: undefined`, and the channel row's own Retry button lives on
    // that same top-level transcript, so both a fresh top-level send and a
    // top-level failure's Retry can fire while an *unrelated* thread in this
    // channel is open in the panel (`openThreadId` naming that other thread's
    // root). Keying on `openThreadId` there used to mis-file the whole send —
    // its receipt and open-turn recovery registered under a thread it never
    // touched, so the panel showed work it did not start while the channel
    // that actually ran it showed nothing (codex P2, PR #2052 review). A
    // threaded retry cannot hit this: its Retry button only renders inside
    // the thread panel for the thread it belongs to, so `openThreadId`
    // already names that same thread whenever it is clickable.
    //
    // **Threaded (`parentId` set) still keys off `openThreadId`, not
    // `parentId` itself.** A review reply is anchored to a *reply*
    // (`threadReviewAnchor.anchorId`), not to the thread root, so keying on
    // the parent would arm a key the panel's own lookup — which keys on the
    // open thread — could never match. `parentId` stays what it was: the
    // host's `parent`, for `client.chat` alone.
    const stateKey = chatId
      ? parentId === undefined
        ? turnStateKey(chatId)
        : turnStateKey(chatId, threadRootOf(openThreadId ?? undefined))
      : undefined;
    const gen = stateKey ? onSendStart?.(stateKey) : undefined;
    // Which of the POST's three outcomes actually happened, decided here and
    // reported once in the `finally`. Only `"resolved"` means the reply is on
    // screen; the other two leave a turn running on the host and the stream as
    // the delivery path, so telling the shell "ended" for either would take the
    // working row down mid-turn (detached) or throw away the reply it was
    // holding (failed). See `PendingSyncPosts` for the table.
    let outcome: "resolved" | "detached" | "failed" | "stale" = "resolved";
    // Every reply line a settled response actually carries, read by
    // `onSendEnd` (issue #101 review) to tell a held system frame the
    // response duplicates from one it never will. Declared here, not inside
    // the `try` block that fills it in, so `finally` below can still see it.
    let responseTexts: string[] = [];
    try {
      const answer = await client.chat(
        text,
        company,
        chatId,
        toHostMessageId(parentId),
        intent,
        // Ask for the turn's id rather than its answer. A host that predates the
        // field ignores this and answers synchronously, which is why the branch
        // below reads the response's shape and never this argument.
        true,
        // Node ids only (issue #1682): the host re-resolves each against this
        // company's workspace and takes the name/mime/size from the store, so
        // the client neither sends nor is trusted for that metadata.
        attachments?.map((a) => a.nodeId),
        // Who the picker resolved. The host re-validates every entry and
        // demotes what no longer exists, so this is a suggestion; omitting it
        // asks the host to extract from the text instead.
        //
        // The composer tracks offsets as UTF-16 indices (they drive textarea
        // and reconcile operations); the host reads them as UTF-8 bytes, so
        // each is converted to the byte length of its prefix here.
        mentions?.map((m) => ({
          ...m,
          offset: utf8ByteLength(text.slice(0, m.offset)),
        })),
      );
      const latestScope = scopeRef.current;
      if (
        latestScope &&
        (scopeAtSend.company !== latestScope.company ||
          scopeAtSend.connection !== latestScope.connection ||
          scopeAtSend.client !== latestScope.client)
      ) {
        outcome = "stale";
        if (chatId) onSendStale?.(chatId, gen);
        // The POST itself succeeded and journaled — this branch only
        // discards the reply because the scope moved on, so anything the
        // request carried (an attachment among them) is durably claimed.
        return true;
      }
      // Reconcile the optimistic id first, for BOTH shapes. On the detached one
      // this is strictly better than what came before: since #983 the message is
      // journaled at accept time, so its durable id is a fact within
      // milliseconds instead of after the whole turn — the bubble becomes
      // replyable and reactable immediately rather than at settle.
      if (answer.messageId) {
        setTranscripts((t) => ({
          ...t,
          [target]: reconcileIds(t[target] ?? [], local.id, answer.messageId!),
        }));
      }
      if (isDetachedChat(answer)) {
        outcome = "detached";
        // Nothing to render: the reply arrives on the stream, and durably in
        // `chat/history` when the shell sees the turn go terminal. The working
        // row stays up, driven by the open turn rather than by this POST.
        // The desk goes with the state key: the key can be composite and the
        // shell's settle poll has to ask the host about a real desk.
        if (stateKey) onSendDetached?.(stateKey, answer.turnId, gen, chatId);
        return true;
      }
      const reply = answer;
      // What `onSendEnd` hands `PendingSyncPosts.ended`, unconditionally —
      // even the `(no reply)` / review-feedback branches count as "this
      // response carried nothing", which is exactly what an empty array
      // already says correctly.
      responseTexts = reply.responses.map((r) => r.text);
      // Fired here, BEFORE `append` below, and not from the `finally` block
      // this used to run from (Codex review, PR #2052). `onSendEnd` releases
      // any held system frame the response above didn't carry — B-101's
      // mention-ambiguity note, always journaled on the host *before* the
      // reply it is about. Releasing it after `append` would render the
      // reply first and the note second, the reverse of `chat/history`'s own
      // order, so the note visibly jumps backward past the answer on the
      // very next reload. Firing it here instead keeps the live order and
      // the durable order the same. `finally` below no longer fires it for
      // the `"resolved"` case this is the only path that reaches.
      if (stateKey) onSendEnd?.(stateKey, gen, responseTexts);
      const replies = reply.responses.length
        ? reply.responses.map((r) =>
            // Same rule as the live path and `fromHistory`: a host-authored
            // response renders as a centred row, not an agent bubble.
            makeMessage(replyVoice(r.channel), r.text, {
              channel: r.channel,
              parentId,
              steps: r.steps,
              taskId: r.taskId,
              messageId: r.messageId,
              mentions: r.mentions,
            }),
          )
        : reply.reviewFeedbackApplied
          ? []
          : [makeMessage("system", "(no reply)", { parentId })];
      append(target, ...replies);
      // The synchronous response predates mention metadata on some hosts. A
      // reply is already journaled by the time this response arrives, so fetch
      // the authoritative projection and merge its mention DTOs onto the
      // optimistic reply instead of leaving the live row chip-less until a
      // full reload.
      if (chatId && replies.some((reply) => reply.id.startsWith("h"))) {
        void client
          .getChatHistory(chatId, company)
          .then((entries) => {
            // The reply was optimistic; the history fetch that hydrates it lands
            // asynchronously. If the operator switched company, channel or
            // connection in between, this target has been re-homed — merging the
            // old scope's mention DTOs onto an unrelated same-id message would
            // decorate the wrong transcript. Drop the hydration in that case;
            // the next history fetch for the new scope is authoritative.
            const latestScope = scopeRef.current;
            if (
              latestScope &&
              (scopeAtSend.company !== latestScope.company ||
                scopeAtSend.connection !== latestScope.connection ||
                scopeAtSend.client !== latestScope.client)
            ) {
              return;
            }
            const hydrated = fromHistory(entries);
            const byId = new Map(hydrated.map((message) => [message.id, message]));
            setTranscripts((transcripts) => ({
              ...transcripts,
              [target]: (transcripts[target] ?? []).map((message) => {
                const authoritative = byId.get(message.id);
                return authoritative?.mentions
                  ? { ...message, mentions: authoritative.mentions }
                  : message;
              }),
            }));
          })
          .catch(() => {
            /* The next history hydration remains the fallback. */
          });
      }
      onReply?.();
      return true;
    } catch (err) {
      outcome = "failed";
      const latestScope = scopeRef.current;
      if (
        latestScope &&
        (scopeAtSend.company !== latestScope.company ||
          scopeAtSend.connection !== latestScope.connection ||
          scopeAtSend.client !== latestScope.client)
      ) {
        outcome = "stale";
        if (chatId) onSendStale?.(chatId, gen);
        // Unlike the try-block's stale branch above, the request THREW here —
        // whether it journaled before failing is unknown, not "no" (see this
        // function's doc comment), so this is `undefined`, not `false`.
        return undefined;
      }
      // Still said, even when the reply arrives on the stream a moment later:
      // the request did fail, and an operator not told that has no way to know
      // whether their message was taken at all. The two facts are not in
      // competition — this marks the request, the shell renders whatever the
      // turn goes on to produce.
      //
      // Marked ON the message rather than appended beside it (B-099). The old
      // sibling `system` line left the bubble itself indistinguishable from a
      // delivered one — same avatar, same timestamp, no warning of any kind —
      // so scrolling away, or a message long enough to push the note off
      // screen, left something that reads as sent and was not. `sendFailed` is
      // a field of the row, so no renderer can draw the bubble without it, and
      // the row can carry its own Retry.
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      setTranscripts((t) => ({
        ...t,
        [target]: markSendFailed(t[target] ?? [], local.id, msg),
      }));
      // What Retry needs to send this line again, kept off the message: the
      // wire `mentions` carry targets the rendered chips do not, and an
      // attachment is a node reference rather than the projection on the
      // bubble. Keyed by the optimistic id, which is the id the failed row
      // still has — a throw never reaches the `reconcileIds` above it.
      failedSends.current.set(local.id, {
        target,
        text,
        intent,
        parentId,
        attachments,
        mentions,
      });
      // Ambiguous, not a confirmed non-send — see this function's doc comment.
      return undefined;
    } finally {
      // A detached turn ends when its row settles, not when this POST resolves.
      // Calling `onSendEnd` here would clear the live step timeline and take the
      // working row down while the turn is still going.
      //
      // A *failed* POST must not reach `onSendEnd` either, and that one is
      // easier to get wrong because the send really is over. `onSendEnd` tells
      // the shell the reply is on screen, which lets it drop the live frame it
      // was holding — but a throw rendered nothing, and since #983 the turn
      // carries on regardless, so the frame it holds is the only copy of the
      // answer. Routing the throw here is the drop this whole change removes,
      // put back on the one path the feature exists for.
      //
      // The `"resolved"` case no longer fires `onSendEnd` from here (issue
      // #101 review, PR #2052) — it fires earlier, in the try block, before
      // `append` renders the response's own replies. See that call site's
      // comment for why the order matters. This block still owns `"failed"`.
      if (stateKey && outcome === "failed") onSendFailed?.(stateKey, gen);
      setSending(false);
    }
  }

  /**
   * Set or clear the operator's own reaction on a message (issue #364).
   *
   * Optimistic, then reconciled by failure: the chip flips at once because a
   * reaction that waits for a round trip feels broken, and rolls back if the
   * host refuses. It never guesses — a host with no reactions route says so,
   * once, instead of leaving a chip that will be gone on the next reload.
   */
  async function react(messageId: string, emoji: string) {
    const seq = toHostMessageId(messageId);
    if (!seq) return;
    const before = transcripts[active.id] ?? [];
    const on = !before.some(
      (m) => m.id === messageId && m.reactions?.some((r) => r.emoji === emoji && r.mine),
    );
    const apply = (rows: ChatMessage[]) =>
      rows.map((m) =>
        m.id === messageId
          ? { ...m, reactions: toggleReaction(m.reactions, emoji, "you") }
          : m,
      );
    setTranscripts((t) => ({ ...t, [active.id]: apply(t[active.id] ?? []) }));
    try {
      await client.reactToMessage(seq, emoji, on, company);
    } catch (error) {
      setTranscripts((t) => ({ ...t, [active.id]: apply(t[active.id] ?? []) }));
      toast.error(
        error instanceof ApiError && error.status === 404
          ? "This host doesn't keep reactions yet."
          : error instanceof Error
            ? error.message
            : "Couldn't save that reaction.",
      );
    }
  }

  /**
   * Delete the board card a line opened, and stop drawing its chip
   * (issue #984).
   *
   * #442 allowed a turn to open a card from an ordinary message on the
   * grounds that *"a spurious card can be dismissed in one click"*. That click
   * did not exist here: the chip was a bare link to the card's detail screen,
   * so clearing a mis-fired card meant leaving the channel, finding the card
   * on the board, and deleting it there — which is how the board filled up.
   *
   * NOT optimistic, unlike `react` directly above, and the asymmetry is the
   * point. A reaction that rolls back costs nothing; a chip that vanishes
   * while the card survives tells the operator the board is clean when it is
   * not, which is the very confusion this issue is about. So: delete on the
   * host first, clear the chip only on success, and leave it exactly where it
   * was on a refusal.
   *
   * Clears by CARD id, not by the row clicked, and across every channel rather
   * than the active one — see {@link clearTaskCardEverywhere}. Once the card is
   * gone every chip naming it is a link to a 404, and they can sit on different
   * lines and in different channels.
   */
  async function dismissCard(taskId: string) {
    if (dismissingCardId) return;
    setDismissingCardId(taskId);
    try {
      await deleteTask(client, company, taskId);
      clearCardEverywhere(taskId);
      toast.success("Card dismissed.");
    } catch (error) {
      // A 404 is positive proof the card is gone — most likely deleted from the
      // board itself, where nothing tells the open chat surface about it. The
      // chip is then a permanent link to a 404 that no amount of clicking can
      // remove, so this is a success for the operator's purpose: clear it and
      // say so. Only a refusal we cannot interpret leaves the chip in place.
      if (error instanceof ApiError && error.status === 404) {
        clearCardEverywhere(taskId);
        toast.success("That card was already gone — chip cleared.");
      } else {
        toast.error(
          error instanceof Error && error.message ? error.message : "Couldn't dismiss that card.",
        );
      }
    } finally {
      setDismissingCardId(null);
    }
  }

  /** Drop the card from every channel — see {@link clearTaskCardEverywhere}. */
  function clearCardEverywhere(taskId: string) {
    setTranscripts((t) => clearTaskCardEverywhere(t, taskId));
  }

  /**
   * Settle the in-review dispatch card a finished card's settle pill links to.
   * Approve finishes it; Revise re-runs it with a note — though the console
   * reaches Revise through a thread reply, not this button.
   *
   * The board move is left to the host's own `task_card_changed` over the SSE
   * feed — the same path a drag settles through — so the Approve control drops
   * off the pill the moment the card leaves `in_review`. This only carries the
   * verdict and its busy state; `taskId` is the pill's card, sent so the host
   * settles that specific card rather than whichever one it would otherwise
   * pick for the thread.
   */
  async function reviewCard(taskId: string, decision: "approve" | "revise") {
    if (activeThreadId === undefined || !canSubmitReview(reviewingCardIds, activeThreadId, taskId))
      return;
    setReviewingCardIds((prev) => new Set(prev).add(taskId));
    try {
      await client.reviewCard(activeThreadId, taskId, decision, undefined, company);
      toast.success(decision === "approve" ? "Card approved." : "Sent for another pass.");
    } catch (error) {
      toast.error(
        error instanceof Error && error.message
          ? error.message
          : "Couldn't record that review.",
      );
    } finally {
      setReviewingCardIds((prev) => {
        const next = new Set(prev);
        next.delete(taskId);
        return next;
      });
    }
  }

  /**
   * The Add-Credits CTA (issue #1846): redeems the parked marker and
   * re-dispatches the original message. The redeemed turn's own reply arrives
   * over the SSE feed like any other, so there is nothing to inject here on
   * success — only the busy state and a failure toast.
   *
   * Sends `noticeMessageId`'s cached read from
   * {@link budgetPauseMarkerByNotice} as `?id=` (issue #1846 review, Codex
   * #3868962374, replacing the live-read-at-click-time shape Codex
   * #3866418876/#3866802268 first added): that cache is read back once, at
   * the moment THIS notice became the latest for its agent — see its own doc
   * for why re-reading live in this handler, at CLICK time, defeated the
   * point of sending an id at all. The card can only ever have been rendered
   * from a chat-transcript notice, and the click can lag that render by
   * however long the operator took to notice it; a background turn (a
   * workflow node, an unstreamed task) pausing for the SAME agent in that gap
   * re-parks with no chat destination, which
   * {@link isBudgetPauseNoticeSuperseded} cannot see — a chat-less park never
   * touches the transcript it watches — so a live re-read at click time would
   * silently pick up ITS id and redeem the background turn's message under
   * the operator's own "add credits" intent instead of theirs.
   *
   * Falls back to a live read when the cache has nothing yet for this notice
   * — a click landing faster than the render-time `GET` resolved, or a host
   * that predates {@link OpenCompanyClient.getBudgetPause} entirely — rather
   * than refusing the click outright; a live-at-click read is exactly this
   * function's own pre-fix behaviour, so this degrades to no worse than
   * before rather than to broken.
   *
   * A 404 means the marker is already gone: redeemed from another tab,
   * expired with the process, or already handled. Read as a (delayed) success
   * rather than an error — the operator's intent ("get this moving again") is
   * either already satisfied or nothing this click can fix by retrying.
   *
   * A 409 means the id sent above no longer names the live marker — the
   * server's own atomic check caught what the cached (or live-fallback) read
   * could only narrow the window on, not close outright. Same "nothing this
   * click can fix by retrying blindly" shape as the 404: the operator is told
   * to look again rather than have their click silently redeem the wrong
   * marker.
   */
  async function redeemBudgetPause(agentId: string, noticeMessageId: string) {
    if (redeemingBudgetPauseAgent) return;
    setRedeemingBudgetPauseAgent(agentId);
    try {
      // Only falls through to a live GET when the render-time cache has
      // nothing yet for this notice — see `redeemBudgetPause`'s doc.
      const cached = budgetPauseMarkerByNotice.get(noticeMessageId);
      const live = cached == null ? await client.getBudgetPause(agentId, company) : null;
      const expectedId = budgetPauseRedeemId(noticeMessageId, budgetPauseMarkerByNotice, live?.id);
      await client.redeemBudgetPause(agentId, company, expectedId);
      toast.success("Resending the stalled message.");
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        toast("Nothing to resend — that pause was already handled.");
      } else if (error instanceof ApiError && error.status === 409) {
        toast(
          "That pause has changed since it was shown — check the latest message and try again.",
        );
      } else {
        toast.error(
          error instanceof Error && error.message
            ? error.message
            : "Couldn't resend — try again in a moment.",
        );
      }
    } finally {
      setRedeemingBudgetPauseAgent(null);
    }
  }


  /**
   * Persist a new teammate through the host (issue #360's Team-page add path),
   * falling back to a local-only add for a host without the write plane yet —
   * the same 404 fallback `boot` uses for the roster read itself.
   */
  /**
   * Writes the teammate and answers whether the write landed (issue #1989).
   *
   * The boolean is what lets the dialog keep the operator's sentence and the
   * design the host was paid for when this fails — it used to be called
   * fire-and-forget and the dialog cleared itself regardless.
   */
  async function addMember(fields: NewMemberFields): Promise<boolean> {
    let created: TeamMemberDto | null = null;
    try {
      created = await client.addTeamMember(
        {
          name: fields.name,
          role: fields.role,
          description: fields.description || undefined,
          // Issue #1989: the reduced dialog arrives with a persona the host
          // designed alongside the role and the mandate, so the teammate is
          // born complete. Omitted by the full form, which collects none — an
          // absent key leaves the blueprint's own wording in force.
          instructions: fields.instructions?.trim() || undefined,
        },
        company,
      );
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        // No team write plane on this host — keep the add local-only.
        setMembers((m) => [...m, newMember(fields)]);
      } else {
        reportAddMember(addMemberFailure(error));
        // The dialog keeps what it holds: this is the transient case, and a
        // retry must not cost a second design pass.
        return false;
      }
    }
    let outcome: AddMemberOutcome;
    if (created) {
      const member = fromDto(created);
      setMembers((m) => [...m, member]);
      // The host directory is re-read so the new teammate can be @-mentioned
      // from the picker immediately, rather than after the next reload.
      void reloadDirectory();
      // A successful host add proves the write plane exists, even for a
      // company that opened on the starter roster (fromHost still false from
      // `boot`) — flip it so this and later actions target the host instead of
      // refusing on a now-stale local-only guard.
      setFromHost(true);
      outcome = { kind: "added", name: fields.name };
    } else {
      outcome = { kind: "console-only", name: fields.name };
    }
    setAddOpen(false);
    reportAddMember(outcome);
    // Issue #1989: the reduced dialog collected a name and a sentence, so the
    // rest of the teammate is still to be written — on their own page, beside
    // the copilot that drafts it. Guarded on `created`, not on the flag alone:
    // the 404 fallback above adds a console-only row with no host id, and there
    // is no detail page for a teammate the host has never heard of.
    if (fields.landOnProfile && created) onOpenAgent?.(created.id, { edit: true });
    return true;
  }

  /**
   * Put an agent already on the roster onto this channel's desk (issue
   * #2224) — not a variant of `addMember`, which creates a brand-new
   * teammate. Dropping one from the roster entirely is a Team-page action;
   * `MembersPane` no longer offers it here. `activeIsDesk` gates
   * `MembersPane`'s own "add existing" affordance, so `active.id` is a real
   * desk id by the time this runs; the check here is defensive, not load
   * bearing.
   *
   * `reloadDirectory` alone does not move the added agent into "In this
   * channel": it only refetches the `@mention` picker's directory.
   * `channelMembers`/`others` come from `inChannel`/`outsideChannel`, which
   * are derived from `desks` — so this also calls `loadDesks`, the same
   * function every desk-membership mutation on the org chart already
   * refetches through after `addDeskMember`/`removeDeskMember`. Confirmed
   * safe to call on a plain revisit, not just a scope change: `loadDesks`'s
   * own comment says it blanks the list only when the client or company
   * changed, never on a revisit — so this does not flash the pane empty.
   */
  async function addExistingMember(agentId: string) {
    if (!activeIsDesk) return;
    // Same rule `send` above follows: if the operator switches company or
    // connection while the POST is in flight, every UI-visible effect of it —
    // refresh or toast — belongs to a scope nobody is looking at anymore, so
    // it is dropped rather than landing on whatever they switched to.
    const scopeAtAdd = { connection: scope.connection, company: scope.company, client };
    const stale = () => {
      const latestScope = scopeRef.current;
      return (
        latestScope !== null &&
        (scopeAtAdd.connection !== latestScope.connection ||
          scopeAtAdd.company !== latestScope.company ||
          scopeAtAdd.client !== latestScope.client)
      );
    };
    try {
      await client.addDeskMember(active.id, agentId, company);
      if (stale()) return;
      void reloadDirectory();
      void loadDesks();
    } catch (error) {
      if (stale()) return;
      if (error instanceof ApiError && error.status === 409) {
        // The one 409 this route answers: already a member. Reached only by
        // a race with another tab or operator — refresh now so the row
        // leaves "Everyone else" immediately rather than on an unrelated
        // reload.
        void loadDesks();
        toast.error("Already on this channel.");
      } else {
        toast.error(error instanceof Error ? error.message : "Couldn't add agent.");
      }
    }
  }

  /**
   * The rail's create affordance (issue #1835) — or `undefined`, which is the
   * rule this codebase follows for a control that would be refused: absent,
   * not disabled. A starter roster (`!fromHost`) has no saved teammates to
   * staff a channel with, and an empty roster has nobody at all.
   */
  const onAddChannel =
    fromHost && members.length > 0 ? () => setChannelCreateOpen(true) : undefined;

  function selectChannel(id: string) {
    onNavigate(id);
    // On a phone the rail is painted inside the sidebar's sheet, which covers
    // the whole screen. Navigating without closing it leaves the operator
    // looking at the channel list they just chose from rather than the
    // transcript they chose — and every other row in this sidebar closes the
    // sheet as it navigates (`SidebarNavigation`), the channel list being one
    // of its sections now (codex P2 review). `dismiss` is `undefined` at every
    // width where the rail is a column beside the transcript, so this is a
    // no-op on desktop rather than a second opinion about layout.
    roomRail.dismiss?.();
  }

  const parent = openThreadId ? messages.find((m) => m.id === openThreadId) : undefined;
  const threadReplies = parent ? repliesInThread(parent, messages) : [];
  // Asked of `buildTimeline`'s own rule rather than re-derived, for the reason
  // the mention map above gives: the panel's count and the channel's chip must
  // not drift about what is already on screen. See `ThreadPanel`'s prop docs.
  const threadInlineReplyIds = parent ? inlineReplyIds(messages) : undefined;
  // Every review surface this thread hangs off, newest first — the thread
  // root itself when opened directly on the pill/relay, or one of its
  // replies when the card that produced them was sent inside an
  // already-open thread. Usually zero or one entry; two when a second card
  // was dispatched into this thread before the first was settled (Codex
  // #3906594069) — the newest still drives the composer's own target and
  // "ready for review" notice below, but every other entry gets its own
  // Approve control so it does not have to wait on the newest one settling.
  const threadReviewAnchors =
    parent !== undefined && taskStatusByTaskId !== undefined
      ? reviewAnchorsForThread(parent, threadReplies, messages, taskStatusByTaskId)
      : [];
  const threadReviewAnchor = threadReviewAnchors[0];
  const threadReviewing = threadReviewAnchor !== undefined;
  const additionalThreadReviewAnchors = threadReviewAnchors.slice(1);

  return (
    <>
      {/* ONE rail, painted in the app sidebar under the Room row.
          `createPortal` moves the node, not the component: every prop below is
          still this view's state, and the dialogs the rail opens still mount
          inside this tree.

          There used to be two of these — an `lg:hidden` one that took over the
          pane below 1024px and a `hidden lg:flex` one beside the transcript —
          because the rail was a second column competing with the app sidebar
          for the viewport (issue #1383). It is not a second column any more, so
          the breakpoint dance goes with it: the sidebar already decides whether
          it is a column, a 3rem rail or a sheet, at exactly one set of
          breakpoints, and the channel list follows it. */}
      {roomRail.element !== null &&
        createPortal(
          <ChannelRail
            sections={sections}
            activeId={channel.id}
            unread={unread ?? {}}
            mentions={mentions}
            onSelect={selectChannel}
            openSections={railOpenSections}
            onToggleSection={toggleRailSection}
            directMessages={directMessageChannels(members)}
            onStartDirectMessage={selectChannel}
            onAddChannel={onAddChannel}
            collapsed={channelsCollapsed}
            onExpand={toggleChannels}
            // Off Room the marked channel is where Room will take you back to,
            // not the page being read — so it stops claiming to be the current
            // page. Without this, `#/finances/wallet` had two nodes answering
            // `aria-current="page"`: this rail's open channel and the section
            // rail's open sub-page.
            currentPage={routeOpen}
            // In the sidebar the rail IS the column: it drops its own width,
            // its own border and its own fill, and lets the sidebar's scroll
            // container handle a long list.
            className="flex w-full overflow-visible border-r-0 bg-transparent"
          />,
          roomRail.element,
        )}

      {/* Everything that belongs to the Room *route*: the transcript, its
          header and the members pane.

          Gated, because this view is mounted on every section now to keep
          the rail above fed (#2130) — and a transcript that is mounted
          without being routed must not paint over the section the operator
          is actually in. The dialogs below are deliberately OUTSIDE this
          gate: their triggers are painted in the sidebar, so they have to
          open from Company and Flows as readily as from Room. */}
      {routeOpen && (
        <div className="flex min-h-0 flex-1">
          <div className="flex min-w-0 flex-1 flex-col">
            <ChatHeader
              channel={channel}
              memberCount={headerCount}
              membersOpen={membersOpen}
              onToggleMembers={() => setMembersOpen((o) => !o)}
              onOpenRail={roomRail.reveal}
              rawAvailable={!!rawAgentId}
              raw={showRaw}
              onToggleRaw={() => setRawRequested(!showRaw)}
            />

            <div className="flex min-h-0 flex-1">
              <div className="flex min-w-0 flex-1 flex-col">
                {unknownChannel && (
                  <p
                    role="status"
                    className="flex shrink-0 items-center gap-1.5 border-b bg-muted/50 px-3 py-1.5 text-xs text-muted-foreground"
                  >
                    <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
                    <span className="min-w-0 truncate">
                      <span className="font-medium text-foreground">#{unknownChannel}</span> isn&apos;t a
                      channel here — showing {channelTitle(active)} instead.
                    </span>
                  </p>
                )}
                {showRaw && rawAgentId ? (
                  <RawTranscript
                    load={rawLoad}
                    rows={rawRows}
                    agentId={rawAgentId}
                    agentName={channelTitle(channel)}
                  />
                ) : (
                <MessageTimeline
                  channel={channel}
                  items={items}
                  episodeTurn={episodeTurn}
                  cognition={cognition}
                  historyPending={historyPending}
                  openThreadId={openThreadId}
                  // An open turn keeps the row up after the POST has resolved, and
                  // puts it back on a console that reloaded mid-turn (#983).
                  // `!openThreadId` used to be here, blanking the channel's row for
                  // every turn whenever any thread was open. `openTurn` now
                  // excludes the open thread's own turn, so the row can stay for
                  // the work that is genuinely the channel's.
                  typing={sending || !!openTurn}
                  queued={!!openTurn?.queued}
                  liveSteps={openThreadId ? undefined : liveSteps}
                  // NOT excluded when a thread is open: these rows render inside
                  // their own message rather than as one strip for the channel, so
                  // there is no ambiguity about which turn they describe — which is
                  // the whole reason `liveSteps` above is withheld.
                  liveStepsByMessage={liveStepsByMessage}
                  // Thread-panel receipts are out of v1 (issue #1934): excluded here
                  // the same way `liveSteps` is when a thread is open.
                  receipt={openThreadId ? undefined : receipt}
                  // Who the host expects to answer, for the leg that has no
                  // receipt to read: a reload keeps the open-turn row and
                  // nothing else, and the row is what carries this.
                  turnAgentId={openTurn?.agentId}
                  agentNames={agentNames}
                  onOpenThread={setOpenThreadId}
                  onReact={react}
                  onDismissCard={(taskId) => void dismissCard(taskId)}
                  dismissingCardId={dismissingCardId}
                  onReviewCard={(taskId, decision) => void reviewCard(taskId, decision)}
                  reviewingCardIds={reviewingCardIds}
                  resolveAttachmentUrl={resolveAttachmentUrl}
                  taskStatusByTaskId={taskStatusByTaskId}
                  onRetrySend={retrySend}
                  onStartBrief={() =>
                    setComposerPrefill((current) => ({
                      text: FIRST_TEAM_BRIEF,
                      revision: (current?.revision ?? 0) + 1,
                    }))
                  }
                  onAddPeople={() => setMembersOpen(true)}
                  now={now}
                  askerNames={askerNames}
                  decidingApprovals={decidingApprovals}
                  failedApprovals={failedApprovals}
                  onDecideApproval={onDecideApproval}
                  onRedeemBudgetPause={(agentId, noticeMessageId) =>
                    void redeemBudgetPause(agentId, noticeMessageId)
                  }
                  redeemingBudgetPauseAgent={redeemingBudgetPauseAgent}
                  latestBudgetPauseMessageIdByAgent={budgetPauseMessageIdByAgent}
                />
                )}
                {budgetProximity && (
                  <p
                    role="status"
                    className="flex shrink-0 items-center gap-1.5 border-t border-status-blocked/30 bg-status-blocked-soft px-3 py-1.5 text-xs text-status-blocked-text"
                  >
                    <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
                    <span className="min-w-0 flex-1">{budgetProximity.message}</span>
                    {onDismissBudgetProximity && (
                      <button
                        type="button"
                        onClick={onDismissBudgetProximity}
                        className="shrink-0 rounded px-1.5 py-0.5 font-medium hover:bg-status-blocked-soft"
                      >
                        Dismiss
                      </button>
                    )}
                  </p>
                )}
                {consoleOnlyMember && (
                  <p
                    role="status"
                    className="flex shrink-0 items-center gap-1.5 border-t bg-muted/50 px-3 py-1.5 text-xs text-muted-foreground"
                  >
                    <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
                    <span className="min-w-0">
                      <span className="font-medium text-foreground">{consoleOnlyMember}</span> only
                      exists in this console — the company has no such agent, so nobody answers
                      here. The transcript is still saved and survives a reload.
                    </span>
                  </p>
                )}
                {readOnly && (
                  <p
                    role="status"
                    className="flex shrink-0 items-center gap-1.5 border-t bg-muted/50 px-3 py-1.5 text-xs text-muted-foreground"
                  >
                    <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
                    <span className="min-w-0">
                      The <span className="font-medium text-foreground">Operator</span> channel is a
                      read-only feed of automation reports and notifications — a scannable “what
                      happened” view. There is nothing to reply to here.
                    </span>
                  </p>
                )}
                <TypingLine names={resolveTypingNames?.(active.id) ?? []} />
                {/* Issues #1734 / #1735, repositioned. Directly above the composer,
                    not above the transcript: what the notice warns about — a reply
                    that comes from the echo brain rather than the agent it appears
                    under — is the consequence of pressing Send, and a caveat at the
                    other end of the page from the control it qualifies is one the
                    operator reads before it means anything and has forgotten by the
                    time it does. It stays OUTSIDE the scroller (a sibling strip,
                    `shrink-0`) like the read-only and budget strips above it, because
                    it is a standing fact about the company rather than a row in the
                    transcript.

                    It sits BELOW `TypingLine`, not above it. Proximity to the
                    composer is the whole reason this strip moved, and a typing line
                    between the two put a row back in the gap in exactly the case
                    where it matters most — mid-conversation, with someone at a
                    keyboard (CodeRabbit review on #1984). `chat-cognition-banner`'s
                    sibling-order test pins this WITH a typing line present, because
                    the order read correct with nobody typing and wrong with someone
                    typing.

                    Kept on a read-only channel, where there is no composer at all.
                    The suppression this replaced argued that nothing can be sent
                    there, so a caveat about what sending produces has nothing left to
                    qualify. But the sentence is not about sending — every state below
                    says the replies in this conversation come from the echo brain
                    rather than the agent they appear under, which is a claim about
                    the messages already on screen. `readOnly` is
                    `Boolean(channel?.system)`, i.e. the `#Operator` feed.

                    Its rows are NOT under a roster agent, and the difference
                    matters (codex review on #2159). `DurableOperatorChannel` journals
                    them under the reserved authors `automation-report` and
                    `owner-fallback-report` (`runtime/channel.rs`), which `senderOf`
                    titleizes into "Automation Report" and "Owner Fallback Report" —
                    author lines naming no person at all. That makes the case for the
                    strip stronger, not weaker: `MessageRow` still marks every one of
                    those rows, because `project` sets `by_person: false` on an
                    `AgentReply` whichever brain produced it, and the marker they get
                    is `EchoPlaceholder` — a non-focusable `<span>` whose entire
                    explanation is a `title`, reaching neither keyboard, touch nor
                    screen reader, and reading "Automation Report did not write this".
                    Without this strip the operator is left with a "Placeholder" pill
                    against a name that is not a person, on a feed that takes no
                    replies, and nothing anywhere saying what did write it.

                    All four states below say "the replies in this conversation", not
                    "the replies below". They said "below" while this strip sat above
                    the transcript, and moving it made that word point at the composer
                    and the keyboard hint instead of at any reply — the copy asserted
                    a position rather than a fact. Direction-free is what keeps the
                    sentence true wherever this strip is put next; do not reintroduce
                    a directional word here.

                    `role="status"` (not `alert`) for the reason
                    `components/ui/alert.tsx` gives — a notice present on mount should
                    not interrupt a screen reader. */}
                {/* The composer and the notice that floats over it.

                    `relative` so the banner below can anchor to this box rather
                    than to the pane: it is `absolute bottom-full`, which puts it
                    immediately above the composer wherever the composer happens
                    to be, with no second number to keep in step. */}
                <div className="relative shrink-0">
                {echoing && (
                  <p
                    role="status"
                    data-testid="chat-cognition-banner"
                    // Hovering over the composer, not stacked above it.
                    //
                    // It was a full-bleed strip in the flow — `border-t`, square
                    // corners, edge to edge — which made it look like a
                    // permanent part of the composer's chrome, so an operator
                    // read it once as furniture and stopped seeing it. It is a
                    // *condition*, and conditions in this console are cards that
                    // sit on top of things.
                    //
                    // `bottom-full mb-2` lifts it clear of the composer's top
                    // edge; `inset-x-3` insets it from both sides so it reads as
                    // an object on the pane rather than another band across it.
                    // It overlaps the last line of the transcript rather than
                    // displacing it — which is the trade, and the right one: the
                    // transcript can be scrolled, and this cannot be missed.
                    //
                    // `pointer-events-none` on the box with `pointer-events-auto`
                    // back on the link inside it, so hovering the strip does not
                    // steal a click meant for the message underneath while the
                    // one thing here that IS clickable still works.
                    className="pointer-events-none absolute inset-x-3 bottom-full z-10 mb-2 flex items-start gap-1.5 rounded-lg border border-chrome-border bg-popover px-3 py-2 text-xs text-muted-foreground shadow-md [&_a]:pointer-events-auto"
                  >
                    <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
                    <span className="min-w-0">
                      {cognition === "unconfigured" && (
                        <>
                          <span className="font-medium text-foreground">
                            Agents can&apos;t think yet.
                          </span>{" "}
                          This company has no model configured, so the replies in this
                          conversation come from the offline echo brain rather than the agent
                          they appear under. Choose a provider in{" "}
                          <a
                            className="font-medium text-foreground transition-opacity hover:opacity-80"
                            href={connectionsHref("inference")}
                          >
                            Connections → Inference
                          </a>
                          .
                        </>
                      )}
                      {/* A provider is configured and resolves; the runtime just
                          predates it. Saying "no model configured" here sends an
                          operator who did exactly the right thing back to redo it,
                          which is why this is its own state. The link goes to the
                          card that owns the restart — and stops there, because
                          whether a restart can be performed in place is that card's
                          fact to report (#1736), not a promise to make from here. */}
                      {cognition === "restart-required" && (
                        <>
                          <span className="font-medium text-foreground">
                            Agents can&apos;t think yet — the model isn&apos;t live.
                          </span>{" "}
                          A provider is configured, but this company&apos;s runtime was built before
                          it was saved, so the replies in this conversation still come from the
                          offline echo brain rather than the agent they appear under. Finish
                          the switch in{" "}
                          <a
                            className="font-medium text-foreground transition-opacity hover:opacity-80"
                            href={connectionsHref("inference")}
                          >
                            Connections → Inference
                          </a>
                          .
                        </>
                      )}
                      {cognition === "unavailable" && (
                        <>
                          <span className="font-medium text-foreground">
                            This host cannot reach a model — no agent harness is available.
                          </span>{" "}
                          The replies in this conversation come from the offline echo brain
                          rather than the agent they appear under. No setting changes that:
                          it takes a host built and started with the harness.
                        </>
                      )}
                      {/* The host is on the echo brain and cannot say why: it could
                          not read this company's inference configuration. Names no
                          remedy on purpose — an unreadable config is no evidence
                          that saving one would help, which is the same #266
                          doctrine that stops the automation-run route answering
                          `inference_required` in this state. A settings link here
                          would be the switch that does nothing, one more time. */}
                      {cognition === "undetermined" && (
                        <>
                          <span className="font-medium text-foreground">
                            Agents can&apos;t think, and this host can&apos;t say why.
                          </span>{" "}
                          Its inference configuration could not be read, so the replies in this
                          conversation come from the offline echo brain rather than the agent
                          they appear under. Until the host can read that configuration, saving a
                          provider is not known to help.
                        </>
                      )}
                    </span>
                  </p>
                )}
                {/* No composer at all on a read-only channel, rather than a disabled
                    one. A disabled control is still a claim that the action exists:
                    the strip above says "there is nothing to reply to here", and a
                    greyed-out reply box with a Send button and an "Enter to send"
                    hint under it says the opposite in the same breath. The notice is
                    what should occupy this space.

                    `disabled` therefore no longer carries `readOnly` — nothing can be
                    read-only and rendered here at the same time. The server's
                    read-only guard and `ThreadPanel`'s no-op `onSend` (issue #1757)
                    are untouched: this removes the affordance, not the belt.

                    `suppressed`, not `{!readOnly && …}`. The element stays in the
                    tree so React keeps the instance — and with it the draft, the
                    staged attachment, the resolved mentions and the selected intent,
                    all of which are state inside `MessageComposer`. Gating the
                    element itself unmounted it, so an operator who opened `#Operator`
                    for a moment with an unsent message in `#general` came back to an
                    empty box (codex review on PR #1984): the disabled composer this
                    PR removed was accidentally holding the draft across channel
                    navigation. `suppressed` renders `null` after its hooks, so the
                    DOM gets nothing — no textarea, no Send, no `data-tour` anchor —
                    while the draft survives. See that prop's doc for why a
                    `display:none` wrapper is not the same thing. */}
                {/* Above the composer, and outside the read-only branch: a channel
                    nobody may post in is still a place the company's runs are
                    visible, and stopping one is not posting. */}
                {inflightRuns !== undefined && onInflightSteered !== undefined && (
                  <InflightRunBar
                    client={client}
                    company={company}
                    runs={inflightRuns}
                    onSteered={onInflightSteered}
                  />
                )}
                <MessageComposer
                  // Passed straight through from `AppShell` — see the prop's
                  // note on `MessageComposer`. This view learns nothing about
                  // policy; it only knows where the control goes.
                  autonomy={autonomy}
                  suppressed={readOnly}
                  placeholder={`Message ${channelTitle(channel)}`}
                  disabled={sending}
                  prefill={composerPrefill ?? undefined}
                  // Not voided (unlike the thread composer below): the composer
                  // awaits this to know whether an attachment it carried actually
                  // journaled, so it can clean up one that did not (codex review
                  // finding on #1682) — see `deleteAttachment` and `send`'s doc.
                  onSend={(text, intent, attachments, mentions) =>
                    send(text, intent, undefined, attachments, mentions)
                  }
                  // Issue #1682: only the channel/DM composer attaches — the paperclip
                  // is present exactly because this prop is.
                  uploadAttachment={uploadAttachment}
                  // Cleans up a staged upload that never got sent (codex review
                  // finding on #1682) — see `deleteAttachment`.
                  deleteAttachment={deleteAttachment}
                  // Every keystroke asks; the hook throttles to one ping per
                  // channel per few seconds and skips entirely while the event
                  // stream is down.
                  onTyping={() => onTyping?.(active.id)}
                  // Channel *and* DM composers offer "just chatting" / "do it once" /
                  // "build me the workflow" (issues #580, #845, #1152) — see
                  // `offersDeliverableChoice`, which owns the rule and is unchanged:
                  // the new position inherits the same channel+DM gating. Only the
                  // thread and copilot composers below go without.
                  deliverableChoice={offersDeliverableChoice(active.kind)}
                  mentionables={mentionables}
                  channelMemberIds={inChannel?.map((m) => m.id)}
                />
                </div>
              </div>

              {parent && (
                <ThreadPanel
                  channel={channel}
                  members={members}
                  parent={parent}
                  replies={threadReplies}
                  inlineReplyIds={threadInlineReplyIds}
                  // A query typed into this panel renders only here — parented
                  // messages never reach the channel timeline — so the panel needs
                  // the per-query rows too, or its turns show nothing at all.
                  liveStepsByMessage={liveStepsByMessage}
                  sending={sending}
                  mentionables={mentionables}
                  channelMemberIds={inChannel?.map((m) => m.id)}
                  readOnly={readOnly}
                  reviewing={threadReviewing}
                  reviewTaskId={threadReviewAnchor?.taskId}
                  onReviewCard={(taskId, decision) => void reviewCard(taskId, decision)}
                  reviewInFlight={
                    threadReviewAnchor !== undefined &&
                    reviewingCardIds.has(threadReviewAnchor.taskId)
                  }
                  additionalReviewAnchors={additionalThreadReviewAnchors}
                  reviewingTaskId={reviewingCardIds}
                  youAvatar={youAvatar}
                  resolveAttachmentUrl={resolveAttachmentUrl}
                  onSend={(text, _intent, _attachments, mentions) => {
                    // Belt to `ThreadPanel`'s own `readOnly` brace: never mutate
                    // state or call `client.chat` for a channel the server's
                    // read-only guard will refuse anyway (issue #1757).
                    if (readOnly) return;
                    void send(text, undefined, threadReviewAnchor?.anchorId ?? parent.id, undefined, mentions);
                  }}
                  onClose={() => setOpenThreadId(null)}
                  typingNames={resolveTypingNames?.(active.id, parent.id) ?? []}
                  openTurn={threadTurn}
                  onTyping={() => onTyping?.(active.id, parent.id)}
                  onRetrySend={retrySend}
                  // A thread is not a lesser transcript (issue #1734): an echoed
                  // reply read here is the same false attribution as one read in
                  // the channel, so the panel marks its rows from the same state.
                  cognition={cognition}
                  onRedeemBudgetPause={(agentId, noticeMessageId) =>
                    void redeemBudgetPause(agentId, noticeMessageId)
                  }
                  redeemingBudgetPauseAgent={redeemingBudgetPauseAgent}
                  latestBudgetPauseMessageIdByAgent={budgetPauseMessageIdByAgent}
                />
              )}

              {membersOpen && !readOnly && (
                <MembersPane
                  channelMembers={inChannel}
                  others={outsideChannel}
                  people={companyPeople}
                  presence={presence}
                  leadId={
                    // An `auto` channel has no lead (issue #1835): its memberIds
                    // are the channel's membership in the host's order, not a
                    // hierarchy, so badging [0] would state a rank nothing
                    // confers — the host's own `desk_lead` is `None` for it.
                    activeIsDesk && !active.leadless ? active.memberIds?.[0] : undefined
                  }
                  loading={loadingTeam}
                  fromHost={fromHost}
                  // `activeIsDesk`, not "`channelMembers` is non-null": a DM
                  // has real (non-null) channel membership too — one row,
                  // itself — and is not a desk. `addDeskMember` has no
                  // meaning there, and the affordance must not appear at all
                  // (absent, never disabled — the rule `onManageDesk` below
                  // already follows for the same reason).
                  onAddExisting={
                    activeIsDesk ? (agentId) => void addExistingMember(agentId) : undefined
                  }
                  onMessage={(m) => selectChannel(dmChannelId(m))}
                  /**
                   * The way from this channel to the desk it is (issue #485).
                   *
                   * Only for a host-backed desk channel. A DM is not a desk, and a
                   * fallback desk (`lib/desks.ts`) carries no `memberIds` because
                   * the host has no desks surface at all — the chart would have
                   * nothing to open. Both simply get no link rather than one that
                   * lands nowhere.
                   *
                   * A desk's channel id **is** its desk id (`deskFromDto`), so
                   * there is no mapping to keep in step. Written to the hash rather
                   * than routed through a callback, as `ArtifactsTab`'s "Open in
                   * workspace" does: this is a cross-view address, and the shell
                   * only hands chat a chat-scoped navigate.
                   */
                  onManageDesk={
                    activeIsDesk && active.memberIds
                      ? () => {
                          window.location.hash = `/company/${active.id}`;
                        }
                      : undefined
                  }
                />
              )}
            </div>
          </div>
        </div>
      )}

      <AddMemberDialog
        open={addOpen}
        onOpenChange={setAddOpen}
        onAdd={addMember}
        client={client}
        company={company}
      />
      <ChannelCreateDialog
        client={client}
        company={company}
        members={members}
        open={channelCreateOpen}
        onOpenChange={setChannelCreateOpen}
        onCreated={(dto) => {
          // Fold the new channel into the rail and land the operator in it —
          // the same deskFromDto every fetched desk goes through, so a
          // just-created channel is indistinguishable from a reloaded one.
          //
          // REPLACING the fallback set, not appending to it, when the rail was
          // showing `defaultDesks()`: the company's first real channel is the
          // event that ends the fallback's mandate, and appending beside it
          // would keep fabricated rows in the rail — one of which could share
          // the new channel's very id — until a reload (codex on #1872).
          const desk = deskFromDto(dto);
          setDesks((prev) => (desksAreFallback.current ? [desk] : [...(prev ?? []), desk]));
          desksAreFallback.current = false;
          selectChannel(desk.id);
        }}
      />
    </>
  );
}

/**
 * The pane while `/desks` is still out.
 *
 * Shaped like the workspace it is about to become — a header bar and message
 * rows — so the real channel does not arrive as a jump. It exists to say "not
 * yet", which is the one thing the old blank pane could not distinguish from
 * "never".
 */
function LoadingPane() {
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-13 shrink-0 items-center gap-2 border-b px-3">
        <Skeleton className="size-4 rounded" />
        <Skeleton className="h-4 w-32 rounded" />
      </div>
      <div className="flex-1 space-y-3 p-4">
        {Array.from({ length: 5 }).map((_, i) => (
          <Skeleton key={i} className="h-12 rounded-lg" />
        ))}
      </div>
      <span className="sr-only">Loading channels…</span>
    </div>
  );
}

/** A whole-pane state: a headline, a sentence, and at most one thing to do. */
function EmptyPane({
  title,
  body,
  action,
  after,
}: {
  title: string;
  body: string;
  action?: { label: string; onClick: () => void };
  /** Rendered alongside — a dialog the action opens, which needs to mount. */
  after?: ReactNode;
}) {
  return (
    <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 p-8 text-center">
      <div className="max-w-sm space-y-1.5">
        <h2 className="text-base font-semibold tracking-tight">{title}</h2>
        <p className="text-sm text-muted-foreground">{body}</p>
      </div>
      {action && (
        <Button variant="outline" size="sm" onClick={action.onClick}>
          {action.label}
        </Button>
      )}
      {after}
    </div>
  );
}

/** How many of a teammate's turns one read of its session brings back. */
const RAW_TURN_PAGE = 200;

type RawLoad = "loading" | "ready" | "unsupported" | "error";

/**
 * Whether a session row belongs to the DM with `agentId`.
 *
 * Both spellings, because the host lists both: `chat_history::agent_channels`
 * registers a teammate's DM under its **bare** id (what `dmThreadId` posts to,
 * after issue #364 re-keyed DMs) *and* under `dm:<id>` (the console's channel
 * key and a documented route key). Matching one would silently drop every line
 * keyed the other way — including, depending on which wrote it, the whole of
 * the operator's own side of the conversation.
 */
function inDmWith(row: AgentSessionMessageDto, agentId: string): boolean {
  return (
    row.sessionChannelId === agentId || row.sessionChannelId === `dm:${agentId}`
  );
}

/**
 * How many merged-channel pages one DM's raw-turns read will walk before
 * giving up on filling {@link RAW_TURN_PAGE}. Bounds the read the same way
 * `SESSION_SCAN_LIMIT` bounds the host's own delta walk — a DM that has gone
 * quiet for a very long time gets whatever history it can find within a few
 * pages, not an unbounded fetch loop.
 */
const RAW_TURN_PAGE_WALK_LIMIT = 10;

/**
 * This DM's own raw turns, walked back page by page until there are
 * {@link RAW_TURN_PAGE} of them or the host's history runs out.
 *
 * `GET .../session` answers the agent's **merged, cross-channel** stream,
 * capped at `limit` — so one page of it can be entirely some other desk's
 * traffic while this DM sits just past the cut (Codex P2: 200 newer rows on
 * `#general` make an older, non-empty DM read as empty). Filtering one page
 * for `agentId` is therefore not enough; this pages backward with `before`,
 * the same cursor `chat/history` pagination already uses, collecting this
 * channel's rows until the window is full or a short page says the host has
 * no more history to give.
 */
async function fetchDmRawTurns(
  client: OpenCompanyClient,
  agentId: string,
  company: string | null | undefined,
): Promise<AgentSessionMessageDto[]> {
  const collected: AgentSessionMessageDto[] = [];
  let before: string | undefined;
  for (let page = 0; page < RAW_TURN_PAGE_WALK_LIMIT; page += 1) {
    const rows = await client.agentSession(agentId, company, {
      limit: RAW_TURN_PAGE,
      before,
    });
    // Oldest-first, same order the route answers in: an earlier page's rows
    // belong in front of what is already collected, not behind it.
    collected.unshift(...rows.filter((row) => inDmWith(row, agentId)));
    if (rows.length < RAW_TURN_PAGE || collected.length >= RAW_TURN_PAGE) break;
    const oldest = rows[0]?.id;
    if (!oldest || oldest === before) break;
    before = oldest;
  }
  return collected.length > RAW_TURN_PAGE
    ? collected.slice(collected.length - RAW_TURN_PAGE)
    : collected;
}

/**
 * The transcript, as the turns the teammate actually took.
 *
 * Scoped to **this conversation**, not to the teammate's whole session. A
 * toggle changes how the thing in front of you is drawn; it must not quietly
 * change what the thing is, and flipping a DM into a stream that also carries
 * `#general` would do exactly that. The cross-channel view has its own address
 * — the Session tab on the teammate's page — and says so below.
 */
function RawTranscript({
  load,
  rows,
  agentId,
  agentName,
}: {
  load: RawLoad;
  rows: AgentSessionMessageDto[];
  agentId: string;
  agentName: string;
}) {
  return (
    <div className="min-h-0 flex-1 overflow-y-auto px-6 py-4">
      {load === "loading" && (
        <p className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="size-4 animate-spin" aria-hidden />
          Reading {agentName}&apos;s turns…
        </p>
      )}
      {load === "unsupported" && (
        <p className="text-sm text-muted-foreground">
          This host does not keep a per-agent session yet, so there are no turns
          to show. The conversation itself is unaffected — switch back to Chat.
        </p>
      )}
      {load === "error" && (
        <p className="text-sm text-muted-foreground">
          {agentName}&apos;s turns could not be read. They are still there — this
          is a failed request, not an empty history.
        </p>
      )}
      {load === "ready" && rows.length === 0 && (
        <p className="flex items-center gap-2 text-sm text-muted-foreground">
          <MessageSquare className="size-4 shrink-0" aria-hidden />
          Nothing has been said in this conversation yet.
        </p>
      )}
      {load === "ready" && rows.length > 0 && (
        <>
          {/* No channel badges: every row here is this one DM, and a badge
              repeating the same word down the page is noise. The whole-session
              view turns them on, because there they are the only thing telling
              two desks apart. */}
          <RawTurns rows={rows} agentId={agentId} />
          <p className="mt-4 text-xs text-muted-foreground">
            These are {agentName}&apos;s turns in this conversation.{" "}
            <a
              className="underline underline-offset-4"
              href={`#/company/agent/${encodeURIComponent(agentId)}?tab=session&raw`}
            >
              Everything it has said and heard
            </a>{" "}
            spans every channel it can read.
          </p>
        </>
      )}
    </div>
  );
}
