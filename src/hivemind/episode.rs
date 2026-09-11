//! The host loop: fold, run the one turn the library authorized, append it,
//! commit the state it returned, repeat.
//!
//! That ordering is the whole contract. [`step`] never appends, never waits and
//! never calls back into this host; it hands back a `next_state` that is only
//! valid once the turn it authorized is **durably** in the journal. Committing
//! it before the append would let a failed write leave the episode believing a
//! turn happened that nothing can read back.
//!
//! Reading the transcript back out of the journal on every iteration — rather
//! than accumulating it in memory as the loop runs — is the same discipline
//! seen from the other side: what the room folds is exactly what a human
//! reading the desk would see, so the standings can never disagree with the
//! transcript they were derived from.

use std::sync::Arc;

use async_trait::async_trait;
use tinyhivemind_hive::{
    Conversation, EpisodeState, HiveStep, SESSION_WINDOW, Sequence, SessionQuery,
    aside::{AsideDecision, AsideInput, Viewer},
    desk::{Desk, DeskSet, ResponderMode},
    dispatch::DispatchConversation,
    mention::{MentionAuthor, MentionTarget},
    pins::{PIN_LIMIT, read_pinboard},
    project_for,
    roster::{Roster, RosterMember},
    step,
};

use super::evidential;
use super::log::EventLogSessionLog;
use super::memory::{HiveMemory, HiveMemoryHit, HiveMemoryNote, NullHiveMemory, RECALL_LIMIT};
use super::moves::{self, MoveViolation};
use super::prompt::{EpisodePrompt, split_reply};
use super::referral::{EpisodeReferrals, HiveFederation, HiveReferralRunner, ReferralLedger};
use super::scope::EpisodeScope;
use super::types::{EpisodeEnding, EpisodeOutcome, HiveDesk};
use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq};

/// Anything that can fill one authorized turn.
///
/// Deliberately the narrowest possible seam — an agent id and a prompt in, one
/// reply out. The production implementation is a normal harness turn, with its
/// tools, its memory loop and its approval gate all intact; a test scripts
/// replies without a model. Neither knows anything about episodes, which is
/// what keeps the driver testable without a provider and keeps a deliberating
/// teammate identical to a replying one in every way but the prompt.
#[async_trait]
pub trait HiveTurnRunner: Send + Sync {
    /// Run one turn on `agent_id` and return what it said.
    ///
    /// # Errors
    ///
    /// Returns whatever the underlying turn failed with. A failed turn ends the
    /// episode: the turns already appended stay in the transcript, and no
    /// closing report claims a decision the room did not reach.
    async fn speak(&self, agent_id: &str, prompt: &str) -> Result<String>;
}

/// How many `!pin` lines one closing note carries.
const MAX_NOTE_PINS: usize = 3;

/// The `!propose` line that put `topic` on the floor, without its marker.
///
/// The FIRST one wins: a topic is introduced once and later lines support it,
/// so the opening proposal is the statement of the option. Matching is on the
/// `#topic` token exactly — `#lazy` must not answer for `#lazy-load` — and the
/// marker and topic are stripped, leaving the sentence a person would read.
///
/// A multi-line turn is searched line by line, because a member may write prose
/// and then its move, and only the marked line is the proposal.
fn carried_proposal(
    transcript: &[tinyhivemind_hive::SessionMessage],
    topic: &str,
) -> Option<String> {
    transcript
        .iter()
        .flat_map(|message| message.content.lines())
        .find_map(|line| match proposed(line) {
            Some((named, text)) if named == topic && !text.is_empty() => Some(text.to_string()),
            _ => None,
        })
}

/// The topic a `!propose` line names, and what it says about it.
///
/// `None` for any other line. The topic token ends at whitespace, so `#lazy`
/// and `#lazy-load` are different topics rather than one being a prefix of the
/// other — crediting or reporting the wrong option is worse than doing neither.
fn proposed(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim().strip_prefix("!propose")?.trim_start();
    let rest = rest.strip_prefix('#')?;
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    let (topic, tail) = rest.split_at(end);
    (!topic.is_empty()).then(|| {
        (
            topic,
            tail.trim_start_matches([':', '\u{2014}', '-', ' ']).trim(),
        )
    })
}

/// The topic this line re-proposes, when the floor already holds one by that
/// name.
///
/// **A topic is put on the floor once.** `!propose` introduces an OPTION; a
/// member that agrees with an option already there is supporting it, and
/// `!support` is the move for that.
///
/// Reusing an id is not a style slip, it is a vote-corrupting one, because the
/// fold counts `Propose` and `Support` alike as backing. Observed live: two
/// members filed *opposite* answers — "lazy-load each section" and "load
/// everything up front" — under one id named after the question, and the room
/// reported a quorum of two for a decision they disagreed about. Neither had
/// supported anything; the tally could not tell them apart.
fn reuses_a_topic(line: &str, visible: &[tinyhivemind_hive::SessionMessage]) -> Option<String> {
    let (topic, _) = proposed(line)?;
    visible
        .iter()
        .flat_map(|message| message.content.lines())
        .any(|earlier| proposed(earlier).is_some_and(|(named, _)| named == topic))
        .then(|| topic.to_string())
}

/// The system line a failed turn leaves on the desk.
///
/// Authored by `hive-report`, the same reserved id the closing row uses, so the
/// log adapter reads it back as a **system** row: it can never be counted as a
/// supporter, and it stays visible through a blind round. It carries no marker,
/// so it folds to no trace at all — the room sees that a seat was asked and did
/// not answer, and nothing more.
fn failure_note(agent_id: &str, error: &dyn std::fmt::Display) -> String {
    let error = error.to_string();
    let first = error
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no detail");
    // Stripped of a leading marker for the same reason a demoted line is: this
    // row must never fold as a trace, and an error message that happens to
    // begin with `!` would.
    let first = first.trim_start_matches('!').trim();
    format!("@{agent_id}'s turn did not finish: {first}")
}

/// One deliberation episode on one desk.
pub struct EpisodeDriver<'a> {
    company: CompanyId,
    desk: HiveDesk,
    events: Arc<dyn EventLog>,
    runner: &'a dyn HiveTurnRunner,
    task: String,
    thread_root: Option<EventSeq>,
    memory: Arc<dyn HiveMemory>,
    federation: Option<(HiveFederation, &'a dyn HiveReferralRunner)>,
    /// Every desk in the company, for loading a speaker's *own* other
    /// conversations into its prompt.
    ///
    /// Deliberately not read off [`Self::federation`], which is `None` unless
    /// the home desk opted in to referral: whether a seat may ask another desk a
    /// question and whether it can already read a desk it sits on are different
    /// permissions, and conflating them would hide half a member's own history
    /// from it because of a knob about somebody else's desk. Empty is the
    /// default, and an empty list renders nothing.
    context_desks: Vec<super::referral::FederationDesk>,
}

impl std::fmt::Debug for EpisodeDriver<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EpisodeDriver")
            .field("company", &self.company)
            .field("desk", &self.desk.id)
            .field("thread_root", &self.thread_root)
            .finish_non_exhaustive()
    }
}

/// What one turn writes back besides the line it is journaled under.
///
/// Bundled rather than passed as two `&mut` parameters because the turn path
/// threads them through three functions (`line_from` → `grounded_and_regraded`
/// → `grounded`), and the retries mean each of those has to be able to replace
/// the aside as well as append a violation.
///
/// The two have different lifetimes on purpose: `violations` accumulates over
/// the whole episode and is reported in the outcome, while `aside` belongs to
/// one turn and is cleared before each.
#[derive(Default)]
struct TurnScratch {
    /// Lines a member deposited that its seat was not entitled to make.
    violations: Vec<MoveViolation>,
    /// The private row this turn's reply carried, if any.
    aside: Option<String>,
}

impl<'a> EpisodeDriver<'a> {
    /// Open a driver over `desk`, answering `task`.
    #[must_use]
    pub fn new(
        company: CompanyId,
        desk: HiveDesk,
        events: Arc<dyn EventLog>,
        runner: &'a dyn HiveTurnRunner,
        task: impl Into<String>,
    ) -> Self {
        Self {
            company,
            desk,
            events,
            runner,
            task: task.into(),
            thread_root: None,
            memory: Arc::new(NullHiveMemory),
            federation: None,
            context_desks: Vec::new(),
        }
    }

    /// Let this desk ask another one a question.
    ///
    /// `federation` is the snapshot `@#desk` and `@teammate` resolve against
    /// ([`desk_federation`](super::desk_federation)), and `runner` is how the
    /// far teammate's turn is actually filled. Absent — the default — no line
    /// is ever considered for a referral and the episode is byte-identical to
    /// the one this driver ran before referral existed.
    #[must_use]
    pub fn with_federation(
        mut self,
        federation: HiveFederation,
        runner: &'a dyn HiveReferralRunner,
    ) -> Self {
        self.federation = Some((federation, runner));
        self
    }

    /// Give every seat the other conversations it is part of.
    ///
    /// `desks` is the whole company's desk list; each speaker is matched against
    /// it by membership, so one snapshot serves every seat in the room. Unset is
    /// the default and renders nothing, which is what every caller that existed
    /// before this builder got.
    #[must_use]
    pub fn with_context_desks(mut self, desks: Vec<super::referral::FederationDesk>) -> Self {
        self.context_desks = desks;
        self
    }

    /// Give the desk a memory: recalled once before the first turn, written
    /// once when the episode ends.
    ///
    /// Defaults to [`NullHiveMemory`], so an episode driven without one is
    /// byte-identical to the one this driver ran before desk memory existed.
    #[must_use]
    pub fn with_memory(mut self, memory: Arc<dyn HiveMemory>) -> Self {
        self.memory = memory;
        self
    }

    /// Run the episode inside the thread rooted at `thread_root`.
    ///
    /// `None` — the default — is the desk channel itself, which is where every
    /// unparented message lives. Every turn is journaled with this same parent,
    /// exactly as a single-responder reply is: an answer joins the thread its
    /// question was asked in rather than opening one underneath it, and the
    /// library's own channel projection only promotes rows whose parent is a
    /// root, so a turn parented to the operator's message would be invisible to
    /// the rest of the episode.
    #[must_use]
    pub fn in_thread(mut self, thread_root: Option<EventSeq>) -> Self {
        self.thread_root = thread_root;
        self
    }

    /// Deliberate until the room converges, deadlocks, exhausts its budget, or
    /// finds it has nothing to say.
    ///
    /// `trigger` is the journal sequence of the operator message that opened
    /// the room, and becomes the episode's watermark: everything at or below it
    /// is context the room may read and may cite, but is not folded into the
    /// episode's own traces. Without it a desk would inherit the votes of every
    /// conversation that preceded this one.
    ///
    /// # Errors
    ///
    /// Returns [`OpenCompanyError::Config`] when the roster, desk snapshot or
    /// policy this host built is malformed — which can only mean a bug here,
    /// since all three are derived from a validated manifest — and the journal's
    /// own error when a turn cannot be appended. A failed append stops the
    /// episode before the state it would have committed is taken up, so the
    /// room never believes in a turn nothing can read.
    pub async fn run(&self, trigger: EventSeq) -> Result<EpisodeOutcome> {
        let conversation = Conversation {
            desk_id: self.desk.id.clone(),
            desk_name: self.desk.name.clone(),
            thread_root: self.thread_root.map(|seq| Sequence(seq.value())),
        };
        // This instance's own fold boundary (issue: two hive episodes in the
        // same thread could count each other's turns as their own votes). A
        // thread already has one root regardless of how many episodes open
        // inside it, so `conversation` alone cannot tell two concurrent
        // episodes apart — only this scope, narrowed to what THIS `run` call
        // itself appends above `trigger`, can. See `EpisodeScope`'s module
        // doc for why the watermark below cannot do this on its own.
        let scope = Arc::new(EpisodeScope::new(trigger));
        let log = EventLogSessionLog::new(
            Arc::clone(&self.events),
            self.company.clone(),
            self.desk.id.clone(),
            self.desk.name.clone(),
        )
        .with_scope(Arc::clone(&scope));
        let members: Vec<RosterMember> = self
            .desk
            .members
            .iter()
            .map(|member| RosterMember {
                id: member.id.clone(),
                name: Some(member.label.clone()),
            })
            .collect();
        let desks = vec![Desk {
            id: self.desk.id.clone(),
            name: self.desk.name.clone(),
            description: self.desk.description.clone(),
            members: self.desk.member_ids(),
            // `Auto`, always. The room is the responder; a lead is what a desk
            // has when exactly one member answers, which is the path this
            // episode was opened instead of.
            responder_mode: ResponderMode::Auto,
        }];
        let retired: Vec<String> = Vec::new();
        let policy = self.desk.policy();

        let mut state = EpisodeState::opened(conversation.clone(), Sequence(trigger.value()));
        let mut turns = 0_u32;
        let mut first_seq: Option<EventSeq> = None;
        let mut last_seq: Option<EventSeq> = None;
        let mut scratch = TurnScratch::default();
        // Every line this episode journaled, in order, so the closing note is
        // written from what the room actually deposited rather than from a
        // second read of the journal that could disagree with it.
        let mut lines: Vec<(EventSeq, String, String)> = Vec::new();
        let mut spoken: Vec<String> = Vec::new();
        let mut last_speaker: Option<String> = None;
        let mut failed_turns = 0_u32;
        // Consecutive failures, reset by any turn that comes back. The cap is
        // members x 2: a room where every seat has failed twice in a row is not
        // a room having a bad turn, it is a harness that is down, and spending
        // the rest of the budget on it buys nothing.
        let mut consecutive_failures = 0_usize;
        let failure_cap = self.desk.members.len().saturating_mul(2).max(2);

        // Recalled once, before the first turn, and rendered into every prompt
        // of this episode. Once rather than per turn because the answer cannot
        // change mid-episode, and best-effort because a store that is briefly
        // unreachable is not a reason to refuse the operator's message.
        let recall = self.recall().await;

        // The referral edge, when the desk opted in and the company has a peer
        // desk to ask. `None` is the overwhelmingly common case and costs the
        // loop below one `if let` per turn.
        let referrals = self.federation.as_ref().map(|(federation, runner)| {
            (
                federation,
                EpisodeReferrals::new(
                    *runner,
                    Arc::clone(&self.events),
                    self.company.clone(),
                    tinyhivemind_hive::dispatch::DispatchConversation {
                        desk_id: self.desk.id.clone(),
                        thread_root: self.thread_root.map(EventSeq::value),
                    },
                    federation,
                    self.desk.config.referral.peer_cap(),
                    Arc::clone(&scope),
                ),
                self.desk.config.referral.policy(),
            )
        });
        // The peer list every prompt of this episode renders. Built once for
        // the same reason the recall is: it cannot change mid-episode, and a
        // seat that saw a different set of desks from the seat before it would
        // be reading a different company.
        let peers: Vec<(String, String, Option<String>)> = referrals
            .as_ref()
            .filter(|(_, _, policy)| policy.enabled && policy.reach.addresses_desks())
            .map(|(federation, _, _)| {
                federation
                    .peers_of(&self.desk.id)
                    .into_iter()
                    .map(|desk| (desk.id.clone(), desk.name.clone(), desk.description.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let ending = loop {
            let transcript = tinyhivemind_hive::project_session(
                &log,
                &SessionQuery {
                    conversation: conversation.clone(),
                    // The *true medium*, deliberately unnarrowed. `step` folds
                    // this, and the room's counting has to be single-valued:
                    // a per-reader fold would make `quorum::standings` return
                    // a well-formed wrong answer with no error path — a quorum
                    // one member can see and another cannot. `project_for`
                    // below is the only thing that narrows, per speaker.
                    viewer: Viewer::Operator,
                    before: None,
                    window: SESSION_WINDOW,
                },
            )
            .await
            .map_err(|error| self.malformed(&error))?;

            let decision = {
                let roster = Roster::new(&members, &[], &retired);
                let desk_set = DeskSet::new(&desks, &[], &[], &[], &retired);
                step(&state, &transcript, &roster, &desk_set, &policy)
                    .map_err(|error| self.malformed(&error))?
            };

            let turn = match decision {
                HiveStep::Speak { turn } => *turn,
                HiveStep::Converged { topic, standing } => {
                    break EpisodeEnding::Converged {
                        // Read from the very transcript `step` just decided on,
                        // so the text reported is the text that carried.
                        proposal: carried_proposal(&transcript, topic.as_str()),
                        topic: topic.to_string(),
                        supporters: standing.supporters.clone(),
                    };
                }
                HiveStep::Deadlocked { topics } => {
                    break EpisodeEnding::Deadlocked {
                        topics: topics.iter().map(ToString::to_string).collect(),
                    };
                }
                HiveStep::Exhausted { .. } => break EpisodeEnding::Exhausted,
                HiveStep::Idle => break EpisodeEnding::Idle,
            };

            let visible = project_for(&turn, &transcript);
            // Folded fresh each turn from the same journal the transcript came
            // from, so a pin laid down *during* the episode is on the board for
            // the next speaker rather than the next episode.
            let pins = match read_pinboard(
                &log,
                &conversation,
                // The board is rendered into *this speaker's* prompt, so it is
                // read as this speaker: a pin over an aside it is not in must
                // not quote content to it, and one over an aside it *is* in
                // must still reach it.
                &Viewer::Agent {
                    id: turn.agent_id.clone(),
                },
                PIN_LIMIT,
                None,
            )
            .await
            {
                Ok(pins) => pins,
                Err(error) => {
                    tracing::warn!(
                        company = %self.company,
                        desk = %self.desk.id,
                        error = %error,
                        "[hive] the pinboard could not be read; this turn sees no pins"
                    );
                    Vec::new()
                }
            };
            let member = self.desk.member(&turn.agent_id).ok_or_else(|| {
                OpenCompanyError::Config(format!(
                    "hive episode on desk `{}`: the floor was given to `{}`, who is not seated",
                    self.desk.id, turn.agent_id,
                ))
            })?;
            // Speaker diversity, as a *prompt* and never as an override. The
            // fold picked this speaker under invariants this host does not get
            // to break, so when it hands the floor straight back to whoever
            // just held it while somebody has not used theirs at all, the
            // repair available is to name the missing members and let the
            // speaker `!question` or `!defer` to them.
            let unspoken: Vec<String> = if last_speaker.as_deref() == Some(turn.agent_id.as_str()) {
                self.desk
                    .member_ids()
                    .into_iter()
                    .filter(|id| !spoken.contains(id))
                    .collect()
            } else {
                Vec::new()
            };
            let elsewhere = self.elsewhere_for(&turn.agent_id).await;
            let prompt = EpisodePrompt::new(member, &self.desk, &self.task, policy.quorum, &pins)
                .with_recall(&recall)
                .with_elsewhere(&elsewhere)
                .with_unspoken(&unspoken)
                .with_peers(peers.clone())
                .with_trigger(Sequence(trigger.value()))
                .render(&turn, &visible);

            // Cleared per turn: the aside belongs to the reply that produced
            // this turn's line, never to the one before it.
            scratch.aside = None;
            let line = match self
                .line_from(&turn.agent_id, &prompt, &visible, &mut scratch)
                .await
            {
                Ok(line) => {
                    consecutive_failures = 0;
                    line
                }
                // A member's turn failing is not the room failing (the live
                // case: one turn hit the harness's per-turn wall-clock ceiling
                // and a `?` here threw away three good turns and answered the
                // operator with a 500). The transcript records the miss as a
                // system row — trace-less, so it folds to nothing and can
                // never be counted as support — the budget still advances, and
                // the next speaker is chosen from a transcript that shows what
                // happened.
                Err(error) => {
                    failed_turns = failed_turns.saturating_add(1);
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::warn!(
                        company = %self.company,
                        desk = %self.desk.id,
                        agent = %turn.agent_id,
                        error = %error,
                        "[hive] a member's turn did not finish; the room continues"
                    );
                    let seq = self
                        .events
                        .append(
                            &self.company,
                            CompanyEvent::AgentReply {
                                chat_id: self.desk.id.clone(),
                                agent_id: super::HIVE_FAILURE_AUTHOR.to_string(),
                                text: failure_note(&turn.agent_id, &error),
                                // The room's own report is never private: a
                                // reader who could not see that a turn failed
                                // would be reading a transcript with a hole in
                                // it that nothing accounts for.
                                audience: Vec::new(),
                                steps: Vec::new(),
                                task_id: None,
                                parent: self.thread_root,
                                mentions: Vec::new(),
                                mention_depth: 0,
                            },
                        )
                        .await?;
                    scope.record(seq);
                    last_seq = Some(seq);
                    state = turn.next_state;
                    turns = turns.saturating_add(1);
                    last_speaker = Some(turn.agent_id.clone());
                    if consecutive_failures >= failure_cap {
                        return Err(OpenCompanyError::Config(format!(
                            "hive episode on desk `{}`: {consecutive_failures} turns in a row failed, which is every seat twice over — the room stopped rather than spending the rest of its budget on a harness that is down: {error}",
                            self.desk.id,
                        )));
                    }
                    continue;
                }
            };
            let seq = self
                .events
                .append(
                    &self.company,
                    CompanyEvent::AgentReply {
                        chat_id: self.desk.id.clone(),
                        agent_id: turn.agent_id.clone(),
                        text: line.clone(),
                        // The turn's own contribution is always the room's. An
                        // aside is a *second* row riding alongside it, appended
                        // below.
                        audience: Vec::new(),
                        // The episode's own turns carry no step timeline: the
                        // room is reading one line per turn, and a tool trace
                        // belongs to the turn's own bubble, which this path
                        // does not raise.
                        steps: Vec::new(),
                        task_id: None,
                        parent: self.thread_root,
                        // A deliberation line names topics and message numbers,
                        // not people. Left empty rather than half-resolved —
                        // and a reply's mentions are never consulted by
                        // dispatch anyway, which is the mention-loop fuse.
                        mentions: Vec::new(),
                        mention_depth: 0,
                    },
                )
                .await?;
            scope.record(seq);
            first_seq.get_or_insert(seq);
            last_seq = Some(seq);
            // The aside rides alongside the turn that authored it and is not
            // charged as one (ADR 0011): one authorized turn produces the
            // member's ordinary contribution *and*, optionally, one private
            // row. `EpisodeState::spent` counts turns rather than rows, and
            // `step` drops a non-desk row before it reaches any trace or
            // standing, so this adds nothing the episode can vote.
            //
            // Under the old rule an aside *was* the turn, and a live six-day
            // run of `companies/vending_machine_co` used the move zero times:
            // asking a peer meant not depositing, not objecting and not
            // refuting, while the rest of the room went on accumulating support
            // for the option the member had stepped away to ask about.
            //
            // A refused audience is **dropped, not published**. Falling back to
            // the desk would put a second desk-visible contribution on one
            // turn, which is the one thing a turn may not produce — and the
            // member has already said its piece in the row above.
            if let Some(aside_line) = scratch.aside.take() {
                let audience = self.aside_audience(
                    &turn.agent_id,
                    &aside_line,
                    &transcript,
                    &members,
                    &desks,
                    &retired,
                );
                if audience.is_empty() {
                    tracing::debug!(
                        company = %self.company,
                        desk = %self.desk.id,
                        agent = %turn.agent_id,
                        "[hive] an aside was not authorized; the row was dropped"
                    );
                } else {
                    let aside_seq = self
                        .events
                        .append(
                            &self.company,
                            CompanyEvent::AgentReply {
                                chat_id: self.desk.id.clone(),
                                agent_id: turn.agent_id.clone(),
                                text: aside_line,
                                audience,
                                steps: Vec::new(),
                                task_id: None,
                                parent: self.thread_root,
                                mentions: Vec::new(),
                                mention_depth: 0,
                            },
                        )
                        .await?;
                    scope.record(aside_seq);
                    last_seq = Some(aside_seq);
                }
            }
            // Considered *after* the line is durable and *before* the next
            // speaker is chosen, which is the whole of the timing. The wiki
            // measures this as the single largest effect in the mechanism: a
            // desk that asks a peer and then votes before the answer lands has
            // voted past the information it paid a turn for, so an answer that
            // arrives one row late is an answer that arrives never.
            //
            // `last_seq` is deliberately not advanced by what a referral
            // journals: it names the episode's last *turn*, and a question's
            // answer is a row the room reads, not a turn the room took.
            //
            // Gated on the line still carrying an allowed marker:
            // `moves::demote` (in `line_from`) strips only the leading `!` off
            // a barred move, and leaves the rest of the text — including any
            // `@#desk` mention it contains — intact. `consider` resolves
            // mentions from raw content with no grammar check of its own, so
            // without this a barred move that happened to mention a desk
            // could still spend a peer-desk turn and return an answer under
            // `HIVE_REFERRAL_AUTHOR`, even though the line itself was refused
            // as an illegitimate move. `line_kind` returns `None` for a
            // demoted line (no leading marker left to recognize), so this is
            // the same "committed and legitimate" test the fold already
            // applies before counting a line as anything.
            if moves::line_kind(&line).is_some()
                && let Some((federation, queue, policy)) = referrals.as_ref()
            {
                super::referral::consider(
                    queue,
                    *policy,
                    federation,
                    &tinyhivemind_hive::dispatch::DispatchConversation {
                        desk_id: self.desk.id.clone(),
                        thread_root: self.thread_root.map(EventSeq::value),
                    },
                    &turn.agent_id,
                    &line,
                    seq,
                )
                .await;
            }
            lines.push((seq, turn.agent_id.clone(), line));
            if !spoken.contains(&turn.agent_id) {
                spoken.push(turn.agent_id.clone());
            }
            last_speaker = Some(turn.agent_id.clone());
            // Durably appended, so — and only now — the state the library
            // returned may be taken up.
            state = turn.next_state;
            turns = turns.saturating_add(1);
        };

        let referral_ledger = match referrals.as_ref() {
            Some((_, queue, _)) => queue.ledger().await,
            None => ReferralLedger::default(),
        };
        let mut outcome = EpisodeOutcome {
            ending,
            turns,
            first_seq,
            last_seq,
            report_seq: None,
            violations: scratch.violations,
            failed_turns,
            referrals: referral_ledger,
        };
        self.remember(&outcome, &lines).await;
        outcome.report_seq = self.report(&outcome, &scope).await;
        Ok(outcome)
    }

    /// Run one turn and return the line the transcript should keep, enforcing
    /// this seat's move grammar on the way.
    ///
    /// Two stages, and never fatal:
    ///
    /// 1. The reply's marker line is read. A marker this seat may make (or no
    ///    marker at all) is the line, unchanged.
    /// 2. A marker it may not make is handed back once, with a one-line
    ///    correction appended to the same prompt — the same prompt so the
    ///    member is not re-reading a truncated version of the room, and one
    ///    correction so a member that has misunderstood the grammar cannot
    ///    spend the desk's whole budget being told about it.
    /// 3. A second violation is journaled with its leading `!` removed. The
    ///    words survive, the trace does not: `resolve` reads a marker at the
    ///    start of a line and nowhere else, so a demoted line can never be
    ///    folded as support for anything. That is the property the whole
    ///    mechanism turns on — a barred move that still counted would be a rule
    ///    the fold does not enforce.
    ///
    /// A line that clears the grammar then goes through the same one-retry
    /// mechanism for citation discipline (see [`evidential`]): under
    /// `require_evidential` a `!support` whose citations reach no `!evidence`
    /// counts for nothing at all, so the member is handed one correction naming
    /// the evidence sequences that would have worked. The second attempt is
    /// journaled **as-is** — the fold, not this host, decides what a line is
    /// worth, and a support that still misses is a real position the transcript
    /// should record.
    async fn line_from(
        &self,
        agent_id: &str,
        prompt: &str,
        visible: &[tinyhivemind_hive::SessionMessage],
        scratch: &mut TurnScratch,
    ) -> Result<String> {
        let allowed = self.desk.config.moves_for(agent_id);
        let (line, rode) = split_reply(&self.runner.speak(agent_id, prompt).await?);
        scratch.aside = rode;
        let Some(kind) = moves::line_kind(&line).filter(|kind| !allowed.contains(kind)) else {
            return self
                .grounded_and_regraded(agent_id, prompt, visible, line, &allowed, scratch)
                .await;
        };
        let corrected = format!("{prompt}\n\n{}", moves::correction(kind, &allowed));
        // The retry's aside replaces the first attempt's: the corrected reply is
        // the turn that actually happened, and carrying a private line over from
        // a reply the room never saw would publish something its author did not
        // write on the turn it was written for.
        let (line, rode) = split_reply(&self.runner.speak(agent_id, &corrected).await?);
        scratch.aside = rode;
        let Some(kind) = moves::line_kind(&line).filter(|kind| !allowed.contains(kind)) else {
            return self
                .grounded_and_regraded(agent_id, prompt, visible, line, &allowed, scratch)
                .await;
        };
        tracing::info!(
            company = %self.company,
            desk = %self.desk.id,
            agent = %agent_id,
            attempted = %kind,
            "[hive] a member used a move its seat does not have, twice; the line was demoted"
        );
        scratch.violations.push(MoveViolation {
            agent_id: agent_id.to_owned(),
            attempted: kind.to_owned(),
        });
        Ok(moves::demote(&line))
    }

    /// [`grounded`](Self::grounded), with the seat's move grammar re-enforced
    /// on whatever it hands back.
    ///
    /// `grounded` may send the member back for one more turn (the evidential
    /// retry, when a `!support` reaches no evidence) and keeps whatever comes
    /// of that unconditionally — its own doc says so, and that is right for
    /// citation discipline: a support that still misses evidence is a real
    /// position. But "keep it unconditionally" also meant the retried line
    /// never had its move *kind* re-checked, so a member answering the
    /// citation-correction prompt could switch to a marker kind its seat is
    /// not entitled to make at all, and it would fold into the transcript as
    /// a legitimate move — exactly the class of bug `moves_for` seat
    /// restriction exists to prevent everywhere else `line_from` returns a
    /// line. Re-deriving `line_kind` and demoting on the same terms as the
    /// first-reply path closes that gap without touching the evidential
    /// retry's own, separate, contract.
    async fn grounded_and_regraded(
        &self,
        agent_id: &str,
        prompt: &str,
        visible: &[tinyhivemind_hive::SessionMessage],
        line: String,
        allowed: &[&'static str],
        scratch: &mut TurnScratch,
    ) -> Result<String> {
        let line = self
            .grounded(agent_id, prompt, visible, line, scratch)
            .await?;
        let Some(kind) = moves::line_kind(&line).filter(|kind| !allowed.contains(kind)) else {
            return Ok(line);
        };
        tracing::info!(
            company = %self.company,
            desk = %self.desk.id,
            agent = %agent_id,
            attempted = %kind,
            "[hive] a member's evidential retry used a move its seat does not have; \
             the line was demoted"
        );
        scratch.violations.push(MoveViolation {
            agent_id: agent_id.to_owned(),
            attempted: kind.to_owned(),
        });
        Ok(moves::demote(&line))
    }

    /// One corrective turn when a member re-proposes a topic already on the
    /// floor — see [`reuses_a_topic`] for why that corrupts the tally rather
    /// than merely reading badly.
    ///
    /// Shaped exactly like [`grounded`](Self::grounded): detect, say what to do
    /// instead, ask once more, keep whatever comes back. A member that
    /// re-proposes twice has made its point, and a second correction would
    /// spend a third turn winning a naming argument.
    async fn fresh_topic(
        &self,
        agent_id: &str,
        prompt: &str,
        visible: &[tinyhivemind_hive::SessionMessage],
        line: String,
        scratch: &mut TurnScratch,
    ) -> Result<String> {
        let Some(topic) = reuses_a_topic(&line, visible) else {
            return Ok(line);
        };
        tracing::info!(
            company = %self.company,
            desk = %self.desk.id,
            agent = %agent_id,
            topic = %topic,
            "[hive] a member re-proposed a topic already on the floor; asked once more"
        );
        let corrected = format!(
            "{prompt}\n\n#{topic} is already on the floor. `!propose` puts a NEW option there, \
             and two proposals under one id count as agreement even when they say opposite \
             things. If you agree with #{topic}, write `!support #{topic} ^N` citing what \
             convinced you. If you are arguing for something else, propose it under its own \
             topic id — one that names YOUR option, not the question."
        );
        let (line, rode) = split_reply(&self.runner.speak(agent_id, &corrected).await?);
        scratch.aside = rode;
        Ok(line)
    }

    /// The same line, once its citations have been given one chance to reach a
    /// fact.
    ///
    /// A no-op unless the desk set `require_evidential` and the line is a
    /// `!support` whose chain lands on no `!evidence` in what this turn could
    /// see. When it is, the member is asked again with one appended line naming
    /// the sequences that carry evidence — the same one-retry shape a barred
    /// move gets, and for the same reason: a member that has misread the rule
    /// must not be able to spend the desk's whole budget on it.
    ///
    /// Whatever comes back is kept. This host does not get a second veto over
    /// what a member is allowed to think; the correction exists because a
    /// support that counts for nothing reads, from the transcript, exactly like
    /// one that counts.
    async fn grounded(
        &self,
        agent_id: &str,
        prompt: &str,
        visible: &[tinyhivemind_hive::SessionMessage],
        line: String,
        scratch: &mut TurnScratch,
    ) -> Result<String> {
        // A distinct rule with its own correction, applied first: a re-proposed
        // topic corrupts the tally whatever the desk's evidential setting is.
        let line = self
            .fresh_topic(agent_id, prompt, visible, line, scratch)
            .await?;
        if self.desk.config.require_evidential != Some(true)
            || !evidential::support_misses_evidence(&line, agent_id, visible)
        {
            return Ok(line);
        }
        let available = evidential::evidence_sequences(visible);
        tracing::info!(
            company = %self.company,
            desk = %self.desk.id,
            agent = %agent_id,
            evidence = ?available,
            "[hive] a support reached no evidence; the member was asked once more"
        );
        let corrected = format!("{prompt}\n\n{}", evidential::correction(&available));
        // Same rule as the move correction: the retry is the turn that
        // happened, so its aside replaces whatever the first attempt carried.
        let (line, rode) = split_reply(&self.runner.speak(agent_id, &corrected).await?);
        scratch.aside = rode;
        Ok(line)
    }

    /// The conversations this speaker is part of besides the one deliberating.
    ///
    /// Read per turn rather than once per episode, for the same reason the
    /// pinboard is: a message that lands on this seat's other desk while this
    /// room is still open is exactly what the next speaker should be holding.
    ///
    /// Projected **as the speaker** and never as the operator, so an aside it is
    /// not in arrives elided here exactly as it would at home. The episode's
    /// `scope` is deliberately not applied either: that boundary is *this* desk's
    /// fold, and another conversation's rows are not votes here at all.
    ///
    /// Best-effort per conversation — one that cannot be read is dropped with a
    /// warning rather than failing the turn. A seat reading less than it might is
    /// a poorer prompt; a stalled room is a worse outcome.
    async fn elsewhere_for(
        &self,
        agent_id: &str,
    ) -> Vec<(String, Vec<tinyhivemind_hive::SessionMessage>)> {
        // Unset is the default, and a caller that never named the company's desks
        // gets exactly the prompt it got before this existed — including no read
        // of the seat's own direct line, which would otherwise be a new query on
        // every turn of every episode ever driven.
        if self.context_desks.is_empty() {
            return Vec::new();
        }
        let viewer = Viewer::Agent {
            id: agent_id.to_string(),
        };
        // Every other desk this seat sits on, and then its own direct line. A
        // DM's `chat_id` *is* the roster agent id, so that id is carried in both
        // fields: `in_desk` matches on id or name, and a desk that happened to be
        // named like an agent could otherwise widen the match.
        //
        // The direct line carries TWO targets, not one (Codex P2): a `desk_dm`
        // is journaled under the bare agent id ordinarily, but under the
        // `dm:<agent-id>` spelling whenever the bare one collides with a desk id
        // (`speech_tools::dm_journal_key`) or names a General spelling (issue
        // #364's grandfather case) — `agent_channels` registers both for exactly
        // this reason, and `EventLogSessionLog::addresses_desk` matches on
        // whichever exact spelling a row was journaled under. Reading only the
        // bare one would silently drop the prefixed rows from this seat's own
        // elsewhere context; both are queried and merged under one label so a
        // hive turn sees its whole direct line regardless of which spelling
        // wrote it.
        let direct_line_label = format!("Your direct line (@{agent_id})");
        // Codex P2: General is the synthetic company-wide channel, not a
        // declared desk — `agent_channels` grants every agent that channel
        // (`company/chat_history.rs`'s own-line loop), but `context_desks` is
        // built from declared desks and so never includes it. Without a
        // target for it here, a hive member reads its other desks and its DM
        // but never `#general`, even though it can. Skipped only when this
        // episode's own desk somehow *is* General — never true in practice
        // (`desk_episode` refuses to open one there), but defensive against a
        // caller passing an odd `HiveDesk`.
        let general = tinyhivemind_core::chat::GENERAL_DESK.to_string();
        let general_label = format!("#{general} ({general})");
        let targets: Vec<(String, String, String)> = self
            .context_desks
            .iter()
            .filter(|desk| {
                desk.id != self.desk.id && desk.members.iter().any(|member| member == agent_id)
            })
            .map(|desk| {
                (
                    desk.id.clone(),
                    desk.name.clone(),
                    format!("#{} ({})", desk.id, desk.name),
                )
            })
            .chain(
                (self.desk.id != general)
                    .then(|| (general.clone(), general.clone(), general_label)),
            )
            .chain(std::iter::once((
                agent_id.to_string(),
                agent_id.to_string(),
                direct_line_label.clone(),
            )))
            .chain(std::iter::once((
                format!("{}{agent_id}", crate::runtime::assignee::DM_PREFIX),
                format!("{}{agent_id}", crate::runtime::assignee::DM_PREFIX),
                direct_line_label,
            )))
            .collect();
        let mut elsewhere: Vec<(String, Vec<tinyhivemind_hive::SessionMessage>)> =
            Vec::with_capacity(targets.len());
        for (desk_id, desk_name, label) in targets {
            let log = EventLogSessionLog::new(
                Arc::clone(&self.events),
                self.company.clone(),
                desk_id.clone(),
                desk_name.clone(),
            );
            match tinyhivemind_hive::project_session(
                &log,
                &SessionQuery {
                    conversation: Conversation {
                        desk_id,
                        desk_name,
                        thread_root: None,
                    },
                    viewer: viewer.clone(),
                    before: None,
                    window: SESSION_WINDOW,
                },
            )
            .await
            {
                Ok(rows) if !rows.is_empty() => {
                    // The two direct-line targets share one label; merge into
                    // the existing section rather than opening a second one
                    // under the same name.
                    if let Some(existing) = elsewhere
                        .iter_mut()
                        .find(|(existing, _)| *existing == label)
                    {
                        // Codex P2 (fresh evidence): each target's own window
                        // is already chronological, but concatenating two
                        // windows is not — a naive `extend` can leave an
                        // older bare-spelling row after a newer prefixed one,
                        // and the combined length can exceed `SESSION_WINDOW`.
                        // Sorted by sequence, deduplicated (both spellings can
                        // in principle carry the same row), and cut back down
                        // to the newest `SESSION_WINDOW`.
                        existing.1.extend(rows);
                        existing.1.sort_by_key(|message| message.sequence);
                        existing.1.dedup_by_key(|message| message.sequence);
                        if existing.1.len() > SESSION_WINDOW {
                            let overflow = existing.1.len() - SESSION_WINDOW;
                            existing.1.drain(..overflow);
                        }
                    } else {
                        elsewhere.push((label, rows));
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    company = %self.company,
                    desk = %self.desk.id,
                    agent = %agent_id,
                    error = %error,
                    "[hive] a seat's other conversation could not be read; the turn runs without it"
                ),
            }
        }
        elsewhere
    }

    /// What the desk remembers about the operator's task, best-effort.
    async fn recall(&self) -> Vec<HiveMemoryHit> {
        match self.memory.recall(&self.task, RECALL_LIMIT).await {
            Ok(hits) => hits,
            Err(error) => {
                tracing::warn!(
                    company = %self.company,
                    desk = %self.desk.id,
                    error = %error,
                    "[hive] the desk's memory could not be recalled; the room opens without it"
                );
                Vec::new()
            }
        }
    }

    /// Write the one note this episode leaves for the next similar question.
    ///
    /// A converged episode leaves the thing that would actually shorten the
    /// next one: what carried, who backed it, every fact anybody put on the
    /// record, whatever the room pinned, and the line that recorded the
    /// decision. An episode that did not converge leaves a shorter note naming
    /// the options that competed — knowing a desk has already argued #stage
    /// against #ship without settling it is worth having, and pretending it
    /// concluded something would be worse than saying nothing.
    ///
    /// An [`EpisodeEnding::Idle`] episode writes nothing at all: nobody spoke,
    /// so there is nothing to have learned.
    ///
    /// Best-effort in both directions. A failed write is logged and the episode
    /// finishes: the decision is already durable in the transcript above it.
    async fn remember(&self, outcome: &EpisodeOutcome, lines: &[(EventSeq, String, String)]) {
        let task_line = self
            .task
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("(no task)");
        let cited = |marker: &str| -> Vec<String> {
            lines
                .iter()
                .filter(|(_, _, line)| line.trim_start().starts_with(marker))
                .map(|(seq, agent, line)| format!("- [{}] @{agent}: {line}", seq.value()))
                .collect()
        };
        let note = match &outcome.ending {
            EpisodeEnding::Idle => return,
            EpisodeEnding::Converged {
                topic, supporters, ..
            } => {
                let mut body = format!(
                    "Task: {task_line}\nCarried: #{topic}\nSupporters: {}\n",
                    if supporters.is_empty() {
                        "(none recorded)".to_owned()
                    } else {
                        supporters.join(", ")
                    },
                );
                let evidence = cited("!evidence");
                if !evidence.is_empty() {
                    body.push_str(&format!("Evidence:\n{}\n", evidence.join("\n")));
                }
                // The *last* pins, not every one: a pin is the room saying
                // "this outlives the window", and the later one supersedes the
                // earlier when both name the same thing.
                let mut pinned = cited("!pin");
                if pinned.len() > MAX_NOTE_PINS {
                    pinned.drain(..pinned.len() - MAX_NOTE_PINS);
                }
                if !pinned.is_empty() {
                    body.push_str(&format!("Pinned:\n{}\n", pinned.join("\n")));
                }
                let commits = cited("!commit");
                if let Some(last) = commits.last() {
                    body.push_str(&format!("Committed:\n{last}\n"));
                }
                HiveMemoryNote {
                    desk_id: self.desk.id.clone(),
                    title: format!("{task_line} — #{topic}"),
                    body,
                }
            }
            EpisodeEnding::Deadlocked { topics } => HiveMemoryNote {
                desk_id: self.desk.id.clone(),
                title: format!("Unresolved: {task_line}"),
                body: format!(
                    "Task: {task_line}\nUnresolved after {} turns: the desk deadlocked between \
                     {}. Nothing carried.\n",
                    outcome.turns,
                    topics
                        .iter()
                        .map(|topic| format!("#{topic}"))
                        .collect::<Vec<_>>()
                        .join(" and "),
                ),
            },
            EpisodeEnding::Exhausted => {
                let competing = self.competing_topics(lines);
                HiveMemoryNote {
                    desk_id: self.desk.id.clone(),
                    title: format!("Unresolved: {task_line}"),
                    body: format!(
                        "Task: {task_line}\nUnresolved after {} turns: the desk spent its budget. \
                         Options that competed: {competing}.\n",
                        outcome.turns,
                    ),
                }
            }
        };
        if let Err(error) = self.memory.remember(note).await {
            tracing::warn!(
                company = %self.company,
                desk = %self.desk.id,
                error = %error,
                "[hive] the episode's note could not be stored; the transcript still holds it"
            );
        }
    }

    /// Every `#topic` this episode's own lines named, in first-seen order.
    fn competing_topics(&self, lines: &[(EventSeq, String, String)]) -> String {
        let mut topics: Vec<String> = Vec::new();
        for (_, _, line) in lines {
            for word in line.split_whitespace() {
                if let Some(topic) = word.strip_prefix('#')
                    && !topic.is_empty()
                    && !topics.iter().any(|seen| seen == topic)
                {
                    topics.push(topic.to_owned());
                }
            }
        }
        if topics.is_empty() {
            "none were named".to_owned()
        } else {
            topics
                .iter()
                .map(|topic| format!("#{topic}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    /// Journal the closing row, under [`HIVE_REPORT_AUTHOR`].
    ///
    /// Best-effort: the decision itself is already durable in the turns above
    /// it, and losing the room's own summary of a conversation the transcript
    /// The audience to stamp on this turn's row: the addressees of an
    /// authorized aside, or empty for an ordinary desk-visible line.
    ///
    /// Every path that is not an authorized aside returns empty, and that is
    /// the safe direction: a line the room can read is never a leak, and the
    /// member has still said what it meant to say. A refusal is therefore
    /// logged rather than raised — the library gives every rung a name, and
    /// none of them is a reason to abandon an episode.
    ///
    /// The budget and the settlement debt are **folded from the transcript**
    /// rather than tracked across iterations, for the same reason the episode
    /// re-reads its transcript every turn: what the fold counts is exactly what
    /// a person reading the desk would see, so the two can never disagree.
    fn aside_audience(
        &self,
        agent_id: &str,
        line: &str,
        transcript: &[tinyhivemind_hive::SessionMessage],
        members: &[RosterMember],
        desks: &[Desk],
        retired: &[String],
    ) -> Vec<String> {
        if !super::aside::opens_aside(line) {
            return Vec::new();
        }
        let policy = self.desk.config.aside.policy();
        let roster = Roster::new(members, &[], retired);
        let desk_set = DeskSet::new(desks, &[], &[], &[], retired);

        let author = MentionAuthor::Agent {
            id: agent_id.to_owned(),
        };
        let mentions = tinyhivemind_hive::mention::resolve(line, None, &author, &roster, &desk_set);

        // The party this line *would* open, needed before the decision because
        // the budget and the settlement debt are per party. A `@#desk` or an
        // `@everyone` addresses nobody privately — the library refuses those
        // too, and this agrees with it rather than inventing a second rule.
        let mut party: Vec<String> = mentions
            .iter()
            .filter(|mention| !mention.quiet)
            .filter_map(|mention| match &mention.target {
                MentionTarget::Agent { id } => Some(id.clone()),
                _ => None,
            })
            .collect();
        party.push(agent_id.to_owned());
        party.sort();
        party.dedup();
        let (spent, unsettled) = super::aside::spent_and_unsettled(transcript, &party);

        let input = AsideInput {
            conversation: DispatchConversation {
                desk_id: self.desk.id.clone(),
                thread_root: self.thread_root.map(|seq| seq.value()),
            },
            author_id: agent_id.to_owned(),
            mentions,
            spent,
            unsettled,
        };

        match tinyhivemind_hive::aside::aside(policy, &input, &roster, &desk_set) {
            Ok(AsideDecision::One { audience }) => {
                let members = audience.members().to_vec();
                tracing::debug!(
                    company = %self.company,
                    desk = %self.desk.id,
                    agent = %agent_id,
                    audience = ?members,
                    "[hive] an aside was authorized"
                );
                members
            }
            Ok(AsideDecision::None { reason }) => {
                tracing::debug!(
                    company = %self.company,
                    desk = %self.desk.id,
                    agent = %agent_id,
                    reason = ?reason,
                    "[hive] no aside was authorized; the private row is dropped"
                );
                Vec::new()
            }
            Err(error) => {
                // A malformed snapshot is this host's bug, not the room's,
                // and it must not cost the episode. The private row is dropped
                // rather than published: the turn's own contribution is already
                // journaled where everyone can read it.
                tracing::warn!(
                    company = %self.company,
                    desk = %self.desk.id,
                    agent = %agent_id,
                    error = %error,
                    "[hive] the aside gate could not decide; the private row is dropped"
                );
                Vec::new()
            }
        }
    }

    /// still holds is not worth discarding the episode over.
    ///
    /// [`HIVE_REPORT_AUTHOR`]: super::HIVE_REPORT_AUTHOR
    async fn report(&self, outcome: &EpisodeOutcome, scope: &EpisodeScope) -> Option<EventSeq> {
        match self
            .events
            .append(
                &self.company,
                CompanyEvent::AgentReply {
                    chat_id: self.desk.id.clone(),
                    agent_id: super::HIVE_REPORT_AUTHOR.to_string(),
                    text: outcome.summary(),
                    // Always desk-visible, for the reason above.
                    audience: Vec::new(),
                    steps: Vec::new(),
                    task_id: None,
                    parent: self.thread_root,
                    mentions: Vec::new(),
                    mention_depth: 0,
                },
            )
            .await
        {
            Ok(seq) => {
                scope.record(seq);
                Some(seq)
            }
            Err(error) => {
                tracing::warn!(
                    company = %self.company,
                    desk = %self.desk.id,
                    error = %error,
                    "[hive] the episode ended but its outcome row could not be journaled; \
                     the transcript still holds every turn"
                );
                None
            }
        }
    }

    /// A library error, named as what it actually is on this host.
    fn malformed(&self, error: &dyn std::fmt::Display) -> OpenCompanyError {
        OpenCompanyError::Config(format!("hive episode on desk `{}`: {error}", self.desk.id))
    }
}

#[cfg(test)]
mod carried_proposal_test {
    use super::carried_proposal;
    use tinyhivemind_hive::{Sequence, SessionAuthor, SessionMessage};

    fn msg(seq: u64, content: &str) -> SessionMessage {
        SessionMessage {
            sequence: Sequence(seq),
            author: SessionAuthor::Agent {
                id: "engineer".to_string(),
                label: "Engineer".to_string(),
            },
            content: content.to_string(),
            elided: None,
            audience: tinyhivemind_hive::aside::Audience::Desk,
        }
    }

    #[test]
    fn takes_the_opening_proposal_and_strips_the_grammar() {
        let transcript = vec![
            msg(
                1,
                "thinking about this\n!propose #lazy-load each section, so the page only pays for what is opened",
            ),
            msg(2, "!support #lazy-load ^1 agreed"),
        ];
        assert_eq!(
            carried_proposal(&transcript, "lazy-load").as_deref(),
            Some("each section, so the page only pays for what is opened"),
            "the marker and topic are stripped, and prose above the move is ignored"
        );
    }

    /// `#lazy` must not answer for `#lazy-load`: a plain prefix match would
    /// report the wrong decision, which is worse than reporting none.
    #[test]
    fn a_topic_that_merely_prefixes_another_does_not_match() {
        let transcript = vec![msg(1, "!propose #lazy-load defer every section")];
        assert_eq!(carried_proposal(&transcript, "lazy"), None);
    }

    #[test]
    fn a_topic_never_proposed_in_the_window_is_absent() {
        let transcript = vec![msg(1, "!support #stage ^0 no proposal survives here")];
        assert_eq!(carried_proposal(&transcript, "stage"), None);
    }
    /// The live failure this rule exists for: two members filed opposite
    /// answers under one id named after the QUESTION, and the fold counted
    /// both `!propose` traces as backing the same topic — a reported quorum
    /// of two for a decision they disagreed about.
    #[test]
    fn a_second_proposal_under_one_id_is_caught() {
        let floor = vec![msg(
            1,
            "!propose #decide-one lazy-load each section because settings pages are single-purpose",
        )];
        assert_eq!(
            super::reuses_a_topic(
                "!propose #decide-one load everything up front, because sections are small forms",
                &floor,
            )
            .as_deref(),
            Some("decide-one"),
        );
        // Supporting it is the right move and is never corrected.
        assert_eq!(
            super::reuses_a_topic("!support #decide-one ^1 agreed", &floor),
            None
        );
        // A genuinely new option is not a reuse.
        assert_eq!(
            super::reuses_a_topic(
                "!propose #load-upfront one fetch, instant switching",
                &floor
            ),
            None
        );
    }

    /// `#lazy` and `#lazy-load` are different topics — a prefix match would
    /// correct a member for proposing something nobody had proposed.
    #[test]
    fn a_topic_that_merely_prefixes_another_is_not_a_reuse() {
        let floor = vec![msg(1, "!propose #lazy-load defer every section")];
        assert_eq!(
            super::reuses_a_topic("!propose #lazy do it later", &floor),
            None
        );
    }
}
