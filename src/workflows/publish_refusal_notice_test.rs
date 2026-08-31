//! Issue #1192 — end-to-end proof that a workflow node whose `publish_artifact`
//! was refused says so **on the run**, and not only through the model's prose.
//!
//! # The incident this reproduces
//!
//! A `campaign_pipeline` run's teammate wrote a file, called `publish_artifact`,
//! and was refused: the destination is [`PublishDestination::Unclaimed`] on a
//! run, because a run has no card to attach a version to. That refusal is
//! correct. What was wrong is what happened next — it was told to nobody but the
//! model. The model wrote an apology, the apology became the node's `text`
//! output, the `=items` binding delivered it downstream as though it were the
//! deliverable, and the run settled clean. The only other trace was a
//! `tracing::warn!`, and a log line is not the operator learning anything.
//!
//! `caps`'s own doc already named this failure for the *gated* case — "that is
//! exactly how a gated `publish_artifact` came to hand the model's apology
//! downstream" — which #881 fixed with a structural notice. The `Unclaimed` case
//! never got the same treatment.
//!
//! # Why this is a turn test rather than a unit test
//!
//! Every part works in isolation and has tests. The queue records the refusal —
//! tested in `publish::test`. `RunNotices` collects and the runner drains it —
//! tested since #638. The history panel renders `run.notices` — tested in the
//! console suite. What was missing is the *join*: nothing on the workflow path
//! ever asked the queue whether a publish had been refused. A unit test over any
//! one part stays green through that, which is precisely how the gap shipped.
//!
//! So this drives the **real** path — real graph, real `run_workflow`, real
//! `HarnessAgentRunner`, real `HarnessPool`, real file + publish tools, real
//! artifact store — and stubs one thing at the one boundary that needs a
//! credential: the model's *choices*, via the scripted endpoint
//! [`gated_tool_turn_test`](crate::workflows::gated_tool_turn_test) established.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::routing::post;
use serde_json::json;

use crate::company::{CompanyManifest, parse_workflow};
use crate::harness::HarnessPool;
use crate::harness::publish::PublishDestination;
use crate::ports::WorkflowRunContext;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::store::FsOps;

use super::gated_tool_turn_test::{Turn, deps, spawn_script};

/// The one-agent graph: trigger → agent → output. The shape a company authors
/// when it wants a teammate to produce something on a schedule — and the shape
/// `campaign_pipeline` has.
const AGENT_GRAPH: &str = r#"
id = "publishing"
name = "Publishing"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "work"
kind = "agent"
name = "Work"
summary = "Draft the launch spec."
agent = "ceo-a"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "work"
[[edge]]
from = "work"
to = "done"
"#;

/// A company that grants `files` — so the file belt **and** `publish_artifact`
/// are both on the teammate's belt, which is the only configuration in which
/// this bug is reachable.
///
/// `mode = "full"` with no `always_approve`: nothing here is gated. That keeps
/// the test on the #1192 axis (a refusal for want of a *destination*) rather
/// than the #881 one (a refusal for want of an *approval*), which is a different
/// mechanism with a different fix.
fn manifest() -> CompanyManifest {
    toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[tools]
allow = ["files"]

[[agent]]
id = "ceo-a"
role = "Chief Executive A"
tier = "orchestrator"

[[agent]]
id = "ceo-b"
role = "Chief Executive B"
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
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

/// The path the scripted teammate writes and then tries to hand over.
const SOURCE: &str = "launch-spec.md";

/// Runs the graph once against a teammate that writes a file, offers it, and —
/// having been refused — apologises in prose, which is exactly what the real
/// model did.
async fn run_publishing(dir: &std::path::Path) -> crate::ports::WorkflowRun {
    let base_url = spawn_script(vec![
        Turn::Call {
            tool: "file_write",
            args: json!({ "path": SOURCE, "content": "# Launch spec\n" }),
        },
        Turn::Call {
            tool: "publish_artifact",
            args: json!({ "path": SOURCE }),
        },
        Turn::Say("I drafted the launch spec but I could not publish it, sorry."),
    ])
    .await;
    let (mut deps, _journal) = deps(base_url, dir);
    // The artifact store production always wires. Without it the tool is not on
    // the belt at all (the #244 fail-closed gate) and the test would prove
    // nothing about the destination.
    deps.artifacts = Some(Arc::new(FsOps::new(dir)));
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    // The premise, asserted rather than assumed: a run claims no destination.
    assert_eq!(
        deps.pending_publishes.destination(),
        PublishDestination::Unclaimed,
        "a workflow run takes no publish claim — that is what makes the refusal reachable"
    );

    let file = parse_workflow(AGENT_GRAPH).expect("graph parses");
    let ctx = WorkflowRunContext::new(false);
    super::runner::run_workflow(
        pool,
        deps.clone(),
        &record,
        &file,
        json!({ "request": "the launch spec" }),
        &ctx,
    )
    .await
    .expect("the run settles — a refused publish is not a failed run")
}

/// A scripted endpoint whose per-lane responses let two runs overlap while
/// they share one publish queue. Each run receives a refused publish before it
/// completes; a two-lane barrier holds each run's final response until both
/// have executed their `publish_artifact`, so either drain is guaranteed to
/// meet both refusals — the exact cross-run schedule the shared bucket got
/// wrong.
async fn spawn_interleaved_publish_script() -> String {
    let steps = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let app = axum::Router::new().route(
        "/chat/completions",
        post(move |Json(body): Json<serde_json::Value>| {
            let steps = Arc::clone(&steps);
            let barrier = Arc::clone(&barrier);
            async move {
                let rendered = body.to_string();
                let (lane, source) = if rendered.contains("run-a") {
                    ("run-a", "run-a.md")
                } else if rendered.contains("run-b") {
                    ("run-b", "run-b.md")
                } else {
                    panic!("scripted workflow request did not name a lane: {rendered}");
                };
                let step = {
                    let mut guard = steps.lock().expect("script steps");
                    let step = guard.entry(lane.to_string()).or_default();
                    let current = *step;
                    *step += 1;
                    current
                };
                let message = match step {
                    0 => json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": format!("write-{lane}"),
                            "type": "function",
                            "function": { "name": "file_write", "arguments": json!({ "path": source, "content": "draft" }).to_string() }
                        }]
                    }),
                    1 => json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": format!("publish-{lane}"),
                            "type": "function",
                            "function": { "name": "publish_artifact", "arguments": json!({ "path": source }).to_string() }
                        }]
                    }),
                    2 => {
                        // Both tool calls have executed and their refusals are
                        // queued; releasing one run before the other would let
                        // the first drain a bucket that had only one entry, and
                        // a reverted shared-bucket implementation would pass.
                        barrier.wait().await;
                        json!({ "role": "assistant", "content": "could not publish" })
                    }
                    _ => json!({ "role": "assistant", "content": "done" }),
                };
                Json(json!({
                    "choices": [{ "index": 0, "message": message }],
                    "usage": { "prompt_tokens": 12, "completion_tokens": 4 }
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted provider");
    let addr = listener.local_addr().expect("scripted provider address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// **The headline.** A refused publish reaches the operator as a run notice that
/// names the file.
///
/// Three assertions, and all three are the incident:
///
/// * the notice exists and **names the path**, so the operator knows *which*
///   file is stranded rather than that "something" went wrong;
/// * the node's own output carries **no apology**, because the notice is
///   composed from the source path and nothing else — the model's prose is not
///   the channel and must never become it;
/// * the run still scores **`ok`**, because a refused publish did not stop the
///   node. That is why this is a notice and not a `Blocked` node: `Blocked`
///   halts the branch and is not auto-resumable, and there is no approval here
///   for anyone to give.
#[tokio::test]
async fn a_workflow_node_whose_publish_was_refused_says_so_on_the_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = run_publishing(dir.path()).await;

    let named: Vec<&String> = run
        .notices
        .iter()
        .filter(|notice| notice.contains(SOURCE))
        .collect();
    assert_eq!(
        named.len(),
        1,
        "exactly one notice must name the stranded file: {:?}",
        run.notices
    );
    let notice = named[0];
    assert!(
        notice.contains("deliverable"),
        "the notice must say what could not happen, in structural words: {notice}"
    );

    // The run is clean. A refused publish is not a failure and not a block.
    assert!(
        run.pending_approvals.is_empty(),
        "nobody is being asked to approve anything: {:?}",
        run.pending_approvals
    );
    assert!(
        run.blocked_nodes.is_empty(),
        "the node ran to completion; promoting this to a block would halt a branch that did \
         not stop: {:?}",
        run.blocked_nodes
    );
    assert!(!run.cancelled);

    // The model's apology stayed where it was — in the node output — and did not
    // become the operator's notification.
    let output = serde_json::to_string(&run.output).expect("serialize output");
    assert!(
        !notice.contains("sorry"),
        "the notice is composed from the path, never from the model's prose: {notice}"
    );
    assert!(
        output.contains("sorry"),
        "the apology is still the node's text — this test asserts the notice is ADDITIONAL, \
         not that the prose was suppressed: {output}"
    );
}

/// Concurrent runs keep their refused-publish notices separate even though both
/// dispatch through one cached roster and its one queue handle.
#[tokio::test]
async fn concurrent_workflow_runs_do_not_take_each_others_publish_refusals() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base_url = spawn_interleaved_publish_script().await;
    let (mut deps, _journal) = deps(base_url, dir.path());
    deps.artifacts = Some(Arc::new(FsOps::new(dir.path())));
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps)
        .await
        .expect("roster builds once");
    let file_a = parse_workflow(AGENT_GRAPH).expect("graph parses");
    let graph_b = AGENT_GRAPH.replace("agent = \"ceo-a\"", "agent = \"ceo-b\"");
    let file_b = parse_workflow(&graph_b).expect("graph parses");
    let a = WorkflowRunContext::new(false);
    let b = WorkflowRunContext::new(false);

    let (run_a, run_b) = tokio::join!(
        super::runner::run_workflow(
            Arc::clone(&pool),
            deps.clone(),
            &record,
            &file_a,
            json!({ "request": "run-a" }),
            &a,
        ),
        super::runner::run_workflow(
            Arc::clone(&pool),
            deps.clone(),
            &record,
            &file_b,
            json!({ "request": "run-b" }),
            &b,
        ),
    );
    let run_a = run_a.expect("run A settles");
    let run_b = run_b.expect("run B settles");

    for (run, own, sibling) in [
        (run_a, "run-a.md", "run-b.md"),
        (run_b, "run-b.md", "run-a.md"),
    ] {
        assert!(
            run.notices.iter().any(|notice| notice.contains(own)),
            "the run must report its own refused publish: {:?}",
            run.notices
        );
        assert!(
            !run.notices.iter().any(|notice| notice.contains(sibling)),
            "the run must not report its sibling's refused publish: {:?}",
            run.notices
        );
    }
}

/// The run's notice does not come at the cost of the #244 unpublished-work scan:
/// the refused file must still read as **unpublished**.
///
/// `sources()` is the scan's whole gate (`changed − staged`). A refusal recorded
/// as a source would make the nudge go quiet on the file that is *most* at risk
/// — the one an agent explicitly tried and failed to hand over.
#[tokio::test]
async fn a_refused_publish_leaves_nothing_staged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base_url = spawn_script(vec![
        Turn::Call {
            tool: "file_write",
            args: json!({ "path": SOURCE, "content": "# Launch spec\n" }),
        },
        Turn::Call {
            tool: "publish_artifact",
            args: json!({ "path": SOURCE }),
        },
        Turn::Say("could not publish"),
    ])
    .await;
    let (mut deps, _journal) = deps(base_url, dir.path());
    deps.artifacts = Some(Arc::new(FsOps::new(dir.path())));
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(AGENT_GRAPH).expect("graph parses");
    let ctx = WorkflowRunContext::new(false);
    super::runner::run_workflow(
        pool,
        deps.clone(),
        &record,
        &file,
        json!({ "request": "the launch spec" }),
        &ctx,
    )
    .await
    .expect("the run settles");

    assert_eq!(
        deps.pending_publishes.sources(),
        Vec::<String>::new(),
        "a refused file is not a staged one — it must still read as unpublished"
    );
    assert_eq!(deps.pending_publishes.queued(), 0);
}
