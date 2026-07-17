# Company Definitions

Each subdirectory is a **business type** — data, not code. The single
configurable host ([`../src/`](../src/)) instantiates any of them; a business is
a manifest plus its docs, never its own program.

Every folder follows the same shape:

- `company.toml` — the manifest: the roster of agents, their responsibilities,
  the output, and the moments reserved for the human (the machine-readable
  definition the host loads).
- `README.md` — what the company does, in plain language.

Adding a business is a new folder, not a new crate. The behavior lives entirely
in the host and the vendored runtimes; each definition just configures it.

The operator console is a separate, company-agnostic app at
[`../frontend/`](../frontend/) — one UI for every company here.

## Catalog

| Harness | Output | Human keeps |
| --- | --- | --- |
| [`agentic_venture_studio`](agentic_venture_studio/) | A portfolio of startups | Capital allocation, major strategy |
| [`agentic_software_company`](agentic_software_company/) | An entire SaaS product | Product direction |
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
