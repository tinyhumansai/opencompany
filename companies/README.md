# Company Definitions

Each subdirectory is a **business type** — data, not code. The single
configurable host ([`../src/`](../src/)) instantiates any of them; a business is
a manifest plus its docs, never its own program.

Every folder follows the same shape:

- `company.toml` — the manifest: company-wide tool grants, the desks, the
  workflow graphs to enable, and the approval tier (the machine-readable
  definition the host loads).
- `AGENTS.md` — the working agreement, routed into every teammate's system
  prompt. The one place a convention reaches the whole roster without being
  repeated in every agent's `context`.
- `README.md` — what the company does, in plain language.
- `agents/<id>.toml` — one file per teammate: role, ledger grants, write scope.
- `ledgers/<slug>.toml` — the axes this vertical keeps beyond the built-in
  `tasks`/`goals`/`decisions` and the baseline's own. Seeded into the company's
  store at first boot; see [`../docs/spec/runtime/ledgers.md`](../docs/spec/runtime/ledgers.md).
- `skills/<slug>/SKILL.md` — the procedures this vertical runs, installed in
  it from the start. Every bundle's skills together are also the **skill
  registry** the console lists (`GET …/skills/registry`), so any company can
  install any other vertical's skill by slug; see [Skills](#skills) below.
- `workflows/<id>.toml` — the graphs `[workflows].enabled` turns on.
- `workspace/**` — the Obsidian-style notes the company starts with, seeded once.
- `mcp.json` — the MCP tool servers this vertical's work needs, in the
  `{"mcpServers": {…}}` shape every other MCP host uses. Merged into the
  manifest at load, so a bundle server is held to the same rules an inline
  `[[mcp_server]]` is; a name declared in both is refused rather than resolved.
  Anything needing a credential ships disabled — see
  [`../docs/spec/runtime/tools.md`](../docs/spec/runtime/tools.md).
- `tasks.toml` — the setup work this vertical starts with, seeded onto the
  board in To-do at first boot, on top of the baseline's own cards in
  [`_globals/tasks.toml`](_globals/tasks.toml). Seeded cards never enter a
  column that dispatches a run.

## Skills

There is no separate skill library: a skill lives in the bundle it belongs to,
and the registry the console browses is the union of every bundle's `skills/`
— the baseline's [`_globals/skills/`](_globals/skills/) first, then each
vertical in name order. Installing resolves the slug against that union
server-side and stores the document verbatim, so the client cannot supply skill
content, and a slug no bundle ships fails with `404`. A company's own bundle
skills are already installed in it; the registry is how it borrows another
vertical's.

Each skill is a directory with a `SKILL.md` — YAML frontmatter (`name`,
`description`, and an optional `category` and `version`) followed by the
write-up (When to use / Steps / Output). `category` groups a skill in the
console's Skills view. `version` records the revision a skill ships: installing
snapshots the whole file into the company, `version` included, so an install is
pinned to the revision it was made from. Bump it when you change a skill's
procedure; the baseline's skills must carry one (a test enforces that).

When two bundles ship the same slug (`bug-triage` in `product_team` and
`software_company`, say), each company still gets its own, and the registry
lists the first in that order. Give a skill a distinct slug if the two are
meant to be installable side by side.

Adding a business is a new folder, not a new crate. The behavior lives entirely
in the host and the vendored runtimes; each definition just configures it.

The operator console is a separate, company-agnostic app at
[`../frontend/`](../frontend/) — one UI for every company here.

## Catalog

| Harness | Output | Human keeps |
| --- | --- | --- |
| [`venture_studio`](venture_studio/) | A portfolio of startups | Capital allocation, major strategy |
| [`software_company`](software_company/) | An entire SaaS product | Product direction |
| [`product_team`](product_team/) | A triaged queue, a groomed backlog, a defended roadmap | Prioritization calls & roadmap sign-off |
| [`startup_accelerator`](startup_accelerator/) | A funded, mentored cohort | Investment & demo-day decisions |
| [`venture_capital`](venture_capital/) | Investment memos & a managed portfolio | Investment decisions |
| [`consultation_firm`](consultation_firm/) | Strategy decks & implementation plans | Executive workshops |
| [`marketing_agency`](marketing_agency/) | Campaigns across channels | Campaign review & sign-off |
| [`design_studio`](design_studio/) | Brand & product design systems | Creative direction sign-off |
| [`media_company`](media_company/) | Published, distributed stories | Editorial standards |
| [`influencer_business`](influencer_business/) | A creator that never sleeps | Occasional appearance / avatar |
| [`game_studio`](game_studio/) | Shippable games | Creative & design direction |
| [`game_business`](game_business/) | LiveOps, UA & monetization for a game | Monetization & growth strategy |
| [`recruiting_company`](recruiting_company/) | Sourced, screened, scheduled candidates | Final hiring decisions |
| [`enterprise_sales`](enterprise_sales/) | Qualified pipeline & proposals | Closing strategic accounts |
| [`customer_support`](customer_support/) | Resolved tickets & docs | Escalation & policy |
| [`realestate_company`](realestate_company/) | Underwritten deals & managed tenants | Purchase approvals |
| [`accounting_firm`](accounting_firm/) | Books, taxes, forecasts | Sign-off on filings |
| [`law_firm`](law_firm/) | Drafts, research, discovery | Approving filings |
| [`pharma_startup`](pharma_startup/) | Candidate molecules & trial plans | Laboratory work |
| [`research_lab`](research_lab/) | Source-backed research reports with the evidence attached | Setting the question & accepting findings |
| [`math_lab`](math_lab/) | Verified answers to computational problems, with the programs that produced them | Stating the problem & accepting the answer |
| [`signals_opportunity_studio`](signals_opportunity_studio/) | A ranked weekly opportunity brief | Which opportunities to fund |
| [`ops_watch`](ops_watch/) | An hourly production health check, and a finding only when something changed | What to do about a finding, and what counts as broken |

Signals and the Opportunity Engine ship as the
[`signals_opportunity_studio`](signals_opportunity_studio/) **template, not
kernel code**: a roster, a charter, and a weekly `[[schedule]]` over the
existing channels, memory/context, and brain ports. There is no Signals
subsystem in `src/`.

## Running one

Validate a definition, then launch it on the host (`--company` points at any
folder here):

```sh
cargo run --bin opencompany -- check companies/marketing_agency
cargo run --bin opencompany -- serve --company companies/marketing_agency
```

Or bring up the host + console together with the attached, hot-reloading Docker
demo launcher:

```sh
./scripts/launch-demo.sh marketing up
./scripts/launch-demo.sh marketing down
./scripts/launch-demo.sh marketing down -v  # also delete persistent data
```

`./scripts/list-demos.sh` lists all accepted company directory names and the
short aliases for the most common demos.

Initialize the vendored runtime before using deeper integrations:

```sh
git submodule update --init --recursive
```

## Bring your own inference (BYOK)

By default a company thinks with the managed TinyHumans brain. To route its
agents through your own provider — OpenRouter, any OpenAI-compatible endpoint,
or a local Ollama server — add an `[inference]` section to `company.toml` (see
[`openhuman_demo`](openhuman_demo/company.toml) for a commented example), or
switch live from the operator console under **Connections → Inference**:

```toml
[inference]
provider = "openrouter"            # managed | openrouter | openai_compatible | ollama

[inference.models]                 # abstract tier → concrete provider model id
"chat-v1" = "deepseek/deepseek-chat"
"reasoning-v1" = "deepseek/deepseek-r1"
```

The credential is **never** written in the manifest — set it write-only from
the console, or name a secret-store key with `api_key_secret`. Switching
providers takes effect on the agents' next turn with no restart.
