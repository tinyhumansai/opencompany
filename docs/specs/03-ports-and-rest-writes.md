# 03 — WS3: New Ports/Stores + the REST Write Plane

## Scope

Durable state and mutations for the console surfaces that are client-only
today: tasks, memory facts, workspace files, skill install state, inboxes,
usage samples — six new port traits with fs/sqlite/mongodb backends — plus a
new `src/server/ops/` REST router family carrying every write.

## Design — ports

All traits `#[async_trait]`, `Send + Sync`, used as `Arc<dyn …>`, keyed by
`&CompanyId`, one file per port in `src/ports/` (re-exported in `mod.rs`):

```rust
// ports/tasks.rs
pub struct TaskRecord { pub id: String, pub title: String, pub note: Option<String>,
    pub column: String, pub priority: String, pub assignee: String, pub updated_at_millis: u64 }
pub trait TaskStore {
    async fn list(&self, company: &CompanyId) -> Result<Vec<TaskRecord>>;
    async fn upsert(&self, company: &CompanyId, task: &TaskRecord) -> Result<()>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}

// ports/workspace.rs — node ids are stable ULIDs, not paths (rename-safe)
pub struct WorkspaceNode { pub id: String, pub name: String, pub kind: NodeKind,
    pub parent_id: Option<String>, pub updated_at_millis: u64 }
pub trait WorkspaceStore {
    async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>>;
    async fn read(&self, company: &CompanyId, id: &str) -> Result<Option<(WorkspaceNode, String)>>;
    async fn write(&self, company: &CompanyId, id: &str, content: &str) -> Result<WorkspaceNode>;
    async fn create(&self, company: &CompanyId, node: &WorkspaceNode, content: Option<&str>) -> Result<()>;
    async fn rename_move(&self, company: &CompanyId, id: &str,
        name: Option<&str>, parent: Option<&str>) -> Result<WorkspaceNode>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;  // folders recursive
    async fn is_empty(&self, company: &CompanyId) -> Result<bool>;          // gates seeding
}

// ports/facts.rs — the console's durable Memory view. Distinct from the existing
// MemoryStore (traces/task_results = runtime working memory).
pub struct FactRecord { pub id: String, pub kind: FactKind, pub title: String,
    pub body: String, pub source: String, pub updated_at_millis: u64 }
pub trait FactStore {
    async fn list(&self, company: &CompanyId, query: Option<&str>, kind: Option<FactKind>)
        -> Result<Vec<FactRecord>>;
    async fn upsert(&self, company: &CompanyId, fact: &FactRecord) -> Result<()>;
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}

// ports/inbox.rs
pub struct InboxMeta { pub key: String, pub name: String, pub address: String, pub enabled: bool }
pub struct EmailRecord { pub id: String, pub inbox: String, pub from_name: String,
    pub from_email: String, pub subject: String, pub body: String,
    pub at_millis: u64, pub read: bool, pub outbound: bool }
pub trait InboxStore {
    async fn inboxes(&self, company: &CompanyId) -> Result<Vec<InboxMeta>>;
    async fn set_enabled(&self, company: &CompanyId, key: &str, meta: &InboxMeta) -> Result<()>;
    async fn messages(&self, company: &CompanyId, key: &str, limit: usize, offset: usize)
        -> Result<Vec<EmailRecord>>;
    async fn append(&self, company: &CompanyId, msg: &EmailRecord) -> Result<()>;
    async fn mark_read(&self, company: &CompanyId, key: &str, ids: Option<&[String]>) -> Result<u64>;
}

// ports/usage.rs — written by the WS4 cost hook, read by WS5
pub struct UsageSample { pub at_millis: u64, pub agent: String, pub provider: String,
    pub input_tokens: u64, pub output_tokens: u64, pub cached_input_tokens: u64,
    pub cost_usd: f64, pub kind: SampleKind /* Inference | OauthCall */ }
pub trait UsageMeter {
    async fn record(&self, company: &CompanyId, sample: &UsageSample) -> Result<()>;
    async fn query(&self, company: &CompanyId, since_millis: u64) -> Result<Vec<UsageSample>>;
}

// ports/skills_state.rs — deltas only; built-in content stays on disk
pub struct SkillState { pub slug: String, pub enabled: bool, pub source: SkillSource,
    pub custom_doc: Option<String> /* full SKILL.md for custom skills */ }
pub trait SkillStateStore {
    async fn list(&self, company: &CompanyId) -> Result<Vec<SkillState>>;
    async fn set(&self, company: &CompanyId, state: &SkillState) -> Result<()>;
    async fn remove(&self, company: &CompanyId, slug: &str) -> Result<bool>;
}
```

**Deliberately not ports:** domain + SMTP config are JSON blobs under
`SecretStore` reserved keys (`__domain`, `__smtp`); finances resolve from the
existing ledger + `[budget]` + economy.

`FactStore` deliberately sits **beside** the two memory ports specified in
[`docs/spec/company-brain/memory.md`](../spec/company-brain/memory.md)
(`MemoryStore` = traces/task results, `ContextStore` = the RLM environment):
facts are the Operator's durable, hand-curated view, not cycle working
memory. The Operator-rights section of that spec applies — deletes propagate
to the backing store and the deletion is journaled to the `EventLog`.

### Backends & wiring

`StorageHandles` (`src/store/select.rs`) grows `tasks`, `workspace`, `facts`,
`inbox`, `usage`, `skills` fields; `open_storage` wires each backend:

- **fs** — JSON/JSONL under the company bundle: `tasks.json`, `facts.jsonl`,
  `inbox/<key>.jsonl`, `usage.jsonl`, `skills.json`; workspace as real files
  plus `.workspace-index.json` (ULID → path).
- **sqlite** — one table per port on the shared bundled connection
  (`tasks`, `facts`, `inbox_messages`, `usage_samples`, `skill_state`,
  `workspace_nodes` with a content column).
- **mongodb** — one collection per port, every doc keyed by `company_id`.

Each port gets a conformance function in `src/store/conformance.rs`
(`task_store_conformance(&dyn TaskStore)`, …) invoked from all three backends'
test mods — the existing pattern.

### Seeding (idempotent, in `RuntimeBuilder::build`)

- **Workspace**: if `is_empty`, walk `companies/<name>/workspace/**` (WS1
  walker) and create the tree. Never re-seed — operator deletions stick.
- **Skills**: effective set = company-dir skills (`source: "built-in"`,
  enabled by default) ∪ `SkillStateStore` rows (library installs, custom
  skills, disable overrides). The store holds deltas only.

## Design — REST write plane (`src/server/ops/`)

One file per domain, merged via `ops::router()` in `routes.rs`. A `scoped()`
helper registers each route under **both** addressing forms, and a
`ScopedCompany` extractor resolves `Arc<CompanyRuntime>` + authorization
(`PlatformOrOperatorAuth` + `authorize_address` on `/companies/{id}`;
`OperatorAuth` + `registry.sole()` on `/company`):

```rust
pub(crate) fn scoped(path: &str, mr: MethodRouter<AppState>) -> Router<AppState>;
// handlers: async fn(company: ScopedCompany, Json<Body>) -> Result<Json<Resp>, ApiError>
```

Errors reuse `server/error.rs`: the `{error, code}` envelope with stable
codes and status semantics already normative in
[`docs/spec/runtime/api.md`](../spec/runtime/api.md) (4xx caller mistakes,
409 lifecycle conflicts). Strings follow the glossary via a shared
`ops/language.rs` const table. Bodies mirror `frontend/src/api/types.ts` /
`src/lib/*` (camelCase serde). Chat stays request/response JSON for now; the
SSE streaming form in `runtime/api.md` is a compatible later upgrade.

### Route table (under both `…/companies/{id}` and `…/company`)

| Method | Path | Body → Response |
|---|---|---|
| POST | `/tasks` | `{title, note?, column?, priority?, assignee?}` → `TaskCard` |
| PATCH | `/tasks/{taskId}` | partial `TaskCard` → `TaskCard` (drag = `{column}`) |
| DELETE | `/tasks/{taskId}` | → 204 |
| POST | `/memory` | `{kind, title, body, source?}` → `MemoryEntry` |
| DELETE | `/memory/{factId}` | → 204 |
| POST | `/workspace` | `{name, kind, parentId?, content?}` → `FsNode` |
| PUT | `/workspace/file/{nodeId}` | `{content}` → `{updatedAt}` |
| PATCH | `/workspace/{nodeId}` | `{name?, parentId?}` → `FsNode` (reject cycles) |
| DELETE | `/workspace/{nodeId}` | → 204 (folders recursive) |
| POST | `/skills/{slug}/install` | → `InstalledSkill` (from registry) |
| POST | `/skills/{slug}/uninstall` | → 204 (built-in → 409) |
| PUT | `/skills/{slug}` | `{enabled}` → `InstalledSkill` |
| POST | `/skills` | `{name, description, category?, body?}` → `InstalledSkill` (custom) |
| POST | `/team` | `{name, role, description?}` → `TeamMemberDto` (operator overlay) |
| DELETE | `/team/{agentId}` | → 204 (manifest agents → 409) |
| PUT | `/team/{agentId}/inbox` | `{enabled}` → `{key, address}` |
| POST | `/chat` | `{message, chat?}` → `{responses:[{channel,text}]}` (extends operator chat with a desk id) |
| POST | `/inboxes/{key}/read` | `{ids?}` → `{unread}` (omit ids = all) |

Connections/domain/SMTP writes are specified in
[06-connections-domain-email.md](06-connections-domain-email.md); approvals,
feedback, lifecycle, provision already exist and are untouched.

**Team writes** use the operator-overlay model (see README open question 1):
overlay agents persist via `CompanyStore` (field on `CompanyRecord`) and merge
into the roster at read/build time; the version-controlled `company.toml` is
never rewritten. Overlay agents are roster-only in v1.

## Subtasks (commit-sized; one subagent per surface)

Per surface — tasks, memory, workspace, skills, inbox — two commits each:

1. `feat(<surface>): port trait + fs/sqlite/mongodb impls + conformance`
2. `feat(server): <surface> REST routes + tests`

Plus, first: `feat(server): ops router scaffolding (scoped(), ScopedCompany,
language table)`; and `feat(runtime): workspace/skills seeding in
RuntimeBuilder` after the workspace/skills ports land. The `UsageMeter` port
ships here (trait + backends) but is written to by WS4 and read by WS5.

Coordination: registration lines in `ports/mod.rs` / `store/*` /
`routes.rs` land serially; everything else parallel.

## Dependencies

WS1 (workspace walker, skill loader for seeding/install). Unblocks WS2c, WS5
(UsageMeter), WS7. Inbox delivery becomes real only with WS6.

## Tests & exit criteria

Conformance suite per port on all backends; four-case REST suites plus
per-surface cases (restart persistence, traversal/cycle rejection, built-in
uninstall 409) per [09-verification.md](09-verification.md). Exit: e2e
restart-persistence green; console views wire up in WS7 without contract
changes.
