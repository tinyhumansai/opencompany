import { Building2, Flag, Monitor, Moon, PanelLeft, Sun } from "lucide-react";
import { useTheme } from "next-themes";

import type { CompanyStatus } from "@/api/types";

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
  onFlag: () => void;
}

/**
 * The controls that used to live in each page's top bar.
 *
 * They sit in the sidebar now so no page carries a header of its own — every
 * view gets its whole frame. Each one is a sidebar menu row, so all of them
 * collapse to a tooltipped icon along with the rest of the nav.
 */
export function SidebarControls({
  lifecycleState,
  companies,
  activeCompany,
  onSwitchCompany,
  onBackToPicker,
  onFlag,
}: Props) {
  const { setTheme } = useTheme();
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
        <SidebarMenuButton tooltip="Flag something" onClick={onFlag}>
          <Flag />
          <span>Flag something</span>
        </SidebarMenuButton>
      </SidebarMenuItem>

      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger render={<SidebarMenuButton tooltip="Change theme" />}>
            <Sun className="rotate-0 scale-100 transition-all dark:-rotate-90 dark:scale-0" />
            <Moon className="absolute rotate-90 scale-0 transition-all dark:rotate-0 dark:scale-100" />
            <span>Theme</span>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" side="right">
            <DropdownMenuItem onClick={() => setTheme("light")}>
              <Sun className="size-4" /> Light
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => setTheme("dark")}>
              <Moon className="size-4" /> Dark
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => setTheme("system")}>
              <Monitor className="size-4" /> System
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarMenuItem>

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
