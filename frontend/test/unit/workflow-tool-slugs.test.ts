import { describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { composeCopilotMessage } from "@/api/workflow-copilot";
import { listWorkflowToolSlugs, type WorkflowGraph } from "@/api/workflows";

/** A minimal graph; this suite is about the tool lists, not the graph render. */
const graph: WorkflowGraph = {
  id: "weekly_report",
  name: "Weekly report",
  version: null,
  nodes: [{ id: "collect", kind: "agent", name: "Collect", agent: "analyst" }],
  edges: [],
};

/**
 * `GET …/workflows/tool-slugs` on the client (issues #783, #874).
 *
 * Since #874 the route answers two lists — the **effective** slugs a proposal
 * may ground on, and the granted-but-unwired ones it must not — and the client's
 * whole job is to hand both on without losing the distinction. The one thing it
 * adds is the back-compat default: a host predating #874 sends no `unwired` key,
 * and "the key is missing" and "nothing is unwired" mean the same thing to every
 * caller, so it normalises to `[]` rather than leaking `undefined` into the
 * copilot context.
 */

/** A fake client that answers one canned body and records the path asked for. */
function readingClient(
  body: unknown,
  sink: { path?: string } = {},
): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async <T>(path: string): Promise<T> => {
      sink.path = path;
      return body as T;
    },
  } as unknown as OpenCompanyClient;
}

describe("listWorkflowToolSlugs", () => {
  it("reads both lists from the scoped route", async () => {
    const sink: { path?: string } = {};
    const result = await listWorkflowToolSlugs(
      readingClient(
        {
          slugs: ["shell", "send_email"],
          unwired: [
            {
              slug: "web_search",
              reason: "searchBackendNotConfigured",
              detail: "granted, but no managed search backend is configured",
            },
          ],
        },
        sink,
      ),
      "acme",
    );
    expect(sink.path).toBe("/api/v1/acme/workflows/tool-slugs");
    expect(result.slugs).toEqual(["shell", "send_email"]);
    expect(result.unwired).toHaveLength(1);
    expect(result.unwired[0].slug).toBe("web_search");
    expect(result.unwired[0].reason).toBe("searchBackendNotConfigured");
  });

  /**
   * The two lists stay apart end to end: what the route separated must still be
   * separated in the prompt the model reads.
   *
   * Asserted against `composeCopilotMessage`, not against this client's return
   * value. The client is a pass-through, so a test that fed it a split body and
   * checked the halves came back split would be asserting a property of its own
   * fixture — it would pass with the whole of #874 reverted. The narrowing that
   * can actually regress on this side is the prompt's: an unwired slug must land
   * under the "do NOT author" heading and never in the groundable list above it.
   */
  it("keeps an unwired tool out of the groundable list in the composed prompt", async () => {
    const tools = await listWorkflowToolSlugs(
      readingClient({
        slugs: ["shell"],
        // Not `web_search`: the prompt's config-schema example names that slug
        // verbatim, so it appears either way and could not witness the split.
        unwired: [
          {
            slug: "deep_research",
            reason: "searchBackendNotConfigured",
            detail: "granted, but no managed search backend is configured",
          },
        ],
      }),
      null,
    );

    const message = composeCopilotMessage(
      {
        graph,
        runs: [],
        runsKnown: true,
        toolSlugs: tools.slugs,
        toolSlugsKnown: true,
        unwiredTools: tools.unwired,
      },
      "add a research step",
    );

    const groundable = message.slice(
      message.indexOf("### Tools"),
      message.indexOf("### Granted but NOT wired"),
    );
    expect(groundable).toContain("shell");
    expect(groundable).not.toContain("deep_research");
    expect(message).toMatch(/granted but NOT wired[\s\S]*deep_research/i);
  });

  it("defaults `unwired` to an empty list on a host predating issue #874", async () => {
    const result = await listWorkflowToolSlugs(
      readingClient({ slugs: ["shell"] }),
      "acme",
    );
    expect(result.slugs).toEqual(["shell"]);
    expect(result.unwired).toEqual([]);
  });
});
