use axum::http::StatusCode;
use serde_json::json;

use super::team_agent_test_support::*;

/// Issue #601: the roster **list** answers for tools and desks too, with
/// the same values as the detail read.
///
/// The overview graph is drawn from the list, so before this it had no way
/// to learn either without an N+1 fetch — and invented both instead, while
/// the detail card beside it rendered the real thing. The equality is the
/// contract; anything less lets the two surfaces disagree again.
#[tokio::test]
async fn the_roster_list_carries_the_same_tools_and_desks_as_the_detail_read() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    // An overlay teammate too, so the agreement is checked on both halves
    // of the merged roster rather than only on the manifest half.
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK, "{roster}");
    let rows = roster.as_array().unwrap();
    // Three manifest teammates, the overlay one, and every global the
    // fixture does not already declare an id for — this roster has its own
    // `writer`, which supersedes the baseline's rather than adding to it.
    let added = crate::globals::agents()
        .iter()
        .filter(|global| !["ceo", "writer", "hermit"].contains(&global.id.as_str()))
        .count();
    assert_eq!(rows.len(), 4 + added, "{roster}");

    for row in rows {
        let id = row["id"].as_str().unwrap();
        let (_, detail) = get_agent(&state, id).await;
        assert_eq!(
            row["tools"], detail["tools"],
            "the graph reads the list and the card reads the detail; they \
             must not disagree about {id}"
        );
        assert_eq!(row["desks"], detail["desks"], "desks disagree for {id}");
    }

    let row_of = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from {roster}"))
            .clone()
    };

    // …and the shared values are the *right* ones, so a shared-but-wrong
    // constructor cannot pass on agreement alone.
    let ceo = row_of("ceo");
    assert_eq!(
        strings(&ceo["tools"]["effective"]),
        vec!["workspace.read"],
        "a request the company never allowed is not a grant: {ceo}"
    );
    let writer = row_of("writer");
    assert!(writer["tools"]["requested"].is_null(), "{writer}");
    assert_eq!(
        strings(&writer["tools"]["effective"]),
        vec!["workspace", "workspace.*", "composio"],
        "an agent that lists no tools holds the whole allow-list: {writer}"
    );
    assert_eq!(
        strings(&writer["tools"]["companyAllow"]),
        vec!["workspace", "workspace.*", "composio"],
        "the ceiling rides along, so a reader can tell an empty request \
         from an empty grant: {writer}"
    );

    // Desks, which are the graph's departments now: declared membership,
    // the lead flag off the effective order, and a stated empty list.
    let writer_desks = writer["desks"].as_array().unwrap();
    assert_eq!(writer_desks.len(), 1, "{writer}");
    assert_eq!(writer_desks[0]["id"], "content", "{writer}");
    assert_eq!(writer_desks[0]["name"], "Content desk", "{writer}");
    assert_eq!(writer_desks[0]["lead"], true, "{writer}");
    assert_eq!(ceo["desks"].as_array().unwrap()[0]["lead"], false, "{ceo}");
    assert!(
        row_of("hermit")["desks"].as_array().unwrap().is_empty(),
        "a teammate on no desk says so with an empty list rather than by \
         omitting the key: {roster}"
    );
    assert!(
        row_of(&jamie)["desks"].as_array().unwrap().is_empty(),
        "{roster}"
    );
}

/// An operator-added desk membership reaches the list, not just the detail
/// read — otherwise the graph's pillars would go stale the moment somebody
/// moved a teammate.
#[tokio::test]
async fn a_desk_change_shows_up_on_the_roster_list() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/desks/content/members",
        Some(json!({"agent_id": "hermit"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    let hermit = roster
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "hermit")
        .unwrap()
        .clone();
    let desks = hermit["desks"].as_array().unwrap();
    assert_eq!(desks.len(), 1, "{hermit}");
    assert_eq!(desks[0]["id"], "content", "{hermit}");
}

/// A teammate created through the console reads back with the grant it
/// actually holds, so the card the console renders from the POST response
/// says the same thing the next list read will.
#[tokio::test]
async fn a_new_overlay_teammate_is_created_with_the_standard_grant() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Robin", "role": "Support"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(
        strings(&created["tools"]["effective"]),
        vec!["workspace", "workspace.*", "composio"],
        "{created}"
    );
    assert!(
        created["desks"].as_array().unwrap().is_empty(),
        "nobody has put it on a desk yet: {created}"
    );

    let (_, detail) = get_agent(&state, created["id"].as_str().unwrap()).await;
    assert_eq!(created["tools"], detail["tools"], "{created} vs {detail}");
    assert_eq!(created["desks"], detail["desks"], "{created} vs {detail}");
}

// --- The edit half ------------------------------------------------------

/// The issue's "write-once per member", gone: a console-defined teammate can
/// be corrected, and the correction is on the host rather than in a tab.
#[tokio::test]
async fn an_overlay_teammate_can_be_edited_and_the_edit_persists() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, edited) = patch_agent(
        &state,
        &jamie,
        json!({"name": "Jamie R", "role": "Head of Growth", "description": "Runs paid."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["name"], "Jamie R", "{edited}");
    assert_eq!(edited["role"], "Head of Growth", "{edited}");
    assert_eq!(edited["description"], "Runs paid.", "{edited}");

    // Read back through a fresh request, so this is the stored record and
    // not the handler's own answer.
    let (_, reread) = get_agent(&state, &jamie).await;
    assert_eq!(reread["name"], "Jamie R", "{reread}");
    assert_eq!(reread["role"], "Head of Growth", "{reread}");

    // …and the roster list agrees, so the card the operator came from is
    // updated too rather than only the panel they edited in.
    let (_, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    let row = roster
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == jamie.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["name"], "Jamie R", "{row}");
    assert_eq!(row["role"], "Head of Growth", "{row}");
}

/// A patch leaves what it does not mention alone, and an explicit `null`
/// clears the description. Collapsing those two would make every partial
/// save erase an agent's instructions.
#[tokio::test]
async fn an_absent_field_is_left_alone_and_null_clears_the_description() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, only_role) = patch_agent(&state, &jamie, json!({"role": "Growth Lead"})).await;
    assert_eq!(status, StatusCode::OK, "{only_role}");
    assert_eq!(only_role["name"], "Jamie", "{only_role}");
    assert_eq!(
        only_role["description"], "Original.",
        "an unmentioned field survives the patch: {only_role}"
    );

    let (status, cleared) = patch_agent(&state, &jamie, json!({"description": null})).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert!(
        cleared["description"].is_null(),
        "an explicit null clears it: {cleared}"
    );
    assert_eq!(cleared["role"], "Growth Lead", "{cleared}");
}

/// A **manifest** teammate — the shape every default and every global
/// baseline agent has — is editable here, and the edit sticks. This is the
/// whole point of the override layer: a hosted operator has no
/// `company.toml` to edit and no redeploy to make, so a roster that could
/// only be changed in the blueprint was a roster nobody could change.
#[tokio::test]
async fn a_manifest_teammate_can_be_edited_here() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, edited) = patch_agent(
        &state,
        "ceo",
        json!({"role": "Chief Vibes", "name": "Robin", "description": "Sets the beat."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");

    let (_, ceo) = get_agent(&state, "ceo").await;
    assert_eq!(ceo["role"], "Chief Vibes", "{ceo}");
    assert_eq!(ceo["name"], "Robin", "{ceo}");
    assert_eq!(ceo["description"], "Sets the beat.", "{ceo}");
    // Still a blueprint teammate — the manifest was not rewritten, the edit
    // is an overlay on top of it.
    assert_eq!(ceo["source"], "manifest", "{ceo}");

    // A second patch merges rather than replacing: a field nobody mentioned
    // keeps the value the first edit gave it.
    let (status, again) = patch_agent(&state, "ceo", json!({"description": null})).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert!(again["description"].is_null(), "{again}");
    assert_eq!(again["role"], "Chief Vibes", "{again}");
    assert_eq!(again["name"], "Robin", "{again}");
}

/// An untouched field keeps tracking the blueprint, so a redeploy that
/// changes it is still felt. The override is per field, not a snapshot of
/// the whole row.
#[tokio::test]
async fn an_unedited_field_still_comes_from_the_manifest() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, edited) = patch_agent(&state, "ceo", json!({"role": "Chief Vibes"})).await;
    assert_eq!(status, StatusCode::OK, "{edited}");

    let (_, ceo) = get_agent(&state, "ceo").await;
    assert_eq!(
        ceo["description"], "Sets direction and delegates.",
        "the manifest still answers for what nobody edited: {ceo}"
    );
    assert_eq!(ceo["tier"], "orchestrator", "{ceo}");
    assert_eq!(
        strings(&ceo["tools"]["requested"]),
        vec!["workspace.read", "email.send"],
        "{ceo}"
    );
}

/// A manifest with a blueprint `prompt`, so the persona-override tests have a
/// seed for "Reset to blueprint" to restore.
pub(super) const PERSONA_MANIFEST: &str = r#"
[company]
name = "Acme"
[policy]
mode = "full"
[tools]
allow = ["workspace", "workspace.*"]

[[agent]]
id = "ceo"
role = "Chief Executive"
prompt = "Lead decisively."
"#;

/// Issue #1530: a manifest teammate's persona `instructions` ARE editable —
/// they write to the override record, not `company.toml`, so no `409` — while
/// every other manifest field stays read-only. The response exposes the
/// effective text, the blueprint it would reset to, and that it is overridden.
#[tokio::test]
async fn instructions_are_editable_on_a_manifest_teammate_without_a_409() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), PERSONA_MANIFEST).await;

    let (status, edited) = patch_agent(
        &state,
        "ceo",
        json!({"instructions": "Answer only in haiku."}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an instructions-only edit is legal: {edited}"
    );
    assert_eq!(edited["instructions"], "Answer only in haiku.", "{edited}");
    assert_eq!(edited["instructionsOverridden"], true, "{edited}");
    assert_eq!(
        edited["blueprintInstructions"], "Lead decisively.",
        "the blueprint seed is surfaced for Reset: {edited}"
    );

    // Persisted, not just echoed by the handler.
    let (_, reread) = get_agent(&state, "ceo").await;
    assert_eq!(reread["instructions"], "Answer only in haiku.", "{reread}");
    assert_eq!(reread["instructionsOverridden"], true, "{reread}");

    // Merged behavior (main's agent-edit surface): a manifest teammate's
    // native fields are editable through the same override layer — a role
    // edit returns 200 and lands as an overlay, `company.toml` untouched —
    // and it composes with the instructions override set above.
    let (status, edited_role) = patch_agent(&state, "ceo", json!({"role": "Chief Vibes"})).await;
    assert_eq!(status, StatusCode::OK, "{edited_role}");
    assert_eq!(edited_role["role"], "Chief Vibes", "{edited_role}");
    assert_eq!(
        edited_role["source"], "manifest",
        "still a blueprint teammate: {edited_role}"
    );
    assert_eq!(
        edited_role["instructions"], "Answer only in haiku.",
        "the role edit leaves the instructions override intact: {edited_role}"
    );
}

/// Issue #1530: `instructions: null` on a manifest teammate clears the
/// override and resets to the blueprint `prompt` — the escape hatch that
/// keeps the override from masking version control forever.
#[tokio::test]
async fn null_instructions_resets_a_manifest_teammate_to_blueprint() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), PERSONA_MANIFEST).await;

    // Override, then reset.
    let (status, _) = patch_agent(&state, "ceo", json!({"instructions": "Custom voice."})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, reset) = patch_agent(&state, "ceo", json!({"instructions": null})).await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    assert_eq!(
        reset["instructions"], "Lead decisively.",
        "reset falls back to the blueprint: {reset}"
    );
    assert_eq!(
        reset["instructionsOverridden"], false,
        "no override masks the blueprint after a reset: {reset}"
    );

    // A blank string is a reset too, so an emptied editor never blanks the
    // persona.
    let (status, _) = patch_agent(&state, "ceo", json!({"instructions": "Custom voice."})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, blanked) = patch_agent(&state, "ceo", json!({"instructions": "   "})).await;
    assert_eq!(status, StatusCode::OK, "{blanked}");
    assert_eq!(blanked["instructions"], "Lead decisively.", "{blanked}");
    assert_eq!(blanked["instructionsOverridden"], false, "{blanked}");
}

/// Picking one of the shipped mascots, and putting it back. `null` resets to
/// "nobody has chosen", which is what makes the console's hashed default
/// reachable again — a stored empty string could not express it.
#[tokio::test]
async fn a_teammate_can_wear_a_tiny_flavour_and_take_it_off() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, worn) = patch_agent(&state, "ceo", json!({"avatar": "tiny:teal"})).await;
    assert_eq!(status, StatusCode::OK, "{worn}");
    assert_eq!(worn["avatar"], "tiny:teal", "{worn}");

    // Persisted, not just echoed — and visible on the roster list, which is
    // what every facepile in the console is drawn from.
    let (_, reread) = get_agent(&state, "ceo").await;
    assert_eq!(reread["avatar"], "tiny:teal", "{reread}");
    let (_, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    let row = roster
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "ceo")
        .expect("the ceo is on the roster");
    assert_eq!(row["avatar"], "tiny:teal", "{row}");

    let (status, bare) = patch_agent(&state, "ceo", json!({"avatar": null})).await;
    assert_eq!(status, StatusCode::OK, "{bare}");
    assert!(
        bare.get("avatar").is_none(),
        "a reset is absent, not empty: {bare}"
    );
}

/// Resetting a face must not reset a persona, and vice versa. The two share
/// one override row, so this is the route-level net under the record-level
/// invariant.
#[tokio::test]
async fn resetting_a_face_leaves_the_persona_alone() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), PERSONA_MANIFEST).await;

    patch_agent(&state, "ceo", json!({"instructions": "Answer in haiku."})).await;
    patch_agent(&state, "ceo", json!({"avatar": "tiny:rose"})).await;

    let (status, reset) = patch_agent(&state, "ceo", json!({"avatar": null})).await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    assert_eq!(
        reset["instructions"], "Answer in haiku.",
        "the persona survives a face reset: {reset}"
    );

    let (_, persona_reset) = patch_agent(&state, "ceo", json!({"instructions": null})).await;
    patch_agent(&state, "ceo", json!({"avatar": "tiny:rose"})).await;
    let (_, after) = patch_agent(&state, "ceo", json!({"instructions": null})).await;
    assert_eq!(
        after["avatar"], "tiny:rose",
        "the face survives a persona reset: {after} (first reset: {persona_reset})"
    );
}

/// The rule the grammar exists for: an avatar names something this host
/// holds. A stored URL would be an instruction the console obeys, in an
/// `src=`, on every surface that draws a face.
#[tokio::test]
async fn a_url_is_not_an_avatar() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    for hostile in [
        "https://tracker.example/beacon.gif",
        "javascript:alert(1)",
        "data:image/gif;base64,R0lGOD",
        "tiny:puce",
    ] {
        let (status, refused) = patch_agent(&state, "ceo", json!({"avatar": hostile})).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{hostile} was accepted: {refused}"
        );
    }
    // And nothing was stored on the way out.
    let (_, reread) = get_agent(&state, "ceo").await;
    assert!(reread.get("avatar").is_none(), "{reread}");
}

/// The custom-image path end to end: upload, then wear what came back.
/// A GIF specifically, because an animated face is the case the format
/// allowlist exists to admit.
#[tokio::test]
async fn an_uploaded_gif_becomes_a_wearable_face() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, uploaded) = upload_avatar(&state, "wave.gif", TINY_GIF).await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    assert_eq!(
        uploaded["mime"], "image/gif",
        "sniffed from the bytes, not taken from the part's `image/png`: {uploaded}"
    );
    let reference = uploaded["avatar"]
        .as_str()
        .expect("a reference")
        .to_string();
    assert!(reference.starts_with("blob:"), "{reference}");

    let (status, worn) = patch_agent(&state, "ceo", json!({"avatar": reference})).await;
    assert_eq!(status, StatusCode::OK, "{worn}");
    assert_eq!(worn["avatar"], reference, "{worn}");

    // And the bytes come back through the blob route the console reads.
    let node = uploaded["nodeId"].as_str().unwrap();
    let (status, _) = send(
        &state,
        "GET",
        &format!("/api/v1/company/workspace/blob/{node}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// What only claims to be an image is refused at the door — the reason the
/// route sniffs rather than trusting the declared type.
#[tokio::test]
async fn an_upload_that_is_not_an_image_is_refused() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, refused) = upload_avatar(
        &state,
        "face.png",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script/></svg>",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
}

/// A payload small enough to pass the 4 MiB ceiling whose header claims a
/// 65535×65535 frame — the decompression bomb. Refused on the upload, so
/// the bytes are never stored to allocate a gigabyte for every member who
/// views the roster.
#[tokio::test]
async fn an_upload_that_decodes_to_a_huge_size_is_refused() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, refused) = upload_avatar(&state, "bomb.png", &bomb_png()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["error"].as_str().is_some() || refused.as_object().is_some(),
        "a named refusal: {refused}"
    );
}

/// The roster **list** answers for the harness, the model and the provider
/// too — from the same helpers as the detail read, and with the key *absent*
/// for a teammate that pins nothing.
///
/// The Agents grid draws a card per teammate, and until the list carried
/// these there was nothing on it to draw: resolving them meant an N+1 over
/// the detail read, or inventing an answer. Absent has to stay absent, so a
/// card can say "inherits the company default" instead of naming a model the
/// record never declared.
#[tokio::test]
async fn the_roster_list_carries_the_declared_harness_model_and_provider() {
    let home_dir = home();
    const TOML: &str = r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"
harness = "laptop"
model = "claude-opus-4-5"

[[agent]]
id = "writer"
role = "Writer"

[[agent]]
id = "hermit"
role = "Hermit"

[[harness]]
id = "main"
kind = "built_in"
default = true

[[harness]]
id = "laptop"
kind = "acp"

[harness.acp]
transport = "local"
agent = "claude"
"#;
    let state = state_with_manifest(home_dir.path(), TOML).await;
    seed_provider(&state, "anthropic", true).await;

    // A blueprint teammate's pin, stored as an overlay edit — the half a read
    // of the raw manifest row would skip.
    let (status, patched) = patch_agent(
        &state,
        "writer",
        json!({"harness": "laptop", "model": "gpt-5-codex"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");

    // An overlay teammate on the default built-in harness, pinning the pair.
    let jamie = add_overlay(&state, "Jamie", "Growth").await;
    let (status, pinned) = patch_agent(
        &state,
        &jamie,
        json!({"provider": "anthropic", "model": "claude-sonnet-4-5"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pinned}");

    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK, "{roster}");
    let rows = roster.as_array().unwrap();
    let row_of = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from {roster}"))
            .clone()
    };

    let ceo = row_of("ceo");
    assert_eq!(ceo["harness"], "laptop", "{ceo}");
    assert_eq!(ceo["model"], "claude-opus-4-5", "{ceo}");

    let writer = row_of("writer");
    assert_eq!(writer["harness"], "laptop", "{writer}");
    assert_eq!(
        writer["model"], "gpt-5-codex",
        "a pin stored in the overlay half reaches the list: {writer}"
    );

    let jamie_row = row_of(&jamie);
    assert_eq!(jamie_row["provider"], "anthropic", "{jamie_row}");
    assert_eq!(jamie_row["model"], "claude-sonnet-4-5", "{jamie_row}");

    // The teammate that declares none: the keys are *missing*, not null and
    // not empty — absence is what lets a card say "inherits" honestly.
    let hermit = row_of("hermit");
    for key in ["harness", "model", "provider"] {
        assert!(
            hermit.get(key).is_none(),
            "a teammate that pins nothing omits `{key}` rather than sending a \
             default: {hermit}"
        );
    }

    // …and the list agrees with the detail read, so the card and the page
    // cannot disagree about the same teammate.
    for row in rows {
        let id = row["id"].as_str().unwrap();
        let (_, detail) = get_agent(&state, id).await;
        for key in ["harness", "model", "provider"] {
            assert_eq!(
                row.get(key),
                detail.get(key),
                "the card reads the list and the page reads the detail; they \
                 must not disagree about {key} for {id}"
            );
        }
    }
}
