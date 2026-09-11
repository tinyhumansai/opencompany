// Tier overrides the platform's subscription proxy would reject.
//
// Lifted out of the single-provider form it used to live in, unchanged. It is
// **nine** recorded regressions deep (issue #1838 and its follow-ups), six of
// which are about stripping a value mid-keystroke, so it moves as a unit with
// its reasoning attached rather than being rewritten beside the new model field.
//
// Where it applies is narrower than it looks: only a config riding the platform
// proxy. A tenant's own OpenRouter account, a custom endpoint or a local runtime
// takes whatever id the operator types, verbatim — so a caller has to know
// whether the provider it is editing is proxied before it strips anything.

/** The abstract tiers this runtime has. A bare tier name is an accepted value. */
const TIERS = ["chat-v1", "reasoning-v1", "agentic-v1", "vision-v1"] as const;

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
export function isProxyCompatible(value: string): boolean {
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
export function stripProxyIncompatible<T extends string>(
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
 * The slug the platform's own subscription proxy answers to.
 *
 * Mirrors the host's `MANAGED_SLUG`. It is the one provider whose model field
 * the rules above apply to: every other provider is a vendor the operator has
 * an account with, and a typed id goes to it verbatim.
 */
export const PROXIED_SLUG = "tinyhumans";

/**
 * Whether a typed override may be sent to `slug`'s endpoint as it stands.
 *
 * `true` for every provider that is not the platform proxy — not because those
 * ids are checked and found good, but because there is nothing to check them
 * against: a vendor's own catalog is the vendor's business, and an id the
 * operator typed is honoured verbatim there.
 *
 * **Asked on save, never on keystroke.** Six of this rule's nine recorded
 * regressions are about stripping a value while it is still being typed; a
 * settled value is the only kind worth judging.
 */
export function overrideIsSendable(slug: string, value: string): boolean {
  if (slug !== PROXIED_SLUG) return true;
  return !value.trim() || isProxyCompatible(value);
}
