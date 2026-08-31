# Inference providers

*Which models a `built_in` harness reaches, and who pays.*

Terms: [glossary](../glossary.md). Which harness consults this at all is
[harnesses.md](harnesses.md); credential doctrine is
[credentials.md](credentials.md).

---

## The provider set

| provider | endpoint | credential |
|---|---|---|
| `openrouter` | dual-mode, below | optional tenant `sk-or-…` |
| `openai_compatible` | required `base_url` | usually a key |
| `ollama` | `base_url`, defaulting to a local server | none |

`openrouter` is the default. There is no provider for OpenCompany's own models,
because OpenCompany does not host models — the spec non-goal "not a model host"
is load-bearing here, and the provider list is where it shows.

### `managed` is gone

`managed` named the hosted TinyHumans brain and addressed proprietary SKUs.
OpenCompany no longer exposes its own models, so there is nothing left for a
distinct kind to name.

A manifest or stored runtime blob still saying `managed` **aliases** to
`openrouter` rather than failing. It named a real thing when it was written, and
the intent — "the platform's brain" — is exactly what proxied OpenRouter is.
Rejecting it would break bundles that were valid, to no purpose. An *unknown*
provider is a different matter and fails loudly; see below.

---

## `openrouter` is dual-mode

Which mode a company is in depends only on whether it holds a key:

| tenant key | endpoint | credential | telemetry slug |
|---|---|---|---|
| **absent** | the platform endpoint | the platform token | `subscription` |
| `sk-or-…` | `https://openrouter.ai/api/v1` | the tenant's key | `openrouter` |

**Keyless is the default a company starts on**, and it must be a working config
rather than a prompt for a credential: the platform proxy fronts OpenRouter
upstream and meters the spend against the subscription. From the workload's
point of view the two endpoints serve the same catalogue; only who pays differs.

The keyless branch inherits the platform's base URL *and* credential. Without
that inheritance a company naming a provider but holding no key of its own would
401 instead of riding the subscription — which is why the branch survived
`managed`'s removal rather than being deleted with it.

`InferenceDecl::is_proxied()` records which mode resolved.

### Setting a key moves you off the proxy

A stored key is an *OpenRouter* key, so it goes to OpenRouter. Sending an
`sk-or-…` to the platform proxy would simply be rejected.

> **Behaviour change.** Under `managed`, a console-set key kept the platform
> endpoint, so an admin could bill their own account through the proxy. That
> combination no longer exists. An admin who wants the platform endpoint with a
> credential of their own names `openai_compatible` with that `base_url`.

Clearing the key returns the company to the subscription rather than 401ing,
which is what makes a key genuinely optional in both directions.

---

## Per-harness configuration

Each `built_in` harness owns its provider, credential slot and model map, so two
harnesses on one company can hold two different OpenRouter accounts. A harness
declaring no `[harness.inference]` falls back to the company-level
`[inference]`.

### Secret slots

```text
inference/config                        # the DEFAULT harness
inference/key

harness/<id>/inference/config           # every other harness
harness/<id>/inference/key
```

The default harness keeps the **flat legacy keys**. This asymmetry is
deliberate: a tenant's stored console override and credential already live at
the flat paths, and the `SecretStore` port has no rename
([ports.md](ports.md)) — so namespacing every harness would silently orphan the
configuration of every company already running.

### Precedence, within a harness

1. **Runtime** — what the console wrote, in that harness's `…/config` slot.
2. **Manifest** — its `[harness.inference]`, else the company's `[inference]`.
3. **Default** — the platform-injected endpoint and token.

Unchanged from the single-provider design; what differs per harness is only
*which* slots tiers 1 and 2 read.

The credential is resolved **per request**, not captured at boot, because a
hosted tenant's platform credential is a projected token the platform rotates in
place. A value captured once would go stale within minutes. The same deferral is
what makes a console key rotation reach agents on their next turn with no
restart.

### Credentials are never inline

`api_key_secret` names a `SecretStore` key. It is never the token. Validation
rejects a value that looks like a pasted credential, so a secret cannot land in
a committed manifest. `InferenceDecl` derives no `Serialize` and its `Debug`
redacts the credential; no read route returns it, and the console sees only a
`keyConfigured` boolean.

This slot is **not** the company's TinyHumans identity — that is
`tinyhumans/key`, and the distinction is spelled out in
[credentials.md](credentials.md). This one is provider-scoped: whatever the
declared provider wants. Handing it to the TinyHumans backend would present one
vendor's credential to another.

---

## Models

Agents address workloads by abstract **tier** — `chat-v1`, `reasoning-v1`,
`agentic-v1`, `vision-v1` — derived from the agent's `tier` field. A tier names
a workload, never a model, which is what lets an agent keep its tier while
moving between harnesses.

What goes on the wire differs by path, and sending the wrong one fails
(`inference::model_for_tier`):

| path | wire value | why |
|---|---|---|
| proxied | the tier name (`chat-v1`) | the platform's registry routes on it, pinning each tier to a sub-provider so its rate card stays exact |
| direct | a concrete slug (`anthropic/claude-sonnet-5`) | OpenRouter has never heard of `chat-v1` |

On the direct path an unmapped tier takes `DEFAULT_TIER_MODELS`, which mirrors
the platform's own OpenRouter bindings — so both paths reach the same models and
adding a key does not silently move a company onto different ones.

A harness's own `models` entry is honoured **verbatim on both paths**: the
operator named a specific model, and rewriting it is not ours to do.

### Naming a specific model on the proxied path

The platform endpoint does accept a concrete model, but under its own
`openrouter/<author>/<slug>` namespace — an explicit prefix, so an arbitrary
caller string can never reach an upstream URL — and only when passthrough is
switched on there, which is **opt-in and off by default**. It prices such a
request from OpenRouter's live catalog and caps upstream spend at that rate.

So a bare tier is the only value that always works proxied. An operator who
wants a specific model through the proxy writes the `openrouter/…` form into
`models` themselves, and it is forwarded untouched.

---

## Model catalog

`GET {scope}/inference/models` lists OpenRouter's public model registry for
the console's picker — the operator-facing complement to `DEFAULT_TIER_MODELS`
above, used when an operator wants to pick a *specific* model rather than
accept a tier default.

The route is authenticated like the rest of `{scope}/inference/*` but does not
read the calling company's own config: the registry is a public, per-process
resource shared by every tenant on the host, not something scoped per company.

| property | value |
|---|---|
| cache lifetime | 1 hour (`MODEL_CATALOG_TTL`) |
| fetch timeout | 10 seconds (`MODEL_CATALOG_TIMEOUT`) — a console page-load waits at most this long on a cold cache, whatever its position in a `fetch_lock` queue |
| concurrent misses | coalesced onto a single upstream fetch (`ModelCatalogCache::fetch_lock`) — after startup or a TTL expiry, several console requests arriving together do not each fire their own OpenRouter call. The lock wait and the fetch share one timeout budget per caller, so a caller queued behind others during an outage is not left waiting `N × MODEL_CATALOG_TIMEOUT` for its turn to fail too |
| malformed entries | skipped individually rather than failing the whole response, so one bad record does not hide every valid model the registry returned |
| response ordering | sorted by model id, distinct from the provider order `discover_models` preserves for the local/custom setup probe |

A registry fetch failure (timeout, non-2xx, an empty catalog) surfaces as an
error on this route rather than silently returning stale or empty data — the
picker shows a failure state instead of an incomplete list that looks
complete.

---

## Outbound headers

| header | when | why |
|---|---|---|
| `HTTP-Referer`, `X-Title` | every `openrouter` request | OpenRouter's own dashboard and rankings |
| `x-sdk-name` | **proxied only** | our endpoint, our telemetry |

The product-identity header is keyed on `is_proxied()`, not on the provider
kind. After `managed`'s removal the kind no longer distinguishes our endpoint
from OpenRouter's — the same `openrouter` kind reaches both — and it is the
endpoint, not the vocabulary, that this rule is about.

It must never reach a third party. A tenant's own OpenRouter account, a
self-hosted OpenAI-compatible server and a local Ollama all belong to operators
who have no relationship with TinyHumans and gain nothing from learning which
product a tenant runs.

---

## History protection

The managed inference profile advertises a **context window**
(`max_input_tokens`) because openhuman's turn loop engages
`ContextCompressionMiddleware` and `ImageAwareMessageTrimMiddleware` only for a
positive window. Without one, intra-turn history grows unbounded across many tool
iterations, and an oversized request to a hosted model can fail *silently*:
HTTP 200, `finish_reason: "failed"`, an empty message, and zero usage — no
diagnosable provider error.

The advertised window defaults to 240,000 tokens and is configurable through
`OPENCOMPANY_CONTEXT_WINDOW` (see [config.md](config.md)). Compression and
deterministic trimming activate at 90% of the window (`window - window / 10`), so
the estimated budget stays under the model's hard limit; the full derivation is
documented in `src/harness/built_in/provider.rs`.

An operator using a model with a smaller advertised window must lower the
threshold to that window (with an estimation margin) rather than trusting the
default, or requests can exceed the provider limit before compaction engages.
`OPENCOMPANY_CONTEXT_WINDOW=off` or `0` restores the previous unbounded behavior
for a model that tolerates arbitrarily long turns.

---

## Unknown providers fail loudly

An unrecognised kind is an error at resolution, not a fallback. The manifest
validator already rejects one, but a **stored runtime blob never passes through
it** — the console wrote it, possibly under an older build whose vocabulary
differed. Resolving one silently would attribute its spend to whatever the
fallback happened to be, hiding the misconfiguration behind a plausible bill.

For the same reason `provider_slug` reports `unknown` rather than folding an
unrecognised kind into a real provider's attribution.

---

## Telemetry

Usage samples carry the slug of the config that actually served the turn, read
live after each turn rather than baked at build — so a console key switch
re-attributes spend on the *next* turn. With named harnesses this is per agent,
so a Usage view separates what each harness spent.

`subscription` and `openrouter` stay distinct slugs because they are two
different payers, and merging them would tell the operator nothing.

---

## Implementation map

| concern | where |
|---|---|
| provider vocabulary, defaults | `src/company/types.rs` |
| resolution, scoping, aliasing | `src/company/inference.rs` |
| the chat models and request plan | `src/harness/built_in/provider.rs` |
| read/write plane | `src/server/ops/inference.rs` |
| model catalog discovery, cache, single-flight | `src/server/inference_models.rs` |
| the subscription proxy itself | the TinyHumans backend |
