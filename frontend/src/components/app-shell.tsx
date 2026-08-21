import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  // AppWindow,  // re-add with the Pages nav entry below
  Brain,
  FolderClosed,
  LayoutDashboard,
  type LucideIcon,
  MessagesSquare,
  Network,
  Settings2,
  ShieldCheck,
  BookText,
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
import { ContentSurface } from "@/components/content-surface";
import { FeedbackDialog } from "@/components/feedback-dialog";
import { HostSwitcher } from "@/components/host-switcher";
import {
  RESTING_ROW,
  SidebarCollapseButton,
  SidebarControls,
} from "@/components/sidebar-controls";
import { SetupController } from "@/setup/SetupController";
import { TourController } from "@/tour/TourController";
import { useCompany } from "@/hooks/use-company";
import { getRun, listRuns } from "@/api/runs";
import { startVisiblePolling } from "@/lib/visible-poll";
import { mergeOpenTurns, openTurnsFromRuns, PendingSyncPosts, type OpenTurn } from "@/lib/live-reply";
import { type AgentReplyEvent, type CompanyStreamEvent, useEvents } from "@/hooks/use-events";
import { useLedgerNav } from "@/hooks/use-ledger-nav";
import type { WorkspaceEvent } from "@/views/WorkspaceView";
import { useHashView } from "@/hooks/use-hash-view";
import { BOARD_LEDGER } from "@/lib/board-columns";
import { VIEWS, type View } from "@/lib/console-routes";
import { taskIdFromSegment } from "@/lib/task-route";
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
import { approvedLine, staleDecisionLine } from "@/lib/approval-wording";
import { writeLastChannel } from "@/lib/last-channel";
import { fromDto, type TeamMember } from "@/lib/team";
import { agentDmThreads, defaultThreads, threadsFromDesks } from "@/lib/threads";
import { Overview } from "@/views/Overview";
import { CompanyView } from "@/views/company/CompanyView";
import { ManageListsView } from "@/views/company/ManageListsView";
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
import { LedgersView, MANAGE_SEGMENT } from "@/views/LedgersView";
import { TaskDetailRoute } from "@/views/TaskDetailRoute";
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
// Hosts a sandboxed iframe and the postMessage bridge — load on demand, same
// as the other heavier, less-visited surfaces.
const PagesView = lazy(() => import("@/views/PagesView").then((m) => ({ default: m.PagesView })));

// The route table lives in `@/lib/console-routes` — a plain module the unit
// lane can import, and the single place a surface is declared routable (issue
// #1311). Re-exported because the console has always imported `View` from the
// shell that renders those views.
export type { View };

interface NavItem {
  view: View;
  label: string;
  icon: LucideIcon;
}

// One flat list. The nav was grouped under "Operate" and "Configure" when the
// second group held five entries; now that configuration is a section of its
// own, a heading over two rows labelled more than it sorted.
//
// "Work" (issue #1284, Rule 2 of docs/spec/runtime/ledgers-console-ia.md) is
// one static row landing on Tasks by default; every other list the company
// holds (Goals, Decisions, whatever it declared) is reachable from a
// switcher on `LedgersView`'s own page title, not a second nav element.
// Three other shapes were tried and rejected first: a row per list (unusable
// at the 12-declared-list cap — 15 list rows plus 8 other NAV entries), a
// collapsible sidebar section (still wrong premise: a declared list is read
// occasionally, mostly written by agents, not a surface an operator works
// out of the way Tasks is), and a tab strip (solved scaling but taxed the
// most-visited screen with a permanent band for lists rarely opened) — see
// the doc for the full reasoning on each. Do not re-add any of them without
// reading that doc first.
const NAV: NavItem[] = [
  { view: "overview", label: "Overview", icon: LayoutDashboard },
  // Issue #311: the company's structure, and the only way in to desk
  // creation and membership since #302 unmounted the flat Desks page.
  { view: "company", label: "Company", icon: Network },
  { view: "chat", label: "Chat", icon: MessagesSquare },
  // Tasks by default; every other list the company holds is one click away
  // through the switcher on `LedgersView`'s own title. See the comment above
  // `NAV` for why this is one row rather than one per list or a tab strip.
  { view: "ledgers", label: "Work", icon: BookText },
  // What the company remembers, and — now that an operator can select a
  // memory engine — WHERE it remembers: the engine panel shows which driver
  // is bound, what it negotiated, and whether the boot probe reached it.
  // Re-listed (issue #302 parked it; the memory-engine work un-parks it).
  { view: "memory", label: "Brain", icon: Brain },
  { view: "workspace", label: "Workspace", icon: FolderClosed },
  { view: "approvals", label: "Approvals", icon: ShieldCheck },
  { view: "workflows", label: "Workflows", icon: Workflow },
  // Agent-authored internal dashboard pages, rendered in a sandboxed iframe
  // (docs/spec/runtime/pages.md). Placed beside Workflows: both are the
  // "something an agent built" surfaces, as opposed to the fixed views above.
  // Pages is deliberately not offered in the nav (issues #1171, #1172). Do not
  // "fix" the omission by adding it back. What keeps `#/pages` answering is its
  // entry in `@/lib/console-routes`, NOT this commented row — a commented row
  // routes nothing, which is exactly how the address died for four months
  // (issue #1311). Remove a nav row here and the surface is hidden; remove it
  // from `console-routes.ts` and the surface is gone.
  // { view: "pages", label: "Pages", icon: AppWindow },
  { view: "settings", label: "Settings", icon: Settings2 },
];

// Which views are routable is decided in `@/lib/console-routes`, not here.
// `NAV` above is presentation: a row means a surface is offered in the sidebar,
// and its absence means only that the surface is not offered. `VIEWS` is every
// surface this shell renders, complete by construction, so a view can never be
// rendered by the block below and unreachable by address at the same time —
// which is what happened to Pages between #1172 and #1311.

/**
 * Views whose **nav row always means the parent page**, never the sub-page the
 * operator was last on.
 *
 * Remembering a sub-segment per view is right for a tab whose sub-pages are
 * places *within* it — `#/workflows/<id>` is still Workflows, and returning to
 * the tab should not throw away which workflow was open.
 *
 * Company is not that (issue #1193). Its segments are two different surfaces:
 * `#/company` is the roster and `#/company/desks` is the org chart, which is
 * where desks are created, deleted and re-staffed. Remembering the segment
 * would mean an operator who once opened Desks gets the org chart every time
 * they click Company afterwards — the same "the page opens on the chart for
 * someone who wanted their team" failure that the remembered *mode* had, wearing
 * a different mechanism. #1193 removed the mode; this keeps the route honest.
 *
 * Explicit addresses are untouched: a `#/company/desks` link, a `#/company/<deskId>`
 * deep link from chat (issue #485), and `onNavigate` all pass a segment
 * outright, and this only governs the no-segment case.
 */
const NAV_ALWAYS_PARENT = new Set<View>(["company"]);

/**
 * `#/tasks` with no card named the board page, which is retired (issue #1140).
 *
 * It lands where the board actually lives now, so a bookmark, a habit, or a
 * link written before the move all still arrive at a board rather than at a
 * 404 — and so does `#/tasks/<malformed>`, which names no card either. A real
 * `#/tasks/<id>` returns `null` from here and resolves untouched.
 *
 * Module scope, because `useHashView` holds this in a `useCallback` dependency
 * list: an inline arrow would be a new identity on every render and would
 * re-resolve the route on each one.
 */
const REWRITE_RETIRED = (
  head: string,
  sub: string | null,
): [View, string | null] | null => {
  if (head === "tasks" && taskIdFromSegment(sub) === null) return ["ledgers", BOARD_LEDGER];
  // Bare `#/team` is the Company page now (issue #1141). It rendered the
  // teammate card grid from a route with no nav entry, so nobody arrived at it;
  // the grid is Company's Cards half, and leaving `#/team` answering as well
  // would leave two live addresses drawing one grid with no relationship
  // between them. A named teammate is untouched — `#/team/<agentId>` is the
  // detail sub-page (issue #264), it is what the org chart's rows and the chat
  // pane's chips link to, and it is deliberately a page so it can be linked.
  if (head === "team" && !sub) return ["company", null];
  return null;
};

/** How many workflow run-progress frames (issue #371) the shell keeps for the
 * Workflows canvas. A run emits roughly one per node, so this holds many runs'
 * worth — it exists to bound a long-lived tab, not to ration frames. */
const WORKFLOW_EVENT_WINDOW = 300;

/**
 * How often an open turn's row is re-read.
 *
 * Slower than a UI tick on purpose — the live SSE frames are what make the turn
 * feel responsive, and this poll exists to catch the *transition* (and to be
 * right when the frames were missed), not to drive the animation.
 */
const TURN_POLL_MS = 4000;

/**
 * Operator-facing copy for a legacy `connect_error` query from the former
 * native OAuth callback (issue #300). The callback now ends in its own dated
 * explanatory page (#838), but an older bookmarked URL still gets a safe
 * message rather than raw provider-controlled error text.
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
  const [view, sub, navigate] = useHashView<View>(
    VIEWS,
    "overview",
    REWRITE_RETIRED,
  );
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
      const remembered = NAV_ALWAYS_PARENT.has(next)
        ? undefined
        : lastSubByViewRef.current[next];
      if (remembered) {
        navigate(next, remembered);
        return;
      }
      navigate(next);
    },
    [navigate],
  );

  // Every list this company holds — the single read `LedgersView`'s own
  // title switcher and Manage Lists both read (issue #1284). `refresh` is
  // handed to Manage Lists so declaring or retiring a list is visible in the
  // switcher's menu the same render cycle, with no reload — there is no SSE
  // event for either (see the hook's own doc comment).
  const ledgerNav = useLedgerNav(client, company);
  const [feedbackOpen, setFeedbackOpen] = useState(false);
  /**
   * Whether the product tour should hold — first-run setup is on screen, or the
   * company still has nobody on it (`docs/spec/runtime/company-setup.md`).
   *
   * Setup runs first, and the tour waits until there is a team to walk through.
   * Holding on emptiness rather than only on the dialog is what stops a skipped
   * setup from handing the operator a tour of empty pages instead.
   */
  // Starts held: until `SetupController` has read the roster we do not know
  // whether setup is about to open, and an unheld tour would flash its welcome
  // over it.
  const [setupOpen, setSetupOpen] = useState(true);
  /** Set by the Team page's prompt to reopen setup after a skip. */
  const [setupForced, setSetupForced] = useState(false);
  /**
   * Did this mount start on a view the operator named?
   *
   * Captured once, from the first render's route, so first-run setup can decline
   * to open over a deep link. `useRef(...).current` rather than state: it is a
   * property of how the console was opened and must never change afterwards —
   * the tour drives `view` around, and re-reading it would let a tour step
   * suppress the very dialog that is meant to precede the tour.
   */
  const deepLinked = useRef(view !== "overview" || Boolean(sub)).current;
  /**
   * Bumped when setup finishes, so the Team page re-reads a roster that now has
   * people on it. A counter rather than a boolean: a second run must re-trigger
   * the read, and a flag that was already `true` would not.
   */
  const [teamBuilt, setTeamBuilt] = useState(0);
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
  // `openTurns` is company-scoped too: the row ids that name a durable turn
  // belong to one company, yet the indicator is keyed by *thread* ("main" being
  // the universal id), so an old company's still-open turn would otherwise
  // keep driving a new company's working indicator after a switch. Empty it on
  // company change; the hydration re-arm (`GET {scope}/runs`) below restores
  // whatever the new company actually has in flight, exactly as it does for a
  // mid-turn reload.
  useEffect(() => {
    setOpenTurns((prev) => (Object.keys(prev).length === 0 ? prev : {}));
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
  /**
   * Threads with a **synchronous** chat POST in flight — the only ones whose
   * live `agent_reply` frames must be held back rather than rendered
   * immediately (see `injectAgentReply`).
   *
   * A thread joins on `onSendStart` and leaves on whichever of three outcomes
   * its POST reaches: `onSendEnd` when it resolved with a body, `onSendDetached`
   * when it answered `202`, `onSendFailed` when it threw. The last two leave the
   * turn running, so from either the POST has stopped being the delivery path
   * and the live frame is the answer rather than an echo of one.
   *
   * A frame that arrives before any of them fires is not dropped — `capture`
   * queues it, and the outcome resolves the queue once the POST's shape is known
   * (issue #1000). Only `onSendEnd` discards what was queued, because only there
   * has the reply already been rendered. That is what makes this safe against a
   * detached turn's reply beating the `202` itself back to the browser, and
   * against a cut connection taking a still-running turn's reply with it.
   */
  const pendingPostThreadsRef = useRef(new PendingSyncPosts<AgentReplyEvent>());
  /**
   * Turns accepted but not settled, by the thread they belong to (issue #983).
   *
   * This is what makes a mid-turn reload work, which was impossible before the
   * turn was durable: the open rows are read back from
   * `GET {scope}/runs?status=pending,running` on hydration, so the working
   * indicator is re-armed on a console that never saw the POST.
   */
  // Per thread, in acceptance order — a thread can hold a running turn and a
  // queued one behind it, and the poll watches them all (issue #1000). The
  // working row is the head; `ChatView` and `Conversation` read `[0]`.
  const [openTurns, setOpenTurns] = useState<Record<string, OpenTurn[]>>({});
  // Approval ids THIS console is deciding right now, or just decided a moment
  // ago (issue #1211) — so the generic SSE echo of `approval_resolved` can be
  // suppressed for exactly the decision this tab made, the same way
  // `pendingPostThreadsRef` suppresses the `agent_reply` echo of a chat send
  // this tab POSTed. Added the instant a decide path starts (before the
  // network call — the SSE frame can race ahead of the awaited response),
  // consumed (checked-and-cleared) the moment the matching frame arrives, in
  // `isOwnDecision` below. A single small `Set` — bounded by however many
  // decisions are in flight or freshly settled, which is never many.
  const ownApprovalDecisionsRef = useRef<Set<string>>(new Set());
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

  // A legacy native OAuth callback may have left `connected` or `connect_error`
  // in a bookmarked URL. Land the operator on Connections, say what happened,
  // then strip the params so a refresh does not re-fire them. The #838 callback
  // itself now terminates on its explanatory page and never writes a credential.
  // Runs once; StrictMode's double invoke is harmless because the first run
  // clears the params the second reads.
  //
  // Connections is a page of the Settings section now (`#/settings/connections`),
  // so the bounce-back lands there rather than on a top-level view.
  //
  // Before issue #300 the host answered a cancelled or expired handshake with a
  // JSON body, which the browser rendered as the page — a dead end with no way
  // back into the console. Preserve the readable landing for legacy URLs even
  // though #838 no longer redirects new native OAuth callbacks here.
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

    // Re-arm the working indicator for turns already in flight (issue #983).
    //
    // This is the leg a reload could not have before: until the turn had a
    // durable row there was nothing to ask, so a console reloaded mid-turn
    // showed a settled-looking transcript and no sign that an answer was still
    // coming. The rows are the query — `pending` and `running` are exactly the
    // open ones — and each carries the conversation that raised it, so the
    // indicator goes back on the right thread.
    //
    // A host that predates the route 404s and nothing is re-armed, which is
    // today's behaviour rather than a broken one.
    listRuns(client, company, { status: ["pending", "running"] })
      .then((runs) => {
        if (cancelled) return;
        // The fold — which rows count, and queued-vs-working — lives in
        // `openTurnsFromRuns` so it is assertable without a React tree.
        const open = openTurnsFromRuns(runs);
        // Merged rather than assigned: a turn POSTed while this request was in
        // flight is already in the map and is the more recent truth. The merge
        // appends per thread and collapses the same turn onto one entry, so a
        // re-arm never evicts a row the POST leg is already watching.
        if (Object.keys(open).length) setOpenTurns((prev) => mergeOpenTurns(prev, open));
      })
      .catch(() => {
        /* host without `/runs`, or offline — nothing to re-arm */
      });

    return () => {
      cancelled = true;
    };
  }, [client, company]);

  /**
   * Whether this shell is still mounted, for work that outlives the effect that
   * started it.
   *
   * An effect-scoped `cancelled` flag answers "is this effect still current",
   * which is the right question for a subscription and the wrong one for a
   * request whose whole purpose is to react to the state change that retires
   * that effect. {@link reReadSettledThread} is that case; see its doc.
   */
  const mountedRef = useRef(true);
  // The latest company, so an async completion started for one company can
  // tell whether it still belongs to the active scope (issue #1000).
  const companyRef = useRef(company);
  useEffect(() => {
    companyRef.current = company;
  }, [company]);
  useEffect(() => {
    // Re-armed on mount, which is not redundant with the initial `true`:
    // `main.tsx` renders under `StrictMode`, so in development React mounts,
    // unmounts and remounts every component once. Without this line the
    // cleanup below would latch the ref to `false` on that first throwaway
    // mount and the re-read would be dead for the rest of the dev session —
    // and only in dev, which is the worst place for a difference to hide.
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  /**
   * Rebuild one thread's transcript from `chat/history` after its turn settled
   * (issue #983), folding into both stores — the parked Conversation reads
   * `threads`, the Chat workspace reads `transcripts`.
   *
   * ## Why this is not inside the poll effect, which is where it started
   *
   * It was, and there it never ran (issue #1000). The caller settles a turn by
   * deleting it from `openTurns`, and `openTurns` is the poll effect's own
   * dependency — so that delete re-renders, React tears the effect down, and
   * its cleanup sets `cancelled = true` long before a network round trip can
   * come back. The fold was guarded on that flag, so every durable re-read this
   * console ever issued was fetched, parsed and thrown away.
   *
   * Nothing reported it because the live `agent_reply` frame had almost always
   * drawn the same reply seconds earlier: a backstop that silently never fires
   * is indistinguishable from one that is never needed. What made it visible is
   * the hosted tenant, where the manager's proxy buffers whole response bodies
   * and an SSE stream therefore never delivers a frame at all
   * (`opencompany-microservice#23`). There the poll is not a backstop — it is
   * the only delivery path — and a detached turn's reply simply never appeared.
   *
   * So the lifetime that governs this read is the component's, not the effect's.
   * Unmount is the only thing that can make the answer unwanted, which is what
   * {@link mountedRef} says and `cancelled` never did.
   *
   * Idempotent by construction: both folds drop entries whose message id is
   * already present, so a second call for the same thread — a late poll tick, a
   * settle racing a re-arm — adds nothing.
   */
  const reReadSettledThread = useCallback(
    (threadId: string) => {
      client
        .getChatHistory(threadId, company)
        .then((entries) => {
          if (!mountedRef.current || entries.length === 0) return;
          const hydrated = fromHistory(entries);
          setThreads((ts) =>
            ts.map((t) => {
              if (t.id !== threadId) return t;
              const known = new Set(t.messages.map((m) => m.id));
              const fresh = hydrated.filter((m) => !known.has(m.id));
              return fresh.length === 0 ? t : { ...t, messages: [...t.messages, ...fresh] };
            }),
          );
          const channelId = chatChannelByThread[threadId];
          if (!channelId) return;
          setTranscripts((t) => {
            const known = new Set((t[channelId] ?? []).map((m) => m.id));
            const fresh = hydrated.filter((m) => !known.has(m.id));
            return fresh.length === 0
              ? t
              : { ...t, [channelId]: [...(t[channelId] ?? []), ...fresh] };
          });
        })
        .catch(() => {
          /* offline — the next hydration pass still rebuilds it */
        });
    },
    [client, company, chatChannelByThread],
  );

  /**
   * Watch each open turn to its end, and rebuild the transcript from the
   * durable record when it gets there (issue #983).
   *
   * ## The read is the backstop; the frames are the optimisation
   *
   * The terminal transition always re-reads `chat/history` for that thread, even
   * though the reply usually arrived live moments earlier. That is the point: a
   * frame dropped by a reconnecting `EventSource`, a proxy that buffered it away
   * or a tab that was asleep leaves the live path with nothing, and the durable
   * transcript is the only thing that is complete in every one of those cases.
   * Once per transition, not on a timer — the fold is idempotent (hydration
   * merges by message id) but a re-read per poll would be a lot of history for
   * nothing.
   *
   * `startVisiblePolling` is the same helper every other polling surface uses,
   * so a hidden tab stops asking and re-reads once on the way back to visible —
   * which is exactly the recovery a slept tab needs.
   */
  useEffect(() => {
    // Every armed turn on every thread, not one per thread: a second detached
    // send queues a row behind the running one, and both have a reply the
    // operator is waiting on (issue #1000). A turn with no row (`turnId`
    // absent) still cannot be watched and is skipped, as before.
    const watching = Object.entries(openTurns).flatMap(([threadId, turns]) =>
      turns.filter((t) => t.turnId).map((t) => [threadId, t] as const),
    );
    if (watching.length === 0) return;
    let cancelled = false;

    const settle = (threadId: string, turnId: string) => {
      setOpenTurns((prev) => {
        const turns = prev[threadId];
        if (!turns) return prev;
        // Drop just this turn; a queued sibling behind it stays watched, so
        // its reply is still delivered when it settles in turn.
        const rest = turns.filter((t) => t.turnId !== turnId);
        const next = { ...prev };
        if (rest.length) next[threadId] = rest;
        else delete next[threadId];
        return next;
      });
      // Deliberately not awaited here, and deliberately not written inline —
      // see `reReadSettledThread` for why the re-read cannot live inside this
      // effect. The line above is what tears this effect down.
      reReadSettledThread(threadId);
    };

    const poll = () => {
      for (const [threadId, turn] of watching) {
        if (!turn.turnId) continue;
        getRun(client, company, turn.turnId)
          .then(({ run }) => {
            if (cancelled) return;
            if (run.phase === "terminal") {
              settle(threadId, turn.turnId!);
              return;
            }
            // Still open: keep the queued/working distinction honest. `pending`
            // means it has not taken the per-company lock yet.
            const queued = run.status === "pending";
            setOpenTurns((prev) =>
              prev[threadId]?.some((t) => t.turnId === turn.turnId && t.queued !== queued)
                ? {
                    ...prev,
                    [threadId]: prev[threadId].map((t) =>
                      t.turnId === turn.turnId ? { ...t, queued } : t,
                    ),
                  }
                : prev,
            );
          })
          .catch((err: unknown) => {
            // Only a confirmed missing row — the host answering 404 for this
            // turn id — is "the turn is over"; a transient network or server
            // blip is not, and settling on one would tear down the very poll
            // that is the sole delivery path when `/events` is buffered or
            // unavailable (issue #1000). The next tick retries; if the host is
            // genuinely gone it will keep answering and the row eventually
            // settles through whatever terminal signal it does answer.
            if (cancelled) return;
            if (err instanceof ApiError && err.status === 404 && turn.turnId)
              settle(threadId, turn.turnId);
          });
      }
    };

    const dispose = startVisiblePolling(poll, TURN_POLL_MS);
    return () => {
      cancelled = true;
      dispose();
    };
    // `watching` is derived from `openTurns`; the transition that matters is a
    // turn opening or closing, which changes that map.
  }, [client, company, openTurns, reReadSettledThread]);

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

  // Render one `AgentReply` (issue #66) into its desk thread's transcript.
  // Dedupe against our own optimistic echo: the backend journals an
  // `AgentReply` for the operator's own chat turn too, and Conversation
  // already rendered that reply locally. Local message ids are ephemeral
  // counters (not content-addressed), so we key the dedupe on an identical
  // company line already present in the thread's recent tail. Only desks that
  // exist as a thread receive an injection; an unmatched chatId is a no-op
  // rather than polluting the wrong thread.
  //
  // Split out from {@link injectAgentReply} so a frame `PendingSyncPosts` held
  // back (issue #983) can be rendered from the same code once its thread's POST
  // resolves, instead of the shell needing a second copy of this logic.
  const renderAgentReply = useCallback(
    (event: AgentReplyEvent) => {
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
   * The live half of the `agent_reply` handler `useEvents` actually subscribes
   * with — routes each frame through `pendingPostThreadsRef` before it ever
   * reaches {@link renderAgentReply}.
   *
   * The operator's own chat turn is delivered synchronously by the awaited
   * POST (and that copy carries the steps timeline). The backend ALSO journals
   * an `AgentReply` for it, which arrives over SSE — first, mid-await — so a
   * blind render here would double the bubble. A thread with a POST in flight
   * therefore has its frames held rather than rendered; the POST reply is
   * authoritative once it lands, and the recent-tail content check inside
   * `renderAgentReply` still guards a late echo that arrives just after.
   *
   * **Conditional since issue #983, and this is the load-bearing part.** The
   * rule above only holds while the POST is going to *deliver* the reply. A
   * detached turn answers `202` immediately and delivers nothing, so for it
   * this live frame IS the answer — dropping it would mean the reply never
   * appears at all, which is a strictly worse failure than the double bubble
   * this guard exists to prevent.
   *
   * `capture` never drops what it holds. A detached turn's own `agent_reply`
   * can — and in a fast turn regularly does — arrive before this browser has
   * even parsed the `202` body that would have told `onSendDetached` to stop
   * holding: `onSendStart` arms synchronously, but nothing makes that race
   * resolve before the network does. Earlier code suppressed by a boolean and
   * threw the frame away for the whole window, which is exactly the bug (issue
   * #1000) — a fast enough reply vanished with nothing left to render. Holding
   * it instead means the POST's outcome always has something correct to do with
   * it: replay it once the shape turns out to be detached, replay it once the
   * request turns out to have died with the turn still running, discard it only
   * once it turns out to be the echo of a reply already rendered. Dedupe by
   * *what the POST turned out to be*, never by how long the frame waited.
   */
  const injectAgentReply = useCallback(
    (event: AgentReplyEvent) => {
      if (pendingPostThreadsRef.current.capture(event)) return;
      renderAgentReply(event);
    },
    [renderAgentReply],
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
    pendingPostThreadsRef.current.started(threadId);
    activeTurnThreadRef.current = threadId;
    setLiveStepsByThread((prev) => ({ ...prev, [threadId]: [] }));
  }, []);
  const onSendEnd = useCallback((threadId: string) => {
    pendingPostThreadsRef.current.ended(threadId);
    if (activeTurnThreadRef.current === threadId) activeTurnThreadRef.current = null;
    setLiveStepsByThread((prev) => {
      if (!prev[threadId]?.length) return prev;
      return { ...prev, [threadId]: [] };
    });
  }, []);
  /**
   * The host accepted the turn and handed back its id instead of its answer
   * (issue #983).
   *
   * Deliberately **not** `onSendEnd`. Two things must not happen here: the live
   * timeline must not be cleared (its steps are the only thing the operator can
   * see while the turn runs), and the working row must not come down — the turn
   * is still going, and a console that went idle the instant the POST resolved
   * would be back to claiming nothing is happening.
   *
   * What it does do is lift the echo suppression, because from here the stream
   * is the delivery path rather than a duplicate of one.
   *
   * `PendingSyncPosts.detached` hands back whatever `injectAgentReply` held
   * for this thread while its shape was still unknown — a fast turn's reply
   * can and does arrive before this callback does (issue #1000). Rendering
   * those now, in the order they arrived, is what makes lifting the
   * suppression lose nothing: the frame was never dropped, only queued.
   */
  const onSendDetached = useCallback(
    (threadId: string, turnId?: string) => {
      const held = pendingPostThreadsRef.current.detached(threadId);
      // Append, never replace (issue #1000). The serial lock queues a second
      // send behind the running turn, and a replace would stop the poll
      // watching the running row — the one whose reply settles first. The list
      // drains oldest-first, so the newest accepted turn goes on the end.
      setOpenTurns((prev) => {
        const turns = prev[threadId] ?? [];
        // The reload arm can race this POST's answer on the same turn.
        if (turnId && turns.some((t) => t.turnId === turnId)) return prev;
        return { ...prev, [threadId]: [...turns, { turnId, queued: true }] };
      });
      held.forEach((frame) => renderAgentReply(frame));
    },
    [renderAgentReply],
  );
  /**
   * The chat POST **threw** — no body, nothing rendered by the view (#1000).
   *
   * Also deliberately not `onSendEnd`, and for a sharper reason than
   * `onSendDetached` has. `onSendEnd` means "the awaited reply is on screen",
   * which licenses `PendingSyncPosts.ended` to discard whatever was held; a
   * throw put nothing on screen, so that call would delete the operator's only
   * copy of a reply that is still coming. The request is what died — the host
   * keeps running the turn and journals its reply onto the stream, which is
   * precisely the property issue #983 bought.
   *
   * So it releases the held frames and renders them, exactly as the detached
   * path does, and leaves the live timeline alone for the same reason: those
   * rows are a running turn's only visible trace, and `onSendStart` cleared
   * them at the top of this POST, so anything still there arrived during it.
   *
   * What it pointedly does **not** do is fabricate a turn id. A throw carries
   * no turn id of its own, so the poll could not be armed from the failure
   * alone without risking a spinner that nothing would ever take down.
   *
   * But a throw is **not** proof the host never accepted the turn — a cut
   * connection after the host journaled it is the whole premise of this
   * feature — so the durable row may exist even though the response died.
   * Re-query the open rows and, if a matching `pending`/`running` turn for this
   * thread was journaled, register it. That arms the real poll-and-history
   * recovery path (issue #983), the only delivery that works when `/events` is
   * buffered or unavailable; the poll's terminal transition re-reads
   * `chat/history`, so the reply the host went on to write lands on screen
   * without relying on SSE. If no such row exists, nothing is armed and the
   * view's `Couldn't send` line stands alone — a throw with no durable turn
   * behind it is not a working row to be invented.
   */
  const onSendFailed = useCallback(
    (threadId: string) => {
      const held = pendingPostThreadsRef.current.failed(threadId);
      held.forEach((frame) => renderAgentReply(frame));

      // Discover whether the host kept the turn after the request died. The
      // throw tells us nothing, but the run rows do: a `pending`/`running` row
      // naming this thread means the turn is durable and worth polling to its
      // terminal `chat/history` re-read — the SSE-less recovery path.
      listRuns(client, company, { status: ["pending", "running"] })
        .then((runs) => {
          if (!mountedRef.current) return;
          // A company switch that happened while the request was in flight
          // invalidates the result: the rows belong to the old company and
          // would restore a stale turn into the new company's openTurns map.
          if (companyRef.current !== company) return;
          const open = openTurnsFromRuns(runs);
          // The fold's whole list for this thread, not just its head: the POST
          // died mid-queue, so any rows the host kept are this turn's kin and
          // each has a reply to deliver. The merge appends and collapses by id.
          const durable = open[threadId];
          if (durable) setOpenTurns((prev) => mergeOpenTurns(prev, { [threadId]: durable }));
        })
        .catch(() => {
          /* host without /runs, or offline — nothing to re-arm */
        });
    },
    [client, company, renderAgentReply],
  );

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
    ownApprovalDecisionsRef.current.add(approval.id);
    markDeciding(approval.id, verdict);
    // A retry starts clean: the previous attempt's error must not sit under a
    // live one, or the operator cannot tell which attempt it belongs to.
    clearFailure(approval.id);
    try {
      const answer = await client.resolveApproval(approval.id, verdict, undefined, company, {
        detach: true,
        scope,
      });
      // Issue #1449: the same read the Approvals page makes, for the same
      // reason. This card detaches, so it gets a `ResolveReceipt` — which, until
      // #1449, had no shape at all for "the host default-denied this because the
      // deadline had passed". A card sitting in a transcript is exactly where a
      // request goes stale unnoticed, so this is the surface it happens on most.
      const stale = staleDecisionLine(answer.outcome);
      if (stale) {
        // The witnessed verdict is deliberately NOT the one that was clicked.
        // `decidedApprovals` feeds the transcript's permanent receipt, and
        // first write wins — so recording the request here would pin
        // "Approved — recorded" onto the card forever, which is the same false
        // claim as the toast, in the one place that never scrolls away.
        //
        // An `expired` card may be witnessed, and as a **deny**: the host has
        // just said it default-denied it, so that is a fact, not a guess. An
        // `already_resolved` one may not — the host cannot tell which way it
        // went, so nothing is written and the `approval_resolved` frame (or the
        // refresh in `finally`) settles the card with the truth.
        if (answer.outcome === "expired") {
          setDecidedApprovals((prev) =>
            prev[approval.id] ? prev : { ...prev, [approval.id]: { verdict: "deny", approval } },
          );
        }
        toast.info(stale);
        noteInChannel(approval.thread, stale);
        return;
      }
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
        noteInChannel(approval.thread, "Declined — the teammate will not take that action.");
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
    // Issue #1211: pop the id this console just decided so `use-events.ts` can
    // suppress the generic echo toast for exactly this decision — and only
    // this one, since a second frame for the same id must not read as "still
    // mine".
    isOwnDecision: (approvalId: string) => {
      const mine = ownApprovalDecisionsRef.current.has(approvalId);
      ownApprovalDecisionsRef.current.delete(approvalId);
      return mine;
    },
    onResync: resyncDurableState,
    onRecoveryError: useCallback(() => {
      toast.error("Live updates couldn't be recovered", {
        description: "We couldn't refresh the latest company state. Check your connection and try again.",
      });
    }, []),
  });

  return (
    // `SidebarProvider` paints the chrome layer itself — see its own note on
    // why that fill lives there and not here (issue #1178).
    <SidebarProvider className="h-svh overflow-hidden">
      <Sidebar collapsible="icon">
        <SidebarHeader>
          {/* The header is the column talking about itself: which host this
              console is looking at, and whether the column is showing.
              Everything BELOW it — the nav group and the footer's standing
              controls — takes you somewhere. Collapse used to be the first row
              under the switcher, which put a chrome control at the head of a
              list of destinations and made it read as one (issue #1177).

              `flex-col` on the rail is not a preference. The collapsed column
              is `--sidebar-width-icon` (3rem) and this block is `p-2`, leaving
              32px — the exact width of the switcher's glyph, with nothing left
              over to put beside it. See `SidebarCollapseButton`. */}
          <div className="flex items-center gap-1.5 group-data-[collapsible=icon]:flex-col group-data-[collapsible=icon]:gap-1">
            {/* Which host, and how every host is doing. It leads because it
                names where you are — the first thing the column should answer
                — and it is the only control here that can take you somewhere
                else entirely. See `host-switcher.tsx`; it replaced the icon
                rail that used to stand outside this sidebar (issue #1142).

                `min-w-0` so the nameplate truncates instead of pushing the
                button off the end of a 13.5rem column. */}
            <div className="min-w-0 flex-1 group-data-[collapsible=icon]:w-full group-data-[collapsible=icon]:flex-none">
              <HostSwitcher companyName={feed.status.name} />
            </div>
            <SidebarCollapseButton />
          </div>
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
        {/* The card half of the two-layer shell: the one opaque sheet in the
            console, floating on the chrome the shell root paints (issue
            #1178). A `div`, not `main` — `SidebarInset` above is already the
            console's one `<main>` landmark, and a second nested one gave every
            page two identical "skip to content" destinations (issue #1221). */}
        <ContentSurface>
          {view === "overview" && (
            <Overview client={client} company={company} companyName={feed.status.name} />
          )}
          {view === "company" && (
            <CompanyView
              client={client}
              company={company}
              // Issue #485: chat's member pane links in at a desk
              // (`#/company/<deskId>`), which needs the hash's second segment
              // to reach this view at all — it was dropped here, so the chart
              // had no per-desk address to link to. `useHashView` hands the
              // segment back unvalidated, so the chart resolves an unknown id
              // itself rather than this shell guessing which desks exist.
              //
              // Issue #1193: and the segment decides the surface outright.
              // Nothing (`#/company`) is the roster; `desks` is the org chart;
              // anything else is a desk on it. There is no remembered mode to
              // disagree with the address.
              sub={sub}
              onNavigate={(next) => navigate("company", next ?? undefined)}
              // The roster half's own sub-page is `#/team/<agentId>`, not a
              // second segment of this view — the teammate detail page is a
              // linkable address of its own (issue #264) and stays one.
              onOpenAgent={(agentId) =>
                agentId ? navigate("team", agentId) : navigate("company")
              }
              // Setup just staffed the company, so the roster read is stale.
              refreshKey={teamBuilt}
              // Skipping setup must not be a dead end: an unstaffed company keeps
              // a visible way back in.
              onRunSetup={() => setSetupForced(true)}
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
              onSendDetached={onSendDetached}
              onSendFailed={onSendFailed}
              openTurns={openTurns}
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
              onSendDetached={onSendDetached}
              onSendFailed={onSendFailed}
              openTurns={openTurns}
            />
          )}
          {view === "inbox" && <InboxView client={client} company={company} />}
          {/* All that is left of the Tasks page: the card detail. `sub` is a
              real id by the time this renders — `REWRITE_RETIRED` sent every
              other `#/tasks…` address to the board in Ledgers. */}
          {view === "tasks" && (
            <TaskDetailRoute
              client={client}
              company={company}
              taskId={taskIdFromSegment(sub) ?? ""}
              attemptEventTick={attemptEventTick}
              // Issue #883: so a waiting card can name the blocked call rather
              // than only counting it. The feed the sidebar badge already polls,
              // so the screen says what it is waiting on with no second request.
              parked={feed.approvals}
              // Issue #246: the card → chat half of the round trip. A card
              // opened from a conversation remembers which one, so its detail
              // screen can put the operator back in that thread.
              onOpenThread={(threadId) => {
                setActiveThreadId(threadId);
                setView("conversation");
              }}
              // Back, and a deleted card, go to the board — which is the
              // `tasks` ledger. Through `navigate` so the address follows.
              onLeave={() => navigate("ledgers", BOARD_LEDGER)}
            />
          )}
          {/*
            `MANAGE_SEGMENT` is checked *here*, before `LedgersView` ever
            mounts — not inside it (issue #1284). `LedgersView`'s own hooks
            read and write real list rows keyed on `sub`; running that
            machinery against a slug that names no list (`manage`, `new`)
            would be all cost and no ledger. Manage Lists lives in Work, not
            Company, on purpose: it is reached almost entirely from the title
            switcher's own menu, and a route that lived under Company while
            being opened from Work meant every visit crossed a section
            boundary and came back. `onBack` is `history.back()`, not a fixed
            destination, because this screen is reached from wherever a
            list's switcher was open, not from one canonical parent.
          */}
          {view === "ledgers" && sub === MANAGE_SEGMENT && (
            <ManageListsView
              client={client}
              company={company}
              ledgerNav={ledgerNav}
              onBack={() => window.history.back()}
            />
          )}
          {view === "ledgers" && sub !== MANAGE_SEGMENT && (
            <LedgersView
              client={client}
              company={company}
              // The single read the title switcher and Manage Lists share
              // (issue #1284) — this view no longer fetches the list itself.
              ledgers={ledgerNav.ledgers}
              ledgersLoading={ledgerNav.loading}
              remaining={ledgerNav.remaining}
              // `#/ledgers/<slug>` opens that list. Unvalidated here, like
              // every other sub-page: only this view knows which slugs
              // exist, and it resolves an unknown one against the host
              // rather than guessing. A bare `#/ledgers` resolves to Tasks.
              sub={sub}
              onOpenLedger={(slug) => navigate("ledgers", slug ?? undefined)}
              // A board card leaves for its own screen. The board renders
              // here; the card's timeline, plan, discussion and attempts stay
              // where they already work.
              onOpenCard={(id) => navigate("tasks", id)}
              // Issue #464: the board learns that work appeared. The same
              // counter the chat's in-flight strip reads, so a card opened from
              // chat lands on the board without a reload.
              taskEventTick={taskEventTick}
              // Issue #883: a paused card is blocked until every approval its
              // turn parked is decided, and neither the ledger's rows nor the
              // task store carries them. This is the feed the sidebar badge
              // already polls, so the card says what it is waiting on without a
              // second request.
              approvals={feed.approvals}
              now={feed.now}
              // Issue #883: "Review" on a blocked card opens the queue narrowed
              // to that card. Through `navigate` rather than `setView` so the
              // filter lands in the hash and survives a refresh and the Back
              // button, like every other sub-page.
              onReviewApprovals={(taskId) => navigate("approvals", encodeURIComponent(taskId))}
              // The switcher's in-place wizard declared a new list — re-read
              // the shared list so it shows up in the menu (and Manage
              // Lists, which reads the same instance) with no reload.
              onListsChanged={ledgerNav.refresh}
            />
          )}
          {/*
            `#/team/<agentId>` only. Bare `#/team` is rewritten to `#/company`
            below (issue #1141) — the grid it used to render is the Company
            page's Cards half now, and two addresses drawing the same grid is
            the ambiguity that rewrite exists to remove.

            The sub-page comes back unvalidated, as `useHashView` documents:
            only this view knows which ids exist, and the detail screen resolves
            an unknown one against the host rather than guessing here.
          */}
          {view === "team" && (
            <TeamView
              client={client}
              company={company}
              sub={sub}
              onOpenAgent={(agentId) =>
                agentId ? navigate("team", agentId) : navigate("company")
              }
              // Setup just staffed the company, so the roster read is stale.
              refreshKey={teamBuilt}
              // Skipping setup must not be a dead end: an unstaffed company keeps
              // a visible way back in.
              onRunSetup={() => setSetupForced(true)}
              // A desk chip on a teammate's detail page opens that desk (issue #1440).
              onNavigateToDesk={(deskId) => navigate("company", deskId)}
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
              // Issue #1211: mark this id as "mine" before the resolve POST
              // goes out, so the SSE echo for it — which can arrive before the
              // POST settles — is not toasted a second time.
              onDecideStart={(approvalId) => ownApprovalDecisionsRef.current.add(approvalId)}
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
          {view === "pages" && (
            <Suspense
              fallback={
                <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
                  Loading pages…
                </div>
              }
            >
              <PagesView client={client} company={company} />
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
        </ContentSurface>

        {/* Mobile only: dedicated chrome for the way back to navigation, not an
            overlay on top of it. A `fixed` trigger here used to float over
            whatever content happened to scroll into the bottom-left corner and
            win every hit-test in that region (issue #1265) — this bar reserves
            its own row in SidebarInset's flex column instead, so the content
            wrapper's flex-1 height (and every view's own overflow-y-auto
            within it) already stops short of it. No view needs to know this
            control exists. */}
        {/* `p-3` on all four sides, matching `--frame-inset`, so this control
            lines up with the card's own margin instead of hanging off a
            different number. The card already supplies the gap above it through
            that bottom margin — every page is framed now, so there is no longer
            a flush-to-the-edge case for this row to compensate for. */}
        <div className="flex shrink-0 items-center bg-transparent p-3 md:hidden">
          <SidebarTrigger aria-label="Toggle sidebar" />
        </div>
      </SidebarInset>

      <FeedbackDialog
        client={client}
        company={company}
        open={feedbackOpen}
        onOpenChange={setFeedbackOpen}
      />

      <SetupController
        client={client}
        company={company}
        force={setupForced}
        deepLinked={deepLinked}
        onForceHandled={() => setSetupForced(false)}
        onOpenChange={setSetupOpen}
        onCompleted={() => setTeamBuilt((n) => n + 1)}
      />

      <TourController company={company} setView={setView} hold={setupOpen} />
    </SidebarProvider>
  );
}
