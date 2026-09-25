use crate::company::CompanyManifest;
use crate::ports::CompanyStore;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::runtime::RuntimeBuilder;
use crate::server::router;
use crate::store::FsCompanyStore;
use crate::{AppConfig, AppState};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

use super::graphql_test_group_1::query;
use super::graphql_test_support_1::*;

/// On the serve path a company has an on-disk source dir; `Company.skills`,
/// `Company.workflow`, and the top-level `skillRegistry` resolve their content
/// from it (and the `companies/` tree behind `skills_root`) rather than the
/// empty bundle.
#[tokio::test]
async fn skills_and_workflows_resolve_from_source_dir() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    // A company source directory with a committed skill and workflow.
    let source_dir = home.join("companies").join("acme");
    tokio::fs::create_dir_all(source_dir.join("skills/deal-memo"))
        .await
        .unwrap();
    tokio::fs::write(
        source_dir.join("skills/deal-memo/SKILL.md"),
        "---\nname: Deal Memo\ndescription: Write a deal memo.\ncategory: Research\n---\n# Deal Memo\n",
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(source_dir.join("workflows"))
        .await
        .unwrap();
    tokio::fs::write(
        source_dir.join("workflows/flow.toml"),
        "id = \"flow\"\nname = \"Test Flow\"\n[[node]]\nid = \"n1\"\nkind = \"trigger\"\nname = \"Start\"\n",
    )
    .await
    .unwrap();

    // A separate `companies/` tree backing `skillRegistry`: the registry is
    // every bundle's `skills/` under it, not this company's source dir.
    let skills_root = home.join("catalog");
    tokio::fs::create_dir_all(skills_root.join("other/skills/web-research"))
        .await
        .unwrap();
    tokio::fs::write(
        skills_root.join("other/skills/web-research/SKILL.md"),
        "---\nname: Web Research\ndescription: Research on the web.\ncategory: Research\n---\n# Web Research\n",
    )
    .await
    .unwrap();

    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[workflows]\nenabled = [\"flow\"]\n",
    )
    .unwrap();
    let store = FsCompanyStore::new(home.to_path_buf());
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest.clone(),
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
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .with_seed_dir(source_dir.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default())
        .with_home(home.to_path_buf())
        .with_skills_root(skills_root);
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    // Company.skills reads the committed source-dir skill.
    let value = query(
        router(state.clone()),
        r#"{"query":"{ company(id:\"acme\"){ skills { id name source } workflow(id:\"flow\"){ id name nodes { id } } } skillRegistry { id name } }"}"#,
    )
    .await;
    let company = &value["data"]["company"];
    let skills = own_skills(&company["skills"]);
    assert_eq!(skills.len(), 1, "source-dir skill resolves");
    assert_eq!(skills[0]["id"], "deal-memo");
    assert_eq!(skills[0]["source"], "company");
    // The baseline is installed in every company, so it is listed here too.
    for doc in crate::globals::skills() {
        assert!(
            company["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["id"] == serde_json::json!(doc.slug)),
            "the global `{}` is listed",
            doc.slug
        );
    }
    // Company.workflow reads the graph from the source dir.
    assert_eq!(company["workflow"]["name"], "Test Flow");
    assert_eq!(company["workflow"]["nodes"].as_array().unwrap().len(), 1);
    // skillRegistry reads the bundle catalog behind `skills_root`.
    let registry = value["data"]["skillRegistry"].as_array().unwrap();
    assert!(registry.iter().any(|s| s["id"] == "web-research"));
}

/// Issue #239: `install` pins the library document into the delta, so a later
/// library edit must not rewrite an existing install. `Company.skills` has to
/// project that pinned snapshot rather than re-reading the live library —
/// otherwise GraphQL and REST report different `version`s for the same install,
/// and a slug that later leaves the library loses its persisted content.
#[tokio::test]
async fn company_skills_project_the_pinned_snapshot_of_a_registry_install() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    // The registry (a `companies/` tree) has moved on to a rewritten v2 of
    // `web-research`, and never had `retired-skill` at all.
    let skills_root = home.join("catalog");
    tokio::fs::create_dir_all(skills_root.join("other/skills/web-research"))
        .await
        .unwrap();
    tokio::fs::write(
        skills_root.join("other/skills/web-research/SKILL.md"),
        "---\nname: Web Research v2\ndescription: Rewritten upstream.\ncategory: Ops\nversion: 2.0.0\n---\n# Web Research v2\n",
    )
    .await
    .unwrap();

    let store = FsCompanyStore::new(home.clone());
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest(),
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
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.clone(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();

    // Two registry installs, each holding the document pinned at install time.
    for (slug, doc) in [
        (
            "web-research",
            "---\nname: Web Research\ndescription: Research on the web.\ncategory: Research\nversion: 1.0.0\n---\n# Web Research\nStep one.\n",
        ),
        (
            "retired-skill",
            "---\nname: Retired Skill\ndescription: Withdrawn from the library.\ncategory: Ops\nversion: 1.0.0\n---\n# Retired Skill\nStill installed here.\n",
        ),
    ] {
        runtime
            .skills()
            .set(
                runtime.id(),
                &crate::ports::skills_state::SkillState {
                    slug: slug.to_string(),
                    enabled: true,
                    source: crate::ports::skills_state::SkillSource::Registry,
                    custom_doc: Some(doc.to_string()),
                    updated_at_millis: None,
                },
            )
            .await
            .unwrap();
    }

    let state = AppState::new(AppConfig::default())
        .with_home(home.clone())
        .with_skills_root(skills_root);
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    let value = query(
        router(state.clone()),
        r#"{"query":"{ company(id:\"acme\"){ skills { id name description category source version } } skillRegistry { id version } }"}"#,
    )
    .await;
    let skills = value["data"]["company"]["skills"].as_array().unwrap();
    // `web-research` is also a baseline slug, so the row below doubles as proof
    // that the install's pinned snapshot supersedes the global it sits on.
    assert_eq!(
        own_skills(&value["data"]["company"]["skills"]).len(),
        1,
        "the retired install is the only row outside the baseline: {value}"
    );

    let pinned = skills
        .iter()
        .find(|s| s["id"] == "web-research")
        .expect("the installed library skill");
    assert_eq!(pinned["source"], "registry");
    assert_eq!(
        pinned["version"], "1.0.0",
        "the pinned revision survives a later library edit"
    );
    assert_eq!(pinned["name"], "Web Research");
    assert_eq!(pinned["category"], "Research");

    // A slug the library no longer serves keeps its real persisted content
    // instead of degrading to a titleized name and a blank description.
    let retired = skills
        .iter()
        .find(|s| s["id"] == "retired-skill")
        .expect("the install whose slug left the library");
    assert_eq!(retired["name"], "Retired Skill");
    assert_eq!(retired["description"], "Withdrawn from the library.");
    assert_eq!(retired["version"], "1.0.0");

    // The registry tab itself is *not* pinned — it browses the live library.
    let registry = value["data"]["skillRegistry"].as_array().unwrap();
    assert_eq!(registry.len(), 1);
    assert_eq!(registry[0]["id"], "web-research");
    assert_eq!(registry[0]["version"], "2.0.0");
}

/// Issue #168: a hosted tenant has no source directory, so its workflows live
/// only as runtime-authored bodies on the record. `Company.workflows` must
/// resolve their real display name (not the id fallback) and `Company.workflow`
/// must return the full graph.
#[tokio::test]
async fn workflows_resolve_from_the_record_overlay_with_no_source_dir() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[workflows]\nenabled = [\"hosted\"]\n",
    )
    .unwrap();
    let store = FsCompanyStore::new(home.to_path_buf());
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: vec![crate::ports::types::OverlayWorkflow {
                id: "hosted".to_string(),
                toml: "id = \"hosted\"\nname = \"Hosted Flow\"\n\
                       [[node]]\nid = \"n1\"\nkind = \"trigger\"\nname = \"Start\"\n\
                       [[node]]\nid = \"n2\"\nkind = \"output\"\nname = \"Done\"\n\
                       [[edge]]\nfrom = \"n1\"\nto = \"n2\"\n"
                    .to_string(),
            }],
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
        })
        .await
        .unwrap();

    // Built WITHOUT `with_seed_dir` — the hosted shape.
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    assert!(
        runtime.source_dir().is_none(),
        "no source dir in hosted mode"
    );
    let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    let value = query(
        router(state),
        r#"{"query":"{ company(id:\"acme\"){ workflows { id name enabled } workflow(id:\"hosted\"){ id name nodes { id } edges { from to } } } }"}"#,
    )
    .await;
    let company = &value["data"]["company"];
    let summaries = own_workflows(&company["workflows"]);
    assert_eq!(summaries.len(), 1, "value: {value}");
    assert_eq!(summaries[0]["id"], "hosted");
    // The real name from the overlay body, not the id fallback.
    assert_eq!(summaries[0]["name"], "Hosted Flow");
    assert_eq!(company["workflow"]["name"], "Hosted Flow");
    assert_eq!(company["workflow"]["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(company["workflow"]["edges"].as_array().unwrap().len(), 1);
}

/// Issue #168: a runtime-authored workflow with an **empty** manifest
/// `[workflows].enabled` must still appear in `Company.workflows`. The resolver
/// used to drive its id set off the enabled list alone, so this returned `[]`
/// while `Company.workflow` happily returned the full graph.
///
/// This used to be the ordinary post-restart state on the fs backend, because a
/// boot rebuild overwrote the record's manifest from the seed. Issue #208 fixed
/// that — a rebuild now merges surviving overlay ids back into `enabled` — so
/// the state is written here *after* the build instead. The resolver's guarantee
/// is unchanged and still worth pinning: it enumerates graph bodies on their own
/// evidence, whatever put the record in this shape (a hand-edited record, a
/// store written by an older build, a future writer that adds a body first).
///
/// Also pins REST/GraphQL agreement: both surfaces must report the same id set.
#[tokio::test]
async fn workflows_summary_lists_an_overlay_workflow_with_no_enabled_entry() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    // Nothing enabled in the manifest — the graph body is the only evidence.
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();

    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest.clone())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    assert!(
        runtime.source_dir().is_none(),
        "no source dir in hosted mode"
    );

    // Write the enabled-less record AFTER the build. Since issue #208 a boot
    // rebuild merges surviving overlay ids back into `[workflows].enabled`, so
    // seeding this state before the build would be healed away — and this test
    // is about the *resolver*, which must enumerate overlay bodies on their own
    // evidence no matter how the record got into this shape.
    let store = FsCompanyStore::new(home.to_path_buf());
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: vec![crate::ports::types::OverlayWorkflow {
                id: "orphan".to_string(),
                toml: "id = \"orphan\"\nname = \"Orphan Flow\"\n\
                       [[node]]\nid = \"n1\"\nkind = \"trigger\"\nname = \"Start\"\n"
                    .to_string(),
            }],
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
        })
        .await
        .unwrap();

    let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    let value = query(
        router(state.clone()),
        r#"{"query":"{ company(id:\"acme\"){ workflows { id name enabled } } }"}"#,
    )
    .await;
    let summaries = own_workflows(&value["data"]["company"]["workflows"]);
    assert_eq!(summaries.len(), 1, "value: {value}");
    assert_eq!(summaries[0]["id"], "orphan");
    assert_eq!(summaries[0]["name"], "Orphan Flow");
    // Honest flag: the graph exists and is runnable, but the manifest does not
    // declare it, so `enabled` reads false rather than being faked to true.
    assert_eq!(
        summaries[0]["enabled"], false,
        "`enabled` reports manifest membership, not existence"
    );

    // REST and GraphQL must report the same id set.
    let response = router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/company/workflows")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let rest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let rest_ids: Vec<&str> = rest
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    // Both sides unfiltered here: the point of this assertion is that the two
    // surfaces answer with the same id set, baseline graphs included.
    let gql_ids: Vec<&str> = value["data"]["company"]["workflows"]
        .as_array()
        .expect("summaries")
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(rest_ids, gql_ids, "REST and GraphQL disagree on the id set");
}

/// A company workflow whose id collides with a global's must win — checked by
/// its own distinguishing content, not by `own_workflows`' id heuristic, which
/// would misclassify this exact row as "the baseline's" because the ids match.
///
/// This is the case the `own_workflows` doc comment calls out directly: a
/// company definition of the same id as a global supersedes it (see
/// `crate::company::list_workflows_with_globals`), so `Company.workflows` must
/// list exactly one row for that id, carrying the company's own name.
#[tokio::test]
async fn graphql_lists_a_company_override_of_a_global_id_by_its_own_content() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");
    let taken = crate::globals::workflows()[0].id.clone();

    let store = FsCompanyStore::new(home.to_path_buf());
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: vec![crate::ports::types::OverlayWorkflow {
                id: taken.clone(),
                toml: format!(
                    "id = \"{taken}\"\nname = \"Ours, Not The Baseline's\"\n\
                     [[node]]\nid = \"n1\"\nkind = \"trigger\"\nname = \"Start\"\n\
                     [[node]]\nid = \"n2\"\nkind = \"output\"\nname = \"Done\"\n\
                     [[edge]]\nfrom = \"n1\"\nto = \"n2\"\n"
                ),
            }],
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
        })
        .await
        .unwrap();

    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    let value = query(
        router(state),
        r#"{"query":"{ company(id:\"acme\"){ workflows { id name } } }"}"#,
    )
    .await;
    let summaries = value["data"]["company"]["workflows"].as_array().unwrap();
    let matching: Vec<&serde_json::Value> = summaries
        .iter()
        .filter(|row| row["id"] == taken.as_str())
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "the shadowed global must not be listed alongside the override: {value}"
    );
    assert_eq!(
        matching[0]["name"], "Ours, Not The Baseline's",
        "the company's own definition must win, not the global's: {value}"
    );
}

/// A company that opts out of a global workflow via `[globals].disable` must
/// neither list it in `Company.workflows` nor resolve it through
/// `Company.workflow(id)` — the same contract `crate::globals::test`'s
/// `a_disabled_global_workflow_neither_lists_nor_loads` pins at the pure
/// `list_workflows_with_globals` / `load_workflow_with_globals` layer, checked
/// here through the actual GraphQL resolvers (`resolve_summaries` /
/// `resolve_one`) instead of calling those functions directly.
#[tokio::test]
async fn graphql_hides_a_company_disabled_global_workflow() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");
    let dropped = crate::globals::workflows()[0].id.clone();
    let kept = crate::globals::workflows()[1].id.clone();

    let disabling_manifest: CompanyManifest = toml::from_str(&format!(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\n\
         [globals]\ndisable = [\"workflow:{dropped}\"]\n"
    ))
    .unwrap();

    let store = FsCompanyStore::new(home.to_path_buf());
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: disabling_manifest.clone(),
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
        })
        .await
        .unwrap();

    let runtime = RuntimeBuilder::new(home.to_path_buf(), disabling_manifest)
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    let value = query(
        router(state),
        &format!(
            r#"{{"query":"{{ company(id:\"acme\"){{ workflows {{ id }} dropped: workflow(id:\"{dropped}\"){{ id }} kept: workflow(id:\"{kept}\"){{ id }} }} }}"}}"#
        ),
    )
    .await;
    let company = &value["data"]["company"];
    let ids: Vec<&str> = company["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert!(
        !ids.contains(&dropped.as_str()),
        "the disabled global must not be listed: {value}"
    );
    assert!(
        ids.contains(&kept.as_str()),
        "an unrelated global must still be listed: {value}"
    );
    assert!(
        company["dropped"].is_null(),
        "the disabled global must not resolve by id either: {value}"
    );
    assert!(
        !company["kept"].is_null(),
        "an unrelated global must still resolve by id: {value}"
    );
}
