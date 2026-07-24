//! Shared desk-history read logic (issue #65).
//!
//! Both the GraphQL `Chat.history` resolver
//! ([`crate::server::graphql::company`]) and the REST `GET .../chat/history`
//! route ([`crate::server::operator`]) need to answer the same question — "what
//! messages belong to this desk, as seen by this viewer?" — and they must never
//! be allowed to disagree about it. This module is the one place that answers
//! it; both surfaces call through it instead of each keeping their own copy of
//! the filter + projection logic.

use std::collections::HashMap;

use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::{ActorKind, CompanyEvent, EventSeq, StoredEvent};
use crate::server::ops::language::DEFAULT_DESK as GENERAL_DESK;

/// The console's default/orchestrator thread id
/// (`frontend/src/lib/threads.ts` `mainThread()`). The console addresses every
/// send on that thread with `chat: "main"`, so `AgentReply`s answering it are
/// journaled with `chat_id == "main"` rather than [`GENERAL_DESK`]. `owns`
/// admits both spellings for the General desk so a transcript is never split
/// across the two ids depending on which one happened to write it (issue #65).
pub const MAIN_THREAD_ID: &str = "main";

/// Whether a stored event belongs to the desk identified by `desk_id` /
/// `desk_name`.
///
/// Both `AgentReply`s and `OperatorMessage`s match on the desk id or name
/// verbatim, plus — only for the General/operator desk — the console's `"main"`
/// thread id and an empty chat id, so no historical message is orphaned by the
/// id it happened to be journaled under (issue #65). An operator message routes
/// by its stored `chat` id symmetrically with an agent reply's `chat_id`; only
/// a legacy operator message with no stored chat id (empty/`None`) falls back
/// to belonging to the General desk.
pub fn owns(desk_id: &str, desk_name: &str, event: &CompanyEvent) -> bool {
    let is_general_desk =
        desk_id.eq_ignore_ascii_case(GENERAL_DESK) || desk_name.eq_ignore_ascii_case(GENERAL_DESK);
    match event {
        CompanyEvent::AgentReply { chat_id, .. } => {
            chat_id == desk_id
                || chat_id == desk_name
                || (is_general_desk
                    && (chat_id.is_empty() || chat_id.eq_ignore_ascii_case(MAIN_THREAD_ID)))
        }
        CompanyEvent::OperatorMessage { chat, .. } => {
            let chat = chat.as_deref().unwrap_or_default();
            chat == desk_id
                || chat == desk_name
                || (is_general_desk
                    && (chat.is_empty() || chat.eq_ignore_ascii_case(MAIN_THREAD_ID)))
        }
        _ => false,
    }
}

/// Who is reading a desk history. `mine` is relative to this.
///
/// There is no `From<StoredEvent> for MessageView`, and there cannot be:
/// `mine` depends on who is asking. With one operator it was safe to hardcode
/// `true`; with several users it would mark everyone's messages as everyone
/// else's.
#[derive(Clone, Debug, PartialEq)]
pub enum Viewer {
    /// An operator or platform credential. Legacy unattributed messages are
    /// theirs, because that is who sent them before users existed.
    Operator,
    /// A human collaborator, by user id.
    User(String),
}

/// One message in a desk history, independent of transport. Mirrors
/// `frontend/src/lib/chat.ts`. The GraphQL `Message` type and the REST
/// `chat/history` JSON shape both project from this.
#[derive(Clone, Debug)]
pub struct MessageView {
    /// The message id (its EventLog sequence position).
    pub id: String,
    /// The channel the message came in on.
    pub channel: String,
    /// The author label.
    pub author: String,
    /// The message text.
    pub text: String,
    /// When it was journaled, epoch millis.
    pub at_millis: f64,
    /// Whether it is the operator's own message.
    pub mine: bool,
}

impl MessageView {
    /// Projects a stored event for one viewer.
    ///
    /// `authors` maps user id → display label, resolved once per history
    /// rather than per message.
    pub fn project(
        stored: StoredEvent,
        viewer: &Viewer,
        authors: &HashMap<String, String>,
    ) -> Self {
        let id = stored.seq.value().to_string();
        let at_millis = stored.at_millis as f64;
        match stored.event {
            CompanyEvent::AgentReply { agent_id, text, .. } => MessageView {
                id,
                channel: agent_id.clone(),
                author: agent_id,
                text,
                at_millis,
                mine: false,
            },
            CompanyEvent::OperatorMessage { text, by, .. } => {
                let (author, mine) = match &by {
                    // Sent by a signed-in human.
                    Some(actor) if actor.kind == ActorKind::User => {
                        let label = authors
                            .get(&actor.id)
                            .cloned()
                            .unwrap_or_else(|| "someone".to_string());
                        (label, *viewer == Viewer::User(actor.id.clone()))
                    }
                    // Sent with a machine credential, or journaled before
                    // attribution existed. Either way there is no person to
                    // name, and it belongs to whoever holds that credential.
                    _ => ("operator".to_string(), matches!(viewer, Viewer::Operator)),
                };
                MessageView {
                    id,
                    channel: "operator".to_string(),
                    author,
                    text,
                    at_millis,
                    mine,
                }
            }
            // `owns` never admits other variants into a history.
            other => MessageView {
                id,
                channel: "system".to_string(),
                author: "system".to_string(),
                text: format!("{other:?}"),
                at_millis,
                mine: false,
            },
        }
    }
}

/// Loads roster display labels for a company: user id → label.
///
/// Prefers a display name, and falls back to the email's *local part* rather
/// than the whole address: a desk history is read by every member, and it
/// should not hand each of them everyone else's email.
pub async fn author_labels(
    runtime: &CompanyRuntime,
) -> Result<HashMap<String, String>, OpenCompanyError> {
    let users = runtime.users().list_users(runtime.id()).await?;
    Ok(users
        .into_iter()
        .map(|user| {
            let label = user.display_name.unwrap_or_else(|| {
                user.email
                    .split('@')
                    .next()
                    .unwrap_or("someone")
                    .to_string()
            });
            (user.id, label)
        })
        .collect())
}

/// One desk's message history for one viewer, most-recent last.
///
/// `before_seq` is an opaque EventLog cursor (a sequence position); only
/// messages before it are considered. `first` caps how many of the remaining,
/// most-recent messages come back. Returns the page plus the total count of
/// matching messages (before the `first` cap, after the `before_seq` cut).
///
/// Shared by the GraphQL `Chat.history` resolver and the REST
/// `GET .../chat/history` route so the two can never disagree about what a
/// desk's history contains (issue #65).
pub async fn history_for_desk(
    runtime: &CompanyRuntime,
    desk_id: &str,
    desk_name: &str,
    viewer: &Viewer,
    before_seq: Option<u64>,
    first: usize,
) -> Result<(Vec<MessageView>, i32), OpenCompanyError> {
    let stored = runtime
        .events()
        .read_from(runtime.id(), EventSeq::new(0), usize::MAX)
        .await?;
    // One roster read per history, not one per message: the scan above is
    // already O(log), and an N+1 on top of it would be worse.
    let authors = author_labels(runtime).await?;

    let mut messages: Vec<MessageView> = stored
        .into_iter()
        .filter(|event| owns(desk_id, desk_name, &event.event))
        .filter(|event| before_seq.is_none_or(|before| event.seq.value() < before))
        .map(|event| MessageView::project(event, viewer, &authors))
        .collect();

    let total = messages.len() as i32;
    // Keep the most recent `first`, still in chronological order.
    if messages.len() > first {
        messages.drain(0..messages.len() - first);
    }
    Ok((messages, total))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::Actor;

    fn agent_reply(chat_id: &str) -> CompanyEvent {
        CompanyEvent::AgentReply {
            chat_id: chat_id.to_string(),
            agent_id: "ceo".to_string(),
            text: "hi".to_string(),
        }
    }

    #[test]
    fn general_desk_owns_agent_replies_under_general_and_main() {
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &agent_reply(GENERAL_DESK)));
        assert!(owns(
            GENERAL_DESK,
            GENERAL_DESK,
            &agent_reply(MAIN_THREAD_ID)
        ));
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &agent_reply("")));
        assert!(!owns(GENERAL_DESK, GENERAL_DESK, &agent_reply("strategy")));
    }

    #[test]
    fn non_general_desk_only_owns_its_own_id_or_name() {
        assert!(owns("strategy", "Strategy desk", &agent_reply("strategy")));
        assert!(owns(
            "strategy",
            "Strategy desk",
            &agent_reply("Strategy desk")
        ));
        assert!(!owns(
            "strategy",
            "Strategy desk",
            &agent_reply(MAIN_THREAD_ID)
        ));
        assert!(!owns("strategy", "Strategy desk", &agent_reply("")));
    }

    #[test]
    fn general_desk_owns_every_operator_message() {
        let event = CompanyEvent::OperatorMessage {
            text: "hi".to_string(),
            by: Some(Actor {
                kind: ActorKind::User,
                id: "u1".to_string(),
            }),
            chat: Some(MAIN_THREAD_ID.to_string()),
        };
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &event));
        assert!(!owns("strategy", "Strategy desk", &event));
    }

    // Regression: issue — operator messages vanished on reload because the read
    // filter ignored the stored chat id.
    #[test]
    fn main_thread_owns_operator_messages_it_stored() {
        let event = CompanyEvent::OperatorMessage {
            text: "hi".to_string(),
            by: None,
            chat: Some(MAIN_THREAD_ID.to_string()),
        };
        // The console queries the main thread with desk = ("main", "main").
        assert!(owns(MAIN_THREAD_ID, MAIN_THREAD_ID, &event));
        // And it is still owned when read under the General desk's own id/name.
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &event));
        // But it must not leak into an unrelated desk.
        assert!(!owns("strategy", "Strategy desk", &event));
    }

    #[test]
    fn desk_addressed_operator_message_belongs_to_that_desk() {
        let event = CompanyEvent::OperatorMessage {
            text: "hi".to_string(),
            by: None,
            chat: Some("strategy".to_string()),
        };
        assert!(owns("strategy", "Strategy desk", &event));
        assert!(!owns(MAIN_THREAD_ID, MAIN_THREAD_ID, &event));
    }

    #[test]
    fn legacy_operator_message_without_chat_stays_on_general() {
        let event = CompanyEvent::OperatorMessage {
            text: "hi".to_string(),
            by: None,
            chat: None,
        };
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &event));
        assert!(!owns("strategy", "Strategy desk", &event));
    }
}
