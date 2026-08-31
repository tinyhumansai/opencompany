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
- `skills/<slug>/SKILL.md` — the procedures this vertical runs, in the shape the
  shared library at [`../skills/`](../skills/) uses.
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
  [`../globals/tasks.toml`](../globals/tasks.toml). Seeded cards never enter a
  column that dispatches a run.

Adding a business is a new folder, not a new crate. The behavior lives entirely
in the host and the vendored runtimes; each definition just configures it.

The operator console is a separate, company-agnostic app at
[`../frontend/`](../frontend/) — one UI for every company here.

## Catalog

| Harness | Output | Human keeps |
| --- | --- | --- |
| [`agentic_venture_studio`](agentic_venture_studio/) | A portfolio of startups | Capital allocation, major strategy |
| [`agentic_software_company`](agentic_software_company/) | An entire SaaS product | Product direction |
| [`agentic_product_team`](agentic_product_team/) | A triaged queue, a groomed backlog, a defended roadmap | Prioritization calls & roadmap sign-off |
| [`startup_accelerator`](startup_accelerator/) | A funded, mentored cohort | Investment & demo-day decisions |
| [`agentic_venture_capital`](agentic_venture_capital/) | Investment memos & a managed portfolio | Investment decisions |
| [`agentic_consultation_firm`](agentic_consultation_firm/) | Strategy decks & implementation plans | Executive workshops |
| [`agentic_marketing_agency`](agentic_marketing_agency/) | Campaigns across channels | Campaign review & sign-off |
| [`agentic_design_studio`](agentic_design_studio/) | Brand & product design systems | Creative direction sign-off |
| [`agentic_media_company`](agentic_media_company/) | Published, distributed stories | Editorial standards |
| [`agentic_influencer_business`](agentic_influencer_business/) | A creator that never sleeps | Occasional appearance / avatar |
| [`agentic_game_studio`](agentic_game_studio/) | Shippable games | Creative & design direction |
| [`agentic_game_business`](agentic_game_business/) | LiveOps, UA & monetization for a game | Monetization & growth strategy |
| [`agentic_recruiting_company`](agentic_recruiting_company/) | Sourced, screened, scheduled candidates | Final hiring decisions |
| [`agentic_enterprise_sales`](agentic_enterprise_sales/) | Qualified pipeline & proposals | Closing strategic accounts |
| [`agentic_customer_support`](agentic_customer_support/) | Resolved tickets & docs | Escalation & policy |
| [`agentic_realestate_company`](agentic_realestate_company/) | Underwritten deals & managed tenants | Purchase approvals |
| [`agentic_accounting_firm`](agentic_accounting_firm/) | Books, taxes, forecasts | Sign-off on filings |
| [`agentic_law_firm`](agentic_law_firm/) | Drafts, research, discovery | Approving filings |
| [`agentic_pharma_startup`](agentic_pharma_startup/) | Candidate molecules & trial plans | Laboratory work |
| [`agentic_research_lab`](agentic_research_lab/) | Source-backed research reports with the evidence attached | Setting the question & accepting findings |
| [`agentic_math_lab`](agentic_math_lab/) | Verified answers to computational problems, with the programs that produced them | Stating the problem & accepting the answer |
| [`signals_opportunity_studio`](signals_opportunity_studio/) | A ranked weekly opportunity brief | Which opportunities to fund |

Signals and the Opportunity Engine ship as the
[`signals_opportunity_studio`](signals_opportunity_studio/) **template, not
kernel code**: a roster, a charter, and a weekly `[[schedule]]` over the
existing channels, memory/context, and brain ports. There is no Signals
subsystem in `src/`.

## Running one

Validate a definition, then launch it on the host (`--company` points at any
folder here):

```sh
cargo run --bin opencompany -- check companies/agentic_marketing_agency
cargo run --bin opencompany -- serve --company companies/agentic_marketing_agency
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
