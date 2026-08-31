// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CapabilityStatusDto, UsageDto } from "@/api/types";
import { UsageView } from "@/views/UsageView";

const USAGE: UsageDto = {
  series: [],
  byAgent: [],
  byProvider: [],
  totals: {
    inputTokens: 20,
    outputTokens: 10,
    tokens: 30,
    costUsd: 0.03,
    oauthCalls: 2,
    connections: 1,
    searchCalls: 1,
  },
};

const CAPS: CapabilityStatusDto = { configured: false };

function clientWith({ usage = USAGE, caps = CAPS }: { usage?: UsageDto | Error; caps?: CapabilityStatusDto | Error }) {
  return {
    usage: vi.fn(() => (usage instanceof Error ? Promise.reject(usage) : Promise.resolve(usage))),
    capabilityStatus: vi.fn(() => (caps instanceof Error ? Promise.reject(caps) : Promise.resolve(caps))),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(UsageView, { client, company: "acme" }));
  });
  await act(async () => {});
}

function at(testid: string): HTMLElement | null {
  return container.querySelector(`[data-testid="${testid}"]`);
}

describe("UsageView load failures", () => {
  it("keeps rejected usage unavailable rather than rendering zero spend", async () => {
    await show(clientWith({ usage: new Error("usage unavailable") }));

    expect(at("usage-load-error")?.textContent).toContain("Couldn't check usage");
    expect(container.textContent).toContain("Total tokens—");
    expect(container.textContent).not.toContain("$0.00");
  });

  it("keeps rejected capability status unavailable rather than rendering ungranted rows", async () => {
    await show(clientWith({ caps: new Error("capabilities unavailable") }));

    expect(at("usage-capabilities-load-error")?.textContent).toContain(
      "Couldn't check capability status",
    );
    expect(container.textContent).not.toContain("No token plan configured.");
    expect(container.textContent).not.toContain("Not granted");
  });

  it("renders a successful empty usage and unconfigured plan distinctly", async () => {
    await show(clientWith({}));

    expect(at("usage-load-error")).toBeNull();
    expect(container.textContent).toContain("Total tokens30");
    expect(container.textContent).toContain("No token plan configured.");
  });
});
