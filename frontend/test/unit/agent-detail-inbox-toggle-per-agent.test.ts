// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { AgentDetailDto } from "@/api/types";
import { AgentDetailView } from "@/views/team/AgentDetailView";

/**
 * Regression proof for a review finding (Codex P2 + CodeRabbit Major,
 * independently, on the same defect): the agent-page Inbox toggle's pending
 * state used to be one shared boolean rather than keyed by agent id.
 *
 * `AgentDetailView` stays mounted across a same-document navigation to a
 * different agent (`TeamView` renders it with no `key`), so a shared flag
 * meant agent A's in-flight write disabled agent B's completely unrelated
 * switch — and because the write's own cleanup compared the *stale* agent id
 * it was called for against whichever agent was on screen when it resolved,
 * navigating away before the write landed left the flag stuck `true`
 * forever, disabling every subsequent agent's switch too.
 */

function detail(id: string): AgentDetailDto {
  return {
    id,
    name: id,
    role: "Growth",
    source: "overlay",
    editable: [],
    isOrchestrator: false,
    tools: { requested: [], companyAllow: [], deskAllow: [], deskCeilingActive: false, effective: [] },
    skills: { requested: null, companyAvailable: [], effective: [], overridden: false },
    desks: [],
    inboxEnabled: false,
  };
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient, agentId: string) {
  await act(async () => {
    root.render(
      createElement(AgentDetailView, { client, company: null, agentId, onBack: () => {} }),
    );
  });
}

/** Base UI's `Switch.Root` renders a `<span role="switch">`, not a native
 *  `<button>` — `aria-disabled` is what a disabled one actually carries. */
function inboxToggle() {
  const el = container.querySelector('[data-testid="agent-inbox-toggle"]');
  if (!el) throw new Error("inbox toggle not found in the rendered page");
  return {
    element: el,
    disabled: el.getAttribute("aria-disabled") === "true",
    click: () => el.dispatchEvent(new MouseEvent("click", { bubbles: true })),
  };
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

describe("the inbox toggle's pending state, across a same-document navigation between agents", () => {
  it("does not disable a different agent's switch while this one's write is in flight, and clears on its own agent when the write lands", async () => {
    let resolvePut: (() => void) | null = null;
    const put = vi.fn(
      () =>
        new Promise<{ key: string; address: string }>((resolve) => {
          resolvePut = () => resolve({ key: "agent-a", address: "agent-a@inbox.test" });
        }),
    );
    const client = {
      getAgent: vi.fn((id: string) => Promise.resolve(detail(id))),
      listTeam: vi.fn(() => Promise.resolve([])),
      updateAgent: vi.fn(),
      get: vi.fn(() => Promise.resolve({ role: "member" })),
      listHarnesses: vi.fn(() => Promise.resolve([])),
      put,
      scopeFor: () => "/api/v1/company",
    } as unknown as OpenCompanyClient;

    await show(client, "agent-a");
    expect(inboxToggle().disabled).toBe(false);

    // Flip agent A's inbox. The write is deliberately never resolved until
    // `resolvePut()` is called below.
    await act(async () => {
      inboxToggle().click();
    });
    expect(put).toHaveBeenCalledTimes(1);

    // Same-document navigation to a different agent while A's write is still
    // in flight. B has no write of its own pending, so its switch must be
    // usable — the bug this guards against disabled it anyway.
    await show(client, "agent-b");
    expect(
      inboxToggle().disabled,
      "agent B's switch must not be disabled by agent A's in-flight write",
    ).toBe(false);

    // Let A's write resolve now that B is on screen. Must not throw, and
    // must not leave B's switch stuck disabled by a cleanup that was still
    // checking for agent A.
    await act(async () => {
      resolvePut?.();
      await Promise.resolve();
    });
    expect(inboxToggle().disabled).toBe(false);

    // And back on A, its switch is usable again too — the write's own
    // cleanup has to have cleared A's pending entry outright, not only when
    // A happened to still be the displayed agent.
    await show(client, "agent-a");
    expect(inboxToggle().disabled).toBe(false);
  });
});
