//! Runtime tests: dispatch, quiescing refusal, and card/DM relay journaling.

#[cfg(feature = "openhuman")]
use super::tests_core::RecordingMeter;
use super::{emergency_from_load, task_enters_in_progress, task_enters_planning};
#[cfg(feature = "openhuman")]
use crate::ports::tasks::TaskTitle;
#[cfg(feature = "openhuman")]
use std::sync::Arc;

pub(super) async fn runtime_and_record() -> (
    super::CompanyRuntime,
    crate::ports::CompanyRecord,
    tempfile::TempDir,
) {
    let home = tempfile::tempdir().expect("tempdir");
    let manifest: crate::company::CompanyManifest = toml::from_str(
        r#"
        [company]
        name = "Acme"

        [[agent]]
        id = "ceo"
        role = "Chief"

        [policy]
        mode = "supervised"
        "#,
    )
    .expect("manifest");
    let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_id(crate::ports::types::CompanyId::new("acme"))
        .build()
        .await
        .expect("runtime");
    let record = runtime
        .store()
        .load(runtime.id())
        .await
        .expect("load")
        .expect("record");
    (runtime, record, home)
}

/// `set_lifecycle` must serialize its load-modify-save cycle against
/// `company_write_lock`, exactly like every other console load-modify-save
/// (PR #1875 review finding, second round). Proven the same way
/// `put_logo_serializes_against_the_company_write_lock`
/// (`server/ops/company_logo.rs`) proves it for that handler: hold the
/// lock externally, drive the real method, and demand it cannot finish
/// while the lock is held.
#[tokio::test]
async fn set_lifecycle_serializes_against_the_company_write_lock() {
    let (runtime, _record, _home) = runtime_and_record().await;
    let runtime = std::sync::Arc::new(runtime);
    let id = runtime.id().clone();

    let lock = crate::ports::store::company_write_lock(&id);
    let guard = lock.lock().await;

    let runtime_for_task = runtime.clone();
    let mut task = tokio::spawn(async move {
        runtime_for_task
            .set_lifecycle(
                "paused",
                crate::ports::types::Actor {
                    kind: crate::ports::types::ActorKind::Operator,
                    id: "op".to_string(),
                },
            )
            .await
    });

    // The method must be blocked behind the held lock — give it every
    // chance to (wrongly) race ahead before declaring it stuck.
    let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
        .await
        .is_ok();
    assert!(
        !raced_ahead,
        "set_lifecycle completed while company_write_lock was held \
         elsewhere — it is not serializing its load-modify-save cycle \
         against concurrent writers (e.g. a racing name-confirm PATCH)"
    );

    drop(guard);
    let from = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("set_lifecycle never resumed after the lock was released")
        .expect("task panicked")
        .expect("set_lifecycle failed");
    assert_eq!(from, "running", "the fixture starts running");
}

/// The shared workflow-wiring fixture, re-exported under the name these
/// tests already use.
#[cfg(feature = "openhuman")]
use crate::harness::workflow_wiring_deps as wiring_deps;

#[cfg(feature = "openhuman")]
#[tokio::test]
async fn workflow_wiring_is_absent_without_harness_deps() {
    let (runtime, record, _home) = runtime_and_record().await;
    assert_eq!(runtime.wired_workflow_namespaces(&record).await, None);
}

#[cfg(feature = "openhuman")]
#[tokio::test]
async fn workflow_wiring_keeps_the_static_capability_filter_without_a_plan() {
    let (mut runtime, record, _home) = runtime_and_record().await;
    runtime.set_workflow_harness_deps(wiring_deps(
        &runtime,
        None,
        crate::harness::toolbelt::CapabilityFilter::DenyNamespaces(["web"].into_iter().collect()),
        None,
    ));
    let namespaces = runtime
        .wired_workflow_namespaces(&record)
        .await
        .expect("wiring");
    assert!(!namespaces.contains("web"));
    assert!(namespaces.contains("shell"));
}

#[cfg(feature = "openhuman")]
#[tokio::test]
async fn workflow_wiring_resolves_the_plan_against_its_company_meter() {
    let (mut runtime, record, _home) = runtime_and_record().await;
    let meter = Arc::new(RecordingMeter::default());
    runtime.set_workflow_harness_deps(wiring_deps(
        &runtime,
        Some(meter.clone()),
        crate::harness::toolbelt::CapabilityFilter::AllowAll,
        Some(crate::harness::capability_budget::CapabilityPlan {
            period: crate::harness::capability_budget::BudgetPeriod::Daily,
            budgets: [("shell".to_string(), u64::MAX)].into_iter().collect(),
            total_budget: None,
        }),
    ));
    let namespaces = runtime
        .wired_workflow_namespaces(&record)
        .await
        .expect("wiring");
    assert!(namespaces.contains("shell"));
    assert!(!namespaces.contains("web"));
    assert!(!namespaces.contains("code"));
    assert_eq!(*meter.queried_companies.lock().unwrap(), vec![record.id]);
}

/// Issue #874: the wiring carries **why** a namespace is out, not just that
/// it is — the two reasons `refusal_for` renders at run time, so a caller
/// (the `tool-slugs` route) can tell an operator "no provider configured"
/// apart from "your capability tier filtered it" before a run fails.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn workflow_wiring_names_why_each_namespace_is_unwired() {
    let (mut runtime, record, _home) = runtime_and_record().await;
    // `wiring_deps` leaves `search: None` — the staging shape in issue #874,
    // where `searchCredentialConfigured` was false — and we deny `web` on top
    // so both reasons appear in one map.
    runtime.set_workflow_harness_deps(wiring_deps(
        &runtime,
        None,
        crate::harness::toolbelt::CapabilityFilter::DenyNamespaces(["web"].into_iter().collect()),
        None,
    ));
    let wiring = runtime.workflow_tool_wiring(&record).await.expect("wiring");
    assert_eq!(
        wiring.missing.get("search").copied(),
        Some(crate::workflows::caps::MissingReason::SearchBackendNotConfigured),
        "no search backend is configured: {:?}",
        wiring.missing
    );
    assert_eq!(
        wiring.missing.get("web").copied(),
        Some(crate::workflows::caps::MissingReason::CapabilityTierFiltered),
        "web is denied by the capability filter: {:?}",
        wiring.missing
    );
    assert!(
        !wiring.missing.contains_key("shell"),
        "a wired namespace carries no reason: {:?}",
        wiring.missing
    );
}

/// An uncredentialed process-wide Search handle exists so a company key
/// added later can reuse its ledger. Presence is not availability: without
/// either credential, workflow grounding must still report Search unwired.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn workflow_wiring_rejects_an_uncredentialed_search_handle() {
    let (mut runtime, mut record, _home) = runtime_and_record().await;
    record.manifest.tools.allow.push("search".to_string());
    let mut deps = wiring_deps(
        &runtime,
        None,
        crate::harness::toolbelt::CapabilityFilter::AllowAll,
        None,
    );
    deps.search = Some(crate::harness::search::SearchBackend::new(
        "https://api.tinyhumans.ai".to_string(),
        crate::company::credentials::Credential::None,
        crate::company::DEFAULT_SEARCH_DAILY_CALLS,
    ));
    deps.secrets = Some(runtime.secrets().clone());
    runtime.set_workflow_harness_deps(deps);

    let wiring = runtime.workflow_tool_wiring(&record).await.expect("wiring");
    assert_eq!(
        wiring.missing.get("search").copied(),
        Some(crate::workflows::caps::MissingReason::SearchBackendNotConfigured)
    );

    runtime
        .secrets()
        .set(
            &record.id,
            crate::company::search::MANAGED_KEY_SECRET,
            crate::ports::types::SecretValue("company-search-key".to_string()),
        )
        .await
        .expect("store company Search key");
    let wiring = runtime.workflow_tool_wiring(&record).await.expect("wiring");
    assert!(wiring.wired_namespaces.contains("search"));
}

/// Issue #874, the staging repro at the layer the route reads: a company that
/// explicitly grants `search` on a deployment with **no** search backend must
/// not be offered `web_search` for grounding — it must be reported as granted
/// but unwired instead, so the copilot cannot author a node that dies at the
/// first run.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn a_granted_but_unwired_tool_is_reported_not_offered() {
    let (mut runtime, mut record, _home) = runtime_and_record().await;
    record.manifest.tools.allow.push("search".to_string());
    record.manifest.tools.allow.push("shell".to_string());
    runtime.set_workflow_harness_deps(wiring_deps(
        &runtime,
        None,
        crate::harness::toolbelt::CapabilityFilter::AllowAll,
        None,
    ));
    let wiring = runtime.workflow_tool_wiring(&record).await;
    let wired = wiring.as_ref().map(|w| &w.wired_namespaces);

    let effective = crate::company::workflow_effective_tool_slugs(&record, wired);
    let unwired = crate::company::workflow_granted_but_unwired_tool_slugs(&record, wired);
    assert!(
        !effective.iter().any(|slug| slug == "web_search"),
        "an unwired search tool is not offered for grounding: {effective:?}"
    );
    assert!(
        unwired.iter().any(|slug| slug == "web_search"),
        "…but it IS reported as granted-and-unwired: {unwired:?}"
    );
    assert!(
        effective.iter().any(|slug| slug == "shell"),
        "a granted AND wired tool is still offered: {effective:?}"
    );
    // The two lists partition the granted set: nothing may appear in both, or
    // a caller grounding on one and warning from the other contradicts itself.
    assert!(
        !effective.iter().any(|slug| unwired.contains(slug)),
        "effective {effective:?} and unwired {unwired:?} overlap"
    );
}

/// The other half of the honesty split: with no harness deps the wiring is
/// *unknowable*, so every granted tool stays offered and nothing is claimed
/// to be unwired. Reporting "all granted tools are broken" on a host that
/// simply cannot say would be the worse failure.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn unknowable_wiring_offers_the_grant_only_set_and_reports_nothing_unwired() {
    let (runtime, mut record, _home) = runtime_and_record().await;
    record.manifest.tools.allow.push("search".to_string());
    let wiring = runtime.workflow_tool_wiring(&record).await;
    assert!(wiring.is_none(), "no harness deps means no wiring answer");
    let wired = wiring.as_ref().map(|w| &w.wired_namespaces);

    assert!(
        crate::company::workflow_effective_tool_slugs(&record, wired)
            .iter()
            .any(|slug| slug == "web_search"),
        "a granted tool is still offered when the deployment cannot be asked"
    );
    assert!(
        crate::company::workflow_granted_but_unwired_tool_slugs(&record, wired).is_empty(),
        "nothing is claimed unwired when the deployment cannot be asked"
    );
}

/// Issue #86: the kill switch's boot decision, including the direction it
/// fails in.
///
/// The `Err` arm is the whole point. An unreadable log must not un-pause a
/// company an operator deliberately stopped: a company wrongly stopped is a
/// visible problem someone fixes with one request, while a company wrongly
/// running is exactly the outcome the endpoint exists to prevent, and
/// nothing would surface it.
#[test]
fn an_unreadable_record_comes_up_stopped() {
    assert!(emergency_from_load(Err(
        crate::error::OpenCompanyError::CompanyNotFound("acme".into())
    )));
}

/// The other three arms, which must stay distinct from the error case.
#[test]
fn a_readable_record_is_taken_at_its_word() {
    // Stopped stays stopped across the restart.
    assert!(emergency_from_load(Ok(Some(true))));
    // Running stays running — the switch is not sticky by accident.
    assert!(!emergency_from_load(Ok(Some(false))));
    // Nothing known is not the same as a read failure.
    assert!(!emergency_from_load(Ok(None)));
}

/// Issue #337: the planning edge, on the same terms as the dispatch one.
/// Entering the column fires; resting in it does not.
#[test]
fn planning_fires_only_on_entering_planning() {
    // The drag this feature exists for.
    assert!(task_enters_planning(Some("todo"), "planning"));
    // A card created straight into Planning is a genuine entry too.
    assert!(task_enters_planning(None, "planning"));
    // Already planning, re-saved — an edit, the pass's own note append, a
    // re-title. This is what makes "one pass per entry, no retry" a
    // property of the edge rather than a rule the planner has to remember,
    // and it is what stops the settle's own write re-triggering the pass.
    assert!(!task_enters_planning(Some("planning"), "planning"));
    // Leaving Planning never fires it — including the success settle.
    assert!(!task_enters_planning(Some("planning"), "in_progress"));
    assert!(!task_enters_planning(Some("planning"), "todo"));
    // No other column entry fires it.
    for column in ["todo", "in_progress", "paused", "in_review", "done"] {
        assert!(!task_enters_planning(Some("todo"), column), "{column}");
    }
}

/// Issue #576: a prompt-box card buys **exactly one** planning pass across
/// its whole life — not zero, not two.
///
/// The assertions above pin the edge one transition at a time. This walks
/// the sequence a self-promoting card actually goes through and *counts*,
/// because the two ways to get this wrong are both invisible to a
/// single-transition test:
///
/// * **Zero** — the card is created directly in `planning` rather than
///   moved there, so if entry required a previous column there would be no
///   transition to observe and the pass would never fire. The card would sit
///   in Planning forever, which is the one column that must never hold a
///   card at rest.
/// * **Two** — the pass writes its plan back onto the card *while the card
///   is still in Planning* (`harness::planning`, via `upsert_task`). If
///   resting in the column counted as entering it, that write-back would
///   start a second pass, which would write back, and bill a model call each
///   time.
///
/// A test that merely asserted "it planned" would pass in the second case.
#[test]
fn a_prompt_box_card_buys_exactly_one_planning_pass() {
    // The life of a card opened from the prompt box: created directly in
    // Planning, its plan written back while it rests there, then settled
    // onward by the pass itself.
    let life = [
        (None, "planning"),                // the prompt box opens it
        (Some("planning"), "planning"),    // the pass writes the plan back
        (Some("planning"), "in_progress"), // the success settle
    ];
    let fires = life
        .iter()
        .filter(|(prev, next)| task_enters_planning(*prev, next))
        .count();
    assert_eq!(
        fires, 1,
        "a prompt-box card must buy exactly one planning pass: {life:?}"
    );

    // And the failure exit, which returns the card to To-do, must not buy a
    // second one on the way out either.
    let returned = [(None, "planning"), (Some("planning"), "todo")];
    assert_eq!(
        returned
            .iter()
            .filter(|(prev, next)| task_enters_planning(*prev, next))
            .count(),
        1,
        "a pass that returned the card must still have cost exactly one"
    );
}

/// The two edges are mutually exclusive by construction: one write names
/// one target column, so no upsert can both plan and dispatch a card. This
/// is what makes the "planning happens BEFORE dispatch" ordering structural
/// rather than a matter of which `if` runs first in `upsert_task`.
#[test]
fn no_single_write_both_plans_and_dispatches() {
    for prev in [None, Some("todo"), Some("planning"), Some("in_progress")] {
        for next in [
            "todo",
            "planning",
            "in_progress",
            "paused",
            "in_review",
            "done",
        ] {
            assert!(
                !(task_enters_planning(prev, next) && task_enters_in_progress(prev, next)),
                "{prev:?} → {next} fires both edges"
            );
        }
    }
}

/// The success settle's shape, pinned end to end: a pass that clears the
/// card writes `planning → in_progress`, which is NOT a planning entry (so
/// it cannot loop) and IS a dispatch entry (so the plan actually hands the
/// work on). Both halves matter; either one alone would be a bug.
#[test]
fn a_cleared_plan_hands_the_card_on_without_replanning_it() {
    assert!(
        !task_enters_planning(Some("planning"), "in_progress"),
        "the settle must not re-enter the pass it is settling"
    );
    assert!(
        task_enters_in_progress(Some("planning"), "in_progress"),
        "the settle must fire the dispatch edge — that is why it routes \
         through upsert_task rather than the plain store port"
    );
}

#[test]
fn dispatch_only_on_entering_in_progress() {
    // Fresh card created straight into `in_progress` → dispatch.
    assert!(task_enters_in_progress(None, "in_progress"));
    // The drag: todo → in_progress → dispatch.
    assert!(task_enters_in_progress(Some("todo"), "in_progress"));
    // Issue #301: planning sits before dispatch, so entering it must not
    // fire one — and leaving it for `in_progress` must.
    assert!(!task_enters_in_progress(Some("todo"), "planning"));
    assert!(task_enters_in_progress(Some("planning"), "in_progress"));
    // Already in_progress, re-saved (e.g. an edit) → no re-dispatch.
    assert!(!task_enters_in_progress(Some("in_progress"), "in_progress"));
    // Any non-in_progress target → no dispatch.
    assert!(!task_enters_in_progress(Some("in_progress"), "in_review"));
    assert!(!task_enters_in_progress(None, "todo"));
    assert!(!task_enters_in_progress(Some("in_review"), "done"));
}

/// Issue #246 spend gate. A card opened from chat goes through
/// `POST …/tasks` with **no** `column`, so what stops it from spending
/// money the operator never approved is that the server's default column is
/// not the dispatch trigger. That is two independent facts — what the
/// default is, and what the trigger is — living in two different modules,
/// so a change to either alone silently opens the gate. This pins them
/// together.
///
/// The second assertion is the positive control: without it the first
/// would still pass if `task_enters_in_progress` were broken to always
/// return `false`, and the test would be guarding nothing.
#[test]
fn the_column_a_chat_created_card_defaults_to_does_not_dispatch() {
    use crate::ports::tasks::{COLUMN_IN_PROGRESS, COLUMN_TODO};

    // `create_task` (src/server/ops/tasks.rs) defaults an omitted `column`
    // to this one.
    assert!(
        !task_enters_in_progress(None, COLUMN_TODO),
        "a chat-created card must not spend an agent turn on arrival — the \
         human drag into in_progress is the approval gate"
    );
    assert!(
        task_enters_in_progress(None, COLUMN_IN_PROGRESS),
        "positive control: the trigger this test relies on is still live"
    );
}

/// Issue #242: the attempt row exists **before** the cycle is spawned, in
/// [`RunStatus::Pending`], carrying the assignee it was dispatched to and a
/// 1-based ordinal that climbs per re-dispatch. This is the whole point of
/// minting at the choke point rather than inside the cycle — a host that
/// dies in the gap leaves a visible orphan instead of nothing.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn a_dispatch_opens_a_pending_attempt_before_the_cycle_spawns() {
    use crate::ports::TaskRecord;
    use crate::ports::runs::{RunFilter, RunStatus};
    use crate::ports::tasks::COLUMN_IN_PROGRESS;

    let home = tempfile::Builder::new()
        .prefix("opencompany-run-open-")
        .tempdir()
        .expect("tempdir");
    let manifest: crate::company::CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
    )
    .expect("manifest");
    let id = crate::ports::types::CompanyId::new("acme");
    let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_id(id.clone())
        .build()
        .await
        .expect("runtime");

    let card = TaskRecord {
        id: "t-1".to_string(),
        title: TaskTitle::authored("Ship it"),
        note: None,
        column: COLUMN_IN_PROGRESS.to_string(),
        priority: "medium".to_string(),
        assignee: "ceo".to_string(),
        updated_at_millis: 0,
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
    };

    let first = runtime.open_run(&card).await.expect("an attempt is minted");
    let runs = runtime
        .runs()
        .list_runs(&id, &RunFilter::for_task("t-1"))
        .await
        .expect("list");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, first);
    assert_eq!(
        runs[0].status,
        RunStatus::Pending,
        "the row is written before anything runs, so it starts Pending"
    );
    assert_eq!(runs[0].attempt, 1, "the first attempt at a card is 1");
    assert_eq!(runs[0].agent_id, "ceo");
    assert!(
        runs[0].trigger_event_seq.is_none(),
        "the driving event has not been appended yet"
    );
    assert!(runs[0].started_at_millis.is_none());

    // A re-dispatch is a NEW attempt, never a resurrection of the first.
    let second = runtime.open_run(&card).await.expect("a second attempt");
    assert_ne!(second, first);
    let runs = runtime
        .runs()
        .list_runs(&id, &RunFilter::for_task("t-1"))
        .await
        .expect("list");
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs.iter()
            .find(|r| r.id == second)
            .expect("second")
            .attempt,
        2
    );
}

/// Issue #290 against issue #242's write path: a card dragged into
/// `in_progress` while this runtime is being replaced must not leave an
/// attempt row claiming to be pending forever.
///
/// The board write is deliberately *not* gated on the quiesce — only cycles
/// are — so this window is reachable, and the refusal happens before
/// `CycleRunner` starts the run, which puts it out of reach of the cycle's
/// own terminality backstop. A rebuild also skips the boot reaper by design,
/// so nothing else would ever clean the row up.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn a_dispatch_refused_by_a_quiescing_runtime_settles_its_attempt() {
    use std::sync::Arc;

    use crate::ports::TaskRecord;
    use crate::ports::runs::{RUNTIME_REPLACED_ERROR, RunStatus};
    use crate::ports::tasks::COLUMN_IN_PROGRESS;

    let home = tempfile::Builder::new()
        .prefix("opencompany-run-quiesce-")
        .tempdir()
        .expect("tempdir");
    let manifest: crate::company::CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
    )
    .expect("manifest");
    let id = crate::ports::types::CompanyId::new("acme");
    let runtime = Arc::new(
        crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .expect("runtime"),
    );

    let card = TaskRecord {
        id: "t-1".to_string(),
        title: TaskTitle::authored("Ship it"),
        note: None,
        column: COLUMN_IN_PROGRESS.to_string(),
        priority: "medium".to_string(),
        assignee: "ceo".to_string(),
        updated_at_millis: 0,
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
    };

    // Positive control: on a live runtime the cycle runs, so the row is
    // settled by the backstop inside it and never reaches the path below.
    let live = runtime.open_run(&card).await.expect("an attempt");
    Arc::clone(&runtime)
        .run_dispatch_cycle(card.id.clone(), Some(live.clone()))
        .await;
    let settled = runtime
        .runs()
        .get_run(&id, &live)
        .await
        .expect("read")
        .expect("row");
    assert!(
        settled.status.is_terminal(),
        "the ordinary dispatch path still settles its own row"
    );
    assert_ne!(
        settled.error.as_deref(),
        Some(RUNTIME_REPLACED_ERROR),
        "the live path must not be settled by the quiesce handler"
    );

    // The window this test exists for.
    let stranded = runtime.open_run(&card).await.expect("an attempt");
    runtime.quiesce().await;
    Arc::clone(&runtime)
        .run_dispatch_cycle(card.id.clone(), Some(stranded.clone()))
        .await;

    let abandoned = runtime
        .runs()
        .get_run(&id, &stranded)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        abandoned.status,
        RunStatus::Failed,
        "an attempt whose cycle was refused must not stay Pending"
    );
    assert_eq!(
        abandoned.error.as_deref(),
        Some(RUNTIME_REPLACED_ERROR),
        "and it must say the runtime was swapped, not that the host died"
    );
    assert!(
        abandoned.started_at_millis.is_none(),
        "it never started, so it has no start time"
    );
    assert!(abandoned.finished_at_millis.is_some());
}

/// Issue #2369: the dispatch event names the conversation the card came from.
///
/// The console paints a "working" row from `TaskDispatched`, so the *start*
/// of an agent turn is only visible in the thread that asked if the event
/// carries that thread. It used to carry neither field, and a thread that
/// dispatched went silent from the hand-off until the answer arrived —
/// minutes of a real turn rendering as nothing, then a reply from nowhere.
///
/// Proven on `run_dispatch_cycle` itself rather than on the projection,
/// because the derivation is a board read inside that function: the card is
/// the only place `TaskOrigin` lives, so a test that hands the origin in
/// would prove nothing about the path the runtime actually takes.
///
/// The board-created card is the other half, and not a throwaway: it must
/// emit the event with **no** conversation, or a card raised on the board
/// would paint a working row in whatever thread happened to be open.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn a_dispatch_names_the_conversation_its_card_came_from() {
    use super::CompanyEvent;
    use crate::ports::TaskRecord;
    use crate::ports::tasks::COLUMN_IN_PROGRESS;
    use crate::ports::types::EventSeq;

    let home_dir = tempfile::Builder::new()
        .prefix("opencompany-dispatch-origin-")
        .tempdir()
        .expect("tempdir");
    let manifest: crate::company::CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
    )
    .expect("manifest");
    let id = crate::ports::types::CompanyId::new("acme");
    let runtime = Arc::new(
        crate::runtime::RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .expect("runtime"),
    );

    let card = |task_id: &str, origin: Option<crate::ports::TaskOrigin>| TaskRecord {
        id: task_id.to_string(),
        title: TaskTitle::authored("Ship it"),
        note: None,
        column: COLUMN_IN_PROGRESS.to_string(),
        priority: "medium".to_string(),
        assignee: "ceo".to_string(),
        updated_at_millis: 0,
        origin,
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
    };

    // Raised inside a thread of `strategy`, and raised on the board.
    let in_thread = card(
        "t-thread",
        crate::ports::TaskOrigin::new(Some("strategy".to_string()), Some(EventSeq::new(41))),
    );
    let on_board = card("t-board", None);
    for record in [&in_thread, &on_board] {
        runtime
            .ops
            .tasks
            .upsert(&id, record)
            .await
            .expect("the card is on the board");
        let run_id = runtime.open_run(record).await;
        Arc::clone(&runtime)
            .run_dispatch_cycle(record.id.clone(), run_id)
            .await;
    }

    let events = runtime
        .events
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .expect("read journal");
    let dispatched: Vec<_> = events
        .iter()
        .filter_map(|stored| match &stored.event {
            CompanyEvent::TaskDispatched {
                task_id,
                origin_chat_id,
                origin_parent,
                ..
            } => Some((task_id.clone(), origin_chat_id.clone(), *origin_parent)),
            _ => None,
        })
        .collect();

    assert_eq!(
        dispatched
            .iter()
            .find(|(task_id, ..)| task_id == "t-thread")
            .map(|(_, chat, parent)| (chat.clone(), *parent)),
        Some((Some("strategy".to_string()), Some(EventSeq::new(41)))),
        "the dispatch must name the thread that asked, found {dispatched:?}"
    );
    assert_eq!(
        dispatched
            .iter()
            .find(|(task_id, ..)| task_id == "t-board")
            .map(|(_, chat, parent)| (chat.clone(), *parent)),
        Some((None, None)),
        "a board-created card belongs to no conversation, found {dispatched:?}"
    );
}
