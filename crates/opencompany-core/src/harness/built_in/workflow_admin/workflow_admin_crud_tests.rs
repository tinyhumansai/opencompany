use std::sync::Arc;

use serde_json::{Value, json};

use super::workflow_admin_fixtures_tests::*;
use super::*;
use crate::company::update_company_workflow;
use crate::ports::types::CompanyEvent;

// ---------------------------------------------------------------------------
// 1. create → read → update round trip
// ---------------------------------------------------------------------------

/// The contract the whole feature rests on: what `read_workflow` hands back is
/// what `update_workflow` accepts, unmodified apart from the edit itself.
///
/// If this fails, the two tools have different schemas and every edit is a
/// guess — which is the blind rewrite the read tool exists to prevent.
#[tokio::test]
async fn read_round_trips_into_an_update_that_keeps_the_workflow_enabled() {
    let fx = Fixture::new();
    let store: Arc<dyn CompanyStore> = fx.store.clone();
    let events: Arc<dyn EventLog> = fx.log.clone();
    crate::company::create_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        Some(&events),
        crate::company::RawWorkflow::try_from(
            serde_json::from_value::<CreateWorkflowArgs>(graph_args(
                "greeter", "Greeter", "Worker",
            ))
            .unwrap(),
        )
        .unwrap(),
        None,
        None,
    )
    .await
    .expect("creates");

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "greeter" }))
        .await
        .unwrap();
    let payload = data(&read);
    assert_eq!(payload["editable"], json!(true));
    assert_eq!(payload["enabled"], json!(true));
    let version = payload["version"]
        .as_str()
        .expect("a version token")
        .to_string();

    // Take the graph exactly as read, change one node's name, hand it straight
    // back with the token. No reshaping step.
    let mut graph = payload["workflow"].clone();
    graph["nodes"][1]["name"] = json!("Renamed worker");
    graph["expected_version"] = json!(version);

    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", err_text(&updated));

    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "greeter")
            .expect("loads")
            .expect("present");
    assert_eq!(reloaded.nodes[1].name, "Renamed worker");
    assert_eq!(reloaded.nodes.len(), 3, "the rest of the graph survived");
    assert_eq!(reloaded.description.as_deref(), Some("A tiny graph."));

    // Enablement is not touched by an edit, and the update journals.
    assert!(fx.enabled().await.contains(&"greeter".to_string()));
    assert!(
        fx.events()
            .iter()
            .any(|e| matches!(e, CompanyEvent::WorkflowUpdated { workflow_id, .. } if workflow_id == "greeter")),
        "{:?}",
        fx.events()
    );
    // #274: the prior body was snapshotted, so the edit is undoable.
    assert_eq!(
        fx.revisions
            .list_revisions(&fx.company, "greeter")
            .await
            .unwrap()
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------
// 2. the version token
// ---------------------------------------------------------------------------

/// An agent that never read the graph has no token, and is told exactly that
/// rather than being allowed a blind full replacement.
#[tokio::test]
async fn an_update_without_a_version_token_is_refused_before_anything_is_read() {
    let fx = Fixture::new();
    let result = UpdateWorkflowTool::new(fx.admin())
        .execute(graph_args("greeter", "Greeter", "Worker"))
        .await
        .unwrap();
    let text = err_text(&result);
    assert!(text.contains("expected_version"), "{text}");
    assert!(text.contains(READ_WORKFLOW_TOOL), "{text}");
    assert!(fx.overlays().await.is_empty(), "nothing was written");
}

/// A token from before a concurrent write is the company layer's 409, passed
/// through with its own reload instruction.
#[tokio::test]
async fn a_stale_version_token_is_the_company_layers_conflict() {
    let fx = Fixture::new();
    let store: Arc<dyn CompanyStore> = fx.store.clone();
    let revisions: Arc<dyn WorkflowRevisionStore> = fx.revisions.clone();
    let draft = crate::company::RawWorkflow::try_from(
        serde_json::from_value::<CreateWorkflowArgs>(graph_args("greeter", "Greeter", "Worker"))
            .unwrap(),
    )
    .unwrap();
    crate::company::create_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        None,
        draft,
        None,
        None,
    )
    .await
    .expect("creates");

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "greeter" }))
        .await
        .unwrap();
    let stale = data(&read)["version"].as_str().unwrap().to_string();

    // Somebody else (the console) edits it in between.
    let other = crate::company::RawWorkflow::try_from(
        serde_json::from_value::<CreateWorkflowArgs>(graph_args(
            "greeter",
            "Greeter",
            "Console worker",
        ))
        .unwrap(),
    )
    .unwrap();
    update_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        &revisions,
        None,
        other,
        None,
        None,
    )
    .await
    .expect("console edit lands");

    let mut graph = graph_args("greeter", "Greeter", "Agent worker");
    graph["expected_version"] = json!(stale);
    let result = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    let text = err_text(&result);
    assert!(text.contains("changed since you loaded it"), "{text}");
    assert!(text.contains("Reload it"), "{text}");

    // The console's edit survived — that is the whole point of the token.
    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "greeter")
            .unwrap()
            .unwrap();
    assert_eq!(reloaded.nodes[1].name, "Console worker");
}

// ---------------------------------------------------------------------------
// 3. seeds
// ---------------------------------------------------------------------------

/// A workflow shipped in the company's source tree is readable and unwritable,
/// and the read says so BEFORE a write is attempted.
#[tokio::test]
async fn a_seed_backed_workflow_reads_uneditable_and_refuses_both_writes() {
    let fx = Fixture::new();
    fx.write_seed("seeded", SEED_TOML);

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "seeded" }))
        .await
        .unwrap();
    let payload = data(&read);
    assert_eq!(payload["editable"], json!(false));
    assert!(
        payload["version"].is_null(),
        "no token for an unwritable graph"
    );
    assert_eq!(payload["workflow"]["name"], json!("Seeded flow"));

    let mut graph = graph_args("seeded", "Seeded flow", "Worker");
    graph["expected_version"] = json!("anything");
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(
        err_text(&updated).contains("defined by a file in the company source tree"),
        "{}",
        err_text(&updated)
    );

    let deleted = DeleteWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "seeded" }))
        .await
        .unwrap();
    assert!(
        err_text(&deleted).contains("defined by a file in the company source tree"),
        "{}",
        err_text(&deleted)
    );
}

const SEED_WITH_POSTCONDITION_TOML: &str = r#"
id = "seeded-pc"
name = "Seeded worker flow"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "worker"
kind = "agent"
name = "Worker"
agent = "assistant"
[node.postcondition]
require = "non_empty"
[[edge]]
from = "start"
to = "worker"
"#;

/// Codex review on #1937 (issue #1866, thread 3) — the RED-on-old proof.
/// `seed_draft` rebuilds a [`crate::company::RawNode`] per node from the
/// parsed [`crate::company::WorkflowFile`] for the seed read path — every
/// other run-policy field (`on_error`, `retry`, `requires_approval`,
/// `repeatable`, `destination`) is carried through `.clone()`, so a seed
/// node's declared `postcondition` must be too, or two things go wrong at
/// once: the runtime still enforces a gate the agent is never told about,
/// and `project_workflow_spec`'s `unexpressible` residue — the ONLY place
/// `read_workflow` surfaces a run-policy field the agent-facing spec can't
/// carry — silently omits it. On the code as it stood before this fix, the
/// second assertion below fails: `seed_draft` zeroed `postcondition` before
/// `project_workflow_spec` ever ran, so `unexpressible` was empty and the
/// whole "per-node run policy" sentence never appeared.
#[tokio::test]
async fn a_seed_backed_postcondition_is_named_in_the_read_projection() {
    let fx = Fixture::new();
    fx.write_seed("seeded-pc", SEED_WITH_POSTCONDITION_TOML);

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "seeded-pc" }))
        .await
        .unwrap();
    let payload = data(&read);
    assert_eq!(payload["editable"], json!(false));

    // The agent-facing spec has no `postcondition` field at all (same as
    // `on_error`/`retry`) — it can only ever be named in the `unexpressible`
    // prose the markdown reply carries. Match the exact phrase
    // `unexpressible_summary` renders (`node \`worker\` (postcondition)`),
    // not a bare substring — the workflow's own name must not collide.
    let markdown = read.output_for_llm(true);
    assert!(
        markdown.contains("node `worker` (postcondition)"),
        "a seed-defined postcondition must be named in the read reply's \
         per-node run policy summary, or the agent is told a stricter gate \
         does not exist when the runtime still enforces one: {markdown}"
    );
}

// ---------------------------------------------------------------------------
// 4. the agent-surface refusals
// ---------------------------------------------------------------------------

/// The gate is on the TOOLS, not on the company layer: the same target the
/// tools refuse is still writable through `update_company_workflow`, so the
/// console is untouched.
///
/// That asymmetry is the whole design claim of the guard, and this is the only
/// test that can show it.
#[tokio::test]
async fn a_scheduled_workflow_is_refused_by_the_tools_and_still_writable_by_the_console() {
    let fx = Fixture::new();
    fx.put_overlay("nightly", SCHEDULED_TOML).await;

    let mut graph = graph_args("nightly", "Nightly flow", "Worker");
    graph["expected_version"] = json!(crate::company::workflow_version(SCHEDULED_TOML));
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    let text = err_text(&updated);
    assert!(text.contains("runs on a schedule"), "{text}");
    assert!(text.contains("0 3 * * *"), "{text}");
    assert!(text.contains("console"), "{text}");

    let deleted = DeleteWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "nightly" }))
        .await
        .unwrap();
    assert!(err_text(&deleted).contains("runs on a schedule"));
    assert_eq!(fx.overlays().await.len(), 1, "nothing was removed");

    // The company layer — the console's path — still accepts the same write.
    let store: Arc<dyn CompanyStore> = fx.store.clone();
    let revisions: Arc<dyn WorkflowRevisionStore> = fx.revisions.clone();
    let draft = crate::company::RawWorkflow::try_from(
        serde_json::from_value::<CreateWorkflowArgs>(graph_args(
            "nightly",
            "Nightly flow",
            "Worker",
        ))
        .unwrap(),
    )
    .unwrap();
    update_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        &revisions,
        None,
        draft,
        None,
        None,
    )
    .await
    .expect("the console path is not gated by the agent tools' refusal");
}

/// An edit that would silently drop an operator's approval gate is refused and
/// names the node and the field.
#[tokio::test]
async fn an_update_will_not_silently_drop_a_nodes_approval_gate() {
    let fx = Fixture::new();
    fx.put_overlay("gated", GATED_TOML).await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "gated" }))
        .await
        .unwrap();
    // The read warns before the write is attempted.
    assert!(md(&read).contains("requires_approval"), "{}", md(&read));

    let mut graph = graph_args("gated", "Gated flow", "Worker");
    graph["expected_version"] = json!(data(&read)["version"].as_str().unwrap());
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    let text = err_text(&updated);
    assert!(text.contains("requires_approval"), "{text}");
    assert!(text.contains("`worker`"), "{text}");

    // And the gate is still on the stored graph.
    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "gated")
            .unwrap()
            .unwrap();
    assert_eq!(reloaded.nodes[1].requires_approval, Some(true));
}

/// **Regression, issue #1882 review.** `owner_desk` must survive an
/// agent-tool update that never mentions it. `ownerDesk` IS on the schema
/// (`create_workflow_parameters_schema`) so a caller can supply it — but an
/// agent that builds a full-replacement edit the way `read_workflow`'s own
/// projection encourages (the fields it returned, not a value it never
/// surfaced) naturally omits a field it was never shown, `RawWorkflow::
/// try_from` then leaves `owner_desk: None` on the draft. Before the fix, ANY
/// full-replacement update through this tool cleared whatever desk was
/// already on the workflow whenever the caller's edit omitted it — the
/// preserve has to happen server-side for that omitted-field case, which is
/// what this pins. The sibling case — the caller DOES supply a different
/// `ownerDesk` and that reassignment must actually apply — is pinned by
/// `an_update_applies_a_newly_supplied_owner_desk` below.
#[tokio::test]
async fn an_update_preserves_the_workflows_owner_desk() {
    let fx = Fixture::new();
    fx.put_overlay("owned", OWNED_TOML).await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "owned" }))
        .await
        .unwrap();
    let version = data(&read)["version"]
        .as_str()
        .expect("a version token")
        .to_string();

    // A full-replacement edit built the way an agent naturally would — the
    // edit omits `ownerDesk` entirely, the same as an agent that never read
    // (or does not care about) the desk assignment.
    let mut graph = graph_args("owned", "Owned flow", "Worker");
    graph["expected_version"] = json!(version);
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", err_text(&updated));

    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "owned")
            .unwrap()
            .unwrap();
    assert_eq!(
        reloaded.owner_desk.as_deref(),
        Some("engineering"),
        "an agent update must not clear a desk it was never shown"
    );
}

/// **Regression, PR #1882 review (bot finding on `workflow_admin.rs:707`).**
/// When the agent DOES supply an `ownerDesk` on a full-replacement update —
/// possible since `ownerDesk` was added to `create_workflow_parameters_schema`
/// / `CreateWorkflowArgs` (issue #1862 prerequisite) — that value must reach
/// storage. Before the fix, the tool ran `draft.owner_desk =
/// raw.owner_desk.clone()` unconditionally after `RawWorkflow::try_from` had
/// already resolved and normalized whatever the caller sent, so a caller
/// reassigning a workflow to a different desk saw `update_workflow` report
/// success while the desk silently stayed on the old value — the stale
/// "the schema has no field for it" reasoning in the comment above this test
/// no longer held once that field existed.
#[tokio::test]
async fn an_update_applies_a_newly_supplied_owner_desk() {
    let fx = Fixture::new();
    // Give the record two real desks so a reassignment resolves: the stored
    // "engineering" and a distinct target "sales".
    {
        let mut record = fx.store.load(&fx.company).await.unwrap().unwrap();
        record.manifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"assistant\"\nrole = \"Assistant\"\n\
             [[group_chat]]\nid = \"engineering\"\nname = \"Engineering\"\nmembers = [\"assistant\"]\n\
             [[group_chat]]\nid = \"sales\"\nname = \"Sales\"\nmembers = [\"assistant\"]\n",
        )
        .expect("valid manifest");
        fx.store.save(&record).await.unwrap();
    }
    fx.put_overlay("owned", OWNED_TOML).await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "owned" }))
        .await
        .unwrap();
    let version = data(&read)["version"]
        .as_str()
        .expect("a version token")
        .to_string();

    // A full-replacement edit that explicitly reassigns the desk.
    let mut graph = graph_args("owned", "Owned flow", "Worker");
    graph["ownerDesk"] = json!("sales");
    graph["expected_version"] = json!(version);
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", err_text(&updated));

    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "owned")
            .unwrap()
            .unwrap();
    assert_eq!(
        reloaded.owner_desk.as_deref(),
        Some("sales"),
        "an agent explicitly reassigning ownerDesk must have that value applied, not silently discarded for the stored one"
    );
}

/// **Regression, PR #1882 review (bot finding on `workflow_admin.rs:713`).**
/// An explicit `"ownerDesk": null` must unassign the workflow, not restore the
/// stored desk. `RawWorkflow::try_from(CreateWorkflowArgs)` parses `null` to
/// `draft.owner_desk == None` — the exact same value an omitted key produces
/// — so the `owner_desk.is_none()` fallback alone cannot tell "the caller
/// never mentioned ownership" (must preserve, per
/// `an_update_preserves_the_workflows_owner_desk` above) apart from "the
/// caller explicitly cleared it" (must apply). Before the fix, `update_workflow`
/// had no payload that could ever produce an unowned result: every value
/// resolved to either "keep stored" or "move to a different desk". This pins
/// the fix's `owner_desk_mentioned` presence check on the raw JSON.
#[tokio::test]
async fn an_update_can_explicitly_clear_owner_desk_with_null() {
    let fx = Fixture::new();
    fx.put_overlay("owned", OWNED_TOML).await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "owned" }))
        .await
        .unwrap();
    let version = data(&read)["version"]
        .as_str()
        .expect("a version token")
        .to_string();

    // A full-replacement edit that explicitly unassigns the desk.
    let mut graph = graph_args("owned", "Owned flow", "Worker");
    graph["ownerDesk"] = Value::Null;
    graph["expected_version"] = json!(version);
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", err_text(&updated));

    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "owned")
            .unwrap()
            .unwrap();
    assert_eq!(
        reloaded.owner_desk, None,
        "an agent explicitly sending ownerDesk: null must clear the stored desk, not restore it"
    );
}

/// Sibling of the null case above: an explicit all-whitespace `ownerDesk`
/// carries the same "I thought about this and I want it unowned" signal as
/// `null` — `normalize_owner_desk` already treats blank the same as absent
/// for validation purposes, and this pins that the update tool's presence
/// check (keyed on the JSON field existing, not on what it normalizes to)
/// clears rather than preserves for this shape too.
#[tokio::test]
async fn an_update_can_explicitly_clear_owner_desk_with_blank_string() {
    let fx = Fixture::new();
    fx.put_overlay("owned", OWNED_TOML).await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "owned" }))
        .await
        .unwrap();
    let version = data(&read)["version"]
        .as_str()
        .expect("a version token")
        .to_string();

    let mut graph = graph_args("owned", "Owned flow", "Worker");
    graph["ownerDesk"] = json!("   ");
    graph["expected_version"] = json!(version);
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", err_text(&updated));

    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "owned")
            .unwrap()
            .unwrap();
    assert_eq!(
        reloaded.owner_desk, None,
        "an agent explicitly sending a blank ownerDesk must clear the stored desk, not restore it"
    );
}

#[tokio::test]
async fn large_workflow_pages_preserve_every_byte_and_reject_mixed_versions() {
    let fx = Fixture::new();
    let store: Arc<dyn CompanyStore> = fx.store.clone();
    let mut spec = graph_args("paged", "Paged", "Worker");
    let prompt = "Reken café 🧪; behoud alle regels.\n".repeat(700);
    spec["nodes"][1]["config"] = json!({"prompt":prompt});
    crate::company::create_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        None,
        crate::company::RawWorkflow::try_from(
            serde_json::from_value::<CreateWorkflowArgs>(spec).unwrap(),
        )
        .unwrap(),
        None,
        None,
    )
    .await
    .unwrap();
    let tool = ReadWorkflowTool::new(fx.admin());
    let first = tool.execute(json!({"id":"paged"})).await.unwrap();
    let payload = data(&first);
    let count = payload["page_count"].as_u64().unwrap();
    assert!(count > 1);
    assert!(
        payload["workflow"].is_null(),
        "a fragment is never a complete graph"
    );
    let mut assembled = String::new();
    for page in 0..count {
        let result = tool
            .execute(json!({"id":"paged","page":page,"read_version":payload["read_version"]}))
            .await
            .unwrap();
        let value = data(&result);
        let fragment = value["graph_fragment"].as_str().unwrap();
        assert!(
            md(&result).contains(fragment),
            "the model must receive the actual graph bytes"
        );
        assert!(md(&result).len() < TOOL_RESULT_BUDGET_BYTES);
        assembled.push_str(fragment);
    }
    let mut graph: Value = serde_json::from_str(&assembled).unwrap();
    assert_eq!(graph["nodes"][1]["config"]["prompt"], prompt);
    graph["name"] = json!("Changed");
    graph["expected_version"] = payload["version"].clone();
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", updated.output_for_llm(false));
    let stale = tool
        .execute(json!({"id":"paged","page":1,"read_version":payload["read_version"]}))
        .await
        .unwrap();
    assert!(err_text(&stale).contains("stale read_version"));
    let missing = tool.execute(json!({"id":"paged","page":1})).await.unwrap();
    assert!(err_text(&missing).contains("read_version"));
    let invalid = tool.execute(json!({"id":"paged","page":-1})).await.unwrap();
    assert!(err_text(&invalid).contains("non-negative"));
}
