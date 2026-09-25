//! Tests for the company journal read as a `tinyhivemind` session log.

use std::sync::Arc;

use tinyhivemind::{
    SESSION_WINDOW, Sequence, SessionAuthor, SessionLog, SessionQuery, project_session,
};

use super::*;
use crate::hive::referral::HIVE_REFERRAL_AUTHOR;
use crate::hive::test_support::{MemoryLog, agent_reply, agent_reply_in, operator_message};
use crate::ports::events::EventLog;

fn adapter(log: &Arc<MemoryLog>) -> EventLogSessionLog {
    seated(log, Vec::new())
}

/// The adapter for a desk that seats `seats`, whose pair channels are part
/// of its transcript.
fn seated(log: &Arc<MemoryLog>, seats: Vec<String>) -> EventLogSessionLog {
    EventLogSessionLog::new(
        Arc::clone(log) as Arc<dyn EventLog>,
        MemoryLog::company(),
        "eng".into(),
        "Engineering".into(),
        seats,
    )
}

#[tokio::test]
async fn the_log_adapter_attributes_and_pages_desk_rows() {
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    log.append(&company, operator_message("sales", "not this desk", None))
        .await
        .unwrap();
    // Addressed by display name, in the wrong case: the adapter canonicalises
    // it, so the row still lands in the room it was meant for.
    let trigger = log
        .append(
            &company,
            operator_message("ENGINEERING", "Decide the rollout.", None),
        )
        .await
        .unwrap();
    log.append(&company, agent_reply("eng", "planner", "Stage it."))
        .await
        .unwrap();
    log.append(
        &company,
        agent_reply(
            "eng",
            HIVE_REFERRAL_AUTHOR,
            "@writer on #Content answered: yes",
        ),
    )
    .await
    .unwrap();
    log.append(
        &company,
        agent_reply_in("eng", "planner", "quietly", vec!["reviewer".into()], None),
    )
    .await
    .unwrap();

    let adapter = adapter(&log);
    assert_eq!(adapter.desk_id(), "eng");
    assert_eq!(adapter.conversation(None).desk_name, "Engineering");
    let projected = project_session(
        &adapter,
        &SessionQuery {
            conversation: adapter.conversation(None),
            viewer: tinyhivemind::aside::Viewer::Operator,
            before: None,
            window: SESSION_WINDOW,
        },
    )
    .await
    .expect("the page contract holds");

    let authors: Vec<&SessionAuthor> = projected.iter().map(|m| &m.author).collect();
    assert_eq!(projected.len(), 4, "only this desk's chat: {projected:?}");
    assert!(matches!(authors[0], SessionAuthor::Operator));
    assert!(matches!(authors[1], SessionAuthor::Agent { id, .. } if id == "planner"));
    // An answer carried home is a system line, never a teammate.
    assert!(
        matches!(authors[2], SessionAuthor::System { kind, .. } if kind == HIVE_REFERRAL_AUTHOR)
    );
    assert_eq!(projected[0].sequence, Sequence(trigger.value()));
    assert!(matches!(
        projected[3].audience,
        Audience::Aside { ref members } if members == &["reviewer".to_string()]
    ));

    // The port's own paging contract: newest-first, `before` exclusive, and a
    // page no larger than asked for.
    let page = SessionLog::read_before(&adapter, None, 2)
        .await
        .expect("a bounded page");
    assert_eq!(page.messages.len(), 2);
    assert!(page.messages[0].sequence > page.messages[1].sequence);
    let cursor = page.next_before.expect("more rows remain");
    let older = SessionLog::read_before(&adapter, Some(cursor), 8)
        .await
        .expect("the older page");
    assert!(
        older.messages.iter().all(|m| m.sequence < cursor),
        "`before` is exclusive: {older:?}"
    );
    assert!(older.next_before.is_none(), "the journal ended: {older:?}");
}

#[tokio::test]
async fn a_desk_buried_behind_unrelated_rows_still_pages_through() {
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    log.append(&company, operator_message("eng", "first", None))
        .await
        .unwrap();
    for _ in 0..600 {
        log.append(&company, operator_message("sales", "noise", None))
            .await
            .unwrap();
    }
    let adapter = adapter(&log);
    let page = SessionLog::read_before(&adapter, None, 4)
        .await
        .expect("keeps reading past a chat-less chunk");
    assert_eq!(page.messages.len(), 1);
    assert!(page.next_before.is_none());
}

// ---------------------------------------------------------------------------
// A desk's private conversations are part of its transcript
// ---------------------------------------------------------------------------

/// A conversation two seats open is written to their own pair channel so the
/// room's timeline stays the room's. It is still this desk's transcript: the
/// asker cannot finish until it is answered, and the askee is turned inside
/// it, so the log has to admit it.
#[tokio::test]
async fn a_pair_channel_of_this_desks_seats_is_admitted() {
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    log.append(
        &company,
        agent_reply_in(
            &crate::hive::referral::pair_conversation("ada", "grace"),
            "ada",
            "between us: does the rollout need a freeze?",
            vec!["grace".to_string()],
            None,
        ),
    )
    .await
    .unwrap();

    let page = seated(&log, vec!["ada".into(), "grace".into()])
        .read_before(None, SESSION_WINDOW)
        .await
        .expect("the log reads");

    assert_eq!(page.messages.len(), 1, "{:?}", page.messages);
    assert_eq!(
        page.messages[0].chat_id.as_deref(),
        Some("eng"),
        "reported under the desk id, like every other admitted row"
    );
}

/// The failure the widening could introduce. A pair channel is minted from
/// two roster ids and says nothing about where those two were talking, so
/// admitting one on the strength of its name alone would pull another desk's
/// private exchange into this transcript.
#[tokio::test]
async fn a_pair_channel_of_seats_elsewhere_is_not() {
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    log.append(
        &company,
        agent_reply_in(
            &crate::hive::referral::pair_conversation("ada", "linus"),
            "ada",
            "a conversation on another desk",
            vec!["linus".to_string()],
            None,
        ),
    )
    .await
    .unwrap();

    // `ada` sits here; `linus` does not. One of the two is not enough.
    let page = seated(&log, vec!["ada".into(), "grace".into()])
        .read_before(None, SESSION_WINDOW)
        .await
        .expect("the log reads");

    assert!(page.messages.is_empty(), "{:?}", page.messages);
}

/// A desk with no seats declared admits no pair channel at all, which is what
/// every caller that does not seat a room passes.
#[tokio::test]
async fn a_desk_that_seats_nobody_admits_no_pair_channel() {
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    log.append(
        &company,
        agent_reply_in(
            &crate::hive::referral::pair_conversation("ada", "grace"),
            "ada",
            "between us",
            vec!["grace".to_string()],
            None,
        ),
    )
    .await
    .unwrap();

    let page = adapter(&log)
        .read_before(None, SESSION_WINDOW)
        .await
        .expect("the log reads");

    assert!(page.messages.is_empty(), "{:?}", page.messages);
}

/// The channel key is not a conversation unless it parses as one. A desk
/// whose own id happens to start with the pair prefix must not be widened by
/// accident.
#[tokio::test]
async fn a_channel_that_is_not_a_pair_key_is_unaffected() {
    assert_eq!(
        crate::hive::referral::pair_seats("dm:ada+grace"),
        Some(("ada", "grace"))
    );
    assert_eq!(crate::hive::referral::pair_seats("eng"), None);
    assert_eq!(crate::hive::referral::pair_seats("dm:ada"), None);
}

/// **A third seat at this desk does not read the pair's exchange.**
///
/// The widening admits a pair channel of two seats that sit here, and the
/// desk has more than two. Every row a conversation writes carries the ask
/// as its parent, so a desk turn -- which asks for the desk's own thread --
/// skips them on thread root alone.
///
/// The conclusion is the exception, and the reason this is pinned: the
/// conductor mints it with no thread, so it is the one row of an exchange
/// that a desk-thread query can reach. Only its audience keeps it private.
/// Narrow the audience wrongly, or drop it, and one seat's private answer is
/// read by everyone at the desk on their next turn.
#[tokio::test]
async fn a_third_seat_at_the_desk_cannot_read_a_pair_of_others() {
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    let pair = crate::hive::referral::pair_conversation("ada", "grace");
    // The conclusion: minted with no thread, so a desk-thread query reaches
    // it, and private to the asker. Every other row of an exchange carries
    // the ask as its parent and is skipped on thread root alone -- this is
    // the one whose privacy rests on audience.
    log.append(
        &company,
        agent_reply_in(
            &pair,
            "grace",
            "concluded our conversation: no, ship it",
            vec!["ada".to_string()],
            None,
        ),
    )
    .await
    .unwrap();

    let adapter = seated(&log, vec!["ada".into(), "grace".into(), "linus".into()]);
    let seen = project_session(
        &adapter,
        &SessionQuery {
            conversation: adapter.conversation(None),
            viewer: tinyhivemind::aside::Viewer::Agent {
                id: "linus".to_string(),
            },
            before: None,
            window: SESSION_WINDOW,
        },
    )
    .await
    .expect("the page contract holds");

    // Not absent -- elided. The seat learns an aside happened and to whom,
    // which is what keeps the transcript honest about a gap, and reads none
    // of it. Absence would be a lie of a different kind; content would be
    // the leak.
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].content.is_empty(),
        "content reached a third seat: {seen:?}"
    );
    assert!(
        seen[0].elided.is_some(),
        "the gap is not even marked: {seen:?}"
    );
}

/// **Another desk's rows are readable, under their own id.**
///
/// A seat that sits at two desks is shown the newest rows of the other as
/// context, and the library reads them through this log. Admitting only this
/// desk answers nothing, so the seat is told it is in nothing else.
///
/// Reported under the other desk's id, not folded into this one: the caller
/// asks per conversation, and folding them in would make another room's rows
/// read as this one's.
#[tokio::test]
async fn a_desk_this_log_also_reads_keeps_its_own_id() {
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    log.append(
        &company,
        agent_reply("content", "ceo", "shipping the brief"),
    )
    .await
    .unwrap();

    let mut adapter = seated(&log, vec!["ceo".into()]);
    assert!(
        adapter
            .read_before(None, SESSION_WINDOW)
            .await
            .expect("reads")
            .messages
            .is_empty(),
        "another desk is not this desk's transcript by default"
    );

    adapter.also_read(vec![("content".into(), "Content".into())]);
    let page = adapter
        .read_before(None, SESSION_WINDOW)
        .await
        .expect("the log reads");
    assert_eq!(page.messages.len(), 1, "{:?}", page.messages);
    assert_eq!(
        page.messages[0].chat_id.as_deref(),
        Some("content"),
        "reported under its own id, not folded into this desk"
    );
}

/// One row an episode wrote, as the host journals it: a reply with its parent
/// and the kind of utterance it committed.
fn episode_row(
    chat: &str,
    who: &str,
    text: &str,
    audience: Vec<String>,
    parent: u64,
    kind: crate::ports::types::UtteranceKind,
) -> crate::ports::types::CompanyEvent {
    let mut event = agent_reply_in(chat, who, text, audience, None);
    if let crate::ports::types::CompanyEvent::AgentReply {
        parent: stored,
        episode,
        ..
    } = &mut event
    {
        *stored = Some(crate::ports::types::EventSeq::new(parent));
        *episode = Some(crate::ports::types::ReplyEpisode {
            id: "ep-1".into(),
            revision: 1,
            kind,
            to: Vec::new(),
            routed_by: None,
        });
    }
    event
}

/// The sequences of the rows `viewer` may read in `conversation`, and of the
/// rows it is shown elided.
async fn read_as(
    adapter: &EventLogSessionLog,
    viewer: &str,
    thread_root: Option<Sequence>,
) -> (Vec<u64>, Vec<u64>) {
    let seen = project_session(
        adapter,
        &SessionQuery {
            conversation: adapter.conversation(thread_root),
            viewer: tinyhivemind::aside::Viewer::Agent {
                id: viewer.to_string(),
            },
            before: None,
            window: SESSION_WINDOW,
        },
    )
    .await
    .expect("the page contract holds");
    let readable = seen
        .iter()
        .filter(|row| row.readable().is_some())
        .map(|row| row.sequence.0)
        .collect();
    let elided = seen
        .iter()
        .filter(|row| row.elided.is_some())
        .map(|row| row.sequence.0)
        .collect();
    (readable, elided)
}

/// **An episode's desk rows are roots, and a conversation is its own thread.**
///
/// The five rows of a live episode (journal 188..208): the operator's ask,
/// one seat asking another inside it, the answer, the conclusion the host
/// threads under the ask, and the asker's completion on the desk. The host
/// threads every desk row under the operator's message, so read by the
/// library's channel-level rule -- each root and its first reply -- the
/// episode was the operator's ask and the seat's ask, and the answer, the
/// conclusion and the completion reached nobody, the asker included. That
/// projection was measured, not argued, before this was written.
///
/// Reported the way the library models conversations -- desk rows as roots,
/// the ask as the root of its thread -- the two seats read the exchange in
/// the desk's order, and a third seat reads the desk and a stub where the
/// aside happened, never the answer: the pair channel is private, and the
/// adapter says so on every row in it.
#[tokio::test]
async fn an_episodes_desk_rows_are_roots_and_a_conversation_is_its_own_thread() {
    use crate::ports::types::UtteranceKind;
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    let pair = crate::hive::referral::pair_conversation("planner", "strategist");
    // 1: the operator opens the episode; every desk row threads under it.
    log.append(
        &company,
        operator_message("eng", "Agree one rollout start date.", None),
    )
    .await
    .unwrap();
    // 2: the strategist asks the planner, aside, under the operator's root.
    log.append(
        &company,
        episode_row(
            &pair,
            "strategist",
            "what constraints are on file?",
            vec!["planner".into()],
            1,
            UtteranceKind::Ask,
        ),
    )
    .await
    .unwrap();
    // 3: the answer, inside the conversation, with no audience of its own.
    log.append(
        &company,
        episode_row(
            &pair,
            "planner",
            "none on file for either client",
            Vec::new(),
            2,
            UtteranceKind::CompleteEpisode,
        ),
    )
    .await
    .unwrap();
    // 4: the conclusion, threaded under the ask by `DeskHost::commit`.
    log.append(
        &company,
        episode_row(
            &pair,
            "planner",
            "concluded our conversation: none on file",
            vec!["strategist".into()],
            2,
            UtteranceKind::Dm,
        ),
    )
    .await
    .unwrap();
    // 5: the asker completes on the desk, under the operator's root.
    log.append(
        &company,
        episode_row(
            "eng",
            "strategist",
            "no date can be settled; confirmed with the planner",
            Vec::new(),
            1,
            UtteranceKind::CompleteEpisode,
        ),
    )
    .await
    .unwrap();
    let adapter = seated(
        &log,
        vec!["strategist".into(), "planner".into(), "researcher".into()],
    );

    // The two seats in the conversation read it in the desk's order: the
    // answer is the ask's first reply, and the completion is a root.
    for seat in ["strategist", "planner"] {
        let (readable, _) = read_as(&adapter, seat, None).await;
        // Row 4 is the conclusion: confided to this pair, so it is promoted
        // into their desk read like every other row of the thread.
        assert_eq!(readable, vec![1, 2, 3, 4, 5], "{seat}'s desk");
    }
    // A third seat reads the desk, and of the exchange only that it happened.
    let (readable, elided) = read_as(&adapter, "researcher", None).await;
    assert_eq!(readable, vec![1, 5], "the researcher's desk");
    assert!(!elided.is_empty(), "the aside is a stub, not absent");

    // The conversation's own thread holds the exchange whole -- the ask, the
    // answer, and the conclusion that closes it -- and nothing of it for a
    // third seat.
    let (readable, _) = read_as(&adapter, "strategist", Some(Sequence(2))).await;
    assert_eq!(readable, vec![2, 3, 4], "the asker's thread");
    let (readable, _) = read_as(&adapter, "researcher", Some(Sequence(2))).await;
    assert!(
        readable.is_empty(),
        "a third seat read the pair's thread: {readable:?}"
    );
}

/// **A conclusion reaches the asker, because a forced one has to.**
///
/// This adapter used to withhold every conclusion from the projection. That
/// was right while the row restated the askee's last line verbatim -- a live
/// run measured that paragraph three times in one prompt -- but `tinyhivemind`
/// 8a672764 took the restatement out at the source. What is left is a marker,
/// and on a forced close it is the only thing that says the answer never
/// came: the rows show an exchange that simply stops, `awaiting` clears, and
/// nothing else tells the seat why. Withholding it now would remove the one
/// message worth sending.
#[tokio::test]
async fn a_forced_conclusion_reaches_the_asker() {
    use crate::ports::types::UtteranceKind;
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    let pair = crate::hive::referral::pair_conversation("planner", "strategist");
    log.append(
        &company,
        operator_message("eng", "Agree one rollout start date.", None),
    )
    .await
    .unwrap();
    log.append(
        &company,
        episode_row(
            &pair,
            "strategist",
            "what constraints are on file?",
            vec!["planner".into()],
            1,
            UtteranceKind::Ask,
        ),
    )
    .await
    .unwrap();
    // No answer: the conversation ran past its wall and the conductor closed
    // it. The only row that says so is the conclusion.
    log.append(
        &company,
        episode_row(
            &pair,
            "planner",
            "concluded our conversation (thread 2): the conversation did not conclude in time; \
             take what was said and proceed",
            vec!["strategist".into()],
            2,
            UtteranceKind::Dm,
        ),
    )
    .await
    .unwrap();
    let adapter = seated(
        &log,
        vec!["strategist".into(), "planner".into(), "researcher".into()],
    );

    // The asker reads it -- on the desk, where the thread is confided to it,
    // and in the thread itself.
    let (desk, _) = read_as(&adapter, "strategist", None).await;
    assert!(
        desk.contains(&3),
        "the asker never learned the close: {desk:?}"
    );
    let (thread, _) = read_as(&adapter, "strategist", Some(Sequence(2))).await;
    assert_eq!(thread, vec![2, 3], "the exchange, ending in why it ended");

    // And a seat outside the pair reads none of it, as before.
    let (readable, elided) = read_as(&adapter, "researcher", None).await;
    assert_eq!(readable, vec![1], "the researcher reads only the desk");
    assert!(!elided.is_empty(), "the aside is a stub, not absent");
}
/// **A party reads the whole of its own conversation on its desk turn.**
///
/// The library's channel-level rule is each root and its *first* reply, so a
/// conversation with more than one exchange left everything past the first
/// answer out of a seat's desk read -- and for a hosted seat, which clears
/// its session and rebuilds from this log every turn, out of its memory
/// entirely. `tinyhivemind` cd23c7fe lifts that for a seat the thread was
/// confided to.
///
/// Confided means [`Audience::Aside`] admitting that seat, which is why this
/// belongs here rather than upstream: an answer inside a conversation is a
/// `complete_episode` and names nobody, so what makes it an aside at all is
/// this adapter (`audience_of`) -- a pair channel is private to the two
/// seats it names whether or not its author wrote that down. Report those
/// rows as desk-visible and the exception stops applying to the one kind of
/// row it exists for.
#[tokio::test]
async fn a_party_reads_every_reply_of_its_own_conversation() {
    use crate::ports::types::UtteranceKind;
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    let pair = crate::hive::referral::pair_conversation("planner", "strategist");
    log.append(&company, operator_message("eng", "Settle the date.", None))
        .await
        .unwrap();
    log.append(
        &company,
        episode_row(
            &pair,
            "strategist",
            "what constraints?",
            vec!["planner".into()],
            1,
            UtteranceKind::Ask,
        ),
    )
    .await
    .unwrap();
    // Three replies under the one ask: today only the first would be read.
    for (who, text) in [
        ("planner", "none on file"),
        ("strategist", "then we are blocked"),
        ("planner", "agreed, blocked"),
    ] {
        log.append(
            &company,
            episode_row(&pair, who, text, vec![], 2, UtteranceKind::CompleteEpisode),
        )
        .await
        .unwrap();
    }
    let adapter = seated(
        &log,
        vec!["strategist".into(), "planner".into(), "researcher".into()],
    );

    for seat in ["strategist", "planner"] {
        let (readable, _) = read_as(&adapter, seat, None).await;
        assert_eq!(
            readable,
            vec![1, 2, 3, 4, 5],
            "{seat} reads its whole exchange"
        );
    }
    // And a seat it was not confided to reads none of it, still.
    let (readable, elided) = read_as(&adapter, "researcher", None).await;
    assert_eq!(readable, vec![1], "the researcher reads only the desk");
    assert!(!elided.is_empty(), "the aside is a stub, not absent");
}

/// An operator message that says nothing is omitted from the port's own
/// page, the same as an agent reply that says nothing — the row's own doc
/// says "or says nothing" without carving out which author it applies to.
///
/// Read through `SessionLog::read_before` directly rather than through
/// `project_session`: the library's own projection already collapses
/// blank-content rows for every caller, so a regression in `row()` itself
/// would be invisible behind that second filter.
#[tokio::test]
async fn a_blank_operator_message_is_omitted_like_a_blank_agent_reply() {
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    log.append(&company, operator_message("eng", "   ", None))
        .await
        .unwrap();
    let real = log
        .append(&company, operator_message("eng", "Ship it.", None))
        .await
        .unwrap();
    log.append(&company, agent_reply("eng", "planner", "   "))
        .await
        .unwrap();

    let adapter = adapter(&log);
    let page = SessionLog::read_before(&adapter, None, 8)
        .await
        .expect("a page");

    assert_eq!(
        page.messages.len(),
        1,
        "both blank rows are omitted: {:?}",
        page.messages
    );
    assert_eq!(page.messages[0].sequence, Sequence(real.value()));
}

/// A running episode's rows stay out of a rival episode on the same desk.
///
/// Episodes are keyed `(desk, thread_root)`, so a threaded one and a channel
/// one coexist on a desk; narrowing the fold by desk alone would let the
/// second read the first's in-flight rows, including a request it had parked
/// and never made itself.
///
/// A **completed** episode is different and must still be read: a desk is a
/// room, not a series of meetings, and scoping by episode identity would make
/// every episode start amnesiac.
#[tokio::test]
async fn an_unfinished_rival_episode_is_withheld_but_a_settled_one_is_not() {
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    log.append(&company, operator_message("eng", "The question.", None))
        .await
        .unwrap();
    let in_episode = |text: &str, id: &str| {
        agent_reply_in(
            "eng",
            "planner",
            text,
            Vec::new(),
            Some(crate::ports::types::ReplyEpisode {
                id: id.to_owned(),
                revision: 0,
                kind: UtteranceKind::Post,
                to: Vec::new(),
                routed_by: None,
            }),
        )
    };
    log.append(&company, in_episode("settled work", "done-ep"))
        .await
        .unwrap();
    log.append(
        &company,
        CompanyEvent::EpisodeCompleted {
            chat_id: "eng".into(),
            episode_id: "done-ep".into(),
            revision: 1,
            completed_by: None,
            rounds: 1,
            reason: crate::ports::types::EpisodeReason::CompleteEpisode,
            summary_seq: None,
        },
    )
    .await
    .unwrap();
    log.append(&company, in_episode("mid-flight request", "live-ep"))
        .await
        .unwrap();

    let mut adapter = seated(&log, vec!["planner".into()]);
    adapter.set_episode("mine-ep");
    let rows: Vec<String> = project_session(
        &adapter,
        &SessionQuery {
            conversation: adapter.conversation(None),
            viewer: tinyhivemind::aside::Viewer::Agent {
                id: "planner".into(),
            },
            before: None,
            window: SESSION_WINDOW,
        },
    )
    .await
    .expect("projects")
    .iter()
    .map(|message| message.content.clone())
    .collect();

    assert!(
        !rows.iter().any(|row| row.contains("mid-flight request")),
        "a rival episode still running does not bleed into this one: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("settled work")),
        "a completed episode is desk history and still reads: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("The question")),
        "and the operator's own words are never withheld: {rows:?}"
    );
}

/// A seat carries its own operator line into the room; nobody else does.
///
/// The teammate the operator told something privately should not be amnesiac
/// about it the moment it sits at a desk. But a DM is *bound* as a hive of
/// the whole roster — `resolve_dm` refuses a non-member, so `ask` needs
/// everyone present — which makes membership useless as an admission rule:
/// every teammate is a member of every DM. Ownership is the only reading that
/// admits one line to one seat, and the aside is what enforces it.
#[tokio::test]
async fn a_seat_reads_its_own_operator_dm_and_no_one_elses() {
    let log = Arc::new(MemoryLog::default());
    let company = MemoryLog::company();
    log.append(&company, operator_message("eng", "Desk business.", None))
        .await
        .unwrap();
    log.append(
        &company,
        operator_message("dm:planner", "Privately: deprioritise passkeys.", None),
    )
    .await
    .unwrap();
    // A DM neither seat owns stays out entirely, membership notwithstanding.
    log.append(
        &company,
        operator_message("dm:outsider", "Not for this desk.", None),
    )
    .await
    .unwrap();

    let adapter = seated(&log, vec!["planner".into(), "reviewer".into()]);
    let read = |seat: &'static str| {
        let adapter = &adapter;
        async move {
            project_session(
                adapter,
                &SessionQuery {
                    conversation: adapter.conversation(None),
                    viewer: tinyhivemind::aside::Viewer::Agent { id: seat.into() },
                    before: None,
                    window: SESSION_WINDOW,
                },
            )
            .await
            .expect("projects")
            .iter()
            .map(|message| message.content.clone())
            .collect::<Vec<_>>()
        }
    };

    let owner = read("planner").await;
    assert!(
        owner
            .iter()
            .any(|row| row.contains("deprioritise passkeys")),
        "the teammate remembers what the operator told it privately: {owner:?}"
    );
    assert!(
        owner.iter().any(|row| row.contains("Desk business")),
        "and still reads the room: {owner:?}"
    );

    let other = read("reviewer").await;
    assert!(
        !other
            .iter()
            .any(|row| row.contains("deprioritise passkeys")),
        "a teammate at the same desk does not read someone else's private line: {other:?}"
    );
    assert!(
        other.iter().any(|row| row.contains("Desk business")),
        "the room itself is unchanged for them: {other:?}"
    );

    for seat in [owner, read("reviewer").await] {
        assert!(
            !seat.iter().any(|row| row.contains("Not for this desk")),
            "a DM belonging to nobody here is not admitted at all: {seat:?}"
        );
    }
}
