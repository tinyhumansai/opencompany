//! Integration tests for the `ops` write plane: tasks, memory, workspace,
//! skills, team, inbox-read, and desk chat — exercised end-to-end over the
//! router against a real fs-backed company.

use axum::http::StatusCode;
use serde_json::json;

use super::write_test_support::*;
use crate::ports::types::CompanyId;

#[tokio::test]
async fn skills_install_persists_the_registry_document_not_the_client_metadata() {
    let home_dir = home();
    let state = state_with_registry(home_dir.path()).await;

    // Deliberately hostile client metadata: if any of it reaches the persisted
    // document, install is still trusting the client.
    let (status, skill) = send(
        &state,
        "POST",
        "/api/v1/company/skills/competitor-scan/install",
        Some(json!({
            "name": "Not The Real Name",
            "description": "a one-line stub the client made up",
            "category": "Finance"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The response reflects the library's own metadata, not the request body.
    assert_eq!(skill["name"], "Competitor Scan");
    assert_eq!(skill["category"], "Research");
    assert_eq!(skill["source"], "registry");
    // The pinned revision rides on the installed projection, so a later "update
    // available" check can diff an install against the live library.
    assert_eq!(skill["version"], "1.0.0");
    assert!(
        skill["description"]
            .as_str()
            .unwrap()
            .starts_with("Profile a handful of competitors"),
        "description came from the registry, got {:?}",
        skill["description"]
    );

    // The persisted SKILL.md carries the whole procedure — the actual bug.
    let deltas = persisted_skills(&state).await;
    let row = deltas
        .iter()
        .find(|s| s.slug == "competitor-scan")
        .expect("the install persisted a row");
    let doc = row.custom_doc.as_deref().expect("a document was persisted");
    assert!(
        doc.contains("## Steps"),
        "body lost its Steps section: {doc}"
    );
    assert!(
        doc.contains("## Output"),
        "body lost its Output section: {doc}"
    );
    assert!(
        doc.contains("version: 1.0.0"),
        "the snapshot pins the library version: {doc}"
    );
    // None of the client's metadata leaked in.
    assert!(!doc.contains("Not The Real Name"), "{doc}");
    assert!(!doc.contains("a one-line stub the client made up"), "{doc}");
    // The body is the real procedure, not a copy of the description.
    let parsed = crate::company::parse_skill_md("competitor-scan", doc).expect("valid");
    assert_ne!(
        parsed.body.trim(),
        parsed.description.trim(),
        "the body must not be a degenerate copy of the description"
    );
}

#[tokio::test]
async fn skills_install_404s_a_slug_the_registry_lacks_and_persists_nothing() {
    let home_dir = home();
    let state = state_with_registry(home_dir.path()).await;

    // `competitor-analysis` was one of the console's phantom entries — it never
    // existed in the shared library. It must now fail loudly.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/skills/competitor-analysis/install",
        Some(json!({"name": "Competitor Analysis", "description": "phantom"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");

    assert!(
        persisted_skills(&state).await.is_empty(),
        "a rejected install must persist nothing"
    );
}

#[tokio::test]
async fn skills_install_falls_back_to_client_metadata_when_no_registry_is_served() {
    // Platform-provisioned mode: no shared library, so there is nothing to
    // resolve against and the client's metadata is all the host has.
    let home_dir = home();
    let state = state_with_company(home_dir.path()).await;

    let (status, skill) = send(
        &state,
        "POST",
        "/api/v1/company/skills/tenant-only-skill/install",
        Some(json!({"name": "Tenant Only", "description": "provisioned elsewhere"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an empty registry must not 404 every install"
    );
    assert_eq!(skill["name"], "Tenant Only");
    assert_eq!(skill["source"], "custom");

    let deltas = persisted_skills(&state).await;
    let row = deltas
        .iter()
        .find(|s| s.slug == "tenant-only-skill")
        .expect("the fallback persisted a row");
    assert_eq!(row.source, crate::ports::skills_state::SkillSource::Custom);
}

/// A *configured* shared library that cannot load must not degrade to the
/// empty-registry fallback above. Doing so would silently hand the client
/// authorship of a registry skill's contents on exactly the hosts that meant to
/// be server-authoritative — one malformed `SKILL.md` in the image and every
/// install starts trusting whatever the browser posted.
#[tokio::test]
async fn skills_install_500s_when_the_configured_library_cannot_load() {
    let home_dir = home();
    // A skills root that exists but holds a bundle whose `SKILL.md` has no
    // `description`, which the parser rejects.
    let broken_root = home_dir.path().join("broken-companies");
    std::fs::create_dir_all(broken_root.join("acme/skills/web-research")).expect("skill dir");
    std::fs::write(
        broken_root.join("acme/skills/web-research/SKILL.md"),
        "---\nname: Web Research\n---\n# Web Research\n",
    )
    .expect("SKILL.md");

    let state = state_with_company(home_dir.path())
        .await
        .with_skills_root(&broken_root);

    // The state itself reports the load failure rather than an empty registry.
    assert!(
        state.shared_skill_registry().is_err(),
        "a configured-but-unloadable library must surface its error"
    );

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/skills/web-research/install",
        Some(json!({"name": "Client Authored", "description": "not the library's"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a broken library is a server error, not a client-metadata install: {body}"
    );
    assert!(
        persisted_skills(&state).await.is_empty(),
        "a failed install must persist nothing"
    );

    // The registry listing fails the same way rather than reporting "no library".
    let (status, _) = send(&state, "GET", "/api/v1/company/skills/registry", None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn skills_registry_lists_the_live_library_without_bodies() {
    let home_dir = home();
    let state = state_with_registry(home_dir.path()).await;

    let (status, body) = send(&state, "GET", "/api/v1/company/skills/registry", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("an array");
    // Counted from disk rather than hardcoded, so adding a skill to a bundle
    // does not break this test — it still asserts the route lists the *whole*
    // catalog: every bundle's skills, the baseline's included.
    let on_disk = crate::company::load_catalog_skills(&repo_skills_root())
        .expect("the shipped bundles parse")
        .len();
    assert!(on_disk >= 14, "sanity: the catalog is populated");
    assert_eq!(rows.len(), on_disk, "every bundle skill is listed");

    for row in rows {
        assert!(
            row.get("body").is_none(),
            "registry rows must never carry a body: {row}"
        );
        assert_eq!(row["publisher"], "OpenCompany", "{row}");
    }
    // The baseline's skills carry a version, so an install of one is pinned.
    let research = rows
        .iter()
        .find(|r| r["id"] == "web-research")
        .expect("web-research is in the baseline");
    assert_eq!(research["version"], "1.0.0", "{research}");

    let scan = rows
        .iter()
        .find(|r| r["id"] == "competitor-scan")
        .expect("competitor-scan is in the library");
    assert_eq!(scan["name"], "Competitor Scan");
    assert_eq!(scan["category"], "Research");

    // The console's old hardcoded array listed slugs the host cannot serve;
    // the live list must not contain them.
    for phantom in ["competitor-analysis", "social-scheduler", "meeting-notes"] {
        assert!(
            !rows.iter().any(|r| r["id"] == phantom),
            "phantom slug {phantom} is not in the live registry"
        );
    }
}

#[tokio::test]
async fn skills_registry_is_empty_when_no_library_is_served() {
    let home_dir = home();
    let state = state_with_company(home_dir.path()).await;
    let (status, body) = send(&state, "GET", "/api/v1/company/skills/registry", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().expect("an array").len(), 0);
}

/// A slug is also a directory name (`skills/<slug>/`), so the handlers that
/// take one from the URL must refuse the values that could escape or traverse
/// the scratch tree: a parent segment (`..`), a path separator (`a/b`), and a
/// leading uppercase (`A` — slugs are lowercase by contract).
#[tokio::test]
async fn skill_handlers_reject_unsafe_slugs_and_write_nothing() {
    let home_dir = home();
    let state = state_with_registry(home_dir.path()).await;

    // `a%2Fb` is how a `/` arrives *inside* one path segment — the router sees
    // one slug and the handler must reject it rather than letting a separator
    // into a directory name.
    for bad in ["..", "a%2Fb", "A"] {
        let (status, body) = send(
            &state,
            "POST",
            &format!("/api/v1/company/skills/{bad}/install"),
            Some(json!({"name": "Name", "description": "desc"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "install {bad}: {body}");

        let (status, body) = send(
            &state,
            "PUT",
            &format!("/api/v1/company/skills/{bad}"),
            Some(json!({"enabled": true})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "toggle {bad}: {body}");
    }

    // A rejected slug must never reach the skill store.
    assert!(
        persisted_skills(&state).await.is_empty(),
        "an unsafe slug must not persist a delta"
    );

    // The same handlers still accept a well-formed slug.
    let (status, skill) = send(
        &state,
        "PUT",
        "/api/v1/company/skills/a-1",
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{skill}");
    assert_eq!(skill["id"], "a-1");
    assert_eq!(skill["enabled"], true);
}

#[tokio::test]
async fn skills_install_toggle_custom_and_builtin_uninstall_conflict() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // No shared library is served, so the install persists a SKILL.md built from
    // the client's metadata and records it as custom.
    let (status, skill) = send(
        &state,
        "POST",
        "/api/v1/company/skills/web-research/install",
        Some(json!({
            "name": "Web Research",
            "description": "Answer a question from multiple sources with citations.",
            "category": "Research"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(skill["source"], "custom");
    assert!(skill["enabled"].as_bool().unwrap());
    // The install response reflects the persisted custom_doc (parsed back), so a
    // non-empty description proves content was stored — the fix for the agent
    // never receiving registry skills.
    assert_eq!(skill["name"], "Web Research");
    assert_eq!(
        skill["description"],
        "Answer a question from multiple sources with citations."
    );

    // Uninstall the installed skill: 204.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/skills/web-research/uninstall",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Uninstalling an unknown/built-in skill is a 409.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/skills/onboard/uninstall",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "conflict");

    // Author a custom skill.
    let (status, custom) = send(
        &state,
        "POST",
        "/api/v1/company/skills",
        Some(json!({"name": "My Skill", "description": "Does a thing", "category": "Ops"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(custom["source"], "custom");
    assert_eq!(custom["name"], "My Skill");

    // Toggle it off.
    let (status, toggled) = send(
        &state,
        "PUT",
        "/api/v1/company/skills/my-skill",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!toggled["enabled"].as_bool().unwrap());

    // `GET …/skills` returns the effective set: here (no source dir) that's the
    // deltas — the custom skill, now disabled.
    let (status, list) = send(&state, "GET", "/api/v1/company/skills", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = list.as_array().expect("a JSON array of skills");
    let my_skill = rows
        .iter()
        .find(|s| s["id"] == "my-skill")
        .expect("the custom skill is listed");
    assert_eq!(my_skill["source"], "custom");
    assert_eq!(my_skill["name"], "My Skill");
    assert!(!my_skill["enabled"].as_bool().unwrap());
}

#[tokio::test]
async fn team_overlay_and_manifest_teammates_can_both_be_added_and_deleted() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // The manifest teammate shows up on the read side before any overlay add,
    // named `null` (the console falls back to the role).
    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK);
    // The company's own teammate; the global baseline is appended to every
    // roster and is not what this test is about.
    let roster = roster.as_array().unwrap();
    let ceo = roster
        .iter()
        .find(|row| row["id"] == "ceo")
        .expect("the manifest teammate is listed");
    assert_eq!(ceo["role"], "Chief");
    assert!(ceo["name"].is_null());

    // Add an overlay teammate.
    let (status, member) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Dana", "role": "Designer"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(member["role"], "Designer");
    let id = member["id"].as_str().unwrap().to_string();

    // The read side now merges in the overlay teammate, named this time.
    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK);
    let roster = roster.as_array().unwrap();
    let dana = roster.iter().find(|m| m["id"] == id).unwrap();
    assert_eq!(dana["name"], "Dana");
    assert_eq!(dana["role"], "Designer");

    // Deleting the overlay teammate is a 204.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/team/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The removed overlay teammate is gone from the read side too.
    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK);
    let roster = roster.as_array().unwrap();
    assert!(
        roster.iter().all(|row| row["id"] != id.as_str()),
        "the deleted overlay teammate is still listed: {roster:?}"
    );
    assert!(roster.iter().any(|row| row["id"] == "ceo"));

    // Toggle an inbox on.
    let (status, ack) = send(
        &state,
        "PUT",
        "/api/v1/company/team/ceo/inbox",
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["key"], "ceo");

    // And a manifest teammate deletes too — recorded as a tombstone on the
    // record, never as a rewrite of `company.toml`, so the blueprint being
    // re-read on the next load does not bring it back.
    let (status, _) = send(&state, "DELETE", "/api/v1/company/team/ceo", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        roster
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != "ceo"),
        "the manifest teammate is still listed after its delete: {roster:?}"
    );
}

#[tokio::test]
async fn inbox_read_marks_and_reports_unread() {
    use crate::ports::inbox::EmailRecord;
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    for i in 0..2 {
        runtime
            .inbox()
            .append(
                runtime.id(),
                &EmailRecord {
                    id: format!("m{i}"),
                    inbox: "ceo".into(),
                    from_name: "S".into(),
                    from_email: "s@x.test".into(),
                    subject: "hi".into(),
                    body: "yo".into(),
                    at_millis: i,
                    read: false,
                    outbound: false,
                },
            )
            .await
            .unwrap();
    }

    // Mark one read; one remains unread.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/inboxes/ceo/read",
        Some(json!({"ids": ["m0"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unread"], 1);

    // Mark the rest.
    let (status, body) = send(&state, "POST", "/api/v1/company/inboxes/ceo/read", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unread"], 0);
}
