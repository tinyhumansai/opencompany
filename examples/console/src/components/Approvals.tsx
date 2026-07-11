import { useState } from "react";

import type { OpenCompanyClient } from "../api/client";
import type { ApprovalSummary } from "../api/types";
import { ApiError } from "../api/types";
import { approvalSummary, timeAgo } from "../lib/language";

interface Props {
  client: OpenCompanyClient;
  company: string;
  approvals: ApprovalSummary[];
  now: number;
  /** Called after a decision so the parent can refresh the queue + chat. */
  onResolved: (systemLine: string) => void;
}

/** The approvals inbox: the few things the company parked for the operator. */
export function Approvals({ client, company, approvals, now, onResolved }: Props) {
  const [busy, setBusy] = useState<string | null>(null);

  if (approvals.length === 0) return null;

  async function decide(a: ApprovalSummary, verdict: "approve" | "deny") {
    if (busy) return;
    setBusy(a.id);
    try {
      await client.resolveApproval(a.id, verdict, undefined, company);
      const verb = verdict === "approve" ? "Approved" : "Declined";
      onResolved(`${verb}: ${approvalSummary(a)}`);
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      onResolved(`Couldn't record your decision — ${msg}`);
    } finally {
      setBusy(null);
    }
  }

  return (
    <section className="approvals">
      <h2>Needs your approval</h2>
      {approvals.map((a) => (
        <div className="approval" key={a.id}>
          <div className="what">
            <div>{approvalSummary(a)}</div>
            <div className="when">{timeAgo(a.at_millis, now)}</div>
          </div>
          <div className="actions">
            <button
              className="btn small danger"
              disabled={busy !== null}
              onClick={() => void decide(a, "deny")}
            >
              Decline
            </button>
            <button
              className="btn small primary"
              disabled={busy !== null}
              onClick={() => void decide(a, "approve")}
            >
              Approve
            </button>
          </div>
        </div>
      ))}
    </section>
  );
}
