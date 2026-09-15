//! Cross-desk referral: the one mechanism in this module that leaves the room.
//!
//! Everything else here stops at the edge of one desk. An episode folds one
//! transcript, and its members read the same rows, work the same part of the
//! company and are wrong about the same things. Their errors are *correlated*,
//! and averaging correlated error does not remove it — so no amount of
//! deliberating inside a desk cancels a mistake the whole desk shares. Pooling
//! across desks is the only operation that can, which is what a referral is
//! for.
//!
//! # One message still means one turn
//!
//! A desk mention reads like fan-out and is not.
//! [`tinyhivemind_hive::referral::referral`] resolves `@#platform` to exactly
//! one agent — that desk's first eligible member other than the author —
//! *before* the decision leaves the fold, and there is no variant of
//! `ReferralDecision` that carries two. This host adds a second bound the
//! library deliberately leaves to it ([`ReferralConfig::peer_cap`]): how many
//! questions one episode may ask, because only the host knows what a question
//! costs it.
//!
//! # Mention dispatch is this, with `reach = "local"`
//!
//! `tinyhivemind` ships two seams that look like two features:
//! `mention_dispatch`/`MentionTurnQueue`, and `referral`/`ReferralQueue`. They
//! are not independent. With `enabled` and `max_hops` set and `reach` left at
//! `local`, a referral decision is **exactly** the decision `mention_dispatch`
//! makes, on the same conversation — an equivalence tinyhivemind asserts with a
//! test over every interesting input rather than merely documenting. So this
//! host wires the wider seam once and reaches the narrower one through
//! [`ReferralReach::Local`], instead of carrying two queues, two policies and
//! two idempotency keys that must agree.
//!
//! # Information crosses, votes do not
//!
//! A crossing forward runs a real turn on the far desk, journaled there under
//! the far teammate's own id, because that is a turn that teammate genuinely
//! took on its own desk. The **answer** comes back as a system row under
//! [`HIVE_REFERRAL_AUTHOR`](super::HIVE_REFERRAL_AUTHOR), not under the far
//! teammate's id. That is not a formatting choice: a row authored by a roster
//! id folds as a trace and can be counted as a supporter, so carrying the far
//! desk's answer back under its author's name would let one supporter count on
//! two desks. The asking room hears another desk's reading and still has to
//! spend its own turns before anything is counted.
//!
//! See `docs/spec/runtime/hivemind-referral.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tinyhivemind_hive::{
    EnqueueOutcome, EnqueueRefusal,
    desk::{Desk, DeskSet, ResponderMode},
    dispatch::{DispatchConversation, DispatchKey},
    mention::{Mention, MentionAuthor, MentionTarget, resolve as resolve_mentions},
    referral::{
        NoReferralReason, Referral, ReferralFuture, ReferralInput, ReferralKind, ReferralOrigin,
        ReferralOutcome, ReferralPolicy, ReferralQueue, ReferralReach, dispatch_referral,
    },
    roster::{Roster, RosterMember},
};
use tokio::sync::Mutex;

use super::scope::EpisodeScope;
use crate::Result;
use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq};

/// The `[[group_chat]].hive.referral` block: whether this desk may ask another.
///
/// Every field is optional and the whole block defaults to
/// [`ReferralPolicy::DEFAULT`] — referral fully off. That is the conservative
/// direction on purpose. tinyhivemind's own benchmark measured crossing a desk
/// changing no answer and costing twice the turns on desks that are
/// individually unbiased; the mechanism pays for itself only where desks have
/// blind spots of their own. A company that says nothing therefore deliberates
/// exactly as it did before this existed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferralConfig {
    /// Whether a committed line on this desk may refer a turn at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Maximum chain depth. Defaults to 2 — one question and its answer —
    /// which is the only depth a `returns = true` round trip needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hops: Option<u32>,
    /// How far a referred turn may travel: `local`, `channels` or `desks`.
    ///
    /// The three widen strictly, which is why the library makes it one knob
    /// rather than two: a `@#desk` mention only means anything once a turn is
    /// allowed to run somewhere other than here. Defaults to `desks` when the
    /// block is present at all, because a desk that opted in to referral and
    /// got `local` would have opted in to nothing it could not already do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<String>,
    /// Whether a crossing question's answer is carried back to the desk that
    /// asked. Defaults to `true`: a question whose answer never returns is a
    /// turn spent for nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<bool>,
    /// How many crossing questions one episode may ask, in total.
    ///
    /// The library bounds how *deep* a chain goes (`max_hops`) and
    /// deliberately bounds nothing about how *wide* it is, because only a host
    /// knows what a question costs it. Here it costs a full model turn on
    /// another desk, so the default is 2 — enough to consult a peer and a
    /// second opinion, not enough for a six-seat room to poll the company.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_cap: Option<u32>,
    /// Whether a question put to a **desk** is answered by that desk
    /// deliberating, rather than by one of its seats taking a single turn.
    ///
    /// Here for exactly the reason [`ReferralConfig::peer_cap`] is: the
    /// library bounds the shape of a crossing and deliberately says nothing
    /// about what one costs, because only a host knows. It resolves `@#desk`
    /// to that desk's *one responder* — the first active member in declared
    /// order — and returns a decision with no plural in it, so `@#returns` on
    /// a `members = ["exchanges", "refunds"]` desk reaches `exchanges` every
    /// time and cannot reach `refunds` at all. Asking a desk and being
    /// answered by whichever seat happens to be listed first is the thing
    /// deliberation exists to stop.
    ///
    /// Defaults to `true` for a desk that opted in to referral: it asked a
    /// peer *desk* for its judgement, and one seat's opinion is a weaker
    /// answer than the one it asked for. Set it `false` to get the
    /// single-responder crossing back — the cheap arm, and the one every
    /// measurement before this flag existed was taken against. It costs a
    /// room instead of a turn, which is the trade the knob is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliberates: Option<bool>,
}

/// The reach words a manifest may write, in widening order.
pub const REACH_WORDS: &[&str] = &["local", "channels", "desks"];

impl ReferralConfig {
    /// Whether this block says nothing at all, so a serialized manifest keeps
    /// omitting it exactly as it did before referral existed.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Whether this desk refers anything.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled == Some(true)
    }

    /// The library policy this block derives.
    ///
    /// Returns [`ReferralPolicy::DEFAULT`] — which refers nothing — whenever
    /// the desk did not explicitly opt in, so every caller can build the policy
    /// unconditionally and let the fold decline.
    #[must_use]
    pub fn policy(&self) -> ReferralPolicy {
        if !self.enabled() {
            return ReferralPolicy::DEFAULT;
        }
        ReferralPolicy {
            enabled: true,
            // At least 2: a round trip is two hops, so a `max_hops = 1` with
            // `returns = true` is a policy that asks a question it has already
            // decided not to hear the answer to.
            max_hops: self.max_hops.unwrap_or(2).max(1),
            reach: self.reach.as_deref().map_or(ReferralReach::Desks, reach_of),
            returns: self.returns.unwrap_or(true),
        }
    }

    /// How many crossing questions one episode may ask.
    #[must_use]
    pub fn peer_cap(&self) -> u32 {
        self.peer_cap.unwrap_or(2)
    }

    /// Whether a desk crossing runs the far desk as a room.
    #[must_use]
    pub fn deliberates(&self) -> bool {
        self.deliberates.unwrap_or(true)
    }
}

/// The reach a manifest word names, falling back to the widest.
///
/// An unknown word cannot reach here — `manifest.rs` refuses it against
/// [`REACH_WORDS`] with the valid list in the message — so the fallback is for
/// totality, and it matches the default a present-but-silent block gets.
#[must_use]
pub fn reach_of(word: &str) -> ReferralReach {
    match word.trim().to_ascii_lowercase().as_str() {
        "local" => ReferralReach::Local,
        "channels" => ReferralReach::Channels,
        _ => ReferralReach::Desks,
    }
}

/// One desk as a referral target: enough to resolve `@#id` and pick its one
/// responder, and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FederationDesk {
    /// Canonical desk id — what `@#id` is written against.
    pub id: String,
    /// Operator-facing name.
    pub name: String,
    /// What the desk is for, rendered into the asking seat's prompt so a
    /// member can tell which desk holds which fact.
    pub description: Option<String>,
    /// Active member ids in desk order.
    pub members: Vec<String>,
}

/// Every desk and teammate an episode may refer to.
///
/// A snapshot taken once when the episode opens, for the same reason
/// [`HiveDesk`](super::HiveDesk) is one: an episode is several turns long, and
/// a company whose desks changed underneath it would resolve a mention against
/// a roster the earlier turns never saw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HiveFederation {
    /// Every desk in the company, including the one deliberating.
    pub desks: Vec<FederationDesk>,
    /// Every teammate seated on any of those desks, as `(id, label)`, sorted by
    /// id.
    ///
    /// Order is deliberately not desk order and does not need to be: nothing
    /// reads a *responder* off this list. The far desk's one answerer is picked
    /// from that desk's own `members`, which does keep desk order, and this is
    /// only what `@teammate` resolves against.
    pub agents: Vec<(String, String)>,
}

impl HiveFederation {
    /// The peer desks of `home` — everything a member could usefully ask.
    #[must_use]
    pub fn peers_of(&self, home: &str) -> Vec<&FederationDesk> {
        self.desks
            .iter()
            .filter(|desk| desk.id != home && !desk.members.is_empty())
            .collect()
    }

    /// The roster this federation resolves mentions against.
    #[must_use]
    pub fn roster_members(&self) -> Vec<RosterMember> {
        self.agents
            .iter()
            .map(|(id, label)| RosterMember {
                id: id.clone(),
                name: Some(label.clone()),
            })
            .collect()
    }

    /// The desk set this federation resolves `@#id` against.
    #[must_use]
    pub fn desk_set(&self) -> Vec<Desk> {
        self.desks
            .iter()
            .map(|desk| Desk {
                id: desk.id.clone(),
                name: desk.name.clone(),
                description: desk.description.clone(),
                members: desk.members.clone(),
                // `Auto` for the same reason the deliberating desk is: a
                // referral picks the far desk's one eligible member itself, and
                // a declared lead would be a second, competing answer to the
                // question the fold has already answered.
                responder_mode: ResponderMode::Auto,
            })
            .collect()
    }
}

/// How one referred turn is filled.
///
/// Deliberately the same shape as [`HiveTurnRunner`](super::HiveTurnRunner) and
/// deliberately *not* the same trait: a referred turn runs on a different desk
/// from the episode's, so it needs the desk id, and folding that into the
/// episode seam would make every existing implementation carry a parameter it
/// must ignore.
#[async_trait]
pub trait HiveReferralRunner: Send + Sync {
    /// Run one turn on `agent_id`, on the desk `desk_id`, and return what it
    /// said.
    ///
    /// # Errors
    ///
    /// Returns whatever the underlying turn failed with. A failed referral does
    /// not end the asking episode: the room is told the question went
    /// unanswered and goes on, exactly as it does when one of its own seats
    /// misses a turn.
    async fn refer(&self, desk_id: &str, agent_id: &str, prompt: &str) -> Result<String>;

    /// Put the question to `desk_id` as a **room**, and return the conclusion
    /// it reached — or `None` when that desk cannot deliberate, in which case
    /// the caller falls back to [`refer`](Self::refer) and one seat answers.
    ///
    /// `None` rather than an error, because "this desk has no `[hive]` block",
    /// "it has one member" and "its quorum is unreachable right now" are all
    /// ordinary shapes a company is allowed to have — and every one of them
    /// has a correct answer that is not a failure: ask the seat. An `Err` is
    /// reserved for a room that was stood up and then broke, which is a failed
    /// crossing exactly as a failed single turn is.
    ///
    /// # The question is journaled on the far desk, which a single-seat
    /// crossing deliberately does not do
    ///
    /// A room decides what to say next by folding its own transcript, so a
    /// question that is not in that transcript is a question the room cannot
    /// read. The implementation therefore writes it there — as the asking
    /// agent's own operator message, the same shape a chat-path crossing
    /// already lands — and roots the episode's thread on it. `asker` is who
    /// that row is attributed to.
    ///
    /// # Errors
    ///
    /// Returns whatever standing up or running the far desk's episode failed
    /// with. Treated exactly as a failed [`refer`](Self::refer): the asking
    /// room is told the question went unanswered and carries on.
    async fn deliberate(
        &self,
        _desk_id: &str,
        _asker: &str,
        _prompt: &str,
    ) -> Result<Option<String>> {
        Ok(None)
    }
}

/// What one episode's referrals actually did, for the closing report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReferralLedger {
    /// Questions that ran a turn on another desk.
    pub asked: Vec<AskedQuestion>,
    /// Questions declined by the host's own width bound.
    pub over_cap: u32,
    /// Questions whose far turn did not finish.
    pub failed: u32,
}

/// One question an episode asked of a named teammate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskedQuestion {
    /// The seat that asked.
    pub asker: String,
    /// The teammate that answered.
    pub target: String,
    /// The desk the answer was journaled on.
    pub desk: String,
    /// Whether the answer was carried back to the asking desk.
    pub returned: bool,
    /// Whether the question actually left this desk.
    ///
    /// `reach` widens strictly (`local` → `channels` → `desks`), so a desk that
    /// opted in to referral can put a question to a peer **on its own desk**,
    /// and that is a legitimate use rather than a misroute. What it is not is a
    /// question of *another* desk, and the close used to call it one
    /// unconditionally: a live six-day `companies/vending_machine_co` run
    /// reported "The room asked 2 questions of another desk (@fleet_tech on
    /// ops, @field_realist on ops)" on the ops desk itself, naming two of its
    /// own seats. An operator reading that has been told the room reached
    /// outside when it did not.
    pub crossed: bool,
}

/// The prompt a referred teammate is given.
///
/// Short on purpose, and framed as a question from a named colleague rather
/// than as an episode turn: the far teammate is not a member of the asking
/// room, has not read its transcript, and must not be handed a deliberation
/// grammar it would then deposit markers under on its own desk.
#[must_use]
pub fn referral_prompt(asker_label: &str, asker_desk: &str, question: &str) -> String {
    format!(
        "{asker_label} on the {asker_desk} desk has asked you a question. \
Answer it from what you and this desk know.

Their message:

{question}

Answer in a few sentences. Say plainly if you do not know — a wrong answer \
crossing desks is worse than no answer, because the room that asked cannot \
check it against anything they have. Do not open your reply with a `!` \
marker: this is not a deliberation turn, it is an answer to a colleague."
    )
}

/// How a crossing reads to a desk that will DELIBERATE on it.
///
/// [`referral_prompt`] closes by telling its reader "Do not open your reply with
/// a `!` marker: this is not a deliberation turn, it is an answer to a
/// colleague." That is right for the seat it was written for and exactly wrong
/// for a room: `EpisodePrompt`'s own protocol opens with "Reply with ONE line
/// only, beginning with exactly one of these markers", so handing the
/// single-seat prompt to a room as its task contradicts the protocol in the
/// same prompt — and a member that follows the task deposits prose, which the
/// fold counts for nothing. Three of the first four live crossings ended `idle`
/// with markerless turns for exactly this reason (CodeRabbit, #2332).
///
/// So this says who asked and what they asked, and says nothing about how to
/// reply: the room already has a protocol, and it is the only thing entitled to
/// set the shape of a turn on that desk.
///
/// The question comes LAST, with no footer after it, so [`asked_message`]
/// recovers it from this shape exactly as it does from the single-seat one.
#[must_use]
pub fn referral_room_prompt(asker_label: &str, asker_desk: &str, question: &str) -> String {
    format!(
        "{asker_label} on the {asker_desk} desk has put a question to this desk. Answer it from \
what this desk knows, and say plainly if it does not know — a wrong answer crossing desks is \
worse than no answer, because the room that asked cannot check it against anything they have.

Their message:

{question}"
    )
}

/// The question out of a [`referral_prompt`], or the text unchanged when it is
/// not one.
///
/// The prompt wraps the asker's words in instructions written for the far
/// teammate — "Answer it from what you and this desk know… Do not open your
/// reply with a `!` marker" — and a *deliberated* crossing journals that whole
/// prompt on the desk being asked, because that is what the room was handed.
/// The collapsed crossing on the asking desk then matched that row as the
/// question and rendered the instructions where the question should be, so a
/// reader opening it saw "cancellations on the Order Operations desk has asked
/// you a question. Answer it from what you and this desk know." instead of what
/// was asked.
///
/// Split on the prompt's own section header, so this cannot drift from the
/// template: if the header stops appearing, the text passes through whole,
/// which is the behaviour before a prompt was ever journaled.
#[must_use]
pub fn asked_message(text: &str) -> String {
    const HEAD: &str = "Their message:";
    let Some((_, rest)) = text.split_once(HEAD) else {
        return text.trim().to_string();
    };
    // **The exact footer, or nothing.**
    //
    // Two earlier attempts both cut the question short, in opposite ways.
    // Splitting FORWARDS on the footer's opening sentence truncated a question
    // that quoted it (CodeRabbit). Splitting BACKWARDS fixed that for the
    // single-seat prompt and broke `referral_room_prompt`, which deliberately
    // has no footer at all — so a room question quoting that sentence was
    // truncated at its own words instead (Codex). Both are the same mistake:
    // recognising the footer by a fragment that the question may also contain.
    //
    // So the footer is taken from `referral_prompt` itself, whole, and removed
    // only as a suffix. A shape that does not end with it — the room prompt, or
    // any future one — keeps its question intact, and the two cannot drift
    // because the literal is never written twice.
    let footer = {
        let empty = referral_prompt("", "", "");
        empty
            .split_once(HEAD)
            .map(|(_, tail)| tail.trim_start().to_string())
            .unwrap_or_default()
    };
    let question = match footer.is_empty() {
        true => rest,
        false => rest
            .trim_end()
            .strip_suffix(footer.as_str())
            .unwrap_or(rest),
    };
    question.trim().to_string()
}

/// The conversation two teammates hold with each other, by their ids.
///
/// **A crossing is a conversation between two people, not a message posted in
/// somebody's office.** The library places a referred turn on the target's home
/// desk, which is what "runs on their own desk" means there — but that puts a
/// question about ANOTHER desk's case into a channel whose transcript is
/// supposed to be the record of what that desk did, in front of colleagues who
/// were never asked. The pair get their own conversation instead: the question
/// goes there, the answer is written there, and neither desk carries somebody
/// else's exchange.
///
/// Sorted, so `a` asking `b` and `b` asking `a` are the same thread rather than
/// two half-conversations. Prefixed `dm:` so it can never collide with a desk
/// id — a manifest desk id is a bare slug.
#[must_use]
pub fn pair_conversation(one: &str, two: &str) -> String {
    let (first, second) = if one <= two { (one, two) } else { (two, one) };
    format!("dm:{first}+{second}")
}

/// How the far desk's answer reads on the asking desk.
///
/// Attributed in the text and authored by the room, never by the answerer: see
/// the module docs on why an answer that crosses must not be able to carry a
/// vote with it.
#[must_use]
pub fn returned_note(target: &str, desk: &str, answer: &str) -> String {
    let answer = answer.trim();
    // Stripped for the same reason a demoted line is: a far desk's answer that
    // happened to begin with `!` would fold as a trace on *this* desk, which is
    // precisely the vote this row exists not to carry.
    let answer = answer.trim_start_matches('!').trim();
    format!(
        "@{target} on {} answered the question: {answer}",
        named_desk(desk)
    )
}

/// How a far desk's *room* answer reads on the asking desk.
///
/// The desk, not a seat. [`returned_note`] names the member whose turn produced
/// the answer, which is right when one member's turn is what produced it; a
/// deliberated answer is the room's conclusion, and the seat the library
/// resolved `@#desk` to may well have argued against it. Attributed to the room
/// and authored by [`HIVE_REFERRAL_AUTHOR`](super::HIVE_REFERRAL_AUTHOR) for
/// the same reason every crossing answer is: information crosses, votes do not.
#[must_use]
pub fn room_note(desk: &str, answer: &str) -> String {
    let answer = answer.trim().trim_start_matches('!').trim();
    format!("{} answered the question: {answer}", named_desk(desk))
}

/// A [`returned_note`] with its attribution removed, for a surface that
/// attributes the line itself.
///
/// The prefix is load-bearing in the journal — it is what makes a crossing's
/// answer read as the far desk's words in a room that never heard them, and the
/// module docs turn on it. Inside the collapsed crossing the console folds onto
/// the asking row it is the third copy of the same fact: the header already
/// names the desk that was asked, and the line already carries the answerer's
/// avatar and name. It rendered as "exchanges — @exchanges on the Returns and
/// Exchanges desk answered the question: …".
///
/// Derived from [`returned_note`] rather than matched by pattern, so the two
/// cannot drift: an empty answer renders exactly the prefix to remove.
#[must_use]
pub fn unattributed(target: &str, desk: &str, text: &str) -> String {
    // Both shapes an answer can come home under — a seat's and a room's. A
    // crossing that convened a desk carries `room_note`, and stripping only the
    // seat form left "the Returns and Exchanges desk answered the question: …"
    // rendered inside a fold whose header already says `#returns`.
    for prefix in [returned_note(target, desk, ""), room_note(desk, "")] {
        if let Some(rest) = text.strip_prefix(prefix.trim_end()) {
            return rest.trim_start().to_string();
        }
    }
    text.to_string()
}

/// A desk named for a sentence: "the Operations desk", "the eng desk".
///
/// The template used to append " desk" unconditionally, and every desk in this
/// repo is *named* "… desk", so a live run printed "@route_planner on the
/// Operations desk desk did not answer the question." A name that already ends
/// in the word carries it; one that does not gets it.
fn named_desk(desk: &str) -> String {
    let trimmed = desk.trim();
    if trimmed
        .rsplit(|c: char| c.is_whitespace())
        .next()
        .is_some_and(|last| last.eq_ignore_ascii_case("desk"))
    {
        format!("the {trimmed}")
    } else {
        format!("the {trimmed} desk")
    }
}

/// The line an unanswered question leaves when a ROOM could not answer it.
///
/// [`unanswered_note`] names the seat that was asked, which is right when a
/// seat was asked. A crossing that convened the far desk asked nobody in
/// particular — and when that room fails, `forward` returns from the
/// `deliberate` arm before `refer` ever runs — so naming the seat the library
/// happened to resolve credits a teammate with a refusal it never made, on a
/// desk where nobody reading the row can check. The same defect
/// [`room_note`] exists to remove on the success path (CodeRabbit, #2332).
#[must_use]
pub fn room_unanswered_note(desk: &str) -> String {
    format!("{} did not answer the question.", named_desk(desk))
}

/// The line an unanswered question leaves on the asking desk.
#[must_use]
pub fn unanswered_note(target: &str, desk: &str) -> String {
    format!(
        "@{target} on {} did not answer the question.",
        named_desk(desk)
    )
}

/// The episode-scoped adapter between the library's referral fold and this
/// host's journal.
///
/// Implements [`ReferralQueue`], which the library documents as an *atomic,
/// durable* enqueue boundary. What that boundary means here is worth stating
/// exactly, because this implementation is narrower than the port allows and
/// the narrowing is deliberate rather than unfinished:
///
/// - **Idempotency is per episode, not per journal.** The key the port asks to
///   be scoped by — the `from` conversation plus the trigger sequence — is held
///   in [`Self::seen`] for the life of one [`EpisodeDriver`][super::EpisodeDriver]
///   run. That is sufficient *and* honest, because an episode is not resumable:
///   it runs to completion inside one process, and a crash does not leave a
///   half-run episode for a later process to retry. A durable idempotency
///   record would be a table nothing ever reads a second time.
/// - **The child turn runs inline rather than being enqueued.** For the same
///   reason: there is no turn queue to enqueue onto, and the episode loop is
///   already the thing that runs turns one at a time. Running it here keeps the
///   answer available to the very next speaker, which the wiki identifies as
///   the single largest effect measured — a desk that asks and then votes
///   before the answer lands has voted past the information it paid for.
pub struct EpisodeReferrals<'a> {
    runner: &'a dyn HiveReferralRunner,
    events: Arc<dyn EventLog>,
    company: CompanyId,
    /// Home desk id and thread, so a return can be journaled where it belongs.
    home: DispatchConversation,
    /// Display labels by agent id, for the referral prompt and the returned row.
    labels: BTreeMap<String, String>,
    /// Desk names by id, likewise.
    desk_names: BTreeMap<String, String>,
    peer_cap: u32,
    /// Whether a `@#desk` crossing convenes that desk rather than asking one
    /// of its seats ([`ReferralConfig::deliberates`]).
    deliberates: bool,
    state: Mutex<ReferralState>,
    /// The asking episode's own fold boundary.
    ///
    /// A row this queue journals onto `home` — a return carried back, or an
    /// unanswered note — is this episode's own doing exactly as much as a
    /// turn from its main loop is, so the next speaker must read it. Recorded
    /// here for the same reason the driver records its own turns: without it,
    /// the episode that paid for the question could not see its own answer.
    scope: Arc<EpisodeScope>,
}

/// The mutable half, behind one lock so the queue is `Sync` without the driver
/// having to be.
#[derive(Default)]
struct ReferralState {
    seen: HashSet<(String, Option<u64>, u64)>,
    asked: u32,
    ledger: ReferralLedger,
    /// The answer the last forward produced, if it finished — read by the
    /// driver to decide whether a return hop is worth folding.
    last_answer: Option<(Referral, String)>,
    /// The agents named DIRECTLY, by the line that named them.
    ///
    /// A crossing goes to the pair's own thread only when a person was asked
    /// for by name. A `@#desk` mention resolves to whoever answers for that
    /// desk, and that is a question put to the DESK — it belongs on the desk,
    /// where the room can deliberate it, exactly as it did before pairs had
    /// their own thread. The two are indistinguishable by the time the library
    /// hands back a `Referral` — both carry the target's home desk in `to` —
    /// so the distinction is recorded here, where the mention is still in hand.
    /// Keyed by the trigger, not accumulated episode-wide: an earlier line
    /// naming `@sre` must not make a LATER `@#platform` — which resolves to
    /// that same person — look like it named them. Directness is a fact about
    /// one line, and a room asks more than once.
    named: HashMap<u64, HashSet<String>>,
    /// The journal sequence of each forward marker this episode wrote, by the
    /// desk it went to. A return names the forward it answers (`answers`), and
    /// this is where that sequence comes from — the episode knows it, because
    /// it wrote the marker itself a moment earlier.
    forwards: HashMap<String, u64>,
    /// Desks whose answer to this episode's last question came from the whole
    /// room rather than from one seat, so the return can attribute it to the
    /// desk instead of to a member who did not personally answer.
    ///
    /// Keyed by desk id and consumed on read. Safe because a forward and the
    /// return that answers it strictly alternate inside one `consider` call —
    /// the same property `ledger.asked.last_mut()` in `ret` already relies on
    /// — so at most one unread entry per desk can exist at a time.
    room_answers: HashSet<String>,
}

impl<'a> EpisodeReferrals<'a> {
    /// Open the adapter for one episode.
    #[must_use]
    pub fn new(
        runner: &'a dyn HiveReferralRunner,
        events: Arc<dyn EventLog>,
        company: CompanyId,
        home: DispatchConversation,
        federation: &HiveFederation,
        // The desk's own `[hive.referral]` block rather than the two bounds
        // read off it. Both are this host's, not the library's — how WIDE a
        // crossing may go and what one costs — so they travel together, and a
        // third such bound adds no parameter here.
        config: &ReferralConfig,
        scope: Arc<EpisodeScope>,
    ) -> Self {
        Self {
            runner,
            events,
            company,
            home,
            labels: federation
                .agents
                .iter()
                .map(|(id, label)| (id.clone(), label.clone()))
                .collect(),
            desk_names: federation
                .desks
                .iter()
                .map(|desk| (desk.id.clone(), desk.name.clone()))
                .collect(),
            peer_cap: config.peer_cap(),
            deliberates: config.deliberates(),
            state: Mutex::new(ReferralState::default()),
            scope,
        }
    }

    /// What this episode's referrals amounted to.
    /// Record which agents THIS line named by name, before its referral is
    /// decided.
    pub async fn note_named(&self, trigger: u64, mentions: &[Mention]) {
        let mut state = self.state.lock().await;
        let named = state.named.entry(trigger).or_default();
        for mention in mentions {
            if let MentionTarget::Agent { id } = &mention.target {
                named.insert(id.clone());
            }
        }
    }

    /// Whether `target` was named by the line that raised `trigger`.
    async fn named_by(&self, trigger: u64, target: &str) -> bool {
        self.state
            .lock()
            .await
            .named
            .get(&trigger)
            .is_some_and(|named| named.contains(target))
    }

    pub async fn ledger(&self) -> ReferralLedger {
        self.state.lock().await.ledger.clone()
    }

    /// The forward whose answer has not been carried back yet, if any.
    async fn take_answer(&self) -> Option<(Referral, String)> {
        self.state.lock().await.last_answer.take()
    }

    fn label(&self, id: &str) -> String {
        self.labels
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_owned())
    }

    fn desk_name(&self, id: &str) -> String {
        self.desk_names
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_owned())
    }

    /// Journal one row on `conversation`, under `author`.
    async fn journal(
        &self,
        conversation: &DispatchConversation,
        author: &str,
        text: String,
    ) -> Result<EventSeq> {
        let seq = self
            .events
            .append(
                &self.company,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    chat_id: conversation.desk_id.clone(),
                    agent_id: author.to_owned(),
                    text,
                    steps: Vec::new(),
                    task_id: None,
                    outputs: Vec::new(),
                    parent: conversation.thread_root.map(EventSeq::new),
                    mentions: Vec::new(),
                    mention_depth: 0,
                },
            )
            .await?;
        // Only a row landing back on the asking episode's own desk and thread
        // is this episode's own doing. A crossing forward's answer is
        // journaled on the far desk's conversation instead (`forward`,
        // below) — a real turn by a real member of *that* desk, which that
        // desk's own concurrent episode (if any) must fold on its own terms,
        // not one this episode's scope should ever admit.
        if conversation == &self.home {
            self.scope.record(seq);
        }
        Ok(seq)
    }

    /// Run one crossing (or local) question and journal its answer where the
    /// turn actually happened.
    /// Journal the `ReferralEnqueued` marker for one crossing.
    ///
    /// The same row the chat path writes, so one projection reads both: the
    /// console's chip and its crossing transcript key off this marker, and a
    /// hand-off that does not write one is a hand-off nobody can audit.
    ///
    /// A return names the forward it answers, taken from what this episode
    /// recorded when it wrote that forward — the pairing is a fact the writer
    /// holds, never something a reader re-derives by scanning.
    async fn mark(&self, referral: &Referral) {
        let returning = matches!(referral.kind, ReferralKind::Return);
        let answers = if returning {
            self.state
                .lock()
                .await
                .forwards
                .get(&referral.from.desk_id)
                .copied()
        } else {
            None
        };
        let event = CompanyEvent::ReferralEnqueued {
            // Where this leg ran. A forward runs in the pair's own thread; a
            // return is carried home to the asking conversation, which the desk
            // fields already name.
            conversation: match returning {
                true => None,
                false => self
                    .named_by(referral.key.trigger_sequence, &referral.target_id)
                    .await
                    .then(|| pair_conversation(&referral.source_id, &referral.target_id)),
            },
            from_desk: referral.from.desk_id.clone(),
            from_desk_name: self.desk_name(&referral.from.desk_id),
            asker: referral.source_id.clone(),
            asker_label: self.label(&referral.source_id),
            trigger_sequence: referral.key.trigger_sequence,
            returning,
            answers,
            to_desk: referral.to.desk_id.clone(),
            target: referral.target_id.clone(),
        };
        match self.events.append(&self.company, event).await {
            Ok(seq) if !returning => {
                // Keyed by the desk being asked, which is the desk a return
                // comes back FROM — the lookup above.
                self.state
                    .lock()
                    .await
                    .forwards
                    .insert(referral.to.desk_id.clone(), seq.value());
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(
                company = %self.company,
                desk = %referral.from.desk_id,
                "[hive] a crossing could not be marked on the journal ({err}); the turn still runs"
            ),
        }
    }

    /// What an unanswered crossing leaves behind, however it failed.
    ///
    /// Extracted because a crossing now has two ways to come back empty — a
    /// single seat's turn, or a whole room stood up on the far desk — and they
    /// owe the asking episode the identical bookkeeping: the failure counted on
    /// the ledger, one note on the desk that spent the question, and a refusal
    /// the fold reads as "nobody answered" rather than as an error.
    ///
    /// Journaled on the *asking* desk, not the far one: it is the room that
    /// spent the question that needs to know it got nothing back.
    async fn unanswered(
        &self,
        referral: &Referral,
        error: &crate::OpenCompanyError,
        // Whether it was a ROOM that failed rather than a seat, so the note
        // names whoever was actually asked. See `room_unanswered_note`.
        by_room: bool,
    ) -> EnqueueOutcome {
        tracing::warn!(
            company = %self.company,
            from = %referral.from.desk_id,
            to = %referral.to.desk_id,
            target = %referral.target_id,
            error = %error,
            "[hive] a referred turn did not finish; the room continues"
        );
        let mut state = self.state.lock().await;
        state.ledger.failed = state.ledger.failed.saturating_add(1);
        drop(state);
        let _ = self
            .journal(
                &self.home,
                super::HIVE_REFERRAL_AUTHOR,
                match by_room {
                    true => room_unanswered_note(&self.desk_name(&referral.to.desk_id)),
                    false => {
                        unanswered_note(&referral.target_id, &self.desk_name(&referral.to.desk_id))
                    }
                },
            )
            .await;
        EnqueueOutcome::Refused {
            reason: EnqueueRefusal::TargetUnavailable,
        }
    }

    async fn forward(&self, referral: &Referral) -> EnqueueOutcome {
        let asker = self.label(&referral.source_id);
        let asker_desk = self.desk_name(&referral.from.desk_id);
        let prompt = referral_prompt(&asker, &asker_desk, &referral.content);
        // Where this runs: the pair's own thread when a person was asked for by
        // name, the target's desk when a desk was. Asking `@#order_ops` is a
        // question put to that desk — turning it into a private chat with
        // whoever leads it would skip the deliberation the desk exists for,
        // which is the whole objection to `delegate_to_desk`.
        let by_name = self
            .named_by(referral.key.trigger_sequence, &referral.target_id)
            .await;
        let pair = if by_name {
            DispatchConversation {
                desk_id: pair_conversation(&referral.source_id, &referral.target_id),
                thread_root: None,
            }
        } else {
            referral.to.clone()
        };
        // The question, written into the pair's thread — and ONLY there.
        //
        // A pair thread is a conversation between two people and has to hold
        // both sides, or it reads as a monologue. A desk crossing is not: the
        // far desk is not told it was asked, it is asked, and its transcript
        // records the turn its own member took. Writing the question there too
        // would put a question that desk never received into its history.
        //
        // Whether it actually landed decides what may follow it. A pair thread
        // is read positionally — the first thing said there IS the question —
        // so an answer written into a thread whose question failed to append
        // renders as that question: the reader is shown an answer and told it
        // was the ask. Better to carry nothing than to carry it mislabelled.
        let asked = if by_name {
            match self
                .journal(&pair, &referral.source_id, referral.content.clone())
                .await
            {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(
                        company = %self.company,
                        pair = %pair.desk_id,
                        error = %error,
                        "[hive] a crossing's question could not be journaled; the turn still runs \
                         and its answer still comes home, but the pair thread keeps neither side"
                    );
                    false
                }
            }
        } else {
            true
        };
        // **A desk was asked, so the desk answers — not whichever seat is
        // listed first on it.**
        //
        // `forward_to_desk` in the library resolves `@#returns` to that desk's
        // one responder and hands it here as `target_id`; on
        // `members = ["exchanges", "refunds"]` that is `exchanges` every single
        // time, and `refunds` is not reachable by a desk crossing at all. The
        // comment above already names the objection — a desk crossing must not
        // "skip the deliberation the desk exists for" — and running the far
        // desk's own episode is what actually honours it. Until this, the
        // objection was answered only by *where the row landed*: the turn ran
        // on the desk's channel instead of a private thread, which changed who
        // could read it and not who thought about it.
        //
        // Never for a crossing asked `by_name`: `@sre` is a question put to a
        // person, and convening their whole desk to answer it is the mirror of
        // the bug above. Never when the desk says `deliberates = false`, which
        // is the cheap single-seat arm every earlier measurement was taken
        // against. And `None` from the seam means that desk cannot hold a room
        // — no `[hive]` block, one member, quorum out of reach — so the seat
        // answers, exactly as it did before.
        let room = match !by_name && self.deliberates {
            true => match self
                .runner
                .deliberate(
                    &referral.to.desk_id,
                    &referral.source_id,
                    // Never `prompt`: that one is addressed to a single seat and
                    // forbids the very markers a room's protocol requires. See
                    // `referral_room_prompt`.
                    &referral_room_prompt(&asker, &asker_desk, &referral.content),
                )
                .await
            {
                Ok(found) => found,
                // A room that was stood up and then broke is a failed
                // crossing, handled by the arm below with the single-seat
                // failures — not silently retried as one seat, which would
                // bill the room's turns and then bill a turn again.
                Err(error) => {
                    return self.unanswered(referral, &error, true).await;
                }
            },
            false => None,
        };
        let deliberated = room.is_some();
        let answer = match room {
            Some(conclusion) => conclusion,
            None => match self
                .runner
                .refer(&pair.desk_id, &referral.target_id, &prompt)
                .await
            {
                Ok(answer) => answer,
                Err(error) => return self.unanswered(referral, &error, false).await,
            },
        };
        // Journaled in the conversation the turn ran in — the pair's own thread,
        // under the teammate that answered, directly beneath the question they
        // were answering. Their desk does not carry it: the question was not
        // put to that desk and its colleagues were never asked, so a transcript
        // that is supposed to record what THAT desk did should not be holding
        // somebody else's exchange.
        // **Never after a room.** A deliberated crossing has already written
        // its whole exchange on the far desk — the question, every turn, the
        // closing report — so writing the answer again here appended a verbatim
        // copy of that report under the seat the library had resolved, with no
        // parent. Three faults in one row: it duplicated text already on the
        // desk, it attributed the room's conclusion to one member who had not
        // written it, and `parent: None` put it at channel level instead of in
        // the crossing's thread, where no projection drops it and it read as
        // `exchanges` posting the room's summary as its own message.
        //
        // The single-seat path still needs it: there the far desk's only row IS
        // the answer, and nothing else journals it.
        if asked
            && !deliberated
            && let Err(error) = self
                .journal(&pair, &referral.target_id, answer.clone())
                .await
        {
            tracing::warn!(
                company = %self.company,
                pair = %pair.desk_id,
                error = %error,
                "[hive] a referred answer could not be journaled in the pair's thread"
            );
        }
        let mut state = self.state.lock().await;
        state.ledger.asked.push(AskedQuestion {
            asker: referral.source_id.clone(),
            target: referral.target_id.clone(),
            desk: referral.to.desk_id.clone(),
            returned: false,
            crossed: referral.to.desk_id != self.home.desk_id,
        });
        if deliberated {
            // Read by `ret`, which is the only place that can still name the
            // answerer and by then has nothing but the `Referral` to go on.
            state.room_answers.insert(referral.to.desk_id.clone());
        }
        state.last_answer = Some((referral.clone(), answer));
        EnqueueOutcome::Enqueued
    }

    /// Carry one answer back to the desk that asked.
    ///
    /// Runs no turn. The far desk has already answered; a `Return` that ran a
    /// second turn would be the asker restating an answer it was handed, which
    /// costs a model call to add nothing and lands a *trace* on the asking desk
    /// under a member's own id.
    async fn ret(&self, referral: &Referral) -> EnqueueOutcome {
        let desk = self.desk_name(&referral.from.desk_id);
        // **Who to credit.** A single-seat crossing is answered by the seat and
        // says so. A room's conclusion is not any one member's line — it is
        // what the desk converged on, and several of its seats may have argued
        // the other way — so naming the responder the library happened to pick
        // would put words in a teammate's mouth that the teammate did not say,
        // over on a desk where nobody can check.
        let by_room = self
            .state
            .lock()
            .await
            .room_answers
            .remove(&referral.from.desk_id);
        let text = match by_room {
            true => room_note(&desk, &referral.content),
            false => returned_note(&referral.source_id, &desk, &referral.content),
        };
        if let Err(error) = self
            .journal(&referral.to, super::HIVE_REFERRAL_AUTHOR, text)
            .await
        {
            tracing::warn!(
                company = %self.company,
                desk = %referral.to.desk_id,
                error = %error,
                "[hive] a referred answer could not be carried back"
            );
            return EnqueueOutcome::Refused {
                reason: EnqueueRefusal::TargetUnavailable,
            };
        }
        let mut state = self.state.lock().await;
        // The last question asked is the one this answers. `consider` folds a
        // forward and then, in the same call, its return — so the two strictly
        // alternate and there is no interleaving for the index to get wrong.
        if let Some(last) = state.ledger.asked.last_mut() {
            last.returned = true;
        }
        EnqueueOutcome::Enqueued
    }
}

impl ReferralQueue for EpisodeReferrals<'_> {
    fn enqueue_once(&self, referral: Referral) -> ReferralFuture<'_> {
        Box::pin(async move {
            let key = (
                referral.from.desk_id.clone(),
                referral.from.thread_root,
                referral.key.trigger_sequence,
            );
            {
                let mut state = self.state.lock().await;
                if !state.seen.insert(key) {
                    return Ok(EnqueueOutcome::Already);
                }
                // The width bound the library leaves to the host. Counted only
                // for forwards: a return spends no turn, and refusing one would
                // strand an answer this episode has already paid for.
                if matches!(referral.kind, ReferralKind::Forward) {
                    if state.asked >= self.peer_cap {
                        state.ledger.over_cap = state.ledger.over_cap.saturating_add(1);
                        return Ok(EnqueueOutcome::Refused {
                            reason: EnqueueRefusal::FeatureDisabled,
                        });
                    }
                    state.asked = state.asked.saturating_add(1);
                }
            }
            // **The marker, before the turn it authorizes.**
            //
            // A crossing raised inside a room used to leave no trace on the
            // journal at all: this adapter kept its idempotency in memory and
            // dispatched straight to the runner, so the console had nothing to
            // attach a chip or a transcript to and a room's question was
            // invisible to the operator it was asked on behalf of. The chat
            // path has always written one (`JournalReferralQueue`); writing it
            // here too means every crossing is recorded the same way, whichever
            // path raised it.
            //
            // Best-effort: a marker that cannot be appended is logged and the
            // turn still runs, for the reason the rest of this module gives —
            // a question that went unrecorded is a worse episode, not a broken
            // one.
            self.mark(&referral).await;
            Ok(match referral.kind {
                ReferralKind::Forward => self.forward(&referral).await,
                ReferralKind::Return => self.ret(&referral).await,
            })
        })
    }
}

/// Consider one committed episode line for a referral, and carry back whatever
/// it earned.
///
/// This is the whole of the host's obligation the library names: authorize both
/// conversations (the caller has, by building the federation from the company's
/// own desks), carry the origin back (the second `dispatch_referral` below),
/// and bound the width ([`ReferralConfig::peer_cap`], enforced in the queue).
///
/// Returns nothing, and cannot fail the episode. Whatever this decides lands in
/// the journal the next speaker folds and in the ledger the closing report
/// reads, so there is no second answer for a caller to act on. A malformed
/// snapshot, a refused enqueue and a far turn that did not finish are logged
/// and dropped, for the same reason a member's own failed turn is: a question
/// that went unanswered is a worse episode, not a broken one.
pub async fn consider(
    queue: &EpisodeReferrals<'_>,
    policy: ReferralPolicy,
    federation: &HiveFederation,
    conversation: &DispatchConversation,
    author_id: &str,
    content: &str,
    seq: EventSeq,
) {
    if !policy.enabled {
        return;
    }
    let members = federation.roster_members();
    let desks = federation.desk_set();
    let retired: Vec<String> = Vec::new();
    let roster = Roster::new(&members, &[], &retired);
    let desk_set = DeskSet::new(&desks, &[], &[], &[], &retired);
    let mentions: Vec<Mention> = resolve_mentions(
        content,
        None,
        &MentionAuthor::Agent {
            id: author_id.to_owned(),
        },
        &roster,
        &desk_set,
    );
    // Recorded before the decision, because the decision cannot tell a person
    // from a desk afterwards — see `ReferralState::named`.
    queue.note_named(seq.value(), &mentions).await;
    if mentions.is_empty() {
        return;
    }
    let input = ReferralInput {
        key: DispatchKey {
            trigger_sequence: seq.value(),
        },
        conversation: conversation.clone(),
        author_id: author_id.to_owned(),
        content: content.to_owned(),
        mentions,
        hop: 0,
        origin: None,
    };
    let outcome = dispatch_referral(queue, policy, &input, &roster, &desk_set).await;
    match outcome {
        Ok(ReferralOutcome::Referred { .. }) => {}
        Ok(ReferralOutcome::NotReferred { reason }) => {
            // Debug rather than warn: every rung here is an ordinary "this line
            // did not ask anybody anything", and the common ones
            // (`NoReferralTarget`, `SelfDesk`) are what a normal deliberation
            // line produces every single turn.
            tracing::debug!(reason = ?reason, "[hive] no referral from this line");
            return;
        }
        Ok(other) => {
            tracing::debug!(outcome = ?other, "[hive] referral not enqueued");
            return;
        }
        Err(error) => {
            tracing::warn!(error = %error, "[hive] referral fold refused this line");
            return;
        }
    }
    // The answer comes back. The library holds no state, so the origin it put
    // on the forward has to be handed to it again as the *input* of the reply
    // that answers it — that is the one thing this host owes it, and skipping
    // it is how an answer ends up with no way home.
    let Some((forward, answer)) = queue.take_answer().await else {
        return;
    };
    if !policy.returns || !forward.crosses() {
        // A forward that did not cross needs no return: the answer was
        // appended to the very conversation the asker is reading.
        return;
    }
    let Some(origin) = forward.origin.clone() else {
        return;
    };
    let reply = ReferralInput {
        key: DispatchKey {
            // Distinct from the forward's key so the queue's per-episode
            // idempotency set does not read the return as a duplicate of the
            // question that caused it. It cannot collide with a later line's
            // own forward either: the key is scoped by conversation, this one
            // is scoped by `forward.to`, and the only path that reaches here
            // is a forward that *crossed* — so `forward.to` is a desk this
            // episode never deliberates on.
            trigger_sequence: forward.key.trigger_sequence.saturating_add(1),
        },
        conversation: forward.to.clone(),
        author_id: forward.target_id.clone(),
        content: answer,
        mentions: Vec::new(),
        hop: forward.child_hop,
        origin: Some(ReferralOrigin {
            conversation: origin.conversation,
            asker_id: origin.asker_id,
        }),
    };
    match dispatch_referral(queue, policy, &reply, &roster, &desk_set).await {
        Ok(ReferralOutcome::Referred { .. }) => {}
        Ok(ReferralOutcome::NotReferred {
            reason: NoReferralReason::HopLimitReached,
        }) => {
            tracing::warn!(
                "[hive] a crossing question's answer had no hop left to come home on; \
                 raise `hive.referral.max_hops` to at least 2"
            );
        }
        Ok(other) => tracing::debug!(outcome = ?other, "[hive] answer not carried back"),
        Err(error) => tracing::warn!(error = %error, "[hive] carrying an answer back failed"),
    }
}
