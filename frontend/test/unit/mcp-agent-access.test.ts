// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { McpAgentAccess as Access, McpServer } from "@/api/types";
import { canToggle, grantedNow, lockedReason, nextTools } from "@/lib/mcp-scope";
import { McpAgentAccess } from "@/views/mcp/McpAgentAccess";

const updateAgent = vi.fn();
const client = { updateAgent } as unknown as OpenCompanyClient;
const onSaved = vi.fn();

const INHERITS: Access = {
  id: "ceo",
  name: "Chief Executive",
  state: "inherited",
  reaches: true,
  revokeTools: ["search", "mcp:linear"],
};
const LEFT_OUT: Access = {
  id: "writer",
  name: "Writer",
  state: "excluded",
  reaches: false,
  grantTools: ["search", "mcp:notion"],
};
const BLOCKED: Access = { id: "hermit", name: "Hermit", state: "blocked", reaches: false };
const BROAD: Access = { id: "ops", name: "Ops", state: "inherited", reaches: true };

function server(over: Partial<McpServer> = {}): McpServer {
  return {
    name: "notion",
    endpoint: "https://mcp.notion.com/mcp",
    source: "runtime",
    enabled: true,
    allowedTools: [],
    disallowedTools: [],
    readOnlyTools: [],
    timeoutSecs: 30,
    authConfigured: true,
    accessGrant: "mcp:notion",
    agentAccess: [INHERITS, LEFT_OUT, BLOCKED, BROAD],
    ...over,
  };
}

let container: HTMLDivElement;
let root: Root;

function testId(id: string): HTMLElement | null {
  return document.body.querySelector<HTMLElement>(`[data-testid="${id}"]`);
}

async function click(id: string) {
  const node = testId(id);
  if (!node) throw new Error(`no ${id}`);
  await act(async () => {
    node.click();
  });
}

async function mount(over: Partial<McpServer> = {}, canManage = true) {
  await act(async () => {
    root.render(
      createElement(McpAgentAccess, {
        client,
        company: "acme",
        server: server(over),
        canManage,
        onSaved,
      }),
    );
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  vi.resetAllMocks();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("mcp-scope", () => {
  it("ticks inherited and included teammates and nothing else", () => {
    expect(grantedNow(INHERITS)).toBe(true);
    expect(grantedNow(LEFT_OUT)).toBe(false);
    expect(grantedNow(BLOCKED)).toBe(false);
  });

  it("sends the host's list for the direction the box moved", () => {
    expect(nextTools(INHERITS, false)).toEqual(["search", "mcp:linear"]);
    expect(nextTools(LEFT_OUT, true)).toEqual(["search", "mcp:notion"]);
    expect(nextTools(INHERITS, true)).toBeUndefined();
  });

  it("locks a blocked teammate and one reached through a broad grant", () => {
    expect(canToggle(BLOCKED)).toBe(false);
    expect(lockedReason(BLOCKED, "mcp:notion")).toContain("mcp:notion");
    expect(canToggle(BROAD)).toBe(false);
    expect(lockedReason(BROAD, "mcp:notion")).toContain("broad tool grant");
    expect(lockedReason(INHERITS, "mcp:notion")).toBeNull();
  });
});

describe("McpAgentAccess", () => {
  it("lists every teammate with its access and greys a blocked one with a Tools link", async () => {
    await mount();
    expect(testId("mcp-access-agent-toggle-ceo")).toHaveProperty("checked", true);
    expect(testId("mcp-access-agent-toggle-writer")).toHaveProperty("checked", false);
    expect(testId("mcp-access-agent-toggle-hermit")).toHaveProperty("disabled", true);
    expect(testId("mcp-access-agent-reason-hermit")?.textContent).toContain("mcp:notion");
    expect(testId("mcp-access-tools-link-hermit")?.getAttribute("href")).toContain("?tab=tools");
    expect(testId("mcp-access-agent-reach-ceo")?.textContent).toBe("Has access");
  });

  it("adds one teammate and removes an inheriting one with a warning, then saves both", async () => {
    updateAgent.mockResolvedValue({});
    await mount();
    await click("mcp-access-agent-toggle-writer");
    expect(testId("mcp-agent-access-pin-warning")).toBeNull();
    await click("mcp-access-agent-toggle-ceo");
    expect(testId("mcp-agent-access-pin-warning")).not.toBeNull();

    await click("mcp-agent-access-save");
    expect(updateAgent.mock.calls).toEqual([
      ["ceo", { tools: ["search", "mcp:linear"] }, "acme"],
      ["writer", { tools: ["search", "mcp:notion"] }, "acme"],
    ]);
    expect(onSaved).toHaveBeenCalledTimes(1);
    expect(testId("mcp-agent-access-saved")).not.toBeNull();
  });

  it("stops at the first refused write and says which teammate failed", async () => {
    updateAgent.mockRejectedValueOnce(new Error("forbidden"));
    await mount();
    await click("mcp-access-agent-toggle-writer");
    await click("mcp-access-agent-toggle-ceo");
    await click("mcp-agent-access-save");
    expect(updateAgent).toHaveBeenCalledTimes(1);
    expect(testId("mcp-agent-access-problem")?.textContent).toContain("Failed on Chief Executive: forbidden");
  });

  it("is read-only for a member", async () => {
    await mount({}, false);
    expect(testId("mcp-agent-access-read-only")).not.toBeNull();
    expect(testId("mcp-access-agent-toggle-ceo")).toBeNull();
    expect(testId("mcp-agent-access-save")).toBeNull();
  });
});
