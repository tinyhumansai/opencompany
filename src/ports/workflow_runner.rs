//! The [`WorkflowRunner`] port: execute a company's workflow graph.
//!
//! A company's workflows are data-only
//! [`WorkflowFile`](crate::company::workflow_file::WorkflowFile) graphs. Running
//! one is dependency-inverted behind this port so the kernel and the HTTP layer
//! depend only on the trait: the concrete engine-backed implementation
//! (`crate::workflows::HarnessWorkflowRunner`, which drives the graph on the
//! embedded `tinyflows` engine with agent nodes on the harness pool) is compiled
//! only under `feature = "openhuman"`. The default build compiles this trait and
//! its result type but wires no implementation — a runtime with no runner leaves
//! the run route reporting "not wired", exactly like the other networked seams.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;
use crate::company::WorkflowFile;
use crate::ports::types::{CompanyId, StartedBy, WorkflowNodeStatus};

/// The outcome of running one workflow to completion.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    /// The final run state after the terminal node(s) completed. Its shape is
    /// the engine's `{ "run": …, "nodes": { "<id>": { "items": [ … ] } } }` map.
    pub output: Value,
    /// Node ids the run is waiting on a human for. Empty for a run that
    /// reached its terminal node(s) without stopping for anybody.
    ///
    /// **Two producers, deliberately unioned** (issue #881). The original is
    /// the tinyflows engine's own gate-node list: a `tool_call` node marked
    /// `requires_approval` pauses the engine, lands here, and resumes through
    /// [`workflow_resume`](crate::runtime::workflow_resume). The second is a
    /// node the *host* blocked — an agent node whose turn had a tool call
    /// parked for approval (see [`blocked_nodes`](Self::blocked_nodes)), which
    /// the engine never learns about because the refusal happens inside the
    /// model's tool loop.
    ///
    /// They are unioned because the question this field answers — "which nodes
    /// is this run waiting on me for?" — has the same answer for both, and the
    /// console renders every entry as a node name. What differs is what
    /// approving does: a paused gate resumes the run, a blocked node does not
    /// (see [`blocked_nodes`](Self::blocked_nodes)), which is why the two stay
    /// separable through that field rather than only through this one.
    pub pending_approvals: Vec<String>,
    /// One row per attempt to route a reached `output` node's report to its
    /// configured destination (issue #170), in graph order.
    ///
    /// Empty for a graph whose `output` nodes name no destination — the
    /// pre-#170 shape — which is why it is `#[serde(default)]`: a `WorkflowRun`
    /// deserialized from an older payload still loads.
    ///
    /// A delivery failure is reported here rather than failing the run: the work
    /// the run did is still valid. An output node the run never reached
    /// contributes no row at all, so an absent row means "not reached", never
    /// "silently dropped".
    #[serde(default)]
    pub deliveries: Vec<DeliveryReport>,
    /// Whether an operator stopped this run before it reached its terminal
    /// node(s) (issue #383).
    ///
    /// **A cancelled run is not a failed one.** The graph did nothing wrong and
    /// the host did not go away — a human decided it had seen enough. That
    /// distinction is the point of a separate flag rather than an `error`
    /// string: the console's three terminal wordings ("failed", "interrupted by
    /// a host restart", "stopped by an operator") stay distinguishable, and a
    /// deliberate stop never lands in the failure count.
    ///
    /// `#[serde(default)]` so a `WorkflowRun` deserialized from a pre-#383
    /// payload still loads, as `false`.
    #[serde(default)]
    pub cancelled: bool,
    /// Per-node progress for this run, in the order the nodes finished (issue
    /// #542 — the same three scalars a `WorkflowNodeFinished` journal row
    /// carries, and no more: a node's own output and error text are
    /// deliberately absent, so they cannot ride this into the run response).
    ///
    /// Collected for **every** run, not only a dry one — the runner's progress
    /// observer feeds it on all paths, so an ordinary synchronous run's
    /// response can carry the same per-node timeline the history panel already
    /// shows. It is the *whole* durable record of a dry run, which journals
    /// nothing: the settled response body is all a test run leaves behind.
    ///
    /// `#[serde(default)]` so a `WorkflowRun` deserialized from a payload
    /// written before this field existed still loads, as an empty list.
    #[serde(default)]
    pub nodes: Vec<WorkflowRunNodeRow>,
    /// System notices raised *about* this run that the operator needs and that
    /// no other field can carry (issue #638).
    ///
    /// Today there is exactly one producer: a node whose turn gated more tool
    /// calls than [`MAX_APPROVAL_REQUESTS_PER_TURN`](crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN)
    /// allows, whose excess is discarded. The chat path says that out loud as
    /// its own bubble (#561); a run has no conversation to speak on, so before
    /// this the only trace was a `tracing::warn!` — and a log line is not the
    /// operator learning anything.
    ///
    /// **Deliberately not `error`.** A run that overflowed the cap did not
    /// fail: its nodes ran, its output is valid, and marking it failed would
    /// inflate the failure count and hide a real failure among them. Same
    /// reasoning [`cancelled`](Self::cancelled) is a flag rather than an error
    /// string.
    ///
    /// **And not a `DeliveryReport` row**, which is per-`output`-node and per
    /// delivery attempt: a run with no `output` node produces no rows at all,
    /// and this notice is about the run, not about a delivery.
    ///
    /// `#[serde(default)]` so a payload written before this field existed still
    /// loads, as empty — and empty is the overwhelmingly common case.
    #[serde(default)]
    pub notices: Vec<String>,
    /// One row per board write this run's agent nodes performed (issue #661 /
    /// M5), in the order they were executed.
    ///
    /// A workflow node's turn may open a card and set who owns it — that is the
    /// whole of what a run may do to the board, and it is what makes the shipped
    /// `→ task cards` seed able to produce one. Every other lifecycle move stays
    /// the operator's, refused at the tool boundary; see
    /// [`DrainClaim::Board`](crate::harness::orchestrator::DrainClaim).
    ///
    /// **A card is real once written, so a row is a receipt rather than an
    /// intention.** A `spawned` row means the `TaskStore` took the write; a
    /// `spawnFailed` row means it did not, and the run still succeeded — a board
    /// write that failed must not discard a completed turn's work, so the
    /// failure is reported here instead of failing the node.
    ///
    /// Empty for every run whose nodes touched no card, which is nearly all of
    /// them — hence `#[serde(default)]`, which also loads a `WorkflowRun`
    /// deserialized from a payload written before this field existed.
    #[serde(default)]
    pub board: Vec<WorkflowRunBoardRow>,
    /// One row per node this run **blocked** on a human (issue #881), in the
    /// order the nodes reported.
    ///
    /// A node blocks when a tool call inside its agent turn was parked for
    /// operator approval. Before this, that node returned the model's apology
    /// as its output, reported `ok`, and the graph carried the apology into the
    /// next node's input — so a run that delivered nothing finished green.
    ///
    /// **Not a failure, and not a pause either.** The branch stops (see
    /// [`WorkflowNodeStatus::Blocked`](crate::ports::types::WorkflowNodeStatus))
    /// but the run is not parked for auto-resume: an agent node is not
    /// re-enterable, so resuming would run a fresh turn that parks a *new*
    /// approval, forever. Approving lets the operator re-run; it does not
    /// continue this one.
    ///
    /// `#[serde(default)]` so a `WorkflowRun` deserialized from a payload
    /// written before this field existed still loads, as empty — which is every
    /// run that blocked on nobody.
    #[serde(default)]
    pub blocked_nodes: Vec<WorkflowBlockedNode>,
    /// One row per approval this run **parked** (issue #880), in the order the
    /// parks were attempted.
    ///
    /// Three real feature-pipeline runs each reported every node `ok` with an
    /// empty [`pending_approvals`](Self::pending_approvals) and an empty
    /// [`deliveries`](Self::deliveries) while the company's approval queue held
    /// fifteen `publish_artifact` cards those very runs had opened. Both of
    /// those fields were *truthful* — they mean the engine's gate nodes and
    /// `output`-node routing respectively — and neither answers what the run
    /// view is asked. This field does.
    ///
    /// # It is a receipt, which is why it is named for what the run parked
    ///
    /// Not `pending_approvals_opened`, not "still outstanding". A settle-time
    /// snapshot of what is *still* waiting rots into a fresh lie the moment the
    /// operator approves one; a record that this run parked two cards is true
    /// forever. The console's wording follows from the name — "parked N
    /// approvals", never "waiting on N".
    ///
    /// **The failure rows are the ones that matter most.** Before this, a park
    /// that could not be performed — no approvals queue wired, or a store that
    /// refused the write — was recorded only by a `tracing::error!`, which is
    /// the sole trace that a call the operator will never be asked about was
    /// dropped. A failure row is a receipt, never a node failure; the same
    /// stance [`WorkflowRunBoardRow`] takes for a board write that did not land.
    ///
    /// `#[serde(default)]` so a payload written before this field existed still
    /// loads, and `skip_serializing_if` so a run that parked nothing — nearly
    /// all of them — serializes byte-for-byte as it did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<WorkflowRunApprovalRow>,
}

/// One node a workflow run blocked on a human (issue #881).
///
/// **Structural only**, the same discipline [`WorkflowRunBoardRow`] keeps: the
/// node, the tools whose calls were gated, and the ids of the cards that were
/// opened. No model prose, no policy error text — so a blocked node cannot
/// become a channel for a turn's apology into the journal, the run response, or
/// a host log. The console writes its own sentence from these ids.
///
/// One shape rides all three surfaces — the `WorkflowRunFinished` journal
/// event, `GET …/workflows/runs`, and the synchronous run response — hence the
/// camelCase serde here rather than at each HTTP DTO. The [`DeliveryReport`]
/// precedent, for the same reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowBlockedNode {
    /// The node that blocked.
    pub node_id: String,
    /// The tools whose calls the node's turn had gated, deduplicated and in
    /// first-seen order. A tool *name*, never its arguments.
    pub tools: Vec<String>,
    /// The approvals this node's gated calls actually opened.
    ///
    /// Empty when every park failed — which is strictly worse than a parked
    /// one, because then nobody can unblock the node at all. The per-call
    /// receipts on [`WorkflowRun::approvals`] say which of the two happened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approval_ids: Vec<String>,
    /// How many of this node's gated calls could **not** be parked.
    ///
    /// Non-zero is the loud case: the call was refused, the operator will never
    /// be asked about it, and re-running is the only way forward. Skipped when
    /// zero, which is the ordinary case.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unparkable: usize,
    /// How many of this node's [`approval_ids`](Self::approval_ids) the journal
    /// no longer holds (issue #1143).
    ///
    /// **Computed on the read, never journaled.** Every writer leaves this zero
    /// and it is skipped when zero, so a run's durable row is byte-for-byte what
    /// it was; `list_runs` fills it in by asking the journal which of the ids
    /// this node parked are still parked. It has to be derived rather than
    /// stored for the same reason the sibling `WorkflowRun::approvals` receipt
    /// is named for what was parked rather than what is outstanding: a stored
    /// count of "still waiting" is a fresh lie the moment the queue moves.
    ///
    /// Semantically this is [`unparkable`](Self::unparkable) arrived at late.
    /// Both mean the operator will never be asked and re-running is the only way
    /// forward — the difference is only *when* that became true. A park is
    /// unparkable at run time because the gate refused it; it is stranded
    /// afterwards because the question did not survive (the park record is
    /// `Durability::Process`, and the "the agent re-parks on its next attempt"
    /// tolerance that justifies it does not hold for a run that already halted —
    /// see #1145).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stranded: usize,
}

/// `skip_serializing_if` predicate for a count that is almost always zero.
fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// What became of one gated tool call a workflow run tried to park (issue
/// #880), as a closed set.
///
/// Three arms because there are exactly three real outcomes at the drain, and
/// an operator reading a run wants to tell them apart: the card is on the
/// Approvals page, the card could not be written, or the call was dropped
/// before parking was even attempted. Carries no payload by construction, so
/// nothing a model or a store wrote can ride an outcome into a log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowApprovalOutcome {
    /// A decidable card is on the Approvals page. The row's `approvalId` names
    /// it.
    Parked,
    /// The park was attempted and did not land — the store refused the write,
    /// or this runtime has no approvals queue wired at all. **Nobody will ever
    /// be asked about this call.**
    ParkFailed,
    /// The call was dropped before parking: the turn gated more calls than
    /// [`MAX_APPROVAL_REQUESTS_PER_TURN`](crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN)
    /// allows and this one was in the excess. The drain caps and drops in one
    /// step, so which tool it was is not recoverable — hence a row with no
    /// `tool`.
    Discarded,
}

impl WorkflowApprovalOutcome {
    /// Whether this row records a call the operator will **never** be asked
    /// about.
    pub fn unparkable(&self) -> bool {
        matches!(self, Self::ParkFailed | Self::Discarded)
    }
}

/// One gated tool call a workflow run's agent node tried to park (issue #880).
///
/// **Structural only** — the node, the tool's name, the outcome, and the
/// approval id when one was minted. No arguments, no policy reason, no store
/// error text: the same rule [`WorkflowRunBoardRow`] follows, so a row cannot
/// become a channel for a turn's payload into the journal or a host log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunApprovalRow {
    /// The agent node whose turn made the call.
    ///
    /// Absent only where node identity is unavailable — the vendored
    /// `AgentRunner` trait boundary carries no node id of its own, so a graph
    /// authored without one leaves this unset rather than inventing a name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// The tool whose call was gated.
    ///
    /// Absent on [`Discarded`](WorkflowApprovalOutcome::Discarded): the drain
    /// caps and drops the excess in one step, so by the time the count is known
    /// the entries are gone. A row with no tool is the honest shape — "one more
    /// call was dropped" — rather than a name guessed from the survivors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// What became of the park attempt.
    pub outcome: WorkflowApprovalOutcome,
    /// The card the operator can decide, on the
    /// [`Parked`](WorkflowApprovalOutcome::Parked) arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
}

/// How many of `pending_approvals`' **nodes** have no live-parked call left
/// among `approvals` (issue #1865 Codex review).
///
/// This is the synchronous-response twin of
/// [`workflow_verdict::stranded_approvals`](crate::ports::workflow_verdict::stranded_approvals):
/// that one reconciles against the live approvals queue and is deliberately
/// not run on the hot settle path (a guaranteed-zero JOIN microseconds after
/// the park), so this one answers the same per-*node* question — "does this
/// node have a live card left?" — from the structural receipts a run already
/// carries in [`WorkflowRun::approvals`].
///
/// `pending_approvals` and `approvals` are counted in different units —
/// one entry per **node** against one entry per **gated call** — so `count()`
/// over `approvals.filter(unparkable)` is not this number: a node with one
/// parked call and one failed park is not stranded (an operator can still act
/// on it), and a node with two failed parks and zero parked ones is, but a
/// call-level count cannot tell the two apart. Grouping by node first is what
/// keeps this **never greater than `pending_approvals.len()`**, matching
/// [`RunVerdictFacts::stranded_approvals`](crate::ports::workflow_verdict::RunVerdictFacts::stranded_approvals)'s
/// own invariant.
///
/// **Absence of a receipt is not a failed park** (PR #1883 Codex review). A
/// `requires_approval` gate `park_pending_gates` parks is never given an
/// `approvals` row at all — that receipt shape is `park_gated_calls`'s alone,
/// for a call gated inside an agent turn (see the module docs on
/// [`WorkflowRunApprovalRow`] and `workflow_verdict`'s two-shapes note). A
/// node in `pending_approvals` with zero rows here is therefore that ordinary
/// gate shape, structurally silent by design, not a park that failed — so it
/// must NOT count as stranded. Only a node that has at least one row, and
/// none of them `Parked`, is one this function can actually see fail.
pub fn stranded_approvals(
    pending_approvals: &[String],
    approvals: &[WorkflowRunApprovalRow],
) -> usize {
    pending_approvals
        .iter()
        .filter(|node_id| {
            let mut rows = approvals
                .iter()
                .filter(|a| a.node_id.as_deref() == Some(node_id.as_str()))
                .peekable();
            rows.peek().is_some() && rows.all(|a| a.outcome != WorkflowApprovalOutcome::Parked)
        })
        .count()
}

/// One node's structural outcome inside a run (issue #542).
///
/// The port-side twin of the HTTP layer's `WorkflowRunNode` and of a
/// `WorkflowNodeFinished` journal row: id, status, elapsed millis, and nothing
/// that could leak a node's payload. It rides [`WorkflowRun::nodes`] out of the
/// runner so the run response can report a per-node timeline without a second
/// read of the journal — which matters most for a dry run, whose timeline is
/// journaled nowhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunNodeRow {
    /// The node that finished.
    pub node_id: String,
    /// Whether it succeeded or errored.
    pub status: WorkflowNodeStatus,
    /// Wall-clock duration of the node's execution, in milliseconds.
    pub elapsed_ms: u64,
    /// The node's non-fatal data-binding diagnostics (issue #1014): the config
    /// path of every `=`-expression that resolved to `null` during this node's
    /// execution — the engine's own list of the broken wiring behind a bad tool
    /// call (see `tinyflows::expr::NullResolution::location`).
    ///
    /// **Paths only, never a resolved value.** A null resolution has no value by
    /// definition, and only the config *location* rides here — the same
    /// no-payload stance the row's `status`/`elapsed_ms` take, so nothing a
    /// model or upstream node produced can leak onto the run response or the
    /// journal-folded history.
    ///
    /// `#[serde(default)]` + `skip_serializing_if` so a row serialized before
    /// this field existed folds back with an empty list, and a node with no
    /// unresolved wiring serializes byte-for-byte as it did before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

/// What a workflow run's node did to the task board, as a closed set (issue
/// #661 / M5).
///
/// Four arms rather than a `bool` beside a kind, because the two axes are not
/// independent in any way a reader benefits from: an operator looking at a run
/// wants "opened a card" / "could not open a card" / "set an owner" / "could not
/// set an owner", and those are exactly the four sentences a console renders.
///
/// **The failure arms are not run failures.** They record that the store refused
/// a write the node's turn had already been told would happen — the same class of
/// honesty [`DeliveryStatus::Failed`] provides for a report that did not send. A
/// run whose every board write failed still finished its graph and still returns
/// `Ok`.
///
/// Carries no payload by construction, so nothing a model or a transport wrote
/// can ride an action into a log. The row beside it carries the ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowBoardAction {
    /// A card was opened in To-do (`spawn_task`). The row's `taskId` is that
    /// card's id, so a console can link straight to it.
    Spawned,
    /// An existing card's owner was set or cleared (`assign_task`). **No column
    /// moved** — see [`WorkflowRun::board`].
    Assigned,
    /// `spawn_task` did not produce a card: the store refused the write, or this
    /// runtime has no task board wired at all. No `taskId`, because there is no
    /// card to point at.
    SpawnFailed,
    /// `assign_task` did not change the card's owner: the store refused the
    /// write, the card is no longer on the board, or the name did not resolve to
    /// anybody on the roster (issue #205 — an unresolvable owner is deliberately
    /// not written, leaving the previous one in place).
    AssignFailed,
}

impl WorkflowBoardAction {
    /// Whether this row records a write that did **not** land.
    ///
    /// Used to decide whether the row is worth a `tracing::error` beside it: a
    /// board write the node was told would happen and that did not is the one
    /// thing on this path an operator cannot infer from the card itself.
    pub fn failed(&self) -> bool {
        matches!(self, Self::SpawnFailed | Self::AssignFailed)
    }
}

/// One board write a workflow run's agent node performed (issue #661 / M5).
///
/// **Structural only**, and deliberately the same discipline
/// [`WorkflowRunNodeRow`] keeps: the action, the ids involved, and nothing else.
/// No error string, no note text, no instruction the model wrote — so a row
/// cannot become a channel for a node's prose into the journal, the run
/// response, or a host log.
///
/// One shape rides all three surfaces — the `WorkflowRunFinished` journal event,
/// `GET …/workflows/runs`, and the synchronous run response — which is why the
/// serde renaming is camelCase here rather than at each HTTP DTO: the
/// [`DeliveryReport`] precedent, for the same reason. A console reads the same
/// keys wherever it finds a run.
///
/// # `title` is the only field that carries authored text, and it is the card's own
///
/// A spawned card's title is what the operator will read off their board a
/// moment later, so it is not new exposure — the board read already serves it
/// under the same `ScopedCompany` guard. It is present only on the spawn arms,
/// where the delegation named it; an assign row leaves it absent rather than
/// re-transcribing a card the console can already resolve by id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunBoardRow {
    /// What was attempted, and whether it landed.
    pub action: WorkflowBoardAction,
    /// The card the row is about.
    ///
    /// Absent on [`SpawnFailed`](WorkflowBoardAction::SpawnFailed) — no card was
    /// written, so there is no id, and synthesizing one would name a card that
    /// is not on the board. Present on every other arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// The title the node asked for, on the two spawn arms.
    ///
    /// Absent on the assign arms: `assign_task` names a card by id and never
    /// carries a title, so anything here would be a second transcription of a
    /// card the reader can already look up — the drift `plan` avoids by being
    /// projected verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The owner the node asked for, as the node wrote it.
    ///
    /// `None` when the node named nobody (a `spawn_task` with no assignee, which
    /// lands unowned in To-do). Deliberately the **requested** name rather than
    /// the resolved roster id: on an `assignFailed` row the requested name is the
    /// only thing that explains the failure, and on a successful row the two
    /// agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
}

/// What became of one attempt to deliver an `output` node's report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryStatus {
    /// The transport accepted the report.
    Sent,
    /// Parked for operator approval and not sent — the destination needs a
    /// human verdict before anything leaves the process (a cold email
    /// recipient, which a workflow may not cold-open by itself).
    ///
    /// **This row is a snapshot taken at run time, not a live status.** A
    /// workflow run is not persisted, so nothing ever comes back to flip this
    /// row to `Sent` once the operator approves. The approvals queue is the
    /// live source of truth: the parked effect is journal-backed, survives a
    /// restart, and executes on approval through the same path an agent's
    /// `email.send` does. Read this row as "an approval was opened for this",
    /// then look at Approvals for what became of it.
    Pending,
    /// Deliberately not attempted — a policy precondition was unmet (no mailbox
    /// configured, or a cold recipient on a runtime that cannot park), or a run
    /// earlier in this lineage already delivered this node's report and the
    /// continuation must not send it twice (issue #438). Not an error; the
    /// report simply was not owed to that address under the current rules.
    Skipped,
    /// Refused by policy: the company does not grant what the destination needs.
    Denied,
    /// Attempted (or attemptable) and did not work — a transport error, an
    /// unwired channel, or a runtime with no delivery ports at all.
    Failed,
}

/// Why a delivery attempt came out the way it did, as a closed set (issue
/// #248).
///
/// # Why this is an enum and not another string
///
/// [`DeliveryReport::detail`] is free text, and on the transport-failure arms it
/// interpolates the transport's own words. A mail transport's refusal routinely
/// quotes the mailbox it refused (an SMTP `550`/`553` reply commonly reads
/// `<recipient@example.invalid>: Recipient address rejected`), so `detail`
/// carries a recipient address on exactly the paths an operator most wants to
/// read about. That is fine on the operator's own surfaces — the run response
/// and the `WorkflowRunFinished` history a tenant reads back — and not fine on
/// host stdout, which on a hosted deployment is the platform rather than the
/// operator.
///
/// This enum is the half that may be logged. It is *unable* to carry
/// transport-supplied text: it has no `String` payload, so there is no
/// `format!` that produces one. The guarantee is the compiler's rather than a
/// reviewer's, and a new delivery outcome cannot be added without classifying
/// it — the construction sites match on this type exhaustively.
///
/// Its [`Display`](std::fmt::Display) rendering is the prose that reaches the
/// log; its serde name is the stable token for querying a run history.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryReason {
    /// This build wired no delivery ports at all, so nothing could be sent.
    NotWired,
    /// An `owner` report reached one of the company's admin mailboxes — an
    /// active admin from the user store, or a standing admin invite (a manifest
    /// `[users] admins` entry or the deployment's bootstrap admin) not yet signed
    /// in (issue #661 / M8).
    OwnerEmailed,
    /// An `email` report reached the named recipient on an established thread.
    RecipientEmailed,
    /// The mail transport refused the message. **The transport's own reason is
    /// in `detail`, not here** — that is the string that quotes the address.
    MailTransportRefused,
    /// `owner` had no mailbox to send from, so the report went to the operator
    /// channel instead.
    OwnerFellBackNoMailbox,
    /// `owner` had a mailbox but no admin address to send to — no active admin
    /// in the user store and no standing admin invite (manifest `[users] admins`
    /// or the deployment's bootstrap admin) either (issue #661 / M8) — so the
    /// report went to the operator channel instead.
    OwnerFellBackNoAdminAddress,
    /// `owner`'s operator-channel fallback itself failed, so nothing was sent.
    OwnerFallbackFailed,
    /// The company's `[tools].allow` does not grant `email`, so a workflow may
    /// not mail a named address.
    EmailNotGranted,
    /// No mailbox is configured for the company, so there was nothing to send
    /// from.
    NoMailboxConfigured,
    /// The recipient is not an established thread and this runtime has no
    /// approvals queue to park the send on.
    RecipientNotEstablished,
    /// The recipient is not an established thread; the send is parked in
    /// Approvals awaiting a human verdict.
    ParkedForApproval,
    /// The recipient is not an established thread and the send could not be
    /// parked for approval either.
    ParkingUnavailable,
    /// A run earlier in this lineage already delivered this node's report, so
    /// the continuation did not send it again (issue #438).
    ///
    /// Resuming an approved gate is a **re-run** — the engine settles when it
    /// pauses, so continuing means walking the graph again from the trigger.
    /// Every `output` node upstream of the gate is therefore reached a second
    /// time, and without this the operator's approval would mail the same
    /// report to the same person twice. The ledger of what a lineage has
    /// already sent rides the continuation's trigger input; see
    /// [`crate::runtime::workflow_resume`].
    ///
    /// A **parked** report counts as delivered too: the card is durable and
    /// approving it sends, so re-parking would stack a second identical card
    /// and approving both would send twice.
    AlreadyDelivered,
    /// A `channel` report was posted to the wired adapter.
    ChannelPosted,
    /// The destination names a channel this deployment never wired. **Which
    /// channel, and what is wired instead, is in `detail`** — a channel id is
    /// the `channel` arm's target, and targets do not go to host logs.
    ChannelNotWired,
    /// The channel adapter refused the message. As with mail, the adapter's own
    /// reason stays in `detail`.
    ChannelRefused,
    /// The operator feed's collision fallback
    /// ([`OPERATOR_CHANNEL_COLLISION_FALLBACK`](crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK))
    /// is itself shadowed by a second grandfathered desk name, so there is no
    /// address left to journal this report to that would not land it in that
    /// desk's own transcript — see
    /// [`CompanyRecord::operator_feed_channel_fallback_shadowed`](crate::ports::types::CompanyRecord::operator_feed_channel_fallback_shadowed)
    /// (issue #1781 review). Refused rather than delivered, unlike the primary
    /// collision.
    ChannelCollisionShadowed,
    /// The destination kind is not one this runtime knows how to deliver to
    /// (unreachable through `parse_workflow`, which rejects unknown kinds).
    UnknownDestinationKind,
    /// The run reached this `output` node and the node names no destination, so
    /// there was nowhere to send its report (issue #925).
    ///
    /// `destination` is optional on the model because it postdates the node kind
    /// — an `output` node without one is the shape every graph had before
    /// [`WorkflowDestinationDef`](crate::company::WorkflowDestinationDef)
    /// existed, and its report surfaced only in the console's run drawer. That
    /// made "the author routed nothing on purpose" and "the author never
    /// configured a destination" the *same* observation: an empty `deliveries`
    /// list and a run summary reading `Finished — this run routed no reports.`
    ///
    /// This row is what tells them apart. It is deliberately a
    /// [`Skipped`](DeliveryStatus::Skipped) rather than a
    /// [`Failed`](DeliveryStatus::Failed): nothing broke and nothing was
    /// attempted, so this is the same class as
    /// [`AlreadyDelivered`](Self::AlreadyDelivered) — a report that was never
    /// owed to an address, stated with its reason instead of by omission.
    NoDestinationConfigured,
    /// This was a **dry run** (issue #542): the report was routed as far as its
    /// destination but deliberately not dispatched, so an operator can see
    /// *where* a report would have gone without anything actually leaving the
    /// process. Carries no transport text — nothing was attempted — so it is
    /// safe to log.
    DryRun,
    /// No reason was recorded. Only reachable by deserializing a
    /// `WorkflowRunFinished` event written before this field existed.
    #[default]
    Unspecified,
}

impl std::fmt::Display for DeliveryReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Every arm is a literal. Keep it that way: the whole point of the type
        // is that nothing runtime-supplied can reach a host log through it.
        f.write_str(match self {
            Self::NotWired => "report delivery is not wired on this runtime",
            Self::OwnerEmailed => "emailed the company's admin",
            Self::RecipientEmailed => "emailed the named recipient on an established thread",
            Self::MailTransportRefused => "the mail transport refused the message",
            Self::OwnerFellBackNoMailbox => {
                "no mailbox is configured for this company, so the report went to the operator \
                 channel"
            }
            Self::OwnerFellBackNoAdminAddress => {
                "no active admin has an email address, so the report went to the operator channel"
            }
            Self::OwnerFallbackFailed => "the operator channel fallback failed",
            Self::EmailNotGranted => {
                "this company's [tools].allow does not grant `email`, so a workflow may not send \
                 mail to a named address"
            }
            Self::NoMailboxConfigured => "no mailbox is configured for this company",
            Self::RecipientNotEstablished => {
                "the recipient has never written to the company, and this runtime has no approvals \
                 queue to park the send on"
            }
            Self::ParkedForApproval => {
                "the recipient has never written to the company, so the report is waiting in \
                 Approvals"
            }
            Self::ParkingUnavailable => {
                "the recipient has never written to the company, and the report could not be \
                 queued for approval either"
            }
            Self::AlreadyDelivered => {
                "an earlier run in this workflow's approval lineage already delivered this report, \
                 so it was not sent again"
            }
            Self::ChannelPosted => "posted to the channel",
            Self::ChannelNotWired => "the destination channel is not wired on this runtime",
            Self::ChannelRefused => "the channel refused the message",
            Self::ChannelCollisionShadowed => {
                "the operator feed's collision-fallback address is itself shadowed by another \
                 desk's name, so the report was refused rather than journaled to that desk"
            }
            Self::UnknownDestinationKind => {
                "the destination kind is not one this runtime can deliver to"
            }
            Self::NoDestinationConfigured => {
                "this output node has no destination, so there was nowhere to send its report"
            }
            Self::DryRun => {
                "this was a test run, so the report was not sent — its destination is shown so you \
                 can see where it would have gone"
            }
            Self::Unspecified => "no reason was recorded for this delivery",
        })
    }
}

/// One attempt to route a reached `output` node's report somewhere.
///
/// On an on-demand run these rows ride the run response into the console's
/// run-result panel, so an operator can tell a delivered report from an
/// undelivered one without reading a log. A scheduled run's rows are journaled
/// as a `WorkflowRunFinished` event (issue #228) that the tenant's own console
/// reads back, and are summarized on host stdout by the scheduler.
///
/// **The two reason fields are not interchangeable.** See [`DeliveryReason`]:
/// `detail` is for the operator's surfaces and may quote a transport; `reason`
/// is the only one that may reach a host log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryReport {
    /// The `output` node whose report this was.
    pub node: String,
    /// The destination kind as authored (`owner` / `email` / `channel`).
    pub kind: String,
    /// The address or channel actually addressed. For `owner` this is the
    /// server-resolved recipient, not something the graph named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// What became of the attempt.
    pub status: DeliveryStatus,
    /// An operator-readable reason, always populated — including on success, so
    /// a `sent` row still says *how* it was sent (which matters for `owner`,
    /// whose recipient the graph never named).
    ///
    /// **Operator surfaces only — never a host log.** On the transport-failure
    /// arms this interpolates the transport's own text, which routinely quotes
    /// the address it refused. Log [`Self::reason`] instead.
    ///
    /// "Operator surfaces" is a claim about specific readers, so it is worth
    /// naming them. This field reaches: the run response, the company-scoped
    /// SSE projection, and `GET …/workflows/runs` — all three behind the
    /// `ScopedCompany` guard, and all three reading a **per-company**
    /// `events.jsonl`. It reaches neither of the journal's two non-tenant
    /// readers: the inference-sidecar wire-out (`brain::medulla::effects`) and
    /// the orchestrator's insight summary (`harness::orchestrator`, compiled
    /// only under `openhuman`) both fold a finished run to counts, and tests
    /// pin that they do.
    pub detail: String,
    /// The same outcome as a closed set, safe to log by construction.
    ///
    /// `#[serde(default)]` so a `WorkflowRunFinished` event journaled before
    /// this field existed still loads, as [`DeliveryReason::Unspecified`].
    #[serde(default)]
    pub reason: DeliveryReason,
}

/// What the *caller* knows about a run that the runner cannot work out for
/// itself (issue #371).
///
/// # Why the entry point mints the id rather than the runner
///
/// The run id correlates a run's progress events with its outcome, and the
/// outcome is journaled by the **caller** —
/// [`record_run_finished`](crate::runtime::record_run_finished) — on *both*
/// arms. On the error arm the runner returns nothing at all, so a runner-minted
/// id would be lost exactly when it is most needed: a failed run's node events
/// would be orphaned from the `WorkflowRunFinished` that carries the reason it
/// failed, and the console could not say how far the run got before it died.
/// Minting here, above the call, is what makes the two halves share one id on
/// every path.
///
/// This is a **crate-internal port type**, not a wire type: it has no serde
/// impl and no HTTP surface. Adding it to
/// [`WorkflowRunner::run`](WorkflowRunner::run) makes the compiler enumerate
/// every entry point, which is the whole point — an entry point that forgot to
/// mint an id would silently journal an uncorrelatable run.
#[derive(Clone, Debug)]
pub struct WorkflowRunContext {
    /// The correlation id for this run, minted by the entry point.
    pub run_id: String,
    /// Whether a cron schedule started this run rather than an operator. Rides
    /// the run's [`WorkflowRunStarted`](crate::ports::types::CompanyEvent)
    /// event so the console can tell a nobody-was-watching run from a Run-button
    /// one *while it is happening*, not only once it settles.
    pub scheduled: bool,
    /// The stop signal for this run (issue #383).
    ///
    /// A context built by
    /// [`RunSupervisor::begin`](crate::runtime::RunSupervisor::begin) shares this
    /// handle with the supervisor's registry, so
    /// `POST …/workflows/runs/{runId}/cancel` can reach a run that is still
    /// walking its graph. A context built by [`new`](Self::new) carries a handle
    /// nobody else holds — nothing can ever fire it, which is exactly right for
    /// a test or an entry point that opted out of registration.
    pub cancel: RunCancel,
    /// Whether this run is a **dry run** (issue #542): walk the real graph with
    /// real branch selection, but over stubbed effectful capabilities so no
    /// agent inference, no tool/http execution, and no delivery, journaling or
    /// gate-parking actually happen.
    ///
    /// It rides the context rather than being a `run` argument for the same
    /// reason `scheduled` does: the host-side layers that skip effects around
    /// the engine (`WorkflowSpawn` skips `record_run_finished`; the runner skips
    /// the started/finished journal writes) read it here, not from a separate
    /// parameter that would have to be threaded through both the runner and the
    /// spawn task in lockstep.
    ///
    /// [`new`](Self::new) and
    /// [`RunSupervisor::begin`](crate::runtime::RunSupervisor::begin) both
    /// default it `false`; only the run route sets it, and every other entry
    /// point (cron, resume) leaves it off — a scheduled or resumed run is always
    /// for real.
    pub dry_run: bool,
    /// Who or what started this run (issue #1862 prerequisite). Rides the
    /// run's [`WorkflowRunStarted`](crate::ports::types::CompanyEvent) event so
    /// a parked blocker later has a fact — not a guess — to attribute its DM
    /// to.
    ///
    /// [`new`](Self::new) derives it from `scheduled` via
    /// [`StartedBy::from_scheduled`], which is deliberately the coarse
    /// default: every current call through `new`/`begin` names a run
    /// `scheduled: false` unless the cron fired it, even the ones an agent
    /// triggered rather than an operator
    /// ([`run_workflow`](crate::harness::built_in::orchestrator), which calls
    /// [`RunSupervisor::begin`](crate::runtime::RunSupervisor::begin) with
    /// `scheduled: false` and so reads back as [`StartedBy::Operator`] here).
    /// A caller that knows the real triggering agent should use
    /// [`with_started_by`](Self::with_started_by) to override the default —
    /// wiring that override into `run_workflow` itself is left to a follow-up
    /// (issue #1861) precisely so this prerequisite slice does not have to
    /// touch that call site.
    pub started_by: StartedBy,
}

/// A one-way stop signal for one workflow run (issue #383).
///
/// # Why this is not itself the engine's `CancellationToken`
///
/// This type is on the **port**, and the port compiles in the default build,
/// which links no `tinyflows` at all — so it cannot *be* the engine's token.
/// Instead the engine-backed runner **bridges** it: when this signal fires, the
/// runner flips a `tinyflows::engine::CancellationToken` it handed to
/// `run_cancellable_with_observer` (issue #398). That entry point takes a token
/// **and** an observer, so a cancellable run keeps its per-node progress trail
/// (issue #371/#382) rather than trading it away.
///
/// # Semantics
///
/// Firing this stops the run at the next **node boundary**: the engine checks
/// the token before each node, so a node already executing runs to completion
/// and is journaled, then the run winds down carrying a real (partial) outcome
/// with `cancelled` set. "Stopped", not "finished" — but stopped cleanly, not
/// mid-await.
///
/// The one exception is a node **wedged** mid-await on a stalled external call:
/// it never reaches the next boundary, so the runner bounds the wait
/// (`CANCEL_HARD_ABORT_GRACE`) and, past it, falls back to dropping the engine
/// future — the hard abort. That case *does* stop mid-await, the same class of
/// outcome as the host being killed, which the boot sweep already handles. A
/// wedged run therefore stays killable even though the clean path cannot reach it.
///
/// Cheap to [`Clone`] (a shared handle); firing is idempotent and safe from any
/// thread.
#[derive(Clone)]
pub struct RunCancel {
    /// A `watch` rather than a `Notify`: the runner has to be able to *await*
    /// the signal inside a `select!`, and it must not matter whether the cancel
    /// arrives before or after that await is first polled. A watch channel
    /// carries the state, so a cancel that lands first is still observed;
    /// `Notify` only carries an edge. The [`Arc`] keeps the sender alive for as
    /// long as any handle exists, so a receiver can never see the channel close.
    tx: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Default for RunCancel {
    fn default() -> Self {
        Self {
            tx: Arc::new(tokio::sync::watch::channel(false).0),
        }
    }
}

impl std::fmt::Debug for RunCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunCancel")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl RunCancel {
    /// An un-fired signal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fires the signal. Idempotent, and infallible by construction —
    /// `send_replace` does not care whether anyone is listening, so cancelling a
    /// run whose future has already settled is a silent no-op rather than an
    /// error the caller has to decide what to do about.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    /// Whether the signal has fired.
    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves once the signal has fired — **including when it fired before
    /// this was first polled**, which is the whole reason for the watch channel.
    ///
    /// Cancel-safe: it holds no state of its own between polls, so dropping it
    /// (which is what `select!` does to the losing branch) loses nothing.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        loop {
            if *rx.borrow_and_update() {
                return;
            }
            if rx.changed().await.is_err() {
                // Unreachable: `self` holds an `Arc` of the sender, so the
                // channel cannot close while this future exists. Park rather
                // than return, because returning would read as "cancelled" to
                // every caller and stop a healthy run.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Equality over the run's **identity**, not its live state.
///
/// [`RunCancel`] is a shared handle whose value changes under both sides, so
/// including it would make two clones of one context compare unequal the moment
/// one of them was cancelled. The id and the scheduled flag are what callers
/// (and tests) actually mean by "the same run context". `started_by` (issue
/// #1862 prerequisite) is deliberately left out of this comparison too, for
/// the same reason: it is derived attribution riding alongside the identity,
/// not part of it — two contexts for the same run id stay "the same context"
/// to every existing caller of this `==` regardless of who is credited with
/// starting it.
impl PartialEq for WorkflowRunContext {
    fn eq(&self, other: &Self) -> bool {
        self.run_id == other.run_id && self.scheduled == other.scheduled
    }
}

impl Eq for WorkflowRunContext {}

impl WorkflowRunContext {
    /// A context with a freshly minted run id and an unregistered stop signal.
    ///
    /// `scheduled` says whether a cron started the run rather than an operator.
    ///
    /// **The run this builds cannot be cancelled from the console**, because
    /// nothing but this context holds the signal. Entry points that want an
    /// operator to be able to stop the run go through
    /// [`RunSupervisor::begin`](crate::runtime::RunSupervisor::begin) instead,
    /// which mints the same thing and registers the handle. This constructor
    /// stays for tests and for any caller that deliberately runs unregistered.
    pub fn new(scheduled: bool) -> Self {
        Self {
            run_id: crate::ports::ids::generate_id(),
            scheduled,
            cancel: RunCancel::new(),
            // A run built through this constructor is for real unless the caller
            // flips it after the fact — which is exactly what `WorkflowSpawn`
            // does with the dry flag the run route hands it (issue #542).
            dry_run: false,
            // Issue #1862 prerequisite: the coarse default, derived from
            // `scheduled` alone. See the field doc for why callers that know
            // the real triggering agent should override it with
            // `with_started_by` instead of trusting this.
            started_by: StartedBy::from_scheduled(scheduled),
        }
    }

    /// Overrides the [`started_by`](Self::started_by) this context was built
    /// with (issue #1862 prerequisite) — for a caller that knows the real
    /// triggering agent and wants the journal to say so, rather than settling
    /// for [`new`](Self::new)'s `scheduled`-derived default.
    pub fn with_started_by(mut self, started_by: StartedBy) -> Self {
        self.started_by = started_by;
        self
    }
}

/// Runs a company's workflow graph to completion.
///
/// `company` names the tenant whose roster the run's agent nodes execute on;
/// `workflow` is the parsed graph; `input` is the trigger payload (an arbitrary
/// JSON value seeded as the trigger node's item); `ctx` carries the caller's run
/// id and whether a cron started the run.
#[async_trait]
pub trait WorkflowRunner: Send + Sync {
    /// Runs `workflow` for `company` with the trigger `input`, returning the
    /// final state and any nodes left pending approval.
    async fn run(
        &self,
        company: &CompanyId,
        workflow: &WorkflowFile,
        input: Value,
        ctx: &WorkflowRunContext,
    ) -> Result<WorkflowRun>;
}

#[cfg(test)]
mod test {
    use super::*;

    fn row(node: &str, outcome: WorkflowApprovalOutcome) -> WorkflowRunApprovalRow {
        WorkflowRunApprovalRow {
            node_id: Some(node.to_string()),
            tool: Some("send_email".to_string()),
            outcome,
            approval_id: matches!(outcome, WorkflowApprovalOutcome::Parked)
                .then(|| "appr-1".to_string()),
        }
    }

    /// Codex review (#1865): a node with one live parked call and one failed
    /// park is not stranded — an operator can still act on it — even though a
    /// call-level count of unparkable rows would equal `pending_approvals.len()`
    /// (1 node, 1 unparkable call) and wrongly report it as fully stranded.
    #[test]
    fn a_node_with_one_live_card_is_not_stranded_even_with_one_failed_park() {
        let pending = vec!["gate".to_string()];
        let approvals = vec![
            row("gate", WorkflowApprovalOutcome::Parked),
            row("gate", WorkflowApprovalOutcome::ParkFailed),
        ];
        assert_eq!(stranded_approvals(&pending, &approvals), 0);
    }

    /// The complementary case: a node whose every gated call failed to park
    /// has no live card left, so it counts once — not twice, even though it
    /// made two unparkable rows.
    #[test]
    fn a_node_with_every_call_unparkable_counts_once() {
        let pending = vec!["gate".to_string()];
        let approvals = vec![
            row("gate", WorkflowApprovalOutcome::ParkFailed),
            row("gate", WorkflowApprovalOutcome::Discarded),
        ];
        assert_eq!(stranded_approvals(&pending, &approvals), 1);
    }

    /// Never greater than `pending_approvals.len()` — the invariant
    /// `RunVerdictFacts::stranded_approvals` documents. Two nodes, one fully
    /// stranded and one with a live card, must read `1`, not `2` even though
    /// three of the four rows are unparkable.
    #[test]
    fn mixed_nodes_stay_within_pending_approvals_count() {
        let pending = vec!["gate-a".to_string(), "gate-b".to_string()];
        let approvals = vec![
            row("gate-a", WorkflowApprovalOutcome::Parked),
            row("gate-a", WorkflowApprovalOutcome::ParkFailed),
            row("gate-b", WorkflowApprovalOutcome::ParkFailed),
            row("gate-b", WorkflowApprovalOutcome::Discarded),
        ];
        assert_eq!(stranded_approvals(&pending, &approvals), 1);
    }

    /// PR #1883 Codex review: a `requires_approval` gate `park_pending_gates`
    /// parks — the ordinary authored/policy-raised gate shape, not a call
    /// gated inside an agent turn — never gets an `approvals` row at all
    /// (`park_pending_gates` writes straight to the approvals queue and
    /// `WorkflowRun::approvals`, and never touches it). Before the fix this
    /// read as `!approvals.iter().any(node_id && Parked)` — vacuously true
    /// for a node with zero rows — so every ordinary gate reported stranded
    /// on a run that never made a single failed park. A card is live and
    /// waiting; `pending` alone, with no matching row, must count zero.
    #[test]
    fn a_node_with_no_approval_rows_at_all_is_not_stranded() {
        let pending = vec!["gate".to_string()];
        let approvals: Vec<WorkflowRunApprovalRow> = Vec::new();
        assert_eq!(stranded_approvals(&pending, &approvals), 0);
    }

    /// The same shape, mixed with a genuinely gated-and-unparkable node: the
    /// receipt-less gate must still not count, while the node with real
    /// failed-park rows does.
    #[test]
    fn a_receiptless_gate_beside_a_genuinely_stranded_node_counts_only_the_latter() {
        let pending = vec!["gate".to_string(), "agent-node".to_string()];
        let approvals = vec![row("agent-node", WorkflowApprovalOutcome::ParkFailed)];
        assert_eq!(stranded_approvals(&pending, &approvals), 1);
    }

    /// A manual `new(false)` reads back [`StartedBy::Operator`] — the coarse
    /// default every call through `new`/`begin` gets unless overridden (issue
    /// #1862 prerequisite).
    #[test]
    fn new_with_scheduled_false_defaults_started_by_to_operator() {
        let ctx = WorkflowRunContext::new(false);
        assert_eq!(ctx.started_by, StartedBy::Operator);
        assert!(!ctx.scheduled);
    }

    /// A cron-started `new(true)` reads back [`StartedBy::Schedule`] —
    /// unambiguous, since only the scheduler ever sets `scheduled: true`.
    #[test]
    fn new_with_scheduled_true_defaults_started_by_to_schedule() {
        let ctx = WorkflowRunContext::new(true);
        assert_eq!(ctx.started_by, StartedBy::Schedule);
        assert!(ctx.scheduled);
    }

    /// [`WorkflowRunContext::with_started_by`] overrides the `scheduled`-derived
    /// default — the lever a caller that knows the real triggering agent uses
    /// instead of settling for `new`'s coarse reading.
    #[test]
    fn with_started_by_overrides_the_default() {
        let ctx =
            WorkflowRunContext::new(false).with_started_by(StartedBy::Agent("ceo".to_string()));
        assert_eq!(ctx.started_by, StartedBy::Agent("ceo".to_string()));
        // Overriding the sender does not retroactively flip `scheduled` — the
        // two are independent facts about the run.
        assert!(!ctx.scheduled);
    }

    /// `started_by` does not participate in [`WorkflowRunContext`] equality
    /// (see the `impl PartialEq` doc) — two contexts sharing a run id and
    /// `scheduled` flag are "the same context" regardless of who is credited
    /// with starting it.
    #[test]
    fn started_by_is_excluded_from_equality() {
        let a = WorkflowRunContext::new(false).with_started_by(StartedBy::Operator);
        let mut b =
            WorkflowRunContext::new(false).with_started_by(StartedBy::Agent("ceo".to_string()));
        b.run_id = a.run_id.clone();
        assert_eq!(
            a, b,
            "differing started_by must not break the identity comparison"
        );
    }
}
