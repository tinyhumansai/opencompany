//! Runtime tests: parked-approval discrimination, continuation replies, pause/resume, planning and dispatch column transitions, and workflow-harness wiring reports.

use super::tests_dispatch::runtime_and_record;

/// A [`JournalStore`](crate::ports::journal::JournalStore) that refuses
/// every append once armed — a full or read-only data volume, which is the
/// failure mode `park_blocker`'s rollback exists for.
///
/// Armed by the test rather than from birth, so boot's own journal writes
/// still land and the runtime under test is an ordinary one that lost its
/// volume mid-life.
#[cfg(feature = "openhuman")]
#[derive(Default)]
pub(super) struct RefusingJournalStore {
    inner: crate::ports::journal::MemoryJournalStore,
    armed: std::sync::atomic::AtomicBool,
    allow_before_failing: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "openhuman")]
impl RefusingJournalStore {
    pub(super) fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Lets appends land again — the volume coming back after a transient
    /// failure.
    pub(super) fn disarm(&self) {
        self.armed.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Once armed, lets the next `n` appends land before refusing —
    /// so the failure can be aimed at a later write in the same request
    /// rather than the very first one.
    pub(super) fn allow_next(&self, n: usize) {
        self.allow_before_failing
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(feature = "openhuman")]
#[async_trait::async_trait]
impl crate::ports::journal::JournalStore for RefusingJournalStore {
    async fn append_journal(
        &self,
        id: &crate::ports::types::CompanyId,
        line: &str,
        durability: crate::ports::journal::Durability,
    ) -> crate::Result<()> {
        if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
            let remaining = self
                .allow_before_failing
                .load(std::sync::atomic::Ordering::SeqCst);
            if remaining > 0 {
                self.allow_before_failing
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            } else {
                return Err(crate::error::OpenCompanyError::Store(
                    "RefusingJournalStore: the volume is full".to_string(),
                ));
            }
        }
        self.inner.append_journal(id, line, durability).await
    }

    async fn read_journal(
        &self,
        id: &crate::ports::types::CompanyId,
    ) -> crate::Result<Vec<String>> {
        self.inner.read_journal(id).await
    }

    async fn journal_imported(&self, id: &crate::ports::types::CompanyId) -> crate::Result<bool> {
        self.inner.journal_imported(id).await
    }

    async fn complete_import(
        &self,
        id: &crate::ports::types::CompanyId,
        lines: Vec<String>,
    ) -> crate::Result<()> {
        self.inner.complete_import(id, lines).await
    }
}
/// A journal that lets a **competing** request run to completion inside the
/// next append, so a race needing one caller suspended mid-`await` is
/// exercised deterministically rather than by hoping two tasks interleave.
#[cfg(feature = "openhuman")]
#[derive(Default)]
pub(super) struct RacingJournalStore {
    inner: crate::ports::journal::MemoryJournalStore,
    interleave: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[cfg(feature = "openhuman")]
impl RacingJournalStore {
    /// Runs `run` once, inside the next append.
    pub(super) fn interleave_next(&self, run: impl FnOnce() + Send + 'static) {
        *self.interleave.lock().expect("interleave poisoned") = Some(Box::new(run));
    }
}

#[cfg(feature = "openhuman")]
#[async_trait::async_trait]
impl crate::ports::journal::JournalStore for RacingJournalStore {
    async fn append_journal(
        &self,
        id: &crate::ports::types::CompanyId,
        line: &str,
        durability: crate::ports::journal::Durability,
    ) -> crate::Result<()> {
        let racer = self.interleave.lock().expect("interleave poisoned").take();
        if let Some(racer) = racer {
            racer();
        }
        self.inner.append_journal(id, line, durability).await
    }

    async fn read_journal(
        &self,
        id: &crate::ports::types::CompanyId,
    ) -> crate::Result<Vec<String>> {
        self.inner.read_journal(id).await
    }

    async fn journal_imported(&self, id: &crate::ports::types::CompanyId) -> crate::Result<bool> {
        self.inner.journal_imported(id).await
    }

    async fn complete_import(
        &self,
        id: &crate::ports::types::CompanyId,
        lines: Vec<String>,
    ) -> crate::Result<()> {
        self.inner.complete_import(id, lines).await
    }
}

use crate::ports::tasks::TaskTitle;

/// Issue #880: which parked approvals name a workflow run, and which must
/// not.
///
/// The discrimination is the whole content of the change, because
/// `Effect::run_id` carries two id spaces — issue #242's task attempt and
/// the workflow run — and `generate_id` is only process-locally unique, so
/// the value cannot be inspected to tell them apart. Getting this wrong in
/// the permissive direction would print a task-attempt id on an approvals
/// card as though it were a workflow run.
#[test]
fn only_an_unlinked_park_with_a_run_id_names_a_workflow_run() {
    use crate::ports::types::{ApprovalId, Effect, EffectGroup};
    use crate::runtime::journal::{PendingApproval, TaskLink};

    let parked = |task: Option<TaskLink>, run_id: Option<&str>| PendingApproval {
        id: ApprovalId::new("appr-1"),
        effect: Effect {
            kind: "publish_artifact".to_string(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({}),
            agent: Some("ceo".to_string()),
            run_id: run_id.map(str::to_string),
        },
        at_millis: 1,
        deadline_anchor_millis: 1,
        task,
        thread: None,
        batch: None,
    };

    // A workflow park: `park_and_journal` records it explicitly unlinked
    // (#333) and the run stamps its id.
    assert_eq!(
        super::workflow_run_of(&parked(Some(TaskLink::Unlinked), Some("run-1"))),
        Some("run-1".to_string())
    );
    // A board task's attempt: same field, different id space. Must NOT be
    // reported as a workflow run.
    assert_eq!(
        super::workflow_run_of(&parked(
            Some(TaskLink::Task {
                id: "card-1".to_string()
            }),
            Some("attempt-1")
        )),
        None
    );
    // A chat turn: unlinked, but nothing stamped a run onto it.
    assert_eq!(
        super::workflow_run_of(&parked(Some(TaskLink::Unlinked), None)),
        None
    );
    // A pre-#333 line records no link at all, so the park site is unknown.
    // Conservative rather than guessing — the same fallback rule #333 set.
    assert_eq!(super::workflow_run_of(&parked(None, Some("run-1"))), None);
}

/// Issue #1092: a continuation whose approval was raised in no conversation
/// must never be journaled into the answering teammate's DM.
///
/// The fallback is the whole content of the fix, so it is asserted per park
/// site rather than through one happy path: the id it returns is what
/// `chat_history::owns` will (or will not) resolve to a chat thread.
#[test]
fn a_continuation_with_no_conversation_answers_outside_every_chat() {
    use crate::runtime::journal::{ApprovalOrigin, TaskLink};

    let origin = |task: Option<TaskLink>, run_id: Option<&str>| ApprovalOrigin {
        at_millis: 1,
        kind: "web_fetch".to_string(),
        task,
        run_id: run_id.map(str::to_string),
        thread: None,
        parent: None,
        cycle: None,
    };

    // A workflow node's parked call: unlinked, with the run stamped on it.
    // The run id is the destination — the timeline the operator was already
    // watching, and a value no desk answers to.
    assert_eq!(
        super::continuation_fallback_chat_id(
            Some(&origin(Some(TaskLink::Unlinked), Some("run-9"))),
            Some("dm:copywriter"),
        ),
        "run-9",
    );
    // A board card's dispatch: the card owns the work, exactly as
    // `journal_task_outcome` already records it.
    assert_eq!(
        super::continuation_fallback_chat_id(
            Some(&origin(
                Some(TaskLink::Task {
                    id: "card-3".to_string()
                }),
                Some("attempt-4"),
            )),
            Some("dm:copywriter"),
        ),
        "card-3",
    );
    // Unlinked with nothing stamped is an unaddressed operator turn, and a
    // pre-#333 line with no link at all is unknown. Both answer in the DM of
    // the agent that asked for the approval.
    let requester = Some("dm:copywriter");
    assert_eq!(
        super::continuation_fallback_chat_id(
            Some(&origin(Some(TaskLink::Unlinked), None)),
            requester
        ),
        "dm:copywriter",
    );
    assert_eq!(
        super::continuation_fallback_chat_id(Some(&origin(None, Some("run-9"))), requester),
        "dm:copywriter",
    );
    assert_eq!(
        super::continuation_fallback_chat_id(None, requester),
        "dm:copywriter"
    );
    assert_eq!(
        super::continuation_fallback_chat_id(None, None),
        "General",
        "only a company with nobody to answer as has nowhere else"
    );
}

/// Issue #1092, the property that actually matters: a workflow park's
/// continuation must not resolve to a teammate's DM or to a desk.
///
/// Asserted through `chat_history::owns` itself rather than by eyeballing
/// the string, so a change on either side fails here instead of silently
/// re-opening the leak. The General arm is asserted the other way round in
/// the same breath — it is *supposed* to be readable — because a fallback
/// that hid every continuation would pass a one-directional test and lose
/// the operator's answer.
#[test]
fn a_workflow_parks_continuation_owns_no_desk_and_no_dm() {
    use crate::ports::types::CompanyEvent;
    use crate::runtime::journal::{ApprovalOrigin, TaskLink};
    use crate::server::chat_history::owns;

    let reply = |chat_id: String| CompanyEvent::AgentReply {
        audience: Vec::new(),
        mentions: Vec::new(),
        mention_depth: 0,
        parent: None,
        chat_id,
        agent_id: "copywriter".to_string(),
        text: "re-issued".to_string(),
        steps: Vec::new(),
        task_id: None,
        outputs: Vec::new(),
        episode: None,
    };
    let origin = |task: Option<TaskLink>, run_id: Option<&str>| ApprovalOrigin {
        at_millis: 1,
        kind: "web_fetch".to_string(),
        task,
        run_id: run_id.map(str::to_string),
        thread: None,
        parent: None,
        cycle: None,
    };

    // The leak: a workflow node's park, answered into the copywriter's DM.
    let workflow = super::continuation_fallback_chat_id(
        Some(&origin(Some(TaskLink::Unlinked), Some("run-9"))),
        Some("dm:copywriter"),
    );
    for (desk_id, desk_name) in [
        ("copywriter", "Copywriter"),
        ("creative", "Creative studio"),
    ] {
        assert!(
            !owns(desk_id, desk_name, &reply(workflow.clone())),
            "`{workflow}` must not be read as the `{desk_id}` conversation",
        );
    }

    // And the other direction: an unaddressed operator turn answers in the
    // requesting agent's DM, where the person who approved is looking.
    let unaddressed = super::continuation_fallback_chat_id(
        Some(&origin(Some(TaskLink::Unlinked), None)),
        Some("dm:copywriter"),
    );
    assert!(
        owns("dm:copywriter", "copywriter", &reply(unaddressed.clone())),
        "`{unaddressed}` must be read as the requesting agent's DM",
    );
    assert!(
        !owns("main", "General", &reply(unaddressed.clone())),
        "`{unaddressed}` must not land in General",
    );
}

#[cfg(feature = "openhuman")]
use std::sync::Mutex;

#[cfg(feature = "openhuman")]
use async_trait::async_trait;

#[cfg(feature = "openhuman")]
#[derive(Default)]
pub(super) struct RecordingMeter {
    pub(super) queried_companies: Mutex<Vec<crate::ports::types::CompanyId>>,
}

#[cfg(feature = "openhuman")]
#[async_trait]
impl crate::ports::UsageMeter for RecordingMeter {
    async fn record(
        &self,
        _company: &crate::ports::types::CompanyId,
        _sample: &crate::ports::UsageSample,
    ) -> crate::Result<()> {
        Ok(())
    }

    async fn query(
        &self,
        company: &crate::ports::types::CompanyId,
        _since_millis: u64,
    ) -> crate::Result<Vec<crate::ports::UsageSample>> {
        self.queried_companies.lock().unwrap().push(company.clone());
        Ok(Vec::new())
    }
}

/// `is_busy` must see **all three** sources, not just the steer registry.
///
/// The first version of the busy endpoint read only `steer.any_inflight()`,
/// which covers dispatched board cards and desk delegations. A top-level
/// operator chat turn registers none of those — it takes `serial` and
/// nothing else — and workflow runs live in `run_supervisor`, a separate
/// registry. So the 15-minute turn opencompany-microservice#22 measured
/// reported `busy: false` and got parked mid-flight, which is exactly the
/// failure the endpoint exists to prevent.
///
/// Each source is exercised idle → busy → idle independently, so dropping
/// any one of them from `is_busy` fails here rather than silently in
/// production. Deliberately outside any feature gate: the steer registry is
/// only wired under `openhuman`, so a test that relied on it alone would not
/// run in the default build at all.
/// **B-037, the other half.** Pausing a company stops the runs it already
/// has in flight, not only the ones it would have started next.
///
/// Refusing new runs was the reported symptom; this is the promise on the
/// same settings screen. A graph twenty nodes into thirty goes on spending
/// until it finishes, and the operator who pressed Pause — usually because
/// of that run — had no control that reached it.
#[tokio::test]
async fn pausing_a_company_stops_the_runs_already_in_flight() {
    let (runtime, _record, _home) = runtime_and_record().await;

    let (ctx, _guard) = runtime
        .run_supervisor()
        .begin("wf-1", false)
        .expect("begin a workflow run");
    assert!(
        !ctx.cancel.is_cancelled(),
        "the run starts un-cancelled, or this test proves nothing"
    );

    runtime
        .set_lifecycle(
            "paused",
            crate::ports::types::Actor {
                kind: crate::ports::types::ActorKind::Operator,
                id: "operator".into(),
            },
        )
        .await
        .expect("pause the company");

    assert!(
        ctx.cancel.is_cancelled(),
        "pausing must fire the stop signal on a run already executing"
    );
}

/// The mirror, so the sweep cannot quietly become "cancel on every
/// transition": **resuming** must not stop the work it is resuming into.
///
/// A resume runs through the same `set_lifecycle`, so a guard keyed on the
/// wrong side of the comparison would kill runs at exactly the moment the
/// operator asked for them to continue.
#[tokio::test]
async fn resuming_a_company_does_not_stop_anything() {
    let (runtime, _record, _home) = runtime_and_record().await;

    let (ctx, _guard) = runtime
        .run_supervisor()
        .begin("wf-1", false)
        .expect("begin a workflow run");

    runtime
        .set_lifecycle(
            "running",
            crate::ports::types::Actor {
                kind: crate::ports::types::ActorKind::Operator,
                id: "operator".into(),
            },
        )
        .await
        .expect("resume the company");

    assert!(
        !ctx.cancel.is_cancelled(),
        "resuming must leave a live run alone"
    );
}

#[tokio::test]
async fn is_busy_sees_every_source_of_work() {
    let (runtime, _record, _home) = runtime_and_record().await;
    assert!(!runtime.is_busy(), "an idle runtime must not report busy");

    // 1. The cycle lock — the operator-chat case the steer registry misses.
    {
        let _cycle = runtime.serial.lock().await;
        assert!(
            runtime.is_busy(),
            "a turn holding the cycle lock must report busy"
        );
    }
    assert!(!runtime.is_busy(), "releasing the cycle lock must clear it");

    // 2. A workflow run — tracked in its own registry, invisible to both
    //    the cycle lock and the steer registry.
    {
        let (_ctx, _run) = runtime
            .run_supervisor()
            .begin("wf-1", false)
            .expect("begin a workflow run");
        assert!(runtime.is_busy(), "a live workflow run must report busy");
    }
    assert!(
        !runtime.is_busy(),
        "the run guard must clear it on drop, or the tenant never parks again"
    );

    // 3. A steerable in-flight run — the original signal, kept because a
    //    dispatched card can outlive the cycle that started it.
    {
        let _guard = runtime.steer().register(
            runtime.id(),
            crate::company::steer::InflightEntry {
                key: "run-1".to_string(),
                task_id: Some("run-1".to_string()),
                kind: crate::company::steer::InflightKind::Task,
                title: "Ship the thing".to_string(),
                agent_id: "ceo".to_string(),
                started_at_millis: 0,
                pending_action: None,
            },
        );
        assert!(runtime.is_busy(), "a registered steer run must report busy");
    }
    assert!(!runtime.is_busy(), "the steer guard must clear it on drop");
}

/// A poisoned run supervisor must make `is_busy` report **busy**.
///
/// The predicate's advertised invariant is that it fails closed, and #1133
/// only delivered that for two of its three sources: `steer.any_inflight`
/// was made poison-tolerant, but the `run_supervisor` arm still reached a
/// `.expect` through `len`. `GET /healthz/busy` has no `CatchPanicLayer`, so
/// that panic reset the connection, the manager read it as "cannot tell",
/// and its default is to park — losing the work the endpoint exists to
/// protect (issue #1239).
///
/// Outside any feature gate on purpose, matching
/// `is_busy_sees_every_source_of_work`: the run supervisor is wired on the
/// default build, and this must not be a test that only CI's `openhuman`
/// lane runs.
#[tokio::test]
async fn is_busy_fails_closed_on_a_poisoned_run_supervisor() {
    let (runtime, _record, _home) = runtime_and_record().await;
    assert!(!runtime.is_busy(), "an idle runtime must not report busy");

    runtime.run_supervisor().poison_for_test();

    assert!(
        runtime.is_busy(),
        "a poisoned run supervisor must report busy rather than panic in the handler"
    );
}

/// Codex review (#1865): "Plan first" on a bounced card is a fresh
/// attempt exactly like a re-dispatch, so the stale bounce chip must not
/// survive the To-do → Planning edge either.
///
/// No harness/planner wired — the default shape ~200 callers use — so
/// `plan_task`'s spawn is a no-op and this exercises only the synchronous
/// clearing `upsert_task` does before it, matching the inert-board
/// pattern `runtime::builder::test` already uses for the sibling
/// dispatch edge.
#[tokio::test]
async fn planning_first_clears_a_stale_bounce_chip_same_as_a_redispatch() {
    use crate::ports::tasks::{COLUMN_PLANNING, COLUMN_TODO, TaskRecord};

    let home = tempfile::tempdir().expect("tempdir");
    let manifest: crate::company::CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n")
            .expect("manifest");
    let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_id(crate::ports::types::CompanyId::new("acme"))
        .build()
        .await
        .expect("runtime");
    let runtime = std::sync::Arc::new(runtime);

    let card = TaskRecord {
        id: "card-1".to_string(),
        title: TaskTitle::authored("Draft the spec"),
        note: None,
        column: COLUMN_TODO.to_string(),
        priority: "medium".to_string(),
        assignee: "ceo".to_string(),
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
        // A stale chip from a dispatch attempt that already bounced.
        bounced: Some("a previous run's dispatch failed".to_string()),
    };
    runtime
        .upsert_task(&card)
        .await
        .expect("seed the bounced card in To-do");

    let mut planned = card.clone();
    planned.column = COLUMN_PLANNING.to_string();
    runtime
        .upsert_task(&planned)
        .await
        .expect("drag it into Planning");

    let after = runtime
        .tasks()
        .list(runtime.id())
        .await
        .expect("list")
        .into_iter()
        .find(|t| t.id == "card-1")
        .expect("card survives");
    assert_eq!(
        after.bounced, None,
        "entering Planning must clear the previous dispatch's bounce chip, not carry it \
         through to whatever the planning pass settles next"
    );
}

/// Codex review on PR #1883 (comment 3874654383): `patch_task` accepts
/// any board column on one write, so an operator can move a bounced
/// To-do card straight to `done` — a departure that touches neither the
/// dispatch nor the planning edge. The manual move supersedes the bounce
/// exactly as much as a re-dispatch does, and
/// [`crate::ports::tasks::TaskRecord::bounced`]'s own doc promises it
/// clears "the instant the card leaves `todo` any other way" — this
/// proves the "any other way" case, not just the two edge-fired ones the
/// sibling test above covers.
///
/// Before the fix, `upsert_task` only cleared `bounced` when
/// `dispatch || plan`, so this direct To-do → Done transition left the
/// stale chip in place — and it would have resurfaced if the card later
/// came back to To-do, naming a failure the intervening manual move had
/// already superseded.
#[tokio::test]
async fn a_direct_move_to_done_clears_a_stale_bounce_chip() {
    use crate::ports::tasks::{COLUMN_DONE, COLUMN_TODO, TaskRecord};

    let home = tempfile::tempdir().expect("tempdir");
    let manifest: crate::company::CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n")
            .expect("manifest");
    let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_id(crate::ports::types::CompanyId::new("acme"))
        .build()
        .await
        .expect("runtime");
    let runtime = std::sync::Arc::new(runtime);

    let card = TaskRecord {
        id: "card-2".to_string(),
        title: TaskTitle::authored("Draft the spec"),
        note: None,
        column: COLUMN_TODO.to_string(),
        priority: "medium".to_string(),
        assignee: "ceo".to_string(),
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
        // A stale chip from a dispatch attempt that already bounced.
        bounced: Some("a previous run's dispatch failed".to_string()),
    };
    runtime
        .upsert_task(&card)
        .await
        .expect("seed the bounced card in To-do");

    let mut done = card.clone();
    done.column = COLUMN_DONE.to_string();
    runtime
        .upsert_task(&done)
        .await
        .expect("drag it straight to Done");

    let after = runtime
        .tasks()
        .list(runtime.id())
        .await
        .expect("list")
        .into_iter()
        .find(|t| t.id == "card-2")
        .expect("card survives");
    assert_eq!(
        after.bounced, None,
        "a direct To-do → Done move must clear the stale bounce chip too — the operator's \
         manual transition supersedes the reason it named, and the chip must not resurface \
         if the card ever comes back to To-do"
    );
}
