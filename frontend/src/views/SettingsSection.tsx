import { Blocks, Plug, type LucideIcon, Settings2, UserCog } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyFeed } from "@/hooks/use-company";
import { cn } from "@/lib/utils";
import { ConnectionsView } from "@/views/ConnectionsView";
import { McpServersView } from "@/views/McpServersView";
import { PeopleView } from "@/views/PeopleView";
import { SettingsView } from "@/views/SettingsView";

/** The sub-pages that live under Settings. The id is the hash's second segment. */
export const SETTINGS_PAGES = [
  { id: "general", label: "General", icon: Settings2, hint: "Connection, lifecycle, domain, mail" },
  { id: "people", label: "People", icon: UserCog, hint: "Who can sign in, and as what" },
  { id: "connections", label: "Connections", icon: Plug, hint: "Third-party accounts" },
  { id: "mcp", label: "MCP Servers", icon: Blocks, hint: "Tool servers and their tools" },
] as const satisfies readonly { id: string; label: string; icon: LucideIcon; hint: string }[];

export type SettingsPage = (typeof SETTINGS_PAGES)[number]["id"];

const DEFAULT_PAGE: SettingsPage = "general";

/** Whether a hash segment names a real sub-page. */
function resolvePage(sub: string | null): SettingsPage {
  return SETTINGS_PAGES.some((p) => p.id === sub) ? (sub as SettingsPage) : DEFAULT_PAGE;
}

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  feed: CompanyFeed;
  /** The hash's second segment, e.g. `people` in `#/settings/people`. */
  sub: string | null;
  onNavigate: (page: SettingsPage) => void;
  onFlag: () => void;
}

/**
 * Settings, as a section rather than a page.
 *
 * Everything that configures the company rather than running it lives here,
 * behind a sub-sidebar: the connection and lifecycle controls, who can sign in,
 * which third-party accounts are linked, and which tool servers are installed.
 * Each is its own route (`#/settings/people`), so a sub-page is linkable and
 * survives a refresh exactly as a top-level view does.
 */
export function SettingsSection({ client, company, feed, sub, onNavigate, onFlag }: Props) {
  const page = resolvePage(sub);

  return (
    <div className="flex min-h-0 flex-1">
      <nav
        aria-label="Settings"
        className="hidden w-60 shrink-0 flex-col gap-0.5 overflow-y-auto border-r p-3 sm:flex"
      >
        <h2 className="px-2 pb-2 pt-1 text-xs font-medium text-muted-foreground">Settings</h2>
        {SETTINGS_PAGES.map((item) => (
          <button
            key={item.id}
            type="button"
            onClick={() => onNavigate(item.id)}
            aria-current={page === item.id ? "page" : undefined}
            className={cn(
              "flex items-start gap-2.5 rounded-lg px-2 py-2 text-left transition-colors",
              page === item.id ? "bg-accent text-accent-foreground" : "hover:bg-accent/50",
            )}
          >
            <item.icon className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
            <span className="min-w-0">
              <span className="block text-sm font-medium">{item.label}</span>
              <span className="block text-xs text-muted-foreground">{item.hint}</span>
            </span>
          </button>
        ))}
      </nav>

      {/* On a narrow screen the rail collapses to a scrolling row of chips, so
          the sub-pages stay reachable without a second drawer. */}
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <div className="flex gap-1 overflow-x-auto border-b p-2 sm:hidden">
          {SETTINGS_PAGES.map((item) => (
            <button
              key={item.id}
              type="button"
              onClick={() => onNavigate(item.id)}
              aria-current={page === item.id ? "page" : undefined}
              className={cn(
                "shrink-0 rounded-full px-3 py-1 text-xs font-medium transition-colors",
                page === item.id ? "bg-accent text-accent-foreground" : "text-muted-foreground",
              )}
            >
              {item.label}
            </button>
          ))}
        </div>

        {page === "general" && (
          <SettingsView client={client} company={company} feed={feed} onFlag={onFlag} />
        )}
        {page === "people" && <PeopleView client={client} company={company} />}
        {page === "connections" && <ConnectionsView client={client} company={company} />}
        {page === "mcp" && <McpServersView client={client} company={company} />}
      </div>
    </div>
  );
}
