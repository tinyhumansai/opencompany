# 09 — Verification Plan

Cross-cutting test strategy for every workstream. Reuses the repo's existing
test infrastructure — do not invent parallel harnesses:

- Rust tests live in-module (`#[cfg(test)] mod test`) or a sibling `test.rs`.
- HTTP tests use `tower::ServiceExt::oneshot` against `crate::server::router(state)`
  with an `AppState` built over `FsCompanyStore` in a `tempfile` dir — the
  canonical pattern is the test mod in `src/server/operator.rs`.
- Store backends prove the port contract via the shared suite in
  `src/store/conformance.rs`, invoked identically from `fs.rs`, `sqlite.rs`,
  `mongodb.rs`.
- CI gates: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test` (plus feature matrices), frontend
  `typecheck`/`build`.

## 1. Unit tests

| Module | Location | Cases |
|---|---|---|
| Workflow TOML parser | `src/company/workflow_file.rs` | happy graph; edge referencing missing node → error; no trigger node → error; empty workflow; unknown keys tolerated; round-trip against `companies/agentic_marketing_agency/workflows/campaign_pipeline.toml` |
| SKILL.md parser | `src/company/skill_file.rs` | frontmatter name/description(+category); missing/malformed frontmatter → error; body preserved verbatim; parses both existing repo skills (`web-research`, `weekly-report`) |
| Workspace walker | `src/company/workspace_seed.rs` | nested tree; wikilink extraction incl. `[[target\|alias]]`; non-markdown files skipped; `../` path traversal rejected |
| Conformance extensions | `src/store/conformance.rs` + backend test mods | one `assert_*` suite per new port (tasks, workspace, facts, inbox, usage, skills-state): CRUD, company isolation, ordering; every backend runs the identical suite |
| Memory adapter | `src/harness/memory.rs` | store/recall round-trips through `MemoryStore`/`ContextStore`; `{company}/{agent}` namespacing; cross-company isolation; degraded vector recall never errors |
| Cost mapping | `src/harness/cost.rs` | `TurnCost`/`UsageInfo` → `LedgerEntry{kind:"inference.spend"}` amount + memo; zero-usage turn → no entry; `UsageSample` fields populated |
| DNS records | `src/company/dns.rs` | deterministic TXT/CNAME/DKIM/SPF generation for a domain; `verify()` with a mock `DnsResolver`: found / missing / partial |
| Metering aggregation | `src/metering/` (WS5) | 7/30/90d bucketing; byAgent/byProvider grouping; budget-cap math; empty range |
| Prosumer language | `src/server/ops/language.rs` | server-authored strings contain no banned runtime jargon (mirror the `frontend/src/lib/language.ts` list) |
| Content validation | `src/company/manifest.rs` (or `validate.rs`) | walk `companies/*`: every `company.toml`, `workflows/*.toml`, `skills/*/SKILL.md`, `workspace/**` parses — guards WS8 content forever |

Commands: `cargo test`, plus `cargo test --features sqlite` /
`--features mongodb` (Mongo cases skip without a live server, per existing
convention) and `cargo test --features openhuman` for harness modules.

## 2. Feature/API tests

Extract the copy-pasted state builders into a shared
`src/server/test_support.rs` (`#[cfg(test)]`-only module):

```rust
pub fn build_state(home: &Path, lifecycle: &str, config: AppConfig) -> AppState;
pub fn state_with_company_dir(dir: &Path) -> AppState; // loads a real companies/<name>/
```

**Four-case minimum for every new REST endpoint and GraphQL query**, in the
owning module's test mod:

1. **Happy path** — status + payload shape (field-level assertions against the
   TS contract).
2. **Auth failure** — `operator_token` configured, missing/wrong bearer → 401.
3. **Cross-tenant 403** — platform claims for tenant A addressing company B
   (the `platform_auth.rs` path).
4. **Not-wired/unknown** — unknown company or unshipped surface → 404 with the
   `{error, code}` envelope (GraphQL: error with `extensions.code`), preserving
   the console's graceful-degrade contract.

Per-surface additions:

- **Tasks** — create→list→patch (column drag, priority)→delete; invalid column
  → 400; **restart persistence**: rebuild `AppState` on the same home dir and
  re-read.
- **Memory** — add/search/delete; search miss → empty page.
- **Workspace** — tree seeded from the company `workspace/**`; file read/save;
  create/rename/move/delete; move-into-own-subtree → 400; `../` → 400; save
  persists across restart.
- **Skills** — installed list = company dir ∪ state store; registry list;
  install from registry → appears installed; enable/disable; uninstall custom
  ok, built-in → 409; unknown slug → 404.
- **Workflows (GraphQL)** — `workflow("campaign_pipeline")` nodes/edges match
  the TOML on disk; unknown id → null.
- **Chat threading** — `{message, chat}` routes to the desk and journals
  `AgentReply`; unknown desk → 400; `Chat.history` returns both directions.
- **Connections** — `start` returns an authorize URL (mock provider config);
  callback exchanges the signed state and stores the token; `disconnect`
  clears the secret; **no token material in any response body** (assert
  serialized JSON).
- **Domain/SMTP** — PUT domain → generated records; verify with mock resolver
  (both outcomes); PUT smtp writes to `SecretStore` and the read surface
  exposes host/username only; smtp test with mock `MailSender`.
- **Usage/Finances (GraphQL)** — seed `UsageSample`s + ledger entries through
  the ports; aggregates match.
- **Harness feature test** (`src/harness/`, `--features openhuman`) — full
  chat cycle with a mock `Provider` → response text, ledger delta, usage
  sample; tool requiring approval → parked → console resolve → turn resumes.
- **GraphQL SDL snapshot** — assert `schema.sdl()` against a checked-in
  expected string so every schema change is a reviewed diff.

## 3. Frontend

No test framework exists today (`typecheck` + `build` only). Add **Vitest**
(dev-dep only, no jsdom) with script `"test": "vitest run"`; colocated
`*.test.ts`:

- `src/lib/workspace.test.ts` — tree ops (create/rename/move/delete),
  wikilink parse, backlinks.
- `src/lib/threads.test.ts`, `src/lib/tasks.test.ts` (column moves),
  `src/lib/language.test.ts` (glossary relabeling), `src/lib/domain.test.ts`
  (record shaping).
- `src/api/client.test.ts` — contract tests with stubbed `globalThis.fetch`:
  scope resolution (`/company` vs `/companies/{id}`), bearer header, `ApiError`
  shaping on 404/network error, `gql()` error unwrapping, and one test per new
  client method asserting URL/method/body.

Component-level (jsdom/RTL) tests are **deferred** — the views are thin over
`lib/` + the client.

## 4. End-to-end

### Server-level (build now): `tests/e2e.rs`

First use of the crate's `tests/` integration dir. The test:

1. Builds the router in-process but serves over a **real ephemeral port**
   (`TcpListener::bind("127.0.0.1:0")` + `axum::serve`) — exercises real HTTP,
   unlike oneshot.
2. Loads `companies/agentic_marketing_agency` with sqlite storage in a
   tempdir and a mock `Provider` harness.
3. Drives with `reqwest` (dev-dependency):
   - GraphQL `team`/`chats`/`skills`/`workflow("campaign_pipeline")` match the
     manifest/TOML/SKILL.md on disk;
   - REST writes (task, memory fact, workspace edit) → **stop, rebuild state
     on the same data dir, restart, re-read** (restart-persistence);
   - chat round-trip → mock-provider reply + an `inference.spend` ledger entry
     visible via the `finances` query;
   - approval flow: gated tool → appears in approvals → approve → follow-up;
   - skill install from registry → visible in `skills`.

Run: `cargo test --test e2e --features "sqlite openhuman"`, wrapped in
`scripts/e2e.sh`, as a dedicated (serial) CI job so unit CI stays fast.

### Browser-level (Playwright): deferred until WS7 lands

Until surfaces are real, Playwright would test localStorage samples. Once WS7
is substantially merged, add `frontend/e2e/` covering exactly:

1. token login → Overview shows the real company name;
2. Conversation: send → reply renders in the right desk;
3. Approvals: pending → approve → confirmation;
4. Workspace: create + edit note → survives reload;
5. Tasks: drag between columns → survives reload.

## 5. Per-workstream exit criteria

| WS | Exit criteria |
|---|---|
| 1 | Parsers merged; content-validation walk green over all `companies/*` + `skills/*`; `cargo test` green |
| 2 | SDL snapshot updated; every query has the 4-case suite; GraphiQL spot-checked |
| 3 | Conformance suites green on fs+sqlite(+mongo); REST 4-case suites; restart-persistence in e2e |
| 4 | Harness feature test green under `--features openhuman`; default (echo) build unaffected; approval-bridge + cost unit tests |
| 5 | Metering unit tests; usage/finances queries match seeded data; e2e chat produces a ledger entry |
| 6 | DNS/SMTP mocked tests; no-secret-leak assertions; default build stays offline |
| 7 | Per surface: sample/localStorage code deleted, fallback retained; typecheck+build+vitest green; manual pass against a live host |
| 8 | Content-validation test green; every SKILL.md renders in the console Skills view |
