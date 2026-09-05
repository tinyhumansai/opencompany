// @vitest-environment jsdom

import { act, createElement, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { PolicyStatus } from "@/api/policy";
import { useAutonomy } from "@/hooks/use-autonomy";

/**
 * What the title row is handed, as distinct from what any one surface knows.
 *
 * The autonomy pill is mounted for the entire life of the console, on every
 * view, and it makes a claim in a sentence: what the agents in the company
 * named an inch to its left are allowed to do without asking. Two ways that
 * claim can be **wrong rather than merely stale**, and both are here:
 *
 * 1. **A write on the settings page.** `PolicySettings` and `useAutonomy` read
 *    the same `GET {scope}/policy`, but only the pill's own switcher used to
 *    publish its result. Change the tier from the settings card — which is the
 *    surface the console points operators at — and the pill above it kept the
 *    previous tier for up to the 30s poll. In the direction that matters most,
 *    a widening, that is the row insisting a more restrictive policy is still
 *    in force while it is not.
 *
 * 2. **A company switch.** `useState` survives a change of `company`; the
 *    effect that cleared it is passive, so React ran it AFTER paint. The first
 *    frame of the new company therefore carried the previous company's tier —
 *    a confident, attributed answer about a different company.
 *
 * Both are tested through the hook rather than through the pill, because the
 * pill is a faithful renderer of whatever it is given and neither bug lives
 * there.
 */

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));

vi.mock("sonner", () => ({ toast: Object.assign(toasts.base, toasts) }));

const { PolicySettings } = await import("@/components/policy-settings");

const TIERS = [
  { value: "readonly", label: "Read-only", description: "Looks, changes nothing." },
  { value: "supervised", label: "Supervised", description: "Asks before acting." },
  { value: "auto", label: "Auto", description: "Acts on its own." },
  { value: "full", label: "Full", description: "No ceiling." },
];

function policy(mode: string): PolicyStatus {
  return {
    mode,
    alwaysApprove: [],
    autoApproveUnderUsd: null,
    approvalTtlHours: 24,
    manifestMode: mode,
    manifestAlwaysApprove: [],
    manifestAutoApproveUnderUsd: null,
    manifestApprovalTtlHours: null,
    overridden: false,
    takesEffect: "on the next turn",
    tiers: TIERS,
  };
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  vi.clearAllMocks();
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

// ---------------------------------------------------------------------------

describe("a policy written on the settings page", () => {
  /**
   * A host whose GET is frozen on the OLD tier for the life of the test.
   *
   * That is the whole design of this fake. If the reader ever showed the new
   * tier because a poll fetched it, this suite would pass against the bug it
   * exists to catch. Frozen, the ONLY route from the settings card to the
   * title row is the publish — so the assertion cannot be satisfied any other
   * way.
   */
  function frozenHost(before: string, after: string) {
    const put = vi.fn(async (_path: string, body: { mode?: string }) =>
      policy(body.mode ?? after),
    );
    return {
      put,
      client: {
        scopeFor: () => "/api/v1/company/acme",
        get: async (path: string) =>
          path.endsWith("/policy") ? policy(before) : { slugs: [], unwired: [] },
        put,
        del: vi.fn(async () => policy(before)),
      } as unknown as OpenCompanyClient,
    };
  }

  /** The settings card and a title-row reader, in the same scope, as the shell mounts them. */
  function Both({ api, seen }: { api: OpenCompanyClient; seen: string[] }): ReactNode {
    const status = useAutonomy(api, "acme");
    seen.push(status?.mode ?? "unknown");
    return createElement(PolicySettings, { client: api, company: "acme", canManage: true });
  }

  async function mount(api: OpenCompanyClient, seen: string[]) {
    await act(async () => {
      root.render(createElement(Both, { api, seen }));
      await Promise.resolve();
    });
  }

  it("reaches the title row's readers at once, not on the next poll", async () => {
    // Narrowing: `readonly` is below `auto` in the host's order, so it writes
    // straight through with no confirmation to click.
    vi.useFakeTimers();
    try {
      const { client, put } = frozenHost("auto", "readonly");
      const seen: string[] = [];
      await mount(client, seen);
      expect(seen.at(-1)).toBe("auto");

      await act(async () => {
        container
          .querySelector<HTMLButtonElement>("[data-testid=policy-tier-readonly]")!
          .click();
        await Promise.resolve();
      });

      expect(put).toHaveBeenCalledWith("/api/v1/company/acme/policy", { mode: "readonly" });
      // No timer has advanced, and the GET still answers `auto`. The only way
      // this reads `readonly` is the settings page having published it.
      expect(seen.at(-1)).toBe("readonly");
    } finally {
      vi.useRealTimers();
    }
  });

  it("publishes a widening, which is the direction that matters most", async () => {
    // `full` is above `auto`, so this goes through the confirmation the page
    // puts in front of every escalation. The pill claiming the narrower tier
    // afterwards is the failure worth having a test for: it says agents are
    // more fenced in than they are.
    vi.useFakeTimers();
    try {
      const { client } = frozenHost("auto", "full");
      const seen: string[] = [];
      await mount(client, seen);

      await act(async () => {
        container.querySelector<HTMLButtonElement>("[data-testid=policy-tier-full]")!.click();
      });
      await act(async () => {
        document
          .querySelector<HTMLButtonElement>("[data-testid=policy-tier-confirm]")!
          .click();
        await Promise.resolve();
      });

      expect(seen.at(-1)).toBe("full");
    } finally {
      vi.useRealTimers();
    }
  });

  it("publishes nothing when the host answers with something that is not a policy", async () => {
    // The fence and the hand-off are the same act: a rejected body is a failed
    // write, and a failed write must leave the row stating the tier actually in
    // force. Publishing it would put an un-rendered shape into the one reader
    // with no error boundary above it.
    vi.useFakeTimers();
    try {
      const put = vi.fn(async () => [] as unknown as PolicyStatus);
      const client = {
        scopeFor: () => "/api/v1/company/acme",
        get: async (path: string) =>
          path.endsWith("/policy") ? policy("auto") : { slugs: [], unwired: [] },
        put,
        del: vi.fn(),
      } as unknown as OpenCompanyClient;
      const seen: string[] = [];
      await mount(client, seen);

      await act(async () => {
        container
          .querySelector<HTMLButtonElement>("[data-testid=policy-tier-readonly]")!
          .click();
        await Promise.resolve();
      });

      expect(put).toHaveBeenCalled();
      expect(seen.at(-1)).toBe("auto");
    } finally {
      vi.useRealTimers();
    }
  });
});

// ---------------------------------------------------------------------------

describe("a company switch", () => {
  /**
   * Records what the hook returned **during render**, before any effect runs.
   *
   * That is the only place the bug was visible: `setPolicy(null)` lives in a
   * passive effect, so by the time an assertion could read the DOM after `act`
   * the clear had already happened and the wrong frame was gone. Pushing from
   * the render body keeps it.
   */
  function Probe({
    api,
    company,
    seen,
  }: {
    api: OpenCompanyClient;
    company: string;
    seen: Array<[string, string | null]>;
  }): ReactNode {
    const status = useAutonomy(api, company);
    seen.push([company, status?.mode ?? null]);
    return null;
  }

  it("answers null for the new company rather than the previous company's tier", async () => {
    // Two companies on one host, on different tiers. `globex` is the one that
    // matters: whatever the console says about it must not have come from
    // `acme`.
    const client = {
      scopeFor: (company: string | null) => `/api/v1/company/${company}`,
      get: async (path: string) =>
        path.startsWith("/api/v1/company/acme") ? policy("full") : policy("readonly"),
      put: vi.fn(),
      del: vi.fn(),
    } as unknown as OpenCompanyClient;

    const seen: Array<[string, string | null]> = [];
    await act(async () => {
      root.render(createElement(Probe, { api: client, company: "acme", seen }));
    });
    expect(seen.at(-1)).toEqual(["acme", "full"]);

    await act(async () => {
      root.render(createElement(Probe, { api: client, company: "globex", seen }));
    });

    const forGlobex = seen.filter(([company]) => company === "globex");
    expect(forGlobex.length).toBeGreaterThan(0);
    // THE assertion: the very first frame drawn for `globex`. `full` here is
    // `acme`'s tier being attributed to a company that is on `readonly` — the
    // pill would have said so in a sentence, beside `globex`'s own name.
    expect(forGlobex[0][1], "the first frame of the new company").toBeNull();
    // And no frame of `globex` ever carried it, not just the first.
    expect(forGlobex.map(([, mode]) => mode)).not.toContain("full");
    // Still resolves, so this is a fence and not a hook that stopped answering.
    expect(seen.at(-1)).toEqual(["globex", "readonly"]);
  });
});
