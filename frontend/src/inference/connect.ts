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
import type { ManagedState } from "@/api/inference";
import type { Provider } from "./types";

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
  id: "edit" | "default" | "replaceKey" | "removeKey" | "remove";
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
  provider: Pick<Provider, "kind" | "enabled" | "keyConfigured"> & { isDefault?: boolean },
): ProviderRowAction[] {
  const ask = credentialAsk(provider.kind);
  const actions: ProviderRowAction[] = [
    { id: "edit", label: ask.needsEndpoint ? "Edit endpoint" : "Edit" },
  ];
  if (!provider.isDefault && provider.enabled) {
    actions.push({ id: "default", label: "Set as default" });
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

/** The slug the managed tier is offered and keyed under. */
export const MANAGED_OPTION_SLUG = "tinyhumans";

/**
 * Whether the managed tier belongs in the Cloud list.
 *
 * The list rule is "only what is not yet connected", and it is applied here
 * without an exception — but **managed resolves through a chain rather than a
 * record**, so "connected" is a question about resolution, not about a row
 * existing. A hosted tenant has a working managed provider nobody ever added.
 *
 * ## The one place the simple rule does not settle the product question
 *
 * At step 4 the chain resolves against the **instance's** identity: it works,
 * and the server is paying. On the plain rule that is "connected", so the entry
 * would disappear — and with it the only route from *the server pays* to *we
 * pay*, which is a decision an operator actively wants to make.
 *
 * So it stays listed at step 4. The cost is that one entry can appear in the
 * list while a row for it is also on the page; the benefit is that a capability
 * does not vanish. The Connected row carries the sentence that explains it
 * ("Billed to whoever runs this server"), so the list itself stays uniform.
 *
 * Steps 1-3 are the company's own credential in one form or another, and there
 * is nothing left to upgrade to — so it is hidden, exactly like OpenRouter once
 * you have added it.
 */
export function offersManaged(managed: ManagedState | undefined): boolean {
  if (!managed) return false;
  return managed.source === "none" || managed.source === "instance";
}

/**
 * What each category offers, minus what is already connected.
 *
 * **Each category lists only what is not yet connected.** The page behind the
 * modal shows the rest, and offering to add something twice is how you get two
 * rows for one provider.
 *
 * The detail lines differ per category because the categories ask three
 * different questions: a cloud provider is identified by the host its key goes
 * to, a local runtime by the fact that it is local, and a CLI login by whose
 * credential it borrows.
 */
export function addOptions(
  providers: readonly Provider[],
  managed?: ManagedState,
): AddOptions {
  const managedEntry: AddOption[] = offersManaged(managed)
    ? [
        {
          value: MANAGED_OPTION_SLUG,
          label: "Managed (TinyHumans)",
          // The endpoint host, like every other cloud row. The reason it is
          // still offered belongs on the Connected row, not in this list.
          detail: endpointHost(managed?.baseUrl),
        },
      ]
    : [];
  return {
    cloud: managedEntry.concat(
      CLOUD_PROVIDERS.filter((p) => !isConnected(providers, p.slug)).map((p) => ({
        value: p.slug,
        label: p.label,
        // The host, not the whole URL: the path is noise at a glance and the
        // host is the part an operator recognises.
        detail: endpointHost(p.endpoint),
      })),
    ),
    local: LOCAL_RUNTIMES.filter((r) => !isConnected(providers, r.slug)).map((r) => ({
      value: r.slug,
      label: r.label,
      detail: COPY.detailLocal,
    })),
    cli: CLI_LOGINS.filter((c) => !isConnected(providers, c.optionSlug)).map((c) => ({
      value: c.optionSlug,
      label: c.label,
      detail: COPY.detailCli,
    })),
  };
}

/**
 * Whether a CLI login can be reached from this host at all.
 *
 * **False, and it is a fact about the product rather than a build flag.** A CLI
 * login is a credential sitting in a dotfile on the machine a person is typing
 * on; openhuman is a desktop app, where that is the same machine as the one
 * running the model call. OpenCompany is a server-side product, so nothing here
 * holds one and the host refuses the kind outright.
 *
 * The group is still **rendered, saying so**, rather than hidden: the shape is
 * then already right if a delegated credential ever becomes available, and an
 * empty labelled group is more honest than a missing one. `addOptions` still
 * filters the category properly for that day — including the Codex trap.
 */
export const CLI_LOGINS_REACHABLE = false;

/** What the group says when there is nothing in it. */
export const CLI_LOGINS_UNAVAILABLE = "Not available on this host.";

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
 * What connecting `optionSlug` asks the operator for.
 *
 * The three categories are three different questions, and this is the function
 * that says so: **cloud wants a key** (its endpoint is a preset, and the paths in
 * that table are too varied to be typed), **local wants an endpoint** (that is
 * the thing being chosen), and **a CLI login wants nothing** because another tool
 * already holds the credential. `omlx` is the one row that wants both.
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
  if (optionSlug === MANAGED_OPTION_SLUG) {
    // Its own shape: the endpoint is the platform's and is not typed, and the
    // *other* way to set it up is an account link rather than a key — which the
    // dialog offers beside the field rather than duplicating here.
    return {
      title: "Connect TinyHumans",
      needsKey: true,
      needsEndpoint: false,
      keyPlaceholder: "th-...",
    };
  }
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
  const trimmed = raw.trim();
  const split = trimmed.indexOf("://");
  const start = split === -1 ? 0 : split + 3;
  const rest = trimmed.slice(start);
  const end = rest.search(/[/?#]/);
  const authority = end === -1 ? rest : rest.slice(0, end);
  return authority.includes("@");
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
