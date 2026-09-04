//! Triage an operator chat message **before** anything is written to the board:
//! is this a question to answer, work to track, or neither?
//!
//! # Two doors, not one (issue #267)
//!
//! Cards reach the board through two independent paths, and a fix on one leaves
//! the other open:
//!
//! 1. **Deterministic** — the REST chat handler runs [`triage_message`] over the
//!    operator's words and opens a `todo` card on [`MessageTriage::Track`]. This
//!    is the path that produced five of the six dead `backlog` cards observed on
//!    a live company ("Call the composio_authorize tool …", four × "Create a
//!    workflow …"), because every one of them leads with an action verb.
//! 2. **The model's own** — the orchestrator calls `spawn_task` /
//!    `delegate_to_desk` / `assign_task` / `review_task`. That is where the
//!    sixth card came from ("Tell what is there in the tasks list" — no action
//!    verb, no request frame, so this module never saw it as work).
//!
//! [`triage_message`] is **Layer A**: it lives in the REST chat handler, is
//! compiled into every build, and fronts both cognition brains. Its answer is
//! also what the harness delegation seam uses to gate door 2 (Layer B) — an
//! [`MessageTriage::Answer`] turn claims the delegation queue for *answering
//! only*, so the model's board-writing tools refuse in its own turn.
//!
//! The gate is a narrowing, not a withdrawal. `delegate_to_desk` still runs on
//! an `Answer` turn — it is how a question the orchestrator cannot answer alone
//! reaches a desk that can — and what stands down is its *card*. So the layer
//! removes the ability to write and never the ability to reply.
//!
//! # Positive triage, and a deliberate lean toward answering
//!
//! The classification is a **three-way** decision rather than a card/no-card
//! boolean, because "not work" and "a question about state" need different
//! treatment: only the latter may gate the model's board tools, and gating
//! small talk would be both pointless and risky.
//!
//! Tie-breakers lean toward [`MessageTriage::Answer`] and, when even that is
//! not clear, toward [`MessageTriage::Chatter`] — which neither cards nor
//! gates, and is the safe middle state. Straight from the issue: *a missed card
//! costs one follow-up message, a spurious card pollutes the board
//! permanently.*
//!
//! Conservative and cheap by design (**no model call**): it fires `Track` on
//! imperative requests and request-framed action asks, `Answer` on
//! interrogatives and read requests ("what's our revenue?", "show me the
//! board"), and `Chatter` on greetings, acknowledgements and everything
//! ambiguous.
//!
//! [`detect_task_intent`] remains as the thin `Track`-only wrapper the card
//! paths call to decide **whether** a message is work. What the card is then
//! *named* is no longer its business: a title is minted through
//! [`mint_task_title`](crate::ports::tasks::mint_task_title), and the card the
//! handler wrote is found again by the message's own sequence position rather
//! than by re-deriving its headline.
//!
//! # What this deliberately is not
//!
//! Not the LLM classifier issue #267 sketches. OpenHuman's `trigger_triage`
//! precedent classifies *external* triggers and pays a fast model to do it;
//! OpenCompany maps its workloads onto [`INFERENCE_TIERS`] but declares no
//! cheap tier for classification, so a pre-turn classifier would add a
//! full-price serial round-trip to every message. It is tracked in **issue
//! #678** along with fast-model routing, a first-class `automate` class, and
//! gating the hosted path — which waits on **#723**, since the hosted brain has
//! no delegation stack to gate until the Medulla transport exists.
//!
//! What this module *does* carry toward it is
//! [`triage_message_detailed`]: the seam that separates a `Chatter` the
//! classifier decided from one it fell back to. Escalating only the abstentions
//! is what keeps such a classifier off the messages this layer already names —
//! the difference between paying per hard message and paying per message.
//!
//! [`INFERENCE_TIERS`]: crate::company::INFERENCE_TIERS

/// The greetings [`small_talk`] answers without a turn — a bare "hi" and its
/// spellings, matched against the whole message like [`GREETINGS`] is.
///
/// A strict subset of [`GREETINGS`] (pinned by
/// `every_pleasantry_is_also_a_greeting`), because the fast path may only ever
/// fire on a message the triage already calls
/// [`Chatter`](MessageTriage::Chatter). Widening this list can therefore never
/// take a card away from a message that was getting one.
const HELLOS: &[&str] = &[
    "hi",
    "hii",
    "hey",
    "hello",
    "yo",
    "sup",
    "gm",
    "good morning",
    "good evening",
];

/// The thanks [`small_talk`] answers without a turn. Same subset rule as
/// [`HELLOS`].
const THANKS: &[&str] = &["thanks", "thank you", "ty", "thx", "cheers"];

/// Bare greeting / acknowledgement messages that must never open a card, matched
/// against the whole message (punctuation stripped). Kept short and exact so a
/// longer message that merely *starts* with "ok" ("ok now build the page")
/// still qualifies.
const GREETINGS: &[&str] = &[
    "hi",
    "hii",
    "hey",
    "hello",
    "yo",
    "sup",
    "gm",
    "good morning",
    "good evening",
    "thanks",
    "thank you",
    "ty",
    "thx",
    "cheers",
    "ok",
    "okay",
    "k",
    "kk",
    "cool",
    "nice",
    "great",
    "awesome",
    "perfect",
    "got it",
    "gotcha",
    "sounds good",
    "sg",
    "np",
    "no problem",
    "yes",
    "yep",
    "yeah",
    "yup",
    "no",
    "nope",
    "sure",
    "done",
    "lgtm",
    "nvm",
];

/// Single-word imperative action verbs. A message whose first word is one of
/// these is an instruction to act.
///
/// `list` used to be here and was removed for issue #267: a bare "list X" is a
/// **read**, not work — "list the tasks" is answered by one `query_company`
/// call and must never mint a card. It now leads [`READ_VERBS`] instead. The
/// removal also narrows [`contains_action`], which is what makes "can you list
/// the tasks?" an [`MessageTriage::Answer`] rather than a request frame around
/// an action verb.
const ACTION_VERBS: &[&str] = &[
    "build",
    "create",
    "make",
    "add",
    "fix",
    "find",
    "get",
    "write",
    "draft",
    "send",
    "schedule",
    "book",
    "research",
    "prepare",
    "plan",
    "generate",
    "design",
    "update",
    "review",
    "analyze",
    "analyse",
    "compile",
    "organize",
    "organise",
    "launch",
    "publish",
    "post",
    "email",
    "message",
    "call",
    "contact",
    "order",
    "buy",
    "hire",
    "recruit",
    "remove",
    "delete",
    "cancel",
    "start",
    "stop",
    "move",
    "close",
    "implement",
    "deploy",
    "configure",
    "install",
    "download",
    "upload",
    "summarize",
    "summarise",
    "track",
    "monitor",
    "investigate",
    "handle",
    "fetch",
    "gather",
    "collect",
    "arrange",
    "coordinate",
    "outreach",
    "onboard",
    "migrate",
    "refactor",
    "test",
    "audit",
    "estimate",
    "calculate",
    "translate",
    "edit",
    "improve",
    "optimize",
    "optimise",
    "setup",
];

/// Multi-word imperative action phrases (checked as a prefix + trailing space).
const ACTION_PHRASES: &[&str] = &[
    "set up",
    "look into",
    "put together",
    "work on",
    "take care of",
    "follow up",
    "reach out",
    "figure out",
    "come up with",
    "draw up",
    "go through",
    "sign up",
    "spin up",
    "kick off",
    "roll out",
    "keep an eye on",
];

/// Request-framing prefixes. On their own these are polite wrappers ("can you",
/// "please"); combined with an action verb *anywhere* in the message they mark a
/// request to act (so "can you build X" fires but "can you tell me the revenue"
/// — no action verb — does not). Also stripped from the title.
const REQUEST_FRAMES: &[&str] = &[
    "can you ",
    "could you ",
    "would you ",
    "will you ",
    "please ",
    "pls ",
    "i need you to ",
    "i want you to ",
    "i'd like you to ",
    "i would like you to ",
    "we need you to ",
    "we want you to ",
    "i need to ",
    "i want to ",
    "we need to ",
    "we want to ",
    "i'd like to ",
    "we'd like to ",
    "let's ",
    "lets ",
    "go ahead and ",
    "make sure to ",
    "make sure you ",
    "can we ",
    "could we ",
];

/// Leading filler / acknowledgement / connective words that a real ask can open
/// with ("ok now build …", "and also fix …"). Stripped before the actionability
/// check and from the title, so the action verb underneath is seen. Kept
/// conservative to avoid eating a genuine first word.
const LEAD_INS: &[&str] = &[
    "ok ",
    "okay ",
    "k ",
    "now ",
    "also ",
    "then ",
    "so ",
    "alright ",
    "and ",
    "next ",
    "plus ",
    "hey ",
    "hi ",
    "yo ",
    "sure ",
    "actually ",
];

/// Lead words that open a question about the company's state. A message
/// starting with one of these wants an answer, not a card (issue #267).
///
/// Deliberately excludes the auxiliaries that double as imperatives — `do`,
/// `have`, `can`, `could`, `should`, `will` — because "do the newsletter" and
/// "have the team look at it" are asks, not questions. The ones they *do* open
/// as questions ("can you tell me the numbers?", "should we ship?") end in a
/// `?` and are caught by the trailing-`?` rule below instead.
const INTERROGATIVE_LEADS: &[&str] = &[
    "what", "what's", "whats", "who", "who's", "whos", "whose", "when", "where", "why", "which",
    "how", "how's", "hows", "is", "are", "was", "were", "anyone", "anybody", "anything",
];

/// Lead verbs that ask to be **told** something rather than to have something
/// done. "Tell what is there in the tasks list" — the sixth dead card from
/// issue #267 — opens with one of these.
const READ_VERBS: &[&str] = &[
    "tell", "show", "explain", "describe", "list", "recap", "clarify", "compare",
];

/// Multi-word read requests (checked as a prefix + word boundary).
///
/// `give me` used to be here and was removed on review of issue #267: every
/// other entry means *tell me* — `walk me through`, `let me know`, `remind me`
/// — while `give me` overwhelmingly means *produce me*. It was the one lead in
/// this list that fired [`MessageTriage::Answer`] on "give me a landing page",
/// which withdraws the board tools from a request to build something. Without
/// it "give me the headcount" falls to [`MessageTriage::Chatter`], which is a
/// deliberate and cheap trade: `Chatter` opens no card and gates nothing, so a
/// read that lands there costs the operator nothing, whereas the gated request
/// cost them the work.
const READ_PHRASES: &[&str] = &["walk me through", "let me know", "remind me"];

/// Phrases that point at the board or a card already on it, rather than at a new
/// deliverable. Word-boundary-checked so "the card" does not fire inside "the
/// cardstock", and object-position-checked ([`board_deixis_is_object`]) so it
/// only fires when the phrase is the request's actual object — not the head of
/// a longer noun ("the board **presentation**") and not the topic of a
/// different object ("a report **about** the board").
const BOARD_DEIXIS: &[&str] = &[
    "the task card",
    "this card",
    "that card",
    "the card",
    "this task",
    "that task",
    "the ticket",
    "the board",
    "the column",
    "the backlog",
    "the kanban",
];

/// Mutable fields of a card. Ambiguous alone ("update the status page" is real
/// work), so they demote only in board context — see
/// [`field_noun_in_board_context`].
const BOARD_FIELD_NOUNS: &[&str] = &["the status", "the priority", "the assignee"];

/// Words that, immediately after a [`BOARD_FIELD_NOUNS`] phrase, mark it as the
/// object of a board operation rather than the head of a longer noun.
///
/// Deliberately excludes `of`: "the status **of** the landing page" makes the
/// landing page the head of the noun phrase — the field belongs to it, so the
/// object of the request is the page, not the card's status field on the
/// board (PR #1949 review, CodeRabbit thread 3895107555). `on`/`to`/`for`/`and`
/// instead introduce a board operation's target value ("change the assignee
/// **to** nova"), which is why they stay.
const BOARD_CONNECTIVES: &[&str] = &["on", "for", "to", "and"];

/// Max length of a generated task title.
const TITLE_MAX: usize = 80;

/// What an operator chat message is, decided before anything reaches the board
/// (issue #267).
///
/// Three states rather than two: `Chatter` is not "a weaker `Answer`" — it is
/// the state in which the runtime asserts *nothing*, so it neither opens a card
/// nor takes the model's board tools away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageTriage {
    /// A question about state, or a request to be told something. Answer it
    /// from the read path; **no board write**, by either door.
    Answer,
    /// Real work. Opens a `todo` card under the carried title.
    Track(String),
    /// Neither — greetings, acknowledgements, and everything too ambiguous to
    /// call. Cards nothing and gates nothing.
    Chatter,
}

impl MessageTriage {
    /// Whether this is [`MessageTriage::Answer`] — the one class that gates the
    /// model's board-writing tools (issue #267, Layer B).
    pub fn is_answer(&self) -> bool {
        matches!(self, Self::Answer)
    }

    /// The task title when this is [`MessageTriage::Track`], else `None`.
    pub fn title(&self) -> Option<&str> {
        match self {
            Self::Track(title) => Some(title),
            _ => None,
        }
    }
}

/// Classifies an operator chat message as [`Answer`](MessageTriage::Answer),
/// [`Track`](MessageTriage::Track) or [`Chatter`](MessageTriage::Chatter).
///
/// # The order is the design
///
/// 1. **Empty** → `Chatter`. Nothing was said.
/// 2. **A bare greeting / acknowledgement** → `Chatter`.
/// 3. **A request frame around an action verb** → `Track`. Checked *before* any
///    question test on purpose: "can you build the landing page?" is work
///    despite the `?`, and reading the `?` first would lose every politely
///    phrased instruction an operator ever types.
/// 4. **A question or read request** → `Answer`: an interrogative lead word, a
///    [`READ_VERBS`] / [`READ_PHRASES`] lead, or a trailing `?` with no action
///    verb anywhere. Both lead-word branches are vetoed when a *later* clause
///    is an imperative ([`later_clause_is_imperative`]) — `Answer` is the one
///    class with teeth, so its entry conditions are the ones held tightest.
/// 5. **A leading imperative** → `Track`.
/// 6. **Anything else** → `Chatter`, the safe middle.
pub fn triage_message(text: &str) -> MessageTriage {
    triage_message_detailed(text).triage
}

/// Whether a rule decided this message, or the classifier simply ran out of
/// rules (issue #678).
///
/// # Why `Chatter` is two different answers
///
/// [`triage_message`] returns `Chatter` from three places, and they do not mean
/// the same thing. An empty message and a bare greeting are *decisions* — the
/// classifier recognised them and is confident nothing should happen. The final
/// arm is an **abstention**: no rule matched, and `Chatter` is chosen because it
/// asserts nothing, not because the message was understood.
///
/// Collapsed into one variant, the two are indistinguishable, so anything
/// downstream that wants to think harder about the hard cases has to think
/// about every "hi" as well. Separating them is what lets a model be asked
/// about *only* the residue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageConfidence {
    /// A rule matched: this classification is the classifier's answer.
    Matched,
    /// No rule matched. The triage is [`MessageTriage::Chatter`] by
    /// construction — the safe middle — and carries no positive claim about
    /// what the message was.
    Abstained,
}

/// A triage plus whether the classifier actually decided it (issue #678).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageOutcome {
    pub triage: MessageTriage,
    pub confidence: TriageConfidence,
}

impl TriageOutcome {
    /// Whether the classifier ran out of rules on this message.
    ///
    /// The escalation trigger: an abstention is the only class where a second,
    /// costlier opinion can add anything, because every other outcome is a rule
    /// firing.
    pub fn abstained(&self) -> bool {
        matches!(self.confidence, TriageConfidence::Abstained)
    }

    /// Whether a rule *recognised* this message as chatter — a bare greeting
    /// or acknowledgement (rule 2) or an empty message (rule 1) — as opposed to
    /// `Chatter` being chosen because no rule matched at all (rule 6, the
    /// abstention default; see [`abstained`](Self::abstained)).
    ///
    /// The distinction matters to callers that want to act on a *positive*
    /// chatter classification (issue #1725's greeting fast path): an abstained
    /// `Chatter` carries no claim about the message and is the escalation
    /// trigger instead, so treating the two alike would fire the fast path on
    /// every unclassifiable message, not just the ones the lexical layer
    /// actually recognised as conversation.
    pub fn is_matched_chatter(&self) -> bool {
        self.confidence == TriageConfidence::Matched && self.triage == MessageTriage::Chatter
    }
}

/// [`triage_message`], plus whether the answer was decided or fallen back to.
///
/// The classification is byte-for-byte what `triage_message` returns; this only
/// reports which arm produced it. See [`TriageConfidence`].
pub fn triage_message_detailed(text: &str) -> TriageOutcome {
    let matched = |triage| TriageOutcome {
        triage,
        confidence: TriageConfidence::Matched,
    };

    let trimmed = text.trim();
    if trimmed.is_empty() {
        // A decision, not a fallback: nothing was said, and no model can find
        // an ask in an empty string.
        return matched(MessageTriage::Chatter);
    }

    let lower = trimmed.to_lowercase();
    // Whole-message greeting/ack check (strip trailing punctuation first).
    let bare = bare_message(&lower);
    if GREETINGS.contains(&bare) {
        return matched(MessageTriage::Chatter);
    }

    // Strip leading filler ("ok now …", "and also …") so the ask underneath is
    // judged on its own.
    let core = strip_lead_ins(&lower);

    // Frame beats interrogative: a polite instruction stays work.
    if REQUEST_FRAMES.iter().any(|f| core.starts_with(f)) && contains_action(core) {
        if refers_to_board_entity(core) {
            return matched(MessageTriage::Chatter);
        }
        return matched(MessageTriage::Track(to_title(trimmed)));
    }
    if is_question(core) {
        return matched(MessageTriage::Answer);
    }
    if starts_with_action(core) {
        if refers_to_board_entity(core) {
            return matched(MessageTriage::Chatter);
        }
        return matched(MessageTriage::Track(to_title(trimmed)));
    }
    // The residue. Every rule above declined, so this says only "no rule
    // recognised it" — which is exactly the set worth a costlier opinion.
    TriageOutcome {
        triage: MessageTriage::Chatter,
        confidence: TriageConfidence::Abstained,
    }
}

/// A lowercased message reduced to what the whole-message lists are matched
/// against: trailing punctuation and spaces removed.
///
/// One implementation, because [`triage_message_detailed`] and [`small_talk`]
/// must agree on what "the whole message" is — a fast path that normalised
/// differently could answer a message the triage had not called
/// [`Chatter`](MessageTriage::Chatter).
fn bare_message(lower: &str) -> &str {
    lower.trim_end_matches(['.', '!', '?', ' ']).trim()
}

/// A pleasantry that a turn adds nothing to (issue #1725).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmallTalk {
    /// "hi", "hello", "good morning" — an opening, with no ask under it.
    Hello,
    /// "thanks", "cheers" — a closing, with no ask under it.
    Thanks,
}

impl SmallTalk {
    /// The one-line answer this pleasantry gets, in place of a turn.
    ///
    /// Deliberately generic and deliberately short. It carries no claim about
    /// the company's state, so it cannot be wrong, and it names no capability,
    /// so it cannot promise one — the two ways a canned reply goes bad.
    pub fn reply(self) -> &'static str {
        match self {
            Self::Hello => "Hey! What can I help you with?",
            Self::Thanks => "Anytime — just say the word if you need anything else.",
        }
    }
}

/// Whether `text` is a bare pleasantry that deserves an answer but no turn
/// (issue #1725).
///
/// # Why this is narrower than [`GREETINGS`]
///
/// [`GREETINGS`] also holds acknowledgements — "yes", "no", "sure", "done",
/// "ok". Those look like small talk in isolation and are nothing of the kind in
/// a conversation: "yes" answering a teammate's *"shall I ship it?"* is an
/// instruction, and short-circuiting it would drop a decision on the floor. The
/// two lists this matches — [`HELLOS`] and [`THANKS`] — are the ones that mean
/// the same thing whatever was said before them.
///
/// Both are subsets of [`GREETINGS`], so anything this answers is already
/// [`MessageTriage::Chatter`]: the fast path can never take a card away from a
/// message that was getting one.
pub fn small_talk(text: &str) -> Option<SmallTalk> {
    let lower = text.trim().to_lowercase();
    let bare = bare_message(&lower);
    if bare.is_empty() {
        return None;
    }
    if HELLOS.contains(&bare) {
        return Some(SmallTalk::Hello);
    }
    if THANKS.contains(&bare) {
        return Some(SmallTalk::Thanks);
    }
    None
}

/// Returns a cleaned task title when `text` is an actionable request, else
/// `None` — the [`MessageTriage::Track`]-only view of [`triage_message`].
///
/// The title it returns is a *fallback* name, used when no titling pass is
/// wired or the model could not answer. Nothing re-derives it to find a card
/// again — adoption is keyed on
/// [`TaskRecord::origin_message_seq`](crate::ports::tasks::TaskRecord::origin_message_seq) —
/// so this no longer has to agree with itself byte-for-byte across two call
/// sites.
pub fn detect_task_intent(text: &str) -> Option<String> {
    match triage_message(text) {
        MessageTriage::Track(title) => Some(title),
        MessageTriage::Answer | MessageTriage::Chatter => None,
    }
}

/// Remove stacked leading filler words from a lowercased message.
fn strip_lead_ins(lower: &str) -> &str {
    let mut s = lower.trim_start();
    loop {
        match LEAD_INS
            .iter()
            .find(|f| s.starts_with(**f))
            .map(|f| f.len())
        {
            Some(len) => s = s[len..].trim_start(),
            None => return s,
        }
    }
}

/// Whether the lowercased message asks for an answer rather than for work
/// (issue #267). Only reached once a request-framed instruction has been ruled
/// out, so a `?` here is a real question mark and not politeness.
///
/// # The asymmetry that sets how tight this is
///
/// [`MessageTriage::Answer`] is the only class with teeth: it suppresses the
/// direct-work card and narrows the model's board tools, so a false `Answer`
/// costs the operator the work itself, while a false [`MessageTriage::Chatter`]
/// costs nothing at all. The trailing-`?` branch has always carried that
/// asymmetry — its `contains_action` guard is there because "fix the login
/// bug?" is an operator second-guessing their own instruction, and an
/// instruction is still an instruction. The lead-word branches did not, and
/// entered `Answer` on a single word with no look at the rest of the sentence
/// (issue #267 review). [`later_clause_is_imperative`] is that same principle
/// applied to them, scoped to conjoined clauses so a single-clause question is
/// untouched.
fn is_question(lower: &str) -> bool {
    // A lead word speaks for its own clause, not for the whole message
    // (issue #267 review). Checked before either lead-word branch because both
    // of them read exactly one word and would otherwise answer for a conjoined
    // imperative they never looked at.
    let conjoined_imperative = later_clause_is_imperative(lower);
    if !conjoined_imperative && READ_PHRASES.iter().any(|p| starts_with_word(lower, p)) {
        return true;
    }
    let first = lower
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches([',', ':', ';', '.', '!', '?']);
    if !conjoined_imperative
        && (INTERROGATIVE_LEADS.contains(&first) || READ_VERBS.contains(&first))
    {
        return true;
    }
    // A trailing `?` with no action verb anywhere. The action-verb guard is
    // what keeps "fix the login bug?" out of here — an operator second-guessing
    // their own instruction is still an instruction.
    lower.trim_end().ends_with('?') && !contains_action(lower)
}

/// The clauses of a lowercased message, split on the joins an operator actually
/// stacks two asks with: ` and `, a semicolon, and a sentence boundary.
///
/// Empty clauses are dropped so a trailing `?` or a doubled separator does not
/// manufacture one — "what's our revenue?" is a single clause, not two.
fn clauses(lower: &str) -> impl Iterator<Item = &str> {
    lower
        .split([';', '.', '!', '?'])
        .flat_map(|sentence| sentence.split(" and "))
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
}

/// Whether a clause *after the first* is an imperative — the veto that stops a
/// leading question word speaking for a conjoined instruction (issue #267
/// review).
///
/// "Explain the auth flow and then fix the login bug" is a read AND work, and
/// [`MessageTriage::Answer`] is the expensive way to be wrong about it: it
/// withdraws the board tools from the half of the sentence that needs them.
/// Each later clause is passed through [`strip_lead_ins`] first, so the
/// connective an operator writes the second ask with ("and **then** fix …")
/// does not hide the verb underneath it.
///
/// Deliberately only the LATER clauses. Re-judging the first would collapse
/// into a blanket `!contains_action(...)` veto over the whole message, and
/// [`ACTION_VERBS`] is full of words that are commonly nouns — `review`,
/// `design`, `audit`, `plan`, `update` — so that would degrade genuine
/// questions like "what's the status of the design review?" to
/// [`MessageTriage::Chatter`] and gut the layer.
///
/// The residual is the mirror of that: a later clause whose first word is one
/// of those noun-ish verbs ("show me the design and review queue") vetoes when
/// it should not, and the message falls to `Chatter` rather than `Answer`.
/// `Chatter` cards nothing and gates nothing, so the cost is a lost gate rather
/// than a spurious card — the cheap direction. A keyword classifier cannot do
/// better than trade here; issue #678 is where it stops having to.
fn later_clause_is_imperative(lower: &str) -> bool {
    clauses(lower)
        .skip(1)
        .any(|clause| starts_with_action(strip_lead_ins(clause)))
}

/// The message's first word (or a leading multi-word phrase) is an action verb.
fn starts_with_action(lower: &str) -> bool {
    if ACTION_PHRASES.iter().any(|p| starts_with_word(lower, p)) {
        return true;
    }
    let first = lower.split_whitespace().next().unwrap_or("");
    // Trim trailing punctuation on the first word ("build," / "fix:").
    let first = first.trim_end_matches([',', ':', ';', '.', '!']);
    ACTION_VERBS.contains(&first)
}

/// Whether the object of the request is the board itself or a card on it — a
/// message *about* the kanban rather than a new deliverable.
fn refers_to_board_entity(lower: &str) -> bool {
    if BOARD_DEIXIS
        .iter()
        .any(|p| board_deixis_is_object(lower, p))
    {
        return true;
    }
    field_noun_in_board_context(lower)
}

/// True when `phrase` occurs in `lower`, word-bounded, in the request's actual
/// object position: not the head of a longer noun ("the board
/// **presentation**" — see [`followed_by_board_context`]) and not the topic of
/// a different object ("a report **about** the board" — see
/// [`preceded_by_topic_marker`]).
fn board_deixis_is_object(lower: &str, phrase: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(phrase) {
        let start = from + rel;
        let end = start + phrase.len();
        from = start + 1;
        let before = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after = end == lower.len() || !bytes[end].is_ascii_alphanumeric();
        if before
            && after
            && followed_by_board_context(&lower[end..])
            && !preceded_by_topic_marker(&lower[..start])
        {
            return true;
        }
    }
    false
}

/// Whether a topic-introducing word immediately precedes a deixis phrase,
/// making the phrase the topic of a different, earlier object ("a memo
/// **about** the board") rather than the object of the request itself. The
/// verb's own complement prepositions ("look **at** the board", "move it
/// **to** the board") are not topic markers and are left alone.
fn preceded_by_topic_marker(lead: &str) -> bool {
    const TOPIC_MARKERS: &[&str] = &["about", "regarding", "concerning"];
    let last_word = lead
        .trim_end()
        .rsplit(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or("");
    TOPIC_MARKERS.contains(&last_word)
}

/// A [`BOARD_FIELD_NOUNS`] phrase used as a board operation's object: clause-final,
/// or immediately followed by a connective/preposition/punctuation rather than a
/// continuing noun.
fn field_noun_in_board_context(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    for phrase in BOARD_FIELD_NOUNS {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(phrase) {
            let start = from + rel;
            let end = start + phrase.len();
            from = start + 1;
            let before = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
            let after = end == lower.len() || !bytes[end].is_ascii_alphanumeric();
            if before && after && followed_by_board_context(&lower[end..]) {
                return true;
            }
        }
    }
    false
}

/// Whether what trails a field noun marks it as a board operation's object: the
/// clause ends, or the next token is punctuation or a [`BOARD_CONNECTIVES`] word
/// — but not a continuing noun ("the status **page**").
fn followed_by_board_context(rest: &str) -> bool {
    let rest = rest.trim_start();
    match rest.chars().next() {
        None => true,
        Some(c) if !c.is_alphanumeric() => true,
        Some(_) => {
            let next_word = rest
                .split(|c: char| !c.is_alphanumeric())
                .next()
                .unwrap_or("");
            BOARD_CONNECTIVES.contains(&next_word)
        }
    }
}

/// An action verb/phrase appears anywhere (used behind a request frame).
fn contains_action(lower: &str) -> bool {
    if ACTION_PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| ACTION_VERBS.contains(&word))
}

/// True when `lower` begins with `phrase` followed by a word boundary.
fn starts_with_word(lower: &str, phrase: &str) -> bool {
    lower == phrase
        || lower
            .strip_prefix(phrase)
            .is_some_and(|rest| rest.starts_with(' '))
}

/// Build a task title: strip a leading request frame, drop trailing
/// punctuation, upper-case the first letter, and bound the length.
///
/// `pub(crate)` since issue #845: the chat route opens a card for an explicit
/// `workflow` deliverable even when the triage declined to name one, and it
/// titles that card through *this* function rather than a second derivation, so
/// a bypassed card is titled byte-for-byte as a `Track` card would have been.
pub(crate) fn to_title(original: &str) -> String {
    let mut s = original.trim();
    // Strip stacked leading filler ("ok now …") and request-framing prefixes
    // ("please can you …") for a clean imperative title.
    loop {
        let lower = s.to_lowercase();
        let hit = LEAD_INS
            .iter()
            .chain(REQUEST_FRAMES.iter())
            .find(|f| lower.starts_with(**f))
            .map(|f| f.len());
        match hit {
            Some(len) => s = s[len..].trim_start(),
            None => break,
        }
    }
    let s = s.trim_end_matches(['?', '.', '!', ' ']).trim();
    if s.is_empty() {
        // A frame with no residual body ("please.") — fall back to the original.
        return truncate(original.trim(), TITLE_MAX);
    }
    let capped = truncate(s, TITLE_MAX);
    capitalize_first(&capped)
}

/// Upper-case the first character, leaving the rest untouched.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// UTF-8-safe truncation to at most `max` chars, appending `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// What an operator's reply to a parked blocker asks the company to do (issue
/// #1862).
///
/// The four verdicts lower onto the existing [`Approve`/`Deny`] resolve surface
/// — no new gate arm — so answering a blocker is the same durable decision as
/// answering any approval. [`Unrelated`](Self::Unrelated) is the escape: a
/// greeting or a question back is not a verdict, and the reply runs as an
/// ordinary chat turn instead of settling anything.
///
/// Deliberately says nothing about resumption. Resolving a blocker is inert
/// until #1863; this only records which verdict the operator gave.
///
/// [`Approve`/`Deny`]: crate::ports::types::Verdict
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockerReplyIntent {
    /// Run the stopped step again as it was.
    Retry,
    /// Answer or correct it — the reply text carries what changed.
    Amend,
    /// Drop this blocker and let the work go on without it.
    Skip,
    /// Abandon the stopped work.
    Cancel,
    /// Not a verdict — ordinary conversation that must run as a normal turn.
    Unrelated,
}

/// Words that plainly abandon the work — checked first, because "stop" and
/// "drop" outrank every other reading.
const CANCEL_WORDS: &[&str] = &[
    "cancel", "abort", "abandon", "drop", "forget", "scrap", "kill", "discard",
];

/// Words that waive the blocker but keep the work going.
const SKIP_WORDS: &[&str] = &["skip", "waive", "ignore", "bypass", "omit"];

/// Words that ask for the same step again, unchanged. Strong signals only — a
/// bare "yes"/"ok" is a [`GREETINGS`] entry and stays [`Unrelated`], so a
/// passing affirmation in an ordinary sentence never reads as a verdict.
///
/// [`Unrelated`]: BlockerReplyIntent::Unrelated
const RETRY_WORDS: &[&str] = &[
    "retry", "again", "proceed", "continue", "approve", "approved", "rerun", "redo",
];

/// Classifies an operator's reply to a parked blocker (issue #1862),
/// lexical-first and conservative.
///
/// The order is the priority: an abandon word wins over a waive word wins over
/// a retry word, because "cancel it, but retry the other one" must read as a
/// cancel of the thing in hand. A reply that is empty, a greeting, or a
/// question back is [`Unrelated`](BlockerReplyIntent::Unrelated) — the operator
/// is talking, not deciding. Everything else is
/// [`Amend`](BlockerReplyIntent::Amend): a substantive line in a blocked
/// teammate's DM is taken as answering the question, and the text becomes the
/// correction.
///
/// Model-assisted disambiguation is #678; this is the lexical tier that abstains
/// to [`Unrelated`] rather than guessing.
///
/// # The one verdict that has to be the whole sentence (defect B-115)
///
/// The priority above is a claim about *ranking* two verdicts, and it was read
/// as a claim about ranking a verdict against everything else. A founder wrote,
/// in one message: "ok scrap the reminder card. 500 candles worth — jar, lid
/// and label for 500 units total across the four scents, cold source it, under
/// 4k. that's the answer". `scrap` appears, so the whole message read as a
/// cancel; the work was abandoned, and the answer it carried — the only thing
/// that could have unblocked the card — was discarded with it. The card was
/// left paused with nothing behind it to restart it.
///
/// So a cancel now has to be the whole of the ask: every clause that is not the
/// cancel clause must be filler. Where it is not, the cancel reading is dropped
/// and the message is judged on its remaining merits — which for that sentence
/// is [`Amend`](BlockerReplyIntent::Amend), i.e. the answer it was written to
/// give.
///
/// **Only cancel**, and the asymmetry is the reason.
/// [`Cancel`](crate::ports::blockers::BlockerVerdict::Cancel) is "the one
/// verdict that does not re-enter" — being wrong about it destroys the work,
/// while being wrong about retry, skip or amend costs a re-run that the next
/// message can correct. It is the same trade [`triage_message`] already makes
/// between [`Answer`](MessageTriage::Answer) and
/// [`Chatter`](MessageTriage::Chatter), for the same reason: only the expensive
/// direction is worth a guard.
pub fn classify_blocker_reply(text: &str) -> BlockerReplyIntent {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return BlockerReplyIntent::Unrelated;
    }
    let lower = trimmed.to_lowercase();
    let bare = bare_message(&lower);
    if GREETINGS.contains(&bare) {
        return BlockerReplyIntent::Unrelated;
    }
    let core = strip_lead_ins(&lower);
    // A question back is the operator asking, not answering — it must run as a
    // normal turn so the teammate can respond, not settle the blocker.
    if is_question(core) {
        return BlockerReplyIntent::Unrelated;
    }
    if mentions_any(&lower, CANCEL_WORDS) && cancel_is_the_whole_ask(&lower) {
        return BlockerReplyIntent::Cancel;
    }
    if mentions_any(&lower, SKIP_WORDS) {
        return BlockerReplyIntent::Skip;
    }
    if mentions_any(&lower, RETRY_WORDS) || lower.contains("go ahead") {
        return BlockerReplyIntent::Retry;
    }
    // A purely social line — "hello there", "thanks so much" — carries no
    // answer, so it runs as a normal turn rather than being taken as a
    // correction. A single non-social word tips it to a substantive answer.
    if is_pure_social(&lower) {
        return BlockerReplyIntent::Unrelated;
    }
    BlockerReplyIntent::Amend
}

/// Whether abandoning the work is the **whole** of what `lower` asks for
/// (defect B-115) — every clause that does not itself carry a
/// [`CANCEL_WORDS`] entry is filler.
///
/// Clause-wise rather than message-wise, over the same [`clauses`] split
/// [`later_clause_is_imperative`] uses, so the two places that ask "does a
/// later part of this sentence say something the first part does not" answer it
/// the same way. A comma is deliberately *not* a clause boundary there, which
/// keeps "cancel this one, retry the other" a cancel — that reading is the
/// priority rule this function is scoped not to disturb.
///
/// An `all` over an empty iterator is `true`, which is the right answer and not
/// an accident: a message whose every clause names the cancel is a cancel and
/// nothing else.
///
/// # A cancel clause is exempt, not invisible (Codex + CodeRabbit review, PR #2054)
///
/// A clause naming the cancel used to be dropped from consideration
/// unconditionally — the whole point above, for "cancel this one, retry the
/// other" and "please drop this one". But a comma does not introduce a clause
/// boundary here, so "scrap the reminder card, budget is $500" is ALSO one
/// clause, and the same blanket exemption threw the budget away with it: the
/// message read as a pure cancel and the amendment never reached anywhere.
///
/// A clause is exempt only when it carries no digit. A cancel word beside
/// ordinary filler — "it", "this one", "that", pronouns `is_pure_social`
/// does not even recognise — stays exempt exactly as before, which is what
/// keeps every existing case above unchanged: none of them names a number.
/// A digit is the cheap, reliable tell that a clause carries a genuine
/// second fact rather than filler around the cancel word — an amount, a
/// count, an id — and once kept in, `is_pure_social` correctly fails it
/// (digits are not [`SOCIAL_WORDS`]), so the whole message falls through to
/// [`BlockerReplyIntent::Amend`] instead of silently discarding it.
fn cancel_is_the_whole_ask(lower: &str) -> bool {
    let carries_a_fact = |clause: &&str| clause.chars().any(|c| c.is_ascii_digit());
    clauses(lower)
        .filter(|clause| !mentions_any(clause, CANCEL_WORDS) || carries_a_fact(clause))
        .all(is_pure_social)
}

/// Whether any whole word of `lower` is in `words` and is not negated by a
/// preceding "not"/"don't"/… within two tokens — so `okay` matches `ok`-the-word
/// but `okra` never matches `ok`, and `don't retry` no longer reads as a retry.
fn mentions_any(lower: &str, words: &[&str]) -> bool {
    let flattened = lower.replace(['\'', '\u{2019}'], "");
    let tokens: Vec<&str> = flattened
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    tokens
        .iter()
        .enumerate()
        .any(|(i, word)| words.contains(word) && !negated_before(&tokens, i))
}

/// Negations that flip a following verdict word to a non-verdict, apostrophes
/// already stripped so `don't` reads as `dont`.
const NEGATIONS: &[&str] = &[
    "not", "no", "never", "cannot", "dont", "doesnt", "wont", "cant", "isnt", "arent",
];

/// Whether a negation sits within the two tokens before `i`.
fn negated_before(tokens: &[&str], i: usize) -> bool {
    tokens[i.saturating_sub(2)..i]
        .iter()
        .any(|token| NEGATIONS.contains(token))
}

/// Words that carry no instruction — greetings, thanks, fillers. A message made
/// only of these is social, not an answer.
const SOCIAL_WORDS: &[&str] = &[
    "hi",
    "hii",
    "hiya",
    "hey",
    "hello",
    "howdy",
    "yo",
    "sup",
    "gm",
    "good",
    "morning",
    "evening",
    "afternoon",
    "there",
    "thanks",
    "thank",
    "you",
    "ty",
    "thx",
    "cheers",
    "so",
    "much",
    "ok",
    "okay",
    "k",
    "kk",
    "cool",
    "nice",
    "great",
    "awesome",
    "perfect",
    "sure",
    "np",
    "sg",
    "lol",
    "haha",
    "please",
];

/// Whether every word of `lower` is a [`SOCIAL_WORDS`] filler — a non-empty
/// message with nothing to act on.
fn is_pure_social(lower: &str) -> bool {
    let mut seen = false;
    for word in lower.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        seen = true;
        if !SOCIAL_WORDS.contains(&word) {
            return false;
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_imperative_is_actionable() {
        for msg in [
            "Build the landing page",
            "create a new email campaign",
            "fix the checkout bug",
            "find three suppliers for widgets",
            "set up the weekly newsletter",
            "look into why signups dropped",
            "research competitors in the EU",
        ] {
            assert!(detect_task_intent(msg).is_some(), "should fire: {msg}");
        }
    }

    #[test]
    fn framed_request_with_action_verb_is_actionable() {
        assert_eq!(
            detect_task_intent("Can you build the landing page?").as_deref(),
            Some("Build the landing page")
        );
        assert!(detect_task_intent("please set up the newsletter").is_some());
        assert!(detect_task_intent("I need you to draft the pitch").is_some());
        assert!(detect_task_intent("let's launch the beta").is_some());
    }

    #[test]
    fn pure_questions_do_not_fire() {
        for msg in [
            "what's our revenue this month?",
            "how many users signed up?",
            "who is on the growth desk?",
            "can you tell me the latest numbers?", // framed, but no action verb
            "is the campaign live?",
            "why did signups drop?",
        ] {
            assert!(detect_task_intent(msg).is_none(), "should not fire: {msg}");
            assert_eq!(
                triage_message(msg),
                MessageTriage::Answer,
                "should be an answer: {msg}"
            );
        }
    }

    /// Issue #267: the six cards found sitting unworked in `backlog` on a live
    /// company, each pinned to the class it now triages as.
    ///
    /// Five of the six lead with an action verb, so they came through the
    /// deterministic handler path and stay `Track` — they *are* instructions,
    /// and the fix for the four workflow asks is that the orchestrator now
    /// authors the graph in-turn rather than parking the card. The sixth has no
    /// action verb and no request frame, so this module never saw it: it was
    /// the model's own `spawn_task`, and `Answer` is what takes that tool away.
    #[test]
    fn the_six_observed_dead_cards_triage_as_recorded() {
        let cases: &[(&str, MessageTriage)] = &[
            (
                "Tell what is there in the tasks list",
                MessageTriage::Answer,
            ),
            (
                "Call the composio_authorize tool with toolkit = gmail and reply with the result",
                MessageTriage::Track(
                    "Call the composio_authorize tool with toolkit = gmail and reply with the \
                     result"
                        .to_string(),
                ),
            ),
            (
                "Create a workflow to, write a 2-sentence status update",
                MessageTriage::Track(
                    "Create a workflow to, write a 2-sentence status update".to_string(),
                ),
            ),
            (
                "Create a simple workflow, Write a 2-sentence status update",
                MessageTriage::Track(
                    "Create a simple workflow, Write a 2-sentence status update".to_string(),
                ),
            ),
            (
                "Create a workflow named topic-to-visual",
                MessageTriage::Track("Create a workflow named topic-to-visual".to_string()),
            ),
            (
                "Create a workflow called Daily Standup",
                MessageTriage::Track("Create a workflow called Daily Standup".to_string()),
            ),
        ];
        for (msg, expected) in cases {
            assert_eq!(&triage_message(msg), expected, "triage of: {msg}");
        }
    }

    /// The class the issue is really about: reads that were becoming cards.
    #[test]
    fn read_requests_are_answers() {
        for msg in [
            "list the tasks",
            "show me the board",
            "what's our revenue?",
            "Tell what is there in the tasks list",
            "explain how the newsletter works",
            "describe the current backlog",
            "walk me through the funnel",
            "can you list the tasks?",
            "where do we stand on the launch?",
        ] {
            assert_eq!(
                triage_message(msg),
                MessageTriage::Answer,
                "should be an answer: {msg}"
            );
            assert!(detect_task_intent(msg).is_none(), "no card for: {msg}");
        }
    }

    /// The class the original suite never probed: for every lead-word pattern
    /// it pinned, it picked the *read*-flavoured instance ("give me the
    /// headcount", "walk me through the funnel", "describe the current
    /// backlog") — so the work-flavoured instance of the same pattern went
    /// unmeasured, and every one of these was [`MessageTriage::Answer`], which
    /// withdraws the board tools from a request to build something (issue #267
    /// review, finding 1).
    ///
    /// The assertion is `!= Answer` rather than `== Track` on purpose. What
    /// costs the operator is the gate; whether one of these lands in `Track` or
    /// in the `Chatter` middle is the classifier's own tie-break and not what
    /// this pins.
    #[test]
    fn a_work_ask_behind_a_read_lead_is_not_gated() {
        for msg in [
            // `give me` no longer leads READ_PHRASES.
            "give me a landing page",
            // …and a lead word no longer speaks for a conjoined imperative.
            "explain the auth flow and then fix the login bug",
            "compare our pricing to competitors and write up a doc",
            "walk me through the funnel and build a dashboard for it",
            "tell me the headcount; then draft the hiring plan",
            "show me the board. create a card for the launch",
        ] {
            assert_ne!(
                triage_message(msg),
                MessageTriage::Answer,
                "a request to produce something must not be gated: {msg}"
            );
        }
    }

    /// The other side of the same trade, and the reason the veto is scoped to
    /// *later* clauses rather than applied as a blanket `!contains_action`:
    /// [`ACTION_VERBS`] is full of words that are commonly nouns, so a blanket
    /// veto would degrade these to `Chatter` and gut the layer.
    #[test]
    fn a_noun_that_doubles_as_an_action_verb_stays_a_question() {
        for msg in [
            "what's the status of the design review?",
            "who is running the security audit?",
            "when is the next product review and the board update?",
            "what's our revenue and how did it change?",
            "is the campaign live and is the newsletter out?",
        ] {
            assert_eq!(
                triage_message(msg),
                MessageTriage::Answer,
                "should stay an answer: {msg}"
            );
        }
    }

    /// Dropping `give me` from [`READ_PHRASES`] moves the read-flavoured
    /// instance to `Chatter`, which is the whole cost of the change: no card,
    /// no gate, so the operator loses nothing.
    #[test]
    fn a_read_flavoured_give_me_falls_to_the_safe_middle() {
        assert_eq!(
            triage_message("give me the headcount"),
            MessageTriage::Chatter
        );
        assert!(detect_task_intent("give me the headcount").is_none());
    }

    /// A request frame is read before any question test, so a politely phrased
    /// instruction stays work even when it ends in `?`.
    #[test]
    fn a_request_frame_beats_an_interrogative() {
        assert_eq!(
            triage_message("can you build the landing page?"),
            MessageTriage::Track("Build the landing page".to_string())
        );
        assert_eq!(
            triage_message("could you please fix the checkout bug?"),
            MessageTriage::Track("Fix the checkout bug".to_string())
        );
    }

    /// The predicate the board guard turns on: deixis fires only when it is
    /// the object of the request (see
    /// [`board_deixis_must_be_the_objects_head_not_a_modifier_or_topic`] for
    /// the cases that must NOT fire), field nouns demote only in board
    /// context, and everything else is real work.
    #[test]
    fn board_entity_predicate_reads_the_object_of_the_request() {
        // Tier 1 — deixis, matched wherever it is the object of the request.
        for msg in [
            "update the status on the task card",
            "move this card to done",
            "close the ticket",
            "reprioritise the backlog",
            "look at the board",
        ] {
            assert!(refers_to_board_entity(msg), "should be board: {msg}");
        }
        // Tier 2 — a field noun that is the object of a board operation.
        assert!(refers_to_board_entity("update the status on the board"));
        assert!(refers_to_board_entity("bump the priority")); // clause-final
        assert!(refers_to_board_entity("change the assignee to nova")); // connective
        // Tier 2 — a field noun that heads a longer noun is real work.
        assert!(!refers_to_board_entity("update the status page"));
        assert!(!refers_to_board_entity("draft the priority list"));
        // No board vocabulary at all.
        for msg in [
            "update the landing page",
            "move the deploy to staging",
            "create a task tracker",
        ] {
            assert!(!refers_to_board_entity(msg), "should not be board: {msg}");
        }
        // A boundary check: deixis must not fire inside a larger word.
        assert!(!refers_to_board_entity("restock the cardstock"));
    }

    /// PR #1949 review (Codex thread 3895066476, CodeRabbit thread
    /// 3895107555): `BOARD_DEIXIS` used to match anywhere in the message, so
    /// a deliverable whose title merely *contains* board vocabulary — as a
    /// compound noun, or as the topic of a different object — got misread as
    /// the object of a board operation and demoted to `Chatter`, opening no
    /// card. The predicate must require the deixis phrase to actually be the
    /// object of the request, the same way [`field_noun_in_board_context`]
    /// already requires for field nouns.
    #[test]
    fn board_deixis_must_be_the_objects_head_not_a_modifier_or_topic() {
        // "the board"/"the ticket" heads a longer noun ("board presentation",
        // "ticket booking flow") — real work, not a board operation.
        assert!(!refers_to_board_entity("build the board presentation"));
        assert!(!refers_to_board_entity("update the ticket booking flow"));
        // "about"/"regarding" make the deixis phrase the *topic* of a
        // different object ("a report"), not the object itself.
        assert!(!refers_to_board_entity("create a report about the board"));
        assert!(!refers_to_board_entity("write a memo regarding the board"));
        // Genuine deixis-as-object still fires — the verb's own complement
        // preposition ("at", "to") is not a topic marker.
        assert!(refers_to_board_entity("look at the board"));
        assert!(refers_to_board_entity("move this card to done"));
    }

    /// PR #1949 review (CodeRabbit thread 3895107555): `field_noun_in_board_
    /// context` treated a trailing `of` exactly like `on`/`to`/`for`/`and`,
    /// but `of` introduces the noun a field *belongs to* ("the status **of**
    /// the landing page" = the landing page's status), not a board
    /// operation's target value the way "change the assignee **to** nova"
    /// does. Demoting real deliverable work phrased with `of` closed no card.
    #[test]
    fn field_noun_followed_by_of_is_not_board_context() {
        assert!(!refers_to_board_entity(
            "update the status of the landing page"
        ));
        // The other connectives are unaffected.
        assert!(refers_to_board_entity("update the status on the board"));
        assert!(refers_to_board_entity("change the assignee to nova"));
    }

    /// A board operation phrased as an instruction is a *decision* to touch the
    /// existing card, not a new deliverable — so it is `Chatter` (Matched), not
    /// a second `Track` card. The incident that opened the issue leads the list.
    #[test]
    fn a_board_operation_does_not_mint_a_second_card() {
        for msg in [
            "can you also update the status on the task card?",
            "update the status on the task card",
            "please move the card to done",
            "can you update the priority on this task?",
            "close the ticket",
            "update the status on the board",
        ] {
            let out = triage_message_detailed(msg);
            assert_eq!(out.triage, MessageTriage::Chatter, "should not card: {msg}");
            assert_eq!(
                out.confidence,
                TriageConfidence::Matched,
                "a board op is a decision, not an abstention: {msg}"
            );
            assert!(detect_task_intent(msg).is_none(), "no card for: {msg}");
        }
    }

    /// The other side of the trade: real work that merely mentions a board word
    /// (or a field noun heading a longer noun) still cards.
    #[test]
    fn real_work_that_mentions_a_field_still_cards() {
        for (msg, title) in [
            ("update the landing page", "Update the landing page"),
            ("move the deploy to staging", "Move the deploy to staging"),
            ("update the status page", "Update the status page"),
            ("create a task tracker", "Create a task tracker"),
        ] {
            assert_eq!(
                triage_message(msg),
                MessageTriage::Track(title.to_string()),
                "should stay work: {msg}"
            );
        }
        assert_eq!(
            triage_message("can you review the design"),
            MessageTriage::Track("Review the design".to_string())
        );
    }

    /// Neither work nor a question: the safe middle that cards nothing and
    /// gates nothing.
    #[test]
    fn neutral_chatter_is_chatter() {
        for msg in [
            "hi",
            "thanks",
            "the deck looks good to me",
            "i'll be offline tomorrow",
            "nice work on the launch",
            "…",
        ] {
            assert_eq!(
                triage_message(msg),
                MessageTriage::Chatter,
                "should be chatter: {msg}"
            );
        }
    }

    // ── Issue #678: which Chatter is a decision, and which is a shrug ───────

    /// The whole point of the seam. `Chatter` covers two unlike things, and only
    /// one of them is worth a second opinion.
    #[test]
    fn a_recognised_chatter_is_a_decision_and_the_residue_is_an_abstention() {
        for decided in ["", "   ", "hi", "hello", "thanks"] {
            let out = triage_message_detailed(decided);
            assert_eq!(out.triage, MessageTriage::Chatter, "{decided:?}");
            assert!(
                !out.abstained(),
                "a greeting or an empty message is recognised, not fallen back to: {decided:?}"
            );
        }
        for residue in [
            "the deck looks good to me",
            "i'll be offline tomorrow",
            "nice work on the launch",
        ] {
            let out = triage_message_detailed(residue);
            assert_eq!(out.triage, MessageTriage::Chatter, "{residue:?}");
            assert!(
                out.abstained(),
                "no rule matched this, so the Chatter is a shrug: {residue:?}"
            );
        }
    }

    /// Every arm that fires a rule reports `Matched` — an abstention must never
    /// be reachable from a positive classification, or the escalation trigger
    /// would spend a model call on messages the cheap layer already named.
    #[test]
    fn every_positive_classification_reports_matched() {
        for msg in [
            "draft the launch plan for next quarter",
            "can you build the landing page?",
            "what is on the board?",
            "show me the headcount",
            "create a workflow named nightly digest",
        ] {
            let out = triage_message_detailed(msg);
            assert!(
                !out.abstained(),
                "a rule decided this, so it is not an abstention: {msg:?} -> {:?}",
                out.triage
            );
            assert_ne!(
                out.triage,
                MessageTriage::Chatter,
                "fixture must exercise a non-Chatter arm: {msg:?}"
            );
        }
    }

    /// The seam is observational. `triage_message` is the byte-for-byte answer
    /// it always was — #463 pins two card paths to the title it returns, so a
    /// classification drift here would desynchronise the REST handler from
    /// `chat_handler_card` and orphan the card.
    #[test]
    fn the_detailed_entry_point_changes_no_classification() {
        for msg in [
            "",
            "   ",
            "hi",
            "thanks",
            "…",
            "the deck looks good to me",
            "i'll be offline tomorrow",
            "draft the launch plan for next quarter",
            "can you build the landing page?",
            "what is on the board?",
            "show me the headcount",
            "ok now also draft the brief",
            "is the build ok?",
            "create a workflow named nightly digest",
        ] {
            assert_eq!(
                triage_message(msg),
                triage_message_detailed(msg).triage,
                "the detailed entry point must not reclassify: {msg:?}"
            );
        }
    }

    /// The `Track` arm still carries the exact string the REST handler writes,
    /// which `chat_handler_card` re-derives to find that card (issue #463).
    #[test]
    fn track_titles_stay_byte_identical_to_detect_task_intent() {
        for msg in [
            "Build the landing page",
            "please can you fix the login bug!",
            "go ahead and publish the blog post",
            "ok now build the dashboard",
            "Can you build the landing page?",
        ] {
            assert_eq!(
                triage_message(msg).title().map(str::to_string),
                detect_task_intent(msg),
                "title contract for: {msg}"
            );
        }
    }

    #[test]
    fn greetings_and_acks_do_not_fire() {
        for msg in [
            "hi",
            "Hello!",
            "hey",
            "thanks",
            "thank you",
            "ok",
            "okay",
            "cool",
            "got it",
            "sounds good",
            "perfect",
            "yes",
            "no",
            "sure",
            "done",
        ] {
            assert!(detect_task_intent(msg).is_none(), "should not fire: {msg}");
        }
    }

    #[test]
    fn title_strips_frame_caps_and_trims() {
        assert_eq!(
            detect_task_intent("please can you fix the login bug!").as_deref(),
            Some("Fix the login bug")
        );
        assert_eq!(
            detect_task_intent("go ahead and publish the blog post").as_deref(),
            Some("Publish the blog post")
        );
    }

    #[test]
    fn title_is_bounded() {
        let long = format!("build {}", "a very detailed feature ".repeat(20));
        let title = detect_task_intent(&long).expect("actionable");
        assert!(title.chars().count() <= TITLE_MAX + 1, "bounded: {title}");
    }

    #[test]
    fn empty_and_whitespace_do_not_fire() {
        assert!(detect_task_intent("").is_none());
        assert!(detect_task_intent("   ").is_none());
    }

    #[test]
    fn ack_prefix_then_request_still_fires() {
        // "ok" as a whole message is an ack, but not as a prefix of a real ask.
        assert!(detect_task_intent("ok now build the dashboard").is_some());
    }

    // ── Issue #1725: the small-talk fast path's own classifier ──

    /// The reported message. "hi" is a pleasantry, in every spelling and
    /// whatever punctuation and case it arrives in.
    #[test]
    fn a_bare_greeting_is_small_talk() {
        for text in ["hi", "Hi", "  hi  ", "hi!", "Hello.", "hey", "Good morning"] {
            assert_eq!(
                small_talk(text),
                Some(SmallTalk::Hello),
                "{text:?} should be a greeting"
            );
        }
        for text in ["thanks", "Thank you!", "cheers", "ty"] {
            assert_eq!(
                small_talk(text),
                Some(SmallTalk::Thanks),
                "{text:?} should be thanks"
            );
        }
    }

    /// The narrowing that keeps the fast path honest. An acknowledgement is
    /// small talk on its own and an *instruction* in a conversation — "yes"
    /// answering "shall I ship it?" must reach the turn that asked.
    #[test]
    fn an_acknowledgement_is_not_small_talk() {
        for text in [
            "yes", "no", "sure", "ok", "okay", "done", "lgtm", "got it", "nvm", "cool",
        ] {
            assert_eq!(small_talk(text), None, "{text:?} must still run a turn");
        }
    }

    /// A greeting with an ask under it is an ask. The fast path matches the
    /// whole message for exactly this reason.
    #[test]
    fn a_greeting_with_a_request_under_it_is_not_small_talk() {
        for text in [
            "hi, build the landing page",
            "hey can you check the numbers?",
            "thanks — now ship it",
            "hi there team",
        ] {
            assert_eq!(small_talk(text), None, "{text:?} must still run a turn");
        }
    }

    /// Nothing said is not a pleasantry: there is no one to greet back.
    #[test]
    fn an_empty_message_is_not_small_talk() {
        assert_eq!(small_talk(""), None);
        assert_eq!(small_talk("   "), None);
        assert_eq!(small_talk("..."), None);
    }

    /// The subset invariant the fast path rests on: everything it answers is
    /// already `Chatter`, so it can never take a card away from a message that
    /// was getting one.
    #[test]
    fn every_pleasantry_is_also_a_greeting() {
        for word in HELLOS.iter().chain(THANKS.iter()) {
            assert!(
                GREETINGS.contains(word),
                "{word:?} is answered by the fast path but is not in GREETINGS"
            );
            assert_eq!(
                triage_message(word),
                MessageTriage::Chatter,
                "{word:?} must triage as Chatter"
            );
            assert!(
                !triage_message_detailed(word).abstained(),
                "{word:?} must be a decision, not an abstention"
            );
        }
    }

    /// The canned replies say nothing that can go stale.
    #[test]
    fn the_canned_replies_are_short_and_claim_nothing() {
        for talk in [SmallTalk::Hello, SmallTalk::Thanks] {
            let reply = talk.reply();
            assert!(!reply.trim().is_empty());
            assert!(reply.chars().count() <= 80, "{reply:?} is too long");
        }
    }
}

#[cfg(test)]
mod blocker_reply_tests {
    use super::*;

    #[test]
    fn a_retry_word_asks_for_the_same_step_again() {
        for reply in [
            "retry",
            "try it again",
            "go ahead",
            "yes, proceed",
            "approved",
        ] {
            assert_eq!(
                classify_blocker_reply(reply),
                BlockerReplyIntent::Retry,
                "reply: {reply}"
            );
        }
    }

    #[test]
    fn a_skip_word_waives_the_blocker() {
        for reply in [
            "skip it",
            "waive this",
            "just ignore it",
            "bypass the check",
        ] {
            assert_eq!(
                classify_blocker_reply(reply),
                BlockerReplyIntent::Skip,
                "reply: {reply}"
            );
        }
    }

    #[test]
    fn a_cancel_word_abandons_the_work() {
        for reply in [
            "cancel",
            "abort this",
            "drop it",
            "forget about it",
            "scrap the task",
        ] {
            assert_eq!(
                classify_blocker_reply(reply),
                BlockerReplyIntent::Cancel,
                "reply: {reply}"
            );
        }
    }

    /// Defect B-115: a message whose main content is the answer is not a cancel
    /// because one clause of it mentions scrapping something else.
    ///
    /// This exact sentence cancelled a card and threw the answer away with it.
    /// The card was left paused with no blocker behind it, so nothing would
    /// ever have resumed it, and the reply named nothing.
    #[test]
    fn an_answer_that_mentions_scrapping_something_is_not_a_cancel() {
        let reply = "@tomas ok scrap the reminder card. 500 candles worth - jar, lid and \
                     label for 500 units total across the four scents, cold source it, \
                     under 4k. that's the answer";
        assert_eq!(
            classify_blocker_reply(reply),
            BlockerReplyIntent::Amend,
            "the message's own words say what it is: {reply:?}"
        );
    }

    /// The guard is a scope, not a removal: a cancel that really is the whole
    /// of the ask still cancels, filler and all.
    #[test]
    fn a_cancel_that_is_the_whole_message_still_cancels() {
        for reply in [
            "ok, scrap it",
            "please drop this one",
            "yeah, abandon it. thanks",
            "cancel that. cheers",
        ] {
            assert_eq!(
                classify_blocker_reply(reply),
                BlockerReplyIntent::Cancel,
                "reply: {reply}"
            );
        }
    }

    /// Abandon outranks retry: "cancel this and retry the other" is a cancel of
    /// the thing in hand.
    #[test]
    fn abandon_outranks_retry_when_both_appear() {
        assert_eq!(
            classify_blocker_reply("cancel this one, retry the other"),
            BlockerReplyIntent::Cancel
        );
    }

    /// Codex + CodeRabbit review, PR #2054: a cancel word beside a genuine
    /// second fact must not swallow it. A comma is not a clause boundary
    /// (`cancel_is_the_whole_ask`'s own doc), so "scrap the reminder card,
    /// budget is $500" is one clause naming both the cancel and the amount —
    /// discarding it wholesale because it names the cancel lost the $500
    /// entirely and settled the blocker as a pure cancel nobody asked for.
    #[test]
    fn a_cancel_word_beside_a_real_fact_is_an_amendment_not_a_cancel() {
        for reply in [
            "scrap the reminder card, budget is $500",
            "scrap the reminder, use 500 units for the final order",
        ] {
            assert_eq!(
                classify_blocker_reply(reply),
                BlockerReplyIntent::Amend,
                "reply: {reply}"
            );
        }
    }

    #[test]
    fn a_substantive_answer_is_an_amendment() {
        for reply in [
            "use gpt-4o-mini instead",
            "deploy to staging, not prod",
            "the brief in the January doc is the current one",
        ] {
            assert_eq!(
                classify_blocker_reply(reply),
                BlockerReplyIntent::Amend,
                "reply: {reply}"
            );
        }
    }

    #[test]
    fn a_greeting_or_a_question_back_is_unrelated() {
        for reply in [
            "hey",
            "hello there",
            "what do you mean?",
            "which one is blocked?",
            "   ",
        ] {
            assert_eq!(
                classify_blocker_reply(reply),
                BlockerReplyIntent::Unrelated,
                "reply: {reply}"
            );
        }
    }

    /// The word match is on whole words: `okra` is not `ok`.
    #[test]
    fn a_verdict_word_matches_only_as_a_whole_word() {
        assert_eq!(
            classify_blocker_reply("order some okra for the office"),
            BlockerReplyIntent::Amend,
            "a substring of a verdict word is not that verdict"
        );
    }

    #[test]
    fn a_negated_verdict_word_is_not_that_verdict() {
        for reply in [
            "don't retry this",
            "do not retry",
            "not approved",
            "no, cancel",
        ] {
            assert_ne!(
                classify_blocker_reply(reply),
                BlockerReplyIntent::Retry,
                "a negated verdict word must not read as the positive verdict: {reply:?}"
            );
        }
        assert_ne!(
            classify_blocker_reply("no, cancel"),
            BlockerReplyIntent::Cancel,
            "a negated cancel is not a cancel"
        );
    }
}
