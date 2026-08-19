import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  FolderClosed,
  LayoutDashboard,
  type LucideIcon,
  MessagesSquare,
  Network,
  Settings2,
  ShieldCheck,
  BookText,
  SquareKanban,
  Workflow,
} from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import {
  ApiError,
  type ApprovalSummary,
  type CompanyStatus,
  type GrantScope,
  type TurnStep,
  type Verdict,
} from "@/api/types";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuBadge,
  SidebarMenuDot,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarRail,
  SidebarTrigger,
} from "@/components/ui/sidebar";
import { FeedbackDialog } from "@/components/feedback-dialog";
import {
  AutoCollapse,
  RESTING_ROW,
  SidebarCollapseToggle,
  SidebarControls,
} from "@/components/sidebar-controls";
import { TourController } from "@/tour/TourController";
import { useCompany } from "@/hooks/use-company";
import { type AgentReplyEvent, type CompanyStreamEvent, useEvents } from "@/hooks/use-events";
import type { WorkspaceEvent } from "@/views/WorkspaceView";
import { useHashView } from "@/hooks/use-hash-view";
import { toast } from "sonner";

import {
  type ChatMessage,
  dispatchMarkerPlacement,
  fromHistory,
  hostMessageId,
  liveReplyIdentity,
  makeMessage,
} from "@/lib/chat";
import { CONNECTION_PROVIDERS } from "@/lib/connections";
import { defaultDesks, type Desk } from "@/lib/desks";
import { mergeReadFloors, unreadCount } from "@/lib/unread";
import { approvedLine } from "@/lib/approval-wording";
import { writeLastChannel } from "@/lib/last-channel";
import { fromDto, type TeamMember } from "@/lib/team";
import { agentDmThreads, defaultThreads, threadsFromDesks } from "@/lib/threads";
import { Overview } from "@/views/Overview";
import { OrgChartView } from "@/views/company/OrgChartView";
import { ChatView } from "@/views/ChatView";
import {
  channelIdForThread,
  deskFromDto,
  dmChannelId,
  HISTORY_UNSTARTED,
  type DecidedApproval,
  type HistoryHydration,
  type HistoryStatus,
  type Transcripts,
} from "@/views/chat/model";
import { Conversation } from "@/views/Conversation";
import { TeamView } from "@/views/TeamView";
import { ApprovalsView } from "@/views/ApprovalsView";
import { LedgersView } from "@/views/LedgersView";
import { TasksView } from "@/views/TasksView";
import { InboxView } from "@/views/InboxView";
import { MemoryView } from "@/views/MemoryView";
import { FeedbackView } from "@/views/FeedbackView";
import { SettingsSection } from "@/views/SettingsSection";
import { useLocalScope } from "@/connections/ConnectionContext";

// React Flow is heavy and only used here — load it on demand.
const WorkflowsView = lazy(() =>
  import("@/views/WorkflowsView").then((m) => ({ default: m.WorkflowsView })),
);
// Pulls in the markdown renderer — load on demand.
const WorkspaceView = lazy(() =>
  import("@/views/WorkspaceView").then((m) => ({ default: m.WorkspaceView })),
);
// Recharts-backed — load on demand.
const FinancesView = lazy(() =>
  import("@/views/FinancesView").then((m) => ({ default: m.FinancesView })),
);

export type View =
  | "overview"
  | "company"
  | "chat"
  | "conversation"
  | "inbox"
  | "tasks"
  | "ledgers"
  | "team"
  | "workspace"
  | "memory"
  | "approvals"
  | "workflows"
  | "finances"
  | "settings"
  | "feedback";

interface NavItem {
  view: View;
  label: string;
  icon: LucideIcon;
}

// One flat list. The nav was grouped under "Operate" and "Configure" when the
// second group held five entries; now that configuration is a section of its
// own, a heading over two rows labelled more than it sorted.
const NAV: NavItem[] = [
  { view: "overview", label: "Overview", icon: LayoutDashboard },
  // Issue #311: the company's structure, and the only way in to desk
  // creation and membership since #302 unmounted the flat Desks page.
  { view: "company", label: "Company", icon: Network },
  { view: "chat", label: "Chat", icon: MessagesSquare },
  // The board, restored and now driven by the `tasks` ledger: its columns,
  // their order and their labels are the host's declaration, and the cards are
  // the task records. It keeps its own entry because it is the surface an
  // operator lives in, and because a card carries far more than a ledger row.
  { view: "tasks", label: "Tasks", icon: SquareKanban },
  // Everything else the company records — goals, decisions, and whatever axis
  // this workspace declared. The board appears here too, as the `tasks`
  // ledger, so this screen is the whole record rather than most of it; the
  // two render through the same component.
  { view: "ledgers", label: "Ledgers", icon: BookText },
  { view: "workspace", label: "Workspace", icon: FolderClosed },
  { view: "approvals", label: "Approvals", icon: ShieldCheck },
  { view: "workflows", label: "Workflows", icon: Workflow },
  { view: "settings", label: "Settings", icon: Settings2 },
];

/**
 * Routable without a nav entry — reachable by URL, absent from the sidebar.
 *
 * Feedback is linked from the sidebar footer instead. The rest are parked
 * rather than retired (issue #302 for Inbox, Brain and Finances; the chat
 * rebuild for Conversation and Team): their host routes, stores and e2e specs
 * are untouched, and re-listing one in `NAV` above is all it takes to bring it
 * back. Conversation and Team are the surfaces the Chat workspace replaces —
 * everything they can do it can do in one screen, including the teammate
 * budget controls `MembersPane` ported from Team (issue #360) — but they
 * keep answering `#/conversation` and `#/team` until the chat covers the
 * last of what they still do better (a desk's persisted transcript).
 */
const HIDDEN_VIEWS: View[] = [
  "feedback",
  "inbox",
  "memory",
  "finances",
  "conversation",
  "team",
];

const VIEWS: View[] = [...NAV.map((i) => i.view), ...HIDDEN_VIEWS];

/** How many workflow run-progress frames (issue #371) the shell keeps for the
 * Workflows canvas. A run emits roughly one per node, so this holds many runs'
 * worth — it exists to bound a long-lived tab, not to ration frames. */
const WORKFLOW_EVENT_WINDOW = 300;

/**
 * Operator-facing copy for a `connect_error` code from the host's OAuth
 * callback (issue #300). The host sends a stable code rather than the
 * provider's own error text — that text is attacker-influenced and may carry
 * credential material, so it never leaves the host's logs.
 *
 * Every message says what to do next: the whole point of the bounce-back is
 * that a failed handshake is recoverable, not a dead end. An unrecognized code
 * (an older console against a newer host) still gets a usable message.
 */
function connectErrorMessage(code: string, provider: string | null): string {
  const name = provider ?? "the provider";
  switch (code) {
    case "denied":
      return `${provider ?? "That"} connection was cancelled. You can try again whenever you're ready.`;
    case "invalid_state":
      return `That ${name} connection link expired. Start the connection again.`;
    case "invalid_request":
      return `That ${name} connection came back incomplete. Start the connection again.`;
    case "unknown_company":
      return `That connection didn't match this company. Start the connection again.`;
    case "provider_disabled":
      return `${provider ?? "That provider"} isn't configured on this host yet.`;
    case "exchange_failed":
      return `Couldn't finish connecting ${name}. Try again in a moment.`;
    case "store_failed":
      return `Connected to ${name}, but saving the credentials failed. Try again.`;
    default:
      return `Couldn't connect ${name}. Try again.`;
  }
}

/**
 * Every host thread id this company can be addressed on, mapped to the chat
 * channel that renders it.
 *
 * The shell needs this the moment anything arrives that it did not send: an SSE
 * frame names a *thread*, `transcripts` is keyed by *channel*, and only the
 * desk list plus the roster can bridge the two. Built once per company beside
 * the transcript hydration that already resolves the same pairing.
 */
function channelMap(desks: Desk[], members: TeamMember[]): Record<string, string> {
  const map: Record<string, string> = {};
  for (const threadId of [...desks.map((d) => d.id), ...members.map((m) => m.id)]) {
    const channelId = channelIdForThread(threadId, desks, members);
    if (channelId) map[threadId] = channelId;
  }
  return map;
}

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  initialStatus: CompanyStatus;
  companies: CompanyStatus[];
  onSwitchCompany: (id: string) => void;
  onBackToPicker?: () => void;
}

/** The dashboard shell: sidebar nav + topbar around one company's views. */
export function AppShell({
  client,
  company,
  initialStatus,
  companies,
  onSwitchCompany,
  onBackToPicker,
}: Props) {
  // Which (connection, company) this subtree's browser-local state belongs to.
  const scope = useLocalScope();
  const [view, sub, navigate] = useHashView<View>(VIEWS, "overview");
  // Track the latest non-default segment per view so returning to a tab with
  // sub-pages restores operator context (for example `#/workflows/<id>`), instead
  // of always dropping it to the parent view.
  // Partial by construction: a view is only present here once it has been
  // visited, and an unvisited view must read back as "nothing remembered"
  // rather than as a key holding `undefined`.
  const lastSubByViewRef = useRef<Partial<Record<View, string | null>>>({});
  const rememberedScopeRef = useRef({
    connection: scope.connection,
    company: scope.company,
  });
  useEffect(() => {
    const scopeChanged =
      rememberedScopeRef.current.connection !== scope.connection ||
      rememberedScopeRef.current.company !== scope.company;
    rememberedScopeRef.current = {
      connection: scope.connection,
      company: scope.company,
    };

    // A selected workflow or thread belongs to this company. Clear it before
    // recording the current route, so an in-place scope change cannot restore
    // a selection from the company being left.
    if (scopeChanged) {
      lastSubByViewRef.current = {};
      if (sub) navigate(view);
      return;
    }

    lastSubByViewRef.current = {
      ...lastSubByViewRef.current,
      [view]: sub,
    };
  }, [scope.connection, scope.company, view, sub, navigate]);
  // Most call sites only ever change the top-level view. Preserve the remembered
  // sub-segment for the target view so tab switches do not discard deep tab state.
  const setView = useCallback(
    (next: View, nextSub?: string) => {
      if (nextSub !== undefined) {
        lastSubByViewRef.current[next] = nextSub;
        navigate(next, nextSub);
        return;
      }
      const remembered = lastSubByViewRef.current[next];
      if (remembered) {
        navigate(next, remembered);
        return;
      }
      navigate(next);
    },
    [navigate],
  );
  const [feedbackOpen, setFeedbackOpen] = useState(false);
  // The shell owns every channel's transcript, not `ChatView` — the shell
  // mounts and unmounts `ChatView` per route, so component-local state there
  // would be discarded on every trip away from Chat and back.
  const [transcripts, setTranscripts] = useState<Transcripts>({});
  // How far each channel's history rehydration has got. Kept beside
  // `transcripts` rather than inside it because an empty transcript is a
  // legitimate final answer, and the timeline has to tell that apart from not
  // having asked yet before it prints "this is the start of…" (issue #934).
  const [hydration, setHydration] = useState<HistoryHydration>(HISTORY_UNSTARTED);
  // Host thread id → chat channel id, for every channel this company has.
  // Resolved by the desks/roster effect below, which already works the pairing
  // out to hydrate each channel and used to throw it away — leaving the shell
  // unable to say which channel an incoming event belongs to (issue #367).
  const [chatChannelByThread, setChatChannelByThread] = useState<Record<string, string>>({});
  // This company's first desk channel — the same channel `ChatView` lands on
  // when the hash names none, and so where a line with nowhere else to go is
  // still somewhere the operator will find it.
  const [firstDeskChannelId, setFirstDeskChannelId] = useState<string | null>(null);
  // The chat channel the operator last had on screen. A ref, not state,
  // because it outlives `ChatView`: it is what an unaddressed system line is
  // addressed to after the operator has walked off to Approvals (issue #368).
  const activeChatChannelRef = useRef<string | null>(null);
  // When each channel was last looked at, and the floor for a channel never
  // looked at. Together with `transcripts` these *derive* the unread counts
  // below — nothing increments a counter, so a message that turns out to be a
  // duplicate cannot leave a badge behind for a line that was never added.
  const [lastViewedChannel, setLastViewedChannel] = useState<Record<string, number>>({});
  const [unreadSince, setUnreadSince] = useState(() => Date.now());
  const [threads, setThreads] = useState(defaultThreads);
  const [activeThreadId, setActiveThreadId] = useState("main");
  // A monotonic nonce bumped on every task-lifecycle SSE event, so the
  // company-chat in-flight steer strip (issue #111) and the board itself
  // (issue #464) refetch live.
  //
  // A counter rather than the payload, and that is what makes it safe to share:
  // both consumers re-read their own surface, so two events collapsing into one
  // React batch still means "re-read" — the frame-loss the workflow canvas had
  // to fold an event window to avoid cannot happen to a tick.
  const [taskEventTick, setTaskEventTick] = useState(0);
  // Issue #1015: bumped on every `run_status_changed`, so the task detail screen
  // sees an attempt move rather than waiting up to four seconds for its poll —
  // and sees it at all while the tab is hidden, which the poll deliberately does
  // not do. Its own counter rather than a share of `taskEventTick`: this fires
  // several times per attempt, and folding it in would make the whole board
  // refetch on every transition of every run.
  //
  // A counter, not the payload, for the same reason the tick above is one: the
  // screen re-reads its own detail, so two frames collapsing inside one React
  // batch still mean "re-read".
  const [attemptEventTick, setAttemptEventTick] = useState(0);
  // Issue #327: the latest workspace write, as the Workspace view needs it.
  //
  // The payload-carrying variant of the `taskEventTick` pattern above, and the
  // one place a counter genuinely is not enough: the view always refetches the
  // tree, but what it does to the OPEN note depends on which node moved — leave
  // it alone, refetch it, or close the pane because it was deleted. `tick` rides
  // alongside so two frames naming the same node in one React batch are still
  // two events rather than a state update React coalesces away.
  const [workspaceEvent, setWorkspaceEvent] = useState<WorkspaceEvent | null>(null);
  // A recovery does not name one node, so it cannot reuse `workspaceEvent`'s
  // payload contract. The workspace re-reads its whole canonical tree on this
  // tick, just as the task and workflow surfaces do below.
  const [workspaceRefreshTick, setWorkspaceRefreshTick] = useState(0);
  // Issue #228: bumped on every `workflow_run_finished` so the Workflows view
  // refreshes its run history live. Same shape as `taskEventTick` — a counter,
  // not the payload, so the view owns what it refetches.
  const [workflowRunTick, setWorkflowRunTick] = useState(0);
  // Issue #384: bumped on every `workflow_created` / `workflow_updated` /
  // `workflow_deleted`, and since issue #276 on `workflow_enabled_changed` too,
  // so the Workflows view re-reads its picker while the tab stays open — a graph
  // authored by the orchestrator, by another session or by a machine credential
  // used to be invisible until a reload, and a workflow armed or paused
  // elsewhere used to keep rendering its old switch.
  //
  // A counter rather than the payload, for the same reason `taskEventTick` is:
  // the view re-reads `GET …/workflows`, so two frames collapsing into one
  // React batch still means "re-read". It also covers the delete case without
  // carrying an id — the workflow that went away is precisely the one the
  // refreshed list no longer has.
  const [workflowListTick, setWorkflowListTick] = useState(0);
  // Issue #371: a rolling WINDOW of run-progress frames, not just a nonce.
  //
  // The canvas paints per-node state, so unlike the tick above it needs the
  // payload — a counter cannot say which node of which run just finished. It is
  // a list rather than a "latest event" slot because two frames routinely land
  // inside one React batch (a transform node finishes in under a millisecond),
  // and a single slot silently drops the earlier one. Losing a
  // `workflow_run_started` that way strands every node frame behind it, which
  // is exactly the bug this shape removes rather than narrows.
  //
  // Bounded so a long-lived tab cannot grow it without limit. The cap is orders
  // of magnitude above a run's ~N+2 frames; if it ever did cut a run's start,
  // the view simply shows no live state and the run history still has it.
  const [workflowRunEvents, setWorkflowRunEvents] = useState<CompanyStreamEvent[]>([]);
  // Issue #1010: and emptied when the company changes.
  //
  // The window is the one company-scoped buffer that was never reset. Every
  // fold that reads it matches frames on `workflowId`/`runId` alone — the
  // frames carry no company — and provisioned companies are built from the same
  // manifests, so two of them routinely hold a workflow of the *same id*.
  // Switching company therefore painted the previous company's run onto an
  // identically-named workflow, with a live-looking node and a Cancel button
  // pointed at a run in a company the operator had left.
  //
  // Emptying is right rather than filtering: the frames that matter after a
  // switch are the ones that arrive after it. The new company's own in-flight
  // runs come back through the history seed (issue #863), which is scoped by
  // the request, so nothing is lost by starting from nothing.
  //
  // The updater returns the SAME array when there is nothing to drop, so React
  // bails out rather than re-rendering the whole shell for a no-op — this
  // effect also fires on mount, when the window is empty by construction.
  useEffect(() => {
    setWorkflowRunEvents((prev) => (prev.length === 0 ? prev : []));
  }, [company]);
  // The live tool timeline, per thread, built from the transient `tool_call` /
  // `tool_result` SSE frames while a turn runs (mirrors OpenHuman's live tool
  // rows). Cleared when the turn's final reply — carrying the authoritative
  // folded steps — lands. `toolCallId` is a transient key for the running→done
  // in-place flip; it is structurally a superset of `TurnStep`, so these render
  // through the same `StepTimeline` as the final steps.
  const [liveStepsByThread, setLiveStepsByThread] = useState<
    Record<string, (TurnStep & { toolCallId?: string })[]>
  >({});
  // The threads with a chat POST currently in flight, so the SSE `agent_reply`
  // echo for each is suppressed — the awaited POST reply is the authoritative,
  // steps-bearing copy (fixes the duplicate-bubble race).
  //
  // Live turn frames route by the frame's own `chatId` (the desk thread the
  // backend journals the reply under — plumbed through `TurnStreamCtx` in
  // `src/turn_stream.rs`), NOT a single global ref. So two chats sending
  // concurrently keep their tool timelines separate even when the same desk
  // member answers both. `activeTurnThreadRef` is only a fallback for a frame
  // that arrives without a `chatId` (older host, or a background turn — which is
  // itself gated off in `run_inner`). See PR #125 review.
  const activeTurnThreadRef = useRef<string | null>(null);
  const pendingPostThreadsRef = useRef<Set<string>>(new Set());
  const feed = useCompany(client, company, initialStatus);
  // Issue #379: the inline approval cards' console-local state, owned here
  // rather than in `ChatView` for the same reason `transcripts` is — the shell
  // mounts and unmounts that view per route, and an operator who approves in a
  // channel then steps over to Approvals must not come back to a card that has
  // forgotten what they did.
  //
  // `deciding` is the request in flight, per approval — a map, not a single
  // slot, because deciding one card must not freeze the others (#373's bug, one
  // surface over). `decided` is what has already been witnessed, from either
  // surface, and it keeps the **whole summary** rather than just the verdict:
  // the host drops a resolved approval from the feed at once, so a console
  // holding only a verdict has nothing left to draw and the card blinks out of
  // the thread the instant it is decided.
  const [decidingApprovals, setDecidingApprovals] = useState<ReadonlyMap<string, Verdict>>(
    () => new Map(),
  );
  const [decidedApprovals, setDecidedApprovals] = useState<Record<string, DecidedApproval>>({});
  /**
   * Decisions that did **not** land, per approval id (#842 review).
   *
   * A third map, and it earns its keep because of consolidation. Deciding three
   * cards separately, a failure belongs to the one card just clicked and the
   * toast is beside it. Deciding one card that covers three, a failure on the
   * third leaves two effects authorised and one not — and an item that simply
   * drops back to its pending look reads as "still working", not "this one did
   * not take". The operator clicked once and got two thirds of what they asked
   * for, with nothing on screen saying which third.
   *
   * Cleared when that item is decided again, so a retry starts from a clean
   * state rather than showing the previous attempt's error under a live one.
   */
  const [failedApprovals, setFailedApprovals] = useState<Record<string, string>>({});

  // The sidebar badge, and the rising edge behind the "needs a sign-off" push.
  //
  // Read off the feed rather than fetched here, and reconciled to the queue in
  // `useCompany` (issue #932): this number sits a click away from the Approvals
  // page's own header, and the two are only guaranteed to agree while they come
  // from one response. Counting `feed.approvals` here directly would work today
  // and would put the rule in the surface that happens to show it, instead of
  // in the feed both surfaces read.
  const pending = feed.status.pending_approvals;

  // OAuth connect bounce-back: the host's callback redirects the browser to
  // `…/connections?connected={provider}` after storing the token, or to
  // `…/connections?connect_error={code}[&provider={id}]` when the handshake
  // failed. Land the operator on the Connections page either way, say what
  // happened, then strip the params so a refresh doesn't re-fire them. Runs
  // once; StrictMode's double invoke is harmless because the first run clears
  // the params the second reads.
  //
  // Connections is a page of the Settings section now (`#/settings/connections`),
  // so the bounce-back lands there rather than on a top-level view.
  //
  // The failure half matters as much as the success half: before issue #300 the
  // host answered a cancelled or expired handshake with a JSON body, which the
  // browser rendered as the page — a dead end with no way back into the
  // console. The host now always redirects, so this is where it becomes
  // recoverable.
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const connected = params.get("connected");
    const failed = params.get("connect_error");
    if (!connected && !failed) return;
    // The provider id is advisory — the host omits it on the arms that fire
    // before the signed state is verified.
    const providerId = connected ?? params.get("provider");
    params.delete("connected");
    params.delete("connect_error");
    params.delete("provider");
    const query = params.toString();
    window.history.replaceState(
      {},
      "",
      window.location.pathname + (query ? `?${query}` : "") + window.location.hash,
    );
    setView("settings", "connections");
    // The callback param carries the raw provider id (e.g. "slack"); show the
    // catalog display name ("Slack") when we know it, falling back to the id.
    const providerName = providerId
      ? (CONNECTION_PROVIDERS.find((p) => p.id === providerId)?.name ?? providerId)
      : null;
    if (failed) toast.error(connectErrorMessage(failed, providerName));
    else toast.success(`Connected ${providerName}.`);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Build the chat threads from the company's real desks (issue #53); keep the
  // static defaults when the host doesn't expose `/desks` (404) or defines none.
  // Merges by id so a transcript typed before desks load survives.
  //
  // Once the thread list is known, rehydrate each thread's transcript from the
  // backend's persisted history (issue #65): the server journals every
  // operator message and agent reply to the EventLog, but the console used to
  // always start every thread empty. Merges by message id so a line typed
  // locally before its thread's history lands isn't lost — hydration can race
  // the operator's first message on a fresh page load.
  useEffect(() => {
    let cancelled = false;
    // Another company's channel ids are another namespace. Drop this one's
    // addressing up front rather than routing the next company's events into
    // channels that no longer exist, and start the unread floor again so the
    // incoming company's rehydrated history isn't counted as news.
    setChatChannelByThread({});
    // Another company's channels are another namespace here too, and a status
    // carried over would let the incoming company's channels claim to be
    // settled before anything has asked about them.
    setHydration(HISTORY_UNSTARTED);
    setFirstDeskChannelId(null);
    setLastViewedChannel({});
    setUnreadSince(Date.now());
    activeChatChannelRef.current = null;

    // Then replace that mount-time floor with the one the host remembers for
    // this person (issue #755). Until this lands the browser floor stands, so
    // the first paint is the old behaviour rather than a blank badge; when it
    // lands, channels this person left unread come back unread.
    //
    // Merged into whatever the operator has viewed since, rather than
    // assigned: this request is in flight while the console is usable, and a
    // channel opened in that window must not have its fresh floor overwritten
    // by an older stored one.
    client
      .readState(company)
      .then(({ markers }) => {
        if (cancelled || markers.length === 0) return;
        setLastViewedChannel((viewed) => mergeReadFloors(viewed, markers));
      })
      .catch(() => {
        /* host without `/chat/read-state`, or offline — the browser floor stands */
      });
    // Another company's approval ids are another namespace, and a settled card
    // must not survive the switch as a ghost in the new company's channels.
    setDecidedApprovals({});
    setDecidingApprovals(new Map());
    setFailedApprovals({});

    const hydrate = (threadId: string) => {
      client
        .getChatHistory(threadId, company)
        .then((entries) => {
          if (cancelled || entries.length === 0) return;
          const hydrated = fromHistory(entries);
          setThreads((ts) =>
            ts.map((t) => {
              if (t.id !== threadId) return t;
              const known = new Set(t.messages.map((m) => m.id));
              const fresh = hydrated.filter((m) => !known.has(m.id));
              return fresh.length === 0 ? t : { ...t, messages: [...fresh, ...t.messages] };
            }),
          );
        })
        .catch(() => {
          /* host without `/chat/history`, or offline — thread stays empty */
        });
    };

    // Same rehydration, into `transcripts` instead of `threads` — the Chat
    // workspace's own transcript store. Chat's channel id and the host's
    // thread id agree for a desk (`deskFromDto` keeps `DeskDto.id`
    // untouched), but not for a DM: the channel id is the console-local
    // `dmChannelId`, while the thread id `getChatHistory`/`chat` read is the
    // roster agent id (see `ChatView`'s `send`) — so this takes both.
    const markHistory = (channelId: string, status: HistoryStatus) =>
      setHydration((h) => ({ ...h, byChannel: { ...h.byChannel, [channelId]: status } }));

    const hydrateChannel = (channelId: string, threadId: string) => {
      // Marked before the request, not after: the gap between "this channel
      // exists" and "its history is in flight" is precisely the window the
      // timeline used to fill with the empty-channel copy.
      markHistory(channelId, "loading");
      client
        .getChatHistory(threadId, company)
        .then((entries) => {
          if (cancelled) return;
          if (entries.length === 0) {
            // An empty answer is still an answer, and the only thing that ever
            // makes the "start of your direct message" copy true.
            markHistory(channelId, "ready");
            return;
          }
          const hydrated = fromHistory(entries);
          setTranscripts((t) => {
            const known = new Set((t[channelId] ?? []).map((m) => m.id));
            const fresh = hydrated.filter((m) => !known.has(m.id));
            return fresh.length === 0 ? t : { ...t, [channelId]: [...fresh, ...(t[channelId] ?? [])] };
          });
          markHistory(channelId, "ready");
        })
        .catch(() => {
          /* host without `/chat/history`, or offline — channel stays empty */
          if (!cancelled) markHistory(channelId, "ready");
        });
    };

    client
      .listDesks(company)
      .then(async (desks) => {
        if (cancelled) return;
        // Issue #151 §3.3: desks first, then one DM thread per roster teammate.
        // The roster is fetched separately and tolerated as optional — a host
        // that 404s `/team` keeps its desks rather than losing the whole list.
        const team = await client.listTeam(company).catch(() => []);
        if (cancelled) return;
        const deskThreads = threadsFromDesks(desks);
        const resolved = [
          ...deskThreads,
          ...agentDmThreads(
            team,
            deskThreads.map((t) => t.id),
          ),
        ];
        setThreads((prev) => {
          const byId = new Map(prev.map((t) => [t.id, t]));
          return resolved.map((t) => {
            const existing = byId.get(t.id);
            return existing ? { ...t, messages: existing.messages } : t;
          });
        });
        resolved.forEach((t) => hydrate(t.id));

        const chatDesks = desks.length ? desks.map(deskFromDto) : defaultDesks();
        const roster = team.map(fromDto);
        // Keep the addressing this loop resolves, not just its side effect.
        setChatChannelByThread(channelMap(chatDesks, roster));
        setFirstDeskChannelId(chatDesks[0]?.id ?? null);
        chatDesks.forEach((d) => hydrateChannel(d.id, d.id));
        roster.forEach((m) => hydrateChannel(dmChannelId(m), m.id));
        // Every channel this pass will hydrate now has a status, so a channel
        // with none is one nothing is coming for.
        setHydration((h) => ({ ...h, discovered: true }));
      })
      .catch(() => {
        // Host without `/desks`, or offline — keep the static default
        // threads, but the operator/General line still deserves a
        // rehydration attempt (it's the one every deployment has).
        const fallbackDesks = defaultDesks();
        defaultThreads().forEach((t) => hydrate(t.id));
        setChatChannelByThread(channelMap(fallbackDesks, []));
        setFirstDeskChannelId(fallbackDesks[0]?.id ?? null);
        fallbackDesks.forEach((d) => hydrateChannel(d.id, d.id));
        if (!cancelled) setHydration((h) => ({ ...h, discovered: true }));
      });

    return () => {
      cancelled = true;
    };
  }, [client, company]);

  /**
   * Unread per channel, for the channel rail's badges (issue #367 — the rail
   * has always rendered them, it was handed a hard-coded empty map).
   *
   * Derived from the transcripts rather than counted as messages arrive. A
   * counter would have to be incremented from inside the injection, which only
   * finds out whether it actually appended anything inside a state updater —
   * and an updater that also bumped a second piece of state would be an impure
   * one, which React is free to run twice. Deriving sidesteps that entirely and
   * is self-correcting: whatever is in the channel and newer than the last look
   * at it is unread, by definition.
   *
   * Your own lines never count. Neither does anything older than the floor,
   * which is why a page load's worth of rehydrated history arrives read.
   */
  const unread = useMemo(() => {
    const counts: Record<string, number> = {};
    for (const [channelId, messages] of Object.entries(transcripts)) {
      const since = lastViewedChannel[channelId] ?? unreadSince;
      const count = unreadCount(messages, since);
      if (count > 0) counts[channelId] = count;
    }
    return counts;
  }, [transcripts, lastViewedChannel, unreadSince]);

  /**
   * `ChatView` reporting which channel is on screen — on every switch, and
   * again as the open channel's transcript grows so a line read as it lands
   * doesn't leave a badge behind.
   */
  const onChannelViewed = useCallback(
    (channelId: string) => {
      activeChatChannelRef.current = channelId;
      const at = Date.now();
      setLastViewedChannel((v) => ({ ...v, [channelId]: at }));
      // The durable half (issue #755). Fire-and-forget on purpose: the local
      // floor above has already cleared the badge, so a failed write costs a
      // stale marker on the next load, not a wrong badge now. The host's write
      // is monotonic, so the many calls this makes while a live channel grows
      // are idempotent and cannot move the floor backwards.
      void client.markChannelRead(channelId, at, company).catch(() => {
        /* older host, or offline — the in-browser floor still holds this session */
      });
      // The same fact, persisted, so re-entering Chat returns to the channel
      // the operator was reading instead of whichever sorts first (issue #412).
      // The ref above cannot do it: it dies with this mount, and a reload is
      // exactly one of the trips that has to survive.
      writeLastChannel(scope, channelId);
    },
    [scope, client, company],
  );

  const setThreadMessages = (
    threadId: string,
    updater: (m: ChatMessage[]) => ChatMessage[],
  ) =>
    setThreads((ts) =>
      ts.map((t) => (t.id === threadId ? { ...t, messages: updater(t.messages) } : t)),
    );

  /**
   * Approval decisions and other unaddressed lines land in a transcript rather
   * than vanishing. Both chat surfaces get the line: Chat appends it to a
   * channel, and the parked Conversation to its active thread. The shell owns
   * `transcripts`, not `ChatView`, so the write survives that view unmounting —
   * which it always has, because these lines are written from Approvals.
   *
   * The channel is resolved, not assumed (issue #368). This used to append to
   * the literal `"main"`, which is the id of the first *fallback* desk and of
   * nothing else: a company with its own desks has channel ids taken verbatim
   * from its manifest, so every decision line — the failures included, which is
   * the half that matters — was filed under a key no channel renders.
   *
   * In order: the channel the operator last had open, which survives the walk
   * over to Approvals and is where they will look first; else this company's
   * first desk channel, the same first-match `ChatView` lands on when the hash
   * names none (issue #366); else there is genuinely no channel to write to, so
   * the line stays out of `transcripts` and the toast `ApprovalsView` raises
   * alongside this call is what surfaces the decision. Never a dead bucket.
   *
   * Either way the channel it lands in shows an unread badge until the operator
   * opens it, so the line says where it went rather than waiting to be found.
   */
  const noteSystem = (line: string) => {
    const target = activeChatChannelRef.current ?? firstDeskChannelId;
    if (target) {
      setTranscripts((t) => ({
        ...t,
        [target]: [...(t[target] ?? []), makeMessage("system", line)],
      }));
    }
    setThreadMessages(activeThreadId, (m) => [...m, makeMessage("system", line)]);
  };

  /**
   * A system line into the channel that owns `threadId`, falling back to
   * {@link noteSystem}'s "wherever the operator is" rule when the thread names
   * no channel this company has (issue #379).
   *
   * The addressed form exists because an inline decision has a *known* home: the
   * conversation the card was raised in. Filing it under "the last channel
   * looked at" would put a decline into whatever the operator happened to open
   * next — #368's bug, re-introduced one surface over.
   */
  const noteInChannel = (threadId: string | null | undefined, line: string) => {
    const target = threadId ? chatChannelByThread[threadId] : undefined;
    if (!target) {
      noteSystem(line);
      return;
    }
    setTranscripts((t) => ({
      ...t,
      [target]: [...(t[target] ?? []), makeMessage("system", line)],
    }));
  };

  // Inject an `AgentReply` pushed over the SSE feed (issue #66) into its desk
  // thread's transcript. Dedupe against our own optimistic echo: the backend
  // journals an `AgentReply` for the operator's own chat turn too, and
  // Conversation already rendered that reply locally. Local message ids are
  // ephemeral counters (not content-addressed), so we key the dedupe on an
  // identical company line already present in the thread's recent tail. Only
  // desks that exist as a thread receive an injection; an unmatched chatId is a
  // no-op rather than polluting the wrong thread.
  const injectAgentReply = useCallback(
    (event: AgentReplyEvent) => {
      // The operator's own chat turn is delivered synchronously by the awaited
      // POST (and that copy carries the steps timeline). The backend ALSO journals
      // an `AgentReply` for it, which arrives over SSE — first, mid-await — so a
      // blind inject here would double the bubble. Suppress the echo for any
      // thread with a POST in flight; the POST reply is authoritative. The
      // recent-tail content check below still guards a late echo that lands just
      // after the POST resolved.
      if (pendingPostThreadsRef.current.has(event.chatId)) return;
      setThreads((ts) =>
        ts.map((t) => {
          if (t.id !== event.chatId) return t;
          const dup = t.messages
            .slice(-8)
            .some((m) => m.from === "company" && m.text === event.text);
          if (dup) return t;
          return {
            ...t,
            messages: [
              ...t.messages,
              makeMessage("company", event.text, {
                channel: event.agentId,
                taskId: event.taskId,
                // Issue #483 — see `liveReplyIdentity`.
                ...liveReplyIdentity(event),
              }),
            ],
          };
        }),
      );

      // …and into the Chat workspace's transcripts, which is a *different*
      // store (issue #367). Chat became the nav-listed surface in #361 while
      // this injection kept writing only to the parked Conversation's threads,
      // so anything the console did not POST for — an inbound Telegram turn, a
      // background desk turn — reached Chat only on a page reload.
      //
      // The event names a thread; `chatChannelByThread` is the only thing that
      // knows which channel renders it. An id no channel owns is a no-op, the
      // same as the thread store above: better silent than in the wrong place.
      const channelId = chatChannelByThread[event.chatId];
      if (!channelId) return;
      setTranscripts((t) => {
        const existing = t[channelId] ?? [];
        // The same recent-tail content dedupe the thread store uses. It still
        // earns its place: the operator's own turn is rendered locally by the
        // awaited POST under an ephemeral `m<seq>` id, so a late echo of that
        // reply can only be matched by content.
        //
        // It is no longer the ONLY guard, and issue #483 is why. This line now
        // carries the host's id (below), so `hydrateChannel`'s id dedupe can
        // recognise it — which the content check could never do from the other
        // side, because hydration prepends history rather than appending to the
        // recent tail this scans. Live-then-hydrate was the one route neither
        // guard covered, and it doubled every reply that arrived while its
        // channel was closed.
        const dup = existing
          .slice(-8)
          .some((m) => m.from === "company" && m.text === event.text);
        if (dup) return t;
        return {
          ...t,
          [channelId]: [
            ...existing,
            makeMessage("company", event.text, {
              channel: event.agentId,
              taskId: event.taskId,
              // Issue #483: same identity as the thread store above. This is
              // the store `hydrateChannel` writes into, so this is where the
              // duplicate was visible.
              ...liveReplyIdentity(event),
              // Issue #364: a reply to a thread joins that thread live, instead
              // of appearing in the channel and moving on the next reload. The
              // host names the parent by its own id, so it takes the same
              // namespace prefix a hydrated line does.
              parentId: event.parentId ? hostMessageId(event.parentId) : undefined,
            }),
          ],
        };
      });

      // The reply is the end of that turn, so its live tool rows have served
      // their purpose — the folded steps on the reply are the durable record.
      // `onSendEnd` does this for a turn this console POSTed; a turn it did not
      // has no send to end, and without this its rows would sit under the
      // channel until the next turn on the same thread replaced them.
      setLiveStepsByThread((prev) =>
        prev[event.chatId]?.length ? { ...prev, [event.chatId]: [] } : prev,
      );
    },
    // `useEvents` holds its callbacks in refs, so this identity churning as the
    // map lands cannot re-open the SSE stream.
    [chatChannelByThread],
  );

  /**
   * Post the card-linked system marker for a settled dispatch into the channel
   * the card was raised in (issue #377).
   *
   * The gap it closes: a card dispatched from a channel could park in `paused`
   * or bounce back to `todo`, and the only thing the channel showed was the
   * agent's relay prose — so a reader, live or arriving fresh, reasonably
   * concluded the work had finished. The marker is the structural fact the
   * prose could not carry: the run *stopped*, and here is where the card
   * landed, with a link to it.
   *
   * Every rule about *where* the line goes — a frame with no `chatId` going
   * nowhere, a thread matching no channel going nowhere rather than to whatever
   * channel is open (#368's bug), and the `h<seq>` identity that lets the next
   * reload recognise its own twin (#483/#498) — lives in
   * `dispatchMarkerPlacement`, so each stays assertable. This callback is only
   * the write.
   *
   * Written into **both** stores for the same reason `injectAgentReply` is: the
   * parked Conversation reads `threads`, the Chat workspace reads
   * `transcripts`, and a line written to one alone is invisible on the other
   * until a reload.
   */
  const injectDispatchMarker = useCallback(
    (event: CompanyStreamEvent) => {
      if (event.type !== "desk_task_completed") return;
      const placement = dispatchMarkerPlacement(event, chatChannelByThread);
      if (!placement) return;
      const { threadId, channelId, message } = placement;

      setThreads((ts) =>
        ts.map((t) => {
          if (t.id !== threadId) return t;
          // The same id guard hydration runs. A marker cannot arrive twice off
          // one stream, but a reconnecting `EventSource` can replay a frame,
          // and the id is what makes that harmless.
          if (t.messages.some((m) => m.id === message.id)) return t;
          return { ...t, messages: [...t.messages, message] };
        }),
      );

      if (!channelId) return;
      setTranscripts((t) => {
        const existing = t[channelId] ?? [];
        if (existing.some((m) => m.id === message.id)) return t;
        return { ...t, [channelId]: [...existing, message] };
      });
    },
    // Same reasoning as `injectAgentReply`: `useEvents` holds its callbacks in
    // refs, so this identity churning as the map lands cannot re-open the
    // stream.
    [chatChannelByThread],
  );

  // Mark/unmark a thread's in-flight POST. `onSendStart` also resets its live
  // timeline so a fresh turn starts clean; `onSendEnd` clears it because the
  // final reply now carries the authoritative folded steps.
  const onSendStart = useCallback((threadId: string) => {
    pendingPostThreadsRef.current.add(threadId);
    activeTurnThreadRef.current = threadId;
    setLiveStepsByThread((prev) => ({ ...prev, [threadId]: [] }));
  }, []);
  const onSendEnd = useCallback((threadId: string) => {
    pendingPostThreadsRef.current.delete(threadId);
    if (activeTurnThreadRef.current === threadId) activeTurnThreadRef.current = null;
    setLiveStepsByThread((prev) => {
      if (!prev[threadId]?.length) return prev;
      return { ...prev, [threadId]: [] };
    });
  }, []);

  // Fold one live turn frame into the in-flight thread's timeline: a `tool_call`
  // upserts a `running` row keyed by `toolCallId`; a `tool_result` flips that row
  // to `ok`/`error` in place (FIFO fallback when no id pairs), mirroring
  // OpenHuman's `toolCallReceived` / `toolResultReceived`.
  const onTurnEvent = useCallback((event: CompanyStreamEvent) => {
    // Route by the frame's own thread id so concurrent turns (even from the same
    // desk member) never cross-attribute; fall back to the in-flight ref only
    // when a frame carries no chatId (older host / background turn).
    const threadId =
      ("chatId" in event && event.chatId) || activeTurnThreadRef.current;
    if (!threadId) return; // a background/task turn — not part of a chat.
    setLiveStepsByThread((prev) => {
      const rows = prev[threadId] ? [...prev[threadId]] : [];
      if (event.type === "tool_call") {
        const idx = event.toolCallId
          ? rows.findIndex((r) => r.toolCallId === event.toolCallId)
          : -1;
        const row = {
          kind: "tool_call" as const,
          status: "running" as const,
          label: event.label ?? "Working",
          toolCallId: event.toolCallId,
        };
        if (idx >= 0) rows[idx] = { ...rows[idx], ...row };
        else rows.push(row);
      } else if (event.type === "tool_result") {
        let idx = event.toolCallId
          ? rows.findIndex((r) => r.toolCallId === event.toolCallId)
          : -1;
        if (idx < 0) idx = rows.findIndex((r) => r.status === "running");
        const status = event.status === "error" ? ("error" as const) : ("ok" as const);
        if (idx >= 0) {
          rows[idx] = {
            ...rows[idx],
            status,
            detail: event.detail ?? rows[idx].detail,
            elapsedMs: event.elapsedMs,
          };
        } else {
          rows.push({
            kind: "tool_call",
            status,
            label: event.label ?? "Working",
            detail: event.detail,
            elapsedMs: event.elapsedMs,
            toolCallId: event.toolCallId,
          });
        }
      } else if (event.type === "thinking") {
        // The backend already coalesces a thinking run into one frame, so each
        // arrival is a distinct row (mirrors the folded "Thinking" step).
        rows.push({ kind: "thinking", status: "ok", label: "Thinking" });
      }
      return { ...prev, [threadId]: rows };
    });
  }, []);

  const markDeciding = useCallback((id: string, verdict: Verdict | null) => {
    setDecidingApprovals((prev) => {
      const next = new Map(prev);
      if (verdict) next.set(id, verdict);
      else next.delete(id);
      return next;
    });
  }, []);

  /** Drops a recorded failure — a retry is starting, or the item is gone. */
  const clearFailure = useCallback((id: string) => {
    setFailedApprovals((prev) => {
      if (!(id in prev)) return prev;
      const next = { ...prev };
      delete next[id];
      return next;
    });
  }, []);

  /**
   * Decide an approval from inside the conversation it was raised in (#379).
   *
   * **Detached** (`detach: true`), unlike the Approvals page. The default
   * resolve answers with the follow-up turn's replies, and this card sits in a
   * transcript that is *already* subscribed to the `agent_reply` frame — so
   * rendering the body too would put one continuation into the channel twice.
   * Detach has exactly one delivery path, so the race cannot arise. The page
   * keeps the default shape, because it has no transcript and the body is its
   * only sight of what happened next.
   *
   * The witnessed verdict is recorded before the refresh settles anything, so
   * the card says what the operator chose rather than snapping back to two live
   * buttons. The refresh in `finally` is the reconciliation: the host drops the
   * approval from the queue in its first step, so the queue either loses this
   * card — proving the verdict landed — or keeps it, showing a decision that
   * still needs making.
   *
   * Not memoized: it closes over `feed` and `noteInChannel`, and it is only ever
   * called from an event handler, so a `useCallback` here would buy a stale
   * closure and nothing else.
   */
  const decideApproval = async (
    approval: ApprovalSummary,
    verdict: Verdict,
    scope: GrantScope = { kind: "once" },
  ) => {
    if (decidingApprovals.has(approval.id)) return;
    markDeciding(approval.id, verdict);
    // A retry starts clean: the previous attempt's error must not sit under a
    // live one, or the operator cannot tell which attempt it belongs to.
    clearFailure(approval.id);
    try {
      const answer = await client.resolveApproval(approval.id, verdict, undefined, company, {
        detach: true,
        scope,
      });
      setDecidedApprovals((prev) => ({ ...prev, [approval.id]: { verdict, approval } }));
      toast.success(
        verdict === "approve"
          ? approvedLine(answer.stillAwaiting)
          : "Declined — recorded.",
      );
      // A decline ends the thread's story, and silence would read as a stall.
      // An approve needs no line: the continuation lands as a real reply, which
      // is the whole point of deciding here.
      if (verdict === "deny") {
        noteInChannel(approval.thread, "Declined — the agent will not take that action.");
      }
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      toast.error(`Couldn't record your decision — ${msg}`);
      noteInChannel(approval.thread, `Couldn't record your decision — ${msg}`);
      // On the card as well as in a toast, and keyed to the item that failed.
      // A toast is the wrong and only home for this once one click covers
      // several calls: it says a decision failed without saying *which*, and it
      // is gone by the time the operator looks back at the card. The row that
      // did not take has to say so itself.
      setFailedApprovals((prev) => ({ ...prev, [approval.id]: msg }));
    } finally {
      markDeciding(approval.id, null);
      void feed.refresh();
    }
  };

  // One recovery path for a signalled gap, a healthy connection, and the hosted
  // proxy's failed-to-open case (#23). These surfaces own their data, so every
  // one re-reads rather than attempting to reconstruct lost payloads here.
  const resyncDurableState = useCallback(async () => {
    setTaskEventTick((n) => n + 1);
    setWorkspaceRefreshTick((n) => n + 1);
    setWorkflowRunTick((n) => n + 1);
    setWorkflowListTick((n) => n + 1);
    await feed.refresh();
  }, [feed.refresh]);

  // The active push half of the attention surface: SSE-driven toasts + chat
  // injection, plus a rising-edge "needs a sign-off" toast off the poll's
  // pending count. Degrades silently to the `useCompany` poll when the host has
  // no `/events` route.
  useEvents(client, company, {
    pendingApprovals: pending,
    onAgentReply: injectAgentReply,
    onTaskEvent: useCallback(() => setTaskEventTick((n) => n + 1), []),
    onRunEvent: useCallback(() => setAttemptEventTick((n) => n + 1), []),
    // Issue #377. Beside the board tick above, not instead of it: a settle both
    // moves a card between columns and needs saying in the conversation the
    // card came from.
    onDispatchTerminal: injectDispatchMarker,
    // Issue #327. The payload is carried, not folded into a counter — see
    // `workspaceEvent` above. The view still re-reads the tree from the host
    // rather than patching it from the frame: the frame carries no node name
    // and no body by design.
    onWorkspaceEvent: useCallback((event: CompanyStreamEvent) => {
      if (event.type !== "workspace_changed") return;
      setWorkspaceEvent((prev) => ({
        tick: (prev?.tick ?? 0) + 1,
        nodeId: event.nodeId,
        change: event.change,
      }));
    }, []),
    onTurnEvent,
    onWorkflowRunEvent: useCallback((event: CompanyStreamEvent) => {
      // Both halves. The tick refreshes the durable history; the frames drive
      // the live canvas. Progress frames are far more frequent than outcomes,
      // so only an outcome bumps the tick — refetching history once per node
      // would be N round trips per run for a list that has not changed yet.
      setWorkflowRunEvents((prev) => [...prev, event].slice(-WORKFLOW_EVENT_WINDOW));
      if (event.type === "workflow_run_finished") setWorkflowRunTick((n) => n + 1);
    }, []),
    // Issue #384. The picker is refreshed from the host rather than patched
    // from the frame: the frame carries no graph body by design, and a console
    // that splices what it *thinks* changed is how a picker drifts in the first
    // place.
    onWorkflowChanged: useCallback(() => setWorkflowListTick((n) => n + 1), []),
    // Issue #379. Both frames do the same one thing — re-read the approvals
    // feed — and that is deliberate: the park frame is thin by design (no
    // payload, no asker), so the redacted summary on the feed is the only place
    // a card's content may come from. One round trip, in exchange for one
    // redaction surface instead of two.
    //
    // The resolution half is what settles an inline card decided on the
    // Approvals page, or in another tab, without a reload.
    //
    // Not memoized, for the same reason as `decideApproval`: `useEvents` keeps
    // its callbacks in refs it refreshes every render, so a plain arrow costs no
    // stream re-open and cannot go stale over the refresh it calls.
    onApprovalEvent: (event: CompanyStreamEvent) => {
      if (event.type === "approval_resolved") {
        const verdict: Verdict = event.verdict === "approve" ? "approve" : "deny";
        // Snapshot the summary from the feed as it stands *now* — before the
        // refresh below drops it. An id this console never had a summary for
        // records nothing, which is right: there is no card to settle.
        const approval = feed.approvals.find((a) => a.id === event.approvalId);
        if (approval) {
          setDecidedApprovals((prev) =>
            prev[event.approvalId] ? prev : { ...prev, [event.approvalId]: { verdict, approval } },
          );
        }
        // A failed attempt here is superseded the moment the approval resolves
        // anywhere (#842 review). The retry path clears its own failure, but a
        // decision made on the Approvals page or in another tab arrives only as
        // this frame — and a settled approval that still carried "not recorded"
        // would be the card contradicting the queue, which is the drift the
        // batching work exists to remove. Cleared unconditionally on the id,
        // whether or not this console ever held a summary for it.
        clearFailure(event.approvalId);
      }
      void feed.refresh();
    },
    onResync: resyncDurableState,
    onRecoveryError: useCallback(() => {
      toast.error("Live updates couldn't be recovered", {
        description: "We couldn't refresh the latest company state. Check your connection and try again.",
      });
    }, []),
  });

  return (
    <SidebarProvider className="h-svh overflow-hidden">
      {/* Mobile turns the sidebar into a sheet, so its own collapse control is
          not mounted while it is closed. Keep the way back fixed to the
          viewport and below the page controls rather than competing with a
          view's toolbar. Desktop keeps the labelled collapse row in the
          sidebar itself. */}
      <SidebarTrigger
        aria-label="Toggle sidebar"
        className="fixed bottom-4 left-4 z-50 md:hidden"
      />
      <AutoCollapse view={view} />
      <Sidebar collapsible="icon">
        <SidebarHeader>
          <SidebarCollapseToggle />
        </SidebarHeader>
        <SidebarContent data-tour="sidebar">
          <SidebarGroup>
            <SidebarMenu>
              {NAV.map((item) => (
                <SidebarMenuItem key={item.view} data-tour={`nav-${item.view}`}>
                  <SidebarMenuButton
                    isActive={view === item.view}
                    tooltip={item.label}
                    onClick={() => setView(item.view)}
                    className={RESTING_ROW}
                  >
                    <item.icon />
                    <span>{item.label}</span>
                  </SidebarMenuButton>
                  {item.view === "approvals" && pending > 0 && (
                    <>
                      <SidebarMenuBadge>{pending}</SidebarMenuBadge>
                      {/* Issue #1018: the badge is the sidebar's only attention
                          signal and `SidebarMenuBadge` hides itself on the
                          collapsed rail, so a collapsed sidebar said nothing was
                          waiting. The dot is the same `pending` value rendered
                          so it survives 32px — not a second source, so it cannot
                          disagree with the badge or fork the count contract
                          #932 pins. Exactly one of the two is visible at a
                          time. */}
                      <SidebarMenuDot
                        label={`${pending} ${pending === 1 ? "approval needs" : "approvals need"} you`}
                      />
                    </>
                  )}
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter>
          <SidebarControls
            lifecycleState={feed.status.lifecycle}
            emergencyPaused={feed.status.emergency_paused}
            companies={companies}
            activeCompany={company}
            onSwitchCompany={onSwitchCompany}
            onBackToPicker={onBackToPicker}
            view={view}
            onNavigate={setView}
          />
        </SidebarFooter>
        <SidebarRail />
      </Sidebar>

      {/* `min-w-0`: the inset is a flex item beside the sidebar, and a flex
          item's default `min-width: auto` floors it at its content's
          min-content width. That floor won — the inset measured a full window
          wide while sitting a sidebar's width to the right of the origin, so
          its last ~256px hung past the right edge of the window inside a
          wrapper that clips and cannot scroll. On the task board that clipped
          strip held the "Done" column, which is why a card could not be dragged
          into it (issue #334); every view was losing the same strip. */}
      <SidebarInset className="min-h-0 min-w-0">
        <main className="flex min-h-0 flex-1 flex-col overflow-hidden">
          {view === "overview" && (
            <Overview client={client} company={company} />
          )}
          {view === "company" && (
            <OrgChartView
              client={client}
              company={company}
              // Issue #485: chat's member pane links in at a desk
              // (`#/company/<deskId>`), which needs the hash's second segment
              // to reach this view at all — it was dropped here, so the chart
              // had no per-desk address to link to. `useHashView` hands the
              // segment back unvalidated, so the chart resolves an unknown id
              // itself rather than this shell guessing which desks exist.
              focusDeskId={sub}
            />
          )}
          {view === "chat" && (
            <ChatView
              client={client}
              company={company}
              sub={sub}
              onNavigate={(channelId) => navigate("chat", channelId)}
              onReply={() => void feed.refresh()}
              transcripts={transcripts}
              setTranscripts={setTranscripts}
              hydration={hydration}
              onSendStart={onSendStart}
              onSendEnd={onSendEnd}
              liveStepsByThread={liveStepsByThread}
              unread={unread}
              onChannelViewed={onChannelViewed}
              approvals={feed.approvals}
              chatChannelByThread={chatChannelByThread}
              now={feed.now}
              onDecideApproval={(approval, verdict, scope) =>
                void decideApproval(approval, verdict, scope)
              }
              decidingApprovals={decidingApprovals}
              decidedApprovals={decidedApprovals}
              failedApprovals={failedApprovals}
            />
          )}
          {view === "conversation" && (
            <Conversation
              client={client}
              company={company}
              threads={threads}
              activeId={activeThreadId}
              onSelect={setActiveThreadId}
              setMessages={setThreadMessages}
              onReply={() => void feed.refresh()}
              taskEventTick={taskEventTick}
              liveStepsByThread={liveStepsByThread}
              onSendStart={onSendStart}
              onSendEnd={onSendEnd}
            />
          )}
          {view === "inbox" && <InboxView client={client} company={company} />}
          {view === "tasks" && (
            <TasksView
              client={client}
              company={company}
              // Issue #464: the board learns that work appeared. The same
              // counter the chat's in-flight strip reads, so a card opened from
              // chat lands on the board without a reload.
              taskEventTick={taskEventTick}
              attemptEventTick={attemptEventTick}
              // Issue #883: a paused card is blocked until every approval its
              // turn parked is decided, and the board's own read carries none
              // of them. This is the feed the sidebar badge already polls, so
              // the card says what it is waiting on without a second request.
              approvals={feed.approvals}
              now={feed.now}
              // Issue #246: the card → chat half of the round trip. A card
              // opened from a conversation remembers which one, so its detail
              // screen can put the operator back in that thread.
              onOpenThread={(threadId) => {
                setActiveThreadId(threadId);
                setView("conversation");
              }}
              // Issue #883: "Review" on a blocked card opens the queue narrowed
              // to that card. Through `navigate` rather than `setView` so the
              // filter lands in the hash and survives a refresh and the Back
              // button, like every other sub-page.
              onReviewApprovals={(taskId) => navigate("approvals", encodeURIComponent(taskId))}
            />
          )}
          {view === "ledgers" && (
            <LedgersView
              client={client}
              company={company}
              // `#/ledgers/<slug>` opens that ledger. Unvalidated here, like
              // every other sub-page: only this view knows which slugs exist,
              // and it resolves an unknown one against the host rather than
              // guessing.
              sub={sub}
              onOpenLedger={(slug) => navigate("ledgers", slug ?? undefined)}
              // A board card leaves for its own screen. The board renders
              // here; the card's timeline, plan, discussion and attempts stay
              // where they already work.
              onOpenCard={(id) => navigate("tasks", id)}
            />
          )}
          {view === "team" && (
            <TeamView
              client={client}
              company={company}
              // `#/team/<agentId>` opens that agent (issue #264). The sub-page
              // comes back unvalidated, as `useHashView` documents: only this
              // view knows which ids exist, and the detail screen resolves an
              // unknown one against the host rather than guessing here.
              sub={sub}
              onOpenAgent={(agentId) => navigate("team", agentId ?? undefined)}
            />
          )}
          {view === "memory" && <MemoryView client={client} company={company} />}
          {view === "workspace" && (
            <Suspense
              fallback={
                <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
                  Loading workspace…
                </div>
              }
            >
              <WorkspaceView
                client={client}
                company={company}
                // Issue #327: live writes, so a note an agent creates or a
                // deliverable the publish drain lands shows up without a
                // refresh.
                event={workspaceEvent}
                refreshTick={workspaceRefreshTick}
                // Issue #552: the Artifacts tab's "Open in workspace" link
                // sets `#/workspace/<nodeId>`, and `useHashView` hands the
                // second segment back unvalidated — only this view knows
                // which node ids exist, so it resolves an unknown one against
                // the host rather than this shell guessing here.
                initialNodeId={sub}
              />
            </Suspense>
          )}
          {view === "approvals" && (
            <ApprovalsView
              client={client}
              company={company}
              feed={feed}
              // Issue #883: `#/approvals/<taskId>` narrows the queue to one
              // card, so "Review" on a blocked card lands on its approvals
              // rather than on a page the operator has to search. Same
              // unvalidated second segment every other sub-page gets — only
              // this view knows whether the id matches anything parked, so it
              // does that check itself and says so when it does not.
              sub={sub}
              onResolved={noteSystem}
              onGoToConversation={() => setView("chat")}
            />
          )}
          {view === "workflows" && (
            <Suspense
              fallback={
                <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
                  Loading canvas…
                </div>
              }
            >
              <WorkflowsView
                client={client}
                company={company}
                // Issue #339: `#/workflows/<workflowId>` names the graph to open
                // on the canvas, so a finished task card can link to the
                // workflow it built or ran. Same unvalidated second segment
                // every other sub-page gets — only this view knows which
                // workflow ids exist, so it does that check itself.
                sub={sub}
                runEventTick={workflowRunTick}
                runEvents={workflowRunEvents}
                listEventTick={workflowListTick}
                // Issue #1002: a run that parked cards can be unblocked from
                // the run drawer, without leaving the run to find the rows in a
                // flat queue. The SAME feed the Approvals page and the sidebar
                // badge read, handed over unfiltered — this is a second reader
                // of one queue, so the page still lists every row and the badge
                // still counts every row.
                //
                // The four maps below are the same console-local state the
                // inline chat card is given, owned here for the same reason: an
                // operator who decides in the drawer, steps over to Approvals
                // and comes back must not find a card that forgot what they did.
                // Their `decided` half is fed by the `approval_resolved` frame
                // as well as by this console's own resolves, which is what makes
                // a decision taken on the page settle in the drawer with no
                // reload.
                approvals={feed.approvals}
                approvalsNow={feed.now}
                decidingApprovals={decidingApprovals}
                decidedApprovals={decidedApprovals}
                failedApprovals={failedApprovals}
                onDecideApproval={(approval, verdict, scope) =>
                  void decideApproval(approval, verdict, scope)
                }
              />
            </Suspense>
          )}
          {view === "finances" && (
            <Suspense
              fallback={
                <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
                  Loading finances…
                </div>
              }
            >
              <FinancesView client={client} company={company} />
            </Suspense>
          )}
          {view === "settings" && (
            <SettingsSection
              client={client}
              company={company}
              feed={feed}
              sub={sub}
              onNavigate={(page) => navigate("settings", page)}
              onFlag={() => setFeedbackOpen(true)}
            />
          )}
          {view === "feedback" && <FeedbackView client={client} company={company} />}
        </main>
      </SidebarInset>

      <FeedbackDialog
        client={client}
        company={company}
        open={feedbackOpen}
        onOpenChange={setFeedbackOpen}
      />

      <TourController company={company} setView={setView} />
    </SidebarProvider>
  );
}
