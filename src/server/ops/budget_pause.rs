//! `GET {scope}/agents/{agent_id}/budget-pause` and
//! `POST {scope}/agents/{agent_id}/budget-pause/redeem` (issue #1846): the
//! console's Add-Credits CTA. A turn that paused for lack of inference
//! budget/credits parks a durable marker
//! ([`crate::runtime::grants::BudgetPauseMarker`]) naming the original
//! message; this route lets the operator read that marker back and trigger
//! its re-issue once the account is topped up.
//!
//! **Not true resume** (issue #561): redeeming re-enters the SAME cycle path
//! an ordinary chat message takes
//! ([`CompanyEvent::OperatorMessage`](crate::ports::types::CompanyEvent::OperatorMessage)
//! through [`CompanyRuntime::run_cycle`](crate::company::runtime::CompanyRuntime::run_cycle)),
//! addressed to the same chat thread the original message was. Whatever the
//! paused attempt had already done stays done; the redeemed turn runs fresh
//! from the top and can repeat a non-idempotent side effect the first attempt
//! already performed.

use std::sync::Arc;

use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyEvent;
use crate::runtime::grants::{BudgetPauseMarker, BudgetPauseSet, RedeemMatch, budget_pauses_for};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the budget-pause route fragment.
pub fn router() -> Router<AppState> {
    scoped("/agents/{agent_id}/budget-pause", get(get_budget_pause)).merge(scoped(
        "/agents/{agent_id}/budget-pause/redeem",
        post(redeem_budget_pause),
    ))
}

#[derive(Debug, Deserialize)]
struct AgentPath {
    agent_id: String,
}

/// The redeem route's `?id=` — the marker id the console last read via
/// `GET`, so the reservation below can be matched rather than blind (issue
/// #1846 review, Codex #3866418876). Absent for a caller with no prior read
/// to compare against, in which case redemption falls back to the
/// unconditional pre-fix behaviour.
#[derive(Debug, Deserialize)]
struct RedeemQuery {
    #[serde(default)]
    id: Option<String>,
}

/// The console's read of a parked budget pause.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BudgetPauseDto {
    id: String,
    agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_id: Option<String>,
    message: String,
    summary: String,
    at_millis: u64,
}

impl From<BudgetPauseMarker> for BudgetPauseDto {
    fn from(marker: BudgetPauseMarker) -> Self {
        Self {
            id: marker.id,
            agent: marker.agent,
            chat_id: marker.chat_id,
            message: marker.message,
            summary: marker.summary,
            at_millis: marker.at_millis,
        }
    }
}

/// `GET {scope}/agents/{agent_id}/budget-pause` — the parked marker for this
/// agent, or `null` when nothing is paused. Read-only: does not consume the
/// marker, so the console can poll/render it (the "approaching"/"exhausted"
/// banner) without accidentally triggering a redeem.
async fn get_budget_pause(
    company: ScopedCompany,
    Path(AgentPath { agent_id }): Path<AgentPath>,
) -> Json<Option<BudgetPauseDto>> {
    let marker = budget_pauses_for(company.id()).peek(&agent_id);
    Json(marker.map(BudgetPauseDto::from))
}

/// `POST {scope}/agents/{agent_id}/budget-pause/redeem` — the Add-Credits CTA.
/// Reserves the marker (single-use, like a
/// [`GrantedCall`](crate::runtime::grants::GrantedCall) redemption), THEN
/// re-dispatches the original message through the same cycle path an
/// ordinary operator send takes, addressed to the same chat the pause
/// happened on.
///
/// Deliberately "reserve, then re-dispatch", not "peek, re-dispatch, then
/// consume" (issue #1846 review, Codex #3865395849, replacing the shape
/// Codex #3864988181 first added): peeking first left a window between two
/// concurrent redeem requests — say, clicks from two browser tabs — where
/// BOTH could read the same marker before either had re-dispatched, so both
/// re-dispatched it, and only one of the two later consume calls actually
/// won while the loser still reported success to its own caller, silently
/// repeating whatever non-idempotent side effect the original attempt
/// performed. [`redeem`](crate::runtime::grants::BudgetPauseSet::redeem)
/// takes the marker atomically up front, so the SECOND request's own
/// reservation finds nothing — the first already took it — and 404s before
/// it ever re-dispatches.
///
/// A reservation that never redispatches (this call errors before
/// `run_cycle` returns) is restored via
/// [`restore_if_absent`](crate::runtime::grants::BudgetPauseSet::restore_if_absent)
/// rather than left gone: a re-dispatch failure — the event store hiccups,
/// the request is cancelled mid-flight — must not silently lose the CTA's
/// saved re-issue payload to a `404` on the very next click. Guarded on
/// absence rather than a plain re-insert: the re-dispatch can itself pause
/// again on the same agent before the restore runs, and restoring only when
/// nothing is parked is what keeps that fresh marker from being clobbered by
/// the stale one being put back.
///
/// 404 when nothing is parked for this agent — the operator's own "add
/// credits" action beat them to it, the process restarted since the pause
/// (this marker is in-memory only, see
/// [`crate::runtime::grants::BudgetPauseMarker`]'s doc comment), or the
/// `agent_id` never had one.
///
/// 409 when `?id=` names a marker that is no longer the one parked (issue
/// #1846 review, Codex #3866418876): a background turn (a workflow node, an
/// unstreamed task) pausing for the SAME agent re-parks with no chat
/// destination and overwrites the marker the console's chat card was reading
/// from, with no signal the transcript-based staleness check can observe.
/// The console re-reads the live marker (`GET` above) immediately before
/// every redeem and sends its `id` here so this mismatch is caught
/// server-side, atomically, rather than the CTA silently re-dispatching
/// whatever is parked NOW under the assumption it is still what the
/// operator clicked. See [`RedeemMatch`]'s doc for the full reasoning.
async fn redeem_budget_pause(
    company: ScopedCompany,
    Path(AgentPath { agent_id }): Path<AgentPath>,
    Query(RedeemQuery { id }): Query<RedeemQuery>,
) -> Result<Json<BudgetPauseDto>, ApiError> {
    // Issue #1846 review (Codex #3870271005): the SAME durable-lifecycle gate
    // `accept_chat_turn` (`src/server/operator.rs`) runs as its own first
    // line, checked here BEFORE reserving the marker or journaling anything.
    // `run_cycle`/`run_journaled_cycle` only check `ensure_accepting`
    // (process-local quiescing, e.g. mid-rebuild) — never the durable
    // `lifecycle` field a `paused`/`archived` company sets — so without this,
    // clicking a stale "Add credits & resend" CTA on a company an operator
    // had explicitly stopped could still reserve the marker and execute a
    // fresh agent turn, bypassing the exact stop every other write path
    // honours.
    company.runtime.ensure_running().await?;
    let pauses = budget_pauses_for(company.id());
    // Reserved (atomically removed) up front, not merely peeked — see this
    // function's doc comment. A concurrent second request's own `redeem`
    // below finds nothing and 404s before it ever re-dispatches.
    //
    // `?id=` present (every console call site sends it, having just read the
    // marker back via `GET`): reserve only if that id is STILL what's
    // parked — `RedeemMatch::Stale` means a background turn overwrote it
    // since the console last read it, and must not silently redispatch the
    // wrong marker. `?id=` absent: unconditional `redeem`, unchanged from
    // before this fix — for any caller with nothing to compare against.
    let marker = match id {
        Some(expected_id) => match pauses.redeem_matching(&agent_id, &expected_id) {
            RedeemMatch::Reserved(marker) => marker,
            RedeemMatch::Absent => {
                tracing::info!(
                    company = %company.id(),
                    agent = %agent_id,
                    "[budget-pause] redeem requested but nothing is parked — already redeemed, expired with the process, or never paused"
                );
                return Err(OpenCompanyError::NotFound(format!(
                    "no parked budget pause for agent '{agent_id}'"
                ))
                .into());
            }
            RedeemMatch::Stale => {
                tracing::info!(
                    company = %company.id(),
                    agent = %agent_id,
                    expected_id = %expected_id,
                    "[budget-pause] redeem requested a marker that is no longer parked — a newer pause (likely a background turn) has since taken its place; leaving it untouched"
                );
                return Err(OpenCompanyError::Conflict(format!(
                    "the budget pause for agent '{agent_id}' has changed since it was read — refresh and try again"
                ))
                .into());
            }
        },
        None => pauses.redeem(&agent_id).ok_or_else(|| {
            tracing::info!(
                company = %company.id(),
                agent = %agent_id,
                "[budget-pause] redeem requested but nothing is parked — already redeemed, expired with the process, or never paused"
            );
            OpenCompanyError::NotFound(format!("no parked budget pause for agent '{agent_id}'"))
        })?,
    };

    // Issue #1846 review (Codex #3869193112): a marker whose ORIGINAL turn
    // had no chat thread an operator was addressing at all — a dispatched
    // task card or a workflow agent node — must not be redeemed through this
    // generic chat-message path. Replaying it as an `OperatorMessage` would
    // route to the orchestrator instead of the original task/node, leaving
    // the original stuck forever while opening unrelated, possibly duplicate
    // work. See `BudgetPauseMarker::background`'s doc for why this check is
    // NOT the same as the `chat_id.is_none()` case already handled below (an
    // unaddressed interactive message legitimately redeems fine).
    //
    // Restored, not dropped: the reservation above already took it, and a
    // refusal must not silently lose the marker any more than a failed
    // redispatch does — see `restore_if_absent`'s own doc.
    if marker.background {
        tracing::info!(
            company = %company.id(),
            agent = %agent_id,
            marker_id = %marker.id,
            "[budget-pause] redeem refused — this pause happened in a dispatched task or \
             workflow node, which the generic chat-message redeem path cannot resume; leaving \
             it parked"
        );
        pauses.restore_if_absent(marker);
        return Err(OpenCompanyError::InvalidRequest(format!(
            "the budget pause for agent '{agent_id}' happened in a background task or workflow \
             and cannot be resumed from here — investigate the task/workflow run directly"
        ))
        .into());
    }

    tracing::info!(
        company = %company.id(),
        agent = %agent_id,
        marker_id = %marker.id,
        "[budget-pause] redeeming; re-dispatching the original message from the top"
    );
    // Issue #1846 review (Codex #3865812419/#3865812423/#3865812432): replay
    // the ORIGINAL message's thread parent, composer intent, and resolved
    // mentions from the marker, rather than the empty defaults that used to
    // sit here. See `BudgetPauseMarker`'s field docs for what each default
    // silently broke.
    let event = CompanyEvent::OperatorMessage {
        text: marker.message.clone(),
        by: company.actor.clone(),
        chat: marker.chat_id.clone(),
        parent: marker.parent,
        deliverable: marker.deliverable,
        mentions: marker.mentions.clone(),
        // Issue #1846 review (Codex #3866418891): replay the ORIGINAL
        // message's structured attachments, not an empty list. `marker.message`
        // is the raw operator text (see `RedeemContext::text`'s doc), so the
        // model-facing wire body is recomposed fresh from `text` +
        // `attachments` exactly once downstream — replaying an empty list
        // here used to flatten `[Attached file: ...]` marker text into
        // `marker.message` itself, so the rerun journaled the generated
        // block as though the operator had typed it, losing the structured
        // attachment metadata and preview links the console renders from
        // `Attachment` values.
        attachments: marker.attachments.clone(),
    };
    // Issue #1846 review (Codex #3865812411): spawned, not awaited directly
    // in this handler's own future. This host is plain
    // `axum::serve(listener, router(state))`; hyper drops a handler's future
    // the moment the peer disconnects, and a reverse proxy in front of a
    // hosted tenant closes it the moment it decides the upstream is too
    // slow. A direct `.await` here left `restore_if_absent` below
    // unreachable on a drop: the reservation `redeem` took above is gone,
    // `run_cycle` is abandoned mid-flight — tokens spent, side effects
    // possibly already applied, nothing ever re-dispatched to completion —
    // and the operator's saved re-issue payload is lost for good instead of
    // restored for their next click. Same shape and same fix as
    // `spawn_chat_turn` (`src/server/operator.rs`) and
    // `CompanyRuntime::resolve_approval_spawned` use for the ordinary chat
    // and approval paths.
    //
    // Awaiting the `JoinHandle` is drop-safe: dropping it abandons only the
    // *waiting*, so `run_cycle` itself always runs to completion no matter
    // what happens to the request that triggered it.
    //
    // Issue #1846 review (Codex #3866802276): restoration itself must live
    // INSIDE the spawned task, not after `redispatch.await` in this
    // handler's own future. A disconnect during the redispatch drops this
    // whole future — including the `match redispatch.await` below — so the
    // pre-fix code's `restore_if_absent` calls in the `Ok(Err(_))`/`Err(_)`
    // arms never ran on that path: `run_cycle` finished (or panicked) with
    // nobody left polling the `JoinHandle` to see it, and the reservation
    // `redeem`/`redeem_matching` took above was lost for good exactly like
    // the pre-#3865812411 bug this was meant to close. `RestoreGuard` fixes
    // that by owning the restore itself and living entirely inside the
    // spawned future: its `Drop` fires whether that future finishes
    // normally, returns `Err`, or the task panics mid-`run_cycle` — the
    // three ways `run_cycle` can end without earning the marker back —  and
    // is a no-op once `disarm`ed on success. None of that depends on this
    // handler's own future still being polled.
    //
    // Issue #1846 review (Codex #3869369474 / #3870112629 / #3870168362):
    // the journal append, the mention notification, the redispatch AND the
    // journaling of ITS replies all live inside this SAME spawned task now,
    // for the SAME disconnect-safety reason spelled out above — an earlier
    // pass of this fix ran the append+notify synchronously in the handler's
    // own future, BEFORE this spawn, which reintroduced exactly the
    // drop-loses-the-reservation bug #3865812411/#3866802276 closed: a
    // disconnect during that `.await` would abandon the append/notify with
    // no `RestoreGuard` in scope to put the already-reserved marker back.
    // Folding everything into one guarded task closes that the same way
    // `run_cycle` itself was already closed.
    //
    // The reply side (Codex #3870168362): `run_journaled_cycle`'s returned
    // `CycleReport` used to be discarded outright (the `Ok(Ok(_report))` arm
    // below never read it) — unlike `accept_chat_turn`'s callers, which
    // explicitly journal every reply via `journal_chat_replies` after the
    // cycle. Without that, a redeemed turn's answer executed (side effects
    // and all) but never appeared in the transcript or over SSE — the
    // console showed only the OLD marker DTO this route returns, and the
    // reply was invisible until some OTHER event happened to refresh
    // history. `journal_chat_replies` is `pub(crate)` in `operator.rs`
    // specifically for this call — it is the ONE other caller outside that
    // file, and it is what keeps a redeemed reply's journaling on the exact
    // same terms as an ordinary chat reply's (same desk, same mention
    // notification path) rather than a second, drifting implementation.
    let runtime = Arc::clone(&company.runtime);
    let company_id = company.id().clone();
    let actor = company.actor.clone();
    let redispatch = tokio::spawn({
        let pauses = Arc::clone(&pauses);
        let marker = marker.clone();
        async move {
            let mut guard = RestoreGuard::new(pauses, marker.clone());
            let message_seq = runtime.events().append(&company_id, event.clone()).await?;
            if let CompanyEvent::OperatorMessage { mentions, .. } = &event
                && !mentions.is_empty()
            {
                // `marker.chat_id` (folded into `event.chat` above) is
                // `None` for an unaddressed original message, the same
                // "default → orchestrator" thread `accept_chat_turn`'s own
                // `desk` fallback resolves to elsewhere in this file's
                // sibling routes.
                let notify_desk = marker
                    .chat_id
                    .as_deref()
                    .unwrap_or(crate::server::ops::language::DEFAULT_DESK);
                runtime
                    .notify_mentions(
                        &company_id,
                        mentions,
                        &message_seq,
                        actor.as_ref(),
                        notify_desk,
                    )
                    .await;
            }
            // `run_journaled_cycle`, not `run_cycle`: `event` is ALREADY
            // appended above, so handing it to the plain `run_cycle` here
            // would journal it a SECOND time.
            let mut result = runtime
                .run_journaled_cycle(vec![(message_seq, event)], None)
                .await;
            if let Ok(report) = result.as_mut() {
                let desk = marker
                    .chat_id
                    .clone()
                    .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string());
                crate::server::operator::journal_chat_replies(
                    &runtime,
                    &company_id,
                    &desk,
                    marker.parent,
                    report,
                )
                .await;
            }
            if result.is_ok() {
                guard.disarm();
            }
            result
        }
    });
    match redispatch.await {
        Ok(Ok(_report)) => Ok(Json(BudgetPauseDto::from(marker))),
        // The redispatch ran to completion but returned an error — the
        // spawned task's own `RestoreGuard` already restored the reservation
        // (above) so the operator's saved payload survives for a retry,
        // rather than being thrown away over a redispatch that never
        // happened.
        Ok(Err(err)) => Err(err.into()),
        // The spawned task itself panicked (not `run_cycle` returning
        // `Err`) — exactly as "never happened" as an `Err`. `RestoreGuard`
        // restores on this path too: a panic unwinds through the guard's
        // scope inside the spawned task, running its `Drop` before the task
        // finishes, regardless of whether anything is still awaiting this
        // `JoinHandle`.
        Err(join_err) => Err(OpenCompanyError::BackgroundTask(format!(
            "budget-pause redeem's redispatch did not finish: {join_err}"
        ))
        .into()),
    }
}

/// Restores a reserved [`BudgetPauseMarker`] unless explicitly [`disarm`ed](Self::disarm)
/// (issue #1846 review, Codex #3866802276).
///
/// Lives entirely inside the spawned redispatch task (see
/// [`redeem_budget_pause`]'s doc comment on why), so its `Drop` fires from
/// THAT task's own unwind — on an `Err` return, on a panic mid-`run_cycle`,
/// or simply falling off the end of the async block — independent of
/// whether the handler that spawned it is still being polled. Disarmed only
/// on the one outcome that has legitimately spent the reservation: a
/// `run_cycle` that returned `Ok`.
struct RestoreGuard {
    pauses: Arc<BudgetPauseSet>,
    marker: Option<BudgetPauseMarker>,
}

impl RestoreGuard {
    fn new(pauses: Arc<BudgetPauseSet>, marker: BudgetPauseMarker) -> Self {
        Self {
            pauses,
            marker: Some(marker),
        }
    }

    /// Marks the reservation as legitimately spent — `Drop` becomes a no-op.
    fn disarm(&mut self) {
        self.marker = None;
    }
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        if let Some(marker) = self.marker.take() {
            self.pauses.restore_if_absent(marker);
        }
    }
}

#[cfg(test)]
mod tests {
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
        let m = manifest();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new(company);
        store
            .save(&CompanyRecord {
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
    struct FailingRedispatchBrain;

    #[async_trait::async_trait]
    impl crate::ports::brain::Brain for FailingRedispatchBrain {
        async fn run_cycle(
            &self,
            _req: crate::ports::types::CycleRequest,
            _host: &dyn crate::ports::brain::CycleHost,
        ) -> crate::Result<crate::ports::types::CycleResult> {
            Err(OpenCompanyError::InvalidRequest(
                "redispatch refused".to_string(),
            ))
        }
    }

    /// Issue #1846 review (Codex #3865812411): a redispatch that returns
    /// `Err` must restore the reservation `redeem` took, not leave the
    /// operator's saved payload gone for good — the failure branch the
    /// spawn-based fix has to keep reachable.
    #[tokio::test]
    async fn a_failed_redispatch_restores_the_reservation() {
        let home = home();
        let company = "acme-redeem-restore";
        let state = state_with_brain(home.path(), company, Arc::new(FailingRedispatchBrain)).await;
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
        assert!(
            status.is_client_error() || status.is_server_error(),
            "a refused redispatch must not report success: {raw}"
        );

        assert!(
            budget_pauses_for(&id).peek("ceo").is_some(),
            "a failed redispatch must restore the reservation so the operator's saved \
             payload survives for a retry, rather than being thrown away over a redispatch \
             that never happened"
        );
    }

    /// A brain that stalls mid-cycle so the test can drop the connection
    /// while the redispatch is still in flight, then release it and prove
    /// the redispatch ran to completion anyway. Same shape as
    /// `operator.rs`'s `StalledContinuationBrain`.
    struct StalledRedispatchBrain {
        /// Fires once the redispatch cycle is under way — the moment a
        /// dropped connection would have cancelled it under the pre-fix
        /// direct `.await`.
        entered: std::sync::Arc<tokio::sync::Notify>,
        /// The test's permission for the cycle to finish.
        release: std::sync::Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::ports::brain::Brain for StalledRedispatchBrain {
        async fn run_cycle(
            &self,
            req: crate::ports::types::CycleRequest,
            _host: &dyn crate::ports::brain::CycleHost,
        ) -> crate::Result<crate::ports::types::CycleResult> {
            if req
                .events
                .iter()
                .any(|e| matches!(e, CompanyEvent::OperatorMessage { .. }))
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(crate::ports::types::CycleResult {
                channel_responses: Vec::new(),
                new_traces: Vec::new(),
                ledger_deltas: Vec::new(),
                token_usage: crate::ports::types::TokenUsage::default(),
            })
        }
    }

    /// Issue #1846 review (Codex #3865812411) — the keystone test for the
    /// cancellation-safety fix. This host is plain
    /// `axum::serve(listener, router(state))`; hyper drops a handler's
    /// future the moment the peer disconnects, and a reverse proxy in front
    /// of a hosted tenant closes it the moment it decides the upstream is
    /// too slow. Before this fix, `redeem_budget_pause` awaited
    /// `run_cycle` directly in its own future, so that drop cancelled the
    /// redispatch mid-flight: `restore_if_absent` never ran, the reservation
    /// `redeem` took was gone for good, and the operator's saved payload
    /// vanished with no redispatch ever having completed.
    ///
    /// `Router::oneshot` reproduces that drop faithfully rather than by
    /// analogy — same mechanism hyper uses, since the handler future is
    /// owned by the future the caller polls.
    #[tokio::test]
    async fn a_dropped_connection_does_not_cancel_the_redispatch() {
        let home = home();
        let company = "acme-redeem-drop";
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let state = state_with_brain(
            home.path(),
            company,
            Arc::new(StalledRedispatchBrain {
                entered: entered.clone(),
                release: release.clone(),
            }),
        )
        .await;
        let id = CompanyId::new(company);

        budget_pauses_for(&id).park(
            "ceo",
            None,
            "ship the API",
            "paused",
            1_000,
            RedeemContext::default(),
        );

        let uri = "/api/v1/company/agents/ceo/budget-pause/redeem";
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("cookie", crate::server::test_support::fixed_cookie(company))
            .body(Body::empty())
            .unwrap();
        let mut redeeming = Box::pin(router(state.clone()).oneshot(request));
        tokio::select! {
            _ = &mut redeeming => panic!("the redeem answered before the redispatch began"),
            _ = entered.notified() => {}
        }
        drop(redeeming);

        // The reservation is gone — exactly the state a client sees the
        // instant a real proxy gives up mid-redispatch.
        assert!(
            budget_pauses_for(&id).peek("ceo").is_none(),
            "the marker was reserved before the connection dropped"
        );

        // So the redispatch the reservation exists for must still run to
        // completion, not die with the dropped connection.
        release.notify_one();
        let recorded = recording_settles(&id, "ceo").await;
        assert!(
            recorded,
            "the redispatch died with the dropped connection: the reservation is spent and \
             the redispatch never ran to completion"
        );
    }

    /// Polls until the marker for `agent` is gone-and-stays-gone (redeemed
    /// and never restored) or the timeout expires, so the drop test above
    /// does not need a bespoke completion channel through `StalledRedispatchBrain`.
    /// `run_cycle` returns `Ok` on release, so a marker that is STILL absent
    /// after a settle window means the background redispatch ran to
    /// completion without erroring — an error would have restored it.
    async fn recording_settles(id: &CompanyId, agent: &str) -> bool {
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if budget_pauses_for(id).peek(agent).is_none() {
                // Give the spawned task's own `Ok` branch a moment past the
                // notify to finish; then confirm it stayed absent rather than
                // having been an in-between read racing a restore.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                return budget_pauses_for(id).peek(agent).is_none();
            }
        }
        false
    }

    /// Same shape as [`StalledRedispatchBrain`], but its cycle FAILS once
    /// released instead of succeeding — so the spawned redispatch owes a
    /// restore, not just a settle.
    struct StalledThenFailingRedispatchBrain {
        entered: std::sync::Arc<tokio::sync::Notify>,
        release: std::sync::Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::ports::brain::Brain for StalledThenFailingRedispatchBrain {
        async fn run_cycle(
            &self,
            req: crate::ports::types::CycleRequest,
            _host: &dyn crate::ports::brain::CycleHost,
        ) -> crate::Result<crate::ports::types::CycleResult> {
            if req
                .events
                .iter()
                .any(|e| matches!(e, CompanyEvent::OperatorMessage { .. }))
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Err(OpenCompanyError::InvalidRequest(
                "redispatch refused".to_string(),
            ))
        }
    }

    /// Polls until the marker for `agent` is parked again (restored) or the
    /// timeout expires — the mirror image of [`recording_settles`], for a
    /// redispatch that owes a restore rather than a settle.
    async fn recording_restores(id: &CompanyId, agent: &str) -> bool {
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if budget_pauses_for(id).peek(agent).is_some() {
                return true;
            }
        }
        false
    }

    /// Issue #1846 review (Codex #3866802276) — the keystone test for the
    /// guard-in-the-detached-task fix. Combines the drop-safety scenario
    /// [`a_dropped_connection_does_not_cancel_the_redispatch`] proves with a
    /// FAILING redispatch: the connection drops while the redispatch is
    /// in-flight (so nothing is left awaiting `redeem_budget_pause`'s own
    /// `redispatch.await`/its `match` arms), and the redispatch then fails.
    ///
    /// Before this fix, `restore_if_absent` sat in the `Ok(Err(_))` arm of
    /// that `match` — code that lived in THIS handler's own future, which
    /// the drop above already cancelled. The spawned task still ran
    /// `run_cycle` to completion (that half was already fixed by
    /// #3865812411's spawn), but its `Err` reached nobody: the reservation
    /// stayed gone forever, indistinguishable from a successful redeem to
    /// anything reading the marker set afterward. `RestoreGuard` lives
    /// inside the spawned task itself, so its restore does not depend on
    /// this handler's future still being polled.
    #[tokio::test]
    async fn a_dropped_connection_still_restores_the_reservation_when_the_redispatch_fails() {
        let home = home();
        let company = "acme-redeem-drop-then-fail";
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let state = state_with_brain(
            home.path(),
            company,
            Arc::new(StalledThenFailingRedispatchBrain {
                entered: entered.clone(),
                release: release.clone(),
            }),
        )
        .await;
        let id = CompanyId::new(company);

        budget_pauses_for(&id).park(
            "ceo",
            None,
            "ship the API",
            "paused",
            1_000,
            RedeemContext::default(),
        );

        let uri = "/api/v1/company/agents/ceo/budget-pause/redeem";
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("cookie", crate::server::test_support::fixed_cookie(company))
            .body(Body::empty())
            .unwrap();
        let mut redeeming = Box::pin(router(state.clone()).oneshot(request));
        tokio::select! {
            _ = &mut redeeming => panic!("the redeem answered before the redispatch began"),
            _ = entered.notified() => {}
        }
        // Drops `redeem_budget_pause`'s own future — including its
        // `match redispatch.await { ... }` and every restore call that used
        // to live inside it. Nothing is left polling the `JoinHandle`.
        drop(redeeming);

        assert!(
            budget_pauses_for(&id).peek("ceo").is_none(),
            "the marker was reserved before the connection dropped"
        );

        // The spawned task's own `run_cycle` still runs to completion, and
        // now fails.
        release.notify_one();
        let restored = recording_restores(&id, "ceo").await;
        assert!(
            restored,
            "a failed redispatch must restore the reservation even when the connection that \
             triggered it dropped before the redispatch finished — otherwise the operator's \
             saved payload is lost for good with no redispatch ever having completed"
        );
    }
}
