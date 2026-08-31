// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { RunArtifactRow, WorkflowRunOutcome } from "@/api/workflows";
import { artifactHref } from "@/lib/task-output";
import { RunHistoryPanel } from "@/views/workflows/RunHistoryPanel";

/**
 * The "Files associated" disclosure on a run-history row (issue #1684).
 *
 * A completed run's row now links to the files that run produced, deep-linking
 * each into the card's Artifacts tab at the version the run wrote. The section
 * is the exception the unit runner earns the same way `workflow-run-board` does:
 * the thing under test IS what reaches the operator's eye — a lazy fetch that
 * must NOT fire until the row is expanded, and a link whose href is the exact
 * `artifactHref` the rest of the console navigates by — and neither can be
 * asserted anywhere but a render.
 */

let container: HTMLDivElement;
let root: Root;

/** A fake client that answers a canned files response and records every path
 * asked for — so a test can prove the fetch is lazy by asserting the sink is
 * empty until the row is expanded. */
function filesClient(
  rows: RunArtifactRow[],
  sink: { calls: string[] },
  truncated = false,
): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async <T>(path: string): Promise<T> => {
      sink.calls.push(path);
      return { files: rows, truncated } as T;
    },
  } as unknown as OpenCompanyClient;
}

/** A client whose `get` answers nothing until the test settles it, in order —
 * so a test can arrange the exact interleaving of two requests and prove the
 * stale one's late settle is dropped (issue #1693). */
function deferredFilesClient(deferred: {
  resolve: (rows: RunArtifactRow[]) => void;
  reject: (err: unknown) => void;
}[]): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async <T>(path: string): Promise<T> => {
      return new Promise<T>((resolve, reject) => {
        deferred.push({
          resolve: (rows) => {
            resolve({ files: rows, truncated: false } as T);
          },
          reject,
        });
        void path;
      });
    },
  } as unknown as OpenCompanyClient;
}

/** A client that fails every request while `modes.fail` is true and answers
 * the canned files response after it flips. The failure is an already-rejected
 * promise, not a manually-rejected deferred — see the retry test below for why
 * that shape is the one the unit runner commits reliably. */
function modeFilesClient(
  rows: RunArtifactRow[],
  sink: { calls: string[] },
  modes: { fail: boolean },
): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async <T>(path: string): Promise<T> => {
      sink.calls.push(path);
      if (modes.fail) return Promise.reject(new Error("boom")) as never;
      return { files: rows, truncated: false } as T;
    },
  } as unknown as OpenCompanyClient;
}

/** Re-renders the same root with a different company/runId but the same
 * `run.seq` — the exact way `WorkflowsView` reuses a row on a company switch —
 * passing through the client so the deferred request queue keeps accumulating. */
async function swapScope(
  client: OpenCompanyClient,
  company: string,
  runId: string,
): Promise<void> {
  await act(async () => {
    root.render(
      createElement(RunHistoryPanel, {
        client,
        company,
        runs: [completedRun(runId)],
        graph: null,
        workflowName: "Launch",
        onClose: () => {},
        selectedRunSeq: null,
        onSelectRun: () => {},
      }),
    );
  });
}

/** A completed, quiet run — the compact row the files section hangs off. */
function completedRun(runId: string | undefined): WorkflowRunOutcome {
  return {
    seq: 1,
    atMillis: 1_000,
    workflowId: "launch",
    scheduled: false,
    runId,
    deliveries: [],
    pendingApprovals: [],
  };
}

const FILE: RunArtifactRow = {
  taskId: "t-a",
  artifactId: "art-a1",
  title: "Launch spec",
  kind: "markdown",
  source: "specs/launch.md",
  latestVersion: 2,
  updatedAtMillis: 30,
  taskTitle: "Draft the launch",
};

async function renderPanel(
  run: WorkflowRunOutcome,
  client: OpenCompanyClient,
): Promise<void> {
  await act(async () => {
    root.render(
      createElement(RunHistoryPanel, {
        client,
        company: "acme",
        runs: [run],
        graph: null,
        workflowName: "Launch",
        onClose: () => {},
        selectedRunSeq: null,
        onSelectRun: () => {},
      }),
    );
  });
}

/** Opens the native `<details>` the way a click would, and flushes the fetch
 * that fires on first open. */
async function expandFiles(): Promise<void> {
  const details = container.querySelector<HTMLDetailsElement>(
    '[data-testid="workflow-run-files"]',
  );
  if (!details) throw new Error("no files disclosure on the row");
  await act(async () => {
    details.open = true;
    details.dispatchEvent(new Event("toggle", { bubbles: true }));
  });
  // A second empty act flushes the resolved-promise microtask's setState.
  await act(async () => {});
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  window.location.hash = "";
});

describe("run row — files associated (issue #1684)", () => {
  it("does not fetch on render, then fetches once on expand", async () => {
    const sink = { calls: [] as string[] };
    await renderPanel(completedRun("run-1"), filesClient([FILE], sink));

    // Lazy: a collapsed row makes zero network calls.
    expect(sink.calls).toEqual([]);
    expect(
      container.querySelector('[data-testid="workflow-run-file"]'),
    ).toBeNull();

    await expandFiles();

    expect(sink.calls).toEqual([
      "/api/v1/acme/workflows/runs/run-1/artifacts",
    ]);
    const entries = container.querySelectorAll(
      '[data-testid="workflow-run-file"]',
    );
    expect(entries.length).toBe(1);
  });

  it("does not re-fetch on a collapse then reopen (issue #1693)", async () => {
    // The one-shot latch: the fetch fires on the first expand, and a
    // collapse-then-reopen must not hit the route again. Without the latch a
    // regression would send one request per toggle and stay green on the
    // assertion above.
    const sink = { calls: [] as string[] };
    await renderPanel(completedRun("run-1"), filesClient([FILE], sink));
    await expandFiles();
    expect(sink.calls.length).toBe(1);

    const details = container.querySelector<HTMLDetailsElement>(
      '[data-testid="workflow-run-files"]',
    )!;
    await act(async () => {
      details.open = false;
      details.dispatchEvent(new Event("toggle", { bubbles: true }));
    });
    await expandFiles();

    expect(sink.calls.length).toBe(1);
    expect(
      container.querySelector('[data-testid="workflow-run-file"]')
        ?.textContent,
    ).toContain("Launch spec");
  });

  it("deep-links each file into the Artifacts tab at the run's version", async () => {
    const sink = { calls: [] as string[] };
    await renderPanel(completedRun("run-1"), filesClient([FILE], sink));
    await expandFiles();

    const link = container.querySelector<HTMLAnchorElement>(
      '[data-testid="workflow-run-file"] a',
    );
    expect(link).not.toBeNull();
    expect(link?.getAttribute("href")).toBe(
      artifactHref("t-a", "art-a1", 2),
    );
    expect(link?.getAttribute("href")).toBe("#/tasks/t-a?artifact=art-a1&v=2");
    expect(link?.textContent).toContain("Launch spec");
  });

  it("offers the workspace link only when the file was mirrored", async () => {
    const sink = { calls: [] as string[] };
    await renderPanel(
      completedRun("run-1"),
      filesClient([{ ...FILE, workspaceNodeId: "node-9" }], sink),
    );
    await expandFiles();

    const wsLink = container.querySelector<HTMLAnchorElement>(
      '[data-testid="workflow-run-file-workspace"]',
    );
    expect(wsLink?.getAttribute("href")).toBe("#/workspace/node-9");
  });

  it("shows an empty state for a run that produced no files", async () => {
    const sink = { calls: [] as string[] };
    await renderPanel(completedRun("run-1"), filesClient([], sink));
    await expandFiles();

    expect(sink.calls.length).toBe(1);
    const empty = container.querySelector(
      '[data-testid="workflow-run-files-empty"]',
    );
    expect(empty?.textContent).toContain("No files from this run.");
    expect(
      container.querySelector('[data-testid="workflow-run-file"]'),
    ).toBeNull();
  });

  it("labels the list when the host truncated older files (issue #1693)", async () => {
    const sink = { calls: [] as string[] };
    await renderPanel(completedRun("run-1"), filesClient([FILE], sink, true));
    await expandFiles();

    const note = container.querySelector(
      '[data-testid="workflow-run-files-truncated"]',
    );
    expect(note?.textContent).toContain("newest files");
    // The files that did come back still render.
    expect(
      container.querySelector('[data-testid="workflow-run-file"]')
        ?.textContent,
    ).toContain("Launch spec");
  });

  it("shows no truncation note for a normal file list", async () => {
    const sink = { calls: [] as string[] };
    await renderPanel(completedRun("run-1"), filesClient([FILE], sink));
    await expandFiles();

    expect(
      container.querySelector('[data-testid="workflow-run-files-truncated"]'),
    ).toBeNull();
  });

  it("renders no files control for a run with no runId", async () => {
    const sink = { calls: [] as string[] };
    await renderPanel(completedRun(undefined), filesClient([FILE], sink));

    expect(
      container.querySelector('[data-testid="workflow-run-files"]'),
    ).toBeNull();
    expect(sink.calls).toEqual([]);
  });

  it("re-fetches instead of showing the old company's files when a company switch reuses the row (issue #1693)", async () => {
    // `RunHistoryPanel` keys rows only by `run.seq` (not by company), and
    // journal sequences commonly repeat across companies. Re-render the SAME
    // root with the same seq (1) but a different company/runId — exactly what
    // React does when an operator switches company with the row left
    // expanded — and prove the stale-company file does not leak through.
    const sink = { calls: [] as string[] };
    const acmeFile: RunArtifactRow = { ...FILE, title: "Acme launch spec" };
    const globexFile: RunArtifactRow = {
      ...FILE,
      taskId: "t-b",
      artifactId: "art-b1",
      title: "Globex launch spec",
    };

    await renderPanel(completedRun("acme-run-1"), filesClient([acmeFile], sink));
    await expandFiles();
    expect(
      container.querySelector('[data-testid="workflow-run-file"]')
        ?.textContent,
    ).toContain("Acme launch spec");

    // Same seq (1, from `completedRun`), different company + runId — the
    // reuse case. Re-render without unmounting, the way `WorkflowsView`
    // re-renders `RunHistoryPanel` in place on a company switch.
    await act(async () => {
      root.render(
        createElement(RunHistoryPanel, {
          client: filesClient([globexFile], sink),
          company: "globex",
          runs: [completedRun("globex-run-1")],
          graph: null,
          workflowName: "Launch",
          onClose: () => {},
          selectedRunSeq: null,
          onSelectRun: () => {},
        }),
      );
    });
    await act(async () => {});

    expect(sink.calls).toEqual([
      "/api/v1/acme/workflows/runs/acme-run-1/artifacts",
      "/api/v1/globex/workflows/runs/globex-run-1/artifacts",
    ]);
    const entries = container.querySelectorAll(
      '[data-testid="workflow-run-file"]',
    );
    expect(entries.length).toBe(1);
    expect(entries[0]?.textContent).toContain("Globex launch spec");
    expect(entries[0]?.textContent).not.toContain("Acme");
  });

  it("drops a late success from a superseded scope (issue #1693)", async () => {
    // The reset-then-refetch path fixes the cached-row leak, but the OLD
    // request is still in flight when the new scope's request starts. If the
    // old one resolves last, its `.then` must not write the previous scope's
    // files into the reused row.
    const deferred: {
      resolve: (rows: RunArtifactRow[]) => void;
      reject: (err: unknown) => void;
    }[] = [];
    const client = deferredFilesClient(deferred);
    const acmeFile: RunArtifactRow = { ...FILE, title: "Acme launch spec" };
    const globexFile: RunArtifactRow = {
      ...FILE,
      taskId: "t-b",
      artifactId: "art-b1",
      title: "Globex launch spec",
    };

    await renderPanel(completedRun("acme-run-1"), client);
    await expandFiles();
    // Request 1 (acme) is in flight, unresolved.
    expect(deferred.length).toBe(1);

    // Same seq, different company + runId: the reset effect starts request 2
    // (globex) while request 1 is still pending.
    await swapScope(client, "globex", "globex-run-1");
    expect(deferred.length).toBe(2);

    // The OLD request settles LAST — the race. Its rows are dropped: the row
    // is still waiting on the globex request, so the acme file must not
    // appear.
    await act(async () => {
      deferred[0].resolve([acmeFile]);
    });
    expect(
      container.querySelector('[data-testid="workflow-run-file"]'),
    ).toBeNull();

    // The current request settles normally and its files render.
    await act(async () => {
      deferred[1].resolve([globexFile]);
    });
    const entries = container.querySelectorAll(
      '[data-testid="workflow-run-file"]',
    );
    expect(entries.length).toBe(1);
    expect(entries[0]?.textContent).toContain("Globex launch spec");
    expect(entries[0]?.textContent).not.toContain("Acme");
  });

  it("drops a late failure from a superseded scope (issue #1693)", async () => {
    const deferred: {
      resolve: (rows: RunArtifactRow[]) => void;
      reject: (err: unknown) => void;
    }[] = [];
    const client = deferredFilesClient(deferred);
    const globexFile: RunArtifactRow = {
      ...FILE,
      taskId: "t-b",
      artifactId: "art-b1",
      title: "Globex launch spec",
    };

    await renderPanel(completedRun("acme-run-1"), client);
    await expandFiles();
    await swapScope(client, "globex", "globex-run-1");
    expect(deferred.length).toBe(2);

    // The current (globex) request succeeds first…
    await act(async () => {
      deferred[1].resolve([globexFile]);
    });
    expect(
      container.querySelector('[data-testid="workflow-run-file"]')
        ?.textContent,
    ).toContain("Globex launch spec");

    // …then the superseded (acme) request FAILS late. Without the scope guard
    // its `.catch` flips the row to the error state over an already-successful
    // render; the guard must drop it, leaving the error line absent and the
    // globex files in place.
    await act(async () => {
      deferred[0].reject(new Error("late acme failure"));
    });
    expect(
      container.querySelector('[data-testid="workflow-run-files-error"]'),
    ).toBeNull();
    expect(
      container.querySelector('[data-testid="workflow-run-file"]')
        ?.textContent,
    ).toContain("Globex launch spec");
  });

  it("shows the error for a current-scope failure and retries on reopen (issue #1693)", async () => {
    // The superseded-scope tests prove the guard drops STALE outcomes; this
    // one covers the in-flight request for the CURRENT scope. Its failure
    // must render the error line, and the failure path's latch reset must let
    // a reopen retry — both reachable in production and neither asserted
    // anywhere else.
    //
    // The failure is an already-rejected promise fired by the expand toggle
    // inside act, not a manually-rejected deferred: the latter lands several
    // microtasks deeper (async wrapper adoption → `.then` pass-through →
    // `.catch`) and intermittently escapes React's act flush, leaving the
    // error line uncommitted and unrecoverable — the flake that red the
    // advisory `Console (current Node)` lane. An already-rejected promise
    // settles before the component attaches its handlers, so every reaction
    // stays inside the act window. The mode flip then heals the client for
    // the reopen, and assertions check outcomes rather than exact request
    // counts because jsdom fires a native `toggle` asynchronously on
    // `details.open = true` alongside the dispatched one — a second request
    // may or may not land while the failure has cleared the latch, and the
    // retry must pass either way.
    const sink = { calls: [] as string[] };
    const modes = { fail: true };
    const file: RunArtifactRow = { ...FILE, title: "Retried launch spec" };
    const client = modeFilesClient([file], sink, modes);

    await renderPanel(completedRun("run-1"), client);
    await expandFiles();

    // The current-scope request failed: the error line appears, and the
    // latch has cleared so the next open retries.
    expect(
      container.querySelector('[data-testid="workflow-run-files-error"]')
        ?.textContent,
    ).toContain("Reopen to try again");
    expect(sink.calls.length).toBeGreaterThanOrEqual(1);

    // Collapse, then reopen with the client healed: a fresh request fires
    // and its success renders the files, proving the retry latch reset.
    modes.fail = false;
    const details = container.querySelector<HTMLDetailsElement>(
      '[data-testid="workflow-run-files"]',
    )!;
    await act(async () => {
      details.open = false;
      details.dispatchEvent(new Event("toggle", { bubbles: true }));
    });
    await expandFiles();
    expect(sink.calls.length).toBeGreaterThanOrEqual(2);

    expect(
      container.querySelector('[data-testid="workflow-run-files-error"]'),
    ).toBeNull();
    expect(
      container.querySelector('[data-testid="workflow-run-file"]')
        ?.textContent,
    ).toContain("Retried launch spec");
  });
});
