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

The typed client (`src/api/client.ts`) resolves that scope, attaches whatever
credential its connection holds, and is the only place HTTP happens.

The console holds **N hosts at once** (`src/connections/`), one client per
connection. Three deployment shapes fall out of that:

- **Same-origin** — served by the host it operates. The session is an `HttpOnly`
  cookie that nothing in the page can read. The ordinary case, unchanged.
- **Hub** — one deployment at its own origin operating hosts on other origins,
  built with `VITE_OC_HUB=1`. No cookie crosses an origin, so it carries a
  session token itself in `x-opencompany-session`, and its event stream runs
  over `fetch` because `EventSource` cannot set a header. See
  [hub-console.md](../docs/spec/runtime/hub-console.md).
- **Desktop** — routes through its own Rust core, where neither CORS nor the
  cookie rules apply. See [desktop.md](../docs/spec/runtime/desktop.md).

Which applies is **derived, not configured**: `needsCarriedSession` compares the
host's origin with the document's, so a console cannot be told to hold a token
where a cookie would have worked.

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
| `company.toml` `[[group_chat]]` | ✅ manifest | Company org chart (desks), Chat channels |
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
- **Source:** ✅ real — `Company.team` (GraphQL) and `GET …/team` (REST, what the
  console calls) merge the manifest `[[agent]]` roster with operator overlays and
  tag each teammate's `inboxEnabled`; `POST/DELETE …/team` and
  `PUT …/team/{id}/inbox` (REST) write the overlay.
- **Note:** overlay teammates are **roster-only in v1** — they show in the
  roster and get an inbox, but no harness agent is built for them yet.

### Conversation threads — `src/views/Conversation.tsx`, `src/lib/threads.ts`
- WhatsApp-style two-pane; left list = the company's **desks** (group chats).
- **Source:** ✅ real — `Company.chats` / `Company.chat(id)` (GraphQL) list the
  desks from `[[group_chat]]` and page their history; send uses the `chat`
  endpoint. Desk-scoped routing of replies is single-responder in v1 (the full
  desk-member handler is WS3).
- **Console:** desk management lives on the **Company** org chart
  (`src/views/company/`) as of issue #311. The flat Desks screen that #302
  unmounted is gone rather than restored — see that section below.

### Company (org chart) — `src/views/company/`, `src/lib/org.ts`
- A three-level tree of the company's declared structure: **company → desk →
  seat**. Creating a desk, deleting an operator-created one, adding and removing
  members, and changing a desk's lead all happen here.
- **Source:** ✅ real — `GET …/desks` (with `overlayCreated` / `overlayMembers`
  carrying provenance), `GET …/team` for who fills a seat, `GET …/users` for the
  humans, `GET /api/v1/companies/{id}` for the company's name. Writes are the
  five desk routes: `POST …/desks`, `DELETE …/desks/{id}`,
  `POST …/desks/{id}/members`, `DELETE …/desks/{id}/members/{agent}` and
  `PUT …/desks/{id}/order`.
- **The three-level cap is structural, not enforced.** No desk can name a parent
  desk — the host's `GroupChat` and `OverlayDesk` have no such field — so a
  fourth level is unrepresentable rather than rejected. `lib/org.ts` derives the
  tree; nothing validates its depth, for the same reason nothing validates that
  a string is a string. This is what issue #311 means by "a new reader over
  existing data, not a data change".
- **The lead is a position, never a flag.** `DeskDto.members[0]` is the host's
  routing target, so changing the lead is a `PUT …/order` that moves somebody to
  the front. There is no set-lead call and the console must not invent one.
- **Provenance decides which controls exist.** The host refuses to delete a
  blueprint desk or remove a blueprint member at runtime, so the chart offers
  neither — a control that always fails is worse than no control.
- **Not on a desk / People** are listed *beside* the tree, not inside it.
  Neither has a position the company declares, and inventing one would be the
  same mistake the Overview graph documents about its own derived departments.

### Inbox — `src/views/InboxView.tsx`, `src/api/inbox.ts`
- Per-agent email inbox; enabled via a Team toggle.
- **Console:** not listed in the sidebar as of issue #302 — hidden, not retired.
  Every endpoint below is unchanged and still serving; only the operator entry
  point is gone. The Team toggle that enables an inbox stays where it was.
- **Source:** ✅ real — `client.listInboxes()` (`GET …/inboxes`) lists every inbox
  with its unread count and `client.inboxMessages()`
  (`GET …/inboxes/{key}/messages`) reads one teammate's mail, both
  `InboxStore`-backed REST twins of the `Company.inboxes` GraphQL resolver (the
  console ships no GraphQL client). `client.markInboxRead()`
  (`POST …/inboxes/{key}/read`) marks read and `setInboxEnabled`
  (`PUT …/team/{id}/inbox`) toggles an inbox. Inbound mail arrives via the
  HMAC-signed `POST …/inboxes/ingest` webhook and the IMAP poller, which file
  into the same store. Real send/receive depends on Domain/SMTP (below).
- **Note:** inboxes are keyed by **agent id**, the same key the ingest webhook
  files mail under. Nothing is seeded or cached client-side — an inbox with no
  mail renders empty (issue #173 replaced a localStorage fixture that showed the
  same four invented emails for every teammate).

### Tasks (Kanban) — `src/views/LedgersView.tsx`, `src/views/TaskCard.tsx`
- **The board has no page of its own.** It is the `tasks` ledger, rendered as
  that ledger's columns under `#/ledgers/tasks` (issue #1140 retired the
  standalone Tasks page, which showed the same records through the same
  component). `src/views/LedgerBoard.tsx` is the board; `TaskCard` is the card
  it renders when the row behind it is a `Task`.
- Columns To-do/Planning/In progress/Paused/In review/Done, declared by the host
  and read off the ledger; drag to move. New work enters through one prompt box
  landing in To-do (issue #301), which keeps its own `POST …/tasks` dialog
  because `record_entry` is refused for this ledger; priority + assignee are
  edited on the card afterwards.
- `#/tasks/<id>` survives as the card detail (`TaskDetailView`, routed by
  `src/views/TaskDetailRoute.tsx`) — a timeline, a plan brief, a discussion, its
  attempts and the steer controls, none of which fits on a column.
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

### Memory (Brain) — `src/views/MemoryView.tsx`, `src/lib/memory.ts`
- Durable facts (fact/preference/person/project/reference); search + add/delete.
- **Console:** not listed in the sidebar as of issue #302 — hidden, not retired.
  The endpoints below are unchanged and agents keep reading and writing memory;
  only the operator-facing Brain tab is gone.
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
- **Console:** not listed in the sidebar as of issue #302 — hidden, not retired.
  The `Company.finances` projection below is unchanged; only the operator entry
  point is gone.
- **Source:** ✅ real — `Company.finances` (GraphQL) projects the ledger +
  `[budget]` + optional economy wallet balance (balance, budget vs spend,
  revenue, byCategory, transactions). **Caveat:** the inference-cost component
  of spend is `0` until openhuman#4940 (as with Usage).

### Connections — `src/views/ConnectionsView.tsx`, `src/lib/provider-grid.ts`
- **One** provider grid with connect/disconnect and connected-account state,
  plus the credential sections above it (MCP, inference, company key, Composio
  token, channels).
- **Source:** ✅ real (feature `oauth`) — `Company.connections` (GraphQL) reads
  manifest intent (`[[connection]]`) + legacy native OAuth status. `POST
  …/connections/{provider}/start` and `GET /api/v1/oauth/callback` are dated
  410 retirement responses (#838); `…/disconnect` remains so a tenant can
  release a token written before #828. The supported actionable connection path
  is Composio.
- **One list, one answer (issue #582).** The page used to render two provider
  lists — `ComposioSection`'s grid off `GET …/composio/connections`, and a
  categorised grid of eleven hardcoded tiles off `GET …/connections` — which
  applied different rules to the same Composio state and so disagreed on screen.
  Now: `GET …/connections` is the sole status source, the backend's Composio
  catalog is the sole list of what can be connected, `src/lib/connections.ts` is
  native-route metadata rather than a list, and `src/lib/provider-grid.ts` merges
  the three into the rows `ProvidersSection` renders. `ComposioSection` keeps
  only the credential layer.

### Domain & Email (Settings) — `src/components/domain-settings.tsx`, `src/api/domain.ts`, `src/api/smtp.ts`
- Custom domain with the DNS records the host wants created (verification TXT,
  CNAME, DKIM, SPF) + per-record verification results; SMTP credentials + test.
- **Source:** ✅ real — `GET …/domain` and `GET …/smtp` read non-secret status;
  `PUT …/domain` + `POST …/domain/verify` (server-side DNS check) and `PUT
  …/smtp` (credentials to the **secret store**) + `POST …/smtp/test` write. The
  DNS/SMTP network seams are dependency-inverted and feature-gated —
  `verify`/`test` `404 not_wired` when absent. The same non-secret status is
  also on `Company.domain`/`Company.smtp` in GraphQL, minus `checks` and the
  SMTP `security`/from fields.
- **Records are the host's answer; the console derives nothing.** It used to:
  `src/lib/domain.ts::dnsRecords` hashed the domain into a verification token
  client-side and pasted it into a hardcoded target, so every row an operator
  copied into their registrar was a guess. That generator is gone, and so is the
  `oc-mail` `localStorage` draft the card kept beside it (issue #1460) — a
  remembered copy is only a second answer that can disagree with the
  authoritative one, and the SMTP password was in it. `src/lib/domain.ts` is now
  a domain pre-flight plus a one-shot purge of what older builds already wrote.

---

## Data models

The console's models are the response contract. Keep host payloads aligned with:

- `src/api/types.ts` — `CompanyStatus`, `ApprovalSummary`, `ChatResponse`,
  `FeedbackResponse`, `TeamMemberDto`, `InboxDto`, `InboxMessageDto`,
  `ConnectionState`.
- `src/lib/threads.ts` `Thread`/`ThreadContact`,
  `src/lib/tasks-sample.ts` `TaskCard`, `src/lib/skills.ts` `InstalledSkill`,
  `src/lib/workspace.ts` `FsNode`, `src/lib/memory.ts` `MemoryEntry`,
  `src/lib/usage-sample.ts` `UsageData`, `src/lib/finance-sample.ts` `FinanceData`,
  `src/api/domain.ts` `DomainStatus`/`DnsRecord`/`RecordCheck`,
  `src/api/smtp.ts` `SmtpStatus`/`SmtpConfig`,
  `src/lib/workflow-sample.ts` `WorkflowNodeData`.

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
- **Toast lifetime:** the one `<Toaster>` is mounted at the app root, outside the
  routed tree, so a toast outlives the view that raised it and nothing about
  changing view clears one. `sonner`'s auto-dismiss is a timer it *pauses* — on
  hover, on a pointer interaction, on a hidden tab — and two of those latch with
  no way back (issue #933). `src/lib/toast-lifetime.ts` is the ceiling that makes
  a stuck toast impossible: it accumulates each toast's *visible* time and
  dismisses one whose duration plus a grace period is spent. Hovering to read, a
  backgrounded tab, and an explicit `duration: Infinity` are all still honoured,
  so callers keep raising plain `toast.*` calls and need not think about it.
- **How a write answers:** a toast, everywhere. The inline `role="alert"` banner
  is reserved for a surface that could not *load* — it is a state of the page and
  it sits with the Retry that clears it, whereas the answer to an action the
  operator just took has to survive the dialog closing over it. Issue #1099 is
  the case that fixed the split: adding a teammate said nothing on success from
  any of its three surfaces (Team, Company, the chat empty state) and reported
  failure two different ways. `src/lib/member-feedback.ts` now owns that one
  answer — the views decide *what happened*, it decides what that is called and
  how loudly. Half-landed writes stay distinguishable from clean ones
  (`toast.warning`, as `PeopleView`'s invite already did), because a teammate
  whose inbox never came up is not a clean add.

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
