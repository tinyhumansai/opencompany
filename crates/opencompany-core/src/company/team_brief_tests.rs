use super::*;
use crate::ports::types::CompanyId;

fn record(manifest: &str) -> CompanyRecord {
    CompanyRecord::from_manifest(
        CompanyId::new("acme"),
        toml::from_str(manifest).expect("valid manifest"),
    )
}

const TEAM: &str = r#"
[company]
name = "Acme"

[[agent]]
id = "pm"
role = "Product Manager"
tier = "orchestrator"
description = "Own the roadmap."

[[agent]]
id = "backend"
role = "Backend Engineer"
description = "Build the services."
delegates_to = ["engineering"]

[[agent]]
id = "designer"
role = "Designer"

[[agent]]
id = "writer"
role = "Writer"

[[group_chat]]
id = "engineering"
name = "Engineering"
members = ["backend", "designer"]

[[group_chat]]
id = "content"
name = "Content"
members = ["writer"]
"#;

#[test]
fn a_solo_roster_gets_no_section() {
    let record = record(
        r#"
[company]
name = "Acme"

[[agent]]
id = "solo"
role = "Everything"
"#,
    );
    assert_eq!(team_section(&record, "solo"), "");
}

#[test]
fn every_other_teammate_is_listed_with_role_and_mandate_but_not_the_agent_itself() {
    let section = team_section(&record(TEAM), "designer");
    assert!(section.starts_with("\n\n## Your team"), "{section}");
    assert!(section.contains("one of 4 teammates at Acme"), "{section}");
    assert!(
        section.contains("- Product Manager (the orchestrator"),
        "{section}"
    );
    assert!(
        section.contains("owns the board): Own the roadmap. (id `pm` for tool calls)\n"),
        "{section}"
    );
    assert!(
        section.contains("- Backend Engineer: Build the services. (id `backend` for tool calls)\n"),
        "{section}"
    );
    assert!(
        section.contains("- Writer (id `writer` for tool calls)\n"),
        "{section}"
    );
    assert!(!section.contains("`designer`"), "{section}");
}

/// The desk block is the seat's OWN desks, with their members and lead.
///
/// It used to list every desk in the company. A seat was then handed the same
/// desk twice in one turn under two different mechanisms — here as somewhere to
/// hand work, whose lead takes one turn, and in `EpisodePrompt::peers` as
/// somewhere to put a question, which the room answers — with different costs
/// and no way to tell them apart (#2368).
///
/// The roster of PEOPLE above is deliberately not narrowed the same way:
/// knowing who does what is how a seat knows who is worth asking.
#[test]
fn desks_list_their_members_and_lead_and_the_agents_own_seat() {
    let section = team_section(&record(TEAM), "designer");
    assert!(
        section.contains(
            "- Engineering: Backend Engineer (lead), Designer (id `engineering` for tool calls)\n"
        ),
        "{section}"
    );
    assert!(
        !section.contains("- Content:"),
        "`designer` does not sit on `content`, so this block must not describe \
         it as somewhere they sit: {section}"
    );
    assert!(section.contains("You sit on (desk: members):"), "{section}");
    assert!(
        section.contains("- Writer (id `writer` for tool calls)"),
        "the roster of people stays whole — `writer` is still someone to ask, \
         even though their desk is not one `designer` sits on: {section}"
    );
}

#[test]
fn an_unrestricted_reach_is_stated_once_at_the_top_and_not_as_a_list() {
    // `designer` declares no `delegates_to`, so it may reach everyone.
    let section = team_section(&record(TEAM), "pm");
    assert!(
        section.contains("Every teammate below is a real agent you can hand work to"),
        "{section}"
    );
    assert!(!section.contains("You may hand work to:"), "{section}");
    assert!(!section.contains("does not let you hand work"), "{section}");
}

#[test]
fn a_narrowed_reach_names_exactly_who_the_tool_would_accept() {
    // `backend` may reach the engineering desk only: its desk-mate `designer`,
    // and nobody on the content desk or the orchestrator.
    let section = team_section(&record(TEAM), "backend");
    assert!(
        section.contains("\nYou may hand work to: Designer (`designer`)."),
        "{section}"
    );
    let reach = teammate_targets(&record(TEAM), "backend", &["engineering".to_string()]);
    assert_eq!(reach, vec!["designer".to_string()]);
}

#[test]
fn the_section_names_the_tools_by_their_real_names() {
    // The orchestrator delegates by design and is told the verb by name.
    let orchestrator = team_section(&record(TEAM), "pm");
    assert!(
        orchestrator.contains(&format!("`{DELEGATE_TO_TEAMMATE_TOOL}`")),
        "{orchestrator}"
    );
    // A member is not offered it on any turn, so its section must not name
    // it: the prompt is composed once and has to be true on every turn.
    let member = team_section(&record(TEAM), "writer");
    assert!(
        !member.contains(&format!("`{DELEGATE_TO_TEAMMATE_TOOL}`")),
        "a member is promised a verb it does not have: {member}"
    );
    assert!(
        member.contains("`spawn_task`"),
        "and is told what it does have instead: {member}"
    );
}

/// The brief no longer advertises `delegate_to_desk`.
///
/// It used to name both tools, which put a desk-wide hand-off in front of every
/// seat on every turn — and a hand-off takes ONE turn from whoever leads that
/// desk, quietly skipping the deliberation the desk exists for. A crossing
/// (`@#desk`) is the move that asks a desk a question, and it is advertised by
/// the episode prompt to the seats a policy actually permits it to. Naming the
/// tool here reached further than that policy and said nothing about its cost.
#[test]
fn the_section_does_not_advertise_the_desk_hand_off() {
    let section = team_section(&record(TEAM), "writer");
    assert!(
        !section.contains("delegate_to_desk"),
        "the brief must not put a desk-wide hand-off on every seat's turn: {section}"
    );
}

#[test]
fn a_company_without_desks_lists_no_desk_block() {
    let section = team_section(
        &record(
            r#"
[company]
name = "Acme"

[[agent]]
id = "a"
role = "A"

[[agent]]
id = "b"
role = "B"
"#,
        ),
        "a",
    );
    assert!(
        section.contains("- B (id `b` for tool calls)\n"),
        "{section}"
    );
    assert!(!section.contains("Desks ("), "{section}");
    assert!(!section.contains("You sit on"), "{section}");
}

#[test]
fn an_operator_added_teammate_is_listed_by_name_and_role() {
    let mut record = record(TEAM);
    record
        .overlay_agents
        .push(crate::ports::types::OverlayAgent {
            id: "sam".to_string(),
            name: "Sam".to_string(),
            role: "Copywriter".to_string(),
            description: Some("Write the words.".to_string()),
            provider: None,
            tools: None,
            skills: None,
            model: None,
            harness: None,
        });
    let section = team_section(&record, "designer");
    assert!(section.contains("one of 5 teammates"), "{section}");
    assert!(
        section.contains("- Sam, Copywriter: Write the words. (id `sam` for tool calls)\n"),
        "{section}"
    );
}

#[test]
fn a_manifest_teammates_operator_rename_is_the_name_other_agents_are_given() {
    let mut record = record(TEAM);
    record
        .overlay_agent_edits
        .push(crate::ports::types::AgentOverride {
            agent_id: "backend".to_string(),
            name: Some("Johnny".to_string()),
            ..Default::default()
        });

    let section = team_section(&record, "writer");
    assert!(
        section.contains(
            "- Johnny, Backend Engineer: Build the services. (id `backend` for tool calls)"
        ),
        "the live overlay name and canonical id must both reach the teammate prompt: {section}"
    );
    // Still the point of the assertion — the prompt has to say how to reach
    // Johnny — but a member's way is not `delegate_to_teammate`, which it is
    // not offered on any turn.
    assert!(
        section.contains("ask them directly") && section.contains("`spawn_task`"),
        "the same prompt must say how to contact Johnny: {section}"
    );
}

fn sections(record: &CompanyRecord) -> Vec<(String, String)> {
    ["pm", "backend", "designer", "writer"]
        .into_iter()
        .flat_map(|id| {
            [
                (format!("{id} (roster)"), team_section(record, id)),
                (format!("{id} (seat)"), seat_team_section(record, id)),
            ]
        })
        .collect()
}

#[test]
fn no_variant_teaches_the_id_as_the_name() {
    for (who, section) in sections(&record(TEAM)) {
        assert!(
            !section.contains("naming the id exactly"),
            "{who}: {section}"
        );
        assert!(
            section.contains("by name when you write"),
            "{who}: {section}"
        );
    }
}

#[test]
fn every_row_leads_with_a_name_and_keeps_the_id_for_tool_calls() {
    for (who, section) in sections(&record(TEAM)) {
        let rows: Vec<&str> = section
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        assert!(!rows.is_empty(), "{who}: {section}");
        for row in rows {
            assert!(
                !row.starts_with("- `"),
                "{who}: a row leads with an id: {row}"
            );
            assert!(row.ends_with("for tool calls)"), "{who}: {row}");
        }
    }
}

#[test]
fn a_seat_is_not_told_about_hand_off_tools_it_does_not_have() {
    let record = record(TEAM);
    for id in ["pm", "backend", "designer", "writer"] {
        let section = seat_team_section(&record, id);
        assert!(section.starts_with("\n\n## Your team"), "{section}");
        for phrase in ["delegate_to_teammate", "hand work", "You may hand work to"] {
            assert!(!section.contains(phrase), "{id}: `{phrase}` in {section}");
        }
    }
    assert!(
        seat_team_section(&record, "designer")
            .contains("- Engineering: Backend Engineer (lead), Designer (id `engineering`"),
        "a seat still knows its desks"
    );
}
