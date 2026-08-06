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
//! tokens on every continuation. A gate normally sits *before* the
//! side-effecting node it is gating — which is the entire reason to author one
//! — so for most graphs the cost is tokens and wall-clock. It is not acceptable
//! silently, which is why it is written here, in the parked card's own `note`
//! payload, and nowhere hidden.
//!
//! # The one cost that was not acceptable: re-delivery (issue #438)
//!
//! A reached `output` node used to **deliver again** on every continuation. The
//! established-recipient check is state-based rather than run-based, so a warm
//! recipient was simply mailed the same report a second time the moment an
//! operator approved a *later* gate — a side effect that left the process and
//! reached a real person, caused by clicking Approve.
//!
//! The fix is a **delivery ledger** carried in the parked card and threaded into
//! the continuation's trigger input under [`CONTINUATION_DELIVERED_KEY`]:
//! `{node, kind}` for every report this lineage has already sent (`Sent`) or
//! parked (`Pending` — the card is durable, and approving it sends, so it counts
//! as delivered). Delivery skips a listed node with
//! [`DeliveryReason::AlreadyDelivered`](crate::ports::DeliveryReason::AlreadyDelivered)
//! and dispatches nothing. The ledger is
//! *unioned* with whatever the incoming input already carried, so a graph with
//! two gates accumulates across both resumes rather than forgetting the first.
//!
//! Carrying it on the card rather than in a side table is the same choice the
//! input itself makes: the card stays self-contained, so the guard survives a
//! restart exactly like the resume does.
//!
//! **The honest limit.** The ledger is per `output` node, not per recipient. An
//! `owner` destination that fanned out to three admins and failed on the third
//! is recorded as delivered, and the continuation will not retry that third
//! address. Re-mailing two people to reach one is the worse outcome, so this is
//! deliberate rather than an oversight — a partial fan-out is repaired from the
//! run history, not by a resume.
//!
//! # At-most-once, deny, and expiry
//!
//! Nothing extra is owed. The resume arm hangs off `perform_effect`, which only
//! runs under [`execute_effect_once`](crate::runtime::cycle)'s
//! `approval:<id>` key, so a double-approve spawns one run. A denied or
//! TTL-expired approval never reaches `perform_effect` at all, and since the
//! paused run is already settled, "nothing runs" is the complete outcome — no
//! task to cancel, no connection to close.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Result;
use crate::company::load_workflow_union;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::Effect;
use crate::ports::{DeliveryReport, DeliveryStatus};
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
/// The payload key holding this lineage's delivery ledger (issue #438) — the
/// reports a continuation must NOT send again.
pub const PAYLOAD_DELIVERED: &str = "delivered";
/// The payload key holding the plain-prose statement of what approving costs.
pub const PAYLOAD_NOTE: &str = "note";

/// The reserved trigger-input key the delivery ledger rides into a continuation
/// run under (issue #438).
///
/// Reserved, and shaped so nobody authors it by accident: it is threaded by the
/// host, read by `deliver_outputs`, and never by the engine or a graph author.
/// It is stripped before two parked gates are compared — see `is_same_gate` — because a continuation's input differs from the paused
/// run's by exactly this key, and letting that difference count would make
/// every continuation gate a "new" decision and stack a duplicate card.
pub const CONTINUATION_DELIVERED_KEY: &str = "__opencompany_delivered";

/// What approving a workflow gate actually does, in the operator's own terms.
///
/// This rides the card as [`PAYLOAD_NOTE`] rather than living only in a design
/// doc, because the person deciding is the one who pays the cost: approving is
/// not "let the run continue from here", it re-runs the graph from the trigger.
/// Prose, not a code reference — the reader is an operator looking at an
/// Approvals card.
pub const CONTINUATION_NOTE: &str = "Approving this re-runs the whole workflow from the start — every step before this gate runs \
     again, and any agent steps spend tokens again. Reports this run already delivered will not be \
     sent a second time.";

/// One `output` node whose report a run in this lineage has already delivered.
///
/// `kind` rides along beside `node` so the record says *what* was sent where —
/// a card an operator reads, and a run history a reviewer reads, both want
/// "the owner summary already went out", not a bare node id. Matching is on
/// `node`: an output node has exactly one destination, so the id is the
/// identity and the kind is the description.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredReport {
    /// The `output` node whose report was delivered.
    pub node: String,
    /// The destination kind it was delivered to (`owner` / `email` / `channel`).
    pub kind: String,
}

/// The reports this lineage has already delivered, read off a trigger input.
///
/// Tolerant by construction — a missing key, a non-array, or a malformed row
/// yields "nothing known to be delivered", which is the pre-#438 behaviour
/// (deliver it). Failing loudly here would turn a garbled continuation into a
/// run that delivers nothing at all, which is the worse error.
pub fn delivered_in_input(input: &Value) -> Vec<DeliveredReport> {
    input
        .get(CONTINUATION_DELIVERED_KEY)
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| serde_json::from_value(row.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The ledger a gate parked on this run should carry: what the run just
/// delivered, unioned with what its own trigger input already listed.
///
/// The union is what makes a **two-gate** graph correct. Approving the first
/// gate starts a continuation that skips the already-delivered report — and
/// then pauses at the second gate. If that second card carried only what the
/// continuation itself delivered (nothing, since it skipped), approving it
/// would deliver the report for real. The ledger has to accumulate down the
/// lineage, not restart at each hop.
///
/// `Sent` and `Pending` both count. `Pending` is a parked cold-recipient card:
/// journal-backed, survives a restart, and approving it sends. Treating it as
/// undelivered would re-park an identical card on every continuation and
/// approving both would send twice — `park_cold_recipient` has no dedupe of its
/// own. `Skipped` / `Denied` / `Failed` deliberately do not count: nothing left
/// the process, so a continuation is free to try again.
fn delivery_ledger(input: &Value, deliveries: &[DeliveryReport]) -> Vec<DeliveredReport> {
    let mut ledger = delivered_in_input(input);
    for report in deliveries {
        if !matches!(
            report.status,
            DeliveryStatus::Sent | DeliveryStatus::Pending
        ) {
            continue;
        }
        let entry = DeliveredReport {
            node: report.node.clone(),
            kind: report.kind.clone(),
        };
        // An `owner` destination fans out to one row per admin, so the same
        // node appears several times; the ledger holds it once.
        if !ledger.contains(&entry) {
            ledger.push(entry);
        }
    }
    ledger
}

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
///
/// `deliveries` is what the run that paused actually delivered (issue #438).
/// It is folded into the card's ledger rather than looked up later for the same
/// reason the input is copied in: a card that needs a side table is a card that
/// stops working after a restart.
pub fn gate_effect(
    workflow_id: &str,
    node_id: &str,
    input: &Value,
    run_id: &str,
    deliveries: &[DeliveryReport],
) -> Effect {
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
            // What must NOT be sent again when this card is approved.
            PAYLOAD_DELIVERED: delivery_ledger(input, deliveries),
            // What approving costs, in the operator's own terms.
            PAYLOAD_NOTE: CONTINUATION_NOTE,
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
/// every re-run, so including it would make the dedupe a no-op. Neither is the
/// [`PAYLOAD_DELIVERED`] ledger, nor the [`CONTINUATION_DELIVERED_KEY`] the
/// input carries it under (issue #438) — both differ by construction between a
/// paused run and the continuation it started, and both describe what has
/// *already happened* rather than what is being decided. Counting either would
/// make every continuation gate read as a new decision, which is precisely the
/// duplicate-card failure this function exists to prevent.
fn is_same_gate(a: &Effect, b: &Effect) -> bool {
    a.kind == b.kind
        && a.kind == WORKFLOW_APPROVE_KIND
        && [PAYLOAD_WORKFLOW_ID, PAYLOAD_NODE_ID]
            .iter()
            .all(|key| a.payload.get(*key) == b.payload.get(*key))
        && decided_input(a) == decided_input(b)
}

/// The part of a parked gate's trigger input that identifies the *decision*:
/// everything except the host-threaded delivery ledger.
fn decided_input(effect: &Effect) -> Option<Value> {
    effect
        .payload
        .get(PAYLOAD_INPUT)
        .cloned()
        .map(without_ledger)
}

/// `input` with the reserved delivery-ledger key removed. A non-object input is
/// returned as-is — there is nothing to strip.
fn without_ledger(mut input: Value) -> Value {
    if let Value::Object(map) = &mut input {
        map.remove(CONTINUATION_DELIVERED_KEY);
    }
    input
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
    let workflow =
        load_workflow_union(runtime.source_dir(), &overlays, workflow_id)?.ok_or_else(|| {
            OpenCompanyError::CompanyNotFound(format!(
                "workflow {workflow_id} (it was approved, but the graph no longer exists)"
            ))
        })?;

    let input = continuation_input(effect)?;
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

/// The trigger input a continuation run starts with: the paused run's own
/// input, plus the approved gate, plus this lineage's delivery ledger.
///
/// One function rather than three call-site steps because the three have to
/// travel together — an input that carries the approval but not the ledger
/// resumes *and re-delivers*, which is issue #438 with extra steps. It is
/// `pub(crate)` so the run-level regression test can build a continuation
/// exactly the way the approvals path does, rather than reconstructing it and
/// proving only that the reconstruction works.
pub(crate) fn continuation_input(effect: &Effect) -> Result<Value> {
    let node_id = required_str(effect, PAYLOAD_NODE_ID)?;
    let input = effect
        .payload
        .get(PAYLOAD_INPUT)
        .cloned()
        .unwrap_or(Value::Null);
    let delivered: Vec<DeliveredReport> = effect
        .payload
        .get(PAYLOAD_DELIVERED)
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| serde_json::from_value(row.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    Ok(with_delivered(with_approval(input, node_id), &delivered))
}

/// Writes `delivered` onto the trigger input under
/// [`CONTINUATION_DELIVERED_KEY`], replacing whatever was there.
///
/// Replace, not merge: the card's ledger was *built* by unioning the input's
/// own list with what the run delivered (see [`delivery_ledger`]), so it is
/// already the superset. Merging again here would be a second, redundant place
/// for that rule to drift.
///
/// An empty ledger writes nothing at all, which keeps a first run's input shape
/// untouched — the reserved key appears only once there is something to
/// suppress.
fn with_delivered(input: Value, delivered: &[DeliveredReport]) -> Value {
    if delivered.is_empty() {
        return input;
    }
    match input {
        Value::Object(mut map) => {
            map.insert(
                CONTINUATION_DELIVERED_KEY.to_string(),
                serde_json::json!(delivered),
            );
            Value::Object(map)
        }
        // `with_approval` always yields an object, so this is unreachable
        // through `continuation_input`. Kept total rather than panicking.
        other => other,
    }
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
        gate_effect(workflow, node, &input, "run-1", &[])
    }

    /// A delivery row with `status`, as `deliver_outputs` would have returned it.
    fn delivery(node: &str, kind: &str, status: DeliveryStatus) -> DeliveryReport {
        DeliveryReport {
            node: node.to_string(),
            kind: kind.to_string(),
            target: None,
            status,
            detail: String::new(),
            reason: crate::ports::DeliveryReason::Unspecified,
        }
    }

    /// The ledger rows a parked card carries.
    fn ledger(effect: &Effect) -> Vec<DeliveredReport> {
        serde_json::from_value(effect.payload[PAYLOAD_DELIVERED].clone()).expect("ledger parses")
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
            effect(
                "digest",
                "second-gate",
                serde_json::json!({ "request": "x" }),
            ),
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

    // --- issue #438: the delivery ledger -------------------------------------

    /// What the run delivered rides the card, so approving it can suppress a
    /// second send. `Sent` and `Pending` are both "already delivered": a parked
    /// cold-send card is durable and approving it sends, so re-parking would
    /// stack a duplicate and approving both would mail twice.
    #[test]
    fn a_gate_card_carries_what_the_run_already_delivered() {
        let e = gate_effect(
            "digest",
            "gate",
            &serde_json::json!({ "request": "x" }),
            "run-1",
            &[
                delivery("owner_summary", "owner", DeliveryStatus::Sent),
                delivery("cold_note", "email", DeliveryStatus::Pending),
            ],
        );
        assert_eq!(
            ledger(&e),
            vec![
                DeliveredReport {
                    node: "owner_summary".into(),
                    kind: "owner".into()
                },
                DeliveredReport {
                    node: "cold_note".into(),
                    kind: "email".into()
                },
            ]
        );
    }

    /// A row that never left the process is NOT on the ledger — nothing was
    /// sent, so a continuation is free to try again. Suppressing these would
    /// silently retire a report on the strength of a failure.
    #[test]
    fn a_report_that_did_not_go_out_stays_deliverable() {
        for status in [
            DeliveryStatus::Skipped,
            DeliveryStatus::Denied,
            DeliveryStatus::Failed,
        ] {
            let e = gate_effect(
                "digest",
                "gate",
                &Value::Null,
                "run-1",
                &[delivery("summary", "owner", status)],
            );
            assert!(
                ledger(&e).is_empty(),
                "{status:?} sent nothing, so it must stay deliverable"
            );
        }
    }

    /// An `owner` destination fans out to one row per admin. The ledger is per
    /// node, so it holds that node once rather than once per recipient.
    #[test]
    fn a_fanned_out_destination_is_one_ledger_row() {
        let e = gate_effect(
            "digest",
            "gate",
            &Value::Null,
            "run-1",
            &[
                delivery("summary", "owner", DeliveryStatus::Sent),
                delivery("summary", "owner", DeliveryStatus::Sent),
            ],
        );
        assert_eq!(ledger(&e).len(), 1);
    }

    /// The ledger rides the continuation's trigger input — this is the whole
    /// mechanism, since `deliver_outputs` reads it from there and nowhere else.
    #[test]
    fn the_ledger_rides_the_continuation_input() {
        let card = gate_effect(
            "digest",
            "gate",
            &serde_json::json!({ "request": "x" }),
            "run-1",
            &[delivery("summary", "owner", DeliveryStatus::Sent)],
        );

        let input = continuation_input(&card).expect("a well-formed card continues");

        assert_eq!(input["approvals"], serde_json::json!(["gate"]));
        assert_eq!(
            input["request"], "x",
            "the original topic still rides along"
        );
        assert_eq!(
            delivered_in_input(&input),
            vec![DeliveredReport {
                node: "summary".into(),
                kind: "owner".into()
            }]
        );
    }

    /// **The two-gate case.** Approving the first gate starts a continuation
    /// that skips the already-sent report and then pauses at the second gate.
    /// That second card must carry the FIRST run's deliveries too — it delivered
    /// nothing itself, so a ledger built only from its own rows would be empty
    /// and approving it would send the report for real.
    #[test]
    fn the_ledger_accumulates_across_two_gates() {
        // Run 1 delivers the summary and pauses on gate-a.
        let first = gate_effect(
            "digest",
            "gate-a",
            &serde_json::json!({ "request": "x" }),
            "run-1",
            &[delivery("summary", "owner", DeliveryStatus::Sent)],
        );
        let continuation = continuation_input(&first).expect("continues");

        // Run 2 skips the summary (delivering nothing) and pauses on gate-b.
        let second = gate_effect("digest", "gate-b", &continuation, "run-2", &[]);
        assert_eq!(
            ledger(&second),
            vec![DeliveredReport {
                node: "summary".into(),
                kind: "owner".into()
            }],
            "the second card must remember what the first run sent"
        );

        // And approving THAT still suppresses it, with both gates approved.
        let next = continuation_input(&second).expect("continues");
        assert_eq!(next["approvals"], serde_json::json!(["gate-a", "gate-b"]));
        assert_eq!(delivered_in_input(&next).len(), 1);
    }

    /// A run that delivered nothing writes no reserved key at all, so an
    /// ordinary continuation's input keeps exactly the shape it always had.
    #[test]
    fn a_lineage_that_delivered_nothing_threads_no_reserved_key() {
        let card = effect("digest", "gate", serde_json::json!({ "request": "x" }));
        let input = continuation_input(&card).expect("continues");
        assert!(input.get(CONTINUATION_DELIVERED_KEY).is_none(), "{input}");
        assert!(delivered_in_input(&input).is_empty());
    }

    /// The ledger must not make a continuation's gate look like a *different*
    /// decision — that would stack a second card for one gate on every resume,
    /// which is the dedupe failure #395 closed.
    #[test]
    fn the_ledger_does_not_split_one_decision_into_two_cards() {
        let paused = gate_effect(
            "digest",
            "gate",
            &serde_json::json!({ "request": "x" }),
            "run-1",
            &[delivery("summary", "owner", DeliveryStatus::Sent)],
        );
        // The same gate, re-reached by the continuation the card started: same
        // input plus the approval… minus the approval, which the gate node
        // consumed. What differs is the ledger key alone.
        let mut continuation = continuation_input(&paused).expect("continues");
        continuation
            .as_object_mut()
            .expect("object")
            .remove("approvals");
        let re_reached = gate_effect("digest", "gate", &continuation, "run-2", &[]);

        assert!(
            is_same_gate(&paused, &re_reached),
            "the ledger is not part of the decision:\n{:?}\n{:?}",
            paused.payload,
            re_reached.payload
        );
    }

    /// The card says, in plain words, what approving actually does. The operator
    /// deciding is the one who pays for the re-run.
    #[test]
    fn the_card_states_what_approving_costs() {
        let e = effect("digest", "gate", Value::Null);
        let note = e.payload[PAYLOAD_NOTE].as_str().expect("a note");
        assert!(note.contains("re-runs"), "{note}");
        assert!(note.contains("tokens"), "{note}");
        assert!(note.contains("not be sent"), "{note}");
    }

    /// A garbled ledger degrades to "nothing known to be delivered" rather than
    /// refusing the resume. Failing here would turn one malformed row into a
    /// continuation that delivers nothing at all — the worse error.
    #[test]
    fn a_malformed_ledger_is_ignored_rather_than_fatal() {
        for garbage in [
            serde_json::json!("not an array"),
            serde_json::json!([{ "node": 7 }]),
            serde_json::json!([null]),
        ] {
            let input = serde_json::json!({ CONTINUATION_DELIVERED_KEY: garbage });
            assert!(delivered_in_input(&input).is_empty());
        }
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

/// The decide path, end to end over a real runtime (issue #395).
///
/// These sit apart from the unit tests above because they need the whole
/// machine: a real [`CompanyRuntime`], a real approval gate, a real journal on
/// disk, and a [`WorkflowRunner`](crate::ports::WorkflowRunner) that records
/// what it was asked to run.
///
/// The runner is the only double, and it is the right one to fake: what is under
/// test is *whether a run is started, with what input, and how many times* —
/// never what the engine then does with the graph, which the engine's own suite
/// and `workflows::runner` already cover. Faking it also puts these tests in the
/// **default lane**, which is where this module compiles and where CI's headline
/// clippy/test run lives.
#[cfg(test)]
mod decide_tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::company::CompanyManifest;
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyId, Verdict};
    use crate::ports::{WorkflowRun, WorkflowRunContext, WorkflowRunner};
    use crate::runtime::RuntimeBuilder;
    use crate::runtime::journal::TaskLink;

    /// What the resume actually asked for.
    #[derive(Clone, Debug)]
    struct StartedRun {
        workflow_id: String,
        input: Value,
        run_id: String,
    }

    /// A runner that records every run it is handed and settles immediately.
    #[derive(Default)]
    struct RecordingRunner {
        started: Mutex<Vec<StartedRun>>,
    }

    impl RecordingRunner {
        fn started(&self) -> Vec<StartedRun> {
            self.started.lock().expect("recording runner").clone()
        }
    }

    #[async_trait]
    impl WorkflowRunner for RecordingRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            workflow: &crate::company::WorkflowFile,
            input: Value,
            ctx: &WorkflowRunContext,
        ) -> crate::Result<WorkflowRun> {
            self.started
                .lock()
                .expect("recording runner")
                .push(StartedRun {
                    workflow_id: workflow.id.clone(),
                    input,
                    run_id: ctx.run_id.clone(),
                });
            Ok(WorkflowRun {
                output: json!({ "ok": true }),
                pending_approvals: Vec::new(),
                deliveries: Vec::new(),
                cancelled: false,
            })
        }
    }

    const GATED_TOML: &str = r#"
id = "gated"
name = "Gated"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "gate"
kind = "output"
name = "Gate"
requires_approval = true
[[edge]]
from = "start"
to = "gate"
"#;

    fn manifest() -> CompanyManifest {
        toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief"

[policy]
mode = "full"
"#,
        )
        .expect("manifest parses")
    }

    fn operator() -> Actor {
        Actor {
            kind: ActorKind::Operator,
            id: "owner".into(),
        }
    }

    /// A seeded home whose `workflows/` directory holds the gated graph, so the
    /// resume's loader finds it exactly as the console run route would.
    fn seed_home() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("opencompany-resume-")
            .tempdir()
            .expect("tempdir");
        let workflows = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows).expect("workflows dir");
        std::fs::write(workflows.join("gated.toml"), GATED_TOML).expect("seed graph");
        dir
    }

    /// A runtime with the recording runner installed and the graph on disk.
    async fn runtime(
        home: &std::path::Path,
        with_runner: bool,
    ) -> (
        Arc<crate::company::runtime::CompanyRuntime>,
        Arc<RecordingRunner>,
    ) {
        let mut rt = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_seed_dir(home.to_path_buf())
            .build()
            .await
            .expect("runtime builds");
        let runner = Arc::new(RecordingRunner::default());
        if with_runner {
            rt.set_workflow_runner(runner.clone());
        }
        (Arc::new(rt), runner)
    }

    /// Parks a gate card the way the workflow runner does, returning its id.
    async fn park_gate(
        rt: &Arc<crate::company::runtime::CompanyRuntime>,
        input: Value,
    ) -> ApprovalId {
        park_gate_after(rt, input, &[]).await
    }

    /// [`park_gate`], for a run that delivered `deliveries` before it paused.
    async fn park_gate_after(
        rt: &Arc<crate::company::runtime::CompanyRuntime>,
        input: Value,
        deliveries: &[DeliveryReport],
    ) -> ApprovalId {
        let effect = gate_effect("gated", "gate", &input, "run-that-paused", deliveries);
        let id = rt
            .approvals
            .park(rt.id(), effect.clone())
            .await
            .expect("parks");
        rt.journal()
            .record_parked(
                &id,
                &effect,
                crate::ports::now_millis(),
                TaskLink::Unlinked,
                None,
                None,
            )
            .await
            .expect("journals");
        id
    }

    /// The resume spawns its run on a detached task, so give it a moment to be
    /// recorded. Bounded so a genuine failure fails rather than hangs.
    async fn wait_for_runs(runner: &Arc<RecordingRunner>, want: usize) -> Vec<StartedRun> {
        for _ in 0..200 {
            let started = runner.started();
            if started.len() >= want {
                return started;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        runner.started()
    }

    /// The headline: approving a parked gate starts a **new** run, carrying the
    /// gate id in the trigger input's `approvals` so the node that paused now
    /// proceeds.
    #[tokio::test]
    async fn approving_a_gate_starts_a_continuation_run() {
        let home = seed_home();
        let (rt, runner) = runtime(home.path(), true).await;
        let id = park_gate(&rt, json!({ "request": "quarterly numbers" })).await;
        assert_eq!(rt.pending_approvals().len(), 1);

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .expect("resolves");

        let started = wait_for_runs(&runner, 1).await;
        assert_eq!(started.len(), 1, "approving must start exactly one run");
        assert_eq!(started[0].workflow_id, "gated");
        // The gate is approved…
        assert_eq!(started[0].input["approvals"], json!(["gate"]));
        // …and the operator's original topic survives, so the re-run does the
        // same work rather than a blank one.
        assert_eq!(started[0].input["request"], "quarterly numbers");
        // A new causal root, not the paused run's id.
        assert_ne!(started[0].run_id, "run-that-paused");
        assert!(rt.pending_approvals().is_empty(), "the card is decided");
    }

    /// Issue #438, over the real decide path: the run an approval starts is
    /// handed the ledger of what its ancestor already delivered.
    ///
    /// The unit tests above pin the ledger's arithmetic; this one pins that it
    /// actually reaches a run — through the gate, the journal, `perform_effect`
    /// and the spawn — because that is the hop where a threading mistake would
    /// leave every other test green and still mail the report twice.
    #[tokio::test]
    async fn a_continuation_run_is_told_what_was_already_delivered() {
        let home = seed_home();
        let (rt, runner) = runtime(home.path(), true).await;
        let id = park_gate_after(
            &rt,
            json!({ "request": "quarterly numbers" }),
            &[DeliveryReport {
                node: "summary".into(),
                kind: "owner".into(),
                target: Some("ada@acme.test".into()),
                status: DeliveryStatus::Sent,
                detail: "emailed the company's admin".into(),
                reason: crate::ports::DeliveryReason::OwnerEmailed,
            }],
        )
        .await;

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .expect("resolves");

        let started = wait_for_runs(&runner, 1).await;
        assert_eq!(started.len(), 1);
        assert_eq!(
            delivered_in_input(&started[0].input),
            vec![DeliveredReport {
                node: "summary".into(),
                kind: "owner".into()
            }],
            "the continuation must know the summary already went out: {:?}",
            started[0].input
        );
    }

    /// Denying starts nothing. The paused run was already settled, so "nothing
    /// runs" is the whole outcome — there is no task to cancel.
    #[tokio::test]
    async fn denying_a_gate_starts_nothing() {
        let home = seed_home();
        let (rt, runner) = runtime(home.path(), true).await;
        let id = park_gate(&rt, json!({ "request": "x" })).await;

        rt.resolve_approval(&id, Verdict::Deny, operator())
            .await
            .expect("resolves");

        // Give a spurious spawn the same window the approve test allows.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(runner.started().is_empty(), "a denied gate must not run");
        assert!(rt.pending_approvals().is_empty());
    }

    /// Approving twice — a double-click, or a retried request — starts one run.
    /// At-most-once comes from `execute_effect_once`'s `approval:<id>` key; this
    /// pins that the resume arm really is under it.
    #[tokio::test]
    async fn approving_twice_starts_one_run() {
        let home = seed_home();
        let (rt, runner) = runtime(home.path(), true).await;
        let id = park_gate(&rt, json!({ "request": "x" })).await;

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .expect("first resolve");
        let _ = rt.resolve_approval(&id, Verdict::Approve, operator()).await;

        let started = wait_for_runs(&runner, 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            runner.started().len(),
            1,
            "a second approve must not start a second run: {started:?}"
        );
    }

    /// A host restart between the park and the approval must lose nothing. The
    /// parked effect is self-contained and the journal replays it, so a fresh
    /// runtime over the same home still resumes.
    #[tokio::test]
    async fn a_gate_parked_before_a_restart_still_resumes_after_it() {
        let home = seed_home();
        {
            let (rt, _) = runtime(home.path(), true).await;
            park_gate(&rt, json!({ "request": "survives" })).await;
        } // the "process" goes away

        // A fresh runtime over the same home, rehydrated from the journal.
        let (rt, runner) = runtime(home.path(), true).await;
        rt.recover().await.expect("replay rehydrates the park");
        let pending = rt.pending_approvals();
        let card = pending
            .iter()
            .find(|a| a.kind == WORKFLOW_APPROVE_KIND)
            .expect("the gate survived the restart");

        rt.resolve_approval(&card.id, Verdict::Approve, operator())
            .await
            .expect("resolves");

        let started = wait_for_runs(&runner, 1).await;
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].input["approvals"], json!(["gate"]));
        assert_eq!(started[0].input["request"], "survives");
    }

    /// A gate nobody decided expires to a default deny, and an expired card can
    /// never start a run.
    ///
    /// The TTL clock is driven explicitly rather than by waiting: `sweep_expired`
    /// takes `now` as a parameter, so a far-future reading exercises the real
    /// expiry path on the real gate. What it proves is structural — expiry
    /// removes the parked effect, so a later approve resolves to `NotParked` and
    /// `settle_approved_effect` (the only route to `perform_effect`) is never
    /// reached. This is the "nothing is ever held open" claim: the paused run
    /// settled long ago, and its card ages out like any other.
    #[tokio::test]
    async fn an_undecided_gate_expires_and_starts_nothing() {
        let home = seed_home();
        let (rt, runner) = runtime(home.path(), true).await;
        let id = park_gate(&rt, json!({ "request": "x" })).await;

        let expired = rt
            .approval_gate
            .sweep_expired(crate::ports::now_millis() + crate::policy::DEFAULT_TTL_MILLIS + 1);
        assert_eq!(expired, vec![id.clone()], "the gate ages out like any card");

        // Approving after expiry is the already-resolved no-op, not a run.
        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .expect("an expired card resolves as already-resolved, not an error");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            runner.started().is_empty(),
            "an expired gate must never start a continuation run"
        );
    }

    /// A build with no workflow execution says so at the moment the operator
    /// clicks Approve, rather than leaving them watching for a run that will
    /// never appear.
    #[tokio::test]
    async fn approving_on_a_build_with_no_runner_says_so() {
        let home = seed_home();
        let (rt, _) = runtime(home.path(), false).await;
        let id = park_gate(&rt, json!({ "request": "x" })).await;

        let err = rt
            .resolve_approval(&id, Verdict::Approve, operator())
            .await
            .expect_err("must surface the gap");
        assert!(
            err.to_string().contains("no workflow execution"),
            "the message must name the gap: {err}"
        );
    }

    /// A gate whose graph was deleted between parking and approving names that,
    /// rather than failing with something the operator cannot act on.
    #[tokio::test]
    async fn approving_a_gate_whose_graph_is_gone_names_it() {
        let home = seed_home();
        let (rt, runner) = runtime(home.path(), true).await;
        let id = park_gate(&rt, json!({ "request": "x" })).await;
        std::fs::remove_file(home.path().join("workflows").join("gated.toml")).expect("delete");

        let err = rt
            .resolve_approval(&id, Verdict::Approve, operator())
            .await
            .expect_err("must surface the missing graph");
        assert!(err.to_string().contains("gated"), "{err}");
        assert!(runner.started().is_empty());
    }
}
