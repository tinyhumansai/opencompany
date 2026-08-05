import { useState } from "react";
import { Check, Loader2, ShieldCheck, X } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type ApprovalSummary, type Verdict } from "@/api/types";
import {
  ApprovalHeadline,
  ApprovalMeta,
  ApprovalPayload,
  useAskerNames,
} from "@/components/approval-card";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import type { CompanyFeed } from "@/hooks/use-company";
import { approvalSummary } from "@/lib/language";

/**
 * Below this, a lost connection cannot have outlived the fast part of a resolve.
 *
 * Generous on purpose: the fast part is three local writes, so anything past a
 * second or two of *waiting* was waiting on the agent turn. The number only has
 * to separate "failed before it started" from "failed after it was underway".
 */
const RESOLVE_SLOW_ENOUGH_MS = 2_000;

/**
 * Whether the host may have carried out this request despite the error (#380).
 *
 * The console genuinely cannot tell "the verdict never landed" from "the
 * verdict landed and the continuation timed out" — but it can tell who
 * answered, and roughly how long it waited, and that is enough to stop lying in
 * the cases that actually occur.
 *
 * **The host answered in its own envelope** (`fromHost`): it considered the
 * request and refused, so nothing landed, whatever the status was. This is the
 * arm that matters most for correctness — the host returns 503 while quiescing
 * and 502 on an upstream transport failure, so a status-only check would read
 * those clean refusals as timeouts and tell the operator a decision was
 * recorded when it provably was not.
 *
 * **A proxy answered** (502/503/504 with no envelope): on a hosted tenant that
 * is a reverse proxy that could not get a reply out of an upstream it had
 * already handed the request to. The verdict very probably landed.
 *
 * **Nothing answered**: only evidence of a slow turn if it was actually slow.
 * An instant rejection is an offline browser or a refused connection, where the
 * request never reached the host — and saying "your decision was recorded"
 * there would be a fresh lie in place of the one being fixed. Hence the
 * elapsed-time floor: the inference is "recorded *before* the delay", so it
 * needs a delay to stand on.
 *
 * Either way `feed.refresh()` at the call site settles it, since the host drops
 * the approval from the queue before anything slow happens.
 */
function mayHaveLanded(err: unknown, elapsedMs: number): boolean {
  if (!(err instanceof ApiError)) return false;
  if (err.fromHost) return false;
  if (err.status >= 502) return true;
  return err.code === "network_error" && elapsedMs >= RESOLVE_SLOW_ENOUGH_MS;
}

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  feed: CompanyFeed;
  onResolved: (systemLine: string) => void;
  onGoToConversation: () => void;
}

/** The approvals inbox: the few things the company parked for the operator. */
export function ApprovalsView({ client, company, feed, onResolved, onGoToConversation }: Props) {
  // Issue #373: in-flight state is per approval, not a single module-wide slot.
  //
  // Approving is not a quick write — the host mints a grant and re-dispatches
  // the agent, holding the POST open for a whole turn (#243) — so two decisions
  // being in flight at once is a legitimate state the operator can reach, and
  // one the host already handles by serialising them behind its per-company
  // lock. The old `string | null` could not represent it, which is why deciding
  // one card greyed out every other card on the screen until a hard reload.
  //
  // A map rather than a set of ids because the verdict has to survive the wait:
  // an approve and a decline are different promises to the operator ("the agent
  // is doing it" vs "recorded"), and the card says which one it is waiting on.
  const [inFlight, setInFlight] = useState<ReadonlyMap<string, Verdict>>(() => new Map());
  const { approvals, now } = feed;
  const askerNames = useAskerNames(client, company, approvals);

  const markInFlight = (id: string, verdict: Verdict | null) =>
    setInFlight((prev) => {
      const next = new Map(prev);
      if (verdict) next.set(id, verdict);
      else next.delete(id);
      return next;
    });

  async function decide(a: ApprovalSummary, verdict: Verdict) {
    // Per-row guard: only a double-press on THIS card is ignored. The global
    // early return that used to live here made every other card inert too.
    if (inFlight.has(a.id)) return;
    markInFlight(a.id, verdict);
    const startedAt = Date.now();
    try {
      await client.resolveApproval(a.id, verdict, undefined, company);
      // Issue #243: approving no longer just records a verdict — it hands the
      // agent a single-use grant and re-dispatches it to make the call. The old
      // "Approved: …" read as "done", which was the exact lie that made the
      // missing re-dispatch invisible: the operator saw a success toast for work
      // that had silently dead-ended. Say what is actually happening instead.
      // Declining IS terminal, so its wording is unchanged.
      const line =
        verdict === "approve"
          ? `Approved — the agent is completing the action: ${approvalSummary(a)}`
          : `Declined: ${approvalSummary(a)}`;
      onResolved(line);
      toast.success(line);
      // The agent's reply arrives as a journaled `AgentReply` on its own thread,
      // so no extra plumbing is needed here — the existing feed refresh plus the
      // per-agent DM thread (#151) surface it.
      void feed.refresh();
    } catch (err) {
      // Issue #380: a failed request is not the same as a failed decision.
      //
      // The host resolves in four steps — drop the approval from the parked
      // queue, journal the verdict durably, mint the grant, then run a whole
      // follow-up agent turn — and only the last one is slow. So when the
      // answer is lost in transit it is lost *after* the verdict is durable,
      // and "Couldn't record your decision" is a lie that invites the one
      // response that cannot help: approving again.
      if (mayHaveLanded(err, Date.now() - startedAt)) {
        const line =
          verdict === "approve"
            ? "Approved — the host didn't answer in time, but your decision was recorded. The agent may still be working; no need to approve again."
            : "Declined — the host didn't answer in time, but your decision was recorded. No need to decline again.";
        onResolved(line);
        // Neither a success nor an error: the verdict is durable, the
        // continuation is unknown. A green tick would overclaim and a red
        // cross is the bug being fixed.
        toast.info(line);
        // The reconciliation, and the reason the copy above can be this
        // confident: the host removes the approval from the queue in step one,
        // so a refresh either drops this card — proving the verdict landed —
        // or leaves it, showing the operator a decision that still needs
        // making. The queue is the answer the response body never delivered.
        void feed.refresh();
      } else {
        const msg = err instanceof ApiError ? err.message : "something went wrong";
        onResolved(`Couldn't record your decision — ${msg}`);
        toast.error(`Couldn't record your decision — ${msg}`);
      }
    } finally {
      // Unconditional, and keyed on the id rather than clearing a single slot:
      // the feed refreshes on its own schedule and routinely drops the decided
      // row while its request is still open, so the flag has to be removable
      // whether or not the row it belongs to still exists. Deleting a key that
      // is already gone is a no-op, which is the point.
      markInFlight(a.id, null);
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
                {approvals.length === 1
                  ? "1 thing needs your approval"
                  : `${approvals.length} things need your approval`}
              </h2>
            </div>
            <div className="flex flex-col gap-3">
              {approvals.map((a) => (
                <ApprovalCard
                  key={a.id}
                  approval={a}
                  now={now}
                  askerNames={askerNames}
                  deciding={inFlight.get(a.id) ?? null}
                  onDecide={(verdict) => void decide(a, verdict)}
                />
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/**
 * One parked approval, told in full (#372).
 *
 * The card answers the four questions an operator needs before they can decide
 * — what will happen, who is asking, what it is for, and how long it has waited
 * — and it answers the first one *concretely*, with the tool call's own
 * arguments, because that is the thing being consented to. Before this it said
 * "Shell", which asks someone to authorise an action they cannot see.
 *
 * Laid out vertically rather than as one row: the payload block needs the full
 * width, and the stacked form leaves the slot #374's per-approval scope control
 * will occupy.
 *
 * **An old host degrades to the pre-#372 card by construction.** It omits
 * `payload` and `agent` from the wire, so the payload block and the "Asked by"
 * line simply do not render and what is left is the headline, the amount and
 * the relative time — exactly what shipped before.
 */
function ApprovalCard({
  approval: a,
  now,
  askerNames,
  deciding,
  onDecide,
}: {
  approval: ApprovalSummary;
  now: number;
  askerNames: Map<string, string>;
  /** The verdict this card is waiting on, or `null` when it is idle (#373). */
  deciding: Verdict | null;
  onDecide: (verdict: Verdict) => void;
}) {
  // No cross-card dimming: another card being decided is not this card's
  // business, and treating it as such is the visual half of the #373 bug.
  return (
    <Card>
      <CardContent className="flex flex-col gap-3 py-4">
        <ApprovalHeadline
          approval={a}
          /* Disabled on THIS card's own state only — a decision in flight on
             another card leaves these live. That is the whole of #373's
             first cause. */
          actions={
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
              <Button size="sm" disabled={deciding !== null} onClick={() => onDecide("approve")}>
                {deciding === "approve" ? (
                  <Loader2 className="size-4 animate-spin" />
                ) : (
                  <Check className="size-4" />
                )}{" "}
                Approve
              </Button>
            </>
          }
        />

        <ApprovalPayload approval={a} />

        <ApprovalMeta
          approval={a}
          now={now}
          askerNames={askerNames}
          /* Honest copy for a request that spans an agent turn (#373): an
             approve is not done when the button stops spinning, it is handed
             to the agent. A decline IS terminal, so it only has to record. */
          status={
            deciding ? (deciding === "approve" ? "Waiting for the agent…" : "Recording…") : undefined
          }
        />
      </CardContent>
    </Card>
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
