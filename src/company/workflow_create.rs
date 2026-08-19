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
//! * **Revision history lives in its own store (issue #274).** OpenHuman keeps a
//!   bounded snapshot ring in a dedicated `flow_revisions` table; our overlay
//!   bodies live inside `CompanyRecord`, which is loaded and saved *whole* on
//!   every write, so a ring per workflow would bloat that hot path. It therefore
//!   got its own [`WorkflowRevisionStore`](crate::ports::WorkflowRevisionStore)
//!   port plus three backends rather than a field on the record.
//!   [`update_company_workflow`] captures the prior body into it under the write
//!   lock, and [`rollback_company_workflow`] restores one *through this same
//!   update path* — so a rollback re-validates against the current record and is
//!   itself undoable. Diffing/merging revisions stays out of scope.
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
//!
//! # Arming, and the disarm rule (issue #276)
//!
//! A workflow's armed state is
//! [`CompanyRecord::disabled_workflows`](crate::ports::types::CompanyRecord::disabled_workflows),
//! read by [`WorkflowScheduler::tick`](crate::runtime::WorkflowScheduler) and by
//! nothing else that decides whether work happens. Three write paths touch it,
//! and **two of them can only ever disarm**:
//!
//! | Path | Writes |
//! | --- | --- |
//! | [`create_company_workflow`] | `false`, when the new graph carries a trigger schedule |
//! | [`update_company_workflow`] | `false`, when the edit adds a schedule to a graph that had none |
//! | [`set_company_workflow_enabled`] | whatever the operator asked for |
//!
//! **A schedule is armed only by a person saying so.** That is the rule, and it
//! is one-directional on purpose: an edit that *removes* a schedule does not
//! re-arm anything, re-saving an already-scheduled graph does not re-arm it, and
//! neither does deleting and recreating around it. A rule that could arm would
//! be a rule that could arm by accident.
//!
//! This is OpenHuman's "B29 Rule 1" — its `flows_update` forces `enabled =
//! false` when an edit turns a manual or absent trigger into an automatic one,
//! after a flow of its own started running on an unreviewed 8am schedule — with
//! one deliberate widening. **Create is covered too.** OpenHuman disarms only on
//! edit; here [`create_company_workflow`] is *also* the orchestrator's
//! `create_workflow` tool, so leaving create armed would mean an agent can put a
//! cron into production by authoring one, and an operator who wanted around the
//! rule would only have to write the graph fresh instead of editing it. Issue
//! #276 says it directly: a rule that does not cover create and update together
//! just moves the hole.
//!
//! **Changing an existing cron does not disarm.** `0 8 * * *` → `0 3 * * *` on
//! an already-armed workflow stays armed. The operator accepted automatic firing
//! for this workflow and is now correcting *when*; disarming there would put a
//! re-enable click behind every typo fix, which is how an operator learns to
//! click through the re-arm without reading it. The decision that was reviewed
//! is "automatic at all", and that one has not changed.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::company::{
    RawEdge, RawNode, RawWorkflow, WorkflowDestinationDef, WorkflowFile, WorkflowNodeKind,
    list_workflows_union, parse_workflow, raw_workflow_from_toml, render_workflow,
    required_config_problems,
};
use crate::error::{OpenCompanyError, Result};
use crate::ports::events::EventLog;
use crate::ports::now_millis;
use crate::ports::store::company_write_lock;
use crate::ports::types::{
    CompanyEvent, CompanyId, CompanyRecord, OverlayWorkflow, WorkflowEnabledReason,
};
use crate::ports::workflow_revisions::{WorkflowRevisionRecord, WorkflowRevisionStore};
use crate::ports::{CompanyStore, ScheduleFireStore};
use crate::runtime::workflow_schedule_id;
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

    // Cross-check every `agent` node against the company's effective roster and
    // every `tool_call` node against the company's wired+granted tools.
    // `parse_workflow` checks the graph's own shape but has no record to validate
    // names against — the same helper gates create and update identically.
    validate_draft_against_record(&draft, &record)?;

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
    // Issue #276: a graph authored with a cron lands **switched off**. It is
    // saved, listed and runnable by hand; it just does not fire until someone
    // arms it. Written in the same save as the body and the enabled id, so a
    // freshly created schedule is never briefly live — there is no window
    // between "the scheduler can see this" and "the scheduler is told not to run
    // it", because a tick reads one record or the other, never a half of both.
    //
    // Note which surface this binds hardest: this function is also the
    // orchestrator's `create_workflow` tool, so an agent cannot arm a cron.
    let disarmed = file.trigger_schedule().is_some();
    if disarmed {
        record.set_workflow_enabled(&file.id, false);
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

    // Issue #276: say so, and say it was the rule rather than a person. A
    // scheduled workflow that never fires is otherwise indistinguishable from a
    // broken one, and this is the line that tells an operator to go arm it.
    if disarmed {
        journal_enabled_change(
            company,
            events,
            &file.id,
            &file.name,
            false,
            WorkflowEnabledReason::Disarmed,
        )
        .await;
        tracing::info!(
            company = %company,
            workflow = %file.id,
            "workflow created with a schedule and left switched off pending review"
        );
    }

    Ok(file)
}

// ---------------------------------------------------------------------------
// The workflow proposal's authoring payload (issue #580)
// ---------------------------------------------------------------------------

/// The `{id, name, description, nodes, edges}` graph a
/// [`TaskWorkflowProposal`](crate::ports::tasks::TaskWorkflowProposal) stores as
/// its `ops` — the same shape `POST …/workflows` accepts, but owned by the
/// company layer so the harness builder (which *produces* a proposal) and the
/// apply route (which *persists* it) rebuild a [`RawWorkflow`] from it the SAME
/// way.
///
/// **The host is the authority.** A proposal never stores a rendered graph; it
/// stores this payload, and apply re-derives and re-validates a `RawWorkflow`
/// from it through [`create_company_workflow`]. So a stored proposal is *input*
/// to the create checks, never a substitute for them — a graph cannot reach the
/// workflow list without passing exactly the validation a hand-authored one does.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowGraphSpec {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) nodes: Vec<WorkflowNodeSpec>,
    #[serde(default)]
    pub(crate) edges: Vec<WorkflowEdgeSpec>,
}

/// One node of a [`WorkflowGraphSpec`]. Mirrors the create route's node body —
/// the subset a builder pass produces (`trigger`, `agent`, `tool_call`,
/// `condition`, `output`). `on_error`/`retry` are omitted because the builder
/// does not author them; they convert to `None` on a [`RawNode`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowNodeSpec {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) schedule: Option<String>,
    /// Free-form engine config as JSON (a `tool_call`'s `slug`, a condition's
    /// expression). Converted to a TOML value on the way into [`RawNode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) requires_approval: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) destination: Option<WorkflowDestinationDef>,
}

/// One edge of a [`WorkflowGraphSpec`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowEdgeSpec {
    #[serde(default)]
    pub(crate) from: String,
    #[serde(default)]
    pub(crate) to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
}

/// Rebuilds a [`RawWorkflow`] from a stored proposal graph — the one conversion
/// both the builder pass and the apply route use, so what the builder validated
/// and what apply persists are the same graph.
///
/// The only fallible step is `config`: TOML (the node config's storage form) has
/// no `null`, so a JSON config carrying one is refused here with an actionable
/// message rather than silently dropped — the same rule the create route's own
/// body conversion applies.
pub(crate) fn raw_workflow_from_spec(spec: &WorkflowGraphSpec) -> Result<RawWorkflow> {
    let mut nodes = Vec::with_capacity(spec.nodes.len());
    for n in &spec.nodes {
        let config = match &n.config {
            Some(json) => Some(toml::Value::try_from(json).map_err(|err| {
                OpenCompanyError::InvalidRequest(format!(
                    "node `{}` has config that can't be stored ({err}) — TOML has no null; drop \
                     null-valued keys.",
                    n.id
                ))
            })?),
            None => None,
        };
        nodes.push(RawNode {
            id: n.id.clone(),
            kind: n.kind.clone(),
            name: n.name.clone(),
            summary: n.summary.clone(),
            agent: n.agent.clone(),
            schedule: n.schedule.clone(),
            config,
            on_error: None,
            retry: None,
            requires_approval: n.requires_approval,
            destination: n.destination.clone(),
        });
    }
    Ok(RawWorkflow {
        id: spec.id.clone(),
        name: spec.name.clone(),
        description: spec.description.clone(),
        nodes,
        edges: spec
            .edges
            .iter()
            .map(|e| RawEdge {
                from: e.from.clone(),
                to: e.to.clone(),
                label: e.label.clone(),
            })
            .collect(),
    })
}

/// The inverse of [`raw_workflow_from_spec`] at the parsed-graph layer (issue
/// #840, PR-3): rebuilds a [`WorkflowGraphSpec`] from a persisted, validated
/// [`WorkflowFile`]. The fix-from-run copilot needs the failing workflow's saved
/// graph *as a spec* — both to hand the agent as evidence and to pin its identity
/// through the correction — and the read path produces a `WorkflowFile`, so this
/// is the one conversion between them.
///
/// `on_error`/`retry` have no field on [`WorkflowNodeSpec`] (the builder never
/// authors them), so they are dropped: the copilot re-proposes the graph's nodes,
/// and the host-owned id/name are what the fix path pins, not the engine policy.
///
/// Gated with the create-time copilot that is its only caller, the same footing
/// as [`courtesy_validate_draft`] and [`workflow_graph_from_spec`].
#[cfg(feature = "openhuman")]
pub(crate) fn workflow_spec_from_graph(file: WorkflowFile) -> WorkflowGraphSpec {
    WorkflowGraphSpec {
        id: file.id,
        name: file.name,
        description: file.description,
        nodes: file
            .nodes
            .into_iter()
            .map(|n| WorkflowNodeSpec {
                id: n.id,
                kind: n.kind.as_str().to_string(),
                name: n.name,
                summary: n.summary,
                agent: n.agent,
                schedule: n.schedule,
                config: n.config,
                requires_approval: n.requires_approval,
                destination: n.destination,
            })
            .collect(),
        edges: file
            .edges
            .into_iter()
            .map(|e| WorkflowEdgeSpec {
                from: e.from,
                to: e.to,
                label: e.label,
            })
            .collect(),
    }
}

/// Runs the full author-time validation on a candidate graph **without
/// persisting it** — the builder pass's courtesy check (issue #580), so a
/// proposal that could never be created never reaches In Review.
///
/// It runs exactly the checks [`create_company_workflow`] runs before its save —
/// shape (id/name/size/one-trigger), the render → byte-cap → `parse_workflow`
/// round trip, and the roster/tool cross-check against the loaded `record` — and
/// then throws the result away. The one thing it deliberately does **not** check
/// is id/name uniqueness, because that is a function of the live record at
/// *apply* time, not build time: a name free when the proposal was built can be
/// taken by the time it is approved, and that is the roster-drift case apply
/// surfaces by keeping the card In Review.
///
/// Gated with the builder it serves: the only caller is
/// `crate::harness::workflow_build`, so in the default build (no harness) this
/// would be dead code.
#[cfg(feature = "openhuman")]
pub(crate) fn courtesy_validate_draft(draft: &RawWorkflow, record: &CompanyRecord) -> Result<()> {
    validate_draft_shape(draft)?;
    let toml_src = render_workflow(draft)?;
    if toml_src.len() > MAX_WORKFLOW_TOML_BYTES {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "the proposed workflow is {} bytes, over the {MAX_WORKFLOW_TOML_BYTES}-byte limit.",
            toml_src.len()
        )));
    }
    parse_workflow(&toml_src).map_err(|err| match err {
        OpenCompanyError::DataInvalid { problems, .. } => {
            OpenCompanyError::InvalidRequest(problems.join(" "))
        }
        OpenCompanyError::DataParse { message, .. } => OpenCompanyError::InvalidRequest(message),
        other => other,
    })?;
    validate_draft_against_record(draft, record)?;
    Ok(())
}

/// Lowers a copilot draft [`WorkflowGraphSpec`] into the tinyflows
/// [`WorkflowGraph`](tinyflows::model::WorkflowGraph) the run-time gates read,
/// through the SAME `RawWorkflow → render → parse → translate` pipeline the
/// create path uses (issue #840). It is the seam the create-time copilot's
/// `check_workflow` tool runs [`tinyflows::gates::failures`] over, so a draft is
/// checked against exactly the graph the engine would compile — not a second,
/// drifting translation.
///
/// Fallible on the render/parse half: a spec whose kind or shape `parse_workflow`
/// refuses cannot be translated, and the error is mapped to an actionable
/// [`InvalidRequest`](OpenCompanyError::InvalidRequest) the same way
/// [`courtesy_validate_draft`] maps it — so the tool hands the model one honest
/// sentence rather than a 500.
///
/// Gated with the copilot it serves (its only caller is
/// `crate::harness::workflow_build`), so it is not dead code in the default build.
#[cfg(feature = "openhuman")]
pub(crate) fn workflow_graph_from_spec(
    spec: &WorkflowGraphSpec,
) -> Result<tinyflows::model::WorkflowGraph> {
    let raw = raw_workflow_from_spec(spec)?;
    let toml_src = render_workflow(&raw)?;
    let file = parse_workflow(&toml_src).map_err(|err| match err {
        OpenCompanyError::DataInvalid { problems, .. } => {
            OpenCompanyError::InvalidRequest(problems.join(" "))
        }
        OpenCompanyError::DataParse { message, .. } => OpenCompanyError::InvalidRequest(message),
        other => other,
    })?;
    Ok(crate::workflows::translate::translate(&file))
}

/// Journals a best-effort [`WorkflowEnabledChanged`](CompanyEvent::WorkflowEnabledChanged).
///
/// Best-effort in the same sense as every other write-path audit event here: the
/// flag is already persisted by the time this runs, so a journal failure is
/// logged and never rolls the change back. Shared by the three write paths so
/// the disarm rule and the operator toggle produce the same audit shape.
async fn journal_enabled_change(
    company: &CompanyId,
    events: Option<&Arc<dyn EventLog>>,
    wid: &str,
    name: &str,
    enabled: bool,
    reason: WorkflowEnabledReason,
) {
    if let Some(log) = events
        && let Err(err) = log
            .append(
                company,
                CompanyEvent::WorkflowEnabledChanged {
                    workflow_id: wid.to_string(),
                    name: name.to_string(),
                    enabled,
                    reason,
                    by: None,
                },
            )
            .await
    {
        tracing::warn!(
            company = %company,
            workflow = %wid,
            error = %err,
            "workflow enablement changed but audit journal append failed"
        );
    }
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

/// Cross-checks a draft graph against the company `record`: every `agent` node
/// names a real roster teammate (manifest agents ∪ operator overlay teammates),
/// and every `tool_call` node names a wired, granted tool. Shared verbatim by
/// [`create_company_workflow`] and [`update_company_workflow`] so a bad draft is
/// refused on exactly the same terms whichever surface authored it, and runs
/// **inside the write lock** because the roster and grants both come from the
/// loaded record.
///
/// This is an author-time convenience, not the enforcement point. The run-time
/// gate in [`WorkflowToolInvoker::invoke`](crate::workflows::caps) stays the
/// backstop: `[tools].allow` grants can be revoked after a graph is saved, and
/// seed / legacy graphs never pass through this create path at all — so passing
/// here means the draft was coherent when written, not that a run is forever
/// permitted.
fn validate_draft_against_record(draft: &RawWorkflow, record: &CompanyRecord) -> Result<()> {
    let roster: HashSet<&str> = record
        .manifest
        .agents
        .iter()
        .map(|a| a.id.as_str())
        .chain(record.overlay_agents.iter().map(|a| a.id.as_str()))
        .collect();

    for node in &draft.nodes {
        match node.kind.as_str() {
            "agent" => match node.agent.as_deref() {
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
            },
            "tool_call" => validate_tool_call_node(node, record)?,
            // Per-kind required config (issue #661): reject a `condition` with no
            // `field`, an `http_request` missing `method`/`url`, or a `switch`
            // with no discriminant at author time — the same gate the on-disk
            // `validate` applies, surfaced here as a 400 so the console/builder
            // draft path never persists a graph whose runtime behaviour is
            // silently wrong. `tool_call` keeps its richer `validate_tool_call_node`
            // (slug + namespace/grant); the structural kinds share the helper.
            "condition" | "http_request" | "switch" => {
                let kind = match node.kind.as_str() {
                    "condition" => WorkflowNodeKind::Condition,
                    "http_request" => WorkflowNodeKind::HttpRequest,
                    _ => WorkflowNodeKind::Switch,
                };
                // Report EVERY missing-config problem for the node, not just the
                // first: an `http_request` missing both `method` AND `url` should
                // name both, since the draft path is where a human/model iterates.
                let problems = required_config_problems(
                    kind,
                    &format!("node `{}`", node.id),
                    node.config.as_ref(),
                );
                if !problems.is_empty() {
                    return Err(OpenCompanyError::InvalidRequest(problems.join(" ")));
                }
            }
            _ => {}
        }
    }

    // Condition branch labels must read `yes`/`no` at author time (issue #661).
    // `parse_workflow` is now LENIENT on this rule (issue #682) so pre-#661 saved
    // graphs still load, which means ALL author-time strictness for it has to
    // live here — mirroring the on-disk `validate` strict rule. The sole
    // exception is the `error` recovery edge of a condition that is also
    // `on_error = "route"`, whose routing is validated separately. The label is
    // lowercased + trimmed before matching (it is compared, never persisted as a
    // lookup key), matching the load rule's asymmetry vs the verbatim `slug`.
    let condition_ids: HashSet<&str> = draft
        .nodes
        .iter()
        .filter(|node| node.kind == "condition" && !node.id.trim().is_empty())
        .map(|node| node.id.as_str())
        .collect();
    let route_ids: HashSet<&str> = draft
        .nodes
        .iter()
        .filter(|node| node.on_error.as_deref() == Some("route") && !node.id.trim().is_empty())
        .map(|node| node.id.as_str())
        .collect();
    for edge in &draft.edges {
        if !condition_ids.contains(edge.from.as_str()) {
            continue;
        }
        let is_route_error =
            edge.label.as_deref() == Some("error") && route_ids.contains(edge.from.as_str());
        let is_yes_no = edge
            .label
            .as_deref()
            .map(|label| label.trim().to_ascii_lowercase())
            .is_some_and(|label| matches!(label.as_str(), "yes" | "no"));
        if !is_route_error && !is_yes_no {
            let shown = edge
                .label
                .as_deref()
                .map(|label| format!("`{label}`"))
                .unwrap_or_else(|| "no label".to_string());
            return Err(OpenCompanyError::InvalidRequest(format!(
                "an edge leaves condition node `{}` with {shown} — a condition's branches must be labeled `yes` or `no`.",
                edge.from
            )));
        }
    }

    Ok(())
}

/// Author-time `tool_call` check: the slug must be a non-empty `config.slug`
/// string, and — under the `openhuman` feature — it must name a wired toolbelt
/// namespace the company's `[tools].allow` actually grants. These are the same
/// two gates [`WorkflowToolInvoker::invoke`](crate::workflows::caps) applies at
/// run time (`namespace_of` for "is it a wired tool", then the grant-glob rule
/// with the priced `search` family requiring an explicit grant), surfaced at
/// save so an author hears about an unwired or ungranted slug now instead of at
/// first run.
///
/// The namespace/grant half is `cfg(feature = "openhuman")` because
/// `namespace_of` / `grants_cover` live behind that feature; the slug-presence
/// half is unconditional. Under the default build only the presence check runs
/// and the run-time gate remains the backstop — `record` is still consumed
/// (the roster check in the caller always reads it), so the helper compiles
/// warning-free with and without the feature.
fn validate_tool_call_node(node: &RawNode, record: &CompanyRecord) -> Result<()> {
    // (a) UNGATED — a tool_call must name a non-empty `slug` string in `config`.
    let raw_slug = node
        .config
        .as_ref()
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("slug"))
        .and_then(toml::Value::as_str);
    // Absent, or empty / whitespace-only, names no tool at all.
    let Some(slug) = raw_slug.filter(|slug| !slug.trim().is_empty()) else {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "node `{}` is a tool_call but names no `slug` — set `config.slug` to the tool to run.",
            node.id
        )));
    };
    // The slug is stored and looked up at run time EXACTLY as written —
    // `render_workflow` persists the raw config, and `WorkflowToolInvoker` indexes
    // tools by literal `name()`. So a padded slug like `" csv_export "` would sail
    // through a trim-normalized check here yet be persisted (and looked up) padded,
    // halting the run on the very lookup this save-time gate promised to prevent.
    // Reject the padding rather than silently trimming, so the validated string
    // and the persisted/runtime string are the same literal.
    if slug != slug.trim() {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "node `{}` has a tool_call `slug` with leading or trailing whitespace (`{slug}`) — \
             set `config.slug` to the exact tool name.",
            node.id
        )));
    }

    #[cfg(feature = "openhuman")]
    {
        // (b) The slug must map to a wired toolbelt namespace — mirroring the
        // run-time gate's "not a wired workflow tool".
        let Some(namespace) = crate::harness::toolbelt::namespace_of(slug) else {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "node `{}` calls tool `{slug}`, which is not a wired workflow tool.",
                node.id
            )));
        };
        // (b.1) The slug's namespace must be one the workflow `tool_call` invoker
        // actually wires (`WORKFLOW_TOOL_NAMESPACES` == shell/code/web/search).
        // `media` and `composio` map to a namespace but are agent-turn families the
        // invoker never builds, so a run passes the grant gate and then ALWAYS
        // misses the tool lookup ("not available in company workflows"). Reject them
        // here so this gate mirrors the run-time outcome instead of green-lighting a
        // slug that can never execute.
        if !crate::workflows::caps::WORKFLOW_TOOL_NAMESPACES.contains(&namespace) {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "node `{}` calls tool `{slug}` (namespace `{namespace}`), which workflow \
                 `tool_call` nodes cannot run — `{namespace}` is an agent-turn tool family, not a \
                 workflow tool.",
                node.id
            )));
        }
        // (c) The company's `[tools].allow` must grant that namespace. The priced
        // `search` family needs an EXPLICIT `search` grant — a `*` wildcard never
        // confers it — while every other namespace uses the ordinary grant-glob
        // intersection, exactly the split `WorkflowToolInvoker::invoke` enforces.
        let grants = &record.manifest.tools.allow;
        if !crate::workflows::caps::grants_workflow_namespace(grants, namespace) {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "node `{}` calls tool `{slug}` (namespace `{namespace}`), which this company's \
                 `[tools].allow` does not grant — grant it in `[tools].allow`.",
                node.id
            )));
        }
        // (d) Required args present (issue #813). The engine reads a `tool_call`'s
        // arguments from `config.args` (tinyflows
        // `nodes/integration/tool_call.rs`), so a known slug whose required args
        // are absent THERE runs and does nothing useful — the legal-but-empty
        // `read_workspace_state` (which cannot read a file anyway) was the case
        // that motivated this. Reject the missing args at author time, naming them
        // and what the tool is, so the console/copilot fixes it now instead of
        // shipping a dud node. Same philosophy as the #661 `required_config`
        // arm, one level down (the args sub-table, not the config root). Because
        // this is the SHARED create/update gate, a hand-author hears it at save
        // and the create-time copilot hears it via courtesy validation → one
        // corrective retry. A tool with no required args (`read_workspace_state`)
        // is unaffected here — its uselessness is handled by copilot grounding.
        if let Some(info) = crate::workflows::caps::workflow_tool_info(slug) {
            let args = node
                .config
                .as_ref()
                .and_then(toml::Value::as_table)
                .and_then(|table| table.get("args"))
                .and_then(toml::Value::as_table);
            let missing: Vec<&str> = info
                .required_args
                .iter()
                .copied()
                .filter(|arg| !tool_arg_present(args, arg))
                .collect();
            if !missing.is_empty() {
                return Err(OpenCompanyError::InvalidRequest(format!(
                    "node `{}` calls tool `{slug}` but its `config.args` is missing {} — {}.",
                    node.id,
                    missing
                        .iter()
                        .map(|arg| format!("`{arg}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    info.capability
                )));
            }
        }
    }
    #[cfg(not(feature = "openhuman"))]
    {
        // Under the default build the namespace/grant resolution is not compiled
        // in; the slug-presence check above still runs and the run-time gate
        // stays the backstop. Consume both bindings so neither warns.
        let _ = (slug, record);
    }

    Ok(())
}

/// Whether `[tools].allow` grants the namespace `info` belongs to — a catalogue
/// -shaped wrapper over
/// [`grants_workflow_namespace`](crate::workflows::caps::grants_workflow_namespace),
/// which is the rule itself and is shared with
/// [`validate_tool_call_node`] and the run-time
/// [`refusal_for`](crate::workflows::caps).
///
/// Exists only so the two grounding lists can filter
/// [`WORKFLOW_TOOL_CATALOG`](crate::workflows::caps) rows directly. Because the
/// catalogue is itself pinned to `namespace_of`, a slug passes here iff
/// validation would accept it — so what a caller is shown and what a proposed
/// `tool_call` node clears at courtesy validation cannot drift.
#[cfg(feature = "openhuman")]
fn grants_workflow_tool(
    grants: &[String],
    info: &crate::workflows::caps::WorkflowToolInfo,
) -> bool {
    crate::workflows::caps::grants_workflow_namespace(grants, info.namespace)
}

/// The tools a caller may ground a proposal on: catalogue, company grant, and
/// deployment wiring all agree (issues #753, #874). Both copilot surfaces read
/// it — the in-process create/fix builder and `GET …/workflows/tool-slugs` — so
/// neither can offer a tool the run would refuse.
///
/// `wired` is `None` when the deployment's wiring is not knowable (no harness
/// deps): the grant filter then stands alone, which is the widest honest answer
/// rather than a claim that nothing is wired.
///
/// Create validation intentionally remains **permissive** for a
/// granted-but-unwired tool so an operator may author now and wire the provider
/// later; this narrower set is grounding only, and
/// [`workflow_granted_but_unwired_tool_slugs`] names the difference so the gap
/// is reported rather than silently dropped.
///
/// Gated with the copilot it serves — the grant helpers live behind the
/// `openhuman` feature, so in the default build this would be dead code over
/// symbols that are not compiled.
#[cfg(feature = "openhuman")]
pub(crate) fn workflow_effective_tool_slugs(
    record: &CompanyRecord,
    wired: Option<&std::collections::BTreeSet<&'static str>>,
) -> Vec<String> {
    let grants = &record.manifest.tools.allow;
    crate::workflows::caps::WORKFLOW_TOOL_CATALOG
        .iter()
        .filter(|info| {
            grants_workflow_tool(grants, info)
                && wired.is_none_or(|namespaces| namespaces.contains(info.namespace))
        })
        .map(|info| info.slug.to_string())
        .collect()
}

/// The exact complement of [`workflow_effective_tool_slugs`] within the granted
/// set: tools this company holds a grant for that cannot run on **this**
/// deployment.
///
/// Reported rather than silently dropped (issue #874) so a reader can tell "this
/// company is not allowed that tool" — absent from both lists — from "allowed,
/// but nobody has configured the provider here". A copilot grounded on both
/// answers "that needs web search, which is not wired here" instead of either
/// proposing a doomed node or denying the tool exists.
///
/// Empty when `wired` is `None`: with the deployment unknowable, "which of these
/// are unwired" has no honest answer, and every granted slug stays in the
/// effective list.
#[cfg(feature = "openhuman")]
pub(crate) fn workflow_granted_but_unwired_tool_slugs(
    record: &CompanyRecord,
    wired: Option<&std::collections::BTreeSet<&'static str>>,
) -> Vec<String> {
    let grants = &record.manifest.tools.allow;
    crate::workflows::caps::WORKFLOW_TOOL_CATALOG
        .iter()
        .filter(|info| {
            grants_workflow_tool(grants, info)
                && !wired.is_none_or(|namespaces| namespaces.contains(info.namespace))
        })
        .map(|info| info.slug.to_string())
        .collect()
}

/// Whether a required `config.args` key is present and carries a usable value
/// (issue #813): a non-blank string — a `=`-expression that binds at run time
/// counts — or any non-null non-string value (a number, a non-empty array or
/// table). A blank string or an absent key is treated as missing.
#[cfg(feature = "openhuman")]
fn tool_arg_present(args: Option<&toml::Table>, key: &str) -> bool {
    match args.and_then(|table| table.get(key)) {
        Some(toml::Value::String(text)) => !text.trim().is_empty(),
        Some(toml::Value::Array(items)) => !items.is_empty(),
        Some(toml::Value::Table(table)) => !table.is_empty(),
        Some(_) => true, // integer / float / bool / datetime — presence is meaningful
        None => false,
    }
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
///
/// Issue #276 adds the disarm: if the stored graph had no trigger schedule and
/// the replacement does, the workflow is switched **off** in the same save, so a
/// cron introduced by an edit cannot fire before anyone has looked at it. See
/// the module docs for why an edit never arms in the other direction and why a
/// changed-but-already-present cron is left alone.
pub(crate) async fn update_company_workflow(
    company: &CompanyId,
    source_dir: Option<&Path>,
    store: &Arc<dyn CompanyStore>,
    revisions: &Arc<dyn WorkflowRevisionStore>,
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

    // Same record cross-check as create (roster + tool_call grants):
    // `parse_workflow` validates the graph's own shape but has no record to check
    // `agent`/`tool_call` node names against.
    validate_draft_against_record(&draft, &record)?;

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

    // Issue #276: did this edit arm a schedule that was not armed before?
    // Measured against the body being REPLACED, read under the same lock as the
    // write — not against anything the caller supplied, which is what makes the
    // rule impossible to talk out of with a crafted request.
    //
    // A stored body that no longer parses counts as "had no schedule", so an
    // edit that repairs a corrupt graph into a scheduled one disarms. That is
    // the conservative reading of an unreadable prior state, and it is the right
    // one: nobody can have reviewed a schedule the host could not read.
    let armed_before = parse_workflow(&record.overlay_workflows[index].toml)
        .ok()
        .and_then(|previous| previous.trigger_schedule().map(str::to_string))
        .is_some();
    let disarmed = file.trigger_schedule().is_some() && !armed_before;

    // Issue #274: snapshot the body we are about to overwrite, so the edit is
    // undoable. Captured HERE — inside the write lock, holding the prior TOML —
    // because this is the only instant a snapshot is race-free (the same reason
    // OpenHuman's `flow_revisions` insert rides inside the guarded UPDATE).
    //
    // Ordering is load-bearing: push the revision BEFORE `store.save`. A failed
    // push aborts the edit and the prior body is still the live one, so nothing
    // is lost — which is the exact failure this feature exists to prevent.
    // Save-first would risk overwriting the body and then losing its only copy.
    //
    // Deduped against a no-op save: when the new TOML is byte-identical to the
    // prior, there is nothing to lose — the version token already defines those
    // two as the same graph — so no snapshot is taken.
    let prior_toml = record.overlay_workflows[index].toml.clone();
    if prior_toml != toml_src {
        // Name from the prior parsed body; fall back to the id when it no longer
        // parses (a corrupt body still deserves a recoverable snapshot).
        let prior_name = parse_workflow(&prior_toml)
            .map(|f| f.name)
            .unwrap_or_else(|_| file.id.clone());
        let revision =
            WorkflowRevisionRecord::new(file.id.clone(), prior_name, prior_toml, now_millis());
        revisions.push_revision(company, &revision).await?;
    }

    // Replace in place: same slot, same order, so the picker doesn't reshuffle.
    record.overlay_workflows[index] = OverlayWorkflow {
        id: file.id.clone(),
        toml: toml_src,
    };
    if disarmed {
        // Same save as the new body, for the same reason create writes both at
        // once: a tick reads one record or the other, so the newly scheduled
        // graph is never visible to the scheduler while still armed.
        record.set_workflow_enabled(&file.id, false);
    }
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

    if disarmed {
        journal_enabled_change(
            company,
            events,
            &file.id,
            &file.name,
            false,
            WorkflowEnabledReason::Disarmed,
        )
        .await;
        tracing::info!(
            company = %company,
            workflow = %file.id,
            "edit added a schedule; workflow switched off pending review"
        );
    }

    Ok(file)
}

/// Restores a workflow to one of its captured revisions (issue #274), returning
/// the parsed [`WorkflowFile`] the union read path will now serve.
///
/// A rollback is **not** a special write path — it is an ordinary edit whose new
/// body happens to be an old one. It loads the revision (scoped to `wid`, so one
/// workflow's snapshot can never be restored onto another), converts its stored
/// TOML back into a draft, and routes it through
/// [`update_company_workflow`] unchanged. That reuse is deliberate and buys four
/// properties for free:
///
/// * **Re-validation against the *current* record.** A revision that named a
///   teammate who has since been removed is a `400`, not a broken restore — the
///   same roster/tool check every edit passes.
/// * **The rollback is itself undoable.** `update_company_workflow` snapshots the
///   *current* body before overwriting it, so restoring A over B captures B — a
///   rollback can be rolled back.
/// * **Optimistic concurrency.** `expected_version`, when supplied, is the token
///   of the body being replaced; a stale one is a `409`, so a rollback cannot
///   silently clobber a concurrent edit.
/// * **The #276 disarm.** If the restored graph carries a schedule the live one
///   lacked, it lands switched **off** — a restored cron cannot fire before
///   anyone has reviewed it.
///
/// Statuses: `400` (the revision is invalid against the current record), `404`
/// (unknown `wid` or unknown `rev_id`), `409` (seed-backed / body-less `wid`, a
/// stale token, or a name collision).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn rollback_company_workflow(
    company: &CompanyId,
    source_dir: Option<&Path>,
    store: &Arc<dyn CompanyStore>,
    revisions: &Arc<dyn WorkflowRevisionStore>,
    events: Option<&Arc<dyn EventLog>>,
    wid: &str,
    rev_id: &str,
    expected_version: Option<&str>,
) -> Result<WorkflowFile> {
    if !is_safe_workflow_id(wid) {
        return Err(OpenCompanyError::InvalidRequest(
            language::WORKFLOW_ID_INVALID.to_string(),
        ));
    }

    // Load the snapshot, scoped to this workflow. Unknown wid or unknown rev both
    // read as "nothing to restore" → 404.
    let revision = revisions
        .get_revision(company, wid, rev_id)
        .await?
        .ok_or_else(|| {
            OpenCompanyError::CompanyNotFound(format!("workflow {wid} revision {rev_id}"))
        })?;

    // Convert the captured body back into an editable draft and pin its id to
    // `wid`: a rollback restores a workflow **in place**, never renames it, and a
    // hand-mangled revision body must not be able to retarget another workflow.
    let mut draft = raw_workflow_from_toml(&revision.toml)?;
    draft.id = wid.to_string();

    update_company_workflow(
        company,
        source_dir,
        store,
        revisions,
        events,
        draft,
        expected_version,
    )
    .await
}

/// Switches a workflow on or off without touching its graph (issue #276).
///
/// Returns `true` when the record actually changed, `false` when the workflow
/// was already in the requested state — the caller reports `200` either way, so
/// a double-click is a no-op rather than a second journal entry.
///
/// # What may be toggled, and why it is a wider set than edit and delete
///
/// Any id the company actually answers for with a **graph** — a seed file or an
/// overlay body. Membership is decided by whether a body *exists*, not by
/// whether it parses: a stored graph the host can no longer read still toggles
/// (journalling under its id), because it is exactly the kind an operator most
/// wants stopped and refusing would leave them nothing to do about it. That is
/// deliberately broader than [`update_company_workflow`] and
/// [`delete_company_workflow`], which are overlay-only:
///
/// * A **seed-backed** workflow can be paused. Editing or deleting one is
///   refused because the read path would keep serving the seed and a boot
///   rebuild would resurrect the change — neither applies here. The switch lives
///   on the record, the source tree is untouched, and pausing can only ever
///   *remove* capability, so it cannot let a runtime write outlive a seed
///   rollback the way a record-wins `[tools]` or `[policy]` merge could. An
///   operator who cannot stop a committed cron without a redeploy has no pause
///   switch at all, which is issue #276(a) verbatim.
/// * A **bodiless** manifest-`enabled` id is refused with a
///   [`Conflict`](OpenCompanyError::Conflict), same as edit and delete: it is a
///   name with no graph, so there is no schedule to stop.
/// * An **unknown** id is a [`CompanyNotFound`](OpenCompanyError::CompanyNotFound)
///   (404).
///
/// # No version token, deliberately
///
/// `PUT`/`DELETE` take an `expectedVersion` because two consoles editing one
/// graph can silently lose an edit. A switch has no such hazard: it carries no
/// content to overwrite, both operators can see the resulting state, and
/// last-write-wins is what a light switch already means. Requiring a token would
/// also make a seed-backed workflow untoggleable, since only overlay bodies have
/// one.
pub(crate) async fn set_company_workflow_enabled(
    company: &CompanyId,
    source_dir: Option<&Path>,
    store: &Arc<dyn CompanyStore>,
    events: Option<&Arc<dyn EventLog>>,
    wid: &str,
    enabled: bool,
) -> Result<bool> {
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

    // Does this company answer for `wid` with a graph at all? Asked as "is there
    // a body" rather than "does it parse", because the two are different states
    // and only one of them is the operator's fault.
    //
    // `load_workflow_union` returns `Err` for a body that exists but no longer
    // parses, and `Ok(None)` only when there is nothing there. Collapsing both
    // into "not found" — which an earlier revision did, with `.ok().flatten()` —
    // sent a corrupt-graph id down the bodiless branch and told the operator it
    // "was provisioned by name only". That is false for a workflow they created,
    // and it leaves them with no way to pause it and no accurate reason why.
    //
    // A **global** graph counts too, as long as this company has not dropped it
    // outright via `[globals].disable` — a global has no seed file and no
    // overlay body of its own, so without this arm it read exactly like a
    // bodiless manifest-`enabled` id and could never be paused from here,
    // despite `disabled_workflows` (the pause flag this function writes) being
    // a wholly separate mechanism from the globals opt-out.
    let is_undisabled_global =
        !crate::globals::disabled(&record.manifest.globals.disable, "workflow", wid)
            && crate::globals::workflows().iter().any(|w| w.id == wid);
    let has_body = seed_file_exists(source_dir, wid)
        || record.overlay_workflows.iter().any(|w| w.id == wid)
        || is_undisabled_global;
    if !has_body {
        if record.manifest.workflows.enabled.iter().any(|id| id == wid) {
            return Err(OpenCompanyError::Conflict(format!(
                "Workflow `{wid}` is enabled for this company but has no saved graph, so there is \
                 no schedule to switch off — it was provisioned by name only."
            )));
        }
        return Err(OpenCompanyError::CompanyNotFound(format!("workflow {wid}")));
    }

    // The display name for the journal, read before anything changes. A body
    // that no longer parses still toggles — the same call
    // [`delete_company_workflow`] makes, and for the same reason: a graph the
    // host cannot read is exactly the kind an operator most wants to stop, and
    // refusing would leave them nothing to do about it. It just journals under
    // its id.
    let file = crate::company::load_workflow_with_globals(
        source_dir,
        &record.overlay_workflows,
        &record.manifest.globals.disable,
        wid,
    )
    .ok()
    .flatten();
    let name = file
        .as_ref()
        .map(|file| file.name.clone())
        .unwrap_or_else(|| wid.to_string());

    // Issue #976: arming is where the promise is made, so it is where the
    // promise is checked. A stage-less graph with a schedule fires on time, runs
    // nothing and reports nothing — `campaign` on staging is exactly that, one
    // resume away from a schedule it cannot keep.
    //
    // **Refused here rather than at save, deliberately.** Saving a stub
    // mid-authoring is legitimate and the module is built for it: `parse_workflow`
    // was made lenient on purpose (#661) so the console can drop a trigger and
    // add stages afterwards, and refusing an empty graph at save would also
    // refuse every existing seed and legacy body on its next edit. Saving
    // promises nothing; switching a schedule on promises that something happens.
    // This is also the human gate #276 already forces a scheduled graph through,
    // and it has the parsed graph in hand for the journal name above — so the
    // check costs no extra load.
    //
    // Only `enabled`, and only when a schedule is what is being armed. Switching
    // such a workflow OFF stays allowed: an operator must always be able to stop
    // a thing, which is the same call the unparseable-body case above makes.
    // A manual (unscheduled) graph is left alone — running a stub by hand is the
    // author's own business, and the run says so through
    // [`STAGELESS_WORKFLOW_NOTICE`](crate::company::STAGELESS_WORKFLOW_NOTICE).
    if enabled
        && let Some(file) = file.as_ref()
        && file.trigger_schedule().is_some()
        && !file.has_runnable_node()
    {
        return Err(OpenCompanyError::InvalidRequest(
            crate::company::STAGELESS_SCHEDULE_REFUSAL.to_string(),
        ));
    }

    if !record.set_workflow_enabled(wid, enabled) {
        return Ok(false);
    }
    store.save(&record).await?;

    drop(_lock);

    journal_enabled_change(
        company,
        events,
        wid,
        &name,
        enabled,
        WorkflowEnabledReason::Operator,
    )
    .await;
    tracing::info!(
        company = %company,
        workflow = %wid,
        enabled,
        "workflow switched by an operator"
    );

    Ok(true)
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
/// After the committed save, three best-effort cascades tear down what the
/// workflow leaves behind: its durable scheduler fire ledger (issue #708), its
/// revision history (issue #274), and an audit-journal entry. The fire-ledger
/// purge runs **first** and **only after** the save has committed and the write
/// lock is dropped: purging before a successful save could strip a still-live
/// workflow's claim rows on a save failure (a #241-class cross-replica
/// double-fire); purging after means a delete+recreate of the same id — which
/// reuses the restart-stable `workflow-<id>` schedule key — starts against an
/// empty ledger, with no inherited anchor and every past minute claimable
/// again. A purge failure is logged, never rolled back: the workflow is already
/// gone, and the worst case is one bounded, logged reinstatement of the old
/// pre-fix behaviour — the same contract as the revision cascade below.
///
/// Returns the removed workflow's display name for the audit journal (falling
/// back to the id when the stored body no longer parses).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn delete_company_workflow(
    company: &CompanyId,
    source_dir: Option<&Path>,
    store: &Arc<dyn CompanyStore>,
    revisions: &Arc<dyn WorkflowRevisionStore>,
    schedule_fires: Option<&Arc<dyn ScheduleFireStore>>,
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

    // Issue #708: purge the schedule's durable fire ledger, minting the key at
    // the ONE authoritative site (`workflow_schedule_id`) so this key can never
    // drift from the scheduler's. This runs AFTER the committed save on purpose
    // (see the doc): purging before a save that then fails would strip a
    // still-live workflow's claim rows and risk a #241-class double-fire.
    // Best-effort, exactly like the revision cascade below: the workflow is
    // already gone, so a failure is logged rather than rolled back — a leftover
    // ledger merely re-instates the bounded pre-fix behaviour once, on a
    // recreate of the same id.
    //
    // `schedule_fires` is `Option` for the same reason `events` is: not every
    // caller wires it. The HTTP delete path passes the runtime store (`Some`) —
    // that is the path a scheduled workflow can be deleted from, so it is the
    // only one that can orphan a ledger. The agent `delete_workflow` tool passes
    // `None`, and correctly: it refuses to delete a scheduled workflow at all
    // (`refuse_scheduled`), so no fire ledger can exist for it to leave behind.
    if let Some(fires) = schedule_fires {
        let schedule_id = workflow_schedule_id(wid);
        if let Err(err) = fires.delete_schedule_fires(company, &schedule_id).await {
            tracing::warn!(
                company = %company,
                workflow = %wid,
                schedule = %schedule_id,
                error = %err,
                "workflow deleted but its schedule fire ledger could not be purged"
            );
        }
    }

    // Issue #274: cascade the workflow's revision history away with it, so a
    // removed workflow leaves no orphaned snapshots behind. Best-effort in the
    // same sense as the audit journal below: the workflow is already gone by the
    // time this runs, so a failure is logged rather than rolling the delete back
    // (and a leftover ring is harmless — a re-created id starts its own history,
    // and nothing reads another workflow's rows).
    if let Err(err) = revisions.delete_revisions(company, wid).await {
        tracing::warn!(
            company = %company,
            workflow = %wid,
            error = %err,
            "workflow deleted but its revision history could not be cleared"
        );
    }

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
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    /// An in-memory [`EventLog`] whose [`subscribe`](EventLog::subscribe) stream
    /// actually delivers what [`append`](EventLog::append) writes — the property
    /// [`MemLog`] above deliberately lacks (its `subscribe` is empty). This is
    /// what lets a test stand in for the live SSE fan-out: the console's picker
    /// re-reads off exactly this broadcast (issue #1045), so a create/delete
    /// that reaches a live subscriber here is evidence that in-process delivery
    /// is intact and a stale picker is a console-side defect, not a lost frame.
    struct BroadcastMemLog {
        tx: tokio::sync::broadcast::Sender<StoredEvent>,
        next_seq: StdMutex<u64>,
    }

    impl BroadcastMemLog {
        fn new() -> Self {
            Self {
                tx: tokio::sync::broadcast::channel(64).0,
                next_seq: StdMutex::new(0),
            }
        }
    }

    #[async_trait]
    impl EventLog for BroadcastMemLog {
        async fn append(&self, id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
            let seq = {
                let mut n = self.next_seq.lock().unwrap();
                *n += 1;
                *n
            };
            let stored = StoredEvent {
                seq: EventSeq::new(seq),
                company: id.clone(),
                event,
                at_millis: now_millis(),
            };
            // No live subscriber is not an error — a send with zero receivers
            // just means nobody is watching yet.
            let _ = self.tx.send(stored);
            Ok(EventSeq::new(seq))
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> Result<Vec<StoredEvent>> {
            Ok(Vec::new())
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            let rx = self.tx.subscribe();
            Box::pin(stream::unfold(rx, |mut rx| async move {
                // Each call to this closure produces exactly one item and hands
                // the receiver back as continuation state, so there is no loop
                // here.
                match rx.recv().await {
                    Ok(event) => Some((crate::ports::events::EventStreamItem::Event(event), rx)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        Some((crate::ports::events::EventStreamItem::Gap { missed }, rx))
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
                }
            }))
        }
    }

    /// An in-memory [`WorkflowRevisionStore`] so the capture, prune, cascade and
    /// rollback behaviour can be asserted without a real backend. Pruning to the
    /// cap is applied on push, mirroring the durable backends.
    #[derive(Default)]
    struct MemRevisions {
        rows: StdMutex<Vec<WorkflowRevisionRecord>>,
    }

    #[async_trait]
    impl WorkflowRevisionStore for MemRevisions {
        async fn push_revision(
            &self,
            _company: &CompanyId,
            revision: &WorkflowRevisionRecord,
        ) -> Result<()> {
            use crate::ports::workflow_revisions::{MAX_WORKFLOW_REVISIONS, sort_newest_first};
            let mut rows = self.rows.lock().unwrap();
            rows.push(revision.clone());
            let mut mine: Vec<WorkflowRevisionRecord> = rows
                .iter()
                .filter(|r| r.workflow_id == revision.workflow_id)
                .cloned()
                .collect();
            if mine.len() > MAX_WORKFLOW_REVISIONS {
                sort_newest_first(&mut mine);
                let keep: std::collections::HashSet<String> = mine
                    .into_iter()
                    .take(MAX_WORKFLOW_REVISIONS)
                    .map(|r| r.id)
                    .collect();
                rows.retain(|r| r.workflow_id != revision.workflow_id || keep.contains(&r.id));
            }
            Ok(())
        }
        async fn list_revisions(
            &self,
            _company: &CompanyId,
            workflow_id: &str,
        ) -> Result<Vec<WorkflowRevisionRecord>> {
            use crate::ports::workflow_revisions::sort_newest_first;
            let mut mine: Vec<WorkflowRevisionRecord> = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.workflow_id == workflow_id)
                .cloned()
                .collect();
            sort_newest_first(&mut mine);
            Ok(mine)
        }
        async fn get_revision(
            &self,
            _company: &CompanyId,
            workflow_id: &str,
            revision_id: &str,
        ) -> Result<Option<WorkflowRevisionRecord>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.workflow_id == workflow_id && r.id == revision_id)
                .cloned())
        }
        async fn delete_revisions(&self, _company: &CompanyId, workflow_id: &str) -> Result<u64> {
            let mut rows = self.rows.lock().unwrap();
            let before = rows.len();
            rows.retain(|r| r.workflow_id != workflow_id);
            Ok((before - rows.len()) as u64)
        }
    }

    /// A throwaway revision store for tests that do not assert on revision
    /// capture — the common case. Tests that DO assert capture/prune/rollback
    /// hold their own `Arc<MemRevisions>` so they can read it back.
    fn revs() -> Arc<dyn WorkflowRevisionStore> {
        Arc::new(MemRevisions::default())
    }

    /// An in-memory [`ScheduleFireStore`] so the delete-time fire-ledger purge
    /// (issue #708) can be asserted without a real backend. Only the verbs the
    /// delete path exercises need real behaviour; `claim_fire` seeds a ledger
    /// and `delete_schedule_fires` purges one schedule's rows.
    #[derive(Default)]
    struct MemFires {
        /// `(company, schedule_id) -> claimed minutes`.
        rows: StdMutex<std::collections::HashMap<(String, String), std::collections::HashSet<u64>>>,
        /// Arm the next `delete_schedule_fires` call to error, to prove the
        /// delete succeeds even when the purge cascade fails.
        fail_delete: std::sync::atomic::AtomicBool,
    }

    impl MemFires {
        fn seed(&self, company: &CompanyId, schedule_id: &str, minute: u64) {
            self.rows
                .lock()
                .unwrap()
                .entry((company.as_ref().to_string(), schedule_id.to_string()))
                .or_default()
                .insert(minute);
        }
        fn minutes(&self, company: &CompanyId, schedule_id: &str) -> Vec<u64> {
            let rows = self.rows.lock().unwrap();
            let mut ms: Vec<u64> = rows
                .get(&(company.as_ref().to_string(), schedule_id.to_string()))
                .map(|set| set.iter().copied().collect())
                .unwrap_or_default();
            ms.sort_unstable();
            ms
        }
        fn arm_delete_failure(&self) {
            self.fail_delete
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ScheduleFireStore for MemFires {
        async fn claim_fire(&self, c: &CompanyId, s: &str, m: u64) -> Result<bool> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .entry((c.as_ref().to_string(), s.to_string()))
                .or_default()
                .insert(m))
        }
        async fn latest_fire(&self, c: &CompanyId, s: &str) -> Result<Option<u64>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&(c.as_ref().to_string(), s.to_string()))
                .and_then(|set| set.iter().max().copied()))
        }
        async fn prune_fires_before(&self, _c: &CompanyId, _m: u64) -> Result<usize> {
            Ok(0)
        }
        async fn delete_schedule_fires(&self, c: &CompanyId, s: &str) -> Result<usize> {
            if self
                .fail_delete
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(OpenCompanyError::Store("flaky fire-ledger purge".into()));
            }
            Ok(self
                .rows
                .lock()
                .unwrap()
                .remove(&(c.as_ref().to_string(), s.to_string()))
                .map_or(0, |set| set.len()))
        }
    }

    /// A throwaway fire store for delete tests that do not assert on the purge —
    /// the common case. Tests that DO assert the purge hold their own
    /// `Arc<MemFires>` so they can read it back.
    fn fires() -> Arc<dyn ScheduleFireStore> {
        Arc::new(MemFires::default())
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
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
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

    /// A graph whose only node is its trigger, carrying a schedule — the
    /// `campaign` shape from staging (issue #976).
    fn stageless_scheduled_draft(id: &str, name: &str) -> RawWorkflow {
        let mut draft = valid_draft(id, name);
        // Keep only the trigger, and put a schedule on it. This is what the
        // console produces when somebody drops a Start node, sets a cron, and
        // saves before adding any stage.
        draft.nodes.retain(|n| n.kind == "trigger");
        draft.edges.clear();
        draft.nodes[0].schedule = Some("0 9 * * *".to_string());
        draft
    }

    /// Saving one is **allowed**, and that is the deliberate half of the fix.
    /// Authoring is incremental: the console drops a Start node first and adds
    /// stages after, `parse_workflow` was made lenient on purpose (#661) to
    /// support exactly that, and refusing at save would also refuse every
    /// existing seed and legacy body on its next edit.
    #[tokio::test]
    async fn a_stageless_graph_still_saves() {
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
            stageless_scheduled_draft("campaign", "Campaign"),
        )
        .await
        .expect("a stub mid-authoring is legitimate and must save");
    }

    /// ...but switching its schedule on is refused. Arming is where the promise
    /// is made, so it is where the promise is checked: resume this and it fires
    /// on time, runs nothing, and reports nothing.
    #[tokio::test]
    async fn arming_a_stageless_schedule_is_refused() {
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
            stageless_scheduled_draft("campaign", "Campaign"),
        )
        .await
        .expect("saves");

        let err = set_company_workflow_enabled(
            &company,
            Some(dir.path()),
            &store,
            None,
            "campaign",
            true,
        )
        .await
        .expect_err("a schedule that cannot run must not be armed");

        let rendered = err.to_string();
        assert!(
            rendered.contains("no stage to run"),
            "the operator is told WHAT is wrong: {rendered}"
        );
        assert!(
            rendered.contains("Add at least one node"),
            "...and the one thing they can do about it: {rendered}"
        );
    }

    /// Switching such a workflow **off** stays allowed. An operator must always
    /// be able to stop a thing — the same call the unparseable-body case makes
    /// — and a guard that trapped a workflow in the armed state would be worse
    /// than the silence it replaced.
    #[tokio::test]
    async fn a_stageless_schedule_can_still_be_switched_off() {
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
            stageless_scheduled_draft("campaign", "Campaign"),
        )
        .await
        .expect("saves");

        set_company_workflow_enabled(&company, Some(dir.path()), &store, None, "campaign", false)
            .await
            .expect("pausing must never be refused");
    }

    /// A graph with a real stage arms normally. Without this the refusal above
    /// would pass against a build that refused every schedule.
    #[tokio::test]
    async fn arming_a_scheduled_graph_with_a_stage_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));

        let mut draft = valid_draft("greeter", "Greeter");
        draft.nodes[0].schedule = Some("0 9 * * *".to_string());
        create_company_workflow(&company, Some(dir.path()), &store, None, draft)
            .await
            .expect("saves");

        set_company_workflow_enabled(&company, Some(dir.path()), &store, None, "greeter", true)
            .await
            .expect("a graph that can actually run may be armed");
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
            &revs(),
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
        // The manifest declaration is untouched — it says which workflows this
        // company has, not which of them are armed.
        assert_eq!(record.manifest.workflows.enabled, vec!["greeter"]);
        // …but this edit added a cron to a manual graph, so issue #276's disarm
        // rule switched it off in the same save. The draft above is exactly the
        // manual→automatic transition the rule exists for, which is why this
        // assertion lives on the general update test rather than only on the
        // dedicated one.
        assert!(
            !record.workflow_enabled("greeter"),
            "an edit that adds a schedule must leave the workflow switched off"
        );

        // What the union read path serves is what we returned.
        let reloaded = load_workflow_union(None, &record.overlay_workflows, "greeter")
            .expect("reloads")
            .expect("present");
        assert_eq!(reloaded, file);

        let events = log.events.lock().unwrap();
        assert_eq!(events.len(), 2, "the edit, then the disarm it triggered");
        match &events[0] {
            CompanyEvent::WorkflowUpdated {
                workflow_id, name, ..
            } => {
                assert_eq!(workflow_id, "greeter");
                assert_eq!(name, "Greeter");
            }
            other => panic!("expected WorkflowUpdated, got {other:?}"),
        }
        match &events[1] {
            CompanyEvent::WorkflowEnabledChanged {
                workflow_id,
                enabled,
                reason,
                ..
            } => {
                assert_eq!(workflow_id, "greeter");
                assert!(!enabled);
                assert_eq!(*reason, WorkflowEnabledReason::Disarmed);
            }
            other => panic!("expected WorkflowEnabledChanged, got {other:?}"),
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
        update_company_workflow(&company, None, &store, &revs(), None, theirs, None)
            .await
            .expect("first writer wins");

        // Our stale token is now wrong.
        let mut ours = valid_draft("greeter", "Greeter");
        ours.description = Some("Ours would clobber.".to_string());
        let err =
            update_company_workflow(&company, None, &store, &revs(), None, ours, Some(&stale))
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
        update_company_workflow(&company, None, &store, &revs(), None, once, Some(&first))
            .await
            .expect("first conditional write");

        let record = store.load(&company).await.unwrap().unwrap();
        let second = workflow_version(&record.overlay_workflows[0].toml);
        assert_ne!(second, first, "the token must move when the body does");

        let mut twice = valid_draft("greeter", "Greeter");
        twice.description = Some("Two.".to_string());
        update_company_workflow(&company, None, &store, &revs(), None, twice, Some(&second))
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
        update_company_workflow(&company, None, &store, &revs(), None, draft, None)
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
        update_company_workflow(&company, None, &store, &revs(), None, draft, Some(&version))
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
            &revs(),
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
        let err = update_company_workflow(&company, None, &store, &revs(), None, no_trigger, None)
            .await
            .expect_err("no trigger");
        assert!(err.to_string().contains("exactly one `trigger`"), "{err}");

        // Off-roster teammate.
        let mut ghost = valid_draft("greeter", "Greeter");
        ghost.nodes[1].agent = Some("ghost".to_string());
        let err = update_company_workflow(&company, None, &store, &revs(), None, ghost, None)
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
            &revs(),
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
            &revs(),
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
            &revs(),
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
        update_company_workflow(&company, None, &store, &revs(), None, draft, None)
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
            &revs(),
            Some(&fires()),
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

    /// Issue #1045: the REST create/delete persist path puts
    /// `WorkflowCreated` / `WorkflowDeleted` on a stream a **live subscriber**
    /// actually receives — the in-process delivery the console's SSE picker
    /// depends on. A green characterization: it locates the reported "graph
    /// authored elsewhere stays invisible" defect on the console side, not in a
    /// dropped host frame.
    ///
    /// The projection of these variants onto the `{type, workflowId, name}` wire
    /// frame the console keys on is asserted next to `project_event` itself
    /// (`server::operator` — `projects_workflow_created_without_the_actor`,
    /// `projects_workflow_updated_and_deleted_without_the_actor`). This test
    /// closes the remaining link: that the persist path emits those variants,
    /// carrying the same id and name, onto a stream `subscribe` delivers.
    #[tokio::test]
    async fn create_and_delete_reach_a_live_subscriber() {
        use futures::StreamExt;

        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let log = Arc::new(BroadcastMemLog::new());
        let log_dyn: Arc<dyn EventLog> = log.clone();
        // Subscribe before the writes, exactly as the SSE handler does.
        let mut stream = log_dyn.subscribe(&company);

        create_company_workflow(
            &company,
            None,
            &store,
            Some(&log_dyn),
            valid_draft("greeter", "Greeter"),
        )
        .await
        .expect("creates");

        let created = stream
            .next()
            .await
            .expect("workflow_created delivered live");
        let created_event = match created {
            crate::ports::events::EventStreamItem::Event(ev) => ev,
            other => panic!("expected a live Event frame, got {other:?}"),
        };
        match &created_event.event {
            CompanyEvent::WorkflowCreated {
                workflow_id, name, ..
            } => {
                assert_eq!(workflow_id, "greeter");
                assert_eq!(name, "Greeter");
            }
            other => panic!("expected WorkflowCreated on the wire, got {other:?}"),
        }

        // Delete the same graph over the same persist path. `None` expected
        // version skips the optimistic-concurrency check — this test is about
        // the emitted frame, not the token.
        delete_company_workflow(
            &company,
            None,
            &store,
            &revs(),
            Some(&fires()),
            Some(&log_dyn),
            "greeter",
            None,
        )
        .await
        .expect("deletes");

        let deleted = stream
            .next()
            .await
            .expect("workflow_deleted delivered live");
        let deleted_event = match deleted {
            crate::ports::events::EventStreamItem::Event(ev) => ev,
            other => panic!("expected a live Event frame, got {other:?}"),
        };
        match &deleted_event.event {
            CompanyEvent::WorkflowDeleted {
                workflow_id, name, ..
            } => {
                assert_eq!(workflow_id, "greeter");
                assert_eq!(name, "Greeter");
            }
            other => panic!("expected WorkflowDeleted on the wire, got {other:?}"),
        }
    }

    /// #708: a committed delete purges the schedule's durable fire ledger under
    /// the exact `workflow-<id>` key, so a recreated same-id workflow inherits
    /// no anchor and no stale claim.
    #[tokio::test]
    async fn deleting_a_workflow_purges_its_schedule_fire_ledger() {
        let company = CompanyId::new("acme");
        let (store, _) = with_one_workflow(&company, "greeter", "Greeter").await;

        // A ledger for greeter's schedule, plus a sibling schedule's row to
        // prove the purge is scoped to exactly the deleted workflow's key.
        let fires = Arc::new(MemFires::default());
        let greeter_key = workflow_schedule_id("greeter");
        fires.seed(&company, &greeter_key, 100);
        fires.seed(&company, &greeter_key, 101);
        fires.seed(&company, &workflow_schedule_id("other"), 100);
        let fires_dyn: Arc<dyn ScheduleFireStore> = fires.clone();

        delete_company_workflow(
            &company,
            None,
            &store,
            &revs(),
            Some(&fires_dyn),
            None,
            "greeter",
            None,
        )
        .await
        .expect("deletes");

        assert!(
            fires.minutes(&company, &greeter_key).is_empty(),
            "the deleted workflow's whole fire ledger is purged"
        );
        assert_eq!(
            fires.minutes(&company, &workflow_schedule_id("other")),
            vec![100],
            "a sibling workflow's schedule ledger is untouched"
        );
    }

    /// #708: the purge is best-effort. A purge failure is logged, never rolled
    /// back — the workflow is already gone, so the delete still succeeds.
    #[tokio::test]
    async fn a_failing_fire_ledger_purge_still_deletes_the_workflow() {
        let company = CompanyId::new("acme");
        let (store, _) = with_one_workflow(&company, "greeter", "Greeter").await;

        let fires = Arc::new(MemFires::default());
        fires.seed(&company, &workflow_schedule_id("greeter"), 100);
        fires.arm_delete_failure();
        let fires_dyn: Arc<dyn ScheduleFireStore> = fires.clone();

        let name = delete_company_workflow(
            &company,
            None,
            &store,
            &revs(),
            Some(&fires_dyn),
            None,
            "greeter",
            None,
        )
        .await
        .expect("delete succeeds even when the purge cascade errors");
        assert_eq!(name, "Greeter");

        // The graph is gone despite the purge error.
        let record = store.load(&company).await.unwrap().unwrap();
        assert!(record.overlay_workflows.is_empty(), "body must be gone");
        assert!(record.manifest.workflows.enabled.is_empty());
    }

    /// The delete is durable across the #208 boot rebuild *because* the overlay
    /// body is gone: `merge_enabled_workflows` re-derives `enabled` from seed
    /// ids ∪ surviving overlay ids, so there is nothing left to resurrect. This
    /// pins the invariant the delete's correctness rests on.
    #[tokio::test]
    async fn a_deleted_workflow_has_nothing_left_for_the_boot_merge_to_re_enable() {
        let company = CompanyId::new("acme");
        let (store, _) = with_one_workflow(&company, "greeter", "Greeter").await;
        delete_company_workflow(
            &company,
            None,
            &store,
            &revs(),
            Some(&fires()),
            None,
            "greeter",
            None,
        )
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
        update_company_workflow(&company, None, &store, &revs(), None, theirs, None)
            .await
            .expect("someone edits first");

        // A held fire store, seeded under the workflow's schedule key, proves the
        // refused delete purges NOTHING — the purge runs only after a committed
        // save, so a version-refused delete (which never saves) leaves the live
        // workflow's ledger intact (#708).
        let fires = Arc::new(MemFires::default());
        fires.seed(&company, &workflow_schedule_id("greeter"), 42);
        let fires_dyn: Arc<dyn ScheduleFireStore> = fires.clone();

        let err = delete_company_workflow(
            &company,
            None,
            &store,
            &revs(),
            Some(&fires_dyn),
            None,
            "greeter",
            Some(&stale),
        )
        .await
        .expect_err("stale delete must be refused");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");

        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1, "nothing was removed");
        assert_eq!(
            fires.minutes(&company, &workflow_schedule_id("greeter")),
            vec![42],
            "a version-refused delete never reaches the purge — the ledger is intact"
        );
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

        let err = delete_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            &revs(),
            Some(&fires()),
            None,
            "seeded",
            None,
        )
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
        let err = delete_company_workflow(
            &company,
            None,
            &store,
            &revs(),
            Some(&fires()),
            None,
            "ghost",
            None,
        )
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
        let err = delete_company_workflow(
            &company,
            None,
            &store,
            &revs(),
            Some(&fires()),
            None,
            "../secrets",
            None,
        )
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

        delete_company_workflow(
            &company,
            None,
            &store,
            &revs(),
            Some(&fires()),
            None,
            "b",
            None,
        )
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

        delete_company_workflow(
            &company,
            None,
            &store,
            &revs(),
            Some(&fires()),
            None,
            "greeter",
            None,
        )
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

    // --- #540: author-time tool_call validation ----------------------------

    /// A manifest with the `assistant` roster agent AND an explicit
    /// `[tools].allow`, so tool_call grant coverage can be exercised precisely.
    ///
    /// Only the tool_call grant-coverage tests use this, and every one of them
    /// is gated on `openhuman` (the tool catalogue lives behind that feature);
    /// without the gate the helper is dead code at default features and trips
    /// `-D warnings` in the default `Rust` CI lane.
    #[cfg(feature = "openhuman")]
    fn manifest_with_allow(allow: &[&str]) -> CompanyManifest {
        let list = allow
            .iter()
            .map(|grant| format!("\"{grant}\""))
            .collect::<Vec<_>>()
            .join(", ");
        toml::from_str(&format!(
            "[company]\nname = \"Acme\"\n[tools]\nallow = [{list}]\n[[agent]]\nid = \"assistant\"\nrole = \"Assistant\"\n"
        ))
        .expect("valid manifest")
    }

    /// A `trigger → tool_call` draft. `slug` of `None` omits `config` entirely,
    /// so the ungated slug-presence check fires. Otherwise the node carries a
    /// generic `config.args` table with every workflow-tool required-arg key set
    /// (issue #813), so a positive-control slug clears the required-args arm — the
    /// arm checks only presence, so the extra keys are harmless and this stays
    /// feature-agnostic (no catalogue reference).
    fn tool_call_draft(id: &str, name: &str, slug: Option<&str>) -> RawWorkflow {
        let mut args = toml::map::Map::new();
        for key in [
            "command",
            "edits",
            "operation",
            "data",
            "filename",
            "url",
            "path",
            "query",
        ] {
            args.insert(key.to_string(), toml::Value::String("x".to_string()));
        }
        tool_call_draft_args(id, name, slug, Some(toml::Value::Table(args)))
    }

    /// A `trigger → tool_call` draft with explicit control over `config.args` —
    /// used to exercise the #813 required-args arm (absent args, present args)
    /// directly. `args` of `None` omits the `args` table entirely.
    fn tool_call_draft_args(
        id: &str,
        name: &str,
        slug: Option<&str>,
        args: Option<toml::Value>,
    ) -> RawWorkflow {
        let config = slug.map(|slug| {
            let mut table = toml::map::Map::new();
            table.insert("slug".to_string(), toml::Value::String(slug.to_string()));
            if let Some(args) = &args {
                table.insert("args".to_string(), args.clone());
            }
            toml::Value::Table(table)
        });
        RawWorkflow {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
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
                    id: "call".to_string(),
                    kind: "tool_call".to_string(),
                    name: "Call".to_string(),
                    summary: None,
                    agent: None,
                    schedule: None,
                    config,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
                },
            ],
            edges: vec![RawEdge {
                from: "start".to_string(),
                to: "call".to_string(),
                label: None,
            }],
        }
    }

    /// UNGATED: a tool_call with no `slug` is refused before any feature-specific
    /// namespace resolution, so it fails the same way in every build.
    #[tokio::test]
    async fn tool_call_without_slug_is_invalid() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft("wf", "WF", None),
        )
        .await
        .expect_err("tool_call with no slug");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("no `slug`"), "{err}");
    }

    /// A slug that maps to no toolbelt namespace is unwired — the run would halt
    /// on it, so the save is refused with the run gate's own wording.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn tool_call_with_bogus_slug_is_invalid() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft("wf", "WF", Some("totally_bogus")),
        )
        .await
        .expect_err("unwired slug");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("not a wired workflow tool"),
            "{err}"
        );
    }

    /// A wired slug whose namespace the company's `[tools].allow` does not cover
    /// is refused; granting that namespace lets the same slug through.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn tool_call_slug_outside_the_granted_namespace_is_invalid() {
        let company = CompanyId::new("acme");
        // `web.*` grants `web` but NOT `code`; `csv_export` is a `code` tool.
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_allow(&["web.*"]),
        )));
        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft("wf", "WF", Some("csv_export")),
        )
        .await
        .expect_err("code slug not granted");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("does not grant"), "{err}");

        // Positive control: grant `code` and the same slug is accepted.
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_allow(&["code"]),
        )));
        create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft("wf2", "WF2", Some("csv_export")),
        )
        .await
        .expect("code slug is granted");
    }

    /// Mirrors `caps/tools.rs`'s
    /// `the_search_namespace_requires_an_explicit_grant_not_a_wildcard`: the
    /// catch-all `*` never confers the priced `search` family, but an explicit
    /// `search` grant does.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn the_search_namespace_needs_an_explicit_grant_not_a_wildcard() {
        let company = CompanyId::new("acme");
        // `*` covers ordinary namespaces but must NOT buy a managed search call.
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_allow(&["*"]),
        )));
        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft("wf", "WF", Some("web_search")),
        )
        .await
        .expect_err("wildcard never confers search");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("does not grant"), "{err}");

        // An explicit `search` grant alongside the belt passes.
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_allow(&["*", "search"]),
        )));
        create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft("wf2", "WF2", Some("web_search")),
        )
        .await
        .expect("explicit search grant is honored");
    }

    /// The shared helper gates BOTH surfaces: an update into a graph with an
    /// unwired tool_call slug is refused the same way a create is.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn update_gates_tool_calls_through_the_shared_helper() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        create_company_workflow(&company, None, &store, None, valid_draft("wf", "WF"))
            .await
            .expect("seed create");

        let err = update_company_workflow(
            &company,
            None,
            &store,
            &revs(),
            None,
            tool_call_draft("wf", "WF", Some("totally_bogus")),
            None,
        )
        .await
        .expect_err("update must gate tool_calls too");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("not a wired workflow tool"),
            "{err}"
        );
    }

    /// A slug padded with leading/trailing whitespace is rejected outright rather
    /// than silently trimmed: the persisted config and the run-time lookup are
    /// literal, so a padded slug that "passed" a trim-normalized check would halt
    /// the run on the very lookup this save-time gate promised to catch.
    /// (Regression, #540.)
    #[tokio::test]
    async fn tool_call_with_a_whitespace_padded_slug_is_rejected() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft("wf", "WF", Some(" csv_export ")),
        )
        .await
        .expect_err("padded slug");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("whitespace"), "{err}");
    }

    /// `media` and `composio` are agent-turn tool families the workflow invoker
    /// never wires (see `WORKFLOW_TOOL_NAMESPACES`), so a `tool_call` naming one
    /// would clear the run-time grant gate and then ALWAYS miss the lookup.
    /// Author-time validation rejects it up front even when the namespace is
    /// explicitly granted, so the save mirrors the run. (Regression, #540.)
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn tool_call_in_an_agent_turn_only_family_is_rejected() {
        let company = CompanyId::new("acme");
        // Grant BOTH families explicitly, so the rejection is about the workflow
        // surface — not a missing grant.
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_allow(&["media", "composio"]),
        )));
        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft("wf", "WF", Some("media_generate_image")),
        )
        .await
        .expect_err("media is not a workflow tool family");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("cannot run"), "{err}");
    }

    // --- #813: required `config.args` on a workflow tool_call -----------------

    /// A granted `tool_call` whose required `config.args` are absent is refused at
    /// author time, naming the missing args — the same gate the create-time
    /// copilot hears via courtesy validation. `csv_export` needs `data` and
    /// `filename`; the run would otherwise export nothing.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn tool_call_missing_required_args_is_invalid() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_allow(&["code"]),
        )));
        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft_args("wf", "WF", Some("csv_export"), None),
        )
        .await
        .expect_err("missing required args");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("config.args") && msg.contains("data") && msg.contains("filename"),
            "the missing args are named: {msg}"
        );
    }

    /// The same slug WITH its required args under `config.args` is accepted — the
    /// arm gates the absence, not the tool. A `=`-expression counts as present.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn tool_call_with_required_args_is_accepted() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_allow(&["code"]),
        )));
        let mut args = toml::map::Map::new();
        args.insert(
            "data".to_string(),
            toml::Value::String("=nodes.pick.items".to_string()),
        );
        args.insert(
            "filename".to_string(),
            toml::Value::String("out.csv".to_string()),
        );
        create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft_args(
                "wf",
                "WF",
                Some("csv_export"),
                Some(toml::Value::Table(args)),
            ),
        )
        .await
        .expect("required args present");
    }

    /// `read_workspace_state` has NO required args, so an empty-args node is not
    /// blocked by the arm — its inability to read a file is a grounding concern
    /// (the copilot's honest capability line), not an author-time gate.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn tool_call_with_no_required_args_is_accepted_empty() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_allow(&["shell"]),
        )));
        create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft_args("wf", "WF", Some("read_workspace_state"), None),
        )
        .await
        .expect("read_workspace_state needs no args");
    }

    /// A required arg that is PRESENT but blank (a whitespace-only string or an
    /// empty array/table) counts as missing — presence alone is not enough, since
    /// a `""` filename would export to nowhere. This is the branch that carries
    /// the real difference from a plain `contains_key` check.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn tool_call_with_a_blank_required_arg_is_invalid() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_allow(&["code"]),
        )));
        let mut args = toml::map::Map::new();
        // Empty array and a whitespace-only string: both present, both unusable.
        args.insert("data".to_string(), toml::Value::Array(Vec::new()));
        args.insert(
            "filename".to_string(),
            toml::Value::String("   ".to_string()),
        );
        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            tool_call_draft_args(
                "wf",
                "WF",
                Some("csv_export"),
                Some(toml::Value::Table(args)),
            ),
        )
        .await
        .expect_err("blank required args count as missing");
        let msg = err.to_string();
        assert!(
            msg.contains("data") && msg.contains("filename"),
            "both blank args are named: {msg}"
        );
    }

    // --- issue #661/#682: required config + condition labels on the draft path

    /// A minimal draft — trigger → condition `gate` → two outputs — with the
    /// gate's `config.field` and both branch labels parameterised. Since
    /// `parse_workflow` is now lenient on the #661 rules (issue #682), these are
    /// the graphs that prove the create/update path still enforces them strictly.
    fn condition_draft(
        field: Option<&str>,
        yes_label: Option<&str>,
        no_label: Option<&str>,
    ) -> RawWorkflow {
        let config = field.map(|field| {
            let mut table = toml::map::Map::new();
            table.insert("field".to_string(), toml::Value::String(field.to_string()));
            toml::Value::Table(table)
        });
        let node = |id: &str, kind: &str, config: Option<toml::Value>| RawNode {
            id: id.to_string(),
            kind: kind.to_string(),
            name: id.to_string(),
            summary: None,
            agent: None,
            schedule: None,
            config,
            on_error: None,
            retry: None,
            requires_approval: None,
            destination: None,
        };
        RawWorkflow {
            id: "wf".to_string(),
            name: "WF".to_string(),
            description: None,
            nodes: vec![
                node("start", "trigger", None),
                node("gate", "condition", config),
                node("a", "output", None),
                node("b", "output", None),
            ],
            edges: vec![
                RawEdge {
                    from: "start".to_string(),
                    to: "gate".to_string(),
                    label: None,
                },
                RawEdge {
                    from: "gate".to_string(),
                    to: "a".to_string(),
                    label: yes_label.map(str::to_string),
                },
                RawEdge {
                    from: "gate".to_string(),
                    to: "b".to_string(),
                    label: no_label.map(str::to_string),
                },
            ],
        }
    }

    /// A condition draft with no `config.field` is refused at author time even
    /// though `parse_workflow` would now let it load.
    #[tokio::test]
    async fn draft_condition_without_field_is_invalid() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            condition_draft(None, Some("yes"), Some("no")),
        )
        .await
        .expect_err("condition with no field");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("config.field"), "{err}");
    }

    /// A condition branch labeled anything but `yes`/`no` is refused at author
    /// time — the load path is lenient, so this rule now lives entirely here.
    #[tokio::test]
    async fn draft_condition_with_non_yes_no_label_is_invalid() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let err = create_company_workflow(
            &company,
            None,
            &store,
            None,
            condition_draft(Some("=item.ok"), Some("pass"), Some("no")),
        )
        .await
        .expect_err("off-vocabulary condition label");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("labeled `yes` or `no`"), "{err}");
    }

    /// The positive control: a condition with a `field` and `yes`/`no` branches
    /// is accepted by the same author path.
    #[tokio::test]
    async fn draft_condition_with_field_and_yes_no_labels_is_valid() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        create_company_workflow(
            &company,
            None,
            &store,
            None,
            condition_draft(Some("=item.approved"), Some("yes"), Some("no")),
        )
        .await
        .expect("a well-formed condition draft is accepted");
    }

    /// An http_request draft missing BOTH `method` and `url` reports both in one
    /// 400 — the draft path collects every required-config problem for a node,
    /// not just the first, so a human/model iterating hears the full list.
    #[tokio::test]
    async fn draft_http_request_missing_method_and_url_reports_both() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let draft = RawWorkflow {
            id: "wf".to_string(),
            name: "WF".to_string(),
            description: None,
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
                    id: "fetch".to_string(),
                    kind: "http_request".to_string(),
                    name: "Fetch".to_string(),
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
            edges: vec![RawEdge {
                from: "start".to_string(),
                to: "fetch".to_string(),
                label: None,
            }],
        };
        let err = create_company_workflow(&company, None, &store, None, draft)
            .await
            .expect_err("http_request with no method or url");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("config.method"), "{message}");
        assert!(message.contains("config.url"), "{message}");
    }

    // --- issue #276: arming, disarming, and the switch -----------------------

    /// A draft whose trigger fires on `cron`.
    fn scheduled_draft(id: &str, name: &str, cron: &str) -> RawWorkflow {
        let mut draft = valid_draft(id, name);
        draft.nodes[0].schedule = Some(cron.to_string());
        draft
    }

    /// A graph authored with a cron lands switched OFF.
    ///
    /// The half of the disarm rule OpenHuman does not have, and the one that
    /// matters most here: this function is also the orchestrator's
    /// `create_workflow` tool, so this assertion is what stops an agent putting a
    /// cron into production by writing one.
    async fn create_scheduled(
        company: &CompanyId,
        dir: &std::path::Path,
        store: &Arc<dyn CompanyStore>,
        log: &Arc<dyn EventLog>,
        id: &str,
        name: &str,
        cron: &str,
    ) -> WorkflowFile {
        create_company_workflow(
            company,
            Some(dir),
            store,
            Some(log),
            scheduled_draft(id, name, cron),
        )
        .await
        .expect("creates")
    }

    #[tokio::test]
    async fn creating_a_scheduled_workflow_leaves_it_switched_off() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let log = Arc::new(MemLog::default());
        let log_dyn: Arc<dyn EventLog> = log.clone();

        create_scheduled(
            &company,
            dir.path(),
            &store,
            &log_dyn,
            "digest",
            "Digest",
            "0 9 * * *",
        )
        .await;

        let saved = store.load(&company).await.unwrap().unwrap();
        assert!(
            !saved.workflow_enabled("digest"),
            "a created schedule must not be armed"
        );
        // Still a normal, complete workflow otherwise — pausing stops the
        // schedule, not the workflow.
        assert_eq!(saved.overlay_workflows.len(), 1);
        assert!(
            saved
                .manifest
                .workflows
                .enabled
                .contains(&"digest".to_string()),
            "the manifest declaration is untouched by the arming decision"
        );

        // Journaled, and journaled as the rule rather than as a person.
        let events = log.events.lock().unwrap();
        let disarm = events
            .iter()
            .find(|e| matches!(e, CompanyEvent::WorkflowEnabledChanged { .. }))
            .expect("a disarm is journaled");
        match disarm {
            CompanyEvent::WorkflowEnabledChanged {
                workflow_id,
                enabled,
                reason,
                by,
                ..
            } => {
                assert_eq!(workflow_id, "digest");
                assert!(!enabled);
                assert_eq!(*reason, WorkflowEnabledReason::Disarmed);
                assert!(by.is_none(), "the rule is not a person");
            }
            other => panic!("expected WorkflowEnabledChanged, got {other:?}"),
        }
    }

    /// A graph authored WITHOUT a cron is armed, because there is nothing to
    /// arm. The disarm rule must not make every manual workflow look paused in
    /// the console.
    #[tokio::test]
    async fn creating_a_manual_workflow_leaves_it_switched_on() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let log = Arc::new(MemLog::default());
        let log_dyn: Arc<dyn EventLog> = log.clone();

        create_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            Some(&log_dyn),
            valid_draft("greeter", "Greeter"),
        )
        .await
        .expect("creates");

        let saved = store.load(&company).await.unwrap().unwrap();
        assert!(saved.workflow_enabled("greeter"));
        assert!(
            saved.disabled_workflows.is_empty(),
            "a manual workflow must not be listed as paused"
        );
        assert!(
            !log.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, CompanyEvent::WorkflowEnabledChanged { .. })),
            "nothing changed, so nothing is journaled"
        );
    }

    /// **The safety-relevant half of issue #276.** An edit that turns a manual
    /// workflow into a scheduled one switches it off, so a cron introduced by an
    /// edit cannot fire before anyone has looked at it.
    #[tokio::test]
    async fn an_edit_that_adds_a_schedule_switches_the_workflow_off() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let log = Arc::new(MemLog::default());
        let log_dyn: Arc<dyn EventLog> = log.clone();

        create_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            Some(&log_dyn),
            valid_draft("greeter", "Greeter"),
        )
        .await
        .expect("creates");
        assert!(
            store
                .load(&company)
                .await
                .unwrap()
                .unwrap()
                .workflow_enabled("greeter"),
            "armed before the edit, so the assertion below is about the edit"
        );

        update_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            &revs(),
            Some(&log_dyn),
            scheduled_draft("greeter", "Greeter", "0 8 * * *"),
            None,
        )
        .await
        .expect("updates");

        let saved = store.load(&company).await.unwrap().unwrap();
        assert!(
            !saved.workflow_enabled("greeter"),
            "an edit that adds a schedule must disarm it"
        );
    }

    /// Correcting an already-armed workflow's cron leaves it armed.
    ///
    /// The deliberate limit of the rule: the reviewed decision is "automatic at
    /// all", and that one has not changed. Disarming here would put a re-enable
    /// click behind every typo fix.
    #[tokio::test]
    async fn changing_an_existing_schedule_does_not_disarm() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let log = Arc::new(MemLog::default());
        let log_dyn: Arc<dyn EventLog> = log.clone();

        create_scheduled(
            &company,
            dir.path(),
            &store,
            &log_dyn,
            "digest",
            "Digest",
            "0 9 * * *",
        )
        .await;
        // The operator reviews it and arms it.
        set_company_workflow_enabled(
            &company,
            Some(dir.path()),
            &store,
            Some(&log_dyn),
            "digest",
            true,
        )
        .await
        .expect("arms");

        update_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            &revs(),
            Some(&log_dyn),
            scheduled_draft("digest", "Digest", "0 3 * * *"),
            None,
        )
        .await
        .expect("updates");

        assert!(
            store
                .load(&company)
                .await
                .unwrap()
                .unwrap()
                .workflow_enabled("digest"),
            "a cron correction must not disarm an already-armed workflow"
        );
    }

    /// An edit never arms. A paused workflow stays paused across a re-save, even
    /// one that removes the schedule entirely — the rule has no arming
    /// direction, which is what stops it arming by accident.
    #[tokio::test]
    async fn an_edit_never_re_arms_a_paused_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let log = Arc::new(MemLog::default());
        let log_dyn: Arc<dyn EventLog> = log.clone();

        create_scheduled(
            &company,
            dir.path(),
            &store,
            &log_dyn,
            "digest",
            "Digest",
            "0 9 * * *",
        )
        .await;

        // Re-save with the schedule removed: still paused.
        update_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            &revs(),
            Some(&log_dyn),
            valid_draft("digest", "Digest"),
            None,
        )
        .await
        .expect("updates");

        assert!(
            !store
                .load(&company)
                .await
                .unwrap()
                .unwrap()
                .workflow_enabled("digest"),
            "removing a schedule must not re-arm the workflow"
        );
    }

    /// The operator switch round-trips, journals once, and is idempotent.
    #[tokio::test]
    async fn the_operator_switch_toggles_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let log = Arc::new(MemLog::default());
        let log_dyn: Arc<dyn EventLog> = log.clone();

        create_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            Some(&log_dyn),
            valid_draft("greeter", "Greeter"),
        )
        .await
        .expect("creates");
        let before = log.events.lock().unwrap().len();

        let changed = set_company_workflow_enabled(
            &company,
            Some(dir.path()),
            &store,
            Some(&log_dyn),
            "greeter",
            false,
        )
        .await
        .expect("pauses");
        assert!(changed, "the first toggle changes the record");
        assert!(
            !store
                .load(&company)
                .await
                .unwrap()
                .unwrap()
                .workflow_enabled("greeter")
        );

        // Setting the state it already holds writes nothing and journals nothing
        // — a double-click is a no-op, not a second audit entry.
        let changed = set_company_workflow_enabled(
            &company,
            Some(dir.path()),
            &store,
            Some(&log_dyn),
            "greeter",
            false,
        )
        .await
        .expect("no-ops");
        assert!(!changed);
        assert_eq!(
            log.events.lock().unwrap().len(),
            before + 1,
            "only the real transition is journaled"
        );

        // And back on, journaled as an operator decision rather than the rule.
        assert!(
            set_company_workflow_enabled(
                &company,
                Some(dir.path()),
                &store,
                Some(&log_dyn),
                "greeter",
                true,
            )
            .await
            .expect("arms")
        );
        let events = log.events.lock().unwrap();
        match events.last().expect("an event") {
            CompanyEvent::WorkflowEnabledChanged {
                enabled, reason, ..
            } => {
                assert!(enabled);
                assert_eq!(*reason, WorkflowEnabledReason::Operator);
            }
            other => panic!("expected WorkflowEnabledChanged, got {other:?}"),
        }
    }

    /// An id with no graph anywhere is a 404, and a manifest-`enabled` id with no
    /// body is a 409 — there is no schedule to switch off in either case.
    #[tokio::test]
    async fn toggling_an_id_with_no_graph_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let mut seed = record(&company, manifest_with_assistant());
        seed.manifest.workflows.enabled.push("ghost".to_string());
        let store = store_of(MemStore::seeded(seed));

        let err = set_company_workflow_enabled(
            &company,
            Some(dir.path()),
            &store,
            None,
            "nowhere",
            false,
        )
        .await
        .expect_err("unknown id");
        assert!(
            matches!(err, OpenCompanyError::CompanyNotFound(_)),
            "{err:?}"
        );

        let err =
            set_company_workflow_enabled(&company, Some(dir.path()), &store, None, "ghost", false)
                .await
                .expect_err("bodiless id");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
    }

    /// A stored graph that no longer parses is pausable, and is NOT reported as
    /// "provisioned by name only".
    ///
    /// The bodiless-409 message says the id was provisioned by name — true for a
    /// manifest entry with no graph, and false for a workflow whose saved body
    /// simply broke. An earlier revision collapsed both into one branch by
    /// swallowing `load_workflow_union`'s error, so a corrupt graph read back as
    /// the wrong explanation with no way to act on it.
    #[tokio::test]
    async fn a_workflow_whose_stored_graph_no_longer_parses_can_still_be_paused() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let mut seed = record(&company, manifest_with_assistant());
        seed.overlay_workflows.push(OverlayWorkflow {
            id: "broken".to_string(),
            toml: "id = \"broken\"\nname = \"Broken\"\n".to_string(), // no nodes: fails validation
        });
        seed.manifest.workflows.enabled.push("broken".to_string());
        let store = store_of(MemStore::seeded(seed));
        let log = Arc::new(MemLog::default());
        let log_dyn: Arc<dyn EventLog> = log.clone();

        assert!(
            set_company_workflow_enabled(
                &company,
                Some(dir.path()),
                &store,
                Some(&log_dyn),
                "broken",
                false,
            )
            .await
            .expect("an unreadable graph is still pausable")
        );
        assert!(
            !store
                .load(&company)
                .await
                .unwrap()
                .unwrap()
                .workflow_enabled("broken")
        );
        // Journals under the id, since there is no readable name to use.
        match log.events.lock().unwrap().last().expect("an event") {
            CompanyEvent::WorkflowEnabledChanged { name, .. } => assert_eq!(name, "broken"),
            other => panic!("expected WorkflowEnabledChanged, got {other:?}"),
        }
    }

    /// A **seed-defined** workflow can be paused, even though `PUT`/`DELETE`
    /// refuse it with a 409.
    ///
    /// This is the deliberate asymmetry, and the reason for it: an edit or a
    /// delete would be undone by the read path's seed precedence and by the boot
    /// rebuild, so refusing them is honesty about what the reader will do.
    /// Pausing writes to the record, leaves the source tree alone, and only ever
    /// removes capability — and without it an operator cannot stop a committed
    /// cron without a redeploy, which is issue #276(a) with extra steps.
    #[tokio::test]
    async fn a_seed_defined_workflow_can_be_paused_even_though_it_cannot_be_edited() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("seeded.toml"), SEED_TOML).unwrap();

        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));

        // Edit is refused, as it has been since #259 …
        let err = update_company_workflow(
            &company,
            Some(dir.path()),
            &store,
            &revs(),
            None,
            valid_draft("seeded", "Seeded flow"),
            None,
        )
        .await
        .expect_err("a seed-defined graph cannot be replaced");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");

        // … and the switch still works.
        assert!(
            set_company_workflow_enabled(
                &company,
                Some(dir.path()),
                &store,
                None,
                "seeded",
                false,
            )
            .await
            .expect("pauses a seed-defined workflow")
        );
        let saved = store.load(&company).await.unwrap().unwrap();
        assert!(!saved.workflow_enabled("seeded"));
        assert!(
            saved.overlay_workflows.is_empty(),
            "pausing must not materialize an overlay body for a seed graph"
        );
    }

    // --- issue #274: revision capture + rollback -----------------------------

    /// The overlay TOML currently stored for `wid`.
    async fn current_toml(store: &Arc<dyn CompanyStore>, company: &CompanyId, wid: &str) -> String {
        store
            .load(company)
            .await
            .unwrap()
            .unwrap()
            .overlay_workflows
            .into_iter()
            .find(|w| w.id == wid)
            .expect("overlay body exists")
            .toml
    }

    /// Creates `greeter`, then returns `(store, revisions, body_a)` ready to edit.
    async fn seeded_greeter() -> (Arc<dyn CompanyStore>, Arc<MemRevisions>, String) {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        create_company_workflow(
            &company,
            None,
            &store,
            None,
            valid_draft("greeter", "Greeter"),
        )
        .await
        .expect("create");
        let body_a = current_toml(&store, &company, "greeter").await;
        (store, Arc::new(MemRevisions::default()), body_a)
    }

    #[tokio::test]
    async fn update_snapshots_the_prior_body_exactly_once() {
        let company = CompanyId::new("acme");
        let (store, revs, body_a) = seeded_greeter().await;
        let revs_dyn: Arc<dyn WorkflowRevisionStore> = revs.clone();

        // Edit the description so the rendered body differs from A.
        let mut edit = valid_draft("greeter", "Greeter");
        edit.description = Some("edited once".to_string());
        update_company_workflow(&company, None, &store, &revs_dyn, None, edit, None)
            .await
            .expect("update");

        let history = revs.list_revisions(&company, "greeter").await.unwrap();
        assert_eq!(history.len(), 1, "one edit captures one snapshot");
        assert_eq!(
            history[0].toml, body_a,
            "the snapshot must hold the prior body byte-for-byte"
        );
        assert_eq!(history[0].workflow_id, "greeter");
        assert_eq!(history[0].name, "Greeter");
    }

    #[tokio::test]
    async fn a_byte_identical_resave_snapshots_nothing() {
        let company = CompanyId::new("acme");
        let (store, revs, _body_a) = seeded_greeter().await;
        let revs_dyn: Arc<dyn WorkflowRevisionStore> = revs.clone();

        // Re-save the exact same graph: the rendered body is byte-identical, so
        // there is nothing to lose and no snapshot is taken.
        update_company_workflow(
            &company,
            None,
            &store,
            &revs_dyn,
            None,
            valid_draft("greeter", "Greeter"),
            None,
        )
        .await
        .expect("no-op resave");
        assert!(
            revs.list_revisions(&company, "greeter")
                .await
                .unwrap()
                .is_empty(),
            "a byte-identical re-save must not snapshot"
        );
    }

    #[tokio::test]
    async fn the_ring_prunes_the_oldest_past_the_cap() {
        use crate::ports::workflow_revisions::MAX_WORKFLOW_REVISIONS;
        let company = CompanyId::new("acme");
        let (store, revs, _body_a) = seeded_greeter().await;
        let revs_dyn: Arc<dyn WorkflowRevisionStore> = revs.clone();

        // MAX+1 distinct edits capture MAX+1 prior bodies; the ring keeps MAX.
        for i in 0..=MAX_WORKFLOW_REVISIONS {
            let mut edit = valid_draft("greeter", "Greeter");
            edit.description = Some(format!("edit {i}"));
            update_company_workflow(&company, None, &store, &revs_dyn, None, edit, None)
                .await
                .expect("update");
        }
        let history = revs.list_revisions(&company, "greeter").await.unwrap();
        assert_eq!(
            history.len(),
            MAX_WORKFLOW_REVISIONS,
            "the ring is capped at MAX_WORKFLOW_REVISIONS"
        );
    }

    #[tokio::test]
    async fn rollback_restores_the_body_and_is_itself_undoable() {
        let company = CompanyId::new("acme");
        let (store, revs, body_a) = seeded_greeter().await;
        let revs_dyn: Arc<dyn WorkflowRevisionStore> = revs.clone();

        // A → edit → B. One revision now holds A.
        let mut edit_b = valid_draft("greeter", "Greeter");
        edit_b.description = Some("this is B".to_string());
        update_company_workflow(&company, None, &store, &revs_dyn, None, edit_b, None)
            .await
            .expect("edit to B");
        let body_b = current_toml(&store, &company, "greeter").await;
        let rev_a = revs.list_revisions(&company, "greeter").await.unwrap()[0]
            .id
            .clone();

        // Restore A: the live body becomes A again, and B is captured as the new
        // newest revision — so the rollback can itself be rolled back.
        let restored = rollback_company_workflow(
            &company, None, &store, &revs_dyn, None, "greeter", &rev_a, None,
        )
        .await
        .expect("rollback to A");
        assert_eq!(restored.description.as_deref(), Some("A tiny graph."));
        assert_eq!(current_toml(&store, &company, "greeter").await, body_a);

        let history = revs.list_revisions(&company, "greeter").await.unwrap();
        assert_eq!(
            history.len(),
            2,
            "the restore captured the body it replaced"
        );
        assert_eq!(history[0].toml, body_b, "B is the newest snapshot now");

        // …and restoring that B snapshot puts B back.
        let rev_b = history[0].id.clone();
        rollback_company_workflow(
            &company, None, &store, &revs_dyn, None, "greeter", &rev_b, None,
        )
        .await
        .expect("rollback the rollback");
        assert_eq!(current_toml(&store, &company, "greeter").await, body_b);
    }

    #[tokio::test]
    async fn rollback_of_a_revision_naming_a_removed_teammate_is_400_and_leaves_current() {
        let company = CompanyId::new("acme");
        let (store, revs, _body_a) = seeded_greeter().await;
        let revs_dyn: Arc<dyn WorkflowRevisionStore> = revs.clone();

        // Edit to capture a revision (which still names `assistant`), then set B.
        let mut edit_b = valid_draft("greeter", "Greeter");
        edit_b.description = Some("B, assistant still valid here".to_string());
        update_company_workflow(&company, None, &store, &revs_dyn, None, edit_b, None)
            .await
            .expect("edit to B");
        let body_b = current_toml(&store, &company, "greeter").await;
        let rev_a = revs.list_revisions(&company, "greeter").await.unwrap()[0]
            .id
            .clone();

        // Remove `assistant` from the roster: the captured revision now names a
        // teammate the current record does not know about.
        let mut rec = store.load(&company).await.unwrap().unwrap();
        rec.manifest = toml::from_str("[company]\nname = \"Acme\"\n").unwrap();
        store.save(&rec).await.unwrap();

        let err = rollback_company_workflow(
            &company, None, &store, &revs_dyn, None, "greeter", &rev_a, None,
        )
        .await
        .expect_err("a revision naming a removed teammate must not restore");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
        // The current body is untouched — a rejected rollback writes nothing.
        assert_eq!(current_toml(&store, &company, "greeter").await, body_b);
    }

    #[tokio::test]
    async fn rollback_with_a_stale_expected_version_is_409_and_writes_nothing() {
        let company = CompanyId::new("acme");
        let (store, revs, body_a) = seeded_greeter().await;
        let revs_dyn: Arc<dyn WorkflowRevisionStore> = revs.clone();

        let mut edit_b = valid_draft("greeter", "Greeter");
        edit_b.description = Some("B".to_string());
        update_company_workflow(&company, None, &store, &revs_dyn, None, edit_b, None)
            .await
            .expect("edit to B");
        let body_b = current_toml(&store, &company, "greeter").await;
        let rev_a = revs.list_revisions(&company, "greeter").await.unwrap()[0]
            .id
            .clone();

        let err = rollback_company_workflow(
            &company,
            None,
            &store,
            &revs_dyn,
            None,
            "greeter",
            &rev_a,
            Some("deadbeef-not-the-current-token"),
        )
        .await
        .expect_err("a stale token must refuse the restore");
        assert!(matches!(err, OpenCompanyError::Conflict(_)), "{err:?}");
        assert_eq!(
            current_toml(&store, &company, "greeter").await,
            body_b,
            "a 409 must leave the live body unchanged"
        );

        // The response token of a successful restore is the hash of the restored
        // body — echo the CURRENT token and the restore lands.
        let current = workflow_version(&body_b);
        let restored = rollback_company_workflow(
            &company,
            None,
            &store,
            &revs_dyn,
            None,
            "greeter",
            &rev_a,
            Some(&current),
        )
        .await
        .expect("the current token lets the restore through");
        // The restored live body is the captured A body, and the token the write
        // response carries is that body's hash.
        let restored_toml = current_toml(&store, &company, "greeter").await;
        assert_eq!(restored_toml, body_a, "restoring A puts A back verbatim");
        assert_eq!(restored.id, "greeter");
        assert_eq!(
            workflow_version(&restored_toml),
            workflow_version(&body_a),
            "the response token is the restored body's hash"
        );
    }

    #[tokio::test]
    async fn rollback_disarms_a_restored_schedule() {
        let company = CompanyId::new("acme");
        let store = store_of(MemStore::seeded(record(
            &company,
            manifest_with_assistant(),
        )));
        let revs: Arc<MemRevisions> = Arc::new(MemRevisions::default());
        let revs_dyn: Arc<dyn WorkflowRevisionStore> = revs.clone();

        // A scheduled workflow lands disarmed on create (issue #276); arm it, so
        // the "restored cron re-arms" hazard is real to test.
        let mut scheduled = valid_draft("greeter", "Greeter");
        scheduled.nodes[0].schedule = Some("0 9 * * *".to_string());
        create_company_workflow(&company, None, &store, None, scheduled)
            .await
            .expect("create scheduled");
        set_company_workflow_enabled(&company, None, &store, None, "greeter", true)
            .await
            .expect("arm it");

        // Edit the schedule away — the workflow stays armed (removal never
        // disarms), and the scheduled body is captured as a revision.
        let mut unscheduled = valid_draft("greeter", "Greeter");
        unscheduled.description = Some("no schedule now".to_string());
        update_company_workflow(&company, None, &store, &revs_dyn, None, unscheduled, None)
            .await
            .expect("remove schedule");
        assert!(
            store
                .load(&company)
                .await
                .unwrap()
                .unwrap()
                .workflow_enabled("greeter"),
            "removing a schedule must not disarm"
        );
        let rev_scheduled = revs.list_revisions(&company, "greeter").await.unwrap()[0]
            .id
            .clone();

        // Restoring the scheduled body re-introduces a cron the live graph lacked
        // → it lands switched off pending review.
        rollback_company_workflow(
            &company,
            None,
            &store,
            &revs_dyn,
            None,
            "greeter",
            &rev_scheduled,
            None,
        )
        .await
        .expect("restore scheduled body");
        assert!(
            !store
                .load(&company)
                .await
                .unwrap()
                .unwrap()
                .workflow_enabled("greeter"),
            "a restored schedule must land disarmed (issue #276)"
        );
    }

    #[tokio::test]
    async fn rollback_unknown_revision_is_not_found() {
        let company = CompanyId::new("acme");
        let (store, revs, _body_a) = seeded_greeter().await;
        let revs_dyn: Arc<dyn WorkflowRevisionStore> = revs.clone();
        let err = rollback_company_workflow(
            &company,
            None,
            &store,
            &revs_dyn,
            None,
            "greeter",
            "no-such-rev",
            None,
        )
        .await
        .expect_err("unknown revision");
        assert!(
            matches!(err, OpenCompanyError::CompanyNotFound(_)),
            "{err:?}"
        );
    }
}
