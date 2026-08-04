//! Shared wire→kernel mapping for the Medulla-family brains.
//!
//! [`HostedMedullaBrain`](crate::brain::HostedMedullaBrain) and the
//! feature-gated [`SidecarBrain`](crate::brain::sidecar::SidecarBrain) drain the
//! *same* `/orchestration/v1` frames, so the translation from a wire frame into
//! a kernel [`Effect`], [`ContextOp`], channel response, or ledger delta lives
//! here once. Both brains import these `pub(crate)` helpers rather than keeping
//! two copies of the mapping.

use serde_json::{Value, json};

use crate::ports::now_millis;
use crate::ports::types::{
    ChunkAddr, CompanyEvent, ContextChunk, ContextOp, ContextOpResult, Effect, EffectGroup,
    LedgerEntry, OutboundMessage, Verdict,
};
use crate::ports::workflow_runner::DeliveryStatus;

use super::wire::{EffectFrame, Role, WireEvent};

/// The device-tool name prefix that routes a tool call to the context store.
pub(crate) const CONTEXT_TOOL_PREFIX: &str = "context_";

/// What one executed effect contributed to the cycle result.
#[derive(Default)]
pub(crate) struct EffectOutcome {
    /// A channel response produced by an executed `Send`-group effect.
    pub(crate) channel_response: Option<OutboundMessage>,
    /// A ledger delta produced by an executed money-moving effect.
    pub(crate) ledger_delta: Option<LedgerEntry>,
    /// Whether the effect warrants a world-diff upload.
    pub(crate) notable: bool,
}

/// Normalizes a [`CompanyEvent`] into the [`WireEvent`] `POST /events` carries.
pub(crate) fn wire_event(seq: u64, event: &CompanyEvent) -> WireEvent {
    let (role, sender, body, kind) = match event {
        CompanyEvent::OperatorMessage { text, .. } => (
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
        CompanyEvent::LifecycleChanged { from, to, by } => (
            Role::System,
            by.id.clone(),
            format!("Lifecycle changed from {from} to {to}"),
            "lifecycle.changed",
        ),
        CompanyEvent::AgentReply {
            chat_id,
            agent_id,
            text,
            ..
        } => (
            Role::Assistant,
            agent_id.clone(),
            format!("[{chat_id}] {text}"),
            "agent.reply",
        ),
        CompanyEvent::MemoryFactDeleted { fact_id } => (
            Role::System,
            "operator".to_string(),
            format!("Deleted memory fact {fact_id}"),
            "memory.fact_deleted",
        ),
        CompanyEvent::TaskDispatched { task_id, .. } => (
            Role::System,
            "board".to_string(),
            format!("Dispatched task {task_id}"),
            "task.dispatched",
        ),
        CompanyEvent::McpCallFailed {
            server,
            tool,
            status,
            message,
            ..
        } => (
            Role::System,
            "mcp".to_string(),
            format!("MCP call to {server}/{tool} failed ({status}): {message}"),
            "mcp.call_failed",
        ),
        // The dispatch terminal (#185). `output` is deliberately not wired:
        // the landing column is what a reader needs here, and the full result
        // text already rides the task's own timeline.
        CompanyEvent::DeskTaskCompleted {
            task_id,
            desk,
            column,
            ..
        } => (
            Role::System,
            "board".to_string(),
            format!("Task {task_id} finished on {desk} → {column}"),
            "task.completed",
        ),
        CompanyEvent::WorkflowCreated {
            workflow_id, name, ..
        } => (
            Role::System,
            "workflow".to_string(),
            format!("Created workflow {name} ({workflow_id})"),
            "workflow.created",
        ),
        // Issue #259. Id + name only, exactly like the create arm above — the
        // variant carries no graph body precisely so this wire-out to the
        // inference sidecar cannot leak one.
        CompanyEvent::WorkflowUpdated {
            workflow_id, name, ..
        } => (
            Role::System,
            "workflow".to_string(),
            format!("Updated workflow {name} ({workflow_id})"),
            "workflow.updated",
        ),
        CompanyEvent::WorkflowDeleted {
            workflow_id, name, ..
        } => (
            Role::System,
            "workflow".to_string(),
            format!("Deleted workflow {name} ({workflow_id})"),
            "workflow.deleted",
        ),
        // Action-only body: the operator's redirect instruction is never wired.
        CompanyEvent::TaskSteered {
            task_id, action, ..
        } => (
            Role::System,
            "operator".to_string(),
            format!("Steered task {task_id} ({action})"),
            "task.steered",
        ),
        // A finished workflow run (#228). Counts and the failure reason only —
        // NOT the per-row `target` or `detail`. A delivery row's target is a
        // recipient's email address and its detail can quote one, and this body
        // is wired out to the inference sidecar; the console reads the full
        // rows from the journal instead, where they belong to the operator.
        //
        // Issue #248 pinned this with a test rather than leaving it a comment:
        // the exclusion is the journal-boundary half of the same rule the
        // scheduler's log line follows, and an unpinned rule is one refactor
        // away from not being true.
        CompanyEvent::WorkflowRunFinished {
            workflow_id,
            scheduled,
            deliveries,
            pending_approvals,
            error,
            ..
        } => {
            let how = if *scheduled { "Scheduled" } else { "Manual" };
            let body = match error {
                Some(err) => format!("{how} run of workflow {workflow_id} failed: {err}"),
                None => {
                    let undelivered = deliveries
                        .iter()
                        .filter(|d| !matches!(d.status, DeliveryStatus::Sent))
                        .count();
                    format!(
                        "{how} run of workflow {workflow_id} finished — {} report(s) routed, \
                         {undelivered} not delivered, {} pending approval",
                        deliveries.len(),
                        pending_approvals.len(),
                    )
                }
            };
            (Role::System, "workflow".to_string(), body, "workflow.run")
        }
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
pub(crate) fn verdict_word(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Approve => "approved",
        Verdict::Deny => "denied",
    }
}

/// Builds an [`Effect`] from an effect frame, classifying its supervised group
/// and lifting `amountUsd` / thread flags out of the payload for the gate.
pub(crate) fn effect_from_frame(frame: &EffectFrame) -> Effect {
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
        agent: None,
        run_id: None,
    }
}

/// Maps a dotted effect kind to its supervised-policy [`EffectGroup`].
pub(crate) fn effect_group_for(kind: &str) -> EffectGroup {
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
pub(crate) fn channel_message_from_effect(effect: &Effect) -> Option<OutboundMessage> {
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
    Some(OutboundMessage {
        task_id: None,
        channel,
        text,
        steps: Vec::new(),
        reply_to: None,
    })
}

/// Records a ledger delta for an executed effect that moved money.
pub(crate) fn ledger_delta_from_effect(effect: &Effect) -> Option<LedgerEntry> {
    let amount = effect.amount_usd?;
    Some(LedgerEntry {
        at_millis: now_millis(),
        kind: effect.kind.clone(),
        amount_usd: amount,
        memo: format!("medulla effect {}", effect.kind),
    })
}

/// Whether an executed effect warrants a world-diff upload.
pub(crate) fn is_notable(effect: &Effect) -> bool {
    !matches!(effect.group, EffectGroup::Other | EffectGroup::Send)
}

/// Maps a `context_*` device tool call into a [`ContextOp`], or `None` when the
/// tool is not a context tool.
pub(crate) fn context_op_from_call(name: &str, args: &Value) -> Option<ContextOp> {
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
pub(crate) fn context_result_to_value(result: ContextOpResult) -> Value {
    match result {
        ContextOpResult::Addr(addr) => json!({ "addr": addr.as_ref() }),
        ContextOpResult::Metas(metas) => serde_json::to_value(metas).unwrap_or(Value::Null),
        ContextOpResult::Text(text) => json!({ "text": text }),
        ContextOpResult::Hits(hits) => serde_json::to_value(hits).unwrap_or(Value::Null),
    }
}

pub(crate) fn payload_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

pub(crate) fn payload_f64(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

pub(crate) fn payload_bool(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::workflow_runner::{DeliveryReason, DeliveryReport};

    /// A recipient address for fixtures. `.invalid` is reserved by RFC 2606 and
    /// can never resolve, so a fixture that escapes names nobody.
    const RECIPIENT: &str = "recipient@example.invalid";

    /// **Issue #248, one layer below the log line.** A company's journal is a
    /// single append-only log shared by chat, audit and run history, and this
    /// function is the seam where it is wired out to the inference sidecar —
    /// a reader that is not the tenant. A `WorkflowRunFinished` row's `target`
    /// is a recipient's address and its `detail` quotes one on the
    /// transport-failure arms, so neither may cross here.
    ///
    /// The exclusion was already written this way by #228; this pins it, so a
    /// later "just include the detail, it is more informative" edit fails CI
    /// instead of quietly widening the boundary.
    #[test]
    fn a_finished_run_wires_out_counts_without_the_recipient_or_the_transport_text() {
        let event = CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".to_string(),
            scheduled: true,
            run_id: None,
            deliveries: vec![DeliveryReport {
                node: "owner_summary".to_string(),
                kind: "email".to_string(),
                target: Some(RECIPIENT.to_string()),
                status: DeliveryStatus::Failed,
                detail: format!(
                    "the mail transport refused the message: 550 5.1.1 <{RECIPIENT}>: Recipient \
                     address rejected"
                ),
                reason: DeliveryReason::MailTransportRefused,
            }],
            pending_approvals: Vec::new(),
            error: None,
        };

        let wired = wire_event(7, &event);

        assert!(!wired.body.contains(RECIPIENT), "{}", wired.body);
        assert!(!wired.body.contains("recipient@"), "{}", wired.body);
        assert!(
            !wired.body.contains("Recipient address rejected"),
            "{}",
            wired.body
        );
        assert!(!wired.body.contains("550"), "{}", wired.body);
        // Still says what happened, in counts.
        assert!(wired.body.contains("digest"), "{}", wired.body);
        assert!(wired.body.contains("1 not delivered"), "{}", wired.body);
        assert_eq!(wired.kind, "workflow.run");
    }
}
