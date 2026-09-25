use super::*;

use crate::ports::types::AgentOverride;

fn overlay_agent(id: &str, skills: Option<Vec<String>>) -> OverlayAgent {
    OverlayAgent {
        provider: None,
        id: id.into(),
        name: "Jamie".into(),
        role: "Growth Lead".into(),
        description: None,
        tools: None,
        skills,
        model: None,
        harness: None,
    }
}

/// An override that carries a scope and nothing else. The shape an admin
/// produces by scoping one teammate on the console and changing nothing about
/// it, which is the case the fingerprint has to catch.
fn skills_only_override(agent_id: &str, skills: Option<Vec<String>>) -> AgentOverride {
    AgentOverride {
        agent_id: agent_id.into(),
        skills: Some(skills),
        ..Default::default()
    }
}

/// `is_avatar_only` filters a row out of the edits loop before anything is
/// hashed, so a scope-carrying row has to fail it. Hashing the field is
/// necessary and not sufficient: if this returns true the fingerprint fix is
/// inert for exactly the edit it exists to catch.
#[test]
fn a_scope_only_override_is_not_avatar_only() {
    assert!(!is_avatar_only(&skills_only_override(
        "growth",
        Some(vec!["brand-voice".to_string()])
    )));
    // Every representable scope state is a real edit, including the reset and
    // the deny-all — the same treatment an emptied tool list gets.
    assert!(!is_avatar_only(&skills_only_override("growth", None)));
    assert!(!is_avatar_only(&skills_only_override(
        "growth",
        Some(Vec::new())
    )));
    // The row it is actually for still filters.
    assert!(is_avatar_only(&AgentOverride {
        agent_id: "growth".into(),
        avatar: Some("tiny:fox".into()),
        ..Default::default()
    }));
}

/// A scope set through an override moves the roster fingerprint. Without this
/// every other axis is stable on a scope-only change, the cached roster is
/// reused, and the scope is silently ignored until the process restarts.
#[test]
fn an_override_scope_moves_the_overlay_fingerprint() {
    let base = overlay_fingerprint(&[], &[skills_only_override("growth", None)], &[]);
    let narrowed = overlay_fingerprint(
        &[],
        &[skills_only_override(
            "growth",
            Some(vec!["brand-voice".to_string()]),
        )],
        &[],
    );
    let emptied = overlay_fingerprint(
        &[],
        &[skills_only_override("growth", Some(Vec::new()))],
        &[],
    );

    assert_ne!(base, narrowed, "narrowing a scope must move it");
    assert_ne!(narrowed, emptied, "emptying a scope must move it");
    assert_ne!(base, emptied, "a deny-all is not a reset");
}

/// The same for a console-created teammate, which is hashed in the other loop.
/// Both loops or neither: an `OverlayAgent` and an `AgentOverride` reach the
/// roster by different paths.
#[test]
fn an_overlay_agent_scope_moves_the_overlay_fingerprint() {
    let inherit = overlay_fingerprint(&[overlay_agent("growth", None)], &[], &[]);
    let narrowed = overlay_fingerprint(
        &[overlay_agent(
            "growth",
            Some(vec!["brand-voice".to_string()]),
        )],
        &[],
        &[],
    );
    let emptied = overlay_fingerprint(&[overlay_agent("growth", Some(Vec::new()))], &[], &[]);

    assert_ne!(inherit, narrowed);
    assert_ne!(narrowed, emptied);
    assert_ne!(inherit, emptied);
}

/// Two different scopes must not collide, and the list is hashed in order, so a
/// reorder is a real change rather than a silent no-op.
#[test]
fn distinct_scopes_hash_distinctly() {
    let a = overlay_fingerprint(
        &[overlay_agent(
            "growth",
            Some(vec!["brand-voice".to_string(), "weekly-report".to_string()]),
        )],
        &[],
        &[],
    );
    let b = overlay_fingerprint(
        &[overlay_agent(
            "growth",
            Some(vec!["weekly-report".to_string(), "brand-voice".to_string()]),
        )],
        &[],
        &[],
    );
    let c = overlay_fingerprint(
        &[overlay_agent(
            "growth",
            Some(vec!["brand-voice".to_string()]),
        )],
        &[],
        &[],
    );

    assert_ne!(a, b);
    assert_ne!(a, c);
}
