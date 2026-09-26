use super::*;
use crate::AppConfig;
#[cfg(feature = "openhuman")]
use crate::ports::tasks::TaskTitle;
use crate::ports::types::{EventSeq, StoredEvent};
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin};
use crate::server::router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::operator_test_support_1::*;
use super::operator_test_support_2::*;

#[async_trait::async_trait]
impl crate::ports::brain::Brain for StalledChatBrain {
    async fn run_cycle(
        &self,
        req: crate::ports::types::CycleRequest,
        _host: &dyn crate::ports::brain::CycleHost,
    ) -> crate::Result<crate::ports::types::CycleResult> {
        let mut channel_responses = Vec::new();
        for event in &req.events {
            if matches!(event, CompanyEvent::OperatorMessage { .. }) {
                self.entered.notify_one();
                self.release.notified().await;
                channel_responses.push(crate::ports::types::OutboundMessage {
                    message_id: None,
                    task_id: None,
                    outputs: Vec::new(),
                    channel: "operator".into(),
                    agent: None,
                    text: SLOW_TURN_REPLY.into(),
                    steps: Vec::new(),
                    reply_to: None,
                    mentions: Vec::new(),
                });
            }
        }
        Ok(crate::ports::types::CycleResult {
            channel_responses,
            new_traces: vec![crate::ports::types::CompressedTrace::now(
                &req.cycle_id,
                "stalled chat",
            )],
            ledger_deltas: Vec::new(),
            token_usage: crate::ports::types::TokenUsage::default(),
        })
    }
}

/// Whether the turn's answer reached the durable journal.
pub(super) async fn reply_journaled(runtime: &Arc<CompanyRuntime>) -> bool {
    runtime
        .events()
        .read_from(runtime.id(), EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .iter()
        .any(|stored| {
            matches!(
                &stored.event,
                CompanyEvent::AgentReply { text, .. } if text == SLOW_TURN_REPLY
            )
        })
}

/// Waits for the released turn to journal its reply.
pub(super) async fn await_reply_journaled(runtime: &Arc<CompanyRuntime>) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !reply_journaled(runtime).await {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

// ── Issue #983: an accepted turn exists and can be read back ────────────

/// A brain that blocks every operator turn on a semaphore the test holds.
///
/// Deliberately a `Semaphore` rather than a `Notify`: these tests run two
/// turns at once and release both, and `notify_one` wakes exactly one
/// waiter while `notify_waiters` wakes only those already parked. Permits
/// are held whether or not anybody is waiting yet, so the release cannot
/// race the turns into a hang.
pub(super) struct BlockingChatBrain {
    /// One permit added per turn that has entered the brain.
    entered: Arc<tokio::sync::Semaphore>,
    /// The test's permission for a turn to finish — one permit each.
    release: Arc<tokio::sync::Semaphore>,
}

impl BlockingChatBrain {
    pub(super) fn new() -> (
        Arc<Self>,
        Arc<tokio::sync::Semaphore>,
        Arc<tokio::sync::Semaphore>,
    ) {
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        (
            Arc::new(Self {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }),
            entered,
            release,
        )
    }
}

#[async_trait::async_trait]
impl crate::ports::brain::Brain for BlockingChatBrain {
    async fn run_cycle(
        &self,
        req: crate::ports::types::CycleRequest,
        _host: &dyn crate::ports::brain::CycleHost,
    ) -> crate::Result<crate::ports::types::CycleResult> {
        let mut channel_responses = Vec::new();
        for event in &req.events {
            if let CompanyEvent::OperatorMessage { text, .. } = event {
                self.entered.add_permits(1);
                self.release.acquire().await.expect("released").forget();
                channel_responses.push(crate::ports::types::OutboundMessage {
                    message_id: None,
                    task_id: None,
                    outputs: Vec::new(),
                    channel: "operator".into(),
                    agent: None,
                    text: format!("answered: {text}"),
                    steps: Vec::new(),
                    reply_to: None,
                    mentions: Vec::new(),
                });
            }
        }
        Ok(crate::ports::types::CycleResult {
            channel_responses,
            new_traces: vec![crate::ports::types::CompressedTrace::now(
                &req.cycle_id,
                "blocking chat",
            )],
            ledger_deltas: Vec::new(),
            token_usage: crate::ports::types::TokenUsage::default(),
        })
    }
}

/// Polls `f` until it holds, or fails the test.
pub(super) async fn until(label: &str, mut f: impl AsyncFnMut() -> bool) {
    let ok = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !f().await {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    assert!(ok, "{label}");
}

/// The operator messages `chat/history` currently shows for the main desk.
pub(super) async fn history_texts(app: &axum::Router, desk: &str) -> Vec<String> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/company/chat/history?desk={desk}"))
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    body.as_array()
        .expect("the history route answers with an array")
        .iter()
        .filter_map(|m| m["text"].as_str().map(str::to_string))
        .collect()
}

/// The company's turn rows, id → status.
pub(super) async fn turn_rows(runtime: &Arc<CompanyRuntime>) -> Vec<(String, String)> {
    let mut rows = runtime
        .runs()
        .list_runs(runtime.id(), &crate::ports::runs::RunFilter::default())
        .await
        .unwrap();
    rows.sort_by_key(|r| r.created_at_millis);
    rows.into_iter()
        .map(|r| (r.id, r.status.to_string()))
        .collect()
}

// ---- issue #66: the operator attention SSE feed ----
pub(super) fn stored(event: CompanyEvent) -> StoredEvent {
    StoredEvent {
        seq: EventSeq::new(7),
        company: CompanyId::new("acme"),
        event,
        at_millis: 1_700_000_000_000,
    }
}

// ---- issue #228: the workflow-run outcome projection ----
pub(super) fn delivery_row(
    node: &str,
    status: crate::ports::DeliveryStatus,
) -> crate::ports::DeliveryReport {
    crate::ports::DeliveryReport {
        node: node.into(),
        kind: "email".into(),
        target: Some("ada@example.com".into()),
        status,
        detail: "this recipient has never written to the company".into(),
        reason: crate::ports::DeliveryReason::RecipientNotEstablished,
    }
}

pub(super) fn racing_standing_grant(id: &str) -> crate::runtime::grants::StandingGrant {
    crate::runtime::grants::StandingGrant {
        id: crate::runtime::grants::GrantId::new(id),
        agent: "ops".into(),
        workflow: None,
        tool: "workspace_write".into(),
        verdict: Verdict::Approve,
        granted_by: Actor {
            kind: ActorKind::User,
            id: "user-7".into(),
        },
        approval_id: ApprovalId::new("appr-1"),
        at_millis: 1_000,
        expires_at_millis: crate::ports::now_millis() + 60 * 60 * 1000,
        origin_thread: None,
        origin_parent: None,
        origin_task: None,
        scope: None,
    }
}

/// A [`JournalStore`](crate::ports::journal::JournalStore) that refuses
/// every `StandingGrantRevoked` line and passes everything else through.
/// Targets the **direct** `DELETE {scope}/grants/{gid}` append —
/// distinct from `runtime::cycle::test::FailStandingRevokeStore`, which
/// pins the mint/revoke *reconcile* path's own (oppositely ordered)
/// append.
pub(super) struct RefusingGrantRevokeStore {
    pub(super) inner: crate::ports::journal::MemoryJournalStore,
}

#[async_trait::async_trait]
impl crate::ports::journal::JournalStore for RefusingGrantRevokeStore {
    async fn append_journal(
        &self,
        id: &CompanyId,
        line: &str,
        durability: crate::ports::journal::Durability,
    ) -> crate::Result<()> {
        if line.contains("StandingGrantRevoked") {
            return Err(OpenCompanyError::Store(
                "RefusingGrantRevokeStore: the volume is full".to_string(),
            ));
        }
        self.inner.append_journal(id, line, durability).await
    }

    async fn read_journal(&self, id: &CompanyId) -> crate::Result<Vec<String>> {
        self.inner.read_journal(id).await
    }

    async fn journal_imported(&self, id: &CompanyId) -> crate::Result<bool> {
        self.inner.journal_imported(id).await
    }

    async fn complete_import(&self, id: &CompanyId, lines: Vec<String>) -> crate::Result<()> {
        self.inner.complete_import(id, lines).await
    }
}

// ---------------------------------------------------------------------
// Issue #469 — a turn that parks several approvals.
//
// Every test above parks exactly one, which is the case that always
// worked. The failure the operator hit needs more than one: four
// `composio_execute` calls from a single turn, all approved, and then
// silence. These drive that shape end to end over the real router.
// ---------------------------------------------------------------------

/// A brain that parks `parks` gated tool calls on one operator message and
/// answers each `ApprovalResolved` it is told about.
///
/// Deliberately shaped like `HarnessBrain`'s approval arm rather than like a
/// convenient stub: it consults the live grant set and produces **no reply
/// at all** when there is no grant left to redeem, because that silent
/// no-op is exactly what the later of several follow-up cycles used to hit.
struct MultiParkBrain {
    parks: usize,
    /// One entry per `ApprovalResolved` the brain was handed, across all
    /// cycles.
    decisions: Arc<std::sync::Mutex<Vec<String>>>,
    /// How many cycles ran in total (the first is the chat turn).
    cycles: Arc<std::sync::atomic::AtomicUsize>,
    /// The runtime, so the brain can reach the grant set the way the
    /// harness's re-dispatch does. Filled by the test after the build.
    rt: Arc<std::sync::OnceLock<Arc<CompanyRuntime>>>,
    /// Fail the continuation cycle, to exercise defect 4.
    fail_continuation: bool,
    /// Stamp a workflow run id onto every parked effect (issue #1092), so
    /// the park records the shape a workflow node's gated tool call has:
    /// explicitly unlinked from any card, and carrying a run.
    run_id: Option<String>,
    /// An `@mention` to append to every continuation reply. Exercises the
    /// durable half of a reply's mention: the re-issue's reply journaling
    /// must badge the person it names, same as the `/chat` path.
    continuation_mention: Option<String>,
}

#[async_trait::async_trait]
impl crate::ports::brain::Brain for MultiParkBrain {
    async fn run_cycle(
        &self,
        req: crate::ports::types::CycleRequest,
        host: &dyn crate::ports::brain::CycleHost,
    ) -> crate::Result<crate::ports::types::CycleResult> {
        self.cycles
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut responses = Vec::new();
        for event in &req.events {
            match event {
                CompanyEvent::OperatorMessage { .. } => {
                    // Deliberately uncatalogued slugs (issue #470): this
                    // brain exists to park a *number* of distinct calls and
                    // is indifferent to how any of them classify. They
                    // still land under the real action key, so each reaches
                    // the catalogue lookup and misses it, rather than
                    // carrying no action for the classifier to find.
                    for i in 0..self.parks {
                        let mut effect = gated_tool_call();
                        effect.payload =
                            crate::policy::test_support::composio_unclassified_args_numbered(i);
                        effect.run_id = self.run_id.clone();
                        host.park_effect(effect).await?;
                    }
                }
                CompanyEvent::ApprovalResolved { approval_id, .. } => {
                    if self.fail_continuation {
                        return Err(crate::error::OpenCompanyError::BackgroundTask(
                            "the continuation turn fell over".into(),
                        ));
                    }
                    self.decisions.lock().unwrap().push(approval_id.to_string());
                    let rt = self.rt.get().expect("the test wires the runtime");
                    let Some(grant) = rt.grants.peek(approval_id) else {
                        continue;
                    };
                    rt.grants.consume(&grant.agent, &grant.tool, &grant.args);
                    let mut text = format!("re-issued {approval_id}");
                    if let Some(mention) = &self.continuation_mention {
                        text.push(' ');
                        text.push_str(mention);
                    }
                    responses.push(crate::ports::types::OutboundMessage {
                        message_id: None,
                        task_id: None,
                        outputs: Vec::new(),
                        channel: grant.agent.clone(),
                        agent: None,
                        text,
                        steps: Vec::new(),
                        reply_to: None,
                        mentions: Vec::new(),
                    });
                }
                _ => {}
            }
        }
        Ok(crate::ports::types::CycleResult {
            channel_responses: responses,
            new_traces: vec![crate::ports::types::CompressedTrace::now(
                &req.cycle_id,
                "multi-park cycle",
            )],
            ledger_deltas: Vec::new(),
            token_usage: crate::ports::types::TokenUsage::default(),
        })
    }
}

/// A company whose next turn parks four sign-offs.
pub(super) struct MultiParkCompany {
    pub(super) app: axum::Router,
    pub(super) runtime: Arc<CompanyRuntime>,
    pub(super) approvals: Vec<ApprovalId>,
    pub(super) decisions: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) cycles: Arc<std::sync::atomic::AtomicUsize>,
}

pub(super) async fn multi_park_company(
    home: &std::path::Path,
    parks: usize,
    chat: Option<&str>,
    fail_continuation: bool,
) -> MultiParkCompany {
    multi_park_company_run(home, parks, chat, fail_continuation, None, None).await
}

/// [`multi_park_company`], with the parked effects stamped as a workflow
/// run (issue #1092).
pub(super) async fn multi_park_company_run(
    home: &std::path::Path,
    parks: usize,
    chat: Option<&str>,
    fail_continuation: bool,
    run_id: Option<&str>,
    continuation_mention: Option<&str>,
) -> MultiParkCompany {
    let decisions = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cycles = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rt_slot: Arc<std::sync::OnceLock<Arc<CompanyRuntime>>> =
        Arc::new(std::sync::OnceLock::new());
    let state = build_state_with_brain(
        home,
        "running",
        AppConfig::default(),
        Some(Arc::new(MultiParkBrain {
            parks,
            decisions: decisions.clone(),
            cycles: cycles.clone(),
            rt: rt_slot.clone(),
            fail_continuation,
            run_id: run_id.map(str::to_string),
            continuation_mention: continuation_mention.map(str::to_string),
        })),
    )
    .await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let _ = rt_slot.set(runtime.clone());
    let app = router(state);

    if chat.is_none() && run_id.is_some() {
        runtime
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "do it".to_string(),
                by: None,
                chat: None,
                parent: None,
                deliverable: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
            }])
            .await
            .expect("the parking cycle runs");
        let approvals: Vec<_> = runtime
            .pending_approvals()
            .iter()
            .map(|a| a.id.clone())
            .collect();
        assert_eq!(approvals.len(), parks, "the turn parked {parks} sign-offs");
        return MultiParkCompany {
            app,
            runtime,
            approvals,
            decisions,
            cycles,
        };
    }
    let body = match chat {
        Some(chat) => serde_json::json!({ "text": "do it", "chat": chat }),
        None => serde_json::json!({ "text": "do it" }),
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let approvals: Vec<_> = runtime
        .pending_approvals()
        .iter()
        .map(|a| a.id.clone())
        .collect();
    assert_eq!(approvals.len(), parks, "the turn parked {parks} sign-offs");

    MultiParkCompany {
        app,
        runtime,
        approvals,
        decisions,
        cycles,
    }
}

/// Every `AgentReply` in the log, as `chat_id|text` — what the console's
/// event stream projects as an `agent_reply` frame and what a transcript
/// reload rebuilds from. An empty list means the operator saw nothing.
pub(super) async fn agent_replies(runtime: &Arc<CompanyRuntime>) -> Vec<String> {
    use crate::ports::types::EventSeq;
    runtime
        .events()
        .read_from(runtime.id(), EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|s| match s.event {
            CompanyEvent::AgentReply { text, chat_id, .. } => Some(format!("{chat_id}|{text}")),
            _ => None,
        })
        .collect()
}

/// The authors of every journaled `AgentReply`, in order (issue #966).
///
/// Separate from [`agent_replies`] because that one folds the author away.
pub(super) async fn agent_reply_authors(runtime: &Arc<CompanyRuntime>) -> Vec<String> {
    use crate::ports::types::EventSeq;
    runtime
        .events()
        .read_from(runtime.id(), EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|s| match s.event {
            CompanyEvent::AgentReply { agent_id, .. } => Some(agent_id),
            _ => None,
        })
        .collect()
}

pub(super) fn approve_detached(id: &ApprovalId) -> Request<Body> {
    resolve_request(id, serde_json::json!({"verdict":"approve","detach":true}))
}

/// Waits for the follow-up work a detached resolve spawned to settle:
/// first for the turn to unblock, then for its continuation to have
/// journaled `expected_replies` `AgentReply` rows.
///
/// # Condition, not clock (issue #1071)
///
/// The second half used to be `sleep(400ms)` with a comment admitting what
/// it was — "the continuation itself runs on a spawned task; let it finish".
/// `ContinuationQueue::waiting()` drops to zero when the turn is
/// **unblocked**, which is strictly earlier than when the continuation's
/// replies are **written**, so the gap had to be covered by something. A
/// fixed sleep covers it only on a machine fast enough that day: on a loaded
/// CI runner the assertions read the event log first and came back short —
/// `3` replies instead of `4`, or `[]` instead of `["ceo"]` — on branches
/// with nothing to do with this code.
///
/// Raising the sleep is the tempting fix and only moves the threshold. This
/// waits for the thing the caller is about to assert, the same way the first
/// half already waits for `waiting()`, under the same 10-second cap. A test
/// that is going to check for N replies has no reason to proceed before N
/// replies exist, and every reason not to.
///
/// The count is the caller's because only the caller knows it. Passing a
/// number smaller than the assertion would reintroduce the race quietly, so
/// pass exactly what is asserted.
pub(super) async fn settle(runtime: &Arc<CompanyRuntime>, expected_replies: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while runtime.continuations.waiting() > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the turn never unblocked");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while agent_replies(runtime).await.len() < expected_replies {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the continuation never journaled {expected_replies} replies"));
}

/// A brain whose every reply names `@everyone` — the fixed shape for
/// proving an agent reply's mentions file a notification, same as an
/// operator message's already does.
pub(super) struct MentioningReplyBrain;

#[async_trait::async_trait]
impl crate::ports::brain::Brain for MentioningReplyBrain {
    async fn run_cycle(
        &self,
        req: crate::ports::types::CycleRequest,
        _host: &dyn crate::ports::brain::CycleHost,
    ) -> crate::Result<crate::ports::types::CycleResult> {
        let mut channel_responses = Vec::new();
        for event in &req.events {
            if matches!(event, CompanyEvent::OperatorMessage { .. }) {
                channel_responses.push(crate::ports::types::OutboundMessage {
                    message_id: None,
                    task_id: None,
                    outputs: Vec::new(),
                    channel: "operator".into(),
                    agent: None,
                    text: "cc @everyone on this".into(),
                    steps: Vec::new(),
                    reply_to: None,
                    mentions: Vec::new(),
                });
            }
        }
        Ok(crate::ports::types::CycleResult {
            channel_responses,
            new_traces: vec![crate::ports::types::CompressedTrace::now(
                &req.cycle_id,
                "mentioning reply",
            )],
            ledger_deltas: Vec::new(),
            token_usage: crate::ports::types::TokenUsage::default(),
        })
    }
}

#[cfg(feature = "openhuman")]
pub(super) fn card_in_review(id: &str, chat_id: &str) -> crate::ports::tasks::TaskRecord {
    crate::ports::tasks::TaskRecord {
        id: id.to_string(),
        title: TaskTitle::authored("Ship it"),
        note: None,
        column: crate::ports::tasks::COLUMN_IN_REVIEW.to_string(),
        priority: "medium".to_string(),
        assignee: "ceo".to_string(),
        updated_at_millis: 1,
        origin: crate::ports::TaskOrigin::new(Some(chat_id.to_string()), None),
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

// -- Approval authority: deciding for the company, not addressing it -----

/// Both address forms. Every ops route is registered under two, and this
/// pair had already drifted apart: only the alias carried the
/// temporary-password refusal, so every assertion below runs against both.
pub(super) const APPROVAL_SCOPES: [&str; 2] = ["/api/v1/companies/acme", "/api/v1/company"];

pub(super) fn resolve_as(scope: &str, approval_id: &str, cookie: Option<&str>) -> Request<Body> {
    let builder = Request::builder()
        .method("POST")
        .uri(format!("{scope}/approvals/{approval_id}"))
        .header("content-type", "application/json");
    let builder = match cookie {
        Some(cookie) => builder.header("cookie", cookie),
        None => builder,
    };
    builder
        .body(Body::from(
            serde_json::json!({ "verdict": "deny" }).to_string(),
        ))
        .unwrap()
}

pub(super) fn extend_as(scope: &str, approval_id: &str, cookie: Option<&str>) -> Request<Body> {
    let builder = Request::builder()
        .method("POST")
        .uri(format!("{scope}/approvals/{approval_id}/extend"));
    let builder = match cookie {
        Some(cookie) => builder.header("cookie", cookie),
        None => builder,
    };
    builder.body(Body::empty()).unwrap()
}

// -- resolve_attachments: IDOR-safe re-resolution, the attachment cap, --
// -- bad-id/folder refusal, and dedup (issue #1682) ----------------------
pub(super) fn attachment_binary_node(id: &str, name: &str, mime: &str) -> WorkspaceNode {
    WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind: NodeKind::File,
        parent_id: None,
        updated_at_millis: 1_700_000_000_000,
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: Some(mime.to_string()),
        size: None,
        sha256: None,
        adopted: false,
    }
}
