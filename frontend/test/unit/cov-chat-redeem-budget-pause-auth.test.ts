// @vitest-environment jsdom

import { act, createElement, useState, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { BUDGET_PAUSE_NOTICE_PREFIX } from "@/hooks/use-events";
import type { OpenCompanyClient } from "@/api/client";
import type { ChatMessage } from "@/lib/chat";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import type { Transcripts } from "@/views/room/model";
import { RoomView } from "@/views/RoomView";

/**
 * `POST {scope}/agents/{agent_id}/budget-pause/redeem`
 * (`server/ops/budget_pause.rs`) is `ScopedCompany` — any company member, not
 * `AdminScopedCompany` — and `RoomView` wires `onRedeemBudgetPause`
 * unconditionally, with no `isAdmin` check anywhere near it. That is the
 * opposite gate from the daily-budget menu item a few rows away in the same
 * pane (`canEditBudget={isAdmin && fromHost}`,
 * `cov-chat-members-budget-auth.test.ts`) — deliberately, per that route's
 * own doc comment. This pins "Add credits & resend" stays live for a plain
 * member.
 */

const DESK_DTO = { id: "main", name: "main", description: "The main channel", members: [] as string[] };
const MEMBER_DTO = { id: "m1", name: "Ada", role: "engineer" };
const NOTICE_TEXT = `${BUDGET_PAUSE_NOTICE_PREFIX} Paused — seo's turn ran out of inference budget/credits.`;
const NOTICE: ChatMessage = { id: "h1", from: "system", text: NOTICE_TEXT, at: Date.UTC(2026, 8, 1, 9, 0, 0) };

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
    redeemBudgetPause: vi.fn(async () => ({})),
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

/** Owns `transcripts` state itself, the way `AppShell` does for the real view. */
function Harness({ client }: { client: OpenCompanyClient }) {
  const [transcripts, setTranscripts] = useState<Transcripts>({ main: [NOTICE] });
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

function redeemButton(): HTMLButtonElement | undefined {
  return Array.from(container.querySelectorAll("button")).find((b) =>
    (b.textContent ?? "").includes("Add credits & resend"),
  );
}

describe("the budget-pause redeem CTA, for a plain member", () => {
  it("offers a live Add-credits button, matching the host's any-member redeem route", async () => {
    await mount(clientAs("member"));

    const btn = redeemButton();
    expect(btn).not.toBeUndefined();
    expect(btn?.disabled).toBe(false);
  });

  it("actually calls client.redeemBudgetPause for a member session", async () => {
    const client = clientAs("member");
    await mount(client);

    await act(async () => redeemButton()?.click());
    await flush();

    expect(client.redeemBudgetPause).toHaveBeenCalledWith("seo", "acme", undefined);
  });

  it("renders the same live button for an admin, so the control does not differ by role", async () => {
    await mount(clientAs("admin"));

    expect(redeemButton()?.disabled).toBe(false);
  });
});
