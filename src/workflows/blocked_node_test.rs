//! Issues #881 and #880 — end-to-end proof that a node whose deliverable was
//! parked for approval does **not** report `ok`, does **not** hand its apology
//! to the next node, and that the run says what it parked.
//!
//! # Why the existing tests all stayed green through this
//!
//! The agent explicitly called `request_approval` — tested.
//! `park_gated_calls` opened a decidable card — tested by
//! [`gated_tool_turn_test`](crate::workflows::gated_tool_turn_test), which is
//! this file's direct ancestor and shares its whole fixture. The engine marked
//! the node from the capability's return value — tested upstream. What nobody
//! tested was the *seam*: a gated call inside an agent node's turn is refused
//! **inside the model's tool loop**, so the turn ends normally, the capability
//! returns `Ok`, and the engine has no way to know the node produced nothing.
//! The model then writes prose about the blockage, and `translate`'s
//! `input = "=items"` binding delivers that prose to the next node as if it were
//! the artifact. Eight green nodes, nothing shipped.
//!
//! So these drive the **real** path — real graph, real `run_workflow`, real
//! `HarnessAgentRunner`, real `HarnessPool`, real `ApprovalPolicy`, real gate,
//! real on-disk journal — and stub one thing: the model's choices, via a
//! scripted OpenAI-compatible endpoint on loopback.
//!
//! The load-bearing assertion is
//! [`a_blocked_node_stops_the_branch_and_never_hands_its_apology_downstream`]'s
//! third claim: the **downstream node's own instruction never reached the
//! model**. Asserting only that the first node is marked `blocked` would pass on
//! a fix that relabelled the row and let the edge fire anyway, which is the half
//! of the bug that actually loses the work.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::company::{CompanyManifest, parse_workflow};
use crate::harness::HarnessPool;
use crate::ports::types::{CompanyId, CompanyRecord, WorkflowNodeStatus};
use crate::ports::{WorkflowApprovalOutcome, WorkflowRun, WorkflowRunContext};
use crate::runtime::journal::RuntimeJournal;

use super::gated_tool_turn_test::{Turn, deps, spawn_script_recording};

/// The instruction authored on the node **after** the one that blocks.
///
/// Read back out of the scripted endpoint's request log: if this string was
/// never sent to the model, the second node never ran. That is the direct proof
/// the apology did not travel, and it cannot be satisfied by relabelling a
/// status.
const DOWNSTREAM_INSTRUCTION: &str = "Publish the finished spec to the newsroom.";

/// `start -> work(agent, gates a tool) -> next(agent) -> done` — the shape of
/// every real pipeline this broke on: one teammate produces an artifact, the
/// next builds on it.
fn pipeline_graph() -> String {
    format!(
        r#"
id = "pipeline"
name = "Pipeline"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "work"
kind = "agent"
name = "Work"
summary = "Write the quarterly spec."
agent = "ceo"
[[node]]
id = "next"
kind = "agent"
name = "Next"
summary = "{DOWNSTREAM_INSTRUCTION}"
agent = "ceo"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "work"
[[edge]]
from = "work"
to = "next"
[[edge]]
from = "next"
to = "done"
"#
    )
}

/// A company running with policy HITL disabled. The approval in these tests is
/// created only by the agent's explicit tool call.
///
/// The same `always_approve` choice `gated_tool_turn_test` makes and for the
/// same reason: parking under the *strongest* tier shows the block comes from
/// the policy's explicit gate rather than from a supervised-mode classifier.
/// A local copy rather than the sibling's fixture because this graph needs the
/// teammate to be addressable from two nodes, which is what makes "the second
/// node never ran" a statement about the graph rather than about the roster.
fn manifest() -> CompanyManifest {
    toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"
always_approve = ["shell"]

[tools]
allow = ["shell"]

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"
"#,
    )
    .expect("manifest parses")
}

fn record() -> CompanyRecord {
    CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest: manifest(),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        // Added by #902's desk-level tool ceiling after this fixture was
        // written. Empty means no desk narrows anything, which is what this
        // test's manifest already describes.
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

/// Runs `graph` against `turns`, returning the settled run, the journal, and
/// the full transcript of what the model was sent.
async fn run_pipeline_over(
    dir: &std::path::Path,
    graph: &str,
    turns: Vec<Turn>,
) -> (WorkflowRun, Arc<RuntimeJournal>, String) {
    let (base_url, script) = spawn_script_recording(turns).await;
    let (deps, journal) = deps(base_url, dir);
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(graph).expect("graph parses");
    let run = super::runner::run_workflow(
        pool,
        deps,
        &record,
        &file,
        json!({ "request": "the quarterly spec" }),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("a blocked run settles rather than failing — a node waiting on a person did not fail");

    let seen = script.seen.lock().unwrap().clone();
    let transcript = serde_json::to_string(&seen).expect("serialize the requests");
    (run, journal, transcript)
}

/// Runs the two-agent pipeline against `turns`, returning the settled run, the
/// journal, and the full transcript of what the model was sent.
async fn run_pipeline(
    dir: &std::path::Path,
    turns: Vec<Turn>,
) -> (WorkflowRun, Arc<RuntimeJournal>, String) {
    run_pipeline_over(dir, &pipeline_graph(), turns).await
}

/// The headline (issue #881). Four claims, and the third is the one that
/// matters: the downstream node never ran.
#[tokio::test]
async fn a_blocked_node_stops_the_branch_and_never_hands_its_apology_downstream() {
    let dir = tempfile::tempdir().unwrap();
    let (run, journal, transcript) = run_pipeline(
        dir.path(),
        vec![
            Turn::Call {
                tool: "request_approval",
                args: json!({
                    "title": "Publish the quarterly spec",
                    "question": "May I publish the finished spec?"
                }),
            },
            // Verbatim the shape the issue reported: the model is refused inside
            // its own tool loop and writes prose about it, which before this
            // became the node's output and the next node's input.
            Turn::Say("The spec is in my sandbox but the hand-off is blocked again."),
        ],
    )
    .await;

    // (a) The node that blocked is `blocked`, not `ok` and not `error`.
    let work = run
        .nodes
        .iter()
        .find(|n| n.node_id == "work")
        .expect("the blocked node still reports a row — it ran, it just produced nothing");
    assert_eq!(
        work.status,
        WorkflowNodeStatus::Blocked,
        "a node whose deliverable was parked is neither green nor failed: {:?}",
        run.nodes
    );

    // (b) The downstream node has no row at all — "not reached", which is the
    // honest reading for a branch that halted above it.
    assert!(
        !run.nodes.iter().any(|n| n.node_id == "next"),
        "the node after a blocked one must not run: {:?}",
        run.nodes
    );

    // (c) The direct proof, and the one a status relabel cannot fake: the
    // downstream node's own instruction never reached the model, so the apology
    // was never delivered to anybody.
    assert!(
        !transcript.contains(DOWNSTREAM_INSTRUCTION),
        "the blocked node's apology must not travel to the next node's turn: {transcript}"
    );

    // (d) Issue #395's property is unweakened: the card is still decidable.
    assert!(
        journal
            .pending()
            .iter()
            .any(|p| p.effect.kind == "request_approval"),
        "blocking the node must not cost the operator the card they have to decide"
    );

    // (e) The run says which node blocked and on what (#881) …
    assert_eq!(run.blocked_nodes.len(), 1, "{:?}", run.blocked_nodes);
    let blocked = &run.blocked_nodes[0];
    assert_eq!(blocked.node_id, "work");
    assert_eq!(blocked.tools, vec!["request_approval".to_string()]);
    assert_eq!(
        blocked.approval_ids.len(),
        1,
        "the block names the card that unblocks it"
    );
    assert_eq!(
        blocked.unparkable, 0,
        "the card was written, so nothing here is unaskable"
    );

    // …and lists it among what it is waiting on a person for.
    assert!(
        run.pending_approvals.contains(&"work".to_string()),
        "a blocked node is something the run is waiting on a human for: {:?}",
        run.pending_approvals
    );

    // (f) …and the run carries a receipt of what it parked (#880).
    assert_eq!(run.approvals.len(), 1, "{:?}", run.approvals);
    assert_eq!(run.approvals[0].node_id.as_deref(), Some("work"));
    assert_eq!(run.approvals[0].tool.as_deref(), Some("request_approval"));
    assert_eq!(run.approvals[0].outcome, WorkflowApprovalOutcome::Parked);
    assert!(run.approvals[0].approval_id.is_some());

    // (g) Blocked is NOT failed. A run that stopped for a person must not land
    // in the failure count, or a real failure hides among the approvals.
    assert!(!run.cancelled);
    assert!(
        run.notices.iter().any(|n| n.contains("parked 1 approval")),
        "the operator is told what the run parked, in those words: {:?}",
        run.notices
    );
}

/// The paired counter-test. Without it, a "fix" that simply stopped returning
/// `Ok` from every agent node would pass the headline above.
#[tokio::test]
async fn a_node_that_parks_nothing_still_reports_ok_and_its_branch_continues() {
    let dir = tempfile::tempdir().unwrap();
    let (run, journal, transcript) = run_pipeline(
        dir.path(),
        vec![Turn::Say("Here is the spec."), Turn::Say("Published.")],
    )
    .await;

    let work = run
        .nodes
        .iter()
        .find(|n| n.node_id == "work")
        .expect("the node ran");
    assert_eq!(
        work.status,
        WorkflowNodeStatus::Ok,
        "an ordinary agent node is unaffected by the block path"
    );
    assert!(
        run.nodes
            .iter()
            .any(|n| n.node_id == "next" && n.status == WorkflowNodeStatus::Ok),
        "the branch must still continue when nothing was parked: {:?}",
        run.nodes
    );
    assert!(
        transcript.contains(DOWNSTREAM_INSTRUCTION),
        "the downstream node's turn really did happen: {transcript}"
    );
    assert!(run.blocked_nodes.is_empty());
    assert!(run.approvals.is_empty());
    assert!(run.pending_approvals.is_empty());
    assert!(journal.pending().is_empty());
}

/// `work`, same as [`pipeline_graph`], but marked `on_error = "continue"`: the
/// author has asked the branch to survive a failing node rather than halt it.
fn pipeline_graph_continues_past_a_block() -> String {
    format!(
        r#"
id = "pipeline"
name = "Pipeline"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "work"
kind = "agent"
name = "Work"
summary = "Write the quarterly spec."
agent = "ceo"
on_error = "continue"
[[node]]
id = "next"
kind = "agent"
name = "Next"
summary = "{DOWNSTREAM_INSTRUCTION}"
agent = "ceo"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "work"
[[edge]]
from = "work"
to = "next"
[[edge]]
from = "next"
to = "done"
"#
    )
}

/// Issue #900 (tinysweeper `missing-test`): the halt-path headline test above
/// pins `reclassify_blocked` as called from [`blocked_run`], but the run
/// function calls it a **second** time — at the "reached the end normally" arm
/// — for exactly this case: an author who wrote `on_error = "continue"` (or
/// `"route"`) asked the block not to halt the branch. Nothing before this test
/// proved that second call site actually ran, so a regression there (an errant
/// early return, a condition that skipped the reclassify call) would silently
/// go back to reporting an unparkable turn's apology as node `ok` — the exact
/// #881 bug, just reachable only through `on_error = continue` instead of the
/// default.
#[tokio::test]
async fn a_blocked_node_under_on_error_continue_still_reports_blocked_and_lets_the_branch_run() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _journal, transcript) = run_pipeline_over(
        dir.path(),
        &pipeline_graph_continues_past_a_block(),
        vec![
            Turn::Call {
                tool: "request_approval",
                args: json!({
                    "title": "Publish the quarterly spec",
                    "question": "May I publish the finished spec?"
                }),
            },
            Turn::Say("The spec is in my sandbox but the hand-off is blocked again."),
            Turn::Say("Published."),
        ],
    )
    .await;

    // `on_error = continue` means the branch survives the block — unlike the
    // halt-path headline test above, `next` DOES run.
    assert!(
        transcript.contains(DOWNSTREAM_INSTRUCTION),
        "on_error=continue must let the branch past a blocked node run: {transcript}"
    );
    assert!(
        run.nodes
            .iter()
            .any(|n| n.node_id == "next" && n.status == WorkflowNodeStatus::Ok),
        "the downstream node must have a settled row too: {:?}",
        run.nodes
    );

    // But the run-level truth must not get lost on the way: `work` is still
    // reported as blocked, never `ok` and never a plain `error` that would
    // hide the block behind an unremarkable failure.
    let work = run
        .nodes
        .iter()
        .find(|n| n.node_id == "work")
        .expect("the blocked node still reports a row");
    assert_eq!(work.status, WorkflowNodeStatus::Blocked, "{:?}", run.nodes);
    assert_eq!(run.blocked_nodes.len(), 1, "{:?}", run.blocked_nodes);
    assert_eq!(run.blocked_nodes[0].node_id, "work");

    // …and `reclassify_blocked`'s union into `pending_approvals` runs on this
    // arm too, not only on the halt arm `blocked_run` covers.
    assert!(
        run.pending_approvals.contains(&"work".to_string()),
        "{:?}",
        run.pending_approvals
    );
    assert_eq!(run.approvals.len(), 1, "{:?}", run.approvals);
    assert_eq!(run.approvals[0].outcome, WorkflowApprovalOutcome::Parked);

    // Issue #900: the continued arm tells the operator what blocked via a
    // notice too — not only via the (easy to miss) node chip — same as the
    // halt arm does.
    assert!(
        run.notices.iter().any(|n| n.contains("parked 1 approval")),
        "a continued run must still say what blocked, in the same words the \
         halted arm uses: {:?}",
        run.notices
    );
}

/// Issue #880's regression, reproducing the audit exactly.
///
/// Three real `feature_pipeline` runs reported every node `ok` with
/// `pendingApprovals: []` and `deliveries: []` while the company's queue held
/// fifteen `publish_artifact` cards those runs had opened. Both of those fields
/// were **truthful** — one means the engine's gate nodes, the other means
/// `output`-node routing — and neither could answer the question. This pins that
/// the receipt does.
#[tokio::test]
async fn a_run_that_parked_a_card_says_so_even_though_it_paused_no_gate_and_routed_no_report() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _journal, _transcript) = run_pipeline(
        dir.path(),
        vec![
            Turn::Call {
                tool: "request_approval",
                args: json!({
                    "title": "Publish the quarterly spec",
                    "question": "May I publish the finished spec?"
                }),
            },
            Turn::Say("Blocked on approval."),
        ],
    )
    .await;

    // The engine paused no gate: this graph has no `requires_approval` node, so
    // every id here came from the host's block, not from the engine.
    let engine_gates: Vec<&String> = run
        .pending_approvals
        .iter()
        .filter(|id| !run.blocked_nodes.iter().any(|b| &&b.node_id == id))
        .collect();
    assert!(
        engine_gates.is_empty(),
        "no engine gate paused this run; the entries are the host's blocked nodes: {engine_gates:?}"
    );

    // And it routed no report — the graph's `output` node was never reached.
    assert!(
        run.deliveries.is_empty(),
        "truthfully empty, and truthfully not an answer: {:?}",
        run.deliveries
    );

    // The field that IS the answer.
    assert_eq!(
        run.approvals.len(),
        1,
        "a run that parked a card must say so somewhere: {:?}",
        run.approvals
    );
}

/// The graph for the #1008 blocked-arm persist test: an agent node that
/// SUCCEEDS (`intro`), then one that blocks on a gated tool (`work`).
///
/// The preceding success is deliberate — it is what makes "earlier node output
/// present" a real claim: the observer captures `intro`'s emitted text before
/// `work` halts the run, and that capture must survive the blocked arm.
fn intro_then_block_graph() -> String {
    r#"
id = "intro_block"
name = "Intro then block"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "intro"
kind = "agent"
name = "Intro"
summary = "Introduce the project."
agent = "ceo"
[[node]]
id = "work"
kind = "agent"
name = "Work"
summary = "Write the quarterly spec."
agent = "ceo"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "intro"
[[edge]]
from = "intro"
to = "work"
[[edge]]
from = "work"
to = "done"
"#
    .to_string()
}

/// Issue #1008 (blocked-arm sibling of the runner's failure-arm test): a run
/// that BLOCKS on an operator (the `#881` halt arm, which returns `Ok` with a
/// `Null` in-memory output) must STILL persist the per-node output the observer
/// captured before the block — flagged `partial`, with the earlier successful
/// node's output present. Before #1008 the blocked arm persisted NOTHING, so
/// reopening the run showed "this run predates output capture".
#[tokio::test]
async fn a_blocked_run_persists_the_partial_output_it_reached() {
    use crate::ports::run_output::WorkflowRunOutputStore;

    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(crate::store::FsOps::new(dir.path()));

    let (base_url, _script) = spawn_script_recording(vec![
        // `intro` node: a plain reply carrying a marker we can find in the
        // persisted snapshot — proof the earlier node's output was captured.
        Turn::Say("Introduction complete. BLOCK-MARKER-42."),
        // `work` node: explicitly asks for approval and the run blocks.
        Turn::Call {
            tool: "request_approval",
            args: json!({
                "title": "Publish the quarterly spec",
                "question": "May I publish the finished spec?"
            }),
        },
        Turn::Say("The spec is blocked pending your approval."),
    ])
    .await;

    let (mut deps, _journal) = deps(base_url, dir.path());
    deps.run_output_store = Some(store.clone());
    let rec = record();
    let pool = std::sync::Arc::new(HarnessPool::new());
    pool.ensure(&rec, &deps).await.expect("roster builds");

    let file = parse_workflow(&intro_then_block_graph()).expect("graph parses");
    let ctx = WorkflowRunContext::new(false);
    let run_id = ctx.run_id.clone();

    let run = super::runner::run_workflow(
        pool,
        deps,
        &rec,
        &file,
        json!({ "request": "the quarterly spec" }),
        &ctx,
    )
    .await
    .expect("a blocked run settles Ok rather than failing");
    assert!(
        run.blocked_nodes.iter().any(|b| b.node_id == "work")
            || run.pending_approvals.iter().any(|id| id == "work"),
        "the run must report that `work` blocked: {run:?}"
    );

    // The decisive assertion: the blocked run persisted a snapshot (was `None`
    // pre-#1008).
    let stored = store
        .get_run_output(&rec.id, &run_id)
        .await
        .expect("store read")
        .expect("a blocked run must still persist the output it reached before blocking");
    assert!(
        stored.partial,
        "a blocked-arm capture must be flagged partial: {stored:?}"
    );
    // The earlier successful `intro` node's produced text is in the snapshot.
    assert!(
        stored.nodes.to_string().contains("BLOCK-MARKER-42"),
        "the node that succeeded before the block must be in the partial snapshot: {}",
        stored.nodes
    );

    // Issue #881's invariant, one surface over: the blocked node produced
    // nothing, so it has no entry. Its turn ended by writing prose about being
    // refused, and filing that prose as the node's output would re-open in the
    // inspector exactly the confusion #881 fixed by stopping it reaching the
    // next node.
    assert!(
        stored.nodes.get("work").is_none(),
        "a blocked node's refusal prose must not be filed as its output: {}",
        stored.nodes
    );

    // And the settled body carries the same capture, so the live run drawer and
    // a run reopened from History cannot disagree about what the run produced.
    assert!(
        run.output.to_string().contains("BLOCK-MARKER-42"),
        "the blocked run must report the output it reached, not `null`: {}",
        run.output
    );
    assert!(
        run.output
            .get("nodes")
            .and_then(|n| n.get("work"))
            .is_none(),
        "and must exclude the blocked node there too: {}",
        run.output
    );
}

#[cfg(test)]
mod pure {
    use super::*;
    use crate::ports::WorkflowBlockedNode;
    use crate::workflows::caps::ParkedCalls;

    /// The block trigger, pinned exhaustively over the shapes a drain can hand
    /// back. Emptiness is the whole decision — a node blocks on the bare
    /// presence of a gated call, never on a guess about which one "mattered".
    #[test]
    fn only_a_drain_that_gated_nothing_leaves_the_node_alone() {
        let cases = [
            (ParkedCalls::default(), true),
            (
                ParkedCalls {
                    tools: vec!["publish_artifact".to_string()],
                    approval_ids: vec!["appr-1".to_string()],
                    unparkable: 0,
                    blockers: 0,
                },
                false,
            ),
            // A park that FAILED still blocks, and is strictly worse: there is
            // no card, so nobody will be asked.
            (
                ParkedCalls {
                    tools: vec!["publish_artifact".to_string()],
                    approval_ids: Vec::new(),
                    unparkable: 1,
                    blockers: 0,
                },
                false,
            ),
            // Excess discarded past the per-turn cap: the drain dropped the
            // entries, so there is no tool name — and the node still blocks.
            (
                ParkedCalls {
                    tools: Vec::new(),
                    approval_ids: Vec::new(),
                    unparkable: 3,
                    blockers: 0,
                },
                false,
            ),
        ];
        for (parked, expected) in cases {
            assert_eq!(parked.is_empty(), expected, "{parked:?}");
        }
    }

    /// The wording rule, and the main review item on this change: the operator
    /// is told what the run **parked**, never what it is "waiting on". A receipt
    /// cannot go stale; a settle-time outstanding count becomes a fresh lie the
    /// moment somebody approves one of the cards.
    #[test]
    fn the_operator_sentence_is_a_receipt_not_a_snapshot() {
        let notice = super::super::runner::blocked_notice(&WorkflowBlockedNode {
            node_id: "spec".to_string(),
            tools: vec!["publish_artifact".to_string()],
            approval_ids: vec!["appr-1".to_string(), "appr-2".to_string()],
            unparkable: 0,
            stranded: 0,
        });
        assert!(notice.contains("parked 2 approvals"), "{notice}");
        assert!(
            !notice.to_lowercase().contains("waiting on"),
            "a stale-able snapshot phrasing must not appear: {notice}"
        );
        assert!(
            notice.contains("the steps after it did not run"),
            "the consequence is the part the operator actually needs: {notice}"
        );
    }

    /// A park that could not happen reads differently from one that did, and
    /// says so out loud: there is no card to decide, so pointing the operator at
    /// Approvals would send them to an empty page.
    #[test]
    fn an_unparkable_call_is_not_worded_as_something_to_approve() {
        let notice = super::super::runner::blocked_notice(&WorkflowBlockedNode {
            node_id: "spec".to_string(),
            tools: vec!["publish_artifact".to_string()],
            approval_ids: Vec::new(),
            unparkable: 1,
            stranded: 0,
        });
        assert!(
            notice.contains("could not be queued for approval"),
            "{notice}"
        );
        assert!(!notice.contains("parked"), "{notice}");
    }

    /// A `WorkflowRun` written before either field existed still loads — the
    /// `#[serde(default)]` contract, which for `WorkflowRunFinished` is the
    /// difference between replaying a company's history at boot and losing it.
    #[test]
    fn a_pre_881_run_payload_still_deserializes() {
        let legacy = json!({
            "output": Value::Null,
            "pending_approvals": [],
            "deliveries": [],
            "cancelled": false,
            "nodes": [],
            "notices": [],
            "board": []
        });
        let run: WorkflowRun = serde_json::from_value(legacy).expect("a pre-#881 payload loads");
        assert!(run.blocked_nodes.is_empty());
        assert!(run.approvals.is_empty());
    }
}

// ── a graph with nothing to run (issue #976) ────────────────────────────────

/// The `campaign` / `QA Test Pipeline` shape from staging: a graph whose only
/// node is the trigger that starts it.
fn stageless_graph() -> String {
    r#"
id = "campaign"
name = "Campaign"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
"#
    .to_string()
}

/// A run of a stage-less graph says so.
///
/// This is the half of #976 that is about honesty rather than prevention.
/// `QA Test Pipeline` had **six recorded runs** that could not have done
/// anything: the engine runs such a graph happily, no stage fails because there
/// is no stage, and it settles as an ordinary finished run. A run row that says
/// nothing is its own small lie — from the run list it is indistinguishable
/// from a run that did work.
#[tokio::test]
async fn a_run_of_a_stageless_graph_says_it_had_nothing_to_run() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _journal, _transcript) =
        run_pipeline_over(dir.path(), &stageless_graph(), Vec::new()).await;

    assert!(
        run.notices
            .iter()
            .any(|n| n.contains("no stage to run") && n.contains("Add at least one node")),
        "the operator is told the run had nothing to do, and what to do about it: {:?}",
        run.notices
    );
}

/// ...and it is a **notice**, not a failure. An empty graph is not a broken
/// one: nothing was attempted and nothing went wrong, so putting a
/// half-authored stub in the failure count beside runs that genuinely failed
/// would trade one misreading for another. The same call `#638` made for the
/// approval-overflow notice this rides alongside, and `#925` made one level
/// down for `NoDestinationConfigured`.
#[tokio::test]
async fn a_stageless_run_is_not_recorded_as_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _journal, _transcript) =
        run_pipeline_over(dir.path(), &stageless_graph(), Vec::new()).await;

    assert!(
        run.blocked_nodes.is_empty(),
        "nothing was blocked: {:?}",
        run.blocked_nodes
    );
    assert!(
        run.pending_approvals.is_empty(),
        "nothing is waiting on a person: {:?}",
        run.pending_approvals
    );
}

/// An ordinary graph raises no such notice. Without this the assertion above
/// would pass against a build that notices on every run, which would train an
/// operator to skip the one that matters.
#[tokio::test]
async fn a_graph_with_a_stage_raises_no_stageless_notice() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _journal, _transcript) = run_pipeline(
        dir.path(),
        vec![Turn::Say("Wrote the spec."), Turn::Say("Reviewed it.")],
    )
    .await;

    assert!(
        !run.notices.iter().any(|n| n.contains("no stage to run")),
        "a graph that can run must stay quiet: {:?}",
        run.notices
    );
}
