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
import { Coins, CreditCard, Gauge, Plug, Search, TriangleAlert, Zap } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { CapabilityStatusDto, UsageDto } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { Badge } from "@/components/ui/badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
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
import { cn } from "@/lib/utils";
import { formatUsdCost } from "@/lib/cost";

/** Compact token/number formatting: 1.2M, 340K, 5.1K. */
function compact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(n >= 100_000 ? 0 : 1)}K`;
  return `${n}`;
}

const RANGES: Record<string, number> = { "7d": 7, "30d": 30, "90d": 90 };
const RANGE_LABELS: Record<string, string> = { "7d": "Last 7 days", "30d": "Last 30 days", "90d": "Last 90 days" };

/*
 * Series colours come from the chart tokens, not from hex pairs.
 *
 * This used to carry a `theme: { light, dark }` pair per series — a second,
 * hand-maintained theme switch sitting beside the one the stylesheet already
 * runs. `--chart-*` is defined for both themes in `index.css`, so naming the
 * token deletes the duplication and the drift: a palette change now reaches
 * this chart without anyone remembering it exists.
 *
 * Slot order is the system's: violet leads, cyan follows. See
 * docs/design-system/color.md.
 */
const chartConfig = {
  inputTokens: { label: "Input", color: "var(--chart-1)" },
  outputTokens: { label: "Output", color: "var(--chart-3)" },
  tokens: { label: "Tokens", color: "var(--chart-1)" },
  calls: { label: "Calls", color: "var(--chart-2)" },
} satisfies ChartConfig;

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

// A successful usage read with no activity. Failed reads deliberately do not
// use this shape: zero is a fact about spend, not a fallback for an unknown.
const EMPTY_USAGE: UsageDto = {
  series: [],
  byAgent: [],
  byProvider: [],
  totals: {
    inputTokens: 0,
    outputTokens: 0,
    tokens: 0,
    costUsd: 0,
    oauthCalls: 0,
    connections: 0,
    searchCalls: 0,
  },
};

/** In-depth usage: token burn over time, by agent, and OAuth calls by provider. */
export function UsageView({ client, company }: Props) {
  const [range, setRange] = useState("30d");
  const [data, setData] = useState<UsageDto | null>(null);
  const [usageFailed, setUsageFailed] = useState(false);
  useEffect(() => {
    let alive = true;
    setData(null);
    setUsageFailed(false);
    client
      .usage(range, company)
      .then((usage) => {
        if (alive) setData(usage);
      })
      .catch(() => {
        if (alive) setUsageFailed(true);
      });
    return () => {
      alive = false;
    };
  }, [client, company, range]);
  const displayedData = data ?? EMPTY_USAGE;
  const { totals } = displayedData;

  // Capability budgets (issue #108) — the one live-wired card on this view.
  const [caps, setCaps] = useState<CapabilityStatusDto | null>(null);
  const [capsLoaded, setCapsLoaded] = useState(false);
  const [capsFailed, setCapsFailed] = useState(false);
  useEffect(() => {
    let alive = true;
    setCaps(null);
    setCapsLoaded(false);
    setCapsFailed(false);
    client
      .capabilityStatus(company)
      .then((status) => {
        if (alive) setCaps(status);
      })
      .catch(() => {
        if (alive) setCapsFailed(true);
      })
      .finally(() => {
        if (alive) setCapsLoaded(true);
      });
    return () => {
      alive = false;
    };
  }, [client, company]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="Usage"
        width="6xl"
        description={
          <>
            What your company is burning — tokens and OAuth calls.
          </>
        }
        actions={
          <>
          <Select value={range} onValueChange={(v) => v && setRange(v)} items={RANGE_LABELS}>
            <SelectTrigger className="w-40" aria-label="Usage date range">
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
          </>
        }
      />
      <div className="mx-auto min-h-0 w-full max-w-6xl flex-1 space-y-6 overflow-y-auto px-4 py-6">
        {usageFailed ? (
          <Alert data-testid="usage-load-error">
            <TriangleAlert className="size-4" />
            <AlertDescription>
              Couldn&apos;t check usage. Totals and charts are unavailable; reload to try again.
            </AlertDescription>
          </Alert>
        ) : null}

        {/* KPIs */}
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-5">
          <Kpi icon={Coins} label="Total tokens" value={data ? compact(totals.tokens) : "—"} hint={data ? `${compact(totals.inputTokens)} in · ${compact(totals.outputTokens)} out` : "—"} />
          <Kpi
            icon={CreditCard}
            label="Cost"
            value={
              !data
                ? "—"
                : data.costHidden
                ? "Cost hidden"
                : formatUsdCost({ amountUsd: totals.costUsd }, "total") ?? "$0.00"
            }
            hint={data ? "Source USD · tokens plus metered calls" : "—"}
          />
          <Kpi icon={Zap} label="OAuth calls" value={data ? compact(totals.oauthCalls) : "—"} hint={data ? `Across ${totals.connections} providers` : "—"} />
          <Kpi icon={Plug} label="Connections" value={data ? String(totals.connections) : "—"} hint={data ? "Active integrations" : "—"} />
          {/* Issue #238. Its own KPI rather than a line inside "OAuth calls":
              a search is a priced call on the managed platform, not a connected
              account, and folding it in would overstate integrations. */}
          <Kpi icon={Search} label="Web searches" value={data ? compact(totals.searchCalls ?? 0) : "—"} hint={data ? "Billed per search" : "—"} />
        </div>

        {/* Tokens over time */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Token burn</CardTitle>
            <CardDescription>Input vs. output tokens per day.</CardDescription>
          </CardHeader>
          <CardContent>
            <ChartContainer config={chartConfig} className="h-64 w-full">
              <AreaChart data={displayedData.series} margin={{ left: 4, right: 8, top: 4 }}>
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
                <BarChart data={displayedData.byAgent} layout="vertical" margin={{ left: 8, right: 40 }}>
                  <XAxis type="number" dataKey="tokens" hide />
                  <YAxis type="category" dataKey="name" tickLine={false} axisLine={false} width={96} />
                  <ChartTooltip content={<ChartTooltipContent formatter={(v) => `${compact(Number(v))} tokens`} />} />
                  <Bar dataKey="tokens" fill="var(--color-tokens)" radius={4}>
                    <LabelList dataKey="tokens" position="right" className="fill-muted-foreground" formatter={(v) => compact(Number(v ?? 0))} />
                  </Bar>
                </BarChart>
              </ChartContainer>
              <div className="mt-3 divide-y rounded-md border">
                {displayedData.byAgent.map((agent) => (
                  <div key={agent.name} className="flex items-center justify-between gap-3 px-3 py-2 text-xs">
                    <span className="min-w-0 truncate">{agent.name}</span>
                    <span className="shrink-0 font-medium tabular-nums">
                      {displayedData.costHidden
                        ? "Cost hidden"
                        : formatUsdCost({ amountUsd: agent.costUsd }, "total") ?? "$0.00"}
                    </span>
                  </div>
                ))}
              </div>
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
                <BarChart data={displayedData.byProvider} layout="vertical" margin={{ left: 8, right: 40 }}>
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
            {capsFailed ? (
              <Alert data-testid="usage-capabilities-load-error">
                <TriangleAlert className="size-4" />
                <AlertDescription>
                  Couldn&apos;t check capability status. Grants and budgets are unavailable; reload to try again.
                </AlertDescription>
              </Alert>
            ) : null}
            {/* Plan-level total token ceiling (issue #188): a HARD stop — once
                spend crosses it the harness refuses to dispatch further turns,
                unlike the soft per-namespace bars below. Rendered first, and on
                its own even when no per-namespace tiers are configured. */}
            {capsLoaded && caps?.configured && caps.total ? (
              <TotalCeilingRow total={caps.total} />
            ) : null}
            {capsLoaded && caps?.configured && caps.tiers && caps.tiers.length > 0 ? (
              <div className="space-y-4">
                {caps.tiers.map((tier) => (
                  <CapabilityRow key={tier.namespace} tier={tier} />
                ))}
              </div>
            ) : capsLoaded && caps?.configured && caps.total ? null : capsFailed ? null : (
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
            {/* Metered web search (issue #238): opt-in, managed-credential-gated,
                and capped per day — its own status row like media/composio. */}
            {capsLoaded && caps ? <SearchStatusRow caps={caps} /> : null}
            {/* Publishing (issue #244, panel half #1192): opt-in per tool grant
                like the three above, but with no credential and no store toggle
                — so two rungs rather than three. */}
            {capsLoaded && caps ? <PublishStatusRow caps={caps} /> : null}
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
  search: "Web search",
};

// Badge variant subset the media status row uses.
export type BadgeVariant = "default" | "secondary" | "destructive" | "outline";

/**
 * The media-generation capability (issue #109) is opt-in per tool grant and
 * gated on a managed platform credential, so it gets its own status row rather
 * than a token-budget bar. Five states: not compiled into this build, unknown,
 * not granted, granted-but-awaiting-credential, and active. A `media` token budget,
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

export function mediaStatus(caps: CapabilityStatusDto): { label: string; variant: BadgeVariant } {
  if (caps.mediaInBuild === false) return { label: "Not in this build", variant: "outline" };
  if (caps.mediaGranted === undefined) return { label: "Couldn't check", variant: "outline" };
  if (caps.mediaGranted === false) return { label: "Not granted", variant: "secondary" };
  if (!caps.mediaCredentialConfigured)
    return { label: "Awaiting credential", variant: "destructive" };
  return { label: "Active", variant: "default" };
}

/**
 * The Composio capability (issue #110) is opt-in per tool grant and gated on a
 * resolved credential, so it gets its own status row like media. Five states —
 * see {@link composioStatus}.
 */
function ComposioStatusRow({ caps }: { caps: CapabilityStatusDto }) {
  const { label, variant } = composioStatus(caps);
  return (
    <div className="flex items-center justify-between gap-3 border-t pt-4 text-sm">
      <div className="space-y-0.5">
        <span className="font-medium">Composio integrations</span>
        <p className="text-xs text-muted-foreground">
          Gmail, Slack &amp; GitHub via Composio — opt-in. Agents can explicitly ask for
          approval before an action with request_approval; policy does not automatically turn
          calls into approval prompts. Read-only mode and the emergency stop still hard-deny
          applicable calls. Runs on this company&apos;s own Composio token when one is set in
          Connections; otherwise on the company&apos;s TinyHumans key, or on the platform identity
          this instance already carries.
        </p>
      </div>
      <Badge variant={variant} className="shrink-0">
        {label}
      </Badge>
    </div>
  );
}

/**
 * The Composio row's five states, in order (issue #886).
 *
 * The credential is resolved over three tiers — a BYO Composio token, the
 * company's TinyHumans key, this instance's platform identity — so "is a token
 * stored" is the wrong question to render. This reads `composioCredentialSource`,
 * the tier the host says the toolbelt actually resolves.
 *
 * The `undefined` rung is load-bearing and must stay above the `"none"` rung.
 * `undefined` means the host did not answer — an older build that does not send
 * the field, or one whose secret store could not be read — and falling through
 * it into the destructive branch is exactly the bug #886 was filed about: a red
 * "no credential" badge over a Composio account that is working. Unknown is
 * shown as unknown, and never in the alarm colour.
 */
export function composioStatus(caps: CapabilityStatusDto): {
  label: string;
  variant: BadgeVariant;
} {
  if (caps.composioInBuild === false) return { label: "Not in this build", variant: "outline" };
  if (caps.composioGranted === undefined) return { label: "Couldn't check", variant: "outline" };
  if (caps.composioGranted === false) return { label: "Not granted", variant: "secondary" };
  if (caps.composioCredentialSource === undefined)
    return { label: "Couldn't check", variant: "outline" };
  if (caps.composioCredentialSource === "none")
    return { label: "Awaiting credential", variant: "destructive" };
  return { label: "Active", variant: "default" };
}

/**
 * Metered web search (issue #238) is opt-in per tool grant, gated on a managed
 * platform credential, and bounded by a per-company daily call cap rather than a
 * token budget — so it gets its own status row like media and composio. The cap
 * is surfaced here because it is the *only* hard boundary on search spend:
 * individual searches deliberately do not park for approval.
 */
function SearchStatusRow({ caps }: { caps: CapabilityStatusDto }) {
  const { label, variant } = searchStatus(caps);
  const cap = caps.searchDailyCallCap;
  // A company searching through its own provider is billed by that provider, so
  // neither the managed-credential sentence nor the daily cap describes it —
  // both would be numbers about somebody else's bill.
  const ownProvider = Boolean(caps.searchProvider && caps.searchProvider !== "managed");
  return (
    <div className="flex items-center justify-between gap-3 border-t pt-4 text-sm">
      <div className="space-y-0.5">
        <span className="font-medium">Web search</span>
        <p className="text-xs text-muted-foreground">
          {ownProvider
            ? `Source discovery for research — running on this company's own ${caps.searchProvider} account, billed there rather than here, so the daily cap does not apply.`
            : "Source discovery for research — opt-in, runs on the managed platform credential, and billed per search."}{" "}
          {ownProvider
            ? ""
            : typeof cap === "number"
              ? `Capped at ${cap} searches per day; past that the tool refuses rather than returning nothing.`
              : "Capped per day; past that the tool refuses rather than returning nothing."}
        </p>
      </div>
      <Badge variant={variant} className="shrink-0">
        {label}
      </Badge>
    </div>
  );
}

export function searchStatus(caps: CapabilityStatusDto): { label: string; variant: BadgeVariant } {
  if (caps.searchInBuild === false) return { label: "Not in this build", variant: "outline" };
  if (caps.searchGranted === undefined) return { label: "Couldn't check", variant: "outline" };
  if (caps.searchGranted === false) return { label: "Not granted", variant: "secondary" };
  // A company on its own provider is working whatever the host's managed
  // credential says, and the daily cap below does not apply to it — that cap
  // bounds the platform's bill, and this company is paying its own. Checked
  // before both, or a self-hosted instance with no platform credential would
  // badge a working search "Awaiting credential".
  if (caps.searchProvider && caps.searchProvider !== "managed")
    return { label: "Own provider", variant: "default" };
  if (!caps.searchCredentialConfigured)
    return { label: "Awaiting credential", variant: "destructive" };
  // A zero cap leaves the grant in place but spends nothing — say so rather
  // than reporting "Active" for a tool that will refuse every call.
  if (caps.searchDailyCallCap === 0) return { label: "Paused (cap 0)", variant: "destructive" };
  return { label: "Active", variant: "default" };
}

/**
 * Publishing (issue #244) is opt-in per tool grant like media/composio/search,
 * so it gets its own status row — but it is the one capability on this card with
 * **no third rung**. There is no credential to configure and no store to switch
 * on: the artifact store is always wired, so a "store configured" badge could
 * only ever read green. States: not compiled into this build, unknown, not
 * granted, active — see {@link publishStatus}.
 */
function PublishStatusRow({ caps }: { caps: CapabilityStatusDto }) {
  const { label, variant } = publishStatus(caps);
  return (
    <div className="flex items-center justify-between gap-3 border-t pt-4 text-sm">
      <div className="space-y-0.5">
        <span className="font-medium">Publishing deliverables</span>
        <p className="text-xs text-muted-foreground">
          Handing a file a teammate wrote to the board as a deliverable — the only way work in a
          teammate&apos;s sandbox becomes something you can open. It rides the same{" "}
          <code className="font-mono">files</code> / <code className="font-mono">docs</code> grant
          as their file tools: add one of those to{" "}
          <code className="font-mono">[tools].allow</code> in the company manifest. Unlike{" "}
          <code className="font-mono">repo</code>, a broad <code className="font-mono">*</code>{" "}
          <em>does</em> confer it — publishing spends nothing and reaches nothing outside this
          company&apos;s own board.
        </p>
      </div>
      <Badge variant={variant} className="shrink-0">
        {label}
      </Badge>
    </div>
  );
}

/**
 * The publishing row's four states, in order (issue #1192).
 *
 * The `undefined` rung takes {@link composioStatus}'s stricter shape and NOT
 * {@link mediaStatus}'s: media collapses "absent" into `!granted` and paints an
 * older host — or one that did not answer — as a definite "Not granted", which
 * is the #886 lie in miniature. An unanswered host is unknown, and unknown is
 * shown as unknown.
 *
 * There is no credential rung below these. Publishing has no credential and no
 * store toggle, so `granted && inBuild` is the whole of the verdict.
 */
export function publishStatus(caps: CapabilityStatusDto): {
  label: string;
  variant: BadgeVariant;
} {
  if (caps.publishInBuild === false) return { label: "Not in this build", variant: "outline" };
  if (caps.publishGranted === undefined) return { label: "Couldn't check", variant: "outline" };
  if (caps.publishGranted === false) return { label: "Not granted", variant: "secondary" };
  return { label: "Active", variant: "default" };
}

// A budget large enough that we treat it as effectively unlimited (the backend
// sends u64::MAX for the `unlimited` tier, which arrives as a huge float).
const UNLIMITED_THRESHOLD = 1e15;

/**
 * The plan-level total token ceiling (issue #188). Unlike a per-namespace tier
 * bar — a soft gate that only trims exec tools — crossing this is a hard stop:
 * the harness refuses to dispatch further turns this period. Rendered with
 * stronger emphasis (its own labelled bar + a "Dispatch paused" badge when
 * exhausted) so an operator can tell the hard cap apart from the soft ones.
 */
function TotalCeilingRow({ total }: { total: NonNullable<CapabilityStatusDto["total"]> }) {
  const unlimited = total.budgetTokens >= UNLIMITED_THRESHOLD;
  const pct = unlimited
    ? Math.min(100, total.spentTokens > 0 ? 2 : 0)
    : total.budgetTokens > 0
      ? Math.min(100, (total.spentTokens / total.budgetTokens) * 100)
      : 100;

  return (
    <div className="space-y-1.5 rounded-md border p-3">
      <div className="flex items-center justify-between gap-2 text-sm">
        <span className="font-medium">Total token ceiling</span>
        {total.exhausted ? (
          <Badge variant="destructive">Dispatch paused</Badge>
        ) : (
          <span className="text-xs text-muted-foreground tabular-nums">
            {compact(total.remainingTokens)} left
          </span>
        )}
      </div>
      <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
        <div
          className={cn(
            "h-full rounded-full transition-all",
            total.exhausted ? "bg-destructive" : "bg-primary",
          )}
          style={{ width: `${pct}%` }}
        />
      </div>
      <div className="flex items-center justify-between text-xs text-muted-foreground tabular-nums">
        <span>{compact(total.spentTokens)} spent</span>
        <span>{unlimited ? "Unlimited" : `${compact(total.budgetTokens)} ceiling`}</span>
      </div>
      <p className="text-xs text-muted-foreground">
        A hard cap on total spend this period. When it&apos;s reached, new turns are
        refused until the period resets — separate from the per-tool budgets below.
      </p>
    </div>
  );
}

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
      <CardContent className="space-y-2">
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
