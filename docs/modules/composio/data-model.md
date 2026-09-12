# The stored shape

Four slots in the secret store, and the reasons each is separate. Nothing in this
file changes in the console rework — it is here because the console cannot be
read correctly without it.

All of it lives in `src/company/composio.rs`.

## The slots

| Key | Holds | Read by |
|---|---|---|
| `composio/mode` | `"managed"` or `"byok"` | every path, first |
| `composio/token` | the **managed** bearer — the *TinyHumans backend* recognises it | managed resolution only |
| `composio/api_key` | the company's **own** Composio key (`ak_…`) — *Composio* recognises it | BYOK resolution only |
| `composio/defaults` | which connected account each toolkit acts as | the execute path |

## Why the two credentials are not one slot

`composio/token` is presented to `api.tinyhumans.ai`. `composio/api_key` is
presented to `backend.composio.dev`. **They authenticate different hosts.**

A single slot holding "whatever the current mode wants" would mean switching mode
without re-entering a credential presents the previous host's secret to the new
one. That fails as a 401, which reads to an operator as "my key is wrong" when
the key is fine and the routing is wrong. The inference surface has exactly this
defect with its single `inference/key` slot, and warns about it in help text
because it cannot prevent it. Composio does not have it and must not acquire it.

The slots are also written by different controls with different lifecycles, and a
credential that two controls can write is a credential two surfaces will
eventually disagree about.

## Why the mode is stored rather than inferred

It would be tempting to say "BYOK if an api_key is present". The module refuses
that, for the reason `crate::company::search::PROVIDER_SECRET` refuses it:

> the mode decides which *API* the credential is presented to, and a credential
> sent to the wrong one fails in a way that reads like a bad credential.

Two further consequences fall out of storing it:

- The console can report the mode **without reading a secret slot at all**.
- A stored-but-unused key is representable. Under managed, an `api_key` is simply
  never read — see [`resolution.md`](resolution.md). If the mode were inferred,
  that state could not exist, and the only way to go back to managed would be to
  destroy the key you would need to return to BYOK.

## Parsing the mode: unknown means managed

`ComposioMode::parse` reads `byok` and, as an alias, `direct`. **Everything
else — an empty slot, a typo, a mode from some future shape — reads as
`managed`.**

Unknown-means-managed rather than unknown-is-an-error because this is read on the
roster path: a hand-edited slot must not be able to take a company's tools away,
and managed is the route that works with nothing stored.

The `direct` alias is not load-bearing today — only `byok` is ever written — but
it exists because of what the fallback would otherwise do. Three repos in this
org spell this split three ways: OpenHuman says `direct` / `backend`, TinyMemory
says `direct` / `proxied`, and this one says `byok` / `managed`. `direct` falling
through to managed would be silent and wrong in the specific way this surface
refuses: a company that asked to act through its own Composio account would act
through the platform's instead.

The managed spellings need no aliases — `backend` and `proxied` already land on
managed through the fallback, which is what they mean.

## Where the calls go

| Mode | Host | Entity |
|---|---|---|
| `managed` | the resolved backend URL | derived by the backend from the bearer |
| `byok` | `https://backend.composio.dev` | `default` |

The managed backend URL resolves first-non-empty from
`OPENCOMPANY_COMPOSIO_BACKEND_URL`, then `TINYHUMANS_API_URL` (so a staging
tenant's Composio follows staging), then the prod default. It is credential-free
and safe on the console read plane.

The status DTO reports `backendUrl` as the host the calls **really** reach —
Composio's own host under BYOK. Echoing the managed backend URL after a switch to
BYOK would read as though nothing had happened.

`DIRECT_ENTITY_ID` is `default` on purpose, matching what a BYOK operator sees in
their own Composio dashboard. Scoping to the company id instead would isolate two
OpenCompany companies sharing one key, but would also hide every connection the
operator already made in that account — which is the first thing a BYOK operator
looks for. The shared-account caveat is the same one `composio/token` already
carries: two companies pasting one credential share one entity, and that cannot
be prevented from this side.

## `composio/defaults`

A small JSON object, `{"gmail": "ca_123"}` — which connected account a toolkit
acts as (issue #820).

It is a **preference, not a secret**: the ids in it are already handed to the
console by `GET …/composio/connections` and are useless without the bearer that
scopes them. It lives in the secret store because that is the one per-company
key/value plane this repo has, and because keeping it beside the credentials
means a company's Composio state moves, backs up and is deleted as one thing.

**Absent for a toolkit means no preference has been expressed**, and the execute
path then sends no connection id at all, leaving resolution to Composio exactly
as before. That absence is the ordinary case, not a degraded one — one account
per toolkit needs no choice. Nothing invents a default from the connection list:
a default the product does not actually make would be a claim the harness could
not honour, which is the failure #820 was filed about.

A blob that will not parse is treated as *no defaults* rather than an error. The
only writer is the console, so unparseable means hand-edited or from a future
shape, and the honest response on the agent path is to fall back to Composio's
own resolution rather than withhold the tools. It is logged, not swallowed.

## What the console is allowed to see

`GET …/composio` carries tier names and non-secret routing. It carries **no**
`token` and **no** `tokenConfigured`, and tests assert their absence — see
[`resolution.md`](resolution.md) for why that boolean is a trap rather than a
convenience.

Nothing holding a credential derives `Serialize`. A credential-bearing field is
private, its `Debug` redacts, and the DTO carries a tier name or a boolean
instead. A leak test drives a real-shaped key through the write and read routes
and asserts no response body contains it; extend it whenever a route is added,
before there is anything to leak.
