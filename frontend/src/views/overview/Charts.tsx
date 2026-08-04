// The Overview's two charts. One series each, so neither needs a legend — the
// card title names what is plotted.

import { Area, AreaChart, Bar, BarChart, CartesianGrid, LabelList, XAxis, YAxis } from "recharts";

import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { ChartContainer, ChartTooltip, ChartTooltipContent } from "@/components/ui/chart";
import { CHART_CONFIG } from "./palette";
import type { ColumnCount, DayPoint } from "./types";

const dayLabel = (iso: string) =>
  new Date(`${iso}T00:00:00Z`).toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
    timeZone: "UTC",
  });

/**
 * Board activity per day.
 *
 * This counts cards *touched*, not cards finished — the board records only a
 * last-updated stamp, and calling it throughput would overstate what we know.
 */
export function ActivityChart({ series }: { series: DayPoint[] }) {
  const empty = series.every((d) => d.value === 0);
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Board activity</CardTitle>
        <CardDescription>Cards touched per day, last {series.length} days.</CardDescription>
      </CardHeader>
      <CardContent>
        {empty ? (
          <EmptyPlot>No cards have moved in this window.</EmptyPlot>
        ) : (
          <ChartContainer config={CHART_CONFIG} className="h-48 w-full">
            <AreaChart data={series} margin={{ left: 4, right: 8, top: 4 }}>
              <CartesianGrid vertical={false} />
              <XAxis
                dataKey="date"
                tickLine={false}
                axisLine={false}
                tickMargin={8}
                minTickGap={28}
                tickFormatter={dayLabel}
              />
              <YAxis tickLine={false} axisLine={false} width={28} allowDecimals={false} />
              <ChartTooltip content={<ChartTooltipContent labelFormatter={(l) => dayLabel(String(l))} />} />
              <Area
                dataKey="value"
                name="Cards touched"
                type="monotone"
                stroke="var(--color-value)"
                fill="var(--color-value)"
                fillOpacity={0.2}
                strokeWidth={2}
              />
            </AreaChart>
          </ChartContainer>
        )}
      </CardContent>
    </Card>
  );
}

/** Where the board's cards are sitting right now. */
export function BoardShapeChart({ columns }: { columns: ColumnCount[] }) {
  const empty = columns.every((c) => c.count === 0);
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Where the work sits</CardTitle>
        <CardDescription>Cards per board column.</CardDescription>
      </CardHeader>
      <CardContent>
        {empty ? (
          <EmptyPlot>The board is empty.</EmptyPlot>
        ) : (
          <ChartContainer config={CHART_CONFIG} className="h-48 w-full">
            <BarChart data={columns} layout="vertical" margin={{ left: 8, right: 36 }}>
              <XAxis type="number" dataKey="count" hide />
              <YAxis type="category" dataKey="label" tickLine={false} axisLine={false} width={88} />
              <ChartTooltip
                content={<ChartTooltipContent formatter={(v) => `${Number(v)} cards`} />}
              />
              <Bar dataKey="count" fill="var(--color-count)" radius={4}>
                <LabelList dataKey="count" position="right" className="fill-muted-foreground" />
              </Bar>
            </BarChart>
          </ChartContainer>
        )}
      </CardContent>
    </Card>
  );
}

function EmptyPlot({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex h-48 items-center justify-center rounded-lg border border-dashed text-sm text-muted-foreground">
      {children}
    </div>
  );
}
