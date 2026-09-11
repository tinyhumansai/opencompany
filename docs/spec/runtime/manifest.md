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
language ("`[policy].mode` must be one of readonly, supervised, auto, full —
you wrote `supervized`"), never serde traces.

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
delegates_to = ["research"]        # desks this agent may hand work to ("*" = all)
budget_usd_daily = 5.0             # per-agent daily spend cap (UTC day)
prompt = "Write for the reader."   # appended to the generated persona
prompt_files = ["prompts/tone.md"] # checked-in briefing docs, under `agents/`
context = ["brief.md"]             # live workspace docs routed into the prompt
classes = ["evidence"]             # routing exclusions: evidence | judge | directive
# A roster may instead live one file per teammate under `agents/<id>.toml`, with
# these same keys and the filename as the id. The two forms are exclusive —
# declaring both is a validation error. See runtime/agents.md.

# ── new tables (all optional) ──────────────────────────────────────────
[users]
# How humans sign in: email (default) | wallet | none. See
# runtime/auth-modes.md. The mode decides which bootstrap list below is read —
# `admins` in email mode, `wallets` in wallet mode, neither in none mode, which
# has no sign-in at all and cannot add a second person.
mode = "email"
# Addresses that may sign in as admins without being invited first. This is
# the bootstrap for invite-only access: someone has to send the first invite,
# and there is no operator token to do it with. Listing an address does not
# create an account — it makes the address eligible, and signing in mints the
# admin. See runtime/users.md.
admins = ["ada@example.com"]
# The same grant, for `mode = "wallet"`: base58 Ed25519 wallet addresses that
# may sign in as admins without an invite.
wallets = ["7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU"]

[brain]
mode = "hosted"                    # hosted (default) | sidecar
max_passes = 12                    # passed through to Medulla

[[harness]]                        # named execution engines — see harnesses.md
id = "embedded"                    # snake_case, unique
kind = "built_in"                  # built_in (default) | acp
default = true                     # exactly one entry, when any is declared

[harness.inference]                # attaches to the [[harness]] above
provider = "openrouter"

# [[harness]]
# id = "my_laptop"
# kind = "acp"
# [harness.acp]
# transport = "local"              # local | runner
# agent = "claude"                 # claude | codex

[inference]                        # per-tenant BYOK (#56) — the fallback for a
provider = "openrouter"            # harness declaring no [harness.inference].
                                   # openrouter (default) | openai_compatible | ollama
# base_url = "https://openrouter.ai/api/v1"  # required for ollama/openai_compatible; defaulted otherwise
# api_key_secret = "byo/openrouter"          # names a secret-store KEY — never the token itself

[inference.models]                 # abstract tier → concrete OpenRouter model id
"chat-v1" = "deepseek/deepseek-chat"
"reasoning-v1" = "deepseek/deepseek-r1"

[channels.operator]
enabled = true                     # built-in chat; default true

[channels.email]
provider = "openhuman"             # delegate to an OpenHuman channel

[[group_chat]]
id = "creative"                    # a desk: who the human talks to
name = "Creative studio"
members = ["copywriter", "editor"] # ids from the roster
tools = ["docs.*"]                 # NEW: this desk's tool ceiling. Optional;
                                   # empty narrows nothing. See runtime/tools.md
hive = { enabled = true, turn_budget = 6, quorum = 2, blind_round = true,
         require_evidential = true, refutation_cap = 2,
         dominance_cap = 4, repetition_cap = 2,
         moves = { copywriter = ["propose", "support", "commit"],
                   editor = ["object", "refute", "evidence", "question"] } }
                                   # how this desk answers. Every key optional;
                                   # omit the table entirely for the defaults.
                                   # `moves` assigns each member the markers it
                                   # may open a line with — a room where
                                   # everyone may `!propose` votes instead of
                                   # deliberating. A member the table omits
                                   # keeps every move. `commit` (with
                                   # `question`/`defer`) is ungated: every
                                   # seat keeps it whether or not the table
                                   # names it, so a `commit` entry here is
                                   # accepted for documentation only and
                                   # restricts nothing. See runtime/hivemind.md

[speech]                           # NEW: do agents speak by calling a tool?
enabled = true                     # off by default. On, every agent's belt
                                   # gains desk_post / desk_dm / desk_close /
                                   # desk_read, and a turn's return text becomes
                                   # private thinking. A turn that calls none of
                                   # them still has its text journaled, so this
                                   # can never silence anybody.
                                   # Company-level, not per-desk: speech is a
                                   # property of an agent's session, which spans
                                   # every desk it sits on. See runtime/speech.md

[group_chat.hive.referral]         # NEW: may this desk ask ANOTHER desk?
enabled = true                     # off unless this says so; the whole block
                                   # defaults to referring nothing
max_hops = 2                       # chain depth; 2 is one round trip
reach = "desks"                    # local | channels | desks — widens strictly
returns = true                     # carry the answer back to the desk that asked
peer_cap = 2                       # crossing questions per episode. The library
                                   # bounds depth; only a host knows what a
                                   # question costs, so width is ours
                                   # See runtime/hivemind-referral.md

[tools]
provider = "openhuman"             # openhuman (default) | builtin
allow = ["web.*", "docs.*", "search"]  # company-wide ceiling. Desks and agents
                                   # narrow it: allow ∩ desk.tools ∩ agent.tools
                                   # `search` must be named — `*` never grants it
search_daily_calls = 200           # per-company daily web_search cap (0 = paused)
max_delegation_depth = 2           # how deep one message's hand-off chain may run
                                   # 1 = desks may not re-delegate at all; 1..=4

[policy]                           # see company-brain/approvals.md
mode = "supervised"                # readonly | supervised | auto | full
                                   # parse default supervised; new companies get auto
always_approve = ["publish_artifact"]   # default []; names a tool or an open
                                   # effect kind — see approvals.md
auto_approve_under_usd = 1.0
approval_ttl_hours = 24            # default 24; how long a parked approval
                                   # waits before it default-denies

[place]                            # see company-as-agent/
discoverable = false               # default false: going public is opt-in
skills = [
  { id = "seo.audit", price_usd = "25.00", description = "Full SEO audit" },
]

[budget]
monthly_usd = 200.0                # hard cap: inference + x402 combined

[plan]                             # capability tier gating (issue #108)
name = "starter"                   # free | starter | pro | unlimited (optional)
period = "daily"                   # daily (default) | monthly
token_budgets = { web = 500000 }   # override/extend the named tier per namespace

[workflows]                        # saved workflow graphs (issue #401)
enabled = ["digest"]               # ids of workflows/<id>.toml graphs to enable
max_in_flight_runs = 8             # concurrent-run ceiling (default 8; must be >= 1)

[[schedule]]
cron = "0 9 * * MON"
prompt = "Weekly review and operator digest"
```


## Semantics

The behaviour of every key and table in the schema above is spelled out in
[manifest-semantics.md](manifest-semantics.md) — split out so this page stays
under the 500-line cap while the schema stays discoverable here.

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
