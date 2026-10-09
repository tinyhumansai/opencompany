// @vitest-environment jsdom

import { act, createElement, StrictMode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { McpHealth, McpServer, McpSource } from "@/api/types";

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
vi.mock("@/views/connections/McpRegistryBrowser", () => ({
  useMcpDirectorySearch: () => ({ kind: "idle" }),
}));

const { McpServersSection } = await import(
  "@/views/connections/McpServersSection"
);

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

const NEEDS_SIGN_IN: McpHealth = {
  status: "needs_config",
  authHint: "oauth_required",
  message: "needs a browser sign-in",
  toolCount: 0,
  checkedAtMillis: 1,
};

const client = {
  capabilityStatus: () => Promise.resolve({ mcpInBuild: true }),
} as unknown as OpenCompanyClient;

let container: HTMLDivElement;
let root: Root;

async function mount(servers: McpServer[]) {
  api.listMcpServers.mockResolvedValue(servers);
  await act(async () => {
    root.render(
      createElement(
        StrictMode,
        null,
        createElement(McpServersSection, {
          client,
          company: "acme",
          canManage: true,
          chrome: "standalone" as const,
        }),
      ),
    );
  });
}

function testId(id: string) {
  return document.body.querySelector(`[data-testid="${id}"]`);
}

async function click(node: Element | null | undefined) {
  if (!node) throw new Error("nothing to click");
  await act(async () => {
    node.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  vi.clearAllMocks();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
});

describe("browser sign-in under StrictMode", () => {
  const OK: McpHealth = { ...NEEDS_SIGN_IN, status: "ok", message: "", toolCount: 3 };

  beforeEach(() => {
    vi.useFakeTimers();
    api.startMcpOAuth.mockResolvedValue({
      authorizeUrl: "https://auth.example.com/authorize",
    });
  });

  it("keeps probing after the double mount and toasts when the server connects", async () => {
    api.testMcpServer
      .mockResolvedValueOnce(NEEDS_SIGN_IN)
      .mockResolvedValueOnce(NEEDS_SIGN_IN)
      .mockResolvedValueOnce(OK);
    await mount([{ ...row({ source: "runtime" }), health: NEEDS_SIGN_IN }]);

    await click(testId("mcp-sign-in"));
    expect(testId("mcp-signin-flight")).not.toBeNull();
    expect(testId("mcp-signin-checked")?.textContent).toContain("waiting");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_000);
    });
    expect(api.testMcpServer).toHaveBeenCalledTimes(1);
    expect(testId("mcp-signin-checked")?.textContent).toContain("checked 0s ago");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(3_000);
    });
    expect(testId("mcp-signin-checked")?.textContent).toContain("checked 1s ago");

    api.listMcpServers.mockResolvedValue([
      { ...row({ source: "runtime" }), health: OK },
    ]);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_000);
    });
    expect(toasts.success).toHaveBeenCalledWith("Connected to notion · 3 tools");
    expect(testId("mcp-signin-flight")).toBeNull();
    expect(testId("mcp-connect-done")).not.toBeNull();
  });

  it("re-checks as soon as the operator comes back to the tab", async () => {
    api.testMcpServer.mockResolvedValue(OK);
    await mount([{ ...row({ source: "runtime" }), health: NEEDS_SIGN_IN }]);
    await click(testId("mcp-sign-in"));
    expect(api.testMcpServer).not.toHaveBeenCalled();

    await act(async () => {
      window.dispatchEvent(new Event("focus"));
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(api.testMcpServer).toHaveBeenCalledTimes(1);
    expect(toasts.success).toHaveBeenCalledTimes(1);
  });

  it("shows the timed-out state with Check now and Try again", async () => {
    api.testMcpServer.mockResolvedValue(NEEDS_SIGN_IN);
    await mount([{ ...row({ source: "runtime" }), health: NEEDS_SIGN_IN }]);
    await click(testId("mcp-sign-in"));

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5 * 60_000 + 4_000);
    });
    expect(testId("mcp-signin-timed-out")).not.toBeNull();
    const probes = api.testMcpServer.mock.calls.length;

    api.testMcpServer.mockResolvedValueOnce(OK);
    await click(testId("mcp-signin-check"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(api.testMcpServer).toHaveBeenCalledTimes(probes + 1);
    expect(toasts.success).toHaveBeenCalledTimes(1);
    expect(testId("mcp-signin-flight")).toBeNull();
  });

  it("starts a fresh sign-in from Try again after a timeout", async () => {
    api.testMcpServer.mockResolvedValue(NEEDS_SIGN_IN);
    await mount([{ ...row({ source: "runtime" }), health: NEEDS_SIGN_IN }]);
    await click(testId("mcp-sign-in"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5 * 60_000 + 4_000);
    });

    await click(testId("mcp-signin-retry"));
    expect(api.startMcpOAuth).toHaveBeenCalledTimes(2);
    expect(testId("mcp-signin-timed-out")).toBeNull();
    expect(testId("mcp-signin-flight")).not.toBeNull();
  });
});
