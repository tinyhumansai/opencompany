// @vitest-environment jsdom

import { act, createElement, useState, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { ChatMessage } from "@/lib/chat";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import type { Transcripts } from "@/views/room/model";
import { RoomView } from "@/views/RoomView";

const toasts = vi.hoisted(() => ({ base: vi.fn(), success: vi.fn(), error: vi.fn() }));
vi.mock("sonner", () => ({
  toast: Object.assign(toasts.base, { success: toasts.success, error: toasts.error, warning: vi.fn(), info: vi.fn() }),
}));

/**
 * `RoomView.react` is optimistic — the chip flips before the
 * round trip — and rolls back only on a refusal it can actually read. The
 * idempotent `on` write itself is straightforward; what had no test is the
 * rollback: a host with no reactions route (`404`) has to leave the chip
 * exactly where it was before the click, with a toast naming why, rather
 * than a chip that quietly disagrees with the host until the next reload.
 */

const DESK_DTO = { id: "main", name: "main", description: "The main channel", members: [] as string[] };
const MESSAGE: ChatMessage = { id: "h1", from: "company", text: "shipped it", at: Date.UTC(2026, 8, 1, 9, 0, 0) };

function clientAs(reactToMessage: (seq: string, emoji: string, on: boolean) => Promise<void>): OpenCompanyClient {
  const named: Record<string, unknown> = {
    scopeFor: () => "/api/v1/companies/acme",
    listDesks: () => Promise.resolve([DESK_DTO]),
    reactToMessage: vi.fn(reactToMessage),
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
  const [transcripts, setTranscripts] = useState<Transcripts>({ main: [MESSAGE] });
  return createElement(RoomView, {
    client,
    company: "acme",
    sub: "main",
    onNavigate: vi.fn(),
    transcripts,
    setTranscripts,
    resolveTypingNames: () => [],
    scopeRef: { current: { connection: "local", company: "acme", client } },
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

function thumbsUp(): HTMLButtonElement {
  return container.querySelector('button[aria-label^="React with "]') as HTMLButtonElement;
}

function reactionChip(): HTMLButtonElement | null {
  const article = container.querySelector('[data-message-id="h1"]');
  if (!article) return null;
  return (
    ([...article.querySelectorAll("button[aria-pressed]")].find(
      (b) => !(b.getAttribute("aria-label") ?? "").startsWith("React with "),
    ) as HTMLButtonElement | undefined) ?? null
  );
}

describe("reacting to a message, when the host's write fails", () => {
  it("rolls the optimistic chip back on a 404 and says the host has no reactions route", async () => {
    const client = clientAs(() =>
      Promise.reject(new ApiError(404, "not_found", "no reactions route")),
    );
    await mount(client);

    await act(async () => thumbsUp().click());
    await flush();

    expect(client.reactToMessage).toHaveBeenCalledWith("1", "👍", true, "acme");
    expect(reactionChip()).toBeNull();
    expect(toasts.error).toHaveBeenCalledWith("This host doesn't keep reactions yet.");
  });

  it("rolls back the same way on any other refusal, with the host's own message", async () => {
    const client = clientAs(() =>
      Promise.reject(new ApiError(500, "internal", "reactions store is down")),
    );
    await mount(client);

    await act(async () => thumbsUp().click());
    await flush();

    expect(reactionChip()).toBeNull();
    expect(toasts.error).toHaveBeenCalledWith("reactions store is down");
  });

  it("keeps the chip and reports nothing when the write lands", async () => {
    const client = clientAs(() => Promise.resolve());
    await mount(client);

    await act(async () => thumbsUp().click());
    await flush();

    expect(reactionChip()).not.toBeNull();
    expect(toasts.error).not.toHaveBeenCalled();
  });
});
