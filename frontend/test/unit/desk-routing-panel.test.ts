// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { DeskRoutingDto } from "@/api/types";
import { DeskRoutingPanel } from "@/views/company/routing/DeskRoutingPanel";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let host: HTMLDivElement;
let root: Root;
beforeEach(() => {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});
afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

function dto(over: Partial<DeskRoutingDto> = {}): DeskRoutingDto {
  return {
    deskId: "engineering",
    source: "manifest",
    declared: { round_width: 2 },
    effective: {
      roundWidth: 2,
      choiceOptionLimit: 8,
      maxRounds: 6,
      turnTimeoutSecs: 600,
      router: "fallback",
    },
    candidates: [
      { agentId: "engineer", label: "Engineer", role: "Builds", sharedWith: [] },
      { agentId: "ceo", label: "CEO", role: "Runs", sharedWith: ["content"] },
    ],
    ...over,
  };
}

function client(initial: DeskRoutingDto) {
  const getDeskRouting = vi.fn().mockResolvedValue(initial);
  const putDeskRouting = vi.fn().mockImplementation((_desk: string, declared: DeskRoutingDto["declared"]) =>
    Promise.resolve(dto({ source: "overlay", declared })),
  );
  const resetDeskRouting = vi.fn().mockResolvedValue(dto({ source: "manifest" }));
  return {
    stub: { getDeskRouting, putDeskRouting, resetDeskRouting } as unknown as OpenCompanyClient,
    getDeskRouting,
    putDeskRouting,
    resetDeskRouting,
  };
}

async function render(stub: OpenCompanyClient, props: Partial<Parameters<typeof DeskRoutingPanel>[0]> = {}) {
  await act(async () => {
    root.render(createElement(DeskRoutingPanel, { client: stub, company: "acme", deskId: "engineering", ...props }));
  });
  return host.querySelector('[data-testid="desk-routing-panel"]') as HTMLElement;
}

const settle = () => act(async () => {});

describe("DeskRoutingPanel", () => {
  it("reads the block and shows source, effective numbers, and shared candidates", async () => {
    const { stub, getDeskRouting } = client(dto());
    const panel = await render(stub);
    expect(getDeskRouting).toHaveBeenCalledWith("engineering", "acme");
    expect(panel.dataset.state).toBe("ready");
    expect(panel.dataset.source).toBe("manifest");
    expect(panel.querySelector('[data-testid="desk-routing-source"]')?.textContent).toBe("from company.toml");
    expect(panel.querySelector('[data-testid="desk-routing-router"]')?.textContent).toBe("router: fallback");
    expect(panel.querySelector('[data-testid="desk-routing-effective"]')?.textContent).toContain("600s");
    const candidates = [...panel.querySelectorAll<HTMLElement>('[data-testid="desk-routing-candidates"] li')];
    expect(candidates.map((c) => `${c.dataset.agentId}:${c.dataset.shared}`)).toEqual(["engineer:false", "ceo:true"]);
    expect(candidates[1].textContent).toContain("also on #content");
    expect((panel.querySelector('[data-testid="desk-routing-round_width"]') as HTMLInputElement).value).toBe("2");
  });

  it("shows the host failure when the first routing read rejects", async () => {
    const { stub, getDeskRouting } = client(dto());
    let reject!: (reason: Error) => void;
    const pending = new Promise<DeskRoutingDto>((_resolve, rejectRead) => {
      reject = rejectRead;
    });
    getDeskRouting.mockReturnValueOnce(pending);
    const loading = await render(stub);
    expect(loading.dataset.state).toBe("loading");
    expect(loading.textContent).toContain("Reading routing");

    await act(async () => reject(new Error("Routing read failed: HTTP 503")));

    const panel = host.querySelector('[data-testid="desk-routing-panel"]') as HTMLElement;
    expect(panel.dataset.state).toBe("error");
    expect(panel.textContent).toBe("Routing read failed: HTTP 503");
    expect(host.textContent).not.toContain("Reading routing");
  });

  it("installs the edited block and re-renders from the host's answer", async () => {
    const { stub, putDeskRouting } = client(dto());
    const panel = await render(stub);
    const width = panel.querySelector('[data-testid="desk-routing-round_width"]') as HTMLInputElement;
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      setter.call(width, "3");
      width.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => {
      (panel.querySelector('[data-testid="desk-routing-save"]') as HTMLButtonElement).click();
    });
    await settle();
    expect(putDeskRouting).toHaveBeenCalledWith(
      "engineering",
      { round_width: 3 },
      "acme",
    );
    expect(panel.dataset.source).toBe("overlay");
    expect((panel.querySelector('[data-testid="desk-routing-reset"]') as HTMLButtonElement).disabled).toBe(false);
  });

  it("offers reset only for an installed block, and drops it on click", async () => {
    const { stub, resetDeskRouting } = client(dto({ source: "overlay" }));
    const panel = await render(stub);
    const reset = panel.querySelector('[data-testid="desk-routing-reset"]') as HTMLButtonElement;
    expect(reset.disabled).toBe(false);
    await act(async () => reset.click());
    await settle();
    expect(resetDeskRouting).toHaveBeenCalledWith("engineering", "acme");
    expect(panel.dataset.source).toBe("manifest");
    expect((panel.querySelector('[data-testid="desk-routing-reset"]') as HTMLButtonElement).disabled).toBe(true);
  });

  it("renders the host's own refusal verbatim", async () => {
    const { stub, putDeskRouting } = client(dto());
    putDeskRouting.mockRejectedValueOnce(new Error("round_width must be at least 1"));
    const panel = await render(stub);
    await act(async () => {
      (panel.querySelector('[data-testid="desk-routing-save"]') as HTMLButtonElement).click();
    });
    await settle();
    expect(panel.querySelector('[data-testid="desk-routing-error"]')?.textContent).toBe("round_width must be at least 1");
  });

  it("re-reads when the shell bumps the refresh key", async () => {
    const { stub, getDeskRouting } = client(dto());
    await render(stub, { refreshKey: 0 });
    await render(stub, { refreshKey: 1 });
    expect(getDeskRouting).toHaveBeenCalledTimes(2);
  });

  it("hides the write controls from a reader who cannot manage", async () => {
    const { stub } = client(dto());
    const panel = await render(stub, { canManage: false });
    expect(panel.querySelector('[data-testid="desk-routing-save"]')).toBeNull();
    expect((panel.querySelector('[data-testid="desk-routing-round_width"]') as HTMLInputElement).disabled).toBe(true);
  });

  it("says so on a host with no routing read", async () => {
    const panel = await render({} as unknown as OpenCompanyClient);
    expect(panel.dataset.state).toBe("unavailable");
  });
});
