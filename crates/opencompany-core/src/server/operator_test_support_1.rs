use super::*;
use crate::company::CompanyManifest;
use crate::ports::types::CompanyRecord;
use crate::runtime::RuntimeBuilder;
use crate::store::FsCompanyStore;
use crate::{AppConfig, AppState};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

pub(super) fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("opencompany-http-")
        .tempdir()
        .expect("tempdir")
}

pub(super) fn manifest() -> CompanyManifest {
    toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
}

pub(super) async fn state_with_company(home: &std::path::Path, lifecycle: &str) -> AppState {
    build_state(home, lifecycle, AppConfig::default()).await
}

pub(super) async fn build_state(
    home: &std::path::Path,
    lifecycle: &str,
    config: AppConfig,
) -> AppState {
    build_state_with_brain(home, lifecycle, config, None).await
}

/// [`build_state`], optionally swapping the runtime's cognition. The
/// approval-continuation tests need a brain they can stall mid-turn.
pub(super) async fn build_state_with_brain(
    home: &std::path::Path,
    lifecycle: &str,
    config: AppConfig,
    brain: Option<Arc<dyn crate::ports::brain::Brain>>,
) -> AppState {
    build_state_with_brain_and_manifest(home, lifecycle, config, brain, manifest()).await
}

/// [`build_state_with_brain`], with the company manifest chosen by the
/// caller — the approval **deadline** lives in `[policy]`, so a test about
/// what a past-deadline card answers has to be able to set it (issue #1449).
pub(super) async fn build_state_with_brain_and_manifest(
    home: &std::path::Path,
    lifecycle: &str,
    config: AppConfig,
    brain: Option<Arc<dyn crate::ports::brain::Brain>>,
    manifest: CompanyManifest,
) -> AppState {
    // Pre-seed a record so the builder preserves the requested lifecycle.
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    use crate::ports::CompanyStore;
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: lifecycle.to_string(),
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

    let mut builder = RuntimeBuilder::new(home.to_path_buf(), manifest).with_id(id.clone());
    if let Some(brain) = brain {
        builder = builder.with_brain(brain);
    }
    let runtime = builder.build().await.unwrap();
    let state = AppState::new(config);
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

/// A run store that refuses every verb — the persistence layer mid-outage.
/// `accept_chat_turn` treats a refused row best-effort, so this store is
/// what probes the other half of that promise: the turn still runs and the
/// request still gets an answer, it just cannot be a pollable `202`.
pub(super) struct FailingRunStore;

#[async_trait::async_trait]
impl crate::ports::runs::RunStore for FailingRunStore {
    async fn create_run(
        &self,
        _company: &CompanyId,
        _spec: crate::ports::runs::NewRun,
    ) -> crate::Result<crate::ports::runs::RunRecord> {
        Err(OpenCompanyError::InvalidRequest(
            "run store offline".to_string(),
        ))
    }
    async fn get_run(
        &self,
        _company: &CompanyId,
        _id: &str,
    ) -> crate::Result<Option<crate::ports::runs::RunRecord>> {
        Ok(None)
    }
    async fn put_run(
        &self,
        _company: &CompanyId,
        _run: &crate::ports::runs::RunRecord,
    ) -> crate::Result<()> {
        Err(OpenCompanyError::InvalidRequest(
            "run store offline".to_string(),
        ))
    }
    async fn list_runs(
        &self,
        _company: &CompanyId,
        _filter: &crate::ports::runs::RunFilter,
    ) -> crate::Result<Vec<crate::ports::runs::RunRecord>> {
        Ok(Vec::new())
    }
    async fn append_run_step(
        &self,
        _company: &CompanyId,
        _step: &crate::ports::runs::RunStepRecord,
    ) -> crate::Result<()> {
        Err(OpenCompanyError::InvalidRequest(
            "run store offline".to_string(),
        ))
    }
    async fn list_run_steps(
        &self,
        _company: &CompanyId,
        _run_id: &str,
    ) -> crate::Result<Vec<crate::ports::runs::RunStepRecord>> {
        Ok(Vec::new())
    }
}

/// A [`CompanyStore`](crate::ports::CompanyStore) whose `load` can be told to
/// fail a fixed number of times before going back to answering from its real
/// backing store — a transient roster-read outage rather than a permanently
/// dead store.
pub(super) struct SometimesFailingCompanyStore {
    inner: FsCompanyStore,
    remaining_load_failures: std::sync::atomic::AtomicUsize,
}

impl SometimesFailingCompanyStore {
    pub(super) fn new(inner: FsCompanyStore) -> Self {
        Self {
            inner,
            remaining_load_failures: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(super) fn fail_next_loads(&self, count: usize) {
        self.remaining_load_failures
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crate::ports::CompanyStore for SometimesFailingCompanyStore {
    async fn load(&self, id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
        let remaining = self
            .remaining_load_failures
            .load(std::sync::atomic::Ordering::SeqCst);
        if remaining > 0 {
            self.remaining_load_failures
                .store(remaining - 1, std::sync::atomic::Ordering::SeqCst);
            return Err(OpenCompanyError::InvalidRequest(
                "company store offline".to_string(),
            ));
        }
        self.inner.load(id).await
    }
    async fn save(&self, record: &CompanyRecord) -> crate::Result<()> {
        self.inner.save(record).await
    }
    async fn list(&self) -> crate::Result<Vec<crate::ports::types::CompanySummary>> {
        self.inner.list().await
    }
    async fn append_ledger(
        &self,
        id: &CompanyId,
        entry: crate::ports::types::LedgerEntry,
    ) -> crate::Result<()> {
        self.inner.append_ledger(id, entry).await
    }
}

/// [`state_with_roster`], with the company store swapped for a
/// [`SometimesFailingCompanyStore`] the caller can arm after the runtime has
/// booted — the fixture for a roster read failing mid-request rather than at
/// boot.
pub(super) async fn state_with_roster_and_failing_store(
    home: &std::path::Path,
) -> (AppState, Arc<SometimesFailingCompanyStore>) {
    let seed_store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    use crate::ports::CompanyStore;
    seed_store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: roster_manifest(),
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

    let failing_store = Arc::new(SometimesFailingCompanyStore::new(FsCompanyStore::new(
        home.to_path_buf(),
    )));
    let runtime = RuntimeBuilder::new(home.to_path_buf(), roster_manifest())
        .with_id(id.clone())
        .with_store(failing_store.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    (state, failing_store)
}

/// [`state_with_company`] with the run store swapped for one that refuses
/// every verb — the setup for the rowless-turn tests.
pub(super) async fn state_with_failing_runs(home: &std::path::Path) -> AppState {
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    use crate::ports::CompanyStore;
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
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
        .with_id(id.clone())
        .with_runs(Arc::new(FailingRunStore))
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

// ── Issue #982: the card goes to whoever was addressed ──────────────────

/// A roster with three teammates and one desk, so a chat can be addressed
/// to something that exists.
///
/// The ids are the ones the smoke that found #982 used, and the roles are
/// deliberately distinct words, so a test can address one teammate in a
/// message whose text points at another.
pub(super) fn roster_manifest() -> CompanyManifest {
    toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "product_manager"
role = "Product Manager"

[[agent]]
id = "backend_engineer"
role = "Backend Engineer"

[[agent]]
id = "designer"
role = "Designer"

[[group_chat]]
id = "engineering"
name = "Engineering"
members = ["backend_engineer"]

[policy]
mode = "full"
"#,
    )
    .unwrap()
}

/// [`state_with_company`] over [`roster_manifest`]. Written out rather than
/// threaded through the shared builders above, which several other suites
/// call with the roster-less fixture.
pub(super) async fn state_with_roster(home: &std::path::Path) -> AppState {
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    use crate::ports::CompanyStore;
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: roster_manifest(),
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
    let runtime = RuntimeBuilder::new(home.to_path_buf(), roster_manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

/// [`roster_manifest`] plus a **memberless** desk — one that exists on the
/// roster but has nobody seated on it, the `EmptyDesk` shape `mention_context`
/// still has to canonicalize.
pub(super) fn memberless_desk_manifest() -> CompanyManifest {
    toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "product_manager"
role = "Product Manager"

[[agent]]
id = "backend_engineer"
role = "Backend Engineer"

[[group_chat]]
id = "engineering"
name = "Engineering"
members = ["backend_engineer"]

[[group_chat]]
id = "sales"
name = "Sales"
members = []

[policy]
mode = "full"
"#,
    )
    .unwrap()
}

/// [`roster_manifest`] plus a desk **literally named** `dm:engineering`,
/// beside the ordinary `engineering` desk — the shape `mention_context`
/// must resolve **as sent** instead of stripping the `dm:` prefix away.
pub(super) fn dm_prefixed_desk_manifest() -> CompanyManifest {
    toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "product_manager"
role = "Product Manager"

[[agent]]
id = "backend_engineer"
role = "Backend Engineer"

[[group_chat]]
id = "engineering"
name = "Engineering"
members = ["backend_engineer"]

[[group_chat]]
id = "dm:engineering"
name = "Dm Engineering"
members = ["backend_engineer"]

[policy]
mode = "full"
"#,
    )
    .unwrap()
}

/// [`state_with_roster`] over [`memberless_desk_manifest`].
pub(super) async fn state_with_memberless_desk(home: &std::path::Path) -> AppState {
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    use crate::ports::CompanyStore;
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: memberless_desk_manifest(),
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
    let runtime = RuntimeBuilder::new(home.to_path_buf(), memberless_desk_manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

/// [`state_with_roster`] over [`dm_prefixed_desk_manifest`].
pub(super) async fn state_with_dm_prefixed_desk(home: &std::path::Path) -> AppState {
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    use crate::ports::CompanyStore;
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: dm_prefixed_desk_manifest(),
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
    let runtime = RuntimeBuilder::new(home.to_path_buf(), dm_prefixed_desk_manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

/// One chat request, optionally addressed to a thread.
pub(super) fn chat_to(text: &str, chat: Option<&str>) -> Request<Body> {
    chat_in_thread(text, chat, None)
}

/// The same send, typed inside a thread — `parent` is the root the console
/// sends when the operator answers in an open thread (#1890 B).
pub(super) fn chat_in_thread(text: &str, chat: Option<&str>, parent: Option<u64>) -> Request<Body> {
    chat_request(text, chat, parent, None)
}

/// A send with the composer's "Build me the workflow" control pressed — the
/// one signal on which the chat route still opens a card by itself. The tests
/// that pin what a route-opened card *records* (its assignee, its origin
/// thread) send this, because a plain message no longer opens one: tracking
/// is the agent's own tool call.
pub(super) fn workflow_chat_to(text: &str, chat: Option<&str>) -> Request<Body> {
    chat_request(text, chat, None, Some("workflow"))
}

/// [`workflow_chat_to`], typed inside a thread.
pub(super) fn workflow_chat_in_thread(
    text: &str,
    chat: Option<&str>,
    parent: Option<u64>,
) -> Request<Body> {
    chat_request(text, chat, parent, Some("workflow"))
}

fn chat_request(
    text: &str,
    chat: Option<&str>,
    parent: Option<u64>,
    deliverable: Option<&str>,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/company/chat")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "text": text,
                "chat": chat,
                // A string, like every other message id on this API — the
                // field's own note says so, and a number is a 422.
                "parent": parent.map(|seq| seq.to_string()),
                "deliverable": deliverable,
            })
            .to_string(),
        ))
        .unwrap()
}

/// The message the smoke sent: an actionable ask whose *text* points at one
/// teammate, addressed to a different one.
pub(super) const CROSSED: &str = "build the backend deployment pipeline";

/// A task store that lists cleanly but refuses every write — the board
/// persistence layer mid-outage, for CHAT-021.
pub(super) struct FailingTaskUpsert;

#[async_trait::async_trait]
impl crate::ports::tasks::TaskStore for FailingTaskUpsert {
    async fn list(
        &self,
        _company: &CompanyId,
    ) -> crate::Result<Vec<crate::ports::tasks::TaskRecord>> {
        Ok(Vec::new())
    }
    async fn upsert(
        &self,
        _company: &CompanyId,
        _task: &crate::ports::tasks::TaskRecord,
    ) -> crate::Result<()> {
        Err(OpenCompanyError::InvalidRequest(
            "task store offline".to_string(),
        ))
    }
    async fn update_if_column(
        &self,
        _company: &CompanyId,
        _task: &crate::ports::tasks::TaskRecord,
        _observed: &crate::ports::tasks::TaskRecord,
        _expected_column: &str,
    ) -> crate::Result<bool> {
        Err(OpenCompanyError::InvalidRequest(
            "task store offline".to_string(),
        ))
    }
    async fn delete(&self, _company: &CompanyId, _id: &str) -> crate::Result<bool> {
        Ok(false)
    }
}

/// A manifest with two agents and one desk (`studio`, led by `ceo`), used by
/// the desk-membership write tests.
pub(super) fn desk_manifest() -> CompanyManifest {
    toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n\
         [[group_chat]]\nid = \"studio\"\nname = \"Studio\"\nmembers = [\"ceo\"]\n",
    )
    .unwrap()
}

/// A bare record carrying `manifest`, for resolvers that read nothing else.
pub(super) fn record_with(manifest: CompanyManifest) -> CompanyRecord {
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest,
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
    }
}

/// Builds an app state whose sole company carries `manifest`.
pub(super) async fn state_with_manifest(
    home: &std::path::Path,
    manifest: CompanyManifest,
) -> AppState {
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    use crate::ports::CompanyStore;
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
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

pub(super) async fn get_desks(app: &axum::Router, cookie: &str) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/desks")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Creates an overlay desk through the same route `create_desk` serves and
/// returns its derived id, so the desk under test exists only in the overlay
/// — nothing about it is declared in the manifest.
pub(super) async fn seed_overlay_desk(app: &axum::Router, cookie: &str, body: &str) -> String {
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/desks")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let bytes = to_bytes(created.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["id"].as_str().unwrap().to_string()
}

pub(super) async fn post_desk_member(
    app: &axum::Router,
    cookie: &str,
    desk: &str,
    body: &str,
) -> Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/company/desks/{desk}/members"))
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}
