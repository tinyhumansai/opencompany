//! Integration tests for the `ops` write plane: tasks, memory, workspace,
//! skills, team, inbox-read, and desk chat — exercised end-to-end over the
//! router against a real fs-backed company.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::company::steer::{InflightEntry, InflightKind};
use crate::ports::facts::{FactKind, FactRecord};
use crate::ports::tasks::{TaskRecord, TaskTitle};
use crate::ports::types::{CompanyId, CompanyRecord, CompressedTrace, ContextChunk};
use crate::runtime::RuntimeBuilder;
use crate::runtime::journal::{ApprovalConversation, TaskLink};
use crate::server::router;
use crate::store::FsCompanyStore;
use crate::{AppConfig, AppState};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("opencompany-ops-")
        .tempdir()
        .expect("tempdir")
}

fn manifest() -> CompanyManifest {
    toml::from_str(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
    )
    .unwrap()
}

/// The sorted node names in a workspace tree body.
///
/// A freshly-built company is no longer an empty tree: boot scaffolds the
/// reserved `agents/` and `desks/` roots (issue #551), so the tests below name
/// what they expect rather than counting to zero. Nothing is provisioned
/// *inside* them — a member folder is minted when that agent or desk first
/// produces something.
fn provisioned_names(tree: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = tree
        .as_array()
        .expect("the tree read is an array")
        .iter()
        .map(|node| node["name"].as_str().unwrap_or_default().to_string())
        .collect();
    names.sort();
    names
}

async fn state_with_company(home: &std::path::Path) -> AppState {
    state_with_quota(home, crate::runtime::WorkspaceQuota::default()).await
}

/// [`state_with_company`], with the workspace held to `quota`.
///
/// Parameterised rather than duplicated so the one test that needs a non-default
/// `[workspace] max_blob_mb` (issue #647) exercises the same wiring every other
/// test here does, instead of a second harness that could drift from it.
async fn state_with_quota(
    home: &std::path::Path,
    quota: crate::runtime::WorkspaceQuota,
) -> AppState {
    state_with(home, quota, None).await
}

/// [`state_with_company`], with the workspace tree served by `workspace`
/// (issue #759).
///
/// The `fs` backend refuses to create two sibling nodes with one name
/// (`reject_path_collision`, issue #665), so the raced tree the repair route
/// exists to fix cannot be built through it. sqlite and mongodb — the backends
/// hosted tenants run, and the reason the state exists at all — accept it, and
/// this swaps in a double that behaves the same way. Everything else about the
/// harness is unchanged, so the route under test is the one the console calls.
async fn state_with_workspace(
    home: &std::path::Path,
    workspace: std::sync::Arc<dyn crate::ports::workspace::WorkspaceStore>,
) -> AppState {
    state_with(
        home,
        crate::runtime::WorkspaceQuota::default(),
        Some(workspace),
    )
    .await
}

async fn state_with(
    home: &std::path::Path,
    quota: crate::runtime::WorkspaceQuota,
    workspace: Option<std::sync::Arc<dyn crate::ports::workspace::WorkspaceStore>>,
) -> AppState {
    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
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
        })
        .await
        .unwrap();
    let mut builder = RuntimeBuilder::new(home.to_path_buf(), manifest())
        .with_id(id.clone())
        .with_workspace_quota(quota);
    if let Some(workspace) = workspace {
        builder = builder.with_workspace(workspace);
    }
    let runtime = builder.build().await.unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, std::sync::Arc::new(runtime));
    // Every route needs a principal now; the harness signs in as an admin so
    // tests keep asserting write behavior rather than auth.
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

/// The repo's shared skill library (`<crate root>/skills`), the same directory
/// the serve path derives `skills_root` from.
fn repo_skills_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills")
}

/// Like [`state_with_company`], but with the repo's shared skill library wired
/// in, so registry reads and server-authoritative installs resolve against real
/// documents instead of degrading to the empty-registry fallback.
async fn state_with_registry(home: &std::path::Path) -> AppState {
    // `with_skills_root` consumes and returns the state, so the registered
    // company and seeded admin move along with it.
    state_with_company(home)
        .await
        .with_skills_root(repo_skills_root())
}

/// The operator deltas persisted for `acme` — the durable rows behind the API.
async fn persisted_skills(state: &AppState) -> Vec<crate::ports::skills_state::SkillState> {
    let runtime = state
        .registry()
        .get(&CompanyId::new("acme"))
        .expect("company");
    runtime.skills().list(runtime.id()).await.expect("deltas")
}

async fn send(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    send_auth(state, method, uri, body, None).await
}

async fn send_auth(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    } else {
        // No explicit credential: sign in as the harness admin. Every route
        // needs a principal now, so an unauthenticated request would only ever
        // assert 401 rather than the behavior under test.
        request = request.header("cookie", crate::server::test_support::fixed_cookie("acme"));
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// A headline that normalises away is refused, not persisted (coderabbit on
/// #2055).
///
/// The `"""` bug's sibling, one layer out. That one was a *model* reply peeling
/// to a lone quote; this is a person typing punctuation into the title field.
/// The junk test that catches the first deliberately does not apply here — a
/// person who names a card `🚀` means it — so the boundary that persists the
/// card is what has to refuse a name with nothing in it, on both routes that
/// can set one.
#[tokio::test]
async fn a_title_that_normalises_to_nothing_is_refused_on_create_and_rename() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // Non-blank on the wire, and nothing left after the sentence-punctuation
    // strip — so a length check on the raw input passes it through.
    for junk in ["...", "!!!", " . . . "] {
        let (status, _body) = send(
            &state,
            "POST",
            "/api/v1/company/tasks",
            Some(json!({ "title": junk })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "create accepted {junk:?}");
    }

    // A symbol a person plainly meant is still a title, and still lands.
    let (status, rocket) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({ "title": "🚀" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rocket["title"], "🚀");
    let id = rocket["id"].as_str().unwrap().to_string();

    // …and the rename route refuses the same junk rather than blanking it.
    let (status, _body) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/tasks/{id}"),
        Some(json!({ "title": "..." })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    let still = board
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id.as_str());
    assert_eq!(
        still.map(|t| &t["title"]),
        Some(&json!("🚀")),
        "a refused rename leaves the card named as it was"
    );
}

#[tokio::test]
async fn tasks_crud_round_trips_under_both_scopes() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // Create via the single-company alias.
    let (status, task) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Q2 brief", "priority": "high"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["title"], "Q2 brief");
    // Issue #206/#301: manual entry lands in To-do, the board's one intake lane
    // — which reads as `pending`, its phase, since issue #1512.
    assert_eq!(task["column"], "pending");
    let id = task["id"].as_str().unwrap().to_string();

    // Drag (PATCH column) via the {id} scope.
    let (status, moved) = send(
        &state,
        "PATCH",
        &format!("/api/v1/companies/acme/tasks/{id}"),
        Some(json!({"column": "done"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(moved["column"], "done");

    // List (GET) reflects the write — the board the console reads.
    let (status, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = board.as_array().expect("array of cards");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], id);
    assert_eq!(rows[0]["column"], "done");

    // Delete.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/tasks/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Second delete is a 404.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/tasks/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// #205: a card may only be assigned to somebody the company actually has.
/// Before this the board's free-text Assignee field accepted anything, the bad
/// value was persisted verbatim, and dispatch silently handed the work to the
/// orchestrator instead.
#[tokio::test]
async fn task_writes_reject_an_off_roster_assignee() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Fetch my activity", "assignee": "Shane"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.to_string().contains("Shane"),
        "the refusal must name what was typed: {body}"
    );

    // A roster teammate is fine, matched case-insensitively…
    let (status, task) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Q2 brief", "assignee": "CEO"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = task["id"].as_str().unwrap().to_string();

    // …and so is blank — an unassigned card is not an error.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Unowned"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The same rule on PATCH, and the rejected patch leaves the card untouched.
    let (status, _) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/tasks/{id}"),
        Some(json!({"title": "Renamed", "assignee": "Shane"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    let card = board
        .as_array()
        .expect("board")
        .iter()
        .find(|c| c["id"] == json!(id))
        .expect("the card survives a rejected patch")
        .clone();
    assert_eq!(
        card["assignee"], "ceo",
        "the typed key is stored as the canonical roster id"
    );
    assert_eq!(
        card["title"], "Q2 brief",
        "a rejected patch must not persist the fields it did apply"
    );
}

/// #205: a column the board does not render is refused too. A typo'd
/// `in-progress` used to be persisted verbatim, hiding the card from every
/// rendered column *and* — since only the exact literal `in_progress`
/// edge-fires a dispatch — silently never running it.
#[tokio::test]
async fn task_writes_reject_a_column_the_board_cannot_render() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Typo'd", "column": "in-progress"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.to_string().contains("working"),
        "the refusal must list the columns that do exist: {body}"
    );

    let (status, task) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Fine", "column": "paused"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = task["id"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/tasks/{id}"),
        Some(json!({"column": "reviewing"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    // The refused patch left the card where it was: paused, which reads as the
    // `working` phase with `paused` named as the stage (issue #1512).
    let card = &board.as_array().expect("board")[0];
    assert_eq!(card["column"], "working");
    assert_eq!(card["stage"], "paused");
}

/// #334: `in_review → done` is a move the write boundary accepts, and the one
/// the board's drag actually sends.
///
/// QA reported that a card "cannot be moved out of In review" — the drop did
/// nothing and said nothing, which cannot distinguish a host refusing the write
/// from a console that never sent it. It was the console (the drop was missing
/// the mostly off-window last column, and every miss was silent), but nothing
/// pinned the host's half of that answer. This does: both columns are in
/// `BOARD_COLUMNS`, the transition is special-cased nowhere, and `done` is
/// terminal — the card lands there and no dispatch fires behind it.
#[tokio::test]
async fn a_card_moves_from_in_review_to_done() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, seeded) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Invoice March retainer", "column": "in_review"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = seeded["id"].as_str().unwrap().to_string();

    // Exactly the body a drag onto Done sends.
    let (status, moved) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/tasks/{id}"),
        Some(json!({"column": "done"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the board's drag PATCH must be accepted: {moved}"
    );
    assert_eq!(moved["column"], crate::ports::tasks::COLUMN_DONE);

    // What the board reads back on its next poll, not just what the echo said —
    // a card that snaps back is the shape of the original report.
    let (_, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    let card = board
        .as_array()
        .expect("board")
        .iter()
        .find(|c| c["id"] == json!(id))
        .expect("the card is still on the board");
    assert_eq!(card["column"], crate::ports::tasks::COLUMN_DONE);
}

/// Issue #206: `POST …/tasks` defaults a new card to To-do — the board's one
/// manual-entry column — while an explicit `column` still wins, so the
/// lifecycle paths that place a card themselves are untouched.
///
/// Issue #301 kept the default and reshaped what "explicit" may say: `planning`
/// is now a column (inert, but the write boundary must accept it before §4's
/// auto-advance starts writing it), and the removed `backlog` pool is refused.
#[tokio::test]
async fn created_tasks_default_to_the_todo_column() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (_, defaulted) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "queued work"})),
    )
    .await;
    assert_eq!(defaulted["column"], crate::ledger::board::PHASE_PENDING);

    // An explicit column is still honored verbatim — `spawn_task`, the
    // orchestrator's `revise`, and a failed run all place their own card.
    let (status, explicit) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "being planned", "column": "planning"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(explicit["column"], crate::ledger::board::PHASE_WORKING);
    assert_eq!(explicit["stage"], crate::ports::tasks::COLUMN_PLANNING);

    // Issue #301: `backlog` is gone from the board, so a client still writing
    // it is refused rather than persisting a card nothing renders. Legacy data
    // heals silently on read (`ports::tasks`), but a *write* fails loudly — the
    // error names the set that replaced it.
    let (status, refused) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "parked", "column": "backlog"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        refused.to_string().contains("pending"),
        "the refusal must name the columns that replaced it: {refused}"
    );

    // …and the same on a drag, so a stale console cannot move a card into the
    // removed column either.
    let id = defaulted["id"].as_str().unwrap().to_string();
    let (status, _) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/tasks/{id}"),
        Some(json!({"column": "backlog"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Issue #246: `POST …/tasks` carries the thread a card was opened from.
///
/// `TaskRecord.origin_chat_id` has existed since #151 and the tool-spawn path
/// stamped it, but this handler hardcoded `None` and no DTO projected it — so a
/// card opened from a conversation had no way back to it, and #151's
/// "answer where you were asked" post-back could never fire for anything the
/// REST surface created. Both halves are checked here: the write keeps it, and
/// **both** reads (the board list and task detail) hand it back.
#[tokio::test]
async fn a_task_created_from_a_thread_remembers_and_reads_back_that_thread() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Ship the brief", "originChatId": "strategy"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["originChatId"], "strategy");
    let id = created["id"].as_str().unwrap().to_string();

    // The board read (what the console lists) carries it…
    let (_, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    assert_eq!(board.as_array().unwrap()[0]["originChatId"], "strategy");

    // …and so does task detail, which is where an operator asks "where did
    // this come from?".
    let (status, detail) = send(&state, "GET", &format!("/api/v1/company/tasks/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["task"]["originChatId"], "strategy");

    // A card created without a thread — the board's `+` button — omits the key
    // entirely rather than sending null, so the pre-#246 wire shape is
    // unchanged for every card that has no conversation behind it.
    let (_, plain) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Typed on the board"})),
    )
    .await;
    assert!(
        plain.get("originChatId").is_none(),
        "a card with no originating thread must not grow the key: {plain}"
    );

    // A blank thread id is normalised away rather than persisted as a thread
    // that matches nothing.
    let (_, blank) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Blank origin", "originChatId": "   "})),
    )
    .await;
    assert!(blank.get("originChatId").is_none(), "{blank}");
}

/// A card raised inside a thread carries the thread root on both the task
/// detail and board wire responses, not only on the stored `TaskRecord`.
///
/// `originParent` is stamped by the tool-spawn path rather than the REST
/// create body, so the record is seeded directly here.
#[tokio::test]
async fn a_task_detail_response_carries_the_thread_root_of_a_threaded_origin() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();

    runtime
        .tasks()
        .upsert(
            &company,
            &TaskRecord {
                id: "threaded".into(),
                title: TaskTitle::authored("Ship the brief"),
                note: None,
                column: crate::ports::tasks::COLUMN_TODO.into(),
                priority: "medium".into(),
                assignee: String::new(),
                updated_at_millis: 1,
                origin: crate::ports::tasks::TaskOrigin::new(
                    Some("strategy".to_string()),
                    Some(crate::ports::types::EventSeq::new(41)),
                ),
                parent_task_id: None,
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: None,
                bounced: None,
            },
        )
        .await
        .unwrap();

    let (status, detail) = send(&state, "GET", "/api/v1/company/tasks/threaded", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["task"]["originChatId"], "strategy");
    assert_eq!(detail["task"]["originParent"], 41, "{detail}");

    let (_, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    let card = board
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "threaded")
        .unwrap();
    assert_eq!(card["originParent"], 41, "{card}");
}

/// Issue #339: the card's output link reaches the console on **both** reads.
///
/// The board read matters as much as task detail here, and that is the whole
/// point: the link is rendered on the card, so a board that had to open every
/// card to discover what it produced would cost N reads per four-second poll.
///
/// A card that never succeeded omits the key entirely rather than sending
/// `null`, so the pre-#339 wire shape is unchanged for every card the board
/// created — which is also what the console reads as "link to the card itself".
#[tokio::test]
async fn a_stamped_card_hands_its_output_link_to_both_reads() {
    use crate::ports::artifacts::ArtifactKind;
    use crate::ports::tasks::{
        TaskOutput, TaskOutputAction, TaskOutputArtifact, TaskOutputSource, TaskOutputWorkflow,
    };

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // A card the board created: no attempt has run, so no link.
    let (_, plain) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Typed on the board"})),
    )
    .await;
    assert!(
        plain.get("output").is_none(),
        "a card that never succeeded must not grow the key: {plain}"
    );
    let id = plain["id"].as_str().unwrap().to_string();

    // Stamp it the way a successful settle does, through the plain store port.
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();
    let mut card = runtime
        .tasks()
        .list(&company)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == id)
        .expect("card");
    card.output = Some(TaskOutput {
        source: TaskOutputSource::Run {
            run_id: "run-2".to_string(),
            attempt: Some(2),
        },
        at_millis: 42,
        artifacts: vec![TaskOutputArtifact {
            artifact_id: "a-1".to_string(),
            version: 3,
            title: "Launch spec".to_string(),
            kind: ArtifactKind::Markdown,
        }],
        workflows: vec![TaskOutputWorkflow {
            workflow_id: "digest".to_string(),
            run_id: Some("wf-1".to_string()),
            action: TaskOutputAction::Ran,
        }],
    });
    runtime.tasks().upsert(&company, &card).await.unwrap();

    // The board read — the one the card's own link is rendered from.
    let (_, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    let listed = board
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == json!(id))
        .expect("the card is on the board");
    assert_eq!(listed["output"]["runId"], "run-2");
    assert_eq!(listed["output"]["attempt"], 2);
    assert_eq!(listed["output"]["artifacts"][0]["artifactId"], "a-1");
    assert_eq!(
        listed["output"]["artifacts"][0]["version"], 3,
        "the link must carry the version the run wrote, not just the record"
    );
    assert_eq!(listed["output"]["artifacts"][0]["kind"], "markdown");
    assert_eq!(listed["output"]["workflows"][0]["workflowId"], "digest");
    assert_eq!(listed["output"]["workflows"][0]["action"], "ran");

    // …and task detail, where the operator opens what the link points at.
    let (status, detail) = send(&state, "GET", &format!("/api/v1/company/tasks/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["task"]["output"]["runId"], "run-2");
    assert_eq!(detail["task"]["output"]["artifacts"][0]["version"], 3);
}

/// Issue #246 spend gate, at the HTTP boundary. The transcript's "Add to
/// board" action omits `column` on purpose so the *server* decides where a
/// chat-created card lands — and the one thing that must never happen is that
/// it lands on the dispatch trigger, which spends an agent turn nobody
/// approved. `dispatch_task` is a no-op in this build (no harness attached),
/// so the load-bearing assertion is the landing column itself; the journal
/// check is the belt to that braces, and would catch a create that started
/// journaling a dispatch of its own.
#[tokio::test]
async fn a_chat_created_card_lands_off_the_dispatch_trigger() {
    use crate::ports::tasks::{COLUMN_IN_PROGRESS, COLUMN_TODO};
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Draft the announcement", "originChatId": "main"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Asserted on `stage`, not on `column`: since issue #1512 the DTO's
    // `column` is the phase, and `working` covers both the dispatched stage and
    // three that are not — so a phase assertion could not tell "dispatched"
    // from "being planned", which is the only thing this test is about.
    assert_ne!(
        created["stage"], COLUMN_IN_PROGRESS,
        "a chat-created card must never arrive already dispatched"
    );
    assert_eq!(
        created["column"],
        crate::ledger::board::PHASE_PENDING,
        "it lands in the board's intake lane, where the human drag is the gate"
    );
    assert_eq!(created["stage"], serde_json::Value::Null, "{created}");
    let _ = COLUMN_TODO;

    // Issue #301 added a second pre-dispatch column and made To-do the only
    // intake lane, so the spend gate is re-checked across every creation shape
    // the board can produce: the bare board `+` (which now sends nothing but a
    // prompt-derived title) and an explicit `planning`. Neither may dispatch —
    // `planning` in particular is *not* a dispatch trigger, which is the whole
    // reason it can ship inert ahead of §4's auto-advance.
    for body in [
        json!({"title": "Typed on the board"}),
        json!({"title": "Being planned", "column": "planning"}),
    ] {
        let (status, created) = send(&state, "POST", "/api/v1/company/tasks", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_ne!(created["stage"], COLUMN_IN_PROGRESS, "{created}");
    }

    let journal = runtime
        .events()
        .read_from(
            runtime.id(),
            crate::ports::types::EventSeq::new(0),
            usize::MAX,
        )
        .await
        .unwrap();
    assert!(
        !journal
            .iter()
            .any(|e| matches!(e.event, CompanyEvent::TaskDispatched { .. })),
        "creating a card must not dispatch it, whichever pre-dispatch column it lands in"
    );
}

#[tokio::test]
async fn steer_task_validates_statuses_and_journals_acceptance() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();
    let endpoint = |key: &str| format!("/api/v1/company/tasks/{key}/steer");

    for body in [
        json!({"action": "unknown"}),
        json!({"action": "cancel"}),
        json!({"action": "redirect", "instruction": "   "}),
    ] {
        let (status, _) = send(&state, "POST", &endpoint("missing"), Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    runtime
        .tasks()
        .upsert(
            &company,
            &TaskRecord {
                id: "idle".into(),
                title: TaskTitle::authored("Idle"),
                note: None,
                column: crate::ports::tasks::COLUMN_TODO.into(),
                priority: "medium".into(),
                assignee: String::new(),
                updated_at_millis: 1,
                origin: None,
                parent_task_id: None,
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: None,
                bounced: None,
            },
        )
        .await
        .unwrap();
    let (status, _) = send(
        &state,
        "POST",
        &endpoint("idle"),
        Some(json!({"action": "pause"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = send(
        &state,
        "POST",
        &endpoint("missing"),
        Some(json!({"action": "pause"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _delegation = runtime.steer().register(
        &company,
        InflightEntry {
            key: "delegation".into(),
            task_id: None,
            kind: InflightKind::Delegation,
            title: "Engineering".into(),
            agent_id: "ceo".into(),
            started_at_millis: 1,
            pending_action: None,
        },
    );
    let (status, _) = send(
        &state,
        "POST",
        &endpoint("delegation"),
        Some(json!({"action": "pause"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _task = runtime.steer().register(
        &company,
        InflightEntry {
            key: "active".into(),
            task_id: Some("active".into()),
            kind: InflightKind::Task,
            title: "Active".into(),
            agent_id: "ceo".into(),
            started_at_millis: 2,
            pending_action: None,
        },
    );
    let (status, _) = send(
        &state,
        "POST",
        &endpoint("active"),
        Some(json!({"action": "redirect", "instruction": "focus on the API"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let events = runtime
        .events()
        .read_from(&company, crate::ports::types::EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    assert!(events.iter().any(|stored| matches!(
        &stored.event,
        crate::ports::types::CompanyEvent::TaskSteered {
            task_id,
            action,
            instruction: Some(instruction),
            ..
        } if task_id == "active" && action == "redirect" && instruction == "focus on the API"
    )));
}

#[tokio::test]
async fn memory_create_and_delete_journals_event() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, fact) = send(
        &state,
        "POST",
        "/api/v1/company/memory",
        Some(json!({"kind": "preference", "title": "Tone", "body": "Warm"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fact["kind"], "preference");
    let id = fact["id"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/memory/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn memory_traces_are_inspectable_newest_last() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    for (cycle_id, summary, at_millis) in [
        ("cycle-1", "first completed cycle", 1_000),
        ("cycle-2", "second completed cycle", 2_000),
    ] {
        runtime
            .memory
            .save_trace(
                runtime.id(),
                CompressedTrace {
                    cycle_id: cycle_id.into(),
                    summary: summary.into(),
                    at_millis,
                },
            )
            .await
            .unwrap();
    }

    let (status, traces) = send(&state, "GET", "/api/v1/company/memory/traces", None).await;
    assert_eq!(status, StatusCode::OK);
    let traces = traces.as_array().unwrap();
    assert_eq!(traces.len(), 2);
    assert_eq!(traces[0]["cycleId"], "cycle-1");
    assert_eq!(traces[0]["summary"], "first completed cycle");
    assert_eq!(traces[0]["atMillis"], 1_000);
    assert_eq!(traces[1]["cycleId"], "cycle-2");
}

#[tokio::test]
async fn memory_list_filters_stats_and_dual_write() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    // Seed three facts with controlled, distinct timestamps so newest-first is
    // deterministic (the HTTP create path stamps `now_millis`, which can tie
    // across rapid inserts). Seeding straight into the FactStore also means
    // these do NOT create ContextStore mirrors — only the HTTP create path does.
    let seed = [
        ("f-old", FactKind::Fact, "Alpha channel report", 1_000u64),
        ("f-mid", FactKind::Preference, "Warm tone", 2_000),
        ("f-new", FactKind::Person, "Priya contact", 3_000),
    ];
    for (id, kind, title, ts) in seed {
        runtime
            .facts()
            .upsert(
                runtime.id(),
                &FactRecord {
                    id: id.into(),
                    kind,
                    title: title.into(),
                    body: "detail".into(),
                    source: "Seed".into(),
                    updated_at_millis: ts,
                },
            )
            .await
            .unwrap();
    }

    // List reflects the store, newest-first.
    let (status, rows) = send(&state, "GET", "/api/v1/company/memory", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = rows["items"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id"], "f-new");
    assert_eq!(rows[2]["id"], "f-old");

    // `?kind=` narrows to one taxonomy.
    let (status, pref) = send(
        &state,
        "GET",
        "/api/v1/company/memory?kind=preference",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pref = pref["items"].as_array().unwrap();
    assert_eq!(pref.len(), 1);
    assert_eq!(pref[0]["id"], "f-mid");

    // `?query=` is a case-insensitive substring over title + body.
    let (status, hit) = send(&state, "GET", "/api/v1/company/memory?query=priya", None).await;
    assert_eq!(status, StatusCode::OK);
    let hit = hit["items"].as_array().unwrap();
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0]["id"], "f-new");

    // Stats over the seeded facts: 3 display items, freshest timestamp, no
    // teammate memory yet (seeding bypassed the mirror), and 0 task outcomes.
    let (status, stats) = send(&state, "GET", "/api/v1/company/memory/stats", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["facts"], 3);
    assert_eq!(stats["factsUpdatedAtMillis"], 3_000);
    assert_eq!(stats["totalItems"], 3);
    assert_eq!(stats["teammateMemory"], 0);
    assert_eq!(stats["taskOutcomes"], 0);
    assert_eq!(stats["documentMemory"], 0);
    // Nothing but facts so far, so "Last updated" tracks the newest fact.
    assert_eq!(stats["lastUpdatedAtMillis"], 3_000);

    // Dual-write: the HTTP create path mirrors the fact into the ContextStore so
    // the agent can recall it. A direct search finds the mirrored text — the
    // fix that closes the operator manual-ingest loop.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/memory",
        Some(json!({"kind": "fact", "title": "Launch date", "body": "ships on Friday"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hits = runtime
        .context
        .search(runtime.id(), "Friday", 5)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.snippet.contains("ships on Friday")),
        "an operator fact must be mirrored into the ContextStore for agent recall"
    );

    // The mirror stays agent-recallable but is not a display item of teammate
    // memory — the fact is the one row the operator sees.
    let (status, stats) = send(&state, "GET", "/api/v1/company/memory/stats", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["facts"], 4);
    assert_eq!(stats["totalItems"], 4);
    assert_eq!(stats["teammateMemory"], 0);
    assert_eq!(stats["taskOutcomes"], 0);
    assert_eq!(stats["documentMemory"], 0);
}

/// The Brain's "Last updated" stat must move when *agents* write memory, not
/// only when the operator hand-authors a fact.
///
/// The reported bug (#153): agent memory and task outcomes land exclusively in
/// the `ContextStore`, and the stat was computed from the `FactStore` alone —
/// so a company whose agents were actively remembering, but whose operator had
/// never added a fact, showed "—" forever.
#[tokio::test]
async fn memory_stats_last_updated_covers_agent_written_context() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    // A brand-new company remembers nothing: the stat is genuinely empty, and
    // "—" is the honest rendering.
    let (status, stats) = send(&state, "GET", "/api/v1/company/memory/stats", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["facts"], 0);
    assert_eq!(stats["totalItems"], 0);
    assert_eq!(stats["teammateMemory"], 0);
    assert_eq!(
        stats["lastUpdatedAtMillis"], 0,
        "no memory of any kind yet, so the stat has nothing to report"
    );

    // Now an agent writes memory — no operator fact anywhere in sight. This is
    // the exact state that used to pin the stat at 0.
    let before = crate::ports::now_millis();
    for (label, body) in [
        ("agent-ceo/notes", "the launch slipped to Friday"),
        ("task-outcome/agent-ceo", "Task: ship it\nOutcome: done"),
    ] {
        runtime
            .context
            .put(
                runtime.id(),
                ContextChunk {
                    label: label.to_string(),
                    body: body.to_string(),
                },
            )
            .await
            .unwrap();
    }

    let (status, stats) = send(&state, "GET", "/api/v1/company/memory/stats", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["facts"], 0, "still no operator facts");
    assert_eq!(stats["totalItems"], 2);
    assert_eq!(stats["teammateMemory"], 1);
    assert_eq!(stats["taskOutcomes"], 1);
    assert_eq!(stats["documentMemory"], 0);
    assert_eq!(
        stats["factsUpdatedAtMillis"], 0,
        "the facts-only figure is unchanged — it is simply not the whole story"
    );
    let last_updated = stats["lastUpdatedAtMillis"].as_u64().unwrap();
    assert!(
        last_updated >= before,
        "agent-written memory must move the Brain's Last updated stat, got {last_updated}"
    );

    // An operator fact newer than any chunk takes over the stat.
    runtime
        .facts()
        .upsert(
            runtime.id(),
            &FactRecord {
                id: "f-future".into(),
                kind: FactKind::Fact,
                title: "Board meeting".into(),
                body: "moved to Monday".into(),
                source: "You".into(),
                updated_at_millis: last_updated + 60_000,
            },
        )
        .await
        .unwrap();
    let (status, stats) = send(&state, "GET", "/api/v1/company/memory/stats", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stats["lastUpdatedAtMillis"],
        last_updated + 60_000,
        "the stat is the max across every memory source, whichever is freshest"
    );
    assert_eq!(stats["totalItems"], 3);

    // The list surfaces the same stamps per row, so a context card no longer
    // renders "—" while the header claims recent activity.
    let (status, rows) = send(&state, "GET", "/api/v1/company/memory", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = rows["items"].as_array().unwrap();
    let context_rows: Vec<&Value> = rows.iter().filter(|r| r["origin"] != "fact").collect();
    assert_eq!(context_rows.len(), 2);
    assert!(
        context_rows
            .iter()
            .all(|r| r["updatedAt"].as_u64().unwrap() >= before),
        "each agent-written row carries the time it was stored"
    );
}

/// End-to-end proof that the dual-write closes the manual-ingest loop: an
/// operator note written over HTTP is retrieved by the harness's ContextStore
/// search and rendered by `memory_loop::inject` into the augmented prompt. Gated
/// on `openhuman` because `memory_loop` is only compiled under that feature.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn memory_operator_fact_is_injected_into_the_agent_turn() {
    use crate::harness::memory_loop;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    // Operator adds a note through the console write path.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/memory",
        Some(json!({"kind": "reference", "title": "Launch plan", "body": "we ship on Friday at noon"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The harness retrieve step searches the ContextStore; the mirror lands
    // there, so a relevant next-turn message recalls it and `inject` renders it
    // into the augmented prompt — the closed loop, end to end.
    // The fs ContextStore search is substring-based, so query a token that
    // appears verbatim in the stored `title\nbody` mirror.
    let hits = runtime
        .context
        .search(runtime.id(), "Friday", memory_loop::RETRIEVE_TOP_K)
        .await
        .unwrap();
    assert!(!hits.is_empty(), "the operator note must be retrievable");
    let augmented = memory_loop::inject("when do we ship?", &hits);
    assert!(augmented.contains("Relevant prior work"));
    assert!(augmented.contains("we ship on Friday at noon"));
    assert!(augmented.trim_end().ends_with("when do we ship?"));
}

/// Two-company isolation over HTTP: company B never sees company A's facts, and
/// a tenant token may not address a company it does not own (403) — the same
/// scoped-auth boundary the credential route enforces.
#[tokio::test]
async fn memory_is_isolated_between_companies() {
    use crate::server::platform_auth::{
        PlatformAuthConfig, PlatformClaims, UnsignedTenantVerifier,
    };
    use std::collections::HashSet;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let verifier = std::sync::Arc::new(UnsignedTenantVerifier::new("plat-secret"));
    let state = AppState::new(AppConfig::default())
        .with_home(home.clone())
        .with_platform_auth(PlatformAuthConfig::new(verifier));

    for name in ["a", "b"] {
        let id = CompanyId::new(name);
        let runtime = RuntimeBuilder::new(home.clone(), manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        state
            .registry()
            .insert(id.clone(), std::sync::Arc::new(runtime));
        state.set_owner(id.clone(), format!("tenant:{name}"));
    }

    let token = |tenant: &str| {
        UnsignedTenantVerifier::tenant_token(&PlatformClaims {
            tenant: tenant.to_string(),
            scopes: HashSet::from(["operator".to_string()]),
            companies: None,
        })
    };

    // Company A's owner writes a fact to A.
    let (status, _) = send_auth(
        &state,
        "POST",
        "/api/v1/companies/a/memory",
        Some(json!({"kind": "fact", "title": "A secret", "body": "A body"})),
        Some(&token("tenant:a")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Company B's owner sees an empty memory — A's fact is invisible to B.
    let (status, list_b) = send_auth(
        &state,
        "GET",
        "/api/v1/companies/b/memory",
        None,
        Some(&token("tenant:b")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list_b["items"].as_array().unwrap().len(), 0);

    // A's own memory holds exactly the one fact.
    let (status, list_a) = send_auth(
        &state,
        "GET",
        "/api/v1/companies/a/memory",
        None,
        Some(&token("tenant:a")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list_a["items"].as_array().unwrap().len(), 1);

    // A's token may not address B's memory at all — 403 (scoped auth).
    let (status, _) = send_auth(
        &state,
        "GET",
        "/api/v1/companies/b/memory",
        None,
        Some(&token("tenant:a")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn workspace_create_write_move_and_cycle_rejection() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (_, folder) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "Brand", "kind": "folder"})),
    )
    .await;
    let folder_id = folder["id"].as_str().unwrap().to_string();

    let (status, file) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "voice.md", "kind": "file", "parentId": folder_id, "content": "# Voice"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let file_id = file["id"].as_str().unwrap().to_string();

    // Overwrite content.
    let (status, ack) = send(
        &state,
        "PUT",
        &format!("/api/v1/company/workspace/file/{file_id}"),
        Some(json!({"content": "# Voice v2"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(ack["updatedAt"].is_number());

    // Explicit `"parentId": null` moves the file back to the workspace root.
    let (status, moved) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/workspace/{file_id}"),
        Some(json!({"parentId": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        moved.get("parentId").is_none(),
        "node moved to root has no parentId"
    );

    // Cycle rejection: move a folder under its own child.
    let (_, child) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "Sub", "kind": "folder", "parentId": folder_id})),
    )
    .await;
    let child_id = child["id"].as_str().unwrap().to_string();
    let (status, body) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/workspace/{folder_id}"),
        Some(json!({"parentId": child_id})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    // Recursive delete.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/workspace/{folder_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// Issue #666 applies to every way the filesystem path can change, not only to
/// creates. A rename that could alias a sibling is refused before either the
/// index or either file body moves.
#[tokio::test]
async fn workspace_rename_cannot_claim_a_siblings_physical_path() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (_, first) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "first.md", "kind": "file", "content": "first body"})),
    )
    .await;
    let first_id = first["id"].as_str().unwrap().to_string();
    let (_, second) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "second.md", "kind": "file", "content": "second body"})),
    )
    .await;
    let second_id = second["id"].as_str().unwrap().to_string();

    let (status, refusal) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/workspace/{second_id}"),
        Some(json!({"name": "first.md"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
    assert_eq!(refusal["code"], "conflict", "{refusal}");

    for (id, name, content) in [
        (&first_id, "first.md", "first body"),
        (&second_id, "second.md", "second body"),
    ] {
        let (status, file) = send(
            &state,
            "GET",
            &format!("/api/v1/company/workspace/file/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{file}");
        assert_eq!(file["name"], name, "the refused rename changed metadata");
        assert_eq!(
            file["content"], content,
            "the refused rename moved or overwrote a sibling body"
        );
    }
}

/// The read plane the console's Workspace tab runs on (issue #177): the tree
/// `GET` reflects writes, and the file `GET` carries content plus
/// server-computed backlinks.
///
/// Before this the only workspace read was GraphQL, which the console has no
/// client for — so the tab rendered a localStorage fixture and never saw a note
/// an agent (or another browser) wrote.
#[tokio::test]
async fn workspace_tree_and_file_reads_reflect_writes() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // A workspace with nothing seeded into it reads as a real tree, not a 404
    // and not a fixture. It is not *empty*, though: boot scaffolds the reserved
    // `agents/` root and operator-only `secrets/README.md`. The manifest here has an agent and it gets
    // no folder — a member folder is minted on first use, not on joining the
    // roster. `desks/` is absent for the same reason since issue #645: nothing
    // writes into it, so it is minted on first use rather than scaffolded.
    let (status, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        provisioned_names(&tree),
        vec!["agents", "artifacts", "readme.md", "readme.md", "secrets"],
        "a fresh company starts with its system scaffold and nothing else"
    );
    let provisioned = tree.as_array().unwrap().len();

    let (_, folder) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "standards", "kind": "folder"})),
    )
    .await;
    let folder_id = folder["id"].as_str().unwrap().to_string();

    let (_, voice) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({
            "name": "voice.md",
            "kind": "file",
            "parentId": folder_id,
            "content": "# Voice\n\nWarm and concise.",
        })),
    )
    .await;
    let voice_id = voice["id"].as_str().unwrap().to_string();

    // A second note links to the first, so it must show up as its backlink.
    let (_, brief) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({
            "name": "brief.md",
            "kind": "file",
            "content": "Follows our [[voice]].",
        })),
    )
    .await;
    let brief_id = brief["id"].as_str().unwrap().to_string();

    // The tree carries every node's metadata — and deliberately no bodies, so a
    // navigation read never grows with the size of the workspace.
    let (status, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    assert_eq!(status, StatusCode::OK);
    let tree = tree.as_array().unwrap();
    assert_eq!(tree.len(), provisioned + 3);
    for node in tree {
        assert!(
            node.get("content").is_none(),
            "the tree read must not ship note bodies"
        );
        assert!(node["updatedAt"].is_number());
    }
    let listed = tree
        .iter()
        .find(|node| node["id"] == json!(voice_id))
        .expect("the created note is in the tree");
    assert_eq!(listed["name"], "voice.md");
    assert_eq!(listed["kind"], "file");
    assert_eq!(listed["parentId"], json!(folder_id));
    // Authorship rides every node of the tree read (issue #326). These routes
    // are the console's, so the console is the operator.
    assert_eq!(listed["createdBy"], json!({"kind": "operator"}));
    assert_eq!(listed["updatedBy"], json!({"kind": "operator"}));
    // …and the scaffold's own nodes say what they are, so the console can tell
    // "the runtime laid this down" from "somebody wrote this".
    let root = tree
        .iter()
        .find(|node| node["name"] == json!("agents"))
        .expect("the Agents root is in the tree");
    assert_eq!(root["createdBy"], json!({"kind": "seed"}));
    assert_eq!(root["kind"], json!("folder"));
    assert!(root["parentId"].is_null());

    // The file read carries the body and the inbound backlink, computed server
    // side — the console derives neither.
    let (status, file) = send(
        &state,
        "GET",
        &format!("/api/v1/company/workspace/file/{voice_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(file["name"], "voice.md");
    assert!(
        file["content"]
            .as_str()
            .unwrap()
            .contains("Warm and concise")
    );
    assert!(file["updatedAt"].is_number());
    assert_eq!(file["createdBy"], json!({"kind": "operator"}));
    assert_eq!(file["updatedBy"], json!({"kind": "operator"}));
    let backlinks = file["backlinks"].as_array().unwrap();
    assert_eq!(backlinks.len(), 1);
    assert_eq!(backlinks[0]["id"], json!(brief_id));
    assert_eq!(backlinks[0]["name"], "brief.md");

    // An out-of-band write (an agent, or another browser) is visible on the very
    // next read — the whole point of the tab reading the store.
    let (status, _) = send(
        &state,
        "PUT",
        &format!("/api/v1/company/workspace/file/{voice_id}"),
        Some(json!({"content": "# Voice\n\nRewritten elsewhere."})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, file) = send(
        &state,
        "GET",
        &format!("/api/v1/company/workspace/file/{voice_id}"),
        None,
    )
    .await;
    assert!(
        file["content"]
            .as_str()
            .unwrap()
            .contains("Rewritten elsewhere")
    );

    // A folder id and an unknown id are both 404 — never an empty note.
    let (status, body) = send(
        &state,
        "GET",
        &format!("/api/v1/company/workspace/file/{folder_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "company_not_found");

    let (status, _) = send(
        &state,
        "GET",
        "/api/v1/company/workspace/file/does-not-exist",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Two-company isolation over the workspace read plane: company A's notes are
/// invisible to B, and a tenant token may not address a company it does not own.
/// The store is per-company by construction — this pins that the new `GET`s do
/// not widen it.
#[tokio::test]
async fn workspace_reads_are_isolated_between_companies() {
    use crate::server::platform_auth::{
        PlatformAuthConfig, PlatformClaims, UnsignedTenantVerifier,
    };
    use std::collections::HashSet;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let verifier = std::sync::Arc::new(UnsignedTenantVerifier::new("plat-secret"));
    let state = AppState::new(AppConfig::default())
        .with_home(home.clone())
        .with_platform_auth(PlatformAuthConfig::new(verifier));

    for name in ["a", "b"] {
        let id = CompanyId::new(name);
        let runtime = RuntimeBuilder::new(home.clone(), manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        state
            .registry()
            .insert(id.clone(), std::sync::Arc::new(runtime));
        state.set_owner(id.clone(), format!("tenant:{name}"));
    }

    let token = |tenant: &str| {
        UnsignedTenantVerifier::tenant_token(&PlatformClaims {
            tenant: tenant.to_string(),
            scopes: HashSet::from(["operator".to_string()]),
            companies: None,
        })
    };

    let (status, note) = send_auth(
        &state,
        "POST",
        "/api/v1/companies/a/workspace",
        Some(json!({"name": "secret.md", "kind": "file", "content": "A body"})),
        Some(&token("tenant:a")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let note_id = note["id"].as_str().unwrap().to_string();

    // B's own workspace holds only its own scaffolded system root — A's note
    // is not in it.
    let (status, tree_b) = send_auth(
        &state,
        "GET",
        "/api/v1/companies/b/workspace",
        None,
        Some(&token("tenant:b")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        provisioned_names(&tree_b),
        vec!["agents", "artifacts", "readme.md", "readme.md", "secrets"]
    );

    // Even naming A's node id explicitly, B's scope does not resolve it.
    let (status, _) = send_auth(
        &state,
        "GET",
        &format!("/api/v1/companies/b/workspace/file/{note_id}"),
        None,
        Some(&token("tenant:b")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And A's token may not address B's workspace at all — 403 (scoped auth).
    let (status, _) = send_auth(
        &state,
        "GET",
        "/api/v1/companies/b/workspace",
        None,
        Some(&token("tenant:a")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// `GET …/workspace/search` (issue #607): the hit body, both scope forms, and
/// the two refusals stated rather than guessed.
#[tokio::test]
async fn workspace_search_returns_hits_with_paths_and_excerpts() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (_, folder) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "standards", "kind": "folder"})),
    )
    .await;
    let folder_id = folder["id"].as_str().unwrap().to_string();
    let (_, note) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({
            "name": "Support.md",
            "kind": "file",
            "parentId": folder_id,
            "content": "# Support\n\nEscalate a REFUND request to the CEO."
        })),
    )
    .await;
    let note_id = note["id"].as_str().unwrap().to_string();

    // A content hit carries the path the tree view would have to derive, the
    // excerpt, the origins the console badges, and what matched.
    let (status, results) = send(
        &state,
        "GET",
        "/api/v1/company/workspace/search?q=refund",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(results["total"], json!(1));
    let hits = results["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["id"], json!(note_id));
    assert_eq!(hits[0]["path"], "standards/support.md");
    assert_eq!(hits[0]["matched"], "content");
    assert_eq!(hits[0]["kind"], "file");
    assert_eq!(hits[0]["updatedBy"], json!({"kind": "operator"}));
    assert!(
        hits[0]["excerpt"].as_str().unwrap().contains("REFUND"),
        "{:?}",
        hits[0]["excerpt"]
    );

    // A folder is a hit in its own right, matched by name and with no excerpt
    // promising a body it does not have.
    let (status, results) = send(
        &state,
        "GET",
        "/api/v1/company/workspace/search?q=standards",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hits = results["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["id"], json!(folder_id));
    assert_eq!(hits[0]["kind"], "folder");
    assert_eq!(hits[0]["matched"], "name");
    assert!(hits[0].get("excerpt").is_none(), "{:?}", hits[0]);

    // `prefix` scopes to a subtree.
    let (status, scoped) = send(
        &state,
        "GET",
        "/api/v1/company/workspace/search?q=support&prefix=standards",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(scoped["total"], json!(1));
    let (_, elsewhere) = send(
        &state,
        "GET",
        "/api/v1/company/workspace/search?q=support&prefix=Desks",
        None,
    )
    .await;
    assert_eq!(elsewhere["total"], json!(0));

    // Both refusals are 400 and say what is wrong. An empty `q` is NOT "match
    // everything" — a cleared search box must not fetch the whole tree.
    for uri in [
        "/api/v1/company/workspace/search",
        "/api/v1/company/workspace/search?q=",
        "/api/v1/company/workspace/search?q=%20%20",
        "/api/v1/company/workspace/search?q=refund&limit=0",
    ] {
        let (status, body) = send(&state, "GET", uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} → {body}");
        assert_eq!(body["code"], "invalid_request", "{uri} → {body}");
    }

    // The route resolves under the platform scope form too, and `search` is
    // never captured as a node id by the `…/workspace/{node_id}` route.
    let (status, results) = send(
        &state,
        "GET",
        "/api/v1/companies/acme/workspace/search?q=refund",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(results["total"], json!(1));
}

/// `POST …/workspace/sweep-empty-agent-folders` (issue #700): the operator's
/// one-time tidy of the empty `agents/<id>/` folders a pre-#570 company still
/// carries.
///
/// The whole route in one test, because the halves only mean something together:
/// the dry run has to name every folder *and* leave the tree alone, or the
/// confirm dialog it feeds is either uninformative or a lie; the real run has to
/// remove exactly those folders, leave the occupied one, and announce each
/// removal so a console watching the feed sees the tree change rather than
/// discovering it on the next refetch.
#[tokio::test]
async fn workspace_sweep_previews_then_removes_only_the_empty_agent_folders() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    // Boot already scaffolded `agents/`; find it rather than making a rival.
    let (_, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    let agents_id = tree
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "agents")
        .expect("boot scaffolds the Agents root")["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Two strays from the #551 era, one folder that actually holds a
    // deliverable, and a note filed directly under the root by an operator.
    let mut empty = Vec::new();
    for id in ["ceo", "cto"] {
        let (_, folder) = send(
            &state,
            "POST",
            "/api/v1/company/workspace",
            Some(json!({"name": id, "kind": "folder", "parentId": agents_id})),
        )
        .await;
        empty.push(folder["id"].as_str().unwrap().to_string());
    }
    let (_, cmo) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "cmo", "kind": "folder", "parentId": agents_id})),
    )
    .await;
    send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({
            "name": "launch-brief.md",
            "kind": "file",
            "parentId": cmo["id"].as_str().unwrap(),
            "content": "# Launch",
        })),
    )
    .await;
    send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({
            "name": "README.md",
            "kind": "file",
            "parentId": agents_id,
            "content": "# who is who",
        })),
    )
    .await;

    let before = {
        let (_, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
        provisioned_names(&tree)
    };
    let events_before = journal_len(&runtime).await;

    // -- the preview ------------------------------------------------------
    let (status, preview) = send(
        &state,
        "POST",
        "/api/v1/company/workspace/sweep-empty-agent-folders?dry_run=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        swept_names(&preview["wouldRemove"]),
        vec!["ceo", "cto"],
        "the confirm dialog needs every folder named, not a count: {preview}"
    );
    assert!(
        preview.get("removed").is_none(),
        "a preview must not claim it removed anything: {preview}"
    );
    let (_, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    assert_eq!(
        provisioned_names(&tree),
        before,
        "a dry run must leave the tree exactly as it found it"
    );
    assert_eq!(
        journal_len(&runtime).await,
        events_before,
        "a dry run must not announce anything either"
    );

    // -- the real thing ---------------------------------------------------
    let (status, done) = send(
        &state,
        "POST",
        "/api/v1/company/workspace/sweep-empty-agent-folders",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        swept_names(&done["removed"]),
        vec!["ceo", "cto"],
        "an operator who disagrees needs to know what went: {done}"
    );
    assert!(
        done.get("wouldRemove").is_none(),
        "a real run must not answer in the preview's field: {done}"
    );

    let (_, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    assert_eq!(
        provisioned_names(&tree),
        vec![
            "agents".to_string(),
            "artifacts".to_string(),
            "cmo".to_string(),
            "launch-brief.md".to_string(),
            "readme.md".to_string(),
            "readme.md".to_string(),
            "readme.md".to_string(),
            "secrets".to_string(),
        ],
        "the folder holding a deliverable, the operator's note and the root all stay"
    );

    // One `WorkspaceChanged{removed}` per folder — the announcer is reached
    // because the handler deletes through `runtime.workspace()`, the same
    // wrapped handle the per-node delete uses (issue #327).
    let journal = runtime
        .events()
        .read_from(
            runtime.id(),
            crate::ports::types::EventSeq::new(0),
            usize::MAX,
        )
        .await
        .unwrap();
    //
    // Sorted on both sides rather than compared in creation order: the sweep
    // walks whatever order `tree()` returns, and the port promises none.
    let mut announced: Vec<&str> = journal
        .iter()
        .filter_map(|stored| match &stored.event {
            CompanyEvent::WorkspaceChanged { node_id, change } if change == "removed" => {
                Some(node_id.as_str())
            }
            _ => None,
        })
        .collect();
    announced.sort_unstable();
    let mut expected: Vec<&str> = empty.iter().map(String::as_str).collect();
    expected.sort_unstable();
    assert_eq!(
        announced, expected,
        "each removal announces itself, exactly once, and nothing else was announced removed"
    );

    // -- and again, which must be a no-op ---------------------------------
    let (status, again) = send(
        &state,
        "POST",
        "/api/v1/company/workspace/sweep-empty-agent-folders",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        again["removed"],
        json!([]),
        "running it twice must remove nothing the second time: {again}"
    );

    // The route resolves under the platform scope form too, and is never
    // captured as a node id by the `…/workspace/{node_id}` route.
    let (status, scoped) = send(
        &state,
        "POST",
        "/api/v1/companies/acme/workspace/sweep-empty-agent-folders?dry_run=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(scoped["wouldRemove"], json!([]));
}

/// The sorted folder names in a sweep response list.
fn swept_names(list: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = list
        .as_array()
        .unwrap_or_else(|| panic!("the sweep answers with a list, got {list}"))
        .iter()
        .map(|folder| folder["name"].as_str().unwrap_or_default().to_string())
        .collect();
    names.sort();
    names
}

/// `POST …/workspace/merge-duplicate-folders` (issue #759): the operator's
/// repair for a tree a publish race already left ambiguous.
///
/// The whole route in one test, because the halves only mean anything together.
/// A preview that did not name the relocations is a confirm dialog nobody can
/// agree to; a real run that reported only its successes would call a tree fixed
/// while two rival documents still sit on one path; and a repair that could not
/// be run twice would be useless precisely on the tenant that needs it, since
/// the first pass deliberately leaves the file collision behind.
///
/// The workspace here is the permissive double, not `FsOps`: the `fs` backend
/// refuses to create the duplicate in the first place (issue #665), so this
/// state is only reachable on the sqlite and mongodb backends hosted tenants
/// actually run.
#[tokio::test]
async fn workspace_merge_folds_duplicate_folders_and_reports_the_file_collision() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_workspace(
        &home,
        std::sync::Arc::new(
            crate::company::workspace_repair::loose_store::LooseWorkspace::default(),
        ),
    )
    .await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    async fn make(state: &AppState, body: Value) -> String {
        let (status, node) = send(state, "POST", "/api/v1/company/workspace", Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{node}");
        node["id"].as_str().expect("an id").to_string()
    }

    // The raced state: one deliverable folder published twice, each copy
    // holding a different note — and both holding a `summary.md`, which is two
    // documents on one path and the thing no merge may decide.
    let a = make(&state, json!({"name": "reports", "kind": "folder"})).await;
    let b = make(&state, json!({"name": "reports", "kind": "folder"})).await;
    let a_note = make(
        &state,
        json!({"name": "q1.md", "kind": "file", "parentId": a, "content": "# Q1"}),
    )
    .await;
    let b_note = make(
        &state,
        json!({"name": "q2.md", "kind": "file", "parentId": b, "content": "# Q2"}),
    )
    .await;
    let a_summary = make(
        &state,
        json!({"name": "summary.md", "kind": "file", "parentId": a, "content": "# Mine"}),
    )
    .await;
    let b_summary = make(
        &state,
        json!({"name": "summary.md", "kind": "file", "parentId": b, "content": "# Theirs"}),
    )
    .await;

    let before = {
        let (_, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
        provisioned_names(&tree)
    };
    let events_before = journal_len(&runtime).await;

    // -- the preview --------------------------------------------------------
    let (status, preview) = send(
        &state,
        "POST",
        "/api/v1/company/workspace/merge-duplicate-folders?dry_run=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let folds = preview["wouldMerge"]
        .as_array()
        .unwrap_or_else(|| panic!("a preview answers with a list of folds, got {preview}"));
    assert_eq!(folds.len(), 1, "{preview}");
    let fold = &folds[0];

    // Which twin survives is derived rather than hard-coded: two ULIDs minted
    // in the same millisecond order by their random half, so the harness cannot
    // know in advance which folder is older. The rule itself — oldest wins, node
    // id breaks the tie — is pinned in `company::workspace_repair`'s own tests,
    // where the timestamps are given.
    let loser = fold["id"]
        .as_str()
        .expect("the fold names its loser")
        .to_string();
    let winner = fold["intoId"]
        .as_str()
        .expect("and its survivor")
        .to_string();
    assert!(
        (loser == a && winner == b) || (loser == b && winner == a),
        "the fold must be between the two `reports` folders, got {fold}"
    );
    let (moved, residual) = if loser == a {
        (a_note.clone(), a_summary.clone())
    } else {
        (b_note.clone(), b_summary.clone())
    };

    assert_eq!(
        fold["moved"].as_array().map(|m| m
            .iter()
            .map(|n| n["id"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()),
        Some(vec![moved.as_str()]),
        "the operator is shown every note that would change hands: {preview}"
    );
    assert_eq!(
        fold["removed"],
        json!(false),
        "the duplicate still holds a rival document, so it cannot go: {preview}"
    );
    assert_eq!(
        preview["residuals"].as_array().map(|r| r
            .iter()
            .map(|n| (
                n["id"].as_str().unwrap_or_default(),
                n["cause"].as_str().unwrap_or_default()
            ))
            .collect::<Vec<_>>()),
        Some(vec![(residual.as_str(), "fileInTheWay")]),
        "and told exactly which document is still theirs to settle: {preview}"
    );
    assert!(
        preview.get("merged").is_none(),
        "a preview must not claim it changed anything: {preview}"
    );
    let (_, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    assert_eq!(
        provisioned_names(&tree),
        before,
        "a dry run must leave the tree exactly as it found it"
    );
    assert_eq!(
        journal_len(&runtime).await,
        events_before,
        "a dry run must not announce anything either"
    );

    // -- the real thing -----------------------------------------------------
    let (status, done) = send(
        &state,
        "POST",
        "/api/v1/company/workspace/merge-duplicate-folders",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        done.get("wouldMerge").is_none(),
        "a real run must not answer in the preview's field: {done}"
    );
    assert_eq!(done["merged"][0]["id"], json!(loser));
    assert_eq!(done["merged"][0]["removed"], json!(false));
    assert_eq!(done["residuals"][0]["id"], json!(residual));

    let (_, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    let parents: std::collections::HashMap<&str, &str> = tree
        .as_array()
        .unwrap()
        .iter()
        .map(|node| {
            (
                node["id"].as_str().unwrap_or_default(),
                node["parentId"].as_str().unwrap_or("-"),
            )
        })
        .collect();
    assert_eq!(
        parents.get(moved.as_str()),
        Some(&winner.as_str()),
        "the note moved into the surviving folder, under the id it was published as"
    );
    assert_eq!(
        parents.get(residual.as_str()),
        Some(&loser.as_str()),
        "and the rival document did not move at all"
    );
    assert!(
        parents.contains_key(loser.as_str()),
        "the duplicate folder still holds something, so it must still be there"
    );

    // The move announces itself, because the repair runs through
    // `runtime.workspace()` — the same announcer-wrapped handle the per-node
    // routes use (issue #327).
    assert!(
        workspace_changes(&runtime)
            .await
            .contains(&(moved.clone(), "updated".to_string())),
        "an open console must see the note change hands"
    );

    // -- the operator settles the collision, and runs it again --------------
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/workspace/{residual}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, again) = send(
        &state,
        "POST",
        "/api/v1/company/workspace/merge-duplicate-folders",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        again["merged"][0]["removed"],
        json!(true),
        "with nothing left in it, the duplicate finally goes: {again}"
    );
    assert_eq!(again["residuals"], json!([]));
    let (_, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    assert!(
        !tree
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["id"] == json!(loser)),
        "the duplicate is gone from the tree: {tree}"
    );
    assert!(
        workspace_changes(&runtime)
            .await
            .contains(&(loser.clone(), "removed".to_string())),
        "and its removal was announced too"
    );

    // -- and once more, which must be a no-op -------------------------------
    let (status, third) = send(
        &state,
        "POST",
        "/api/v1/company/workspace/merge-duplicate-folders",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(third["merged"], json!([]), "nothing left to merge: {third}");
    assert_eq!(third["residuals"], json!([]));

    // The route resolves under the platform scope form too, and is never
    // captured as a node id by the `…/workspace/{node_id}` route.
    let (status, scoped) = send(
        &state,
        "POST",
        "/api/v1/companies/acme/workspace/merge-duplicate-folders?dry_run=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(scoped["wouldMerge"], json!([]));
}

/// Every `WorkspaceChanged` the company has journalled, as `(node id, change)`.
async fn workspace_changes(
    runtime: &std::sync::Arc<crate::runtime::CompanyRuntime>,
) -> Vec<(String, String)> {
    use crate::ports::types::CompanyEvent;
    runtime
        .events()
        .read_from(
            runtime.id(),
            crate::ports::types::EventSeq::new(0),
            usize::MAX,
        )
        .await
        .unwrap()
        .into_iter()
        .filter_map(|stored| match stored.event {
            CompanyEvent::WorkspaceChanged { node_id, change } => Some((node_id, change)),
            _ => None,
        })
        .collect()
}

/// How many events the company has journalled so far.
async fn journal_len(runtime: &std::sync::Arc<crate::company::runtime::CompanyRuntime>) -> usize {
    runtime
        .events()
        .read_from(
            runtime.id(),
            crate::ports::types::EventSeq::new(0),
            usize::MAX,
        )
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn skills_install_persists_the_registry_document_not_the_client_metadata() {
    let home_dir = home();
    let state = state_with_registry(home_dir.path()).await;

    // Deliberately hostile client metadata: if any of it reaches the persisted
    // document, install is still trusting the client.
    let (status, skill) = send(
        &state,
        "POST",
        "/api/v1/company/skills/competitor-scan/install",
        Some(json!({
            "name": "Not The Real Name",
            "description": "a one-line stub the client made up",
            "category": "Finance"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The response reflects the library's own metadata, not the request body.
    assert_eq!(skill["name"], "Competitor Scan");
    assert_eq!(skill["category"], "Research");
    assert_eq!(skill["source"], "registry");
    // The pinned revision rides on the installed projection, so a later "update
    // available" check can diff an install against the live library.
    assert_eq!(skill["version"], "1.0.0");
    assert!(
        skill["description"]
            .as_str()
            .unwrap()
            .starts_with("Profile a handful of competitors"),
        "description came from the registry, got {:?}",
        skill["description"]
    );

    // The persisted SKILL.md carries the whole procedure — the actual bug.
    let deltas = persisted_skills(&state).await;
    let row = deltas
        .iter()
        .find(|s| s.slug == "competitor-scan")
        .expect("the install persisted a row");
    let doc = row.custom_doc.as_deref().expect("a document was persisted");
    assert!(
        doc.contains("## Steps"),
        "body lost its Steps section: {doc}"
    );
    assert!(
        doc.contains("## Output"),
        "body lost its Output section: {doc}"
    );
    assert!(
        doc.contains("version: 1.0.0"),
        "the snapshot pins the library version: {doc}"
    );
    // None of the client's metadata leaked in.
    assert!(!doc.contains("Not The Real Name"), "{doc}");
    assert!(!doc.contains("a one-line stub the client made up"), "{doc}");
    // The body is the real procedure, not a copy of the description.
    let parsed = crate::company::parse_skill_md("competitor-scan", doc).expect("valid");
    assert_ne!(
        parsed.body.trim(),
        parsed.description.trim(),
        "the body must not be a degenerate copy of the description"
    );
}

#[tokio::test]
async fn skills_install_404s_a_slug_the_registry_lacks_and_persists_nothing() {
    let home_dir = home();
    let state = state_with_registry(home_dir.path()).await;

    // `competitor-analysis` was one of the console's phantom entries — it never
    // existed in the shared library. It must now fail loudly.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/skills/competitor-analysis/install",
        Some(json!({"name": "Competitor Analysis", "description": "phantom"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");

    assert!(
        persisted_skills(&state).await.is_empty(),
        "a rejected install must persist nothing"
    );
}

#[tokio::test]
async fn skills_install_falls_back_to_client_metadata_when_no_registry_is_served() {
    // Platform-provisioned mode: no shared library, so there is nothing to
    // resolve against and the client's metadata is all the host has.
    let home_dir = home();
    let state = state_with_company(home_dir.path()).await;

    let (status, skill) = send(
        &state,
        "POST",
        "/api/v1/company/skills/tenant-only-skill/install",
        Some(json!({"name": "Tenant Only", "description": "provisioned elsewhere"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an empty registry must not 404 every install"
    );
    assert_eq!(skill["name"], "Tenant Only");
    assert_eq!(skill["source"], "registry");
}

/// A *configured* shared library that cannot load must not degrade to the
/// empty-registry fallback above. Doing so would silently hand the client
/// authorship of a registry skill's contents on exactly the hosts that meant to
/// be server-authoritative — one malformed `SKILL.md` in the image and every
/// install starts trusting whatever the browser posted.
#[tokio::test]
async fn skills_install_500s_when_the_configured_library_cannot_load() {
    let home_dir = home();
    // A skills root that exists but holds a `SKILL.md` with no `description`,
    // which the parser rejects.
    let broken_root = home_dir.path().join("broken-skills");
    std::fs::create_dir_all(broken_root.join("web-research")).expect("skill dir");
    std::fs::write(
        broken_root.join("web-research/SKILL.md"),
        "---\nname: Web Research\n---\n# Web Research\n",
    )
    .expect("SKILL.md");

    let state = state_with_company(home_dir.path())
        .await
        .with_skills_root(&broken_root);

    // The state itself reports the load failure rather than an empty registry.
    assert!(
        state.shared_skill_registry().is_err(),
        "a configured-but-unloadable library must surface its error"
    );

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/skills/web-research/install",
        Some(json!({"name": "Client Authored", "description": "not the library's"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a broken library is a server error, not a client-metadata install: {body}"
    );
    assert!(
        persisted_skills(&state).await.is_empty(),
        "a failed install must persist nothing"
    );

    // The registry listing fails the same way rather than reporting "no library".
    let (status, _) = send(&state, "GET", "/api/v1/company/skills/registry", None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn skills_registry_lists_the_live_library_without_bodies() {
    let home_dir = home();
    let state = state_with_registry(home_dir.path()).await;

    let (status, body) = send(&state, "GET", "/api/v1/company/skills/registry", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("an array");
    // Counted from disk rather than hardcoded, so adding a skill to the shared
    // library does not break this test — it still asserts the route lists the
    // *whole* library.
    let on_disk = crate::company::load_dir_skills(&repo_skills_root())
        .expect("the shared library parses")
        .len();
    assert!(on_disk >= 14, "sanity: the library is populated");
    assert_eq!(rows.len(), on_disk, "every shared skill is listed");

    for row in rows {
        assert!(
            row.get("body").is_none(),
            "registry rows must never carry a body: {row}"
        );
        assert_eq!(row["version"], "1.0.0", "{row}");
        assert_eq!(row["publisher"], "OpenCompany", "{row}");
    }

    let scan = rows
        .iter()
        .find(|r| r["id"] == "competitor-scan")
        .expect("competitor-scan is in the library");
    assert_eq!(scan["name"], "Competitor Scan");
    assert_eq!(scan["category"], "Research");

    // The console's old hardcoded array listed slugs the host cannot serve;
    // the live list must not contain them.
    for phantom in ["competitor-analysis", "social-scheduler", "meeting-notes"] {
        assert!(
            !rows.iter().any(|r| r["id"] == phantom),
            "phantom slug {phantom} is not in the live registry"
        );
    }
}

#[tokio::test]
async fn skills_registry_is_empty_when_no_library_is_served() {
    let home_dir = home();
    let state = state_with_company(home_dir.path()).await;
    let (status, body) = send(&state, "GET", "/api/v1/company/skills/registry", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().expect("an array").len(), 0);
}

/// A slug is also a directory name (`skills/<slug>/`), so the handlers that
/// take one from the URL must refuse the values that could escape or traverse
/// the scratch tree: a parent segment (`..`), a path separator (`a/b`), and a
/// leading uppercase (`A` — slugs are lowercase by contract).
#[tokio::test]
async fn skill_handlers_reject_unsafe_slugs_and_write_nothing() {
    let home_dir = home();
    let state = state_with_registry(home_dir.path()).await;

    // `a%2Fb` is how a `/` arrives *inside* one path segment — the router sees
    // one slug and the handler must reject it rather than letting a separator
    // into a directory name.
    for bad in ["..", "a%2Fb", "A"] {
        let (status, body) = send(
            &state,
            "POST",
            &format!("/api/v1/company/skills/{bad}/install"),
            Some(json!({"name": "Name", "description": "desc"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "install {bad}: {body}");

        let (status, body) = send(
            &state,
            "PUT",
            &format!("/api/v1/company/skills/{bad}"),
            Some(json!({"enabled": true})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "toggle {bad}: {body}");
    }

    // A rejected slug must never reach the skill store.
    assert!(
        persisted_skills(&state).await.is_empty(),
        "an unsafe slug must not persist a delta"
    );

    // The same handlers still accept a well-formed slug.
    let (status, skill) = send(
        &state,
        "PUT",
        "/api/v1/company/skills/a-1",
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{skill}");
    assert_eq!(skill["id"], "a-1");
    assert_eq!(skill["enabled"], true);
}

#[tokio::test]
async fn skills_install_toggle_custom_and_builtin_uninstall_conflict() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // Install from registry, carrying the entry's metadata so the host persists
    // a real SKILL.md the agent can act on (not a content-less slug).
    let (status, skill) = send(
        &state,
        "POST",
        "/api/v1/company/skills/web-research/install",
        Some(json!({
            "name": "Web Research",
            "description": "Answer a question from multiple sources with citations.",
            "category": "Research"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(skill["source"], "registry");
    assert!(skill["enabled"].as_bool().unwrap());
    // The install response reflects the persisted custom_doc (parsed back), so a
    // non-empty description proves content was stored — the fix for the agent
    // never receiving registry skills.
    assert_eq!(skill["name"], "Web Research");
    assert_eq!(
        skill["description"],
        "Answer a question from multiple sources with citations."
    );

    // Uninstall the registry skill: 204.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/skills/web-research/uninstall",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Uninstalling an unknown/built-in skill is a 409.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/skills/onboard/uninstall",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "conflict");

    // Author a custom skill.
    let (status, custom) = send(
        &state,
        "POST",
        "/api/v1/company/skills",
        Some(json!({"name": "My Skill", "description": "Does a thing", "category": "Ops"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(custom["source"], "custom");
    assert_eq!(custom["name"], "My Skill");

    // Toggle it off.
    let (status, toggled) = send(
        &state,
        "PUT",
        "/api/v1/company/skills/my-skill",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!toggled["enabled"].as_bool().unwrap());

    // `GET …/skills` returns the effective set: here (no source dir) that's the
    // deltas — the custom skill, now disabled.
    let (status, list) = send(&state, "GET", "/api/v1/company/skills", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = list.as_array().expect("a JSON array of skills");
    let my_skill = rows
        .iter()
        .find(|s| s["id"] == "my-skill")
        .expect("the custom skill is listed");
    assert_eq!(my_skill["source"], "custom");
    assert_eq!(my_skill["name"], "My Skill");
    assert!(!my_skill["enabled"].as_bool().unwrap());
}

#[tokio::test]
async fn team_overlay_and_manifest_teammates_can_both_be_added_and_deleted() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // The manifest teammate shows up on the read side before any overlay add,
    // named `null` (the console falls back to the role).
    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK);
    // The company's own teammate; the global baseline is appended to every
    // roster and is not what this test is about.
    let roster = roster.as_array().unwrap();
    let ceo = roster
        .iter()
        .find(|row| row["id"] == "ceo")
        .expect("the manifest teammate is listed");
    assert_eq!(ceo["role"], "Chief");
    assert!(ceo["name"].is_null());

    // Add an overlay teammate.
    let (status, member) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Dana", "role": "Designer"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(member["role"], "Designer");
    let id = member["id"].as_str().unwrap().to_string();

    // The read side now merges in the overlay teammate, named this time.
    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK);
    let roster = roster.as_array().unwrap();
    let dana = roster.iter().find(|m| m["id"] == id).unwrap();
    assert_eq!(dana["name"], "Dana");
    assert_eq!(dana["role"], "Designer");

    // Deleting the overlay teammate is a 204.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/team/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The removed overlay teammate is gone from the read side too.
    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK);
    let roster = roster.as_array().unwrap();
    assert!(
        roster.iter().all(|row| row["id"] != id.as_str()),
        "the deleted overlay teammate is still listed: {roster:?}"
    );
    assert!(roster.iter().any(|row| row["id"] == "ceo"));

    // Toggle an inbox on.
    let (status, ack) = send(
        &state,
        "PUT",
        "/api/v1/company/team/ceo/inbox",
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["key"], "ceo");

    // And a manifest teammate deletes too — recorded as a tombstone on the
    // record, never as a rewrite of `company.toml`, so the blueprint being
    // re-read on the next load does not bring it back.
    let (status, _) = send(&state, "DELETE", "/api/v1/company/team/ceo", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        roster
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != "ceo"),
        "the manifest teammate is still listed after its delete: {roster:?}"
    );
}

#[tokio::test]
async fn inbox_read_marks_and_reports_unread() {
    use crate::ports::inbox::EmailRecord;
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    for i in 0..2 {
        runtime
            .inbox()
            .append(
                runtime.id(),
                &EmailRecord {
                    id: format!("m{i}"),
                    inbox: "ceo".into(),
                    from_name: "S".into(),
                    from_email: "s@x.test".into(),
                    subject: "hi".into(),
                    body: "yo".into(),
                    at_millis: i,
                    read: false,
                    outbound: false,
                },
            )
            .await
            .unwrap();
    }

    // Mark one read; one remains unread.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/inboxes/ceo/read",
        Some(json!({"ids": ["m0"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unread"], 1);

    // Mark the rest.
    let (status, body) = send(&state, "POST", "/api/v1/company/inboxes/ceo/read", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unread"], 0);
}

/// Appends one received email to `inbox`, for the read-surface tests below.
async fn append_mail(
    runtime: &crate::company::runtime::CompanyRuntime,
    inbox: &str,
    id: &str,
    subject: &str,
    at_millis: u64,
) {
    use crate::ports::inbox::EmailRecord;
    runtime
        .inbox()
        .append(
            runtime.id(),
            &EmailRecord {
                id: id.into(),
                inbox: inbox.into(),
                from_name: format!("{inbox} correspondent"),
                from_email: format!("{inbox}-sender@x.test"),
                subject: subject.into(),
                body: format!("body for {subject}"),
                at_millis,
                read: false,
                outbound: false,
            },
        )
        .await
        .unwrap();
}

/// The regression for issue #173: two teammates' inboxes must read back as two
/// *different* sets of mail. The console used to render a client-side fixture —
/// the same four invented emails for everybody — because no per-agent read was
/// reachable over REST at all.
#[tokio::test]
async fn inbox_reads_are_per_agent_and_never_shared() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    // Enable two inboxes and file distinct mail in each. Inbox keys are agent
    // ids; `cto` is an operator-added teammate as far as the toggle cares, so it
    // takes its own key without a manifest entry.
    for agent in ["ceo", "cto"] {
        let (status, _) = send(
            &state,
            "PUT",
            &format!("/api/v1/company/team/{agent}/inbox"),
            Some(json!({"enabled": true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    append_mail(&runtime, "ceo", "c1", "board deck", 10).await;
    append_mail(&runtime, "ceo", "c2", "investor intro", 20).await;
    append_mail(&runtime, "cto", "t1", "on-call rotation", 30).await;

    // The roster lists both, each with its own unread count.
    let (status, body) = send(&state, "GET", "/api/v1/company/inboxes", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let ceo = rows.iter().find(|r| r["key"] == "ceo").unwrap();
    let cto = rows.iter().find(|r| r["key"] == "cto").unwrap();
    assert_eq!(ceo["enabled"], true);
    assert_eq!(ceo["unread"], 2);
    assert_eq!(cto["unread"], 1);

    // Each inbox reads back only its own mail — the shared-fixture bug. The
    // route serves store (append) order; the console sorts newest-first.
    let (status, body) = send(&state, "GET", "/api/v1/company/inboxes/ceo/messages", None).await;
    assert_eq!(status, StatusCode::OK);
    let ceo_subjects: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["subject"].as_str().unwrap())
        .collect();
    assert_eq!(ceo_subjects, vec!["board deck", "investor intro"]);

    let (status, body) = send(&state, "GET", "/api/v1/company/inboxes/cto/messages", None).await;
    assert_eq!(status, StatusCode::OK);
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["subject"], "on-call rotation");
    assert_eq!(items[0]["fromEmail"], "cto-sender@x.test");
    assert_eq!(items[0]["inbox"], "cto");
}

/// An inbox nobody has mail in — or that does not exist at all — reads as an
/// empty list rather than a 404. An enabled-but-empty inbox is a legitimate
/// state, and the console must render it as such rather than as an error.
#[tokio::test]
async fn inbox_messages_soft_fail_on_unknown_key() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    append_mail(&runtime, "ceo", "m0", "mail 0", 1).await;

    let (status, body) = send(
        &state,
        "GET",
        "/api/v1/company/inboxes/nobody/messages",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());

    // …and the inbox that *does* hold mail is unaffected by that read.
    let (_, body) = send(&state, "GET", "/api/v1/company/inboxes/ceo/messages", None).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
}

/// An inbox switched on but never written to is still listed, so the console can
/// show it the moment the Team toggle flips — and `GET …/team` reports the same
/// enabled state, so the toggle isn't a client-side guess.
#[tokio::test]
async fn team_read_reports_inbox_enabled_and_empty_inbox_is_listed() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // Before the toggle: no inbox on the roster, and nothing listed.
    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK);
    let ceo = roster
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "ceo")
        .unwrap()
        .clone();
    assert_eq!(ceo["inboxEnabled"], false);
    let (_, body) = send(&state, "GET", "/api/v1/company/inboxes", None).await;
    assert!(body.as_array().unwrap().is_empty());

    // Toggle it on: listed with zero mail, and the roster agrees.
    let (status, _) = send(
        &state,
        "PUT",
        "/api/v1/company/team/ceo/inbox",
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = send(&state, "GET", "/api/v1/company/inboxes", None).await;
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "ceo");
    assert_eq!(rows[0]["enabled"], true);
    assert_eq!(rows[0]["unread"], 0);
    // The manifest role is the display name until a domain gives it an address.
    assert_eq!(rows[0]["name"], "Chief");

    let (_, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    let ceo = roster
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "ceo")
        .unwrap()
        .clone();
    assert_eq!(ceo["inboxEnabled"], true);

    // Toggling back off keeps the inbox listed but disabled — the console
    // filters on `enabled`, so it drops out of the selector without losing mail.
    let (status, _) = send(
        &state,
        "PUT",
        "/api/v1/company/team/ceo/inbox",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = send(&state, "GET", "/api/v1/company/inboxes", None).await;
    assert_eq!(body.as_array().unwrap()[0]["enabled"], false);
}

/// Mail that arrives through the ingest webhook is exactly what the console's
/// read surface returns — the end-to-end path issue #173's repro step 4 walked.
#[tokio::test]
async fn ingested_mail_shows_up_on_the_console_read_surface() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    // Straight into the store, as `file_and_notify` does for a verified payload
    // (the HMAC path itself is covered in `ops::test`).
    append_mail(&runtime, "ceo", "ingested-1", "hello from outside", 42).await;

    let (status, body) = send(&state, "GET", "/api/v1/company/inboxes/ceo/messages", None).await;
    assert_eq!(status, StatusCode::OK);
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "ingested-1");
    assert_eq!(items[0]["subject"], "hello from outside");
    assert_eq!(items[0]["read"], false);
    assert_eq!(items[0]["outbound"], false);

    // Reading it drops the unread count the selector badges.
    let (_, body) = send(
        &state,
        "POST",
        "/api/v1/company/inboxes/ceo/read",
        Some(json!({"ids": ["ingested-1"]})),
    )
    .await;
    assert_eq!(body["unread"], 0);
    let (_, body) = send(&state, "GET", "/api/v1/company/inboxes", None).await;
    assert_eq!(body.as_array().unwrap()[0]["unread"], 0);
}

#[tokio::test]
async fn inbox_list_and_messages_project_store() {
    use crate::ports::inbox::EmailRecord;
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    // One inbound (unread) + one outbound reply in inbox "ceo".
    runtime
        .inbox()
        .append(
            runtime.id(),
            &EmailRecord {
                id: "in1".into(),
                inbox: "ceo".into(),
                from_name: "Priya".into(),
                from_email: "p@x.test".into(),
                subject: "hi".into(),
                body: "hello world".into(),
                at_millis: 1,
                read: false,
                outbound: false,
            },
        )
        .await
        .unwrap();
    runtime
        .inbox()
        .append(
            runtime.id(),
            &EmailRecord {
                id: "out1".into(),
                inbox: "ceo".into(),
                from_name: String::new(),
                from_email: "ceo@acme.test".into(),
                subject: "re: hi".into(),
                body: "reply".into(),
                at_millis: 2,
                read: false,
                outbound: true,
            },
        )
        .await
        .unwrap();

    // GET /inboxes surfaces the message-bearing inbox; outbound doesn't count toward unread.
    let (status, body) = send(&state, "GET", "/api/v1/company/inboxes", None).await;
    assert_eq!(status, StatusCode::OK);
    let ceo = body
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"] == "ceo")
        .expect("ceo inbox listed");
    assert_eq!(ceo["unread"], 1);

    // GET messages returns both, camelCase, oldest first.
    let (status, body) = send(&state, "GET", "/api/v1/company/inboxes/ceo/messages", None).await;
    assert_eq!(status, StatusCode::OK);
    let msgs = body.as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["id"], "in1");
    assert_eq!(msgs[0]["fromEmail"], "p@x.test");
    assert_eq!(msgs[1]["outbound"], true);
}

/// Issue #416, the half that holds in every build: a question asked on a
/// workflow copilot thread must not leave work on the company's board.
///
/// The copilot is a conversation *about a graph*, and its questions are phrased
/// at the graph — "add a node that emails the report". The chat route's
/// deterministic intent detector reads that as a request to the company and
/// opens a `todo` card, which is the same class of over-reach this issue is
/// about, reached from the route rather than from the model. The control half
/// matters as much as the confined half: the identical sentence on an ordinary
/// thread still opens its card, so this narrows the copilot rather than
/// disabling a feature.
#[tokio::test]
async fn a_copilot_thread_question_opens_no_board_card() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // Deliberately the most actionable phrasing the copilot invites.
    let ask = "build a node that emails the weekly report";

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({"message": ask, "chat": "workflow-copilot:weekly_report"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    // Strict on the shape, like the control half below: `is_none_or` would pass
    // on a body that is not a list at all, so a route that started answering an
    // error object would go green here and panic there — reported as a failure
    // of the control rather than of the thing under test.
    let cards = board.as_array().expect("the board lists cards");
    assert!(
        cards.is_empty(),
        "a copilot question left work on the board: {board}"
    );

    // The same rule reaches the other deterministic side effect a chat turn
    // has: a complaint phrase on a copilot thread is the operator correcting a
    // conversation about their graph, not feedback about the company's work.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({
            "message": "no, that is wrong, this node keeps failing",
            "chat": "workflow-copilot:weekly_report",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, filed) = send(&state, "GET", "/api/v1/company/feedback", None).await;
    assert_eq!(status, StatusCode::OK);
    let items = filed.as_array().expect("the feedback list is an array");
    assert!(
        items.is_empty(),
        "a copilot correction filed company feedback: {filed}"
    );

    // Control: the same sentence on the ordinary thread still opens a card, so
    // the suppression is scoped to the copilot and not a regression of #246.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({"message": ask})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    let cards = board.as_array().expect("the board lists cards");
    assert_eq!(
        cards.len(),
        1,
        "the ordinary thread must still open exactly one card: {board}"
    );
}

/// Issue #267: a question about the board's own state is answered, not carded.
///
/// This is the exact message that produced one of the six dead `backlog` cards
/// on a live company. The route now triages it as `Answer`, so the
/// deterministic card path stands down — and the reply still comes back OK,
/// because triage decides what gets *written*, never whether the operator gets
/// an answer.
///
/// The control half is the point: the same route, one sentence later, still
/// opens a card for a real instruction. A test that only proved the question
/// wrote nothing would also pass on a route that had stopped carding entirely.
#[tokio::test]
async fn a_question_about_the_board_opens_no_card() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    for ask in [
        "what is there in the tasks list?",
        "Tell what is there in the tasks list",
        "list the tasks",
        "show me the board",
    ] {
        let (status, body) = send(
            &state,
            "POST",
            "/api/v1/company/chat",
            Some(json!({ "message": ask })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "asking `{ask}` must still answer");
        assert!(body["responses"].is_array(), "no reply for `{ask}`: {body}");

        let (status, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
        assert_eq!(status, StatusCode::OK);
        let cards = board.as_array().expect("the board lists cards");
        assert!(
            cards.is_empty(),
            "the question `{ask}` left work on the board: {board}"
        );
    }

    // Control: a real instruction on the same route still opens exactly one
    // card, so this narrows the detector rather than switching it off.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({"message": "build the landing page"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    let cards = board.as_array().expect("the board lists cards");
    assert_eq!(
        cards.len(),
        1,
        "an instruction must still open exactly one card: {board}"
    );
}

#[tokio::test]
async fn chat_accepts_desk_id_and_replies() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({"message": "hello", "chat": "Creative studio"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["responses"].is_array());
}

#[tokio::test]
async fn credential_route_rejects_foreign_tenant() {
    use crate::server::platform_auth::{
        PlatformAuthConfig, PlatformClaims, UnsignedTenantVerifier,
    };
    use std::collections::HashSet;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // Platform mode: `acme` is owned by `tenant:acme`.
    let verifier = std::sync::Arc::new(UnsignedTenantVerifier::new("plat-secret"));
    let state = AppState::new(AppConfig::default())
        .with_home(home.clone())
        .with_platform_auth(PlatformAuthConfig::new(verifier));
    let id = CompanyId::new("acme");
    let runtime = RuntimeBuilder::new(home.clone(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    state
        .registry()
        .insert(id.clone(), std::sync::Arc::new(runtime));
    state.set_owner(id.clone(), "tenant:acme");

    let token = |tenant: &str| {
        UnsignedTenantVerifier::tenant_token(&PlatformClaims {
            tenant: tenant.to_string(),
            scopes: HashSet::from(["operator".to_string()]),
            companies: None,
        })
    };

    // A foreign tenant cannot set acme's domain (credential route is scoped).
    let (status, _) = send_auth(
        &state,
        "PUT",
        "/api/v1/companies/acme/domain",
        Some(json!({"domain": "acme.test"})),
        Some(&token("tenant:evil")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The owning tenant succeeds.
    let (status, _) = send_auth(
        &state,
        "PUT",
        "/api/v1/companies/acme/domain",
        Some(json!({"domain": "acme.test"})),
        Some(&token("tenant:acme")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn unknown_company_scope_is_404() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/companies/ghost/tasks",
        Some(json!({"title": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// MCP servers (issue #50)
// ---------------------------------------------------------------------------

/// A manifest that declares one committed `[[mcp_server]]` — used to assert the
/// manifest-server guards (cannot delete; overridable).
fn mcp_manifest() -> CompanyManifest {
    toml::from_str(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n[[mcp_server]]\nname = \"docs\"\nendpoint = \"https://docs.example/mcp\"\n",
    )
    .unwrap()
}

/// Boots an fs-backed company from a caller-supplied manifest (mirrors
/// `state_with_company`, which pins the default manifest).
async fn state_with_manifest(home: &std::path::Path, manifest: CompanyManifest) -> AppState {
    state_with_manifest_and_defaults(home, manifest, Vec::new()).await
}

/// Like [`state_with_manifest`], but with install-wide default MCP servers
/// configured (issue #527), for asserting the default-override guards.
async fn state_with_manifest_and_defaults(
    home: &std::path::Path,
    manifest: CompanyManifest,
    defaults: Vec<crate::company::McpServer>,
) -> AppState {
    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest.clone(),
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
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .with_default_mcp_servers(defaults)
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, std::sync::Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

/// Like [`state_with_manifest`], but seeds operator-added overlay teammates too,
/// so a test can assert MCP reachability over the full runtime roster — manifest
/// agents plus overlay agents — the way `build_roster` composes it (issue #568).
async fn state_with_manifest_and_overlays(
    home: &std::path::Path,
    manifest: CompanyManifest,
    overlay_agents: Vec<crate::ports::types::OverlayAgent>,
) -> AppState {
    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents,
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
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, std::sync::Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

#[tokio::test]
async fn mcp_servers_crud_round_trips_and_token_is_write_only() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // Cold: no servers.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 0);

    // Add a runtime server WITH a token.
    let (status, added) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({
            "name": "notion",
            "endpoint": "https://notion.example/mcp",
            "token": "sk-write-only-abc",
            "allowedTools": ["search"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(added["server"]["name"], "notion");
    assert_eq!(added["server"]["source"], "runtime");
    assert_eq!(added["server"]["authConfigured"], true);
    // Issue #566: a mutating MCP change reaches agents on the company's next turn
    // (the effective set is re-fingerprinted every `HarnessPool::ensure` cycle), so
    // the note must state the no-restart contract outright — not merely avoid one
    // stale phrase. Asserting the positive claim rejects any "restart required"
    // variant too, which a bare `!contains("restart the company")` would let pass.
    let note = added["note"].as_str().unwrap();
    assert!(
        note.contains("next turn"),
        "note should promise next-turn pickup: {note}"
    );
    assert!(
        note.contains("no restart needed"),
        "mutating MCP response must state no restart is needed: {note}"
    );

    // The token must NOT appear anywhere in the add response.
    assert!(
        !serde_json::to_string(&added)
            .unwrap()
            .contains("sk-write-only-abc"),
        "add response leaked the token"
    );

    // GET reflects it, still without the token.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    let body = serde_json::to_string(&list).unwrap();
    assert!(body.contains("notion"));
    assert!(body.contains("\"authConfigured\":true"));
    assert!(!body.contains("sk-write-only-abc"), "list leaked the token");

    // Duplicate add is a 409.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({ "name": "notion", "endpoint": "https://notion.example/mcp" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Non-http endpoint is a 400.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({ "name": "bad", "endpoint": "ftp://x/mcp" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Disable via PUT.
    let (status, updated) = send(
        &state,
        "PUT",
        "/api/v1/company/mcp/servers/notion",
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["server"]["enabled"], false);
    assert_eq!(
        updated["server"]["authConfigured"], true,
        "token survives an update"
    );

    // Delete (runtime server) → 204, then it's gone.
    let (status, _) = send(&state, "DELETE", "/api/v1/company/mcp/servers/notion", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
}

/// Issue #1270: a build without the `mcp` feature must serve List A exactly as
/// before and answer the directory routes `not_wired`.
///
/// Gated on the absence of the feature rather than written once for both builds:
/// with `mcp` on, these routes reach a live registry and two upstream
/// directories over the network, which is not a thing a unit test may do. The
/// default `cargo test --locked` lane is what runs this, and it is the lane that
/// compiles the unwired half in the first place.
#[cfg(not(feature = "mcp"))]
#[tokio::test]
async fn without_the_mcp_feature_the_directory_is_not_wired_and_list_a_is_unchanged() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({ "name": "notion", "endpoint": "https://notion.example/mcp" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // List A is served, and carries none of the registry-only keys.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["source"], "runtime");
    for key in ["serverId", "qualifiedName", "iconUrl", "transport"] {
        assert!(
            list[0].get(key).is_none(),
            "`{key}` must not appear without a registry install"
        );
    }

    // Every directory route answers the console's degrade signal.
    for (method, uri) in [
        ("GET", "/api/v1/company/mcp/registry/search?q=git"),
        (
            "GET",
            "/api/v1/company/mcp/registry/entry?qualifiedName=@a/b",
        ),
        ("POST", "/api/v1/company/mcp/registry/install"),
        ("POST", "/api/v1/company/mcp/registry/sid/connect"),
        ("POST", "/api/v1/company/mcp/registry/sid/disconnect"),
        ("PUT", "/api/v1/company/mcp/registry/sid/env"),
        ("DELETE", "/api/v1/company/mcp/registry/sid"),
    ] {
        let (status, body) = send(&state, method, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(body["code"], "not_wired", "{method} {uri}");
    }

    // And the List A delete still works with no install behind the row.
    let (status, _) = send(&state, "DELETE", "/api/v1/company/mcp/servers/notion", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn mcp_manifest_server_cannot_be_deleted_but_can_be_overridden() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, mcp_manifest()).await;

    // The manifest server shows up as `manifest`.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["name"], "docs");
    assert_eq!(list[0]["source"], "manifest");

    // Deleting a manifest server is a 409.
    let (status, _) = send(&state, "DELETE", "/api/v1/company/mcp/servers/docs", None).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // But it can be disabled via a runtime override — still badged manifest.
    let (status, updated) = send(
        &state,
        "PUT",
        "/api/v1/company/mcp/servers/docs",
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["server"]["source"], "manifest");
    assert_eq!(updated["server"]["enabled"], false);
    // The mutating response carries reachability too (issue #568), so the console
    // reflects who can reach the server right after an edit, not only on reload.
    assert!(
        updated["server"]["reachableBy"].is_array(),
        "a mutating response also carries reachableBy"
    );
}

/// Issue #568: each listed server carries the agents whose *effective* grants
/// reach it — over the full runtime roster, manifest agents plus overlay
/// teammates. With a company `allow = ["*", "mcp:*"]`, an agent that declares
/// no `tools` (and every overlay teammate, which has no tools row) inherits the
/// wildcard and explicit MCP grant and reaches every server; an agent that
/// narrows itself to `mcp:notion` reaches only that server.
#[tokio::test]
async fn mcp_reachability_lists_reaching_agents_including_overlay() {
    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[tools]\nallow = [\"*\", \"mcp:*\"]\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\ntools = [\"mcp:notion\"]\n\
         [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n[policy]\nmode = \"full\"\n\
         [[mcp_server]]\nname = \"notion\"\nendpoint = \"https://notion.example/mcp\"\n\
         [[mcp_server]]\nname = \"linear\"\nendpoint = \"https://linear.example/mcp\"\n",
    )
    .unwrap();
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // A minted id, exactly as `POST …/team` gives an operator-added teammate —
    // the shape that used to reach the console's "Reachable by" line raw (#931).
    let overlay = crate::ports::types::OverlayAgent {
        id: "019fa75dbc9b-000000000001".to_string(),
        name: "Helper".to_string(),
        role: "Assistant".to_string(),
        description: None,
        tools: None,
        model: None,
        harness: None,
    };
    let state = state_with_manifest_and_overlays(&home, manifest, vec![overlay]).await;

    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    let reach = |name: &str| -> Vec<(String, String)> {
        let row = list
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("server `{name}` is listed"));
        let mut agents: Vec<(String, String)> = row["reachableBy"]
            .as_array()
            .expect("reachableBy serializes as an array")
            .iter()
            .map(|v| {
                (
                    v["id"].as_str().unwrap().to_string(),
                    v["name"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        agents.sort();
        agents
    };
    let pair = |id: &str, name: &str| (id.to_string(), name.to_string());

    // notion: the narrowed ceo, the wildcard-inheriting eng, and the overlay.
    // Issue #931: every row carries the display label the rest of the console
    // uses — a manifest agent's role, an overlay teammate's name — so the minted
    // overlay id is never what a reader sees.
    assert_eq!(
        reach("notion"),
        vec![
            pair("019fa75dbc9b-000000000001", "Helper"),
            pair("ceo", "Chief"),
            pair("eng", "Engineer"),
        ]
    );
    // linear: only the wildcard holders — ceo scoped itself out of it.
    assert_eq!(
        reach("linear"),
        vec![
            pair("019fa75dbc9b-000000000001", "Helper"),
            pair("eng", "Engineer"),
        ],
        "ceo narrowed to mcp:notion, so it cannot reach linear"
    );
}

/// Issue #568: a server no agent's grants cover comes back with an **empty**
/// `reachableBy` — the signal the console flags loudly rather than showing a
/// healthy server that is silently unreachable. Here a narrow company
/// `allow = ["mcp:docs"]` reaches `docs` but never `notion`.
#[tokio::test]
async fn mcp_reachability_flags_a_server_no_agent_can_reach() {
    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[tools]\nallow = [\"mcp:docs\"]\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\ntools = [\"mcp:docs\"]\n[policy]\nmode = \"full\"\n\
         [[mcp_server]]\nname = \"docs\"\nendpoint = \"https://docs.example/mcp\"\n\
         [[mcp_server]]\nname = \"notion\"\nendpoint = \"https://notion.example/mcp\"\n",
    )
    .unwrap();
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, manifest).await;

    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    let row = |name: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("server `{name}` is listed"))
            .clone()
    };
    assert_eq!(
        row("docs")["reachableBy"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (v["id"].as_str().unwrap(), v["name"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        vec![("ceo", "Chief")],
        "the company allow covers mcp:docs for the one agent"
    );
    assert!(
        row("notion")["reachableBy"].as_array().unwrap().is_empty(),
        "no agent's grants cover mcp:notion — the flagged zero case"
    );
}

/// Issue #568: a **disabled** server reaches nobody, however wide the grants.
/// `registry_for_agent` filters on `decl.enabled && grants_cover_server(..)`, so
/// an agent holding `mcp:docs` is handed no such tool while the server is off —
/// reporting it as reachable would be the console/harness disagreement this
/// feature exists to remove. Asserted on both readers: the mutating response
/// that turns the server off, and the later list.
#[tokio::test]
async fn mcp_reachability_is_empty_for_a_disabled_server() {
    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[tools]\nallow = [\"*\", \"mcp:*\"]\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\ntools = [\"mcp:docs\"]\n[policy]\nmode = \"full\"\n\
         [[mcp_server]]\nname = \"docs\"\nendpoint = \"https://docs.example/mcp\"\n",
    )
    .unwrap();
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, manifest).await;

    let reach = |body: &serde_json::Value| -> Vec<String> {
        body["reachableBy"]
            .as_array()
            .expect("reachableBy serializes as an array")
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect()
    };

    // Enabled: the one agent's grant covers it.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reach(&list[0]), vec!["ceo".to_string()]);

    // Disabling it empties reachability in the mutating response itself.
    let (status, updated) = send(
        &state,
        "PUT",
        "/api/v1/company/mcp/servers/docs",
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["server"]["enabled"], false);
    assert!(
        reach(&updated["server"]).is_empty(),
        "a disabled server is handed to no agent, so it is reachable by none"
    );

    // And the list agrees on the next read — the grant is unchanged, the server is off.
    let (_, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(list[0]["enabled"], false);
    assert!(
        reach(&list[0]).is_empty(),
        "the list reader applies the same enabled filter as the harness"
    );
}

/// An install default is disabled by its *first* runtime override: no prior
/// runtime entry exists to patch, so `update_server` must fall back to the
/// default declaration as its patch base rather than 404 (issue #527).
#[tokio::test]
async fn mcp_default_server_can_be_disabled_with_its_first_override() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let default = crate::company::McpServer {
        name: "deepwiki".to_string(),
        endpoint: "https://deepwiki.example/mcp".to_string(),
        ..Default::default()
    };
    let state = state_with_manifest_and_defaults(&home, manifest(), vec![default]).await;

    // Cold, the default is visible and badged `default`.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["name"], "deepwiki");
    assert_eq!(list[0]["source"], "default");

    // The first override disables it — the override persists alongside the
    // default, keeping the effective body but flipping `enabled` off.
    let (status, updated) = send(
        &state,
        "PUT",
        "/api/v1/company/mcp/servers/deepwiki",
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["server"]["enabled"], false);
    assert_eq!(
        updated["server"]["source"], "default",
        "an override inherits the default badge, so delete still refuses it"
    );

    // Delete still refuses: the declaration lives in the install config, and the
    // disable override is the supported toggle.
    let (status, _) = send(
        &state,
        "DELETE",
        "/api/v1/company/mcp/servers/deepwiki",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // The disable took: listing reflects `enabled: false`.
    let (_, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(list[0]["enabled"], false);
}

/// Without the `openhuman` feature there is no MCP transport, so live discovery
/// is "not wired". (Under the feature it would attempt a real network call.)
#[cfg(not(feature = "openhuman"))]
#[tokio::test]
async fn mcp_discovery_is_not_wired_without_the_feature() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, mcp_manifest()).await;
    let (status, body) = send(
        &state,
        "GET",
        "/api/v1/company/mcp/servers/docs/tools",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_wired");
}

/// A `user:pass@host` endpoint smuggles a credential into the URL — rejected as
/// a 400 (the error-hardening cell's validate-on-add).
#[tokio::test]
async fn mcp_userinfo_endpoint_is_rejected() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({ "name": "creds", "endpoint": "https://user:pass@host/mcp" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A query-parameter credential (BrowserBase style) round-trips write-only:
/// `authConfigured` flips true, the value never appears in the response, and a
/// non-secret id left in the endpoint URL raises the non-blocking advisory.
#[tokio::test]
async fn mcp_query_param_auth_round_trips_write_only_with_advisory() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, added) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({
            "name": "browserbase",
            // A secret-looking query param triggers the advisory; the real
            // credential rides write-only as a query-parameter auth.
            "endpoint": "https://api.browserbase.com/mcp?apiKey=leftover",
            "authKind": "query_param",
            "paramName": "apiKey",
            "token": "qp-secret-xyz"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(added["server"]["authConfigured"], true);
    assert!(
        added["warning"].as_str().is_some(),
        "a secret-looking endpoint query raises the advisory: {added}"
    );
    assert!(
        !serde_json::to_string(&added)
            .unwrap()
            .contains("qp-secret-xyz"),
        "the query-parameter credential leaked into the response"
    );

    // A query_param auth WITHOUT a paramName is a 400.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({
            "name": "noparam",
            "endpoint": "https://host/mcp",
            "authKind": "query_param",
            "token": "x"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Workflow creator (issue #69)
// ---------------------------------------------------------------------------

/// Boots an fs-backed company with a writable source directory (a `seed_dir`)
/// — the workflow creator writes `workflows/<id>.toml` under it, mirroring how
/// a real `companies/<name>` checkout is wired via `--company`.
async fn state_with_source_dir(
    home: &std::path::Path,
    seed_dir: &std::path::Path,
    manifest: CompanyManifest,
) -> AppState {
    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest.clone(),
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
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .with_seed_dir(seed_dir.to_path_buf())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, std::sync::Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

/// A valid graph body: a trigger → an agent node naming the roster's `ceo` →
/// an output. `$id` becomes both the workflow id and its display name.
fn workflow_body(id: &str) -> Value {
    json!({
        "id": id,
        "name": id,
        "description": "A tiny test graph.",
        "nodes": [
            {"id": "start", "kind": "trigger", "name": "Start"},
            {"id": "worker", "kind": "agent", "name": "Worker", "agent": "ceo"},
            {"id": "done", "kind": "output", "name": "Done"},
        ],
        "edges": [
            {"from": "start", "to": "worker"},
            {"from": "worker", "to": "done", "label": "ok"},
        ],
    })
}

/// Issue #168: the create path persists the graph **on the record**, never in
/// the company source tree (which is a read-only mount in hosted mode), and
/// both read routes serve it from there.
#[tokio::test]
async fn workflow_create_persists_on_the_record_appends_enabled_and_is_listed() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let seed_dir = home.join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();
    let state = state_with_source_dir(&home, &seed_dir, manifest()).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/workflows",
        Some(workflow_body("greet")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["id"], "greet");
    assert_eq!(created["nodes"].as_array().unwrap().len(), 3);
    assert_eq!(created["edges"].as_array().unwrap().len(), 2);

    // Nothing was written into the company source tree — the read-only mount in
    // hosted mode, and the whole reason #168 failed with EROFS.
    let path = seed_dir.join("workflows").join("greet.toml");
    assert!(!path.exists(), "the source tree must not be written to");

    // The body and the enabled id both landed on the operator's live record —
    // the version-controlled seed dir's own `company.toml` was never touched
    // (there isn't one here; only the store's copy is checked).
    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.to_path_buf());
    let record = store.load(&CompanyId::new("acme")).await.unwrap().unwrap();
    assert_eq!(record.manifest.workflows.enabled, vec!["greet".to_string()]);
    assert_eq!(record.overlay_workflows.len(), 1);
    assert_eq!(record.overlay_workflows[0].id, "greet");
    assert!(record.overlay_workflows[0].toml.contains("agent = \"ceo\""));

    // `GET …/workflows` (seed ∪ overlay) now lists it.
    let (status, list) = send(&state, "GET", "/api/v1/company/workflows", None).await;
    assert_eq!(status, StatusCode::OK);
    // The company's own graphs; the baseline is listed in every company.
    // Id heuristic, not provenance — `greet` never collides with a global id
    // here, so this is safe; see
    // `workflow_create_of_an_id_matching_a_global_wins_by_content` for the
    // colliding case, asserted by content rather than this filter.
    let rows: Vec<&serde_json::Value> = list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            let id = row["id"].as_str().unwrap_or_default();
            !crate::globals::workflows().iter().any(|w| w.id == id)
        })
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "greet");

    // `GET …/workflows/{wid}` round-trips the full graph too.
    let (status, graph) = send(&state, "GET", "/api/v1/company/workflows/greet", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(graph["name"], "greet");
}

/// A company workflow whose id matches a global's must win — checked by its
/// own content (the name it was created with), not by an id-membership
/// filter, which would misclassify this exact row as "the baseline's" because
/// the ids match. `create_company_workflow` does not special-case global ids
/// (only seed files, overlays and `[workflows].enabled` reserve one), so
/// creating over a global id is exactly this: the company's overlay
/// definition of that id supersedes the global on every read.
#[tokio::test]
async fn workflow_create_of_an_id_matching_a_global_wins_by_content() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let seed_dir = home.join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();
    let state = state_with_source_dir(&home, &seed_dir, manifest()).await;
    let taken = crate::globals::workflows()[0].id.clone();

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/workflows",
        Some(workflow_body(&taken)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["id"], taken);

    let (status, list) = send(&state, "GET", "/api/v1/company/workflows", None).await;
    assert_eq!(status, StatusCode::OK);
    let matching: Vec<&serde_json::Value> = list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["id"] == taken.as_str())
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "the shadowed global must not be listed alongside the override: {list}"
    );
    assert_eq!(
        matching[0]["name"], taken,
        "the company's own definition (named after its id, per `workflow_body`) must win"
    );

    let (status, graph) = send(
        &state,
        "GET",
        &format!("/api/v1/company/workflows/{taken}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(graph["name"], taken);
}

#[tokio::test]
async fn workflow_create_duplicate_id_is_conflict() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let seed_dir = home.join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();
    let state = state_with_source_dir(&home, &seed_dir, manifest()).await;

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/workflows",
        Some(workflow_body("greet")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/workflows",
        Some(workflow_body("greet")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "conflict");
}

#[tokio::test]
async fn workflow_create_rejects_bad_edges_missing_agent_and_no_trigger() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let seed_dir = home.join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();
    let state = state_with_source_dir(&home, &seed_dir, manifest()).await;

    // An edge referencing a node id that doesn't exist.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/workflows",
        Some(json!({
            "id": "bad-edge",
            "name": "Bad edge",
            "nodes": [{"id": "start", "kind": "trigger", "name": "Start"}],
            "edges": [{"from": "start", "to": "ghost"}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Issue #1016: a dangling edge is now a structured `workflow_invalid` whose
    // `problems` array names the endpoint and the field, so the console can
    // highlight the id the author wrote.
    assert_eq!(body["code"], "workflow_invalid");
    assert_eq!(body["problems"][0]["node_id"], "ghost");
    assert_eq!(body["problems"][0]["field"], "to");

    // An agent node naming a teammate not on the roster.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/workflows",
        Some(json!({
            "id": "bad-agent",
            "name": "Bad agent",
            "nodes": [
                {"id": "start", "kind": "trigger", "name": "Start"},
                {"id": "worker", "kind": "agent", "name": "Worker", "agent": "ghost"},
            ],
            "edges": [{"from": "start", "to": "worker"}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
    assert!(body["error"].as_str().unwrap().contains("roster"), "{body}");

    // No trigger node at all.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/workflows",
        Some(json!({
            "id": "no-trigger",
            "name": "No trigger",
            "nodes": [{"id": "only", "kind": "output", "name": "Only"}],
            "edges": [],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    // None of the rejected attempts left a file behind.
    assert!(
        !seed_dir.join("workflows").is_dir() || {
            std::fs::read_dir(seed_dir.join("workflows"))
                .map(|mut d| d.next().is_none())
                .unwrap_or(true)
        }
    );
}

#[tokio::test]
async fn workflow_create_without_source_dir_succeeds() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // `state_with_company` boots with no `seed_dir`, so the company has no
    // source directory at all — the platform-provisioned-mode case. Issue #168:
    // creation used to be refused here with a 400; the body now lands on the
    // record, so it succeeds and reads back.
    let state = state_with_company(&home).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/workflows",
        Some(workflow_body("greet")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {created}");
    assert_eq!(created["id"], "greet");

    let (status, graph) = send(&state, "GET", "/api/v1/company/workflows/greet", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 3);
}

/// Without the `openhuman` feature the on-demand Test route is "not wired".
#[cfg(not(feature = "openhuman"))]
#[tokio::test]
async fn mcp_test_route_is_not_wired_without_the_feature() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({ "name": "notion", "endpoint": "https://notion.example/mcp" })),
    )
    .await;
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers/notion/test",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_wired");
}

/// Under the `openhuman` feature, adding a server probes it — and a probe that
/// fails (dead endpoint) is **never** rolled back: the server stays added, and
/// its scrubbed health is returned as `test` and persisted onto the GET shape.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn mcp_add_probes_without_rollback_and_persists_health() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // A syntactically valid but unreachable endpoint (nothing listening).
    let (status, added) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({ "name": "dead", "endpoint": "http://127.0.0.1:1/mcp" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // No rollback — the server is present despite the failed probe.
    assert_eq!(added["server"]["name"], "dead");
    // The probe result is echoed and the status is a non-ok tier.
    assert!(added["test"].is_object(), "probe result echoed: {added}");
    assert_ne!(added["test"]["status"], "ok");

    // The health is persisted onto the GET shape too.
    let (_, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    let server = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "dead")
        .expect("server present");
    assert!(server["health"].is_object(), "health persisted: {server}");

    // On-demand Test re-probes and returns health.
    let (status, health) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers/dead/test",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        health["status"].is_string(),
        "test returns health: {health}"
    );
}

/// #187: the Artifacts tab's full loop — an agent draft, a human edit appended
/// as a new version, and the diff between them.
///
/// The point of the port is that the operator's edit does **not** overwrite the
/// agent's text, so this asserts v1 survives verbatim after the edit. A store
/// that mutated in place would still serve a plausible-looking artifact while
/// having destroyed the one datum the epic wants.
#[tokio::test]
async fn artifact_versions_capture_the_human_edit_and_diff() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // The agent's draft.
    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/artifacts",
        Some(json!({
            "taskId": "t-1",
            "title": "Launch post",
            "kind": "markdown",
            "body": "alpha\nbeta\ngamma",
            "authorId": "ceo"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["versions"].as_array().unwrap().len(), 1);
    // No human has touched it, so no diff is offered.
    assert!(created.get("humanEditDiff").is_none());

    // The operator edits one line before approving.
    let (status, edited) = send(
        &state,
        "POST",
        &format!("/api/v1/company/artifacts/{id}/versions"),
        Some(json!({ "body": "alpha\nBETA\ngamma" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let versions = edited["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    // v1 is untouched — the whole reason versions are append-only.
    assert_eq!(versions[0]["body"], "alpha\nbeta\ngamma");
    assert_eq!(versions[0]["author"], "agent");
    assert_eq!(versions[1]["author"], "operator");
    assert_eq!(versions[1]["note"], "operator edit before approval");

    // The derived diff rides along, so the tab needs one call.
    let diff = &edited["humanEditDiff"];
    assert_eq!(diff["fromVersion"], 1);
    assert_eq!(diff["toVersion"], 2);
    assert_eq!(diff["added"], 1);
    assert_eq!(diff["removed"], 1);

    // …and is also addressable on its own.
    let (status, standalone) = send(
        &state,
        "GET",
        &format!("/api/v1/company/artifacts/{id}/diff"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(standalone["toVersion"], 2);

    // Listing by task returns it; an unrelated task sees nothing.
    let (status, listed) = send(&state, "GET", "/api/v1/company/tasks/t-1/artifacts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let (_, empty) = send(
        &state,
        "GET",
        "/api/v1/company/tasks/t-other/artifacts",
        None,
    )
    .await;
    assert_eq!(empty.as_array().unwrap().len(), 0);

    // Issue #244: this record was created through the REST route, not by a
    // publish, so it carries no `source` — and the response omits the key
    // entirely rather than sending a null the console would have to special-case.
    assert!(
        listed[0].get("source").is_none(),
        "a record with no source must not carry an empty one: {}",
        listed[0]
    );
}

/// Issue #244: a published artifact's `source` reaches the console.
///
/// The record is flattened into [`ArtifactView`], so this is really asserting
/// that the projection stays a flatten and never becomes a hand-written field
/// list — the moment it does, a new record field is silently invisible to the
/// tab that exists to render it.
#[tokio::test]
async fn a_published_artifacts_source_reaches_the_console() {
    use crate::ports::artifacts::{ArtifactKind, ArtifactRecord, ArtifactStore};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let company = state.registry().list()[0].clone();
    let runtime = state.registry().get(&company).expect("company");
    let published = ArtifactRecord::new(
        "art-1",
        "t-1",
        "Launch spec",
        ArtifactKind::Markdown,
        "# Spec",
        "ceo",
        1,
    )
    .with_source("specs/launch.md");
    ArtifactStore::upsert(runtime.artifacts().as_ref(), &company, &published)
        .await
        .expect("seed");

    let (status, listed) = send(&state, "GET", "/api/v1/company/tasks/t-1/artifacts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[0]["source"], "specs/launch.md");

    let (status, one) = send(&state, "GET", "/api/v1/company/artifacts/art-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["source"], "specs/launch.md");
}

/// #185: `GET …/tasks/{id}` assembles the header, the per-task timeline, and
/// the lineage in one read.
///
/// The timeline half is the point: the journal is company-scoped, so this
/// asserts that a reply tagged with *this* task is admitted while an untagged
/// chat reply and a reply tagged to a *different* task are both excluded. Those
/// three cases are exactly what the `task_id` threading exists to separate.
#[tokio::test]
async fn task_detail_assembles_timeline_and_lineage() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();

    let card = |id: &str, title: &str, parent: Option<&str>| TaskRecord {
        id: id.into(),
        title: TaskTitle::authored(title),
        note: None,
        column: "in_review".into(),
        priority: "medium".into(),
        assignee: "ceo".into(),
        updated_at_millis: 1,
        origin: None,
        parent_task_id: parent.map(str::to_string),
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
        origin_message_seq: None,
        bounced: None,
    };
    for t in [
        card("t-parent", "Parent", None),
        card("t-1", "Ship it", Some("t-parent")),
        card("t-child", "Subtask", Some("t-1")),
        card("t-other", "Unrelated", None),
    ] {
        runtime.tasks().upsert(&company, &t).await.unwrap();
    }

    for event in [
        CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: None,
        },
        // Tagged to this task — admitted.
        CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            chat_id: "t-1".into(),
            agent_id: "ceo".into(),
            text: "on it".into(),
            steps: Vec::new(),
            task_id: Some("t-1".into()),
        },
        // An ordinary chat reply — excluded.
        CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            chat_id: "General".into(),
            agent_id: "ceo".into(),
            text: "unrelated chatter".into(),
            steps: Vec::new(),
            task_id: None,
        },
        // Tagged to a different task — excluded.
        CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            chat_id: "t-other".into(),
            agent_id: "ceo".into(),
            text: "someone else's work".into(),
            steps: Vec::new(),
            task_id: Some("t-other".into()),
        },
        CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".into(),
            desk: "ceo".into(),
            output: "shipped".into(),
            column: "in_review".into(),
            artifact_ids: Vec::new(),
            origin_chat_id: None,
            origin_parent: None,
        },
    ] {
        runtime.events().append(&company, event).await.unwrap();
    }

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(body["task"]["id"], "t-1");
    assert_eq!(body["task"]["parentTaskId"], "t-parent");

    let kinds: Vec<&str> = body["timeline"]
        .as_array()
        .expect("timeline array")
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["dispatched", "reply", "completed"]);

    let raw = serde_json::to_string(&body["timeline"]).unwrap();
    assert!(
        !raw.contains("unrelated chatter") && !raw.contains("someone else's work"),
        "another task's / an untagged chat reply leaked onto this timeline: {raw}"
    );

    assert_eq!(body["lineage"]["parent"]["id"], "t-parent");
    let children = body["lineage"]["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["id"], "t-child");

    // An unknown id 404s, matching PATCH/DELETE.
    let (status, _) = send(&state, "GET", "/api/v1/company/tasks/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// #187: the diff route's argument contract, and the 404s.
#[tokio::test]
async fn artifact_diff_rejects_a_half_specified_range() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (_, created) = send(
        &state,
        "POST",
        "/api/v1/company/artifacts",
        Some(json!({ "taskId": "t-1", "title": "Draft", "body": "one" })),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    // Neither bound, and no operator edit yet → nothing to diff, stated plainly
    // rather than silently returning an empty diff.
    let (status, _) = send(
        &state,
        "GET",
        &format!("/api/v1/company/artifacts/{id}/diff"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Half a range is a 400, not a guess about the other end.
    let (status, _) = send(
        &state,
        "GET",
        &format!("/api/v1/company/artifacts/{id}/diff?from=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A version that does not exist names itself.
    let (status, _) = send(
        &state,
        "GET",
        &format!("/api/v1/company/artifacts/{id}/diff?from=1&to=9"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unknown artifact ids 404 on every handler that takes one.
    let (status, _) = send(&state, "GET", "/api/v1/company/artifacts/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/artifacts/nope/versions",
        Some(json!({ "body": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&state, "DELETE", "/api/v1/company/artifacts/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// #185 gave `GET …/tasks/{task_id}` a handler, which now overlaps the static
/// `GET …/tasks/inflight` the operator strip reads.
///
/// Before #185 the dynamic segment carried no GET, so nothing could shadow the
/// strip. Now something can: if the routes were ever reordered (or the static
/// one dropped), `inflight` would be parsed as a *card id*, `task_detail` would
/// find no such card, and the strip would 404 — with no test failing anywhere
/// else, because no card can be named `inflight` for the collision to show up
/// in ordinary use.
#[tokio::test]
async fn inflight_read_is_not_shadowed_by_task_detail() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // The strip's read still resolves to the inflight handler: an array, not
    // the object `task_detail` would return, and not a 404.
    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/inflight", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.is_array(),
        "GET /tasks/inflight must hit list_inflight, not task_detail: {body}"
    );
}

/// Seeds a board card for the discussion tests (#335).
fn discussion_card(id: &str, title: &str) -> TaskRecord {
    TaskRecord {
        id: id.into(),
        title: TaskTitle::authored(title),
        note: None,
        column: "todo".into(),
        priority: "medium".into(),
        assignee: "ceo".into(),
        updated_at_millis: 1,
        origin: None,
        parent_task_id: None,
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
        origin_message_seq: None,
        bounced: None,
    }
}

/// #358: a posted message can be withdrawn, and the text stops being served.
///
/// The shape the issue asks for, asserted end to end over the real HTTP stack:
/// the row survives (position, author, time), the text does not, the withdrawal
/// is attributed, the journal keeps both events, and nothing about a message
/// nobody withdrew changes.
#[tokio::test]
async fn a_withdrawn_discussion_message_stops_being_served() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();
    runtime
        .tasks()
        .upsert(&company, &discussion_card("t-1", "Ship it"))
        .await
        .unwrap();

    const SECRET: &str = "sk-live-0000-DO-NOT-KEEP";
    let (status, posted) = send(
        &state,
        "POST",
        "/api/v1/company/tasks/t-1/discussion",
        Some(json!({ "text": format!("blocked on the API key: {SECRET}") })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let seq = posted["seq"].as_u64().expect("the post carries its seq");

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/tasks/t-1/discussion",
        Some(json!({ "text": "rotated it, we are unblocked" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, withdrawn) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/tasks/t-1/discussion/{seq}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(withdrawn["redacted"], true);
    assert_eq!(
        withdrawn["redactedBy"], "Harness Admin",
        "a withdrawal nobody's name is on is a message that can vanish quietly"
    );
    assert_eq!(
        withdrawn["seq"], seq,
        "the row keeps its place in the thread"
    );

    // The reload: what every reader of this card now gets.
    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    let thread = body["discussion"].as_array().expect("discussion array");
    assert_eq!(thread.len(), 2, "the row is withdrawn, not deleted: {body}");
    assert_eq!(thread[0]["seq"], seq);
    assert_eq!(thread[0]["redacted"], true);
    assert_eq!(thread[0]["redactedBy"], "Harness Admin");
    assert_eq!(
        thread[0]["author"], "Harness Admin",
        "the poster is still named"
    );
    assert_eq!(
        thread[1]["text"], "rotated it, we are unblocked",
        "withdrawing one message must not touch another"
    );
    assert!(
        thread[1].get("redacted").is_none(),
        "an ordinary row must keep the shape a pre-#358 console renders: {thread:?}"
    );
    assert!(
        !serde_json::to_string(&body).unwrap().contains(SECRET),
        "the withdrawn text is still being served on the detail read: {body}"
    );

    // The journal keeps both events: the post's existence is a fact, and the
    // withdrawal is a second fact about it.
    let events = runtime
        .events()
        .read_from(&company, crate::ports::types::EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    assert!(
        events.iter().any(|e| matches!(
            &e.event,
            CompanyEvent::TaskDiscussionRedacted { task_id, seq: s, .. }
                if task_id == "t-1" && *s == seq
        )),
        "the withdrawal was not journaled"
    );

    // Idempotent: asking twice is not an error, and does not grow the journal.
    let before = events.len();
    let (status, again) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/tasks/t-1/discussion/{seq}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["redacted"], true);
    let after = runtime
        .events()
        .read_from(&company, crate::ports::types::EventSeq::new(0), usize::MAX)
        .await
        .unwrap()
        .len();
    assert_eq!(
        before, after,
        "a repeated withdrawal appended a second tombstone"
    );
}

/// #358: a `seq` that is not a discussion post on *this* card is a `404`, not a
/// tombstone written into the journal against something else.
#[tokio::test]
async fn withdrawing_something_that_is_not_this_cards_post_is_refused() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();
    for card in [
        discussion_card("t-1", "Ship it"),
        discussion_card("t-other", "Unrelated"),
    ] {
        runtime.tasks().upsert(&company, &card).await.unwrap();
    }

    let (_, posted) = send(
        &state,
        "POST",
        "/api/v1/company/tasks/t-other/discussion",
        Some(json!({ "text": "another card's message" })),
    )
    .await;
    let seq = posted["seq"].as_u64().unwrap();

    // Another card's post, addressed through this card.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/tasks/t-1/discussion/{seq}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A sequence position that holds no event at all.
    let (status, _) = send(
        &state,
        "DELETE",
        "/api/v1/company/tasks/t-1/discussion/99999",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The other card's thread is untouched by either attempt.
    let (_, other) = send(&state, "GET", "/api/v1/company/tasks/t-other", None).await;
    let thread = other["discussion"].as_array().unwrap();
    assert_eq!(thread.len(), 1);
    assert_eq!(thread[0]["text"], "another card's message");
    assert!(thread[0].get("redacted").is_none());
}

/// #358 + #335's paging: a withdrawal is applied even when the tombstone sits
/// *newer* than the cursor the caller is paging back through.
///
/// The trap this pins: the discussion arm skips events at or after
/// `discussionBefore`, and a tombstone is always newer than the post it
/// withdraws. Applying the cursor to tombstones too would serve the original
/// text to anybody who scrolled far enough back — the one reader most likely to
/// be looking for it.
#[tokio::test]
async fn a_withdrawal_survives_paging_back_past_it() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();
    runtime
        .tasks()
        .upsert(&company, &discussion_card("t-1", "Ship it"))
        .await
        .unwrap();

    const SECRET: &str = "sk-live-PAGED-BACK";
    let (_, posted) = send(
        &state,
        "POST",
        "/api/v1/company/tasks/t-1/discussion",
        Some(json!({ "text": SECRET })),
    )
    .await;
    let seq = posted["seq"].as_u64().unwrap();

    // Enough newer posts that the first one falls off the first page.
    for n in 0..60 {
        runtime
            .events()
            .append(
                &company,
                CompanyEvent::TaskDiscussionPosted {
                    task_id: "t-1".into(),
                    text: format!("message {n}"),
                    by: None,
                },
            )
            .await
            .unwrap();
    }

    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/tasks/t-1/discussion/{seq}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Page back to the start of the thread, past the tombstone's own position.
    let (_, first) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    let oldest_on_page = first["discussion"].as_array().unwrap()[0]["seq"]
        .as_u64()
        .unwrap();
    let (status, older) = send(
        &state,
        "GET",
        &format!("/api/v1/company/tasks/t-1?discussionBefore={oldest_on_page}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !serde_json::to_string(&older).unwrap().contains(SECRET),
        "paging back served the withdrawn text: {older}"
    );
    let row = older["discussion"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["seq"] == seq)
        .expect("the withdrawn row is on the older page");
    assert_eq!(row["redacted"], true);
}

/// #335: the per-task Discussion tab's whole contract — a post persists, reads
/// back on the card's own detail, and belongs to exactly one card.
///
/// The acceptance criterion is "posts survive a reload and are visible from
/// another browser", which is the same thing as: the message lives in the
/// company journal, not in the posting session. So the assertions are made
/// through a *second, independent request* rather than off the POST's echo.
///
/// The scoping half matters as much: the journal is company-scoped, so a fold
/// that forgot to compare `task_id` would show every card the same thread.
#[tokio::test]
async fn task_discussion_posts_persist_and_are_scoped_to_their_card() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();

    for card in [
        discussion_card("t-1", "Ship it"),
        discussion_card("t-other", "Unrelated"),
        discussion_card("t-quiet", "Nobody has said anything"),
    ] {
        runtime.tasks().upsert(&company, &card).await.unwrap();
    }

    // Surrounding whitespace is trimmed, and the poster is named from the
    // roster — never by user id, and never by email address.
    let (status, posted) = send(
        &state,
        "POST",
        "/api/v1/company/tasks/t-1/discussion",
        Some(json!({ "text": "  blocked on the API key  " })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(posted["text"], "blocked on the API key");
    assert_eq!(posted["author"], "Harness Admin");

    for (task, text) in [
        ("t-1", "unblocked, the key was rotated"),
        ("t-other", "someone else's thread"),
    ] {
        let (status, _) = send(
            &state,
            "POST",
            &format!("/api/v1/company/tasks/{task}/discussion"),
            Some(json!({ "text": text })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // The reload: a fresh read of the card, which reaches the journal rather
    // than anything the posting request kept.
    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    let thread = body["discussion"].as_array().expect("discussion array");
    let texts: Vec<&str> = thread.iter().map(|m| m["text"].as_str().unwrap()).collect();
    assert_eq!(
        texts,
        vec!["blocked on the API key", "unblocked, the key was rotated"],
        "the thread reads back oldest-first"
    );
    assert!(
        thread[0]["seq"].as_u64().unwrap() < thread[1]["seq"].as_u64().unwrap(),
        "seq is the thread's strict order: {thread:?}"
    );
    assert!(
        !serde_json::to_string(&body["discussion"])
            .unwrap()
            .contains("someone else's thread"),
        "another card's message leaked onto this thread"
    );

    // The two projections stay apart: a discussion post is not a run event, so
    // it must not appear on the timeline the Timeline tab renders.
    assert!(
        body["timeline"].as_array().unwrap().is_empty(),
        "a discussion post must not land on the run timeline: {body}"
    );

    // The other card sees only its own message, and a card nobody has posted on
    // reads back an empty thread — what keeps the tab's empty state honest.
    let (_, other) = send(&state, "GET", "/api/v1/company/tasks/t-other", None).await;
    let other_thread = other["discussion"].as_array().unwrap();
    assert_eq!(other_thread.len(), 1);
    assert_eq!(other_thread[0]["text"], "someone else's thread");

    let (_, quiet) = send(&state, "GET", "/api/v1/company/tasks/t-quiet", None).await;
    assert_eq!(quiet["discussion"].as_array().unwrap().len(), 0);

    // Both scope forms serve the same thread.
    let (status, scoped) = send(&state, "GET", "/api/v1/companies/acme/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(scoped["discussion"].as_array().unwrap().len(), 2);
}

/// #335: what the write boundary refuses, and what it forgives.
///
/// An empty message is refused because there is no delete in v1 — a blank row
/// would be permanent noise. An unknown card is refused because the post would
/// otherwise be journaled somewhere no read surface can reach. An over-long
/// message is *not* refused: it is truncated, so a long paste still posts.
#[tokio::test]
async fn task_discussion_rejects_an_empty_message_and_an_unknown_card() {
    use crate::ports::tasks::MAX_DISCUSSION_CHARS;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();
    runtime
        .tasks()
        .upsert(&company, &discussion_card("t-1", "Ship it"))
        .await
        .unwrap();

    for text in ["", "   \n\t "] {
        let (status, _) = send(
            &state,
            "POST",
            "/api/v1/company/tasks/t-1/discussion",
            Some(json!({ "text": text })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "empty text: {text:?}");
    }

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/tasks/nope/discussion",
        Some(json!({ "text": "into the void" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A long paste posts, capped on a character boundary.
    let long = "é".repeat(MAX_DISCUSSION_CHARS + 500);
    let (status, posted) = send(
        &state,
        "POST",
        "/api/v1/company/tasks/t-1/discussion",
        Some(json!({ "text": long })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        posted["text"].as_str().unwrap().chars().count(),
        MAX_DISCUSSION_CHARS
    );

    // Only the accepted post is on the thread: the three refusals journaled
    // nothing.
    let (_, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(body["discussion"].as_array().unwrap().len(), 1);
}

/// #348 review: the thread is served on a screen that polls every 4s, so it
/// comes back as a **page** — the newest slice — with the rest reachable behind
/// a cursor. Without the cap, one busy card re-sends its whole history fifteen
/// times a minute per open browser, forever.
///
/// Asserted as a reader experiences it: the newest messages are the ones on the
/// first read, the response admits there are older ones, and passing the oldest
/// held `seq` back walks to the page before it without dropping or repeating a
/// message. The cursor's page is the *end* of the thread, which is what makes
/// `discussionHasMore` false there.
#[tokio::test]
async fn task_discussion_is_paged_newest_first_and_walks_back_with_a_cursor() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();
    runtime
        .tasks()
        .upsert(&company, &discussion_card("t-1", "Ship it"))
        .await
        .unwrap();

    // A thread longer than one page. Journaled directly: this test is about the
    // read's shape, and the write path is pinned by the tests above.
    const POSTS: usize = 62;
    for n in 0..POSTS {
        runtime
            .events()
            .append(
                &company,
                CompanyEvent::TaskDiscussionPosted {
                    task_id: "t-1".into(),
                    text: format!("message {n}"),
                    by: None,
                },
            )
            .await
            .unwrap();
    }

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    let page = body["discussion"].as_array().unwrap();
    assert!(
        page.len() < POSTS,
        "an unbounded thread came back whole: {} posts",
        page.len()
    );
    assert_eq!(
        body["discussionHasMore"], true,
        "a truncated thread that does not say so reads as the whole conversation"
    );
    // The tail, not the head: what somebody opening the card needs first.
    assert_eq!(
        page.last().unwrap()["text"],
        format!("message {}", POSTS - 1)
    );
    let first_seq = page[0]["seq"].as_u64().unwrap();
    let oldest_on_page = page[0]["text"].as_str().unwrap().to_string();

    // Walk back: the page *before* the oldest message held.
    let (status, older) = send(
        &state,
        "GET",
        &format!("/api/v1/company/tasks/t-1?discussionBefore={first_seq}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let older_page = older["discussion"].as_array().unwrap();
    assert_eq!(
        older_page.len(),
        POSTS - page.len(),
        "the cursor page plus the first page must be the whole thread"
    );
    assert_eq!(
        older["discussionHasMore"], false,
        "nothing precedes the start of the thread"
    );
    assert_eq!(older_page[0]["text"], "message 0");
    assert!(
        older_page
            .iter()
            .all(|m| m["seq"].as_u64().unwrap() < first_seq),
        "the cursor is exclusive — a message must not be served twice: {older_page:?}"
    );
    assert!(
        !older_page
            .iter()
            .any(|m| m["text"].as_str() == Some(oldest_on_page.as_str())),
        "the cursor message repeated on its own older page"
    );

    // A short thread is not paged at all — the flag stays honest downward.
    runtime
        .tasks()
        .upsert(&company, &discussion_card("t-2", "Quiet"))
        .await
        .unwrap();
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::TaskDiscussionPosted {
                task_id: "t-2".into(),
                text: "just the one".into(),
                by: None,
            },
        )
        .await
        .unwrap();
    let (_, quiet) = send(&state, "GET", "/api/v1/company/tasks/t-2", None).await;
    assert_eq!(quiet["discussion"].as_array().unwrap().len(), 1);
    assert_eq!(quiet["discussionHasMore"], false);
}

/// #348 review: every post in the tests above is the harness admin, which
/// exercises one of `into_message`'s three branches. The other two are the ones
/// that matter for what reaches a reader's screen:
///
/// * a **departed** user — off the roster, so there is no name to resolve —
///   must read as `someone`, never as the raw user id the journal holds;
/// * a **machine credential** — the platform scope, which names no person —
///   must read as `operator`, and must journal no actor at all.
///
/// The machine half goes through the real write path with a tenant token, so it
/// pins `ScopedCompany`'s "keep the person, drop the credential" rule too: a
/// platform post that started attributing itself to *something* would show up
/// here as a label that is not `operator`.
#[tokio::test]
async fn task_discussion_names_a_departed_user_someone_and_a_machine_credential_operator() {
    use crate::ports::types::{Actor, ActorKind, CompanyEvent};
    use crate::server::platform_auth::{
        PlatformAuthConfig, PlatformClaims, UnsignedTenantVerifier,
    };
    use std::collections::HashSet;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let verifier = std::sync::Arc::new(UnsignedTenantVerifier::new("plat-secret"));
    let state = state_with_company(&home)
        .await
        .with_platform_auth(PlatformAuthConfig::new(verifier));
    let company = CompanyId::new("acme");
    state.set_owner(company.clone(), "tenant:acme".to_string());
    let runtime = state.registry().get(&company).unwrap();
    runtime
        .tasks()
        .upsert(&company, &discussion_card("t-1", "Ship it"))
        .await
        .unwrap();

    // A user who has since left: journaled with an id the roster can no longer
    // resolve. Only the journal can hold this state, so the fixture is written
    // there rather than posted.
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::TaskDiscussionPosted {
                task_id: "t-1".into(),
                text: "I looked at this before I left".into(),
                by: Some(Actor {
                    kind: ActorKind::User,
                    id: "u-departed".into(),
                }),
            },
        )
        .await
        .unwrap();

    let token = UnsignedTenantVerifier::tenant_token(&PlatformClaims {
        tenant: "tenant:acme".to_string(),
        scopes: HashSet::from(["operator".to_string()]),
        companies: None,
    });
    let (status, posted) = send_auth(
        &state,
        "POST",
        "/api/v1/companies/acme/tasks/t-1/discussion",
        Some(json!({ "text": "posted by the platform" })),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(posted["author"], "operator");

    let (_, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    let thread = body["discussion"].as_array().unwrap();
    let authors: Vec<&str> = thread
        .iter()
        .map(|m| m["author"].as_str().unwrap())
        .collect();
    assert_eq!(authors, vec!["someone", "operator"]);
    // The id the journal holds must not reach a reader — a thread is read by
    // every member of the company.
    let wire = serde_json::to_string(&body["discussion"]).unwrap();
    assert!(
        !wire.contains("u-departed"),
        "a user id reached the wire: {wire}"
    );
}

/// #352: `GET …/tasks/{id}/export` answers a downloadable HTML document, built
/// from the same read the console consumes, and changes nothing.
///
/// The last clause is an acceptance criterion in its own right and the one thing
/// the renderer's own tests cannot see: a document that quietly journalled an
/// "exported" event, or touched the card's column or `updatedAt`, would make an
/// audit export a modification of the thing being audited. So the board row and
/// the journal length are both compared across the call.
#[tokio::test]
async fn task_export_serves_a_readable_document_and_alters_nothing() {
    use crate::ports::types::{CompanyEvent, EventSeq};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();

    runtime
        .tasks()
        .upsert(
            &company,
            &TaskRecord {
                id: "t-1".into(),
                title: TaskTitle::authored("Launch post"),
                note: Some("Write the launch post.".into()),
                column: "in_review".into(),
                priority: "high".into(),
                assignee: "writer".into(),
                updated_at_millis: 1,
                origin: None,
                parent_task_id: None,
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: None,
                bounced: None,
            },
        )
        .await
        .unwrap();
    for event in [
        CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            // `None` is the honest value here, not a placeholder: this fixture
            // journals a dispatch directly rather than going through the choke
            // point that mints a run row (#242), and the export renders the
            // timeline, which does not read `run_id`.
            run_id: None,
        },
        CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            chat_id: "t-1".into(),
            agent_id: "writer".into(),
            text: "First draft is up.".into(),
            steps: Vec::new(),
            task_id: Some("t-1".into()),
        },
    ] {
        runtime.events().append(&company, event).await.unwrap();
    }

    let before_board = runtime.tasks().list(&company).await.unwrap();
    let before_events = runtime
        .events()
        .read_from(&company, EventSeq::new(0), usize::MAX)
        .await
        .unwrap()
        .len();

    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/company/tasks/t-1/export")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let disposition = response
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(content_type, "text/html; charset=utf-8");
    assert_eq!(
        disposition, "attachment; filename=\"task-launch-post.html\"",
        "the export must download as a named file"
    );

    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(bytes.to_vec()).expect("the document is utf-8");
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("Launch post"));
    assert!(html.contains("<dd>Working — In review</dd>"));
    assert!(html.contains("First draft is up."));

    let after_board = runtime.tasks().list(&company).await.unwrap();
    assert_eq!(after_board, before_board, "exporting altered the board");
    let after_events = runtime
        .events()
        .read_from(&company, EventSeq::new(0), usize::MAX)
        .await
        .unwrap()
        .len();
    assert_eq!(after_events, before_events, "exporting journalled an event");

    let (status, _) = send(&state, "GET", "/api/v1/company/tasks/nope/export", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// #185 review follow-up: pin the two timeline branches the first test skipped —
/// `tool_failed`, and the window-correlated `approval` arm.
///
/// The approval arm is the only branch in `fold_task_journal` whose correlation is
/// heuristic (parked effects carry no task id, so it is scoped by the run
/// window). That makes it the one most likely to regress into leaking another
/// run's resolution, so it is asserted from both sides: a resolution *before*
/// the dispatch anchor must be excluded, one *inside* the window admitted.
#[tokio::test]
async fn task_timeline_scopes_approvals_to_the_run_window() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, Verdict};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();

    runtime
        .tasks()
        .upsert(
            &company,
            &TaskRecord {
                id: "t-1".into(),
                title: TaskTitle::authored("Ship it"),
                note: None,
                column: "in_review".into(),
                priority: "medium".into(),
                assignee: "ceo".into(),
                updated_at_millis: 1,
                origin: None,
                parent_task_id: None,
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: None,
                bounced: None,
            },
        )
        .await
        .unwrap();

    let approval = |id: &str| CompanyEvent::ApprovalResolved {
        approval_id: ApprovalId::new(id),
        verdict: Verdict::Approve,
        by: Actor {
            kind: ActorKind::User,
            id: "u-1".into(),
        },
    };

    for event in [
        // Before the dispatch anchor — belongs to some other run, must not leak.
        approval("before"),
        CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: None,
        },
        // Inside the window — admitted.
        approval("during"),
        CompanyEvent::McpCallFailed {
            task_id: Some("t-1".into()),
            server: "gh".into(),
            tool: "issues".into(),
            status: "credential_required".into(),
            message: "needs auth".into(),
        },
        CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".into(),
            desk: "ceo".into(),
            output: "shipped".into(),
            column: "in_review".into(),
            artifact_ids: Vec::new(),
            origin_chat_id: None,
            origin_parent: None,
        },
        // After the window closed — must not leak either.
        approval("after"),
    ] {
        runtime.events().append(&company, event).await.unwrap();
    }

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);

    let kinds: Vec<&str> = body["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["dispatched", "approval", "tool_failed", "completed"],
        "exactly one approval — the one inside the run window"
    );

    // The failure carries its scrubbed message; the operator's identity on the
    // approval is dropped, matching the SSE projection's deny-by-default stance.
    let raw = serde_json::to_string(&body["timeline"]).unwrap();
    assert!(raw.contains("needs auth"));
    assert!(!raw.contains("u-1"), "operator identity leaked: {raw}");
}

// ── Issue #305: working time vs waiting-on-a-human time ──────────────────────
//
// The split is a *read-time join*: the park instant lives only in the runtime
// journal (`ApprovalParked`), the resolution only in the event log
// (`ApprovalResolved`), and `approval_id` is the single key shared by both.
// These tests pin that join, its window clamp, and the two ways a wait can end
// (an operator decided, or the TTL swept it) — plus the negative case, which is
// an acceptance criterion in its own right: a task that never waited must
// report no waiting figure at all rather than a zero.

/// Parks an approval in the journal and seeds a card + its dispatch anchor.
/// Returns `(runtime, dispatched_at_millis)`.
async fn dispatched_task(
    state: &AppState,
    company: &CompanyId,
) -> (std::sync::Arc<crate::CompanyRuntime>, u64) {
    use crate::ports::types::CompanyEvent;

    let runtime = state.registry().get(company).unwrap();
    runtime
        .tasks()
        .upsert(
            company,
            &TaskRecord {
                id: "t-1".into(),
                title: TaskTitle::authored("Ship it"),
                note: None,
                column: "in_progress".into(),
                priority: "medium".into(),
                assignee: "ceo".into(),
                updated_at_millis: 1,
                origin: None,
                parent_task_id: None,
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: None,
                bounced: None,
            },
        )
        .await
        .unwrap();
    runtime
        .events()
        .append(
            company,
            CompanyEvent::TaskDispatched {
                task_id: "t-1".into(),
                run_id: None,
            },
        )
        .await
        .unwrap();
    let dispatched_at = runtime
        .events()
        .read_from(company, crate::ports::types::EventSeq::new(0), 64)
        .await
        .unwrap()
        .last()
        .unwrap()
        .at_millis;
    (runtime, dispatched_at)
}

/// A parked effect to journal. Its content is irrelevant to the join — only the
/// id and the instant matter.
fn parked_effect() -> crate::ports::types::Effect {
    use crate::ports::types::{Effect, EffectGroup};
    Effect {
        kind: "filing.submit".into(),
        group: EffectGroup::Sign,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: false,
        payload: serde_json::Value::Null,
        agent: None,
        run_id: None,
    }
}

/// Pulls the single `approval` row out of a task-detail body.
fn only_approval(body: &Value) -> Value {
    let rows: Vec<Value> = body["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "approval")
        .cloned()
        .collect();
    assert_eq!(rows.len(), 1, "expected exactly one approval row: {rows:?}");
    rows[0].clone()
}

/// **The acceptance test** (#305): an approval that parked and later resolved
/// reports the wait it actually caused.
///
/// Before this, `ApprovalResolved` carried a verdict and an actor but no park
/// time, so the console could only show one undifferentiated elapsed figure — a
/// task idle all day on a human looked exactly like one busy all day. The
/// assertion is exact arithmetic against the observed event timestamps, not a
/// tolerance: the whole value of the number is that it is not an estimate.
#[tokio::test]
async fn task_timeline_reports_the_wait_an_approval_actually_caused() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, Verdict};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    // Parked 40ms into the run. The sleep only guarantees the resolution lands
    // strictly after the park; the assertion below derives the expected span
    // from the real timestamps rather than from the sleep's duration.
    let id = ApprovalId::new("appr-1");
    let parked_at = dispatched_at + 40;
    runtime
        .journal
        .record_parked(
            &id,
            &parked_effect(),
            parked_at,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Both halves of a real resolution: the journal drops it from the parked
    // queue *and* the event log gains the resolution. Doing only the latter
    // would leave the task reading as still waiting — the assertion at the end
    // of this test is what pins the pair together.
    runtime.journal.record_resolved(&id).await.unwrap();
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::ApprovalResolved {
                approval_id: id.clone(),
                verdict: Verdict::Approve,
                by: Actor {
                    kind: ActorKind::User,
                    id: "u-1".into(),
                },
            },
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);

    let approval = only_approval(&body);
    let resolved_at = approval["atMillis"].as_u64().unwrap();
    assert!(
        resolved_at > parked_at,
        "the sleep did not outlast the park"
    );
    assert_eq!(
        approval["waitedMillis"].as_u64().unwrap(),
        resolved_at - parked_at,
        "the wait must be the real park→resolve span, not an inference",
    );
    assert_eq!(approval["label"], "Approval approved");

    // The join must not become a new identity leak: it reads `approval_id` and
    // `by.kind`, never `by.id`.
    let raw = serde_json::to_string(&body["timeline"]).unwrap();
    assert!(!raw.contains("u-1"), "operator identity leaked: {raw}");

    // The wait is over, so nothing is pending: no live figure.
    assert!(
        body.get("waitingSince").is_none(),
        "a resolved approval must not leave the task reading as still waiting",
    );
}

/// A wait that ended in a TTL sweep is still a wait, and must not read as a
/// human decision.
///
/// Expiry used to write *only* a journal record, so a default-deny-on-silence
/// produced no event at all — the single case where waiting is most costly was
/// the one case the timeline could not see. The sweep now also appends a
/// system-attributed `ApprovalResolved`, and the read side labels it as an
/// expiry: rendering "Approval denied" would claim somebody looked at it.
#[tokio::test]
async fn expired_approval_is_labelled_as_an_expiry_and_carries_its_wait() {
    use crate::ports::types::ApprovalId;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    let id = ApprovalId::new("appr-stale");
    let parked_at = dispatched_at + 40;
    runtime
        .journal
        .record_parked(
            &id,
            &parked_effect(),
            parked_at,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    // Re-park into the gate at epoch 0 so it is unambiguously past any TTL.
    runtime
        .approval_gate
        .rehydrate(id.clone(), parked_effect(), 0);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let expired = runtime.sweep_expired_approvals().await.unwrap();
    assert_eq!(expired, vec![id]);

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);

    let approval = only_approval(&body);
    assert_eq!(
        approval["label"], "Approval expired (auto-denied)",
        "an expiry must not read as though a human decided",
    );
    let resolved_at = approval["atMillis"].as_u64().unwrap();
    assert_eq!(
        approval["waitedMillis"].as_u64().unwrap(),
        resolved_at - parked_at,
        "an expired approval's wait is the span nobody answered in",
    );
}

/// An approval already parked when the task was dispatched charges this run only
/// for the part of its wait that overlapped the run.
///
/// Approvals carry no task id, so they are correlated to the dispatch window.
/// Without the clamp, an effect parked hours before this card was dispatched
/// would dump its whole backlog wait onto this task's header — a figure larger
/// than the task's own elapsed time, which is visibly wrong.
#[tokio::test]
async fn a_wait_that_began_before_dispatch_is_clamped_to_the_run_window() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, Verdict};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    // Parked a full hour before this task was ever dispatched.
    let id = ApprovalId::new("appr-old");
    runtime
        .journal
        .record_parked(
            &id,
            &parked_effect(),
            dispatched_at - 3_600_000,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    runtime.journal.record_resolved(&id).await.unwrap();
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::ApprovalResolved {
                approval_id: id,
                verdict: Verdict::Deny,
                by: Actor {
                    kind: ActorKind::Operator,
                    id: "owner".into(),
                },
            },
        )
        .await
        .unwrap();

    let (_, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    let approval = only_approval(&body);
    let resolved_at = approval["atMillis"].as_u64().unwrap();
    let waited = approval["waitedMillis"].as_u64().unwrap();
    assert_eq!(
        waited,
        resolved_at - dispatched_at,
        "the pre-dispatch hour must not be charged to this run",
    );
    assert!(waited < 3_600_000, "the clamp did not apply: {waited}");
    assert_eq!(approval["label"], "Approval denied");
}

/// A task parked on an operator *right now* reports it, even though no
/// resolution event exists yet.
///
/// This is the state the screen most needs to surface — "your agent is stopped,
/// waiting on you" — and it is invisible in the event log by construction: the
/// approval has not been resolved, so nothing has been appended. It comes from
/// the still-pending queue instead, scoped to the open run window.
#[tokio::test]
async fn a_currently_parked_approval_surfaces_as_a_live_wait() {
    use crate::ports::types::ApprovalId;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    let parked_at = dispatched_at + 10;
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-live"),
            &parked_effect(),
            parked_at,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["waitingSince"].as_u64().unwrap(),
        parked_at,
        "the live wait must start at the park instant",
    );
    // Nothing resolved, so nothing reached the timeline.
    assert!(
        body["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "approval"),
        "an unresolved approval must not fake a timeline row",
    );
}

/// A task that never waited reports no waiting at all — not a zero.
///
/// Both fields are `skip_serializing_if = "Option::is_none"`, so their absence
/// is what lets the console omit the figure entirely. If either were serialized
/// as `0`, every task on the board would grow a permanent "Waiting 0s", which
/// the issue calls out by name.
#[tokio::test]
async fn a_task_that_never_waited_reports_no_waiting_fields() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, _) = dispatched_task(&state, &company).await;

    runtime
        .events()
        .append(
            &company,
            CompanyEvent::DeskTaskCompleted {
                task_id: "t-1".into(),
                desk: "ceo".into(),
                output: "shipped".into(),
                column: "in_review".into(),
                artifact_ids: Vec::new(),
                origin_chat_id: None,
                origin_parent: None,
            },
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("waitingSince").is_none(),
        "a task with nothing parked must not report a live wait",
    );
    for entry in body["timeline"].as_array().unwrap() {
        assert!(
            entry.get("waitedMillis").is_none(),
            "a non-approval row must never carry a wait: {entry:?}",
        );
    }
}

// ── Issue #333: a task's Approvals tab shows that task's approvals ──────────
//
// The tab used to filter the *timeline* for `kind == "approval"`, which meant
// it could only ever show a resolution that fell inside the run window — and
// showed nothing at all for the state that matters most, an approval parked
// right now with the card stopped behind it. These pin the real query: the
// task id the runtime journal records with every parked effect.

/// **The acceptance test**: an approval raised while working a task appears on
/// that task's Approvals tab while it is still parked.
///
/// This is the QA repro — a request sitting on the main Approvals page while
/// the originating card's own tab read "No approvals in this run" — and it is
/// unreachable through the timeline by construction: nothing is appended to the
/// event log until somebody decides.
#[tokio::test]
async fn a_parked_approval_appears_on_its_own_task() {
    use crate::ports::types::ApprovalId;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    let parked_at = dispatched_at + 10;
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-mine"),
            &parked_effect(),
            parked_at,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);

    let approvals = body["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert_eq!(approvals[0]["id"], "appr-mine");
    assert_eq!(approvals[0]["status"], "pending");
    assert_eq!(approvals[0]["atMillis"].as_u64().unwrap(), parked_at);
    // #468 shrank this projection to what the card's one waiting line reads.
    // `kind`, `resolvedAtMillis` and `waitedMillis` left with the Approvals tab.
    for gone in ["kind", "resolvedAtMillis", "waitedMillis"] {
        assert!(
            approvals[0].get(gone).is_none(),
            "`{gone}` was dropped with the Approvals tab (#468)",
        );
    }
    // The timeline is untouched — a parked approval still has no event.
    assert!(
        body["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "approval"),
    );
}

/// **The acceptance test that the old window could not pass**: two cards worked
/// in the same window keep their own approvals.
///
/// Under the window correlation both rows landed on both tabs, because the only
/// question asked was "did this resolve while that card was running". The join
/// is an id now, so a card's tab shows its own sign-off and nothing else.
#[tokio::test]
async fn a_second_task_in_the_same_window_does_not_absorb_the_first_s_approvals() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, Verdict};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    // A second card, dispatched into the same open window as `t-1`.
    runtime
        .tasks()
        .upsert(
            &company,
            &TaskRecord {
                id: "t-2".into(),
                title: TaskTitle::authored("Also ship it"),
                note: None,
                column: "in_progress".into(),
                priority: "medium".into(),
                assignee: "ceo".into(),
                updated_at_millis: 1,
                origin: None,
                parent_task_id: None,
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: None,
                bounced: None,
            },
        )
        .await
        .unwrap();
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::TaskDispatched {
                task_id: "t-2".into(),
                run_id: None,
            },
        )
        .await
        .unwrap();

    // One approval each, both parked and resolved inside both windows.
    for (id, owner) in [("appr-one", "t-1"), ("appr-two", "t-2")] {
        let id = ApprovalId::new(id);
        runtime
            .journal
            .record_parked(
                &id,
                &parked_effect(),
                dispatched_at + 5,
                TaskLink::Task { id: owner.into() },
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        runtime.journal.record_resolved(&id).await.unwrap();
        runtime
            .events()
            .append(
                &company,
                CompanyEvent::ApprovalResolved {
                    approval_id: id,
                    verdict: Verdict::Approve,
                    by: Actor {
                        kind: ActorKind::User,
                        id: "u-1".into(),
                    },
                },
            )
            .await
            .unwrap();
    }

    for (task, own, other) in [
        ("t-1", "appr-one", "appr-two"),
        ("t-2", "appr-two", "appr-one"),
    ] {
        let (status, body) = send(
            &state,
            "GET",
            &format!("/api/v1/company/tasks/{task}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let ids: Vec<&str> = body["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![own], "{task} must own exactly its own approval");
        assert!(!ids.contains(&other));
        // And the timeline agrees — one surface, one correlation.
        let rows: Vec<&Value> = body["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "approval")
            .collect();
        assert_eq!(rows.len(), 1, "{task}: {rows:?}");
    }
}

/// A resolved approval keeps its row on the tab, carrying the verdict and the
/// wait it caused. The same resolution the main Approvals page performed, seen
/// from the card: approving on either surface reflects on both.
#[tokio::test]
async fn a_resolved_approval_reports_its_verdict_and_wait_on_the_tab() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, Verdict};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    let id = ApprovalId::new("appr-done");
    let parked_at = dispatched_at + 20;
    runtime
        .journal
        .record_parked(
            &id,
            &parked_effect(),
            parked_at,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    runtime.journal.record_resolved(&id).await.unwrap();
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::ApprovalResolved {
                approval_id: id,
                verdict: Verdict::Deny,
                by: Actor {
                    kind: ActorKind::Operator,
                    id: "owner".into(),
                },
            },
        )
        .await
        .unwrap();

    let (_, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    let approvals = body["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    let row = &approvals[0];
    assert_eq!(row["status"], "denied");
    // The row is anchored at the *park*, so approvals read in the order things
    // were asked rather than the order they were answered.
    assert_eq!(row["atMillis"].as_u64().unwrap(), parked_at);
    // The park→resolve span moved off this row with the Approvals tab (#468).
    // It is unchanged, and still asserted here — on the `approval` timeline
    // entry, which is where it now lives. Dropping the assertion along with the
    // field would have quietly retired the arithmetic's only coverage.
    let entry = body["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "approval")
        .expect("a resolved approval reaches the timeline");
    let resolved_at = entry["atMillis"].as_u64().unwrap();
    assert!(resolved_at > parked_at);
    assert_eq!(
        entry["waitedMillis"].as_u64().unwrap(),
        resolved_at - parked_at,
    );
    // Nothing is parked any more, so the card is not still waiting.
    assert!(body.get("waitingSince").is_none());
    // The join must not become a new identity leak.
    let raw = serde_json::to_string(&body["approvals"]).unwrap();
    assert!(!raw.contains("owner"), "operator identity leaked: {raw}");
}

/// A task with no approvals reports an empty list, not a fabricated one — the
/// honest empty state the console renders.
///
/// Covers *another card's* approval parked mid-window. The case where the
/// approval belongs to nothing at all is
/// [`an_unlinked_approval_is_not_absorbed_by_the_running_card`], which is a
/// different fact and was the one the old window got wrong.
#[tokio::test]
async fn a_task_with_no_approvals_of_its_own_reports_an_empty_list() {
    use crate::ports::types::ApprovalId;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    // Parked for a different card entirely, while this one is mid-run.
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-elsewhere"),
            &parked_effect(),
            dispatched_at + 5,
            TaskLink::Task {
                id: "t-other".into(),
            },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["approvals"].as_array().unwrap().is_empty(),
        "{:?}",
        body["approvals"],
    );
    assert!(
        body.get("waitingSince").is_none(),
        "another card's approval must not make this one read as waiting",
    );
}

/// An approval that belongs to **no** card — a workflow delivery, a chat turn,
/// a scheduler tick — parked while a card is mid-run must not be absorbed by
/// that card (#333 review follow-up).
///
/// This is the case the first cut of the ownership test got wrong. It tested
/// `origins.get(id).and_then(|o| o.task_id)`, which is `None` both for a park
/// that recorded no card *and* for a pre-#333 park that could not record one —
/// so every unlinked park since #333 fell through to the run window and landed
/// on whatever happened to be running, dragging `waitingSince` with it.
#[tokio::test]
async fn an_unlinked_approval_is_not_absorbed_by_the_running_card() {
    use crate::ports::types::ApprovalId;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    // The shape `workflows::delivery` writes: parked mid-window, owned by
    // nothing, and recorded as such.
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-delivery"),
            &parked_effect(),
            dispatched_at + 5,
            TaskLink::Unlinked,
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["approvals"].as_array().unwrap().is_empty(),
        "an approval owned by no card must not appear on a card: {:?}",
        body["approvals"],
    );
    assert!(
        body.get("waitingSince").is_none(),
        "nor may it make that card read as waiting on the operator",
    );
}

/// The two correlation keys, in the order the read side resolves them: the
/// attempt (`run_id`, #242) is authoritative wherever it is present, and the
/// parked card link (#333) is the fallback for every park with no attempt
/// behind it.
///
/// Neither key is a superset of the other, which is why both are kept. A
/// `RunRecord` names its card, so a run id resolves to a task — but a task id
/// can never say which *attempt* parked an approval, and #183 settled that
/// repeat trips through review are normal. Meanwhile `run_id` is `None` by
/// design for a chat turn, a workflow delivery, or the hosted brain's gate, so
/// it cannot be the only key either.
#[tokio::test]
async fn the_attempt_id_outranks_the_card_link_when_both_are_present() {
    use crate::ports::runs::NewRun;
    use crate::ports::types::ApprovalId;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    // Two attempts at this card, and one at another — the case a card-level key
    // alone cannot tell apart.
    for (id, task) in [("run-a", "t-1"), ("run-b", "t-1"), ("run-c", "t-other")] {
        runtime
            .runs()
            .create_run(&company, NewRun::for_task(id, task, "ceo"))
            .await
            .unwrap();
    }

    let under_run = |run: &str| {
        let mut effect = parked_effect();
        effect.run_id = Some(run.to_string());
        effect
    };

    // Parked under this card's *second* attempt, and stamped Unlinked at the
    // card level. The run id is authoritative, so it still lands here.
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-attempt-2"),
            &under_run("run-b"),
            dispatched_at + 5,
            TaskLink::Unlinked,
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    // Parked under another card's attempt, but stamped with *our* card. The run
    // id outranks the link, so it must not appear.
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-elsewhere"),
            &under_run("run-c"),
            dispatched_at + 6,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    let approvals = body["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert_eq!(
        approvals[0]["id"], "appr-attempt-2",
        "the attempt id decides ownership, not the card link",
    );
}

/// The **queue** answers ownership the same way the card does (#1891).
///
/// [`the_attempt_id_outranks_the_card_link_when_both_are_present`] pins the task
/// detail read. `GET …/approvals` projected the raw park stamp instead, so the
/// two surfaces disagreed about the same approval: the card refused to show
/// `appr-elsewhere` and the queue handed it out labelled `t-1`. Every console
/// join on that link — the board's blocked row, the Approvals page's per-card
/// filter — inherited the disagreement.
///
/// Read-only that was a wrong label. Once the board card grew Approve and
/// Decline it became an operator resolving another card's request, so the two
/// reads are pinned against each other here rather than left to agree by
/// convention.
#[tokio::test]
async fn the_queue_resolves_ownership_the_same_way_the_card_does() {
    use crate::ports::runs::NewRun;
    use crate::ports::types::ApprovalId;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    for (id, task) in [("run-b", "t-1"), ("run-c", "t-other")] {
        runtime
            .runs()
            .create_run(&company, NewRun::for_task(id, task, "ceo"))
            .await
            .unwrap();
    }

    let under_run = |run: &str| {
        let mut effect = parked_effect();
        effect.run_id = Some(run.to_string());
        effect
    };

    // Stamped with this card, parked under another card's attempt. The card
    // read refuses it; the queue must not label it `t-1` either.
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-elsewhere"),
            &under_run("run-c"),
            dispatched_at + 5,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    // Stamped Unlinked *and* carrying a run id — which `workflow_run_of` reads
    // as a workflow park, because the two id spaces are indistinguishable by
    // value. The card read claims it (it checks membership in this card's own
    // attempt ids, which the queue has no way to do); the queue leaves it
    // alone rather than risk relabelling a workflow approval onto a card.
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-attempt-2"),
            &under_run("run-b"),
            dispatched_at + 6,
            TaskLink::Unlinked,
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    // No attempt at all: the stamp is the whole answer, unchanged.
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-stamped"),
            &parked_effect(),
            dispatched_at + 7,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/approvals", None).await;
    assert_eq!(status, StatusCode::OK);
    let queue = body.as_array().unwrap();
    let owner_of = |id: &str| {
        queue
            .iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from the queue: {queue:?}"))["task"]
            .clone()
    };

    assert_eq!(
        owner_of("appr-elsewhere"),
        json!({ "link": "task", "id": "t-other" }),
        "the attempt outranks the stamp on the queue, exactly as on the card",
    );
    assert_eq!(
        owner_of("appr-attempt-2"),
        json!({ "link": "unlinked" }),
        "an Unlinked park carrying a run id is a workflow park by `workflow_run_of`'s \
         rule, and the queue must not claim it for a card on the strength of an id \
         whose space it cannot identify",
    );
    assert_eq!(
        owner_of("appr-stamped"),
        json!({ "link": "task", "id": "t-1" }),
        "a park with no attempt keeps the link it was stamped with",
    );

    // The pinning half, and it is a **subset** rather than an equality, which is
    // the honest shape of the guarantee.
    //
    // The queue may never claim an approval the card does not — that direction
    // is the defect, and it is what puts a decision the operator should not
    // have in front of them. It may fall short: `approval_owner` asks whether a
    // run is among *this card's* attempts, which the queue cannot ask without
    // per-card state, so where the id space is ambiguous the queue abstains.
    // The cost of abstaining is a blocked row the board does not draw; the cost
    // of the other direction is deciding somebody else's request.
    let (_, card) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    let on_card: std::collections::HashSet<&str> = card["approvals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    let from_queue: std::collections::HashSet<&str> = queue
        .iter()
        .filter(|row| row["task"] == json!({ "link": "task", "id": "t-1" }))
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert!(
        from_queue.is_subset(&on_card),
        "the queue must never put an approval on a card the card itself disowns: \
         queue={from_queue:?} card={on_card:?}",
    );
    assert!(
        from_queue.contains("appr-stamped"),
        "and must still carry the unambiguous ones: {from_queue:?}",
    );
    assert!(
        !from_queue.contains("appr-elsewhere"),
        "least of all the one parked under another card's attempt: {from_queue:?}",
    );
}

/// An approval parked by a build older than #333 carries no link at all. It
/// keeps the pre-#333 run-window correlation rather than vanishing, so existing
/// history still renders.
///
/// The legacy line is written **raw and replayed**, not produced by
/// `record_parked` — which is the point. Since #333 there is no way to record a
/// park without a link, so the only source of a missing one is a file written
/// by an older host, and that is exactly what this pins. Contrast
/// [`an_unlinked_approval_is_not_absorbed_by_the_running_card`]: same "no task
/// id", opposite outcome, because one is unrecorded and the other is recorded.
#[tokio::test]
async fn a_pre_333_approval_falls_back_to_the_run_window() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, Verdict};
    use crate::store::paths::Bundle;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    // A journal line as an older host wrote it: no `task` key whatsoever.
    let legacy = json!({
        "record": "ApprovalParked",
        "id": "appr-legacy",
        "effect": parked_effect(),
        "at_millis": dispatched_at + 5,
    });
    let path = Bundle::new(&home, runtime.id()).journal_jsonl();
    tokio::fs::write(&path, format!("{legacy}\n"))
        .await
        .unwrap();
    runtime.journal.load().await.unwrap();

    let (_, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    let approvals = body["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert_eq!(approvals[0]["id"], "appr-legacy");
    assert_eq!(
        body["waitingSince"].as_u64().unwrap(),
        dispatched_at + 5,
        "the legacy live-wait behaviour is unchanged",
    );

    let id = ApprovalId::new("appr-legacy");
    runtime.journal.record_resolved(&id).await.unwrap();
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::ApprovalResolved {
                approval_id: id,
                verdict: Verdict::Approve,
                by: Actor {
                    kind: ActorKind::User,
                    id: "u-1".into(),
                },
            },
        )
        .await
        .unwrap();

    let (_, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    let approvals = body["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert_eq!(approvals[0]["id"], "appr-legacy");
    assert_eq!(approvals[0]["status"], "approved");
    // As above (#468): the span lives on the timeline entry now, and the legacy
    // clamping behaviour is asserted there rather than dropped.
    let entry = body["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "approval")
        .expect("a resolved approval reaches the timeline");
    let resolved_at = entry["atMillis"].as_u64().unwrap();
    assert_eq!(
        entry["waitedMillis"].as_u64().unwrap(),
        resolved_at.saturating_sub(dispatched_at + 5),
        "the resolved legacy row keeps the original park-to-resolve wait",
    );
}

/// #185 review follow-up: the lineage forest is enforced at the write boundary.
///
/// Without this a card could be its own parent (appearing as both parent and
/// child of itself in `task_detail`), point at a card that does not exist, or
/// close a `t1 → t2 → t1` loop — all persisted silently.
#[tokio::test]
async fn parent_task_id_rejects_self_unknown_and_cycles() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let create = |title: &str| {
        let title = title.to_string();
        async move { json!({ "title": title }) }
    };
    let (_, a) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(create("A").await),
    )
    .await;
    let (_, b) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(create("B").await),
    )
    .await;
    let (a_id, b_id) = (
        a["id"].as_str().unwrap().to_string(),
        b["id"].as_str().unwrap().to_string(),
    );

    // Unknown parent on create.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({ "title": "C", "parentTaskId": "nope" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Self-parenting on patch.
    let (status, _) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/tasks/{a_id}"),
        Some(json!({ "parentTaskId": a_id })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A legitimate edge: B's parent is A.
    let (status, _) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/tasks/{b_id}"),
        Some(json!({ "parentTaskId": a_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // …which makes A → B a cycle.
    let (status, _) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/tasks/{a_id}"),
        Some(json!({ "parentTaskId": b_id })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "A → B → A must be rejected"
    );
}

/// #185 review follow-up: validation is only as good as its atomicity.
///
/// Each half of `A → B` / `B → A` is individually legal against a board that
/// has neither edge yet. Read → validate → write therefore has to be one
/// critical section: without it both requests can validate against a snapshot
/// taken before the other wrote, and the pair persists the very cycle
/// `validate_parent` exists to reject.
///
/// With the writes serialized this is deterministic rather than probabilistic —
/// whichever request takes the lock second sees the first one's edge and is
/// rejected — so the assertion is *exactly* one success, not "usually one".
#[tokio::test]
async fn concurrent_reparents_cannot_race_a_cycle_onto_the_board() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = std::sync::Arc::new(state_with_company(&home).await);

    let mut ids = Vec::new();
    for title in ["A", "B"] {
        let (_, card) = send(
            &state,
            "POST",
            "/api/v1/company/tasks",
            Some(json!({ "title": title })),
        )
        .await;
        ids.push(card["id"].as_str().unwrap().to_string());
    }
    let (a_id, b_id) = (ids[0].clone(), ids[1].clone());

    // Fire both halves of the would-be cycle at once.
    let reparent = |child: String, parent: String| {
        let state = state.clone();
        tokio::spawn(async move {
            send(
                &state,
                "PATCH",
                &format!("/api/v1/company/tasks/{child}"),
                Some(json!({ "parentTaskId": parent })),
            )
            .await
            .0
        })
    };
    let first = reparent(b_id.clone(), a_id.clone());
    let second = reparent(a_id.clone(), b_id.clone());
    let (first, second) = (first.await.unwrap(), second.await.unwrap());

    let outcomes = [first, second];
    assert_eq!(
        outcomes.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "exactly one re-parent may win: {outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|s| **s == StatusCode::BAD_REQUEST)
            .count(),
        1,
        "the loser must be rejected as a cycle, not silently applied: {outcomes:?}"
    );

    // And the board itself is a forest: the two cards cannot both have parents.
    let (_, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    let parented = board
        .as_array()
        .expect("board is a list")
        .iter()
        .filter(|c| c["parentTaskId"].is_string())
        .count();
    assert_eq!(parented, 1, "a cycle reached the board: {board}");
}

// ---------------------------------------------------------------------------
// Who may decide on the company's behalf (issue #403)
// ---------------------------------------------------------------------------

/// Sends with an explicit cookie, so the role boundary can be driven with a
/// member session rather than the harness admin.
async fn send_cookie(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
    cookie: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie);
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Every route that decides what this company reaches the outside world *as* —
/// which credential it presents, which third-party account its agents act
/// through, where its mail and its model calls go — refuses a member.
///
/// One table rather than a test per module, on purpose. The gap issue #403
/// reported was not that one route forgot a check; it was that a whole plane
/// shared an extractor whose name did not suggest "any member may write". A
/// per-module test would have let the next route added to that plane be added
/// without one. This list is the plane, and a new route joins it here.
///
/// The assertion is `403` specifically, not merely "not 200": a `404` or a
/// `409` would also be non-200 while meaning the route simply did not run, and
/// that would pass a test which proves nothing.
#[tokio::test]
async fn a_member_cannot_change_what_the_company_reaches_the_world_as() {
    let home_dir = home();
    let state = state_with_company(home_dir.path()).await;
    let member =
        crate::server::test_support::seed_session(&state, "acme", crate::ports::UserRole::Member)
            .await;

    let cases: Vec<(&str, &str, Option<Value>)> = vec![
        // The company's Composio identity, and the accounts its agents use.
        (
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": "x" })),
        ),
        (
            "POST",
            "/api/v1/company/composio/authorize",
            Some(json!({ "toolkit": "gmail" })),
        ),
        // Revoking one of those accounts is the same decision as choosing it
        // (issue #404) — a member who cannot connect must not be able to
        // disconnect either.
        (
            "DELETE",
            "/api/v1/company/composio/connections/conn-1",
            None,
        ),
        // And choosing WHICH of two accounts every agent acts as (issue #820) —
        // the same decision again, one step finer: it does not change what the
        // company is connected to, only what it sends as, which is precisely
        // the kind of company-wide answer this plane exists to hold.
        (
            "PUT",
            "/api/v1/company/composio/connections/conn-1/default",
            None,
        ),
        (
            "DELETE",
            "/api/v1/company/composio/connections/conn-1/default",
            None,
        ),
        // The model every agent thinks with, and the key it is billed against.
        (
            "PUT",
            "/api/v1/company/inference",
            Some(json!({ "provider": "openai_compatible", "baseUrl": "https://example.test" })),
        ),
        ("DELETE", "/api/v1/company/inference", None),
        // The company's outbound mail identity — and a send from its address.
        (
            "PUT",
            "/api/v1/company/smtp",
            Some(
                json!({ "provider": "smtp", "host": "mail.example.test", "port": 587,
                         "username": "u", "password": "p", "from_email": "a@example.test" }),
            ),
        ),
        (
            "POST",
            "/api/v1/company/smtp/test",
            Some(json!({ "to": "elsewhere@example.test" })),
        ),
        (
            "PUT",
            "/api/v1/company/domain",
            Some(json!({"domain": "x.test"})),
        ),
        // Which tool servers exist, and the credentials they carry.
        (
            "POST",
            "/api/v1/company/mcp/servers",
            Some(json!({ "name": "evil", "endpoint": "https://example.test" })),
        ),
        (
            "PUT",
            "/api/v1/company/mcp/servers/anything",
            Some(json!({ "endpoint": "https://example.test" })),
        ),
        ("DELETE", "/api/v1/company/mcp/servers/anything", None),
        (
            "POST",
            "/api/v1/company/mcp/servers/anything/oauth/start",
            None,
        ),
    ];

    for (method, uri, body) in cases {
        let (status, response) = send_cookie(&state, method, uri, body, &member).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} let a member through: {response}"
        );
        assert_eq!(
            response["code"], "forbidden",
            "{method} {uri} refused without saying why: {response}"
        );
    }
}

/// The counterpart to the table above, on the same two surfaces: a member is
/// let *through* the reads.
///
/// `docs/modules/server/authority.md` asserts in prose that reads on these
/// surfaces stay open to any member — they carry non-secret routing and never a
/// credential — and until now nothing pinned it. `GET …/domain` and
/// `GET …/smtp` are new (issue #1460), and the easy mistake when adding a route
/// to a module whose every other handler takes `AdminScopedCompany` is to reach
/// for the same extractor: the console's Settings screen would then `403` for
/// every member while the identical data stayed readable to them over GraphQL
/// as `Company.domain` and `Company.smtp`.
///
/// `200` specifically, not merely "not 403": these read stored config that may
/// be absent, and both answer that case with a body rather than a status, so
/// anything else would mean the route did not run.
#[tokio::test]
async fn a_member_may_read_what_the_company_reaches_the_world_as() {
    let home_dir = home();
    let state = state_with_company(home_dir.path()).await;
    let member =
        crate::server::test_support::seed_session(&state, "acme", crate::ports::UserRole::Member)
            .await;

    for uri in ["/api/v1/company/domain", "/api/v1/company/smtp"] {
        let (status, response) = send_cookie(&state, "GET", uri, None, &member).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "GET {uri} refused a member: {response}"
        );
    }
}

/// The other side, on the same table: the harness admin is refused by none of
/// them on role grounds.
///
/// Several answer `409`/`404`/`502` for their own reasons — no feature in this
/// build, no such server, no reachable host — and that is the point. What must
/// never appear is `403`, which would mean the guard caught the wrong person
/// and the fix had quietly removed the capability instead of assigning it.
#[tokio::test]
async fn an_admin_is_refused_by_none_of_them() {
    let home_dir = home();
    let state = state_with_company(home_dir.path()).await;

    let cases: Vec<(&str, &str, Option<Value>)> = vec![
        (
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": "x" })),
        ),
        (
            "PUT",
            "/api/v1/company/domain",
            Some(json!({"domain": "x.test"})),
        ),
        (
            "POST",
            "/api/v1/company/mcp/servers",
            Some(json!({ "name": "svc", "endpoint": "https://example.test" })),
        ),
    ];

    for (method, uri, body) in cases {
        let (status, response) = send(&state, method, uri, body).await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} refused an admin: {response}"
        );
    }
}

/// Issue #552: a published deliverable lives on two surfaces, and the console's
/// workspace `PUT` is where an operator edits one. Saving that note must record
/// an **operator version** on the artifact chain, because that edit is exactly
/// the datum `human_edit_diff` exists to answer — and overwriting only the node
/// would leave the history claiming the agent's draft shipped unchanged.
///
/// The ordering is asserted too, by refusing the node write: an artifact
/// stamped with a node id the tree does not have makes the chain append
/// succeed and the node write fail, and the version must still be there
/// afterwards. Chain-ahead-of-node is the survivable direction and
/// node-ahead-of-chain is the silent one, so a failed save must land on the
/// first.
#[tokio::test]
async fn saving_a_published_note_records_the_operators_edit_on_the_artifact() {
    use crate::ports::artifacts::{ArtifactKind, ArtifactRecord, ArtifactStore};
    use crate::ports::workspace::WorkspaceStore;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = state.registry().list()[0].clone();
    let runtime = state.registry().get(&company).expect("company");

    // A note in the tree…
    let (status, note) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "launch.md", "kind": "file", "content": "the agent's draft"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let node_id = note["id"].as_str().expect("node id").to_string();

    // …that is the projection of a published artifact.
    let mut published = ArtifactRecord::new(
        "art-1",
        "t-1",
        "Launch spec",
        ArtifactKind::Markdown,
        "the agent's draft",
        "ceo",
        1,
    )
    .with_source("launch.md");
    published.stamp_workspace_node(&node_id);
    ArtifactStore::upsert(runtime.artifacts().as_ref(), &company, &published)
        .await
        .expect("seed");

    let (status, _) = send(
        &state,
        "PUT",
        &format!("/api/v1/company/workspace/file/{node_id}"),
        Some(json!({"content": "the operator's rewrite"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, artifact) = send(&state, "GET", "/api/v1/company/artifacts/art-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(artifact["versions"].as_array().unwrap().len(), 2);
    assert_eq!(artifact["versions"][1]["body"], "the operator's rewrite");
    assert_eq!(artifact["versions"][1]["author"], "operator");
    assert_eq!(
        artifact["versions"][1]["note"], "operator edit before approval",
        "the wording the console recognises, shared with the append route"
    );
    assert_eq!(
        artifact["versions"][1]["workspaceNodeId"], node_id,
        "the appended version must inherit the node, or the NEXT save mirrors nothing"
    );
    assert!(
        artifact["humanEditDiff"].is_object(),
        "the whole point: a console edit of a deliverable is now diffable"
    );

    // And the node itself carries the operator's text.
    let (node, body) = WorkspaceStore::read(runtime.workspace().as_ref(), &company, &node_id)
        .await
        .unwrap()
        .expect("the note still exists");
    assert_eq!(body, "the operator's rewrite");
    assert_eq!(
        node.updated_by,
        crate::ports::workspace::WorkspaceOrigin::Operator
    );

    // -- and now the ordering, with the node write refused ------------------
    //
    // A deliverable whose node the operator deleted still carries that node's
    // id on its latest version, so the reverse lookup matches and the append
    // runs — then the write fails, because the node is gone. That is the
    // failure this route's ordering was chosen for, and it is reachable
    // without a mock: the refusal comes from the real store.
    let mut orphaned = ArtifactRecord::new(
        "art-2",
        "t-1",
        "Retired spec",
        ArtifactKind::Markdown,
        "the agent's draft",
        "ceo",
        1,
    )
    .with_source("retired.md");
    orphaned.stamp_workspace_node("node-the-operator-deleted");
    ArtifactStore::upsert(runtime.artifacts().as_ref(), &company, &orphaned)
        .await
        .expect("seed");

    let (status, _) = send(
        &state,
        "PUT",
        "/api/v1/company/workspace/file/node-the-operator-deleted",
        Some(json!({"content": "an edit the tree cannot take"})),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::OK,
        "the node write must fail — there is no such node"
    );

    let (status, artifact) = send(&state, "GET", "/api/v1/company/artifacts/art-2", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        artifact["versions"].as_array().unwrap().len(),
        2,
        "the version must survive the refused node write: chain-ahead-of-node is \
         the direction that heals, and this is the ordering that guarantees it"
    );
    assert_eq!(
        artifact["versions"][1]["body"],
        "an edit the tree cannot take"
    );
}

/// Nearly every note in the tree is an ordinary note, not a deliverable.
/// Saving one must append nothing anywhere — the reverse lookup answering
/// "no artifact owns this" is the common case, and deliberately silent.
#[tokio::test]
async fn saving_an_unpublished_note_appends_no_artifact_version() {
    use crate::ports::artifacts::{ArtifactKind, ArtifactRecord, ArtifactStore};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = state.registry().list()[0].clone();
    let runtime = state.registry().get(&company).expect("company");

    // A published artifact exists, but points at a DIFFERENT node.
    let mut published = ArtifactRecord::new(
        "art-1",
        "t-1",
        "Launch spec",
        ArtifactKind::Markdown,
        "deliverable",
        "ceo",
        1,
    );
    published.stamp_workspace_node("some-other-node");
    ArtifactStore::upsert(runtime.artifacts().as_ref(), &company, &published)
        .await
        .expect("seed");

    let (_, note) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "notes.md", "kind": "file", "content": "just a note"})),
    )
    .await;
    let node_id = note["id"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "PUT",
        &format!("/api/v1/company/workspace/file/{node_id}"),
        Some(json!({"content": "still just a note"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, artifact) = send(&state, "GET", "/api/v1/company/artifacts/art-1", None).await;
    assert_eq!(
        artifact["versions"].as_array().unwrap().len(),
        1,
        "an ordinary note's save must not touch an unrelated artifact"
    );
}

/// The other direction of the same invariant: appending a version through the
/// Artifacts tab must push the new body into the deliverable's workspace note,
/// or the tree keeps serving a draft the history has superseded.
#[tokio::test]
async fn appending_an_artifact_version_updates_its_workspace_note() {
    use crate::ports::artifacts::{ArtifactKind, ArtifactRecord, ArtifactStore};
    use crate::ports::workspace::WorkspaceStore;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = state.registry().list()[0].clone();
    let runtime = state.registry().get(&company).expect("company");

    let (_, note) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "launch.md", "kind": "file", "content": "v1"})),
    )
    .await;
    let node_id = note["id"].as_str().unwrap().to_string();

    let mut published = ArtifactRecord::new(
        "art-1",
        "t-1",
        "Launch spec",
        ArtifactKind::Markdown,
        "v1",
        "ceo",
        1,
    )
    .with_source("launch.md");
    published.stamp_workspace_node(&node_id);
    ArtifactStore::upsert(runtime.artifacts().as_ref(), &company, &published)
        .await
        .expect("seed");

    let (status, appended) = send(
        &state,
        "POST",
        "/api/v1/company/artifacts/art-1/versions",
        Some(json!({"body": "v2, edited in the Artifacts tab"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        appended["versions"][1]["workspaceNodeId"], node_id,
        "the appended version keeps naming the node it lives in"
    );

    let (_, body) = WorkspaceStore::read(runtime.workspace().as_ref(), &company, &node_id)
        .await
        .unwrap()
        .expect("the note exists");
    assert_eq!(
        body, "v2, edited in the Artifacts tab",
        "the shared tree must not keep serving a superseded draft"
    );
}

/// An artifact with no workspace note — a legacy capture, or one recorded
/// while no tree was wired — appends exactly as it always did, with no node
/// write attempted and nothing invented for it.
#[tokio::test]
async fn appending_to_an_unmirrored_artifact_touches_no_note() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/artifacts",
        Some(json!({"taskId": "t-1", "title": "Draft", "body": "v1"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_string();

    let (status, appended) = send(
        &state,
        "POST",
        &format!("/api/v1/company/artifacts/{id}/versions"),
        Some(json!({"body": "v2"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(appended["versions"].as_array().unwrap().len(), 2);
    assert!(
        appended["versions"][1].get("workspaceNodeId").is_none(),
        "nothing may invent a node for an artifact that has none"
    );
}

/// An artifact store with one chosen fault, so a test can ask for exactly the
/// failure it means: unreadable (`list`) or unwritable (`upsert`).
struct FaultyArtifacts {
    listed: Vec<crate::ports::artifacts::ArtifactRecord>,
    list_fails: bool,
    upsert_fails: bool,
}

#[async_trait::async_trait]
impl crate::ports::artifacts::ArtifactStore for FaultyArtifacts {
    async fn list(
        &self,
        _: &CompanyId,
        _: Option<&str>,
    ) -> crate::Result<Vec<crate::ports::artifacts::ArtifactRecord>> {
        if self.list_fails {
            return Err(crate::error::OpenCompanyError::Store(
                "the artifact store is down".into(),
            ));
        }
        Ok(self.listed.clone())
    }
    async fn get(
        &self,
        _: &CompanyId,
        _: &str,
    ) -> crate::Result<Option<crate::ports::artifacts::ArtifactRecord>> {
        Ok(None)
    }
    async fn upsert(
        &self,
        _: &CompanyId,
        _: &crate::ports::artifacts::ArtifactRecord,
    ) -> crate::Result<()> {
        if self.upsert_fails {
            return Err(crate::error::OpenCompanyError::Store(
                "the disk is full".into(),
            ));
        }
        Ok(())
    }
    async fn delete(&self, _: &CompanyId, _: &str) -> crate::Result<bool> {
        Ok(false)
    }
}

/// [`state_with_company`] with the artifact store swapped for a faulty one, so
/// the workspace `PUT` can be exercised against a store that will not answer.
async fn state_with_faulty_artifacts(
    home: &std::path::Path,
    artifacts: FaultyArtifacts,
) -> (AppState, CompanyId) {
    let state = state_with_company(home).await;
    let company = state.registry().list()[0].clone();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
        .with_id(company.clone())
        .with_artifacts(std::sync::Arc::new(artifacts))
        .build()
        .await
        .expect("runtime");
    // `insert` replaces, so the routes now resolve through the faulty store
    // while the seeded admin on `state` carries over untouched.
    state
        .registry()
        .insert(company.clone(), std::sync::Arc::new(runtime));
    (state, company)
}

/// Issue #552 made every note save consult the artifact store, and an ordinary
/// note must not inherit that store's health.
///
/// Nearly the whole tree is ordinary notes. They own no artifact chain, and
/// their save touches the artifact store for one reason only — to ask whether
/// they are a deliverable. When that question cannot be answered, refusing the
/// save would discard an operator's typing to protect a chain the note does not
/// have.
#[tokio::test]
async fn an_ordinary_note_still_saves_when_the_artifact_store_cannot_be_read() {
    use crate::ports::workspace::WorkspaceStore;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, company) = state_with_faulty_artifacts(
        &home,
        FaultyArtifacts {
            listed: Vec::new(),
            list_fails: true,
            upsert_fails: false,
        },
    )
    .await;
    let runtime = state.registry().get(&company).expect("company");

    let (_, note) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "notes.md", "kind": "file", "content": "just a note"})),
    )
    .await;
    let node_id = note["id"].as_str().expect("node id").to_string();

    let (status, _) = send(
        &state,
        "PUT",
        &format!("/api/v1/company/workspace/file/{node_id}"),
        Some(json!({"content": "the operator kept typing"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an unreadable artifact store must not reject a plain note's save"
    );

    let (_, body) = WorkspaceStore::read(runtime.workspace().as_ref(), &company, &node_id)
        .await
        .unwrap()
        .expect("the note still exists");
    assert_eq!(
        body, "the operator kept typing",
        "the edit must actually land, not merely report success"
    );
}

/// The other direction, and the one the availability fix must not have cost:
/// once the store *has* answered and named this node a published deliverable,
/// a version that cannot be recorded still refuses the save.
///
/// This is the fail-closed guarantee the module exists for. A node written
/// behind a version that was never appended is the silent, permanent direction
/// — `human_edit_diff` would answer for a draft the operator had already
/// rewritten.
#[tokio::test]
async fn a_published_note_refuses_the_save_when_its_version_cannot_be_recorded() {
    use crate::ports::artifacts::{ArtifactKind, ArtifactRecord};
    use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();

    // The store answers the lookup — this node IS a deliverable — but refuses
    // the append.
    let mut published = ArtifactRecord::new(
        "art-1",
        "t-1",
        "Launch spec",
        ArtifactKind::Markdown,
        "the agent's draft",
        "ceo",
        1,
    );
    published.stamp_workspace_node("node-published");
    let (state, company) = state_with_faulty_artifacts(
        &home,
        FaultyArtifacts {
            listed: vec![published],
            list_fails: false,
            upsert_fails: true,
        },
    )
    .await;
    let runtime = state.registry().get(&company).expect("company");

    // The node the artifact points at, created directly so its id is the one
    // the record was stamped with.
    WorkspaceStore::create(
        runtime.workspace().as_ref(),
        &company,
        &WorkspaceNode {
            id: "node-published".to_string(),
            name: "launch.md".to_string(),
            kind: NodeKind::File,
            parent_id: None,
            updated_at_millis: 1,
            created_by: WorkspaceOrigin::Operator,
            updated_by: WorkspaceOrigin::Operator,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        },
        Some("the agent's draft"),
    )
    .await
    .expect("seed the node");

    let (status, _) = send(
        &state,
        "PUT",
        "/api/v1/company/workspace/file/node-published",
        Some(json!({"content": "the operator's rewrite"})),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::OK,
        "a deliverable whose version cannot be recorded must not have its node written"
    );

    let (_, body) = WorkspaceStore::read(runtime.workspace().as_ref(), &company, "node-published")
        .await
        .unwrap()
        .expect("the note still exists");
    assert_eq!(
        body, "the agent's draft",
        "the node must be untouched — writing it would strand the chain behind it"
    );
}

// ---------------------------------------------------------------------------
// The plan → workflow bridge: apply / reject a proposal (issue #580)
// ---------------------------------------------------------------------------

/// Seeds a card sitting In Review with a `workflow` deliverable and the given
/// proposal graph, straight through the task store (the builder pass that would
/// normally mint it is behind the `openhuman` feature). Returns the card id.
async fn seed_proposal_card(state: &AppState, ops: Value) -> String {
    seed_proposal_card_assigned(state, ops, "ceo").await
}

/// [`seed_proposal_card`], with the assignee set to whatever the caller
/// passes rather than the hardcoded `"ceo"` — for proving the owning-desk
/// default against a card assigned directly to a desk (issue #1882 review),
/// where `assignee` is the desk's own canonical id rather than a teammate's.
async fn seed_proposal_card_assigned(state: &AppState, ops: Value, assignee: &str) -> String {
    let runtime = state
        .registry()
        .get(&CompanyId::new("acme"))
        .expect("company");
    let id = crate::ports::generate_id();
    let record = TaskRecord {
        id: id.clone(),
        title: TaskTitle::authored("Automate the weekly digest"),
        note: None,
        column: "in_review".to_string(),
        priority: "medium".to_string(),
        assignee: assignee.to_string(),
        updated_at_millis: 1,
        origin: None,
        parent_task_id: None,
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Workflow,
        workflow_proposal: Some(crate::ports::tasks::TaskWorkflowProposal {
            summary: "Email the digest".to_string(),
            ops,
            generated_at_millis: 1,
            run_id: "run-build-1".to_string(),
        }),
        origin_run_id: None,
        origin_workflow_id: None,
        origin_message_seq: None,
        bounced: None,
    };
    runtime
        .tasks()
        .upsert(runtime.id(), &record)
        .await
        .expect("seed the proposal card");
    id
}

/// A valid two-node graph (trigger → agent) whose agent names a real roster
/// teammate. `schedule` arms the trigger when `Some`.
fn digest_ops(schedule: Option<&str>) -> Value {
    let mut trigger = json!({ "id": "start", "kind": "trigger", "name": "Start" });
    if let Some(cron) = schedule {
        trigger["schedule"] = json!(cron);
    }
    json!({
        "id": "weekly-digest",
        "name": "Weekly digest",
        "description": "Email the weekly digest",
        "nodes": [
            trigger,
            { "id": "write", "kind": "agent", "name": "Draft it", "agent": "ceo" }
        ],
        "edges": [{ "from": "start", "to": "write" }]
    })
}

/// Applying a manual-trigger proposal creates the workflow, stamps the card's
/// output link to the build attempt, finishes the card in Done, and clears the
/// proposal — the whole happy path in one assertion set.
#[tokio::test]
async fn applying_a_proposal_creates_the_workflow_and_finishes_the_card() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let id = seed_proposal_card(&state, digest_ops(None)).await;

    let (status, card) = send(
        &state,
        "POST",
        &format!("/api/v1/company/tasks/{id}/workflow-proposal/apply"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{card}");
    // Done is reached — the create path is the human approval the epic requires.
    assert_eq!(card["column"], "done");
    // The proposal is consumed, and the card links to the workflow it created and
    // to the attempt that built it (issue #339).
    assert!(card.get("workflowProposal").is_none(), "{card}");
    assert_eq!(card["output"]["runId"], "run-build-1");
    assert_eq!(
        card["output"]["workflows"][0]["workflowId"],
        "weekly-digest"
    );
    assert_eq!(card["output"]["workflows"][0]["action"], "created");

    // The workflow now exists in the company's list — and, with no schedule, it
    // is armed (nothing to disarm).
    let (status, workflows) = send(&state, "GET", "/api/v1/company/workflows", None).await;
    assert_eq!(status, StatusCode::OK);
    let created = workflows
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == "weekly-digest")
        .expect("the created workflow is listed");
    assert_eq!(created["enabled"], true, "a manual trigger is not disarmed");
}

/// Issue #1862 prerequisite: a proposal that names no `ownerDesk` defaults to
/// the proposing card's assignee's desk. `seed_proposal_card` assigns the
/// card to `ceo`, and `desk_manifest` seats `ceo` on the `engineering` desk —
/// so the created workflow must come out owned by `engineering` even though
/// `digest_ops` never mentions it.
///
/// This reads the default back off the persisted overlay TOML directly,
/// rather than the `GET …/workflows/{id}` response (which now also projects
/// `ownerDesk`, see `WorkflowGraph::owner_desk`) — pinning the actual stored
/// effect of the defaulting logic, independent of the read projection.
#[tokio::test]
async fn applying_a_proposal_defaults_the_owner_desk_from_the_assignees_desk() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let id = seed_proposal_card(&state, digest_ops(None)).await;

    let (status, card) = send(
        &state,
        "POST",
        &format!("/api/v1/company/tasks/{id}/workflow-proposal/apply"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{card}");

    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.clone());
    let record = store.load(&CompanyId::new("acme")).await.unwrap().unwrap();
    let overlay = record
        .overlay_workflows
        .iter()
        .find(|w| w.id == "weekly-digest")
        .expect("the created workflow is saved as an overlay");
    let file = crate::company::parse_workflow(&overlay.toml).expect("saved TOML parses");
    assert_eq!(
        file.owner_desk.as_deref(),
        Some("engineering"),
        "the assignee's desk fills the omitted owner_desk"
    );
}

/// **Regression, issue #1882 review — blank must default the same as absent.**
/// A stored proposal that names `ownerDesk` as a blank/whitespace string (a
/// builder pass that emits the key but leaves it empty, rather than omitting
/// it) must still fall through to the assignee-desk default. Before the fix,
/// `Some("   ")` passed the `is_none()` gate in `apply_workflow_proposal`, so
/// the default never ran and the blank string was persisted as the "owner"
/// instead.
#[tokio::test]
async fn applying_a_proposal_with_a_blank_owner_desk_still_defaults_it() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let mut ops = digest_ops(None);
    ops["ownerDesk"] = json!("   ");
    let id = seed_proposal_card(&state, ops).await;

    let (status, card) = send(
        &state,
        "POST",
        &format!("/api/v1/company/tasks/{id}/workflow-proposal/apply"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{card}");

    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.clone());
    let record = store.load(&CompanyId::new("acme")).await.unwrap().unwrap();
    let overlay = record
        .overlay_workflows
        .iter()
        .find(|w| w.id == "weekly-digest")
        .expect("the created workflow is saved as an overlay");
    let file = crate::company::parse_workflow(&overlay.toml).expect("saved TOML parses");
    assert_eq!(
        file.owner_desk.as_deref(),
        Some("engineering"),
        "a blank ownerDesk must default the same as an omitted one: {file:?}"
    );
}

/// **Regression, issue #1882 review — a desk-assigned card must default to
/// its own desk.** `runtime::assignee::AssigneeResolution::canonical` stores
/// a desk assignment as the desk's own canonical id, not a teammate id — so
/// `record.assignee` can BE `"engineering"` directly. Before the fix, the
/// defaulting fallback only checked desk MEMBERSHIP (`desk_of_member`), which
/// a desk id is never a member of, so a card already naming its owning desk
/// still produced an ownerless workflow.
#[tokio::test]
async fn applying_a_proposal_for_a_desk_assigned_card_defaults_to_that_desk() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let id = seed_proposal_card_assigned(&state, digest_ops(None), "engineering").await;

    let (status, card) = send(
        &state,
        "POST",
        &format!("/api/v1/company/tasks/{id}/workflow-proposal/apply"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{card}");

    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.clone());
    let record = store.load(&CompanyId::new("acme")).await.unwrap().unwrap();
    let overlay = record
        .overlay_workflows
        .iter()
        .find(|w| w.id == "weekly-digest")
        .expect("the created workflow is saved as an overlay");
    let file = crate::company::parse_workflow(&overlay.toml).expect("saved TOML parses");
    assert_eq!(
        file.owner_desk.as_deref(),
        Some("engineering"),
        "a card assigned straight to a desk must default owner_desk to that desk: {file:?}"
    );
}

/// **Regression, issue #1882 review — a multi-desk teammate must not default
/// an arbitrary owner.** `desk_of_member` returns the first desk in
/// `desk_ids` declaration order, which is fine for the informational message
/// it was written for (`unknown_desk_message`) but wrong for a value that
/// gets persisted: a proposal naming no `ownerDesk`, assigned to a teammate
/// who sits on two desks, has no basis for picking either one. Before the
/// fix, `apply_workflow_proposal`'s fallback used `desk_of_member` directly
/// and silently persisted `"engineering"` — the desk declared first in the
/// manifest — even though `ceo` sits on `legal` too. The fix must leave
/// `owner_desk` `None` rather than guess.
#[tokio::test]
async fn applying_a_proposal_for_a_multi_desk_assignee_leaves_owner_desk_unset() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[group_chat]]\nid = \"engineering\"\nname = \"Engineering\"\nmembers = [\"ceo\"]\n\
         [[group_chat]]\nid = \"legal\"\nname = \"Legal\"\nmembers = [\"ceo\"]\n\
         [policy]\nmode = \"full\"\n",
    )
    .unwrap();
    let state = state_with_manifest(&home, manifest).await;
    let id = seed_proposal_card(&state, digest_ops(None)).await;

    let (status, card) = send(
        &state,
        "POST",
        &format!("/api/v1/company/tasks/{id}/workflow-proposal/apply"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{card}");

    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.clone());
    let record = store.load(&CompanyId::new("acme")).await.unwrap().unwrap();
    let overlay = record
        .overlay_workflows
        .iter()
        .find(|w| w.id == "weekly-digest")
        .expect("the created workflow is saved as an overlay");
    let file = crate::company::parse_workflow(&overlay.toml).expect("saved TOML parses");
    assert_eq!(
        file.owner_desk, None,
        "a teammate on two desks gives no basis for picking either one: {file:?}"
    );
}

/// #276: applying a proposal whose trigger carries a schedule creates the
/// workflow **switched off** — armed only by a person, never by approving the
/// proposal.
#[tokio::test]
async fn applying_a_scheduled_proposal_lands_it_disarmed() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let id = seed_proposal_card(&state, digest_ops(Some("0 9 * * 1"))).await;

    let (status, card) = send(
        &state,
        "POST",
        &format!("/api/v1/company/tasks/{id}/workflow-proposal/apply"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{card}");
    assert_eq!(card["column"], "done");

    let (_status, workflows) = send(&state, "GET", "/api/v1/company/workflows", None).await;
    let created = workflows
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == "weekly-digest")
        .expect("the created workflow is listed");
    assert_eq!(
        created["enabled"], false,
        "a scheduled graph lands disarmed until a person arms it (#276)"
    );
}

/// Roster drift (the proposal names a teammate no longer on the roster) is
/// refused by the create's roster check: the card **stays In Review** with its
/// proposal intact, and the refusal is a 400 the operator sees.
#[tokio::test]
async fn a_proposal_that_fails_validation_keeps_the_card_in_review() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let ops = json!({
        "id": "weekly-digest",
        "name": "Weekly digest",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Start" },
            { "id": "write", "kind": "agent", "name": "Draft it", "agent": "ghost" }
        ],
        "edges": [{ "from": "start", "to": "write" }]
    });
    let id = seed_proposal_card(&state, ops).await;

    let (status, _body) = send(
        &state,
        "POST",
        &format!("/api/v1/company/tasks/{id}/workflow-proposal/apply"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The card is untouched save for the reason on its note: still In Review,
    // still carrying the proposal to retry once the roster is fixed.
    let (_status, card) = send(&state, "GET", &format!("/api/v1/company/tasks/{id}"), None).await;
    assert_eq!(card["task"]["stage"], "in_review");
    assert!(card["task"].get("workflowProposal").is_some(), "{card}");

    // …and no workflow was created.
    let (_status, workflows) = send(&state, "GET", "/api/v1/company/workflows", None).await;
    assert!(
        workflows
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["id"] != "weekly-digest"),
        "a refused proposal must not leave a workflow behind"
    );
}

/// A company with one desk, so its runtime deliverable set is exactly
/// `["engineering"]` — enough to tell a channel target that works from one that
/// does not (issue #1191).
fn desk_manifest() -> CompanyManifest {
    toml::from_str(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[group_chat]]\nid = \"engineering\"\nname = \"Engineering\"\nmembers = [\"ceo\"]\n\
         [policy]\nmode = \"full\"\n",
    )
    .unwrap()
}

/// [`digest_ops`], with the output node posting its report to `target`.
fn digest_ops_posting_to(target: &str) -> Value {
    json!({
        "id": "weekly-digest",
        "name": "Weekly digest",
        "description": "Post the weekly digest",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Start" },
            { "id": "write", "kind": "agent", "name": "Draft it", "agent": "ceo" },
            {
                "id": "post_summary",
                "kind": "output",
                "name": "Post to engineering desk",
                "destination": { "kind": "channel", "target": target }
            }
        ],
        "edges": [
            { "from": "start", "to": "write" },
            { "from": "write", "to": "post_summary" }
        ]
    })
}

/// **The #1191 regression.** The builder appended `-desk` to a desk's display
/// name, so the proposal routed its report to `engineering-desk` — not a channel
/// this runtime can deliver to.
///
/// Apply used to persist it: the operator was told "Workflow created — the card
/// is done", the card flipped to Done, and the workflow that now existed could
/// never deliver and could not be saved again from the editor without first
/// fixing a destination the operator never chose. Apply is a save, and it is now
/// held to the save rule — with the located `workflow_invalid` envelope, so the
/// console can say WHICH node.
#[tokio::test]
async fn applying_a_proposal_with_an_unwired_channel_is_refused_and_keeps_the_card_in_review() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let id = seed_proposal_card(&state, digest_ops_posting_to("engineering-desk")).await;

    let (status, body) = send(
        &state,
        "POST",
        &format!("/api/v1/company/tasks/{id}/workflow-proposal/apply"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "workflow_invalid", "{body}");
    let problem = body["problems"]
        .as_array()
        .unwrap_or_else(|| panic!("the refusal must carry a breakdown: {body}"))
        .iter()
        .find(|p| p["node_id"] == "post_summary")
        .unwrap_or_else(|| panic!("no problem names the output node: {body}"));
    assert_eq!(problem["field"], "destination.target", "{body}");
    assert!(
        problem["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is not a workflow delivery channel"),
        "{body}"
    );

    // The card is recoverable, exactly as it is for roster drift: still In
    // Review, still carrying its proposal, with the reason on its note.
    let (_status, card) = send(&state, "GET", &format!("/api/v1/company/tasks/{id}"), None).await;
    assert_eq!(card["task"]["stage"], "in_review", "{card}");
    assert!(card["task"].get("workflowProposal").is_some(), "{card}");
    assert!(
        card["task"]["note"]
            .as_str()
            .unwrap_or_default()
            .contains("still waiting for review"),
        "{card}"
    );

    // …and nothing was persisted.
    let (_status, workflows) = send(&state, "GET", "/api/v1/company/workflows", None).await;
    assert!(
        workflows
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["id"] != "weekly-digest"),
        "a refused apply must not leave a workflow behind: {workflows}"
    );
}

/// The invariant the defect broke, stated directly: whatever apply persists,
/// the ordinary editor save route accepts back unchanged.
///
/// Before #1191 these two routes gave opposite answers to the same bytes —
/// apply created the graph and `PUT` refused it — so the operator's first edit
/// of a copilot-built workflow was blocked on a destination they never chose.
#[tokio::test]
async fn an_applied_proposal_can_be_saved_again_unchanged() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let id = seed_proposal_card(&state, digest_ops_posting_to("engineering")).await;

    let (status, card) = send(
        &state,
        "POST",
        &format!("/api/v1/company/tasks/{id}/workflow-proposal/apply"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{card}");
    assert_eq!(card["column"], "done");

    // Read the created graph back and save it straight to the editor's route,
    // byte-for-byte, with its own version token.
    let (status, graph) = send(
        &state,
        "GET",
        "/api/v1/company/workflows/weekly-digest",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{graph}");

    let mut body = graph.clone();
    body["expectedVersion"] = graph["version"].clone();
    let (status, saved) = send(
        &state,
        "PUT",
        "/api/v1/company/workflows/weekly-digest",
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "what apply persists, the editor must accept back unchanged: {saved}"
    );
}

/// Rejecting a proposal returns the card to To-do and clears the proposal
/// (decision D2c). The card keeps its `workflow` deliverable, so it can be built
/// again.
#[tokio::test]
async fn rejecting_a_proposal_returns_the_card_to_todo() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let id = seed_proposal_card(&state, digest_ops(None)).await;

    let (status, card) = send(
        &state,
        "POST",
        &format!("/api/v1/company/tasks/{id}/workflow-proposal/reject"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{card}");
    assert_eq!(card["column"], "pending");
    assert!(card.get("workflowProposal").is_none(), "{card}");
    assert_eq!(
        card["deliverable"], "workflow",
        "reject keeps the deliverable"
    );
}

/// Applying or rejecting a card that has no proposal is a 400, not a silent
/// no-op — the operator asked for an action on something that is not there.
#[tokio::test]
async fn applying_with_no_proposal_is_a_bad_request() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let (_status, task) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({ "title": "Plain card" })),
    )
    .await;
    let id = task["id"].as_str().unwrap().to_string();

    for verb in ["apply", "reject"] {
        let (status, _body) = send(
            &state,
            "POST",
            &format!("/api/v1/company/tasks/{id}/workflow-proposal/{verb}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{verb} with no proposal");
    }
}

/// The create route accepts an explicit `deliverable`, and it round-trips on the
/// board read — the operator's once-vs-workflow choice (D2a), with `once` staying
/// off the wire so a plain card is byte-identical to a pre-#580 one.
#[tokio::test]
async fn a_card_can_be_created_as_a_workflow_deliverable() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, workflow_card) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({ "title": "Automate onboarding", "deliverable": "workflow" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(workflow_card["deliverable"], "workflow");

    let (_status, once_card) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({ "title": "One-off note" })),
    )
    .await;
    assert!(
        once_card.get("deliverable").is_none(),
        "a once card stays off the wire: {once_card}"
    );

    // A patch can flip a once card to workflow before it is dragged into In
    // Progress.
    let id = once_card["id"].as_str().unwrap();
    let (status, flipped) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/tasks/{id}"),
        Some(json!({ "deliverable": "workflow" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(flipped["deliverable"], "workflow");
}

// ---------------------------------------------------------------------------
// Binary workspace nodes over HTTP (issue #553)
// ---------------------------------------------------------------------------

/// Sends a `multipart/form-data` upload with one file part and an optional
/// `parentId`, hand-rolling the body so the test exercises the real
/// `Multipart` extractor rather than a stub.
async fn upload_file(
    state: &AppState,
    filename: &str,
    content_type: Option<&str>,
    bytes: &[u8],
    parent_id: Option<&str>,
) -> (StatusCode, Value) {
    const BOUNDARY: &str = "----opencompany553boundary";
    let mut body: Vec<u8> = Vec::new();
    if let Some(parent) = parent_id {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"parentId\"\r\n\r\n{parent}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    if let Some(ct) = content_type {
        body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
    }
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/company/workspace/upload")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let out = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if out.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&out).unwrap_or(Value::Null)
    };
    (status, value)
}

/// The headline of #553 over HTTP: a PNG uploads, appears in the tree with its
/// metadata, and streams back byte-exactly — the round trip that used to be
/// impossible because the create route only took a JSON body.
#[tokio::test]
async fn an_uploaded_image_round_trips_through_the_blob_route() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // Not valid UTF-8, so nothing on this path can be quietly routing it
    // through a `String`.
    let png: Vec<u8> = vec![
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe, 0x00,
    ];
    let (status, node) = upload_file(&state, "hero.png", Some("image/png"), &png, None).await;
    assert_eq!(status, StatusCode::OK, "{node}");
    assert_eq!(node["name"], "hero.png");
    assert_eq!(node["mime"], "image/png");
    assert_eq!(node["size"], png.len() as u64);
    let sha = node["sha256"]
        .as_str()
        .expect("a digest is returned")
        .to_string();
    assert_eq!(sha.len(), 64, "the store's digest, not the caller's");
    assert!(
        node["content"].is_null(),
        "a payload is never inlined into the node body"
    );
    let id = node["id"].as_str().unwrap().to_string();

    // It is in the tree, with its metadata, so the console can decide how to
    // render it without opening it.
    let (status, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed = tree
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == id.as_str())
        .expect("the uploaded node is in the tree");
    assert_eq!(listed["mime"], "image/png");
    assert_eq!(listed["size"], png.len() as u64);

    // The payload streams back exactly, with the headers a browser needs.
    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/company/workspace/blob/{id}"))
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.headers()["etag"], format!("\"{sha}\""));
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("hero.png")
    );
    let got = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(got.to_vec(), png, "the bytes must survive the round trip");
}

/// Issue #666: the filesystem backend derives payload paths from sibling
/// names, so accepting the same name twice used to leave two ids pointing at
/// one file. The second upload overwrote the first while the first node kept
/// its old length and digest.
///
/// Refusing the colliding create is the filesystem backend's honest answer: it
/// preserves the first payload and prevents the blob route from serving bytes
/// under metadata computed for a different file.
#[tokio::test]
async fn a_same_name_upload_is_refused_without_overwriting_the_first_blob() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let first_bytes = vec![0x89, b'P', b'N', b'G', 0xff];
    let (status, first) =
        upload_file(&state, "chart.png", Some("image/png"), &first_bytes, None).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let first_id = first["id"].as_str().unwrap().to_string();
    let first_sha = first["sha256"].as_str().unwrap().to_string();

    let second_bytes = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 0xff];
    let (status, refusal) =
        upload_file(&state, "chart.png", Some("image/png"), &second_bytes, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
    assert_eq!(refusal["code"], "conflict", "{refusal}");
    assert!(
        refusal["error"]
            .as_str()
            .is_some_and(|message| message.contains("chart.png")),
        "the operator can identify the occupied name: {refusal}"
    );

    let response = blob_response(&state, &first_id).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-length"],
        first_bytes.len().to_string(),
        "the surviving node still describes its own payload"
    );
    assert_eq!(response.headers()["etag"], format!("\"{first_sha}\""));
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        body.as_ref(),
        first_bytes.as_slice(),
        "the refused upload must not overwrite the first file"
    );

    let (status, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        tree.as_array()
            .unwrap()
            .iter()
            .filter(|node| node["name"] == "chart.png")
            .count(),
        1,
        "a rejected collision must not leave a second metadata row"
    );

    // The physical paths differ when the parent differs, so this is not a
    // workspace-wide filename ban.
    let (_, folder) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "Archive", "kind": "folder"})),
    )
    .await;
    let folder_id = folder["id"].as_str().unwrap();
    let (status, nested) = upload_file(
        &state,
        "chart.png",
        Some("image/png"),
        &second_bytes,
        Some(folder_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{nested}");
    assert_eq!(nested["parentId"], folder_id);
}

/// A Markdown upload stays a **note**, not a payload. Storing it as bytes would
/// silently cost it the editor, the diff-free text read, backlinks and search —
/// so the decision is asserted rather than left to whichever branch ran.
#[tokio::test]
async fn a_markdown_upload_is_stored_as_a_note_not_as_bytes() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, node) = upload_file(
        &state,
        "brief.md",
        Some("text/markdown"),
        b"# Launch\n\nLinks to [[voice]].",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{node}");
    assert!(
        node["mime"].is_null(),
        "a note carries no mime — that field is what marks a node binary"
    );
    let id = node["id"].as_str().unwrap().to_string();

    // …and it reads back through the *text* route, with backlinks.
    let (status, file) = send(
        &state,
        "GET",
        &format!("/api/v1/company/workspace/file/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(file["content"].as_str().unwrap().contains("# Launch"));
}

/// A file *typed* as text whose bytes are not UTF-8 becomes a payload. The
/// decision is made on the bytes, so a mislabelled upload cannot be mangled
/// into a note.
#[tokio::test]
async fn a_mislabelled_text_upload_is_stored_as_bytes() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, node) = upload_file(
        &state,
        "notes.txt",
        Some("text/plain"),
        &[0xff, 0xfe, 0x01],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{node}");
    assert_eq!(
        node["mime"], "text/plain",
        "it keeps the declared type, but is stored as a payload"
    );
    assert_eq!(node["size"], 3);
}

/// The text read is for prose and says so when handed a payload, rather than
/// answering with an empty body. The blob read is the download of whatever the
/// node holds, so a note downloads through it as its own bytes.
#[tokio::test]
async fn the_text_read_refuses_a_payload_and_the_blob_read_downloads_a_note() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (_, image) = upload_file(&state, "chart.png", Some("image/png"), &[0x89, 0xff], None).await;
    let image_id = image["id"].as_str().unwrap().to_string();
    let (_, note) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "voice.md", "kind": "file", "content": "# Voice"})),
    )
    .await;
    let note_id = note["id"].as_str().unwrap().to_string();

    // Text read of a payload: refused, and it names the route that works.
    let (status, body) = send(
        &state,
        "GET",
        &format!("/api/v1/company/workspace/file/{image_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("workspace/blob/"),
        "the refusal must point at the route that serves it: {body}"
    );

    // Blob read of a note: the note downloads, neutralised the same way every
    // payload this route serves is.
    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/company/workspace/blob/{note_id}"))
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );
    let got = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(String::from_utf8(got.to_vec()).unwrap(), "# Voice");
}

// ---------------------------------------------------------------------------
// The blob route never hands a browser a document (issue #667)
// ---------------------------------------------------------------------------

/// `GET …/workspace/blob/{id}` as a browser navigating to it would.
async fn blob_response(state: &AppState, id: &str) -> axum::response::Response {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/company/workspace/blob/{id}"))
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    router(state.clone()).oneshot(request).await.unwrap()
}

/// The header triple a response must show before it can be called inert:
/// a type that is not a document, a browser told not to second-guess it, and a
/// disposition that downloads rather than renders.
fn assert_not_executable(response: &axum::response::Response, context: &str) {
    let content_type = response.headers()["content-type"].to_str().unwrap();
    let disposition = response.headers()["content-disposition"].to_str().unwrap();
    let nosniff = response
        .headers()
        .get("x-content-type-options")
        .map(|v| v.to_str().unwrap().to_string());

    assert!(
        disposition.starts_with("attachment;"),
        "{context}: a browser must download this, not render it — got {disposition:?}"
    );
    assert_eq!(
        nosniff.as_deref(),
        Some("nosniff"),
        "{context}: without nosniff the type below is a suggestion"
    );
    for executable in [
        "text/html",
        "image/svg+xml",
        "application/xhtml+xml",
        "text/xml",
    ] {
        assert!(
            !content_type.starts_with(executable),
            "{context}: served as {content_type:?}, which a browser parses into a \
             document with a script context"
        );
    }
}

/// The vector in #667, end to end: a payload stored under a document media type
/// is not servable as a document.
///
/// The bytes carry a trailing `0xff` so the upload takes the **binary** branch —
/// a valid-UTF-8 `text/html` upload is stored as a prose note and never reaches
/// this route at all. That byte is not a contrivance to reach the branch: a
/// browser decoding these bytes substitutes U+FFFD for it and runs the script
/// exactly the same, so this is the real shape of the attack.
///
/// The assertion that matters is the one about the *stored* mime: it is still
/// `text/html` afterwards. The fix is on the read path precisely so that every
/// payload already sitting in a tree under a caller's chosen mime is covered,
/// which an upload-side sanitiser would not have been.
#[tokio::test]
async fn a_blob_stored_as_html_cannot_be_served_as_a_document() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let payload = b"<script>fetch('/api/v1/company/team')</script>\xff";
    let (status, node) = upload_file(&state, "payload.png", Some("text/html"), payload, None).await;
    assert_eq!(status, StatusCode::OK, "{node}");
    assert_eq!(
        node["mime"], "text/html",
        "the stored mime is untouched — the read path is what neutralises it"
    );
    let id = node["id"].as_str().unwrap().to_string();

    let response = blob_response(&state, &id).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream",
        "a type nobody vouched for is served as opaque bytes"
    );
    assert_not_executable(&response, "an html-typed payload");

    // Neutralised, not corrupted: an operator who downloads it still gets the
    // file they stored.
    let got = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(got.to_vec(), payload.to_vec());
}

/// SVG is why an `image/*` prefix rule would not have closed this.
///
/// It is an image the console previews and a document a browser executes, so the
/// two halves are answered separately: the type survives (an `<img>` will not
/// decode SVG without it, and inside an `<img>` the SVG spec's secure static
/// mode means no script runs), and the disposition becomes `attachment` so the
/// same bytes at the top of a tab are downloaded instead of rendered.
#[tokio::test]
async fn an_svg_keeps_its_type_for_the_console_but_is_never_rendered_as_a_document() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>fetch('/api/v1/company/team')</script></svg>"#;
    let (status, node) = upload_file(&state, "logo.svg", Some("image/svg+xml"), svg, None).await;
    assert_eq!(status, StatusCode::OK, "{node}");
    let id = node["id"].as_str().unwrap().to_string();

    let response = blob_response(&state, &id).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "image/svg+xml",
        "the console's <img> preview needs this exact type to decode the bytes"
    );
    let disposition = response.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        disposition.starts_with("attachment;"),
        "a top-level navigation must download an SVG, not render it: {disposition:?}"
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
}

/// An arbitrary caller-declared type — one nobody has ever vetted — is opaque.
/// This is the closed-list half of the fix: the default arm is the safe one, so
/// a media type invented after this was written is downloaded, not rendered.
#[tokio::test]
async fn an_unrecognised_stored_type_is_served_as_opaque_bytes() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    for (name, declared) in [
        ("doc.xhtml", "application/xhtml+xml"),
        ("sheet.xml", "text/xml"),
        ("archive.zip", "application/zip"),
    ] {
        let (status, node) =
            upload_file(&state, name, Some(declared), &[0x50, 0x4b, 0xff], None).await;
        assert_eq!(status, StatusCode::OK, "{node}");
        let id = node["id"].as_str().unwrap().to_string();
        let response = blob_response(&state, &id).await;
        assert_eq!(
            response.headers()["content-type"],
            "application/octet-stream",
            "{declared} is not on the inline list"
        );
        assert_not_executable(&response, declared);
    }
}

/// The behaviour #611 built, pinned so the fix above cannot quietly cost it: an
/// image still arrives with its own type and `inline`, which is what makes the
/// console's preview and a direct navigation both show the picture.
#[tokio::test]
async fn an_image_still_renders_inline_with_its_own_type() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, node) = upload_file(
        &state,
        "hero.png",
        Some("image/png"),
        &[0x89, b'P', b'N', b'G', 0xff],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{node}");
    let id = node["id"].as_str().unwrap().to_string();

    let response = blob_response(&state, &id).await;
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let disposition = response.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        disposition.starts_with("inline;"),
        "an image must still render in place: {disposition:?}"
    );
    assert!(disposition.contains("hero.png"));
}

/// An upload lands under the folder it names, like any other node.
#[tokio::test]
async fn an_upload_can_target_a_parent_folder() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (_, folder) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "Shots", "kind": "folder"})),
    )
    .await;
    let folder_id = folder["id"].as_str().unwrap().to_string();

    let (status, node) = upload_file(
        &state,
        "a.png",
        Some("image/png"),
        &[0x89, 0x50],
        Some(&folder_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{node}");
    assert_eq!(node["parentId"], folder_id.as_str());
}

// ---------------------------------------------------------------------------
// An over-cap upload says "too large", not "malformed" (issue #647)
// ---------------------------------------------------------------------------

/// The boundary the raw-body helpers below agree on.
const OVERSIZE_BOUNDARY: &str = "----opencompany647boundary";

/// Posts an already-built body at the upload route.
///
/// [`upload_file`] assembles its body into a `Vec`, which is the one thing these
/// tests cannot do: the smallest of them weighs 65 MiB and two of them have to
/// out-weigh a 256 MiB limit. Taking a `Body` lets the caller stream one — or
/// malform one on purpose.
async fn post_upload(state: &AppState, body: Body) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/company/workspace/upload")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={OVERSIZE_BOUNDARY}"),
        )
        .body(body)
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let out = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&out).unwrap_or(Value::Null);
    (status, value)
}

/// A body of `prefix` + `payload` zero bytes + `suffix`, streamed in 1 MiB
/// frames with a yield before each.
///
/// The framing is load-bearing, not tidiness. A contiguous body would make the
/// *test process* hold the whole payload before the server saw a byte of it,
/// and the multipart reader drains every frame that is ready in one poll — so
/// an always-ready stream is buffered whole however finely it was cut up. The
/// yield keeps the reader one frame ahead of the parser rather than a whole
/// body ahead, which is what lets a 257 MiB request that the handler *skips*
/// cost about a megabyte instead of 257 of them.
fn streamed_multipart(prefix: Vec<u8>, payload: usize, suffix: Vec<u8>) -> Body {
    const FRAME: usize = 1024 * 1024;
    let prefix = std::sync::Arc::new(prefix);
    let suffix = std::sync::Arc::new(suffix);
    let filler = bytes::Bytes::from(vec![0u8; FRAME]);
    let frames = payload.div_ceil(FRAME);

    let stream = futures::stream::unfold(0usize, move |step| {
        let prefix = prefix.clone();
        let suffix = suffix.clone();
        let filler = filler.clone();
        async move {
            tokio::task::yield_now().await;
            let frame = if step == 0 {
                bytes::Bytes::from(prefix.as_ref().clone())
            } else if step <= frames {
                filler.slice(..(payload - (step - 1) * FRAME).min(FRAME))
            } else if step == frames + 1 {
                bytes::Bytes::from(suffix.as_ref().clone())
            } else {
                return None;
            };
            Some((Ok::<_, std::io::Error>(frame), step + 1))
        }
    });
    Body::from_stream(stream)
}

/// The opening of a `file` part, up to (not including) its bytes.
fn file_part_prefix(filename: &str) -> Vec<u8> {
    format!(
        "--{OVERSIZE_BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes()
}

/// The node names currently in the workspace tree.
async fn tree_names(state: &AppState) -> Vec<String> {
    let (status, tree) = send(state, "GET", "/api/v1/company/workspace", None).await;
    assert_eq!(status, StatusCode::OK);
    provisioned_names(&tree)
}

/// The headline of #647: a file over the store's per-file cap is refused as
/// **too large**, with the sentence an operator can act on.
///
/// This failed before the fix, and not subtly — the route's `DefaultBodyLimit`
/// was the same 64 MiB as the cap, so it truncated the body first and the
/// truncation surfaced as a parse failure: `400 invalid request: unreadable
/// file part: Error parsing multipart/form-data request`. A correctly-formed
/// request, described as broken, for a reason the operator could not guess.
/// The store's refusal below existed the whole time and could never be reached
/// through this route.
#[tokio::test]
async fn a_file_over_the_per_file_cap_is_refused_as_too_large_not_as_malformed() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let before = tree_names(&state).await;

    // One megabyte over the 64 MiB default: enough to break the cap, nowhere
    // near the 256 MiB the route will now read.
    let oversize = 65 * 1024 * 1024;
    let (status, body) = post_upload(
        &state,
        streamed_multipart(
            file_part_prefix("hero.mov"),
            oversize,
            format!("\r\n--{OVERSIZE_BOUNDARY}--\r\n").into_bytes(),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["code"], "workspace_quota_exceeded", "{body}");
    let message = body["error"].as_str().expect("an error message");
    assert!(message.contains("hero.mov"), "names the file: {message}");
    assert!(message.contains("65.0 MiB"), "names its size: {message}");
    assert!(message.contains("64.0 MiB"), "names the limit: {message}");
    assert!(message.contains("Nothing was stored"), "{message}");

    // The two words the bug used to answer with. Asserting on their absence is
    // the regression guard: a future change that lets the body limit preempt
    // the store again would put them straight back.
    assert!(
        !message.contains("unreadable file part"),
        "the request was not unreadable: {message}"
    );
    assert!(
        !message.contains("Error parsing"),
        "nor was it malformed: {message}"
    );

    assert_eq!(tree_names(&state).await, before, "and nothing was stored");
}

/// A file part declared as a texty type, so the upload takes the **text**
/// branch rather than the binary one.
fn text_file_part_prefix(filename: &str) -> Vec<u8> {
    format!(
        "--{OVERSIZE_BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"{filename}\"\r\nContent-Type: text/csv\r\n\r\n"
    )
    .into_bytes()
}

/// Issue #665: an over-cap upload is refused even when its bytes are valid
/// UTF-8.
///
/// The store's quota decorator meters **binary payloads only**, and that is a
/// deliberate narrowing — `src/runtime/workspace_quota.rs` says so, on the
/// grounds that "a note is bounded by what a model will emit into a tool call".
/// That premise holds for every writer the decorator covers and is false for
/// this route, which is where arbitrary operator-supplied bytes enter the tree.
///
/// So a 65 MiB `.csv` — valid UTF-8, therefore classified as prose — used to be
/// stored with **no size check at all**, while the byte-identical payload under
/// a binary content type was refused. Same request, same size, opposite answer,
/// decided by whether the bytes happened to decode.
///
/// The narrowing itself is untouched: an agent's note is still unmetered, and
/// `tree_quota_gb` still counts binary payloads alone.
#[tokio::test]
async fn an_over_cap_upload_is_refused_even_when_its_bytes_are_valid_utf8() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let before = tree_names(&state).await;

    // NUL bytes: valid UTF-8, so `text_body` decodes them and the upload takes
    // the text branch. One megabyte over the 64 MiB default cap.
    let oversize = 65 * 1024 * 1024;
    let (status, body) = post_upload(
        &state,
        streamed_multipart(
            text_file_part_prefix("export.csv"),
            oversize,
            format!("\r\n--{OVERSIZE_BOUNDARY}--\r\n").into_bytes(),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["code"], "workspace_quota_exceeded", "{body}");
    let message = body["error"].as_str().expect("an error message");
    assert!(message.contains("export.csv"), "names the file: {message}");
    assert!(message.contains("65.0 MiB"), "names its size: {message}");
    assert!(message.contains("64.0 MiB"), "names the limit: {message}");
    assert!(message.contains("Nothing was stored"), "{message}");

    assert_eq!(tree_names(&state).await, before, "and nothing was stored");
}

/// The other half of #665, and the reason the fix is a cap rather than a
/// reclassification: an *under*-cap text upload is still stored as prose.
///
/// Refusing large text must not turn ordinary text uploads into opaque blobs — a
/// `.csv` an operator uploads is meant to stay searchable, backlinkable and
/// editable in the console. If this ever fails, the fix has started deciding
/// storage representation instead of bounding size.
#[tokio::test]
async fn an_under_cap_text_upload_is_still_stored_as_prose() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, node) = upload_file(
        &state,
        "notes.csv",
        Some("text/csv"),
        b"a,b,c\n1,2,3\n",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{node}");
    assert_eq!(
        node["content"], "a,b,c\n1,2,3\n",
        "a text upload keeps its body: {node}"
    );
    assert!(
        node.get("mime").is_none() || node["mime"].is_null(),
        "and is a prose note, not a binary payload: {node}"
    );
}

/// The route's own backstop is classified too, and it fires while *skipping* a
/// part — the reader can notice the limit anywhere it reads, not only where the
/// handler wants bytes.
///
/// Without this the classifier arm ships untested and a drift in axum's status
/// mapping would silently regress the answer to the old lying 400.
#[tokio::test]
async fn a_body_over_the_route_limit_is_classified_while_a_part_is_skipped() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let before = tree_names(&state).await;

    // A field the handler ignores by name, so it is drained rather than
    // buffered — and the drain runs past the 256 MiB the route will read.
    let prefix = format!(
        "--{OVERSIZE_BOUNDARY}\r\nContent-Disposition: form-data; name=\"ignored\"\r\n\r\n"
    )
    .into_bytes();
    let mut suffix = format!("\r\n--{OVERSIZE_BOUNDARY}\r\n").into_bytes();
    suffix.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.bin\"\r\n\r\nxx\r\n",
    );
    suffix.extend_from_slice(format!("--{OVERSIZE_BOUNDARY}--\r\n").as_bytes());

    let (status, body) = post_upload(
        &state,
        streamed_multipart(prefix, 257 * 1024 * 1024, suffix),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["code"], "workspace_quota_exceeded", "{body}");
    let message = body["error"].as_str().expect("an error message");
    assert!(
        message.contains("256.0 MiB"),
        "names the ceiling: {message}"
    );
    assert!(message.contains("Nothing was stored"), "{message}");
    // The size is deliberately absent: the body was cut off, so the true total
    // is not knowable here and a guess would be worse than silence.
    assert!(
        !message.contains("Error parsing"),
        "still not a parse failure: {message}"
    );

    assert_eq!(tree_names(&state).await, before, "and nothing was stored");
}

/// The same backstop, noticed at the other read site — while the handler is
/// pulling the `file` part's bytes rather than skipping past someone else's.
#[tokio::test]
async fn a_file_part_over_the_route_limit_is_classified_too() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let before = tree_names(&state).await;

    let (status, body) = post_upload(
        &state,
        streamed_multipart(
            file_part_prefix("enormous.bin"),
            257 * 1024 * 1024,
            format!("\r\n--{OVERSIZE_BOUNDARY}--\r\n").into_bytes(),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["code"], "workspace_quota_exceeded", "{body}");
    let message = body["error"].as_str().expect("an error message");
    assert!(
        message.contains("256.0 MiB"),
        "names the ceiling: {message}"
    );
    assert!(
        !message.contains("unreadable file part"),
        "the part was readable, just too long: {message}"
    );

    assert_eq!(tree_names(&state).await, before, "and nothing was stored");
}

/// The counter-test, and the half of the issue that is easiest to lose: a
/// genuinely malformed body still answers 400.
///
/// Classifying by size must not swallow the case the old message was right
/// about. These two shapes stay `invalid_request` — and stay distinguishable
/// from the 413s above, which is the whole point of the change.
#[tokio::test]
async fn a_malformed_multipart_body_is_still_a_400() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // The declared boundary never appears.
    let (status, body) =
        post_upload(&state, Body::from(b"this is not a multipart body".to_vec())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request", "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|m| m.contains("malformed multipart upload")),
        "{body}"
    );

    // A part that opens and never closes: headers, some bytes, no terminating
    // boundary. Truncated — the shape the body limit used to be mistaken for.
    let mut unterminated = file_part_prefix("half.bin");
    unterminated.extend_from_slice(b"partial bytes and then nothing");
    let (status, body) = post_upload(&state, Body::from(unterminated)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request", "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|m| m.contains("unreadable file part")),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// First-run company setup (docs/spec/runtime/company-setup.md)
// ---------------------------------------------------------------------------

/// The e-commerce worked example, end to end over the router.
///
/// The default test build has no harness, so this is the unpolished path — and
/// that is exactly the contract worth pinning: a company with no inference
/// credential still gets a real industry roster rather than an empty page or an
/// error. Decision D3's floor, asserted at the surface an operator meets.
#[tokio::test]
async fn setup_proposes_a_real_roster_with_no_model_wired() {
    let home = home();
    let state = state_with_company(home.path()).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/setup/roster",
        Some(json!({
            "industry": "E-commerce — I sell homeware online",
            "teamHint": "",
            "automate": "Social media posts, Meta ads, generating my reports, order dispatch",
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["template"], "ecommerce", "{body}");
    assert_eq!(
        body["source"], "fallback",
        "no harness is wired, so the curated team ships: {body}"
    );
    let agents = body["agents"].as_array().expect("agents array");
    assert!(
        (4..=6).contains(&agents.len()),
        "a proposal must be a workable team, got {}: {body}",
        agents.len()
    );
    let roles: Vec<&str> = agents
        .iter()
        .map(|a| a["role"].as_str().unwrap_or_default())
        .collect();
    assert!(roles.contains(&"Logistics Coordinator"), "{roles:?}");
    // Every row must be directly usable as a `POST …/team` body — the console
    // passes them straight through, so a missing field would surface as a
    // half-created teammate rather than as a validation error here.
    for agent in agents {
        for field in ["name", "role", "description"] {
            assert!(
                agent[field].as_str().is_some_and(|v| !v.trim().is_empty()),
                "agent is missing `{field}`: {agent}"
            );
        }
    }
}

/// Setup proposes; it does not create. The roster must be untouched afterwards,
/// because the console is what creates each teammate — and because the empty
/// roster is also the "has setup run?" signal (decision D4), a route that
/// created them itself would answer that question before the operator had seen
/// a single name.
#[tokio::test]
async fn setup_creates_no_teammates_of_its_own() {
    let home = home();
    let state = state_with_company(home.path()).await;

    let (before_status, before) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(before_status, StatusCode::OK);
    let before_len = before.as_array().expect("roster").len();

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/setup/roster",
        Some(json!({ "industry": "content creator", "automate": "daily posts" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, after) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(
        after.as_array().expect("roster").len(),
        before_len,
        "setup created teammates itself: {after}"
    );
}

/// The answers are persisted, because Phase 2 builds this company's workflows
/// from them and must not have to ask a second time.
#[tokio::test]
async fn setup_remembers_the_answers() {
    let home = home();
    let state = state_with_company(home.path()).await;

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/setup/roster",
        Some(json!({
            "industry": "E-commerce",
            "teamHint": "plus customer support",
            "automate": "Meta ads, order dispatch",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.path().to_path_buf());
    let record = store
        .load(&CompanyId::new("acme"))
        .await
        .expect("load")
        .expect("record");
    let answers = record.setup.expect("the answers were stored");
    assert_eq!(answers.industry, "E-commerce");
    assert_eq!(answers.team_hint, "plus customer support");
    assert_eq!(answers.automate, "Meta ads, order dispatch");
}

/// An operator who types nothing still gets a team. The three questions are
/// free text and the last two are skippable, so an empty body is a real request
/// rather than a client bug — and stranding someone on the setup screen is the
/// one outcome worse than a generic roster.
#[tokio::test]
async fn setup_answers_an_empty_body_with_the_generic_team() {
    let home = home();
    let state = state_with_company(home.path()).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/setup/roster",
        Some(json!({})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["template"], "generic", "{body}");
    assert!(
        body["agents"].as_array().expect("agents").len() >= 4,
        "{body}"
    );
}

/// `[workspace] max_blob_mb` above the default is a real knob again.
///
/// It never was one: the route stopped reading at 64 MiB whatever a company had
/// configured, so raising the cap bought nothing but a different way to fail.
/// A company at 128 MiB can now actually store a 65 MiB file.
#[tokio::test]
async fn a_company_that_raised_its_blob_cap_can_use_it() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_quota(
        &home,
        crate::runtime::WorkspaceQuota {
            max_blob_bytes: 128 * 1024 * 1024,
            tree_quota_bytes: None,
        },
    )
    .await;

    let size = 65 * 1024 * 1024;
    let (status, node) = post_upload(
        &state,
        streamed_multipart(
            file_part_prefix("raised.bin"),
            size,
            format!("\r\n--{OVERSIZE_BOUNDARY}--\r\n").into_bytes(),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{node}");
    assert_eq!(node["name"], "raised.bin");
    assert_eq!(node["size"], size as u64);
    assert!(tree_names(&state).await.contains(&"raised.bin".to_string()));
}

// ---------------------------------------------------------------------------
// Issue #705 — an irreversible effect's amount is admin-only
// ---------------------------------------------------------------------------

/// Reads a task's detail as a specific principal.
///
/// The harness signs every other request in as an admin, which is exactly why
/// this exists: a redaction verified only as an admin passes identically
/// against no redaction at all.
async fn detail_as(state: &AppState, id: &str, cookie: String) -> Value {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/company/tasks/{id}"))
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Issue #705: any Member could read the dollar value of every irreversible
/// effect on a card.
///
/// #618 restricted the money on an approval — the effect nobody has signed off
/// yet. The *executed* effect carries the same number through a different DTO
/// on a different route, and that route had no role check at all.
///
/// **Asserted on the serialized JSON, not on the struct.** `amount_usd` carries
/// `skip_serializing_if`, so the wire shape is the only thing that settles
/// whether the field shipped; a struct-level assertion can pass while the bytes
/// still carry the amount.
#[tokio::test]
async fn a_member_does_not_see_an_irreversible_effects_amount() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;

    let (status, task) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Pay the Q3 retainer"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{task}");
    let id = task["id"].as_str().unwrap().to_string();

    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    // Two effects: one carrying money, one not. The second is the control that
    // keeps "withheld" and "there was never an amount" distinguishable.
    runtime
        .journal
        .record_executed(
            "exec-705-paid",
            crate::runtime::journal::ExecutedEffect {
                kind: "payment.send".to_string(),
                amount_usd: Some(2400.0),
                task_id: Some(id.clone()),
                at_millis: 1_000,
                irreversible: true,
            },
        )
        .await
        .unwrap();
    runtime
        .journal
        .record_executed(
            "exec-705-free",
            crate::runtime::journal::ExecutedEffect {
                kind: "email.send".to_string(),
                amount_usd: None,
                task_id: Some(id.clone()),
                at_millis: 2_000,
                irreversible: true,
            },
        )
        .await
        .unwrap();

    // The admin signs these off, so the admin sees what they cost.
    let as_admin = detail_as(
        &state,
        &id,
        crate::server::test_support::fixed_cookie("acme"),
    )
    .await;
    let admin_effects = as_admin["irreversibleEffects"].as_array().unwrap();
    assert_eq!(admin_effects.len(), 2, "{as_admin}");
    let admin_paid = admin_effects
        .iter()
        .find(|e| e["kind"] == "payment.send")
        .unwrap();
    assert_eq!(admin_paid["amountUsd"].as_f64(), Some(2400.0), "{as_admin}");
    assert!(
        admin_paid.get("amountHidden").is_none(),
        "an admin is not told anything was withheld: {as_admin}"
    );

    let as_member = detail_as(
        &state,
        &id,
        crate::server::test_support::member_cookie("acme"),
    )
    .await;
    let member_effects = as_member["irreversibleEffects"].as_array().unwrap();
    assert_eq!(
        member_effects.len(),
        2,
        "the rows survive — a member must still see what a retry would re-do: {as_member}"
    );
    let member_paid = member_effects
        .iter()
        .find(|e| e["kind"] == "payment.send")
        .unwrap();

    // The leak, closed. Absent from the wire, not null: `skip_serializing_if`.
    assert!(
        member_paid.get("amountUsd").is_none(),
        "the amount must not reach a member: {as_member}"
    );
    // Hidden is not absent — the console has to be able to say why.
    assert_eq!(
        member_paid["amountHidden"], true,
        "a withheld amount must be distinguishable from an effect that cost \
         nothing: {as_member}"
    );
    // Everything that makes the retry warning legible survives.
    assert_eq!(member_paid["kind"], "payment.send");
    assert_eq!(member_paid["atMillis"].as_u64(), Some(1_000));

    // The control: an effect that never carried money is not reported as
    // redacted, or "nothing to show" and "not shown to you" collapse.
    let member_free = member_effects
        .iter()
        .find(|e| e["kind"] == "email.send")
        .unwrap();
    assert!(member_free.get("amountUsd").is_none(), "{as_member}");
    assert!(
        member_free.get("amountHidden").is_none(),
        "an effect with no amount was not redacted: {as_member}"
    );
}

/// The export path is covered by construction, because it is handed the same
/// value.
///
/// `assemble_detail` is deliberately shared between the JSON route and the
/// export document (issue #352 calls that sharing "the export's redaction
/// guarantee"). This drives that shared function directly with a
/// member-scoped principal, so the guarantee is asserted at the seam both
/// readers pass through rather than only at the JSON one.
///
/// **Why not assert on the exported HTML alone.** The export template does not
/// currently render effect amounts at all, so an HTML-only assertion would pass
/// whether or not the redaction exists — coverage that cannot fail. The HTML
/// check below is kept as a secondary guard against a future template that does
/// render them; the assertion that actually holds the line is the one on the
/// shared projection.
#[tokio::test]
async fn the_export_document_is_built_from_the_redacted_detail() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;

    let (status, task) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Pay the Q3 retainer"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{task}");
    let id = task["id"].as_str().unwrap().to_string();

    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    runtime
        .journal
        .record_executed(
            "exec-705-export",
            crate::runtime::journal::ExecutedEffect {
                kind: "payment.send".to_string(),
                amount_usd: Some(2400.0),
                task_id: Some(id.clone()),
                at_millis: 1_000,
                irreversible: true,
            },
        )
        .await
        .unwrap();

    // The seam both readers share, driven as a member. `assemble_detail` is
    // exactly what `export_task` calls.
    let as_member = super::tasks::assemble_detail(
        &super::ScopedCompany {
            runtime: runtime.clone(),
            actor: None,
            may_read_contents: false,
            is_admin: false,
        },
        &id,
    )
    .await
    .expect("detail assembles");
    // Serialized, not read off the struct: `skip_serializing_if` means the wire
    // shape is what settles whether the amount shipped.
    let wire = serde_json::to_value(&as_member.irreversible_effects).unwrap();
    let paid = &wire.as_array().unwrap()[0];
    assert!(
        paid.get("amountUsd").is_none(),
        "the shared projection the export renders must already be redacted: {wire}"
    );
    assert_eq!(paid["amountHidden"], true, "{wire}");

    // …and the same principal reading it as an admin still gets the number, so
    // the assertion above is redaction rather than the field being gone.
    let as_admin = super::tasks::assemble_detail(
        &super::ScopedCompany {
            runtime: runtime.clone(),
            actor: None,
            may_read_contents: true,
            is_admin: true,
        },
        &id,
    )
    .await
    .expect("detail assembles");
    let admin_wire = serde_json::to_value(&as_admin.irreversible_effects).unwrap();
    assert_eq!(
        admin_wire.as_array().unwrap()[0]["amountUsd"].as_f64(),
        Some(2400.0),
        "{admin_wire}"
    );

    // Secondary guard: the rendered document must not carry it either. This
    // passes today regardless (the template renders no effects) and exists so a
    // future template that does render them cannot reintroduce the leak.
    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/company/tasks/{id}/export"))
        .header("cookie", crate::server::test_support::member_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(bytes.to_vec()).expect("the export is utf-8");
    assert!(
        !html.contains("2400"),
        "the exported document must not carry an amount this reader may not read"
    );
}

// ── Issue #661 (M5): the console's two new reads ───────────────────────────

/// A run's board rows reach `GET …/workflows/runs`.
///
/// This is the surface PR3's console history panel consumes, and the only one a
/// **scheduled** run has: nobody awaited its response, so without this the sole
/// evidence a 3am run opened a card is the card itself, with nothing saying
/// which run put it there.
///
/// Asserted through the real route and the real group-by-run fold, because the
/// fold is where a row can be dropped — a `WorkflowRunFinished` that settles an
/// open entry writes every field across, and one missing line there is invisible
/// to a serialization test.
#[tokio::test]
async fn the_run_history_carries_a_runs_board_rows() {
    use crate::ports::types::CompanyEvent;
    use crate::ports::{WorkflowBoardAction, WorkflowRunBoardRow};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();

    for event in [
        CompanyEvent::WorkflowRunStarted {
            workflow_id: "digest".into(),
            run_id: "run-1".into(),
            scheduled: true,
            started_by: None,
            resume_semantic: None,
        },
        CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".into(),
            scheduled: true,
            run_id: Some("run-1".into()),
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: vec![WorkflowRunBoardRow {
                action: WorkflowBoardAction::Spawned,
                task_id: Some("card-1".into()),
                title: Some("Reply to the auditor".into()),
                assignee: None,
            }],
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        },
        // A second run that touched no card, so the omission is asserted on a
        // real row rather than on an absence that could be the fold failing.
        CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".into(),
            scheduled: false,
            run_id: Some("run-2".into()),
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        },
    ] {
        runtime.events().append(&company, event).await.unwrap();
    }

    let (status, runs) = send(&state, "GET", "/api/v1/company/workflows/runs", None).await;
    assert_eq!(status, StatusCode::OK);
    let runs = runs["runs"].as_array().expect("an array of runs");

    let settled = runs
        .iter()
        .find(|r| r["runId"] == "run-1")
        .unwrap_or_else(|| panic!("run-1 must be in the history: {runs:?}"));
    assert_eq!(settled["board"][0]["action"], "spawned");
    assert_eq!(settled["board"][0]["taskId"], "card-1");
    assert_eq!(settled["board"][0]["title"], "Reply to the auditor");

    let untouched = runs
        .iter()
        .find(|r| r["runId"] == "run-2")
        .unwrap_or_else(|| panic!("run-2 must be in the history: {runs:?}"));
    assert!(
        untouched["board"].is_null(),
        "a run that touched no card must omit the key entirely, so every existing history row's \
         wire shape is unchanged: {untouched}"
    );
}

/// A card opened by a run carries its provenance onto the board read, and a card
/// opened any other way is byte-unchanged.
///
/// The second half is the compatibility claim and needs its own card rather than
/// a re-read of the first: `skip_serializing_if` is what keeps every card the
/// board rendered before #661 identical, and only an actually-absent field
/// proves it.
#[tokio::test]
async fn a_card_opened_by_a_run_projects_its_provenance() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();

    let mut from_run = discussion_card("t-run", "Reply to the auditor");
    from_run.origin_run_id = Some("run-1".to_string());
    from_run.origin_workflow_id = Some("digest".to_string());
    runtime.tasks().upsert(&company, &from_run).await.unwrap();
    runtime
        .tasks()
        .upsert(&company, &discussion_card("t-hand", "Opened by hand"))
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    let cards = body.as_array().expect("an array of cards");

    let from_run = cards
        .iter()
        .find(|c| c["id"] == "t-run")
        .unwrap_or_else(|| panic!("the run's card must be on the board: {cards:?}"));
    assert_eq!(from_run["originRunId"], "run-1");
    assert_eq!(from_run["originWorkflowId"], "digest");

    let by_hand = cards
        .iter()
        .find(|c| c["id"] == "t-hand")
        .unwrap_or_else(|| panic!("the hand-opened card must be on the board: {cards:?}"));
    assert!(
        by_hand["originRunId"].is_null() && by_hand["originWorkflowId"].is_null(),
        "a card no run opened must carry neither key, so the board's existing wire shape is \
         unchanged: {by_hand}"
    );
}

/// Both addressing forms reach it, like every other write-plane route — the
/// platform `…/companies/{id}/…` spelling and the single-company alias.
#[tokio::test]
async fn setup_is_reachable_under_both_scope_forms() {
    let home = home();
    let state = state_with_company(home.path()).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/companies/acme/setup/roster",
        Some(json!({ "industry": "software" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["template"], "software", "{body}");
}

// ---------------------------------------------------------------------------
// Chat attachments (issue #1682)
// ---------------------------------------------------------------------------

/// Uploads one file to the chat-attachment route, hand-rolling the multipart
/// body so the test drives the real `Multipart` extractor rather than a stub —
/// the same shape as `upload_file`, pointed at `/chat/upload`.
async fn chat_upload(
    state: &AppState,
    filename: &str,
    content_type: Option<&str>,
    bytes: &[u8],
) -> (StatusCode, Value) {
    const BOUNDARY: &str = "----opencompany1682boundary";
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    if let Some(ct) = content_type {
        body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
    }
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/company/chat/upload")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let out = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if out.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&out).unwrap_or(Value::Null)
    };
    (status, value)
}

/// The upload half of #1682: a file posts to `/chat/upload`, comes back as a
/// compact `AttachmentRef` with the store's own metadata, and lands in the
/// workspace tree as a binary node the existing blob route can serve. This is
/// the reference the send path then carries by id.
#[tokio::test]
async fn chat_upload_stores_binary_and_returns_ref() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // Not valid UTF-8, so nothing on this path can be quietly routing it
    // through a `String` and turning the attachment into a prose note.
    let png: Vec<u8> = vec![
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe, 0x00,
    ];
    let (status, reference) = chat_upload(&state, "hero.png", Some("image/png"), &png).await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    assert_eq!(reference["name"], "hero.png");
    assert_eq!(reference["mime"], "image/png");
    assert_eq!(reference["size"], png.len() as u64);
    let node_id = reference["nodeId"].as_str().expect("a node id").to_string();

    // It is a real binary node in the tree, so it shares the workspace quota
    // and the hardened blob serve rather than a parallel store.
    let (status, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed = tree
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == node_id.as_str())
        .expect("the uploaded node is in the tree");
    assert_eq!(listed["mime"], "image/png");
    assert_eq!(listed["size"], png.len() as u64);

    // And it streams back byte-exactly through the existing blob route — the
    // download path #1682 reuses untouched.
    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/company/workspace/blob/{node_id}"))
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let got = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(got.to_vec(), png, "the bytes must survive the round trip");
}

/// A browser may send a full path as the filename; the route stores under the
/// last segment only, named by the workspace rule — the same sanitizer the
/// workspace upload applies, so no client string reaches a filesystem path.
#[tokio::test]
async fn chat_upload_sanitizes_pathy_filename() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let bytes: Vec<u8> = vec![0x00, 0x01, 0x02, 0xff];
    let (status, reference) = chat_upload(
        &state,
        "../../etc/Secret Report.bin",
        Some("application/octet-stream"),
        &bytes,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    let name = reference["name"].as_str().unwrap();
    assert!(
        !name.contains('/') && !name.contains('\\'),
        "the stored name kept a path separator: {name}"
    );
    assert_eq!(
        name, "secret-report.bin",
        "stored under the sanitized last segment"
    );
}

/// Codex review finding on #1682: chat uploads all land at the workspace
/// root, so a second message attaching a file under an earlier one's exact
/// name — the common case of picking `image.png` twice — used to 409 rather
/// than attach, since the only way to free the name was deleting the first
/// upload and breaking its download. The route now retries once under a
/// disambiguated name instead of failing the attach.
#[tokio::test]
async fn chat_upload_disambiguates_a_repeated_filename() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let first: Vec<u8> = vec![0x01, 0x02, 0x03];
    let (status, first_ref) = chat_upload(&state, "image.png", Some("image/png"), &first).await;
    assert_eq!(status, StatusCode::OK, "{first_ref}");
    assert_eq!(first_ref["name"], "image.png");

    // A later message attaches a *different* file under the same filename.
    let second: Vec<u8> = vec![0x09, 0x08, 0x07, 0x06];
    let (status, second_ref) = chat_upload(&state, "image.png", Some("image/png"), &second).await;
    assert_eq!(status, StatusCode::OK, "{second_ref}");
    let second_name = second_ref["name"].as_str().expect("a stored name");
    assert_ne!(
        second_name, "image.png",
        "the second upload must not silently fail or overwrite the first"
    );
    assert!(
        second_name.starts_with("image-") && second_name.ends_with(".png"),
        "expected a disambiguated image-*.png name, got {second_name}"
    );

    // Both node ids are distinct, live in the tree, and stream back their own
    // (not each other's) bytes — no data was lost or aliased on the collision.
    let first_id = first_ref["nodeId"].as_str().unwrap();
    let second_id = second_ref["nodeId"].as_str().unwrap();
    assert_ne!(first_id, second_id);
    for (node_id, want) in [(first_id, &first), (second_id, &second)] {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/company/workspace/blob/{node_id}"))
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let got = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&got.to_vec(), want);
    }
}

/// The headline of #1682 end-to-end: an operator attaches a file, the message
/// carries it, and a reload projects the attachment back with the **store's**
/// name / mime / size — never a client claim, because `/chat` was handed only
/// the node id. This is the reload proof the whole two-step design exists for.
#[tokio::test]
async fn chat_message_with_attachment_journals_and_hydrates() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x01];
    let (status, reference) = chat_upload(&state, "diagram.png", Some("image/png"), &png).await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    let node_id = reference["nodeId"].as_str().unwrap().to_string();

    // The send carries the id only — no name, mime or size the host could be
    // tricked into trusting.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "here is the diagram", "attachments": [node_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The reload: the operator's own message comes back with the attachment,
    // and every field is the store's.
    let (status, history) = send(&state, "GET", "/api/v1/company/chat/history", None).await;
    assert_eq!(status, StatusCode::OK);
    let mine = history
        .as_array()
        .expect("history is a list")
        .iter()
        .find(|m| m["text"] == "here is the diagram")
        .expect("the operator message survived the reload");
    let attachments = mine["attachments"]
        .as_array()
        .expect("the message carries its attachment on reload");
    assert_eq!(attachments.len(), 1, "exactly one attachment: {mine}");
    let attachment = &attachments[0];
    assert_eq!(attachment["nodeId"], node_id.as_str());
    assert_eq!(attachment["name"], "diagram.png", "name is the store's");
    assert_eq!(attachment["mime"], "image/png", "mime is the store's");
    assert_eq!(attachment["size"], png.len() as u64, "size is the store's");
}

/// Codex review finding on #1682, round 2: a bare node id told a hosted or
/// sidecar brain a file existed but gave it nothing to act on — no device
/// tool bridges a `context_*` call into the workspace's binary store. The
/// send route now extracts a readable attachment's text and journals it
/// alongside the reference, so `wire_event` (`brain::medulla::effects`) has
/// real content to put on the wire.
///
/// Reads the raw journal rather than `/chat/history` on purpose:
/// `extracted_text` is an internal server-to-brain channel, not operator-
/// facing data, so `ChatAttachmentDto` deliberately drops it — the console
/// never sees it and must not.
#[tokio::test]
async fn chat_attachment_text_is_extracted_and_journaled_for_the_brain() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let text = b"Q3 revenue grew 12% year over year.".to_vec();
    let (status, reference) = chat_upload(&state, "report.txt", Some("text/plain"), &text).await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    let node_id = reference["nodeId"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "summarize the attached report", "attachments": [node_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let journaled = runtime
        .events()
        .read_from(runtime.id(), crate::ports::types::EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .find_map(|s| match s.event {
            crate::ports::types::CompanyEvent::OperatorMessage { attachments, .. }
                if !attachments.is_empty() =>
            {
                Some(attachments)
            }
            _ => None,
        })
        .expect("the message with an attachment is in the journal");

    assert_eq!(journaled.len(), 1);
    assert_eq!(
        journaled[0].extracted_text.as_deref(),
        Some("Q3 revenue grew 12% year over year."),
        "a plain-text attachment's content must reach the durable event, \
         not just its node id"
    );
}

/// A binary attachment nothing here parses (an image) journals with no
/// extracted text — the reference alone rides the wire, and honestly: no
/// content is fabricated for a format extraction cannot read.
#[tokio::test]
async fn chat_attachment_with_no_readable_text_journals_none() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x01];
    let (status, reference) = chat_upload(&state, "photo.png", Some("image/png"), &png).await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    let node_id = reference["nodeId"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "what's in this photo?", "attachments": [node_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let journaled = runtime
        .events()
        .read_from(runtime.id(), crate::ports::types::EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .find_map(|s| match s.event {
            crate::ports::types::CompanyEvent::OperatorMessage { attachments, .. }
                if !attachments.is_empty() =>
            {
                Some(attachments)
            }
            _ => None,
        })
        .expect("the message with an attachment is in the journal");

    assert_eq!(journaled.len(), 1);
    assert_eq!(journaled[0].extracted_text, None);
}

/// The IDOR / phantom guard: a `node_id` that resolves to no binary node in
/// this company's workspace refuses the send with a `400`, on the same terms a
/// malformed thread `parent` does — so a stale or hostile client cannot attach
/// another company's file, or a file that does not exist.
#[tokio::test]
async fn chat_message_rejects_foreign_node() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "trust me", "attachments": ["01JZZZNOTAREALNODE00000000"] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And nothing was journaled: the refusal is before the append, so the
    // transcript does not hold a message pointing at a file this company lacks.
    let (status, history) = send(&state, "GET", "/api/v1/company/chat/history", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        history
            .as_array()
            .expect("history is a list")
            .iter()
            .all(|m| m["text"] != "trust me"),
        "a refused attachment message still reached the transcript: {history}"
    );
}
/// Codex review finding: an unbounded attachment list turns one `/chat` POST
/// into an attacker-controlled multiplier on `resolve_attachments`' tree scan
/// and extraction work. Refused with a `400` before any of that work runs.
#[tokio::test]
async fn chat_message_rejects_too_many_attachments() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let ids: Vec<String> = (0..21).map(|n| format!("node-{n}")).collect();
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "too many", "attachments": ids })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, history) = send(&state, "GET", "/api/v1/company/chat/history", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        history
            .as_array()
            .expect("history is a list")
            .iter()
            .all(|m| m["text"] != "too many"),
        "a refused attachment message still reached the transcript: {history}"
    );
}

/// Codex review finding: the same node id repeated in `attachments` used to
/// resolve — and extract — once per repetition. A message attaching the same
/// file three times over carries it exactly once.
#[tokio::test]
async fn chat_message_deduplicates_a_repeated_attachment_id() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let text = b"Q3 revenue grew 12%.".to_vec();
    let (status, reference) = chat_upload(&state, "report.txt", Some("text/plain"), &text).await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    let node_id = reference["nodeId"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({
            "message": "attached three times",
            "attachments": [node_id.clone(), node_id.clone(), node_id],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, history) = send(&state, "GET", "/api/v1/company/chat/history", None).await;
    assert_eq!(status, StatusCode::OK);
    let mine = history
        .as_array()
        .expect("history is a list")
        .iter()
        .find(|m| m["text"] == "attached three times")
        .expect("the message survived");
    assert_eq!(
        mine["attachments"].as_array().map(Vec::len),
        Some(1),
        "a repeated id must resolve to one attachment, not three: {mine}"
    );
}

// ---------------------------------------------------------------------------
// Prose attachments (issue #2029)
// ---------------------------------------------------------------------------

/// A markdown file uploaded through the console's own workspace upload — the
/// route that stores a UTF-8 `text/*` payload as a prose note — attaches to a
/// chat message and hydrates with the store's own name, mime and size.
///
/// The issue's case (a) verbatim: the upload endpoint accepted the file and
/// returned its id, and the send route refused that id as "not a file in this
/// company's workspace".
#[tokio::test]
async fn chat_message_attaches_a_note_uploaded_to_the_workspace() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let markdown = b"# Q3 notes\n\nRevenue grew 12% year over year.\n".to_vec();
    let (status, uploaded) = upload_file(
        &state,
        "q3-notes.md",
        Some("text/markdown"),
        &markdown,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    assert!(
        uploaded["mime"].is_null(),
        "the upload route stores a UTF-8 text payload as a prose note: {uploaded}"
    );
    let node_id = uploaded["id"].as_str().expect("a node id").to_string();

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "notes attached", "attachments": [node_id.clone()] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, history) = send(&state, "GET", "/api/v1/company/chat/history", None).await;
    assert_eq!(status, StatusCode::OK);
    let mine = history
        .as_array()
        .expect("history is a list")
        .iter()
        .find(|m| m["text"] == "notes attached")
        .expect("the message survived");
    let attachments = mine["attachments"]
        .as_array()
        .expect("the message carries its attachment on reload");
    assert_eq!(attachments.len(), 1, "exactly one attachment: {mine}");
    assert_eq!(attachments[0]["nodeId"], node_id.as_str());
    assert_eq!(attachments[0]["name"], "q3-notes.md", "name is the store's");
    assert_eq!(
        attachments[0]["mime"], "text/markdown",
        "a prose note's mime is guessed from the store's own name"
    );
    assert_eq!(
        attachments[0]["size"],
        markdown.len() as u64,
        "size is the note's content length"
    );
}

/// The issue's case (b): a note already sitting in the company workspace —
/// seeded, agent-authored or created from the console — attaches on the same
/// terms. Nothing about a chat attachment requires the file to have arrived
/// through an upload.
#[tokio::test]
async fn chat_message_attaches_a_note_already_in_the_workspace() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({ "name": "roadmap.md", "kind": "file", "content": "Ship the thing." })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let node_id = created["id"].as_str().expect("a node id").to_string();

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "roadmap attached", "attachments": [node_id.clone()] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, history) = send(&state, "GET", "/api/v1/company/chat/history", None).await;
    assert_eq!(status, StatusCode::OK);
    let mine = history
        .as_array()
        .expect("history is a list")
        .iter()
        .find(|m| m["text"] == "roadmap attached")
        .expect("the message survived");
    let attachments = mine["attachments"].as_array().expect("attachments");
    assert_eq!(attachments.len(), 1, "{mine}");
    assert_eq!(attachments[0]["nodeId"], node_id.as_str());
    assert_eq!(attachments[0]["name"], "roadmap.md");
    assert_eq!(attachments[0]["size"], "Ship the thing.".len() as u64);
}

/// A prose attachment's own words reach the durable event, the same as an
/// uploaded text blob's do — otherwise the brain is handed a filename and
/// nothing to read.
#[tokio::test]
async fn chat_attachment_note_text_is_extracted_and_journaled() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({
            "name": "brief.md",
            "kind": "file",
            "content": "Q3 revenue grew 12% year over year.",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let node_id = created["id"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "summarize the brief", "attachments": [node_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let journaled = runtime
        .events()
        .read_from(runtime.id(), crate::ports::types::EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .find_map(|s| match s.event {
            crate::ports::types::CompanyEvent::OperatorMessage { attachments, .. }
                if !attachments.is_empty() =>
            {
                Some(attachments)
            }
            _ => None,
        })
        .expect("the message with an attachment is in the journal");

    assert_eq!(journaled.len(), 1);
    assert_eq!(
        journaled[0].extracted_text.as_deref(),
        Some("Q3 revenue grew 12% year over year."),
        "a note's content must reach the durable event, not just its node id"
    );
}

/// A folder is still refused — and now the refusal says so, rather than
/// claiming a node the operator is looking at is absent from the workspace.
#[tokio::test]
async fn chat_message_rejects_a_folder_attachment() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, folder) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({ "name": "designs", "kind": "folder" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{folder}");
    let node_id = folder["id"].as_str().unwrap().to_string();

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "folder attached", "attachments": [node_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("folder"),
        "the refusal must name the real reason, got: {error}"
    );

    let (status, history) = send(&state, "GET", "/api/v1/company/chat/history", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        history
            .as_array()
            .expect("history is a list")
            .iter()
            .all(|m| m["text"] != "folder attached"),
        "a refused attachment message still reached the transcript: {history}"
    );
}

/// The download half: the blob route serves a prose note's bytes exactly, as a
/// neutralised download — never inline, never under a type a caller chose — so
/// the chip an attached note renders has a working download behind it. A
/// folder and an unknown id still 404 identically.
#[tokio::test]
async fn workspace_blob_serves_a_prose_note_as_a_download() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let content = "# Plan\n\nShip it.\n";
    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({ "name": "plan.md", "kind": "file", "content": content })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let node_id = created["id"].as_str().unwrap().to_string();

    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/company/workspace/blob/{node_id}"))
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers["content-type"], "application/octet-stream",
        "a note is served under a neutral type, not one a caller influenced"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert!(
        headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;"),
        "a note must never be served inline on the console's origin"
    );
    assert_eq!(headers["content-length"], content.len().to_string());
    assert!(
        !headers.contains_key("etag"),
        "a prose note has no stored digest to answer a conditional request with"
    );
    let got = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        String::from_utf8(got.to_vec()).unwrap(),
        content,
        "the note's bytes must survive the round trip"
    );

    // A folder and an id naming nothing still 404 identically.
    let (status, folder) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({ "name": "archive", "kind": "folder" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{folder}");
    for id in [folder["id"].as_str().unwrap(), "01JZZZNOTAREALNODE00000000"] {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/company/workspace/blob/{id}"))
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{id} must not be servable as a blob"
        );
    }
}

/// A note past the extraction cap still **attaches** — the reference is what
/// the operator asked for — it simply carries no extracted text, the same
/// answer an oversized binary gets.
#[tokio::test]
async fn chat_attachment_oversized_note_attaches_without_extracted_text() {
    use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).expect("company");

    // Written straight to the store: the JSON create route's body limit is far
    // below the extraction cap, so this size cannot arrive through it.
    let huge = "x".repeat(5 * 1024 * 1024);
    WorkspaceStore::create(
        runtime.workspace().as_ref(),
        &company,
        &WorkspaceNode {
            id: "node-oversized-note".to_string(),
            name: "huge.md".to_string(),
            kind: NodeKind::File,
            parent_id: None,
            updated_at_millis: 1,
            created_by: WorkspaceOrigin::Operator,
            updated_by: WorkspaceOrigin::Operator,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        },
        Some(&huge),
    )
    .await
    .expect("seed the oversized note");

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "big note", "attachments": ["node-oversized-note"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let journaled = runtime
        .events()
        .read_from(runtime.id(), crate::ports::types::EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .find_map(|s| match s.event {
            crate::ports::types::CompanyEvent::OperatorMessage { attachments, .. }
                if !attachments.is_empty() =>
            {
                Some(attachments)
            }
            _ => None,
        })
        .expect("the message with an attachment is in the journal");
    assert_eq!(journaled.len(), 1);
    assert_eq!(journaled[0].size, huge.len() as u64);
    assert_eq!(
        journaled[0].extracted_text, None,
        "a note past the extraction cap attaches with no text, rather than not at all"
    );
}

/// A workspace that records how many bytes each unbounded [`WorkspaceStore::read`]
/// handed back, per node.
///
/// Wraps the permissive in-memory double and sits **under** the runtime's own
/// decorators, so what it records is what the request actually pulled through
/// the whole production stack — including whether a decorator forwarded
/// `read_capped` or let it fall back to reading.
struct RecordingReads {
    inner: std::sync::Arc<dyn crate::ports::workspace::WorkspaceStore>,
    read_bytes: std::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl RecordingReads {
    fn new(inner: std::sync::Arc<dyn crate::ports::workspace::WorkspaceStore>) -> Self {
        Self {
            inner,
            read_bytes: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn bytes_read(&self, id: &str) -> u64 {
        self.read_bytes
            .lock()
            .unwrap()
            .get(id)
            .copied()
            .unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl crate::ports::workspace::WorkspaceStore for RecordingReads {
    async fn admit_upload(&self, company: &CompanyId, name: &str, len: u64) -> crate::Result<()> {
        self.inner.admit_upload(company, name, len).await
    }

    async fn tree(
        &self,
        company: &CompanyId,
    ) -> crate::Result<Vec<crate::ports::workspace::WorkspaceNode>> {
        self.inner.tree(company).await
    }

    async fn read(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> crate::Result<Option<(crate::ports::workspace::WorkspaceNode, String)>> {
        let got = self.inner.read(company, id).await?;
        if let Some((_, body)) = &got {
            *self
                .read_bytes
                .lock()
                .unwrap()
                .entry(id.to_string())
                .or_default() += body.len() as u64;
        }
        Ok(got)
    }

    async fn read_capped(
        &self,
        company: &CompanyId,
        id: &str,
        max_bytes: u64,
    ) -> crate::Result<Option<(crate::ports::workspace::WorkspaceNode, String, u64)>> {
        self.inner.read_capped(company, id, max_bytes).await
    }

    async fn write(
        &self,
        company: &CompanyId,
        id: &str,
        content: &str,
        author: crate::ports::workspace::WorkspaceOrigin,
    ) -> crate::Result<crate::ports::workspace::WorkspaceNode> {
        self.inner.write(company, id, content, author).await
    }

    async fn create(
        &self,
        company: &CompanyId,
        node: &crate::ports::workspace::WorkspaceNode,
        content: Option<&str>,
    ) -> crate::Result<()> {
        self.inner.create(company, node, content).await
    }

    async fn adopt_or_create_folder(
        &self,
        company: &CompanyId,
        parent: Option<&str>,
        name: &str,
        origin: crate::ports::workspace::WorkspaceOrigin,
    ) -> crate::Result<crate::ports::workspace::FolderClaim> {
        self.inner
            .adopt_or_create_folder(company, parent, name, origin)
            .await
    }

    async fn create_binary(
        &self,
        company: &CompanyId,
        node: &crate::ports::workspace::WorkspaceNode,
        bytes: &[u8],
    ) -> crate::Result<crate::ports::workspace::WorkspaceNode> {
        self.inner.create_binary(company, node, bytes).await
    }

    async fn write_binary(
        &self,
        company: &CompanyId,
        id: &str,
        bytes: &[u8],
        mime: Option<&str>,
        author: crate::ports::workspace::WorkspaceOrigin,
    ) -> crate::Result<crate::ports::workspace::WorkspaceNode> {
        self.inner
            .write_binary(company, id, bytes, mime, author)
            .await
    }

    async fn read_bytes(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> crate::Result<
        Option<(
            crate::ports::workspace::WorkspaceNode,
            crate::ports::workspace::BlobStream,
        )>,
    > {
        self.inner.read_bytes(company, id).await
    }

    async fn rename_move(
        &self,
        company: &CompanyId,
        id: &str,
        name: Option<&str>,
        parent: Option<Option<&str>>,
    ) -> crate::Result<crate::ports::workspace::WorkspaceNode> {
        self.inner.rename_move(company, id, name, parent).await
    }

    async fn swap_files(
        &self,
        company: &CompanyId,
        expected_id: Option<&str>,
        replacement_id: &str,
        name: &str,
    ) -> crate::Result<Option<crate::ports::workspace::WorkspaceNode>> {
        self.inner
            .swap_files(company, expected_id, replacement_id, name)
            .await
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> crate::Result<bool> {
        self.inner.delete(company, id).await
    }

    async fn is_empty(&self, company: &CompanyId) -> crate::Result<bool> {
        self.inner.is_empty(company).await
    }
}

/// The ceiling on a prose attachment holds at read time, not after.
///
/// The binary half of this path has never had to buffer what it will discard:
/// `size` rides the node, so an over-cap payload is refused on metadata and
/// `read_bytes` is never called. A note carries no `size`, so the same
/// discipline needs the store to answer the length and withhold the body in one
/// step — otherwise the cap is applied to a `String` that has already been
/// allocated, which is the allocation the cap exists to prevent, and one
/// message may carry twenty of them.
///
/// Recorded through the whole runtime stack, so a decorator that stopped
/// forwarding `read_capped` fails here too.
#[tokio::test]
async fn an_over_cap_note_attachment_is_never_fully_read() {
    use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let recorder = std::sync::Arc::new(RecordingReads::new(std::sync::Arc::new(
        crate::company::workspace_repair::loose_store::LooseWorkspace::default(),
    )));
    let state = state_with_workspace(&home, recorder.clone()).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).expect("company");

    let huge = "x".repeat(5 * 1024 * 1024);
    WorkspaceStore::create(
        runtime.workspace().as_ref(),
        &company,
        &WorkspaceNode {
            id: "node-oversized".to_string(),
            name: "huge.md".to_string(),
            kind: NodeKind::File,
            parent_id: None,
            updated_at_millis: 1,
            created_by: WorkspaceOrigin::Operator,
            updated_by: WorkspaceOrigin::Operator,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        },
        Some(&huge),
    )
    .await
    .expect("seed the oversized note");
    let seeded = recorder.bytes_read("node-oversized");

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "big note", "attachments": ["node-oversized"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(
        recorder.bytes_read("node-oversized") - seeded,
        0,
        "resolving an over-cap attachment must not pull the note's body through \
         the unbounded read"
    );

    // And it still attaches, with the length the store measured and no text.
    let journaled = runtime
        .events()
        .read_from(runtime.id(), crate::ports::types::EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .find_map(|s| match s.event {
            crate::ports::types::CompanyEvent::OperatorMessage { attachments, .. }
                if !attachments.is_empty() =>
            {
                Some(attachments)
            }
            _ => None,
        })
        .expect("the message with an attachment is in the journal");
    assert_eq!(journaled.len(), 1);
    assert_eq!(journaled[0].size, huge.len() as u64);
    assert_eq!(journaled[0].extracted_text, None);
}

/// A note under the cap is read once and reaches the brain whole — the bound
/// above must not be paid for by an attachment that fits.
#[tokio::test]
async fn an_under_cap_note_attachment_still_carries_its_text() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let recorder = std::sync::Arc::new(RecordingReads::new(std::sync::Arc::new(
        crate::company::workspace_repair::loose_store::LooseWorkspace::default(),
    )));
    let state = state_with_workspace(&home, recorder.clone()).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).expect("company");

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({
            "name": "brief.md",
            "kind": "file",
            "content": "Q3 revenue grew 12% year over year.",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let node_id = created["id"].as_str().unwrap().to_string();

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "small note", "attachments": [node_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let journaled = runtime
        .events()
        .read_from(runtime.id(), crate::ports::types::EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .find_map(|s| match s.event {
            crate::ports::types::CompanyEvent::OperatorMessage { attachments, .. }
                if !attachments.is_empty() =>
            {
                Some(attachments)
            }
            _ => None,
        })
        .expect("the message with an attachment is in the journal");
    assert_eq!(
        journaled[0].extracted_text.as_deref(),
        Some("Q3 revenue grew 12% year over year."),
    );
    assert_eq!(
        journaled[0].size,
        "Q3 revenue grew 12% year over year.".len() as u64
    );
}
