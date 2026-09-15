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
    // Keys rework (#2306), slice 4a: `PUT …/credential` fans the account key
    // out to the LLM TinyHumans slot and probes it before any row or default
    // write (Q6). Forcing this here — rather than per test — is what keeps
    // every test in this file from dialing `api.tinyhumans.ai`; a test that
    // wants a different answer (a rejection, an endpoint failure) overrides
    // it again after this call.
    super::prober_override::set(company, Ok(vec!["acme/test-model".to_string()]));
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

    // The fan-out consequence, stated before the save rather than discovered on
    // the next invoice — and stated *conditionally*, because it is conditional
    // twice. This same notice comes back from the paste route and the grant
    // route, and they do different amounts: a paste fans the key out to
    // Composio and the LLM TinyHumans slot and stops there, while `finish_link`
    // runs the same fan-out and declares no provider of its own (Q10). And the
    // managed chain has two rungs above the copy this fan-out makes (a key
    // pasted for TinyHumans on the LLM page directly, then the legacy
    // `inference/key`), either of which goes on answering after this one is
    // set — #2266. A flat "this moves every agent turn onto the account" would
    // be false on both counts.
    assert!(
        notice.contains("no key of their own"),
        "the notice must say the copy never overwrites a key set on its own page: {notice}"
    );
    assert!(
        notice.contains("only when no default is set"),
        "the notice must not promise a default move a higher rung would prevent: {notice}"
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
/// per-tenant provider app — the fan-out (keys rework #2306, slice 4a) copies
/// the account key straight into `composio/tinyhumans/key`, so the Composio
/// plane reports the stored-token tier rather than falling through to the
/// shared brokered-credential seam.
#[tokio::test]
async fn setting_the_key_fills_the_composio_tinyhumans_key() {
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
    // assertion: no Composio token pasted anywhere, no provider app, still
    // connectable — via the fan-out's own copy, which reads back as the
    // stored-token tier (`static`), not the fallback tier (`company`).
    let (_, dto, raw) = send(&state, "brokered", "GET", "/api/v1/company/composio", None).await;
    assert_eq!(dto["credentialSource"], "static", "{raw}");
    assert!(
        !raw.contains(KEY),
        "the Composio status leaked the key: {raw}"
    );
}

/// Acceptance: clearing is real, and reverts to the honest degraded state
/// rather than stranding the console on a stale "connected" claim.
///
/// The first `PUT` fans the key out to `composio/tinyhumans/key`, so the
/// clear below is exactly what the in-use guard exists for (its Composio copy
/// still equals the account key) — hence `confirmInUse: true`. Phase-4a's own
/// plan predates that guard and called this test "unchanged"; applying
/// `docs/key-reworks/in-use-guards.md` on top is this dispatch's added scope,
/// and this is the one place it changes what an already-passing test has to
/// send.
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
        Some(json!({ "key": "", "confirmInUse": true })),
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
    // rather than to nothing. Guarded while the company is on the managed
    // route (in-use-guards.md §2); this test is about the fallback tier,
    // not the guard, so it confirms.
    send(
        &state,
        "byo",
        "PUT",
        "/api/v1/company/composio/token",
        Some(json!({ "token": "", "confirmInUse": true })),
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
        // The first PUT already fanned the key out to Composio, so this clear
        // is guarded (its Composio copy still equals the account key).
        Some(json!({ "key": "", "confirmInUse": true })),
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
    super::prober_override::set(company, Ok(vec!["acme/test-model".to_string()]));
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
// The account-key fan-out (keys rework #2306, slice 4a) and its in-use guard.
// ---------------------------------------------------------------------------

/// One slot's `outcome` field, looked up by slot name rather than by
/// position — `company_key::fan_out` promises the order, but a test should
/// not have to remember it to read one entry.
fn slot_outcome<'a>(resp: &'a Value, slot: &str) -> &'a Value {
    let entry = resp["slots"]
        .as_array()
        .expect("slots array")
        .iter()
        .find(|s| s["slot"] == slot)
        .unwrap_or_else(|| panic!("no {slot} slot in {resp}"));
    &entry["outcome"]
}

/// M1 by route: a bare key save on an empty company fills Composio and the
/// LLM copy, cannot create a row or a default for want of a model, and says
/// so — the exact JSON shape `phase-4a-account-key-fanout.md` §3.3 quotes.
#[tokio::test]
async fn put_credential_answers_slots_and_needs_model() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "fanm1", GRANTED).await;

    let (status, resp, raw) = send(
        &state,
        "fanm1",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");

    assert_eq!(*slot_outcome(&resp, "composio"), json!("filled"));
    assert_eq!(*slot_outcome(&resp, "inference"), json!("filled"));
    assert_eq!(*slot_outcome(&resp, "provider"), json!("skipped"));
    assert_eq!(*slot_outcome(&resp, "default"), json!("skipped"));
    assert_eq!(*slot_outcome(&resp, "health"), json!("ok"));
    assert_eq!(resp["needsModel"], true);
    assert_eq!(resp["setsDefault"], true);
    assert!(
        resp["models"]
            .as_array()
            .unwrap()
            .contains(&json!("acme/test-model")),
        "{resp}"
    );

    let (_, inference, raw) = send(&state, "fanm1", "GET", "/api/v1/company/inference", None).await;
    assert!(
        inference["providers"].as_array().unwrap().is_empty(),
        "no row without a model: {raw}"
    );
}

/// M2 by route: sending a model with the same save adds the row and, since
/// none was set, the default.
#[tokio::test]
async fn put_credential_with_a_model_adds_the_row_and_default() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "fanm2", GRANTED).await;

    let (status, resp, raw) = send(
        &state,
        "fanm2",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY, "model": "acme/test-model" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(*slot_outcome(&resp, "provider"), json!("filled"));
    assert_eq!(*slot_outcome(&resp, "default"), json!("filled"));
    assert_eq!(resp["needsModel"], false);

    let (_, inference, raw) = send(&state, "fanm2", "GET", "/api/v1/company/inference", None).await;
    let providers = inference["providers"].as_array().unwrap();
    let row = providers
        .iter()
        .find(|p| p["slug"] == "tinyhumans")
        .unwrap_or_else(|| panic!("no tinyhumans row: {raw}"));
    assert_eq!(row["origin"], "indexed");
    assert_eq!(inference["defaultChoice"]["provider"], "tinyhumans");
    assert_eq!(inference["defaultChoice"]["model"], "acme/test-model");
}

/// A brain that reports the harness cognition path, so the successor a rebuild
/// installs reads as "thinking" rather than "echo" (the observable
/// `restart_pending` keys on). Mirrors the stub in `ops::inference`'s tests.
#[cfg(feature = "openhuman")]
struct RebuiltBrain;

#[cfg(feature = "openhuman")]
#[async_trait::async_trait]
impl crate::ports::brain::Brain for RebuiltBrain {
    async fn run_cycle(
        &self,
        _req: crate::ports::types::CycleRequest,
        _host: &dyn crate::ports::brain::CycleHost,
    ) -> crate::Result<crate::ports::types::CycleResult> {
        Ok(crate::ports::types::CycleResult {
            channel_responses: Vec::new(),
            new_traces: Vec::new(),
            ledger_deltas: Vec::new(),
            token_usage: crate::ports::types::TokenUsage::default(),
        })
    }

    fn cognition(&self) -> crate::ports::Cognition {
        crate::ports::Cognition {
            path: crate::ports::brain::HARNESS_PATH,
            provider: "stub",
            model: None,
            metering: crate::ports::UsageMetering::PerTurn,
        }
    }
}

#[cfg(feature = "openhuman")]
struct StubRebuilder {
    home: std::path::PathBuf,
}

#[cfg(feature = "openhuman")]
#[async_trait::async_trait]
impl crate::runtime::RuntimeRebuilder for StubRebuilder {
    async fn rebuild(
        &self,
        _state: &AppState,
        request: crate::runtime::RebuildRequest,
    ) -> crate::Result<crate::company::runtime::CompanyRuntime> {
        RuntimeBuilder::new(self.home.clone(), request.manifest)
            .with_id(request.id)
            .with_harness(std::sync::Arc::new(crate::harness::HarnessPool::new()))
            .with_brain(std::sync::Arc::new(RebuiltBrain))
            .with_handover(request.handover)
            .build()
            .await
    }
}

/// The account-key save that creates the `tinyhumans` row and default for a
/// company that booted with no inference source rebuilds that company in
/// place — the same thing `PUT …/inference` does (issue #290) — instead of
/// answering `restartRequired` and leaving every turn on the echo brain
/// until someone finds the toast's "Restart now" action. Found by the
/// umbrella e2e (`workflow-opencompany/scripts/e2e-tinyhumans-key.sh`):
/// key saved, model chosen, default set, and the next chat still said
/// `You said: …`.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn put_credential_that_configures_inference_rebuilds_the_runtime_in_place() {
    use crate::ports::CompanyStore;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let manifest: CompanyManifest = toml::from_str(GRANTED).unwrap();
    let id = CompanyId::new("fanrb");
    FsCompanyStore::new(home.clone())
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
    // Booted with a harness pool but no inference source: the echo brain.
    let runtime = RuntimeBuilder::new(home.clone(), manifest)
        .with_id(id.clone())
        .with_harness(std::sync::Arc::new(crate::harness::HarnessPool::new()))
        .build()
        .await
        .unwrap();
    assert_eq!(runtime.cognition().path, "echo");
    let outgoing = std::sync::Arc::new(runtime);
    let state = AppState::new(AppConfig::default())
        .with_rebuilder(std::sync::Arc::new(StubRebuilder { home: home.clone() }));
    state.registry().insert(id.clone(), outgoing.clone());
    crate::server::test_support::seed_fixed_admin(&state, "fanrb").await;
    super::prober_override::set("fanrb", Ok(vec!["acme/test-model".to_string()]));

    // Step one: key only. Nothing about inference is configured yet (no row,
    // no default), so nothing is rebuilt and the company keeps its runtime.
    let (status, resp, raw) = send(
        &state,
        "fanrb",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(resp["needsModel"], true, "{raw}");
    let registered = state.registry().get(&id).expect("still registered");
    assert!(std::sync::Arc::ptr_eq(&registered, &outgoing));

    // Step two: the model. The row and default land, and the company is
    // rebuilt onto the harness rather than told to restart.
    let (status, resp, raw) = send(
        &state,
        "fanrb",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY, "model": "acme/test-model" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(*slot_outcome(&resp, "provider"), json!("filled"));
    assert_eq!(*slot_outcome(&resp, "default"), json!("filled"));
    assert!(
        resp.get("restartRequired").is_none(),
        "the rebuilt company must not ask for a restart: {raw}"
    );
    assert!(!raw.contains(KEY), "PUT response leaked the key: {raw}");

    let registered = state.registry().get(&id).expect("still registered");
    assert!(!std::sync::Arc::ptr_eq(&registered, &outgoing));
    assert!(outgoing.is_quiesced());
    assert!(!registered.is_quiesced());

    let (_, dto, raw) = send(&state, "fanrb", "GET", "/api/v1/company/inference", None).await;
    assert_eq!(dto["cognition"], "harness", "{raw}");
    assert_eq!(dto["restartRequired"], false, "{raw}");
}

/// Q6: a probe classified `auth` rolls the LLM copy back and never touches
/// the account key or the Composio copy.
#[tokio::test]
async fn put_credential_auth_probe_rolls_back_the_llm_copy() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "fanauth", GRANTED).await;
    super::prober_override::set(
        "fanauth",
        Err(crate::company::inference::probe::ProbeClass::Auth),
    );

    let (status, resp, raw) = send(
        &state,
        "fanauth",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(*slot_outcome(&resp, "inference"), json!("rolledBack"));
    assert_eq!(
        *slot_outcome(&resp, "composio"),
        json!("filled"),
        "the Composio copy is kept even though the LLM copy is not"
    );
    assert_eq!(resp["needsModel"], false);
    assert_eq!(
        resp["status"]["configured"], true,
        "the account key itself is kept"
    );

    let (_, composio, _) = send(&state, "fanauth", "GET", "/api/v1/company/composio", None).await;
    assert_eq!(composio["credentialSource"], "static");
}

/// The key never appears anywhere on the wire — not in the mutation response,
/// not in either status read it feeds.
#[tokio::test]
async fn put_credential_never_echoes_the_key() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "fanleak", GRANTED).await;

    let (_, _, raw) = send(
        &state,
        "fanleak",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY, "model": "acme/test-model" })),
    )
    .await;
    assert!(!raw.contains(KEY), "{raw}");

    let (_, _, raw) = send(&state, "fanleak", "GET", "/api/v1/company/inference", None).await;
    assert!(!raw.contains(KEY), "{raw}");
    let (_, _, raw) = send(&state, "fanleak", "GET", "/api/v1/company/composio", None).await;
    assert!(!raw.contains(KEY), "{raw}");
}

/// §3.5: one journal line per slot that actually changed, never for the
/// health slot.
#[tokio::test]
async fn put_credential_journals_one_line_per_changed_slot() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "fanjournal", GRANTED).await;
    send(
        &state,
        "fanjournal",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY, "model": "acme/test-model" })),
    )
    .await;

    let id = CompanyId::new("fanjournal");
    let runtime = state.registry().get(&id).expect("registered");
    let events = runtime
        .events()
        .read_from(&id, crate::ports::types::EventSeq::new(0), 200)
        .await
        .expect("events");
    let changes: Vec<String> = events
        .iter()
        .filter_map(|stored| match &stored.event {
            CompanyEvent::ToolAccessChanged { change, .. } => Some(change.clone()),
            _ => None,
        })
        .collect();
    for expected in [
        "company_key_set",
        "company_key_composio_filled",
        "company_key_inference_filled",
        "company_key_provider_filled",
        "company_key_default_filled",
    ] {
        assert!(
            changes.contains(&expected.to_string()),
            "missing {expected}: {changes:?}"
        );
    }
    assert!(
        !changes.iter().any(|c| c.contains("health")),
        "the health slot never journals: {changes:?}"
    );
}

/// The grant runs the same fan-out and, per Q10, declares no provider of its
/// own: no entry zero appears, and `managed.source` reads as a plain
/// provider-key credential rather than a declared runtime config.
#[tokio::test]
async fn finish_link_runs_the_fan_out_and_writes_no_inference_config() {
    let home_dir = home();
    let state = state_with_hub(home_dir.path(), "fanlink").await;

    let (_, resp, _) = send(
        &state,
        "fanlink",
        "POST",
        "/api/v1/company/credential/link/start",
        Some(json!({})),
    )
    .await;
    let url = resp["authorizeUrl"].as_str().unwrap().to_string();
    let state_value = state_param(&url);
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

    let (status, _, raw) = send(
        &state,
        "fanlink",
        "POST",
        "/api/v1/company/credential/link/finish",
        Some(json!({ "state": state_value, "code": "grant-code" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");

    let (_, inference, raw) =
        send(&state, "fanlink", "GET", "/api/v1/company/inference", None).await;
    assert!(
        !inference["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["origin"] == "entryZero"),
        "a grant declares no provider of its own: {raw}"
    );
    assert_eq!(inference["managed"]["source"], "provider_key", "{raw}");
}

/// The in-use guard (new scope beyond phase-4a, from
/// `docs/key-reworks/in-use-guards.md`): clearing the account key while it
/// backs a `tinyhumans` row and resolves both the LLM and Composio slots is
/// refused without confirmation, naming both surfaces.
#[tokio::test]
async fn clearing_the_account_key_when_it_backs_the_llm_row_is_refused_without_confirmation() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "fanguard", GRANTED).await;
    send(
        &state,
        "fanguard",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY, "model": "acme/test-model" })),
    )
    .await;

    let (status, body, raw) = send(
        &state,
        "fanguard",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{raw}");
    assert_eq!(body["code"], "in_use", "{body}");
    let surfaces: Vec<&str> = body["usedBy"]["surfaces"]
        .as_array()
        .expect("surfaces")
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(surfaces.contains(&"llm"), "{body}");
    assert!(surfaces.contains(&"composio"), "{body}");

    // Refused, so nothing changed.
    let (_, dto, _) = send(
        &state,
        "fanguard",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    assert_eq!(
        dto["configured"], true,
        "a refused clear must not have stored anything"
    );
}

/// The same clear, confirmed: proceeds exactly as §6's C1 describes (the row
/// and the default survive; only the key copies clear) and echoes the
/// `usedBy` it would have refused with.
#[tokio::test]
async fn a_confirmed_clear_of_the_account_key_proceeds_and_echoes_used_by() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "fanconfirm", GRANTED).await;
    send(
        &state,
        "fanconfirm",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY, "model": "acme/test-model" })),
    )
    .await;

    let (status, resp, raw) = send(
        &state,
        "fanconfirm",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": "", "confirmInUse": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    let surfaces: Vec<&str> = resp["usedBy"]["surfaces"]
        .as_array()
        .expect("usedBy echoed on a confirmed clear")
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(surfaces.contains(&"llm"), "{resp}");
    assert!(surfaces.contains(&"composio"), "{resp}");

    assert_eq!(*slot_outcome(&resp, "composio"), json!("cleared"));
    assert_eq!(*slot_outcome(&resp, "inference"), json!("cleared"));
    assert_eq!(*slot_outcome(&resp, "provider"), json!("skipped"));
    assert_eq!(*slot_outcome(&resp, "default"), json!("skipped"));

    let (_, inference, raw) = send(
        &state,
        "fanconfirm",
        "GET",
        "/api/v1/company/inference",
        None,
    )
    .await;
    assert!(
        inference["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["slug"] == "tinyhumans"),
        "the row stays: {raw}"
    );
    assert_eq!(
        inference["defaultChoice"]["provider"], "tinyhumans",
        "the default stays: {raw}"
    );
}

/// Custom keys everywhere (matrix shape M6/C2): the account key's clear
/// touches nothing either derived slot still resolves through, so it needs no
/// confirmation and echoes no `usedBy` at all.
#[tokio::test]
async fn clearing_when_nothing_depends_on_it_needs_no_confirmation() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "fanclean", GRANTED).await;
    send(
        &state,
        "fanclean",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    // A custom key pasted directly on both the Composio and LLM pages, which
    // the fan-out never overwrites (Q7) and which the guard must not treat as
    // still depending on the account key.
    send(
        &state,
        "fanclean",
        "PUT",
        "/api/v1/company/composio/token",
        Some(json!({ "token": "custom-composio-token" })),
    )
    .await;
    send(
        &state,
        "fanclean",
        "PUT",
        "/api/v1/company/inference/managed/key",
        Some(json!({ "key": "custom-llm-key" })),
    )
    .await;

    let (status, resp, raw) = send(
        &state,
        "fanclean",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert!(
        resp.get("usedBy").is_none(),
        "nothing depends on the account key any more: {resp}"
    );
    assert_eq!(*slot_outcome(&resp, "composio"), json!("kept"));
    assert_eq!(*slot_outcome(&resp, "inference"), json!("kept"));
}

/// KR-L3-01: `GET …/credential` carries the same `usedBy` a clear would be
/// refused with, computed the moment the page loads rather than only after a
/// stale-UI 409 — the Remove-key dialog's first-open text.
#[tokio::test]
async fn status_reports_used_by_when_a_clear_would_strand_dependents() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "statususedby", GRANTED).await;
    send(
        &state,
        "statususedby",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY, "model": "acme/test-model" })),
    )
    .await;

    let (_, dto, raw) = send(
        &state,
        "statususedby",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    let surfaces: Vec<&str> = dto["usedBy"]["surfaces"]
        .as_array()
        .expect("usedBy on the status DTO")
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(surfaces.contains(&"llm"), "{raw}");
    assert!(surfaces.contains(&"composio"), "{raw}");
}

/// The other half: nothing set, or nothing left depending on the account key
/// (matrix M6/C2's shape) — `usedBy` is absent, never an empty object, so a
/// plain `"usedBy" in dto` check on the console reads false.
#[tokio::test]
async fn status_reports_no_used_by_when_nothing_depends_on_it() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "statusnousedby", GRANTED).await;

    let (_, empty_dto, raw) = send(
        &state,
        "statusnousedby",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    assert!(empty_dto.get("usedBy").is_none(), "no key set yet: {raw}");

    send(
        &state,
        "statusnousedby",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    send(
        &state,
        "statusnousedby",
        "PUT",
        "/api/v1/company/composio/token",
        Some(json!({ "token": "custom-composio-token" })),
    )
    .await;
    send(
        &state,
        "statusnousedby",
        "PUT",
        "/api/v1/company/inference/managed/key",
        Some(json!({ "key": "custom-llm-key" })),
    )
    .await;

    let (_, dto, raw) = send(
        &state,
        "statusnousedby",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    assert!(
        dto.get("usedBy").is_none(),
        "both slots hold their own key, not the account key's copy: {raw}"
    );
}

// ---------------------------------------------------------------------------
// P1-1 (keys rework #2306, KR-L3-01 review): the composio/mode gate and the
// legacy managed entry-zero decision, pinned by dedicated tests.
// ---------------------------------------------------------------------------

/// P1-1 (a): BYOK mode with an account key equal to the Composio copy — no
/// row exists either, so nothing at all could be stranded, and clearing needs
/// no confirmation. This isolates the composio/mode gate specifically:
/// `composio/tinyhumans/key` still equals the account key (`decide_copy`
/// would `Clear` it), but a company on `byok` has nothing live resolving
/// through that slot (`in-use-guards.md` §2's `"composio"` row).
#[tokio::test]
async fn p1_1_byok_mode_with_a_matching_composio_copy_needs_no_confirmation() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "p11byok", GRANTED).await;
    let id = CompanyId::new("p11byok");
    // No model: no `tinyhumans` row is created, so nothing can make "llm"
    // appear either.
    send(
        &state,
        "p11byok",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    // Switch Composio to BYOK directly at the store, bypassing
    // `PUT …/composio/api-key` and the real network probe it would run in
    // this feature build — this test is about the mode gate, not the probe.
    // `composio/tinyhumans/key` is untouched by this — it still holds the
    // fan-out's own copy of the account key.
    let runtime = state.registry().get(&id).expect("registered");
    runtime
        .secrets()
        .set(
            &id,
            crate::company::composio::MODE_KEY,
            crate::ports::types::SecretValue("byok".to_string()),
        )
        .await
        .unwrap();

    let (status, resp, raw) = send(
        &state,
        "p11byok",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert!(
        resp.get("usedBy").is_none(),
        "byok has nothing live resolving through composio/tinyhumans/key: {resp}"
    );
}

/// P1-1 (b): the mirror image of (a) — managed mode (the default), same
/// matching Composio copy, same absence of a row. Refused with `409`,
/// naming `"composio"` and nothing else.
#[tokio::test]
async fn p1_1_managed_mode_with_a_matching_composio_copy_is_refused() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "p11managed", GRANTED).await;
    send(
        &state,
        "p11managed",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;

    let (status, body, raw) = send(
        &state,
        "p11managed",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{raw}");
    assert_eq!(body["code"], "in_use", "{body}");
    let surfaces: Vec<&str> = body["usedBy"]["surfaces"]
        .as_array()
        .expect("surfaces")
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(surfaces, vec!["composio"], "{body}");
}

/// P1-1 (c): a company still on the legacy managed entry zero — no indexed
/// `tinyhumans` row at all, but `inference/config` already names
/// `tinyhumans` — counts as `"llm"` in use too (the KR-L3-01 review decision
/// recorded in `docs/key-reworks/in-use-guards.md` §2's `"llm"` row).
/// `row_exists` alone only ever sees indexed rows; this pins that the
/// entry-zero path is deliberately counted as well, with an explicit
/// assertion rather than leaving the decision undertested.
#[tokio::test]
async fn p1_1_legacy_managed_entry_zero_counts_as_llm_in_use() {
    use crate::company::inference::RuntimeInference;

    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "p11entryzero", GRANTED).await;
    let id = CompanyId::new("p11entryzero");
    let runtime = state.registry().get(&id).expect("registered");

    // The account key, and the SAME value at the legacy flat slot
    // `inference/key` — the address `load_managed_key`'s own entry-zero
    // fallback reads, so the guard's `decide_copy` sees a copy that still
    // equals the account key.
    runtime
        .secrets()
        .set(
            &id,
            crate::company::company_key::KEY_KEY,
            crate::ports::types::SecretValue(KEY.to_string()),
        )
        .await
        .unwrap();
    runtime
        .secrets()
        .set(
            &id,
            crate::company::inference::KEY_KEY,
            crate::ports::types::SecretValue(KEY.to_string()),
        )
        .await
        .unwrap();
    crate::company::inference::save_runtime_config(
        &id,
        runtime.secrets().as_ref(),
        &RuntimeInference {
            provider: crate::company::inference::MANAGED_SLUG.to_string(),
            base_url: None,
            models: Default::default(),
        },
    )
    .await
    .unwrap();

    let (status, body, raw) = send(
        &state,
        "p11entryzero",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{raw}");
    assert_eq!(body["code"], "in_use", "{body}");
    let surfaces: Vec<&str> = body["usedBy"]["surfaces"]
        .as_array()
        .expect("surfaces")
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(
        surfaces.contains(&"llm"),
        "entry-zero legacy managed config counts as llm in use: {body}"
    );
}

// ---------------------------------------------------------------------------
// `slot_facts` — the account-key dialog's own-key booleans (keys rework
// #2306, slice 4b). See `docs/key-reworks/phase-4b-account-dialog.md` §3.1/§6.
// ---------------------------------------------------------------------------

/// A fresh company reports no own keys anywhere and no default — the
/// dialog's starting state, where saving would fill both derived slots.
#[tokio::test]
async fn status_reports_no_own_keys_on_an_empty_company() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "factsempty", GRANTED).await;

    let (_, dto, raw) = send(
        &state,
        "factsempty",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    assert_eq!(dto["inferenceHasOwnKey"], false, "{raw}");
    assert_eq!(dto["composioHasOwnKey"], false, "{raw}");
    assert_eq!(dto["defaultSet"], false, "{raw}");
}

/// A copy that merely equals the account key is not "its own" — Q7's whole
/// point is that such a copy is filled again on the next rotation.
#[tokio::test]
async fn a_copy_equal_to_the_account_key_is_not_an_own_key() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "factscopy", GRANTED).await;
    send(
        &state,
        "factscopy",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;

    let (_, dto, raw) = send(
        &state,
        "factscopy",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    assert_eq!(
        dto["inferenceHasOwnKey"], false,
        "the fan-out's own copy is not an own key: {raw}"
    );
    assert_eq!(
        dto["composioHasOwnKey"], false,
        "the fan-out's own copy is not an own key: {raw}"
    );
}

/// A key pasted directly on the LLM page is that slot's own key, and the
/// dialog must be able to tell.
#[tokio::test]
async fn a_key_set_on_the_llm_page_is_an_own_key() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "factsllm", GRANTED).await;
    send(
        &state,
        "factsllm",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    send(
        &state,
        "factsllm",
        "PUT",
        "/api/v1/company/inference/managed/key",
        Some(json!({ "key": "th-not-a-real-key-custom" })),
    )
    .await;

    let (_, dto, raw) = send(
        &state,
        "factsllm",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    assert_eq!(dto["inferenceHasOwnKey"], true, "{raw}");
}

/// A legacy `composio/token` (the pre-1a address, never mirrored to the new
/// one) still counts as the Composio slot's own key —
/// `load_tinyhumans_key`'s own fallback read. The account key is written
/// **raw** here, not through `PUT …/credential`: that route's own fan-out
/// would fill the new `composio/tinyhumans/key` address and this test needs
/// it to stay empty, exactly the M14 shape (a company whose Composio
/// credential was only ever read through the legacy address).
#[tokio::test]
async fn a_legacy_composio_token_counts_as_the_composio_slot() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "factslegacy", GRANTED).await;

    let id = CompanyId::new("factslegacy");
    let runtime = state.registry().get(&id).expect("registered");
    runtime
        .secrets()
        .set(
            &id,
            crate::company::company_key::KEY_KEY,
            crate::ports::types::SecretValue(KEY.to_string()),
        )
        .await
        .unwrap();
    runtime
        .secrets()
        .set(
            &id,
            crate::company::composio::LEGACY_TOKEN_KEY,
            crate::ports::types::SecretValue("th-not-a-real-key-custom".to_string()),
        )
        .await
        .unwrap();

    let (_, dto, raw) = send(
        &state,
        "factslegacy",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    assert_eq!(dto["composioHasOwnKey"], true, "{raw}");
}

/// A bare provider slug (Q1: "provider chosen, model not chosen") still
/// counts as a set default — never overwritten, and the dialog must not
/// promise a default move that would not happen.
#[tokio::test]
async fn default_set_is_true_for_a_bare_slug() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "factsdefault", GRANTED).await;

    let id = CompanyId::new("factsdefault");
    let runtime = state.registry().get(&id).expect("registered");
    runtime
        .secrets()
        .set(
            &id,
            crate::company::inference::store::DEFAULT_PROVIDER_KEY,
            crate::ports::types::SecretValue("openrouter".to_string()),
        )
        .await
        .unwrap();

    let (_, dto, raw) = send(
        &state,
        "factsdefault",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    assert_eq!(dto["defaultSet"], true, "{raw}");
}

/// None of the three new booleans ever needs to echo a value to compute —
/// the status body carries neither fake key, whatever is set on either slot.
#[tokio::test]
async fn status_never_carries_a_key_when_reporting_own_key_facts() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), "factsleak", GRANTED).await;
    send(
        &state,
        "factsleak",
        "PUT",
        "/api/v1/company/credential",
        Some(json!({ "key": KEY })),
    )
    .await;
    send(
        &state,
        "factsleak",
        "PUT",
        "/api/v1/company/inference/managed/key",
        Some(json!({ "key": "th-not-a-real-key-custom" })),
    )
    .await;
    let id = CompanyId::new("factsleak");
    let runtime = state.registry().get(&id).expect("registered");
    runtime
        .secrets()
        .set(
            &id,
            crate::company::composio::LEGACY_TOKEN_KEY,
            crate::ports::types::SecretValue("th-not-a-real-key-custom".to_string()),
        )
        .await
        .unwrap();
    runtime
        .secrets()
        .set(
            &id,
            crate::company::inference::store::DEFAULT_PROVIDER_KEY,
            crate::ports::types::SecretValue("openrouter".to_string()),
        )
        .await
        .unwrap();

    let (_, _, raw) = send(
        &state,
        "factsleak",
        "GET",
        "/api/v1/company/credential",
        None,
    )
    .await;
    assert!(!raw.contains(KEY), "{raw}");
    assert!(!raw.contains("th-not-a-real-key-custom"), "{raw}");
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

/// The scopes ask reaches the wire, from the shared builder rather than here.
///
/// A key minted without `connections` cannot drive
/// `/agent-integrations/composio/*`, and this route is the only way a console
/// that is not a provisioned tenant gets a key at all — so the ask being
/// absent from this URL is the whole third-party integrations surface
/// answering 403, with nothing local to point at.
#[tokio::test]
async fn starting_a_link_asks_the_hub_for_the_connections_scope() {
    let home_dir = home();
    let state = state_with_hub(home_dir.path(), "acme").await;

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
    let (_, query) = url.split_once('?').expect("a query");

    // Last, and exactly where `key_grant_query` puts it. The route appends
    // nothing of its own to the builder's output, so a `scopes` that arrived
    // by concatenation here would land somewhere else or twice —
    // `hub_identity::both_readers_carry_the_same_built_query` pins the other
    // half of that.
    assert!(
        query.ends_with("&scopes=connections"),
        "managed Composio 403s without the ask: {url}"
    );
    assert_eq!(
        query.matches("scopes=").count(),
        1,
        "one ask, from one builder: {url}"
    );
}

/// The grant runs the same fan-out `PUT …/credential` does, and — per Q10 —
/// declares no provider of its own: `finish_link_runs_the_fan_out_and_writes_no_inference_config`
/// pins the "no entry zero, no `inference/config`" half of that; this test
/// keeps the acceptance shape (one grant, no key echoed, single-use).
#[tokio::test]
async fn finishing_a_link_copies_the_minted_key_without_declaring_a_provider() {
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

    // One grant, and the agents already think through it by resolution
    // (`resolve_effective` reads the account key for a managed provider) — no
    // second declaration needed for Connections to report the identity.
    assert_eq!(resp["status"]["configured"], true);
    assert_eq!(resp["status"]["source"], "company");
    assert!(
        !raw.contains(GRANTED_KEY),
        "the minted key must never be echoed to the console: {raw}"
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
// KR-ACCT-01 (2026-09-15): the wire spellings `frontend/src/api/credential.ts`
// reads — `CompanyCredentialMutation.restartRequired` and
// `CompanyCredentialStatus.inferenceHasModel` — pinned directly against the
// DTOs' own `Serialize` impl, independent of any route or runtime.
// ---------------------------------------------------------------------------

#[test]
fn restart_required_serializes_camel_case_and_is_omitted_when_false() {
    let mut response = super::MutationResponse {
        status: minimal_status(),
        note: "note".to_string(),
        slots: Vec::new(),
        needs_model: false,
        sets_default: false,
        models: Vec::new(),
        used_by: None,
        restart_required: true,
    };
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(
        json["restartRequired"],
        Value::Bool(true),
        "must match frontend/src/api/credential.ts's CompanyCredentialMutation.restartRequired: {json}"
    );

    // Never serialized as `false` — the console reads an absent field as "did
    // not say" and a present `false` would be a second, contradictory way to
    // say the same thing.
    response.restart_required = false;
    let json = serde_json::to_value(&response).unwrap();
    assert!(
        !json.as_object().unwrap().contains_key("restartRequired"),
        "restartRequired must be omitted rather than sent as false: {json}"
    );
}

#[test]
fn inference_has_model_serializes_camel_case() {
    let mut status = minimal_status();
    status.inference_has_model = true;
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(
        json["inferenceHasModel"],
        Value::Bool(true),
        "must match frontend/src/api/credential.ts's CompanyCredentialStatus.inferenceHasModel: {json}"
    );

    status.inference_has_model = false;
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(
        json["inferenceHasModel"],
        Value::Bool(false),
        "unlike restartRequired, this field is a plain bool and is always present: {json}"
    );
}

/// The smallest [`super::CredentialStatusDto`] that serializes without
/// panicking — every field the two tests above don't care about set to its
/// most inert value.
fn minimal_status() -> super::CredentialStatusDto {
    super::CredentialStatusDto {
        configured: false,
        source: crate::company::credentials::CredentialSource::None,
        notice: String::new(),
        account: None,
        hub_link: false,
        inference_has_own_key: false,
        composio_has_own_key: false,
        default_set: false,
        inference_has_model: false,
        used_by: None,
    }
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
