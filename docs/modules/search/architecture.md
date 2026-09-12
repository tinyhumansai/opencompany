# Architecture

How the code is arranged, and how each seam is tested. The organising rule is the
inference rework's, unchanged: **every decision this feature makes should be
testable without a host, a browser, or a network.**

That is not a testing preference. It is what keeps the interesting logic — which
failure class is this? which provider is active? is this instance URL safe to
fetch? — out of components and handlers where it can only be exercised end to
end.

## The seams

```
                          ┌───────────────────────────────────────┐
  HTTP ───────────────────│  server/ops/search.rs                 │  handlers only:
                          │  routes, extractors, DTO mapping      │  auth + shape
                          └──────────────────┬────────────────────┘
                                             │
            ┌────────────────────────────────┼────────────────────────────────┐
            ▼                                ▼                                ▼
  ┌───────────────────┐          ┌───────────────────┐          ┌───────────────────┐
  │ search/store      │          │ search/resolve    │          │ search/probe      │
  │                   │          │                   │          │                   │
  │ list / put /      │          │ active provider   │          │ one search call   │
  │ delete / enable   │          │ effective slug    │          │ classify per      │
  │ credential slots  │          │ default marker    │          │   provider        │
  │ entry-zero + the  │          │                   │          │ address guard     │
  │ convergence write │          │ PURE              │          │                   │
  │                   │          │                   │          │ IO at the edge,   │
  │ PURE over a port  │          │                   │          │ classifier PURE   │
  └───────────────────┘          └─────────┬─────────┘          └───────────────────┘
            │                              │
            │                              ├──────────────▶ harness/built_in/search_byo.rs
            └──────────────┬───────────────┘                 TenantSearch::resolve
                           ▼                                 (feature = "openhuman")
                ┌───────────────────────┐
                │ search/catalogue      │  four entries, asserted against
                │ (data, no behaviour)  │  its TypeScript mirror
                └───────────────────────┘
```

`src/company/search.rs` becomes `src/company/search/` with `mod.rs` keeping the
module header — the per-company-never-from-environment rule and the
configuration-is-not-feature-gated rule are the two things a reader must meet
first, and they must not move.

**Real modules, not `include!`d halves of one file.** That is the eighth defect
in the borrowed design, and the split here is on responsibility seams anyway.

### `catalogue` — data only

Four entries, no behaviour beyond lookup by slug.
[`catalogue.md`](catalogue.md) is the table.

**Tested by:** a table test asserting every entry has a parseable endpoint and a
known auth style, and a cross-language test that fails when the Rust table and
`frontend/src/search-providers/catalogue.ts` diverge.

### `store` — persistence over the `SecretStore` port

Reads and writes provider records and credential slots. Knows about entry zero —
the legacy flat `search/provider` / `search/api_key` / `search/endpoint` — and
about the `search/provider/<slug>/…` namespace. Knows nothing about HTTP or about
which provider is *right*.

**Tested by:** unit tests against an in-memory `SecretStore`. No host. The cases
that must exist: a company with nothing configured; entry zero round-trips; the
legacy flat keys still read after the change; adding a second provider; **a write
for the entry-zero slug clears the flat key it converged from**; deleting a
provider clears its credential; clearing is a write-of-empty because the port has
no delete; and the one that is the whole point — **two providers hold two
independent credentials, and writing one does not touch the other.**

### `resolve` — the decisions, all pure

```rust
fn active(providers: &[SearchProvider], marked: Option<&str>) -> Option<&SearchProvider>;
fn effective_slug(active: Option<&SearchProvider>, key_present: bool) -> &str;
```

`active` is the marker rule: the marked provider when it exists and is enabled,
else the first enabled one, else `None` — which means managed.
`effective_slug` is today's `effective_provider` with a list behind it, and it
stays **the one derivation** of "which index actually answers", called by the
status route, the capabilities panel and the harness alike. It is the function
whose doc comment already warns that two surfaces mirroring each other's rule
would drift.

**Tested by:** pure unit tests, no async. Marked-and-enabled, marked-but-disabled
falls through, marked-but-deleted falls through, nothing marked uses first
enabled, everything disabled resolves to managed, and a provider whose credential
is missing resolves to managed rather than reporting itself connected.

### `probe` — IO at the edge, classification pure

```rust
async fn probe(entry: &SearchProviderInfo, credential: Option<&str>, endpoint: Option<&str>)
    -> Result<(), ProbeError>;                                  // IO
fn classify(slug: &str, status: u16, body: &str) -> ProbeClass; // PURE
fn describe(class: ProbeClass, provider: &str) -> String;       // PURE
```

`classify` takes the **slug** as well as the status and body, which is the
signature difference that matters: classification is per provider here, because
Brave signals a rejected key with `422` and a body code while everything else
uses `401`. See [`connect-flow.md`](connect-flow.md).

**Tested by:** a table test with a real response per provider per class —
Brave `422 SUBSCRIPTION_TOKEN_INVALID` asserting `auth`, Brave `403` asserting
`unknown` (a WAF, not a key), Exa `401 INVALID_API_KEY` asserting `auth`, Exa
`402` asserting `quota`, Querit's string-typed `error_code` asserting `auth`,
SearXNG `403` asserting `format` **not** `auth`, `407 Proxy Authentication
Required` asserting `unknown` not `auth`, and an id containing `1403` asserting
no match. Those are the cases the ordering and the per-provider dispatch exist
for.

The address guard **is** written here, against the plan's intention, and
[`connect-flow.md`](connect-flow.md) records why: `guard_link` in
`src/server/ops/memory_ingest.rs` refuses every private address including
`.internal` hostnames, which is exactly where a self-hosted SearXNG instance
lives, and it is `#[cfg(feature = "documents")]` while this surface is ungated.
`guard_instance_url` is therefore a narrower rule — metadata and link-local only —
rather than a copy of a stricter one.

### The harness seam barely moves

`TenantSearch::resolve` keeps its signature and its contract — `Ok(None)` means
"search through the managed surface", a store read failure is `Err` and not
`Ok(None)`, and `fingerprint` still covers the credential so a rotation rebuilds
the roster. What changes is its middle: it asks `search::resolve` for the active
provider instead of reading three flat keys.

`byo_search_tools` is untouched. `BYO_SEARCH_TOOLS` is untouched — it is the
closed set of tool names the `search` namespace must account for, and a provider
list does not add a name.

**This is the ordering hazard.** The harness reader, the capabilities reader and
the store's convergence write must land together; a console that writes the new
address while a reader is still on the flat keys silently drops the company to
managed search. [`data-model.md`](data-model.md) states it; the tests that hold
it are the harness resolve tests, which must be run under
`--features openhuman,mcp` and therefore must be named in a CI lane that enables
them (issue #770 — a feature-gated test nothing runs reports nothing).

## The console side

```
  frontend/src/search-providers/
    catalogue.ts        the mirror; data only
    types.ts            SearchProvider, ProbeClass, the DTOs
    resolve.ts          active provider, row model, control set per kind   PURE
    classify.ts         probe class -> copy                                PURE
    connect.ts          the add/connect state machine                      PURE
    ProviderList.tsx    the connected rows
    AddProviderDialog.tsx
    ProviderConnectDialog.tsx
    use-search-providers.ts
```

`frontend/src/views/SearchView.tsx` becomes layout plus handlers and nothing
else. Every branch worth a test moves into `resolve.ts`, `classify.ts` or
`connect.ts` — including **which controls a row offers**, which is per kind and
is exactly the sort of conditional that rots inside a component:

```ts
controlsFor(row)  // managed  -> []               no toggle, no remove
                  // account  -> [enable, test, replace key, remove, default]
                  // searxng  -> [enable, test, edit address, remove, default]
```

"Remove key" is not offered where there is no key. That is the brief's rule and
it is one function.

## Testing

| Layer | Kind | Must cover |
|---|---|---|
| `catalogue` | unit + cross-language | every entry valid; Rust and TS agree |
| `store` | unit, in-memory port | entry zero, convergence clear, independent credentials, delete clears key |
| `resolve` | unit, pure | the marker rule, fall-through, managed fallback |
| `probe::classify` | unit, table | every provider's auth shape, the 422/403/407 traps, SearXNG `format` |
| address guard | unit | loopback, link-local, metadata, post-DNS recheck, redirect target |
| routes | integration | authority (admin vs scoped), **key never in any response body** |
| harness | unit, `--features openhuman,mcp` | active provider reaches `byo_search_tools`; fingerprint covers rotation |
| console pure | vitest | control sets per kind, copy selection, connect state machine |
| console UI | Playwright | the flows below |

### The test that must never be deleted

The leak test drives a real BYO key through every route and asserts no response
body contains it. **Extend it to each new route as that route is added**, before
there is anything to leak — and assert on the **value**, so a field rename cannot
make it pass.

### The browser flows

Not shipped as Playwright specs in this change — they were driven by hand
against a real host, and the list is here as the matrix that pass covers rather
than as a claim about CI. Adding them as specs needs a lane that selects them
(issue #475), which is a separate change.

1. Add an account provider with a good key → row appears, marked ok.
2. Add one with a bad key → rejected, **no row, no credential stored**.
3. Add one behind a non-auth failure → row created, amber advisory, **key kept**.
4. Add a SearXNG instance whose JSON output is off → row created, `format`
   advisory naming `search.formats`, endpoint kept.
5. Add a second provider → both rows, both credentials independent.
6. Set the second as default → the effective provider changes and survives reload.
7. Delete the default → the marker moves and the UI says which provider is active now.
8. Disable a provider → it is not eligible as active, credential retained.

## Rules for the implementation

- **No decision in a component or a handler.** If it has a branch worth a test,
  it belongs in a pure module beside the others.
- **No `Serialize` on anything holding a credential.** Private field, redacting
  `Debug`, boolean on the DTO. `TenantSearch` is the existing example.
- **Additive DTO changes only.** `SearchStatus` is read by the capabilities panel
  as well as this page. Add fields; do not reshape it.
- **`AdminScopedCompany` on every write, and on probe.** Reads stay
  `ScopedCompany`, which is what the existing routes do.
- **Load-bearing comments.** Brave's 422, SearXNG's 403, the convergence clear
  and the entry-zero special case each deserve one where the code enforces them.
- **Small commits on a seam.** One per module.
