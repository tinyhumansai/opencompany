use super::*;
use oh::agent::prompts::GROUNDING_HEADING;

const STYLE_HEADING: &str = "# Writing style";

fn blueprint(dir: &std::path::Path, is_orchestrator: bool) -> AgentBlueprint {
    let deps = pin_deps(dir.to_path_buf());
    let manifest_agent = ManifestAgent {
        provider: None,
        global: false,
        id: "qa_engineer".to_string(),
        role: "QA Engineer".to_string(),
        name: Some("Quinn".to_string()),
        description: Some("Finds the failing case.".to_string()),
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
    };
    build_agent(
        &CompanyId::new("acme"),
        "Acme",
        &manifest_agent,
        Arc::new(ApprovalPolicy::new(&Policy::default(), None)),
        &deps,
        &[],
        &[],
        &[],
        None,
        is_orchestrator,
    )
    .expect("agent builds")
}

#[test]
fn a_seat_persona_carries_grounding_and_the_writing_style() {
    let dir = tempfile::tempdir().expect("tempdir");
    let agent = blueprint(dir.path(), false);
    assert!(
        !agent.system_prompt.contains(STYLE_HEADING),
        "the body alone carries no style block"
    );

    let persona = rendered_seat_persona(&agent).expect("renders");
    assert!(
        persona.starts_with(agent.system_prompt.trim_end()),
        "{persona}"
    );
    assert_eq!(persona.matches(STYLE_HEADING).count(), 1, "{persona}");
    assert_eq!(persona.matches(GROUNDING_HEADING).count(), 1, "{persona}");
    assert!(
        persona.find(GROUNDING_HEADING) < persona.find(STYLE_HEADING),
        "grounding closes the stable tier ahead of the style rules: {persona}"
    );
}

#[test]
fn the_seat_persona_is_stable_and_reads_the_seats_own_style() {
    let dir = tempfile::tempdir().expect("tempdir");
    let agent = blueprint(dir.path(), true);
    let persona = rendered_seat_persona(&agent).expect("renders");
    assert_eq!(
        persona,
        rendered_seat_persona(&agent).expect("renders again"),
        "stable across renders, so every seeded turn carries the same prefix"
    );
    assert!(
        agent.workspace.join("STYLE.md").exists(),
        "the style block is read from the seat's own workspace"
    );
}

#[test]
fn the_reader_brief_reaches_pooled_and_seated_prompts_exactly_once() {
    for is_orchestrator in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = blueprint(dir.path(), is_orchestrator);
        let seat = rendered_seat_persona(&agent).expect("renders");
        for (surface, prompt) in [("pooled", &agent.system_prompt), ("seat", &seat)] {
            assert_eq!(
                prompt.matches(READER_BRIEF).count(),
                1,
                "{surface} prompt (orchestrator: {is_orchestrator}): {prompt}"
            );
        }
    }
}

#[test]
fn the_reader_brief_sets_the_audience_not_a_length_budget() {
    assert!(READER_BRIEF.contains("A person reads what you post"));
    assert!(READER_BRIEF.contains("belong in tool calls"));
    assert!(
        !READER_BRIEF.contains('—'),
        "OpenHuman's style forbids em-dashes"
    );
    for budget in ["word limit", "at most", "no more than", "concise"] {
        assert!(
            !READER_BRIEF.to_lowercase().contains(budget),
            "`{budget}` reads as a length rule: {READER_BRIEF}"
        );
    }
}

#[test]
fn the_mention_brief_models_a_name_not_a_roster_id() {
    let examples: Vec<&str> = MENTION_BRIEF.split('"').skip(1).step_by(2).collect();
    assert!(
        !examples.is_empty(),
        "the brief shows an example: {MENTION_BRIEF}"
    );
    for example in examples {
        let id_like = example
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .any(|token| token.contains('_') || token.contains('-'));
        assert!(!id_like, "the example reads as a roster id: {example}");
    }
}
