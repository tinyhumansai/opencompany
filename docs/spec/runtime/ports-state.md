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
And `overlay_agent_edits`: an operator's edits of the **manifest-declared**
teammates — their name, role, description and tool scope — read through
`CompanyRecord::effective_agent` / `effective_agents`, which the harness roster
build and every console read share, so a teammate renamed on the Team page is
the teammate that takes the next turn. Before it, a `[[agent]]` in
`company.toml` (which includes every agent from the global baseline, merged into
every company) was uneditable from the console: the answer was "edit the
blueprint and redeploy", which a hosted tenant can do neither half of. The merge
is **per field**, so a field nobody edited keeps tracking the blueprint across a
rebuild, and a stored empty description is the operator clearing it rather than
"not overridden". `company.toml` is never rewritten. At most one entry per
teammate — the write path merges through
`CompanyRecord::upsert_agent_override`.

And `overlay_retired_agents`: the tombstone half of the same layer — the ids of
manifest teammates the operator has removed. A tombstone rather than a manifest
rewrite for the reason the edits are an overlay: `company.toml` and the baseline
merged into it are re-read on every rebuild, so a teammate "deleted" by editing
the roster would simply come back. `effective_agents` filters them out and
`is_roster_agent` / `effective_desk_members` answer accordingly, so a removed
teammate is not built, not dispatchable, not seated on a desk, not a delegation
target and not the orchestrator (the role moves to the next teammate that is
actually there). `retire_agent` is idempotent — a second tombstone would change
no roster but would move the harness's overlay fingerprint and drop every live
session for a delete that had already happened. An id that names nobody is
inert, which is what makes the tombstone safe to keep across a redeploy that
drops the teammate from the blueprint too. The one refusal left is the
company's **last** teammate: an empty roster has no orchestrator, nobody to
answer a message, and no way back from the console.
And (issue #562) `overlay_policy`: the operator's `[policy]` override — the
autonomy tier and always-ask list an admin sets from the console, read through
`CompanyRecord::effective_policy`, which resolves it *ahead* of the manifest for
the same single-reconciliation-point reason as `effective_budget`. Its two
fields are independently optional, so `None` means "not overridden" while
`Some(vec![])` is a deliberately emptied always-ask list. An unknown stored
tier falls back to the manifest rather than to `supervised`, so version skew
cannot loosen a `readonly` seed. It is an overlay
rather than a manifest write **by necessity**: a rebuild re-persists
`record.manifest` from the seed and merges only `[workflows].enabled`, because
for `[tools]` / `[policy]` a record-wins merge would let a runtime grant outlive
the operator revoking it in version control. It is cleared by either of two paths: the seed's
`[policy]` **changing** across a rebuild (`carry_policy_override` — version
control wins when it speaks, and stays quiet when it doesn't, so a redeploy that
changed nothing does not silently revert the operator), or an explicit
`DELETE …/policy`. Between seed edits it is durable, and attributed, so the
console can show who moved the gate and when.
And (issue #168) `overlay_workflows`: the
workflow graph bodies authored at runtime through the console's create dialog or
the orchestrator's `create_workflow` tool, and thereafter replaced or removed
through the console's `PUT`/`DELETE …/workflows/{wid}` routes or the
orchestrator's `update_workflow` / `delete_workflow` tools (issue #661).
Both surfaces run the same company-layer core, so an agent edit snapshots the
prior body to the #274 revision ring and an agent delete cascades that ring away
exactly as an operator's does. These are persisted here rather than
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
    async fn prune(&self, id: &CompanyId, policy: &RetentionPolicy)
        -> Result<PruneReport>;
}
```

### Retention (issue #275)

`prune` is the only operation that removes a journal entry. Nothing calls it on
the append path — retention is an operator action, not a side effect of writing
— and it is built so that the irreversible direction is the hard one to reach:

| Guard | Effect |
| --- | --- |
| `RetentionPolicy::default()` | removes nothing; both bounds are `None` |
| `CompanyEvent::retention_class()` | exhaustive match, no `_` arm — a new variant fails the build until classified, and `Permanent` is the answer for anything ambiguous |
| `plan_prune` | one pure function all three backends route through, so fs / sqlite / mongodb cannot disagree about what a policy means |
| the sequence watermark | the highest-sequence entry is never removed, whatever the policy |
| default trait body | `Err(Unimplemented)` — a backend without retention refuses, rather than reporting a pass that removed nothing |

Only four kinds are `Prunable`: `WorkflowRunStarted`, `WorkflowRunFinished`,
`WorkflowNodeFinished` and `McpCallFailed`. Everything else is `Permanent`,
either because it is the audit trail (approvals, lifecycle, payments) or
because another entry addresses it *by sequence* — thread parents, reaction
targets, and the `TaskDiscussionRedacted` tombstone from issue #358 all name a
message by its `EventSeq`, so pruning a referent would dangle a pointer no fold
can repair.

**Sequences are never renumbered.** Pruning leaves gaps, which `read_from`'s
`seq >=` scan already tolerates: a cursor or SSE subscriber parked on a removed
sequence resumes at the next survivor. The watermark rule exists because
`FsEventLog` and `SqliteStore` both derive the next sequence from the highest
one present, so an emptied log would hand the next append a number a retained
cross-reference still holds.

The age bound is measured back from the newest entry in the log, not from
wall-clock now — the same anchoring
[`UsageMeter`](ports-console.md#usagemeter) uses, which keeps a pass
deterministic, testable without a clock, and unable to empty a dormant
company's journal just because time passed. Unlike `UsageMeter`, which evicts
on write against a fixed 90-day window, the journal never evicts implicitly:
it is what export/import ships and what boot replays.

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

The equivalent of Medulla's `CyclePersistence`; a hosted provider is the
target backend ([memory-engine.md](memory-engine.md)).

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
    async fn peek_many(&self, id: &CompanyId, addrs: &[ChunkAddr])
        -> Result<Vec<Option<String>>>; // defaulted: loops peek
    async fn search(&self, id: &CompanyId, query: &str, limit: usize)
        -> Result<Vec<ChunkHit>>;
    async fn delete(&self, id: &CompanyId, addr: &ChunkAddr) -> Result<bool>;
    async fn delete_label(&self, id: &CompanyId, addr: &ChunkAddr, label: &str)
        -> Result<bool>;
}
```

Chunks are content-addressed: byte-identical bodies share one address, and
every backend keeps one claim per `(addr, label)` (issue #1300 — a re-`put`
of an identical body under a new label lands that label's claim; under an
identical label it is a no-op). `delete` is address-level and takes every
claim with the body — the operator's hard-delete. `delete_label` removes one
claim and reaps the body only with the last one, decided atomically inside
the backend, which is what lets `memory_forget` and the fact-mirror reap
remove their own claim on a shared address without racing a concurrent
identical-content write.

## SecretStore

Per-company secrets (channel credentials, GitHub token). Company A's secrets
MUST be invisible to company B.

```rust
pub trait SecretStore: Send + Sync {
    async fn get(&self, company: &CompanyId, key: &str) -> Result<Option<SecretValue>>;
    async fn set(&self, company: &CompanyId, key: &str, value: SecretValue) -> Result<()>;
}
```

There is no `delete`: callers clear a secret by writing an empty value
(`src/company/mcp.rs::clear_auth`, `src/company/inference.rs::clear_key`), so an
empty value and an unset key are **different states** and a backend must keep
them apart — collapsing `""` into `None` would fall back to whatever the
manifest or the environment supplies and silently undo the operator's
revocation.

`SecretValue` is **opaque by construction** (issue #1741): it hand-writes both
`Debug` and `Serialize` to emit `[redacted]`, so a struct that embeds one and
derives either cannot leak the credential — the guard is on the type, not on
each enclosing struct. Before this, five separate hand-written `Debug` impls
guarded the containers and nothing at all guarded serialization, so
`serde_json::to_value` over a config holding a secret emitted plaintext.

`Deserialize` stays derived — reading a secret *in* never leaks one — so a
serde round-trip is deliberately asymmetric and yields `SecretValue("[redacted]")`,
which fails closed at the point of use. Persistence is unaffected because it
never used serde: every backend writes `value.expose()` and reads back through
the `SecretValue` constructor.

`expose()` is the *named* door out, not the only one: the field is `pub`, so
`let SecretValue(raw) = value` reads the plaintext without it, and about ten
production call sites already do. Audit with
`grep -E 'expose\(|SecretValue\('`, not `grep 'expose()'` — the shorter search
reads clean while missing them. Privatizing the field behind a constructor
would close the gap and is deliberately left as separate work: it touches
roughly 110 construction sites and is unrelated to the serialization guard.

Credential-bearing fields hold a `SecretValue` rather than a `String` for the
same reason (issue #1770): `SmtpCredentials::password`, `ImapCredentials::password`
and the legacy `StoredConfig::password` were each guarded on one rendering
surface and not the other — three structs, three hand-written or documented
`Debug` decisions, and a derived `Serialize` nobody considered. Holding the
credential in the guarded type is what makes `#[derive(Debug, Serialize)]` on
`MailCredentials`, `TenantMailboxConfig` or the next mail struct safe without
anyone remembering.

`assert_secret_store` in `src/store/conformance.rs` is the contract every
backend is checked against (issue #1505): read-back, absence, per-key
independence, overwrite, the empty-value distinction above, and isolation in
both directions. See
[storage.md](storage.md#conformance-coverage) for what it deliberately does not
assert yet.

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
    /// Stamps `notified_at_millis` on an invite that still exists, leaving
    /// every other field alone. Returns whether one was updated.
    async fn mark_invite_notified(&self, company: &CompanyId, id: &str, at_millis: u64)
        -> Result<bool>;
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
- `UserStore::mark_invite_notified` MUST NOT insert. It is called after an
  invite mail is sent, from a record read before the send, so a revocation that
  lands during delivery must leave it nothing to update — a backend that
  upserted here would restore an address the admin had just removed from the
  allowlist.
- `LoginCodeStore::consume` MUST make its check-and-mark a **single atomic
  step**. It is the only place single-use is enforced; a read-then-write in a
  handler would be a check-time/use-time gap.
- `token_hash` and `code_hash` hold hashes only. Never store, log, or return a
  plaintext secret.
