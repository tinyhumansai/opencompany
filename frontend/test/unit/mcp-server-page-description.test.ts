// @vitest-environment jsdom

// `McpDescription.save()` calls `updateMcpServer` and then only flips out of
// edit mode — it never updates what is rendered, and nothing here reloads the
// `server` prop. Without carrying the saved value in local state, the view
// keeps showing the description that was true before the save, so a
// successful save reads to the operator as though it failed.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { McpServer } from "@/api/types";

const api = vi.hoisted(() => ({
  updateMcpServer: vi.fn(),
}));

vi.mock("@/api/mcp", () => api);
vi.mock("@/views/mcp/McpToolPermissions", () => ({
  McpToolPermissions: () => null,
}));
vi.mock("@/views/connections/connection-usage", () => ({
  UsageSection: () => null,
  useConnectionUsage: () => ({ load: "unavailable", calls: null, key: null }),
}));

const { McpServerPage } = await import("@/views/mcp/McpServerPage");

function server(over: Partial<McpServer> = {}): McpServer {
  return {
    name: "notion",
    source: "runtime",
    endpoint: "https://mcp.notion.com/mcp",
    enabled: true,
    allowedTools: [],
    disallowedTools: [],
    readOnlyTools: [],
    timeoutSecs: 30,
    authConfigured: false,
    description: "Our own reporting replica.",
    ...over,
  };
}

const client = {} as unknown as OpenCompanyClient;

let container: HTMLDivElement;
let root: Root;

function testId(id: string) {
  return document.body.querySelector(`[data-testid="${id}"]`);
}

async function click(node: Element | null | undefined) {
  if (!node) throw new Error("nothing to click");
  await act(async () => {
    node.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

async function mount(over: Partial<McpServer> = {}) {
  await act(async () => {
    root.render(
      createElement(McpServerPage, {
        client,
        company: "acme",
        server: server(over),
        health: undefined,
        canManage: true,
        bridge: "present" as const,
        approvalsPark: true,
        agents: [],
        reloadKey: 0,
        focusPermissions: false,
        primary: null,
        busy: null,
        onPrimary: () => {},
        onDisconnect: null,
        onBack: () => {},
        onAccessSaved: () => {},
      }),
    );
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
});

describe("editing a server's description", () => {
  it("shows what was just saved, not the description from before the edit", async () => {
    api.updateMcpServer.mockResolvedValue({ server: server(), note: "" });

    await mount({ description: "Our own reporting replica." });

    expect(testId("mcp-page-description")?.textContent).toContain(
      "Our own reporting replica.",
    );

    await click(testId("mcp-page-describe"));
    const textarea = document.body.querySelector("textarea");
    expect(textarea).not.toBeNull();
    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLTextAreaElement.prototype,
      "value",
    )?.set;
    await act(async () => {
      setter?.call(textarea, "Search, read and update pages across Notion.");
      textarea?.dispatchEvent(new Event("input", { bubbles: true }));
    });

    const saveButtons = [...document.body.querySelectorAll("button")].filter(
      (b) => b.textContent?.trim() === "Save",
    );
    await click(saveButtons[0]);

    expect(api.updateMcpServer).toHaveBeenCalledWith(client, "acme", "notion", {
      description: "Search, read and update pages across Notion.",
    });
    // Back in the read view, showing the value just saved — not the stale
    // `server.description` prop, which the parent has not refreshed.
    expect(testId("mcp-page-description-edit")).toBeNull();
    expect(testId("mcp-page-description")?.textContent).toContain(
      "Search, read and update pages across Notion.",
    );
    expect(testId("mcp-page-description")?.textContent).not.toContain(
      "Our own reporting replica.",
    );
  });
});
