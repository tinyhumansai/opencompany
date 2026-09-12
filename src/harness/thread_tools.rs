//! Reading another thread in this channel, on demand (issue #1890 F).
//!
//! # Why a tool at all
//!
//! Sub-issue A scoped a turn's context to its own thread, which is what stops
//! two conversations in one channel from bleeding into each other. Sub-issue E
//! then tells a turn what *else* its channel is about — and explicitly tells it
//! not to read any of it.
//!
//! That leaves one gap, and it is the one the operator opens themselves: "make
//! it match the tone of the launch email thread". The agent can see that thread
//! exists and cannot look at it.
//!
//! This is the looking. The epic's framing is **escalate on demand rather than
//! preload**: cost scales with how often threads are actually cross-referenced
//! instead of with how busy the channel is, and *isolation breaks only where
//! the operator asked it to*. A did not isolate threads because cross-reference
//! is bad — it isolated them because unconditional cross-reference is.
//!
//! # Scoped to the current channel
//!
//! Through the same `owns` predicate the seed uses, against the channel the
//! turn is ambiently in (`delegation::turn_conversation`). A tool able to read
//! any thread anywhere would reintroduce A's leak through the back door, so a
//! root in another channel is refused rather than silently returning nothing —
//! a refusal that says why is the difference between "not yours to read" and
//! "there is nothing there".
//!
//! # It declares its truncation
//!
//! `query_company` is the cautionary case the epic names: a full log handed the
//! orchestrator only its last ten rows and read as complete, so "we have no
//! record of that" became a conclusion it could reach from a partial list. A
//! thread cut short says how much it left.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};
use openhuman_core::openhuman as oh;

use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq};

/// The tool name, shared with the belt that registers it.
pub const READ_THREAD_TOOL: &str = "read_thread";

/// How many turns of one thread are returned before the tail is declared.
///
/// A thread is one topic and one level deep, so this is generous for the shape
/// rather than a guess — and what does not fit is counted, never dropped in
/// silence.
const THREAD_TURN_LIMIT: usize = 40;

/// How much of the journal's tail is searched for the requested root.
///
/// Bounded for the reason every read in this epic is: cheap since #1890 G reads
/// a page from the end of the journal rather than streaming it from the head. A
/// root older than this page is reported as out of reach rather than hunted for
/// — which is honest, and is the gap `find_thread` exists to close.
const THREAD_SEARCH_PAGE: usize = 1024;

/// Reads one thread of the channel the current turn is in. Read-only.
pub struct ReadThreadTool {
    company: CompanyId,
    events: Arc<dyn EventLog>,
    /// Resolves the addressed chat id to the desk's `(id, name)` pair.
    ///
    /// Both are needed: a named desk's id and its display name are different
    /// strings and a message is journaled under whichever the caller used, so
    /// `owns` takes two terms. With one, a root stored under the other alias
    /// reads as belonging to a different channel and is refused — a same-desk
    /// thread the operator can see, that the tool insists is not theirs
    /// (codex + coderabbit on #1972).
    store: Arc<dyn crate::ports::store::CompanyStore>,
}

impl ReadThreadTool {
    pub fn new(
        company: CompanyId,
        events: Arc<dyn EventLog>,
        store: Arc<dyn crate::ports::store::CompanyStore>,
    ) -> Self {
        Self {
            company,
            events,
            store,
        }
    }
}

#[async_trait]
impl Tool for ReadThreadTool {
    fn name(&self) -> &str {
        READ_THREAD_TOOL
    }

    fn description(&self) -> &str {
        "Read one other conversation thread in THIS channel, by the id shown in square brackets \
         in the channel's thread list. USE ONLY when the message you are answering explicitly \
         refers to another thread — do not read one speculatively, and if a reference could mean \
         more than one thread, ask which rather than reading several."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "root": {
                    "type": "integer",
                    "description": "The thread's id, exactly as shown in square brackets in the \
                                    channel's thread list (e.g. 41 for `[41]`)."
                }
            },
            "required": ["root"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(root) = args.get("root").and_then(Value::as_u64).map(EventSeq::new) else {
            return Ok(ToolResult::error(
                "`read_thread` needs `root`: the thread's id, as shown in square brackets in the \
                 channel's thread list."
                    .to_string(),
            ));
        };

        // The channel this turn is answering in. `None` is a refusal and not a
        // wildcard: a turn with no conversation — a dispatched card, a workflow
        // node — has no threads it is entitled to read.
        let Some(channel) = crate::runtime::delegation::turn_conversation() else {
            return Ok(ToolResult::error(
                "`read_thread` is only available while answering in a channel; this turn is not \
                 in one."
                    .to_string(),
            ));
        };

        // The desk's own id and name, resolved the way the seed resolves them.
        let (desk_id, desk_name) = crate::server::chat_history::resolve_seed_desk(
            &self.store,
            &self.company,
            Some(channel.as_str()),
        )
        .await;

        let page = match self
            .events
            .read_before(&self.company, None, THREAD_SEARCH_PAGE)
            .await
        {
            Ok(page) => page,
            Err(error) => {
                return Ok(ToolResult::error(format!(
                    "Could not read the channel's history: {error}."
                )));
            }
        };

        // Oldest-first, so the thread reads in the order it happened.
        let mut turns: Vec<String> = Vec::new();
        let mut found_root = false;
        let mut owned_elsewhere = false;
        for stored in page.iter().rev() {
            let in_channel = crate::server::chat_history::owns(&desk_id, &desk_name, &stored.event);
            let (parent, line) = match &stored.event {
                CompanyEvent::OperatorMessage { parent, text, .. } => {
                    (*parent, format!("operator: {text}"))
                }
                CompanyEvent::AgentReply {
                    parent,
                    agent_id,
                    text,
                    ..
                } => (*parent, format!("{agent_id}: {text}")),
                _ => continue,
            };
            let belongs = stored.seq == root || parent == Some(root);
            if !belongs {
                continue;
            }
            if !in_channel {
                // The root exists, but in a different channel. Named as a
                // refusal rather than answered with silence — "not yours to
                // read" and "there is nothing there" are different facts, and
                // the second invites a retry that will also fail.
                owned_elsewhere = true;
                continue;
            }
            if stored.seq == root {
                found_root = true;
            }
            turns.push(line);
        }

        if owned_elsewhere && !found_root {
            return Ok(ToolResult::error(format!(
                "Thread {root} belongs to a different channel. `read_thread` only reads threads \
                 in the channel you are answering in."
            )));
        }
        if !found_root {
            return Ok(ToolResult::error(format!(
                "No thread {root} in this channel's recent history. It may be older than the \
                 window this tool searches."
            )));
        }

        // Keep the NEWEST turns, not the oldest. `turns` is oldest-first, so a
        // plain `truncate` kept the opening of the thread and dropped its
        // conclusion — while the notice below said the opposite, so a request
        // about what was decided would be answered from the part before anyone
        // decided anything (codex + coderabbit on #1972).
        let omitted = turns.len().saturating_sub(THREAD_TURN_LIMIT);
        if omitted > 0 {
            turns.drain(..omitted);
        }
        let mut body = turns.join("\n");
        if omitted > 0 {
            // Declared, never silent — `query_company`'s lesson.
            body.push_str(&format!(
                "\n… and {omitted} earlier turn(s) in this thread, not shown."
            ));
        }
        Ok(ToolResult::success(body))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::StoredEvent;
    use futures::stream::{self, BoxStream};

    /// A log that replays a fixed history, newest-first for `read_before`.
    struct FixedLog(Vec<StoredEvent>);

    #[async_trait]
    impl EventLog for FixedLog {
        async fn append(&self, _id: &CompanyId, _e: CompanyEvent) -> crate::Result<EventSeq> {
            unreachable!("read_thread only reads")
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            seq: EventSeq,
            limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            Ok(self
                .0
                .iter()
                .filter(|e| e.seq >= seq)
                .take(limit)
                .cloned()
                .collect())
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    /// A log whose `read_from` always fails, so `read_before`'s default
    /// fallback propagates the error rather than ever returning a page.
    struct FailingLog;

    #[async_trait]
    impl EventLog for FailingLog {
        async fn append(&self, _id: &CompanyId, _e: CompanyEvent) -> crate::Result<EventSeq> {
            unreachable!("read_thread only reads")
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            Err(crate::error::OpenCompanyError::Store("boom".into()))
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    fn op(seq: u64, chat: &str, parent: Option<u64>, text: &str) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            company: CompanyId::new("acme"),
            event: CompanyEvent::OperatorMessage {
                text: text.to_string(),
                by: None,
                chat: Some(chat.to_string()),
                parent: parent.map(EventSeq::new),
                deliverable: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
            },
            at_millis: seq,
        }
    }

    fn reply(seq: u64, chat: &str, parent: u64, text: &str) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            company: CompanyId::new("acme"),
            event: CompanyEvent::AgentReply {
                audience: Vec::new(),
                chat_id: chat.to_string(),
                agent_id: "ceo".to_string(),
                text: text.to_string(),
                steps: Vec::new(),
                task_id: None,
                outputs: Vec::new(),
                parent: Some(EventSeq::new(parent)),
                mentions: Vec::new(),
                mention_depth: 0,
            },
            at_millis: seq,
        }
    }

    fn tool(events: Vec<StoredEvent>) -> (ReadThreadTool, tempfile::TempDir) {
        // A real store, so desk resolution runs the way it does in production
        // rather than being stubbed past. With no company record on disk,
        // `resolve_seed_desk` passes the addressed id through verbatim as both
        // terms — which is the shape these fixtures journal under.
        let dir = tempfile::Builder::new()
            .prefix("read-thread-")
            .tempdir()
            .expect("tempdir");
        let store: Arc<dyn crate::ports::store::CompanyStore> =
            Arc::new(crate::store::FsCompanyStore::new(dir.path()));
        (
            ReadThreadTool::new(CompanyId::new("acme"), Arc::new(FixedLog(events)), store),
            dir,
        )
    }

    /// Run `fut` as a turn answering in `channel`.
    async fn in_channel<T>(channel: Option<&str>, fut: impl std::future::Future<Output = T>) -> T {
        crate::runtime::delegation::with_turn_conversation(channel.map(str::to_string), fut).await
    }

    #[tokio::test]
    async fn it_reads_a_thread_oldest_first() {
        let (tool, _dir) = tool(vec![
            op(41, "growth", None, "draft the launch email"),
            reply(42, "growth", 41, "here is a draft"),
            op(43, "growth", Some(41), "make it shorter"),
        ]);
        let out = in_channel(Some("growth"), tool.execute(json!({ "root": 41 })))
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            out.output(),
            "operator: draft the launch email\nceo: here is a draft\noperator: make it shorter",
            "the thread reads in the order it happened, root included"
        );
    }

    /// A sibling thread's turns are not this thread's. Reading them would be
    /// the leak #1890 A closed, arriving through the tool.
    #[tokio::test]
    async fn it_returns_only_the_requested_thread() {
        let (tool, _dir) = tool(vec![
            op(41, "growth", None, "draft the launch email"),
            reply(42, "growth", 41, "here is a draft"),
            op(43, "growth", None, "what's our Q3 CAC?"),
            reply(44, "growth", 43, "$412, up 18%"),
        ]);
        let out = in_channel(Some("growth"), tool.execute(json!({ "root": 41 })))
            .await
            .unwrap();
        assert!(out.output().contains("here is a draft"), "{out:?}");
        assert!(!out.output().contains("$412"), "{out:?}");
    }

    /// **The back door this tool must not open.** A root in another channel is
    /// refused BY NAME rather than answered with silence: "not yours to read"
    /// and "there is nothing there" are different facts, and the second invites
    /// a retry that will also fail.
    #[tokio::test]
    async fn a_thread_in_another_channel_is_refused_and_says_so() {
        let (tool, _dir) = tool(vec![
            op(41, "engineering", None, "the migration plan"),
            reply(42, "engineering", 41, "here it is"),
        ]);
        let out = in_channel(Some("growth"), tool.execute(json!({ "root": 41 })))
            .await
            .unwrap();
        assert!(out.is_error, "{out:?}");
        assert!(out.output().contains("different channel"), "{out:?}");
        assert!(
            !out.output().contains("here it is"),
            "the refusal must not leak the thread it refuses: {out:?}"
        );
    }

    /// A turn with no conversation — a dispatched card, a workflow node — has
    /// no threads it is entitled to read. `None` is a refusal, not a wildcard.
    #[tokio::test]
    async fn a_turn_with_no_conversation_may_read_nothing() {
        let (tool, _dir) = tool(vec![op(41, "growth", None, "draft the launch email")]);
        let out = in_channel(None, tool.execute(json!({ "root": 41 })))
            .await
            .unwrap();
        assert!(out.is_error, "{out:?}");
        assert!(out.output().contains("not in one"), "{out:?}");
    }

    #[tokio::test]
    async fn an_unknown_root_says_it_may_be_out_of_the_window() {
        let (tool, _dir) = tool(vec![op(41, "growth", None, "draft the launch email")]);
        let out = in_channel(Some("growth"), tool.execute(json!({ "root": 999 })))
            .await
            .unwrap();
        assert!(out.is_error, "{out:?}");
        assert!(out.output().contains("older than the window"), "{out:?}");
    }

    /// Truncation is DECLARED — `query_company`'s lesson, where a partial list
    /// read as complete and "we have no record of that" became a conclusion the
    /// orchestrator could reach from it.
    /// The cut keeps the **newest** turns.
    ///
    /// `turns` is oldest-first, so a plain `truncate` kept the opening of the
    /// thread and dropped its conclusion — while the notice said the opposite,
    /// so a question about what was decided would be answered from the part
    /// before anything was (codex + coderabbit on #1972).
    #[tokio::test]
    async fn a_long_thread_keeps_the_newest_turns() {
        let mut events = vec![op(1, "growth", None, "the root")];
        for n in 0..THREAD_TURN_LIMIT + 5 {
            events.push(reply(10 + n as u64, "growth", 1, &format!("turn {n}")));
        }
        let (tool, _dir) = tool(events);
        let out = in_channel(Some("growth"), tool.execute(json!({ "root": 1 })))
            .await
            .unwrap();
        let last = THREAD_TURN_LIMIT + 4;
        assert!(
            out.output().contains(&format!("turn {last}")),
            "the thread's conclusion must survive the cut: {out:?}"
        );
        assert!(
            !out.output().contains("turn 0\n") && !out.output().ends_with("turn 0"),
            "and its opening is what gets dropped: {out:?}"
        );
    }

    #[tokio::test]
    async fn a_long_thread_declares_what_it_left_out() {
        let mut events = vec![op(1, "growth", None, "the root")];
        for n in 0..THREAD_TURN_LIMIT + 5 {
            events.push(reply(10 + n as u64, "growth", 1, &format!("turn {n}")));
        }
        let (tool, _dir) = tool(events);
        let out = in_channel(Some("growth"), tool.execute(json!({ "root": 1 })))
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.output()
                .contains("and 6 earlier turn(s) in this thread, not shown"),
            "{out:?}"
        );
    }

    /// The event log is a dependency, not the ground: when it errors,
    /// `read_thread` must report the failure rather than let it become a
    /// panic or a silent empty thread.
    #[tokio::test]
    async fn a_history_read_failure_is_reported_not_swallowed() {
        let dir = tempfile::Builder::new()
            .prefix("read-thread-fail-")
            .tempdir()
            .expect("tempdir");
        let store: Arc<dyn crate::ports::store::CompanyStore> =
            Arc::new(crate::store::FsCompanyStore::new(dir.path()));
        let tool = ReadThreadTool::new(CompanyId::new("acme"), Arc::new(FailingLog), store);
        let out = in_channel(Some("growth"), tool.execute(json!({ "root": 41 })))
            .await
            .expect("execute returns a ToolResult, not a Rust error");
        assert!(
            out.is_error,
            "a log failure must surface as a refusal: {out:?}"
        );
        assert!(
            out.output()
                .contains("Could not read the channel's history"),
            "{out:?}"
        );
        assert!(
            out.output().contains("boom"),
            "the underlying error is named, not hidden: {out:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_root_argument_is_a_refusal_not_a_panic() {
        let (tool, _dir) = tool(vec![]);
        let out = in_channel(Some("growth"), tool.execute(json!({})))
            .await
            .unwrap();
        assert!(out.is_error, "{out:?}");
        assert!(out.output().contains("needs `root`"), "{out:?}");
    }

    /// The tool is read-only, which is what keeps it out of every approval and
    /// grant path a write would have to pass.
    #[test]
    fn the_tool_is_read_only() {
        let (tool, _dir) = tool(vec![]);
        assert!(matches!(tool.permission_level(), PermissionLevel::ReadOnly));
        assert_eq!(tool.name(), READ_THREAD_TOOL);
    }
}
