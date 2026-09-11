# Inference: providers and routing

How a company reaches a model, and how a request finds one. This folder is the
design for reworking that surface; `docs/spec/runtime/providers.md` remains the
spec for what ships today.

| File | What it settles |
|---|---|
| `README.md` | The case for the change, the target shape, and what must not regress |
| [`current-state.md`](current-state.md) | What exists today, honestly — including what is already better than the thing we are copying |
| [`data-model.md`](data-model.md) | The provider record, credential storage, and the migration constraint that shapes both |
| [`credentials.md`](credentials.md) | **Which credential a call presents** — the identity-versus-vendor split, and why `inference/key` cannot mean two things |
| [`catalogue.md`](catalogue.md) | **The provider list, verbatim** — 26 cloud, 3 local, 2 CLI, and the copy |
| [`connect-flow.md`](connect-flow.md) | Adding a provider: the modal, the classified probe, the rollback rules |
| [`routing.md`](routing.md) | **Manage Routing, verbatim** — three modes, nine workloads, the per-workload dialog |
| [`known-defects.md`](known-defects.md) | Bugs in the design we are borrowing from, and why we are not inheriting them |
| [`architecture.md`](architecture.md) | Module seams, and how each one is tested |
| [`staging.md`](staging.md) | The order the work lands in, and what is shippable at each step |

## The stance

**The catalogue and the two surfaces are a verbatim port.** The provider list,
its endpoints and auth styles, the three add-categories, the three routing modes,
the workload rows and the copy all come across as they are — see
[`catalogue.md`](catalogue.md) and [`routing.md`](routing.md). They are the
result of several rounds of real bugs, and reinterpreting them re-earns those
bugs.

What is *not* copied is the parts where our constraints differ — multi-tenant
scoping, the credential seam, vocabulary discovery — and the parts where
openhuman is simply wrong, which are enumerated in
[`known-defects.md`](known-defects.md) with a fix attached to each.

## Why

A company reaches exactly one model provider. The console surface for it is one
form — pick a provider, type a URL, type a key, Save
(`frontend/src/views/connections/InferenceSection.tsx`). The form calls itself a
"switch to" form in its own comments, and that is accurate: there is no list of
what is connected, because at most one thing ever is.

Three consequences an operator meets:

- **One credential slot per company.** `inference/key` holds whatever the
  *currently selected* provider wants. Switching provider without re-entering a
  key leaves the old vendor's credential in place, and the first turn fails with
  a 401 the host has to explain in a paragraph
  (`src/server/ops/inference.rs`, `probe_failure`). The console warns about this
  in help text because it cannot prevent it.
- **No second opinion.** One endpoint, one attempt. A 401, a 429, a 500 or a
  timeout ends the turn. There is no secondary provider and no degraded model;
  this was searched for specifically and does not exist.
- **No way to keep two accounts.** The provider is a string from a closed
  three-element list, not an identified record, so "my OpenRouter account" and
  "the team's OpenRouter account" cannot both exist.

Meanwhile a working design for exactly this problem exists next door, in
openhuman, and has been through several rounds of the bugs we would otherwise
discover ourselves.

## The target, in one picture

```
┌──────────────────────────────────────────────────────────────────────────┐
│  Connections → API Keys → LLM                                            │
├──────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│   ┌─ Providers ──┬─ Routing ─────────────────────────┐   ← two views,    │
│   │              │                                   │     one subject   │
│                                                                          │
│   Connected                                      [ + Add provider ]      │
│   ─────────────────────────────────────────────────────────────────      │
│   ⬤  Managed (TinyHumans)      billed to your account         ┃ On ┃    │
│   ⬤  OpenRouter                •••• configured   ✓ ok   ┃   ▣ on   ┃ ⋯   │
│   ⬤  Acme gateway   (custom)   api.acme.dev/v1   ⚠ 401  ┃   ▣ on   ┃ ⋯   │
│   ⬤  Ollama         (local)    127.0.0.1:11434   ✓ ok   ┃   ▣ on   ┃ ⋯   │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
```

Four things change:

1. **A company may hold several providers**, each an identified record with its
   own credential slot, its own endpoint, and its own enabled state.
2. **Adding one is a modal**, not a form the page rests on — three categories
   (cloud, local, delegated) because they ask three different questions.
3. **Connecting tests the credential before committing to it**, and classifies
   the failure rather than reducing it to a boolean.
4. **Routing becomes addressable**: a tier resolves against a *named* provider,
   so a company can put reasoning on one account and everything else on another.

## What must not regress

These are properties the current code has, several of which are better than the
design we are borrowing from. Losing one of them is a failed rework, however good
the new surface looks.

**Credentials never cross a wire.** `InferenceDecl` derives no `Serialize`, its
credential field is private, its `Debug` prints `<redacted>`, and the read DTO
carries `keyConfigured: bool` and nothing else. Two tests pin it, one of which
drives a real BYOK token through `PUT`, `GET` and `POST …/test` and asserts no
response body contains it. **The easiest way to regress this is to derive
`Serialize` on a new provider record for convenience.** Do not.

**Credentials are read per request, not captured at boot.** That is what lets the
managed tier be a rotating platform token. A provider list must not turn into a
list of captured secrets.

**Endpoint vocabulary is discovered, not assumed.** `TierVocabulary` reads
`{base_url}/models` and classifies the endpoint as tier-native, concrete (with a
per-tier bitmask), or unknown — and substitutes per tier, not per endpoint. It
exists because conflating *who pays* with *what vocabulary the endpoint speaks*
broke in production twice. openhuman has no equivalent. **Keep it.**

**Failure states are named for their remedy.** `CognitionState` has five states,
`RunnerGap` three, `InferenceResolution` three. Each pair exists because
collapsing it told an operator to do something that would not help — most
sharply, "we cannot read your config" must never render as "you have no config",
because a settings link there is the switch that does nothing.

**Authority is in the type system.** `AdminScopedCompany` in the handler
signature, not a check in a body, so the guard cannot be lost in an edit. The
axis is "does this decide something for the company", not read-versus-write.

**A console switch takes effect on the next turn.** `TenantProvider` bakes no
config; it re-resolves every invoke. A rework that builds a routing table at
startup loses this, and gets `restartRequired` on every change instead.

## What this is not

Not a change to what a tier *means*, not a new brain, and not a host feature for
defining harnesses from the console. Where the work touches the half-built
per-harness seam it says so explicitly (see [`data-model.md`](data-model.md)),
rather than quietly completing it.
