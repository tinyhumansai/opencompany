//! Feature-free core for authoring a new workflow graph (issue #112).
//!
//! [`create_company_workflow`] is the single validated-persist sequence for
//! creating a workflow graph, shared by **both** the console's
//! `POST …/workflows` route and the orchestrator's `create_workflow` tool so
//! they run exactly the same checks and land the exact same artifact — no
//! create-vs-run drift, one place to reason about safety.
//!
//! The graph body is persisted **on the company record** as an
//! [`OverlayWorkflow`], never written into the company source tree. The source
//! tree is the version-controlled seed and, in hosted mode, a read-only crate
//! mount — writing there failed every hosted tenant with `EROFS` (issue #168).
//! Readers union the two sources via
//! [`load_workflow_union`](crate::company::load_workflow_union), with the seed
//! file winning on an id collision.
//!
//! The sequence, in order, each step an actionable error before anything is
//! persisted:
//!
//! 1. the id is a safe filename stem (no slashes / `..`) and within length caps
//!    — it is still an id a seed file could carry, so the two sources stay
//!    interchangeable;
//! 2. the graph is within the node/edge size caps (a runaway graph can't be
//!    persisted);
//! 3. it names exactly one `trigger` (a freshly authored graph must say what
//!    starts it — stricter than [`parse_workflow`], which allows many);
//! 4. every `agent` node names a real roster teammate (manifest ∪ overlay);
//! 5. its id is unique against the company's seed ids ∪ overlay ids ∪
//!    manifest-enabled ids (a [`Conflict`](OpenCompanyError::Conflict));
//! 6. its display name is unique (case-insensitive) against the company's
//!    existing seed + overlay + manifest-enabled workflows;
//! 7. the rendered TOML re-parses through [`parse_workflow`] (the same
//!    structural validation a hand-authored file passes) and is within the byte
//!    cap;
//! 8. the body **and** the enabled id are pushed onto the record and persisted
//!    in **one** [`save`](CompanyStore::save) — a single atomic write, so there
//!    is no half-created state to roll back;
//! 9. a best-effort [`WorkflowCreated`](CompanyEvent::WorkflowCreated) audit
//!    event is journaled — never rolling the create back if the journal fails.
//!
//! Steps 4–8 run under the per-company [`company_write_lock`] so a concurrent
//! `create_workflow` (tool) and `POST …/workflows` (REST) can never clobber
//! each other's `overlay`/`enabled` write, the same primitive `add_agent` uses.
//! That lock is what makes the id-uniqueness check of step 5 atomic now that
//! the filesystem's `create_new(true)` no longer serializes the two surfaces.
//!
//! Compiled in the default build (no harness imports) so the REST route reaches
//! it without any feature gate.
//!
//! # Editing and removing (issue #259)
//!
//! [`update_company_workflow`] and [`delete_company_workflow`] complete the
//! write lifecycle. Both run the same validation and hold the same
//! [`company_write_lock`] as create, and both are **overlay-only**: they refuse
//! (with a [`Conflict`](OpenCompanyError::Conflict)) to touch an id backed by a
//! seed file or by a bodiless manifest-`enabled` entry. That is not squeamishness
//! about writing to disk, it is the only shape that is honest about what the
//! reader will do:
//!
//! * [`load_workflow_union`](crate::company::load_workflow_union) gives the
//!   **seed file precedence** on an id collision, so persisting an overlay edit
//!   for a seed-backed id would store a graph the read path never serves — the
//!   operator's change would appear to save and then silently not exist.
//! * `merge_enabled_workflows` (`src/runtime/builder.rs`, issue #208) rebuilds
//!   `[workflows].enabled` at boot from seed ids ∪ surviving overlay ids, so a
//!   "deleted" seed workflow would come back on the next restart.
//!
//! The same invariant is what makes an overlay delete *durable*: with no overlay
//! body left, the boot merge has nothing to re-enable.
//!
//! ## The version token
//!
//! [`workflow_version`] hashes the stored overlay TOML. `GET …/workflows/{wid}`
//! hands it out, a `PUT`/`DELETE` may hand it back, and the comparison happens
//! **inside** the write lock immediately before the mutation — so it is a real
//! optimistic-concurrency guard, not a check-then-act race. The token is opaque
//! on the wire (the contract is "echo back what the read returned"), so the
//! algorithm can change without a client migration.
//!
//! Passing no token is an unconditional write. That mirrors OpenHuman's
//! `flows_update`, whose `expected_version` is likewise `Option`: it keeps a
//! `curl` caller usable without a read-modify-write dance, while the console —
//! which has a stale-tab problem — always sends one.
//!
//! ## What is deliberately not here
//!
//! * **No revision history.** OpenHuman keeps a bounded snapshot ring in a
//!   dedicated `flow_revisions` table; our overlay bodies live inside
//!   `CompanyRecord`, which is loaded and saved *whole* on every write, so a ring
//!   per workflow would bloat that hot path. It needs its own store surface.
//! * **No run-history reaping.** Past runs are
//!   [`WorkflowRunFinished`](CompanyEvent::WorkflowRunFinished) entries on the
//!   company's single append-only journal, interleaved with chat and audit. What
//!   a workflow did stays true after the workflow is gone, and `GET
//!   …/workflows/runs` keeps serving those rows.
//! * **No schedule re-registration.** Nothing to re-register:
//!   [`WorkflowScheduler::tick`](crate::runtime::WorkflowScheduler) re-reads the
//!   record and re-derives the schedule set from the overlay union every minute,
//!   so the tick *is* a continuous reconcile. OpenHuman needs
//!   `reconcile_schedule_triggers_on_boot` because a bound cron job lives in a
//!   second durable store (`cron.db`) that can drift from `flows.db`; we persist
//!   no registration at all, so that class of bug cannot arise here.
//! * **No disarm-on-edit.** OpenHuman forces `enabled = false` when an edit turns
//!   a manual trigger into an automatic one, so a schedule cannot go live
//!   unreviewed. That rule has no lever here yet: our scheduler deliberately does
//!   not gate on `[workflows].enabled` at all (see `workflow_scheduler.rs`), so
//!   writing `false` would stop nothing. Note this makes update no riskier than
//!   create, which already persists a live schedule the same way. Reversing the
//!   scheduler decision and adopting the disarm rule for create and update
//!   together is one follow-up, not two.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::company::{
    RawWorkflow, WorkflowFile, list_workflows_union, parse_workflow, render_workflow,
};
use crate::error::{OpenCompanyError, Result};
use crate::ports::CompanyStore;
use crate::ports::events::EventLog;
use crate::ports::store::company_write_lock;
use crate::ports::types::{CompanyEvent, CompanyId, CompanyRecord, OverlayWorkflow};
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
/// parsed [`WorkflowFile`] exactly as
/// [`load_workflow_union`](crate::company::load_workflow_union) would hand it
/// to the runner (so what a caller reads back and what runs are identical).
///
/// The body is persisted on the company record, so this works on a deployment
/// with **no** source directory at all — the hosted case that used to be
/// refused outright and then failed with `EROFS` anyway (issue #168).
/// `source_dir` is the company source directory (`companies/<name>`) when one
/// exists, read-only here: its `workflows/` subtree contributes the seed ids and
/// names the uniqueness checks guard against. `events` is the company event log
/// for the best-effort audit journal; pass `None` to skip journaling.
///
/// Errors map to the same HTTP statuses the REST route always returned:
/// [`InvalidRequest`](OpenCompanyError::InvalidRequest) → 400,
/// [`Conflict`](OpenCompanyError::Conflict) → 409.
pub(crate) async fn create_company_workflow(
    company: &CompanyId,
    source_dir: Option<&Path>,
    store: &Arc<dyn CompanyStore>,
    events: Option<&Arc<dyn EventLog>>,
    draft: RawWorkflow,
) -> Result<WorkflowFile> {
    // --- Input validation (no lock; pure function of the draft) -------------
    validate_draft_shape(&draft)?;

    // --- Serialized write section -------------------------------------------
    // Load record → roster check → id/name uniqueness → save record all under
    // the per-company write lock, so a concurrent create/add_agent can never
    // clobber the record's `enabled`/`overlay` write.
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

    // Id uniqueness against every id this company already answers for: the seed
    // files, the record's overlay bodies, and the manifest-enabled ids. The
    // write lock held above is what makes this atomic — it replaces the
    // filesystem `create_new(true)` that used to serialize the two surfaces.
    // The seed side is checked by path rather than by scanning: a *malformed*
    // seed file still owns its id (it would shadow the overlay body on read),
    // and a scan would silently skip it.
    let id_taken = seed_file_exists(source_dir, &draft.id)
        || record.overlay_workflows.iter().any(|w| w.id == draft.id)
        || record
            .manifest
            .workflows
            .enabled
            .iter()
            .any(|id| id == &draft.id);
    if id_taken {
        return Err(OpenCompanyError::Conflict(format!(
            "A workflow with id `{}` already exists. Pick a different id.",
            draft.id
        )));
    }

    // Case-insensitive display-name uniqueness against the company's existing
    // workflows (seed ∪ overlay ∪ manifest-enabled), so two differently-id'd
    // workflows can't share one indistinguishable name in the picker.
    let existing_names = existing_workflow_names(
        source_dir,
        &record.overlay_workflows,
        &record.manifest.workflows.enabled,
    );
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

    // Persist the graph body and the enabled id in ONE save. Both live on the
    // record, so there is no file-then-record window to roll back: the save
    // either lands both or neither. The version-controlled `company.toml` on
    // disk is never rewritten — the same team-overlay convention `add_agent`
    // follows.
    record.overlay_workflows.push(OverlayWorkflow {
        id: file.id.clone(),
        toml: toml_src,
    });
    if !record
        .manifest
        .workflows
        .enabled
        .iter()
        .any(|e| e == &file.id)
    {
        record.manifest.workflows.enabled.push(file.id.clone());
    }
    store.save(&record).await?;

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

/// The validation that is a pure function of the draft — safe id, size caps,
/// exactly one `trigger` — shared verbatim by [`create_company_workflow`] and
/// [`update_company_workflow`] so a bad edit is refused on exactly the same
/// terms as a bad create. Runs before any lock is taken: nothing here reads the
/// company record.
fn validate_draft_shape(draft: &RawWorkflow) -> Result<()> {
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
    // have more than one entry point); the author-time path is stricter — a
    // graph written from the console must name exactly one starting point.
    let trigger_count = draft.nodes.iter().filter(|n| n.kind == "trigger").count();
    if trigger_count != 1 {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "a workflow needs exactly one `trigger` node to say what starts it (found {trigger_count})."
        )));
    }

    Ok(())
}

/// An opaque version token for a stored overlay body: the hex SHA-256 of the
/// TOML exactly as persisted.
///
/// The wire contract is "echo back what the read handed you" — callers must not
/// parse it, derive it, or compare it to anything but another token from the
/// same route. That is what lets the algorithm change later without a client
/// migration.
///
/// Hashing the body rather than stamping a counter or a timestamp means the
/// token is a pure function of what is stored: it needs no extra field on
/// [`OverlayWorkflow`], it is stable across a save that rewrites the record for
/// unrelated reasons, and two writers who happen to persist byte-identical TOML
/// do not conflict — because there is nothing to lose between them.
pub(crate) fn workflow_version(toml: &str) -> String {
    let digest = Sha256::digest(toml.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Whether `wid` is backed by a **seed file** in the company source tree.
///
/// Checked by path rather than by scanning: a *malformed* seed file still owns
/// its id — it would shadow an overlay body on read — and a scan that parses
/// would silently skip it.
///
/// This is the one probe behind three answers that must agree: create's
/// id-uniqueness 409, update/delete's "not editable from the console" 409, and
/// the read routes' `editable` flag. Duplicating it is how the console ends up
/// offering an Edit button for a graph the host will refuse.
pub(crate) fn seed_file_exists(source_dir: Option<&Path>, wid: &str) -> bool {
    source_dir.is_some_and(|dir| dir.join("workflows").join(format!("{wid}.toml")).is_file())
}

/// Resolves `wid` to the record's overlay body, or explains why it can't be
/// written to. Shared by update and delete so the two answer identically.
///
/// * seed-backed → [`Conflict`](OpenCompanyError::Conflict): the read path would
///   keep serving the seed, and a boot rebuild would resurrect it;
/// * enabled but bodiless → [`Conflict`](OpenCompanyError::Conflict): a
///   provisioned id with no graph in either source, so there is nothing to
///   replace or remove;
/// * unknown → [`CompanyNotFound`](OpenCompanyError::CompanyNotFound) (404).
///
/// Returns the index into `record.overlay_workflows`, so the caller can replace
/// the body **in place** and keep the picker's order stable across an edit.
fn locate_editable_overlay(
    record: &CompanyRecord,
    source_dir: Option<&Path>,
    wid: &str,
) -> Result<usize> {
    if seed_file_exists(source_dir, wid) {
        return Err(OpenCompanyError::Conflict(format!(
            "Workflow `{wid}` is defined by a file in the company source tree, so it can't be \
             changed or removed from the console. Edit `workflows/{wid}.toml` in the company \
             repository instead."
        )));
    }

    if let Some(index) = record.overlay_workflows.iter().position(|w| w.id == wid) {
        return Ok(index);
    }

    if record.manifest.workflows.enabled.iter().any(|id| id == wid) {
        return Err(OpenCompanyError::Conflict(format!(
            "Workflow `{wid}` is enabled for this company but has no saved graph to change or \
             remove — it was provisioned by name only."
        )));
    }

    Err(OpenCompanyError::CompanyNotFound(format!("workflow {wid}")))
}

/// Fails with a [`Conflict`](OpenCompanyError::Conflict) when the caller's
/// `expected` token disagrees with what is actually stored.
///
/// Called **inside** the write lock, immediately before the mutation — the whole
/// point is that the compare and the save are one critical section, so a writer
/// that lands in between cannot be overwritten. `None` is an unconditional
/// write; see the module docs for why that stays allowed.
fn check_expected_version(expected: Option<&str>, current_toml: &str) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let current = workflow_version(current_toml);
    if expected != current {
        return Err(OpenCompanyError::Conflict(format!(
            "This workflow changed since you loaded it (current version `{current}`). Reload it \
             to see the latest, then reapply your change."
        )));
    }
    Ok(())
}

/// Replaces an existing workflow graph wholesale, returning the parsed
/// [`WorkflowFile`] the union read path will now serve.
///
/// Runs [`create_company_workflow`]'s validation with three deltas:
///
/// 1. the id must resolve to an **overlay body** that is not shadowed by a seed
///    file (see [`locate_editable_overlay`]) — 409 if it is source-defined or
///    body-less, 404 if it is unknown;
/// 2. display-name uniqueness excludes this workflow's own current name, so
///    re-saving without renaming isn't a self-conflict;
/// 3. `expected_version`, when supplied, must match what is stored — compared
///    under the same lock as the save.
///
/// The overlay is replaced **in place** (order preserved, so the picker doesn't
/// reshuffle on an edit) and `[workflows].enabled` is left exactly as it was: a
/// workflow that was enabled stays enabled across an edit, and one that somehow
/// wasn't is not silently armed by saving it.
pub(crate) async fn update_company_workflow(
    company: &CompanyId,
    source_dir: Option<&Path>,
    store: &Arc<dyn CompanyStore>,
    events: Option<&Arc<dyn EventLog>>,
    draft: RawWorkflow,
    expected_version: Option<&str>,
) -> Result<WorkflowFile> {
    // --- Input validation (no lock; pure function of the draft) -------------
    validate_draft_shape(&draft)?;

    // --- Serialized write section -------------------------------------------
    let write_lock = company_write_lock(company);
    let _lock = write_lock.lock().await;

    let mut record = store
        .load(company)
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.to_string()))?;

    // Which overlay body are we replacing — and may we replace it at all?
    let index = locate_editable_overlay(&record, source_dir, &draft.id)?;

    // Optimistic concurrency, inside the lock and before any mutation.
    check_expected_version(expected_version, &record.overlay_workflows[index].toml)?;

    // Same roster cross-check as create: `parse_workflow` validates the graph's
    // own shape but has no roster to check `agent` node names against.
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

    // Display-name uniqueness, MINUS this workflow's own current name — a
    // re-save that doesn't rename must not collide with itself. Every other
    // workflow's name is still guarded, so an edit can't take a sibling's name.
    let mut existing_names = existing_workflow_names(
        source_dir,
        &record.overlay_workflows,
        &record.manifest.workflows.enabled,
    );
    // A body that no longer parses contributes no name to the set above either
    // (the union scan skips it), so the two stay consistent.
    if let Ok(current) = parse_workflow(&record.overlay_workflows[index].toml) {
        existing_names.remove(&current.name.trim().to_ascii_lowercase());
    }
    if existing_names.contains(&draft.name.trim().to_ascii_lowercase()) {
        return Err(OpenCompanyError::Conflict(format!(
            "A workflow named `{}` already exists. Pick a different name.",
            draft.name.trim()
        )));
    }

    // Render and re-parse through the same structural validation a hand-authored
    // file passes, so a bad edit is a 400 rather than a persisted graph that
    // breaks the read routes.
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

    // Replace in place: same slot, same order, so the picker doesn't reshuffle.
    record.overlay_workflows[index] = OverlayWorkflow {
        id: file.id.clone(),
        toml: toml_src,
    };
    store.save(&record).await?;

    drop(_lock);

    // Best-effort audit journal — id and name only, never the body.
    if let Some(log) = events
        && let Err(err) = log
            .append(
                company,
                CompanyEvent::WorkflowUpdated {
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
            "workflow updated but audit journal append failed"
        );
    }

    Ok(file)
}

/// Removes a workflow: its overlay body **and** its id in
/// `[workflows].enabled`, in one save.
///
/// Both halves matter. Dropping only the body would leave an enabled id the
/// picker still lists (under the id as its name, per `list_workflows`'s
/// fallback); dropping only the enabled id would leave a body the union read
/// path still serves and the scheduler still fires. One atomic save means there
/// is no window where a half-deleted workflow exists.
///
/// Gated exactly like [`update_company_workflow`] — source-defined and bodiless
/// ids are refused rather than half-removed — and honours the same optional
/// version token, so "delete the thing I was looking at" can't remove something
/// that changed underneath the operator.
///
/// Returns the removed workflow's display name for the audit journal (falling
/// back to the id when the stored body no longer parses).
pub(crate) async fn delete_company_workflow(
    company: &CompanyId,
    source_dir: Option<&Path>,
    store: &Arc<dyn CompanyStore>,
    events: Option<&Arc<dyn EventLog>>,
    wid: &str,
    expected_version: Option<&str>,
) -> Result<String> {
    if !is_safe_workflow_id(wid) {
        return Err(OpenCompanyError::InvalidRequest(
            language::WORKFLOW_ID_INVALID.to_string(),
        ));
    }

    let write_lock = company_write_lock(company);
    let _lock = write_lock.lock().await;

    let mut record = store
        .load(company)
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.to_string()))?;

    let index = locate_editable_overlay(&record, source_dir, wid)?;
    check_expected_version(expected_version, &record.overlay_workflows[index].toml)?;

    // The display name, read before the body goes away, so the journal entry can
    // name what was removed. A body that no longer parses still deletes — it is
    // exactly the kind an operator most wants gone — it just journals under its
    // id.
    let name = parse_workflow(&record.overlay_workflows[index].toml)
        .map(|f| f.name)
        .unwrap_or_else(|_| wid.to_string());

    // Body and enabled id, one save. `merge_enabled_workflows` (#208) rebuilds
    // `enabled` at boot from seed ids ∪ surviving overlay ids — with the body
    // gone there is nothing left to re-enable, which is what makes this durable
    // across a restart.
    record.overlay_workflows.remove(index);
    record.manifest.workflows.enabled.retain(|id| id != wid);
    store.save(&record).await?;

    drop(_lock);

    if let Some(log) = events
        && let Err(err) = log
            .append(
                company,
                CompanyEvent::WorkflowDeleted {
                    workflow_id: wid.to_string(),
                    name: name.clone(),
                    by: None,
                },
            )
            .await
    {
        tracing::warn!(
            company = %company,
            workflow = %wid,
            error = %err,
            "workflow deleted but audit journal append failed"
        );
    }

    Ok(name)
}

/// The set of existing workflow display names (trimmed, lowercased) for a
/// company: every graph the union read path can serve — the seed
/// `workflows/*.toml` files and the record's overlay bodies — plus the
/// id-as-name fallback for each manifest-`enabled` id that has neither, exactly
/// how `list_workflows` names the same set. A malformed graph contributes no
/// name (it's skipped, same as the picker), so it can't false-positive a
/// conflict; an absent or unreadable source tree simply degrades to overlay ∪
/// enabled.
fn existing_workflow_names(
    source_dir: Option<&Path>,
    overlays: &[OverlayWorkflow],
    enabled: &[String],
) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut seen_ids = HashSet::new();

    for file in list_workflows_union(source_dir, overlays) {
        names.insert(file.name.trim().to_ascii_lowercase());
        seen_ids.insert(file.id);
    }
    // A malformed graph is skipped by the union scan above but still owns its
    // id, so mark those seen too — otherwise the enabled-fallback below would
    // re-add them as id-named entries.
    for overlay in overlays {
        seen_ids.insert(overlay.id.clone());
    }

    // Manifest-enabled ids with no loadable graph fall back to the id as their
    // display name (what `list_workflows` shows), so a new workflow can't
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

    use crate::company::{CompanyManifest, RawEdge, RawNode, load_workflow_union};
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

    /// A committed seed graph, id `seeded` / name `Seeded flow`.
    const SEED_TOML: &str = r#"
id = "seeded"
name = "Seeded flow"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#;

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
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            template_provenance: None,
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
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
                },
                RawNode {
                    id: "worker".to_string(),
                    kind: "agent".to_string(),
                    name: "Worker".to_string(),
                    summary: None,
                    agent: Some("assistant".to_string()),
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
                },
                RawNode {
                    id: "done".to_string(),
                    kind: "output".to_string(),
                    name: "Report".to_string(),
                    summary: None,
                    agent: None,
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
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
            Some(dir.path()),
            &store,
            Some(&log_dyn),
            valid_draft("greeter", "Greeter"),
        )
        .await
        .expect("creates");

        assert_eq!(file.id, "greeter");
        assert_eq!(file.nodes.len(), 3);

        // The body landed on the RECORD, not in the (read-only in hosted mode)
        // source tree — the whole point of #168.
        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1);
        assert_eq!(record.overlay_workflows[0].id, "greeter");
        assert!(
            !dir.path().join("workflows").exists(),
            "creation must not write into the company source tree"
        );

        // The persisted body re-loads to exactly what we returned (contract).
        let reloaded = load_workflow_union(Some(dir.path()), &record.overlay_workflows, &file.id)
            .expect("reloads")
            .expect("one file");
        assert_eq!(
            reloaded, file,
            "returned WorkflowFile must equal what the union read path serves"
        );

        // Enabled on the record.
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

    /// A second create with the same id collides against the record's overlay.
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
            Some(dir.path()),
            &store,
            None,
            valid_draft("dup", "First"),
        )
        .await
        .expect("first create");
        let err = create_company_workflow(
            &company,
            Some(dir.path()),
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
            Some(dir.path()),
            &store,
            None,
            valid_draft("one", "Greeter"),
        )
        .await
        .expect("first");
        let err = create_company_workflow(
            &company,
            Some(dir.path()),
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

        let err = create_company_workflow(&company, Some(dir.path()), &store, None, draft)
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

        let err = create_company_workflow(&company, Some(dir.path()), &store, None, draft)
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
        let err = create_company_workflow(&company, Some(dir.path()), &store, None, zero)
            .await
            .expect_err("no trigger");
        assert!(err.to_string().contains("exactly one `trigger`"), "{err}");

        // Two triggers.
        let mut two = valid_draft("t", "T");
        two.nodes[2].kind = "trigger".to_string();
        let err = create_company_workflow(&company, Some(dir.path()), &store, None, two)
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
            Some(dir.path()),
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
                schedule: None,
                config: None,
                on_error: None,
                retry: None,
                requires_approval: None,
                destination: None,
            });
        }
        assert!(draft.nodes.len() > MAX_WORKFLOW_NODES);
        let err = create_company_workflow(&company, Some(dir.path()), &store, None, draft)
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
        let err = create_company_workflow(&company, Some(dir.path()), &store, None, draft)
            .await
            .expect_err("too many bytes");
        assert!(err.to_string().contains("byte"), "{err}");
    }

    /// The body and the enabled id land in ONE save, so a failing save leaves
    /// the record exactly as it was — no orphaned body, no orphaned enabled id,
    /// nothing to roll back. (Before #168 this needed a file-removal dance.)
    #[tokio::test]
    async fn store_save_failure_leaves_the_record_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::failing(record(
            &company,
            manifest_with_assistant(),
        )));

        let err = create_company_workflow(
            &company,
            Some(dir.path()),
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

        let record = store.load(&company).await.unwrap().unwrap();
        assert!(record.overlay_workflows.is_empty(), "no orphaned body");
        assert!(
            record.manifest.workflows.enabled.is_empty(),
            "no orphaned enabled id"
        );
        assert!(
            !dir.path().join("workflows").join("rollback.toml").exists(),
            "nothing was written to the source tree"
        );
    }

    // --- #168: no source directory at all (the hosted case) ------------------

    /// The direct #168 regression at the core level: a hosted tenant has no
    /// source directory (its crate mount is read-only), and creation must still
    /// succeed by persisting the body on the record.
    #[tokio::test]
    async fn creates_with_no_source_dir() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));

        let file = create_company_workflow(
            &company,
            None,
            &store,
            None,
            valid_draft("hosted", "Hosted"),
        )
        .await
        .expect("creates with no source dir");
        assert_eq!(file.id, "hosted");

        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1);
        assert_eq!(record.overlay_workflows[0].id, "hosted");
        assert!(
            record
                .manifest
                .workflows
                .enabled
                .contains(&"hosted".to_string())
        );
        // And it reads back as a full graph through the union path.
        let loaded = load_workflow_union(None, &record.overlay_workflows, "hosted")
            .expect("loads")
            .expect("present");
        assert_eq!(loaded, file);
    }

    /// An id already taken by a *seed* file is a 409 even though the new body
    /// would live somewhere else entirely — the seed would shadow it on read.
    #[tokio::test]
    async fn id_colliding_with_a_seed_file_is_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("seeded.toml"), SEED_TOML).unwrap();

        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let err = create_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            None,
            valid_draft("seeded", "Different name"),
        )
        .await
        .expect_err("id is taken by a seed file");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
        assert!(err.to_string().contains("seeded"), "{err}");
    }

    /// A *name* already used by a seed file collides too — the picker would show
    /// two indistinguishable entries.
    #[tokio::test]
    async fn name_colliding_with_a_seed_file_is_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("seeded.toml"), SEED_TOML).unwrap();

        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let err = create_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            None,
            valid_draft("other", "  seeded FLOW  "),
        )
        .await
        .expect_err("name collides with the seed file's name");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
    }

    /// With no source tree at all, the name guard still works — it degrades to
    /// overlay ∪ enabled rather than erroring or silently allowing duplicates.
    #[tokio::test]
    async fn name_guard_works_without_a_source_tree() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));

        create_company_workflow(&company, None, &store, None, valid_draft("one", "Greeter"))
            .await
            .expect("first");
        let err =
            create_company_workflow(&company, None, &store, None, valid_draft("two", "GREETER"))
                .await
                .expect_err("name collides with the overlay body");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
    }

    /// A manifest-`enabled` id with no body in either source is shown by the
    /// picker under its id, so a new workflow can't take that name either.
    #[tokio::test]
    async fn name_collides_with_a_bodiless_enabled_id() {
        let company = CompanyId::new("acme");
        let mut rec = record(&company, manifest_with_assistant());
        rec.manifest.workflows.enabled.push("legacy".to_string());
        let store = store_of(MemStore::seeded(rec));

        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            valid_draft("new", "  LEGACY  "),
        )
        .await
        .expect_err("name collides with the enabled-id fallback name");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
    }

    #[tokio::test]
    async fn no_company_record_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("ghost");
        let store: Arc<dyn CompanyStore> = Arc::new(MemStore::default());
        let err = create_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            None,
            valid_draft("wf", "WF"),
        )
        .await
        .expect_err("no record");
        assert!(
            matches!(err, OpenCompanyError::CompanyNotFound(_)),
            "{err:?}"
        );
    }

    // --- #259: update ------------------------------------------------------

    /// Seeds a company with one created workflow and hands back the store plus
    /// the version token a `GET` would have returned for it.
    async fn with_one_workflow(
        company: &CompanyId,
        id: &str,
        name: &str,
    ) -> (Arc<dyn CompanyStore>, String) {
        let store = store_of(MemStore::seeded(record(company, manifest_with_assistant())));
        create_company_workflow(company, None, &store, None, valid_draft(id, name))
            .await
            .expect("seed create");
        let record = store.load(company).await.unwrap().unwrap();
        let version = workflow_version(&record.overlay_workflows[0].toml);
        (store, version)
    }

    #[tokio::test]
    async fn updates_the_body_in_place_and_journals() {
        let company = CompanyId::new("acme");
        let (store, version) = with_one_workflow(&company, "greeter", "Greeter").await;
        let log = Arc::new(MemLog::default());
        let log_dyn: Arc<dyn EventLog> = log.clone();

        let mut draft = valid_draft("greeter", "Greeter");
        draft.nodes[0].schedule = Some("0 9 * * *".to_string());
        draft.description = Some("Now on a cron.".to_string());

        let file = update_company_workflow(
            &company,
            None,
            &store,
            Some(&log_dyn),
            draft,
            Some(&version),
        )
        .await
        .expect("updates");
        assert_eq!(file.nodes[0].schedule.as_deref(), Some("0 9 * * *"));

        let record = store.load(&company).await.unwrap().unwrap();
        // Replaced, not appended — an edit must never fork the graph in two.
        assert_eq!(record.overlay_workflows.len(), 1);
        assert_eq!(record.overlay_workflows[0].id, "greeter");
        // Still enabled: an edit leaves the arming decision alone.
        assert_eq!(record.manifest.workflows.enabled, vec!["greeter"]);

        // What the union read path serves is what we returned.
        let reloaded = load_workflow_union(None, &record.overlay_workflows, "greeter")
            .expect("reloads")
            .expect("present");
        assert_eq!(reloaded, file);

        let events = log.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            CompanyEvent::WorkflowUpdated {
                workflow_id, name, ..
            } => {
                assert_eq!(workflow_id, "greeter");
                assert_eq!(name, "Greeter");
            }
            other => panic!("expected WorkflowUpdated, got {other:?}"),
        }
    }

    /// The version token is what makes concurrent edits safe. A caller holding a
    /// token from before someone else's write must be refused, not silently win.
    #[tokio::test]
    async fn a_stale_version_is_refused_and_changes_nothing() {
        let company = CompanyId::new("acme");
        let (store, stale) = with_one_workflow(&company, "greeter", "Greeter").await;

        // Someone else edits first, unconditionally.
        let mut theirs = valid_draft("greeter", "Greeter");
        theirs.description = Some("Theirs landed first.".to_string());
        update_company_workflow(&company, None, &store, None, theirs, None)
            .await
            .expect("first writer wins");

        // Our stale token is now wrong.
        let mut ours = valid_draft("greeter", "Greeter");
        ours.description = Some("Ours would clobber.".to_string());
        let err = update_company_workflow(&company, None, &store, None, ours, Some(&stale))
            .await
            .expect_err("stale version must be refused");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");

        // And the other writer's edit is intact — the refusal is not partial.
        let record = store.load(&company).await.unwrap().unwrap();
        let current = load_workflow_union(None, &record.overlay_workflows, "greeter")
            .unwrap()
            .unwrap();
        assert_eq!(current.description.as_deref(), Some("Theirs landed first."));
    }

    /// The fresh token from the *previous* write is accepted, so the
    /// reload-and-retry loop the console offers actually terminates.
    #[tokio::test]
    async fn a_fresh_version_is_accepted() {
        let company = CompanyId::new("acme");
        let (store, first) = with_one_workflow(&company, "greeter", "Greeter").await;

        let mut once = valid_draft("greeter", "Greeter");
        once.description = Some("One.".to_string());
        update_company_workflow(&company, None, &store, None, once, Some(&first))
            .await
            .expect("first conditional write");

        let record = store.load(&company).await.unwrap().unwrap();
        let second = workflow_version(&record.overlay_workflows[0].toml);
        assert_ne!(second, first, "the token must move when the body does");

        let mut twice = valid_draft("greeter", "Greeter");
        twice.description = Some("Two.".to_string());
        update_company_workflow(&company, None, &store, None, twice, Some(&second))
            .await
            .expect("refreshed token is accepted");
    }

    /// No token at all is an unconditional write — the `curl` contract.
    #[tokio::test]
    async fn no_version_is_an_unconditional_write() {
        let company = CompanyId::new("acme");
        let (store, _) = with_one_workflow(&company, "greeter", "Greeter").await;
        let mut draft = valid_draft("greeter", "Greeter");
        draft.description = Some("No token needed.".to_string());
        update_company_workflow(&company, None, &store, None, draft, None)
            .await
            .expect("unconditional write");
    }

    /// Re-saving without renaming must not collide with the workflow's own name.
    #[tokio::test]
    async fn keeping_the_same_name_is_not_a_self_conflict() {
        let company = CompanyId::new("acme");
        let (store, version) = with_one_workflow(&company, "greeter", "Greeter").await;
        let mut draft = valid_draft("greeter", "  greeter  ");
        draft.description = Some("Same name, different case and padding.".to_string());
        update_company_workflow(&company, None, &store, None, draft, Some(&version))
            .await
            .expect("own name must not conflict with itself");
    }

    /// …but a *sibling's* name is still guarded.
    #[tokio::test]
    async fn taking_another_workflows_name_is_a_conflict() {
        let company = CompanyId::new("acme");
        let (store, _) = with_one_workflow(&company, "greeter", "Greeter").await;
        create_company_workflow(&company, None, &store, None, valid_draft("other", "Other"))
            .await
            .expect("second workflow");

        let err = update_company_workflow(
            &company,
            None,
            &store,
            None,
            valid_draft("greeter", "OTHER"),
            None,
        )
        .await
        .expect_err("sibling name is taken");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
    }

    /// An edit runs the same shape validation a create does.
    #[tokio::test]
    async fn a_bad_edit_is_refused_on_the_same_terms_as_a_bad_create() {
        let company = CompanyId::new("acme");
        let (store, _) = with_one_workflow(&company, "greeter", "Greeter").await;

        // Zero triggers.
        let mut no_trigger = valid_draft("greeter", "Greeter");
        no_trigger.nodes[0].kind = "output".to_string();
        let err = update_company_workflow(&company, None, &store, None, no_trigger, None)
            .await
            .expect_err("no trigger");
        assert!(err.to_string().contains("exactly one `trigger`"), "{err}");

        // Off-roster teammate.
        let mut ghost = valid_draft("greeter", "Greeter");
        ghost.nodes[1].agent = Some("ghost".to_string());
        let err = update_company_workflow(&company, None, &store, None, ghost, None)
            .await
            .expect_err("off-roster teammate");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );

        // And nothing was persisted by either attempt.
        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1);
        let current = load_workflow_union(None, &record.overlay_workflows, "greeter")
            .unwrap()
            .unwrap();
        assert_eq!(current.nodes.len(), 3);
    }

    /// **The core overlay-only rule for update.** A seed-backed id is refused,
    /// because `load_workflow_union` gives the seed file precedence — persisting
    /// the edit would store a graph the read path never serves.
    #[tokio::test]
    async fn updating_a_seed_backed_workflow_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("seeded.toml"), SEED_TOML).unwrap();

        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));

        let err = update_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            None,
            valid_draft("seeded", "Seeded flow"),
            None,
        )
        .await
        .expect_err("a source-defined workflow is not editable");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
        assert!(err.to_string().contains("source tree"), "{err}");
    }

    #[tokio::test]
    async fn updating_an_unknown_workflow_is_not_found() {
        let company = CompanyId::new("acme");
        let (store, _) = with_one_workflow(&company, "greeter", "Greeter").await;
        let err = update_company_workflow(
            &company,
            None,
            &store,
            None,
            valid_draft("ghost", "Ghost"),
            None,
        )
        .await
        .expect_err("unknown id");
        assert!(
            matches!(err, OpenCompanyError::CompanyNotFound(_)),
            "{err:?}"
        );
    }

    /// A manifest-`enabled` id with no body in either source has nothing to
    /// replace — a 409 that says so beats a 404 that implies it never existed.
    #[tokio::test]
    async fn updating_a_bodiless_enabled_id_is_a_conflict() {
        let company = CompanyId::new("acme");
        let mut rec = record(&company, manifest_with_assistant());
        rec.manifest.workflows.enabled.push("legacy".to_string());
        let store = store_of(MemStore::seeded(rec));

        let err = update_company_workflow(
            &company,
            None,
            &store,
            None,
            valid_draft("legacy", "Legacy"),
            None,
        )
        .await
        .expect_err("no body to replace");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
    }

    /// An edit must not reshuffle the picker.
    #[tokio::test]
    async fn an_edit_preserves_overlay_order() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        for (id, name) in [("a", "Alpha"), ("b", "Bravo"), ("c", "Charlie")] {
            create_company_workflow(&company, None, &store, None, valid_draft(id, name))
                .await
                .expect("seed");
        }

        let mut draft = valid_draft("a", "Alpha");
        draft.description = Some("Edited.".to_string());
        update_company_workflow(&company, None, &store, None, draft, None)
            .await
            .expect("edit the first");

        let record = store.load(&company).await.unwrap().unwrap();
        let ids: Vec<&str> = record
            .overlay_workflows
            .iter()
            .map(|w| w.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"], "an edit must not reorder");
    }

    // --- #259: delete ------------------------------------------------------

    #[tokio::test]
    async fn deletes_the_body_and_the_enabled_id_and_journals() {
        let company = CompanyId::new("acme");
        let (store, version) = with_one_workflow(&company, "greeter", "Greeter").await;
        let log = Arc::new(MemLog::default());
        let log_dyn: Arc<dyn EventLog> = log.clone();

        let name = delete_company_workflow(
            &company,
            None,
            &store,
            Some(&log_dyn),
            "greeter",
            Some(&version),
        )
        .await
        .expect("deletes");
        assert_eq!(name, "Greeter");

        let record = store.load(&company).await.unwrap().unwrap();
        // BOTH halves gone, in one save. Either alone would leave a workflow
        // that is half-present: a listed id with no graph, or a graph the
        // scheduler still fires.
        assert!(record.overlay_workflows.is_empty(), "body must be gone");
        assert!(
            record.manifest.workflows.enabled.is_empty(),
            "enabled id must be gone"
        );
        assert!(
            load_workflow_union(None, &record.overlay_workflows, "greeter")
                .unwrap()
                .is_none(),
            "the union read path must no longer serve it"
        );

        let events = log.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            CompanyEvent::WorkflowDeleted {
                workflow_id, name, ..
            } => {
                assert_eq!(workflow_id, "greeter");
                assert_eq!(name, "Greeter");
            }
            other => panic!("expected WorkflowDeleted, got {other:?}"),
        }
    }

    /// The delete is durable across the #208 boot rebuild *because* the overlay
    /// body is gone: `merge_enabled_workflows` re-derives `enabled` from seed
    /// ids ∪ surviving overlay ids, so there is nothing left to resurrect. This
    /// pins the invariant the delete's correctness rests on.
    #[tokio::test]
    async fn a_deleted_workflow_has_nothing_left_for_the_boot_merge_to_re_enable() {
        let company = CompanyId::new("acme");
        let (store, _) = with_one_workflow(&company, "greeter", "Greeter").await;
        delete_company_workflow(&company, None, &store, None, "greeter", None)
            .await
            .expect("deletes");

        let record = store.load(&company).await.unwrap().unwrap();
        let surviving: Vec<&str> = record
            .overlay_workflows
            .iter()
            .map(|w| w.id.as_str())
            .collect();
        assert!(
            !surviving.contains(&"greeter"),
            "no overlay body means the boot merge cannot re-enable it"
        );
        assert!(list_workflows_union(None, &record.overlay_workflows).is_empty());
    }

    #[tokio::test]
    async fn deleting_with_a_stale_version_is_refused_and_keeps_the_workflow() {
        let company = CompanyId::new("acme");
        let (store, stale) = with_one_workflow(&company, "greeter", "Greeter").await;

        let mut theirs = valid_draft("greeter", "Greeter");
        theirs.description = Some("Edited after you loaded it.".to_string());
        update_company_workflow(&company, None, &store, None, theirs, None)
            .await
            .expect("someone edits first");

        let err = delete_company_workflow(&company, None, &store, None, "greeter", Some(&stale))
            .await
            .expect_err("stale delete must be refused");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");

        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1, "nothing was removed");
    }

    /// Deleting a source-defined workflow is refused: `merge_enabled_workflows`
    /// would re-enable it from the seed id on the next boot, so the console
    /// would be promising a removal it cannot keep.
    #[tokio::test]
    async fn deleting_a_seed_backed_workflow_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("seeded.toml"), SEED_TOML).unwrap();

        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));

        let err = delete_company_workflow(&company, Some(dir.path()), &store, None, "seeded", None)
            .await
            .expect_err("a source-defined workflow is not deletable");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
        assert!(err.to_string().contains("source tree"), "{err}");
        // And the seed file is untouched — this path never writes to the tree.
        assert!(workflows.join("seeded.toml").is_file());
    }

    #[tokio::test]
    async fn deleting_an_unknown_workflow_is_not_found() {
        let company = CompanyId::new("acme");
        let (store, _) = with_one_workflow(&company, "greeter", "Greeter").await;
        let err = delete_company_workflow(&company, None, &store, None, "ghost", None)
            .await
            .expect_err("unknown id");
        assert!(
            matches!(err, OpenCompanyError::CompanyNotFound(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn deleting_a_traversal_id_is_invalid() {
        let company = CompanyId::new("acme");
        let (store, _) = with_one_workflow(&company, "greeter", "Greeter").await;
        let err = delete_company_workflow(&company, None, &store, None, "../secrets", None)
            .await
            .expect_err("traversal id");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
    }

    /// Only the named workflow goes; siblings keep their bodies and their
    /// enabled ids.
    #[tokio::test]
    async fn deleting_one_leaves_the_others_alone() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        for (id, name) in [("a", "Alpha"), ("b", "Bravo"), ("c", "Charlie")] {
            create_company_workflow(&company, None, &store, None, valid_draft(id, name))
                .await
                .expect("seed");
        }

        delete_company_workflow(&company, None, &store, None, "b", None)
            .await
            .expect("deletes the middle one");

        let record = store.load(&company).await.unwrap().unwrap();
        let ids: Vec<&str> = record
            .overlay_workflows
            .iter()
            .map(|w| w.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "c"]);
        assert_eq!(record.manifest.workflows.enabled, vec!["a", "c"]);
    }

    /// A save failure leaves the record exactly as it was — the same one-save
    /// atomicity create relies on.
    #[tokio::test]
    async fn a_failing_save_leaves_the_workflow_in_place() {
        let company = CompanyId::new("acme");
        let mut rec = record(&company, manifest_with_assistant());
        rec.overlay_workflows.push(OverlayWorkflow {
            id: "greeter".to_string(),
            toml: render_workflow(&valid_draft("greeter", "Greeter")).unwrap(),
        });
        rec.manifest.workflows.enabled.push("greeter".to_string());
        let store = store_of(MemStore::failing(rec));

        delete_company_workflow(&company, None, &store, None, "greeter", None)
            .await
            .expect_err("save fails");

        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1, "nothing was removed");
        assert_eq!(record.manifest.workflows.enabled, vec!["greeter"]);
    }

    // --- #259: the version token itself ------------------------------------

    #[test]
    fn the_version_token_is_stable_and_body_derived() {
        let a = workflow_version("id = \"x\"\n");
        assert_eq!(a, workflow_version("id = \"x\"\n"), "must be deterministic");
        assert_ne!(
            a,
            workflow_version("id = \"y\"\n"),
            "a different body must produce a different token"
        );
        // Hex sha256: 64 lowercase hex characters, so it is safe in a URL query
        // without escaping (the DELETE route passes it as `?expectedVersion=`).
        assert_eq!(a.len(), 64, "{a}");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "{a}"
        );
    }
}
