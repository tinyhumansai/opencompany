use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use super::team_agent_test_support::*;
use crate::server::router;

/// The authority line this route draws (`docs/modules/server/authority.md`):
/// a member may pick a colleague's face — it decides nothing about what the
/// company reaches the world as — while `tools` stays admin-only. Verified
/// as a member specifically, because a rule checked only as an admin passes
/// identically against no rule at all.
#[tokio::test]
async fn a_member_may_change_a_face_but_still_not_a_tool_grant() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;

    let (status, worn) = send_as(
        &state,
        "PATCH",
        "/api/v1/company/team/ceo",
        Some(json!({"avatar": "tiny:clay"})),
        crate::server::test_support::member_cookie("acme"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{worn}");
    assert_eq!(worn["avatar"], "tiny:clay", "{worn}");

    let (status, refused) = send_as(
        &state,
        "PATCH",
        "/api/v1/company/team/ceo",
        Some(json!({"tools": ["docs.*"]})),
        crate::server::test_support::member_cookie("acme"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a grant is still admin-only: {refused}"
    );
}

/// A `blob:` reference is just a node id, and any member can type one.
/// Pointing it at nothing — or at a prose note — is refused on the request
/// that asked for it, rather than becoming a broken image on every surface.
#[tokio::test]
async fn a_blob_reference_must_point_at_an_image() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, refused) =
        patch_agent(&state, "ceo", json!({"avatar": "blob:01NOSUCHNODE"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    // A real node that holds prose rather than bytes.
    let (status, note) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "notes.md", "kind": "file", "content": "hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{note}");
    let id = note["id"].as_str().expect("a node id");
    let (status, refused) =
        patch_agent(&state, "ceo", json!({"avatar": format!("blob:{id}")})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
}

/// The gap between the avatar route and the generic workspace upload: a
/// `blob:` reference must be judged on the bytes, not on the type an upload
/// declared. A non-image binary uploaded through the workspace route with
/// an `image/png` label is stored under that declared type, so a referent
/// check that believed it would let arbitrary or oversized bytes ride every
/// avatar surface. The reference is refused instead.
#[tokio::test]
async fn a_blob_reference_is_refused_when_the_bytes_are_not_an_image() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    // A PDF labelled `image/png` — stored as a binary node whose declared
    // type is exactly the claim the referent check must not trust.
    let (status, uploaded) =
        upload_workspace_binary(&state, "face.png", "image/png", b"%PDF-1.7 not an image").await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let id = uploaded["id"].as_str().expect("a node id");

    let (status, refused) =
        patch_agent(&state, "ceo", json!({"avatar": format!("blob:{id}")})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
}

/// The same decompression bomb, reached through a hand-typed `blob:`
/// reference instead of the upload route: a node whose bytes are a real
/// image by signature but a 65535×65535 header is refused on the request
/// that named it, so a member cannot park it in the workspace and point
/// every avatar surface at it.
#[tokio::test]
async fn a_blob_reference_is_refused_when_the_bytes_are_a_decompression_bomb() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, uploaded) =
        upload_workspace_binary(&state, "bomb.png", "image/png", &bomb_png()).await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let id = uploaded["id"].as_str().expect("a node id");

    let (status, refused) =
        patch_agent(&state, "ceo", json!({"avatar": format!("blob:{id}")})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
}

/// A real image uploaded through the generic workspace route is accepted
/// as a face when its declared type matches what its bytes sniff as. This
/// is what keeps a face pickable from the Files tab — and the face is then
/// served from an **immutable copy** under `avatars/`, never from the
/// Files-tab node itself, whose bytes a later republish could rewrite
/// without ever passing the avatar checks again.
#[tokio::test]
async fn a_blob_reference_is_accepted_when_the_bytes_are_an_image() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, uploaded) =
        upload_workspace_binary(&state, "face.gif", "image/gif", TINY_GIF).await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let id = uploaded["id"].as_str().expect("a node id");

    let (status, worn) = patch_agent(&state, "ceo", json!({"avatar": format!("blob:{id}")})).await;
    assert_eq!(status, StatusCode::OK, "{worn}");
    let reference = worn["avatar"].as_str().expect("a reference");
    let copy_id = reference
        .strip_prefix("blob:")
        .expect("the stored face is a blob reference");
    assert_ne!(
        copy_id, id,
        "a Files-tab node is mutable; the face must be an immutable copy"
    );

    // And the copy really holds the uploaded bytes, served from the
    // workspace blob route the console draws faces through.
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/company/workspace/blob/{copy_id}"))
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(bytes, TINY_GIF, "the copy must serve the validated bytes");
}

/// The declared type is a claim, and the claim has to match the bytes: the
/// same GIF labelled `image/png` is refused, because accepting it would let
/// the same bytes render as one type from the avatar's own path and as
/// another from the Files tab.
#[tokio::test]
async fn a_blob_reference_is_refused_when_the_declared_type_does_not_match() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (status, uploaded) =
        upload_workspace_binary(&state, "face.png", "image/png", TINY_GIF).await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let id = uploaded["id"].as_str().expect("a node id");

    let (status, refused) =
        patch_agent(&state, "ceo", json!({"avatar": format!("blob:{id}")})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
}

/// Issue #1530: an overlay teammate's persona is editable the same way. It
/// has no manifest `prompt`, so `blueprintInstructions` is absent and a reset
/// falls all the way to nothing.
#[tokio::test]
async fn instructions_are_editable_on_an_overlay_teammate() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, edited) = patch_agent(
        &state,
        &jamie,
        json!({"instructions": "Be terse and data-first."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(
        edited["instructions"], "Be terse and data-first.",
        "{edited}"
    );
    assert_eq!(edited["instructionsOverridden"], true, "{edited}");
    assert!(
        edited["blueprintInstructions"].is_null(),
        "an overlay teammate has no manifest seed to reset to: {edited}"
    );

    let (status, reset) = patch_agent(&state, &jamie, json!({"instructions": null})).await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    assert!(
        reset["instructions"].is_null(),
        "clearing an overlay override leaves no persona text: {reset}"
    );
    assert_eq!(reset["instructionsOverridden"], false, "{reset}");
}

/// Review (PR #1549): an oversized `instructions` write is capped to the
/// prompt budget rather than stored verbatim, so a single pasted
/// "AGENT.md"-style document cannot unboundedly inflate every turn's
/// persona prompt. The leading portion is kept and the cut is marked.
#[tokio::test]
async fn overlong_instructions_are_capped_at_the_write_boundary() {
    use crate::company::PROMPT_FILE_BUDGET_CHARS;

    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let over: String = "x".repeat(PROMPT_FILE_BUDGET_CHARS + 40);
    let (status, edited) = patch_agent(&state, &jamie, json!({"instructions": over})).await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    let stored = edited["instructions"].as_str().unwrap();
    assert!(
        stored.starts_with(&"x".repeat(PROMPT_FILE_BUDGET_CHARS)),
        "the leading portion is kept: {:?}",
        &stored[..64.min(stored.len())]
    );
    assert!(
        stored.contains("truncated"),
        "an overlong override is marked as cut"
    );
    assert!(
        stored.chars().count() <= PROMPT_FILE_BUDGET_CHARS + 40,
        "capped text stays bounded: {}",
        stored.chars().count()
    );
}

/// The console renders read-only from the host's answer, not from a rule of
/// its own — so this list is part of the contract.
#[tokio::test]
async fn the_host_states_which_fields_are_editable() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (_, agent) = get_agent(&state, &jamie).await;
    assert_eq!(
        strings(&agent["editable"]),
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
        "{agent}"
    );
}

/// Issue #619: a teammate can be narrowed **after** it exists, not only at
/// creation.
///
/// #661 made the scope writable on `POST …/team` and through `add_agent`.
/// This is the half that was missing — without it, correcting a teammate's
/// grant means deleting and recreating it, which orphans its workspace
/// folder, budget row, desk memberships and inbox.
///
/// The three levels are asserted separately on purpose: `requested` proves
/// the scope was stored, `effective` proves it reached the function the
/// harness builds the agent with, and the untouched company `allow` proves
/// the narrowing is per-teammate rather than a company-wide edit.
#[tokio::test]
async fn an_overlay_teammate_can_be_scoped_after_creation() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (_, before) = get_agent(&state, &jamie).await;
    assert!(
        before["tools"]["requested"].is_null(),
        "unscoped to begin with: {before}"
    );
    assert_eq!(
        strings(&before["tools"]["effective"]),
        vec!["workspace", "workspace.*", "composio"],
        "which resolves to everything the company allows: {before}"
    );

    let (status, scoped) = patch_agent(&state, &jamie, json!({"tools": ["workspace"]})).await;
    assert_eq!(status, StatusCode::OK, "{scoped}");
    assert_eq!(
        strings(&scoped["tools"]["requested"]),
        vec!["workspace"],
        "{scoped}"
    );
    assert_eq!(
        strings(&scoped["tools"]["effective"]),
        vec!["workspace"],
        "and it is narrower than the company grant, which is the point: {scoped}"
    );
    assert_eq!(
        strings(&scoped["tools"]["companyAllow"]),
        vec!["workspace", "workspace.*", "composio"],
        "the company ceiling is untouched — this scoped one teammate: {scoped}"
    );

    // Read back through a fresh request, so this is the stored record and
    // not the handler's own answer.
    let (_, reread) = get_agent(&state, &jamie).await;
    assert_eq!(
        strings(&reread["tools"]["requested"]),
        vec!["workspace"],
        "{reread}"
    );

    // Since #1804 an explicit empty list is a deliberate deny-all, NOT the
    // way back to the standard grant: it stores `[]` (not null) and must
    // read as "holds nothing".
    let (status, denied) = patch_agent(&state, &jamie, json!({"tools": []})).await;
    assert_eq!(status, StatusCode::OK, "{denied}");
    assert_eq!(
        strings(&denied["tools"]["requested"]),
        Vec::<String>::new(),
        "an explicit empty list stores an empty (deny-all) grant, not null: {denied}"
    );
    assert!(
        strings(&denied["tools"]["effective"]).is_empty(),
        "a deny-all teammate holds nothing: {denied}"
    );

    // `null` is the deliberate way back to the standard grant, and must read
    // as "inherits everything" (requested null) rather than "holds nothing".
    let (status, cleared) = patch_agent(&state, &jamie, json!({"tools": null})).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert!(cleared["tools"]["requested"].is_null(), "{cleared}");
    assert_eq!(
        strings(&cleared["tools"]["effective"]),
        vec!["workspace", "workspace.*", "composio"],
        "{cleared}"
    );
}

/// **The review finding (#745).** A member must not be able to widen a
/// teammate's scope — and since #1804 the widest possible widening is
/// `{"tools": null}`, the reset back to the company's standard grant. (An
/// empty list `{"tools": []}` is now a deny-all, the *narrowest* scope, but
/// it is equally admin-only: every `tools` edit is gated, whichever state.)
///
/// This is #619's own defect reachable through the route added to fix it:
/// resetting to the standard grant inherits everything, and leaving
/// `edit_agent` member-open would have let any signed-in member undo any
/// scoping with one call.
///
/// The two-account shape is the point: the harness signs every other
/// request in as an admin, so a check verified only as an admin passes
/// identically against no check at all.
#[tokio::test]
async fn a_member_cannot_widen_a_teammates_scope() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    // Scoped by an admin.
    let (status, _) = patch_agent(&state, &jamie, json!({"tools": ["workspace"]})).await;
    assert_eq!(status, StatusCode::OK);

    let uri = format!("/api/v1/company/team/{jamie}");
    let member = || crate::server::test_support::member_cookie("acme");

    // The widening a member must not be able to perform: `null` resets to
    // the company's whole standard grant.
    let (status, refusal) = send_as(
        &state,
        "PATCH",
        &uri,
        Some(json!({"tools": null})),
        member(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "resetting to null is the company's whole grant: {refusal}"
    );

    // …and neither may a member set a different scope at all.
    let (status, _) = send_as(
        &state,
        "PATCH",
        &uri,
        Some(json!({"tools": ["composio"]})),
        member(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Nothing was written by either attempt.
    let (_, unchanged) = get_agent(&state, &jamie).await;
    assert_eq!(
        strings(&unchanged["tools"]["requested"]),
        vec!["workspace"],
        "the scope an admin set must survive both refusals: {unchanged}"
    );
}

/// Issue #1245's per-agent follow-up: an admin can set and clear a
/// teammate's own model override, and a member meets the same `403` this
/// module already enforces for `tools` — the two fields share the
/// "cost/scope decision" character `edit_agent`'s own docs give for why
/// `tools` is admin-only.
///
/// `ACP_ROSTER`, not `ROSTER`: the fresh overlay teammate lands on
/// whichever harness is `default = true`, and a model only means
/// anything there when that harness is `acp` — see the cross-field
/// rejection test below for the `built_in` case this deliberately avoids.
#[tokio::test]
async fn an_admin_can_set_and_clear_a_teammates_model_override() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ACP_ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    // Undeclared until set.
    let (_, before) = get_agent(&state, &jamie).await;
    assert!(before["model"].is_null(), "{before}");

    // A member may not set one.
    let (status, refusal) = send_as(
        &state,
        "PATCH",
        &format!("/api/v1/company/team/{jamie}"),
        Some(json!({"model": "claude-opus-4-5"})),
        crate::server::test_support::member_cookie("acme"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refusal}");

    // An admin may.
    let (status, set) = patch_agent(&state, &jamie, json!({"model": "claude-opus-4-5"})).await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["model"], "claude-opus-4-5");

    let (_, reread) = get_agent(&state, &jamie).await;
    assert_eq!(reread["model"], "claude-opus-4-5", "{reread}");

    // `null` clears it back to the harness's own default.
    let (status, cleared) = patch_agent(&state, &jamie, json!({"model": null})).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert!(cleared["model"].is_null(), "{cleared}");
}

/// Issue #1245's harness-picker follow-up: a model override is refused
/// outright when the teammate's harness (here, the implicit `built_in`
/// default `ROSTER` never overrides) has no ACP transport to forward it
/// to — the overlay-write mirror of `CompanyManifest::validate`'s
/// identical rule for a manifest agent's own `model`.
#[tokio::test]
async fn a_model_override_is_refused_off_an_acp_harness() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, refusal) = patch_agent(&state, &jamie, json!({"model": "claude-opus-4-5"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");

    let (_, unchanged) = get_agent(&state, &jamie).await;
    assert!(unchanged["model"].is_null(), "{unchanged}");
}

/// Issue #1245's harness-picker follow-up: a teammate's harness binding
/// is admin-only (same gate as `model`/`tools`), validated against the
/// company's own declared set, and clears back to the default with
/// `null` — the same three behaviours the model test above proves for
/// `model`, on the sibling field.
#[tokio::test]
async fn an_admin_can_pin_and_clear_a_teammates_harness() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ACP_ROSTER).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    // Undeclared until set — this teammate is on the default (`laptop`)
    // implicitly, not by naming it.
    let (_, before) = get_agent(&state, &jamie).await;
    assert!(before["harness"].is_null(), "{before}");

    // A member may not set one.
    let (status, refusal) = send_as(
        &state,
        "PATCH",
        &format!("/api/v1/company/team/{jamie}"),
        Some(json!({"harness": "main"})),
        crate::server::test_support::member_cookie("acme"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refusal}");

    // An unknown id is refused, not silently accepted into a binding
    // that would orphan the teammate from every harness's serve set.
    let (status, refusal) = patch_agent(&state, &jamie, json!({"harness": "does-not-exist"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");

    // An admin may pin it to a declared harness.
    let (status, set) = patch_agent(&state, &jamie, json!({"harness": "main"})).await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["harness"], "main");

    let (_, reread) = get_agent(&state, &jamie).await;
    assert_eq!(reread["harness"], "main", "{reread}");

    // `null` clears it back to the declared default.
    let (status, cleared) = patch_agent(&state, &jamie, json!({"harness": null})).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert!(cleared["harness"].is_null(), "{cleared}");
}

/// A coding CLI this build drives is bindable without any `[[harness]]`
/// naming it — but only where this host can actually run one (issue
/// #1245's detected-harness follow-up).
///
/// `harness_by_id` resolves an `ACP_AGENTS` id on any build through the
/// implicit-local fallback, so without this gate a hosted admin could bind
/// a teammate to a CLI the server has nothing to launch — accepted by
/// `PATCH`, then dead on the next rebuild. The picker (`GET
/// {scope}/harnesses`) refuses to offer such CLIs; the write path must
/// agree, and this test is what holds the two together.
///
/// Issue #1814: "can run one" is `can_run_local_acp()`, not "a factory was
/// wired". The desktop wires one even when compiled without `acp`, where
/// nothing can be built from it — so the wired-factory half below expects
/// a refusal in that configuration, matching the picker.
#[tokio::test]
async fn an_undeclared_coding_cli_is_bindable_only_where_this_host_can_run_one() {
    struct StubFactory;
    impl crate::ports::acp::AcpAgentFactory for StubFactory {
        fn build(
            &self,
            _agent: &str,
            _model: Option<&str>,
            _agent_models: &std::collections::HashMap<String, String>,
            _workspace_root: &std::path::Path,
        ) -> crate::Result<std::sync::Arc<dyn crate::ports::acp::AcpAgent>> {
            unreachable!("this route never builds an agent")
        }
    }

    // Hosted shape (no factory): an undeclared coding CLI is refused, just
    // as the picker that does not offer it.
    let hosted_home = home();
    let hosted = state_with_manifest(hosted_home.path(), ACP_ROSTER).await;
    let jamie = add_overlay(&hosted, "Jamie", "Growth").await;
    let (status, refusal) = patch_agent(&hosted, &jamie, json!({"harness": "claude"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");

    // Desktop shape (factory wired): bindable only where this build can
    // actually build an engine from that factory (issue #1814).
    let desktop_home = home();
    let desktop = state_with_manifest(desktop_home.path(), ACP_ROSTER)
        .await
        .with_acp_agents(std::sync::Arc::new(StubFactory));
    let jamie = add_overlay(&desktop, "Jamie", "Growth").await;
    let (status, set) = patch_agent(&desktop, &jamie, json!({"harness": "claude"})).await;
    if cfg!(feature = "acp") {
        assert_eq!(status, StatusCode::OK, "{set}");
        assert_eq!(set["harness"], "claude");
    } else {
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a build without `acp` cannot run `claude`, so the write path \
             must refuse it exactly as the picker declines to offer it: {set}"
        );
    }

    // A factory must not widen the vocabulary beyond the coding CLIs.
    let (status, refusal) = patch_agent(&desktop, &jamie, json!({"harness": "not-a-cli"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");
}

/// Issue #1245's harness-picker follow-up: switching a teammate onto an
/// ACP harness and setting its model happen in the same `PATCH` in the
/// console's own edit flow, so the cross-field check has to validate
/// against the *new* binding, not the stale one — this is the case that
/// would wrongly 400 if it read `declared_harness` unconditionally
/// instead of preferring the harness this same request also sent.
#[tokio::test]
async fn harness_and_model_can_be_set_together_against_the_new_binding() {
    let home_dir = home();
    // `main` (`built_in`) is default here — the opposite of `ACP_ROSTER`
    // — so a model alone would be refused, and only succeeds because
    // this request also moves the teammate onto `laptop` in the same call.
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
    let jamie = add_overlay(&state, "Jamie", "Growth").await;

    let (status, set) = patch_agent(
        &state,
        &jamie,
        json!({"harness": "laptop", "model": "claude-opus-4-5"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["harness"], "laptop");
    assert_eq!(set["model"], "claude-opus-4-5");
}
