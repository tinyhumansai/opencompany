//! [`HostedMedullaBrain`]: the [`Brain`] that drives hosted Medulla cognition
//! over a [`MedullaTransport`].
//!
//! One company is one Medulla session: the `sessionId` is the [`CompanyId`] and
//! the `counterpartAgentId` is `opencompany:<slug>`. Each [`CycleRequest`] event
//! is posted as its own `POST /events` keyed on the durable
//! [`EventLog`](crate::ports::EventLog) sequence, and the returned cycle's frames
//! are drained:
//!
//! - Every **effect** frame passes through [`CycleHost::emit_effect`] — the
//!   approval gate — *before* it is acked. An executed effect acks `ok:true`; a
//!   parked or denied effect acks `ok:false` with the reason, so Medulla hears
//!   the gate's verdict rather than a silent success.
//! - Every **tool_call** frame is serviced through [`CycleHost::call_tool`]
//!   (or [`CycleHost::context_op`] for the context device-tools) and answered.
//! - Every **usage** frame is folded into the cycle's
//!   [`TokenUsage`] total. The model runs upstream, so this frame is the only way
//!   the host can know what a hosted cycle cost; the runtime's cycle loop meters
//!   whatever lands here onto the Usage/Finances surfaces (issue #174). A cycle
//!   that carries no usage frame is metered as zero rather than guessed at.
//! - Frames are deduped on `callId`, matching the at-least-once delivery
//!   contract, so a replay is handled exactly once — including usage reports,
//!   which would otherwise double-charge.
//!
//! Cycle summaries are journaled as [`CompressedTrace`]s, spend effects become
//! ledger deltas, and notable effects trigger a `POST /world-diff`. The brain
//! never fabricates a channel response: the ones it returns are the `send_dm`
//! effects Medulla executed.
//!
//! The transport is abstracted, so the brain and its whole test surface live in
//! the default build against [`MockTransport`](super::medulla::MockTransport);
//! the networked `HttpSocketTransport` lands behind the `medulla` feature.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::StreamExt;

use crate::Result;
use crate::metering::MEDULLA_PROVIDER;
use crate::ports::brain::{Brain, Cognition, CycleHost, UsageMetering};
use crate::ports::now_millis;
use crate::ports::types::{
    CompanyId, CompressedTrace, CycleRequest, CycleResult, EffectDisposition, SecretValue,
    TokenUsage, ToolCall,
};

use super::medulla::effects::{
    EffectOutcome, channel_message_from_effect, context_op_from_call, context_result_to_value,
    effect_from_frame, is_notable, ledger_delta_from_effect, wire_event,
};
use super::medulla::transport::{InboundFrame, MedullaTransport};
use super::medulla::wire::{
    EffectFrame, EffectResult, EventsRequest, ToolCallFrame, ToolManifestEntry, ToolResultFrame,
    WorldDiffEntry, WorldDiffRequest,
};

/// The hosted Medulla brain: one company's cognition over a transport.
pub struct HostedMedullaBrain {
    transport: Arc<dyn MedullaTransport>,
    session_id: String,
    counterpart: String,
    /// The hosted-brain bearer credential. Held for parity with the transport
    /// and redacted in [`std::fmt::Debug`]; never logged or serialized.
    credential: SecretValue,
    tool_catalog: Vec<ToolManifestEntry>,
    registered: AtomicBool,
}

impl HostedMedullaBrain {
    /// Builds a hosted brain for `company` addressed as `opencompany:<slug>`.
    ///
    /// `slug` is the company's manifest-derived slug (typically the same string
    /// as the [`CompanyId`]); `credential` is the TinyHumans bearer token and is
    /// never logged. `tool_catalog` is the device-tool manifest registered with
    /// Medulla on the first cycle.
    pub fn new(
        transport: Arc<dyn MedullaTransport>,
        company: &CompanyId,
        slug: &str,
        credential: SecretValue,
        tool_catalog: Vec<ToolManifestEntry>,
    ) -> Self {
        Self {
            transport,
            session_id: company.as_ref().to_string(),
            counterpart: format!("opencompany:{slug}"),
            credential,
            tool_catalog,
            registered: AtomicBool::new(false),
        }
    }

    /// The Medulla session id (the company id).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The counterpart agent id (`opencompany:<slug>`).
    pub fn counterpart(&self) -> &str {
        &self.counterpart
    }

    /// Registers the device-tool manifest exactly once (on the first cycle).
    ///
    /// A no-op when the catalog is empty. The `registered` flag is flipped with
    /// `compare_exchange` so concurrent first cycles register at most once.
    async fn ensure_registered(&self) -> Result<()> {
        if self.tool_catalog.is_empty() {
            return Ok(());
        }
        if self
            .registered
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.transport
                .register_tools(self.tool_catalog.clone())
                .await?;
        }
        Ok(())
    }

    /// Services one `orch:tool_call` frame and builds its answer.
    ///
    /// Context device-tools (`context_*`) route to [`CycleHost::context_op`];
    /// everything else routes to [`CycleHost::call_tool`], which enforces the
    /// manifest grant. A host error (e.g. an ungranted tool) becomes an
    /// `ok:false` answer rather than aborting the cycle.
    async fn service_tool_call(
        &self,
        host: &dyn CycleHost,
        call: &ToolCallFrame,
    ) -> Result<ToolResultFrame> {
        if let Some(op) = context_op_from_call(&call.name, &call.args) {
            return Ok(match host.context_op(op).await {
                Ok(result) => ToolResultFrame {
                    call_id: call.call_id.clone(),
                    ok: true,
                    result: Some(context_result_to_value(result)),
                    error: None,
                },
                Err(err) => ToolResultFrame {
                    call_id: call.call_id.clone(),
                    ok: false,
                    result: None,
                    error: Some(err.to_string()),
                },
            });
        }

        let tool_call = ToolCall {
            tool: call.name.clone(),
            args: call.args.clone(),
        };
        Ok(match host.call_tool(tool_call).await {
            Ok(result) => ToolResultFrame {
                call_id: call.call_id.clone(),
                ok: result.ok,
                result: Some(result.output),
                error: None,
            },
            Err(err) => ToolResultFrame {
                call_id: call.call_id.clone(),
                ok: false,
                result: None,
                error: Some(err.to_string()),
            },
        })
    }

    /// Passes an effect frame through the gate, acks it, and returns the channel
    /// response and ledger delta it produced (if any).
    async fn service_effect(
        &self,
        host: &dyn CycleHost,
        frame: &EffectFrame,
    ) -> Result<EffectOutcome> {
        let effect = effect_from_frame(frame);
        // The gate runs BEFORE the ack, so a parked effect acks `ok:false` and
        // Medulla learns it must wait rather than seeing a false success.
        let disposition = host.emit_effect(effect.clone()).await?;

        let mut outcome = EffectOutcome::default();
        let ack = match &disposition {
            EffectDisposition::Executed => {
                outcome.channel_response = channel_message_from_effect(&effect);
                outcome.ledger_delta = ledger_delta_from_effect(&effect);
                outcome.notable = is_notable(&effect);
                EffectResult {
                    call_id: frame.call_id.clone(),
                    ok: true,
                    error: None,
                    result: None,
                }
            }
            EffectDisposition::PendingApproval(id) => EffectResult {
                call_id: frame.call_id.clone(),
                ok: false,
                error: Some(format!("pending approval ({id})")),
                result: None,
            },
            EffectDisposition::Denied { reason } => EffectResult {
                call_id: frame.call_id.clone(),
                ok: false,
                error: Some(reason.clone()),
                result: None,
            },
        };
        self.transport.ack_effect(ack).await?;
        Ok(outcome)
    }
}

#[async_trait]
impl Brain for HostedMedullaBrain {
    async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
        self.ensure_registered().await?;

        let mut channel_responses = Vec::new();
        let mut new_traces = Vec::new();
        let mut ledger_deltas = Vec::new();
        let mut world_notes: Vec<WorldDiffEntry> = Vec::new();
        // What the hosted side reported this cycle's inference cost. Folded from
        // `orch:usage` frames; stays zero when the upstream reports none, which
        // the runtime then meters as nothing rather than as a fake charge.
        let mut token_usage = TokenUsage::default();
        for (index, event) in req.events.iter().enumerate() {
            // Prefer the durable EventLog seq; fall back to the position when a
            // caller did not thread seqs (idempotency then holds within a cycle).
            let seq = req
                .event_seqs
                .get(index)
                .map(|s| s.value())
                .unwrap_or(index as u64);

            let request = EventsRequest {
                counterpart_agent_id: self.counterpart.clone(),
                session_id: self.session_id.clone(),
                event: wire_event(seq, event),
            };
            let accepted = self.transport.post_events(request).await?;
            let cycle_id = accepted.cycle_id;

            // Drain the cycle's frames, deduping on callId (at-least-once).
            let mut seen: HashSet<String> = HashSet::new();
            // Provider adapters reject a model response that co-batches
            // request_approval with another call. This guard is the transport
            // backstop for any later frames in the same orchestration cycle;
            // answers remain streaming because the peer may await each one.
            let mut approval_requested = false;
            let mut frames = self.transport.cycle_frames(&cycle_id);
            while let Some(frame) = frames.next().await {
                match frame? {
                    InboundFrame::CycleComplete => break,
                    InboundFrame::Effect(effect_frame) => {
                        if !seen.insert(effect_frame.call_id.clone()) {
                            continue;
                        }
                        if approval_requested {
                            self.transport
                                .ack_effect(EffectResult {
                                    call_id: effect_frame.call_id,
                                    ok: false,
                                    error: Some(
                                        "effect cannot execute after a request_approval boundary"
                                            .to_string(),
                                    ),
                                    result: None,
                                })
                                .await?;
                            continue;
                        }
                        let outcome = self.service_effect(host, &effect_frame).await?;
                        if let Some(message) = outcome.channel_response {
                            channel_responses.push(message);
                        }
                        if let Some(delta) = outcome.ledger_delta {
                            ledger_deltas.push(delta);
                        }
                        if outcome.notable {
                            world_notes.push(WorldDiffEntry {
                                seq,
                                note: format!("executed {}", effect_frame.kind),
                                ts: now_millis() as i64,
                            });
                        }
                    }
                    InboundFrame::ToolCall(call) => {
                        if !seen.insert(call.call_id.clone()) {
                            continue;
                        }
                        if approval_requested
                            && context_op_from_call(&call.name, &call.args).is_none()
                        {
                            self.transport
                                .answer_tool_call(ToolResultFrame {
                                    call_id: call.call_id,
                                    ok: false,
                                    result: None,
                                    error: Some(
                                        "tool cannot execute after a request_approval boundary"
                                            .to_string(),
                                    ),
                                })
                                .await?;
                            continue;
                        }
                        let requests_approval =
                            call.name == crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND;
                        let answer = self.service_tool_call(host, &call).await?;
                        approval_requested |= requests_approval && answer.ok;
                        self.transport.answer_tool_call(answer).await?;
                        // Keep draining: every later sibling frame must receive
                        // its explicit refusal instead of being abandoned.
                    }
                    InboundFrame::Usage(usage) => {
                        // Deduped like every other frame: delivery is
                        // at-least-once, and a replayed usage report would
                        // double-charge the meter and the ledger.
                        if !seen.insert(usage.call_id.clone()) {
                            continue;
                        }
                        let reported = usage.to_token_usage();
                        tracing::debug!(
                            session = %self.session_id,
                            cycle = %usage.cycle_id,
                            call_id = %usage.call_id,
                            input = reported.input,
                            output = reported.output,
                            cached_input = reported.cached_input,
                            cost_usd = reported.cost_usd,
                            "[medulla] folding a reported usage frame into the cycle total"
                        );
                        token_usage.fold(&reported);
                    }
                }
            }

            new_traces.push(CompressedTrace::now(
                &cycle_id,
                format!("medulla cycle for `{}` (seq {seq})", self.session_id),
            ));
        }

        // Upload a world-diff after notable effects (payments, filings, etc.).
        if !world_notes.is_empty() {
            self.transport
                .post_world_diff(WorldDiffRequest {
                    session_id: self.session_id.clone(),
                    entries: world_notes,
                })
                .await?;
        }

        Ok(CycleResult {
            channel_responses,
            new_traces,
            ledger_deltas,
            token_usage,
        })
    }

    /// Hosted cognition: the model runs upstream, so usage arrives per cycle on
    /// the [`USAGE`](super::medulla::wire::USAGE) frame and the runtime meters it
    /// under [`MEDULLA_PROVIDER`].
    fn cognition(&self) -> Cognition {
        Cognition {
            path: "hosted",
            provider: MEDULLA_PROVIDER,
            // The model is chosen upstream and never named on the `orch:usage`
            // frame, so this process cannot honestly report one (issue #1749).
            model: None,
            metering: UsageMetering::PerCycle,
        }
    }
}

/// A hand-written `Debug` that redacts the credential so it can never reach a
/// log line or panic message.
impl std::fmt::Debug for HostedMedullaBrain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Borrow the credential so it counts as held/used, but never render its
        // bytes — only the redaction marker reaches the formatter.
        let _held = &self.credential;
        f.debug_struct("HostedMedullaBrain")
            .field("session_id", &self.session_id)
            .field("counterpart", &self.counterpart)
            .field("credential", &"<redacted>")
            .field("tools", &self.tool_catalog.len())
            .field("registered", &self.registered.load(Ordering::Acquire))
            .finish()
    }
}

#[cfg(test)]
mod test;
