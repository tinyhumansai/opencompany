import { useEffect, useRef } from "react";
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
 * bar lives here: the company's state and the switcher. Collapsing is its own
 * control at the top of the sidebar (`SidebarCollapseToggle`). Theming and
 * flagging are deliberately absent — Settings owns both, under Appearance and
 * "Something off?", and a second entry point would just be two places to keep
 * in step.
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

    </SidebarMenu>
  );
}

/**
 * The collapse toggle, at the top of the sidebar.
 *
 * It sits above the nav rather than among the footer controls, because it is
 * the one control here that acts on the sidebar itself — everything below it
 * navigates. Expanded, it stays quiet and reads as another row; collapsed, it
 * takes the primary fill, so the single control that gets the rail back is the
 * one thing standing out in a column of identical icons.
 */
export function SidebarCollapseToggle() {
  const { toggleSidebar, state } = useSidebar();
  const collapsed = state === "collapsed";

  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <SidebarMenuButton
          tooltip={collapsed ? "Expand sidebar" : "Collapse sidebar"}
          onClick={toggleSidebar}
          className={cn(
            collapsed &&
              "bg-primary text-primary-foreground hover:bg-primary/90 hover:text-primary-foreground active:bg-primary/90 active:text-primary-foreground",
          )}
        >
          <PanelLeft className={cn("transition-transform", collapsed && "rotate-180")} />
          <span>Collapse</span>
        </SidebarMenuButton>
      </SidebarMenuItem>
    </SidebarMenu>
  );
}

/**
 * Collapse the app's sidebar when entering a view that carries a nav of its
 * own — Chat's channel rail, Settings' sub-page rail. Two full-width sidebars
 * side by side leaves the actual content squeezed into what is left.
 *
 * It only ever collapses. Re-expanding on the way out would undo a collapse
 * the operator chose for themselves, so leaving is their move to make.
 *
 * It fires once per arrival, tracked in a ref. `setOpen` is rebuilt whenever
 * the sidebar's own open state changes, so an effect keyed on its identity
 * would re-collapse the instant you expanded — pinning you shut for as long as
 * you stayed on the view.
 */
export function AutoCollapse({ view }: { view: View }) {
  const { setOpen } = useSidebar();
  const acted = useRef<View | null>(null);

  useEffect(() => {
    if (acted.current === view) return;
    acted.current = view;
    if (NESTED_NAV_VIEWS.has(view)) setOpen(false);
  }, [view, setOpen]);

  return null;
}

/** The views that bring their own sidebar. */
const NESTED_NAV_VIEWS = new Set<View>(["chat", "settings"]);
