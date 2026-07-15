import { Building2, Check, ChevronsUpDown, LayoutGrid } from "lucide-react";

import type { CompanyStatus } from "@/api/types";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  useSidebar,
} from "@/components/ui/sidebar";
import { StatusPill } from "@/components/status-pill";
import { cn } from "@/lib/utils";

interface Props {
  active: CompanyStatus;
  companies: CompanyStatus[];
  onSwitch: (id: string) => void;
  onBackToPicker?: () => void;
}

/** Sidebar header: the current company, with a switcher on multi-company hosts. */
export function CompanySwitcher({ active, companies, onSwitch, onBackToPicker }: Props) {
  const { isMobile } = useSidebar();
  const multi = companies.length > 1 || Boolean(onBackToPicker);

  const brand = (
    <>
      <div className="flex aspect-square size-8 items-center justify-center rounded-lg bg-primary text-primary-foreground">
        <Building2 className="size-4" />
      </div>
      <div className="grid flex-1 text-left leading-tight">
        <span className="truncate text-sm font-semibold">{active.name}</span>
        <span className="truncate text-xs text-muted-foreground">Your company</span>
      </div>
    </>
  );

  if (!multi) {
    return (
      <SidebarMenu>
        <SidebarMenuItem>
          <SidebarMenuButton size="lg" className="cursor-default hover:bg-transparent">
            {brand}
          </SidebarMenuButton>
        </SidebarMenuItem>
      </SidebarMenu>
    );
  }

  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger
            render={
              <SidebarMenuButton
                size="lg"
                className="data-[popup-open]:bg-sidebar-accent data-[popup-open]:text-sidebar-accent-foreground"
              />
            }
          >
            {brand}
            <ChevronsUpDown className="ml-auto size-4" />
          </DropdownMenuTrigger>
          <DropdownMenuContent
            className="min-w-56 rounded-lg"
            align="start"
            side={isMobile ? "bottom" : "right"}
            sideOffset={4}
          >
            <DropdownMenuLabel className="text-xs text-muted-foreground">Companies</DropdownMenuLabel>
            {companies.map((c) => (
              <DropdownMenuItem
                key={c.id}
                onClick={() => onSwitch(c.id)}
                className="gap-2"
              >
                <StatusPill lifecycle={c.lifecycle} className="border-0 bg-transparent px-0" />
                <span className={cn("flex-1 truncate", c.id === active.id && "font-medium")}>
                  {c.name}
                </span>
                {c.id === active.id && <Check className="size-4" />}
              </DropdownMenuItem>
            ))}
            {onBackToPicker && (
              <>
                <DropdownMenuSeparator />
                <DropdownMenuItem onClick={onBackToPicker} className="gap-2 text-muted-foreground">
                  <LayoutGrid className="size-4" />
                  All companies
                </DropdownMenuItem>
              </>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarMenuItem>
    </SidebarMenu>
  );
}
