import { useCallback, useEffect, useRef, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import type { ApprovalSummary, CompanyStatus } from "@/api/types";

const POLL_MS = 5000;

export interface CompanyFeed {
  status: CompanyStatus;
  approvals: ApprovalSummary[];
  /** Wall-clock at the last successful refresh, for relative timestamps. */
  now: number;
  refresh: () => Promise<void>;
}

/**
 * Polls a single company's status and approvals on an interval, keeping the
 * last good view on transient errors. Re-subscribes when the company changes.
 */
export function useCompany(
  client: OpenCompanyClient,
  company: string | null,
  initialStatus: CompanyStatus,
): CompanyFeed {
  const [status, setStatus] = useState<CompanyStatus>(initialStatus);
  const [approvals, setApprovals] = useState<ApprovalSummary[]>([]);
  const [now, setNow] = useState(() => Date.now());
  const mounted = useRef(true);

  // Reset to the freshly-picked company's status when switching.
  useEffect(() => {
    setStatus(initialStatus);
    setApprovals([]);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [company]);

  const refresh = useCallback(async () => {
    try {
      const [s, a] = await Promise.all([client.status(company), client.approvals(company)]);
      if (!mounted.current) return;
      setStatus(s);
      setApprovals(a);
      setNow(Date.now());
    } catch {
      /* transient; keep the last good view */
    }
  }, [client, company]);

  useEffect(() => {
    mounted.current = true;
    void refresh();
    const timer = setInterval(() => void refresh(), POLL_MS);
    return () => {
      mounted.current = false;
      clearInterval(timer);
    };
  }, [refresh]);

  return { status, approvals, now, refresh };
}
