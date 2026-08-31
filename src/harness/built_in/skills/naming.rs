//! Skill read tools, named after skills (issue #845).
//!
//! # The collision
//!
//! Upstream OpenHuman calls a *skill* a "workflow". Its three skill read tools
//! are literally named `list_workflows`, `describe_workflow` and
//! `read_workflow_resource`, they take a `workflow_id`, and they answer with a
//! JSON payload keyed `workflows`.
//!
//! OpenCompany has its own, entirely unrelated **workflow registry** — the saved
//! graphs under `companies/<name>/workflows/`, rendered by the console's
//! Workflows page, enumerated by [`list_workflows_union`](crate::company::list_workflows_union),
//! and executed by the orchestrator's
//! [`run_workflow`](crate::harness::orchestrator::RUN_WORKFLOW_TOOL).
//!
//! Wiring upstream's tools in unrenamed put both concepts on one belt under one
//! word, and the agent-facing half of that word pointed at the wrong registry:
//!
//! * `list_workflows` enumerated the four installed **skills**, so an agent
//!   asked what workflows the company had answered with the contents of
//!   `Settings → Skills` — a set with *zero* overlap with the Workflows page.
//! * `run_workflow` runs a **workflow**. So an agent that listed its
//!   "workflows", picked one and ran it was crossing registries mid-turn, and an
//!   agent asked to run a real workflow (`Campaign pipeline`) reported that it
//!   did not exist — having looked in the skill tree.
//!
//! Nothing errored. The shared word is the whole of why it stayed silent.
//!
//! # What this module does
//!
//! [`skill_read_tools`] wraps each upstream tool in a [`SkillTool`] that renames
//! it and everything the agent can see through it:
//!
//! | Upstream | Here |
//! | --- | --- |
//! | `list_workflows` | [`LIST_SKILLS_TOOL`] |
//! | `describe_workflow` | [`DESCRIBE_SKILL_TOOL`] |
//! | `read_workflow_resource` | [`READ_SKILL_RESOURCE_TOOL`] |
//!
//! The rename is not cosmetic and is not only the tool name. An agent reads four
//! surfaces, and leaving any of them saying "workflow" leaves the bug:
//!
//! 1. **the name** — what it calls;
//! 2. **the description** — written here rather than delegated, because
//!    upstream's tells the model to go and `run_workflow` what it just listed,
//!    which is the cross-registry step itself;
//! 3. **the argument** — `workflow_id` is exposed as `skill_id` and mapped back
//!    before the inner tool runs, so the schema an agent copies from never says
//!    "workflow";
//! 4. **the result** — the payload keys `workflows` / `workflow_id` are renamed
//!    on the way out, and upstream's error prose is rewritten. Without this the
//!    agent still reads `{"count":4,"workflows":[…]}` and still reports "4
//!    installed workflows", which is the sentence in the issue.
//!
//! Renaming happens **at this boundary**, not in `vendor/openhuman`: "workflow"
//! is upstream's own settled word for a skill, and its console, docs and other
//! hosts use it consistently. It is only wrong inside OpenCompany, because only
//! OpenCompany also has workflows.
//!
//! # This grants nothing
//!
//! All three tools stay exactly as read-only as they were — same inner tools,
//! same workspace scoping, same permission level, arguments only re-keyed. No
//! agent gains any reach over the workflow registry here; an agent that could
//! not author, edit or run a company workflow before still cannot. What changes
//! is that it now says so instead of answering from the wrong list. The
//! `list_skills` description states the separation outright so a model that
//! finds no workflow tools does not fall back to the skill list as a substitute.
//!
//! ## Keep the names in the consequence catalogue
//!
//! [`crate::policy::consequence`]'s `DECLARED` table names all three. It must
//! follow a rename: an unlisted tool falls through to `undeclared`, whose
//! read-only prefix list has `list` and `read` but **not** `describe` — so
//! `describe_skill` would park and interrupt an operator for a local read, which
//! is precisely the regression that module's own comment records
//! `describe_workflow` having caused before the table existed.

use async_trait::async_trait;
use serde_json::Value;

use oh::tools::traits::{PermissionLevel, Tool, ToolCallOptions, ToolResult};
use openhuman_core::openhuman as oh;

/// Tool name: enumerate the skills installed for this agent.
pub const LIST_SKILLS_TOOL: &str = "list_skills";
/// Tool name: describe one installed skill and the inputs it declares.
pub const DESCRIBE_SKILL_TOOL: &str = "describe_skill";
/// Tool name: read one file bundled inside an installed skill.
pub const READ_SKILL_RESOURCE_TOOL: &str = "read_skill_resource";

/// The argument key upstream's tools read a skill's directory name from.
const UPSTREAM_ID_ARG: &str = "workflow_id";
/// The argument key this module exposes in its place.
const SKILL_ID_ARG: &str = "skill_id";

/// Top-level result keys renamed on the way out, upstream → here.
///
/// Top-level only, and by exact key: a skill's own `description`, and the
/// `content` of a file read through `read_skill_resource`, are the user's text
/// and are never rewritten. A skill that documents a workflow keeps saying so.
const RESULT_KEY_RENAMES: &[(&str, &str)] =
    &[("workflows", "skills"), (UPSTREAM_ID_ARG, "skill_id")];

/// Upstream tool names rewritten in agent-facing prose, so an error naming the
/// tool names the one the agent actually called.
const PROSE_TOOL_RENAMES: &[(&str, &str)] = &[
    ("list_workflows", LIST_SKILLS_TOOL),
    ("describe_workflow", DESCRIBE_SKILL_TOOL),
    ("read_workflow_resource", READ_SKILL_RESOURCE_TOOL),
    (UPSTREAM_ID_ARG, SKILL_ID_ARG),
];

/// Bare words rewritten in agent-facing prose, after [`PROSE_TOOL_RENAMES`].
///
/// Plural first only for readability — [`replace_whole_words`] cannot match
/// `workflow` inside `workflows` anyway, since `s` is a token character.
const PROSE_WORD_RENAMES: &[(&str, &str)] = &[
    ("workflows", "skills"),
    ("workflow", "skill"),
    ("Workflows", "Skills"),
    ("Workflow", "Skill"),
];

/// The three skill read tools, renamed for a host that also has workflows.
///
/// `inner` is upstream's tool, already scoped to the agent's materialized skill
/// tree — this only renames what the agent sees.
pub(super) fn skill_read_tools(
    list: Box<dyn Tool>,
    describe: Box<dyn Tool>,
    read_resource: Box<dyn Tool>,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(SkillTool {
            inner: list,
            name: LIST_SKILLS_TOOL,
            description: "List the skills installed for this agent — packaged, reusable procedures \
                 (a goal plus the steps to reach it). Returns each skill's name, dir, \
                 description, tags, tool hints, scope, and any warnings. Use `describe_skill` \
                 to inspect one before following it. Skills are NOT this company's saved \
                 workflows: a workflow is a stored graph on the Workflows page, and it is not \
                 listed here — never answer a question about the company's workflows from this \
                 tool.",
            renames_id_arg: false,
        }),
        Box::new(SkillTool {
            inner: describe,
            name: DESCRIBE_SKILL_TOOL,
            description: "Describe one installed skill by `skill_id` (its directory name, as returned \
                 by `list_skills`): the skill's definition (id, display name, when to use it) \
                 and the inputs it declares (name, description, required, type). Read a skill \
                 before following it. This describes a skill, never one of the company's saved \
                 workflows.",
            renames_id_arg: true,
        }),
        Box::new(SkillTool {
            inner: read_resource,
            name: READ_SKILL_RESOURCE_TOOL,
            description: "Read a file bundled inside an installed skill (`skill_id` + `relative_path` \
                 under that skill's directory, e.g. `scripts/run.sh` or `references/spec.md`). \
                 Path-hardened and size-capped. Use to read a skill's helper scripts or \
                 reference docs.",
            renames_id_arg: true,
        }),
    ]
}

/// One upstream skill read tool, presented under its skill name.
///
/// Every method either overrides with a skill-named value or forwards to
/// `inner`, so the wrapper cannot silently drop a behaviour the inner tool
/// declares (its concurrency safety, its permission level, its timeout policy).
/// The `spec` / `display_label` defaults derive from [`Self::name`] and
/// [`Self::description`], so they follow the rename without an override.
struct SkillTool {
    inner: Box<dyn Tool>,
    name: &'static str,
    description: &'static str,
    /// Whether this tool takes upstream's `workflow_id`. False for the
    /// argument-less list tool.
    renames_id_arg: bool,
}

impl SkillTool {
    /// Rewrites `skill_id` back to the `workflow_id` the inner tool reads.
    ///
    /// Only ever *adds* the upstream key from ours; a call that already used
    /// `workflow_id` (a model working from a stale schema) is left alone rather
    /// than refused, so the rename cannot break a turn mid-conversation.
    fn to_inner_args(&self, mut args: Value) -> Value {
        if !self.renames_id_arg {
            return args;
        }
        let Some(map) = args.as_object_mut() else {
            return args;
        };
        if let Some(id) = map.remove(SKILL_ID_ARG) {
            map.entry(UPSTREAM_ID_ARG).or_insert(id);
        }
        args
    }

    /// Rewrites one execution's outcome: result payloads and error prose.
    fn rewrite_outcome(&self, out: anyhow::Result<ToolResult>) -> anyhow::Result<ToolResult> {
        match out {
            Ok(result) => Ok(rewrite_result(result)),
            // The inner tools return their "not found" / bad-argument cases as
            // `Err`, and that string reaches the agent too.
            Err(err) => Err(anyhow::anyhow!(rewrite_prose(&err.to_string()))),
        }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn parameters_schema(&self) -> Value {
        rename_schema_id_arg(self.inner.parameters_schema(), self.renames_id_arg)
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        self.rewrite_outcome(self.inner.execute(self.to_inner_args(args)).await)
    }

    async fn execute_with_options(
        &self,
        args: Value,
        options: ToolCallOptions,
    ) -> anyhow::Result<ToolResult> {
        self.rewrite_outcome(
            self.inner
                .execute_with_options(self.to_inner_args(args), options)
                .await,
        )
    }

    // `Option<&dyn ToolRunContext>`, not the concrete TinyAgents
    // `ToolExecutionContext` this used to name. The tinytools extraction turned
    // the context into a trait so a shared tool vocabulary need not depend on
    // tinyagents (that would be a dependency cycle — tinyagents depends on
    // tinytools). The trait exposes the workspace, the thread id and the output
    // budget and nothing else; the run id, event sink and cancellation token
    // stay harness-internal on purpose.
    //
    // What matters here is unchanged and is the reason this method is
    // overridden at all: the context carries the per-worker worktree the
    // vendored tool uses as its action dir, so it is forwarded whole. Dropping
    // it would silently move where commands run.
    async fn execute_with_context(
        &self,
        args: Value,
        options: ToolCallOptions,
        context: Option<&dyn oh::tools::traits::ToolRunContext>,
    ) -> anyhow::Result<ToolResult> {
        self.rewrite_outcome(
            self.inner
                .execute_with_context(self.to_inner_args(args), options, context)
                .await,
        )
    }

    fn supports_markdown(&self) -> bool {
        self.inner.supports_markdown()
    }

    fn permission_level(&self) -> PermissionLevel {
        self.inner.permission_level()
    }

    fn permission_level_with_args(&self, args: &Value) -> PermissionLevel {
        self.inner
            .permission_level_with_args(&self.to_inner_args(args.clone()))
    }

    fn scope(&self) -> oh::tools::traits::ToolScope {
        self.inner.scope()
    }

    fn category(&self) -> oh::tools::traits::ToolCategory {
        self.inner.category()
    }

    fn is_concurrency_safe(&self, args: &Value) -> bool {
        self.inner
            .is_concurrency_safe(&self.to_inner_args(args.clone()))
    }

    fn external_effect(&self) -> bool {
        self.inner.external_effect()
    }

    fn external_effect_with_args(&self, args: &Value) -> bool {
        self.inner
            .external_effect_with_args(&self.to_inner_args(args.clone()))
    }

    fn max_result_size_chars(&self) -> Option<usize> {
        self.inner.max_result_size_chars()
    }

    fn timeout_policy(&self, args: &Value) -> oh::tools::traits::ToolTimeout {
        self.inner.timeout_policy(&self.to_inner_args(args.clone()))
    }
}

/// Re-keys `workflow_id` to `skill_id` in a parameter schema.
///
/// Structural — the `properties` key and the `required` entry — rather than a
/// string replace over the serialized schema, so a *description* inside the
/// schema that legitimately says "workflow" is not mangled. The description text
/// upstream attaches to the id property is replaced wholesale, since it reads
/// "Workflow id (directory name)."
fn rename_schema_id_arg(mut schema: Value, renames: bool) -> Value {
    if !renames {
        return schema;
    }
    let Some(map) = schema.as_object_mut() else {
        return schema;
    };
    if let Some(props) = map.get_mut("properties").and_then(Value::as_object_mut)
        && let Some(mut prop) = props.remove(UPSTREAM_ID_ARG)
    {
        if let Some(prop_map) = prop.as_object_mut() {
            prop_map.insert(
                "description".to_string(),
                Value::String("Skill id (its directory name).".to_string()),
            );
        }
        props.insert(SKILL_ID_ARG.to_string(), prop);
    }
    if let Some(required) = map.get_mut("required").and_then(Value::as_array_mut) {
        for entry in required.iter_mut() {
            if entry.as_str() == Some(UPSTREAM_ID_ARG) {
                *entry = Value::String(SKILL_ID_ARG.to_string());
            }
        }
    }
    schema
}

/// Rewrites every agent-visible block of one result.
///
/// A `Json` block is re-keyed structurally. A `Text` block is re-keyed the same
/// way when it parses as a JSON object — which is how all three tools return
/// success — and prose-scrubbed when it does not, which is how they return the
/// allowlist refusal.
fn rewrite_result(mut result: ToolResult) -> ToolResult {
    use oh::skills::types::ToolContent;
    for block in &mut result.content {
        match block {
            ToolContent::Json { data } => rename_top_level_keys(data),
            ToolContent::Text { text } => match serde_json::from_str::<Value>(text) {
                Ok(mut parsed) if parsed.is_object() => {
                    rename_top_level_keys(&mut parsed);
                    if let Ok(re) = serde_json::to_string(&parsed) {
                        *text = re;
                    }
                }
                _ => *text = rewrite_prose(text),
            },
        }
    }
    if let Some(md) = result.markdown_formatted.as_mut() {
        *md = rewrite_prose(md);
    }
    result
}

/// Applies [`RESULT_KEY_RENAMES`] to a JSON object's own keys.
///
/// Top level only, deliberately: below it lies skill-authored content, and a
/// rename there would edit what a person wrote.
fn rename_top_level_keys(value: &mut Value) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    for (from, to) in RESULT_KEY_RENAMES {
        if let Some(v) = map.remove(*from) {
            map.entry(*to).or_insert(v);
        }
    }
}

/// Rewrites upstream's tool names and its bare "workflow"s in agent-facing prose.
fn rewrite_prose(text: &str) -> String {
    let mut out = text.to_string();
    for (from, to) in PROSE_TOOL_RENAMES.iter().chain(PROSE_WORD_RENAMES) {
        out = replace_whole_words(&out, from, to);
    }
    out
}

/// Replaces `needle` only where it stands as a whole token.
///
/// A token character is alphanumeric, `_` or `-`, so a skill whose own slug is
/// `content-workflow` survives an error message about it intact — `\b` would
/// not, since it treats `-` as a boundary. Run before the bare-word passes, the
/// same rule is what keeps `describe_workflow` from being half-rewritten into
/// `describe_skill` twice over: the `workflow` inside it is not a whole token.
fn replace_whole_words(haystack: &str, needle: &str, replacement: &str) -> String {
    let is_token_char = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let mut out = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(at) = rest.find(needle) {
        let before_ok = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_token_char(c));
        let after = &rest[at + needle.len()..];
        let after_ok = after.chars().next().is_none_or(|c| !is_token_char(c));
        if before_ok && after_ok {
            out.push_str(&rest[..at]);
            out.push_str(replacement);
        } else {
            out.push_str(&rest[..at + needle.len()]);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod test;
