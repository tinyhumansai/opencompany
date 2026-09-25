use super::agent_effective_skills;

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

/// Absent inherits, so promoting the field moves no existing company.
#[test]
fn an_absent_scope_inherits_every_enabled_skill() {
    let enabled = strings(&["brand-voice", "meeting-brief", "weekly-report"]);
    assert_eq!(
        agent_effective_skills(&enabled, None),
        strings(&["brand-voice", "meeting-brief", "weekly-report"])
    );
}

/// The state that is unrepresentable without the option: a deliberate
/// no-skills scope, distinct from "never said".
#[test]
fn an_explicit_empty_scope_is_a_deny_all() {
    let enabled = strings(&["brand-voice", "meeting-brief"]);
    assert!(agent_effective_skills(&enabled, Some(&[])).is_empty());
}

#[test]
fn a_listed_scope_narrows_to_its_own_slugs() {
    let enabled = strings(&["brand-voice", "meeting-brief", "weekly-report"]);
    let scope = strings(&["meeting-brief"]);
    assert_eq!(
        agent_effective_skills(&enabled, Some(&scope)),
        strings(&["meeting-brief"])
    );
}

/// The narrow-only property. A scope naming a skill the company does not have
/// enabled resolves to nothing — it can never re-enable what was turned off,
/// whichever layer turned it off.
#[test]
fn a_scope_cannot_reach_a_skill_the_company_has_not_enabled() {
    let enabled = strings(&["brand-voice"]);
    let scope = strings(&["meeting-brief", "retired-playbook"]);
    assert!(agent_effective_skills(&enabled, Some(&scope)).is_empty());

    let mixed = strings(&["brand-voice", "meeting-brief"]);
    assert_eq!(
        agent_effective_skills(&enabled, Some(&mixed)),
        strings(&["brand-voice"])
    );
}

/// An unknown slug is dropped rather than fatal, so retiring a skill does not
/// brick a manifest that still names it.
#[test]
fn an_unknown_slug_is_dropped_and_the_rest_survives() {
    let enabled = strings(&["brand-voice", "meeting-brief"]);
    let scope = strings(&["meeting-brief", "never-installed"]);
    assert_eq!(
        agent_effective_skills(&enabled, Some(&scope)),
        strings(&["meeting-brief"])
    );
}

/// Slugs match exactly. A prefix entry reaches nothing, so a scope written
/// today cannot pick up a skill installed tomorrow.
#[test]
fn a_prefix_entry_is_not_a_glob() {
    let enabled = strings(&["brand-voice", "brand-pricing-internal"]);
    for entry in ["brand-*", "brand", "*", "brand-voice*"] {
        let scope = strings(&[entry]);
        assert!(
            agent_effective_skills(&enabled, Some(&scope)).is_empty(),
            "{entry} matched something"
        );
    }
}

/// The company's order is the result's order, and a slug listed twice appears
/// once — the same treatment the tool grants get.
#[test]
fn the_result_follows_the_company_order_and_is_deduplicated() {
    let enabled = strings(&["weekly-report", "brand-voice", "meeting-brief"]);
    let scope = strings(&["meeting-brief", "weekly-report", "meeting-brief"]);
    assert_eq!(
        agent_effective_skills(&enabled, Some(&scope)),
        strings(&["weekly-report", "meeting-brief"])
    );
}

/// An empty company set resolves to nothing for every scope shape, so an agent
/// in a company with no skills never carries a catalogue.
#[test]
fn an_empty_company_set_yields_nothing() {
    let enabled: Vec<String> = Vec::new();
    let scope = strings(&["meeting-brief"]);
    assert!(agent_effective_skills(&enabled, None).is_empty());
    assert!(agent_effective_skills(&enabled, Some(&[])).is_empty());
    assert!(agent_effective_skills(&enabled, Some(&scope)).is_empty());
}
