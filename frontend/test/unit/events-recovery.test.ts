// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useEvents, type CompanyStreamEvent } from "@/hooks/use-events";
import type { OpenCompanyClient } from "@/api/client";

(
  globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }
).IS_REACT_ACT_ENVIRONMENT = true;

type StreamHandlers = Parameters<OpenCompanyClient["subscribeToEvents"]>[1];

function mountEvents(
  client: OpenCompanyClient,
  onResync: () => Promise<void> | void,
  onRecoveryError = vi.fn(),
) {
  const host = document.createElement("div");
  const root = createRoot(host);
  function Test() {
    useEvents(client, "acme", {
      pendingApprovals: 0,
      onResync,
      onRecoveryError,
    });
    return null;
  }
  act(() => root.render(createElement(Test)));
  return { root, onRecoveryError };
}

afterEach(() => {
  vi.useRealTimers();
  document.body.replaceChildren();
});

describe("event-stream recovery", () => {
  function clientWithStream() {
    let handlers: StreamHandlers | undefined;
    const client = {
      baseUrl: "",
      scopeFor: () => "/api/companies/acme",
      subscribeToEvents(
        _company: string | null | undefined,
        next: StreamHandlers,
      ) {
        handlers = next;
        return vi.fn();
      },
    } as unknown as OpenCompanyClient;
    return { client, handlers: () => handlers! };
  }

  it("recovers silently when the server reports a structural gap", async () => {
    const { client, handlers } = clientWithStream();
    const onResync = vi.fn();
    const mounted = mountEvents(client, onResync);

    await act(async () =>
      handlers().onMessage(
        JSON.stringify({
          type: "stream_gap",
          missed: 44,
        } satisfies CompanyStreamEvent),
      ),
    );

    expect(onResync).toHaveBeenCalledTimes(1);
    act(() => mounted.root.unmount());
  });

  it("reconciles if a stream never reaches OPEN, then stops on a connection", async () => {
    vi.useFakeTimers();
    const { client, handlers } = clientWithStream();
    const onResync = vi.fn();
    const mounted = mountEvents(client, onResync);

    await act(async () => vi.advanceTimersByTimeAsync(10_000));
    expect(onResync).toHaveBeenCalledTimes(1);
    await act(async () => vi.advanceTimersByTimeAsync(30_000));
    expect(onResync).toHaveBeenCalledTimes(2);

    await act(async () => handlers().onOpen?.());
    expect(onResync).toHaveBeenCalledTimes(3);
    await act(async () => vi.advanceTimersByTimeAsync(30_000));
    expect(onResync).toHaveBeenCalledTimes(3);
    act(() => mounted.root.unmount());
  });

  it("reports a failed canonical recovery once while the stream is down", async () => {
    vi.useFakeTimers();
    const { client } = clientWithStream();
    const onResync = vi.fn().mockRejectedValue(new Error("offline"));
    const mounted = mountEvents(client, onResync);

    await act(async () => vi.advanceTimersByTimeAsync(10_000));
    await act(async () => vi.advanceTimersByTimeAsync(30_000));

    expect(mounted.onRecoveryError).toHaveBeenCalledTimes(1);
    act(() => mounted.root.unmount());
  });
});
