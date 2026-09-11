# The provider catalogue

Ported **verbatim** from openhuman at `5e543a76b`. This is the list, the
endpoints, the auth styles and the copy — not a reinterpretation of them.

Sources, both of which must agree and today are hand-maintained copies of each
other:

- `vendor/tinymemory/crates/tinymemory-api/src/host/cloud_providers.rs` —
  `BUILTIN_CLOUD_PROVIDERS`, 27 entries (slug, label, endpoint, auth style)
- `app/src/components/settings/panels/builtinCloudProviders.ts` — the same list
  minus the first-party entry, plus `tone` and `keyPlaceholder`

When porting, generate one from the other or assert they match. openhuman does
neither, and it is listed as a defect for that reason.

## Cloud providers

26 user-addable entries. `auth` is the header style: `bearer` sends
`Authorization: Bearer <key>`; `anthropic` sends `x-api-key: <key>` plus
`anthropic-version: 2023-06-01`.

| # | slug | Label | Auth | Key placeholder | Endpoint |
|---|---|---|---|---|---|
| 1 | `openai` | OpenAI | bearer | `sk-...` | `https://api.openai.com/v1` |
| 2 | `anthropic` | Anthropic | **anthropic** | `sk-ant-...` | `https://api.anthropic.com/v1` |
| 3 | `openrouter` | OpenRouter | bearer | `sk-or-...` | `https://openrouter.ai/api/v1` |
| 4 | `orcarouter` | OrcaRouter | bearer | `sk-orca-...` | `https://api.orcarouter.ai/v1` |
| 5 | `gmi` | GMI | bearer | `eyJ....` | `https://api.gmi-serving.com/v1` |
| 6 | `fireworks` | Fireworks | bearer | `fw-...` | `https://api.fireworks.ai/inference/v1` |
| 7 | `moonshot` | Kimi (Moonshot) | bearer | `sk-...` | `https://api.moonshot.ai/v1` |
| 8 | `groq` | Groq | bearer | `gsk_...` | `https://api.groq.com/openai/v1` |
| 9 | `mistral` | Mistral | bearer | — | `https://api.mistral.ai/v1` |
| 10 | `deepseek` | DeepSeek | bearer | `sk-...` | `https://api.deepseek.com/v1` |
| 11 | `together` | Together AI | bearer | — | `https://api.together.xyz/v1` |
| 12 | `google` | Google Gemini | bearer | — | `https://generativelanguage.googleapis.com/v1beta/openai` |
| 13 | `cerebras` | Cerebras | bearer | — | `https://api.cerebras.ai/v1` |
| 14 | `xai` | xAI | bearer | — | `https://api.x.ai/v1` |
| 15 | `huggingface` | Hugging Face | bearer | `hf_...` | `https://router.huggingface.co/v1` |
| 16 | `nvidia` | NVIDIA | bearer | — | `https://integrate.api.nvidia.com/v1` |
| 17 | `zai` | Z.AI | bearer | — | `https://api.z.ai/api/paas/v4` |
| 18 | `minimax` | MiniMax | bearer | — | `https://api.minimax.io/v1` |
| 19 | `stepfun` | StepFun | bearer | — | `https://api.stepfun.ai/step_plan/v1` |
| 20 | `kilocode` | Kilo Code | bearer | — | `https://api.kilo.ai/api/gateway` |
| 21 | `deepinfra` | DeepInfra | bearer | — | `https://api.deepinfra.com/v1/openai` |
| 22 | `novita` | Novita | bearer | — | `https://api.novita.ai/v3/openai` |
| 23 | `venice` | Venice | bearer | — | `https://api.venice.ai/api/v1` |
| 24 | `vercel-ai-gateway` | Vercel AI Gateway | bearer | — | `https://ai-gateway.vercel.sh/v1` |
| 25 | `sumopod` | SumoPod | bearer | `sk-...` | `https://ai.sumopod.com/v1` |
| 26 | `modelscope` | ModelScope | bearer | `ms-...` | `https://api-inference.modelscope.cn/v1` |

The 27th Rust entry is `openhuman` (`https://api.openhuman.ai/v1`, auth style
`openhuman_jwt`) — their managed first-party backend, always present, never
removable, not offered in the add list. **Our equivalent is the managed
TinyHumans brain**, which is already modelled and must keep its own auth path;
do not port `openhuman` as a row of this table.

It **is** offered in the add dialog, though — as an entry the console injects
into the Cloud list rather than as a row of `CLOUD_PROVIDERS`, because it is a
resolution chain rather than a vendor account and has no preset endpoint of its
own. It appears there only while its chain resolves to nothing or to the
*instance's* identity; see [`connect-flow.md`](connect-flow.md) for why the
second case is deliberate.

### Endpoint paths are not uniform, and that is the point

Note how varied the base URLs are — `/openai/v1`, `/inference/v1`,
`/v1beta/openai`, `/v1/openai`, `/v3/openai`, `/api/paas/v4`, `/api/gateway`,
`/step_plan/v1`. This is exactly why the endpoint is a per-provider preset rather
than `https://{host}/v1`. Any attempt to derive it will be wrong for a third of
the list.

### Two entries carry a bug fix in their comment — keep it

**MiniMax** was `https://api.minimax.io/anthropic` with Anthropic auth, pointing
at MiniMax's Messages-protocol API, which openhuman does not speak. Both chat and
model-listing 404'd; the listing 404 was a Sentry issue. The `/v1` OpenAI surface
with bearer auth is the fix. Port the fixed value and the comment.

**Anthropic** is the only non-bearer entry in the list. A port that assumes one
auth style across the catalogue breaks exactly one provider, and it is the one
people will try first.

### Where `auth` is *applied* — the catalogue entry alone is not enough

Recording the auth style and never consulting it is the same bug as recording it
wrongly, and it is the one this codebase actually shipped: the catalogue said
`anthropic`, and the catalog reader sent `Authorization: Bearer` to every
provider regardless. Verified against Anthropic's own docs, the picture is
narrower and stranger than "apply the right header everywhere":

| Path | What it reaches at `api.anthropic.com/v1` | Auth |
|---|---|---|
| `GET {base}/models` | Anthropic's **native** Models API | `x-api-key` + `anthropic-version` |
| `POST {base}/chat/completions` | Anthropic's **OpenAI-compatibility layer** | `Authorization: Bearer` |

So `AuthStyle::Anthropic` means **"this provider's native endpoints use
`x-api-key`"**, and the only native call this product makes is the catalog
listing. The chat path must stay on bearer — "fixing" it to match the catalogue
would break a path that works. `harness::built_in::provider::send_plan` carries
that warning at the line someone would change.

`probe::apply_auth` is the one implementation, called by the connect-time probe,
the catalog reader (`inference_models::discover_models`) and the per-provider
test. Its tests assert on the **headers actually sent**, because this bug is
invisible to a test that only checks a return value.

Two facts confirmed from `platform.claude.com` rather than from memory:
`anthropic-version: 2023-06-01` is still the current value, and the Models API
response envelope is `{"data": [{"id": …}], "first_id", "has_more", "last_id"}` —
`data[].id` is exactly what the OpenAI-shaped reader already parses, so **no
per-provider response mapping is needed**.

Two caveats worth carrying:

- **The compatibility layer is not Anthropic's recommended production path.**
  Their own words: "primarily intended to test and compare model capabilities,
  and is not considered a long-term or production-ready solution for most use
  cases." It ignores `strict` and `response_format`, supports no prompt caching,
  and hoists system messages into a single leading one. Reaching Anthropic
  through a gateway, or through a native request path, remains the better answer
  if this provider matters — but it works today and does not need one.
- **The Models API paginates** (`after_id` / `before_id` / `limit`, default
  **20**, max 1000, with `has_more`). Our reader requests one page and ignores
  `has_more`, so any provider publishing more than its default page size is
  silently truncated. It does not bite Anthropic, whose list is short.

### Provider-specific behaviour that travels with the list

| Provider | Behaviour | Why |
|---|---|---|
| `openai` | the **only** one where a chat-completions 404 may fall back to `/responses` | every other preset is chat-completions-only; the fallback guarantees a second 404 and floods error reporting |
| custom / unknown slug | keeps the `/responses` fallback | a user-defined endpoint may be a genuine OpenAI proxy |
| known chat-only hosts | the fallback is withheld by **host**, not slug | closes the gap where a custom slug points at e.g. `integrate.api.nvidia.com` |
| `openrouter` | sends `HTTP-Referer` and `X-Title` attribution headers | required by OpenRouter to attribute traffic |
| Azure hosts | detected by **endpoint host, not slug**, and forced to free-text model entry | Azure routes by *deployment name*, while `/models` lists *base model ids* — a dropdown makes the only correct value unreachable |

The Azure rule deserves porting in full: their host list deliberately **excludes**
`inference.ai.azure.com` and `models.ai.azure.com`, because those are the Foundry
serverless endpoints that key `model` on the model name rather than a deployment
name — classifying them would relabel a correct model id as a "deployment name"
and mislead the operator in the one place the module exists to clarify.

## Local runtimes

Three, all keyed on an endpoint rather than a credential:

| slug | Label | Needs | Note |
|---|---|---|---|
| `ollama` | Ollama | endpoint | default `http://localhost:11434` |
| `lmstudio` | LM Studio | endpoint | |
| `omlx` | OMLX | endpoint **and** key | the only local runtime that takes both |

Client-side endpoint validation for this category only: parse as a URL, require
`http:`/`https:`, and append `/v1` when the path is empty or `/`. Cloud providers
skip this — their endpoint comes from the preset.

**These are a desktop concern.** OpenCompany is a server-side product, so
`ollama` reaching `localhost` means the *host's* localhost, not the operator's
laptop. Port the category and the slugs, but see the SSRF rules in
[`connect-flow.md`](connect-flow.md): loopback is an explicit allowance made
*because* this category exists, not a hole.

## CLI logins

Two, both credential-less from the console's point of view:

| Option slug | Stored as | Label source |
|---|---|---|
| `claude-code` | `claude-code` | "Claude Code" |
| `codex` | **`openai`** | "Codex" |

**Codex is the trap, and openhuman documents it:** the Codex CLI login is an
OpenAI credential, so it is stored under the `openai` slug and shows up as the
OpenAI row. Keying its "already connected" check on the literal `codex` never
matches, so the dialog would offer Codex forever. It also deliberately has **no
row of its own** — a second row would imply a second connection the operator
could remove separately.

Connecting `claude-code` sends `credentialMode: 'cli_login'` with no key and
**skips the probe entirely**. Codex uses an OAuth token-set flow and, after
connecting, **clears the API key** for the `openai` slug.

**On a server-side host, this category is empty.** Render it saying so rather
than hiding it — the shape is then correct if a delegated credential ever becomes
available, and an empty labelled group is more honest than a missing one.

## Custom

Not a fourth category. One action, one button, because — openhuman's words — *"a
select over one option is a button wearing a costume."*

Three fields: name, OpenAI-compatible URL, API key. **The slug is derived from
the name, never typed**, and validated against three failures before anything is
written: empty, already in use, or colliding with a reserved builtin slug.

## The copy, verbatim

Port these strings rather than rewriting them. They are the result of several
passes and they say the distinction the categories exist to make.

| Key | String |
|---|---|
| group, cloud | `Cloud` |
| group, local | `Local runtimes` |
| group, CLI | `CLI logins` |
| placeholder, cloud | `Choose a cloud provider…` |
| placeholder, local | `Choose a local runtime…` |
| placeholder, CLI | `Choose a CLI login…` |
| helper, cloud | `Hosted models. You supply an API key.` |
| helper, local | `Models running on this machine. You supply the endpoint.` |
| helper, CLI | `Reuses a login another command line tool already holds.` |
| detail, local row | `Runs on this machine` |
| detail, CLI row | `Uses a login another CLI already holds` |

Cloud rows use the endpoint's **host** as their detail line, not the full URL.

## Two list rules that are not optional

**Each category lists only what is not yet connected.** The page behind the modal
shows the rest, and offering to add something twice is how you get two rows for
one provider.

**The select's value is pinned empty.** Choosing an item starts a connect flow and
leaves nothing selected — a select that kept the last pick would claim a
selection it does not own, since the connection state lives in the page, not the
control.

## The account-scoped catalogue (OpenRouter only)

`GET /api/v1/models` on OpenRouter carries **no `security` block** — it is a
public registry, and the bearer changes nothing about what comes back. So the
picker offered all ~450 models to an account whose Settings → Privacy
allowed-providers list permits only `novita, openai, baseten, deepseek,
deepinfra`. Every `anthropic/*` entry was unreachable for that account, and
choosing one failed at the first turn with a 404 whose reason surfaced in a
thread reply. A picker that offers models the account cannot use is worse than a
short list.

`GET /api/v1/models/user` is OpenRouter's own answer, documented as *"List models
filtered by user provider preferences, privacy settings, and guardrails"*. It is
one of only two endpoint groups in their spec with `security: [{"bearer": []}]`,
and it takes the **ordinary inference key** — not a management key. The response
shape is identical to `/models`, so the existing reader parses it unchanged.

Four things this depends on, each from the docs rather than inference:

- **`output_modalities` defaults to `text`.** Left off, every image, audio,
  embedding and video model vanishes silently. `all` is passed explicitly.
- **`limit=1000` is the maximum**, and the whole permitted catalogue fits in one
  response, so there is no paging to get wrong.
- **It accepts none of `/models`' rich filters** (`q`, `sort`, `category`,
  `providers`, price, context). Anything that needed those does them client-side.
- **A 404 falls back to `/models`**, loudly. A proxy or gateway answering on
  OpenRouter's host does not serve this path, and degrading to the public
  registry beats reporting that a company has no models at all.

### What is not readable, and so is not guessed at

The account's allowed-providers list itself appears in **no API response**.
`GET /api/v1/key` carries credits, limits and usage only; there is no settings or
preferences endpoint; the `allowed_providers` field that does exist belongs to
Guardrails, a separate mechanism behind a management key. So the console can show
*which* models are reachable — exactly, from `/models/user` — but cannot say
*why* one is missing, because a model can be absent for provider preferences,
privacy settings or guardrails and the response gives nothing to tell them apart.
Nor can the 404 be keyed on: `error.code: 404` does not distinguish "no allowed
provider" from "no such model", and the discriminating text is undocumented
English prose.

`provider.only` in the request body is **not** an alternative. The docs are
explicit: *"your account-wide allowed providers act as the ceiling, and the
request's `only` list narrows within it."* It can narrow, never widen.

### What `/models/user` does not filter — a stated limit, not an approximation

**It does not encode the account's allowed-providers ceiling.** Observed on an
account whose Settings → Privacy list permits only `novita, openai, baseten,
deepseek, deepinfra`: the scoped read returned **51** models rather than the
public catalogue's ~444, and 23 `anthropic/*` and 5 `google/*` were still among
them. Every one of those 404s at turn time. So the endpoint narrows the list a
long way, and it does not narrow it to what the key can reach.

That is consistent with its own wording — *"filtered by user provider
preferences, privacy settings, and guardrails"* — if "provider preferences" means
something other than the allowed-providers list. The docs do not say which, and
**no API surface exposes that list** (see above), so there is nothing to
cross-reference it against.

We do not close the remaining gap by inference. Diffing `/models` against
`/models/user` and attributing the difference via
`/models/{author}/{slug}/endpoints` would yield a plausible-looking attribution
that is unwarranted: a model can be absent for privacy settings or guardrails
rather than provider preferences, and the response gives nothing to tell the
three apart. Hiding a model the operator can actually use is a worse failure than
showing one they cannot, so the picker stops where the documented API stops.

What remains, therefore, is a turn-time failure for a subset of the list — which
is why the routing row's check reports whether the endpoint *publishes* a model
and says plainly that it does not send a turn.

### Why it is one host's rule

`scoped_catalog_path` keys on OpenRouter's own host, the same way the Azure
deployment-name rule keys on Azure's — every other provider has its own
account-level restrictions or none, and a general assumption here would send
`/models/user` to endpoints that have never heard of it. The platform proxy is
deliberately excluded: it fronts OpenRouter and serves the same catalogue, but
the account behind it is the server operator's rather than the tenant's.

## Reserved slugs

openhuman keeps two lists under the same name that mean different things — a
Rust one (what gets re-injected on save) and a TypeScript one (what the add list
hides). The divergence is deliberate there and documented, but two lists called
the same thing meaning different things is a trap. **Port one list with one
meaning**, and if a second is genuinely needed, name it for what it does.

One carve-out worth copying with its reasoning: openhuman deliberately does
**not** reserve `ollama`, because the settings panel registers an `ollama` entry
so the model dropdown can resolve the user's chosen base URL — and the factory's
`ollama:` prefix branch fires before the slug lookup, so a synthetic entry never
reaches the cloud path. Reserving it would break the model picker.
