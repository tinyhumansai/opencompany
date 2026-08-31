//! Assembling one agent's system prompt from its manifest definition.
//!
//! An agent's prompt is built from four kinds of material, and the order they
//! appear in is a decision rather than an accident:
//!
//! 1. the generated **persona** — who this teammate is, at which company;
//! 2. its **inline prompt**, the operator's working instruction for the role;
//! 3. its **bundle documents** ([`prompt_files`](crate::company::Agent::prompt_files)),
//!    version controlled beside the agent definition;
//! 4. its **routed documents** ([`context`](crate::company::Agent::context)),
//!    live operator-owned workspace notes.
//!
//! Static material comes first and volatile material last, because the prompt
//! prefix is what a provider cache can reuse across turns: a workspace note the
//! operator edits between two turns invalidates everything after it, so it is
//! placed where "everything after it" is nothing.
//!
//! This module is deliberately **always compiled** and free of any runtime
//! dependency. The harness that consumes it (`crate::harness::build`) is behind
//! the `openhuman` feature, but the composition and clamping rules are ordinary
//! text manipulation with real edge cases — a budget cut that splits a
//! codepoint, a document that is only whitespace — and they are worth testing
//! in the default build rather than only where the agent runtime links.

use crate::company::{Agent, PROMPT_FILE_BUDGET_CHARS};

/// The marker appended to a section that the budget cut short.
///
/// Visible rather than silent, and it names the budget: an agent whose briefing
/// was truncated should be able to say so, and an operator reading the prompt
/// should not have to guess whether the document simply ended there. A silent
/// cut is indistinguishable from a document that was written short.
pub const TRUNCATION_MARKER: &str = "\n\n[… truncated to fit the prompt budget]";

/// The heading introducing the routed-document section.
const CONTEXT_HEADING: &str = "\n\n## Working documents\n\nYou are told to reason from the documents below. They are the company's current \
working state, not background reading.\n";

/// The heading introducing the bundle-document section.
const BUNDLE_HEADING: &str = "\n\n## Your brief\n";

/// The persona sentence for a company agent, plus the operator's inline prompt.
///
/// Frames the agent as its manifest role at the company, in the first person.
/// This is what makes the agent answer *as* the CEO of Acme rather than falling
/// back to the runtime's own assistant identity.
///
/// An agent carrying a [`name`](Agent::name) — an operator-added teammate — is
/// framed as that name *and* the role, because the console addresses it by name
/// everywhere (DM header, subtitle, composer) and an agent told only its role
/// contradicts the interface it is speaking through (issue #1105). The name is
/// stated as an address, not a character: a teammate should answer to it
/// without inventing a persona around it.
///
/// The `instructions` — the agent's **effective** persona text, resolved by the
/// caller through [`CompanyRecord::effective_instructions`](crate::ports::types::CompanyRecord::effective_instructions)
/// (an operator override when one is set, else the manifest agent's `prompt`,
/// else `None`) — are **appended** to that framing rather than replacing it: an
/// operator writing instructions is stating how the role should work, not
/// disclaiming which role it is, and text that replaced the framing would
/// silently cost the agent its identity (issue #1530).
///
/// Taken as a parameter rather than read off `agent.prompt` so the single
/// injection point serves both agent kinds uniformly: a manifest agent whose
/// persona an operator edited from the console, and an overlay teammate that has
/// no manifest `prompt` at all, both arrive here as the same resolved
/// `Option<&str>`. A blank or whitespace-only value adds nothing.
pub fn persona_prompt(company_name: &str, agent: &Agent, instructions: Option<&str>) -> String {
    // Blank is absent, as it is for `description` and `prompt` below. A name
    // that just restates the role is dropped too, or the framing reads "You are
    // Content Writer, the Content Writer at Acme."
    let named = agent
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case(agent.role.trim()));
    let mut prompt = match named {
        Some(name) => format!(
            "You are {name}, the {role} at {company}. Speak in the first person as this role. \
             Teammates and the operator address you as {name}; it is how you are called here, \
             not a separate character to play.",
            name = name,
            role = agent.role,
            company = company_name,
        ),
        None => format!(
            "You are the {role} at {company}. Speak in the first person as this role.",
            role = agent.role,
            company = company_name,
        ),
    };
    if let Some(description) = agent.description.as_deref() {
        let description = description.trim();
        if !description.is_empty() {
            prompt.push(' ');
            prompt.push_str(description);
        }
    }
    if let Some(custom) = instructions {
        let custom = custom.trim();
        if !custom.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(custom);
        }
    }
    prompt
}

/// The bundle-document section for an agent, or `""` when it has none.
///
/// Reads the bodies the manifest loader already resolved
/// ([`prompt_files_resolved`](crate::company::Agent::prompt_files_resolved)), so
/// this does no I/O and can run on every roster rebuild.
pub fn bundle_section(agent: &Agent) -> String {
    let documents: Vec<(&str, &str)> = agent
        .prompt_files_resolved
        .iter()
        .map(|(path, body)| (path.as_str(), body.as_str()))
        .collect();
    document_section(BUNDLE_HEADING, &documents)
}

/// The routed-document section for `documents`, or `""` when there are none.
///
/// Takes the resolved `(name, body)` pairs rather than reading them, because the
/// workspace store is async and the agent build is not — the caller resolves
/// them ahead of time, the same way it resolves skill deltas.
pub fn context_section(documents: &[(String, String)]) -> String {
    let documents: Vec<(&str, &str)> = documents
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_str()))
        .collect();
    document_section(CONTEXT_HEADING, &documents)
}

/// Renders a titled list of documents under `heading`, clamped as a whole.
///
/// The budget applies to the **section**, not to each document: a role routed
/// five documents and a role routed one are spending from the same prompt, and a
/// per-document budget would let the first quietly cost five times the second.
///
/// Empty and whitespace-only bodies are dropped rather than rendered as a bare
/// heading with nothing under it — an empty section reads to the model as a
/// document that exists and says nothing, which is worse than its absence.
fn document_section(heading: &str, documents: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, content) in documents {
        if content.trim().is_empty() {
            continue;
        }
        body.push_str("\n### ");
        body.push_str(name);
        body.push('\n');
        body.push_str(content.trim_end());
        body.push('\n');
    }
    if body.is_empty() {
        return String::new();
    }
    format!("{heading}{}", clamp(&body, PROMPT_FILE_BUDGET_CHARS))
}

/// Clamps `text` to `budget` codepoints, keeping the leading portion.
///
/// Three properties, each of which prevents a specific failure:
///
/// * It cuts on a **character** boundary, never a byte one, so a multi-byte
///   codepoint at the limit is dropped whole rather than sliced into invalid
///   UTF-8.
/// * It keeps the **leading** portion, because these documents are written
///   most-important-first — a brief leads with what is established, an operator
///   brief leads with the instruction.
/// * It marks the cut, so truncation is legible instead of looking like a short
///   document.
///
/// Clamping happens here, where the text is spent, rather than at load: refusing
/// or truncating the read would cost the company the whole document, while
/// clamping at assembly costs only its tail.
pub fn clamp(text: &str, budget: usize) -> String {
    // `chars().count()` walks the string, so only pay for it when the cheap byte
    // length says a cut is even possible (bytes >= chars, always).
    if text.len() <= budget {
        return text.to_string();
    }
    let mut kept: String = text.chars().take(budget).collect();
    if kept.chars().count() == text.chars().count() {
        return kept;
    }
    kept.push_str(TRUNCATION_MARKER);
    kept
}

/// Caps operator-authored persona instructions to the prompt budget.
///
/// `instructions` written through the team/agent edit surfaces are injected,
/// verbatim, into every turn of the teammate's system prompt via
/// [`persona_prompt`]. Unlike `bundle_section`/`context_section`, that injection
/// point applied no budget of its own — the persona grew without ceiling as an
/// operator pasted more text, inflating every dispatch. Capping here, at the
/// write boundary (mirroring [`crate::ports::tasks::cap_discussion`]), keeps the
/// stored override bounded without refusing an operator's edit; the leading,
/// most-important portion is preserved and a cut is marked.
pub fn cap_persona_instructions(text: &str) -> String {
    clamp(text, PROMPT_FILE_BUDGET_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(role: &str) -> Agent {
        Agent {
            global: false,
            id: "a".into(),
            role: role.into(),
            name: None,
            description: None,
            tier: None,
            harness: None,
            tools: None,
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
}
