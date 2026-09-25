import { describe, expect, it } from "vitest";

import {
  agentDisplayName,
  agentPairBrokenCopy,
  companyDefaultLabel,
  pairEdits,
  pairLabel,
  pairMissingModel,
  providerEdit,
  resolveAgentDefault,
} from "@/lib/agent";
import type { AgentDetailDto } from "@/api/types";
import type { Provider } from "@/inference/types";

/**
 * The agent pair editor's pure derivations (keys rework, issue #2306, slice
 * 3b). An agent on a built-in harness may pin its own `{provider, model}`
 * pair; unpinned, it falls back to the company default; with neither, it
 * fails closed (D-model). Every sentence here is decision X9's, verbatim.
 */

function agent(over: Partial<AgentDetailDto> = {}): AgentDetailDto {
  return {
    id: "researcher",
    name: "Researcher",
    role: "Researcher",
    source: "overlay",
    editable: ["name", "role", "harness", "model", "provider"],
    isOrchestrator: false,
    tools: { requested: null, companyAllow: ["*"], deskAllow: [], deskCeilingActive: false, effective: ["*"] },
    skills: { requested: null, companyAvailable: [], effective: [], overridden: false },
    desks: [],
    inboxEnabled: false,
    ...over,
  };
}

function provider(over: Partial<Provider> = {}): Provider {
  return {
    id: "prv_anthropic",
    slug: "anthropic",
    label: "Anthropic",
    kind: "anthropic",
    baseUrl: "https://api.anthropic.com/v1",
    models: {},
    enabled: true,
    keyConfigured: true,
    ...over,
  };
}

describe("providerEdit — the sibling of modelEdit/harnessEdit", () => {
  it("sends nothing when the draft did not change", () => {
    expect(providerEdit(undefined, "")).toBeUndefined();
    expect(providerEdit("anthropic", "anthropic")).toBeUndefined();
  });

  it("sends the slug when a provider is chosen", () => {
    expect(providerEdit(undefined, "anthropic")).toBe("anthropic");
  });

  it("sends null to clear back to the company default", () => {
    expect(providerEdit("anthropic", "")).toBeNull();
  });
});

describe("pairEdits — one PATCH body for both halves (round-2 review, P2-6)", () => {
  it("is null when neither half changed", () => {
    expect(pairEdits(agent({ provider: undefined, model: undefined }), "", "")).toBeNull();
    expect(pairEdits(agent({ provider: "anthropic", model: "test-model-large" }), "anthropic", "test-model-large")).toBeNull();
  });

  it("sends provider and model together when a new pair is chosen", () => {
    expect(pairEdits(agent({ provider: undefined, model: undefined }), "anthropic", "test-model-large")).toEqual({
      provider: "anthropic",
      model: "test-model-large",
    });
  });

  it("sends null for BOTH halves when the pin is cleared back to the company default", () => {
    expect(pairEdits(agent({ provider: "anthropic", model: "test-model-large" }), "", "")).toEqual({
      provider: null,
      model: null,
    });
  });

  it("sends both halves again when only the model changes on an already-pinned provider", () => {
    expect(
      pairEdits(agent({ provider: "anthropic", model: "test-model-large" }), "anthropic", "test-model-small"),
    ).toEqual({ provider: "anthropic", model: "test-model-small" });
  });

  it("sends both halves when switching to a different provider, seeding its own model", () => {
    expect(
      pairEdits(agent({ provider: "anthropic", model: "test-model-large" }), "openrouter", "acme/test-model"),
    ).toEqual({ provider: "openrouter", model: "acme/test-model" });
  });
});

describe("pairMissingModel — the Save button's own gate (round-2 review, P2-6)", () => {
  it("blocks a chosen provider with no valid model", () => {
    expect(pairMissingModel("anthropic", "")).toBe(true);
    expect(pairMissingModel("anthropic", "  ")).toBe(true);
    expect(pairMissingModel("anthropic", "reasoning-v1")).toBe(true); // a tier name, never a model
  });

  it("is ready with a chosen provider and a real model id", () => {
    expect(pairMissingModel("anthropic", "test-model-large")).toBe(false);
  });

  it("is always ready for the company default — there is no model field to fill in", () => {
    expect(pairMissingModel("", "")).toBe(false);
    expect(pairMissingModel("", "anything")).toBe(false);
  });
});

describe("companyDefaultLabel", () => {
  const providers = [provider({ slug: "openrouter", label: "OpenRouter" })];

  it("names the provider and model when the default is full", () => {
    expect(companyDefaultLabel({ provider: "openrouter", model: "acme/test-model" }, providers)).toBe(
      "Company default · OpenRouter · acme/test-model",
    );
  });

  it("falls back to the slug when the provider is not in the list", () => {
    expect(companyDefaultLabel({ provider: "ghost", model: "x" }, providers)).toBe(
      "Company default · ghost · x",
    );
  });

  it("says none chosen for a bare-slug default or no default at all", () => {
    expect(companyDefaultLabel({ provider: "openrouter", model: null }, providers)).toBe(
      "Company default · none chosen",
    );
    expect(companyDefaultLabel(null, providers)).toBe("Company default · none chosen");
    expect(companyDefaultLabel(undefined, providers)).toBe("Company default · none chosen");
  });
});

describe("pairLabel", () => {
  it("names the provider by label and the model", () => {
    const providers = [provider({ slug: "anthropic", label: "Anthropic" })];
    expect(pairLabel("anthropic", "test-model-large", providers)).toBe("Anthropic · test-model-large");
  });

  it("falls back to the slug for a provider not in the list (a gone or disabled pin, F6)", () => {
    expect(pairLabel("gone", "x", [])).toBe("gone · x");
  });
});

describe("resolveAgentDefault — what an unpinned agent's fallback line says (decisions X9, X14)", () => {
  const healthy = [provider({ slug: "openrouter", label: "OpenRouter", enabled: true })];

  it("shows the full company default", () => {
    const resolution = resolveAgentDefault({ provider: "openrouter", model: "acme/test-model" }, healthy, "Writer");
    expect(resolution).toEqual({ kind: "full", label: "Company default · OpenRouter · acme/test-model" });
  });

  it("says nothing is chosen when the default is entirely unset", () => {
    const resolution = resolveAgentDefault(null, healthy, "Writer");
    expect(resolution.kind).toBe("none");
    expect(resolution.kind === "none" && resolution.message).toBe(
      "No model is chosen. Choose a provider and model for Writer, or set the company default in Connections → API Keys → LLM.",
    );
  });

  it("says nothing is chosen for a bare-slug default (no model)", () => {
    const resolution = resolveAgentDefault({ provider: "openrouter", model: null }, healthy, "Writer");
    expect(resolution.kind).toBe("none");
  });

  it("names a removed or disabled default provider — durable per X14, never silently cleared", () => {
    const removed = resolveAgentDefault({ provider: "ghost", model: "x" }, healthy, "Writer");
    expect(removed).toEqual({
      kind: "broken",
      message: "The company default uses ghost, which is removed. Choose a new default in Connections → API Keys → LLM.",
    });

    const disabled = resolveAgentDefault(
      { provider: "openrouter", model: "x" },
      [provider({ slug: "openrouter", label: "OpenRouter", enabled: false })],
      "Writer",
    );
    expect(disabled.kind).toBe("broken");
    expect(disabled.kind === "broken" && disabled.message).toContain("turned off");
  });
});

describe("agentPairBrokenCopy — a pinned pair naming a gone or disabled provider (X9, F6)", () => {
  const providers = [provider({ slug: "anthropic", label: "Anthropic", enabled: true })];

  it("says nothing when the pin is healthy", () => {
    expect(agentPairBrokenCopy(agent({ provider: "anthropic", model: "test-model-large" }), providers)).toBeNull();
  });

  it("says nothing when there is no pin at all", () => {
    expect(agentPairBrokenCopy(agent({ provider: undefined, model: undefined }), providers)).toBeNull();
  });

  it("names the agent, the provider, and the fix — for a removed provider", () => {
    expect(agentPairBrokenCopy(agent({ provider: "gone", model: "x" }), providers)).toBe(
      "Researcher uses gone, which is removed. Choose another provider and model for Researcher, or clear its model to use the company default.",
    );
  });

  it("names a switched-off provider", () => {
    const off = [provider({ slug: "anthropic", label: "Anthropic", enabled: false })];
    expect(agentPairBrokenCopy(agent({ provider: "anthropic", model: "x" }), off)).toContain("turned off");
  });

  it("names a present, enabled, keyless provider — mirrors the host's provider_has_no_key", () => {
    const keyless = [provider({ slug: "anthropic", label: "Anthropic", enabled: true, keyConfigured: false })];
    expect(agentPairBrokenCopy(agent({ provider: "anthropic", model: "x" }), keyless)).toBe(
      "Researcher uses Anthropic, which has no key. Add one in Connections → API Keys → LLM, or choose another provider and model for Researcher.",
    );
  });

  it("names a manifest or global-baseline agent by its role, not a generic 'This teammate' (KR-L2-02)", () => {
    // Most agents are exactly this shape: no chosen name, named by role —
    // per AgentDetailDto.name's own doc, "Absent for a manifest teammate,
    // which is named by its role." The pair-broken banner used to skip
    // straight past that to a generic fallback, unlike its sibling call
    // into resolveAgentDefault, which already tried `role` first.
    const manifest = agent({ name: undefined, role: "Page Builder", provider: "gone", model: "x" });
    expect(agentPairBrokenCopy(manifest, providers)).toBe(
      "Page Builder uses gone, which is removed. Choose another provider and model for Page Builder, or clear its model to use the company default.",
    );
  });
});

describe("agentDisplayName (round-2 review, KR-L2-02)", () => {
  it("prefers the chosen name", () => {
    expect(agentDisplayName({ name: "Researcher", role: "researcher" })).toBe("Researcher");
  });

  it("falls back to role when there is no chosen name — most agents", () => {
    expect(agentDisplayName({ name: undefined, role: "Page Builder" })).toBe("Page Builder");
  });

  it("falls back to a generic label only in the pathological case of neither", () => {
    expect(agentDisplayName({ name: undefined, role: undefined as unknown as string })).toBe(
      "This teammate",
    );
  });
});
