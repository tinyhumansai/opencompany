use super::*;

/// HTTP-level coverage of the two path-slug handlers. A slug that fails
/// `validate_slug` must be rejected with `400` **before** any write, so the
/// effective skill set is untouched; a valid slug succeeds and lands.
///
/// `..` and `a/b` cannot be carried as a single path segment (a `/` splits
/// them, and `..` is normalized away by the router), so their rejection is
/// pinned in [`validate_slug_rejects_traversal_separator_case_and_length`].
/// `A` is a
/// single segment the router will pass through, so it is the shape we drive
/// through the handlers to prove the `400` and the no-mutation guarantee.
mod http {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::company::skill_validate::{MAX_SLUG_CHARS, validate_slug};
    use crate::ports::CompanyStore;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::runtime::RuntimeBuilder;
    use crate::server::ops::skills::{MAX_SKILL_DOC_BYTES, write_lock};
    use crate::server::router;
    use crate::server::test_support::{
        fixed_cookie, member_cookie, seed_fixed_admin, seed_fixed_member,
    };
    use crate::{AppConfig, AppState};

    async fn state_with_company(home: &std::path::Path) -> AppState {
        let id = CompanyId::new("acme");
        let manifest: CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
        crate::store::FsCompanyStore::new(home.to_path_buf())
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
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        seed_fixed_admin(&state, "acme").await;
        state
    }

    /// Sends as the fixed admin session — the common case, since every write
    /// route here is admin-gated.
    async fn send(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> (StatusCode, Value, String) {
        send_as(state, method, uri, body, Some(&fixed_cookie("acme"))).await
    }

    /// [`send`], with the caller's cookie explicit — `None` for no session at
    /// all, so the privilege boundary can be driven with an admin session, a
    /// member session, or nothing.
    async fn send_as(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<&str>,
        cookie: Option<&str>,
    ) -> (StatusCode, Value, String) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let raw = String::from_utf8_lossy(&bytes).to_string();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value, raw)
    }

    /// The effective skill set, as the console reads it.
    async fn slugs(state: &AppState) -> Vec<String> {
        let (status, value, raw) = send(state, "GET", "/api/v1/company/skills", None).await;
        assert_eq!(status, StatusCode::OK, "list skills: {raw}");
        value
            .as_array()
            .expect("skills list is an array")
            .iter()
            .map(|s| s["id"].as_str().expect("an id").to_string())
            .collect()
    }

    /// Both write handlers reject an invalid slug with `400` and leave the
    /// effective skill set untouched; a valid slug then succeeds and lands.
    #[tokio::test]
    async fn invalid_slugs_are_400_and_leave_state_unchanged() {
        let home = tempfile::tempdir().unwrap();
        let state = state_with_company(home.path()).await;

        let before = slugs(&state).await;

        // `install` rejects the uppercase slug without writing.
        let (status, _, raw) = send(&state, "POST", "/api/v1/company/skills/A/install", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "install A: {raw}");
        assert!(
            raw.contains("not a valid skill slug"),
            "the 400 explains why: {raw}"
        );

        // `set_enabled` rejects the same slug without writing.
        let (status, _, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/skills/A",
            Some(r#"{"enabled":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "set_enabled A: {raw}");

        // Neither attempt mutated the effective set.
        assert_eq!(
            slugs(&state).await,
            before,
            "a rejected slug must not land a delta"
        );

        // A valid slug succeeds on both handlers and does land.
        let (status, _, raw) =
            send(&state, "POST", "/api/v1/company/skills/a-1/install", None).await;
        assert_eq!(status, StatusCode::OK, "install a-1: {raw}");

        let (status, _, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/skills/a-1",
            Some(r#"{"enabled":false}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "set_enabled a-1: {raw}");

        assert!(
            slugs(&state).await.iter().any(|s| s == "a-1"),
            "the valid slug lands in the effective set"
        );
    }

    /// Every write route here decides something for the whole company (see
    /// the module doc): a Member is refused exactly like an unauthenticated
    /// caller, on all four of them, and neither refusal lands a delta.
    #[tokio::test]
    async fn every_write_route_refuses_a_member_and_an_unauthenticated_caller() {
        let home = tempfile::tempdir().unwrap();
        let state = state_with_company(home.path()).await;
        seed_fixed_member(&state, "acme").await;
        let member = member_cookie("acme");
        let before = slugs(&state).await;

        let attempts: [(&str, &str, Option<&str>); 4] = [
            ("POST", "/api/v1/company/skills/seo-audit/install", None),
            (
                "PUT",
                "/api/v1/company/skills/seo-audit",
                Some(r#"{"enabled":true}"#),
            ),
            (
                "POST",
                "/api/v1/company/skills",
                Some(r#"{"name":"Member Skill","description":"a member tried this"}"#),
            ),
            ("POST", "/api/v1/company/skills/seo-audit/uninstall", None),
        ];

        for (method, uri, body) in attempts {
            let (status, resp, raw) = send_as(&state, method, uri, body, Some(&member)).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {uri} as a member: {raw}"
            );
            assert_eq!(resp["code"], "forbidden", "{method} {uri}: {raw}");

            let (status, resp, raw) = send_as(&state, method, uri, body, None).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} with no session: {raw}"
            );
            assert_eq!(resp["code"], "unauthorized", "{method} {uri}: {raw}");
        }

        assert_eq!(
            slugs(&state).await,
            before,
            "no member or unauthenticated attempt landed a delta"
        );
    }

    /// The other half of the boundary: an admin is not caught by the same
    /// gate, and every write route still does its job end to end —
    /// install, toggle, author, then uninstall the one route that allows
    /// it.
    #[tokio::test]
    async fn an_admin_can_use_every_write_route() {
        let home = tempfile::tempdir().unwrap();
        let state = state_with_company(home.path()).await;

        let (status, _, raw) = send(
            &state,
            "POST",
            "/api/v1/company/skills/seo-audit/install",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "admin install: {raw}");

        let (status, _, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/skills/seo-audit",
            Some(r#"{"enabled":false}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "admin set_enabled: {raw}");

        let (status, resp, raw) = send(
            &state,
            "POST",
            "/api/v1/company/skills",
            Some(r#"{"name":"Admin Skill","description":"authored by an admin"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "admin create_custom: {raw}");
        assert_eq!(resp["id"], "admin-skill");

        let (status, _, raw) = send(
            &state,
            "POST",
            "/api/v1/company/skills/seo-audit/uninstall",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "admin uninstall: {raw}");

        let after = slugs(&state).await;
        assert!(
            !after.iter().any(|s| s == "seo-audit"),
            "the uninstall landed: {after:?}"
        );
        assert!(
            after.iter().any(|s| s == "admin-skill"),
            "the authored skill landed: {after:?}"
        );
    }

    /// The affordance the missing rows were costing: a global reaches every
    /// agent, and the only control that can withhold it is the row's own
    /// switch. Driven over the real route, end to end.
    #[tokio::test]
    async fn a_global_can_be_disabled_and_re_enabled_through_the_put_route() {
        let home = tempfile::tempdir().unwrap();
        let state = state_with_company(home.path()).await;
        let slug = super::tests_skill_md_frontmatter_resists::global_slug();

        let listed = |state: &AppState, slug: String| {
            let state = state.clone();
            async move {
                let (status, value, raw) =
                    send(&state, "GET", "/api/v1/company/skills", None).await;
                assert_eq!(status, StatusCode::OK, "list skills: {raw}");
                value
                    .as_array()
                    .expect("an array")
                    .iter()
                    .find(|row| row["id"] == slug)
                    .unwrap_or_else(|| panic!("no `{slug}` row: {raw}"))
                    .clone()
            }
        };

        let row = listed(&state, slug.clone()).await;
        assert_eq!(row["enabled"], true, "a global starts enabled");

        let (status, _, raw) = send(
            &state,
            "PUT",
            &format!("/api/v1/company/skills/{slug}"),
            Some(r#"{"enabled":false}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "disable a global: {raw}");

        let row = listed(&state, slug.clone()).await;
        assert_eq!(row["enabled"], false, "the switch stuck");
        assert_eq!(row["source"], "company", "still not uninstallable");

        let (status, _, raw) = send(
            &state,
            "PUT",
            &format!("/api/v1/company/skills/{slug}"),
            Some(r#"{"enabled":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "re-enable a global: {raw}");
        assert_eq!(listed(&state, slug).await["enabled"], true);
    }

    /// A skill's document becomes part of every agent's effective prompt,
    /// so [`MAX_SKILL_DOC_BYTES`] is enforced on the assembled `SKILL.md`,
    /// not just accepted and truncated later — and the refusal is the same
    /// `400 invalid_request` shape every other bad-input write already
    /// uses, not a bespoke code.
    #[tokio::test]
    async fn an_over_cap_custom_skill_body_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let state = state_with_company(home.path()).await;
        let before = slugs(&state).await;

        let oversized = "x".repeat(MAX_SKILL_DOC_BYTES);
        let body = serde_json::json!({
            "name": "Huge Skill",
            "description": "short",
            "body": oversized,
        })
        .to_string();

        let (status, resp, raw) = send(&state, "POST", "/api/v1/company/skills", Some(&body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
        assert_eq!(resp["code"], "invalid_request", "{raw}");

        assert_eq!(
            slugs(&state).await,
            before,
            "an over-cap body must not land a delta"
        );
    }

    /// The property [`write_lock`] exists for, asserted where it actually
    /// has to hold: on the handlers, not on the primitive.
    ///
    /// [`write_lock_serializes_same_company_writes`](super::write_lock_serializes_same_company_writes)
    /// proves the mutex is a mutex; it passes unchanged if every handler
    /// stops taking it. This holds the addressed company's lock and drives
    /// each write route over the real router: a route that reached the
    /// store anyway answers while the lock is held, which is the whole
    /// defect — `set_enabled`'s list-then-write window is only closed
    /// while *every* writer waits on the same lock.
    #[tokio::test]
    async fn every_write_route_waits_on_the_company_write_lock() {
        let home = tempfile::tempdir().unwrap();
        let state = state_with_company(home.path()).await;

        // One uncontended write first, so everything a request lazily opens
        // on its way to the handler (session lookup, the skill store) is
        // already warm and the wait below is measuring the lock alone.
        let (status, _, raw) = send(
            &state,
            "POST",
            "/api/v1/company/skills/warm-up/install",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "warm-up install: {raw}");

        let attempts: [(&'static str, &'static str, Option<&'static str>); 4] = [
            ("POST", "/api/v1/company/skills/seo-audit/install", None),
            (
                "PUT",
                "/api/v1/company/skills/seo-audit",
                Some(r#"{"enabled":false}"#),
            ),
            (
                "POST",
                "/api/v1/company/skills",
                Some(r#"{"name":"Locked Skill","description":"waits its turn"}"#),
            ),
            ("POST", "/api/v1/company/skills/warm-up/uninstall", None),
        ];

        for (method, uri, body) in attempts {
            let lock = write_lock(&CompanyId::new("acme"));
            let guard = lock.lock().await;

            let held = state.clone();
            let mut pending = tokio::spawn(async move {
                send_as(&held, method, uri, body, Some(&fixed_cookie("acme"))).await
            });

            let ran_anyway =
                tokio::time::timeout(std::time::Duration::from_millis(750), &mut pending).await;
            assert!(
                ran_anyway.is_err(),
                "{method} {uri} reached the store while another writer held \
                 the company write lock"
            );

            drop(guard);
            let (status, _, raw) = pending.await.expect("the write task did not panic");
            assert!(
                status.is_success(),
                "{method} {uri} once the lock was free: {raw}"
            );
        }
    }

    /// Authoring derives the slug from the display name, so the name is the
    /// untrusted input that decides a store key and a `skills/<slug>/`
    /// directory name. Whatever `create_custom` accepts must therefore
    /// derive a slug the slug-bearing routes accept: an id `validate_slug`
    /// refuses is a skill nobody can toggle or uninstall afterwards, and a
    /// path segment nothing else in the product will honour.
    ///
    /// Asserted end to end — the derived id is fed straight back to
    /// `PUT …/skills/{slug}`, the route that does apply `validate_slug`.
    #[tokio::test]
    async fn an_authored_slug_is_always_one_the_slug_routes_accept() {
        let home = tempfile::tempdir().unwrap();
        let state = state_with_company(home.path()).await;

        for name in [
            "!!!",
            "  ---  ",
            "-leading dash",
            "Ünïcödé Skill",
            "42",
            "A/B\\C",
            "UPPER CASE",
            "under_score",
        ] {
            let body = serde_json::json!({
                "name": name,
                "description": "a description",
            })
            .to_string();
            let (status, resp, raw) =
                send(&state, "POST", "/api/v1/company/skills", Some(&body)).await;
            assert_eq!(status, StatusCode::OK, "authoring {name:?}: {raw}");

            let slug = resp["id"].as_str().expect("an id").to_string();
            assert!(
                validate_slug(&slug).is_ok(),
                "{name:?} derived {slug:?}, which the slug routes refuse"
            );

            let (status, _, raw) = send(
                &state,
                "PUT",
                &format!("/api/v1/company/skills/{slug}"),
                Some(r#"{"enabled":false}"#),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "the skill authored from {name:?} cannot be managed by its own id \
                 {slug:?}: {raw}"
            );
        }
    }

    /// A refusal raised *inside* the guarded region must still hand the
    /// company's write lock back.
    ///
    /// Every write handler takes the lock before it validates, so the
    /// over-cap refusal returns with the guard live. A guard that outlived
    /// its request would not fail that request — it would wedge every
    /// later write for that one company, for the life of the process, with
    /// nothing in the failed response to say so.
    #[tokio::test]
    async fn a_write_refused_inside_the_lock_still_hands_it_back() {
        let home = tempfile::tempdir().unwrap();
        let state = state_with_company(home.path()).await;

        let body = serde_json::json!({
            "name": "Huge Skill",
            "description": "short",
            "body": "x".repeat(MAX_SKILL_DOC_BYTES),
        })
        .to_string();
        let (status, _, raw) = send(&state, "POST", "/api/v1/company/skills", Some(&body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");

        let next = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            send(&state, "POST", "/api/v1/company/skills/a-1/install", None),
        )
        .await
        .expect("the refused write left the company write lock held");
        assert_eq!(next.0, StatusCode::OK, "{}", next.2);
    }

    /// Authoring never writes over a slug that is already resolving.
    ///
    /// A slug is a store key, and the store's `set` replaces whatever holds it.
    /// Two authored skills can arrive at one slug — the display names differ
    /// only past the truncation point, or contain no alphanumerics at all and
    /// both fall back to `skill` — and before this, the second write silently
    /// destroyed the first: one row left, holding the second name and body,
    /// with `200` on both requests and nothing naming the loss.
    #[tokio::test]
    async fn an_authored_skill_never_writes_over_a_slug_already_in_use() {
        let home = tempfile::tempdir().unwrap();
        let state = state_with_company(home.path()).await;

        let author = async |name: &str, description: &str| {
            let body = serde_json::json!({"name": name, "description": description}).to_string();
            let (status, resp, raw) =
                send(&state, "POST", "/api/v1/company/skills", Some(&body)).await;
            assert_eq!(status, StatusCode::OK, "authoring {name:?}: {raw}");
            resp["id"].as_str().expect("an id").to_string()
        };

        let first = author("Alpha", "The first one, which must survive.").await;
        let second = author("Alpha", "The second one, which must not replace it.").await;
        assert_ne!(first, second, "a taken slug must not be handed out twice");

        let (_, list, raw) = send(&state, "GET", "/api/v1/company/skills", None).await;
        let rows = list.as_array().expect("a list");
        let find = |id: &str| {
            rows.iter()
                .find(|row| row["id"] == id)
                .unwrap_or_else(|| panic!("{id} is gone from the list: {raw}"))
        };
        assert_eq!(
            find(&first)["description"],
            "The first one, which must survive.",
            "the first skill's document was replaced: {raw}"
        );
        assert_eq!(
            find(&second)["description"],
            "The second one, which must not replace it."
        );
        // Both display names are kept verbatim — only the slug was made unique.
        assert_eq!(find(&first)["name"], "Alpha");
        assert_eq!(find(&second)["name"], "Alpha");
    }

    /// The two ways distinct names reach one slug, each getting its own.
    ///
    /// Truncation: the names differ only past `MAX_SLUG_CHARS`. Fallback: the
    /// names carry no alphanumerics, so both slugify to `skill`. Both are
    /// reachable with names an operator can type, which is why neither is
    /// refused — the slug is made unique instead of the name rejected.
    #[tokio::test]
    async fn names_that_collide_only_after_slugification_each_get_a_slug() {
        let home = tempfile::tempdir().unwrap();
        let state = state_with_company(home.path()).await;

        let author = async |name: &str| {
            let body =
                serde_json::json!({"name": name, "description": "a description"}).to_string();
            let (status, resp, raw) =
                send(&state, "POST", "/api/v1/company/skills", Some(&body)).await;
            assert_eq!(status, StatusCode::OK, "authoring {name:?}: {raw}");
            resp["id"].as_str().expect("an id").to_string()
        };

        let long = "a".repeat(MAX_SLUG_CHARS);
        let truncating = [
            author(&format!("{long}one")).await,
            author(&format!("{long}two")).await,
        ];
        assert_ne!(truncating[0], truncating[1], "truncated names collided");

        let falling_back = [author("!!!").await, author("  ---  ").await];
        assert_eq!(falling_back[0], "skill");
        assert_ne!(
            falling_back[1], "skill",
            "both names fell back onto one slug"
        );

        // Every derived slug still has to be one the slug-bearing routes accept.
        for slug in truncating.iter().chain(falling_back.iter()) {
            assert!(
                validate_slug(slug).is_ok(),
                "{slug:?} is not a routable slug"
            );
        }
    }
}
