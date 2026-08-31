import { lazy, Suspense } from "react";

import type { OpenCompanyClient } from "@/api/client";
import { RouteLoading } from "@/components/route-loading";
import type { CompanyFeed } from "@/hooks/use-company";
import { cn } from "@/lib/utils";
import { InferenceView } from "@/views/InferenceView";
import { HostingView } from "@/views/HostingView";
import { SearchView } from "@/views/SearchView";
import { McpServersView } from "@/views/McpServersView";
import { OAuthView } from "@/views/OAuthView";
import { PeopleView } from "@/views/PeopleView";
import { SkillsView } from "@/views/SkillsView";
import { SettingsView } from "@/views/SettingsView";
import {
  SETTINGS_PAGE_GROUPS,
  SETTINGS_PAGES,
  resolveSettingsPage,
  type SettingsPage,
} from "@/views/settings-pages";

// The table itself lives in `settings-pages.ts` so that prose pointing at a
// sub-page can name one without importing this section and everything under it.
export { SETTINGS_PAGES, type SettingsPage };

// Recharts is heavy and only used here — load the usage dashboard on demand.
const UsageView = lazy(() => import("@/views/UsageView").then((m) => ({ default: m.UsageView })));

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  feed: CompanyFeed;
  /** The hash's second segment, e.g. `people` in `#/settings/people`. */
  sub: string | null;
  onFlag: () => void;
  /** Start the reset (archive + start clean) flow for the active company (#1807). */
  onResetCompany?: (id: string, name: string) => void;
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
export function SettingsSection({ client, company, feed, sub, onFlag, onResetCompany }: Props) {
  const page = resolveSettingsPage(sub);
  const activePage = SETTINGS_PAGES.find((item) => item.id === page)!;

  return (
    <div className="flex min-h-0 flex-1">
      <nav
        aria-label="Settings"
        className="hidden w-60 shrink-0 flex-col gap-0.5 overflow-y-auto border-r p-3 lg:flex"
      >
        {/* A visual caption for the rail, not a heading. The `nav` is already
            named by its `aria-label`, so an `h2` here added nothing for a screen
            reader and broke the document outline: the rail renders before the
            sub-page, so heading navigation met a section-level heading ahead of
            the page's own `h1` (issue #1392). */}
        <div className="px-2 pb-2 pt-1 text-xs font-medium text-muted-foreground">
          Settings
        </div>
        {SETTINGS_PAGE_GROUPS.map((group) => (
          <section key={group.id} aria-labelledby={`settings-group-${group.id}`}>
            {/* Named by `aria-labelledby`, which resolves against any element,
                so the group keeps its accessible name without sitting in the
                document outline ahead of the sub-page's `h1` (issue #1392). */}
            <div
              id={`settings-group-${group.id}`}
              className="px-2 pb-1 pt-3 text-xs font-medium tracking-wide text-muted-foreground uppercase first:pt-1"
            >
              {group.label}
            </div>
            {SETTINGS_PAGES.filter((item) => item.group === group.id).map((item) => (
              <a
                key={item.id}
                href={`#/settings/${item.id}`}
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
              </a>
            ))}
          </section>
        ))}
      </nav>

      {/* Below `lg` the rail collapses to a scrolling row of chips, so the
          sub-pages stay reachable without a second drawer. The breakpoint is
          `lg`, not `sm`: from 768–1023px the app sidebar is still on, and a
          second `w-60` rail here would squeeze the settings pane below the
          width its widest card (SMTP) needs, clipping it on both sides
          (issue #1383). */}
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        {/* On the macOS desktop, `ContentSurface` overlays every page's top
            28px with an absolutely-positioned, pointer-events-enabled drag
            band (`WindowDragBar`, z-20) so the window stays movable without a
            native title bar — content-surface.tsx explains the trade-off it
            accepted: that band wins the click over whatever a page draws
            underneath it. This row is the one page top that actually sits in
            that band below `lg`, so without a higher stacking order its links
            are unreachable at 880–1023px window widths on macOS. `relative
            z-30` gives it its own stacking context above the drag band without
            touching `WindowDragBar` itself, whose absolute-overlay contract
            other pages (the graph, the workflow editor) still rely on. */}
        <div className="relative z-30 border-b lg:hidden">
          <div className="flex gap-1 overflow-x-auto p-2">
            {SETTINGS_PAGES.map((item) => (
              <a
                key={item.id}
                href={`#/settings/${item.id}`}
                title={item.hint}
                aria-current={page === item.id ? "page" : undefined}
                className={cn(
                  "shrink-0 rounded-full px-3 py-1 text-xs font-medium transition-colors",
                  page === item.id ? "bg-accent text-accent-foreground" : "text-muted-foreground",
                )}
              >
                {item.label}
              </a>
            ))}
          </div>
          <p className="px-3 pb-2 text-xs text-muted-foreground">{activePage.hint}</p>
        </div>

        {page === "general" && (
          <SettingsView
            client={client}
            company={company}
            feed={feed}
            onFlag={onFlag}
            onResetCompany={onResetCompany}
          />
        )}
        {page === "people" && <PeopleView client={client} company={company} />}
        {page === "oauth" && <OAuthView client={client} company={company} />}
        {page === "mcp" && <McpServersView client={client} company={company} />}
        {page === "inference" && <InferenceView client={client} company={company} />}
        {/* Billing was here. It moved to Finance → Invoicing and Finance → Wallet
            (docs/spec/runtime/finance-console.md): a credential form belongs
            beside the data it unlocks, and "Billing" read as *what OpenCompany
            charges me* — which is Usage, two rows down.
            Same `key` remount as the providers in FinanceSection: it keeps one
            company's typed-but-unsaved token out of another's Save. */}
        {page === "hosting" && (
          <HostingView key={company ?? "self"} client={client} company={company} />
        )}
        {/* Same remount rule, same reason: a search key typed for one company
            must never ride into another company's Save. */}
        {page === "search" && (
          <SearchView key={company ?? "self"} client={client} company={company} />
        )}
        {page === "skills" && <SkillsView client={client} company={company} />}
        {page === "usage" && (
          <Suspense fallback={<RouteLoading title="Usage" label="Loading usage…" />}>
            <UsageView client={client} company={company} />
          </Suspense>
        )}
      </div>
    </div>
  );
}
