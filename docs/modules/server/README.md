# Server Module

The server module owns the Axum HTTP surface. The base routes are:

- `GET /healthz`
- `GET /spec`
- `GET /tiny`

Operator chat and approvals live under `/api/v1/...` (see `server::operator`),
and feedback under `server::feedback`. Add future API routes as focused handler
groups rather than wiring behavior directly in the binary entrypoint.

## Read plane — `server::graphql`

The console's reads are one async-graphql query surface (`POST /graphql`, plus
a `GET /graphql` GraphiQL explorer). The schema is **built once at startup**
(`build_schema`) and stored on `AppState`; each request injects its resolved
`GqlAuth` principal via request data. It is query-only — REST owns writes.

The module is split one file per surface: `mod.rs` (the `Company`-rooted
`QueryRoot` — `companies`, `company(id)`, `skillRegistry`), `auth.rs`
(`GqlAuth`, claim resolution + `visible_companies`), `company.rs` (the
aggregation object every view fetches through), `pagination.rs`, and one
resolver file per view (`tasks`, `workspace`, `memory_facts`, `skills`,
`inbox`, `workflows`, `usage`, `finances`, `connections`). `schema.graphql` is
the checked-in SDL snapshot (the read contract); `graphql::sdl()` regenerates
it and a snapshot test guards drift.

## Write plane — `server::ops`

Console writes are the `server::ops` router family. Each route is registered
under **both** scope forms — `…/companies/{id}/…` and the `…/company/…`
prosumer alias — by the `scoped` helper; the `ScopedCompany` extractor resolves
the target runtime and enforces authorization per form (platform-or-operator +
address check for `{id}`, operator + `sole()` for the alias).

### Addressing is not authority

`ScopedCompany` answers *may this principal talk to this company*, and stops
there. A write that decides something **for** the company — what it reaches the
world as — takes `AdminScopedCompany` instead. The routes, the reasoning, and
why this is not a read/write split: [authority.md](authority.md).

| Surface (`ops::*`) | Routes |
|---|---|
| `tasks` | `POST …/tasks`, `GET …/tasks`, `GET …/tasks/{id}` (the Task Detail read, #185), `PATCH`/`DELETE …/tasks/{id}`, `GET …/tasks/inflight`, `POST …/tasks/{id}/steer` (#111), `POST …/tasks/{id}/discussion` (#335), `DELETE …/tasks/{id}/discussion/{seq}` (#358) |
| `task_export` | `GET …/tasks/{id}/export` (the task's record as a document, #352) |
| `memory` | `POST …/memory`, `DELETE …/memory/{id}` (journals `MemoryFactDeleted`) |
| `workspace` | `GET …/workspace`, `GET …/workspace/file/{id}`, `GET …/workspace/search?q=…` (#607), `POST …/workspace`, `PUT …/workspace/file/{id}`, `PATCH`/`DELETE …/workspace/{id}`, `POST …/workspace/sweep-empty-agent-folders?dry_run=` (#700, removes only folders with no children counted structurally), `POST …/workspace/merge-duplicate-folders?dry_run=` (#759, folds duplicate sibling folders into the oldest twin and reports the file collisions it refuses to decide) (the `GET`s are REST twins of the GraphQL reads — the console has no GraphQL client, #177). Node bodies carry `createdBy`/`updatedBy` (#326) |
| `skills` | `POST …/skills`, `GET …/skills/registry`, `POST …/skills/{slug}/install\|uninstall`, `PUT …/skills/{slug}` |
| `team` | `POST …/team`, `DELETE …/team/{id}`, `PUT …/team/{id}/inbox` (overlay; roster-only in v1) |
| `mail` | `POST …/inboxes/{key}/read` |
| `inbox` | `POST …/inboxes/ingest` (HMAC-signed inbound email) |
| `domain` | `GET …/domain` (the stored records and last verify result, or `null`), `PUT …/domain`, `POST …/domain/verify` (the `GET` is a REST twin of the GraphQL `Company.domain` read, #1460) |
| `smtp` | `GET …/smtp` (non-secret status; never the password), `PUT …/smtp` (the password is a patch — omit it to keep the stored one), `POST …/smtp/test` (the `GET` is a REST twin of the GraphQL `Company.smtp` read, #1460) |
| `connections` (feature `oauth`) | `POST …/connections/{provider}/start` → dated `410` retirement bridge, `POST …/connections/{provider}/disconnect`, `GET /api/v1/oauth/callback` → dated `410` browser landing page (#838; removal #1023) |
| `workflows` | `POST …/workflows`, `GET …/workflows`, `GET …/workflows/runs`, `POST …/workflows/cron/preview`, `GET …/workflows/{wid}`, `PUT …/workflows/{wid}`, `DELETE …/workflows/{wid}`, `POST …/workflows/{wid}/run`, `POST …/workflows/runs/{runId}/cancel` |

### Chat mentions

`GET …/chat/mentionables` returns the names an operator may address in that
company: agents, signed-in people, desks, and the `@everyone` broadcast target.
The response is company-scoped and omits the current viewer where appropriate;
clients must treat it as a directory, not as authorization to invent targets.

A chat request may include `mentions`, each with a target (`agent`, `user`,
`desk`, or `everyone`), the literal `text` span including `@`, and its **UTF-8
byte offset** in the submitted message. JavaScript clients may use UTF-16 indices
while editing, but must convert them before sending. The host revalidates every
supplied span against the live roster and exact message text. Stale, overlapping,
duplicate, ambiguous, or over-cap entries remain renderable as `quiet` mentions
but do not notify anyone. If the field is absent, the host extracts unambiguous
mentions from the message while ignoring Markdown code regions.

Mention routing is deliberately one-turn and has no fan-out: the first valid
non-quiet agent mention is the responder, otherwise the desk lead (or normal
channel fallback) answers. Responder selection is a property of the
agent-harness brain, which runs the roster's turns; hosted and echo cognition
reply through their own service, which does not select a roster responder, and
the resolved mentions still render as chips on the returned message. Additional
agent mentions, people, desks, and `@everyone` are context for that same turn;
they do not start additional agent runs. A desk mention contributes that desk's
context, while `@everyone` expands to the channel's visible audience for
notification purposes. Clients should
render the returned mention DTOs, including `label`, `mine`, and optional
`quiet`, rather than re-resolving display text locally.

### Exporting a task's record (issue #352)

`GET …/tasks/{taskId}/export` answers **one self-contained HTML file** carrying
what the Task Detail screen shows: the header and the worked/waiting split, the
whole ordered timeline with its details expanded, the text of every artifact
revision and the human-edit diff, and the neighbouring cards. `Content-Type:
text/html`, `Content-Disposition: attachment`, so `curl -OJ` lands a named file
and the console's Export button downloads one.

**Why HTML rather than Markdown or PDF.** The bar in epic #184 is "a
non-technical person can read it unaided", which already rules out the JSON the
screen fetches. HTML opens by double-click on any machine with no reader
installed and nothing to explain; it keeps the **proportional waiting bands**
(#305) that are the entire point of the worked/waiting work — a four-hour wait
must not look like a four-second one, which is exactly what a text format
flattens; and the reader's own browser prints it to the PDF a client asks for by
name. It also costs **no new dependency**: the document is `format!` plus
inlined CSS, no templating engine, no PDF crate, no headless browser. Markdown
was rejected because it loses the proportions and, opened in a text editor by
the non-technical reader this exists for, shows raw `##` and `|`. PDF was
rejected because generating one means a new rendering stack for an artifact the
browser already produces from this file.

**Why the server renders it.** One implementation serves the console button and
an automated caller alike, so an audit or scheduled export needs no second
renderer. A client-side document would live only inside the React view — the
same place the record is stuck today.

**Redaction is structural, not procedural.** The handler renders
`tasks::assemble_detail` and `artifacts::artifacts_for_task` — the *same values*
`GET …/tasks/{id}` and the Artifacts tab return. It never reads the event log
itself, so there is no second path whose scrubbing could drift from the
console's; `detail` text is scrubbed at source before either caller sees it.
Everything interpolated is HTML-escaped, so a card titled `<script>…</script>`
renders as text in a file that will be opened in a browser and forwarded on.

**Exporting is a pure read** — no journal entry, no column change, no state on
the task. A test compares the board rows and the journal length across the call,
because an audit export that modifies what it audits is worse than none.

**One figure, computed once.** The worked/waiting split (#305) is computed
host-side in `TaskDurations` and carried on `TaskDetail`, so the console and the
exported record read the same numbers instead of deriving them separately. It
used to be a hand-maintained mirror between `task_export.rs` and
`frontend/src/views/TaskDetailView.tsx`: they agreed, but nothing failed if they
stopped, and the failure mode is an exported record disagreeing with the screen
about how long a person was waited on. `TaskDurations` carries the totals plus
`workedLive` / `waitingLive` and `asOfMillis`; a caller wanting a ticking figure
adds `now - asOfMillis` to the live half, which is exact because every closed
span already ended before `asOfMillis`. The waiting *band height* stays mirrored
— it is presentation, and a drifted pixel curve misleads nobody about a fact.

**No sign-offs section, until #333 lands.** #352 deferred it explicitly: until an
approval carries a task id, the only sign-offs attributable to a card are the
resolutions that fell inside its run window, which is a correlation rather than a
link. An imprecise attribution is least acceptable in the one artifact that goes
to a client or an auditor, so the document omits the section rather than printing
it with a caveat. When #333 (PR #349) merges, `TaskDetail` gains an `approvals[]`
keyed by a real id and the section lands reading that — including the pending
sign-offs that have no resolution event and so cannot appear at all today.

**Bounded output.** The document is one `String` in one response, and it prints
every revision of every artifact, so a long editing history scales it. Each
revision body is capped (`MAX_BODY_CHARS`) and each human-edit diff is capped
(`MAX_DIFF_LINES`); both cuts are announced in the document, because a reader
must never be left believing they hold the whole text when they do not.

### The per-task discussion (issue #335)

The Task Detail screen's Discussion tab shipped as an honest stub with no
backend. This is what was decided when it got one, written down here rather
than left to be inferred from the schema.

**A task discussion is its own thread, not a filtered view of company chat.**
The alternative — reuse the chat store and filter it to a card — is cheaper and
was rejected: chat is addressed to a *desk* and is a conversation with the
company, while a discussion is addressed to a *card* and is a note about one
piece of work. Filtering one into the other would mean every card's thread is a
slice of a stream whose messages were written for a different audience, and a
card would stop being the record of its own work the moment somebody replied in
the desk instead.

**One store, two projections.** The thread is *not* a second message store: a
post is journaled as `CompanyEvent::TaskDiscussionPosted`, in the same
company `EventLog` the timeline is folded from, and `GET …/tasks/{task_id}`
serves both out of **one** traversal (`fold_task_journal`). That is what keeps
the two notions of "something happened on this task" from drifting — they are
the same log read two ways.

They stay two *arrays* rather than one merged list because they answer different
questions: the timeline is the record of what the **company** did on the card
(dispatch, replies, failed tool calls, approvals, completion), and the
discussion is what **people** said about it. Merged, an operator's aside would
sit between a dispatch and its completion and read as part of the run.

**What the shared log costs, stated plainly.** The read rides a cost that was
already there — the detail folded the whole journal per request before this
existed. The *write* does not: it points a new, human-paced, unbounded writer at
the log every other projection folds over, with no retention or compaction for
it anywhere in the tree. That is not hypothetical — it is the conclusion the
runtime already reached and acted on: `src/ports/runs.rs` (#342) says outright
that "folding the whole journal per read is what makes the existing workflow-run
list expensive", #242 answered it by giving runs a store of their own instead of
more events, and #357 now writes a `RunRecord` per attempt on `main`. This goes
the other way knowingly: one small event per post with no per-step fan-out, and
a paged read so thread length no longer sets response size. But it is a shared
log with an extra writer on it, and the moment a thread needs history, search or
retention it wants the same treatment runs got.

**The read is paged.** `GET …/tasks/{taskId}` answers with the newest
`DISCUSSION_PAGE` (50) messages plus `discussionHasMore`, and
`?discussionBefore=<seq>` walks backwards — the `first` + `before_seq` shape
`chat_history::history_for_desk` already uses. Without it the whole thread is
re-sent on every 4s poll, per browser, forever. The fold drops out-of-window
posts as it reads them, so a long thread is traversed but never resident. The
timeline is *not* paged: it is bounded by what the company did on one card.

**Operator-only in v1; agents do not participate.** Posting journals a message
and nothing else: no cycle runs, no turn is dispatched, and no agent reads the
thread back. The projections enforce it rather than merely documenting it — the
sidecar wire body (`wire_event`) and the orchestrator's insight line
(`summarize_event`) both name the *card* and deliberately omit the *text*, so
the tab cannot become an unannounced prompt surface. Nor does a post hold a slot
in the orchestrator's ten-event activity tail: they fold there into one
"N discussion posts" line, because a card's afternoon of back-and-forth would
otherwise evict every dispatch, reply and approval from the only view the
orchestrator has of the company — "agents do not participate" has to hold for
the *slot* as well as the *text*.

Agent participation is a product decision to take with first-class runs (#242):
a discussion anchored to a task and one anchored to a run are different things.
That deferral was cleaner when this branch was cut than it is now — #342 landed
`RunStore` and #357 landed the write path, so an attempt *is* a first-class
record on `main` and the question is answerable rather than blocked. Still
deferred, but on scope, not on missing prior art.

**Ordering, editing, references.** Oldest-first by journal sequence, which is
also the console's render key. There is still no *edit*: what was said stays
said, and a message that could be rewritten after the fact would make the thread
unciteable. A message is plain text: it cannot yet reference an artifact or an
approval, left until there is a link target more durable than a row's position.

### Withdrawing a discussion message (issue #358)

The gap #335 shipped with: a discussion is exactly where somebody pastes the API
key they are blocked on, the log is append-only, and the log is what
export/import ships — so a pasted secret was not merely permanent, it was
**portable**. Four decisions close it, and each of them is a decision rather
than a default.

**A tombstone, not a delete.** `CompanyEvent::TaskDiscussionRedacted { task_id,
seq, by }` *supersedes* the post at `seq`; the post itself is never rewritten or
removed. The append-only property is load-bearing beyond this tab — no control
alters a past event, sequence numbers are stable ids other events name, and
import replays a bundle from zero to reproduce them — and a delete would break
all three to serve one screen.

**What a reader sees.** The row keeps its place, its `seq`, its author and its
time; its text is replaced by `REDACTED_DISCUSSION_TEXT` ("This message was
removed.") and the row carries `redacted: true` plus `redactedBy`, the label of
whoever withdrew it. Not a silent disappearance: a message that vanishes without
trace lets one member quietly rewrite what a thread said, which is a different
product from "anyone may take back a mistake in the open". The substitution
happens in the fold, so the console thread and the task-detail export document
(#352, which renders the same assembled value) cannot disagree.

**Export and import.** `store::export::scrub_redacted_discussion` applies the
same substitution to `events.jsonl` — on the way **out**, so a bundle never
carries a withdrawn message, and on the way **in**, so a bundle written by a
host that predates this cannot smuggle one back through import. The tombstone
travels with it, so the imported thread says the same thing the exporting one
did, with the same attribution. This is the half that actually closes the issue:
a redaction that stopped at the console would move the exposure somewhere nobody
is looking.

**Who may.** `DELETE …/tasks/{taskId}/discussion/{seq}`, under the same
`ScopedCompany` guard as every other task write — any member of the company, not
admin-only. That matches the surrounding surface rather than inventing a rule:
the same member may `DELETE` the whole card, thread and all, so holding one
message to a higher bar than the card containing it would be incoherent. The
withdrawal is attributed instead. An unknown card, or a `seq` that is not a
discussion post *on this card*, is a `404`; withdrawing an already-withdrawn
message is a no-op success, so a retry is not an error.

**What it does not claim.** This is not erasure at rest on the instance that
already holds the bytes — that is exactly what append-only forbids, and
pretending otherwise would be worse than the gap. A leaked credential still has
to be rotated. What the withdrawal guarantees is that the text stops being
served by any read surface and stops leaving the building. Posts journaled
before this shipped are fully covered, since the tombstone names a `seq` and
nothing about the post has to change; bundles already exported are not, and
cannot be.

**Scope: the discussion, not the journal.** The mechanism is deliberately
discussion-only. `OperatorMessage` has the same shape of problem, and generalizing
before there is a second caller would fix its shape around one. The next human-prose
writer that needs this should read this section and decide whether to widen the
event or add its own — the pattern to inherit is the pair (tombstone in the log,
substitution in every fold *and* in the bundle), not the variant.

**Attribution.** A post carries the signed-in user as `by`, resolved to a roster
label on read (a display name, or an email's local part). A user no longer on
the roster reads as `someone`; a post made with a machine credential reads as
`operator`. A user id and an email address never reach the wire — a thread is
read by every member of the company.

The write is `POST …/tasks/{taskId}/discussion` with `{text}`. Empty or
whitespace-only text is a `400` (there is no delete, so a blank row would be
permanent noise), an unknown card is a `404`, and over-long text is truncated to
`MAX_DISCUSSION_CHARS` rather than refused. The `201` echoes the journaled row —
read back at its own `seq`, not re-stamped — so the console renders the post at
once under the key the next poll returns it under. Reads ride the detail's 4s
poll, which is what makes another operator's post appear without a reload.

### Reading, editing and running workflows (issues #262, #259, #228)

The cron preview (`#262`), the `PUT`/`DELETE` authoring round trip (`#259`)
and run-time report delivery (`#228`) are documented together in
[workflow-routes.md](workflow-routes.md).

What a run **adds up to** — the `verdict` both run DTOs carry, why an
undelivered report is its own reading rather than a failure, and why it is
derived on the read rather than journaled (issue #981) — has its own focused
page: [run-verdict.md](run-verdict.md).

How the run history is **paged** — why the page is cut by `seq` but displayed
by `(atMillis, seq)`, why the cursor is server-issued rather than derived from
the last row, and what a console must do when an older host omits it
(issue #1012) — likewise: [run-history-paging.md](run-history-paging.md).

The **authoring** surfaces — building a graph from a task card (`#580`),
drafting one from a free-text description (`#753`), and grounding either on the
tools a company can actually reach (`#783`, `#874`) — have their own focused
page: [workflow-authoring-routes.md](workflow-authoring-routes.md). The files a
past run produced, deep-linked into each card's Artifacts tab (issue #1684), are
documented with the run routes in
[workflow-routes.md](workflow-routes.md).

### Pausing a workflow, and the disarm rule (issue #276)

The pause switch, what it does **not** stop, why it lives in
`disabled_workflows` rather than the manifest, and the create/edit disarm rule
have their own focused page: [pausing-workflows.md](pausing-workflows.md).

### Connections: hosted versus the self-hosted hatch (issues #396, #404, #582, #822)

The self-hosted OAuth hatch and why the console stopped offering it, the
`credentialSource` tiers, the single connection status behind the console's one
provider grid, and the two non-interchangeable disconnect routes have their own
focused page: [connections.md](connections.md).

## tiny.place A2A inbound + discovery (`tinyplace` feature)

Behind the `tinyplace` feature the server mounts the agent-to-agent surface
(`server::a2a`). With the feature off, none of these routes exist and the
default build links no crypto.

| Route | Purpose |
| --- | --- |
| `POST /a2a/{handle}` | JSON-RPC `tasks/send` from a counterparty agent |
| `GET  /a2a/{handle}` | the company's Agent Card (directory record) |
| `GET  /a2a/{handle}/skill.md` | human/agent-readable priced-skill catalog |
| `GET  /.well-known/agent-card.json` | the sole company's card (prosumer) |
| `GET  /companies/{handle}/.well-known/agent-card.json` | a named company's card |

`POST /a2a/{handle}` enforces the trust boundary in a fixed order before any
work reaches cognition:

1. Resolve a **discoverable** company (`[place].discoverable = true` with a
   matching `[company].handle`); a miss is `404`.
2. Verify the SIWX `Authorization` header (skew window + single-use replay
   protection via a host-global nonce cache). A bad/missing header is `401`.
3. For a skill priced above `0.00`, require a valid x402 authorization; without
   one the response is a `402` challenge naming the amount and the company's
   own tiny.place address.
4. Sanitize the counterparty payload (a minimal promptguard pass — control
   characters are stripped) before it becomes an `A2aTaskReceived` event and
   drives exactly one cycle. Paying customers run under the same approval gates
   as any other stimulus.

An unreachable tiny.place backend maps to `503`; any other transport failure is
`502`.

## Enable discovery for all companies

Every company declares its own discoverability in its manifest:

```toml
[company]
name = "Acme SEO"
handle = "acme"

[place]
discoverable = true
skills = [{ id = "seo.audit", price_usd = "25.00", description = "Full audit" }]
```

To opt **every** loaded company into going public regardless of its manifest,
pass `serve --discoverable`. It marks each company discoverable and synthesizes
a `@handle` (a slug of the company name) when one is missing, so Agent Card
generation and validation succeed:

```bash
cargo run --features tinyplace --bin opencompany -- \
  serve --discoverable \
  --company companies/agentic_law_firm \
  --company companies/agentic_marketing_agency
```

At boot each discoverable company runs the going-public flow (lifecycle step 3):
load-or-generate the Ed25519 keypair, `ensure_registered`, then publish the
Agent Card — all best-effort. An unreachable tiny.place degrades the company to
"private" with a warning and never blocks or fails boot.

Relevant configuration:

- `TINYPLACE_API_URL` — tiny.place economy base URL (default
  `https://api.tiny.place`).
- `OPENCOMPANY_PUBLIC_URL` — public host base embedded in published Agent Card
  endpoints. When unset, the endpoint falls back to `http://{bind}`.

## Inbound channel webhooks

There are none. `POST /hooks/{company}/telegram` was the hosted fast-path for
Telegram inbound; it left with the channel itself, along with the
`getUpdates` poller behind it. What reaches a company from outside now is mail
(IMAP in, SMTP out) and the console's own write plane.

The Chargebee webhook (`hooks_chargebee.rs`) is unrelated and stays: it is a
billing callback, not a messaging channel.
