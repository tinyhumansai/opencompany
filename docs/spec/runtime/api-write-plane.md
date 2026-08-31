# The console write plane

Every write the operator console makes, under `src/server/ops/`.

Split out of [`api.md`](api.md), which was over the repository's 500-line ceiling.
That file is the map of the API surface; this one is the write routes in detail.

## Console write plane (`src/server/ops/`)

The console's writes are a REST router family under `src/server/ops/`, each
route registered under **both** scope forms (`…/companies/{id}/…` and the
`…/company/…` prosumer alias) by the `scoped` helper. These are the mutations,
plus a deliberate set of **read exceptions** — reads the console must reach over
REST because it ships no GraphQL client: the two inbox `GET`s and the three
workspace `GET`s (tree, file, `search`), the task export
(`GET …/tasks/{taskId}/export`), the skill-registry browse
(`GET …/skills/registry`), the agent detail (`GET …/team/{agentId}`), the policy
read (`GET …/policy`), the activation read (`GET …/activation`, issue #1843),
and the credential status `GET` — every one detailed below or in the
credential section. Every other console read goes through
GraphQL (see the [read plane](api-graphql.md)). Anything a build doesn't serve
`404`s — the console treats that as "not wired yet".

```text
POST   …/tasks                              create a task card (`originChatId` records the thread it came from, #246)
PATCH  …/tasks/{taskId}                      edit / move a task
DELETE …/tasks/{taskId}                      delete a task
GET    …/tasks/{taskId}/export               the task's record as a readable HTML document (#352)
POST   …/tasks/{taskId}/discussion           post a message to the card's thread (#335)
POST   …/memory                             add a memory fact
DELETE …/memory/{factId}                     delete a memory fact
GET    …/workspace                          the whole tree (metadata; no bodies)
GET    …/workspace/file/{nodeId}             one file: content + inbound backlinks
GET    …/workspace/search?q=…                which notes mention a phrase (#607)
POST   …/workspace                          create a folder/file (or upload)
PUT    …/workspace/file/{nodeId}             write file content
PATCH  …/workspace/{nodeId}                  rename / move
DELETE …/workspace/{nodeId}                  delete a node
POST   …/workspace/sweep-empty-agent-folders?dry_run=  tidy `agents/` strays (#700)
POST   …/workspace/merge-duplicate-folders?dry_run=    repair a raced tree (#759)
POST   …/skills                             add a custom skill
GET    …/skills/registry                     browse the shared skill library
POST   …/skills/{slug}/install              install a registry/company skill
POST   …/skills/{slug}/uninstall            uninstall a skill
PUT    …/skills/{slug}                       enable / disable a skill
POST   …/team                               add an operator-overlay teammate
GET    …/tools/catalog                       everything this company can grant
GET    …/team/{agentId}                      one agent in full (tier, tools, desks)
PATCH  …/team/{agentId}                      edit a teammate
DELETE …/team/{agentId}                      remove a teammate
PUT    …/team/{agentId}/inbox                toggle a teammate's inbox
PUT    …/team/{agentId}/budget               set / change / remove a daily cap
DELETE …/team/{agentId}/budget               reset the cap to the manifest default
POST   …/team/{agentId}/draft                one copilot turn on this teammate's mandate or persona (writes nothing)
POST   …/team/draft                          the same, for a teammate being added
POST   …/avatars                            upload an image → an avatar reference (avatars.md)
POST   …/setup/roster                       propose a starting team from three answers (company-setup/overview.md)
GET    …/policy                              the autonomy tier + spend cap + deadline + always-ask list
PUT    …/policy                              set the tier, spend cap, deadline, and/or always-ask list
DELETE …/policy                              reset the policy to the manifest's
GET    …/activation                          the account-activation funnel (issue #1843)
PATCH  {scope}                               set/confirm the company's display name (issue #1844)  [admin]
GET    …/tools/grants                        the `[tools].allow` in force + what a connect page may grant (#1796)
PUT    …/tools/grants                        grant one namespace from a connect page  [admin]
DELETE …/tools/grants                        withdraw one console grant (`?namespace=`) or all of them  [admin]
POST   …/inboxes/{key}/read                  mark inbox messages read
POST   …/inboxes/ingest                     HMAC-signed inbound email → inbox
GET    …/inboxes                            list inboxes + unread counts
GET    …/inboxes/{key}/messages              one teammate's mail (store order)
```

The two inbox `GET`s are the read exception, and they are **REST twins of the
`Company.inboxes` GraphQL resolver**: the operator console ships no GraphQL
client, so without them the Inbox view had no reachable per-agent read at all
and fell back to a client-side fixture (issue #173). They read the same
`InboxStore` both inbound paths — the ingest webhook and the IMAP poller — file
into, and `GET …/team` tags each teammate with `inboxEnabled` so the Team toggle
reflects that store too. Messages come back in append order; the console sorts
them newest-first. The GraphQL resolver stays the canonical read for any client
that does speak GraphQL — these routes duplicate it, they do not replace it.

The two original workspace `GET`s (#177) are the same story one issue later:
the console had no reachable workspace read either, so the Workspace tab
persisted to `localStorage` and the operator and the agents looked at two
different trees —
a note written by an agent through its `workspace_*` tools (#237) was invisible
to the operator, and vice versa. They are REST twins of `Company.workspaceTree`
/ `workspaceFile`, differing only in timestamp shape (epoch millis, matching
every other console read, rather than ISO-8601 strings). The backlink scan is
literally shared code (`company::workspace_links`), so the two surfaces cannot
report different backlinks for the same note. The tree read carries metadata
only — bodies are fetched per file, so a navigation read does not grow with the
size of the workspace. Reading a folder id as a file is a `404`, never an empty
note.

`GET …/workspace/search?q=…` (#607) is the **third workspace read**: it answers
which notes mention a phrase, so discovery costs one call rather than a listing
plus one read per candidate.
Matching is a plain **case-insensitive substring** over node names and text
bodies — no tokenising, no stemming, no ranking — which is the only definition
that answers identically on all three storage backends, and it is defined once
in `company::workspace_search`, shared with the GraphQL `Company.workspaceSearch`
resolver and the agent `workspace_search` tool. Optional `prefix` scopes to a
subtree; optional `limit` pages the answer (default 20, hard cap 50). A hit
carries the node, its logical `path`, whether the `name` or the `content`
matched, and — for a content match — a short `excerpt`. `total` reports every
match, so a capped page says it is one. An empty `q` is a `400`, not "match
everything": that is the tree read above, and answering it here would turn a
cleared search box into a full-tree fetch. `limit=0` is a `400` for the same
reason — it is never read as "no limit". A **binary** node matches on its name
only: a text read of a payload is empty by the port's definition, and its bytes
are never scanned or excerpted.

`POST …/workspace/sweep-empty-agent-folders` (#700) removes the empty
`agents/<id>/` folders a pre-#570 company still carries. Operator-triggered
rather than automatic — the affected tenants are hosted, so a subcommand would
be unreachable for the operators who need it, and a boot sweep would change a
tree on an upgrade nobody asked for. `?dry_run=true` answers
`{"wouldRemove":[{id,name}…]}` and touches nothing, so the console can name
every folder on a confirm dialog; the real call answers `{"removed":[…]}`, and
which field carries the list is what actually happened. A node qualifies only
when it is a direct child of the `agents` root **by id**, is a folder, and has
**no children counted structurally** — over every node in the tree, before any
path is rendered, because a folder whose only child carries a path separator
reads as empty to anything path-shaped while the recursive delete would still
take it (#671), and sqlite and mongodb both accept such a name. An ambiguous
root is a `409`, not a guess. Each removal announces its own
`WorkspaceChanged{removed}` (#327); a second run removes nothing.

`POST …/workspace/merge-duplicate-folders` (#759) repairs a tree a publish race
already broke. Two concurrent first publishes of one deliverable can each create
the folder they both read as missing; from then on every publish beneath that
path is refused as ambiguous, for every agent, until somebody edits the tree.
Stopping new races does nothing for a tenant already in that state, and on a
hosted tenant this route is the only way out of it — operator-triggered, never
automatic, for the reason the sweep above is.

A duplicate set is two or more sibling **folders** sharing a name, matched by
`parent_id` and never by rendered path. The oldest by `updated_at_millis` wins,
with the node id breaking a tie so a preview and the confirm behind it cannot
disagree. The losers' children are `rename_move`d into the winner — ids
preserved, so an artifact recorded against a published node still resolves —
and a folder-folder collision among those children becomes another merge in the
same run, iterating to a fixpoint. Nothing is renamed, nothing is overwritten,
and a loser is deleted only once it is **structurally empty in a fresh read**
taken after the relocations, so a publish landing mid-merge leaves its folder
standing rather than losing anything (#671 / #700 discipline).

Any collision involving a **file** is refused and reported: two files at one
path are two documents, and every rule for picking one discards somebody's work.
`?dry_run=true` answers `{"wouldMerge":[{id,name,intoId,moved:[{id,name}…],
removed}…],"residuals":[…]}` and touches nothing; the real call answers
`{"merged":[…],"residuals":[…]}`, and which field carries the folds is what
actually happened. `residuals` is on **both**, always — each entry is
`{id,name,parentId?,cause}` with `cause` one of `fileSharesTheName` (a note
shares the duplicated name, so the whole group was left alone), `fileInTheWay`
(the winner already holds a note of that name), or `treeMovedOn` (the store
refused the move because the tree changed; run it again). It is the half of the
answer that says whether the tree is actually repaired. Moves and deletes
announce `WorkspaceChanged` (#327); a second run with nothing left to do changes
nothing.

All three workspace `GET`s — tree, file and `search` — and the `POST` / `PATCH`
node bodies carry `createdBy` and `updatedBy` (#326), each
`{"kind":"seed"|"operator"|"agent",
"id"?}` with `id` present exactly when `kind` is `agent`. `createdBy` is fixed
at creation; `updatedBy` follows content writes only, so an operator rename does
not repaint an agent's authorship. The console renders the creator as a badge
and the last writer only when the two differ. Both fields are always serialized,
and a node predating the field reads back as `operator`. The `PUT` write route
stamps `operator`; agent writes stamp `agent{id}` from the agent's roster id,
which is fixed at agent-build time and never taken from tool arguments. Agents
reach the same tree through `workspace_list` / `workspace_search` /
`workspace_read` / `workspace_create` / `workspace_write`, and a created note has its default home
in the reserved `agents/<agent-id>/` folder (#551) — a convention the persona
brief steers toward, not a boundary the routes enforce. `workspace_rename` and
`workspace_delete` (#671) are the exception: those two *are* bounded to
`agents/<agent-id>/`, checked on the resolved node so an `id` argument refuses
exactly as its path would. Neither restamps authorship; a delete leaves any
artifact version that pointed at the node with a dangling `workspaceNodeId`,
which is the same state the `DELETE` route above produces and is read-guarded
before reuse. Boot scaffolds the
`agents/` root empty; an individual `agents/<agent-id>/` is minted the first
time that agent writes into it, and the `desks/` root is minted whole the first
time a desk produces something (#645) — so a tree read on a fresh company shows
exactly one root and no member folders.

Team writes are an **operator overlay** persisted through the store, merged
into the manifest roster at read time — the version-controlled `company.toml`
is never rewritten. Overlay teammates are addressable: since issue #71 the
harness builds a real agent for each one, with the company-wide tool grant, no
cognition tier, and never the orchestrator.

`POST …/team` and the orchestrator's `add_agent` tool both **derive the roster
id from the display name** (issue #686): "Dana Designer" becomes
`dana_designer`, in the same snake_case grammar the manifest validator enforces
on a hand-authored `[[agent]].id`. They used to mint an opaque
`{millis}-{counter}` id, which #570/#552/#607 render as a workspace folder and
in search-hit paths — so half a company's tree read as
`agents/019fad5ada20-000000000003/` beside `agents/backend_engineer/`.

- **Collisions suffix, they do not refuse.** A slug already held by a manifest
  agent, another teammate, a desk id or name, or a reserved word (`operator`,
  `agents`, `desks`) becomes `<slug>_2`, `_3`, … Duplicate display names have
  always been accepted here, and an unsuffixed collision with a *manifest* id is
  worse than a refusal: the roster build skips it, so the teammate would persist
  and never materialise.
- **Minted once, never re-minted.** `PATCH …/team/{agentId}` renames a teammate
  and leaves the id alone; a name-keyed id would orphan its workspace folder,
  budget row, desk memberships and inbox on every correction.
- **Removal frees the slug**, so re-adding the same name takes the id back and
  **adopts the old `agents/<slug>/` folder** — the intended remedy for a typo'd
  name, and not a way to get a clean slate.

Teammates carrying generated ids are **not migrated**: rewriting them would
rewrite the `WorkspaceOrigin` stamps issue #326 keeps honest, and every path
into their folders. They keep working, reachable by display name through
`crate::runtime::assignee`.

`GET …/team/{agentId}` is the **agent detail** read (issue #264). `GET …/team`
answers "who is on the roster"; this answers "what is this agent", and before it
existed neither the console nor any other client could reach an agent's tier,
its tool grants or its desk membership — the roster row carried none of them, so
checking what a company actually grants an agent was not possible from outside
the process.

The `tools` object is the reason the route earns its keep. It carries four
lists, because only the last is the answer:

| field | meaning |
|---|---|
| `requested` | the agent's own `tools` globs. **Empty means the company's standard grant**, not "no tools" |
| `companyAllow` | the `[tools].allow` ceiling |
| `deskAllow` | the union of the `tools` ceilings of the desks this agent sits on, already narrowed by `companyAllow`. **Empty means the narrowed ceiling grants nothing** — which is *not* "no desk narrows anything"; `deskCeilingActive` tells those apart |
| `deskCeilingActive` | whether any desk this agent sits on states a `tools` ceiling. Distinct from `deskAllow`, which can resolve to an empty list while a ceiling is still in play — a console keying on `deskAllow`'s emptiness would substitute `companyAllow` and promise grants the host drops |
| `effective` | what the agent actually holds, after all three levels |

The three ceilings shrink monotonically, so a console can render them as a
chain. Both "empty" readings above are the same trap in two places: an empty
grant list means *inherit*, never *nothing*, and a surface rendering it as "no
tools" has inverted the meaning.

`effective` is computed by the same `agent_scoped_grants` the harness calls when
it builds the agent, so the readout cannot drift from what is enforced. The
constructor takes the company record and agent id rather than a pre-extracted
allow-list, because the desk level cannot be derived from the company grant
alone — that shape is what makes "forgot to apply the desk ceiling"
unrepresentable at a call site rather than something three callers each have to
remember.

`GET …/tools/catalog` is the companion read: every grantable thing this company
has — built-in families, `[[mcp_server]]` entries, `[tools.composio]` toolkits —
in one vocabulary, each row carrying the exact grant token an operator would
write. See [runtime/tools.md](tools.md).
`isOrchestrator` is likewise resolved by the roster rule (a `tier =
"orchestrator"` agent, else the first declared) rather than read off `tier`, so
a company that tags nobody still names its orchestrator.

`global` marks the teammates the [global baseline](globals.md) merged in — the
ones every company has whichever vertical it started from. It is sent on every
row because a client
cannot otherwise tell a company somebody staffed from one nobody has: the
baseline is on every roster, so `length === 0` is a question with one answer.
The console's first-run gate turns on it
([company-setup/overview.md](company-setup/overview.md)); before the field existed that gate could
never open.

`PATCH …/team/{agentId}` edits a teammate's `name`, `role`, `description` and
`tools`. It is a patch: an omitted key is left alone, and `"description": null`
clears it — the two must stay apart or every partial save would erase an agent's
instructions. A blank `name`/`role` is `400` and an unknown teammate `404`.

A **manifest** teammate is edited here too, and this is the one thing the route
does differently: instead of rewriting `company.toml`, the host stores the change
as an `overlay_agent_edits` entry on the company record and resolves it through
`CompanyRecord::effective_agent`, the same call `build_roster` makes. So the
blueprint keeps stating what the company launched with, the overlay states what
the operator has since decided, and the console card and the running teammate
cannot disagree. The merge is per field, so a field nobody edited still tracks
the blueprint across a rebuild. This is what makes a *deployed* company's roster
— including the global-baseline agents every company gets — changeable at all: a
hosted tenant has no `company.toml` to edit and no redeploy to make. The edit
reaches the next turn rather than the next restart, because it moves the pool's
overlay fingerprint (`overlay_fingerprint`, see
[ports-state.md](ports-state.md)).

Every detail response carries an `editable` list naming the fields this route
will accept, so a client renders read-only from the host's answer instead of
re-deriving the rule. `tools` is admin-only for both kinds and three-state since
#1804 — `null` resets to the company's standard grant (a potential *widening*),
an **explicit empty list** (`[]`) is a deliberate deny-all, and a non-empty list
narrows — and `tier` is read-only for both: it has no override layer, and adding
one is a policy decision rather than a form field.

### Drafting a mandate or a persona

`POST …/team/{agentId}/draft` and `POST …/team/draft` run one turn of a
conversation about a teammate's `description` or `instructions`. **Neither
writes**: the answer is text beside the field, and it becomes a persona only
through the ordinary `PATCH` a Save performs. The conversation shape, the
host-side grounding, the four refusal reasons and the argument for why the
first-run design pass's rule still stands are in
[api-team-drafting.md](api-team-drafting.md).

`DELETE …/team/{agentId}` removes a teammate. An overlay teammate is deleted
outright — the record is the only thing that declares it. A **manifest**
teammate is removed by recording a tombstone in `overlay_retired_agents`, for
the reason an edit is an overlay: `company.toml` and the baseline merged into it
are re-read on every rebuild, so a teammate deleted by rewriting the roster
would simply come back. `CompanyRecord::effective_agents` filters the tombstoned
ids out, which is what takes the teammate off the roster, off its desks, out of
the delegation targets and out of the harness build — rather than merely off the
Team page. If it was the orchestrator, the role moves to the next teammate that
is actually there.

Either way the teammate's operator-added desk seats, its edit overlay and its
budget override go with it; a blueprint desk seat is left in the manifest and
filtered at read time. The **one refusal** is the company's last teammate
(`409`): an empty roster has no orchestrator, nobody to answer a message and no
way back from the console.

The two **budget** routes (issue #343) are how a teammate's `budget_usd_daily`
becomes changeable without a redeploy. Both are **admin-only** — a member gets
`403` and an unauthenticated caller `401` — and both stamp who set the cap and
when, surfaced as `budgetSetBy` / `budgetSetAtMillis` on the roster row. A
stored cap wins over the manifest, and the change is enforced on the teammate's
**next dispatch**: the harness fingerprints the override set alongside its other
freshness axes, so the roster is rebuilt before the next turn rather than at the
next process start.

`PUT` takes `{"budgetUsdDaily": <number|null>}` and the three cases stay apart
on the wire, which is the point of the route:

| body | effect |
|---|---|
| `{"budgetUsdDaily": 5}` | cap at $5/day |
| `{"budgetUsdDaily": 0}` | cap at nothing — a real cap, not "uncapped" |
| `{"budgetUsdDaily": null}` | remove the cap, beating a manifest cap |
| `{}` | **`422`** — an omitted key is never read as "remove the cap" |

A negative or non-finite amount is `400`; an unknown teammate is `404`.
`DELETE` drops the override so the manifest default applies again — distinct
from `PUT null`, and not expressible by it. `POST …/team` also accepts an
optional `budgetUsdDaily`, so a console-created teammate can be given a cap at
creation; only that form of the add requires an admin.

The three **policy** routes (issue #562) are the company-scoped twin of the
budget pair. Before them the autonomy tier lived only in `[policy].mode` and
nothing in the console read or wrote it, so an operator drowning in approval
cards could change it only by redeploying an edited `company.toml` — or, on a
hosted tenant with a read-only manifest snapshot, not at all.

`GET` returns the tier, spend cap, approval deadline, and always-ask list **in force**, what the manifest would restore, whether an override is set and by whom, and the selectable tiers with the host's own description of each (`POLICY_MODES` narrowed to tiers the console has text for, so it never offers one the host would downgrade). `PUT` takes `mode`, `alwaysApprove`, `autoApproveUnderUsd`, and `approvalTtlHours`, all optional and independent — `{"mode": "auto"}` leaves the other fields alone, `{"alwaysApprove": []}` clears the list (a real state, not a reset), `{"autoApproveUnderUsd": 25}` permits qualifying spends strictly below $25 without asking, `{"autoApproveUnderUsd": null}` stores an explicit no-cap override so every spend remains subject to approval, and `{"approvalTtlHours": 48}` changes the approval deadline. `{"mode": null}` and `{"approvalTtlHours": null}` stop overriding those individual fields, while `{}` is a **`422`** because a body that sets nothing is never stored. An unknown `mode` is `422`
too, not accepted-and-downgraded, or the console would show a tier the gate was
not running. Both writes are admin-only and attributed. `DELETE` restores the
manifest's `[policy]` — its own verb, since a `PUT` of the manifest's current
values would pin them. Tier, cap, and always-ask changes take effect on the
company's **next turn** (`ApprovalPolicy` is built per roster build, and these
overrides are fingerprinted alongside the other freshness axes). A deadline
change is enforced **immediately** by the live approval gate, including when
parked approvals are swept against their new deadline. The daily budget and
always-ask list still outrank a spend cap, and the strict cap comparison means
a spend exactly equal to the cap still parks. The override survives a rebuild
unless the seed's `[policy]` itself changed: version control wins when it
speaks, so tightening `company.toml` clears a looser setting here, and a
redeploy that changed nothing does not.

`GET …/activation` (issue #1843) answers the account-activation funnel: three
booleans (`nameConfirmed`, `integrationConnected`, `workflowRunSucceeded`), the
overall `isActivated` verdict, and `activationCompletedAtMillis` — omitted from
the body entirely (not `null`) until the funnel has latched. `nameConfirmed`
mirrors the record's own field; `workflowRunSucceeded` is true once the
journal shows one real (non-dry) run reach the `Ok` verdict, the same ladder
the console's Steps panel uses; `integrationConnected` requires a live Composio
connection **and** an explicit `composio` grant in `[tools].allow` — except in
two cases, where the step is waived rather than permanently blocking
activation. First, when the manifest can never grant that namespace at all
(several bundled companies drop `composio` on purpose, e.g. `agentic_math_lab`,
`agentic_product_team`). Second, when the running binary was built without the
`composio` feature — including the documented default `cargo run --bin
opencompany -- serve` — because that build has no client with which to hold a
connection, so no company running under it could ever satisfy the step. In
that build the step reads true even for a manifest that does grant `composio`
and holds no connection; a build that compiles the feature still requires a
live one. Once every step has read
true simultaneously, the answer **latches**: `isActivated` stays `true`
forever after even if a live signal later regresses (a connection gets
disconnected), because the question is "did this operator ever clear
onboarding", not "is onboarding state currently intact". This GET is not a
pure read — the first call where every step is true durably **persists** the
latch before answering, which is why it is a console read exception here
rather than a GraphQL resolver. It then appends the terminal
`OnboardingCompleted` event on a **best-effort** basis: the latch has already
landed by that point, so a journal failure is logged and ignored rather than
un-activating the company, and the response can report activation with the
audit entry missing. Every call after the latch is set short-circuits before any
Composio round trip or journal scan, so polling an already-activated company
stays cheap. Any member may call it — it decides nothing on the company's
behalf, only answers a question.

`PATCH {scope}` (issue #1844) is the funnel's naming step: it sets
`[company].name` and stamps `CompanyRecord::name_confirmed`, the field
`nameConfirmed` above mirrors. Admin-only, body is `{"name": "…"}`, `422` on a
blank name. Unlike every other console write it lands directly on the
manifest field rather than an overlay — see the route's own doc comment
(`src/server/ops/company_profile.rs`) for why that survives a rebuild.
Idempotent and re-callable after confirmation (an ordinary rename), but the
`OnboardingStepCompleted` audit event fires only on the first call. A company
saved by build code that predates the funnel is told apart from a fresh
company's own interrupted first boot by the store-level
`CompanyStore::activation_gate_seen` marker — see that method's doc comment.

### Tool grants

The three **tool-grant** routes (issue #1796) are the same shape again, applied
to the field next door — connecting an integration stores a credential, it
does not grant the tool namespace. Full detail, including the grantable-list
rationale and the three-answer "when does it take effect" table, is in
[api-tool-grants.md](api-tool-grants.md).

### Credential-bearing surfaces (feature-gated)

These write secrets to the `SecretStore` and expose only non-secret status.
The native OAuth compatibility routes below deliberately **do not** write a
credential: the old credential was unreachable by agents.

```text
GET    …/credential                         whether the company has its own key + which tier it presents
PUT    …/credential                         set / rotate / clear the company's TinyHumans key  [admin]
GET    …/domain                             the stored domain + records + last verify result, or `null`
PUT    …/domain                             set the custom domain  [admin]
POST   …/domain/verify                       server-side DNS check
GET    …/smtp                               non-secret SMTP status (`configured: false` when unset)
PUT    …/smtp                               store SMTP credentials (secret store)  [admin]
POST   …/smtp/test                           send a test email  [admin]
POST   …/connections/{provider}/start        retired native OAuth bridge → 410 JSON until 2026-09-30  [feature: oauth]
POST   …/connections/{provider}/disconnect   drop a legacy stored OAuth token  [feature: oauth]
GET    /api/v1/oauth/callback                retired browser landing page → 410 HTML until 2026-09-30  [feature: oauth]
```

The two `GET`s are the REST siblings of the GraphQL `Company.domain` /
`Company.smtp` reads and share their loaders, so the planes cannot disagree
about the fields they both carry. They can still differ in *detail*: REST
answers the full `DomainStatus` and `SmtpStatus`, while `DomainStatusGql` omits
the per-record `checks` from the last verify pass and `SmtpStatusGql` omits
`security`, `from_name` and `from_email`. Both are open to any member (the
`[admin]` line guards the company's outward identity, not the reading of it) and
neither carries credential material: the SMTP password is absent from
`SmtpStatus` by construction.

`PUT …/smtp` treats the password as a **patch** — a body that omits it keeps the
stored one, so a form can offer "stored — leave blank to keep" instead of
charging a credential re-entry for a from-name fix. A body carrying one behaves
exactly as before, and one that supplies neither with nothing stored is `400`.
A supplied password is stored **byte for byte** — leading and trailing
whitespace is preserved, because it can be significant to the remote server.
Trimming decides only *whether* one was supplied: a value that is empty or
entirely whitespace counts as omitted and keeps the stored password, so an
all-whitespace password cannot be set through this route. Any value with a
non-whitespace character in it is stored exactly as sent.

Keeping the stored password costs no read-modify-write. The configuration and
the password live under separate secret keys, so a passwordless save rewrites
the configuration and never touches the secret — a rotation arriving at the same
moment survives instead of being reverted, however many processes are writing.

Credentials written before that split still carry the password inside the
configuration blob; reads fall back to it, and the first passwordless save after
the split migrates it to its own key. It is **read-only**: the configuration
blob is rewritten on every save without it, guaranteed by
`#[serde(skip_serializing)]` on the field rather than by every construction site
remembering to pass `None` (issue #1770). Writing it back would not only put a
credential in the blob, it would overwrite the pre-split password that the
legacy read path still depends on. That migration is the one path that must
read and then write, so `PUT …/smtp` serializes per company for the duration of
the handler. The lock is in-process, which covers the deployed topology (a
tenant is a single container); two replicas of one company would reopen the
window on the legacy path alone, and closing it there would need a conditional
write that `SecretStore` cannot express today.

`…/credential` is the company's **one** TinyHumans key, presented by every
surface wired to it (**Composio today**) — see
[`credentials.md`](credentials.md) for the resolution order, the rotation
guarantee, and which surfaces are deliberately outside it.

### Retired native OAuth callback

`/api/v1/oauth/callback` stays reachable for a browser that began consent
immediately before a deploy. It returns a non-caching `410 Gone` HTML page that
says the authorization was not saved, why native OAuth cannot make agents able
to use the provider, and to use Composio instead. It ignores the provider's
`code` and `state` rather than exchanging or storing them.

`POST …/connections/{provider}/start` is likewise a `410 Gone` JSON response
with stable code `native_oauth_retired`, an explanatory message, and
`removalAfter: "2026-09-30"`. Both temporary endpoints send `Deprecation:
true` and a `Sunset: Wed, 30 Sep 2026 00:00:00 GMT` header. #1023 removes the
bridge after the cache compatibility window established by #979; it keeps
Disconnect and the read projection so tenants can release credentials written
before #828.
