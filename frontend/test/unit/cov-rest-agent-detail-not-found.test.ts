// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { AgentDetailDto } from "@/api/types";
import { AgentDetailView } from "@/views/team/AgentDetailView";

/**
 * `classifyFailure` (`views/team/AgentDetailView.tsx`) turns a `GET
 * …/team/{id}` failure into three distinct honest states by re-asking the
 * roster — `missing` (the teammate is really gone), `unsupported` (the host
 * predates the detail route), `error` (nothing answered). No test anywhere
 * exercised the render for any of the three, or the roster-based branching
 * that tells them apart.
 */

function detail(): AgentDetailDto {
  return {
    id: "jamie",
    name: "Jamie",
    role: "Growth",
    source: "overlay",
    editable: [],
    isOrchestrator: false,
    tools: { requested: [], companyAllow: [], deskAllow: [], deskCeilingActive: false, effective: [] },
    skills: { requested: null, companyAvailable: [], effective: [], overridden: false },
    desks: [],
    inboxEnabled: false,
  };
}

function clientWith(opts: {
  getAgent: () => Promise<AgentDetailDto>;
  listTeam?: () => Promise<{ id: string }[]>;
}): OpenCompanyClient {
  return {
    getAgent: vi.fn(opts.getAgent),
    listTeam: vi.fn(opts.listTeam ?? (() => Promise.resolve([]))),
    updateAgent: vi.fn(),
    get: vi.fn(() => Promise.resolve({ role: "member" })),
    listHarnesses: vi.fn(() => Promise.resolve([])),
    scopeFor: () => "",
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient, agentId = "jamie") {
  await act(async () => {
    root.render(
      createElement(AgentDetailView, { client, company: null, agentId, onBack: () => {} }),
    );
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

describe("an agent detail read that fails answers with the right one of three honest states", () => {
  it("says the agent is gone when the roster no longer lists it", async () => {
    const client = clientWith({
      getAgent: () => Promise.reject(new ApiError(404, "not_found", "no such agent")),
      listTeam: () => Promise.resolve([{ id: "somebody-else" }]),
    });
    await show(client);

    expect(container.textContent).toContain("no longer on the roster");
  });

  it("says the host is too old when the roster still lists the agent", async () => {
    const client = clientWith({
      getAgent: () => Promise.reject(new ApiError(404, "not_found", "route not found")),
      listTeam: () => Promise.resolve([{ id: "jamie" }]),
    });
    await show(client);

    expect(container.textContent).toContain("can't open an agent yet");
  });

  it("says the host didn't answer for a transport failure, never blank", async () => {
    const client = clientWith({
      getAgent: () => Promise.reject(new ApiError(0, "network_error", "fetch failed")),
    });
    await show(client);

    expect(container.textContent).toContain("Couldn't load this agent");
  });

  it("treats a genuine access refusal as an honest failure too, not a blank screen", async () => {
    const client = clientWith({
      getAgent: () => Promise.reject(new ApiError(403, "forbidden", "not on this desk")),
      listTeam: () => Promise.reject(new Error("roster also refused")),
    });
    await show(client);

    expect(container.textContent).toContain("Couldn't load this agent");
    expect(container.querySelector('[role="alert"]')).toBeNull();
  });

  it("renders the agent normally once the read actually succeeds", async () => {
    const client = clientWith({ getAgent: () => Promise.resolve(detail()) });
    await show(client);

    expect(container.textContent).toContain("Jamie");
    expect(container.textContent).not.toContain("no longer on the roster");
  });
});
