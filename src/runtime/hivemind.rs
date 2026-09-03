//! The `tinyhivemind` session adapter — off by default (`hivemind`).
//!
//! [`tinyhivemind::session::project_session`] answers the question this host
//! answers today in [`crate::server::chat_history`]: what does one participant
//! see of a transcript several of them share? It answers it **attributed** —
//! every projected line keeps the author that wrote it. This host's own
//! projection does not: an agent's turn history is a `(role, content)` ladder
//! in which every prior reply is folded into the *reader's* own voice, so on a
//! shared desk agent B reads agent A's replies as B's own earlier turns, and a
//! system notice, a workflow report and a real teammate are indistinguishable.
//! That is the first of the two defects `vendor/tinyhivemind` was extracted to
//! fix (its `ROADMAP.md`, P4).
//!
//! What lands here is the port that fix needs and nothing more:
//! [`JournalSessionLog`] reads this company's journal as
//! [`tinyhivemind::session::SessionLog`] wants to read it — newest-first, by an
//! exclusive cursor, from the tail. **No turn calls it yet.** The adapter is
//! built, gated and tested before it is wired, because the wiring is where the
//! behavior change lives and it deserves its own diff; see the roadmap's
//! "paired OpenCompany adapter integration", which lands disabled.
//!
//! # Why the journal is already the right shape
//!
//! [`EventLog::read_before`](crate::ports::events::EventLog::read_before) is
//! this port's primitive under another name — the same exclusive `before`, the
//! same newest-first order, the same tail seek. That is not a coincidence: the
//! trait's own cost note cites this host's measurement (72.8ms against 0.4ms at
//! 100k events) as the reason it is specified that way. So the adapter is a
//! mapping, not a reimplementation, and the only real work it does is deciding
//! which stored events are *messages*.
//!
//! # Which events are messages
//!
//! Exactly the three [`owns`](crate::server::chat_history::owns) admits into a
//! desk history, mapped to the author each one preserves. Everything else in
//! the journal — runs, budgets, lifecycle — is not a line anybody said, and is
//! skipped. Skipping is what makes the cursor contract load-bearing below.
//!
//! # What this module does not decide
//!
//! Which desk a row belongs to. The projection folds the four spellings of
//! General itself (`tinyhivemind_core::chat::same_conversation`, the same rule
//! [`owns`](crate::server::chat_history::owns) applies), so the adapter hands
//! over every chat row and lets the library filter. An adapter that pre-filtered
//! by desk would be a second answer to a question the library exists to answer
//! once.

use std::collections::HashMap;

use tinyhivemind::session::{
    LogMessage, Sequence, SessionAuthor, SessionFuture, SessionLog, SessionPage,
};

use crate::ports::events::EventLog;
use crate::ports::types::{
    ActorKind, CompanyEvent, CompanyId, CompanyRecord, EventSeq, StoredEvent,
};

/// This company's journal, read as a [`SessionLog`].
///
/// Borrows rather than owns: one is built for the duration of a single
/// projection, from the log, record and label map a caller already holds.
pub struct JournalSessionLog<'a> {
    events: &'a dyn EventLog,
    company: &'a CompanyId,
    record: &'a CompanyRecord,
    people: &'a HashMap<String, String>,
}

// Hand-written because `&dyn EventLog` has no `Debug`, and the port is not this
// module's to change for the sake of a derive.
impl std::fmt::Debug for JournalSessionLog<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JournalSessionLog")
            .field("company", &self.company)
            .finish_non_exhaustive()
    }
}

impl<'a> JournalSessionLog<'a> {
    /// Borrow everything one projection needs.
    ///
    /// `people` is user id → display label, exactly as
    /// [`author_labels`](crate::server::chat_history::author_labels) builds it:
    /// the ladder that prefers a display name over an email local part is one
    /// this adapter must not fork, because a transcript read by every member
    /// should not hand each of them everyone else's address.
    #[must_use]
    pub const fn new(
        events: &'a dyn EventLog,
        company: &'a CompanyId,
        record: &'a CompanyRecord,
        people: &'a HashMap<String, String>,
    ) -> Self {
        Self {
            events,
            company,
            record,
            people,
        }
    }
}

impl SessionLog for JournalSessionLog<'_> {
    /// One page of chat rows older than `before`, newest first.
    ///
    /// # The cursor, and why this loops
    ///
    /// Most journal rows are not messages, so a raw read of `limit` rows can
    /// yield none. An empty page is only legal with **no** cursor
    /// (`Error::EmptyPageCursor`), and returning no cursor means "the log ends
    /// here" — which would silently truncate a transcript whose older half sits
    /// behind a run of workflow events. So a page that filtered down to nothing
    /// keeps reading rather than reporting an end that is not there.
    ///
    /// It terminates: each pass starts strictly older than the last row it saw,
    /// and a short raw read is the log's own end.
    fn read_before(&self, before: Option<Sequence>, limit: usize) -> SessionFuture<'_> {
        Box::pin(async move {
            let mut cursor = before.map(|sequence| EventSeq::new(sequence.0));
            loop {
                let raw = self
                    .events
                    .read_before(self.company, cursor, limit)
                    .await
                    .map_err(|error| Box::new(error) as tinyhivemind::session::SourceError)?;
                let Some(oldest) = raw.last().map(|stored| stored.seq) else {
                    return Ok(SessionPage::default());
                };
                // A short read is the only end-of-log signal the port has: the
                // store returns what it has, so fewer rows than asked for means
                // there are no older ones.
                let exhausted = raw.len() < limit;
                let messages: Vec<LogMessage> = raw
                    .iter()
                    .filter_map(|stored| self.log_message(stored))
                    .collect();
                if !messages.is_empty() || exhausted {
                    return Ok(SessionPage {
                        messages,
                        // The oldest row *scanned*, not the oldest returned: the
                        // rows in between were skipped, and a cursor that
                        // re-offered them would make the next page repeat work
                        // this one already did. Never newer than the oldest
                        // returned row, which is the contract.
                        next_before: (!exhausted).then(|| Sequence(oldest.value())),
                    });
                }
                cursor = Some(oldest);
            }
        })
    }
}

impl JournalSessionLog<'_> {
    /// One stored event as a log row, or `None` if it is not a message.
    fn log_message(&self, stored: &StoredEvent) -> Option<LogMessage> {
        let sequence = Sequence(stored.seq.value());
        match &stored.event {
            CompanyEvent::AgentReply {
                chat_id,
                agent_id,
                text,
                parent,
                ..
            } => Some(LogMessage {
                sequence,
                chat_id: Some(chat_id.clone()),
                parent: parent.map(|seq| Sequence(seq.value())),
                author: self.agent_author(agent_id),
                content: text.clone(),
            }),
            CompanyEvent::OperatorMessage {
                text,
                by,
                chat,
                parent,
                ..
            } => Some(LogMessage {
                sequence,
                chat_id: chat.clone(),
                parent: parent.map(|seq| Sequence(seq.value())),
                author: self.authored_by(by.as_ref()),
                content: text.clone(),
            }),
            CompanyEvent::DeskTaskCompleted {
                column,
                origin_chat_id,
                origin_parent,
                ..
            } => Some(LogMessage {
                sequence,
                chat_id: origin_chat_id.clone(),
                parent: origin_parent.map(|seq| Sequence(seq.value())),
                author: SessionAuthor::System {
                    kind: "task_settled".to_string(),
                    label: crate::ports::SYSTEM_AUTHOR.to_string(),
                },
                // Through the one function that owns this sentence, rather than
                // a second spelling of it: `dispatch_marker_text`'s own doc
                // names the console copy as "the one remaining exception", and
                // a third would make a marker reword itself depending on which
                // reader rendered it.
                content: crate::server::chat_history::dispatch_marker_text(column),
            }),
            _ => None,
        }
    }

    /// An agent id as an attributed author.
    ///
    /// The label ladder is the operator-given display name, then the manifest
    /// role, then the id — the order the console shows a teammate in, so a
    /// transcript names it the way its reader's interface does.
    fn agent_author(&self, agent_id: &str) -> SessionAuthor {
        // Manifest before overlay, the order `resolve_roster_agent_id` reads
        // them in — and a manifest `[[agent]]` carries no display name at all,
        // which is why the two tiers cannot share one pass.
        let declared = self
            .record
            .manifest
            .agents
            .iter()
            .find(|agent| agent.id.eq_ignore_ascii_case(agent_id))
            .map(|agent| {
                agent
                    .name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| agent.role.clone())
            });
        let label = declared
            .or_else(|| {
                self.record
                    .overlay_agents
                    .iter()
                    .find(|agent| agent.id.eq_ignore_ascii_case(agent_id))
                    .map(|agent| {
                        if agent.name.trim().is_empty() {
                            agent.role.clone()
                        } else {
                            agent.name.clone()
                        }
                    })
            })
            .filter(|label| !label.trim().is_empty())
            .unwrap_or_else(|| agent_id.to_string());
        SessionAuthor::Agent {
            id: agent_id.to_string(),
            label,
        }
    }

    /// An operator message's stored actor as an attributed author.
    ///
    /// `None` is the operator: an event journaled before per-user auth existed,
    /// or a send made with a platform credential, has no person behind it —
    /// the same reading [`crate::server::chat_history`] gives it.
    fn authored_by(&self, actor: Option<&crate::ports::types::Actor>) -> SessionAuthor {
        let Some(actor) = actor else {
            return SessionAuthor::Operator;
        };
        match actor.kind {
            ActorKind::Operator => SessionAuthor::Operator,
            ActorKind::User => SessionAuthor::Person {
                id: actor.id.clone(),
                label: self
                    .people
                    .get(&actor.id)
                    .cloned()
                    .unwrap_or_else(|| "someone".to_string()),
            },
            ActorKind::Agent => self.agent_author(&actor.id),
            ActorKind::System => SessionAuthor::System {
                kind: "runtime".to_string(),
                label: crate::ports::SYSTEM_AUTHOR.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::error::OpenCompanyError;
    use crate::ports::types::{Actor, CompanyEvent, EventSeq, StoredEvent};
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use tinyhivemind::session::{Conversation, SESSION_WINDOW, SessionQuery, project_session};

    /// A journal that answers `read_before` the way a production store does —
    /// from the tail, by an exclusive cursor. The default fallback on the trait
    /// would pass these tests too, and would hide a page-contract mistake that
    /// only a real tail read makes: this port is specified newest-first, so the
    /// double it is tested against has to be newest-first for real.
    struct Journal(Vec<StoredEvent>);

    #[async_trait]
    impl EventLog for Journal {
        async fn append(
            &self,
            _id: &CompanyId,
            _event: CompanyEvent,
        ) -> Result<EventSeq, OpenCompanyError> {
            unreachable!("the adapter never appends")
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            seq: EventSeq,
            limit: usize,
        ) -> Result<Vec<StoredEvent>, OpenCompanyError> {
            Ok(self
                .0
                .iter()
                .filter(|stored| stored.seq >= seq)
                .take(limit)
                .cloned()
                .collect())
        }
        async fn read_before(
            &self,
            _id: &CompanyId,
            before: Option<EventSeq>,
            limit: usize,
        ) -> Result<Vec<StoredEvent>, OpenCompanyError> {
            let mut rows: Vec<StoredEvent> = self
                .0
                .iter()
                .filter(|stored| before.is_none_or(|cursor| stored.seq < cursor))
                .cloned()
                .collect();
            rows.reverse();
            rows.truncate(limit);
            Ok(rows)
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(futures::stream::empty())
        }
    }

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    fn record() -> CompanyRecord {
        let src = "[company]\nname = \"Acme\"\n\n[policy]\nmode = \"full\"\n\
                   \n[[agent]]\nid = \"engineer\"\nrole = \"Engineer\"\ntier = \"orchestrator\"\n\
                   \n[[agent]]\nid = \"designer\"\nrole = \"Designer\"\ntier = \"orchestrator\"\n";
        let manifest: crate::company::CompanyManifest =
            toml::from_str(src).expect("manifest parses");
        CompanyRecord {
            id: company(),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: true,
            created_at_millis: None,
            activation_completed_at: None,
        }
    }

    fn stored(seq: u64, event: CompanyEvent) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            company: company(),
            event,
            at_millis: 1_000 + seq,
        }
    }

    fn reply(agent_id: &str, chat: &str, text: &str) -> CompanyEvent {
        CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            task_id: None,
            chat_id: chat.to_string(),
            agent_id: agent_id.to_string(),
            text: text.to_string(),
            steps: Vec::new(),
        }
    }

    fn message(by: Option<Actor>, chat: Option<&str>, text: &str) -> CompanyEvent {
        CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: text.to_string(),
            by,
            chat: chat.map(str::to_string),
            deliverable: None,
            attachments: Vec::new(),
        }
    }

    /// An event that is emphatically not a line anybody said.
    fn noise(seq: u64) -> StoredEvent {
        stored(
            seq,
            CompanyEvent::WorkflowRunStarted {
                workflow_id: "wf".into(),
                run_id: format!("run-{seq}"),
                scheduled: false,
                started_by: None,
                resume_semantic: None,
            },
        )
    }

    async fn read(journal: &Journal, before: Option<u64>, limit: usize) -> SessionPage {
        let record = record();
        let people = HashMap::new();
        let company = company();
        let log = JournalSessionLog::new(journal, &company, &record, &people);
        log.read_before(before.map(Sequence), limit)
            .await
            .expect("the journal reads")
    }

    /// The defect this adapter exists to remove: a shared desk's transcript
    /// keeps the author of every line, so one agent reads its colleague as its
    /// colleague and not as itself.
    #[tokio::test]
    async fn a_shared_desk_reads_back_attributed() {
        let journal = Journal(vec![
            stored(
                1,
                message(None, Some("engineering"), "who owns the migration?"),
            ),
            stored(2, reply("engineer", "engineering", "I do.")),
            stored(
                3,
                reply("designer", "engineering", "I will take the console half."),
            ),
        ]);
        let record = record();
        let people = HashMap::new();
        let company = company();
        let log = JournalSessionLog::new(&journal, &company, &record, &people);

        let projected = project_session(
            &log,
            &SessionQuery {
                conversation: Conversation {
                    desk_id: "engineering".to_string(),
                    desk_name: "Engineering".to_string(),
                    thread_root: None,
                },
                before: None,
                window: SESSION_WINDOW,
            },
        )
        .await
        .expect("the projection folds");

        let authors: Vec<&SessionAuthor> = projected.iter().map(|m| &m.author).collect();
        assert_eq!(
            authors,
            vec![
                &SessionAuthor::Operator,
                &SessionAuthor::Agent {
                    id: "engineer".to_string(),
                    label: "Engineer".to_string(),
                },
                &SessionAuthor::Agent {
                    id: "designer".to_string(),
                    label: "Designer".to_string(),
                },
            ],
            "three lines, three authors — none of them collapsed into the reader"
        );
        assert!(
            projected.iter().all(|m| !m.content.is_empty()),
            "content travels with the author"
        );
    }

    /// The four spellings of General are the library's business, not the
    /// adapter's: an unaddressed post stores `None` and still belongs to the
    /// General desk.
    #[tokio::test]
    async fn an_unaddressed_post_projects_under_general() {
        let journal = Journal(vec![
            stored(1, message(None, None, "morning")),
            stored(2, reply("engineer", "main", "morning")),
            stored(3, reply("designer", "strategy", "not this desk")),
        ]);
        let record = record();
        let people = HashMap::new();
        let company = company();
        let log = JournalSessionLog::new(&journal, &company, &record, &people);

        let projected = project_session(
            &log,
            &SessionQuery {
                conversation: Conversation {
                    desk_id: "General".to_string(),
                    desk_name: "General".to_string(),
                    thread_root: None,
                },
                before: None,
                window: SESSION_WINDOW,
            },
        )
        .await
        .expect("the projection folds");

        assert_eq!(projected.len(), 2, "`None` and `main` are one conversation");
        assert!(
            projected.iter().all(|m| m.content != "not this desk"),
            "a named desk's traffic stays out of General"
        );
    }

    /// A user's message is that person, and an unattributed one is the
    /// operator — the reading `chat_history` already gives a stored `by`.
    #[tokio::test]
    async fn an_operator_message_keeps_who_sent_it() {
        let journal = Journal(vec![
            stored(1, message(None, Some("engineering"), "legacy send")),
            stored(
                2,
                message(
                    Some(Actor {
                        kind: ActorKind::User,
                        id: "u1".to_string(),
                    }),
                    Some("engineering"),
                    "mine",
                ),
            ),
        ]);
        let record = record();
        let people = HashMap::from([("u1".to_string(), "Ada".to_string())]);
        let company = company();
        let log = JournalSessionLog::new(&journal, &company, &record, &people);
        let page = log.read_before(None, 8).await.expect("the journal reads");

        assert_eq!(
            page.messages[1].author,
            SessionAuthor::Operator,
            "an event journaled before per-user auth is the operator"
        );
        assert_eq!(
            page.messages[0].author,
            SessionAuthor::Person {
                id: "u1".to_string(),
                label: "Ada".to_string(),
            },
            "and a user's send is that person, labelled the way the console labels them"
        );
    }

    /// The page contract, asserted rather than assumed: newest-first, never
    /// larger than asked for, and a cursor no newer than the oldest row.
    #[tokio::test]
    async fn a_page_is_newest_first_and_bounded() {
        let journal = Journal(
            (1..=10)
                .map(|seq| stored(seq, reply("engineer", "engineering", "hi")))
                .collect(),
        );
        let page = read(&journal, None, 4).await;

        let sequences: Vec<u64> = page.messages.iter().map(|m| m.sequence.0).collect();
        assert_eq!(
            sequences,
            vec![10, 9, 8, 7],
            "newest first, at most `limit`"
        );
        assert_eq!(
            page.next_before,
            Some(Sequence(7)),
            "the cursor is exclusive, so the next page starts at 6"
        );

        let older = read(&journal, Some(7), 4).await;
        let sequences: Vec<u64> = older.messages.iter().map(|m| m.sequence.0).collect();
        assert_eq!(
            sequences,
            vec![6, 5, 4, 3],
            "and it neither repeats nor skips"
        );
    }

    /// The reason [`JournalSessionLog::read_before`] loops. A run of non-message
    /// events longer than one page must not read as the end of the log: an empty
    /// page may only carry `None`, and `None` means "no older rows".
    #[tokio::test]
    async fn a_run_of_non_messages_does_not_end_the_transcript() {
        let mut events = vec![stored(
            1,
            reply("engineer", "engineering", "the oldest line"),
        )];
        events.extend((2..=20).map(noise));
        events.push(stored(
            21,
            reply("designer", "engineering", "the newest line"),
        ));
        let journal = Journal(events);

        // Page size 4, so the walk crosses five straight pages of pure noise.
        let newest = read(&journal, None, 4).await;
        assert_eq!(newest.messages.len(), 1, "the newest page holds one line");

        let older = read(&journal, newest.next_before.map(|cursor| cursor.0), 4).await;
        assert_eq!(
            older.messages.len(),
            1,
            "and the next page reaches past the noise to the oldest line"
        );
        assert_eq!(older.messages[0].sequence, Sequence(1));
        assert_eq!(
            older.next_before, None,
            "a short read is the end of the log, and only then is the cursor absent"
        );
    }

    /// A settle marker is a line on the desk, said by the runtime.
    #[tokio::test]
    async fn a_settled_card_is_a_system_line() {
        let journal = Journal(vec![stored(
            1,
            CompanyEvent::DeskTaskCompleted {
                task_id: "t1".into(),
                desk: "engineering".into(),
                output: "done".into(),
                column: "done".into(),
                artifact_ids: Vec::new(),
                origin_chat_id: Some("engineering".into()),
                origin_parent: None,
            },
        )]);
        let page = read(&journal, None, 8).await;

        assert_eq!(
            page.messages[0].author,
            SessionAuthor::System {
                kind: "task_settled".to_string(),
                label: crate::ports::SYSTEM_AUTHOR.to_string(),
            },
        );
        assert_eq!(
            page.messages[0].content,
            crate::server::chat_history::dispatch_marker_text("done"),
            "through the one function that owns the sentence"
        );
    }
}
