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

A dashboard shell (collapsible sidebar + topbar, light/dark/system theme) wraps
one company's views. Navigation is **hash-routed** (`#/conversation`), so views
are linkable and survive a refresh.

| View | What it does |
|---|---|
| **Overview** | Status, pending-approval and conversation stat cards, quick actions |
| **Conversation** | WhatsApp-style two-pane chat: a thread list (company line + desks) on the left, the selected transcript + composer on the right |
| **Tasks** | A built-in Kanban board (drag cards between columns) |
| **Approvals** | The inbox of things parked for your decision, with approve/decline |
| **Workflows** | A read-only [React Flow](https://reactflow.dev) canvas of how work is routed (lazy-loaded) |
| **Connections** | OAuth connection catalog (Gmail, Slack, GitHub, …); degrades to read-only when the host has no connections surface |
| **Settings** | Connection details, lifecycle controls (pause/resume/suspend/archive), appearance |
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

The console introduces many surfaces (Team, Skills, Workspace, Memory, Usage,
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
  prop-driven views and pieces (`Conversation`, `TasksView`, `WorkflowsView`,
  `FeedbackForm`, …).

## Build

```sh
npm run build     # tsc typecheck + vite bundle -> dist/
npm run preview   # serve the production build
npm run typecheck # tsc only
```

The `dist/` can be served as static files by any web server (or mounted by the
OpenCompany host); use `window.OPENCOMPANY_CONFIG` to point it at the API.

> This is a Vite/TypeScript app, not a Cargo package — it lives outside the Rust
> crate, so `cargo build` ignores it. Business definitions live one level up in
> [`../companies/`](../companies/); this one console serves them all.
