//! Issue #885: the audit's classification rule.
//!
//! The rule is "an `agent_id` naming no roster teammate", not
//! `== "operator"`, so these pin both the shape actually observed and the
//! generalisation — the same writer bug on another channel produces a
//! different wrong string and still has to be counted.

use super::tests_reactions::at;
use super::*;

fn reply(seq: u64, agent_id: &str) -> StoredEvent {
    at(
        seq,
        CompanyEvent::AgentReply {
            audience: Vec::new(),
            mentions: Vec::new(),
            mention_depth: 0,
            chat_id: "engineering".to_string(),
            agent_id: agent_id.to_string(),
            text: "…".to_string(),
            steps: Vec::new(),
            task_id: None,
            outputs: Vec::new(),
            parent: None,
            episode: None,
        },
    )
}

/// The roster for these: two real teammates and nothing else.
///
/// Deliberately *excludes* the confined copilot, because that is the
/// point of `is_known_author` — the copilot is a real author that no
/// roster will ever resolve.
fn on_roster(agent_id: &str) -> bool {
    matches!(agent_id, "engineer" | "product_manager")
}

/// A record whose roster is exactly `on_roster`'s two teammates.
///
/// Built so the tests below call the **real** `is_known_author` rather
/// than a local restatement of it. The first version of these tests
/// re-implemented the predicate in the test module, which meant
/// reverting the production function changed nothing and the tests
/// passed either way — proving only that the test agreed with itself.
fn record() -> CompanyRecord {
    let src = "[company]\nname = \"Acme\"\n\n[policy]\nmode = \"full\"\n\
               \n[[agent]]\nid = \"engineer\"\nrole = \"Worker\"\ntier = \"orchestrator\"\n\
               \n[[agent]]\nid = \"product_manager\"\nrole = \"Worker\"\ntier = \"orchestrator\"\n";
    let manifest: crate::company::CompanyManifest = toml::from_str(src).expect("manifest parses");
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: crate::ports::types::CompanyId::new("acme"),
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

/// Issue #966. The runtime speaking for itself is a *correct* row, not
/// damage. Counting it would inflate the blast-radius figure on a company
/// doing nothing wrong, and would caption a legitimate system message as
/// something nobody can attribute.
#[test]
fn a_host_authored_notice_is_a_known_author_not_an_affected_row() {
    let record = record();
    let mut audit = AttributionAudit::default();
    audit.fold(
        &[reply(1, crate::ports::SYSTEM_AUTHOR), reply(2, "engineer")],
        |agent_id| is_known_author(agent_id, &record),
    );
    assert_eq!(audit.replies, 2);
    assert_eq!(audit.affected, 0);
}

/// Issue #966. The console reaches the centred system pill by comparing
/// the projected author against a literal `"system"`
/// (`frontend/src/lib/chat.ts`), and `MessageView` projects an
/// `AgentReply`'s `agent_id` straight into that field. So the *value* is
/// the contract with the console, not merely the constant's identity.
///
/// Redefining `SYSTEM_AUTHOR` to anything else keeps every other test
/// here green and silently returns these three notices to rendering as
/// company bubbles — the exact appearance this change exists to end.
/// Two copies of one literal is the same coupling
/// `dispatch_marker_text` already carries with that file, and it is
/// deliberate for the same reason.
#[test]
fn the_notice_author_is_the_literal_the_console_keys_on() {
    assert_eq!(
        crate::ports::SYSTEM_AUTHOR,
        "system",
        "frontend/src/lib/chat.ts renders `author === \"system\"` as the centred pill"
    );
}

/// The whole point of the reserved id: a notice and a damaged reply used
/// to be the same bytes. This pins that they are now different ones, so
/// the distinction a marker would rely on actually exists in the data.
#[test]
fn a_notice_and_an_overwritten_reply_are_no_longer_the_same_author() {
    let record = record();
    assert_ne!(
        crate::ports::SYSTEM_AUTHOR,
        "operator",
        "a notice must not share the author a destination-overwrite produces"
    );
    assert!(is_known_author(crate::ports::SYSTEM_AUTHOR, &record));
    assert!(!is_known_author("operator", &record));
}

/// Issue #966. A copilot turn genuinely authored its reply, so the id it
/// stores is a truthful author — not a destination that leaked into the
/// field. Counting it would swap one wrong answer for a permanent false
/// positive that climbs on a company doing nothing wrong.
#[test]
fn the_confined_copilot_is_a_known_author_not_an_affected_row() {
    let record = record();
    let mut audit = AttributionAudit::default();
    audit.fold(
        &[
            reply(1, crate::ports::CONFINED_AGENT_ID),
            reply(2, "engineer"),
        ],
        |agent_id| is_known_author(agent_id, &record),
    );
    assert_eq!(audit.replies, 2);
    assert_eq!(audit.affected, 0);
}

/// …and it is still not on the roster, which is what makes the widening
/// necessary rather than incidental. If `resolve_roster_agent_id` ever
/// started answering for it, this says so before the extra arm quietly
/// becomes dead code.
#[test]
fn the_confined_copilot_is_not_reachable_through_the_roster_alone() {
    let record = record();
    assert!(
        record
            .resolve_roster_agent_id(crate::ports::CONFINED_AGENT_ID)
            .is_none(),
        "the confined id resolved on the roster; `is_known_author`'s extra arm is now \
         unnecessary and this test should be deleted deliberately, not left passing"
    );
    assert!(is_known_author(crate::ports::CONFINED_AGENT_ID, &record));
    assert!(is_known_author("engineer", &record));
    assert!(!is_known_author("operator", &record));
}

/// A delivered workflow report is journaled under
/// [`crate::runtime::WORKFLOW_REPLY_AUTHOR`] on purpose — it is the
/// workflow speaking, not a teammate's own reply. Counting it would
/// flag every delivered report on a company with no roster match for
/// "workflow" as damaged, and — worse — a teammate who *did* mint that
/// id would have every report silently misattributed to them by
/// `senderOf` before this reservation existed.
#[test]
fn a_workflow_report_is_a_known_author_not_an_affected_row() {
    let record = record();
    assert!(
        record
            .resolve_roster_agent_id(crate::runtime::WORKFLOW_REPLY_AUTHOR)
            .is_none(),
        "workflow reports resolve through the extra arm, not the roster"
    );
    let mut audit = AttributionAudit::default();
    audit.fold(
        &[
            reply(1, crate::runtime::WORKFLOW_REPLY_AUTHOR),
            reply(2, "engineer"),
        ],
        |agent_id| is_known_author(agent_id, &record),
    );
    assert_eq!(audit.replies, 2);
    assert_eq!(audit.affected, 0);
}

/// Issue #1781 review, Codex P2: an owner-fallback report is journaled
/// under [`crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR`] on purpose —
/// same reservation as `WORKFLOW_REPLY_AUTHOR`, one arm narrower — so it
/// must not inflate the audit either. Before this arm existed, every
/// legitimate no-mailbox fallback counted as damaged attribution.
#[test]
fn an_owner_fallback_report_is_a_known_author_not_an_affected_row() {
    let record = record();
    assert!(
        record
            .resolve_roster_agent_id(crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR)
            .is_none(),
        "owner-fallback reports resolve through the extra arm, not the roster"
    );
    let mut audit = AttributionAudit::default();
    audit.fold(
        &[
            reply(1, crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR),
            reply(2, "engineer"),
        ],
        |agent_id| is_known_author(agent_id, &record),
    );
    assert_eq!(audit.replies, 2);
    assert_eq!(audit.affected, 0);
}

/// Review on PR #1781 (Codex P2): a company that named an overlay
/// teammate "Workflow" before this reservation existed would have
/// minted the bare id `workflow` — the id `WORKFLOW_REPLY_AUTHOR`
/// itself used to be, until it was reshaped to the unmintable,
/// hyphenated `workflow-report`. That persisted teammate is not
/// migrated or renamed by this fix — there is nothing to migrate: the
/// pseudo-author a workflow report is now journaled under is a
/// **different, disjoint id** from the one that teammate holds, so
/// the collision this reservation exists to prevent cannot occur for
/// it, retroactively as well as going forward. Proven here rather than
/// asserted, since the whole point is that the two ids must never
/// again be able to resolve to the same author.
#[test]
fn a_persisted_teammate_named_workflow_does_not_shadow_the_reply_author() {
    let mut record = record();
    record
        .overlay_agents
        .push(crate::ports::types::OverlayAgent {
            provider: None,
            id: "workflow".to_string(),
            name: "Workflow".to_string(),
            role: "Worker".to_string(),
            description: None,
            tools: Some(Vec::new()),
            skills: None,
            model: None,
            harness: None,
        });

    assert_ne!(
        "workflow",
        crate::runtime::WORKFLOW_REPLY_AUTHOR,
        "the two ids must be disjoint for the rest of this test to mean anything"
    );
    assert!(
        record.resolve_roster_agent_id("workflow").is_some(),
        "the pre-existing teammate is still on the roster, unmigrated"
    );
    assert!(
        record
            .resolve_roster_agent_id(crate::runtime::WORKFLOW_REPLY_AUTHOR)
            .is_none(),
        "the reply-author id does not resolve to that (or any) teammate"
    );

    let mut audit = AttributionAudit::default();
    audit.fold(
        &[
            // The teammate's own reply — attributed to them, as before.
            reply(1, "workflow"),
            // A new workflow report, delivered after this fix ships —
            // journaled under the disjoint id, not theirs.
            reply(2, crate::runtime::WORKFLOW_REPLY_AUTHOR),
        ],
        |agent_id| is_known_author(agent_id, &record),
    );
    assert_eq!(audit.replies, 2);
    assert_eq!(
        audit.affected, 0,
        "both rows resolve, to two different authors"
    );
}

#[test]
fn a_reply_authored_by_a_real_teammate_is_not_counted() {
    let mut audit = AttributionAudit::default();
    audit.fold(
        &[reply(1, "engineer"), reply(2, "product_manager")],
        on_roster,
    );
    assert_eq!(audit.replies, 2);
    assert_eq!(audit.affected, 0);
    assert!(audit.by_agent_id.is_empty());
}

/// The observed #885 shape: the operator channel copied into the author.
#[test]
fn a_reply_authored_by_the_operator_channel_is_counted() {
    let mut audit = AttributionAudit::default();
    audit.fold(
        &[
            reply(1, "operator"),
            reply(2, "engineer"),
            reply(3, "operator"),
        ],
        on_roster,
    );
    assert_eq!(audit.replies, 3);
    assert_eq!(audit.affected, 2);
    assert_eq!(audit.by_agent_id.get("operator"), Some(&2));
}

/// The generalisation. A Telegram chat id or a desk slug in the author
/// field is the same defect, and a rule keyed on the literal
/// `"operator"` would report a clean company.
#[test]
fn any_non_roster_author_is_counted_not_just_the_operator_channel() {
    let mut audit = AttributionAudit::default();
    audit.fold(
        &[reply(1, "operator"), reply(2, "-100123456789")],
        on_roster,
    );
    assert_eq!(audit.affected, 2);
    assert_eq!(audit.by_agent_id.get("-100123456789"), Some(&1));
}

/// Only replies. An operator's own message is not an `AgentReply` and
/// has no `agent_id` to be wrong, so counting it would inflate the
/// blast radius of a data-integrity bug — the one number that has to be
/// trustworthy here.
#[test]
fn a_non_reply_event_is_neither_scanned_nor_counted() {
    let mut audit = AttributionAudit::default();
    audit.fold(
        &[
            at(
                1,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    text: "hello".to_string(),
                    by: None,
                    chat: None,
                    parent: None,
                    deliverable: None,
                    attachments: Vec::new(),
                },
            ),
            reply(2, "operator"),
        ],
        on_roster,
    );
    assert_eq!(audit.replies, 1);
    assert_eq!(audit.affected, 1);
}
