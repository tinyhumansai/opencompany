//! The host loop: fold, run the round of turns the library authorized, append
//! every one of them, commit the state it returned, repeat.
//!
//! That ordering is the whole contract. [`step`] never appends, never waits and
//! never calls back into this host; it hands back a `next_state` that is only
//! valid once **every** turn the round authorized is **durably** in the
//! journal. Committing it before the appends would let a failed write leave the
//! episode believing a turn happened that nothing can read back; committing it
//! part-way through a round would charge a threshold nobody spent.
//!
//! A round's turns are authorized to run *concurrently* and by construction
//! cannot read one another — each is projected against the single transcript
//! `step` decided on. This host runs them in series anyway, which the library
//! deliberately leaves open: a round says who may speak now, not how the host
//! should schedule them.
//!
//! Reading the transcript back out of the journal on every iteration — rather
//! than accumulating it in memory as the loop runs — is the same discipline
//! seen from the other side: what the room folds is exactly what a human
//! reading the desk would see, so the standings can never disagree with the
//! transcript they were derived from.

use std::sync::Arc;

use async_trait::async_trait;
use tinyhivemind_hive::{
    Conversation, EpisodeState, SESSION_WINDOW, Sequence, SessionQuery,
    aside::{AsideDecision, AsideInput, Viewer},
    desk::{Desk, DeskSet, ResponderMode},
    dispatch::DispatchConversation,
    mention::{MentionAuthor, MentionTarget},
    pins::{PIN_LIMIT, read_pinboard},
    project_for,
    roster::{Roster, RosterMember},
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
pub(super) fn carried_proposal(
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
    /// A semantic router for `!broadcast`, when this instance has one.
    ///
    /// `None` is routing off, not a failure: a broadcast still lands as a desk
    /// row and still assigns, it just falls to the mechanical responder instead
    /// of being routed by meaning.
    #[cfg(feature = "typesafe")]
    router: Option<std::sync::Arc<dyn tinyhivemind_embed::routing::Router + Send + Sync>>,
    /// Run this episode on explicit completion reports rather than on quorum.
    completion: bool,
    /// Who the episode opens assigned to, when it is completion-driven.
    ///
    /// Empty means the desk's first member, which is the mechanical responder
    /// and what this desk answered with before routing existed. A caller that
    /// has routed the opening message supplies its recipients instead.
    opening: Vec<String>,
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

/// Whether two lines say the same thing, for the continuation guard.
///
/// Collapsed whitespace and nothing cleverer. The failure this catches is a
/// seat re-emitting its own committed line verbatim, so anything fuzzier would
/// risk swallowing a real conclusion that happens to quote the question.
fn same_line(one: &str, two: &str) -> bool {
    let words = |text: &str| {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    words(one) == words(two)
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
            #[cfg(feature = "typesafe")]
            router: None,
            completion: false,
            opening: Vec::new(),
        }
    }

    /// Open a completion-driven episode assigned to `ids`.
    ///
    /// Only these owe a `!complete`. Every other seat is seeded finished and
    /// becomes pending only if a routed handoff assigns it — so a room ends
    /// when the work is done, not when every chair has spoken.
    #[must_use]
    pub fn assigned_to(mut self, ids: Vec<String>) -> Self {
        self.opening = ids;
        self
    }

    /// Who should take one `!broadcast`, decided by meaning.
    ///
    /// Falls back to the **mechanical** responder — this desk's first other
    /// member — whenever routing declines: no credential configured, a transport
    /// outage, a stale roster, a malformed evaluation. The worst case of routing
    /// by meaning is therefore the routing this desk did before it existed,
    /// which is the same direction `built_in::selector` takes.
    ///
    /// The author is excluded here **and** re-checked by the library: a message
    /// routed back to its own author is a loop, and the library fails it closed
    /// without spending a model call.
    #[cfg(feature = "typesafe")]
    async fn route_handoff(
        &self,
        author: &str,
        work: &str,
        members: &[RosterMember],
        _retired: &[String],
    ) -> Vec<String> {
        let candidates: Vec<_> = members
            .iter()
            .filter(|member| member.id != author)
            .map(|member| {
                super::broadcast::candidate(
                    &member.id,
                    member.name.as_deref().unwrap_or(&member.id),
                    self.desk
                        .members
                        .iter()
                        .find(|seat| seat.id == member.id)
                        .map(|seat| seat.role.clone()),
                    None,
                )
            })
            .collect();
        let mechanical = candidates
            .first()
            .map(|candidate| candidate.id.clone())
            .unwrap_or_else(|| author.to_owned());
        if candidates.is_empty() {
            return Vec::new();
        }
        let Some(router) = self.router.as_deref() else {
            // Routing off: the handoff still lands, mechanically.
            return vec![mechanical];
        };
        super::broadcast::route(
            router,
            super::broadcast::Broadcast {
                message: work,
                author,
                desk_id: &self.desk.id,
                desk_purpose: self.desk.description.clone(),
                candidates,
                roster_version: members.len() as u64,
                fallback_responder: &mechanical,
            },
        )
        .await
    }

    /// Without the `typesafe` feature there is no router to ask, so a handoff
    /// goes to the desk's first other member and costs no model call.
    #[cfg(not(feature = "typesafe"))]
    #[expect(
        clippy::unused_async,
        reason = "one signature for both builds; the routed arm is genuinely async"
    )]
    async fn route_handoff(
        &self,
        author: &str,
        _work: &str,
        members: &[RosterMember],
        _retired: &[String],
    ) -> Vec<String> {
        members
            .iter()
            .find(|member| member.id != author)
            .map(|member| vec![member.id.clone()])
            .unwrap_or_default()
    }

    /// Terminate on explicit `!complete` reports instead of on quorum.
    ///
    /// The mode the reference runner uses: every seat it gives an agent is
    /// `broadcast` or `complete_episode`, and the room ends when each assigned
    /// member has reported. Well-defined at one assignee, which quorum is not.
    #[must_use]
    pub const fn completing(mut self) -> Self {
        self.completion = true;
        self
    }

    /// Route `!broadcast` handoffs through `router` rather than to the desk's
    /// mechanical responder.
    #[cfg(feature = "typesafe")]
    #[must_use]
    pub fn with_router(
        mut self,
        router: Option<std::sync::Arc<dyn tinyhivemind_embed::routing::Router + Send + Sync>>,
    ) -> Self {
        self.router = router;
        self
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
        // Whether a crossing THIS desk makes convenes the far desk as a room or
        // asks a single seat. The prompt described the room unconditionally,
        // which is wrong for a desk that set `deliberates = false` and gets the
        // single-responder crossing (CodeRabbit, #2341).
        let deliberates = self.desk.config.referral.deliberates();
        // Held separately because the referral block below binds its own
        // `policy` (a `ReferralPolicy`), and the continuation inside it still
        // renders a prompt for THIS room.
        let episode_quorum = policy.quorum;

        // The loop holds no episode state of its own any more: the scheduler
        // owns it, which is what lets a second termination share this loop.
        // Two terminations, one loop. Which is chosen is the whole of what the
        // scheduler seam buys: everything below reads the `HiveTurn` it hands
        // back and never the state behind it.
        let mut scheduler: Box<dyn super::schedule::Scheduler> = if self.completion {
            Box::new(super::schedule::Completion::new(
                {
                    let roster: Vec<String> =
                        members.iter().map(|member| member.id.clone()).collect();
                    // Falls to the desk's first member, which is the mechanical
                    // responder this desk used before any of this existed.
                    let opening = if self.opening.is_empty() {
                        roster.first().cloned().into_iter().collect()
                    } else {
                        self.opening.clone()
                    };
                    super::completion::opened(
                        conversation.clone(),
                        Sequence(trigger.value()),
                        &roster,
                        &opening,
                    )?
                },
                policy.turn_budget,
            ))
        } else {
            Box::new(super::schedule::Quorum::new(
                EpisodeState::opened(conversation.clone(), Sequence(trigger.value())),
                policy,
            ))
        };
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
                    &self.desk.config.referral,
                    Arc::clone(&scope),
                ),
                self.desk.config.referral.policy(),
            )
        });
        // The peer list every prompt of this episode renders. Built once for
        // the same reason the recall is: it cannot change mid-episode, and a
        // seat that saw a different set of desks from the seat before it would
        // be reading a different company.
        // **Can this seat actually get an answer, not just is federation wired.**
        //
        // `referrals` is `Some` whenever a federation exists, but `consider`
        // returns immediately on a policy that is not `enabled` — and `enabled`
        // defaults to OFF. Deriving the prompt's capability from `is_some()`
        // therefore promised every seat in a federated company that writing an
        // `@handle` would be answered "at once", for a question that was
        // silently dropped (CodeRabbit, #2341).
        let can_ask = referrals
            .as_ref()
            .is_some_and(|(_, _, policy)| policy.enabled);
        // Carries each peer desk's members, because the list is narrowed PER
        // SEAT below and this snapshot is the only place the federation is
        // read. Taken once for the episode for the reason `HiveFederation`
        // itself is a snapshot: a company whose desks changed mid-episode would
        // offer later turns a roster the earlier ones never saw.
        let peers: Vec<(String, String, Option<String>, Vec<String>)> = referrals
            .as_ref()
            .filter(|(_, _, policy)| policy.enabled && policy.reach.addresses_desks())
            .map(|(federation, _, _)| {
                federation
                    .peers_of(&self.desk.id)
                    .into_iter()
                    // The company line is not a peer to ask. Every seat is on
                    // it, so the membership filter below admits it for
                    // everybody — and being offered the room you are already
                    // sitting in is a move that spends the asking room's turn
                    // and can return nothing it did not already hold.
                    .filter(|desk| federation.general_desk.as_deref() != Some(desk.id.as_str()))
                    .map(|desk| {
                        (
                            desk.id.clone(),
                            desk.name.clone(),
                            desk.description.clone(),
                            desk.members.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        // **What ONE seat may ask, not what the desk may ask.**
        //
        // The same list used to go to every member of the room, because it was
        // built from the desk's peers and cloned per turn. A seat was therefore
        // offered desks it does not sit on, cannot read, and gets nothing back
        // from but a single report line — which is the whole of the containment
        // question: membership is what makes a crossing's answer legible.
        let peers_for = |agent: &str| -> Vec<(String, String, Option<String>)> {
            peers
                .iter()
                .filter(|(_, _, _, members)| members.iter().any(|member| member == agent))
                .map(|(id, name, about, _)| (id.clone(), name.clone(), about.clone()))
                .collect()
        };

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
                scheduler.next(&transcript, &roster, &desk_set)?
            };

            // Two outcomes, not five: which quorum shape ended the room is the
            // scheduler's vocabulary and it renders its own ending. The loop
            // only needs turns to run, or a reason to stop.
            let round = match decision {
                super::schedule::Scheduled::Round(turns) => turns,
                super::schedule::Scheduled::Ended(ending) => break ending,
            };

            // A round authorizes several members to speak *together*, so each
            // of its turns is projected against the one transcript `step`
            // decided on and none of them can read another's row —
            // `project_for` withholds any peer row above `round_start`.
            // Running them in series is a scheduling choice the library leaves
            // to the host; what it does not leave open is the commit, which
            // belongs to the round and lands once, below.
            let round_len = u32::try_from(round.len()).unwrap_or(u32::MAX);
            for turn in round {
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
                    // Bounded at the round, for the same reason `project_for`
                    // withholds a peer row above `round_start`: a pin a
                    // round-mate laid down was written *concurrently* with this
                    // turn, so it must no more reach this prompt than that
                    // member's row reaches the transcript. Without the bound
                    // this host runs the round in series and quietly leaks it —
                    // the transcript path stays honest while the board does not.
                    // A pin from an earlier round sits below the boundary and
                    // still arrives; at width one nothing is above it at all.
                    //
                    // `+ 1` because the two bounds are not the same shape.
                    // `readable` keeps a row **at** the boundary
                    // (`sequence <= round_start`) while `EventLog::read_before`
                    // is strictly exclusive (`seq < before`), so passing
                    // `round_start` unchanged would hide the boundary row from
                    // the board alone — a `!pin` on the last row of the
                    // previous round would reach the transcript and not the
                    // prompt, and would only reappear once some later row moved
                    // the boundary past it.
                    Some(Sequence(turn.round_start.0.saturating_add(1))),
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
                let unspoken: Vec<String> =
                    if last_speaker.as_deref() == Some(turn.agent_id.as_str()) {
                        self.desk
                            .member_ids()
                            .into_iter()
                            .filter(|id| !spoken.contains(id))
                            .collect()
                    } else {
                        Vec::new()
                    };
                let elsewhere = self.elsewhere_for(&turn.agent_id, turn.round_start).await;
                let prompt =
                    EpisodePrompt::new(member, &self.desk, &self.task, policy.quorum, &pins)
                        .with_recall(&recall)
                        .with_elsewhere(&elsewhere)
                        .with_unspoken(&unspoken)
                        .with_peers(peers_for(&turn.agent_id))
                        .desks_deliberate(deliberates)
                        .able_to_ask(can_ask)
                        .with_trigger(Sequence(trigger.value()))
                        .completing(scheduler.completing())
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
                                    outputs: Vec::new(),
                                    parent: self.thread_root,
                                    mentions: Vec::new(),
                                    mention_depth: 0,
                                },
                            )
                            .await?;
                        scope.record(seq);
                        last_seq = Some(seq);
                        // The failure note is this turn's durable row, so the turn
                        // is appended and the budget advances for it — but the
                        // state belongs to the *round*, and is taken up once below
                        // rather than part-way through.
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
                            outputs: Vec::new(),
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
                if let Some(aside_seq) = self
                    .journaled_aside(
                        &turn.agent_id,
                        &mut scratch,
                        &transcript,
                        &members,
                        &desks,
                        &retired,
                    )
                    .await?
                {
                    scope.record(aside_seq);
                    last_seq = Some(aside_seq);
                }
                // **The asker's continuation, held until its own line is
                // recorded.** `lines` is documented as every line this episode
                // journaled *in order*, and `remember` depends on that —
                // `commits.last()` takes the final `!commit` and the pin trim
                // keeps the LAST pins. Pushed from inside the referral block, a
                // continuation landed ahead of the line it continues, so a
                // higher sequence sat earlier in the vec (CodeRabbit, #2332).
                let mut continuation: Option<(EventSeq, String, String)> = None;
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
                    // Counted before and after, because the ledger is the
                    // host's own record of which crossings actually ran a turn
                    // on another desk. A crossing that FAILED also journals a
                    // `hive-referral` row — "did not answer the question" — so
                    // recognising an answer by its author would read a failure
                    // notice as an answer and hand the asker a second turn to
                    // respond to nothing. `asked` grows only on a real one;
                    // `failed` and `declined` are counted apart from it.
                    let answered_before = queue.ledger().await.asked.len();
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
                    // **Continue only on an answer this seat can actually
                    // read.**
                    //
                    // Counting successful far-desk turns is not that. A desk
                    // with `returns = false` still records the question on the
                    // ledger, but `consider` exits before journaling anything
                    // on THIS desk — so the count grew while the transcript
                    // gained nothing, and the continuation ran with a prompt
                    // announcing an answer that was not there, free to commit a
                    // decision on information its speaker never received
                    // (Codex, #2332).
                    //
                    // `returned || !crossed`, not `returned` alone: a question
                    // put to a peer on this same desk never needs carrying back
                    // — `forward` appends the answer to the very conversation
                    // the asker is reading — so that continuation is legitimate
                    // and gating on `returned` would suppress it.
                    let (answered, held_in) = {
                        let ledger = queue.ledger().await;
                        let last = ledger.asked.last();
                        (
                            ledger.asked.len() > answered_before
                                && last.is_some_and(|asked| asked.returned || !asked.crossed),
                            last.and_then(|asked| {
                                asked
                                    .conversation
                                    .clone()
                                    .map(|thread| (thread, asked.opened, asked.target.clone()))
                            }),
                        )
                    };
                    // **The exchange the asker is about to speak on, when it
                    // happened somewhere the asker cannot otherwise read.**
                    //
                    // A question put to a person by name runs in the pair's own
                    // thread. Nothing puts that thread in front of either
                    // participant again — `elsewhere_for` gathers a seat's other
                    // desks and its own direct line, and a `dm:<a>+<b>` pair is
                    // neither — so the pair agreed on something and the room
                    // watched both seats re-derive it in the open, line by line.
                    //
                    // Handed to the continuation as `elsewhere` because that is
                    // exactly what it is: rows from outside this fold, which the
                    // asker may quote and which move no option here. The desk
                    // record stays private; what reaches the floor is whatever
                    // the asker chooses to say in its own words.
                    //
                    // Bounded at the row this exchange opened on. The pair key
                    // is deterministic, so two agents reuse one thread for
                    // every crossing they have; unbounded, the asker was handed
                    // every past exchange with that teammate as though it were
                    // the answer it had just received (tinysweeper, #2341).
                    let mut carried = elsewhere.clone();
                    if let Some((thread, opened, other)) = held_in.as_ref()
                        && answered
                        && let Some(rows) = self
                            .conversation_rows(thread, &turn.agent_id, *opened)
                            .await
                    {
                        // Labelled by WHO, not by where. `thread` is the pair's
                        // storage key — `dm:<a>+<b>` — and interpolating it put
                        // "Your exchange with @dm:planner+sre" in front of the
                        // model, naming an internal id as though it were a
                        // colleague's handle.
                        //
                        // Merged into the section that label already names,
                        // rather than opening a second one beside it: now that
                        // `elsewhere_for` carries this seat's pair threads, an
                        // earlier exchange with the SAME teammate is already
                        // here under this exact label, and two sections with
                        // one name is the ambiguity `elsewhere` renders per
                        // conversation to avoid. Sorted and deduplicated for
                        // the same reason the direct line's two spellings are.
                        let label = format!("Your exchange with @{other}");
                        if let Some(existing) =
                            carried.iter_mut().find(|(existing, _)| *existing == label)
                        {
                            existing.1.extend(rows);
                            existing.1.sort_by_key(|message| message.sequence);
                            existing.1.dedup_by_key(|message| message.sequence);
                        } else {
                            carried.push((label, rows));
                        }
                    }
                    // **The asker's turn continues on the answer it paid for.**
                    //
                    // Landing the answer before the *next* speaker is chosen
                    // stops the room voting past it, but it still leaves the one
                    // member that spent its turn asking unable to use what came
                    // back: its line was the question, and the reply is read by
                    // whoever speaks after. Observed live on
                    // `companies/retail_co` — `cancellations` asked `@#returns`,
                    // the answer came home two rows later, and the room spent
                    // seven of eleven turns re-asking a question that had
                    // already been answered.
                    //
                    // So when a crossing answered, the same member speaks again
                    // with it in context: one turn, two rows, which is the shape
                    // ADR 0011 already accepted for an aside riding alongside a
                    // move. The room's budget is untouched — `turns` counts the
                    // round, not the rows — and a second pass is taken at most
                    // once, so a continuation that asks again cannot regress.
                    if let Some(transcript) = self.refolded(&conversation, answered, &scope).await {
                        let visible = project_for(&turn, &transcript);
                        let prompt = EpisodePrompt::new(
                            member,
                            &self.desk,
                            &self.task,
                            episode_quorum,
                            &pins,
                        )
                        .with_recall(&recall)
                        .with_elsewhere(&carried)
                        .with_unspoken(&unspoken)
                        .with_peers(peers_for(&turn.agent_id))
                        .desks_deliberate(deliberates)
                        .able_to_ask(can_ask)
                        .with_trigger(Sequence(trigger.value()))
                        .continuing()
                        .completing(scheduler.completing())
                        .render(&turn, &visible);
                        scratch.aside = None;
                        match self
                            .line_from(&turn.agent_id, &prompt, &visible, &mut scratch)
                            .await
                        {
                            // **A continuation that repeats the line is not a
                            // second row.**
                            //
                            // The prompt now says what a continuation is for
                            // (`EpisodePrompt::continuing`), which is the fix;
                            // this is the guarantee. A seat that answers with
                            // the line it already committed has added nothing,
                            // and journaling it put the identical row on the
                            // desk twice — visible in the console as one member
                            // saying the same thing back to back, the second
                            // copy carrying the crossing.
                            //
                            // Compared on collapsed whitespace, because that is
                            // the whole of the observed difference: a genuinely
                            // new conclusion is not a whitespace variant of the
                            // question that earned it.
                            Ok(second) if same_line(&second, &line) => {
                                tracing::debug!(
                                    company = %self.company,
                                    desk = %self.desk.id,
                                    agent = %turn.agent_id,
                                    "[hive] a crossing's continuation repeated the line it \
                                     already committed; not journaled"
                                );
                            }
                            // **A continuation naming another desk is still
                            // journaled, and still not dispatched.**
                            //
                            // Codex (#2332) is right that this leaves a
                            // `!question @#desk` on the transcript that nothing
                            // acts on. Both remedies measured WORSE than the
                            // defect, live on `companies/retail_co`:
                            //
                            // - Refusing such a continuation converged (1/1) but
                            //   discarded the conclusion this pass exists for.
                            //   The library's `candidate` does not read the move
                            //   kind, so the ordinary idiom on an answered desk
                            //   — `!support … @#returns confirmed …` — reads as a
                            //   crossing attempt and was thrown away with it.
                            // - Dispatching it (calling `consider` on this line)
                            //   let the continuation spend a crossing, so
                            //   `peer_cap` was gone before a later turn needed
                            //   it and the asking desk re-supported itself to
                            //   its turn budget: converged 3/3 before, 0/2
                            //   after.
                            //
                            // So the row stays. An in-room `!question` is never
                            // dispatched anywhere either; the cost here is a
                            // reader inferring a crossing that did not happen,
                            // which is smaller than a desk that reaches no
                            // decision at all. Letting a continuation ask
                            // WITHOUT charging the episode's crossing budget is
                            // the real fix, and it changes that budget, so it is
                            // its own change.
                            Ok(second) => {
                                let second_seq = self
                                    .events
                                    .append(
                                        &self.company,
                                        CompanyEvent::AgentReply {
                                            chat_id: self.desk.id.clone(),
                                            agent_id: turn.agent_id.clone(),
                                            text: second.clone(),
                                            audience: Vec::new(),
                                            steps: Vec::new(),
                                            task_id: None,
                                            outputs: Vec::new(),
                                            parent: self.thread_root,
                                            mentions: Vec::new(),
                                            mention_depth: 0,
                                        },
                                    )
                                    .await?;
                                scope.record(second_seq);
                                last_seq = Some(second_seq);
                                // **A continuation may ride an aside too.**
                                //
                                // `line_from` puts one in `scratch.aside`
                                // whichever pass produced it, and only the
                                // first pass was draining it — so an aside a
                                // continuation authorized was cleared by the
                                // next turn and lost (CodeRabbit, #2332). Same
                                // helper, so the authorize-or-drop rule has one
                                // implementation rather than two.
                                //
                                // Never into `lines`: an aside is not a
                                // desk-visible contribution and the fold must
                                // not read it as one.
                                if let Some(aside_seq) = self
                                    .journaled_aside(
                                        &turn.agent_id,
                                        &mut scratch,
                                        &transcript,
                                        &members,
                                        &desks,
                                        &retired,
                                    )
                                    .await?
                                {
                                    scope.record(aside_seq);
                                    last_seq = Some(aside_seq);
                                }
                                continuation = Some((second_seq, turn.agent_id.clone(), second));
                            }
                            // The question and its answer are already durable,
                            // and the room can read both. A continuation that
                            // did not finish costs the episode nothing it did
                            // not already have.
                            Err(error) => tracing::warn!(
                                company = %self.company,
                                desk = %self.desk.id,
                                agent = %turn.agent_id,
                                error = %error,
                                "[hive] the asker could not answer on its crossing; the room reads it regardless"
                            ),
                        }
                    }
                }
                // **The one place semantic routing is actually spent.**
                //
                // A `!broadcast` hands work on without naming who takes it, so
                // the host decides by meaning. The row is already appended above
                // — the message is both the desk row and what the Choice matches
                // against — and only the *assignment* happens here.
                //
                // Costs a second model call, which is why nothing else on this
                // path routes: a seat that already knows who should go next
                // writes `@id` and pays nothing.
                // Gated on the scheduler, not just on the marker. A quorum room
                // picks its own next speaker through the attention market, so a
                // routed recipient has nowhere to land — `Quorum::assign` is a
                // no-op — and routing one anyway would spend a provider call to
                // discard the answer. A handoff in a deliberating room is an
                // ordinary desk row the fold already reads.
                if scheduler.completing()
                    && let Some(work) = super::completion::reply_broadcast(&line)
                {
                    let recipients = self
                        .route_handoff(&turn.agent_id, work, &members, &retired)
                        .await;
                    if !recipients.is_empty() {
                        scheduler.assign(&recipients, Sequence(seq.value()));
                    }
                }
                lines.push((seq, turn.agent_id.clone(), line));
                // After its own line, never before: see the declaration above.
                if let Some(second) = continuation {
                    lines.push(second);
                }
                if !spoken.contains(&turn.agent_id) {
                    spoken.push(turn.agent_id.clone());
                }
                last_speaker = Some(turn.agent_id.clone());
            }
            // Every turn the round authorized is now durably appended — a
            // failed one as a system row — so, and only now, the state the
            // library returned may be taken up. Committing after a subset
            // would charge a threshold nobody spent.
            scheduler.commit();
            turns = turns.saturating_add(round_len);
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
    /// The transcript re-folded, once a crossing this member opened has been
    /// answered.
    ///
    /// `answered` is read off the referral ledger rather than off the rows: a
    /// crossing that *failed* journals a `hive-referral` notice too ("did not
    /// answer the question"), and recognising an answer by its author would
    /// hand the asker a second turn to respond to nothing — which is exactly
    /// what `a_far_turn_that_does_not_finish_leaves_the_room_running` caught.
    /// `ReferralLedger::asked` counts only the crossings that ran a turn
    /// somewhere else; `failed` and `declined` are counted apart from it.
    ///
    /// `None` when nothing answered, and the turn ends where it always did.
    /// Authorize and journal whatever aside a turn's reply carried, returning
    /// the sequence it landed on.
    ///
    /// One implementation, because both passes of a turn can produce one: the
    /// member's own line, and the continuation it takes on a crossing's answer.
    /// Only the first was draining `scratch.aside`, so an aside a continuation
    /// authorized was cleared by the next turn and lost (CodeRabbit, #2332).
    ///
    /// **A refused audience is dropped, not published.** Falling back to the
    /// desk would put a second desk-visible contribution on one turn, which is
    /// the one thing a turn may not produce — and the member has already said
    /// its piece in the row above.
    ///
    /// Never returned into `lines`: an aside is not a desk-visible
    /// contribution and the fold must not count it as one.
    async fn journaled_aside(
        &self,
        agent_id: &str,
        scratch: &mut TurnScratch,
        transcript: &[tinyhivemind_hive::SessionMessage],
        members: &[RosterMember],
        desks: &[Desk],
        retired: &[String],
    ) -> Result<Option<EventSeq>> {
        let Some(aside_line) = scratch.aside.take() else {
            return Ok(None);
        };
        let audience =
            self.aside_audience(agent_id, &aside_line, transcript, members, desks, retired);
        if audience.is_empty() {
            tracing::debug!(
                company = %self.company,
                desk = %self.desk.id,
                agent = %agent_id,
                "[hive] an aside was not authorized; the row was dropped"
            );
            return Ok(None);
        }
        let seq = self
            .events
            .append(
                &self.company,
                CompanyEvent::AgentReply {
                    chat_id: self.desk.id.clone(),
                    agent_id: agent_id.to_owned(),
                    text: aside_line,
                    audience,
                    steps: Vec::new(),
                    task_id: None,
                    outputs: Vec::new(),
                    parent: self.thread_root,
                    mentions: Vec::new(),
                    mention_depth: 0,
                },
            )
            .await?;
        Ok(Some(seq))
    }

    /// One conversation's recent rows, projected for a seat that was in it.
    ///
    /// Used for a pair thread the asker cannot otherwise read: its rows live in
    /// `dm:<a>+<b>`, which is neither a desk this seat sits on nor its own
    /// direct line, so `elsewhere_for` never gathers it.
    ///
    /// `None` when the projection fails or the thread is empty, which the
    /// caller treats as nothing to carry — the same best-effort stance
    /// `elsewhere_for` takes.
    async fn conversation_rows(
        &self,
        conversation: &str,
        viewer_id: &str,
        // The row this exchange opened on. A pair thread is keyed on its two
        // members and nothing else, so without this the projection returns
        // every crossing those two have ever had inside `SESSION_WINDOW`.
        since: Option<u64>,
    ) -> Option<Vec<tinyhivemind_hive::SessionMessage>> {
        let log = EventLogSessionLog::new(
            Arc::clone(&self.events),
            self.company.clone(),
            conversation.to_string(),
            conversation.to_string(),
        );
        let rows = tinyhivemind_hive::project_session(
            &log,
            &SessionQuery {
                conversation: Conversation {
                    desk_id: conversation.to_string(),
                    desk_name: conversation.to_string(),
                    thread_root: None,
                },
                viewer: Viewer::Agent {
                    id: viewer_id.to_string(),
                },
                before: None,
                window: SESSION_WINDOW,
            },
        )
        .await
        .ok()?;
        // Inclusive: `since` IS the question that opened the exchange, and an
        // exchange rendered without it starts at the answer.
        let rows: Vec<_> = match since {
            Some(from) => rows
                .into_iter()
                .filter(|row| row.sequence.0 >= from)
                .collect(),
            None => rows,
        };
        (!rows.is_empty()).then_some(rows)
    }

    async fn refolded(
        &self,
        conversation: &Conversation,
        answered: bool,
        // **This episode's own fold boundary.** The desk and thread filters do
        // not separate two episodes that share them, so an unscoped refold
        // admits a concurrent episode's rows and `EpisodePrompt::standings`
        // folds them — showing the continuation supporter counts belonging to
        // somebody else's room. The main loop scopes its log for exactly this
        // reason; this one was built without it (CodeRabbit, #2332).
        scope: &Arc<EpisodeScope>,
    ) -> Option<Vec<tinyhivemind_hive::SessionMessage>> {
        if !answered {
            return None;
        }
        let log = EventLogSessionLog::new(
            Arc::clone(&self.events),
            self.company.clone(),
            self.desk.id.clone(),
            self.desk.name.clone(),
        )
        .with_scope(Arc::clone(scope));
        tinyhivemind_hive::project_session(
            &log,
            &SessionQuery {
                conversation: conversation.clone(),
                viewer: Viewer::Operator,
                before: None,
                window: SESSION_WINDOW,
            },
        )
        .await
        .ok()
    }

    async fn line_from(
        &self,
        agent_id: &str,
        prompt: &str,
        visible: &[tinyhivemind_hive::SessionMessage],
        scratch: &mut TurnScratch,
    ) -> Result<String> {
        let allowed = self.desk.config.moves_for(agent_id);
        let (line, rode) = split_reply(&self.runner.speak(agent_id, prompt).await?);

        // **A marker carrying nothing is a malformed move, not a message.**
        //
        // Observed live: a seat wrote its work as one reply and then `!broadcast`
        // alone as a second. A bare marker routes nobody — `broadcast_body`
        // finds no work for the Choice to match — and reports nothing, so it is
        // a turn that looks taken and did nothing.
        //
        // Corrected rather than dropped, on the crate's own rule for a
        // malformed tool call: the seat is told inside its own turn, "while it
        // can still call again". Dropping would hide the failure from the room
        // and from the operator; correcting gets the hand-off the turn was for.
        let (line, rode) =
            if let Some(correction) = super::completion::bare_marker_correction(&line) {
                let corrected = format!("{prompt}\n\n{correction}");
                // The retry stands even if it is bare again: a second empty marker
                // is the seat's answer, and journaling it keeps the transcript
                // honest about a turn that happened. It still surfaces — the row is
                // the member's reply either way.
                split_reply(&self.runner.speak(agent_id, &corrected).await?)
            } else {
                (line, rode)
            };
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
        round_start: Sequence,
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
        // The direct line carries TWO targets, not one (Codex P2): an operator
        // DM is journaled under the bare agent id ordinarily, but under the
        // `dm:<agent-id>` spelling whenever the bare one collides with a desk id
        // or names a General spelling (issue
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
        // A bare direct-line target is only safe when it cannot also name a
        // declared desk. `addresses_desk` accepts either spelling, so querying
        // the bare id for an unrelated desk with the same id/name leaks that
        // desk's rows into this agent's prompt. DMs written after the collision
        // guard use the prefixed spelling below.
        let bare_direct_line_is_safe = self
            .context_desks
            .iter()
            .all(|desk| desk.id != agent_id && desk.name != agent_id);
        // **Every private line this seat holds, not just the one with the
        // operator.**
        //
        // A pair thread is `dm:<a>+<b>` — neither a desk nor either
        // participant's own direct line — so it matched none of the targets
        // above and no agent ever read one again. The asker got the exchange it
        // had just had, on the single turn straight after it, and that was all:
        // the teammate who ANSWERED never saw it again, and neither side
        // retained anything from a previous one. Two seats could settle
        // something on Monday and rediscover it from scratch on Tuesday, which
        // is exactly what a live retail run did — agents re-asking each other
        // about capabilities they had already exchanged.
        //
        // Deterministic keys make this enumerable rather than searchable:
        // `pair_conversation` sorts the two ids, so the thread this seat shares
        // with any teammate is computable without an index. A thread that never
        // happened simply projects empty and is skipped by the loop below.
        //
        // Only threads this seat is IN: every key is built from `agent_id`
        // itself, so a pair between two other people is not addressable here.
        // The viewer narrowing below is the second line of that defence.
        let mut partners: Vec<String> = self
            .context_desks
            .iter()
            .flat_map(|desk| desk.members.iter())
            .filter(|member| *member != agent_id)
            .cloned()
            .collect();
        partners.sort();
        partners.dedup();
        let pairs: Vec<(String, String, String)> = partners
            .into_iter()
            .map(|other| {
                let thread = super::referral::pair_conversation(agent_id, &other);
                (
                    thread.clone(),
                    thread,
                    format!("Your exchange with @{other}"),
                )
            })
            .collect();
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
            .chain(bare_direct_line_is_safe.then(|| {
                (
                    agent_id.to_string(),
                    agent_id.to_string(),
                    direct_line_label.clone(),
                )
            }))
            .chain(std::iter::once((
                format!("{}{agent_id}", crate::runtime::assignee::DM_PREFIX),
                format!("{}{agent_id}", crate::runtime::assignee::DM_PREFIX),
                direct_line_label,
            )))
            .chain(pairs)
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
                    // Frozen at the round boundary, for the reason the pinboard
                    // read above is. These are *other* desks, but a round-mate
                    // can reach them within this round — a synchronous desk
                    // referral journals its answer on the target desk before
                    // this loop reaches the next member — and an unbounded read
                    // would hand that answer to a speaker who, by the round's
                    // definition, cannot have read it. Left unbounded, what a
                    // member sees would depend on the host's serial iteration
                    // order, which is the one thing a round is supposed to make
                    // irrelevant. `+ 1` for the same off-by-one as the board:
                    // `before` is an exclusive upper bound and `round_start` is
                    // the last row the round may see.
                    before: Some(Sequence(round_start.0.saturating_add(1))),
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
            EpisodeEnding::Completed { completed } => HiveMemoryNote {
                desk_id: self.desk.id.clone(),
                title: format!("Finished: {task_line}"),
                body: format!(
                    "Task: {task_line}\nCompleted after {} turns by: {}.\n",
                    outcome.turns,
                    if completed.is_empty() {
                        "(nobody recorded)".to_owned()
                    } else {
                        completed.join(", ")
                    },
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
                    outputs: Vec::new(),
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
#[path = "episode_carried_proposal_tests.rs"]
mod carried_proposal_tests;
