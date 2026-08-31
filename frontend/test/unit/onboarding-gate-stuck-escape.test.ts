// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ActivationStatus } from "@/api/activation";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type CompanyStatus, type TeamMemberDto } from "@/api/types";
import { AppShell } from "@/components/app-shell";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { HostsProvider, type HostsValue } from "@/connections/HostsContext";
import type { Connection, ConnectionId, LocalScope } from "@/connections/types";
import { useActivationGate } from "@/onboarding/useActivationGate";

/**
 * PR #1875 review finding, round 15.
 *
 * `useActivationGate` never settles `checked` on a non-terminal
 * `getActivation` failure — it retries forever, by design, because a blip
 * must not be mistaken for an answer. `shouldHoldShellPending` holds the
 * shell for as long as `checked` is false, and the pending branch renders
 * only `<RouteLoading>`.
 *
 * Put together, a *durable* backend fault — a malformed event that fails the
 * host's whole-journal activation scan on every read — locked the operator
 * out of the entire console behind a loader that could never resolve. The
 * "skip for now" escape exists only inside `OnboardingGate`, which this
 * branch never mounts, so there was no way forward at all.
 *
 * The fix gives the hook a `stuck` signal after `STUCK_AFTER_FAILURES`
 * consecutive failures and renders a recovery affordance instead of the
 * loader. Polling continues underneath, so a backend that recovers still
 * settles the gate on its own.
 */

const SCOPE: LocalScope = { connection: "test-connection" as ConnectionId, company: null };

const STATUS: CompanyStatus = {
  id: "co",
  name: "Acme",
  lifecycle: "running",
  pending_approvals: 0,
};

/**
 * A `HostsProvider` value for the one test in this file that reaches the
 * ordinary shell (`useHosts` throws outside a provider by design — see its
 * own doc) — mirrors `onboarding-gate-setup-controller-mount.test.ts`'s
 * `HOSTS`.
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

/** Staffed, so `SetupController` closes and `setupChecked` lands true. */
const STAFFED: TeamMemberDto[] = [
  { id: "operations", role: "Analyst", inboxEnabled: false, global: true } as TeamMemberDto,
  { id: "ada", role: "Operations", inboxEnabled: false } as TeamMemberDto,
];

function hang(): Promise<never> {
  return new Promise<never>(() => {});
}

/**
 * `/activation` fails every time with a non-terminal error — the shape
 * `resolveActivationReadError` does NOT settle, so the hook retries rather
 * than answering. `/auth/me` is left hanging: an unresolved admin check does
 * not release the hold, which keeps this test aimed at the activation read
 * alone.
 */
function buildClient(activationCalls: { count: number }): OpenCompanyClient {
  const known = {
    baseUrl: "",
    scopeFor: (company: string | null) => `/api/v1/companies/${company ?? ""}`,
    listTeam: vi.fn(async () => STAFFED),
    subscribeToEvents: () => () => {},
    get: (path: string) => {
      if (path.includes("/activation")) {
        activationCalls.count += 1;
        return Promise.reject(new Error("activation scan failed"));
      }
      return hang();
    },
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

/** A real, incomplete funnel read — used so `activationGate.stuck` never flips. */
const INCOMPLETE_ACTIVATION: ActivationStatus = {
  nameConfirmed: true,
  integrationConnected: false,
  workflowRunSucceeded: false,
  isActivated: false,
};

/**
 * `/activation` succeeds immediately with an incomplete status — so
 * `activationGate.stuck` never flips — and `/auth/me` fails every time with a
 * non-401 `ApiError`, the shape `resolveGateAdminCheckError` does NOT settle.
 * Isolates the admin-check stuck path (`isGateAdminStuck`, PR #1875 review
 * finding, round 16) from the activation-read one `buildClient` above covers:
 * this proves the escape appears even when activation itself never fails
 * once.
 */
function buildAdminCheckStuckClient(meCalls: { count: number }): OpenCompanyClient {
  const known = {
    baseUrl: "",
    scopeFor: (company: string | null) => `/api/v1/companies/${company ?? ""}`,
    listTeam: vi.fn(async () => STAFFED),
    subscribeToEvents: () => () => {},
    get: (path: string) => {
      if (path.includes("/auth/me")) {
        meCalls.count += 1;
        return Promise.reject(new ApiError(502, "bad_gateway", "upstream failure"));
      }
      if (path.includes("/activation")) {
        return Promise.resolve(INCOMPLETE_ACTIVATION);
      }
      return hang();
    },
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

const ACTIVATED: ActivationStatus = {
  nameConfirmed: true,
  integrationConnected: true,
  workflowRunSucceeded: true,
  isActivated: true,
  activationCompletedAtMillis: 1_700_000_000_000,
};

/** Longer than `POLL_MS` (5000ms, `useActivationGate.ts`) — see `buildSlowActivationClient`. */
const SLOW_READ_MS = 6000;

/**
 * `/activation` always *succeeds* — no rejection anywhere — but every call
 * takes `SLOW_READ_MS`, longer than the hook's own poll interval. Isolates
 * the generation/staleness race (PR #1875 review finding) from every test
 * above, which all drive a rejection path: this proves the read itself never
 * failing is not enough to guarantee it ever lands.
 */
function buildSlowActivationClient(activationCalls: { count: number }): OpenCompanyClient {
  const known = {
    baseUrl: "",
    scopeFor: (company: string | null) => `/api/v1/companies/${company ?? ""}`,
    listTeam: vi.fn(async () => STAFFED),
    subscribeToEvents: () => () => {},
    get: (path: string) => {
      if (path.includes("/activation")) {
        activationCalls.count += 1;
        return new Promise<ActivationStatus>((resolve) => {
          setTimeout(() => resolve(ACTIVATED), SLOW_READ_MS);
        });
      }
      return hang();
    },
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

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
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
  vi.useRealTimers();
  act(() => root.unmount());
  container.remove();
});

describe("a durable activation-read failure does not strand the operator", () => {
  it("offers a way into the console once the read keeps failing", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const activationCalls = { count: 0 };
    const client = buildClient(activationCalls);

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

    // One failure is routine: still the neutral loader, no error shown.
    expect(container.textContent).toContain("Loading");
    expect(container.textContent).not.toContain("Continue to the console");

    // Drive the retries. Each non-terminal failure schedules the next attempt
    // at ACTIVATION_READ_RETRY_MS; STUCK_AFTER_FAILURES of them flips `stuck`.
    for (let i = 0; i < 4; i += 1) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(3000);
      });
    }

    expect(activationCalls.count).toBeGreaterThanOrEqual(3);
    // The operator must now have a way forward rather than an endless loader.
    expect(container.textContent).toContain("Continue to the console");
  });
});

/**
 * PR #1875 review finding, round 16.
 *
 * `useActivationGate`'s `stuck` only tracks its own `getActivation` reads.
 * The admin check `AppShell` runs alongside it (`isGateAdmin`, behind
 * `fetchMe`) has the identical shape — it retries a non-401 failure forever
 * and never settles `isGateAdmin` — but round 15's fix gave the recovery
 * affordance no way to know *that* read is the one wedged: a durable
 * `fetchMe` fault leaves `isGateAdmin` at `null` forever while activation
 * itself reads fine the whole time, so `activationGate.stuck` never flips and
 * the operator is locked behind the exact same permanent loader round 15
 * closed for the activation side only.
 *
 * The fix mirrors round 15's shape on the admin-check effect: a
 * `GATE_ADMIN_CHECK_STUCK_AFTER_FAILURES`-failure counter feeding
 * `isGateAdminStuck`, and the same recovery affordance now renders on
 * `activationGate.stuck || isGateAdminStuck`.
 */
describe("a durable admin-check failure does not strand the operator either", () => {
  it("offers a way into the console once /auth/me keeps failing, even though activation itself never fails", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const meCalls = { count: 0 };
    const client = buildAdminCheckStuckClient(meCalls);

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

    // isGateAdmin starts null and the first fetchMe hasn't failed yet: still
    // the neutral loader, no error shown — activation itself reads fine
    // immediately, so nothing here comes from the activation-read path.
    expect(container.textContent).toContain("Loading");
    expect(container.textContent).not.toContain("Continue to the console");

    // Drive the retries. Each non-settled failure schedules the next attempt
    // at GATE_ADMIN_CHECK_RETRY_MS; GATE_ADMIN_CHECK_STUCK_AFTER_FAILURES of
    // them flips `isGateAdminStuck`.
    for (let i = 0; i < 4; i += 1) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(3000);
      });
    }

    expect(meCalls.count).toBeGreaterThanOrEqual(3);
    // The operator must now have a way forward rather than an endless loader,
    // driven entirely by the admin check — activation never failed once.
    expect(container.textContent).toContain("Continue to the console");
  });
});

/**
 * PR #1875 review finding.
 *
 * `useActivationGate`'s `load` bumps `generation` on every call it starts,
 * and discards a response whose captured `gen` no longer matches — the
 * mechanism that lets a *later* read win over a stale one. `startVisiblePolling`
 * is a bare, non-waiting `setInterval`, though: it has no idea whether the
 * `load` it last called has returned. When a read consistently takes longer
 * than `POLL_MS`, every tick starts a new call that bumps `generation` before
 * the previous one can land — so every single response is discarded as
 * stale, forever, and `checked` never settles. Every individual read
 * "succeeds"; the operator is stuck behind the loader regardless, and neither
 * `retrying` nor `stuck` ever fires, because nothing here is an error.
 *
 * The fix adds an in-flight guard: `load` declines to start a new call while
 * one it started is still outstanding, so the interval's ticks in between are
 * skipped rather than racing ahead of it. The first read is then left alone
 * to land.
 *
 * Exercised directly against the hook rather than through `AppShell`
 * (`Probe` below): `useActivationGate` takes no React context — it is a
 * plain `client`/`company`/`enabled` function of its inputs — and the race
 * lives entirely inside it, so mounting the full authenticated shell (which
 * needs a `HostsProvider` this suite's minimal client double does not
 * provide) would only be testing plumbing this bug has nothing to do with.
 */
function Probe({ client, company }: { client: OpenCompanyClient; company: string | null }): ReturnType<
  typeof createElement
> {
  const gate = useActivationGate(client, company, true);
  return createElement(
    "div",
    { "data-testid": "probe" },
    JSON.stringify({ checked: gate.checked, isActivated: gate.status?.isActivated ?? null }),
  );
}

describe("a slow-but-successful activation read is not starved by overlapping polls", () => {
  it("lands once the interval stops racing ahead of the in-flight read", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const activationCalls = { count: 0 };
    const client = buildSlowActivationClient(activationCalls);

    await act(async () => {
      root.render(createElement(Probe, { client, company: null }));
    });

    expect(container.textContent).toContain('"checked":false');

    // Well past several 5s poll ticks and the 6s response latency each read
    // takes — long enough that, without the guard, no response would ever
    // have landed no matter how much further this ran.
    for (let i = 0; i < 8; i += 1) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5000);
      });
    }

    // Exactly one call ever went out: the guard skipped every tick that
    // landed while it was still outstanding, rather than starting a second
    // (and third, and fourth…) that would each bump `generation` and discard
    // the one before it.
    expect(activationCalls.count).toBe(1);
    // That one call was left alone to land, so the hook has activation's
    // answer now instead of starving forever.
    expect(container.textContent).toBe('{"checked":true,"isActivated":true}');
  });
});

/**
 * PR #1875 review finding (comment 3878631085, `app-shell.tsx:957`) — one of
 * three "time out a hung read" threads that land on the same root cause
 * (`lib/read-timeout.ts`'s own doc has the shared explanation).
 *
 * `isGateAdminStuck` (round 16, above) is driven entirely by `fetchMe`
 * *settling* — the `catch` block below is what counts failures. `fetchMe`
 * goes through `OpenCompanyClient`, whose request path had no timeout of its
 * own, so a `fetchMe` that neither resolves nor rejects (a stalled proxy, a
 * backend that accepts the connection and never answers) left `failures` at
 * zero forever: no `catch` ever ran, `isGateAdminStuck` never flipped, no
 * matter how long the hang ran. `hang()` — already used throughout this file
 * for "this test does not care about this endpoint" — is exactly that shape
 * once a real endpoint uses it on purpose.
 *
 * `withReadTimeout` closes it by turning "never settles" into "settles late,
 * as a rejection" at `GATE_ADMIN_CHECK_TIMEOUT_MS` (20s, comfortably above
 * any legitimate read) — round 16's *existing* failure counter, unchanged
 * here, is what actually recovers.
 */
describe("a hung admin-role read does not strand the operator either", () => {
  it("times out a hung admin-role read into the existing isGateAdminStuck escape", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const meCalls = { count: 0 };
    // `/auth/me` hangs forever — never resolves, never rejects. `/activation`
    // answers immediately with an incomplete status, so `activationGate.stuck`
    // never flips: this isolates the admin-check hang from the activation
    // read the same way `buildAdminCheckStuckClient` isolates a rejecting one.
    const client = (() => {
      const known = {
        baseUrl: "",
        scopeFor: (company: string | null) => `/api/v1/companies/${company ?? ""}`,
        listTeam: vi.fn(async () => STAFFED),
        subscribeToEvents: () => () => {},
        get: (path: string) => {
          if (path.includes("/auth/me")) {
            meCalls.count += 1;
            return hang();
          }
          if (path.includes("/activation")) {
            return Promise.resolve(INCOMPLETE_ACTIVATION);
          }
          return hang();
        },
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
    })();

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

    expect(meCalls.count).toBe(1);
    expect(container.textContent).toContain("Loading");
    expect(container.textContent).not.toContain("Continue to the console");

    // Each attempt needs GATE_ADMIN_CHECK_TIMEOUT_MS (20s) to time out, then
    // GATE_ADMIN_CHECK_RETRY_MS (3s) before the next one starts.
    // GATE_ADMIN_CHECK_STUCK_AFTER_FAILURES (3) of those flips the escape —
    // advance well past that, the same margin the slow-read test above uses
    // relative to its own bound.
    for (let i = 0; i < 12; i += 1) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(10000);
      });
    }

    // Without the timeout, exactly one hung call would ever have gone out —
    // the first `fetchMe` await would still be pending right now. Proving a
    // second one started proves the first one settled (as a rejection) and
    // the retry loop resumed, which only `withReadTimeout` makes possible.
    expect(meCalls.count).toBeGreaterThanOrEqual(2);
    expect(container.textContent).toContain("Continue to the console");
  });
});

/**
 * PR #1875 review finding (comment 3878631082, `useActivationGate.ts:155`) —
 * one of three "time out a hung read" threads that land on the same root
 * cause (`lib/read-timeout.ts`'s own doc has the shared explanation).
 *
 * `inFlight` (the in-flight guard fixed just above, round 17) only clears
 * once the call it is guarding *settles* — resolve or reject, either flips
 * it back to `false` in `finally`. A `getActivation` that neither resolves
 * nor rejects left `inFlight` stuck `true` forever: every later poll tick
 * kept getting skipped by the very guard meant to stop them racing ahead of
 * a live read, and `failures`/`stuck` never fired either, since nothing was
 * ever a caught error for them to count.
 *
 * `withReadTimeout` closes it the same way as the admin-check thread:
 * `ACTIVATION_READ_TIMEOUT_MS` (20s — comfortably above `SLOW_READ_MS`
 * above, so a merely slow-but-successful read is never mistaken for a hang)
 * turns the silence into an ordinary rejection, which flows straight into
 * the *existing* `failures` counter and eventually `stuck`, and `inFlight`
 * clears in the same `finally` as any other rejection.
 */
describe("a hung activation read does not strand the operator either", () => {
  it("times out a hung activation read into the existing failures/stuck counter", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const activationCalls = { count: 0 };
    // `/activation` hangs forever. Exercised directly against the hook, the
    // same way the overlapping-polls test above is: the race (and now the
    // timeout that closes its hung-read gap) lives entirely inside
    // `useActivationGate`, not in anything `AppShell` adds on top.
    const client = (() => {
      const known = {
        baseUrl: "",
        scopeFor: (company: string | null) => `/api/v1/companies/${company ?? ""}`,
        listTeam: vi.fn(async () => STAFFED),
        subscribeToEvents: () => () => {},
        get: (path: string) => {
          if (path.includes("/activation")) {
            activationCalls.count += 1;
            return hang();
          }
          return hang();
        },
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
    })();

    function StuckProbe({
      client: c,
      company,
    }: {
      client: OpenCompanyClient;
      company: string | null;
    }): ReturnType<typeof createElement> {
      const gate = useActivationGate(c, company, true);
      return createElement(
        "div",
        { "data-testid": "probe" },
        JSON.stringify({ checked: gate.checked, retrying: gate.retrying, stuck: gate.stuck }),
      );
    }

    await act(async () => {
      root.render(createElement(StuckProbe, { client, company: null }));
    });

    expect(activationCalls.count).toBe(1);
    expect(container.textContent).toBe('{"checked":false,"retrying":false,"stuck":false}');

    // Same shape as the admin-check case: ACTIVATION_READ_TIMEOUT_MS (20s) to
    // time out, ACTIVATION_READ_RETRY_MS (3s) before the next attempt,
    // STUCK_AFTER_FAILURES (3) of those flips `stuck`.
    for (let i = 0; i < 12; i += 1) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(10000);
      });
    }

    // More than one call proves the first hung read settled (late, as a
    // rejection) and freed `inFlight` rather than wedging it forever.
    expect(activationCalls.count).toBeGreaterThanOrEqual(2);
    expect(container.textContent).toBe('{"checked":false,"retrying":true,"stuck":true}');
  });
});

/**
 * PR #1875 review finding (comment 3878631089, `gate-logic.ts:191`) — one of
 * three "time out a hung read" threads that land on the same root cause
 * (`lib/read-timeout.ts`'s own doc has the shared explanation).
 *
 * Unlike the two threads above, `shouldHoldShellPending`'s `!input.setupChecked`
 * branch has no stuck counter on this axis at all — `SetupController`'s own
 * `catch` around `client.listTeam` already treats any rejection as "cannot
 * tell a fresh company from a staffed one, offer nothing" and settles
 * `checked` regardless, so ordinarily there is nothing left to fix here. The
 * one thing that `catch` cannot handle is a promise that never settles at
 * all: a `listTeam` that neither resolves nor rejects left `checked` `false`
 * inside `SetupController` forever, so its `onOpenChange` never fired,
 * `AppShell`'s `setupChecked` never flipped, and the `!input.setupChecked`
 * hold above had nothing else — no escape — to release it.
 *
 * `withReadTimeout` closes it the same way as the other two: `SETUP_ROSTER_
 * TIMEOUT_MS` (20s) turns the silence into an ordinary rejection, which the
 * *existing* `catch` already handles exactly like any other unreachable host.
 *
 * A later review round (round 19, comment 3878766538) added `readRoster`'s
 * own single retry on a `ReadTimeoutError` — see its own doc. A read that
 * hangs forever times out on *both* attempts, so this test now expects two
 * `listTeam` calls and twice the wall-clock wait, not one; the retry itself
 * is proven separately below.
 */
describe("a hung setup-roster read does not strand the operator either", () => {
  it("times out a hung setup-roster read into the existing setupChecked gate, unblocking the shell", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const rosterCalls = { count: 0 };
    // Setup's own roster read (`SetupController`, not the chat-thread
    // `listTeam` call in `app-shell.tsx`) hangs forever. `/auth/me` and
    // `/activation` both answer immediately — an admin, incomplete funnel —
    // so nothing here comes from either of the other two reads: the only
    // thing holding the shell is the roster read that never settles. Activation
    // is left incomplete (not the already-activated fixture the admin-check
    // and activation tests above use) so the release lands on `OnboardingGate`
    // rather than the full authenticated shell — `HostSwitcher`, mounted deep
    // in that shell's sidebar, needs a `HostsProvider` this suite's minimal
    // client double does not provide, the same reason the overlapping-polls
    // test above exercises `useActivationGate` directly rather than through
    // `AppShell`. `OnboardingGate` carries no such dependency, and reaching it
    // is just as much proof the hold released as reaching the ordinary shell
    // would be.
    const client = (() => {
      const known = {
        baseUrl: "",
        scopeFor: (company: string | null) => `/api/v1/companies/${company ?? ""}`,
        listTeam: vi.fn(async () => {
          rosterCalls.count += 1;
          return hang();
        }),
        subscribeToEvents: () => () => {},
        get: (path: string) => {
          if (path.includes("/auth/me")) {
            return Promise.resolve({
              id: "op",
              email: "op@example.com",
              role: "admin",
              company: "co",
            });
          }
          if (path.includes("/activation")) {
            return Promise.resolve(INCOMPLETE_ACTIVATION);
          }
          return hang();
        },
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
    })();

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

    expect(rosterCalls.count).toBe(1);
    // `shouldHoldShellPending`'s `!input.setupChecked` branch holds
    // unconditionally — there is no stuck flag on this axis at all, only
    // `RouteLoading`, forever, without the timeout.
    expect(container.textContent).toContain("Loading");
    expect(container.textContent).not.toContain("Continue to the console");
    expect(container.textContent).not.toContain("Skip for now");

    // `readRoster`'s first attempt hits SETUP_ROSTER_TIMEOUT_MS (20s) and
    // retries once (round 19); the mock hangs on every call, so the retry
    // times out too, at 40s total. Five 10s ticks comfortably clears both.
    for (let i = 0; i < 5; i += 1) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(10000);
      });
    }

    // Two calls now — the first attempt plus the round-19 retry — and both
    // timed out, so the second attempt's rejection is what finally settles
    // (late, as a rejection the existing `catch` already handles as "cannot
    // tell a fresh company from a staffed one, offer nothing"), which is
    // enough to flip `setupChecked` and release the hold. With admin true and
    // the funnel still incomplete, `shouldShowOnboardingGate` now has
    // everything it needs and renders the gate — including its own
    // always-available "Skip for now" escape — instead of a loader that never
    // resolves.
    expect(rosterCalls.count).toBe(2);
    expect(container.textContent).toContain("Skip for now");
  });
});

/**
 * PR #1875 review finding, round 19 (comment 3878766538, `SetupController.
 * tsx:223`).
 *
 * The round-18 fix above closes a `listTeam` read that never settles at all,
 * but `withReadTimeout` cannot distinguish that from a read that is merely
 * slower than `SETUP_ROSTER_TIMEOUT_MS` and would have answered correctly a
 * moment later. Before `readRoster`'s retry, that slow-but-healthy read fell
 * into the same `catch` as a truly unreachable host, reported "cannot tell a
 * fresh company from a staffed one, offer nothing", and silently discarded
 * the late, correct answer once it did arrive (`withReadTimeout`'s own doc:
 * a promise settles once). For a genuinely unstaffed admin, that meant
 * `unstaffed` stayed at its default `false`, `SetupController` never opened
 * its dialog, and — because `shouldShowOnboardingGate` only holds off on
 * `setupOpen` — the shell went straight to the activation gate instead of
 * offering setup, with no remount to correct it.
 *
 * This proves the fix: a `listTeam` that hangs past `SETUP_ROSTER_TIMEOUT_MS`
 * on its first call but answers quickly on the retry lands on the *setup*
 * dialog with the correct, empty roster — not the activation gate reporting
 * a false "staffed".
 */
describe("a setup-roster read that only looked hung gets a second chance", () => {
  it("honors a late, correct roster answer instead of discarding it as unreachable", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const rosterCalls = { count: 0 };
    const client = (() => {
      const known = {
        baseUrl: "",
        scopeFor: (company: string | null) => `/api/v1/companies/${company ?? ""}`,
        listTeam: vi.fn(async () => {
          rosterCalls.count += 1;
          // First attempt hangs past SETUP_ROSTER_TIMEOUT_MS — genuinely
          // slow, not unreachable. `readRoster`'s retry (round 19) is the
          // second call, and it answers promptly with the true, empty
          // roster: the company really is unstaffed.
          if (rosterCalls.count === 1) return hang();
          return new Promise<TeamMemberDto[]>((resolve) => {
            setTimeout(() => resolve([]), 500);
          });
        }),
        subscribeToEvents: () => () => {},
        get: (path: string) => {
          if (path.includes("/auth/me")) {
            return Promise.resolve({
              id: "op",
              email: "op@example.com",
              role: "admin",
              company: "co",
            });
          }
          if (path.includes("/activation")) {
            return Promise.resolve(INCOMPLETE_ACTIVATION);
          }
          return hang();
        },
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
    })();

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

    expect(rosterCalls.count).toBe(1);
    expect(container.textContent).toContain("Loading");

    // SETUP_ROSTER_TIMEOUT_MS (20s): the first attempt times out and
    // `readRoster` immediately starts the retry.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(20000);
    });
    expect(rosterCalls.count).toBe(2);

    // The retry's own 500ms delay before it resolves.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(500);
    });

    // The late answer was honored, not discarded: the roster really is
    // empty, so `SetupController` opens its own dialog (`data-testid=
    // "setup-dialog"`, `SetupDialog.tsx`) rather than the activation gate
    // reporting a false "staffed" and asking the operator to run a workflow
    // before there is a team to have written one. `SetupDialog`'s own
    // inference check (an unrelated read this client double leaves hanging)
    // keeps it on its first internal screen, so this asserts the dialog
    // mounted at all rather than which question it reached.
    expect(document.querySelector('[data-testid="setup-dialog"]')).not.toBeNull();
    expect(document.body.textContent).not.toContain("Skip for now");
  });
});
