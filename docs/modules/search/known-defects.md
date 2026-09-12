# What is not inherited, and why

This rework is shaped after the inference provider surface
(`docs/modules/inference/`). That design is good and most of it transfers
verbatim. This file records the parts that do **not**, so the next reader does
not "restore consistency" by adding them back.

The rule behind every entry: consistency is the goal, symmetry is not. An
operator who has used the LLM page should find Search obvious — which is a claim
about the page, not about the record behind it.

## 1. Not inherited: `id` separate from `slug`

**There.** `ProviderId` is opaque identity; `slug` is the address a routing entry
names. Two fields because a rename must not break routing.

**Here.** One field. The harness dispatches on the slug
(`match config.provider.as_str()` in `search_byo.rs`), so the slug is fixed by
the catalogue and there is nothing to rename. Adding an id would add a second
key to keep in step in exchange for a capability nothing uses.

## 2. Not inherited: custom providers, `slugify`, collision errors

**There.** An operator can name their own OpenAI-compatible endpoint, so labels
are free-form, slugs are derived, and collisions are a typed error.

**Here.** There is no generic search API to be custom against — Brave, Exa and
Querit each speak a different REST shape parsed by a different struct. A custom
search provider would be a custom *parser*, not a custom URL. SearXNG is the
self-hosted escape hatch and the only one there can be.

Consequence: at most one row per catalogue slug, so the add dialog's
"only what is not yet connected" rule also keeps the set closed.

## 3. Not inherited: an editable base URL on account providers

**There.** Every cloud provider has a `base_url` an operator can override.

**Here.** Brave's base URL is a `const` in the vendored tool and
`BraveWebSearchTool::new` takes no URL at all; Exa's and Querit's constructors
accept one but `search_byo` passes `None`. A base-URL field on those rows would
be a field the request ignores — the worst kind, because it looks like it worked.

Only SearXNG has a writable endpoint, and for it the endpoint is the whole record.

## 4. Not inherited: per-workload routing, and what "Default" means

**There.** Several providers are live at once; the default serves *unset*
workloads.

**Here.** One provider is live. Every provider's canonical web search is aliased
to the single tool name `web_search`, because the shipped research skills name
that tool in their instructions and a belt where the name appears and disappears
with a settings change is how an agent starts inventing URLs instead of
searching. Two simultaneously active search providers would need two names.

The marker, the storage and the fall-through rule are inherited unchanged. The
*meaning* is stronger: the default is the only one used. The page carries one
sub-line saying so, which is the only explanatory sentence that survives the
deletion pass.

## 5. Not inherited: one classifier over an error string

**This is the one that would have been a bug.**

**There.** `classify(err: &str)` reads the upstream error text in a fixed branch
order — proxy first, then `401`-or-`403`-with-credential-wording, then model,
endpoint, timeout. It works because every OpenAI-compatible endpoint signals a
rejected key with `401`.

**Here.** Brave signals a rejected key with **`422`** and
`error.code == "SUBSCRIPTION_TOKEN_INVALID"`; its API reference documents 200,
404, 422 and 429 and **no 401 and no 403 at all**. Ported unchanged, the
classifier would:

- never fire its destructive branch for Brave, leaving a key we have positive
  evidence is dead stored under an amber "the check did not complete"; and
- read a genuine Brave `403` — which can only be a WAF, since Brave never uses
  403 for auth — as a rejected key, which is the exact incident the inference
  branch ordering exists to prevent, arriving through the other door.

So `classify` takes the slug, the status and the body. The generic branches stay,
in the inference order, for everything the per-provider rules do not claim.

## 6. Not inherited: "add anyway"

**There.** A probe failure unlocks an "add anyway" button, because a provider
that does not serve an OpenAI-shaped `/models` listing is still usable — the
probe can fail on a working provider.

**Here.** The probe *is* the provider's own search call. A probe that fails for
anything but a transport reason is a provider that will not answer a turn either,
so "add anyway" would be offering to store a credential we have just watched
fail. The non-auth classes already keep the record and the credential, which
covers every case "add anyway" existed for.

## 7. Not inherited: a model catalogue, its cache, and the tier vocabularies

There are no models to list, so there is no per-company-per-endpoint catalog
cache — and therefore no analogue of the incident where one harness's
entitlement-scoped catalog was served to another for an hour.

## 8. Newly needed: a `format` probe class

Nothing in the inference design corresponds to it. SearXNG ships
`search.formats: [html]` and aborts with `403` when a format is not enabled, so
the most likely failure when connecting a perfectly healthy instance is a `403`
meaning *"add `json` to `search.formats` in settings.yml"*. There is no
credential involved at all. Classifying it as `auth` would try to delete a key
that does not exist; classifying it as `unknown` would throw away the one message
that could have fixed it.

## 9. Newly needed: the probe costs money

Inference probes with a free `GET /models`. No hosted search provider offers a
free credential validator — checked against all three providers' own docs. All
three reject a bad key *before* running a search, so a failed check is free, but
confirming a good one costs a real query (~$0.005 Brave, ~$0.007 Exa, 1 Querit
credit).

So: the connect dialog says "Checking costs one search" on the button it applies
to, following the precedent the inference routing dialog set with its cost
warning; SearXNG's dialog says nothing, because `GET /config` is genuinely free;
and there is no background health poller, which the inference design had already
refused for its own reasons and which here would also spend the company's money.

## 10. Newly needed: a private-network allowance in the address guard

A SearXNG instance is legitimately at `10.0.0.5`. A public-content fetcher can
refuse private ranges; this cannot. The allowance is explicit and narrow —
link-local and metadata addresses still refused, re-checked after DNS — and it is
why the probe route is `AdminScopedCompany` here where inference allows
`ScopedCompany`.

The guard itself is **not** rewritten: `guard_link` in
`src/server/ops/memory_ingest.rs` already does the resolving check, and a second
copy of a rule like that drifts. It is lifted and called.

## Inherited without change

Worth stating, so the borrowing is not mistaken for wholesale rejection: the
credential rules, the entry-zero convergence, the default-as-a-marker argument,
the "only `auth` is destructive" discipline, the classify/describe split, the
never-interpolate-the-upstream-string rule, the add dialog's categories-are-
different-questions reasoning, the row shape, the silence of a healthy row, the
monogram instead of a trademark, and the extend-the-leak-test-with-every-route
rule are all taken verbatim.
