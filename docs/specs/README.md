# Implementation Specs — Making Every Console Surface Real

This folder is the implementation plan for the work that remains after
[PR #4](https://github.com/tinyhumansai/opencompany/pull/4) shipped the shadcn
operator console. The console was built to a **seam pattern**: each surface
calls a forward-looking host endpoint and, until it exists, degrades to a
localStorage sample. [`frontend/ARCHITECTURE.md`](../../frontend/ARCHITECTURE.md)
is the product brief; these specs are the engineering plan that moves every
surface from *seam/client-only* to *real* — and swaps the agent harness to
openhuman embedded as a library.

Each workstream spec follows one template: **Scope → Design → Subtasks
(commit-sized, subagent-assignable) → Dependencies → Tests & exit criteria**.

## Decisions (locked)

1. **GraphQL is the read plane.** async-graphql 7 is already wired query-only
   at `/graphql` (`src/server/graphql.rs`). All new reads land there, rooted at
   a `Company` aggregation object so a view fetches in one round trip.
2. **REST is the write plane.** New writes live in a `src/server/ops/` router
   family following the existing dual-scope convention
   (`/api/v1/companies/{id}/…` and `/api/v1/company/…`).
3. **openhuman as a library.** The tenant harness embeds
   `vendor/openhuman` (`openhuman_core`) directly via `AgentBuilder` — the RPC
   transport, launcher, and sidecar process go away. Memory, inference
   provider, tools, skills, and approval policy are all injected through the
   builder's seams.
4. **Persistence through ports.** New durable state gets port traits in
   `src/ports/`, backends in `src/store/{fs,sqlite,mongodb}.rs`, registration
   in `StorageHandles`, and coverage in the backend-agnostic conformance suite.
5. **Graceful rollout.** Anything unshipped 404s (REST) or is absent from the
   schema (GraphQL); the console's bare-catch seams treat both as "not wired
   yet" and fall back — so every workstream can merge independently.
6. **Secrets never leave the host.** SMTP credentials and OAuth tokens go to
   `SecretStore` only; responses expose non-secret status.
7. **Prosumer language everywhere.** Server-authored strings follow the same
   glossary the console enforces in `frontend/src/lib/language.ts` (desk, not
   group-chat channel; teammate, not agent tier; approval, not parked effect).

## The specs

| Doc | Workstream | Depends on |
|---|---|---|
| [01-parsers.md](01-parsers.md) | WS1 — manifest/data parsers (workflow TOML, SKILL.md, workspace) | — |
| [02-graphql-reads.md](02-graphql-reads.md) | WS2 — GraphQL read plane | WS1 (a/b), WS3 (c), WS5 (d) |
| [03-ports-and-rest-writes.md](03-ports-and-rest-writes.md) | WS3 — new ports/stores + REST write plane | WS1 |
| [04-openhuman-harness.md](04-openhuman-harness.md) | WS4 — openhuman-as-library embedding | — |
| [05-usage-finances.md](05-usage-finances.md) | WS5 — metering pipeline + Usage/Finances | WS4 |
| [06-connections-domain-email.md](06-connections-domain-email.md) | WS6 — OAuth, domain/DNS, SMTP, inbox transport | — |
| [07-frontend-wiring.md](07-frontend-wiring.md) | WS7 — console seam swaps | trails WS2/3/5/6 |
| [08-company-content.md](08-company-content.md) | WS8 — skills library + company starter content | WS1 |
| [09-verification.md](09-verification.md) | cross-cutting test strategy + exit criteria | all |

## Dependency graph & critical path

```
WS1 (parsers) ──► WS2a/b (manifest+workspace reads) ──► WS7 (per surface)
WS3 (ports/stores/REST) ───────► WS2c ─────────────────┘
WS4 (harness) ──► WS5 (metering) ──► WS2d (usage/finances) ──► WS7 usage/finances
WS6 (OAuth/domain/SMTP) ──► WS3-inbox real delivery ──► WS7 inbox
WS8 (content) — anytime after WS1 freezes the file formats
```

**Critical path: WS4 → WS5 → WS2d** — the only serial three-chain that touches
the runtime core. Start WS4 on day one, in parallel with WS1.

## Subagent execution model

Implementation is executed by parallel agents, one per workstream (WS3 and WS8
fan out further — one agent per surface / per company batch). Coordination
rules:

- **Merge-conflict hotspots** are the registration files: `src/ports/mod.rs`,
  `src/store/mod.rs`/`select.rs`, `src/server/routes.rs`, `src/server/mod.rs`,
  `frontend/src/api/client.ts`. Registration commits land serially; everything
  else is parallel.
- **Format freezes**: WS1's parser types freeze the on-disk formats before WS8
  authors content at scale; WS2's SDL snapshot freezes the read contract
  before WS7 wires views.
- Every commit is small, conventional-prefix, and green on the full local gate
  (below) before it lands.

## PR slicing

Small PRs against `tinyhumansai/opencompany` `main`, in this order (parallel
tracks interleave):

1. `feat(company): workflow/skill/workspace parsers` (WS1)
2. `feat(graphql): manifest-derived reads` (WS2a/b)
3. Per surface: `feat(<surface>): port + stores + REST routes` (WS3, ~5 PRs)
4. `feat(graphql): store-backed reads` (WS2c)
5. `feat(harness): embedded openhuman` (WS4, 3–4 PRs: scaffold+provider,
   memory adapter, approval bridge, cost hook)
6. `feat(metering): usage pipeline + usage/finances reads` (WS5 + WS2d)
7. `feat(server): connections oauth` / `feat(server): domain + smtp` (WS6)
8. Per view: `feat(console): wire <surface>` (WS7, ~10 tiny PRs)
9. `feat(companies): starter content for <batch>` (WS8, 3–4 PRs)
10. `docs: …` trailing each workstream (flip rows in
    `frontend/ARCHITECTURE.md`, update `docs/spec/runtime/*`, module docs)

Local gate for every PR:

```sh
cargo fmt --all -- --check \
  && cargo clippy --all-targets -- -D warnings \
  && cargo test
(cd frontend && npm run typecheck && npm run build && npm test)
```

## Open questions (recommendations inline)

1. **Team writes vs the manifest.** `POST …/team` adds agents `company.toml`
   doesn't know. *Recommended:* an operator-overlay persisted through
   `CompanyStore` (extra field on `CompanyRecord`), merged into the roster at
   read/build time — rewriting the version-controlled manifest is not an
   option. Open: whether overlay agents get harness `Agent`s immediately or
   are roster-only at first (roster-only recommended for v1).
2. **Inbound email transport.** *Recommended:* v1 is an HMAC-signed ingest
   webhook (`POST …/inboxes/ingest`) a mail-forwarding provider or the manager
   pushes into; IMAP polling and manager-owned MX are alternatives that add
   deps and ops burden.
3. **Chat-history backfill.** Pre-threading `OperatorMessage` events carry no
   desk id. *Recommended:* attribute them to a synthetic "General" desk.
4. **Usage retention.** `UsageMeter` grows unbounded. *Recommended:* evict
   samples older than 90 days (the console's max range).
5. **Vector recall on the fs backend.** *Recommended:* degrade
   `recall_relevant_by_vector` to FTS/substring recall rather than requiring
   sqlite/tinycortex when the harness feature is on.
