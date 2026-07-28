//! Transient per-turn progress bus — the live tool-call timeline that rides the
//! company SSE feed *while a turn is still running*.
//!
//! This mirrors OpenHuman's web-chat event bus
//! (`vendor/openhuman/src/openhuman/web_chat/event_bus.rs`): a process-wide
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
//! [`crate::harness::steps`] — a label, an optional whitelisted detail, an
//! elapsed time, a status. It never carries raw tool arguments, raw tool
//! output, or an MCP call's nested remote payload, exactly like `fold_steps`.
//! The mapping that enforces that lives next to `fold_steps` (same helpers, same
//! rules); this module is a dumb transport and models no openhuman types, so it
//! stays compiled in the default (non-`openhuman`) build where it simply has no
//! publishers and every subscription is an empty stream.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use futures::stream::BoxStream;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::ports::types::CompanyId;

/// Per-company broadcast senders. Created lazily on first publish/subscribe for
/// a company and kept for the process lifetime (companies are few and long
/// lived, matching the durable event log's own `senders` map in `store::fs`).
static REGISTRY: LazyLock<Mutex<HashMap<CompanyId, broadcast::Sender<TurnStreamEvent>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Ring capacity per company. A console that lags past this many un-drained
/// live events drops the gap (`Lagged`) and keeps going — the authoritative
/// timeline still arrives folded on the final reply, so a dropped live frame is
/// cosmetic, never lost state.
const CAPACITY: usize = 256;

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
    /// Scrubbed structural detail (whitelisted success enrichment, or the
    /// classifier's plain-language failure cause). Never raw remote text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `"running"` on a start, `"ok"`/`"error"` on a completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    /// Wall-clock the completed call took, on `tool_result` only.
    #[serde(rename = "elapsedMs", skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
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
    /// The chat/desk thread this turn answers, stamped onto each frame's
    /// `chatId` so the console routes the live timeline to the same thread the
    /// durable `agent_reply` lands on. Matches the id journaled as
    /// `AgentReply.chat_id` for this turn.
    pub chat_id: String,
}

/// The sender for a company, created on first use. Mirrors `store::fs`'s
/// `sender_for`: a subscribers-may-be-zero broadcast whose `send` error (no
/// listeners) is ignored — a turn streams whether or not a console is watching.
fn sender_for(company: &CompanyId) -> broadcast::Sender<TurnStreamEvent> {
    let mut reg = REGISTRY.lock().expect("turn-stream registry poisoned");
    reg.entry(company.clone())
        .or_insert_with(|| broadcast::channel(CAPACITY).0)
        .clone()
}

/// Publish one live frame for `company`. A no-op (send error ignored) when no
/// console is subscribed. Called only from the `openhuman` harness collector.
pub fn publish(company: &CompanyId, event: TurnStreamEvent) {
    let _ = sender_for(company).send(event);
}

/// Subscribe to a company's live turn frames as a stream, for merging into the
/// operator SSE feed. On a cold company (no turn yet) this still yields a live
/// receiver — it simply stays quiet until the first `publish`. A `Lagged` gap is
/// skipped (see [`CAPACITY`]); `Closed` never happens because `REGISTRY` holds a
/// sender for the process lifetime.
pub fn subscribe(company: &CompanyId) -> BoxStream<'static, TurnStreamEvent> {
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

    fn frame(kind: &'static str, seq: u64) -> TurnStreamEvent {
        TurnStreamEvent {
            kind,
            seq,
            agent_id: None,
            chat_id: None,
            tool_call_id: Some("c1".to_string()),
            label: Some("Searching".to_string()),
            detail: None,
            status: Some("running"),
            elapsed_ms: None,
        }
    }

    /// A published frame reaches an already-subscribed console, agent stamp and
    /// all. (Broadcast only delivers to receivers that existed at publish time,
    /// so subscribe first — exactly the SSE route's order.)
    #[tokio::test]
    async fn publish_reaches_subscriber() {
        let company = CompanyId::new("turn-stream-roundtrip");
        let mut stream = subscribe(&company);
        publish(
            &company,
            frame("tool_call", 0).with_agent("ceo").with_chat("General"),
        );
        let got = stream.next().await.expect("a frame arrives");
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
        let a = stream.next().await.expect("first frame");
        let b = stream.next().await.expect("second frame");
        assert_eq!(a.agent_id.as_deref(), Some("ceo"));
        assert_eq!(b.agent_id.as_deref(), Some("ceo"));
        // agentId alone is ambiguous; chatId disambiguates the two threads.
        assert_eq!(a.chat_id.as_deref(), Some("General"));
        assert_eq!(b.chat_id.as_deref(), Some("eng_desk"));
        assert_ne!(a.chat_id, b.chat_id);
    }

    /// A publish with no subscriber is a silent no-op — a turn streams whether or
    /// not a console is watching.
    #[test]
    fn publish_without_subscriber_is_noop() {
        let company = CompanyId::new("turn-stream-nobody");
        publish(&company, frame("tool_call", 0)); // must not panic
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
            status: Some("ok"),
            elapsed_ms: Some(12),
        };
        let j = serde_json::to_value(f.with_chat("General")).expect("serialize");
        assert_eq!(j["type"], "tool_result");
        assert_eq!(j["toolCallId"], "c1");
        assert_eq!(j["elapsedMs"], 12);
        assert_eq!(j["status"], "ok");
        assert_eq!(j["chatId"], "General");
        assert!(j.get("agentId").is_none(), "empty optional omitted");
    }
}
