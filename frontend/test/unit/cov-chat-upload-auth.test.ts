// @vitest-environment jsdom

import { act, createElement, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { AttachmentDto } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { RoomView } from "@/views/RoomView";

/**
 * `POST {scope}/chat/upload` (`workspace.rs`) sits under the same `scoped(…)`
 * guard as every other chat write — any company member, not admin-only — and
 * `RoomView` wires `uploadAttachment` into the composer unconditionally
 * (`useCallback(… uploadChatAttachment …)`, no `isAdmin` check anywhere near
 * it). That is the opposite gate from the daily-budget menu item a few rows
 * away in the same pane (`canEditBudget={isAdmin && fromHost}`,
 * `cov-chat-members-budget-auth.test.ts`), so this pins the paperclip stays
 * live for a plain member rather than having quietly inherited an admin-only
 * check the way that field once needed adding.
 */

const REFERENCE: AttachmentDto = { nodeId: "n1", name: "diagram.png", mime: "image/png", size: 2048 };
const DESK_DTO = { id: "main", name: "main", description: "The main channel", members: [] as string[] };
const MEMBER_DTO = { id: "m1", name: "Ada", role: "engineer" };

function clientAs(role: "admin" | "member"): OpenCompanyClient {
  const named: Record<string, unknown> = {
    scopeFor: () => "/api/v1/companies/acme",
    get: (path: string) => {
      if (path.endsWith("/auth/me")) {
        return Promise.resolve({ id: "u1", email: "a@b.c", role, company: "acme" });
      }
      return Promise.resolve([]);
    },
    listDesks: () => Promise.resolve([DESK_DTO]),
    listTeam: () => Promise.resolve([MEMBER_DTO]),
    postForm: vi.fn(async () => REFERENCE),
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
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

function tree(client: OpenCompanyClient): ReactNode {
  const view = createElement(RoomView, {
    client,
    company: "acme",
    sub: "main",
    onNavigate: vi.fn(),
    transcripts: {},
    setTranscripts: vi.fn(),
    resolveTypingNames: () => [],
    scopeRef: { current: { connection: "local", company: "acme", client } },
  });
  return createElement(ConnectionScopeProvider, {
    scope: { connection: "local", company: "acme" },
    children: view,
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

function paperclip(): HTMLButtonElement | null {
  return container.querySelector('[aria-label="Attach a file"]');
}

describe("the chat attach control, for a plain member", () => {
  it("offers a live paperclip, matching the host's any-member upload route", async () => {
    await mount(clientAs("member"));

    const clip = paperclip();
    expect(clip).not.toBeNull();
    expect(clip?.disabled).toBe(false);
  });

  it("actually uploads through client.postForm for a member session", async () => {
    const client = clientAs("member");
    await mount(client);

    const file = new File(["bytes"], "diagram.png", { type: "image/png" });
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    Object.defineProperty(input, "files", { value: [file], configurable: true });
    await act(async () => {
      input.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await flush();

    expect(client.postForm).toHaveBeenCalledWith("/api/v1/companies/acme/chat/upload", expect.anything());
    expect(container.textContent).toContain("diagram.png");
  });

  it("renders the same live paperclip for an admin, so the control does not differ by role", async () => {
    await mount(clientAs("admin"));

    expect(paperclip()?.disabled).toBe(false);
  });
});
