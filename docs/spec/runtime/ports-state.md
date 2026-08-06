# Company state ports

The durable record of a company: its charter and roster, its append-only
journal, the two cognition-facing memory stores, its secrets, and its human
collaborators. Part of the port contracts indexed by [ports.md](ports.md);
backends for these live in [storage.md](storage.md).

## CompanyStore

Durable company records: charter, roster, ledger, approval queue.

The record also carries the **operator overlays** — teammates, desk members,
desk order, operator-created desks, (issue #343) `overlay_budgets`: the
per-teammate daily spend caps an admin sets from the console, which win over the
manifest's `budget_usd_daily` and are read through
`CompanyRecord::effective_budget` — the single reconciliation point the harness
gate, the approval policy, and both roster reads share, so a cap raised in the
console cannot be honoured by one surface and ignored by another. At most one
`overlay_budgets` entry may exist per teammate — the console write path upserts
through `CompanyRecord::upsert_budget_override`, and a bundle import carrying two
entries for the same teammate is rejected rather than resolved by guesswork.
And (issue #168) `overlay_workflows`: the
workflow graph bodies authored at runtime through the console's create dialog or
the orchestrator's `create_workflow` tool. These are persisted here rather than
written into `companies/<name>/workflows/<id>.toml` because the company source
tree is the version-controlled seed and, in hosted mode, a read-only crate mount
(writing there failed every hosted tenant with `EROFS`). Every reader unions the
two sources — `load_workflow_union` / `list_workflows_union` in
`src/company/workflow_file.rs` — with the committed seed file winning on an id
collision, matching the manifest-first convention the desk resolvers use. The
workflow scheduler (issue #169) reads through the same union, which is what
makes a schedule on a console-created workflow survive a restart.

**A boot rebuild is not a plain re-seed** (issue #208). `RuntimeBuilder::build`
persists the freshly-parsed seed manifest with exactly one field merged:
`[workflows].enabled` becomes the seed's ids plus every surviving
`overlay_workflows` id not already among them. `create_workflow` writes the graph
body and the enabled id in one save, so overlay presence *is* the enablement
invariant — deriving from the bodies carries a runtime enablement forward and
re-heals records an earlier rebuild wiped, with no migration. Ids dropped from
the seed, and enabled ids with no surviving body, do not survive.

Every other manifest field is **seed-authoritative**; for `[tools]` and
`[policy]` that is a security property, not a convention — a record-wins merge
would let a runtime grant or a relaxed approval mode outlive the operator
revoking it in version control. Runtime additions that must persist get their own
overlay field instead (`overlay_agents`, `overlay_desks`, `overlay_budgets`, the
`SecretStore` for console MCP credentials). `overlay_budgets` is the clearest
case for why: a manifest baked into a hosted tenant's image cannot be edited at
all, so without a record-side override the shipped cap would be the only cap
forever — while keeping it *seed-authoritative* for everything else is what stops
a console write from silently outliving a revocation in version control.

```rust
// src/ports/store.rs
pub trait CompanyStore: Send + Sync {
    async fn load(&self, id: &CompanyId) -> Result<Option<CompanyRecord>>;
    async fn save(&self, record: &CompanyRecord) -> Result<()>;
    async fn list(&self) -> Result<Vec<CompanySummary>>;
    async fn append_ledger(&self, id: &CompanyId, entry: LedgerEntry) -> Result<()>;
}
```

## EventLog

Append-only, replayable, single-writer per company. Boot replays the tail to
rebuild in-flight state.

```rust
// src/ports/events.rs
pub trait EventLog: Send + Sync {
    async fn append(&self, id: &CompanyId, event: CompanyEvent) -> Result<EventSeq>;
    async fn read_from(&self, id: &CompanyId, seq: EventSeq, limit: usize)
        -> Result<Vec<StoredEvent>>;
    fn subscribe(&self, id: &CompanyId) -> BoxStream<'static, StoredEvent>;
}
```

**The `CompanyEvent` vocabulary lives in
[`events.md`](events.md)** — every variant, the additive-serialization contract
they all share, and the correlation rules that fold a company-scoped journal
back into per-task, per-approval and per-run views:

* [Variants](events.md#variants)
* [Per-task event correlation (issue #185)](events.md#per-task-event-correlation-issue-185)
* [Per-task approval correlation (issue #333)](events.md#per-task-approval-correlation-issue-333)
* [What a retry would repeat (issue #351)](events.md#what-a-retry-would-repeat-issue-351)
* [Workflow run progress (issue #371)](events.md#workflow-run-progress-issue-371)

It was split out (issue #371) because the port contracts had grown past the
repo's 500-line cap for a Markdown file, and the event vocabulary is the half
that keeps growing: these files own the port *traits*, that one owns the
*payloads* they carry.

## MemoryStore

The equivalent of Medulla's `CyclePersistence`; TinyCortex is the target
backend ([integrations/tinycortex.md](../integrations/tinycortex.md)).

```rust
// src/ports/memory.rs
pub trait MemoryStore: Send + Sync {
    async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()>;
    async fn recent_traces(&self, id: &CompanyId, limit: usize)
        -> Result<Vec<CompressedTrace>>;
    async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()>;
    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64>;
}
```

## ContextStore

The RLM environment: addressable chunks the brain queries lazily. Mirrors
Medulla's `ContextStore` port.

```rust
// src/ports/context.rs
pub trait ContextStore: Send + Sync {
    async fn put(&self, id: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr>;
    async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>>;
    async fn peek(&self, id: &CompanyId, addr: &ChunkAddr, range: Option<Range<usize>>)
        -> Result<String>;
    async fn search(&self, id: &CompanyId, query: &str, limit: usize)
        -> Result<Vec<ChunkHit>>;
}
```

## SecretStore

Per-company secrets (channel credentials, GitHub token). Company A's secrets
MUST be invisible to company B.

```rust
pub trait SecretStore: Send + Sync {
    async fn get(&self, company: &CompanyId, key: &str) -> Result<Option<SecretValue>>;
    async fn set(&self, company: &CompanyId, key: &str, value: SecretValue) -> Result<()>;
}
```

## UserStore, SessionStore, LoginCodeStore

The company's human collaborators and their credentials
(`src/ports/{users,sessions,login_codes}.rs`). Full design in
[users.md](users.md).

```rust
#[async_trait]
pub trait UserStore: Send + Sync {
    async fn list_users(&self, company: &CompanyId) -> Result<Vec<UserRecord>>;
    async fn get_user(&self, company: &CompanyId, id: &str) -> Result<Option<UserRecord>>;
    async fn find_user_by_email(&self, company: &CompanyId, email: &str)
        -> Result<Option<UserRecord>>;
    async fn upsert_user(&self, company: &CompanyId, user: &UserRecord) -> Result<()>;
    async fn delete_user(&self, company: &CompanyId, id: &str) -> Result<bool>;

    async fn list_invites(&self, company: &CompanyId) -> Result<Vec<InviteRecord>>;
    async fn find_invite_by_email(&self, company: &CompanyId, email: &str)
        -> Result<Option<InviteRecord>>;
    async fn upsert_invite(&self, company: &CompanyId, invite: &InviteRecord) -> Result<()>;
    async fn delete_invite(&self, company: &CompanyId, id: &str) -> Result<bool>;
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, company: &CompanyId, session: &SessionRecord) -> Result<()>;
    async fn find_by_token_hash(&self, company: &CompanyId, token_hash: &str)
        -> Result<Option<SessionRecord>>;
    async fn list_for_user(&self, company: &CompanyId, user_id: &str)
        -> Result<Vec<SessionRecord>>;
    async fn touch(&self, company: &CompanyId, id: &str, at_millis: u64) -> Result<()>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
    async fn delete_for_user(&self, company: &CompanyId, user_id: &str) -> Result<u64>;
    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64>;
}

#[async_trait]
pub trait LoginCodeStore: Send + Sync {
    async fn create(&self, company: &CompanyId, code: &LoginCodeRecord) -> Result<()>;
    /// Atomically redeems a code. Returns the record only if THIS call consumed
    /// it; every later call returns `None`.
    async fn consume(&self, company: &CompanyId, code_hash: &str, now_millis: u64)
        -> Result<Option<LoginCodeRecord>>;
    async fn delete_for_email(&self, company: &CompanyId, email: &str) -> Result<u64>;
    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64>;
}
```

Normative requirements beyond the usual per-company isolation:

- `email` is unique within a company, for users and invites independently.
  Lookups by email and by token hash are on request-path hot loops and MUST be
  indexed, not scanned.
- Email lookup is **exact**. Stores never normalize on the caller's behalf, so
  a caller that skips `normalize_email` misses rather than silently matching an
  address it did not ask for.
- `LoginCodeStore::consume` MUST make its check-and-mark a **single atomic
  step**. It is the only place single-use is enforced; a read-then-write in a
  handler would be a check-time/use-time gap.
- `token_hash` and `code_hash` hold hashes only. Never store, log, or return a
  plaintext secret.
