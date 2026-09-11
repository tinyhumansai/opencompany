// The live inference API (issue #56 — BYOK): the console reads and writes the
// company's effective inference provider through the host's `.../inference`
// routes (REST, camelCase over the wire). The effective config is the
// highest-precedence of a runtime console override, the committed manifest
// `[inference]`, and the platform managed default.
//
// The outbound credential is WRITE-ONLY: a `key` is sent on set and stored in
// the host's secret store; it is never returned. The read shape carries only a
// `keyConfigured` boolean. Standalone functions over the shared client (mirrors
// `api/skills.ts` / `api/mcp.ts`), so no change to `OpenCompanyClient` or the
// shared `api/types.ts` is needed.

import type { OpenCompanyClient } from "./client";

/** Provider kinds the console offers. */
export type InferenceProvider = "managed" | "openrouter" | "openai_compatible" | "ollama";

/** Where the effective config came from — drives the source badge. */
export type InferenceSource = "managed" | "default" | "manifest" | "runtime";

/**
 * The cognition path the company actually booted onto. Config resolving to a
 * provider does not guarantee `harness`: a build without the harness, or a
 * config that fails to resolve at boot, falls back to `hosted`/`echo`.
 */
export type CognitionPath = "harness" | "hosted" | "sidecar" | "echo" | "custom" | "test";

/**
 * Where that path's inference usage is metered (issue #174):
 * - `perTurn` — the harness meters each agent turn from real provider totals.
 * - `perCycle` — the runtime meters what the cycle reports (hosted Medulla reads
 *   it off the `orch:usage` wire frame), so zero means the upstream reported
 *   nothing.
 * - `none` — no model runs on this path, so a zero Usage reading is the truth.
 */
export type UsageMetering = "perTurn" | "perCycle" | "none";

/** The company's effective inference status. Never carries the credential. */
export interface InferenceStatus {
  /**
   * Provider kind **as the operator selected it**, not as the host resolves it.
   *
   * `managed` is a legacy alias the host folds onto `openrouter` at resolution,
   * and this field used to carry that resolved answer — which made the managed
   * route unselectable from this card: `seedFromStatus` takes this value
   * verbatim, so saving `managed` and reading back `openrouter` snapped the
   * select (and the managed-only Connect button) straight back to OpenRouter.
   * The host now reports the selection; `proxied` carries the resolution fact
   * this field used to stand in for.
   */
  provider: string;
  /**
   * Whether the *saved* config rides the platform's subscription proxy rather
   * than a key this company supplied.
   *
   * Reported by the host rather than re-derived from `provider` +
   * `keyConfigured`: that derivation only held while `provider` was the
   * resolved kind, and would now read a managed company with its own OpenRouter
   * key as riding a subscription it does not.
   *
   * Optional because a host predating this field answers without it, and this
   * console talks to hosts it did not ship with. Absent means "ask the old
   * way" — see `savedIsProxied` in `InferenceSection` — not "not proxied".
   */
  proxied?: boolean;
  /** Telemetry slug: `managed` | `openrouter` | `byok` | `ollama`. */
  slug: string;
  /** Resolved OpenAI-compatible base URL. */
  baseUrl: string;
  /** Abstract-tier → concrete model id. */
  models: Record<string, string>;
  /**
   * The shipped tier → model defaults — **OpenRouter's vocabulary**, and
   * nothing wider. The console's OpenRouter preset used to hard-code its own
   * copy of these ids so the form had something to prefill before an operator
   * typed an override; that duplicate could silently drift from what the host
   * actually defaults to. This is read off the host on every status load, so
   * the preset is never more than one request stale.
   *
   * It is only that preset. These are OpenRouter catalog ids and mean nothing
   * at another endpoint; what the *configured* endpoint wants is
   * `InferenceModelCatalog.tierDefaults`, derived from that endpoint's own
   * published catalog.
   */
  defaultTierModels: Record<string, string>;
  /** Provenance badge. */
  source: InferenceSource;
  /** Whether an outbound key is stored — never the key itself. */
  keyConfigured: boolean;
  /** The cognition path this company is running on. */
  cognition: CognitionPath;
  /** Where this path's inference usage is metered. */
  usageMetering: UsageMetering;
  /**
   * Whether a stored config resolves but the *running* brain predates it, so
   * only a restart puts it to work (issue #266).
   *
   * Which brain a company runs is decided once, when the company is built. A
   * company that started with no inference source is on the offline echo brain
   * with an unwired workflow runner, and saving a credential afterwards changes
   * neither — so "agents use it on their next turn" is false for exactly that
   * transition, which is also the first one a new operator makes.
   *
   * `false` covers both "already live" and "a restart would not help either"
   * (this host has no harness path at all) — tell those apart with `cognition`.
   */
  restartRequired: boolean;
  /**
   * Whether the harness cognition path is reachable on this host at all (the
   * `openhuman` feature compiled in and a pool attached). `false` means no
   * model configuration can ever move this company onto the design path, so
   * the setup dialog's "set up a model" call-to-action would be a dead end —
   * it omits the CTA rather than send the operator round a redesign loop that
   * cannot end.
   */
  harnessReachable: boolean;
  /**
   * Whether this company can run a profile design pass — the one behind
   * `POST {scope}/team/design` and the two `/team/…/draft` routes.
   *
   * Optional because an older host does not send it. `undefined` means "this
   * host did not say", which is read exactly as `cognition: null` is: the
   * capability is unknown, so the reduced dialog is offered and the refusal
   * (if any) is met honestly. Only an explicit `false` retires it up front.
   *
   * Here at all because the console had no way to ask, and was inferring the
   * answer from `cognition`: the reduced Add-teammate dialog treated every
   * path but `echo` as able to draft. That is wrong for three of the six —
   * `profile_drafter()` on the host is built from `workflow_harness_deps`,
   * which is assigned in exactly one place, inside the embedded harness arm of
   * `RuntimeBuilder::build`. So `hosted`, `sidecar` and `custom` companies
   * have no drafter either, and every create through the reduced dialog on
   * them went: type a sentence, press Create, wait on a model call that could
   * only answer `no_model`, then meet the full form and write it by hand.
   *
   * Not the same question as `harnessReachable`, which is the pool being
   * *attached* rather than this company having *booted onto* it: a company
   * whose config failed to resolve at boot reports `harnessReachable: true`
   * and has no drafter.
   */
  designsProfiles?: boolean;
  /**
   * Whether this host can rebuild the company's runtime in place, so the
   * console may offer to perform the restart `restartRequired` names (issue
   * #1736).
   *
   * The two are independent, and the card only had the first: it rendered a
   * "Restart now" button on hosts where `POST …/inference/restart` can only
   * answer "this host cannot rebuild a company runtime in place". An operator
   * was told a restart was required, handed the control for it, and the control
   * could never work. `false` means name the remedy instead of offering the
   * action — the same rule the setup capability flags exist for (`api/setup.ts`):
   * say "not in this build" rather than offer a switch that does nothing.
   */
  canRebuildInPlace: boolean;
}

/** The set-provider body. `key` is write-only (never returned). */
export interface SetInferenceInput {
  provider: InferenceProvider;
  baseUrl?: string;
  models?: Record<string, string>;
  /** The outbound credential. Omit to leave unchanged; "" to clear. */
  key?: string;
}

/** A mutating response: the resulting status plus a plain-language note. */
export interface InferenceMutation {
  status: InferenceStatus;
  note: string;
}

/** One model published by the configured provider's catalog. */
export interface InferenceModel {
  id: string;
  name?: string;
  contextLength?: number;
}

/**
 * How the configured endpoint spells a tier.
 *
 * - `tiers` — it publishes the tier names themselves (`chat-v1`, `agentic-v1`)
 *   and resolves them against its own registry, so a tier is what it wants.
 * - `concrete` — it publishes the ids `defaultTierModels` names, so a bare tier
 *   would be rejected and the shipped mapping is the right default.
 * - `unknown` — its catalog publishes neither, so there is no default anyone can
 *   supply and the operator has to name a model per tier.
 *
 * Absent (`undefined`) means the catalog could not be read at all, which is a
 * different thing from `unknown` and must not be shown as one.
 */
export type TierVocabulary = "tiers" | "concrete" | "unknown";

/**
 * The catalog of the endpoint **this company is configured against**.
 *
 * This route used to answer with a bare array that was always OpenRouter's
 * public registry, whatever endpoint the company had been pointed at: the
 * console listed 421 models to a company whose provider published eleven, the
 * operator picked one the console had offered, and the provider rejected it.
 * Discovery follows the configured base URL now, and the response says which
 * endpoint answered so the console can name it rather than imply a vendor.
 */
export interface InferenceModelCatalog {
  /** The endpoint the catalog was read from. */
  baseUrl: string;
  /** Every model that endpoint publishes, sorted. Empty when `error` is set. */
  models: InferenceModel[];
  /**
   * How this endpoint spells a tier.
   *
   * `null` when the catalog could not be read — which is a different fact from
   * `"unknown"` ("the endpoint answered, and publishes neither vocabulary") and
   * must not be shown as one. The host sends the field either way:
   * `ModelCatalogDto::tier_vocabulary` is an `Option` with no
   * `skip_serializing_if`, so the wire carries an explicit `null` rather than
   * omitting the key (CodeRabbit review on #2045). Optional as well as nullable
   * because a stub or an older host may omit it; both mean "no vocabulary".
   */
  tierVocabulary?: TierVocabulary | null;
  /**
   * The tier → model mapping this endpoint's vocabulary implies. Empty for
   * `unknown` and for an unreadable catalog: prefilling ids the endpoint has
   * already told us it does not publish is the defect this field replaces.
   */
  tierDefaults: Record<string, string>;
  /**
   * Why the catalog is empty, naming the endpoint — a 200 rather than a 5xx,
   * because an empty picker with no explanation reads as "this provider has no
   * models", which nobody established.
   */
  error?: string;
}

/** The live-probe result. */
export interface InferenceTestResult {
  ok: boolean;
  provider?: string;
  note?: string;
  error?: string;
  code?: string;
}

/** The company's effective inference status. */
export function getInferenceStatus(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InferenceStatus> {
  return client.get<InferenceStatus>(`${client.scopeFor(company)}/inference`);
}

/** The model catalog of the endpoint this company is configured against. */
export function listInferenceModels(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InferenceModelCatalog> {
  return client.get<InferenceModelCatalog>(`${client.scopeFor(company)}/inference/models`);
}

/** Set (or replace) the runtime provider override, optionally rotating the key. */
export function setInference(
  client: OpenCompanyClient,
  company: string | null,
  body: SetInferenceInput,
): Promise<InferenceMutation> {
  return client.put<InferenceMutation>(`${client.scopeFor(company)}/inference`, body);
}

/** Clear the runtime override, reverting to the manifest (or managed) config. */
export function revertInference(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InferenceMutation> {
  return client.del<InferenceMutation>(`${client.scopeFor(company)}/inference`);
}

/**
 * Rebuild this company's runtime in place, now.
 *
 * The action behind the "Restart required" notice. Which brain a company runs
 * is chosen when its runtime is built, so a company that booted with no
 * inference source keeps echoing however the config changes underneath it.
 * Saving already attempts this rebuild; this asks for it on its own, which is
 * what a company already sitting in that state needs — or one whose rebuild
 * failed the first time.
 *
 * In-flight work is preserved rather than dropped. The turn in progress
 * completes, and the journal, parked approvals and single-use grants are handed
 * to the successor — so an approval waiting on a person survives and nobody has
 * to approve a tool call twice. Cycles arriving during the swap take a `503`
 * and are retried against the successor.
 *
 * Rejects on a host that wired no rebuilder, with a message naming the process
 * restart that would work instead. That is the honest answer, and the reason
 * the console has to surface the failure rather than assume this always works.
 */
export function restartInference(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InferenceMutation> {
  return client.post<InferenceMutation>(`${client.scopeFor(company)}/inference/restart`, {});
}

/** Live-probe the resolved provider (one `ping` turn). */
export function testInference(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InferenceTestResult> {
  return client.post<InferenceTestResult>(`${client.scopeFor(company)}/inference/test`, {});
}
