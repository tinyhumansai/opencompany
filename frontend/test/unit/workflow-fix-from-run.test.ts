import { describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { fixWorkflowFromRun, type WorkflowFixFromRun } from "@/api/workflows";

/**
 * The fix-from-run copilot's request shaping (issue #840, PR-3).
 *
 * Two properties are worth pinning at this layer: the route it posts to (the
 * failing workflow's `wid`, encoded, under `fix-from-run`) and the body it sends
 * (`runId` + optional `errorHint`, exactly the camelCase the host reads). The
 * behaviour behind it lives on the host; duplicating its rules here would produce
 * a second set that can disagree.
 */

/** A client that records calls and replays a scripted body. */
function fakeClient(body: unknown) {
  const calls: Array<{ method: string; path: string; body?: unknown }> = [];
  const client = {
    scopeFor: (company: string | null) =>
      company ? `/api/v1/companies/${company}` : "/api/v1/company",
    post: async (path: string, payload?: unknown) => {
      calls.push({ method: "POST", path, body: payload });
      return body;
    },
  } as unknown as OpenCompanyClient;
  return { client, calls };
}

describe("fixWorkflowFromRun", () => {
  it("POSTs runId + errorHint to the wid's fix-from-run route, encoding the id", async () => {
    const answer: WorkflowFixFromRun = {
      automatable: true,
      summary: "dropped the unwired step",
      workflow: {
        id: "weekly digest",
        name: "Weekly digest",
        version: null,
        nodes: [],
        edges: [],
      },
      readiness: { ok: true },
    };
    const { client, calls } = fakeClient(answer);

    const res = await fixWorkflowFromRun(client, "acme", "weekly digest", {
      runId: "run-1",
      errorHint: "the tool was not wired",
    });

    expect(calls).toEqual([
      {
        method: "POST",
        path: "/api/v1/companies/acme/workflows/weekly%20digest/fix-from-run",
        body: { runId: "run-1", errorHint: "the tool was not wired" },
      },
    ]);
    // The host's answer is returned unchanged — the caller keys on `automatable`.
    expect(res).toBe(answer);
  });

  it("uses the company-scoped path and carries an absent hint as undefined", async () => {
    const { client, calls } = fakeClient({
      automatable: false,
      reason: "this cannot be fixed by re-wiring",
    } satisfies WorkflowFixFromRun);

    await fixWorkflowFromRun(client, null, "wid", { runId: "r" });

    expect(calls[0].path).toBe("/api/v1/company/workflows/wid/fix-from-run");
    expect(calls[0].body).toEqual({ runId: "r", errorHint: undefined });
  });
});
