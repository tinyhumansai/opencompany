// @vitest-environment jsdom

import { act, createElement, StrictMode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { RunSummary } from "@/api/runs";
import type { DeskDto } from "@/api/types";
import type { CompanyFeed } from "@/hooks/use-company";

let container: HTMLDivElement;
let root: Root;

/**
 * The view, re-imported per test because a module registry is a page load.
 *
 * The since-visit boundary settles once per page load (issue #1700), and it is
 * module state that remembers that. A statically imported view would carry one
 * settled boundary across the whole file, so the first test's empty
 * `localStorage` would decide the answer for every test that stages a stored
 * visit afterwards.
 */
let OperatorOverview: typeof import("@/views/OperatorOverview")["OperatorOverview"];

const scope = { connection: "test-host", company: "acme" };
const readyFeed = { approvals: [], queue: "ready" as const };

function run(over: Partial<RunSummary> = {}): RunSummary {
  return {
    id: "run-1",
    taskId: "task-1",
    agentId: "maya",
    attempt: 1,
    status: "failed",
    phase: "terminal",
    createdAtMillis: 1_700_000_000_000,
    finishedAtMillis: 1_700_000_000_100,
    usage: { input: 0, output: 0, cachedInput: 0, costUsd: 0 },
    stepCount: 0,
    stepCountCapped: false,
    ...over,
  };
}

function client(
  runs: Promise<RunSummary[]>,
  desks?: Promise<DeskDto[]>,
): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/company/acme",
    get: () => runs,
    // The desks read is best-effort and may be absent entirely — a mock that
    // does not implement `listDesks` exercises the degraded DM default.
    ...(desks ? { listDesks: () => desks } : {}),
  } as unknown as OpenCompanyClient;
}

/** A client that answers the two run reads this page makes differently. */
function clientByUrl(
  answer: (url: string) => Promise<RunSummary[]>,
): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/company/acme",
    // The `as unknown` cast above drops the contextual type for the object
    // literal's methods, so the parameter would otherwise be implicitly `any`.
    get: (url: string) => answer(url),
  } as unknown as OpenCompanyClient;
}

async function render(
  host: OpenCompanyClient,
  feed: Pick<CompanyFeed, "approvals" | "queue">,
  attemptEventTick?: number,
) {
  await act(async () => {
    root.render(
      createElement(OperatorOverview, {
        client: host,
        company: "acme",
        feed,
        scope,
        ...(attemptEventTick === undefined ? {} : { attemptEventTick }),
      }),
    );
  });
}

async function settle() {
  await act(async () => {
    await Promise.resolve();
  });
}

beforeEach(async () => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  window.localStorage.clear();
  vi.resetModules();
  ({ OperatorOverview } = await import("@/views/OperatorOverview"));
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the operator overview landing page (#1321)", () => {
  it("has one primary action and routes attention to the real queues", async () => {
    await render(client(Promise.resolve([])), {
      approvals: [{ id: "approval-1" }] as CompanyFeed["approvals"],
      queue: "ready",
    });
    await settle();

    expect(container.querySelector('[href="#/chat"]')?.textContent).toContain("Start a conversation");
    expect(container.querySelector('[href="#/approvals"]')?.textContent).toContain("Review approvals");
    expect(container.querySelector('[href="#/company/graph"]')?.textContent).toContain("knowledge graph");
    expect(container.textContent).toContain("No work is paused or failed right now.");
  });

  it("keeps loading and unreadable queue states distinct from an empty queue", async () => {
    let resolveRuns: (runs: RunSummary[]) => void;
    const pending = new Promise<RunSummary[]>((resolve) => {
      resolveRuns = resolve;
    });
    await render(client(pending), { approvals: [], queue: "loading" });

    expect(container.textContent).toContain("Loading approvals…");
    expect(container.textContent).toContain("Loading recent work…");

    await act(async () => resolveRuns!([]));
    await render(client(Promise.reject(new Error("offline"))), { approvals: [], queue: "error" });
    await settle();

    expect(container.querySelector('[role="alert"]')?.textContent).toContain("Couldn't read what needs your approval");
    expect(container.textContent).not.toContain("Nothing is waiting for your approval.");
  });

  it("uses the persisted browser boundary to show failed work since the prior visit", async () => {
    window.localStorage.setItem("oc.overview.last-visit:test-host::acme", "1700000000000");
    await render(client(Promise.resolve([run()])), readyFeed);
    await settle();

    expect(container.textContent).toContain("Failed attempts recorded after the previous visit.");
    expect(container.querySelector('[href="#/tasks/task-1?run=run-1"]')?.textContent).toContain("Open");
  });

  it("keeps the previous-visit boundary intact across StrictMode's mount replay (#1745)", async () => {
    // StrictMode double-invokes mount effects (setup → cleanup → setup) in
    // development. A `[scope]` *read* effect paired with a `[scope]` *write*
    // effect looks idempotent but is not: the replay's read effect would
    // observe the timestamp the first pass's write effect had just recorded,
    // silently clobbering the true previous-visit boundary with "now" and
    // hiding this failure on every dev mount.
    window.localStorage.setItem("oc.overview.last-visit:test-host::acme", "1700000000000");
    await act(async () => {
      root.render(
        createElement(
          StrictMode,
          null,
          createElement(OperatorOverview, {
            // Answer only the failed-only page so the run is unambiguously a
            // "since you last opened" hit, not also a "stopped work" hit —
            // the header text ("Failed attempts recorded after the previous
            // visit.") is a static description shown whenever a boundary
            // exists at all, so it cannot prove the boundary is the *right*
            // one; the run's presence can.
            client: clientByUrl((url) =>
              url.includes("status=failed%2Cpaused") ? Promise.resolve([]) : Promise.resolve([run()]),
            ),
            company: "acme",
            feed: readyFeed,
            scope,
          }),
        ),
      );
    });
    await settle();

    // A clobbered boundary ("now") would push this run's finish time before
    // it, so it silently drops out of the since-visit panel.
    expect(container.querySelector('[href="#/tasks/task-1?run=run-1"]')).not.toBeNull();
  });

  it("uses the new scope's own boundary immediately on a scope switch, never the old scope's", async () => {
    const scopeA = { connection: "host-a", company: "acme" };
    const scopeB = { connection: "host-b", company: "acme" };
    window.localStorage.setItem("oc.overview.last-visit:host-a::acme", "1700000000000");
    window.localStorage.setItem("oc.overview.last-visit:host-b::acme", "1800000000000");
    // Finished after A's boundary but before B's — only a stale read of A's
    // boundary after switching to B would surface it as "since your visit".
    const failedAfterAOnly = run({ finishedAtMillis: 1_750_000_000_000 });

    // Mount directly on scope A rather than the module-level `scope` used by
    // the `render()` helper — this test cares about the transition between
    // two specific scopes.
    await act(async () => {
      root.render(
        createElement(OperatorOverview, {
          client: client(Promise.resolve([])),
          company: "acme",
          feed: readyFeed,
          scope: scopeA,
        }),
      );
    });
    await settle();

    await act(async () => {
      root.render(
        createElement(OperatorOverview, {
          // Answer only the failed-only page so the run appears solely in the
          // "since you last opened" panel, not the unrelated stopped-work
          // panel above it — isolates the assertion to the boundary this
          // test is about.
          client: clientByUrl((url) =>
            url.includes("status=failed%2Cpaused") ? Promise.resolve([]) : Promise.resolve([failedAfterAOnly]),
          ),
          company: "acme",
          feed: readyFeed,
          scope: scopeB,
        }),
      );
    });
    await settle();

    // A stale read of A's boundary would surface this failure under "since
    // your visit"; B's own (later) boundary must instead read it as absent.
    expect(container.textContent).toContain("No failed attempts were recorded since the previous visit.");
    expect(container.querySelector('[href="#/tasks/task-1?run=run-1"]')).toBeNull();
  });

  it("reads failures on their own page, so paused attempts cannot crowd one out of the since-visit answer", async () => {
    window.localStorage.setItem("oc.overview.last-visit:test-host::acme", "1700000000000");
    const paused = run({
      id: "paused-1",
      status: "paused",
      phase: "parked",
      finishedAtMillis: 1_700_000_000_200,
    });
    const failed = run({
      id: "failed-1",
      finishedAtMillis: 1_700_000_000_100,
    });
    await render(
      clientByUrl((url) =>
        // The stopped panel's capped mixed page is all-paused — the failure
        // finished after the visit but is older than the paused pack, so it
        // would fall off that page. The since-visit panel reads its own
        // failed-only page, so it still sees the attempt.
        // `URLSearchParams` percent-encodes the comma, so the stopped page's
        // `status=failed%2Cpaused` is what actually hits the wire.
        url.includes("status=failed%2Cpaused")
          ? Promise.resolve([paused])
          : Promise.resolve([failed]),
      ),
      readyFeed,
    );
    await settle();

    expect(container.textContent).toContain("Failed attempts recorded after the previous visit.");
    expect(container.querySelector('[href="#/tasks/task-1?run=failed-1"]')).not.toBeNull();
  });

  it("re-reads the run panels when the shell reports a run status change", async () => {
    let calls = 0;
    const host: OpenCompanyClient = {
      scopeFor: () => "/api/v1/company/acme",
      get: () => {
        calls += 1;
        return Promise.resolve([]);
      },
    } as unknown as OpenCompanyClient;

    await render(host, readyFeed, 0);
    await settle();
    const afterBoot = calls;
    expect(afterBoot).toBeGreaterThan(0);

    await render(host, readyFeed, 1);
    await settle();
    expect(calls).toBeGreaterThan(afterBoot);
  });

  it("does not let a slower initial snapshot overwrite a fresher tick re-read", async () => {
    // The tick refresh added in #1015 races the initial load when a run parks
    // or fails while the first snapshot is still outstanding. The generation
    // ticket must make the *latest* read win even when the initial answer
    // lands last — otherwise the fresher lists get overwritten by stale ones.
    window.localStorage.setItem("oc.overview.last-visit:test-host::acme", "1700000000000");
    const tickFailed = run({ id: "tick-failed", taskId: "tick-failed", finishedAtMillis: 1_700_000_000_100 });
    const initialFailed = run({ id: "initial-failed", taskId: "initial-failed", finishedAtMillis: 1_700_000_000_100 });
    const stoppedResolvers: Array<(runs: RunSummary[]) => void> = [];
    const failedResolvers: Array<(runs: RunSummary[]) => void> = [];
    const host: OpenCompanyClient = {
      scopeFor: () => "/api/v1/company/acme",
      get: (url: string) =>
        new Promise<RunSummary[]>((resolve) => {
          const u = String(url);
          (u.includes("status=failed%2Cpaused") ? stoppedResolvers : failedResolvers).push(resolve);
        }),
    } as unknown as OpenCompanyClient;

    await render(host, readyFeed, 0);
    await settle();
    expect(stoppedResolvers).toHaveLength(1);
    expect(failedResolvers).toHaveLength(1);

    // A run status change lands while the initial snapshot is outstanding.
    await render(host, readyFeed, 1);
    await settle();
    expect(stoppedResolvers).toHaveLength(2);
    expect(failedResolvers).toHaveLength(2);

    // The tick re-read answers first with a fresh failure…
    await act(async () => {
      failedResolvers[1]!([tickFailed]);
      stoppedResolvers[1]!([]);
    });
    expect(container.textContent).toContain("Task tick-failed");

    // …then the stale initial snapshot lands; it must not overwrite it.
    await act(async () => {
      failedResolvers[0]!([initialFailed]);
      stoppedResolvers[0]!([]);
    });
    expect(container.textContent).toContain("Task tick-failed");
    expect(container.textContent).not.toContain("Task initial-failed");
  });

  it("does not claim no failures since the visit when the failed read came back capped", async () => {
    // The host clamps the run list read, so a full page of failures cannot
    // prove the absence of older ones that finished after the visit. The
    // empty state must say it is looking at the newest cap, not claim the
    // whole history was read.
    window.localStorage.setItem("oc.overview.last-visit:test-host::acme", "1700000000000");
    const capped = Array.from({ length: 200 }, (_, i) =>
      run({ id: `old-${i}`, finishedAtMillis: 1_600_000_000_000 }),
    );
    await render(client(Promise.resolve(capped)), readyFeed);
    await settle();

    expect(container.textContent).toContain("the host caps the read here");
    expect(container.textContent).not.toContain("No failed attempts were recorded since the previous visit.");
  });

  it("links an operator-chat run to its desk channel when the chat id names a desk", async () => {
    const chatRun = run({
      id: "chat-desk-1",
      taskId: undefined,
      chatId: "engineering",
      agentId: "engineering",
    });
    await render(
      client(
        Promise.resolve([chatRun]),
        Promise.resolve([{ id: "engineering", name: "Engineering desk", members: [] }]),
      ),
      readyFeed,
    );
    await settle();

    // A desk's channel id is its thread id, so the run links by bare id.
    expect(container.querySelector('[href="#/chat/engineering"]')?.textContent).toContain("Open");
    expect(container.textContent).toContain("Conversation work");
  });

  it("links an operator-chat run to its DM when the chat id is not a known desk", async () => {
    const chatRun = run({
      id: "chat-dm-1",
      taskId: undefined,
      chatId: "ada-1f3k",
      agentId: "maya",
    });
    await render(client(Promise.resolve([chatRun]), Promise.resolve([])), readyFeed);
    await settle();

    expect(container.querySelector('[href="#/chat/dm:ada-1f3k"]')?.textContent).toContain("Open");
  });

  it("keeps the since-visit boundary across a navigate-away-and-back (#1700)", async () => {
    // The shell mounts this view conditionally, so a trip to Chat and back is
    // an unmount and a remount inside one page load. The boundary must not
    // advance across it: when it did, the second open compared against the
    // first open — milliseconds earlier — and the panel answered "No failed
    // attempts were recorded since the previous visit" over a failure the
    // operator had never seen.
    window.localStorage.setItem("oc.overview.last-visit:test-host::acme", "1700000000000");
    const failed = run({ id: "failed-1", taskId: "failed-1", finishedAtMillis: 1_700_000_000_100 });

    await render(client(Promise.resolve([failed])), readyFeed);
    await settle();
    expect(container.querySelector('[href="#/tasks/failed-1?run=failed-1"]')).not.toBeNull();

    // …away to another view, and back. Same page load, so the same modules.
    act(() => root.unmount());
    container.remove();
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    await render(client(Promise.resolve([failed])), readyFeed);
    await settle();

    expect(container.textContent).toContain("Failed attempts recorded after the previous visit.");
    expect(container.textContent).not.toContain("No failed attempts were recorded since the previous visit.");
    expect(container.querySelector('[href="#/tasks/failed-1?run=failed-1"]')).not.toBeNull();
  });

  it("records the visit only once the mount commits (#1700)", async () => {
    // Review of PR #1752: the durable write moved out of the render
    // initializer and into an effect, so a render React discards cannot become
    // the boundary the next page load hides failures behind. This is the other
    // side of that — a mount that DOES commit still has to record itself, or
    // the next page load would have no boundary at all.
    expect(window.localStorage.getItem("oc.overview.last-visit:test-host::acme")).toBeNull();

    await render(client(Promise.resolve([])), readyFeed);
    await settle();

    const recorded = window.localStorage.getItem("oc.overview.last-visit:test-host::acme");
    expect(recorded).not.toBeNull();
    expect(Number(recorded)).toBeGreaterThan(0);

    // …and exactly once. A remount is a second mount, not a second visit, and a
    // second write here would leave the NEXT page load comparing against a
    // moment ago — #1700 again, one page load later. This is the assertion that
    // catches a `writeOverviewVisit(scope, Date.now())` effect finding its way
    // back in: the module map keeps THIS load's boundary right whatever the
    // stored value does, so nothing else in this file would notice.
    act(() => root.unmount());
    container.remove();
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await render(client(Promise.resolve([])), readyFeed);
    await settle();

    expect(window.localStorage.getItem("oc.overview.last-visit:test-host::acme")).toBe(recorded);
  });

  it("re-reads the boundary when the company changes under a mounted view (#1700)", async () => {
    // `scope` is a connection plus a company, and switching company changes it
    // while this component stays mounted. The panel would otherwise keep
    // comparing against the previous company's boundary.
    window.localStorage.setItem("oc.overview.last-visit:test-host::acme", "1700000000000");
    const failed = run({ id: "failed-1", taskId: "failed-1", finishedAtMillis: 1_700_000_000_100 });
    const host = client(Promise.resolve([failed]));

    await render(host, readyFeed);
    await settle();
    expect(container.textContent).toContain("Failed attempts recorded after the previous visit.");

    // Globex has never been opened in this browser, so it has no earlier visit
    // to compare against — and must say so rather than reusing Acme's.
    await act(async () => {
      root.render(
        createElement(OperatorOverview, {
          client: host,
          company: "globex",
          feed: readyFeed,
          scope: { connection: "test-host", company: "globex" },
        }),
      );
    });
    await settle();

    expect(container.textContent).toContain("There is no earlier visit in this browser to compare yet.");
  });

  it("keeps the alert icon only for attempts with neither a task nor a conversation", async () => {
    const stray = run({
      id: "stray-1",
      taskId: undefined,
      chatId: undefined,
    });
    await render(client(Promise.resolve([stray])), readyFeed);
    await settle();

    expect(container.textContent).toContain("Unattributed attempt");
    expect(
      container.querySelector('[aria-label="No task or conversation is attached to this attempt"]'),
    ).not.toBeNull();
    // The header's own `#/chat` CTA is excluded by requiring a segment after
    // the slash — an unattributed run must mint no task or thread link of its
    // own.
    expect(container.querySelector('a[href^="#/tasks/"], a[href^="#/chat/"]')).toBeNull();
  });
});
