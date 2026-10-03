use super::*;

/// The layer the desktop never had: an environment that names a hub is the
/// hub, rather than the compiled-in production constant.
#[test]
fn resolve_host_takes_the_hub_the_environment_names() {
    let env = crate::app::config::MapEnv::new([(
        "TINYHUMANS_API_URL",
        "https://staging-api.tinyhumans.ai",
    )]);
    let config = AppConfig::resolve_host(&env, None).expect("resolves");
    assert_eq!(config.api_url, "https://staging-api.tinyhumans.ai");
}

/// And the file under it, which is the layer the first-run setup wizard
/// writes — a host that ignored it would honour an operator's choice until
/// they quit.
///
/// The file also carries `tinyplace_api_url`, which the setup wizard wrote
/// before tiny.place was removed. A retired key is ignored, not refused: an
/// existing install's `config.toml` must keep loading.
#[test]
fn resolve_host_falls_through_to_the_config_file() {
    let file = toml::from_str::<crate::app::config::ConfigFile>(
        "api_url = \"https://toml-api.example\"\ntinyplace_api_url = \"https://toml-place.example\"\n",
    )
    .expect("a config.toml holding the retired tiny.place key still parses");
    let config = AppConfig::resolve_host(&crate::app::config::MapEnv::new([("", "")]), Some(&file))
        .expect("resolves");
    assert_eq!(config.api_url, "https://toml-api.example");
}

/// The environment outranks the file, and a value that is only whitespace
/// is nobody having said anything — not a blank that beats a real setting.
#[test]
fn an_environment_hub_outranks_the_file_but_a_blank_one_does_not() {
    let file = toml::from_str::<crate::app::config::ConfigFile>(
        "api_url = \"https://toml-api.example\"\n",
    )
    .expect("parses");

    let env = crate::app::config::MapEnv::new([("TINYHUMANS_API_URL", "https://env.example")]);
    assert_eq!(
        AppConfig::resolve_host(&env, Some(&file))
            .expect("resolves")
            .api_url,
        "https://env.example"
    );

    let blank = crate::app::config::MapEnv::new([("TINYHUMANS_API_URL", "   ")]);
    assert_eq!(
        AppConfig::resolve_host(&blank, Some(&file))
            .expect("resolves")
            .api_url,
        "https://toml-api.example"
    );
}

/// `hub_site` derives from the resolved hub, so pointing a host at staging
/// points its "manage keys" and "top up" links at the staging dashboard —
/// the reason `api_url` reaching the desktop matters beyond `/spec`.
#[test]
fn a_resolved_hub_carries_the_dashboard_with_it() {
    let env = crate::app::config::MapEnv::new([(
        "TINYHUMANS_API_URL",
        "https://staging-api.tinyhumans.ai",
    )]);
    let config = AppConfig::resolve_host(&env, None).expect("resolves");
    assert_ne!(
        config.hub_site(),
        AppConfig::default().hub_site(),
        "a host on staging must not link to the production dashboard"
    );
}

/// The `[workspace]` knobs every company builder reads come from the same
/// pass, so a host cannot resolve the hub and still run the compiled-in
/// quota.
#[test]
fn resolve_host_carries_the_workspace_section() {
    let file =
        toml::from_str::<crate::app::config::ConfigFile>("[workspace]\ngit_enabled = true\n")
            .expect("parses");
    let config = AppConfig::resolve_host(&crate::app::config::MapEnv::new([("", "")]), Some(&file))
        .expect("resolves");
    assert!(config.workspace_git_enabled);
}

#[test]
fn default_config_binds_locally() {
    assert_eq!(AppConfig::default().bind, "127.0.0.1:8080");
}

/// Automatic Git checkpoints in agent workspaces are opt-in: the host
/// default is off, preserving the pre-checkpoint behavior exactly.
#[test]
fn workspace_git_checkpoints_default_off() {
    assert!(!AppConfig::default().workspace_git_enabled);
}

/// Callable and finite in every build, `acp` feature or not — the whole
/// point of the facade: an embedder linking this crate as a library
/// (`src-tauri/src/embedded.rs`) cannot know at its own compile time
/// whether this crate's default dependency features happened to include
/// `acp`, so the call site must never need a `cfg` of its
/// own to stay buildable.
#[tokio::test]
async fn spawn_acp_session_sweeper_is_always_callable_and_stoppable() {
    let state = AppState::new(AppConfig::default());
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let handle = state.spawn_acp_session_sweeper(Arc::clone(&shutdown));
    // `notify_one`, not `notify_waiters`: this test is the only holder of
    // `shutdown` and the sweeper its only waiter, and unlike
    // `notify_waiters`, `notify_one` stores a permit for a task that has
    // not registered as waiting yet — so this cannot lose the
    // notification to a scheduling race.
    shutdown.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("the sweeper task must stop once notified, in every build")
        .expect("the sweeper task must not panic");
}

fn bound_to(bind: &str) -> AppConfig {
    AppConfig {
        bind: bind.to_string(),
        ..AppConfig::default()
    }
}

#[test]
fn loopback_binds_are_local_only() {
    for bind in [
        "127.0.0.1:8080",
        "127.0.0.53:8080",
        "localhost:8080",
        "LocalHost:8080",
        "[::1]:8080",
    ] {
        assert!(bound_to(bind).is_local_only(), "{bind} is loopback");
    }
}

#[test]
fn routable_binds_are_not_local_only() {
    for bind in ["0.0.0.0:8080", "192.168.1.10:8080", "[::]:8080"] {
        assert!(!bound_to(bind).is_local_only(), "{bind} is routable");
    }
}

#[test]
fn an_unprovable_bind_host_fails_closed() {
    // A DNS name could resolve anywhere, and a malformed bind is not
    // evidence of safety. Neither may unlock loopback-only behavior.
    for bind in ["example.com:8080", ":8080", "garbage", ""] {
        assert!(
            !bound_to(bind).is_local_only(),
            "{bind:?} is not provably loopback and must fail closed"
        );
    }
}

#[test]
fn a_public_url_settles_it_regardless_of_bind() {
    // Someone expects to reach this from elsewhere. Whatever the bind says,
    // this host is not a private laptop.
    let config = AppConfig {
        public_url: Some("https://acme.example".into()),
        ..bound_to("127.0.0.1:8080")
    };
    assert!(!config.is_local_only());
}

#[test]
fn spec_reports_axum_framework() {
    let spec = AppState::new(AppConfig::default()).spec();

    assert_eq!(spec.framework, "axum");
    assert!(spec.modules.contains(&"server"));
}

/// A host with nothing to open is the only one the wizard is for.
///
/// The registered-company half of this lives in `server::setup::test`,
/// beside the helper that can build a real runtime:
/// `spec_reports_setup_complete_once_a_company_is_registered`.
#[test]
fn spec_reports_setup_incomplete_for_an_empty_unstamped_host() {
    let spec = AppState::new(AppConfig::default()).spec();

    assert!(
        !spec.setup_complete,
        "no stamp and no companies is exactly the first-run case"
    );
}

#[cfg(feature = "mcp")]
#[test]
fn parked_oauth_flow_is_single_use() {
    use crate::company::mcp_oauth::PendingOAuth;
    use crate::ports::types::CompanyId;

    let state = AppState::new(AppConfig::default());
    let pending = PendingOAuth {
        company_id: CompanyId::new("acme"),
        server_name: "notion".into(),
        code_verifier: "verifier".into(),
        client_id: "cid".into(),
        client_secret: Some("secret".into()),
        token_endpoint: "https://as.example/token".into(),
        redirect_uri: "https://acme.example/oauth/mcp/callback".into(),
    };

    state.park_oauth("state-1".into(), pending.clone());
    // First take reclaims it; a replayed callback finds nothing (single-use).
    assert!(state.take_oauth("state-1").is_some());
    assert!(state.take_oauth("state-1").is_none());
    // An unknown state is always None.
    assert!(state.take_oauth("never-parked").is_none());
}

#[cfg(feature = "mcp")]
#[test]
fn parked_oauth_flow_expires_and_is_swept() {
    use crate::company::mcp_oauth::PendingOAuth;
    use crate::ports::types::CompanyId;
    use std::time::{Duration, Instant};

    let state = AppState::new(AppConfig::default());
    let pending = |server: &str| PendingOAuth {
        company_id: CompanyId::new("acme"),
        server_name: server.into(),
        code_verifier: "verifier".into(),
        client_id: "cid".into(),
        client_secret: Some("secret".into()),
        token_endpoint: "https://as.example/token".into(),
        redirect_uri: "https://acme.example/oauth/mcp/callback".into(),
    };
    let stale_at = Instant::now() - (AppState::OAUTH_PENDING_TTL + Duration::from_secs(1));

    // Stale-on-read: an entry parked past its TTL is rejected (and removed).
    state.park_oauth_at("expired".into(), pending("notion"), stale_at);
    assert!(state.take_oauth("expired").is_none());

    // Sweep-on-park: parking a fresh flow evicts any stale sibling first, so
    // an abandoned flow's secrets can't outlive the TTL even if never taken.
    state.park_oauth_at("stale".into(), pending("slack"), stale_at);
    state.park_oauth("fresh".into(), pending("github"));
    assert!(state.take_oauth("stale").is_none());
    assert!(state.take_oauth("fresh").is_some());
}

#[test]
fn host_base_url_falls_back_to_bind() {
    let config = AppConfig::default();
    assert_eq!(config.host_base_url(), "http://127.0.0.1:8080");

    let public = AppConfig {
        public_url: Some("https://acme.example".into()),
        ..AppConfig::default()
    };
    assert_eq!(public.host_base_url(), "https://acme.example");
}

/// Issue #203: unlike `host_base_url`, this has no bind fallback — a URL a
/// provider cannot reach is worse than none, because it silently swallows
/// every inbound delivery.
#[test]
fn public_webhook_base_url_requires_an_explicit_https_url() {
    // The default (loopback bind, no public_url) offers no webhook — this
    // is exactly the `http://127.0.0.1:8080/hooks/...` URL of issue #203.
    assert_eq!(AppConfig::default().public_webhook_base_url(), None);

    let with = |url: &str| AppConfig {
        public_url: Some(url.into()),
        ..AppConfig::default()
    };
    // Plain http never qualifies: Telegram's setWebhook refuses it, and a
    // public-looking http URL is no more deliverable than a loopback one.
    assert_eq!(with("http://acme.example").public_webhook_base_url(), None);
    // Neither does a scheme-less or empty value.
    assert_eq!(with("acme.example").public_webhook_base_url(), None);
    assert_eq!(with("   ").public_webhook_base_url(), None);
    assert_eq!(with("https://").public_webhook_base_url(), None);

    assert_eq!(
        with("https://acme.example").public_webhook_base_url(),
        Some("https://acme.example")
    );
    // Surrounding whitespace and a trailing slash are normalized away, so
    // callers can join a `/hooks/...` path without doubling the separator.
    assert_eq!(
        with("  https://acme.example/  ").public_webhook_base_url(),
        Some("https://acme.example")
    );
    // The scheme is case-insensitive, as in any URL.
    assert_eq!(
        with("HTTPS://acme.example").public_webhook_base_url(),
        Some("HTTPS://acme.example")
    );
}

#[test]
fn debug_redacts_the_credential() {
    let config = AppConfig {
        tinyhumans_credential: Some(SecretValue("th_super_secret_value".into())),
        ..AppConfig::default()
    };
    let rendered = format!("{config:?}");
    assert!(!rendered.contains("th_super_secret_value"));
    assert!(rendered.contains("set"));
}

/// With neither tier configured there is nothing to obtain, so no cycles.
/// Driven through the env seam so an ambient `TINYHUMANS_*` in a developer's
/// shell cannot decide the result.
#[test]
fn default_config_cannot_run_cycles() {
    use crate::app::config::MapEnv;
    let empty = MapEnv::default();
    assert!(!AppConfig::default().cycles_available_in(&empty));
    assert!(!AppConfig::default().credential_available_in(&empty));
    assert_eq!(
        AppConfig::default().credential_source_in(&empty),
        CredentialSource::None
    );
}

/// A temp directory holding a stand-in for the platform's projected token
/// file (mounted at `/var/run/secrets/tinyhumans.ai/token` in production).
/// The tier is only selected when the path exists, so the test needs a real
/// one.
/// Returns the directory handle alongside the path: dropping it removes the
/// fixture, and holding it is what keeps the file alive for the assertions.
fn projected_token_file() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("oc-appcfg-")
        .tempdir()
        .expect("tempdir");
    let path = dir.path().join("token");
    std::fs::write(&path, "projected-token").unwrap();
    (dir, path)
}

/// The hosted shape: no static secret at all, just a projected token file.
/// Cognition must be considered available, and the source reads `attested`.
#[test]
fn a_projected_token_file_alone_enables_cycles() {
    use crate::app::config::MapEnv;
    let (_dir, path) = projected_token_file();
    let env = MapEnv::new([(
        crate::company::credentials::TOKEN_FILE_ENV,
        path.display().to_string(),
    )]);
    let config = AppConfig::default();
    assert!(config.tinyhumans_credential.is_none(), "nothing stored");
    assert!(config.credential_available_in(&env));
    assert!(config.cycles_available_in(&env));
    assert_eq!(
        config.credential_source_in(&env),
        CredentialSource::Attested
    );

    // A sidecar brain still cannot run hosted cycles, credential or not.
    let sidecar = AppConfig {
        brain_mode: crate::app::config::BrainMode::Sidecar,
        ..AppConfig::default()
    };
    assert!(!sidecar.cycles_available_in(&env));

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

/// Docker development is unaffected: the static tier still answers yes, and
/// a projected file present alongside it outranks it as the source.
#[test]
fn static_tier_still_answers_and_is_outranked_by_a_projected_file() {
    use crate::app::config::MapEnv;
    let config = AppConfig {
        tinyhumans_credential: Some(SecretValue("th_static".into())),
        ..AppConfig::default()
    };
    let empty = MapEnv::default();
    assert!(config.cycles_available_in(&empty));
    assert_eq!(
        config.credential_source_in(&empty),
        CredentialSource::Static
    );

    let (_dir, path) = projected_token_file();
    let projected = MapEnv::new([(
        crate::company::credentials::TOKEN_FILE_ENV,
        path.display().to_string(),
    )]);
    assert_eq!(
        config.credential_source_in(&projected),
        CredentialSource::Attested
    );

    // A leftover variable pointing at a path the runtime never mounted (the
    // docker case) degrades to the static tier rather than breaking cycles.
    let stale = MapEnv::new([(
        crate::company::credentials::TOKEN_FILE_ENV,
        "/nonexistent/oc/token",
    )]);
    assert_eq!(
        config.credential_source_in(&stale),
        CredentialSource::Static
    );
    assert!(config.cycles_available_in(&stale));

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn skill_registry_loads_the_shipped_bundles_and_caches() {
    let state = AppState::new(AppConfig::default());
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../companies");

    let first = state.skill_registry(&dir).expect("registry loads");
    assert!(first.iter().any(|skill| skill.slug == "web-research"));
    assert!(first.iter().any(|skill| skill.slug == "weekly-report"));
    // The post-call half of the meeting pair (#240): its body must carry the
    // full contract, not just the frontmatter description.
    let debrief = first
        .iter()
        .find(|skill| skill.slug == "call-debrief")
        .expect("call-debrief is in the registry (enterprise_sales ships it)");
    assert_eq!(debrief.name, "Call Debrief");
    assert_eq!(debrief.category.as_deref(), Some("Ops"));
    assert!(debrief.body.contains("## Steps"), "{}", debrief.body);
    assert!(debrief.body.contains("## Output"), "{}", debrief.body);

    // A second call returns the same cached allocation, ignoring the path.
    let second = state
        .skill_registry(std::path::Path::new("/nonexistent"))
        .expect("cached registry");
    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn skill_registry_rejects_a_configured_but_missing_library() {
    // A configured `skills_root` that does not exist is a host
    // misconfiguration. `load_catalog_skills` returns `Ok(empty)` for a missing
    // dir, so without the `is_dir` guard the registry would silently flatten
    // to empty — downgrading a server-authoritative install to a
    // client-authored one, the invariant `shared_skill_registry` forbids.
    let state = AppState::new(AppConfig::default());
    let err = state
        .skill_registry(std::path::Path::new("/nonexistent"))
        .expect_err("a missing configured library must fail, not load empty");
    assert!(
        matches!(err, crate::OpenCompanyError::Config(_)),
        "expected a Config error for a missing library, got {err:?}"
    );
}

#[test]
fn namespaced_company_id_is_noop_when_unset() {
    let config = AppConfig::default();
    assert!(config.tenant_namespace.is_none());
    let id = CompanyId::new("agentic-software-company");
    assert_eq!(config.namespaced_company_id(id.clone()), id);
}

#[test]
fn namespaced_company_id_prefixes_when_set() {
    let config = AppConfig {
        tenant_namespace: Some("acme".into()),
        ..AppConfig::default()
    };
    assert_eq!(
        config.namespaced_company_id(CompanyId::new("agentic-software-company")),
        CompanyId::new("acme--agentic-software-company")
    );
}

#[test]
fn namespaced_company_id_is_idempotent() {
    let config = AppConfig {
        tenant_namespace: Some("acme".into()),
        ..AppConfig::default()
    };
    let once = config.namespaced_company_id(CompanyId::new("agentic-software-company"));
    let twice = config.namespaced_company_id(once.clone());
    assert_eq!(once, twice);
    assert_eq!(once, CompanyId::new("acme--agentic-software-company"));
}

#[test]
fn canonical_tenant_strips_prefix() {
    assert_eq!(canonical_tenant("tenant:acme"), "acme");
    assert_eq!(canonical_tenant("acme"), "acme");
    // Only the leading `tenant:` is stripped, and only once.
    assert_eq!(canonical_tenant("company:acme"), "company:acme");
    assert_eq!(canonical_tenant("tenant:tenant:x"), "tenant:x");
}

#[test]
fn tenant_namespace_rejects_the_id_delimiter() {
    // A namespace containing `--` makes the `<tenant>--` id prefix
    // ambiguous between tenants, so the boundary that reads
    // `OPENCOMPANY_TENANT_ID` rejects it.
    assert!(validate_tenant_namespace("acme").is_ok());
    assert!(validate_tenant_namespace("acme-corp").is_ok());
    assert_eq!(
        validate_tenant_namespace("acme--other").unwrap_err(),
        "tenant namespace `acme--other` contains `--`, which is the company-id \
         delimiter; a namespace may not contain it"
    );
}

#[test]
fn ownership_is_keyed_canonically_across_representations() {
    let state = AppState::new(AppConfig::default());
    let id = CompanyId::new("acme--acme");

    // A row stored in the claim shape (as hydration would set it) is keyed by
    // the bare slug, so a query in either representation finds it.
    state.set_owner(id.clone(), "tenant:acme");
    assert_eq!(state.owner_of(&id).as_deref(), Some("acme"));
    assert_eq!(state.tenant_company_count("acme"), 1);
    assert_eq!(state.tenant_company_count("tenant:acme"), 1);
    assert_eq!(state.tenant_company_count("tenant:globex"), 0);

    // Re-recording under the bare form is the same identity, not a second.
    state.set_owner(id.clone(), "acme");
    assert_eq!(state.tenant_company_count("tenant:acme"), 1);
}

#[test]
fn hosted_with_credential_can_run_cycles() {
    let config = AppConfig {
        tinyhumans_credential: Some(SecretValue("th_secret".into())),
        ..AppConfig::default()
    };
    assert!(config.cycles_available());
    assert!(AppState::new(config).spec().cycles_available);
}

/// `config_root` defaults to `home` — the aligned shape every deployment
/// but an explicit, diverging `--home` takes (see the field doc and
/// `store::home_divergence_warning`).
#[test]
fn config_root_defaults_to_home() {
    let state = AppState::new(AppConfig::default()).with_home("/data/companies");
    assert_eq!(state.config_root(), std::path::Path::new("/data/companies"));
}

/// Once set explicitly, `config_root` diverges from `home` — this is the
/// fix for #908's review: `server::setup` must resolve `config.toml`
/// through this, not through `home`, or it reads and writes a different
/// file than startup does on a deployment where the two differ.
#[test]
fn config_root_can_diverge_from_home() {
    let state = AppState::new(AppConfig::default())
        .with_home("/data/companies")
        .with_config_root("/data");
    assert_eq!(state.home(), std::path::Path::new("/data/companies"));
    assert_eq!(state.config_root(), std::path::Path::new("/data"));
}
