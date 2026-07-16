# Console Architecture & Backend Requirements

This document captures the surfaces the operator console introduces, the data
each needs, and the **backend contract** required to make them real. It is the
brief for implementing the host-side APIs the console already anticipates.

## How the console is wired

The console is a company-agnostic SPA. It talks to an **OpenCompany host** over
a scoped REST surface:

- Multi-company: `/api/v1/companies/{id}/…`
- Single-company alias: `/api/v1/company/…`

The typed client (`src/api/client.ts`) resolves that scope, adds the operator
`Bearer` token, and is the only place HTTP happens.

Every surface is built to one pattern so the backend can land incrementally:

1. **Real** — the endpoint exists today; the console uses it directly.
2. **Seam** — the client already calls a forward-looking endpoint; on `404`/error
   it **degrades gracefully** (read-only notice or built-in sample).
3. **Client-only** — no endpoint yet; state lives in `localStorage` (per company)
   seeded with sample data. These need endpoints to become real.

The goal of the backend work is to move every surface from *seam/client-only* to
*real*, reading from the company's manifest and directory (below).

## Source of truth: the company directory

A company is a directory (`companies/<name>/`). Beyond `company.toml`, it ships
declarative, version-controlled data the endpoints should read:

| Path | Parsed today? | Feeds |
|---|---|---|
| `company.toml` `[company]`, `[[agent]]` | ✅ manifest | identity, Team roster |
| `company.toml` `[[group_chat]]` | ✅ manifest | Conversation threads (desks) |
| `company.toml` `[[connection]]` | ✅ manifest | Connections priorities (intent, no secrets) |
| `company.toml` `[workflows].enabled` | ✅ manifest | which Workflows are on |
| `workflows/<id>.toml` | ⛔ data-only | Workflow graph (nodes/edges) |
| `workspace/**` (Markdown) | ⛔ data-only | Workspace template (notes, `[[wiki]]`) |
| `skills/<id>/SKILL.md` | ⛔ data-only | Skills (frontmatter `name`/`description` + body) |

`⛔ data-only` means the files exist and are the intended source, but no Rust
parser/endpoint reads them yet. Shared, non-company skills live in the repo-level
`skills/` library and are installable into any company.

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
(connection + lifecycle). Everything below is what the console still needs.

---

## Surfaces and the endpoints they need

Each surface lists its data, current source, and the proposed endpoint(s).
Responses should mirror the TypeScript models in `src/lib/*` and `src/api/types.ts`.

### Team — `src/views/TeamView.tsx`
- Shows the agent roster (name, role, description); operator can add/remove.
- **Source:** seam `GET …/team` (falls back to a starter roster).
- **Needs:**
  - `GET …/team` → `TeamMemberDto[]` `{ id, name?, role, description? }` (from `[[agent]]`).
  - *(later)* `POST/DELETE …/team` for operator-defined agents.

### Conversation threads — `src/views/Conversation.tsx`, `src/lib/threads.ts`
- WhatsApp-style two-pane; left list = the company's **desks** (group chats).
- **Source:** client-only sample desks; send uses the real `chat` endpoint.
- **Needs:**
  - `GET …/chats` → `{ id, name, description?, members: agentId[] }[]` (from `[[group_chat]]`).
  - `chat` should accept a `chat`/thread id so replies route to the right desk
    and carry an agent `channel` (already surfaced per-message).

### Inbox — `src/views/InboxView.tsx`, `src/lib/inbox.ts`
- Per-agent email inbox; enabled via a Team toggle.
- **Source:** client-only (localStorage), one inbox seeded.
- **Needs:**
  - `GET …/inboxes` → enabled inboxes `{ key, name, address }[]`.
  - `GET …/inboxes/{key}/messages` → `EmailMessage[]`.
  - `POST …/inboxes/{key}/read` (mark read), `PUT …/team/{id}/inbox {enabled}`.
  - Depends on Domain/SMTP (below) for real send/receive.

### Tasks (Kanban) — `src/views/TasksView.tsx`, `src/lib/tasks-sample.ts`
- Columns Backlog/In progress/In review/Done; drag to move; priority + assignee.
- **Source:** client-only sample.
- **Needs:** `GET/POST/PATCH/DELETE …/tasks` with `{ id, title, note?, column, priority, assignee }`.

### Skills — `src/views/SkillsView.tsx`, `src/lib/skills.ts`
- Installed skills (enable/disable, uninstall) + an installable registry.
- **Source:** client-only; seeded from the company's `skills/` + a static registry.
- **Needs:**
  - `GET …/skills` → installed `{ id, name, description, category, source, enabled }` (read `skills/<id>/SKILL.md`).
  - `GET /api/v1/skills/registry` → shared library (repo `skills/`).
  - `POST …/skills/{id}/install|uninstall`, `PUT …/skills/{id} {enabled}`,
    `POST …/skills` (custom).

### Workspace — `src/views/WorkspaceView.tsx`, `src/lib/workspace.ts`
- Obsidian-style: file tree, Markdown notes, `[[wiki links]]`, backlinks.
- **Source:** client-only (localStorage), seeded from a built-in sample; the
  **company `workspace/`** is the intended template.
- **Needs:**
  - `GET …/workspace/tree` → `FsNode[]` `{ id, name, kind, parentId, updatedAt }`.
  - `GET …/workspace/file/{id}` → `{ content }`; `PUT` to save.
  - `POST` (create folder/file/upload), `PATCH` (rename/move), `DELETE`.
  - New instances seed from `companies/<name>/workspace/**`.

### Memory — `src/views/MemoryView.tsx`, `src/lib/memory.ts`
- Durable facts (fact/preference/person/project/reference); search + add/delete.
- **Source:** client-only sample.
- **Needs:** `GET/POST/DELETE …/memory` with `{ id, kind, title, body, source, updatedAt }`.

### Workflows — `src/views/WorkflowsView.tsx`, `src/lib/workflow-sample.ts`
- Read-only React Flow canvas of a company's routing graph.
- **Source:** client-only sample graph.
- **Needs:** `GET …/workflows` (enabled ids from manifest) and
  `GET …/workflows/{id}` → graph `{ nodes:[{id,kind,name,summary,…}], edges:[{from,to,label?}] }`
  read from `workflows/<id>.toml`.

### Usage — `src/views/UsageView.tsx`, `src/lib/usage-sample.ts`
- Token burn over time, tokens by desk, OAuth calls by provider; 7/30/90d.
- **Source:** client-only deterministic sample.
- **Needs:** `GET …/usage?range=30d` → `{ series:[{date,inputTokens,outputTokens}], byAgent:[{name,tokens}], byProvider:[{provider,calls}], totals:{…} }` from the metering pipeline.

### Finances — `src/views/FinancesView.tsx`, `src/lib/finance-sample.ts`
- Wallet balance, revenue, spend vs budget, spend-by-category, transactions.
- **Source:** client-only sample.
- **Needs:** `GET …/finances` → `{ balanceUsd, budgetUsd, spentUsd, revenueUsd, byCategory:[…], transactions:[…] }` from the wallet/ledger (`[budget]` sets the cap; tiny.place economy + inference cost feed it).

### Connections — `src/views/ConnectionsView.tsx`, `src/lib/connections.ts`
- OAuth catalog with connect/disconnect and connected-account state.
- **Source:** seam `GET …/connections` (degrades to read-only catalog).
- **Needs:**
  - `GET …/connections` → `ConnectionState[]` `{ provider, connected, account? }`.
  - `POST …/connections/{provider}/start` → `{ url }` (OAuth authorize).
  - `POST …/connections/{provider}/disconnect`.
  - Prioritized providers come from `[[connection]]`.

### Domain & Email (Settings) — `src/components/domain-settings.tsx`, `src/lib/domain.ts`
- Custom domain with generated DNS records (verification TXT, CNAME, DKIM, SPF)
  + verification status; SMTP credentials + test.
- **Source:** client-only draft (localStorage).
- **Needs:**
  - `GET …/domain` → `{ domain, verified, records: DnsRecord[] }`; `PUT` to set;
    `POST …/domain/verify` (server-side DNS check).
  - `PUT …/smtp` (credentials to the **secret store**, not the workload);
    `POST …/smtp/test` (send a test email).

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

When an endpoint lands, replace the `localStorage` seed with a fetch and drop the
sample; the seam pattern already isolates each fetch behind a client method.

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

## Suggested implementation order

1. **Read-only manifest reflection** (cheap, high value): `…/team`, `…/chats`,
   `…/connections` (state), `…/skills`, `…/workflows[/{id}]` — all read the
   directory/manifest that already parses or ships as data.
2. **Workspace** file API (tree/file CRUD) seeded from `workspace/**`.
3. **Metering** feeds: `…/usage`, `…/finances` from the token + wallet pipelines.
4. **Connections OAuth** (`start`/`disconnect`) + **Domain/SMTP** provisioning +
   **Inbox** (depends on domain/SMTP) — the credential-bearing surfaces, secret
   store-backed.
5. **Tasks / Memory** persistence.
