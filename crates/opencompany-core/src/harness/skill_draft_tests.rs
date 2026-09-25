//! Reading a skill-draft turn out of whatever the model actually sent.
//!
//! The parser is the whole of what this pass owns — the dispatch is one
//! `ModelRequest` — and every arm of it is a real answer shape a provider has
//! produced: the fence it was asked for, a generic fence, an unterminated fence
//! from an answer cut off at the token ceiling, and prose from a turn that
//! asked a question instead.

use super::*;

const DOC: &str = "---\nname: Press Outreach\ndescription: Pitch a story.\n---\nSteps.";

#[test]
fn the_requested_fence_is_read_as_the_document() {
    let raw = format!("Here is a first pass.\n\n```{SKILL_FENCE}\n{DOC}\n```");
    let (reply, doc) = parse_answer(&raw).unwrap();
    assert_eq!(reply, "Here is a first pass.");
    assert_eq!(doc.as_deref(), Some(DOC));
}

#[test]
fn a_generic_markdown_fence_is_read_as_the_document() {
    let raw = format!("Drafted it.\n\n```markdown\n{DOC}\n```");
    let (_, doc) = parse_answer(&raw).unwrap();
    assert_eq!(doc.as_deref(), Some(DOC));
}

/// An answer cut off at the token ceiling has no closing fence. A document
/// missing its last step is worth more to an operator than no document: they
/// can see the cut and ask for the rest.
#[test]
fn an_unterminated_fence_is_read_to_the_end_rather_than_discarded() {
    let raw = format!("Started.\n\n```{SKILL_FENCE}\n{DOC}\n- and then");
    let (reply, doc) = parse_answer(&raw).unwrap();
    assert_eq!(reply, "Started.");
    assert!(doc.unwrap().ends_with("- and then"));
}

/// A skill body writes its examples fenced. Closing at the first ``` would cut
/// the document at its first example and spill the rest into the reply, with
/// nothing on screen saying anything was dropped.
#[test]
fn a_document_containing_its_own_fences_closes_at_the_last_one() {
    let raw =
        format!("Done.\n\n```{SKILL_FENCE}\n{DOC}\n\n```text\nexample\n```\n\nThen file it.\n```");
    let (_, doc) = parse_answer(&raw).unwrap();
    let doc = doc.unwrap();
    assert!(doc.contains("example"), "{doc}");
    assert!(doc.ends_with("Then file it."), "{doc}");
}

#[test]
fn prose_with_no_fence_is_a_question_rather_than_a_document() {
    let (reply, doc) = parse_answer("What should this skill do?").unwrap();
    assert_eq!(reply, "What should this skill do?");
    assert_eq!(doc, None);
}

#[test]
fn an_empty_answer_is_unreadable() {
    assert!(parse_answer("   \n ").is_none());
}

/// The description limit reaches the model as a number rather than as "keep it
/// short", and it is the validator's own constant — so a change to the cap
/// cannot leave the prompt asking for the old one.
#[test]
fn the_system_prompt_states_the_description_cap_the_validator_enforces() {
    let prompt = system_prompt();
    assert!(
        prompt.contains(&MAX_DESCRIPTION_CHARS.to_string()),
        "{prompt}"
    );
    assert!(prompt.contains("when an agent should use it"), "{prompt}");
}

/// The grounding is the company and nothing else. A drafting pass that could
/// read the roster would be a wider capability than the feature needs.
#[test]
fn the_grounding_carries_the_company_and_nothing_else() {
    let prompt = user_prompt(&SkillSubject {
        company_name: "Acme".to_string(),
        company_output: Some("robots".to_string()),
        conversation: Vec::new(),
    });
    assert!(prompt.contains("Acme"), "{prompt}");
    assert!(prompt.contains("robots"), "{prompt}");
}
