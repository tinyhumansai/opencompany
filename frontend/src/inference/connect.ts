// Connecting a provider, as decisions rather than markup.
//
// What the add dialog offers, what each category asks for, and whether a typed
// name may be used — every branch the connect flow makes, as a plain function
// over plain data. The dialogs are then layout plus handlers, with nothing in
// them that deserves a test of its own.
//
// These mirror rules the host also holds (`store::slugify`, `store::check_slug`,
// `catalogue::normalize_local_endpoint`). That is not a second opinion: the
// console needs them to disable a button before a request is made, and the host
// needs them because a console is not a security boundary. Where they overlap
// they are written to agree, and both sides are pinned.

import {
  CLI_LOGINS,
  CLOUD_PROVIDERS,
  COPY,
  LOCAL_RUNTIMES,
  cloudProvider,
  endpointHost,
  isReservedSlug,
  localRuntime,
} from "./catalogue";
import type { DefaultChoice, Provider } from "./types";

/** One choosable row in the add dialog. */
export interface AddOption {
  /** What is sent as `kind`. */
  value: string;
  /** The name. */
  label: string;
  /** The second line — monospace, and different per category. */
  detail: string;
}

/** The three lists, each already filtered to what is not yet connected. */
export interface AddOptions {
  cloud: AddOption[];
  local: AddOption[];
  cli: AddOption[];
}

/**
 * Whether this company already holds the provider a catalogue option would add.
 *
 * **Codex is the trap and this is where it is sprung.** The Codex CLI login *is*
 * an OpenAI credential, so it stores under the `openai` slug and surfaces as the
 * OpenAI row. Keying its already-connected check on the literal `codex` never
 * matches, so the dialog would offer Codex forever however many times it was
 * connected. The check is on the slug the option **stores under**, not the slug
 * it is called.
 */
export function isConnected(providers: readonly Provider[], optionSlug: string): boolean {
  const cli = CLI_LOGINS.find((c) => c.optionSlug === optionSlug);
  const stored = cli?.storedSlug ?? optionSlug;
  return providers.some((p) => p.slug === stored);
}

/** One entry in a connected provider's ⋯ menu. */
export interface ProviderRowAction {
  // "setDefault", not "default" (round-2 live lane, KR-L1-04): the row's
  // testid for this action is derived as `inference-provider-${slug}-${id}`,
  // and the "Default" badge next to it already uses
  // `inference-provider-${slug}-default` — the same id here made the two
  // share one testid, which threw on any strict single-element lookup.
  id: "edit" | "setDefault" | "replaceKey" | "removeKey" | "remove";
  label: string;
  /** Rendered in the destructive style, and confirmed before it runs. */
  destructive?: boolean;
}

/**
 * What a connected provider's ⋯ menu offers.
 *
 * **Per kind, and derived rather than listed.** The key actions key on
 * {@link credentialAsk}`.needsKey`, which is the same function the connect
 * dialog uses to decide whether to show a key field — so a local runtime that
 * asks for an endpoint and a CLI login whose credential lives in another tool
 * cannot be offered "Replace key" for something they do not have. A hand-written
 * list of kinds here would be a second opinion about that, and the two would
 * drift the first time a row is added to the catalogue.
 *
 * Two entries are conditional for the same reason: an action that would change
 * nothing does not belong in a menu. "Set as default" is absent on the provider
 * that already is one, and on one switched off (which cannot be a routing target
 * at all). "Remove key" is absent where there is no key to remove — the store
 * has no delete, so removing one is a write of the empty string, and offering it
 * against nothing would be a destructive-looking no-op.
 */
export function providerMenu(
  provider: Pick<Provider, "kind" | "enabled" | "keyConfigured"> & {
    isDefault?: boolean;
    origin?: "entryZero" | "indexed";
  },
): ProviderRowAction[] {
  // **Entry zero is changed through the inference config, not as a list entry.**
  // The host says so three times, in three separate 400s, and the console had no
  // way to know which row they applied to — so it rendered Edit, Replace key and
  // Remove provider live on a row where every one of them is a round trip to a
  // refusal. The rules do not move; this stops offering what cannot work.
  //
  // Setting it as the default is not one of the three: it is a marker on the
  // company, not a write to the row, and it is the one thing the operator may
  // genuinely want from this row.
  if (provider.origin === "entryZero") {
    return provider.isDefault || !provider.enabled ? [] : [{ id: "setDefault", label: "Set as default" }];
  }
  const ask = credentialAsk(provider.kind);
  const actions: ProviderRowAction[] = [
    { id: "edit", label: ask.needsEndpoint ? "Edit endpoint" : "Edit" },
  ];
  if (!provider.isDefault && provider.enabled) {
    actions.push({ id: "setDefault", label: "Set as default" });
  }
  if (ask.needsKey) {
    actions.push({
      id: "replaceKey",
      label: provider.keyConfigured ? "Replace key" : "Add a key",
    });
    if (provider.keyConfigured) {
      actions.push({ id: "removeKey", label: "Remove key", destructive: true });
    }
  }
  actions.push({ id: "remove", label: "Remove provider", destructive: true });
  return actions;
}

/**
 * The slug TinyHumans is offered and keyed under — also the legacy Managed
 * fallback chain's own identity in the route grammar.
 *
 * @deprecated keys-rework #2306: the name is overdue for a rename now that
 * TinyHumans is an ordinary catalogue row (slice 2a) rather than a
 * managed-only sentinel, but the value is a stored slug
 * (`provider/tinyhumans/key`) and every call site below still needs it, so the
 * rename is deferred rather than done as drive-by churn here.
 */
export const MANAGED_OPTION_SLUG = "tinyhumans";

/**
 * What each category offers, minus what is already connected.
 *
 * **Each category lists only what is not yet connected.** The page behind the
 * modal shows the rest, and offering to add something twice is how you get two
 * rows for one provider.
 *
 * TinyHumans is one more `CLOUD_PROVIDERS` row now (keys rework, issue #2306,
 * slice 2a) rather than a special-cased list entry keyed on the legacy managed
 * chain's resolution — `isConnected` already hides it once a `tinyhumans` row
 * exists (an added row, or entry zero on a managed config), which is exactly
 * the one-row rule (decision Q3). The legacy chain resolving through the
 * company account or the instance identity with **no** row yet is still real
 * and is shown separately, on the Connected list's own legacy row
 * (`showsLegacyManagedRow` in `ProviderList.tsx`) — not duplicated here.
 *
 * The detail lines differ per category because the categories ask three
 * different questions: a cloud provider is identified by the host its key goes
 * to, a local runtime by the fact that it is local, and a CLI login by whose
 * credential it borrows.
 */
export function addOptions(providers: readonly Provider[]): AddOptions {
  return {
    cloud: CLOUD_PROVIDERS.filter((p) => !isConnected(providers, p.slug)).map((p) => ({
      value: p.slug,
      label: p.label,
      // The host, not the whole URL: the path is noise at a glance and the
      // host is the part an operator recognises.
      detail: endpointHost(p.endpoint),
    })),
    local: LOCAL_RUNTIMES.filter((r) => !isConnected(providers, r.slug)).map((r) => ({
      value: r.slug,
      label: r.label,
      detail: COPY.detailLocal,
    })),
    // No add-provider flow by design: a CLI login has no key and no endpoint,
    // and a stored "connected" flag would be a second source of truth that
    // could disagree with the CLI actually being installed. A teammate binds
    // to one on its own Model tab. Kept populated because the label lookup
    // still resolves an already-connected CLI row.
    cli: CLI_LOGINS.filter((c) => !isConnected(providers, c.optionSlug)).map((c) => ({
      value: c.optionSlug,
      label: c.label,
      detail: COPY.detailCli,
    })),
  };
}


/** What the key dialog for a chosen option has to ask for. */
export interface CredentialAsk {
  /** The dialog's title. */
  title: string;
  /** Whether an API key field is shown. */
  needsKey: boolean;
  /** Whether an endpoint field is shown. */
  needsEndpoint: boolean;
  /** What a key for this provider looks like, for the input placeholder. */
  keyPlaceholder?: string;
  /** A starting endpoint, where one is conventional. */
  defaultEndpoint?: string;
}

/**
 * The endpoint a draft of `optionSlug` would be probed at, or `null` when there
 * is nothing to probe.
 *
 * A cloud provider's comes from the preset — the paths in that table are too
 * varied to derive and the operator never types one. A local runtime's is the
 * thing being chosen, so it comes from the field. A CLI login has neither and
 * skips the probe entirely, which is the same call the host makes.
 *
 * Used to ask an endpoint what it publishes **before** a record is written, so
 * the dialog can offer a model rather than the host refusing after a round trip.
 */
export function probeEndpoint(optionSlug: string, typed?: string): string | null {
  // TinyHumans (`MANAGED_OPTION_SLUG`) is a `cloudProvider` row now (keys
  // rework, issue #2306, slice 2a), so this branch already answers it with
  // the proxy endpoint — no special case needed.
  const cloud = cloudProvider(optionSlug);
  if (cloud) return cloud.endpoint;
  return typed ? normalizeEndpoint(typed) : null;
}

/**
 * What connecting `optionSlug` asks the operator for.
 *
 * The three categories are three different questions, and this is the function
 * that says so: **cloud wants a key** (its endpoint is a preset, and the paths in
 * that table are too varied to be typed), **local wants an endpoint** (that is
 * the thing being chosen), and **a CLI login wants nothing** because another tool
 * already holds the credential.
 *
 * No local runtime demands a key. `omlx` used to, and the host enforces this
 * value, so it could not be added at all — no build of any of the three projects
 * called "omlx" requires one. Accepting a key is a separate question from
 * requiring one, and only the second belongs here.
 */
export function credentialAsk(optionSlug: string): CredentialAsk {
  const cloud = cloudProvider(optionSlug);
  if (cloud) {
    return {
      title: `Connect ${cloud.label}`,
      needsKey: true,
      needsEndpoint: false,
      keyPlaceholder: cloud.keyPlaceholder,
    };
  }
  const local = localRuntime(optionSlug);
  if (local) {
    return {
      title: `Connect ${local.label}`,
      needsKey: local.needsKey,
      needsEndpoint: true,
      defaultEndpoint: local.defaultEndpoint,
    };
  }
  const cli = CLI_LOGINS.find((c) => c.optionSlug === optionSlug);
  if (cli) {
    return { title: `Connect ${cli.label}`, needsKey: false, needsEndpoint: false };
  }
  // TinyHumans (`MANAGED_OPTION_SLUG`) is a `CLOUD_PROVIDERS` row now (keys
  // rework, issue #2306, slice 2a), so the `cloud` branch above already
  // answers it: title "Connect TinyHumans", `needsKey: true`,
  // `needsEndpoint: false`, placeholder "th-...". Its *other* way to set up —
  // an account link rather than a key — is offered beside the field in
  // `ProviderConnectDialog`, not duplicated here.
  return {
    title: "Add cloud provider",
    needsKey: true,
    needsEndpoint: true,
    keyPlaceholder: "sk-...",
  };
}

/**
 * Turns a typed name into a slug.
 *
 * **The slug is derived, never typed.** An operator names the thing; the address
 * falls out. Asking for both invites them to disagree, and the one they see in a
 * routing entry would then be the one they never chose. Mirrors `store::slugify`.
 */
export function slugify(label: string): string {
  let out = "";
  let lastDash = true;
  for (const ch of label.trim()) {
    if (/[a-zA-Z0-9]/.test(ch)) {
      out += ch.toLowerCase();
      lastDash = false;
    } else if (!lastDash) {
      out += "-";
      lastDash = true;
    }
  }
  return out.replace(/-+$/, "");
}

/**
 * The longest a provider name may be, in characters.
 *
 * Mirrors `store::MAX_PROVIDER_NAME_CHARS` on the host, which is where the rule
 * actually lives — the name becomes the address of a secret
 * (`provider/<slug>/key`), and an unbounded name produced an unbounded path
 * that 500ed a credential read and truncated a stored key on the way to failing
 * the delete. This copy only spares the operator a round trip to find that out.
 */
export const MAX_PROVIDER_NAME_CHARS = 80;

/** Why a slug cannot be used. */
export type SlugError = "empty" | "taken" | "reserved" | "too-long";

/**
 * Whether a derived slug may be used for a **custom** provider.
 *
 * Four named failures rather than a boolean, because they need four different
 * sentences: one is "pick another name", one is "you already have this", one
 * is "that name belongs to something we ship", and one is "that name is too
 * long".
 *
 * The catalogue check applies to custom providers only. Adding the catalogue's
 * own `groq` entry *should* take the slug `groq` — that is the same provider,
 * not a collision.
 */
export function checkSlug(providers: readonly Provider[], slug: string): SlugError | null {
  const trimmed = slug.trim();
  if (!trimmed) return "empty";
  if ([...trimmed].length > MAX_PROVIDER_NAME_CHARS) return "too-long";
  if (providers.some((p) => p.slug === trimmed)) return "taken";
  if (isReservedSlug(trimmed)) return "reserved";
  return null;
}

/** What to say about a slug that cannot be used. */
export function slugErrorCopy(error: SlugError): string {
  switch (error) {
    case "empty":
      return "Enter a provider name to generate a slug.";
    case "taken":
      return "This company already has a provider with that name.";
    case "reserved":
      return "That name belongs to a built-in provider.";
    case "too-long":
      return `A provider name can be at most ${MAX_PROVIDER_NAME_CHARS} characters.`;
  }
}

/**
 * A typed endpoint, normalised — or `null` when it is not one.
 *
 * `/v1` is appended when the path is empty, because that is where an
 * OpenAI-compatible surface lives and `http://localhost:11434` is what the
 * runtime's own documentation prints. A path the operator supplied is left
 * exactly as typed: appending is not guessing, and someone who typed a path
 * meant it. Mirrors `catalogue::normalize_local_endpoint`.
 */
export function normalizeEndpoint(raw: string): string | null {
  const trimmed = raw.trim().replace(/\/+$/, "");
  if (endpointHasCredentials(trimmed)) return null;
  const split = trimmed.indexOf("://");
  if (split === -1) return null;
  const scheme = trimmed.slice(0, split).toLowerCase();
  if (scheme !== "http" && scheme !== "https") return null;
  const rest = trimmed.slice(split + 3);
  if (!rest.trim()) return null;
  if (!rest.includes("/")) return `${trimmed}/v1`;
  return trimmed;
}

/**
 * Whether an endpoint URL carries a credential in its authority
 * (`http://user:password@host/v1`).
 *
 * Mirrors `catalogue::endpoint_has_credentials`. The host refuses such an
 * endpoint at every point one can be set, and redacts it anywhere one is said;
 * this copy exists so the operator is told *why* beside the field rather than
 * after a round trip.
 *
 * The `@` has to be inside the authority — a path may legitimately contain one
 * (`https://host/v1/@me`), and that is not a credential.
 */
export function endpointHasCredentials(raw: string): boolean {
  // As a URL parser reads it: ASCII tab, LF and CR are removed wherever they
  // appear, so `http:\t//alice:pw@host` is credentialed (Codex review on #2281).
  const trimmed = raw.trim().replace(/[\t\n\r]/g, "");
  // Query and fragment are never an authority.
  const cut = trimmed.search(/[?#]/);
  const head = cut === -1 ? trimmed : trimmed.slice(0, cut);
  // Read as an HTTP client reads it, mirroring the host's
  // `endpoint_credential_range`: a leading `http:`/`https:` (any case) or other
  // `scheme://`, any run of `/` or `\`, then the authority up to the next `/`
  // or `\`. The read continues past an authority only for a doubled scheme
  // (`http://HTTP://alice:pw@host`), never into ordinary path text, so
  // `https://gateway.example/proxy/http:user@example.com/v1` stays an endpoint.
  let pos = 0;
  for (let hop = 0; hop < 8; hop++) {
    const scheme = /^(?:https?:|[A-Za-z][A-Za-z0-9+.-]*:\/\/)/i.exec(head.slice(pos));
    if (!scheme && pos > 0) return false;
    const after = pos + (scheme ? scheme[0].length : 0);
    const from = after + (/^[/\\]*/.exec(head.slice(after))?.[0].length ?? 0);
    const tail = head.slice(from);
    const end = tail.search(/[/\\]/);
    const authority = end === -1 ? tail : tail.slice(0, end);
    if (authority.includes("@")) return true;
    // A doubled `scheme://scheme://` only: one slash after a bare scheme is a
    // host with an empty port (`http://http:/v1@beta`), as on the host.
    const doubled = /^[/\\]{2}/.test(tail.slice(authority.length));
    if (from === pos || !doubled || !/^[A-Za-z][A-Za-z0-9+.-]*:$/.test(authority)) return false;
    pos = from;
  }
  return false;
}

/**
 * Whether the custom-provider dialog's Add button may be pressed.
 *
 * Every reason it cannot, answered in one place so the button's disabled state
 * and the inline errors beside the fields cannot disagree about the same form.
 */
export function customProviderReady(
  providers: readonly Provider[],
  draft: { label: string; baseUrl: string },
): boolean {
  return (
    checkProviderName(draft.label) === null &&
    checkSlug(providers, slugify(draft.label)) === null &&
    normalizeEndpoint(draft.baseUrl) !== null
  );
}

/**
 * Whether a typed provider **name** may be used, before any slug is derived.
 *
 * Mirrors `store::check_provider_name`. Separate from {@link checkSlug} for the
 * same reason it is separate on the host: a name can be long while its slug is
 * short, because `slugify` drops everything that is not alphanumeric.
 */
export function checkProviderName(label: string): SlugError | null {
  const trimmed = label.trim();
  if (!trimmed) return "empty";
  if ([...trimmed].length > MAX_PROVIDER_NAME_CHARS) return "too-long";
  return null;
}

/**
 * Cuts a typed provider name to {@link MAX_PROVIDER_NAME_CHARS} Unicode code
 * points — the unit the host counts with `chars()`.
 *
 * Not the native `maxLength` attribute, which counts UTF-16 code units: most
 * emoji are one character to the host and two to the DOM, so a name the host
 * accepts could not be typed or pasted (Codex review on #2281). The same gap
 * was closed for the company name by `clampToCompanyNameLimit`.
 */
export function clampToProviderNameLimit(label: string): string {
  // Counted on the trimmed name, because `checkProviderName`, the submit and the
  // host all trim first: spaces around a paste are not part of the name and must
  // not push its last characters out (Codex review on #2281).
  const name = Array.from(label.trim());
  if (name.length <= MAX_PROVIDER_NAME_CHARS) return label;
  const leading = label.slice(0, label.length - label.trimStart().length);
  return leading + name.slice(0, MAX_PROVIDER_NAME_CHARS).join("");
}

// ---- the model step: every provider asks for one, always (keys rework, ------
// ---- issue #2306, slices 2a/2b/2c/2d) ----------------------------------------
//
// D-model: a provider is a provider plus one chosen model, and there is no
// bare-tier passthrough left to fall back to (2d) — so unlike the old
// `needsModel`-gated ask, the model step is unconditional for every kind:
// cloud, local, custom and TinyHumans alike. No tier name is ever presented as
// a model (2d, D-no-tier).

/**
 * The workload tier names (`INFERENCE_TIERS` on the host), the one thing a
 * model id may never equal. Kept here rather than imported from `./routing`,
 * which is deleted with per-workload routing (phase 5b) — this list outlives
 * it, because `check_model_id` still refuses these on every write.
 */
/** Whether `id` contains a C0 or C1 control character (see `checkModelId`). */
function hasControlChar(id: string): boolean {
  for (const ch of id) {
    const code = ch.codePointAt(0) ?? 0;
    if ((code >= 0x00 && code <= 0x1f) || (code >= 0x7f && code <= 0x9f)) return true;
  }
  return false;
}

const TIER_NAMES: readonly string[] = ["chat-v1", "reasoning-v1", "agentic-v1", "vision-v1"];

/** The longest a model id may be, in Unicode code points. Mirrors `store::MAX_MODEL_ID_CHARS`. */
export const MAX_MODEL_ID_CHARS = 256;

/** Why a typed model id cannot be used. */
export type ModelIdError = "empty" | "control" | "whitespace" | "tooLong" | "tier";

/**
 * The console half of the host's `check_model_id` — same checks, same order,
 * counted in Unicode code points as the host counts with `chars()`. Never a
 * network check: a catalogue can be stale, and an Azure deployment name is
 * never in `/models` by design, so this validates shape only.
 */
export function checkModelId(raw: string): ModelIdError | null {
  const id = raw.trim();
  if (!id) return "empty";
  // Mirrors the host's `char::is_control`: C0 controls (0x00-0x1f, 0x7f)
  // and C1 controls (0x80-0x9f). Checked via code points rather than a
  // regex literal, which editor and tool pipelines have a way of mangling.
  if (hasControlChar(id)) return "control";
  if (/\s/.test(id)) return "whitespace";
  if ([...id].length > MAX_MODEL_ID_CHARS) return "tooLong";
  if (TIER_NAMES.includes(id)) return "tier";
  return null;
}

/** What to say about a model id {@link checkModelId} refused. */
export function modelIdErrorCopy(error: ModelIdError): string {
  switch (error) {
    case "empty":
      return "Choose a model.";
    case "control":
      return "A model id cannot contain control characters.";
    case "whitespace":
      return "A model id cannot contain spaces.";
    case "tooLong":
      return `A model id can be at most ${MAX_MODEL_ID_CHARS} characters.`;
    case "tier":
      return "That is a workload name, not a model. Choose a model id.";
  }
}

/** What the model step is fed: a list already read, or a reason there is none. */
export interface ModelAsk {
  /** The ids to offer, exactly as the endpoint published them. */
  models: string[];
  /** An endpoint whose `model` field keys on a deployment name `/models` never publishes. */
  freeTextOnly: boolean;
  /** Why the list is empty, when it is worth saying — never shown as a failure to save. */
  error?: string;
}

/**
 * Builds the model step's input from a draft probe's answer.
 *
 * The step **always opens** now (D-model): a failed or empty probe still opens
 * it, in free text, with the reason said rather than swallowed — never "add
 * anyway skips the model", because a provider is never shown as set without
 * one.
 */
export function modelAskFromProbe(
  url: string | null,
  probe: { ok: boolean; message?: string; models?: string[] } | null,
  isAzureEndpoint: (url: string | null | undefined) => boolean,
): ModelAsk {
  if (url === null) return { models: [], freeTextOnly: false };
  if (!probe || !probe.ok) {
    // The host's own sentence already ends with a period (or is a fragment
    // with none) — never assume either, or a doubled or missing full stop
    // shows up exactly where an operator is reading for the reason.
    const reason = (probe?.message ?? "no answer").trim().replace(/\.+$/, "");
    return {
      models: [],
      freeTextOnly: false,
      error: `Could not read this provider's models: ${reason}. Type a model id.`,
    };
  }
  return { models: probe.models ?? [], freeTextOnly: isAzureEndpoint(url) };
}

/** Whether the company's stored default names a provider but no model — Q1's bare-slug case. */
export function defaultNeedsModel(choice: DefaultChoice | null | undefined): boolean {
  return choice != null && choice.model == null;
}

/**
 * Whether this row is the company's **full** default: the slug matches and a
 * model is chosen. A resolved-but-unmarked default (nothing stored at all) is
 * never full — `provider.isDefault` alone answers "unrouted work resolves
 * here", which is a different question from "there is a full default to show".
 */
export function isFullDefault(
  provider: Pick<Provider, "slug">,
  choice: DefaultChoice | null | undefined,
): boolean {
  return choice != null && choice.model != null && choice.provider === provider.slug;
}

/** "Default · <model>" for a row holding the full default, else plain "Default". */
export function defaultBadgeLabel(
  provider: Pick<Provider, "slug">,
  choice: DefaultChoice | null | undefined,
): string {
  return isFullDefault(provider, choice) && choice ? `Default · ${choice.model}` : "Default";
}

/**
 * Whether a row's "Needs a model" chip should show: its own stored model is
 * ambiguous (two or more distinct ids, never guessed), or it is the row a
 * bare-slug default names.
 */
export function rowNeedsModel(
  provider: Pick<Provider, "slug" | "modelAmbiguous">,
  choice: DefaultChoice | null | undefined,
): boolean {
  return Boolean(provider.modelAmbiguous) || (defaultNeedsModel(choice) && choice?.provider === provider.slug);
}

/**
 * Whether saving `model` as `providerSlug`'s default would replace a
 * **different** stored default — the X4 confirm step's own gate (round-2
 * review, P2-1).
 *
 * Includes a bare-slug default naming a different provider: the earlier
 * condition required `current.model != null` across the *whole* expression,
 * which suppressed the confirm exactly where it mattered — replacing an
 * unfinished "provider chosen, no model" default with an entirely different
 * provider, silently. Completing *this same* row's own bare-slug default, or
 * saving the exact pair already stored, both still save directly — there is
 * nothing for a confirm to usefully name in either case.
 */
export function replacesDifferentDefault(
  current: DefaultChoice | null | undefined,
  providerSlug: string,
  model: string,
): boolean {
  return (
    current != null &&
    (current.provider !== providerSlug || (current.model != null && current.model !== model.trim()))
  );
}

/**
 * What to seed the "Set as default" model field with: the row's own model when
 * it has one, else the stored choice's model when the choice names this row
 * (the bare-slug case), else blank.
 */
export function defaultModelPrefill(
  provider: Pick<Provider, "slug" | "model">,
  choice: DefaultChoice | null | undefined,
): string {
  if (provider.model) return provider.model;
  if (choice?.provider === provider.slug && choice.model) return choice.model;
  return "";
}

// ---- shared copy (orchestrator decision X9, 2026-09-15) ---------------------
//
// Exact sentences, used verbatim wherever the console builds one of these
// states itself. Where the host's own refusal already carries this wording,
// its message is shown verbatim instead (`stripEnvelopePrefix`) — these exist
// for the states the console detects client-side, from a status read, before
// any request naming them is even sent.

/**
 * The console path every sentence below points to (decision D-copy / X9,
 * mirrored from the host's own `src/company/inference/copy.rs`, which is the
 * shared sentence table this file's copy must never drift from): the LLM
 * page, under the "API Keys" group of Connections
 * (`frontend/src/views/connection-pages.ts`: group `keys` is labelled "API
 * Keys"; the `inference` page in it is labelled "LLM" — verified against that
 * file, not guessed).
 */
export const SETTINGS_PATH = "Connections → API Keys → LLM";

/**
 * Whether `slug` names a provider this company can actually turn to: gone,
 * switched off, present-and-enabled but keyless, or fine. Mirrors the host's
 * `copy::ProviderGone` plus the separate `provider_has_no_key` case.
 */
export function providerState(
  slug: string,
  providers: readonly Pick<Provider, "slug" | "enabled" | "keyConfigured">[],
): "ok" | "removed" | "disabled" | "noKey" {
  const row = providers.find((p) => p.slug === slug);
  if (!row) return "removed";
  if (!row.enabled) return "disabled";
  return row.keyConfigured ? "ok" : "noKey";
}

/** X9: "Choose a model for {Provider} before saving." */
export function modelRequiredCopy(providerLabel: string): string {
  return `Choose a model for ${providerLabel} before saving.`;
}

/**
 * X9 / X14: the company default names a provider this company no longer has,
 * or has switched off, or `null` when the default is fine — including when it
 * is unset, or a bare slug with no model to be "broken" about (that is
 * {@link defaultNeedsModel}'s banner instead).
 *
 * Decision X14 (2026-09-15, confirmed over an earlier draft that carved out
 * an exception for deletes — see `docs/key-reworks/in-use-guards.md` §4):
 * disabling, deleting, or clearing the key of the default's provider never
 * clears the stored default, so this state is reachable and durable.
 *
 * Mirrors the host's `copy::default_broken`, which — like this — only covers
 * `removed` and `turned off`. A keyless-but-enabled default has no sentence
 * of its own on the host; its turns fail with `provider_has_no_key`, naming
 * whichever agent's turn hit it, which is not a fact this static banner can
 * show without one.
 */
export function defaultBrokenCopy(
  choice: DefaultChoice | null | undefined,
  providers: readonly Pick<Provider, "slug" | "label" | "enabled" | "keyConfigured">[],
): string | null {
  if (!choice || choice.model == null) return null;
  const state = providerState(choice.provider, providers);
  if (state === "ok" || state === "noKey") return null;
  const label = providers.find((p) => p.slug === choice.provider)?.label ?? choice.provider;
  return `The company default uses ${label}, which is ${state === "removed" ? "removed" : "turned off"}. Choose a new default in ${SETTINGS_PATH}.`;
}
