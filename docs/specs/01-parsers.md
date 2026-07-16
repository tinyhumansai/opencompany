# 01 — WS1: Manifest & Data Parsers

## Scope

Rust readers for the company-directory files that exist as data-only
conventions today (established in PR #4) but have no parser:

| File | Feeds |
|---|---|
| `companies/<name>/workflows/<id>.toml` | Workflows canvas (GraphQL `workflow(id)`) |
| `companies/<name>/skills/<slug>/SKILL.md` | Skills view + harness `workflows(...)` |
| `companies/<name>/workspace/**` | Workspace seeding + backlinks |
| repo-level `skills/<slug>/SKILL.md` | Shared skill registry (`skillRegistry`) |

These parsers are the format freeze for WS8 (content authoring) and the data
source for WS2a/b (GraphQL reads) and WS3 (workspace seeding, skill install).

## Design

New modules in `src/company/` (registered in `src/company/mod.rs`), following
the existing `manifest.rs`/`types.rs` conventions — serde where the format is
structured, prosumer-language validation messages, `crate::error::Result`.

### `src/company/workflow_file.rs`

```rust
pub struct WorkflowFile {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub nodes: Vec<WorkflowNodeDef>,
    pub edges: Vec<WorkflowEdgeDef>,
}
pub struct WorkflowNodeDef { pub id: String, pub kind: WorkflowNodeKind,
    pub name: String, pub summary: Option<String>, pub agent: Option<String> }
pub enum WorkflowNodeKind { Trigger, Agent, ToolCall, HttpRequest, Condition, Output }
pub struct WorkflowEdgeDef { pub from: String, pub to: String, pub label: Option<String> }

pub fn parse_workflow(toml_src: &str) -> Result<WorkflowFile>;
pub fn load_company_workflows(dir: &Path, enabled: &[String]) -> Result<Vec<WorkflowFile>>;
```

TOML shape (already shipped in
`companies/agentic_marketing_agency/workflows/campaign_pipeline.toml`):
top-level `id`/`name`/`description`, repeated `[[node]]` and `[[edge]]`.

Validation (prosumer messages, matching `manifest.rs` style):
- node ids unique; edges reference existing nodes; no self-loop edges;
- at least one `trigger` node;
- `kind` limited to the six known kinds; `agent` set only on `agent` nodes.

### `src/company/skill_file.rs`

```rust
pub struct SkillDoc {
    pub slug: String,          // directory name
    pub name: String,          // frontmatter
    pub description: String,   // frontmatter
    pub category: Option<String>,
    pub body: String,          // markdown after frontmatter, verbatim
}
pub fn parse_skill_md(slug: &str, src: &str) -> Result<SkillDoc>;
pub fn load_dir_skills(dir: &Path) -> Result<Vec<SkillDoc>>;   // skills/*/SKILL.md
```

Frontmatter is `key: value` lines between `---` fences. **Hand-parse the known
keys** (`name`, `description`, optional `category`) — no serde_yaml dependency.
The body is preserved verbatim so WS4 can feed it to
`openhuman::skills::ops_parse` unchanged.

### `src/company/workspace_seed.rs`

```rust
pub struct SeedNode { pub rel_path: PathBuf, pub kind: NodeKind, pub content: Option<String> }
pub fn walk_workspace(dir: &Path) -> Result<Vec<SeedNode>>;  // folders + markdown only
pub fn extract_wikilinks(md: &str) -> Vec<String>;           // [[target]] and [[target|alias]]
```

- Deterministic ordering (sorted walk) so seeding is reproducible.
- Skips non-markdown files; rejects paths escaping the root (`../`, absolute).
- `extract_wikilinks` powers the `WorkspaceFile.backlinks` resolver (WS2) —
  keep it a pure function here so both seeding and reads share it.

### Registry loader

`load_dir_skills` doubles for the repo-level `skills/` library. `AppState`
caches the parsed registry (invalidation not needed — repo content is
immutable at runtime).

## Subtasks (commit-sized)

1. `feat(company): parse workflow TOML into a validated node/edge graph`
2. `feat(company): parse SKILL.md frontmatter and body`
3. `feat(company): walk workspace/ into a seed tree with wikilink extraction`
4. `feat(company): shared skills registry loader + AppState cache`
5. `test(company): content-validation walk over companies/* and skills/*`

Subtasks 1–4 are independent (one subagent each is fine); 5 lands last.

## Dependencies

None. Unblocks WS2a/b, WS3 (seeding, skill install), WS8 (format freeze).

## Tests & exit criteria

Unit tests per [09-verification.md §1](09-verification.md): parser happy
paths, validation failures, round-trips against the real
`agentic_marketing_agency` files and both repo skills, wikilink alias
handling, path-traversal rejection. Exit: content-validation walk green over
every `companies/*` directory; `cargo test` green.
