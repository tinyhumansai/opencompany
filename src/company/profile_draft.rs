//! Issue #1776 — drafting **one** teammate's mandate or persona, for an
//! operator who then keeps it or throws it away.
//!
//! A teammate's `description` (the mandate: one line on what it owns) and
//! `instructions` (the persona appended to its system prompt) are the two
//! fields that decide how it behaves, and the two an operator has the least
//! help writing. This module holds the shape of a draft; the model call that
//! produces one lives in [`crate::harness::profile_draft`], behind the
//! `openhuman` feature.
//!
//! ## Why this is not the roster designer's rule being relaxed
//!
//! [`crate::company::setup`] deliberately keeps the model out of a teammate's
//! standing instructions: it names a work *shape* from a closed enum and the
//! host owns every word. That rule is untouched, and it must stay that way —
//! it governs teammates that are **created** from a design pass, where the text
//! reaches a system prompt with nobody having read it, through a route any
//! member can call.
//!
//! A draft is the opposite case in the one way that matters. It is returned to
//! the operator and stored by **nothing**: the route that produces it never
//! writes, the console shows it beside the field rather than in it, and the
//! text only becomes a persona if a person takes it and then saves. That is the
//! same stance the workflow copilot's proposal protocol takes — the model's
//! output is data in a reply, and the operator's own action is what writes.
//!
//! So the boundary this module holds is narrow and specific: **a draft is
//! bounded like the field it is for, and it is never applied here.**

use crate::company::prompt::cap_persona_instructions;
use crate::company::setup::clamp_description;

/// Which authored field a draft is for.
///
/// Only the two prose fields. `name` and `role` are short identity values an
/// operator picks in seconds — and `role` is what delegation grounds on, so a
/// drafted one would change who the company routes work to, which is not a
/// thing to hand a model on a screen whose whole promise is "this changes
/// nothing until you save".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileField {
    /// The one-line mandate, shown on the roster card.
    Description,
    /// The persona appended to this teammate's system prompt.
    Instructions,
}

impl ProfileField {
    /// The wire spelling, which is also the `PATCH` field name — the console
    /// asks for a draft of the field it is about to fill.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Description => "description",
            Self::Instructions => "instructions",
        }
    }

    /// Reads a field off the wire. Anything else is `None`, so a request naming
    /// a field this pass does not draft is refused rather than silently
    /// answered about a different one.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "description" => Some(Self::Description),
            "instructions" => Some(Self::Instructions),
            _ => None,
        }
    }

    /// Brings a draft inside the bound the field itself obeys.
    ///
    /// Applied **host-side**, on the way out, so the console is not the only
    /// thing holding the limit — the same reason the roster pass clamps its own
    /// mandates rather than trusting the review screen to.
    ///
    /// The two bounds are different in kind, and each field gets its own:
    /// a mandate is clamped to [`MAX_DESCRIPTION`](crate::company::setup::MAX_DESCRIPTION)
    /// because the roster card has one line for it, while a persona is capped
    /// by prompt weight because it is read on every turn of that teammate.
    pub fn clamp(self, text: &str) -> String {
        match self {
            Self::Description => clamp_description(text),
            Self::Instructions => cap_persona_instructions(text.trim()),
        }
    }
}

/// Why no draft came back.
///
/// Several reasons rather than one, because **the operator's next move
/// differs** — the same split [`FallbackReason`](crate::company::setup::FallbackReason)
/// makes for the roster pass, and for the same reason: one sentence covering
/// all of them can only be vague enough to be useless. "Wire up a model",
/// "try again", "say more" and "wait for the period to reset" are four
/// different actions, and a reader who cannot tell which one they are in has
/// been told nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftRefusal {
    /// Nothing was reachable, so no call ran. Wire up a model.
    NoModel,
    /// A model is wired and the call did not land — a timeout, an unreachable
    /// provider. Retry, or check the provider; adding a key would fix nothing.
    ModelUnreachable,
    /// A model answered and the answer could not be used. Say more in the hint,
    /// or write the field by hand.
    Unreadable,
    /// The company has spent its plan-level token ceiling for the period
    /// (issue #188), so no call ran. Nothing the operator types will change
    /// that until the window resets or the ceiling is raised.
    BudgetExhausted,
}

impl DraftRefusal {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoModel => "no_model",
            Self::ModelUnreachable => "model_unreachable",
            Self::Unreadable => "unreadable",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }
}

/// Who said one thing in a copilot conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    /// The operator asking for something.
    Operator,
    /// The copilot's own earlier answer.
    Copilot,
}

impl TurnRole {
    /// Reads a role off the wire. Anything unrecognised is `None` — a turn
    /// whose speaker cannot be established is dropped rather than guessed at,
    /// because attributing the operator's words to the copilot (or the reverse)
    /// is how a conversation starts arguing with itself.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "operator" => Some(Self::Operator),
            "copilot" => Some(Self::Copilot),
            _ => None,
        }
    }
}

/// One thing said in a copilot conversation.
///
/// The console holds the transcript and sends it back each turn — the host
/// stores nothing. That is the whole of "in-session": closing the form ends the
/// conversation, and there is no journal to rehydrate from, no thread id to
/// collide, and no cleanup to get wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopilotTurn {
    pub role: TurnRole,
    pub text: String,
}

/// How many turns of a conversation are carried into the prompt.
///
/// A bound rather than a trim the console is trusted to do. Long conversations
/// are exactly the ones where an operator has been iterating hardest, so the
/// tail is what matters — the oldest turns are dropped first, and the grounding
/// block is re-sent every turn regardless, so nothing load-bearing ages out.
pub const MAX_TURNS: usize = 16;

/// The longest one turn may be.
///
/// Applies to both sides. An operator pasting a page into the box, or a copilot
/// that answered with an essay, must not be able to push the grounding out of a
/// later prompt.
pub const MAX_TURN_CHARS: usize = 2_000;

/// Brings a conversation inside the bounds the prompt obeys: the last
/// [`MAX_TURNS`], each clamped to [`MAX_TURN_CHARS`], blank turns dropped.
///
/// Host-side, so the console is not the only thing holding the bound — the same
/// argument the field clamps make.
pub fn clamp_conversation(turns: Vec<CopilotTurn>) -> Vec<CopilotTurn> {
    let kept: Vec<CopilotTurn> = turns
        .into_iter()
        .filter(|turn| !turn.text.trim().is_empty())
        .map(|turn| CopilotTurn {
            role: turn.role,
            text: turn.text.chars().take(MAX_TURN_CHARS).collect(),
        })
        .collect();
    let start = kept.len().saturating_sub(MAX_TURNS);
    kept[start..].to_vec()
}

/// One teammate a draft is about, plus everything the pass is allowed to see.
///
/// A closed set, assembled host-side from the company record. The console does
/// not compose it and cannot add to it: a draft is grounded in this teammate,
/// its siblings' roles, and what the operator typed into the hint — never in
/// the rest of the company.
#[derive(Debug, Clone, Default)]
pub struct ProfileSubject {
    /// The company's name, so a mandate reads as one of *this* company's.
    pub company_name: String,
    /// What the company produces (`[company].output`), when it declares it.
    pub company_output: Option<String>,
    /// The teammate's roster id.
    pub agent_id: String,
    /// Its name, when an operator has given it one.
    pub name: Option<String>,
    /// Its role — the one field a draft can always lean on.
    pub role: String,
    /// The mandate in force, so a redraft improves on it rather than ignoring
    /// it, and so a persona can be written to fit the job the card claims.
    pub description: Option<String>,
    /// The persona in force, for the same reason.
    pub instructions: Option<String>,
    /// The rest of the roster — **id and role only**.
    ///
    /// Named so a drafted mandate does not restate a sibling's. The delegation
    /// surface renders id and role and nothing else, so two teammates whose
    /// mandates overlap are two the company cannot tell apart when it comes to
    /// hand out work (issue #1162).
    pub siblings: Vec<Sibling>,
    /// The conversation so far, oldest first — already clamped.
    ///
    /// Empty on the opening turn, which means "draft something, I have not said
    /// anything yet". That path is kept deliberately: an operator staring at a
    /// blank persona box often wants a starting point to react to, and making
    /// them type first would ask for the very thing they opened the copilot
    /// because they could not write.
    pub conversation: Vec<CopilotTurn>,
}

/// One other teammate on the roster, as a draft is told about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sibling {
    pub id: String,
    pub role: String,
}

/// What one copilot turn produced.
///
/// Either an answer or a reason there is none — never both, and never neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileDraft {
    /// The copilot said something, and possibly drafted.
    Answered {
        /// What it says in the conversation — what it changed, or what it needs
        /// to know. Always present.
        reply: String,
        /// The whole field as it now stands, already clamped. `None` when the
        /// turn was a question rather than a draft.
        ///
        /// A copilot that must always produce text cannot ask what the operator
        /// means; it can only guess and hand back a paragraph. Letting a turn
        /// carry a question and no draft is what makes this a conversation
        /// rather than a slot machine with a text box.
        draft: Option<String>,
    },
    /// No answer at all, and why.
    Refused(DraftRefusal),
}

impl ProfileDraft {
    /// The drafted text, or `None` when this turn asked rather than drafted.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Answered { draft, .. } => draft.as_deref(),
            Self::Refused(_) => None,
        }
    }

    /// What the copilot said, or `None` when the pass refused.
    pub fn reply(&self) -> Option<&str> {
        match self {
            Self::Answered { reply, .. } => Some(reply),
            Self::Refused(_) => None,
        }
    }

    /// The refusal, or `None` when the copilot answered.
    pub fn refusal(&self) -> Option<DraftRefusal> {
        match self {
            Self::Answered { .. } => None,
            Self::Refused(reason) => Some(*reason),
        }
    }

    /// Builds the outcome for a model answer: the reply as-is, the draft
    /// clamped for its field, and a blank draft treated as "asked, did not
    /// draft" rather than as an empty suggestion.
    ///
    /// A turn with neither is unreadable. A model that answers with whitespace
    /// has technically replied, and putting that on screen reads as the copilot
    /// having nothing to say about a teammate rather than as the failure it is.
    pub fn from_answer(field: ProfileField, reply: &str, draft: Option<&str>) -> Self {
        let reply = reply.trim();
        let draft = draft
            .map(|text| field.clamp(text))
            .filter(|text| !text.trim().is_empty());
        match (reply.is_empty(), draft) {
            // Nothing said and nothing drafted: there is no turn here.
            (true, None) => Self::Refused(DraftRefusal::Unreadable),
            // A draft with no covering sentence is still a good turn — the
            // draft speaks for itself, and inventing prose for it would put
            // words in the copilot's mouth.
            (true, Some(draft)) => Self::Answered {
                reply: String::new(),
                draft: Some(draft),
            },
            (false, draft) => Self::Answered {
                reply: reply.to_string(),
                draft,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::company::setup::MAX_DESCRIPTION;

    #[test]
    fn only_the_two_prose_fields_are_draftable() {
        assert_eq!(
            ProfileField::parse("description"),
            Some(ProfileField::Description)
        );
        assert_eq!(
            ProfileField::parse(" instructions "),
            Some(ProfileField::Instructions)
        );
        for other in ["name", "role", "tools", "model", "", "Description"] {
            assert_eq!(ProfileField::parse(other), None, "{other}");
        }
    }

    /// The bound is the field's own, applied here rather than trusted to the
    /// console — a caller that is not our console gets the same clamp.
    #[test]
    fn a_long_mandate_is_clamped_to_the_card() {
        let long = "x ".repeat(MAX_DESCRIPTION);
        let draft =
            ProfileDraft::from_answer(ProfileField::Description, "here you go", Some(&long));
        let text = draft.text().expect("a long answer still drafts");
        assert!(
            text.chars().count() <= MAX_DESCRIPTION + 1,
            "clamped to the card: {} chars",
            text.chars().count()
        );
    }

    /// A persona is bounded by prompt weight, not by the card — the two limits
    /// are different in kind, so a persona well over the mandate bound survives.
    #[test]
    fn a_persona_is_not_clamped_to_the_mandate_bound() {
        let persona = "Confirm the budget before launching. ".repeat(20);
        let draft =
            ProfileDraft::from_answer(ProfileField::Instructions, "tightened it", Some(&persona));
        let text = draft.text().expect("a persona drafts");
        assert!(
            text.chars().count() > MAX_DESCRIPTION,
            "a persona is not held to the card's one line: {} chars",
            text.chars().count()
        );
    }

    /// Nothing said and nothing drafted is not a turn.
    #[test]
    fn an_empty_turn_is_unreadable_rather_than_a_blank_suggestion() {
        for blank in ["", "   ", "\n\t "] {
            let draft = ProfileDraft::from_answer(ProfileField::Instructions, blank, Some(blank));
            assert_eq!(draft.refusal(), Some(DraftRefusal::Unreadable), "{blank:?}");
            assert_eq!(draft.text(), None);
            assert_eq!(
                ProfileDraft::from_answer(ProfileField::Instructions, blank, None).refusal(),
                Some(DraftRefusal::Unreadable),
                "{blank:?}"
            );
        }
    }

    /// A question with no draft is a good turn — it is what lets the copilot
    /// find out what the operator means instead of guessing at a paragraph.
    #[test]
    fn a_question_without_a_draft_is_a_real_turn() {
        let turn = ProfileDraft::from_answer(
            ProfileField::Instructions,
            "Should they be able to sign off releases themselves, or does that go to the lead?",
            None,
        );
        assert_eq!(turn.refusal(), None);
        assert_eq!(turn.text(), None, "a question drafts nothing");
        assert!(
            turn.reply()
                .expect("it said something")
                .contains("sign off")
        );
    }

    /// A blank draft beside a real reply is a question, not an empty
    /// suggestion card.
    #[test]
    fn a_reply_with_a_blank_draft_drafts_nothing() {
        let turn =
            ProfileDraft::from_answer(ProfileField::Description, "What do they own?", Some("  "));
        assert_eq!(turn.text(), None);
        assert_eq!(turn.refusal(), None);
    }

    /// The conversation is bounded host-side: oldest turns drop first, each
    /// turn is clamped, and blank turns never reach the prompt.
    #[test]
    fn a_long_conversation_keeps_its_tail() {
        let turns: Vec<CopilotTurn> = (0..MAX_TURNS + 6)
            .map(|i| CopilotTurn {
                role: if i % 2 == 0 {
                    TurnRole::Operator
                } else {
                    TurnRole::Copilot
                },
                text: format!("turn {i}"),
            })
            .collect();
        let kept = clamp_conversation(turns);
        assert_eq!(kept.len(), MAX_TURNS);
        assert_eq!(
            kept.first().expect("kept").text,
            "turn 6",
            "the oldest drop first"
        );
        assert_eq!(
            kept.last().expect("kept").text,
            format!("turn {}", MAX_TURNS + 5)
        );
    }

    #[test]
    fn a_conversation_drops_blanks_and_clamps_each_turn() {
        let kept = clamp_conversation(vec![
            CopilotTurn {
                role: TurnRole::Operator,
                text: "   ".to_string(),
            },
            CopilotTurn {
                role: TurnRole::Operator,
                text: "x".repeat(MAX_TURN_CHARS + 500),
            },
        ]);
        assert_eq!(kept.len(), 1, "a blank turn is not a turn");
        assert_eq!(kept[0].text.chars().count(), MAX_TURN_CHARS);
    }

    /// A turn whose speaker cannot be established is dropped rather than
    /// guessed at — attributing the operator's words to the copilot is how a
    /// conversation starts arguing with itself.
    #[test]
    fn only_the_two_known_speakers_parse() {
        assert_eq!(TurnRole::parse("operator"), Some(TurnRole::Operator));
        assert_eq!(TurnRole::parse(" copilot "), Some(TurnRole::Copilot));
        for other in ["system", "assistant", "user", ""] {
            assert_eq!(TurnRole::parse(other), None, "{other}");
        }
    }

    #[test]
    fn a_refusal_names_the_operators_next_move() {
        assert_eq!(DraftRefusal::NoModel.as_str(), "no_model");
        assert_eq!(DraftRefusal::ModelUnreachable.as_str(), "model_unreachable");
        assert_eq!(DraftRefusal::Unreadable.as_str(), "unreadable");
    }
}
