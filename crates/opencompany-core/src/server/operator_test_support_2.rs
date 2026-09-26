use super::*;
use crate::AppConfig;
use crate::server::router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::operator_test_support_1::*;

pub(super) async fn delete_desk_member(
    app: &axum::Router,
    cookie: &str,
    desk: &str,
    agent: &str,
) -> Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/company/desks/{desk}/members/{agent}"))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Reads the `error` string out of an api.md error envelope.
pub(super) async fn error_message(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["error"].as_str().unwrap().to_string()
}

/// Returns the effective member list of `desk` from `list_desks`.
pub(super) async fn desk_members(app: &axum::Router, cookie: &str, desk: &str) -> Vec<String> {
    let desks = get_desks(app, cookie).await;
    desks
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == desk)
        .unwrap_or_else(|| panic!("desk {desk} present in list"))["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap().to_string())
        .collect()
}

/// Seeds `eng` as an overlay member of `studio` so a desk has two members to
/// reorder.
pub(super) async fn seed_overlay_eng(app: &axum::Router, cookie: &str) {
    let add = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/desks/studio/members")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"eng"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(add.status(), StatusCode::NO_CONTENT);
}

pub(super) async fn put_desk_order(
    app: &axum::Router,
    cookie: &str,
    desk: &str,
    body: &str,
) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/company/desks/{desk}/order"))
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// Everything one agent said and heard, for a company with one reply in it.
///
/// Fetched through the router so the assertion is about the wire, not about
/// the struct it was built from.
pub(super) async fn session_rows(uri: &str) -> Vec<serde_json::Value> {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    runtime
        .events()
        .append(
            runtime.id(),
            CompanyEvent::AgentReply {
                audience: Vec::new(),
                mentions: Vec::new(),
                mention_depth: 0,
                parent: None,
                task_id: None,
                outputs: Vec::new(),
                chat_id: "General".to_string(),
                agent_id: "ceo".to_string(),
                text: "one turn, from one session".to_string(),
                steps: Vec::new(),
                episode: None,
            },
        )
        .await
        .unwrap();

    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "GET {uri}");
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let rows = value.as_array().cloned().unwrap_or_default();
    assert!(!rows.is_empty(), "no session rows came back from {uri}");
    rows
}

/* ---- issue #364: durable ids, threads, reactions, channel isolation ---- */

/// Posts a chat message and returns the decoded `ChatResponse` body.
pub(super) async fn post_chat(app: &Router, cookie: &str, body: &str) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "chat POST failed");
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The default agent's DM — where a message with no `chat` lands.
pub(super) async fn default_dm(state: &AppState) -> String {
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    runtime
        .default_agent_dm()
        .await
        .unwrap()
        .expect("the company has a default agent")
}

/// Reads a desk's history. `desk` empty reads the default General thread.
pub(super) async fn get_history(app: &Router, cookie: &str, desk: &str) -> Vec<serde_json::Value> {
    let uri = if desk.is_empty() {
        "/api/v1/company/chat/history".to_string()
    } else {
        format!("/api/v1/company/chat/history?desk={desk}")
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value.as_array().cloned().unwrap_or_default()
}

/// Sets or clears one reaction, returning the status.
pub(super) async fn post_reaction(
    app: &Router,
    cookie: &str,
    seq: &str,
    emoji: &str,
    on: bool,
) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/company/chat/messages/{seq}/reactions"))
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "emoji": emoji, "on": on }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The tool call the operator is asked to sign off. `agent: Some(_)` is what
/// makes approving it mint a single-use grant rather than execute it
/// (issue #243) — which is the whole reason a lost continuation hurts: the
/// grant is spent on a turn that never happens.
///
/// The payload names an action the vendored catalogue tags `Write`, so
/// `consequence_of` classifies it as a send on its merits. Until issue #470
/// it named the slug under `tool_slug`, a key neither the tool nor the
/// classifier reads — so it was a call with no action at all, and it
/// reached the per-call verdict through the unknown-slug fallback instead.
pub(super) fn gated_tool_call() -> crate::ports::types::Effect {
    crate::ports::types::Effect {
        kind: "composio_execute".into(),
        group: crate::ports::types::EffectGroup::Sign,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: false,
        payload: crate::policy::test_support::composio_send_args(),
        agent: Some("ceo".into()),
        run_id: None,
    }
}

/// A tool call an operator MAY grant a standing permission for (issue
/// #431), which `gated_tool_call` deliberately is not: its Composio payload
/// names an action the catalogue tags `Write`, so `consequence_of` reads it
/// as a send and it stays a per-call decision. `file_write` is declared
/// grantable in `src/policy/consequence.rs` and carries an agent, so it
/// satisfies both halves of `check_broadly_grantable` — it mutates, but
/// only the agent's own sandboxed workspace.
pub(super) fn grantable_tool_call() -> crate::ports::types::Effect {
    crate::ports::types::Effect {
        kind: "file_write".into(),
        group: crate::ports::types::EffectGroup::Other,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: false,
        payload: serde_json::json!({ "path": "notes/a.md", "body": "one" }),
        agent: Some("ceo".into()),
        run_id: None,
    }
}

/// The dotted kind the stalled brain parks once its follow-up turn gets
/// past the barrier. Parking journals durably (`record_parked`), so its
/// presence in `pending_approvals()` is proof the continuation reached the
/// end of the turn *and* wrote to disk — not merely that a task was alive.
pub(super) const CONTINUATION_MARKER: &str = "continuation.marker";

/// A brain that parks one gated tool call per operator message and, on the
/// follow-up `ApprovalResolved` cycle, blocks mid-turn until the test
/// releases it — the shape of a slow agent turn behind a proxy.
pub(super) struct StalledContinuationBrain {
    /// Fires once the follow-up turn has begun. By this point the verdict
    /// is journaled and the grant minted, so this is exactly the moment the
    /// field report's connection died.
    pub(super) entered: Arc<tokio::sync::Notify>,
    /// The test's permission for the turn to finish.
    pub(super) release: Arc<tokio::sync::Notify>,
    /// The effect parked for the operator's sign-off. Whether it may be
    /// granted a standing permission is a property of this effect, so the
    /// scope tests supply their own rather than sharing one fixture.
    pub(super) parked: crate::ports::types::Effect,
}

#[async_trait::async_trait]
impl crate::ports::brain::Brain for StalledContinuationBrain {
    async fn run_cycle(
        &self,
        req: crate::ports::types::CycleRequest,
        host: &dyn crate::ports::brain::CycleHost,
    ) -> crate::Result<crate::ports::types::CycleResult> {
        for event in &req.events {
            match event {
                CompanyEvent::OperatorMessage { .. } => {
                    host.park_effect(self.parked.clone()).await?;
                }
                CompanyEvent::ApprovalResolved { .. } => {
                    self.entered.notify_one();
                    self.release.notified().await;
                    host.park_effect(crate::ports::types::Effect {
                        kind: CONTINUATION_MARKER.into(),
                        group: crate::ports::types::EffectGroup::Other,
                        amount_usd: None,
                        established_thread: false,
                        first_time_counterparty: false,
                        payload: serde_json::json!({}),
                        agent: None,
                        run_id: None,
                    })
                    .await?;
                }
                _ => {}
            }
        }
        Ok(crate::ports::types::CycleResult {
            channel_responses: Vec::new(),
            new_traces: vec![crate::ports::types::CompressedTrace::now(
                &req.cycle_id,
                "stalled continuation",
            )],
            ledger_deltas: Vec::new(),
            token_usage: crate::ports::types::TokenUsage::default(),
        })
    }
}

pub(super) fn chat_request(text: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/company/chat")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::json!({ "text": text }).to_string()))
        .unwrap()
}

/// A resolve against the single-company alias. `scope` lets the same body be
/// aimed at the `/companies/{id}` form, which must behave identically.
pub(super) fn resolve_request_scoped(
    scope: &str,
    approval_id: &ApprovalId,
    body: serde_json::Value,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("{scope}/approvals/{approval_id}"))
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub(super) fn resolve_request(approval_id: &ApprovalId, body: serde_json::Value) -> Request<Body> {
    resolve_request_scoped("/api/v1/company", approval_id, body)
}

// -- A blocker answered from the Approvals page (issue #2028) -------------

/// Parks a workflow-node blocker: `TaskLink::Unlinked` with no
/// conversation, which is the shape a node blocker takes and the reason the
/// chat blocker path — which filters on the thread — can never reach one.
#[cfg(feature = "openhuman")]
pub(super) async fn park_node_blocker(
    runtime: &Arc<CompanyRuntime>,
    id: &str,
    group_key: Option<&str>,
) -> ApprovalId {
    use crate::ports::blockers::{BlockerKind, BlockerPayload, BlockerSource, BlockerStep};
    use crate::runtime::journal::{ApprovalConversation, TaskLink};

    let payload = BlockerPayload {
        kind: BlockerKind::Infrastructure,
        source: BlockerSource::Provider,
        step: Some(BlockerStep::Node {
            run_id: "run-1".to_string(),
            node_id: "draft".to_string(),
        }),
        reason: "the model id `gpt-nope` was rejected".to_string(),
        needed: "a model id this provider serves".to_string(),
        group_key: group_key.map(str::to_string),
    };
    let approval = ApprovalId::new(id);
    let effect = crate::ports::types::Effect {
        kind: payload.effect_kind(),
        group: crate::ports::types::EffectGroup::Other,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: false,
        payload: serde_json::to_value(&payload).unwrap(),
        agent: None,
        run_id: Some("run-1".to_string()),
    };
    let at = crate::ports::now_millis();
    runtime
        .approval_gate
        .rehydrate(approval.clone(), effect.clone(), at);
    runtime
        .journal
        .record_parked(
            &approval,
            &effect,
            at,
            TaskLink::Unlinked,
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    approval
}

/// Every `BlockerResolved` line the durable journal holds, in append order —
/// what the operator's answer actually banked, read off disk rather than off
/// the in-memory map the resume consumes.
#[cfg(feature = "openhuman")]
pub(super) async fn banked_resolutions(
    home: &std::path::Path,
    company: &CompanyId,
) -> Vec<serde_json::Value> {
    let path = crate::store::paths::Bundle::new(home, company).journal_jsonl();
    let raw = tokio::fs::read_to_string(path).await.unwrap_or_default();
    raw.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|line| line["record"] == "BlockerResolved")
        .collect()
}

/// A company with one parked workflow-node blocker, and the pieces a resolve
/// test needs to read back what its click banked.
#[cfg(feature = "openhuman")]
pub(super) struct BlockedCompany {
    pub(super) app: axum::Router,
    pub(super) runtime: Arc<CompanyRuntime>,
    pub(super) home: std::path::PathBuf,
    pub(super) company: CompanyId,
    pub(super) approval_id: ApprovalId,
}

#[cfg(feature = "openhuman")]
pub(super) async fn blocked_company(home: &std::path::Path) -> BlockedCompany {
    let home = home.to_path_buf();
    let state = state_with_company(&home, "running").await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();
    let app = router(state);
    let approval_id = park_node_blocker(&runtime, "blocker-1", None).await;
    BlockedCompany {
        app,
        runtime,
        home,
        company,
        approval_id,
    }
}

/// Posts a resolve and returns its status and parsed body.
#[cfg(feature = "openhuman")]
pub(super) async fn post_resolve(
    app: &axum::Router,
    id: &ApprovalId,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(resolve_request(id, body))
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// Every refusal owes the same two things beyond its 400: the blocker is
/// still parked, and nothing was banked. A validation that answered 400
/// after journaling a verdict would have spent the operator's question.
#[cfg(feature = "openhuman")]
pub(super) async fn assert_refused(body: serde_json::Value, expect_in_error: &str) {
    let home_dir = home();
    let c = blocked_company(home_dir.path()).await;

    let (status, answer) = post_resolve(&c.app, &c.approval_id, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    let message = answer["error"].as_str().unwrap_or_default();
    assert!(
        message.contains(expect_in_error),
        "the refusal must say why; got {message:?}"
    );
    assert!(
        c.runtime
            .pending_approvals()
            .iter()
            .any(|p| p.id == c.approval_id),
        "a refused request must leave the blocker parked"
    );
    assert!(
        banked_resolutions(&c.home, &c.company).await.is_empty(),
        "a refused request must journal no verdict"
    );
}

// -- Extend the deadline (issue #1805) -----------------------------------

/// Parks one effect in BOTH the gate and the journal under a fixed id, at a
/// controllable instant — the gate is what `extend_approval` asks whether an
/// id is live, and the journal is what projects the deadline, so an extend
/// test needs both seeded exactly as a real park leaves them.
pub(super) async fn park_for_extend(
    runtime: &Arc<CompanyRuntime>,
    id: &str,
    at_millis: u64,
) -> ApprovalId {
    use crate::runtime::journal::{ApprovalConversation, TaskLink};
    let approval = ApprovalId::new(id);
    let effect = crate::ports::types::Effect {
        kind: "payment.send".into(),
        group: crate::ports::types::EffectGroup::Spend,
        amount_usd: Some(1_200.0),
        established_thread: false,
        first_time_counterparty: false,
        payload: serde_json::json!({ "to": "vendor@example.test" }),
        agent: Some("ceo".into()),
        run_id: None,
    };
    runtime
        .approval_gate
        .rehydrate(approval.clone(), effect.clone(), at_millis);
    runtime
        .journal
        .record_parked(
            &approval,
            &effect,
            at_millis,
            TaskLink::Unlinked,
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    approval
}

pub(super) fn extend_request_with_cookie(
    approval_id: &ApprovalId,
    cookie: String,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/api/v1/company/approvals/{approval_id}/extend"))
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap()
}

pub(super) fn extend_request(approval_id: &ApprovalId) -> Request<Body> {
    extend_request_with_cookie(
        approval_id,
        crate::server::test_support::fixed_cookie("acme"),
    )
}

pub(super) async fn body_json(response: Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Whether the stalled brain's follow-up turn has journaled its marker yet.
pub(super) fn continued(runtime: &Arc<CompanyRuntime>) -> bool {
    runtime
        .pending_approvals()
        .iter()
        .any(|a| a.kind == CONTINUATION_MARKER)
}

/// Waits for the stalled brain's follow-up turn to journal its marker.
pub(super) async fn await_continuation(runtime: &Arc<CompanyRuntime>) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !continued(runtime) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// A running company with one tool call parked and a brain that will stall
/// on the follow-up turn until `release` is fired.
pub(super) struct StalledCompany {
    pub(super) app: axum::Router,
    pub(super) runtime: Arc<CompanyRuntime>,
    pub(super) approval_id: ApprovalId,
    /// Fires once the follow-up turn has begun — by which point the verdict
    /// is journaled and the grant minted.
    pub(super) entered: Arc<tokio::sync::Notify>,
    /// The test's permission for that turn to finish.
    pub(super) release: Arc<tokio::sync::Notify>,
}

pub(super) async fn stalled_company(home: &std::path::Path) -> StalledCompany {
    stalled_company_parking(home, gated_tool_call()).await
}

/// `stalled_company`, with the parked effect chosen by the caller — because
/// whether a scope may be granted is decided by the effect, not the route.
pub(super) async fn stalled_company_parking(
    home: &std::path::Path,
    parked: crate::ports::types::Effect,
) -> StalledCompany {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let state = build_state_with_brain(
        home,
        "running",
        AppConfig::default(),
        Some(Arc::new(StalledContinuationBrain {
            entered: entered.clone(),
            release: release.clone(),
            parked,
        })),
    )
    .await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let app = router(state);

    let response = app.clone().oneshot(chat_request("do it")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let parked = runtime.pending_approvals();
    assert_eq!(parked.len(), 1, "the brain parked one tool call");
    let approval_id = parked[0].id.clone();

    StalledCompany {
        app,
        runtime,
        approval_id,
        entered,
        release,
    }
}

/// The reply a stalled chat turn produces once released.
pub(super) const SLOW_TURN_REPLY: &str = "the slow turn's answer";

/// A brain that stalls on the operator's **first** turn — the chat lane,
/// rather than the approval follow-up `StalledContinuationBrain` stalls on.
pub(super) struct StalledChatBrain {
    /// Fires once the turn is under way, which is the moment the field
    /// report's proxy gave up and closed the connection.
    pub(super) entered: Arc<tokio::sync::Notify>,
    /// The test's permission for that turn to finish.
    pub(super) release: Arc<tokio::sync::Notify>,
}
