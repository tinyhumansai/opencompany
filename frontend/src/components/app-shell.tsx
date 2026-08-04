import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import {
  FolderClosed,
  LayoutDashboard,
  type LucideIcon,
  MessagesSquare,
  Settings2,
  ShieldCheck,
  SquareKanban,
  Workflow,
} from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus, TurnStep } from "@/api/types";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuBadge,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarRail,
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
import { useHashView } from "@/hooks/use-hash-view";
import { toast } from "sonner";

import { type ChatMessage, fromHistory, makeMessage } from "@/lib/chat";
import { CONNECTION_PROVIDERS } from "@/lib/connections";
import { agentDmThreads, defaultThreads, threadsFromDesks } from "@/lib/threads";
import { Overview } from "@/views/Overview";
import { ChatView } from "@/views/ChatView";
import { DEFAULT_CHANNEL, type Transcripts } from "@/views/chat/model";
import { Conversation } from "@/views/Conversation";
import { TeamView } from "@/views/TeamView";
import { ApprovalsView } from "@/views/ApprovalsView";
import { TasksView } from "@/views/TasksView";
import { InboxView } from "@/views/InboxView";
import { MemoryView } from "@/views/MemoryView";
import { FeedbackView } from "@/views/FeedbackView";
import { SettingsSection } from "@/views/SettingsSection";

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
  | "chat"
  | "conversation"
  | "inbox"
  | "tasks"
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
  { view: "chat", label: "Chat", icon: MessagesSquare },
  { view: "tasks", label: "Tasks", icon: SquareKanban },
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
 * everything they can do it can do in one screen — but they keep answering
 * `#/conversation` and `#/team` until the chat covers the last of what they
 * still do better (a desk's persisted transcript, a teammate's budget line).
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
  const [view, sub, navigate] = useHashView<View>(VIEWS, "overview");
  // Most call sites only ever change the top-level view.
  const setView = useCallback((next: View, nextSub?: string) => navigate(next, nextSub), [navigate]);
  const [feedbackOpen, setFeedbackOpen] = useState(false);
  // The shell owns every channel's transcript, not `ChatView` — the shell
  // mounts and unmounts `ChatView` per route, so component-local state there
  // would be discarded on every trip away from Chat and back.
  const [transcripts, setTranscripts] = useState<Transcripts>({});
  const [threads, setThreads] = useState(defaultThreads);
  const [activeThreadId, setActiveThreadId] = useState("main");
  // A monotonic nonce bumped on every task-lifecycle SSE event, so the
  // company-chat in-flight steer strip (issue #111) refetches live.
  const [taskEventTick, setTaskEventTick] = useState(0);
  // Issue #228: bumped on every `workflow_run_finished` so the Workflows view
  // refreshes its run history live. Same shape as `taskEventTick` — a counter,
  // not the payload, so the view owns what it refetches.
  const [workflowRunTick, setWorkflowRunTick] = useState(0);
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
      })
      .catch(() => {
        // Host without `/desks`, or offline — keep the static default
        // threads, but the operator/General line still deserves a
        // rehydration attempt (it's the one every deployment has).
        defaultThreads().forEach((t) => hydrate(t.id));
      });

    return () => {
      cancelled = true;
    };
  }, [client, company]);

  const setThreadMessages = (
    threadId: string,
    updater: (m: ChatMessage[]) => ChatMessage[],
  ) =>
    setThreads((ts) =>
      ts.map((t) => (t.id === threadId ? { ...t, messages: updater(t.messages) } : t)),
    );

  // Approval decisions and other events land in a transcript rather than
  // vanishing. Both chat surfaces get the line: Chat's `main` channel gets it
  // appended directly (the shell owns `transcripts`, not `ChatView`, so this
  // survives `ChatView` unmounting), and the parked Conversation appends to
  // its active thread.
  const noteSystem = (line: string) => {
    setTranscripts((t) => ({
      ...t,
      [DEFAULT_CHANNEL]: [...(t[DEFAULT_CHANNEL] ?? []), makeMessage("system", line)],
    }));
    setThreadMessages(activeThreadId, (m) => [...m, makeMessage("system", line)]);
  };

  // Inject an `AgentReply` pushed over the SSE feed (issue #66) into its desk
  // thread's transcript. Dedupe against our own optimistic echo: the backend
  // journals an `AgentReply` for the operator's own chat turn too, and
  // Conversation already rendered that reply locally. Local message ids are
  // ephemeral counters (not content-addressed), so we key the dedupe on an
  // identical company line already present in the thread's recent tail. Only
  // desks that exist as a thread receive an injection; an unmatched chatId is a
  // no-op rather than polluting the wrong thread.
  const injectAgentReply = useCallback((event: AgentReplyEvent) => {
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
            }),
          ],
        };
      }),
    );
  }, []);

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

  // The active push half of the attention surface: SSE-driven toasts + chat
  // injection, plus a rising-edge "needs a sign-off" toast off the poll's
  // pending count. Degrades silently to the `useCompany` poll when the host has
  // no `/events` route.
  useEvents(client, company, {
    pendingApprovals: pending,
    onAgentReply: injectAgentReply,
    onTaskEvent: useCallback(() => setTaskEventTick((n) => n + 1), []),
    onTurnEvent,
    onWorkflowRunEvent: useCallback(() => setWorkflowRunTick((n) => n + 1), []),
  });

  return (
    <SidebarProvider className="h-svh overflow-hidden">
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
                    <SidebarMenuBadge>{pending}</SidebarMenuBadge>
                  )}
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter>
          <SidebarControls
            lifecycleState={feed.status.lifecycle}
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
          {view === "chat" && (
            <ChatView
              client={client}
              company={company}
              sub={sub}
              onNavigate={(channelId) => navigate("chat", channelId)}
              onReply={() => void feed.refresh()}
              transcripts={transcripts}
              setTranscripts={setTranscripts}
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
              // Issue #246: the card → chat half of the round trip. A card
              // opened from a conversation remembers which one, so its detail
              // screen can put the operator back in that thread.
              onOpenThread={(threadId) => {
                setActiveThreadId(threadId);
                setView("conversation");
              }}
            />
          )}
          {view === "team" && <TeamView client={client} company={company} />}
          {view === "memory" && <MemoryView client={client} company={company} />}
          {view === "workspace" && (
            <Suspense
              fallback={
                <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
                  Loading workspace…
                </div>
              }
            >
              <WorkspaceView client={client} company={company} />
            </Suspense>
          )}
          {view === "approvals" && (
            <ApprovalsView
              client={client}
              company={company}
              feed={feed}
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
                runEventTick={workflowRunTick}
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
