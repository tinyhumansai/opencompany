# Company Manifest

The manifest is the on-disk definition of a [Company](../glossary.md). The
preferred filename is `company.toml`; `agents.toml` (the current examples
format) is accepted unchanged with a deprecation note from `opencompany
check`.

**Compatibility rule:** every key in today's `agents.toml` keeps its exact
meaning, and a bare `agents.toml` (just `[company]` + `[[agent]]`) remains a
complete, valid company. **Prosumer rule:** every new table is optional with
a safe default; the defaults produce a working company with only
`TINYHUMANS_API_KEY` set.

Parsing lives in `src/company/manifest.rs` (`CompanyManifest::from_path`,
serde + validation). Validation errors MUST be actionable in prosumer
language ("`[policy].mode` must be one of readonly, supervised, full — you
wrote `supervized`"), never serde traces.

## Full schema

```toml
# ── existing keys (unchanged from agents.toml) ─────────────────────────
[company]
name = "Agentic Marketing Agency"
output = "Campaigns across every channel"
human_role = "Campaign review and sign-off"
handle = "acme-marketing"          # NEW, optional: tiny.place @handle

[[agent]]
id = "copywriter"                  # snake_case, unique
role = "Copywriter"
description = "Write ads, pages, and campaign copy."
# NEW optional per-agent keys:
tier = "reasoning"                 # cognition tier hint (see glossary)
tools = ["docs.*", "email.send"]   # tool grant globs
budget_usd_daily = 5.0             # per-agent daily spend cap

# ── new tables (all optional) ──────────────────────────────────────────
[users]
# Addresses that may sign in as admins without being invited first. This is
# the bootstrap for invite-only access: someone has to send the first invite,
# and there is no operator token to do it with. Listing an address does not
# create an account — it makes the address eligible, and signing in mints the
# admin. See runtime/users.md.
admins = ["ada@example.com"]

[brain]
mode = "hosted"                    # hosted (default) | sidecar
max_passes = 12                    # passed through to Medulla

[inference]                        # NEW: per-tenant Bring-Your-Own-Key (#56)
provider = "openrouter"            # managed (default) | openrouter | openai_compatible | ollama
# base_url = "https://openrouter.ai/api/v1"  # required for ollama/openai_compatible; defaulted otherwise
# api_key_secret = "byo/openrouter"          # names a secret-store KEY — never the token itself

[inference.models]                 # abstract tier → concrete provider model id
"chat-v1" = "deepseek/deepseek-chat"
"reasoning-v1" = "deepseek/deepseek-r1"

[channels.operator]
enabled = true                     # built-in chat; default true

[channels.email]
provider = "openhuman"             # delegate to an OpenHuman channel

[tools]
provider = "openhuman"             # openhuman (default) | builtin
allow = ["web.*", "docs.*"]        # company-wide grant; agents intersect

[policy]                           # see company-brain/approvals.md
mode = "supervised"                # readonly | supervised (default) | full
always_approve = ["payment.send", "filing.submit", "external.publish"]
auto_approve_under_usd = 1.0

[place]                            # see company-as-agent/
discoverable = false               # default false: going public is opt-in
skills = [
  { id = "seo.audit", price_usd = "25.00", description = "Full SEO audit" },
]

[budget]
monthly_usd = 200.0                # hard cap: inference + x402 combined

[[schedule]]
cron = "0 9 * * MON"
prompt = "Weekly review and operator digest"
```

## Semantics

- **`[company]`** becomes the seed of the [Charter](../company-brain/charter.md).
  `handle` is only used when `[place].discoverable = true`.
- **`[[agent]]`** entries define the Roster. `tier` is a hint the brain may
  use when delegating; it never selects a model (the backend maps tiers to
  SKUs). `tools` and `budget_usd_daily` intersect with the company-wide
  `[tools].allow` and `[budget]` — the most restrictive wins.
- **`[brain]`** selects the `Brain` implementation. `hosted` requires a
  TinyHumans credential at runtime; `sidecar` requires the `sidecar` feature.
- **`[inference]`** (issue #56 — BYOK) routes the company's agents through a
  chosen model provider. Absent (the default) keeps the managed TinyHumans
  brain. `provider` is one of `managed` / `openrouter` / `openai_compatible` /
  `ollama`; `base_url` is required for the latter two. `api_key_secret` names a
  **secret-store key** — never the token, which is written write-only through
  the console (Connections → Inference). `[inference.models]` maps an abstract
  cognition tier (`chat-v1`, `reasoning-v1`, `agentic-v1`, `vision-v1`) to a
  concrete provider model id; an unmapped tier passes through verbatim.
  Precedence is **runtime console override > manifest `[inference]` > managed
  default**, and a per-tenant provider re-resolves it every turn — so a console
  switch takes effect on the agents' next turn with **no restart**.
- **`[channels.*]`** enables `ChannelAdapter`s. Unknown channels are a
  validation error; disabled OpenHuman means non-operator channels degrade
  with a boot warning, never a failure.
- **`[policy]`** configures the default `ApprovalGate`. `mode` mirrors
  OpenHuman's security tiers. `always_approve` lists effect kinds that park
  for approval regardless of amount; `auto_approve_under_usd` lets small
  spends through. Defaults are conservative: `supervised`, with all
  money/publish/filing effects gated.
- **`[place]`** drives the [going-public flow](../company-as-agent/README.md).
  `skills` feed Agent Card generation; prices are decimal strings (USDC).
- **`[budget].monthly_usd`** is a hard ceiling enforced by the kernel across
  inference usage and x402 spend; reaching it pauses the company with an
  operator notification rather than silently degrading.
- **`[[schedule]]`** entries become `ScheduleFired` events; cron syntax is
  standard 5-field.

## Layering and provenance

Effective configuration = template defaults ⟵ manifest ⟵ onboarding-interview
answers ⟵ operator runtime edits. Later layers win; the runtime records which
layer set each value so the Charter can show provenance
([charter.md](../company-brain/charter.md)). Operator edits at runtime are
persisted to the `CompanyStore`, not written back into the manifest file.

## Tooling

- `opencompany check <dir>` — validate a manifest, print effective config,
  lint deprecations (e.g. `agents.toml` filename).
- The 18 `examples/*` crates shrink to a manifest plus a two-line `main`
  calling `opencompany::run_company(manifest_path)`; they double as the
  [Template Gallery](../product/templates.md) source.
