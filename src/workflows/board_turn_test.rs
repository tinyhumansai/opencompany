//! Issue #661 / M5 — end-to-end proof that a workflow node can put work on the
//! board, and that everything it may **not** do stays refused.
//!
//! # Why these are turn tests rather than unit tests
//!
//! Every piece already existed and had tests. `SpawnTaskTool` staged a
//! delegation — tested. The queue scoped buckets per claimant — tested (#771).
//! `DelegationRunner`'s `SpawnTask` arm wrote a card — tested since #185. What
//! was missing was the *join*: nothing on the workflow path ever claimed or
//! drained the queue, so a run's `spawn_task` got an honest in-turn refusal and
//! the shipped `→ task cards` seed could not make a card. A unit test over any
//! one of those parts stays green through that.
//!
//! So these drive the **real** path — real graph, real `run_workflow`, real
//! `HarnessAgentRunner`, real `HarnessPool`, real orchestrator toolbelt, real
//! `TaskStore` — and stub exactly one thing, at the one boundary that needs a
//! credential: the model's *choices*, via a scripted OpenAI-compatible endpoint
//! on loopback. That is the shape
//! [`gated_tool_turn_test`](crate::workflows::gated_tool_turn_test) established
//! for #395, and it is reused here down to the `deps`/`record` fixtures.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

use crate::company::parse_workflow;
use crate::harness::HarnessPool;
use crate::ports::types::CompanyId;
use crate::ports::{RunCancel, TaskRecord, TaskStore, WorkflowBoardAction, WorkflowRunContext};
use crate::store::FsOps;

use super::gated_tool_turn_test::{Turn, record, spawn_script};

/// The one-agent graph these tests run: trigger → agent → output. The same
/// shape a company authors when it wants a teammate to do something on a
/// schedule, and the shape the shipped `→ task cards` seed has.
const AGENT_GRAPH: &str = r#"
id = "board"
name = "Board"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "work"
kind = "agent"
name = "Work"
summary = "Triage the inbox."
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

/// A parent graph whose only working node resolves a **child** workflow by id.
/// The child is the one with the agent node, so its `spawn_task` is what stamps
/// the card — see [`a_sub_workflow_childs_card_carries_the_parent_runs_ids`].
const PARENT_GRAPH: &str = r#"
id = "parent"
name = "Parent"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "child"
kind = "sub_workflow"
name = "Child"
[node.config]
workflow_id = "board"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "child"
[[edge]]
from = "child"
to = "done"
"#;

/// The company's board, wired onto `deps.tasks` the way the runtime builder
/// does — without it a run has nothing to write to.
fn tasks(dir: &std::path::Path) -> Arc<dyn TaskStore> {
    Arc::new(FsOps::new(dir))
}

/// Runs `graph` once against `turns`, with a real task board.
///
/// Returns the run's outcome, the board, and the run id, so a test can assert on
/// all three halves of the claim: what the run reported, what actually landed,
/// and that the two name the same run.
async fn run_with_board(
    dir: &std::path::Path,
    graph: &str,
    turns: Vec<Turn>,
    seed: Option<TaskRecord>,
) -> (crate::ports::WorkflowRun, Arc<dyn TaskStore>, String) {
    let base_url = spawn_script(turns).await;
    let (mut deps, _journal) = super::gated_tool_turn_test::deps(base_url, dir);
    let store = tasks(dir);
    deps.tasks = Some(store.clone());
    let record = record();
    if let Some(card) = seed {
        store
            .upsert(&record.id, &card)
            .await
            .expect("seed the card");
    }
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(graph).expect("graph parses");
    let ctx = WorkflowRunContext::new(false);
    let run_id = ctx.run_id.clone();
    let run = super::runner::run_workflow(
        pool,
        deps.clone(),
        &record,
        &file,
        json!({ "request": "triage the inbox" }),
        &ctx,
    )
    .await
    .expect("the run completes");
    (run, store, run_id)
}

/// A board card as the tests want to read it: one card, or a panic naming what
/// was there instead.
async fn only_card(store: &Arc<dyn TaskStore>, company: &CompanyId) -> TaskRecord {
    let cards = store.list(company).await.expect("list the board");
    assert_eq!(cards.len(), 1, "expected exactly one card, got {cards:?}");
    cards.into_iter().next().expect("checked above")
}

// ── 1. The headline: a run can open a card, stamped with the run ────────────

/// The whole point of M5. A workflow node calls `spawn_task`, and a card exists
/// on the board afterwards — in To-do, unparented, and carrying a reference back
/// to the run that opened it.
///
/// Four assertions and each is a separate claim:
///
/// * the card **exists** (the drain ran at all — before this the tool was
///   refused in-turn and nothing was written);
/// * it is in **To-do** (a run may open work, never move it — the column rule
///   that bounds run → card → dispatch → run cycles);
/// * `parent_task_id` / `origin_chat_id` are **None** (the lineage-root
///   decision: a run has no card and no conversation behind it, so machine
///   provenance is a reference rather than a parent);
/// * both origin ids are **stamped** (that reference actually being written —
///   without it the card is unexplained on the board).
#[tokio::test]
async fn a_workflow_node_opens_a_card_stamped_with_its_run() {
    let dir = tempfile::tempdir().unwrap();
    let (run, store, run_id) = run_with_board(
        dir.path(),
        AGENT_GRAPH,
        vec![
            Turn::Call {
                tool: "spawn_task",
                args: json!({ "title": "Reply to the auditor", "assignee": "ceo" }),
            },
            Turn::Say("Opened a card for it."),
        ],
        None,
    )
    .await;

    let card = only_card(&store, &CompanyId::new("acme")).await;
    assert_eq!(card.title, "Reply to the auditor");
    assert_eq!(
        card.column,
        crate::ports::tasks::COLUMN_TODO,
        "a run opens work; only an operator moves it"
    );
    assert_eq!(
        card.parent_task_id, None,
        "a run has no card behind it, so the card it opens is a lineage root"
    );
    assert_eq!(
        card.origin_chat_id, None,
        "a run has no conversation behind it, so there is nowhere to post back to"
    );
    assert_eq!(
        card.origin_run_id.as_deref(),
        Some(run_id.as_str()),
        "the card must name the run that opened it — this is the provenance M5 adds"
    );
    assert_eq!(
        card.origin_workflow_id.as_deref(),
        Some("board"),
        "and the workflow, so the card survives the journal being trimmed"
    );

    // …and the run reports it, which is how a console learns without polling
    // the board.
    assert_eq!(run.board.len(), 1, "one write, one row: {:?}", run.board);
    assert_eq!(run.board[0].action, WorkflowBoardAction::Spawned);
    assert_eq!(run.board[0].task_id.as_deref(), Some(card.id.as_str()));
    assert_eq!(run.board[0].title.as_deref(), Some("Reply to the auditor"));
    assert_eq!(run.board[0].assignee.as_deref(), Some("ceo"));
}

// ── 2. Assign sets the owner and moves nothing ─────────────────────────────

/// A run may set who owns an existing card. It may **not** move it, and the note
/// it leaves must not be attributed to the CEO.
///
/// The column assertion is the load-bearing one: `todo → planning` is an
/// operator drag and `planning → in_progress` is the dispatch gate, so run →
/// card → dispatch → run cycles are bounded precisely because every dispatch
/// needs a person. A run that could move a column would take that bound with it.
#[tokio::test]
async fn a_workflow_node_assigns_an_existing_card_without_moving_it() {
    let dir = tempfile::tempdir().unwrap();
    let seed = TaskRecord {
        id: "card-1".to_string(),
        title: "Quarterly close".to_string(),
        note: None,
        column: crate::ports::tasks::COLUMN_TODO.to_string(),
        priority: "medium".to_string(),
        assignee: String::new(),
        updated_at_millis: 1,
        origin_chat_id: None,
        parent_task_id: None,
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
    };
    let (run, store, _run_id) = run_with_board(
        dir.path(),
        AGENT_GRAPH,
        vec![
            Turn::Call {
                tool: "assign_task",
                args: json!({ "task_id": "card-1", "assignee": "ceo", "note": "you own this" }),
            },
            Turn::Say("Assigned it."),
        ],
        Some(seed),
    )
    .await;

    let card = only_card(&store, &CompanyId::new("acme")).await;
    assert_eq!(card.assignee, "ceo", "the owner must actually be written");
    assert_eq!(
        card.column,
        crate::ports::tasks::COLUMN_TODO,
        "assignment records ownership and NEVER starts the work — the loop bound"
    );
    let note = card.note.unwrap_or_default();
    assert!(
        note.contains("[workflow:board]"),
        "the note must be in the workflow's voice, not the CEO's: {note}"
    );
    assert!(
        !note.contains("[ceo]"),
        "attributing a machine write to a person is the misattribution this fixes: {note}"
    );

    assert_eq!(run.board.len(), 1, "{:?}", run.board);
    assert_eq!(run.board[0].action, WorkflowBoardAction::Assigned);
    assert_eq!(run.board[0].task_id.as_deref(), Some("card-1"));
    assert_eq!(run.board[0].assignee.as_deref(), Some("ceo"));
}

// ── 3. A failed write reports, and does not fail the node ──────────────────

/// A [`TaskStore`] whose `upsert` always fails, so the drain's error arm is
/// reachable without corrupting anything.
struct FailingTasks;

#[async_trait::async_trait]
impl TaskStore for FailingTasks {
    async fn list(&self, _company: &CompanyId) -> crate::Result<Vec<TaskRecord>> {
        Ok(Vec::new())
    }
    async fn upsert(&self, _company: &CompanyId, _task: &TaskRecord) -> crate::Result<()> {
        Err(crate::error::OpenCompanyError::Harness(
            "the board is unavailable".to_string(),
        ))
    }
    async fn delete(&self, _company: &CompanyId, _id: &str) -> crate::Result<bool> {
        Ok(false)
    }
}

/// The requirement `execute_board_writes` holds by signature: a board write that
/// fails must never fail the node that made it.
///
/// The turn already happened and the graph is mid-walk; discarding a completed
/// node's work over a store hiccup would be the worst available trade. So the
/// run **succeeds**, and the failure is loud in the two places an operator
/// actually reads — a `spawnFailed` row and a run notice.
#[tokio::test]
async fn a_board_write_that_fails_reports_a_row_and_does_not_fail_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let base_url = spawn_script(vec![
        Turn::Call {
            tool: "spawn_task",
            args: json!({ "title": "Reply to the auditor" }),
        },
        Turn::Say("Opened a card for it."),
    ])
    .await;
    let (mut deps, _journal) = super::gated_tool_turn_test::deps(base_url, dir.path());
    deps.tasks = Some(Arc::new(FailingTasks));
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(AGENT_GRAPH).expect("graph parses");
    let run = super::runner::run_workflow(
        pool,
        deps,
        &record,
        &file,
        Value::Null,
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("a failed board write must NOT fail the run");

    assert_eq!(run.board.len(), 1, "{:?}", run.board);
    assert_eq!(
        run.board[0].action,
        WorkflowBoardAction::SpawnFailed,
        "the row must say the write did not land"
    );
    assert_eq!(
        run.board[0].task_id, None,
        "no card exists, so naming one would point at a card that is not on the board"
    );
    assert!(
        run.notices.iter().any(|n| n.contains("could not")),
        "the operator needs telling: {:?}",
        run.notices
    );
}

// ── 4. An ungrounded hand-off surfaces on the run's OWN notices ────────────

/// The live half of the defect PR #771 identified, closed end-to-end.
///
/// `DelegateToDeskTool` calls `push_refusal` **before** it consults the claim, so
/// a workflow node naming a desk the company does not have wrote into the shared
/// `refused` vector — and a concurrent chat turn's `drain_refusals` took it,
/// recorded the hand-off on *that* turn's card, and cleared it. A hand-off
/// attempt nobody on that turn made, attributed to that turn, and destroyed for
/// the run that actually made it.
///
/// Two assertions, and they are different facts: the refusal reaches **this
/// run's** notices, and the `Unscoped` bucket a chat turn drains is left
/// untouched.
#[tokio::test]
async fn an_ungrounded_hand_off_surfaces_on_the_runs_own_notices() {
    let dir = tempfile::tempdir().unwrap();
    let base_url = spawn_script(vec![
        Turn::Call {
            tool: "delegate_to_desk",
            args: json!({ "desk": "legal", "instruction": "review the contract" }),
        },
        Turn::Say("I could not hand that off."),
    ])
    .await;
    let (mut deps, _journal) = super::gated_tool_turn_test::deps(base_url, dir.path());
    deps.tasks = Some(tasks(dir.path()));
    let record = record();
    // The record has to be READABLE from the store, because `delegate_to_desk`
    // grounds its target against the company's real desks at call time. Without
    // it the orchestrator fails open (issue #272's deliberate stance) and the
    // hand-off is refused by the board claim instead — which is a different
    // refusal, on a path that records nothing, and would leave this test green
    // for the wrong reason.
    crate::ports::CompanyStore::save(&*deps.store, &record)
        .await
        .expect("the company record is readable");
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(AGENT_GRAPH).expect("graph parses");
    let run = super::runner::run_workflow(
        pool,
        deps.clone(),
        &record,
        &file,
        Value::Null,
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("the run completes");

    assert!(
        run.notices.iter().any(|n| n.contains("legal")),
        "the run that attempted the hand-off is the one that must hear about it: {:?}",
        run.notices
    );
    // Nothing was left in the bucket a chat turn drains — which is where this
    // used to land and be stolen from.
    assert!(
        deps.delegations
            .drain_refusals(crate::harness::orchestrator::MAX_DELEGATIONS_PER_TURN)
            .is_empty(),
        "an unscoped chat turn must find nothing of this run's to record on its own card"
    );
    // And the hand-off itself never became a card: a run has nowhere to put a
    // synchronous reply, so the tool refused before staging anything.
    assert!(run.board.is_empty(), "{:?}", run.board);
}

// ── 7. A dry run writes nothing, by construction ───────────────────────────

/// Issue #542's discipline applied to the board: a dry run of a graph whose node
/// calls `spawn_task` must leave no card, no row and no journal line.
///
/// **Empty by construction rather than by a check.** The dry bundle wires
/// `DryRunAgent`, so `HarnessAgentRunner` — the only thing that ever takes a
/// board claim or drains one — is never built. This pins that, so a future edit
/// that moved the claim out of the live arm would be caught rather than
/// discovered by a test run mutating a real board.
#[tokio::test]
async fn a_dry_run_of_a_spawning_graph_writes_no_card() {
    let dir = tempfile::tempdir().unwrap();
    let base_url = spawn_script(vec![
        Turn::Call {
            tool: "spawn_task",
            args: json!({ "title": "Reply to the auditor" }),
        },
        Turn::Say("Opened a card for it."),
    ])
    .await;
    let (mut deps, journal) = super::gated_tool_turn_test::deps(base_url, dir.path());
    let store = tasks(dir.path());
    deps.tasks = Some(store.clone());
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(AGENT_GRAPH).expect("graph parses");
    let mut ctx = WorkflowRunContext::new(false);
    ctx.dry_run = true;
    let run = super::runner::run_workflow(pool, deps, &record, &file, Value::Null, &ctx)
        .await
        .expect("the dry run completes");

    assert!(run.board.is_empty(), "a test run reports no board writes");
    assert!(
        store.list(&record.id).await.expect("list").is_empty(),
        "a test run must not put a card on a real board"
    );
    assert!(
        journal.pending().is_empty(),
        "a test run journals nothing at all"
    );
}

// ── 8. A sub-workflow child stamps the PARENT run's ids ────────────────────

/// Pins the finding that contradicts the original design: a `sub_workflow`
/// child has **no run identity of its own**.
///
/// `StoreWorkflowResolver` resolves and runs the child *inside the engine under
/// the parent's capability bundle* — same runner, same `run_id`, same
/// collectors — and a child journals no `WorkflowRunStarted`/`Finished` pair
/// (which is also why issue #617 exists). So the parent's run id is the only run
/// identity in existence on that path, and the only run row a console can
/// navigate to.
///
/// The design said a child's cards would reference the child run. They cannot.
/// This pins the actual behaviour rather than leaving it to be rediscovered:
/// the card carries the **parent's** run id and the **parent's** workflow id.
#[tokio::test]
async fn a_sub_workflow_childs_card_carries_the_parent_runs_ids() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("company");
    std::fs::create_dir_all(source.join("workflows")).unwrap();
    std::fs::write(source.join("workflows").join("board.toml"), AGENT_GRAPH).unwrap();

    let base_url = spawn_script(vec![
        Turn::Call {
            tool: "spawn_task",
            args: json!({ "title": "Child's card" }),
        },
        Turn::Say("Opened a card for it."),
    ])
    .await;
    let (mut deps, _journal) = super::gated_tool_turn_test::deps(base_url, dir.path());
    let store = tasks(dir.path());
    deps.tasks = Some(store.clone());
    deps.workflow_source_dir = Some(source);
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(PARENT_GRAPH).expect("graph parses");
    let ctx = WorkflowRunContext::new(false);
    let parent_run = ctx.run_id.clone();
    let run = super::runner::run_workflow(pool, deps, &record, &file, Value::Null, &ctx)
        .await
        .expect("the parent run completes");

    let card = only_card(&store, &record.id).await;
    assert_eq!(
        card.origin_run_id.as_deref(),
        Some(parent_run.as_str()),
        "a child runs under the parent's bundle and has no run id of its own — the parent's is \
         the only one a console can navigate to"
    );
    assert_eq!(
        card.origin_workflow_id.as_deref(),
        Some("parent"),
        "and the workflow id is the parent's for the same reason: the bundle was built for it"
    );
    // The rows land on the parent's run too — one collector, one run.
    assert_eq!(run.board.len(), 1, "{:?}", run.board);
    assert_eq!(run.board[0].action, WorkflowBoardAction::Spawned);
}

// ── The claim never reaches the chat path ──────────────────────────────────

/// A run's claim is on its own scope, so the bucket a chat cycle claims is
/// untouched — the property #771's scoping bought, asserted from the workflow
/// side now that the workflow side actually claims something.
#[tokio::test]
async fn a_runs_board_claim_leaves_the_unscoped_bucket_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (run, _store, _run_id) = run_with_board(
        dir.path(),
        AGENT_GRAPH,
        vec![
            Turn::Call {
                tool: "spawn_task",
                args: json!({ "title": "Reply to the auditor" }),
            },
            Turn::Say("Opened a card for it."),
        ],
        None,
    )
    .await;
    assert_eq!(run.board.len(), 1, "precondition: the run wrote the board");

    // A fresh queue read from an unscoped task: nothing committed, nothing
    // staged. The run's claim was released with its bundle and never named this
    // bucket in the first place.
    let queue = crate::harness::orchestrator::DelegationQueue::default();
    assert!(
        !queue.drain_committed(),
        "a chat turn must still find the unscoped bucket unclaimed"
    );
}

/// A [`Script`]-driven turn that calls nothing writes no rows. The drain must be
/// invisible to every run that was already working.
#[tokio::test]
async fn a_node_that_touches_no_card_reports_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (run, store, _run_id) = run_with_board(
        dir.path(),
        AGENT_GRAPH,
        vec![Turn::Say("Nothing to do.")],
        None,
    )
    .await;
    assert!(run.board.is_empty());
    assert!(
        store
            .list(&CompanyId::new("acme"))
            .await
            .expect("list")
            .is_empty()
    );
}

// ── 5. A cancelled run's card survives, and stays listed ───────────────────

/// The scripted endpoint again, but with a hook fired on each request — the one
/// thing the shared helper cannot do, and the only deterministic way to cancel a
/// run at a known point in its graph walk.
///
/// The hook fires *inside* the model call, so by the time it returns the node's
/// turn is still running: the drain below it has not happened yet, and the
/// engine has not reached its next node boundary. That ordering is what makes
/// this a **clean** cancel of a run that has already written the board, rather
/// than a race.
async fn spawn_script_with_hook(
    turns: Vec<Turn>,
    hook: Arc<dyn Fn(usize) + Send + Sync>,
) -> String {
    let turns = Arc::new(Mutex::new(turns));
    let seen = Arc::new(Mutex::new(0usize));
    let app = axum::Router::new().route(
        "/chat/completions",
        post(move |Json(_body): Json<Value>| {
            let turns = Arc::clone(&turns);
            let seen = Arc::clone(&seen);
            let hook = Arc::clone(&hook);
            async move {
                let n = {
                    let mut seen = seen.lock().unwrap();
                    *seen += 1;
                    *seen
                };
                hook(n);
                let next = {
                    let mut turns = turns.lock().unwrap();
                    if turns.is_empty() {
                        None
                    } else {
                        Some(turns.remove(0))
                    }
                };
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
    format!("http://{addr}")
}

/// A card is real the moment the drain writes it, so stopping the run must not
/// unmake it — and must not un-say it either.
///
/// This is the semantic touchpoint #675's cancel work needed agreeing: an
/// operator who stops a run still has the card in front of them, so a cancelled
/// run that drops its rows leaves a card on the board that no run admits to
/// opening. The run reports `cancelled` **and** the row.
#[tokio::test]
async fn a_cancelled_runs_card_survives_and_stays_listed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = WorkflowRunContext::new(false);
    let run_id = ctx.run_id.clone();
    let cancel: RunCancel = ctx.cancel.clone();

    // Fired on the model's SECOND call — the one that closes the node's turn
    // after the tool result came back. The turn is still in flight, so the drain
    // that writes the card still runs; the engine sees the flipped token at the
    // next node boundary and winds down cleanly.
    let base_url = spawn_script_with_hook(
        vec![
            Turn::Call {
                tool: "spawn_task",
                args: json!({ "title": "Reply to the auditor" }),
            },
            Turn::Say("Opened a card for it."),
        ],
        Arc::new(move |n| {
            if n == 2 {
                cancel.cancel();
            }
        }),
    )
    .await;

    let (mut deps, _journal) = super::gated_tool_turn_test::deps(base_url, dir.path());
    let store = tasks(dir.path());
    deps.tasks = Some(store.clone());
    let record = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&record, &deps).await.expect("roster builds");

    let file = parse_workflow(AGENT_GRAPH).expect("graph parses");
    let run = super::runner::run_workflow(pool, deps, &record, &file, Value::Null, &ctx)
        .await
        .expect("a stopped run settles rather than failing");

    assert!(run.cancelled, "precondition: the operator stopped this run");
    let card = only_card(&store, &record.id).await;
    assert_eq!(
        card.origin_run_id.as_deref(),
        Some(run_id.as_str()),
        "the card is a durable write and outlives the stop"
    );
    assert_eq!(
        run.board.len(),
        1,
        "and the stopped run must still LIST it — otherwise the board shows a card no run admits \
         to opening: {:?}",
        run.board
    );
    assert_eq!(run.board[0].action, WorkflowBoardAction::Spawned);
}
