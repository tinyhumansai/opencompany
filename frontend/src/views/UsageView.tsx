import { useEffect, useState } from "react";
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
import { Coins, CreditCard, Gauge, Plug, Zap } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { CapabilityStatusDto, UsageDto } from "@/api/types";
import { Badge } from "@/components/ui/badge";
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
import { compact, usd } from "@/lib/usage-sample";
import { cn } from "@/lib/utils";

const RANGES: Record<string, number> = { "7d": 7, "30d": 30, "90d": 90 };
const RANGE_LABELS: Record<string, string> = { "7d": "Last 7 days", "30d": "Last 30 days", "90d": "Last 90 days" };

const chartConfig = {
  inputTokens: { label: "Input", theme: { light: "#2a78d6", dark: "#3987e5" } },
  outputTokens: { label: "Output", theme: { light: "#008300", dark: "#008300" } },
  tokens: { label: "Tokens", theme: { light: "#2a78d6", dark: "#3987e5" } },
  calls: { label: "Calls", theme: { light: "#1baf7a", dark: "#199e70" } },
} satisfies ChartConfig;

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

// The empty usage read — shown before the first fetch resolves and as the
// fallback when a host doesn't expose the surface yet (404). Real-or-zero: an
// empty series renders flat rather than inventing numbers.
const EMPTY_USAGE: UsageDto = {
  series: [],
  byAgent: [],
  byProvider: [],
  totals: { inputTokens: 0, outputTokens: 0, tokens: 0, costUsd: 0, oauthCalls: 0, connections: 0 },
};

/** In-depth usage: token burn over time, by agent, and OAuth calls by provider. */
export function UsageView({ client, company }: Props) {
  const [range, setRange] = useState("30d");
  const [data, setData] = useState<UsageDto>(EMPTY_USAGE);
  useEffect(() => {
    let alive = true;
    client
      .usage(range, company)
      .then((usage) => {
        if (alive) setData(usage);
      })
      // Hosts without the surface (older builds) 404 — show the empty state
      // rather than fabricating a series.
      .catch(() => {
        if (alive) setData(EMPTY_USAGE);
      });
    return () => {
      alive = false;
    };
  }, [client, company, range]);
  const { totals } = data;

  // Capability budgets (issue #108) — the one live-wired card on this view.
  const [caps, setCaps] = useState<CapabilityStatusDto | null>(null);
  const [capsLoaded, setCapsLoaded] = useState(false);
  useEffect(() => {
    let alive = true;
    client
      .capabilityStatus(company)
      .then((status) => {
        if (alive) setCaps(status);
      })
      // Hosts without the surface (older builds) 404 — treat as unconfigured.
      .catch(() => {
        if (alive) setCaps({ configured: false });
      })
      .finally(() => {
        if (alive) setCapsLoaded(true);
      });
    return () => {
      alive = false;
    };
  }, [client, company]);

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
                <ChartTooltip content={<ChartTooltipContent labelFormatter={(l) => new Date(l as string).toLocaleDateString(undefined, { month: "short", day: "numeric" })} />} />
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

        {/* Capability budgets (issue #108) */}
        <Card>
          <CardHeader>
            <div className="flex items-center gap-2">
              <Gauge className="size-4 text-muted-foreground" />
              <CardTitle className="text-base">Capability budgets</CardTitle>
            </div>
            <CardDescription>
              Token budgets that gate each exec tool family this{" "}
              {caps?.period ?? "period"}. When spend crosses a tier&apos;s budget, its tools
              switch off until the next period.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-4">
            {capsLoaded && caps?.configured && caps.tiers && caps.tiers.length > 0 ? (
              <div className="space-y-4">
                {caps.tiers.map((tier) => (
                  <CapabilityRow key={tier.namespace} tier={tier} />
                ))}
              </div>
            ) : (
              <p className="py-2 text-sm text-muted-foreground">
                {capsLoaded ? "No token plan configured." : "Loading budgets…"}
              </p>
            )}
            {/* Media generation (issue #109): opt-in, managed-credential-gated —
                its own status row, separate from the token-budget bars. */}
            {capsLoaded && caps ? <MediaStatusRow caps={caps} /> : null}
            {/* Per-tenant Composio (issue #110): opt-in, per-tenant-token-gated —
                its own status row like media. */}
            {capsLoaded && caps ? <ComposioStatusRow caps={caps} /> : null}
          </CardContent>
        </Card>
      </div>
    </div>
  );
}

// Friendly labels for the exec tool namespaces a plan can budget.
const NAMESPACE_LABELS: Record<string, string> = {
  shell: "Shell & commands",
  code: "Code & patches",
  web: "Web & HTTP",
  subagent: "Sub-agents",
  media: "Media generation",
  composio: "Composio (Gmail/Slack/GitHub)",
};

// Badge variant subset the media status row uses.
type BadgeVariant = "default" | "secondary" | "destructive" | "outline";

/**
 * The media-generation capability (issue #109) is opt-in per tool grant and
 * gated on a managed platform credential, so it gets its own status row rather
 * than a token-budget bar. Four states: not compiled into this build, not
 * granted, granted-but-awaiting-credential, and active. A `media` token budget,
 * if set, still surfaces as its own bar above via the tiers loop.
 */
function MediaStatusRow({ caps }: { caps: CapabilityStatusDto }) {
  const { label, variant } = mediaStatus(caps);
  return (
    <div className="flex items-center justify-between gap-3 border-t pt-4 text-sm">
      <div className="space-y-0.5">
        <span className="font-medium">Media generation</span>
        <p className="text-xs text-muted-foreground">
          Image &amp; video generation — opt-in, runs on the managed platform credential, and
          every generation is approved before it bills.
        </p>
      </div>
      <Badge variant={variant} className="shrink-0">
        {label}
      </Badge>
    </div>
  );
}

function mediaStatus(caps: CapabilityStatusDto): { label: string; variant: BadgeVariant } {
  if (caps.mediaInBuild === false) return { label: "Not in this build", variant: "outline" };
  if (!caps.mediaGranted) return { label: "Not granted", variant: "secondary" };
  if (!caps.mediaCredentialConfigured)
    return { label: "Awaiting credential", variant: "destructive" };
  return { label: "Active", variant: "default" };
}

/**
 * The Composio capability (issue #110) is opt-in per tool grant and gated on a
 * per-tenant OAuth token, so it gets its own status row like media. Four states:
 * not compiled into this build, not granted, granted-but-awaiting-token, and
 * active. Set the token from Connections.
 */
function ComposioStatusRow({ caps }: { caps: CapabilityStatusDto }) {
  const { label, variant } = composioStatus(caps);
  return (
    <div className="flex items-center justify-between gap-3 border-t pt-4 text-sm">
      <div className="space-y-0.5">
        <span className="font-medium">Composio integrations</span>
        <p className="text-xs text-muted-foreground">
          Gmail, Slack &amp; GitHub via Composio — opt-in, runs on the company&apos;s own OAuth
          token, and every send/authorize is approved before it runs. Set the token in Connections.
        </p>
      </div>
      <Badge variant={variant} className="shrink-0">
        {label}
      </Badge>
    </div>
  );
}

function composioStatus(caps: CapabilityStatusDto): { label: string; variant: BadgeVariant } {
  if (caps.composioInBuild === false) return { label: "Not in this build", variant: "outline" };
  if (!caps.composioGranted) return { label: "Not granted", variant: "secondary" };
  if (!caps.composioTokenConfigured)
    return { label: "Awaiting token", variant: "destructive" };
  return { label: "Active", variant: "default" };
}

// A budget large enough that we treat it as effectively unlimited (the backend
// sends u64::MAX for the `unlimited` tier, which arrives as a huge float).
const UNLIMITED_THRESHOLD = 1e15;

function CapabilityRow({ tier }: { tier: NonNullable<CapabilityStatusDto["tiers"]>[number] }) {
  const label = NAMESPACE_LABELS[tier.namespace] ?? tier.namespace;
  const unlimited = tier.budgetTokens >= UNLIMITED_THRESHOLD;
  const pct = unlimited
    ? Math.min(100, tier.spentTokens > 0 ? 2 : 0)
    : tier.budgetTokens > 0
      ? Math.min(100, (tier.spentTokens / tier.budgetTokens) * 100)
      : 100;

  return (
    <div className="space-y-1.5">
      <div className="flex items-center justify-between gap-2 text-sm">
        <span className="font-medium">{label}</span>
        {tier.exhausted ? (
          <Badge variant="destructive">Tools disabled</Badge>
        ) : (
          <span className="text-xs text-muted-foreground tabular-nums">
            {compact(tier.remainingTokens)} left
          </span>
        )}
      </div>
      <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
        <div
          className={cn(
            "h-full rounded-full transition-all",
            tier.exhausted ? "bg-destructive" : "bg-primary",
          )}
          style={{ width: `${pct}%` }}
        />
      </div>
      <div className="flex items-center justify-between text-xs text-muted-foreground tabular-nums">
        <span>{compact(tier.spentTokens)} spent</span>
        <span>{unlimited ? "Unlimited" : `${compact(tier.budgetTokens)} budget`}</span>
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
