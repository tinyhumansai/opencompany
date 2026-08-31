import type { ApprovalSummary } from "@/api/types";

/**
 * Orders an approvals queue by the deadline the host actually enforces.
 *
 * An old host may omit its deadline. Those rows follow every dated row because
 * their urgency is unknown, while returning `0` for equal keys keeps the host's
 * relative order intact for an operator who is comparing otherwise equal calls.
 */
export function approvalsByDeadline(approvals: ApprovalSummary[]): ApprovalSummary[] {
  return [...approvals].sort((left, right) => {
    const leftDeadline = left.expires_at_millis;
    const rightDeadline = right.expires_at_millis;
    const leftHasDeadline = typeof leftDeadline === "number";
    const rightHasDeadline = typeof rightDeadline === "number";

    if (leftHasDeadline && rightHasDeadline) return leftDeadline - rightDeadline;
    if (leftHasDeadline) return -1;
    if (rightHasDeadline) return 1;
    return 0;
  });
}
