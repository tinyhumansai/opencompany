use super::*;

fn agent(role: &str) -> Agent {
    Agent {
        provider: None,
        global: false,
        id: "a".into(),
        role: role.into(),
        name: None,
        description: None,
        tier: None,
        harness: None,
        tools: None,
        skills: None,
        delegates_to: Vec::new(),
        context: None,
        budget_usd_daily: None,
        prompt: None,
        prompt_files: Vec::new(),
        prompt_files_resolved: Vec::new(),
        classes: Vec::new(),
        ledgers: None,
        can_declare_ledgers: true,
        model: None,
    }
}

#[test]
fn the_persona_names_the_role_and_company() {
    let prompt = persona_prompt("Acme", &agent("Copywriter"), None);
    assert!(prompt.contains("Copywriter"), "{prompt}");
    assert!(prompt.contains("Acme"), "{prompt}");
}

#[test]
fn a_named_teammate_is_framed_as_the_name_and_the_role() {
    // Issue #1105: the console addresses an operator-added teammate by name
    // everywhere, so the model has to be told the name it is answering to —
    // without losing the role, which is what it is here to do.
    let mut a = agent("Content Writer");
    a.name = Some("Alex".into());

    let prompt = persona_prompt("Acme", &a, None);
    assert!(
        prompt.contains("You are Alex, the Content Writer at Acme"),
        "{prompt}"
    );
    // Stated as an address rather than a character to inhabit.
    assert!(prompt.contains("address you as Alex"), "{prompt}");
    assert!(
        prompt.contains("not a separate character to play"),
        "{prompt}"
    );
}

#[test]
fn a_teammate_with_no_name_keeps_the_role_only_framing() {
    // The unnamed arm must stay byte-identical: every manifest teammate
    // takes it, and its wording is pinned by tests elsewhere.
    assert_eq!(
        persona_prompt("Acme", &agent("Content Writer"), None),
        "You are the Content Writer at Acme. Speak in the first person as this role."
    );
}

#[test]
fn a_blank_name_falls_back_to_the_role_only_framing() {
    let mut a = agent("Content Writer");
    a.name = Some("   \n ".into());
    assert_eq!(
        persona_prompt("Acme", &a, None),
        persona_prompt("Acme", &agent("Content Writer"), None)
    );
}

#[test]
fn a_name_that_restates_the_role_is_not_repeated() {
    // Otherwise: "You are Content Writer, the Content Writer at Acme."
    let mut a = agent("Content Writer");
    a.name = Some("content writer".into());
    assert_eq!(
        persona_prompt("Acme", &a, None),
        persona_prompt("Acme", &agent("Content Writer"), None)
    );
}

#[test]
fn an_inline_prompt_is_appended_to_the_persona_not_substituted_for_it() {
    let mut a = agent("Copywriter");
    a.description = Some("Write ads.".into());
    a.prompt = Some("Write in the brand's voice.".into());

    let prompt = persona_prompt("Acme", &a, a.prompt.as_deref());
    // The identity framing survives — that is the whole reason this appends.
    assert!(
        prompt.contains("You are the Copywriter at Acme"),
        "{prompt}"
    );
    assert!(prompt.contains("Write ads."), "{prompt}");
    assert!(prompt.contains("Write in the brand's voice."), "{prompt}");
    // And the operator's instruction comes last, where it is not buried.
    assert!(
        prompt.find("Write ads.") < prompt.find("Write in the brand's voice."),
        "{prompt}"
    );
}

#[test]
fn a_blank_inline_prompt_adds_nothing() {
    let mut a = agent("Copywriter");
    a.prompt = Some("   \n  ".into());
    assert_eq!(
        persona_prompt("Acme", &a, a.prompt.as_deref()),
        persona_prompt("Acme", &agent("Copywriter"), None)
    );
}

#[test]
fn an_agent_with_no_documents_gets_no_section() {
    assert_eq!(bundle_section(&agent("X")), "");
    assert_eq!(context_section(&[]), "");
}

/// A document that exists but says nothing is dropped rather than rendered
/// as an empty heading — the model reads the latter as a real, empty source.
#[test]
fn whitespace_only_documents_are_dropped() {
    let mut a = agent("X");
    a.prompt_files_resolved = vec![("empty.md".into(), "   \n\n  ".into())];
    assert_eq!(bundle_section(&a), "");

    assert_eq!(context_section(&[("blank.md".into(), "\n".into())]), "");
}

#[test]
fn documents_are_rendered_under_their_names_in_order() {
    let mut a = agent("X");
    a.prompt_files_resolved = vec![
        ("prompts/tone.md".into(), "Be direct.".into()),
        ("prompts/style.md".into(), "Short sentences.".into()),
    ];

    let section = bundle_section(&a);
    assert!(section.contains("### prompts/tone.md"), "{section}");
    assert!(section.contains("Be direct."), "{section}");
    assert!(
        section.find("prompts/tone.md") < section.find("prompts/style.md"),
        "declared order is preserved: {section}"
    );
}

#[test]
fn the_two_sections_have_distinct_headings() {
    let mut a = agent("X");
    a.prompt_files_resolved = vec![("brief.md".into(), "body".into())];
    let bundle = bundle_section(&a);
    let context = context_section(&[("GOAL.md".into(), "body".into())]);
    assert_ne!(bundle, context);
    assert!(bundle.contains("Your brief"), "{bundle}");
    assert!(context.contains("Working documents"), "{context}");
}

#[test]
fn text_within_budget_is_returned_untouched() {
    assert_eq!(clamp("short", 100), "short");
    // Exactly at the budget is within it.
    assert_eq!(clamp("abcde", 5), "abcde");
}

#[test]
fn an_over_budget_clamp_keeps_the_leading_portion_and_marks_the_cut() {
    let clamped = clamp("abcdefghij", 4);
    assert_eq!(
        clamped,
        format!("abcd{TRUNCATION_MARKER}"),
        "the leading portion is kept, the tail dropped, and the cut marked"
    );
}

/// The property a byte-based clamp gets wrong: cutting mid-codepoint would
/// panic or produce invalid UTF-8.
#[test]
fn the_clamp_cuts_on_a_character_boundary() {
    // Each emoji is 4 bytes, so a byte-indexed slice at 5 would split one.
    let text = "🙂🙂🙂🙂";
    let clamped = clamp(text, 2);
    assert!(clamped.starts_with("🙂🙂"), "{clamped}");
    assert!(!clamped.starts_with("🙂🙂🙂"), "{clamped}");
    // Reaching here at all proves it did not panic on a byte boundary.
    assert!(clamped.contains("truncated"));
}

/// Multi-byte text whose byte length exceeds the budget but whose codepoint
/// count does not must survive whole — the cheap byte-length pre-check must
/// not itself become the cut.
#[test]
fn multibyte_text_within_the_codepoint_budget_is_not_cut() {
    let text = "🙂🙂🙂"; // 3 chars, 12 bytes
    assert_eq!(clamp(text, 4), text);
}

/// Overlong persona instructions are capped to the prompt budget at the
/// write boundary, keeping the leading portion and marking the cut — the
/// same budget `bundle_section`/`context_section` already apply, applied
/// here because `instructions` are injected into every turn's prompt.
#[test]
fn persona_instructions_are_capped_to_the_prompt_budget() {
    let over = "x".repeat(PROMPT_FILE_BUDGET_CHARS + 50);
    let capped = cap_persona_instructions(&over);
    assert!(capped.starts_with(&"x".repeat(PROMPT_FILE_BUDGET_CHARS)));
    assert!(capped.contains("truncated"), "a cut is marked: {capped:?}");

    // Under-budget text passes through untouched.
    let short = "Answer only in haiku.";
    assert_eq!(cap_persona_instructions(short), short);

    // The cut is on a character boundary for multi-byte text.
    let many = "é".repeat(PROMPT_FILE_BUDGET_CHARS + 100);
    let capped = cap_persona_instructions(&many);
    assert!(
        capped.starts_with(&"é".repeat(PROMPT_FILE_BUDGET_CHARS)),
        "multi-byte text is cut whole, never panicking: {capped:?}"
    );
}

#[test]
fn the_section_budget_applies_across_documents_not_per_document() {
    let long = "x".repeat(PROMPT_FILE_BUDGET_CHARS);
    let mut a = agent("X");
    a.prompt_files_resolved = vec![
        ("one.md".into(), long.clone()),
        ("two.md".into(), long.clone()),
    ];

    let section = bundle_section(&a);
    assert!(
        section.contains("truncated"),
        "two budget-sized documents must not buy two budgets"
    );
    // The second document is past the budget, so its heading never appears.
    assert!(
        !section.contains("### two.md"),
        "section rendered past budget"
    );
}
