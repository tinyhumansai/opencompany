use axum::http::StatusCode;
use serde_json::json;

use super::team_agent_test_support::*;
use crate::AppState;
use crate::ports::store::company_write_lock;
use crate::ports::types::CompanyId;
use crate::runtime::RuntimeBuilder;

/// The same edit against a **manifest** teammate, which is the common case
/// and the one that silently did nothing.
///
/// Both fields were advertised in `editable` and accepted with a 200, but
/// the override written for a blueprint agent carried only name, role,
/// tools and description — so the values were dropped on the floor and the
/// next read returned the blueprint's. Nothing surfaced the loss: the
/// response body echoed the request, so it looked saved.
///
/// Asserted through a fresh `GET` rather than the `PATCH` response,
/// because echoing the request back is precisely what made the bug
/// invisible.
#[tokio::test]
async fn harness_and_model_persist_for_a_manifest_teammate() {
    let home_dir = home();
    const TOML: &str = r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

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

    let (status, set) = patch_agent(
        &state,
        "ceo",
        json!({"harness": "laptop", "model": "claude-opus-4-5"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");

    let (_, reread) = get_agent(&state, "ceo").await;
    assert_eq!(reread["harness"], "laptop", "{reread}");
    assert_eq!(reread["model"], "claude-opus-4-5", "{reread}");

    // And clearing returns it to the blueprint rather than sticking.
    let (status, cleared) =
        patch_agent(&state, "ceo", json!({"harness": null, "model": null})).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    let (_, after) = get_agent(&state, "ceo").await;
    assert!(after["harness"].is_null(), "{after}");
    assert!(after["model"].is_null(), "{after}");
}

// ---- agent pair: {provider, model} (keys rework, issue #2306, slice 3a) ----

/// A pin naming a provider this company does not have, one that is
/// switched off, and a provider with no model at all are each refused
/// with a distinct 400 before anything is written.
#[tokio::test]
async fn pinning_a_built_in_agent_needs_an_existing_enabled_provider() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    seed_provider(&state, "anthropic", true).await;
    seed_provider(&state, "groq", false).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, refusal) =
        patch_agent(&state, &jamie, json!({"provider": "nope", "model": "m"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");
    assert!(
        refusal["error"]
            .as_str()
            .unwrap_or_default()
            .contains("nope"),
        "{refusal}"
    );

    let (status, refusal) =
        patch_agent(&state, &jamie, json!({"provider": "groq", "model": "m"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");
    assert!(
        refusal["error"]
            .as_str()
            .unwrap_or_default()
            .contains("switched off"),
        "{refusal}"
    );

    let (status, refusal) = patch_agent(&state, &jamie, json!({"provider": "anthropic"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");
    assert!(
        refusal["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Choose a model"),
        "{refusal}"
    );
}

/// The happy path: both halves save together and read back together, for
/// both an overlay teammate and a manifest one. That saving one moves the
/// overlay/override fingerprint is `mod.rs`'s own
/// `a_provider_edit_moves_the_overlay_and_override_fingerprints` — this
/// route's test fixture wires no `HarnessPool` (like its
/// `harness`/`model` siblings above), so it stays scoped to the route's
/// own read/write contract.
#[tokio::test]
async fn pinning_saves_both_and_rebuilds() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    seed_provider(&state, "anthropic", true).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, set) = patch_agent(
        &state,
        &jamie,
        json!({"provider": "anthropic", "model": "test-model-small"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["provider"], "anthropic");
    assert_eq!(set["model"], "test-model-small");

    let (_, reread) = get_agent(&state, &jamie).await;
    assert_eq!(reread["provider"], "anthropic", "{reread}");
    assert_eq!(reread["model"], "test-model-small", "{reread}");

    // Same for a manifest teammate.
    let (status, set) = patch_agent(
        &state,
        "ceo",
        json!({"provider": "anthropic", "model": "test-model-large"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    let (_, reread) = get_agent(&state, "ceo").await;
    assert_eq!(reread["provider"], "anthropic", "{reread}");
    assert_eq!(reread["model"], "test-model-large", "{reread}");
}

/// Clearing both halves returns the teammate to the company default —
/// the DTO reports both absent, matching `model`'s existing clear
/// contract.
#[tokio::test]
async fn clearing_the_pin_returns_to_the_default() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    seed_provider(&state, "anthropic", true).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    patch_agent(
        &state,
        &jamie,
        json!({"provider": "anthropic", "model": "test-model-small"}),
    )
    .await;

    let (status, cleared) =
        patch_agent(&state, &jamie, json!({"provider": null, "model": null})).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert!(cleared["provider"].is_null(), "{cleared}");
    assert!(cleared["model"].is_null(), "{cleared}");
}

/// `provider`/`model` are admin-gated exactly like `tools`/`harness` —
/// same 403 a member meets for those.
#[tokio::test]
async fn a_member_cannot_pin() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    seed_provider(&state, "anthropic", true).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, refusal) = send_as(
        &state,
        "PATCH",
        &format!("/api/v1/company/team/{jamie}"),
        Some(json!({"provider": "anthropic", "model": "x"})),
        crate::server::test_support::member_cookie("acme"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refusal}");
}

/// An ACP agent brings its own credential — a provider is refused
/// outright, independent of whether `model` is also sent.
#[tokio::test]
async fn an_acp_agent_rejects_a_provider() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ACP_ROSTER).await;
    seed_provider(&state, "anthropic", true).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, refusal) = patch_agent(
        &state,
        &jamie,
        json!({"provider": "anthropic", "model": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");
    assert!(
        refusal["error"]
            .as_str()
            .unwrap_or_default()
            .contains("ACP harness"),
        "{refusal}"
    );
}

/// A pin is validated against the provider list only when the request
/// actually touches the pair or the harness binding — a name-only edit
/// must not 400 because the provider was switched off since the pin was
/// saved. `resolve_for_turn`'s own fail-closed check is what catches a
/// pin that goes bad after being saved, at turn time.
#[tokio::test]
async fn a_name_edit_does_not_revalidate_a_disabled_pin() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    seed_provider(&state, "anthropic", true).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, set) = patch_agent(
        &state,
        &jamie,
        json!({"provider": "anthropic", "model": "test-model-small"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");

    seed_provider(&state, "anthropic", false).await;

    let (status, renamed) = patch_agent(&state, &jamie, json!({"name": "Jamie R."})).await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["provider"], "anthropic", "{renamed}");
}

/// `provider` is offered in `editable` to an admin only — the same rule
/// `model`/`harness`/`tools` already follow.
#[tokio::test]
async fn the_agent_detail_editable_list_offers_provider_to_an_admin_only() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (_, as_admin) = get_agent(&state, &jamie).await;
    assert!(
        strings(&as_admin["editable"]).contains(&"provider".to_string()),
        "{as_admin}"
    );

    let (_, as_member) = send_as(
        &state,
        "GET",
        &format!("/api/v1/company/team/{jamie}"),
        None,
        crate::server::test_support::member_cookie("acme"),
    )
    .await;
    assert!(
        !strings(&as_member["editable"]).contains(&"provider".to_string()),
        "{as_member}"
    );
}

/// Resetting instructions must not take the harness and model with it.
///
/// `clear_agent_override` drops an override row once nothing is left in
/// it, and its retention predicate named only the fields that existed when
/// it was written — so for a teammate whose row held instructions plus a
/// harness, clearing the first deleted the row and silently reverted the
/// second to the blueprint.
#[tokio::test]
async fn clearing_instructions_leaves_the_harness_binding_alone() {
    let home_dir = home();
    const TOML: &str = r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

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

    let (status, _) = patch_agent(
        &state,
        "ceo",
        json!({"harness": "laptop", "instructions": "Be brief."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = patch_agent(&state, "ceo", json!({"instructions": null})).await;
    assert_eq!(status, StatusCode::OK);

    let (_, after) = get_agent(&state, "ceo").await;
    assert_eq!(
        after["harness"], "laptop",
        "clearing one override field must not discard the others: {after}"
    );
}

/// A `runner` harness is `kind = "acp"` and still cannot carry a model —
/// its wire protocol has no field for one. `CompanyManifest::validate`
/// already refuses the combination, so accepting it here let the API store
/// a binding a manifest may not declare and that could never take effect.
#[tokio::test]
async fn a_model_is_refused_on_a_runner_bound_harness() {
    let home_dir = home();
    const TOML: &str = r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[harness]]
id = "main"
kind = "built_in"
default = true

[[harness]]
id = "shared"
kind = "acp"

[harness.acp]
transport = "runner"
runner = "build-box"
agent = "claude"
"#;
    let state = state_with_manifest(home_dir.path(), TOML).await;

    let (status, refused) = patch_agent(
        &state,
        "ceo",
        json!({"harness": "shared", "model": "claude-opus-4-5"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused.to_string().contains("runner"),
        "the refusal names the reason: {refused}"
    );
}

/// PR #1875 review finding (CodeRabbit): `edit_agent` drops
/// `company_write_lock` before calling into `rebuild_company` when a
/// harness/model edit needs one — `rebuild_company` now takes that same
/// non-reentrant lock itself, so this task still holding it across the
/// call would deadlock the request against its own rebuild. Nothing
/// proved that until this test; proven the same way
/// `rebuild_company_serializes_against_the_company_write_lock`
/// (`src/runtime/rebuild.rs`) proves the equivalent property one layer
/// down: hold the lock externally, drive the real request through the
/// router, and demand it completes only once the lock is released.
#[tokio::test]
async fn edit_agent_does_not_deadlock_against_its_own_rebuild() {
    struct AlwaysRebuilds {
        home: std::path::PathBuf,
    }

    #[async_trait::async_trait]
    impl crate::runtime::RuntimeRebuilder for AlwaysRebuilds {
        async fn rebuild(
            &self,
            _state: &AppState,
            request: crate::runtime::RebuildRequest,
        ) -> crate::Result<crate::company::runtime::CompanyRuntime> {
            RuntimeBuilder::new(self.home.clone(), request.manifest)
                .with_id(request.id)
                .with_handover(request.handover)
                .build()
                .await
        }
    }

    let home_dir = home();
    const TOML: &str = r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[harness]]
id = "laptop"
kind = "acp"
default = true

[harness.acp]
transport = "local"
agent = "claude"
"#;
    let state = state_with_manifest(home_dir.path(), TOML)
        .await
        .with_rebuilder(std::sync::Arc::new(AlwaysRebuilds {
            home: home_dir.path().to_path_buf(),
        }));

    let lock = company_write_lock(&CompanyId::new("acme"));
    let guard = lock.lock().await;

    let state_for_task = state.clone();
    let mut task = tokio::spawn(async move {
        patch_agent(&state_for_task, "ceo", json!({"model": "claude-opus-4-5"})).await
    });

    // The request must be blocked behind the held lock — give it every
    // chance to (wrongly) race ahead before declaring it stuck.
    let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
        .await
        .is_ok();
    assert!(
        !raced_ahead,
        "edit_agent completed while company_write_lock was held elsewhere — it is not \
         serializing its save against a concurrent writer"
    );

    drop(guard);
    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect(
            "edit_agent never resumed after the lock was released — it deadlocked against \
             its own rebuild_company call",
        )
        .expect("task panicked");
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// **Review of #745.** An unknown id answers the same way whether or not
/// the body carries `tools`.
///
/// The invariant, stated independently of which ordering is "right": one
/// route must not give two answers about whether a teammate exists,
/// decided by an unrelated field. Putting the conditional admin check
/// before the existence lookup did exactly that — `{"name": "x"}` on an
/// unknown id returned `404` while `{"tools": […]}` on the same id
/// returned `403`.
///
/// Driven as a **member**, because that is the only actor for whom the two
/// orderings differ: an admin passes the check either way and would see
/// `404` regardless, so a test written as an admin would pass against the
/// broken ordering too.
#[tokio::test]
async fn an_unknown_teammate_is_a_404_whether_or_not_tools_are_sent() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;

    let uri = "/api/v1/company/team/nobody";
    let member = || crate::server::test_support::member_cookie("acme");

    let (without_tools, _) = send_as(
        &state,
        "PATCH",
        uri,
        Some(json!({"role": "Ghost"})),
        member(),
    )
    .await;
    let (with_tools, _) = send_as(
        &state,
        "PATCH",
        uri,
        Some(json!({"tools": ["workspace"]})),
        member(),
    )
    .await;

    assert_eq!(
        with_tools, without_tools,
        "an unrelated field must not change whether a teammate is reported \
         as existing"
    );
    assert_eq!(
        with_tools,
        StatusCode::NOT_FOUND,
        "and the shared answer is 404: existence is already readable by any \
         member through GET, so 403-first would hide nothing"
    );
}

/// The same identity-before-validation rule, applied to the slowest path
/// the body can take: an unknown id with a malformed `blob:` avatar is a
/// `404`, not a `400`. The roster check has to run before the referent is
/// resolved — which can otherwise cost up to 4 MiB of workspace I/O for an
/// id nobody could have edited anyway.
#[tokio::test]
async fn an_unknown_teammate_is_a_404_even_when_the_avatar_is_malformed() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;

    let (status, _) = send_as(
        &state,
        "PATCH",
        "/api/v1/company/team/nobody",
        // Would be a `400` on its own — `blob:` node ids allow neither
        // spaces nor `!` — but the id answers `404` before the body is
        // ever judged.
        Some(json!({"avatar": "blob:not a node id!"})),
        crate::server::test_support::member_cookie("acme"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The conditional check must not take an existing capability away: a
/// member editing a name or a role keeps working exactly as before, which
/// is the same rule `POST …/team` applies to its budget cap.
#[tokio::test]
async fn a_member_may_still_edit_a_teammates_name_and_role() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, edited) = send_as(
        &state,
        "PATCH",
        &format!("/api/v1/company/team/{jamie}"),
        Some(json!({"name": "Jamie R", "role": "Head of Growth"})),
        crate::server::test_support::member_cookie("acme"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["name"], "Jamie R", "{edited}");
    assert_eq!(edited["role"], "Head of Growth", "{edited}");
}

/// `editable` is the host stating the rule so the console does not
/// re-derive it. It therefore has to answer per **actor**, or a member is
/// offered a `tools` field whose save is a `403` — the drift this list
/// exists to remove.
#[tokio::test]
async fn editable_names_tools_only_for_an_admin() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (_, as_admin) = get_agent(&state, &jamie).await;
    assert_eq!(
        strings(&as_admin["editable"]),
        vec![
            "name",
            "role",
            "description",
            "tools",
            "instructions",
            "avatar",
            "mascotMode",
            "mascotCostume",
            "mascotSkinColor",
            "mascotHandColor",
            "model",
            "harness",
            "provider"
        ],
        "{as_admin}"
    );

    let (_, as_member) = send_as(
        &state,
        "GET",
        &format!("/api/v1/company/team/{jamie}"),
        None,
        crate::server::test_support::member_cookie("acme"),
    )
    .await;
    assert_eq!(
        strings(&as_member["editable"]),
        vec![
            "name",
            "role",
            "description",
            "instructions",
            "avatar",
            "mascotMode",
            "mascotCostume",
            "mascotSkinColor",
            "mascotHandColor"
        ],
        "a member is not offered a field they cannot save — but a face (and its \
         mascot mode/costume/colors) is not one of those: picking a colleague's \
         icon is no privilege boundary, and `tools`, `model` and `harness` stay \
         admin-gated: {as_member}"
    );
}

/// A blank glob is refused rather than stored: `""` matches nothing an
/// operator meant, so it would read as a scope that grants nothing while
/// looking like a scope that was set.
#[tokio::test]
async fn a_blank_tool_glob_is_refused() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, refusal) =
        patch_agent(&state, &jamie, json!({"tools": ["workspace", "  "]})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");

    let (_, unchanged) = get_agent(&state, &jamie).await;
    assert!(
        unchanged["tools"]["requested"].is_null(),
        "and nothing was written: {unchanged}"
    );
}
