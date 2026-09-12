# The provider catalogue

Four providers, one row each. Every fact below was read from either the vendored
OpenHuman tool that **makes the call** or the provider's own API documentation,
and the two are named separately wherever they could disagree.

The inference rework's fourth known defect is that its preset catalogue is
duplicated by hand across the language boundary with nothing testing that the
copies agree. This catalogue is small enough that the answer is easy: one table
in `src/company/search/catalogue.rs`, one mirror in
`frontend/src/search-providers/catalogue.ts`, and a test that fails when they
diverge.

## The authority question

For endpoints and auth headers the **vendored tool is authoritative, not the
provider's docs**, because the vendored tool is what runs. Where the docs offer
a second accepted form, the catalogue records what `oh::search::tools` actually
sends:

| Provider | Vendored call site | Docs also allow |
|---|---|---|
| `brave` | `X-Subscription-Token` header | — |
| `exa` | `x-api-key` header | `Authorization: Bearer` |
| `querit` | `Authorization: Bearer` (`.bearer_auth`) | — |
| `searxng` | no auth of any kind | — |

## The table

### `brave` — Brave Search

| | |
|---|---|
| Category | Search account |
| Endpoint | `https://api.search.brave.com/res/v1` + `/web/search` |
| Configurable endpoint | **No** — `DEFAULT_API_BASE` is a `const` and `BraveWebSearchTool::new` takes no URL |
| Auth | `X-Subscription-Token: <key>` |
| Key shape | **No documented prefix.** Brave's docs show only `<YOUR_API_KEY>`; the commonly repeated `BSA…` prefix appears nowhere in them, so nothing validates on shape |
| Key from | `api-dashboard.search.brave.com/app/keys` |
| API shape | REST `GET`, query params, JSON response. Not OpenAI-shaped |
| Auth failure | **`422`**, body `error.code == "SUBSCRIPTION_TOKEN_INVALID"`. Brave's reference documents 200/404/422/429 and **no 401 or 403** |
| Extra tools | `brave_news_search`, `brave_image_search`, `brave_video_search` |
| Cost note | ~$5 per 1,000 requests, with $5 of free credit a month; a card is required |

Brave's 422 is the single most consequential fact in this file; see
[`connect-flow.md`](connect-flow.md).

### `exa` — Exa

| | |
|---|---|
| Category | Search account |
| Endpoint | `https://api.exa.ai` + `/search` |
| Configurable endpoint | In principle (`ExaSearchTool::new` takes an `api_url`), but `search_byo` passes `None`, so **no** |
| Auth | `x-api-key: <key>` |
| Key shape | No documented prefix |
| Key from | `dashboard.exa.ai/api-keys` |
| API shape | `POST` JSON — `{"query": …, "numResults": …}`. Not OpenAI-shaped |
| Auth failure | `401`, body `tag == "INVALID_API_KEY"`. A **`402`** means *either* no credential at all *or* out of credit, so the body decides, not the status |
| Extra tools | `exa_find_similar`, `exa_get_contents` |
| Cost note | ~$7 per 1,000 searches; new accounts get free credit |

### `querit` — Querit

| | |
|---|---|
| Category | Search account |
| Endpoint | `https://api.querit.ai/v1` + `/search` |
| Configurable endpoint | In principle, but `search_byo` passes `None`, so **no** |
| Auth | `Authorization: Bearer <key>` |
| Key shape | No documented prefix |
| Key from | The Querit dashboard — **the docs never print a URL for it**, so the catalogue links to `querit.ai` rather than inventing one |
| API shape | `POST` JSON. Not OpenAI-shaped |
| Auth failure | `401`, body `error_code`. Note the wire quirk: `error_code` is the **string** `"401"` on errors and the **integer** `200` on success, so it deserializes loosely |
| Extra tools | none |
| Cost note | 1,000 free requests a month, no card; ~$4 per 1,000 after |

### `searxng` — SearXNG (self-hosted)

| | |
|---|---|
| Category | Self-hosted |
| Endpoint | the operator's own instance; the tool appends `/search` unless the base already ends in it |
| Configurable endpoint | **Yes, and required** — it is the whole record |
| Auth | **None.** SearXNG has no API authentication of any kind; anything in front of an instance is a reverse proxy the operator added |
| API shape | `GET /search?q=…&format=json`. Not OpenAI-shaped |
| Failure | `403` means **JSON output is not enabled**, not a credential problem — see below |
| Free probe | `GET /config` returns the instance configuration and runs no search; `GET /healthz` returns `OK` |
| Cost note | none — it is the operator's own hardware |

**JSON output is off by default.** SearXNG ships `settings.yml` with
`search.formats: [html]`, and requesting an unset format aborts with `403`. So
the single most likely failure when connecting a working instance is a `403` that
means *"add `json` to `search.formats`"*. The connect dialog says it before the
operator submits, and the `format` probe class says it again afterwards.

## Managed is not in this table

`managed` is a slug in today's `SUPPORTED_PROVIDERS`, and it stays one for the
write route's validation, but it is not a catalogue entry: it has no endpoint the
company owns, no key the company pastes and no row to add. It is rendered from
resolution — see [`data-model.md`](data-model.md).

## What a catalogue entry holds

```rust
pub struct SearchProviderInfo {
    /// The slug. Also the identity; the harness dispatches on it.
    pub slug: &'static str,
    /// Display name.
    pub label: &'static str,
    /// Which of the two questions this provider answers.
    pub category: Category,       // Account | SelfHosted
    /// The API host, for the dropdown's detail line and the probe.
    pub endpoint: &'static str,
    /// How the credential is presented. `None` for SearXNG.
    pub auth: Option<AuthStyle>,  // Header(&'static str) | Bearer
    /// Where an operator gets a key. `None` where no authoritative URL exists.
    pub key_source: Option<&'static str>,
}
```

No `key_prefix` field. None of the three account providers documents one, and a
placeholder or a validation rule invented from a blog post would reject a valid
key for a reason the operator cannot see.
