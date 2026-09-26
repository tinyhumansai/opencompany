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
    let desk = crate::server::operator::addressed_or_default_dm(
        &company.runtime,
        marker.chat_id.as_deref(),
    )
    .await
    .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string());
    let event = CompanyEvent::OperatorMessage {
        text: marker.message.clone(),
        by: company.actor.clone(),
        chat: Some(desk.clone()),
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
                runtime
                    .notify_mentions(&company_id, mentions, &message_seq, actor.as_ref(), &desk)
                    .await;
            }
            // `run_journaled_cycle`, not `run_cycle`: `event` is ALREADY
            // appended above, so handing it to the plain `run_cycle` here
            // would journal it a SECOND time.
            let mut result = runtime
                .run_journaled_cycle(vec![(message_seq, event)], None)
                .await;
            if let Ok(report) = result.as_mut() {
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
#[path = "budget_pause_a_failed_redispatch_restores_tests.rs"]
mod tests_a_failed_redispatch_restores;
#[cfg(test)]
#[path = "budget_pause_redeem_replays_the_markers_tests.rs"]
mod tests_redeem_replays_the_markers;
