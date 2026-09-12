// The console's mirror of `src/company/search/catalogue.rs`.
//
// Data only. A test asserts this list and the Rust one agree slug for slug — the
// inference catalogue's fourth known defect is a hand-duplicated provider table
// with nothing noticing when the copies drift, and this one is small enough that
// there is no excuse for repeating it.
//
// There is deliberately **no key-prefix or key-placeholder pattern**. None of
// the three account providers documents one, and a validation rule invented from
// a blog post rejects a valid key for a reason the operator cannot see.

import type { ProviderCategory } from "./types";

/** One catalogue entry, as the add dialog needs it. */
export interface CatalogueEntry {
  slug: string;
  label: string;
  category: ProviderCategory;
  /** The API host, for the dropdown's detail line. Empty for self-hosted. */
  endpoint: string;
  /** Where an operator gets a key, or `null` where there is no key. */
  keySource: string | null;
  /** What the connect dialog says under the key field. */
  keyHint: string | null;
}

/** Every provider that can appear as a row. */
export const CATALOGUE: readonly CatalogueEntry[] = [
  {
    slug: "brave",
    label: "Brave Search",
    category: "account",
    endpoint: "api.search.brave.com",
    keySource: "https://api-dashboard.search.brave.com/app/keys",
    keyHint: "Create one at api-dashboard.search.brave.com",
  },
  {
    slug: "exa",
    label: "Exa",
    category: "account",
    endpoint: "api.exa.ai",
    keySource: "https://dashboard.exa.ai/api-keys",
    keyHint: "Create one at dashboard.exa.ai",
  },
  {
    slug: "querit",
    label: "Querit",
    category: "account",
    // Querit's docs say to copy the key "from the Dashboard" and never print a
    // URL for it. Linking a guess is worse than linking nothing.
    endpoint: "api.querit.ai",
    keySource: "https://www.querit.ai/",
    keyHint: "Create one in your Querit dashboard",
  },
  {
    slug: "searxng",
    label: "SearXNG",
    category: "self-hosted",
    endpoint: "",
    keySource: null,
    keyHint: null,
  },
] as const;

/** The catalogue entry for `slug`, if there is one. */
export function entry(slug: string): CatalogueEntry | undefined {
  return CATALOGUE.find((item) => item.slug === slug);
}

/** The display name for a slug, falling back to the slug itself. */
export function labelOf(slug: string): string {
  return entry(slug)?.label ?? slug;
}

/**
 * The copy the add dialog and the connect dialogs use.
 *
 * Collected here rather than inline so the wording is greppable and so a
 * component stays layout plus handlers.
 */
export const COPY = {
  groupAccount: "Search accounts",
  placeholderAccount: "Choose a search provider…",
  helperAccount: "Hosted search APIs. You supply an API key.",

  groupSelfHosted: "Self-hosted",
  placeholderSelfHosted: "Choose a self-hosted instance…",
  helperSelfHosted:
    "Your own SearXNG instance. No account, no key — just the address it answers on.",

  everythingConnected: "Everything here is already connected.",

  /**
   * Said on the button it applies to, not above the fold.
   *
   * No hosted search provider publishes a free credential validator — checked
   * against all three providers' own docs. All three reject a bad key before
   * running a search, so a failed check is free; confirming a good one costs one
   * real query. SearXNG is free, and is the only one that is.
   */
  checkCosts: "Checking costs one search.",

  /**
   * SearXNG ships `search.formats: [html]` and aborts with 403 when a format is
   * not enabled, so this is said *before* the operator submits as well as after
   * it fails.
   */
  searxngFormats:
    "JSON output must be enabled — add `json` to `search.formats` in settings.yml.",

  /** The only explanatory sentence that survives on the page. */
  defaultIsTheOnlyOne: "Your teammates search through the default provider.",

  /**
   * The dead end, said rather than implied.
   *
   * Reachable only with no provider connected *and* nothing behind the Managed
   * row — at which point no teammate can search at all, which is a stronger
   * statement than "not connected yet" and is the one that makes Add the
   * obvious next step. The Managed row says *why* it does not resolve; this
   * says what that costs.
   */
  noSearchAnswers: "No search answers for this company yet.",
} as const;

/** The detail line an add-dialog option shows. */
export function optionDetail(item: CatalogueEntry): string {
  return item.category === "self-hosted"
    ? "Runs on your own network"
    : item.endpoint;
}

/** The host part of an instance URL, for a row's sub-line. */
export function endpointHost(url: string | null): string {
  if (!url) return "";
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}
