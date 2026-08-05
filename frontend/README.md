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
npm run build         # tsc typecheck + vite bundle -> dist/
npm run preview       # serve the production build
npm run typecheck     # tsc only, over src/
npm run typecheck:e2e # tsc only, over test/ + playwright.config.ts
```

CI runs `npm ci` and all three of `typecheck`, `typecheck:e2e` and `build` in
the `Console` job of `.github/workflows/ci.yml`.

`typecheck` covers `src/` and nothing else — `tsconfig.app.json` is
`include: ["src"]`. The end-to-end suite is a separate TypeScript project
([`tsconfig.e2e.json`](tsconfig.e2e.json)) with its own script, so a broken spec
fails on its own rather than blocking `npm run build`.

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

**CI type-checks these specs and runs none of them** (`typecheck:e2e` is all the
automated coverage `test/e2e/` gets). Type-checking proves a spec compiles, not
that it holds: `workflow-edit-delete.spec.ts` spent months red against a fixture
that was never committed, and nothing reported it. Run the suite before touching
a view it covers.

### What a default-feature host cannot cover

The host `test/e2e/host.sh` starts is the default feature set, which boots the
offline echo brain. That is enough for the great majority of the suite, but four
specs need an agent that actually executes — a build with the `openhuman`
harness and a mocked inference backend — and one needs an external MCP server:

| Spec | Needs |
|------|-------|
| `wiring.spec.ts` | the harness + a mock LLM backend echoing `__MOCK_LLM__` |
| `chat-to-card.spec.ts` (card chip) | an orchestrator that opens a card |
| `workflow-run-history.spec.ts` (durable history) | a workflow run that executes |
| `mcp.spec.ts` | `PW_MCP_SERVER` pointing at a live MCP server |

Point `PW_HOST_BINARY` at a feature-gated build, or `PW_BASE_URL` at a host you
brought up yourself, to cover those.

The `dist/` can be served as static files by any web server (or mounted by the
OpenCompany host); use `window.OPENCOMPANY_CONFIG` to point it at the API.

> This is a Vite/TypeScript app, not a Cargo package — it lives outside the Rust
> crate, so `cargo build` ignores it. Business definitions live one level up in
> [`../companies/`](../companies/); this one console serves them all.
