//! Route tests for the company's TinyHumans credential (issue #586).

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::runtime::RuntimeBuilder;
use crate::server::router;
use crate::store::FsCompanyStore;
use crate::{AppConfig, AppState};

/// A value long and opaque enough that a leak would be unmistakable in a body.
const KEY: &str = "th_company_credential_SECRET_do_not_echo_me";

/// A company that grants Composio, so the status route has something to report
/// a credential tier *for*.
const GRANTED: &str = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
     [tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\"]\n";

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oc-company-key-")
        .tempdir()
        .expect("tempdir")
}

async fn state_with_manifest(
    home: &std::path::Path,
    company: &str,
    manifest_toml: &str,
) -> AppState {
    use crate::ports::CompanyStore;
    let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new(company);
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
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, std::sync::Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, company).await;
    state
}

async fn send_as(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
    cookie: String,
) -> (StatusCode, Value, String) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie);
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

async fn send(
    state: &AppState,
    company: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, String) {
    send_as(
        state,
        method,
        uri,
        body,
        crate::server::test_support::fixed_cookie(company),
    )
    .await
}

/// The core round trip: an admin sets the key, the read plane reports it as the
/// company's own identity, and the value never comes back out.
#[tokio::test]
async fn the_key_round_trips_write_only_and_reports_the_company_tier() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "acme", GRANTED).await;

    // Nothing set, and the test process carries no platform identity: the
    // honest degraded state, not a broken picker.
    let (status, dto, raw) = send(&state, "acme", "GET", "/api/v1/company/credential", None).await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(dto["configured"], false);
    assert_eq!(dto["source"], "none");
    let degraded = dto["notice"].as_str().unwrap_or_default();
    assert!(
        degraded.contains("no provider can be connected"),
        "the degraded state has to say what is unavailable: {dto}"
    );
    // …and must not overstate it. "Providers cannot be connected or used" read
    // as "nothing works", and a company whose LLM page holds a key of its own
    // goes on thinking perfectly well without this credential — that key
    // outranks it in the managed chain, and a provider of its own never
    // consults it. Overstating the breakage sends that operator to fix
    // something that is not broken.
    assert!(
        degraded.contains("can still think while this is unset"),
        "the degraded state must not claim the whole company has stopped: {dto}"
    );
    assert!(dto.get("key").is_none(), "status must never carry the key");

    // Set it.
    let (status, resp, raw) = send(
        &state,
        "acme",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(resp["status"]["configured"], true);
    assert_eq!(resp["status"]["source"], "company");
    assert!(!raw.contains(KEY), "PUT response leaked the key: {raw}");

    // The consequence is stated, because it is the thing an admin most needs to
    // understand before pasting.
    let notice = resp["status"]["notice"].as_str().unwrap_or_default();
    assert!(notice.contains("spend"), "{notice}");
    assert!(notice.contains("company"), "{notice}");
    // …and so is the distinction from the model-provider key. These two cards
    // sit next to each other and both read "configured"; the copy is the only
    // thing standing between an admin and pasting an OpenRouter key here.
    assert!(
        notice.contains("LLM page"),
        "the notice must say which key this is NOT: {notice}"
    );

    // The billing consequence, stated before the save rather than discovered on
    // the next invoice — and stated *conditionally*, because it is conditional
    // twice. This same notice comes back from the paste route and the grant
    // route, and they do different amounts: a paste writes `tinyhumans/key` and
    // stops, while `finish_link` also declares the `managed` provider. And the
    // managed chain has two rungs above this key (a key pasted for TinyHumans
    // on the LLM page, then the legacy `inference/key`), either of which goes
    // on answering after this one is set — #2266. A flat "this moves every
    // agent turn onto the account" would be false on both counts.
    assert!(
        notice.contains("resolve through this same key"),
        "the notice must say how the thinking reaches this account: {notice}"
    );
    assert!(
        notice.contains("outranks it"),
        "the notice must not promise a move a higher rung would prevent: {notice}"
    );
    assert!(
        notice.contains("leaves the choice of provider alone"),
        "the notice must not let a paste be read as choosing the provider: {notice}"
    );

    // And it must not overshoot the other way. "It is not the model-provider
    // key" was false: this credential very often IS what the agents think on,
    // and denying it sends an admin hunting for a second key they do not need.
    // The distinction that survives is narrower — not a *provider's* key.
    assert!(
        !notice.contains("not the model-provider key"),
        "the notice must not claim this key has nothing to do with models: {notice}"
    );

    // GET reflects it and still never carries the key.
    let (_, dto, raw) = send(&state, "acme", "GET", "/api/v1/company/credential", None).await;
    assert_eq!(dto["configured"], true);
    assert_eq!(dto["source"], "company");
    assert!(!raw.contains(KEY), "GET status leaked the key: {raw}");
}

/// Acceptance: a company with its key set can connect a provider without any
/// per-tenant provider app — the Composio plane must see the company's own
/// identity, with no Composio token pasted anywhere.
#[tokio::test]
async fn setting_the_key_credentials_composio_with_no_composio_token() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "brokered", GRANTED).await;

    // No Composio token, no platform identity in this process → nothing.
    let (_, dto, _) = send(&state, "brokered", "GET", "/api/v1/company/composio", None).await;
    assert_eq!(dto["credentialSource"], "none");

    send(
        &state,
        "brokered",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;

    // The company key alone credentials Composio. This is the issue in one
    // assertion: no `composio/token`, no provider app, still connectable.
    let (_, dto, raw) = send(&state, "brokered", "GET", "/api/v1/company/composio", None).await;
    assert_eq!(dto["credentialSource"], "company", "{raw}");
    assert!(
        !raw.contains(KEY),
        "the Composio status leaked the key: {raw}"
    );
}

/// Acceptance: clearing is real, and reverts to the honest degraded state
/// rather than stranding the console on a stale "connected" claim.
#[tokio::test]
async fn clearing_the_key_reverts_to_the_degraded_state() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "cleared", GRANTED).await;
    send(
        &state,
        "cleared",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;

    let (_, resp, _) = send(
        &state,
        "cleared",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": "" })),
    )
    .await;
    assert_eq!(resp["status"]["configured"], false);
    assert_eq!(resp["status"]["source"], "none");

    let (_, dto, _) = send(&state, "cleared", "GET", "/api/v1/company/composio", None).await;
    assert_eq!(dto["credentialSource"], "none");
}

/// A company's own Composio token still outranks the company key — the BYO
/// escape hatch is not taken away by this change.
#[tokio::test]
async fn a_pasted_composio_token_still_outranks_the_company_key() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "byo", GRANTED).await;
    send(
        &state,
        "byo",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    send(
        &state,
        "byo",
        "PUT",
        "/api/v1/company/composio/token",
        Some(json!({ "token": "byo-composio-token" })),
    )
    .await;

    let (_, dto, _) = send(&state, "byo", "GET", "/api/v1/company/composio", None).await;
    assert_eq!(
        dto["credentialSource"], "static",
        "a company that pasted its own Composio token keeps it: {dto}"
    );

    // The company key is still stored — the two are separate slots, and
    // clearing the Composio token falls back to the company's own identity
    // rather than to nothing.
    send(
        &state,
        "byo",
        "PUT",
        "/api/v1/company/composio/token",
        Some(json!({ "token": "" })),
    )
    .await;
    let (_, dto, _) = send(&state, "byo", "GET", "/api/v1/company/composio", None).await;
    assert_eq!(dto["credentialSource"], "company", "{dto}");
}

/// The two read planes answer **different questions**, and a company holding
/// both credentials is where that stops being pedantry.
///
/// `GET …/credential` reports whose identity the company *has*; `GET …/composio`
/// reports what a Composio call *presents*, which its BYO token overrides. Both
/// are correct simultaneously, and any refactor that "unifies" them would have
/// to break one of these two assertions.
#[tokio::test]
async fn the_credential_plane_and_the_composio_plane_may_honestly_disagree() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "disagree", GRANTED).await;

    send(
        &state,
        "disagree",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    send(
        &state,
        "disagree",
        "PUT",
        "/api/v1/company/composio/token",
        Some(json!({ "token": "byo-composio-token" })),
    )
    .await;

    let (_, credential, _) = send(
        &state,
        "disagree",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    let (_, composio, _) = send(&state, "disagree", "GET", "/api/v1/company/composio", None).await;

    assert_eq!(
        credential["source"], "company",
        "the company's own identity is set, whatever Composio presents: {credential}"
    );
    assert_eq!(
        composio["credentialSource"], "static",
        "a Composio call presents the BYO token, whatever identity the company holds: {composio}"
    );
    assert_eq!(credential["configured"], true);
}

/// The write is admin-only, for the same reason the Composio token write is:
/// this key repoints the company's entire brokered surface at whatever account
/// the caller controls, and it is the company's wallet.
#[tokio::test]
async fn a_member_cannot_set_the_companys_credential() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "acme", GRANTED).await;
    let member =
        crate::server::test_support::seed_session(&state, "acme", crate::ports::UserRole::Member)
            .await;

    let (status, body, raw) = send_as(
        &state,
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
        member,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{raw}");
    assert_eq!(body["code"], "forbidden", "{body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("admin"),
        "the refusal has to say why: {body}"
    );

    // The refusal is real, not merely a different status.
    let (_, dto, _) = send(&state, "acme", "GET", "/api/v1/company/credential", None).await;
    assert_eq!(
        dto["configured"], false,
        "a refused write must not have stored anything: {dto}"
    );
}

/// Setting the credential is journaled, so a change to what the company acts
/// through is never invisible — and a clear is told apart from a set.
#[tokio::test]
async fn setting_and_clearing_are_journaled_with_an_actor() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "audited", GRANTED).await;
    send(
        &state,
        "audited",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    send(
        &state,
        "audited",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": "" })),
    )
    .await;

    let id = CompanyId::new("audited");
    let runtime = state.registry().get(&id).expect("registered");
    let events = runtime
        .events()
        .read_from(&id, crate::ports::types::EventSeq::new(0), 200)
        .await
        .expect("events");
    let changes: Vec<(String, bool)> = events
        .iter()
        .filter_map(|stored| match &stored.event {
            CompanyEvent::ToolAccessChanged { change, by, .. } => {
                Some((change.clone(), by.is_some()))
            }
            _ => None,
        })
        .collect();
    assert!(
        changes.contains(&("company_key_set".to_string(), true)),
        "a set must be journaled with who did it: {changes:?}"
    );
    assert!(
        changes.contains(&("company_key_cleared".to_string(), true)),
        "a clear must be told apart from a set: {changes:?}"
    );
    // …and it must not borrow the Composio token's vocabulary. Both routes
    // append `ToolAccessChanged` to one log, so if this route spoke
    // `credential_set` an auditor could not tell a rotation of the company's
    // whole identity from a swap of one integration's token.
    assert!(
        !changes
            .iter()
            .any(|(change, _)| change == "credential_set" || change == "credential_cleared"),
        "no Composio write happened here, so no Composio audit word may appear: {changes:?}"
    );
}

/// An [`EventLog`](crate::ports::events::EventLog) decorator whose `append`
/// can be switched to fail after setup, so a test can build a real company
/// through a working log and then drive a route through the journal-refusal
/// arm. Reads always delegate to a real
/// [`FsEventLog`](crate::store::fs::FsEventLog), so `build()`'s own boot reads
/// are never touched by the failure.
struct FailingAppendLog {
    inner: crate::store::fs::FsEventLog,
    fail_appends: std::sync::atomic::AtomicBool,
}

impl FailingAppendLog {
    fn new(inner: crate::store::fs::FsEventLog) -> Self {
        Self {
            inner,
            fail_appends: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn fail_appends_from_now_on(&self) {
        self.fail_appends
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crate::ports::events::EventLog for FailingAppendLog {
    async fn append(
        &self,
        id: &CompanyId,
        event: crate::ports::types::CompanyEvent,
    ) -> crate::Result<crate::ports::types::EventSeq> {
        if self.fail_appends.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::error::OpenCompanyError::Config(
                "the event journal is unwritable".to_string(),
            ));
        }
        self.inner.append(id, event).await
    }

    async fn read_from(
        &self,
        id: &CompanyId,
        seq: crate::ports::types::EventSeq,
        limit: usize,
    ) -> crate::Result<Vec<crate::ports::types::StoredEvent>> {
        self.inner.read_from(id, seq, limit).await
    }

    fn subscribe(
        &self,
        id: &CompanyId,
    ) -> futures::stream::BoxStream<'static, crate::ports::events::EventStreamItem> {
        self.inner.subscribe(id)
    }
}

/// [`state_with_manifest`], with the company's journal swapped for
/// [`FailingAppendLog`] — armed only after the company finishes booting, so
/// `RuntimeBuilder::build`'s own event reads/writes see a working log and the
/// test controls exactly when the journal starts refusing.
async fn state_with_failing_journal(
    home: &std::path::Path,
    company: &str,
    manifest_toml: &str,
) -> (AppState, std::sync::Arc<FailingAppendLog>) {
    use crate::ports::CompanyStore;
    let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new(company);
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
    let journal = std::sync::Arc::new(FailingAppendLog::new(crate::store::fs::FsEventLog::new(
        home.to_path_buf(),
    )));
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .with_events(journal.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, std::sync::Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, company).await;
    (state, journal)
}

/// Issue #403's discipline, the unhappy half: `set_key`'s own doc comment
/// says the journal write is propagated rather than swallowed "so a change to
/// what the company's agents act through is never invisible" — but a
/// propagated error is not the same as an undone one. `store_key` has already
/// landed by the time `journal` runs, so a refused audit line leaves the
/// credential rotated with the caller holding nothing but a 500.
///
/// This is the documented trade, not a bug this test is trying to catch —
/// but until now nothing forced the journal to refuse and checked which side
/// of "propagates rather than swallows" actually happened.
#[tokio::test]
async fn a_journal_failure_after_the_key_is_stored_still_leaves_the_key_stored() {
    let home_dir = home();
    let (state, journal) = state_with_failing_journal(home_dir.path(), "acme", GRANTED).await;
    let app = router(state.clone());
    let cookie = crate::server::test_support::fixed_cookie("acme");

    journal.fail_appends_from_now_on();

    let (status, _, raw) = send_as(
        &state,
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
        cookie.clone(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a refused journal write must not be swallowed into a 200: {raw}"
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/company/credential")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let status_body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        status_body["configured"], true,
        "the key was stored before the journal ever ran, so it stays stored even though the \
         caller was told the request failed: {status_body}"
    );
}

// ---------------------------------------------------------------------------
// The one-click key grant (PKCE). See `server::hub_link`.
// ---------------------------------------------------------------------------

/// The key the mock hub mints when a grant is redeemed.
const GRANTED_KEY: &str = "tiny_live_granted_by_the_hub_do_not_echo_me";

/// A state with a hub that will mint `GRANTED_KEY` for the right verifier.
///
/// The verifier is not known until the host mints one, so the exchange is
/// seeded *after* `start` — which is also the only way to assert that the host
/// sends the challenge for the verifier it actually kept.
async fn state_with_hub(home: &std::path::Path, company: &str) -> AppState {
    state_with_manifest(home, company, GRANTED)
        .await
        .with_hub_identity(std::sync::Arc::new(
            crate::server::hub_identity::MockHubIdentityExchange::new(),
        ))
}

/// Pulls `state=` out of the authorize URL the console is told to navigate to.
fn state_param(authorize_url: &str) -> String {
    let (_, after) = authorize_url
        .split_once("state%3D")
        .expect("state in callback");
    after
        .split(['&', '%'])
        .next()
        .expect("state value")
        .to_string()
}

#[tokio::test]
async fn a_host_with_no_hub_offers_no_link_and_refuses_to_start_one() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "acme", GRANTED).await;

    // The status says so, which is what keeps the console from rendering a
    // button whose only possible outcome is a 404.
    let (status, dto, raw) = send(&state, "acme", "GET", "/api/v1/company/credential", None).await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(dto["hubLink"], false);

    let (status, _, raw) = send(
        &state,
        "acme",
        "POST",
        "/api/v1/company/credential/link/start",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{raw}");
}

#[tokio::test]
async fn starting_a_link_sends_the_console_to_the_hub_with_a_challenge_not_a_secret() {
    let home_dir = home();
    let state = state_with_hub(home_dir.path(), "acme").await;

    let (status, dto, raw) = send(&state, "acme", "GET", "/api/v1/company/credential", None).await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(dto["hubLink"], true);

    let (status, resp, raw) = send(
        &state,
        "acme",
        "POST",
        "/api/v1/company/credential/link/start",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");

    let url = resp["authorizeUrl"].as_str().expect("authorizeUrl");
    // Through the site's provider chooser, which forwards to the hub's own
    // `/auth/key` with the provider the person picked. Straight at `/auth/key`
    // would be the hub's `provider=google` default: an account picker naming
    // nobody, for somebody who pressed a button in their own console.
    assert!(
        url.contains("/connect?"),
        "must start the grant flow on the site's chooser: {url}"
    );
    assert!(
        url.contains("code_challenge_method=S256"),
        "plain must never be offered: {url}"
    );
    assert!(
        url.contains("key%3Dlink"),
        "the return leg needs this console's own marker, not the hub's key=auth: {url}"
    );

    // The verifier is the one thing that must not be in a URL the browser
    // follows. Only its SHA-256 goes out, and the response carries neither the
    // verifier nor anything else redeemable.
    let state_value = state_param(url);
    let link = state
        .hub_links()
        .take(&state_value, "acme")
        .expect("the start parked a pending link");
    assert!(
        !url.contains(&link.verifier),
        "the verifier must never leave this host: {url}"
    );
    assert!(url.contains(&crate::server::hub_link::challenge_for(&link.verifier)));
}

#[tokio::test]
async fn finishing_a_link_stores_the_minted_key_as_both_the_company_and_inference_credential() {
    let home_dir = home();
    let state = state_with_hub(home_dir.path(), "acme").await;

    let (_, resp, _) = send(
        &state,
        "acme",
        "POST",
        "/api/v1/company/credential/link/start",
        Some(json!({})),
    )
    .await;
    let url = resp["authorizeUrl"].as_str().unwrap().to_string();
    let state_value = state_param(&url);

    // Play the hub: it is the one participant that legitimately learns both the
    // verifier (from the challenge it was sent, at redemption) and the code it
    // handed the browser. Peeked rather than taken, so the link is still parked
    // for the route to spend.
    let verifier = state
        .hub_links()
        .peek_verifier(&state_value)
        .expect("the start parked a pending link");
    let state = state.with_hub_identity(std::sync::Arc::new(
        crate::server::hub_identity::MockHubIdentityExchange::new().with_grant(
            "grant-code",
            &verifier,
            GRANTED_KEY,
        ),
    ));

    let (status, resp, raw) = send(
        &state,
        "acme",
        "POST",
        "/api/v1/company/credential/link/finish",
        Some(json!({ "state": state_value, "code": "grant-code" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");

    // One grant, both credentials. An admin who had to run this twice — once
    // for Connections, once for Inference — would be back to two errands.
    assert_eq!(resp["status"]["configured"], true);
    assert_eq!(resp["status"]["source"], "company");
    assert!(
        !raw.contains(GRANTED_KEY),
        "the minted key must never be echoed to the console: {raw}"
    );

    let (_, inference, raw) = send(&state, "acme", "GET", "/api/v1/company/inference", None).await;
    assert_eq!(
        inference["keyConfigured"], true,
        "the same grant must arm inference: {raw}"
    );

    // Single-use: the same handle and code cannot be spent again.
    let (status, _, raw) = send(
        &state,
        "acme",
        "POST",
        "/api/v1/company/credential/link/finish",
        Some(json!({ "state": state_value, "code": "grant-code" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
}

#[tokio::test]
async fn a_replayed_or_unknown_state_is_refused() {
    let home_dir = home();
    let state = state_with_hub(home_dir.path(), "acme").await;

    let (status, _, raw) = send(
        &state,
        "acme",
        "POST",
        "/api/v1/company/credential/link/finish",
        Some(json!({ "state": "never-minted", "code": "whatever" })),
    )
    .await;
    // A `state` this host never minted, one that expired, and one already spent
    // are the same answer on purpose: the remedy is identical, and telling them
    // apart would say which handles had once been real.
    assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
}

#[tokio::test]
async fn a_member_cannot_start_or_finish_a_link() {
    let home_dir = home();
    let state = state_with_hub(home_dir.path(), "acme").await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let member = crate::server::test_support::member_cookie("acme");

    // Same authority `PUT /credential` needs. That the key is minted rather
    // than pasted changes who types it, not what it does.
    let (status, _, raw) = send_as(
        &state,
        "POST",
        "/api/v1/company/credential/link/start",
        Some(json!({})),
        member.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{raw}");

    let (status, _, raw) = send_as(
        &state,
        "POST",
        "/api/v1/company/credential/link/finish",
        Some(json!({ "state": "x", "code": "y" })),
        member,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{raw}");
}

// ---------------------------------------------------------------------------
// `GET .../credential/billing`
// ---------------------------------------------------------------------------

/// A company with no key of its own reports `configured: false` and no
/// figures — never a fallback account's balance.
///
/// This is the negative control for a real regression: `get_billing` once
/// resolved through [`crate::company::company_key::resolve`], which falls
/// through to this instance's platform identity when the company has set
/// nothing. That would report `configured: true` and query billing for the
/// shared host identity — exposing that account's balance and plan to any
/// company member. The route must load the company's own credential only.
#[tokio::test]
async fn a_company_with_no_key_reports_unconfigured_billing_not_a_fallback_balance() {
    let home_dir = home();
    let state = state_with_hub(home_dir.path(), "acme").await;

    let (status, dto, raw) = send(
        &state,
        "acme",
        "GET",
        "/api/v1/company/credential/billing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(dto["configured"], false, "{raw}");
    assert!(dto["summary"].is_null(), "{raw}");
}

/// Once the company sets its own key, billing reads that key's standing from
/// the hub — the intended path this route exists for.
#[tokio::test]
async fn a_companys_own_key_reads_its_own_billing_summary() {
    use crate::server::hub_identity::{BillingSummary, MockHubIdentityExchange};

    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "acme", GRANTED)
        .await
        .with_hub_identity(std::sync::Arc::new(
            MockHubIdentityExchange::new().with_billing(
                KEY,
                BillingSummary {
                    balance_usd: 12.5,
                    plan: "pro".to_string(),
                    active_subscription: true,
                    ..Default::default()
                },
            ),
        ));

    let (status, _, raw) = send_as(
        &state,
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
        crate::server::test_support::fixed_cookie("acme"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");

    let (status, dto, raw) = send(
        &state,
        "acme",
        "GET",
        "/api/v1/company/credential/billing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(dto["configured"], true, "{raw}");
    assert_eq!(dto["summary"]["balanceUsd"], 12.5, "{raw}");
    assert_eq!(dto["summary"]["plan"], "pro", "{raw}");
}

/// A member — not just an admin — can read the balance: nobody should have to
/// ask an admin why their agents stopped working this afternoon.
#[tokio::test]
async fn a_member_can_read_billing_without_admin_rights() {
    let home_dir = home();
    let state = state_with_hub(home_dir.path(), "acme").await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let member = crate::server::test_support::member_cookie("acme");

    let (status, dto, raw) = send_as(
        &state,
        "GET",
        "/api/v1/company/credential/billing",
        None,
        member,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(dto["configured"], false, "{raw}");
}

// ---------------------------------------------------------------------------
// Where the grant comes back to
// ---------------------------------------------------------------------------

mod callback_origin {
    use super::super::{callback_origin, is_loopback_origin};
    use crate::{AppConfig, AppState};
    use axum::http::{HeaderMap, HeaderValue, header::ORIGIN};

    fn state_with(public_url: Option<&str>) -> AppState {
        AppState::new(AppConfig {
            bind: "127.0.0.1:8080".to_string(),
            public_url: public_url.map(str::to_string),
            ..AppConfig::default()
        })
    }

    fn headers_from(origin: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(origin).expect("header"));
        headers
    }

    #[test]
    fn a_stated_public_url_wins_over_the_browser() {
        // A deployment that names its origin has said where its console is, and
        // that is not something a request gets to move.
        let state = state_with(Some("https://acme.opencompany.example/"));
        let origin = callback_origin(&state, &headers_from("http://localhost:5173"));
        assert_eq!(origin, "https://acme.opencompany.example");
    }

    #[test]
    fn the_dev_console_gets_its_own_port_back() {
        // The failure this exists to stop: with nothing configured, the callback
        // was `http://127.0.0.1:8080`, where a dev host serves no page — so the
        // approval landed on a 404 holding a spent code.
        let state = state_with(None);
        let origin = callback_origin(&state, &headers_from("http://localhost:5173"));
        assert_eq!(origin, "http://localhost:5173");
    }

    #[test]
    fn a_remote_origin_is_ignored_for_the_bind_address() {
        // A header is attacker-controllable. A stolen code redeems nothing
        // without this host's verifier, but a callback is not somewhere to take
        // an arbitrary address on a request's say-so.
        let state = state_with(None);
        let origin = callback_origin(&state, &headers_from("https://evil.example"));
        assert_eq!(origin, "http://127.0.0.1:8080");
    }

    #[test]
    fn no_origin_header_falls_back_to_the_bind_address() {
        let state = state_with(None);
        assert_eq!(
            callback_origin(&state, &HeaderMap::new()),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn an_empty_public_url_is_not_an_origin() {
        // A launcher that exported the variable with nothing in it has said
        // nothing, and must not produce a callback of `/?company=…`.
        let state = state_with(Some("   "));
        let origin = callback_origin(&state, &headers_from("http://127.0.0.1:5173"));
        assert_eq!(origin, "http://127.0.0.1:5173");
    }

    #[test]
    fn loopback_is_the_hub_gates_own_shape() {
        // Accepting an origin the hub would refuse would only move the failure
        // one leg later, into a 400 nobody can act on.
        assert!(is_loopback_origin("http://localhost:5173"));
        assert!(is_loopback_origin("http://127.0.0.1:8080"));
        assert!(is_loopback_origin("http://[::1]:5173"));
        assert!(!is_loopback_origin("https://localhost:5173"));
        assert!(!is_loopback_origin("http://localhost.evil.example"));
        assert!(!is_loopback_origin("http://127.0.0.1:5173/steal"));
        assert!(!is_loopback_origin("not a url"));
    }
}
