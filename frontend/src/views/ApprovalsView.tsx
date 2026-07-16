import { useState } from "react";
import {
  AtSign,
  Check,
  CreditCard,
  FileSignature,
  FileText,
  Globe,
  KeyRound,
  Mail,
  MessageSquare,
  Repeat,
  RefreshCw,
  Rocket,
  ShieldCheck,
  type LucideIcon,
  X,
} from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type ApprovalSummary } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import type { CompanyFeed } from "@/hooks/use-company";
import { approvalSummary, money, timeAgo } from "@/lib/language";
import { cn } from "@/lib/utils";

const KIND_ICONS: Record<string, LucideIcon> = {
  "payment.send": CreditCard,
  "subscription.start": Repeat,
  "email.send": Mail,
  "dm.external": MessageSquare,
  "filing.submit": FileText,
  "contract.accept": FileSignature,
  "external.publish": Globe,
  "website.deploy": Rocket,
  "handle.register": AtSign,
  "handle.renew": RefreshCw,
  "key.rotate": KeyRound,
};

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  feed: CompanyFeed;
  onResolved: (systemLine: string) => void;
  onGoToConversation: () => void;
}

/** The approvals inbox: the few things the company parked for the operator. */
export function ApprovalsView({ client, company, feed, onResolved, onGoToConversation }: Props) {
  const [busy, setBusy] = useState<string | null>(null);
  const { approvals, now } = feed;

  async function decide(a: ApprovalSummary, verdict: "approve" | "deny") {
    if (busy) return;
    setBusy(a.id);
    try {
      await client.resolveApproval(a.id, verdict, undefined, company);
      const verb = verdict === "approve" ? "Approved" : "Declined";
      const line = `${verb}: ${approvalSummary(a)}`;
      onResolved(line);
      toast.success(line);
      void feed.refresh();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      onResolved(`Couldn't record your decision — ${msg}`);
      toast.error(`Couldn't record your decision — ${msg}`);
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl px-4 py-6">
        {approvals.length === 0 ? (
          <EmptyApprovals onGoToConversation={onGoToConversation} />
        ) : (
          <>
            <div className="mb-4 flex items-baseline justify-between">
              <h2 className="text-sm font-medium text-muted-foreground">
                {approvals.length === 1 ? "1 thing needs your approval" : `${approvals.length} things need your approval`}
              </h2>
            </div>
            <div className="flex flex-col gap-3">
              {approvals.map((a) => {
                const Icon = KIND_ICONS[a.kind] ?? ShieldCheck;
                const isBusy = busy === a.id;
                return (
                  <Card key={a.id} className={cn("transition-opacity", busy && !isBusy && "opacity-60")}>
                    <CardContent className="flex items-center gap-4 py-4">
                      <div className="flex size-10 shrink-0 items-center justify-center rounded-lg bg-muted text-foreground">
                        <Icon className="size-5" />
                      </div>
                      <div className="min-w-0 flex-1">
                        <p className="truncate font-medium">{approvalSummary(a)}</p>
                        <p className="text-xs text-muted-foreground">
                          {a.amount_usd != null && <span className="font-medium">{money(a.amount_usd)} · </span>}
                          {timeAgo(a.at_millis, now)}
                        </p>
                      </div>
                      <div className="flex shrink-0 gap-2">
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={busy !== null}
                          onClick={() => void decide(a, "deny")}
                        >
                          <X className="size-4" /> Decline
                        </Button>
                        <Button size="sm" disabled={busy !== null} onClick={() => void decide(a, "approve")}>
                          <Check className="size-4" /> Approve
                        </Button>
                      </div>
                    </CardContent>
                  </Card>
                );
              })}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function EmptyApprovals({ onGoToConversation }: { onGoToConversation: () => void }) {
  return (
    <div className="mt-16 flex flex-col items-center gap-3 text-center">
      <div className="flex size-12 items-center justify-center rounded-2xl bg-emerald-500/10 text-emerald-600 dark:text-emerald-400">
        <ShieldCheck className="size-6" />
      </div>
      <div className="space-y-1">
        <p className="font-medium">All clear</p>
        <p className="max-w-sm text-sm text-muted-foreground">
          Nothing is waiting on you. Your company will park anything that needs a sign-off here.
        </p>
      </div>
      <Button variant="outline" size="sm" onClick={onGoToConversation}>
        Back to the conversation
      </Button>
    </div>
  );
}
