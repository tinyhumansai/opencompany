// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { ApprovalSummary } from "@/api/types";
import { useApprovalThreadLinks, type ApprovalThreadLink } from "@/components/approval-card";

const T0 = new Date("2026-08-20T20:00:00Z").getTime();

function fakeClient(): OpenCompanyClient {
  return {
    listDesks: vi.fn(async () => [{ id: "engineering", name: "Engineering", members: [] }]),
    listTeam: vi.fn(async () => []),
  } as unknown as OpenCompanyClient;
}

function approval(id: string, thread?: string): ApprovalSummary {
  return {
    id,
    kind: "runtime.unlabelled_effect",
    amount_usd: null,
    at_millis: T0,
    agent: null,
    ...(thread ? { thread } : {}),
  };
}

let container: HTMLDivElement;
let root: Root;
let lastLinks: Map<string, ApprovalThreadLink> | null;

function Probe({
  client,
  approvals,
  holding = false,
}: {
  client: OpenCompanyClient;
  approvals: ApprovalSummary[];
  holding?: boolean;
}) {
  lastLinks = useApprovalThreadLinks(client, "acme", approvals, holding);
  return null;
}

async function render(client: OpenCompanyClient, approvals: ApprovalSummary[]) {
  await act(async () => {
    root.render(createElement(Probe, { client, approvals }));
  });
  // The effect resolves `listDesks`/`listTeam` in a microtask; let the
  // topology land before asserting on the derived links.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  lastLinks = null;
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("useApprovalThreadLinks", () => {
  it("links a pending request whose thread resolves to a desk", async () => {
    const client = fakeClient();
    await render(client, [approval("a1", "engineering")]);

    expect(lastLinks?.get("a1")).toEqual({ channelId: "engineering", label: "#engineering" });
  });

  it("derives links for a newly arrived approval on an already-known thread", async () => {
    const client = fakeClient();
    await render(client, [approval("a1", "engineering")]);
    expect(lastLinks?.get("a1")).toEqual({ channelId: "engineering", label: "#engineering" });

    // The second request shares a1's thread, so the set of distinct thread ids
    // — the key the topology fetch is keyed on — does not change. The link map
    // must still pick up a2, or its card would omit the "Asked in" line until
    // some later thread change rebuilt the map.
    await render(client, [approval("a1", "engineering"), approval("a2", "engineering")]);

    expect(lastLinks?.get("a1")).toEqual({ channelId: "engineering", label: "#engineering" });
    expect(lastLinks?.get("a2")).toEqual({ channelId: "engineering", label: "#engineering" });
  });

  it("leaves an unresolvable thread unlinked", async () => {
    const client = fakeClient();
    await render(client, [approval("a1", "someone-else")]);

    expect(lastLinks?.has("a1")).toBe(false);
  });

  it("links an approval raised on the main line to #general on a real company", async () => {
    // The built-in `#general` is in no desk list, so the desk scan cannot name
    // it — and this is the ordinary case, not an edge one: every company with
    // real desks reached it. `channelIdForThread` resolved `main` to a channel
    // and the label lookup then found nothing, so the card read "Origin
    // unavailable" for the one channel every company has.
    const client = fakeClient();
    await render(client, [approval("a1", "main")]);

    expect(lastLinks?.get("a1")).toEqual({ channelId: "main", label: "#general" });
  });

  it("labels an alias with the grandfathered desk that owns the line", async () => {
    // The approval was raised under `main`; the line renders as the blueprint's
    // own `#ops-lead` desk. Looking the desk up by the raw thread id found
    // nothing and the card read "Origin unavailable" — for a conversation whose
    // transcript is on screen. The lookup follows the resolved channel instead.
    const client = {
      listDesks: vi.fn(async () => [{ id: "general", name: "Ops lead", members: [] }]),
      listTeam: vi.fn(async () => []),
    } as unknown as OpenCompanyClient;
    await render(client, [approval("a1", "main")]);

    expect(lastLinks?.get("a1")).toEqual({ channelId: "general", label: "#ops-lead" });
  });

  it("lets a blueprint desk that authored a general id keep its own label", async () => {
    const client = {
      listDesks: vi.fn(async () => [{ id: "general", name: "Ops lead", members: [] }]),
      listTeam: vi.fn(async () => []),
    } as unknown as OpenCompanyClient;
    await render(client, [approval("a1", "general")]);

    expect(lastLinks?.get("a1")).toEqual({ channelId: "general", label: "#ops-lead" });
  });

  it("falls back to the default desks when /desks comes back empty", async () => {
    // A company with no declared `[[group_chat]]` entries gets `[]` from
    // /desks, yet ChatView and AppShell still show the default desks, and
    // `#general` above them. An approval raised on the main line must resolve
    // here too, or its "Asked in" link would silently disappear — and this is
    // the case that tells an empty *response* apart from a failed read below.
    const client = {
      listDesks: vi.fn(async () => []),
      listTeam: vi.fn(async () => []),
    } as unknown as OpenCompanyClient;
    await render(client, [approval("a1", "main")]);

    expect(lastLinks?.get("a1")).toEqual({ channelId: "main", label: "#general" });
  });

  it("does not guess desks when the desks read fails", async () => {
    // A failed read is not an empty response: ChatView surfaces the error rather
    // than inventing desks, and the hook's contract is that an unresolved thread
    // must not be guessed. The `main` thread stays unlinked.
    const client = {
      listDesks: vi.fn(async () => {
        throw new Error("offline");
      }),
      listTeam: vi.fn(async () => []),
    } as unknown as OpenCompanyClient;
    await render(client, [approval("a1", "main")]);

    expect(lastLinks?.has("a1")).toBe(false);
  });

  it("defers a link that resolves during the queue hold until release (#1593)", async () => {
    // The operator starts interacting before the desks/roster reads finish. A
    // topology that lands mid-hold must not swap an "Asked in" link into the
    // card under the pointer — it wraps differently from "Origin unavailable"
    // and would shift the decide buttons despite the frozen row snapshot. The
    // link applies when the hold releases instead.
    let resolveDesks!: (desks: unknown[]) => void;
    const client = {
      listDesks: vi.fn(
        () =>
          new Promise((resolve) => {
            resolveDesks = resolve;
          }),
      ),
      listTeam: vi.fn(async () => []),
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(
        createElement(Probe, {
          client,
          approvals: [approval("a1", "engineering")],
          holding: true,
        }),
      );
      await Promise.resolve();
    });

    // The desks read resolves while the hold is still active.
    await act(async () => {
      resolveDesks([{ id: "engineering", name: "Engineering", members: [] }]);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(lastLinks?.has("a1")).toBe(false);

    // Releasing the hold applies the deferred topology.
    await act(async () => {
      root.render(
        createElement(Probe, {
          client,
          approvals: [approval("a1", "engineering")],
          holding: false,
        }),
      );
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(lastLinks?.get("a1")).toEqual({ channelId: "engineering", label: "#engineering" });
  });
});
