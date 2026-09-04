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
    // -----------------------------------------------------------------------
    // SPIKE: the two cross-desk referral journeys, decided against a real
    // `agentic_software_company`-shaped topology.
    //
    // These exercise the PURE decision only — `referral()` performs no I/O and
    // enqueues nothing. What a host still owes before either journey can run is
    // the `ReferralQueue` transaction (idempotency, dual-conversation
    // authorization, the back edge); this proves the decision half fits this
    // host's desks, which is the question that comes first.
    // -----------------------------------------------------------------------

    /// The shipped software company's shape: three desks, no overlap.
    fn software_company() -> CompanyRecord {
        let src = "[company]\nname = \"Acme\"\n\n[policy]\nmode = \"full\"\n\
                   \n[[agent]]\nid = \"software_engineer\"\nrole = \"Engineer\"\n\
                   \n[[agent]]\nid = \"qa_engineer\"\nrole = \"QA\"\n\
                   \n[[agent]]\nid = \"product_designer\"\nrole = \"Designer\"\n\
                   \n[[group_chat]]\nid = \"engineering\"\nname = \"Engineering\"\n\
                   members = [\"software_engineer\", \"qa_engineer\"]\n\
                   \n[[group_chat]]\nid = \"product_design\"\nname = \"Product & Design\"\n\
                   members = [\"product_designer\"]\n";
        let manifest: crate::company::CompanyManifest =
            toml::from_str(src).expect("manifest parses");
        CompanyRecord {
            manifest,
            ..record()
        }
    }

    fn referral_input(
        content: &str,
        mentions: Vec<tinyhivemind_core::mention::Mention>,
    ) -> tinyhivemind_core::referral::ReferralInput {
        tinyhivemind_core::referral::ReferralInput {
            key: tinyhivemind_core::dispatch::DispatchKey {
                trigger_sequence: 42,
            },
            conversation: tinyhivemind_core::dispatch::DispatchConversation {
                desk_id: "engineering".to_string(),
                thread_root: None,
            },
            author_id: "software_engineer".to_string(),
            content: content.to_string(),
            mentions,
            hop: 0,
            origin: None,
        }
    }

    /// **The asker is told it may ask again — that is what makes it able to.**
    ///
    /// The returning answer never reaches the channel, so this frame is the
    /// only thing the asker ever reads about it. Raw, it offered one ending;
    /// framed, it offers both and gives the spelling that reaches back.
    #[test]
    fn a_returning_answer_offers_both_endings_and_a_forward_is_untouched() {
        let record = software_company();
        let members = roster_members(&record);
        let people: Vec<tinyhivemind_core::roster::Person> = Vec::new();
        let retired: Vec<String> = Vec::new();
        let roster = tinyhivemind_core::roster::Roster::new(&members, &people, &retired);
        let desks = desk_snapshots(&record);

        let body = "@product_designer can you take the login screen?";
        let mentions = tinyhivemind_core::mention::resolve(
            body,
            None,
            &tinyhivemind_core::mention::MentionAuthor::Agent {
                id: "software_engineer".to_string(),
            },
            &roster,
            &desks.set(),
        );
        let tinyhivemind_core::referral::ReferralDecision::One { mut referral } =
            tinyhivemind_core::referral::referral(
                tinyhivemind_core::referral::ReferralPolicy {
                    enabled: true,
                    max_hops: 4,
                    reach: tinyhivemind_core::referral::ReferralReach::Channels,
                    returns: true,
                },
                &referral_input(body, mentions),
                &roster,
                &desks.set(),
            )
            .expect("decides")
        else {
            panic!("a crossing referral was available");
        };

        assert_eq!(
            returned_answer(&record, &referral),
            body,
            "a forward carries the asker's own words into a room people read"
        );

        // A real return has the mirrored geometry: it was committed on the desk
        // that answered, and travels to the one that asked. Flipping `kind`
        // alone would leave a forward's desks in place and let this pass while
        // naming the wrong room to go back to.
        referral.kind = tinyhivemind_core::referral::ReferralKind::Return;
        referral.from = referral.to.clone();
        referral.source_id = "product_designer".to_string();
        referral.content = "use a skeleton, not a spinner".to_string();
        let framed = returned_answer(&record, &referral);
        assert!(
            framed.contains("use a skeleton, not a spinner"),
            "the answer survives the framing: {framed}"
        );
        assert!(
            framed.contains("@#product_design"),
            "and names the exact spelling that reaches back: {framed}"
        );
        assert!(
            framed.contains("report back") && framed.contains("ask them again"),
            "both endings are stated, so choosing is the asker's: {framed}"
        );
    }

    /// **The chain deepens, and therefore ends.**
    ///
    /// Each generation must sit one hop below the one that caused it, or the
    /// bound is decorative. The host used to hand every referred turn a
    /// hardcoded depth of `1`, which is true of the first one and of no other:
    /// a follow-up claimed the same depth as the answer it followed, so
    /// `max_hops` was never approached however long two desks went on. Nothing
    /// drove such a loop then — the asker could not ask again — so the defect
    /// was invisible until the moment it mattered.
    ///
    /// Walked here as the policy sees it: the depth a turn reports is the depth
    /// its own replies are offered at, so the walk is `child_hop` feeding the
    /// next `hop`. Four hops is ask, answer, ask again, answer again — and the
    /// fifth is refused.
    #[test]
    fn a_follow_up_is_one_hop_deeper_and_the_budget_ends_it() {
        let record = software_company();
        let members = roster_members(&record);
        let people: Vec<tinyhivemind_core::roster::Person> = Vec::new();
        let retired: Vec<String> = Vec::new();
        let roster = tinyhivemind_core::roster::Roster::new(&members, &people, &retired);
        let desks = desk_snapshots(&record);
        let policy = tinyhivemind_core::referral::ReferralPolicy {
            enabled: true,
            max_hops: 4,
            reach: tinyhivemind_core::referral::ReferralReach::Channels,
            returns: true,
        };

        let body = "@product_designer can you take the login screen?";
        let mut depths: Vec<u32> = Vec::new();
        let mut hop = 0;
        loop {
            let mentions = tinyhivemind_core::mention::resolve(
                body,
                None,
                &tinyhivemind_core::mention::MentionAuthor::Agent {
                    id: "software_engineer".to_string(),
                },
                &roster,
                &desks.set(),
            );
            let mut input = referral_input(body, mentions);
            input.hop = hop;
            match tinyhivemind_core::referral::referral(policy, &input, &roster, &desks.set())
                .expect("decides")
            {
                tinyhivemind_core::referral::ReferralDecision::One { referral } => {
                    assert_eq!(
                        referral.child_hop,
                        hop + 1,
                        "a child sits exactly one below its cause"
                    );
                    depths.push(referral.child_hop);
                    hop = referral.child_hop;
                }
                tinyhivemind_core::referral::ReferralDecision::None { reason } => {
                    assert_eq!(
                        reason,
                        tinyhivemind_core::referral::NoReferralReason::HopLimitReached,
                        "the chain ends because the budget ran out, not for some other reason"
                    );
                    break;
                }
            }
            assert!(hop <= 8, "the walk must terminate; it did not");
        }

        assert_eq!(
            depths,
            vec![1, 2, 3, 4],
            "four hops: ask, answer, ask again, answer again"
        );
    }

    /// **Journey 1** — a named teammate who is not on this desk runs on THEIR
    /// desk, rather than being pulled into this conversation.
    #[test]
    fn a_named_outsider_is_referred_to_their_own_desk() {
        let record = software_company();
        let members = roster_members(&record);
        let people: Vec<tinyhivemind_core::roster::Person> = Vec::new();
        let retired: Vec<String> = Vec::new();
        let roster = tinyhivemind_core::roster::Roster::new(&members, &people, &retired);
        let desks = desk_snapshots(&record);

        let body = "@product_designer can you take the login screen?";
        let mentions = tinyhivemind_core::mention::resolve(
            body,
            None,
            &tinyhivemind_core::mention::MentionAuthor::Agent {
                id: "software_engineer".to_string(),
            },
            &roster,
            &desks.set(),
        );
        let decision = tinyhivemind_core::referral::referral(
            tinyhivemind_core::referral::ReferralPolicy {
                enabled: true,
                max_hops: 2,
                reach: tinyhivemind_core::referral::ReferralReach::Channels,
                returns: true,
            },
            &referral_input(body, mentions),
            &roster,
            &desks.set(),
        )
        .expect("decides");

        match decision {
            tinyhivemind_core::referral::ReferralDecision::One { referral } => {
                assert_eq!(referral.target_id, "product_designer");
                assert_eq!(referral.from.desk_id, "engineering");
                assert_eq!(
                    referral.to.desk_id, "product_design",
                    "the outsider runs on their OWN desk, not as a guest here"
                );
                assert!(
                    referral.origin.is_some(),
                    "a crossing forward carries the way home"
                );
            }
            other => panic!("expected a referral, got {other:?}"),
        }
    }

    /// **Journey 2** — the asker names a DESK, not a person, and the library
    /// picks that desk's responder.
    ///
    /// This is the one that answers "I can't help with this and I don't know
    /// who can": addressing is at desk granularity, so the engineering agent
    /// needs to know that design exists, not who is on it.
    #[test]
    fn a_desk_mention_selects_that_desks_responder() {
        let record = software_company();
        let members = roster_members(&record);
        let people: Vec<tinyhivemind_core::roster::Person> = Vec::new();
        let retired: Vec<String> = Vec::new();
        let roster = tinyhivemind_core::roster::Roster::new(&members, &people, &retired);
        let desks = desk_snapshots(&record);

        let body = "@#product_design who owns the login screen?";
        let mentions = tinyhivemind_core::mention::resolve(
            body,
            None,
            &tinyhivemind_core::mention::MentionAuthor::Agent {
                id: "software_engineer".to_string(),
            },
            &roster,
            &desks.set(),
        );
        let decision = tinyhivemind_core::referral::referral(
            tinyhivemind_core::referral::ReferralPolicy {
                enabled: true,
                max_hops: 2,
                // `Desks` is what makes an `@#desk` mention selectable at all;
                // under `Channels` it decides nothing.
                reach: tinyhivemind_core::referral::ReferralReach::Desks,
                returns: true,
            },
            &referral_input(body, mentions),
            &roster,
            &desks.set(),
        )
        .expect("decides");

        match decision {
            tinyhivemind_core::referral::ReferralDecision::One { referral } => {
                assert_eq!(
                    referral.to.desk_id, "product_design",
                    "the desk mention chose the desk"
                );
                assert_eq!(
                    referral.target_id, "product_designer",
                    "and the library picked its responder — the asker never named a person"
                );
            }
            other => panic!("expected a desk referral, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// SPIKE: the snapshots the referral / dispatch decisions borrow (P7, P15)
// ---------------------------------------------------------------------------

/// This company's agents as `tinyhivemind` names them.
///
/// The library takes borrowed slices, so a caller owns these for the length of
/// one decision. Retired agents are excluded here rather than filtered later:
/// `referral` answers `TargetInactive` off the roster it is given, and a
/// retired teammate is not somebody work may be handed to.
#[must_use]
pub fn roster_members(record: &CompanyRecord) -> Vec<tinyhivemind_core::roster::RosterMember> {
    record
        .effective_agents()
        .into_iter()
        .filter(|agent| !record.is_retired(&agent.id))
        .map(|agent| tinyhivemind_core::roster::RosterMember {
            id: agent.id,
            name: agent.name,
        })
        .collect()
}

/// The five desk snapshots [`DeskSet`](tinyhivemind_core::desk::DeskSet) borrows,
/// owned together so one decision can borrow them all.
///
/// The split mirrors this host's own overlay model exactly — declared desks from
/// the manifest, operator-added desks, appended members, explicit orderings, and
/// the retired set — which is why this is a mapping and not a merge. Folding
/// them here would be a second answer to the membership question `DeskSet`
/// exists to answer.
pub struct DeskSnapshots {
    declared: Vec<tinyhivemind_core::desk::Desk>,
    added: Vec<tinyhivemind_core::desk::Desk>,
    member_additions: Vec<tinyhivemind_core::desk::DeskMember>,
    orders: Vec<tinyhivemind_core::desk::DeskOrder>,
    retired: Vec<String>,
}

impl DeskSnapshots {
    /// Borrow them as the library's view.
    #[must_use]
    pub fn set(&self) -> tinyhivemind_core::desk::DeskSet<'_> {
        tinyhivemind_core::desk::DeskSet::new(
            &self.declared,
            &self.added,
            &self.member_additions,
            &self.orders,
            &self.retired,
        )
    }
}

/// Project this company's desks into [`DeskSnapshots`].
#[must_use]
pub fn desk_snapshots(record: &CompanyRecord) -> DeskSnapshots {
    use tinyhivemind_core::desk::{Desk, DeskMember, DeskOrder, ResponderMode as HiveMode};
    let mode = |mode: crate::ports::types::ResponderMode| match mode {
        crate::ports::types::ResponderMode::Auto => HiveMode::Auto,
        crate::ports::types::ResponderMode::Lead => HiveMode::Lead,
    };
    DeskSnapshots {
        declared: record
            .manifest
            .group_chats
            .iter()
            .map(|chat| Desk {
                id: chat.id.clone(),
                name: chat.name.clone(),
                description: chat.description.clone(),
                members: chat.members.clone(),
                // A manifest desk has no responder field — `desk_responder_mode`
                // only consults the overlay — so it is lead-routed by
                // definition, which is the library's default too.
                responder_mode: HiveMode::Lead,
            })
            .collect(),
        added: record
            .overlay_desks
            .iter()
            .map(|desk| Desk {
                id: desk.id.clone(),
                name: desk.name.clone(),
                description: desk.description.clone(),
                members: desk.members.clone(),
                responder_mode: mode(desk.responder),
            })
            .collect(),
        member_additions: record
            .overlay_desk_members
            .iter()
            .map(|added| DeskMember {
                desk_id: added.desk_id.clone(),
                agent_id: added.agent_id.clone(),
            })
            .collect(),
        orders: record
            .overlay_desk_order
            .iter()
            .map(|order| DeskOrder {
                desk_id: order.desk_id.clone(),
                ordered: order.ordered.clone(),
            })
            .collect(),
        retired: record.overlay_retired_agents.clone(),
    }
}

// ---------------------------------------------------------------------------
// SPIKE: the host side of a crossing referral (tinyhivemind P15)
// ---------------------------------------------------------------------------

/// This company's [`ReferralQueue`]: the transaction that turns one decided
/// referral into at most one durable child turn.
///
/// # Why this is a transaction and not a spawn
///
/// `referral()` is pure — it decides and does nothing. The trigger is a
/// COMMITTED reply, so anything that reprocesses that reply (a restart, a
/// redelivered frame, a retry) decides the same referral again. Without a
/// durable marker the target desk is asked twice: two turns, two answers, twice
/// the spend, and an operator looking at one question answered twice.
///
/// So the marker is the point, and it is journaled — `ReferralEnqueued`, keyed
/// by `(from_desk, trigger_sequence)`, and `Permanent` because a pruned marker
/// silently reopens the duplicate.
///
/// # What makes it atomic here
///
/// A `tokio::Mutex` held across the check-and-write, plus the host's own
/// single-instance guarantee (the `.lock` in the instance home). That is
/// honest about its scope: it serialises this process, and this process is the
/// only writer of this company's journal. It is NOT a distributed lock, and a
/// second host pointed at the same home would defeat it — which the home lock
/// already refuses.
///
/// # The gap it does not close
///
/// The marker is written BEFORE the child turn is spawned. A crash in that
/// window leaves a marker with no turn — the referral is silently dropped
/// rather than duplicated. That is the safer of the two failures and it is the
/// same exposure `dispatch_task` has between minting a run and spawning its
/// cycle, but it is a gap and it is written down rather than implied.
pub struct JournalReferralQueue {
    runtime: std::sync::Arc<crate::company::runtime::CompanyRuntime>,
    /// Serialises check-then-write. One referral decision at a time.
    gate: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for JournalReferralQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalReferralQueue")
            .finish_non_exhaustive()
    }
}

impl JournalReferralQueue {
    /// Bind the queue to a runtime and the gate it shares with its siblings.
    #[must_use]
    pub fn new(
        runtime: std::sync::Arc<crate::company::runtime::CompanyRuntime>,
        gate: std::sync::Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self { runtime, gate }
    }

    /// Has this exact trigger already created its child turn?
    ///
    /// Backwards from the tail and bounded: a trigger is by construction a
    /// recent sequence, so a marker for it is near the tail or is not there.
    /// `read_before` is the primitive that makes this cheap — the same argument
    /// [`JournalSessionLog`] makes for reading a transcript.
    async fn already(&self, from_desk: &str, trigger: u64) -> bool {
        /// Far enough back to cover a busy company between trigger and replay.
        const LOOKBACK: usize = 2048;
        let Ok(page) = self
            .runtime
            .events()
            .read_before(self.runtime.id(), None, LOOKBACK)
            .await
        else {
            // Fail CLOSED: an unreadable journal cannot prove this is the
            // first attempt, and asking a desk twice is worse than not asking.
            return true;
        };
        page.into_iter().any(|stored| {
            matches!(
                &stored.event,
                CompanyEvent::ReferralEnqueued {
                    from_desk: desk,
                    trigger_sequence,
                    ..
                } if desk == from_desk && *trigger_sequence == trigger
            )
        })
    }

    /// Is this return answering a forward this journal actually recorded?
    ///
    /// The mirror of the marker written for the forward: it went `to.desk_id →
    /// from.desk_id` and named this agent as its target, so a return travelling
    /// the other way is the answer it asked for. Anything else claiming to be a
    /// return has no forward behind it and is refused.
    async fn answering_a_forward(&self, referral: &tinyhivemind::referral::Referral) -> bool {
        const LOOKBACK: usize = 2048;
        let Ok(page) = self
            .runtime
            .events()
            .read_before(self.runtime.id(), None, LOOKBACK)
            .await
        else {
            return false;
        };
        page.into_iter().any(|stored| {
            matches!(
                &stored.event,
                CompanyEvent::ReferralEnqueued { from_desk, to_desk, target, .. }
                    if *from_desk == referral.to.desk_id
                        && *to_desk == referral.from.desk_id
                        && *target == referral.source_id
            )
        })
    }

    /// May `source` cause a turn on `to_desk`?
    ///
    /// Reuses `delegates_to` rather than inventing a second authorization key.
    /// "Which desks may this agent send work to" is ONE fact about an agent,
    /// and expressing it twice would let the tool path and the referral path
    /// disagree about the same question. It fails closed: the shipped companies
    /// all declare `delegates_to = []`.
    ///
    /// This is checked INSIDE the gate, against the record as it stands now —
    /// the decision was made from a snapshot, and a roster can move underneath
    /// it.
    async fn authorized(&self, source: &str, to_desk: &str) -> bool {
        let Ok(Some(record)) = self.runtime.store().load(self.runtime.id()).await else {
            // Fail closed: an unreadable roster authorizes nothing.
            return false;
        };
        record
            .manifest
            .agents
            .iter()
            .find(|agent| agent.id == source)
            .is_some_and(|agent| {
                agent
                    .delegates_to
                    .iter()
                    .any(|allowed| allowed == "*" || allowed == to_desk)
            })
    }
}

impl tinyhivemind::referral::ReferralQueue for JournalReferralQueue {
    fn enqueue_once(
        &self,
        referral: tinyhivemind::referral::Referral,
    ) -> tinyhivemind::referral::ReferralFuture<'_> {
        Box::pin(async move {
            use tinyhivemind::dispatch::{EnqueueOutcome, EnqueueRefusal};

            let _serialised = self.gate.lock().await;

            // 1. Already done? The whole reason this type exists.
            if self
                .already(&referral.from.desk_id, referral.key.trigger_sequence)
                .await
            {
                return Ok(EnqueueOutcome::Already);
            }

            // 2. Revalidate, now, inside the gate. A crossing referral writes
            //    into a channel the author is not a member of, so authorizing
            //    the source desk alone authorizes nothing — the check is on the
            //    source AGENT against the TARGET desk.
            // A RETURN is not a new referral and must not be judged as one.
            //
            // It carries an answer back to a conversation that ASKED for it, so
            // permission was settled when the forward was allowed — requiring
            // the answering agent to hold `delegates_to` for the asking desk
            // refuses every answer unless both desks happen to be able to refer
            // to each other, which is not what either operator agreed to.
            //
            // It is still checked, just against the right fact: a forward from
            // that conversation to this one, addressed to this agent, must
            // actually be on the journal. The marker that makes the forward
            // idempotent is the same marker that authorizes its answer home.
            let permitted = match referral.kind {
                tinyhivemind::referral::ReferralKind::Return => {
                    self.answering_a_forward(&referral).await
                }
                tinyhivemind::referral::ReferralKind::Forward => {
                    self.authorized(&referral.source_id, &referral.to.desk_id)
                        .await
                }
            };
            if !permitted {
                return Ok(EnqueueOutcome::Refused {
                    reason: EnqueueRefusal::Unauthorized,
                });
            }
            let Ok(Some(record)) = self.runtime.store().load(self.runtime.id()).await else {
                return Ok(EnqueueOutcome::Refused {
                    reason: EnqueueRefusal::TargetUnavailable,
                });
            };
            if !record.is_roster_agent(&referral.target_id)
                || record.is_retired(&referral.target_id)
            {
                return Ok(EnqueueOutcome::Refused {
                    reason: EnqueueRefusal::TargetUnavailable,
                });
            }

            // 3. The marker, durably, BEFORE the turn — see the type's doc for
            //    which way this window fails.
            self.runtime
                .events()
                .append(
                    self.runtime.id(),
                    CompanyEvent::ReferralEnqueued {
                        from_desk: referral.from.desk_id.clone(),
                        from_desk_name: desk_label(&record, &referral.from.desk_id),
                        asker: referral.source_id.clone(),
                        asker_label: agent_label(&record, &referral.source_id),
                        // The same discriminant the authorization arm above
                        // switched on, recorded rather than re-derived: the
                        // console needs it to say "Asked by" or "Answered by",
                        // and nothing downstream can tell the two legs apart.
                        returning: matches!(
                            referral.kind,
                            tinyhivemind::referral::ReferralKind::Return
                        ),
                        trigger_sequence: referral.key.trigger_sequence,
                        to_desk: referral.to.desk_id.clone(),
                        target: referral.target_id.clone(),
                    },
                )
                .await
                .map_err(|err| Box::new(err) as tinyhivemind::responder::BoxError)?;

            // 4. The child turn, on the TARGET's conversation.
            self.runtime.clone().spawn_referred_turn(
                referral.to.desk_id.clone(),
                returned_answer(&record, &referral),
                referral.source_id.clone(),
                referral.origin.clone(),
                // The depth THIS child sits at, so the chain it may start is
                // measured from here. Passing a constant made every generation
                // claim the same depth, and a bound that never advances bounds
                // nothing — see `spawn_referred_turn`.
                referral.child_hop,
            );

            Ok(EnqueueOutcome::Enqueued)
        })
    }
}

/// What the asker is handed when an answer comes home.
///
/// # Why the answer is framed rather than passed through
///
/// A returning answer is not a message in the channel — it is dropped from the
/// projection (see `attach_referral_origins`) precisely so the asker's own
/// report is the only thing a reader sees. That makes this text private to the
/// asker, and the only place it can be told what its options are.
///
/// Passed through raw, they were exactly one: the answer arrived as bare prose
/// with no indication of where it came from or what to do with it, and the
/// asker did the obvious thing — summarise it and move on. That is right when
/// the answer lands, and wrong when it misses. "You covered layout, but what
/// about the error state?" was unreachable, not because the machinery could not
/// carry it, but because nothing ever told the asker it could ask.
///
/// So the frame states both endings and the exact spelling that reaches the
/// other desk. Deciding WHICH ending is the asker's judgement and stays the
/// asker's — this only makes the second one expressible.
///
/// A forward is passed through untouched: that content is the asker's own words,
/// and it RENDERS on the desk it lands on. Framing it would put the host's
/// scaffolding in a room where a person is reading.
fn returned_answer(record: &CompanyRecord, referral: &tinyhivemind::referral::Referral) -> String {
    if !matches!(referral.kind, tinyhivemind::referral::ReferralKind::Return) {
        return referral.content.clone();
    }
    let who = agent_label(record, &referral.source_id);
    let desk = desk_label(record, &referral.from.desk_id);
    let back = &referral.from.desk_id;
    format!(
        "{who} on #{desk} answered what you asked them:\n\n{}\n\n         ---\n         This did not appear in your channel — you are the only one who has seen it.          Decide which of these the answer deserves:\n         - It covers the question: report back in your channel, in your own words,          what they said. Do not paste it back verbatim.\n         - Something important is still missing: ask them again. Begin your reply          with @#{back} and put the ONE thing still open in a single short line.\n         Ask again only for a real gap, not for more detail on what they already covered.",
        referral.content
    )
}

/// A desk's operator-facing name, or its id when it has none to show.
fn desk_label(record: &CompanyRecord, desk_id: &str) -> String {
    record
        .manifest
        .group_chats
        .iter()
        .find(|chat| chat.id == desk_id)
        .map(|chat| chat.name.clone())
        .or_else(|| {
            record
                .overlay_desks
                .iter()
                .find(|desk| desk.id == desk_id)
                .map(|desk| desk.name.clone())
        })
        .unwrap_or_else(|| desk_id.to_string())
}

/// An agent's display name, or its id when it has none.
fn agent_label(record: &CompanyRecord, agent_id: &str) -> String {
    record
        .effective_agents()
        .into_iter()
        .find(|agent| agent.id == agent_id)
        .and_then(|agent| agent.name)
        .unwrap_or_else(|| agent_id.to_string())
}
