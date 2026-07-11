# OpenCompany Console

A single, **company-agnostic** operator console for any OpenCompany host —
built with Vite + React + TypeScript. One build talks to any company on any
host, discovered at runtime, so it is reused everywhere instead of shipping a
bespoke UI per example.

It is an operator surface, not a dashboard of internals: you chat with **the
company** (one voice), see the few things it parked for **your approval**, and
**flag** anything that was wrong. Per the spec's language rules, it never
exposes runtime mechanics ("agent graph", "tier", "dispatch", "cycle") — every
label goes through [`src/lib/language.ts`](src/lib/language.ts).

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

## Pluggable pieces

Everything is decoupled so you can embed parts elsewhere:

- [`src/api/client.ts`](src/api/client.ts) — a typed `OpenCompanyClient` with no
  React dependency; use it from any TS app.
- [`src/api/types.ts`](src/api/types.ts) — the API payload types, mirrored from
  the Rust server.
- [`src/components/`](src/components/) — `Chat`, `Approvals`, `StatusBar`,
  `FeedbackDialog`, `CompanyPicker` — prop-driven and reusable.
- [`src/theme.css`](src/theme.css) — a token-based design system (light/dark);
  swap the CSS variables to reskin.

## Build

```sh
npm run build     # tsc typecheck + vite bundle -> dist/
npm run preview   # serve the production build
```

The `dist/` can be served as static files by any web server (or mounted by the
OpenCompany host); use `window.OPENCOMPANY_CONFIG` to point it at the API.

> This is a Vite/TypeScript app, not a Cargo package — it lives outside the Rust
> crate, so `cargo build` ignores it. Business definitions live one level up in
> [`../companies/`](../companies/); this one console serves them all.
