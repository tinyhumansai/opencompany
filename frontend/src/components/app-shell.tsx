import { useState } from "react";
import {
  Flag,
  LayoutDashboard,
  type LucideIcon,
  MessagesSquare,
  Settings2,
  ShieldCheck,
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
import { useCompany } from "@/hooks/use-company";
import { type ChatMessage, nextMessageId } from "@/lib/chat";
import { Overview } from "@/views/Overview";
import { Conversation } from "@/views/Conversation";
import { ApprovalsView } from "@/views/ApprovalsView";
import { SettingsView } from "@/views/SettingsView";

export type View = "overview" | "conversation" | "approvals" | "settings";

interface NavItem {
  view: View;
  label: string;
  icon: LucideIcon;
}

const NAV: NavItem[] = [
  { view: "overview", label: "Overview", icon: LayoutDashboard },
  { view: "conversation", label: "Conversation", icon: MessagesSquare },
  { view: "approvals", label: "Approvals", icon: ShieldCheck },
  { view: "settings", label: "Settings", icon: Settings2 },
];

const TITLES: Record<View, string> = {
  overview: "Overview",
  conversation: "Conversation",
  approvals: "Approvals",
  settings: "Settings",
};

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
  const [view, setView] = useState<View>("overview");
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [feedbackOpen, setFeedbackOpen] = useState(false);
  const feed = useCompany(client, company, initialStatus);

  const pending = feed.status.pending_approvals;

  const noteSystem = (line: string) =>
    setMessages((m) => [...m, { id: nextMessageId(), from: "system", text: line }]);

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
          <SidebarGroup>
            <SidebarGroupLabel>Operate</SidebarGroupLabel>
            <SidebarMenu>
              {NAV.map((item) => (
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
        </SidebarContent>
        <SidebarFooter>
          <SidebarMenu>
            <SidebarMenuItem>
              <SidebarMenuButton tooltip="Flag something" onClick={() => setFeedbackOpen(true)}>
                <Flag />
                <span>Flag something</span>
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
              messages={messages}
              setMessages={setMessages}
              onReply={() => void feed.refresh()}
            />
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
          {view === "settings" && (
            <SettingsView client={client} company={company} feed={feed} onFlag={() => setFeedbackOpen(true)} />
          )}
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
