import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { OpenCompanyClient } from "@/api/client";
import {
  ApiError,
  type ApprovalSummary,
  type BlockerVerdict,
  type CompanyStatus,
  type GrantScope,
  type NotificationDto,
  type TurnStep,
  type Verdict,
} from "@/api/types";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarInset,
  SidebarProvider,
  SidebarRail,
  SidebarTrigger,
} from "@/components/ui/sidebar";
import { AgentProfileProvider } from "@/components/agent-profile-sheet";
import { ApprovalsButton } from "@/components/approvals-button";
import { ContentSurface } from "@/components/content-surface";
import { FeedbackDialog } from "@/components/feedback-dialog";
import { HostSwitcher } from "@/components/host-switcher";
import { OverviewButton } from "@/components/overview-button";
import { RouteLoading } from "@/components/route-loading";
import { WINDOW_TITLE_BAR_HEIGHT } from "@/components/window-chrome";
import { WindowTitleBar } from "@/components/window-title-bar";
import { SidebarCollapseButton, SidebarUtilityBar } from "@/components/sidebar-controls";
import { SidebarNavigation } from "@/components/sidebar-navigation";
import { RoomRailSlotProvider } from "@/components/room-rail";
import { SetupController } from "@/setup/SetupController";
import {
  arrivedViaSetupHandoff,
  clearSetupHandoff,
  setupHandoffHasScope,
} from "@/setup/state";
import { TourController } from "@/tour/TourController";
import { OnboardingGate } from "@/onboarding/OnboardingGate";
import { useActivationGate } from "@/onboarding/useActivationGate";
import { clearGateSkipped, gateSkippedThisSession, markGateSkipped } from "@/onboarding/state";
import {
  resolveGateAdminCheckError,
  shouldHoldShellPending,
  shouldPollActivationForRole,
  shouldShowOnboardingGate,
} from "@/onboarding/gate-logic";
import { me as fetchMe } from "@/api/auth";
import { useCompany } from "@/hooks/use-company";
import { getRun, listRuns } from "@/api/runs";
import {
  listInflight,
  listTasks,
  taskStatusesById,
  type InflightRun,
  type TaskStatus,
} from "@/api/tasks";
import { startVisiblePolling } from "@/lib/visible-poll";
import { withReadTimeout } from "@/lib/read-timeout";
import {
  hasOtherOpenTurns,
  mergeOpenTurns,
  openTurnsFromRuns,
  PendingSyncPosts,
  type OpenTurn,
} from "@/lib/live-reply";
import {
  type AgentReplyEvent,
  budgetProximityExpiresAt,
  type CompanyStreamEvent,
  isBudgetProximityExpired,
  useEvents,
} from "@/hooks/use-events";
import { useLedgerNav } from "@/hooks/use-ledger-nav";
import {
  mentionCountsByChannel,
  mentionsToClear,
  threadsToReReadForMentions,
} from "@/lib/mention-badge";
import {
  flushPendingAcknowledgements,
  operationalNotificationSeverity,
  operationalNotificationsToAnnounce,
  scheduleAcknowledgement,
  type PendingAcknowledgement,
} from "@/lib/operational-notifications";
import { usePresence } from "@/hooks/use-presence";
import { useAutonomy } from "@/hooks/use-autonomy";
import { AutonomyPill } from "@/components/autonomy-pill";
import { useTyping } from "@/hooks/use-typing";
import { typersIn } from "@/lib/awareness";
import type { WorkspaceEvent } from "@/views/WorkspaceView";
import { useHashView } from "@/hooks/use-hash-view";
import { LEDGER_VIEW_PARAM, readLedgerViewMode } from "@/hooks/use-ledger-view-mode";
import { BOARD_LEDGER } from "@/lib/board-columns";
import { DEFAULT_VIEW, isNavigationActive, VIEWS, type View } from "@/lib/console-routes";
import { REWRITE_RETIRED } from "@/lib/console-route-rewrites";
import { taskIdFromSegment } from "@/lib/task-route";
import { toast } from "sonner";

import { foldLiveFrame } from "@/lib/live-frame";

import {
  type ChatMessage,
  dispatchMarkerPlacement,
  fromHistory,
  hostMessageId,
  liveFrameThreadKey,
  liveReplyIdentity,
  replyVoice,
  MAIN_THREAD_ID,
  makeMessage,
  mergeHistoryInOrder,
} from "@/lib/chat";
import { CONNECTION_PROVIDERS } from "@/lib/connections";
import { defaultDesks, GENERAL_CHANNEL, type Desk } from "@/lib/desks";
import { lifecycle } from "@/lib/language";
import { mergeReadFloors, unreadCount } from "@/lib/unread";
import {
  approvedLine,
  blockerDecidedLine,
  staleDecisionLine,
} from "@/lib/approval-wording";
import { writeLastChannel } from "@/lib/last-channel";
import { ProfileRow } from "@/components/profile-row";
import { ConsoleProvider } from "@/lib/console-context";
import { fromDto, type TeamMember } from "@/lib/team";
import { agentDmThreads, defaultThreads, threadsFromDesks } from "@/lib/threads";
import { drainReReadQueue, type PendingReRead } from "@/lib/re-read-queue";
import { fetchWithOneRetry } from "@/lib/fetch-with-retry";
import { Overview } from "@/views/Overview";
import { CompanyView } from "@/views/company/CompanyView";
import { ManageListsView } from "@/views/company/ManageListsView";
import { ChatView } from "@/views/ChatView";
import { shouldClearReceipt, type ChatReceipt } from "@/views/chat/ChatLiveReceipt";
import {
  channelForThread,
  channelIdForThread,
  deskFromDto,
  dmChannelId,
  dmThreadId,
  HISTORY_UNSTARTED,
  isOperatorChannelDto,
  type DecidedApproval,
  type HistoryHydration,
  type HistoryStatus,
  type Transcripts,
} from "@/views/chat/model";
import { TeamView } from "@/views/TeamView";
import { ApprovalsView } from "@/views/ApprovalsView";
import { LedgersView, MANAGE_SEGMENT } from "@/views/LedgersView";
import { TaskDetailRoute } from "@/views/TaskDetailRoute";
import { InboxView } from "@/views/InboxView";
import { FeedbackView } from "@/views/FeedbackView";
import { UnknownRouteView } from "@/views/UnknownRouteView";
import { ConnectionsSection } from "@/views/connections/ConnectionsSection";
import { SettingsSection } from "@/views/SettingsSection";
import { useLocalScope } from "@/connections/ConnectionContext";
import { signedOut } from "@/connections/registry";
import { canCreateCompanies } from "@/components/create-company-dialog";

// React Flow is heavy and only used here — load it on demand.
const WorkflowsView = lazy(() =>
  import("@/views/WorkflowsView").then((m) => ({ default: m.WorkflowsView })),
);
// Lazy for the reason the canvas is: it pulls recharts, and an operator who
// never opens the Observatory should not pay for it.
const ObservatoryView = lazy(() =>
  import("@/views/observatory/ObservatoryView").then((m) => ({
    default: m.ObservatoryView,
  })),
);
// Pulls in the markdown renderer — load on demand.
const WorkspaceView = lazy(() =>
  import("@/views/WorkspaceView").then((m) => ({ default: m.WorkspaceView })),
);
// The company's durable memory. Its own route since it left the settings rail;
// lazy for the same reason its neighbours are — the shell should paint before a
// page nobody has asked for yet is parsed.
const MemoryView = lazy(() =>
  import("@/views/MemoryView").then((m) => ({ default: m.MemoryView })),
);
// The Finance section: Overview (the ledger fold), Invoicing (Chargebee) and
// Wallet (PayPal). Load on demand — its Overview page is Recharts-backed and
// its two provider pages are only reached by an operator who went looking.
const FinanceSection = lazy(() =>
  import("@/views/finance/FinanceSection").then((m) => ({ default: m.FinanceSection })),
);

/**
 * The `h1` a cold visit to `#/finances/<sub>` announces before the chunk lands.
 *
 * On a direct visit — a bookmark, a pasted link — this boundary *is* the whole
 * page for as long as the chunk takes, so its heading is what a screen reader
 * announces. A single "Finances" for every subpage told someone who had opened
 * `#/finances/wallet` that they were somewhere else, and it corrected itself
 * only once the chunk arrived, which is exactly when they no longer needed it.
 *
 * Spelled out here rather than imported from `FinanceSection`: a static import
 * of anything in that module pulls the chunk eagerly and there is no lazy
 * boundary left to name. `the finance fallback names every subpage` holds these
 * to `FINANCE_PAGES`, which a test may import freely.
 */
const FINANCE_FALLBACK_TITLES: Readonly<Record<string, string>> = {
  invoicing: "Invoicing",
  wallet: "Wallet",
};

/** The fallback heading for `#/finances/<sub>`, defaulting to the section. */
export function financeFallbackTitle(sub: string | null): string {
  return (sub && FINANCE_FALLBACK_TITLES[sub]) || "Finances";
}
// Hosts a sandboxed iframe and the postMessage bridge — load on demand, same
// as the other heavier, less-visited surfaces.
const PagesView = lazy(() => import("@/views/PagesView").then((m) => ({ default: m.PagesView })));

// The route table lives in `@/lib/console-routes` — a plain module the unit
// lane can import, and the single place a surface is declared routable (issue
// #1311). Re-exported because the console has always imported `View` from the
// shell that renders those views.
export type { View };

// The nav model and the rows that render it live in
// `@/components/sidebar-navigation` — the four sections, what is filed under
// each, and the sub-navigation the sidebar draws beneath the active one. It is
// a module of its own for the same reason `console-routes.ts` is: the unit lane
// runs with no React plugin, so a table it can import is a table it can pin.

// The console is hash-routed, so a normal `href="#main-content"` would also
// be treated as a route change. Keep the conventional fragment for link
// semantics, then focus this stable landmark without changing the route.
const MAIN_CONTENT_ID = "main-content";

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

const LEGACY_CONNECT_QUERY_KEYS = ["connected", "connect_error", "provider"] as const;

/**
 * Reads a former native OAuth callback's query whether it was appended to the
 * path or, in a bookmarked hash address, to the hash itself.
 *
 * `useHashView` canonicalizes a retired hash before the shell's effects run,
 * so this must happen during the initial render while the fragment query is
 * still present. Path-query values take precedence if an address has both.
 */
function legacyConnectParams(): URLSearchParams {
  const params = new URLSearchParams(window.location.search);
  const [, hashQuery = ""] = window.location.hash.split("?");
  const hashParams = new URLSearchParams(hashQuery);
  for (const key of LEGACY_CONNECT_QUERY_KEYS) {
    if (!params.has(key) && hashParams.has(key)) params.set(key, hashParams.get(key)!);
  }
  return params;
}

/** Removes consumed legacy OAuth callback values without disturbing hash flags. */
function stripLegacyConnectParams(hash: string): string {
  const separator = hash.indexOf("?");
  if (separator === -1) return hash;
  const path = hash.slice(0, separator);
  const params = new URLSearchParams(hash.slice(separator + 1));
  for (const key of LEGACY_CONNECT_QUERY_KEYS) params.delete(key);
  const query = params.toString().replace(/=(?=&|$)/g, "");
  return query ? `${path}?${query}` : path;
}

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
 * How long the onboarding gate's admin check (PR #1875 review finding) waits
 * before retrying a `fetchMe` failure that was not a definitive `401` — a
 * dropped connection or a proxy 5xx, not "this user is not an admin". A few
 * seconds is generous relative to how rarely this fires (a fresh mount's
 * first read, or a genuine network blip) and cheap relative to how bad the
 * alternative is: giving up and reading as non-admin would fail the blocking
 * gate open for an actual admin.
 */
const GATE_ADMIN_CHECK_RETRY_MS = 3000;

/**
 * How long a single `fetchMe` call is allowed to sit with no response at all
 * before it is treated as a failure (PR #1875 review finding).
 *
 * `resolveGateAdminCheckError`/`GATE_ADMIN_CHECK_STUCK_AFTER_FAILURES` only
 * ever run once the call's promise *settles* — one way or the other. `fetchMe`
 * goes through `OpenCompanyClient`, and its request path has no timeout of
 * its own (`api/transport/browser.ts` calls bare `fetch`, no `AbortSignal`),
 * so a stalled proxy or a backend that accepts the connection and then never
 * answers leaves that promise pending forever: no rejection ever reaches the
 * `catch` below, `failures` never increments, and `isGateAdminStuck` never
 * flips even though the admin is exactly as wedged as the retry-forever case
 * three rounds of this file's history already closed. `withReadTimeout` turns
 * that silence into an ordinary rejection at this bound, which
 * `resolveGateAdminCheckError` already classifies as non-terminal — so the
 * existing failure counter below is what actually recovers, this only makes
 * sure it gets the chance to. Long enough that the legitimate "cold host"
 * case (the same class of cost `useActivationGate`'s poll interval doc calls
 * out) is never mistaken for a hang.
 */
const GATE_ADMIN_CHECK_TIMEOUT_MS = 20000;

/**
 * How many consecutive non-settled `fetchMe` failures before the admin check
 * reports itself stuck, mirroring `useActivationGate`'s `STUCK_AFTER_FAILURES`
 * (PR #1875 review finding).
 *
 * That hook's `stuck` only tracks its own `getActivation` reads — it has no
 * way to know the admin check is the one wedged. A durable non-401 `fetchMe`
 * failure (the same class of backend fault: a proxy 5xx, a downstream outage)
 * leaves `isGateAdmin` at `null` forever, which keeps `shouldHoldShellPending`
 * returning `true` (its own `input.isAdmin === null` branch) even while
 * activation itself is reading fine — so `activationGate.stuck` never flips
 * and the recovery affordance below never appears, wedging an admin who
 * cannot reach it behind a loader indistinguishable from the one the
 * activation-side fix already closed. Three failures matches
 * `STUCK_AFTER_FAILURES`'s own ~9s-at-`GATE_ADMIN_CHECK_RETRY_MS` reasoning.
 */
const GATE_ADMIN_CHECK_STUCK_AFTER_FAILURES = 3;

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
  // Every spelling the host folds the company-wide line under (issue #1743).
  //
  // The main line used to be seeded with the first desk's id instead: with no
  // `#general` channel to land in, it was parked on whichever desk sorted
  // first, so it would still be somewhere the operator could find it. That is
  // now actively wrong — an unaddressed message and its reply were rendered in
  // `#engineering`, complete with an unread badge, while the host's own
  // history for that desk was empty.
  //
  // Resolved through `channelIdForThread` rather than answered here, so there
  // is one rule and not two: a blueprint desk grandfathered under a General id
  // owns the line in its own company, and `buildChannels` renders no built-in
  // channel beside it — pointing these spellings at a `main` nothing renders
  // parks live frames and their unread badges where they cannot be opened.
  for (const spelling of ["", MAIN_THREAD_ID, "General", GENERAL_CHANNEL]) {
    const channelId = channelIdForThread(spelling, desks, members);
    if (channelId) map[spelling] = channelId;
  }
  // `dmThreadId`, not `m.id`: a teammate whose id is a General spelling is
  // addressed on `dm:<id>`, and the host emits its live frames under that key.
  // Seeded bare, `channelForThread` could place neither that DM's reply nor its
  // working indicator anywhere at all (issue #1743).
  for (const threadId of [...desks.map((d) => d.id), ...members.map(dmThreadId)]) {
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
  /** Start the New-company flow (issue #1807), owned by `ConnectionConsole`. */
  onCreateCompany?: () => void;
  /**
   * Start the reset (archive + start clean) flow for the given company. Called
   * from Settings → Lifecycle with the active company's id and name.
   */
  onResetCompany?: (id: string, name: string) => void;
}

/** The dashboard shell: sidebar navigation and content around one company's views. */
export function AppShell({
  client,
  company,
  initialStatus,
  companies,
  onSwitchCompany,
  onBackToPicker,
  onCreateCompany,
  onResetCompany,
}: Props) {
  // Which (connection, company) this subtree's browser-local state belongs to.
  const scope = useLocalScope();
  // Room is where the console opens. An empty hash, a bare `#/`, a bookmark
  // whose view was retired — all of them land in the room the operator talks
  // to their company in, rather than on a dashboard about it.
  const [view, sub, navigate] = useHashView<View>(VIEWS, DEFAULT_VIEW, REWRITE_RETIRED);
  const legacyConnectParamsRef = useRef(legacyConnectParams());
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
  /**
   * Whether `SetupController`'s own roster read has landed (PR #1875 review
   * finding, round 12).
   *
   * `setupOpen` starting `true` and a roster read that already landed with
   * the company genuinely unstaffed are indistinguishable to anything that
   * only reads `setupOpen` — `shouldHoldShellPending` needs to tell them
   * apart (see its own doc). `SetupController`'s `onOpenChange` only ever
   * fires once its internal `checked` is true, so its firing at all is
   * itself the signal; `handleSetupOpenChange` below turns that into state
   * the gate predicate can read. A separate flag rather than folding into
   * `setupOpen` itself: `setupOpen` must stay a plain "is setup on screen or
   * blocking" boolean for every other reader (`TourController`'s `hold`,
   * `shouldShowOnboardingGate`), and conflating "resolved" into its value is
   * exactly the bug this fixes.
   */
  const [setupChecked, setSetupChecked] = useState(false);
  const handleSetupOpenChange = useCallback((open: boolean) => {
    setSetupChecked(true);
    setSetupOpen(open);
  }, []);
  /** Set by the Team page's prompt to reopen setup after a skip. */
  const [setupForced, setSetupForced] = useState(false);
  // `#/setup` is an intentional, manual recovery path. It is a route rather
  // than a nav page: setup remains a dialog over the ordinary console, but the
  // address works for staffed companies and after someone has skipped. Entering
  // it forces the dialog open; leaving it (Back, or an edit) hands the dialog
  // back to `SetupController`'s `routeOpen` edge, which closes what the route
  // opened.
  useEffect(() => {
    if (view === "setup") setSetupForced(true);
  }, [view]);
  /**
   * Did this mount start on a view the operator named?
   *
   * Captured once, from the first render's route, so first-run setup can decline
   * to open over a deep link. `useRef(...).current` rather than state: it is a
   * property of how the console was opened and must never change afterwards —
   * the tour drives `view` around, and re-reading it would let a tour step
   * suppress the very dialog that is meant to precede the tour.
   */
  const deepLinked = useRef(view !== DEFAULT_VIEW || Boolean(sub)).current;
  /**
   * Bumped when setup finishes, so the Team page re-reads a roster that now has
   * people on it. A counter rather than a boolean: a second run must re-trigger
   * the read, and a flag that was already `true` would not.
   */
  const [teamBuilt, setTeamBuilt] = useState(0);
  /**
   * Setup has already introduced the console while it builds the team, so do
   * not immediately cover the roster it leads to with the tour welcome.
   */
  const [setupCompleted, setSetupCompleted] = useState(false);
  // A setup that had to sign in hands off with a full-page navigation
  // (`window.location.href`), so `onCompleted` never fires in this mount. The
  // link carries a one-shot marker (`#/company?from=setup`); consume it here so
  // this fresh mount applies the same welcome suppression a same-mount
  // completion gets, and so a reload cannot re-apply it.
  const setupScope = { connection: scope.connection, company };
  useEffect(() => {
    // SetupWizard and the magic-link flow use the unscoped marker because the
    // connection/company may not survive their full-page hand-off. Accept that
    // form only when the marker really carries no scope: a marker scoped to a
    // different connection/company must not suppress setup on this one, nor be
    // cleared before the company it names has consumed it.
    const scoped = arrivedViaSetupHandoff(setupScope);
    const unscoped = !setupHandoffHasScope() && arrivedViaSetupHandoff();
    if (!scoped && !unscoped) return;
    setSetupCompleted(true);
    clearSetupHandoff();
  }, [scope.connection, company]);
  // The shell owns every channel's transcript, not `ChatView` — the shell
  // mounts and unmounts `ChatView` per route, so component-local state there
  // would be discarded on every trip away from Chat and back.
  const [transcripts, setTranscripts] = useState<Transcripts>({});
  // The latest transcripts, readable from the stable `refreshMentions`
  // callback without rebuilding it on every channel that lands a line (the
  // same reason `mentionFeedRef` and `chatChannelByThreadRef` exist).
  const transcriptsRef = useRef(transcripts);
  useEffect(() => {
    transcriptsRef.current = transcripts;
  }, [transcripts]);
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
  // Whether `ChatView`'s transcript is actually rendered right now, as opposed
  // to `activeChatChannelRef` merely still *naming* the channel last shown
  // before the operator dropped to the mobile channel rail. Starts `true` to
  // match `ChatView`'s own initial pane state; kept out of `activeChatChannelRef`
  // because that ref has a second job — addressing an unaddressed system line
  // after a walk to Approvals — that must keep using the last channel even
  // while the rail is what's on screen (#1768 codex review).
  const chatPaneVisibleRef = useRef(true);
  // Which thread panel is open in that channel, or `null` for none (#1890 B).
  //
  // A third condition on "is this completion's marker actually on screen",
  // beside the channel and the pane. Since a card records the thread it was
  // raised in, a threaded settle's marker is folded into its root's replies by
  // `buildTimeline` and does **not** render in the channel timeline — so the
  // channel being open is no longer proof the operator can see it. Without
  // this the toast is suppressed on the channel match and the marker is
  // nowhere: the exact "suppressed a toast for a marker the operator cannot
  // see" defect #1768's review established the rule against.
  const openThreadRootRef = useRef<string | null>(null);
  const onChatPaneVisibilityChange = useCallback((visible: boolean) => {
    chatPaneVisibleRef.current = visible;
  }, []);
  // When each channel was last looked at, and the floor for a channel never
  // looked at. Together with `transcripts` these *derive* the unread counts
  // below — nothing increments a counter, so a message that turns out to be a
  // duplicate cannot leave a badge behind for a line that was never added.
  const [lastViewedChannel, setLastViewedChannel] = useState<Record<string, number>>({});
  const [unreadSince, setUnreadSince] = useState(() => Date.now());
  // A monotonic nonce bumped on every task-lifecycle SSE event, so the
  // company-chat in-flight steer strip (issue #111) and the board itself
  // (issue #464) refetch live.
  //
  // A counter rather than the payload, and that is what makes it safe to share:
  // both consumers re-read their own surface, so two events collapsing into one
  // React batch still means "re-read" — the frame-loss the workflow canvas had
  // to fold an event window to avoid cannot happen to a tick.
  const [taskEventTick, setTaskEventTick] = useState(0);
  /**
   * Board-card state for chat's durable background-work indicator (#1758).
   * Owned here because ChatView unmounts on navigation while the task keeps
   * running, and because the task SSE tick already terminates in this shell.
   */
  const [taskStatusByTaskId, setTaskStatusByTaskId] = useState<
    Record<string, TaskStatus>
  >({});
  /**
   * The same in-flight read, kept whole rather than only as the card-keyed map
   * above. A delegation has no card, so `taskStatusByTaskId` cannot hold it and
   * a surface offering a control over a run needs the rows themselves.
   */
  const [inflightRuns, setInflightRuns] = useState<readonly InflightRun[]>([]);
  const taskStatusRead = useRef(0);
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
  /**
   * Bumped by the events that actually signal a workflow node's agent is
   * working — `workflow_run_started`, `workflow_node_started` and
   * `workflow_node_finished`. Node turns stream no live turn frames
   * (`run_background` publishes nothing), so this tick is fed from the node
   * boundaries rather than from `onTurnEvent`; see `onWorkflowRunEvent`.
   */
  const [backgroundTurnTick, setBackgroundTurnTick] = useState(0);
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
  // Company-scoped for the reason `openTurns` above is: the keys are message ids
  // from one company's journal, and two companies' sequences collide freely.
  useEffect(() => {
    setLiveStepsByMessage((prev) => (Object.keys(prev).length === 0 ? prev : {}));
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
  // The same timeline, per **query** rather than per thread, for a frame that
  // says which operator message its turn answers (`messageSeq`). Keyed by that
  // message's console id, so a running turn's rows render under the question
  // that asked them — the way a settled turn's folded steps already render under
  // its reply.
  //
  // Two turns in one thread is the case this exists for. `liveStepsByThread`
  // holds ONE row-list per thread, and `onSendStart` resets it, so asking a
  // second question destroyed the first turn's rows outright — and a turn
  // blocked on a teammate emits nothing further, so its timeline never came
  // back. Measured on a real pair: the reset discarded two rows from a live
  // delegated turn. Separate keys mean neither turn can clear the other, and
  // the merged pile that would otherwise render is split back into the two
  // questions it came from.
  //
  // Not a replacement: a frame with no `messageSeq` still keys by thread, which
  // is every turn answering no journaled message and every older host.
  const [liveStepsByMessage, setLiveStepsByMessage] = useState<
    Record<string, (TurnStep & { toolCallId?: string })[]>
  >({});
  /**
   * Retires the live rows of every message that now has durable steps of its
   * own, and of every message named in `alsoDrop`.
   *
   * Two cleanup paths meet here because a turn can end two ways.
   *
   * A turn that **answers** journals its folded steps onto the message, so the
   * arrival of those steps is the swap signal — the durable timeline is there
   * to replace the transient one, with no empty frame between them. That is a
   * fact about the message itself, unlike the reply's `parentId`, which names
   * the thread root rather than the question (see `renderAgentReply`).
   *
   * A turn that **fails** journals a `TurnFailed` line and no reply at all, so
   * nothing ever grows steps for it. Its bucket would sit there for the life of
   * the session — quite possibly holding a row still marked `running`, since a
   * result that never arrived cannot flip it. `alsoDrop` is how the terminal
   * settle path retires those (Codex on #2069).
   */
  const clearLiveRowsSettledBy = useCallback(
    (messages: readonly ChatMessage[], alsoDrop: readonly string[] = []) => {
      setLiveStepsByMessage((prev) => {
        const done = new Set(alsoDrop);
        for (const m of messages) if (m.steps && m.steps.length > 0) done.add(m.id);
        let hit = false;
        for (const id of done) if (id in prev) { hit = true; break; }
        if (!hit) return prev;
        const next = { ...prev };
        for (const id of done) delete next[id];
        return next;
      });
    },
    [],
  );
  // The live receipt for each synchronous chat turn in flight (issue #1934),
  // keyed by host thread id — armed on `onSendStart`, bumped by every live
  // frame (which also captures who picked the turn up), and cleared on whichever
  // outcome the POST reaches. Its lifecycle mirrors `liveStepsByThread`'s: the
  // reply landing on `onSendEnd` is what clears it, exactly as the reply bubble
  // is appended, so the two swap with no empty frame between them.
  const [receiptByThread, setReceiptByThread] = useState<Record<string, ChatReceipt>>({});
  // Roster agent id → display name, so the receipt names the teammate rather
  // than rendering a raw id (issue #1934). Populated by the desks/roster read
  // below, which already fetches the roster this is derived from.
  const [agentNames, setAgentNames] = useState<Record<string, string>>({});
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

  /**
   * The account-activation funnel (issue #1844): blocks the shell behind
   * `OnboardingGate` until the company is named, has an integration and has
   * run a workflow — see `useActivationGate` for the polling contract.
   *
   * `gateSkippedThisSession` is read once, into state rather than a plain
   * `const`, so clicking "skip for now" re-renders past the gate without a
   * page reload — `sessionStorage` alone would need one.
   */
  const [gateSkipped, setGateSkipped] = useState(() => gateSkippedThisSession(scope));
  useEffect(() => {
    setGateSkipped(gateSkippedThisSession(scope));
  }, [scope]);
  const skipGate = useCallback(() => {
    markGateSkipped(scope);
    setGateSkipped(true);
  }, [scope]);

  /**
   * Whether the signed-in user is this company's admin (PR #1875 review
   * finding) — `null` until the read lands. Mirrors the `admin =
   * (await fetchMe(...)).role === "admin"` pattern every other admin-gated
   * view in this app already uses (`OAuthView`, `TeamView`, etc.), with two
   * differences, both because this reader feeds a *blocking* gate rather
   * than a read-only view:
   *
   * - The `null` "not yet known" state — see `shouldShowOnboardingGate`'s own
   *   guard for why the gate must never flash open on it.
   * - A failed read is classified through `resolveGateAdminCheckError`
   *   instead of settling straight to `false`. Every other view's `catch {
   *   admin = false }` is safe because the worst case is a control staying
   *   disabled one round trip longer; here `false` is what suppresses the
   *   gate, so a transient failure (a dropped connection, a proxy 5xx) would
   *   read exactly like a real "not an admin" and fail the gate open for an
   *   actual admin for the rest of that mount (PR #1875 review finding,
   *   round 2). Only a definitive `401` settles to `false`; anything else
   *   retries.
   *
   * Declared before `activationGate` (below) because that hook's `enabled`
   * input now reads this state — PR #1875 review finding, round 5.
   */
  const [isGateAdmin, setIsGateAdmin] = useState<boolean | null>(null);
  /**
   * True once `GATE_ADMIN_CHECK_STUCK_AFTER_FAILURES` consecutive `fetchMe`
   * failures have failed to settle — see that constant's own doc. Read
   * alongside `activationGate.stuck` below so the recovery affordance covers
   * either read wedging, not only the activation one.
   */
  const [isGateAdminStuck, setIsGateAdminStuck] = useState(false);
  useEffect(() => {
    let live = true;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    let failures = 0;
    setIsGateAdmin(null);
    setIsGateAdminStuck(false);
    const load = () => {
      void (async () => {
        try {
          const admin =
            (await withReadTimeout(fetchMe(client, company), GATE_ADMIN_CHECK_TIMEOUT_MS)).role ===
            "admin";
          if (!live) return;
          setIsGateAdmin(admin);
          failures = 0;
          setIsGateAdminStuck(false);
        } catch (err) {
          if (!live) return;
          const outcome = resolveGateAdminCheckError(err);
          if (outcome.settled) {
            setIsGateAdmin(outcome.isAdmin);
            failures = 0;
            setIsGateAdminStuck(false);
          } else {
            failures += 1;
            if (failures >= GATE_ADMIN_CHECK_STUCK_AFTER_FAILURES) setIsGateAdminStuck(true);
            retryTimer = setTimeout(load, GATE_ADMIN_CHECK_RETRY_MS);
          }
        }
      })();
    };
    load();
    return () => {
      live = false;
      if (retryTimer !== undefined) clearTimeout(retryTimer);
    };
  }, [client, company]);

  // The poll below is passed `shouldPollActivationForRole(isGateAdmin)`, NOT
  // a bare `true` (PR #1875 review finding, round 5) and NOT `!gateSkipped`
  // (round 4): `GET {scope}/activation` is the only production caller of
  // `compute_and_latch` on the host, so an admin who skips and then finishes
  // the funnel anyway (connects an integration, runs a workflow from the
  // ordinary shell) needs the poll to still be running to ever notice and
  // persist it — see `shouldPollActivation` for that half. Round 5 also tried
  // to stop this poll for a confirmed non-admin, on the premise that no
  // funnel step is reachable by anyone but the admin; round 7 found that
  // premise false (`POST {scope}/workflows/{wid}/run` is `ScopedCompany`, not
  // admin-gated) and reverted it — see `shouldPollActivationForRole`'s own
  // doc for why every role now polls alike. The poll still stops itself once
  // the company is actually activated; nothing here needs to.
  const activationGate = useActivationGate(client, company, shouldPollActivationForRole(isGateAdmin));

  // PR #1875 review finding, round 4: a skip marker from before the funnel
  // completed cannot matter once `isActivated` is true (`shouldShowOnboardingGate`
  // already stops gating on it either way), but leaving it in `sessionStorage`
  // is still a leak worth cleaning up — see `clearGateSkipped`'s own doc.
  useEffect(() => {
    if (activationGate.status?.isActivated) clearGateSkipped(scope);
  }, [activationGate.status?.isActivated, scope]);

  const refreshTaskStatuses = useCallback(async () => {
    const read = ++taskStatusRead.current;
    const [tasks, inflight] = await Promise.allSettled([
      listTasks(client, company),
      listInflight(client, company),
    ]);
    if (read !== taskStatusRead.current) return;
    // Both reads are best-effort, like the board's own decoration read. Keep a
    // last good snapshot when the host is offline instead of making a running
    // pill disappear because one poll failed.
    if (tasks.status === "rejected" && inflight.status === "rejected") return;
    setTaskStatusByTaskId(
      taskStatusesById(
        tasks.status === "fulfilled" ? tasks.value : [],
        inflight.status === "fulfilled" ? inflight.value : [],
      ),
    );
    // Only on a read that actually landed. A failed inflight poll must not empty
    // the bar and take a still-running delegation's cancel with it.
    if (inflight.status === "fulfilled") setInflightRuns(inflight.value);
  }, [client, company]);

  useEffect(() => {
    taskStatusRead.current += 1;
    setTaskStatusByTaskId({});
    // A steer key is company-scoped, so a row held across a company switch
    // would address the previous company's registry.
    setInflightRuns([]);
  }, [client, company]);

  // Ride the existing visible-tab company poll, and also re-read immediately
  // when the task stream reports a dispatch, move or settle (#1758).
  useEffect(() => {
    void refreshTaskStatuses();
  }, [feed.now, taskEventTick, refreshTaskStatuses]);
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
  // in a bookmarked URL. Land the operator on the OAuth page, say what happened,
  // then strip the params so a refresh does not re-fire them. The #838 callback
  // itself now terminates on its explanatory page and never writes a credential.
  // Runs once; StrictMode's double invoke is harmless because the first run
  // clears the params the second reads.
  //
  // The accounts page is `#/connections/apps` since it left the settings rail
  // for the Connections section, so the bounce-back lands there.
  //
  // Before issue #300 the host answered a cancelled or expired handshake with a
  // JSON body, which the browser rendered as the page — a dead end with no way
  // back into the console. Preserve the readable landing for legacy URLs even
  // though #838 no longer redirects new native OAuth callbacks here.
  useEffect(() => {
    const params = legacyConnectParamsRef.current;
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
      window.location.pathname + (query ? `?${query}` : "") + stripLegacyConnectParams(window.location.hash),
    );
    setView("connections", "apps");
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
    let disposeRehydratePolling: (() => void) | undefined;
    const requestCompany = company;
    // Another company's channel ids are another namespace. Drop this one's
    // addressing up front rather than routing the next company's events into
    // channels that no longer exist, and start the unread floor again so the
    // incoming company's rehydrated history isn't counted as news.
    setChatChannelByThread({});
    // Drop the deferred re-read queue with the channel map it was keyed against
    // (issue #1701). A thread parked under the old company must not replay when
    // the new company's map lands — channel ids like `general` collide across
    // companies, so a stale id would fold the wrong thread's history in.
    pendingReReadRef.current.clear();
    // Another company's channels are another namespace here too, and a status
    // carried over would let the incoming company's channels claim to be
    // settled before anything has asked about them.
    setHydration(HISTORY_UNSTARTED);
    setFirstDeskChannelId(null);
    setLastViewedChannel({});
    setUnreadSince(Date.now());
    activeChatChannelRef.current = null;
    openThreadRootRef.current = null;
    // Another company's transcripts are another namespace too: a channel id
    // is this company's desk id or a `dm:<roster-id>`, and a provisioned
    // company is built from the same manifests, so ids recur across
    // companies. A transcript left behind by a switch would paint the
    // previous company's conversation onto the new company's identically
    // named channel — and since the active-DM rail and unread counts derive
    // from `transcripts`, a DM the previous company talked in would look
    // active here before this company's own history has anything to say.
    // Drop them; the hydration below repopulates this company's channels
    // from its own history. The updater returns the same object when there
    // is nothing to drop, so this does not re-render the shell for a no-op.
    setTranscripts((t) => (Object.keys(t).length === 0 ? t : {}));

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
          if (cancelled || requestCompany !== company || markers.length === 0) return;
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
    // Another company's live receipts are another namespace too (issue #1935
    // review, codex 3892523790 / coderabbit 3892517512). Host thread ids like
    // `main` recur across companies, so left uncleared here a switch could
    // render the previous company's still-ticking "Sent"/"Picked up by" row
    // on the new company's identically-named thread until that thread's own
    // next send re-arms it. The generation-guarded `clearReceipt` (see
    // `shouldClearReceipt`) closes the OTHER half of this bug — a late
    // completion from the old company arriving *after* this reset must not
    // delete whatever the new company has since armed — this reset only
    // covers the instant of the switch itself.
    setReceiptByThread((prev) => (Object.keys(prev).length === 0 ? prev : {}));
    // The name map a receipt resolves an agent id through is exactly as
    // company-scoped as the receipts it names (same issue). Cleared here too
    // rather than left to the roster fetch below: that fetch is async, so
    // without this an id captured off a fresh frame on the new company would
    // resolve through the previous company's roster until it answers.
    setAgentNames((prev) => (Object.keys(prev).length === 0 ? prev : {}));

    // How far each channel's cold hydration has got, and which thread's
    // request is still in flight, so the 5-second poll below (`rehydrateAll`
    // reused as both the mount-time call and the recurring one) can tell a
    // cold load from a recovery tick. A cold load marks each channel
    // `"loading"` before its first request — the gap between "this channel
    // exists" and "its history is in flight" is precisely the window the
    // timeline used to fill with the empty-channel copy — and a poll tick
    // must not: the channel is already `"ready"`, and cycling it back through
    // `"loading"` every five seconds is what forced `MessageTimeline` to
    // re-anchor to the bottom on the same cadence (its scroll-to-bottom
    // effect is keyed on `historyPending`), yanking an operator reading
    // scrollback back down every poll even though nothing new arrived.
    //
    // A channel counts as hydrated only once its cold read has *settled*, not
    // when it merely started: a first request still in flight when the timer
    // fires must not be treated as a poll, and a transient failure must not
    // make the next tick fold the whole persisted history in as new tail data.
    const hydratedChannels = new Set<string>();
    const inFlight = new Set<string>();

    // Same status again — a poll tick re-reading history that changed nothing —
    // must not mint a new object: it re-renders every consumer of hydration
    // state on a five-second cadence for no change. Returning `h` unchanged
    // lets React bail out.
    const markHistory = (channelId: string, status: HistoryStatus) =>
      setHydration((h) => {
        if (h.byChannel[channelId] === status) return h;
        return { ...h, byChannel: { ...h.byChannel, [channelId]: status } };
      });

    // One history fetch per thread, fanned into the channels that render it.
    // `transcripts` is keyed by channel id while history is addressed by thread
    // id: a desk's channel id *is* its thread id, and a DM's channel id is the
    // console-local `dmChannelId` while its thread id is the roster agent id
    // (see `ChatView`'s `send`). Fetching per unique thread means a thread
    // rendered by more than one channel is read once, not twice, on every tick
    // (issue #1690).
    const hydrateThread = (threadId: string, channels: readonly { channelId: string }[]) => {
      // Serialize: a tick that fires while the cold read is still in flight
      // does not fire a second request for the same thread (issue #1690).
      if (inFlight.has(threadId)) return;
      inFlight.add(threadId);
      channels.forEach(({ channelId }) => {
        if (!hydratedChannels.has(channelId)) markHistory(channelId, "loading");
      });
      client
        .getChatHistory(threadId, company)
        .then((entries) => {
          if (cancelled || requestCompany !== company) return;
          const hydrated = fromHistory(entries);
          // Any message that came back carrying steps has a durable timeline
          // now, so its transient one is spent. Covers the ordinary success
          // swap, and re-converges a console that reloaded mid-turn.
          clearLiveRowsSettledBy(hydrated);
          if (hydrated.length > 0) {
            // Persisted rows take the history's own oldest-first order, and
            // local rows the host has not persisted yet stay at the tail — so
            // a row the live SSE path missed lands where the host says it
            // belongs, gap or tail (issue #1690). Durable rows outside the
            // newest page remain in their existing prefix, while only
            // browser-local rows are tail optimistic sends.
            channels.forEach(({ channelId }) => {
              setTranscripts((t) => {
                const merged = mergeHistoryInOrder(t[channelId] ?? [], hydrated);
                return merged === (t[channelId] ?? []) ? t : { ...t, [channelId]: merged };
              });
            });
          }
          channels.forEach(({ channelId }) => markHistory(channelId, "ready"));
        })
        .catch(() => {
          /* host without `/chat/history`, or offline — stores stay as they are */
          if (!cancelled) channels.forEach(({ channelId }) => markHistory(channelId, "ready"));
        })
        .finally(() => {
          inFlight.delete(threadId);
          // Settled — success or failure — is the moment a channel stops being
          // a cold load and starts being a poll target. Not the moment the
          // request was *sent* (issue #1690).
          channels.forEach(({ channelId }) => hydratedChannels.add(channelId));
        });
    };

    const rehydrateTargets = (
      threadIds: readonly string[],
      channels: readonly { channelId: string; threadId: string }[],
    ) => {
      const channelsByThread = new Map<string, { channelId: string }[]>();
      for (const { channelId, threadId } of channels) {
        const list = channelsByThread.get(threadId);
        if (list) list.push({ channelId });
        else channelsByThread.set(threadId, [{ channelId }]);
      }
      // Every resolved thread gets its own fetch, even when nothing renders as
      // a channel — the main line is a thread with no Chat channel. And every
      // channel's backing thread is in `threadIds`, so the union is the full
      // set, each exactly once (issue #1690).
      [...new Set([...threadIds, ...channelsByThread.keys()])].forEach((threadId) =>
        hydrateThread(threadId, channelsByThread.get(threadId) ?? []),
      );
    };

    Promise.all([
      client.listDesks(company).catch(() => null),
      // The always-present Operator feed's identity (issue #1757 rework) —
      // fetched alongside desks, not derived from them, since it is its own
      // surface now. `null` on any failure (offline, or a host that predates
      // the route) rather than sinking the whole pass: a company can still
      // rehydrate its real desks/DMs without the pinned Operator row.
      //
      // One retry (issue #1781 review, Codex P2): `ChatView` fetches this
      // same identity independently for rendering the pinned row, so a
      // single dropped request here — while `ChatView`'s own, later call
      // succeeds — used to render the row but permanently omit its id from
      // this pass's rehydration targets and five-second polling, since this
      // pass had already given up. A bounded retry closes the common
      // transient case without turning the fetch into an open-ended one; see
      // `fetchWithOneRetry`'s doc for why it is extracted rather than inline.
      fetchWithOneRetry(() => client.getOperatorChannel(company)),
    ])
      .then(async ([desks, operatorChannelRaw]) => {
        // See `isOperatorChannelDto`'s doc comment — a client stub that
        // resolves every unlisted method to `[]` would otherwise satisfy the
        // `Promise.all` type and reach the field reads below.
        const operatorChannel = isOperatorChannelDto(operatorChannelRaw)
          ? operatorChannelRaw
          : null;
        if (cancelled || requestCompany !== company) return;
        // Issue #151 §3.3: desks first, then one DM thread per roster teammate.
        // The roster is fetched separately and tolerated as optional — a host
        // that 404s `/team` keeps its desks rather than losing the whole list.
        const team = await client.listTeam(company).catch(() => []);
        if (cancelled) return;
        const deskThreads = desks === null ? defaultThreads() : threadsFromDesks(desks);
        const resolved = [
          ...deskThreads,
          ...agentDmThreads(
            team,
            deskThreads.map((t) => t.id),
          ),
        ];
        // The host answered, so this is the company's desk list — empty
        // included. `defaultDesks()` stands in only when `listDesks` itself
        // failed (`desks === null`, from the `.catch(() => null)` above); a
        // company that simply declares no `[[group_chat]]` used to be given
        // three fabricated ones here.
        const chatDesks = desks === null ? defaultDesks() : desks.map(deskFromDto);
        const roster = team.map(fromDto);
        // The name map the live receipt resolves an agent id through (issue
        // #1934) — derived from the roster this effect already read, so it costs
        // no extra request and is scoped to the company the effect ran for.
        setAgentNames(Object.fromEntries(roster.map((m) => [m.id, m.name])));
        // Keep the addressing this loop resolves, not just its side effect.
        //
        // The Operator feed's id is folded in here too (issue #1781 review,
        // Codex P2): `channelMap` only knows desks and roster teammates, so
        // without this the map a **live** SSE frame is resolved through
        // (`channelForThread(chatChannelByThread, event.chatId)`, a few
        // hundred lines below) missed the Operator channel entirely and
        // dropped the frame — `renderAgentReply` returns on the very next
        // line when the lookup misses. The five-second history poll still
        // recovered it eventually, because the `channels` rehydration-target
        // list a little further down already carries this same id→id pair;
        // this closes the live-event gap the poll was quietly papering over.
        setChatChannelByThread({
          ...channelMap(chatDesks, roster),
          ...(operatorChannel ? { [operatorChannel.id]: operatorChannel.id } : {}),
        });
        // The channel `ChatView` lands on when the hash names none, which since
        // issue #1743 is the built-in `#general` rather than the first desk —
        // the two must agree, or a line with nowhere else to go lands in a
        // channel the operator is not looking at. Resolved rather than
        // hard-coded, for the reason `generalChannelId` gives: a grandfathered
        // blueprint desk owns the line in its own company, and `main` is then
        // not a channel at all.
        setFirstDeskChannelId(channelIdForThread(MAIN_THREAD_ID, chatDesks, roster));
        // Fold the Operator feed's id into the same rehydration pass, keyed on
        // its own id both as channel and thread (its channel id *is* its
        // thread id — `chat/history?desk=<id>` reads it through the ordinary
        // path). Without this, `ChatView`'s pinned row would sit on a channel
        // id `historyReady` never sees a status for until `discovered` alone
        // resolves it, and `transcripts[operatorChannel.id]` would never fill
        // in — the spinner-forever failure mode this pass exists to avoid.
        const threadIds = [
          ...resolved.map((t) => t.id),
          ...(operatorChannel ? [operatorChannel.id] : []),
        ];
        const channels = [
          // `#general` is not in the desk list (it is not a desk), so its
          // history has to be named here or nothing would rehydrate it on
          // reload — the one channel every company has would come back empty.
          {
            channelId: channelIdForThread(MAIN_THREAD_ID, chatDesks, roster) ?? MAIN_THREAD_ID,
            threadId: MAIN_THREAD_ID,
          },
          ...chatDesks.map((d) => ({ channelId: d.id, threadId: d.id })),
          // A DM's history is fetched under the teammate's **own id** — but
          // that id is not always this DM's address. A manifest may declare a
          // teammate whose id is a General spelling (`mint_agent_id` reserves
          // `main` and `General`, but a blueprint is not something this console
          // overrules), and `GET chat/history?desk=main` then returns the
          // *folded General conversation*, not that teammate's transcript:
          // `is_general_chat` has folded `""`, `main`, `General` and `general`
          // into one conversation since issue #65. Naming `dm:<id>` as its
          // channel therefore poured the company-wide line into that DM on
          // every reload.
          //
          // Resolved through `channelIdForThread` so the one rule that decides
          // where a thread renders decides it here too (issue #1743). For every
          // ordinary teammate that is exactly `dm:<id>`, unchanged.
          ...roster.map((m) => ({
            channelId: channelIdForThread(dmThreadId(m), chatDesks, roster) ?? dmChannelId(m),
            // The address the DM is actually written under. Bare, this fetched
            // the folded General history for a teammate whose id is a General
            // spelling, so its own transcript could never be recovered after a
            // reload (issue #1743).
            threadId: dmThreadId(m),
          })),
          ...(operatorChannel
            ? [{ channelId: operatorChannel.id, threadId: operatorChannel.id }]
            : []),
        ];
        const rehydrateAll = () => rehydrateTargets(threadIds, channels);
        // SSE remains the fast path. This catches a persisted channel message
        // whose live frame arrived during a disconnect or before its thread
        // mapping existed, and pauses automatically while the tab is hidden.
        rehydrateAll();
        disposeRehydratePolling = startVisiblePolling(rehydrateAll, 5000);
        // Every channel this pass will hydrate now has a status, so a channel
        // with none is one nothing is coming for.
        setHydration((h) => ({ ...h, discovered: true }));
      })
      .catch(() => {
        // Last-resort safety net: `listDesks`/`getOperatorChannel` already
        // degrade to `null` on their own failure above, so this only fires on
        // something unexpected inside the `.then` (e.g. a state setter
        // throwing) — keep the static default threads so the console still
        // renders something rather than getting stuck.
        if (cancelled || requestCompany !== company) return;
        const fallbackDesks = defaultDesks();
        // No roster answered, so no agent names for this company — clear rather
        // than carry the previous company's map into a receipt here (#1934).
        setAgentNames({});
        setChatChannelByThread(channelMap(fallbackDesks, []));
        // `MAIN_THREAD_ID`, not `fallbackDesks[0]?.id`: `defaultDesks()` no
        // longer carries a fabricated `main` row (issue #1743), so the first
        // fallback desk is just whichever one sorts first — landing the
        // console on an arbitrary desk on an unexpected error instead of the
        // company-wide line every other path opens on (issue #1781 review,
        // Codex P2/medium).
        setFirstDeskChannelId(MAIN_THREAD_ID);
        const threadIds = defaultThreads().map((t) => t.id);
        const channels = [
          // `#general` is not a desk here either — same reason the success
          // path above names it explicitly. Without this entry `mainThread()`
          // is still in `threadIds` (via `defaultThreads()`) but has no
          // channel to rehydrate history through.
          { channelId: MAIN_THREAD_ID, threadId: MAIN_THREAD_ID },
          ...fallbackDesks.map((d) => ({ channelId: d.id, threadId: d.id })),
        ];
        const rehydrateAll = () => rehydrateTargets(threadIds, channels);
        rehydrateAll();
        disposeRehydratePolling = startVisiblePolling(rehydrateAll, 5000);
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
      disposeRehydratePolling?.();
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
  // Thread ids whose turn settled before `chatChannelByThread` knew their
  // channel, parked for replay once it does (issue #1701). A ref, not state:
  // it must survive renders without itself provoking one, and the drain that
  // reads it is triggered by the channel map landing, not by this set changing.
  const pendingReReadRef = useRef<Map<string, PendingReRead>>(new Map());
  // Mirrors `chatChannelByThread` so `reReadSettledThread`'s `.then()` always
  // reads the map's current value instead of the one closed over when the
  // request started (issue #1701 follow-up). `reReadSettledThread` is
  // recreated whenever `chatChannelByThread` changes — if a `getChatHistory`
  // response lands *after* the map-populating render but its callback closure
  // predates that render (the request started while the map was still
  // empty), reading state directly would see the stale empty map even though
  // the drain effect below already ran with the fresh one — parking the
  // thread with nothing left to trigger its replay. Reading this ref instead
  // means the response always sees whatever the map holds *now*.
  const chatChannelByThreadRef = useRef(chatChannelByThread);
  useEffect(() => {
    chatChannelByThreadRef.current = chatChannelByThread;
  }, [chatChannelByThread]);
  /**
   * Mirrors `openTurns` for the same reason `chatChannelByThreadRef` mirrors
   * its map: `reReadSettledThread` needs the value as of *when its response
   * lands*, not as of when the request started.
   *
   * What it guards is the live-row clear below. `settle` drops only the turn
   * that ended and deliberately keeps a queued sibling watched, so a thread
   * with more work still has an entry here — and a thread-wide clear that
   * ignored it would delete the rows of a turn that is still running, on the
   * wide window a history round trip opens (PR #1904 review).
   */
  const openTurnsRef = useRef(openTurns);
  useEffect(() => {
    openTurnsRef.current = openTurns;
  }, [openTurns]);
  // The latest full browser scope, so async completions cannot cross either a
  // company switch or an in-place connection reconfiguration. `client` is part
  // of the scope: `reseat` edits a host address by swapping the client while
  // preserving the connection id, so connection+company alone do not move on
  // reconfiguration — only the client instance does.
  const scopeRef = useRef({ connection: scope.connection, company, client });
  useEffect(() => {
    scopeRef.current = { connection: scope.connection, company, client };
  }, [scope.connection, company, client]);
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
    (threadId: string, settledTurnId?: string, stateKey?: string) => {
      // Two identities, and after #2042 they are no longer the same string.
      // `threadId` is the **desk** — what `chat/history`, the `threads` fold and
      // `channelForThread` are addressed by. `liveKey` is the **open-turn state
      // key**, which is what `openTurns`, `liveStepsByThread` and
      // `receiptByThread` are keyed by, because `ChatView` hands `onSendStart`
      // its `stateKey` and that key is `engineering#41` for a threaded send.
      //
      // Conflating them breaks one side or the other: reading the desk out of
      // the map misses a queued sibling and leaves a threaded turn's receipt
      // ticking forever, and asking the host about the composite recovers no
      // history at all (Codex review on #2044).
      const liveKey = stateKey ?? threadId;
      client
        .getChatHistory(threadId, company)
        .then((entries) => {
          if (!mountedRef.current || entries.length === 0) return;
          // A company switch while the re-read was in flight invalidates the
          // result: the messages belong to the old company and must not
          // repopulate the new company's (just-cleared) transcripts. The
          // re-read is recreated when `company` changes, so the closure's
          // `company` is the scope it started for and `scopeRef` is where
          // the current connection/company scope landed.
          if (
            scopeRef.current.company !== company ||
            scopeRef.current.connection !== scope.connection ||
            scopeRef.current.client !== client
          ) return;
          const hydrated = fromHistory(entries);
          // The turn is over, so its transient tool rows have served their
          // purpose — the durable record just folded in is what stands now.
          //
          // `injectAgentReply` does this for a turn that *answered*, and for a
          // long time that covered everything a console would see. A turn that
          // settles **failed** journals a `TurnFailed` line and emits no
          // `agent_reply`, so nothing cleared its rows: a detached POST has
          // already resolved, `onSendEnd` has already run, and the live
          // timeline sat under the channel claiming work was in flight until
          // the next send or a reload (PR #1904 review). Harmless while ACP
          // published no rows at all; not harmless now that it does.
          //
          // …but only when the thread has nothing else running. A thread can
          // hold several detached turns, and `settle` keeps the queued ones
          // watched; clearing unconditionally would wipe a *newer* turn's rows
          // whenever its frames arrived while this history read was in flight,
          // which on a round trip is a wide window. The newer turn's own
          // settle clears them when it gets there.
          if (!hasOtherOpenTurns(openTurnsRef.current, liveKey, settledTurnId)) {
            setLiveStepsByThread((prev) =>
              prev[liveKey]?.length ? { ...prev, [liveKey]: [] } : prev,
            );
            // The receipt the detached turn carried through its queued/working
            // window (issue #2021) is cleared on the same terminal transition
            // that clears the live rows, under the same guard: a queued sibling
            // still running keeps it, and this only runs once the scope checks
            // above confirm the settle belongs to the company on screen — so a
            // late cross-company settle cannot delete a newer company's receipt.
            setReceiptByThread((prev) => {
              if (!(liveKey in prev)) return prev;
              const next = { ...prev };
              delete next[liveKey];
              return next;
            });
            // The per-query buckets retire on the same transition, inside the
            // same guard and for the same reason: a queued sibling still
            // running owns its rows, and this must not take them.
            //
            // Every message here, not only those carrying steps — which is what
            // covers a turn that FAILED. It journals a `TurnFailed` line and no
            // reply, so it never grows durable steps to swap for, and its bucket
            // would otherwise hold a row marked `running` for the whole session
            // (Codex on #2069).
            clearLiveRowsSettledBy(hydrated, hydrated.map((m) => m.id));
          }
          const channelId = channelForThread(chatChannelByThreadRef.current, threadId);
          // The thread settled before the desks/roster effect populated its
          // channel id — on a cold load, or the moment after a company switch
          // (issue #1701). Park the id so the drain effect replays the
          // transcript fold once the map lands, rather than dropping it and
          // leaving the Chat panel stale.
          if (!channelId) {
            // Both identities, not just the desk: the replay re-runs the
            // cleanup above, and that cleanup is filed under the state key. A
            // desk-only replay clears whatever sits under the desk — which on
            // a cold load can be a live unthreaded send's own live steps and
            // receipt, armed before its `openTurns` row landed (Codex on
            // #2044). Keyed by the pair so two threads of one desk park
            // separately rather than collapsing onto one entry.
            pendingReReadRef.current.set(`${liveKey}\u0000${settledTurnId ?? ""}`, {
              desk: threadId,
              stateKey: liveKey,
              turnId: settledTurnId,
            });
            return;
          }
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
    // Deliberately excludes `chatChannelByThread`: the callback reads the map
    // through `chatChannelByThreadRef` (always current) instead, so its
    // identity no longer churns on every map update — see the ref's doc above.
    [client, company],
  );

  // Replay any thread parked by the branch above once its channel becomes
  // known (issue #1701). Fires when the desks/roster effect populates
  // `chatChannelByThread` — the exact edge that a cold-load or post-switch
  // settle was waiting on. Deliberately keyed on the channel map and the
  // callback only: `transcripts`/`threads` are written by the replay itself,
  // so depending on them would loop.
  useEffect(() => {
    drainReReadQueue(pendingReReadRef.current, chatChannelByThread, reReadSettledThread);
  }, [chatChannelByThread, reReadSettledThread]);

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

    // `stateKey` prunes the map; `chatId` is the desk the re-read talks to.
    // They are different strings for a threaded turn — the map is keyed
    // `engineering#41` while the desk is `engineering` — and asking the host
    // for the composite recovers nothing at all (Codex review on #2042).
    const settle = (stateKey: string, chatId: string, turnId: string) => {
      setOpenTurns((prev) => {
        const turns = prev[stateKey];
        if (!turns) return prev;
        // Drop just this turn; a queued sibling behind it stays watched, so
        // its reply is still delivered when it settles in turn.
        const rest = turns.filter((t) => t.turnId !== turnId);
        const next = { ...prev };
        if (rest.length) next[stateKey] = rest;
        else delete next[stateKey];
        return next;
      });
      // Deliberately not awaited here, and deliberately not written inline —
      // see `reReadSettledThread` for why the re-read cannot live inside this
      // effect. The line above is what tears this effect down.
      //
      // The turn id goes with it: the re-read's own clear must not be fooled
      // by a ref that has not caught up with the `setOpenTurns` above.
      //
      // Both identities, and both required. The desk is what `getChatHistory`
      // is addressed by — a composite key names no desk the host knows — and
      // the state key is what the per-turn cleanup is filed under. `chatId` is
      // non-optional on `OpenTurn`, so this cannot silently lose the desk the
      // way a derived fallback did.
      reReadSettledThread(chatId, turnId, stateKey);
    };

    const poll = () => {
      for (const [stateKey, turn] of watching) {
        if (!turn.turnId) continue;
        getRun(client, company, turn.turnId)
          .then(({ run }) => {
            if (cancelled) return;
            if (run.phase === "terminal") {
              settle(stateKey, turn.chatId, turn.turnId!);
              return;
            }
            // Still open: keep the queued/working distinction honest. `pending`
            // means it has not taken the per-company lock yet.
            const queued = run.status === "pending";
            setOpenTurns((prev) =>
              prev[stateKey]?.some((t) => t.turnId === turn.turnId && t.queued !== queued)
                ? {
                    ...prev,
                    [stateKey]: prev[stateKey].map((t) =>
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
              settle(stateKey, turn.chatId, turn.turnId);
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
  /**
   * This person's unread mentions.
   *
   * Polled rather than streamed, deliberately: the company SSE feed has **no
   * per-viewer projection**, which is the documented reason `ReactionToggled`
   * is dropped from it entirely — a mention frame would have to carry either
   * everyone's user ids or nobody's. So the feed is refetched on the same
   * cadence the console already polls, on each reply, and on window focus.
   *
   * A host without the route leaves this empty, so no mention badges render and
   * nothing else changes.
   */
  const [mentionFeed, setMentionFeed] = useState<NotificationDto[]>([]);
  const mentionFeedRevision = useRef(0);
  const mentionFeedVersion = mentionFeedRevision.current;
  // Mention subject ids this session has already asked the shell to re-read a
  // thread for. One re-read per mention, not one per poll: a re-read that
  // fails (offline) is retried by the next reload rather than hammered.
  const mentionReReadSubjectsRef = useRef<Set<string>>(new Set());
  // Ids of non-mention (`dispatch_failed` / `approval_expired` /
  // `workflow_run_*`) rows this session has already toasted. These rows come
  // back on every poll until marked read server-side, so this local guard is
  // what keeps a single dispatch failure from toasting once per interval
  // instead of once — see `@/lib/operational-notifications`.
  const operationalAnnouncedRef = useRef<Set<string>>(new Set());
  // Toasted operational ids waiting for the tab to become visible before the
  // server-side ack fires (Codex #1883 P2). See
  // `scheduleAcknowledgement`/`flushPendingAcknowledgements`.
  const pendingAckRef = useRef<PendingAcknowledgement[]>([]);
  const refreshMentions = useCallback(() => {
    const requestCompany = company;
    const revision = ++mentionFeedRevision.current;
    void client
      .notifications(requestCompany)
      // A host that answers this route with something other than the documented
      // shape must not take the console down with it. `?? []` rather than a
      // trusted `feed.notifications`: an older or proxied host can return a bare
      // array, or `null`, and iterating that throws during render — which blanks
      // the whole app, not just the badge. The badge is the least important
      // thing on the screen and must fail like it.
      .then((feed) => {
        if (
          revision !== mentionFeedRevision.current ||
          requestCompany !== scopeRef.current.company
        )
          return;
        const next = Array.isArray(feed?.notifications) ? feed.notifications : [];
        setMentionFeed(next);
        // A mention posted by another operator never reaches this tab through
        // SSE (`OperatorMessage` is dropped from the projection), and the
        // transcripts are otherwise re-read only when a turn this tab is
        // *watching* settles — a turn another operator's message opened is not
        // one it watches. So a newly polled mention whose message is absent
        // from the loaded transcript would leave its badged channel with
        // nothing to show and the `loadedMessageIds` gate unable to clear it
        // (Codex). Re-read the host thread so the mentioned message lands.
        const loadedByChannel: Record<string, ReadonlySet<string>> = {};
        for (const [channelId, rows] of Object.entries(transcriptsRef.current)) {
          loadedByChannel[channelId] = new Set(rows.map((m) => m.id));
        }
        const { threadIds, subjects } = threadsToReReadForMentions(
          next,
          loadedByChannel,
          chatChannelByThreadRef.current,
          firstDeskChannelId ?? undefined,
          mentionReReadSubjectsRef.current,
        );
        if (threadIds.length > 0) {
          // Guard first, then re-read: the fold `reReadSettledThread` runs is
          // idempotent, but the poll that noticed the mention keeps firing, so
          // without the guard every tick would re-read the same threads.
          subjects.forEach((s) => mentionReReadSubjectsRef.current.add(s));
          threadIds.forEach((threadId) => reReadSettledThread(threadId));
        }
        // `dispatch_failed` / `approval_expired` / `workflow_run_*` rows go
        // through this same durable feed but are not mentions, so nothing
        // above ever renders or acknowledges them — they would sit "unread"
        // forever despite coming back on every poll (Codex #1883 P1). A toast
        // is this feed's minimal rendering. The row is marked read once the
        // toast has actually been SEEN, not the instant it is enqueued
        // (Codex #1883 P2 fallout): sonner still renders a toast raised in a
        // hidden tab (only `toast-lifetime.ts`'s auto-dismiss clock pauses for
        // one), so an immediate ack survived even a tab closed/reloaded before
        // the operator ever returned to see it — the row reads as handled and
        // nobody saw it, defeating the point of this consumer.
        const toAnnounce = operationalNotificationsToAnnounce(
          next,
          operationalAnnouncedRef.current,
        );
        if (toAnnounce.length > 0) {
          const ids = toAnnounce.map((n) => n.id);
          // Added the instant a row is toasted, hidden tab or not — this is
          // what stops a still-unacknowledged row from being re-toasted on
          // the next poll, independent of when (or whether) the server-side
          // ack below fires.
          ids.forEach((id) => operationalAnnouncedRef.current.add(id));
          for (const n of toAnnounce) {
            if (operationalNotificationSeverity(n) === "error") toast.error(n.title);
            else toast.warning(n.title);
          }
          setMentionFeed((current) =>
            current.map((n) => (ids.includes(n.id) ? { ...n, readAt: Date.now() } : n)),
          );
          const { ackNow, pending } = scheduleAcknowledgement(
            ids,
            requestCompany,
            document.hidden,
            pendingAckRef.current,
          );
          pendingAckRef.current = pending;
          if (ackNow.length > 0) {
            void client.markNotificationsRead(ackNow, requestCompany).catch(() => {
              // A failed mark-read leaves the row unread server-side; the next
              // poll re-fetches it, finds it still in `readAt: undefined`, but
              // `operationalAnnouncedRef` has already seen its id, so it is not
              // re-toasted. The row itself is not lost — it is still durable
              // and still returned — only the toast is best-effort, matching
              // how mention marking already treats offline/older-host failure.
            });
          }
        }
      })
      .catch(() => {
        // A transient refresh failure must not erase the last successful feed:
        // keeping it is safer than making durable unread mentions disappear.
        // The next successful refresh reconciles the optimistic snapshot.
      });
  }, [client, company, firstDeskChannelId, reReadSettledThread]);

  useEffect(() => {
    mentionFeedRevision.current++;
    mentionReReadSubjectsRef.current = new Set();
    setMentionFeed([]);
    refreshMentions();
    const onFocus = () => refreshMentions();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refreshMentions]);

  // Ride the existing company poll rather than adding a second timer.
  useEffect(() => {
    refreshMentions();
  }, [feed.now, refreshMentions]);

  // The other half of the deferred ack above: flush whatever was toasted
  // while the tab was hidden the moment it is actually seen (Codex #1883
  // P2). `scopeRef.current.company`, not the `company` prop, so this effect
  // does not need to resubscribe on every company switch — it only needs the
  // value at the instant visibility flips.
  useEffect(() => {
    const onVisibilityChange = () => {
      if (document.visibilityState !== "visible") return;
      const { ackNow, pending } = flushPendingAcknowledgements(
        scopeRef.current.company,
        pendingAckRef.current,
      );
      pendingAckRef.current = pending;
      if (ackNow.length > 0) {
        void client.markNotificationsRead(ackNow, scopeRef.current.company).catch(() => {
          // Same best-effort contract as the immediate path above — a failed
          // flush leaves the rows unread server-side, re-fetched (but not
          // re-toasted, `operationalAnnouncedRef` already has their ids) on
          // the next poll.
        });
      }
    };
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => document.removeEventListener("visibilitychange", onVisibilityChange);
  }, [client]);

  const mentionCounts = useMemo(() => {
    // `main` may be undefined while the desks/roster effect has not resolved —
    // passing a fabricated `""` would file every legacy "General"/"main"
    // mention under a channel the rail never has, invisible and unclearable.
    // The lib drops those rows when there is no rendered main channel.
    return mentionCountsByChannel(
      mentionFeed,
      firstDeskChannelId ?? undefined,
      new Set(Object.values(chatChannelByThread)),
    );
  }, [mentionFeed, firstDeskChannelId, chatChannelByThread]);
  const mentionFeedRef = useRef(mentionFeed);
  mentionFeedRef.current = mentionFeed;
  /**
   * The same feed, readable from a callback that must not be rebuilt when it
   * changes.
   *
   * `onChannelViewed` is handed to `ChatView` and is deliberately stable — it
   * is called on every channel view and on every transcript growth, and adding
   * the feed to its dependencies would rebuild it on every poll. But it also
   * has to clear *this* channel's mentions, which means reading the current
   * feed. A ref is how both hold: the callback stays stable and still sees the
   * latest value, instead of capturing the empty array it was created with.
   */
  const onChannelViewed = useCallback(
    (
      channelId: string,
      historyPending: boolean,
      mentionFeedRevision?: number,
      replyParents?: ReadonlyMap<string, string>,
      openThreadId?: string | null,
      loadedMessageIds?: ReadonlySet<string>,
      advanceChannelRead = true,
    ) => {
      activeChatChannelRef.current = channelId;
      // #1890 B. `ChatView` re-reports on every open/close (its effect lists
      // `openThreadId`), so this ref tracks the panel rather than lagging it.
      openThreadRootRef.current = openThreadId ?? null;
      if (mentionFeedRevision === undefined) return;
      // Clear only THIS channel's mentions, and only once its history is
      // actually on screen. A mention is durable and there is no
      // older-history pagination to recover one, so clearing it before the
      // named message has loaded — or while hydration is still failing —
      // would lose the summons for good with nothing left to notice it by.
      // The effect above re-fires once `historyPending` goes false, so this
      // simply waits rather than dropping the clear.
      const clearing = historyPending
        ? []
        : mentionsToClear(
            mentionFeedRef.current,
            channelId,
            // Same undefined-means-none signal as the count memo: with no
            // rendered main channel the general-chat arm matches nothing.
            firstDeskChannelId ?? undefined,
            new Set(
              Object.keys(chatChannelByThread).filter(
                (threadId) => chatChannelByThread[threadId] === channelId,
              ),
            ),
            new Set(Object.values(chatChannelByThread)),
            replyParents ?? new Map(),
            openThreadId ?? null,
            loadedMessageIds,
          );
      if (clearing.length > 0) {
        // Optimistic, so the badge goes at once; the next poll reconciles.
        setMentionFeed((current) =>
          current.map((n) =>
            clearing.includes(n.id) ? { ...n, readAt: Date.now() } : n,
          ),
        );
        void client.markNotificationsRead(clearing, company).catch(() => {
          // Older host, or offline. The next poll restores the true state
          // rather than leaving a badge permanently wrong.
          refreshMentions();
        });
      }
      // A Conversation thread view that aliases `main` onto a real desk shows
      // the legacy General transcript, not the desk's own (see `onThreadViewed`).
      // The mention clear above is still owed — the thread's loaded ids prove
      // the summoning message is on screen — but the channel-read side effects
      // are not: advancing the desk's floor here would permanently un-badge
      // unread lines the operator never saw.
      if (!advanceChannelRead) return;
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
    // The whole map, not just `.main`: the callback reads every key and value
    // (`Object.keys`/`Object.values` for the rendered and visible thread sets),
    // and a company switch whose first desk id matches the previous company's
    // leaves `.main` unchanged while the rest of the map — the DM channels for
    // a different roster — moves. Rebuilding the callback on the whole map is
    // cheap: it changes on desk load and company switch, never per poll (the
    // `mentionFeedRef` comment above is what keeps the *feed* out of the deps).
    [scope, client, company, chatChannelByThread],
  );

  /**
   * Approval decisions and other unaddressed lines land in a transcript rather
   * than vanishing: Chat appends the line to a channel. The shell owns
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
    // Through `channelForThread`, not a bare index: the host accepts any casing
    // of a General spelling and echoes back the one the caller used, so a map
    // of four literals misses `MAIN` from an API client (issue #1743).
    const target = threadId ? (channelForThread(chatChannelByThread, threadId) ?? undefined) : undefined;
    if (!target) {
      noteSystem(line);
      return;
    }
    setTranscripts((t) => ({
      ...t,
      [target]: [...(t[target] ?? []), makeMessage("system", line)],
    }));
  };

  // Render one `AgentReply` (issue #66) into the Chat workspace's transcripts.
  //
  // Split out from {@link injectAgentReply} so a frame `PendingSyncPosts` held
  // back (issue #983) can be rendered from the same code once its thread's POST
  // resolves, instead of the shell needing a second copy of this logic.
  const renderAgentReply = useCallback(
    (event: AgentReplyEvent) => {
      // The event names a thread; `chatChannelByThread` is the only thing that
      // knows which channel renders it. An id no channel owns is a no-op:
      // better silent than in the wrong place.
      //
      // `channelForThread`, for the reason `noteInChannel` gives: the map holds
      // four literal General spellings and the host echoes whatever casing the
      // caller addressed, so a bare index drops the live reply and it appears
      // only when polling recovers the durable history (issue #1743).
      const channelId = channelForThread(chatChannelByThread, event.chatId);
      if (!channelId) return;
      // This turn's answer is here, carrying the authoritative folded steps, so
      // the live rows filed under the question it answers have done their job.
      // NOT keyed off `event.parentId`. That is the reply's *placement* parent,
      // and `AcceptedTurn::thread_root` is explicit that "a reply is parented to
      // its question's parent, never to the question" — so for a follow-up typed
      // inside a thread it names the thread ROOT. Clearing by it would leave the
      // follow-up's own rows resident and, far worse, delete the root's bucket:
      // if the root's turn were still running this would erase a live sibling's
      // timeline, which is the exact failure this whole change exists to stop
      // (Codex on #2069).
      //
      // The swap is driven by the durable steps instead — see
      // `clearLiveRowsSettledBy` below, which retires a bucket once the message
      // it belongs to has real steps to render, and the terminal settle path,
      // which covers a turn that failed and so journals no reply at all.
      setTranscripts((t) => {
        const existing = t[channelId] ?? [];
        // The same recent-tail content dedupe the thread store uses. It still
        // earns its place: the operator's own turn is rendered locally by the
        // awaited POST under an ephemeral `m<seq>` id, so a late echo of that
        // reply can only be matched by content.
        //
        // It is no longer the ONLY guard, and issue #483 is why. This line now
        // carries the host's id (below), so `mergeHistoryInOrder`'s id dedupe
        // can recognise it — which the content check could never do from the
        // other side, because hydration folds the persisted rows in the
        // history's own order rather than appending to the recent tail this
        // scans. Live-then-hydrate was the one route neither guard covered,
        // and it doubled every reply that arrived while its channel was closed.
        const dup = existing
          .slice(-8)
          .some((m) => m.from === "company" && m.text === event.text);
        if (dup) return t;
        return {
          ...t,
          [channelId]: [
            ...existing,
            // `replyVoice`, not a literal: a host-authored line (the
            // iteration-cap pause) is projected with `agentId: "system"` and
            // must render as the same centred row `fromHistory` gives it, or
            // whoever watched the turn live keeps an agent-style bubble that
            // hydration will never correct.
            makeMessage(replyVoice(event.agentId), event.text, {
              channel: event.agentId,
              taskId: event.taskId,
              mentions: event.mentions,
              // Issue #483: same identity as the thread store above. This is
              // the store `hydrateThread` folds into, so this is where the
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
      //
      // Guarded on the same condition `reReadSettledThread` uses, and for the
      // same reason (PR #1904 review): a thread can hold several detached
      // turns, and this clear is thread-wide. An earlier turn's reply landing
      // while a later one is still working would erase the rows of the turn
      // that is *currently* running — which reads as a teammate that stopped,
      // the exact appearance this timeline exists to prevent. The rows left
      // standing belong to work that really happened, and the open turn's own
      // settle clears them.
      // No turn id to exclude here: a reply arriving over SSE and its turn
      // settling on the poll are independent events, so the replying turn may
      // still be listed. That only defers the clear to its own settle, which
      // then runs the re-read above — the conservative direction, and the one
      // that never erases a running turn's rows.
      if (!hasOtherOpenTurns(openTurnsRef.current, event.chatId)) {
        setLiveStepsByThread((prev) =>
          prev[event.chatId]?.length ? { ...prev, [event.chatId]: [] } : prev,
        );
      }
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
   */
  const injectDispatchMarker = useCallback(
    (event: CompanyStreamEvent) => {
      if (event.type !== "desk_task_completed") return;
      const placement = dispatchMarkerPlacement(event, chatChannelByThread);
      if (!placement) return;
      const { channelId, message } = placement;

      if (!channelId) return;
      // The same id guard hydration runs. A marker cannot arrive twice off one
      // stream, but a reconnecting `EventSource` can replay a frame, and the id
      // is what makes that harmless.
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
  // Drop a thread's receipt when its POST reaches any terminal outcome (issue
  // #1934). `onSendEnd` is the resolved path and clears it as the reply bubble
  // lands; a detached/failed/stale POST clears it too, so a receipt never
  // outlives the send bracket that armed it — from there the openTurns-driven
  // working row is the live surface, not the receipt.
  //
  // `gen`, when given, is the generation the clearing send's own `onSendStart`
  // returned (issue #1935 review — see `shouldClearReceipt`'s doc for the
  // cross-company race this closes). Omitted by `Conversation`/
  // `conversation/runtime.ts`'s calls, which clear unconditionally as before.
  const clearReceipt = useCallback((threadId: string, gen?: number) => {
    setReceiptByThread((prev) => {
      if (!shouldClearReceipt(prev[threadId], gen)) return prev;
      const next = { ...prev };
      delete next[threadId];
      return next;
    });
  }, []);
  // The generation counter a receipt is stamped with at arm time (issue #1935
  // review). A plain ref, not state: it drives no render on its own, only the
  // value handed back to the caller and threaded through to whichever
  // terminal callback eventually clears the receipt it stamped.
  const receiptGenRef = useRef(0);
  const onSendStart = useCallback((threadId: string) => {
    pendingPostThreadsRef.current.started(threadId);
    activeTurnThreadRef.current = threadId;
    setLiveStepsByThread((prev) => ({ ...prev, [threadId]: [] }));
    // `lastFrameAt` seeds to `startedAt` so the stall check is "no frame for
    // 30s" from the send, not an instant stall.
    const now = Date.now();
    const gen = ++receiptGenRef.current;
    setReceiptByThread((prev) => ({
      ...prev,
      [threadId]: { startedAt: now, lastFrameAt: now, gen },
    }));
    return gen;
  }, []);
  const onSendEnd = useCallback(
    (threadId: string, gen?: number) => {
      pendingPostThreadsRef.current.ended(threadId);
      if (activeTurnThreadRef.current === threadId) activeTurnThreadRef.current = null;
      setLiveStepsByThread((prev) => {
        if (!prev[threadId]?.length) return prev;
        return { ...prev, [threadId]: [] };
      });
      clearReceipt(threadId, gen);
    },
    [clearReceipt],
  );
  /**
   * A chat POST that resolved for a company the operator has since left
   * (issue #1000).
   *
   * The turn and its reply are durably journaled in the OLD company's
   * history, so nothing about them belongs in the active scope. But the
   * send bracket `onSendStart` armed for the thread must still be released:
   * if the echo suppression were left up, `agent_reply` frames for the
   * thread would be captured into `pendingPostThreadsRef` and never
   * rendered. So release the held frames — discarding them, because history
   * re-reads them back when the operator returns — and lift the
   * suppression. Pointedly NOT `onSendDetached`: that renders the held
   * frames and arms an `openTurns` row, folding the old company's reply
   * into the active company's state, which is exactly the cross-company
   * leak the company guard exists to stop. Not `onSendEnd` either: it may
   * clear a live step timeline or the `activeTurnThreadRef` fallback that a
   * *current* company's own in-flight POST is using.
   */
  const onSendStale = useCallback(
    (threadId: string, gen?: number) => {
      pendingPostThreadsRef.current.detached(threadId);
      // The reply belongs to a company the operator has left; nothing about
      // this turn is in the active scope, so its receipt must not linger —
      // unless a newer send has already re-armed this thread id for the
      // company now on screen, in which case `gen` (this stale send's own
      // generation) will not match and `clearReceipt` leaves it alone.
      clearReceipt(threadId, gen);
    },
    [clearReceipt],
  );
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
    (threadId: string, turnId?: string, _gen?: number, chatId?: string) => {
      const held = pendingPostThreadsRef.current.detached(threadId);
      // Append, never replace (issue #1000). The serial lock queues a second
      // send behind the running turn, and a replace would stop the poll
      // watching the running row — the one whose reply settles first. The list
      // drains oldest-first, so the newest accepted turn goes on the end.
      setOpenTurns((prev) => {
        const turns = prev[threadId] ?? [];
        // The reload arm can race this POST's answer on the same turn.
        if (turnId && turns.some((t) => t.turnId === turnId)) return prev;
        // The desk travels with the row, from the caller that knows it. The map
        // key can be a composite (`engineering#41`) and no desk is called that,
        // so a row minted here without it left the settle poll unable to ask the
        // host anything — the poll being the only delivery path when SSE is
        // unavailable. `threadId` is the desk for callers whose key is not
        // composite (`Conversation`), which is why it is the fallback rather
        // than a parse of the key (CodeRabbit on #2044).
        return {
          ...prev,
          [threadId]: [...turns, { turnId, queued: true, chatId: chatId ?? threadId }],
        };
      });
      held.forEach((frame) => renderAgentReply(frame));
      // The receipt is NOT cleared here (issue #2021). The 202 hands the turn to
      // the open-turn row, but that row alone is a strict downgrade — bare
      // "Queued…"/"Working…" with no elapsed clock, no picked-up-by name, no 30s
      // stall notice. Keeping the receipt alive lets it ride the turn through the
      // queued/working window with every #1934 affordance intact; its own frames
      // keep bumping it, and the poll's terminal settle clears it (see
      // `reReadSettledThread`), so it still never outlives the turn.
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
    (threadId: string, gen?: number) => {
      const held = pendingPostThreadsRef.current.failed(threadId);
      held.forEach((frame) => renderAgentReply(frame));

      // Discover whether the host kept the turn after the request died. The
      // throw tells us nothing, but the run rows do: a `pending`/`running` row
      // naming this thread means the turn is durable and worth polling to its
      // terminal `chat/history` re-read — the SSE-less recovery path.
      //
      // The receipt clear now waits on this answer (issue #2021). A durable turn
      // survived the dead request, so keeping the receipt alive lets it ride
      // that turn through the queued/working window with its #1934 affordances
      // (elapsed, picked-up-by, 30s stall) rather than dropping to the bare
      // open-turn row — the poll's settle clears it. Only when NO durable turn
      // exists is the receipt dropped here, so it never ticks on over a POST the
      // host genuinely never kept, with the view's `Couldn't send` standing alone.
      listRuns(client, company, { status: ["pending", "running"] })
        .then((runs) => {
          if (!mountedRef.current) return;
          // A company switch that happened while the request was in flight
          // invalidates the result: the rows belong to the old company and
          // would restore a stale turn into the new company's openTurns map. The
          // switch already wholesale-cleared this company's receipts, so leave
          // the map alone rather than clearing a slot the new company may own.
          if (
            scopeRef.current.company !== company ||
            scopeRef.current.connection !== scope.connection ||
            scopeRef.current.client !== client
          ) return;
          const open = openTurnsFromRuns(runs);
          // The fold's whole list for this thread, not just its head: the POST
          // died mid-queue, so any rows the host kept are this turn's kin and
          // each has a reply to deliver. The merge appends and collapses by id.
          const durable = open[threadId];
          if (durable) setOpenTurns((prev) => mergeOpenTurns(prev, { [threadId]: durable }));
          else clearReceipt(threadId, gen);
        })
        .catch(() => {
          // Host without /runs, or offline — nothing to re-arm, so nothing will
          // ever settle the receipt. Drop it (generation-guarded) so it does not
          // tick on over a dead POST.
          clearReceipt(threadId, gen);
        });
    },
    [client, company, renderAgentReply, clearReceipt],
  );

  // Fold one live turn frame into the in-flight thread's timeline: a `tool_call`
  // upserts a `running` row keyed by `toolCallId`; a `tool_result` flips that row
  // to `ok`/`error` in place (FIFO fallback when no id pairs), mirroring
  // OpenHuman's `toolCallReceived` / `toolResultReceived`.
  // Who is here, and who is typing. Both are shell-level because the SSE
  // subscription is: the frames arrive on one stream for the whole console, so
  // the state they feed has to live where that stream is read.
  const presence = usePresence(client, company);
  const typing = useTyping(client, company);
  // The standing autonomy tier, for the title row. Shell-owned because the row
  // is: it outlives every view, so the read has to sit above all of them. It is
  // the same `GET {scope}/policy` the settings page makes, so the pill and the
  // page that changes it cannot disagree about which tier is in force.
  const autonomy = useAutonomy(client, company);
  /**
   * The coarse "near your credit limit" warning (issue #1846), off the live
   * `budget_proximity` frame. Shell-owned for the same reason presence/typing
   * are: the SSE subscription lives here, and the banner must outlive a
   * channel switch (the warning is about the company, not one conversation).
   * `null` once dismissed or once a fresh dispatch has not re-raised it.
   *
   * "Outlive a channel switch" stops at the company boundary, though — see
   * the reset effect right below, the same shape `workflowRunEvents` and
   * `openTurns` already use for their own company-scoped state.
   */
  const [budgetProximity, setBudgetProximity] = useState<{
    message: string;
    atMillis: number;
    // The frame's own `agentId` (issue #1846 review, Codex #3869601278):
    // present for one teammate's daily cap, absent for the company-wide
    // ceiling. Carried through to state (not just read at arrival) because
    // the expiry effect below needs it too, to pick the right boundary —
    // see `isBudgetProximityExpired`'s doc.
    agentId?: string;
  } | null>(null);
  // Issue #1846 review (Codex #3864988188): company-scoped, and reset on
  // company change for the same reason `workflowRunEvents`/`openTurns` are
  // (see their own reset effects above) — this state has no company id on
  // it, so switching without clearing left company A's warning rendered
  // under company B until B dismissed it or emitted its own.
  useEffect(() => {
    setBudgetProximity((prev) => (prev === null ? prev : null));
  }, [client, company]);
  // Issue #1846 review (Codex #3866418899, refined by #3868962376 /
  // #3869601278): bounded self-expiry, since the backend only ever
  // publishes a `budget_proximity` frame while usage is at least 90%
  // (`is_approaching_budget_ceiling`'s callers in `harness/built_in/mod.rs`)
  // and never a "cleared" counterpart. Without this, a daily agent cap
  // resetting at 00:00 UTC, a plan period rolling over, or an operator
  // raising the cap all leave usage back below that threshold with nothing
  // on the wire to say so, and the banner claimed the previous period's
  // status forever.
  //
  // The boundary itself depends on `agentId` — see
  // `isBudgetProximityExpired`/`budgetProximityExpiresAt`'s docs: a per-agent
  // DAILY warning is anchored to the next UTC midnight (that reset's actual
  // instant), but the COMPANY-WIDE warning has no such fixed boundary the
  // console can compute, so it keeps the flat 24h ceiling instead — anchoring
  // IT to midnight too would clear a still-valid warning hours or days before
  // its own (not-necessarily-UTC-day-aligned) plan period actually ends. A
  // dispatch that is STILL near the cap re-publishes its own frame well
  // inside the window, which reaches this effect as a new `budgetProximity`
  // value and re-arms the timer, so a genuinely ongoing warning never
  // flickers off mid-window.
  useEffect(() => {
    if (budgetProximity === null) return;
    if (isBudgetProximityExpired(budgetProximity.atMillis, Date.now(), budgetProximity.agentId)) {
      setBudgetProximity(null);
      return;
    }
    const remainingMs =
      budgetProximityExpiresAt(budgetProximity.atMillis, budgetProximity.agentId) - Date.now();
    const timer = setTimeout(() => setBudgetProximity(null), Math.max(remainingMs, 0));
    return () => clearTimeout(timer);
  }, [budgetProximity]);
  /**
   * The company's people, id → label.
   *
   * Presence and typing frames carry a user id and no label — deliberately, so
   * the wire does not repeat a name the console already holds — which means
   * something has to hold it. This is that. Read from the mention directory
   * rather than the admin user route, because it is the one people-listing a
   * *member* may read.
   *
   * A host without the route leaves this empty, which degrades cleanly: the
   * People section does not render and a typing line falls back to naming
   * nobody rather than naming a raw id.
   */
  const [companyPeople, setCompanyPeople] = useState<Array<{ id: string; label: string }>>(
    [],
  );
  useEffect(() => {
    let live = true;
    void client
      .mentionables(company)
      .then((d) => {
        // `d.people` is trusted by the types and not by reality: a host that
        // answers this route with a different shape — an older one, a proxy, a
        // stub that returns `[]` for anything it does not recognise — makes
        // this `undefined`, and `.map` on it throws during render. That blanks
        // the WHOLE console, not just the presence roster.
        //
        // Not hypothetical: it took out every test in
        // chat-channel-membership.spec.ts, a file with nothing to do with
        // presence, because its mock returns `[]` for unmatched routes.
        if (!live) return;
        const people = Array.isArray(d?.people) ? d.people : [];
        setCompanyPeople(people.map((p) => ({ id: p.id, label: p.label })));
      })
      .catch(() => {
        if (live) setCompanyPeople([]);
      });
    return () => {
      live = false;
    };
  }, [client, company]);

  /**
   * Who to name in the typing line for a given channel (and, when a thread
   * is open, that thread) — a resolver rather than one precomputed array,
   * because `ChatView` needs two independent lines: the main composer's
   * (`parentId` unset) and the open thread panel's (`parentId` set to the
   * parent message's id). A single array could only ever answer one of them,
   * which is why thread typing indicators never worked before this: the wire
   * and `useTyping` already carried `parentId`, but everything upstream threw
   * it away.
   *
   * Resolved here rather than in the view because the label map is here.
   * Somebody the directory does not name is dropped rather than shown as a raw
   * id — "u_01H4… is typing" is worse than saying nothing. Reuses `typersIn`
   * — the same filter+sort `TypingLine`'s stable ordering already relies on —
   * rather than re-deriving it here.
   */
  const resolveTypingNames = useCallback(
    (chatId: string, parentId?: string) => {
      const byId = new Map(companyPeople.map((p) => [p.id, p.label]));
      return typersIn(typing.typers, chatId, parentId, Date.now())
        .map((t) => byId.get(t.userId))
        .filter((label): label is string => Boolean(label));
    },
    [typing.typers, companyPeople],
  );
  const onTurnEvent = useCallback((event: CompanyStreamEvent) => {
    // The three kinds this folds. `use-events` only routes these here, so the
    // guard is a type narrowing rather than a runtime filter — but it is stated
    // rather than assumed, because `foldLiveFrame` takes the narrow shape and a
    // cast would let a fourth kind through silently if that routing ever grew.
    if (event.type !== "tool_call" && event.type !== "tool_result" && event.type !== "thinking") {
      return;
    }
    // Workflow agent-node frames carry `workflowRunId`/`nodeId` instead of a
    // `chatId` (issue #1702) and belong to the run-trace sheet's own
    // subscription, not to any chat timeline. Route them out BEFORE the legacy
    // no-`chatId` fallback below, which would otherwise fold them into
    // whichever chat turn is currently in flight.
    if ("workflowRunId" in event && event.workflowRunId) return;
    // Route by the frame's own thread id so concurrent turns (even from the same
    // desk member) never cross-attribute; fall back to the in-flight ref only
    // when a frame carries no chatId (older host / background turn).
    const frameThreadId =
      ("chatId" in event && event.chatId) || activeTurnThreadRef.current;
    // …then through the shared resolver, which normalizes General spellings and
    // leaves every other id in the host-thread namespace these maps are keyed
    // in. Its doc carries the reasoning for both halves and for why an
    // unresolved General alias falls back to `MAIN_THREAD_ID` rather than to
    // its own spelling.
    const threadId = frameThreadId
      ? liveFrameThreadKey(chatChannelByThreadRef.current, frameThreadId)
      : frameThreadId;
    if (!threadId) {
      // No chat bubble to fold the frame into. A dispatched card raised from a
      // conversation now DOES stream — `run_steered_background` derives its
      // stream from the `origin_chat_id` the card carries — but it streams
      // *keyed*, so those frames arrive with a `chatId` and take the branch
      // above. What still reaches here is a card no conversation raised (a
      // board-raised card, a cron tick): its chat id is absent or empty, the
      // host resolves that to `LiveStream::Off`, and nothing is published. So a
      // chat-less frame is a host emitting a shape this console does not
      // render, and the Observatory's live re-read is instead driven by the
      // workflow node events in `onWorkflowRunEvent`.
      return;
    }
    // Which bucket this row belongs in. A frame that names the operator message
    // it answers is filed under that **query**; one that does not falls back to
    // the thread, which is every turn answering no journaled message and every
    // host older than `messageSeq`.
    //
    // Filing under one or the other — never both — is what keeps a row from
    // rendering twice, and is why arming a second turn can no longer clear the
    // first one's rows: they are not in the same list any more.
    const messageKey =
      "messageSeq" in event && event.messageSeq !== undefined
        ? hostMessageId(String(event.messageSeq))
        : undefined;
    const setRows = messageKey ? setLiveStepsByMessage : setLiveStepsByThread;
    const rowKey = messageKey ?? threadId;
    setRows((prev) => {
      const rows = foldLiveFrame(prev[rowKey] ?? [], event);
      // `null` is "this frame belongs to rows we do not hold" — keep the
      // previous object so React skips the re-render.
      if (!rows) return prev;
      return { ...prev, [rowKey]: rows };
    });
    // Keep this thread's receipt alive off the same frame (issue #1934): a frame
    // arriving means the turn is advancing, so bump `lastFrameAt` (which clears
    // any stall) and capture the first agent id we see. Guarded on an existing
    // receipt — a stray background frame for a thread we never sent on must not
    // conjure one, mirroring the `if (!threadId) return` guard above.
    setReceiptByThread((prev) => {
      const existing = prev[threadId];
      if (!existing) return prev;
      const frameAgentId = "agentId" in event ? event.agentId : undefined;
      const agentId = existing.agentId ?? (frameAgentId || undefined);
      return { ...prev, [threadId]: { ...existing, lastFrameAt: Date.now(), agentId } };
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
    blocker?: { verdict: BlockerVerdict; answer?: string },
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
        blocker,
      });
      // A blocker answers for its whole root-cause group. Each sibling the host
      // settled is this tab's decision too, so its SSE echo must not surface as
      // a second toast for a card the operator decided once (#1211).
      for (const settled of answer.settledIds ?? []) {
        ownApprovalDecisionsRef.current.add(settled);
      }
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
        blocker
          ? blockerDecidedLine(blocker.verdict, undefined, answer.settledIds)
          : verdict === "approve"
            ? approvedLine(answer.stillAwaiting)
            : "Declined — recorded.",
      );
      // A decline ends the thread's story, and silence would read as a stall.
      // An approve needs no line: the continuation lands as a real reply, which
      // is the whole point of deciding here.
      if (blocker) {
        noteInChannel(
          approval.thread,
          blockerDecidedLine(blocker.verdict, undefined, answer.settledIds),
        );
      } else if (verdict === "deny") {
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
    // The inline terminal marker is enough only while its origin channel is
    // actually on screen. Elsewhere — including another chat channel — the
    // event hook raises the linked completion toast (#1758).
    isViewingTaskOrigin: useCallback(
      (event: CompanyStreamEvent) => {
        if (event.type !== "desk_task_completed" || view !== "chat") return false;
        // Below `lg`, selecting the channel rail hides the transcript while
        // leaving `activeChatChannelRef` naming whatever was last shown — a
        // completion from that channel must not suppress its toast while the
        // operator cannot actually see the inline marker (#1768 codex review).
        if (!chatPaneVisibleRef.current) return false;
        const placement = dispatchMarkerPlacement(event, chatChannelByThread);
        const origin = placement?.channelId;
        if (origin == null || activeChatChannelRef.current !== origin) return false;
        // #1890 B: a card raised in a thread settles into that thread, and
        // `buildTimeline` folds every parented line into its root's replies —
        // so the marker is NOT in the channel timeline and the channel being
        // open proves nothing. It is visible only while that thread's panel is
        // the one open. An unparented marker still renders inline, so the
        // channel check alone remains right for it.
        const root = placement?.message.parentId;
        return root == null || openThreadRootRef.current === root;
      },
      [view, chatChannelByThread],
    ),
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
    onPresenceEvent: useCallback(
      (event: CompanyStreamEvent) => {
        if (event.type !== "presence") return;
        presence.onFrame(event);
      },
      [presence],
    ),
    onTypingEvent: useCallback(
      (event: CompanyStreamEvent) => {
        if (event.type !== "typing") return;
        typing.onFrame(event);
      },
      [typing],
    ),
    onBudgetProximityEvent: useCallback((event: CompanyStreamEvent) => {
      if (event.type !== "budget_proximity") return;
      setBudgetProximity({
        message: event.message,
        atMillis: event.atMillis,
        agentId: event.agentId,
      });
    }, []),
    onWorkflowRunEvent: useCallback((event: CompanyStreamEvent) => {
      // Both halves. The tick refreshes the durable history; the frames drive
      // the live canvas. Progress frames are far more frequent than outcomes,
      // so only an outcome bumps the tick — refetching history once per node
      // would be N round trips per run for a list that has not changed yet.
      setWorkflowRunEvents((prev) => [...prev, event].slice(-WORKFLOW_EVENT_WINDOW));
      if (event.type === "workflow_run_finished") setWorkflowRunTick((n) => n + 1);
      // The Observatory's live refresh is a separate tick fed by node
      // boundaries, not the run-history tick above: a node starting or settling
      // is exactly when a watching operator's attempt trace changes, and the
      // Workflows history must not pay a refetch per node. A node's turn
      // streams no frames of its own, so the boundary events are the signal.
      if (
        event.type === "workflow_run_started" ||
        event.type === "workflow_node_started" ||
        event.type === "workflow_node_finished"
      ) {
        setBackgroundTurnTick((n) => n + 1);
      }
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

  // PR #1875 review finding, round 13: `shouldHoldShellPending` holds on
  // `!setupChecked` precisely because `SetupController`'s own `onOpenChange`
  // is the *only* thing that ever sets it (see `setupChecked`'s own doc) —
  // but the JSX that mounted `<SetupController>` lived below both of this
  // function's early returns, reachable only once the ordinary shell itself
  // was chosen. Every fresh mount starts `setupChecked === false`, so the
  // very predicate this component exists to satisfy made `SetupController`
  // unreachable: the hold fired, returned before that JSX, `SetupController`
  // never mounted, `onOpenChange` never fired, and `setupChecked` stayed
  // `false` forever — a permanent loader, not a brief hold, for every
  // signed-in operator except a confirmed non-admin (`isAdmin === false`,
  // the one path `shouldHoldShellPending` returns early on before ever
  // reaching `setupChecked`) or one who had already skipped in this tab.
  // Hoisted here and rendered in every branch below so its roster read can
  // land regardless of which content this render currently picks. Radix's
  // `Dialog` (via `SetupDialog`) portals its own content and renders nothing
  // into normal flow while closed, so mounting it alongside `RouteLoading`
  // or `OnboardingGate` costs nothing visually.
  //
  // Round 14: rendering it in every branch is not enough on its own — it has to
  // sit at the *same* position in all three, or React reconciles it as a
  // different node and unmounts it on the very transition it exists to survive.
  // An unstaffed company's first roster result sets `setupChecked` and
  // `setupOpen` together, which flips this render from a branch below to the
  // ordinary shell; with the controller under a different root there, React
  // would throw away the already-proven `unstaffed`/`open` state and issue a
  // second `listTeam` — exposing the interactive shell while that read is in
  // flight, and leaving the dialog shut for good if it hangs or fails. So all
  // three outcomes root at the same `ConsoleProvider` with this as its first
  // child. That provider is pure context and renders no DOM of its own, so
  // wrapping the loader and the gate in it costs nothing and hands them the
  // same ambient `(client, company)` the shell already has.
  const setupController = (
    <SetupController
      client={client}
      company={company}
      force={setupForced}
      routeOpen={view === "setup"}
      deepLinked={deepLinked}
      onForceHandled={() => setSetupForced(false)}
      onOpenChange={handleSetupOpenChange}
      onCompleted={() => {
        // Keep these together: Company mounts with the new refresh key, and
        // setup's payoff is the roster rather than the Overview graph.
        setTeamBuilt((n) => n + 1);
        setSetupCompleted(true);
        setView("company");
      }}
      onRouteDismiss={() => setView(DEFAULT_VIEW)}
    />
  );

  // PR #1875 review finding, round 8 (widened round 10): hold the shell in a
  // neutral pending state — never the ordinary interactive shell, never the
  // gate itself — for as long as the first activation read is unresolved,
  // whether it is still in flight or already failed once and is retrying.
  // Without this, the gap below fell straight through to the full shell (its
  // `shouldShowOnboardingGate` guard reads "not checked yet" identically for
  // an unresolved read of any cause), leaving an operator clicking around a
  // shell the funnel had not actually cleared for them to be in, until the
  // read finally landed and abruptly yanked the gate over it — including on
  // a merely slow first read (the host scans the journal for this company's
  // funnel; see `shouldHoldShellPending`'s own doc), not only a proven
  // outage. `RouteLoading` is the same neutral loader every code-split route
  // fallback in this file already uses — see its own doc for why a bare
  // "Loading…" line is not enough (`title` names the page for a screen reader
  // that never sees a mounted heading otherwise).
  if (
    shouldHoldShellPending({
      status: activationGate.status,
      checked: activationGate.checked,
      setupOpen,
      setupChecked,
      skippedThisSession: gateSkipped,
      isAdmin: isGateAdmin,
      retrying: activationGate.retrying,
    })
  ) {
    // A durable read failure must not read as a hang. `stuck` means three
    // consecutive non-terminal `getActivation` failures (see
    // `STUCK_AFTER_FAILURES`) — a malformed event failing the host's
    // whole-journal scan on every read, say. `checked` never settles, so the
    // hold above is permanent, and the "skip for now" escape lives inside
    // `OnboardingGate`, which this branch never mounts: the operator would be
    // locked out of the whole console by a backend fault with no way forward
    // (PR #1875 review finding). Offer the same escape here instead of a
    // loader that never resolves. The polling continues underneath, so a
    // recovered backend still settles the gate on its own.
    //
    // `isGateAdminStuck` covers the other read this hold depends on (PR #1875
    // review finding): a durable non-401 `fetchMe` failure leaves `isGateAdmin`
    // at `null` forever with activation reading fine the whole time, so
    // `activationGate.stuck` alone never flips even though the hold above is
    // just as permanent — see `GATE_ADMIN_CHECK_STUCK_AFTER_FAILURES`'s own
    // doc.
    if (activationGate.stuck || isGateAdminStuck) {
      return (
        <ConsoleProvider client={client} company={company}>
          {setupController}
          <div className="flex min-h-svh items-center justify-center p-6">
            <div className="max-w-md space-y-3 text-center">
              <h1 className="text-lg font-medium">We can’t check your setup right now</h1>
              <p className="text-sm text-muted-foreground">
                The console keeps failing to read this company’s setup status. It will keep
                retrying, but you don’t have to wait.
              </p>
              <button
                type="button"
                onClick={skipGate}
                className="inline-flex items-center justify-center rounded-md bg-primary px-3 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90"
              >
                Continue to the console
              </button>
            </div>
          </div>
        </ConsoleProvider>
      );
    }
    return (
      <ConsoleProvider client={client} company={company}>
        {setupController}
        <RouteLoading title="Console" label="Loading…" />
      </ConsoleProvider>
    );
  }

  // Issue #1844: the blocking first-run gate. Held behind `!setupOpen` —
  // `setupOpen` is already true for as long as `SetupController`'s dialog is
  // open OR the company is unstaffed (see its own `onOpenChange`), the exact
  // signal `TourController` holds on below for the same reason: staffing runs
  // first, so an operator is never asked to run a workflow with nobody on the
  // roster yet to have written it. `activationGate.status` gates on `checked`
  // rather than rendering the instant `company` is known, so a fresh mount
  // never flashes the gate open for the one round trip it takes to learn the
  // company already cleared it.
  if (
    shouldShowOnboardingGate({
      status: activationGate.status,
      checked: activationGate.checked,
      setupOpen,
      skippedThisSession: gateSkipped,
      isAdmin: isGateAdmin,
    }) &&
    // Narrows `status` for the render below — `shouldShowOnboardingGate`
    // already guarantees this is non-null whenever it returns `true`, but
    // that guarantee lives in a separate module TypeScript cannot see through.
    activationGate.status
  ) {
    return (
      <ConsoleProvider client={client} company={company}>
        {setupController}
        <OnboardingGate
          client={client}
          company={company}
          status={activationGate.status}
          currentName={feed.status.name}
          onRefresh={activationGate.refresh}
          onSkip={skipGate}
        />
      </ConsoleProvider>
    );
  }

  return (
    // The ambient `(client, company)` for the leaves that have to fetch and are
    // drawn from too many parents to thread props to — today, the avatar tile,
    // which fetches an uploaded face through the client because an `<img>`
    // cannot carry a credential. See `lib/console-context.tsx` for why this is
    // deliberately not a general escape from props.
    <ConsoleProvider client={client} company={company}>
      {setupController}

      {/* `SidebarProvider` paints the chrome layer itself — see its own note on
          why that fill lives there and not here (issue #1178).

          `flex-col`, because the shell is now a title row above a
          sidebar-and-content row rather than a bare row of two columns. The
          provider stays the outermost box — the title row holds the profile
          control, which is inside this context — so the direction is flipped
          here rather than by wrapping the provider in another element. */}
      <SidebarProvider className="h-svh flex-col overflow-hidden">
      {/* Room's channel list is rendered by `ChatView`, in the content column,
          and painted in the sidebar column. This provider is the slot the two
          agree on; `room-rail.tsx` explains why it is a portal rather than the
          whole chat model lifted up here. Inside `SidebarProvider` because it
          also carries the sidebar's density down to the rail — the sidebar's
          collapse IS the channel list's collapse now — and above both the
          sidebar and the inset, which are the two ends of the portal. */}
      <RoomRailSlotProvider>
        <a
          href={`#${MAIN_CONTENT_ID}`}
          className="sr-only focus:fixed focus:top-4 focus:left-4 focus:z-50 focus:not-sr-only focus:rounded-md focus:bg-background focus:px-4 focus:py-2 focus:text-sm focus:font-medium focus:text-foreground focus:ring-2 focus:ring-ring focus:outline-none"
          onClick={(event) => {
            event.preventDefault();
            document.getElementById(MAIN_CONTENT_ID)?.focus();
          }}
        >
          Skip to content
        </a>
      {/* The window's one title row, above the sidebar and above the content
          and spanning the full width of the window. It carries the two controls
          that are about the *console* rather than about the page: which company
          you are in, and who you are signed in as. Both used to sit in the
          sidebar column — the switcher at its head under a reserved strip for
          the traffic lights, the profile row in its footer — which put them at
          opposite ends of a 13.5rem column and left the lights overlapping a
          narrow column instead of insetting a bar. See `window-title-bar.tsx`,
          which owns the geometry including the traffic-light inset. */}
      <WindowTitleBar
        switcher={
          <HostSwitcher
            variant="titlebar"
            companyName={feed.status.name}
            // The company's lifecycle, and every company on this host: both
            // were rows in the sidebar footer, and both are facts about *which
            // company you are in* — which is what this control is. See
            // `HostSwitcher`'s `companyState` for why the lifecycle is not
            // folded into the connection dot.
            companyState={lifecycle(feed.status.lifecycle, feed.status.emergency_paused)}
            companies={companies}
            activeCompany={company}
            onSwitchCompany={onSwitchCompany}
            onBackToPicker={onBackToPicker}
            onCreateCompany={onCreateCompany}
            canCreateCompany={canCreateCompanies(client)}
          />
        }
        overview={
          // The console's front page, as a glyph. `NAV` still carries the
          // labelled row and will until the sidebar restructure removes it; in a
          // chrome band a labelled button reads as content, so the name moves
          // here to `aria-label` and `title`. First thing the row drops as the
          // window narrows — see `TITLE_BAR_LADDER`.
          <OverviewButton
            active={isNavigationActive("overview", view)}
            onNavigate={() => setView("overview")}
          />
        }
        approvals={
          // What is waiting on you, from every page in every sidebar state.
          // `pending` is `feed.status.pending_approvals` passed straight
          // through — the same single value the sidebar badge and the collapsed
          // rail dot both used before this row took the signal off them.
          <ApprovalsButton
            pending={pending}
            active={isNavigationActive("approvals", view)}
            onNavigate={() => setView("approvals")}
          />
        }
        autonomy={
          // What the agents in this company are allowed to do without asking.
          // Renders nothing until the host has said, rather than guessing a
          // tier — see `useAutonomy`.
          //
          // `canManage` is the role this shell already knows. Both write
          // routes behind the pill call `require_admin`
          // (`src/server/ops/policy.rs:309,427`), so without it a member was
          // offered a menu whose every selection ends in a 403. The pill still
          // STATES the tier for them — standing policy is a fact about what
          // the agents around you may do, not an admin setting — it simply
          // stops pretending to be a control. `null` while `fetchMe` is in
          // flight reads as read-only, which is the safe direction: it hides
          // an affordance for one round trip rather than offering one that
          // cannot work.
          <AutonomyPill status={autonomy} canManage={isGateAdmin} />
        }
        profile={
          // Who you are signed in as, and nothing else. It renders nothing
          // where there is nobody to name — a host with no sign-in, or a
          // session that has just gone — and the row simply closes up.
          <ProfileRow
            variant="titlebar"
            client={client}
            company={company}
            onSignedOut={() => signedOut(scope.connection)}
          />
        }
      />

      {/* The shell proper, below the title row: the sidebar column and the
          content column, still flex siblings so the sidebar's `peer` selectors
          and its in-flow width gap keep working. */}
      <div className="flex w-full min-h-0 flex-1">
      <Sidebar
        collapsible="icon"
        // The sidebar's container is `fixed inset-y-0 h-svh` — it positions
        // against the VIEWPORT, so a title row placed above it in the flow does
        // not push it down and the column would slide underneath the bar. This
        // is the offset that puts it back, as inline style rather than a class
        // because `top-*` and `h-*` would be fighting `inset-y-0` and `h-svh`
        // on the same element and the winner would come down to stylesheet
        // order.
        style={{
          top: WINDOW_TITLE_BAR_HEIGHT,
          height: `calc(100svh - ${WINDOW_TITLE_BAR_HEIGHT}px)`,
        }}
      >

        <nav aria-label="Main navigation" className="flex min-h-0 flex-1 flex-col">
          <SidebarContent data-tour="sidebar">
          <SidebarNavigation view={view} sub={sub} onNavigate={setView} />
        </SidebarContent>
        {/* The console's own utilities sit at the FOOT of the column, under the
            destinations rather than over them. They act on the console, not on
            the company, so they belong after the list of places you can go —
            and the header they used to occupy is gone entirely now that the
            switcher lives in the window's title row. */}
        <SidebarFooter>
          <SidebarUtilityBar view={view} onNavigate={setView} />
        </SidebarFooter>
        </nav>
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
      <SidebarInset id={MAIN_CONTENT_ID} tabIndex={-1} className="min-h-0 min-w-0">
          {/* Show/hide the sidebar, on the corner it acts on.

              It used to sit in the sidebar's own header, which put the control
              that *hides* a panel inside the panel it hides — collapsing the
              column took the button with it. On the inset's leading corner it
              stays put through both states and points at the edge that moves.

              Here rather than inside `ContentSurface`: this control needs
              `useSidebar`, and that card is deliberately free of sidebar
              context — every page renders it, including ones with no sidebar at
              all. Centred ON the card's leading border, not inside it:
              `left-(--frame-inset)` puts it at the edge and `-translate-x-1/2`
              straddles it. Inside the card it sat over the page's own heading
              and read as part of the content; on the seam it reads as chrome
              belonging to the boundary it moves. Absolutely positioned, so it
              costs the page no layout and no view makes room for it.

              `hidden md:block` — desktop only, and the breakpoint is not an
              approximation. `useIsMobile` flips at exactly 768px, which is
              Tailwind's `md`, so this gate is the precise complement of the
              `!isMobile` that `SidebarCollapseButton` already reasons about:
              the two agree by construction rather than by coincidence.

              Below it the sidebar is a sheet, not a column, and it already has
              a control — the `md:hidden` "Toggle sidebar" bar at the foot of
              this inset. Leaving this one on made that two controls for one
              job on one viewport, and the second one was wrong in both of its
              halves: `SidebarCollapseButton` deliberately treats mobile as
              not-collapsed, so with the sheet closed it read "Collapse
              sidebar" and showed the close icon while pressing it OPENED the
              sheet. Teaching it `openMobile` and retiring the bar was the
              other way out and is the worse one — this button is absolutely
              positioned over the content, and issue #1265 moved the mobile
              trigger into a reserved row precisely to stop a floating control
              winning the hit-test in that corner. */}
          <div className="pointer-events-none absolute top-4 left-(--frame-inset) z-20 hidden -translate-x-1/2 md:block">
            <div className="pointer-events-auto">
              <SidebarCollapseButton />
            </div>
          </div>
        {/* The card half of the two-layer shell: the one opaque sheet in the
            console, floating on the chrome the shell root paints (issue
            #1178). A `div`, not `main` — `SidebarInset` above is already the
            console's one `<main>` landmark, and a second nested one gave every
            page two identical "skip to content" destinations (issue #1221). */}
        {/* Every teammate's face in here is a way into who they are (issue
            #1653): the panel is mounted once around the whole surface so a
            click on an avatar in a transcript, a member list or a channel
            header opens the same summary, over the page rather than instead of
            it. */}
        <AgentProfileProvider client={client} company={company}>
        <ContentSurface>
          {/* `#/overview` is the company graph again — the page #1321 swapped
              out for the operator landing view. The graph keeps the
              `#/company/graph` alias that issue gave it, so every link minted
              while it lived there still resolves.

              `OperatorOverview` is left in the tree, unrouted: its panels are
              real work (#1015, #1700, #1745) and the decision about where they
              belong is not this change's to make. Nothing renders it today. */}
          {(view === "overview" || view === "setup") && (
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
              // The graph at `#/company/graph` names its core node after the
              // company the way the rest of the console does (issue #1219),
              // not after the slug.
              companyName={feed.status.name}
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
              presence={presence.peers}
              companyPeople={companyPeople}
              resolveTypingNames={resolveTypingNames}
              onTyping={typing.announce}
              onNavigate={(channelId) => navigate("chat", channelId)}
              onReply={() => void feed.refresh()}
              transcripts={transcripts}
              setTranscripts={setTranscripts}
              hydration={hydration}
              onSendStart={onSendStart}
              onSendEnd={onSendEnd}
              onSendDetached={onSendDetached}
              onSendFailed={onSendFailed}
              onSendStale={onSendStale}
          scopeRef={scopeRef}
              openTurns={openTurns}
              liveStepsByThread={liveStepsByThread}
              liveStepsByMessage={liveStepsByMessage}
              receiptByThread={receiptByThread}
              agentNames={agentNames}
              unread={unread}
              onChannelViewed={onChannelViewed}
              onChatPaneVisibilityChange={onChatPaneVisibilityChange}
              mentionFeedRevision={mentionFeedVersion}
              mentions={mentionCounts}
              approvals={feed.approvals}
              chatChannelByThread={chatChannelByThread}
              taskStatusByTaskId={taskStatusByTaskId}
              inflightRuns={inflightRuns}
              onInflightSteered={refreshTaskStatuses}
              now={feed.now}
              onDecideApproval={(approval, verdict, scope, blocker) =>
                void decideApproval(approval, verdict, scope, blocker)
              }
              decidingApprovals={decidingApprovals}
              decidedApprovals={decidedApprovals}
              failedApprovals={failedApprovals}
              budgetProximity={budgetProximity}
              onDismissBudgetProximity={() => setBudgetProximity(null)}
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
              // Issue #1891: and decided here too, not only named. The same
              // bundle the board and the run drawer get, so a verdict given on
              // any of the three settles on the others with no reload. Named
              // as this route's own props rather than the `…Approvals` suffix
              // the section views take: it is a thin wrapper whose props mirror
              // `TaskDetailView`'s, which has no other kind of decision to
              // qualify these against.
              deciding={decidingApprovals}
              decided={decidedApprovals}
              failed={failedApprovals}
              onDecide={(approval, verdict, scope, blocker) =>
                void decideApproval(approval, verdict, scope, blocker)
              }
              // Issue #246: the card → chat half of the round trip. The card
              // carries the host thread it was opened from; the map is what
              // turns that into the Room channel rendering it, which is the
              // whole address (`#/chat/<channelId>`) — so the destination is
              // linkable and Back returns to the card. The row states the
              // origin without offering a jump when no channel carries it.
              chatChannelByThread={chatChannelByThread}
              onOpenChannel={(channelId, threadId) =>
                navigate("chat", channelId, { thread: threadId ?? null })
              }
              // Back, and a deleted card, go to the board — which is the
              // `tasks` ledger. Through `navigate` so the address follows.
              onLeave={() =>
                navigate("ledgers", BOARD_LEDGER, {
                  [LEDGER_VIEW_PARAM]:
                    readLedgerViewMode() === "list" ? "list" : null,
                })
              }
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
              onOpenCard={(id, mode) =>
                navigate("tasks", id, {
                  [LEDGER_VIEW_PARAM]: mode === "list" ? "list" : null,
                })
              }
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
              // Issue #1891: a blocked card decides in place rather than only
              // reporting that it is blocked. The same four maps the run drawer
              // receives, owned here for the same reason — an operator who
              // decides on the board, steps over to Approvals and comes back
              // must not find a card that forgot what they did. `decided` is
              // fed by the `approval_resolved` frame as well as by this
              // console's own resolves, so a decision taken on the page settles
              // on the board with no reload.
              //
              // This replaces `onReviewApprovals`: the card's own "View
              // details" is an `href` built with `withHostParam`, which lands
              // the same `#/approvals/<taskId>` in the hash — surviving a
              // refresh and the Back button — without a callback to route it.
              decidingApprovals={decidingApprovals}
              decidedApprovals={decidedApprovals}
              failedApprovals={failedApprovals}
              onDecideApproval={(approval, verdict, scope, blocker) =>
                void decideApproval(approval, verdict, scope, blocker)
              }
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
          {view === "workspace" && (
            <Suspense
              fallback={<RouteLoading title="Workspace" label="Loading workspace…" />}
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
          {view === "brain" && (
            <Suspense fallback={<RouteLoading title="Brain" label="Loading what your company remembers…" />}>
              <MemoryView client={client} company={company} />
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
              chatChannelByThread={chatChannelByThread}
              onResolved={noteSystem}
              onGoToConversation={() => setView("chat")}
              // Issue #1211: mark this id as "mine" before the resolve POST
              // goes out, so the SSE echo for it — which can arrive before the
              // POST settles — is not toasted a second time.
              onDecideStart={(approvalId) => ownApprovalDecisionsRef.current.add(approvalId)}
            />
          )}
          {view === "observatory" && (
          <Suspense
            fallback={
              // Matches `ObservatoryView`'s own rule for the same reason
              // `RouteLoading`'s doc gives: the loading state has to settle on
              // the name the loaded header will actually show, or the title
              // flips the moment the chunk lands.
              <RouteLoading title={sub ? "Run" : "Observatory"} label="Loading observatory…" />
            }
          >
            <ObservatoryView
              client={client}
              company={company}
              // `#/observatory/<workflowRunId>` — the run to inspect, or null
              // for the index. Unvalidated here for the reason every other
              // sub-page is: only the view knows which run ids exist.
              runId={sub}
              // One tick for both signals a re-read should follow: a workflow
              // run moved, or a workflow node started or settled (a node's turn
              // streams no frames of its own, so the boundary is the signal).
              eventTick={workflowRunTick + backgroundTurnTick}
            />
          </Suspense>
        )}
        {view === "workflows" && (
            <Suspense
              fallback={
                // Static, not sub-aware: the loaded header's "Workflows" /
                // "Runs" split reads `indexTab`, a tab this boundary cannot
                // see (persisted client-side, not carried by the route) — see
                // `RouteLoading`'s own doc for why a guess here would be worse
                // than no bar.
                <RouteLoading title="Workflows" label="Loading canvas…" />
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
                onDecideApproval={(approval, verdict, scope, blocker) =>
                  void decideApproval(approval, verdict, scope, blocker)
                }
              />
            </Suspense>
          )}
          {view === "pages" && (
            <Suspense fallback={<RouteLoading title="Pages" label="Loading pages…" />}>
              <PagesView client={client} company={company} />
            </Suspense>
          )}
          {view === "finances" && (
            <Suspense
              fallback={
                // Names the section itself while its own lazy chunk is still
                // in flight — before `FinanceSection` has even mounted to run
                // its own per-subpage Suspense (which already uses
                // `RouteLoading` with this same static title, one level in).
                <RouteLoading
                  title={financeFallbackTitle(sub)}
                  label={`Loading ${financeFallbackTitle(sub).toLowerCase()}…`}
                />
              }
            >
              <FinanceSection
                client={client}
                company={company}
                sub={sub}
                onNavigate={(page) => navigate("finances", page)}
              />
            </Suspense>
          )}
          {view === "connections" && (
            <ConnectionsSection client={client} company={company} sub={sub} />
          )}
          {view === "settings" && (
            <SettingsSection
              client={client}
              company={company}
              feed={feed}
              sub={sub}
              onFlag={() => setFeedbackOpen(true)}
              onResetCompany={onResetCompany}
            />
          )}
          {view === "feedback" && <FeedbackView client={client} company={company} />}
          {view === "not-found" && <UnknownRouteView address={sub} />}
        </ContentSurface>
        </AgentProfileProvider>

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
      </div>

      <FeedbackDialog
        client={client}
        company={company}
        open={feedbackOpen}
        onOpenChange={setFeedbackOpen}
      />

      <TourController
        company={company}
        setView={setView}
        hold={setupOpen}
        suppressWelcome={setupCompleted}
      />
      </RoomRailSlotProvider>
      </SidebarProvider>
    </ConsoleProvider>
  );
}
