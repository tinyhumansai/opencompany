// @vitest-environment jsdom

import { act, createElement, useState, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { ChatMessage } from "@/lib/chat";
import type { TaskStatus } from "@/api/tasks";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import type { Transcripts } from "@/views/room/model";
import { RoomView } from "@/views/RoomView";

/**
 * `RoomView.reviewCard` — the settle pill's Approve, `POST {scope}/chat/review`
 * (`review_card`, `operator.rs`) — already has a `catch`; nothing pinned it.
 * A refused verdict must not remove the card or claim it settled: the row
 * stays exactly as it was, Approve returns to `disabled: false`, and the
 * failure is said on screen rather than only in the console the operator
 * cannot see. Same route the ThreadPanel inline Approve calls
 * (`cov-chat-thread-panel-review-auth.test.ts`) — its own failure toast is
 * the same `catch`, exercised here at the channel-pill surface.
 */

const toasts = vi.hoisted(() => ({ base: vi.fn(), success: vi.fn(), error: vi.fn() }));
vi.mock("sonner", () => ({
  toast: Object.assign(toasts.base, { success: toasts.success, error: toasts.error, warning: vi.fn(), info: vi.fn() }),
}));

const DESK_DTO = { id: "main", name: "main", description: "The main channel", members: [] as string[] };
const CARD: ChatMessage = {
  id: "h1",
  from: "system",
  text: "Task done — Ship the thing",
  at: Date.UTC(2026, 8, 1, 9, 0, 0),
  taskId: "t1",
};

function clientAs(reviewCard: () => Promise<never>): OpenCompanyClient {
  const named: Record<string, unknown> = {
    scopeFor: () => "/api/v1/companies/acme",
    listDesks: () => Promise.resolve([DESK_DTO]),
    reviewCard: vi.fn(reviewCard),
  };
  return new Proxy(named, {
    get: (target, prop: string) => target[prop] ?? (() => Promise.resolve([])),
  }) as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  window.matchMedia = ((query: string) => ({
    matches: query.includes("min-width"),
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
  Object.defineProperty(window, "innerWidth", { value: 1440, writable: true });
  Element.prototype.scrollTo = vi.fn();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  toasts.base.mockClear();
  toasts.success.mockClear();
  toasts.error.mockClear();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

/** Owns `transcripts` state itself, the way `AppShell` does for the real view. */
function Harness({ client }: { client: OpenCompanyClient }) {
  const [transcripts, setTranscripts] = useState<Transcripts>({ main: [CARD] });
  return createElement(RoomView, {
    client,
    company: "acme",
    sub: "main",
    onNavigate: vi.fn(),
    transcripts,
    setTranscripts,
    resolveTypingNames: () => [],
    scopeRef: { current: { connection: "local", company: "acme", client } },
    taskStatusByTaskId: { t1: { column: "in_review" } as TaskStatus },
  });
}

function tree(client: OpenCompanyClient): ReactNode {
  return createElement(ConnectionScopeProvider, {
    scope: { connection: "local", company: "acme" },
    children: createElement(Harness, { client }),
  });
}

async function flush() {
  await act(async () => {
    for (let i = 0; i < 10; i++) await Promise.resolve();
    await new Promise((r) => setTimeout(r, 0));
  });
}

async function mount(client: OpenCompanyClient) {
  await act(async () => {
    root.render(tree(client));
  });
  await flush();
}

function approveButton(): HTMLButtonElement | undefined {
  return Array.from(container.querySelectorAll("button")).find(
    (b) => b.textContent === "Approve" || b.textContent === "Approving…",
  );
}

describe("a card review the host refuses", () => {
  it("reports the refusal and leaves the card exactly as it was", async () => {
    const client = clientAs(() =>
      Promise.reject(new ApiError(404, "not_found", "no card is awaiting review")),
    );
    await mount(client);

    await act(async () => approveButton()?.click());
    await flush();

    expect(client.reviewCard).toHaveBeenCalled();
    expect(toasts.error).toHaveBeenCalledWith("no card is awaiting review");
    expect(toasts.success).not.toHaveBeenCalled();
    // Not silently dropped, not stuck "Approving…" — a retry is still on offer.
    const btn = approveButton();
    expect(btn?.textContent).toBe("Approve");
    expect(btn?.disabled).toBe(false);
  });

  it("names a generic problem for a refusal with no host message", async () => {
    const client = clientAs(() => Promise.reject(new Error()));
    await mount(client);

    await act(async () => approveButton()?.click());
    await flush();

    expect(toasts.error).toHaveBeenCalledWith("Couldn't record that review.");
  });
});
