//! Feature-free core for authoring a new workflow graph (issue #112).
//!
//! [`create_company_workflow`] is the single validated-persist sequence for
//! creating a `workflows/<id>.toml`, lifted out of the REST handler so **both**
//! the console's `POST …/workflows` route and the orchestrator's
//! `create_workflow` tool run exactly the same checks and land the exact same
//! artifact — no create-vs-run drift, one place to reason about safety.
//!
//! The sequence, in order, each step an actionable error before anything is
//! written:
//!
//! 1. the id is a safe filename (no slashes / `..`) and within length caps;
//! 2. the graph is within the node/edge size caps (a runaway graph can't be
//!    persisted);
//! 3. it names exactly one `trigger` (a freshly authored graph must say what
//!    starts it — stricter than [`parse_workflow`], which allows many);
//! 4. every `agent` node names a real roster teammate (manifest ∪ overlay);
//! 5. its display name is unique (case-insensitive) against the company's
//!    existing on-disk + manifest-enabled workflows;
//! 6. the rendered TOML re-parses through [`parse_workflow`] (the same
//!    structural validation a hand-authored file passes) and is within the
//!    byte cap;
//! 7. the file is written atomically (`create_new(true)` → a duplicate id is a
//!    [`Conflict`](OpenCompanyError::Conflict), closing the TOCTOU gap);
//! 8. the id is recorded as enabled on the operator's live record (the
//!    version-controlled manifest is never rewritten — the team-overlay
//!    convention); a store-save failure rolls the file back so the id isn't
//!    orphaned;
//! 9. a best-effort [`WorkflowCreated`](CompanyEvent::WorkflowCreated) audit
//!    event is journaled — never rolling the create back if the journal fails.
//!
//! Steps 4–8 run under the per-company [`company_write_lock`] so a concurrent
//! `create_workflow` (tool) and `POST …/workflows` (REST) can never clobber
//! each other's `overlay`/`enabled` write, the same primitive `add_agent` uses.
//!
//! Compiled in the default build (no harness imports) so the REST route reaches
//! it without any feature gate.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::company::{
    RawWorkflow, WorkflowFile, load_company_workflows, parse_workflow, render_workflow,
};
use crate::error::{OpenCompanyError, Result};
use crate::ports::CompanyStore;
use crate::ports::events::EventLog;
use crate::ports::store::company_write_lock;
use crate::ports::types::{CompanyEvent, CompanyId};
use crate::server::ops::language;

/// Max nodes a freshly authored graph may declare. A larger graph is refused
/// before anything is rendered or written.
pub(crate) const MAX_WORKFLOW_NODES: usize = 50;
/// Max edges a freshly authored graph may declare.
pub(crate) const MAX_WORKFLOW_EDGES: usize = 100;
/// Max size of the rendered `workflows/<id>.toml`, checked after render and
/// before the file is written.
pub(crate) const MAX_WORKFLOW_TOML_BYTES: usize = 64 * 1024;
/// Max length of a workflow id (also the on-disk filename stem).
pub(crate) const MAX_WORKFLOW_ID_LEN: usize = 64;
/// Max length of a workflow display name.
pub(crate) const MAX_WORKFLOW_NAME_LEN: usize = 200;

/// Authors and persists a new workflow graph for `company`, returning the
/// parsed [`WorkflowFile`] exactly as [`load_company_workflows`] would hand it
/// to the runner (so what a caller reads back and what runs are identical).
///
/// `source_dir` is the company source directory (`companies/<name>`) whose
/// `workflows/` subtree the graph lands in — the caller supplies it (both
/// surfaces refuse a deployment with no source directory before calling here,
/// with [`language::WORKFLOW_NEEDS_SOURCE_DIR`]). `events` is the company event
/// log for the best-effort audit journal; pass `None` to skip journaling.
///
/// Errors map to the same HTTP statuses the REST route always returned:
/// [`InvalidRequest`](OpenCompanyError::InvalidRequest) → 400,
/// [`Conflict`](OpenCompanyError::Conflict) → 409.
pub(crate) async fn create_company_workflow(
    company: &CompanyId,
    source_dir: &Path,
    store: &Arc<dyn CompanyStore>,
    events: Option<&Arc<dyn EventLog>>,
    draft: RawWorkflow,
) -> Result<WorkflowFile> {
    // --- Input validation (no lock; pure function of the draft) -------------

    if !is_safe_workflow_id(&draft.id) {
        return Err(OpenCompanyError::InvalidRequest(
            language::WORKFLOW_ID_INVALID.to_string(),
        ));
    }
    if draft.id.len() > MAX_WORKFLOW_ID_LEN {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "a workflow id can be at most {MAX_WORKFLOW_ID_LEN} characters."
        )));
    }
    if draft.name.trim().len() > MAX_WORKFLOW_NAME_LEN {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "a workflow name can be at most {MAX_WORKFLOW_NAME_LEN} characters."
        )));
    }
    if draft.nodes.len() > MAX_WORKFLOW_NODES {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "a workflow can have at most {MAX_WORKFLOW_NODES} nodes (this one has {}).",
            draft.nodes.len()
        )));
    }
    if draft.edges.len() > MAX_WORKFLOW_EDGES {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "a workflow can have at most {MAX_WORKFLOW_EDGES} edges (this one has {}).",
            draft.edges.len()
        )));
    }

    // `parse_workflow` only rejects zero triggers (a saved graph may legally
    // have more than one entry point); the creator is stricter — a freshly
    // authored graph must name exactly one starting point.
    let trigger_count = draft.nodes.iter().filter(|n| n.kind == "trigger").count();
    if trigger_count != 1 {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "a workflow needs exactly one `trigger` node to say what starts it (found {trigger_count})."
        )));
    }

    // --- Serialized write section -------------------------------------------
    // Load record → roster check → name uniqueness → file write → save record
    // all under the per-company write lock, so a concurrent create/add_agent
    // can never clobber the record's `enabled`/`overlay` write.
    let write_lock = company_write_lock(company);
    let _lock = write_lock.lock().await;

    let mut record = store
        .load(company)
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.to_string()))?;

    // Cross-check every `agent` node against the company's effective roster
    // (manifest agents ∪ operator overlay teammates). `parse_workflow` checks
    // the graph's own shape but has no roster to validate names against.
    let roster: HashSet<&str> = record
        .manifest
        .agents
        .iter()
        .map(|a| a.id.as_str())
        .chain(record.overlay_agents.iter().map(|a| a.id.as_str()))
        .collect();
    for node in &draft.nodes {
        if node.kind != "agent" {
            continue;
        }
        match node.agent.as_deref() {
            Some(agent_id) if roster.contains(agent_id) => {}
            Some(agent_id) => {
                return Err(OpenCompanyError::InvalidRequest(format!(
                    "node `{}` names teammate `{agent_id}`, which is not on this company's roster.",
                    node.id
                )));
            }
            None => {
                return Err(OpenCompanyError::InvalidRequest(format!(
                    "node `{}` is an agent node but names no teammate.",
                    node.id
                )));
            }
        }
    }

    // Case-insensitive display-name uniqueness against the company's existing
    // workflows (on-disk ∪ manifest-enabled). Id uniqueness stays atomic via
    // `create_new(true)` below — this only guards two *differently-id'd*
    // workflows sharing one indistinguishable name in the picker.
    let existing_names = existing_workflow_names(source_dir, &record.manifest.workflows.enabled);
    if existing_names.contains(&draft.name.trim().to_ascii_lowercase()) {
        return Err(OpenCompanyError::Conflict(format!(
            "A workflow named `{}` already exists. Pick a different name.",
            draft.name.trim()
        )));
    }

    // Render the candidate to TOML and re-parse it through `parse_workflow`,
    // the same structural validation a hand-authored file passes. Any problem
    // becomes an `InvalidRequest` (400), never the 500 a malformed on-disk file
    // gets from the read routes.
    let toml_src = render_workflow(&draft)?;
    if toml_src.len() > MAX_WORKFLOW_TOML_BYTES {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "the rendered workflow is {} bytes, over the {MAX_WORKFLOW_TOML_BYTES}-byte limit.",
            toml_src.len()
        )));
    }
    let file = parse_workflow(&toml_src).map_err(|err| match err {
        OpenCompanyError::DataInvalid { problems, .. } => {
            OpenCompanyError::InvalidRequest(problems.join(" "))
        }
        OpenCompanyError::DataParse { message, .. } => OpenCompanyError::InvalidRequest(message),
        other => other,
    })?;

    let workflows_dir = source_dir.join("workflows");
    let path = workflows_dir.join(format!("{}.toml", file.id));
    std::fs::create_dir_all(&workflows_dir).map_err(|source| OpenCompanyError::StoreIo {
        path: workflows_dir.clone(),
        source,
    })?;

    // Write atomically: `create_new(true)` fails if the path already exists,
    // closing the TOCTOU gap between a separate existence check and the write —
    // a duplicate id is a clean `Conflict` (409). Clean the empty file up on a
    // write failure so the id isn't permanently blocked.
    let mut wf_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => OpenCompanyError::Conflict(format!(
                "A workflow with id `{}` already exists. Pick a different id.",
                file.id
            )),
            _ => OpenCompanyError::StoreIo {
                path: path.clone(),
                source: e,
            },
        })?;
    wf_file.write_all(toml_src.as_bytes()).map_err(|source| {
        let _ = std::fs::remove_file(&path);
        OpenCompanyError::StoreIo {
            path: path.clone(),
            source,
        }
    })?;
    drop(wf_file);

    // Record the id as enabled on the live record — mirrors the team overlay:
    // the version-controlled `company.toml` on disk is never rewritten. Save
    // **after** the file lands; on a save failure remove the file we wrote so a
    // retry can succeed without admin intervention.
    let save_result = if record
        .manifest
        .workflows
        .enabled
        .iter()
        .any(|e| e == &file.id)
    {
        Ok(())
    } else {
        record.manifest.workflows.enabled.push(file.id.clone());
        store.save(&record).await
    };
    if let Err(e) = save_result {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }

    // Drop the write lock before journaling: the audit event is best-effort and
    // never gates the create, so it needn't hold the serialization lock.
    drop(_lock);

    // Best-effort audit journal. A journal failure never rolls the create back
    // (the workflow is already persisted + enabled) — we only log it.
    if let Some(log) = events
        && let Err(err) = log
            .append(
                company,
                CompanyEvent::WorkflowCreated {
                    workflow_id: file.id.clone(),
                    name: file.name.clone(),
                    by: None,
                },
            )
            .await
    {
        tracing::warn!(
            company = %company,
            workflow = %file.id,
            error = %err,
            "workflow created but audit journal append failed"
        );
    }

    Ok(file)
}

/// The set of existing workflow display names (trimmed, lowercased) for
/// `source_dir`'s company: every successfully-loaded on-disk `workflows/*.toml`
/// name, unioned with the id-as-name fallback for each manifest-`enabled` id
/// that has no loadable file — mirroring how `list_workflows` names the same
/// set. A malformed on-disk file contributes no name (it's skipped, same as the
/// picker), so it can't false-positive a conflict.
fn existing_workflow_names(source_dir: &Path, enabled: &[String]) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut seen_ids = HashSet::new();

    if let Ok(entries) = std::fs::read_dir(source_dir.join("workflows")) {
        let ids: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
            .filter_map(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_string)
            })
            .collect();
        for id in ids {
            match load_company_workflows(source_dir, std::slice::from_ref(&id)) {
                Ok(mut files) => {
                    if let Some(file) = files.pop() {
                        names.insert(file.name.trim().to_ascii_lowercase());
                        seen_ids.insert(file.id);
                    }
                }
                // A malformed file skips only its own name (same tolerance the
                // picker has); still mark the id seen so the enabled-fallback
                // below doesn't re-add it as an id-named entry.
                Err(_) => {
                    seen_ids.insert(id);
                }
            }
        }
    }

    // Manifest-enabled ids with no loadable on-disk file fall back to the id as
    // their display name (what `list_workflows` shows), so a new workflow can't
    // collide with that fallback name either.
    for id in enabled {
        if !seen_ids.contains(id) {
            names.insert(id.trim().to_ascii_lowercase());
        }
    }

    names
}

/// Whether `wid` is a single safe on-disk filename stem — no path separators, no
/// `..`, not empty — so it can't escape the `workflows/` directory.
fn is_safe_workflow_id(wid: &str) -> bool {
    use std::path::Component;
    let mut comps = Path::new(wid).components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use crate::company::{CompanyManifest, RawEdge, RawNode};
    use crate::ports::types::{CompanyRecord, CompanySummary, EventSeq, LedgerEntry, StoredEvent};
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream};

    // --- test doubles --------------------------------------------------------

    /// An in-memory `CompanyStore` seeded with one record; `save` can be told to
    /// fail so the file-rollback path is exercised.
    #[derive(Default)]
    struct MemStore {
        record: StdMutex<Option<CompanyRecord>>,
        fail_save: bool,
    }

    impl MemStore {
        fn seeded(record: CompanyRecord) -> Self {
            Self {
                record: StdMutex::new(Some(record)),
                fail_save: false,
            }
        }
        fn failing(record: CompanyRecord) -> Self {
            Self {
                record: StdMutex::new(Some(record)),
                fail_save: true,
            }
        }
    }

    #[async_trait]
    impl CompanyStore for MemStore {
        async fn load(&self, _id: &CompanyId) -> Result<Option<CompanyRecord>> {
            Ok(self.record.lock().unwrap().clone())
        }
        async fn save(&self, record: &CompanyRecord) -> Result<()> {
            if self.fail_save {
                return Err(OpenCompanyError::InvalidRequest("save boom".into()));
            }
            *self.record.lock().unwrap() = Some(record.clone());
            Ok(())
        }
        async fn list(&self) -> Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> Result<()> {
            Ok(())
        }
    }

    /// An in-memory `EventLog` that records appended events so the audit journal
    /// can be asserted.
    #[derive(Default)]
    struct MemLog {
        events: StdMutex<Vec<CompanyEvent>>,
    }

    #[async_trait]
    impl EventLog for MemLog {
        async fn append(&self, _id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
            let mut guard = self.events.lock().unwrap();
            guard.push(event);
            Ok(EventSeq::new(guard.len() as u64))
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> Result<Vec<StoredEvent>> {
            Ok(Vec::new())
        }
        fn subscribe(&self, _id: &CompanyId) -> BoxStream<'static, StoredEvent> {
            Box::pin(stream::empty())
        }
    }

    // --- fixtures ------------------------------------------------------------

    /// A manifest with an `assistant` roster agent so `agent`-node graphs pass
    /// the roster check.
    fn manifest_with_assistant() -> CompanyManifest {
        toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"assistant\"\nrole = \"Assistant\"\n",
        )
        .expect("valid manifest")
    }

    fn record(id: &CompanyId, manifest: CompanyManifest) -> CompanyRecord {
        CompanyRecord {
            id: id.clone(),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
        }
    }

    /// A valid trigger → agent → output draft naming the `assistant` teammate.
    fn valid_draft(id: &str, name: &str) -> RawWorkflow {
        RawWorkflow {
            id: id.to_string(),
            name: name.to_string(),
            description: Some("A tiny graph.".to_string()),
            nodes: vec![
                RawNode {
                    id: "start".to_string(),
                    kind: "trigger".to_string(),
                    name: "Start".to_string(),
                    summary: None,
                    agent: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                },
                RawNode {
                    id: "worker".to_string(),
                    kind: "agent".to_string(),
                    name: "Worker".to_string(),
                    summary: None,
                    agent: Some("assistant".to_string()),
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                },
                RawNode {
                    id: "done".to_string(),
                    kind: "output".to_string(),
                    name: "Report".to_string(),
                    summary: None,
                    agent: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                },
            ],
            edges: vec![
                RawEdge {
                    from: "start".to_string(),
                    to: "worker".to_string(),
                    label: None,
                },
                RawEdge {
                    from: "worker".to_string(),
                    to: "done".to_string(),
                    label: Some("ok".to_string()),
                },
            ],
        }
    }

    fn store_of(store: MemStore) -> Arc<dyn CompanyStore> {
        Arc::new(store)
    }

    // --- happy path ----------------------------------------------------------

    #[tokio::test]
    async fn creates_enables_and_journals() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let log = Arc::new(MemLog::default());
        let log_dyn: Arc<dyn EventLog> = log.clone();

        let file = create_company_workflow(
            &company,
            dir.path(),
            &store,
            Some(&log_dyn),
            valid_draft("greeter", "Greeter"),
        )
        .await
        .expect("creates");

        assert_eq!(file.id, "greeter");
        assert_eq!(file.nodes.len(), 3);

        // File landed and re-loads to exactly what we returned (contract).
        let reloaded = load_company_workflows(dir.path(), std::slice::from_ref(&file.id))
            .expect("reloads")
            .into_iter()
            .next()
            .expect("one file");
        assert_eq!(
            reloaded, file,
            "returned WorkflowFile must equal the on-disk load"
        );

        // Enabled on the record.
        let record = store.load(&company).await.unwrap().unwrap();
        assert!(
            record
                .manifest
                .workflows
                .enabled
                .contains(&"greeter".to_string())
        );

        // Journaled a WorkflowCreated audit event.
        let events = log.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            CompanyEvent::WorkflowCreated {
                workflow_id,
                name,
                by,
            } => {
                assert_eq!(workflow_id, "greeter");
                assert_eq!(name, "Greeter");
                assert!(by.is_none());
            }
            other => panic!("expected WorkflowCreated, got {other:?}"),
        }
    }

    // --- guardrail failures --------------------------------------------------

    #[tokio::test]
    async fn duplicate_id_is_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));

        create_company_workflow(
            &company,
            dir.path(),
            &store,
            None,
            valid_draft("dup", "First"),
        )
        .await
        .expect("first create");
        let err = create_company_workflow(
            &company,
            dir.path(),
            &store,
            None,
            valid_draft("dup", "Second name"),
        )
        .await
        .expect_err("second create with same id");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
    }

    #[tokio::test]
    async fn duplicate_name_case_insensitive_is_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));

        create_company_workflow(
            &company,
            dir.path(),
            &store,
            None,
            valid_draft("one", "Greeter"),
        )
        .await
        .expect("first");
        let err = create_company_workflow(
            &company,
            dir.path(),
            &store,
            None,
            valid_draft("two", "  GREETER  "),
        )
        .await
        .expect_err("name collides case-insensitively");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
    }

    #[tokio::test]
    async fn unknown_roster_teammate_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let mut draft = valid_draft("wf", "WF");
        draft.nodes[1].agent = Some("ghost".to_string());

        let err = create_company_workflow(&company, dir.path(), &store, None, draft)
            .await
            .expect_err("unknown teammate");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[tokio::test]
    async fn missing_agent_on_agent_node_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let mut draft = valid_draft("wf", "WF");
        draft.nodes[1].agent = None;

        let err = create_company_workflow(&company, dir.path(), &store, None, draft)
            .await
            .expect_err("agent node with no teammate");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn zero_or_two_triggers_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));

        // Zero triggers.
        let mut zero = valid_draft("z", "Z");
        zero.nodes[0].kind = "output".to_string();
        let err = create_company_workflow(&company, dir.path(), &store, None, zero)
            .await
            .expect_err("no trigger");
        assert!(err.to_string().contains("exactly one `trigger`"), "{err}");

        // Two triggers.
        let mut two = valid_draft("t", "T");
        two.nodes[2].kind = "trigger".to_string();
        let err = create_company_workflow(&company, dir.path(), &store, None, two)
            .await
            .expect_err("two triggers");
        assert!(err.to_string().contains("exactly one `trigger`"), "{err}");
    }

    #[tokio::test]
    async fn traversal_id_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let err = create_company_workflow(
            &company,
            dir.path(),
            &store,
            None,
            valid_draft("../secrets", "Escape"),
        )
        .await
        .expect_err("traversal id");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn oversized_node_count_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let mut draft = valid_draft("big", "Big");
        for i in 0..MAX_WORKFLOW_NODES {
            draft.nodes.push(RawNode {
                id: format!("n{i}"),
                kind: "output".to_string(),
                name: format!("N{i}"),
                summary: None,
                agent: None,
                config: None,
                on_error: None,
                retry: None,
                requires_approval: None,
            });
        }
        assert!(draft.nodes.len() > MAX_WORKFLOW_NODES);
        let err = create_company_workflow(&company, dir.path(), &store, None, draft)
            .await
            .expect_err("too many nodes");
        assert!(err.to_string().contains("at most"), "{err}");
    }

    #[tokio::test]
    async fn oversized_toml_bytes_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        // Stay within the node cap but blow the byte cap with a huge summary.
        let mut draft = valid_draft("fat", "Fat");
        draft.nodes[0].summary = Some("x".repeat(MAX_WORKFLOW_TOML_BYTES + 10));
        let err = create_company_workflow(&company, dir.path(), &store, None, draft)
            .await
            .expect_err("too many bytes");
        assert!(err.to_string().contains("byte"), "{err}");
    }

    #[tokio::test]
    async fn store_save_failure_rolls_the_file_back() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::failing(record(
            &company,
            manifest_with_assistant(),
        )));

        let err = create_company_workflow(
            &company,
            dir.path(),
            &store,
            None,
            valid_draft("rollback", "Rollback"),
        )
        .await
        .expect_err("save fails");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );

        // The file must have been removed so a retry can succeed.
        let path = dir.path().join("workflows").join("rollback.toml");
        assert!(!path.exists(), "the orphaned file must be cleaned up");
    }

    #[tokio::test]
    async fn no_company_record_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("ghost");
        let store: Arc<dyn CompanyStore> = Arc::new(MemStore::default());
        let err =
            create_company_workflow(&company, dir.path(), &store, None, valid_draft("wf", "WF"))
                .await
                .expect_err("no record");
        assert!(
            matches!(err, OpenCompanyError::CompanyNotFound(_)),
            "{err:?}"
        );
    }
}
