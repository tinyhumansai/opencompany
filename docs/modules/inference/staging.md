# Staging

The order the work lands in. Each stage is shippable on its own and leaves the
product working; none of them is a flag-day.

The sequencing rule: **storage before surface, surface before routing.** A
provider list with no way to add one is useless but harmless; an add flow writing
into a storage shape that later changes is a migration nobody planned.

```
  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐
  │ 1       │──▶│ 2       │──▶│ 3       │──▶│ 4       │──▶│ 5       │
  │ storage │   │ read +  │   │ connect │   │ routing │   │ health  │
  │ + entry0│   │ list UI │   │ flow    │   │ table   │   │ on rows │
  └─────────┘   └─────────┘   └─────────┘   └─────────┘   └─────────┘
       │             │             │             │             │
   no visible    list of one   add a second  tiers can    rows carry
   change        (today's)     provider      name one     last state
```

Every stage is built on the seams in [`architecture.md`](architecture.md):
`catalogue` (data), `store` (persistence over a port), `resolve` (pure
decisions), `probe` (IO at the edge, classification pure). A stage that puts a
decision in a handler or a component has been done wrong, however well it works.

## Stage 0 — collapse the duplication

Before anything else, because every later stage would otherwise add a seventh
copy.

One source for provider kinds and abstract tiers, with the console's descriptor
fields (`acceptsKey`, `requiresBaseUrl`, `keyKind`, presets) derived from or
asserted against it. Fix the two already-drifted copies: the OpenRouter
attribution headers in `provider.rs` versus `roster_build.rs`, and the stale
`"managed"` member of the wire union in `api/inference.ts`.

Lands as the `catalogue` module plus its console mirror, with the full list from
[`catalogue.md`](catalogue.md) — 26 cloud providers, 3 local runtimes, 2 CLI
logins — replacing the three-provider allowlist.

Ships with: a test that fails when the lists diverge. No behaviour change beyond
the catalogue growing.

## Stage 1 — storage takes a list, with the flat slot as entry zero

The reader yields a list; the existing flat `inference/config` and
`inference/key` are its first element. New entries live at `provider/<slug>/…`.
Nothing existing moves — see the migration constraint in
[`data-model.md`](data-model.md).

Also in this stage, because they are one decision:

- `ProviderId` and `slug`, generated and reserved.
- `enabled`, defaulting true.
- The catalog cache key gains the provider slug, and eviction fires per provider.
- The boot predicate becomes *does any enabled entry resolve* —
  `restart_pending`, `runner_gap_for`, `cognition_state` and rebuild-in-place all
  read the same predicate.

Ships with: every existing test still passing unchanged, especially
`the_default_harness_reads_the_legacy_flat_keys` and the three-tier precedence
tests. **If those need editing, the entry-zero design is wrong — stop.**

No visible change. A company still has one provider; it is now the first element
of a list.

## Stage 2 — the list, read-only

`GET {scope}/inference` grows a `providers[]` alongside the existing fields,
which stay exactly as they are so the five non-inference consumers of
`InferenceStatus` keep working.

The console renders the Connected list. For a company with one provider it shows
one row — which is the truth, and already more than the current form says.

Ships with: the existing form still present and still the way to change things.
Two surfaces briefly, deliberately: the list tells you what is connected, the
form still changes it.

## Stage 3 — the connect flow

The modal, the three categories, the classified probe, the rollback rules, and
the draft-probe route with its SSRF answer — all in
[`connect-flow.md`](connect-flow.md).

Per-provider credential slots become real here: adding a second provider is the
first moment two keys exist at once, and the moment the "your stored key may
belong to a vendor you are no longer using" paragraph can be deleted.

Delete and disable land here too, with their route-scrubbing.

Ships with: the old single-provider form retired, because now it is the thing
that cannot express the truth. Its `stripProxyIncompatible` machinery moves to
the per-provider model field — read its nine-regression comment before touching
it; six of those nine are about stripping a value mid-keystroke.

**Browser verification is mandatory for this stage**, light and dark, including:
add each category, a wrong key, a proxy-shaped failure (the amber path), delete
with a route pointing at the deleted provider.

## Stage 4 — the routing table

Tiers gain an optional provider, using the extended string grammar. Unset means
primary — never a sibling's provider.

Ships with: fail-closed on a route naming an unknown slug, the load-time
reconciliation for routes orphaned outside the UI, and error copy that names the
tier as well as the provider.

## Stage 5 — health on rows

Record what the system already discovers — add-time probe, manual Test, and the
turn path's 401 — against the provider. Latch once per failure episode. Never
cache a credential failure against an endpoint.

No poller. See [`known-defects.md`](known-defects.md) §1.

## Explicitly out of scope

Named here so they are decisions rather than omissions:

- **Cross-provider failover.** The reasoning is in [`routing.md`](routing.md):
  it does not fit inside the turn's own timeout without eager resolution whose
  cost is paid on every turn for a path almost never taken. If it is wanted
  later it is an explicit per-tier secondary, not an implicit chain.
- **Completing the per-harness write path.** The scoped storage helpers exist
  and have no callers; `GET {scope}/harnesses` is read-only by design, and its
  own comment says defining a harness from the console needs an overlay
  mechanism harnesses do not have. This work runs at company scope and leaves
  that seam exactly as it found it.
- **A native Anthropic provider kind.** Anthropic's API is not OpenAI-shaped, so
  it is a new request/response path, not a preset. Reachable via
  `openai_compatible` plus a gateway in the meantime.
- **Per-agent model overrides on the built-in path.** `agent.model` is
  validator-rejected there today. The tier remains the per-agent knob.
- **Cost- or latency-based routing.** Prices can be displayed in the model
  picker; nothing selects on them.

## Risks worth stating before starting

**The rework's biggest regression risk is the credential seam.** Four independent
mechanisms keep keys off the wire today and the easiest way to break all four is
to derive `Serialize` on a new record. Extend
`key_never_leaks_across_any_response` to the list routes in Stage 2, before there
is anything to leak.

**The second is `TierVocabulary`.** It has no equivalent in the design being
copied, so a faithful port would quietly drop it. Two production incidents are
recorded in its comments. Each provider keeps its own.

**The third is the five non-inference consumers of `InferenceStatus`.**
`SetupDialog`, `AgentDetailView`, `CopilotPanel` and `WorkflowCreateDialog` use
it as the "can this company think?" oracle. Add to that DTO; do not reshape it.
