//! The `skills` key on `PATCH {scope}/team/{agent_id}`: its four double-option
//! rows, what it refuses, and who may send it.
//!
//! Sibling of the `tools` route tests in
//! `team_agent_a_member_may_change_tests.rs`, and deliberately built on the same
//! fixtures. The two keys make the same three-state promise over different
//! vocabularies, and the failures they can have are the same failures: a state
//! that collapses into another one silently widens or narrows a teammate, and an
//! authority check verified only as an admin passes identically against no check
//! at all.

use axum::http::StatusCode;
use serde_json::json;

use super::team_agent_test_support::*;

/// The company's enabled skill slugs, read off the card rather than restated
/// here: the set comes from the global baseline, and a test that hard-coded it
/// would start failing the day a baseline skill is added rather than the day
/// this route breaks.
async fn available(state: &crate::AppState, agent: &str) -> Vec<String> {
    let (status, card) = get_agent(state, agent).await;
    assert_eq!(status, StatusCode::OK, "{card}");
    let slugs = strings(&card["skills"]["companyAvailable"]);
    assert!(
        !slugs.is_empty(),
        "the global baseline installs skills in every company, so there is something to scope: \
         {card}"
    );
    slugs
}

/// All four rows of the double option, in one teammate's life, because the
/// distinction between them is the entire contract: absent leaves the scope
/// alone, `null` resets to inherit, `[]` is a deliberate no-skills scope, and a
/// list narrows.
///
/// `requested` proves what was stored and `effective` proves what the harness
/// will materialize from it — the two are asserted separately so a row that
/// stores correctly and resolves wrongly cannot pass.
#[tokio::test]
async fn a_skill_scope_moves_through_all_four_double_option_rows() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;
    let enabled = available(&state, &jamie).await;
    let first = enabled[0].clone();

    // Unscoped to begin with: inherit, which resolves to the whole enabled set.
    let (_, before) = get_agent(&state, &jamie).await;
    assert!(
        before["skills"]["requested"].is_null(),
        "a fresh teammate inherits: {before}"
    );
    assert_eq!(strings(&before["skills"]["effective"]), enabled, "{before}");

    // Row 4 — a list narrows.
    let (status, narrowed) = patch_agent(&state, &jamie, json!({"skills": [first]})).await;
    assert_eq!(status, StatusCode::OK, "{narrowed}");
    assert_eq!(
        strings(&narrowed["skills"]["requested"]),
        vec![first.clone()],
        "{narrowed}"
    );
    assert_eq!(
        strings(&narrowed["skills"]["effective"]),
        vec![first.clone()],
        "narrower than the company's enabled set, which is the point: {narrowed}"
    );
    assert_eq!(
        strings(&narrowed["skills"]["companyAvailable"]),
        enabled,
        "the company's own set is untouched — this scoped one teammate: {narrowed}"
    );

    // Row 1 — the field absent leaves the scope alone. Sent alongside an edit
    // that does land, so this proves omission rather than a no-op request.
    let (status, renamed) = patch_agent(&state, &jamie, json!({"role": "Demand gen"})).await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["role"], "Demand gen", "{renamed}");
    assert_eq!(
        strings(&renamed["skills"]["requested"]),
        vec![first.clone()],
        "a patch that never mentioned `skills` must not touch the scope: {renamed}"
    );

    // Read back through a fresh request, so this is the stored record rather
    // than the handler's own answer.
    let (_, reread) = get_agent(&state, &jamie).await;
    assert_eq!(
        strings(&reread["skills"]["requested"]),
        vec![first.clone()],
        "{reread}"
    );

    // Row 3 — an explicit empty list is a deliberate no-skills scope, stored as
    // `[]` rather than collapsing back to `null`.
    let (status, denied) = patch_agent(&state, &jamie, json!({"skills": []})).await;
    assert_eq!(status, StatusCode::OK, "{denied}");
    assert_eq!(
        strings(&denied["skills"]["requested"]),
        Vec::<String>::new(),
        "an explicit empty list stores an empty scope, not null: {denied}"
    );
    assert!(
        strings(&denied["skills"]["effective"]).is_empty(),
        "a no-skills teammate reads nothing: {denied}"
    );

    // Row 2 — `null` is the way back to inherit, and must read as "inherits
    // everything" rather than as "reads nothing".
    let (status, reset) = patch_agent(&state, &jamie, json!({"skills": null})).await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    assert!(reset["skills"]["requested"].is_null(), "{reset}");
    assert_eq!(strings(&reset["skills"]["effective"]), enabled, "{reset}");
}

/// A slug is a directory name under the agent's materialized skill tree, so an
/// entry that is not a safe slug is refused on the request that asked for it
/// rather than stored and quietly ignored.
///
/// Blank and malformed are separate rows because they come from different
/// mistakes — a cleared input versus a tool grant's glob vocabulary carried over
/// — and each earns its own message.
#[tokio::test]
async fn a_blank_or_malformed_skill_slug_is_refused() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;
    let enabled = available(&state, &jamie).await;

    for blank in ["", "   ", "\t"] {
        let (status, refused) = patch_agent(&state, &jamie, json!({"skills": [blank]})).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a blank entry is a cleared input, not a scope: {refused}"
        );
    }

    // `*` is the tool grant's vocabulary. A slug is flat, so a wildcard here
    // would be stored as a literal slug matching nothing.
    for malformed in ["brand-*", "*", "Brand-Voice", "../escape", "brand voice"] {
        let (status, refused) = patch_agent(&state, &jamie, json!({"skills": [malformed]})).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "`{malformed}` is not a slug: {refused}"
        );
    }

    // Nothing was written by any of the refusals.
    let (_, unchanged) = get_agent(&state, &jamie).await;
    assert!(
        unchanged["skills"]["requested"].is_null(),
        "a refused scope must not half-land: {unchanged}"
    );
    assert_eq!(
        strings(&unchanged["skills"]["effective"]),
        enabled,
        "{unchanged}"
    );
}

/// A well-formed slug the company does not have enabled **is** stored: the
/// picker renders against a set fetched at page load, and a concurrent uninstall
/// would otherwise fail an honest save.
///
/// It confers nothing, which is what `effective` has to say — the card reads the
/// difference between the two as "asked for and not granted" and renders it.
#[tokio::test]
async fn a_well_formed_unknown_slug_is_stored_and_confers_nothing() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;
    let enabled = available(&state, &jamie).await;
    let held = enabled[0].clone();
    assert!(
        !enabled.iter().any(|slug| slug == "retired-playbook"),
        "the fixture's premise: this slug is not one the company has"
    );

    let (status, scoped) = patch_agent(
        &state,
        &jamie,
        json!({"skills": [held.clone(), "retired-playbook"]}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an unknown-but-well-formed slug is stored, not refused: {scoped}"
    );
    assert_eq!(
        strings(&scoped["skills"]["requested"]),
        vec![held.clone(), "retired-playbook".to_string()],
        "stored verbatim, so the card can show what was asked for: {scoped}"
    );
    assert_eq!(
        strings(&scoped["skills"]["effective"]),
        vec![held.clone()],
        "the unknown slug confers nothing: {scoped}"
    );

    let (_, reread) = get_agent(&state, &jamie).await;
    assert_eq!(
        strings(&reread["skills"]["requested"]),
        vec![held, "retired-playbook".to_string()],
        "{reread}"
    );
}

/// `skills` is admin-only, matching `tools`: a scope is capability, and a member
/// who could reset one to `null` could hand any teammate every skill the company
/// has.
///
/// The two-account shape is the point — the rest of this harness signs in as an
/// admin, so a gate verified only that way passes identically against no gate.
#[tokio::test]
async fn a_member_cannot_change_a_skill_scope() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;
    let enabled = available(&state, &jamie).await;
    let first = enabled[0].clone();

    let (status, _) = patch_agent(&state, &jamie, json!({"skills": [first.clone()]})).await;
    assert_eq!(status, StatusCode::OK);

    let uri = format!("/api/v1/company/team/{jamie}");
    let member = || crate::server::test_support::member_cookie("acme");

    // The widening: `null` hands back every skill the company has enabled.
    let (status, refusal) = send_as(
        &state,
        "PATCH",
        &uri,
        Some(json!({"skills": null})),
        member(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "resetting to null inherits the company's whole enabled set: {refusal}"
    );

    // …and narrowing is equally gated: every `skills` edit is, whichever state.
    let (status, refusal) =
        send_as(&state, "PATCH", &uri, Some(json!({"skills": []})), member()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refusal}");

    let (_, unchanged) = get_agent(&state, &jamie).await;
    assert_eq!(
        strings(&unchanged["skills"]["requested"]),
        vec![first],
        "the scope an admin set must survive both refusals: {unchanged}"
    );
}

/// A manifest teammate's scope is stored as an operator override rather than by
/// rewriting `company.toml`, and the card says so: `overridden` is what keeps an
/// override legible instead of looking like the manifest line itself.
#[tokio::test]
async fn scoping_a_manifest_teammate_is_reported_as_an_override() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let enabled = available(&state, "ceo").await;
    let first = enabled[0].clone();

    let (_, before) = get_agent(&state, "ceo").await;
    assert_eq!(before["skills"]["overridden"], false, "{before}");

    let (status, scoped) = patch_agent(&state, "ceo", json!({"skills": [first.clone()]})).await;
    assert_eq!(status, StatusCode::OK, "{scoped}");
    assert_eq!(scoped["skills"]["overridden"], true, "{scoped}");
    assert_eq!(
        strings(&scoped["skills"]["requested"]),
        vec![first.clone()],
        "{scoped}"
    );
    assert_eq!(
        strings(&scoped["skills"]["effective"]),
        vec![first],
        "{scoped}"
    );
}

/// The same four rows as the first test, but on a manifest teammate, whose
/// scope is stored as an override row rather than on the teammate itself.
///
/// The first write creates that row and every later one merges into it, so the
/// two halves are different code with the same promise, and only the merge can
/// fail this way: a second write that returns `200` and changes nothing reads
/// as saved until the page is reloaded. A single write cannot see it, which is
/// why the transitions are walked here rather than asserted once.
#[tokio::test]
async fn a_manifest_teammates_scope_survives_every_later_write() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let enabled = available(&state, "ceo").await;
    assert!(
        enabled.len() >= 2,
        "re-narrowing needs a second slug to move to: {enabled:?}"
    );
    let first = enabled[0].clone();
    let second = enabled[1].clone();

    let (status, scoped) = patch_agent(&state, "ceo", json!({"skills": [first.clone()]})).await;
    assert_eq!(status, StatusCode::OK, "{scoped}");

    // Re-narrow to a different slug. The stored row already exists by now.
    let (status, moved) = patch_agent(&state, "ceo", json!({"skills": [second.clone()]})).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(
        strings(&moved["skills"]["requested"]),
        vec![second.clone()],
        "the second write must replace the scope, not be dropped: {moved}"
    );
    let (_, reread) = get_agent(&state, "ceo").await;
    assert_eq!(
        strings(&reread["skills"]["requested"]),
        vec![second],
        "and it must be the stored record that moved, not the handler's answer: {reread}"
    );

    let (status, denied) = patch_agent(&state, "ceo", json!({"skills": []})).await;
    assert_eq!(status, StatusCode::OK, "{denied}");
    assert_eq!(
        strings(&denied["skills"]["requested"]),
        Vec::<String>::new(),
        "{denied}"
    );
    assert!(
        strings(&denied["skills"]["effective"]).is_empty(),
        "{denied}"
    );

    let (status, reset) = patch_agent(&state, "ceo", json!({"skills": null})).await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    assert!(
        reset["skills"]["requested"].is_null(),
        "an admin has to be able to hand the scope back to inherit: {reset}"
    );
    assert_eq!(strings(&reset["skills"]["effective"]), enabled, "{reset}");
}

/// Clearing an unrelated field must not widen a teammate's reach.
///
/// A manifest teammate's scope lives on an override row shared with their
/// avatar and instructions, and clearing either of those prunes the row when it
/// is left carrying nothing. A prune predicate that does not count the scope
/// therefore deletes a row that is still holding one — and because a missing
/// scope reads as "inherit", the teammate silently widens to every enabled
/// skill on an edit that was about their face.
#[tokio::test]
async fn clearing_an_unrelated_field_leaves_a_manifest_teammates_scope_alone() {
    for clearing in ["avatar", "instructions"] {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let enabled = available(&state, "ceo").await;
        let first = enabled[0].clone();

        let (status, scoped) = patch_agent(&state, "ceo", json!({"skills": [first.clone()]})).await;
        assert_eq!(status, StatusCode::OK, "{scoped}");

        let (status, cleared) = patch_agent(&state, "ceo", json!({clearing: null})).await;
        assert_eq!(status, StatusCode::OK, "clearing {clearing}: {cleared}");

        let (_, reread) = get_agent(&state, "ceo").await;
        assert_eq!(
            strings(&reread["skills"]["requested"]),
            vec![first.clone()],
            "clearing {clearing} must not drop the scope: {reread}"
        );
        assert_eq!(
            strings(&reread["skills"]["effective"]),
            vec![first],
            "and must not widen what the teammate actually holds: {reread}"
        );
    }
}

/// A scope handed back to inherit is not an override any more.
///
/// `overridden` says whether an override is *currently setting* the scope, and
/// the console renders it as such. A reset leaves the row in place holding the
/// reset itself, so a predicate that only asks whether the field was ever
/// written reports an override on a teammate that inherits — with `requested`
/// null beside it saying the opposite.
#[tokio::test]
async fn a_scope_reset_stops_reporting_as_an_override() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let enabled = available(&state, "ceo").await;
    let first = enabled[0].clone();

    let (_, scoped) = patch_agent(&state, "ceo", json!({"skills": [first]})).await;
    assert_eq!(scoped["skills"]["overridden"], true, "{scoped}");

    let (_, reset) = patch_agent(&state, "ceo", json!({"skills": null})).await;
    assert!(reset["skills"]["requested"].is_null(), "{reset}");
    assert_eq!(
        reset["skills"]["overridden"], false,
        "an inherited scope is not an override, whatever the row still holds: {reset}"
    );

    // A deny-all is still very much an override — the two must not collapse.
    let (_, denied) = patch_agent(&state, "ceo", json!({"skills": []})).await;
    assert_eq!(denied["skills"]["overridden"], true, "{denied}");
}
