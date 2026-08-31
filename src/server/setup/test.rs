//! HTTP-level tests for the first-run setup surface.
//!
//! The two things worth guarding hardest are the ones that are invisible from a
//! running host if they break: that an env-owned field cannot be "configured"
//! into a file nothing will read, and that the open-while-unconfigured access
//! gate closes the moment either of its two conditions stops holding.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::app::config::MapEnv;
use crate::company::CompanyManifest;
use crate::ports::CompanyStore;
use crate::ports::types::{CompanyId, CompanyRecord, SecretValue};
use crate::runtime::RuntimeBuilder;
use crate::server::ops::ConnectionsRuntime;
use crate::server::ops::mailer::{MailCredentials, RecordingMailSender};
use crate::server::ops::smtp::{SmtpCredentials, SmtpSecurity};
use crate::server::router;
use crate::{AppConfig, AppState};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("opencompany-setup-")
        .tempdir()
        .expect("tempdir")
}

fn manifest() -> CompanyManifest {
    toml::from_str("[company]\nname = \"Acme\"\n").unwrap()
}

/// A loopback-bound host with an empty registry: a genuine first run.
fn fresh_state(home: &std::path::Path) -> AppState {
    AppState::new(AppConfig {
        bind: "127.0.0.1:8080".to_string(),
        ..AppConfig::default()
    })
    .with_home(home.to_path_buf())
}

/// A loopback host with a mail transport wired — the shape where a magic link
/// is genuinely mailed rather than handed back in the response.
fn state_with_mail(home: &std::path::Path) -> AppState {
    let connections = ConnectionsRuntime::new()
        .with_mail(Arc::new(RecordingMailSender::new()))
        .with_mail_credentials(MailCredentials::Smtp(SmtpCredentials {
            host: "smtp.test".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "u".into(),
            password: SecretValue("p".into()),
            from_name: "Acme".into(),
            from_email: "noreply@acme.test".into(),
        }));
    fresh_state(home).with_connections(connections)
}

/// A routable host, where the anonymous gate must never open.
fn routable_state(home: &std::path::Path) -> AppState {
    AppState::new(AppConfig {
        bind: "0.0.0.0:8080".to_string(),
        ..AppConfig::default()
    })
    .with_home(home.to_path_buf())
}

/// Registers `acme`, as a host that has already been used would have.
async fn with_company(state: &AppState, home: &std::path::Path) -> CompanyId {
    let store = crate::store::FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_tool_grants: None,
            overlay_desk_tools: std::collections::BTreeMap::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
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
    state.registry().insert(id.clone(), Arc::new(runtime));
    id
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn get_setup(state: AppState) -> (StatusCode, serde_json::Value) {
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/setup")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

/// Reads the payload with an admin session, for the routable host — where the
/// anonymous gate is (correctly) shut and there is no other way in.
async fn get_setup_as_admin(state: AppState) -> serde_json::Value {
    let cookie =
        crate::server::test_support::seed_session(&state, "acme", crate::ports::UserRole::Admin)
            .await;
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/setup")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    body_json(response).await
}

async fn post_setup(state: AppState, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/setup")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

fn field<'a>(dto: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    dto["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .find(|f| f["key"] == key)
        .unwrap_or_else(|| panic!("no field `{key}` in the payload"))
}

// ---------------------------------------------------------------------------
// The payload
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fresh_host_reports_itself_unconfigured() {
    let home_dir = home();
    let (status, dto) = get_setup(fresh_state(home_dir.path())).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(dto["complete"], false);
    assert!(
        dto["companies"].as_array().unwrap().is_empty(),
        "a first run has no company yet"
    );
    assert!(
        dto["config_path"]
            .as_str()
            .unwrap()
            .ends_with("config.toml"),
        "the flow must name the file it writes"
    );
}

/// The template catalog is the shipped preset list, and each entry carries
/// enough to draw a card without the console parsing manifests itself.
#[tokio::test]
async fn the_payload_lists_the_shipped_templates() {
    let home_dir = home();
    let (_, dto) = get_setup(fresh_state(home_dir.path())).await;

    let templates = dto["templates"].as_array().unwrap();
    assert_eq!(
        templates.len(),
        crate::desktop::PRESETS.len(),
        "every shipped preset must be offered"
    );
    let default = templates
        .iter()
        .find(|t| t["id"] == crate::desktop::DEFAULT_PRESET_ID)
        .expect("the default preset is in the catalog");
    assert!(
        default["agent_count"].as_u64().unwrap() > 0,
        "a template's roster size must be readable: {default}"
    );
}

/// ACP is a cargo feature whose transport is mounted under that feature, so
/// the flow reports build state rather than offering a switch. A flag that
/// claimed otherwise would send a client to an endpoint that 404s.
#[tokio::test]
async fn the_payload_reports_acp_as_build_state_not_a_setting() {
    let home_dir = home();
    let (_, dto) = get_setup(fresh_state(home_dir.path())).await;

    assert_eq!(dto["build"]["acp_in_build"], cfg!(feature = "acp"));
    assert_eq!(
        dto["build"]["acp_transport_mounted"],
        cfg!(feature = "acp"),
        "the flag must match whether the /acp handler is actually mounted"
    );
    assert!(
        !dto["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["key"].as_str().unwrap().contains("acp")),
        "ACP must not appear as a writable field"
    );
}

/// `none` has no sign-in. Offering it on a routable bind would produce a choice
/// the next boot refuses, so it is withheld there.
#[tokio::test]
async fn none_is_offered_only_on_a_loopback_host() {
    let home_dir = home();
    let (_, local) = get_setup(fresh_state(home_dir.path())).await;
    assert!(
        local["auth_modes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("none")),
        "loopback may choose `none`"
    );

    // Read the routable host's modes through the admin path, since the
    // anonymous gate is (correctly) shut there.
    let state = routable_state(home_dir.path());
    with_company(&state, home_dir.path()).await;
    let cookie =
        crate::server::test_support::seed_session(&state, "acme", crate::ports::UserRole::Admin)
            .await;
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/setup")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let dto = body_json(response).await;
    assert!(
        !dto["auth_modes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("none")),
        "a routable host must not offer an unauthenticated console: {dto}"
    );
}

/// A laptop with no SMTP is not a broken host — it is the one shape where the
/// honest hand-off is a link the operator opens themselves. The wizard has to
/// be able to tell that apart from a host where a magic link simply goes
/// nowhere, and only the payload can say which it is on.
#[tokio::test]
async fn mail_on_a_loopback_host_with_no_transport_reports_the_code_echo() {
    let home_dir = home();
    let (_, dto) = get_setup(fresh_state(home_dir.path())).await;

    assert_eq!(dto["mail"]["wired"], false, "nothing is configured: {dto}");
    assert_eq!(
        dto["mail"]["echoes_code"], true,
        "a loopback host hands the code back in the response: {dto}"
    );
}

/// The dead end. A routable host with no transport can neither mail a link nor
/// echo one, so a wizard that offered the link form here would be offering a
/// sign-in that arrives nowhere.
#[tokio::test]
async fn mail_on_a_routable_host_with_no_transport_reports_neither() {
    let home_dir = home();
    let state = routable_state(home_dir.path());
    with_company(&state, home_dir.path()).await;
    let dto = get_setup_as_admin(state).await;

    assert_eq!(dto["mail"]["wired"], false, "{dto}");
    assert_eq!(
        dto["mail"]["echoes_code"], false,
        "a routable host must not be described as echoing codes: {dto}"
    );
}

/// With a transport wired the link is a real send, and the echo stops — the
/// same either/or the login route itself branches on.
#[tokio::test]
async fn mail_with_a_transport_wired_reports_a_real_send() {
    let home_dir = home();
    let (_, dto) = get_setup(state_with_mail(home_dir.path())).await;

    assert_eq!(dto["mail"]["wired"], true, "{dto}");
    assert_eq!(
        dto["mail"]["echoes_code"], false,
        "a wired transport is delivered to, never echoed: {dto}"
    );
}

/// `auth_modes` says which modes are *legal*, not which are convenient today.
/// A host with no SMTP still runs `email` mode perfectly well over hub OAuth
/// and passwords, so withholding the mode here would take away a working
/// sign-in on the strength of a transport it does not need. `mail` is the field
/// that says what the mailbox path can do; this one must stay a policy answer.
#[tokio::test]
async fn email_is_still_offered_on_a_host_that_cannot_mail() {
    let home_dir = home();
    let state = routable_state(home_dir.path());
    with_company(&state, home_dir.path()).await;
    let dto = get_setup_as_admin(state).await;

    assert_eq!(
        dto["mail"]["wired"], false,
        "this host is the one that cannot mail: {dto}"
    );
    assert!(
        dto["auth_modes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("email")),
        "email mode does not depend on a transport: {dto}"
    );
}

/// A credential's status is reportable; its bytes are not.
#[tokio::test]
async fn a_secret_field_never_echoes_its_value() {
    let home_dir = home();
    std::fs::write(
        home_dir.path().join("config.toml"),
        "tinyhumans_api_key = \"sk-do-not-echo\"\n",
    )
    .unwrap();

    let (_, dto) = get_setup(fresh_state(home_dir.path())).await;

    let key = field(&dto, "tinyhumans_api_key");
    assert_eq!(key["secret"], true);
    assert!(key["value"].is_null(), "the value must not be echoed");
    assert!(
        !dto.to_string().contains("sk-do-not-echo"),
        "the secret must appear nowhere in the payload"
    );
}

// ---------------------------------------------------------------------------
// Precedence honesty
// ---------------------------------------------------------------------------

/// A value in the file is reported as owned by `config.toml` and stays editable.
#[tokio::test]
async fn a_file_owned_field_is_editable() {
    let home_dir = home();
    std::fs::write(
        home_dir.path().join("config.toml"),
        "bind = \"127.0.0.1:9999\"\n",
    )
    .unwrap();

    let (_, dto) = get_setup(fresh_state(home_dir.path())).await;

    let bind = field(&dto, "bind");
    assert_eq!(bind["layer"], "config.toml");
    assert_eq!(bind["value"], "127.0.0.1:9999");
    assert_eq!(bind["editable"], true);
    assert_eq!(
        bind["requires_restart"], true,
        "a bind change only takes effect at the next boot"
    );
}

/// A field nothing sets falls to its built-in default and is still editable.
#[tokio::test]
async fn an_unset_field_reports_its_default_layer() {
    let home_dir = home();
    let (_, dto) = get_setup(fresh_state(home_dir.path())).await;

    let quota = field(&dto, "workspace.tree_quota_gb");
    assert_eq!(quota["layer"], "default");
    assert!(quota["value"].is_null());
    assert_eq!(quota["editable"], true);
}

/// Writing a field the environment owns would produce a file nothing reads, so
/// the flow refuses rather than reporting a success that changes nothing at the
/// next boot. This is the failure mode the whole surface exists to prevent.
/// Driven through the injected [`EnvSource`] rather than `std::env::set_var`.
/// Tests share one process, so mutating the real environment would leak into
/// whichever unrelated test happened to resolve config at the same moment —
/// which is exactly why `resolve` takes this seam in the first place.
#[tokio::test]
async fn a_write_to_an_env_owned_field_is_refused() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());
    let env = MapEnv::new([("OPENCOMPANY_AUTH_MODE", "wallet")]);

    let dto = serde_json::to_value(super::snapshot(&state, &env).unwrap()).unwrap();
    let mode = field(&dto, "auth_mode");
    assert_eq!(mode["layer"], "env");
    assert_eq!(
        mode["editable"], false,
        "an env-owned field must render read-only"
    );

    let err = super::apply_inner(
        &state,
        super::SetupRequest {
            fields: [("auth_mode".to_string(), Some("email".to_string()))]
                .into_iter()
                .collect(),
            template: None,
            company: None,
            name: None,
            admin_email: None,
        },
        &env,
    )
    .await
    .expect_err("writing an env-owned field must be refused");

    assert_eq!(err.code(), "conflict");
    assert!(
        err.to_string().contains("environment"),
        "the refusal must explain why: {err}"
    );
    assert!(
        !home_dir.path().join("config.toml").exists(),
        "a refused apply must write nothing"
    );
    assert!(
        !state.setup_complete(),
        "a refused apply must not mark the instance configured"
    );
}

// ---------------------------------------------------------------------------
// Applying
// ---------------------------------------------------------------------------

#[tokio::test]
async fn applying_writes_the_file_and_marks_setup_complete() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({
            "fields": { "bind": "127.0.0.1:9100", "workspace.max_blob_mb": "64" }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["complete"], true);
    assert!(
        body["restart_required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("bind")),
        "the flow must say what is still pending: {body}"
    );

    let file = crate::app::config::ConfigFile::load(home_dir.path())
        .unwrap()
        .unwrap();
    assert_eq!(file.bind.as_deref(), Some("127.0.0.1:9100"));
    assert_eq!(file.workspace.max_blob_mb, Some(64.0));
    assert!(
        file.setup_completed_at.is_some(),
        "completion must be recorded in the file, not just in memory"
    );
    assert!(state.setup_complete(), "the live flag must flip too");
}

/// The template choice seeds the operator's pick, not the hardcoded default.
#[tokio::test]
async fn applying_seeds_the_chosen_template() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({ "fields": {}, "template": "agentic_law_firm" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["seeded_company"].is_string(),
        "a company must have been seeded: {body}"
    );
    assert_eq!(state.registry().len(), 1);

    let id = state.registry().list().into_iter().next().unwrap();
    let record = crate::store::FsCompanyStore::new(home_dir.path().to_path_buf())
        .load(&id)
        .await
        .unwrap()
        .expect("the seeded company is persisted");
    assert_eq!(
        record
            .template_provenance
            .as_ref()
            .map(|p| p.source_id.as_str()),
        Some("agentic_law_firm"),
        "provenance must record which template this install started from"
    );
}

/// Choosing "no sign-in" must actually mean no sign-in, immediately.
///
/// The mode is resolved once, at build, and cached on the runtime, so writing
/// `auth_mode` to `config.toml` alone only takes effect at the next boot. That
/// left an operator who picked "no sign-in" looking at a login form on a host
/// they had just told not to have one — the setting appeared to save and did
/// nothing. Setup now makes it live before it builds anything with it.
#[tokio::test]
async fn choosing_no_sign_in_applies_to_the_company_it_seeds() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({
            "fields": { "auth_mode": "none" },
            "template": "agentic_law_firm",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let id = state.registry().list().into_iter().next().expect("seeded");
    assert_eq!(
        state.registry().get(&id).unwrap().auth_mode(),
        crate::app::config::AuthMode::None,
        "the seeded company must be built with the mode the operator just chose, \
         not the one the process booted with"
    );
    assert!(
        !body["restart_required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("auth_mode")),
        "it applied live, so telling the operator to restart for it would be a lie: {body}"
    );
}

/// The host-wide mode is what a later build reads, not the frozen boot value.
#[tokio::test]
async fn the_chosen_mode_becomes_the_hosts_live_mode() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());
    assert_eq!(state.auth_mode_override(), None, "nothing set at boot");

    let (status, _) = post_setup(
        state.clone(),
        serde_json::json!({ "fields": { "auth_mode": "wallet" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state.auth_mode_override(),
        Some(crate::app::config::AuthMode::Wallet),
    );

    // Clearing it hands the answer back to each manifest's `[users].mode`.
    let (status, _) = post_setup(
        state.clone(),
        serde_json::json!({ "fields": { "auth_mode": null } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state.auth_mode_override(), None);
}

/// A host with no rebuilder cannot re-apply the mode to a company it already
/// built, so it must say a restart is needed rather than claim success.
#[tokio::test]
async fn an_existing_company_that_cannot_rebuild_reports_a_restart() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());
    with_company(&state, home_dir.path()).await;

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({ "fields": { "auth_mode": "none" } }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["restart_required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("auth_mode")),
        "no rebuilder is wired in this fixture, so the honest answer is `restart`: {body}"
    );
}

/// A re-run must never hand the operator a second starter company.
#[tokio::test]
async fn applying_does_not_seed_when_a_company_already_exists() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());
    with_company(&state, home_dir.path()).await;

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({ "fields": {}, "template": "agentic_law_firm" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["seeded_company"].is_null(), "{body}");
    assert_eq!(state.registry().len(), 1, "still exactly one company");
}

#[tokio::test]
async fn an_unknown_field_is_refused() {
    let home_dir = home();
    let (status, body) = post_setup(
        fresh_state(home_dir.path()),
        serde_json::json!({ "fields": { "not_a_setting": "x" } }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("not_a_setting"));
    assert!(!home_dir.path().join("config.toml").exists());
}

#[tokio::test]
async fn a_malformed_value_is_refused_before_anything_is_written() {
    let home_dir = home();
    let (status, body) = post_setup(
        fresh_state(home_dir.path()),
        serde_json::json!({ "fields": { "workspace.max_blob_mb": "lots" } }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("a number"),
        "{body}"
    );
    assert!(
        !home_dir.path().join("config.toml").exists(),
        "validation happens before the write"
    );
}

/// An unparseable `auth_mode` aborts boot, so a typo here would leave a host
/// that will not come back up. It is caught at the write instead.
#[tokio::test]
async fn an_invalid_auth_mode_is_refused() {
    let home_dir = home();
    let (status, body) = post_setup(
        fresh_state(home_dir.path()),
        serde_json::json!({ "fields": { "auth_mode": "sso" } }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(!home_dir.path().join("config.toml").exists());
}

/// An unresolvable `bind` (a malformed port, here) would abort `TcpListener`
/// at the next boot, so it is refused at the write instead — the same
/// treatment `auth_mode` gets just above.
#[tokio::test]
async fn an_unresolvable_bind_is_refused() {
    let home_dir = home();
    let (status, body) = post_setup(
        fresh_state(home_dir.path()),
        serde_json::json!({ "fields": { "bind": "127.0.0.1:notaport" } }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        !home_dir.path().join("config.toml").exists(),
        "validation happens before the write"
    );
}

/// The boot path resolves `bind` through `ToSocketAddrs`, which accepts a
/// hostname alongside a literal IP — so `localhost:PORT` must be accepted
/// here too, not just an IP-shaped address.
#[tokio::test]
async fn a_hostname_bind_is_accepted() {
    let home_dir = home();
    let (status, body) = post_setup(
        fresh_state(home_dir.path()),
        serde_json::json!({ "fields": { "bind": "localhost:8080" } }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(home_dir.path().join("config.toml").exists());
}

/// A failed persist must leave the process exactly as it was: no live
/// auth-mode override, no seeded company, and `setup_complete` still false.
/// Before #908's fix, `apply_inner` set the live override and seeded the
/// company *before* calling `write_config_toml`, so a write failure returned
/// an error while the live host had already moved — breaking both the module
/// doc's "one transaction" claim and `AppliedDto::complete`'s "a partial
/// apply is an error, not a result".
///
/// The write is forced to fail by making `config.toml` a directory: the read
/// that opens `write_config_toml` fails before anything is touched, the same
/// shape a permission or disk-full failure would take.
#[tokio::test]
async fn a_failed_write_leaves_no_live_state_behind() {
    let home_dir = home();
    std::fs::create_dir(home_dir.path().join("config.toml")).unwrap();

    let state = fresh_state(home_dir.path());
    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({
            "fields": { "auth_mode": "wallet" },
            "template": "agentic_marketing_agency",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        state.auth_mode_override().is_none(),
        "the live auth-mode override must not survive a failed write"
    );
    assert!(
        state.registry().is_empty(),
        "no company may be seeded when the write that should record it failed"
    );
    assert!(
        !state.setup_complete(),
        "setup must not read as complete when its write failed"
    );
}

/// Clearing a field removes the key so the layer below applies, rather than
/// writing a blank that shadows it.
#[tokio::test]
async fn clearing_a_field_removes_the_key() {
    let home_dir = home();
    std::fs::write(
        home_dir.path().join("config.toml"),
        "public_url = \"https://old.example\"\n",
    )
    .unwrap();

    let (status, _) = post_setup(
        fresh_state(home_dir.path()),
        serde_json::json!({ "fields": { "public_url": null } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let file = crate::app::config::ConfigFile::load(home_dir.path())
        .unwrap()
        .unwrap();
    assert!(file.public_url.is_none(), "the key must be gone");
}

// ---------------------------------------------------------------------------
// Access control
// ---------------------------------------------------------------------------

/// Both conditions, not either: an unconfigured host that is *routable* is not
/// open. Otherwise a freshly deployed instance would be configurable by whoever
/// reached it first.
#[tokio::test]
async fn an_unconfigured_but_routable_host_is_not_open() {
    let home_dir = home();
    let (status, _) = get_setup(routable_state(home_dir.path())).await;

    assert_ne!(
        status,
        StatusCode::OK,
        "a routable unconfigured host must not serve its configuration anonymously"
    );
}

/// A routable host must not accept an anonymous write either.
#[tokio::test]
async fn an_unconfigured_but_routable_host_refuses_an_anonymous_write() {
    let home_dir = home();
    let (status, _) = post_setup(
        routable_state(home_dir.path()),
        serde_json::json!({ "fields": { "bind": "0.0.0.0:1234" } }),
    )
    .await;

    assert_ne!(status, StatusCode::OK);
    assert!(
        !home_dir.path().join("config.toml").exists(),
        "nothing may be written by an unauthorized caller"
    );
}

/// A loopback-*configured* bind is not the same claim as a loopback
/// *request*: an undeclared reverse proxy in front of a loopback-bound
/// listener still presents a loopback peer to `TcpListener`, but the console
/// review on #908 flagged that `is_local_only()` alone cannot see that — it
/// only inspects the configured bind and `public_url`, never the request
/// itself. `request_looks_local` is the second gate that closes that gap: a
/// non-loopback peer on an otherwise loopback-configured, unconfigured host
/// must still be refused.
#[tokio::test]
async fn a_loopback_configured_host_still_refuses_a_non_loopback_peer() {
    use axum::extract::ConnectInfo;

    let home_dir = home();
    let app = router(fresh_state(home_dir.path()));

    let mut req = Request::builder()
        .uri("/api/v1/setup")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(
        "203.0.113.7:54321".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(req).await.unwrap();
    assert_ne!(
        response.status(),
        StatusCode::OK,
        "a non-loopback peer must not pass the anonymous setup gate even on a \
         loopback-configured bind"
    );
}

/// The other half of the same gap: a same-host reverse proxy connects to a
/// loopback-bound listener over loopback too, so the peer alone cannot catch
/// an *undeclared* one — only a proxy-forwarding header can. Any request
/// carrying one must be refused just as a non-loopback peer is.
#[tokio::test]
async fn a_loopback_configured_host_refuses_a_forwarded_request() {
    let home_dir = home();
    let app = router(fresh_state(home_dir.path()));

    let req = Request::builder()
        .uri("/api/v1/setup")
        .header("x-forwarded-for", "203.0.113.7")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_ne!(
        response.status(),
        StatusCode::OK,
        "a request carrying a proxy-forwarding header must not pass the \
         anonymous setup gate even on a loopback-configured bind"
    );
}

/// The other half: a configured host is closed even on loopback, so a page in
/// the browser cannot rewrite a laptop's settings after setup has run.
#[tokio::test]
async fn a_configured_loopback_host_is_closed() {
    let home_dir = home();
    let state = fresh_state(home_dir.path()).with_setup_complete(true);
    with_company(&state, home_dir.path()).await;

    let (status, _) = get_setup(state.clone()).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "once setup is done, re-running it takes an admin"
    );

    let (status, _) = post_setup(state, serde_json::json!({ "fields": {} })).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// An admin may re-run setup on a configured host — the "Run setup again" path.
#[tokio::test]
async fn an_admin_may_re_run_setup_on_a_configured_host() {
    let home_dir = home();
    let state = fresh_state(home_dir.path()).with_setup_complete(true);
    with_company(&state, home_dir.path()).await;
    let cookie =
        crate::server::test_support::seed_session(&state, "acme", crate::ports::UserRole::Admin)
            .await;

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/setup")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let dto = body_json(response).await;
    assert_eq!(dto["complete"], true);
}

/// A member is not an admin, and host-level configuration is an admin action.
#[tokio::test]
async fn a_member_may_not_re_run_setup() {
    let home_dir = home();
    let state = fresh_state(home_dir.path()).with_setup_complete(true);
    with_company(&state, home_dir.path()).await;
    let cookie =
        crate::server::test_support::seed_session(&state, "acme", crate::ports::UserRole::Member)
            .await;

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/setup")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// Regression: `serve --company <dir>` predates this flow entirely, so every
/// existing deployment has companies and no `setup_completed_at`. `/spec`
/// reporting the raw stamp sent all of them into the wizard on their next
/// console load — and because the wizard replaces the console outright, the
/// end-to-end suite sat waiting on selectors that would never appear, until the
/// 30-minute job timeout killed it.
///
/// The two questions come apart deliberately: `/spec` answers "must the console
/// offer setup", while `AppState::setup_complete` stays the literal stamp,
/// because `authorize` needs "has an admin to check against", not "has been
/// configured".
#[tokio::test]
async fn spec_reports_setup_complete_once_a_company_is_registered() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());

    assert!(
        !state.spec().setup_complete,
        "precondition: no stamp and no companies is the genuine first run"
    );

    with_company(&state, home_dir.path()).await;

    assert!(
        state.spec().setup_complete,
        "a host already serving a company has something to open, so the \
         console must not replace it with the first-run wizard"
    );
    assert!(
        !state.setup_complete(),
        "the raw stamp stays false — `authorize` reads it, and a host with \
         companies authorizes through its admin rather than anonymously"
    );
}

// ---------------------------------------------------------------------------
// The roster proposal, before any company exists
// ---------------------------------------------------------------------------

async fn post_roster(state: AppState, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/setup/roster")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

/// A template the operator picked is the roster they get back, not the curated
/// team matched from their words.
///
/// The two are different rosters and only one of them was chosen by anybody.
/// Picking "Agentic Marketing Agency" — a card that says eight teammates — and
/// skipping the model step returned the five-person curated marketing team,
/// under a heading naming the template. Asserted against the template's own
/// count rather than a literal, so a template that gains a teammate does not
/// fail this.
#[tokio::test]
async fn a_picked_template_proposes_its_own_roster() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());
    let expected = crate::desktop::preset("agentic_marketing_agency")
        .expect("a bundled template")
        .manifest_parsed()
        .expect("it parses")
        .agents;

    let (status, body) = post_roster(
        state,
        serde_json::json!({
            "template": "agentic_marketing_agency",
            "industry": "",
            "teamHint": "",
            "automate": "campaign briefs and weekly reporting",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["source"], "preset",
        "the console needs to know this roster can be seeded as the template itself: {body}"
    );
    assert_eq!(
        body["agents"].as_array().map(Vec::len),
        Some(expected.len()),
        "the roster on the review screen must be the roster the card advertised: {body}"
    );
    let roles: Vec<&str> = body["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["role"].as_str().unwrap())
        .collect();
    assert!(
        expected
            .iter()
            .all(|agent| roles.contains(&agent.role.as_str())),
        "every teammate the template declares must be on it: {roles:?}"
    );
}

/// The curated path is untouched where no template was picked.
///
/// The pair matters: the fix above must not become "always ship a preset", or
/// an operator who typed their business in their own words and never opened the
/// template list would get a roster matched by slug instead of by what they
/// wrote.
#[tokio::test]
async fn answers_without_a_template_still_propose_the_curated_team() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());

    let (status, body) = post_roster(
        state,
        serde_json::json!({
            "industry": "E-commerce",
            "teamHint": "",
            "automate": "order dispatch and returns",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], "fallback", "{body}");
}

/// Setup seeds the template itself when the console sends a slug, under the
/// name the operator typed.
///
/// Both halves are the point. The template arm was unreachable from the console
/// — the wizard only ever sent a designed company — so a picked template was
/// rebuilt from the review screen and lost the belt and prompts it ships. And
/// the name was derived from the *industry* answer with no way to say
/// otherwise, on a field that mints the company id.
#[tokio::test]
async fn applying_a_template_seeds_it_under_the_name_the_operator_chose() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({
            "fields": {},
            "template": "agentic_marketing_agency",
            "name": "Northwind Studio",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["seeded_company"], "northwind-studio",
        "the id is minted from the name the operator gave: {body}"
    );

    let id = crate::ports::types::CompanyId::new("northwind-studio");
    let runtime = state
        .registry()
        .get(&id)
        .expect("the seeded company is registered");
    // Read back off the store rather than off the runtime: what matters is the
    // bundle the next launch adopts, which is what `adopt_companies` reads.
    let record = runtime
        .store()
        .load(&id)
        .await
        .expect("the bundle is readable")
        .expect("the bundle exists");
    let manifest = record.manifest;
    assert_eq!(manifest.company.name, "Northwind Studio");
    // Every teammate the template declares, by role. Not a count: a registered
    // company's stored manifest also carries the roster `globals/` contributes,
    // so an equality here would be asserting the size of something this change
    // has nothing to do with.
    let template_roles: Vec<String> = crate::desktop::preset("agentic_marketing_agency")
        .unwrap()
        .manifest_parsed()
        .unwrap()
        .agents
        .iter()
        .map(|agent| agent.role.clone())
        .collect();
    let seeded_roles: Vec<String> = manifest.agents.iter().map(|a| a.role.clone()).collect();
    assert!(
        template_roles
            .iter()
            .all(|role| seeded_roles.contains(role)),
        "a renamed template is still that template's roster: {seeded_roles:?}"
    );
}

/// A template seed carries the address that will administer it.
///
/// No shipped product template names an admin, so on a host that asks people to
/// sign in, seeding one without this produces a company nobody can administer:
/// setup completes, email sign-in is on, and the address the operator typed two
/// screens earlier is ineligible. Only reachable since a picked template began
/// being seeded as itself — before that every company came through the designed
/// path, which has always written it.
#[tokio::test]
async fn a_template_seed_names_the_operator_as_its_admin() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({
            "fields": {},
            "template": "agentic_law_firm",
            "admin_email": "ada@example.com",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let id = crate::ports::types::CompanyId::new("agentic-law-firm");
    let record = state
        .registry()
        .get(&id)
        .expect("the seeded company is registered")
        .store()
        .load(&id)
        .await
        .expect("the bundle is readable")
        .expect("the bundle exists");
    assert_eq!(
        record.manifest.users.admins,
        vec!["ada@example.com".to_string()],
        "a company that lists nobody cannot be signed into"
    );
}

/// A pasted paragraph is truncated, not turned into a directory nobody can
/// write.
///
/// `company_id_from_name` keeps every alphanumeric character it is handed, and
/// that id becomes one component under the store — so an unbounded name fails
/// the apply while writing the bundle, on most filesystems at 255 bytes. The
/// derivation has always clamped at `MAX_COMPANY_NAME`; a name the operator
/// supplies now meets the same bound.
#[tokio::test]
async fn a_very_long_name_is_bounded_before_it_becomes_an_id() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());
    let long = "Northwind ".repeat(40);

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({
            "fields": {},
            "template": "agentic_law_firm",
            "name": long,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["seeded_company"]
        .as_str()
        .expect("a company was seeded");
    assert!(
        id.len() <= crate::company::setup::MAX_COMPANY_NAME,
        "the id is a directory component and must stay one: {id}"
    );
    let registered = state
        .registry()
        .get(&crate::ports::types::CompanyId::new(id))
        .expect("the seeded company is registered");
    let record = registered
        .store()
        .load(&crate::ports::types::CompanyId::new(id))
        .await
        .expect("the bundle is readable")
        .expect("the bundle exists");
    assert!(
        record.manifest.company.name.chars().count() <= crate::company::setup::MAX_COMPANY_NAME,
        "the name is bounded too, not just the id: {}",
        record.manifest.company.name
    );
}

/// A blank name is not a name.
///
/// `company_id_from_name` slugs an empty string to the literal id `company`, so
/// obeying a cleared field would produce a company called nothing at an id
/// naming nothing. The template's own name is the better answer to "I typed no
/// name" than that is.
#[tokio::test]
async fn a_blank_name_falls_back_to_the_templates_own() {
    let home_dir = home();
    let state = fresh_state(home_dir.path());

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({
            "fields": {},
            "template": "agentic_law_firm",
            "name": "   ",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["seeded_company"], "agentic-law-firm", "{body}");
}

/// The wizard's whole reason for a second route: it needs a roster *before*
/// there is a company to scope one to. The company-scoped twin resolves a
/// `CompanyRuntime` and would 404 here.
#[tokio::test]
async fn a_roster_is_proposed_before_any_company_exists() {
    let home = home();
    let state = fresh_state(home.path());
    assert!(state.registry().is_empty(), "the premise: no company yet");

    let (status, body) = post_roster(
        state,
        serde_json::json!({
            "industry": "E-commerce — I sell homeware online",
            "automate": "Meta ads, order dispatch, daily reports",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["template"], "ecommerce", "{body}");
    let agents = body["agents"].as_array().expect("agents");
    assert!(
        (4..=6).contains(&agents.len()),
        "a proposal must be a workable team, got {}: {body}",
        agents.len()
    );
    // Every row has to be directly usable as an apply's roster, so a missing
    // field would surface as a half-built company rather than as a 400 here.
    for agent in agents {
        for key in ["name", "role", "description"] {
            assert!(
                agent[key].as_str().is_some_and(|v| !v.trim().is_empty()),
                "agent is missing `{key}`: {agent}"
            );
        }
    }
}

/// The default build links no harness, so the curated team is the whole answer.
/// It must still be a real team and must say where it came from — an operator
/// shown a canned roster with no indication judges the product on a team it
/// never designed.
#[tokio::test]
async fn with_no_model_the_curated_team_ships_and_says_so() {
    let home = home();
    let (status, body) = post_roster(
        fresh_state(home.path()),
        serde_json::json!({ "industry": "zzzz qqqq" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["template"], "generic", "{body}");
    assert_eq!(
        body["source"], "fallback",
        "the default build has no harness, so nothing designed this: {body}"
    );
}

/// An operator who types nothing still gets a team. The last two questions are
/// skippable by design, and stranding someone on the wizard is worse than a
/// generic roster.
#[tokio::test]
async fn an_empty_body_still_yields_a_team() {
    let home = home();
    let (status, body) = post_roster(fresh_state(home.path()), serde_json::json!({})).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["agents"].as_array().expect("agents").len() >= 4,
        "{body}"
    );
}

/// The proposal creates nothing. The wizard shows it for review first, and the
/// company is built by the apply — so a wizard abandoned at the review step
/// leaves the host exactly as it was.
#[tokio::test]
async fn proposing_creates_no_company() {
    let home = home();
    let state = fresh_state(home.path());
    let (status, _) =
        post_roster(state.clone(), serde_json::json!({ "industry": "software" })).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        state.registry().is_empty(),
        "the proposal route must not register a company"
    );
}

/// The same gate the rest of this flow uses: open while unconfigured on
/// loopback, closed on a routable host where it would let whoever reached a
/// fresh deployment first drive it.
#[tokio::test]
async fn a_routable_host_refuses_an_anonymous_proposal() {
    let home = home();
    let (status, _) = post_roster(
        routable_state(home.path()),
        serde_json::json!({ "industry": "software" }),
    )
    .await;

    assert_ne!(
        status,
        StatusCode::OK,
        "an unauthenticated caller must not reach this on a routable host"
    );
}

// ---------------------------------------------------------------------------
// Applying a company the wizard designed
// ---------------------------------------------------------------------------

/// The manifest as it was persisted. `CompanyRuntime` exposes no accessor, and
/// the record is the thing a restart would read back anyway.
async fn seeded_manifest(home: &std::path::Path, id: &str) -> CompanyManifest {
    let store = crate::store::FsCompanyStore::new(home.to_path_buf());
    store
        .load(&CompanyId::new(id))
        .await
        .expect("load")
        .expect("the seeded company has a record")
        .manifest
}

fn designed_company(email: Option<&str>) -> serde_json::Value {
    let mut company = serde_json::json!({
        "industry": "E-commerce — I sell homeware online",
        "automate": "Meta ads, order dispatch",
        "agents": [
            { "name": "Meta Ads", "role": "Meta Ads Specialist", "description": "Campaigns and budgets." },
            { "name": "Dispatch", "role": "Order Dispatch Coordinator", "description": "Paid to delivered." },
            { "name": "Accounts", "role": "Accountant", "description": "Margins and spend." },
            { "name": "Ops", "role": "Operations Lead", "description": "Unblocks the team." }
        ]
    });
    if let Some(email) = email {
        company["adminEmail"] = serde_json::Value::String(email.to_string());
    }
    company
}

/// The merge, end to end over HTTP: three answers and a reviewed roster become
/// a registered company, with no template involved.
#[tokio::test]
async fn an_apply_seeds_the_company_the_wizard_designed() {
    let home = home();
    let state = fresh_state(home.path());
    assert!(state.registry().is_empty(), "the premise: nothing yet");

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({ "company": designed_company(Some("ada@example.com")) }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let seeded = body["seeded_company"]
        .as_str()
        .expect("a company was seeded");
    assert!(
        state.registry().get(&CompanyId::new(seeded)).is_some(),
        "the seeded company is registered"
    );
    let manifest = seeded_manifest(home.path(), seeded).await;
    // Every designed teammate is on the roster. NOT an exact count: a company
    // also receives the global baseline agents (`src/globals/`), so asserting a
    // total here would pin this test to how many globals ship rather than to
    // anything this flow decides.
    let roles: Vec<&str> = manifest.agents.iter().map(|a| a.role.as_str()).collect();
    for designed in [
        "Meta Ads Specialist",
        "Order Dispatch Coordinator",
        "Accountant",
        "Operations Lead",
    ] {
        assert!(
            roles.contains(&designed),
            "{designed} is missing from {roles:?}"
        );
    }
    // The dead end this closes: without the address, email sign-in completes
    // and nobody can log in.
    assert_eq!(manifest.users.admins, vec!["ada@example.com".to_string()]);
    // Indistinguishable from a provisioned company.
    assert_eq!(
        manifest.policy.mode,
        crate::company::PROVISIONED_POLICY_MODE
    );
}

#[tokio::test]
async fn onboarding_persists_the_local_model_it_tested() {
    let home = home();
    let state = fresh_state(home.path());
    let mut company = designed_company(None);
    company["inference"] = serde_json::json!({
        "provider": "ollama",
        "baseUrl": "localhost:6969",
        "model": "qwen3:8b"
    });

    let (status, body) = post_setup(state.clone(), serde_json::json!({ "company": company })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seeded = body["seeded_company"].as_str().expect("seeded");
    let manifest = seeded_manifest(home.path(), seeded).await;
    assert_eq!(manifest.inference.provider.as_deref(), Some("ollama"));
    assert_eq!(
        manifest.inference.base_url.as_deref(),
        Some("http://localhost:6969/v1")
    );
    for tier in crate::company::INFERENCE_TIERS {
        assert_eq!(
            manifest.inference.models.get(*tier).map(String::as_str),
            Some("qwen3:8b")
        );
    }
}

#[cfg(feature = "openhuman")]
#[tokio::test]
async fn local_model_probe_normalizes_the_address_and_detects_its_model() {
    let app = axum::Router::new()
        .route(
            "/v1/models",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({ "data": [{ "id": "qwen3:8b" }] }))
            }),
        )
        .route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "choices": [{ "message": { "content": "pong" } }]
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let result = super::probe_inference(
        &super::InferenceTestRequest {
            provider: "ollama".to_string(),
            base_url: Some(address.to_string()),
            ..Default::default()
        },
        &MapEnv::default(),
    )
    .await;
    server.abort();

    assert!(result.ok, "{:?}", result.error);
    assert_eq!(result.base_url, format!("http://{address}/v1"));
    assert_eq!(result.model.as_deref(), Some("qwen3:8b"));
}

/// A designed company beats a template slug. An operator who answered three
/// questions and edited a roster has expressed a preference a preset cannot
/// override — and sending both must never produce two companies.
#[tokio::test]
async fn a_designed_company_wins_over_a_template() {
    let home = home();
    let state = fresh_state(home.path());

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({
            "template": "agentic_marketing_agency",
            "company": designed_company(None),
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state.registry().list().len(), 1, "exactly one company");
    let seeded = body["seeded_company"].as_str().expect("seeded");
    let roles: Vec<String> = seeded_manifest(home.path(), seeded)
        .await
        .agents
        .into_iter()
        .map(|a| a.role)
        .collect();
    // The designed roster landed; the template's did not. Named rather than
    // counted, because the global baseline agents are on here too.
    assert!(
        roles.iter().any(|r| r == "Order Dispatch Coordinator"),
        "the designed roster is missing: {roles:?}"
    );
    assert!(
        !roles.iter().any(|r| r == "Creative Director"),
        "the marketing template's roster leaked in: {roles:?}"
    );
}

/// The re-run guard applies to a designed company exactly as it does to a
/// template: setup must never hand an operator a second starter company.
#[tokio::test]
async fn a_second_apply_does_not_seed_another_company() {
    let home = home();
    let state = fresh_state(home.path());
    with_company(&state, home.path()).await;

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({ "company": designed_company(None) }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["seeded_company"].is_null(),
        "a host with a company must not be handed a second: {body}"
    );
    assert_eq!(state.registry().list().len(), 1);
}

/// The roster arrives over the wire after an operator edited it, so neither the
/// bounds nor the de-duplication can be assumed to have survived. Validation
/// runs again on the way in rather than trusting the client.
#[tokio::test]
async fn an_edited_roster_is_revalidated_on_the_way_in() {
    let home = home();
    let state = fresh_state(home.path());

    let mut company = designed_company(None);
    // Two rows that slug alike, and a blank one — all three are things a client
    // could send and `validate` would refuse.
    company["agents"] = serde_json::json!([
        { "name": "Ops", "role": "Ops Lead", "description": "a" },
        { "name": "Ops", "role": "ops  lead", "description": "b" },
        { "name": "", "role": "   ", "description": "c" },
        { "name": "Accounts", "role": "Accountant", "description": "d" }
    ]);

    let (status, body) = post_setup(state.clone(), serde_json::json!({ "company": company })).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let seeded = body["seeded_company"].as_str().expect("seeded");
    let manifest = seeded_manifest(home.path(), seeded).await;
    assert!(
        manifest.validate().is_empty(),
        "a registered company must be valid: {:?}",
        manifest.validate()
    );
    let ids: Vec<&str> = manifest.agents.iter().map(|a| a.id.as_str()).collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "duplicate ids survived: {ids:?}");
}

/// Phase 2 builds this company's workflows from the same answers, so it must
/// never have to ask again. The company-scoped route already stores them; the
/// wizard is the *default* path, and a company created through it arriving
/// without them would be the one that gets re-interrogated.
#[tokio::test]
async fn the_answers_are_stored_on_the_company_the_wizard_built() {
    let home = home();
    let state = fresh_state(home.path());

    let (status, body) = post_setup(
        state.clone(),
        serde_json::json!({ "company": designed_company(Some("ada@example.com")) }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let seeded = body["seeded_company"].as_str().expect("seeded");
    let store = crate::store::FsCompanyStore::new(home.path().to_path_buf());
    let record = store
        .load(&CompanyId::new(seeded))
        .await
        .expect("load")
        .expect("record");
    let answers = record.setup.expect("the answers were stored");
    assert_eq!(answers.industry, "E-commerce — I sell homeware online");
    assert_eq!(answers.automate, "Meta ads, order dispatch");
}
