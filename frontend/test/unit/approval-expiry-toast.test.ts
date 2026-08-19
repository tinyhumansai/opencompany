import { beforeEach, describe, expect, it, vi } from "vitest";

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));

vi.mock("sonner", () => {
  const toast = Object.assign(toasts.base, {
    success: toasts.success,
    error: toasts.error,
    warning: toasts.warning,
    info: toasts.info,
  });
  return { toast };
});

const { handleEvent } = await import("@/hooks/use-events");
type Ev = import("@/hooks/use-events").CompanyStreamEvent;

/**
 * Issue #971: an expiry must not be announced as a decision somebody made.
 *
 * The host resolves an overdue approval as `Deny` by the system — a
 * default-deny on silence, which is a real resolution and has to be one (#305,
 * #469). The stream frame carried only the verdict, so the console toasted
 * "Approval denied" and told an operator they had declined a request they never
 * saw. That was rare while the deadline was a week; at 24 hours it is routine,
 * which is what makes it a defect rather than a wording nit.
 *
 * The host now sets `automatic` on that arm and only that arm, derived from the
 * event's actor without carrying the actor — the attention feed stays
 * deny-by-default. These pin both directions: the new words for an expiry, and
 * the unchanged words for a person's own decision.
 */

function resolved(overrides: Partial<Extract<Ev, { type: "approval_resolved" }>> = {}): Ev {
  return {
    type: "approval_resolved",
    seq: 1,
    atMillis: 1_700_000_000_000,
    approvalId: "a1",
    verdict: "deny",
    ...overrides,
  } as Ev;
}

beforeEach(() => {
  toasts.base.mockClear();
  toasts.success.mockClear();
  toasts.error.mockClear();
});

describe("what the console says when an approval resolves", () => {
  it("names the deadline, not the operator, when the host expired it", () => {
    handleEvent(resolved({ automatic: true }), {});
    expect(toasts.base).toHaveBeenCalledTimes(1);
    const [title, opts] = toasts.base.mock.calls[0] as [string, { description: string }];
    expect(title).toBe("Approval expired");
    expect(opts.description).toContain("deadline");
    // The exact failure: never tell someone they declined something.
    expect(title).not.toContain("denied");
    expect(opts.description).not.toContain("denied");
  });

  it("still says 'Approval denied' when a person actually declined it", () => {
    handleEvent(resolved(), {});
    const [title] = toasts.base.mock.calls[0] as [string];
    expect(title).toBe("Approval denied");
  });

  it("still says 'Approval granted' when a person approved it", () => {
    handleEvent(resolved({ verdict: "approve" }), {});
    const [title] = toasts.base.mock.calls[0] as [string];
    expect(title).toBe("Approval granted");
  });

  it("reads an absent flag as a decision somebody made, so an old host is unchanged", () => {
    // `automatic` is skipped when false, so absence is the common case and the
    // pre-#971 host's case alike. Both mean "treat this as a decision".
    const event = resolved();
    expect("automatic" in (event as Record<string, unknown>)).toBe(false);
    handleEvent(event, {});
    const [title] = toasts.base.mock.calls[0] as [string];
    expect(title).toBe("Approval denied");
  });

  it("refreshes the queue either way, so a card settles whoever resolved it", () => {
    const onApprovalEvent = vi.fn();
    handleEvent(resolved({ automatic: true }), { onApprovalEvent });
    handleEvent(resolved(), { onApprovalEvent });
    expect(onApprovalEvent).toHaveBeenCalledTimes(2);
  });
});
