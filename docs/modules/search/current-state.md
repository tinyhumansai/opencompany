# Current state, and the bug that motivates the rework

What `Connections → Search` is today, read off `upstream/main` rather than
remembered, and the one defect that makes a provider list worth building rather
than merely tidier.

## What exists

Three flat secrets on the company, declared in `src/company/search.rs`:

```
search/provider   the chosen slug — one of SUPPORTED_PROVIDERS
search/api_key    the BYO key                    (write-only, never echoed)
search/endpoint   the instance URL, SearXNG only (not a secret)
```

`SUPPORTED_PROVIDERS = ["managed", "brave", "exa", "querit", "searxng"]`.

Four routes' worth of surface in `src/server/ops/search.rs`:

| Route | Authority | What it does |
|---|---|---|
| `GET …/search` | `ScopedCompany` | returns `SearchStatus` — slugs and booleans, never the key |
| `PUT …/search` | `AdminScopedCompany` | writes any supplied field, rolls back on partial failure |
| `DELETE …/search/key` | `AdminScopedCompany` | clears **all three** keys, falling back to managed |

One console page, `frontend/src/views/SearchView.tsx`: a `Select` over five
slugs, an API-key input, an endpoint input, Save, and Remove key.

One harness seam, `src/harness/built_in/search_byo.rs`, gated on the `openhuman`
feature. `TenantSearch::resolve` reads the three flat keys, and
`byo_search_tools` matches on the provider slug to construct OpenHuman's own
tool structs, presenting each provider's canonical web search to the model under
the single alias `web_search`.

## The rules that must survive

Both are already written down in the code, and both constrain the rework more
than the UI does.

**The key is per company and never from the environment.** The module header on
`src/company/search.rs` states it: a BYO search key is billed to whoever pasted
it, so an environment fallback would let one company's searches ride on a
credential somebody else pays for. With nothing stored the company falls back to
`managed`, which is metered and daily-capped against the platform.

**The configuration surface is not feature-gated; the harness is.** `search_byo`
is behind `openhuman`; `src/company/search.rs` and `src/server/ops/search.rs`
are always compiled, so a build without the agent harness renders "this build
has no search tools" rather than a 404. `SearchStatus::in_build` is the field
that carries it.

## The defect: one key slot, five providers

There is exactly one `search/api_key` for the whole company, and the provider
slug is a separate field that can be changed without it.

```
   stored:  search/provider = "exa"      search/api_key = <an Exa key>

   operator switches the Select to Brave and presses Save, typing no key
   (the placeholder says a stored key is kept, and it is)

   stored:  search/provider = "brave"    search/api_key = <the same Exa key>
```

Every layer downstream then agrees the company is correctly configured:

- `configuration_complete("brave", has_key = true, …)` → `true`
- `effective_provider(…)` → `"brave"`, so the page shows a connected Brave badge
- `TenantSearch::resolve` returns `Some`, so the harness wires Brave's tools
- `BraveWebSearchTool` presents the Exa key as `X-Subscription-Token`

The first agent that searches gets a 401 from Brave. Nothing in the console says
so, because nothing between the Save and the turn ever presents the credential.
This is the same failure the inference rework names — *"switching provider
without re-entering a key failed on the next turn with a 401 the host had to
explain in a paragraph"* — and it has the same cause: **one credential slot for
a field that selects between several accounts.**

One credential per provider slug fixes it by construction, and it is the reason
a list is worth building rather than merely a nicer form. The list is not there
so a company can search through two indexes at once — [`architecture.md`] shows
why it cannot — it is there so the credential and the thing it authenticates
cannot come apart.

## What the current page gets right and keeps

- **`provider` and `effectiveProvider` are two fields.** "I picked Exa and pasted
  no key" must not read like "I picked Exa". The new list keeps the distinction
  per row rather than per page.
- **Not-configured is a working state, not an error.** Managed search answers.
- **Three things can each be missing and they fail differently** — no key, no
  `search` grant in the manifest, no harness in the build — and the status
  reports them separately because the remedies are on three different pages.
- **The rollback in `write_all`.** A store that took the key and then failed on
  the provider would leave a company searching one index with another's key. The
  per-provider layout makes that particular half-write unrepresentable, but the
  rollback discipline carries to the credential/index pair.
