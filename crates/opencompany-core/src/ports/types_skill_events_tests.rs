use super::*;
use crate::ports::skills_state::SkillTier;

/// The document body an operator installed. Every assertion below is about
/// this string, or a fragment of it, being absent from the journal row.
const BODY: &str = "---\nname: Web research\ndescription: research a topic\n---\n\
                    1. Open the browser tool.\n2. Summarize what you find.";

fn installed() -> CompanyEvent {
    CompanyEvent::SkillChanged {
        slug: "web-research".to_string(),
        change: SkillChange::Installed,
        tier: SkillTier::Registry,
        digest: Some(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".to_string(),
        ),
        by: Some(Actor {
            kind: ActorKind::Operator,
            id: "ops@example.com".to_string(),
        }),
    }
}

/// The wire shape, pinned the same way `WorkflowUpdated`'s is: `kind` plus the
/// structural fields, with an absent `by` omitted entirely.
#[test]
fn skill_changed_pins_its_wire_shape() {
    assert_eq!(
        serde_json::to_string(&installed()).expect("serialize"),
        r#"{"kind":"SkillChanged","slug":"web-research","change":"installed","tier":"registry","digest":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","by":{"kind":"operator","id":"ops@example.com"}}"#
    );

    let unattributed = CompanyEvent::SkillChanged {
        slug: "notes".to_string(),
        change: SkillChange::Removed,
        tier: SkillTier::Custom,
        digest: None,
        by: None,
    };
    assert_eq!(
        serde_json::to_string(&unattributed).expect("serialize"),
        r#"{"kind":"SkillChanged","slug":"notes","change":"removed","tier":"custom"}"#
    );
}

/// The rule the variant exists to keep: the journal records *that* a skill
/// changed and *which* document it changed to, never the document. A skill body
/// is instructions an agent reads, and the journal reaches readers with no
/// business holding them.
///
/// Asserted as an absence, not as a set of present fields: a later field that
/// carried the body would still satisfy a test that only checked `digest` and
/// `slug` are there.
#[test]
fn skill_changed_carries_no_document_body() {
    let line = serde_json::to_string(&installed()).expect("serialize");

    assert!(!line.contains(BODY), "the whole document: {line}");
    for fragment in [
        "Open the browser tool",
        "Summarize what you find",
        "research a topic",
    ] {
        assert!(!line.contains(fragment), "`{fragment}` leaked into {line}");
    }

    let value: serde_json::Value = serde_json::from_str(&line).expect("parse");
    let fields: Vec<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        ["kind", "slug", "change", "tier", "digest", "by"],
        "a new field on this variant is a new thing the journal carries"
    );
}

/// The digest is what makes the no-body rule affordable: it names which
/// document without reproducing it, and it is the same value the install
/// recorded, so an audit row and a pin can be matched.
#[test]
fn the_digest_on_the_row_is_the_one_the_install_recorded() {
    let pinned = crate::company::skill_provenance::skill_digest(BODY);
    let event = CompanyEvent::SkillChanged {
        slug: "web-research".to_string(),
        change: SkillChange::Installed,
        tier: SkillTier::Registry,
        digest: Some(pinned.clone()),
        by: None,
    };

    let line = serde_json::to_string(&event).expect("serialize");
    assert!(line.contains(&pinned));
    assert!(!line.contains("Open the browser tool"));
}

/// An audit row must survive as long as the question it answers. The store
/// holds one row per slug and rewrites it in place, so nothing but the journal
/// records that an earlier install ever existed.
#[test]
fn skill_changed_is_permanent() {
    assert_eq!(
        installed().retention_class(),
        crate::ports::events::RetentionClass::Permanent
    );
    assert_eq!(installed().kind(), "SkillChanged");
}
