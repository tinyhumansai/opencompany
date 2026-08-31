// Issue #302: unmounted from the console — hidden, not retired. The host's
// finances routes, economy state and tests are unchanged; re-listing "finances"
// in `app-shell.tsx`'s `View`/`NAV` (behind a `lazy()` import, as it was)
// brings this surface back. Do not delete it as dead code.
import { useEffect, useState } from "react";
import { Bar, BarChart, LabelList, XAxis, YAxis } from "recharts";
import { ArrowDownLeft, ArrowUpRight, CircleAlert, Coins, PiggyBank, TrendingUp, Wallet } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type FinancesDto } from "@/api/types";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from "@/components/ui/chart";
import { cn } from "@/lib/utils";
import { PageHeader } from "@/components/page-header";

function usd(n: number, maxFrac = 2): string {
  return (n === 0 ? 0 : n).toLocaleString(undefined, {
    style: "currency",
    currency: "USD",
    maximumFractionDigits: maxFrac,
  });
}

/* The brand leads slot 1, and the token already themes itself — see the note
   in UsageView on why the hex pair this replaced was a liability. */
const chartConfig = {
  amount: { label: "Spend", color: "var(--chart-1)" },
} satisfies ChartConfig;

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

type FinanceLoad = "loading" | "ready" | "unavailable" | "error";

/** Company finances: balance, budget, revenue, spend by category, transactions. */
export function FinancesView({ client, company }: Props) {
  const [data, setData] = useState<FinancesDto | null>(null);
  const [load, setLoad] = useState<FinanceLoad>("loading");

  useEffect(() => {
    let alive = true;
    setLoad("loading");
    client
      .finances(company)
      .then((finances) => {
        if (alive) {
          setData(finances);
          setLoad("ready");
        }
      })
      .catch((error: unknown) => {
        if (alive) {
          setData(null);
          // A bare 404 (no host error envelope) is the signature of a host that
          // never wired the finances route — the "unavailable" surface. A 404
          // the host answered itself (e.g. `company_not_found`) is a real
          // failure and must go through the normal error state instead.
          const unwired =
            error instanceof ApiError && error.status === 404 && error.code === "http_404";
          setLoad(unwired ? "unavailable" : "error");
        }
      });
    return () => {
      alive = false;
    };
  }, [client, company]);
  /*
    Hoisted above the three load-state returns (codex review, #1785). Two of
    them are terminal — `unavailable` on a host with no finances route, `error`
    on a read that nothing retries — so the page permanently offered a screen
    reader nothing but `FinanceNotice`'s `h2`, with no page-level name at all.

    The notice keeps its own `h2`: it names the *state*, not the page, and the
    two are different sentences ("Finances" / "Could not load finances").
  */
  const header = (
    <PageHeader
      title="Finances"
      width="6xl"
      description={
        <>
          What your company is earning and spending this month.
        </>
      }
    />
  );

  if (load !== "ready" || !data) {
    const notice =
      load === "loading"
        ? { title: "Loading finances…", description: "Reading the company ledger." }
        : load === "unavailable"
          ? {
              title: "Finances unavailable",
              description:
                "This host doesn't expose finances, so there is no ledger data to show.",
            }
          : {
              title: "Could not load finances",
              description: "The company ledger could not be read. Try refreshing the page.",
            };
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <FinanceNotice title={notice.title} description={notice.description} />
      </div>
    );
  }

  const budgetUsd = data.budgetUsd;
  const hasBudget = budgetUsd !== null && budgetUsd > 0;
  const budgetPct = hasBudget ? Math.min(100, Math.round((data.spentUsd / budgetUsd) * 100)) : 0;
  const netSign = data.netUsd > 0 ? "+" : data.netUsd < 0 ? "−" : "";

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {header}
      <div className="mx-auto min-h-0 w-full max-w-6xl flex-1 space-y-6 overflow-y-auto px-4 py-6">
        {/* KPIs */}
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <Kpi icon={Wallet} label="Wallet balance" value={usd(data.balanceUsd)} hint="Ledger balance" />
          <Kpi icon={TrendingUp} label="Revenue" value={usd(data.revenueUsd, 0)} hint="This month" />
          <Kpi icon={Coins} label="Spend" value={usd(data.spentUsd, 0)} hint={budgetUsd === null ? "This month" : `of ${usd(budgetUsd, 0)} budget`} />
          <Kpi
            icon={PiggyBank}
            label="Net"
            value={`${netSign}${usd(Math.abs(data.netUsd), 0)}`}
            hint="Revenue − spend"
            valueClass={data.netUsd > 0 ? "text-status-done-text" : data.netUsd < 0 ? "text-status-failed-text" : undefined}
          />
        </div>

        {/* Budget progress */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Monthly budget</CardTitle>
            <CardDescription>
              {budgetUsd === null
                ? "No monthly budget is set."
                : budgetUsd === 0
                  ? "Spending is capped at $0.00 this month."
                  : `${usd(data.spentUsd, 0)} of ${usd(budgetUsd, 0)} used · ${usd(budgetUsd - data.spentUsd, 0)} left`}
            </CardDescription>
          </CardHeader>
          {hasBudget && (
            <CardContent className="space-y-2">
              <div className="h-2.5 w-full overflow-hidden rounded-full bg-muted">
                <div
                  className={cn("h-full rounded-full", budgetPct >= 90 ? "bg-status-failed" : budgetPct >= 70 ? "bg-status-blocked" : "bg-status-done")}
                  style={{ width: `${budgetPct}%` }}
                />
              </div>
              <p className="text-xs text-muted-foreground">{budgetPct}% of budget used</p>
            </CardContent>
          )}
        </Card>

        <div className="grid gap-4 lg:grid-cols-2">
          {/* Spend by category */}
          <Card>
            <CardHeader>
              <CardTitle className="text-base">Spend by category</CardTitle>
              <CardDescription>Where the money goes.</CardDescription>
            </CardHeader>
            <CardContent>
              {data.byCategory.length === 0 ? (
                <EmptyCard icon={Coins} message="No spending has been recorded yet." />
              ) : (
                <ChartContainer config={chartConfig} className="h-64 w-full">
                  <BarChart data={data.byCategory} layout="vertical" margin={{ left: 8, right: 48 }}>
                    <XAxis type="number" dataKey="amount" hide />
                    <YAxis type="category" dataKey="category" tickLine={false} axisLine={false} width={110} />
                    <ChartTooltip content={<ChartTooltipContent formatter={(v) => usd(Number(v), 0)} />} />
                    <Bar dataKey="amount" fill="var(--color-amount)" radius={4}>
                      <LabelList dataKey="amount" position="right" className="fill-muted-foreground" formatter={(v) => usd(Number(v ?? 0), 0)} />
                    </Bar>
                  </BarChart>
                </ChartContainer>
              )}
            </CardContent>
          </Card>

          {/* Transactions */}
          <Card>
            <CardHeader>
              <CardTitle className="text-base">Recent transactions</CardTitle>
              <CardDescription>Latest inflows and outflows.</CardDescription>
            </CardHeader>
            <CardContent>
              {data.transactions.length === 0 ? (
                <EmptyCard icon={ArrowDownLeft} message="No transactions have been recorded yet." />
              ) : (
                <ul className="divide-y">
                  {data.transactions.map((t) => {
                    const inflow = t.direction === "in";
                    return (
                      <li key={t.id} className="flex items-center gap-3 py-2.5 first:pt-0 last:pb-0">
                        <span
                          className={cn(
                            "flex size-8 shrink-0 items-center justify-center rounded-full",
                            inflow ? "bg-status-done-soft text-status-done-text" : "bg-muted text-muted-foreground",
                          )}
                        >
                          {inflow ? <ArrowDownLeft className="size-4" /> : <ArrowUpRight className="size-4" />}
                        </span>
                        <div className="min-w-0 flex-1">
                          <p className="truncate text-sm font-medium">{t.description}</p>
                          <p className="text-xs text-muted-foreground">
                            {new Date(t.date).toLocaleDateString(undefined, { month: "short", day: "numeric" })} · {t.category}
                          </p>
                        </div>
                        <span className={cn("shrink-0 text-sm font-medium tabular-nums", inflow ? "text-status-done-text" : "text-foreground")}>
                          {inflow ? "+" : "−"}
                          {usd(t.amountUsd)}
                        </span>
                      </li>
                    );
                  })}
                </ul>
              )}
            </CardContent>
          </Card>
        </div>
      </div>
    </div>
  );
}

function FinanceNotice({ title, description }: { title: string; description: string }) {
  return (
    <div className="flex flex-1 overflow-y-auto">
      <div className="m-auto w-full max-w-md px-4 text-center">
        <CircleAlert className="mx-auto size-8 text-muted-foreground" />
        <h2 className="mt-3 text-lg font-semibold">{title}</h2>
        <p className="mt-1 text-sm text-muted-foreground">{description}</p>
      </div>
    </div>
  );
}

function EmptyCard({ icon: Icon, message }: { icon: React.ComponentType<{ className?: string }>; message: string }) {
  return (
    <div className="flex h-64 flex-col items-center justify-center gap-2 text-center text-sm text-muted-foreground">
      <Icon className="size-6" />
      <p>{message}</p>
    </div>
  );
}

function Kpi({
  icon: Icon,
  label,
  value,
  hint,
  valueClass,
}: {
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  value: string;
  hint: string;
  valueClass?: string;
}) {
  return (
    <Card>
      <CardContent className="space-y-2">
        <div className="flex items-center justify-between">
          <span className="text-sm font-medium text-muted-foreground">{label}</span>
          <Icon className="size-4 text-muted-foreground" />
        </div>
        <div className={cn("text-2xl font-semibold tracking-tight tabular-nums", valueClass)}>{value}</div>
        <p className="text-xs text-muted-foreground">{hint}</p>
      </CardContent>
    </Card>
  );
}
