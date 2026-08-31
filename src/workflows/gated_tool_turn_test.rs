//! Issue #395, hole 1 — end-to-end proof that a tool call gated *inside a
//! workflow agent node* reaches the operator's Approvals page and stays there.
//!
//! # Why a unit test could not have caught this
//!
//! Every part of the mechanism already worked in isolation, and each had tests.
//! [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) recorded the
//! blocked call on the shared queue — tested. The queue stamped run ids and
//! deduped — tested.
//! [`DeliveryParking`](crate::workflows::DeliveryParking) parked and journaled
//! transactionally — tested. What was missing was the *join*: nothing on the
//! workflow path ever drained the queue, because the only drain lives inside
//! `run_cycle` behind a [`CycleHost`](crate::ports::brain::CycleHost) and
//! `run_agent` → [`HarnessPool::run_background`](crate::harness::HarnessPool)
//! never goes near a cycle. The request was recorded, then thrown away by the
//! next chat cycle's `clear()`. Green tests, empty Approvals page.
//!
//! So this drives the **real** path — real graph, real `run_workflow`, real
//! `HarnessAgentRunner`, real `HarnessPool`, real `ApprovalPolicy`, real gate
//! and real on-disk journal — and stubs exactly one thing, at the one boundary
//! that needs a credential: the model's *choices*, via a scripted
//! OpenAI-compatible endpoint on loopback. That is the shape
//! [`workspace_turn_test`](crate::harness::workspace_turn_test) established.
//!
//! The load-bearing assertion is the last one:
//! [`the_parked_request_survives_a_later_chat_cycle`]. Parking the request is
//! only half the fix — a card that a subsequent conversation wipes is the same
//! bug with a longer fuse.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

use crate::company::credentials::Credential;
use crate::company::{CompanyManifest, parse_workflow};
use crate::harness::mcp_probe::McpFailureQueue;
use crate::harness::orchestrator::{DelegationQueue, WorkflowRunnerHandle};
use crate::harness::policy::ApprovalRequestQueue;
use crate::harness::provider::{HostedProvider, HostedProviderConfig};
use crate::harness::{HarnessDeps, HarnessPool};
use crate::ports::WorkflowRunContext;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::runtime::journal::RuntimeJournal;
use crate::store::{FsCompanyStore, FsContextStore, FsInboxStore, FsOps};
use crate::workflows::delivery::{DeliveryParking, WorkflowDeliveryDeps};

/// A graph whose single working node is an agent turn — the exact shape a
/// company authors when it wants a teammate to do something on a schedule.
const AGENT_GRAPH: &str = r#"
id = "gated"
name = "Gated"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "work"
kind = "agent"
name = "Work"
summary = "Export the quarterly numbers."
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
to = "done"
"#;

/// What the scripted model does on each successive call.
///
/// `pub(super)` alongside [`deps`] and [`record`] so the sibling
/// [`board_turn_test`](crate::workflows::board_turn_test) drives the same
/// scripted-model harness rather than duplicating it (issue #661).
#[derive(Clone, Debug)]
pub(super) enum Turn {
    /// Emit a native tool call the policy will gate.
    Call {
        /// The tool the model asks for.
        tool: &'static str,
        /// Its arguments, verbatim.
        args: Value,
    },
    /// Finish with plain assistant text.
    Say(&'static str),
}

/// A scripted OpenAI-compatible `/chat/completions` endpoint.
pub(super) struct Script {
    turns: Mutex<Vec<Turn>>,
    /// Every request body the model was sent, in order (issue #453). The tool
    /// results of a turn come back to the model inside the *next* request, so
    /// this is where a test reads what a refused tool actually told it — and,
    /// since #881, what a node downstream of a blocked one was never sent.
    pub(super) seen: Mutex<Vec<Value>>,
}

/// Serve the script on loopback and return its base URL, handing back the
/// shared script so a test can read what the model was sent.
///
/// `pub(super)` since #881: proving a blocked node's branch did **not** continue
/// means reading what the model was never asked, so the sibling
/// [`blocked_node_test`](crate::workflows::blocked_node_test) needs the recorder
/// rather than only the URL.
pub(super) async fn spawn_script_recording(turns: Vec<Turn>) -> (String, Arc<Script>) {
    let script = Arc::new(Script {
        turns: Mutex::new(turns),
        seen: Mutex::new(Vec::new()),
    });
    let handle = Arc::clone(&script);
    let app = axum::Router::new().route(
        "/chat/completions",
        post(move |Json(body): Json<Value>| {
            let script = Arc::clone(&script);
            async move {
                script.seen.lock().unwrap().push(body);
                let next = {
                    let mut turns = script.turns.lock().unwrap();
                    if turns.is_empty() {
                        None
                    } else {
                        Some(turns.remove(0))
                    }
                };
                // Running off the end of the script means the loop went round
                // more times than expected; end the turn rather than hang.
                let message = match next.unwrap_or(Turn::Say("done")) {
                    Turn::Say(text) => json!({ "role": "assistant", "content": text }),
                    Turn::Call { tool, args } => json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": format!("call-{tool}"),
                            "type": "function",
                            "function": { "name": tool, "arguments": args.to_string() }
                        }]
                    }),
                };
                Json(json!({
                    "choices": [{ "index": 0, "message": message }],
                    "usage": { "prompt_tokens": 12, "completion_tokens": 4 }
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), handle)
}

/// Serve the script on loopback and return its base URL.
pub(super) async fn spawn_script(turns: Vec<Turn>) -> String {
    spawn_script_recording(turns).await.0
}

/// A one-agent company that gates `shell`.
///
/// `always_approve` rather than `mode = "supervised"` deliberately: it parks
/// under **`full`** autonomy, which is the strongest form of the claim. A test
/// that only parks under `supervised` leaves open the reading that the
/// classifier did the work; this one shows the request is recorded on the queue
/// whatever the tier, which is exactly what
/// `always_approve_records_the_request_even_under_full_autonomy` pins one layer
/// down — and what this test then follows all the way to a journal card.
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

/// Deps with the production approvals wiring: a real gate over the manifest's
/// `[policy]` and a real on-disk journal, both reachable through the same
/// `delivery.parking` bundle the runtime builder wires.
pub(super) fn deps(base_url: String, dir: &std::path::Path) -> (HarnessDeps, Arc<RuntimeJournal>) {
    let policy = toml::from_str("mode = \"full\"\n").expect("valid [policy] block");
    let gate = Arc::new(crate::policy::ManifestApprovalGate::new(policy));
    let journal = Arc::new(RuntimeJournal::new(dir.join("journal.jsonl")));
    let deps = HarnessDeps {
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider: Arc::new(HostedProvider::new(HostedProviderConfig {
            base_url,
            credential: Credential::from_value("stub-key"),
            extra_headers: Vec::new(),
        })),
        provider_slug: "managed".to_string(),
        serves: None,
        context: Arc::new(FsContextStore::new(dir)),
        store: Arc::new(FsCompanyStore::new(dir)),
        meter: None,
        workspace_root: dir.to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.to_path_buf(),
        model_override: Some("stub-model".to_string()),
        tasks: None,
        artifacts: None,
        skills: None,
        skills_source_dir: None,
        skills_registry: std::sync::Arc::from([]),
        default_mcp_servers: Vec::new(),
        mcp_servers: Vec::new(),
        facts: None,
        events: None,
        delegations: DelegationQueue::default(),
        workflow_runner: WorkflowRunnerHandle::default(),
        mcp_failures: McpFailureQueue::default(),
        pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
        workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
        run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
        run_output_store: None,
        workflow_revisions: None,
        approval_requests: ApprovalRequestQueue::default(),
        secrets: None,
        web_allowed_domains: Vec::new(),
        capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
        workflow_source_dir: None,
        plan: None,
        media: None,
        composio: None,
        #[cfg(feature = "chargebee")]
        chargebee: None,
        #[cfg(feature = "paypal")]
        paypal: None,
        hosting: None,
        steer: crate::company::steer::InflightRegistry::default(),
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: Some(WorkflowDeliveryDeps {
            mail: None,
            inbox: Arc::new(FsInboxStore::new(dir)),
            users: Arc::new(FsOps::new(dir)),
            bootstrap_admin: None,
            channels: Vec::new(),
            parking: Some(DeliveryParking {
                approvals: gate,
                journal: journal.clone(),
                // Issue #978: a test fixture parks into its own queues. The
                // production wiring is `RuntimeBuilder`, which hands the
                // runtime's own handles in so a park arms what the resolve
                // path releases.
                continuations: Default::default(),
                gates: Default::default(),
                blocked_nodes: Default::default(),
            }),
            events: Arc::new(crate::store::FsEventLog::new(dir)),
        }),
        workspace: None,
        search: None,
        tenant_search: None,
        workflow_runs: None,
        deep_trace: None,
    };
    (deps, journal)
}

pub(super) fn record() -> CompanyRecord {
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

/// Runs the agent graph once against a script that explicitly requests approval and then
/// answers, returning the journal and the run's id.
async fn run_gated(dir: &std::path::Path) -> (Arc<RuntimeJournal>, HarnessDeps, String) {
    let base_url = spawn_script(vec![
        Turn::Call {
            tool: "request_approval",
            args: json!({
                "title": "Publish quarterly numbers",
                "question": "May I publish the quarterly numbers?"
            }),
        },
        Turn::Say("I was not allowed to run that."),
    ])
    .await;
    let (deps, journal) = deps(base_url, dir);
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(AGENT_GRAPH).expect("graph parses");
    let ctx = WorkflowRunContext::new(false);
    let run_id = ctx.run_id.clone();
    super::runner::run_workflow(
        pool,
        deps.clone(),
        &record,
        &file,
        json!({ "request": "quarterly numbers" }),
        &ctx,
    )
    .await
    // Issue #881 changed what "completes" means here: the node now BLOCKS, so
    // the run settles short of its output node. It still settles `Ok` — a node
    // waiting on a person is not a failed one — which is what this `expect`
    // pins. The parking claims below are unaffected: blocking the node must not
    // cost the operator the card, and `blocked_node_test` asserts the same
    // thing from the other side.
    .expect("the run settles — a gated tool blocks the node, it does not fail the run");
    (journal, deps, run_id)
}

/// The headline. A tool call gated inside a workflow agent node must leave a
/// card on the Approvals page — which reads the journal, so "on the page" means
/// "in `journal.pending()`".
#[tokio::test]
async fn an_explicit_approval_call_inside_a_workflow_node_parks() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _deps, run_id) = run_gated(dir.path()).await;

    let pending = journal.pending();
    let card = pending
        .iter()
        .find(|p| p.effect.kind == "request_approval")
        .unwrap_or_else(|| {
            panic!("the explicit request should be waiting on the operator: {pending:?}")
        });

    // `agent: Some` is what routes approval to a single-use grant and a
    // re-dispatch, rather than to the native executor — which for a tool call
    // would ledger a phantom spend and run nothing.
    assert_eq!(
        card.effect.agent.as_deref(),
        Some("ceo"),
        "the card must name the teammate that asked, so approving can re-issue the call"
    );
    // Issue #242's stamp, applied at the workflow run's boundary.
    assert_eq!(
        card.effect.run_id.as_deref(),
        Some(run_id.as_str()),
        "the card must name the run waiting on it"
    );
    // The tool's own arguments, verbatim — this is the thing being consented to.
    assert_eq!(card.effect.payload["title"], "Publish quarterly numbers");
}

/// The regression this issue is actually about. Parking is only half the fix:
/// the in-memory queue is emptied around **every** chat cycle, and before #395
/// a workflow's request sat on it waiting to be wiped by the next conversation.
/// A card in the journal is independent of that queue, and this asserts it.
///
/// Still worth pinning after issue #439, on narrower grounds. A workflow run's
/// entries now live in their own scope, so a chat cycle can no longer reach
/// them at all — but the property under test was never really about who could
/// reach the queue. It is that the journal is the durable record and the queue
/// is not, which is what makes the card survive *any* queue lifecycle.
#[tokio::test]
async fn the_explicit_request_survives_a_later_chat_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let (journal, deps, _run_id) = run_gated(dir.path()).await;
    assert!(
        journal
            .pending()
            .iter()
            .any(|p| p.effect.kind == "request_approval"),
        "precondition: the request parked"
    );

    // Stands in for the cycle's own queue lifecycle — `clear()` outside a claim
    // empties the unscoped bucket, and #439's `Cycle` claim clears on entry and
    // on drop. Either way the journal must not care.
    deps.approval_requests.clear();

    assert!(
        journal
            .pending()
            .iter()
            .any(|p| p.effect.kind == "request_approval"),
        "the operator's card must outlive the in-memory queue it came from — this is the \
         disappearance the issue reported"
    );
    assert_eq!(
        deps.approval_requests.queued(),
        0,
        "and nothing is left on the queue for a later cycle to park a second time"
    );
}

// ── Issue #453: a workflow node's board action is refused, not destroyed ────

/// The second reachability hole of exactly the #395 shape, one queue over.
///
/// A workflow `agent` node runs a full toolbelt turn, and an orchestrator-tier
/// `agent_ref` carries `review_task`. Nothing on this path drains the delegation
/// queue — there is no card behind a run and no conversation to answer into — so
/// before this the tool staged the verdict, answered *"Approved card X; it is
/// complete and has moved to done"*, and the next chat cycle's `clear()` threw
/// it away. Green tests, moved card in the transcript, unmoved card on the
/// board.
///
/// This drives the **real** path — real graph, real `run_workflow`, real
/// `HarnessAgentRunner`, real pool, real orchestrator tools — and asserts the
/// two things a unit test over the tool cannot: that the refusal is what a
/// workflow node actually receives, and that the queue is empty afterwards.
///
/// The refusal is read out of the *next* request the model was sent, because
/// that is where a tool result lands. Asserting on the tool's return value in
/// isolation would prove the string, not the reachability.
#[tokio::test]
async fn a_workflow_node_is_refused_in_turn_instead_of_having_its_verdict_destroyed() {
    let dir = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_recording(vec![
        Turn::Call {
            tool: "review_task",
            args: json!({ "task_id": "card-1", "decision": "approve" }),
        },
        Turn::Say("I could not approve that card."),
    ])
    .await;
    let (deps, _journal) = deps(base_url, dir.path());
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(AGENT_GRAPH).expect("graph parses");
    super::runner::run_workflow(
        pool,
        deps.clone(),
        &record,
        &file,
        json!({ "request": "approve the launch card" }),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect(
        "the run completes — a refused board action refuses the call, it does not fail the node. \
         It is NOT a #881 block either: a board refusal parks no approval, so the node stays ok",
    );

    // The model was told, in its own turn, that the card did NOT move.
    let seen = script.seen.lock().unwrap().clone();
    let transcript = serde_json::to_string(&seen).expect("serialize the requests");
    assert!(
        transcript.contains("card card-1 was NOT reviewed"),
        "the node's turn must carry the refusal: {transcript}"
    );
    assert!(
        transcript.contains("Do not retry"),
        "a workflow node drains no better next turn: {transcript}"
    );
    assert!(
        !transcript.contains("has moved to done"),
        "the receipt that made this a lie must be gone: {transcript}"
    );

    // …and nothing was staged for a later chat cycle to clear — the silent
    // destruction this replaces.
    assert_eq!(
        deps.delegations.queued(),
        0,
        "a workflow node must leave nothing on the shared delegation queue"
    );
    assert!(
        !deps.delegations.drain_committed(),
        "this path deliberately claims nothing; see `HarnessAgentRunner`'s doc"
    );
}

/// A workflow node whose turn calls nothing gated parks nothing. The drain must
/// be invisible to every run that was already working.
#[tokio::test]
async fn a_node_that_gates_nothing_parks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let base_url = spawn_script(vec![Turn::Say("Here are the numbers.")]).await;
    let (deps, journal) = deps(base_url, dir.path());
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(AGENT_GRAPH).expect("graph parses");
    super::runner::run_workflow(
        pool,
        deps,
        &record,
        &file,
        Value::Null,
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("the run completes");

    assert!(journal.pending().is_empty());
}
