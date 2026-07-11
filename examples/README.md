# OpenCompany Example Harnesses

Each subdirectory is a **company harness** — a template that wires a roster of
agents, their responsibilities, and the human-in-the-loop checkpoints into the
OpenCompany host. A harness is the "org chart" the runtime instantiates: the
host loads the `agents.toml` manifest, backs each agent with the vendored
OpenHuman / TinyAgents runtime modules, and exposes the company over the Axum
HTTP surface.

Every harness follows the same shape:

- `README.md` — what the company does, its agent roster, and the human role.
- `agents.toml` — the machine-readable manifest of agent definitions.

These are scaffolds: the manifests declare the agents each company *will* run;
the behavior lives in the OpenCompany host and the vendored modules.

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

## Running a harness

Harnesses are launched through the OpenCompany CLI/host (see the repository
[`README.md`](../README.md)). Initialize the vendored runtime first:

```sh
git submodule update --init --recursive
```

Then boot the host and point it at a harness manifest (surface under active
development):

```sh
cargo run --bin opencompany -- serve --bind 127.0.0.1:8080
```
