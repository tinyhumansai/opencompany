use super::tests_capped_halt::{GREET, deps, record, tools_record};
use super::tests_delivery_gate::REPORT_TO_DESK;
use super::tests_sub_workflow::{GATED, THREE_GATES, deps_with_parking};
use super::*;

use crate::company::parse_workflow;
use crate::store::FsOps;

/// Issue #1825 (P1, found by chatgpt-codex-connector): "Preserve a
/// completed batch when a later park fails."
///
/// # The race this closes
///
/// `park_pending_gates` parks a run's gates one at a time, and each
/// successful `park_and_journal` arms `ContinuationQueue` for the run's
/// shared turn key. An operator can resolve an EARLIER card while a
/// LATER card in this loop is still being attempted; if that later park
/// then fails, `park_and_journal`'s own error branch releases the slot it
/// armed for it. Pre-fix, nothing held the counter open across the loop,
/// so that release could itself be the batch's last decrement — and the
/// batch it got back (every sibling decided while this loop was still
/// running) was dropped inside that failure branch: no caller was left to
/// route it anywhere, and `ContinuationQueue::decide` discards the turn's
/// whole banked state, the already-approved sibling's event included, the
/// moment `outstanding` hits zero.
///
/// # How this is reproduced deterministically
///
/// Three gates share one turn. `RaceThenFail` wraps the approval gate
/// `park_pending_gates` parks through: its SECOND `park()` call first
/// decides the FIRST card via the SAME `ContinuationQueue` handle
/// `park_and_journal` arms — simulating a fast operator racing the loop —
/// then fails outright, exactly like a real `park()`/`record_parked()`
/// fault. The THIRD gate parks normally, on the same principle as
/// `approving_the_first_card_of_a_multi_call_node_does_not_complete_the_batch_early`
/// in `workflows::caps::mod`. If the first card's decision survived the
/// second card's faulted park, deciding the third (simulating the
/// operator's next click) must hand back BOTH events; if it was dropped,
/// only the third's.
#[tokio::test]
async fn a_batch_completed_by_a_failed_park_is_not_silently_dropped() {
    use crate::ports::approvals::ApprovalGate;
    use crate::ports::types::{Actor, ActorKind, ApprovalId, Effect, PolicyDecision, Verdict};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex as AsyncMutex;

    /// Delegates every call to `inner`, except that the SECOND `park` it
    /// sees first decides the FIRST approval it minted — via the same
    /// `ContinuationQueue` the real park path arms — then fails,
    /// simulating an operator racing ahead of `park_pending_gates`'s own
    /// loop into a park that then errors.
    struct RaceThenFail {
        inner: Arc<dyn ApprovalGate>,
        continuations: crate::runtime::continuation::ContinuationQueue,
        turn: String,
        calls: AtomicUsize,
        first_approval: AsyncMutex<Option<ApprovalId>>,
        third_approval: AsyncMutex<Option<ApprovalId>>,
        /// What `ContinuationQueue::decide` returned for the interleaved
        /// decision on the first card — the assertion this test exists
        /// for. Outer `Option`: whether the interleave actually ran.
        early_decide_result: AsyncMutex<Option<Option<Vec<CompanyEvent>>>>,
    }

    #[async_trait]
    impl ApprovalGate for RaceThenFail {
        async fn evaluate(
            &self,
            company: &CompanyId,
            effect: &Effect,
        ) -> crate::Result<PolicyDecision> {
            self.inner.evaluate(company, effect).await
        }

        async fn park(&self, company: &CompanyId, effect: Effect) -> crate::Result<ApprovalId> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            match call {
                0 => {
                    let id = self.inner.park(company, effect).await?;
                    *self.first_approval.lock().await = Some(id.clone());
                    Ok(id)
                }
                1 => {
                    let first = self
                        .first_approval
                        .lock()
                        .await
                        .clone()
                        .expect("the first card must have parked before the second");
                    let event = CompanyEvent::ApprovalResolved {
                        approval_id: first,
                        verdict: Verdict::Approve,
                        by: Actor {
                            kind: ActorKind::Operator,
                            id: "operator".to_string(),
                        },
                    };
                    let result = self.continuations.decide(&self.turn, Some(event));
                    *self.early_decide_result.lock().await = Some(result);
                    Err(OpenCompanyError::InvalidRequest(
                        "simulated park fault".to_string(),
                    ))
                }
                2 => {
                    let id = self.inner.park(company, effect).await?;
                    *self.third_approval.lock().await = Some(id.clone());
                    Ok(id)
                }
                other => panic!("unexpected park call #{other}"),
            }
        }

        async fn resolve(
            &self,
            id: &ApprovalId,
            verdict: Verdict,
            by: Actor,
        ) -> crate::Result<Option<Effect>> {
            self.inner.resolve(id, verdict, by).await
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (mut deps, _journal) = deps_with_parking(dir.path());
    let file = parse_workflow(THREE_GATES).expect("parses");

    let ctx = WorkflowRunContext::new(false);
    let turn = crate::runtime::workflow_resume::workflow_turn_key(&ctx.run_id);

    let parking = deps
        .delivery
        .as_ref()
        .and_then(|d| d.parking.clone())
        .expect("deps_with_parking wires parking");
    let race = Arc::new(RaceThenFail {
        inner: parking.approvals.clone(),
        continuations: parking.continuations.clone(),
        turn: turn.clone(),
        calls: AtomicUsize::new(0),
        first_approval: AsyncMutex::new(None),
        third_approval: AsyncMutex::new(None),
        early_decide_result: AsyncMutex::new(None),
    });
    deps.delivery
        .as_mut()
        .unwrap()
        .parking
        .as_mut()
        .unwrap()
        .approvals = race.clone();

    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps,
        &tools_record(),
        &file,
        serde_json::json!({ "request": "quarterly numbers" }),
        &ctx,
    )
    .await
    .expect("run pauses cleanly even though one gate's park faulted");

    assert_eq!(
        run.pending_approvals.len(),
        3,
        "the engine pauses on all three gates regardless of parking outcome: {:?}",
        run.pending_approvals
    );

    assert_eq!(
        race.early_decide_result.lock().await.clone(),
        Some(None),
        "an earlier card's decision must not complete the batch while a later card is \
         still being attempted"
    );

    let third = race
        .third_approval
        .lock()
        .await
        .clone()
        .expect("the third gate must have parked cleanly");
    let final_event = CompanyEvent::ApprovalResolved {
        approval_id: third,
        verdict: Verdict::Approve,
        by: Actor {
            kind: ActorKind::Operator,
            id: "operator".to_string(),
        },
    };
    let batch = parking
        .continuations
        .decide(&turn, Some(final_event))
        .expect("deciding the last outstanding card must release the batch");
    assert_eq!(
        batch.len(),
        2,
        "the first card's decision, banked while the faulted second park was still in \
         flight, must still be in the batch the last decision releases — not dropped by \
         the faulted park's own slot release: {batch:?}"
    );
}

/// A run an operator stopped parks nothing. They are not asking to be asked
/// about gates the run never reached, and `cancelled_run` reports no pending
/// approvals for the same reason.
#[tokio::test]
async fn a_cancelled_run_parks_no_gates() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, journal) = deps_with_parking(dir.path());
    let file = parse_workflow(GATED).expect("parses");
    let ctx = WorkflowRunContext::new(false);
    ctx.cancel.cancel();

    let run = run_workflow(
        Arc::new(HarnessPool::new()),
        deps,
        &tools_record(),
        &file,
        serde_json::json!({ "request": "x" }),
        &ctx,
    )
    .await
    .expect("a cancelled run is Ok");
    assert!(run.cancelled);
    assert!(journal.pending().is_empty());
}

/// An already-cancelled run must report `cancelled` **every** time, not most
/// of the time.
///
/// `tokio::select!` polls its branches in random order and picks among those
/// ready. On a token that is already cancelled, the `cancelled()` arm is
/// ready on the very first poll — so if the engine future is also ready on
/// that poll, which arm wins is a coin flip, and the losing half settles as a
/// completed run with `cancelled: false`. An operator's stop was reported as
/// a completion.
///
/// Both `select!` sites are `biased;` with the cancel arm first, which makes
/// an already-signalled cancellation win deterministically.
///
/// This runs the path repeatedly on purpose. A single iteration reproduced
/// the unbiased defect only about half the time — which is why it read as a
/// flaky test for as long as it did, and why anyone who re-ran it in
/// isolation concluded it was not real. At this iteration count a revert to
/// the unbiased form fails here essentially every time.
#[tokio::test]
async fn an_already_cancelled_run_always_reports_cancelled() {
    for iteration in 0..16 {
        let dir = tempfile::tempdir().unwrap();
        let (deps, _journal) = deps_with_parking(dir.path());
        let file = parse_workflow(GATED).expect("parses");
        let ctx = WorkflowRunContext::new(false);
        ctx.cancel.cancel();

        let run = run_workflow(
            Arc::new(HarnessPool::new()),
            deps,
            &tools_record(),
            &file,
            serde_json::json!({ "request": "x" }),
            &ctx,
        )
        .await
        .expect("a cancelled run is Ok");

        assert!(
            run.cancelled,
            "iteration {iteration}: a run cancelled before it started reported \
             itself as not cancelled — the cancel arm lost the select race"
        );
    }
}

/// A graph with no gate parks nothing — the addition must be invisible to
/// every run that was already working.
#[tokio::test]
async fn a_run_that_pauses_on_nothing_parks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, journal) = deps_with_parking(dir.path());
    let file = parse_workflow(GREET).expect("parses");
    let rec = record();
    let pool = Arc::new(HarnessPool::new());
    pool.ensure(&rec, &deps).await.expect("roster builds");

    let run = run_workflow(
        pool,
        deps,
        &rec,
        &file,
        Value::Null,
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("run completes");
    assert!(run.pending_approvals.is_empty());
    assert!(journal.pending().is_empty());
}

// --- #438: a report is delivered once per approval lineage ---------------

/// A graph that **delivers before it pauses**: the report goes out, then the
/// gate stops the run. This is the shape that made approving a gate mail the
/// same person twice, and it is not exotic — "summarise, send it, then ask
/// me before doing the irreversible thing" is an ordinary workflow.
const DELIVER_THEN_GATE: &str = r#"
id = "lineage"
name = "Lineage"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "summary"
kind = "output"
name = "Owner summary"
[node.destination]
kind = "owner"
[[node]]
id = "gate"
kind = "output"
name = "Gate"
requires_approval = true
[[edge]]
from = "start"
to = "summary"
[[edge]]
from = "summary"
to = "gate"
"#;

/// Deps with a real approvals queue **and** a counting mail sender, plus an
/// active admin so an `owner` destination resolves to a real address.
///
/// The mail sender is the instrument the whole test rests on: the claim is
/// not "the row says skipped", it is "the transport was called exactly
/// once across the entire lineage".
async fn deps_with_parking_and_mail(
    dir: &std::path::Path,
) -> (
    HarnessDeps,
    Arc<crate::runtime::journal::RuntimeJournal>,
    crate::server::ops::mailer::RecordingMailSender,
) {
    use crate::ports::{UserRecord, UserRole, UserStatus, UserStore};

    let users = Arc::new(FsOps::new(dir));
    users
        .upsert_user(
            &CompanyId::new("acme"),
            &UserRecord {
                id: "u1".to_string(),
                email: "ada@acme.test".to_string(),
                display_name: None,
                avatar: None,
                role: UserRole::Admin,
                status: UserStatus::Active,
                password_hash: None,
                must_change_password: false,
                created_at_millis: 1,
                last_seen_at_millis: None,
                updated_at_millis: 1,
            },
        )
        .await
        .expect("admin upserted");

    let mail = crate::server::ops::mailer::RecordingMailSender::new();
    let policy = toml::from_str("mode = \"full\"\n").expect("valid [policy] block");
    let journal = Arc::new(crate::runtime::journal::RuntimeJournal::new(
        dir.join("journal.jsonl"),
    ));
    let mut deps = deps(dir);
    deps.delivery = Some(super::super::delivery::WorkflowDeliveryDeps {
        mail: Some(crate::company::runtime::CompanyMail {
            sender: Arc::new(mail.clone()),
            smtp: crate::server::ops::smtp::SmtpCredentials {
                host: "smtp.example.test".into(),
                port: 587,
                security: crate::server::ops::smtp::SmtpSecurity::Starttls,
                username: "acme".into(),
                password: crate::ports::types::SecretValue("hunter2".into()),
                from_name: "Acme".into(),
                from_email: "acme@opencompany.test".into(),
            },
        }),
        inbox: Arc::new(crate::store::FsInboxStore::new(dir)),
        users,
        bootstrap_admin: None,
        channels: Vec::new(),
        notifications: None,
        parking: Some(super::super::delivery::DeliveryParking {
            approvals: Arc::new(crate::policy::ManifestApprovalGate::new(policy)),
            journal: journal.clone(),
            // Issue #978: a test fixture parks into its own queues. The
            // production wiring is `RuntimeBuilder`, which hands the
            // runtime's own handles in so a park arms what the resolve
            // path releases.
            continuations: Default::default(),
            gates: Default::default(),
            blocked_nodes: Default::default(),
            grants: Default::default(),
            events: Arc::new(crate::store::FsEventLog::new(dir)),
        }),
        events: Arc::new(crate::store::FsEventLog::new(dir)),
    });
    (deps, journal, mail)
}

/// **The headline regression for issue #438.** Across a whole lineage — the
/// run that paused plus the continuation an approval starts — the report is
/// delivered exactly **once**.
///
/// Counted at the transport, not at the row: a `skipped` row proves the
/// bookkeeping, and only the send count proves nobody's inbox was touched
/// twice. Before the fix this test reads `2` on the last assertion.
///
/// The continuation is built by
/// [`continuation_input`](crate::runtime::workflow_resume::continuation_input)
/// — the same function the Approvals path calls — rather than assembled
/// here, so what is proven is the production path and not a lookalike. The
/// approvals plumbing on either side of it (the card is parked, approving
/// resolves it and spawns) is pinned by `workflow_resume`'s own suite.
#[tokio::test]
async fn a_report_is_delivered_once_across_a_gate_and_its_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, journal, mail) = deps_with_parking_and_mail(dir.path()).await;
    let file = parse_workflow(DELIVER_THEN_GATE).expect("parses");

    // --- run 1: the report goes out, then the run pauses on the gate.
    let first = run_workflow(
        Arc::new(HarnessPool::new()),
        deps.clone(),
        &record(),
        &file,
        serde_json::json!({ "request": "quarterly numbers" }),
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("run pauses cleanly");

    assert!(
        first.pending_approvals.iter().any(|id| id == "gate"),
        "the run must pause on the gate: {:?}",
        first.pending_approvals
    );
    assert_eq!(first.deliveries.len(), 1, "{:?}", first.deliveries);
    assert_eq!(
        first.deliveries[0].status,
        crate::ports::DeliveryStatus::Sent
    );
    assert_eq!(mail.sent().len(), 1, "run 1 sends the report once");

    // --- the operator approves: the card becomes a continuation input.
    let card = journal
        .pending()
        .into_iter()
        .find(|p| p.effect.kind == crate::runtime::WORKFLOW_APPROVE_KIND)
        .expect("the paused gate is waiting on the operator")
        .effect;
    let continuation = crate::runtime::workflow_resume::continuation_input(
        &card,
        &[
            card.payload[crate::runtime::workflow_resume::PAYLOAD_NODE_ID]
                .as_str()
                .expect("the card names its gate")
                .to_string(),
        ],
        &[],
    )
    .expect("a well-formed card continues");

    // --- run 2: the same graph, from the trigger, with the gate approved.
    let second = run_workflow(
        Arc::new(HarnessPool::new()),
        deps,
        &record(),
        &file,
        continuation,
        &WorkflowRunContext::new(false),
    )
    .await
    .expect("the continuation runs");

    // The whole point, in one number — asserted FIRST, so a regression
    // fails on the send count itself rather than on the bookkeeping that
    // describes it.
    assert_eq!(
        mail.sent().len(),
        1,
        "one report, one send, across the whole lineage: {:?}",
        mail.sent()
    );

    assert!(
        second.pending_approvals.is_empty(),
        "the approved gate must not pause again: {:?}",
        second.pending_approvals
    );
    assert_eq!(second.deliveries.len(), 1, "{:?}", second.deliveries);
    assert_eq!(
        second.deliveries[0].status,
        crate::ports::DeliveryStatus::Skipped
    );
    assert_eq!(
        second.deliveries[0].reason,
        crate::ports::DeliveryReason::AlreadyDelivered
    );
}

// --- issue #529: the durable delivery ledger across a crash --------------

/// Deps that deliver a report to the operator channel, sharing an event log
/// and a channel across runs — so a run 1's write-behind record and the
/// count of sends are both visible to run 2.
pub(super) fn deps_delivering_to_channel(
    dir: &std::path::Path,
    events: Arc<dyn crate::ports::EventLog>,
    channel: crate::runtime::channel::RecordingChannel,
    consult_journal: bool,
) -> HarnessDeps {
    let mut deps = deps(dir);
    // `consult_journal` toggles ONLY whether the runner reads the durable
    // ledger — the delivery bundle always journals write-behind to the same
    // log, so the journal state is identical between the two. This is what
    // makes the negative control a true control: same journal, guard off.
    deps.events = consult_journal.then(|| events.clone());
    deps.delivery = Some(crate::workflows::WorkflowDeliveryDeps {
        mail: None,
        inbox: Arc::new(crate::store::FsInboxStore::new(dir)),
        users: Arc::new(FsOps::new(dir)),
        bootstrap_admin: None,
        channels: vec![Arc::new(channel)],
        notifications: None,
        parking: None,
        events,
    });
    deps
}

/// **The headline regression for issue #529.** A run delivers a report and
/// then crashes — `run_workflow` returns, but the caller never journals the
/// finish (exactly what a host kill leaves). The boot sweep settles it with
/// the synthetic interrupted finish, and an operator re-runs the workflow.
/// The report must NOT go out a second time: the transport count stays at 1
/// and the re-run's row reads `Skipped` / `AlreadyDelivered`.
///
/// Breaking the union/fold (the durable consult in `run_workflow_inner`)
/// turns this into the negative control below — the count becomes 2 and this
/// assertion fails, which is what makes the guard provably load-bearing.
#[tokio::test]
async fn a_crashed_runs_delivery_is_not_repeated_on_an_independent_re_run() {
    use crate::runtime::channel::RecordingChannel;

    let dir = tempfile::tempdir().unwrap();
    let events: Arc<dyn crate::ports::EventLog> =
        Arc::new(crate::store::FsEventLog::new(dir.path()));
    let channel = RecordingChannel::new("engineering");
    let rec = record();
    let file = parse_workflow(REPORT_TO_DESK).expect("parses");

    // Run 1 delivers, then "crashes": run_workflow returns, but nothing
    // journals a WorkflowRunFinished for it.
    let deps1 = deps_delivering_to_channel(dir.path(), events.clone(), channel.clone(), true);
    let ctx1 = WorkflowRunContext::new(false);
    let run1 = run_workflow(
        Arc::new(HarnessPool::new()),
        deps1,
        &rec,
        &file,
        serde_json::json!({ "brief": "quarterly numbers" }),
        &ctx1,
    )
    .await
    .expect("run 1 runs");
    assert_eq!(
        run1.deliveries[0].status,
        crate::ports::DeliveryStatus::Sent
    );
    assert_eq!(channel.sent().len(), 1, "run 1 delivered exactly once");

    // The boot sweep settles the crashed run with the synthetic interrupted
    // finish — an error, which must NOT clear the durable ledger.
    crate::runtime::sweep_interrupted_runs(&events, &rec.id).await;

    // Run 2 is an independent re-run of the same workflow.
    let deps2 = deps_delivering_to_channel(dir.path(), events.clone(), channel.clone(), true);
    let ctx2 = WorkflowRunContext::new(false);
    let run2 = run_workflow(
        Arc::new(HarnessPool::new()),
        deps2,
        &rec,
        &file,
        serde_json::json!({ "brief": "quarterly numbers" }),
        &ctx2,
    )
    .await
    .expect("run 2 runs");

    assert_eq!(
        channel.sent().len(),
        1,
        "the durable ledger stopped the crashed run's report from being re-delivered"
    );
    assert_eq!(run2.deliveries.len(), 1, "{:?}", run2.deliveries);
    assert_eq!(
        run2.deliveries[0].status,
        crate::ports::DeliveryStatus::Skipped
    );
    assert_eq!(
        run2.deliveries[0].reason,
        crate::ports::DeliveryReason::AlreadyDelivered
    );
}
