// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type McpHealth, type McpServer, type McpSource } from "@/api/types";

/**
 * Yours and Discover, kept apart.
 *
 * Yours searches only the servers this company has and never calls the
 * directory. Discover browses the directory before anything is typed, marks an
 * entry this company already holds instead of offering it again, and fails on
 * its own without taking this company's servers off screen.
 */

const api = vi.hoisted(() => ({
  listMcpServers: vi.fn(),
  testMcpServer: vi.fn(),
  discoverMcpTools: vi.fn(),
  addMcpServer: vi.fn(),
  removeMcpServer: vi.fn(),
  updateMcpServer: vi.fn(),
  startMcpOAuth: vi.fn(),
}));

const registryApi = vi.hoisted(() => ({
  connectMcpRegistryServer: vi.fn(),
  disconnectMcpRegistryServer: vi.fn(),
  getMcpRegistryEntry: vi.fn(),
  installMcpRegistryEntry: vi.fn(),
  searchMcpRegistry: vi.fn(),
  uninstallMcpRegistryServer: vi.fn(),
  updateMcpRegistryEnv: vi.fn(),
}));

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  message: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));

vi.mock("@/api/mcp", () => api);
vi.mock("@/api/mcp-registry", () => registryApi);
vi.mock("sonner", () => ({
  toast: Object.assign(toasts.base, {
    success: toasts.success,
    error: toasts.error,
    message: toasts.message,
    warning: toasts.warning,
    info: toasts.info,
  }),
}));

const { McpServersSection } = await import(
  "@/views/connections/McpServersSection"
);

function row(over: Partial<McpServer> & { source: McpSource }): McpServer {
  return {
    name: "linear",
    endpoint: "https://mcp.linear.app/mcp",
    enabled: true,
    allowedTools: [],
    disallowedTools: [],
    readOnlyTools: [],
    timeoutSecs: 30,
    authConfigured: false,
    ...over,
  };
}

const OK: McpHealth = {
  status: "ok",
  message: "",
  toolCount: 9,
  checkedAtMillis: 1,
};

function entry(over: { qualifiedName: string; displayName: string } & Record<string, unknown>) {
  return {
    description: "Read and update issues.",
    source: "mcp_official",
    official: true,
    useCount: 0,
    ...over,
  };
}

const LINEAR = entry({ qualifiedName: "@linear/mcp", displayName: "Linear" });
const GITHUB = entry({
  qualifiedName: "io.github.github/github-mcp-server",
  displayName: "GitHub",
});

const client = {
  capabilityStatus: () => Promise.resolve({ mcpInBuild: true }),
} as unknown as OpenCompanyClient;

let container: HTMLDivElement;
let root: Root;

function all(selector: string): HTMLElement[] {
  return [...document.body.querySelectorAll<HTMLElement>(selector)];
}

function testId(id: string): HTMLElement | null {
  return document.body.querySelector<HTMLElement>(`[data-testid="${id}"]`);
}

async function settle() {
  await act(async () => {
    vi.advanceTimersByTime(400);
    await Promise.resolve();
  });
  await act(async () => {
    await Promise.resolve();
  });
}

async function mount(servers: McpServer[], canManage = true) {
  api.listMcpServers.mockResolvedValue(servers);
  await act(async () => {
    root.render(
      createElement(McpServersSection, {
        client,
        company: "acme",
        canManage,
        chrome: "standalone" as const,
      }),
    );
  });
  await settle();
}

async function click(id: string) {
  const node = testId(id);
  if (!node) throw new Error(`no ${id}`);
  await act(async () => {
    node.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
  await settle();
}

async function type(id: string, term: string) {
  const field = document.body.querySelector<HTMLInputElement>(`[data-testid="${id}"]`);
  if (!field) throw new Error(`no ${id}`);
  const setter = Object.getOwnPropertyDescriptor(
    window.HTMLInputElement.prototype,
    "value",
  )?.set;
  await act(async () => {
    setter?.call(field, term);
    field.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await settle();
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  vi.clearAllMocks();
  vi.useFakeTimers({ shouldAdvanceTime: true });
  window.location.hash = "";
  window.localStorage.clear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
});

describe("Yours", () => {
  it("searches only this company's servers and never calls the directory", async () => {
    await mount([
      { ...row({ source: "runtime", name: "linear" }), health: OK },
      { ...row({ source: "runtime", name: "notion", endpoint: "https://mcp.notion.com/mcp" }), health: OK },
    ]);

    await type("mcp-search", "lin");

    expect(registryApi.searchMcpRegistry).not.toHaveBeenCalled();
    expect(all('[data-testid="mcp-server-row"]')).toHaveLength(1);
    expect(testId("mcp-discover")).toBeNull();
  });

  it("hands a search with no match to the directory, carrying the term", async () => {
    registryApi.searchMcpRegistry.mockResolvedValue({ page: 1, totalPages: 1, servers: [LINEAR] });
    await mount([{ ...row({ source: "runtime", name: "notion", endpoint: "https://mcp.notion.com/mcp" }), health: OK }]);

    await type("mcp-search", "linear");
    expect(testId("mcp-search-nothing")?.textContent).toContain("None of your servers match");

    await click("mcp-search-directory");

    expect(testId("mcp-mode-discover")?.getAttribute("aria-pressed")).toBe("true");
    expect(registryApi.searchMcpRegistry).toHaveBeenLastCalledWith(client, "acme", {
      q: "linear",
      page: 1,
      pageSize: 20,
    }, expect.objectContaining({ signal: expect.any(AbortSignal) }));
  });
});

describe("a member", () => {
  it("can still read mcp.json but is offered no way to add a server", async () => {
    await mount([{ ...row({ source: "runtime", name: "linear" }), health: OK }], false);

    expect(testId("mcp-json-open")?.getAttribute("title")).toBe("View mcp.json");
    expect(testId("mcp-add-open")).toBeNull();
  });
});

describe("Discover", () => {
  it("shows top connectors before anything is typed", async () => {
    registryApi.searchMcpRegistry.mockResolvedValue({
      page: 1,
      totalPages: 1,
      servers: [LINEAR, GITHUB],
    });
    await mount([{ ...row({ source: "runtime" }), health: OK }]);

    await click("mcp-mode-discover");

    expect(registryApi.searchMcpRegistry).toHaveBeenCalledWith(client, "acme", {
      q: undefined,
      page: 1,
      pageSize: 20,
    }, expect.objectContaining({ signal: expect.any(AbortSignal) }));
    expect(testId("mcp-discover")?.textContent).toContain("Top connectors");
    expect(all('[data-testid="mcp-discover-card"]')).toHaveLength(2);
    expect(all('[data-testid="mcp-discover-verified"]')).toHaveLength(2);
    expect(testId("mcp-discover")?.textContent).toContain("by github");
  });

  it("marks an entry this company already holds and offers no install for it", async () => {
    registryApi.searchMcpRegistry.mockResolvedValue({
      page: 1,
      totalPages: 1,
      servers: [LINEAR, GITHUB],
    });
    await mount([
      {
        ...row({ source: "registry", serverId: "srv_1", qualifiedName: "@linear/mcp" }),
        health: OK,
      },
    ]);

    await click("mcp-mode-discover");

    expect(all('[data-testid="mcp-discover-installed"]')).toHaveLength(1);
    const offers = all('[data-testid="mcp-discover-install"]');
    expect(offers).toHaveLength(1);
    expect(offers[0]?.getAttribute("aria-label")).toBe("Install GitHub");
  });

  it("marks a Discover install saved under the slug of its shown name", async () => {
    registryApi.searchMcpRegistry.mockResolvedValue({
      page: 1,
      totalPages: 1,
      servers: [LINEAR, GITHUB],
    });
    await mount([{ ...row({ source: "runtime", name: "github" }), health: OK }]);

    await click("mcp-mode-discover");

    const offers = all('[data-testid="mcp-discover-install"]');
    expect(offers).toHaveLength(1);
    expect(offers[0]?.getAttribute("aria-label")).toBe("Install Linear");
  });

  it("searches the directory with its own field", async () => {
    registryApi.searchMcpRegistry.mockResolvedValue({ page: 1, totalPages: 1, servers: [GITHUB] });
    await mount([{ ...row({ source: "runtime" }), health: OK }]);
    await click("mcp-mode-discover");

    await type("mcp-discover-search", "github");

    expect(registryApi.searchMcpRegistry).toHaveBeenLastCalledWith(client, "acme", {
      q: "github",
      page: 1,
      pageSize: 20,
    }, expect.objectContaining({ signal: expect.any(AbortSignal) }));
    expect(testId("mcp-discover")?.textContent).toContain("Results");
  });

  it("appends the next page on Show more", async () => {
    registryApi.searchMcpRegistry
      .mockResolvedValueOnce({ page: 1, totalPages: 2, servers: [LINEAR] })
      .mockResolvedValueOnce({ page: 2, totalPages: 2, servers: [GITHUB] });
    await mount([{ ...row({ source: "runtime" }), health: OK }]);
    await click("mcp-mode-discover");

    await click("mcp-discover-more");

    expect(all('[data-testid="mcp-discover-card"]')).toHaveLength(2);
    expect(testId("mcp-discover-more")).toBeNull();
  });

  it("leaves this company's servers on screen through a directory outage", async () => {
    registryApi.searchMcpRegistry.mockRejectedValue(
      new ApiError(502, "upstream", "The registry returned 502.", true),
    );
    await mount([{ ...row({ source: "runtime", name: "linear" }), health: OK }]);

    await click("mcp-mode-discover");
    expect(all('[data-testid="mcp-registry-error"]')).toHaveLength(1);
    expect(all('[data-testid="mcp-load-error"]')).toHaveLength(0);

    await click("mcp-mode-yours");
    expect(all('[data-testid="mcp-server-row"]')).toHaveLength(1);
  });

  it("reads a build without the feature as a missing feature, not an error", async () => {
    registryApi.searchMcpRegistry.mockRejectedValue(new ApiError(404, "not_wired", "", true));
    await mount([{ ...row({ source: "runtime" }), health: OK }]);

    await click("mcp-mode-discover");

    expect(all('[data-testid="mcp-registry-unwired"]')).toHaveLength(1);
    expect(all('[data-testid="mcp-registry-error"]')).toHaveLength(0);
  });
});

describe("the card / list toggle", () => {
  it("defaults Yours to a list and Discover to cards", async () => {
    registryApi.searchMcpRegistry.mockResolvedValue({ page: 1, totalPages: 1, servers: [LINEAR] });
    await mount([{ ...row({ source: "runtime" }), health: OK }]);

    expect(all('[data-testid="mcp-server-row"]')).toHaveLength(1);
    expect(all('[data-testid="mcp-server-card"]')).toHaveLength(0);

    await click("mcp-mode-discover");
    expect(all('[data-testid="mcp-discover-card"]')).toHaveLength(1);
    expect(all('[data-testid="mcp-discover-row"]')).toHaveLength(0);
  });

  it("switches each tab on its own and remembers the choice", async () => {
    registryApi.searchMcpRegistry.mockResolvedValue({ page: 1, totalPages: 1, servers: [LINEAR] });
    await mount([{ ...row({ source: "runtime" }), health: OK }]);

    await click("mcp-layout-cards");
    expect(all('[data-testid="mcp-server-card"]')).toHaveLength(1);

    await click("mcp-mode-discover");
    expect(all('[data-testid="mcp-discover-card"]')).toHaveLength(1);

    act(() => root.unmount());
    root = createRoot(container);
    window.location.hash = "";
    await mount([{ ...row({ source: "runtime" }), health: OK }]);
    expect(all('[data-testid="mcp-server-card"]')).toHaveLength(1);
  });

  it("keeps the search term when the layout changes", async () => {
    await mount([
      { ...row({ source: "runtime", name: "linear" }), health: OK },
      { ...row({ source: "runtime", name: "notion", endpoint: "https://mcp.notion.com/mcp" }), health: OK },
    ]);
    await type("mcp-search", "lin");

    await click("mcp-layout-cards");

    expect(
      document.body.querySelector<HTMLInputElement>('[data-testid="mcp-search"]')?.value,
    ).toBe("lin");
    expect(all('[data-testid="mcp-server-card"]')).toHaveLength(1);
  });
});

describe("installing from Discover", () => {
  it("lands on a connected confirmation for the new server", async () => {
    registryApi.searchMcpRegistry.mockResolvedValue({ page: 1, totalPages: 1, servers: [GITHUB] });
    const installed = {
      ...row({
        source: "registry",
        name: "github",
        serverId: "srv_gh",
        qualifiedName: "io.github.github/github-mcp-server",
        endpoint: "https://api.githubcopilot.com/mcp/",
      }),
      health: { ...OK, toolCount: 5 },
    };
    registryApi.installMcpRegistryEntry.mockResolvedValue({
      server: installed,
      note: "Installed.",
      test: { ...OK, toolCount: 5 },
    });
    api.listMcpServers
      .mockResolvedValueOnce([{ ...row({ source: "runtime" }), health: OK }])
      .mockResolvedValue([{ ...row({ source: "runtime" }), health: OK }, installed]);
    await act(async () => {
      root.render(
        createElement(McpServersSection, {
          client,
          company: "acme",
          canManage: true,
          chrome: "standalone" as const,
        }),
      );
    });
    await settle();
    await click("mcp-mode-discover");

    await click("mcp-discover-install");

    expect(registryApi.installMcpRegistryEntry).toHaveBeenCalledWith(client, "acme", {
      qualifiedName: "io.github.github/github-mcp-server",
    });
    expect(testId("mcp-connect-done")?.textContent).toContain("Connected · 5 tools");
    expect(all('[data-testid="mcp-discover-installed"]')).toHaveLength(1);
  });
});
