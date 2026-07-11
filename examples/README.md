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
- `src/main.rs` — a two-line entrypoint that calls
  `opencompany::run_company(...)` on the manifest.

The manifests declare the agents each company *will* run; the behavior lives
in the OpenCompany host and the vendored modules. Running a harness today
parses and validates its manifest and prints the company's effective
configuration.

## The operator console

[`console/`](console/) is the exception to the shape above: a single,
company-agnostic operator UI (Vite + React + TypeScript) that talks to any
OpenCompany host and any company at runtime — chat, approvals, and feedback in
one place. It is reused across every harness instead of shipping a bespoke
front end per company. See [`console/README.md`](console/README.md).

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

## Running a harness

Boot any harness directly — it loads, validates, and reports its company:

```sh
cargo run -p agentic-marketing-agency
```

Or validate a manifest and print its effective configuration without booting:

```sh
cargo run --bin opencompany -- check examples/agentic_marketing_agency
```

Initialize the vendored runtime before using deeper integrations, then boot
the shared HTTP host (surface under active development):

```sh
git submodule update --init --recursive
cargo run --bin opencompany -- serve --bind 127.0.0.1:8080
```
