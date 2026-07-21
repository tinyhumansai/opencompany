<p align="center">
  <img src="https://raw.githubusercontent.com/tinyhumansai/opencompany/refs/heads/main/gitbooks/.gitbook/assets/opencompany.png" alt="OpenCompany" />
</p>

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
  <a href="https://github.com/tinyhumansai/opencompany/blob/main/LICENSE"><img src="https://img.shields.io/github/license/tinyhumansai/opencompany?style=flat-square" alt="License: GPL-3.0" /></a>
  <a href="https://github.com/tinyhumansai/opencompany/stargazers"><img src="https://img.shields.io/github/stars/tinyhumansai/opencompany?style=flat-square" alt="GitHub stars" /></a>
  <a href="https://github.com/tinyhumansai/opencompany/issues"><img src="https://img.shields.io/github/issues/tinyhumansai/opencompany?style=flat-square" alt="Open issues" /></a>
  <a href="https://github.com/tinyhumansai/opencompany/pulls"><img src="https://img.shields.io/github/issues-pr/tinyhumansai/opencompany?style=flat-square" alt="Open pull requests" /></a>
  <a href="https://github.com/tinyhumansai/opencompany/commits/main"><img src="https://img.shields.io/github/last-commit/tinyhumansai/opencompany?style=flat-square" alt="Last commit" /></a>
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

Every folder under [`companies/`](companies/) is a complete company you can
launch today — a roster of agents, their responsibilities, and the handful of
moments where a human signs off:

| You want to run a… | Your agents handle | You keep |
| --- | --- | --- |
| **[Venture Studio](companies/agentic_venture_studio/)** | Scouting, founding, building, launching, operating a portfolio | Capital allocation & strategy |
| **[Startup Accelerator](companies/startup_accelerator/)** | Sourcing, screening, mentoring, demo day, investor intros | Investment decisions |
| **[VC Firm](companies/agentic_venture_capital/)** | Deal flow, diligence, memos, portfolio support | The final "yes" |
| **[Consulting Firm](companies/agentic_consultation_firm/)** | Research, analysis, modeling, decks, implementation plans | Executive workshops |
| **[Software Company](companies/agentic_software_company/)** | PM, design, frontend, backend, QA, security, docs, support, DevRel | Product direction |
| **[Marketing Agency](companies/agentic_marketing_agency/)** | Creative, copy, SEO, paid, email, landing pages, analytics | Campaign sign-off |
| **[Design Studio](companies/agentic_design_studio/)** | Branding, UI, motion, illustration, user testing | Creative direction |
| **[Media Company](companies/agentic_media_company/)** | Finding, verifying, writing, illustrating, distributing stories | Editorial standards |
| **[Influencer Brand](companies/agentic_influencer_business/)** | Scripting, editing, thumbnails, posting, community, sponsorships | Your face (or an avatar) |
| **[Game Studio](companies/agentic_game_studio/)** | Worlds, story, code, art, QA, balance, launch | Creative direction |
| **[Game Business](companies/agentic_game_business/)** | UA, monetization, LiveOps, community, store optimization | Growth strategy |
| **[Recruiting Firm](companies/agentic_recruiting_company/)** | Sourcing, outreach, screening, interviews, offers | Final hiring calls |
| **[Enterprise Sales](companies/agentic_enterprise_sales/)** | Lead gen, outreach, CRM, proposals, contracts, follow-up | Closing strategic accounts |
| **[Support Org](companies/agentic_customer_support/)** | Tickets, docs, bug reports, escalations, refunds | Policy & escalation |
| **[Real Estate Co](companies/agentic_realestate_company/)** | Sourcing, analysis, underwriting, contractors, tenants | Purchase approvals |
| **[Accounting Firm](companies/agentic_accounting_firm/)** | Bookkeeping, tax, payroll, forecasting, audit prep | Signing the filings |
| **[Law Firm](companies/agentic_law_firm/)** | Research, drafting, litigation support, discovery, compliance | Approving filings |
| **[Pharma Startup](companies/agentic_pharma_startup/)** | Literature, molecule discovery, simulation, trial planning | The lab work |
| **[Signals + Opportunity Studio](companies/signals_opportunity_studio/)** | Scouting signals, clustering pains, ranking opportunities into a weekly brief | Which opportunities to fund |

Nineteen companies. One operator. Pick one and run it — or run several at once.

**Signals and the Opportunity Engine are a template, not kernel code.** The
[Signals + Opportunity Studio](companies/signals_opportunity_studio/) realizes
them as a roster, a charter, and a weekly `[[schedule]]` over the existing
channel, memory, and brain ports — nothing new in the runtime.

## Why it works

- **A real org chart, not a prompt.** Each company is declared as a roster of
  agents with distinct mandates in a simple `company.toml`. The host
  instantiates them, coordinates them, and keeps them running.
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

# 2. Check a company definition before you launch it
cargo run --bin opencompany -- check companies/agentic_marketing_agency

# 3. Launch that company on the host — pick any folder under companies/
cargo run --bin opencompany -- serve --company companies/agentic_marketing_agency
```

The host is one configurable backend; each folder under
[`companies/`](companies/) is a business definition (a `company.toml` manifest
plus docs), not its own program. Point `--company` at a different folder to run
a different business.

Without a key you can still build, inspect, and explore every company in
`companies/`. Add the key when you're ready to put Medulla in the driver's seat
and let the agents run for real.

To let companies trade with other agents on tiny.place, build with the
`tinyplace` feature and pass `serve --discoverable` to opt every loaded company
into going public (register a `@handle`, publish an Agent Card, and answer
inbound A2A `tasks/send` over SIWX + x402). See
[`docs/modules/server/README.md`](docs/modules/server/README.md) for the full
discovery flow and the `TINYPLACE_API_URL` / `OPENCOMPANY_PUBLIC_URL` settings.

Each company folder holds a `company.toml` (the manifest — the team, the
output, the human's role) and a `README.md` describing the business. Edit the
manifest to reshape the team; `opencompany check` reports any problems in plain
language. Adding a new business is a new folder, not a new program.

```sh
cargo build               # the host (the one configurable backend)
```

## Run it anywhere (Docker)

One script spins up a company **and** its [operator console](frontend/) in
development mode. Pass a friendly site name (or any directory name under
`companies/`) and keep the stack attached to the terminal:

```sh
./scripts/launch-demo.sh marketing up     # console → :5173, host API → :8080
# Press Ctrl-C when finished, then destroy its containers and network:
./scripts/launch-demo.sh marketing down
# Or destroy the stack and its persistent data volume:
./scripts/launch-demo.sh marketing down -v
```

The launcher bind-mounts the local checkout. Vite hot-updates frontend edits;
`cargo-watch` rebuilds and restarts the backend when Rust source, Cargo files,
or company definitions change. The first start builds the development images
and dependencies; later launches reuse named Cargo and `node_modules` caches.

Use `./scripts/list-demos.sh` to list friendly names and every available
company. Each company uses a separate Compose project and persistent data
volume. `down` removes its containers and network but keeps that volume;
`down -v` deletes the volume and its data too.

For custom ports, credentials, or feature flags, copy `.env.example` to `.env`
before launching. For production-like images without source mounts or hot
reload, run `OPENCOMPANY_COMPANY=marketing docker compose up --build` directly.

The same two images deploy to DigitalOcean (App Platform spec in
[`.do/app.yaml`](.do/app.yaml)), AWS (Fargate task in
[`deploy/`](deploy/aws-ecs-task-definition.json)), or any Docker host. See
[`deploy/README.md`](deploy/README.md).

Compile against vendored TinyAgents, or preview an OpenHuman launch:

```sh
cargo check --features tiny
cargo run --bin opencompany -- open-human --dry-run -- status
```

## Under the hood

OpenCompany is a Rust 2024 crate: one configurable host. Business types are
data, not code — a manifest plus docs — and the operator console is a separate
Vite app.

```text
src/app/                Runtime config and shared state
src/company/            Company manifest parsing, validation, and boot
src/ports/              Kernel port traits and shared types
src/store/              File-based CompanyStore/EventLog/Memory/Context/Secrets
src/policy/             Manifest-driven ApprovalGate
src/brain/              Offline EchoBrain (the default cognition seam)
src/feedback/           Feedback items, privacy scrubber, GitHub issue filing
src/runtime/            CompanyRuntime, CycleRunner, cron scheduler, registry
src/server/             Axum HTTP router and handlers
src/server/users/       Human sign-in: magic link, passwords, sessions, invites
src/openhuman/          OpenHuman launcher seams
src/tiny/               TinyAgents/OpenHuman status surface
src/bin/opencompany.rs  CLI entrypoint
companies/              19 business definitions (a company.toml + docs each)
frontend/               Company-agnostic operator console (Vite + React)
docs/spec/              Architecture reference
docs/modules/           Per-package design docs
vendor/openhuman/       OpenHuman git submodule
vendor/tinyagents/      TinyAgents git submodule
```

Package surfaces: **`app`** (config + shared state), **`company`** (manifest
parsing, validation, and boot), **`ports`** (kernel trait seams),
**`store`** (file-based default stores), **`policy`** (approval gate),
**`brain`** (offline cognition seam), **`runtime`** (company runtime + cycle
loop), **`server`** (Axum router), **`openhuman`** (launcher seams),
**`tiny`** (vendored TinyAgents status).

See [docs/spec/README.md](docs/spec/README.md) for the architecture reference
and [companies/README.md](companies/README.md) for the full company catalog.

## Documentation

The full docs live in [`gitbooks/`](gitbooks/README.md): what OpenCompany is,
what one person can run, how [Medulla](gitbooks/overview/medulla.md) drives it,
and the [tiny.place economy](gitbooks/overview/tiny-place.md). Builders should
start with the [developer section](gitbooks/developers/README.md) — build,
CLI, architecture, authoring companies, deployment, and configuration.

## License

OpenCompany is licensed under the GNU General Public License v3. See
[LICENSE](LICENSE).
