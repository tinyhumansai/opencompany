// The shapes the search-provider surfaces pass around.
//
// Types only — no values, no behaviour. Kept apart from `resolve.ts` and
// `classify.ts` so those stay readable as *decisions*, which is the whole reason
// they are separate from the components that render them.
//
// The credential rule, restated here because this is where someone would add a
// field to one of these: **no shape in this file carries a key.** The wire
// carries `keyConfigured: boolean` and nothing else.

/** Which of the two questions a provider answers. */
export type ProviderCategory = "account" | "self-hosted";

/**
 * What a failed check means.
 *
 * `format` has no counterpart on the LLM page. SearXNG ships with JSON output
 * turned off and answers `403` when a format is not enabled, which makes it the
 * most likely failure when connecting a perfectly healthy instance — and it
 * involves no credential at all.
 */
export type ProbeClass =
  "auth" | "format" | "quota" | "endpoint" | "timeout" | "unknown";

/**
 * One search account (or instance) this company has connected.
 *
 * There is no `id` separate from `slug`, and that is deliberate rather than an
 * omission: the harness dispatches on the slug, so it comes from the catalogue
 * and an operator never names one. See `docs/modules/search/known-defects.md`.
 */
export interface SearchProvider {
  /** The catalogue slug. Identity and address at once. */
  slug: string;
  /** Display name. */
  label: string;
  /** Which of the two questions it answers. */
  category: ProviderCategory;
  /** Whether it is eligible to be the provider agents search through. */
  enabled: boolean;
  /** Whether a credential is stored — **never the credential**. */
  keyConfigured: boolean;
  /**
   * Whether this kind of provider takes a key at all.
   *
   * `false` for SearXNG, and it is what keeps "Remove key" off a row that has
   * no key to remove.
   */
  takesKey: boolean;
  /** Whether this kind of provider takes an instance address. */
  takesEndpoint: boolean;
  /** The instance address, for a self-hosted provider. Not a secret. */
  endpoint: string | null;
  /** Whether it has everything it needs to answer a search. */
  complete: boolean;
  /**
   * Whether this is the provider agents search through.
   *
   * The **resolved** answer rather than the raw marker, so a row can say
   * "Default" whether it was chosen or inherited — the operator sees the same
   * answer either way.
   */
  isDefault: boolean;
}

/** What a connect or test attempt came back with. */
export interface ConnectOutcome {
  ok: boolean;
  probeClass: ProbeClass | null;
  /** One sentence. **Never** carries the upstream error body. */
  message: string | null;
  /** Whether the record and the credential were kept. */
  saved: boolean;
}

/**
 * A destructive action waiting on a confirmation.
 *
 * All three are irreversible in the only sense that matters on this page: a key
 * is write-only and is never shown back, so an operator who clears the wrong one
 * has nothing on screen to retype.
 */
export type ConfirmTarget =
  | { kind: "remove"; slug: string; label: string }
  | { kind: "remove-key"; slug: string; label: string }
  | { kind: "disconnect-all"; label: string };
