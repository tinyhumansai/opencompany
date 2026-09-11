//! One agent, one session.
//!
//! # What changed, and why
//!
//! Before this module, an agent had no continuous existence. One
//! [`oh::agent::Agent`] is held per `(company, agent_id)`, but
//! [`CompanyAgent::run_with_steer`](super::CompanyAgent::run_with_steer)
//! called `clear_history()` every time the incoming chat differed from the
//! bound one and re-seeded a window of *that desk's* transcript. So an agent
//! that answered you in a DM and then answered on `#general` was, from its own
//! point of view, two entities that happened to share a name: it could not
//! notice that the question just asked in the DM is the one it answered on the
//! desk an hour ago, and it could not hold a thought between rooms.
//!
//! This module is the replacement. The session is never cleared. Each turn the
//! agent is handed the rows it has **not yet seen**, across **every** channel
//! it can read, each stamped with where it came from.
//!
//! # The watermark, not a mailbox
//!
//! There is no queue and no per-agent inbox. `tinyhivemind` refuses to have one
//! — `NoDispatchReason::SelfMention`, `NoReferralReason::SelfMention` and
//! `UtteranceRejection::SelfRecipient` all decline self-addressing by name —
//! because the architecture is stigmergic: work leaves an attributed trace in a
//! shared, globally sequenced log, and the trace is the stimulus for the next
//! turn. Continuity is a **watermark the host owns**.
//!
//! [`AgentSessionState`] is that watermark, modelled on
//! `tinyhivemind::sharing::SharingState` **minus its `conversation` field**.
//! That field is what makes the vendored type return
//! `ReinitializeReason::ConversationChanged` on a channel switch — the exact
//! behaviour this module exists to remove — so the shape is reused and the key
//! is not.
//!
//! # Channel isolation is dropped; audience isolation is not
//!
//! Merging the channels is deliberate, and it gives up the isolation
//! `#1725` / `#1730` / `#1890` installed. What replaces it is the **cue**: every
//! delivered row is prefixed with the channel it was said on, per line, through
//! the same [`prefix_every_line`](super::chat_seed) machinery that already
//! defends attribution against forgery.
//!
//! What is **not** given up is who may read what. An aside this agent is not
//! party to still never reaches it: [`Audience::admits`] is applied here with
//! `Viewer::Agent`, exactly as `EpisodeDriver` applies it. Privacy between
//! agents is a deliberation device and never a security boundary — an operator
//! reads everything — but a peer agent's narrowing is real and stays.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::ports::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, CompanyRecord, EventSeq};
use crate::server::chat_history::{self, Channel, agent_channels};

/// How many raw journal events one delta walk may read before giving up and
/// asking for a full re-seed.
///
/// Mirrors `tinyhivemind::session::SCAN_LIMIT`. The walk counts **raw** rows,
/// not delivered ones, so an agent on one quiet desk in a busy company crosses
/// this sooner than its own traffic suggests — which is why crossing it is a
/// re-seed rather than an error.
pub const SESSION_SCAN_LIMIT: usize = 2048;

/// The most rows one delta hands over.
///
/// A turn that would be handed more than this has been away long enough that
/// the recent window is a better context than a partial replay, so it re-seeds
/// instead. Mirrors `tinyhivemind::session::SESSION_WINDOW` at desk scale,
/// widened because this window spans every channel rather than one.
pub const SESSION_DELTA_LIMIT: usize = 60;

/// How many events are read per journal page during the walk.
const EVENT_PAGE: usize = 256;

/// What this agent has already accepted, company-wide.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentSessionState {
    /// Exclusive lower boundary: every row at or below this has been handed
    /// over. `None` before the first delivery, which is the cold-boot case.
    pub watermark: Option<EventSeq>,
    /// Rows above the watermark already accepted through another path — the
    /// agent's own reply to the turn in flight, chiefly. Bounded by
    /// [`SESSION_DELTA_LIMIT`]; a state that would exceed it advances the
    /// watermark instead.
    pub present_above_watermark: BTreeSet<EventSeq>,
}

impl AgentSessionState {
    /// Whether `seq` has already been handed to this agent.
    fn already_seen(&self, seq: EventSeq) -> bool {
        match self.watermark {
            Some(mark) if seq <= mark => true,
            _ => self.present_above_watermark.contains(&seq),
        }
    }

    /// State after a re-seed whose recent-window context covered only the
    /// turn's own channel (`ChatSeedRequest::build` is given `incoming` and
    /// nothing else).
    ///
    /// The watermark is deliberately **not** reset to the turn's own
    /// sequence: this agent's session is company-wide, so overwriting it here
    /// would mark every row below it "already delivered" — including an
    /// older, still-unseen message on a channel the reseed never looked at
    /// (Codex P1: a greeting on desk A permanently hides an earlier unseen
    /// message on desk B). `self`'s prior watermark survives untouched, and
    /// only the turn's own message is [`accept`](Self::accept)ed, through the
    /// same compaction a delta uses — which is also what lets a true cold
    /// start (no prior watermark at all) still come out with one afterward,
    /// so the next turn can walk a delta instead of re-seeding forever.
    pub(super) fn reseeded(&self, current_message: Option<EventSeq>) -> Self {
        let mut next = Self {
            watermark: self.watermark,
            present_above_watermark: BTreeSet::new(),
        };
        if let Some(seq) = current_message {
            next.accept(seq);
        }
        next
    }

    /// Records `seq` as delivered, compacting the present set into the
    /// watermark whenever it can.
    fn accept(&mut self, seq: EventSeq) {
        self.present_above_watermark.insert(seq);
        // Compaction: while the row directly above the watermark is present,
        // the watermark can swallow it. Keeps the set small in the ordinary
        // case, where a delta is a contiguous run.
        loop {
            let next = match self.watermark {
                Some(mark) => EventSeq::new(mark.value() + 1),
                None => match self.present_above_watermark.iter().next().copied() {
                    Some(first) => first,
                    None => return,
                },
            };
            if self.present_above_watermark.remove(&next) {
                self.watermark = Some(next);
            } else {
                return;
            }
        }
    }
}

/// One row on its way into the session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Envelope {
    /// Where in the journal it sits.
    pub seq: EventSeq,
    /// The channel it was said on, as the cue names it.
    pub channel: String,
    /// Who said it.
    pub author: String,
    /// Whether this agent is the author — its own lines are not cued as input.
    pub mine: bool,
    /// What was said.
    pub text: String,
}

/// What a turn should do about the rows it has not seen.
#[derive(Clone, Debug)]
pub enum SessionPlan {
    /// Hand these over, then commit `next_state` once the turn has accepted
    /// them. The ordering is the contract: committing first would let a failed
    /// turn leave the session believing it read something it never saw.
    Delta {
        envelopes: Vec<Envelope>,
        next_state: AgentSessionState,
    },
    /// The walk could not reach the watermark inside its bounds, or the agent
    /// has no session yet. Fall back to the recent-window seed.
    Reinitialize { reason: ReinitializeReason },
}

/// Why a delta could not be prepared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReinitializeReason {
    /// No watermark yet — this is the agent's first turn in this process.
    ColdStart,
    /// The watermark was not crossed inside [`SESSION_SCAN_LIMIT`] raw rows.
    GapTooLarge,
    /// More unseen rows than [`SESSION_DELTA_LIMIT`]; the recent window is the
    /// better context.
    TooManyUnseen,
}

/// The rows this agent may read and has not yet been handed, oldest first.
///
/// `before` is exclusive and is this turn's own message: the turn appends that
/// itself, so seeding it here would duplicate it on the wire.
pub async fn prepare_delta(
    events: &Arc<dyn EventLog>,
    company: &CompanyId,
    record: &CompanyRecord,
    agent_id: &str,
    state: &AgentSessionState,
    before: Option<EventSeq>,
) -> SessionPlan {
    let Some(watermark) = state.watermark else {
        return SessionPlan::Reinitialize {
            reason: ReinitializeReason::ColdStart,
        };
    };

    let channels = agent_channels(record, agent_id);
    let mut newest_first: Vec<Envelope> = Vec::new();
    // Rows above the watermark this agent will never be handed — see the
    // comment where these are pushed, below.
    let mut skip_seen: Vec<EventSeq> = Vec::new();
    let mut scanned: usize = 0;
    let mut cursor = before;
    let mut crossed = false;

    loop {
        if scanned >= SESSION_SCAN_LIMIT {
            break;
        }
        let page = match events.read_before(company, cursor, EVENT_PAGE).await {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(
                    company = %company,
                    agent = agent_id,
                    %error,
                    "[agent-session] journal read failed; re-seeding instead of continuing"
                );
                return SessionPlan::Reinitialize {
                    reason: ReinitializeReason::GapTooLarge,
                };
            }
        };
        if page.is_empty() {
            // The log ended before the watermark. Nothing older can arrive, so
            // everything above it has been collected — treat that as crossed
            // rather than as a gap.
            crossed = true;
            break;
        }
        scanned += page.len();
        cursor = page.last().map(|event| event.seq);

        for stored in page {
            if stored.seq <= watermark {
                crossed = true;
                break;
            }
            if state.already_seen(stored.seq) {
                continue;
            }
            // Codex P1: a row above the watermark that this agent will never
            // be delivered — wrong channel, `Audience::admits` refuses it, not
            // a conversational event, or blank — still has to be marked
            // consumed. `accept` only compacts the watermark through
            // CONSECUTIVE sequences, so leaving a structurally-undeliverable
            // seq out of `next_state` pins the watermark at the row below it
            // forever: every later turn rescans from there, and once the scan
            // exceeds `SESSION_SCAN_LIMIT` it reinitializes instead of
            // completing, permanently falling back to a chat-only reseed.
            // These are safe to accept unconditionally, unlike the reseed's
            // watermark (see `AgentSessionState::reseeded`): that case skipped
            // a row that WOULD eventually be delivered on another channel;
            // this one is filtered out for this agent by construction and can
            // never become deliverable later.
            let Some(channel) = channel_for(&channels, &stored.event, record) else {
                skip_seen.push(stored.seq);
                continue;
            };
            if !readable_by(agent_id, &stored.event) {
                skip_seen.push(stored.seq);
                continue;
            }
            let Some((author, mine, text)) = body_of(agent_id, &stored.event) else {
                skip_seen.push(stored.seq);
                continue;
            };
            if text.trim().is_empty() {
                skip_seen.push(stored.seq);
                continue;
            }
            newest_first.push(Envelope {
                seq: stored.seq,
                channel,
                author,
                mine,
                text,
            });
            if newest_first.len() > SESSION_DELTA_LIMIT {
                return SessionPlan::Reinitialize {
                    reason: ReinitializeReason::TooManyUnseen,
                };
            }
        }
        if crossed {
            break;
        }
    }

    if !crossed {
        return SessionPlan::Reinitialize {
            reason: ReinitializeReason::GapTooLarge,
        };
    }

    newest_first.reverse();
    let mut next_state = state.clone();
    for envelope in &newest_first {
        next_state.accept(envelope.seq);
    }
    for seq in skip_seen {
        next_state.accept(seq);
    }
    // The turn's own message is accepted too: the turn appends it, so the next
    // delta must not hand it back.
    if let Some(seq) = before {
        next_state.accept(seq);
    }
    SessionPlan::Delta {
        envelopes: newest_first,
        next_state,
    }
}

/// Which of this agent's channels owns `event`, if any.
fn channel_for(
    channels: &[Channel],
    event: &CompanyEvent,
    record: &CompanyRecord,
) -> Option<String> {
    // Codex P1: `owns(id, name, event)` matches EITHER spelling, which is
    // right for `desk_read`/`chat_history` where `desk_id`+`desk_name` name
    // the SAME desk — but here `channels` is the whole list of desks this
    // agent sits on, checked one at a time, so a channel `{id: "ops", name:
    // "sales"}` this agent sits on would also claim an event journaled under
    // chat_id = "sales" that belongs to an ACTUAL desk named `sales` this
    // agent does not sit on. Exact ids are resolved first and alone across
    // every one of this agent's channels; a name match is only trusted once
    // it is confirmed the event's key is not some OTHER real desk's id.
    if let Some(channel) = channels
        .iter()
        .find(|channel| chat_history::owns(&channel.id, &channel.id, event))
    {
        return Some(channel.label.clone());
    }
    let collides_with_a_real_desk_id = record
        .manifest
        .group_chats
        .iter()
        .any(|chat| chat_history::owns(&chat.id, &chat.id, event))
        || record
            .overlay_desks
            .iter()
            .any(|desk| chat_history::owns(&desk.id, &desk.id, event));
    if collides_with_a_real_desk_id {
        return None;
    }
    channels
        .iter()
        .find(|channel| chat_history::owns(&channel.name, &channel.name, event))
        .map(|channel| channel.label.clone())
}

/// Whether `agent_id` may read this row at all.
///
/// The **only** narrowing this module applies, and it is not about channels: an
/// `AgentReply` carrying a non-empty `audience` is a private aside, and a peer
/// outside it reads the row elided rather than in full. Applied here so a
/// session can never be handed something the desk projection would withhold.
fn readable_by(agent_id: &str, event: &CompanyEvent) -> bool {
    match event {
        CompanyEvent::AgentReply {
            agent_id: author,
            audience,
            ..
        } => {
            audience.is_empty()
                || author == agent_id
                || audience.iter().any(|member| member == agent_id)
        }
        _ => true,
    }
}

/// The conversational body of `event`, or `None` for a row that carries none.
fn body_of(agent_id: &str, event: &CompanyEvent) -> Option<(String, bool, String)> {
    match event {
        CompanyEvent::OperatorMessage {
            text,
            attachments,
            by,
            ..
        } => Some((
            super::chat_seed::operator_label(by),
            false,
            crate::brain::medulla::effects::with_attachment_refs(text, attachments),
        )),
        CompanyEvent::AgentReply {
            agent_id: author,
            text,
            ..
        } => Some((author.clone(), author == agent_id, text.clone())),
        // `owns` also admits `DeskTaskCompleted`, a structural marker with no
        // conversational body. Not a turn; not delivered.
        _ => None,
    }
}

/// Renders a delta as the cue block prepended to a turn's message.
///
/// # Why a cued turn and not a tool result
///
/// The reference implementation this shape was taken from is deliberately
/// asymmetric: **outbound is a tool call, inbound is a plain turn carrying a
/// text cue** (`[inbound]`, `[agent]`, `[routine]`, `[Group chat: "…"]`). That
/// is the right way round here too, and cheaper: OpenHuman's resume path
/// already speaks `(role, content)`, so a cued turn needs no new plumbing,
/// whereas a synthesised tool result would need a fabricated call id with no
/// matching call and would confuse [`fold_steps`](super::steps::fold_steps).
///
/// Returns `None` for an empty delta, so an ordinary same-channel reply pays
/// nothing for this.
pub fn render_cues(envelopes: &[Envelope]) -> Option<String> {
    let lines: Vec<String> = envelopes
        .iter()
        .filter(|envelope| !envelope.mine)
        .map(|envelope| {
            format!(
                "[{} · {}] {}",
                envelope.channel,
                envelope.author,
                envelope.text.trim()
            )
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "While you were away, this was said elsewhere in the company. It is \
         context, not a request — answer the message at the end of this turn.\n\n{}\n",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod test {
    /// The byline an agent is handed and the one the console is shipped are the
    /// same string.
    ///
    /// The console's raw view renders the cue line an agent received, and it
    /// gets the author half from `MessageView::cue_author` on the session
    /// route. If these two projections ever answered differently, that view
    /// would be putting a name in front of an operator that the agent never
    /// saw — which is the one thing it exists not to do. They are separate
    /// matches in separate modules (one is `#[cfg(feature = "openhuman")]`),
    /// so nothing but this test holds them together.
    #[test]
    fn cue_author_matches_the_envelope_the_agent_is_handed() {
        use crate::ports::types::{Actor, ActorKind};

        let cases = [
            // A signed-in person: the stable user id, never their screen name.
            Some(Actor {
                kind: ActorKind::User,
                id: "01a08d773a62-000000000034".to_string(),
            }),
            // A crossing referral, authored by a teammate: no person to name.
            Some(Actor {
                kind: ActorKind::Agent,
                id: "engineer".to_string(),
            }),
            // A machine credential, or a line journaled before attribution.
            None,
        ];
        for by in cases {
            let event = CompanyEvent::OperatorMessage {
                text: "hello".to_string(),
                by: by.clone(),
                chat: None,
                parent: None,
                deliverable: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
            };
            let (author, _, _) = body_of("brand_designer", &event).expect("a body");
            assert_eq!(
                author,
                crate::server::chat_history::cue_author(&by),
                "the cue's author and the route's `cue_author` disagree for {by:?}",
            );
        }
    }

    use super::*;
    use crate::ports::types::StoredEvent;
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream};

    /// A log that replays a fixed history, newest-first for `read_before`.
    struct FixedLog(Vec<StoredEvent>);

    #[async_trait]
    impl EventLog for FixedLog {
        async fn append(&self, _id: &CompanyId, _e: CompanyEvent) -> crate::Result<EventSeq> {
            unreachable!("a session delta only reads")
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

    fn op(seq: u64, chat: &str, text: &str) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            company: CompanyId::new("acme"),
            event: CompanyEvent::OperatorMessage {
                text: text.to_string(),
                by: None,
                chat: Some(chat.to_string()),
                parent: None,
                deliverable: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
            },
            at_millis: seq,
        }
    }

    fn reply(seq: u64, chat: &str, author: &str, text: &str, audience: &[&str]) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            company: CompanyId::new("acme"),
            event: CompanyEvent::AgentReply {
                audience: audience.iter().map(|id| id.to_string()).collect(),
                chat_id: chat.to_string(),
                agent_id: author.to_string(),
                text: text.to_string(),
                steps: Vec::new(),
                task_id: None,
                parent: None,
                mentions: Vec::new(),
                mention_depth: 0,
            },
            at_millis: seq,
        }
    }

    /// A company where `designer` sits on `#brand` and `copy` does not.
    ///
    /// Built from TOML rather than by struct literal, on the same reasoning
    /// `hivemind::test::record` uses: the manifest this exercises is the one an
    /// operator writes, and a literal would let a field drift out of the parse
    /// path without any test noticing.
    fn record() -> CompanyRecord {
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "designer"
role = "Designer"

[[agent]]
id = "copy"
role = "Writer"

[[group_chat]]
id = "brand"
name = "Brand"
members = ["designer"]

[[group_chat]]
id = "finance"
name = "Finance"
members = ["copy"]
"#,
        )
        .expect("test manifest parses");
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        }
    }

    fn log(events: Vec<StoredEvent>) -> Arc<dyn EventLog> {
        Arc::new(FixedLog(events))
    }

    /// The whole point of the module: a channel switch is no longer a reason to
    /// forget. What the agent has already seen is what it does not get again —
    /// not what channel it was on when it saw it.
    #[tokio::test]
    async fn a_delta_hands_over_only_what_this_agent_has_not_seen() {
        let events = log(vec![
            op(1, "brand", "start the hero"),
            reply(2, "brand", "designer", "on it", &[]),
            op(3, "dm:designer", "what did you decide?"),
        ]);
        let state = AgentSessionState {
            watermark: Some(EventSeq::new(2)),
            present_above_watermark: BTreeSet::new(),
        };
        let plan = prepare_delta(
            &events,
            &CompanyId::new("acme"),
            &record(),
            "designer",
            &state,
            Some(EventSeq::new(3)),
        )
        .await;
        let SessionPlan::Delta { envelopes, .. } = plan else {
            panic!("expected a delta, got {plan:?}");
        };
        // Rows 1 and 2 are below the watermark; row 3 is the turn's own message
        // and is excluded by the exclusive `before`. Nothing is left, and that
        // is correct — the agent has seen everything.
        assert!(envelopes.is_empty(), "{envelopes:?}");
    }

    /// A line said on a desk the agent sits on reaches it even though the turn
    /// it is taking is in a different conversation. Before the session was
    /// continuous this was unreachable by construction.
    #[tokio::test]
    async fn a_line_from_another_channel_reaches_the_session() {
        let events = log(vec![
            reply(1, "brand", "copy", "tone should be warmer", &[]),
            op(2, "dm:designer", "redo the hero"),
        ]);
        let state = AgentSessionState {
            watermark: Some(EventSeq::new(0)),
            present_above_watermark: BTreeSet::new(),
        };
        let plan = prepare_delta(
            &events,
            &CompanyId::new("acme"),
            &record(),
            "designer",
            &state,
            Some(EventSeq::new(2)),
        )
        .await;
        let SessionPlan::Delta { envelopes, .. } = plan else {
            panic!("expected a delta, got {plan:?}");
        };
        assert_eq!(envelopes.len(), 1, "{envelopes:?}");
        assert_eq!(envelopes[0].channel, "#Brand");
        assert_eq!(envelopes[0].author, "copy");
        assert!(!envelopes[0].mine);
    }

    /// A desk this agent is not on is not in its session, however busy it is.
    /// Merging the channels widened what an agent reads; it did not make the
    /// company one room.
    #[tokio::test]
    async fn a_desk_this_agent_is_not_on_stays_out() {
        let events = log(vec![
            reply(1, "finance", "copy", "the invoice is out", &[]),
            op(2, "dm:designer", "anything from finance?"),
        ]);
        let state = AgentSessionState {
            watermark: Some(EventSeq::new(0)),
            present_above_watermark: BTreeSet::new(),
        };
        let plan = prepare_delta(
            &events,
            &CompanyId::new("acme"),
            &record(),
            "designer",
            &state,
            Some(EventSeq::new(2)),
        )
        .await;
        let SessionPlan::Delta {
            envelopes,
            next_state,
        } = plan
        else {
            panic!("expected a delta, got {plan:?}");
        };
        assert!(envelopes.is_empty(), "{envelopes:?}");
        // Codex P1: neither row produced an envelope, but both were scanned —
        // and both must still be marked consumed. Before the fix, `accept`
        // was never called for either, so the watermark stayed pinned at its
        // starting value forever: every later turn would rescan the same two
        // non-deliverable rows, and once that rescan crossed
        // `SESSION_SCAN_LIMIT` it would reinitialize instead of completing,
        // permanently falling back to a chat-only reseed.
        assert_eq!(
            next_state.watermark,
            Some(EventSeq::new(2)),
            "scanned-but-nondeliverable rows must still advance the watermark: {next_state:?}"
        );
    }

    /// The one narrowing that survives. Channel isolation was dropped
    /// deliberately; audience isolation is not, and a private exchange this
    /// agent is not party to must never arrive in its context.
    #[tokio::test]
    async fn an_aside_this_agent_is_not_party_to_never_arrives() {
        let events = log(vec![
            reply(
                1,
                "brand",
                "copy",
                "between us: the client hated it",
                &["ceo"],
            ),
            reply(2, "brand", "copy", "and to you: ship it", &["designer"]),
            op(3, "brand", "where are we?"),
        ]);
        let state = AgentSessionState {
            watermark: Some(EventSeq::new(0)),
            present_above_watermark: BTreeSet::new(),
        };
        let plan = prepare_delta(
            &events,
            &CompanyId::new("acme"),
            &record(),
            "designer",
            &state,
            Some(EventSeq::new(3)),
        )
        .await;
        let SessionPlan::Delta { envelopes, .. } = plan else {
            panic!("expected a delta, got {plan:?}");
        };
        let texts: Vec<&str> = envelopes.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["and to you: ship it"], "{envelopes:?}");
    }

    /// A session with no watermark has nothing to be a delta against, and says
    /// so rather than replaying the company's whole history into a cold agent.
    #[tokio::test]
    async fn a_cold_session_asks_for_a_seed() {
        let plan = prepare_delta(
            &log(vec![op(1, "brand", "hello")]),
            &CompanyId::new("acme"),
            &record(),
            "designer",
            &AgentSessionState::default(),
            Some(EventSeq::new(2)),
        )
        .await;
        assert!(
            matches!(
                plan,
                SessionPlan::Reinitialize {
                    reason: ReinitializeReason::ColdStart
                }
            ),
            "{plan:?}"
        );
    }

    /// An agent away long enough that the replay would be worse than the
    /// recent window gets the recent window.
    #[tokio::test]
    async fn too_much_unseen_falls_back_to_a_seed() {
        let mut events = Vec::new();
        for seq in 1..=(SESSION_DELTA_LIMIT as u64 + 5) {
            events.push(reply(seq, "brand", "copy", &format!("line {seq}"), &[]));
        }
        let state = AgentSessionState {
            watermark: Some(EventSeq::new(0)),
            present_above_watermark: BTreeSet::new(),
        };
        let plan = prepare_delta(
            &log(events),
            &CompanyId::new("acme"),
            &record(),
            "designer",
            &state,
            None,
        )
        .await;
        assert!(
            matches!(
                plan,
                SessionPlan::Reinitialize {
                    reason: ReinitializeReason::TooManyUnseen
                }
            ),
            "{plan:?}"
        );
    }

    /// The watermark swallows a contiguous run rather than growing a set, so a
    /// busy session does not carry an ever-longer list of individual sequences.
    #[test]
    fn accepting_a_run_compacts_into_the_watermark() {
        let mut state = AgentSessionState::default();
        state.accept(EventSeq::new(1));
        state.accept(EventSeq::new(2));
        state.accept(EventSeq::new(3));
        assert_eq!(state.watermark, Some(EventSeq::new(3)));
        assert!(state.present_above_watermark.is_empty());
    }

    /// Codex P1: a chat-only reseed's recent-window seed covers only the
    /// incoming channel. Overwriting the watermark with the turn's own
    /// sequence — as this used to — would mark an OLDER, still-unseen row on
    /// some OTHER channel "already delivered" underneath it, and it would
    /// never reach the agent. `reseeded` must leave the prior watermark where
    /// it was: a row below it stays exactly as seen or unseen as it already
    /// was, and only the turn's own message is newly accepted.
    #[test]
    fn reseed_does_not_swallow_an_older_unseen_row_on_another_channel() {
        // Desk B's message at seq 6 has never been delivered.
        let before_reseed = AgentSessionState {
            watermark: Some(EventSeq::new(5)),
            present_above_watermark: BTreeSet::new(),
        };
        // A greeting on desk A, seq 7, triggers a chat-only reseed.
        let after_reseed = before_reseed.reseeded(Some(EventSeq::new(7)));
        assert_eq!(
            after_reseed.watermark,
            Some(EventSeq::new(5)),
            "the prior watermark must survive the reseed unchanged"
        );
        assert!(
            !after_reseed.already_seen(EventSeq::new(6)),
            "desk B's still-unseen row must not become 'delivered' by a reseed \
             that never showed it to the agent"
        );
        assert!(
            after_reseed.already_seen(EventSeq::new(7)),
            "the turn's own message, which the seed DID show the agent, is accepted"
        );
    }

    /// A true cold start (no watermark at all yet) must still come out of its
    /// first reseed WITH a watermark — otherwise `session.watermark.is_none()`
    /// keeps tripping the reseed branch forever and the agent can never walk a
    /// delta.
    #[test]
    fn reseed_from_a_true_cold_start_still_establishes_a_watermark() {
        let cold = AgentSessionState::default();
        let after_reseed = cold.reseeded(Some(EventSeq::new(3)));
        assert_eq!(after_reseed.watermark, Some(EventSeq::new(3)));
        assert!(after_reseed.present_above_watermark.is_empty());
    }

    /// The agent's own lines are in the session — it said them — but they are
    /// not cued as things it needs to read. Cueing them would have the model
    /// answering its own last reply.
    #[test]
    fn the_cue_block_leaves_out_this_agents_own_lines() {
        let mine = Envelope {
            seq: EventSeq::new(1),
            channel: "#Brand".to_string(),
            author: "designer".to_string(),
            mine: true,
            text: "on it".to_string(),
        };
        let theirs = Envelope {
            seq: EventSeq::new(2),
            channel: "#Brand".to_string(),
            author: "copy".to_string(),
            mine: false,
            text: "warmer".to_string(),
        };
        assert!(render_cues(std::slice::from_ref(&mine)).is_none());
        let rendered = render_cues(&[mine, theirs]).expect("a peer line is cued");
        assert!(rendered.contains("[#Brand · copy] warmer"), "{rendered}");
        assert!(!rendered.contains("on it"), "{rendered}");
    }
}
