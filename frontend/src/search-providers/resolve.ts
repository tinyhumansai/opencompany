// The decisions the search-provider surfaces make, as plain functions over plain
// data.
//
// Nothing here renders and nothing here fetches. The rule this module exists to
// enforce is that a component is readable as *layout plus handlers*, with no
// branch in it that deserves a test of its own — which is where "should this row
// offer Remove key?" would otherwise end up, and rot.

import { CATALOGUE, endpointHost, optionDetail } from "./catalogue";
import type { CatalogueEntry } from "./catalogue";
import type { SearchProvider } from "./types";

/** The managed row's name. */
export const MANAGED_LABEL = "Managed";

/** The slug it is keyed on. `managed` is never a provider record. */
export const MANAGED_SLUG = "managed";

/**
 * The one sub-line the Managed row shows.
 *
 * It does **not** say "Always on". Managed search is always the *fallback*,
 * which is a different claim from always *working*: a deployment with no
 * platform search credential falls back to a surface that answers nothing, and
 * that badge would be the one claim on this page an operator most needs to be
 * true.
 */
export function managedSubline(
  inBuild: boolean,
  configured: boolean,
  dailyCap: number,
): string {
  if (!inBuild) return "This build has no search tools";
  if (!configured) return "No managed credential on this deployment";
  return `Metered — up to ${dailyCap} searches a day`;
}

/** Whether the Managed row may show its `On` badge. */
export function managedIsOn(inBuild: boolean, configured: boolean): boolean {
  return inBuild && configured;
}

/**
 * The one sub-line a connected row gets.
 *
 * One fact, chosen by what the row is: a self-hosted instance is identified by
 * where it answers, a keyed account by the fact that it holds a key, and one
 * that has not been given a key yet by saying so. Three facts stacked would be a
 * table, and an operator is scanning for the row rather than reading it.
 */
export function rowSubline(provider: SearchProvider): string {
  if (provider.takesEndpoint) {
    return endpointHost(provider.endpoint) || "no address";
  }
  return provider.keyConfigured ? "•••• configured" : "no key";
}

/** A control a row can offer. */
export type RowControl =
  | "toggle"
  | "test"
  | "replace-key"
  | "remove-key"
  | "make-default"
  | "edit-endpoint"
  | "remove";

/**
 * Which controls a row offers.
 *
 * Per kind, which is the brief's rule and the reason this is a function rather
 * than four conditions inside a `<DropdownMenuContent>`: **"Remove key" is not
 * offered where there is no key.** SearXNG has an address and no account, so
 * offering to remove its key would be offering to remove nothing.
 */
export function controlsFor(provider: SearchProvider): RowControl[] {
  const controls: RowControl[] = ["toggle", "test"];
  if (provider.takesKey) {
    controls.push("replace-key");
    // Only where there is one to remove.
    if (provider.keyConfigured) controls.push("remove-key");
  }
  if (provider.takesEndpoint) controls.push("edit-endpoint");
  // Offered only where it would change something: not on the row that is
  // already the default, not on one that is switched off and so cannot be the
  // active provider at all, and not on one with no key or address yet.
  //
  // That last condition is the one that was missing. `resolve::active` requires
  // the marked provider to be complete and otherwise falls through, so marking
  // an incomplete row changes nothing an agent can feel — while the console
  // answers with "Teammates now search through …". A control that reports a
  // routing change that did not happen is worse than one that is not offered.
  // Reachable straight after "Remove key", which leaves the row enabled.
  if (!provider.isDefault && provider.enabled && provider.complete) {
    controls.push("make-default");
  }
  controls.push("remove");
  return controls;
}

/** One option in the add dialog. */
export interface AddOption {
  value: string;
  label: string;
  detail: string;
}

/**
 * What the add dialog offers, by category.
 *
 * **Only what is not yet connected.** The page behind the modal shows the rest,
 * and offering to add something twice is how you get two rows for one provider.
 * Here that rule does a second job: the harness dispatches on the slug, so at
 * most one row per slug is not merely tidy, it is what keeps the set closed.
 */
export function addOptions(providers: readonly SearchProvider[]): {
  account: AddOption[];
  selfHosted: AddOption[];
} {
  const connected = new Set(providers.map((provider) => provider.slug));
  const available = CATALOGUE.filter((item) => !connected.has(item.slug));
  const toOption = (item: CatalogueEntry): AddOption => ({
    value: item.slug,
    label: item.label,
    detail: optionDetail(item),
  });
  return {
    account: available
      .filter((item) => item.category === "account")
      .map(toOption),
    selfHosted: available
      .filter((item) => item.category === "self-hosted")
      .map(toOption),
  };
}

/**
 * Whether this company has connected nothing of its own.
 *
 * **Not whether the list is empty** — it never is. The Managed row is rendered
 * whatever managed search resolves to, so there is always a row, and the
 * question left is the one the operator can act on: has anything been connected
 * beside it? When the answer is no, the list carries a notice and the CTA to
 * add the first provider.
 *
 * Managed deliberately does not count either way. It used to, and that made the
 * branch a statement about the *deployment's* credential rather than about this
 * company's records — which is how an e2e that pinned one branch came to fail on
 * a runner that happened to have no managed credential.
 */
export function hasNoProviders(providers: readonly SearchProvider[]): boolean {
  return providers.length === 0;
}
