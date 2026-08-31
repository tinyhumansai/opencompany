// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { ActivationStatus } from "@/api/activation";
import type { CompanyStatus, TeamMemberDto } from "@/api/types";
import { AppShell } from "@/components/app-shell";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { HostsProvider, type HostsValue } from "@/connections/HostsContext";
import type { Connection, ConnectionId, LocalScope } from "@/connections/types";

/**
 * PR #1875 review finding, round 13.
 *
 * `shouldHoldShellPending` (`@/onboarding/gate-logic`) holds `AppShell` in a
 * neutral pending state — `<RouteLoading>` — for as long as `setupChecked` is
 * `false`, and `SetupController`'s own `onOpenChange` is the *only* thing
 * that ever sets it (see that state's own doc in `app-shell.tsx`). Before
 * this fix, the JSX that mounted `<SetupController>` lived below both of
 * `AppShell`'s early returns — reachable only once the ordinary shell itself
 * was chosen. Every fresh mount starts `setupChecked === false`, so the very
 * predicate this component exists to satisfy made `SetupController`
 * unreachable: the hold fired, the function returned before that JSX,
 * `SetupController` never mounted, `onOpenChange` never fired, and
 * `setupChecked` stayed `false` forever — a permanent loader for every
 * signed-in operator who was not a confirmed non-admin or an already-skipped
 * session.
 *
 * This is not testable as a pure `shouldHoldShellPending`/`shouldShowOnboarding
 * Gate` unit case (`onboarding-gate-logic.test.ts`) — the predicates
 * themselves are correct; the defect is in which JSX branch `AppShell`
 * mounted `<SetupController>` under. Proving the fix requires actually
 * rendering `AppShell` and observing whether `SetupController`'s own roster
 * read (`client.listTeam`) runs while the shell is held pending.
 */

const SCOPE: LocalScope = { connection: "test-connection" as ConnectionId, company: null };

const STATUS: CompanyStatus = {
  id: "co",
  name: "Acme",
  lifecycle: "running",
  pending_approvals: 0,
};

/** A staffed roster — baseline plus one real teammate (`teamIsUnstaffed`'s own contract). */
const STAFFED: TeamMemberDto[] = [
  { id: "operations", role: "Analyst", inboxEnabled: false, global: true } as TeamMemberDto,
  { id: "ada", role: "Operations", inboxEnabled: false } as TeamMemberDto,
];

/** A promise that never settles — every mount-time read this test is not exercising. */
function hang(): Promise<never> {
  return new Promise<never>(() => {});
}

/**
 * A minimal `OpenCompanyClient` double.
 *
 * `listDesks` is deliberately left hanging: `AppShell`'s own thread-hydration
 * effect only calls `client.listTeam` inside `listDesks(...).then(...)`, so
 * hanging `listDesks` suppresses that unrelated call and leaves
 * `SetupController`'s own `listTeam` read as the sole caller — the signal
 * this test actually needs. `/auth/me` and `/activation` both route through
 * `get`, which hangs by default, so `shouldHoldShellPending` stays true (the
 * pending branch) — the same state a fresh mount starts every session in; the
 * round-14 case below overrides `get` to answer them and walk the shell out of
 * that branch on purpose. Anything this large a component reaches for that is
 * not named here becomes a permanently-pending no-op via the `Proxy` below
 * rather than a hard crash; this test only cares about `SetupController`'s own
 * read.
 */
function buildClient(
  listTeam: ReturnType<typeof vi.fn>,
  get: (path: string) => Promise<unknown> = hang,
): OpenCompanyClient {
  const known = {
    baseUrl: "",
    scopeFor: (company: string | null) => `/api/v1/companies/${company ?? ""}`,
    listTeam,
    subscribeToEvents: () => () => {},
    get,
    status: hang,
    approvals: hang,
    listDesks: hang,
  };
  return new Proxy(known, {
    get(target, prop, receiver) {
      if (prop in target) return Reflect.get(target, prop, receiver);
      return hang;
    },
  }) as unknown as OpenCompanyClient;
}

/**
 * The one host this console is connected to.
 *
 * Only the round-14 case below needs this: reaching the ordinary shell draws
 * the sidebar's `HostSwitcher`, and `useHosts` throws outside a provider by
 * design (see its own doc). The pending and gate branches never render it.
 */
const CONNECTION: Connection = {
  id: SCOPE.connection,
  defaultCompany: null,
  label: "test",
  baseUrl: "",
  credential: { kind: "cookie" },
  status: "live",
  identity: null,
  companies: [],
  connector: { kind: "remote" },
};

const HOSTS: HostsValue = {
  connections: [CONNECTION],
  selected: SCOPE.connection,
  onSelect: () => {},
  onAdd: () => {},
  localInstances: [],
  onEditHost: () => {},
  onRemoveHost: () => {},
  hub: false,
};

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  // jsdom implements no media queries at all. `useIsMobile` calls
  // `window.matchMedia` on mount, and `SidebarProvider` — which only the
  // ordinary shell renders — uses it, so the round-14 case below reaches it.
  // Always desktop: the breakpoint is not what any test here is about.
  window.matchMedia = ((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
  window.location.hash = "#/overview";
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("AppShell holds pending without stranding SetupController", () => {
  it("runs SetupController's own roster read while the shell is held pending", async () => {
    const listTeam = vi.fn(async () => STAFFED);
    const client = buildClient(listTeam);

    await act(async () => {
      root.render(
        createElement(ConnectionScopeProvider, {
          scope: SCOPE,
          children: createElement(AppShell, {
            client,
            company: null,
            initialStatus: STATUS,
            companies: [STATUS],
            onSwitchCompany: () => {},
          }),
        }),
      );
    });

    // `/auth/me` and `/activation` are left hanging above, so this render
    // must still be in the neutral pending state — confirm that directly so
    // this test fails loudly (not silently) if some unrelated change moved
    // the render to a different branch before the real assertion below.
    expect(container.textContent).toContain("Loading");

    // The bug this guards: `SetupController` mounting (and completing its own
    // roster read) has nothing to do with the still-unresolved activation and
    // admin reads. Before the fix, `SetupController` was nested in JSX only
    // the fully-resolved shell reached, so `listTeam` here was never called.
    expect(listTeam).toHaveBeenCalled();
  });
});

/**
 * PR #1875 review finding, round 14.
 *
 * Round 13 (above) put `<SetupController>` in every one of `AppShell`'s three
 * render outcomes. That is necessary but not sufficient: React reconciles by
 * *position*, so a controller rendered under a different root in each branch is
 * a different node in each, and the transition between them unmounts it.
 *
 * The transition is not hypothetical — it is the ordinary first-run sequence.
 * The roster read that settles `setupChecked` is the same read that settles
 * `setupOpen`, so the moment it lands `shouldHoldShellPending` stops holding
 * and this render leaves the pending branch. Before the fix the ordinary shell
 * mounted the controller deep inside `ConsoleProvider > SidebarProvider`, so
 * that hand-off tore down the very component whose answer caused it: the
 * proven `unstaffed`/`open` state was discarded, a second `listTeam` went out,
 * the interactive shell was exposed for its whole flight, and a hang or failure
 * on that second read left the setup dialog shut for good.
 *
 * `listTeam` call count across the transition is the observable form of that:
 * a controller that stayed mounted does not read the roster twice.
 */
describe("AppShell keeps SetupController mounted across its branch transitions", () => {
  it("does not re-read the roster when the shell leaves pending for the ordinary shell", async () => {
    const listTeam = vi.fn(async () => STAFFED);

    // `/activation` is deferred so this render starts in the pending branch and
    // is walked out of it deliberately, mid-test — the real sequence, rather
    // than a shell that was never held in the first place.
    let landActivation!: (status: ActivationStatus) => void;
    const activation = new Promise<ActivationStatus>((resolve) => {
      landActivation = resolve;
    });
    const client = buildClient(listTeam, (path: string) => {
      // An admin: `shouldHoldShellPending` must not take its `isAdmin === false`
      // short circuit, so the hold is the activation read's alone.
      if (path.endsWith("/auth/me")) return Promise.resolve({ role: "admin" });
      if (path.endsWith("/activation")) return activation;
      return hang();
    });

    await act(async () => {
      root.render(
        createElement(HostsProvider, {
          value: HOSTS,
          children: createElement(ConnectionScopeProvider, {
            scope: SCOPE,
            children: createElement(AppShell, {
              client,
              company: null,
              initialStatus: STATUS,
              companies: [STATUS],
              onSwitchCompany: () => {},
            }),
          }),
        }),
      );
    });

    // Precondition: held pending on the deferred activation read, with the
    // controller already mounted and its roster read done (round 13's fix).
    expect(container.textContent).toContain("Loading");
    expect(container.querySelector("#main-content")).toBeNull();
    const readsWhilePending = listTeam.mock.calls.length;
    expect(readsWhilePending).toBeGreaterThan(0);

    // Land it activated: no gate, so the shell hands off straight from the
    // pending branch to the ordinary one — the transition under test.
    await act(async () => {
      landActivation({
        nameConfirmed: true,
        integrationConnected: true,
        workflowRunSucceeded: true,
        isActivated: true,
      });
    });

    // The hand-off actually happened (this assertion is what keeps the one
    // below from passing vacuously on a shell that never left pending).
    expect(container.querySelector("#main-content")).not.toBeNull();

    // The fix: same position in both branches, so the controller was never
    // torn down and never re-read the roster.
    //
    // The ordinary branch paints `#/overview`, which is the company graph, and
    // the graph's snapshot takes one roster read of its own on mount. That read
    // belongs to the graph, not to a re-mounted controller, so it is accounted
    // for exactly rather than folded into a `>=` that would let a genuine
    // re-read through unnoticed.
    const GRAPH_ROSTER_READS = 1;
    expect(listTeam.mock.calls.length).toBe(readsWhilePending + GRAPH_ROSTER_READS);
  });
});
