<h1 align="center">OpenCompany</h1>

<p align="center">
  <strong>Run an entire company with a headcount of one.</strong>
</p>

<p align="center">
  OpenCompany is the operating layer for one-person businesses powered by
  agents. You bring the vision and the judgment calls. Your agents do the work —
  every function, around the clock, at the speed of software.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/status-work%20in%20progress-orange?style=flat-square" alt="Work in progress" />
</p>

> [!WARNING]
> **🚧 Work in progress.** OpenCompany is under active development and moving
> fast. APIs, the CLI, the example harnesses, and the docs will change without
> notice, and things may be incomplete or break between commits. Explore it,
> fork it, build on it — but don't depend on anything staying put yet. Not
> production-ready.

---

## The company of one

For a century, ambition meant headcount. Want to ship a product? Hire
engineers. Want customers? Hire marketers, then sales, then support. Every new
capability was a new payroll line, a new manager, a new quarter of ramp-up.

That tax is gone.

OpenCompany turns a single operator into a full org chart. Opportunity scouts,
founders, engineers, designers, marketers, lawyers, finance, support,
recruiters — instantiated as agents, coordinated by one host, working while you
sleep. You stay where humans are irreplaceable: **capital, taste, and the
decisions that actually matter.** Everything else is delegated.

This isn't a chatbot with a to-do list. It's a **company runtime** — a durable
host that stands up a roster of specialized agents, gives each one a clear
mandate, and runs them as a coordinated business on top of the OpenHuman and
TinyHumans runtimes.

## What one person can now run

Every folder in [`examples/`](examples/) is a complete company you can launch
today — a roster of agents, their responsibilities, and the handful of moments
where a human signs off:

| You want to run a… | Your agents handle | You keep |
| --- | --- | --- |
| **[Venture Studio](examples/agentic_venture_studio/)** | Scouting, founding, building, launching, operating a portfolio | Capital allocation & strategy |
| **[Startup Accelerator](examples/startup_accelerator/)** | Sourcing, screening, mentoring, demo day, investor intros | Investment decisions |
| **[VC Firm](examples/agentic_venture_capital/)** | Deal flow, diligence, memos, portfolio support | The final "yes" |
| **[Consulting Firm](examples/agentic_consultation_firm/)** | Research, analysis, modeling, decks, implementation plans | Executive workshops |
| **[Software Company](examples/agentic_software_company/)** | PM, design, frontend, backend, QA, security, docs, support, DevRel | Product direction |
| **[Marketing Agency](examples/agentic_marketing_agency/)** | Creative, copy, SEO, paid, email, landing pages, analytics | Campaign sign-off |
| **[Design Studio](examples/agentic_design_studio/)** | Branding, UI, motion, illustration, user testing | Creative direction |
| **[Media Company](examples/agentic_media_company/)** | Finding, verifying, writing, illustrating, distributing stories | Editorial standards |
| **[Influencer Brand](examples/agentic_influencer_business/)** | Scripting, editing, thumbnails, posting, community, sponsorships | Your face (or an avatar) |
| **[Game Studio](examples/agentic_game_studio/)** | Worlds, story, code, art, QA, balance, launch | Creative direction |
| **[Game Business](examples/agentic_game_business/)** | UA, monetization, LiveOps, community, store optimization | Growth strategy |
| **[Recruiting Firm](examples/agentic_recruiting_company/)** | Sourcing, outreach, screening, interviews, offers | Final hiring calls |
| **[Enterprise Sales](examples/agentic_enterprise_sales/)** | Lead gen, outreach, CRM, proposals, contracts, follow-up | Closing strategic accounts |
| **[Support Org](examples/agentic_customer_support/)** | Tickets, docs, bug reports, escalations, refunds | Policy & escalation |
| **[Real Estate Co](examples/agentic_realestate_company/)** | Sourcing, analysis, underwriting, contractors, tenants | Purchase approvals |
| **[Accounting Firm](examples/agentic_accounting_firm/)** | Bookkeeping, tax, payroll, forecasting, audit prep | Signing the filings |
| **[Law Firm](examples/agentic_law_firm/)** | Research, drafting, litigation support, discovery, compliance | Approving filings |
| **[Pharma Startup](examples/agentic_pharma_startup/)** | Literature, molecule discovery, simulation, trial planning | The lab work |

Eighteen companies. One operator. Pick one and run it — or run several at once.

## Why it works

- **A real org chart, not a prompt.** Each company is declared as a roster of
  agents with distinct mandates in a simple `agents.toml`. The host instantiates
  them, coordinates them, and keeps them running.
- **Humans in the loop where it counts.** Every harness names the exact
  decisions reserved for you. Delegate the work; keep the judgment.
- **Built on proven runtimes.** OpenCompany is a light host over OpenHuman and
  the TinyHumans agent modules — it reuses their runtime instead of
  reinventing it.
- **Rust-fast and inspectable.** An Axum HTTP surface, a small default build,
  and deeper capabilities behind feature flags. Simple to start, honest to
  operate, easy to test.
- **Yours to own.** GPL-3.0, self-hostable, no lock-in.

## The engine: Medulla

A company of one only works if something can hold the whole company in its head.
That something is **Medulla** — TinyHumans' orchestrator model, purpose-built to
run large fleets of agents as a single coordinated business.

Medulla is **orchestrator-first**. Every event — a customer email, a market
signal, a finished task — lands on a deep orchestration tier that reads the full
picture, decides what matters, and fans the work out across your agents. It
compresses the noise coming in, compiles the right output for each channel going
out, and routes results back into the loop. One brain, many hands.

It's the difference between a pile of chatbots and an actual org that runs. As
your company grows from nine agents to nine hundred, Medulla is what keeps it
coherent, on-strategy, and moving — without you in every message.

**Medulla is a hosted model.** You reach it with a **TinyHumans API key**;
OpenCompany is the open host that points your companies at it.

> Grab your key and request Medulla access at
> **[tinyhumans.ai](https://tinyhumans.ai)**.

## Start your company

```sh
# 0. Get a TinyHumans API key (unlocks Medulla, the orchestrator) and set it
export TINYHUMANS_API_KEY="th-..."

# 1. Pull in the OpenHuman + TinyAgents runtimes
git submodule update --init --recursive

# 2. Boot the company host
cargo run --bin opencompany -- serve --bind 127.0.0.1:8080

# 3. Check a company manifest before you launch it
cargo run --bin opencompany -- check examples/agentic_marketing_agency

# 4. Launch a company — e.g. your one-person marketing agency
cargo run -p agentic-marketing-agency
```

Without a key you can still build, inspect, and explore every company in
`examples/`. Add the key when you're ready to put Medulla in the driver's seat
and let the agents run for real.

Each example loads its manifest, validates it, and prints the company's
effective configuration. Open its `README.md` to see what it does and
`agents.toml` to see (or edit) the team; `opencompany check` reports any
manifest problems in plain language.

Build every company at once, or just the host:

```sh
cargo build --workspace   # the host + all 18 example companies
cargo build               # just the host (fast default build)
```

Compile against vendored TinyAgents, or preview an OpenHuman launch:

```sh
cargo check --features tiny
cargo run --bin opencompany -- open-human --dry-run -- status
```

## Under the hood

OpenCompany is a Rust 2024 workspace. The host crate is a thin operating layer;
each example company is a standalone member crate that inherits from it.

```text
src/app/                Runtime config and shared state
src/company/            Company manifest parsing, validation, and boot
src/server/             Axum HTTP router and handlers
src/openhuman/          OpenHuman launcher seams
src/tiny/               TinyAgents/OpenHuman status surface
src/bin/opencompany.rs  CLI entrypoint
examples/               18 runnable one-person companies (workspace members)
docs/spec/              Architecture reference
docs/modules/           Per-package design docs
vendor/openhuman/       OpenHuman git submodule
vendor/tinyagents/      TinyAgents git submodule
```

Package surfaces: **`app`** (config + shared state), **`company`** (manifest
parsing, validation, and boot), **`server`** (Axum router), **`openhuman`**
(launcher seams), **`tiny`** (vendored TinyAgents status).

See [docs/spec/README.md](docs/spec/README.md) for the architecture reference
and [examples/README.md](examples/README.md) for the full company catalog.

## License

OpenCompany is licensed under the GNU General Public License v3. See
[LICENSE](LICENSE).
