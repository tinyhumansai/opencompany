// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type McpServer, type McpSource } from "@/api/types";
import type { ToolPolicyDocument, ToolPolicyRow } from "@/api/mcp-tool-policy";

/**
 * The console's per-tool permissions panel (issue #2373).
 *
 * Two properties are worth pinning, and neither is visible from the component's
 * props. The first is which half of the API a row may call: a server can be
 * both a declaration and a directory install, and the host reconciles those
 * into one row that keeps the declaration's provenance *and* carries a
 * `serverId` — so "has a serverId" is not "is a registry row" (#1270). The
 * second is that what the panel renders is the document the host echoed back,
 * never the value that was clicked: the host resolves a mode out of the
 * override, the tier's bulk default and the declaration, so a console that
 * rendered the click would be carrying a second copy of that ladder and would
 * eventually disagree with the gate.
 */

const api = vi.hoisted(() => ({
  readToolPolicy: vi.fn(),
  writeToolPolicy: vi.fn(),
  resetToolPolicy: vi.fn(),
}));

vi.mock("@/api/mcp-tool-policy", async () => {
  const actual = await vi.importActual<typeof import("@/api/mcp-tool-policy")>(
    "@/api/mcp-tool-policy",
  );
  return { ...actual, ...api };
});

const { policyTarget } = await import("@/api/mcp-tool-policy");
const { McpToolPermissions, tierPatch } =
  await import("@/views/mcp/McpToolPermissions");

function row(over: Partial<McpServer> & { source: McpSource }): McpServer {
  return {
    name: "notion",
    endpoint: "https://mcp.notion.com/mcp",
    enabled: true,
    allowedTools: [],
    disallowedTools: [],
    readOnlyTools: [],
    timeoutSecs: 30,
    authConfigured: false,
    ...over,
  };
}

function doc(over: Partial<ToolPolicyDocument> = {}): ToolPolicyDocument {
  return {
    server: "notion",
    tierDefaults: {
      read_only: { mode: "always_allow", stored: false },
      interactive: { mode: "needs_approval", stored: false },
      write_delete: { mode: "needs_approval", stored: false },
    },
    tools: [],
    discoveredAtMillis: 1,
    ...over,
  };
}

describe("which routes a row's permissions live behind", () => {
  it("sends a directory install to the registry route, keyed by its install id", () => {
    expect(
      policyTarget(row({ source: "registry", serverId: "srv_9fa1" })),
    ).toEqual({
      kind: "registry",
      serverId: "srv_9fa1",
    });
  });

  it("keeps a reconciled row on the declared route despite its serverId", () => {
    // Declared in company.toml AND installed from the directory. The registry
    // route would address the install and leave the declaration's own policy —
    // the one the `mcp:<name>` bridge actually enforces — untouched.
    expect(
      policyTarget(row({ source: "manifest", serverId: "srv_9fa1" })),
    ).toEqual({
      kind: "declared",
      name: "notion",
    });
  });

  it("refuses to guess for a registry row with no install id", () => {
    expect(policyTarget(row({ source: "registry" }))).toBeNull();
  });
});

const client = {} as unknown as OpenCompanyClient;

let container: HTMLDivElement;
let root: Root;

function el(testId: string): HTMLElement | null {
  return container.querySelector(`[data-testid="${testId}"]`);
}

async function mount(server: McpServer, canManage = true, reloadKey = 0) {
  await act(async () => {
    root.render(
      createElement(McpToolPermissions, {
        client,
        company: "acme",
        server,
        canManage,
        reloadKey,
      }),
    );
  });
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  vi.clearAllMocks();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("a damaged policy document", () => {
  it("offers the repair instead of rendering permissions nobody chose", async () => {
    api.readToolPolicy.mockRejectedValue(
      new ApiError(
        409,
        "policy_unreadable",
        "the stored tool permissions cannot be read.",
        true,
      ),
    );

    await mount(row({ source: "runtime" }));

    expect(el("mcp-permissions-unreadable")).not.toBeNull();
    expect(el("mcp-permissions-clear")).not.toBeNull();
    // An empty policy would read as "every tool runs on whatever the tier says",
    // and an edit saved from that view would make it true.
    expect(
      container.querySelectorAll('[data-testid="mcp-permission-row"]'),
    ).toHaveLength(0);
  });

  it("does not offer the repair to someone who cannot write", async () => {
    api.readToolPolicy.mockRejectedValue(
      new ApiError(
        409,
        "policy_unreadable",
        "the stored tool permissions cannot be read.",
        true,
      ),
    );

    await mount(row({ source: "runtime" }), false);

    expect(el("mcp-permissions-unreadable")).not.toBeNull();
    expect(el("mcp-permissions-clear")).toBeNull();
  });
});

describe("what the panel renders after a write", () => {
  const before = doc({
    tools: [
      {
        tool: "delete_page",
        effectiveTier: "read_only",
        suggestedTier: "write_delete",
        mode: "always_allow",
        isOverride: true,
      },
    ],
  });

  it("renders the host's answer, not the row as it was clicked", async () => {
    api.readToolPolicy.mockResolvedValue(before);
    // Clearing the override drops the operator's `read_only` reclassification,
    // so the row comes back under discovery's suggestion and the tier's own
    // default. Nothing in the console could have computed that.
    api.writeToolPolicy.mockResolvedValue(
      doc({
        tools: [
          {
            tool: "delete_page",
            effectiveTier: "write_delete",
            suggestedTier: "write_delete",
            mode: "needs_approval",
            isOverride: false,
          },
        ],
      }),
    );

    await mount(row({ source: "runtime" }));
    expect(el("mcp-permission-clear-row")).not.toBeNull();

    await act(async () => {
      el("mcp-permission-clear-row")?.click();
    });

    expect(api.writeToolPolicy).toHaveBeenCalledWith(
      client,
      "acme",
      { kind: "declared", name: "notion" },
      {
        tools: [{ tool: "delete_page" }],
      },
    );
    // The clear control is gone because the echoed row is no longer an
    // override — the panel re-derived from the response.
    expect(el("mcp-permission-clear-row")).toBeNull();
  });

  it("keeps the panel standing when a write is refused", async () => {
    api.readToolPolicy.mockResolvedValue(before);
    api.writeToolPolicy.mockRejectedValue(
      new ApiError(403, "forbidden", "admins only", true),
    );

    await mount(row({ source: "runtime" }));
    await act(async () => {
      el("mcp-permission-clear-row")?.click();
    });

    expect(el("mcp-permissions-write-error")?.textContent).toContain(
      "admins only",
    );
    // The refused row still reads as the override it still is.
    expect(el("mcp-permission-clear-row")).not.toBeNull();
  });
});

describe("a server nothing has discovered yet", () => {
  it("says the tier defaults still apply rather than showing an empty list", async () => {
    api.readToolPolicy.mockResolvedValue(doc({ discoveredAtMillis: 0 }));

    await mount(row({ source: "runtime" }));

    expect(el("mcp-permissions-empty")).not.toBeNull();
  });
});

describe("what the per-tier control says is set", () => {
  it("reads as unset when the host stored nothing for that tier", async () => {
    // The bug this pins: the control rendered the tier's nominal mode, so a
    // fresh server showed the read-only tier as "Allow" while every read-only
    // row needed approval — and choosing the value already on screen granted a
    // bulk allow.
    api.readToolPolicy.mockResolvedValue(doc());

    await mount(row({ source: "runtime" }));

    const trigger = container.querySelector<HTMLElement>("#tier-read_only");
    expect(trigger?.textContent).toContain("Not set");
    expect(trigger?.textContent).not.toContain("Allow");
  });

  it("reads as the stored mode once an operator has written one", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({
        tierDefaults: {
          read_only: { mode: "always_allow", stored: true },
          interactive: { mode: "needs_approval", stored: false },
          write_delete: { mode: "needs_approval", stored: false },
        },
      }),
    );

    await mount(row({ source: "runtime" }));

    expect(container.querySelector("#tier-read_only")?.textContent).toContain(
      "Allow",
    );
  });
});

describe("a tool the allow and deny lists keep from being sent", () => {
  it("says so on the row, because its mode will never be consulted", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({
        tools: [
          {
            tool: "place_order",
            effectiveTier: "interactive",
            mode: "needs_approval",
            isOverride: false,
          },
        ],
      }),
    );

    await mount(row({ source: "runtime", disallowedTools: ["place_order"] }));

    expect(container.textContent).toContain("Not sent");
  });

  it("says nothing when the lists let every discovered tool through", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({
        tools: [
          {
            tool: "place_order",
            effectiveTier: "interactive",
            mode: "needs_approval",
            isOverride: false,
          },
        ],
      }),
    );

    await mount(row({ source: "runtime" }));

    expect(container.textContent).not.toContain("Not sent");
  });
});

describe("what a choice in the tier control means on the wire", () => {
  it("sends the mode when one was chosen", () => {
    expect(tierPatch("read_only", "always_allow")).toEqual({
      tierDefaults: { read_only: "always_allow" },
    });
  });

  it("sends null when the tier was put back to unset", () => {
    // Not an omitted key: the host reads a missing tier as "leave it alone",
    // so an omission would leave the bulk allow in force and the control
    // would read as cleared while the gate went on letting tools through.
    expect(tierPatch("read_only", "unset")).toEqual({
      tierDefaults: { read_only: null },
    });
  });

  it("clears the tier the operator named, not some other one", () => {
    expect(tierPatch("write_delete", "unset")).toEqual({
      tierDefaults: { write_delete: null },
    });
  });
});

describe("a probe that ran while the panel was open", () => {
  it("re-reads the policy, because the probe rewrote what it resolves against", async () => {
    api.readToolPolicy.mockResolvedValue(doc({ discoveredAtMillis: 0 }));

    const server = row({ source: "runtime" });
    await mount(server);
    expect(el("mcp-permissions-empty")).not.toBeNull();
    expect(api.readToolPolicy).toHaveBeenCalledTimes(1);

    api.readToolPolicy.mockResolvedValue(
      doc({
        discoveredAtMillis: 2,
        tools: [
          {
            tool: "search_pages",
            effectiveTier: "read_only",
            suggestedTier: "read_only",
            mode: "needs_approval",
            isOverride: false,
          },
        ],
      }),
    );
    await mount(server, true, 1);

    expect(api.readToolPolicy).toHaveBeenCalledTimes(2);
    expect(el("mcp-permissions-empty")).toBeNull();
    expect(container.textContent).toContain("search_pages");
  });

  it("does not re-read when nothing probed it", async () => {
    api.readToolPolicy.mockResolvedValue(doc());

    const server = row({ source: "runtime" });
    await mount(server);
    await mount(server);

    expect(api.readToolPolicy).toHaveBeenCalledTimes(1);
  });
});

function tool(over: Partial<ToolPolicyRow> & { tool: string }): ToolPolicyRow {
  return {
    effectiveTier: "read_only",
    mode: "always_allow",
    isOverride: false,
    ...over,
  };
}

function sectionRows(tier: string): string[] {
  const section = container.querySelector(
    `[data-testid="mcp-tier-section-${tier}"]`,
  );
  return Array.from(
    section?.querySelectorAll('[data-testid="mcp-permission-row"]') ?? [],
  ).map((row) => row.querySelector("span")?.textContent ?? "");
}

describe("the three tiers as sections", () => {
  it("opens the riskiest section that has tools, and leaves the rest shut", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({
        tools: [
          tool({ tool: "archive_db", effectiveTier: "write_delete" }),
          tool({ tool: "move_page", effectiveTier: "interactive" }),
          tool({ tool: "get_page" }),
        ],
      }),
    );

    await mount(row({ source: "runtime" }));

    expect(
      el("mcp-tier-toggle-write_delete")?.getAttribute("aria-expanded"),
    ).toBe("true");
    expect(
      el("mcp-tier-toggle-interactive")?.getAttribute("aria-expanded"),
    ).toBe("false");
    expect(el("mcp-tier-toggle-read_only")?.getAttribute("aria-expanded")).toBe(
      "false",
    );
  });

  it("skips an empty tier when choosing which section opens", async () => {
    // Nothing writes or deletes, so opening that section would greet the
    // operator with an empty box while eleven decided tools sat collapsed.
    api.readToolPolicy.mockResolvedValue(
      doc({ tools: [tool({ tool: "get_page" })] }),
    );

    await mount(row({ source: "runtime" }));

    expect(
      el("mcp-tier-toggle-write_delete")?.getAttribute("aria-expanded"),
    ).toBe("false");
    expect(el("mcp-tier-toggle-read_only")?.getAttribute("aria-expanded")).toBe(
      "true",
    );
  });

  it("groups each tool under the tier the host resolved, not the one discovery guessed", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({
        tools: [
          // An operator reclassified this down. Grouping it by `suggestedTier`
          // would file it where its permission is not resolved from.
          tool({
            tool: "delete_page",
            effectiveTier: "read_only",
            suggestedTier: "write_delete",
            isOverride: true,
          }),
          tool({
            tool: "move_page",
            effectiveTier: "interactive",
            suggestedTier: "interactive",
          }),
          tool({
            tool: "archive_db",
            effectiveTier: "write_delete",
            suggestedTier: "write_delete",
          }),
        ],
      }),
    );

    await mount(row({ source: "runtime" }));

    // Only the riskiest non-empty section is open on arrival, so the other two
    // are opened to read what is filed under them.
    expect(sectionRows("write_delete")).toEqual(["archive_db"]);
    for (const tier of ["interactive", "read_only"]) {
      await act(async () => {
        el(`mcp-tier-toggle-${tier}`)?.click();
      });
    }
    expect(sectionRows("read_only")).toEqual(["delete_page"]);
    expect(sectionRows("interactive")).toEqual(["move_page"]);
  });

  it("counts each section, so its size is readable while it is shut", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({
        tools: [
          tool({ tool: "get_page" }),
          tool({ tool: "search_pages" }),
          tool({ tool: "archive_db", effectiveTier: "write_delete" }),
        ],
      }),
    );

    await mount(row({ source: "runtime" }));

    expect(el("mcp-tier-count-read_only")?.textContent).toBe("(2)");
    expect(el("mcp-tier-count-write_delete")?.textContent).toBe("(1)");
    expect(el("mcp-tier-count-interactive")?.textContent).toBe("(0)");
  });

  it("reads riskiest first, so the tools worth deciding are not below the fold", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({ tools: [tool({ tool: "get_page" })] }),
    );

    await mount(row({ source: "runtime" }));

    const order = Array.from(
      container.querySelectorAll('[data-testid^="mcp-tier-section-"]'),
    ).map((section) => section.getAttribute("data-testid"));
    expect(order).toEqual([
      "mcp-tier-section-write_delete",
      "mcp-tier-section-interactive",
      "mcp-tier-section-read_only",
    ]);
  });

  it("keeps a tier with no tools, because its default still governs what turns up", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({
        tools: [tool({ tool: "get_page" })],
        tierDefaults: {
          read_only: { mode: "always_allow", stored: false },
          interactive: { mode: "needs_approval", stored: false },
          write_delete: { mode: "blocked", stored: true },
        },
      }),
    );

    await mount(row({ source: "runtime" }));

    expect(el("mcp-tier-section-write_delete")).not.toBeNull();
    expect(
      container.querySelector("#tier-write_delete")?.textContent,
    ).toContain("Block");
  });

  it("shuts a section without forgetting what is in it", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({ tools: [tool({ tool: "get_page" })] }),
    );

    await mount(row({ source: "runtime" }));
    expect(sectionRows("read_only")).toEqual(["get_page"]);

    await act(async () => {
      el("mcp-tier-toggle-read_only")?.click();
    });

    expect(el("mcp-tier-toggle-read_only")?.getAttribute("aria-expanded")).toBe(
      "false",
    );
    expect(sectionRows("read_only")).toEqual([]);
    expect(el("mcp-tier-count-read_only")?.textContent).toBe("(1)");
  });
});

describe("a section longer than the panel wants to show", () => {
  const many = (
    n: number,
    over: (i: number) => Partial<ToolPolicyRow> = () => ({}),
  ) =>
    Array.from({ length: n }, (_, i) => tool({ tool: `get_${i}`, ...over(i) }));

  it("shows the first eight and offers the rest", async () => {
    api.readToolPolicy.mockResolvedValue(doc({ tools: many(11) }));

    await mount(row({ source: "runtime" }));

    expect(sectionRows("read_only")).toHaveLength(8);
    expect(el("mcp-tier-more-read_only")?.textContent).toBe("3 more");
  });

  it("offers the way back once the rest is out", async () => {
    api.readToolPolicy.mockResolvedValue(doc({ tools: many(11) }));

    await mount(row({ source: "runtime" }));
    await act(async () => {
      el("mcp-tier-more-read_only")?.click();
    });

    expect(sectionRows("read_only")).toHaveLength(11);
    expect(el("mcp-tier-more-read_only")?.textContent).toBe("Show fewer");
  });

  it("offers nothing when the section fits", async () => {
    api.readToolPolicy.mockResolvedValue(doc({ tools: many(8) }));

    await mount(row({ source: "runtime" }));

    expect(sectionRows("read_only")).toHaveLength(8);
    expect(el("mcp-tier-more-read_only")).toBeNull();
  });

  it("never hides a decision an operator made", async () => {
    // The twelfth tool is the one that was blocked. Capped away, the section
    // would read as though nobody had touched it.
    api.readToolPolicy.mockResolvedValue(
      doc({
        tools: [
          ...many(11),
          tool({ tool: "blocked_one", mode: "blocked", isOverride: true }),
        ],
      }),
    );

    await mount(row({ source: "runtime" }));

    // Nine shown: the cap's eight unremarkable rows plus the decided one,
    // which the cap does not get a vote on.
    expect(sectionRows("read_only")).toContain("blocked_one");
    expect(sectionRows("read_only")).toHaveLength(9);
    expect(el("mcp-tier-more-read_only")?.textContent).toBe("3 more");
  });

  it("never hides a tool the transport will refuse to send", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({ tools: [...many(11), tool({ tool: "never_sent" })] }),
    );

    await mount(row({ source: "runtime", disallowedTools: ["never_sent"] }));

    expect(sectionRows("read_only")).toContain("never_sent");
  });

  it("keeps truncation and collapse independent of one another", async () => {
    api.readToolPolicy.mockResolvedValue(doc({ tools: many(11) }));

    await mount(row({ source: "runtime" }));
    await act(async () => {
      el("mcp-tier-more-read_only")?.click();
    });
    await act(async () => {
      el("mcp-tier-toggle-read_only")?.click();
    });
    await act(async () => {
      el("mcp-tier-toggle-read_only")?.click();
    });

    expect(sectionRows("read_only")).toHaveLength(11);
  });
});

describe("the mode a row is set to", () => {
  it("names what it suggests on every row, not only where it disagrees", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({
        tools: [
          tool({
            tool: "get_page",
            effectiveTier: "read_only",
            suggestedTier: "read_only",
          }),
        ],
      }),
    );

    await mount(row({ source: "runtime" }));

    expect(el("mcp-permission-row")?.textContent).toContain(
      "suggested: read-only",
    );
  });

  it("says nothing about a suggestion the host did not make", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({ tools: [tool({ tool: "get_page" })] }),
    );

    await mount(row({ source: "runtime" }));

    expect(el("mcp-permission-row")?.textContent).not.toContain("suggested:");
  });

  it("writes the mode a segment was clicked for, and re-renders from the answer", async () => {
    api.readToolPolicy.mockResolvedValue(
      doc({ tools: [tool({ tool: "get_page" })] }),
    );
    api.writeToolPolicy.mockResolvedValue(
      doc({
        tools: [tool({ tool: "get_page", mode: "blocked", isOverride: true })],
      }),
    );

    await mount(row({ source: "runtime" }));
    await act(async () => {
      container
        .querySelector<HTMLButtonElement>('[data-testid="mcp-mode-blocked"]')
        ?.click();
    });

    expect(api.writeToolPolicy).toHaveBeenCalledWith(
      client,
      "acme",
      { kind: "declared", name: "notion" },
      { tools: [{ tool: "get_page", mode: "blocked" }] },
    );
    expect(
      container
        .querySelector('[data-testid="mcp-mode-blocked"]')
        ?.getAttribute("aria-checked"),
    ).toBe("true");
    expect(el("mcp-permission-clear-row")).not.toBeNull();
  });
});
