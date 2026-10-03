use super::*;

// PR #1875 review finding: `activation_gate_bypass_enabled` guards
// whether a company's activation-funnel gate is bypassed at first boot
// (issue #1844). Only the literal `"1"` may enable it — unset, and
// anything that merely *looks* truthy, must stay disabled so a typo in
// the e2e host script's env fails closed onto the real gate.
#[test]
fn activation_gate_bypass_disabled_when_unset() {
    assert!(!activation_gate_bypass_enabled(None));
}

#[test]
fn activation_gate_bypass_enabled_on_literal_one() {
    assert!(activation_gate_bypass_enabled(Some("1")));
}

#[test]
fn activation_gate_bypass_disabled_on_truthy_lookalike() {
    assert!(!activation_gate_bypass_enabled(Some("true")));
}

#[test]
fn activation_gate_bypass_disabled_on_zero() {
    assert!(!activation_gate_bypass_enabled(Some("0")));
}

#[test]
fn the_home_flag_is_taken_verbatim() {
    // The binary owns no home policy of its own: it delegates to
    // `store::resolve_home`, whose precedence chain (flag >
    // OPENCOMPANY_DATA_DIR > $HOME/.opencompany) is covered in
    // `src/store/paths.rs`. This only pins the wiring.
    assert_eq!(
        opencompany::store::resolve_home(Some(PathBuf::from("/flag"))).unwrap(),
        PathBuf::from("/flag")
    );
}

#[test]
fn every_home_resolving_command_migrates_the_legacy_nest() {
    // `serve`, `export`, and `import` all resolve through
    // `resolve_home_migrated`, so an un-migrated install's first
    // post-upgrade command is not the one that finds no bundles.
    let home = std::env::temp_dir().join(format!(
        "oc-bin-migrate-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    let nested = home.join("companies/companies/acme");
    std::fs::create_dir_all(&nested).expect("legacy bundle");
    std::fs::write(nested.join("company.toml"), "[company]\n").expect("manifest");

    let resolved = resolve_home_migrated(Some(home.clone())).expect("resolves and migrates");

    assert_eq!(resolved, home);
    assert!(home.join("companies/acme/company.toml").exists());
    assert!(!home.join("companies/companies").exists());
    // The wiring is what is under test, but the bundle path it produces is
    // the point of the whole change.
    assert_eq!(
        opencompany::store::Bundle::new(resolved, &CompanyId::new("acme"))
            .dir()
            .to_path_buf(),
        home.join("companies/acme")
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn no_command_resolves_a_home_without_migrating_it() {
    // The test above pins the helper; this pins that the helper is the only
    // door. A command that called `store::resolve_home` directly would read
    // an un-migrated install and find no companies — and no runtime test
    // would catch it, because the defect is a call that never happens. The
    // needle is split so this assertion does not match its own source line.
    let needle = concat!("store::", "resolve_home(");
    let source = include_str!("../opencompany.rs");
    // The test module now lives in this sibling file rather than inline, so
    // production code is everything before its `#[cfg(test)]` declaration.
    let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);

    let direct: Vec<&str> = production
        .lines()
        .filter(|line| line.contains(needle))
        .collect();

    assert_eq!(
        direct.len(),
        1,
        "`resolve_home` belongs to `resolve_home_migrated` alone; found {direct:?}"
    );
    let (before_helper, _) = production
        .split_once("fn resolve_home_migrated")
        .expect("the helper is declared");
    assert!(
        !before_helper.contains(needle),
        "the one call must be the one inside `resolve_home_migrated`"
    );
}

#[tokio::test]
async fn export_and_import_migrate_before_they_read() {
    // Both commands run against an install whose first post-upgrade command
    // may well be one of them, so neither may be the one that finds no
    // bundles. Their results are irrelevant here — the migration happens
    // before either touches a path, which is the whole point.
    for command in ["export", "import"] {
        let home = std::env::temp_dir().join(format!(
            "oc-bin-{command}-migrate-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let nested = home.join("companies/companies/acme");
        std::fs::create_dir_all(&nested).expect("legacy bundle");
        std::fs::write(nested.join("company.toml"), "[company]\n").expect("manifest");

        match command {
            "export" => {
                let out = home.join("out");
                let _ = run_export("acme".to_string(), Some(out), false, Some(home.clone())).await;
            }
            _ => {
                let _ = import_from_dir(&home.join("nothing-here"), Some(home.clone())).await;
            }
        }

        assert!(
            home.join("companies/acme/company.toml").exists(),
            "`{command}` resolved a home without migrating it"
        );
        assert!(!home.join("companies/companies").exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[test]
fn live_ports_layers_the_config_file_memory_section() {
    // A self-hosted operator's engine selection lives only in `config.toml`
    // (the console never exports environment variables), so `live_ports`
    // must layer that section the way `serve` does — otherwise a bundle
    // would read and write the base stores instead of the engine the host
    // actually remembers with. This pins the load-and-layer composition
    // `live_ports` feeds its settings through.
    let tmp = std::env::temp_dir().join(format!(
        "oc-bin-memcfg-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("config.toml"),
        "[memory]\nbackend = \"remote\"\ndriver = \"cortexdb\"\nurl = \"https://memory.example\"\n",
    )
    .unwrap();
    let settings =
        layer_config_memory(opencompany::store::StorageSettings::default(), &tmp).unwrap();
    assert_eq!(
        settings.memory_backend,
        opencompany::store::MemoryBackend::Remote
    );
    assert_eq!(settings.memory_driver.as_deref(), Some("cortexdb"));
    assert_eq!(
        settings.memory_url.as_deref(),
        Some("https://memory.example")
    );

    // The absent-file shape (a fresh root, or the fs default) stays on the
    // base backend's own memory.
    let absent = std::env::temp_dir().join(format!(
        "oc-bin-memcfg-absent-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&absent);
    std::fs::create_dir_all(&absent).unwrap();
    let settings =
        layer_config_memory(opencompany::store::StorageSettings::default(), &absent).unwrap();
    assert_eq!(
        settings.memory_backend,
        opencompany::store::MemoryBackend::Store
    );
    let _ = std::fs::remove_dir_all(&absent);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn register_company_loads_manifest_and_registers() {
    let home = std::env::temp_dir().join(format!("oc-bin-{}", std::process::id()));
    let state = AppState::new(AppConfig::default());
    let dir = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../companies/law_firm"
    ));

    let (id, name, _schedules) = register_company(&state, &home, dir, false).await.unwrap();

    assert_eq!(name, "Agentic Law Firm");
    assert_eq!(id, "agentic-law-firm");
    assert_eq!(state.registry().list().len(), 1);
    let runtime = state.registry().sole().expect("sole company");

    // The serve path records the source dir and seeds the workspace from
    // `companies/<name>/workspace/**` on first boot.
    assert_eq!(runtime.source_dir(), Some(dir));
    assert!(
        !runtime.workspace().is_empty(runtime.id()).await.unwrap(),
        "workspace seeded from the company source dir"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn company_source_dir_normalizes_manifest_file_to_its_directory() {
    let dir = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../companies/law_firm"
    ));
    // A directory argument is returned unchanged.
    assert_eq!(company_source_dir(dir), dir);
    // A manifest-file argument resolves to its parent company directory, so
    // the serve-path `workspace`/`skills`/`workflows` lookups stay correct.
    assert_eq!(company_source_dir(&dir.join("company.toml")), dir);
}

#[test]
fn skills_root_for_normalizes_a_relative_dot_source_dir() {
    // Codex review, PR #2326: `--company .` makes `company_source_dir`
    // return `.`, whose `Path::parent()` is the empty path — an empty
    // `skills_root` then fails `AppState::skill_registry`'s directory
    // check even though the company loaded fine. `skills_root_for` must
    // resolve `.` against the real working directory before taking its
    // parent, so the catalog root comes back as the actual `companies/`
    // dir rather than empty.
    let dot = std::path::Path::new(".");
    let root = skills_root_for(dot).expect("skills_root_for must resolve `.`");
    assert_ne!(
        root.as_os_str(),
        "",
        "skills_root_for must not return the empty path for `.`"
    );
    assert_eq!(
        root,
        std::env::current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    );
}

#[tokio::test]
async fn register_company_accepts_a_manifest_file_path() {
    let home = std::env::temp_dir().join(format!("oc-bin-file-{}", std::process::id()));
    let state = AppState::new(AppConfig::default());
    // `--company` also accepts the manifest file inside the company dir.
    let file = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../companies/law_firm/company.toml"
    ));

    let (_id, name, _schedules) = register_company(&state, &home, file, false).await.unwrap();

    assert_eq!(name, "Agentic Law Firm");
    let runtime = state.registry().sole().expect("sole company");
    // The recorded source dir is the company directory, not `company.toml`.
    assert_eq!(
        runtime.source_dir(),
        Some(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../companies/law_firm"
        )))
    );
    assert!(
        !runtime.workspace().is_empty(runtime.id()).await.unwrap(),
        "workspace still seeds when --company is a manifest file"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Issue #85: launching from a template *directory* derives the provenance
/// from that directory's basename — `source_id` and `path` both the slug,
/// `version` faithfully `None` (the serve path exposes no template version).
/// Distinct from the builder-injection test in `runtime::builder`: this
/// exercises the serve-path derivation in `register_company` itself. Per
/// 8b40fa7 the stamped `path` is the basename, never the absolute host path.
#[tokio::test]
async fn register_company_stamps_provenance_from_directory() {
    let home = std::env::temp_dir().join(format!("oc-prov-dir-{}", std::process::id()));
    let state = AppState::new(AppConfig::default());
    let dir = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../companies/law_firm"
    ));

    register_company(&state, &home, dir, false).await.unwrap();

    let runtime = state.registry().sole().expect("sole company");
    let record = runtime
        .store()
        .load(runtime.id())
        .await
        .unwrap()
        .expect("persisted record");
    let provenance = record
        .template_provenance
        .expect("a directory launch stamps template provenance");
    assert_eq!(provenance.source_id, "law_firm");
    assert_eq!(provenance.version, None, "serve path records no version");
    assert_eq!(
        provenance.path.as_deref(),
        Some("law_firm"),
        "path is the template basename, not the absolute host path"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Issue #85: launching from a `company.toml` *file* path normalizes to its
/// parent company directory before deriving provenance, so the stamped
/// provenance matches the directory launch exactly (basename `source_id` +
/// `path`, `None` version). Guards the file-path input shape of the
/// serve-path derivation.
#[tokio::test]
async fn register_company_stamps_provenance_from_manifest_file() {
    let home = std::env::temp_dir().join(format!("oc-prov-file-{}", std::process::id()));
    let state = AppState::new(AppConfig::default());
    let file = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../companies/law_firm/company.toml"
    ));

    register_company(&state, &home, file, false).await.unwrap();

    let runtime = state.registry().sole().expect("sole company");
    let record = runtime
        .store()
        .load(runtime.id())
        .await
        .unwrap()
        .expect("persisted record");
    let provenance = record
        .template_provenance
        .expect("a manifest-file launch stamps provenance from the parent dir");
    assert_eq!(provenance.source_id, "law_firm");
    assert_eq!(provenance.version, None);
    assert_eq!(
        provenance.path.as_deref(),
        Some("law_firm"),
        "path is the template basename, not the absolute host path"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Records the target and level of every event a subscriber was actually
/// asked to record, so a filter can be tested on behaviour rather than on
/// the contents of its own string.
#[derive(Clone)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<(String, tracing::Level)>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Captured {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        self.0
            .lock()
            .expect("capture lock")
            .push((meta.target().to_string(), *meta.level()));
    }
}

#[test]
fn the_default_filter_passes_durable_append_warnings_and_still_drops_other_ones() {
    // The point of `DEFAULT_LOG_FILTER` is the vendored append worker's
    // warn-level reports — "still failing after N", "recovered, N lost",
    // "never recovered before shutdown, N lost". They are the only account
    // of how much of the durable agent journal was lost, and a bare `error`
    // filter drops all three (issue #450).
    //
    // Asserted by running the filter, not by reading it: this builds the
    // real `EnvFilter` from the real constant, installs it over a capturing
    // layer exactly as `async_main` installs it over `fmt`, and emits the
    // four events that matter. Revert the constant to `"error"` and the
    // first assertion fails.
    use tracing_subscriber::layer::SubscriberExt;

    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(Captured(std::sync::Arc::clone(&captured)))
        .with(log_filter(None));

    tracing::subscriber::with_default(subscriber, || {
        // The worker's recovery summary — the line the operator needs.
        tracing::warn!(
            target: "tinyagents::observability",
            sink = "journal",
            lost = 3_u64,
            "durable append recovered"
        );
        // An unrelated warning stays dropped: the default is still `error`
        // for everything the exception does not name.
        tracing::warn!(target: "opencompany::unrelated", "ordinary warning");
        // And a real error still gets through, unchanged from before.
        tracing::error!(target: "opencompany::unrelated", "ordinary error");
        // The exception is scoped to `warn`, not opened wide.
        tracing::info!(target: "tinyagents::observability", "chatter");
    });

    let events = captured.lock().expect("capture lock").clone();
    let seen = |target: &str, level: tracing::Level| {
        events.iter().any(|(t, l)| t == target && *l == level)
    };

    assert!(
        seen("tinyagents::observability", tracing::Level::WARN),
        "the durable-append recovery/reminder/shutdown reports must survive \
         the default filter; captured {events:?}"
    );
    assert!(
        !seen("opencompany::unrelated", tracing::Level::WARN),
        "the exception is one target, not a global level bump; captured {events:?}"
    );
    assert!(
        seen("opencompany::unrelated", tracing::Level::ERROR),
        "errors kept passing exactly as they did before; captured {events:?}"
    );
    assert!(
        !seen("tinyagents::observability", tracing::Level::INFO),
        "the exception stops at `warn`; captured {events:?}"
    );
}

/// The analytics transport's refused-credential and rejected-event warnings are
/// the only account of a collector that will never take this instance's events,
/// and a bare `error` default swallowed them. Asserted by running the real
/// filter, like the durable-append test above: the directive must let an
/// `opencompany::analytics` warning through and still stop its `info`/`debug`.
#[test]
fn the_default_filter_passes_analytics_warnings_and_not_its_chatter() {
    use tracing_subscriber::layer::SubscriberExt;

    assert!(
        DEFAULT_LOG_FILTER.contains("opencompany::analytics=warn"),
        "the directive must be named in the default; got {DEFAULT_LOG_FILTER}"
    );

    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(Captured(std::sync::Arc::clone(&captured)))
        .with(log_filter(None));

    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!(target: "opencompany::analytics::openpanel::http", "refused");
        tracing::info!(target: "opencompany::analytics::openpanel::http", "accepted");
        tracing::debug!(target: "opencompany::analytics::openpanel::http", "chatter");
        tracing::warn!(target: "opencompany::unrelated", "ordinary warning");
    });

    let events = captured.lock().expect("capture lock").clone();
    let seen = |target: &str, level: tracing::Level| {
        events.iter().any(|(t, l)| t == target && *l == level)
    };
    let transport = "opencompany::analytics::openpanel::http";
    assert!(seen(transport, tracing::Level::WARN), "captured {events:?}");
    assert!(
        !seen(transport, tracing::Level::INFO),
        "captured {events:?}"
    );
    assert!(
        !seen(transport, tracing::Level::DEBUG),
        "captured {events:?}"
    );
    assert!(
        !seen("opencompany::unrelated", tracing::Level::WARN),
        "the directive is scoped to analytics; captured {events:?}"
    );
}

/// Issue #2147: the consequence-floor shadow reader's whole output is one
/// `info!` per call it would have stopped. Without a named exception a
/// bare `error` filter drops every line of it, so a week of staging
/// traffic measures nothing and nobody notices — the same failure mode
/// `the_default_filter_passes_durable_append_warnings_and_still_drops_other_ones`
/// pins for the durable-append worker, one level quieter. Revert the
/// `policy::shadow_floor=info` directive and this fails.
#[test]
fn the_default_filter_passes_the_shadow_floor_measurement() {
    use tracing_subscriber::layer::SubscriberExt;

    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(Captured(std::sync::Arc::clone(&captured)))
        .with(log_filter(None));

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(
            target: "policy::shadow_floor",
            "[policy:shadow-floor] agent=- tool='gmail_send_email' would_stop=irreversible_send \
             mode=Auto hitl=false issue=2147"
        );
        // An unrelated `info!` stays dropped: this is one named target,
        // not a global level bump.
        tracing::info!(target: "opencompany::unrelated", "ordinary chatter");
    });

    let events = captured.lock().expect("capture lock").clone();
    let seen = |target: &str, level: tracing::Level| {
        events.iter().any(|(t, l)| t == target && *l == level)
    };

    assert!(
        seen("policy::shadow_floor", tracing::Level::INFO),
        "the shadow-floor measurement must survive the default filter; captured {events:?}"
    );
    assert!(
        !seen("opencompany::unrelated", tracing::Level::INFO),
        "the exception is one target, not a global level bump; captured {events:?}"
    );
}

/// A `RUST_LOG` the operator set is theirs, even when part of it is junk.
///
/// `try_from_default_env` rejects the whole variable over one malformed
/// directive, so falling back to [`DEFAULT_LOG_FILTER`] on that error would
/// throw away every valid directive the operator wrote — turning a typo into
/// a silent, total loss of their logging configuration. [`log_filter`] parses
/// it lossily instead, which is what this binary did before the constant
/// existed. Flagged by review on PR #1186.
#[test]
fn a_malformed_directive_does_not_discard_the_rest_of_rust_log() {
    use tracing_subscriber::layer::SubscriberExt;

    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    // One good directive, one that cannot parse.
    let subscriber = tracing_subscriber::registry()
        .with(Captured(std::sync::Arc::clone(&captured)))
        .with(log_filter(Some(
            "opencompany::kept=info,@@@not a directive@@@",
        )));

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "opencompany::kept", "the operator asked for this");
    });

    let events = captured.lock().expect("capture lock").clone();
    assert!(
        events
            .iter()
            .any(|(t, l)| t == "opencompany::kept" && *l == tracing::Level::INFO),
        "the valid directive must survive the invalid one beside it; captured {events:?}"
    );
}

/// `RUST_LOG=` — set, but empty — must not silence the binary.
///
/// This is the sharper half of the same mistake as
/// `a_malformed_directive_does_not_discard_the_rest_of_rust_log`, and it is
/// worth its own test because it fails in the opposite direction from the
/// bug this file's constant exists to fix. `from_default_env` carries an
/// `ERROR` default directive; `try_from_default_env` carries none, so an
/// empty value parsed strictly yields a filter with no directives at all
/// and drops **everything** — errors included. That is worse than the
/// silence issue #450 is about, and it is reachable from an empty
/// environment variable in a compose file or a shell export.
#[test]
fn an_empty_rust_log_still_reports_errors() {
    use tracing_subscriber::layer::SubscriberExt;

    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(Captured(std::sync::Arc::clone(&captured)))
        .with(log_filter(Some("")));

    tracing::subscriber::with_default(subscriber, || {
        tracing::error!(target: "opencompany::anything", "something broke");
    });

    let events = captured.lock().expect("capture lock").clone();
    assert!(
        events
            .iter()
            .any(|(t, l)| t == "opencompany::anything" && *l == tracing::Level::ERROR),
        "an empty RUST_LOG must keep the ERROR default, not silence the binary; \
         captured {events:?}"
    );
}

/// Issue #1077: the `orphans` command refuses to run without a tenant
/// namespace.
///
/// Without `OPENCOMPANY_TENANT_ID` no durable owner rows are ever written,
/// so the report would claim every company is orphaned on every run. The
/// gate is the same condition that guards the owner-row write at
/// `register_company`, and it fires before storage is even opened — the
/// reviewer's false-positive case, with no database needed to hit it.
#[tokio::test]
async fn orphans_refuses_to_run_without_a_tenant_namespace() {
    let err = run_orphans_from(None, false, &opencompany::app::config::MapEnv::default())
        .await
        .unwrap_err();

    assert!(
        matches!(&err, opencompany::error::OpenCompanyError::Config(_)),
        "expected a Config refusal, got: {err:?}"
    );
}

// These use a var name no environment (local or CI) ever sets, so the
// env-var branch of `resolve_serve_base_url` never fires here — no
// `std::env::set_var` needed, and so nothing to race against the rest of
// this binary's tests.
const UNSET_VAR: &str = "OPENCOMPANY_TEST_RESOLVE_SERVE_BASE_URL_UNSET_PROBE";

/// Codex review finding on PR #2141: `doctor` (via
/// `app::config::resolve_base_url`) accepts a hosted tenant's backend URL
/// from `config.toml`, but `serve`'s manual `AppConfig` build checked only
/// the process environment — so a tenant configured entirely through
/// `config.toml` passed `doctor` and then refused to boot.
#[test]
fn serve_base_url_accepts_config_toml_for_a_hosted_tenant() {
    let resolved = resolve_serve_base_url(
        UNSET_VAR,
        opencompany::app::deployment::Deployment::HostedTenant,
        HostedDefault::Refuse,
        Some("https://toml.example".to_string()),
        "https://default.example".to_string(),
    )
    .expect("a config.toml value must satisfy the hosted-tenant gate");

    assert_eq!(resolved, "https://toml.example");
}

#[test]
fn serve_base_url_still_refuses_a_hosted_tenant_with_neither_env_nor_toml() {
    let err = resolve_serve_base_url(
        UNSET_VAR,
        opencompany::app::deployment::Deployment::HostedTenant,
        HostedDefault::Refuse,
        None,
        "https://default.example".to_string(),
    )
    .unwrap_err();

    assert!(matches!(
        &err,
        opencompany::error::OpenCompanyError::Config(_)
    ));
}

#[test]
fn serve_base_url_ignores_a_blank_config_toml_value() {
    let err = resolve_serve_base_url(
        UNSET_VAR,
        opencompany::app::deployment::Deployment::HostedTenant,
        HostedDefault::Refuse,
        Some("   ".to_string()),
        "https://default.example".to_string(),
    )
    .unwrap_err();

    assert!(matches!(
        &err,
        opencompany::error::OpenCompanyError::Config(_)
    ));
}

/// **The boot path the tenant container actually takes.**
///
/// `serve` builds `AppConfig` field-by-field through this twin, so a rule
/// relaxed only in `app::config::resolve_base_url` would leave every
/// hosted tenant still refusing to start. Observed on staging: a tenant
/// rolled onto an image carrying PR #2141 crash-looped with
/// `TINYPLACE_API_URL is not set`, for a company whose manifest has no
/// `[place]` block at all.
#[test]
fn serve_base_url_lets_a_hosted_tenant_default_an_opt_in_backend() {
    let resolved = resolve_serve_base_url(
        UNSET_VAR,
        opencompany::app::deployment::Deployment::HostedTenant,
        HostedDefault::Allow,
        None,
        "https://default.example".to_string(),
    )
    .expect("an opt-in backend must not stop a tenant from booting");

    assert_eq!(resolved, "https://default.example");
}

#[test]
fn serve_base_url_self_hosted_still_defaults_when_neither_env_nor_toml() {
    let resolved = resolve_serve_base_url(
        UNSET_VAR,
        opencompany::app::deployment::Deployment::SelfHosted,
        HostedDefault::Refuse,
        None,
        "https://default.example".to_string(),
    )
    .expect("self-hosted must not refuse to boot");

    assert_eq!(resolved, "https://default.example");
}
