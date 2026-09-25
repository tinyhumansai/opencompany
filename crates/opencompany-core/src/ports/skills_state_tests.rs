//! Wire-compatibility tests for [`SkillState`].
//!
//! Every backing store — sqlite, mongodb and the fs bundle — persists this
//! struct as `serde_json` of the whole value, so a field added to it is a
//! change to an on-disk format that already has rows in it.

use super::{SkillSource, SkillState};

/// A row written before `updated_at_millis` existed still reads back.
///
/// The three stores all `serde_json::from_str` the stored blob, so without the
/// `#[serde(default)]` this asserts, every company with an existing skill delta
/// would fail to list its skills at all after the upgrade.
#[test]
fn a_row_stored_without_a_timestamp_reads_back_with_none() {
    let stored = r#"{"slug":"web-research","enabled":true,"source":"registry"}"#;

    let state: SkillState = serde_json::from_str(stored).expect("an old row still deserializes");

    assert_eq!(state.slug, "web-research");
    assert_eq!(state.source, SkillSource::Registry);
    assert_eq!(
        state.updated_at_millis, None,
        "an absent stamp is unknown, never zero — the console must not date it to 1970"
    );
}

/// An absent stamp is omitted rather than written as `null`, so re-storing an
/// untouched old row does not grow it.
#[test]
fn an_absent_timestamp_is_left_out_of_the_serialized_row() {
    let state = SkillState {
        slug: "web-research".to_string(),
        enabled: true,
        source: SkillSource::Registry,
        custom_doc: None,
        updated_at_millis: None,
    };

    let json = serde_json::to_string(&state).expect("serializes");

    assert!(
        !json.contains("updatedAtMillis"),
        "an unknown stamp adds no key: {json}"
    );
}

/// The stamp survives a full round trip under the camelCase wire name every
/// store uses.
#[test]
fn a_stamped_row_round_trips() {
    let state = SkillState {
        slug: "my-skill".to_string(),
        enabled: true,
        source: SkillSource::Custom,
        custom_doc: Some("---\nname: Mine\ndescription: Does a thing\n---\nbody\n".to_string()),
        updated_at_millis: Some(1_759_000_000_000),
    };

    let json = serde_json::to_string(&state).expect("serializes");
    assert!(json.contains("\"updatedAtMillis\":1759000000000"), "{json}");

    let back: SkillState = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(back, state);
}
