import { describe, expect, it } from "vitest";

import { categoryOf } from "@/inference/catalogue";
import {
  ADVANCED_INTRO,
  MODE_COPY,
  WORKLOADS,
  WORKLOAD_COPY,
  WORKLOAD_TIER,
  applyToEveryWorkload,
  formatRef,
  inferRoutingMode,
  MANAGED_TARGET_LABEL,
  UNSET_TARGET,
  modelTarget,
  orphanedRoutes,
  ownModeDraft,
  parseRef,
  modelAfterProviderChange,
  refForTarget,
  removalImpact,
  removalWarnings,
  routingOptions,
  targetForRef,
  targetLabel,
  refSignature,
  routingTargets,
  rowValue,
  scrubOnRemove,
} from "@/inference/routing";
import type { RemovalImpact } from "@/inference/routing";
import type { Provider, RoutingMap } from "@/inference/types";

/**
 * Manage Routing, as decisions.
 *
 * Every case here is a branch that would otherwise only be reachable through a
 * rendered screen. They are the rules the Rust resolver holds for the turn path,
 * asserted on the side that has to draw a row before any request is made.
 */

function provider(slug: string, kind: string, enabled = true): Provider {
  return {
    id: `prv_${slug}`,
    slug,
    label: slug,
    kind,
    baseUrl: `https://${slug}.example/v1`,
    models: {},
    enabled,
    keyConfigured: true,
  };
}

describe("the rows the routing screen ships", () => {
  it("has one row per tier the runtime actually has", () => {
    expect(WORKLOADS).toEqual(["chat", "reasoning", "agentic", "vision"]);
    const tiers = WORKLOADS.map((w) => WORKLOAD_TIER[w]);
    expect(new Set(tiers).size).toBe(tiers.length);
  });

  it("has no coding row, because it would write the agentic tier twice", () => {
    // Two editable rows over one tier means setting one silently changes the
    // other — the inheritance bug in a different hat.
    expect(WORKLOADS).not.toContain("coding");
    expect(WORKLOAD_COPY.agentic.description).toContain("coding");
  });

  it("keeps the recommendation hint on every row", () => {
    // The hints are what make this screen usable by someone who has never
    // chosen a model. They are the first thing a rewrite would drop.
    for (const workload of WORKLOADS) {
      expect(WORKLOAD_COPY[workload].hint.length).toBeGreaterThan(40);
      expect(WORKLOAD_COPY[workload].label).toBeTruthy();
      expect(WORKLOAD_COPY[workload].description).toBeTruthy();
    }
  });

  it("names this product in the managed description, not the one the copy came from", () => {
    expect(MODE_COPY.managed.description).toContain("TinyHumans");
    expect(MODE_COPY.managed.description).not.toContain("OpenHuman");
    expect(ADVANCED_INTRO).toContain("Managed");
  });
});

describe("the hand-editable route grammar", () => {
  it("round-trips through a person", () => {
    const cases = ["", "managed", "acme", "acme:gpt-5", "local:llama3.1", "claude-code:opus"];
    for (const raw of cases) {
      expect(formatRef(parseRef(raw))).toBe(raw === "default" ? "" : raw);
    }
  });

  it("reads an empty value as unset rather than as a parse failure", () => {
    // Deleting the text is how an operator says "nothing here".
    expect(parseRef("")).toEqual({ kind: "default" });
    expect(parseRef("   ")).toEqual({ kind: "default" });
    expect(parseRef("default")).toEqual({ kind: "default" });
  });

  it("reads a trailing colon as a slug with no model", () => {
    expect(parseRef("acme:")).toEqual({ kind: "cloud", providerSlug: "acme", model: undefined });
  });

  it("gives two refs that mean the same thing the same signature", () => {
    // Structural equality would say no for an absent versus undefined model.
    expect(refSignature({ kind: "cloud", providerSlug: "acme" })).toBe(
      refSignature({ kind: "cloud", providerSlug: "acme", model: undefined }),
    );
    expect(refSignature({ kind: "default" })).toBe("default");
  });
});

describe("inferring the routing mode", () => {
  it("calls a company that has chosen nothing managed", () => {
    expect(inferRoutingMode({})).toBe("managed");
    expect(inferRoutingMode({ chat: { kind: "managed" }, vision: { kind: "default" } })).toBe(
      "managed",
    );
  });

  it("calls one provider and model on every row own", () => {
    expect(inferRoutingMode(applyToEveryWorkload("acme", "gpt-5"))).toBe("own");
  });

  it("calls a single differing row advanced", () => {
    const mixed: RoutingMap = {
      ...applyToEveryWorkload("acme", "gpt-5"),
      vision: { kind: "cloud", providerSlug: "acme", model: "vision" },
    };
    expect(inferRoutingMode(mixed)).toBe("advanced");
  });

  it("calls a partly-set map advanced, because an unset row is not the same", () => {
    expect(inferRoutingMode({ chat: { kind: "cloud", providerSlug: "acme" } })).toBe("advanced");
  });

  it("is a function of the routes and nothing else", () => {
    // There is no mode field, so nothing can disagree with the four routes.
    const map = applyToEveryWorkload("acme", "gpt-5");
    expect(inferRoutingMode(map)).toBe(inferRoutingMode({ ...map }));
  });
});

describe("scrubbing the routes a removal orphans", () => {
  it("matches a cloud provider precisely by slug", () => {
    const routing: RoutingMap = {
      chat: { kind: "cloud", providerSlug: "acme", model: "gpt-5" },
      reasoning: { kind: "cloud", providerSlug: "openrouter", model: "big" },
    };
    const { routing: next, reset } = scrubOnRemove(
      routing,
      provider("acme", "openai_compatible"),
      [provider("openrouter", "openrouter")],
      categoryOf,
    );
    expect(reset).toEqual(["chat"]);
    expect(next.chat).toEqual({ kind: "default" });
    expect(next.reasoning).toEqual({ kind: "cloud", providerSlug: "openrouter", model: "big" });
  });

  it("scrubs a CLI login's slug-less routes", () => {
    // Without this, disconnecting Claude Code left workloads pinned to
    // `claude-code:<model>`, which the resolver still honours — so chats kept
    // using the CLI after the provider was removed.
    const routing: RoutingMap = { chat: { kind: "claudeCode", model: "opus" } };
    const { routing: next, reset } = scrubOnRemove(
      routing,
      provider("claude-code", "claude-code"),
      [provider("openrouter", "openrouter")],
      categoryOf,
    );
    expect(reset).toEqual(["chat"]);
    expect(next.chat).toEqual({ kind: "default" });
  });

  it("leaves a local route alone while another local runtime remains", () => {
    const routing: RoutingMap = { chat: { kind: "local", model: "llama3.1" } };
    const { routing: next, reset } = scrubOnRemove(
      routing,
      provider("ollama", "ollama"),
      [provider("lmstudio", "lmstudio")],
      categoryOf,
    );
    expect(reset).toEqual([]);
    expect(next.chat).toEqual({ kind: "local", model: "llama3.1" });
  });

  it("scrubs a local route once no local runtime remains", () => {
    // And before this rule existed the local case was silently a no-op.
    const routing: RoutingMap = { chat: { kind: "local", model: "llama3.1" } };
    const { routing: next, reset } = scrubOnRemove(
      routing,
      provider("ollama", "ollama"),
      [provider("openrouter", "openrouter")],
      categoryOf,
    );
    expect(reset).toEqual(["chat"]);
    expect(next.chat).toEqual({ kind: "default" });
  });

  it("never touches a managed or unset row", () => {
    const routing: RoutingMap = { chat: { kind: "managed" }, vision: { kind: "default" } };
    const { reset } = scrubOnRemove(
      routing,
      provider("acme", "openai_compatible"),
      [],
      categoryOf,
    );
    expect(reset).toEqual([]);
  });
});

describe("the second mechanism behind the same invariant", () => {
  it("catches a route edited in outside the UI", () => {
    // The UI path can be bypassed by a config edit or an older build, so an
    // unresolvable route has to be reported at load rather than mid-turn.
    const routing: RoutingMap = {
      chat: { kind: "cloud", providerSlug: "ghost", model: "gpt-5" },
      reasoning: { kind: "cloud", providerSlug: "acme" },
    };
    expect(orphanedRoutes(routing, [provider("acme", "openai_compatible")])).toEqual([
      { workload: "chat", slug: "ghost" },
    ]);
  });
});

describe("what a row offers and reads", () => {
  it("says Choose Model when nothing is set and Change Model when something is", () => {
    // An unset row names where it will actually go. "No model selected" said
    // nothing about a row that still resolves somewhere, on the one screen whose
    // job is to say where work goes.
    expect(rowValue({ kind: "default" }, [])).toEqual({
      value: "Primary (Managed)",
      action: "Choose Model",
    });
    expect(rowValue({ kind: "managed" }, []).action).toBe("Change Model");
  });

  it("names the primary an unset row resolves through, and follows the marker", () => {
    const openrouter = { ...provider("openrouter", "openrouter"), label: "OpenRouter" };
    const acme = { ...provider("acme", "openai_compatible"), label: "Acme gateway" };
    expect(rowValue({ kind: "default" }, [openrouter, acme]).value).toBe("Primary (OpenRouter)");
    // Marked, it moves — read on every render rather than cached.
    expect(
      rowValue({ kind: "default" }, [openrouter, { ...acme, isDefault: true }]).value,
    ).toBe("Primary (Acme gateway)");
    // A disabled marked provider is not a routing target, so it is not the
    // primary either.
    expect(
      rowValue({ kind: "default" }, [openrouter, { ...acme, isDefault: true, enabled: false }])
        .value,
    ).toBe("Primary (OpenRouter)");
  });

  it("names the provider by its label, not its slug", () => {
    const acme = { ...provider("acme", "openai_compatible"), label: "Acme gateway" };
    expect(rowValue({ kind: "cloud", providerSlug: "acme", model: "gpt-5" }, [acme]).value).toBe(
      "Acme gateway · gpt-5",
    );
  });

  it("falls back to the slug for a provider it cannot find", () => {
    expect(rowValue({ kind: "cloud", providerSlug: "ghost" }, []).value).toBe("ghost");
  });

  it("does not offer a disabled provider as a routing target", () => {
    const providers = [provider("openrouter", "openrouter"), provider("acme", "openai_compatible", false)];
    expect(routingTargets(providers).map((p) => p.slug)).toEqual(["openrouter"]);
  });
});

describe("ownModeDraft", () => {
  it("reads back what applyToEveryWorkload wrote", () => {
    // The round trip is the whole property: the shared-model form saved
    // correctly and then rendered empty, which reads as a lost save and is
    // indistinguishable from the routing table being inert.
    const saved = applyToEveryWorkload("anthropic", "claude-sonnet-5");
    expect(ownModeDraft(saved)).toEqual({ slug: "anthropic", model: "claude-sonnet-5" });
    expect(inferRoutingMode(saved)).toBe("own");
  });

  it("keeps a blank model blank rather than inventing a placeholder", () => {
    // Blank means "send the tier and let the endpoint resolve it" — a real
    // state of the field, not an absence to be filled in.
    const saved = applyToEveryWorkload("openrouter");
    expect(ownModeDraft(saved)).toEqual({ slug: "openrouter", model: "" });
  });

  it("is blank when the rows disagree", () => {
    // The same condition `inferRoutingMode` calls advanced. Showing the first
    // row's provider here would misreport the other three.
    const mixed = {
      ...applyToEveryWorkload("openrouter", "gpt-5"),
      vision: parseRef("anthropic:claude-sonnet-5"),
    } as RoutingMap;
    expect(inferRoutingMode(mixed)).toBe("advanced");
    expect(ownModeDraft(mixed)).toEqual({ slug: "", model: "" });
  });

  it("is blank for a managed or unset table, which own mode does not describe", () => {
    const managed = Object.fromEntries(
      WORKLOADS.map((w) => [w, { kind: "managed" } as const]),
    ) as RoutingMap;
    expect(ownModeDraft(managed)).toEqual({ slug: "", model: "" });
    expect(ownModeDraft({} as RoutingMap)).toEqual({ slug: "", model: "" });
  });
});

describe("the per-workload select", () => {
  const connected = [provider("openrouter", "openrouter"), provider("anthropic", "anthropic")];

  it("lists providers only — the primary is never a second entry", () => {
    // Three connected providers, three options. It used to list five: the unset
    // row's own display (`Primary (OpenRouter)`) and a second Managed sentinel,
    // both beside the real rows, two of them naming the same account.
    const options = routingOptions(connected);
    expect(options.map((o) => o.slug)).toEqual(["tinyhumans", "openrouter", "anthropic"]);
    expect(options[0].label).toBe(MANAGED_TARGET_LABEL);
    expect(options.some((o) => o.slug === UNSET_TARGET)).toBe(false);
  });

  it("lists managed once even when it is also a provider record", () => {
    // A company whose entry zero is the managed config has a `tinyhumans` row of
    // its own. One identity, one entry.
    const options = routingOptions([provider("tinyhumans", "managed"), ...connected]);
    expect(options.filter((o) => o.slug === "tinyhumans")).toHaveLength(1);
  });

  it("gives Default and the provider it resolves to the same field shape", () => {
    // The bug this closes: `Primary (OpenRouter)` showed no Model id field and
    // `OpenRouter` did, for two names of one provider.
    expect(modelTarget(UNSET_TARGET, connected)).toBe("openrouter");
    expect(modelTarget("openrouter", connected)).toBe("openrouter");
    expect(modelTarget(UNSET_TARGET, connected)).toBe(modelTarget("openrouter", connected));
  });

  it("follows the marked default, not list order, when resolving unset", () => {
    const marked = [
      provider("openrouter", "openrouter"),
      { ...provider("anthropic", "anthropic"), isDefault: true },
    ];
    expect(modelTarget(UNSET_TARGET, marked)).toBe("anthropic");
  });

  it("offers no model id for managed, under either of its names", () => {
    // A `managed` ref carries no model in the route grammar, so a field there
    // could only be discarded on save — and both synonyms have to agree.
    expect(modelTarget("tinyhumans", connected)).toBeNull();
    expect(modelTarget(UNSET_TARGET, [])).toBeNull();
  });

  it("stays unset while the model is blank, and pins when one is chosen", () => {
    expect(refForTarget(UNSET_TARGET, "", connected)).toEqual({ kind: "default" });
    expect(refForTarget(UNSET_TARGET, "   ", connected)).toEqual({ kind: "default" });
    expect(refForTarget(UNSET_TARGET, "gpt-5", connected)).toEqual({
      kind: "cloud",
      providerSlug: "openrouter",
      model: "gpt-5",
    });
    expect(refForTarget("anthropic", "claude-sonnet-5", connected)).toEqual({
      kind: "cloud",
      providerSlug: "anthropic",
      model: "claude-sonnet-5",
    });
    expect(refForTarget("tinyhumans", "gpt-5", connected)).toEqual({ kind: "managed" });
  });

  it("round-trips a stored ref back to the value the select shows", () => {
    expect(targetForRef({ kind: "default" })).toBe(UNSET_TARGET);
    expect(targetForRef({ kind: "managed" })).toBe("tinyhumans");
    expect(targetForRef(parseRef("anthropic:claude-sonnet-5"))).toBe("anthropic");
  });

  it("never shows a sentinel to an operator", () => {
    // The helper labels a provider by its slug, so these read lowercase — the
    // point is that neither sentinel reaches the screen.
    expect(targetLabel(UNSET_TARGET, connected)).toBe("Primary (openrouter)");
    expect(targetLabel("tinyhumans", connected)).toBe(MANAGED_TARGET_LABEL);
    expect(targetLabel("anthropic", connected)).toBe("anthropic");
  });
});

describe("what a removal costs", () => {
  const openrouter = { ...provider("openrouter", "openrouter"), isDefault: true };
  const anthropic = provider("anthropic", "anthropic");
  const providers = [openrouter, anthropic];

  it("names the workloads whose routes would reset", () => {
    const routing = {
      ...applyToEveryWorkload("anthropic", "claude-sonnet-5"),
      chat: parseRef("openrouter:gpt-5"),
    } as RoutingMap;
    const impact = removalImpact(anthropic, providers, routing, categoryOf);
    expect(impact.routed).toEqual(["reasoning", "agentic", "vision"]);
    expect(impact.isDefault).toBe(false);
    expect(impact.lastEnabled).toBe(false);
  });

  it("notices the default and the last provider switched on", () => {
    const impact = removalImpact(openrouter, [openrouter], {} as RoutingMap, categoryOf);
    expect(impact.isDefault).toBe(true);
    expect(impact.lastEnabled).toBe(true);
  });

  it("says removing a key is not removing a provider, either way round", () => {
    const impact: RemovalImpact = {
      routed: [],
      isDefault: false,
      lastEnabled: false,
      defaultMovesTo: null,
    };
    const key = removalWarnings("key", "Anthropic", impact)[0];
    const provider_ = removalWarnings("provider", "Anthropic", impact)[0];
    expect(key).toContain("stays on this page");
    expect(provider_).toContain("deletes Anthropic");
    expect(key).not.toEqual(provider_);
  });

  it("warns about the default on the action that moves it", () => {
    // The operator who removed their default provider was not told the default
    // had relocated. Silence there is a company's unrouted spend moving accounts.
    const lines = removalWarnings("provider", "Anthropic", {
      routed: [],
      isDefault: true,
      lastEnabled: false,
      defaultMovesTo: "OpenRouter",
    });
    expect(lines.some((l) => l.includes("default"))).toBe(true);
  });

  it("counts the routed workloads in words an operator can act on", () => {
    const one = removalWarnings("provider", "Anthropic", {
      routed: ["chat"],
      isDefault: false,
      lastEnabled: false,
      defaultMovesTo: null,
    });
    expect(one.some((l) => l.includes("One workload routes") && l.includes("Chat"))).toBe(true);
    const many = removalWarnings("provider", "Anthropic", {
      routed: ["chat", "vision"],
      isDefault: false,
      lastEnabled: false,
      defaultMovesTo: null,
    });
    expect(many.some((l) => l.includes("2 workloads route"))).toBe(true);
  });

  it("only mentions the last-provider case where it is true and relevant", () => {
    const impact: RemovalImpact = {
      routed: [],
      isDefault: false,
      lastEnabled: true,
      defaultMovesTo: null,
    };
    expect(removalWarnings("provider", "Anthropic", impact).some((l) => l.includes("only provider"))).toBe(true);
    // Clearing a credential does not remove the row, so it is still the only
    // provider switched on afterwards.
    expect(removalWarnings("key", "Anthropic", impact).some((l) => l.includes("only provider"))).toBe(false);
  });
});

describe("modelAfterProviderChange", () => {
  it("drops a model id when the provider changes", () => {
    // `claude-haiku-4-5-20251001` is meaningless at OpenRouter, and the field
    // was silently dropping into free-text mode rather than saying so — the
    // "wrong model at the wrong provider" failure, arriving through the form.
    expect(modelAfterProviderChange("claude-haiku-4-5-20251001", "anthropic", "openrouter")).toBe(
      "",
    );
  });

  it("keeps it when the provider has not changed", () => {
    // Not the mid-keystroke rule: re-selecting the same provider is not an edit.
    expect(modelAfterProviderChange("gpt-5", "openrouter", "openrouter")).toBe("gpt-5");
  });

  it("leaves blank blank, which is a working route everywhere", () => {
    expect(modelAfterProviderChange("", "anthropic", "openrouter")).toBe("");
  });
});

describe("where the default goes when a provider is removed", () => {
  it("names the provider that will actually hold it", () => {
    const anthropic = { ...provider("anthropic", "anthropic"), isDefault: true };
    const openrouter = provider("openrouter", "openrouter");
    const impact = removalImpact(
      anthropic,
      [anthropic, openrouter],
      {} as RoutingMap,
      categoryOf,
    );
    expect(impact.defaultMovesTo).toBe("openrouter");
    const lines = removalWarnings("provider", "Anthropic", impact);
    expect(lines.some((l) => l.includes("moves the default to openrouter"))).toBe(true);
    // One-way, and said so: re-adding the credential did not bring the marker
    // back, and nothing on screen admitted that.
    expect(lines.some((l) => l.includes("will not move it back"))).toBe(true);
  });

  it("says so plainly when nothing is left to take it over", () => {
    const only = { ...provider("anthropic", "anthropic"), isDefault: true };
    const impact = removalImpact(only, [only], {} as RoutingMap, categoryOf);
    expect(impact.defaultMovesTo).toBeNull();
    expect(
      removalWarnings("provider", "Anthropic", impact).some((l) => l.includes("Managed")),
    ).toBe(true);
  });
});

describe("removing a local runtime", () => {
  it("scrubs the routes that name it by slug", () => {
    // `ollama:llama3` parses as a cloud ref because it carries a slug, while
    // `categoryOf("ollama")` is local — so gating the cloud arm on the category
    // meant the two rules never met and removal scrubbed nothing. The routing
    // table on disk then became unsaveable, because the host fails closed on a
    // route naming a provider nobody holds.
    const ollama = provider("ollama", "ollama");
    const openrouter = provider("openrouter", "openrouter");
    const routing = {
      chat: parseRef("ollama:llama3"),
      reasoning: parseRef("openrouter:gpt-5"),
    } as RoutingMap;

    const { routing: next, reset } = scrubOnRemove(routing, ollama, [openrouter], categoryOf);
    expect(reset).toEqual(["chat"]);
    expect(next.chat).toEqual({ kind: "default" });
    expect(next.reasoning).toEqual(parseRef("openrouter:gpt-5"));
    expect(orphanedRoutes(next, [openrouter])).toEqual([]);
  });

  it("leaves a slug-less local route alone while another runtime serves it", () => {
    const ollama = provider("ollama", "ollama");
    const lmstudio = provider("lmstudio", "lmstudio");
    const routing = { chat: parseRef("local:llama3") } as RoutingMap;
    expect(scrubOnRemove(routing, ollama, [lmstudio], categoryOf).reset).toEqual([]);
    expect(scrubOnRemove(routing, ollama, [], categoryOf).reset).toEqual(["chat"]);
  });
});
