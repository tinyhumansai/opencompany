import { lazy, Suspense, useState } from "react";
import {
  Brain,
  ChartColumnBig,
  FolderClosed,
  Inbox,
  LayoutDashboard,
  type LucideIcon,
  MessagesSquare,
  Settings2,
  ShieldCheck,
  Sparkles,
  SquareKanban,
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
} from "@/components/ui/sidebar";
import { FeedbackDialog } from "@/components/feedback-dialog";
import { SidebarCollapseToggle, SidebarControls } from "@/components/sidebar-controls";
import { useCompany } from "@/hooks/use-company";
import { useHashView } from "@/hooks/use-hash-view";
import { Overview } from "@/views/Overview";
import { ChatView } from "@/views/ChatView";
import { ApprovalsView } from "@/views/ApprovalsView";
import { TasksView } from "@/views/TasksView";
import { SkillsView } from "@/views/SkillsView";
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
// Recharts is heavy — load the usage dashboard on demand.
const UsageView = lazy(() => import("@/views/UsageView").then((m) => ({ default: m.UsageView })));
// Also Recharts-backed — load on demand.
const FinancesView = lazy(() =>
  import("@/views/FinancesView").then((m) => ({ default: m.FinancesView })),
);

export type View =
  | "overview"
  | "chat"
  | "inbox"
  | "tasks"
  | "skills"
  | "workspace"
  | "memory"
  | "approvals"
  | "workflows"
  | "usage"
  | "finances"
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
      { view: "chat", label: "Chat", icon: MessagesSquare },
      { view: "inbox", label: "Inbox", icon: Inbox },
      { view: "tasks", label: "Tasks", icon: SquareKanban },
      { view: "skills", label: "Skills", icon: Sparkles },
      { view: "workspace", label: "Workspace", icon: FolderClosed },
      { view: "memory", label: "Memory", icon: Brain },
      { view: "approvals", label: "Approvals", icon: ShieldCheck },
      { view: "workflows", label: "Workflows", icon: Workflow },
    ],
  },
  {
    label: "Configure",
    items: [
      { view: "usage", label: "Usage", icon: ChartColumnBig },
      { view: "finances", label: "Finances", icon: Wallet },
      { view: "settings", label: "Settings", icon: Settings2 },
    ],
  },
];

// Feedback is reachable from the sidebar footer rather than the nav, but it
// is still a real view — keep it routable so `#/feedback` resolves.
const VIEWS: View[] = [...NAV.flatMap((g) => g.items.map((i) => i.view)), "feedback"];

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
  const setView = (next: View) => navigate(next);
  const [feedbackOpen, setFeedbackOpen] = useState(false);
  // System lines raised outside chat (an approval decision, say). Chat owns
  // the transcripts now, so the shell hands it an append-only log to fold in.
  const [notices, setNotices] = useState<string[]>([]);
  const feed = useCompany(client, company, initialStatus);

  const pending = feed.status.pending_approvals;

  const noteSystem = (line: string) => setNotices((n) => [...n, line]);

  return (
    <SidebarProvider>
      <Sidebar collapsible="icon">
        <SidebarHeader>
          <SidebarCollapseToggle />
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

      <SidebarInset>
        <main className="flex flex-1 flex-col overflow-hidden">
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
              notices={notices}
            />
          )}
          {view === "inbox" && <InboxView company={company} />}
          {view === "tasks" && <TasksView client={client} company={company} />}
          {view === "skills" && <SkillsView client={client} company={company} />}
          {view === "memory" && <MemoryView company={company} />}
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
              <WorkflowsView />
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
    </SidebarProvider>
  );
}
