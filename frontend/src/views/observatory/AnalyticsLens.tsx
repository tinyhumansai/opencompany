/**
 * Cross-run analytics: where the effort goes, and where runs stop.
 *
 * Every chart takes its colours from the design tokens rather than hex, and
 * every one renders its empty state as an empty chart with a caption — never a
 * spinner that never resolves, and never an invented number. A company that has
 * run nothing should be told so.
 */

import { useMemo } from "react";
import { Bar, BarChart, CartesianGrid, XAxis, YAxis } from "recharts";

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
import type { ObservatoryRun } from "@/api/observatory";
import { byAgent, byNode, failureHistogram } from "./model";

const config = {
  inputTokens: { label: "Input", color: "var(--chart-1)" },
  outputTokens: { label: "Output", color: "var(--chart-3)" },
  cachedInputTokens: { label: "Cached", color: "var(--chart-2)" },
  costUsd: { label: "Cost", color: "var(--chart-1)" },
  succeeded: { label: "Succeeded", color: "var(--status-done)" },
  failed: { label: "Failed", color: "var(--status-failed)" },
  blocked: { label: "Blocked", color: "var(--status-blocked)" },
  // A by-design refusal (issue #1809) — never folded into "Succeeded".
  declined: { label: "Declined", color: "var(--status-idle)" },
  n: { label: "Occurrences", color: "var(--chart-4)" },
} satisfies ChartConfig;

/** `12.3k` — a compact tick, so an axis of token counts stays readable. */
function compact(n: number): string {
  if (n < 1000) return String(Math.round(n));
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

/** A chart with nothing to show says so, rather than rendering an empty box. */
function Empty({ children }: { children: string }) {
  return (
    <p className="text-muted-foreground flex h-48 items-center justify-center text-sm">
      {children}
    </p>
  );
}

export function AnalyticsLens({ runs }: { runs: ObservatoryRun[] }) {
  const agents = useMemo(() => byAgent(runs), [runs]);
  const nodes = useMemo(() => byNode(runs), [runs]);
  const failures = useMemo(() => failureHistogram(runs), [runs]);

  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Tokens per agent</CardTitle>
          <CardDescription>
            Input and output are additive; cached input is shown separately because it is
            already included in input.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {agents.length === 0 ? (
            <Empty>No attempts recorded yet.</Empty>
          ) : (
            <ChartContainer config={config} className="h-64 w-full">
              <BarChart data={agents} layout="vertical" margin={{ left: 8, right: 8 }}>
                <CartesianGrid horizontal={false} />
                <XAxis type="number" tickFormatter={compact} tickLine={false} axisLine={false} />
                <YAxis
                  type="category"
                  dataKey="agentId"
                  width={90}
                  tickLine={false}
                  axisLine={false}
                />
                <ChartTooltip content={<ChartTooltipContent />} />
                <ChartLegend content={<ChartLegendContent />} />
                <Bar dataKey="inputTokens" stackId="t" fill="var(--color-inputTokens)" />
                <Bar dataKey="outputTokens" stackId="t" fill="var(--color-outputTokens)" />
                <Bar
                  dataKey="cachedInputTokens"
                  fill="var(--color-cachedInputTokens)"
                />
              </BarChart>
            </ChartContainer>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Cost per agent</CardTitle>
          <CardDescription>USD, summed across every attempt.</CardDescription>
        </CardHeader>
        <CardContent>
          {agents.length === 0 ? (
            <Empty>No attempts recorded yet.</Empty>
          ) : (
            <ChartContainer config={config} className="h-64 w-full">
              <BarChart data={agents} layout="vertical" margin={{ left: 8, right: 8 }}>
                <CartesianGrid horizontal={false} />
                <XAxis
                  type="number"
                  tickFormatter={(v: number) => `$${v.toFixed(2)}`}
                  tickLine={false}
                  axisLine={false}
                />
                <YAxis
                  type="category"
                  dataKey="agentId"
                  width={90}
                  tickLine={false}
                  axisLine={false}
                />
                <ChartTooltip content={<ChartTooltipContent />} />
                <Bar dataKey="costUsd" fill="var(--color-costUsd)" />
              </BarChart>
            </ChartContainer>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Where runs stop</CardTitle>
          <CardDescription>
            Outcomes per graph node. Blocked is kept apart from failed — a node
            waiting on a person has not gone wrong. Declined is kept apart from
            succeeded — a node the compiler refused to automate has not
            succeeded either.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {nodes.length === 0 ? (
            <Empty>No workflow nodes have run yet.</Empty>
          ) : (
            <ChartContainer config={config} className="h-64 w-full">
              <BarChart data={nodes} layout="vertical" margin={{ left: 8, right: 8 }}>
                <CartesianGrid horizontal={false} />
                <XAxis type="number" allowDecimals={false} tickLine={false} axisLine={false} />
                <YAxis
                  type="category"
                  dataKey="nodeId"
                  width={90}
                  tickLine={false}
                  axisLine={false}
                />
                <ChartTooltip content={<ChartTooltipContent />} />
                <ChartLegend content={<ChartLegendContent />} />
                <Bar dataKey="succeeded" stackId="o" fill="var(--color-succeeded)" />
                <Bar dataKey="blocked" stackId="o" fill="var(--color-blocked)" />
                <Bar dataKey="declined" stackId="o" fill="var(--color-declined)" />
                <Bar dataKey="failed" stackId="o" fill="var(--color-failed)" />
              </BarChart>
            </ChartContainer>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">How steps fail</CardTitle>
          <CardDescription>
            The typed failure class of every failed step.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {failures.length === 0 ? (
            <Empty>No steps have failed.</Empty>
          ) : (
            <ChartContainer config={config} className="h-64 w-full">
              <BarChart data={failures} layout="vertical" margin={{ left: 8, right: 8 }}>
                <CartesianGrid horizontal={false} />
                <XAxis type="number" allowDecimals={false} tickLine={false} axisLine={false} />
                <YAxis
                  type="category"
                  dataKey="failure"
                  width={130}
                  tickLine={false}
                  axisLine={false}
                />
                <ChartTooltip content={<ChartTooltipContent />} />
                <Bar dataKey="n" fill="var(--color-n)" />
              </BarChart>
            </ChartContainer>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
