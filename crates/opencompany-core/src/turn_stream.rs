//! Transient per-turn progress bus — the live tool-call timeline that rides the
//! company SSE feed *while a turn is still running*.
//!
//! This mirrors OpenHuman's web-chat event bus
//! (`vendor/openhuman/crates/openhuman-core/src/web_chat/event_bus.rs`): a process-wide
//! `tokio::sync::broadcast` fabric, one sender per company, that the harness
//! publishes each mapped [`AgentProgress`](openhuman) event onto the instant it
//! happens, and that the operator SSE route ([`company_events`]) fans back out
//! to every watching console.
//!
//! It is deliberately **separate from the durable company event log**
//! ([`CompanyEvent`](crate::ports::types::CompanyEvent) / [`EventLog`]):
//!
//! * Tool-call progress is high-volume and ephemeral — one turn can emit dozens
//!   of start/complete pairs. Journaling it would bloat the audit log and the
//!   GraphQL `Chat.history` for no lasting value.
//! * The durable, operator-facing timeline is still folded into
//!   [`TurnStep`](crate::ports::types::TurnStep)s at turn end
//!   ([`fold_steps`](crate::harness::steps::fold_steps)) and rides the final
//!   chat reply. This bus only makes those same steps *appear live* first.
//!
//! ## Security
//!
//! The bus carries only the **already-scrubbed** projection produced by
//! [`crate::harness::steps`] — a label, a redacted argument line, a result
//! *summary*, a typed failure, an elapsed time, a status. It never carries raw
//! tool output, exactly like `fold_steps`; arguments reach it only through
//! issue #372's host-side redactor, and a remote body reaches it only as a
//! shape (issue #411). The mapping that enforces all of that lives next to
//! `fold_steps` (same helpers, same rules); this module is a dumb transport and
//! models no openhuman types, so it stays compiled in the default
//! (non-`openhuman`) build where it simply has no publishers and every
//! subscription is an empty stream.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use futures::stream::BoxStream;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::ports::types::{CompanyId, TurnStepFailure};

/// Per-company broadcast senders. Created lazily on first publish/subscribe for
/// a company and kept for the process lifetime (companies are few and long
/// lived, matching the durable event log's own `senders` map in `store::fs`).
static REGISTRY: LazyLock<Mutex<HashMap<CompanyId, broadcast::Sender<LiveFrame>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Ring capacity per company. A console that lags past this many un-drained
/// live events drops the gap (`Lagged`) and keeps going — the authoritative
/// timeline still arrives folded on the final reply, so a dropped live frame is
/// cosmetic, never lost state.
const CAPACITY: usize = 256;

/// Anything this bus carries.
///
/// `untagged` because every variant already serializes its own `type` key, so
/// the wire form of a turn frame is **byte-identical** to what it was before
/// presence existed — no envelope, no nesting, no second discriminant. The
/// console keeps switching on `type` exactly as it does for `agent_reply`.
///
/// Presence and typing belong here rather than on the durable event log for
/// the reason stated in this module's header: they are high-volume, worthless
/// a second later, and journaling them would bloat the audit log and every
/// chat history read for no lasting value. They are also, unlike a turn frame,
/// facts about *people* — which is why the projection that puts them on the
/// wire lives beside the route that authenticates one.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum LiveFrame {
    /// Tool-call progress while a turn runs.
    Turn(Box<TurnStreamEvent>),
    /// Somebody arrived, went idle, or left.
    Presence(PresenceFrame),
    /// Somebody is typing in a channel.
    Typing(TypingFrame),
    /// A coarse "near your credit limit" warning (issue #1846).
    BudgetProximity(BudgetProximityFrame),
}

impl LiveFrame {
    /// The turn frame this carries, or `None` for a presence/typing frame.
    ///
    /// The bus is mostly turn frames and every pre-existing reader wants one,
    /// so this keeps those call sites reading as they did rather than matching
    /// on a union they do not care about.
    pub fn as_turn(&self) -> Option<&TurnStreamEvent> {
        match self {
            Self::Turn(event) => Some(event),
            _ => None,
        }
    }
}

impl From<TurnStreamEvent> for LiveFrame {
    fn from(event: TurnStreamEvent) -> Self {
        // Boxed because `TurnStreamEvent` is much the largest variant, and an
        // un-boxed union would make every presence frame on the broadcast ring
        // pay for it. The ring holds `CAPACITY` of these per company.
        Self::Turn(Box::new(event))
    }
}

impl From<PresenceFrame> for LiveFrame {
    fn from(frame: PresenceFrame) -> Self {
        Self::Presence(frame)
    }
}

impl From<TypingFrame> for LiveFrame {
    fn from(frame: TypingFrame) -> Self {
        Self::Typing(frame)
    }
}

impl From<BudgetProximityFrame> for LiveFrame {
    fn from(frame: BudgetProximityFrame) -> Self {
        Self::BudgetProximity(frame)
    }
}

/// A coarse, non-blocking "near your credit limit" warning (issue #1846).
///
/// Rides the same ephemeral, journal-less bus as [`TurnStreamEvent`] and for
/// the same reason: this is a soft heads-up, not an authoritative fact worth
/// persisting — a console that missed one because it was offline sees the
/// next one on the next dispatch, and there is nothing to "catch up" on. It
/// is published **beside the existing pre-flight cap reads** in
/// [`crate::harness::HarnessPool`]'s dispatch gate, reusing the meter read
/// that already happens there — never a second query, and never a per-task
/// cost estimate (that needs a net-new `TaskPlan` cost primitive and is
/// explicitly out of scope here).
///
/// Fail-open by construction: this is emitted only on the branch where the
/// meter read already SUCCEEDED and a coarse threshold was crossed. An
/// unreadable meter publishes nothing, exactly like the pre-flight refusals
/// beside it fall through to running the turn rather than bricking dispatch.
#[derive(Clone, Debug, Serialize)]
pub struct BudgetProximityFrame {
    /// Always `"budget_proximity"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The teammate whose dispatch triggered the read, when the warning is
    /// scoped to one agent's own cap. `None` for the company-wide ceiling.
    #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// An operator-facing sentence — no raw figures the config threshold
    /// might not want surfaced verbatim, just "you're near your limit".
    pub message: String,
    #[serde(rename = "atMillis")]
    pub at_millis: u64,
}

/// One person's presence changed.
///
/// Published on a change only — an arrival, a status flip, a departure — never
/// on a routine heartbeat renewal, or a company of ten would put ten frames a
/// minute on every open console for no visible difference.
///
/// Carries a user id and no label: every signed-in member already holds the
/// directory that names them (`GET {scope}/chat/mentionables`), so a label here
/// would be a second copy to keep in step.
#[derive(Clone, Debug, Serialize)]
pub struct PresenceFrame {
    /// Always `"presence"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(rename = "userId")]
    pub user_id: String,
    /// `online` / `away` / `offline`.
    pub status: &'static str,
    #[serde(rename = "atMillis")]
    pub at_millis: u64,
}

/// Somebody is typing.
///
/// Stored nowhere at all, not even in the presence registry: a typing
/// indicator is eight seconds of leased truth, and the console expires it on
/// its own. There is deliberately no "stopped typing" frame — the absence of a
/// renewal is the stop signal, which means a console that closes mid-word
/// clears itself with no teardown to get wrong.
#[derive(Clone, Debug, Serialize)]
pub struct TypingFrame {
    /// Always `"typing"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(rename = "userId")]
    pub user_id: String,
    /// The channel being typed in.
    #[serde(rename = "chatId")]
    pub chat_id: String,
    /// The thread inside it, when it is one.
    #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(rename = "atMillis")]
    pub at_millis: u64,
}

/// One live progress frame for the console's in-flight tool timeline.
///
/// Serializes onto the same SSE stream as the durable
/// [`project_event`](crate::server::operator) projections, so it carries a
/// `type` discriminant the frontend switches on alongside `agent_reply`,
/// `task_steered`, etc. Every string field here is the scrubbed projection from
/// [`crate::harness::steps`] — never raw arguments/output.
#[derive(Clone, Debug, Serialize)]
pub struct TurnStreamEvent {
    /// The wire discriminant: `"tool_call"` (a call just started, `status`
    /// `running`) or `"tool_result"` (it finished, `status` `ok`/`error`).
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Monotonic per-turn sequence so the client can order/dedup frames that a
    /// reconnecting console might otherwise interleave.
    pub seq: u64,
    /// The responding agent/desk this turn belongs to, so the console can label
    /// which member is working.
    #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The chat/desk thread this turn belongs to — the same id the durable
    /// `agent_reply` frame carries (`chat_id`). The console keys the live tool
    /// timeline on this so concurrent turns on different threads never
    /// cross-attribute their frames. `agentId` alone is insufficient: a thread
    /// is a desk, which doesn't map 1:1 to the responding member.
    #[serde(rename = "chatId", skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// Stable id pairing a `tool_result` back to its `tool_call` row so the
    /// console flips that row `running → ok/error` in place (mirrors
    /// OpenHuman's `tool_call_id` keying).
    #[serde(rename = "toolCallId", skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The step label (server-computed `display_label`, else humanized tool
    /// name). Never derived from arguments or output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// **What the call was doing** — its arguments, put through issue #372's
    /// host-side redactor and bounded (issue #411). Never raw remote text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// **What came back** — a success's shape summary or an intrinsic tool's
    /// own message, or a failure's plain-language cause (issue #411). Never a
    /// remote body's content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// The typed reason a call did not succeed (issue #411), so a live row
    /// renders the same known state the final folded step does. `None` on a
    /// success and on a parked call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<TurnStepFailure>,
    /// The result was cut before the agent could read all of it (issue #410).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// `"running"` on a start; `"ok"` / `"error"` / `"awaiting_approval"` on a
    /// completion. Sourced from
    /// [`TurnStepStatus::wire_word`](crate::ports::types::TurnStepStatus::wire_word)
    /// so the live word and the persisted one cannot drift.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    /// Wall-clock the completed call took, on `tool_result` only.
    #[serde(rename = "elapsedMs", skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// The journal sequence of the operator message this turn is answering,
    /// when it is answering one at all.
    ///
    /// `chatId` says which *conversation* a frame belongs to; this says which
    /// *query*. The distinction only starts to matter when one conversation has
    /// two turns in flight, which is exactly when the console needs it. Keyed by
    /// thread alone, a second send shares one live timeline with the first, so
    /// two questions' rows pile into one list with nothing to tell them apart —
    /// and because the console holds a single row-list per thread, arming the
    /// second turn discards the first's rows outright. A turn blocked on a
    /// teammate emits nothing further, so its timeline never comes back.
    ///
    /// With this, a console can group live rows per query and render each under
    /// the message that asked it, the same way a settled turn's folded steps
    /// already render under their reply.
    ///
    /// Carried from `ChatTarget::message_seq`, so it is `None` on every turn
    /// that answers no journaled operator message — a relay, a delegate's
    /// instruction, a dispatched card, a workflow node. A consumer falls back to
    /// keying by `chatId` alone, which is what every consumer did before this
    /// field existed.
    #[serde(rename = "messageSeq", skip_serializing_if = "Option::is_none")]
    pub message_seq: Option<u64>,
    /// The workflow **run** this frame belongs to, when the turn is a workflow
    /// agent node rather than a chat turn (issue #1702). The console's run-trace
    /// sheet keys the live tool timeline on this so a node's in-flight frames
    /// append to the right run. Absent on a chat turn, which routes by `chatId`
    /// instead — the two are mutually exclusive routing dimensions, never both.
    #[serde(rename = "workflowRunId", skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    /// The workflow **node** inside that run this frame belongs to (issue
    /// #1702), so the sheet groups a run's live frames under the same node the
    /// durable trace attributes them to. Absent on a chat turn.
    #[serde(rename = "nodeId", skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

impl Default for TurnStreamEvent {
    /// An empty `tool_call` frame, so the mapping in
    /// [`crate::harness::steps`] can fill only the fields a given event kind
    /// actually carries via `..Default::default()`. `kind` is a placeholder
    /// every caller overrides.
    fn default() -> Self {
        Self {
            kind: "tool_call",
            seq: 0,
            agent_id: None,
            chat_id: None,
            tool_call_id: None,
            label: None,
            detail: None,
            result: None,
            failure: None,
            truncated: false,
            status: None,
            elapsed_ms: None,
            message_seq: None,
            workflow_run_id: None,
            node_id: None,
        }
    }
}

impl TurnStreamEvent {
    /// Stamp the responding agent/desk onto a frame just before publish.
    pub fn with_agent(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Stamp the chat/desk thread onto a frame just before publish, so the
    /// console routes it to the same thread the durable reply lands on.
    pub fn with_chat(mut self, chat_id: impl Into<String>) -> Self {
        self.chat_id = Some(chat_id.into());
        self
    }

    /// Stamp the workflow run + node onto a frame just before publish, so the
    /// console's run-trace sheet routes it under the right run and node (issue
    /// #1702). Used instead of [`with_chat`](Self::with_chat) on a workflow
    /// agent node, which carries no chat thread — the two routing dimensions
    /// are mutually exclusive.
    pub fn with_workflow(mut self, run_id: impl Into<String>, node_id: impl Into<String>) -> Self {
        self.workflow_run_id = Some(run_id.into());
        self.node_id = Some(node_id.into());
        self
    }

    /// Stamps the operator message this turn answers, so the console can group
    /// the frame under that query rather than under the thread as a whole.
    ///
    /// Takes the `Option` rather than the value: every publish site reads it off
    /// a `ChatTarget` that may legitimately carry none, and answering no
    /// journaled message is the normal case for half of them.
    pub fn with_message_seq(mut self, seq: Option<u64>) -> Self {
        self.message_seq = seq;
        self
    }
}

/// Routing context a turn carries so its live frames reach the right console
/// thread: which company to publish on, which responding agent/desk the frames
/// belong to, and which chat thread they render onto. Built by the harness brain
/// (which knows all three) and handed to the turn runner; `None` disables live
/// streaming (e.g. background/system turns and the default non-`openhuman`
/// build).
#[derive(Clone, Debug)]
pub struct TurnStreamCtx {
    /// The company whose SSE feed the frames publish onto.
    pub company: CompanyId,
    /// The responding agent/desk, stamped onto each frame's `agentId`.
    pub agent_id: String,
    /// Where this turn's live frames route on the console: a chat thread, or a
    /// workflow run+node. The two are mutually exclusive, so an enum keeps a
    /// frame from ever carrying both (or neither) routing key.
    pub route: LiveRoute,
    /// The operator message this turn answers, stamped onto every frame as
    /// `messageSeq` so two turns sharing a thread stay tellable apart. `None`
    /// for a turn answering no journaled message; see
    /// [`TurnStreamEvent::message_seq`].
    pub message_seq: Option<u64>,
}

/// Which console surface a turn's live frames route to.
///
/// A chat turn keys its in-flight tool timeline on `chatId`; a workflow agent
/// node has no chat thread and keys on the workflow run + node instead (issue
/// #1702). Modelled as an enum rather than two `Option`s so a frame cannot be
/// built with both keys set or neither — the same "make the illegal state
/// unrepresentable" posture the run-trace sink takes.
#[derive(Clone, Debug)]
pub enum LiveRoute {
    /// The chat/desk thread this turn answers, stamped onto each frame's
    /// `chatId` so the console routes the live timeline to the same thread the
    /// durable `agent_reply` lands on. Matches the id journaled as
    /// `AgentReply.chat_id` for this turn.
    Chat { chat_id: String },
    /// The workflow run + node this turn belongs to, stamped onto each frame's
    /// `workflowRunId`/`nodeId` so the console's run-trace sheet appends the
    /// node's in-flight frames to the right run while it is still executing.
    Workflow { run_id: String, node_id: String },
}

/// The sender for a company, created on first use. Mirrors `store::fs`'s
/// `sender_for`: a subscribers-may-be-zero broadcast whose `send` error (no
/// listeners) is ignored — a turn streams whether or not a console is watching.
fn sender_for(company: &CompanyId) -> broadcast::Sender<LiveFrame> {
    let mut reg = REGISTRY.lock().expect("turn-stream registry poisoned");
    reg.entry(company.clone())
        .or_insert_with(|| broadcast::channel(CAPACITY).0)
        .clone()
}

/// Publish one live frame for `company`. A no-op (send error ignored) when no
/// console is subscribed.
///
/// Takes `impl Into<LiveFrame>` so every existing harness call site — which
/// passes a bare [`TurnStreamEvent`] — is unchanged by the bus being widened.
pub fn publish(company: &CompanyId, event: impl Into<LiveFrame>) {
    let _ = sender_for(company).send(event.into());
}

/// Subscribe to a company's live turn frames as a stream, for merging into the
/// operator SSE feed. On a cold company (no turn yet) this still yields a live
/// receiver — it simply stays quiet until the first `publish`. A `Lagged` gap is
/// skipped (see [`CAPACITY`]); `Closed` never happens because `REGISTRY` holds a
/// sender for the process lifetime.
pub fn subscribe(company: &CompanyId) -> BoxStream<'static, LiveFrame> {
    let rx = sender_for(company).subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => return Some((event, rx)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    /// Unwraps the turn frame a test just published.
    ///
    /// Every assertion here predates the bus carrying anything but turns, and
    /// panicking on the wrong variant is the right failure: a test that
    /// published a turn frame and received a presence one has found a real bug.
    fn turn(frame: LiveFrame) -> TurnStreamEvent {
        frame
            .as_turn()
            .cloned()
            .expect("this bus published a turn frame")
    }

    fn frame(kind: &'static str, seq: u64) -> TurnStreamEvent {
        TurnStreamEvent {
            kind,
            seq,
            agent_id: None,
            chat_id: None,
            tool_call_id: Some("c1".to_string()),
            label: Some("Searching".to_string()),
            status: Some("running"),
            ..TurnStreamEvent::default()
        }
    }

    /// A published frame reaches an already-subscribed console, agent stamp and
    /// all. (Broadcast only delivers to receivers that existed at publish time,
    /// so subscribe first — exactly the SSE route's order.)
    /// The whole reason [`LiveFrame`] is `untagged`: widening the bus must not
    /// change one byte of what a turn frame looks like on the wire, or every
    /// console in the field starts ignoring frames it used to render.
    #[test]
    fn wrapping_a_turn_frame_in_the_union_does_not_change_its_wire_form() {
        let event = frame("tool_call", 7);
        let bare = serde_json::to_string(&event).expect("serialize");
        let wrapped = serde_json::to_string(&LiveFrame::from(event)).expect("serialize");
        assert_eq!(bare, wrapped);
    }

    #[test]
    fn presence_and_typing_carry_their_own_type_discriminant() {
        let presence = serde_json::to_value(LiveFrame::Presence(PresenceFrame {
            kind: "presence",
            user_id: "u1".to_string(),
            status: "online",
            at_millis: 5,
        }))
        .expect("serialize");
        assert_eq!(presence["type"], "presence");
        assert_eq!(presence["userId"], "u1");
        // No label: the console already holds the directory that names people.
        assert!(presence.get("label").is_none());

        let typing = serde_json::to_value(LiveFrame::Typing(TypingFrame {
            kind: "typing",
            user_id: "u1".to_string(),
            chat_id: "eng".to_string(),
            parent_id: None,
            at_millis: 5,
        }))
        .expect("serialize");
        assert_eq!(typing["type"], "typing");
        assert_eq!(typing["chatId"], "eng");
        assert!(
            typing.get("parentId").is_none(),
            "a channel-level typing frame omits the thread key"
        );
    }

    #[tokio::test]
    async fn publish_reaches_subscriber() {
        let company = CompanyId::new("turn-stream-roundtrip");
        let mut stream = subscribe(&company);
        publish(
            &company,
            frame("tool_call", 0).with_agent("ceo").with_chat("General"),
        );
        let got = turn(stream.next().await.expect("a frame arrives"));
        assert_eq!(got.kind, "tool_call");
        assert_eq!(got.seq, 0);
        assert_eq!(got.agent_id.as_deref(), Some("ceo"));
        // The chat thread rides along so the console routes the frame to the same
        // thread the durable reply lands on.
        assert_eq!(got.chat_id.as_deref(), Some("General"));
        assert_eq!(got.tool_call_id.as_deref(), Some("c1"));
    }

    /// Two turns run by the SAME agent on DIFFERENT chat threads keep their live
    /// frames separable by `chatId` — the console keys the in-flight tool
    /// timeline on that, so concurrent sends never cross-attribute (PR #125
    /// review: routing must be per-thread, not a single global ref keyed on the
    /// responding member, which is identical across both turns here).
    #[tokio::test]
    async fn concurrent_threads_same_agent_route_by_chat() {
        let company = CompanyId::new("turn-stream-concurrent");
        let mut stream = subscribe(&company);
        // Same responding agent ("ceo"), two distinct desk threads in flight.
        publish(
            &company,
            frame("tool_call", 0).with_agent("ceo").with_chat("General"),
        );
        publish(
            &company,
            frame("tool_call", 1)
                .with_agent("ceo")
                .with_chat("eng_desk"),
        );
        let a = turn(stream.next().await.expect("first frame"));
        let b = turn(stream.next().await.expect("second frame"));
        assert_eq!(a.agent_id.as_deref(), Some("ceo"));
        assert_eq!(b.agent_id.as_deref(), Some("ceo"));
        // agentId alone is ambiguous; chatId disambiguates the two threads.
        assert_eq!(a.chat_id.as_deref(), Some("General"));
        assert_eq!(b.chat_id.as_deref(), Some("eng_desk"));
        assert_ne!(a.chat_id, b.chat_id);
    }

    /// Issue #1702: a workflow agent node's frame carries the workflow run +
    /// node instead of a chat thread, and serializes them as `workflowRunId` /
    /// `nodeId`. This is what lets the console's run-trace sheet key the live
    /// tool timeline on the run.
    #[test]
    fn with_workflow_stamps_run_and_node_on_the_wire() {
        let f = frame("tool_call", 3)
            .with_agent("researcher")
            .with_workflow("wfr-42", "summarise");
        assert_eq!(f.workflow_run_id.as_deref(), Some("wfr-42"));
        assert_eq!(f.node_id.as_deref(), Some("summarise"));
        // A workflow node has no chat thread, so `with_workflow` must not invent
        // one — the two routing dimensions are mutually exclusive.
        assert!(f.chat_id.is_none());

        let j = serde_json::to_value(&f).expect("serialize");
        assert_eq!(j["workflowRunId"], "wfr-42");
        assert_eq!(j["nodeId"], "summarise");
        assert!(
            j.get("chatId").is_none(),
            "a workflow-tagged frame carries no chatId"
        );
    }

    /// The tagging is additive: a chat turn's frame still stamps `chatId` and
    /// omits the workflow ids entirely, so an existing chat console reads the
    /// wire form byte-for-byte as it did before #1702.
    #[test]
    fn a_chat_frame_omits_the_workflow_ids() {
        let j =
            serde_json::to_value(frame("tool_call", 0).with_chat("General")).expect("serialize");
        assert_eq!(j["chatId"], "General");
        assert!(
            j.get("workflowRunId").is_none(),
            "a chat frame must not carry a workflowRunId"
        );
        assert!(
            j.get("nodeId").is_none(),
            "a chat frame must not carry a nodeId"
        );
    }

    /// A publish with no subscriber is a silent no-op — a turn streams whether or
    /// not a console is watching.
    #[test]
    fn publish_without_subscriber_is_noop() {
        let company = CompanyId::new("turn-stream-nobody");
        publish(&company, frame("tool_call", 0)); // must not panic
    }

    /// A subscriber that lags past [`CAPACITY`] must skip the gap and keep
    /// reading — never block, and never end the stream. `subscribe`'s `loop`
    /// treats `RecvError::Lagged` as a reason to keep polling; only `Closed`
    /// ends the stream, and `REGISTRY` holds a sender for the process
    /// lifetime, so that never fires here.
    #[tokio::test]
    async fn a_lagging_subscriber_skips_dropped_frames_instead_of_ending() {
        let company = CompanyId::new("turn-stream-overflow");
        let mut stream = subscribe(&company);

        // Publish well past the ring's capacity with nobody draining it, so
        // the receiver's cursor falls behind the oldest frame still buffered.
        let overflow = CAPACITY as u64 + 50;
        for seq in 0..overflow {
            publish(&company, frame("tool_call", seq));
        }

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("a lagging subscriber must not block forever")
            .expect("the stream must not end on a lag");
        let got = turn(got);
        assert!(
            got.seq >= overflow - CAPACITY as u64,
            "a lagging read must resume at the oldest frame still in the ring, not replay one \
             that was already dropped: got seq {}",
            got.seq
        );
    }

    /// The wire shape carries a `type` discriminant (so the console switches on
    /// it alongside the durable projections), camelCases its keys, and omits
    /// empty optionals.
    #[test]
    fn serializes_to_typed_camelcase_wire_shape() {
        let f = TurnStreamEvent {
            kind: "tool_result",
            seq: 2,
            agent_id: None,
            chat_id: None,
            tool_call_id: Some("c1".to_string()),
            label: Some("Search".to_string()),
            detail: Some("brave · search".to_string()),
            result: Some("12 items".to_string()),
            failure: None,
            truncated: false,
            status: Some("ok"),
            elapsed_ms: Some(12),
            message_seq: None,
            workflow_run_id: None,
            node_id: None,
        };
        let j = serde_json::to_value(f.with_chat("General")).expect("serialize");
        assert_eq!(j["type"], "tool_result");
        assert_eq!(j["toolCallId"], "c1");
        assert_eq!(j["elapsedMs"], 12);
        assert_eq!(j["status"], "ok");
        assert_eq!(j["chatId"], "General");
        assert!(j.get("agentId").is_none(), "empty optional omitted");
    }

    /// Two turns racing in one chat are tellable apart, and by nothing else.
    ///
    /// `chatId` is the *conversation*; two questions asked in one channel share
    /// it. So does `agentId` whenever the same desk answers both — which is the
    /// common case, not a corner. Before `messageSeq` those were the only
    /// routing keys a chat frame carried, so a console had no way to say which
    /// question a row belonged to and merged both into one timeline.
    ///
    /// The observed cost was worse than a merged list: the console holds a
    /// single row-list per thread, so arming the second turn cleared the first
    /// turn's rows outright — and a turn blocked on a teammate emits nothing
    /// further, so its steps never came back.
    #[test]
    fn two_queries_in_one_chat_are_told_apart_by_message_seq() {
        let first = serde_json::to_value(
            frame("tool_call", 0)
                .with_chat("general")
                .with_agent("product_manager")
                .with_message_seq(Some(45)),
        )
        .expect("serialize");
        let second = serde_json::to_value(
            frame("tool_call", 0)
                .with_chat("general")
                .with_agent("product_manager")
                .with_message_seq(Some(49)),
        )
        .expect("serialize");

        // Every key they previously routed on agrees — including `seq`, which is
        // per-turn and so restarts at 0 for each of them.
        assert_eq!(first["chatId"], second["chatId"]);
        assert_eq!(first["agentId"], second["agentId"]);
        assert_eq!(first["seq"], second["seq"]);
        // The query does not.
        assert_eq!(first["messageSeq"], 45);
        assert_eq!(second["messageSeq"], 49);
        assert_ne!(first["messageSeq"], second["messageSeq"]);
    }

    /// Omitted, not null, on a turn answering no journaled message — a relay, a
    /// dispatched card, a workflow node. A console that keys on its presence
    /// falls back to the thread, which is what every console did before the
    /// field existed, so an older one reads the wire form unchanged.
    #[test]
    fn a_turn_answering_no_message_carries_no_message_seq() {
        let j =
            serde_json::to_value(frame("tool_call", 0).with_chat("general")).expect("serialize");
        assert!(
            j.get("messageSeq").is_none(),
            "absent rather than null, so presence is the check: {j}"
        );
    }
}
