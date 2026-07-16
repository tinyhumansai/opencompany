# 02 — WS2: The GraphQL Read Plane

## Scope

Expand `src/server/graphql.rs` (today: query-only `companies` / `company(id)`
/ `approvals`) into the complete read surface for every console view. Reads
land in GraphQL first; the existing REST reads (`GET …/team`, `…/chats`,
`…/connections`, status, approvals) stay as **compat** until the console is
GraphQL-only — no *new* REST reads.

## Design

### Module split

`src/server/graphql.rs` becomes a directory module:

```
src/server/graphql/
  mod.rs          # router(), schema build (once, at startup), GraphiQL
  auth.rs         # GqlAuth context, authorize(), visible_companies()
  pagination.rs   # Page<T> (offset/limit), PageArgs
  company.rs      # Company root object, Approval, TeamMember, Chat/Message
  workspace.rs    # FsNode, WorkspaceFile (+backlinks)
  tasks.rs        # Task
  skills.rs       # Skill, RegistrySkill
  memory_facts.rs # MemoryFact
  workflows.rs    # WorkflowSummary, Workflow, nodes/edges
  usage.rs        # Usage series/totals
  finances.rs     # Finances, Transaction
  inbox.rs        # Inbox, EmailMessage
```

Build the `Schema` **once at startup** and store it in `AppState`; inject
per-request auth via `req.data(GqlAuth)`. (Today the schema is rebuilt per
request — fix that in the foundation commit.)

### SDL sketch

```graphql
type Query {
  companies: [Company!]!            # platform: all visible; operator: local
  company(id: ID): Company          # id optional in single-company mode (registry.sole())
  skillRegistry: [RegistrySkill!]!  # repo-level skills/ library (unscoped)
}

type Company {
  id: ID!  name: String!  lifecycle: String!  pendingApprovals: Int!
  approvals: [Approval!]!
  team: [TeamMember!]!
  chats: [Chat!]!  chat(id: ID!): Chat
  inboxes: [Inbox!]!
  tasks(column: String, first: Int = 100, offset: Int = 0): TaskPage!
  skills: [Skill!]!
  workspaceTree: [FsNode!]!  workspaceFile(id: ID!): WorkspaceFile
  memory(query: String, kind: MemoryKind, first: Int = 50, offset: Int = 0): MemoryFactPage!
  workflows: [WorkflowSummary!]!  workflow(id: ID!): Workflow
  usage(range: UsageRange! = D30): Usage!
  finances: Finances!
  connections: [ConnectionState!]!
  domain: DomainStatus
  smtp: SmtpStatus                  # host/port/username only — never the password
}

type TeamMember { id: ID! name: String role: String! description: String inboxEnabled: Boolean! }
type Chat { id: ID! name: String! description: String members: [ID!]!
            history(first: Int = 50, before: String): MessagePage! }
type Message { id: ID! channel: String! author: String! text: String! atMillis: Float! mine: Boolean! }
type Inbox { key: ID! name: String! address: String! unread: Int!
             messages(first: Int = 50, offset: Int = 0): EmailPage! }
type Task { id: ID! title: String! note: String column: String! priority: String! assignee: String! }
type Skill { id: ID! name: String! description: String! category: String!
             source: String!  # "built-in" | "library" | "custom"
             enabled: Boolean! }
type FsNode { id: ID! name: String! kind: String! parentId: ID updatedAt: String! }
type WorkspaceFile { id: ID! name: String! content: String! updatedAt: String! backlinks: [FsNode!]! }
enum MemoryKind { FACT PREFERENCE PERSON PROJECT REFERENCE }
type MemoryFact { id: ID! kind: MemoryKind! title: String! body: String! source: String! updatedAt: String! }
type WorkflowSummary { id: ID! name: String! enabled: Boolean! }
type Workflow { id: ID! name: String! nodes: [WorkflowNode!]! edges: [WorkflowEdge!]! }
type WorkflowNode { id: ID! kind: String! name: String! summary: String }
type WorkflowEdge { from: ID! to: ID! label: String }
enum UsageRange { D7 D30 D90 }
type Usage { series: [UsagePoint!]! byAgent: [AgentTokens!]! byProvider: [ProviderCalls!]!
             totals: UsageTotals! }
type Finances { balanceUsd: Float! budgetUsd: Float! spentUsd: Float! revenueUsd: Float!
                netUsd: Float! byCategory: [CategorySpend!]!
                transactions(first: Int = 50, offset: Int = 0): [Transaction!]! }
type ConnectionState { provider: String! connected: Boolean! account: String reason: String }
type DomainStatus { domain: String! verified: Boolean! records: [DnsRecord!]! }
type SmtpStatus { host: String! port: Int! username: String! configured: Boolean! }
```

Page wrappers (`TaskPage`, `MemoryFactPage`, `EmailPage`, `MessagePage`) are a
generic `Page<T> { items, total }` in `pagination.rs`. **Offset/limit, not
Relay cursors** — the console renders full lists. Exception: `Chat.history`
takes an opaque `before` cursor (EventLog position) because the log is
append-only and long.

### Key decisions

- **`Company` is the aggregation root.** Every per-company read hangs off it;
  the only top-level fields are `companies`, `company`, `skillRegistry`.
- **`Company` is a handle, not a `SimpleObject`**: `CompanyGql { id,
  runtime: Arc<CompanyRuntime> }` with `#[Object]` async field resolvers, each
  awaiting the relevant port/parser — no eager loading.
- **Timestamps**: `atMillis: Float` where the console model uses epoch millis;
  ISO-8601 `String` where it uses `at`/`updatedAt` strings. Match
  `frontend/src/lib/*` exactly.
- **Not-wired = absent.** Fields for unshipped surfaces are added to the
  schema only when their workstream lands; resolver-level unavailability
  returns a GraphQL error with `extensions.code = "NOT_WIRED"`, which the
  console's bare-catch treats like a 404.

### Auth (`auth.rs`)

```rust
pub enum GqlAuth { Dev, Operator, Platform(PlatformClaims) }
impl GqlAuth {
    pub fn authorize(&self, company: &CompanyId) -> async_graphql::Result<()>;
    pub fn visible_companies(&self, registry: &CompanyRegistry) -> Vec<CompanyId>;
}
```

Refactor the claims resolution out of `platform_auth.rs` into a shared
`fn resolve_claims(headers) -> Result<GqlAuth>` used by both the REST
extractors and `graphql_handler`. `Query::company`/`companies` call
`authorize`/`visible_companies`; nested fields are safe because they're only
reachable through an authorized `Company`.

### Data sources per field

| Field | Source |
|---|---|
| team, chats, connections (state intent), workflows summaries | manifest (`CompanyManifest`) |
| workflow(id) | WS1 `workflow_file.rs` |
| skills, skillRegistry | WS1 `skill_file.rs` ∪ WS3 `SkillStateStore` |
| workspaceTree/File, tasks, memory, inboxes | WS3 ports |
| chat history | `EventLog` (`OperatorMessage` + new `AgentReply`) |
| usage | WS5 `UsageMeter` |
| finances | ledger (`CompanyStore`) + `[budget]` + economy (feature `tinyplace`) |
| domain/smtp status | `SecretStore` reserved keys (non-secret projection) |
| connections connected/account | `SecretStore` oauth entries (WS6) |

### Frontend consumption

No codegen, no Apollo. One method on the existing client
(`frontend/src/api/client.ts`):

```ts
async gql<T>(query: string, variables?: Record<string, unknown>): Promise<T>
```

POSTs to `{baseUrl}/graphql` with the same bearer header; throws `ApiError`
from `errors[0]` (`extensions.code`). Query constants live next to the views
(`src/views/queries.ts`); response types already exist in `src/lib/*`.
graphql-code-generator is a later nicety, not a dependency.

## Subtasks (commit-sized)

1. `refactor(graphql): module split, schema-at-startup, GqlAuth context,
   Page<T>` — port the three existing reads onto the `Company` handle.
2. `feat(graphql): manifest-derived reads` — team, chats, connections,
   workflow summaries (**WS2a**).
3. `feat(graphql): workflow(id) + skills + skillRegistry` (**WS2b**, needs WS1).
4. `feat(graphql): workspace tree/file with backlinks` (**WS2b**, needs WS3
   workspace port for live data; can land against seed-only reads first).
5. `feat(graphql): tasks + memory + inboxes + chat history` (**WS2c**, needs
   WS3 ports + WS4 `AgentReply` for history).
6. `feat(graphql): usage + finances` (**WS2d**, needs WS5).
7. `test(graphql): SDL snapshot` — updated in every schema commit thereafter.

## Dependencies

WS1 for 2–4; WS3 for 4–5; WS4 (AgentReply event) for history; WS5 for 6.
Subtask 1 depends on nothing — land it first; it also unblocks WS7's `gql()`
helper.

## Tests & exit criteria

Four-case suite per query + SDL snapshot per
[09-verification.md §2](09-verification.md). Exit: every field above resolves
against `companies/agentic_marketing_agency` in the e2e harness; GraphiQL
spot-check documented in the PR.
