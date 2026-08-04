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
budget_usd_daily = 5.0             # per-agent daily spend cap (UTC day)

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
allow = ["web.*", "docs.*", "search"]  # company-wide grant; agents intersect
                                   # `search` must be named — `*` never grants it
search_daily_calls = 200           # per-company daily web_search cap (0 = paused)

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

[plan]                             # capability tier gating (issue #108)
name = "starter"                   # free | starter | pro | unlimited (optional)
period = "daily"                   # daily (default) | monthly
token_budgets = { web = 500000 }   # override/extend the named tier per namespace

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

  **`budget_usd_daily`** (enforced since issue #304 — before that it was
  validated, stored and displayed, but nothing read it) caps one teammate's
  spend over the **UTC calendar day**, resetting at `00:00Z` — the same
  boundary `[plan]`'s daily token budget uses. Spend is re-read from the usage
  meter on every check, so a restart mid-day resumes against the real figure
  rather than a fresh zero.

  It covers **metered, attributed** spend: inference turns and priced tool
  calls (`web_search`, `media_generate_*`), plus any tool call that declares an
  `amount_usd`. Two behaviours at the cap:

  - **Dispatch is refused** for that teammate, before any model call, with a
    notice naming the cap and the reset. The rest of the company keeps running.
  - **A priced tool call parks for approval** rather than being denied.
    Approving it runs that one call and nothing more; the cap is not raised.
    Free reads and sends are unaffected — a spend cap caps spend.

  Known limits, stated rather than papered over:

  - **Executed x402 payments escape the counter.** Ledger entries carry no
    agent, so there is nothing to attribute them to. What *is* covered is the
    pre-flight case: a call declaring an `amount_usd` that would breach the
    remaining budget parks before the money moves. Company-wide payment
    spending is governed by `[budget].monthly_usd`, which is enforced on the
    economy path.
  - **Turn-boundary overshoot.** A turn or call that starts under the cap can
    finish over it; the overshoot is bounded by one call. There is no
    reservation ledger in v1 — the same documented window `[plan]` carries.
  - **Unreadable spend fails differently at each layer, deliberately.** With no
    meter or a failing meter, dispatch **runs** (bricking a teammate's
    cognition with no operator recourse is worse than a day of overspend),
    while a priced tool call **parks** (a human can wave that one through).
  - **Operator-added (overlay) teammates are uncapped in v1.** Only manifest
    `[[agent]]` entries carry the field.
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
  That holds only once the company is already on the harness cognition path.
  *Which brain a company runs* is decided once, when the runtime is built: a
  company that resolved no inference source at boot gets the offline echo brain
  and an unwired workflow runner, and a credential saved afterwards reaches
  neither. The status route reports that state as `restartRequired` — a resolved
  config next to a non-harness `cognition` — and the console says "restart"
  instead of "next turn" for it (issue #266).
  Saving `managed` from the console is a *revert* (`DELETE …/inference`) and
  carries no credential, so the console refuses that save while a key is still
  typed in the form rather than dropping it and reporting success (issue #265).
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
- **`[plan]`** (issue #108) gates the exec tool families (`shell`, `code`,
  `web`, `subagent`) by the company's **token spend this period**, a distinct
  axis from `[policy].mode` (autonomy) and an agent's `tier` (cognition). A
  built-in `name` (`free` / `starter` / `pro` / `unlimited`) supplies a base
  budget map; `token_budgets` overrides/extends it per namespace. The map's key
  set **is** the capability set — a gateable namespace absent from it is denied
  outright. Each budget is a **threshold over total period token spend**, not a
  per-namespace meter (usage samples carry no per-tool attribution): when spend
  reaches a tier's budget, that tier's tools switch off for the rest of the
  period; different budgets give **graduated degradation**. `period` is `daily`
  (default) or `monthly`, aligned to UTC calendar boundaries. Gating is
  **fail-closed** — if the usage meter can't be read, every gateable family is
  denied (the turn still runs on its intrinsic memory/file/MCP tools). The gate
  re-resolves before every turn, so a tier that crosses its budget mid-session
  switches off on the **next** turn (a turn already in flight finishes). An
  absent `[plan]` leaves gating off entirely. The console's Usage view shows a
  live per-tier budget card (`GET …/capabilities`).
  - **`media`** (issue #109) is a fifth gateable namespace covering the
    image/video generation tools (`media_generate_image`,
    `media_generate_video`, `media_list_models`), but it is **real-money and
    opt-in**: it is granted only by an **explicit** `media` / `media.*` entry in
    `[tools].allow` — the `*` wildcard deliberately does **not** grant it — and
    it runs exclusively on a **managed platform credential** (resolved from the
    environment, never a tenant BYOK key or secret). It is absent from the
    `free` / `starter` / `pro` tiers (denied there) and uncapped only under
    `unlimited`; a company opts in per-namespace with
    `token_budgets = { media = N }`. Every generation additionally **parks for
    operator approval** before the backend bills it, and the whole family is
    compiled out unless the build enables the `media` feature. With no managed
    credential configured, a `media` grant wires no tools (fail-closed). The
    Usage view surfaces a dedicated media status row (active / awaiting
    credential / not granted / not in this build).
  - **`search`** (issue #238) is a seventh gateable namespace covering the single
    `web_search` tool — source *discovery* for the research skills, which
    previously ran on a belt that could read a known URL but never find one. It
    is **priced and opt-in**: granted only by an **explicit** `search` /
    `search.*` entry in `[tools].allow` (the `*` wildcard deliberately does
    **not** grant it, and unlike `media`/`composio` it is **not** in the default
    grant list either), and it runs exclusively on the **managed platform
    credential** — the same identity as managed inference, resolved from the
    environment, never a tenant key. The backend charges per request and reports
    the amount, which is recorded as one `SearchCall` usage sample and rolls into
    the window's cost.
    Three things differ from `media` on purpose:
    - **Individual searches do not park for approval.** Consent is the explicit
      grant; the boundary is `[tools].search_daily_calls`, a per-company **daily
      call cap** (default 200; `0` pauses search without editing `allow`).
      Over-cap returns a loud "search budget exhausted" tool error, never an
      empty result set — an agent handed silence invents citations. An operator
      who does want a per-call gate sets
      `[policy].always_approve = ["web_search"]`, which overrides every tier.
    - **`[policy].mode = "readonly"` still denies it.** A search reaches a third
      party and spends money, so a desk whose contract is that nothing is spent
      does not get one.
    - **There is no `search` Cargo feature.** The tool rides the `openhuman`
      harness feature so CI's gated lane actually compiles and tests it.
    The Usage view surfaces a `Web searches` KPI plus a search status row
    (active / paused at cap 0 / awaiting credential / not granted / not in this
    build).
  - **`workspace`** (issue #237) grants the company's shared note tree — the
    operator-owned `Standards/` / `Playbooks/` / `Product/` documents seeded
    from `companies/<name>/workspace/**`. It is **split**, unlike every other
    namespace: *reads* (`workspace_list`, `workspace_read`) follow the ordinary
    rule, so a catch-all `*` confers them; *writes* (`workspace_write`) need an
    **explicit** `workspace` or `workspace.write` entry in `[tools].allow`,
    because a write mutates guidance every other agent then treats as the
    company's source of truth. `workspace.read` is therefore a genuinely
    read-only grant. Writes overwrite one **existing** note only and require an
    `expected_updated_at` revision token taken from a prior read, so a note
    edited in the console since the agent read it is refused rather than
    clobbered; creating, renaming and deleting notes stay operator-only. Both
    sides are capped at 64 KiB, which makes a larger note agent-read-only: the
    agent sees a truncated body, and a write against it is refused rather than
    discarding the part it could not see, so only an operator can edit it in the
    console. Under
    `[policy].mode = "supervised"` (the default) a write additionally parks for
    approval, and under `readonly` it is denied — reads stay available in every
    mode. The namespace is **not** gateable by `[plan].token_budgets`: reads
    cost nothing and shedding them would only make agents guess at company
    standards. The tools hit the store per call, so an operator's console edit
    is visible to the next turn with no restart.
- **`[[schedule]]`** entries become `ScheduleFired` events; cron syntax is
  standard 5-field, interpreted in UTC. A saved *workflow* schedules itself
  separately, with the same dialect: its `trigger` node carries a `schedule`
  cron that the workflow scheduler fires (issue #169). A manifest schedule
  drives a company cycle; a trigger schedule drives one workflow run.

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
