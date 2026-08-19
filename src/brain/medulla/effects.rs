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
        // Issue #379: the brain is told a request is now waiting, so it can
        // reason about being blocked rather than only learning when the verdict
        // arrives. The kind is the effect's type name; the payload is not here
        // and never should be — the operator has not consented to it yet.
        CompanyEvent::ApprovalParked {
            approval_id,
            effect_kind,
            ..
        } => (
            Role::System,
            "approvals".to_string(),
            format!("parked {effect_kind} approval {approval_id}"),
            "approval.parked",
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
        // Issue #86: carried on the same terms as the lifecycle change beside
        // it. The operator's free-text reason is deliberately NOT sent — the
        // sidecar gets that an emergency stop happened and who pulled it, which
        // is what it needs to make sense of a company that suddenly stops
        // acting, without putting an operator's incident note on the wire.
        CompanyEvent::EmergencyPauseChanged { engaged, by, .. } => (
            Role::System,
            by.id.clone(),
            format!(
                "Emergency stop {}",
                if *engaged { "engaged" } else { "released" }
            ),
            "emergency.pause.changed",
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
        // Issue #364. Written for completeness, not for a live path: this
        // function normalizes a cycle's *input* events, and a reaction is
        // appended straight to the log by its route — it drives no cycle, so a
        // brain never sees one here. Spelled out rather than swept into a
        // wildcard so the next variant that IS a stimulus cannot inherit a
        // silent default. `by` is dropped, as every other arm drops it.
        CompanyEvent::ReactionToggled {
            message_seq,
            emoji,
            on,
            ..
        } => (
            Role::System,
            "operator".to_string(),
            format!(
                "{} {emoji} on message {message_seq}",
                if *on { "Reacted" } else { "Un-reacted" }
            ),
            "reaction.toggled",
        ),
        // Issue #403. Worth telling the brain, because it changes what the
        // brain can *do*: a cleared credential is why a tool it had yesterday
        // stops answering today, and without this that reads as an unexplained
        // failure. `by` is deliberately dropped — the sidecar needs to know the
        // company's tool access moved, not which human moved it, and the actor
        // is a user id. Same omission the operator SSE projection makes.
        CompanyEvent::ToolAccessChanged {
            change, toolkit, ..
        } => (
            Role::System,
            "operator".to_string(),
            match toolkit {
                Some(toolkit) => format!("Tool access changed: {change} ({toolkit})"),
                None => format!("Tool access changed: {change}"),
            },
            "tool_access.changed",
        ),
        // Issue #983. Written for completeness on the same terms as
        // `ReactionToggled` above, not for a live path: these bracket a chat
        // turn and are appended beside it rather than driving a cycle, so a
        // brain never sees one here. The turn id is the whole payload — the
        // message text rides the `OperatorMessage` this brackets, and the
        // failure reason is tenant-scoped, so neither is copied here.
        CompanyEvent::TurnStarted {
            turn_id, chat_id, ..
        } => (
            Role::System,
            "operator".to_string(),
            format!("[{chat_id}] turn {turn_id} accepted"),
            "turn.started",
        ),
        CompanyEvent::TurnFailed { turn_id, .. } => (
            Role::System,
            "operator".to_string(),
            format!("Turn {turn_id} did not finish"),
            "turn.failed",
        ),
        // Issue #1015. Structural only, like every arm here: a minted run id and
        // two fixed-vocabulary statuses. `error` is deliberately not copied —
        // it is tenant-scoped, the same reason `TurnFailed` above carries only
        // its id.
        CompanyEvent::RunStatusChanged {
            run_id, from, to, ..
        } => (
            Role::System,
            "board".to_string(),
            match from {
                Some(from) => format!("Attempt {run_id}: {from} -> {to}"),
                None => format!("Attempt {run_id}: {to}"),
            },
            "run.status_changed",
        ),
        CompanyEvent::TaskDispatched { task_id, .. } => (
            Role::System,
            "board".to_string(),
            format!("Dispatched task {task_id}"),
            "task.dispatched",
        ),
        // Issue #464. Written for completeness, not for a live path — the same
        // standing as the reaction arm above: this normalizes a cycle's *input*
        // events, and a board announcement is appended by the task store after
        // a write it did not start. It drives no cycle, so a brain never sees
        // one here. Spelled out rather than swept into a wildcard so the next
        // variant that IS a stimulus cannot inherit a silent default.
        //
        // Structural only. The card's title and note stay off this wire for the
        // same reason they stay off the operator SSE projection: this body is
        // sent to the inference sidecar, and a board write is not a place to
        // hand it operator-authored text nobody asked to send.
        // Issue #327: the tree's counterpart to the board announcement below,
        // and on exactly its terms — written for completeness, not for a live
        // path, since a workspace write starts no cycle and a brain therefore
        // never sees one here. Structural only: the node's NAME is deliberately
        // absent, because it is operator-authored free text and this body goes
        // to the inference sidecar.
        CompanyEvent::WorkspaceChanged { node_id, change } => (
            Role::System,
            "workspace".to_string(),
            format!("Workspace node {node_id} {change}"),
            "workspace.changed",
        ),
        CompanyEvent::TaskCardChanged {
            task_id,
            change,
            column,
        } => (
            Role::System,
            "board".to_string(),
            match column {
                Some(column) => format!("Task {task_id} {change} → {column}"),
                None => format!("Task {task_id} {change}"),
            },
            "task.card_changed",
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
        // Issue #276. Id, name and the state it moved to — no `reason`, because
        // "the host disarmed this" is a fact about our write path, not something
        // an agent should be reasoning about or reciting back.
        CompanyEvent::WorkflowEnabledChanged {
            workflow_id,
            name,
            enabled,
            ..
        } => (
            Role::System,
            "workflow".to_string(),
            format!(
                "{} workflow {name} ({workflow_id})",
                if *enabled {
                    "Switched on"
                } else {
                    "Switched off"
                }
            ),
            "workflow.enabled_changed",
        ),
        // Card-only body, for the same reason the steer arm below is action-only:
        // the message text is human free text that no agent consumes in v1
        // (#335), and this body is wired out to the inference sidecar. Naming
        // the card records that a human said something on it; quoting them would
        // make the Discussion tab an unannounced prompt surface.
        CompanyEvent::TaskDiscussionPosted { task_id, .. } => (
            Role::System,
            "operator".to_string(),
            format!("Posted to the discussion on task {task_id}"),
            "task.discussion_posted",
        ),
        // Card-only for the same reason, and one stronger: a withdrawal (#358)
        // is the operator saying that text should stop being readable, so the
        // one thing this arm must never do is quote it. It carries no text to
        // quote either — the event holds a pointer, not a payload.
        CompanyEvent::TaskDiscussionRedacted { task_id, .. } => (
            Role::System,
            "operator".to_string(),
            format!("Removed a discussion message on task {task_id}"),
            "task.discussion_redacted",
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
            cancelled,
            ..
        } => {
            let how = if *scheduled { "Scheduled" } else { "Manual" };
            let body = match error {
                Some(err) => format!("{how} run of workflow {workflow_id} failed: {err}"),
                // Issue #383: a stopped run is neither a failure nor a finish,
                // and it is bound explicitly rather than left to `..` — a
                // cancelled run carries NO error, so without this arm it fell
                // through to the success wording below and told the sidecar a
                // run somebody stopped had "finished". The distinction the whole
                // issue rests on has to survive at every reader, not only at the
                // tenant-facing ones.
                None if *cancelled => {
                    format!("{how} run of workflow {workflow_id} was stopped by an operator")
                }
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
        // Issue #371's per-node progress trail. Structural ids and a duration —
        // that is the entire payload, by construction — so wiring it out could
        // leak nothing even if we wanted it to. It is still excluded, because
        // the sidecar reads company activity for *insight*, and "node 4 of 6
        // took 1.2s" is telemetry: it would spend the wire budget on the
        // finest-grained events the journal produces while saying nothing about
        // what the company did. The run's own `WorkflowRunFinished` arm above
        // already carries that.
        CompanyEvent::WorkflowRunStarted { workflow_id, .. } => (
            Role::System,
            "workflow".to_string(),
            format!("Run of workflow {workflow_id} started"),
            "workflow.run.started",
        ),
        // Issue #382: the per-node start bracket. Structural, like the finish
        // arm below — a node id and nothing else, so the sidecar reads "a node
        // began" without any of the node's own payload.
        CompanyEvent::WorkflowNodeStarted {
            workflow_id,
            node_id,
            ..
        } => (
            Role::System,
            "workflow".to_string(),
            format!("Workflow {workflow_id} started node {node_id}"),
            "workflow.node",
        ),
        CompanyEvent::WorkflowNodeFinished {
            workflow_id,
            node_id,
            ..
        } => (
            Role::System,
            "workflow".to_string(),
            format!("Workflow {workflow_id} finished node {node_id}"),
            "workflow.node",
        ),
        // Issue #529: a report left the process. Structural, like the per-node
        // arm above — node id and destination kind, never the target address:
        // the sidecar reads company activity for insight, and "the owner summary
        // went out" is the insight; *whom* it reached is operator-only, the same
        // boundary `DeliveryReport::detail` draws. The run's own
        // `WorkflowRunFinished` arm already folds the delivery counts.
        CompanyEvent::WorkflowReportDelivered {
            workflow_id,
            node,
            kind,
            ..
        } => (
            Role::System,
            "workflow".to_string(),
            format!("Workflow {workflow_id} delivered its {kind} report from node {node}"),
            "workflow.report_delivered",
        ),
        // Issue #617. Structural only, like every arm here: the child graph,
        // the node and the tool. The policy's `reason` is deliberately NOT
        // carried onto the wire — it is a sentence built for an operator's
        // approval card, and this surface is a short non-sensitive one-liner.
        CompanyEvent::WorkflowChildCallNotOffered {
            child_workflow_id,
            node,
            tool,
            ..
        } => (
            Role::System,
            "workflow".to_string(),
            format!(
                "Workflow child {child_workflow_id} ran {tool} at node {node} without offering \
                 it for approval"
            ),
            "workflow.child_call_not_offered",
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

/// Splits an effect kind into its lowercase segments.
///
/// Both delimiters are live, which is why neither can be dropped (issue #704).
/// `kind` is free `string(1..64)` on the wire — [`effect_from_frame`] copies it
/// verbatim out of the `orch:effect:<kind>` event name — and real traffic uses
/// each: `payment.received` and `x402.spend` are dotted, while `send_dm` is
/// underscore-joined and carries no dot at all.
fn segments(kind: &str) -> Vec<&str> {
    kind.split(['.', '_']).filter(|s| !s.is_empty()).collect()
}

/// Whether `needle`'s own segments appear as a **contiguous run** in `segments`.
///
/// A run rather than a single segment because one needle — `send_dm` — is itself
/// two segments, and it must keep matching the kind of the same name. Matching it
/// as a run also keeps it from matching `payment.send`, which would be the worst
/// possible reclassification here: `Send` is tested before `Spend`, so a bare
/// `send` needle would route money movement into the messaging group and past the
/// gate that reads `amount_usd`.
fn has_segment_run(segments: &[&str], needle: &str) -> bool {
    let want: Vec<&str> = needle.split(['.', '_']).filter(|s| !s.is_empty()).collect();
    if want.is_empty() || want.len() > segments.len() {
        return false;
    }
    segments.windows(want.len()).any(|window| window == want)
}

/// Maps a dotted effect kind to its supervised-policy [`EffectGroup`].
///
/// # Segments, not bare substrings (issue #704)
///
/// Each needle below is matched against whole `.`/`_` segments. It used to be
/// matched with `contains`, which fires *inside* a word: `de-sign.review` and
/// `as-sign-ment.create` both classified as [`Sign`](EffectGroup::Sign), and a
/// misspelled `pay-mnt.send` classified as [`Spend`](EffectGroup::Spend).
///
/// Both directions of that were wrong in a way worth naming. A wrong `Sign`
/// parks a routine effect for a human on *every* call under `supervised` — the
/// standing interruption EPIC #558 exists to remove — while a wrong `Spend`
/// hands an effect to the money gate, which then reads an `amount_usd` that a
/// non-payment effect never carried.
///
/// The needle set is deliberately unchanged, including `pay`. Under segment
/// matching `pay` can no longer fire inside `paymnt`, so the false positive that
/// made it look redundant is gone; dropping it would only lose a literal `pay`
/// segment, and losing a *true* `Spend` is the one error here with real
/// consequence — an ungated payment rather than an annoyed operator.
pub(crate) fn effect_group_for(kind: &str) -> EffectGroup {
    let lowered = kind.to_ascii_lowercase();
    let segments = segments(&lowered);
    let has = |needle: &str| has_segment_run(&segments, needle);

    if has("send_dm") || has("message") || has("email") || has("reply") {
        EffectGroup::Send
    } else if has("payment") || has("spend") || has("x402") || has("pay") {
        EffectGroup::Spend
    } else if has("sign") || has("filing") || has("contract") {
        EffectGroup::Sign
    } else if has("publish") {
        EffectGroup::Publish
    } else if has("hire") || has("engage") {
        EffectGroup::Hire
    } else if has("identity") || has("register") {
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
        message_id: None,
        task_id: None,
        channel,
        agent: None,
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
            cancelled: false,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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

    /// **Issue #383: a stopped run must not wire out as a finished one.**
    ///
    /// A cancelled run carries `cancelled: true` and **no error**, so the arm
    /// that only branched on `error` fell straight through to the success
    /// wording — this consumer was told a run somebody deliberately stopped had
    /// "finished", with a tidy zero-report tally to prove it.
    ///
    /// Asserted as a *negative* on the success word rather than only as a
    /// positive on the new one: the failure mode is the two readings becoming
    /// indistinguishable, and only the negative catches that.
    #[test]
    fn a_cancelled_run_wires_out_as_stopped_rather_than_finished() {
        let event = CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".to_string(),
            scheduled: false,
            run_id: Some("run-1".to_string()),
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: true,
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        };

        let wired = wire_event(7, &event);

        assert!(
            wired.body.contains("stopped by an operator"),
            "{}",
            wired.body
        );
        assert!(
            !wired.body.contains("finished"),
            "a stopped run must not read as a finished one: {}",
            wired.body
        );
        assert_eq!(wired.kind, "workflow.run");
    }

    /// **Issue #335, the same pin one variant over.** A discussion post is
    /// operator free text that no agent consumes in v1 — the tab is a note on a
    /// card, not a prompt box. This function wires the journal out to the
    /// inference sidecar, so quoting a post's text here would make it exactly
    /// the prompt surface the design says it is not, without anybody choosing
    /// that.
    ///
    /// The exclusion is what makes "agents do not participate" a property of the
    /// code rather than a sentence in a doc, so it is asserted rather than
    /// described: a later "include the message, it is more informative" edit
    /// fails CI.
    #[test]
    fn a_discussion_post_wires_out_the_card_without_the_message_text() {
        let event = CompanyEvent::TaskDiscussionPosted {
            task_id: "t-42".to_string(),
            // The shape of message the exclusion exists for: an operator pasting
            // something into a thread nobody said would be read by a model.
            text: "the staging key is sk-live-not-a-real-secret".to_string(),
            by: None,
        };

        let wired = wire_event(11, &event);

        assert!(!wired.body.contains("sk-live"), "{}", wired.body);
        assert!(!wired.body.contains("staging key"), "{}", wired.body);
        // Still says what happened: a human posted, and on which card.
        assert!(wired.body.contains("t-42"), "{}", wired.body);
        assert_eq!(wired.kind, "task.discussion_posted");
    }

    // -----------------------------------------------------------------------
    // Issue #704: the effect classifier matches segments, not bare substrings
    // -----------------------------------------------------------------------

    /// **The table that actually guards #704.**
    ///
    /// The defect is a *false positive*, so the intended-match table in the test
    /// below cannot catch a regression on its own: a bare `contains` satisfies
    /// every row of it and still misclassifies everything here. This is the half
    /// that fails if segment matching is ever undone.
    ///
    /// `design.review` and `assignment.create` are the expensive direction —
    /// under `supervised` a spurious `Sign` parks a routine effect for a human on
    /// every single call, which is the standing interruption EPIC #558 exists to
    /// remove. `paymnt.send` is the dangerous one: a misspelling was handed to the
    /// money gate, which then reads an `amount_usd` the effect never carried.
    #[test]
    fn a_needle_spelled_inside_a_word_does_not_classify_the_kind() {
        for kind in [
            "design.review",     // de-SIGN
            "design.approve",    // de-SIGN
            "assignment.create", // as-SIGN-ment
            "signal.emit",       // SIGN-al
            "paymnt.send",       // PAY-mnt, a typo that must not reach the money gate
        ] {
            assert_eq!(
                effect_group_for(kind),
                EffectGroup::Other,
                "`{kind}` spells a needle inside a word and must not be classified by it"
            );
        }
    }

    /// A word that merely *contains* an earlier arm's needle must not be stolen
    /// by that arm — it must fall through to the group it really belongs to.
    ///
    /// `redesign.publish` is the sharp case: `Sign` is tested before `Publish`,
    /// so under bare-substring matching re-de-**sign** captured it and the effect
    /// was classified `Sign` while never reaching `Publish` at all. Asserting
    /// `Publish` rather than `Other` is the point — it shows the fix restores the
    /// correct group instead of merely dropping the wrong one.
    #[test]
    fn an_infix_match_does_not_steal_a_kind_from_a_later_arm() {
        assert_eq!(effect_group_for("redesign.publish"), EffectGroup::Publish);
    }

    /// `Send` is tested before `Spend`, so a bare `send` needle would route money
    /// movement into the messaging group — past the gate that reads `amount_usd`.
    /// This is why `send_dm` is matched as a two-segment *run* rather than by its
    /// segments individually, and it is asserted because the cheap simplification
    /// (splitting on `_` and then looking for a lone `send`) is silently wrong.
    #[test]
    fn a_payment_that_sends_is_spend_rather_than_send() {
        assert_eq!(effect_group_for("payment.send"), EffectGroup::Spend);
        assert_eq!(effect_group_for("payment.send_dm"), EffectGroup::Send);
    }

    /// Every intended match still classifies, including the underscore-joined
    /// `send_dm` — a live kind that carries no dot at all, which is why the
    /// segment split has to accept `_` as well as `.`.
    #[test]
    fn the_intended_kinds_still_classify() {
        for (kind, group) in [
            ("send_dm", EffectGroup::Send),
            ("operator.message", EffectGroup::Send),
            ("email.deliver", EffectGroup::Send),
            ("payment.received", EffectGroup::Spend),
            ("x402.spend", EffectGroup::Spend),
            ("pay.invoice", EffectGroup::Spend),
            ("document.sign", EffectGroup::Sign),
            ("filing.submit", EffectGroup::Sign),
            ("contract.execute", EffectGroup::Sign),
            ("workspace.publish", EffectGroup::Publish),
            ("hire.contractor", EffectGroup::Hire),
            ("identity.register", EffectGroup::Identity),
            ("echo.noop", EffectGroup::Other),
        ] {
            assert_eq!(effect_group_for(kind), group, "`{kind}`");
        }
    }

    /// **Issue #382: the per-node start bracket wires out structurally.** The
    /// node has not run, so the event carries ids alone — no status, no
    /// duration, never any input. This pins that the sidecar reads "a node
    /// began" as a system record on the `workflow` sender under the
    /// `workflow.node` kind, carrying the structural ids and nothing of the
    /// node's own payload (there is none to leak). Mirrors the finish arm one
    /// variant over.
    #[test]
    fn a_started_node_wires_out_structurally_on_the_workflow_sender() {
        let event = CompanyEvent::WorkflowNodeStarted {
            workflow_id: "digest".to_string(),
            run_id: "run-1".to_string(),
            node_id: "owner_summary".to_string(),
        };

        let wired = wire_event(3, &event);

        assert_eq!(wired.role, Role::System);
        assert_eq!(wired.sender, "workflow");
        assert_eq!(wired.kind, "workflow.node");
        // Says what happened, in ids: which graph, which node, that it started.
        assert!(wired.body.contains("digest"), "{}", wired.body);
        assert!(wired.body.contains("owner_summary"), "{}", wired.body);
        assert!(wired.body.contains("started"), "{}", wired.body);
    }
}
