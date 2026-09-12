# Composio: how a company connects its apps

How a company reaches Composio, and which credential that reach presents. This
folder is the design for the console surface over it; the runtime spec for what
the harness does with the result lives beside the other runtime docs.

| File | What it settles |
|---|---|
| `README.md` | The case for the change, the target shape, and what must not regress |
| [`data-model.md`](data-model.md) | The four stored slots, and why two credentials cannot be merged into one |
| [`resolution.md`](resolution.md) | **Which credential a call presents** — the managed chain, the BYOK short-circuit, and the #886 trap |
| [`connect-flow.md`](connect-flow.md) | Adding a key: the classified probe, and why this one validates a draft |
| [`architecture.md`](architecture.md) | Module seams, and how each one is tested |

## The stance

**This is a UI-consistency change, not a rebuild.** Composio already has the
right data model and has had it since issue #110. `src/company/composio.rs`
models exactly two mutually exclusive modes, stores the mode rather than
inferring it, and keeps the two credentials apart because they authenticate
different hosts. None of that changes here.

What changes is the console: the page is brought to the visual language of the
reworked LLM/inference surface — a Connected card of rows, each a mark, a name,
one sub-line and controls on the right — and the managed route is offered again.

## Why

Two problems, one of each kind.

**The page did not look like its neighbour.** The LLM surface was reworked into
two sober cards with a row per connected thing. Composio's page was a stack of
explanatory paragraphs above a form, every paragraph explaining a control that
was visible while it was being read. Same console, same kind of decision, two
different languages.

**Only one of the two routes was on offer.** `COMPOSIO_MANAGED_HIDDEN` was
`true`, so the console showed BYOK as the only choice. That flag was set when
choosing the managed route meant sending an operator off to another site to mint
a TinyHumans key by hand — a worse errand than pasting a Composio key, for a
credential most people did not have.

The company-key grant removed the errand. The inference rework had the identical
problem, flipped `INFERENCE_MANAGED_HIDDEN` to `false`, and wrote the reason down
four lines above this flag in the same file:

> "Managed (TinyHumans)" was hidden while choosing it meant going and minting a
> key by hand, which made OpenRouter the honestly easier option. With the grant
> it is one click, and it is the only option that also arms the company's
> connections in the same step.

That last clause is about **these** connections. The argument applies here more
directly than it did there, so the flag is flipped and both routes are offered.

## The target, in one picture

```
┌──────────────────────────────────────────────────────────────────────────┐
│  Connections → Apps                                                      │
├──────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│   Connected                                                              │
│   ─────────────────────────────────────────────────────────────────      │
│   ◼  TinyHumans (managed)                                   ┃ Active ┃   │
│      Billed to this company's TinyHumans account                         │
│                                                                          │
│   ◼  Own Composio key                            [ Use this ]            │
│      Not configured                                                      │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
```

## Rows, single-select — and why not a toggle

The inference page gives each row an independent toggle plus one `Default`
marker. That models providers which **coexist**: a company can hold OpenRouter
and Ollama at once and route different workloads to each.

Composio has no coexistence. `composio/mode` is a single stored scalar and
[`resolve_access`](resolution.md) reads exactly one branch of it. A toggle on
each of two mutually exclusive rows can reach states the model has no meaning
for — both on, both off — and a control that can reach a meaningless state is
the same defect as a control that cannot act. That rule is why the Managed
inference row has no toggle at all.

Two large selectable cards, which is what the page shipped before, model the
exclusion honestly but abandon the row language this change exists to adopt, and
leave nowhere comfortable for the managed row's four-way sub-line.

So: **keep the row language, replace the toggle with single-select.** The active
row carries an `Active` badge; the other offers `Use this`. Selecting a row is
selecting a mode, which is what the model actually says.

## What must not regress

**The two credentials stay apart.** `composio/token` is a bearer the *TinyHumans
backend* recognises; `composio/api_key` is a key *Composio* recognises. They
authenticate different hosts and are not interchangeable. Merging them into one
slot would send a credential to the wrong API, where it fails in a way that reads
like a bad credential.

**The mode stays stored, not inferred.** Inferring it from "is there an API key"
loses the distinction the slug is there to make, and would make the console
unable to report the mode without reading a secret slot.

**Credentials never cross a wire.** The read DTO deliberately carries no `token`
and no `tokenConfigured`, and tests assert their absence. Nothing holding a
credential derives `Serialize`. The easiest way to regress this is to add a new
record for convenience and derive `Serialize` on it. Do not.

**Reads stay open to members, writes stay admin-only.** Issue #403 settled that a
connection is company property, not the personal property of whichever member
clicked Connect. `AdminScopedCompany` sits in the write handlers' signatures so
the guard cannot be lost in an edit.

**BYOK fails closed.** A company in BYOK mode with no key gets no tools. It does
**not** fall back to the managed tiers — a company that asked to act through its
own Composio account and silently acted through the platform's would connect
providers into the wrong tenant and bill the wrong party.

## What this is not

Not a change to the data model, not a change to what the harness does with the
resolved credential, and not a new provider list. Composio needs no catalogue of
vendors, no tier vocabulary and no multi-step provider identity chain — those are
inference's problems, and importing them here would be symmetry for its own sake.
