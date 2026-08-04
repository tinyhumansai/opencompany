import { Building2, MessageSquareWarning, PanelLeft } from "lucide-react";

import type { CompanyStatus } from "@/api/types";
import type { View } from "@/components/app-shell";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  useSidebar,
} from "@/components/ui/sidebar";
import { DiscordIcon } from "@/components/discord-icon";
import { lifecycle } from "@/lib/language";
import { DISCORD_INVITE_URL } from "@/lib/links";
import { cn } from "@/lib/utils";

const TONE_DOT: Record<string, string> = {
  live: "bg-emerald-500",
  idle: "bg-amber-500",
  stopped: "bg-rose-500",
};

interface Props {
  /** The company's lifecycle, shown as a dot + label. */
  lifecycleState: string;
  /** Every company this operator can reach, for the switcher. */
  companies: CompanyStatus[];
  activeCompany: string | null;
  onSwitchCompany: (id: string) => void;
  onBackToPicker?: () => void;
  /** The active view, so the Feedback row can show as selected. */
  view: View;
  onNavigate: (view: View) => void;
}

/**
 * The sidebar's standing controls.
 *
 * No page carries a header of its own any more, so what is left of the old top
 * bar lives here: the company's state, the switcher, and the collapse toggle.
 * Theming and flagging are deliberately absent — Settings owns both, under
 * Appearance and "Something off?", and a second entry point would just be two
 * places to keep in step.
 */
export function SidebarControls({
  lifecycleState,
  companies,
  activeCompany,
  onSwitchCompany,
  onBackToPicker,
  view,
  onNavigate,
}: Props) {
  const { toggleSidebar, state } = useSidebar();
  const { label, tone } = lifecycle(lifecycleState);

  return (
    <SidebarMenu>
      {/* Company state. Not a control — `cursor-default` and inert, so it does
          not read as something to press. */}
      <SidebarMenuItem>
        <SidebarMenuButton tooltip={label} className="cursor-default hover:bg-transparent">
          <span className="flex size-4 items-center justify-center">
            <span
              className={cn(
                "size-2 rounded-full",
                TONE_DOT[tone],
                tone === "live" && "animate-pulse",
              )}
            />
          </span>
          <span>{label}</span>
        </SidebarMenuButton>
      </SidebarMenuItem>

      {/* Feedback is a destination like any nav item, but it belongs with the
          standing controls at the bottom rather than in the working nav. */}
      <SidebarMenuItem>
        <SidebarMenuButton
          tooltip="Feedback"
          isActive={view === "feedback"}
          onClick={() => onNavigate("feedback")}
        >
          <MessageSquareWarning />
          <span>Feedback</span>
        </SidebarMenuButton>
      </SidebarMenuItem>

      {/* Switching companies lived in the header block that was removed. It
          is a real capability, so it moves here rather than disappearing —
          hidden entirely when there is only one company to be in. */}
      {(companies.length > 1 || onBackToPicker) && (
        <SidebarMenuItem>
          <DropdownMenu>
            <DropdownMenuTrigger render={<SidebarMenuButton tooltip="Switch company" />}>
              <Building2 />
              <span>Switch company</span>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" side="right">
              {companies.map((c) => (
                <DropdownMenuItem
                  key={c.id}
                  onClick={() => onSwitchCompany(c.id)}
                  className={c.id === activeCompany ? "font-medium" : undefined}
                >
                  {c.name}
                </DropdownMenuItem>
              ))}
              {onBackToPicker && (
                <DropdownMenuItem onClick={onBackToPicker}>All companies…</DropdownMenuItem>
              )}
            </DropdownMenuContent>
          </DropdownMenu>
        </SidebarMenuItem>
      )}

      <SidebarMenuItem>
        <SidebarMenuButton
          tooltip="Join our Discord"
          render={<a href={DISCORD_INVITE_URL} target="_blank" rel="noreferrer" />}
        >
          <DiscordIcon className="size-4" />
          <span>Join our Discord</span>
        </SidebarMenuButton>
      </SidebarMenuItem>

      <SidebarMenuItem>
        <SidebarMenuButton
          tooltip={state === "collapsed" ? "Expand sidebar" : "Collapse sidebar"}
          onClick={toggleSidebar}
        >
          <PanelLeft />
          <span>Collapse</span>
        </SidebarMenuButton>
      </SidebarMenuItem>
    </SidebarMenu>
  );
}
