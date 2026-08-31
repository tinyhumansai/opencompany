import { lazy, Suspense } from "react";
import { CreditCard, LayoutDashboard, Wallet, type LucideIcon } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { RouteLoading } from "@/components/route-loading";
import { cn } from "@/lib/utils";
import { InvoicingView } from "@/views/finance/InvoicingView";
import { WalletView } from "@/views/finance/WalletView";

// Recharts-backed and only used here — load the ledger overview on demand, as
// it was loaded when it hung off the shell directly.
const FinancesView = lazy(() =>
  import("@/views/FinancesView").then((m) => ({ default: m.FinancesView })),
);

/**
 * The sub-pages under Finance. The id is the hash's second segment.
 *
 * Three, not two. Overview is the ledger projection (`GET …/finances`, folded by
 * `metering::finances_from` from the company's own ledger and its manifest
 * `[budget]`) — the company's internal accounting, which owes nothing to either
 * provider. It leads because it is the one page that has something to show on a
 * host where nothing is connected yet, so the section is never an empty shell.
 */
export const FINANCE_PAGES = [
  {
    id: "overview",
    label: "Overview",
    icon: LayoutDashboard,
    hint: "Balance, budget and spend from the ledger",
  },
  {
    id: "invoicing",
    label: "Invoicing",
    icon: CreditCard,
    hint: "What customers owe, through Chargebee",
  },
  {
    id: "wallet",
    label: "Wallet",
    icon: Wallet,
    hint: "The PayPal balance and what moved through it",
  },
] as const satisfies readonly { id: string; label: string; icon: LucideIcon; hint: string }[];

export type FinancePage = (typeof FINANCE_PAGES)[number]["id"];

const DEFAULT_PAGE: FinancePage = "overview";

/** Whether a hash segment names a real sub-page. */
export function resolveFinancePage(sub: string | null): FinancePage {
  return FINANCE_PAGES.some((p) => p.id === sub) ? (sub as FinancePage) : DEFAULT_PAGE;
}

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** The hash's second segment, e.g. `wallet` in `#/finances/wallet`. */
  sub: string | null;
  onNavigate: (page: FinancePage) => void;
}

/**
 * Finance, as a section rather than a page.
 *
 * Modelled on `SettingsSection`, deliberately: a sub-sidebar on `sm:` and up, a
 * scrolling chip row below it, and each sub-page its own route
 * (`#/finances/wallet`) so it is linkable and survives a refresh exactly as a
 * top-level view does.
 *
 * # Why this replaced Settings → Billing
 *
 * Chargebee and PayPal were configured at `#/settings/billing`, which is the
 * right home for a credential and the wrong home for everything else about
 * money: a settings tab is a place an operator visits once, and invoices and a
 * balance are read repeatedly. "Billing" was also ambiguous in a product that is
 * itself billed — an operator reading it reasonably expects *what OpenCompany
 * charges me*, which is Settings → Usage. The credential forms now sit in a
 * collapsible panel at the top of the page whose data they unlock.
 *
 * # The `key` on each provider page is load-bearing
 *
 * Both hold typed-but-unsaved credentials. `key={company}` remounts on a company
 * switch rather than re-running against carried-over state — without it, an
 * operator who typed a Chargebee key for one company, switched, and pressed Save
 * writes that credential into the other company's secret store. Clearing fields
 * by hand covers the ones somebody remembered; a remount covers all of them, and
 * also makes `company` constant for the instance's lifetime, so a slow response
 * from a previous company cannot land on a later one's view.
 */
export function FinanceSection({ client, company, sub, onNavigate }: Props) {
  const page = resolveFinancePage(sub);

  return (
    <div className="flex min-h-0 flex-1">
      <nav
        aria-label="Finance"
        className="hidden w-60 shrink-0 flex-col gap-0.5 overflow-y-auto border-r p-3 sm:flex"
      >
        {/* A visual caption for the rail, not a heading. The `nav` is already
            named by its `aria-label`, so an `h2` here added nothing for a screen
            reader and broke the document outline: the rail renders before the
            sub-page, so heading navigation met a section-level heading ahead of
            the page's own `h1` (issue #1392). */}
        <div className="px-2 pb-2 pt-1 text-xs font-medium text-muted-foreground">
          Finance
        </div>
        {FINANCE_PAGES.map((item) => (
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

      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <div className="flex gap-1 overflow-x-auto border-b p-2 sm:hidden">
          {FINANCE_PAGES.map((item) => (
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

        {page === "overview" && (
          <Suspense fallback={<RouteLoading title="Finances" label="Loading finances…" />}>
            <FinancesView client={client} company={company} />
          </Suspense>
        )}
        {page === "invoicing" && (
          <InvoicingView key={company ?? "self"} client={client} company={company} />
        )}
        {page === "wallet" && (
          <WalletView key={company ?? "self"} client={client} company={company} />
        )}
      </div>
    </div>
  );
}
