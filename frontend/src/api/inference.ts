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
import type { ProbeClass, Provider, ProviderHealth, RoutingMode } from "@/inference/types";

/**
 * Provider kinds the console offers.
 *
 * `"managed"` is deliberately here and is **not** a member of the host's
 * `INFERENCE_PROVIDERS`, which is the manifest validator's allowlist
 * (`openrouter`, `openai_compatible`, `ollama`). It is two other things at once:
 * the sentinel the host answers with for a company that has configured nothing
 * (`effective_status_with`'s `None` arm), and a legacy alias the host normalizes
 * onto `openrouter` on the way in (`LEGACY_MANAGED`). The console still offers
 * the card behind `INFERENCE_MANAGED_HIDDEN`.
 *
 * So this union being wider than the host's allowlist is correct, and it is not
 * the six-copies duplication that `@/inference/catalogue` exists to collapse.
 * Deleting `"managed"` for tidiness breaks the unconfigured card, which is the
 * first thing a new operator sees. Two host tests pin the value on the wire:
 * `unconfigured_company_reports_the_platform_url_not_the_built_in_default` and
 * `status_defaults_to_managed_then_switches_to_runtime`.
 */
export type InferenceProvider = "managed" | "openrouter" | "openai_compatible" | "ollama";

/**
 * Where the effective config came from — drives the source badge.
 *
 * `"default" | "manifest" | "runtime"` are the host's `InferenceSource`.
 * `"managed"` is the fourth value the same field carries when nothing resolved
 * at all, and it means something the other three cannot: *a platform endpoint is
 * not tenant config*. Reporting that case as `"default"` would move the badge
 * onto a config the tenant never wrote. Same reasoning, and the same tests, as
 * {@link InferenceProvider} above.
 */
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
  /** Provider kind. */
  provider: string;
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
  /**
   * Every provider this company holds, entry zero first.
   *
   * **Additive, and it must stay that way.** This interface is the "can this
   * company think?" oracle for four surfaces that are not about inference at
   * all — `SetupDialog`, `AgentDetailView`, `CopilotPanel` and
   * `WorkflowCreateDialog` — so every field above keeps its exact meaning. Add
   * fields here; do not reshape the ones that are already read elsewhere.
   *
   * Optional because an older host does not send it. `undefined` means "this
   * host did not say", which is not the same as "this company has no
   * providers" — read it as unknown and fall back to the single-provider
   * fields, exactly as `designsProfiles` is read.
   *
   * A company with one provider reports a list of one. That is the truth, and
   * already more than the single form ever said.
   */
  providers?: Provider[];
  /**
   * What the **managed** brain would resolve to, and who pays for it.
   *
   * Optional because an older host does not send it. `undefined` is read as
   * "this host did not say" — the row then falls back to the boolean facts
   * above rather than claiming a state nobody established.
   */
  managed?: ManagedState;
}

/**
 * The managed tier's honest state.
 *
 * The row for it used to carry a permanent "Always on" badge, inherited from a
 * design where the same company runs the managed backend. Here it needs a
 * credential and can resolve to nothing, and a row claiming availability while
 * agents cannot think is exactly the dishonesty the five-state cognition model
 * exists to prevent.
 */
export interface ManagedState {
  /**
   * Which step of the chain answers.
   *
   * `instance` and `companyAccount` are separate on purpose: one bills the
   * company's own TinyHumans account and the other bills whoever runs the
   * server, and that is the decision an operator is here to make.
   */
  source: "provider_key" | "company_account" | "instance" | "none";
  /** Whether it can be reached at all. */
  configured: boolean;
  /** The endpoint managed requests travel to. */
  baseUrl: string;
  /**
   * Whether it is a routing target.
   *
   * A provider like any other in this one respect. "Stop routing work here" and
   * "remove the credential" are different statements, and switching managed off
   * leaves every step of its chain where it was.
   *
   * Optional because an older host does not send it; absent reads as on, which
   * is what managed always was.
   */
  enabled?: boolean;
  /** What was last learnt about reaching it, if anything. */
  health?: ProviderHealth;
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

// ---- the provider list, and writing it ---------------------------------------

/**
 * What a connect attempt learnt.
 *
 * **Never carries the raw upstream error.** That text can echo request material
 * — headers, fragments of a key — and the sentence it would land in is one
 * someone screenshots into a ticket. The host logs it and sends the class plus a
 * chosen sentence instead.
 */
export interface ProbeResult {
  ok: boolean;
  /** The failure class, absent on success. */
  class?: ProbeClass;
  /** One sentence, chosen host-side by the same `describe` the console mirrors. */
  message?: string;
  /** How many models the endpoint published. Zero is not a failure. */
  modelCount: number;
  /**
   * Whether the model that was asked about is in that catalog.
   *
   * Absent when none was asked about, or when the endpoint publishes no
   * catalogue to check against. **Absent is not a failure** — an Azure
   * deployment name is never published by design.
   */
  modelKnown?: boolean;
}

/** Every provider write answers with the whole status, so nothing has to be reconciled. */
export interface ProviderMutation {
  status: InferenceStatus;
  note: string;
  /** The probe's verdict, when one ran. Absent when there was nothing to check. */
  probe?: ProbeResult;
  /** Tiers this change moved or parked, so the console can say which rows changed. */
  affectedTiers?: string[];
}

/** The add-provider body. `key` is write-only (never returned). */
export interface AddProviderInput {
  /** A catalogue slug, a CLI option slug, or `custom`. */
  kind: string;
  /** The operator's name, for a custom provider only. */
  label?: string;
  /** The endpoint, for a local runtime or a custom provider. */
  baseUrl?: string;
  /** The outbound credential. */
  key?: string;
  /**
   * Add despite a probe failure that would otherwise be destructive.
   *
   * Offered only after a **typed probe failure**, never after a slug collision
   * or a failed key write, and cleared on every retry — so an attempt that fails
   * for an unrelated reason does not still offer to skip verification.
   */
  addAnyway?: boolean;
}

/** The edit body. Omit a field to leave it; send `key: ""` to clear the key. */
export interface EditProviderInput {
  label?: string;
  baseUrl?: string;
  models?: Record<string, string>;
  key?: string;
}

/** The routing table as the host holds it. */
export interface RoutesResponse {
  /** Tier → route string (`acme:gpt-5`, `managed`, `local:llava`). */
  routes: Record<string, string>;
  /** The mode these routes describe. **Inferred host-side, never stored.** */
  mode: RoutingMode;
  /** Routes naming a provider this company does not hold, as `[tier, slug]`. */
  orphaned: [string, string][];
}

/** Connect a provider. */
export function addProvider(
  client: OpenCompanyClient,
  company: string | null,
  body: AddProviderInput,
): Promise<ProviderMutation> {
  return client.post<ProviderMutation>(`${client.scopeFor(company)}/inference/providers`, body);
}

/** Change a connected provider. The slug is fixed; the kind cannot change. */
export function editProvider(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
  body: EditProviderInput,
): Promise<ProviderMutation> {
  return client.put<ProviderMutation>(
    `${client.scopeFor(company)}/inference/providers/${encodeURIComponent(slug)}`,
    body,
  );
}

/**
 * Disconnect a provider.
 *
 * Clears its credential, removes the record and resets every route pointing at
 * it, as one operation. The response names the tiers that moved.
 */
export function deleteProvider(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
): Promise<ProviderMutation> {
  return client.del<ProviderMutation>(
    `${client.scopeFor(company)}/inference/providers/${encodeURIComponent(slug)}`,
  );
}

/**
 * Switch a provider on or off.
 *
 * Distinct from deleting it: a disabled provider keeps its endpoint, its label
 * and its credential, and is simply not a routing target. Routes pointing at it
 * are **parked, not scrubbed**, so switching it back on restores them — the
 * response names which ones.
 */
export function setProviderEnabled(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
  enabled: boolean,
): Promise<ProviderMutation> {
  return client.post<ProviderMutation>(
    `${client.scopeFor(company)}/inference/providers/${encodeURIComponent(slug)}/enabled`,
    { enabled },
  );
}

/**
 * Test an endpoint and a key that are **not stored yet**.
 *
 * `POST …/inference/test` probes the *saved* config, which by definition does
 * not exist at the moment an operator wants to know whether what they have typed
 * will work.
 */
export function probeDraft(
  client: OpenCompanyClient,
  company: string | null,
  body: { baseUrl: string; key?: string; kind?: string },
): Promise<ProbeResult> {
  return client.post<ProbeResult>(`${client.scopeFor(company)}/inference/probe`, body);
}

/**
 * Switch the managed tier in or out of routing.
 *
 * **Not the credential.** Every step of its chain stays where it is; what
 * changes is whether a workload may be routed there.
 */
export function setManagedEnabled(
  client: OpenCompanyClient,
  company: string | null,
  enabled: boolean,
): Promise<ProviderMutation> {
  return client.post<ProviderMutation>(`${client.scopeFor(company)}/inference/managed/enabled`, {
    enabled,
  });
}

/**
 * Check whatever the managed chain resolves to.
 *
 * The credential it presents is whichever step answers — which for a company on
 * the instance identity is the server's, tested against the platform endpoint,
 * exactly as its turns would. It never deletes anything, whatever the answer.
 */
export function testManaged(
  client: OpenCompanyClient,
  company: string | null,
): Promise<ProbeResult> {
  return client.post<ProbeResult>(`${client.scopeFor(company)}/inference/managed/test`, {});
}

/** One provider's own model catalog. */
export interface ProviderCatalog {
  /** The endpoint the catalog was read from. */
  baseUrl: string;
  /** Every model that endpoint publishes, sorted. Empty when `error` is set. */
  models: string[];
  /**
   * Whether this endpoint's `model` field keys on a **deployment name** rather
   * than a published model id.
   *
   * Azure separates the base model a deployment was made from
   * (`gpt-5.6-terra-2026-07-09`) from the deployment name (`gpt-5.6-terra`) that
   * actually routes the request, and `/models` publishes the first while the
   * request body wants the second. A closed dropdown there makes the only
   * correct value unreachable, so the field defaults to free text.
   */
  freeTextOnly: boolean;
  /** Why the list is empty, naming the endpoint. */
  error?: string;
}

/**
 * That provider's own catalog — **per provider, not per company**.
 *
 * Two providers are two catalogs. `listInferenceModels` answers for the
 * *configured* endpoint, which was the only question worth asking when a company
 * had one provider and is a different question now.
 *
 * The stored key is presented host-side; the console never sees it.
 */
export function listProviderModels(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
): Promise<ProviderCatalog> {
  return client.get<ProviderCatalog>(
    `${client.scopeFor(company)}/inference/providers/${encodeURIComponent(slug)}/models`,
  );
}

/**
 * Paste a key for the managed tier — step 1 of its chain.
 *
 * Managed has no provider record, so this is not an ordinary add: it resolves
 * from a chain rather than from a row, and a record for it would collide with
 * the company's own entry zero. Send `""` to clear the key and fall back down
 * the chain to the company's account, then the instance's.
 *
 * The other half of setting managed up is the hub link flow, which writes the
 * company **account** rather than a key. That lives on Connections → Account
 * and is deliberately not duplicated here.
 */
export function setManagedKey(
  client: OpenCompanyClient,
  company: string | null,
  key: string,
): Promise<ProviderMutation> {
  return client.put<ProviderMutation>(`${client.scopeFor(company)}/inference/managed/key`, { key });
}

/**
 * Say which provider an **unset** workload goes through.
 *
 * Explicit rather than positional. Without it the default is whatever sorts
 * first: add three providers, delete the first, and the company's unrouted spend
 * moves to a different account with nothing on screen having changed to say so.
 *
 * Setting one clears the previous one — not as a second call, but because the
 * host keeps the marker in a single slot holding a single slug.
 */
export function setDefaultProvider(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
): Promise<ProviderMutation> {
  return client.post<ProviderMutation>(
    `${client.scopeFor(company)}/inference/providers/${encodeURIComponent(slug)}/default`,
    {},
  );
}

/**
 * Re-check a provider that is already connected.
 *
 * One of the three things that feed a row's health, and the only one an operator
 * can ask for — the others are the add-time probe and the turn path's own 401.
 * There is deliberately **no poller**: one would cost a request per provider per
 * interval across every company on the host, to learn something the next real
 * turn learns for free.
 *
 * It never deletes a credential, whatever the answer. An add is a commitment
 * being made and a rollback undoes it; a test is a question being asked, and
 * making the button that reports a problem the button that causes one would be a
 * trap.
 */
export function testProvider(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
  /** The model a routing row has chosen, so the check answers about that id. */
  model?: string,
): Promise<ProbeResult> {
  return client.post<ProbeResult>(
    `${client.scopeFor(company)}/inference/providers/${encodeURIComponent(slug)}/test`,
    model ? { model } : {},
  );
}

/** The routing table, its inferred mode, and any route naming a provider that is gone. */
export function getRoutes(
  client: OpenCompanyClient,
  company: string | null,
): Promise<RoutesResponse> {
  return client.get<RoutesResponse>(`${client.scopeFor(company)}/inference/routes`);
}

/**
 * Replace the routing table.
 *
 * A whole-table write, because the modes are whole-table statements: "route
 * everything through one model" is not four independent edits, and applying it
 * as four would leave a visible state where two rows have moved and two have not.
 */
export function putRoutes(
  client: OpenCompanyClient,
  company: string | null,
  routes: Record<string, string>,
): Promise<RoutesResponse> {
  return client.put<RoutesResponse>(`${client.scopeFor(company)}/inference/routes`, { routes });
}

/** Live-probe the resolved provider (one `ping` turn). */
export function testInference(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InferenceTestResult> {
  return client.post<InferenceTestResult>(`${client.scopeFor(company)}/inference/test`, {});
}
