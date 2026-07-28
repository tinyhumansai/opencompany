import { useEffect, useState } from "react";
import { Bar, BarChart, LabelList, XAxis, YAxis } from "recharts";
import { ArrowDownLeft, ArrowUpRight, Coins, PiggyBank, TrendingUp, Wallet } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { FinancesDto } from "@/api/types";
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

function usd(n: number, maxFrac = 2): string {
  return n.toLocaleString(undefined, { style: "currency", currency: "USD", maximumFractionDigits: maxFrac });
}

const chartConfig = {
  amount: { label: "Spend", theme: { light: "#2a78d6", dark: "#3987e5" } },
} satisfies ChartConfig;

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

// The empty finances read — shown before the first fetch resolves and as the
// fallback when a host doesn't expose the surface yet (404). Real-or-zero: no
// fabricated balance, budget, or transactions.
const EMPTY_FINANCE: FinancesDto = {
  balanceUsd: 0,
  budgetUsd: 0,
  spentUsd: 0,
  revenueUsd: 0,
  netUsd: 0,
  byCategory: [],
  transactions: [],
};

/** Company finances: balance, budget, revenue, spend by category, transactions. */
export function FinancesView({ client, company }: Props) {
  const [data, setData] = useState<FinancesDto>(EMPTY_FINANCE);
  useEffect(() => {
    let alive = true;
    client
      .finances(company)
      .then((finances) => {
        if (alive) setData(finances);
      })
      // Hosts without the surface (older builds) 404 — show the empty state.
      .catch(() => {
        if (alive) setData(EMPTY_FINANCE);
      });
    return () => {
      alive = false;
    };
  }, [client, company]);
  // Guard against an uncapped budget (0) so the bar reads 0%, not NaN.
  const budgetPct =
    data.budgetUsd > 0 ? Math.min(100, Math.round((data.spentUsd / data.budgetUsd) * 100)) : 0;

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-6xl space-y-6 px-4 py-6">
        <div className="space-y-1">
          <h2 className="text-2xl font-semibold tracking-tight">Finances</h2>
          <p className="text-sm text-muted-foreground">
            What your company is earning and spending this month.
          </p>
        </div>

        {/* KPIs */}
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <Kpi icon={Wallet} label="Wallet balance" value={usd(data.balanceUsd)} hint="Available USDC" />
          <Kpi icon={TrendingUp} label="Revenue" value={usd(data.revenueUsd, 0)} hint="This month" />
          <Kpi icon={Coins} label="Spend" value={usd(data.spentUsd, 0)} hint={`of ${usd(data.budgetUsd, 0)} budget`} />
          <Kpi
            icon={PiggyBank}
            label="Net"
            value={`${data.netUsd >= 0 ? "+" : "−"}${usd(Math.abs(data.netUsd), 0)}`}
            hint="Revenue − spend"
            valueClass={data.netUsd >= 0 ? "text-emerald-600 dark:text-emerald-400" : "text-rose-600 dark:text-rose-400"}
          />
        </div>

        {/* Budget progress */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Monthly budget</CardTitle>
            <CardDescription>
              {usd(data.spentUsd, 0)} of {usd(data.budgetUsd, 0)} used · {usd(data.budgetUsd - data.spentUsd, 0)} left
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-2">
            <div className="h-2.5 w-full overflow-hidden rounded-full bg-muted">
              <div
                className={cn("h-full rounded-full", budgetPct >= 90 ? "bg-rose-500" : budgetPct >= 70 ? "bg-amber-500" : "bg-emerald-500")}
                style={{ width: `${budgetPct}%` }}
              />
            </div>
            <p className="text-xs text-muted-foreground">{budgetPct}% of budget used</p>
          </CardContent>
        </Card>

        <div className="grid gap-4 lg:grid-cols-2">
          {/* Spend by category */}
          <Card>
            <CardHeader>
              <CardTitle className="text-base">Spend by category</CardTitle>
              <CardDescription>Where the money goes.</CardDescription>
            </CardHeader>
            <CardContent>
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
            </CardContent>
          </Card>

          {/* Transactions */}
          <Card>
            <CardHeader>
              <CardTitle className="text-base">Recent transactions</CardTitle>
              <CardDescription>Latest inflows and outflows.</CardDescription>
            </CardHeader>
            <CardContent>
              <ul className="divide-y">
                {data.transactions.map((t) => {
                  const inflow = t.direction === "in";
                  return (
                    <li key={t.id} className="flex items-center gap-3 py-2.5 first:pt-0 last:pb-0">
                      <span
                        className={cn(
                          "flex size-8 shrink-0 items-center justify-center rounded-full",
                          inflow ? "bg-emerald-500/12 text-emerald-600 dark:text-emerald-400" : "bg-muted text-muted-foreground",
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
                      <span className={cn("shrink-0 text-sm font-medium tabular-nums", inflow ? "text-emerald-600 dark:text-emerald-400" : "text-foreground")}>
                        {inflow ? "+" : "−"}
                        {usd(t.amountUsd)}
                      </span>
                    </li>
                  );
                })}
              </ul>
            </CardContent>
          </Card>
        </div>
      </div>
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
      <CardContent className="space-y-2 py-5">
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
