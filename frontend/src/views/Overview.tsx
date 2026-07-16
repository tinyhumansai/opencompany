import { Activity, ArrowRight, Flag, MessagesSquare, ShieldCheck } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { StatusPill } from "@/components/status-pill";
import type { CompanyFeed } from "@/hooks/use-company";
import { approvalSummary, lifecycle, money, timeAgo } from "@/lib/language";
import type { View } from "@/components/app-shell";

interface Props {
  feed: CompanyFeed;
  client: OpenCompanyClient;
  company: string | null;
  onNavigate: (view: View) => void;
  onFlag: () => void;
}

/** The landing surface: a calm summary of where the company stands. */
export function Overview({ feed, onNavigate, onFlag }: Props) {
  const { status, approvals, now } = feed;
  const state = lifecycle(status.lifecycle);
  const pending = status.pending_approvals;

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-5xl space-y-6 px-4 py-6">
        {/* Greeting */}
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="space-y-1">
            <h2 className="text-2xl font-semibold tracking-tight">{status.name}</h2>
            <p className="text-sm text-muted-foreground">
              Here&apos;s where your company stands right now.
            </p>
          </div>
          <StatusPill lifecycle={status.lifecycle} />
        </div>

        {/* Stat cards */}
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          <StatCard
            icon={Activity}
            label="Status"
            value={state.label}
            hint={
              state.tone === "live"
                ? "Running and handling work."
                : state.tone === "stopped"
                  ? "Not currently working."
                  : "Getting things in order."
            }
          />
          <StatCard
            icon={ShieldCheck}
            label="Needs approval"
            value={String(pending)}
            hint={pending === 0 ? "Nothing waiting on you." : "Waiting for your sign-off."}
            action={pending > 0 ? { label: "Review", onClick: () => onNavigate("approvals") } : undefined}
          />
          <StatCard
            icon={MessagesSquare}
            label="Conversation"
            value="Open"
            hint="Ask for an update or hand off a task."
            action={{ label: "Open", onClick: () => onNavigate("conversation") }}
          />
        </div>

        {/* Pending approvals preview */}
        <Card>
          <CardHeader className="flex-row items-center justify-between space-y-0">
            <div className="space-y-1">
              <CardTitle className="text-base">Approvals</CardTitle>
              <CardDescription>The few things parked for your decision.</CardDescription>
            </div>
            {pending > 0 && <Badge variant="secondary">{pending}</Badge>}
          </CardHeader>
          <CardContent>
            {approvals.length === 0 ? (
              <div className="flex items-center gap-3 rounded-lg border border-dashed p-4 text-sm text-muted-foreground">
                <ShieldCheck className="size-4 text-emerald-500" />
                All clear — nothing needs your approval.
              </div>
            ) : (
              <ul className="divide-y">
                {approvals.slice(0, 4).map((a) => (
                  <li key={a.id} className="flex items-center gap-3 py-2.5 first:pt-0 last:pb-0">
                    <span className="min-w-0 flex-1 truncate text-sm">{approvalSummary(a)}</span>
                    <span className="shrink-0 text-xs text-muted-foreground">
                      {a.amount_usd != null && (
                        <span className="font-medium text-foreground">{money(a.amount_usd)} · </span>
                      )}
                      {timeAgo(a.at_millis, now)}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </CardContent>
          {approvals.length > 0 && (
            <CardContent className="pt-0">
              <Button variant="outline" size="sm" className="w-full" onClick={() => onNavigate("approvals")}>
                Review all approvals <ArrowRight className="size-4" />
              </Button>
            </CardContent>
          )}
        </Card>

        {/* Quick actions */}
        <div className="grid gap-3 sm:grid-cols-3">
          <QuickAction icon={MessagesSquare} label="Talk to your company" onClick={() => onNavigate("conversation")} />
          <QuickAction icon={ShieldCheck} label="Review approvals" onClick={() => onNavigate("approvals")} />
          <QuickAction icon={Flag} label="Flag something" onClick={onFlag} />
        </div>
      </div>
    </div>
  );
}

function StatCard({
  icon: Icon,
  label,
  value,
  hint,
  action,
}: {
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  value: string;
  hint: string;
  action?: { label: string; onClick: () => void };
}) {
  return (
    <Card>
      <CardContent className="space-y-3 py-5">
        <div className="flex items-center justify-between">
          <span className="text-sm font-medium text-muted-foreground">{label}</span>
          <Icon className="size-4 text-muted-foreground" />
        </div>
        <div className="text-2xl font-semibold tracking-tight">{value}</div>
        <div className="flex items-center justify-between gap-2">
          <p className="text-xs text-muted-foreground">{hint}</p>
          {action && (
            <Button variant="ghost" size="sm" className="-mr-2 h-7 shrink-0 px-2" onClick={action.onClick}>
              {action.label} <ArrowRight className="size-3.5" />
            </Button>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

function QuickAction({
  icon: Icon,
  label,
  onClick,
}: {
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      className="flex items-center gap-3 rounded-xl border bg-card p-4 text-left text-sm font-medium shadow-sm transition-colors hover:bg-accent hover:text-accent-foreground"
    >
      <span className="flex size-9 items-center justify-center rounded-lg bg-muted">
        <Icon className="size-4" />
      </span>
      {label}
    </button>
  );
}
