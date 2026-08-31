// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { ApprovalSummary } from "@/api/types";
import { useAskerNames } from "@/components/approval-card";

/**
 * Issue #1593: `useAskerNames` populates the labels the standing-permissions
 * section shows ABOVE the approvals queue. A roster name landing mid-hold (raw
 * agent id → display name) can change a card's wrapping and shift every
 * approve/decline control under the operator's pointer — so, like
 * `useStandingGrants`, the names must be deferred for the interaction hold and
 * applied the moment it releases.
 */

function approval(id: string, agent: string): ApprovalSummary {
  return { id, kind: "web_fetch", amount_usd: null, at_millis: 0, agent };
}

let container: HTMLDivElement;
let root: Root;
let lastNames: Map<string, string> | null;

function Probe({
  client,
  approvals,
  holding,
}: {
  client: OpenCompanyClient;
  approvals: ApprovalSummary[];
  holding: boolean;
}) {
  lastNames = useAskerNames(client, "acme", approvals, holding);
  return null;
}

/** A client whose roster read resolves on demand, so the test can land it
 *  during a hold. */
function controllableClient(roster: { id: string; name: string }[]) {
  let resolve!: (value: { id: string; name: string }[]) => void;
  const promise = new Promise<{ id: string; name: string }[]>((r) => {
    resolve = r;
  });
  return {
    client: { listTeam: () => promise } as unknown as OpenCompanyClient,
    resolveRoster: () => resolve(roster),
  };
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  lastNames = null;
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("useAskerNames (#1593)", () => {
  it("defers names that arrive during the hold and applies them on release", async () => {
    const { client, resolveRoster } = controllableClient([
      { id: "ops", name: "Ops Agent" },
    ]);

    // Mount while the queue is held. The roster read resolves during the hold.
    await act(async () => {
      root.render(
        createElement(Probe, {
          client,
          approvals: [approval("a1", "ops")],
          holding: true,
        }),
      );
    });
    await act(async () => {
      resolveRoster();
      await Promise.resolve();
      await Promise.resolve();
    });
    // Held: the name is stashed, not applied — the label must not change under
    // the operator's pointer.
    expect(lastNames?.get("ops")).toBeUndefined();

    // The hold releases: the stashed name lands.
    await act(async () => {
      root.render(
        createElement(Probe, {
          client,
          approvals: [approval("a1", "ops")],
          holding: false,
        }),
      );
    });
    expect(lastNames?.get("ops")).toBe("Ops Agent");
  });

  it("applies names immediately when not holding", async () => {
    const { client, resolveRoster } = controllableClient([
      { id: "ops", name: "Ops Agent" },
    ]);
    await act(async () => {
      root.render(
        createElement(Probe, {
          client,
          approvals: [approval("a1", "ops")],
          holding: false,
        }),
      );
    });
    await act(async () => {
      resolveRoster();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(lastNames?.get("ops")).toBe("Ops Agent");
  });
});
