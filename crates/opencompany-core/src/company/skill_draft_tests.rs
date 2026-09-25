//! What counts as a drafted skill.

use super::*;

#[test]
fn a_turn_that_wrote_a_document_carries_it() {
    let draft = SkillDraft::from_answer("  Here it is.  ", Some("---\nname: x\n---\n"));
    assert_eq!(draft.reply(), Some("Here it is."));
    assert_eq!(draft.doc(), Some("---\nname: x\n---"));
    assert!(draft.refusal().is_none());
}

#[test]
fn a_turn_that_asked_a_question_carries_no_document() {
    let draft = SkillDraft::from_answer("What should it do?", None);
    assert_eq!(draft.doc(), None);
    assert!(draft.reply().is_some());
}

/// An opened fence with nothing in it is a question, not an empty skill. The
/// alternative is a blank box in front of the operator with nothing saying why.
#[test]
fn an_empty_document_is_read_as_no_document() {
    let draft = SkillDraft::from_answer("Working on it.", Some("   \n  "));
    assert_eq!(draft.doc(), None);
}

#[test]
fn a_refusal_carries_its_reason_and_nothing_else() {
    let draft = SkillDraft::Refused(DraftRefusal::NoModel);
    assert_eq!(draft.refusal(), Some(DraftRefusal::NoModel));
    assert_eq!(draft.doc(), None);
    assert_eq!(draft.reply(), None);
}
