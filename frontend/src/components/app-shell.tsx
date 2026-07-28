import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import {
  Brain,
  ChartColumnBig,
  FolderClosed,
  Flag,
  Inbox,
  LayoutDashboard,
  type LucideIcon,
  MessageSquareWarning,
  MessagesSquare,
  PanelsTopLeft,
  Plug,
  Settings2,
  ShieldCheck,
  Sparkles,
  SquareKanban,
  UserCog,
  Users,
  Wallet,
  Workflow,
} from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus, TurnStep } from "@/api/types";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuBadge,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarRail,
  SidebarTrigger,
} from "@/components/ui/sidebar";
import { Button } from "@/components/ui/button";
import { CompanySwitcher } from "@/components/company-switcher";
import { FeedbackDialog } from "@/components/feedback-dialog";
import { TourController } from "@/tour/TourController";
import { StatusPill } from "@/components/status-pill";
import { ThemeToggle } from "@/components/theme-toggle";
import { DiscordIcon } from "@/components/discord-icon";
import { useCompany } from "@/hooks/use-company";
import { type AgentReplyEvent, type CompanyStreamEvent, useEvents } from "@/hooks/use-events";
import { useHashView } from "@/hooks/use-hash-view";
import { toast } from "sonner";

import { type ChatMessage, fromHistory, makeMessage } from "@/lib/chat";
import { CONNECTION_PROVIDERS } from "@/lib/connections";
import { DISCORD_INVITE_URL } from "@/lib/links";
import { defaultThreads, threadsFromDesks } from "@/lib/threads";
import { Overview } from "@/views/Overview";
import { Conversation } from "@/views/Conversation";
import { ApprovalsView } from "@/views/ApprovalsView";
import { TasksView } from "@/views/TasksView";
import { TeamView } from "@/views/TeamView";
import { DesksView } from "@/views/DesksView";
import { PeopleView } from "@/views/PeopleView";
import { SkillsView } from "@/views/SkillsView";
import { InboxView } from "@/views/InboxView";
import { MemoryView } from "@/views/MemoryView";
import { ConnectionsView } from "@/views/ConnectionsView";
import { SettingsView } from "@/views/SettingsView";
import { FeedbackView } from "@/views/FeedbackView";

// React Flow is heavy and only used here — load it on demand.
const WorkflowsView = lazy(() =>
  import("@/views/WorkflowsView").then((m) => ({ default: m.WorkflowsView })),
);
// Pulls in the markdown renderer — load on demand.
const WorkspaceView = lazy(() =>
  import("@/views/WorkspaceView").then((m) => ({ default: m.WorkspaceView })),
);
// Recharts is heavy — load the usage dashboard on demand.
const UsageView = lazy(() => import("@/views/UsageView").then((m) => ({ default: m.UsageView })));
// Also Recharts-backed — load on demand.
const FinancesView = lazy(() =>
  import("@/views/FinancesView").then((m) => ({ default: m.FinancesView })),
);

export type View =
  | "overview"
  | "people"
  | "conversation"
  | "inbox"
  | "tasks"
  | "team"
  | "desks"
  | "skills"
  | "workspace"
  | "memory"
  | "approvals"
  | "workflows"
  | "usage"
  | "finances"
  | "connections"
  | "settings"
  | "feedback";

interface NavItem {
  view: View;
  label: string;
  icon: LucideIcon;
}

interface NavGroup {
  label: string;
  items: NavItem[];
}

const NAV: NavGroup[] = [
  {
    label: "Operate",
    items: [
      { view: "overview", label: "Overview", icon: LayoutDashboard },
      { view: "conversation", label: "Conversation", icon: MessagesSquare },
      { view: "inbox", label: "Inbox", icon: Inbox },
      { view: "tasks", label: "Tasks", icon: SquareKanban },
      { view: "team", label: "Team", icon: Users },
      { view: "desks", label: "Desks", icon: PanelsTopLeft },
      { view: "skills", label: "Skills", icon: Sparkles },
      { view: "workspace", label: "Workspace", icon: FolderClosed },
      { view: "memory", label: "Brain", icon: Brain },
      { view: "approvals", label: "Approvals", icon: ShieldCheck },
      { view: "workflows", label: "Workflows", icon: Workflow },
    ],
  },
  {
    label: "Configure",
    items: [
      { view: "usage", label: "Usage", icon: ChartColumnBig },
      { view: "finances", label: "Finances", icon: Wallet },
      { view: "connections", label: "Connections", icon: Plug },
      { view: "people", label: "People", icon: UserCog },
      { view: "settings", label: "Settings", icon: Settings2 },
    ],
  },
  {
    label: "Support",
    items: [{ view: "feedback", label: "Feedback", icon: MessageSquareWarning }],
  },
];

const TITLES: Record<View, string> = {
  overview: "Overview",
  conversation: "Conversation",
  inbox: "Inbox",
  tasks: "Tasks",
  team: "Team",
  desks: "Desks",
  skills: "Skills",
  workspace: "Workspace",
  memory: "Brain",
  approvals: "Approvals",
  workflows: "Workflows",
  usage: "Usage",
  finances: "Finances",
  connections: "Connections",
  people: "People",
  settings: "Settings",
  feedback: "Feedback",
};

const VIEWS = NAV.flatMap((g) => g.items.map((i) => i.view));

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
  const [view, setView] = useHashView<View>(VIEWS, "overview");
  const [threads, setThreads] = useState(defaultThreads);
  const [activeThreadId, setActiveThreadId] = useState("main");
  const [feedbackOpen, setFeedbackOpen] = useState(false);
  // A monotonic nonce bumped on every task-lifecycle SSE event, so the
  // company-chat in-flight steer strip (issue #111) refetches live.
  const [taskEventTick, setTaskEventTick] = useState(0);
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
  // `…/connections?connected={provider}` after storing the token. Land the
  // operator on the Connections view, confirm with a toast, then strip the
  // param so a refresh doesn't re-fire it. Runs once; StrictMode's double
  // invoke is harmless because the first run clears the param the second reads.
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const connected = params.get("connected");
    if (!connected) return;
    params.delete("connected");
    const query = params.toString();
    window.history.replaceState(
      {},
      "",
      window.location.pathname + (query ? `?${query}` : "") + window.location.hash,
    );
    setView("connections");
    // The callback param carries the raw provider id (e.g. "slack"); show the
    // catalog display name ("Slack") when we know it, falling back to the id.
    const providerName =
      CONNECTION_PROVIDERS.find((p) => p.id === connected)?.name ?? connected;
    toast.success(`Connected ${providerName}.`);
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
      .then((desks) => {
        if (cancelled) return;
        const resolved = threadsFromDesks(desks);
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

  // Approval decisions and other events land in the active thread's transcript.
  const noteSystem = (line: string) =>
    setThreadMessages(activeThreadId, (m) => [...m, makeMessage("system", line)]);

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
          messages: [...t.messages, makeMessage("company", event.text, { channel: event.agentId })],
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
  });

  return (
    <SidebarProvider className="h-svh overflow-hidden">
      <Sidebar collapsible="icon">
        <SidebarHeader>
          <CompanySwitcher
            active={feed.status}
            companies={companies}
            onSwitch={onSwitchCompany}
            onBackToPicker={onBackToPicker}
          />
        </SidebarHeader>
        <SidebarContent data-tour="sidebar">
          {NAV.map((group) => (
            <SidebarGroup key={group.label}>
              <SidebarGroupLabel>{group.label}</SidebarGroupLabel>
              <SidebarMenu>
                {group.items.map((item) => (
                  <SidebarMenuItem key={item.view} data-tour={`nav-${item.view}`}>
                    <SidebarMenuButton
                      isActive={view === item.view}
                      tooltip={item.label}
                      onClick={() => setView(item.view)}
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
          ))}
        </SidebarContent>
        <SidebarFooter>
          <SidebarMenu>
            <SidebarMenuItem>
              <SidebarMenuButton
                tooltip="Join our Discord"
                render={<a href={DISCORD_INVITE_URL} target="_blank" rel="noreferrer" />}
              >
                <DiscordIcon className="size-4" />
                <span>Join our Discord</span>
              </SidebarMenuButton>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarFooter>
        <SidebarRail />
      </Sidebar>

      <SidebarInset className="min-h-0">
        <header className="flex h-14 shrink-0 items-center gap-2 border-b px-4">
          <SidebarTrigger className="-ml-1" />
          <h1 className="text-sm font-semibold">{TITLES[view]}</h1>
          <div className="ml-auto flex items-center gap-2">
            <StatusPill lifecycle={feed.status.lifecycle} className="hidden sm:inline-flex" />
            <Button
              variant="outline"
              size="sm"
              className="hidden sm:inline-flex"
              onClick={() => setFeedbackOpen(true)}
            >
              <Flag className="size-4" />
              Flag something
            </Button>
            <ThemeToggle />
          </div>
        </header>

        <main className="flex min-h-0 flex-1 flex-col overflow-hidden">
          {view === "overview" && (
            <Overview
              feed={feed}
              client={client}
              company={company}
              onNavigate={setView}
              onFlag={() => setFeedbackOpen(true)}
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
          {view === "inbox" && <InboxView company={company} />}
          {view === "tasks" && <TasksView client={client} company={company} />}
          {view === "team" && <TeamView client={client} company={company} />}
          {view === "desks" && <DesksView client={client} company={company} />}
          {view === "people" && <PeopleView client={client} company={company} />}
          {view === "skills" && <SkillsView client={client} company={company} />}
          {view === "memory" && <MemoryView client={client} company={company} />}
          {view === "workspace" && (
            <Suspense
              fallback={
                <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
                  Loading workspace…
                </div>
              }
            >
              <WorkspaceView company={company} />
            </Suspense>
          )}
          {view === "approvals" && (
            <ApprovalsView
              client={client}
              company={company}
              feed={feed}
              onResolved={noteSystem}
              onGoToConversation={() => setView("conversation")}
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
              <WorkflowsView client={client} company={company} />
            </Suspense>
          )}
          {view === "usage" && (
            <Suspense
              fallback={
                <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
                  Loading usage…
                </div>
              }
            >
              <UsageView client={client} company={company} />
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
          {view === "connections" && <ConnectionsView client={client} company={company} />}
          {view === "settings" && (
            <SettingsView client={client} company={company} feed={feed} onFlag={() => setFeedbackOpen(true)} />
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
