import { useEffect, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import { getPolicy } from "@/api/policy";
import { startVisiblePolling } from "@/lib/visible-poll";

/** The historical fallback when a host's policy omits the deadline. */
const DEFAULT_TTL_HOURS = 24;

/**
 * How often the sentence refreshes while the approvals view is mounted.
 *
 * A policy PUT from another operator or tab applies on the host at once, and
 * the approvals feed — which paints each card's own deadline — polls on this
 * cadence, so the header refreshes beside it. A sentence that only refetched
 * on mount would keep the value it loaded with until the view was remounted.
 */
const POLL_MS = 5000;

/**
 * The queue's "Each one has a deadline" sentence, read from the company policy.
 *
 * An older host's `/policy` still returns 200 but omits `approvalTtlHours`, so
 * the value is normalized to the historical 24-hour default instead of leaking
 * `undefined` into the rendered sentence. A new scoped read restarts from that
 * default too: when the operator switches company and the next read fails, the
 * previous company's deadline must not carry into the new one's queue.
 *
 * Refreshed on the same visibility-gated cadence as the approvals feed (issue
 * #581), so a live policy change reaches the sentence without a remount, and a
 * backgrounded tab costs the host nothing.
 */
export function useApprovalDeadline(
  client: OpenCompanyClient,
  company: string | null,
): number {
  const [hours, setHours] = useState(DEFAULT_TTL_HOURS);
  useEffect(() => {
    let live = true;
    // Serialize refreshes: a slow request must not be invalidated by the next
    // polling tick, otherwise a consistently slow host would never update the
    // header. The next tick runs immediately after the current request settles.
    let refreshing = false;
    let queued = false;
    setHours(DEFAULT_TTL_HOURS);
    const refresh = () => {
      if (refreshing) {
        queued = true;
        return;
      }
      refreshing = true;
      void getPolicy(client, company)
        .then((policy) => {
          if (live) setHours(policy.approvalTtlHours ?? DEFAULT_TTL_HOURS);
        })
        .catch(() => {
          // A policy read is explanatory here. Keep the historical default if an
          // older or temporarily unreachable host cannot serve it.
        })
        .finally(() => {
          refreshing = false;
          if (queued && live) {
            queued = false;
            refresh();
          }
        });
    };
    refresh();
    const dispose = startVisiblePolling(refresh, POLL_MS);
    return () => {
      live = false;
      dispose();
    };
  }, [client, company]);
  return hours;
}
