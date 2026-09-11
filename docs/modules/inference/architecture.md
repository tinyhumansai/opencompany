# Architecture

How the code is arranged, and how each seam is tested. The organising rule:
**every decision this feature makes should be testable without a host, a browser,
or a network.**

That is not a testing preference — it is what keeps the interesting logic (which
failure class is this? which provider serves this workload? is this slug
available?) out of components and handlers where it can only be exercised end to
end.

## The seams

```
                          ┌───────────────────────────────────────┐
  HTTP ───────────────────│  server/ops/inference.rs              │  handlers only:
                          │  routes, extractors, DTO mapping      │  auth + shape
                          └──────────────────┬────────────────────┘
                                             │
            ┌────────────────────────────────┼────────────────────────────────┐
            ▼                                ▼                                ▼
  ┌───────────────────┐          ┌───────────────────┐          ┌───────────────────┐
  │ inference/store   │          │ inference/resolve │          │ inference/probe   │
  │                   │          │                   │          │                   │
  │ list / get / put  │          │ precedence chain  │          │ GET /models       │
  │ delete / enable   │          │ route -> provider │          │ classify failure  │
  │ credential slots  │          │ tier  -> model    │          │ SSRF guard        │
  │                   │          │ vocabulary        │          │                   │
  │ PURE over a port  │          │ PURE              │          │ IO at the edge,   │
  │                   │          │                   │          │ classifier PURE   │
  └───────────────────┘          └───────────────────┘          └───────────────────┘
            │                                │
            └────────────────┬───────────────┘
                             ▼
                  ┌───────────────────────┐
                  │ inference/catalogue   │   the 26+3+2 table, generated or
                  │ (data, no behaviour)  │   asserted against its TS mirror
                  └───────────────────────┘
```

### `catalogue` — data only

The provider table from [`catalogue.md`](catalogue.md). No behaviour beyond
lookup by slug. One source of truth with the console mirror either generated from
it or asserted equal by a test — the current six-copies-and-two-have-drifted
situation is what this exists to end.

**Tested by:** a table test asserting every entry has a parseable endpoint and a
known auth style, and a cross-language test that fails when Rust and TypeScript
diverge.

### `store` — persistence over the `SecretStore` port

Reads and writes provider records and credential slots. Knows about entry-zero
(the flat legacy slot) and the `provider/<slug>/…` namespace. Knows nothing about
HTTP or about which provider is *right*.

**Tested by:** unit tests against an in-memory `SecretStore`. No host. Cases that
must exist: entry-zero round-trips; a company with no providers; adding a second;
deleting one clears its credential; clearing is a write-of-empty (the store has
no delete); slug collision refused; the legacy flat keys still read after the
change.

### `resolve` — the decisions, all pure

Three functions, none of which does IO:

```rust
fn providers_for(company_state) -> Vec<Provider>;                  // precedence applied
fn provider_for_workload(w: Workload, routes: &Routes, ps: &[Provider]) -> Resolution;
fn model_for_tier(tier, overrides, vocabulary) -> String;           // exists today; unchanged
```

`Resolution` is an enum, not an `Option` — it distinguishes *resolved*, *unset
and falling through to primary*, and *names a provider that does not exist*,
because the third is an error that must name the workload and the slug rather
than silently demoting.

**Tested by:** pure unit tests, no async, no fixtures beyond structs. This is
where the no-inheritance rule, the fail-closed rule, and the three scrub-matching
rules (cloud by slug, CLI with no slug, local only when no local remains) are
pinned.

### `probe` — IO at the edge, classification pure

```rust
async fn probe(endpoint, credential) -> Result<Catalog, ProbeError>;   // IO
fn classify(err: &str) -> ProbeClass;                                  // PURE
fn describe(class: ProbeClass, provider: &str) -> String;              // PURE
```

Splitting `classify` from `describe` is what makes the six failure classes
testable without a translator, and it is how openhuman does it.

**Tested by:** `classify` gets a table test with a real error string per class —
including `407 Proxy Authentication Required` asserting `unknown` not `auth`, a
bare WAF `403 Forbidden` asserting `unknown`, a `403` *with* credential wording
asserting `auth`, and an id containing `1403` asserting no match. Those four
cases are the whole reason the ordering exists.

The SSRF guard is its own pure function over a parsed URL, tested against
loopback, link-local, metadata addresses, and a redirect target.

## The console side

```
  frontend/src/inference/
    catalogue.ts        the mirror; data only
    types.ts            Provider, ProviderRef, RoutingMap, ProbeClass
    routing.ts          inferRoutingMode, refSignature, scrub-on-remove   PURE
    classify.ts         probe-class -> copy                                PURE
    ProviderList.tsx    the connected rows
    AddProviderDialog.tsx
    ProviderKeyDialog.tsx
    RoutingTab.tsx
    WorkloadRow.tsx
    WorkloadModelDialog.tsx
```

Same rule: `routing.ts` and `classify.ts` hold the decisions and are plain
functions over plain data. A component should be readable as *layout plus
handlers*, with no branch in it that deserves a test of its own.

This mirrors what the search feature did in `frontend/src/search/` — `query.ts`,
`rank.ts`, `sources.ts` pure and unit-tested; the dialog a thin shell. Follow
that precedent; it is the closest thing in this repo to a worked example.

## Testing

| Layer | Kind | Must cover |
|---|---|---|
| `catalogue` | unit + cross-language | every entry valid; Rust and TS agree |
| `store` | unit, in-memory port | entry-zero, add, delete-clears-key, collisions, legacy reads |
| `resolve` | unit, pure | precedence, no-inheritance, fail-closed, the three scrub rules |
| `probe::classify` | unit, table | all six classes, the proxy/WAF ordering traps |
| SSRF guard | unit | loopback, link-local, metadata, redirect target |
| routes | integration | auth (admin vs scoped), **key never in any response body** |
| console pure | unit (vitest) | `inferRoutingMode`, ref signatures, scrub-on-remove, copy selection |
| console UI | e2e (Playwright) | the flows below |

### The e2e flows that must exist

Written against a real host, because they are the ones that only break in
integration:

1. Add a cloud provider with a good key → row appears, marked ok.
2. Add one with a bad key → rejected, **no row created, no key stored**.
3. Add one behind a simulated proxy failure → row created, amber advisory, **key
   kept**.
4. Add a second provider → both rows, both credentials independent.
5. Route one workload to the second provider → persists across reload.
6. Delete that provider → the workload's route resets, and the UI says so.
7. Disable a provider → not offered as a routing target, credential retained.
8. Switch routing mode Managed → Own → Advanced → the inferred mode round-trips.

### The test that must never be deleted

`key_never_leaks_across_any_response` drives a real BYOK token through the write
route, the read route and the probe route and asserts no response body contains
it. **Extend it to the list routes in the first stage that adds one**, before
there is anything to leak. Four separate mechanisms keep credentials off the
wire today; the easiest way to break all four at once is to derive `Serialize` on
a new record for convenience.

## Rules for the implementation

- **No decision in a component or a handler.** If it has a branch worth a test,
  it belongs in a pure module beside the others.
- **No `Serialize` on anything holding a credential.** Private field, redacting
  `Debug`, boolean on the DTO.
- **Additive DTO changes only.** `InferenceStatus` is the "can this company
  think?" oracle for `SetupDialog`, `AgentDetailView`, `CopilotPanel` and
  `WorkflowCreateDialog`. Add fields; do not reshape it.
- **Load-bearing comments.** This repo's style is a header explaining *why* the
  arrangement is what it is, and every incident referenced here — the proxy/WAF
  ordering, the per-tier vocabulary bitmask, the entry-zero decision — deserves
  one where the code enforces it.
- **Small commits on a seam.** One per module, not one per stage.
