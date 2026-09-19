//! Who speaks next, and when the room is done.
//!
//! [`super::episode`]'s loop does one thing per pass: ask what to do, run the
//! turns it is given, append every row, then take up the new state. Only the
//! *asking* is specific to how a room ends — the ~580 lines that build each
//! member's prompt, withhold a round-mate's row, and journal a failed turn are
//! the same however the room terminates.
//!
//! This is that ask, behind a trait, so a second way of ending a room can exist
//! without a second copy of the loop.
//!
//! # Why not just return `HiveStep`
//!
//! [`tinyhivemind_hive::HiveStep`] is what the *quorum fold* says:
//! `Speak | Converged | Deadlocked | Exhausted | Idle`, with `Speak` carrying a
//! `Box<EpisodeState>`. A completion-driven room finishing has none of those
//! shapes — it is not converged, deadlocked, exhausted or idle — and would have
//! to manufacture a quorum state, with standings and a budget nothing reads,
//! purely to satisfy the type.
//!
//! [`Scheduled`] is what the *loop* needs to hear, which is only ever two
//! things: here are turns, or stop and here is why. Today those coincide, which
//! is why no name was needed until now.
//!
//! # Why `commit` is separate from `next`
//!
//! Because the loop commits **after** every row is durably appended, and that
//! ordering is load-bearing — the episode's own comment says why: *"Committing
//! after a subset would charge a threshold nobody spent."* A scheduler that
//! folded its new state inside [`Scheduler::next`] would charge the round before
//! anybody spoke, so the state a pass produced is held and taken up only when
//! [`Scheduler::commit`] is called.

use tinyhivemind_hive::{
    EpisodeState, HiveStep, HiveTurn, SessionMessage, desk::DeskSet, episode::EpisodePolicy,
    roster::Roster, step,
};

use super::types::EpisodeEnding;
use crate::Result;

/// What the loop does next.
#[derive(Debug)]
pub enum Scheduled {
    /// Run these turns, append every row, then call
    /// [`Scheduler::commit`].
    Round(Vec<HiveTurn>),
    /// Stop, and report the room this way.
    Ended(EpisodeEnding),
}

/// Decides who holds the floor and when a room is finished.
///
/// Implementations own their own state. The loop holds no episode state of its
/// own, which is what lets two terminations share one loop.
pub trait Scheduler: Send {
    /// Decide the next pass over `transcript`.
    ///
    /// # Errors
    ///
    /// Whatever the underlying fold rejects — a malformed roster or desk
    /// snapshot, or a policy error. A scheduler that cannot decide stops the
    /// episode rather than guessing at a round.
    fn next(
        &mut self,
        transcript: &[SessionMessage],
        roster: &Roster<'_>,
        desks: &DeskSet<'_>,
    ) -> Result<Scheduled>;

    /// Whether turns from this scheduler are completion-driven.
    ///
    /// The prompt builder asks this rather than reading `HiveTurn.phase`:
    /// `Phase` is `Deliberate | Commit`, the two stages of reaching a quorum
    /// decision, and a completion room has neither. A scheduler that answers
    /// `true` gets the completion protocol; everything else renders exactly as
    /// it did before this existed.
    fn completing(&self) -> bool {
        false
    }

    /// Record work handed to `recipients` by a routed `!broadcast`.
    ///
    /// A no-op by default: a quorum room decides by counting supporters, so
    /// there is no assignment to reopen and a handoff is just another desk row
    /// the fold reads. Only a completion room tracks who owes what.
    fn assign(&mut self, _recipients: &[String], _at: tinyhivemind_hive::Sequence) {}

    /// Take up the state the last [`next`](Scheduler::next) produced.
    ///
    /// Called by the loop **only** once every row of that round is durably
    /// appended. A scheduler with nothing to carry may do nothing.
    fn commit(&mut self);
}

/// The existing termination: a quorum the room can name, a deadlock, a spent
/// budget, or nobody with anything to say.
///
/// A thin wrapper over [`step`], which is a pure fold — it takes `&EpisodeState`
/// and returns a value, so moving the call behind a trait cannot lose an effect
/// there was never any of.
pub struct Quorum {
    state: EpisodeState,
    policy: EpisodePolicy,
    /// The state `next` folded, held until the round it authorized is appended.
    pending: Option<Box<EpisodeState>>,
}

impl Quorum {
    /// Open a quorum-terminated episode over `state`.
    #[must_use]
    pub const fn new(state: EpisodeState, policy: EpisodePolicy) -> Self {
        Self {
            state,
            policy,
            pending: None,
        }
    }
}

impl Scheduler for Quorum {
    fn next(
        &mut self,
        transcript: &[SessionMessage],
        roster: &Roster<'_>,
        desks: &DeskSet<'_>,
    ) -> Result<Scheduled> {
        let decision =
            step(&self.state, transcript, roster, desks, &self.policy).map_err(|error| {
                crate::error::OpenCompanyError::Config(format!(
                    "hive episode: the room's own snapshot was rejected: {error}"
                ))
            })?;
        Ok(match decision {
            HiveStep::Speak { turns, next_state } => {
                self.pending = Some(next_state);
                Scheduled::Round(turns)
            }
            HiveStep::Converged { topic, standing } => Scheduled::Ended(EpisodeEnding::Converged {
                // Read from the very transcript `step` just decided on, so the
                // text reported is the text that carried — exactly as the loop
                // did before this moved behind the trait.
                proposal: super::episode::carried_proposal(transcript, topic.as_str()),
                topic: topic.to_string(),
                supporters: standing.supporters.clone(),
            }),
            HiveStep::Deadlocked { topics } => Scheduled::Ended(EpisodeEnding::Deadlocked {
                topics: topics.iter().map(ToString::to_string).collect(),
            }),
            HiveStep::Exhausted { .. } => Scheduled::Ended(EpisodeEnding::Exhausted),
            HiveStep::Idle => Scheduled::Ended(EpisodeEnding::Idle),
        })
    }

    fn commit(&mut self) {
        if let Some(next) = self.pending.take() {
            self.state = *next;
        }
    }
}

/// The alternative termination: the room ends when every assigned member has
/// reported its own work finished.
///
/// Modelled on the reference runner in `tinyhivemind`'s OpenHuman example: a
/// FIFO of scheduled members, one turn at a time, bounded by a turn cap, with
/// [`completion_status`](tinyhivemind_hive::completion_status) as the stop
/// condition. An assignment pushes its recipients onto the queue; a member is
/// popped when it speaks.
///
/// # The transcript is the source of truth
///
/// Completions are folded out of the transcript on the next
/// [`next`](Scheduler::next) rather than pushed in through
/// [`commit`](Scheduler::commit), for the same reason the quorum fold re-reads
/// it every pass: a second record of what was said could disagree with what was
/// journaled, and the journal is the one that survives.
pub struct Completion {
    state: tinyhivemind_hive::CompletionEpisodeState,
    queue: std::collections::VecDeque<String>,
    /// Sequences already folded as completions, so one report is taken once
    /// however many passes it stays in the window.
    seen: std::collections::BTreeSet<u64>,
    turns: u32,
    cap: u32,
}

impl Completion {
    /// Open a completion-terminated episode over the members already assigned.
    ///
    /// `cap` bounds the whole episode the way a quorum room's turn budget does:
    /// without it a pair that never reports runs until something else stops it.
    #[must_use]
    pub fn new(state: tinyhivemind_hive::CompletionEpisodeState, cap: u32) -> Self {
        let queue = super::completion::pending(&state).into_iter().collect();
        Self {
            state,
            queue,
            seen: std::collections::BTreeSet::new(),
            turns: 0,
            cap,
        }
    }

    /// Record work assigned to `recipients`, reopening exactly those members.
    ///
    /// This is the edge a routed broadcast comes in on: the recipients the
    /// Choice picked are assigned, and any of them that had already reported is
    /// live again. Ignores an id this episode never opened — a broadcast may
    /// route to a teammate who is not a participant, and that is the caller's
    /// business to widen, not this scheduler's to fail on.
    /// Fold every completion report the transcript carries and has not been
    /// counted yet.
    fn absorb(&mut self, transcript: &[SessionMessage]) -> Result<()> {
        for message in transcript {
            let Some(body) = message.readable() else {
                continue;
            };
            if !super::completion::reply_reports_completion(body) {
                continue;
            }
            if !self.seen.insert(message.sequence.0) {
                continue;
            }
            let Some(agent_id) = agent_author(&message.author) else {
                continue;
            };
            // A report from somebody this episode never assigned is somebody
            // else's row in a shared desk, not a completion — skipped rather
            // than failed, because refusing would make an unrelated teammate
            // speaking on the same desk end the episode.
            if let Ok(next) = super::completion::completed(&self.state, agent_id, message.sequence)
            {
                self.state = next;
            }
        }
        Ok(())
    }
}

/// The agent id a row is attributed to, or `None` for any other author.
fn agent_author(author: &tinyhivemind_hive::SessionAuthor) -> Option<&str> {
    match author {
        tinyhivemind_hive::SessionAuthor::Agent { id, .. } => Some(id.as_str()),
        _ => None,
    }
}

impl Scheduler for Completion {
    fn next(
        &mut self,
        transcript: &[SessionMessage],
        _roster: &Roster<'_>,
        _desks: &DeskSet<'_>,
    ) -> Result<Scheduled> {
        self.absorb(transcript)?;

        if super::completion::pending(&self.state).is_empty() {
            return Ok(Scheduled::Ended(EpisodeEnding::Completed {
                completed: match super::completion::step(&self.state) {
                    tinyhivemind_hive::CompletionStep::Complete { completed_ids } => completed_ids,
                    tinyhivemind_hive::CompletionStep::Active { .. } => Vec::new(),
                },
            }));
        }
        if self.turns >= self.cap {
            return Ok(Scheduled::Ended(EpisodeEnding::Exhausted));
        }

        // Re-queue anybody still pending that the queue has lost — a member
        // reopened by an assignment after it had already spoken. Without this a
        // reopened member would be pending forever with nothing scheduling it,
        // which is the one shape the reference runner treats as a bug.
        for id in super::completion::pending(&self.state) {
            if !self.queue.contains(&id) {
                self.queue.push_back(id);
            }
        }

        // Skip anybody the queue still holds who has since reported: the queue
        // is filled when work is assigned and a member may complete before it
        // reaches the front, so a stale entry would hand the floor to somebody
        // with nothing left to do.
        let still_pending = super::completion::pending(&self.state);
        let agent_id = loop {
            let Some(candidate) = self.queue.pop_front() else {
                return Ok(Scheduled::Ended(EpisodeEnding::Exhausted));
            };
            if still_pending.contains(&candidate) {
                break candidate;
            }
        };
        self.turns = self.turns.saturating_add(1);

        let round_start = transcript
            .last()
            .map_or(self.state.watermark, |message| message.sequence);
        Ok(Scheduled::Round(vec![HiveTurn {
            agent_id,
            // No blind round: independence is bought by a quorum room forming
            // positions unseen, and nothing here counts positions.
            visibility: tinyhivemind_hive::Visibility::Full,
            // Neither `Phase` nor `BidReason` has a value that means "was
            // assigned" — both are quorum and attention-market vocabulary. These
            // are the least wrong of what exists, and `prompt.rs` branching on
            // `phase` is why a completion room still reads the deliberation
            // protocol. See the module docs on the third protocol still owed.
            phase: tinyhivemind_hive::Phase::Deliberate,
            reason: tinyhivemind_hive::BidReason::Addressed,
            watermark: self.state.watermark,
            round_start,
        }]))
    }

    fn completing(&self) -> bool {
        true
    }

    /// Record work assigned to `recipients`, reopening exactly those members.
    ///
    /// This is the edge a routed broadcast comes in on: the recipients the
    /// Choice picked are assigned, and any of them that had already reported is
    /// live again. Ignores an id this episode never opened — a broadcast may
    /// route to a teammate who is not a participant, and that is the caller's
    /// business to widen, not this scheduler's to fail on.
    fn assign(&mut self, recipients: &[String], at: tinyhivemind_hive::Sequence) {
        if let Ok(next) = super::completion::assigned(&self.state, recipients, at) {
            self.state = next;
        }
    }

    fn commit(&mut self) {
        // Nothing to carry: the next pass folds what was appended out of the
        // transcript, which is the record that survives.
    }
}

#[cfg(test)]
#[path = "schedule_tests.rs"]
mod tests;
