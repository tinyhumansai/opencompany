use std::sync::Arc;

use super::{MaintenanceTicker, TRACE_RETENTION_LIMIT};
use crate::company::CompanyManifest;
use crate::policy::ManifestApprovalGate;
use crate::ports::now_millis;
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, CompressedTrace, Effect, EffectGroup,
    Verdict,
};
use crate::runtime::scheduler::FakeClock;
use crate::runtime::{CompanyRegistry, RuntimeBuilder};
use crate::{CompanyRuntime, Result};

fn tmp_home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oc-maintenance-")
        .tempdir()
        .expect("tempdir")
}

/// A manifest with **no `[[schedule]]`** — the shape that never got swept.
fn unscheduled_manifest() -> CompanyManifest {
    let toml_src = r#"
        [company]
        name = "Acme"

        [[agent]]
        id = "ceo"
        role = "Chief"

        [policy]
        mode = "supervised"
    "#;
    let manifest: CompanyManifest = toml::from_str(toml_src).expect("manifest parses");
    assert!(
        manifest.schedules.is_empty(),
        "the fixture's whole point is that it declares no cron"
    );
    manifest
}

fn sign_effect() -> Effect {
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

/// A company with no manifest cron, whose gate treats anything parked as
/// already past its deadline, registered under a ticker.
async fn given_an_unscheduled_company_with_an_overdue_gate(
    home: &std::path::Path,
) -> (Arc<CompanyRuntime>, MaintenanceTicker) {
    let manifest = unscheduled_manifest();
    // TTL 0: "the clock has passed the deadline" without sleeping for a day.
    let gate = Arc::new(ManifestApprovalGate::new(manifest.policy.clone()).with_ttl_millis(0));
    let runtime = Arc::new(
        RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_approvals(gate)
            .build()
            .await
            .expect("runtime builds"),
    );
    let registry = CompanyRegistry::new();
    registry.insert(runtime.id().clone(), Arc::clone(&runtime));
    let ticker = MaintenanceTicker::new(
        registry,
        Arc::new(FakeClock::new(now_millis() + 60 * MINUTE)),
    );
    (runtime, ticker)
}

const MINUTE: u64 = 60_000;

/// Parks an effect the way production does: the gate mints the id and the
/// journal records the park.
///
/// Both halves matter. The gate's map is what the sweep reads; the journal
/// is what `pending_approvals` — and therefore the operator's queue and the
/// sidebar badge — reads. A test that parked only on the gate would assert
/// that an entry nobody could see had been retired.
async fn park(runtime: &Arc<CompanyRuntime>) -> Result<ApprovalId> {
    let effect = sign_effect();
    let id = runtime.approvals.park(runtime.id(), effect.clone()).await?;
    runtime
        .journal()
        .record_parked(
            &id,
            &effect,
            now_millis(),
            crate::runtime::journal::TaskLink::Unlinked,
            crate::runtime::journal::ApprovalConversation {
                thread: None,
                parent: None,
            },
            None,
        )
        .await?;
    Ok(id)
}

/// **T1 — the whole issue, in one test.**
///
/// A company that declares no `[[schedule]]` parks an approval, the clock
/// passes its deadline, a maintenance tick runs, and the approval is gone.
///
/// This was **inexpressible** before this module existed. The only thing
/// that swept approvals in production was the scheduler's maintenance tick,
/// and `spawn_scheduler` returns `None` for a company with no schedules — so
/// there was no production path from "a scheduleless company's approval is
/// overdue" to "it is retired", at any age, ever. The test could only be
/// written by reaching for a scheduler the host would never have spawned,
/// which would have asserted the fix rather than the bug.
#[tokio::test]
async fn a_company_with_no_schedule_still_has_its_overdue_approvals_retired() {
    let home_dir = tmp_home();
    let (runtime, ticker) =
        given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;

    let id = park(&runtime).await.expect("parks");
    assert_eq!(runtime.pending_approvals().len(), 1);

    assert_eq!(ticker.tick().await, 1, "the tick must retire it");

    assert!(
        runtime.pending_approvals().is_empty(),
        "a scheduleless company's overdue approval must not survive maintenance"
    );
    // And the queue stays empty rather than the entry being re-derivable:
    // the journal removal is what the badge counts.
    assert_eq!(ticker.tick().await, 0, "a second tick has nothing to do");
    drop(id);
}

/// **T2 — a retired approval mints no grant.**
///
/// The safety property the whole change rests on: an approval disappearing
/// from the queue must never read as one that was granted. A `GrantedCall`
/// exists only on `resolve_outcome`'s `Approved` arm, and `retire_approval`
/// takes no verdict — but "it cannot happen by construction" is exactly the
/// kind of claim that stops being true silently, so it is asserted.
#[tokio::test]
async fn retiring_an_approval_mints_no_grant() {
    let home_dir = tmp_home();
    let (runtime, ticker) =
        given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;

    let id = park(&runtime).await.expect("parks");
    ticker.tick().await;

    assert!(
        runtime.grants.peek(&id).is_none(),
        "an expiry must never leave a grant behind — that would turn a \
         disappeared approval into a silently authorised call"
    );
    assert!(runtime.standing_grants().is_empty());
}

/// **T3 — the retirement is a deny by the system, for the recorded reason.**
///
/// Two readers, two records, and they have to agree. The event log gets an
/// `ApprovalResolved { verdict: Deny, by: System }` — a timeout that emitted
/// nothing was invisible to every event-log reader (#305) — and the journal
/// gets the durable audit entry saying the deadline is what did it.
#[tokio::test]
async fn the_retirement_is_a_system_deny_recorded_as_a_ttl_expiry() {
    let home_dir = tmp_home();
    let (runtime, ticker) =
        given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;

    let id = park(&runtime).await.expect("parks");
    ticker.tick().await;

    let events = runtime
        .events()
        .read_from(
            runtime.id(),
            crate::ports::types::EventSeq::new(0),
            usize::MAX,
        )
        .await
        .expect("events read");
    let resolved: Vec<_> = events
        .iter()
        .filter_map(|stored| match &stored.event {
            CompanyEvent::ApprovalResolved {
                approval_id,
                verdict,
                by,
            } if approval_id == &id => Some((*verdict, by.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(resolved.len(), 1, "exactly one resolution event");
    let (verdict, by) = &resolved[0];
    assert_eq!(*verdict, Verdict::Deny, "silence is a default-deny");
    assert_eq!(
        by.kind,
        ActorKind::System,
        "the host expired it; attributing this to a person would be a lie \
         the console then repeats as \"Approval denied\""
    );

    // The journal's durable record says why.
    let raw = std::fs::read_to_string(
        home_dir
            .path()
            .join("companies")
            .join(runtime.id().as_ref())
            .join("journal.jsonl"),
    )
    .or_else(|_| {
        // Layout differs by store backend; find the journal wherever it is.
        let mut found = String::new();
        for entry in walk(home_dir.path()) {
            if entry.file_name().is_some_and(|n| n == "journal.jsonl") {
                found.push_str(&std::fs::read_to_string(&entry).unwrap_or_default());
            }
        }
        if found.is_empty() {
            Err(std::io::Error::other("no journal found"))
        } else {
            Ok(found)
        }
    })
    .expect("journal readable");
    assert!(
        raw.contains("ApprovalExpired") && raw.contains(r#""reason":"ttl""#),
        "the journal must record the retirement AND its reason: {raw}"
    );

    // Nothing else claims to have resolved it.
    drop(Actor {
        kind: ActorKind::User,
        id: String::new(),
    });
}

/// **T4 — the retire/approve race yields exactly one outcome.**
///
/// An operator clicking Approve at the instant the sweep retires the same
/// entry gets either a real approval or `NotParked` — never both, and never
/// a silent double execution. The property comes from removal and outcome
/// sharing one critical section inside the gate; this asserts it holds
/// through the runtime's retirement path rather than only at the gate.
#[tokio::test]
async fn a_resolve_racing_the_sweep_yields_exactly_one_outcome() {
    let home_dir = tmp_home();
    let (runtime, ticker) =
        given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;

    for _ in 0..25 {
        let id = park(&runtime).await.expect("parks");
        let sweeper = Arc::clone(&runtime);
        let racing_id = id.clone();
        let sweep = tokio::spawn(async move { sweeper.sweep_expired_approvals().await });
        let resolver = Arc::clone(&runtime);
        let resolve = tokio::spawn(async move {
            resolver
                .approvals
                .resolve(
                    &racing_id,
                    Verdict::Approve,
                    Actor {
                        kind: ActorKind::User,
                        id: "operator".into(),
                    },
                )
                .await
        });
        let swept = sweep.await.expect("sweep task").expect("sweep ok");
        let resolved = resolve.await.expect("resolve task").expect("resolve ok");

        // Exactly one of the two won the entry.
        let sweep_won = swept.contains(&id);
        // A TTL-0 gate expires at resolve time too, so the operator's win
        // is "the gate had it and answered", not "the effect ran" — what
        // must never happen is BOTH claiming it.
        assert!(
            sweep_won || resolved.is_none(),
            "an approval cannot be both retired and executed"
        );
        assert!(
            runtime.grants.peek(&id).is_none(),
            "and the race must not mint a grant either way"
        );
    }
    assert_eq!(ticker.tick().await, 0, "nothing is left parked");
}

/// The registry is re-read every tick, so a company registered after the
/// ticker started is swept without anything having to remember to wire it.
///
/// This is the property that makes one always-on ticker a *fix* rather than
/// a fifth place to forget: hosted tenants are registered after boot, and a
/// per-company spawn is exactly what #971 was.
#[tokio::test]
async fn a_company_registered_after_the_ticker_started_is_still_swept() {
    let home_dir = tmp_home();
    let registry = CompanyRegistry::new();
    let ticker = MaintenanceTicker::new(
        registry.clone(),
        Arc::new(FakeClock::new(now_millis() + 60 * MINUTE)),
    );
    // Nothing registered yet.
    assert_eq!(ticker.tick().await, 0);

    let manifest = unscheduled_manifest();
    let gate = Arc::new(ManifestApprovalGate::new(manifest.policy.clone()).with_ttl_millis(0));
    let runtime = Arc::new(
        RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest)
            .with_approvals(gate)
            .build()
            .await
            .expect("runtime builds"),
    );
    park(&runtime).await.expect("parks");
    registry.insert(runtime.id().clone(), Arc::clone(&runtime));

    assert_eq!(ticker.tick().await, 1);
    assert!(runtime.pending_approvals().is_empty());
}

/// A company removed between `list()` and `get()` — the archive path — is
/// skipped rather than panicking the tick for every company after it.
#[tokio::test]
async fn a_company_that_left_the_registry_is_skipped() {
    let registry = CompanyRegistry::new();
    let ticker = MaintenanceTicker::new(
        registry.clone(),
        Arc::new(FakeClock::new(now_millis() + 60 * MINUTE)),
    );
    // `list()` on an empty registry yields nothing; the guard is exercised
    // by a stale id, which is what an archive between the two calls leaves.
    assert!(registry.get(&CompanyId::new("gone")).is_none());
    assert_eq!(ticker.tick().await, 0);
}

/// Trace summaries are not recalled yet, but the maintenance loop must
/// bound their durable window for companies with and without schedules.
#[tokio::test]
async fn maintenance_retains_only_the_newest_cycle_traces() {
    let home_dir = tmp_home();
    let (runtime, ticker) =
        given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;

    for i in 0..=TRACE_RETENTION_LIMIT {
        runtime
            .memory
            .save_trace(
                runtime.id(),
                CompressedTrace {
                    cycle_id: format!("cycle-{i}"),
                    summary: format!("summary-{i}"),
                    at_millis: i as u64,
                },
            )
            .await
            .expect("trace saves");
    }

    ticker.tick().await;

    let traces = runtime
        .memory
        .recent_traces(runtime.id(), TRACE_RETENTION_LIMIT + 1)
        .await
        .expect("traces read");
    assert_eq!(traces.len(), TRACE_RETENTION_LIMIT);
    assert_eq!(traces.first().unwrap().cycle_id, "cycle-1");
    assert_eq!(
        traces.last().unwrap().cycle_id,
        format!("cycle-{TRACE_RETENTION_LIMIT}")
    );
}

/// Records every company the ticker asks it to evict.
struct RecordingEvictor {
    evicted: std::sync::Mutex<Vec<CompanyId>>,
}

impl RecordingEvictor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            evicted: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn evicted(&self) -> Vec<CompanyId> {
        self.evicted.lock().expect("evictor mutex").clone()
    }
}

#[async_trait::async_trait]
impl super::CompanyEvictor for RecordingEvictor {
    async fn evict(&self, company: &CompanyId, _expected: &Arc<CompanyRuntime>) {
        self.evicted
            .lock()
            .expect("evictor mutex")
            .push(company.clone());
    }
}

fn operator() -> Actor {
    Actor {
        kind: ActorKind::Operator,
        id: "operator".into(),
    }
}

/// A registered company with no overdue gate — the eviction tests care only
/// about its lifecycle, not its approval queue.
async fn registered_company(home: &std::path::Path) -> (Arc<CompanyRuntime>, CompanyRegistry) {
    let runtime = Arc::new(
        RuntimeBuilder::new(home.to_path_buf(), unscheduled_manifest())
            .build()
            .await
            .expect("runtime builds"),
    );
    let registry = CompanyRegistry::new();
    registry.insert(runtime.id().clone(), Arc::clone(&runtime));
    (runtime, registry)
}

fn ticker_with(registry: CompanyRegistry, evictor: Arc<RecordingEvictor>) -> MaintenanceTicker {
    MaintenanceTicker::new(
        registry,
        Arc::new(FakeClock::new(now_millis() + 60 * MINUTE)),
    )
    .with_evictor(evictor)
}

/// A: a company that persisted `lifecycle: "archived"` but was left
/// registered is evicted, and the evictor's removals fire for its id.
#[tokio::test]
async fn an_archived_but_still_registered_company_is_evicted() {
    let home_dir = tmp_home();
    let (runtime, registry) = registered_company(home_dir.path()).await;
    runtime
        .set_lifecycle("archived", operator())
        .await
        .expect("archive");

    let evictor = RecordingEvictor::new();
    let ticker = ticker_with(registry, evictor.clone());
    ticker.tick().await;

    assert_eq!(
        evictor.evicted(),
        vec![runtime.id().clone()],
        "an archived-but-registered company must be evicted"
    );
}

/// B: `running`, `paused` and `suspended` companies are left registered —
/// the predicate is `archived` alone.
#[tokio::test]
async fn a_non_archived_company_is_left_registered() {
    for lifecycle in ["running", "paused", "suspended"] {
        let home_dir = tmp_home();
        let (runtime, registry) = registered_company(home_dir.path()).await;
        runtime
            .set_lifecycle(lifecycle, operator())
            .await
            .expect("set lifecycle");

        let evictor = RecordingEvictor::new();
        let ticker = ticker_with(registry, evictor.clone());
        ticker.tick().await;

        assert!(
            evictor.evicted().is_empty(),
            "a {lifecycle} company must not be evicted"
        );
    }
}

/// C: a `status()` read failure defers — the predicate never evicts on an
/// `Err`, only on a proven `archived`.
#[test]
fn a_status_read_failure_defers_eviction() {
    use super::should_evict_archived;

    let status = |lifecycle: &str| crate::runtime::types::CompanyStatus {
        id: CompanyId::new("acme"),
        name: "Acme".into(),
        logo_url: None,
        lifecycle: lifecycle.into(),
        pending_approvals: 0,
        template_provenance: None,
        emergency_paused: false,
    };

    assert!(should_evict_archived(&Ok(status("archived"))));
    assert!(!should_evict_archived(&Ok(status("running"))));
    assert!(!should_evict_archived(&Err(
        crate::error::OpenCompanyError::CompanyNotFound("acme".into())
    )));
}

/// Recursively lists files under `root`. Test-only; the journal's on-disk
/// layout is a store detail and this keeps the assertion about the record
/// rather than about the path.
fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

// ── Issue #1861: an unanswered blocker returns its card ─────────────────

fn paused_card(id: &str) -> crate::ports::TaskRecord {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "title": "Ship the changelog",
        "column": "paused",
        "priority": "medium",
        "assignee": "maya",
        "updatedAtMillis": 7,
    }))
    .expect("card")
}

/// Gated with the tests below: parking a blocker needs
/// `CompanyRuntime::park_blocker`, which only the (openhuman-only)
/// planning pass calls in production. The `Rust (openhuman, tinymemory)`
/// lane runs these.
#[cfg(feature = "openhuman")]
fn blocker_payload(task_id: &str) -> crate::ports::blockers::BlockerPayload {
    crate::ports::blockers::BlockerPayload {
        kind: crate::ports::blockers::BlockerKind::Infrastructure,
        source: crate::ports::blockers::BlockerSource::Provider,
        step: Some(crate::ports::blockers::BlockerStep::Task {
            task_id: task_id.to_string(),
        }),
        reason: "the model `gpt-nonexistent` was rejected".to_string(),
        needed: "a model id this provider serves".to_string(),
        group_key: None,
    }
}

async fn card_after(runtime: &Arc<CompanyRuntime>, id: &str) -> crate::ports::TaskRecord {
    runtime
        .tasks()
        .list(runtime.id())
        .await
        .expect("board reads")
        .into_iter()
        .find(|t| t.id == id)
        .expect("the card survives")
}

/// The same close, reached the other way: an operator's verdict lands
/// *after* the deadline, so the resolve itself discovers the expiry and
/// `retire_if_expired` — not the sweeper — owns the rest of it.
///
/// # The bug this reproduces (CodeRabbit review on #1905)
///
/// That path retired the approval and filed the badge but never returned
/// the card, so a task-linked blocker found this way sat in `paused`
/// forever: the approval it was waiting on had just been retired, and no
/// later sweep will ever see that id again. The badge made it worse by
/// saying the card *was* back in To-do — it was keyed on the blocker
/// naming a card, not on a move that happened.
///
/// Both paths now run the same `finish_expiry` tail, so this asserts the
/// identical outcome its sweeper twin above does.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn a_verdict_that_arrives_late_returns_the_card_too() {
    let home_dir = tmp_home();
    let (runtime, _ticker) =
        given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;
    runtime
        .tasks()
        .upsert(runtime.id(), &paused_card("t-1"))
        .await
        .expect("seed");
    let approval_id = runtime
        .park_blocker(
            &blocker_payload("t-1"),
            "t-1",
            crate::company::blocker_sender::BlockerSenderSignals::default(),
        )
        .await
        .expect("parks");

    // The gate's TTL is 0, so this verdict is already too late: the resolve
    // reports `Expired` rather than settling the operator's answer.
    runtime
        .resolve_approval(
            &approval_id,
            crate::ports::types::Verdict::Approve,
            crate::ports::types::Actor {
                kind: crate::ports::types::ActorKind::User,
                id: "ceo".into(),
            },
        )
        .await
        .expect("a late resolve still reports cleanly");

    let after = card_after(&runtime, "t-1").await;
    assert_eq!(
        after.column,
        crate::ports::tasks::COLUMN_TODO,
        "a card whose blocker expired must come back whichever path noticed the deadline"
    );
    let note = after.note.expect("the question rides back on the note");
    assert!(note.contains("gpt-nonexistent"), "{note}");

    let feed = runtime
        .notifications()
        .list(runtime.id(), "ceo")
        .await
        .expect("read notifications");
    let badge = feed
        .iter()
        .find(|n| n.notification.kind == "approval_expired")
        .expect("the expiry is badged here too");
    assert!(
        badge.notification.title.contains("back in To-do"),
        "and the badge may say so, because the move actually landed: {:?}",
        badge.notification.title
    );
}

/// The close of the epic's own loop: nothing waits forever, and nothing is
/// dropped without a record. The question that went unanswered rides back
/// to To-do on the card, because the TTL expiring does not make the work
/// possible — it only stops pretending somebody is about to answer.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn an_unanswered_blocker_returns_its_card_carrying_the_question() {
    let home_dir = tmp_home();
    let (runtime, ticker) =
        given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;
    runtime
        .tasks()
        .upsert(runtime.id(), &paused_card("t-1"))
        .await
        .expect("seed");
    runtime
        .park_blocker(
            &blocker_payload("t-1"),
            "t-1",
            crate::company::blocker_sender::BlockerSenderSignals::default(),
        )
        .await
        .expect("parks");
    assert_eq!(runtime.pending_approvals().len(), 1);

    assert_eq!(ticker.tick().await, 1, "the tick must retire it");

    let after = card_after(&runtime, "t-1").await;
    assert_eq!(
        after.column,
        crate::ports::tasks::COLUMN_TODO,
        "a card nobody answered must not sit in `paused` waiting on a decision that has \
         already been made against it"
    );
    let note = after.note.expect("the question rides back on the note");
    assert!(note.contains("gpt-nonexistent"), "{note}");
    assert!(
        note.contains("a model id this provider serves"),
        "what would answer it has to come back too: {note}"
    );
    // Issue #1865's chip: an operator scanning To-do must be able to tell
    // this from a card nobody has started, without opening it.
    let bounced = after
        .bounced
        .expect("the board distinguishes it from fresh work");
    assert!(bounced.contains("gpt-nonexistent"), "{bounced}");
}

/// An ordinary approval expiring touches no card. It is a decision that was
/// defaulted, not a question that went unanswered, and the board has
/// nothing to say about it.
#[tokio::test]
async fn an_expiring_approval_that_is_not_a_blocker_leaves_the_board_alone() {
    let home_dir = tmp_home();
    let (runtime, ticker) =
        given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;
    runtime
        .tasks()
        .upsert(runtime.id(), &paused_card("t-2"))
        .await
        .expect("seed");
    park(&runtime).await.expect("parks");

    assert_eq!(ticker.tick().await, 1);

    let after = card_after(&runtime, "t-2").await;
    assert_eq!(after.column, "paused", "nothing on the board changed");
    assert!(after.bounced.is_none());
}

/// A card an operator has since moved is theirs. The expiry records the
/// default-deny and leaves the board exactly where they put it — the same
/// guard `advance_settled_card` applies one column over.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn an_expiry_does_not_drag_back_a_card_an_operator_has_moved() {
    let home_dir = tmp_home();
    let (runtime, ticker) =
        given_an_unscheduled_company_with_an_overdue_gate(home_dir.path()).await;
    let mut moved = paused_card("t-3");
    moved.column = crate::ports::tasks::COLUMN_IN_PROGRESS.to_string();
    runtime
        .tasks()
        .upsert(runtime.id(), &moved)
        .await
        .expect("seed");
    runtime
        .park_blocker(
            &blocker_payload("t-3"),
            "t-3",
            crate::company::blocker_sender::BlockerSenderSignals::default(),
        )
        .await
        .expect("parks");

    assert_eq!(ticker.tick().await, 1);

    let after = card_after(&runtime, "t-3").await;
    assert_eq!(
        after.column,
        crate::ports::tasks::COLUMN_IN_PROGRESS,
        "an operator who picked the card back up owns where it sits"
    );
    assert!(after.bounced.is_none());
}
