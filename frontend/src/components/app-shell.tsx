import { lazy, Suspense, useCallback, useEffect, useState } from "react";
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
  Plug,
  Settings2,
  ShieldCheck,
  Sparkles,
  SquareKanban,
  Blocks,
  UserCog,
  Users,
  Wallet,
  Workflow,
} from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
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
import { Separator } from "@/components/ui/separator";
import { CompanySwitcher } from "@/components/company-switcher";
import { FeedbackDialog } from "@/components/feedback-dialog";
import { StatusPill } from "@/components/status-pill";
import { ThemeToggle } from "@/components/theme-toggle";
import { DiscordIcon } from "@/components/discord-icon";
import { useCompany } from "@/hooks/use-company";
import { type AgentReplyEvent, useEvents } from "@/hooks/use-events";
import { useHashView } from "@/hooks/use-hash-view";
import { type ChatMessage, makeMessage } from "@/lib/chat";
import { DISCORD_INVITE_URL } from "@/lib/links";
import { defaultThreads, threadsFromDesks } from "@/lib/threads";
import { Overview } from "@/views/Overview";
import { Conversation } from "@/views/Conversation";
import { ApprovalsView } from "@/views/ApprovalsView";
import { TasksView } from "@/views/TasksView";
import { TeamView } from "@/views/TeamView";
import { PeopleView } from "@/views/PeopleView";
import { SkillsView } from "@/views/SkillsView";
import { InboxView } from "@/views/InboxView";
import { MemoryView } from "@/views/MemoryView";
import { ConnectionsView } from "@/views/ConnectionsView";
import { McpServersView } from "@/views/McpServersView";
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
  | "skills"
  | "workspace"
  | "memory"
  | "approvals"
  | "workflows"
  | "usage"
  | "finances"
  | "connections"
  | "mcp"
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
      { view: "mcp", label: "MCP Servers", icon: Blocks },
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
  skills: "Skills",
  workspace: "Workspace",
  memory: "Brain",
  approvals: "Approvals",
  workflows: "Workflows",
  usage: "Usage",
  finances: "Finances",
  connections: "Connections",
  mcp: "MCP Servers",
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
  const feed = useCompany(client, company, initialStatus);

  const pending = feed.status.pending_approvals;

  // Build the chat threads from the company's real desks (issue #53); keep the
  // static defaults when the host doesn't expose `/desks` (404) or defines none.
  // Merges by id so a transcript typed before desks load survives.
  useEffect(() => {
    let cancelled = false;
    client
      .listDesks(company)
      .then((desks) => {
        if (cancelled) return;
        setThreads((prev) => {
          const byId = new Map(prev.map((t) => [t.id, t]));
          return threadsFromDesks(desks).map((t) => {
            const existing = byId.get(t.id);
            return existing ? { ...t, messages: existing.messages } : t;
          });
        });
      })
      .catch(() => {
        /* host without `/desks`, or offline — keep the current threads */
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

  // The active push half of the attention surface: SSE-driven toasts + chat
  // injection, plus a rising-edge "needs a sign-off" toast off the poll's
  // pending count. Degrades silently to the `useCompany` poll when the host has
  // no `/events` route.
  useEvents(client, company, {
    pendingApprovals: pending,
    onAgentReply: injectAgentReply,
  });

  return (
    <SidebarProvider>
      <Sidebar collapsible="icon">
        <SidebarHeader>
          <CompanySwitcher
            active={feed.status}
            companies={companies}
            onSwitch={onSwitchCompany}
            onBackToPicker={onBackToPicker}
          />
        </SidebarHeader>
        <SidebarContent>
          {NAV.map((group) => (
            <SidebarGroup key={group.label}>
              <SidebarGroupLabel>{group.label}</SidebarGroupLabel>
              <SidebarMenu>
                {group.items.map((item) => (
                  <SidebarMenuItem key={item.view}>
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

      <SidebarInset>
        <header className="flex h-14 shrink-0 items-center gap-2 border-b px-4">
          <SidebarTrigger className="-ml-1" />
          <Separator orientation="vertical" className="mr-1 h-4" />
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

        <main className="flex flex-1 flex-col overflow-hidden">
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
            />
          )}
          {view === "inbox" && <InboxView company={company} />}
          {view === "tasks" && <TasksView client={client} company={company} />}
          {view === "team" && <TeamView client={client} company={company} />}
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
              <UsageView />
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
              <FinancesView />
            </Suspense>
          )}
          {view === "connections" && <ConnectionsView client={client} company={company} />}
          {view === "mcp" && <McpServersView client={client} company={company} />}
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
    </SidebarProvider>
  );
}
