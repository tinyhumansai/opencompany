// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { EngineProbe, MemoryEngineState } from "@/api/memory";
import { EngineSection } from "@/views/memory/EngineSection";

/**
 * MEM-002's backend half (`memory_engine.rs`) is a test-then-apply pair: a
 * probe that must never persist, and an apply that does. The console side of
 * that contract is `EngineSection` — "Test connection" hits `POST
 * …/memory/engine/test`, "Use this engine" hits `PUT …/memory/engine` — and
 * nothing here can prove the host's own race safety, but the console's own
 * half of the contract is fully testable: a probe must never be the call that
 * changes what is bound, and the bound state on screen must not move until an
 * apply actually answers.
 */

const STATE: MemoryEngineState = {
  active: "store",
  capabilities: [],
  selected: "store",
  apiKeySet: false,
  layer: "config.toml",
  editable: true,
  configPath: "/data/config.toml",
  options: [
    { id: "store", label: "Store", description: "Built-in.", available: true, requiresUrl: false, acceptsUrl: false, requiresKey: false, durable: true },
    {
      id: "cortexdb",
      label: "Supermemory",
      description: "Hosted.",
      available: true,
      requiresUrl: true, acceptsUrl: true,
      requiresKey: true,
      durable: true,
    },
  ],
};

function clientWith(
  probe: EngineProbe,
): { client: OpenCompanyClient; put: ReturnType<typeof vi.fn>; test: ReturnType<typeof vi.fn> } {
  const put = vi.fn(() => Promise.reject(new Error("apply should not be called by a probe")));
  const test = vi.fn((_body: unknown) => Promise.resolve(probe));
  const client = {
    scopeFor: () => "/api/v1/company/acme",
    get: vi.fn(() => Promise.resolve(STATE)),
    post: (path: string, body: unknown) => {
      if (path.endsWith("/memory/engine/test")) return test(body);
      return Promise.reject(new Error(`unexpected POST ${path}`));
    },
    put,
  } as unknown as OpenCompanyClient;
  return { client, put, test };
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(EngineSection, { client, company: "acme" }));
  });
}

function tile(id: string): HTMLElement | null {
  return container.querySelector(`[data-testid="engine-tile-${id}"]`);
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

describe("the probe path never binds anything", () => {
  it("tests a candidate engine without applying it, whatever the probe answers", async () => {
    const { client, put, test } = clientWith({ healthy: true, capabilities: ["core"] });
    await show(client);

    await act(async () => {
      tile("cortexdb")!.click();
    });
    const testButton = Array.from(container.querySelectorAll("button")).find(
      (b) => b.textContent?.trim() === "Test connection",
    ) as HTMLButtonElement;
    await act(async () => {
      testButton.click();
    });

    // A healthy probe still must not have bound anything: the standing engine
    // on screen is unchanged, and the write route was never called — but the
    // click must have actually reached the probe route for the candidate,
    // not just left the screen looking untouched.
    expect(test).toHaveBeenCalledTimes(1);
    expect(test).toHaveBeenCalledWith(expect.objectContaining({ engine: "cortexdb" }));
    expect(put).not.toHaveBeenCalled();
    expect(container.querySelector('[data-testid="memory-engine-health"]')?.textContent).toContain(
      "store",
    );
  });

  it("leaves the bound engine on screen unchanged after a failed probe too", async () => {
    const { client, put, test } = clientWith({
      healthy: false,
      capabilities: [],
      detail: "connection refused",
    });
    await show(client);

    await act(async () => {
      tile("cortexdb")!.click();
    });
    const testButton = Array.from(container.querySelectorAll("button")).find(
      (b) => b.textContent?.trim() === "Test connection",
    ) as HTMLButtonElement;
    await act(async () => {
      testButton.click();
    });

    expect(test).toHaveBeenCalledTimes(1);
    expect(test).toHaveBeenCalledWith(expect.objectContaining({ engine: "cortexdb" }));
    expect(put).not.toHaveBeenCalled();
    expect(container.querySelector('[data-testid="memory-engine-health"]')?.textContent).toContain(
      "store",
    );
  });
});
