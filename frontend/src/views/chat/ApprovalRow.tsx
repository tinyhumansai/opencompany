// A parked approval, raised inside the conversation that produced it (#379).
//
// The same content the Approvals page shows — headline, payload, asker, waiting
// time, all from `@/components/approval-card` — laid out as a channel row so it
// reads as part of the thread rather than as a panel bolted beside it.
//
// The one thing it does differently is how it resolves: **detached** (#391).
// The default resolve answers with the follow-up turn's replies, and rendering
// those here would put the continuation into the channel once from the POST
// body and again from its SSE echo. Detach has exactly one delivery path, so
// the duplicate-bubble race #391 deliberately left open outside chat POSTs
// cannot exist here.

import { Check, Loader2, X } from "lucide-react";

import type { ApprovalSummary, Verdict } from "@/api/types";
import {
  ApprovalHeadline,
  ApprovalMeta,
  ApprovalPayload,
} from "@/components/approval-card";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/** What the card says once a verdict has been witnessed. */
function settledLabel(verdict: Verdict): string {
  // Mirrors the wording the Approvals page files into the transcript, and for
  // the same reason: approving is not "done", it hands the agent a single-use
  // grant and re-dispatches it. A decline IS terminal.
  return verdict === "approve"
    ? "Approved — the agent is completing the action"
    : "Declined — recorded, and nothing will run";
}

export function ApprovalRow({
  approval,
  now,
  askerNames,
  deciding,
  decided,
  onDecide,
}: {
  approval: ApprovalSummary;
  now: number;
  askerNames: Map<string, string>;
  /** The verdict this card is waiting on, or `null` when idle. */
  deciding: Verdict | null;
  /** A verdict already witnessed — from this console or from the page. */
  decided: Verdict | null;
  onDecide: (verdict: Verdict) => void;
}) {
  return (
    <div className="px-4 py-2">
      <div
        role="group"
        aria-label="Approval request"
        data-approval-id={approval.id}
        className={cn(
          "rounded-xl border bg-card px-4 py-3 shadow-sm",
          // A settled card steps back rather than disappearing: the operator
          // has to be able to see their own decision land.
          decided && "opacity-70",
        )}
      >
        <div className="flex flex-col gap-3">
          <ApprovalHeadline
            approval={approval}
            actions={
              decided ? undefined : (
                <>
                  <Button
                    variant="outline"
                    size="sm"
                    disabled={deciding !== null}
                    onClick={() => onDecide("deny")}
                  >
                    {deciding === "deny" ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <X className="size-4" />
                    )}{" "}
                    Decline
                  </Button>
                  <Button
                    size="sm"
                    disabled={deciding !== null}
                    onClick={() => onDecide("approve")}
                  >
                    {deciding === "approve" ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <Check className="size-4" />
                    )}{" "}
                    Approve
                  </Button>
                </>
              )
            }
          />

          <ApprovalPayload approval={approval} />

          <ApprovalMeta
            approval={approval}
            now={now}
            askerNames={askerNames}
            status={
              decided
                ? settledLabel(decided)
                : deciding
                  ? deciding === "approve"
                    ? "Waiting for the agent…"
                    : "Recording…"
                  : undefined
            }
          />
        </div>
      </div>
    </div>
  );
}
