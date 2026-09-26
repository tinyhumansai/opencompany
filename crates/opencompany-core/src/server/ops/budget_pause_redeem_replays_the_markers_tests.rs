use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use super::*;
use crate::company::CompanyManifest;
use crate::ports::CompanyStore;
use crate::ports::types::{
    Attachment, CompanyId, CompanyRecord, EventSeq, Mention, MentionTarget, MessageIntent,
};
use crate::runtime::RuntimeBuilder;
use crate::runtime::grants::RedeemContext;
use crate::server::router;
use crate::store::FsCompanyStore;
use crate::{AppConfig, AppState};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("opencompany-budget-pause-")
        .tempdir()
        .expect("tempdir")
}

fn manifest() -> CompanyManifest {
    toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
}

/// Builds an [`AppState`] for a single company, its lone registered
/// runtime running on `brain`. Same shape as `operator.rs`'s
/// `build_state_with_brain` — a fresh `company` id per test, never the
/// shared `"acme"` other files' budget-pause tests use, so this file's
/// `BudgetPauseSet` (keyed globally by company id) never collides with a
/// concurrently-running test elsewhere in the same binary.
async fn state_with_brain(
    home: &std::path::Path,
    company: &str,
    brain: Arc<dyn crate::ports::brain::Brain>,
) -> AppState {
    state_with_brain_on(home, company, brain, manifest()).await
}

async fn state_with_brain_on(
    home: &std::path::Path,
    company: &str,
    brain: Arc<dyn crate::ports::brain::Brain>,
    m: CompanyManifest,
) -> AppState {
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new(company);
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: m.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            overlay_tool_grants: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        })
        .await
        .unwrap();

    let runtime = RuntimeBuilder::new(home.to_path_buf(), m)
        .with_id(id.clone())
        .with_brain(brain)
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, company).await;
    state
}

async fn send(
    state: &AppState,
    company: &str,
    method: &str,
    uri: &str,
) -> (StatusCode, Value, String) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", crate::server::test_support::fixed_cookie(company))
        .body(Body::empty())
        .unwrap();
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

/// A brain that records the last `OperatorMessage` event any cycle it
/// runs carries, and otherwise reports an uneventful cycle.
#[derive(Default)]
struct RecordingBrain {
    last: std::sync::Mutex<Option<CompanyEvent>>,
}

#[async_trait::async_trait]
impl crate::ports::brain::Brain for RecordingBrain {
    async fn run_cycle(
        &self,
        req: crate::ports::types::CycleRequest,
        _host: &dyn crate::ports::brain::CycleHost,
    ) -> crate::Result<crate::ports::types::CycleResult> {
        if let Some(event) = req
            .events
            .into_iter()
            .find(|e| matches!(e, CompanyEvent::OperatorMessage { .. }))
        {
            *self.last.lock().unwrap() = Some(event);
        }
        Ok(crate::ports::types::CycleResult {
            channel_responses: Vec::new(),
            new_traces: Vec::new(),
            ledger_deltas: Vec::new(),
            token_usage: crate::ports::types::TokenUsage::default(),
        })
    }
}

/// As [`RecordingBrain`], but also answers with one real reply bubble —
/// for a test that needs to see the redeemed turn's ANSWER actually
/// land, not just prove which event the brain saw (issue #1846 review,
/// Codex #3870168362).
#[derive(Default)]
struct ReplyingBrain {
    last: std::sync::Mutex<Option<CompanyEvent>>,
}

#[async_trait::async_trait]
impl crate::ports::brain::Brain for ReplyingBrain {
    async fn run_cycle(
        &self,
        req: crate::ports::types::CycleRequest,
        _host: &dyn crate::ports::brain::CycleHost,
    ) -> crate::Result<crate::ports::types::CycleResult> {
        if let Some(event) = req
            .events
            .into_iter()
            .find(|e| matches!(e, CompanyEvent::OperatorMessage { .. }))
        {
            *self.last.lock().unwrap() = Some(event);
        }
        Ok(crate::ports::types::CycleResult {
            channel_responses: vec![crate::ports::types::OutboundMessage {
                message_id: None,
                task_id: None,
                outputs: Vec::new(),
                channel: "general".to_string(),
                agent: Some("ceo".to_string()),
                text: "the API shipped".to_string(),
                steps: Vec::new(),
                reply_to: None,
                mentions: Vec::new(),
            }],
            new_traces: Vec::new(),
            ledger_deltas: Vec::new(),
            token_usage: crate::ports::types::TokenUsage::default(),
        })
    }
}

/// Issue #1846 review (Codex #3865812419/#3865812423/#3865812432): a
/// redeem replays the marker's parent/deliverable/mentions onto the
/// redispatched `OperatorMessage` instead of the empty defaults this
/// route used to fall back to.
#[tokio::test]
async fn redeem_replays_the_markers_parent_deliverable_and_mentions() {
    let home = home();
    let company = "acme-redeem-fields";
    let recording = Arc::new(RecordingBrain::default());
    let state = state_with_brain(home.path(), company, recording.clone()).await;
    let id = CompanyId::new(company);

    let parent = EventSeq::new(11);
    let deliverable = MessageIntent::Workflow;
    let mentions = vec![Mention {
        target: MentionTarget::Agent {
            id: "researcher".to_string(),
        },
        text: "@researcher".to_string(),
        offset: 0,
        quiet: false,
    }];
    budget_pauses_for(&id).park(
        "ceo",
        Some("general".to_string()),
        "ship the API",
        "paused",
        1_000,
        RedeemContext {
            parent: Some(parent),
            deliverable: Some(deliverable),
            mentions: mentions.clone(),
            text: None,
            attachments: Vec::new(),
        },
    );

    let (status, _resp, raw) = send(
        &state,
        company,
        "POST",
        "/api/v1/company/agents/ceo/budget-pause/redeem",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");

    let recorded = recording
        .last
        .lock()
        .unwrap()
        .clone()
        .expect("the redispatch reached the brain");
    match recorded {
        CompanyEvent::OperatorMessage {
            text,
            parent: got_parent,
            deliverable: got_deliverable,
            mentions: got_mentions,
            ..
        } => {
            // Not `assert_eq!`: a `Workflow`-deliverable message picks up
            // the builder-pass briefing (`cycle_conversation`'s
            // `inject_workflow_builder_awareness`) between the redispatch
            // and the brain seeing it — itself proof `deliverable`
            // actually reached the live cycle, not just this route's own
            // event construction.
            assert!(
                text.starts_with("ship the API"),
                "the operator's original words must lead the redispatched text: {text}"
            );
            assert_eq!(got_parent, Some(parent));
            assert_eq!(got_deliverable, Some(deliverable));
            assert_eq!(got_mentions, mentions);
        }
        other => panic!("expected an OperatorMessage, got {other:?}"),
    }
}

/// Issue #1846 review (Codex #3869369474) — **the regression.** Redeeming
/// a budget pause hands the reconstructed `OperatorMessage` straight to
/// `run_cycle`, bypassing `accept_chat_turn` (`src/server/operator.rs`)
/// entirely — the ordinary `/chat` path this route stands in for. That
/// bypass carried the mentions through onto the journaled event (chips
/// still render, per the sibling test above), but skipped the SEPARATE
/// `notify_mentions` call `accept_chat_turn` makes right after journaling
/// — so an `@user`/`@everyone` the original, paused message named badged
/// and notified nobody once it was resent, even though the console still
/// rendered the chip.
///
/// Mirrors `operator.rs`'s own
/// `a_continuation_reply_that_mentions_a_user_files_a_notification` —
/// same shape, this route's own fixture.
///
/// Mentions a SECOND, distinct user rather than the fixed admin
/// `state_with_brain` already seeds: `fixed_cookie` authenticates every
/// request in this module AS that admin, and `notify_mentions` never
/// notifies the author of their own message — mentioning the admin here
/// would self-exclude and read as "notify_mentions was never called" for
/// the wrong reason.
#[tokio::test]
async fn redeem_notifies_a_mentioned_user() {
    let home = home();
    let company = "acme-redeem-notify";
    let recording = Arc::new(RecordingBrain::default());
    let state = state_with_brain(home.path(), company, recording.clone()).await;
    let id = CompanyId::new(company);

    let runtime = state.registry().get(&id).expect("company is registered");
    let target_id = crate::ports::generate_id();
    runtime
        .users()
        .upsert_user(
            &id,
            &crate::ports::users::UserRecord {
                id: target_id.clone(),
                email: "teammate@example.test".to_string(),
                display_name: None,
                avatar: None,
                role: crate::ports::users::UserRole::Member,
                status: crate::ports::users::UserStatus::Active,
                password_hash: None,
                must_change_password: false,
                created_at_millis: crate::ports::now_millis(),
                last_seen_at_millis: None,
                updated_at_millis: crate::ports::now_millis(),
            },
        )
        .await
        .expect("seed the mentioned user");

    budget_pauses_for(&id).park(
        "ceo",
        Some("general".to_string()),
        "ship the API",
        "paused",
        1_000,
        RedeemContext {
            parent: None,
            deliverable: None,
            mentions: vec![Mention {
                target: MentionTarget::User {
                    id: target_id.clone(),
                },
                text: "@teammate".to_string(),
                offset: 0,
                quiet: false,
            }],
            text: None,
            attachments: Vec::new(),
        },
    );

    let (status, _resp, raw) = send(
        &state,
        company,
        "POST",
        "/api/v1/company/agents/ceo/budget-pause/redeem",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");

    // The append + notify happen synchronously in the handler, before the
    // redispatch is even spawned (see `redeem_budget_pause`'s doc), so —
    // unlike the continuation-reply sibling test — there is no
    // spawned-task race to poll for here.
    let notes = runtime.notifications().list(&id, &target_id).await.unwrap();
    let mentions: Vec<_> = notes
        .into_iter()
        .filter(|n| n.notification.kind == "mention")
        .collect();
    assert_eq!(
        mentions.len(),
        1,
        "redeeming a marker whose original message named a user must file that user's \
         mention notification, not just the chip"
    );
}

/// Issue #1846 review (Codex #3870168362) — **the regression.** The
/// redispatch's `CycleReport` used to be discarded (`Ok(Ok(_report))`
/// never read it) — unlike `accept_chat_turn`'s callers, which always
/// journal a cycle's `responses` via `journal_chat_replies`. The
/// redeemed turn's own answer therefore executed but never appeared in
/// the transcript or over SSE.
#[tokio::test]
async fn redeem_journals_the_redispatched_turns_reply() {
    let home = home();
    let company = "acme-redeem-journals-reply";
    let replying = Arc::new(ReplyingBrain::default());
    let state = state_with_brain(home.path(), company, replying.clone()).await;
    let id = CompanyId::new(company);
    let runtime = state.registry().get(&id).expect("company is registered");

    budget_pauses_for(&id).park(
        "ceo",
        Some("general".to_string()),
        "ship the API",
        "paused",
        1_000,
        RedeemContext::default(),
    );

    let (status, _resp, raw) = send(
        &state,
        company,
        "POST",
        "/api/v1/company/agents/ceo/budget-pause/redeem",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");

    let events = runtime
        .events()
        .read_from(&id, crate::ports::EventSeq::new(0), 100)
        .await
        .unwrap();
    let reply = events.iter().find_map(|e| match &e.event {
        CompanyEvent::AgentReply { text, .. } if text == "the API shipped" => Some(text),
        _ => None,
    });
    assert!(
        reply.is_some(),
        "the redeemed turn's reply must be journaled as an AgentReply — the transcript and \
         SSE feed have no other way to learn the turn answered at all: {events:?}"
    );
}

/// Issue #1846 review (Codex #3866418891) — the keystone test for the
/// attachment-flattening fix. Before this fix, `redeem_budget_pause`
/// always redispatched with `attachments: Vec::new()`, so a paused
/// message that had files parked would journal the rerun as though the
/// operator had typed the `[Attached file: ...]` marker text themselves
/// (baked into `marker.message` by `with_attachment_refs` upstream of
/// `park`), with the structured attachment gone. This proves the
/// opposite: the marker's `attachments` survive parking and are replayed
/// on the redispatched event, not flattened.
#[tokio::test]
async fn redeem_replays_the_markers_attachments() {
    let home = home();
    let company = "acme-redeem-attachments";
    let recording = Arc::new(RecordingBrain::default());
    let state = state_with_brain(home.path(), company, recording.clone()).await;
    let id = CompanyId::new(company);

    let attachments = vec![Attachment {
        node_id: "node-1".to_string(),
        name: "quarterly-report.pdf".to_string(),
        mime: "application/pdf".to_string(),
        size: 4096,
        extracted_text: Some("Q3 revenue grew 12%".to_string()),
    }];
    budget_pauses_for(&id).park(
        "ceo",
        Some("general".to_string()),
        "review the attached report",
        "paused",
        1_000,
        RedeemContext {
            attachments: attachments.clone(),
            ..RedeemContext::default()
        },
    );

    let (status, _resp, raw) = send(
        &state,
        company,
        "POST",
        "/api/v1/company/agents/ceo/budget-pause/redeem",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");

    let recorded = recording
        .last
        .lock()
        .unwrap()
        .clone()
        .expect("the redispatch reached the brain");
    match recorded {
        CompanyEvent::OperatorMessage {
            text,
            attachments: got_attachments,
            ..
        } => {
            assert_eq!(
                text, "review the attached report",
                "the raw operator text must not carry a baked attachment marker"
            );
            assert_eq!(
                got_attachments, attachments,
                "the marker's structured attachments must replay onto the redispatched \
                 event instead of being dropped"
            );
        }
        other => panic!("expected an OperatorMessage, got {other:?}"),
    }
}

/// Issue #1846 review (Codex #3869193112) — **the regression.** A marker
/// parked via `park_background` (a dispatched task card or a workflow
/// agent node — the ONLY call site that uses it, in `mod.rs`'s
/// `run_inner`) has no chat thread an operator was ever addressing. Its
/// `chat_id` is `None`, same as an ordinary unaddressed interactive
/// message's — the ONE case redeeming through the generic
/// `OperatorMessage` path is actually correct for. Before this fix
/// nothing told the two apart: redeeming the background one would have
/// routed an unaddressed message to the orchestrator instead of the
/// original task/node, leaving the original stuck forever.
///
/// The marker must survive the refusal (restored, not dropped) and the
/// brain must never be reached — same "left completely untouched" shape
/// as the stale-id sibling test below.
#[tokio::test]
async fn redeem_refuses_a_background_marker() {
    let home = home();
    let company = "acme-redeem-background";
    let recording = Arc::new(RecordingBrain::default());
    let state = state_with_brain(home.path(), company, recording.clone()).await;
    let id = CompanyId::new(company);

    let marker = budget_pauses_for(&id).park_background(
        "ceo",
        None,
        "run the nightly workflow node",
        "paused for the background turn",
        1_000,
        RedeemContext::default(),
    );
    assert!(
        marker.background,
        "park_background must set the field it names"
    );

    let (status, _resp, raw) = send(
        &state,
        company,
        "POST",
        "/api/v1/company/agents/ceo/budget-pause/redeem",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a background marker must be refused, not silently redispatched to the \
         orchestrator: {raw}"
    );
    assert!(
        recording.last.lock().unwrap().is_none(),
        "a refused background redeem must never reach the brain"
    );

    let still_parked = budget_pauses_for(&id)
        .peek("ceo")
        .expect("the marker must survive the refusal, not be dropped");
    assert_eq!(still_parked.id, marker.id);
}

/// Issue #1846 review (Codex #3870271005) — **the regression.** This
/// route never checked the company's durable `lifecycle` field the way
/// `accept_chat_turn` (`src/server/operator.rs`) does as its own first
/// line. `run_cycle`/`run_journaled_cycle` only refuse on process-local
/// quiescing (a runtime mid-rebuild), never on an operator having
/// explicitly paused or archived the company — so a stale "Add credits &
/// resend" CTA on a stopped company could still reserve the marker and
/// execute a fresh agent turn.
#[tokio::test]
async fn redeem_refuses_on_a_paused_company() {
    let home = home();
    let company = "acme-redeem-paused-company";
    let recording = Arc::new(RecordingBrain::default());
    let state = state_with_brain(home.path(), company, recording.clone()).await;
    let id = CompanyId::new(company);

    let marker = budget_pauses_for(&id).park(
        "ceo",
        Some("general".to_string()),
        "ship the API",
        "paused",
        1_000,
        RedeemContext::default(),
    );

    // Flip the company to `paused`, the same durable field
    // `ensure_running` reads — same store, same path `state_with_brain`
    // itself wrote the initial `running` record to.
    let store = FsCompanyStore::new(home.path().to_path_buf());
    let mut record = store
        .load(&id)
        .await
        .unwrap()
        .expect("the company record exists");
    record.lifecycle = "paused".to_string();
    store.save(&record).await.unwrap();

    let (status, _resp, raw) = send(
        &state,
        company,
        "POST",
        "/api/v1/company/agents/ceo/budget-pause/redeem",
    )
    .await;
    assert_ne!(
        status,
        StatusCode::OK,
        "a paused company must refuse the redeem, not execute a fresh turn: {raw}"
    );
    assert!(
        recording.last.lock().unwrap().is_none(),
        "a refused redeem on a paused company must never reach the brain"
    );

    let still_parked = budget_pauses_for(&id)
        .peek("ceo")
        .expect("the marker must survive the refusal, not be dropped");
    assert_eq!(still_parked.id, marker.id);
}

/// Issue #1846 review (Codex #3866418876) — the keystone test for the
/// background-overwrite fix. A chat-visible pause parks a marker for
/// `ceo` with a chat destination; a background turn (a workflow node or
/// an unstreamed task) for the SAME agent then pauses too and
/// overwrites it with a marker that has none. The console's stale-card
/// check never sees this happen — a chat-less park never touches the
/// transcript it watches — so the OLD chat card is still what the
/// operator clicks. Redeeming with that card's (now stale) `?id=` must
/// be refused with 409 and must NOT redispatch anything — proven here
/// by the recording brain seeing no `OperatorMessage` at all, not merely
/// the "wrong" one. Redeeming with the CURRENT marker's id then succeeds
/// and redispatches the background pause's own message.
#[tokio::test]
async fn a_stale_marker_id_is_refused_without_redispatching_the_background_pause() {
    let home = home();
    let company = "acme-redeem-stale-id";
    let recording = Arc::new(RecordingBrain::default());
    let state = state_with_brain(home.path(), company, recording.clone()).await;
    let id = CompanyId::new(company);

    let chat_marker = budget_pauses_for(&id).park(
        "ceo",
        Some("general".to_string()),
        "ship the API",
        "paused for the chat turn",
        1_000,
        RedeemContext::default(),
    );
    let background_marker = budget_pauses_for(&id).park(
        "ceo",
        None,
        "run the nightly workflow node",
        "paused for the background turn",
        2_000,
        RedeemContext::default(),
    );

    // The console clicks the OLD chat card, so it sends the OLD id.
    let (status, _resp, raw) = send(
        &state,
        company,
        "POST",
        &format!(
            "/api/v1/company/agents/ceo/budget-pause/redeem?id={}",
            chat_marker.id
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a stale marker id must be refused, not silently honoured: {raw}"
    );
    assert!(
        recording.last.lock().unwrap().is_none(),
        "a refused stale redeem must never reach the brain — the background pause's \
         message must not be silently redispatched under a click meant for the chat one"
    );
    // Left completely untouched — still there, still the background one.
    let still_parked = budget_pauses_for(&id)
        .peek("ceo")
        .expect("survives the refusal");
    assert_eq!(still_parked.id, background_marker.id);

    // The console re-reads the live marker and redeems with ITS id.
    let (status, _resp, raw) = send(
        &state,
        company,
        "POST",
        &format!(
            "/api/v1/company/agents/ceo/budget-pause/redeem?id={}",
            background_marker.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    let recorded = recording
        .last
        .lock()
        .unwrap()
        .clone()
        .expect("the redispatch reached the brain");
    match recorded {
        CompanyEvent::OperatorMessage { text, .. } => {
            assert!(
                text.starts_with("run the nightly workflow node"),
                "the background pause's own message must be what gets resent: {text}"
            );
        }
        other => panic!("expected an OperatorMessage, got {other:?}"),
    }
}

/// A caller that sends no `?id=` at all falls back to the pre-fix,
/// unconditional redeem — the escape hatch for anything that has no
/// prior marker read to compare against.
#[tokio::test]
async fn omitting_the_id_query_param_redeems_unconditionally() {
    let home = home();
    let company = "acme-redeem-no-id-param";
    let recording = Arc::new(RecordingBrain::default());
    let state = state_with_brain(home.path(), company, recording.clone()).await;
    let id = CompanyId::new(company);

    budget_pauses_for(&id).park(
        "ceo",
        None,
        "ship the API",
        "paused",
        1_000,
        RedeemContext::default(),
    );

    let (status, _resp, raw) = send(
        &state,
        company,
        "POST",
        "/api/v1/company/agents/ceo/budget-pause/redeem",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
}

/// A brain whose `run_cycle` always refuses — the redispatch never
/// completes successfully.
pub(super) struct FailingRedispatchBrain;

#[tokio::test]
async fn redeeming_an_unaddressed_pause_answers_in_the_default_agents_dm() {
    let home = home();
    let company = "acme-redeem-unaddressed";
    let replying = Arc::new(ReplyingBrain::default());
    let roster: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief Executive\"\n",
    )
    .unwrap();
    let state = state_with_brain_on(home.path(), company, replying.clone(), roster).await;
    let id = CompanyId::new(company);
    let runtime = state.registry().get(&id).expect("company is registered");

    budget_pauses_for(&id).park(
        "ceo",
        None,
        "ship the API",
        "paused",
        1_000,
        RedeemContext::default(),
    );

    let (status, _resp, raw) = send(
        &state,
        company,
        "POST",
        "/api/v1/company/agents/ceo/budget-pause/redeem",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");

    let events = runtime
        .events()
        .read_from(&id, crate::ports::EventSeq::new(0), 100)
        .await
        .unwrap();
    let asked = events.iter().find_map(|e| match &e.event {
        CompanyEvent::OperatorMessage { chat, .. } => Some(chat.clone()),
        _ => None,
    });
    assert_eq!(asked, Some(Some("dm:ceo".to_string())));
    match replying.last.lock().unwrap().clone() {
        Some(CompanyEvent::OperatorMessage { chat, .. }) => {
            assert_eq!(chat.as_deref(), Some("dm:ceo"), "the turn runs in the DM")
        }
        other => panic!("expected the redispatched OperatorMessage, got {other:?}"),
    }
}
