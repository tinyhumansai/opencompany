# Console-surface stores (WS3)

The durable stores behind the operator console's own surfaces — the board, the
deliverables, the note tree, memory, usage, skills and inboxes. `RunStore` is
one of them and has its own file, [ports-runs.md](ports-runs.md), because its
contract is the longest. Part of the port contracts indexed by
[ports.md](ports.md).

Seven additional ports back the operator console's durable surfaces. They follow
the same one-trait-per-file convention (`src/ports/{tasks,workspace,facts,
usage,skills_state,inbox,runs}.rs`), key everything on `CompanyId`, return the
crate `Result<T>`, and are covered by the conformance suite
([storage.md](storage.md)). Their fs/sqlite/mongodb backends live alongside the
[five core ports](ports-state.md).

### TaskStore

The Kanban task board (`src/ports/tasks.rs`).

```rust
pub trait TaskStore: Send + Sync {
    async fn list(&self, company: &CompanyId) -> Result<Vec<TaskRecord>>;
    async fn upsert(&self, company: &CompanyId, task: &TaskRecord) -> Result<()>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}
```

`TaskRecord` carries `{id, title, note, column, priority, assignee,
updated_at}`.

`TaskRecord::column` — the **stage** — ∈
`todo|planning|in_progress|paused|in_review|done`, the `BOARD_COLUMNS` constant
in `src/ports/tasks.rs`, which is the one authority the REST write boundary, the
dispatch edge and the harness lifecycle seam all read. (`paused` arrived with
steering, issue #111; this line used to omit it.) Entering `in_progress` is what
dispatches the card; nothing dispatches out of `done`.

**The wire says less than the record does (issue #1512).** `TaskCard.column` on
the REST DTO is the stage's *phase* — `pending`, `working` or `done` — and the
stage rides beside it as `TaskCard.stage`, omitted on a pending or done card.
That is what the console renders as columns and what an agent reads in
`derived/tasks.md`: three states, because four of the six meant the same thing
to everyone who was not the runtime. A write takes a phase and the boundary
resolves it to that phase's entry stage (`working` → `in_progress`, which
dispatches); a stage word is still accepted, but the refusal names only the
three. See `docs/spec/runtime/ledgers.md`.

`todo` is the one **not started** stage, and the board's one manual-entry
column: the console's `+` button lives on Pending alone and `POST …/tasks`
defaults to it (issue #206), so an operator cannot create a card straight into
Working or a terminal column. The transcript's "Add to board" action (issue
#246) relies on exactly that default: it omits `column` so the *server* decides
where a chat-created card lands, which is what keeps the human drop into Working
the only thing that spends an agent turn.

**The collapsed `backlog` pool (issue #301, epic #183 §3).** `todo` used to be
one of two not-started columns: `backlog` was the unqueued pool *and* where the
lifecycle returned work needing another pass (a failed dispatch, a cancellation,
an orchestrator `revise` verdict). #206 split them deliberately, to record *why*
a card had not started — never picked up vs bounced back. #301 reverses that:
the distinction is **provenance, not position**, and every return path already
stamps its reason onto the card's note (`review_note`'s "reviewed: needs another
pass — …", the dispatch error text, `[operator] cancelled while in flight`),
which the board renders on the card. So a task that cannot proceed goes **back
to Pending with the reason on the card**, never into a stuck state of its own.

**The note stopped being the only carrier (issue #1865).** The reason text
above is still appended to `note` — nothing about that changed — but a settle
that lands a card back on `todo` because its run **failed or was cancelled**
now also stamps `TaskRecord::bounced: Option<String>` with the same reason,
via the one rule (`bounced_reason` in `src/runtime/advance.rs`) both card-write
sites share. That gives the board a structured signal to render a dedicated
chip instead of parsing prose out of the note, and — unlike the note, which is
append-only — `bounced` is cleared the instant the card leaves `todo` any other
way: a re-dispatch, a manual drag, or any other write that takes it off `todo`
(`task_leaves_todo` in `src/company/runtime.rs`). A card re-entering `todo`
later earns a fresh reading, never a stale chip left over from the last bounce.
`None` is the default for every card that has never bounced and every board
written before this field existed — additive on the wire like the rest of
`TaskCard`, so no stored board needs migrating.

Nothing about that is silent for stored data: `backlog` is no longer a board
column, so a card persisted under it would fail `is_board_column` and vanish
from the board — the exact silent disappearance #205 exists to prevent.
`TaskRecord::column` therefore deserializes through a normalizer that rewrites
the legacy `backlog` literal to `todo`. Every backend funnels through it (sqlite
and mongodb store the record as a `task_json` string, the fs bundle as a JSON
array), so one seam heals every stored board lazily on read and the next upsert
persists the new literal. Reads heal; **writes do not** — the REST DTOs
deserialize `column` as a plain string and validate it separately, so a client
still sending `backlog` gets a `400` naming the valid set.

`planning` sits between intake and dispatch: the card is being turned into a
plan. It is **accepted but inert** — nothing writes it automatically yet.
Epic #183 §4's auto-advance owns it and is blocked on #242/#243; the vocabulary lands
first so §4's code can write the column through a boundary that already accepts
it, rather than having #242-dependent code write a column the host rejects. An
operator may drag a card into it manually and nothing happens, which is correct:
`planning` is not the dispatch edge.

`assignee` names a **roster teammate id, a desk, or nobody** (`""`), resolved by
`crate::runtime::assignee` against the full roster — manifest agents, operator
overlay teammates, and desks (by id or case-insensitive name). The write plane
rejects anything else with a `400` and stores the canonical key rather than what
was typed; dispatch refuses a card whose assignee no longer resolves, returning
it to `todo` with the reason on the note, and writes the agent that actually
worked the card back onto `assignee` so the board names the doer (issue #205).
That write-back covers an unassigned card and a card assigned to a teammate; a
card assigned to a **desk** keeps the desk id. A desk assignment records who the
card belongs to, and dispatch only chooses which member runs the current turn, so
writing the lead back would erase the desk from the board the first time the card
ran — the member that did the work is named on the note instead.

### ArtifactStore

Versioned task outputs and the human-edit diff (`src/ports/artifacts.rs`,
issue #187) — what the Task Detail **Artifacts** tab renders.

```rust
pub trait ArtifactStore: Send + Sync {
    async fn list(&self, company: &CompanyId, task_id: Option<&str>)
        -> Result<Vec<ArtifactRecord>>;
    async fn get(&self, company: &CompanyId, id: &str) -> Result<Option<ArtifactRecord>>;
    async fn upsert(&self, company: &CompanyId, artifact: &ArtifactRecord) -> Result<()>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}
```

`ArtifactRecord` carries `{id, task_id, title, kind, versions, created_at,
updated_at}`; `ArtifactKind` ∈ `text|markdown|image|file`. Each
`ArtifactVersion` carries `{version, body, author, author_id, created_at,
step_seq?, note?}`; `ArtifactAuthor` ∈ `agent|operator`.

**Versions are append-only.** An operator's pre-approval edit is recorded as a
*new version by a different author*, never as a mutation of the agent's — which
is what makes `human_edit_diff()` ("the agent wrote X, the operator shipped Y")
answerable at any later point, and why no route rewrites a stored version.
Editing in place would destroy the single highest-signal quality datum the
product can produce: sustained high `churn` on an agent's artifacts means its
instructions need work.

Independent of the per-task timeline (#185). A version may cross-reference the
step that produced it via the optional `step_seq`, but this port never reads the
event journal, so an artifact stands on its own.

Backends must uphold `store::conformance::assert_artifact_store`, which asserts
the full ordered version history survives a round-trip — a backend that stored
only the latest body would otherwise pass a naive check while silently
destroying the diff.

### WorkspaceStore

The Obsidian-style note tree (`src/ports/workspace.rs`), seeded from the
company's `workspace/**` on first use and thereafter written by both the
operator (console/REST) and the company's agents (`workspace_create` /
`workspace_write`, plus `workspace_rename` / `workspace_delete` within
`agents/<agent-id>/` since issue #671).

```rust
pub trait WorkspaceStore: Send + Sync {
    async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>>;
    async fn read(&self, company: &CompanyId, id: &str)
        -> Result<Option<(WorkspaceNode, String)>>;
    async fn write(&self, company: &CompanyId, id: &str, content: &str,
                   author: WorkspaceOrigin) -> Result<WorkspaceNode>;
    async fn create(&self, /* parent, name, kind, content */) -> Result<WorkspaceNode>;
    async fn adopt_or_create_folder(&self, company: &CompanyId, parent: Option<&str>,
                                    name: &str, origin: WorkspaceOrigin)
        -> Result<FolderClaim>;                  // Created | Adopted  (#759)
    async fn rename_move(&self, /* id, new_name, new_parent */) -> Result<WorkspaceNode>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
    async fn is_empty(&self, company: &CompanyId) -> Result<bool>;

    // Bytes (#553) — additive; the text path above is untouched.
    async fn create_binary(&self, company: &CompanyId, node: &WorkspaceNode,
                           bytes: &[u8]) -> Result<()>;
    async fn write_binary(&self, company: &CompanyId, id: &str, bytes: &[u8],
                          mime: Option<&str>, author: WorkspaceOrigin)
        -> Result<WorkspaceNode>;
    async fn read_bytes(&self, company: &CompanyId, id: &str)
        -> Result<Option<(WorkspaceNode, BlobStream)>>;
}
```

Nodes are folders or files (`NodeKind`); `[[wikilink]]` backlinks are derived
at read time by the GraphQL layer.

**No torn reads (#887).** A `read` concurrent with a `write` on the same node
returns one whole revision or the other — never an error, and never a prefix.
sqlite and mongodb get this from a row update and a document replace; `fs` gets
it from a tmp-file-plus-rename (`store::fs::write_atomic{,_bytes}`), which is
what it was missing. `conformance::assert_workspace_read_never_tears` holds all
three to it. The half that made this a data-integrity bug rather than a noisy
one: a prefix that happens to end on a codepoint boundary *decodes cleanly*, so
an agent grounds its answer in half a document with nothing failing anywhere.

**Bytes (#553).** A node holds prose or it holds a payload, never both. A
binary node is a `File` whose `mime`, `size` and `sha256` are all `Some`;
`mime` alone is the discriminator every surface keys off. `size` and `sha256`
are computed **by the store** from the bytes it persists (`blob_metadata`) and
are never accepted from a caller — a digest a caller supplies is an unverified
claim, which is the one thing a digest must not be.

The text path stays `String`, deliberately: ~17 call sites reference this port
and none of them (the backlink scan, the seeder, the GraphQL projection, the
agent tools) can do anything with a PNG. So a text `read` of a binary node
yields an **empty body**, the same answer a folder gives, and a text `write` to
one is refused rather than allowed to leave the recorded digest describing
bytes that are gone.

Writes buffer and reads stream. The asymmetry is what makes the quota
enforceable: `QuotaEnforcedWorkspace` (`src/runtime/workspace_quota.rs`) wraps
the store at the single assembly site, inside the announcer, and sees the full
size **before** anything is written — so a refused write leaves no partial
blob, no node and no orphan. It meters payloads only (prose is uncounted; the
threat model is media) against a per-file cap (`[workspace] max_blob_mb`,
default 64 MiB) and a per-company total (`[workspace] tree_quota_gb`,
unlimited by default), answering 413. The console's upload route holds a
separate 256 MiB body limit — a backstop against buffering an unbounded
request, deliberately above the cap so the store's refusal is the one an
operator sees (#647); see `storage.md`.

Backends: sqlite keeps the blob in the node's own row, so it cannot orphan one.
`FsOps` writes the file then the index — the benign order the text path already
uses. MongoDB needs **GridFS** (a payload cannot ride in a 16 MB BSON document)
and writes blob-first/document-second, deleting in the mirror order, so a crash
strands only a blob nothing references; `MongoStore::from_database` sweeps those
at boot. Tenancy in the shared bucket is a filter on `metadata.company_id` **and**
`metadata.node_id` for every read, delete and sweep.

**Folder claims (#759).** `adopt_or_create_folder` is the only way the publish
walk and the system-root scaffold create a folder. It is a store primitive rather
than a read plus a `create` because the read is honest about one instant and the
create acts on it later: two publishes needing `artifacts/<agent>/<task>/` both saw
it free, and sqlite and mongodb both created — leaving two folders under one
name. That state does not decay. Path resolution answers a duplicated name with
`Conflict`, so a race lasting microseconds refuses every later publish beneath
that path, for every agent, permanently.

Afterwards exactly one folder answers to `(parent, name)` among the nodes this
primitive governs, and the returned node is it. A `parent` of `None` is the
workspace root, so the roots are claimed by the same call as everything below
them. Adoption **preserves the original authorship stamp**: `origin` is used only
when this call is the one that creates. A file holding the name, or a
pre-existing ambiguity, is a `Conflict` — fail-closed, exactly as before.

`FolderClaim::{Created, Adopted}` exists for one consumer: `WorkspaceAnnouncer`
emits its node-created frame **only** on `Created`. A frame for a folder that was
already standing would tell an open console something appeared when nothing did,
on the adoption path nearly every publish takes.

It is deliberately not `swap_files`' compare-and-swap. That stages a payload and
makes the loser *fail*, which is right for a file — its bytes are a content claim
with one legitimate winner. A folder is a payload-free container claim: two
publishers wanting one folder want the same thing, so the loser must adopt and
carry on. That also dissolves loser cleanup, since a loser never owns a folder.

Backends decide it where each of them can: `FsOps` under the workspace-index lock
it already holds for every write (single-process per data dir by contract),
sqlite inside a `TransactionBehavior::Immediate` transaction (two stores can open
one file), and MongoDB with a second partial unique index — see `storage.md`. The
agent's own `workspace_create` tool is **not** routed through this: adopting there
would silently merge two intentionally separate hand-made folders.

**Search (#607).** Workspace search is a **company-layer helper**
(`company::workspace_search`) over the port's existing `tree` + `read`, not a
port method. A trait method would need five implementations *and* one agreed
definition of matching across three engines, and SQLite's FTS5 tokeniser,
MongoDB's `$text` stemming and a hand-rolled filesystem scan cannot be made to
agree without either forbidding the native indexes or accepting that the same
query answers differently per deployment. The helper is correct on all three
backends by construction; the cost it accepts is O(N) reads per query, the same
shape `workspace_links` already pays on every file open, and it is the named
place to add an index later.

A binary node is **matched by name and never content-scanned**. Its bytes are
not text, so scanning them would produce mojibake matches, waste I/O
proportional to the payload, and — on a streaming backend — pull a whole video
through memory to find nothing.

The port already makes the safe behaviour the default rather than a rule to
remember: a text `read` of a binary node returns an **empty body** on all three
backends, so a content scan built over `read`/`tree` finds nothing for one
automatically. It does not need to know binaries exist, and it cannot
accidentally index them. The helper states the rule anyway rather than
inheriting the silence, because "found nothing" and "not searchable this way"
are different answers and only one of them is true. A scan that wants to *say*
something about a payload uses `mime`/`size`/`sha256` off the node, which the
tree read already carries — the same fields `workspace_list` renders.
`read_bytes` is for serving a download and appears nowhere in a search path.

Paths in a hit come from `company::workspace_paths`, the same ancestor-chain
rules the agent tools' `PathIndex` uses, so a node search offers is always a
node `workspace_read` can open — an unaddressable node (dangling ancestor,
illegal name segment) is excluded from hits and from `total` alike.

`assert_workspace_binary_store` pins all of it across all three backends,
including a 17 MiB case that proves the BSON cap is not in play. Over HTTP:
`GET …/workspace/blob/{id}` streams the payload (`ETag` = sha256) and
`POST …/workspace/upload` takes multipart; a text-typed upload whose bytes
decode as UTF-8 is stored as a note instead, so an uploaded `.md` keeps its
editor and backlinks.

An upload is still bounded by the company's `max_blob_mb` before that
text/binary choice (#665). The bound is a property of bytes entering through
the upload route, not their eventual representation; ordinary text writes stay
unmetered and text nodes still do not contribute to `tree_quota_gb`.

The filesystem backend mirrors logical names as real paths, so it refuses a
create, rename, or move that would land on a path another node already holds
(#666). It compares the whole resolved name chain rather than the sibling name,
because a tree can legitimately hold two folders under one name — the scaffold
finds such roots, declines to resolve them and leaves them standing — and their
equally-named children are not siblings by `parent_id` yet still resolve to one
path. The check runs under the workspace-index lock: a refused upload leaves the
existing node, payload, size, and digest unchanged. Id-keyed database backends
can continue to represent duplicate names without aliasing payloads.

**A stored `mime` does not decide how the blob route serves it (#667).** That
value is the uploader's declared `Content-Type`, or `mime_guess` on a published
deliverable's filename — a claim, not a property of the bytes — and the route is
same-origin with the console's `SameSite=Lax` session cookie, which a top-level
navigation sends. So `read_blob` classifies against a **closed list** instead:

| stored mime | served as | disposition |
| --- | --- | --- |
| raster image (`png`, `jpeg`, `gif`, `webp`, `avif`, `bmp`, `tiff`, `apng`, icon) or `application/pdf` | unchanged | `inline` |
| `image/svg+xml` | unchanged | `attachment` |
| anything else, including absent | `application/octet-stream` | `attachment` |

Every response also carries `X-Content-Type-Options: nosniff`, without which the
forced type is a suggestion. SVG is split out rather than folded into either
neighbour because it is an image *and* a document: an `<img>` will not decode it
without `image/svg+xml`, and inside an `<img>` the SVG spec's secure static mode
runs no script — but the same bytes at the top of a tab are a scripted document
on this origin. Keeping the type and forcing `attachment` serves both.

This lives on the **read** path deliberately. Sanitising on upload would leave
every payload already stored under a caller-chosen mime exploitable; classifying
at serve time covers those too. The console is unaffected either way — it fetches
blobs through `getBlob` and wraps them in an object URL, and `fetch` ignores
`Content-Disposition` entirely.

A repo-wide Content-Security-Policy is **not** part of this and is still absent;
its lack is not specific to this route and closing this one does not close that.

**Authorship (#326).** Every `WorkspaceNode` carries `created_by` and
`updated_by`, both a `WorkspaceOrigin` ∈ `seed | operator | agent{id}`. `write`
takes the author and stamps `updated_by`; `create` receives a fully-formed node
whose caller sets both. `rename_move` deliberately touches **neither**, so an
operator reorganising the tree cannot mask who wrote the body that is stored.
Backends persist the node as opaque JSON and both fields are
`#[serde(default)]`, so a node written before the fields existed loads as
`operator` — no column, no migration. `assert_workspace_store` pins the
round-trip, the write stamp, and the rename non-stamp across all three
backends.

**System workspace roots.** `company::workspace_scaffold` owns `agents/`,
`artifacts/`, `desks/`, and the operator-only `secrets/` subtree on different
schedules:

* `ensure_workspace_scaffold` adopt-or-creates the roots listed in
  `SYSTEM_ROOTS` — `agents`, `artifacts` and `secrets` — from one seam:
  `RuntimeBuilder::build` (boot). It takes no roster (a company with no agents
  still gets them), so an existing company picks them up on its next boot.
  `agents/` stays empty; `secrets/readme.md` explains that every agent workspace
  tool omits or refuses this subtree while operator APIs retain normal access,
  and `artifacts/readme.md` explains that an empty deliverable list is a real
  outcome rather than a gap. Idempotent.
* `ensure_agent_folder` / `ensure_artifact_folder` / `ensure_desk_folder`
  adopt-or-create `agents/<agent-id>/`, `artifacts/<agent-id>/` and
  `desks/<desk-id>/` **on demand**, returning the node id, called when that agent
  or desk first produces something. A folder means
  "this member produced something"; an eager folder per roster member would be
  a claim the tree cannot back. `workspace_create` calls the agent minter when
  an agent writes into its own home; #552's publish path calls the artifact
  minter.
* **`desks/` is not scaffolded** (#645). Both minters have always created an
  absent root on their way down, and `ensure_desk_folder` has no callers yet, so
  scaffolding the root gave every company a permanently empty folder
  advertising a feature nothing fills — the same promise-not-record shape #570
  removed at the member level. `ensure_desk_folder` now mints root and member
  folder together on first use. A `desks/` that already exists is untouched:
  the scaffold only ever looks up the names in `SYSTEM_ROOTS`, so a legacy
  company keeps its root, its contents and its authorship, un-warned.

Both are fail-closed: a name collision (a *file* of that name, or several nodes
sharing it) is never resolved by creating a duplicate that would make the path
permanently ambiguous. The scaffold warns and skips, since nothing waits on its
result; a minter returns the collision as an error, since its caller needs the
id. The agent/desks folders are organizational and attribution units only;
agents may create and write ordinary shared content anywhere. `secrets/` is
the operator-only exception on the workspace tool surface.

### FactStore

The operator's durable, hand-curated Memory view — distinct from the two
cognition-facing memory ports (see
[company-brain/memory.md](../company-brain/memory.md)).

```rust
pub trait FactStore: Send + Sync {
    async fn list(&self, company: &CompanyId, /* query, kind, page */)
        -> Result<Vec<FactRecord>>;
    async fn upsert(&self, company: &CompanyId, fact: &FactRecord) -> Result<()>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}
```

`FactRecord` carries `{id, kind, title, body, source, updated_at}`; `FactKind`
∈ `fact|preference|person|project|reference`.

### UsageMeter

Durable per-company usage accounting (`src/ports/usage.rs`); the WS5
usage/finances projections read it.

```rust
pub trait UsageMeter: Send + Sync {
    async fn record(&self, company: &CompanyId, sample: &UsageSample) -> Result<()>;
    async fn query(&self, company: &CompanyId, since_millis: u64)
        -> Result<Vec<UsageSample>>;
}
```

`UsageSample` records one metered event (`SampleKind::Inference` tokens or
`SampleKind::OauthCall`). **Writers** — three, and they do **not** share failure
semantics:

| Writer | Called | On write failure |
| --- | --- | --- |
| `metering::inference::record_inference_usage` (always compiled) | per cycle by `CycleRunner`, for every cognition path that is not `PerTurn`-metered | logs and swallows — returns `()`, so the cycle still succeeds |
| `metering::oauth::record_oauth_call` | per connected-tool call | logs and swallows — returns `()` |
| `harness::cost::record_turn_cost` | per turn by the openhuman harness's cost hook | **propagates** — returns `Result<()>` and `HarnessPool::run_inner` applies `?`, so a ledger or meter failure fails the turn |

The per-cycle and OAuth paths hold "accounting never fails the work it accounts
for"; the per-turn harness path deliberately does not, because it writes the
`inference.spend` ledger entry in the same call and a silently dropped ledger
write is a money bug. **Retention:** backends evict samples older than
**90 days** (`RETENTION_DAYS`, the console's maximum `D90` window) on write,
anchored to the newest observed sample for deterministic eviction. Samples are
non-secret accounting rows; money still resolves from the ledger and `[budget]`.

**Model attribution (issue #1749).** `UsageSample::model` is an
`Option<ModelSlug>`, not a `String`. `provider` says *who served* the tokens
(`subscription`, `byok`, `ollama`); only `model` says *what ran*, which is what
"is this company's spend going to Sonnet or to Haiku?" asks. It is a closed
vocabulary — `<vendor>` or `<vendor>-<line>`, plus this repo's four workload
tiers, plus `other` — because a BYOK or `openai_compatible` tenant names its
models itself and that string is operator-authored free text: as a payload it
is a content leak, and as a stored column it is unbounded-cardinality data kept
for 90 days. The raw name is classified inside the harness, at the same place it
is put on the wire (`HarnessModel::telemetry_model`), and never leaves it; the
vocabulary and the rule for extending it are documented in
`src/metering/model.rs`; `ModelSlug::as_str` returns a `&'static str`, so a
telemetry payload can carry it directly without a second classifier. `ModelSlug`'s `Deserialize` re-classifies, so a stored
row cannot smuggle raw text back into the process either. A provider publishes
that value only **after its own call has succeeded**: one provider is shared by
every agent on a company and the cache is read after a turn finishes, so a turn
that was rejected — and therefore metered nothing — must not name the model for
a concurrent turn that did run. `None` means no model
to name — an `OauthCall`/`SearchCall`, a path that cannot identify one, or a
sample written before the field existed; the field is `#[serde(default,
skip_serializing_if)]`, so pre-existing rows on all three backends load
unchanged and need no migration.

### SkillStateStore

Per-company installed-skill state overlay (`src/ports/skills_state.rs`) —
enable/disable and provenance on top of the read-only `skills/` directory.

```rust
pub trait SkillStateStore: Send + Sync {
    async fn list(&self, company: &CompanyId) -> Result<Vec<SkillState>>;
    async fn set(&self, company: &CompanyId, state: &SkillState) -> Result<()>;
    async fn remove(&self, company: &CompanyId, slug: &str) -> Result<bool>;
}
```

`SkillState` carries the slug, `enabled`, and a `SkillSource`
(`company|registry|custom`).

### InboxStore

Per-teammate email inboxes and their messages (`src/ports/inbox.rs`).

```rust
pub trait InboxStore: Send + Sync {
    async fn inboxes(&self, company: &CompanyId) -> Result<Vec<InboxMeta>>;
    async fn set_enabled(&self, company: &CompanyId, key: &str, meta: &InboxMeta)
        -> Result<()>;
    async fn messages(&self, company: &CompanyId, /* key, page */)
        -> Result<Vec<EmailRecord>>;
    async fn append(&self, company: &CompanyId, msg: &EmailRecord) -> Result<()>;
    async fn mark_read(&self, /* company, key, ids */) -> Result<u64>;
}
```

Real send/receive depends on the domain/SMTP transport and the HMAC-signed
inbound ingest webhook ([api.md](api.md)); the store itself is transport-blind.
