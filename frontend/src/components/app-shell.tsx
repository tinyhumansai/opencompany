import { lazy, Suspense, useState } from "react";
import {
  Brain,
  FolderClosed,
  LayoutDashboard,
  type LucideIcon,
  MessagesSquare,
  Settings2,
  ShieldCheck,
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
import { useCompany } from "@/hooks/use-company";
import { useHashView } from "@/hooks/use-hash-view";
import { Overview } from "@/views/Overview";
import { ChatView } from "@/views/ChatView";
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
  | "inbox"
  | "tasks"
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
  { view: "memory", label: "Memory", icon: Brain },
  { view: "approvals", label: "Approvals", icon: ShieldCheck },
  { view: "workflows", label: "Workflows", icon: Workflow },
  { view: "finances", label: "Finances", icon: Wallet },
  { view: "settings", label: "Settings", icon: Settings2 },
];

// Two views are routable without a nav entry: Feedback, which the sidebar
// footer links to, and Inbox, which is unfinished and hidden until it is worth
// showing — `#/inbox` still resolves so the work stays reachable.
const VIEWS: View[] = [...NAV.map((i) => i.view), "feedback", "inbox"];

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
      <AutoCollapse view={view} />
      <Sidebar collapsible="icon">
        <SidebarHeader>
          <SidebarCollapseToggle />
        </SidebarHeader>
        <SidebarContent>
          <SidebarGroup>
            <SidebarMenu>
              {NAV.map((item) => (
                <SidebarMenuItem key={item.view}>
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
