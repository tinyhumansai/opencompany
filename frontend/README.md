# OpenCompany Console

A single, **company-agnostic** operator console for any OpenCompany host —
built with **Vite + React + TypeScript + Tailwind v4 + [shadcn/ui]**. One build
talks to any company on any host, discovered at runtime, so it is reused
everywhere instead of shipping a bespoke UI per example.

It is an operator surface: you chat with **the company**, see the few things it
parked for **your approval**, watch its **workflows**, and **flag** anything
that was wrong. Per the spec's language rules, product text never exposes
runtime mechanics ("agent graph", "tier", "dispatch", "cycle") — every label
goes through [`src/lib/language.ts`](src/lib/language.ts).

[shadcn/ui]: https://ui.shadcn.com

## What's inside

A dashboard shell (collapsible sidebar, light/dark/system theme) wraps one
company's views. Navigation is **hash-routed** (`#/chat`, and `#/chat/strategy`
or `#/settings/people` for a view with sub-pages), so every surface is linkable
and survives a refresh.

| View | What it does |
|---|---|
| **Overview** | The company's knowledge graph, full-bleed — see [`src/views/overview/README.md`](src/views/overview/README.md) |
| **Chat** | A channel-and-DM workspace: channel rail, threaded timeline, composer, thread panel, and the roster in a side pane — see [`src/views/chat/README.md`](src/views/chat/README.md) |
| **Tasks** | A built-in Kanban board (drag cards between columns) |
| **Approvals** | The inbox of things parked for your decision, with approve/decline |
| **Workflows** | A read-only [React Flow](https://reactflow.dev) canvas of how work is routed (lazy-loaded) |
| **Settings** | A section with its own nav: General (connection, lifecycle, domain, mail), People, Connections, MCP Servers |
| **Feedback** | The scrub-then-preview feedback flow, plus a Join-our-Discord nudge |

## Run it

Start a company host, then the console dev server (it proxies the API, so no
CORS in dev):

```sh
# 1. From the repo root — a company on 127.0.0.1:8080
cargo run --bin opencompany -- serve --company companies/agentic_marketing_agency

# 2. From frontend/ — the console on http://localhost:5173
npm install
npm run dev
```

Point the dev proxy at a host elsewhere with `OC_API_TARGET`:

```sh
OC_API_TARGET=http://192.168.1.20:8080 npm run dev
```

## Agnostic by configuration

The same build works against any host/company. Resolution order (first wins):

1. **URL query** — `?api=<url>&company=<id>&token=<token>`
2. **Runtime global** — `window.OPENCOMPANY_CONFIG` (set in `index.html`; for
   serving the built `dist/` as static files with no rebuild)
3. **Build env** — `VITE_OC_API`, `VITE_OC_COMPANY`, `VITE_OC_TOKEN`
4. **Defaults** — same-origin API, single-company mode

- **Single-company (prosumer)** hosts: omit `company`; the console
  auto-selects the sole company (falling back to the `/api/v1/company/*`
  aliases).
- **Multi-company (platform)** hosts: it lists companies and shows a picker;
  `?company=<id>` jumps straight in. Add `?token=` for platform/operator auth.

## Design system

- **Tokens** live in [`src/index.css`](src/index.css) — the shadcn "new-york"
  neutral theme (OKLCH CSS variables, light + `.dark`). Swap the variables to
  reskin; theming is driven by `next-themes`.
- **Primitives** are shadcn/ui on **Base UI** under
  [`src/components/ui/`](src/components/ui/) — owned in-tree, add more with
  `npx shadcn@latest add <component>`.
- Base UI composes with the `render` prop (not Radix's `asChild`).

## Architecture & backend contract

The console introduces many surfaces (Skills, Workspace, Memory, Usage,
Finances, Connections, Inbox, Domain/SMTP, …). Most are built to a **seam +
client-side fallback** pattern so the host-side APIs can land incrementally.
[`ARCHITECTURE.md`](ARCHITECTURE.md) is the full brief: every surface, its data,
the proposed endpoint contract, and the company-directory conventions the
backend should read.

## Pluggable pieces

Everything is decoupled so you can embed parts elsewhere:

- [`src/api/client.ts`](src/api/client.ts) — a typed `OpenCompanyClient` with no
  React dependency; use it from any TS app. Includes a forward-looking
  `connections` seam that light hosts can ignore.
- [`src/api/types.ts`](src/api/types.ts) — the API payload types, mirrored from
  the Rust server.
- [`src/views/`](src/views/) and [`src/components/`](src/components/) —
  prop-driven views and pieces (`ChatView`, `TasksView`, `WorkflowsView`,
  `FeedbackForm`, …).

## Build

```sh
npm run build          # tsc typecheck + vite bundle -> dist/
npm run preview        # serve the production build
npm run typecheck      # tsc only, over src/
npm run typecheck:e2e  # tsc only, over test/e2e/ + playwright.config.ts
npm run typecheck:unit # tsc only, over test/unit/ + vitest.config.ts
```

CI runs `npm ci`, then `typecheck`, `typecheck:e2e`, `typecheck:unit`, `test`
and `build`, in the `Console` job of `.github/workflows/ci.yml`.

`typecheck` covers `src/` and nothing else — `tsconfig.app.json` is
`include: ["src"]`. Each test suite is a separate TypeScript project with its
own script ([`tsconfig.e2e.json`](tsconfig.e2e.json),
[`tsconfig.unit.json`](tsconfig.unit.json)), so a broken test fails on its own
rather than blocking `npm run build`.

## Unit suite

```sh
npm test              # vitest, once — this is what CI runs
npm run test:watch    # re-runs on change while you work
```

Pure functions only, under [`test/unit/`](test/unit). The whole suite is
sub-second, so it runs on every push and there is never a reason to skip it.

**What belongs here versus in the browser suite.** This runner is for a helper
that maps A to B with no document, no host and no React — id reconciliation,
channel-id derivation and the legacy-URL shim, link precedence on a card,
timeline folding, anything that truncates or folds a value. The end-to-end suite
below is for what is only true in a browser driving a live host: a disabled
affordance explaining itself, a banner that must not be a toast, a redirect that
survives a full-page navigation.

The line matters because each is tempted into the other's territory. A browser
walk *can* reach a pure helper — through six layers of render, in forty seconds,
reporting the failure as "the board looked wrong". A unit test cannot reach a
redirect at all. Put a helper here the moment it has a second caller or a branch
worth naming.

A test earns its place by being **seen failing** against the behaviour it
guards. Every test in `test/unit/` was proven red by breaking its subject before
it was trusted — a test that passes while asserting nothing is worse than no
test, because it reports coverage.

## End-to-end suite

```sh
cargo build --locked --bin opencompany   # once, from the repository root
npm run e2e                              # boots a host, signs in, runs test/e2e/
npm run e2e -- workflow-edit-delete.spec.ts   # one file
npm run e2e:headed                       # watch it drive the browser
```

The specs drive a **real** host — the Rust binary serving this app's `dist/` —
so one has to exist. With `PW_BASE_URL` unset,
[`playwright.config.ts`](playwright.config.ts) starts one itself through
[`test/e2e/host.sh`](test/e2e/host.sh): the `e2e_harness` company, a freshly
built console bundle, and an isolated data root under `../target/e2e/`, wiped
each run. It does not build the binary — that is minutes of silence, and a test
harness that looks like it has hung is worse than one that tells you what to
run.

Set `PW_BASE_URL` to drive a host you brought up yourself and the config stays
out of the way entirely: no `webServer`, and `PW_STORAGE_STATE` decides whether
the suite signs in.

**CI runs this suite** in two jobs. `Console E2E` drives a default-feature host
built by the `Rust` job and passed across as an artifact (issue #428).
`Console E2E (live brain)` drives a feature-gated one from `Rust (openhuman,
tinycortex)`, with the fixtures below behind it, and is the only thing that runs
the four specs described next (issue #467).

Neither existed for a long time: `typecheck:e2e` was the only automated coverage
`test/e2e/` had, and type-checking proves a spec compiles, not that it holds.
`workflow-edit-delete.spec.ts` spent months red against a fixture that was never
committed; two further specs were found red against product changes that had
been deliberate, one of which had been filed as a bug that did not exist.
Nothing reported any of it, because nothing ran it.

Run the suite before touching a view it covers — CI is a backstop, not a
substitute for seeing your own change work.

### The four specs a default-feature host cannot run

The host `test/e2e/host.sh` starts is the default feature set, which boots the
offline echo brain. That is enough for the great majority of the suite, but four
specs need an agent that actually executes — a build with the `openhuman`
harness and something for it to think with.

Against a default host they **skip themselves** rather than failing, through
[`test/e2e/capabilities.ts`](test/e2e/capabilities.ts), which is a true
statement about that host rather than a debt. Against a gated one they run:

| Spec | What it proves | Needs |
|------|----------------|-------|
| `wiring.spec.ts` | a typed message reaches the backend and its reply renders | the harness + `mock-brain.mjs` |
| `chat-to-card.spec.ts` (card chip) | an orchestrator opens a board card, and the chip survives a reload | a scripted tool choice (`SPAWNONE`) |
| `workflow-run-history.spec.ts` (durable history) | a run is journaled and outlives the console | the workflow runner |
| `mcp-agent.spec.ts` | an agent calls a tool on a registered MCP server | `mcp-server.mjs` |

To run them:

```sh
cargo build --locked --features openhuman,tinycortex,mcp --bin opencompany
npm --prefix frontend run e2e:live       # PW_LIVE_BRAIN=1 npm run e2e
```

`PW_LIVE_BRAIN=1` is a **declaration**, not a probe — nothing in a host's
answers distinguishes a gated build from a default one, which
[`capabilities.ts`](test/e2e/capabilities.ts) explains at length. When the run
also manages the host, that flag additionally starts two fixtures and points the
host at them:

* [`test/e2e/mock-brain.mjs`](test/e2e/mock-brain.mjs) — an OpenAI-compatible
  chat-completions and embeddings endpoint with no model behind it. Ordinary
  turns get a fixed line carrying `__MOCK_LLM__`; a prompt carrying `SPAWNONE`
  makes it call `spawn_task` **once**, and one carrying `__MOCK_TOOL_CALL__ {…}`
  makes it call exactly the named tool, once. "Once" is the part with teeth: one
  operator message reaches several agents and several model calls, so the server
  tracks directive identity rather than trusting the transcript. Bind with
  `PW_MOCK_BRAIN_BIND` (default `127.0.0.1:8099`).

* [`test/e2e/mcp-server.mjs`](test/e2e/mcp-server.mjs) — an HTTP MCP server with
  two tools. HTTP, not stdio: this host rejects any MCP declaration carrying a
  `command`. Bind with `PW_MCP_FIXTURE_BIND` (default `127.0.0.1:8098`), or name
  a server of your own in `PW_MCP_SERVER` (a URL).

Against a host you brought up yourself (`PW_BASE_URL`), the flag still enables
the four specs, but starting the fixtures and pointing the host at them is
yours to do — this config will not reconfigure a host it did not launch.

Some specs skip **the other way**, in the live lane only, and say so where they
sit: three in `chat-live-events.spec.ts`, which find the reply to their own turn
by the offline brain's `You said: <text>` (precisely how they prove an SSE frame
carried the answer to *that* message, and precisely why a different brain breaks
them), and the Planning drag in `board-columns.spec.ts`, where a planner-attached
host settles the card rather than leaving it parked. The default lane runs all of
them on every push.

The managed host starts from an **empty** environment and is handed only what it
needs, so an inherited `OPENCOMPANY_PUBLIC_URL`, `OPENCOMPANY_MAIL_*`,
`OPENCOMPANY_STORAGE` or `OPENCOMPANY_TENANT_ID` cannot quietly change what you
are testing — the first two would stop the host echoing the sign-in code and
strand the suite in bootstrap. Name anything else it should receive, such as a
feature-gated build's inference credentials, in `PW_HOST_PASSTHROUGH` (a
space-separated list of variable names).

`PW_HOST_DATA_DIR` is wiped at the start of each run, so a run only ever deletes
inside `../target/e2e`. Point it anywhere else and it is reused as it stands,
with a line saying so — a mistyped or inherited value cannot take a directory
you care about with it.

The `dist/` can be served as static files by any web server (or mounted by the
OpenCompany host); use `window.OPENCOMPANY_CONFIG` to point it at the API.

> This is a Vite/TypeScript app, not a Cargo package — it lives outside the Rust
> crate, so `cargo build` ignores it. Business definitions live one level up in
> [`../companies/`](../companies/); this one console serves them all.
