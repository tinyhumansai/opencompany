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
| **Settings** | A section with its own nav: General (connection, lifecycle, domain, mail), People, OAuth, MCP Servers, Inference |
| **Feedback** | The scrub-then-preview feedback flow, plus a Join-our-Discord nudge |

## Run it

**Node 22 or newer** — `.nvmrc` pins it and `engines.node` declares it, so
`nvm use` picks it up and `npm` warns if you are below it. CI and
`frontend/Dockerfile` both build on 22; the floor exists because a version
mismatch does not announce itself as one. It surfaces wherever the newer
runtime happens to have moved a global, deep inside a dependency, and reads as
a dependency bug (issues #852 and #858).

The desktop build additionally needs **pnpm 10 or newer** — `tauri.conf.json`
runs `pnpm dev` / `pnpm build`, and `pnpm-workspace.yaml` uses a key pnpm 9
cannot parse.

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

- **Tokens** live in [`src/index.css`](src/index.css), in three layers —
  primitive ramps → semantic names → Tailwind utilities. Components may only
  use the third. Light lives in `:root`, dark in `.dark`; theming is driven by
  `next-themes`.
- **Living reference:** open **`#/styleguide`** — every token and shipped UI
  primitive, rendered by this stylesheet. It reads the variables at runtime,
  so it cannot drift, and it needs no host, company, or sign-in.
- **Primitives** are shadcn/ui on **Base UI** under
  [`src/components/ui/`](src/components/ui/) — owned in-tree, add more with
  `npx shadcn@latest add <component>`.
- Base UI composes with the `render` prop (not Radix's `asChild`).

Written reference, in order of usefulness:

| Doc | Answers |
| --- | --- |
| [`docs/design-system/README.md`](../docs/design-system/README.md) | The layer rule, anti-patterns, how to change a token |
| [`docs/design-system/color.md`](../docs/design-system/color.md) | Every colour, its role, its measured contrast |
| [`docs/design-system/typography.md`](../docs/design-system/typography.md) | The scale, the mono policy, the migration list |
| [`docs/design-system/components.md`](../docs/design-system/components.md) | Anatomy and required states per primitive |
| [`docs/brand/README.md`](../docs/brand/README.md) | Why these choices — positioning, voice, form |

> Two rules save the most time. **Never write an arbitrary value**
> (`text-[11px]`, `bg-[#5865f2]`) — the scale has a name for it, or the system
> needs one. **Never assemble a class name from a template** — Tailwind scans
> source text, so `` `bg-status-${key}` `` is never generated and fails
> silently.

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
  prop-driven views and pieces (`ChatView`, `LedgersView`, `WorkflowsView`,
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
| `orchestration-simulation.spec.ts` | **the whole loop**: a goal stated in chat is delegated to two teammates, dispatched from the board, worked, and closed out by review | scripted turns (`__MOCK_PLAN__`) |

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
  tracks directive identity rather than trusting the transcript. A message
  carrying `__MOCK_PLAN__ [[…],[…]]` scripts a whole **turn** instead of a single
  call — several calls in one assistant message, and several steps across the
  turn's tool loop — which is what lets one goal fan out to two teammates and be
  closed out afterwards. Set `MOCK_BRAIN_DEBUG=1` to have it dump each request
  it receives, which is the fastest way to find out why an arm stopped matching.
  Bind with `PW_MOCK_BRAIN_BIND` (default `127.0.0.1:8099`).

* [`test/e2e/mcp-server.mjs`](test/e2e/mcp-server.mjs) — an HTTP MCP server with
  two tools. HTTP, not stdio: this host rejects any MCP declaration carrying a
  `command`. Bind with `PW_MCP_FIXTURE_BIND` (default `127.0.0.1:8098`), or name
  a server of your own in `PW_MCP_SERVER` (a URL).

Against a host you brought up yourself (`PW_BASE_URL`), the flag still enables
the four specs, but starting the fixtures and pointing the host at them is
yours to do — this config will not reconfigure a host it did not launch.

### The lane with a real model in it

```sh
cargo build --locked --features openhuman,tinycortex,mcp --bin opencompany
npm --prefix frontend run e2e:live-llm    # PW_LIVE_LLM=1 npm run e2e
```

One spec, `orchestration-live.spec.ts`, and one claim the scripted lane cannot
make: that a **model**, handed a goal and this company's real roster and tool
descriptions, decides to break the goal up, give the pieces to the right people,
and accept the results afterwards. A prompt that stopped describing the board, a
tool description that stopped saying what it is for, a roster the orchestrator
can no longer see — every one of those leaves the scripted lane green, because
the scripted lane never reads them.

The run narrows itself to that spec, exactly as the first-run lane does and for
the same reason: every other spec asserts on the mock's answers, and a host
thinking with a real model gives none of them.

* [`test/e2e/live-brain-proxy.mjs`](test/e2e/live-brain-proxy.mjs) sits between
  the host and the router. It forwards `/chat/completions` untouched — nothing
  is scripted, filtered or retried — and supplies the two things a plain
  upstream will not: `/embeddings`, which those routers answer `404` for and the
  host validates the width of, and the model name, so the rung is named once
  here rather than through the host's own `OPENCOMPANY_INFERENCE_MODEL`. It logs
  one line per turn naming the tool calls the model chose, because "never asked"
  and "asked and chose nothing" are otherwise the same silence.
* Point it with `PW_LIVE_LLM_URL` (default `http://127.0.0.1:6969/v1`),
  `PW_LIVE_LLM_MODEL` (default `flash`) and `PW_LIVE_LLM_KEY` (default
  `$LADDER_API_KEY`); bind with `PW_LIVE_LLM_BIND` (default `127.0.0.1:8096`).

**CI does not run it**, deliberately: it spends tokens and its verdict is a
model's judgement, so a model having a bad day would turn unrelated pull
requests red. Run it by hand before changing an orchestrator prompt, a
delegation tool's description, or the delegation drain — and let
`orchestration-simulation.spec.ts`, which asserts the same chain against
scripted choices, be what guards it on every push.

### The lane with a right answer

```sh
cargo build --locked --features openhuman,tinycortex,mcp --bin opencompany
npm --prefix frontend run e2e:euler       # PW_LIVE_LLM=1 PW_EULER=1 npm run e2e
PW_EULER_PROBLEM=61 npm --prefix frontend run e2e:euler   # a different rung
```

Every other spec in this directory asserts that the machinery *ran*: a card was
opened, a turn fired, a marker appeared, a column changed. Those are the right
claims and they share one limit — what the company actually produced is prose,
and prose has no pass condition. A company that delegated correctly, ran every
turn, closed every card and reached a confidently wrong conclusion is green
everywhere else here.

`euler-live.spec.ts` closes that. It serves
[`companies/agentic_math_lab`](../companies/agentic_math_lab) — a roster split
into decide / program / break, with no `web` and no `search` grant — states a
Project Euler problem in the main line, dispatches whatever the orchestrator
opens, keeps asking until the work settles, and then compares the integer the
lab reports against the published one. The verdict is that integer, so what
passes is not "the orchestration ran" but "the orchestration produced the right
answer" — and the spec additionally requires that the lab actually *ran*
something, because every published answer is in a model's training data and
recall would otherwise pass. Withholding `web`/`search` removes the obvious
shortcut but is not a network boundary (`shell` is granted; see
`docs/spec/security/agent-isolation.md`), so the program on disk is what
carries the claim.

* `PW_EULER_PROBLEM` picks the problem (default `100`); the set, each statement
  and each published answer live in [`test/e2e/euler.ts`](test/e2e/euler.ts).
  All of them are settled by a program that finishes in seconds once the right
  program is written, so a red run means the lab could not work out *what* to
  compute rather than that a sandbox timed out.
* `PW_EULER_ROUNDS` (default `6`) is how many times the operator says "carry
  on". A turn ends when the model stops calling tools and a hard problem does
  not fit in one, so the spec keeps asking — and the loop exits the moment the
  answer appears.
* Only the answer is asserted. Which tools were used, how the work was split,
  how many rounds it took and whether the answer was filed on the `answers`
  ledger are attached to the run as annotations and never failed on; a lane that
  failed a correct answer over its bookkeeping would be measuring diligence
  rather than capability.

It uses the same real-model proxy and the same environment variables as the lane
above, on a company and a data root of its own, and **CI does not run it** for
the same reasons plus one more: it takes tens of minutes.

### The lane that compares pixels

```sh
cargo build --locked --bin opencompany   # the ordinary default-feature host
npm run e2e:visual                       # compare against the committed baselines
npm run e2e:visual:update                # re-record them
```

`visual.spec.ts` renders each top-level surface — Overview, Tasks, Workflows,
Company, Memory, Inbox, Approvals, Settings — full-page in both themes and
compares it against a PNG in
[`test/e2e/visual.spec.ts-snapshots/`](test/e2e/visual.spec.ts-snapshots/).

It is the only spec here that judges a page by how it looks, and that is the
point of keeping it apart. Every other spec asserts a named quantity, and
[`shell-two-layer.spec.ts`](test/e2e/shell-two-layer.spec.ts) says why at
length: an inset of one pixel over a flat tint is structurally a two-layer
shell and visually nothing, and only "eight pixels on all four sides, a fill
measurably different from the chrome" fails that. None of those assertions
should become "it looks like it did last week".

What a baseline catches is the complement — the regression nobody had a
quantity for, because nobody knew to write one. A token that shifted lightness
across every surface. A web font that stopped loading and fell back. Padding
lost on one view out of eight. A reviewer spots all three in a screenshot in a
second and in a diff not at all.

**CI does not run it.** Baselines are per-platform — Playwright suffixes each
file with the platform name, and the ones committed here were recorded on
`linux`. A required check that is red for everyone not on the recording
platform teaches people to reach for `--update-snapshots` without looking,
which is how a baseline suite stops meaning anything. Run it either side of a
styling change and read the diff Playwright writes into `playwright-report/`.

The false-positive rate is what makes this worth having, so the spec leaves the
page on the real clock — the console paints time-derived labels *relative to
now*, and a frozen page clock against a host that keeps real time would make
those labels less stable, not more — and masks the labels that would otherwise
drift. It also disables animations, waits on `document.fonts.ready`, hides the
fading overlay scrollbar, and masks regions whose *value* legitimately changes
between runs. To exempt something new, put `data-visual-volatile` on it at the
call site rather than adding a CSS path to the mask list — a path stops masking
anything the day it changes, and a mask that matches nothing looks exactly like
a mask that was not needed.

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

The managed host's default port is derived from this checkout's path, so every
worktree gets one of its own and no run can silently adopt another's host — a
fixed default collides SILENTLY, because `reuseExistingServer` is on outside CI
and a second run on the busy port does not fail: it drives the *other*
worktree's host and reports on code that is not its own. `PW_HOST_BIND` names
the bind explicitly when the derived default is not the one you want —
`PW_HOST_BIND=127.0.0.1:8123 npm run e2e` is a run that cannot collide with
anyone. `PW_BASE_URL` moves the port too, but by handing the host over to you
entirely.

`PW_HOST_DATA_DIR` is wiped at the start of each run, so a run only ever deletes
inside `../target/e2e`. Point it anywhere else and it is reused as it stands,
with a line saying so — a mistyped or inherited value cannot take a directory
you care about with it.

A derived port makes a collision unlikely; it does not make one *loud*. On
2026-08-25 a suite **passed** against the wrong server — a sibling agent's dev
server held the port, `reuseExistingServer` adopted it, and the run drove another
worktree's bundle serving a different company. No readiness check could have
caught it: Playwright's `url` check reads the status code and discards the body,
and `/healthz` is a hardcoded `{"status":"ok"}` naming no instance.

So `test/e2e/global-setup.ts` — which runs *after* `webServer` resolves, and is
therefore the only hook that sees the server actually adopted — asks `/spec` who
answered and aborts before a single spec runs unless it is an OpenCompany host
(`application/json`, `name` is `opencompany`) **and** the right one, by
`instance_id` against the `instance-id` file the host mints under
`PW_HOST_DATA_DIR`. Both halves are needed: a dev server proxies `/spec` to
whatever host it points at, so the incident above satisfies the type check alone.
Nothing is pinned or cached, so a host restarted between runs is not an impostor.
Against a host you brought yourself there is no root of ours to compare against —
set `PW_EXPECTED_INSTANCE_ID` to the id you expect. Full reasoning:
[`test/e2e/host-identity.ts`](test/e2e/host-identity.ts).

The `dist/` can be served as static files by any web server (or mounted by the
OpenCompany host); use `window.OPENCOMPANY_CONFIG` to point it at the API.

## Driving the console from an agent

[`.mcp.json`](../.mcp.json) at the repository root registers two browser MCP
servers, so an agent working in this checkout can open the console and look at
it rather than reasoning about the markup:

| Server | Good for |
|--------|----------|
| `chrome-devtools` | CDP: computed styles, console, network, performance traces, Lighthouse |
| `playwright` | accessibility-tree snapshots, clicking through a flow, screenshots |

Both are headless and use a throwaway profile. They need the same browser the
suite does:

```sh
npm install
npx playwright install chromium                          # for the suite and chrome-devtools
npx @playwright/mcp@0.0.79 install-browser chrome-for-testing   # for the playwright server
```

The two are separate downloads because the MCP server pins its own Playwright,
whose browser revision is usually a step ahead of the one in `package.json`.

`chrome-devtools` is launched through
[`test/tools/chrome-devtools-mcp.sh`](test/tools/chrome-devtools-mcp.sh) rather
than directly. The wrapper resolves the browser from `@playwright/test` at
launch — the revision directory is versioned, so a literal path in `.mcp.json`
would break at the next pin bump with a `Target closed` error that says nothing
about paths — and, on a host where AppArmor denies unprivileged user namespaces
(Ubuntu 24.04+), drops the Chromium sandbox, which the downloaded build cannot
start without a distribution profile.

> This is a Vite/TypeScript app, not a Cargo package — it lives outside the Rust
> crate, so `cargo build` ignores it. Business definitions live one level up in
> [`../companies/`](../companies/); this one console serves them all.
