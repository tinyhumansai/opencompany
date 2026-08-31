import { describe, expect, it, vi } from "vitest";

import { fetchFile, fetchTree, originLabel, type WorkspaceOrigin } from "@/api/workspace";
import { rosterNameMap } from "@/lib/roster-names";

/**
 * Workspace authorship (issue #326), on the console side.
 *
 * Two properties are worth pinning here rather than leaving to the view. The
 * label is what the operator actually reads, and getting the operator case
 * wrong would put a chip on every note in the tree. The normalization default
 * is the rollout-skew guard: a console served by a host that predates the field
 * must render "operator", not a blank badge or a crash.
 */

const client = (payload: unknown) =>
  ({
    scopeFor: () => "/api/v1/company/acme",
    get: vi.fn().mockResolvedValue(payload),
  }) as never;

describe("originLabel", () => {
  it("names the agent that authored a node", () => {
    expect(originLabel({ kind: "agent", id: "ceo" })).toBe("Teammate · ceo");
  });

  it("distinguishes a seeded node from one somebody wrote", () => {
    expect(originLabel({ kind: "seed" })).toBe("Seeded");
  });

  it("names the teammate when it is given a roster to resolve against (issue #1723)", () => {
    // The raw handle is engine plumbing — `seo_specialist` where the operator
    // knows "SEO Specialist" — and this label sits beside names the rest of
    // the workspace has already resolved. Routed through the one shared
    // `rosterDisplayName` rather than a second lookup of its own.
    const names = rosterNameMap([{ id: "seo_specialist", name: "SEO Specialist" }]);
    expect(originLabel({ kind: "agent", id: "seo_specialist" }, names)).toBe(
      "Teammate · SEO Specialist",
    );
    // An id the roster does not carry falls back to the id, never to a blank
    // label — and a caller with no roster to hand gets exactly the string it
    // got before the parameter existed.
    expect(originLabel({ kind: "agent", id: "ghost" }, names)).toBe("Teammate · ghost");
    expect(originLabel({ kind: "agent", id: "ceo" })).toBe("Teammate · ceo");
  });

  it("says nothing for a plain operator note", () => {
    // Deliberately null rather than "Operator": the operator is the
    // unremarkable default here, and badging it would decorate nearly every
    // note in the tree while conveying nothing.
    expect(originLabel({ kind: "operator" })).toBeNull();
    expect(originLabel(undefined)).toBeNull();
  });
});

describe("normalization off the wire", () => {
  it("defaults a node with no origins to operator", async () => {
    const [node] = await fetchTree(
      client([{ id: "n1", name: "voice.md", kind: "file", updatedAt: 1 }]),
      "acme",
    );
    expect(node.createdBy).toEqual<WorkspaceOrigin>({ kind: "operator" });
    expect(node.updatedBy).toEqual<WorkspaceOrigin>({ kind: "operator" });
    // The pre-existing parentId normalization still holds alongside it.
    expect(node.parentId).toBeNull();
  });

  it("keeps the origins the host actually sent, on the node and its backlinks", async () => {
    const node = await fetchFile(
      client({
        id: "n1",
        name: "voice.md",
        content: "# Voice",
        updatedAt: 2,
        createdBy: { kind: "agent", id: "cmo" },
        updatedBy: { kind: "operator" },
        backlinks: [
          { id: "n2", name: "brief.md", kind: "file", updatedAt: 1, createdBy: { kind: "seed" } },
        ],
      }),
      "acme",
      "n1",
    );
    expect(node.createdBy).toEqual<WorkspaceOrigin>({ kind: "agent", id: "cmo" });
    expect(node.updatedBy).toEqual<WorkspaceOrigin>({ kind: "operator" });
    expect(node.backlinks[0].createdBy).toEqual<WorkspaceOrigin>({ kind: "seed" });
    expect(node.backlinks[0].updatedBy).toEqual<WorkspaceOrigin>({ kind: "operator" });
  });
});
