// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { McpCatalogueEntry, McpCatalogueSearch } from "@/api/mcp-registry";
import { ApiError } from "@/api/types";

const registryApi = vi.hoisted(() => ({
  getMcpRegistryEntry: vi.fn(),
  searchMcpRegistry: vi.fn(),
}));

vi.mock("@/api/mcp-registry", () => registryApi);

const { McpDiscover } = await import("@/views/connections/McpRegistryBrowser");
const { appendPage, matchFeatured, mergeFeatured } = await import("@/hooks/use-mcp-directory");

function entry(name: string): McpCatalogueEntry {
  return {
    qualifiedName: `io.example/${name}`,
    displayName: name,
    description: `${name} tools`,
    source: "mcp_official",
    official: true,
    useCount: 0,
  } as McpCatalogueEntry;
}

function page(n: number, total: number, names: string[]): McpCatalogueSearch {
  return { page: n, totalPages: total, servers: names.map(entry) };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (err: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const client = {} as OpenCompanyClient;
let container: HTMLDivElement;
let root: Root;
const onInstall = vi.fn();

function testId(id: string): HTMLElement | null {
  return document.body.querySelector<HTMLElement>(`[data-testid="${id}"]`);
}

function names(): string[] {
  return [...document.body.querySelectorAll('[data-testid="mcp-discover-card"] span.truncate.text-sm')].map(
    (n) => n.textContent ?? "",
  );
}

async function settle(ms = 400) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

async function render(query: string) {
  await act(async () => {
    root.render(
      createElement(McpDiscover, {
        client,
        company: "acme",
        query,
        layout: "cards",
        servers: [],
        installing: null,
        canManage: true,
        onInstall,
      }),
    );
  });
}

async function click(id: string) {
  const node = testId(id);
  if (!node) throw new Error(`no ${id}`);
  await act(async () => {
    node.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  vi.resetAllMocks();
  vi.useFakeTimers();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
});

describe("appendPage", () => {
  it("drops rows already listed and duplicates inside the new page", () => {
    const merged = appendPage([entry("a"), entry("b")], [entry("b"), entry("c"), entry("c")]);
    expect(merged.map((e) => e.displayName)).toEqual(["a", "b", "c"]);
  });
});

describe("matchFeatured", () => {
  const rows = [
    { ...entry("Linear"), description: "Sync Notion pages into issues" },
    { ...entry("Notion"), qualifiedName: "com.notion/mcp", description: "Pages and databases" },
    entry("GitHub"),
  ];

  it("matches every word case-insensitively and ranks name matches first", () => {
    expect(matchFeatured(rows, "NOTION").map((e) => e.displayName)).toEqual(["Notion", "Linear"]);
    expect(matchFeatured(rows, "notion pages").map((e) => e.displayName)).toEqual(["Linear", "Notion"]);
    expect(matchFeatured(rows, "notion github")).toEqual([]);
  });

  it("matches the qualified name and returns nothing for a blank query", () => {
    expect(matchFeatured(rows, "com.notion").map((e) => e.displayName)).toEqual(["Notion"]);
    expect(matchFeatured(rows, "   ")).toEqual([]);
  });
});

describe("mergeFeatured", () => {
  it("puts popular matches first and drops live duplicates by qualified name", () => {
    const merged = mergeFeatured([entry("notion")], [entry("other"), entry("notion"), entry("other")]);
    expect(merged.map((e) => e.displayName)).toEqual(["notion", "other"]);
  });
});

describe("Show more", () => {
  it("appends the next page without duplicates and hides at the end", async () => {
    registryApi.searchMcpRegistry
      .mockResolvedValueOnce(page(1, 2, ["a", "b"]))
      .mockResolvedValueOnce(page(2, 2, ["b", "c"]));
    await render("");
    await settle();
    expect(names()).toEqual(["a", "b"]);

    await click("mcp-discover-more");
    await settle();
    expect(names()).toEqual(["a", "b", "c"]);
    expect(testId("mcp-discover-more")).toBeNull();
  });

  it("shows a failed page inline and appends it on Retry", async () => {
    registryApi.searchMcpRegistry
      .mockResolvedValueOnce(page(1, 3, ["a"]))
      .mockRejectedValueOnce(new ApiError(504, "registry_timeout", "The MCP directory is taking too long to search right now."))
      .mockResolvedValueOnce(page(2, 3, ["b"]));
    await render("");
    await settle();

    await click("mcp-discover-more");
    await settle();
    expect(testId("mcp-discover-more-error")?.textContent).toContain("taking too long");
    expect(testId("mcp-discover-more")?.textContent).toContain("Retry");
    expect(names()).toEqual(["a"]);

    await click("mcp-discover-more");
    await settle();
    expect(testId("mcp-discover-more-error")).toBeNull();
    expect(names()).toEqual(["a", "b"]);
    expect(registryApi.searchMcpRegistry).toHaveBeenLastCalledWith(
      client,
      "acme",
      { q: undefined, page: 2, pageSize: 20 },
      expect.anything(),
    );
  });
});

describe("search", () => {
  it("keeps earlier results on screen and says what it is searching for", async () => {
    registryApi.searchMcpRegistry.mockResolvedValueOnce(page(1, 1, ["a", "b"]));
    await render("");
    await settle();

    const slow = deferred<McpCatalogueSearch>();
    registryApi.searchMcpRegistry.mockReturnValueOnce(slow.promise);
    await render("notion");
    await settle();

    expect(testId("mcp-discover-searching")?.textContent).toContain("Searching for “notion”");
    expect(names()).toEqual(["a", "b"]);
    expect(testId("mcp-discover-results")?.getAttribute("aria-busy")).toBe("true");

    await act(async () => {
      slow.resolve(page(1, 1, ["notion"]));
    });
    expect(testId("mcp-discover-searching")).toBeNull();
    expect(names()).toEqual(["notion"]);
  });

  it("aborts a superseded search so a slow earlier answer never lands", async () => {
    registryApi.searchMcpRegistry.mockResolvedValueOnce(page(1, 1, ["a"]));
    await render("");
    await settle();

    const first = deferred<McpCatalogueSearch>();
    registryApi.searchMcpRegistry.mockReturnValueOnce(first.promise);
    await render("git");
    await settle();
    const firstSignal = registryApi.searchMcpRegistry.mock.calls.at(-1)?.[3]?.signal as AbortSignal;

    registryApi.searchMcpRegistry.mockResolvedValueOnce(page(1, 1, ["github"]));
    await render("github");
    await settle();
    expect(firstSignal.aborted).toBe(true);
    expect(names()).toEqual(["github"]);

    await act(async () => {
      first.resolve(page(1, 1, ["gitlab"]));
    });
    expect(names()).toEqual(["github"]);
  });

  it("shows the typed timeout copy with a Retry that searches again", async () => {
    registryApi.searchMcpRegistry
      .mockRejectedValueOnce(
        new ApiError(504, "registry_timeout", "The MCP directory is taking too long to search right now. Try again in a moment."),
      )
      .mockResolvedValueOnce(page(1, 1, ["notion"]));
    await render("notion");
    await settle();

    expect(testId("mcp-registry-error")?.textContent).toContain("Couldn't search for “notion”");
    expect(testId("mcp-registry-error")?.textContent).toContain("taking too long");
    expect(testId("mcp-registry-error")?.textContent).not.toContain("harness");

    await click("mcp-registry-retry");
    await settle();
    expect(names()).toEqual(["notion"]);
  });
});

describe("popular matches", () => {
  const timeout = () =>
    new ApiError(504, "registry_timeout", "The MCP directory is taking too long to search right now.");

  async function browse() {
    registryApi.searchMcpRegistry.mockResolvedValueOnce(page(1, 1, ["notion", "github"]));
    await render("");
    await settle();
  }

  it("shows matching popular rows at once while the live search runs, then merges", async () => {
    await browse();
    const slow = deferred<McpCatalogueSearch>();
    registryApi.searchMcpRegistry.mockReturnValueOnce(slow.promise);
    await render("Notion");
    await settle(0);

    expect(testId("mcp-discover-searching")?.textContent).toContain("Searching for “Notion”");
    expect(names()).toEqual(["notion"]);
    expect(testId("mcp-discover-results")?.getAttribute("aria-busy")).toBeNull();

    await settle();
    await act(async () => {
      slow.resolve(page(1, 1, ["notion-tools", "notion"]));
    });
    expect(names()).toEqual(["notion", "notion-tools"]);
  });

  it("keeps popular matches with a note and Retry when the search times out", async () => {
    await browse();
    registryApi.searchMcpRegistry
      .mockRejectedValueOnce(timeout())
      .mockResolvedValueOnce(page(1, 1, ["notion", "notion-tools"]));
    await render("notion");
    await settle();

    expect(names()).toEqual(["notion"]);
    expect(testId("mcp-discover-fallback")?.textContent).toContain("Showing popular matches");
    expect(testId("mcp-registry-error")).toBeNull();

    await click("mcp-discover-fallback-retry");
    expect(names()).toEqual(["notion"]);
    await settle();
    expect(names()).toEqual(["notion", "notion-tools"]);
    expect(testId("mcp-discover-fallback")).toBeNull();
  });

  it("shows the error state when no popular row matches", async () => {
    await browse();
    registryApi.searchMcpRegistry.mockRejectedValueOnce(timeout());
    await render("zomato");
    await settle();

    expect(testId("mcp-discover-fallback")).toBeNull();
    expect(testId("mcp-registry-error")?.textContent).toContain("Couldn't search for “zomato”");
  });

  it("shows the error state for a failure that is not the directory being slow", async () => {
    await browse();
    registryApi.searchMcpRegistry.mockRejectedValueOnce(new ApiError(400, "bad_request", "That query is not valid."));
    await render("notion");
    await settle();

    expect(testId("mcp-discover-fallback")).toBeNull();
    expect(testId("mcp-registry-error")?.textContent).toContain("That query is not valid.");
  });
});

describe("the detail dialog", () => {
  async function openDetail() {
    registryApi.searchMcpRegistry.mockResolvedValueOnce(page(1, 1, ["zomato"]));
    await render("");
    await settle();
    const card = testId("mcp-discover-card");
    await act(async () => {
      card?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    await settle(0);
  }

  it("will not install while the directory lookup has failed, and can retry it", async () => {
    registryApi.getMcpRegistryEntry
      .mockRejectedValueOnce(new ApiError(503, "registry_unavailable", "The MCP directory can't look up this server right now."))
      .mockResolvedValueOnce({ ...entry("zomato"), endpoint: "https://mcp.example/mcp", installable: true, requiredEnvKeys: [] });
    await openDetail();

    expect(testId("mcp-discover-detail-failed")?.textContent).toContain("can't look up");
    expect((testId("mcp-discover-detail-install") as HTMLButtonElement).disabled).toBe(true);

    await click("mcp-discover-detail-retry");
    await settle(0);
    expect(testId("mcp-discover-detail-failed")).toBeNull();
    expect((testId("mcp-discover-detail-install") as HTMLButtonElement).disabled).toBe(false);
  });

  it("closes once the install succeeds", async () => {
    registryApi.getMcpRegistryEntry.mockResolvedValue({
      ...entry("zomato"),
      endpoint: "https://mcp.example/mcp",
      installable: true,
      requiredEnvKeys: [],
    });
    onInstall.mockResolvedValueOnce(true);
    await openDetail();

    await click("mcp-discover-detail-install");
    await settle(0);
    expect(onInstall).toHaveBeenCalledTimes(1);
    expect(testId("mcp-discover-detail")).toBeNull();
  });

  it("stays open when the install fails", async () => {
    registryApi.getMcpRegistryEntry.mockResolvedValue({
      ...entry("zomato"),
      endpoint: "https://mcp.example/mcp",
      installable: true,
      requiredEnvKeys: [],
    });
    onInstall.mockResolvedValueOnce(false);
    await openDetail();

    await click("mcp-discover-detail-install");
    await settle(0);
    expect(testId("mcp-discover-detail")).not.toBeNull();
  });
});
