// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { ApprovalSummary, StandingGrant } from "@/api/types";
import type { CompanyFeed } from "@/hooks/use-company";
import { ApprovalsView } from "@/views/ApprovalsView";

const NOW = new Date("2026-08-23T10:00:00Z").getTime();

function approval(id: string, expires_at_millis: number): ApprovalSummary {
  return {
    id,
    kind: "web_fetch",
    amount_usd: null,
    at_millis: NOW,
    expires_at_millis,
  };
}

const GRANT: StandingGrant = {
  id: "grant-1",
  agent: "ops",
  tool: "shell",
  granted_by: { kind: "user", id: "operator" },
  verdict: "approve",
  at_millis: NOW,
  expires_at_millis: NOW + 60 * 60 * 1000,
};

const client = {
  get: async <T>(path: string): Promise<T> =>
    (path.endsWith("/users") ? [] : null) as T,
  listGrants: async () => [GRANT],
  listTeam: async () => [],
  revokeGrant: async () => undefined,
  scopeFor: () => "/api/v1/company",
} as unknown as OpenCompanyClient;

const feed: CompanyFeed = {
  status: {} as CompanyFeed["status"],
  approvals: [
    approval("tomorrow", NOW + 24 * 60 * 60 * 1000),
    approval("soon", NOW + 4 * 60 * 1000),
  ],
  queue: "ready",
  now: NOW,
  refresh: async () => undefined,
};

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
});

describe("the approvals backlog queue (#1427)", () => {
  it("keeps orientation sticky, orders cards by deadline, and puts revocation first", async () => {
    await act(async () => {
      root.render(
        createElement(ApprovalsView, {
          client,
          company: null,
          feed,
          onResolved: () => {},
          onGoToConversation: () => {},
        }),
      );
      await Promise.resolve();
      await Promise.resolve();
    });

    const queueHeading = [...container.querySelectorAll("h2")].find(
      (heading) => heading.textContent === "2 things need your approval",
    );
    const permissionsHeading = [...container.querySelectorAll("h2")].find(
      (heading) => heading.textContent === "Standing permissions",
    );
    expect(queueHeading).toBeDefined();
    expect(permissionsHeading).toBeDefined();
    expect(
      permissionsHeading!.compareDocumentPosition(queueHeading!) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    // The heading and the deadline sentence both live in the sticky orientation
    // header; main's bulk-actions merge put a flex row between the heading and
    // the sticky edge, so find the header rather than asserting an exact parent.
    const stickyHeader = queueHeading!.closest(".sticky");
    expect(stickyHeader).not.toBeNull();
    expect(stickyHeader!.textContent).toContain("Each one has a deadline.");
    expect(
      [...container.querySelectorAll("[data-approval-id]")].map((row) =>
        row.getAttribute("data-approval-id"),
      ),
    ).toEqual(["soon", "tomorrow"]);
  });

  it("holds the whole permissions-and-queue region while revoking a grant (#1593)", async () => {
    vi.useFakeTimers();
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });

    let granted = true;
    const revokingClient = {
      ...client,
      listGrants: async () => (granted ? [GRANT] : []),
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(
        createElement(ApprovalsView, {
          client: revokingClient,
          company: null,
          feed,
          onResolved: () => {},
          onGoToConversation: () => {},
        }),
      );
      await Promise.resolve();
      await Promise.resolve();
    });

    const permissionHeading = () =>
      [...container.querySelectorAll("h2")].find(
        (heading) => heading.textContent === "Standing permissions",
      );
    expect(permissionHeading()).toBeDefined();

    const revoke = [...container.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Remove"),
    )!;
    await act(async () => revoke.focus());

    // Another tab revokes it while the operator is still aiming at this control.
    // The section must not disappear and pull the approval buttons upward.
    granted = false;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(permissionHeading()).toBeDefined();

    await act(async () => revoke.blur());
    expect(permissionHeading()).toBeUndefined();
  });

  it("holds approval timestamps with the queue snapshot while interacting (#1593)", async () => {
    vi.useFakeTimers();
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });

    const feedAt = (now: number): CompanyFeed => ({ ...feed, now });

    await act(async () => {
      root.render(
        createElement(ApprovalsView, {
          client,
          company: null,
          feed: feedAt(NOW),
          onResolved: () => {},
          onGoToConversation: () => {},
        }),
      );
      await Promise.resolve();
      await Promise.resolve();
    });

    // "soon" was parked at `NOW`, so its meta reads "just now" / "4m".
    const card = () =>
      [...container.querySelectorAll("[data-approval-id]")].find((row) =>
        row.getAttribute("data-approval-id")?.startsWith("soon"),
      )!;
    expect(card().textContent).toContain("just now");

    // Aim at the card's decide control to hold the interaction region.
    await act(async () => {
      card().querySelector("button")!.focus();
    });

    // A poll lands with a newer clock while the operator is still inside the
    // queue. The card's age and deadline must not advance with it — the held
    // snapshot is what the operator is looking at.
    await act(async () => {
      root.render(
        createElement(ApprovalsView, {
          client,
          company: null,
          feed: feedAt(NOW + 61_000),
          onResolved: () => {},
          onGoToConversation: () => {},
        }),
      );
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(card().textContent).toContain("just now");

    // Moving away releases the hold and reconciles to the newer clock.
    await act(async () => {
      (document.activeElement as HTMLElement).blur();
    });
    expect(card().textContent).not.toContain("just now");
  });

  it("holds batch positions with the frozen rows while interacting (#1593)", async () => {
    vi.useFakeTimers();
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });

    // A turn that parked two gated calls reads "1 of 2" and "2 of 2".
    const batched = (id: string, batch = "turn-7"): ApprovalSummary => ({
      ...approval(id, NOW + 4 * 60 * 1000),
      batch,
    });
    const feedAt = (approvals: ApprovalSummary[]): CompanyFeed => ({
      ...feed,
      approvals,
    });

    await act(async () => {
      root.render(
        createElement(ApprovalsView, {
          client,
          company: null,
          feed: feedAt([batched("first"), batched("second")]),
          onResolved: () => {},
          onGoToConversation: () => {},
        }),
      );
      await Promise.resolve();
      await Promise.resolve();
    });

    const card = (id: string) =>
      [...container.querySelectorAll("[data-approval-id]")].find(
        (row) => row.getAttribute("data-approval-id") === id,
      )!;
    expect(card("first").textContent).toContain("1 of 2 from the same turn");
    expect(card("second").textContent).toContain("2 of 2 from the same turn");

    // Aim at the first card's decide control to hold the interaction region.
    await act(async () => {
      card("first").querySelector("button")!.focus();
    });

    // A poll lands while the operator is still inside the queue: "second" was
    // decided in another tab. The frozen rows still show both cards, and the
    // batch map must not renumber the card under the pointer to "1 of 1" (line
    // vanishes) or drop its sibling's count — either would rewrap the card
    // above the targeted control.
    await act(async () => {
      root.render(
        createElement(ApprovalsView, {
          client,
          company: null,
          feed: feedAt([batched("first")]),
          onResolved: () => {},
          onGoToConversation: () => {},
        }),
      );
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(card("first").textContent).toContain("1 of 2 from the same turn");
    expect(card("second").textContent).toContain("2 of 2 from the same turn");

    // Moving away releases the hold and reconciles to the live count: the one
    // remaining card is now alone in its batch and the line is not shown.
    await act(async () => {
      (document.activeElement as HTMLElement).blur();
    });
    expect(card("first").textContent).not.toContain("1 of 2");
  });
});
