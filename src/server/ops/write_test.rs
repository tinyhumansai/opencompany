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
use crate::ports::tasks::TaskRecord;
use crate::ports::types::{CompanyId, CompanyRecord, ContextChunk};
use crate::runtime::RuntimeBuilder;
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

async fn state_with_company(home: &std::path::Path) -> AppState {
    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            template_provenance: None,
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
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
    // Issue #206/#301: manual entry lands in To-do, the board's one intake lane.
    assert_eq!(task["column"], "todo");
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
        body.to_string().contains("in_progress"),
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
    assert_eq!(board.as_array().expect("board")[0]["column"], "paused");
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
    assert_eq!(defaulted["column"], crate::ports::tasks::COLUMN_TODO);

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
    assert_eq!(explicit["column"], crate::ports::tasks::COLUMN_PLANNING);

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
        refused.to_string().contains("todo"),
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
    assert_ne!(
        created["column"], COLUMN_IN_PROGRESS,
        "a chat-created card must never arrive already dispatched"
    );
    assert_eq!(
        created["column"], COLUMN_TODO,
        "it lands in the board's intake lane, where the human drag is the gate"
    );

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
        assert_ne!(created["column"], COLUMN_IN_PROGRESS, "{created}");
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
                title: "Idle".into(),
                note: None,
                column: crate::ports::tasks::COLUMN_TODO.into(),
                priority: "medium".into(),
                assignee: String::new(),
                updated_at_millis: 1,
                origin_chat_id: None,
                parent_task_id: None,
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
    let rows = rows.as_array().unwrap();
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
    let pref = pref.as_array().unwrap();
    assert_eq!(pref.len(), 1);
    assert_eq!(pref[0]["id"], "f-mid");

    // `?query=` is a case-insensitive substring over title + body.
    let (status, hit) = send(&state, "GET", "/api/v1/company/memory?query=priya", None).await;
    assert_eq!(status, StatusCode::OK);
    let hit = hit.as_array().unwrap();
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0]["id"], "f-new");

    // Stats over the seeded facts: 3 facts, freshest timestamp, no agent chunks
    // yet (seeding bypassed the mirror), 0 task outcomes.
    let (status, stats) = send(&state, "GET", "/api/v1/company/memory/stats", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["facts"], 3);
    assert_eq!(stats["factsUpdatedAtMillis"], 3_000);
    assert_eq!(stats["agentChunks"], 0);
    assert_eq!(stats["taskOutcomes"], 0);
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

    // Stats now count that mirror as an agent chunk (not a task outcome).
    let (status, stats) = send(&state, "GET", "/api/v1/company/memory/stats", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["facts"], 4);
    assert_eq!(stats["agentChunks"], 1);
    assert_eq!(stats["taskOutcomes"], 0);
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
    assert_eq!(stats["agentChunks"], 0);
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
    assert_eq!(stats["agentChunks"], 2);
    assert_eq!(stats["taskOutcomes"], 1);
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

    // The list surfaces the same stamps per row, so a context card no longer
    // renders "—" while the header claims recent activity.
    let (status, rows) = send(&state, "GET", "/api/v1/company/memory", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = rows.as_array().unwrap();
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
        PlatformAuthConfig, PlatformClaims, StaticPlatformVerifier,
    };
    use std::collections::HashSet;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let verifier = std::sync::Arc::new(StaticPlatformVerifier::new("plat-secret"));
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
        StaticPlatformVerifier::tenant_token(&PlatformClaims {
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
    assert_eq!(list_b.as_array().unwrap().len(), 0);

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
    assert_eq!(list_a.as_array().unwrap().len(), 1);

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

    // A workspace with nothing seeded into it reads as an empty tree, not a 404
    // and not a fixture.
    let (status, tree) = send(&state, "GET", "/api/v1/company/workspace", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tree.as_array().unwrap().len(), 0);

    let (_, folder) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "Standards", "kind": "folder"})),
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
    assert_eq!(tree.len(), 3);
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
        PlatformAuthConfig, PlatformClaims, StaticPlatformVerifier,
    };
    use std::collections::HashSet;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let verifier = std::sync::Arc::new(StaticPlatformVerifier::new("plat-secret"));
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
        StaticPlatformVerifier::tenant_token(&PlatformClaims {
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

    // B's own workspace is empty — A's note is not in it.
    let (status, tree_b) = send_auth(
        &state,
        "GET",
        "/api/v1/companies/b/workspace",
        None,
        Some(&token("tenant:b")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tree_b.as_array().unwrap().len(), 0);

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
async fn team_overlay_add_delete_and_manifest_delete_conflict() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // The manifest teammate shows up on the read side before any overlay add,
    // named `null` (the console falls back to the role).
    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK);
    let roster = roster.as_array().unwrap();
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["id"], "ceo");
    assert_eq!(roster[0]["role"], "Chief");
    assert!(roster[0]["name"].is_null());

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
    assert_eq!(roster.len(), 2);
    let dana = roster.iter().find(|m| m["id"] == id).unwrap();
    assert_eq!(dana["name"], "Dana");
    assert_eq!(dana["role"], "Designer");

    // Deleting a manifest teammate is a 409.
    let (status, body) = send(&state, "DELETE", "/api/v1/company/team/ceo", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "conflict");

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
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["id"], "ceo");

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
        PlatformAuthConfig, PlatformClaims, StaticPlatformVerifier,
    };
    use std::collections::HashSet;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // Platform mode: `acme` is owned by `tenant:acme`.
    let verifier = std::sync::Arc::new(StaticPlatformVerifier::new("plat-secret"));
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
        StaticPlatformVerifier::tenant_token(&PlatformClaims {
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
    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            template_provenance: None,
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
    assert!(added["note"].as_str().unwrap().contains("rebuild"));

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
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            template_provenance: None,
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
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "greet");

    // `GET …/workflows/{wid}` round-trips the full graph too.
    let (status, graph) = send(&state, "GET", "/api/v1/company/workflows/greet", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(graph["name"], "greet");
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
    assert_eq!(body["code"], "invalid_request");

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

// -- Telegram channel (issue #31) -------------------------------------------

use crate::company::telegram::RecordingTelegramApi;

/// A running "acme" company whose host has a recording Telegram transport
/// injected, so the inbound webhook can actually deliver a reply offline. The
/// host is loopback-only (no `public_url`) — the local/self-host shape of issue
/// #203.
async fn state_with_telegram(home: &std::path::Path, api: RecordingTelegramApi) -> AppState {
    state_with_telegram_at(home, api, None).await
}

/// As [`state_with_telegram`], but with `public_url` set — the hosted shape,
/// where Telegram can actually deliver to the `/hooks/...` route.
async fn state_with_telegram_at(
    home: &std::path::Path,
    api: RecordingTelegramApi,
    public_url: Option<&str>,
) -> AppState {
    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            template_provenance: None,
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let connections =
        crate::server::ops::ConnectionsRuntime::new().with_telegram(std::sync::Arc::new(api));
    let state = AppState::new(AppConfig {
        public_url: public_url.map(str::to_string),
        ..AppConfig::default()
    })
    .with_connections(connections);
    state.registry().insert(id, std::sync::Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

/// Posts a raw Telegram update to the inbound webhook (no session; the secret
/// header is the only credential), returning the status and JSON body.
async fn telegram_hook(
    state: &AppState,
    secret_header: Option<&str>,
    body: Value,
) -> (StatusCode, Value, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/hooks/acme/telegram")
        .header("content-type", "application/json");
    if let Some(secret) = secret_header {
        request = request.header("x-telegram-bot-api-secret-token", secret);
    }
    let request = request.body(Body::from(body.to_string())).unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let raw = String::from_utf8_lossy(&bytes).to_string();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value, raw)
}

const BOT_TOKEN: &str = "7654321:AAExampleBotTokenNeverLeaks";
const WEBHOOK_SECRET: &str = "wh-secret-abc123";

fn telegram_update(chat_id: i64, text: &str) -> Value {
    json!({
        "update_id": 1,
        "message": {
            "message_id": 7,
            "from": { "id": 999, "username": "bob" },
            "chat": { "id": chat_id, "type": "private" },
            "text": text,
        }
    })
}

#[tokio::test]
async fn telegram_config_is_write_only_and_status_reads_back() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // Nothing configured yet. This host binds loopback with no `public_url`, so
    // it never offers a webhook URL (issue #203) — Telegram could not deliver
    // to one, and inbound rides `getUpdates` polling instead.
    let (status, cfg) = send(&state, "GET", "/api/v1/company/channels/telegram", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cfg["configured"], false);
    assert_eq!(cfg["tokenSet"], false);
    assert!(cfg["webhookUrl"].is_null(), "unreachable host: {cfg}");

    // Store both credentials (write-only).
    let (status, cfg) = send(
        &state,
        "PUT",
        "/api/v1/company/channels/telegram",
        Some(json!({ "botToken": BOT_TOKEN, "webhookSecret": WEBHOOK_SECRET })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cfg["configured"], true);
    assert_eq!(cfg["tokenSet"], true);
    assert_eq!(cfg["secretSet"], true);
    // Neither secret is ever echoed back.
    let body = cfg.to_string();
    assert!(
        !body.contains(BOT_TOKEN),
        "bot token leaked into PUT status"
    );
    assert!(
        !body.contains(WEBHOOK_SECRET),
        "secret leaked into PUT status"
    );

    // A partial write rotates the secret without re-sending the token.
    let (status, _) = send(
        &state,
        "PUT",
        "/api/v1/company/channels/telegram",
        Some(json!({ "webhookSecret": "rotated" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, cfg) = send(&state, "GET", "/api/v1/company/channels/telegram", None).await;
    assert_eq!(cfg["tokenSet"], true, "token survived a secret-only PUT");

    // DELETE clears both.
    let (status, cfg) = send(&state, "DELETE", "/api/v1/company/channels/telegram", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cfg["configured"], false);
    assert_eq!(cfg["tokenSet"], false);
}

#[tokio::test]
async fn telegram_webhook_rejects_an_unverified_post() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    send(
        &state,
        "PUT",
        "/api/v1/company/channels/telegram",
        Some(json!({ "botToken": BOT_TOKEN, "webhookSecret": WEBHOOK_SECRET })),
    )
    .await;

    // No secret header at all.
    let (status, _, _) = telegram_hook(&state, None, telegram_update(1, "hi")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Wrong secret.
    let (status, _, _) = telegram_hook(&state, Some("nope"), telegram_update(1, "hi")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn telegram_inbound_runs_a_turn_and_delivers_the_reply_back() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let api = RecordingTelegramApi::new();
    let state = state_with_telegram(&home, api.clone()).await;
    send(
        &state,
        "PUT",
        "/api/v1/company/channels/telegram",
        Some(json!({ "botToken": BOT_TOKEN, "webhookSecret": WEBHOOK_SECRET })),
    )
    .await;

    // A verified inbound update runs one cycle; the echo brain replies and the
    // reply is delivered back to the ORIGIN chat (555), not any other.
    let (status, body, raw) = telegram_hook(
        &state,
        Some(WEBHOOK_SECRET),
        telegram_update(555, "status?"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["delivered"], 1);
    assert_eq!(api.sent(), vec![(555, "You said: status?".to_string())]);
    // The bot token never appears in the webhook response.
    assert!(
        !raw.contains(BOT_TOKEN),
        "token leaked into webhook response"
    );
}

#[tokio::test]
async fn telegram_set_webhook_registers_the_public_url() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let api = RecordingTelegramApi::new();
    // The hosted shape: a real public https URL Telegram can deliver to.
    let state = state_with_telegram_at(&home, api.clone(), Some("https://acme.example")).await;
    send(
        &state,
        "PUT",
        "/api/v1/company/channels/telegram",
        Some(json!({ "botToken": BOT_TOKEN, "webhookSecret": WEBHOOK_SECRET })),
    )
    .await;

    // The status advertises the webhook only on such a host.
    let (_, cfg) = send(&state, "GET", "/api/v1/company/channels/telegram", None).await;
    assert_eq!(
        cfg["webhookUrl"],
        "https://acme.example/hooks/acme/telegram"
    );

    let (status, res) = send(
        &state,
        "POST",
        "/api/v1/company/channels/telegram/webhook",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["ok"], true);
    let webhooks = api.webhooks();
    assert_eq!(webhooks.len(), 1);
    assert_eq!(webhooks[0], "https://acme.example/hooks/acme/telegram");
}

/// Issue #203: on a host with no public https URL, registering a webhook is
/// refused outright. Accepting it would be actively harmful — Telegram could
/// never deliver to the URL *and* a registration blocks `getUpdates`, so it
/// would take down the one inbound path that does work.
#[tokio::test]
async fn telegram_set_webhook_is_refused_on_a_host_telegram_cannot_reach() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let api = RecordingTelegramApi::new();
    let state = state_with_telegram(&home, api.clone()).await;
    send(
        &state,
        "PUT",
        "/api/v1/company/channels/telegram",
        Some(json!({ "botToken": BOT_TOKEN, "webhookSecret": WEBHOOK_SECRET })),
    )
    .await;

    let (status, res) = send(
        &state,
        "POST",
        "/api/v1/company/channels/telegram/webhook",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        res.to_string().contains("OPENCOMPANY_PUBLIC_URL"),
        "the refusal must say how to fix it: {res}"
    );
    assert!(
        api.webhooks().is_empty(),
        "no loopback URL was ever handed to Telegram"
    );
}

/// The channel is usable with a bot token alone: no webhook secret, no public
/// URL, no `setWebhook` — the polling listener covers inbound.
#[tokio::test]
async fn telegram_is_configured_by_a_bot_token_alone() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let api = RecordingTelegramApi::new();
    let state = state_with_telegram(&home, api).await;

    let (status, cfg) = send(
        &state,
        "PUT",
        "/api/v1/company/channels/telegram",
        Some(json!({ "botToken": BOT_TOKEN })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cfg["configured"], true, "a token is the whole setup: {cfg}");
    assert_eq!(cfg["secretSet"], false);
    assert!(cfg["webhookUrl"].is_null());
    // The host has a transport wired, so it long-polls for inbound.
    assert_eq!(cfg["polling"], true);
}

#[tokio::test]
async fn telegram_token_never_leaks_even_when_delivery_fails() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // A transport that fails with an error embedding the bot token.
    let api = RecordingTelegramApi::failing_with_token_echo();
    let state = state_with_telegram(&home, api).await;
    send(
        &state,
        "PUT",
        "/api/v1/company/channels/telegram",
        Some(json!({ "botToken": BOT_TOKEN, "webhookSecret": WEBHOOK_SECRET })),
    )
    .await;

    // The turn still runs; a delivery failure never fails the webhook and never
    // surfaces the token in the response body.
    let (status, body, raw) =
        telegram_hook(&state, Some(WEBHOOK_SECRET), telegram_update(42, "ping")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["delivered"], 0);
    assert!(
        !raw.contains(BOT_TOKEN),
        "token leaked on a failed delivery"
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
        title: title.into(),
        note: None,
        column: "in_review".into(),
        priority: "medium".into(),
        assignee: "ceo".into(),
        updated_at_millis: 1,
        origin_chat_id: None,
        parent_task_id: parent.map(str::to_string),
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
            chat_id: "t-1".into(),
            agent_id: "ceo".into(),
            text: "on it".into(),
            steps: Vec::new(),
            task_id: Some("t-1".into()),
        },
        // An ordinary chat reply — excluded.
        CompanyEvent::AgentReply {
            chat_id: "General".into(),
            agent_id: "ceo".into(),
            text: "unrelated chatter".into(),
            steps: Vec::new(),
            task_id: None,
        },
        // Tagged to a different task — excluded.
        CompanyEvent::AgentReply {
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

/// #185 review follow-up: pin the two timeline branches the first test skipped —
/// `tool_failed`, and the window-correlated `approval` arm.
///
/// The approval arm is the only branch in `task_timeline` whose correlation is
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
                title: "Ship it".into(),
                note: None,
                column: "in_review".into(),
                priority: "medium".into(),
                assignee: "ceo".into(),
                updated_at_millis: 1,
                origin_chat_id: None,
                parent_task_id: None,
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
                title: "Ship it".into(),
                note: None,
                column: "in_progress".into(),
                priority: "medium".into(),
                assignee: "ceo".into(),
                updated_at_millis: 1,
                origin_chat_id: None,
                parent_task_id: None,
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
        .record_parked(&id, &parked_effect(), parked_at)
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
        .record_parked(&id, &parked_effect(), parked_at)
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
        .record_parked(&id, &parked_effect(), dispatched_at - 3_600_000)
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
        .record_parked(&ApprovalId::new("appr-live"), &parked_effect(), parked_at)
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
