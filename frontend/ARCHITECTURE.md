# Console Architecture & Backend Requirements

This document captures the surfaces the operator console introduces, the data
each needs, and the **backend contract** that makes them real. It began as the
brief for the host-side APIs; the WS1–WS8 train has since landed them, so the
surface rows below now read **real/backed** rather than seam/client-only.

> **Status (WS1–WS8 delivered).** Every read is served by the GraphQL read
> plane (`POST /graphql`, rooted at a `Company` object); every write is served
> by the REST `ops/` router family (dual-scoped `/api/v1/companies/{id}/…` and
> `/api/v1/company/…`). Remaining caveats are called out inline: real inference
> **cost** in Usage/Finances is pending upstream openhuman#4940 (tokens flow,
> cost is zero until then); operator-added Team members are **roster-only** in
> v1 (no harness agent yet); Domain/SMTP/Connections write paths are
> feature-gated and `404 not_wired` when their network seam is absent.

## How the console is wired

The console is a company-agnostic SPA. It talks to an **OpenCompany host** over
a scoped REST surface:

- Multi-company: `/api/v1/companies/{id}/…`
- Single-company alias: `/api/v1/company/…`

The typed client (`src/api/client.ts`) resolves that scope, adds the operator
`Bearer` token, and is the only place HTTP happens.

Every surface is built to one pattern so the backend can land incrementally:

1. **Real** — the endpoint exists; the console uses it directly. **This is now
   the state of every surface below.**
2. **Seam** — the client calls a forward-looking endpoint; on `404`/error it
   **degrades gracefully** (read-only notice or built-in sample). Retained for
   the feature-gated write paths (Domain/SMTP/Connections) that `404 not_wired`
   when their seam is absent.
3. **Client-only** — no endpoint; state in `localStorage`. No surface is here
   anymore; the pattern remains as the graceful fallback.

Reads are served by GraphQL and writes by REST; the manifest and company
directory (below) are the source of truth the endpoints read from.

## Source of truth: the company directory

A company is a directory (`companies/<name>/`). Beyond `company.toml`, it ships
declarative, version-controlled data the endpoints should read:

| Path | Parsed today? | Feeds |
|---|---|---|
| `company.toml` `[company]`, `[[agent]]` | ✅ manifest | identity, Team roster |
| `company.toml` `[[group_chat]]` | ✅ manifest | Conversation threads (desks) |
| `company.toml` `[[connection]]` | ✅ manifest | Connections priorities (intent, no secrets) |
| `company.toml` `[workflows].enabled` | ✅ manifest | which Workflows are on |
| `workflows/<id>.toml` | ✅ parsed (WS1) | Workflow graph (nodes/edges) |
| `workspace/**` (Markdown) | ✅ parsed (WS1) | Workspace template (notes, `[[wiki]]`) — seeds `WorkspaceStore` |
| `skills/<id>/SKILL.md` | ✅ parsed (WS1) | Skills (frontmatter `name`/`description` + body) |

WS1 froze these on-disk formats and their parsers; every row above now feeds a
read endpoint. Shared, non-company skills live in the repo-level `skills/`
library (`skillRegistry`) and are installable into any company.

Secrets are never in the directory. OAuth tokens and SMTP credentials are held
by the manager/host secret store and injected per tenant — never handed to the
workload or committed.

---

## Existing API (real — already implemented)

| Method | Path | Purpose |
|---|---|---|
| GET | `/healthz` | liveness |
| GET | `/api/v1/companies` | list companies (platform) |
| GET | `…/{id}` | company status (`id`, `name`, `lifecycle`, `pending_approvals`) |
| POST | `…/{id}/chat` | send operator message → `{ responses: [{channel, text}] }` |
| GET | `…/{id}/approvals` | parked approvals |
| POST | `…/{id}/approvals/{approvalId}` | `{verdict, note?}` → follow-up reply |
| POST | `…/{id}/feedback` | scrub-then-preview feedback |
| POST | `…/{id}/{pause\|resume\|suspend\|archive}` | lifecycle control |

These back Overview, Conversation (send/reply), Approvals, Feedback, Settings
(connection + lifecycle). Everything below is now **delivered** too — the
sections document the surface, its read (GraphQL) and its writes (REST).

---

## Surfaces and the endpoints they need

Each surface lists its data, its now-real source, and the endpoints that back
it. Responses mirror the TypeScript models in `src/lib/*` and `src/api/types.ts`.

### Team — `src/views/TeamView.tsx`
- Shows the agent roster (name, role, description); operator can add/remove.
- **Source:** ✅ real — `Company.team` (GraphQL) merges the manifest `[[agent]]`
  roster with operator overlays; `POST/DELETE …/team` and
  `PUT …/team/{id}/inbox` (REST) write the overlay.
- **Note:** overlay teammates are **roster-only in v1** — they show in the
  roster and get an inbox, but no harness agent is built for them yet.

### Conversation threads — `src/views/Conversation.tsx`, `src/lib/threads.ts`
- WhatsApp-style two-pane; left list = the company's **desks** (group chats).
- **Source:** ✅ real — `Company.chats` / `Company.chat(id)` (GraphQL) list the
  desks from `[[group_chat]]` and page their history; send uses the `chat`
  endpoint. Desk-scoped routing of replies is single-responder in v1 (the full
  desk-member handler is WS3).

### Inbox — `src/views/InboxView.tsx`, `src/lib/inbox.ts`
- Per-agent email inbox; enabled via a Team toggle.
- **Source:** ✅ real — `Company.inboxes` (GraphQL, `InboxStore`-backed) lists
  enabled inboxes and pages messages; `POST …/inboxes/{key}/read` marks read and
  `PUT …/team/{id}/inbox` toggles an inbox. Inbound mail arrives via the
  HMAC-signed `POST …/inboxes/ingest` webhook. Real send/receive depends on
  Domain/SMTP (below).

### Tasks (Kanban) — `src/views/TasksView.tsx`, `src/lib/tasks-sample.ts`
- Columns Backlog/In progress/In review/Done; drag to move; priority + assignee.
- **Source:** ✅ real — `Company.tasks` (GraphQL, `TaskStore`-backed) reads the
  board; `POST …/tasks`, `PATCH`/`DELETE …/tasks/{id}` (REST) write it.

### Skills — `src/views/SkillsView.tsx`, `src/lib/skills.ts`
- Installed skills (enable/disable, uninstall) + an installable registry.
- **Source:** ✅ real — `Company.skills` (GraphQL) reads `skills/<id>/SKILL.md`
  overlaid with `SkillStateStore` enable/provenance; `skillRegistry` (GraphQL)
  is the shared repo `skills/` library. Writes: `POST …/skills/{slug}/install`
  `|uninstall`, `PUT …/skills/{slug}` (enable/disable), `POST …/skills`
  (custom).

### Workspace — `src/views/WorkspaceView.tsx`, `src/lib/workspace.ts`
- Obsidian-style: file tree, Markdown notes, `[[wiki links]]`, backlinks.
- **Source:** ✅ real — `Company.workspaceTree` / `workspaceFile(id)` (GraphQL,
  `WorkspaceStore`-backed, `[[wikilink]]` backlinks derived at read); writes:
  `POST …/workspace` (create/upload), `PUT …/workspace/file/{id}` (save),
  `PATCH`/`DELETE …/workspace/{id}`. New companies seed from
  `companies/<name>/workspace/**` on first use.

### Memory — `src/views/MemoryView.tsx`, `src/lib/memory.ts`
- Durable facts (fact/preference/person/project/reference); search + add/delete.
- **Source:** ✅ real — `Company.memory` (GraphQL, `FactStore`-backed, with
  query/kind filters); `POST …/memory` adds and `DELETE …/memory/{id}` deletes
  (deletion journals `MemoryFactDeleted` to the EventLog).

### Workflows — `src/views/WorkflowsView.tsx`, `src/lib/workflow-sample.ts`
- Read-only React Flow canvas of a company's routing graph.
- **Source:** ✅ real — `Company.workflows` (enabled ids from the manifest) and
  `Company.workflow(id)` (the graph read from `workflows/<id>.toml`), both
  GraphQL. Read-only, as designed.

### Usage — `src/views/UsageView.tsx`, `src/lib/usage-sample.ts`
- Token burn over time, tokens by desk, OAuth calls by provider; 7/30/90d.
- **Source:** ✅ real — `Company.usage(range: D7|D30|D90)` (GraphQL) projects the
  `UsageMeter` samples via the metering pipeline (series, byAgent, byProvider,
  totals). **Caveat:** token counts flow, but real inference **cost** is `0`
  until upstream openhuman#4940 exposes turn usage — see the status banner.

### Finances — `src/views/FinancesView.tsx`, `src/lib/finance-sample.ts`
- Wallet balance, revenue, spend vs budget, spend-by-category, transactions.
- **Source:** ✅ real — `Company.finances` (GraphQL) projects the ledger +
  `[budget]` + optional economy wallet balance (balance, budget vs spend,
  revenue, byCategory, transactions). **Caveat:** the inference-cost component
  of spend is `0` until openhuman#4940 (as with Usage).

### Connections — `src/views/ConnectionsView.tsx`, `src/lib/connections.ts`
- OAuth catalog with connect/disconnect and connected-account state.
- **Source:** ✅ real (feature `oauth`) — `Company.connections` (GraphQL) reads
  manifest intent (`[[connection]]`) + live OAuth status; `POST
  …/connections/{provider}/start` returns the authorize URL,
  `…/disconnect` drops tokens, and `GET /api/v1/oauth/callback` completes the
  flow. Without the `oauth` feature the write routes `404 not_wired` and the
  console shows the read-only catalog.

### Domain & Email (Settings) — `src/components/domain-settings.tsx`, `src/lib/domain.ts`
- Custom domain with generated DNS records (verification TXT, CNAME, DKIM, SPF)
  + verification status; SMTP credentials + test.
- **Source:** ✅ real — `Company.domain` and `Company.smtp` (GraphQL) read
  non-secret status; `PUT …/domain` + `POST …/domain/verify` (server-side DNS
  check) and `PUT …/smtp` (credentials to the **secret store**) + `POST
  …/smtp/test` write. The DNS/SMTP network seams are dependency-inverted and
  feature-gated — `verify`/`test` `404 not_wired` when absent.

---

## Data models

The console's models are the response contract. Keep host payloads aligned with:

- `src/api/types.ts` — `CompanyStatus`, `ApprovalSummary`, `ChatResponse`,
  `FeedbackResponse`, `TeamMemberDto`, `ConnectionState`, `ConnectionStart`.
- `src/lib/threads.ts` `Thread`/`ThreadContact`, `src/lib/inbox.ts` `EmailMessage`,
  `src/lib/tasks-sample.ts` `TaskCard`, `src/lib/skills.ts` `InstalledSkill`,
  `src/lib/workspace.ts` `FsNode`, `src/lib/memory.ts` `MemoryEntry`,
  `src/lib/usage-sample.ts` `UsageData`, `src/lib/finance-sample.ts` `FinanceData`,
  `src/lib/domain.ts` `DnsRecord`/`SmtpConfig`, `src/lib/workflow-sample.ts` `WorkflowNodeData`.

The reads now come from GraphQL (one `Company` query per view) rather than a
`localStorage` seed; the fallback seam remains only where a write path is
feature-gated off.

## Cross-cutting requirements

- **Auth:** all scoped routes require the operator/platform `Bearer` token; the
  console already sends it. `401` → the console prompts for `?token=`.
- **Secrets:** SMTP credentials and OAuth tokens go to the host secret store and
  are injected per tenant. Never returned to the console beyond non-secret
  status (e.g. connected account label, `smtp.host`).
- **Language rules:** product responses must avoid runtime jargon (agent graph,
  tier, dispatch, cycle) — the console re-labels via `src/lib/language.ts`, but
  server-authored strings should follow the same glossary.
- **Graceful 404:** until an endpoint exists it should 404; the console already
  treats that as "not wired yet" and shows the sample/notice — so partial
  rollout is safe.

## Implementation order (delivered)

The train landed in this order; kept as the record of how the surfaces went
real:

1. **Read-only manifest reflection** (cheap, high value): `…/team`, `…/chats`,
   `…/connections` (state), `…/skills`, `…/workflows[/{id}]` — all read the
   directory/manifest that already parses or ships as data.
2. **Workspace** file API (tree/file CRUD) seeded from `workspace/**`.
3. **Metering** feeds: `…/usage`, `…/finances` from the token + wallet pipelines.
4. **Connections OAuth** (`start`/`disconnect`) + **Domain/SMTP** provisioning +
   **Inbox** (depends on domain/SMTP) — the credential-bearing surfaces, secret
   store-backed.
5. **Tasks / Memory** persistence.
