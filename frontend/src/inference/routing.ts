// Manage Routing, as decisions rather than markup.
//
// Every branch the routing screen makes lives here, as a plain function over
// plain data: which mode the current routes describe, whether two rows point at
// the same thing, what a removal orphans, how a row reads. The components are
// then layout plus handlers, with nothing in them that deserves a test of its
// own. This is the shape `frontend/src/search/` uses and it is the closest
// worked example in this console.
//
// The Rust side holds the same rules for the turn path (`inference::resolve`).
// These are not a second opinion: the console needs them to render a row before
// a request is made, and the host needs them to decide where a turn goes. Where
// they overlap they are written to agree, and the ones that matter — the mode
// inference and the three scrub rules — are pinned on both sides.

import { MANAGED_OPTION_SLUG } from "./connect";
import { overrideIsSendable } from "./proxy-compat";
import type { Provider, ProviderRef, RoutingMap, RoutingMode, Workload } from "./types";

/**
 * The workloads with a row of their own — one per abstract tier the runtime
 * actually has.
 *
 * The design being ported lists nine, in two groups. Five of those have no
 * equivalent here: `memory`, `heartbeat`, `learning` and `subconscious` are
 * background loops OpenCompany does not run, and shipping four empty rows would
 * be a screen that lies about what the product does.
 *
 * `coding` is the fifth, and it is absent for a different and sharper reason:
 * it maps onto the same `agentic-v1` tier as `agentic`, so an editable coding
 * row would write one tier's route under two names. Setting one would silently
 * change the other — which is the inheritance bug this module exists to prevent,
 * wearing a different hat. It resolves through the agentic route as an alias,
 * and it gets a row when a coding tier exists to give it.
 */
export const WORKLOADS: readonly Workload[] = ["chat", "reasoning", "agentic", "vision"];

/** The abstract tier a workload routes through. */
export const WORKLOAD_TIER: Record<Workload, string> = {
  chat: "chat-v1",
  reasoning: "reasoning-v1",
  agentic: "agentic-v1",
  vision: "vision-v1",
};

/** What a row says about itself. */
export interface WorkloadCopy {
  label: string;
  description: string;
  /** What to pick, for someone who does not already know. */
  hint: string;
}

/**
 * The row copy, ported verbatim.
 *
 * The recommendation hints are the part that makes this screen usable by someone
 * who has never chosen a model before, and they are the first thing a rewrite
 * would drop as verbose. They are not verbose; they are the feature.
 *
 * One substitution from the source: the managed brain is named for this product
 * rather than the one the copy was written for. Keeping another product's name
 * in our own UI would be a bug, not fidelity.
 */
export const WORKLOAD_COPY: Record<Workload, WorkloadCopy> = {
  chat: {
    label: "Chat",
    description: "Direct conversational back-and-forth",
    hint: "a cheap or mid-cost fast chat model with high tokens/sec and low latency. Open-source local models can work well here if they feel responsive.",
  },
  reasoning: {
    label: "Reasoning",
    description: "Main chat agent, meeting summarizer",
    hint: "a more expensive frontier or strong reasoning model for deep thinking. Used for the main chat agent, meeting summaries, and heavier answer synthesis.",
  },
  agentic: {
    label: "Agentic",
    description: "Sub-agent runners, tool loops, and coding passes",
    hint: "a reliable instruction-following model with strong tool use. Mid-cost frontier models are usually safest; capable open-source models can work if tool calling is stable.",
  },
  vision: {
    label: "Vision",
    description: "Image understanding for the vision sub-agent: always multimodal",
    hint: "a multimodal model that accepts image input. The managed default is image-capable; any provider routed here is always treated as vision-enabled.",
  },
};

/** The three modes, and what each one claims. */
export const MODE_COPY: Record<RoutingMode, { label: string; description: string }> = {
  managed: {
    label: "Managed",
    description:
      "TinyHumans will run all inference in the cloud, choose the best model for the task, optimize for cost, and keep the safest routing defaults.",
  },
  own: {
    label: "Use Your Own Models",
    description:
      "Choose one provider + model and route every workload through it. This is simple, but it can be inefficient because lightweight and heavyweight inference all share the same route.",
  },
  advanced: {
    label: "Advanced",
    description:
      "Pick different models for different tasks. This is the best option for tight cost optimization and the most control.",
  },
};

/**
 * Managed is a badge, not a disabled toggle.
 *
 * A locked switch reads as switchable-but-broken and invites a fight the
 * operator cannot win. A badge says the same thing and is honest about it.
 *
 * **What it says depends on whether managed actually resolves.** The design this
 * ports says `Always on`, which is true there — they run the managed backend.
 * Here it needs a credential, so a badge claiming permanent availability on a
 * company whose chain resolves to nothing would be the same lie the provider row
 * was carrying.
 */
export function managedModeBadge(configured: boolean | undefined): string {
  return configured ? "Available" : "Not set up";
}

/**
 * What managed's absence means — the statement, with no navigation in it.
 *
 * Used on the page that **holds the action**, where the button is already on
 * screen. Telling an operator to go to the tab they are looking at is a sentence
 * that has stopped reading its own surroundings.
 */
export const MANAGED_NOT_SET_UP =
  "Managed is not set up on this company, so it is not a fallback.";

/** The same statement plus where to fix it, for a page that does not hold the action. */
export const MANAGED_NOT_SET_UP_ELSEWHERE = `${MANAGED_NOT_SET_UP} Connect it on the LLM Providers tab.`;

/**
 * What to say under the Connected list about managed being a fallback, or `null`
 * when the row above has already said everything true.
 *
 * Three states, and only two of them need a sentence:
 *
 * - **Not set up** — nothing in the chain answers. Say so.
 * - **Set up but switched off** — it resolves and is still not a fallback,
 *   which is the one case the row's own "On" badge cannot express.
 * - **Set up and on** — the row names the step that answers and who it bills.
 *   Repeating it here would be duplication, and the sentence it used to repeat
 *   ("always available") was not true besides.
 */
export function managedFallbackNote(
  managed: { configured?: boolean; enabled?: boolean } | undefined,
): string | null {
  if (!managed) return null;
  if (managed.configured === false) return MANAGED_NOT_SET_UP;
  if (managed.enabled === false) return MANAGED_SWITCHED_OFF;
  return null;
}

/** Set up, but switched out of routing — which is also not a fallback. */
export const MANAGED_SWITCHED_OFF =
  "Managed is switched off, so it is not a fallback. Its credential is untouched.";

/** What the shared-model row covers, said out loud rather than implied. */
export const OWN_MODE_SCOPE =
  "Applies the same provider + model to chat, reasoning, agentic and vision. Changes save when you click save.";

/** What the shared-model row says when there is nothing to choose from. */
export const OWN_MODE_EMPTY =
  "Add or connect a provider first. Then you can route every workload through one model here.";

/** The line above the rows in Advanced. */
export const ADVANCED_INTRO =
  "Fine-grained routing gives you the best cost optimization and the most control. Use the rows below to decide which workloads stay Managed, which use your shared default, and which pin to a specific model.";

/** The unset ref. Its own constant so the absence is a value, not a `null`. */
export const UNSET: ProviderRef = { kind: "default" };

/**
 * Parses the hand-editable string grammar (`acme:gpt-5`).
 *
 * An operator reads and edits routes as text, so the grammar has to survive a
 * round trip through a person. An empty string is `default` — an absence, not a
 * parse failure, because "nothing set here" is what deleting the value means.
 */
export function parseRef(raw: string): ProviderRef {
  const trimmed = raw.trim();
  if (!trimmed || trimmed === "default") return { kind: "default" };
  if (trimmed === "managed") return { kind: "managed" };
  const colon = trimmed.indexOf(":");
  const slug = (colon === -1 ? trimmed : trimmed.slice(0, colon)).trim();
  const rawModel = colon === -1 ? "" : trimmed.slice(colon + 1).trim();
  const model = rawModel || undefined;
  if (slug === "claude-code") return { kind: "claudeCode", model };
  if (slug === "local") return { kind: "local", model };
  return { kind: "cloud", providerSlug: slug, model };
}

/** The string form of a ref, for the same grammar. */
export function formatRef(ref: ProviderRef): string {
  switch (ref.kind) {
    case "default":
      return "";
    case "managed":
      return "managed";
    case "cloud":
      return ref.model ? `${ref.providerSlug}:${ref.model}` : ref.providerSlug;
    case "local":
      return ref.model ? `local:${ref.model}` : "local";
    case "claudeCode":
      return ref.model ? `claude-code:${ref.model}` : "claude-code";
  }
}

/**
 * A comparable identity for a ref.
 *
 * Used to answer "do all the rows point at the same thing", which is what makes
 * the mode inferable. Structural equality on the objects would say no for two
 * refs that differ only by an absent versus undefined `model`.
 */
export function refSignature(ref: ProviderRef): string {
  return formatRef(ref) || "default";
}

/** The ref for a workload, or the unset one. */
export function refFor(routing: RoutingMap, workload: Workload): ProviderRef {
  return routing[workload] ?? UNSET;
}

/**
 * Which mode the current routes describe.
 *
 * **Inferred, never stored.** A mode field would be a fifth thing that can
 * disagree with the four routes, and the routes are the truth. Four fields, one
 * derived mode.
 */
export function inferRoutingMode(routing: RoutingMap): RoutingMode {
  const refs = WORKLOADS.map((w) => refFor(routing, w));
  if (refs.every((r) => r.kind === "managed" || r.kind === "default")) return "managed";
  const first = refSignature(refs[0]);
  if (refs.every((r) => refSignature(r) === first)) return "own";
  return "advanced";
}

/**
 * The routes a removal orphans, and the map with them reset.
 *
 * Three matching rules, because only one of the three kinds of ref carries a
 * slug, and all three cases are real bugs:
 *
 * - **Cloud and custom** — matched precisely by `providerSlug`.
 * - **CLI logins** — their refs carry no slug. Without this, disconnecting one
 *   left workloads pinned to `claude-code:<model>`, which the resolver still
 *   honours, so chats kept using the CLI after the provider was removed.
 * - **Local runtimes** — no slug either, and a `local` ref is only definitively
 *   orphaned once **no** local runtime remains. Scrubbing on the first removal
 *   would unpin a route a second runtime still serves.
 *
 * Returns the affected workloads as well as the new map, so the UI can say which
 * rows moved rather than leaving the operator to notice.
 */
export function scrubOnRemove(
  routing: RoutingMap,
  removed: Pick<Provider, "slug" | "kind">,
  remaining: readonly Pick<Provider, "slug" | "kind">[],
  categoryOf: (kind: string) => "cloud" | "local" | "cli",
): { routing: RoutingMap; reset: Workload[] } {
  const category = categoryOf(removed.kind);
  const categorySurvives = remaining.some((p) => categoryOf(p.kind) === category);

  const next: RoutingMap = { ...routing };
  const reset: Workload[] = [];
  for (const workload of WORKLOADS) {
    const ref = next[workload];
    if (!ref) continue;
    let orphaned = false;
    if (ref.kind === "cloud") {
      // A slug match is decisive, whatever the category — `ollama:llama3`
      // parses as a cloud ref because it carries a slug, while `categoryOf`
      // says local, so gating on the category meant the two rules never met and
      // removing a local runtime scrubbed nothing. See `scrub_removed`.
      orphaned = ref.providerSlug === removed.slug;
    } else if (ref.kind === "local") {
      orphaned = category === "local" && !categorySurvives;
    } else if (ref.kind === "claudeCode") {
      orphaned = category === "cli" && !categorySurvives;
    }
    if (orphaned) {
      next[workload] = { kind: "default" };
      reset.push(workload);
    }
  }
  return { routing: next, reset };
}

/**
 * Routes naming a provider this company does not hold.
 *
 * The second, independent check behind the same invariant as `scrubOnRemove`.
 * Two mechanisms for one rule, because the UI path can be bypassed by a config
 * edit or an older build — and an unresolvable route has to be reported rather
 * than discovered mid-turn.
 */
export function orphanedRoutes(
  routing: RoutingMap,
  providers: readonly Pick<Provider, "slug">[],
): { workload: Workload; slug: string }[] {
  const out: { workload: Workload; slug: string }[] = [];
  for (const workload of WORKLOADS) {
    const ref = routing[workload];
    if (ref?.kind !== "cloud") continue;
    if (!providers.some((p) => p.slug === ref.providerSlug)) {
      out.push({ workload, slug: ref.providerSlug });
    }
  }
  return out;
}

/**
 * The provider an **unset** workload goes through.
 *
 * The marked default, and only then list order. `isDefault` is the host's
 * resolved answer — a company that has never said reports its first enabled
 * provider — so this reads the same fact the turn path resolves, rather than
 * a second opinion about it.
 *
 * `undefined` means nothing enabled resolves, which every surface reads as the
 * managed brain: always available, and the right fallback.
 */
export function primaryProvider(providers: readonly Provider[]): Provider | undefined {
  return providers.find((p) => p.isDefault && p.enabled) ?? providers.find((p) => p.enabled);
}

/**
 * What an unset row says it will actually use.
 *
 * `Primary (OpenRouter)` rather than a bare "Default", because "unset" is
 * otherwise a mystery on the one screen whose job is to say where work goes.
 * Read through {@link primaryProvider} on every render and never cached, so the
 * rows move when the marked default does.
 */
export function primaryLabel(providers: readonly Provider[]): string {
  const primary = primaryProvider(providers);
  return primary ? `Primary (${primary.label})` : "Primary (Managed)";
}

/** The providers a row may be pointed at: enabled ones, in list order. */
export function routingTargets(providers: readonly Provider[]): Provider[] {
  return providers.filter((p) => p.enabled);
}

/**
 * The model id to carry when the provider changes.
 *
 * **A model id belongs to the provider it was chosen from.** Keeping
 * `claude-haiku-4-5-20251001` when the select moves from Anthropic to OpenRouter
 * leaves a value that is meaningless at the new endpoint — and worse, one the
 * field then quietly presents as free text, because it is not in the new
 * catalogue. That is the "wrong model at the wrong provider" failure the routing
 * fix exists to prevent, arriving through the form instead of the resolver.
 *
 * Blank is not a broken state here: it means *send the tier and let the endpoint
 * resolve it*, which is a working route at every provider. So clearing costs the
 * operator one choice and never costs them a turn.
 *
 * This is **not** the rule the nine proxy regressions are about. Those are about
 * discarding a value **mid-keystroke**, while it is still being typed. Changing
 * the provider is a settled act with an explicit target.
 */
export function modelAfterProviderChange(model: string, from: string, to: string): string {
  return from === to ? model : "";
}

/** What removing a provider — or just its key — would cost. */
export interface RemovalImpact {
  /** Workloads whose route would be reset to the primary. */
  routed: Workload[];
  /** Whether unrouted work goes through it today. */
  isDefault: boolean;
  /** Whether it is the last provider this company has switched on. */
  lastEnabled: boolean;
  /**
   * The provider the default marker would move to, when removing this one moves
   * it. `null` when it is not the default, or when nothing is left to hold it.
   */
  defaultMovesTo: string | null;
}

/**
 * What a removal would cost, as facts rather than prose.
 *
 * Read through {@link scrubOnRemove} rather than by matching slugs here, so the
 * confirmation names exactly the workloads the write will actually reset — all
 * three of its matching rules included, which is what makes it right for a local
 * runtime and a CLI login as well as a cloud row.
 */
export function removalImpact(
  provider: Pick<Provider, "slug" | "kind" | "enabled"> & { isDefault?: boolean },
  providers: readonly Provider[],
  routing: RoutingMap,
  categoryOf: (kind: string) => "cloud" | "local" | "cli",
): RemovalImpact {
  const remaining = providers.filter((p) => p.slug !== provider.slug);
  const { reset } = scrubOnRemove(routing, provider, remaining, categoryOf);
  const isDefault = Boolean(provider.isDefault);
  return {
    routed: reset,
    isDefault,
    lastEnabled: provider.enabled && !remaining.some((p) => p.enabled),
    // Read through the same `primaryProvider` the rows render, so the sentence
    // names the provider that will actually hold it rather than a guess at list
    // order.
    defaultMovesTo: isDefault ? (primaryProvider(remaining)?.label ?? null) : null,
  };
}

/**
 * The sentences a removal confirmation says, in the order they matter.
 *
 * A confirmation that only asks "are you sure?" is a speed bump, not a safeguard:
 * it tells the operator nothing they did not already know and trains them to
 * click through. These name what is actually about to change, and there is one
 * per fact rather than a paragraph, so an operator can see at a glance whether
 * any of them is the one they care about.
 *
 * **Removing a key is not removing a provider**, and the first sentence says so
 * either way. The two are one menu apart and one is recoverable by retyping a
 * credential while the other deletes a record, its endpoint and its routes.
 */
export function removalWarnings(
  intent: "key" | "provider",
  label: string,
  impact: RemovalImpact,
): string[] {
  const lines: string[] =
    intent === "key"
      ? [
          `${label} stays on this page, keeping its endpoint and every route that names it. It just has no credential, so it cannot answer until you add one.`,
        ]
      : [
          `This deletes ${label} and clears its stored key in the same operation. Adding it again means entering the credential again.`,
        ];
  if (impact.isDefault) {
    if (intent === "key") {
      lines.push(
        `It is this company's default, so every unrouted workload goes through it — and would start failing.`,
      );
    } else {
      // **Named, not implied.** An operator removed their default provider, the
      // marker relocated in silence, and re-adding the credential did not bring
      // it back — so a setting they had chosen was reassigned by a delete and
      // stayed reassigned. A removal that moves the default has to say where to
      // and that it is one-way.
      lines.push(
        impact.defaultMovesTo
          ? `It is this company's default. Removing it moves the default to ${impact.defaultMovesTo}, and adding this provider again will not move it back.`
          : `It is this company's default, and nothing is left to take that over — unrouted work falls back to Managed.`,
      );
    }
  }
  if (impact.routed.length > 0) {
    const named = impact.routed.map((w) => WORKLOAD_COPY[w].label).join(", ");
    lines.push(
      intent === "key"
        ? `${impact.routed.length === 1 ? "One workload routes" : `${impact.routed.length} workloads route`} to it (${named}), and the route stays pointed here.`
        : `${impact.routed.length === 1 ? "One workload routes" : `${impact.routed.length} workloads route`} to it (${named}). Those rows reset to the primary.`,
    );
  }
  if (impact.lastEnabled && intent === "provider") {
    lines.push(
      `It is the only provider switched on, so removing it leaves Managed as the only thing that can answer.`,
    );
  }
  return lines;
}

/** The managed brain's name wherever a row or a list has to say it. */
export const MANAGED_TARGET_LABEL = "Managed";

/**
 * The sentinel a per-workload select uses for the row's **unset** state.
 *
 * Unset is a state, not an option — see {@link routingOptions}.
 */
export const UNSET_TARGET = "__unset__";

/** One choosable provider in the per-workload select. */
export interface RoutingOption {
  /** The slug a chosen row writes. */
  slug: string;
  /** The name shown. */
  label: string;
}

/**
 * The providers one workload may be pointed at — **providers only**.
 *
 * The dialog used to list the primary twice: once as `Primary (OpenRouter)`,
 * which is the unset row's own display, and once as `OpenRouter`. Three
 * connected providers produced five entries, two of which named the same
 * account and behaved differently. "Follow the default" is not a fourth
 * provider; it is the absence of a choice, and the Routing tab already renders
 * it as `Primary (OpenRouter)`. Getting back to it is an **action**, not a list
 * entry.
 *
 * Managed is first and is listed by its own slug rather than a second sentinel,
 * so it is one identity everywhere — and so a company whose entry zero *is* the
 * managed config does not get a row for it twice.
 */
export function routingOptions(providers: readonly Provider[]): RoutingOption[] {
  const rest = routingTargets(providers)
    .filter((p) => p.slug !== MANAGED_OPTION_SLUG)
    .map((p) => ({ slug: p.slug, label: p.label }));
  return [{ slug: MANAGED_OPTION_SLUG, label: MANAGED_TARGET_LABEL }, ...rest];
}

/**
 * The provider whose model id a chosen target would set, or `null` when it
 * takes none.
 *
 * **The one place that decides whether the Model id field appears**, and it
 * decides on the *kind of provider* rather than on which synonym was picked.
 * `Primary (OpenRouter)` and `OpenRouter` are two names for one provider, and a
 * field that appeared under one and not the other was reporting a difference
 * that does not exist.
 *
 * `null` for managed because the route grammar has no managed-plus-model form:
 * a `managed` ref carries no model, so a field there could only be discarded on
 * save. Both of its synonyms resolve through this function, so they agree by
 * construction rather than by two branches being kept in step.
 */
export function modelTarget(target: string, providers: readonly Provider[]): string | null {
  const slug = target === UNSET_TARGET ? primaryProvider(providers)?.slug : target;
  if (!slug || slug === MANAGED_OPTION_SLUG) return null;
  return slug;
}

/**
 * The ref a chosen target and model write.
 *
 * Unset stays unset while the model is blank. **Choosing a model pins the row**
 * to the provider the default currently resolves to, because "follow the
 * default, but with this model" is not expressible in the route grammar — and
 * silently dropping the model would be the worse of the two answers.
 *
 * The override is judged here, at the one point that crosses the boundary,
 * rather than by clearing the input: the operator can see what they typed and
 * why it will not be used.
 */
export function refForTarget(
  target: string,
  model: string,
  providers: readonly Provider[],
): ProviderRef {
  if (target === MANAGED_OPTION_SLUG) return { kind: "managed" };
  const slug = modelTarget(target, providers);
  const pinned = slug && overrideIsSendable(slug, model) ? model.trim() : "";
  if (target === UNSET_TARGET) {
    return pinned && slug ? parseRef(`${slug}:${pinned}`) : { kind: "default" };
  }
  return parseRef(pinned ? `${target}:${pinned}` : target);
}

/**
 * What the per-workload trigger reads for a chosen target.
 *
 * A function rather than an inline ternary because a select that shows
 * `__unset__` to an operator is the failure this exists to prevent — the same
 * trap `TaskEditDialog` documents for a column id versus its label.
 */
export function targetLabel(target: string, providers: readonly Provider[]): string {
  if (target === UNSET_TARGET) return primaryLabel(providers);
  if (target === MANAGED_OPTION_SLUG) return MANAGED_TARGET_LABEL;
  return providers.find((p) => p.slug === target)?.label ?? target;
}

/** The select value a stored ref restores to. */
export function targetForRef(ref: ProviderRef): string {
  if (ref.kind === "managed") return MANAGED_OPTION_SLUG;
  if (ref.kind === "cloud") return ref.providerSlug;
  return UNSET_TARGET;
}

/**
 * What a row's value column reads, and what its button says.
 *
 * The button is **Change Model** when something is set and **Choose Model**
 * when nothing is, because those are two different invitations.
 */
export function rowValue(
  ref: ProviderRef,
  providers: readonly Provider[],
): { value: string; action: "Change Model" | "Choose Model" } {
  switch (ref.kind) {
    case "default":
      // Not "No model selected". An unset row is not a gap — it resolves
      // somewhere, and naming where is the difference between a screen that
      // reports routing and one that hides half of it.
      return { value: primaryLabel(providers), action: "Choose Model" };
    case "managed":
      return { value: MANAGED_TARGET_LABEL, action: "Change Model" };
    case "cloud": {
      const label = providers.find((p) => p.slug === ref.providerSlug)?.label ?? ref.providerSlug;
      return { value: ref.model ? `${label} · ${ref.model}` : label, action: "Change Model" };
    }
    case "local":
      return { value: ref.model ? `Local · ${ref.model}` : "Local", action: "Change Model" };
    case "claudeCode":
      return {
        value: ref.model ? `Claude Code · ${ref.model}` : "Claude Code",
        action: "Change Model",
      };
  }
}

/**
 * Applies one provider and model to every row — what Use Your Own Models does.
 *
 * Written as a whole-map replacement rather than a loop at the call site so the
 * "every row, not some rows" part is the function's contract instead of a
 * component's discipline.
 */
/**
 * The provider and model an **own**-mode table is describing — the inverse of
 * {@link applyToEveryWorkload}.
 *
 * Without it the shared-model form saves correctly and then shows nothing back:
 * an operator sets a provider and a model, saves, returns to the tab, and finds
 * an empty select with Save disabled while the store holds exactly what they
 * chose. Nothing is lost, but "my save did not stick" is what it reads as, and
 * that is indistinguishable from the routing table being inert — which is the
 * bug it sat next to.
 *
 * Blank when the rows do not agree, which is the same condition
 * {@link inferRoutingMode} calls `advanced`: there is no single provider to show
 * and inventing one from the first row would misreport the other three.
 */
export function ownModeDraft(routing: RoutingMap): { slug: string; model: string } {
  const refs = WORKLOADS.map((w) => refFor(routing, w));
  const first = refs[0];
  if (!first || first.kind !== "cloud") return { slug: "", model: "" };
  const signature = refSignature(first);
  if (!refs.every((r) => refSignature(r) === signature)) return { slug: "", model: "" };
  // `model` is optional and blank is meaningful — it means "send the tier and
  // let the endpoint resolve it" — so an absent one becomes the empty string the
  // field renders, never a placeholder.
  return { slug: first.providerSlug, model: first.model ?? "" };
}

export function applyToEveryWorkload(providerSlug: string, model?: string): RoutingMap {
  const ref: ProviderRef = { kind: "cloud", providerSlug, model };
  return Object.fromEntries(WORKLOADS.map((w) => [w, ref])) as RoutingMap;
}
