// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { InferenceModel, InferenceStatus } from "@/api/inference";
import { InferenceSection } from "@/views/connections/InferenceSection";

let container: HTMLDivElement;
let root: Root;

function status(
  provider: string,
  models: Record<string, string>,
  keyConfigured = true,
  defaultTierModels: Record<string, string> = {},
): InferenceStatus {
  return {
    provider,
    slug: provider === "openai_compatible" ? "byok" : provider,
    baseUrl: provider === "openai_compatible" ? "https://models.example/v1" : "https://openrouter.ai/api/v1",
    models,
    defaultTierModels,
    source: "runtime",
    keyConfigured,
    cognition: "harness",
    usageMetering: "perTurn",
    restartRequired: false,
    harnessReachable: true,
    canRebuildInPlace: true,
  };
}

function clientFor(
  inference: InferenceStatus,
  catalog: InferenceModel[] | Error,
): { client: OpenCompanyClient; calls: string[]; puts: unknown[] } {
  const calls: string[] = [];
  const puts: unknown[] = [];
  const client = {
    scopeFor: () => "/api/v1/companies/acme",
    get: async (path: string) => {
      calls.push(path);
      if (path.endsWith("/inference/models")) {
        if (catalog instanceof Error) throw catalog;
        return catalog;
      }
      return inference;
    },
    put: async (_path: string, body: unknown) => {
      puts.push(body);
      return { status: inference, note: "" };
    },
  } as unknown as OpenCompanyClient;
  return { client, calls, puts };
}

async function mount(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(InferenceSection, { client, company: "acme", canManage: true }));
  });
  await act(async () => {});
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
});

describe("OpenRouter tier model pickers", () => {
  it("renders one registry-backed select per tier and preserves a custom stored id", async () => {
    const { client } = clientFor(
      status("openrouter", {
        "chat-v1": "operator/custom-chat",
        "reasoning-v1": "openai/reasoning",
        "agentic-v1": "anthropic/agentic",
        "vision-v1": "google/vision",
      }),
      [
        { id: "openai/reasoning", name: "Reasoning" },
        { id: "anthropic/agentic", name: "Agentic" },
        { id: "google/vision", name: "Vision" },
      ],
    );

    await mount(client);

    for (const tier of ["chat-v1", "reasoning-v1", "agentic-v1", "vision-v1"]) {
      expect(container.querySelector(`[data-testid="inference-model-select-${tier}"]`)).not.toBeNull();
      expect(container.querySelector(`input#inference-model-${tier}`)).toBeNull();
    }
    expect(container.querySelector("#inference-model-chat-v1")?.textContent).toContain(
      "operator/custom-chat",
    );
  });

  it("falls back to editable model ids when the registry request fails", async () => {
    const { client } = clientFor(
      status("openrouter", { "chat-v1": "operator/custom-chat" }),
      new Error("offline"),
    );

    await mount(client);

    expect(container.querySelector('[data-testid="inference-model-catalog-fallback"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="inference-model-catalog-proxied"]')).toBeNull();
    expect(container.querySelector("input#inference-model-chat-v1")).toHaveProperty(
      "value",
      "operator/custom-chat",
    );
  });

  it("also warns a keyless company that a typed model id is dropped when the registry fails to load", async () => {
    // Companion to the fallback case above: that one has a stored key, so
    // `wouldSaveProxied` is false and "Enter model ids directly" is accurate
    // as written — a direct id really is what gets saved. Here there is no
    // key, so Save resolves proxied and `stripProxyIncompatible` silently
    // drops any `<author>/<model>` shaped id typed into the field the
    // fallback copy just pointed the operator at. The proxied clarification
    // must render alongside the fallback message in this state, not only
    // once the catalog reaches `ready`.
    const { client } = clientFor(
      status("openrouter", { "chat-v1": "reasoning-v1" }, false),
      new Error("offline"),
    );

    await mount(client);

    expect(container.querySelector('[data-testid="inference-model-catalog-fallback"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="inference-model-catalog-proxied"]')).not.toBeNull();
  });

  it("keeps free-text inputs for a proxied OpenRouter company with no key configured", async () => {
    // No stored key -> the platform's subscription proxy resolves the tier,
    // which only accepts an abstract tier name (or its own disabled-by-default
    // `openrouter/<author>/<model>` passthrough). The registry's raw catalog
    // ids the select would save are exactly what that proxy rejects, so the
    // picker must not offer them here even though the catalog loaded fine.
    const { client } = clientFor(
      status("openrouter", { "chat-v1": "chat-v1" }, false),
      [{ id: "anthropic/claude-sonnet-5", name: "Claude Sonnet" }],
    );

    await mount(client);

    expect(container.querySelector('[data-testid="inference-model-catalog-proxied"]')).not.toBeNull();
    expect(container.querySelector("input#inference-model-chat-v1")).toHaveProperty("value", "chat-v1");
    expect(container.querySelector('[data-testid="inference-model-select-chat-v1"]')).toBeNull();
  });

  it("shows the proxied warning while the catalog is still loading for a keyless company (PR #1838 review)", async () => {
    // `wouldSaveProxied` already renders an editable free-text field and
    // leaves Save enabled the instant `modelCatalog.kind` is "loading" — see
    // `useFreeText` above, which includes `wouldSaveProxied` in its OR
    // regardless of catalog state. The proxied clarification used to be
    // gated to exclude "loading", so a keyless operator who typed a raw
    // `<author>/<model>` id and hit Save during a cold catalog fetch (which
    // can run for up to ten seconds) got no warning at all before
    // `stripProxyIncompatible` silently dropped it.
    const inference = status("openrouter", { "chat-v1": "chat-v1" }, false);
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get: async (path: string) => {
        // Never resolves — the registry request is permanently in flight.
        if (path.endsWith("/inference/models")) return new Promise(() => {});
        return inference;
      },
      put: async () => ({ status: inference, note: "" }),
    } as unknown as OpenCompanyClient;

    await mount(client);

    expect(container.querySelector('[data-testid="inference-model-select-chat-v1"]')).toBeNull();
    expect(
      container.querySelector('[data-testid="inference-model-catalog-proxied"]'),
    ).not.toBeNull();
  });

  it("clears a catalog-picked id from the tier field once the form would save proxied", async () => {
    // Mirrors what a key-clear leaves behind: a company stores a raw
    // `<author>/<model>` id (exactly what the catalog select writes) while no
    // key is configured. `model_for_tier` honours a tier override verbatim on
    // *both* the direct and proxied paths, so this id would ride straight to
    // the platform proxy, which only resolves an abstract tier name (or its
    // own disabled-by-default `openrouter/<author>/<model>` passthrough) —
    // the proxy rejects it. The free-text field that reappears here must not
    // keep offering that id back to Save; a tier id the operator typed by
    // hand (not present in the catalog) is unaffected.
    const { client } = clientFor(
      status(
        "openrouter",
        { "chat-v1": "anthropic/claude-sonnet-5", "reasoning-v1": "reasoning-v1" },
        false,
      ),
      [{ id: "anthropic/claude-sonnet-5", name: "Claude Sonnet" }],
    );

    await mount(client);

    expect(container.querySelector('[data-testid="inference-model-catalog-proxied"]')).not.toBeNull();
    expect(container.querySelector("input#inference-model-chat-v1")).toHaveProperty("value", "");
    expect(container.querySelector("input#inference-model-reasoning-v1")).toHaveProperty(
      "value",
      "reasoning-v1",
    );
  });

  it("keeps free-text inputs for non-OpenRouter providers without fetching the registry", async () => {
    const { client, calls } = clientFor(
      status("openai_compatible", { "chat-v1": "private/model" }),
      [],
    );

    await mount(client);

    expect(container.querySelector("input#inference-model-chat-v1")).toHaveProperty(
      "value",
      "private/model",
    );
    expect(calls.some((path) => path.endsWith("/inference/models"))).toBe(false);
  });

  it("strips a catalog-picked id from the stored config when Remove Key is clicked", async () => {
    // A keyed company saved a raw catalog id straight to OpenRouter (allowed
    // while keyed — this is not the proxied path). Remove Key clears the key
    // and, per `wouldSaveProxied`, immediately switches the company onto the
    // platform proxy. `model_for_tier` honours the stored override verbatim on
    // both paths, so the id it just carried over is exactly what the proxy
    // rejects unless Remove Key strips it before saving. The hand-typed tier
    // id on the other tier (not present in the catalog) must survive — Remove
    // Key only clears what the catalog select itself wrote.
    const { client, puts } = clientFor(
      status(
        "openrouter",
        { "chat-v1": "anthropic/claude-sonnet-5", "reasoning-v1": "reasoning-v1" },
        true,
      ),
      [{ id: "anthropic/claude-sonnet-5", name: "Claude Sonnet" }],
    );

    await mount(client);

    const button = container.querySelector('[data-testid="inference-remove-key"]') as HTMLButtonElement;
    expect(button).not.toBeNull();
    await act(async () => {
      button.click();
    });
    await act(async () => {});

    expect(puts).toHaveLength(1);
    const body = puts[0] as { key?: string; models?: Record<string, string> };
    expect(body.key).toBe("");
    expect(body.models).toEqual({ "reasoning-v1": "reasoning-v1" });
  });
});

describe("raw OpenRouter registry ids vs the proxy's own passthrough shape (issue #1838 follow-up, fifth instance)", () => {
  it("strips a two-segment OpenRouter-registry id (e.g. openrouter/auto) when Remove Key switches to the proxy", async () => {
    // OpenRouter's own catalog has ids under the `openrouter/` author too —
    // `openrouter/auto` is a real two-segment registry id, not the proxy's
    // three-segment `openrouter/<author>/<slug>` passthrough form. A prefix
    // check that only tests `startsWith("openrouter/")` mistakes the former
    // for the latter and leaves it in place; `model_for_tier` then forwards
    // it to the proxy verbatim, which rejects it.
    const { client, puts } = clientFor(
      status(
        "openrouter",
        { "chat-v1": "openrouter/auto", "reasoning-v1": "reasoning-v1" },
        true,
      ),
      [{ id: "openrouter/auto", name: "Auto" }],
    );

    await mount(client);

    const button = container.querySelector('[data-testid="inference-remove-key"]') as HTMLButtonElement;
    expect(button).not.toBeNull();
    await act(async () => {
      button.click();
    });
    await act(async () => {});

    expect(puts).toHaveLength(1);
    const body = puts[0] as { key?: string; models?: Record<string, string> };
    expect(body.key).toBe("");
    expect(body.models).toEqual({ "reasoning-v1": "reasoning-v1" });
  });

  it("keeps the proxy's genuine three-segment openrouter/<author>/<slug> passthrough id", async () => {
    // The exemption exists for this shape specifically — an operator who
    // enabled proxy passthrough and saved its own `openrouter/<author>/<slug>`
    // form must not have it stripped out from under them by the same fix.
    const { client, puts } = clientFor(
      status(
        "openrouter",
        { "chat-v1": "openrouter/anthropic/claude-3-opus", "reasoning-v1": "reasoning-v1" },
        true,
      ),
      [{ id: "openrouter/anthropic/claude-3-opus", name: "Claude 3 Opus (passthrough)" }],
    );

    await mount(client);

    const button = container.querySelector('[data-testid="inference-remove-key"]') as HTMLButtonElement;
    expect(button).not.toBeNull();
    await act(async () => {
      button.click();
    });
    await act(async () => {});

    expect(puts).toHaveLength(1);
    const body = puts[0] as { key?: string; models?: Record<string, string> };
    expect(body.models).toEqual({
      "chat-v1": "openrouter/anthropic/claude-3-opus",
      "reasoning-v1": "reasoning-v1",
    });
  });

  it("keeps a passthrough id with surrounding whitespace (issue #1838 follow-up, seventh instance)", async () => {
    // The shape check used to run on the untrimmed value: a leading space
    // fails `startsWith("openrouter/")`, so a pasted id with surrounding
    // whitespace was misclassified as a raw catalog id and dropped here —
    // even though `save()`'s own trim pass, which runs *after* this strip,
    // would otherwise have normalized it to the exact same accepted form.
    const { client, puts } = clientFor(
      status(
        "openrouter",
        { "chat-v1": "  openrouter/anthropic/claude-3-opus  ", "reasoning-v1": "reasoning-v1" },
        true,
      ),
      [{ id: "openrouter/anthropic/claude-3-opus", name: "Claude 3 Opus (passthrough)" }],
    );

    await mount(client);

    const button = container.querySelector('[data-testid="inference-remove-key"]') as HTMLButtonElement;
    expect(button).not.toBeNull();
    await act(async () => {
      button.click();
    });
    await act(async () => {});

    expect(puts).toHaveLength(1);
    const body = puts[0] as { key?: string; models?: Record<string, string> };
    expect(body.models).toEqual({
      "chat-v1": "openrouter/anthropic/claude-3-opus",
      "reasoning-v1": "reasoning-v1",
    });
  });

  it("strips a bare, unnamespaced model id typed on the proxied path (issue #1838 follow-up, ninth instance, PR #1838 review)", async () => {
    // `model_for_tier` honours an override verbatim on the proxied path
    // regardless of its shape — the platform endpoint's curated tier
    // registry only knows the four tier names it mirrors from
    // DEFAULT_TIER_MODELS. A slashless id like `gpt-4o` used to read as "a
    // bare tier id, not an override at all" purely because it contained no
    // `/`, and rode straight through Save. The endpoint does not recognize
    // `gpt-4o` as a tier, so the request fails instead of the incompatible
    // value being dropped the way the console's own warning promises. Only
    // an *exact* tier name (`chat-v1`, `reasoning-v1`, `agentic-v1`,
    // `vision-v1`) or the three-segment `openrouter/<author>/<model>`
    // passthrough is actually proxy-safe.
    const { client, puts } = clientFor(
      status(
        "openrouter",
        { "chat-v1": "gpt-4o", "reasoning-v1": "reasoning-v1" },
        true,
      ),
      [],
    );

    await mount(client);

    const button = container.querySelector('[data-testid="inference-remove-key"]') as HTMLButtonElement;
    expect(button).not.toBeNull();
    await act(async () => {
      button.click();
    });
    await act(async () => {});

    expect(puts).toHaveLength(1);
    const body = puts[0] as { key?: string; models?: Record<string, string> };
    expect(body.models).toEqual({ "reasoning-v1": "reasoning-v1" });
  });
});

describe("clearing a tier override back to the tier default (issue #1838 follow-up)", () => {
  it("offers a 'Use the tier default' item in the catalog select for a tier with a saved override", async () => {
    // A keyed OpenRouter company with a saved override and a loaded catalog
    // used to only ever offer concrete models — no way to remove the one
    // mapping and let `model_for_tier` fall back to its own default for that
    // tier. Opening the select must show an explicit way out.
    const { client } = clientFor(
      status("openrouter", { "chat-v1": "anthropic/claude-sonnet-5" }, true),
      [{ id: "anthropic/claude-sonnet-5", name: "Claude Sonnet" }],
    );

    await mount(client);

    const trigger = container.querySelector(
      '[data-testid="inference-model-select-chat-v1"]',
    ) as HTMLButtonElement;
    expect(trigger).not.toBeNull();
    await act(async () => {
      trigger.click();
    });
    await act(async () => {});

    expect(document.body.querySelector('[data-testid="inference-model-clear-chat-v1"]')).not.toBeNull();
  });

  it("clears the tier override when 'Use the tier default' is picked, and Save drops it from the wire", async () => {
    const { client, puts } = clientFor(
      status(
        "openrouter",
        { "chat-v1": "anthropic/claude-sonnet-5", "reasoning-v1": "anthropic/agentic" },
        true,
      ),
      [
        { id: "anthropic/claude-sonnet-5", name: "Claude Sonnet" },
        { id: "anthropic/agentic", name: "Agentic" },
      ],
    );

    await mount(client);

    const trigger = container.querySelector(
      '[data-testid="inference-model-select-chat-v1"]',
    ) as HTMLButtonElement;
    await act(async () => {
      trigger.click();
    });
    await act(async () => {});

    const clearItem = document.body.querySelector(
      '[data-testid="inference-model-clear-chat-v1"]',
    ) as HTMLElement;
    expect(clearItem).not.toBeNull();
    await act(async () => {
      clearItem.click();
    });
    await act(async () => {});

    // Cleared tier reverts to the placeholder select (no stored value); the
    // untouched tier keeps its override and stays a select, not free text.
    expect(container.querySelector("#inference-model-chat-v1")?.textContent).not.toContain(
      "anthropic/claude-sonnet-5",
    );
    expect(container.querySelector("#inference-model-reasoning-v1")?.textContent).toContain(
      "Agentic",
    );

    const save = container.querySelector('[data-testid="inference-save"]') as HTMLButtonElement;
    await act(async () => {
      save.click();
    });
    await act(async () => {});

    expect(puts).toHaveLength(1);
    const body = puts[0] as { models?: Record<string, string> };
    expect(body.models).toEqual({ "reasoning-v1": "anthropic/agentic" });
  });
});

describe("typing an id the registry does not list (issue #1838 follow-up)", () => {
  it("offers an 'Enter a model id' escape hatch and lets Save persist an id absent from the catalog", async () => {
    // Once the catalog loads, every tier becomes a select — `optionsForTier`
    // only keeps an *already-stored* custom id selectable, so a keyed
    // operator who wants a model the registry has not caught up to yet had no
    // way to type one. The escape hatch flips that one tier's control to free
    // text without touching the others.
    const { client, puts } = clientFor(status("openrouter", {}, true), [
      { id: "anthropic/claude-sonnet-5", name: "Claude Sonnet" },
    ]);

    await mount(client);

    const trigger = container.querySelector(
      '[data-testid="inference-model-select-chat-v1"]',
    ) as HTMLButtonElement;
    expect(trigger).not.toBeNull();
    await act(async () => {
      trigger.click();
    });
    await act(async () => {});

    const customItem = document.body.querySelector(
      '[data-testid="inference-model-custom-chat-v1"]',
    ) as HTMLElement;
    expect(customItem).not.toBeNull();
    await act(async () => {
      customItem.click();
    });
    await act(async () => {});

    // The select is gone; a plain text field takes its place.
    expect(container.querySelector('[data-testid="inference-model-select-chat-v1"]')).toBeNull();
    const input = container.querySelector<HTMLInputElement>("input#inference-model-chat-v1");
    expect(input).not.toBeNull();

    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLInputElement.prototype,
      "value",
    )?.set;
    await act(async () => {
      setter?.call(input, "moonshotai/kimi-k2-thinking");
      input?.dispatchEvent(new Event("input", { bubbles: true }));
    });

    // The other tier stays a select — the escape hatch is per-tier.
    expect(container.querySelector('[data-testid="inference-model-select-reasoning-v1"]')).not.toBeNull();

    const save = container.querySelector('[data-testid="inference-save"]') as HTMLButtonElement;
    await act(async () => {
      save.click();
    });
    await act(async () => {});

    expect(puts).toHaveLength(1);
    const body = puts[0] as { models?: Record<string, string> };
    expect(body.models).toEqual({ "chat-v1": "moonshotai/kimi-k2-thinking" });
  });

  it("returns a tier to the catalog select via 'Choose from the OpenRouter catalog instead'", async () => {
    const { client } = clientFor(status("openrouter", {}, true), [
      { id: "anthropic/claude-sonnet-5", name: "Claude Sonnet" },
    ]);

    await mount(client);

    const trigger = container.querySelector(
      '[data-testid="inference-model-select-chat-v1"]',
    ) as HTMLButtonElement;
    await act(async () => {
      trigger.click();
    });
    await act(async () => {});
    const customItem = document.body.querySelector(
      '[data-testid="inference-model-custom-chat-v1"]',
    ) as HTMLElement;
    await act(async () => {
      customItem.click();
    });
    await act(async () => {});

    const backLink = container.querySelector(
      '[data-testid="inference-model-back-to-catalog-chat-v1"]',
    ) as HTMLButtonElement;
    expect(backLink).not.toBeNull();
    await act(async () => {
      backLink.click();
    });
    await act(async () => {});

    expect(container.querySelector('[data-testid="inference-model-select-chat-v1"]')).not.toBeNull();
    expect(container.querySelector("input#inference-model-chat-v1")).toBeNull();
  });
});

describe("the OpenRouter preset prefills from the host's own defaults (issue #1838 follow-up)", () => {
  it("seeds the switch form from status.defaultTierModels rather than a hard-coded copy", async () => {
    // The console used to duplicate `DEFAULT_TIER_MODELS` locally; a value
    // that could only ever drift is not a fixture worth asserting against —
    // this proves the picker actually reads the host's own answer.
    //
    // Status reports an already-keyed direct OpenRouter connection, so
    // `wouldSaveProxied` stays false for the whole test — that guard exists
    // to protect an *unkeyed* save from a raw catalog-shaped id it would
    // save proxied (issue #1838 follow-up, sixth instance), which is a
    // separate concern from what this test is proving. Switch through a
    // second provider and back: the initial render seeds `models` straight
    // from `status.models`, not through `presetFor`, so the preset only
    // gets exercised on an actual provider pick.
    const { client } = clientFor(
      status("openrouter", {}, true, {
        "chat-v1": "vendor-x/model-a",
        "reasoning-v1": "vendor-x/model-b",
        "agentic-v1": "vendor-x/model-c",
        "vision-v1": "vendor-x/model-d",
      }),
      [],
    );

    await mount(client);

    async function selectProvider(label: string) {
      const trigger = container.querySelector("#inference-provider") as HTMLButtonElement;
      expect(trigger).not.toBeNull();
      await act(async () => {
        trigger.click();
      });
      await act(async () => {});

      const item = Array.from(
        document.body.querySelectorAll('[data-slot="select-item"]'),
      ).find((el) => el.textContent?.includes(label)) as HTMLElement | undefined;
      expect(item).not.toBeUndefined();
      await act(async () => {
        item?.click();
      });
      await act(async () => {});
    }

    await selectProvider("Ollama");
    await selectProvider("OpenRouter");

    // No catalog entries, so the tier renders free text — the preset value
    // shows up as the input's value, not as rendered text.
    expect(
      container.querySelector<HTMLInputElement>("input#inference-model-chat-v1"),
    ).toHaveProperty("value", "vendor-x/model-a");
  });
});

describe("proxy-incompatible overrides survive an unready catalog (issue #1838 follow-up)", () => {
  it("strips a raw catalog id from the draft while the registry is still loading, before Save is clicked", async () => {
    // Third instance of the #1838 class: the earlier fix only stripped a
    // tier value once `modelCatalog.kind === "ready"`, so a keyless company
    // that already has a raw `<author>/<model>` override stored (from an
    // earlier keyed session, or from switching onto OpenRouter's own preset)
    // kept offering it back to Save for as long as the registry request was
    // still in flight — which, on a slow network, can be indefinitely.
    const calls: string[] = [];
    const puts: unknown[] = [];
    const inference = status(
      "openrouter",
      { "chat-v1": "anthropic/claude-sonnet-5", "reasoning-v1": "reasoning-v1" },
      false,
    );
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get: async (path: string) => {
        calls.push(path);
        if (path.endsWith("/inference/models")) {
          // Never resolves — the registry request is permanently in flight
          // for the duration of this test.
          return new Promise(() => {});
        }
        return inference;
      },
      put: async (_path: string, body: unknown) => {
        puts.push(body);
        return { status: inference, note: "" };
      },
    } as unknown as OpenCompanyClient;

    await mount(client);

    // Still loading, never reached "ready".
    expect(container.querySelector('[data-testid="inference-model-select-chat-v1"]')).toBeNull();
    expect(container.querySelector("input#inference-model-chat-v1")).toHaveProperty("value", "");
    expect(container.querySelector("input#inference-model-reasoning-v1")).toHaveProperty(
      "value",
      "reasoning-v1",
    );

    const button = container.querySelector('[data-testid="inference-save"]') as HTMLButtonElement;
    await act(async () => {
      button.click();
    });
    await act(async () => {});

    expect(puts).toHaveLength(1);
    const body = puts[0] as { models?: Record<string, string> };
    expect(body.models).toEqual({ "reasoning-v1": "reasoning-v1" });
  });

  it("does not let Save persist a raw catalog id when the registry failed to load", async () => {
    // Same class, the "or has failed" half: a failed registry fetch also
    // never reaches `kind === "ready"`.
    const { client, puts } = clientFor(
      status(
        "openrouter",
        { "chat-v1": "anthropic/claude-sonnet-5", "reasoning-v1": "reasoning-v1" },
        false,
      ),
      new Error("registry unreachable"),
    );

    await mount(client);

    expect(container.querySelector('[data-testid="inference-model-catalog-fallback"]')).not.toBeNull();
    expect(container.querySelector("input#inference-model-chat-v1")).toHaveProperty("value", "");

    const button = container.querySelector('[data-testid="inference-save"]') as HTMLButtonElement;
    await act(async () => {
      button.click();
    });
    await act(async () => {});

    expect(puts).toHaveLength(1);
    const body = puts[0] as { models?: Record<string, string> };
    expect(body.models).toEqual({ "reasoning-v1": "reasoning-v1" });
  });

  it("strips a raw catalog id from Remove Key's carried models when the registry fetch fails", async () => {
    // Fourth instance: Remove Key used to fetch the registry itself to
    // decide what to carry over, and fell back to sending the stored
    // overrides completely unfiltered when that fetch failed — the one
    // outcome guaranteed to break every proxied tier, on exactly the
    // condition (registry unreachable) that triggers it.
    const inference = status(
      "openrouter",
      { "chat-v1": "anthropic/claude-sonnet-5", "reasoning-v1": "reasoning-v1" },
      true,
    );
    const puts: unknown[] = [];
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get: async (path: string) => {
        if (path.endsWith("/inference/models")) throw new Error("registry unreachable");
        return inference;
      },
      put: async (_path: string, body: unknown) => {
        puts.push(body);
        return { status: inference, note: "" };
      },
    } as unknown as OpenCompanyClient;

    await mount(client);

    const button = container.querySelector('[data-testid="inference-remove-key"]') as HTMLButtonElement;
    expect(button).not.toBeNull();
    await act(async () => {
      button.click();
    });
    await act(async () => {});

    expect(puts).toHaveLength(1);
    const body = puts[0] as { key?: string; models?: Record<string, string> };
    expect(body.key).toBe("");
    expect(body.models).toEqual({ "reasoning-v1": "reasoning-v1" });
  });

  it("preserves an untouched tier draft when the strip effect updates baseline (issue #1838 follow-up, eighth instance)", async () => {
    // Reproduces the "type a key, edit multiple tiers, clear the key again"
    // class the effect's own docstring describes. Mount keyless so the
    // proxied window is open from the first render and the mount-time strip
    // already drops `chat-v1`'s raw catalog id, leaving `models` and
    // `baseline` in agreement — nothing to distinguish old vs. new behavior
    // yet. Typing a key closes the window; while it's closed, type a genuine
    // hand-typed draft into `reasoning-v1` — a *different* tier's bare name
    // (`agentic-v1`), which `isProxyCompatible` keeps as-is since any of the
    // four tier names is a valid proxy passthrough regardless of which field
    // it was typed into, so the strip never touches it — and a fresh raw
    // catalog id into `chat-v1`.
    // Clearing the key reopens the window: the strip effect fires again and
    // must fold *only* `chat-v1` into baseline. The old code replaced
    // `baseline.models` wholesale with the full stripped snapshot, which
    // silently promoted `reasoning-v1`'s untouched draft into baseline too —
    // `hasTypedDraft()` then saw no difference, and switching providers
    // discarded that draft with no confirmation.
    const inference = status(
      "openrouter",
      { "chat-v1": "anthropic/claude-sonnet-5", "reasoning-v1": "reasoning-v1" },
      false,
    );
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get: async (path: string) => {
        if (path.endsWith("/inference/models")) throw new Error("registry unreachable");
        return inference;
      },
      put: async () => ({ status: inference, note: "" }),
    } as unknown as OpenCompanyClient;

    await mount(client);

    // Mount-time strip already ran; baseline and models started equal so
    // this pass has nothing left to prove.
    expect(container.querySelector("input#inference-model-chat-v1")).toHaveProperty("value", "");

    function setValue(input: HTMLInputElement | null, value: string) {
      const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")?.set;
      setter?.call(input, value);
      input?.dispatchEvent(new Event("input", { bubbles: true }));
    }

    const keyInput = container.querySelector<HTMLInputElement>("input#inference-key");
    const chatInput = container.querySelector<HTMLInputElement>("input#inference-model-chat-v1");
    const reasoningInput = container.querySelector<HTMLInputElement>("input#inference-model-reasoning-v1");
    expect(keyInput).not.toBeNull();
    expect(chatInput).not.toBeNull();
    expect(reasoningInput).not.toBeNull();

    await act(async () => setValue(keyInput, "sk-test")); // closes the proxied window
    await act(async () => setValue(reasoningInput, "agentic-v1")); // genuine operator draft
    await act(async () => setValue(chatInput, "openai/reasoning")); // fresh raw id to strip
    await act(async () => setValue(keyInput, "")); // reopens the window; strip fires again
    await act(async () => {});

    expect(chatInput).toHaveProperty("value", "");
    expect(reasoningInput).toHaveProperty("value", "agentic-v1");

    // Switching provider now must still ask before discarding reasoning-v1's
    // draft. If the strip effect wrongly promoted it into baseline, this
    // switches immediately with no confirmation dialog.
    const providerTrigger = container.querySelector("#inference-provider") as HTMLButtonElement;
    await act(async () => {
      providerTrigger.click();
    });
    await act(async () => {});
    const ollamaItem = Array.from(document.body.querySelectorAll('[data-slot="select-item"]')).find(
      (el) => el.textContent?.includes("Ollama"),
    ) as HTMLElement | undefined;
    expect(ollamaItem).not.toBeUndefined();
    await act(async () => {
      ollamaItem?.click();
    });
    await act(async () => {});

    expect(document.body.querySelector('[data-slot="alert-dialog-content"]')).not.toBeNull();
    expect(container.querySelector<HTMLInputElement>("input#inference-model-reasoning-v1")).toHaveProperty(
      "value",
      "agentic-v1",
    );
  });
});

describe("typing a passthrough id one keystroke at a time while proxied (issue #1838 follow-up, sixth instance)", () => {
  it("does not strip an in-progress openrouter/<author>/<model> id before the operator finishes typing it", async () => {
    // The auto-strip effect exists to catch a *settled* proxy-incompatible
    // value — a catalog pick, a preset, or Remove Key's carried-over
    // override — landing in `models` in one shot. It used to run on every
    // render instead, including the renders a keystroke into the free-text
    // input this same proxied state puts on screen produces. `openrouter/
    // <author>/<model>` only reaches three segments once it is complete, so
    // typing it by hand passes through `openrouter/` and `openrouter/a` —
    // both counted as raw by segment count — and a per-render strip cleared
    // the field before those segments could ever be finished.
    //
    // Each keystroke below is dispatched with the value a real browser would
    // report: the field's *current* DOM value plus one more character — not
    // the target string sliced up front. That is what actually reproduces a
    // clear-mid-word: if the effect wipes the field between keystrokes, the
    // DOM the next keystroke appends to is already empty, and the loop ends
    // holding only the tail of the id instead of the whole thing.
    const { client } = clientFor(status("openrouter", {}, false), [
      { id: "anthropic/claude-sonnet-5", name: "Claude Sonnet" },
    ]);

    await mount(client);

    const input = container.querySelector<HTMLInputElement>("input#inference-model-chat-v1");
    expect(input).not.toBeNull();

    const target = "openrouter/anthropic/claude-sonnet-5";
    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLInputElement.prototype,
      "value",
    )?.set;
    for (const ch of target) {
      const next = (input as HTMLInputElement).value + ch;
      await act(async () => {
        setter?.call(input, next);
        input?.dispatchEvent(new Event("input", { bubbles: true }));
      });
    }

    expect(input).toHaveProperty("value", target);
  });
});
