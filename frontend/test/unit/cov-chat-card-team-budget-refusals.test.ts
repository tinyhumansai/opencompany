// @vitest-environment jsdom

import { act, createElement, useState, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { BUDGET_PAUSE_NOTICE_PREFIX } from "@/hooks/use-events";
import { RoomView } from "@/views/RoomView";
import type { ChatMessage } from "@/lib/chat";
import type { Transcripts } from "@/views/room/model";
import type { TaskStatus } from "@/api/tasks";

/**
 * `RoomView` wires several inline actions with their own host round trip and
 * their own catch block — dismissing a card, redeeming a budget pause. Every
 * one of those catch blocks was written and never driven end to end: this
 * file mounts the real view, triggers each control, and has the client
 * refuse the write the way the host actually does, then asserts the screen
 * says something true instead of a false success or a silent nothing.
 *
 * (Send, reactions and the settle-pill Approve get the same treatment in
 * `cov-chat-composer-send-fail.test.ts`, `cov-chat-reaction-404-rollback.test.ts`
 * and `cov-chat-review-card-fail.test.ts`. Removing a teammate used to be
 * covered here too — `RoomView`'s member pane dropped that action entirely
 * in issue #2224; the equivalent refusal handling now lives only in
 * `TeamView.removeMember`, which has no refusal-path coverage of its own yet.)
 */

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));
vi.mock("sonner", () => ({
  toast: Object.assign(toasts.base, {
    success: toasts.success,
    error: toasts.error,
    warning: toasts.warning,
    info: toasts.info,
  }),
}));

const DESK_DTO = { id: "main", name: "main", description: "The main channel", members: [] as string[] };
const MEMBER_DTO = { id: "m1", name: "Ada", role: "engineer" };

interface Overrides {
  del?: () => Promise<unknown>;
  redeemBudgetPause?: () => Promise<unknown>;
}

function clientAs(overrides: Overrides): OpenCompanyClient {
  const named: Record<string, unknown> = {
    scopeFor: () => "/api/v1/companies/acme",
    get: (path: string) => {
      if (path.endsWith("/auth/me")) {
        return Promise.resolve({ id: "u1", email: "a@b.c", role: "member", company: "acme" });
      }
      return Promise.resolve([]);
    },
    listDesks: () => Promise.resolve([DESK_DTO]),
    listTeam: () => Promise.resolve([MEMBER_DTO]),
    mentionables: () => Promise.resolve([]),
    capabilityStatus: () => Promise.resolve({ cognition: null }),
    del: vi.fn(overrides.del ?? (() => Promise.resolve())),
    getBudgetPause: vi.fn(() => Promise.resolve(null)),
    redeemBudgetPause: vi.fn(overrides.redeemBudgetPause ?? (() => Promise.resolve())),
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
  toasts.base.mockClear();
  toasts.success.mockClear();
  toasts.error.mockClear();
  toasts.info.mockClear();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

/**
 * `transcripts`/`setTranscripts` are controlled props the shell owns — every
 * action under test here (`clearCardEverywhere`, the approve pill's own
 * re-render) writes through `setTranscripts`, so a `vi.fn()` stub would
 * silently discard every one of them. This holds the real state a host page
 * would.
 */
function Harness({
  client,
  initialTranscripts,
  taskStatusByTaskId,
}: {
  client: OpenCompanyClient;
  initialTranscripts: Transcripts;
  taskStatusByTaskId?: Record<string, TaskStatus>;
}) {
  const [transcripts, setTranscripts] = useState<Transcripts>(initialTranscripts);
  const view = createElement(RoomView, {
    client,
    company: "acme",
    sub: "main",
    onNavigate: vi.fn(),
    transcripts,
    setTranscripts,
    resolveTypingNames: () => [],
    scopeRef: { current: { connection: "local", company: "acme", client } },
    taskStatusByTaskId,
  });
  return createElement(ConnectionScopeProvider, {
    scope: { connection: "local", company: "acme" },
    children: view,
  });
}

function tree(client: OpenCompanyClient, transcripts: Transcripts, taskStatusByTaskId?: Record<string, TaskStatus>): ReactNode {
  return createElement(Harness, { client, initialTranscripts: transcripts, taskStatusByTaskId });
}

async function flush() {
  await act(async () => {
    for (let i = 0; i < 10; i++) await Promise.resolve();
    await new Promise((r) => setTimeout(r, 0));
  });
}

async function mount(client: OpenCompanyClient, transcripts: Transcripts = {}, taskStatusByTaskId?: Record<string, TaskStatus>) {
  await act(async () => {
    root.render(tree(client, transcripts, taskStatusByTaskId));
  });
  await flush();
}

function bodyButton(text: string): HTMLButtonElement | undefined {
  return [...document.body.querySelectorAll("button")].find((b) => (b.textContent ?? "").includes(text)) as
    | HTMLButtonElement
    | undefined;
}

/**
 * `dismissCard` is deliberately NOT optimistic — the chip must stay
 * exactly where it was on a refusal, unlike `react`'s rollback, because a chip
 * that vanished while the card survives on the board is the false state this
 * function's own doc says it exists to avoid.
 */
describe("dismissing a card the host refuses to delete (AUTH — the console must not lie about it)", () => {
  const MESSAGE: ChatMessage = { id: "h2", from: "company", text: "opened a card", at: 0, taskId: "task-1" };

  async function openConfirm() {
    const dismiss = container.querySelector('[aria-label="Dismiss this card"]') as HTMLButtonElement;
    expect(dismiss).not.toBeNull();
    await act(async () => dismiss.click());
    await flush();
    const confirm = bodyButton("Dismiss card");
    expect(confirm).not.toBeUndefined();
    await act(async () => confirm?.click());
    await flush();
  }

  it("keeps the chip in place and reports the refusal, rather than clearing it", async () => {
    const client = clientAs({
      del: () => Promise.reject(new ApiError(409, "conflict", "This card still has work running.")),
    });
    await mount(client, { main: [MESSAGE] }, { "task-1": { column: "in_review" } });
    await openConfirm();

    expect(toasts.error).toHaveBeenCalledWith("This card still has work running.");
    // Still there: the link half of the chip survives a refused delete.
    expect(container.querySelector('a[href="#/company/tasks/task-1"]')).not.toBeNull();
  });

  it("clears the chip when the host says the card is already gone (404)", async () => {
    const client = clientAs({
      del: () => Promise.reject(new ApiError(404, "not_found", "no such card")),
    });
    await mount(client, { main: [MESSAGE] }, { "task-1": { column: "in_review" } });
    await openConfirm();

    expect(toasts.success).toHaveBeenCalledWith("That card was already gone — chip cleared.");
    expect(container.querySelector('a[href="#/company/tasks/task-1"]')).toBeNull();
  });
});

/**
 * `redeemBudgetPause`'s two documented, non-retryable refusals — a
 * 404 (already redeemed elsewhere) read as a delayed success, and a 409 (the
 * marker changed since the notice was shown, e.g. the company is paused) told
 * to the operator as "look again" rather than silently retried against the
 * wrong marker.
 */
describe("redeeming a parked budget pause from its chat notice (AUTH — a stale/paused refusal)", () => {
  function pauseMessage(): ChatMessage {
    return {
      id: "h4",
      from: "system",
      text: `${BUDGET_PAUSE_NOTICE_PREFIX} Paused — agent1's turn ran out of inference budget/credits.`,
      at: 0,
    };
  }

  async function clickResend() {
    const btn = [...container.querySelectorAll("button")].find((b) =>
      (b.textContent ?? "").includes("Add credits & resend"),
    );
    expect(btn, "the Add credits & resend button").not.toBeUndefined();
    await act(async () => (btn as HTMLButtonElement).click());
    await flush();
  }

  it("reads a 404 as already handled, not as an error", async () => {
    const client = clientAs({
      redeemBudgetPause: () => Promise.reject(new ApiError(404, "not_found", "no such pause")),
    });
    await mount(client, { main: [pauseMessage()] });
    await clickResend();

    expect(toasts.base).toHaveBeenCalledWith("Nothing to resend — that pause was already handled.");
    expect(toasts.error).not.toHaveBeenCalled();
  });

  it("tells the operator to look again on a 409 (stale-marker/paused-company refusal), rather than silently retrying", async () => {
    const client = clientAs({
      redeemBudgetPause: () => Promise.reject(new ApiError(409, "conflict", "marker changed")),
    });
    await mount(client, { main: [pauseMessage()] });
    await clickResend();

    expect(toasts.base).toHaveBeenCalledWith(
      "That pause has changed since it was shown — check the latest message and try again.",
    );
    expect(toasts.error).not.toHaveBeenCalled();
  });

  it("falls back to an honest error toast for any other refusal", async () => {
    const client = clientAs({
      redeemBudgetPause: () => Promise.reject(new ApiError(500, "server_error", "temporarily unavailable")),
    });
    await mount(client, { main: [pauseMessage()] });
    await clickResend();

    expect(toasts.error).toHaveBeenCalledWith("temporarily unavailable");
  });
});
