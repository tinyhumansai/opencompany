# Data model

The record, where its credential lives, and the four places this deliberately
does **less** than the inference model it is shaped after.

## The record

```rust
/// One search account (or instance) this company has connected.
///
/// Derives no `Serialize`: there is no credential on this struct and there must
/// never be one. The wire shape is a separate DTO carrying `keyConfigured:
/// bool`.
pub struct SearchProvider {
    /// The catalogue slug — `brave`, `exa`, `querit`, `searxng`. Identity and
    /// address at once; see "No id, no label, no custom provider" below.
    pub slug: String,
    /// Whether this provider is eligible to be the one agents search through.
    /// Distinct from "not connected": a disabled provider keeps its credential.
    pub enabled: bool,
    /// The instance URL. `Some` for `searxng` and `None` for every account
    /// provider, because no account provider's base URL is configurable — see
    /// below.
    pub endpoint: Option<String>,
}
```

**There is no key on this record**, which is the single most important line here
and is the same line the inference data model opens with.

### No id, no label, no custom provider

Inference separates `id` (identity, survives a rename) from `slug` (address, what
a routing entry names) and lets an operator name a custom OpenAI-compatible
endpoint. None of that transfers, and forcing it would be symmetry for its own
sake:

1. **The harness dispatches on the slug.** `byo_search_tools` in
   `src/harness/built_in/search_byo.rs` is a `match config.provider.as_str()`
   over the four literals. A slug an operator chose would match nothing and wire
   no tools. Making the slug free-form therefore means adding a `kind` field and
   rewriting that match — paid for by a capability (two Brave accounts on one
   company) nobody has asked for.
2. **There is nothing to rename.** No routing entry names a search provider;
   there is one active provider and the harness asks for it by slug. Identity
   that survives a rename buys nothing when nothing renames.
3. **There is no generic search API.** Inference can offer "your own
   OpenAI-compatible endpoint" because that shape is a standard. Brave, Exa and
   Querit each speak a different REST shape, parsed by a different struct in
   `oh::search::tools`. A custom search provider would be a custom parser, not a
   custom URL. **SearXNG is the self-hosted escape hatch**, and it is the only
   one that can exist.

So: one row per catalogue slug, at most. The add list offers only what is not yet
connected, which is the inference rule anyway, and it is now also what keeps the
set closed.

### Only SearXNG has an endpoint

Read off the vendored code that makes the calls, not off the provider's docs:

| Provider | Base URL in the call | Configurable? |
|---|---|---|
| `brave` | `const DEFAULT_API_BASE = "https://api.search.brave.com/res/v1"` | **no** — `BraveWebSearchTool::new` takes no URL at all |
| `exa` | `const DEFAULT_API_URL = "https://api.exa.ai"` | in principle — but `search_byo` passes `None` |
| `querit` | `const DEFAULT_API_URL = "https://api.querit.ai/v1"` | in principle — `search_byo` passes `None` |
| `searxng` | the operator's instance | **yes, and required** |

Offering an endpoint field for Brave would be a field the request ignores. The
catalogue carries each provider's endpoint as a **display and probe** value, and
only `searxng` has a writable one.

## Storage

```
   SearchProvider records                SecretStore (per CompanyId)
   ┌────────────────────┐                ┌──────────────────────────────────┐
   │ slug               │                │ search/providers        index    │
   │ enabled            │ ──names──▶     │ search/provider/<slug>/key       │
   │ endpoint           │                │ search/provider/<slug>/endpoint  │
   │                    │                │ search/default          one slug │
   │   NO KEY FIELD     │                │ ──────────────────────────────── │
   └────────────────────┘                │ search/provider   entry zero     │
                                         │ search/api_key    entry zero     │
                                         │ search/endpoint   entry zero     │
                                         └──────────────────────────────────┘
```

The credential rules are the inference ones verbatim, because they are right and
because three of the four are already satisfied by the code being replaced:

1. **Never a field on a serializable record.** No `Serialize`, private field,
   redacting `Debug` — `TenantSearch` already does exactly this and is the model.
2. **Never returned by any route.** `keyConfigured` is derived by asking the
   store whether a non-empty value exists, never stored as a flag that could go
   stale against a cleared secret.
3. **Read per request, not captured at boot.** Already true: `TenantSearch`
   resolves per roster build, and its `fingerprint` covers the key so a rotation
   rebuilds the roster instead of authenticating with the old one until restart.
4. **One credential slot per provider slug, with no exception.** This is the
   change, and [`current-state.md`](current-state.md) is the bug it fixes.

## Convergence, not migration

The `SecretStore` port has **no rename and no delete** — clearing is a write of
the empty string that reads back as unset. So the provider list cannot take over
`search/provider`, and a flag-day migration on a store with no transaction can
leave a company with neither configuration.

The inference plan's option (b), which applies here with less work because entry
zero is a slug rather than a config blob:

```
read providers(company):
    legacy = read("search/provider")                  ← may be absent or "managed"
    if legacy is a BYO slug and no record exists for it:
        yield SearchProvider {
            slug:     legacy,
            enabled:  true,
            endpoint: read("search/endpoint"),         ← searxng only
        }                                              key at "search/api_key"
    for slug in index("search/providers"):
        yield SearchProvider::from_scoped(slug)
```

- **A write always goes to the new address and clears the old in the same
  operation.** Saving a key for `exa` writes `search/provider/exa/key` and
  clears `search/api_key`. A key left at the old address after the new one is
  written is an orphaned secret, so the clear must be *issued* and a failure
  logged loudly — it cannot be inferred, because "cleared" and "never set" are
  the same state in this store.
- **A read tries the new address and falls back to the old one.** An existing
  company keeps working untouched; its first save moves it.
- The cost is one special case in the reader, kept until nothing reads the flat
  keys. That is cheaper than a migration, and the change is additive for every
  running tenant.

### The ordering hazard this creates

`TenantSearch::resolve` in the harness reads the three flat keys directly, and so
do `company::search::resolve_effective_provider` and the capabilities panel. The
moment the console writes a credential to `search/provider/<slug>/key` and
clears `search/api_key`, **any reader still on the flat keys sees an unconfigured
company and silently falls back to managed search.** Every agent keeps searching;
it just quietly stops using the account the operator pays for.

So the store, the harness resolver and the capabilities reader move in the same
commit, through **one** resolution function. This is the same "exactly one
derivation" argument `effective_provider`'s doc comment already makes — it just
now has to be obeyed across a feature gate.

### No stored health map

The inference design records what the system last learnt about reaching each
provider, sourced from things that already happen. **That is not stored here.**
A row's health is whatever this console session learnt from the add-time probe or
a manual Test, and it is gone on reload.

The reason is the one that makes search different: every check of an account
provider costs a real, billed query. Inference can afford to re-learn health
cheaply because its probe is a free `GET /models`; persisting a health map here
would invite exactly the background refresh that would spend the company's money
to keep it current. A row that says nothing until somebody asks is the honest
shape for a check that is not free.

## The default marker means something different here

One slot, `search/default`, holding one slug. A marker rather than a flag per
record, for the inference reason: two flags can both be true and then need a
tie-break rule; a slot cannot, so there is no rule to write and no state to
reconcile.

What differs is what it *means*, and the docs should not pretend otherwise:

| | LLM | Search |
|---|---|---|
| Default provider | what an **unset workload** uses | the **only** provider agents search through |
| Non-default connected providers | serve the workloads routed to them | serve nothing; they are stored credentials, ready |

The cause is one line in the harness: every provider's canonical web search is
aliased to the single tool name `web_search`, because the shipped research skills
name that tool in their instructions and a belt where the name comes and goes
with a settings change is how an agent starts inventing URLs instead of
searching. Two simultaneously active search providers would need two names, and
that is a different feature.

So the row badge stays the word **Default** — an operator arriving from the LLM
page should read the same word for the same marker — and the card carries one
sub-line saying agents search through it. Resolution is the inference rule
unchanged:

```
active(providers, marked) =
    marked, when it exists and is enabled
    else the first enabled provider
    else None → managed
```

An unmarked company therefore resolves exactly as it did before, and nothing is
backfilled. Both write paths keep the marker honest: disabling the marked
provider clears the marker rather than moving it to something the operator never
chose, and deleting it clears the marker in the same operation that clears its
key.

## Managed is a row, and it says what is true

Managed is not a record and cannot be one — it has no company credential, it is
the platform's metered surface. It is rendered as a row keyed on **whether it
resolves**, the same rule the inference Managed row uses, answered from facts the
host already computes:

| State | Badge | Sub-line |
|---|---|---|
| harness in build, platform credential resolves from env | `On` | `Metered — up to N searches a day` |
| harness in build, no platform credential on this deployment | *(none)* | `No managed credential on this deployment` |
| build has no agent harness | *(none)* | `This build has no search tools` |

`search_credential_configured()` in `src/server/ops/capabilities.rs` is the
existing derivation (`search_backend_from_env`, `false` when the `openhuman`
feature is off) and the status route calls it rather than restating it.

**No "Always on" badge.** Managed search is always the *fallback*, which is a
different claim from always *working*: a self-hosted deployment with no platform
search credential falls back to a surface that answers nothing. The badge would
be the one claim on the page an operator most needs to be true.

**No toggle and no Remove on this row.** Managed cannot be switched off — it is
what the absence of everything else means — and a control that does nothing is
worse than no control.

## What is deliberately not carried over

- **Tier vocabularies and per-workload routing.** There is one workload.
- **`ProviderId`, `slugify`, `check_slug`, collision errors.** The slug set is
  closed and comes from the catalogue.
- **A model catalogue and its per-company cache.** There are no models to list,
  so the cache-key incident the inference model records has no analogue.
- **`base_url` as a writable field on account providers.** See above.
