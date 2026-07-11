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
//! - Frames are deduped on `callId`, matching the at-least-once delivery
//!   contract, so a replay is handled exactly once.
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
use serde_json::{Value, json};

use crate::Result;
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::now_millis;
use crate::ports::types::{
    ChunkAddr, CompanyEvent, CompanyId, CompressedTrace, ContextChunk, ContextOp, ContextOpResult,
    CycleRequest, CycleResult, Effect, EffectDisposition, EffectGroup, LedgerEntry,
    OutboundMessage, SecretValue, TokenUsage, ToolCall, Verdict,
};

use super::medulla::transport::{InboundFrame, MedullaTransport};
use super::medulla::wire::{
    EffectFrame, EffectResult, EventsRequest, Role, ToolCallFrame, ToolManifestEntry,
    ToolResultFrame, WireEvent, WorldDiffEntry, WorldDiffRequest,
};

/// The device-tool name prefix that routes a tool call to the context store.
const CONTEXT_TOOL_PREFIX: &str = "context_";

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

/// What one executed effect contributed to the cycle result.
#[derive(Default)]
struct EffectOutcome {
    channel_response: Option<OutboundMessage>,
    ledger_delta: Option<LedgerEntry>,
    notable: bool,
}

#[async_trait]
impl Brain for HostedMedullaBrain {
    async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
        self.ensure_registered().await?;

        let mut channel_responses = Vec::new();
        let mut new_traces = Vec::new();
        let mut ledger_deltas = Vec::new();
        let mut world_notes: Vec<WorldDiffEntry> = Vec::new();

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
            let mut frames = self.transport.cycle_frames(&cycle_id);
            while let Some(frame) = frames.next().await {
                match frame? {
                    InboundFrame::CycleComplete => break,
                    InboundFrame::Effect(effect_frame) => {
                        if !seen.insert(effect_frame.call_id.clone()) {
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
                        let answer = self.service_tool_call(host, &call).await?;
                        self.transport.answer_tool_call(answer).await?;
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
            token_usage: TokenUsage::default(),
        })
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

// ---------------------------------------------------------------------------
// Event, effect, and context mapping helpers
// ---------------------------------------------------------------------------

/// Normalizes a [`CompanyEvent`] into the [`WireEvent`] `POST /events` carries.
fn wire_event(seq: u64, event: &CompanyEvent) -> WireEvent {
    let (role, sender, body, kind) = match event {
        CompanyEvent::OperatorMessage { text } => (
            Role::User,
            "operator".to_string(),
            text.clone(),
            "operator.message",
        ),
        CompanyEvent::WebhookReceived { channel, body } => (
            Role::User,
            channel.clone(),
            body.to_string(),
            "webhook.received",
        ),
        CompanyEvent::ScheduleFired { cron, prompt } => (
            Role::System,
            "scheduler".to_string(),
            format!("[{cron}] {prompt}"),
            "schedule.fired",
        ),
        CompanyEvent::A2aTaskReceived { from, task } => (
            Role::User,
            from.clone(),
            task.to_string(),
            "a2a.task_received",
        ),
        CompanyEvent::ApprovalResolved {
            approval_id,
            verdict,
            by,
        } => (
            Role::System,
            by.id.clone(),
            format!("{} approval {approval_id}", verdict_word(*verdict)),
            "approval.resolved",
        ),
        CompanyEvent::FeedbackFiled { note } => (
            Role::User,
            "operator".to_string(),
            note.clone(),
            "feedback.filed",
        ),
        CompanyEvent::PaymentReceived { amount_usd, memo } => (
            Role::System,
            "ledger".to_string(),
            format!("received ${amount_usd}: {memo}"),
            "payment.received",
        ),
    };
    WireEvent {
        seq,
        role,
        sender,
        body,
        ts: now_millis() as i64,
        kind: kind.to_string(),
    }
}

/// The lowercase wire word for an operator verdict.
fn verdict_word(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Approve => "approved",
        Verdict::Deny => "denied",
    }
}

/// Builds an [`Effect`] from an effect frame, classifying its supervised group
/// and lifting `amountUsd` / thread flags out of the payload for the gate.
fn effect_from_frame(frame: &EffectFrame) -> Effect {
    let payload = &frame.payload;
    Effect {
        kind: frame.kind.clone(),
        group: effect_group_for(&frame.kind),
        amount_usd: payload_f64(payload, "amountUsd")
            .or_else(|| payload_f64(payload, "amount_usd")),
        established_thread: payload_bool(payload, "establishedThread")
            .or_else(|| payload_bool(payload, "established_thread"))
            .unwrap_or(false),
        first_time_counterparty: payload_bool(payload, "firstTimeCounterparty")
            .or_else(|| payload_bool(payload, "first_time_counterparty"))
            .unwrap_or(false),
        payload: frame.payload.clone(),
    }
}

/// Maps a dotted effect kind to its supervised-policy [`EffectGroup`].
fn effect_group_for(kind: &str) -> EffectGroup {
    let k = kind.to_ascii_lowercase();
    if k.contains("send_dm") || k.contains("message") || k.contains("email") || k.contains("reply")
    {
        EffectGroup::Send
    } else if k.contains("payment")
        || k.contains("spend")
        || k.contains("x402")
        || k.contains("pay")
    {
        EffectGroup::Spend
    } else if k.contains("sign") || k.contains("filing") || k.contains("contract") {
        EffectGroup::Sign
    } else if k.contains("publish") {
        EffectGroup::Publish
    } else if k.contains("hire") || k.contains("engage") {
        EffectGroup::Hire
    } else if k.contains("identity") || k.contains("register") {
        EffectGroup::Identity
    } else {
        EffectGroup::Other
    }
}

/// Extracts a channel response from an executed `Send`-group effect.
///
/// Returns `None` for non-send effects. The channel is read from `channel`/`to`
/// and the text from `text`/`body`/`message`, so the runtime's own effect
/// executor (which only routes a `{channel,text}` pair) does not double-send
/// when the payload uses the `{to,body}` shape.
fn channel_message_from_effect(effect: &Effect) -> Option<OutboundMessage> {
    if effect.group != EffectGroup::Send {
        return None;
    }
    let payload = &effect.payload;
    let channel = payload_str(payload, "channel")
        .or_else(|| payload_str(payload, "to"))
        .unwrap_or("operator")
        .to_string();
    let text = payload_str(payload, "text")
        .or_else(|| payload_str(payload, "body"))
        .or_else(|| payload_str(payload, "message"))?
        .to_string();
    Some(OutboundMessage { channel, text })
}

/// Records a ledger delta for an executed effect that moved money.
fn ledger_delta_from_effect(effect: &Effect) -> Option<LedgerEntry> {
    let amount = effect.amount_usd?;
    Some(LedgerEntry {
        at_millis: now_millis(),
        kind: effect.kind.clone(),
        amount_usd: amount,
        memo: format!("medulla effect {}", effect.kind),
    })
}

/// Whether an executed effect warrants a world-diff upload.
fn is_notable(effect: &Effect) -> bool {
    !matches!(effect.group, EffectGroup::Other | EffectGroup::Send)
}

/// Maps a `context_*` device tool call into a [`ContextOp`], or `None` when the
/// tool is not a context tool.
fn context_op_from_call(name: &str, args: &Value) -> Option<ContextOp> {
    let op = name.strip_prefix(CONTEXT_TOOL_PREFIX)?;
    match op {
        "put" => Some(ContextOp::Put(ContextChunk {
            label: payload_str(args, "label").unwrap_or("").to_string(),
            body: payload_str(args, "body").unwrap_or("").to_string(),
        })),
        "list" => Some(ContextOp::List {
            prefix: payload_str(args, "prefix").unwrap_or("").to_string(),
        }),
        "peek" => Some(ContextOp::Peek {
            addr: ChunkAddr::new(payload_str(args, "addr").unwrap_or("")),
            range: None,
        }),
        "search" => Some(ContextOp::Search {
            query: payload_str(args, "query").unwrap_or("").to_string(),
            limit: payload_f64(args, "limit").map(|n| n as usize).unwrap_or(10),
        }),
        _ => None,
    }
}

/// Renders a [`ContextOpResult`] as the JSON a `tool_result` frame carries.
fn context_result_to_value(result: ContextOpResult) -> Value {
    match result {
        ContextOpResult::Addr(addr) => json!({ "addr": addr.as_ref() }),
        ContextOpResult::Metas(metas) => serde_json::to_value(metas).unwrap_or(Value::Null),
        ContextOpResult::Text(text) => json!({ "text": text }),
        ContextOpResult::Hits(hits) => serde_json::to_value(hits).unwrap_or(Value::Null),
    }
}

fn payload_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn payload_f64(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

fn payload_bool(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod test;
