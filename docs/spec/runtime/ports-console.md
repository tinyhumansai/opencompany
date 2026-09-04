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

Moved to [`ports-console-workspace.md`](ports-console-workspace.md) — this file was over the repository's 500-line limit. See that page for the full detail.

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
