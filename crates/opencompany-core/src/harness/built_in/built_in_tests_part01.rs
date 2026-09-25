//! `built_in`'s own inline tests, part 1 of 10. Split out of the
//! single inline `mod tests` block because it exceeded the 750-line file
//! limit; grouped in the original file's order, not by topic (the block
//! covered dozens of unrelated issues with no existing topical boundaries).
//! Shared setup lives in [`super::built_in_test_fixtures`] and
//! [`super::built_in_test_fixtures_2`].

use super::built_in_test_fixtures::*;
use super::*;
use crate::ports::types::ContextChunk;

#[test]
fn dispatched_cards_are_isolated_from_an_agents_other_conversations() {
    // A turn that names no conversation runs on its own session; one that
    // does resumes the agent's conversation session; one that brings its own
    // context is isolated whatever it names (plan hive-desks, Phase 2).
    assert!(CompanyAgent::isolated_session(None, true));
    assert!(!CompanyAgent::isolated_session(Some("general"), true));
    assert!(CompanyAgent::isolated_session(Some("general"), false));
    assert!(CompanyAgent::isolated_session(None, false));
}

/// The fingerprint moves when the tier moves (issue #562).
///
/// This is the assertion that keeps the feature from being a no-op.
/// `ApprovalPolicy` is built once per roster build, and `ensure` reuses the
/// cached roster unless a fingerprint changed — so if this returned a
/// constant, a console tier change would persist, return `204`, render as
/// applied, and be **silently ignored until the process restarted**. Every
/// other test in this change would still pass.
#[test]
fn the_policy_fingerprint_moves_when_the_tier_does() {
    let supervised = effective_policy_fingerprint(&fp_policy("supervised", &[], None, None));
    let full = effective_policy_fingerprint(&fp_policy("full", &[], None, None));

    assert_ne!(
        supervised, full,
        "a tier change must move the fingerprint or the roster is never rebuilt"
    );
}

/// An always-ask edit moves it too, including clearing the list.
///
/// `always_approve` wins over every tier including `full`, so an edit that
/// did not rebuild would leave the gate enforcing a list the operator had
/// already changed — the failure mode is stricter *or* looser than what the
/// console shows, depending on the edit.
#[test]
fn the_policy_fingerprint_moves_when_the_always_ask_list_does() {
    let empty = effective_policy_fingerprint(&fp_policy("auto", &[], None, None));
    let one = effective_policy_fingerprint(&fp_policy("auto", &["payment.send"], None, None));
    let two = effective_policy_fingerprint(&fp_policy(
        "auto",
        &["payment.send", "filing.submit"],
        None,
        None,
    ));

    assert_ne!(empty, one, "adding an entry must move the fingerprint");
    assert_ne!(one, two, "a second entry must move it again");

    // Order is part of the value: the list is the operator's own, not an
    // accumulation of independent rows, so a reorder is a real edit.
    let reordered = effective_policy_fingerprint(&fp_policy(
        "auto",
        &["filing.submit", "payment.send"],
        None,
        None,
    ));
    assert_ne!(two, reordered);

    // Length is folded in, so concatenation cannot collide.
    let split = effective_policy_fingerprint(&fp_policy("auto", &["a", "b"], None, None));
    let joined = effective_policy_fingerprint(&fp_policy("auto", &["ab"], None, None));
    assert_ne!(split, joined);
}

/// A deadline-only change has no roster fingerprint.
///
/// The TTL is enforced by the live gate, not the roster snapshot
/// (`ApprovalPolicy` carries no TTL), so a deadline-only edit must not
/// discard live agent sessions for a rebuild that could not apply it.
#[test]
fn a_deadline_only_change_has_no_roster_fingerprint() {
    let no_deadline = effective_policy_fingerprint(&fp_policy("auto", &[], None, None));
    let deadline = effective_policy_fingerprint(&fp_policy("auto", &[], None, Some(72)));
    assert_eq!(no_deadline, deadline);
}

/// A spend-cap edit moves it too — the third axis a console save can touch
/// without touching the tier or the list.
///
/// `ApprovalPolicy` is built once per roster, so a cap-only edit that left
/// the fingerprint stable would keep the harness gate enforcing the old
/// threshold until restart. `auto_approve_under_usd`'s `Some`/`None` are
/// both states: an explicit no-cap (`None`) and a finite cap must each be
/// distinct. The deadline is deliberately NOT in the fingerprint — the
/// roster snapshot carries no TTL, and the deadline lives in the live gate,
/// so a deadline-only edit must not discard agent sessions for a rebuild
/// that could not apply it.
#[test]
fn the_policy_fingerprint_moves_when_the_cap_does() {
    let base = effective_policy_fingerprint(&fp_policy("auto", &[], None, None));
    let finite = effective_policy_fingerprint(&fp_policy("auto", &[], Some(25.0), None));
    let tighter = effective_policy_fingerprint(&fp_policy("auto", &[], Some(10.0), None));
    let deadline = effective_policy_fingerprint(&fp_policy("auto", &[], None, Some(72)));

    assert_ne!(base, finite, "a finite cap must rebuild");
    assert_ne!(finite, tighter, "a different cap value must rebuild");
    assert_eq!(
        base, deadline,
        "a deadline-only edit must NOT rebuild: the roster snapshot carries no TTL, \
         so a rebuild could not apply it and would only discard live agent sessions"
    );
    // Re-setting the same cap is a no-op, like re-setting the same tier.
    assert_eq!(
        finite,
        effective_policy_fingerprint(&fp_policy("auto", &[], Some(25.0), None))
    );
}

/// Issue #661 / L5: an overlay teammate's own `tools` grant flows into the
/// manifest shape `build_agent` consumes, and is INTERSECTED with the company
/// allow-list — narrow-only, never a widen. An empty grant is the standard
/// company-wide grant, exactly as the pre-L5 hardcoded empty was.
#[test]
fn overlay_agent_to_manifest_carries_the_tool_grant() {
    let allow = vec!["docs.*".to_string(), "web".to_string()];

    // A scoped overlay teammate: the grant is carried, then narrowed to what
    // the company already allows. `payment.send` is NOT in `allow`, so the
    // overlay cannot escalate to it — the security invariant.
    let scoped = OverlayAgent {
        provider: None,
        id: "scoped".into(),
        name: "Scoped".into(),
        role: "Researcher".into(),
        description: None,
        tools: Some(vec!["docs.*".into(), "payment.send".into()]),
        skills: None,
        model: None,
        harness: None,
    };
    let manifest = overlay_agent_to_manifest(&scoped);
    assert_eq!(
        manifest.tools,
        Some(vec!["docs.*".to_string(), "payment.send".to_string()]),
        "the overlay's own grant must reach the manifest shape"
    );
    assert_eq!(
        agent_effective_grants(&allow, manifest.tools.as_deref()),
        vec!["docs.*".to_string()],
        "narrow-only: the un-allowed `payment.send` is intersected out"
    );

    // An absent (`None`) overlay grant is the standard company-wide grant.
    // Since #1804 this is `None`, NOT an empty list (which is a deny-all).
    let standard = OverlayAgent {
        provider: None,
        id: "std".into(),
        name: "Std".into(),
        role: "Generalist".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    };
    let manifest = overlay_agent_to_manifest(&standard);
    assert!(manifest.tools.is_none());
    assert_eq!(
        agent_effective_grants(&allow, manifest.tools.as_deref()),
        allow,
        "an empty grant falls back to the full company allow-list"
    );
}

/// Issue #1105: the overlay's display name is the only place the operator's
/// chosen name exists, and the console shows it on the DM header, subtitle
/// and composer. Dropping it here left the persona framed from the role
/// alone, so the teammate denied being the person on its own header.
#[test]
fn overlay_agent_to_manifest_carries_the_display_name() {
    let overlay = OverlayAgent {
        provider: None,
        id: "alex".into(),
        name: "Alex".into(),
        role: "Content Writer".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    };

    let manifest = overlay_agent_to_manifest(&overlay);
    assert_eq!(manifest.name.as_deref(), Some("Alex"));
    // And it reaches the one place it has to: the persona the model reads.
    let persona = crate::company::prompt::persona_prompt("Acme", &manifest, None);
    assert!(
        persona.contains("You are Alex, the Content Writer at Acme"),
        "{persona}"
    );
}

/// Keys rework slice 3a: an overlay teammate's own `{provider, model}` pair
/// carries straight through to the synthesized `ManifestAgent`, the same
/// way `model`/`harness` already do — `build_agent`'s pin logic reads it
/// from there.
#[test]
fn overlay_agent_to_manifest_carries_the_provider() {
    let overlay = OverlayAgent {
        provider: Some("anthropic".into()),
        id: "sam".into(),
        name: "Sam".into(),
        role: "Web search".into(),
        description: None,
        tools: None,
        skills: None,
        model: Some("test-model-small".into()),
        harness: None,
    };
    let manifest = overlay_agent_to_manifest(&overlay);
    assert_eq!(manifest.provider.as_deref(), Some("anthropic"));
    assert_eq!(manifest.model.as_deref(), Some("test-model-small"));
}

/// Issue #661 / L5: a grant edit changes the roster the harness must build, so
/// it has to move the overlay fingerprint — otherwise a re-grant would
/// persist, render as applied, and be silently ignored until the process
/// restarted, the same staleness the tier/skill fingerprints guard against.
#[test]
fn overlay_fingerprint_moves_on_a_tools_only_edit() {
    let one = |tools: Option<Vec<String>>| {
        vec![OverlayAgent {
            provider: None,
            id: "a".into(),
            name: "A".into(),
            role: "r".into(),
            description: None,
            tools,
            skills: None,
            model: None,
            harness: None,
        }]
    };
    // `None` = standard grant, `Some(list)` = narrowed (issue #1804).
    let standard = one(None);
    let scoped = one(Some(vec!["docs.*".into()]));
    let scoped_more = one(Some(vec!["docs.*".into(), "email".into()]));

    assert_ne!(
        overlay_fingerprint(&standard, &[], &[]),
        overlay_fingerprint(&scoped, &[], &[]),
        "adding a grant must move the fingerprint or the re-grant is ignored until restart"
    );
    assert_ne!(
        overlay_fingerprint(&scoped, &[], &[]),
        overlay_fingerprint(&scoped_more, &[], &[]),
        "widening the grant list must move it too"
    );
    // Identical grants → identical fingerprint (no spurious rebuild).
    assert_eq!(
        overlay_fingerprint(&scoped, &[], &[]),
        overlay_fingerprint(&one(Some(vec!["docs.*".into()])), &[], &[])
    );
}

/// An overlay teammate's routing binding has to move the same axis: a
/// model/harness change is not a persona edit, but the roster the harness
/// builds consumes it (`overlay_agent_to_manifest` carries both straight
/// through), so a re-bind that moved nothing would be ignored until the
/// process restarted (issue #1676 review note).
#[test]
fn overlay_fingerprint_moves_on_a_model_or_harness_change() {
    let one = |model: Option<&str>, harness: Option<&str>| {
        vec![OverlayAgent {
            provider: None,
            id: "a".into(),
            name: "A".into(),
            role: "r".into(),
            description: None,
            tools: None,
            skills: None,
            model: model.map(str::to_string),
            harness: harness.map(str::to_string),
        }]
    };
    let none = one(None, None);
    let model = one(Some("chat-v2"), None);
    let model_again = one(Some("chat-v2"), None);
    let harness = one(None, Some("acp"));
    let cleared = one(Some(""), None);

    assert_ne!(
        overlay_fingerprint(&none, &[], &[]),
        overlay_fingerprint(&model, &[], &[]),
        "binding an overlay to a model must move the fingerprint or the re-bind is ignored until restart"
    );
    assert_ne!(
        overlay_fingerprint(&model, &[], &[]),
        overlay_fingerprint(&harness, &[], &[]),
        "binding an overlay to a harness must move the fingerprint too"
    );
    // The stored `Some("")` "cleared" form is a distinct routing state from
    // `None` ("never edited"), the same discriminant the resolver uses.
    assert_ne!(
        overlay_fingerprint(&none, &[], &[]),
        overlay_fingerprint(&cleared, &[], &[]),
        "an explicit clear must not hash like an untouched overlay"
    );
    // The same binding twice → the same fingerprint (no spurious rebuild).
    assert_eq!(
        overlay_fingerprint(&model, &[], &[]),
        overlay_fingerprint(&model_again, &[], &[])
    );
}

/// G5: a provider-only change to the pair (keys rework slice 3a) must move
/// both the overlay and the override fingerprints on its own — the same
/// staleness the `model`/`harness` hashes above guard against. Without
/// this a PATCH that only rebinds the provider half would save and change
/// nothing about the running roster until a restart.
#[test]
fn a_provider_edit_moves_the_overlay_and_override_fingerprints() {
    let overlays = |provider: Option<&str>| {
        vec![OverlayAgent {
            provider: provider.map(str::to_string),
            id: "a".into(),
            name: "A".into(),
            role: "r".into(),
            description: None,
            tools: None,
            skills: None,
            model: Some("test-model-large".into()),
            harness: None,
        }]
    };
    assert_ne!(
        overlay_fingerprint(&overlays(None), &[], &[]),
        overlay_fingerprint(&overlays(Some("anthropic")), &[], &[]),
        "an overlay's provider must move the overlay fingerprint"
    );

    let edits = |provider: Option<&str>| {
        vec![crate::ports::types::AgentOverride {
            agent_id: "ceo".into(),
            provider: provider.map(str::to_string),
            model: Some("test-model-large".to_string()),
            ..Default::default()
        }]
    };
    assert_ne!(
        overlay_fingerprint(&[], &edits(None), &[]),
        overlay_fingerprint(&[], &edits(Some("anthropic")), &[]),
        "a manifest teammate's provider edit must move the overlay fingerprint too"
    );
    assert_ne!(
        override_fingerprint(&edits(None)),
        override_fingerprint(&edits(Some("anthropic"))),
        "and the override fingerprint on its own"
    );
}

/// An edit of a **manifest** teammate has to move the same axis, and for the
/// same reason: a persona is assembled once per roster, so a rename that
/// moved nothing would read back correctly on the Team page and be invisible
/// to every turn the teammate took until the process restarted.
#[test]
fn overlay_fingerprint_moves_on_an_edit_of_a_manifest_teammate() {
    let edit = |role: &str| {
        vec![crate::ports::types::AgentOverride {
            agent_id: "ceo".into(),
            role: Some(role.to_string()),
            ..Default::default()
        }]
    };
    let none: Vec<crate::ports::types::AgentOverride> = Vec::new();

    assert_ne!(
        overlay_fingerprint(&[], &none, &[]),
        overlay_fingerprint(&[], &edit("Chief Vibes"), &[]),
        "a console rename must move the fingerprint or it is ignored until restart"
    );
    assert_ne!(
        overlay_fingerprint(&[], &edit("Chief Vibes"), &[]),
        overlay_fingerprint(&[], &edit("Chief Executive"), &[]),
        "and re-editing it must move it again"
    );
    // The same edit twice → the same fingerprint, so a save that changed
    // nothing does not drop every live session.
    assert_eq!(
        overlay_fingerprint(&[], &edit("Chief Vibes"), &[]),
        overlay_fingerprint(&[], &edit("Chief Vibes"), &[])
    );
}

/// A **routing** edit of a manifest teammate — a model or harness re-bind —
/// has to move the same axis for the same reason: the roster the harness
/// builds reads the override's routing fields, so a re-bind that moved
/// nothing would be silently ignored until the process restarted (issue
/// #1676 review note). `Some("")` (the stored "cleared" form) is a distinct
/// routing state from `None` ("never edited"), mirroring the resolver's
/// reset-to-blueprint contract.
#[test]
fn overlay_fingerprint_moves_on_a_model_or_harness_edit_of_a_manifest_teammate() {
    use crate::ports::types::AgentOverride;
    let edit = |model: Option<&str>, harness: Option<&str>| {
        vec![AgentOverride {
            agent_id: "ceo".into(),
            model: model.map(str::to_string),
            harness: harness.map(str::to_string),
            ..Default::default()
        }]
    };
    let none: Vec<AgentOverride> = Vec::new();

    assert_ne!(
        overlay_fingerprint(&[], &none, &[]),
        overlay_fingerprint(&[], &edit(Some("chat-v2"), None), &[]),
        "a model re-bind must move the fingerprint or it is ignored until restart"
    );
    assert_ne!(
        overlay_fingerprint(&[], &edit(Some("chat-v2"), None), &[]),
        overlay_fingerprint(&[], &edit(None, Some("acp")), &[]),
        "a harness re-bind must move it too"
    );
    assert_ne!(
        overlay_fingerprint(&[], &none, &[]),
        overlay_fingerprint(&[], &edit(Some(""), None), &[]),
        "an explicit model clear must not hash like an untouched teammate"
    );
    // The same edit twice → the same fingerprint (no spurious rebuild).
    assert_eq!(
        overlay_fingerprint(&[], &edit(Some("chat-v2"), None), &[]),
        overlay_fingerprint(&[], &edit(Some("chat-v2"), None), &[])
    );
}

/// Choosing or clearing a face writes an `AgentOverride` row whose only set
/// field is `avatar` (a teammate with no other override). The fingerprints
/// hash what a teammate *is*, never its face, so such a row must not move
/// either fingerprint — otherwise a purely cosmetic change would rebuild the
/// roster and drop every live agent session (issue #1676 review note).
#[test]
fn overlay_fingerprint_ignores_an_avatar_only_edit() {
    use crate::ports::types::AgentOverride;
    let avatar_only = |avatar: &str| {
        vec![AgentOverride {
            agent_id: "ceo".into(),
            avatar: Some(avatar.to_string()),
            ..Default::default()
        }]
    };
    let none: Vec<AgentOverride> = Vec::new();

    // Choosing a face for a teammate with no other override writes a row
    // whose only set field is `avatar`. That is not a persona change — the
    // harness reads nothing from the face — so it must not move the
    // fingerprint.
    assert_eq!(
        overlay_fingerprint(&[], &none, &[]),
        overlay_fingerprint(&[], &avatar_only("tiny:robot"), &[]),
        "an avatar-only row must not move the fingerprint"
    );
    // Clearing the face drops the row entirely (`clear_agent_avatar` →
    // `retain_nonempty_agent_edits`), which must not move it either.
    assert_eq!(
        overlay_fingerprint(&[], &avatar_only("tiny:robot"), &[]),
        overlay_fingerprint(&[], &none, &[]),
        "clearing an avatar-only row must not move the fingerprint"
    );
    // The filter is narrow: a real persona edit still moves the
    // fingerprint, even when the same teammate also carries a face.
    let edited = || {
        vec![AgentOverride {
            agent_id: "ceo".into(),
            role: Some("Chief".into()),
            avatar: Some("tiny:robot".into()),
            ..Default::default()
        }]
    };
    assert_ne!(
        overlay_fingerprint(&[], &none, &[]),
        overlay_fingerprint(&[], &edited(), &[]),
        "a real persona edit must still move the fingerprint"
    );
    // The filter is narrow the other way too: a row that changed only the
    // routing — `model` or `harness` with nothing else set — is not a face
    // change. The harness reads those fields when it binds a teammate, so
    // such a row must move the fingerprint or the old binding survives
    // until restart (codex review note).
    let routing = || {
        vec![AgentOverride {
            agent_id: "ceo".into(),
            model: Some("claude-opus-4-5".into()),
            ..Default::default()
        }]
    };
    assert_ne!(
        overlay_fingerprint(&[], &none, &[]),
        overlay_fingerprint(&[], &routing(), &[]),
        "a routing-only edit must still move the fingerprint"
    );
    let harness_routing = || {
        vec![AgentOverride {
            agent_id: "ceo".into(),
            harness: Some("external".into()),
            ..Default::default()
        }]
    };
    assert_ne!(
        overlay_fingerprint(&[], &none, &[]),
        overlay_fingerprint(&[], &harness_routing(), &[]),
        "a harness-only edit must still move the fingerprint"
    );
}

/// A removal has to move the same axis: a retired teammate left in a cached
/// roster would keep taking turns and keep receiving delegations after the
/// console said it was gone — the sharpest form of the staleness this axis
/// exists to prevent.
#[test]
fn overlay_fingerprint_moves_when_a_teammate_is_retired() {
    assert_ne!(
        overlay_fingerprint(&[], &[], &[]),
        overlay_fingerprint(&[], &[], &["ceo".to_string()]),
        "removing a teammate must move the fingerprint or it keeps running until restart"
    );
    assert_ne!(
        overlay_fingerprint(&[], &[], &["ceo".to_string()]),
        overlay_fingerprint(&[], &[], &["ceo".to_string(), "engineer".to_string()]),
        "and removing a second one must move it again"
    );
    // Re-recording the same removal changes nothing, which is what
    // `retire_agent`'s idempotence buys: no rebuild, no dropped sessions.
    assert_eq!(
        overlay_fingerprint(&[], &[], &["ceo".to_string()]),
        overlay_fingerprint(&[], &[], &["ceo".to_string()])
    );
}

/// Re-setting the same tier does not rebuild the roster.
///
/// Attribution is structurally absent from `Policy` — a re-save of the same
/// tier writes the same effective values, so the fingerprint cannot move
/// and live agent sessions are not dropped for a change no agent can
/// observe (the same reason `budget_fingerprint` omits attribution). The
/// inverted guard lives in `a_manifest_policy_edit_rebuilds_the_roster_with_no_override`
/// below: a manifest `[policy]` edit with no override in between MUST move
/// the key.
#[test]
fn re_setting_the_same_tier_does_not_rebuild_the_roster() {
    assert_eq!(
        effective_policy_fingerprint(&fp_policy("auto", &["payment.send"], Some(25.0), None)),
        effective_policy_fingerprint(&fp_policy("auto", &["payment.send"], Some(25.0), None)),
        "re-setting the same tier must not move the fingerprint"
    );
}

/// The mock's addresses are monotonic, not len-derived: a delete must not
/// make the next put reuse a surviving chunk's address (len-derived bug:
/// delete `addr-0` of `[addr-0, addr-1]`, and the next put minted
/// `addr-1` again — a later delete of `addr-1` then removed both rows).
#[tokio::test]
async fn mock_context_addresses_survive_deletion_without_reuse() {
    let ctx = MockContext::default();
    let company = CompanyId::new("acme");
    let chunk = |label: &str| ContextChunk {
        label: label.into(),
        body: label.into(),
    };
    let first = ctx.put(&company, chunk("l/0")).await.unwrap();
    let second = ctx.put(&company, chunk("l/1")).await.unwrap();
    assert!(
        ctx.delete(&company, &first).await.unwrap(),
        "first delete removes the row"
    );
    assert!(
        !ctx.delete(&company, &first).await.unwrap(),
        "repeat delete of the same addr finds nothing"
    );
    let third = ctx.put(&company, chunk("l/2")).await.unwrap();
    assert_ne!(
        third.as_ref() as &str,
        second.as_ref(),
        "a post-delete put must not reuse a surviving address"
    );
    assert!(ctx.delete(&company, &second).await.unwrap());
    let left = ctx.list(&company, "l/").await.unwrap();
    assert_eq!(left.len(), 1, "only the newest row remains: {left:?}");
}

#[tokio::test]
async fn roster_builds_every_manifest_agent() {
    let fx = fixture();
    let roster = build_roster(&test_runtime(), &record(), &fx.deps, &[], &HashMap::new())
        .expect("roster builds");
    let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
    assert_eq!(ids, vec!["ceo", "engineer"]);
    assert_eq!(roster[0].role, "Chief Executive");
}

/// Every teammate is a **named** openhuman session.
///
/// `AgentBuilder` defaults `event_session_id` to the literal
/// `"standalone"`, and this crate did not set it — so every agent of every
/// company on the process published `AgentTurnStarted`,
/// `AgentTurnCompleted` and `AgentError` under one shared id. That was
/// invisible while one turn ran at a time; openhuman's library host now
/// overlaps many sessions on one core, and an event stream nobody can
/// attribute is what that costs.
///
/// Asserted on [`CompanyAgent::session_key`] rather than on the built
/// session, because openhuman keeps `event_session_id()` `pub(super)` — a
/// session cannot be asked its own name from outside that crate. The field
/// and the `.event_context` call are filled from the same function, so this
/// pins the name the roster hands out; `session_key`'s own unit tests pin
/// the shape.
#[tokio::test]
async fn every_roster_teammate_gets_its_own_openhuman_session_name() {
    let rec = record();
    let fx = fixture();
    let roster =
        build_roster(&test_runtime(), &rec, &fx.deps, &[], &HashMap::new()).expect("roster builds");

    for agent in &roster {
        assert_eq!(
            agent.session_key,
            crate::harness::session_key::openhuman_session_key(&rec.id, &agent.agent_id),
            "{} was not named for its company and id",
            agent.agent_id
        );
    }

    let names: std::collections::HashSet<&str> =
        roster.iter().map(|a| a.session_key.as_str()).collect();
    assert_eq!(
        names.len(),
        roster.len(),
        "two teammates shared a session name — which is the `standalone`              collision this exists to end: {names:?}"
    );
    assert!(
        !names.contains("standalone"),
        "a teammate is still on the builder's unnamed default"
    );
}
