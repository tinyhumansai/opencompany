import { useMemo, useState } from "react";
import {
  Area,
  AreaChart,
  Bar,
  BarChart,
  CartesianGrid,
  LabelList,
  XAxis,
  YAxis,
} from "recharts";
import { Coins, CreditCard, Plug, Zap } from "lucide-react";

import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  ChartContainer,
  ChartLegend,
  ChartLegendContent,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from "@/components/ui/chart";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { buildUsage, compact, usd } from "@/lib/usage-sample";

const RANGES: Record<string, number> = { "7d": 7, "30d": 30, "90d": 90 };
const RANGE_LABELS: Record<string, string> = { "7d": "Last 7 days", "30d": "Last 30 days", "90d": "Last 90 days" };

const chartConfig = {
  inputTokens: { label: "Input", theme: { light: "#2a78d6", dark: "#3987e5" } },
  outputTokens: { label: "Output", theme: { light: "#008300", dark: "#008300" } },
  tokens: { label: "Tokens", theme: { light: "#2a78d6", dark: "#3987e5" } },
  calls: { label: "Calls", theme: { light: "#1baf7a", dark: "#199e70" } },
} satisfies ChartConfig;

// A stable "today" so the seeded series don't shift between renders.
const TODAY_MS = Date.now();

/** In-depth usage: token burn over time, by agent, and OAuth calls by provider. */
export function UsageView() {
  const [range, setRange] = useState("30d");
  const data = useMemo(() => buildUsage(RANGES[range], TODAY_MS), [range]);
  const { totals } = data;

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-6xl space-y-6 px-4 py-6">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="space-y-1">
            <h2 className="text-2xl font-semibold tracking-tight">Usage</h2>
            <p className="text-sm text-muted-foreground">
              What your company is burning — tokens and OAuth calls.
            </p>
          </div>
          <Select value={range} onValueChange={(v) => v && setRange(v)} items={RANGE_LABELS}>
            <SelectTrigger className="w-40">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {Object.keys(RANGES).map((k) => (
                <SelectItem key={k} value={k}>
                  {RANGE_LABELS[k]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        {/* KPIs */}
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <Kpi icon={Coins} label="Total tokens" value={compact(totals.tokens)} hint={`${compact(totals.inputTokens)} in · ${compact(totals.outputTokens)} out`} />
          <Kpi icon={CreditCard} label="Est. cost" value={usd(totals.costUsd)} hint="At blended token rates" />
          <Kpi icon={Zap} label="OAuth calls" value={compact(totals.oauthCalls)} hint={`Across ${totals.connections} providers`} />
          <Kpi icon={Plug} label="Connections" value={String(totals.connections)} hint="Active integrations" />
        </div>

        {/* Tokens over time */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Token burn</CardTitle>
            <CardDescription>Input vs. output tokens per day.</CardDescription>
          </CardHeader>
          <CardContent>
            <ChartContainer config={chartConfig} className="h-64 w-full">
              <AreaChart data={data.series} margin={{ left: 4, right: 8, top: 4 }}>
                <CartesianGrid vertical={false} />
                <XAxis
                  dataKey="date"
                  tickLine={false}
                  axisLine={false}
                  tickMargin={8}
                  minTickGap={32}
                  tickFormatter={(d: string) => new Date(d).toLocaleDateString(undefined, { month: "short", day: "numeric" })}
                />
                <YAxis tickLine={false} axisLine={false} width={40} tickFormatter={(v: number) => compact(v)} />
                <ChartTooltip content={<ChartTooltipContent labelFormatter={(l) => new Date(l).toLocaleDateString(undefined, { month: "short", day: "numeric" })} />} />
                <ChartLegend content={<ChartLegendContent />} />
                <Area dataKey="inputTokens" name="Input" type="monotone" stackId="t" stroke="var(--color-inputTokens)" fill="var(--color-inputTokens)" fillOpacity={0.2} strokeWidth={2} />
                <Area dataKey="outputTokens" name="Output" type="monotone" stackId="t" stroke="var(--color-outputTokens)" fill="var(--color-outputTokens)" fillOpacity={0.2} strokeWidth={2} />
              </AreaChart>
            </ChartContainer>
          </CardContent>
        </Card>

        <div className="grid gap-4 lg:grid-cols-2">
          {/* Tokens by agent */}
          <Card>
            <CardHeader>
              <CardTitle className="text-base">Tokens by desk</CardTitle>
              <CardDescription>Where the token spend goes.</CardDescription>
            </CardHeader>
            <CardContent>
              <ChartContainer config={chartConfig} className="h-64 w-full">
                <BarChart data={data.byAgent} layout="vertical" margin={{ left: 8, right: 40 }}>
                  <XAxis type="number" dataKey="tokens" hide />
                  <YAxis type="category" dataKey="name" tickLine={false} axisLine={false} width={96} />
                  <ChartTooltip content={<ChartTooltipContent formatter={(v) => `${compact(Number(v))} tokens`} />} />
                  <Bar dataKey="tokens" fill="var(--color-tokens)" radius={4}>
                    <LabelList dataKey="tokens" position="right" className="fill-muted-foreground" formatter={(v) => compact(Number(v ?? 0))} />
                  </Bar>
                </BarChart>
              </ChartContainer>
            </CardContent>
          </Card>

          {/* OAuth by provider */}
          <Card>
            <CardHeader>
              <CardTitle className="text-base">OAuth calls by provider</CardTitle>
              <CardDescription>Third-party API burn.</CardDescription>
            </CardHeader>
            <CardContent>
              <ChartContainer config={chartConfig} className="h-64 w-full">
                <BarChart data={data.byProvider} layout="vertical" margin={{ left: 8, right: 40 }}>
                  <XAxis type="number" dataKey="calls" hide />
                  <YAxis type="category" dataKey="provider" tickLine={false} axisLine={false} width={96} />
                  <ChartTooltip content={<ChartTooltipContent formatter={(v) => `${compact(Number(v))} calls`} />} />
                  <Bar dataKey="calls" fill="var(--color-calls)" radius={4}>
                    <LabelList dataKey="calls" position="right" className="fill-muted-foreground" formatter={(v) => compact(Number(v ?? 0))} />
                  </Bar>
                </BarChart>
              </ChartContainer>
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
}: {
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  value: string;
  hint: string;
}) {
  return (
    <Card>
      <CardContent className="space-y-2 py-5">
        <div className="flex items-center justify-between">
          <span className="text-sm font-medium text-muted-foreground">{label}</span>
          <Icon className="size-4 text-muted-foreground" />
        </div>
        <div className="text-2xl font-semibold tracking-tight tabular-nums">{value}</div>
        <p className="text-xs text-muted-foreground">{hint}</p>
      </CardContent>
    </Card>
  );
}
