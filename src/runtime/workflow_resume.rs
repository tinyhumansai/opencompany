//! Issue #395: continuing a workflow run the operator has signed off.
//!
//! # The hole this closes
//!
//! A workflow node marked `requires_approval` pauses the run: the tinyflows
//! engine settles with the gate's node id on its outcome's
//! `pending_approvals`. Those ids
//! reached exactly two places — the run route's HTTP response and the
//! `WorkflowRunFinished` journal line — and **neither is an approval**. The
//! Approvals page reads the runtime journal's `pending()`, which lists parked
//! [`Effect`]s, so it was empty by construction however many gates a run
//! paused on. An operator watching a run they expected to be gated saw "All
//! clear" while the run sat unresumable forever.
//!
//! The run side of the fix is `park_pending_gates` in the workflow runner —
//! every pending gate becomes a parked [`WORKFLOW_APPROVE_KIND`] effect (a
//! `#[cfg(feature = "openhuman")]` module, which is why it is not linked here).
//! This module is the other half:
//! what approving one of those cards actually *does*.
//!
//! # Resume is a re-run, because a paused run is settled
//!
//! This is the fact the whole design turns on, and it is easy to get wrong.
//! A paused tinyflows run is **not suspended**. Nothing holds a task, a
//! connection, or a continuation — the engine returns, the future completes,
//! and the run is over. `engine::resume` is not a resumption primitive either:
//! it unions the newly-approved gate ids into `input["approvals"]` and calls
//! `run()` again. The gate node reads that array (at
//! `state["run"]["trigger"]["approvals"]`) and, finding its own id, proceeds
//! instead of pausing.
//!
//! So the host can do exactly that itself, with no new engine entry point: put
//! the node id into the trigger input's `approvals` array and start an ordinary
//! run. Three things fall out of it for free —
//!
//! * **restart durability.** The parked effect carries the whole input, so a
//!   host that dies between the park and the approval loses nothing: journal
//!   replay rehydrates the card and approving it still resumes.
//! * **no live-run registry.** The alternative — holding a `ResumableRun` in a
//!   map keyed by run id — is in-memory, so it dies on restart while the
//!   durable parked approval outlives it. The operator would approve a card
//!   pointing at a continuation that no longer exists.
//! * **observability and cancellation.** The re-run is a normal supervised run,
//!   so it journals per-node progress and is stoppable, which neither
//!   `ResumableRun::resume` nor `resume_with_checkpointer` currently offers
//!   alongside an observer.
//!
//! # What it costs, stated plainly
//!
//! **Upstream nodes re-execute.** That is the engine's documented semantic for
//! resume, not something added here, but it is real: agent nodes re-spend
//! tokens, and a reached `output` node **re-delivers** — a warm-recipient email
//! will send a second time, because the established-thread check is state-based
//! rather than run-based. This is acceptable for v1 because a gate normally
//! sits *before* the side-effecting node it is gating, which is the entire
//! reason to author one. It is not acceptable silently, which is why it is
//! written here, in the PR, and nowhere hidden.
//!
//! # At-most-once, deny, and expiry
//!
//! Nothing extra is owed. The resume arm hangs off `perform_effect`, which only
//! runs under [`execute_effect_once`](crate::runtime::cycle)'s
//! `approval:<id>` key, so a double-approve spawns one run. A denied or
//! TTL-expired approval never reaches `perform_effect` at all, and since the
//! paused run is already settled, "nothing runs" is the complete outcome — no
//! task to cancel, no connection to close.

use serde_json::{Map, Value};

use crate::Result;
use crate::company::runtime::CompanyRuntime;
use crate::company::load_workflow_union;
use crate::error::OpenCompanyError;
use crate::ports::types::Effect;
use crate::runtime::workflow_spawn::WorkflowSpawn;

/// The effect kind a paused `requires_approval` node parks as (issue #395).
///
/// A constant rather than a literal because three places key on it — the park
/// in the workflow runner, the dedupe that keeps a re-run from stacking a
/// second card for the same gate, and the `perform_effect` arm that resumes it.
/// Three copies of a magic string is three chances for one of them to drift and
/// fail silently, which for this kind means an approval nobody acts on.
pub const WORKFLOW_APPROVE_KIND: &str = "workflow.approve";

/// The payload key holding the workflow whose run paused.
pub const PAYLOAD_WORKFLOW_ID: &str = "workflow_id";
/// The payload key holding the gate node awaiting sign-off.
pub const PAYLOAD_NODE_ID: &str = "node_id";
/// The payload key holding the trigger input the paused run was started with.
pub const PAYLOAD_INPUT: &str = "input";

/// Builds the effect a paused gate parks as.
///
/// Shared by the runner (which parks it) and this module's tests, so the shape
/// the resume arm reads is the shape the park writes — by construction rather
/// than by two matching literals.
///
/// [`EffectGroup::Other`](crate::ports::types::EffectGroup::Other) with
/// `agent: None` is the honest classification and it decides two things
/// downstream. `agent: None` routes the approval to
/// `execute_effect_once` — the native path — rather than minting a tool grant,
/// which is right: no teammate asked for this and there is no tool call to
/// re-issue. And because
/// [`ApprovalSummary::broadly_grantable`](crate::runtime::ApprovalSummary) requires an agent,
/// the console never offers "let it do this for a period" on a card where that
/// would mean nothing.
pub fn gate_effect(workflow_id: &str, node_id: &str, input: &Value, run_id: &str) -> Effect {
    Effect {
        kind: WORKFLOW_APPROVE_KIND.to_string(),
        group: crate::ports::types::EffectGroup::Other,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: false,
        payload: serde_json::json!({
            PAYLOAD_WORKFLOW_ID: workflow_id,
            PAYLOAD_NODE_ID: node_id,
            // The whole trigger input, so the parked card is self-contained and
            // a resume needs nothing but the journal. This is what makes
            // approve-after-restart work.
            PAYLOAD_INPUT: input.clone(),
        }),
        // Native, not a teammate's tool call — see the doc above.
        agent: None,
        // The run that paused. Not the run the approval will start (which does
        // not exist yet); this is the causal ancestor, and it is what lets the
        // console tie the card back to the run history the operator was
        // watching.
        run_id: Some(run_id.to_string()),
    }
}

/// Whether two parked gate effects describe the **same** pending decision.
///
/// Identity is `(kind, workflow_id, node_id, input)` — all four, and each earns
/// its place. Two runs of the same graph with *different* inputs are genuinely
/// two decisions and must both be asked about; two runs with the same input
/// reaching the same gate are one decision asked twice, and stacking them turns
/// a re-runnable workflow into an approvals queue the operator learns to
/// rubber-stamp.
///
/// `run_id` is deliberately **not** part of it: it differs by construction on
/// every re-run, so including it would make the dedupe a no-op.
fn is_same_gate(a: &Effect, b: &Effect) -> bool {
    a.kind == b.kind
        && a.kind == WORKFLOW_APPROVE_KIND
        && [PAYLOAD_WORKFLOW_ID, PAYLOAD_NODE_ID, PAYLOAD_INPUT]
            .iter()
            .all(|key| a.payload.get(*key) == b.payload.get(*key))
}

/// True when `effect` names a gate the journal is already holding a card for.
pub fn already_parked(journal: &crate::runtime::journal::RuntimeJournal, effect: &Effect) -> bool {
    journal
        .pending()
        .iter()
        .any(|parked| is_same_gate(&parked.effect, effect))
}

/// Resumes the workflow run a parked [`WORKFLOW_APPROVE_KIND`] effect describes,
/// by starting a fresh supervised run with the gate approved.
///
/// # Why a **new** supervised run rather than a continuation of the old one
///
/// Because there is no old one to continue — see the module docs. A re-run is a
/// new causal root, so it gets its own [`RunSupervisor`](crate::runtime::RunSupervisor)
/// registration (it must be stoppable like any other run) and its own
/// `WorkflowRunFinished` (the operator gets two rows: the run that paused, and
/// the run that finished the job). Reusing the paused run's id would produce a
/// second finish for one id and make the run history self-contradictory.
///
/// # Errors
///
/// Propagated rather than swallowed, and that is a deliberate choice about who
/// hears about the failure. `execute_effect_once` has already committed the
/// approval by the time this runs, so the runtime will never retry it — if the
/// graph has since been deleted, or this build has no workflow execution, the
/// operator must be told at the moment they click Approve rather than left
/// watching for a run that will never appear. Same stance the `email.send` arm
/// beside it takes.
pub async fn resume_from_effect(runtime: &CompanyRuntime, effect: &Effect) -> Result<()> {
    let workflow_id = required_str(effect, PAYLOAD_WORKFLOW_ID)?;
    let node_id = required_str(effect, PAYLOAD_NODE_ID)?;
    let input = effect.payload.get(PAYLOAD_INPUT).cloned().unwrap_or(Value::Null);

    // Through the runtime's own accessor so a build without workflow execution
    // gives an honest error instead of a compile-time edge — this module is in
    // the default build, where `src/workflows` does not exist at all.
    let Some(runner) = runtime.workflow_runner().cloned() else {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "approved the gate on workflow `{workflow_id}`, but this runtime has no workflow \
             execution wired, so there is nothing to continue"
        )));
    };

    // The same seed ∪ overlay union the run route loads through, so a graph
    // authored on a hosted tenant (no source directory) resumes exactly like a
    // committed one.
    let overlays = runtime
        .store()
        .load(runtime.id())
        .await?
        .map(|record| record.overlay_workflows)
        .unwrap_or_default();
    let workflow = load_workflow_union(runtime.source_dir(), &overlays, workflow_id)?.ok_or_else(
        || {
            OpenCompanyError::CompanyNotFound(format!(
                "workflow {workflow_id} (it was approved, but the graph no longer exists)"
            ))
        },
    )?;

    let input = with_approval(input, node_id);
    // The handle is dropped on purpose. The task holds its own guard, journals
    // its own outcome and deregisters itself; awaiting it here would hold the
    // approvals request open for the length of a whole workflow run, which is
    // the drop-safety failure issue #380 already paid for once.
    let (run_id, _handle) = WorkflowSpawn::new(runtime, runner).spawn(workflow, input, false);
    tracing::info!(
        company = %runtime.id(),
        workflow = %workflow_id,
        node = %node_id,
        %run_id,
        "workflow: an approved gate started a continuation run; upstream nodes re-execute"
    );
    Ok(())
}

/// Unions `node_id` into the trigger input's `approvals` array.
///
/// Mirrors `engine::resume`'s own merge, including its tolerances: a non-object
/// input is replaced by a fresh object carrying just the approvals (there is
/// nowhere else to put them), a non-array or absent `approvals` starts an empty
/// set rather than panicking, non-string entries are dropped, and an id already
/// present is not duplicated.
///
/// Preserving prior approvals is what makes a graph with **two** gates work:
/// approving the second must not un-approve the first, or the re-run pauses at
/// the gate the operator already cleared and the workflow can never finish.
fn with_approval(input: Value, node_id: &str) -> Value {
    let mut approvals: Vec<String> = input
        .get("approvals")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if !approvals.iter().any(|id| id == node_id) {
        approvals.push(node_id.to_string());
    }

    match input {
        Value::Object(mut map) => {
            map.insert("approvals".to_string(), serde_json::json!(approvals));
            Value::Object(map)
        }
        _ => {
            let mut map = Map::new();
            map.insert("approvals".to_string(), serde_json::json!(approvals));
            Value::Object(map)
        }
    }
}

/// Reads a required string off the parked payload, naming the key when it is
/// missing — a parked effect this malformed is a host bug, and the operator
/// clicking Approve needs to know it is not their graph.
fn required_str<'e>(effect: &'e Effect, key: &str) -> Result<&'e str> {
    effect
        .payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            OpenCompanyError::InvalidRequest(format!(
                "this approval is a workflow gate but its record carries no `{key}`, so there is \
                 no run to continue"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effect(workflow: &str, node: &str, input: Value) -> Effect {
        gate_effect(workflow, node, &input, "run-1")
    }

    #[test]
    fn the_parked_effect_is_native_and_self_contained() {
        let e = effect("digest", "gate", serde_json::json!({ "request": "topic" }));
        assert_eq!(e.kind, WORKFLOW_APPROVE_KIND);
        // Native: routes to `execute_effect_once`, not to a tool grant — and
        // keeps the console from offering a standing permission that would mean
        // nothing.
        assert!(e.agent.is_none());
        // Self-contained: everything a resume needs survives a restart in the
        // journal, with no live state anywhere.
        assert_eq!(e.payload[PAYLOAD_WORKFLOW_ID], "digest");
        assert_eq!(e.payload[PAYLOAD_NODE_ID], "gate");
        assert_eq!(e.payload[PAYLOAD_INPUT]["request"], "topic");
        assert_eq!(e.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn the_same_gate_on_the_same_input_is_one_decision() {
        let a = effect("digest", "gate", serde_json::json!({ "request": "x" }));
        let mut b = effect("digest", "gate", serde_json::json!({ "request": "x" }));
        // A re-run mints a new run id; that must not make it a second card.
        b.run_id = Some("run-2".to_string());
        assert!(is_same_gate(&a, &b));
    }

    #[test]
    fn a_different_gate_input_or_workflow_is_a_different_decision() {
        let base = effect("digest", "gate", serde_json::json!({ "request": "x" }));
        for other in [
            effect("digest", "second-gate", serde_json::json!({ "request": "x" })),
            effect("other", "gate", serde_json::json!({ "request": "x" })),
            effect("digest", "gate", serde_json::json!({ "request": "y" })),
        ] {
            assert!(
                !is_same_gate(&base, &other),
                "these are two decisions and both must be asked about: {other:?}"
            );
        }
    }

    #[test]
    fn approving_a_gate_adds_it_to_the_trigger_inputs_approvals() {
        let out = with_approval(serde_json::json!({ "request": "topic" }), "gate");
        assert_eq!(out["request"], "topic", "the original input is preserved");
        assert_eq!(out["approvals"], serde_json::json!(["gate"]));
    }

    #[test]
    fn a_second_gate_does_not_un_approve_the_first() {
        // The two-gate graph. Without the union the re-run pauses at the gate
        // the operator already cleared and the workflow can never finish.
        let first = with_approval(serde_json::json!({ "request": "topic" }), "gate-a");
        let second = with_approval(first, "gate-b");
        assert_eq!(second["approvals"], serde_json::json!(["gate-a", "gate-b"]));
    }

    #[test]
    fn approving_the_same_gate_twice_does_not_duplicate_it() {
        let once = with_approval(serde_json::json!({}), "gate");
        let twice = with_approval(once, "gate");
        assert_eq!(twice["approvals"], serde_json::json!(["gate"]));
    }

    #[test]
    fn a_non_object_input_still_yields_a_resumable_one() {
        // `engine::resume`'s own tolerance: there is nowhere to put the array on
        // a bare string or null, so it becomes a fresh object holding just the
        // approvals rather than a panic or a lost gate.
        for input in [
            Value::Null,
            serde_json::json!("a bare topic"),
            serde_json::json!(42),
            serde_json::json!(["not", "an", "object"]),
            // A malformed `approvals` starts an empty set rather than erroring.
            serde_json::json!({ "approvals": "gate" }),
        ] {
            let out = with_approval(input.clone(), "gate");
            assert_eq!(
                out["approvals"],
                serde_json::json!(["gate"]),
                "input {input} must still produce a resumable trigger"
            );
        }
    }

    #[test]
    fn non_string_entries_in_a_prior_approvals_array_are_dropped() {
        let out = with_approval(serde_json::json!({ "approvals": ["a", 7, null] }), "b");
        assert_eq!(out["approvals"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn a_malformed_gate_record_names_the_key_it_is_missing() {
        let mut e = effect("digest", "gate", Value::Null);
        e.payload = serde_json::json!({ PAYLOAD_NODE_ID: "gate" });
        let err = required_str(&e, PAYLOAD_WORKFLOW_ID).expect_err("must refuse");
        assert!(err.to_string().contains(PAYLOAD_WORKFLOW_ID), "{err}");

        // A blank id is as unusable as a missing one and must not reach the
        // loader as an empty filename.
        e.payload = serde_json::json!({ PAYLOAD_WORKFLOW_ID: "   " });
        assert!(required_str(&e, PAYLOAD_WORKFLOW_ID).is_err());
    }
}
