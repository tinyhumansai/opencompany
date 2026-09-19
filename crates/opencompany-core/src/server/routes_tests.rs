use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

use super::*;
use crate::AppConfig;

/// Writes a minimal console tree — the shell plus one hashed asset, which
/// is the shape the cache policy distinguishes between.
fn console_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("index.html"),
        "<!doctype html><title>console</title>",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("assets")).unwrap();
    std::fs::write(
        dir.path().join("assets").join("index-abc123.js"),
        "export const x = 1;\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("pages-sdk")).unwrap();
    std::fs::write(
        dir.path().join("pages-sdk").join("react.mjs"),
        "export const createElement = () => null;\n",
    )
    .unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn cache_control(response: &axum::response::Response) -> &str {
    response
        .headers()
        .get(axum::http::header::CACHE_CONTROL)
        .expect("the console fallback stamps a cache policy on every response")
        .to_str()
        .unwrap()
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn console_serves_index_at_root() {
    let (_guard, dir) = console_fixture();
    let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_text(response).await.contains("<title>console</title>"));
}

#[tokio::test]
async fn console_falls_back_to_index_for_spa_routes() {
    let (_guard, dir) = console_fixture();
    let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/some/spa/route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_text(response).await.contains("<title>console</title>"));
}

#[tokio::test]
async fn the_shell_is_never_kept_without_revalidating() {
    let (_guard, dir) = console_fixture();
    let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

    for path in ["/", "/some/spa/route"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(cache_control(&response), "no-cache", "for {path}");
    }
}

#[tokio::test]
async fn a_hashed_asset_is_kept_forever() {
    let (_guard, dir) = console_fixture();
    let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/assets/index-abc123.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        cache_control(&response),
        "public, max-age=31536000, immutable"
    );
}

#[tokio::test]
async fn page_sdk_modules_allow_credentialed_imports_from_opaque_frames() {
    let (_guard, dir) = console_fixture();
    let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/pages-sdk/react.mjs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "null"
    );
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .unwrap(),
        "true"
    );
    assert_eq!(
        response.headers().get(axum::http::header::VARY).unwrap(),
        "Origin"
    );
}

/// The failure that made issue #979 a blank page rather than a 404: a
/// stale shell asks for a chunk this build does not have, the SPA fallback
/// answers with `index.html`, and the browser is handed HTML where it
/// expected a module. Whatever else that response is, it must not be
/// cached as though it were the immutable asset it was addressed as —
/// which is why the policy is keyed on what came back, not what was asked
/// for.
#[tokio::test]
async fn a_missing_chunk_answers_with_the_shell_and_is_not_kept() {
    let (_guard, dir) = console_fixture();
    let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/assets/KnowledgeGraph-fromlastbuild.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(cache_control(&response), "no-cache");
    assert!(body_text(response).await.contains("<title>console</title>"));
}

#[tokio::test]
async fn console_does_not_shadow_api_routes() {
    let (_guard, dir) = console_fixture();
    let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

    // A real API route still reaches the API and answers with its own auth
    // status (401), never the SPA shell.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/companies")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!body_text(response).await.contains("<title>console</title>"));
}

#[tokio::test]
async fn console_does_not_shadow_unmatched_reserved_paths() {
    let (_guard, dir) = console_fixture();
    let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

    // Paths under reserved API/discovery prefixes must never fall through
    // to the SPA shell, so API and protocol clients can distinguish "this
    // surface is absent" from "here is your JSON response". The invariant
    // is on the *property* — no HTML shell — not on the specific 4xx code,
    // which depends on whether a route is mounted at all.
    for path in [
        "/api/v1/does-not-exist",
        "/.well-known/agent-card.json",
        "/companies/acme/.well-known/agent-card.json",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();

        let status = response.status();
        let body = body_text(response).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "reserved path {path} must 404 when unmatched; got {status}, body starts: {:?}",
            body.chars().take(120).collect::<String>(),
        );
        assert!(
            !body.contains("<title>console</title>"),
            "reserved path {path} must not fall through to the SPA shell; body starts: {:?}",
            body.chars().take(120).collect::<String>(),
        );
    }

    // `/acp` is a reserved prefix in both feature states (see `RESERVED_PREFIXES`
    // in routes.rs), so the console shell must never answer it. The specific
    // status code is feature-dependent:
    //
    // - Without `acp`: no route is mounted; the fallback sees the reserved
    //   prefix and returns 404 — the honest "no ACP here" signal.
    // - With    `acp`: the route is mounted as POST-only; a GET receives 405 —
    //   the correct method-level rejection from the mounted handler.
    //
    // A 405 is still proof the console did not shadow the path, so both codes
    // satisfy the invariant (issue #1979). CI covers both arms: the default
    // `cargo test --locked` lane (no `acp`) and the ACP lane (`--features acp,…`).
    let acp_response = app
        .oneshot(Request::builder().uri("/acp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let expected_acp_status = if cfg!(feature = "acp") {
        StatusCode::METHOD_NOT_ALLOWED
    } else {
        StatusCode::NOT_FOUND
    };
    let acp_status = acp_response.status();
    let acp_body = body_text(acp_response).await;
    assert_eq!(
        acp_status,
        expected_acp_status,
        "GET /acp with feature acp={} must be {expected_acp_status}; got {acp_status}, body starts: {:?}",
        cfg!(feature = "acp"),
        acp_body.chars().take(120).collect::<String>(),
    );
    assert!(
        !acp_body.contains("<title>console</title>"),
        "GET /acp must not fall through to the SPA shell (feature acp={}); body starts: {:?}",
        cfg!(feature = "acp"),
        acp_body.chars().take(120).collect::<String>(),
    );
}

#[test]
fn reserved_path_matches_prefixes_and_subpaths_only() {
    assert!(is_reserved_path("/api"));
    assert!(is_reserved_path("/api/v1/companies"));
    // `/hooks/{companyId}/{channel}` inbound webhooks (api.md) — a
    // server-owned namespace, reserved even before the route is wired.
    assert!(is_reserved_path("/hooks/acme/slack"));
    assert!(is_reserved_path("/.well-known/agent-card.json"));
    assert!(is_reserved_path("/a2a/handle"));
    // A `.well-known` discovery URI is reserved wherever the segment sits,
    // including under a company handle the SPA otherwise owns.
    assert!(is_reserved_path(
        "/companies/acme/.well-known/agent-card.json"
    ));
    // A console route that merely shares a prefix substring is not reserved.
    assert!(!is_reserved_path("/apidocs"));
    assert!(!is_reserved_path("/tinyplace-console"));
    // `/companies/{handle}` client-side console routes still fall through.
    assert!(!is_reserved_path("/companies/acme"));
    assert!(!is_reserved_path("/"));
    assert!(!is_reserved_path("/some/spa/route"));
}

#[test]
fn browser_analytics_config_accepts_only_plain_collector_urls() {
    assert_eq!(
        public_browser_endpoint("https://collector.example/api/track").as_deref(),
        Some("https://collector.example/")
    );
    assert_eq!(
        public_browser_endpoint("http://localhost:3000/track").as_deref(),
        Some("http://localhost:3000/")
    );
    assert!(public_browser_endpoint("http://127.0.0.1:3000/track").is_some());
    assert!(public_browser_endpoint("http://[::1]:3000/track").is_some());
    assert!(public_browser_endpoint("http://collector.example/api/track").is_none());
    assert!(public_browser_endpoint("http://collector.internal/api/track").is_none());
    assert!(public_browser_endpoint("https://user:secret@collector.example/api/track").is_none());
    assert!(public_browser_endpoint("https://collector.example/api/track?token=secret").is_none());
    assert!(public_browser_endpoint("not a URL").is_none());
}

#[test]
fn hosted_console_config_enables_openpanel_without_exposing_credentials() {
    let script = render_console_config(Some("https://collector.example/api/track"), true, true);
    assert_eq!(
        script,
        "window.OPENCOMPANY_CONFIG=Object.assign(window.OPENCOMPANY_CONFIG||{},\
{analytics:true,analyticsEndpoint:\"https://collector.example/\"});\n"
    );

    for endpoint in [
        "https://user:secret@collector.example/api/track",
        "https://collector.example/api/track?token=secret",
    ] {
        let script = render_console_config(Some(endpoint), true, true);
        assert!(!script.contains("analytics:true"), "{script}");
        assert!(!script.contains("secret"), "{script}");
    }

    assert!(
        !render_console_config(Some("https://collector.example/api/track"), false, true)
            .contains("analytics:true")
    );
    assert!(
        !render_console_config(Some("https://collector.example/api/track"), true, false)
            .contains("analytics:true")
    );
}

#[test]
fn hosted_deployment_accepts_either_hosted_signal() {
    assert!(hosted_deployment_from_values(Some("hosted-tenant"), None));
    assert!(hosted_deployment_from_values(None, Some("tenant-a")));
    assert!(hosted_deployment_from_values(Some("  "), Some("tenant-a")));
    assert!(hosted_deployment_from_values(
        Some("self-hosted"),
        Some("tenant-a")
    ));
    assert!(hosted_deployment_from_values(
        Some("other"),
        Some("tenant-a")
    ));
    assert!(!hosted_deployment_from_values(None, Some("  ")));
    assert!(!hosted_deployment_from_values(None, None));
}

#[test]
fn browser_analytics_switch_fails_closed_on_unrecognised_values() {
    for value in [Some("on"), Some(" ON ")] {
        assert!(browser_analytics_enabled_from_value(value), "{value:?}");
    }
    for value in [
        None,
        Some(""),
        Some("  "),
        Some("YES"),
        Some("true"),
        Some("1"),
        Some("off"),
        Some("FALSE"),
        Some("0"),
        Some("no"),
        Some("of"),
    ] {
        assert!(!browser_analytics_enabled_from_value(value), "{value:?}");
    }
}

#[tokio::test]
async fn console_config_route_returns_uncached_javascript() {
    let env = crate::test_support::EnvVarGuard::capture(&[
        "OPENCOMPANY_DEPLOYMENT",
        "OPENCOMPANY_TENANT_ID",
        "OPENCOMPANY_ANALYTICS",
        "OPENCOMPANY_ANALYTICS_ENDPOINT",
    ]);
    env.remove("OPENCOMPANY_DEPLOYMENT");
    env.remove("OPENCOMPANY_TENANT_ID");
    env.remove("OPENCOMPANY_ANALYTICS");
    env.remove("OPENCOMPANY_ANALYTICS_ENDPOINT");

    let app = router_with_console(AppState::new(AppConfig::default()), None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/opencompany-config.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap(),
        "application/javascript; charset=utf-8"
    );
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .unwrap(),
        "no-store"
    );
    assert_eq!(
        body_text(response).await,
        "window.OPENCOMPANY_CONFIG=window.OPENCOMPANY_CONFIG||{};\n"
    );
}

#[tokio::test]
async fn console_config_route_serves_only_safe_hosted_configuration() {
    let env = crate::test_support::EnvVarGuard::capture(&[
        "OPENCOMPANY_DEPLOYMENT",
        "OPENCOMPANY_TENANT_ID",
        "OPENCOMPANY_ANALYTICS",
        "OPENCOMPANY_ANALYTICS_ENDPOINT",
    ]);
    env.set("OPENCOMPANY_DEPLOYMENT", "hosted-tenant");
    env.remove("OPENCOMPANY_TENANT_ID");
    env.set("OPENCOMPANY_ANALYTICS", "on");
    env.set(
        "OPENCOMPANY_ANALYTICS_ENDPOINT",
        "https://collector.example/api/track",
    );

    let app = router_with_console(AppState::new(AppConfig::default()), None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/opencompany-config.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        body_text(response).await,
        "window.OPENCOMPANY_CONFIG=Object.assign(window.OPENCOMPANY_CONFIG||{},\
{analytics:true,analyticsEndpoint:\"https://collector.example/\"});\n"
    );

    env.set(
        "OPENCOMPANY_ANALYTICS_ENDPOINT",
        "http://collector.example/api/track",
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/opencompany-config.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        body_text(response).await,
        "window.OPENCOMPANY_CONFIG=window.OPENCOMPANY_CONFIG||{};\n"
    );
}

#[tokio::test]
async fn root_404s_without_console_dir() {
    let app = router_with_console(AppState::new(AppConfig::default()), None);

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// An idle workload reports `busy: false`, so the manager parks it as it
/// always has.
///
/// Note what this does *not* prove: with an empty registry the handler
/// iterates nothing and returns `false` without consulting any runtime, so
/// this cannot fail for an aggregation bug. The direction that matters —
/// that the endpoint can report `true` — is covered by
/// `is_busy_sees_every_source_of_work` in `company::runtime`, which
/// exercises the real signal against a real runtime, and by
/// `is_busy_fails_closed_on_a_poisoned_run_supervisor` alongside it.
#[tokio::test]
async fn busy_is_false_when_nothing_is_running() {
    let app = router(AppState::new(AppConfig::default()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz/busy")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["busy"], serde_json::Value::Bool(false), "{json}");
}

/// The wire shape the manager parses. It reads a top-level boolean `busy`
/// and treats anything else — a missing field, a rename, a string `"true"` —
/// as *not busy*, so a change here would silently stop protecting work in
/// flight rather than fail loudly.
#[tokio::test]
async fn busy_responds_with_the_shape_the_manager_parses() {
    let app = router(AppState::new(AppConfig::default()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz/busy")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("busy")
            .and_then(serde_json::Value::as_bool)
            .is_some(),
        "the manager reads a top-level boolean `busy`; got {json}"
    );
}

#[tokio::test]
async fn healthz_returns_ok() {
    let app = router(AppState::new(AppConfig::default()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn spec_returns_axum_framework() {
    let app = router(AppState::new(AppConfig::default()));

    let response = app
        .oneshot(Request::builder().uri("/spec").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

/// `/spec` is the handshake a multi-connection client boots from.
async fn spec_body(state: AppState) -> serde_json::Value {
    let response = router(state)
        .oneshot(Request::builder().uri("/spec").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The embedded shell asks for `:0` and needs the port back.
///
/// A desktop app cannot hardcode 8080 — it would collide with a dev server
/// or a second app, and a support case that reads "it works on my machine
/// unless I have the terminal open" is the result. `serve` cannot answer
/// which port the OS chose, because by the time it runs the listener is
/// consumed and `config().bind` still says `:0`.
#[tokio::test]
async fn binding_an_ephemeral_port_reports_the_one_actually_bound() {
    let state = AppState::new(AppConfig::default());
    let (addr, serving) = bind("127.0.0.1:0", state)
        .await
        .expect("bind an ephemeral port");

    assert_ne!(addr.port(), 0, "the OS-chosen port must come back");
    assert!(
        addr.ip().is_loopback(),
        "an embedded host must not be routable"
    );

    // The listener really is open: something is accepting on that port
    // before anything is served, which is what lets the caller hand the
    // address to a webview without racing the server task.
    let serve_task = tokio::spawn(serving.run());
    let probe = tokio::net::TcpStream::connect(addr).await;
    assert!(probe.is_ok(), "the reported address must be connectable");
    serve_task.abort();
}

#[tokio::test]
async fn spec_identifies_the_instance_and_its_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());

    let body = spec_body(state.clone()).await;
    let id = body["instance_id"].as_str().expect("an instance id");
    assert_eq!(id.len(), 32, "a 128-bit id, hex-encoded");
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

    // Stable across calls, and across a fresh state over the same home —
    // the client's whole reason for reading it.
    assert_eq!(spec_body(state).await["instance_id"], id);
    let restarted = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());
    assert_eq!(spec_body(restarted).await["instance_id"], id);

    let caps = body["capabilities"].as_array().expect("capabilities");
    for expected in ["rest", "graphql", "sse"] {
        assert!(caps.iter().any(|c| c == expected), "missing {expected}");
    }
    assert_eq!(body["storage"], "fs");
    // Unset by default rather than an empty string, so a client can tell
    // "unnamed" from "named the empty string".
    assert!(body.get("display_name").is_none());
}

/// **P1 review finding (Codex) on PR #2038.** A console offering the
/// four-way blocker answer to a host predating it sends `blocker_verdict`,
/// that host ignores the unknown field and resolves from the lowered
/// two-way verdict alone — a skip becomes a retry, an amend re-runs
/// without the operator's words — while the console reports the four-way
/// result it believes it asked for. A console can only decline to send
/// what a host cannot carry out if the host says which it is, and this
/// capability is that statement.
///
/// Feature-gated because the answer is: `blocker_verdict` handling lives
/// behind `openhuman`, and a build without it refuses the field outright
/// (`operator::blocker_verdict`). Advertising it unconditionally would
/// promise a resume this build has no code for.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn spec_advertises_the_four_way_blocker_answer() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());
    let body = spec_body(state).await;
    let caps = body["capabilities"].as_array().expect("capabilities");
    assert!(
        caps.iter().any(|c| c == "blocker-verdict"),
        "a build that answers blocker_verdict must advertise it — otherwise a console              cannot tell it apart from one that will silently lower a skip to a retry: {caps:?}"
    );
}

#[tokio::test]
async fn spec_names_the_build_commit_beside_the_version() {
    // `version` has read `0.1.0` for thousands of commits, so it alone
    // cannot tell an operator which build a host is running. The commit is
    // the same *kind* of fact — identical for every instance compiled from
    // one artifact, and saying nothing about this host — which is why it
    // sits on the unauthenticated handshake where `version` already does.
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());
    let body = spec_body(state).await;

    let commit = body["build_commit"].as_str().expect("a build commit");
    assert_eq!(commit, crate::BUILD_COMMIT);
    assert!(!commit.is_empty(), "an absent stamp must read `unknown`");
    assert!(
        commit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
        "{commit:?} is not a sanitized stamp"
    );
    assert_eq!(body["version"], crate::VERSION);
}

#[tokio::test]
async fn two_instances_are_distinguishable() {
    // The multi-connection requirement in one assertion: a client holding
    // two servers must be able to tell them apart even when both are
    // freshly initialised with identical config.
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let spec_a =
        spec_body(AppState::new(AppConfig::default()).with_home(a.path().to_path_buf())).await;
    let spec_b =
        spec_body(AppState::new(AppConfig::default()).with_home(b.path().to_path_buf())).await;
    assert_ne!(spec_a["instance_id"], spec_b["instance_id"]);
}

#[tokio::test]
async fn spec_never_leaks_the_storage_location() {
    // `/spec` is unauthenticated. The backend *kind* is useful to a client;
    // a path or a connection string would be a gift to anyone who asks.
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(AppConfig {
        instance_name: Some("prod-eu".to_string()),
        ..AppConfig::default()
    })
    .with_home(dir.path().to_path_buf());

    let body = spec_body(state).await;
    assert_eq!(body["display_name"], "prod-eu");
    let rendered = body.to_string();
    assert!(
        !rendered.contains(&dir.path().display().to_string()),
        "the home path must not appear in /spec: {rendered}"
    );
    assert!(!rendered.contains("mongodb://"), "no connection strings");
}

/// Every string `/spec` serves must be a build fact. The previous test
/// pins the home path, but it builds its state from `AppConfig::default()`,
/// where every configurable path is `None` — so it asserts against fields
/// nothing populated. This one configures them.
#[tokio::test]
async fn spec_never_leaks_a_configured_host_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sentinel-openhuman-checkout");
    let state = AppState::new(AppConfig {
        openhuman_root: Some(root.clone()),
        ..AppConfig::default()
    })
    .with_home(dir.path().to_path_buf());

    let body = spec_body(state).await;
    // The replacement contract, not just the absence: without this the test
    // also passes on a host that dropped the field altogether, or that
    // reports `false` while a checkout is configured.
    assert_eq!(body["openhuman_configured"].as_bool(), Some(true));

    let rendered = body.to_string();
    assert!(
        !rendered.contains("sentinel-openhuman-checkout"),
        "a configured checkout path must not appear in /spec: {rendered}"
    );
    assert!(
        !rendered.contains(&dir.path().display().to_string()),
        "the home path must not appear in /spec: {rendered}"
    );

    let unset =
        spec_body(AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf())).await;
    assert_eq!(unset["openhuman_configured"].as_bool(), Some(false));
}

/// Catches the *next* such field rather than this one: whatever `/spec`
/// grows, no value it serves may be an absolute filesystem path. The
/// fixture above can only assert about fields whoever wrote it knew to
/// populate; this holds for fields that do not exist yet.
#[tokio::test]
async fn spec_serves_no_absolute_path_in_any_field() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(AppConfig {
        openhuman_root: Some(dir.path().join("checkout")),
        instance_name: Some("prod-eu".to_string()),
        ..AppConfig::default()
    })
    .with_home(dir.path().to_path_buf());

    let mut offenders = Vec::new();
    collect_absolute_paths(&spec_body(state).await, String::new(), &mut offenders);
    assert!(
        offenders.is_empty(),
        "/spec serves deployment paths at {offenders:?}"
    );
}

#[test]
fn absolute_paths_are_recognised_on_both_platforms() {
    for path in [
        "/Users/someone/checkout",
        "/data",
        r"C:\checkout",
        r"c:/checkout",
        r"\\server\share",
    ] {
        assert!(is_absolute_path(path), "`{path}` is an absolute path");
    }
    for text in [
        "vendor/openhuman",
        "https://api.tinyhumans.ai",
        "fs",
        "prod-eu",
        "",
        "C:",
        "Cx\\checkout",
    ] {
        assert!(!is_absolute_path(text), "`{text}` is not an absolute path");
    }
}

/// Whether a string reads as an absolute filesystem path.
///
/// Windows shapes as well as POSIX: this host builds for Windows, and a
/// guard that only knows `/` would pass while `/spec` served `C:\checkout`.
fn is_absolute_path(text: &str) -> bool {
    if text.starts_with('/') || text.starts_with(r"\\") {
        return true;
    }
    let mut chars = text.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic()
    )
}

/// Walks a JSON value and records the location of every string that reads
/// as an absolute filesystem path.
fn collect_absolute_paths(value: &serde_json::Value, at: String, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => {
            if is_absolute_path(text) {
                out.push(format!("{at} = {text}"));
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_absolute_paths(item, format!("{at}[{index}]"), out);
            }
        }
        serde_json::Value::Object(fields) => {
            for (key, field) in fields {
                let at = if at.is_empty() {
                    key.clone()
                } else {
                    format!("{at}.{key}")
                };
                collect_absolute_paths(field, at, out);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn spec_does_not_disclose_memory_engine_details() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());
    let body = spec_body(state).await;
    // `/spec` is the unauthenticated manager handshake. Memory-provider
    // identity, capability and reachability details belong to the
    // operator-authenticated engine route instead.
    assert!(
        body.get("memory").is_none(),
        "memory leaked through /spec: {body}"
    );
}
