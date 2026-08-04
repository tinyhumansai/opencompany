//! [`CycleRunner`]: the serial drain → load → think → gate → persist loop.
//!
//! One cycle turns a batch of [`CompanyEvent`]s into a [`CycleReport`]:
//!
//! 1. **Drain** — accept the batched events.
//! 2. **Persist input** — append each event to the log (durable before work).
//! 3. **Load** — recent traces, the context index, and the roster.
//! 4. **Think** — call the brain, servicing its callbacks through a
//!    [`CycleHost`] that gates every emitted effect.
//! 5. **Gate** — inside the host: evaluate, then execute (at-most-once), park,
//!    or deny each effect.
//! 6. **Persist output** — save traces and ledger deltas, meter the cycle's
//!    inference usage, and route channel responses to their adapters.
//!
//! Step 6's metering is the *generic* cost seam: whatever the brain reports as
//! [`CycleResult::token_usage`](crate::ports::types::CycleResult::token_usage)
//! lands on the Usage/Finances surfaces, so hosted Medulla cognition is metered
//! like the openhuman harness instead of reading a blind zero (issue #174).
//!
//! The per-company serial lock is held for the whole cycle, so cycles never
//! interleave within a company while distinct companies stay concurrent.

use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::Result;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::feedback::tool::SEND_EMAIL_TOOL;
use crate::policy::gate::ResolveOutcome;
use crate::ports::brain::{CycleHost, UsageMetering};
use crate::ports::runs::{RunOutcome, RunStatus};
use crate::ports::tasks::{COLUMN_TODO, TaskRecord};
use crate::ports::types::{
    Actor, ApprovalId, CompanyEvent, CompanyId, CompanyRecord, ContextOp, ContextOpResult,
    CycleRequest, Effect, EffectDisposition, EffectGroup, LedgerEntry, OutboundMessage,
    PolicyDecision, TokenUsage, ToolCall, ToolResult, Verdict,
};
use crate::ports::{generate_id, now_millis};
use crate::runtime::channel::OPERATOR_CHANNEL;
use crate::runtime::delegation_tools::{
    DELEGATE_TO_DESK_TOOL, DelegateArgs, SPAWN_TASK_TOOL, SpawnTaskArgs, desk_lead,
    unknown_desk_message,
};
use crate::runtime::grants::GrantedCall;
use crate::runtime::types::CycleReport;
use crate::server::ops::mailer::{MailCredentials, OutboundEmail};

/// How many recent traces to load into a cycle's compressed history.
const HISTORY_LIMIT: usize = 32;

/// The `Effect::kind` for an outbound email send. Shared between where the
/// effect is built (`CycleHostImpl::send_email`, and the workflow delivery path
/// in [`crate::workflows::delivery`]) and where it is executed
/// (`perform_effect`) so they can't drift apart.
///
/// `pub(crate)` because delivery parks an effect this same executor has to
/// recognise on approval: a duplicated `"email.send"` literal over there would
/// park cards that silently do nothing when approved.
pub(crate) const EMAIL_SEND_KIND: &str = "email.send";

/// The `error` the terminality backstop stamps on an attempt row whose cycle
/// ended without settling it (issue #242) — a brain that ignored the dispatch,
/// not a brain that failed at it.
pub(crate) const RUN_UNSETTLED_ERROR: &str =
    "the dispatch cycle ended without settling this attempt";

/// The `error` prefix the backstop stamps when the cycle itself errored, so the
/// row carries the same reason the caller saw rather than a generic one.
pub(crate) const RUN_CYCLE_FAILED_ERROR: &str = "the dispatch cycle failed";

/// Drives cycles for one [`CompanyRuntime`].
pub struct CycleRunner<'a> {
    rt: &'a CompanyRuntime,
}

impl<'a> CycleRunner<'a> {
    /// Binds a runner to a runtime.
    pub fn new(rt: &'a CompanyRuntime) -> Self {
        Self { rt }
    }

    /// Runs one cycle over `events`, holding the per-company serial lock.
    pub async fn run(&self, events: Vec<CompanyEvent>) -> Result<CycleReport> {
        let _guard = self.rt.serial.lock().await;
        self.run_locked(events).await
    }

    async fn run_locked(&self, mut events: Vec<CompanyEvent>) -> Result<CycleReport> {
        let company = self.rt.id.clone();

        // 2. Persist input — durable before any thinking.
        let mut persisted_seq = None;
        let mut event_seqs = Vec::with_capacity(events.len());
        // Issue #242: the attempt rows this cycle is about to run, moved
        // `Pending` → `Running` below and backstopped after the brain returns.
        let mut dispatched_runs: Vec<String> = Vec::new();
        for event in &events {
            let seq = self.rt.events.append(&company, event.clone()).await?;
            event_seqs.push(seq);
            persisted_seq = Some(seq);
            // Start the run here, not inside the brain: the serial lock is held,
            // the driving event's seq now exists, and every brain — harness,
            // hosted, echo — passes through this one place. A brain that ignores
            // `TaskDispatched` entirely still leaves a correctly-started row for
            // the backstop below to settle.
            if let CompanyEvent::TaskDispatched {
                run_id: Some(run_id),
                ..
            } = event
            {
                match self.rt.runs().begin_run(&company, run_id, seq).await {
                    Ok(_) => dispatched_runs.push(run_id.clone()),
                    // Not fatal, and not silent. The row may be missing (its
                    // `create_run` failed at the choke point and the dispatch
                    // proceeded anyway) or already past `Pending` (a replayed
                    // event). Either way the work still runs — record-keeping
                    // does not fail the work it records — but the run is not
                    // tracked as this cycle's, so the backstop leaves it alone.
                    Err(err) => tracing::warn!(
                        company = %company,
                        run = %run_id,
                        error = %err,
                        "[runs] could not start an attempt row; the cycle runs untracked"
                    ),
                }
            }
        }

        // 3. Load — history, context index, roster.
        let compressed_history = self
            .rt
            .memory
            .recent_traces(&company, HISTORY_LIMIT)
            .await?;
        let context_index = self.rt.context.list(&company, "").await?;
        let record = self.rt.store.load(&company).await?;
        let roster = match &record {
            Some(record) => record
                .manifest
                .agents
                .iter()
                .map(|agent| agent.id.clone())
                .collect(),
            None => Vec::new(),
        };

        // Issue #176 (handed-task awareness): when an operator message is
        // addressed to a desk/agent that already has open work handed to it,
        // fold a briefing of that work into the message the brain sees — so a
        // direct "what are you working on?" surfaces the handed task truthfully.
        // Brain-agnostic (both brains read `req.events`); mutates only the
        // in-memory copy handed to the brain, never the durable log persisted
        // above.
        if let Some(record) = &record {
            self.inject_handed_task_awareness(record, &mut events).await;
        }

        let cycle_id = crate::ports::generate_id();
        let request = CycleRequest {
            cycle_id: cycle_id.clone(),
            company_id: company.clone(),
            events,
            event_seqs,
            compressed_history,
            roster,
            context_index,
        };

        // 4. Think + 5. Gate — the host services callbacks and gates effects.
        let host = CycleHostImpl::new(company.clone(), cycle_id.clone(), self.rt);
        let result = self.rt.brain.run_cycle(request, &host).await;
        // Issue #242: the terminality backstop. Whatever the brain did — settled
        // the run richly (the harness path), ignored `TaskDispatched` entirely
        // (the echo brain), or errored out — no attempt row may be left claiming
        // to be live once the cycle that owned it is over. Runs deliberately
        // BEFORE the `?` so a brain error settles its rows too; the only path
        // that escapes it is a panic, which is the boot reaper's job.
        self.backstop_dispatched_runs(&company, &dispatched_runs, result.as_ref().err())
            .await;
        let result = result?;

        // 6. Persist output.
        for trace in &result.new_traces {
            self.rt.memory.save_trace(&company, trace.clone()).await?;
        }
        for delta in &result.ledger_deltas {
            self.rt.store.append_ledger(&company, delta.clone()).await?;
        }
        // 6b. Meter what the cycle's thinking cost. This is the *generic* seam, so
        // every brain that reports usage is metered — before issue #174 only the
        // openhuman harness metered (per turn, through its own hook) and the
        // hosted/sidecar paths dropped `CycleResult.token_usage` on the floor,
        // leaving the Usage view at a blind zero. A brain that meters itself
        // reports zero here (see `HarnessBrain`), so nothing is counted twice.
        self.record_cycle_usage(&company, &result.token_usage).await;
        for response in &result.channel_responses {
            self.route_response(response).await?;
        }

        // 6c. Issue #243: journal every grant this cycle's turns redeemed.
        //
        // Redemption happens inside `ToolPolicy::check`, which is sync and holds
        // no journal handle, so the id is buffered on the grant set and written
        // here — after the cycle it belongs to. Best-effort and logged rather
        // than propagated: the tool has already run by this point, so failing the
        // cycle over the bookkeeping write would discard real model output to
        // record something whose only consequence is that a restart might re-arm
        // a spent grant (which then re-asks the operator — the safe direction).
        for id in self.rt.grants.drain_consumed() {
            if let Err(err) = self.rt.journal.record_grant_consumed(&id).await {
                tracing::warn!(
                    approval_id = %id,
                    error = %err,
                    "[approval] a grant was redeemed but its journal record failed; \
                     a restart before it is re-written may re-arm it"
                );
            }
        }

        let (executed_effects, parked) = host.into_outcomes();
        Ok(CycleReport {
            cycle_id,
            responses: result.channel_responses,
            executed_effects,
            parked,
            persisted_seq,
        })
    }

    /// Settles any attempt row this cycle started that is *still* claiming to be
    /// live (issue #242) — the terminality backstop.
    ///
    /// On the ordinary harness path this is a no-op: `run_task` settles the run
    /// richly (status, cost, step count, failure reason) and returns before
    /// `run_locked` gets here, so every row is already terminal or parked and
    /// [`RunStatus::is_active`] is false. The backstop exists for the paths that
    /// produce no rich settle at all:
    ///
    /// * a brain that ignores `TaskDispatched` (the default build's `EchoBrain`,
    ///   an injected test brain) — the row would otherwise sit `Running` until
    ///   the next boot reaped it;
    /// * a brain that **errored**, which is why this runs before the `?`.
    ///
    /// Best-effort per row, never propagated: the cycle either produced output
    /// the operator can already see or failed for a reason worth surfacing, and
    /// neither should be replaced by a bookkeeping error.
    ///
    /// A panic still escapes it — that is deliberately the boot reaper's job
    /// ([`reap_orphaned_runs`](crate::ports::runs::reap_orphaned_runs)), since a
    /// panicking cycle cannot run its own cleanup by definition.
    async fn backstop_dispatched_runs(
        &self,
        company: &CompanyId,
        run_ids: &[String],
        cycle_error: Option<&OpenCompanyError>,
    ) {
        for id in run_ids {
            let run = match self.rt.runs().get_run(company, id).await {
                Ok(Some(run)) => run,
                // Vanished between `begin_run` and here — nothing to settle.
                Ok(None) => continue,
                Err(err) => {
                    tracing::warn!(
                        company = %company,
                        run = %id,
                        error = %err,
                        "[runs] could not read an attempt row for the terminality backstop"
                    );
                    continue;
                }
            };
            if !run.is_active() {
                // The rich settle already happened (or the run parked). Leaving
                // a parked run alone is the point: `Paused` / `WaitingApproval`
                // are waiting on something outside the cycle, not stranded by it.
                continue;
            }
            let reason = match cycle_error {
                Some(err) => format!("{RUN_CYCLE_FAILED_ERROR}: {err}"),
                None => RUN_UNSETTLED_ERROR.to_string(),
            };
            let outcome = RunOutcome::new(RunStatus::Failed).with_error(reason);
            if let Err(err) = self.rt.runs().finish_run(company, id, outcome).await {
                tracing::warn!(
                    company = %company,
                    run = %id,
                    error = %err,
                    "[runs] the terminality backstop could not settle an attempt row"
                );
            }
        }
    }

    /// Meters a finished cycle's inference usage onto the Usage + Finances
    /// surfaces, attributed to the brain's own provider slug (issue #174).
    ///
    /// A zero-usage cycle writes nothing, which covers the idle cycle, the
    /// offline echo brain, and the openhuman harness — the harness meters each
    /// turn as it runs and deliberately reports zero here, so its spend is never
    /// double-counted. Both non-`PerCycle` declarations are also enforced
    /// directly, so a path that reports usage against its own contract is warned
    /// about and dropped rather than trusted.
    ///
    /// Accounting never fails the cycle it accounts for: the write is
    /// logged-and-swallowed inside
    /// [`record_inference_usage`](crate::metering::record_inference_usage), so a
    /// meter fault cannot undo model output the operator can already see.
    async fn record_cycle_usage(&self, company: &CompanyId, usage: &TokenUsage) {
        if usage.is_zero() {
            return;
        }
        let cognition = self.rt.brain.cognition();
        // Both non-`PerCycle` arms declare "do not meter me here", so both are
        // enforced. Leaving `None` to fall through would have metered a brain
        // that runs no model at all under its own slug — the echo brain would
        // post a `provider: "none"` row into `byProvider`.
        match cognition.metering {
            UsageMetering::PerTurn => {
                // Defensive: a self-metering path should report zero. If one ever
                // reports usage too, drop it here rather than charge it twice, and
                // say so loudly.
                tracing::warn!(
                    company = %company,
                    path = %cognition.path,
                    input = usage.input,
                    output = usage.output,
                    "[usage] a per-turn-metered brain also reported cycle usage; ignoring it to avoid double-counting"
                );
                return;
            }
            UsageMetering::None => {
                tracing::warn!(
                    company = %company,
                    path = %cognition.path,
                    input = usage.input,
                    output = usage.output,
                    "[usage] a brain that declares it runs no model reported cycle usage; ignoring it — \
                     the path's Cognition::metering is wrong, or it grew a real model call"
                );
                return;
            }
            UsageMetering::PerCycle => {}
        }
        crate::metering::record_inference_usage(
            usage,
            crate::metering::UNATTRIBUTED_AGENT,
            cognition.provider,
            company,
            self.rt.store.as_ref(),
            self.rt.usage().as_ref(),
        )
        .await;
    }

    /// Folds a briefing of open handed work into any operator message addressed
    /// to a desk/agent that owns it (issue #176, handed-task awareness). Reads
    /// open task cards once and appends, to each addressed `OperatorMessage`,
    /// the cards whose assignee resolves to the addressed target — so a direct
    /// "what are you working on?" is answered truthfully. A no-op when nothing
    /// is addressed or no open work matches. Mutates only the in-memory events
    /// handed to the brain, never the durable event log.
    async fn inject_handed_task_awareness(
        &self,
        record: &CompanyRecord,
        events: &mut [CompanyEvent],
    ) {
        // Cheap exit before touching the task store: nothing is addressed.
        if !events
            .iter()
            .any(|e| matches!(e, CompanyEvent::OperatorMessage { chat: Some(_), .. }))
        {
            return;
        }
        let cards = self.rt.tasks().list(&self.rt.id).await.unwrap_or_default();
        let open: Vec<&TaskRecord> = cards
            .iter()
            .filter(|c| c.column != "done" && !c.assignee.trim().is_empty())
            .collect();
        if open.is_empty() {
            return;
        }
        for event in events.iter_mut() {
            let CompanyEvent::OperatorMessage {
                text,
                chat: Some(target),
                ..
            } = event
            else {
                continue;
            };
            let mut lines: Vec<String> = open
                .iter()
                .filter(|c| assignment_matches(record, target.as_str(), &c.assignee))
                .map(|c| match &c.note {
                    Some(note) if !note.trim().is_empty() => {
                        format!("- {} — {}", c.title, first_line(note, 120))
                    }
                    _ => format!("- {}", c.title),
                })
                .collect();
            if lines.is_empty() {
                continue;
            }
            lines.sort();
            text.push_str(&format!(
                "\n\n[Open work already handed to you (answer truthfully if asked what you are \
working on):\n{}\n]",
                lines.join("\n")
            ));
        }
    }

    /// Resolves a parked approval, executes the effect on approval, and runs a
    /// follow-up cycle feeding the resolution back to the brain.
    ///
    /// Resolving an approval that is **not parked** — an unknown id, or one a
    /// concurrent request already resolved — is a no-op that answers with a
    /// fixed line (issue #243). It writes no journal record and runs no cycle.
    ///
    /// Before this the double-submit path was indistinguishable from a deny (see
    /// [`ResolveOutcome`]), so a double-clicked approve appended a second
    /// `ApprovalResolved` to the journal and ran a second follow-up cycle over an
    /// approval that no longer existed — burning a model turn to tell the brain
    /// about a resolution it had already been told about.
    pub async fn resolve_approval(
        &self,
        id: &ApprovalId,
        verdict: Verdict,
        by: Actor,
    ) -> Result<CycleReport> {
        let outcome = self
            .rt
            .approval_gate
            .resolve_outcome(id, verdict, by.clone(), now_millis());
        if outcome == ResolveOutcome::NotParked {
            return Ok(self.already_resolved_report());
        }
        self.rt.journal.record_resolved(id).await?;
        if let ResolveOutcome::Approved(effect) = outcome {
            self.settle_approved_effect(id, effect).await?;
        }
        // Follow-up cycle so the brain learns the verdict. Appending the
        // resolution here (rather than separately) keeps the event logged once.
        self.run(vec![CompanyEvent::ApprovalResolved {
            approval_id: id.clone(),
            verdict,
            by,
        }])
        .await
    }

    /// Applies an approved effect: **mint a grant** when it came from a harness
    /// tool call, **execute it** when it is native (issue #243).
    ///
    /// This is the fork the whole feature turns on, and it is decided by
    /// [`Effect::agent`], which only
    /// [`ApprovalPolicy::effect_for`](crate::harness::policy::ApprovalPolicy::effect_for)
    /// ever stamps:
    ///
    /// * **`None` — native.** Unchanged, byte for byte: `execute_effect_once`
    ///   under the `approval:<id>` key. Emails, workflow deliveries and Medulla
    ///   effect frames keep their at-most-once path exactly as before.
    /// * **`Some(agent)` — a harness tool call.** Executing it would be
    ///   meaningless: the payload is a tool's *arguments*, and `perform_effect`
    ///   would ledger a phantom spend and route nothing. Worse, it would look
    ///   like success while the tool never ran. So the effect is deliberately
    ///   NOT executed; a single-use grant is minted instead, and the brain's
    ///   `ApprovalResolved` arm re-dispatches the agent to re-issue the call for
    ///   real.
    ///
    /// The journal record is written **before** the grant enters the live set.
    /// A crash between the two therefore replays as "granted", re-arming it —
    /// the safe direction. The reverse order would lose the operator's approval
    /// entirely on a crash, and the agent would come back asking for a
    /// permission it had already been given.
    async fn settle_approved_effect(&self, id: &ApprovalId, effect: Effect) -> Result<()> {
        let Some(agent) = effect.agent.clone() else {
            let key = format!("approval:{id}");
            return execute_effect_once(self.rt, &key, &effect).await;
        };
        self.mint_grant(id, agent, effect).await
    }

    /// Journals then arms a single-use grant for `(agent, effect.kind,
    /// effect.payload)`.
    async fn mint_grant(&self, id: &ApprovalId, agent: String, effect: Effect) -> Result<()> {
        let grant = GrantedCall {
            approval_id: id.clone(),
            agent,
            tool: effect.kind.clone(),
            // The parked effect's payload IS the tool's argument object — see
            // `effect_for`. Granting against it verbatim is what makes the
            // policy's match "the exact call the operator saw".
            args: effect.payload.clone(),
            at_millis: now_millis(),
        };
        self.rt.journal.record_granted(&grant).await?;
        self.rt.grants.grant(grant);
        tracing::debug!(
            approval_id = %id,
            tool = %effect.kind,
            "[approval] minted a single-use grant; the agent will re-issue the call"
        );
        Ok(())
    }

    /// The deterministic answer to resolving an approval that is already gone.
    ///
    /// Synthetic on purpose: no events, no effects, nothing parked, and a
    /// `persisted_seq` of `None` — the caller gets a well-formed report saying
    /// "nothing happened" instead of an error, because from the operator's side
    /// a double-submit is not a failure, it is a request whose work was already
    /// done.
    fn already_resolved_report(&self) -> CycleReport {
        CycleReport {
            cycle_id: generate_id(),
            responses: vec![OutboundMessage {
                task_id: None,
                channel: OPERATOR_CHANNEL.to_string(),
                text: "This approval was already resolved.".to_string(),
                steps: Vec::new(),
                reply_to: None,
            }],
            executed_effects: Vec::new(),
            parked: Vec::new(),
            persisted_seq: None,
        }
    }

    /// Resolves a parked approval to an operator-amended effect
    /// (approve-with-edit): overlays `amended_payload` onto the parked effect,
    /// executes the amended version (at-most-once), and runs a follow-up cycle.
    ///
    /// Both the original and the amended effect are preserved in the immutable
    /// journal (`ApprovalParked` + `ApprovalAmended`), so the audit trail shows
    /// what the brain requested and what the operator approved.
    pub async fn resolve_approval_amended(
        &self,
        id: &ApprovalId,
        amended_payload: serde_json::Value,
        by: Actor,
    ) -> Result<CycleReport> {
        let now = now_millis();

        // Overlay the operator's edit onto the parked effect. A missing id (or
        // an expired one, caught by the gate below) yields no executable effect.
        let amended = self.rt.approval_gate.parked_effect(id).map(|mut original| {
            original.payload = overlay_payload(original.payload, amended_payload);
            original
        });
        let executed = match amended {
            Some(effect) => self
                .rt
                .approval_gate
                .resolve_amended(id, effect, by.clone(), now),
            None => None,
        };

        // Audit the amendment (when one ran) and drain the queue durably.
        if let Some(effect) = &executed {
            self.rt.journal.record_amended(id, effect, now).await?;
        }
        self.rt.journal.record_resolved(id).await?;

        // Issue #243: same fork as the plain approve — a harness tool call mints
        // a grant instead of executing. Crucially the grant is minted against the
        // **amended** arguments, so what the policy will admit is what the
        // operator actually approved. Granting the original would let the agent
        // re-issue the very call the operator edited, silently discarding the
        // edit — which is worse than not supporting amend at all, because the
        // operator would have every reason to believe their change took effect.
        if let Some(effect) = &executed {
            self.settle_approved_effect(id, effect.clone()).await?;
        }

        // Follow-up cycle so the brain learns the approval resolved (with an
        // edit). `CompanyEvent` is closed, so the verdict rides as `Approve`;
        // the edit itself lives in the journal audit trail.
        self.run(vec![CompanyEvent::ApprovalResolved {
            approval_id: id.clone(),
            verdict: Verdict::Approve,
            by,
        }])
        .await
    }

    /// Replays the journal to rebuild the executed-key set, the approval queue,
    /// and the live grant set.
    ///
    /// The grant window spans a model turn, so a deploy or a crash inside it is
    /// ordinary rather than exotic. Without this seeding, an operator's approval
    /// would evaporate across a restart and the agent would come back asking for
    /// a permission it had just been given. Consumed and expired grants are
    /// folded out during replay, so this can only ever re-arm one that never
    /// fired.
    pub async fn recover(&self) -> Result<()> {
        self.rt.journal.load().await?;
        self.rt.grants.rehydrate(self.rt.journal.replayed_grants());
        Ok(())
    }

    async fn route_response(&self, msg: &OutboundMessage) -> Result<()> {
        for channel in &self.rt.channels {
            if channel.channel_id() == msg.channel {
                channel.send(msg.clone()).await?;
                return Ok(());
            }
        }
        // Issue #151: an agent reply is addressed by *agent id*, not by adapter
        // id — a delegated desk bubble and a dispatched card's post-back both
        // carry `channel: "<agent_id>"` so the console can attribute them. No
        // adapter answers to an agent id, so this used to drop them silently:
        // the operator REST route reads `CycleReport.responses` directly and
        // never noticed, but a company reached over a real channel adapter got
        // the orchestrator's reply and lost every delegated one.
        //
        // Fall back to the operator adapter, which is the console's own surface
        // and always the right destination for an agent→human reply. The
        // message is forwarded unchanged, so its `channel` still names the agent
        // and attribution survives.
        if let Some(operator) = self
            .rt
            .channels
            .iter()
            .find(|c| c.channel_id() == OPERATOR_CHANNEL)
        {
            tracing::debug!(
                channel = %msg.channel,
                "no adapter for this channel id; delivering via the operator channel"
            );
            operator.send(msg.clone()).await?;
            return Ok(());
        }
        // Nothing to deliver on at all (a runtime with no operator adapter).
        tracing::debug!(
            channel = %msg.channel,
            "no adapter for this channel id and no operator channel; response not delivered"
        );
        Ok(())
    }
}

/// Overlays an operator's payload edit onto the original effect payload.
///
/// When both are JSON objects the top-level keys are merged (the edit wins);
/// otherwise the edit replaces the original wholesale. An operator can thus
/// tweak individual fields (e.g. lower an amount) without restating the payload.
fn overlay_payload(original: serde_json::Value, edit: serde_json::Value) -> serde_json::Value {
    match (original, edit) {
        (serde_json::Value::Object(mut base), serde_json::Value::Object(over)) => {
            for (key, value) in over {
                base.insert(key, value);
            }
            serde_json::Value::Object(base)
        }
        (_, edit) => edit,
    }
}

/// Executes an effect at most once, keyed by `key`.
///
/// The key is committed to the journal *before* the side effect runs, so a
/// crash after the commit drops the effect rather than repeating it — the
/// at-most-once durability guarantee.
pub(crate) async fn execute_effect_once(
    rt: &CompanyRuntime,
    key: &str,
    effect: &Effect,
) -> Result<()> {
    if rt.journal.is_executed(key) {
        return Ok(());
    }
    rt.journal.record_executed(key).await?;
    perform_effect(rt, effect).await
}

/// The Phase-1 effect executor: record spend to the ledger and route any
/// message payload to its channel. Richer effect kinds land in later phases.
async fn perform_effect(rt: &CompanyRuntime, effect: &Effect) -> Result<()> {
    if let Some(amount) = effect.amount_usd {
        rt.store
            .append_ledger(
                &rt.id,
                LedgerEntry {
                    at_millis: now_millis(),
                    kind: effect.kind.clone(),
                    amount_usd: amount,
                    memo: format!("effect {}", effect.kind),
                },
            )
            .await?;
    }
    if let (Some(channel), Some(text)) = (
        effect.payload.get("channel").and_then(|v| v.as_str()),
        effect.payload.get("text").and_then(|v| v.as_str()),
    ) {
        for adapter in &rt.channels {
            if adapter.channel_id() == channel {
                adapter
                    .send(OutboundMessage {
                        task_id: None,
                        channel: channel.to_string(),
                        text: text.to_string(),
                        steps: Vec::new(),
                        reply_to: None,
                    })
                    .await?;
                break;
            }
        }
    }
    if effect.kind == EMAIL_SEND_KIND {
        let to = effect
            .payload
            .get("to")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let subject = effect
            .payload
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let body = effect
            .payload
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        send_company_email(rt, to, subject, body).await?;
    }
    Ok(())
}

/// Sends an `email.send` effect via the company's own outbound-mail handle
/// and records the send to the sender's own inbox (so the console shows
/// outbound mail alongside inbound).
async fn send_company_email(
    rt: &CompanyRuntime,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<()> {
    let Some(mail) = rt.mail() else {
        return Err(OpenCompanyError::InvalidRequest(
            "email is not configured for this company".into(),
        ));
    };
    let email = OutboundEmail {
        to: to.to_string(),
        subject: subject.to_string(),
        body: body.to_string(),
    };
    mail.sender
        .send(&MailCredentials::Smtp(mail.smtp.clone()), &email)
        .await?;
    // Record to the sender's own inbox (from = the company's own address).
    crate::server::ops::smtp::record_outbound(rt, &mail.smtp, &email).await;
    Ok(())
}

/// The company's own outbound-mail address, or empty when no mail is
/// configured for this company.
fn company_address(rt: &CompanyRuntime) -> String {
    rt.mail()
        .map(|mail| mail.smtp.from_email.clone())
        .unwrap_or_default()
}

/// True iff the company's inbox already holds a prior **inbound** email from
/// `to` — an established thread, so replying is auto-allowed instead of
/// parking for approval. Fails closed (`false`) on a missing mail handle or a
/// store error, which routes the caller to the cold-recipient park path.
///
/// Delegates the lookup to [`has_inbound_from`](crate::ports::InboxStore::has_inbound_from)
/// rather than scanning a page of
/// [`messages`](crate::ports::InboxStore::messages): this answer decides
/// an approval gate, and a gate built on a capped oldest-first page silently
/// stops finding real correspondents once the inbox outgrows the cap — past
/// that point every reply parks, and an approval queue full of legitimate mail
/// is one operators learn to rubber-stamp (issue #232).
async fn recipient_is_established(rt: &CompanyRuntime, to: &str) -> bool {
    let address = company_address(rt);
    if address.is_empty() {
        return false;
    }
    let key = crate::server::ops::smtp::local_part(&address);
    rt.inbox()
        .has_inbound_from(rt.id(), &key, to)
        .await
        .unwrap_or(false) // fail closed → parks for approval
}

/// The host the brain calls back into mid-cycle. Bridges tool, context, and
/// effect callbacks to the runtime's ports and gates every effect.
struct CycleHostImpl<'a> {
    company: CompanyId,
    cycle_id: String,
    rt: &'a CompanyRuntime,
    counter: AtomicU64,
    executed: StdMutex<Vec<Effect>>,
    parked: StdMutex<Vec<ApprovalId>>,
}

impl<'a> CycleHostImpl<'a> {
    fn new(company: CompanyId, cycle_id: String, rt: &'a CompanyRuntime) -> Self {
        Self {
            company,
            cycle_id,
            rt,
            counter: AtomicU64::new(0),
            executed: StdMutex::new(Vec::new()),
            parked: StdMutex::new(Vec::new()),
        }
    }

    fn into_outcomes(self) -> (Vec<Effect>, Vec<ApprovalId>) {
        (
            self.executed.into_inner().expect("executed poisoned"),
            self.parked.into_inner().expect("parked poisoned"),
        )
    }

    /// Evaluates an effect against policy and either executes it (at-most-once),
    /// parks it for approval, or denies it. Shared by `emit_effect` and the
    /// `send_email` tool interception.
    async fn gate_effect(&self, effect: Effect) -> Result<EffectDisposition> {
        match self.rt.approvals.evaluate(&self.company, &effect).await? {
            PolicyDecision::Allow => {
                let idx = self.counter.fetch_add(1, Ordering::Relaxed);
                let key = format!("{}:{idx}", self.cycle_id);
                execute_effect_once(self.rt, &key, &effect).await?;
                self.executed
                    .lock()
                    .expect("executed poisoned")
                    .push(effect);
                Ok(EffectDisposition::Executed)
            }
            PolicyDecision::RequireApproval => {
                Ok(EffectDisposition::PendingApproval(self.park(effect).await?))
            }
            PolicyDecision::Deny => Ok(EffectDisposition::Denied {
                reason: format!("policy denied {}", effect.kind),
            }),
        }
    }

    /// Parks `effect` on the approval gate, journals it durably, and records the
    /// id on this cycle's outcome.
    ///
    /// The single write path into the operator's approval queue: the
    /// `RequireApproval` arm of [`gate_effect`](Self::gate_effect) and the
    /// already-decided [`CycleHost::park_effect`] callback both land here, so a
    /// parked effect is journaled exactly one way and survives a restart with its
    /// original [`ApprovalId`] regardless of who decided it.
    async fn park(&self, effect: Effect) -> Result<ApprovalId> {
        let approval_id = self
            .rt
            .approvals
            .park(&self.company, effect.clone())
            .await?;
        self.rt
            .journal
            .record_parked(&approval_id, &effect, now_millis())
            .await?;
        self.parked
            .lock()
            .expect("parked poisoned")
            .push(approval_id.clone());
        tracing::debug!(
            kind = %effect.kind,
            group = ?effect.group,
            approval_id = %approval_id,
            cycle = %self.cycle_id,
            "[cycle] parked effect for operator approval"
        );
        Ok(approval_id)
    }

    /// Intercepts the `send_email` tool: parses `to`/`subject`/`body`, checks
    /// whether the recipient is an established thread, and routes the result
    /// through the effect gate as an `email.send` effect rather than invoking
    /// the tool provider directly.
    async fn send_email(&self, args: serde_json::Value) -> Result<ToolResult> {
        if self.rt.mail().is_none() {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({ "error": "email is not configured for this company" }),
            });
        }
        let get = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let (Some(to), Some(subject), Some(body)) = (get("to"), get("subject"), get("body")) else {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({ "error": "send_email requires to, subject, body" }),
            });
        };
        if to.trim().is_empty() {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({ "error": "recipient (to) is empty" }),
            });
        }
        let established = recipient_is_established(self.rt, &to).await;
        let effect = Effect {
            kind: EMAIL_SEND_KIND.into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: established,
            first_time_counterparty: !established,
            payload: serde_json::json!({ "to": to, "subject": subject, "body": body }),
            agent: None,
            run_id: None,
        };
        match self.gate_effect(effect).await? {
            EffectDisposition::Executed => Ok(ToolResult {
                ok: true,
                output: serde_json::json!({ "status": "sent" }),
            }),
            EffectDisposition::PendingApproval(id) => Ok(ToolResult {
                ok: true,
                output: serde_json::json!({ "status": "pending_approval", "approval_id": id.as_ref() }),
            }),
            EffectDisposition::Denied { reason } => Ok(ToolResult {
                ok: false,
                output: serde_json::json!({ "status": "denied", "reason": reason }),
            }),
        }
    }

    /// Services the `spawn_task` tool (issue #176): opens a tracked task card on
    /// the company's board through the same [`TaskStore`](crate::ports::TaskStore)
    /// path the console and the harness path use. A blank title is a clean tool
    /// error rather than a silent no-op. The card is durable, so a later direct
    /// query to its assignee surfaces it (handed-task awareness).
    async fn spawn_task(&self, args: serde_json::Value) -> Result<ToolResult> {
        let Some(parsed) = SpawnTaskArgs::parse(&args) else {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({ "error": "spawn_task requires a non-empty title" }),
            });
        };
        let card = TaskRecord {
            id: generate_id(),
            title: parsed.title.clone(),
            note: parsed.note,
            column: COLUMN_TODO.to_string(),
            priority: "medium".to_string(),
            assignee: parsed.assignee.unwrap_or_default(),
            updated_at_millis: now_millis(),
            origin_chat_id: None,
            // No parent (#185), for the same reason as the harness path: this
            // is a chat-turn delegation, so no task is in scope to be the
            // parent. Lineage is set through the task API's `parentTaskId`.
            parent_task_id: None,
        };
        self.rt.tasks().upsert(&self.company, &card).await?;
        Ok(ToolResult {
            ok: true,
            output: serde_json::json!({
                "status": "queued",
                "task_id": card.id,
                "title": parsed.title,
            }),
        })
    }

    /// Services the `delegate_to_desk` tool (issue #176) on the hosted path: a
    /// *durable, asynchronous* hand-off. Resolves the target desk, writes a task
    /// card assigned to that desk (so a later direct query to the desk surfaces
    /// the handed work), and returns a summary the remote cognition relays to
    /// the operator.
    ///
    /// This deliberately does NOT run the desk lead's turn: a hosted build has
    /// no in-process cognition pool. The synchronous, one-voice relay the
    /// harness performs needs Medulla multi-agent support and is tracked in
    /// #176; the durable hand-off is the brain-agnostic capability that ships
    /// now. An unknown desk is a clean tool error, not a lost hand-off.
    async fn delegate_to_desk(&self, args: serde_json::Value) -> Result<ToolResult> {
        let Some(parsed) = DelegateArgs::parse(&args) else {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({
                    "error": "delegate_to_desk requires a desk and an instruction"
                }),
            });
        };
        let record = self.rt.store.load(&self.company).await?;
        let Some(desk_id) = record
            .as_ref()
            .and_then(|r| r.resolve_desk_id(&parsed.desk))
        else {
            // Issue #272: the refusal now carries the company's real desk ids
            // (and, when the invented target names a teammate, the desk that
            // teammate is on), so the remote cognition can correct itself in the
            // same turn rather than only learning that its pick was wrong. The
            // message is the one the harness tool's boundary check uses, so the
            // two paths cannot drift.
            //
            // Only the *unknown* desk is refused here. A real desk with no
            // roster lead is left alone on this path: the hosted hand-off is a
            // durable card assigned to the desk, which is visible on the board
            // whether or not anyone leads it yet — there is nothing silent
            // about it.
            let error = match record.as_ref() {
                Some(record) => unknown_desk_message(record, &parsed.desk),
                None => format!("no desk matches \"{}\"", parsed.desk),
            };
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({
                    "status": "unknown_desk",
                    "error": error,
                }),
            });
        };
        // The desk's lead, when it has a roster-backed one, is recorded in the
        // note; the card is assigned to the DESK so an operator asking the desk
        // directly (chat targets the desk) sees the hand-off.
        let lead = record.as_ref().and_then(|r| desk_lead(r, &parsed.desk));
        let note = match &lead {
            Some(member) => format!(
                "Delegated to the {desk_id} desk (lead: {member}).\n\n{instruction}",
                instruction = parsed.instruction
            ),
            None => format!(
                "Delegated to the {desk_id} desk (no lead member on the roster yet).\n\n{instruction}",
                instruction = parsed.instruction
            ),
        };
        let card = TaskRecord {
            id: generate_id(),
            title: first_line(&parsed.instruction, 80),
            note: Some(note),
            column: COLUMN_TODO.to_string(),
            priority: "medium".to_string(),
            assignee: desk_id.clone(),
            updated_at_millis: now_millis(),
            origin_chat_id: None,
            // No parent (#185), for the same reason as the harness path: this
            // is a chat-turn delegation, so no task is in scope to be the
            // parent. Lineage is set through the task API's `parentTaskId`.
            parent_task_id: None,
        };
        self.rt.tasks().upsert(&self.company, &card).await?;
        Ok(ToolResult {
            ok: true,
            output: serde_json::json!({
                "status": "handed_off",
                "desk": desk_id,
                "lead": lead,
                "task_id": card.id,
            }),
        })
    }
}

/// The first non-empty line of `text`, trimmed and capped to `max` chars — the
/// task-card title derived from a delegation instruction (which may be a whole
/// paragraph). Falls back to a short cap of the whole string when there is no
/// line break. UTF-8-safe: never slices mid-codepoint.
fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(text)
        .trim();
    match line.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &line[..idx]),
        None => line.to_string(),
    }
}

/// Whether a task card assigned to `assignee` counts as "handed to" the target
/// a direct operator message is addressed to (issue #176). Matches when the two
/// are the same string (case-insensitively), resolve to the same desk, or the
/// assignee is the addressed desk's lead — so a hand-off recorded against a desk
/// id surfaces when the operator addresses that desk by id or name, and a card
/// assigned to a person surfaces when that person is addressed.
fn assignment_matches(record: &CompanyRecord, target: &str, assignee: &str) -> bool {
    if assignee.eq_ignore_ascii_case(target) {
        return true;
    }
    if let (Some(a), Some(b)) = (
        record.resolve_desk_id(target),
        record.resolve_desk_id(assignee),
    ) && a == b
    {
        return true;
    }
    if let Some(lead) = desk_lead(record, target) {
        return lead.eq_ignore_ascii_case(assignee);
    }
    false
}

#[async_trait]
impl CycleHost for CycleHostImpl<'_> {
    async fn call_tool(&self, call: ToolCall) -> Result<ToolResult> {
        if call.tool == SEND_EMAIL_TOOL {
            return self.send_email(call.args).await;
        }
        // Issue #176: service the delegation tools device-side so the hosted
        // (Medulla) path can delegate. Unlike the harness path — which runs the
        // desk lead's turn in-process and relays it in one voice — a hosted
        // build has no local cognition pool, so the hand-off is *durable and
        // asynchronous*: a board card the desk sees when asked directly. (The
        // synchronous cross-agent cognition relay needs Medulla multi-agent
        // support; tracked in #176.)
        if call.tool == SPAWN_TASK_TOOL {
            return self.spawn_task(call.args).await;
        }
        if call.tool == DELEGATE_TO_DESK_TOOL {
            return self.delegate_to_desk(call.args).await;
        }
        // The provider enforces the manifest grant before any side effect.
        self.rt.tools.invoke(&self.company, call).await
    }

    async fn context_op(&self, op: ContextOp) -> Result<ContextOpResult> {
        match op {
            ContextOp::Put(chunk) => Ok(ContextOpResult::Addr(
                self.rt.context.put(&self.company, chunk).await?,
            )),
            ContextOp::List { prefix } => Ok(ContextOpResult::Metas(
                self.rt.context.list(&self.company, &prefix).await?,
            )),
            ContextOp::Peek { addr, range } => Ok(ContextOpResult::Text(
                self.rt.context.peek(&self.company, &addr, range).await?,
            )),
            ContextOp::Search { query, limit } => Ok(ContextOpResult::Hits(
                self.rt.context.search(&self.company, &query, limit).await?,
            )),
        }
    }

    async fn emit_effect(&self, effect: Effect) -> Result<EffectDisposition> {
        self.gate_effect(effect).await
    }

    async fn park_effect(&self, effect: Effect) -> Result<ApprovalId> {
        self.park(effect).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use crate::company::CompanyManifest;
    use crate::company::runtime::CompanyMail;
    use crate::policy::ManifestApprovalGate;
    use crate::ports::ChannelAdapter;
    use crate::ports::brain::Brain;
    use crate::ports::types::{
        ActorKind, CompressedTrace, CycleResult, EffectGroup, EventSeq, ReplyTo, TokenUsage,
    };
    use crate::runtime::RuntimeBuilder;
    use crate::runtime::channel::OperatorChannel;
    use crate::server::ops::mailer::RecordingMailSender;
    use crate::server::ops::smtp::{SmtpCredentials, SmtpSecurity};
    use crate::store::paths::Bundle;

    fn tmp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-cycle-")
            .tempdir()
            .expect("tempdir")
    }

    fn manifest(policy_mode: &str) -> CompanyManifest {
        let toml_src = format!(
            r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "{policy_mode}"
            "#
        );
        toml::from_str(&toml_src).expect("parse manifest")
    }

    fn operator() -> Actor {
        Actor {
            kind: ActorKind::Operator,
            id: "owner".into(),
        }
    }

    /// A brain that emits one caller-supplied effect on each `OperatorMessage`.
    struct EffectBrain {
        effect: Effect,
    }

    #[async_trait]
    impl Brain for EffectBrain {
        async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
            let mut responses = Vec::new();
            for event in &req.events {
                if let CompanyEvent::OperatorMessage { text, .. } = event {
                    host.emit_effect(self.effect.clone()).await?;
                    responses.push(OutboundMessage {
                        task_id: None,
                        channel: "operator".into(),
                        text: format!("handled: {text}"),
                        steps: Vec::new(),
                        reply_to: None,
                    });
                }
            }
            Ok(CycleResult {
                channel_responses: responses,
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "effect cycle")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// A brain that parks one caller-supplied effect per `OperatorMessage`
    /// through [`CycleHost::park_effect`] — the shape the harness brain produces
    /// when its openhuman policy blocked a tool call inside the turn (#172).
    struct ParkingBrain {
        effect: Effect,
    }

    #[async_trait]
    impl Brain for ParkingBrain {
        async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
            let mut responses = Vec::new();
            for event in &req.events {
                if let CompanyEvent::OperatorMessage { text, .. } = event {
                    host.park_effect(self.effect.clone()).await?;
                    responses.push(OutboundMessage {
                        task_id: None,
                        channel: "operator".into(),
                        text: format!("that needs your approval: {text}"),
                        steps: Vec::new(),
                        reply_to: None,
                    });
                }
            }
            Ok(CycleResult {
                channel_responses: responses,
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "parking cycle")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// A brain that answers on the operator channel and *also* emits a
    /// delegated reply addressed by agent id — the shape `run_delegation` and a
    /// dispatched card's post-back both produce.
    struct DelegatingBrain;

    #[async_trait]
    impl Brain for DelegatingBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            Ok(CycleResult {
                channel_responses: vec![
                    OutboundMessage {
                        task_id: None,
                        channel: "operator".into(),
                        text: "orchestrator".into(),
                        steps: Vec::new(),
                        reply_to: None,
                    },
                    OutboundMessage {
                        task_id: None,
                        // Addressed by *agent id*: no adapter answers to this.
                        channel: "maya".into(),
                        text: "delegated reply".into(),
                        steps: Vec::new(),
                        reply_to: Some(ReplyTo {
                            chat_id: "strategy".into(),
                        }),
                    },
                ],
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "delegating cycle")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// Issue #151: a delegated reply is addressed by agent id, so no adapter
    /// matches it. It used to be dropped silently — the operator REST route
    /// never noticed because it reads `CycleReport.responses` directly, but a
    /// company reached over a channel adapter lost every delegated reply while
    /// still receiving the orchestrator's.
    #[tokio::test]
    async fn a_reply_addressed_by_agent_id_reaches_the_operator_channel() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let operator_channel = OperatorChannel::new();
        let channels: Vec<Arc<dyn ChannelAdapter>> = vec![Arc::new(operator_channel.clone())];
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_brain(Arc::new(DelegatingBrain))
            .with_channels(channels)
            .build()
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "hand it off".into(),
            by: None,
            chat: None,
        }])
        .await
        .unwrap();

        let sent = operator_channel.sent();
        assert_eq!(sent.len(), 2, "both replies must be delivered: {sent:?}");
        // Attribution survives the fallback — the bubble still names the agent.
        let delegated = sent
            .iter()
            .find(|m| m.text == "delegated reply")
            .expect("the delegated reply must be delivered");
        assert_eq!(delegated.channel, "maya");
        assert_eq!(
            delegated.reply_to.as_ref().map(|r| r.chat_id.as_str()),
            Some("strategy")
        );
    }

    /// A brain that fails every cycle — the shape the terminality backstop has
    /// to cover, because a `?` on `run_cycle` would otherwise skip every settle
    /// and strand the attempt row `Running` until the next boot.
    struct FailingBrain;

    #[async_trait]
    impl Brain for FailingBrain {
        async fn run_cycle(
            &self,
            _req: CycleRequest,
            _host: &dyn CycleHost,
        ) -> Result<CycleResult> {
            Err(OpenCompanyError::Store("the brain fell over".into()))
        }
    }

    /// A brain that settles the dispatched run itself, the way `run_task` does
    /// on the harness path — so the backstop can be shown to leave a rich settle
    /// alone rather than racing it.
    struct SettlingBrain {
        runs: Arc<dyn crate::ports::RunStore>,
        status: RunStatus,
    }

    #[async_trait]
    impl Brain for SettlingBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            for event in &req.events {
                if let CompanyEvent::TaskDispatched {
                    run_id: Some(run_id),
                    ..
                } = event
                {
                    let mut outcome = RunOutcome::new(self.status);
                    if self.status == RunStatus::Failed {
                        outcome = outcome.with_error("the brain said so");
                    }
                    self.runs
                        .finish_run(&req.company_id, run_id, outcome)
                        .await?;
                }
            }
            Ok(CycleResult {
                channel_responses: vec![OutboundMessage {
                    task_id: None,
                    channel: "operator".into(),
                    text: "settled".into(),
                    steps: Vec::new(),
                    reply_to: None,
                }],
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "settling cycle")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// The rich settle always wins: `run_task` finishes the row *inside*
    /// `brain.run_cycle`, which is awaited before the backstop, so there is no
    /// race for the backstop to lose. Pinned rather than argued, because a
    /// backstop that overwrote a real outcome with a generic failure would be
    /// worse than no backstop at all.
    ///
    /// Both cases matter. A **terminal** settle must survive; so must a
    /// **parked** one — `Paused` and `WaitingApproval` are waiting on something
    /// outside the cycle, and reclaiming them would delete real pending work
    /// every time a cycle ended.
    #[tokio::test]
    async fn the_backstop_never_overwrites_a_settle_the_brain_already_made() {
        for (status, error) in [
            (RunStatus::Succeeded, None),
            (RunStatus::Paused, None),
            (RunStatus::WaitingApproval, None),
            (RunStatus::Failed, Some("the brain said so")),
        ] {
            let home_dir = tmp_home();
            let runs: Arc<dyn crate::ports::RunStore> =
                Arc::new(crate::store::FsOps::new(home_dir.path().to_path_buf()));
            let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
                .with_runs(Arc::clone(&runs))
                .with_brain(Arc::new(SettlingBrain {
                    runs: Arc::clone(&runs),
                    status,
                }))
                .build()
                .await
                .unwrap();
            let run_id = pending_run(&rt, "t-1").await;

            rt.run_cycle(vec![CompanyEvent::TaskDispatched {
                task_id: "t-1".into(),
                run_id: Some(run_id.clone()),
            }])
            .await
            .expect("cycle");

            let settled = rt
                .runs()
                .get_run(rt.id(), &run_id)
                .await
                .expect("read")
                .expect("row");
            assert_eq!(
                settled.status, status,
                "the backstop must not overwrite a {status} settle"
            );
            assert_eq!(settled.error.as_deref(), error);
        }
    }

    /// Mints a `Pending` run for `task`, so a test can drive a dispatch cycle
    /// the way `CompanyRuntime::dispatch_task` does.
    async fn pending_run(rt: &crate::company::runtime::CompanyRuntime, task: &str) -> String {
        rt.runs()
            .create_run(
                rt.id(),
                crate::ports::runs::NewRun {
                    id: crate::ports::generate_id(),
                    task_id: task.to_string(),
                    agent_id: "ceo".to_string(),
                },
            )
            .await
            .expect("mint a run")
            .id
    }

    /// Issue #242, the `begin_run` half: the run moves `Pending` → `Running`
    /// stamped with the **seq of the very `TaskDispatched` event that drove
    /// it**, and by the end of the cycle it is terminal rather than stranded —
    /// even though the default build's brain ignores `TaskDispatched` entirely
    /// and settles nothing.
    #[tokio::test]
    async fn a_dispatch_cycle_starts_its_run_and_never_leaves_it_claiming_to_be_live() {
        let home_dir = tmp_home();
        let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
            .await
            .unwrap();
        let run_id = pending_run(&rt, "t-1").await;

        let report = rt
            .run_cycle(vec![CompanyEvent::TaskDispatched {
                task_id: "t-1".into(),
                run_id: Some(run_id.clone()),
            }])
            .await
            .expect("the cycle itself succeeds");

        let run = rt
            .runs()
            .get_run(rt.id(), &run_id)
            .await
            .expect("read")
            .expect("the run survives its cycle");
        assert_eq!(
            run.trigger_event_seq, report.persisted_seq,
            "the run must name the exact log line that drove it"
        );
        assert!(
            run.started_at_millis.is_some(),
            "begin_run stamps when the attempt actually began"
        );
        assert_eq!(
            run.status,
            RunStatus::Failed,
            "an echo-brain dispatch produces no rich settle, so the backstop closes it"
        );
        assert_eq!(run.error.as_deref(), Some(RUN_UNSETTLED_ERROR));
        assert!(run.finished_at_millis.is_some());
    }

    /// The other backstop arm: the brain **errored**. The cycle error still
    /// propagates to the caller (nothing is swallowed), and the row settles
    /// carrying that same reason instead of sitting `Running` forever.
    #[tokio::test]
    async fn a_failed_cycle_settles_its_run_and_still_reports_the_failure() {
        let home_dir = tmp_home();
        let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
            .with_brain(Arc::new(FailingBrain))
            .build()
            .await
            .unwrap();
        let run_id = pending_run(&rt, "t-1").await;

        let err = rt
            .run_cycle(vec![CompanyEvent::TaskDispatched {
                task_id: "t-1".into(),
                run_id: Some(run_id.clone()),
            }])
            .await
            .expect_err("a failing brain still fails the cycle");
        assert!(err.to_string().contains("the brain fell over"), "{err}");

        let run = rt
            .runs()
            .get_run(rt.id(), &run_id)
            .await
            .expect("read")
            .expect("run");
        assert_eq!(run.status, RunStatus::Failed);
        let reason = run.error.unwrap_or_default();
        assert!(reason.starts_with(RUN_CYCLE_FAILED_ERROR), "{reason}");
        assert!(
            reason.contains("the brain fell over"),
            "the row must carry the reason the caller saw: {reason}"
        );
    }

    /// A dispatch whose run row could not be minted (`run_id: None`) — the
    /// documented degraded path — must still run the cycle normally. The
    /// dispatch is the work; the row is only the record of it.
    #[tokio::test]
    async fn an_untracked_dispatch_still_runs_its_cycle() {
        let home_dir = tmp_home();
        let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: None,
        }])
        .await
        .expect("an untracked dispatch is still a dispatch");

        assert!(
            rt.runs()
                .list_runs(rt.id(), &crate::ports::runs::RunFilter::default())
                .await
                .expect("list")
                .is_empty(),
            "no row was minted, so none may be invented"
        );
    }

    /// A `run_id` naming a row that does not exist (a replayed journal line, a
    /// row lost with its store) must not fail the cycle either — and must not
    /// be tracked, so the backstop has nothing to settle.
    #[tokio::test]
    async fn a_dispatch_naming_an_unknown_run_does_not_fail_the_cycle() {
        let home_dir = tmp_home();
        let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: Some("run-that-never-was".into()),
        }])
        .await
        .expect("an unknown run id is a bookkeeping miss, not a cycle failure");
    }

    #[tokio::test]
    async fn end_to_end_operator_message_echoes_and_persists() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::fs_defaults(home.clone(), manifest("full"))
            .await
            .unwrap();

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "hi".into(),
                by: None,
                chat: None,
            }])
            .await
            .unwrap();

        // (a) an operator response came back.
        assert_eq!(report.responses.len(), 1);
        assert_eq!(report.responses[0].channel, "operator");
        assert_eq!(report.responses[0].text, "You said: hi");

        // (b) the event was appended to the log.
        let stored = rt
            .events
            .read_from(rt.id(), EventSeq::new(0), 10)
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].event,
            CompanyEvent::OperatorMessage {
                text: "hi".into(),
                by: None,
                chat: None
            }
        );

        // (c) a compressed trace was persisted.
        let traces = rt.memory.recent_traces(rt.id(), 10).await.unwrap();
        assert!(!traces.is_empty());
    }

    #[tokio::test]
    async fn effect_executes_at_most_once_across_reload() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::fs_defaults(home.clone(), manifest("full"))
            .await
            .unwrap();

        let effect = Effect {
            kind: "x402.spend".into(),
            group: EffectGroup::Spend,
            amount_usd: Some(3.0),
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };

        execute_effect_once(&rt, "k1", &effect).await.unwrap();
        // Same key again: skipped, no second ledger entry.
        execute_effect_once(&rt, "k1", &effect).await.unwrap();

        let record = rt.store().load(rt.id()).await.unwrap().unwrap();
        assert_eq!(record.ledger.len(), 1);

        // Rebuild the runtime over the same home; journal replay must remember
        // the executed key so a replayed effect does not run twice.
        let rt2 = RuntimeBuilder::fs_defaults(home.clone(), manifest("full"))
            .await
            .unwrap();
        assert!(rt2.journal.is_executed("k1"));
        execute_effect_once(&rt2, "k1", &effect).await.unwrap();
        let record = rt2.store.load(rt2.id()).await.unwrap().unwrap();
        assert_eq!(record.ledger.len(), 1);
    }

    #[tokio::test]
    async fn supervised_effect_parks_then_resolves() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sign_effect = Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_brain(Arc::new(EffectBrain {
                effect: sign_effect,
            }))
            .build()
            .await
            .unwrap();

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "file it".into(),
                by: None,
                chat: None,
            }])
            .await
            .unwrap();
        assert_eq!(report.parked.len(), 1);
        let approval_id = report.parked[0].clone();
        assert_eq!(rt.pending_approvals().len(), 1);

        // Approving executes the effect and runs a follow-up cycle. The
        // follow-up carries an ApprovalResolved event (not OperatorMessage), so
        // the brain emits nothing and the queue drains.
        let follow_up = rt
            .resolve_approval(&approval_id, Verdict::Approve, operator())
            .await
            .unwrap();
        assert!(follow_up.parked.is_empty());
        assert!(rt.pending_approvals().is_empty());
    }

    // --- Single-use grants on approve (issue #243) ---------------------------

    /// A harness-projected effect, i.e. one carrying `agent`. Its payload is a
    /// tool's argument object, not something the runtime can perform.
    fn harness_effect(agent: &str, tool: &str, args: serde_json::Value) -> Effect {
        Effect {
            kind: tool.into(),
            group: EffectGroup::Sign,
            // A real spend amount, deliberately: it is what proves the effect
            // was NOT executed. `perform_effect` ledgers any `amount_usd`, so an
            // empty ledger is positive evidence that the native path was skipped
            // rather than merely evidence that nothing observable happened.
            amount_usd: Some(42.0),
            established_thread: false,
            first_time_counterparty: false,
            payload: args,
            agent: Some(agent.to_string()),
            run_id: None,
        }
    }

    /// Parks `effect` through a real cycle and returns the runtime + approval id.
    async fn park_one(home: std::path::PathBuf, effect: Effect) -> (CompanyRuntime, ApprovalId) {
        let rt = RuntimeBuilder::new(home, manifest("supervised"))
            .with_brain(Arc::new(EffectBrain { effect }))
            .build()
            .await
            .unwrap();
        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "do it".into(),
                by: None,
                chat: None,
            }])
            .await
            .unwrap();
        assert_eq!(report.parked.len(), 1);
        let id = report.parked[0].clone();
        (rt, id)
    }

    /// The core of #243: approving an agent's blocked tool call mints a
    /// single-use grant and does **not** execute the effect.
    ///
    /// Executing it would be worse than useless. The payload is the tool's
    /// arguments, so `perform_effect` would ledger a spend for money nothing
    /// actually moved and route no message — the operator would see an approval
    /// marked done, a charge on the books, and no email sent. The grant is what
    /// makes approval mean "the agent may now really do this, once".
    #[tokio::test]
    async fn approving_a_harness_tool_call_mints_a_grant_instead_of_executing() {
        let home_dir = tmp_home();
        let args = serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL", "to": "a@b.test" });
        let (rt, id) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect("finance", "composio_execute", args.clone()),
        )
        .await;

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .unwrap();

        // A grant exists, scoped to the agent, tool and exact arguments.
        let grant = rt.grants.peek(&id).expect("a grant was minted");
        assert_eq!(grant.agent, "finance");
        assert_eq!(grant.tool, "composio_execute");
        assert_eq!(grant.args, args);

        // ...and the effect was NOT executed: no ledger row, no journal key.
        let record = rt.store.load(rt.id()).await.unwrap().unwrap();
        assert!(
            record.ledger.is_empty(),
            "a harness tool call must not be performed natively — its payload is \
             arguments, so executing it books a spend for work that never happened"
        );
        assert!(!rt.journal.is_executed(&format!("approval:{id}")));
    }

    /// Approve-with-edit mints against the **amended** arguments.
    ///
    /// Granting the original would let the agent re-issue the very call the
    /// operator edited, silently discarding the edit — worse than not supporting
    /// amend at all, because the operator has every reason to think their change
    /// took effect.
    #[tokio::test]
    async fn amending_an_approval_grants_the_edited_arguments() {
        let home_dir = tmp_home();
        let (rt, id) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect(
                "finance",
                "composio_execute",
                serde_json::json!({ "to": "wrong@b.test", "body": "hi" }),
            ),
        )
        .await;

        rt.resolve_approval_amended(&id, serde_json::json!({ "to": "right@b.test" }), operator())
            .await
            .unwrap();

        let grant = rt.grants.peek(&id).expect("a grant was minted");
        assert_eq!(
            grant.args,
            serde_json::json!({ "to": "right@b.test", "body": "hi" }),
            "the grant admits the operator's edit, overlaid onto the original"
        );
        // The un-edited call must NOT be redeemable.
        assert!(
            rt.grants
                .consume(
                    "finance",
                    "composio_execute",
                    &serde_json::json!({ "to": "wrong@b.test", "body": "hi" })
                )
                .is_none()
        );
    }

    /// A denied approval grants nothing. "No" must not leave a live permission
    /// behind for the agent to find.
    #[tokio::test]
    async fn denying_a_harness_tool_call_mints_nothing() {
        let home_dir = tmp_home();
        let (rt, id) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect("finance", "composio_execute", serde_json::json!({})),
        )
        .await;

        rt.resolve_approval(&id, Verdict::Deny, operator())
            .await
            .unwrap();

        assert!(rt.grants.peek(&id).is_none());
        assert_eq!(rt.grants.live_count(), 0);
    }

    /// An approval that expired past its TTL grants nothing either, even though
    /// the operator clicked approve — default-deny-on-silence wins, and it must
    /// win here too or expiry would become a way to smuggle a live grant out of
    /// a stale approval.
    #[tokio::test]
    async fn an_expired_approval_mints_nothing_even_on_approve() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let gate = Arc::new(
            ManifestApprovalGate::new(manifest("supervised").policy.clone()).with_ttl_millis(0),
        );
        let rt = RuntimeBuilder::new(home, manifest("supervised"))
            .with_approvals(gate)
            .with_brain(Arc::new(EffectBrain {
                effect: harness_effect("finance", "composio_execute", serde_json::json!({})),
            }))
            .build()
            .await
            .unwrap();
        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "do it".into(),
                by: None,
                chat: None,
            }])
            .await
            .unwrap();
        let id = report.parked[0].clone();

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .unwrap();
        assert_eq!(
            rt.grants.live_count(),
            0,
            "an expired approval is a deny, so it hands out no permission"
        );
    }

    /// A live grant survives a restart; a consumed one does not come back.
    ///
    /// The window between approve and re-issue spans a model turn, so a deploy
    /// inside it is ordinary — and a resurrected single-use grant is no longer
    /// single-use.
    #[tokio::test]
    async fn grants_replay_on_boot_but_a_spent_one_does_not() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let args = serde_json::json!({ "to": "a@b.test" });
        let (rt, id) = park_one(
            home.clone(),
            harness_effect("finance", "composio_execute", args.clone()),
        )
        .await;
        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .unwrap();
        drop(rt);

        // Restart: the grant comes back.
        let rt2 = RuntimeBuilder::fs_defaults(home.clone(), manifest("supervised"))
            .await
            .unwrap();
        assert_eq!(rt2.grants.live_count(), 1);
        // Redeem it and journal the consumption the way a cycle would.
        assert!(
            rt2.grants
                .consume("finance", "composio_execute", &args)
                .is_some()
        );
        for spent in rt2.grants.drain_consumed() {
            rt2.journal.record_grant_consumed(&spent).await.unwrap();
        }
        drop(rt2);

        // Restart again: the spent grant stays spent.
        let rt3 = RuntimeBuilder::fs_defaults(home, manifest("supervised"))
            .await
            .unwrap();
        assert_eq!(
            rt3.grants.live_count(),
            0,
            "a redeemed grant must not be re-armed by a restart"
        );
    }

    /// Issue #243: a grant the agent never redeemed expires, is journaled, and
    /// the operator is TOLD.
    ///
    /// The silent version of this is the failure worth designing against: the
    /// operator approves, watches nothing happen, and has no way to tell whether
    /// the work is in flight, already done, or quietly dead. Announcing the lapse
    /// is what makes re-approving an informed choice rather than a guess.
    #[tokio::test]
    async fn an_unredeemed_grant_expires_journals_and_tells_the_operator() {
        let home_dir = tmp_home();
        let operator_channel = Arc::new(crate::runtime::channel::OperatorChannel::new());
        let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("supervised"))
            .with_channels(vec![operator_channel.clone()])
            .build()
            .await
            .unwrap();

        // `at_millis: 0` is unambiguously past the 15-minute TTL.
        rt.grants.grant(GrantedCall {
            approval_id: ApprovalId::new("appr-stale"),
            agent: "finance".into(),
            tool: "composio_execute".into(),
            args: serde_json::json!({ "to": "a@b.test" }),
            at_millis: 0,
        });
        // A fresh one, to prove the sweep is selective rather than a flush.
        rt.grants.grant(GrantedCall {
            approval_id: ApprovalId::new("appr-fresh"),
            agent: "finance".into(),
            tool: "workspace_write".into(),
            args: serde_json::json!({}),
            at_millis: now_millis(),
        });

        let expired = rt.sweep_expired_grants().await.unwrap();
        assert_eq!(expired, vec![ApprovalId::new("appr-stale")]);
        assert_eq!(rt.grants.live_count(), 1, "the fresh grant is untouched");
        assert!(rt.grants.peek(&ApprovalId::new("appr-fresh")).is_some());

        // The operator was told, and told which tool and which agent — enough to
        // decide whether to re-approve without going digging.
        let sent = operator_channel.sent();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].text.contains("composio_execute"),
            "{}",
            sent[0].text
        );
        assert!(sent[0].text.contains("finance"), "{}", sent[0].text);
        assert!(sent[0].text.contains("re-approve"), "{}", sent[0].text);

        // The expiry is durable: a restart must not hand the permission back.
        assert!(
            rt.journal
                .replayed_grants()
                .iter()
                .all(|g| g.approval_id != ApprovalId::new("appr-stale"))
        );
    }

    /// Issue #243: resolving an approval that is already gone is a no-op, not a
    /// second resolution.
    ///
    /// A double-clicked approve, a retried request, or two operators on the same
    /// queue all hit this. Before the outcome enum, the second call could not be
    /// told apart from a deny: the gate returned `None` either way, so the
    /// runner appended a second `ApprovalResolved` journal record AND ran a
    /// second follow-up cycle — a whole model turn spent re-announcing a
    /// resolution the brain had already been given.
    #[tokio::test]
    async fn resolving_an_already_resolved_approval_is_a_deterministic_no_op() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sign_effect = Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_brain(Arc::new(EffectBrain {
                effect: sign_effect,
            }))
            .build()
            .await
            .unwrap();

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "file it".into(),
                by: None,
                chat: None,
            }])
            .await
            .unwrap();
        let approval_id = report.parked[0].clone();

        rt.resolve_approval(&approval_id, Verdict::Approve, operator())
            .await
            .unwrap();
        let events_after_first = rt
            .events
            .read_from(rt.id(), EventSeq::new(0), 1000)
            .await
            .unwrap()
            .len();

        // The second submit.
        let again = rt
            .resolve_approval(&approval_id, Verdict::Approve, operator())
            .await
            .unwrap();

        assert_eq!(again.responses.len(), 1);
        assert_eq!(
            again.responses[0].text, "This approval was already resolved.",
            "the operator gets a deterministic line, not an error and not a re-run"
        );
        assert!(again.executed_effects.is_empty());
        assert!(again.parked.is_empty());
        assert!(
            again.persisted_seq.is_none(),
            "a no-op must not claim to have persisted anything"
        );
        assert_eq!(
            rt.events
                .read_from(rt.id(), EventSeq::new(0), 1000)
                .await
                .unwrap()
                .len(),
            events_after_first,
            "no second ApprovalResolved event, and no follow-up cycle behind it"
        );
    }

    /// Issue #172: an already-decided approval request parks and reaches the
    /// operator's queue **without** being re-evaluated.
    ///
    /// The company runs `full` autonomy and the effect classifies as `Other` —
    /// the two conditions under which `ApprovalGate::evaluate` returns `Allow`.
    /// Had the request gone through `emit_effect` it would have been "executed"
    /// as a no-op and vanished, which is exactly how a chat-gated tool call used
    /// to disappear before ever reaching the Approvals page.
    #[tokio::test]
    async fn a_decided_request_parks_without_being_re_evaluated() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let tool_effect = Effect {
            kind: "composio_execute".into(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" }),
            agent: None,
            run_id: None,
        };
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(ParkingBrain {
                effect: tool_effect.clone(),
            }))
            .build()
            .await
            .unwrap();

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "send that email".into(),
                by: None,
                chat: None,
            }])
            .await
            .unwrap();

        assert_eq!(report.parked.len(), 1, "the request parked");
        assert!(
            report.executed_effects.is_empty(),
            "a parked request must not execute"
        );

        // The Approvals page reads exactly this.
        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 1, "the operator sees the request");
        assert_eq!(pending[0].kind, "composio_execute");
        assert_eq!(pending[0].id, report.parked[0]);

        // And it is durable: a fresh runtime over the same home replays it, so a
        // restart does not lose what the operator still owes an answer to.
        let rt2 = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(ParkingBrain {
                effect: tool_effect,
            }))
            .build()
            .await
            .unwrap();
        assert_eq!(rt2.pending_approvals().len(), 1);
    }

    #[tokio::test]
    async fn approval_survives_runtime_restart() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sign_effect = Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };
        let approval_id = {
            let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
                .with_brain(Arc::new(EffectBrain {
                    effect: sign_effect.clone(),
                }))
                .build()
                .await
                .unwrap();
            let report = rt
                .run_cycle(vec![CompanyEvent::OperatorMessage {
                    text: "file it".into(),
                    by: None,
                    chat: None,
                }])
                .await
                .unwrap();
            report.parked[0].clone()
        };

        // A fresh runtime over the same home rehydrates the parked approval and
        // can resolve it by its original id.
        let rt2 = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_brain(Arc::new(EffectBrain {
                effect: sign_effect,
            }))
            .build()
            .await
            .unwrap();
        assert_eq!(rt2.pending_approvals().len(), 1);
        rt2.resolve_approval(&approval_id, Verdict::Deny, operator())
            .await
            .unwrap();
        assert!(rt2.pending_approvals().is_empty());
    }

    #[tokio::test]
    async fn amend_then_approve_executes_edited_effect() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        // A parked Sign effect whose payload the operator will overwrite so the
        // executed effect routes an amended message to the operator channel.
        let sign_effect = Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "channel": "operator", "text": "ORIGINAL" }),
            agent: None,
            run_id: None,
        };
        // A recording operator channel we keep a handle to (Arc-shared buffer).
        let operator_channel = OperatorChannel::new();
        let channels: Vec<Arc<dyn ChannelAdapter>> = vec![Arc::new(operator_channel.clone())];
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_brain(Arc::new(EffectBrain {
                effect: sign_effect,
            }))
            .with_channels(channels)
            .build()
            .await
            .unwrap();

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "file it".into(),
                by: None,
                chat: None,
            }])
            .await
            .unwrap();
        let approval_id = report.parked[0].clone();

        // Approve with an edited payload: only `text` changes.
        let follow_up = rt
            .resolve_approval_amended(
                &approval_id,
                serde_json::json!({ "text": "AMENDED" }),
                operator(),
            )
            .await
            .unwrap();
        assert!(follow_up.parked.is_empty());
        assert!(rt.pending_approvals().is_empty());

        // The amended effect executed: the operator channel saw "AMENDED",
        // never the original "ORIGINAL" text.
        let sent = operator_channel.sent();
        assert!(
            sent.iter().any(|m| m.text == "AMENDED"),
            "amended text was routed, got {sent:?}"
        );
        assert!(sent.iter().all(|m| m.text != "ORIGINAL"));

        // The immutable journal records both the original park and the amend.
        let raw = tokio::fs::read_to_string(Bundle::new(&home, rt.id()).journal_jsonl())
            .await
            .unwrap();
        assert!(raw.contains("ApprovalParked"));
        assert!(raw.contains("ApprovalAmended"));
        assert!(raw.contains("AMENDED"));
    }

    #[tokio::test]
    async fn sweep_expires_parked_approval_to_deny() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sign_effect = Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };
        // A zero-TTL gate: anything parked is immediately past its deadline.
        let gate = Arc::new(
            ManifestApprovalGate::new(manifest("supervised").policy.clone()).with_ttl_millis(0),
        );
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_brain(Arc::new(EffectBrain {
                effect: sign_effect,
            }))
            .with_approvals(gate)
            .build()
            .await
            .unwrap();

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "file it".into(),
                by: None,
                chat: None,
            }])
            .await
            .unwrap();
        let approval_id = report.parked[0].clone();
        assert_eq!(rt.pending_approvals().len(), 1);

        // The maintenance sweep resolves the silent approval to a default-deny.
        let expired = rt.sweep_expired_approvals().await.unwrap();
        assert_eq!(expired, vec![approval_id]);
        assert!(rt.pending_approvals().is_empty());

        let raw = tokio::fs::read_to_string(Bundle::new(&home, rt.id()).journal_jsonl())
            .await
            .unwrap();
        assert!(raw.contains("ApprovalExpired"));
    }

    // ── Issue #174: the generic cycle seam meters inference usage ────────────

    /// A brain that reports a fixed [`TokenUsage`] for every cycle — the shape
    /// hosted Medulla cognition produces once its `orch:usage` frames land.
    struct MeteredBrain {
        usage: TokenUsage,
        metering: UsageMetering,
    }

    impl MeteredBrain {
        fn per_cycle(usage: TokenUsage) -> Self {
            Self {
                usage,
                metering: UsageMetering::PerCycle,
            }
        }
    }

    #[async_trait]
    impl Brain for MeteredBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            Ok(CycleResult {
                channel_responses: vec![OutboundMessage {
                    task_id: None,
                    channel: "operator".into(),
                    text: "thought about it".into(),
                    steps: Vec::new(),
                    reply_to: None,
                }],
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "metered cycle")],
                ledger_deltas: Vec::new(),
                token_usage: self.usage,
            })
        }

        fn cognition(&self) -> crate::ports::Cognition {
            crate::ports::Cognition {
                path: "test",
                provider: "medulla",
                metering: self.metering,
            }
        }
    }

    fn reported_usage(cost_usd: f64) -> TokenUsage {
        TokenUsage {
            input: 1_200,
            output: 340,
            cached_input: 200,
            cost_usd,
        }
    }

    /// The bug: a brain outside the openhuman harness reported real token usage
    /// and the cycle loop dropped it, so the Usage view read zero forever.
    #[tokio::test]
    async fn reported_cycle_usage_reaches_the_usage_meter() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain::per_cycle(reported_usage(0.031))))
            .build()
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "how are we doing".into(),
            by: None,
            chat: None,
        }])
        .await
        .unwrap();

        let samples = rt.usage().query(rt.id(), 0).await.unwrap();
        assert_eq!(samples.len(), 1, "one inference sample per metered cycle");
        let sample = &samples[0];
        assert_eq!(sample.kind, crate::ports::usage::SampleKind::Inference);
        assert_eq!(sample.input_tokens, 1_200);
        assert_eq!(sample.output_tokens, 340);
        assert_eq!(sample.cached_input_tokens, 200);
        assert_eq!(sample.cost_usd, 0.031);
        assert_eq!(sample.provider, "medulla");
        assert_eq!(sample.agent, crate::metering::UNATTRIBUTED_AGENT);

        // Cost also lands on Finances as an `inference.spend` ledger entry.
        let record = rt.store().load(rt.id()).await.unwrap().unwrap();
        let spend: Vec<_> = record
            .ledger
            .iter()
            .filter(|e| e.kind == crate::metering::INFERENCE_SPEND_KIND)
            .collect();
        assert_eq!(spend.len(), 1);
        assert_eq!(spend[0].amount_usd, 0.031);
    }

    /// Tokens without USD (the managed passthrough bills backend-side) still
    /// count on the Usage surface, but must not post a `$0.00` spend line.
    #[tokio::test]
    async fn token_only_usage_meters_without_a_ledger_entry() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain::per_cycle(reported_usage(0.0))))
            .build()
            .await
            .unwrap();

        rt.run_cycle(Vec::new()).await.unwrap();

        assert_eq!(rt.usage().query(rt.id(), 0).await.unwrap().len(), 1);
        let record = rt.store().load(rt.id()).await.unwrap().unwrap();
        assert!(
            !record
                .ledger
                .iter()
                .any(|e| e.kind == crate::metering::INFERENCE_SPEND_KIND)
        );
    }

    /// A cycle that spent nothing writes nothing — an idle cycle or the offline
    /// echo brain must not mint an empty sample.
    #[tokio::test]
    async fn a_zero_usage_cycle_writes_no_sample() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain::per_cycle(TokenUsage::default())))
            .build()
            .await
            .unwrap();

        rt.run_cycle(Vec::new()).await.unwrap();

        assert!(rt.usage().query(rt.id(), 0).await.unwrap().is_empty());
    }

    /// The openhuman harness meters every turn itself, so the cycle seam must
    /// stay out of its way: a self-metering path's cycle usage is ignored rather
    /// than charged a second time.
    #[tokio::test]
    async fn a_self_metering_brain_is_not_metered_twice() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain {
                usage: reported_usage(9.99),
                metering: UsageMetering::PerTurn,
            }))
            .build()
            .await
            .unwrap();

        rt.run_cycle(Vec::new()).await.unwrap();

        assert!(rt.usage().query(rt.id(), 0).await.unwrap().is_empty());
        let record = rt.store().load(rt.id()).await.unwrap().unwrap();
        assert!(
            !record
                .ledger
                .iter()
                .any(|e| e.kind == crate::metering::INFERENCE_SPEND_KIND)
        );
    }

    /// `UsageMetering::None` means "no model runs on this path", so the cycle
    /// seam must enforce it too. Without that arm a `None` brain reporting
    /// non-zero usage was still metered under its own slug — the echo brain
    /// would post a `provider: "none"` row into `byProvider`.
    #[tokio::test]
    async fn a_brain_that_runs_no_model_is_not_metered() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain {
                usage: reported_usage(4.2),
                metering: UsageMetering::None,
            }))
            .build()
            .await
            .unwrap();

        rt.run_cycle(Vec::new()).await.unwrap();

        assert!(rt.usage().query(rt.id(), 0).await.unwrap().is_empty());
        let record = rt.store().load(rt.id()).await.unwrap().unwrap();
        assert!(
            !record
                .ledger
                .iter()
                .any(|e| e.kind == crate::metering::INFERENCE_SPEND_KIND)
        );
    }

    /// Every cycle meters independently, so a multi-turn conversation
    /// accumulates rather than overwriting.
    #[tokio::test]
    async fn each_cycle_meters_its_own_usage() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain::per_cycle(reported_usage(0.01))))
            .build()
            .await
            .unwrap();

        for _ in 0..3 {
            rt.run_cycle(Vec::new()).await.unwrap();
        }

        let samples = rt.usage().query(rt.id(), 0).await.unwrap();
        assert_eq!(samples.len(), 3);
        let total: u64 = samples.iter().map(|s| s.input_tokens).sum();
        assert_eq!(total, 3_600);
    }

    /// A brain that tracks the peak number of concurrently-active cycles.
    struct ConcurrencyBrain {
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Brain for ConcurrencyBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "concurrency")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn cycles_are_serial_per_company() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let peak = Arc::new(AtomicUsize::new(0));
        let brain = Arc::new(ConcurrencyBrain {
            active: Arc::new(AtomicUsize::new(0)),
            peak: peak.clone(),
        });
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("full"))
                .with_brain(brain)
                .build()
                .await
                .unwrap(),
        );

        let a = {
            let rt = rt.clone();
            tokio::spawn(async move { rt.run_cycle(Vec::new()).await })
        };
        let b = {
            let rt = rt.clone();
            tokio::spawn(async move { rt.run_cycle(Vec::new()).await })
        };
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();

        // The serial lock kept the two cycles from overlapping.
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn distinct_companies_run_concurrently() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let one = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_id(CompanyId::new("one"))
            .build()
            .await
            .unwrap();
        let two = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_id(CompanyId::new("two"))
            .build()
            .await
            .unwrap();

        let (ra, rb) = tokio::join!(
            one.run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "a".into(),
                by: None,
                chat: None
            }]),
            two.run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "b".into(),
                by: None,
                chat: None
            }]),
        );
        assert_eq!(ra.unwrap().responses.len(), 1);
        assert_eq!(rb.unwrap().responses.len(), 1);
    }

    fn test_smtp(from_email: &str) -> SmtpCredentials {
        SmtpCredentials {
            host: "smtp.example.com".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "user".into(),
            password: "hunter2".into(),
            from_name: "Acme".into(),
            from_email: from_email.into(),
        }
    }

    #[tokio::test]
    async fn email_send_effect_sends_and_records() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let email_effect = Effect {
            kind: "email.send".into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: true,
            first_time_counterparty: false,
            payload: serde_json::json!({ "to": "x@ext.com", "subject": "Hi", "body": "yo" }),
            agent: None,
            run_id: None,
        };
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(EffectBrain {
                effect: email_effect,
            }))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "send it".into(),
            by: None,
            chat: None,
        }])
        .await
        .unwrap();

        assert_eq!(sender.sent().len(), 1);
        // The From address is the company's own address, never spoofable via
        // the effect payload (which carries no `from` field at all).
        assert_eq!(sender.sent()[0].0, "ceo@acme.test");
        let inbox = rt.inbox().messages(rt.id(), "ceo", 10, 0).await.unwrap();
        assert!(inbox.iter().any(|r| r.outbound && r.subject == "Hi"));
    }

    /// **The acceptance bar for issue #227.** Parking a cold recipient's report
    /// is only worth doing if approving it actually sends the mail — otherwise
    /// `pending` is a nicer-looking way to drop the report.
    ///
    /// This parks an `email.send` effect the way
    /// [`crate::workflows::delivery`] does — straight onto the gate + journal,
    /// with no cycle running and no brain involved — then resolves it the way
    /// the HTTP handler does. The mail must go out and leave the outbound audit
    /// record, through `resolve_approval` → `execute_effect_once` →
    /// `perform_effect` → `send_company_email`.
    ///
    /// Policy mode is `full` on purpose: nothing here relies on the gate
    /// deciding to park. It was parked directly, exactly as delivery parks it.
    #[tokio::test]
    async fn a_directly_parked_email_send_is_mailed_when_approved() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();

        // What `park_cold_recipient` builds, field for field.
        let effect = Effect {
            kind: EMAIL_SEND_KIND.into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: true,
            payload: serde_json::json!({
                "to": "stranger@ext.com",
                "subject": "[Acme] Report flow — Owner summary",
                "body": "Q3 is up 12%.",
            }),
            agent: None,
            run_id: None,
        };
        let approval_id = rt.approvals.park(rt.id(), effect.clone()).await.unwrap();
        rt.journal
            .record_parked(&approval_id, &effect, now_millis())
            .await
            .unwrap();

        // It reaches the operator's queue — the same list a workflow's park
        // shows up in, since it is the same journal.
        assert_eq!(rt.pending_approvals().len(), 1);
        assert_eq!(rt.pending_approvals()[0].kind, EMAIL_SEND_KIND);
        assert!(sender.sent().is_empty(), "parked means not yet sent");

        rt.resolve_approval(&approval_id, Verdict::Approve, operator())
            .await
            .unwrap();

        // Approving SENDS.
        assert_eq!(sender.sent().len(), 1, "approving must mail the report");
        assert_eq!(sender.sent()[0].1.to, "stranger@ext.com");
        assert!(sender.sent()[0].1.body.contains("Q3 is up 12%."));
        // From the company's own address, never anything the payload named.
        assert_eq!(sender.sent()[0].0, "ceo@acme.test");
        // …and leaves the outbound audit record, which also makes the recipient
        // an established thread for next time.
        let inbox = rt.inbox().messages(rt.id(), "ceo", 10, 0).await.unwrap();
        assert!(
            inbox
                .iter()
                .any(|r| r.outbound && r.subject.contains("Owner summary")),
            "{inbox:?}"
        );
        assert!(rt.pending_approvals().is_empty(), "the queue drains");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// The other half of the same bar: DENYING sends nothing and drains the
    /// queue. A parked report must not leak out on a refusal.
    #[tokio::test]
    async fn a_directly_parked_email_send_is_not_mailed_when_denied() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();

        let effect = Effect {
            kind: EMAIL_SEND_KIND.into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: true,
            payload: serde_json::json!({
                "to": "stranger@ext.com",
                "subject": "[Acme] Report flow — Owner summary",
                "body": "Q3 is up 12%.",
            }),
            agent: None,
            run_id: None,
        };
        let approval_id = rt.approvals.park(rt.id(), effect.clone()).await.unwrap();
        rt.journal
            .record_parked(&approval_id, &effect, now_millis())
            .await
            .unwrap();

        rt.resolve_approval(&approval_id, Verdict::Deny, operator())
            .await
            .unwrap();

        assert!(sender.sent().is_empty(), "a denied report must not go out");
        assert!(
            rt.inbox()
                .messages(rt.id(), "ceo", 10, 0)
                .await
                .unwrap()
                .iter()
                .all(|r| !r.outbound),
            "nothing was sent, so there is no outbound record"
        );
        assert!(rt.pending_approvals().is_empty());
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// **Restart durability.** A parked report survives a process restart with
    /// its original id and still sends on approval — the property that makes a
    /// `pending` row honest even though the run itself is not persisted.
    #[tokio::test]
    async fn a_parked_email_send_survives_a_restart_and_still_sends() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let effect = Effect {
            kind: EMAIL_SEND_KIND.into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: true,
            payload: serde_json::json!({
                "to": "stranger@ext.com",
                "subject": "[Acme] Report flow — Owner summary",
                "body": "Q3 is up 12%.",
            }),
            agent: None,
            run_id: None,
        };
        let approval_id = {
            let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
                .build()
                .await
                .unwrap();
            let id = rt.approvals.park(rt.id(), effect.clone()).await.unwrap();
            rt.journal
                .record_parked(&id, &effect, now_millis())
                .await
                .unwrap();
            id
        };

        // Fresh runtime over the same home: boot replay rehydrates the card.
        let sender = Arc::new(RecordingMailSender::new());
        let rt2 = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();
        let pending = rt2.pending_approvals();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0].id, approval_id, "the ORIGINAL id, not a new one");

        rt2.resolve_approval(&approval_id, Verdict::Approve, operator())
            .await
            .unwrap();
        assert_eq!(
            sender.sent().len(),
            1,
            "a card approved after a restart must still mail"
        );
        assert_eq!(sender.sent()[0].1.to, "stranger@ext.com");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn email_send_effect_without_mail_errors() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let email_effect = Effect {
            kind: "email.send".into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: true,
            first_time_counterparty: false,
            payload: serde_json::json!({ "to": "x@ext.com", "subject": "Hi", "body": "yo" }),
            agent: None,
            run_id: None,
        };
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(EffectBrain {
                effect: email_effect,
            }))
            .build()
            .await
            .unwrap();

        let err = perform_effect(
            &rt,
            &Effect {
                kind: "email.send".into(),
                group: EffectGroup::Send,
                amount_usd: None,
                established_thread: true,
                first_time_counterparty: false,
                payload: serde_json::json!({ "to": "x@ext.com", "subject": "Hi", "body": "yo" }),
                agent: None,
                run_id: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("email is not configured"));
    }

    #[tokio::test]
    async fn established_true_only_after_inbound_from_recipient() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_mail(CompanyMail {
                sender: Arc::new(RecordingMailSender::new()),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();

        assert!(!recipient_is_established(&rt, "x@ext.com").await);

        rt.inbox()
            .append(
                rt.id(),
                &crate::ports::inbox::EmailRecord {
                    id: "1".into(),
                    inbox: "ceo".into(),
                    from_name: "".into(),
                    from_email: "x@ext.com".into(),
                    subject: "hi".into(),
                    body: "".into(),
                    at_millis: 0,
                    read: false,
                    outbound: false,
                },
            )
            .await
            .unwrap();

        assert!(recipient_is_established(&rt, "X@EXT.COM").await);
    }

    #[tokio::test]
    async fn send_email_without_mail_returns_clean_error() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        // No `.with_mail(..)`: the company has no mailbox wired at all.
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(rt.id().clone(), "cyc-nomail".into(), &rt);

        let res = host
            .send_email(serde_json::json!({ "to": "x@ext.com", "subject": "s", "body": "b" }))
            .await
            .unwrap();
        assert!(!res.ok);
        assert!(
            res.output["error"]
                .as_str()
                .unwrap_or_default()
                .contains("not configured")
        );
    }

    #[tokio::test]
    async fn send_email_bad_args_missing_to_yields_no_effect() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(rt.id().clone(), "cyc-bad".into(), &rt);

        let res = host
            .send_email(serde_json::json!({ "subject": "s", "body": "b" }))
            .await
            .unwrap();
        assert!(!res.ok);
        assert!(res.output["error"].is_string());
    }

    #[tokio::test]
    async fn send_email_parks_for_new_recipient() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(rt.id().clone(), "cyc-park".into(), &rt);

        let res = host
            .send_email(serde_json::json!({ "to": "new@ext.com", "subject": "s", "body": "b" }))
            .await
            .unwrap();
        assert_eq!(res.output["status"], "pending_approval");
        assert_eq!(sender.sent().len(), 0);
    }

    #[tokio::test]
    async fn send_email_sends_for_established_recipient() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();
        rt.inbox()
            .append(
                rt.id(),
                &crate::ports::inbox::EmailRecord {
                    id: "1".into(),
                    inbox: "ceo".into(),
                    from_name: "".into(),
                    from_email: "known@ext.com".into(),
                    subject: "hi".into(),
                    body: "".into(),
                    at_millis: 0,
                    read: false,
                    outbound: false,
                },
            )
            .await
            .unwrap();
        let host = CycleHostImpl::new(rt.id().clone(), "cyc-send".into(), &rt);

        let res = host
            .send_email(serde_json::json!({ "to": "known@ext.com", "subject": "s", "body": "b" }))
            .await
            .unwrap();
        assert_eq!(res.output["status"], "sent");
        assert_eq!(sender.sent().len(), 1);
    }

    /// Issue #232: the established-correspondent gate must not weaken as the
    /// inbox grows.
    ///
    /// [`InboxStore::messages`] returns **oldest-first**, so the old
    /// `messages(.., 500, 0)` scan only ever saw the 500 *oldest* messages.
    /// Past that size every newer correspondent read as unknown, and every
    /// reply to a real thread parked for approval — an approval queue nobody
    /// can distinguish from noise is an approval queue everyone rubber-stamps.
    ///
    /// So the correspondent here is filed **last**, past the old cap. Policy is
    /// `full` (every effect executes) to isolate the flags on the effect from
    /// the gate decision they feed: this asserts what the send path *believes*
    /// about the recipient, not what the policy did with that belief.
    #[tokio::test]
    async fn established_recipient_past_the_old_page_cap_is_not_first_time() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();

        let file = async |id: usize, from: &str| {
            rt.inbox()
                .append(
                    rt.id(),
                    &crate::ports::inbox::EmailRecord {
                        id: format!("m{id}"),
                        inbox: "ceo".into(),
                        from_name: String::new(),
                        from_email: from.to_string(),
                        subject: "hi".into(),
                        body: String::new(),
                        at_millis: id as u64,
                        read: false,
                        outbound: false,
                    },
                )
                .await
                .unwrap();
        };

        // 501 older messages from other people, so the real correspondent lands
        // at index 501 — one past the end of the old 500-message page.
        for i in 0..501 {
            file(i, &format!("filler{i}@ext.com")).await;
        }
        file(501, "known@ext.com").await;

        let host = CycleHostImpl::new(rt.id().clone(), "cyc-deep".into(), &rt);
        let res = host
            .send_email(serde_json::json!({ "to": "known@ext.com", "subject": "s", "body": "b" }))
            .await
            .unwrap();
        assert_eq!(res.output["status"], "sent");

        let (executed, parked) = host.into_outcomes();
        assert!(parked.is_empty(), "an established thread must not park");
        let effect = executed
            .iter()
            .find(|e| e.kind == EMAIL_SEND_KIND)
            .expect("the send path emitted an email.send effect");
        assert!(
            effect.established_thread,
            "a correspondent who wrote in past message 500 is still established"
        );
        assert!(
            !effect.first_time_counterparty,
            "a correspondent who wrote in is never a first-time counterparty"
        );
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    // -----------------------------------------------------------------------
    // Issue #176: delegation host arms + handed-task awareness.
    // -----------------------------------------------------------------------

    /// A manifest with an Engineering desk (`eng`, lead `eng1`) — for the desk
    /// resolution paths of `delegate_to_desk` and the awareness matcher.
    fn desk_manifest() -> CompanyManifest {
        let toml_src = r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "chief"
            role = "Chief"
            tier = "orchestrator"

            [[agent]]
            id = "eng1"
            role = "Engineer"

            [[group_chat]]
            id = "eng"
            name = "Engineering"
            members = ["eng1"]

            [policy]
            mode = "full"
            "#;
        toml::from_str(toml_src).expect("parse desk manifest")
    }

    /// A brain that records the text of every operator message it is handed, so
    /// a test can assert what awareness the kernel folded in before the brain.
    struct CapturingBrain {
        seen: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl Brain for CapturingBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            for event in &req.events {
                if let CompanyEvent::OperatorMessage { text, .. } = event {
                    self.seen.lock().expect("seen").push(text.clone());
                }
            }
            Ok(CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "capture")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn spawn_task_arm_opens_a_board_card() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(rt.id().clone(), "cyc".into(), &rt);

        let res = host
            .spawn_task(serde_json::json!({ "title": "  Ship it ", "assignee": " eng " }))
            .await
            .unwrap();
        assert!(res.ok);
        assert_eq!(res.output["status"], "queued");

        let cards = rt.tasks().list(rt.id()).await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].title, "Ship it");
        assert_eq!(cards[0].assignee, "eng");
        // Intake lands in To-do, never on the dispatch edge: a spawned card
        // must not spend an agent turn before an operator has seen it.
        assert_eq!(cards[0].column, COLUMN_TODO);

        // A blank title is a clean tool error, no card.
        let bad = host
            .spawn_task(serde_json::json!({ "title": "  " }))
            .await
            .unwrap();
        assert!(!bad.ok);
        assert_eq!(rt.tasks().list(rt.id()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delegate_to_desk_arm_records_handoff_and_rejects_unknown_desk() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), desk_manifest())
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(rt.id().clone(), "cyc".into(), &rt);

        // Known desk (by name) → card assigned to the resolved desk id, lead noted.
        let ok = host
            .delegate_to_desk(
                serde_json::json!({ "desk": "Engineering", "instruction": "build invoicing" }),
            )
            .await
            .unwrap();
        assert!(ok.ok);
        assert_eq!(ok.output["desk"], "eng");
        assert_eq!(ok.output["lead"], "eng1");

        // Unknown desk → clean error, no card.
        let bad = host
            .delegate_to_desk(serde_json::json!({ "desk": "Legal", "instruction": "review" }))
            .await
            .unwrap();
        assert!(!bad.ok);
        assert_eq!(bad.output["status"], "unknown_desk");

        let cards = rt.tasks().list(rt.id()).await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].assignee, "eng");
        // Same as the spawn path: a handoff opens a card, it does not dispatch.
        assert_eq!(cards[0].column, COLUMN_TODO);
    }

    #[tokio::test]
    async fn call_tool_dispatches_delegation_tools() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(rt.id().clone(), "cyc".into(), &rt);

        // Reached through the CycleHost trait exactly as the hosted brain does.
        let res = host
            .call_tool(ToolCall {
                tool: SPAWN_TASK_TOOL.to_string(),
                args: serde_json::json!({ "title": "via call_tool" }),
            })
            .await
            .unwrap();
        assert!(res.ok);
        assert_eq!(rt.tasks().list(rt.id()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn handed_task_awareness_surfaces_open_cards_on_a_direct_query() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let rt = RuntimeBuilder::new(home.clone(), desk_manifest())
            .with_brain(Arc::new(CapturingBrain { seen: seen.clone() }))
            .build()
            .await
            .unwrap();

        // Hand work to the Engineering desk (card assigned to the desk id).
        rt.tasks()
            .upsert(
                rt.id(),
                &TaskRecord {
                    id: "t1".into(),
                    title: "Ship invoicing".into(),
                    note: Some("build the importer".into()),
                    column: COLUMN_TODO.into(),
                    priority: "medium".into(),
                    assignee: "eng".into(),
                    updated_at_millis: 0,
                    origin_chat_id: None,
                    parent_task_id: None,
                },
            )
            .await
            .unwrap();

        // Asking the desk directly (by name) surfaces the handed task...
        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "what are you working on?".into(),
            by: None,
            chat: Some("Engineering".into()),
        }])
        .await
        .unwrap();

        // ...and asking with no address (the orchestrator) does NOT get the
        // desk's briefing folded into it.
        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "status?".into(),
            by: None,
            chat: None,
        }])
        .await
        .unwrap();

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert!(
            seen[0].contains("Open work already handed to you")
                && seen[0].contains("Ship invoicing"),
            "direct query carries the briefing: {:?}",
            seen[0]
        );
        assert!(
            !seen[1].contains("Open work already handed to you"),
            "unaddressed query has no desk briefing: {:?}",
            seen[1]
        );
    }

    #[tokio::test]
    async fn awareness_skips_done_cards() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let rt = RuntimeBuilder::new(home.clone(), desk_manifest())
            .with_brain(Arc::new(CapturingBrain { seen: seen.clone() }))
            .build()
            .await
            .unwrap();
        rt.tasks()
            .upsert(
                rt.id(),
                &TaskRecord {
                    id: "t1".into(),
                    title: "Already finished".into(),
                    note: None,
                    column: "done".into(),
                    priority: "medium".into(),
                    assignee: "eng".into(),
                    updated_at_millis: 0,
                    origin_chat_id: None,
                    parent_task_id: None,
                },
            )
            .await
            .unwrap();
        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "what's up?".into(),
            by: None,
            chat: Some("eng".into()),
        }])
        .await
        .unwrap();
        let seen = seen.lock().unwrap().clone();
        assert!(
            !seen[0].contains("Open work already handed to you"),
            "done cards are not surfaced as open work: {:?}",
            seen[0]
        );
    }
}
