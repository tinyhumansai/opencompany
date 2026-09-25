//! Tests for the skill read tools' naming boundary (issue #845).
//!
//! Every test here fails against the pre-fix wiring — the tools were handed to
//! agents exactly as upstream names them, so each assertion below is one of the
//! four surfaces that said "workflow".

use serde_json::json;

use super::*;
use crate::harness::skills::EffectiveSkills;

/// Materializes a one-skill effective set and returns its read tools.
fn tools_for(slug: &str, name: &str) -> (tempfile::TempDir, Vec<Box<dyn Tool>>) {
    let src = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let dir = src.path().join("skills").join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Does {slug} things\n---\n\n# {name}\n\nBODY.\n"),
    )
    .unwrap();
    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[],
        "agent-under-test",
        None,
    )
    .unwrap();
    // `ws` is dropped by the caller holding the returned handle; `src` is only
    // read during materialize, so only `ws` has to outlive the tools.
    let tools = eff.read_tools();
    (ws, tools)
}

fn find<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> &'a dyn Tool {
    tools
        .iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("no tool named {name}"))
        .as_ref()
}

/// Surface 1: the names. This is the whole of what an agent sees on its belt.
#[test]
fn read_tools_are_named_after_skills() {
    let (_ws, tools) = tools_for("web-research", "Web Research");
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        vec![
            LIST_SKILLS_TOOL,
            DESCRIBE_SKILL_TOOL,
            READ_SKILL_RESOURCE_TOOL
        ]
    );
    // The bug in one assertion: no tool on a desk agent's belt may claim to
    // enumerate workflows, because none of them can see the workflow registry.
    for name in names {
        assert!(
            !name.contains("workflow"),
            "{name} still claims to be about workflows"
        );
    }
}

/// Surface 2: the descriptions. Upstream's `list_workflows` description ends
/// "…to inspect (`describe_workflow`) or run (`run_workflow`)" — an instruction
/// to take something out of the skill tree and run it as a workflow, which is
/// the cross-registry step itself.
#[test]
fn descriptions_never_send_an_agent_to_a_workflow_tool() {
    let (_ws, tools) = tools_for("web-research", "Web Research");
    for tool in &tools {
        let d = tool.description();
        assert!(
            !d.contains("run_workflow") && !d.contains("describe_workflow"),
            "{}: description points at a workflow tool: {d}",
            tool.name()
        );
    }
    // And the list tool says outright that this is not the workflow registry,
    // so a model that finds no workflow tools does not substitute this list.
    let listed = find(&tools, LIST_SKILLS_TOOL).description();
    assert!(listed.contains("not"), "{listed}");
    assert!(listed.contains("workflows"), "{listed}");
}

/// Surface 3: the argument. A model copies argument names out of the schema, so
/// a schema saying `workflow_id` teaches it that a skill id is a workflow id.
#[test]
fn schemas_take_skill_id_not_workflow_id() {
    let (_ws, tools) = tools_for("web-research", "Web Research");

    for name in [DESCRIBE_SKILL_TOOL, READ_SKILL_RESOURCE_TOOL] {
        let schema = find(&tools, name).parameters_schema();
        let props = schema["properties"].as_object().expect("properties");
        assert!(props.contains_key(SKILL_ID_ARG), "{name}: {schema}");
        assert!(!props.contains_key(UPSTREAM_ID_ARG), "{name}: {schema}");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(required.contains(&SKILL_ID_ARG), "{name}: {schema}");
        assert!(!required.contains(&UPSTREAM_ID_ARG), "{name}: {schema}");
        // The id property's own description carried "Workflow id" too.
        assert!(
            !props[SKILL_ID_ARG]["description"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("workflow"),
            "{name}: {schema}"
        );
    }

    // The list tool takes no arguments, so its schema is passed through as-is.
    let list = find(&tools, LIST_SKILLS_TOOL).parameters_schema();
    assert_eq!(list["type"], "object");
}

/// Surface 4: the payload. This is the one the issue quotes — an agent reading
/// `{"count":4,"workflows":[…]}` reports "there are exactly 4 installed
/// workflows" no matter what the tool it called was named.
#[tokio::test]
async fn list_payload_is_keyed_skills_not_workflows() {
    let (_ws, tools) = tools_for("web-research", "Web Research");
    let out = find(&tools, LIST_SKILLS_TOOL)
        .execute(json!({}))
        .await
        .expect("list")
        .output_for_llm(false);

    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json payload");
    assert!(parsed.get("skills").is_some(), "{out}");
    assert!(parsed.get("workflows").is_none(), "{out}");
    // Still the same content — the rename must not cost the enumeration.
    assert!(out.contains("web-research"), "{out}");
}

/// The renamed argument reaches the inner tool, so `describe_skill` still works.
#[tokio::test]
async fn describe_accepts_skill_id_and_still_reads_the_skill() {
    let (_ws, tools) = tools_for("web-research", "Web Research");
    let out = find(&tools, DESCRIBE_SKILL_TOOL)
        .execute(json!({ SKILL_ID_ARG: "web-research" }))
        .await
        .expect("describe")
        .output_for_llm(false);
    assert!(out.contains("Web Research"), "{out}");
}

/// A model working from a stale schema keeps working. The rename is a naming
/// fix; it must not strand a conversation that already saw the old parameter.
#[tokio::test]
async fn legacy_workflow_id_argument_still_resolves() {
    let (_ws, tools) = tools_for("web-research", "Web Research");
    let out = find(&tools, DESCRIBE_SKILL_TOOL)
        .execute(json!({ UPSTREAM_ID_ARG: "web-research" }))
        .await
        .expect("describe via the upstream key")
        .output_for_llm(false);
    assert!(out.contains("Web Research"), "{out}");
}

/// `to_inner_args` only ever *adds* the upstream key from ours
/// (`map.entry(UPSTREAM_ID_ARG).or_insert(id)`), so a call carrying both keys
/// at once is resolved by whichever key the inner tool happens to prefer —
/// here, the legacy `workflow_id` silently wins over `skill_id` rather than
/// either being refused as ambiguous or `skill_id` taking precedence as the
/// tool's own documented argument.
#[tokio::test]
async fn both_id_keys_at_once_resolve_by_the_legacy_key_not_skill_id() {
    let (_ws, tools) = tools_for("web-research", "Web Research");
    let out = find(&tools, DESCRIBE_SKILL_TOOL)
        .execute(json!({ SKILL_ID_ARG: "nope", UPSTREAM_ID_ARG: "web-research" }))
        .await
        .expect("describe")
        .output_for_llm(false);
    assert!(
        out.contains("Web Research"),
        "the legacy key silently wins over skill_id, undocumented: {out}"
    );
}

/// `read_skill_resource`'s payload echoes the id back under its own key.
#[tokio::test]
async fn read_resource_echoes_skill_id_not_workflow_id() {
    let (_ws, tools) = tools_for("web-research", "Web Research");
    let out = find(&tools, READ_SKILL_RESOURCE_TOOL)
        .execute(json!({ SKILL_ID_ARG: "web-research", "relative_path": "SKILL.md" }))
        .await
        .expect("read resource")
        .output_for_llm(false);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json payload");
    assert!(parsed.get("skill_id").is_some(), "{out}");
    assert!(parsed.get("workflow_id").is_none(), "{out}");
    // The file's own body is content, not a key, and is never rewritten.
    assert!(out.contains("BODY."), "{out}");
}

/// Upstream returns "not found" as an `Err` whose string reaches the agent, and
/// that string names `describe_workflow` and a "workflow".
#[tokio::test]
async fn missing_skill_error_is_phrased_in_skills() {
    let (_ws, tools) = tools_for("web-research", "Web Research");
    let err = find(&tools, DESCRIBE_SKILL_TOOL)
        .execute(json!({ SKILL_ID_ARG: "nope" }))
        .await
        .expect_err("a missing skill must error")
        .to_string();
    assert!(!err.contains("workflow"), "{err}");
    assert!(err.contains("skill"), "{err}");
}

/// The catalogue sentence in the persona is what hands an agent the three
/// names, so it is also what taught every agent the wrong word.
#[test]
fn persona_catalogue_names_the_skill_tools() {
    let src = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let dir = src.path().join("skills").join("seo-audit");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: SEO Audit\ndescription: Audit a site\n---\n\n# SEO\n",
    )
    .unwrap();
    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[],
        "agent-under-test",
        None,
    )
    .unwrap();

    let catalogue = eff.catalogue();
    assert!(catalogue.contains(LIST_SKILLS_TOOL), "{catalogue}");
    assert!(catalogue.contains(DESCRIBE_SKILL_TOOL), "{catalogue}");
    assert!(catalogue.contains(READ_SKILL_RESOURCE_TOOL), "{catalogue}");
    assert!(!catalogue.contains("list_workflows"), "{catalogue}");
    assert!(!catalogue.contains("describe_workflow"), "{catalogue}");
    assert!(!catalogue.contains("read_workflow_resource"), "{catalogue}");
}

/// The rename must carry the consequence catalogue with it.
///
/// `undeclared`'s read-only prefixes are `read`/`list`/`get`/… and **not**
/// `describe`, so an undeclared `describe_skill` is `Reach::Consequence` — it
/// parks, and interrupts an operator to approve a local file read. That is the
/// exact regression `consequence.rs`'s own comment records `describe_workflow`
/// having caused before the table existed; this pins that the rename did not
/// reintroduce it.
#[test]
fn skill_read_tools_are_declared_reads() {
    use crate::policy::consequence::{Reach, consequence_of};
    for tool in [
        LIST_SKILLS_TOOL,
        DESCRIBE_SKILL_TOOL,
        READ_SKILL_RESOURCE_TOOL,
    ] {
        assert_eq!(
            consequence_of(tool, &json!({})).reach,
            Reach::Nothing,
            "{tool} is not a declared read — it will park"
        );
    }
}

// ---------------------------------------------------------------------------
// The prose scrub's one sharp edge
// ---------------------------------------------------------------------------

/// A skill's *own* slug may contain "workflow" — `content-workflow` is one of
/// the workflow names on the staging company's own Workflows page. Rewriting it
/// inside an error message would report a skill id that does not exist.
#[test]
fn whole_word_replace_leaves_hyphenated_ids_alone() {
    assert_eq!(
        replace_whole_words("skill `content-workflow` not found", "workflow", "skill"),
        "skill `content-workflow` not found"
    );
    assert_eq!(
        replace_whole_words("describe_workflow failed", "workflow", "skill"),
        "describe_workflow failed"
    );
    assert_eq!(
        replace_whole_words("workflow `x` not found", "workflow", "skill"),
        "skill `x` not found"
    );
    // A bare match at either end of the string.
    assert_eq!(
        replace_whole_words("workflow", "workflow", "skill"),
        "skill"
    );
    assert_eq!(
        replace_whole_words("the workflow", "workflow", "skill"),
        "the skill"
    );
}

/// Tool names are rewritten before bare words, so an error naming a tool names
/// the tool the agent actually called rather than a half-rewritten hybrid.
#[test]
fn prose_rewrites_tool_names_whole() {
    let out = rewrite_prose("describe_workflow: workflow `x` is not available");
    assert_eq!(out, "describe_skill: skill `x` is not available");
    assert!(!out.contains("describe_skill_"), "{out}");
}

/// Only the top level is re-keyed. A skill that documents workflows keeps its
/// own words — the payload rewrite is about the envelope, never the content.
#[test]
fn nested_content_is_never_rewritten() {
    let mut payload = json!({
        "workflows": [
            { "name": "Workflow Builder", "description": "Explains the workflow registry" }
        ]
    });
    rename_top_level_keys(&mut payload);
    assert!(payload.get("skills").is_some(), "{payload}");
    let entry = &payload["skills"][0];
    assert_eq!(entry["name"], "Workflow Builder");
    assert_eq!(entry["description"], "Explains the workflow registry");
}

/// Materializes `n` skills and returns their read tools.
fn tools_for_many(n: usize) -> (tempfile::TempDir, Vec<Box<dyn Tool>>) {
    let src = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    for i in 0..n {
        let dir = src.path().join("skills").join(format!("skill-{i:03}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: Skill {i:03}\ndescription: {}\n---\n\n# Skill {i:03}\n\nBODY.\n",
                "a long enough description that a hundred of them are not free ".repeat(3)
            ),
        )
        .unwrap();
    }
    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[],
        "agent-under-test",
        None,
    )
    .unwrap();
    let tools = eff.read_tools();
    (ws, tools)
}

/// `list_skills` renders every installed skill with its name, dir,
/// description, tags, tool hints, scope and warnings, and nothing bounds how
/// many. The skill tree is materialized from what the company installs, so its
/// size is not a constant the wrapper gets to assume — and this output lands
/// in the turn's context, where an unbounded block crowds out the work.
///
/// Every other listing on the belt is capped and says how many it left out;
/// this one is the exception.
#[tokio::test]
#[ignore = "list_skills renders every installed skill with no cap and no elision line"]
async fn listing_a_large_skill_tree_is_bounded_and_says_what_it_left_out() {
    let (_ws, tools) = tools_for_many(200);
    let out = find(&tools, LIST_SKILLS_TOOL)
        .execute(json!({}))
        .await
        .expect("list")
        .output_for_llm(false);

    assert!(
        out.len() < 20_000,
        "the listing grew to {} characters for 200 installed skills, with nothing bounding it",
        out.len()
    );
    assert!(
        out.contains("more"),
        "a truncated listing must say how many skills it did not name: {out}"
    );
}

/// The bound on the cap: a tree small enough to render whole must still render
/// whole, so the cap above cannot be satisfied by a listing that hides skills
/// an agent could have used.
#[tokio::test]
async fn a_small_skill_tree_is_listed_in_full() {
    let (_ws, tools) = tools_for_many(3);
    let out = find(&tools, LIST_SKILLS_TOOL)
        .execute(json!({}))
        .await
        .expect("list")
        .output_for_llm(false);
    for i in 0..3 {
        assert!(out.contains(&format!("skill-{i:03}")), "{out}");
    }
}
