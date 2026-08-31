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
//! paths call, so the issue-#463 title contract with
//! `DelegationRunner::chat_handler_card` — which has to derive byte-for-byte
//! the same title the handler wrote — is untouched.
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
        return matched(MessageTriage::Track(to_title(trimmed)));
    }
    if is_question(core) {
        return matched(MessageTriage::Answer);
    }
    if starts_with_action(core) {
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
/// Kept as its own function because two card paths depend on it agreeing with
/// itself byte-for-byte: the REST chat handler writes the title, and
/// `DelegationRunner::chat_handler_card` re-derives it moments later to find
/// the card that handler wrote (issue #463).
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
