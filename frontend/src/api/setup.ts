// The first-run setup API (`/api/v1/setup`): read what this instance is
// configured with, and apply a completed wizard.
//
// Standalone functions over the shared client, mirroring `api/policy.ts` and
// `api/inference.ts`, so neither `OpenCompanyClient` nor the shared
// `api/types.ts` needs to change.
//
// Field names are snake_case here rather than the camelCase most console
// surfaces use, matching the host's own DTOs. That is deliberate: half the
// payload's content *is* `config.toml` keys (`workspace.max_blob_mb`), and
// camel-casing the wrapper around snake_case keys reads worse than being
// consistent with the file the whole surface exists to write.

import type { OpenCompanyClient } from "./client";
import type { ComposioApiKeyTest } from "./composio";
import type { AddProviderInput, ProbeResult } from "./inference";
import { INFERENCE_MANAGED_HIDDEN } from "@/product-scope";

/** Which precedence layer supplied a field's current value. */
export type ConfigLayer = "env" | "config.toml" | "manifest" | "default";

/**
 * One configurable setting, with the layer that currently owns it.
 *
 * The layer is the important part. Config resolution is
 * `env ⟵ config.toml ⟵ manifest ⟵ default`, and setup can only write the
 * `config.toml` layer — so on a hosted instance, where the control plane
 * injects `OPENCOMPANY_*`, an edit to an env-owned field would write a file
 * that the next boot ignores. `editable` is the host's answer to that, and the
 * UI must render a non-editable field read-only rather than letting an operator
 * submit a change that will be refused.
 */
export interface SetupField {
  /** The dotted `config.toml` key, e.g. `bind` or `workspace.max_blob_mb`. */
  key: string;
  /** The value held in `config.toml`, or null when the file does not set it. */
  value: string | null;
  /** Which layer supplied it. */
  layer: ConfigLayer;
  /** False when `env` owns the field — `config.toml` cannot outrank it. */
  editable: boolean;
  /** Whether a change takes effect only after the host restarts. */
  requires_restart: boolean;
  /** A credential: `value` is always null, and the UI shows only its status. */
  secret: boolean;
}

/** A company template this instance can be seeded from. */
export interface SetupTemplate {
  /** Stable preset slug, e.g. `marketing_agency`. */
  id: string;
  /** Human-readable name. */
  name: string;
  /** How many agents the template's roster declares. */
  agent_count: number;
  /** What a company built from this template produces. */
  output: string | null;
}

/**
 * Which optional surfaces are compiled into the host's build.
 *
 * These are cargo features, not settings — nothing the wizard writes turns one
 * on. They exist so the flow can say "not in this build" instead of offering a
 * switch that does nothing.
 */
export interface SetupBuild {
  acp_in_build: boolean;
  /**
   * Whether the ACP JSON-RPC transport is actually mounted, which is a separate
   * question from whether the feature is compiled in: the host builds the ACP
   * session model but mounts no `/acp` route, so a client would get a 404 even
   * on a build with the feature on.
   */
  acp_transport_mounted: boolean;
  mcp_in_build: boolean;
  harness_in_build: boolean;
  oauth_in_build: boolean;
}

/** Everything the wizard needs to draw itself. */
export interface SetupStatus {
  /** Whether setup has already been completed on this instance. */
  complete: boolean;
  /** Absolute path of the `config.toml` a write lands in. */
  config_path: string;
  /** Every configurable field, in a stable order. */
  fields: SetupField[];
  /** The company templates this build ships. */
  templates: SetupTemplate[];
  /** Sign-in modes this host accepts. `none` is absent on a routable bind. */
  auth_modes: string[];
  /**
   * The mode the wizard should preselect when `config.toml` names none.
   *
   * `none` on the packaged desktop host, which already runs without a
   * sign-in: one machine, one person, no mailbox. Reported by the host so a
   * browser tab pointed at a desktop host gets the same answer as the webview.
   * Absent everywhere else, and on a host too old to report it.
   */
  default_auth_mode?: string;
  /** Which optional surfaces this build has. */
  build: SetupBuild;
  /** Companies already registered. Non-empty means the seed step is skipped. */
  companies: string[];
  /** What this host can already reach without the operator supplying anything. */
  inference: InferenceReady;
  /** What this host can do with a mailbox. */
  mail: MailReady;
  /**
   * Whether this GET was authorized by the platform SSO bootstrap session — the
   * console arrived via the dashboard's one-click SSO. When true the wizard
   * skips the sign-in step and sends no password: the apply signs that owner in
   * directly. Absent on a host too old to report it (treated as `false`).
   */
  sso_bootstrap?: boolean;
}

/**
 * What this host can do with a mailbox.
 *
 * Separate from `auth_modes`, which says which modes are *legal*: `email` stays
 * on that list whatever mail the host has, because a password signs people in
 * without a transport. This says whether a magic link can honestly be offered
 * today.
 *
 * Required rather than optional, deliberately. An optional field would let a
 * host too old to report it fall through to whatever the UI treats `undefined`
 * as and render confident copy about a mailbox nobody checked; required makes
 * the compiler name every place that has to decide.
 */
export interface MailReady {
  /** A transport and credentials are wired — a link is genuinely sent. */
  wired: boolean;
  /**
   * A minted code comes back in the response instead of going to a mailbox:
   * loopback bind, no public URL, no transport. The laptop case, where the
   * honest hand-off is a link rather than an inbox.
   */
  echoes_code: boolean;
}

/**
 * The credential the host already holds.
 *
 * A hosted tenant has one injected by the control plane, and its operator has no
 * key of their own and no way to get one. This is what lets the first step
 * arrive already answered rather than demanding something unobtainable.
 */
export interface InferenceReady {
  /** A credential is already resolvable — the design pass would run regardless. */
  ready: boolean;
  /** The provider behind it, for the picker's initial value. */
  provider: string | null;
  /** The endpoint it resolves to. Shown so a green tick is checkable. */
  base_url: string | null;
  /**
   * The API-keys page of the hub this host is on, for minting a key by hand.
   * `null` when the host's `api_url` follows no known convention; older hosts
   * omit it. Never a production default: a staging host links to staging.
   */
  keys_url?: string | null;
}

/** The providers this host can talk to (`INFERENCE_PROVIDERS`). */
export const INFERENCE_PROVIDERS = [
  {
    id: "managed",
    label: "TinyHumans",
    hint: "The managed endpoint. What a hosted company runs on.",
    needsUrl: false,
    needsKey: true,
  },
  {
    id: "openrouter",
    label: "OpenRouter",
    hint: "One key, many models. Billed by OpenRouter.",
    needsUrl: false,
    needsKey: true,
  },
  {
    id: "openai_compatible",
    label: "Other endpoint",
    hint: "Anything speaking the OpenAI chat API — a proxy, vLLM, a gateway.",
    needsUrl: true,
    needsKey: true,
  },
  {
    id: "ollama",
    label: "Ollama",
    hint: "A model running on this machine. No key needed.",
    needsUrl: true,
    needsKey: false,
  },
] as const;

export type InferenceProviderId = (typeof INFERENCE_PROVIDERS)[number]["id"];

/**
 * The providers the wizard offers. {@link INFERENCE_PROVIDERS} keeps every
 * entry, so a host already reporting a hidden one still resolves its label and
 * its key/URL requirements.
 */
export const SETUP_INFERENCE_OPTIONS = INFERENCE_PROVIDERS.filter(
  (provider) => provider.id !== "managed" || !INFERENCE_MANAGED_HIDDEN,
);

/** What the connection test answers. */
export interface InferenceTestResult {
  ok: boolean;
  /** The endpoint actually reached — a tick from the wrong URL is not a pass. */
  baseUrl: string;
  /** Concrete model discovered from the endpoint's OpenAI-compatible catalog. */
  model?: string | null;
  /** Present only on failure, already summarised into one actionable line. */
  error?: string;
}

/**
 * Probe a credential before anything is written.
 *
 * The design pass is deliberately silent about credentials — it falls back to a
 * curated team on any failure — so without this an operator who mistypes a key
 * gets a plausible company and finds out five screens later, if at all.
 *
 * The key is used and discarded by the host. Testing a credential and committing
 * to it are separate acts.
 */
export function testInference(
  client: OpenCompanyClient,
  body: { provider: string; key?: string | null; baseUrl?: string | null },
): Promise<InferenceTestResult> {
  return client.post<InferenceTestResult>("/api/v1/setup/inference/test", body);
}

/**
 * Read a drafted endpoint's model catalogue before a company exists.
 *
 * The same probe `POST {scope}/inference/probe` runs, reached through the
 * first-run gate rather than through the company admin one — the add-provider
 * sequence mounted in the wizard needs the endpoint's own list to offer, and
 * there is no company to scope the read to yet.
 *
 * Not {@link testInference}: that answers with one model rather than the list
 * the model step is made of, and it falls back to the host's own injected
 * credential when the key is blank — which would report a pass for a key this
 * operator never gave.
 */
export function probeSetupDraft(
  client: OpenCompanyClient,
  body: { baseUrl: string; key?: string; kind?: string },
): Promise<ProbeResult> {
  return client.post<ProbeResult>("/api/v1/setup/inference/probe", body);
}

/**
 * Check a Composio API key before there is a company to store it against.
 *
 * Takes the key, where the company-scoped route deliberately takes no body —
 * there is no store to read it from yet. The destination is not in the body
 * either way: the host dials Composio's own compile-time URL, so no caller can
 * point this anywhere.
 *
 * Without it an operator types a wrong key here and learns nothing until they
 * open Connections and find an empty tool belt.
 */
export function testSetupComposioKey(
  client: OpenCompanyClient,
  apiKey: string,
): Promise<ComposioApiKeyTest> {
  return client.post<ComposioApiKeyTest>("/api/v1/setup/composio/api-key/test", {
    apiKey,
  });
}

/**
 * A completed wizard.
 *
 * A `null` field value clears the key, letting the next precedence layer supply
 * it — which is not the same as sending `""`, a set-but-empty value that would
 * shadow the layer below instead of deferring to it.
 */
export interface SetupInput {
  fields: Record<string, string | null>;
  /** Ignored by the host when a company already exists. */
  template?: string | null;
  /**
   * A company the wizard designed, from the answers and the reviewed roster.
   *
   * Wins over {@link SetupInput.template} when both are present: someone who
   * answered three questions and edited a roster has expressed a preference a
   * preset cannot override.
   */
  company?: DesignedCompany | null;
  /**
   * What to call the company, as the review step asks for it.
   *
   * Applies to either path above. Omitted or blank means "derive it", which is
   * what every company built here used to get: the host takes the first clause
   * of {@link DesignedCompany.industry}, so the answer to "what kind of company
   * are you setting up?" silently became the company's name — and its id, which
   * is then permanent.
   */
  name?: string | null;
  /**
   * The address that will be able to sign in, for the template path.
   *
   * {@link DesignedCompany.adminEmail} carries the same thing for a designed
   * company. This one exists because a seeded template has no designed company
   * to carry it in, and no shipped template names an admin — so a template plus
   * a sign-in mode would otherwise finish setup into a company nobody can
   * administer.
   *
   * Snake case, unlike the camelCase inside {@link DesignedCompany}: the top
   * level of this request is read field-for-field by the host.
   */
  admin_email?: string | null;
  /**
   * The first admin's password, set on the account the moment the company
   * exists so the wizard can sign the operator straight in. Write-only.
   * Ignored by a host with no sign-in, and by one too old to know the field.
   */
  admin_password?: string | null;
  /**
   * The TinyHumans account key the managed branch collected, stored against
   * the company this call seeds and fanned out from there.
   *
   * The **company's** credential — what `setCompanyCredential` writes from the
   * Connections Account page — not the instance-wide `tinyhumans_api_key`
   * config field, which is why it does not travel in {@link SetupInput.fields}.
   * Sent here rather than written by a second request because
   * `PUT …/credential` is admin-scoped to an existing company and first run
   * has neither: this call is what creates the company, and nobody has signed
   * in yet to be its admin.
   *
   * Top level, so it survives the template path too — a managed operator who
   * kept an untouched preset roster sends a slug and no designed company.
   */
  tinyhumans_key?: string | null;
  /**
   * The model to finish the TinyHumans row with — the one the setup probe
   * actually reached.
   *
   * Omitted when the probe named none, which the fan-out reports back through
   * {@link SetupApplied.credential_note} rather than leaving the row silently
   * unmade.
   */
  tinyhumans_model?: string | null;
  /**
   * The provider the self-managed branch connected, added to the company this
   * call seeds.
   *
   * The **same body** `POST …/inference/providers` takes — so the wizard's
   * provider is created by the same host function the LLM page's add runs,
   * with its slot guard, its first-provider default, its rollback pair and its
   * auto-route, rather than by a wizard-only write that would land the row
   * without any of them.
   *
   * Carried on the apply rather than sent from the step that collected it,
   * because that route is admin-scoped to an existing company and first run
   * has neither.
   */
  provider_draft?: AddProviderInput | null;
  /**
   * The Composio credential the self-managed branch collected, stored against
   * the company this call seeds.
   *
   * One field for two routes, because the Connections dialog is one form for
   * both: a company's own Composio API key (which also selects BYOK), or a
   * token for the TinyHumans-managed route. The values are the same two
   * `ComposioForm.credential` carries, so the form's own answer travels
   * unchanged.
   */
  composio_draft?: { credential: "composio-api-key" | "composio-token"; value: string } | null;
}

/** The company the wizard designed, as the review step hands it over. */
export interface DesignedCompany {
  industry: string;
  teamHint: string;
  automate: string;
  /**
   * The roster **as reviewed** — renamed, removed and reordered by the operator.
   * Sent back rather than regenerated, so what they approved is exactly what
   * gets built; a second pass could return a different team.
   */
  agents: DesignedAgent[];
  /**
   * The address that will be able to sign in, written into `[users].admins`.
   *
   * Not optional in spirit: no shipped template invites anybody, so without it
   * an operator who chose email sign-in finishes setup and can then sign in as
   * nobody. Omitted only on a host that needs no sign-in at all.
   */
  adminEmail?: string | null;
  /** The tested provider to persist onto the new company. */
  inference?: SetupInferenceInput | null;
}

export interface SetupInferenceInput {
  provider: string;
  baseUrl?: string | null;
  model?: string | null;
  /** Write-only; stored in the company's secret store. */
  key?: string | null;
}

export interface DesignedAgent {  name: string;
  role: string;
  description: string;
  /**
   * The job shape the design pass assigned, carried back untouched.
   *
   * This is what narrows the teammate's tool belt on the host. Dropping it here
   * would silently hand every designed agent the company-wide belt — which is
   * `["*", "media", "composio"]` by default, i.e. real-money media and
   * per-tenant credentials — so the round trip is load-bearing, not incidental.
   */
  focus?: string | null;
}

/** One agent the host proposes for a company that does not exist yet. */
export interface SetupRosterAgent {
  name: string;
  role: string;
  description: string;
  /**
   * The job shape that decides this teammate's tool belt on the host
   * (`AgentFocus` in `src/company/setup.rs`).
   *
   * Carried, never shown and never edited. The console has no business picking
   * a permission boundary; round-tripping it untouched is what makes the belt
   * an operator approves on the review screen the belt they actually get.
   */
  focus?: string | null;
}

/** What `POST /api/v1/setup/roster` answers. */
export interface SetupRoster {
  agents: SetupRosterAgent[];
  /** Which curated roster framed the proposal, e.g. `ecommerce`. */
  template: string;
  /**
   * `model` — designed from the answers. `fallback` — the curated team matched
   * from them. `preset` — the roster of the template the operator *picked*,
   * shipped whole.
   *
   * The last one is the only source the console may send back as a
   * {@link SetupInput.template} rather than as a designed company: it is the
   * one case where the host can seed the real thing — that template's belt and
   * prompts as well as its roster — instead of rebuilding it from this screen.
   */
  source: "model" | "fallback" | "preset";
  /**
   * The jobs the operator named, as the **host** split them.
   *
   * Echoed on the review screen so the list the roster was judged against is the
   * list they can see — a bad split is visible to the person who typed it rather
   * than silently shaping a prompt.
   */
  jobs?: string[];
  /**
   * The jobs no teammate on this roster owns.
   *
   * Non-empty only when `source` is `"model"`: coverage is a claim the design
   * pass makes and the host checks against its own list, by set maths. A curated
   * team was chosen by keyword and never read the list, so it claims nothing
   * about it.
   */
  uncovered?: string[];
  /**
   * Why this is the curated team, when it is. Absent on the `model` path.
   *
   * `"no_model"` — no credential was reachable, so no design pass ran.
   * `"model_unreachable"` — a credential is wired, but its provider did not
   * answer in time.
   * `"not_designable"` — a model answered and the answer was unusable: too thin,
   * unreadable, or the reference team handed back unchanged. In practice, the
   * answers were too sparse to design from.
   * `"output_budget_exhausted"` — a model answered but stopped on its output-token
   * limit before writing anything usable (a reasoning model that spent the
   * whole budget deliberating). Retry, or use a model with more output room.
   *
   * The review screen needs the distinction because the **action differs**. It
   * used to say "we couldn't reach a model" for every fallback, which is a plain
   * falsehood in the second case — and it pointed the operator at adding a key
   * when what they actually needed was to say more about their business.
   */
  reason?:
    | "no_model"
    | "model_unreachable"
    | "not_designable"
    | "output_budget_exhausted";
}

/**
 * Ask the host to design a starting team, before any company exists.
 *
 * The company-scoped twin (`api/company-setup.ts`) cannot serve the wizard: it
 * resolves a company, and during first-run setup there is none.
 *
 * `inferenceKey` is the credential the operator has just typed into this wizard,
 * passed so the design can run on it before anything is written. The host uses
 * it and discards it — the apply that persists it is still a single atomic step.
 */
export function proposeSetupRoster(
  client: OpenCompanyClient,
  body: {
    industry: string;
    teamHint: string;
    automate: string;
    template?: string | null;
    inferenceKey?: string | null;
    inferenceProvider?: string | null;
    inferenceBaseUrl?: string | null;
    inferenceModel?: string | null;
    /**
     * The operator chose "no model" and means it, so no model designs this
     * roster whatever the host itself holds.
     *
     * Sending no credential does not say this: the host reads that as "use
     * what you have", and a hosted tenant has one injected — which would
     * design a roster with a model the operator had just declined.
     */
    forceCurated?: boolean;
  },
): Promise<SetupRoster> {
  return client.post<SetupRoster>("/api/v1/setup/roster", body);
}

/** What an apply reports back. */
export interface SetupApplied {
  complete: boolean;
  config_path: string;
  /** Keys whose new value only takes effect after a restart. */
  restart_required: string[];
  /** The company seeded by this call, if any. */
  seeded_company: string | null;
  /**
   * What the account-key fan-out did, in the host's own words — the same
   * sentence the Account page's save toast carries.
   *
   * Absent when no key was sent, and absent on a host predating the field.
   * Rendered verbatim rather than summarised: the fan-out honestly reports
   * the slots it left alone, and a wizard that answered "all set" over that
   * would be the one thing this step exists to stop doing.
   */
  credential_note?: string | null;
  /**
   * What connecting the self-managed branch's provider did, in the host's own
   * words — the same sentence the LLM page's add toast carries.
   *
   * Absent when no provider was drafted, and absent on a host predating the
   * field. Carries the refusal too: the company is built by the time the add
   * runs, so an endpoint that stopped answering is said rather than turned
   * into a failed setup.
   */
  provider_note?: string | null;
  /**
   * What the Composio credential the wizard collected did. Absent when none was
   * sent, and absent on a host predating the field.
   */
  composio_note?: string | null;
}

/** Read this instance's setup state. */
export function getSetup(client: OpenCompanyClient): Promise<SetupStatus> {
  return client.get<SetupStatus>("/api/v1/setup");
}

/** Apply a completed wizard. All-or-nothing: a refusal writes nothing. */
export function submitSetup(
  client: OpenCompanyClient,
  body: SetupInput,
): Promise<SetupApplied> {
  return client.post<SetupApplied>("/api/v1/setup", body);
}

/** The subset of fields a given wizard step owns, in payload order. */
export function fieldsFor(status: SetupStatus, keys: readonly string[]): SetupField[] {
  return keys
    .map((key) => status.fields.find((f) => f.key === key))
    .filter((f): f is SetupField => f !== undefined);
}

/**
 * The fields a wizard should actually submit, given the form's current values.
 *
 * Pure, and separate from the component, because three rules meet here and each
 * one silently corrupts a config if it is wrong:
 *
 *   1. **Unchanged fields are omitted.** Re-sending a field's existing value is
 *      harmless for most keys but would re-assert a value the operator never
 *      looked at, turning "I set the bind address" into "I confirmed all
 *      thirteen of these", which is not what they did.
 *   2. **Env-owned fields are never sent.** The host refuses them, and since an
 *      apply is all-or-nothing, including one would fail the whole submission
 *      over a field the form rendered read-only in the first place.
 *   3. **Secrets are write-only.** The host never echoes a credential, so
 *      "unchanged" cannot be detected by comparison — an empty box means "leave
 *      it alone", never "clear it". Only a typed value is sent.
 *
 * A cleared (empty) editable field becomes `null`, which deletes the key so the
 * next precedence layer applies. Sending `""` instead would write a
 * set-but-empty value that shadows that layer rather than deferring to it.
 */
export function changedFields(
  status: SetupStatus,
  values: Record<string, string>,
): Record<string, string | null> {
  const out: Record<string, string | null> = {};
  for (const f of status.fields) {
    const typed = values[f.key];
    if (f.secret) {
      // Rule 3 — and rule 2 still applies: an env-owned secret is not writable.
      if (f.editable && typed !== undefined && typed !== "") out[f.key] = typed;
      continue;
    }
    if (!f.editable) continue; // rule 2
    // Absent is "untouched", not "cleared". The two are only the same when the
    // form has an entry for every field, which is true today (the wizard seeds
    // from the file on load) and is exactly the kind of invariant that stops
    // being true quietly. Reading absent as a clear would delete a key the
    // operator never saw, on a step they skipped.
    if (typed === undefined) continue;
    if (typed === (f.value ?? "")) continue; // rule 1
    out[f.key] = typed === "" ? null : typed;
  }
  return out;
}
