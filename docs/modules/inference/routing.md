# Manage Routing

Ported **verbatim** from openhuman at `5e543a76b` — the three modes, the nine
workloads, the copy and the mechanics. Source:
`app/src/components/settings/panels/ai/aiPanelTypes.ts` and the routing tab in
`AIPanel.tsx`.

## The tab, as shipped

`LLM Providers | Routing` — two pills, and the tab **ids** are unchanged
(`connect`, `routing`) because `#/connections/inference` is linked from the chat
pane's "cannot reach a model" banner and from workflow run rows. Relabelling a
tab is not a reason to break a link.

```
  ┌────────────────────────────────────────────────────────────────────┐
  │ Routing mode                                                       │
  │ ⬤ Managed                                              ┃ On ┃      │
  │ ○ Use Your Own Models                                              │
  │ ○ Advanced                                                         │
  └────────────────────────────────────────────────────────────────────┘

  ┌─ Advanced ─────────────────────────────────────────────────────────┐
  │ Chat            Direct conversational…   Primary (OpenRouter)  [⌄] │
  │ Reasoning       Main chat agent…         Acme · gpt-5          [⌄] │
  │ Agentic         Sub-agent runners…       Managed               [⌄] │
  │ Vision          Image understanding…     Primary (OpenRouter)  [⌄] │
  │ ───────────────────────────────────────────────────────────────── │
  │ Coding          Code generation…    Follows Agentic — one tier,    │
  │                                     two names          (read-only) │
  └────────────────────────────────────────────────────────────────────┘
```

**An unset row names what it will actually use** — `Primary (OpenRouter)` — not
"No model selected". An unset row is not a gap: it resolves somewhere, and
naming where is the difference between a screen that reports routing and one
that hides half of it. It is read through `primary()` on every render and never
cached, so the rows move when the marked default does.

The per-workload dialog carries the workload's recommendation hint, a provider
select whose first two items are the primary and Managed (an absence and a
choice, which are different states), a free-text model id, and **Test** — the
one control in this surface that sends a real completion, with the cost warning
on the button rather than above the fold.

The model field is **a catalog select with a free-text escape hatch**, not one or
the other. The catalog is what makes the screen usable — an operator who has to
know a vendor's id scheme by heart is being asked the wrong question — and the
escape hatch is what keeps it correct.

It is sourced **per provider** (`GET …/inference/providers/{slug}/models`), not
from one global list: two providers are two catalogs, and the stored key is
presented host-side so the console never sees it.

Four states, each said out loud:

| Catalog | Field | What the line under it says |
|---|---|---|
| loads | select | *Enter a model id instead* |
| still loading | text | *Reading this provider's models…* |
| empty, 404, or not OpenAI-shaped | text | why, naming the endpoint |
| Azure host | text | it routes on a deployment name |

Azure is the reason the escape hatch exists rather than the reason the select
does not: the request keys on a **deployment name** while `/models` publishes
**base model ids**, so there the only correct value is one the catalog can never
contain. That is one endpoint family, not an argument against catalogs — and a
catalog can be stale or incomplete anywhere, so the toggle is always one click
away.

**Blank stays meaningful.** Empty means *send the tier and let the endpoint
resolve it*, which is how `TierVocabulary` passthrough works and is the right
answer at a tier-native endpoint. It is an item in the list with its own label,
never an absence to be guessed at.

And nothing typed is ever discarded: the field starts as text and upgrades to a
select when the list lands **only if nothing has been typed**. Changing a field's
shape under a value someone is mid-way through entering is the same class of bug
as stripping one mid-keystroke, which this feature's proxy rule has nine recorded
instances of.

## Three modes

```
┌─ Routing ───────────────────────────────────────────────────────────────────┐
│                                                                             │
│  ⬤ Managed                                                     ┃ On ┃       │
│    OpenHuman will run all inference in the cloud, choose the best model      │
│    for the task, optimize for cost, and keep the safest routing defaults.    │
│                                                                             │
│  ○ Use Your Own Models                                                      │
│    Choose one provider + model and route every workload through it. This    │
│    is simple, but it can be inefficient because lightweight and heavyweight │
│    inference all share the same route.                                      │
│                                                                             │
│  ○ Advanced                                                                 │
│    Pick different models for different tasks. This is the best option for   │
│    tight cost optimization and the most control.                            │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

**The mode is inferred from the data, never stored.** There is no mode field to
drift out of sync with the routes:

```
inferRoutingMode(routing):
    refs = the nine workload refs
    if every ref is 'openhuman' or 'default'        -> managed
    if every ref has the same provider+model         -> own
    otherwise                                        -> custom
```

Nine fields, one derived mode. A mode field would be a tenth thing that can
disagree with the other nine.

## The nine workloads

Two groups. Port the labels, the descriptions **and** the recommendation hints —
the hints are the part that makes the screen usable by someone who does not
already know which model to pick.

### Chat and Conversations

> Models used during direct user interaction, replies, reasoning, agent loops,
> and coding help.

| id | Label | Description | Recommendation hint |
|---|---|---|---|
| `chat` | Chat | Direct conversational back-and-forth: "Quick" mode in Conversations | a cheap or mid-cost fast chat model with high tokens/sec and low latency. Open-source local models can work well here if they feel responsive. |
| `reasoning` | Reasoning | Main chat agent, meeting summarizer: "Reasoning" mode in Conversations | a more expensive frontier or strong reasoning model for deep thinking. Used for the main chat agent, meeting summaries, and heavier answer synthesis. |
| `agentic` | Agentic | Sub-agent runners, tool loops, GIF decisions | a reliable instruction-following model with strong tool use. Mid-cost frontier models are usually safest; capable open-source models can work if tool calling is stable. |
| `coding` | Coding | Code generation and refactor passes | a coding-tuned model with strong instruction following, edit quality, and long-context performance. Usually worth spending more on. |
| `vision` | Vision | Image understanding for the vision sub-agent: always multimodal | a multimodal model that accepts image input. The managed default is image-capable; any provider routed here is always treated as vision-enabled. |

### Background Tasks

> Models used outside the main conversation flow for summarization, heartbeat,
> learning, and subconscious evaluation.

| id | Label | Description | Recommendation hint |
|---|---|---|---|
| `memory` | Memory summarization | Tree-extracts and consolidations | a cheaper summarization model. Consistent and compact, but it does not need premium frontier-level reasoning. |
| `heartbeat` | Heartbeat | Background reasoning between user turns | a cheap, efficient background model. This runs often between turns, so low cost matters more than maximum intelligence. |
| `learning` | Learning · Reflections | Periodic reflection over recent history | a stronger reflective model. Can be mid-cost or premium because it benefits from better synthesis over recent history. |
| `subconscious` | Subconscious | Eventfulness scoring + drift checks | a very cheap monitoring model, lightweight and predictable. For eventfulness scoring, drift checks, and quiet background evaluation. |

## What a row can point at

```rust
enum ProviderRef {
    Openhuman,                                  // explicitly managed
    Default,                                    // unset — falls through to managed
    Cloud { provider_slug, model, temperature },
    Local { model, temperature },
    ClaudeCode { model, temperature },
}
```

`Openhuman` and `Default` are different states on purpose: one is a choice, the
other is an absence. Collapsing them loses the ability to say "this row is
deliberately managed" versus "this row was never set".

## Advanced mode — the row

```
┌─ Advanced ──────────────────────────────────────────────────────────────────┐
│  Fine-grained routing gives you the best cost optimization and the most      │
│  control. Use the rows below to decide which workloads stay Managed, which   │
│  use your shared default, and which pin to a specific model.                 │
│                                                                             │
│  Chat and Conversations                                                     │
│  ─────────────────────────────────────────────────────────────────────────  │
│  Workload                                             Model                 │
│  Chat                                                                       │
│    Direct conversational back-and-forth…        [ Managed    ] [Change Model]│
│  Reasoning                                                                  │
│    Main chat agent, meeting summarizer…         [ Acme·gpt-5 ] [Change Model]│
│  …                                                                          │
│                                                                             │
│  Background Tasks                                                           │
│  ─────────────────────────────────────────────────────────────────────────  │
│  Memory summarization                                                       │
│    Tree-extracts and consolidations             [ No model   ] [Choose Model]│
│  …                                                                          │
└─────────────────────────────────────────────────────────────────────────────┘
```

The button reads **Change Model** when one is set and **Choose Model** when none
is; the value column reads **No model selected** when unset.

## Use Your Own Models — the shared row

> Choose one model for everything. This routes all inference through one model.
> It is simpler, but it can be inefficient for cost and quality because
> lightweight and heavy tasks will all use the same route.

Two selects — Provider, then Model — and a line stating exactly what it covers:

> Applies the same provider + model to chat, reasoning, coding, memory,
> heartbeat, learning, and subconscious. Embeddings are configured separately.
> Changes save when you click save.

With no providers connected it says so rather than showing empty selects:

> Add or connect a provider first. Then you can route every workload through one
> model here.

## Managed stays a fallback, always

> Managed is always available as a fallback. To use your own model, choose a
> routing mode below.

It renders a **badge**, not a disabled toggle. openhuman's note on why, which is
worth keeping: a locked switch reads as switchable-but-broken and invites a fight
the user cannot win.

What the badge **says** is not ported. openhuman's reads `Always on`, which is
true there — they run the managed backend. Ours needs a credential and can
resolve to nothing, so the row reports which step of the credential chain
answered, and shows no badge at all when none did. See
[`connect-flow.md`](connect-flow.md).

## The per-workload dialog

Opened by Change Model / Choose Model. It carries the workload's recommendation
hint, a provider select, a model select sourced from that provider's `/models`,
a free-text escape hatch (**Enter model id**), and a **Test** button that sends
one real one-turn completion for that workload and reports the result inline.

openhuman tests here with a real completion rather than a catalog listing,
because the add flow wants "is this reachable" and a routing row wants "will this
model actually answer".

**What shipped does not send a completion, and no longer claims to.** The button
called the per-provider check, which is `GET /models` — so it cost nothing,
charged nothing, and could not tell `this-model-does-not-exist` from a good id:
it answered "Reached the provider." either way. That is a true sentence about a
question nobody asked, and it is worse than a plain failure, because the operator
has been told the row is fine.

The check now carries the row's chosen model and reports whether the endpoint
publishes it, and the copy says what it actually does — free, no turn. An id the
catalogue does not list is a **caution, not a failure**: an Azure deployment name
is never published by design, and failing it would make the only correct value at
that endpoint unreachable.

A real completion remains the stronger check and is the open decision here: it
needs the `openhuman`-gated provider, which the default test lane does not
compile, so it wants a feature-lane row of its own rather than a quiet `#[cfg]`.

### The select lists providers, and unset is a state

The select offers **providers only** — Managed, and each connected provider.
Three connected, three options. It used to list five: the unset row's own
display (`Primary (OpenRouter)`) sat in the list beside the real `OpenRouter`
row, so the primary appeared twice under two names that behaved differently, and
Managed had a second identity of its own beside any `tinyhumans` provider record.

`ProviderRef::Default` is not a list entry, because it is not a provider. It is
the row's **unset state**, which the Routing tab already renders as
`Primary (OpenRouter)` — that display is good and stays. Getting back to it is an
**action** under the select, not an option inside it, and it clears the model
with it (a model pins the row, so leaving one behind would put it straight back).

### The Model id field depends on the kind of provider

`modelTarget` is the one function that decides whether the field appears, and it
resolves an unset row to the provider it actually uses first. So
`Primary (OpenRouter)` and `OpenRouter` produce the same field, by construction
rather than by two branches being kept in step. It answers `null` for Managed
under either of its names, because the route grammar has no managed-plus-model
form and a field there could only be discarded on save.

Choosing a model while the row is unset **pins it** to the provider the default
currently resolves to, and the dialog says so. "Follow the default, but with this
model" is not expressible in the grammar, and silently dropping the model is the
worse of the two answers.

## Removing a provider scrubs the routes pointing at it

When a provider is removed, every workload pinned to it resets to
`{ kind: 'default' }`. The matching rule differs by kind and all three cases are
real bugs openhuman fixed:

- **Cloud / custom** — matched precisely by `providerSlug`.
- **Claude Code** — its refs carry no `providerSlug`. Without special handling,
  disconnecting it left workloads pinned to `claude-code:<model>`, which the
  factory still honours, so chats kept using the CLI after the provider was
  removed.
- **Local runtimes** — their refs carry no slug either, so a `local` ref is only
  definitively orphaned once **no** local runtime remains enabled. Before this
  helper the local case was silently a no-op.

And a second, independent mechanism at load: a config migration reconciles
routes naming a provider that no longer exists. Two mechanisms for one invariant,
because the UI path can be bypassed by a config edit or an older build — and an
unresolvable route **hard-errors that workload's inference** rather than falling
back.

## The primary is chosen, not inherited from list order

An unset workload resolves through the **primary**. Which provider that is used
to be answered by list order — the first enabled one — and that is a default
nobody said. Add three providers, delete the first, and the company's unrouted
spend moves to a different account with nothing on screen having changed.

So there is an explicit marker: one slot (`inference/default`) holding one slug.
**Two defaults are not representable**, because one slot cannot hold two slugs —
"setting a default clears the previous one" is the storage shape rather than an
operation that could be forgotten.

```
primary(providers, marked):
    the marked provider, if it exists and is enabled
    else the first enabled provider          ← every company that has not said
    else None                                ← the managed brain, always available
```

No migration and no backfill: an unmarked company behaves exactly as it did.

Three rules the write paths keep:

- **A disabled provider is never the default.** Disabling it **clears** the
  marker rather than moving it to the next enabled provider — moving it would
  mark something the operator never chose, which is the positional default this
  replaces.
- **Deleting the default clears the marker**, in the same operation that scrubs
  the routes pointing at it, and for the same reason.
- **A stale marker falls back rather than stranding the company.** A route the
  operator *did* set fails closed when it names a provider that is missing or
  off, because that is a choice with a workload attached; an unset workload has
  no such choice behind it, and the alternative to falling back is a company
  that cannot think because of a marker it forgot about.

On screen: the Providers tab marks one row as the default and offers a menu item
to move it. The Routing tab's unset rows read `Primary (OpenRouter)` — resolved
through `primary()` on every render, never cached, so they move when the marker
does.

## No workload inherits another's route

openhuman shipped the opposite and had to undo it:

> Setting only `coding_provider` used to move `chat` and `reasoning` onto that
> key too — ordinary conversations silently billed to the user's own account,
> with no settings field saying so.

An unset workload resolves through the primary, never through a sibling's
configured provider. Two deliberate **aliases** survive that change and are not
the same thing: `burst` reads the agentic route, and `summarization` reads the
memory route. Those are two names for one configured route, which is different
from an unset route borrowing a set one.

## How a route reaches the turn

A routing table nothing reads is a screen that persists choices and changes
nothing — which is what shipped first, and it is worse than a control that
visibly fails: the value stuck across a reload, the tab reported success, and
every turn kept going to the primary. Nothing looked wrong.

The read happens in `inference::resolve_effective_for_tier`, which the tenant
provider calls **per turn** with the abstract tier that turn carries. The order
is:

1. **A tier with no row** — anything outside `ROUTABLE_WORKLOADS` — resolves
   exactly as it did before routes existed. It does not acquire a route by
   accident, and it does not fail closed for want of one.
2. **`Primary`** (the row is unset) falls through to the whole existing chain:
   the provider list, then `inference/config`, then the manifest, then the
   platform default. A company that has never opened this tab resolves where it
   always did.
3. **`Managed`** resolves through the managed credential chain, not as a provider
   slug. `managed` is a word in the route grammar — it is what the Managed mode
   button writes into every row — and reading it as a slug would fail closed
   against a provider the operator never had.
4. **`Resolved`** uses that provider's record, except for **entry zero**, which
   resolves through the legacy chain instead: entry zero *is* the legacy blob
   wearing a provider record's clothes, and the record carries none of the proxy
   inheritance or unknown-provider rejection that chain holds.
5. **`Missing` and `Disabled` fail closed**, with the workload and the slug in
   the message. An unset workload falls back because nobody chose anything for
   it; a route is a choice with a workload attached, and quietly moving it to a
   different account is the failure the explicit default marker exists to
   prevent, wearing a different hat.

A route's **pinned model** is written into the decl's tier map for that tier
alone, so `model_for_tier` reads it first and the provider's own map stays the
default for every other workload. Pinning one row must not move the others — a
fix that routed everything through the chat row would satisfy the obvious test
and be worse than the bug it replaced.

## Mapping onto OpenCompany

openhuman's nine workloads do not map one-to-one onto our four abstract tiers
(`chat-v1`, `reasoning-v1`, `agentic-v1`, `vision-v1`). The port has a real
decision to make here, and it should be made explicitly rather than by
coincidence:

| openhuman workload | Nearest OpenCompany tier |
|---|---|
| chat | `chat-v1` |
| reasoning | `reasoning-v1` |
| agentic | `agentic-v1` |
| coding | `agentic-v1` (no distinct tier today) |
| vision | `vision-v1` |
| memory, heartbeat, learning, subconscious | **no equivalent** — these are background loops OpenCompany does not run |

Two honest options:

1. **Ship five rows** — the tiers that exist — keeping openhuman's grouping,
   labels, hints and dialog exactly. The Background Tasks group is omitted until
   there are background loops to route.
2. **Add the tiers first**, then ship all nine.

Option 1 is the one this plan takes: the routing *surface* is ported faithfully,
and the row count follows what the runtime actually has. Shipping four empty
background rows would be a screen that lies about what the product does.

Everything else — the three modes, the inferred mode, the `ProviderRef` union,
the row layout, Change/Choose Model, the per-workload dialog with its Test, the
scrub-on-remove with all three matching rules, and the no-inheritance rule —
ports as-is.

## What does not change on our side

- **Per-request re-resolution.** A console change lands on the next turn with no
  rebuild. A routing table built at startup would turn every change into
  `restartRequired`.
- **`TierVocabulary` discovery, per provider.** Each entry classifies its own
  endpoint. openhuman has no equivalent; a faithful port would drop it.
- **An operator override wins in every vocabulary.** A typed model id is honoured
  verbatim whatever the endpoint publishes.
- **`agent.tier` stays the per-agent knob.** A tier names a workload, never a
  model.
