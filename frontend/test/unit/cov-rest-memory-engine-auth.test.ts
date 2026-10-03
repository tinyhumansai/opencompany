// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import type { EngineOption, MemoryEngineState } from "@/api/memory";
import { EngineSection } from "@/views/memory/EngineSection";

/**
 * The three engine routes (`GET/POST/PUT …/memory/engine*`) are all
 * `AdminScopedCompany` (`memory_engine.rs:438,567,681`) — unlike the fact CRUD
 * beside it on the same page. `EngineSection` carries no `canManage` prop of
 * its own; it happens to land in the right place anyway because its very
 * first read is that same admin-gated route, and a 403 there renders the
 * section's own error state before the picker — with its Test/Apply buttons
 * — ever mounts. This pins that the accident holds: a member never reaches
 * the buttons, and an admin gets both. It also pins the one thing a frontend
 * test can prove about the test/apply split named in the audit: the probe
 * button calls only the probe route, never the persisting one.
 */

const OPTIONS: EngineOption[] = [
  {
    id: "cortexdb",
    label: "Supermemory",
    description: "A hosted engine.",
    available: true,
    requiresUrl: true, acceptsUrl: true,
    requiresKey: true,
    durable: true,
  },
];

function state(): MemoryEngineState {
  return {
    active: "cortexdb",
    capabilities: [],
    selected: "cortexdb",
    apiKeySet: false,
    layer: "default",
    editable: true,
    configPath: "config.toml",
    options: OPTIONS,
  };
}

let container: HTMLDivElement;
let root: Root;

async function show(element: React.ReactElement) {
  await act(async () => {
    root.render(element);
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

describe("EngineSection, a member whose read the host actually refuses", () => {
  it("shows the refusal, not a live picker with buttons the write will 403 on", async () => {
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get: () => Promise.reject(new ApiError(403, "forbidden", "only an admin can do that")),
    } as unknown as OpenCompanyClient;
    await show(createElement(EngineSection, { client, company: "acme" }));

    expect(container.textContent).toContain("only an admin can do that");
    expect(
      Array.from(container.querySelectorAll("button")).find((b) =>
        b.textContent?.includes("Test connection"),
      ),
    ).toBeUndefined();
  });
});

describe("EngineSection, an admin whose read the host answers", () => {
  it("gets the picker, Test connection and all", async () => {
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get: () => Promise.resolve(state()),
    } as unknown as OpenCompanyClient;
    await show(createElement(EngineSection, { client, company: "acme" }));

    expect(
      Array.from(container.querySelectorAll("button")).find((b) =>
        b.textContent?.includes("Test connection"),
      ),
    ).toBeDefined();
  });
});

describe("the probe/persist split", () => {
  it("clicking Test connection calls only the probe route, never the one that binds it", async () => {
    const testEngine = vi.fn((_body: unknown) => Promise.resolve({ healthy: true, capabilities: [] }));
    const applyEngine = vi.fn((_body: unknown) => Promise.resolve({ engine: "embedded" }));
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get: () => Promise.resolve(state()),
      post: (path: string, body: unknown) => {
        if (path.endsWith("/memory/engine/test")) return testEngine(body);
        return Promise.reject(new Error(`unexpected POST ${path}`));
      },
      put: (path: string, body: unknown) => {
        if (path.endsWith("/memory/engine")) return applyEngine(body);
        return Promise.reject(new Error(`unexpected PUT ${path}`));
      },
    } as unknown as OpenCompanyClient;
    await show(createElement(EngineSection, { client, company: "acme" }));

    await act(async () => {
      Array.from(container.querySelectorAll("button"))
        .find((b) => b.textContent?.includes("Test connection"))!
        .click();
    });

    expect(testEngine).toHaveBeenCalledTimes(1);
    expect(applyEngine).not.toHaveBeenCalled();
  });
});
