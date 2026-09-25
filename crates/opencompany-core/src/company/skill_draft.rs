//! What a drafted skill is, before any model is involved.
//!
//! The counterpart of [`profile_draft`](super::profile_draft) for a skill
//! document: the grounding one turn is allowed to see, and the answer it can
//! produce. The conversation types, the turn bounds and the refusal reasons are
//! that module's — one copilot, one set of rules, so the two cannot disagree
//! about what a transcript is or why a draft was refused.
//!
//! The answer is a whole `SKILL.md`, never a diff. A skill is a frontmatter
//! block and a body that only make sense together: a model handed back "change
//! the description to X" would leave the console reassembling a document from
//! prose, which is the step at which a drafted skill stops matching what the
//! operator read.

use super::profile_draft::{CopilotTurn, DraftRefusal};

/// Everything a skill draft is allowed to see.
///
/// Deliberately narrow: the company it is for, what that company makes, and the
/// conversation. A skill is a procedure, not a teammate — nothing about the
/// roster grounds it, and a drafting pass that could read one would be a wider
/// capability than the feature needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillSubject {
    /// The company the skill is being written for.
    pub company_name: String,
    /// What that company makes, when its manifest says.
    pub company_output: Option<String>,
    /// The conversation so far, oldest first — empty on the opening turn.
    pub conversation: Vec<CopilotTurn>,
}

/// One turn's answer: written by a model when one is wired, refused with a
/// reason when none is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillDraft {
    /// The copilot answered.
    Answered {
        /// What it says — what it wrote, or what it needs to know.
        reply: String,
        /// The whole `SKILL.md` as it now stands. `None` on a turn that asked a
        /// question instead of drafting, which is not a failure: letting a turn
        /// ask is what makes this a conversation rather than a hint box.
        doc: Option<String>,
    },
    /// No draft, and why.
    Refused(DraftRefusal),
}

impl SkillDraft {
    /// The drafted document, when this turn produced one.
    pub fn doc(&self) -> Option<&str> {
        match self {
            Self::Answered { doc, .. } => doc.as_deref(),
            Self::Refused(_) => None,
        }
    }

    /// What the copilot said, when it answered.
    pub fn reply(&self) -> Option<&str> {
        match self {
            Self::Answered { reply, .. } => Some(reply),
            Self::Refused(_) => None,
        }
    }

    /// Why there is no draft, when there is none.
    pub fn refusal(&self) -> Option<DraftRefusal> {
        match self {
            Self::Answered { .. } => None,
            Self::Refused(reason) => Some(*reason),
        }
    }

    /// Reads one answer, treating a blank document as no document.
    ///
    /// A model that opened the fence and wrote nothing inside it has asked a
    /// question with extra steps; storing the empty string as the draft would
    /// put a blank box in front of the operator with nothing saying why.
    pub fn from_answer(reply: &str, doc: Option<&str>) -> Self {
        Self::Answered {
            reply: reply.trim().to_string(),
            doc: doc
                .map(str::trim)
                .filter(|doc| !doc.is_empty())
                .map(str::to_string),
        }
    }
}

#[cfg(test)]
#[path = "skill_draft_tests.rs"]
mod tests;
