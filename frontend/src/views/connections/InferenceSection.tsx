import { useCallback, useEffect, useRef, useState } from "react";
import { BrainCircuit, Check, Loader2, RotateCcw, Save, Trash2, Zap } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  getInferenceStatus,
  listInferenceModels,
  restartInference,
  revertInference,
  setInference,
  testInference,
  type InferenceMutation,
  type InferenceModel,
  type InferenceProvider,
  type InferenceStatus,
  type UsageMetering,
} from "@/api/inference";
import { ApiError } from "@/api/types";
import { classifyLoadFailure } from "@/lib/section-load";
import { SectionUnreachable } from "@/views/connections/SectionUnreachable";
import { Badge } from "@/components/ui/badge";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { INFERENCE_MANAGED_HIDDEN } from "@/product-scope";

/** The abstract cognition tiers the tenant model table maps. */
const TIERS = ["chat-v1", "reasoning-v1", "agentic-v1", "vision-v1"] as const;
type Tier = (typeof TIERS)[number];

/**
 * What each provider is, as data rather than as name comparisons scattered
 * through the view.
 *
 * `acceptsKey` / `requiresBaseUrl` used to be written inline as
 * `provider !== "ollama"` and `provider === "ollama" || provider ===
 * "openai_compatible"`. Both were deny-lists over a closed set: a future local
 * or credential-less provider would have inherited a key input nobody wanted
 * simply by not being named in them. Asking the descriptor means a new provider
 * declares what it needs when it is added here, and the view stops caring.
 *
 * `keyKind` is the other half of that. The credential a provider wants is
 * vendor-specific, and this field is not the only place in Connections that
 * accepts a TinyHumans key — so saying which vendor's key belongs here, per
 * provider, is what stops one being pasted into the other.
 */
const PROVIDERS: Record<
  InferenceProvider,
  {
    label: string;
    /** Whether this provider authenticates with a bearer at all. */
    acceptsKey: boolean;
    /** Whether the endpoint must be given explicitly (no useful default). */
    requiresBaseUrl: boolean;
    /** Which vendor's credential belongs in the key field, in the operator's words. */
    keyKind: string;
    /** Form defaults applied when the operator picks this provider. */
    preset: { baseUrl: string; models: Partial<Record<Tier, string>> };
  }
> = {
  managed: {
    label: "Managed (TinyHumans)",
    acceptsKey: true,
    requiresBaseUrl: false,
    // The managed brain is OpenRouter with the platform paying: the host treats
    // `managed` as a legacy alias for `openrouter` (`LEGACY_MANAGED`, since
    // OpenCompany stopped exposing its own model SKUs), so a key saved here is
    // resolved onto OpenRouter's own endpoint and sent there.
    //
    // This line used to read "a TinyHumans API key" — true when `managed` was a
    // provider of its own, and never updated when it stopped being one. It is
    // what issue #1737 actually cost: the operator was asked for one vendor's
    // key on a card that stored it against another, and every turn and every
    // Test since has presented it to OpenRouter, which rejects it (issue #1737).
    keyKind: "an OpenRouter key (`sk-or-…`) — the managed brain is OpenRouter, so that is where a key set here is sent",
    preset: { baseUrl: "", models: {} },
  },
  openrouter: {
    label: "OpenRouter",
    acceptsKey: true,
    requiresBaseUrl: false,
    keyKind: "an OpenRouter key (`sk-or-…`)",
    // Fallback only, used before the first status read resolves —
    // `presetFor` prefers the host's own `defaultTierModels` once it has
    // loaded, so this copy cannot drift from `DEFAULT_TIER_MODELS` and stay
    // wrong (issue #1838 follow-up).
    preset: {
      baseUrl: "",
      models: {
        "chat-v1": "anthropic/claude-sonnet-5",
        "reasoning-v1": "openai/gpt-5.6-sol-pro",
        "agentic-v1": "anthropic/claude-opus-5",
        "vision-v1": "qwen/qwen3.8-max",
      },
    },
  },
  ollama: {
    label: "Ollama (local)",
    acceptsKey: false,
    requiresBaseUrl: true,
    keyKind: "no key — Ollama takes no bearer",
    preset: { baseUrl: "http://localhost:11434/v1", models: { "chat-v1": "llama3.1" } },
  },
  openai_compatible: {
    label: "Custom (OpenAI-compatible)",
    acceptsKey: true,
    requiresBaseUrl: true,
    keyKind: "whatever key the endpoint below expects",
    preset: { baseUrl: "", models: {} },
  },
};

/**
 * The providers on offer. {@link PROVIDERS} keeps every descriptor, including the
 * ones not listed here — the form still needs a hidden route's `requiresBaseUrl`
 * and `preset`, and the host resolves the stored value exactly as it always did.
 *
 * Declared ahead of everything that reads it: `PROVIDER_LABEL_ITEMS` below is a
 * module-level const built through `isOffered`, so a later declaration would be
 * read in its temporal dead zone and throw on import.
 */
const PROVIDER_OPTIONS: InferenceProvider[] = (Object.keys(PROVIDERS) as InferenceProvider[]).filter(
  (p) => p !== "managed" || !INFERENCE_MANAGED_HIDDEN,
);

/**
 * What a provider this console does not offer is called on screen.
 *
 * Deliberately says nothing about the route it stands in for. The host reports
 * an unconfigured company as `provider: "managed"` — the same value a newer
 * host could send for something else entirely — and this console neither offers
 * that route nor brands it, so the honest thing to render is the fact that
 * nothing is configured *here*, which is true of every value that lands in it.
 */
const NOT_CONFIGURED = "Not configured";

/** Whether this console offers `provider` as something to choose. */
function isOffered(provider: string): provider is InferenceProvider {
  return (PROVIDER_OPTIONS as string[]).includes(provider);
}

/**
 * The label for a provider as the operator sees it.
 *
 * Never reaches into {@link PROVIDERS} for a route that is not offered: that
 * table still holds the descriptor (the form needs its `requiresBaseUrl` and
 * `preset`), but its label is a brand name for a choice this console does not
 * present, and printing it is what made the card claim a route.
 */
function providerLabel(provider: string): string {
  return isOffered(provider) ? PROVIDERS[provider].label : NOT_CONFIGURED;
}

/**
 * The base-ui `Select` wants a plain id -> label map for its `items` prop, so
 * project one out of the descriptor rather than maintaining a second list.
 *
 * Built through {@link providerLabel}, because `items` is what the *trigger*
 * renders — a filtered `SelectItem` list alone leaves the closed control still
 * showing the hidden route's own name.
 */
const PROVIDER_LABEL_ITEMS: Record<InferenceProvider, string> = Object.fromEntries(
  (Object.keys(PROVIDERS) as InferenceProvider[]).map((p) => [p, providerLabel(p)]),
) as Record<InferenceProvider, string>;

/**
 * What the live cognition path's metering mode means for the Usage view — so a
 * zero token/cost reading is legible instead of alarming (issue #174).
 */
const METERING_NOTES: Record<UsageMetering, string> = {
  perTurn: "usage metered per turn",
  perCycle: "usage metered per cycle, from what the provider reports",
  none: "no model runs on this path, so Usage stays at zero",
};

/** Per-provider form defaults applied when the operator picks a provider. */
function presetFor(
  provider: InferenceProvider,
  defaultTierModels?: Partial<Record<Tier, string>>,
): {
  baseUrl: string;
  models: Partial<Record<Tier, string>>;
} {
  const preset = PROVIDERS[provider].preset;
  // The host's own `defaultTierModels` (from `GET …/inference`) is the source
  // of truth once it has loaded; `PROVIDERS.openrouter.preset.models` is only
  // the fallback used before that first status read resolves, so switching to
  // OpenRouter still has something to prefill with immediately.
  if (provider === "openrouter" && defaultTierModels && Object.keys(defaultTierModels).length > 0) {
    return { ...preset, models: defaultTierModels };
  }
  return preset;
}

type Load = "loading" | "ready" | "unavailable" | "unconfigured" | "error";
type TestState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ok"; note: string }
  | { kind: "error"; message: string };

type ModelCatalogState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ready"; models: InferenceModel[] }
  | { kind: "empty" }
  | { kind: "error" };

/** Keep a stored custom id selectable even after the registry no longer lists it. */
function optionsForTier(catalog: InferenceModel[], current: string): InferenceModel[] {
  if (!current || catalog.some((model) => model.id === current)) return catalog;
  return [{ id: current, name: "Current custom model" }, ...catalog];
}

/**
 * The tier model select's value for "no override — fall back to
 * `model_for_tier`'s own default for this tier".
 *
 * Not `""`: an empty string is Base UI Select's own placeholder/unset
 * sentinel, so a real option needs a value of its own — the same reason
 * `AgentDetailView`'s `HARNESS_DEFAULT` exists. `""` is what `models[tier]`
 * already uses to mean "no override" on the wire (`stripProxyIncompatible`,
 * `save()`'s `cleanModels` filter), so the boundary is translated at the one
 * point that crosses it, the select's `onValueChange` below.
 *
 * Without this item, a keyed company with a saved override had no way to
 * clear it back to the tier default once the catalog loaded: the select only
 * ever offered concrete models, selecting one only replaced the override, and
 * Reset discards the whole provider configuration and key rather than one
 * tier (issue #1838 follow-up).
 */
const TIER_DEFAULT_MODEL = "__tier_default__";

/**
 * The tier model select's value for "the operator wants to type an id
 * themselves rather than pick one off the registry."
 *
 * The catalog select otherwise only ever offers ids OpenRouter's registry
 * already lists. `optionsForTier` keeps an *already-stored* custom id
 * selectable once it is there, but nothing let an operator enter a *new* one
 * once the catalog loaded — and the registry can lag a model OpenRouter only
 * just published, or list it under a slightly different slug. `model_for_tier`
 * forwards any concrete override verbatim, so the runtime has always accepted
 * an unlisted id; only the picker stopped offering a way to type one (issue
 * #1838 follow-up).
 */
const CUSTOM_MODEL = "__custom_model__";

function modelLabel(model: InferenceModel): string {
  return model.name ? `${model.name} — ${model.id}` : model.id;
}

function modelItems(models: InferenceModel[]): Record<string, string> {
  return Object.fromEntries(models.map((model) => [model.id, modelLabel(model)]));
}

/**
 * Whether `value`, once trimmed, is one of the exact two shapes the
 * platform's subscription proxy accepts for a tier override: a bare tier
 * name (`model_for_tier` reads that as "not really an override — let the
 * platform resolve this tier itself"), or the proxy's own explicit
 * three-segment `openrouter/<author>/<model>` passthrough form. Everything
 * else — including a raw OpenRouter registry id (`<author>/<model>`), which
 * `model_for_tier` forwards to the proxy verbatim and the proxy rejects — is
 * incompatible.
 *
 * Whitelisting the two accepted shapes, not blacklisting the one known-bad
 * one (issue #1838 follow-up, ninth instance): an earlier version asked "is
 * this shaped like a raw catalog id?" and treated everything else, including
 * *any* slashless string, as a safe bare-tier passthrough. `model_for_tier`
 * honours an override verbatim on the proxied path regardless of its shape
 * (see `src/company/inference.rs`), so an operator typing a real but
 * unnamespaced model id out of direct-path habit — `gpt-4o`, `llama3`, no
 * `/` in sight — read as "a bare tier id, not an override at all" and rode
 * straight through Save. The platform endpoint's curated tier registry does
 * not know `gpt-4o`; the request fails instead of the incompatible value
 * being dropped the way the console's own warning promises. Naming the tier
 * names outright closes that gap without reopening the ones the shape-based
 * check below still exists to catch.
 *
 * Shape-based rather than catalog-membership-based on purpose (issue #1838
 * follow-up, third and fourth instance): checking membership in the loaded
 * catalog only answers the question once the catalog has actually loaded,
 * and a raw `<author>/<model>` id breaks the proxy whether or not it happens
 * to appear in whatever catalog snapshot we managed to fetch — an id the
 * operator typed by hand that just isn't in the catalog (yet, or ever) fails
 * the exact same way a catalog-picked one does. Keying off the shape instead
 * means every caller gets the same, correct answer with no dependency on a
 * network fetch having already resolved.
 *
 * The `openrouter/` prefix alone is not enough to confirm the passthrough
 * shape (issue #1838 follow-up, fifth instance): OpenRouter's own registry
 * owns two-segment ids under that same author name, such as
 * `openrouter/auto`, and a plain `startsWith` mistook one for the proxy's
 * three-segment `openrouter/<author>/<slug>` form. That let `model_for_tier`
 * forward a raw registry id straight through under the same "already the
 * proxy's own form" exemption meant for ids the proxy actually accepts.
 * Counting the segments is what actually distinguishes the two shapes.
 *
 * Trimmed before any shape check (issue #1838 follow-up, seventh instance):
 * this runs inside `stripProxyIncompatible`, which `save()` calls *before*
 * its own trim pass over `models` (surrounding whitespace only gets cleaned
 * up after this check would already have run). Untrimmed, a pasted
 * ` openrouter/anthropic/model ` fails `startsWith("openrouter/")` on the
 * leading space and gets classified as incompatible — silently dropped here
 * even though it is exactly the three-segment passthrough form the proxy
 * accepts and the later trim would have normalized it to.
 */
function isProxyCompatible(value: string): boolean {
  const trimmed = value.trim();
  if ((TIERS as readonly string[]).includes(trimmed)) return true;
  if (!trimmed.includes("/")) return false;
  if (!trimmed.startsWith("openrouter/")) return false;
  return trimmed.split("/").length >= 3;
}

/**
 * Drop tier overrides that the platform's subscription proxy would reject —
 * the one place every "about to save under the proxy" call site funnels
 * through, so the rule can't drift out of sync with itself between the
 * live-edit effect, `save()`, and `removeKey()` the way three separate partial
 * implementations of it already had (issue #1838 follow-up).
 *
 * Also the one place a kept value is trimmed (issue #1838 follow-up, seventh
 * instance): `save()` trims `models` again after this runs, but `removeKey()`
 * sends `carriedModels` straight to the wire with no trim pass of its own —
 * so a kept id has to come out of here already normalized, or a pasted
 * ` openrouter/anthropic/model ` would survive Remove Key with its whitespace
 * intact even though it correctly survives the shape check.
 */
function stripProxyIncompatible<T extends string>(
  models: Partial<Record<T, string>>,
): Partial<Record<T, string>> {
  const next = {} as Partial<Record<T, string>>;
  for (const key of Object.keys(models) as T[]) {
    const value = models[key];
    if (value && isProxyCompatible(value)) next[key] = value.trim();
  }
  return next;
}

/**
 * Bring-Your-Own-Key inference (issue #56). Shows the company's effective
 * provider (with a source badge + tier→model rows + a "key set" indicator), a
 * live "Test" probe, and a switch form with per-provider presets. The key input
 * is **write-only** — it is sent on Save, stored server-side, and never read
 * back.
 *
 * A switch takes effect on the agents' next turn with no restart — *except* the
 * not-configured → configured transition, where the running brain was already
 * chosen without one. The host reports that as `restartRequired`, and this
 * section says so in the toast and keeps saying it in the status card until the
 * restart happens (issue #266).
 */
export function InferenceSection({
  client,
  company,
  canManage,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * Whether this viewer may change where the company's model calls go (issue
   * #403). Courtesy only — the host answers 403 regardless; this stops the
   * console offering a form whose Save cannot land. `Test` stays available: it
   * probes the config as already stored, and the host leaves it open.
   */
  canManage: boolean;
}) {
  const [load, setLoad] = useState<Load>("loading");
  const [status, setStatus] = useState<InferenceStatus | null>(null);
  const [busy, setBusy] = useState<
    "save" | "reset" | "test" | "removeKey" | "restart" | null
  >(null);
  const [test, setTest] = useState<TestState>({ kind: "idle" });
  const [modelCatalog, setModelCatalog] = useState<ModelCatalogState>({ kind: "idle" });

  // Switch form.
  const [provider, setProvider] = useState<InferenceProvider>("managed");
  const [baseUrl, setBaseUrl] = useState("");
  const [models, setModels] = useState<Partial<Record<Tier, string>>>({});
  /**
   * Tiers the operator switched from the catalog select into free-text entry
   * via the "Enter a model id" option, so a registry that has not caught up
   * to a newly published model does not leave them stuck picking from a list
   * that will never contain it (issue #1838 follow-up).
   */
  const [manualEntryTiers, setManualEntryTiers] = useState<ReadonlySet<Tier>>(new Set());
  const [key, setKey] = useState("");
  const [pendingProvider, setPendingProvider] = useState<InferenceProvider | null>(null);
  /**
   * The values the form last started from — a provider's preset, or what the
   * host actually holds. Anything differing from this is something the operator
   * typed, which is the only thing the destructive-draft dialog exists to
   * protect (issue #1474).
   *
   * Kept as state rather than recomputed from `presetFor(provider)` because the
   * form no longer always starts at a preset: seeding it from the stored
   * configuration means the baseline can be a saved endpoint and model table
   * that no preset matches, and comparing those against the preset would ask an
   * operator to confirm discarding a draft nobody wrote.
   */
  const [baseline, setBaseline] = useState<{
    baseUrl: string;
    models: Partial<Record<Tier, string>>;
  }>({ baseUrl: "", models: {} });

  /**
   * Point the switch form at what the host actually holds.
   *
   * Without this the Provider select was a constant: `useState("managed")` and
   * nothing ever wrote it back, so the card opened on "Managed (TinyHumans)"
   * whatever was stored — on first load, after a Save, and after a full process
   * restart, which re-runs the same initializer. The header beside it renders
   * the *host's* provider, so the two named different providers on the same
   * card at the same moment, and an operator could store a key against a
   * provider they never chose without anything on screen saying so (#1737).
   *
   * Seeded from `status.provider` verbatim, never from a mapping of our own:
   * that value is what the header renders, so taking it as-is is what makes
   * "the header and the select can never disagree" structural rather than
   * something to keep in step by hand.
   *
   * `baseUrl` is seeded only for the providers that require one. `managed` and
   * `openrouter` resolve theirs from the environment or a well-known default,
   * and putting the *resolved* value in the form would pin the company to it on
   * the next Save — silently overriding `OPENCOMPANY_INFERENCE_URL`. Same
   * reasoning as `removeKey` below.
   */
  const seedFromStatus = useCallback((next: InferenceStatus) => {
    const stored = (
      next.provider in PROVIDERS ? next.provider : "openrouter"
    ) as InferenceProvider;
    // The form is a "switch to" form, so it opens on something the operator can
    // actually complete. A company on a route this console does not offer has no
    // provider to reflect here, and resting on an inert row leaves onboarding
    // with no way forward — the card's header above still reports the real state.
    const seeded = isOffered(stored) ? stored : PROVIDER_OPTIONS[0];
    const nextBaseUrl = PROVIDERS[seeded].requiresBaseUrl
      ? next.baseUrl
      : presetFor(seeded).baseUrl;
    const nextModels = next.models as Partial<Record<Tier, string>>;
    setProvider(seeded);
    setBaseUrl(nextBaseUrl);
    setModels(nextModels);
    setBaseline({ baseUrl: nextBaseUrl, models: nextModels });
  }, []);

  const refresh = useCallback(async () => {
    try {
      const next = await getInferenceStatus(client, company);
      setStatus(next);
      // Safe to reseed on every refresh because `refresh` runs on mount and
      // after a *completed* mutation only — there is no poll here, so this can
      // never pull a draft out from under someone mid-edit.
      seedFromStatus(next);
      setLoad("ready");
    } catch (err) {
      // A 404 is a host with no inference plane; anything else is a host that
      // could not answer, and this is the section an operator opens when the
      // company has stopped working — precisely when a transient error is most
      // likely and disappearing is least helpful (issue #1470).
      setLoad(classifyLoadFailure(err));
    }
  }, [client, company, seedFromStatus]);

  useEffect(() => {
    setLoad("loading");
    void refresh();
  }, [refresh]);

  useEffect(() => {
    let current = true;
    if (provider !== "openrouter") {
      setModelCatalog({ kind: "idle" });
      return () => {
        current = false;
      };
    }

    setModelCatalog({ kind: "loading" });
    void listInferenceModels(client, company)
      .then((catalog) => {
        if (!current) return;
        setModelCatalog(catalog.length ? { kind: "ready", models: catalog } : { kind: "empty" });
      })
      .catch(() => {
        if (current) setModelCatalog({ kind: "error" });
      });

    return () => {
      current = false;
    };
  }, [client, company, provider]);

  /**
   * Whether saving right now would ride the platform's subscription proxy
   * rather than go straight to OpenRouter with a tenant key.
   *
   * The proxy only resolves an abstract tier (or its own `openrouter/…`
   * passthrough form, off by default) — see `model_for_tier` in
   * `src/company/inference.rs`. The catalog picker below stores the
   * registry's raw `<author>/<model>` id verbatim, which is exactly the form
   * the proxy rejects. `key` is write-only and never comes back from the
   * host, so "no key typed" only means proxied when the stored config is not
   * already a direct, keyed OpenRouter connection.
   *
   * Hoisted above the early return below so the clear-on-transition effect
   * that follows can depend on it — hooks cannot come after a conditional
   * return.
   */
  const wouldSaveProxied =
    key.trim().length === 0 && !(status?.provider === "openrouter" && status.keyConfigured);

  /**
   * Drop a catalog-shaped tier override the moment the form would save
   * proxied (issue #1838 follow-up).
   *
   * A keyless company can type a key, pick a model from the catalog select —
   * which stores the registry's raw `<author>/<model>` id verbatim — and then
   * clear the key again before Save. `wouldSaveProxied` flips back to `true`
   * and the free-text input reappears, but nothing cleared the value it
   * inherited from the select, so Save still persists that raw id as an
   * override. `model_for_tier` honours overrides verbatim on *both* paths, so
   * the proxy receives a model namespace it rejects. The same thing happens
   * without ever touching a key at all: switching a keyless company onto
   * OpenRouter installs that provider's raw-id presets (`PROVIDERS.openrouter
   * .preset.models`) up front, and Save is not disabled while the catalog is
   * still loading or has failed.
   *
   * Runs off `stripProxyIncompatible`'s shape check, not catalog membership —
   * an earlier version of this effect only stripped a value present in the
   * *loaded* catalog, so it did nothing while `modelCatalog.kind` was
   * `"loading"` or `"error"`, letting a raw preset or catalog-picked id ride
   * straight through Save during that window (issue #1838 follow-up, third
   * instance). A tier id the operator typed by hand that is not a raw
   * `<author>/<model>` id — a bare tier passthrough, or the proxy's own
   * `openrouter/<author>/<model>` form — is left alone either way.
   *
   * Folds the strip into `baseline` in the same pass, not just `models` —
   * but only for the tiers the strip actually touched. `pickProvider` seeds
   * both together from the same preset, so skipping `baseline` here left it
   * holding the raw ids this effect had just stripped out of `models` — and
   * `hasTypedDraft()` reads any such gap as an operator draft, however it
   * got there. That flipped a same-tick `pickProvider("openrouter")` →
   * `pickProvider(<something else>)` (typing a key in between, never
   * touching Base URL or a model field) into an unwanted "Replace the
   * provider draft?" confirmation, because this effect's own cleanup — not
   * anything the operator typed — was the entire difference `hasTypedDraft()`
   * saw. An automatic proxy-safety strip is no more a typed draft than the
   * preset it is correcting; `baseline` has to move with it for the same
   * reason it moves with the preset itself.
   *
   * Merging only the *changed* tiers into `baseline.models` (not replacing
   * the whole map with `next`) matters because `next` is a full snapshot of
   * every tier, stripped or not. A keyless operator can edit several tiers
   * in the same proxied window — say a hand-typed passthrough id on one
   * tier that `stripProxyIncompatible` leaves alone, alongside a raw
   * catalog id on another that it strips. Setting `baseline.models = next`
   * wholesale would silently promote that untouched tier's draft into the
   * baseline too, since `next` carries it through unchanged — and
   * `hasTypedDraft()` would then see baseline and draft agree and let a
   * provider switch discard it without the confirmation dialog, even though
   * the operator never asked for that edit to become the new baseline
   * (issue #1838 follow-up, eighth instance).
   *
   * Latched to the *edge* of entering a proxied window, not every render
   * inside one (issue #1838 follow-up, sixth instance). `models` has to stay
   * a dependency — it is what changed when a catalog pick or Remove Key's
   * carry-over needs catching — but that means this effect also re-runs on
   * every keystroke into the free-text input `wouldSaveProxied` itself put on
   * screen (`useFreeText` below), because typing calls `setModel` the same as
   * any other write to `models`. `openrouter/<author>/<model>` only reaches
   * three segments once the id is complete, so a strip that fires on every
   * render caught the field mid-word — after `openrouter/`, after
   * `openrouter/a` — and cleared it before an operator typing one by hand
   * could ever finish it. `stripProxyIncompatible` normalizes a *settled*
   * value; applying it while the value is still being composed is the bug,
   * not a stricter version of the fix. `strippedForWindow` records that the
   * strip already ran for the current proxied window and skips re-running it
   * until the window closes (provider leaves `openrouter`, or a key makes
   * `wouldSaveProxied` false again) and a new one opens — which is exactly
   * the set of transitions (`pickProvider`, typing then clearing a key) this
   * effect exists to catch.
   */
  const strippedForWindow = useRef(false);
  useEffect(() => {
    if (provider !== "openrouter" || !wouldSaveProxied) {
      strippedForWindow.current = false;
      return;
    }
    if (strippedForWindow.current) return;
    strippedForWindow.current = true;
    const next = stripProxyIncompatible(models);
    const changedTiers = TIERS.filter((tier) => (next[tier] ?? "") !== (models[tier] ?? ""));
    if (changedTiers.length === 0) return;
    setModels(next);
    setBaseline((b) => ({
      ...b,
      models: {
        ...b.models,
        ...Object.fromEntries(changedTiers.map((tier) => [tier, next[tier]])),
      },
    }));
  }, [provider, wouldSaveProxied, models]);

  function pickProvider(next: InferenceProvider) {
    setProvider(next);
    const preset = presetFor(next, status?.defaultTierModels);
    setBaseUrl(preset.baseUrl);
    setModels(preset.models);
    setBaseline({ baseUrl: preset.baseUrl, models: preset.models });
    setTest({ kind: "idle" });
    setManualEntryTiers(new Set());
  }

  /**
   * Whether the operator has typed anything into the Base URL or model fields
   * since the form last started from a known baseline.
   *
   * The destructive-draft dialog exists to protect values the operator typed —
   * the copy promises "confirmation is only required for values the operator
   * typed", and neither a provider's *preset* nor the configuration the host
   * already holds is a typed draft. OpenRouter pre-fills model ids and Ollama a
   * local base URL, so switching away from either without editing anything must
   * not ask to discard a draft nobody wrote (issue #1474) — and since the form
   * is now seeded from the stored configuration, a saved endpoint and model
   * table must not either.
   */
  function hasTypedDraft(): boolean {
    return (
      baseUrl !== baseline.baseUrl ||
      TIERS.some((tier) => (models[tier] ?? "") !== (baseline.models[tier] ?? ""))
    );
  }

  function requestProvider(next: InferenceProvider) {
    if (next === provider) return;
    if (hasTypedDraft()) {
      setPendingProvider(next);
      return;
    }
    pickProvider(next);
  }

  function setModel(tier: Tier, value: string) {
    setModels((m) => ({ ...m, [tier]: value }));
  }

  async function save() {
    if (busy) return;
    setBusy("save");
    try {
      // "Managed" with no company key — neither typed now nor already stored —
      // means "use the platform default", which is a revert rather than a
      // runtime override. With a key it is a real override (issue #585): the
      // company pays for its own agents on the platform brain, so the override
      // has to be written or the key would be stored and then ignored.
      //
      // This is what retires issue #265's refusal guard. That guard existed
      // because a managed save was *only* ever a revert, and a revert cannot
      // carry a credential, so a typed key would have been dropped while the
      // toast claimed success. The invariant it protected — a save that reports
      // success never discarded what the operator typed — is what this branch
      // now upholds directly, by storing the key instead of refusing it.
      let result: InferenceMutation;
      if (provider === "managed" && !key.trim() && !status?.keyConfigured) {
        result = await revertInference(client, company);
      } else {
        // Belt-and-suspenders alongside the live-edit effect above: that
        // effect strips a proxy-incompatible draft value out of `models`
        // asynchronously, after the state update that made
        // `wouldSaveProxied` true has committed. Re-applying the same
        // shape-based strip here means Save can never persist a raw
        // catalog id under the proxy even in the window before that effect
        // has run (issue #1838 follow-up).
        const draftModels =
          provider === "openrouter" && wouldSaveProxied
            ? stripProxyIncompatible(models)
            : models;
        const cleanModels = Object.fromEntries(
          Object.entries(draftModels)
            .map(([t, v]) => [t, (v ?? "").trim()])
            .filter(([, v]) => v.length > 0),
        );
        result = await setInference(client, company, {
          provider,
          baseUrl: baseUrl.trim() || undefined,
          models: Object.keys(cleanModels).length ? cleanModels : undefined,
          key: key.trim() || undefined,
        });
      }
      // Issue #266: only the host knows whether the *running* brain can act on
      // what was just saved. Which brain a company runs is fixed when it is
      // built, so a company that started with no inference source keeps echoing
      // no matter what lands here — "teammates use it on their next turn" was a
      // promise the runtime could not keep for exactly the transition an
      // operator makes first. Follow the response instead of asserting.
      if (result.status.restartRequired) {
        toast.warning("Inference saved — restart the company for teammates to use it.", {
          description: result.note,
        });
      } else {
        toast.success("Inference updated. Teammates use it on their next turn.");
      }
      setKey("");
      setTest({ kind: "idle" });
      await refresh();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't update inference.");
    } finally {
      setBusy(null);
    }
  }

  /** Clears the stored company key without changing the effective provider. */
  async function removeKey() {
    if (busy) return;
    setBusy("removeKey");
    const effective = (status?.provider as InferenceProvider) ?? provider;
    try {
      let carriedModels =
        status && Object.keys(status.models).length ? status.models : undefined;
      // Clearing the key on OpenRouter flips this company onto the platform
      // proxy immediately (`wouldSaveProxied` above). A catalog-picked raw
      // `<author>/<model>` id is exactly what that proxy rejects —
      // `model_for_tier` honours an override verbatim on both paths. The
      // live-edit effect above already drops that id out of the *draft form*
      // the moment the transition happens there, but Remove Key echoes back
      // `status.models` — the *stored* config — straight past that effect, so
      // an id saved while keyed survives this transition and breaks every
      // proxied call afterwards (issue #1838 follow-up).
      //
      // Shape-based (`stripProxyIncompatible`), not a fresh catalog fetch: an
      // earlier version of this block fetched the registry here and, on
      // failure, fell back to sending the stored overrides completely
      // unfiltered — which is exactly the condition an unreachable registry
      // produces, so the one case this fallback existed for was also the one
      // case guaranteed to break every proxied tier (issue #1838 follow-up,
      // fourth instance). `stripProxyIncompatible` needs nothing from the
      // network, so there is no failure mode left to fall back from, and a
      // tier id the operator typed by hand (not shaped like a raw catalog id)
      // is still left alone, same as the live-edit effect.
      if (effective === "openrouter" && carriedModels) {
        const kept = stripProxyIncompatible(carriedModels);
        carriedModels = Object.keys(kept).length ? (kept as Record<string, string>) : undefined;
      }
      await setInference(client, company, {
        provider: effective,
        // Only echo the resolved base URL back for the providers that require
        // one. `managed` and `openrouter` resolve theirs from the environment or
        // a well-known default, and persisting the *displayed* value would pin
        // the company to it — silently overriding `OPENCOMPANY_INFERENCE_URL`.
        baseUrl: PROVIDERS[effective]?.requiresBaseUrl ? status?.baseUrl || undefined : undefined,
        models: carriedModels,
        key: "",
      });
      toast.success("Removed the company key. Teammates fall back on their next turn.");
      setKey("");
      setTest({ kind: "idle" });
      await refresh();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't remove the key.");
    } finally {
      setBusy(null);
    }
  }

  /**
   * Rebuild the company's runtime now, clearing the restart-required state.
   *
   * The banner used to name a restart and offer no way to perform one, which is
   * a dead end for exactly the operator most likely to hit it: a hosted tenant
   * cannot restart its own container, and the control plane has no button for
   * it either.
   *
   * The resulting status is read from the response rather than assumed. A host
   * that wired no rebuilder genuinely cannot do this and says so, and reporting
   * success there would replace a visible dead end with an invisible one.
   *
   * Since #1736 the button is only rendered where `canRebuildInPlace` holds, so
   * that arm is defence in depth rather than the everyday path: the capability
   * is read at load and the rebuilder could still be absent by the time the
   * click lands.
   */
  async function restartNow() {
    if (busy) return;
    setBusy("restart");
    try {
      const result = await restartInference(client, company);
      if (result.status.restartRequired) {
        // The rebuild ran and the company is still on the old brain. Follow the
        // host's note, which names the process restart that would work.
        toast.warning("Still needs a restart.", { description: result.note });
      } else {
        toast.success("Restarted. Teammates think with the new provider from their next turn.");
      }
      setStatus(result.status);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't restart the company.");
    } finally {
      setBusy(null);
    }
  }

  async function reset() {
    if (busy) return;
    setBusy("reset");
    try {
      const result = await revertInference(client, company);
      // The host names what the company actually falls back to: its committed
      // manifest `[inference]`, or the platform default when the manifest
      // declares none. Claiming "managed" here would be wrong for a company
      // whose manifest names its own provider (issue #1474).
      toast.success(result.note);
      setKey("");
      setTest({ kind: "idle" });
      // The form is not reset to a guess here: `refresh` reseeds it from what
      // the host holds after the revert, which for a company whose manifest
      // names its own provider is not `managed` (issue #1474). Assuming a
      // provider here is what put a value in the select that the header
      // disagreed with (issue #1737).
      await refresh();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't revert inference.");
    } finally {
      setBusy(null);
    }
  }

  async function probe() {
    if (busy) return;
    setBusy("test");
    setTest({ kind: "loading" });
    try {
      const result = await testInference(client, company);
      if (result.ok) {
        setTest({ kind: "ok", note: result.note ?? "Reached the provider." });
      } else {
        setTest({ kind: "error", message: result.error ?? "The provider did not respond." });
      }
    } catch (err) {
      setTest({
        kind: "error",
        message: err instanceof ApiError ? err.message : "The probe failed.",
      });
    } finally {
      setBusy(null);
    }
  }

  if (load === "unavailable") return null;

  const modelRows = status ? Object.entries(status.models) : [];
  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2">
        <BrainCircuit className="size-4 text-muted-foreground" />
        <h2 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Inference (BYOK)
        </h2>
        {/* Mirrors the Company credential card's own subtitle. Two cards in
            Connections accept a key and one of them accepts a TinyHumans key,
            so each says in one line which it is — the distinction is otherwise
            only in a module doc no operator reads (#637). */}
        <span className="text-xs text-muted-foreground">
          the key your teammates think with — not the company account key
        </span>
      </div>
      <p className="text-sm text-muted-foreground">
        Choose which model provider your teammates think with. Bring your own key for OpenRouter, a
        custom OpenAI-compatible endpoint, or a local Ollama server — the key is stored securely and
        never shown again. Switching provider or model takes effect on the teammates' next turn.
        Giving inference to a company that started without any does not: the brain is chosen at
        startup, so that first setup needs a restart.
      </p>

      {load === "loading" ? (
        <Skeleton className="h-40 rounded-xl" />
      ) : load === "error" ? (
        <SectionUnreachable label="Couldn't read this company's inference settings" />
      ) : (
        <Card>
          <CardContent className="space-y-4">
            {/* Status card. */}
            {status && (
              <div className="space-y-2">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium" data-testid="inference-current-provider">
                    {providerLabel(status.provider)}
                  </span>
                  {/* `source` reads `managed` for a company this console has no
                      route for, which is the hidden route's own name. The badge
                      says where a configuration came from, and there is no
                      configuration to attribute. */}
                  {isOffered(status.provider) && (
                    <Badge variant={status.source === "runtime" ? "outline" : "secondary"}>
                      {status.source}
                    </Badge>
                  )}
                  {status.keyConfigured && (
                    <span className="inline-flex items-center gap-1 text-xs text-status-done-text">
                      <Check className="size-3" /> key set
                    </span>
                  )}
                  <Button
                    variant="ghost"
                    size="sm"
                    className="ml-auto"
                    disabled={busy !== null}
                    onClick={() => void probe()}
                  >
                    {busy === "test" ? <Loader2 className="size-4 animate-spin" /> : <Zap className="size-4" />}
                    Test
                  </Button>
                </div>
                <p className="text-xs text-muted-foreground">
                  Test sends one real message to this provider using its stored key, and your provider
                  may charge for it; it does not change the saved configuration. A company with no
                  custom inference configured has nothing to send, and Test just reports that
                  instead of sending anything.
                </p>
                {isOffered(status.provider) && (
                  <p className="truncate text-xs text-muted-foreground">{status.baseUrl}</p>
                )}
                {/* Issue #174: config resolving to a provider does not mean the
                    company booted onto it. Say which cognition path is live and
                    whether its usage is metered, so a zero Usage reading reads as
                    "nothing was spent" rather than "accounting is broken". */}
                <p className="text-xs text-muted-foreground">
                  Cognition: <span className="font-mono">{status.cognition}</span> ·{" "}
                  {METERING_NOTES[status.usageMetering] ?? "usage metering unknown"}
                </p>
                {/* Issue #266: a saved config the running brain cannot act on.
                    The toast that says so is gone in seconds — and an operator
                    who reloads the page, or comes back tomorrow, sees only a
                    correct-looking provider next to agents that still echo. This
                    is the surface that stays until the restart happens. */}
                {status.restartRequired && (
                  <div
                    className="flex items-start gap-1.5 rounded-lg border border-status-blocked/40 bg-status-blocked-soft px-3 py-2 text-xs text-status-blocked-text"
                    data-testid="inference-restart-required"
                  >
                    <RotateCcw className="mt-px size-3.5 shrink-0" />
                    <div className="space-y-2">
                      <span>
                        <span className="font-medium">Restart required.</span> This company started
                        with no inference source, so it is running the offline echo brain and its
                        scheduled workflows cannot fire. The brain is chosen at startup — this
                        configuration is saved, but teammates keep echoing until the company is
                        restarted.
                      </span>
                      {/* The action, not just the diagnosis. Telling a hosted
                          operator to "restart the company" names something they
                          have no way to do — the container is the unit of
                          restart and the control plane has no button for it —
                          so this rebuilds the runtime in place instead (#290).
                          Only rendered for an admin, matching the route's own
                          authority check, so a member is not shown a control
                          that can only 403.

                          And only on a host that can honour it. `POST
                          …/inference/restart` needs a `RuntimeRebuilder` wired
                          into the host; where none is, it fails
                          unconditionally with "this host cannot rebuild a
                          company runtime in place". This card used to render
                          the button anyway, so the operator was told a restart
                          was required, handed the control for it, and the
                          control could only ever fail (#1736). */}
                      {status.canRebuildInPlace ? (
                        canManage && (
                          <div className="space-y-1">
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={busy !== null}
                              data-testid="inference-restart-now"
                              onClick={() => void restartNow()}
                            >
                              {busy === "restart" ? (
                                <Loader2 className="size-3.5 animate-spin" />
                              ) : (
                                <RotateCcw className="size-3.5" />
                              )}
                              Restart now
                            </Button>
                            <p className="text-xs">
                              The current turn finishes; journals, parked approvals, and single-use grants
                              carry over to the replacement runtime.
                            </p>
                          </div>
                        )
                      ) : (
                        /* Named for everyone, admin or not: it is the only
                           thing that changes this state, and it is not an
                           action the console can take on anyone's behalf.
                           Both spellings are given because the host reports
                           the capability, not the shell it is packaged in —
                           and a desktop console can be pointed at a remote
                           host, so guessing from the window would name the
                           wrong one. */
                        <p className="text-xs" data-testid="inference-restart-manual">
                          This host cannot restart a company on its own, so the saved configuration
                          is picked up the next time OpenCompany starts: quit and reopen the app, or
                          restart the server process it runs on.
                        </p>
                      )}
                    </div>
                  </div>
                )}
                {modelRows.length > 0 && (
                  <ul className="space-y-1 rounded-md bg-muted/40 p-2">
                    {modelRows.map(([tier, model]) => (
                      <li key={tier} className="text-xs">
                        <span className="font-mono font-medium">{tier}</span>
                        <span className="text-muted-foreground"> → {model}</span>
                      </li>
                    ))}
                  </ul>
                )}
                {test.kind === "ok" && (
                  <p className="flex items-center gap-1 text-xs text-status-done-text">
                    <Check className="size-3" /> {test.note}
                  </p>
                )}
                {test.kind === "error" && <p className="text-xs text-destructive">{test.message}</p>}
                {test.kind === "loading" && (
                  <p className="flex items-center gap-1 text-xs text-muted-foreground">
                    <Loader2 className="size-3 animate-spin" /> Probing the provider…
                  </p>
                )}
              </div>
            )}

            {/* Switch form. */}
            {/* The switch form is an admin's: it decides the base URL every
                agent's prompts travel to and the key they are billed against
                (issue #403). */}
            {canManage && (
              <div className="space-y-3 border-t border-border pt-3">
                <div className="grid gap-2 sm:grid-cols-2 sm:items-end">
                  <div className="space-y-1">
                    <Label htmlFor="inference-provider" className="text-xs">
                      Provider
                    </Label>
                    <Select
                      value={provider}
                      onValueChange={(v) => requestProvider(v as InferenceProvider)}
                      items={PROVIDER_LABEL_ITEMS}
                    >
                      <SelectTrigger id="inference-provider" className="w-full">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        {PROVIDER_OPTIONS.map((p) => (
                          <SelectItem key={p} value={p}>
                            {PROVIDERS[p].label}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                    {isOffered(status?.provider ?? "") ? (
                      <p className="text-xs text-muted-foreground">
                        Choosing a provider applies its Base URL and model defaults. If you have
                        typed either, we ask before replacing them; your API key stays in this form.
                      </p>
                    ) : (
                      <p
                        className="text-xs text-muted-foreground"
                        data-testid="inference-not-configured"
                      >
                        No provider is configured for this company yet. Pick one above and paste its
                        key below to give its teammates a brain of their own.
                      </p>
                    )}
                  </div>
                  {PROVIDERS[provider].requiresBaseUrl && (
                    <div className="space-y-1">
                      <Label htmlFor="inference-base-url" className="text-xs">
                        Base URL
                      </Label>
                      <Input
                        id="inference-base-url"
                        value={baseUrl}
                        placeholder="https://host/v1"
                        onChange={(e) => setBaseUrl(e.target.value)}
                      />
                    </div>
                  )}
                </div>

                {isOffered(provider) && (
                  <div className="space-y-2">
                    {provider === "openrouter" && modelCatalog.kind === "error" && (
                      <p
                        className="text-xs text-muted-foreground"
                        data-testid="inference-model-catalog-fallback"
                      >
                        OpenRouter&apos;s model list could not be loaded. Enter model ids directly.
                      </p>
                    )}
                    {provider === "openrouter" && modelCatalog.kind === "empty" && (
                      <p
                        className="text-xs text-muted-foreground"
                        data-testid="inference-model-catalog-empty"
                      >
                        OpenRouter returned no models. Enter model ids directly.
                      </p>
                    )}
                    {/*
                      `kind !== "idle"` is a no-op here — the effect above
                      only ever sets "idle" when `provider !== "openrouter"`,
                      which the surrounding check already excludes. Left in
                      as a defensive guard against that invariant changing,
                      not a live branch.

                      "loading" used to be excluded like "idle", but a
                      keyless company's free-text input (`useFreeText` below
                      is true here because `wouldSaveProxied` is one of its
                      conditions) is already editable and Save-able during a
                      cold catalog fetch, which can run for up to ten
                      seconds. Withholding the warning until "ready" left
                      that whole window silent: an id typed and saved while
                      still loading was dropped by `stripProxyIncompatible`
                      with no explanation (issue #1838 follow-up).
                    */}
                    {provider === "openrouter" &&
                      modelCatalog.kind !== "idle" &&
                      wouldSaveProxied && (
                        <p
                          className="text-xs text-muted-foreground"
                          data-testid="inference-model-catalog-proxied"
                        >
                          Without an OpenRouter key this runs on the shared subscription, which only
                          resolves a tier name. A specific model id typed above will be dropped on
                          Save. Add a key to pick a specific model, or enter a tier id directly.
                        </p>
                      )}
                    <div className="grid gap-2 sm:grid-cols-2">
                      {TIERS.map((tier) => {
                        const value = models[tier] ?? "";
                        // Whether this tier is in manual entry *by the
                        // operator's own choice* — as opposed to the other
                        // `useFreeText` reasons below, which are the catalog
                        // giving out on its own and offer no way back to a
                        // select this render.
                        const manualEntry = manualEntryTiers.has(tier);
                        const useFreeText =
                          provider !== "openrouter" ||
                          modelCatalog.kind === "error" ||
                          modelCatalog.kind === "empty" ||
                          wouldSaveProxied ||
                          manualEntry;
                        const options =
                          modelCatalog.kind === "ready"
                            ? optionsForTier(modelCatalog.models, value)
                            : value
                              ? [{ id: value }]
                              : [];

                        return (
                          <div key={tier} className="space-y-1">
                            <Label htmlFor={`inference-model-${tier}`} className="text-xs">
                              {tier}
                            </Label>
                            {useFreeText ? (
                              <div className="space-y-1">
                                <Input
                                  id={`inference-model-${tier}`}
                                  value={value}
                                  placeholder="provider model id"
                                  onChange={(e) => setModel(tier, e.target.value)}
                                />
                                {/* Only the operator's own "enter a model id"
                                    choice gets a way back — the other
                                    `useFreeText` reasons (no key, no catalog)
                                    have no select to return to this render. */}
                                {manualEntry && modelCatalog.kind === "ready" && (
                                  <button
                                    type="button"
                                    className="text-xs text-muted-foreground underline-offset-2 hover:underline"
                                    data-testid={`inference-model-back-to-catalog-${tier}`}
                                    onClick={() =>
                                      setManualEntryTiers((prev) => {
                                        const next = new Set(prev);
                                        next.delete(tier);
                                        return next;
                                      })
                                    }
                                  >
                                    Choose from the OpenRouter catalog instead
                                  </button>
                                )}
                              </div>
                            ) : (
                              <Select
                                value={value || null}
                                disabled={modelCatalog.kind !== "ready"}
                                onValueChange={(next) => {
                                  if (!next) return;
                                  if (next === CUSTOM_MODEL) {
                                    setManualEntryTiers((prev) => new Set(prev).add(tier));
                                    return;
                                  }
                                  setModel(tier, next === TIER_DEFAULT_MODEL ? "" : String(next));
                                }}
                                items={{
                                  [TIER_DEFAULT_MODEL]: "Use the tier default",
                                  [CUSTOM_MODEL]: "Enter a model id…",
                                  ...modelItems(options),
                                }}
                              >
                                <SelectTrigger
                                  id={`inference-model-${tier}`}
                                  className="w-full"
                                  data-testid={`inference-model-select-${tier}`}
                                >
                                  <SelectValue
                                    placeholder={
                                      modelCatalog.kind === "loading"
                                        ? "Loading OpenRouter models…"
                                        : "Choose a model"
                                    }
                                  />
                                  {modelCatalog.kind === "loading" && (
                                    <Loader2 className="size-3.5 animate-spin" />
                                  )}
                                </SelectTrigger>
                                <SelectContent>
                                  {value && (
                                    <SelectItem
                                      value={TIER_DEFAULT_MODEL}
                                      data-testid={`inference-model-clear-${tier}`}
                                    >
                                      <span>Use the tier default</span>
                                    </SelectItem>
                                  )}
                                  {options.map((model) => (
                                    <SelectItem key={model.id} value={model.id}>
                                      <span>{model.name ?? model.id}</span>
                                      {model.name && (
                                        <span className="font-mono text-xs text-muted-foreground">
                                          {model.id}
                                        </span>
                                      )}
                                    </SelectItem>
                                  ))}
                                  {/* The registry can lag a model OpenRouter
                                      only just published — this is the way out
                                      of "pick from what the catalog happens to
                                      list today" (issue #1838 follow-up). */}
                                  <SelectItem
                                    value={CUSTOM_MODEL}
                                    data-testid={`inference-model-custom-${tier}`}
                                  >
                                    <span>Enter a model id…</span>
                                  </SelectItem>
                                </SelectContent>
                              </Select>
                            )}
                          </div>
                        );
                      })}
                    </div>
                  </div>
                )}

                {/*
                  The key field is offered for `managed` too (issue #585). The
                  company's TinyHumans key is the admin's to set, and hiding the
                  input here was the whole reason it could only arrive as a
                  deploy-time environment variable — `resolve_endpoint` has
                  always preferred a stored key over the env default on this
                  provider. Ollama is the one provider that takes no bearer.
                */}
                {/* No key field until a provider is chosen. The descriptor for a
                    route this console does not offer still carries its `keyKind`
                    prose, which names that route — and a key typed against a
                    provider nobody selected has nowhere to be scoped to. */}
                {isOffered(provider) && PROVIDERS[provider].acceptsKey && (
                  <div className="space-y-1">
                    <Label htmlFor="inference-key" className="text-xs">
                      API key {status?.keyConfigured ? "(leave blank to keep)" : ""}
                    </Label>
                    <Input
                      id="inference-key"
                      type="password"
                      value={key}
                      placeholder="write-only"
                      autoComplete="off"
                      onChange={(e) => setKey(e.target.value)}
                    />
                    <p className="text-xs text-muted-foreground" data-testid="inference-key-note">
                      Paste {PROVIDERS[provider].keyKind}. This field is scoped to the provider
                      selected above and is stored against it — a key for any other vendor will
                      fail at the first turn, so it is not the place for one.
                    </p>
                    <p className="text-xs text-muted-foreground">
                      Whatever you set here is what the company spends against: everyone you invite
                      spends on it — inference, embeddings, tools and capabilities all bill to this
                      one account, and spend cannot be attributed back to individual members.
                      Removing someone from the roster stops their future access; it does not
                      separate what they already spent.
                    </p>
                  </div>
                )}

                <div className="flex items-center gap-2">
                  <Button
                    data-testid="inference-save"
                    disabled={busy !== null}
                    onClick={() => void save()}
                  >
                    {busy === "save" ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
                    Save
                  </Button>
                  <Button variant="outline" disabled={busy !== null} onClick={() => void reset()}>
                    {busy === "reset" ? <Loader2 className="size-4 animate-spin" /> : <RotateCcw className="size-4" />}
                    Reset to default
                  </Button>
                  {status?.keyConfigured && (
                    <Button
                      variant="ghost"
                      className="text-destructive"
                      data-testid="inference-remove-key"
                      disabled={busy !== null}
                      onClick={() => void removeKey()}
                    >
                      {busy === "removeKey" ? (
                        <Loader2 className="size-4 animate-spin" />
                      ) : (
                        <Trash2 className="size-4" />
                      )}
                      Remove key
                    </Button>
                  )}
                </div>
                <p className="text-xs text-muted-foreground">
                  Reset removes this company&apos;s provider override, endpoint, model choices, and stored
                  key, then falls back to the committed manifest configuration — or the platform
                  default when the manifest declares none. Remove key keeps the displayed
                  provider, endpoint, and models as a runtime override, but clears only its stored key.
                  Both changes apply on teammates&apos; next turn unless this card says a restart is required.
                </p>
              </div>
            )}
          </CardContent>
        </Card>
      )}
      <AlertDialog open={pendingProvider !== null} onOpenChange={(open) => !open && setPendingProvider(null)}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Replace the provider draft?</AlertDialogTitle>
            <AlertDialogDescription>
              Choosing {pendingProvider ? PROVIDERS[pendingProvider].label : "this provider"} replaces
              the typed Base URL and model fields with that provider&apos;s defaults. Your API key stays in
              the form until you save or leave this page.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Keep draft</AlertDialogCancel>
            <AlertDialogAction
              onClick={() => {
                if (pendingProvider) pickProvider(pendingProvider);
                setPendingProvider(null);
              }}
            >
              Replace fields
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </section>
  );
}
