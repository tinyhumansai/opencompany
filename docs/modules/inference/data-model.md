# Data model

The record, where its credential lives, and how it is scoped in a product that
serves many companies from one process.

## The record

```rust
/// One configured way for a company to reach a model.
///
/// Derives no `Serialize`: see the credential rule below. The wire shape is a
/// separate DTO that carries `key_configured: bool` and never the key.
pub struct Provider {
    /// Stable, opaque, generated once. Never shown, never reused after delete.
    pub id: ProviderId,
    /// Routing key the operator chooses or inherits from the kind. Unique per
    /// scope. This is what a routing entry names.
    pub slug: String,
    /// Display label. Never used in routing.
    pub label: String,
    /// Provider kind, from the closed set. Decides defaults and auth shape.
    pub kind: ProviderKind,
    /// OpenAI-compatible base URL. Resolved, never empty for a valid record.
    pub base_url: String,
    /// Abstract tier -> concrete model id. Empty means pass tiers through.
    pub models: BTreeMap<String, String>,
    /// Whether this provider is available for routing. Distinct from deleted.
    pub enabled: bool,
    /// Who this belongs to. See "Scoping" — this field is the whole reason
    /// openhuman's record could not be copied verbatim.
    pub scope: ProviderScope,
}
```

**There is no key on this record.** That is the single most important line in
this document.

### Why the slug is not the id

`id` is identity; `slug` is address. They are separated because a routing entry
names a provider in a string grammar an operator can read and hand-edit
(`reasoning-v1 -> acme:gpt-5`), while identity has to survive a rename. openhuman
learned this the same way — it keeps both, with the id *"never shown in the UI"*.

The current code has neither. `provider_slug()` exists but is telemetry-only and
derived *from* the kind, so it cannot distinguish two OpenRouter accounts.

## Credentials

```
   Provider record                    SecretStore
   ┌──────────────────┐               ┌────────────────────────────────┐
   │ id, slug, label  │               │ provider/<slug>/key            │  ← ALL
   │ kind, base_url   │  ──names──▶   │ inference/key                  │  ← legacy,
   │ models, enabled  │               └────────────────────────────────┘    read-only
   │ scope            │                            │
   │                  │                            │ read once, at call time,
   │  NO KEY FIELD    │                            ▼ by exactly one function
   └──────────────────┘               InferenceDecl::bearer() -> Authorization
```

The rules, each of which the current code already satisfies and must continue to:

1. **The key is never a field on a record that can be serialized.** Derive no
   `Serialize`. Keep the field private. Make `Debug` redact.
2. **The key is never returned by any route.** The read shape carries
   `key_configured: bool`, derived by asking the store whether a value exists —
   never by storing a flag, which can go stale against a deleted secret.
3. **The key is read per request, not captured at boot.** This is what allows the
   managed tier to be a rotating projected token.
4. **One credential slot per provider**, keyed by slug, *with no exception*.
   This is the change: there used to be one slot per company, which is why
   switching provider without re-entering a key failed on the next turn with a
   401 the host had to explain in a paragraph. The managed/TinyHumans provider
   is keyed on the slug `tinyhumans` rather than on its OpenRouter-shaped kind —
   the kind says what shape of API this is, the slug says whose account it is,
   and a company holding both a managed credential and a real OpenRouter account
   must not have one slot between them.

   **Convergence rather than migration.** A write always goes to
   `provider/<slug>/key` and clears `inference/key` in the same operation; a
   read tries the new address and falls back to the old one. An existing company
   keeps working untouched and the first save moves it. No flag day, no
   half-migrated state, and the fallback is one line to delete once nothing
   reads it. The clear must be *issued* — the store has no delete — and a
   failure is logged loudly, because a key left at the old address after the new
   one is written is an orphaned secret.
5. **Deleting a provider clears its key.** openhuman does not do this — removing
   a provider leaves `provider:<slug>` on disk and re-adding silently reuses the
   old key. See [`known-defects.md`](known-defects.md).

### The test that must keep passing

`key_never_leaks_across_any_response` drives a real BYOK token through `PUT`,
`GET` and `POST …/test` and asserts no response body contains it. Extend it to
the list routes rather than replacing it.

## Scoping — where openhuman's design does not transfer

openhuman's `CloudProviderCreds` has **no owner column**, and `primary_cloud` is
a single global pointer. That is correct for one user on one machine with one
TOML file. It is wrong here in three ways at once, and this is not a detail to
add later — it shapes the record and every routing field from the first line.

```
                       ┌─────────────────────────────────────────────┐
                       │  one OpenCompany process                    │
                       │                                             │
  env  (host-wide) ────┼──▶ EnvDefault ──┐                           │
  OPENCOMPANY_         │                 │  lowest precedence,       │
  INFERENCE_KEY/_URL   │                 │  shared by every company  │
                       │                 ▼                           │
                       │   ┌───────────────────────────────────┐     │
                       │   │ company A   providers[] + keys    │     │
                       │   │ company B   providers[] + keys    │     │  ← SecretStore
                       │   │ company C   providers[] + keys    │     │    keyed on
                       │   └───────────────────────────────────┘     │    CompanyId
                       │             │                               │
                       │             ├── harness "default"  (flat keys)
                       │             └── harness "<id>"     (namespaced)
                       └─────────────────────────────────────────────┘
```

Three scoping facts that constrain the design:

**Per company.** Every persisted value is keyed on `CompanyId` through the
`SecretStore` port. Every route is company-scoped and registered under both
addressing forms — the platform `…/companies/{id}/…` form and the single-company
`…/company/…` alias, with different auth on each.

**Per harness, partially.** The default harness keeps flat keys; named harnesses
namespace under `harness/<id>/`. The read path is complete. The write path has no
caller. See "The migration constraint" below.

**Per process, at the floor.** The `EnvDefault` is host-wide. One injected
endpoint, shared by every company that host serves. It reaches a request only
through `resolve_endpoint`, and only for the managed kind or a keyless
`openrouter` with no base-URL override — a keyless `openrouter` that *does*
override the endpoint is deliberately denied the platform credential, because
sending the platform token to an arbitrary URL would leak it. There is a test
named for exactly that.

### The catalog cache is scoped too, and it has already bitten

Model catalogs are cached per base URL **plus** the reading company **plus** the
harness. Keyed on the company alone, one harness's entitlement-scoped catalog was
reused for another for an hour without its credential ever being presented. A
keyless read is a public property of an endpoint and may be shared; an
authenticated one is not.

**A provider list multiplies this.** N providers per company means N catalog
entries per company per hour. The cache key must gain the provider slug, and the
eviction rule ("every path that writes a credential evicts") must fire per
provider, not per company.

## The default provider is a marker, not a position

Which provider an **unset** workload goes through is stored, not inferred from
list order. One slot, `inference/default`, holding one slug:

```
inference/default  ──▶  "acme"        ← at most one; two are not representable
```

A flag per record could be true twice and would then need a rule for which wins.
A slot cannot, so there is no rule to write down and no state to reconcile.

`resolve::primary(providers, marked)` returns the marked provider when it exists
and is enabled, and otherwise the first enabled one — which is what it returned
unconditionally before, so an unmarked company is unchanged and nothing is
backfilled. `None` means the managed brain, which is always available.

Both write paths keep the marker honest: disabling the marked provider clears
the marker (rather than moving it to something the operator never chose), and
deleting it clears the marker in the same operation that scrubs its routes.

## The migration constraint

This is the hardest constraint in the rework and it is already written down in
the code:

> a tenant's stored console override and credential already live at
> `RUNTIME_CONFIG_KEY` / `KEY_KEY`, and the store has no rename — so namespacing
> every harness would silently orphan the config of every company already
> running, which is the one migration this design cannot afford.

The store has **no rename and no delete** (clearing is a write of the empty
string). So a provider list keyed by slug cannot simply take over
`inference/config`.

Two options, and the plan picks the second:

**(a) Migrate.** Read the flat blob, write it as a one-entry list under a new
key, clear the old. Requires owning the failure mode: a half-completed migration
on a store with no transaction leaves a company with neither config.

**(b) Keep the flat slot as entry zero.** The existing `inference/config` and
`inference/key` remain exactly where they are and *are* the first provider in the
list — slug derived from its kind, id generated on first read. New providers are
written to `provider/<slug>/…`. Nothing existing moves; nothing is orphaned;
there is no migration to half-complete.

(b) costs one special case in the reader forever. That is cheaper than a
migration the codebase has already declared unaffordable, and it means the change
is additive for every running tenant.

```
read providers(company, scope):
    entry0 = read("inference/config")          ← may be absent
    if entry0:  yield Provider::from_flat(entry0, key="inference/key")
    for slug in list("provider/*/config"):
        yield Provider::from_scoped(slug)
```

## Boot-time brain selection

`RuntimeBuilder` asks one question at boot — *does anything resolve?* — and picks
the brain once. A company that resolved nothing is on the echo brain until the
runtime is rebuilt in place or the process restarts; that is what `restartRequired`
reports, and it exists because promising "next turn" when the runtime cannot keep
it was a real bug.

With a list, that predicate becomes **does any enabled entry resolve**, and four
call sites plus their tests move with it: `restart_pending`, `runner_gap_for`,
`cognition_state`, and the rebuild-in-place path.

## What does not change

- The two tier vocabularies, and `TierVocabulary` discovery. A provider gains a
  vocabulary per entry; the classification logic is untouched.
- `agent.tier` as the per-agent knob. A tier names a workload, never a model,
  which is what lets an agent keep its tier while moving between harnesses.
- The authority model — `AdminScopedCompany` on anything that decides for the
  company, `ScopedCompany` on reads and on probe.
- The manifest `[inference]` block. It stays valid and stays the declarative
  tier, below runtime and above env.
