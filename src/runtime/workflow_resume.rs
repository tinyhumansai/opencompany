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
//! # The durable ledger behind the card (issue #529)
//!
//! The card ledger above guards **one approval lineage**: it rides the trigger
//! input, so it only reaches a continuation an approval starts. It does nothing
//! for a run that *crashed* — its deliveries never made it onto any card,
//! because the run never paused — nor for a workflow re-run or re-triggered
//! *independently* of the paused lineage. Issue #529 backs the card ledger with
//! a durable one: every dispatch that leaves the process journals a
//! [`WorkflowReportDelivered`](crate::ports::types::CompanyEvent::WorkflowReportDelivered)
//! write-behind, and the runner folds those stranded deliveries
//! ([`delivered_by_unsettled_runs`](crate::runtime::delivered_by_unsettled_runs))
//! into the same `already_delivered` list this lineage ledger feeds — unioned,
//! deduped by node. The two share [`DeliveredReport`] as their identity, so a
//! crash-then-re-run skips what a crashed run already sent for the same reason a
//! resume skips what the paused run sent.
//!
//! **The honest limit.** The ledger is per `output` node, not per recipient. An
//! `owner` destination that fanned out to three admins and failed on the third
//! is recorded as delivered, and the continuation will not retry that third
//! address. Re-mailing two people to reach one is the worse outcome, so this is
//! deliberate rather than an oversight — a partial fan-out is repaired from the
//! run history, not by a resume. The durable #529 ledger keeps the same per-node
//! identity, so this limit is unchanged across a crash.
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
use serde_json::{Map, Value, json};

use crate::Result;
use crate::company::load_workflow_with_globals;
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

/// The prefix a workflow run's continuation turn key carries (issue #978).
///
/// Namespaced rather than the bare run id because the turn key space is shared
/// with the cycle ids a chat turn arms
/// ([`ContinuationQueue`](crate::runtime::continuation::ContinuationQueue)), and
/// the release side has to fork on which kind it is holding — a brain turn is
/// continued, a workflow run is re-dispatched, and running the wrong one is
/// silent. The prefix is what makes that question answerable from the key
/// alone, with no side lookup that could disagree.
pub const WORKFLOW_TURN_PREFIX: &str = "workflow-run:";

/// The continuation turn key **every gate one workflow run parked** shares
/// (issue #978).
///
/// This is the whole of the fix's accounting change. Before it,
/// `park_and_journal` recorded no turn key for a workflow gate at all, so
/// [`approval_cycle`](crate::runtime::journal::RuntimeJournal::approval_cycle)
/// answered `Some(None)` and every branch of a fan-out believed it was the only
/// decision outstanding: `decisions_still_awaited` read 0 on all three of three,
/// and all three re-dispatched. Keying on the **run** rather than the node is
/// what makes "approving never increases pending approvals" expressible — N
/// gates of one run are one decision batch, released once.
///
/// The run id is already on every parked gate ([`gate_effect`] stamps
/// `Effect::run_id`), so nothing new has to be threaded to mint this.
pub fn workflow_turn_key(run_id: &str) -> String {
    format!("{WORKFLOW_TURN_PREFIX}{run_id}")
}

/// The run behind a turn key minted by [`workflow_turn_key`], or `None` for a
/// key that names a brain turn instead (issue #978).
///
/// Paired with its writer in one module, deliberately: a reader that rebuilt the
/// prefix from a literal is a second place for the format to drift, and drift
/// here does not fail loudly — it reads a workflow key as a brain turn and runs
/// an agent cycle over a run that then never continues.
///
/// An empty remainder is rejected rather than returned: `workflow-run:` with no
/// run behind it names nothing, and treating it as a run id would spawn a
/// continuation for a graph that cannot be found.
pub fn run_id_from_turn(turn: &str) -> Option<&str> {
    turn.strip_prefix(WORKFLOW_TURN_PREFIX)
        .filter(|run_id| !run_id.is_empty())
}

/// The prefix a **blocked agent node's** continuation turn key carries
/// (issue #899, Stage 1).
///
/// # Why a third turn-key namespace, distinct from `workflow-run:`
///
/// A `requires_approval` **gate** and a policy-gated call *inside an agent
/// node's own tool loop* block a run in two structurally different ways, and
/// they must not share a batch:
///
/// * A gate parks a `WORKFLOW_APPROVE_KIND` effect (`agent: None`), and
///   approving it re-runs the graph with the gate's node id in the trigger's
///   `approvals` array. That is the `workflow-run:` batch ([`workflow_turn_key`]).
/// * An agent node's gated call parks a **tool-call-shaped** effect
///   (`agent: Some`, minted by `ApprovalPolicy::effect_for`), and approving it
///   mints a grant. Nothing re-dispatches the run — the whole hole this issue
///   closes. The re-run needs no `approvals` array (the call is not a graph
///   node); it re-runs from the top and the minted grant lets the identical call
///   pass ([`ContinuationQueue`](crate::runtime::continuation::ContinuationQueue)-armed
///   at park time, stashed by
///   [`BlockedNodeQueue`](crate::runtime::blocked_nodes::BlockedNodeQueue)).
///
/// Keying per **(run, node)** rather than per run is deliberate: one agent node
/// blocking on several gated calls is one batch owed one continuation (the #469
/// property), but two agent nodes of one run that each block are two independent
/// blocks — a continuation of one is not a continuation of the other.
pub const WORKFLOW_NODE_TURN_PREFIX: &str = "workflow-node:";

/// The continuation turn key **every gated call one blocked agent node parked**
/// shares (issue #899, Stage 1).
///
/// Per (run, node): all of a node's parked calls carry this one key, so the
/// [`ContinuationQueue`](crate::runtime::continuation::ContinuationQueue) counts
/// them as one batch and releases once, when the last decision lands. `node_id`
/// is the block's resolved node id — the graph node id when the engine gave the
/// turn one, else the agent ref — so the runner's stash and the parker agree on
/// the key by construction.
pub fn workflow_node_turn_key(run_id: &str, node_id: &str) -> String {
    format!("{WORKFLOW_NODE_TURN_PREFIX}{run_id}:{node_id}")
}

/// Whether `turn` names a blocked agent node minted by [`workflow_node_turn_key`]
/// (issue #899, Stage 1).
///
/// What `continue_turn` forks on to route a released batch to a workflow-run
/// continuation rather than a brain cycle. The full key is the
/// [`BlockedNodeQueue`](crate::runtime::blocked_nodes::BlockedNodeQueue) stash
/// key, so nothing here parses the run/node back out — the prefix test is the
/// whole question, and it is disjoint from `workflow-run:` so the gate fork
/// ([`run_id_from_turn`]) never misfires on it.
pub fn is_node_turn(turn: &str) -> bool {
    turn.strip_prefix(WORKFLOW_NODE_TURN_PREFIX)
        .is_some_and(|rest| !rest.is_empty())
}

/// The `(run, node)` behind a key minted by [`workflow_node_turn_key`], or
/// `None` for any other turn key (B-012, Codex review).
///
/// # Why this parses what `is_node_turn` deliberately does not
///
/// Routing a released batch only ever needed the prefix test, so nothing pulled
/// the halves back out. Settling the attempt an *expired* gated call left behind
/// does need them, and this key is the only place they survive: a gated tool
/// call is parked with `ApprovalPolicy::effect_for`'s own effect, whose `kind`
/// is the tool name and whose `run_id` is `None` — so neither the effect kind
/// nor `Effect::run_id` can name the run or the node, and the cycle recorded at
/// park time is the durable correlation.
///
/// Split on the **first** separator after the prefix: run ids are generated ids
/// with no `:` in them, while a node id is a graph-authored string that may well
/// contain one.
pub fn run_and_node_from_node_turn(turn: &str) -> Option<(&str, &str)> {
    let rest = turn.strip_prefix(WORKFLOW_NODE_TURN_PREFIX)?;
    let (run_id, node_id) = rest.split_once(':')?;
    (!run_id.is_empty() && !node_id.is_empty()).then_some((run_id, node_id))
}

/// The payload key holding the workflow whose run paused.
pub const PAYLOAD_WORKFLOW_ID: &str = "workflow_id";
/// The payload key holding the gate node awaiting sign-off.
pub const PAYLOAD_NODE_ID: &str = "node_id";
/// The payload key holding the trigger input the paused run was started with.
pub const PAYLOAD_INPUT: &str = "input";
/// The payload key holding the [`StartedBy`](crate::ports::types::StartedBy)
/// of the run that paused (issue #1862 prerequisite) — read back by
/// [`started_by_of`] so a continuation carries the same attribution rather
/// than resetting to the `scheduled`-derived default every resumed run's
/// `scheduled` (always `false`, issue #542) would otherwise stamp.
pub const PAYLOAD_STARTED_BY: &str = "started_by";
/// The durable tinyflows lineage used for a node-level continuation.
pub const PAYLOAD_THREAD_ID: &str = "thread_id";
/// The workflow graph's [`content_fingerprint`](crate::company::WorkflowFile::content_fingerprint)
/// at the moment this gate parked — compared against a freshly loaded copy of
/// the graph before a continuation trusts `PAYLOAD_THREAD_ID`'s checkpoint, so
/// an edit made to the workflow while an approval sat pending falls back to
/// [`ResumeSemantic::ReRunFromTrigger`](crate::ports::ResumeSemantic::ReRunFromTrigger)
/// instead of resuming a stale checkpoint into a graph it no longer matches.
pub const PAYLOAD_WORKFLOW_FINGERPRINT: &str = "workflow_fingerprint";
/// The payload key holding this lineage's delivery ledger (issue #438) — the
/// reports a continuation must NOT send again.
pub const PAYLOAD_DELIVERED: &str = "delivered";
/// The payload key holding this lineage's **outward-call** ledger (issue #846)
/// — the `tool_call` nodes that already reached a counterparty, and the result
/// each returned, so a continuation replays them instead of calling again.
///
/// The `output`-node sibling of [`PAYLOAD_DELIVERED`], for the other half of
/// #438's exposure: #496 guarded a report a *delivery* node routed, and nothing
/// guarded a `send` / `publish` / `repo_publish` the graph made as a node of its
/// own. See [`PerformedCall`].
pub const PAYLOAD_PERFORMED: &str = "performed";
/// The payload key holding this lineage's **denial** ledger (issue #978) — the
/// gate nodes an operator has refused, so a continuation neither runs them nor
/// asks about them again.
///
/// The third ledger, and the one that makes a *mixed* verdict safe. A run that
/// fans out to three gates and is approved twice and denied once re-dispatches
/// once, carrying two approvals; the denied node is not in that set, so without
/// this the replay would reach it, pause on it, and park a **new** card — an
/// approval round that cleared three and created one, which is the invariant
/// this issue exists to restore. Listed here, `park_pending_gates` skips it: the
/// refusal is final, and the branch below it simply never completes.
///
/// Accumulates down the lineage exactly as [`PAYLOAD_DELIVERED`] and
/// [`PAYLOAD_PERFORMED`] do — a two-gate graph must not forget the first gate's
/// denial when it parks the second.
pub const PAYLOAD_DENIED: &str = "denied";
/// The payload key holding the plain-prose statement of what approving costs.
pub const PAYLOAD_NOTE: &str = "note";
/// The payload key holding the verbatim output of the gate's upstream nodes —
/// the content awaiting sign-off (issue #596). A map `{ "<upstream node id>":
/// <bounded node output> }`. Deliberately **not** part of the gate's dedupe
/// identity (see [`is_same_gate`]): two runs whose only difference is the content
/// their upstream nodes produced are still one decision on the same gate.
pub const PAYLOAD_CONTENT: &str = "content";
/// The payload key holding the toolbelt slug a policy-gated `tool_call` node
/// would run (issue #460). Absent on an authored `requires_approval` gate,
/// which stops the run without any particular call behind it.
pub const PAYLOAD_TOOL: &str = "tool";
/// The payload key holding the policy's own words for why it stopped this call
/// (issue #460) — the same sentence the agent path puts on its card. Absent for
/// the same reason as [`PAYLOAD_TOOL`].
pub const PAYLOAD_REASON: &str = "reason";
/// The payload key holding what a gated call would reach — `"POST
/// api.example.com"` for an `http_request` node (issue #614), the host for a
/// `tool_call` node whose arguments name a URL (issue #846). Absent when the
/// node's call has no destination worth naming, or when the URL is still an
/// unresolved `=`-expression at gate time.
pub const PAYLOAD_TARGET: &str = "target";
/// The payload key holding the gated node's **authored arguments** (issue #846)
/// — the `url` a `web_fetch` will fetch, the recipient a `send` will reach.
///
/// This is what makes a workflow card decidable in the way #372/#375 made a
/// chat card decidable: the operator sees the call, not a node id. It is
/// credential-redacted host-side by the same projection that redacts a tool
/// call's payload on the chat path — see
/// [`display_payload`](crate::runtime::approval_display) — so this key adds no
/// new redaction rule and cannot bypass the existing one.
///
/// Absent when the node makes no classifiable call (an authored gate on a
/// `transform`, say), which the console must render as "no arguments" rather
/// than as an empty object.
pub const PAYLOAD_ARGS: &str = "args";

/// What a gated node's card says about the call being decided (issues #460,
/// #614, #846).
///
/// Grouped rather than passed as four more arguments to [`gate_effect`]: they
/// describe one thing — the call — and a node whose call the host cannot
/// classify at all passes `None` for the lot.
#[derive(Debug, Clone, Copy)]
pub struct GateCall<'a> {
    /// The tool the node would run.
    pub tool: &'a str,
    /// The policy's own words for why it stopped — `None` on an authored
    /// `requires_approval` gate, where nobody wrote a reason because nobody was
    /// asked to. The call is still named (issue #846).
    pub reason: Option<&'a str>,
    /// The node's authored arguments, so the operator decides about a call
    /// rather than about a node id (issue #846). Redacted downstream, at the
    /// same projection that redacts a chat card's payload.
    pub args: Option<&'a Value>,
    /// Method and host, when knowable. Never the path or query — see
    /// `GatedCall::target` in `crate::workflows::gate` for why.
    pub target: Option<&'a str>,
}

/// The reserved trigger-input key the delivery ledger rides into a continuation
/// run under (issue #438).
///
/// Reserved, and shaped so nobody authors it by accident: it is threaded by the
/// host, read by `deliver_outputs`, and never by the engine or a graph author.
/// It is stripped before two parked gates are compared — see `is_same_gate` — because a continuation's input differs from the paused
/// run's by exactly this key, and letting that difference count would make
/// every continuation gate a "new" decision and stack a duplicate card.
pub const CONTINUATION_DELIVERED_KEY: &str = "__opencompany_delivered";

/// The reserved trigger-input key the **outward-call** ledger rides into a
/// continuation run under (issue #846).
///
/// Reserved on exactly the terms [`CONTINUATION_DELIVERED_KEY`] is: host-written,
/// host-read, never authored and never seen by the engine as anything but
/// opaque trigger data. Stripped before two parked gates are compared
/// ([`is_same_gate`]) for the same reason its sibling is — it describes what has
/// already happened, not what is being decided, so counting it would make every
/// continuation gate read as a new decision and stack a duplicate card.
pub const CONTINUATION_PERFORMED_KEY: &str = "__opencompany_performed";

/// The reserved trigger-input key the **denial** ledger rides into a
/// continuation run under (issue #978).
///
/// Reserved on exactly [`CONTINUATION_DELIVERED_KEY`]'s terms: host-written,
/// host-read, never authored and opaque to the engine. Stripped before two
/// parked gates are compared ([`is_same_gate`]) for the same reason both its
/// siblings are — it records what has already been decided, not what is being
/// decided, so counting it would make every continuation's gate read as a new
/// question and stack a duplicate card.
pub const CONTINUATION_DENIED_KEY: &str = "__opencompany_denied";

/// The reserved trigger-input key an **answered node blocker** rides into a
/// continuation run under (issue #2005).
///
/// Reserved on [`CONTINUATION_DELIVERED_KEY`]'s terms — host-written, host-read,
/// opaque to the engine — and stripped before two parked gates are compared
/// ([`is_same_gate`]) for the reason all three ledger keys are.
///
/// Where it differs from them is the direction it is allowed to fail in.
/// A ledger says what must NOT happen again, so a garbled one degrades to
/// "deliver / call / ask" — the pre-ledger behaviour, and safe. This key
/// carries the operator's own decision about a stopped node, so degrading it to
/// "nothing known" is exactly the silent drop the blocker family exists to
/// prevent. [`blocker_answers_in_input`] therefore reads it strictly and fails
/// loudly, and the caller settles the node rather than running it as if nobody
/// had answered.
pub const CONTINUATION_BLOCKER_KEY: &str = "__opencompany_blockers";

/// What approving a workflow gate actually does, in the operator's own terms.
///
/// This rides the card as [`PAYLOAD_NOTE`] rather than living only in a design
/// doc, because the person deciding is the one who pays the cost. Prose, not a
/// code reference — the reader is an operator looking at an Approvals card.
///
/// Gated the same way `checkpoint_resume_available` is: on a build with
/// checkpoint machinery wired, approving normally resumes from the
/// checkpoint rather than re-running the whole graph, so the note names both
/// outcomes instead of unconditionally warning of the costlier one.
#[cfg(feature = "openhuman")]
pub const CONTINUATION_NOTE: &str = "Approving this normally resumes the workflow from this step — earlier steps do not run again, \
     and their token spend is not repeated. If this workflow's graph changed while this was \
     pending, approving instead starts the whole workflow over from the trigger, and every step \
     before this gate runs again and spends tokens again. Either way, reports this run already \
     delivered will not be sent a second time, and a step that already sent or published \
     something replays what it returned instead of doing it again.";

/// The build with no checkpoint machinery wired: every approval is
/// unconditionally a full re-run from the trigger.
#[cfg(not(feature = "openhuman"))]
pub const CONTINUATION_NOTE: &str = "Approving this re-runs the whole workflow from the start — every step before this gate runs \
     again, and any agent steps spend tokens again. Reports this run already delivered will not be \
     sent a second time, and a step that already sent or published something replays what it \
     returned instead of doing it again.";

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

/// One `tool_call` node whose call **left the building** in a prior run of this
/// lineage, together with the result it returned (issue #846).
///
/// # Why this exists beside [`DeliveredReport`] rather than inside it
///
/// #438's exposure is "approving re-runs the graph, so something that already
/// left the building leaves it again". #496 closed that for the half the host
/// performs itself — an `output` node's report, routed by `deliver_outputs` —
/// because that is the half the host can skip by simply not calling out. A
/// `tool_call` node's send is performed by the **engine**, through a capability,
/// and the host cannot decline it after the fact: it has to arrange, before the
/// run starts, for the call not to be made. So the identity is the same (the
/// node) but the mechanism is not, and folding the two ledgers into one type
/// would put a `result` field on a record whose whole point is that there is
/// nothing to replay.
///
/// `result` is the **verbatim capability return** — the value the engine wrapped
/// in its `{ json, text, raw }` envelope — so replaying it reconstructs the
/// node's output byte-for-byte rather than approximating it. A node whose
/// recorded result would have to be truncated to fit the card is deliberately
/// **not** recorded: see `outward_calls_performed` in `crate::workflows::replay`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PerformedCall {
    /// The `tool_call` node that made the call.
    pub node: String,
    /// The toolbelt slug it invoked, for the log line and the operator's card.
    pub tool: String,
    /// The verbatim value the capability returned.
    pub result: Value,
}

/// The outward calls this lineage has already made, read off a trigger input.
///
/// Tolerant on exactly [`delivered_in_input`]'s terms, and for the same reason:
/// a missing key, a non-array or a malformed row yields "nothing known to have
/// been performed", which is the pre-#846 behaviour (call it). The failure mode
/// of being wrong in the other direction is a node that silently never runs.
pub fn performed_in_input(input: &Value) -> Vec<PerformedCall> {
    input
        .get(CONTINUATION_PERFORMED_KEY)
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| serde_json::from_value(row.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The outward-call ledger a gate parked on this run should carry: what this run
/// performed, unioned with what its own trigger input already listed.
///
/// The union is what makes a **two-gate** graph correct, exactly as
/// [`delivery_ledger`]'s is: a continuation that replayed a send rather than
/// making it has performed nothing itself, so a ledger built from this run alone
/// would be empty at the second gate and the third run would send for real.
/// First entry per node wins — the earliest run in the lineage is the one that
/// actually reached the counterparty, and its result is the one to replay.
fn performed_ledger(input: &Value, performed: &[PerformedCall]) -> Vec<PerformedCall> {
    let mut ledger = performed_in_input(input);
    for call in performed {
        if !ledger.iter().any(|prior| prior.node == call.node) {
            ledger.push(call.clone());
        }
    }
    ledger
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

/// The gate nodes this lineage has already refused, read off a trigger input
/// (issue #978).
///
/// Tolerant on [`delivered_in_input`]'s terms and for its reason: a missing key,
/// a non-array or a non-string row yields "nothing known to be denied", which is
/// the pre-#978 behaviour (ask about it). Being wrong in the other direction —
/// inventing a denial — would silently strand a branch the operator never
/// refused.
pub fn denied_in_input(input: &Value) -> Vec<String> {
    input
        .get(CONTINUATION_DENIED_KEY)
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// One node whose blocker an operator has answered, riding a continuation's
/// trigger input under [`CONTINUATION_BLOCKER_KEY`] (issue #2005).
///
/// The durable [`BlockerResolution`](crate::ports::blockers::BlockerResolution)
/// narrowed to what the engine side needs: which node the answer is about, what
/// the operator asked for, and the words an
/// [`Amend`](crate::ports::blockers::BlockerVerdict::Amend) re-enters carrying.
/// `node` is the run's own resolved node id — the graph node when there is one,
/// the agent ref otherwise — which is the same identity
/// [`BlockerStep::Node`](crate::ports::blockers::BlockerStep::Node) and
/// [`WorkflowBlockedNode::node_id`](crate::ports::WorkflowBlockedNode::node_id)
/// carry, so the park and the re-entry agree by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerAnswer {
    /// The node the answer re-enters.
    pub node: String,
    /// What the operator asked that node to do.
    pub verdict: crate::ports::blockers::BlockerVerdict,
    /// The operator's words, for the verdict that carries them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer: String,
}

/// Every answered blocker riding `input`, or a loud error for a key that is
/// present and unreadable (issue #2005).
///
/// Strict where [`delivered_in_input`] and its siblings are tolerant, and the
/// asymmetry is the point — see [`CONTINUATION_BLOCKER_KEY`]. A row that names
/// a verdict this build does not know, or that is not an answer at all, means
/// the host and the engine disagree about what the operator decided; running
/// the node as though nobody had answered would spend a turn on the identical
/// failure and re-park the identical question, with the answer gone.
///
/// An absent key is not an error: it is a first run, and every node runs as it
/// always did.
pub fn blocker_answers_in_input(input: &Value) -> Result<Vec<BlockerAnswer>> {
    let Some(raw) = input.get(CONTINUATION_BLOCKER_KEY) else {
        return Ok(Vec::new());
    };
    let rows = raw.as_array().ok_or_else(|| {
        OpenCompanyError::InvalidRequest(format!(
            "a workflow run's trigger input carries `{CONTINUATION_BLOCKER_KEY}` as something \
             other than a list of answered blockers, so an operator's answer cannot be read"
        ))
    })?;
    rows.iter()
        .map(|row| {
            serde_json::from_value::<BlockerAnswer>(row.clone()).map_err(|err| {
                OpenCompanyError::InvalidRequest(format!(
                    "a workflow run's trigger input carries an unreadable answered blocker \
                     under `{CONTINUATION_BLOCKER_KEY}`: {err}"
                ))
            })
        })
        .collect()
}

/// The answer riding `input` for `node`, if the operator gave one.
///
/// The one call an executing node makes. Errors on the same terms
/// [`blocker_answers_in_input`] does, and on one more:
/// [`Cancel`](crate::ports::blockers::BlockerVerdict::Cancel) never starts a
/// run — the resume fork short-circuits before any cycle — so finding one here
/// means a cancelled blocker's answer reached a running graph, and the node
/// must stop rather than quietly carry on as if the operator had said yes.
pub fn blocker_answer_for(input: &Value, node: &str) -> Result<Option<BlockerAnswer>> {
    let Some(answer) = blocker_answers_in_input(input)?
        .into_iter()
        .find(|answer| answer.node == node)
    else {
        return Ok(None);
    };
    if !answer.verdict.resumes() {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "workflow node `{node}` was reached carrying a `{}` blocker answer, which starts \
             no run at all",
            answer.verdict.as_str()
        )));
    }
    Ok(Some(answer))
}

/// Writes `answer` onto the trigger input under [`CONTINUATION_BLOCKER_KEY`],
/// superseding any earlier answer for the same node (issue #2005).
///
/// Accumulating rather than replacing the whole list, on [`with_denied`]'s
/// reasoning: a graph whose second node blocks after the first was answered
/// must not forget the first, or the continuation re-runs a node the operator
/// already skipped. Last write per node wins — a node retried, blocked again
/// and then skipped is skipped, because the newer decision is the operator's
/// current one.
///
/// A non-object input becomes one carrying just this key: there is nowhere else
/// to put it, and [`with_approvals`] already takes the same liberty.
pub fn with_blocker_answer(input: Value, answer: &BlockerAnswer) -> Value {
    let mut answers = blocker_answers_in_input(&input).unwrap_or_default();
    answers.retain(|prior| prior.node != answer.node);
    answers.push(answer.clone());
    let mut map = match input {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    map.insert(
        CONTINUATION_BLOCKER_KEY.to_string(),
        serde_json::json!(answers),
    );
    Value::Object(map)
}

/// The trigger input a node blocker's continuation runs with (issue #2005):
/// the blocked run's own input, plus the operator's answer for `node`.
///
/// Refuses a non-resuming verdict rather than building an input for it. A
/// [`Cancel`](crate::ports::blockers::BlockerVerdict::Cancel) settles the run
/// and starts nothing, so reaching here with one is a caller that skipped the
/// fork — loud beats a continuation that re-runs the work the operator asked to
/// abandon.
pub fn blocker_continuation_input(
    input: Value,
    node: &str,
    resolution: &crate::ports::blockers::BlockerResolution,
) -> Result<Value> {
    if !resolution.resumes() {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "a workflow continuation was asked for on node `{node}` with a `{}` blocker \
             answer, which abandons the work rather than re-entering it",
            resolution.verdict.as_str()
        )));
    }
    Ok(with_blocker_answer(
        input,
        &BlockerAnswer {
            node: node.to_string(),
            verdict: resolution.verdict,
            answer: resolution.answer.clone(),
        },
    ))
}

/// The gate node a parked [`WORKFLOW_APPROVE_KIND`] effect is asking about, or
/// `None` for any other effect (issue #978).
///
/// The kind is checked as well as the key, so a non-gate effect that happens to
/// carry a `node_id` payload cannot be mistaken for one. This is what the
/// run-scoped stash keys its batch on — see
/// [`WorkflowGateQueue`](crate::runtime::workflow_gates::WorkflowGateQueue).
pub fn gate_node_id(effect: &Effect) -> Option<&str> {
    if effect.kind != WORKFLOW_APPROVE_KIND {
        return None;
    }
    effect
        .payload
        .get(PAYLOAD_NODE_ID)
        .and_then(Value::as_str)
        .filter(|node| !node.trim().is_empty())
}

/// The workflow a parked [`WORKFLOW_APPROVE_KIND`] effect belongs to, or `None`
/// for any other effect (issue #1098).
///
/// The subject half of a workflow standing permission, read off the payload the
/// park already writes. Kind-checked for the same reason [`gate_node_id`] is: a
/// non-gate effect carrying a `workflow_id` must not be mistaken for one.
pub fn gate_workflow_id(effect: &Effect) -> Option<&str> {
    if effect.kind != WORKFLOW_APPROVE_KIND {
        return None;
    }
    effect
        .payload
        .get(PAYLOAD_WORKFLOW_ID)
        .and_then(Value::as_str)
        .filter(|workflow| !workflow.trim().is_empty())
}

/// The [`StartedBy`](crate::ports::types::StartedBy) of the run a parked
/// [`WORKFLOW_APPROVE_KIND`] effect belongs to (issue #1862 prerequisite).
///
/// Falls back to [`StartedBy::from_scheduled(false)`](crate::ports::types::StartedBy::from_scheduled)
/// — the same coarse default [`WorkflowRunContext::new`](crate::ports::WorkflowRunContext::new)
/// uses — for a card parked before this field existed, or one whose payload
/// value fails to parse. Not kind-checked like [`gate_node_id`]/
/// [`gate_workflow_id`]: every caller that reaches this already knows it is
/// holding a gate card, so the fallback exists for the payload shape, not the
/// effect kind.
pub fn started_by_of(effect: &Effect) -> crate::ports::types::StartedBy {
    effect
        .payload
        .get(PAYLOAD_STARTED_BY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_else(|| crate::ports::types::StartedBy::from_scheduled(false))
}

/// The call a parked [`WORKFLOW_APPROVE_KIND`] effect is stopping — `(tool,
/// args)` — or `None` for any other effect, and for a gate whose call the host
/// could not classify (issue #1098).
///
/// # Why this exists
///
/// A gate's [`kind`](Effect::kind) is the wrapper `workflow.approve`, not the
/// tool the node is about to call. Anything that classifies an effect by asking
/// [`consequence_of`](crate::policy::consequence_of) about its `kind` therefore
/// asks about a name the declaration table has never heard of, and gets the
/// undeclared fallback rather than an answer about the real call. That is
/// correct as a default — an unknown name must fail closed — but it means the
/// classifier never sees the `web_fetch` the operator is actually looking at.
///
/// The call itself is already on the payload: issue #846 wrote
/// [`PAYLOAD_TOOL`] / [`PAYLOAD_ARGS`] so a workflow card could say *which*
/// call it is stopping instead of naming a node id. This reads the same two
/// keys back, so the thing classified is the thing the card showed.
///
/// # Absent arguments are `Null`, not an error
///
/// [`PAYLOAD_ARGS`] is written only when the node had arguments to write, so a
/// tool present with no args is an ordinary shape rather than a broken one. It
/// answers `Value::Null`, which every argument-aware classifier reads as "no
/// argument I can place" and resolves in the cautious direction — a `web_fetch`
/// with no readable host is [`Standing::PerCall`](crate::policy::Standing), not
/// an unscoped permission. Node arguments may also still be unresolved
/// `=`-expressions at gate time, which lands in the same place for the same
/// reason.
pub fn gate_inner_call(effect: &Effect) -> Option<(&str, &Value)> {
    if effect.kind != WORKFLOW_APPROVE_KIND {
        return None;
    }
    let tool = effect
        .payload
        .get(PAYLOAD_TOOL)
        .and_then(Value::as_str)
        .filter(|tool| !tool.trim().is_empty())?;
    // `'static` rather than `&Value::Null`: the return borrows from `effect`, so
    // a reference to a temporary would not outlive the call.
    static NO_ARGS: Value = Value::Null;
    let args = effect.payload.get(PAYLOAD_ARGS).unwrap_or(&NO_ARGS);
    Some((tool, args))
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
///
/// `call` describes the call the gate is stopping — which tool, with which
/// arguments, reaching where. It is `Some(..)` for **every** node whose call the
/// host can classify, whoever raised the gate: the company's `ApprovalPolicy`
/// (issues #460, #614) or the author's own `requires_approval` (issue #846).
/// Only [`GateCall::reason`] distinguishes them, because only a policy has words
/// for why it stopped. It is deliberately outside the dedupe identity
/// ([`is_same_gate`]): it describes the *same* decision in more words, so two
/// cards that differ only here are still one question.
///
/// `performed` is what this lineage has already sent (issue #846), folded into
/// the card's ledger on exactly the terms `deliveries` is: a card that needs a
/// side table is a card that stops working after a restart.
pub fn gate_effect(
    workflow_id: &str,
    node_id: &str,
    input: &Value,
    run_id: &str,
    deliveries: &[DeliveryReport],
    performed: &[PerformedCall],
    call: Option<GateCall<'_>>,
) -> Effect {
    let mut payload = Map::new();
    payload.insert(PAYLOAD_WORKFLOW_ID.to_string(), json!(workflow_id));
    payload.insert(PAYLOAD_NODE_ID.to_string(), json!(node_id));
    // The whole trigger input, so the parked card is self-contained and a
    // resume needs nothing but the journal. This is what makes
    // approve-after-restart work.
    payload.insert(PAYLOAD_INPUT.to_string(), input.clone());
    // What must NOT be sent again when this card is approved.
    payload.insert(
        PAYLOAD_DELIVERED.to_string(),
        json!(delivery_ledger(input, deliveries)),
    );
    // Issue #846: what must NOT be *called* again when this card is approved.
    // Written unconditionally, including when empty, so a reader can tell a host
    // that considered the question and found nothing from one that never asked.
    payload.insert(
        PAYLOAD_PERFORMED.to_string(),
        json!(performed_ledger(input, performed)),
    );
    // Issue #978: what must NOT be *asked about* again when this card is
    // approved. Purely inherited — a denial is made at resolve time, never at
    // park time — but it has to ride the card, or a two-gate graph forgets the
    // first gate's refusal the moment it parks the second.
    payload.insert(PAYLOAD_DENIED.to_string(), json!(denied_in_input(input)));
    // What approving costs, in the operator's own terms.
    payload.insert(PAYLOAD_NOTE.to_string(), json!(CONTINUATION_NOTE));
    // Issue #460: which call the policy stopped, and why. The keys are ABSENT
    // rather than null on an authored gate — a card that names no tool is a
    // different thing from one whose tool could not be determined, and a
    // console reading `payload.tool` should be able to tell them apart.
    // Issue #460: which call was stopped, and — when a policy stopped it — why.
    // The keys are ABSENT rather than null on a node whose call cannot be
    // classified: a card that names no tool is a different thing from one whose
    // tool could not be determined, and a console reading `payload.tool` should
    // be able to tell them apart.
    //
    // Issue #846: `reason` remains policy-only while `tool` / `args` / `target`
    // are written for an **authored** gate too. That asymmetry is the point. An
    // author's `requires_approval` has no reason to state — they asked for a
    // human, and the console says so in its own words — but the call itself is
    // just as knowable, and a card that withholds it because *nobody wrote a
    // sentence about it* is the bug this closes.
    if let Some(call) = call {
        payload.insert(PAYLOAD_TOOL.to_string(), json!(call.tool));
        if let Some(reason) = call.reason {
            payload.insert(PAYLOAD_REASON.to_string(), json!(reason));
        }
        if let Some(args) = call.args {
            payload.insert(PAYLOAD_ARGS.to_string(), args.clone());
        }
        if let Some(target) = call.target {
            payload.insert(PAYLOAD_TARGET.to_string(), json!(target));
        }
    }

    Effect {
        kind: WORKFLOW_APPROVE_KIND.to_string(),
        group: crate::ports::types::EffectGroup::Other,
        amount_usd: None,
        established_thread: false,
        first_time_counterparty: false,
        payload: Value::Object(payload),
        // Native, not a teammate's tool call — see the doc above.
        agent: None,
        // The run that paused. Not the run the approval will start (which does
        // not exist yet); this is the causal ancestor, and it is what lets the
        // console tie the card back to the run history the operator was
        // watching.
        run_id: Some(run_id.to_string()),
    }
}

/// The verbatim output of a gate's **upstream** nodes — the content an operator
/// is being asked to sign off before it publishes (issue #596).
///
/// At park time the paused run's `outcome.output["nodes"]` already holds every
/// completed upstream node's output (the gate node itself has not run — that is
/// what "requires approval" means), so the pre-publish content is available with
/// **zero engine change**: this reads it straight off the settled output.
///
/// For the given `gate_node`, it walks `edges` for every `from → gate_node` edge
/// and collects that upstream node's output, [`bound`](crate::ports::bound_node_output)
/// so a runaway node cannot bloat the card. The result is a map keyed by upstream
/// node id; a gate with no upstream output (an unreachable graph, or output the
/// run never produced) yields an empty map, which the console renders as "no
/// content to preview".
pub fn upstream_content(
    output: &Value,
    edges: &[crate::company::WorkflowEdgeDef],
    gate_node: &str,
) -> Value {
    let nodes = output.get("nodes");
    let mut content = Map::new();
    for edge in edges {
        if edge.to != gate_node {
            continue;
        }
        // One entry per distinct upstream node, even if two edges connect them.
        if content.contains_key(&edge.from) {
            continue;
        }
        if let Some(node_output) = nodes.and_then(|n| n.get(&edge.from)) {
            let (bounded, _truncated) = crate::ports::bound_node_output(node_output);
            content.insert(edge.from.clone(), bounded);
        }
    }
    Value::Object(content)
}

/// Attaches the gate's upstream content (issue #596) to an already-built park
/// effect under [`PAYLOAD_CONTENT`].
///
/// A self-contained addition on top of [`gate_effect`] rather than a change to
/// it: the effect's dedupe identity ([`is_same_gate`]) ignores this key, so
/// enriching a card with content never splits one decision into two. A no-op on
/// a non-object payload (which `gate_effect` never produces).
pub fn attach_upstream_content(
    effect: &mut Effect,
    output: &Value,
    edges: &[crate::company::WorkflowEdgeDef],
    gate_node: &str,
) {
    let content = upstream_content(output, edges, gate_node);
    if let Value::Object(map) = &mut effect.payload {
        map.insert(PAYLOAD_CONTENT.to_string(), content);
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
/// input carries it under (issue #438), nor either of their siblings — the
/// outward-call ledger (#846) and the denial ledger (#978). All of them differ
/// by construction between a paused run and the continuation it started, and
/// all describe what has *already happened* rather than what is being decided
/// (a decision already made is the clearest case of that). Counting any would
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

/// `input` with the reserved host-threaded ledger keys removed. A non-object
/// input is returned as-is — there is nothing to strip.
///
/// All four keys, and none is optional: a continuation's input differs from
/// the paused run's by exactly these — the delivery ledger (#438), the
/// outward-call ledger (#846), the denial ledger (#978) and an answered node
/// blocker (#2005) — so letting any of those differences count would make every
/// continuation gate a "new" decision and stack a duplicate card.
fn without_ledger(mut input: Value) -> Value {
    if let Value::Object(map) = &mut input {
        map.remove(CONTINUATION_DELIVERED_KEY);
        map.remove(CONTINUATION_PERFORMED_KEY);
        map.remove(CONTINUATION_DENIED_KEY);
        map.remove(CONTINUATION_BLOCKER_KEY);
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
    let node_id = required_str(effect, PAYLOAD_NODE_ID)?;
    // The legacy single-gate shape: one approval, one continuation. Issue #978's
    // run-scoped path goes through [`resume_run`] and carries the whole batch;
    // this arm stays exactly as it was for a card with no run turn key.
    let approved = [node_id.to_string()];
    spawn_continuation(runtime, effect, &approved, &[]).await
}

/// Re-dispatches a workflow run **once**, for every gate its batch approved
/// (issue #978).
///
/// The run-scoped replacement for [`resume_from_effect`], and the half of the
/// fix that stops the amplification rather than merely reporting it correctly.
/// It is reached from `continue_turn` when the released turn key names a run
/// (see [`run_id_from_turn`]), which happens exactly once per run — the
/// continuation queue's counting decides who releases, under one lock.
///
/// Three things it does that N independent re-dispatches could not:
///
/// * **one run, not N.** The paused run is replayed once, so the run table stops
///   growing by N per approval round.
/// * **every approval, not one.** The trigger input carries the whole approved
///   set, so a sibling gate does not pause the replay and park itself again.
/// * **refusals are final.** Denied and expired nodes ride the denial ledger, so
///   the replay neither runs them nor asks about them a second time. Without it
///   a mixed verdict would still net new cards, which is the invariant this
///   issue is about.
///
/// # Nothing approved
///
/// A batch whose every gate was denied or expired starts **no run**, and that is
/// the complete outcome rather than a gap: the paused run settled long ago, so
/// there is nothing to cancel, and replaying it would only pause at the first
/// refused node. The approved work of a *mixed* verdict is not discarded — that
/// case has a non-empty approved set and runs.
pub async fn resume_run(runtime: &CompanyRuntime, turn: &str) -> Result<()> {
    let released = match runtime.workflow_gates().release(turn) {
        Err(crate::runtime::workflow_gates::ReleaseRefusal::EmergencyStop) => {
            return Err(OpenCompanyError::EmergencyStop(format!(
                "refusing to continue workflow turn `{turn}` while stopped; the decided batch \
                 stays intact and resumes once an operator releases the stop"
            )));
        }
        Ok(None) => {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "the decisions on `{turn}` are all in, but this host is no longer holding that \
                 run's parked gates, so there is nothing to continue — re-run the workflow"
            )));
        }
        Ok(Some(released)) => released,
    };
    if released.approved.is_empty() {
        tracing::info!(
            company = %runtime.id(),
            %turn,
            denied = released.denied.len(),
            "workflow: every gate on this run was refused, so no continuation runs"
        );
        // Issue #1991 review (`3903797619`): a wholly refused batch is a
        // terminal outcome — `resume_run` starts nothing, and no other path
        // ever comes back for this lineage's checkpoint thread — so its
        // `workflow-checkpoints` state is prunable exactly like the runner's
        // own cancel/settle/genuine-failure arms already do.
        prune_checkpoint_lineage_for_effect(runtime, &released.effect).await;
        return Ok(());
    }
    tracing::info!(
        company = %runtime.id(),
        %turn,
        approved = released.approved.len(),
        denied = released.denied.len(),
        "workflow: the run's gates are all decided; starting ONE continuation for the batch"
    );
    if let Err(error) = spawn_continuation(
        runtime,
        &released.effect,
        &released.approved,
        &released.denied,
    )
    .await
    {
        // Every error here is terminal for this lineage: `release(turn)`
        // above already took the batch out of `workflow_gates()` for good,
        // so nothing will retry `released.effect`'s checkpoint thread. Prune
        // it the same way the all-denied arm above does.
        prune_checkpoint_lineage_for_effect(runtime, &released.effect).await;
        return Err(error);
    }
    Ok(())
}

/// What an approved workflow gate does at the moment its effect is performed
/// (issue #978).
///
/// This is the seam the amplification lived on. `perform_effect` fires once per
/// approved effect, so spawning here spawned **per approval** — three approvals
/// on one run meant three runs, each replaying the graph with one usable
/// approval and re-parking the rest. The spawn therefore moves to the batch
/// release, and this arm only decides which of the two paths a card is on:
///
/// * **run-scoped** — the card's run has a batch armed, so the decision is
///   banked and the continuation is owed by whichever decision turns out to be
///   the last. Nothing runs now, and the operator is told so by the
///   still-waiting receipt.
/// * **unarmed** — a card parked by a build from before this issue (its journal
///   line carries no turn key), or one whose batch this process no longer holds.
///   It re-dispatches immediately, exactly as it always did, because there is no
///   batch coming to release it and deferring would strand the run forever.
pub async fn on_gate_approved(runtime: &CompanyRuntime, effect: &Effect) -> Result<()> {
    if let Some(run_id) = effect.run_id.as_deref() {
        let turn = workflow_turn_key(run_id);
        if runtime.workflow_gates().is_armed(&turn) {
            tracing::debug!(
                company = %runtime.id(),
                %run_id,
                undecided = runtime.workflow_gates().undecided(&turn),
                "workflow: gate approved; the run continues once its remaining gates are decided"
            );
            return Ok(());
        }
    }
    resume_from_effect(runtime, effect).await
}

/// Starts one continuation run for `effect`'s workflow, with `approved` cleared
/// and `denied` recorded.
///
/// Shared by the legacy per-card path and issue #978's run-scoped one so the two
/// cannot drift on how a continuation is loaded, spawned or logged.
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
async fn spawn_continuation(
    runtime: &CompanyRuntime,
    effect: &Effect,
    approved: &[String],
    denied: &[String],
) -> Result<()> {
    let workflow_id = required_str(effect, PAYLOAD_WORKFLOW_ID)?;

    // Through the runtime's own accessor so a build without workflow execution
    // gives an honest error instead of a compile-time edge — this module is in
    // the default build, where `src/workflows` does not exist at all.
    let Some(runner) = runtime.workflow_runner().cloned() else {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "approved the gate on workflow `{workflow_id}`, but this runtime has no workflow \
             execution wired, so there is nothing to continue"
        )));
    };

    let (overlays, disable) = runtime
        .store()
        .load(runtime.id())
        .await?
        .map(|record| (record.overlay_workflows, record.manifest.globals.disable))
        .unwrap_or_default();
    let workflow =
        load_workflow_with_globals(runtime.source_dir(), &overlays, &disable, workflow_id)?
            .ok_or_else(|| {
                OpenCompanyError::CompanyNotFound(format!(
                    "workflow {workflow_id} (it was approved, but the graph no longer exists)"
                ))
            })?;
    if a_contested_id_no_longer_holds_the_parked_graph(
        effect
            .payload
            .get(PAYLOAD_WORKFLOW_FINGERPRINT)
            .and_then(Value::as_str),
        &workflow,
        &disable,
    ) {
        return Err(OpenCompanyError::CompanyNotFound(format!(
            "workflow {workflow_id} (it was approved, but the graph it parked against is gone)"
        )));
    }

    let input = continuation_input(effect, approved, denied)?;
    // Issue #1862 prerequisite: carry the paused run's attribution into the
    // continuation instead of letting `spawn`'s `scheduled: false` reset it to
    // `Operator` — see `started_by_of`.
    let started_by = started_by_of(effect);
    // The handle is dropped on purpose. The task holds its own guard, journals
    // its own outcome and deregisters itself; awaiting it here would hold the
    // approvals request open for the length of a whole workflow run, which is
    // the drop-safety failure issue #380 already paid for once.
    // Issue #542: resuming an approved gate is always a real run — `false`.
    // Issue #401: `spawn` refuses at the concurrency ceiling; propagate it so
    // the approval-resume caller surfaces the same `WorkflowRunLimit` refusal.
    // Issue #978 sharpens why that must be surfaced rather than logged: a batch
    // gets ONE spawn attempt, and every card that would have retried it is
    // already consumed, so a swallowed refusal loses the run with no way back.
    let checkpoint_thread_id = effect
        .payload
        .get(PAYLOAD_THREAD_ID)
        .and_then(Value::as_str);
    let node_restart = checkpoint_resume_available(runtime, checkpoint_thread_id).await
        && graph_unchanged_since_park(effect, &workflow);
    let ws = WorkflowSpawn::new(runtime, runner);
    let (ctx, guard) = runtime.run_supervisor().begin(&workflow.id, false)?;
    let ctx = ctx.with_started_by(started_by);
    let ctx = if node_restart {
        ctx.with_checkpoint_resume(
            checkpoint_thread_id.expect("checked checkpoint lineage"),
            approved.to_vec(),
            denied.to_vec(),
        )
    } else {
        // Issue #1991 review (`3904304781`): this fallback starts a fresh
        // trigger run rather than resuming `checkpoint_thread_id`'s lineage —
        // whether because no checkpoint was ever taken (a no-op prune below)
        // or because `graph_unchanged_since_park` just rejected a stale one
        // (a real lineage nothing else will ever come back for). Pruning here
        // closes the same leak `resume_run`'s all-denied arm already closes
        // for its own terminal exit: an edit-while-gated run that lands here
        // must not leave an unreachable lineage under `workflow-checkpoints`.
        if let Some(thread_id) = checkpoint_thread_id {
            prune_checkpoint_lineage(runtime, thread_id).await;
        }
        ctx.with_resume_semantic(crate::ports::ResumeSemantic::ReRunFromTrigger)
    };
    let (run_id, _handle) = ws.spawn_admitted(ctx, guard, workflow, input, false);
    tracing::info!(
        company = %runtime.id(),
        workflow = %workflow_id,
        approved = ?approved,
        denied = ?denied,
        %run_id,
        semantic = if node_restart { "nodeRestart" } else { "reRunFromTrigger" },
        "workflow: an approved gate started a continuation run"
    );
    Ok(())
}

/// Re-dispatches the workflow run a **blocked agent node** belonged to, by
/// starting a fresh supervised run with the paused run's own trigger input
/// (issue #899, Stage 1).
///
/// # Why this is simpler than [`spawn_continuation`]
///
/// A `requires_approval` gate is a graph node, so approving it threads the
/// node id into the trigger's `approvals` array and the re-run proceeds past it.
/// A gated call *inside an agent node's tool loop* is not a graph node — there
/// is nothing to add to `approvals`. The re-run simply runs the graph again from
/// the top; the grant minted when the operator approved (a shared
/// [`GrantSet`](crate::runtime::grants::GrantSet), redeemed at the top of the
/// policy's `check`) lets the identical call through without re-parking. So this
/// spawns with the input **unchanged** — no ledger transform, no approvals set.
///
/// # The honest Stage-1 limit
///
/// If the model **diverges** on the re-run (different arguments, or an extra
/// call), the grant does not match and the new call parks a fresh card. That is
/// a genuinely new decision rather than a loop, but it is a re-ask — Stage 2's
/// at-most-once grant capture is what closes it. Re-delivery of any report an
/// earlier partial run already sent is guarded independently by the durable
/// #529 delivery ledger the runner folds in, so it is not re-threaded here.
///
/// # Errors
///
/// Propagated, on [`spawn_continuation`]'s terms: the approval is already
/// committed, so a graph that has since been deleted or a build with no workflow
/// execution has to reach the operator at click time, not vanish.
///
/// # Marking the dispatch (issue #1825, P1 follow-up)
///
/// `turn` is threaded in so the durable
/// [`BlockedNodeDispatched`](crate::runtime::journal::JournalRecord::BlockedNodeDispatched)
/// marker can be banked **from inside this function**, between
/// [`RunSupervisor::begin`](crate::runtime::RunSupervisor::begin) admitting the
/// run and [`WorkflowSpawn::spawn_admitted`] actually launching its detached
/// task — not, as the marker's first cut had it, after this whole function
/// returns to `resume_blocked_agent_node`. That ordering left the marker
/// racing the *entire* detached run: `spawn`'s own doc is explicit that the
/// caller does not await the task it launches, so a crash any time between
/// launch and the write landing — however long the graph took to run — looked
/// identical to a strand and could re-dispatch a continuation that had
/// already finished. `begin` and the write it gates are both on this
/// function's own stack with no detached task between them yet, so the same
/// crash window now spans only the synchronous handful of instructions
/// between the write's `.await` returning and `spawn_admitted` being called —
/// no further `.await` sits in between for anything to preempt.
///
/// **A write that outright fails, as opposed to a crash racing it, is a
/// different case and gets `?`-propagated rather than warned past.** The
/// first cut of this fix logged the failure and launched anyway, which broke
/// the invariant every read of this marker depends on — "absent" must mean
/// "never launched". A crash between that unmarked launch and
/// [`BlockedNodeReleased`](crate::runtime::journal::JournalRecord::BlockedNodeReleased)
/// landing left a run genuinely in flight with nothing durable saying so, and
/// `reconcile_stranded_blocked_nodes` re-dispatched it a second time at the
/// next boot. Propagating instead means this attempt is abandoned before
/// `spawn_admitted` ever runs — `begin`'s guard drops, freeing the
/// concurrency slot it briefly held — and the caller's own retry
/// classification (`CompanyRuntime::is_retryable_dispatch_failure`) already
/// treats this write's error type as retryable, so the stash and its
/// approval stay exactly as durably recoverable as before this attempt.
///
/// The crash-race window above this note is **not** closed by that change,
/// and is not something a caller can retry its way out of: a crash between
/// the write landing and `spawn_admitted` running leaves a durable marker for
/// a run that never actually started, which `reconcile_stranded_blocked_nodes`
/// reads as "already dispatched" and permanently retires without ever
/// launching. Closing that fully needs a durable record this single boolean
/// marker cannot express — something that distinguishes "dispatch attempted"
/// from "dispatch confirmed", written from inside the launched task itself
/// rather than by its caller — not a heuristic guess at this call site.
pub async fn spawn_blocked_node_continuation(
    runtime: &CompanyRuntime,
    turn: &str,
    workflow_id: &str,
    input: Value,
    started_by: crate::ports::types::StartedBy,
    checkpoint_thread_id: Option<String>,
    workflow_fingerprint: Option<String>,
) -> Result<()> {
    let Some(runner) = runtime.workflow_runner().cloned() else {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "approved a blocked step on workflow `{workflow_id}`, but this runtime has no \
             workflow execution wired, so there is nothing to continue"
        )));
    };
    let (overlays, disable) = runtime
        .store()
        .load(runtime.id())
        .await?
        .map(|record| (record.overlay_workflows, record.manifest.globals.disable))
        .unwrap_or_default();
    let workflow =
        load_workflow_with_globals(runtime.source_dir(), &overlays, &disable, workflow_id)?
            .ok_or_else(|| {
                OpenCompanyError::CompanyNotFound(format!(
                    "workflow {workflow_id} (a blocked step was approved, but the graph no longer \
             exists)"
                ))
            })?;
    if a_contested_id_no_longer_holds_the_parked_graph(
        workflow_fingerprint.as_deref(),
        &workflow,
        &disable,
    ) {
        return Err(OpenCompanyError::CompanyNotFound(format!(
            "workflow {workflow_id} (a blocked step was approved, but the graph it parked \
             against is gone)"
        )));
    }
    // Issue #401: `begin` refuses at the concurrency ceiling; propagate it so
    // the caller surfaces the same refusal rather than losing the run
    // silently. Deliberately split from `spawn_admitted` below (mirroring the
    // cron scheduler's own begin/claim ordering, issue #661) rather than
    // calling the combined `WorkflowSpawn::spawn` — admission has to land
    // (and can still cleanly fail) *before* the dispatch marker is written,
    // so a refusal here writes no marker for a run that never started.
    let ws = WorkflowSpawn::new(runtime, runner);
    let (ctx, guard) = runtime.run_supervisor().begin(&workflow.id, false)?;
    // Issue #1862 prerequisite: `started_by` is the blocked run's own
    // attribution, stashed at block-settle by `BlockedNodeQueue::arm` and
    // handed back on release — stamped onto the admitted context here so it
    // overrides `begin`'s `scheduled`-derived `Operator` default, the same as
    // `spawn_as` does for the gate path in `spawn_continuation`. Done on the
    // already-admitted `ctx` (rather than via `spawn_as`, which owns its own
    // `begin` call) so this still gets the split-`begin`/dispatch-marker
    // ordering below.
    // Issue #1991 review (`3904397452`/`3904304754`): the blocked-node twin of
    // `spawn_continuation`'s own `graph_unchanged_since_park` check — a
    // checkpoint being available says nothing about whether the graph it
    // describes is still the one this node blocked against. Without this, an
    // agent node that blocks on approval and gets edited before that approval
    // lands would resume the stale checkpoint into the edited graph: stale
    // completed-node state, or an engine failure where the topology no longer
    // matches.
    let node_restart = checkpoint_resume_available(runtime, checkpoint_thread_id.as_deref()).await
        && fingerprint_unchanged_since_park(workflow_fingerprint.as_deref(), &workflow);
    let ctx = ctx.with_started_by(started_by);
    let ctx = if node_restart {
        ctx.with_checkpoint_resume(
            checkpoint_thread_id
                .as_deref()
                .expect("checked checkpoint lineage"),
            Vec::new(),
            Vec::new(),
        )
    } else {
        // Same leak class `spawn_continuation`'s own fallback closes
        // (`3904304781`): falling back to a trigger re-run — whether because
        // no checkpoint was ever taken, or because the fingerprint check just
        // rejected a stale one — must not leave `checkpoint_thread_id`'s
        // lineage behind. Nothing else ever comes back for it once this block
        // is about to re-dispatch on the trigger input instead.
        if let Some(thread_id) = checkpoint_thread_id.as_deref() {
            prune_checkpoint_lineage(runtime, thread_id).await;
        }
        ctx.with_resume_semantic(crate::ports::ResumeSemantic::ReRunFromTrigger)
    };
    // Issue #1825 (P1 follow-up): abort rather than launch unmarked. Warning
    // and proceeding anyway broke the exact invariant `BlockedNodeDispatched`'s
    // own doc comment depends on — "no marker" must mean "nothing launched",
    // or `reconcile_stranded_blocked_nodes` can no longer tell a genuine
    // strand apart from a run this very call already started, and re-spawns a
    // continuation that is already under way (repeating token spend or
    // unprotected upstream work). Propagating drops `guard` here, freeing the
    // concurrency slot without `spawn_admitted` ever running, and this
    // function's only caller (`resume_blocked_agent_node`) already classifies
    // a durable-store failure as retryable — the stash and its approval stay
    // recorded exactly as they were, for a later boot's
    // `reconcile_stranded_blocked_nodes` to pick back up, instead of this
    // call quietly deciding on its own that an unmarked launch was fine.
    // Specifically a *later boot's*: this journal's in-memory
    // `blocked_node_dispatched` mirror (like every sibling `record_*` on it)
    // is written before the durable append it guards even runs and is not
    // rolled back on failure, so calling `reconcile_stranded_blocked_nodes`
    // again in *this* process would read a stale "dispatched" for `turn` and
    // retire the stash without ever having launched it. Safe only from a
    // fresh boot's replay, which correctly excludes a write that never
    // durably landed.
    runtime.journal.record_blocked_node_dispatched(turn).await?;
    // Issue #542: a resumed run is always real (`false`). Nothing here can
    // fail — `begin`'s ceiling check already ran — so the task exists the
    // moment this returns, immediately after the write above.
    let (run_id, _handle) = ws.spawn_admitted(ctx, guard, workflow, input, false);
    tracing::info!(
        company = %runtime.id(),
        workflow = %workflow_id,
        %run_id,
        semantic = if node_restart { "nodeRestart" } else { "reRunFromTrigger" },
        "workflow: an approved agent-node call started a continuation run"
    );
    Ok(())
}

/// Whether `effect`'s parked workflow fingerprint (if it stashed one) still
/// matches `workflow`'s current [`content_fingerprint`](crate::company::WorkflowFile::content_fingerprint).
///
/// `true` when the parked effect carries no `PAYLOAD_WORKFLOW_FINGERPRINT` —
/// a card parked before this check existed — so it behaves exactly as it did
/// before. `false` only when a fingerprint WAS stashed and no longer matches:
/// the workflow was edited while this approval sat pending, so its checkpoint
/// no longer describes the graph the continuation would run against.
fn graph_unchanged_since_park(effect: &Effect, workflow: &crate::company::WorkflowFile) -> bool {
    fingerprint_unchanged_since_park(
        effect
            .payload
            .get(PAYLOAD_WORKFLOW_FINGERPRINT)
            .and_then(Value::as_str),
        workflow,
    )
}

/// Whether an id a global could answer to resolved to a graph this run did not
/// park against.
///
/// A company graph and a global can hold one id, and either can replace the
/// other while a run sits parked: deleting the company's copy surfaces the
/// global, and authoring one buries it. Both directions end with a graph that
/// is not what the operator answered for, and a trigger re-run there executes
/// nodes nobody approved.
///
/// Keyed on the id being contested rather than on which side won, so the two
/// directions cannot drift apart. An id no global answers to is untouched: an
/// edited company graph is the operator's own edit of their own graph, and
/// still falls back to a trigger re-run. A run parked before fingerprints were
/// stashed still replays as it did.
fn a_contested_id_no_longer_holds_the_parked_graph(
    parked: Option<&str>,
    workflow: &crate::company::WorkflowFile,
    disable: &[String],
) -> bool {
    let contested = !crate::globals::disabled(disable, "workflow", &workflow.id)
        && crate::globals::workflows()
            .iter()
            .any(|global| global.id == workflow.id);
    contested && parked.is_some_and(|parked| parked != workflow.content_fingerprint())
}

/// The shared check behind [`graph_unchanged_since_park`] (the gate path,
/// which reads its parked fingerprint off an [`Effect`]) and
/// [`spawn_blocked_node_continuation`] (the blocked-node path, which reads
/// its parked fingerprint off a
/// [`StashedBlock`](crate::runtime::blocked_nodes::StashedBlock) — a parked
/// tool call has no effect payload of its own to carry one).
///
/// `true` when `parked` is `None` — a card stashed before either path tracked
/// a fingerprint — so an upgrade replays exactly as it did before this check
/// existed. `false` only when a fingerprint WAS stashed and no longer
/// matches: the workflow was edited while this approval or block sat
/// pending, so its checkpoint no longer describes the graph the continuation
/// would run against.
fn fingerprint_unchanged_since_park(
    parked: Option<&str>,
    workflow: &crate::company::WorkflowFile,
) -> bool {
    let Some(parked) = parked else {
        return true;
    };
    if parked == workflow.content_fingerprint() {
        return true;
    }
    tracing::info!(
        workflow = %workflow.id,
        "workflow: the graph changed while this run was paused for approval; falling back to a \
         trigger re-run instead of resuming a stale checkpoint into the edited graph"
    );
    false
}

/// Prunes `thread_id`'s stashed checkpoint lineage, when this build has a
/// checkpoint store wired.
///
/// The core behind [`prune_checkpoint_lineage_for_effect`] (the gate path,
/// keyed off an [`Effect`]'s `PAYLOAD_THREAD_ID`) and
/// [`spawn_blocked_node_continuation`]'s own fingerprint-rejection exit,
/// which already holds its thread id directly off `StashedBlock` with no
/// effect to read it from.
#[cfg(feature = "openhuman")]
async fn prune_checkpoint_lineage(runtime: &CompanyRuntime, thread_id: &str) {
    let Some(store) = runtime.workflow_checkpoints() else {
        return;
    };
    if let Err(error) = store.prune_settled(thread_id).await {
        tracing::warn!(%thread_id, %error, "workflow: failed to prune settled checkpoints");
    }
}

#[cfg(not(feature = "openhuman"))]
async fn prune_checkpoint_lineage(_runtime: &CompanyRuntime, _thread_id: &str) {}

/// Prunes `effect`'s stashed checkpoint lineage, when it has one and this
/// build has a checkpoint store wired.
///
/// A thin wrapper so the two terminal-refusal call sites — this module's
/// [`resume_run`] and `CompanyRuntime::resume_blocked_agent_node`'s own
/// all-refused arm — can prune the same way the workflow runner's cancel,
/// clean-settle and genuine-failure arms already do (issue #1991 review,
/// `3903797619`), without either of those default-build call sites needing to
/// know that `workflow_checkpoints()` only exists under the `openhuman`
/// feature.
async fn prune_checkpoint_lineage_for_effect(runtime: &CompanyRuntime, effect: &Effect) {
    let Some(thread_id) = effect
        .payload
        .get(PAYLOAD_THREAD_ID)
        .and_then(Value::as_str)
    else {
        return;
    };
    prune_checkpoint_lineage(runtime, thread_id).await;
}

#[cfg(feature = "openhuman")]
async fn checkpoint_resume_available(runtime: &CompanyRuntime, thread_id: Option<&str>) -> bool {
    let (Some(store), Some(thread_id)) = (runtime.workflow_checkpoints(), thread_id) else {
        return false;
    };
    match store.has_resume_point(thread_id).await {
        Ok(available) => available,
        Err(error) => {
            tracing::warn!(%thread_id, %error, "workflow: checkpoint lookup failed; falling back to trigger re-run");
            false
        }
    }
}

#[cfg(not(feature = "openhuman"))]
async fn checkpoint_resume_available(_runtime: &CompanyRuntime, _thread_id: Option<&str>) -> bool {
    false
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
pub(crate) fn continuation_input(
    effect: &Effect,
    approved: &[String],
    newly_denied: &[String],
) -> Result<Value> {
    if approved.is_empty() {
        return Err(OpenCompanyError::InvalidRequest(
            "a workflow continuation was asked for with no approved gate, so the run would \
             pause again at the node it started for"
                .to_string(),
        ));
    }
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
    // Issue #846: the outward-call ledger travels on the same terms and for the
    // same reason. An input that carries the approval but not this resumes and
    // **re-sends**, which is #438 on the node the host does not route itself.
    let performed: Vec<PerformedCall> = effect
        .payload
        .get(PAYLOAD_PERFORMED)
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| serde_json::from_value(row.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    // Issue #978: the refusals this lineage has accumulated, plus the ones this
    // batch just made. Unioned rather than replaced, on `delivery_ledger`'s
    // reasoning exactly — a two-gate graph must not forget the first gate's
    // denial when the second is decided.
    let mut denied: Vec<String> = effect
        .payload
        .get(PAYLOAD_DENIED)
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for node in newly_denied {
        if !denied.contains(node) {
            denied.push(node.clone());
        }
    }
    Ok(with_denied(
        with_performed(
            with_delivered(with_approvals(input, approved), &delivered),
            &performed,
        ),
        &denied,
    ))
}

/// Writes `denied` onto the trigger input under [`CONTINUATION_DENIED_KEY`],
/// replacing whatever was there (issue #978).
///
/// Replace rather than merge, on [`with_delivered`]'s reasoning: the union was
/// already taken in [`continuation_input`], so this is the superset and merging
/// again would be a second place for the rule to drift. An empty ledger writes
/// nothing, keeping a first run's input shape untouched.
fn with_denied(input: Value, denied: &[String]) -> Value {
    if denied.is_empty() {
        return input;
    }
    match input {
        Value::Object(mut map) => {
            map.insert(
                CONTINUATION_DENIED_KEY.to_string(),
                serde_json::json!(denied),
            );
            Value::Object(map)
        }
        // `with_approvals` always yields an object, so this is unreachable
        // through `continuation_input`. Kept total rather than panicking.
        other => other,
    }
}

/// Writes `performed` onto the trigger input under
/// [`CONTINUATION_PERFORMED_KEY`], replacing whatever was there.
///
/// Replace rather than merge, on [`with_delivered`]'s reasoning exactly: the
/// card's ledger was *built* by unioning the input's own list with what the run
/// performed ([`performed_ledger`]), so it is already the superset, and merging
/// again here would be a second place for that rule to drift.
///
/// An empty ledger writes nothing at all, so a first run's input shape is
/// untouched and the reserved key appears only once there is something to
/// suppress.
fn with_performed(input: Value, performed: &[PerformedCall]) -> Value {
    if performed.is_empty() {
        return input;
    }
    match input {
        Value::Object(mut map) => {
            map.insert(
                CONTINUATION_PERFORMED_KEY.to_string(),
                serde_json::json!(performed),
            );
            Value::Object(map)
        }
        // `with_approval` always yields an object, so this is unreachable
        // through `continuation_input`. Kept total rather than panicking.
        other => other,
    }
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

/// Unions `node_ids` into the trigger input's `approvals` array.
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
fn with_approvals(input: Value, node_ids: &[String]) -> Value {
    let mut approvals: Vec<String> = input
        .get("approvals")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // Issue #978: **every** gate the run's batch approved, not just the one
    // whose card happened to be clicked last. Carrying one is what made a
    // three-way fan-out re-park its two siblings on every continuation.
    for node_id in node_ids {
        if !approvals.iter().any(|id| id == node_id) {
            approvals.push(node_id.clone());
        }
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

    /// A continuation for a card's own single gate — the pre-#978 shape these
    /// tests were written against, kept so they keep asserting what they were
    /// written to assert. The run-scoped multi-gate form is exercised in
    /// `crate::workflows::parallel_gate_fanout_test`.
    fn single_continuation_input(effect: &Effect) -> Result<Value> {
        let node_id = required_str(effect, PAYLOAD_NODE_ID)?.to_string();
        continuation_input(effect, std::slice::from_ref(&node_id), &[])
    }

    fn effect(workflow: &str, node: &str, input: Value) -> Effect {
        gate_effect(workflow, node, &input, "run-1", &[], &[], None)
    }

    /// A gate whose call the host classified — the shape issue #846 writes and
    /// issue #1098 reads back.
    fn gate_with_call(tool: &str, args: &Value) -> Effect {
        gate_effect(
            "sports_blog",
            "fetch_bbc",
            &json!({}),
            "run-1",
            &[],
            &[],
            Some(GateCall {
                tool,
                reason: None,
                args: Some(args),
                target: None,
            }),
        )
    }

    #[test]
    fn gate_inner_call_reads_the_call_the_card_shows() {
        let args = json!({ "url": "https://www.bbc.co.uk/sport" });
        let gate = gate_with_call("web_fetch", &args);

        let (tool, read_back) = gate_inner_call(&gate).expect("a classified gate names its call");
        assert_eq!(tool, "web_fetch");
        assert_eq!(read_back, &args);
    }

    /// The wrapper is what a naive classifier sees, and it is not the call. This
    /// is the whole reason the projection exists.
    #[test]
    fn gate_inner_call_is_not_the_effect_kind() {
        let gate = gate_with_call("web_fetch", &json!({ "url": "https://www.bbc.co.uk" }));
        assert_eq!(gate.kind, WORKFLOW_APPROVE_KIND);
        assert_eq!(
            gate_inner_call(&gate).map(|(tool, _)| tool),
            Some("web_fetch")
        );
    }

    /// A gate the host could not classify has no inner call to offer, and must
    /// not invent one — it stays per-call, as it always did.
    #[test]
    fn gate_inner_call_is_none_when_the_call_was_never_named() {
        let bare = effect("sports_blog", "fetch_bbc", json!({}));
        assert!(gate_inner_call(&bare).is_none());
    }

    /// `args` is written only when the node had some, so tool-without-args is an
    /// ordinary shape. It answers `Null`, which every argument-aware classifier
    /// resolves in the cautious direction.
    #[test]
    fn gate_inner_call_answers_null_for_a_call_with_no_arguments() {
        let gate = gate_effect(
            "sports_blog",
            "fetch_bbc",
            &json!({}),
            "run-1",
            &[],
            &[],
            Some(GateCall {
                tool: "web_fetch",
                reason: None,
                args: None,
                target: None,
            }),
        );
        assert_eq!(gate_inner_call(&gate), Some(("web_fetch", &Value::Null)));
    }

    /// Kind-checked for the same reason [`gate_node_id`] is: a teammate's own
    /// `web_fetch` effect carries a tool name too, and must not be read as a
    /// gate.
    #[test]
    fn gate_inner_call_ignores_a_non_gate_effect() {
        let mut agent_call = gate_with_call("web_fetch", &json!({ "url": "https://x.test" }));
        agent_call.kind = "web_fetch".to_string();
        assert!(gate_inner_call(&agent_call).is_none());
    }

    /// The second half of why a job card was never grantable, independent of its
    /// `agent: None`. Classifying the wrapper asks about `workflow.approve`, an
    /// undeclared name that fails closed; classifying the inner call asks about
    /// the `web_fetch` the operator is looking at.
    #[test]
    fn a_gate_is_classified_by_the_call_it_stops_not_by_its_wrapper() {
        let gate = gate_with_call(
            crate::policy::consequence::WEB_FETCH,
            &json!({ "url": "https://www.bbc.co.uk/sport" }),
        );

        assert!(
            !crate::policy::consequence_of(&gate.kind, &gate.payload)
                .standing
                .is_grantable(),
            "the wrapper kind is undeclared and must keep failing closed on its own"
        );
        assert!(
            gate.may_be_granted_standing(),
            "a BBC fetch is ScopedGrantable, and that is the call the card shows"
        );
    }

    /// A gate stopping a per-call tool stays per-call. Reading the inner call
    /// must widen nothing on its own — the classifier still decides.
    #[test]
    fn a_gate_stopping_a_per_call_tool_is_still_not_grantable() {
        let gate = gate_with_call("shell", &json!({ "command": "echo hi" }));
        assert!(!gate.may_be_granted_standing());
    }

    /// No readable host means no scope, and `ScopedGrantable` falls back to
    /// per-call rather than minting a permission that would admit everything.
    #[test]
    fn a_gate_whose_url_names_no_host_is_still_not_grantable() {
        let gate = gate_with_call(crate::policy::consequence::WEB_FETCH, &json!({}));
        assert!(!gate.may_be_granted_standing());
    }

    #[test]
    fn gate_workflow_id_names_the_permission_subject() {
        let gate = effect("sports_blog", "fetch_bbc", json!({}));
        assert_eq!(gate_workflow_id(&gate), Some("sports_blog"));

        let mut not_a_gate = gate.clone();
        not_a_gate.kind = "web_fetch".to_string();
        assert!(gate_workflow_id(&not_a_gate).is_none());
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
        let out = with_approvals(
            serde_json::json!({ "request": "topic" }),
            &["gate".to_string()],
        );
        assert_eq!(out["request"], "topic", "the original input is preserved");
        assert_eq!(out["approvals"], serde_json::json!(["gate"]));
    }

    #[test]
    fn a_second_gate_does_not_un_approve_the_first() {
        // The two-gate graph. Without the union the re-run pauses at the gate
        // the operator already cleared and the workflow can never finish.
        let first = with_approvals(
            serde_json::json!({ "request": "topic" }),
            &["gate-a".to_string()],
        );
        let second = with_approvals(first, &["gate-b".to_string()]);
        assert_eq!(second["approvals"], serde_json::json!(["gate-a", "gate-b"]));
    }

    #[test]
    fn approving_the_same_gate_twice_does_not_duplicate_it() {
        let once = with_approvals(serde_json::json!({}), &["gate".to_string()]);
        let twice = with_approvals(once, &["gate".to_string()]);
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
            let out = with_approvals(input.clone(), &["gate".to_string()]);
            assert_eq!(
                out["approvals"],
                serde_json::json!(["gate"]),
                "input {input} must still produce a resumable trigger"
            );
        }
    }

    #[test]
    fn non_string_entries_in_a_prior_approvals_array_are_dropped() {
        let out = with_approvals(
            serde_json::json!({ "approvals": ["a", 7, null] }),
            &["b".to_string()],
        );
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
            &[],
            None,
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
                &[],
                None,
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
            &[],
            None,
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
            &[],
            None,
        );

        let input = single_continuation_input(&card).expect("a well-formed card continues");

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
            &[],
            None,
        );
        let continuation = single_continuation_input(&first).expect("continues");

        // Run 2 skips the summary (delivering nothing) and pauses on gate-b.
        let second = gate_effect("digest", "gate-b", &continuation, "run-2", &[], &[], None);
        assert_eq!(
            ledger(&second),
            vec![DeliveredReport {
                node: "summary".into(),
                kind: "owner".into()
            }],
            "the second card must remember what the first run sent"
        );

        // And approving THAT still suppresses it, with both gates approved.
        let next = single_continuation_input(&second).expect("continues");
        assert_eq!(next["approvals"], serde_json::json!(["gate-a", "gate-b"]));
        assert_eq!(delivered_in_input(&next).len(), 1);
    }

    // --- issue #846: the outward-call ledger -------------------------------

    /// One outward call, made once, across a whole lineage — **the headline
    /// claim for issue #846**, in the shape #496 proved for the delivery half.
    ///
    /// A `POST` upstream of two gates. Run 1 fires it and pauses; approving
    /// starts run 2, which must NOT fire it again and must pause on the second
    /// gate; approving that starts run 3, which must not fire it either. The
    /// ledger has to accumulate down the lineage or run 3 posts for real —
    /// exactly the trap the delivery ledger's own two-gate test exists for.
    ///
    /// Asserted on the ledger the card carries and the input it produces,
    /// because those are what decide whether the call is made: the graph rewrite
    /// that consumes them is pinned in `crate::workflows::replay`, and the
    /// invoker arm that answers it is pinned in `crate::workflows::caps::tools`.
    #[test]
    fn an_outward_call_is_made_once_across_two_gates() {
        let posted = PerformedCall {
            node: "notify".into(),
            tool: "http_request POST".into(),
            result: serde_json::json!({ "status": 201 }),
        };

        // Run 1: the POST fires, the run pauses on gate-a.
        let first = gate_effect(
            "digest",
            "gate-a",
            &serde_json::json!({ "request": "x" }),
            "run-1",
            &[],
            std::slice::from_ref(&posted),
            None,
        );
        let continuation = single_continuation_input(&first).expect("continues");
        assert_eq!(
            performed_in_input(&continuation),
            vec![posted.clone()],
            "the continuation must know what run 1 already posted"
        );

        // Run 2: the first POST is replayed (this run does not repeat it), and
        // a SECOND outward node fires before the run pauses on gate-b.
        //
        // The second call is what makes this the real two-gate case rather than
        // a walk-through. A card carrying a non-empty ledger REPLACES the input's
        // key rather than merging into it — `with_performed` documents why — so
        // if `performed_ledger` did not union the input's own entries first, run
        // 2's card would carry only its own call and run 3 would post the first
        // one a second time. Reverting that union fails this test on the
        // `notify` entry alone.
        let also_posted = PerformedCall {
            node: "escalate".into(),
            tool: "http_request POST".into(),
            result: serde_json::json!({ "status": 202 }),
        };
        let second = gate_effect(
            "digest",
            "gate-b",
            &continuation,
            "run-2",
            &[],
            std::slice::from_ref(&also_posted),
            None,
        );
        let next = single_continuation_input(&second).expect("continues");

        assert_eq!(
            performed_in_input(&next),
            vec![posted, also_posted],
            "run 3 must be told about BOTH earlier posts, not just the last one"
        );
        assert_eq!(next["approvals"], serde_json::json!(["gate-a", "gate-b"]));
    }

    /// The earliest result in the lineage wins, and a node is listed once.
    ///
    /// The run that actually reached the counterparty is the one whose receipt
    /// downstream nodes saw, so a later run must not overwrite it — and a ledger
    /// that grew an entry per hop would bloat every card in a long lineage.
    #[test]
    fn the_outward_ledger_keeps_the_first_result_per_node() {
        let original = PerformedCall {
            node: "notify".into(),
            tool: "http_request POST".into(),
            result: serde_json::json!({ "id": "first" }),
        };
        let input = serde_json::json!({ CONTINUATION_PERFORMED_KEY: [original.clone()] });

        let card = gate_effect(
            "digest",
            "gate",
            &input,
            "run-2",
            &[],
            &[PerformedCall {
                node: "notify".into(),
                tool: "http_request POST".into(),
                result: serde_json::json!({ "id": "second" }),
            }],
            None,
        );

        let ledger: Vec<PerformedCall> = serde_json::from_value(
            card.payload
                .get(PAYLOAD_PERFORMED)
                .expect("carried")
                .clone(),
        )
        .expect("well-formed");
        assert_eq!(ledger, vec![original]);
    }

    /// A card with no outward ledger produces an input with no reserved key —
    /// so a first run's trigger payload keeps exactly the shape it always had.
    #[test]
    fn an_empty_outward_ledger_leaves_the_input_untouched() {
        let card = gate_effect(
            "digest",
            "gate",
            &serde_json::json!({ "request": "x" }),
            "run-1",
            &[],
            &[],
            None,
        );
        let input = single_continuation_input(&card).expect("continues");
        assert!(
            input.get(CONTINUATION_PERFORMED_KEY).is_none(),
            "nothing to suppress must write nothing: {input}"
        );
        assert!(performed_in_input(&input).is_empty());
    }

    /// The outward ledger is NOT part of a gate's identity.
    ///
    /// A continuation's input differs from the paused run's by exactly the
    /// reserved ledger keys, so counting either would make every continuation
    /// gate read as a new decision and stack a duplicate card for one question —
    /// the failure `is_same_gate` exists to prevent. The delivery half of this
    /// is pinned beside it; this is the same claim for the #846 key.
    #[test]
    fn the_outward_ledger_is_not_part_of_a_gates_identity() {
        let input = serde_json::json!({ "request": "x" });
        let paused = gate_effect("wf", "publish", &input, "run-1", &[], &[], None);

        let mut continuation = input.clone();
        continuation.as_object_mut().expect("object").insert(
            CONTINUATION_PERFORMED_KEY.to_string(),
            serde_json::json!([PerformedCall {
                node: "notify".into(),
                tool: "http_request POST".into(),
                result: serde_json::json!({ "status": 201 }),
            }]),
        );
        let re_reached = gate_effect("wf", "publish", &continuation, "run-2", &[], &[], None);

        assert!(
            is_same_gate(&paused, &re_reached),
            "a ledger key must not split one decision into two cards"
        );
    }

    /// A run that delivered nothing writes no reserved key at all, so an
    /// ordinary continuation's input keeps exactly the shape it always had.
    #[test]
    fn a_lineage_that_delivered_nothing_threads_no_reserved_key() {
        let card = effect("digest", "gate", serde_json::json!({ "request": "x" }));
        let input = single_continuation_input(&card).expect("continues");
        assert!(input.get(CONTINUATION_DELIVERED_KEY).is_none(), "{input}");
        assert!(delivered_in_input(&input).is_empty());
    }

    // ── Issue #2005: the answered-blocker key ────────────────────────────────

    fn resolution(
        verdict: crate::ports::blockers::BlockerVerdict,
        answer: &str,
    ) -> crate::ports::blockers::BlockerResolution {
        crate::ports::blockers::BlockerResolution::answered(verdict, answer)
    }

    /// A first run carries no answer, and reading one is not an error — it is
    /// the ordinary case for every node that never blocked.
    #[test]
    fn an_input_with_no_answered_blocker_reads_as_nothing() {
        let input = serde_json::json!({ "request": "x" });
        assert!(
            blocker_answers_in_input(&input)
                .expect("readable")
                .is_empty()
        );
        assert!(
            blocker_answer_for(&input, "draft")
                .expect("readable")
                .is_none()
        );
    }

    /// The headline of the thread: the verdict and the operator's words ride
    /// the continuation's trigger input, keyed by the node they re-enter.
    #[test]
    fn an_answer_rides_the_trigger_input_keyed_by_its_node() {
        use crate::ports::blockers::BlockerVerdict;
        let input = blocker_continuation_input(
            serde_json::json!({ "request": "x" }),
            "draft",
            &resolution(BlockerVerdict::Amend, "use gpt-4o-mini"),
        )
        .expect("continues");
        assert_eq!(input["request"], "x", "the blocked run's input is kept");
        let answer = blocker_answer_for(&input, "draft")
            .expect("readable")
            .expect("the answer is there");
        assert_eq!(answer.verdict, BlockerVerdict::Amend);
        assert_eq!(answer.answer, "use gpt-4o-mini");
        assert!(
            blocker_answer_for(&input, "other")
                .expect("readable")
                .is_none(),
            "an answer is about one node, not the graph"
        );
    }

    /// A two-blocker lineage must not forget the first node's answer when the
    /// second is decided — the same accumulation the denial ledger keeps.
    #[test]
    fn a_second_nodes_answer_does_not_forget_the_first() {
        use crate::ports::blockers::BlockerVerdict;
        let first = blocker_continuation_input(
            serde_json::json!({ "request": "x" }),
            "draft",
            &resolution(BlockerVerdict::Skip, ""),
        )
        .expect("continues");
        let second =
            blocker_continuation_input(first, "review", &resolution(BlockerVerdict::Retry, ""))
                .expect("continues");
        assert_eq!(
            blocker_answers_in_input(&second).expect("readable").len(),
            2
        );
        assert_eq!(
            blocker_answer_for(&second, "draft")
                .expect("readable")
                .expect("kept")
                .verdict,
            BlockerVerdict::Skip
        );
    }

    /// A node retried, blocked again and then skipped is skipped: the operator's
    /// newest decision is the one in force.
    #[test]
    fn a_later_answer_supersedes_an_earlier_one_for_the_same_node() {
        use crate::ports::blockers::BlockerVerdict;
        let first = blocker_continuation_input(
            serde_json::json!({ "request": "x" }),
            "draft",
            &resolution(BlockerVerdict::Retry, ""),
        )
        .expect("continues");
        let second =
            blocker_continuation_input(first, "draft", &resolution(BlockerVerdict::Skip, ""))
                .expect("continues");
        assert_eq!(
            blocker_answers_in_input(&second).expect("readable").len(),
            1,
            "one node carries one live decision"
        );
        assert_eq!(
            blocker_answer_for(&second, "draft")
                .expect("readable")
                .expect("kept")
                .verdict,
            BlockerVerdict::Skip
        );
    }

    /// The asymmetry with the ledgers, pinned: an unreadable answer is loud.
    /// Degrading it to "nobody answered" would re-run the node into the
    /// identical failure with the operator's decision gone.
    #[test]
    fn an_unreadable_answer_is_loud_rather_than_ignored() {
        let not_a_list = serde_json::json!({ CONTINUATION_BLOCKER_KEY: "retry" });
        assert!(blocker_answers_in_input(&not_a_list).is_err());

        let unknown_verdict = serde_json::json!({
            CONTINUATION_BLOCKER_KEY: [{ "node": "draft", "verdict": "shrug" }]
        });
        assert!(blocker_answers_in_input(&unknown_verdict).is_err());
        assert!(blocker_answer_for(&unknown_verdict, "draft").is_err());

        let no_node = serde_json::json!({
            CONTINUATION_BLOCKER_KEY: [{ "verdict": "retry" }]
        });
        assert!(blocker_answers_in_input(&no_node).is_err());
    }

    /// A cancel abandons the work, so neither the writer nor the reader will
    /// carry one into a run.
    #[test]
    fn a_cancelled_answer_is_refused_at_both_ends() {
        use crate::ports::blockers::BlockerVerdict;
        assert!(
            blocker_continuation_input(
                serde_json::json!({ "request": "x" }),
                "draft",
                &resolution(BlockerVerdict::Cancel, ""),
            )
            .is_err(),
            "a cancel builds no continuation input"
        );
        let smuggled = serde_json::json!({
            CONTINUATION_BLOCKER_KEY: [{ "node": "draft", "verdict": "cancel" }]
        });
        assert!(
            blocker_answer_for(&smuggled, "draft").is_err(),
            "a node reached carrying a cancel must stop, not carry on"
        );
    }

    /// The key describes what was already decided, not what is being decided,
    /// so it must not split one gate into two cards on a continuation.
    #[test]
    fn an_answered_blocker_is_not_part_of_a_gates_identity() {
        use crate::ports::blockers::BlockerVerdict;
        let input = serde_json::json!({ "request": "x" });
        let paused = gate_effect("digest", "gate", &input, "run-1", &[], &[], None);
        let resumed_input =
            blocker_continuation_input(input, "draft", &resolution(BlockerVerdict::Retry, ""))
                .expect("continues");
        let resumed = gate_effect("digest", "gate", &resumed_input, "run-2", &[], &[], None);
        assert!(
            is_same_gate(&paused, &resumed),
            "one gate, one decision, however many blockers the lineage answered"
        );
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
            &[],
            None,
        );
        // The same gate, re-reached by the continuation the card started: same
        // input plus the approval… minus the approval, which the gate node
        // consumed. What differs is the ledger key alone.
        let mut continuation = single_continuation_input(&paused).expect("continues");
        continuation
            .as_object_mut()
            .expect("object")
            .remove("approvals");
        let re_reached = gate_effect("digest", "gate", &continuation, "run-2", &[], &[], None);

        assert!(
            is_same_gate(&paused, &re_reached),
            "the ledger is not part of the decision:\n{:?}\n{:?}",
            paused.payload,
            re_reached.payload
        );
    }

    // --- issue #596: the pre-publish content preview -------------------------

    fn edge(from: &str, to: &str) -> crate::company::WorkflowEdgeDef {
        crate::company::WorkflowEdgeDef {
            from: from.to_string(),
            to: to.to_string(),
            label: None,
        }
    }

    /// The parked gate carries the verbatim output of the nodes feeding it — the
    /// content awaiting sign-off — keyed by upstream node id.
    #[test]
    fn a_parked_gate_carries_its_upstream_content() {
        let output = serde_json::json!({
            "nodes": {
                "writer": { "items": [{ "text": "the draft tweet" }] },
                "unrelated": { "items": ["not upstream of the gate"] },
            }
        });
        let edges = [edge("start", "writer"), edge("writer", "publish")];

        let mut effect = gate_effect("wf", "publish", &Value::Null, "run-1", &[], &[], None);
        attach_upstream_content(&mut effect, &output, &edges, "publish");

        let content = &effect.payload[PAYLOAD_CONTENT];
        assert_eq!(
            content["writer"]["items"][0]["text"], "the draft tweet",
            "the gate's upstream node output must ride the card: {content}"
        );
        assert!(
            content.get("unrelated").is_none(),
            "a node that does not feed the gate must not be previewed: {content}"
        );
    }

    /// A gate whose upstream produced nothing (or a graph with no such edge) gets
    /// an empty preview rather than a missing key — the console renders "no
    /// content".
    #[test]
    fn a_gate_with_no_upstream_output_gets_an_empty_preview() {
        let output = serde_json::json!({ "nodes": {} });
        let edges = [edge("writer", "publish")];
        let mut effect = gate_effect("wf", "publish", &Value::Null, "run-1", &[], &[], None);
        attach_upstream_content(&mut effect, &output, &edges, "publish");
        assert_eq!(effect.payload[PAYLOAD_CONTENT], serde_json::json!({}));
    }

    /// The content preview is NOT part of the gate's decision identity: two parks
    /// that differ only in the upstream content their nodes produced are still one
    /// decision on one gate, and must dedupe to a single card. Without this a
    /// workflow whose upstream text changes each run would stack a fresh card
    /// every time — the rubber-stamp failure #395 closed, re-opened by #596.
    #[test]
    fn two_parks_differing_only_in_content_still_dedupe() {
        let edges = [edge("writer", "publish")];
        let input = serde_json::json!({ "request": "x" });

        let mut a = gate_effect("wf", "publish", &input, "run-1", &[], &[], None);
        attach_upstream_content(
            &mut a,
            &serde_json::json!({ "nodes": { "writer": { "items": ["draft one"] } } }),
            &edges,
            "publish",
        );

        let mut b = gate_effect("wf", "publish", &input, "run-2", &[], &[], None);
        attach_upstream_content(
            &mut b,
            &serde_json::json!({ "nodes": { "writer": { "items": ["a totally different draft"] } } }),
            &edges,
            "publish",
        );

        assert_ne!(
            a.payload[PAYLOAD_CONTENT], b.payload[PAYLOAD_CONTENT],
            "the two cards really do carry different content"
        );
        assert!(
            is_same_gate(&a, &b),
            "…but they are one decision on one gate and must dedupe to a single card"
        );
    }

    /// The card says, in plain words, what approving actually does. On a
    /// build with checkpoint-backed resume wired, that is conditional —
    /// normally no re-run, a re-run only if the graph changed underneath the
    /// pending approval.
    #[cfg(feature = "openhuman")]
    #[test]
    fn the_card_states_what_approving_costs() {
        let e = effect("digest", "gate", Value::Null);
        let note = e.payload[PAYLOAD_NOTE].as_str().expect("a note");
        assert!(note.contains("resumes"), "{note}");
        assert!(note.contains("graph changed"), "{note}");
        assert!(note.contains("tokens"), "{note}");
        assert!(note.contains("not be sent"), "{note}");
    }

    /// The build with no checkpoint machinery wired at all: every approval
    /// really is the full re-run, so the operator is told exactly that, with
    /// no conditional hedging the runtime cannot back up.
    #[cfg(not(feature = "openhuman"))]
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

    const FINGERPRINT_V1: &str = r#"
id = "editable"
name = "Editable"
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

    /// Same graph, one node renamed — the shape of an in-place edit an author
    /// makes to a workflow while one of its runs sits parked on an approval.
    const FINGERPRINT_V2: &str = r#"
id = "editable"
name = "Editable"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "gate"
kind = "output"
name = "Gate — renamed"
requires_approval = true
[[edge]]
from = "start"
to = "gate"
"#;

    /// A contested id that no longer holds the graph a run parked against is
    /// refused, whichever side won it.
    ///
    /// A company graph and a global can hold one id. Deleting the company's
    /// copy surfaces the global; authoring one buries it. Both directions end
    /// with a graph the operator never answered for.
    #[test]
    fn a_contested_id_is_refused_in_both_directions() {
        let contested = &crate::globals::workflows()[0].id;
        let parked = crate::company::parse_workflow(FINGERPRINT_V1).expect("parses");
        let other = crate::company::parse_workflow(FINGERPRINT_V2).expect("parses");

        let mut surfaced_global = other.clone();
        surfaced_global.id = contested.clone();
        surfaced_global.global = true;
        assert!(
            a_contested_id_no_longer_holds_the_parked_graph(
                Some(&parked.content_fingerprint()),
                &surfaced_global,
                &[],
            ),
            "a global surfacing under a deleted company graph's id must not run in its place"
        );

        let mut authored_company = other.clone();
        authored_company.id = contested.clone();
        authored_company.global = false;
        assert!(
            a_contested_id_no_longer_holds_the_parked_graph(
                Some(&parked.content_fingerprint()),
                &authored_company,
                &[],
            ),
            "a company graph authored over a parked global must not run in its place either"
        );

        let mut unchanged = surfaced_global.clone();
        unchanged.global = true;
        assert!(
            !a_contested_id_no_longer_holds_the_parked_graph(
                Some(&unchanged.content_fingerprint()),
                &unchanged,
                &[],
            ),
            "the graph a run genuinely parked against still resumes"
        );
        assert!(
            !a_contested_id_no_longer_holds_the_parked_graph(None, &surfaced_global, &[]),
            "a run parked before fingerprints were stashed keeps replaying as it did"
        );
        assert!(
            !a_contested_id_no_longer_holds_the_parked_graph(
                Some(&parked.content_fingerprint()),
                &other,
                &[],
            ),
            "an id no global answers to is the operator's own edit — unchanged behaviour"
        );
        assert!(
            !a_contested_id_no_longer_holds_the_parked_graph(
                Some(&parked.content_fingerprint()),
                &surfaced_global,
                &[format!("workflow:{contested}")],
            ),
            "a global the company disabled cannot contest the id at all"
        );
    }

    /// A card parked before this check existed carries no
    /// `PAYLOAD_WORKFLOW_FINGERPRINT` at all — must not be treated as a
    /// mismatch, or every pre-existing parked card would spuriously fall back
    /// to a trigger re-run the moment this check shipped.
    #[test]
    fn a_card_with_no_stashed_fingerprint_is_treated_as_unchanged() {
        let workflow = crate::company::parse_workflow(FINGERPRINT_V1).expect("parses");
        let e = effect("editable", "gate", Value::Null);
        assert!(
            !e.payload
                .as_object()
                .unwrap()
                .contains_key(PAYLOAD_WORKFLOW_FINGERPRINT)
        );
        assert!(graph_unchanged_since_park(&e, &workflow));
    }

    /// The headline: a graph edited while its run sat parked no longer matches
    /// the fingerprint that run's card stashed at park time (PR #1991 review,
    /// `3903797615`).
    #[test]
    fn an_edited_graph_no_longer_matches_its_parked_fingerprint() {
        let parked_against = crate::company::parse_workflow(FINGERPRINT_V1).expect("parses");
        let edited = crate::company::parse_workflow(FINGERPRINT_V2).expect("parses");
        assert_ne!(
            parked_against.content_fingerprint(),
            edited.content_fingerprint(),
            "the two graphs differ, so their fingerprints must too, or this whole check is inert"
        );

        let mut e = effect("editable", "gate", Value::Null);
        if let Value::Object(ref mut payload) = e.payload {
            payload.insert(
                PAYLOAD_WORKFLOW_FINGERPRINT.to_string(),
                json!(parked_against.content_fingerprint()),
            );
        }

        assert!(
            !graph_unchanged_since_park(&e, &edited),
            "an edit made while the approval was pending must be detected"
        );
        assert!(
            graph_unchanged_since_park(&e, &parked_against),
            "the unedited graph must still read as unchanged against its own stashed fingerprint"
        );
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
    use crate::runtime::journal::{ApprovalConversation, TaskLink};

    /// What the resume actually asked for.
    #[derive(Clone, Debug)]
    struct StartedRun {
        workflow_id: String,
        input: Value,
        run_id: String,
        started_by: crate::ports::types::StartedBy,
        /// Which way the run entered the engine — `NodeRestart` (a checkpoint
        /// resume) or `ReRunFromTrigger` — so a test can pin which one a
        /// fingerprint check chose without inspecting the engine's own
        /// behaviour. Read only by the `openhuman`-gated checkpoint-fingerprint
        /// tests below — `#[cfg]`'d rather than left unconditional, or the
        /// default lane (which never reads it) fails `-D dead_code`.
        #[cfg(feature = "openhuman")]
        resume_semantic: Option<crate::ports::ResumeSemantic>,
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
                    started_by: ctx.started_by.clone(),
                    #[cfg(feature = "openhuman")]
                    resume_semantic: ctx.resume_semantic,
                });
            Ok(WorkflowRun {
                output: json!({ "ok": true }),
                pending_approvals: Vec::new(),
                deliveries: Vec::new(),
                cancelled: false,
                nodes: Vec::new(),
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: Vec::new(),
                approvals: Vec::new(),
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
        let effect = gate_effect(
            "gated",
            "gate",
            &input,
            "run-that-paused",
            deliveries,
            &[],
            None,
        );
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
                ApprovalConversation::default(),
                None,
            )
            .await
            .expect("journals");
        id
    }

    /// [`park_gate`], stamping the card with `started_by` the way
    /// `park_pending_gates` does in production (issue #1862 prerequisite) —
    /// for pinning that a continuation carries it forward.
    async fn park_gate_started_by(
        rt: &Arc<crate::company::runtime::CompanyRuntime>,
        input: Value,
        started_by: &crate::ports::types::StartedBy,
    ) -> ApprovalId {
        let mut effect = gate_effect("gated", "gate", &input, "run-that-paused", &[], &[], None);
        if let Value::Object(ref mut payload) = effect.payload {
            payload.insert(PAYLOAD_STARTED_BY.to_string(), json!(started_by));
        }
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
                ApprovalConversation::default(),
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

    /// Issue #1862 prerequisite (a distinct gap from the trigger-site one
    /// #1861 owns): approving a gate parked by an agent-triggered run must
    /// carry that attribution into the continuation, not reset it to
    /// `Operator`.
    ///
    /// `WorkflowSpawn::spawn`'s `scheduled` is always `false` for a resume
    /// (issue #542), which on its own stamps every continuation
    /// `StartedBy::Operator` via `WorkflowRunContext::new`'s coarse default —
    /// regardless of who or what actually started the run that paused. Before
    /// the fix this assertion reads `Operator` even though the parked card
    /// says `Agent("ceo")`.
    #[tokio::test]
    async fn a_continuation_run_carries_the_paused_runs_attribution() {
        let home = seed_home();
        let (rt, runner) = runtime(home.path(), true).await;
        let id = park_gate_started_by(
            &rt,
            json!({ "request": "quarterly numbers" }),
            &crate::ports::types::StartedBy::Agent("ceo".into()),
        )
        .await;

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .expect("resolves");

        let started = wait_for_runs(&runner, 1).await;
        assert_eq!(started.len(), 1);
        assert_eq!(
            started[0].started_by,
            crate::ports::types::StartedBy::Agent("ceo".into()),
            "the continuation must credit the same agent the paused run did: {:?}",
            started[0].started_by
        );
    }

    /// The other half of the same gap: a card parked before this field
    /// existed carries no `PAYLOAD_STARTED_BY` at all, and must not fail the
    /// resume — it degrades to the same `Operator` default the pre-fix
    /// behaviour always produced.
    #[tokio::test]
    async fn a_pre_fix_gate_with_no_started_by_resumes_as_operator() {
        let home = seed_home();
        let (rt, runner) = runtime(home.path(), true).await;
        // `park_gate` never stamps `PAYLOAD_STARTED_BY` — a stand-in for a card
        // parked before this field existed.
        let id = park_gate(&rt, json!({ "request": "legacy" })).await;

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .expect("resolves");

        let started = wait_for_runs(&runner, 1).await;
        assert_eq!(started.len(), 1);
        assert_eq!(
            started[0].started_by,
            crate::ports::types::StartedBy::Operator,
            "a card with no started_by payload must degrade to the old default, not error: {:?}",
            started[0].started_by
        );
    }

    /// The gate path resolves a global too.
    ///
    /// The blocked-node twin is the reachable half today — no global declares
    /// an approval gate — so this pins the other arm against the day one does,
    /// and against a reader restoring the company-only loader on either.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn an_approved_gate_on_a_global_graph_continues() {
        let global = crate::globals::workflows()
            .first()
            .expect("the baseline ships at least one workflow");
        let home = seed_home();
        assert!(
            !home
                .path()
                .join("workflows")
                .join(format!("{}.toml", global.id))
                .exists(),
            "the fixture must not carry a company copy of {}",
            global.id
        );
        let (rt, runner) = runtime(home.path(), true).await;

        let effect = gate_effect(
            &global.id,
            "gather",
            &json!({ "request": "the week" }),
            "run-that-paused",
            &[],
            &[],
            None,
        );
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
                ApprovalConversation::default(),
                None,
            )
            .await
            .expect("journals");

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .expect("resolves");

        let started = wait_for_runs(&runner, 1).await;
        assert_eq!(
            started
                .iter()
                .map(|run| run.workflow_id.as_str())
                .collect::<Vec<_>>(),
            vec![global.id.as_str()],
            "approving a gate on a global must start its continuation"
        );
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

    /// A workflow run whose whole approval batch is refused starts no
    /// continuation — `resume_run`'s documented terminal case — and, since PR
    /// #1991's review (`3903797619`), must also stop leaving that lineage's
    /// checkpoint on disk forever: no other path ever comes back for a wholly
    /// denied run's thread id.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn an_all_denied_batch_prunes_its_checkpoint_lineage() {
        use tinyflows::graph::Checkpointer;

        let home = seed_home();
        let mut rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest())
            .with_seed_dir(home.path().to_path_buf())
            .build()
            .await
            .expect("runtime builds");
        let checkpoints = Arc::new(
            crate::workflows::checkpoint_store::WorkflowCheckpointStore::new(
                home.path().join("checkpoints"),
            ),
        );
        checkpoints
            .put(tinyflows::graph::Checkpoint {
                thread_id: "run-that-paused".to_string(),
                checkpoint_id: "c1".to_string(),
                run_id: Some("run-that-paused".to_string()),
                parent_checkpoint_id: None,
                namespace: Vec::new(),
                state: json!({}),
                next_nodes: vec![tinyflows::graph::ids::NodeId::new("gate")],
                completed_tasks: Vec::new(),
                pending_writes: Vec::new(),
                interrupts: Vec::new(),
                pending_activations: None,
                barrier_arrivals: Vec::new(),
                metadata: Value::Null,
            })
            .await
            .expect("seed checkpoint");
        rt.set_workflow_checkpoints(checkpoints.clone());
        let rt = Arc::new(rt);

        let turn = workflow_turn_key("run-that-paused");
        let mut effect = gate_effect(
            "gated",
            "gate",
            &json!({ "request": "x" }),
            "run-that-paused",
            &[],
            &[],
            None,
        );
        if let Value::Object(ref mut payload) = effect.payload {
            payload.insert(PAYLOAD_THREAD_ID.to_string(), json!("run-that-paused"));
        }
        let id = ApprovalId::new("gate-1");
        rt.workflow_gates().arm(&turn, &id, &effect);
        rt.workflow_gates().decide(&turn, &id, Verdict::Deny);

        resume_run(&rt, &turn)
            .await
            .expect("an all-denied batch does not error");

        let remaining = checkpoints
            .get_thread("run-that-paused")
            .await
            .expect("checkpoint read");
        assert!(
            remaining.is_empty(),
            "a wholly refused batch starts no continuation, so its checkpoint lineage must be \
             pruned: {remaining:?}"
        );
    }

    /// A gate batch's continuation admission can fail terminally too, not
    /// just resolve to no run at all. Here the batch has real approvals and a
    /// live checkpoint, but the run supervisor is already at its ceiling when
    /// `resume_run` tries to spawn the continuation — `spawn_continuation`'s
    /// `begin(...)?` refuses with `WorkflowRunLimit`, and the release above
    /// already took the batch out of `workflow_gates()` for good, so nothing
    /// will ever retry this lineage. Its checkpoint must not outlive that
    /// refusal on disk.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_gate_batch_prunes_its_checkpoint_when_admission_hits_the_run_ceiling() {
        use tinyflows::graph::Checkpointer;

        let home = seed_home();
        let mut rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest())
            .with_seed_dir(home.path().to_path_buf())
            .build()
            .await
            .expect("runtime builds");
        let runner = Arc::new(RecordingRunner::default());
        rt.set_workflow_runner(runner.clone());
        let checkpoints = Arc::new(
            crate::workflows::checkpoint_store::WorkflowCheckpointStore::new(
                home.path().join("checkpoints"),
            ),
        );
        checkpoints
            .put(tinyflows::graph::Checkpoint {
                thread_id: "run-that-paused".to_string(),
                checkpoint_id: "c1".to_string(),
                run_id: Some("run-that-paused".to_string()),
                parent_checkpoint_id: None,
                namespace: Vec::new(),
                state: json!({}),
                next_nodes: vec![tinyflows::graph::ids::NodeId::new("gate")],
                completed_tasks: Vec::new(),
                pending_writes: Vec::new(),
                interrupts: Vec::new(),
                pending_activations: None,
                barrier_arrivals: Vec::new(),
                metadata: Value::Null,
            })
            .await
            .expect("seed checkpoint");
        rt.set_workflow_checkpoints(checkpoints.clone());
        rt.set_run_supervisor(crate::runtime::RunSupervisor::with_limit(1));
        let rt = Arc::new(rt);

        // Occupy the run supervisor's only slot so `spawn_continuation`'s own
        // `begin(...)?` refuses once this test releases the gate batch below.
        let (_ctx, _guard) = rt
            .run_supervisor()
            .begin("someone-elses-run", false)
            .expect("the ceiling has room for the first run");

        let turn = workflow_turn_key("run-that-paused");
        let mut effect = gate_effect(
            "gated",
            "gate",
            &json!({ "request": "x" }),
            "run-that-paused",
            &[],
            &[],
            None,
        );
        if let Value::Object(ref mut payload) = effect.payload {
            payload.insert(PAYLOAD_THREAD_ID.to_string(), json!("run-that-paused"));
        }
        let id = ApprovalId::new("gate-1");
        rt.workflow_gates().arm(&turn, &id, &effect);
        rt.workflow_gates().decide(&turn, &id, Verdict::Approve);

        let err = resume_run(&rt, &turn)
            .await
            .expect_err("the run supervisor is already at its ceiling");
        assert!(
            matches!(err, OpenCompanyError::WorkflowRunLimit { .. }),
            "expected the ceiling refusal to surface rather than something else: {err}"
        );
        assert!(runner.started().is_empty(), "admission never happened");

        let remaining = checkpoints
            .get_thread("run-that-paused")
            .await
            .expect("checkpoint read");
        assert!(
            remaining.is_empty(),
            "a batch whose continuation could not be admitted is just as terminal for this \
             lineage as an all-denied batch, and its checkpoint must be pruned the same way: \
             {remaining:?}"
        );
    }

    /// Issue #1991 review (`3904304781`): `spawn_continuation`'s fallback to a
    /// trigger re-run — reached here because
    /// `graph_unchanged_since_park` just rejected a stale checkpoint — used to
    /// leave `checkpoint_thread_id`'s lineage on disk forever: nothing else
    /// ever comes back for it once this run re-dispatches on the trigger
    /// input instead. Same leak class `an_all_denied_batch_prunes_its_checkpoint_lineage`
    /// already covers for the wholly-refused exit; this is the fingerprint-
    /// rejection exit.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_gate_refuses_a_stale_checkpoint_after_the_graph_is_edited_and_prunes_it() {
        use tinyflows::graph::Checkpointer;

        let home = seed_home();
        let parked_fingerprint = crate::company::parse_workflow(GATED_TOML)
            .expect("parses")
            .content_fingerprint();
        let mut rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest())
            .with_seed_dir(home.path().to_path_buf())
            .build()
            .await
            .expect("runtime builds");
        let runner = Arc::new(RecordingRunner::default());
        rt.set_workflow_runner(runner.clone());
        let checkpoints = Arc::new(
            crate::workflows::checkpoint_store::WorkflowCheckpointStore::new(
                home.path().join("checkpoints"),
            ),
        );
        checkpoints
            .put(tinyflows::graph::Checkpoint {
                thread_id: "run-that-paused".to_string(),
                checkpoint_id: "c1".to_string(),
                run_id: Some("run-that-paused".to_string()),
                parent_checkpoint_id: None,
                namespace: Vec::new(),
                state: json!({}),
                next_nodes: vec![tinyflows::graph::ids::NodeId::new("gate")],
                completed_tasks: Vec::new(),
                pending_writes: Vec::new(),
                interrupts: Vec::new(),
                pending_activations: None,
                barrier_arrivals: Vec::new(),
                metadata: Value::Null,
            })
            .await
            .expect("seed checkpoint");
        rt.set_workflow_checkpoints(checkpoints.clone());
        let rt = Arc::new(rt);

        // The edit: an author renames the gate node while this approval sits
        // pending.
        std::fs::write(
            home.path().join("workflows").join("gated.toml"),
            GATED_TOML.replace("name = \"Gate\"", "name = \"Gate — renamed\""),
        )
        .expect("edit graph on disk");

        let turn = workflow_turn_key("run-that-paused");
        let mut effect = gate_effect(
            "gated",
            "gate",
            &json!({ "request": "x" }),
            "run-that-paused",
            &[],
            &[],
            None,
        );
        if let Value::Object(ref mut payload) = effect.payload {
            payload.insert(PAYLOAD_THREAD_ID.to_string(), json!("run-that-paused"));
            payload.insert(
                PAYLOAD_WORKFLOW_FINGERPRINT.to_string(),
                json!(parked_fingerprint),
            );
        }
        let id = ApprovalId::new("gate-1");
        rt.workflow_gates().arm(&turn, &id, &effect);
        rt.workflow_gates().decide(&turn, &id, Verdict::Approve);

        resume_run(&rt, &turn)
            .await
            .expect("an approved batch still starts a continuation, just not a checkpoint resume");

        let started = wait_for_runs(&runner, 1).await;
        assert_eq!(started.len(), 1, "exactly one continuation run must start");
        assert_eq!(
            started[0].resume_semantic,
            Some(crate::ports::ResumeSemantic::ReRunFromTrigger),
            "the graph changed since this gate parked, so the stale checkpoint must be refused"
        );

        let remaining = checkpoints
            .get_thread("run-that-paused")
            .await
            .expect("checkpoint read");
        assert!(
            remaining.is_empty(),
            "the refused lineage is unreachable from here on, so it must be pruned rather than \
             leaked: {remaining:?}"
        );
    }

    /// A blocked agent node's edited-graph twin of the gate-path proof above
    /// (`an_edited_graph_no_longer_matches_its_parked_fingerprint`) — this is
    /// the finding both `3904397452` (coderabbit) and `3904304754` (codex)
    /// raised on `spawn_blocked_node_continuation`'s own `node_restart` check,
    /// which used to consult checkpoint availability alone.
    ///
    /// Pre-fix, `node_restart` was `checkpoint_resume_available(..)` with no
    /// fingerprint term at all, so this test's checkpoint (seeded and
    /// available) made it pick `NodeRestart` regardless of the edit below —
    /// this assertion is what fails against that code.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_blocked_node_refuses_a_stale_checkpoint_after_the_graph_is_edited() {
        use tinyflows::graph::Checkpointer;

        let home = seed_home();
        let parked_fingerprint = crate::company::parse_workflow(GATED_TOML)
            .expect("parses")
            .content_fingerprint();
        let mut rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest())
            .with_seed_dir(home.path().to_path_buf())
            .build()
            .await
            .expect("runtime builds");
        let runner = Arc::new(RecordingRunner::default());
        rt.set_workflow_runner(runner.clone());
        let checkpoints = Arc::new(
            crate::workflows::checkpoint_store::WorkflowCheckpointStore::new(
                home.path().join("checkpoints"),
            ),
        );
        checkpoints
            .put(tinyflows::graph::Checkpoint {
                thread_id: "blocked-thread".to_string(),
                checkpoint_id: "c1".to_string(),
                run_id: Some("blocked-thread".to_string()),
                parent_checkpoint_id: None,
                namespace: Vec::new(),
                state: json!({}),
                next_nodes: vec![tinyflows::graph::ids::NodeId::new("gate")],
                completed_tasks: Vec::new(),
                pending_writes: Vec::new(),
                interrupts: Vec::new(),
                pending_activations: None,
                barrier_arrivals: Vec::new(),
                metadata: Value::Null,
            })
            .await
            .expect("seed checkpoint");
        rt.set_workflow_checkpoints(checkpoints.clone());
        let rt = Arc::new(rt);

        // The edit: an author renames the gate node while this block sits
        // pending — the same shape `FINGERPRINT_V2` gives the gate-path test,
        // applied to the graph on disk `spawn_blocked_node_continuation`
        // re-loads.
        std::fs::write(
            home.path().join("workflows").join("gated.toml"),
            GATED_TOML.replace("name = \"Gate\"", "name = \"Gate — renamed\""),
        )
        .expect("edit graph on disk");

        spawn_blocked_node_continuation(
            &rt,
            "blocked-turn",
            "gated",
            json!({ "request": "x" }),
            crate::ports::types::StartedBy::Operator,
            Some("blocked-thread".to_string()),
            Some(parked_fingerprint),
        )
        .await
        .expect("a blocked-node continuation still starts, just not as a checkpoint resume");

        let started = wait_for_runs(&runner, 1).await;
        assert_eq!(started.len(), 1, "exactly one continuation run must start");
        assert_eq!(
            started[0].resume_semantic,
            Some(crate::ports::ResumeSemantic::ReRunFromTrigger),
            "the graph changed since this node parked, so the stale checkpoint must be refused \
             in favour of a trigger re-run, not resumed into the edited graph"
        );

        let remaining = checkpoints
            .get_thread("blocked-thread")
            .await
            .expect("checkpoint read");
        assert!(
            remaining.is_empty(),
            "the refused lineage is unreachable from here on, so it must be pruned rather than \
             leaked: {remaining:?}"
        );
    }

    /// A blocked node on a global graph resumes.
    ///
    /// A global lives only in the static baseline, so a resume that reloads
    /// the graph through the company's own two sources cannot find it: the
    /// answer is banked, the continuation never starts, and the run ends
    /// stranded carrying a message that says the graph no longer exists.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_blocked_node_on_a_global_graph_resumes() {
        let global = crate::globals::workflows()
            .first()
            .expect("the baseline ships at least one workflow");

        let home = seed_home();
        assert!(
            !home
                .path()
                .join("workflows")
                .join(format!("{}.toml", global.id))
                .exists(),
            "the fixture must not carry a company copy of {}, or the union loader would find \
             it and the global layer would go untested",
            global.id
        );

        let mut rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest())
            .with_seed_dir(home.path().to_path_buf())
            .build()
            .await
            .expect("runtime builds");
        let runner = Arc::new(RecordingRunner::default());
        rt.set_workflow_runner(runner.clone());
        let rt = Arc::new(rt);

        spawn_blocked_node_continuation(
            &rt,
            "blocked-turn",
            &global.id,
            json!({ "request": "x" }),
            crate::ports::types::StartedBy::Operator,
            None,
            None,
        )
        .await
        .expect("a blocked node on a global graph continues");

        let started = wait_for_runs(&runner, 1).await;
        assert_eq!(
            started
                .iter()
                .map(|run| run.workflow_id.as_str())
                .collect::<Vec<_>>(),
            vec![global.id.as_str()],
            "answering the question must start the continuation, not strand the run"
        );
    }

    /// The positive control beside the test above: an unedited graph must
    /// still resume its checkpoint — the fingerprint check must not become a
    /// blanket refusal.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_blocked_node_resumes_its_checkpoint_when_the_graph_is_unchanged() {
        use tinyflows::graph::Checkpointer;

        let home = seed_home();
        let parked_fingerprint = crate::company::parse_workflow(GATED_TOML)
            .expect("parses")
            .content_fingerprint();
        let mut rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest())
            .with_seed_dir(home.path().to_path_buf())
            .build()
            .await
            .expect("runtime builds");
        let runner = Arc::new(RecordingRunner::default());
        rt.set_workflow_runner(runner.clone());
        let checkpoints = Arc::new(
            crate::workflows::checkpoint_store::WorkflowCheckpointStore::new(
                home.path().join("checkpoints"),
            ),
        );
        checkpoints
            .put(tinyflows::graph::Checkpoint {
                thread_id: "blocked-thread".to_string(),
                checkpoint_id: "c1".to_string(),
                run_id: Some("blocked-thread".to_string()),
                parent_checkpoint_id: None,
                namespace: Vec::new(),
                state: json!({}),
                next_nodes: vec![tinyflows::graph::ids::NodeId::new("gate")],
                completed_tasks: Vec::new(),
                pending_writes: Vec::new(),
                interrupts: Vec::new(),
                pending_activations: None,
                barrier_arrivals: Vec::new(),
                metadata: Value::Null,
            })
            .await
            .expect("seed checkpoint");
        rt.set_workflow_checkpoints(checkpoints.clone());
        let rt = Arc::new(rt);

        spawn_blocked_node_continuation(
            &rt,
            "blocked-turn",
            "gated",
            json!({ "request": "x" }),
            crate::ports::types::StartedBy::Operator,
            Some("blocked-thread".to_string()),
            Some(parked_fingerprint),
        )
        .await
        .expect("a blocked-node continuation starts");

        let started = wait_for_runs(&runner, 1).await;
        assert_eq!(started.len(), 1, "exactly one continuation run must start");
        assert_eq!(
            started[0].resume_semantic,
            Some(crate::ports::ResumeSemantic::NodeRestart),
            "an unedited graph must still resume its checkpoint — the fingerprint check must \
             not refuse a lineage that is still valid"
        );

        let remaining = checkpoints
            .get_thread("blocked-thread")
            .await
            .expect("checkpoint read");
        assert!(
            !remaining.is_empty(),
            "a resumed lineage must not be pruned out from under the run using it"
        );
    }
}
