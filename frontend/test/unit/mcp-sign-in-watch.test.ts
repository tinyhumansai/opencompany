import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { McpHealth } from "@/api/types";
import { watchSignIn } from "@/lib/mcp-sign-in-watch";

const PENDING: McpHealth = {
  status: "needs_config",
  authHint: "oauth_required",
  message: "needs a browser sign-in",
  toolCount: 0,
  checkedAtMillis: 1,
};
const OK: McpHealth = { ...PENDING, status: "ok", message: "", toolCount: 3 };

function handlers() {
  return {
    onProbe: vi.fn(),
    onConnected: vi.fn(),
    onTimeout: vi.fn(),
  };
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(0);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("watchSignIn", () => {
  it("probes on an interval until the server reports ok", async () => {
    const probe = vi
      .fn<() => Promise<McpHealth>>()
      .mockResolvedValueOnce(PENDING)
      .mockResolvedValueOnce(OK);
    const h = handlers();
    const watch = watchSignIn({ probe, ...h, intervalMs: 1_000 });

    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.onProbe).toHaveBeenCalledWith(PENDING);
    expect(h.onConnected).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.onConnected).toHaveBeenCalledWith(OK);
    expect(watch.stopped).toBe(true);

    await vi.advanceTimersByTimeAsync(10_000);
    expect(probe).toHaveBeenCalledTimes(2);
  });

  it("keeps probing through a failed probe", async () => {
    const probe = vi
      .fn<() => Promise<McpHealth>>()
      .mockRejectedValueOnce(new Error("offline"))
      .mockResolvedValueOnce(OK);
    const h = handlers();
    watchSignIn({ probe, ...h, intervalMs: 1_000 });

    await vi.advanceTimersByTimeAsync(2_000);
    expect(h.onConnected).toHaveBeenCalledWith(OK);
  });

  it("times out at the deadline and stops probing on its own", async () => {
    const probe = vi.fn<() => Promise<McpHealth>>().mockResolvedValue(PENDING);
    const h = handlers();
    const watch = watchSignIn({ probe, ...h, intervalMs: 1_000, deadlineMs: 3_000 });

    await vi.advanceTimersByTimeAsync(4_000);
    expect(h.onTimeout).toHaveBeenCalledTimes(1);
    expect(watch.timedOut).toBe(true);
    const calls = probe.mock.calls.length;

    await vi.advanceTimersByTimeAsync(10_000);
    expect(probe).toHaveBeenCalledTimes(calls);
  });

  it("checks once on demand after the deadline and reports a late success", async () => {
    const probe = vi.fn<() => Promise<McpHealth>>().mockResolvedValue(PENDING);
    const h = handlers();
    const watch = watchSignIn({ probe, ...h, intervalMs: 1_000, deadlineMs: 1_500 });
    await vi.advanceTimersByTimeAsync(3_000);
    expect(watch.timedOut).toBe(true);

    probe.mockResolvedValueOnce(OK);
    watch.checkNow();
    await vi.advanceTimersByTimeAsync(0);
    expect(h.onConnected).toHaveBeenCalledWith(OK);
  });

  it("probes immediately on checkNow instead of waiting for the interval", async () => {
    const probe = vi.fn<() => Promise<McpHealth>>().mockResolvedValue(OK);
    const h = handlers();
    const watch = watchSignIn({ probe, ...h, intervalMs: 60_000 });

    watch.checkNow();
    await vi.advanceTimersByTimeAsync(0);
    expect(h.onConnected).toHaveBeenCalledTimes(1);
  });

  it("ignores an answer that arrives after stop", async () => {
    let resolve!: (h: McpHealth) => void;
    const probe = vi.fn<() => Promise<McpHealth>>(
      () => new Promise((r) => (resolve = r)),
    );
    const h = handlers();
    const watch = watchSignIn({ probe, ...h, intervalMs: 1_000 });
    await vi.advanceTimersByTimeAsync(1_000);
    watch.stop();
    resolve(OK);
    await vi.advanceTimersByTimeAsync(5_000);
    expect(h.onProbe).not.toHaveBeenCalled();
    expect(h.onConnected).not.toHaveBeenCalled();
    expect(probe).toHaveBeenCalledTimes(1);
  });

  it("does not stack a second probe while one is in flight", async () => {
    const probe = vi.fn<() => Promise<McpHealth>>(() => new Promise(() => {}));
    const h = handlers();
    const watch = watchSignIn({ probe, ...h, intervalMs: 1_000 });
    await vi.advanceTimersByTimeAsync(1_000);
    watch.checkNow();
    watch.checkNow();
    expect(probe).toHaveBeenCalledTimes(1);
  });
});
