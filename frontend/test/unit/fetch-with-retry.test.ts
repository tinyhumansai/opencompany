import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { fetchWithOneRetry } from "@/lib/fetch-with-retry";

describe("fetchWithOneRetry (issue #1781 review, Codex P2)", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("resolves on the first attempt without waiting or retrying", async () => {
    const fetch = vi.fn().mockResolvedValue("ok");
    const result = await fetchWithOneRetry(fetch);
    expect(result).toBe("ok");
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("retries exactly once, after the delay, when the first attempt fails", async () => {
    const fetch = vi.fn().mockRejectedValueOnce(new Error("blip")).mockResolvedValueOnce("ok");
    const promise = fetchWithOneRetry(fetch, 300);

    // Not yet retried — still waiting out the delay.
    await Promise.resolve();
    expect(fetch).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(300);
    await expect(promise).resolves.toBe("ok");
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it("gives up to null after the retry also fails, rather than looping", async () => {
    const fetch = vi.fn().mockRejectedValue(new Error("gone"));
    const promise = fetchWithOneRetry(fetch, 300);

    await vi.advanceTimersByTimeAsync(300);
    await expect(promise).resolves.toBeNull();
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it("defaults the delay to 300ms", async () => {
    const fetch = vi.fn().mockRejectedValueOnce(new Error("blip")).mockResolvedValueOnce("ok");
    const promise = fetchWithOneRetry(fetch);

    await vi.advanceTimersByTimeAsync(299);
    expect(fetch).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(1);
    await expect(promise).resolves.toBe("ok");
    expect(fetch).toHaveBeenCalledTimes(2);
  });
});
