//! What one authorized turn is shown.
//!
//! This is a port of the reference host's prompt builder
//! (`tinyhivemind-hive/examples/bench/live.rs`), and it is deliberately a port
//! rather than a fresh design: every block here exists because a live room
//! failed without it.
//!
//! - **The move list is phase-gated.** `!commit` is absent while the room is
//!   still deliberating, because the library authorizes a commit turn by
//!   setting the phase — a `!commit` deposited before that adds no supporter to
//!   anything, and a room that reaches for it early spends its whole budget
//!   recording a decision it never reached. In the Commit phase it is present
//!   for **every** seat, and the block names the carried topic outright: the
//!   fold hands the floor to whoever the attention market picks, so the seat
//!   asked to record a decision is regularly not the seat that reached it.
//! - **The task's topic id is stated.** A room that coins `#euler12`,
//!   `#euler12-triangle` and `#euler12-triangular` for one number splits its
//!   own support three ways and never carries anything. The id is derived once
//!   from the operator's message and every prompt of the episode repeats it.
//! - **The floor is rendered with its standings.** Models coin a fresh topic id
//!   for an idea the room already has one for (`#rollout` and
//!   `#rollout-strategy` in one episode), and support split across two names
//!   never adds up to a quorum. A member also cannot tell that one more
//!   supporter would settle a question unless it is shown how far each option
//!   is from carrying.
//! - **A member is shown its own last line.** Live models restate their
//!   previous line verbatim when they have nothing new; `repetition_cap` damps
//!   a restated *support* and cannot see this at all.
//! - **The pinboard is rendered.** The transcript window is thirty messages, so
//!   what the desk settled long ago is otherwise gone; a `!pin` is how it stays
//!   unavoidable.
//!
//! The whole prompt is assembled from the projection the library handed this
//! turn, so a host driving a live run can predict exactly what an agent sees
//! from the transcript plus the turn's own visibility.

use tinyhivemind_hive::{
    HiveTurn, Phase, QuorumPolicy, Sequence, SessionAuthor, SessionMessage, Visibility,
    pins::Pin,
    quorum::{TopicStanding, standings},
    trace::resolve,
};

use super::memory::{HiveMemoryHit, MAX_RECALL_CHARS};
use super::types::{HiveDesk, HiveMember};

/// One line of the move list, per kind.
///
/// Rendered per seat rather than as one fixed block: a desk that assigns moves
/// must not show a member a marker it is about to be corrected for using. The
/// prompt and the enforcement therefore read the same table, so what a member
/// is shown is exactly what it is allowed to deposit.
fn move_line(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "propose" => "!propose #topic  then one sentence putting a new option on the floor",
        "support" => "!support #topic ^N  then why, citing message N as grounds",
        "object" => "!object >N ^M  then why, objecting to message N and citing message M",
        "refute" => {
            "!refute #topic ^N  then the fact that argues against the option itself, citing \
             message N"
        }
        "evidence" => {
            "!evidence #topic ^N  then a fact, adding grounds without taking a side; keep the # \
             when the fact bears on a named option"
        }
        "question" => "!question  then what you need that nobody has established",
        "defer" => "!defer #topic  then who should answer instead, when this is not your area",
        "pin" => {
            "!pin ^N  then why message N must stay on the board once the window has scrolled past \
             it"
        }
        _ => return None,
    })
}

/// The rules those moves are read under.
/// **Write about yourself in the first person.**
///
/// The transcript attributes every row, the reader's own included — `[20]
/// refunds (you):` — so a member can see which lines are its own. Seeing them
/// is not the same as writing that way: rows are labelled by id, and a member
/// copies the convention it was shown, which produced commits like "both
/// refunds and exchanges agreed" written BY refunds — a member describing
/// itself as somebody else in the one line whose job is to say who carried the
/// decision.
///
/// Naming a COLLEAGUE by id stays right: that is how a citation is read back
/// and how the fold counts them.
const FIRST_PERSON_RULE: &str = "Lines marked `(you)` in the transcript are your own. Write about \
yourself in the first person — never by your own id — and name colleagues by their id as usual.";

/// **A turn is work, then one line — not one line instead of work.**
///
/// The move grammar describes the LINE a turn ends with, and a seat reading only
/// that treats the whole turn as speech: it reasons from whatever facts happen to
/// be in the transcript, and when there are none it asks the room instead of
/// looking. Observed, with the tools sitting in its own belt: a two-seat desk
/// spent all eight turns asking each other who held which tool and exhausted its
/// budget without a single read; a seat wrote "I hold the tool and can produce the
/// answer this turn" and then produced a line about the answer rather than the
/// answer. The one time a seat did call a tool, the operator's message had
/// literally told it to.
///
/// Nothing was blocking those calls. `speak()` runs the ordinary turn machinery,
/// the belt is built the same way, and the same agent on the same desk calls the
/// same tools freely when it answers outside a room. What was missing is that
/// nobody asked it to: the prompt requested a position and it gave one.
///
/// So the one-line contract stays exactly as it was — the fold reads the final
/// line and nothing else — and this says what the turn is allowed to do BEFORE
/// that line, which is everything an ordinary turn may do.
const WORK_BEFORE_LINE: &str = "\
Before you write that line, USE YOUR TOOLS. A turn is work and then one line, \
not one line instead of work. Look up what you need — the order, the item, the \
product's variants, the customer — rather than asking the room for a fact you \
can fetch yourself, and rather than reasoning from what happens to be in the \
transcript already. A seat that asks its colleagues what it could have read \
costs the room a turn and adds nothing.\n\
And when the room has already carried an option that YOUR tool performs, \
perform it in this turn, then write the line saying you did. Deciding is not \
doing: no step after the room closes will carry out what it settled on, so an \
action nobody performs never happens, however clearly it was agreed.";

const DELIBERATE_RULES: &str = "\
The # on a topic and the ^ on a citation are part of the grammar: `!propose \
#canary ...` names an option, `!propose canary ...` names nothing and is \
discarded. A support with no ^citation does not count, and only support moves \
an option towards a decision. Angle brackets are not part of any line — write \
the sentence itself, not a placeholder in brackets. The marker is what the \
room counts and your prose is not, so never write !support for one option \
while arguing for another: put the marker on the option you actually mean. Do \
not write !commit: the room has not reached a decision yet, and a commit line \
now counts for nothing. !defer costs you this turn and adds no support to \
anything. Use it when the question on the floor turns on something you do not \
hold and somebody here does: a confident guess from outside your area is \
worse for the room than saying so and standing aside. If you have nothing to \
add, reply !defer #topic naming who should act next, or !question. Prose \
without a marker counts for nothing and costs the room a turn. Write nothing \
before or after the single marker line.";

/// The two markers a desk that enabled private asides adds, and the rules they
/// are read under.
///
/// Rendered **only when the desk enabled asides**, because a grammar is a fixed
/// cost paid in every agent's system text on every turn, and teaching a move
/// nobody may make spends that budget for nothing.
///
/// The fourth sentence is the one an agent can act on rather than a disclaimer:
/// a reader that meets an elided row needs to know it may ask, not merely that
/// it cannot read.
const ASIDE_RULES: &str = "\
This desk also allows a private line, and it costs you nothing. After your one \
marker line you may add ONE more line, !aside @peer <what you need from them>, \
which only that peer can read. It is not a turn and does not replace your move: \
write your move first, then the aside under it. Your peer answers on its own \
next turn. !surface <what the room needs to know> reports back to everyone and \
is an ordinary move. An aside carries information and never support: a !support \
written privately moves nothing towards a decision, for you or for anybody, so \
an aside you never surface bought the room nothing. Some rows in the transcript \
show only that an aside happened, with its author and who was in it — you \
cannot read those, and if one matters, ask its author here on the desk.";

/// What a member is told on a turn it cannot see its peers on.
///
/// **Deposit what you know, do not advocate what you want.** This sentence used
/// to read "Form your own first", and that instruction is the single biggest
/// measured defect in the deliberation protocol.
///
/// The blind round exists so members form positions independently. But on any
/// question where members hold *correlated* priors and one member holds the
/// decisive fact — the shape the literature calls a hidden profile, and the
/// shape this company's desks actually have, since the fleet technician holds
/// machine facts nobody else does — independence is exactly what makes it
/// fatal. Every member opens by advocating what its own reading favours, a
/// proposal counts as its own author's support in `tinyhivemind`, and the
/// option the shared prior favours therefore reaches quorum *inside the blind
/// round*, before the one informed member has been able to say anything. A
/// traced episode shows it happening in five turns: four seats propose the
/// decoy, the fifth proposes the truth, the room enters the Commit phase, and
/// the dissenting fact arrives one turn too late to count.
///
/// The room is not converging there. It is amplifying a shared error and
/// calling the result agreement.
///
/// Depositing instead is measured, on the deliberation benchmark over 2000
/// seeded rooms (`vendor/tinyhivemind/crates/tinyhivemind-hive/examples/bench`,
/// `--blind-evidence`):
///
/// | profile | advocate first | deposit first |
/// | --- | --- | --- |
/// | hidden (one member holds the fact) | 16.2% | **66.6%** |
/// | uniform (everyone holds a noisy copy) | 78.8% | 75.1% |
///
/// It costs 3.7 points where every member's reading is equally good, and buys
/// **fifty** where one member knows something the others do not. A desk of
/// specialists is the second case by construction, which is why this host takes
/// the trade for every desk rather than making it a knob.
///
/// Raising `quorum` was tried first and does not work: at unanimity the room
/// simply stops deciding (31% of episodes reach a decision, and accuracy falls
/// to 10.1%). The bar is not the problem; what the bar is counting is.
const BLIND_SIGHT: &str = "\
You cannot yet see your peers' positions. Put what you *know* on the floor — \
the fact, figure or reading you hold that the others may not — rather than the \
option you already favour. If your seat may deposit evidence, deposit it: the \
room can weigh a fact it has been shown, and cannot weigh one you kept while \
arguing from it.";

/// The extra sentence a room under `require_evidential` is given.
///
/// Rendered only when the desk actually requires it, because on a desk that
/// does not, citing a proposal is a perfectly good support and a rule saying
/// otherwise would be false.
const EVIDENTIAL_RULE: &str = "\
This desk counts a !support only when its ^citation reaches an !evidence line \
— either the evidence itself, or a support that cites it. A !support that \
cites only a !propose counts for nothing, however well argued: the room needs \
a fact under the option, not a second opinion about it. If no evidence is on \
the floor yet, put one there with !evidence #topic ^N.";

/// The extra sentence a seat with an assigned grammar is given.
///
/// Only rendered when the desk actually narrowed this member, because a member
/// that may make every move is not being restricted and telling it so would be
/// a rule about nothing.
const ASSIGNED_MOVES_RULE: &str = "\
These are the ONLY markers this desk gives you. A line opening with any other \
marker is handed back to you once for correction, and on a second attempt it \
is journaled with its marker stripped — it will say what you wrote and count \
for nothing.";

/// The move every seat has once the room has reached quorum, naming the topic
/// that carried.
///
/// The topic is named rather than described because the seat holding the floor
/// in the Commit phase is regularly not one of the seats that carried it: the
/// attention market picks the speaker, so the cheapest member on the desk can
/// be the one asked to do the bookkeeping. Told *which* id to record, it needs
/// to re-derive nothing.
fn commit_protocol(topic: &str) -> String {
    format!(
        "The room has reached quorum and carried `#{topic}`; record it. Reply with ONE line \
         only:\n!commit #{topic} ^N  then why, citing the evidence it rests on\nKeep the # on the \
         topic and the ^ on the citation; without them the line records nothing. Angle brackets \
         are not part of the line — write the sentence itself. This is bookkeeping, not a fresh \
         judgement: record the topic the room actually settled on rather than the one you would \
         have preferred, and do not re-derive the answer. {FIRST_PERSON_RULE} Write nothing before or after the \
         single marker line."
    )
}

/// Everything about one seat in a room except how its answer is fetched.
#[derive(Debug)]
pub struct EpisodePrompt<'a> {
    /// The member taking this turn.
    member: &'a HiveMember,
    /// The desk it is taking it on.
    desk: &'a HiveDesk,
    /// What the operator asked, verbatim — the reason the room opened.
    task: &'a str,
    /// The room's quorum rule. Public on purpose: a participant is entitled to
    /// know how many grounded supporters settle a question, and a member that
    /// cannot see how close an option is has no way to know one more supporter
    /// would end it.
    quorum: QuorumPolicy,
    /// The desk's pinboard, folded from the same journal the transcript came
    /// from.
    pins: &'a [Pin],
    /// What the desk remembers, recalled once at the top of the episode.
    recall: &'a [HiveMemoryHit],
    /// Members who have not taken a turn yet in this episode, when the fold has
    /// just handed the floor back to whoever spoke last.
    unspoken: &'a [String],
    /// The other desks this seat may put a question to, when the desk opted in
    /// to referral. Empty otherwise, and the block is then not rendered at all.
    peers: Vec<(String, String, Option<String>)>,
    /// This seat's *other* conversations — every other desk it is seated on and
    /// its own direct line — each already projected for it, newest window last.
    ///
    /// Empty for a seat with nowhere else to be, and the block is then not
    /// rendered at all. These rows are context and never floor: they are outside
    /// the fold `step` runs, so nothing here can move an option towards a
    /// decision on this desk.
    elsewhere: &'a [(String, Vec<SessionMessage>)],
    /// The episode's watermark — `EpisodeDriver::run`'s `trigger` — or `None`
    /// for a caller that never set one.
    ///
    /// `None` is not "the watermark is sequence zero": it means no divider is
    /// ever drawn, which is what every caller before this field existed got,
    /// byte for byte. A caller that does set a trigger and hands a transcript
    /// wholly above it gets the same output for the same reason — the divider
    /// is drawn only between two non-empty regions.
    trigger: Option<Sequence>,
}

impl<'a> EpisodePrompt<'a> {
    /// Build the prompt state for one seat.
    #[must_use]
    pub fn new(
        member: &'a HiveMember,
        desk: &'a HiveDesk,
        task: &'a str,
        quorum: QuorumPolicy,
        pins: &'a [Pin],
    ) -> Self {
        Self {
            member,
            desk,
            task,
            quorum,
            pins,
            recall: &[],
            unspoken: &[],
            peers: Vec::new(),
            elsewhere: &[],
            trigger: None,
        }
    }

    /// Render what the desk remembers from earlier episodes into this prompt.
    ///
    /// Recalled once per episode by the driver and handed to every turn, so a
    /// room does not pay one store round-trip per speaker for an answer that
    /// cannot change mid-episode.
    #[must_use]
    pub fn with_recall(mut self, recall: &'a [HiveMemoryHit]) -> Self {
        self.recall = recall;
        self
    }

    /// Name the members who have not spoken yet in this episode.
    ///
    /// Rendered only when the driver has something to say with it — the fold
    /// gave the floor back to the member who just held it while somebody has
    /// still not used theirs. It is a *prompt*, never an override: the library
    /// picked this speaker under invariants this host does not get to break, so
    /// the repair available is to tell the speaker who is missing and let it
    /// `!question` or `!defer` to them.
    #[must_use]
    pub fn with_unspoken(mut self, unspoken: &'a [String]) -> Self {
        self.unspoken = unspoken;
        self
    }

    /// Name the other desks this seat may ask a question of.
    ///
    /// Rendered only for a desk that opted in to referral and only when the
    /// company has a peer with somebody on it, because a seat told it may ask
    /// `@#platform` when no such desk exists has been handed a move it cannot
    /// make — and a member that spends its one line on an impossible move has
    /// spent a turn of the room's budget on nothing.
    ///
    /// Timing is why this is worth the tokens at all. tinyhivemind's own
    /// benchmark found the largest single effect was not in the mechanism but
    /// in *when* a desk asks: a room whose members share a blind spot reaches
    /// quorum inside its own opening round, so a question asked after the desk
    /// has backed something arrives as information it has already voted past.
    /// The block therefore says to ask early, in as many words.
    #[must_use]
    pub fn with_peers(mut self, peers: Vec<(String, String, Option<String>)>) -> Self {
        self.peers = peers;
        self
    }

    /// Name the episode's watermark, so the transcript can show where its own
    /// floor begins.
    ///
    /// The live failure this exists for: a six-seat run's episode for one
    /// Project Euler problem spent its whole eighteen-turn budget arguing the
    /// *previous* problem, because the transcript rendered a prior episode's
    /// live-looking `!propose` and `!support` rows exactly like its own. The
    /// watermark this host already folds on (`EpisodeDriver::run`'s `trigger`)
    /// says precisely where the fold stops treating a row as a vote; this
    /// builder is that same line, drawn where a reader can see it.
    #[must_use]
    pub fn with_trigger(mut self, trigger: Sequence) -> Self {
        self.trigger = Some(trigger);
        self
    }

    /// Give this seat the conversations it is part of elsewhere.
    ///
    /// Each entry is `(label, rows)` where the rows were projected for **this
    /// member** — `Viewer::Agent`, never the operator — so an aside it is not in
    /// arrives elided rather than readable, exactly as it would on its own desk.
    #[must_use]
    pub fn with_elsewhere(mut self, elsewhere: &'a [(String, Vec<SessionMessage>)]) -> Self {
        self.elsewhere = elsewhere;
        self
    }

    /// Render exactly what this turn is allowed to see.
    #[must_use]
    pub fn render(&self, turn: &HiveTurn, visible: &[SessionMessage]) -> String {
        let sight = match turn.visibility {
            Visibility::Blind => BLIND_SIGHT,
            Visibility::Full => "You can see the whole room.",
        };
        // Folded once and read twice: the block a member reads its standings
        // off and the topic the Commit phase names have to be the same fold,
        // or a seat could be told to record a topic the floor does not show.
        let standings = self.standings(visible);
        let protocol = match turn.phase {
            Phase::Commit => commit_protocol(&self.carried(&standings)),
            Phase::Deliberate => self.deliberate_protocol(),
        };
        format!(
            "You are @{}, the {} on the {} desk. {sight}\n\n{}{}{}\n\n{protocol}\n\n{}{}{}{}{}{}\
             Shared attributed transcript:\n{}\n\nYour one line:",
            self.member.id,
            self.member.role,
            self.desk.name,
            self.room(),
            self.remembered(),
            self.board(),
            self.topic_discipline(turn.phase),
            self.floor(&standings),
            self.missing(),
            self.peers(),
            self.elsewhere(),
            self.last_line(visible),
            render_transcript(visible, self.trigger, Some(&self.member.id)),
        )
    }

    /// The topic id this task's answer is named by, and the rule for coining
    /// another.
    ///
    /// Rendered only while the room is deliberating: the Commit block names the
    /// carried id outright, so repeating the derivation rule there would be a
    /// second, weaker instruction about the same string.
    fn topic_discipline(&self, phase: Phase) -> String {
        if phase == Phase::Commit {
            return String::new();
        }
        let topic = canonical_topic(self.task);
        format!(
            "Topic id for this task's answer: `#{topic}`. Every !propose, !support and !evidence \
             about the answer uses exactly this id. Only a genuinely different candidate value \
             gets a different id (`#{topic}-2`). Never invent a synonym for an id already on the \
             floor.\n\n",
        )
    }

    /// The topic the room has carried, for the Commit phase to name.
    ///
    /// Falls back to the task's canonical id when the fold shows nothing
    /// carried — which the Commit phase should make impossible, but a prompt
    /// naming `#answer` is a better failure than one naming an empty string.
    fn carried(&self, standings: &[TopicStanding]) -> String {
        standings
            .iter()
            .find(|standing| standing.carried(&self.quorum))
            .map_or_else(
                || canonical_topic(self.task),
                |standing| standing.topic.to_string(),
            )
    }

    /// The markers this seat may open a line with while the room deliberates,
    /// and the rules they are read under.
    ///
    /// Phase-gated on top of the per-member grammar, in that order: `!commit`
    /// is the library's to authorize, and the deliberation markers are the
    /// desk's to assign. `!question` and `!defer` are in every seat's list
    /// whatever the table says (see
    /// [`UNGATED_KINDS`](super::moves::UNGATED_KINDS)), so this block always
    /// offers a member with nothing to add something to say that is not prose.
    fn deliberate_protocol(&self) -> String {
        let allowed = self.desk.config.moves_for(&self.member.id);
        let assigned = allowed.len() < super::moves::MOVE_KINDS.len();
        let lines: Vec<&str> = allowed
            .iter()
            .filter(|kind| **kind != "commit")
            .filter_map(|kind| move_line(kind))
            .collect();
        debug_assert!(
            !lines.is_empty(),
            "every seat keeps !question and !defer, so a deliberating member always has a marker",
        );
        let head = "Reply with ONE line only, beginning with exactly one of these markers:";
        let mut tail = DELIBERATE_RULES.to_owned();
        tail.push('\n');
        tail.push_str(WORK_BEFORE_LINE);
        tail.push('\n');
        tail.push_str(FIRST_PERSON_RULE);
        if self.quorum.require_evidential {
            tail.push('\n');
            tail.push_str(EVIDENTIAL_RULE);
        }
        if assigned {
            tail.push('\n');
            tail.push_str(ASSIGNED_MOVES_RULE);
        }
        // Last, and only when the desk opted in. `!aside` and `!surface` are
        // not deliberation markers — neither appears in `MOVE_KINDS`, neither
        // is gated by `hive.moves`, and neither deposits a trace — so they are
        // taught after the move list rather than inside it.
        if self.desk.config.aside.enabled() {
            tail.push('\n');
            tail.push_str(ASIDE_RULES);
        }
        format!("{head}\n{}\n{tail}", lines.join("\n"))
    }

    /// What the desk remembers, or nothing when it remembers nothing.
    ///
    /// Attributed as memory rather than rendered into the transcript, and
    /// carrying no sequence, because it is not a message on this desk: a member
    /// that could cite it with `^N` would be grounding a decision in a number
    /// nothing in this conversation answers to.
    fn remembered(&self) -> String {
        if self.recall.is_empty() {
            return String::new();
        }
        let lines = self
            .recall
            .iter()
            .map(|hit| {
                let flat = hit.snippet.split_whitespace().collect::<Vec<_>>().join(" ");
                format!("- {}", truncate_chars(&flat, MAX_RECALL_CHARS))
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\nThe desk remembers:\n(From earlier episodes on this desk. This is memory, not a \
             line in this conversation — it has no message number and cannot be cited with ^.)\n\
             {lines}\n",
        )
    }

    /// Who has not spoken yet, when the driver asked for it to be said.
    fn missing(&self) -> String {
        if self.unspoken.is_empty() {
            return String::new();
        }
        let who = self
            .unspoken
            .iter()
            .map(|id| format!("@{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Members who have not spoken yet: {who}. You have the floor twice in a row. If what \
             the room is missing is theirs to supply, !question them or !defer #topic to them \
             rather than restating your own position.\n\n",
        )
    }

    /// The other desks this seat may ask, or nothing when it may ask none.
    ///
    /// One question per line, and only one seat's worth: the far desk answers
    /// with a single turn by a single member, so this is a colleague to consult
    /// and not a channel to broadcast into.
    fn peers(&self) -> String {
        if self.peers.is_empty() {
            return String::new();
        }
        let listed = self
            .peers
            .iter()
            .map(|(id, name, about)| match about {
                Some(about) => format!("- @#{id} ({name}) — {about}"),
                None => format!("- @#{id} ({name})"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Other desks you may put ONE question to, by writing their handle in your line:\n             {listed}\n             Ask only for a fact this desk does not hold and cannot check for itself, and ask it              EARLY — a room that has already backed an answer has voted past whatever comes back.              Exactly one member of that desk answers, and their answer arrives as a message you              can read and cite; it is not a vote, and it supports nothing here until one of us              spends a line on it.\n\n",
        )
    }

    /// This seat's other conversations, as context it may quote and never as
    /// floor it may count.
    ///
    /// Every row here was written somewhere outside the fold `step` runs — on
    /// another desk this member sits on, or on its own direct line — so quoting
    /// one moves no option towards a decision here. That is the same bargain an
    /// aside strikes and a referral's answer strikes: information crosses a
    /// boundary, support never does. A member that wants the room to act on
    /// something it read elsewhere spends its own line saying so, on this desk.
    ///
    /// Rendered per conversation rather than merged into one list, because two
    /// desks number their rows independently and a merged `[7]` would name two
    /// different messages.
    fn elsewhere(&self) -> String {
        let blocks: Vec<String> = self
            .elsewhere
            .iter()
            .filter(|(_, messages)| !messages.is_empty())
            .map(|(label, messages)| {
                format!(
                    "{label}:\n{}",
                    // No trigger: the episode watermark belongs to *this* desk,
                    // and drawing its divider through another desk's rows would
                    // claim a boundary that conversation never had.
                    render_transcript(messages, None, Some(&self.member.id)),
                )
            })
            .collect();
        if blocks.is_empty() {
            return String::new();
        }
        format!(
            "Elsewhere you are part of. Reference only: these rows are not on this desk's \
             floor, so citing one settles nothing here. If something below matters to the \
             question in front of you, say it yourself in your own line.\n\n{}\n\n",
            blocks.join("\n\n"),
        )
    }

    /// Who else is in the room, what the desk is for, and what was asked.
    fn room(&self) -> String {
        let teammates: Vec<String> = self
            .desk
            .members
            .iter()
            .filter(|member| member.id != self.member.id)
            .map(|member| format!("@{} ({})", member.id, member.role))
            .collect();
        let mut room = String::new();
        if let Some(description) = &self.desk.description {
            room.push_str(&format!("This desk is for: {description}\n"));
        }
        if !teammates.is_empty() {
            room.push_str(&format!(
                "In the room with you: {}. Address them by the ids above.\n",
                teammates.join(", "),
            ));
        }
        room.push_str(&format!("The operator asked the desk:\n{}\n", self.task));
        room
    }

    /// The desk's pinboard, or nothing when it holds nothing.
    ///
    /// Rendered from the fold rather than restated as prose: a pin is a
    /// sequence the room can cite with `^N`, so the sequence has to be visible.
    fn board(&self) -> String {
        if self.pins.is_empty() {
            return String::new();
        }
        let lines = self
            .pins
            .iter()
            .map(|pin| {
                let label = pin
                    .label
                    .as_ref()
                    .map_or_else(String::new, |label| format!(" #{label}"));
                let body = pin
                    .excerpt
                    .as_deref()
                    .or(pin.note.as_deref())
                    .unwrap_or("(pinned)");
                format!("[{}]{label} {body}", pin.sequence)
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("\nPinned on this desk, whatever else has scrolled away:\n{lines}\n")
    }

    /// This turn's standings, folded with the same [`standings`] the episode
    /// itself uses, so the numbers a member reads are the numbers the room will
    /// decide on.
    ///
    /// An unfoldable transcript yields nothing rather than an error: the floor
    /// block and the carried topic both degrade to their "nothing yet" shapes,
    /// which is a worse prompt and not a failed turn.
    fn standings(&self, visible: &[SessionMessage]) -> Vec<TopicStanding> {
        let traces: Vec<_> = visible
            .iter()
            .flat_map(|message| resolve(&message.content, None, &message.author, message.sequence))
            .collect();
        let at = visible
            .last()
            .map_or(Sequence(0), |message| message.sequence);
        standings(&traces, at, &self.quorum).unwrap_or_default()
    }

    /// The topics on the floor, with the standing the library gives each.
    fn floor(&self, standings: &[TopicStanding]) -> String {
        if standings.is_empty() {
            return format!(
                "No option is on the floor yet, so the id above is still free. An option carries \
                 once {} different members have backed it with grounds.\n\n",
                self.quorum.threshold,
            );
        }
        let floor = standings
            .iter()
            .map(|standing| {
                format!(
                    "#{} — {} of the {} supporters it needs ({})",
                    standing.topic,
                    standing.supporters.len(),
                    self.quorum.threshold,
                    if standing.supporters.is_empty() {
                        "nobody counted yet".to_owned()
                    } else {
                        standing.supporters.join(", ")
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Options already on the floor. Reuse one of these ids exactly if your point is about \
             it — support split across two names for one idea never adds up to a decision — and \
             only coin a new id for a genuinely different option:\n{floor}\n\n",
        )
    }

    /// The line this member last authored, if any.
    fn last_line(&self, visible: &[SessionMessage]) -> String {
        visible
            .iter()
            .rev()
            .find(|message| match &message.author {
                SessionAuthor::Agent { id, .. } => id == &self.member.id,
                SessionAuthor::Operator
                | SessionAuthor::Person { .. }
                | SessionAuthor::System { .. } => false,
            })
            .map(|message| message.content.trim())
            .map_or_else(String::new, |line| {
                format!(
                    "You already said this, so do not repeat it — say something that moves the \
                     room on:\n{line}\n\n",
                )
            })
    }
}

/// Words a task's first line uses to frame the question rather than to name it.
///
/// Dropped before the id is derived, because they are exactly the tokens two
/// different tasks share: "Project Euler 12" and "Problem: Euler 145" have
/// nothing in common that a topic id should record except the part that
/// differs.
const FRAMING_WORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "for", "to", "in", "on", "with", "please", "project",
    "problem", "question", "task", "solve", "compute", "find", "what", "is", "we", "our",
];

/// How many characters a derived topic id may run to.
const MAX_TOPIC_CHARS: usize = 32;

/// The id every line about this task's answer should use.
///
/// The live failure this exists for: one Project Euler problem produced
/// `#euler12`, `#euler12-triangle` and `#euler12-triangular` in a single
/// episode, so support for one number was split three ways and nothing ever
/// reached a quorum. The room cannot agree on a name it was never given, so the
/// host derives one and every prompt of the episode repeats it.
///
/// An explicit `topic: #foo` line anywhere in the operator's message wins — the
/// operator naming the id is better than any derivation. Otherwise the id is a
/// slug of the message's first line: the part before a leading colon when there
/// is a short one (a title), with framing words dropped and a trailing number
/// folded onto the word before it, so "Project Euler 12: Highly divisible
/// triangular number" becomes `euler12`. A line with nothing left to slug falls
/// back to `answer`.
#[must_use]
pub fn canonical_topic(task: &str) -> String {
    if let Some(declared) = declared_topic(task) {
        return declared;
    }
    let first = task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    // A short head before a colon is a title, and the rest is the statement:
    // slugging the whole line would bury the name under its own description.
    let head = match first.split_once(':') {
        Some((head, _)) if !head.trim().is_empty() && head.split_whitespace().count() <= 6 => head,
        _ => first,
    };
    let mut words: Vec<String> = Vec::new();
    for word in head
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let word = word.to_ascii_lowercase();
        if FRAMING_WORDS.contains(&word.as_str()) {
            continue;
        }
        // A bare number names nothing on its own — it is the index of whatever
        // came before it — so it joins that word rather than becoming one.
        match words.last_mut() {
            Some(last) if word.chars().all(|c| c.is_ascii_digit()) => last.push_str(&word),
            _ => words.push(word),
        }
    }
    words.truncate(2);
    let slug = words.join("-");
    let slug = truncate_chars(&slug, MAX_TOPIC_CHARS)
        .trim_matches('-')
        .trim_end_matches('\u{2026}')
        .to_owned();
    if slug.is_empty() {
        "answer".to_owned()
    } else {
        slug
    }
}

/// The id an operator named outright, if the message names one.
///
/// `topic: #foo`, `Topic: foo` — the `#` is optional, because an operator
/// writing the line at all has already said what they mean and refusing it over
/// a missing sigil would be a rule nobody can see.
fn declared_topic(task: &str) -> Option<String> {
    task.lines()
        .map(str::trim)
        .filter_map(|line| {
            let rest = line
                .strip_prefix("topic:")
                .or_else(|| line.strip_prefix("Topic:"))
                .or_else(|| line.strip_prefix("TOPIC:"))?;
            let word = rest
                .trim()
                .trim_start_matches('#')
                .split_whitespace()
                .next()?;
            let slug: String = word
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect::<String>()
                .to_ascii_lowercase();
            let slug = truncate_chars(&slug, MAX_TOPIC_CHARS)
                .trim_end_matches('\u{2026}')
                .to_owned();
            (!slug.is_empty()).then_some(slug)
        })
        .next()
}

/// The line marking where a prior episode's context ends and this episode's
/// own floor begins.
///
/// Deliberately not a transcript row: it carries no `[N]` prefix, so nothing
/// in the grammar reads it as a citable message, and it never collides with a
/// sequence number a member might cite with `^`. It says three things, because
/// a member acted on the wrong one of them in the live failure this fix is
/// for: the rows above are old (so a live-looking `!propose` up there is not
/// live), they are still legitimate to read and cite (the watermark hides
/// nothing — `EpisodeDriver::run` promises exactly that), and their topics
/// carry no standing here (so a `!support` needs a fresh citation on this
/// side to count for anything).
const EPISODE_DIVIDER: &str = "--- Above: earlier conversation on this desk, from before this \
                                question was asked. Still readable and citable with ^N — none of \
                                it is on this episode's floor. ---";

/// Render an attributed transcript the way every prompt here shows one.
///
/// `[sequence] author: content`, because the sequence IS the citation: a
/// `!support #topic ^12` names message 12, and a member that cannot see the
/// numbers cannot ground anything.
///
/// `trigger` draws [`EPISODE_DIVIDER`] between the rows at or below it — the
/// context `EpisodeDriver::run`'s watermark lets the room read but never
/// folds — and the rows above it, which are this episode's own. The divider
/// is drawn only when both sides are non-empty: a transcript wholly above the
/// watermark (the overwhelmingly common case, and every call site before this
/// parameter existed) renders exactly as it always has, and a caller that
/// never learned a trigger passes `None` and gets the same guarantee.
#[must_use]
pub fn render_transcript(
    visible: &[SessionMessage],
    trigger: Option<Sequence>,
    viewer: Option<&str>,
) -> String {
    let split = trigger
        .map(|trigger| visible.partition_point(|message| message.sequence <= trigger))
        .filter(|&split| split > 0 && split < visible.len());
    let mut lines: Vec<String> = Vec::with_capacity(visible.len() + 1);
    for (index, message) in visible.iter().enumerate() {
        if split == Some(index) {
            lines.push(EPISODE_DIVIDER.to_owned());
        }
        lines.push(render_transcript_line(message, viewer));
    }
    lines.join("\n")
}

/// One transcript row: `[sequence] author: content`.
///
/// The reading member's own rows carry a `(you)` marker after their author id,
/// and everyone else's render by name alone. Without that distinction a member
/// read its own turns in exactly the third person it read its colleagues' —
/// and wrote back in the same voice.
/// Observed live: `software_engineer` closed a room with "carried with support
/// from software_engineer and junior_engineer", crediting itself by name as
/// though it were someone else, because the transcript it had just read gave it
/// no other convention to copy.
///
/// This is the same fix `Speaker::Viewer` is for the chat seed (issue #1956),
/// which the episode's own renderer never had: "there were no colleagues in the
/// room" is one failure, and "there is no *self* in the room" is its twin.
///
/// The id stays in front of the marker rather than being replaced by a bare
/// `You`, because the transcript is the citation surface: a member is named by
/// its id when a colleague objects to `>N` or backs `^N`, so a row it cannot
/// tie back to that id is one it cannot recognise as the thing being argued
/// with. Attribution is upstream's contract here — `tests/hivemind_e2e.rs`
/// parses these rows by author id — and this adds to it rather than
/// substituting for it.
///
/// `None` renders every row by name, for a caller with no reader to speak of.
fn render_transcript_line(message: &SessionMessage, viewer: Option<&str>) -> String {
    if let (Some(viewer), SessionAuthor::Agent { id, .. }) = (viewer, &message.author)
        && id == viewer
    {
        return format!("[{}] {id} (you): {}", message.sequence, message.content);
    }
    let author = match &message.author {
        SessionAuthor::Agent { label, .. }
        | SessionAuthor::Person { label, .. }
        | SessionAuthor::System { label, .. } => label.as_str(),
        SessionAuthor::Operator => "operator",
    };
    format!("[{}] {author}: {}", message.sequence, message.content)
}

/// The one line a turn's answer contributes to the transcript.
///
/// A harness turn can return a paragraph however firmly it was asked for one
/// line, and the whole paragraph would then be rendered back into every
/// subsequent prompt — thirty of those is the transcript window spent on one
/// answer. The marker is what the room counts, so the marker line is what the
/// transcript keeps.
///
/// Colour and banners are stripped first (a harness reply can carry escapes
/// from a tool's captured output), then the marker line is taken if the agent
/// wrapped it in prose. A turn that deposits no trace is still a legal turn, so
/// prose falls through to the first thing the agent actually said.
#[must_use]
pub fn marker_line(text: &str) -> String {
    let text = plain(text);
    let marker = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('!') || line.starts_with('@'));
    marker
        .or_else(|| {
            text.lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('>'))
        })
        .unwrap_or("(no answer)")
        .to_owned()
}

/// Split one turn's reply into its desk-visible line and the aside riding on it.
///
/// An aside **costs no turn** ([ADR 0011]): one authorized turn produces the
/// member's ordinary desk-visible contribution *and*, optionally, one aside row.
/// So a reply may legitimately carry two markers, and this is what separates
/// them — the deliberation marker the room counts, and at most one `!aside`.
///
/// Before this, a reply's `!aside` line was simply lost: [`marker_line`] takes
/// the *first* marker, so a member that proposed and then asked a peer had its
/// aside discarded, and a member that only asked had its whole turn become the
/// aside. Under one-message-one-turn the latter was right; under ADR 0011 it is
/// not, because the turn still owes the room its ordinary contribution.
///
/// **At most one** aside is taken, which is the spec's own bound: a room of *n*
/// members writes at most *n* aside rows per round, and each cost its author a
/// turn it had already won, so no arrangement of asides can outrun the room.
///
/// A reply that is *only* an aside yields the desk line [`marker_line`] would
/// give the rest of the text — in practice `(no answer)`. That is honest rather
/// than lossy: the member did spend its turn without saying anything to the
/// room, and the transcript should show that it did.
///
/// [ADR 0011]: https://github.com/tinyhumansai/tinyhivemind/blob/main/docs/adr/0011-an-aside-rides-alongside-a-turn.md
#[must_use]
pub fn split_reply(text: &str) -> (String, Option<String>) {
    let text = plain(text);
    let aside = text
        .lines()
        .map(str::trim)
        .find(|line| super::aside::opens_aside(line))
        .map(str::to_owned);
    if aside.is_none() {
        return (marker_line(&text), None);
    }
    // The desk line is read from the reply with its aside removed, so an aside
    // written *before* the move does not become the line the room counts.
    let rest = text
        .lines()
        .map(str::trim)
        .filter(|line| !super::aside::opens_aside(line))
        .collect::<Vec<_>>()
        .join("\n");
    (marker_line(&rest), aside)
}

/// Strip ANSI escape sequences from a turn's reply.
fn plain(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            plain.push(character);
            continue;
        }
        // CSI sequences end at their final byte in `@`..=`~`; anything else
        // after the escape is a two-character sequence.
        if characters.next() == Some('[') {
            for byte in characters.by_ref() {
                if ('@'..='~').contains(&byte) {
                    break;
                }
            }
        }
    }
    plain
}

/// Truncate `text` to at most `max` characters, on a char boundary.
///
/// The ellipsis is budgeted *inside* `max`, so the cap never quietly exceeds
/// the bound it advertises — the same accounting
/// `memory_loop::truncate_chars` does
/// for injected prior work, duplicated because that module is harness-gated and
/// this one compiles in every build.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let head: String = text.chars().take(max - 1).collect();
    format!("{head}\u{2026}")
}
