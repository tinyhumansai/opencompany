// @vitest-environment jsdom

import { act, createElement, useState, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import type { Transcripts } from "@/views/room/model";
import { RoomView } from "@/views/RoomView";

/**
 * `RoomView.send`'s own `catch`: a host that refuses
 * the POST — an over-length body, a 4xx of any other shape — must leave an
 * honest line in the transcript rather than dropping the operator's message
 * or leaving it looking sent. `MessageComposer` clears the draft the instant
 * `onSend` is called, so the only place a refusal can still be said is the
 * transcript this function appends to in its `catch`.
 *
 * No client-side length cap exists anywhere in the composer or `chat.ts` to
 * test against — this exercises the one path that
 * genuinely stands between a refused send and a silently lost message: the
 * host's own rejection, whatever shape it takes.
 */

const DESK_DTO = { id: "main", name: "main", description: "The main channel", members: [] as string[] };

function clientAs(chat: () => Promise<never>): OpenCompanyClient {
  const named: Record<string, unknown> = {
    scopeFor: () => "/api/v1/companies/acme",
    listDesks: () => Promise.resolve([DESK_DTO]),
    chat: vi.fn(chat),
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
  // jsdom does not implement scrollTo; `MessageTimeline` calls it to follow
  // new messages, which would otherwise throw on every render with content.
  Element.prototype.scrollTo = vi.fn();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

/** Owns `transcripts` state itself, the way `AppShell` does for the real view. */
function Harness({ client }: { client: OpenCompanyClient }) {
  const [transcripts, setTranscripts] = useState<Transcripts>({});
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

function composerInput(): HTMLTextAreaElement {
  return container.querySelector('textarea[aria-label^="Message "]') as HTMLTextAreaElement;
}

function type(el: HTMLTextAreaElement, text: string) {
  const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!;
  act(() => {
    setter.call(el, text);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function send() {
  const button = container.querySelector('[aria-label="Send"]') as HTMLButtonElement;
  await act(async () => {
    button.click();
  });
  await flush();
}

describe("a chat send the host refuses", () => {
  it("leaves an honest line in the transcript instead of dropping the message", async () => {
    const client = clientAs(() =>
      Promise.reject(new ApiError(413, "message_too_long", "That message is too long to send.")),
    );
    await mount(client);

    type(composerInput(), "a message the host will refuse");
    await send();

    expect(client.chat).toHaveBeenCalled();
    // The draft is cleared optimistically (`MessageComposer.send`) — the
    // refusal must not leave the operator's words looking lost.
    expect(composerInput().value).toBe("");
    expect(container.textContent).toContain("a message the host will refuse");
    expect(container.textContent).toContain(
      "Not sent — That message is too long to send.",
    );
  });

  it("names a generic problem for a refusal with no host message", async () => {
    const client = clientAs(() => Promise.reject(new Error("network down")));
    await mount(client);

    type(composerInput(), "hello");
    await send();

    expect(container.textContent).toContain("hello");
    expect(container.textContent).toContain("Not sent — something went wrong");
  });
});
