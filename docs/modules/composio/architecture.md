# Architecture

How the code is arranged, and how each seam is tested. The organising rule is the
one the inference rework states: **every decision this feature makes should be
testable without a host, a browser, or a network.**

That is not a testing preference. It is what keeps the interesting logic — which
failure class is this? which tier answers? may this control act? — out of
components and handlers, where it can only be exercised end to end.

## The seams

```
                      ┌──────────────────────────────────────┐
  HTTP ───────────────│  server/ops/composio.rs              │  handlers only:
                      │  routes, extractors, DTO mapping     │  auth + shape
                      └───────────────┬──────────────────────┘
                                      │
              ┌───────────────────────┼───────────────────────┐
              ▼                       ▼                       ▼
    ┌───────────────────┐  ┌───────────────────┐  ┌───────────────────┐
    │ company/composio  │  │ company/composio  │  │ company/composio  │
    │   ::resolve_*     │  │   ::store_api_key │  │   _probe          │
    │                   │  │                   │  │                   │
    │ managed chain     │  │ direction-ordered │  │ IO at the edge,   │
    │ BYOK short-circuit│  │ writes            │  │ classify() PURE   │
    │ mode parsing      │  │                   │  │ describe() PURE   │
    │                   │  │ PURE over a port  │  │                   │
    │ PURE over a port  │  │                   │  │                   │
    └───────────────────┘  └───────────────────┘  └───────────────────┘
```

Nothing here is new except the probe module. The resolution and storage seams
already existed and already had this shape; the rework adds one derivation to the
status handler and one classified probe beside it.

### `company/composio` — resolution and storage

Pure over the `SecretStore` port. Knows the four slots, the mode vocabulary, the
managed chain and the BYOK short-circuit. Knows nothing about HTTP.

**Tested by:** unit tests against an in-memory `SecretStore`. No host. The cases
that must exist: each of the four managed tiers resolving in order; BYOK reading
only `composio/api_key`; BYOK with no key resolving to `None` rather than falling
through; a store read error propagating rather than degrading; `parse` mapping
`direct` to BYOK and everything unknown to managed; and both write orderings in
`store_api_key` leaving an inert state when the second write fails.

### `company/composio_probe` — IO at the edge, classification pure

```rust
async fn probe(api_key) -> Result<(), String>;   // IO, constant endpoint
fn classify(err: &str) -> ProbeClass;            // PURE
fn describe(class: ProbeClass) -> &'static str;  // PURE
```

Splitting `classify` from `describe` is what makes the classes testable without a
translator.

**Tested by:** a table test with a real error string per class, and specifically
the four cases the branch ordering exists for — `407 Proxy Authentication
Required` asserting `unknown` **not** `auth`; a bare `403 Forbidden` asserting
`unknown`; a `403` *with* credential wording asserting `auth`; and a string
containing `1403` asserting no spurious match.

There is no SSRF guard to test, because there is no caller-supplied address. See
[`connect-flow.md`](connect-flow.md).

### The handler

`effective_status` gains one call: `resolve_credential`, run **regardless of
mode**, to fill `managedCredentialSource`. That is the only new decision at this
layer, and it is a delegation rather than a branch.

`set_api_key` gains the probe-then-store ordering. The branch it contains —
auth-class refuses, everything else stores — is one `match` over a value a pure
function produced.

## The console side

```
  frontend/src/composio/
    types.ts      ComposioMode, CredentialSource, ProbeClass, the row model
    rows.ts       composioRows(status) -> ComposioRow[]            PURE
    classify.ts   probeCopy(class) -> string                        PURE
  frontend/src/views/connections/
    ComposioSection.tsx    layout + handlers, no decisions
```

`rows.ts` is the centrepiece and the reason the component is thin. It decides,
for both rows at once: the badge, the sub-line, and which controls are permitted.
A component should read as *layout plus handlers*, with no branch in it that
deserves a test of its own.

This mirrors `frontend/src/search/` — `query.ts`, `rank.ts`, `sources.ts` pure and
unit-tested, the dialog a thin shell. That is the closest worked example in this
repo; follow it.

## Testing

| Layer | Kind | Must cover |
|---|---|---|
| resolution | unit, in-memory port | four managed tiers, BYOK short-circuit, fail-closed, error propagation |
| `store_api_key` | unit, in-memory port | both write orderings leave an inert state on partial failure |
| `probe::classify` | unit, table | every class, and the proxy/WAF ordering traps |
| status DTO | unit | `managedCredentialSource` under BYOK reports the managed chain, not the BYOK key |
| routes | integration | admin vs member authority; **key never in any response body** |
| console pure | vitest | `composioRows` over mode × source × managedSource; copy selection |
| console UI | Playwright | the flows below |

### The test that must never be deleted

The credential-leak test drives a real-shaped BYOK key through the write route
(both the probe-pass and probe-fail paths) and the read route, and asserts **no
response body contains it**, asserting on field *values* rather than on the
absence of a field name.

**Extend it in the same change that adds a route**, before there is anything to
leak. Several mechanisms keep credentials off the wire here; the easiest way to
break all of them at once is to derive `Serialize` on a new record for
convenience.

The companion assertions — that `token` and `tokenConfigured` are **absent** from
the status DTO — are not redundant with it. The leak test catches a value going
out; those catch the shape that would invite one back in, and they are the
guardrail against re-answering "does Composio work" with the #886 boolean.

### The e2e flows that must exist

Written against a real host, because they are the ones that only break in
integration:

1. Switch managed → BYOK with a good key: the row becomes `Active`, the other
   offers `Use this`, and `backendUrl` reports Composio's host.
2. Switch BYOK → managed: the key stays stored and unread, and the managed row
   reports the tier that answered.
3. A bad BYOK key: rejected, **nothing stored**, the company stays on its
   previous mode.
4. A non-auth probe failure: the key **is** stored, the row goes active, and an
   amber advisory appears — not an error.
5. Remove the key: the row reports not-configured and the company returns to
   managed.
6. A hosted-shaped instance with nothing pasted: the managed row reports
   `attested` and reads as working — the #886 regression test at the UI layer.

## Rules for the implementation

- **No decision in a component or a handler.** If it has a branch worth a test,
  it belongs in a pure module beside the others.
- **No `Serialize` on anything holding a credential.** Private field, redacting
  `Debug`, tier name or boolean on the DTO.
- **Additive DTO changes only.** Existing consumers read `credentialSource`,
  `mode`, `effectiveToolkits` and the connection shapes. Add fields; do not
  reshape.
- **Load-bearing comments.** Every incident referenced in these docs — the
  proxy/WAF ordering, the #886 boolean, the direction-ordered writes, the
  probe-a-draft deviation — deserves one where the code enforces it.
- **Small commits on a seam.** One per module, not one per stage.
