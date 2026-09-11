# Current state

What exists today, read from the code rather than from intent. This file is here
so the rework is measured against the real thing — including the parts that are
already better than what we are borrowing from, which are the parts easiest to
break by accident.

## The three records

There are three, deliberately different shapes, and confusing them is the first
mistake available.

```
company.toml  [inference]         ──┐
  provider / base_url               │  declarative intent, committed,
  api_key_secret  (a KEY NAME)      │  never a credential
  models { tier -> model }        ──┘
                                          ┌──────────────────────────┐
SecretStore  "inference/config"   ──┐     │      InferenceDecl       │
  { provider, base_url, models }    ├────▶│  resolved, in memory,    │
  written by the console            │     │  credential PRIVATE,     │
SecretStore  "inference/key"        │     │  no Serialize derive     │
  the bearer, write-only          ──┘     └──────────────────────────┘
                                                      ▲
env  OPENCOMPANY_INFERENCE_KEY / _URL ────────────────┘
     (or TINYHUMANS_TOKEN_FILE / _API_KEY)   lowest precedence
```

Precedence is three `if` blocks in `resolve_effective_scoped`
(`src/company/inference.rs`): runtime blob, then manifest, then the env default,
then `Ok(None)` — which lands the company on the echo brain.

## What an operator can do today

One form, four controls, one Save:

```
┌─ Connect ─────────────────────────┬─ Manage Routing ──────────────────┐
│                                   │                                   │
│  Provider   [ OpenRouter      ▾]  │   chat-v1       [ ...          ▾] │
│  Base URL   [ ................ ]  │   reasoning-v1  [ ...          ▾] │
│             (only if required)    │   agentic-v1    [ ...          ▾] │
│  API key    [ •••••••••••••••• ]  │   vision-v1     [ ...          ▾] │
│             (only if accepted)    │                                   │
│                                   │                                   │
│       [ Save ]  [ Reset ]  [ Remove key ]     ← one draft, one Save    │
└───────────────────────────────────────────────────────────────────────┘
```

Both tabs are **one mounted component**. That is deliberate: a draft typed on
Connect survives switching to Manage Routing, because one Save writes both. The
comment in `InferenceView.tsx` says splitting them would discard half a draft.

**This is load-bearing for the rework.** A provider *list* is a different
interaction — per-row state, per-row save, per-row test — and roughly none of
`InferenceSection.tsx` survives it. In particular the `stripProxyIncompatible`
machinery (nine documented regressions, about two hundred lines of comment) is
written against a single global "is this draft proxied" boolean, which stops
being a single boolean the moment there are N entries.

## Two tier vocabularies

The most confusing thing in the current code, and it must survive the rework
intact.

```
agent.tier                         INFERENCE_TIERS              wire model id
(per agent, company.toml)          (the abstract workloads)     (what is sent)

  orchestrator ──┐
  frontend     ──┼──▶  agentic-v1   ──┐
  agentic      ──┘                    │
  reasoning    ─────▶  reasoning-v1   ├──▶  model_for_tier(tier, overrides,
  vision       ─────▶  vision-v1      │                     vocabulary)
  (anything else) ──▶  chat-v1      ──┘                │
                                                       ▼
       build.rs::model_for_tier                inference.rs::model_for_tier
       (baked at roster build)                 (resolved per request)
```

The second step is where the sophistication is. `model_for_tier` consults a
`TierVocabulary` **discovered from the endpoint's own catalog**:

| Vocabulary | Meaning | Behaviour |
|---|---|---|
| `Tiers` | the endpoint publishes `chat-v1` etc. | pass the tier name through |
| `Concrete(mask)` | it publishes real model ids | substitute a default **per tier**, only for tiers the mask says it serves |
| `Unknown` | it publishes neither | pass the tier through, so the provider's 400 names a string the operator can find |

An operator override always wins, in every vocabulary.

Two production incidents are recorded in the comments there, and both are the
reason this is not simpler:

- Vocabulary was once read off `is_proxied` — conflating *who pays* with *what
  the endpoint speaks*. A company pointing `openrouter` at the TinyHumans URL
  with its own key was not proxied, so every tier was rewritten to
  `anthropic/claude-opus-5` and the endpoint answered that the model was not
  available — for an endpoint whose catalog publishes `chat-v1` directly.
- Classification was once per-endpoint and substitution followed it. A partial
  mirror publishing only `anthropic/claude-sonnet-5` was classified `Concrete`
  and then handed `openai/gpt-5.6-sol-pro` for `reasoning-v1` — an id that
  catalog had just said it does not serve. Hence the per-tier bitmask.

## Credential handling, which is already good

This is the part the rework is most likely to regress and least likely to
improve. Four independent mechanisms, not one:

1. `InferenceDecl` derives **no** `Serialize`; the credential field is private.
2. `Credential`'s `Debug` prints `Credential(<redacted>)`.
3. The read DTO carries `key_configured: bool` — *"never the credential itself"*.
4. Two tests pin it, and the end-to-end one uses an **`openai_compatible` (BYOK)
   key**, not the TinyHumans one — so the property is verified for the arbitrary
   vendor case, which is the one that matters here.

Note there are **two** write-only credential slots per company, and they are not
interchangeable: `tinyhumans/key` is an identity (it brokers Composio and
billing), `inference/key` is a provider-scoped outbound bearer. The code
disclaims the relationship explicitly — handing one to the other would present
one vendor's credential to another.

## The half-built seam

`HarnessScope` namespaces storage per harness:

```
default harness   →  "inference/config"              ← flat, legacy, load-bearing
named harness     →  "harness/<id>/inference/config"
```

The read path is complete and tested, including a test asserting the default
harness still reads the flat keys. The reason is stated in the code and is the
single hardest constraint on this rework:

> a tenant's stored console override and credential already live at
> `RUNTIME_CONFIG_KEY` / `KEY_KEY`, and the store has no rename — so namespacing
> every harness would silently orphan the config of every company already
> running, which is the one migration this design cannot afford.

**The write path does not exist.** `save_runtime_config_scoped`,
`store_key_scoped` and `clear_runtime_config_scoped` have zero callers outside
their own module, and `HarnessScope::named` is used in exactly one place — lane
construction. `GET {scope}/harnesses` is read-only and carries no inference
fields.

So a second harness's provider can only come from a committed `[harness.inference]`
block. This is the largest "already exists, unfinished" surface the rework builds
on, and the plan must say whether it is completing it or working beside it.

## What is missing

Verified absent, not merely unfound:

- **No provider list.** One company, one provider.
- **No stable id.** A provider is a string from a closed three-element allowlist
  (`openrouter`, `openai_compatible`, `ollama`). The `slug` that exists is
  telemetry-only and derives *from* the kind. Two OpenRouter accounts cannot
  coexist.
- **No fallback chain.** One endpoint, one attempt. The only retry is
  same-endpoint, once, for the empty-response class.
- **No per-agent model override on the built-in path.** `agent.model` exists but
  the validator rejects it there. Every internal pass — planning, triage, title,
  selector, workflow build, profile draft, roster build — is hardcoded to
  `chat-v1`, with no override short of `OPENCOMPANY_INFERENCE_MODEL`, which
  flattens the entire roster.
- **No Anthropic or OpenAI provider kind.** Both are reachable only via
  `openai_compatible` plus a base URL, and Anthropic's native API is not
  OpenAI-shaped.
- **No stored health.** `POST …/inference/test` is a live probe whose result
  lives only in React state.

## Duplication that will drift

Provider and tier knowledge is written down in six places with no shared source.
Two have already drifted:

| Fact | Copies |
|---|---|
| Provider kinds | `types.rs` (`INFERENCE_PROVIDERS`), `InferenceSection.tsx` (`PROVIDERS`), `api/setup.ts` (`INFERENCE_PROVIDERS`) — three shapes, different field names |
| Abstract tiers | `types.rs` (`INFERENCE_TIERS`) and `InferenceSection.tsx` (`TIERS`) |
| OpenRouter attribution headers | `provider.rs` and `roster_build.rs` — **already divergent**, one copy stale |
| Wire provider union | `api/inference.ts` still carries `"managed"`, which the host's list dropped |

The rework should collapse these rather than add a seventh.

## Consumers a rework must not break

`InferenceStatus` is used as the "can this company think?" oracle by surfaces
that are not about inference at all:

- `SetupDialog` — decides whether to offer a tailored team
- `AgentDetailView` — gates the profile-draft copilot
- `CopilotPanel`, `WorkflowCreateDialog` — gate on `CognitionPath`
- `runner_gap_for` → workflow run refusals
- `cognition_state` → the chat banner

Changing the shape of that DTO breaks five call sites, three of which have
nothing to do with this page.
