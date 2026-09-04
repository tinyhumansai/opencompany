//! A workflow run's terminal reading, as one word (issue #981).
//!
//! # Why the host owns this
//!
//! A run's outcome was spread across six fields — `running`, `error`,
//! `cancelled`, `blockedNodes`, `deliveries`, `pendingApprovals` — and no
//! surface said what they added up to. The only place that answered "did this
//! run succeed?" was the console's TypeScript, so every other reader had to
//! re-derive it and the obvious derivation is wrong: a run whose nodes all
//! reported `ok` looks green even when its report never left the process,
//! because delivery is host-side and post-engine (`crate::workflows::delivery`)
//! and never touches a node's status.
//!
//! That is not hypothetical. The QA pass on issue #981 watched a run paint its
//! `output` node **`DONE`, green**, list it as **`ok`** in the Steps panel, and
//! score PASS in a harness folding `nodes[].status` — while the run's own
//! delivery row said `channel-not-wired` and the report was gone. Three readers,
//! three transcriptions of the same ladder, and the one fact that mattered
//! lived in none of them.
//!
//! [`WorkflowRunVerdict`] is that ladder, once, on the host. Both run DTOs
//! serialize it, so a client reads the verdict instead of inventing one.
//!
//! # Derived, never stored
//!
//! Nothing journals a verdict. It is a pure function of fields already on the
//! wire, computed at serialization time, which buys three things:
//!
//! * **No migration.** Every run already in a company's journal re-scores on
//!   deploy, including the ones written before this existed.
//! * **No third state to keep in sync.** The read-side settle (issue #1081)
//!   rewrites `running` and `error` on a run it finds dead; a stored verdict
//!   would have to be rewritten alongside them, and the one that was forgotten
//!   would be the bug. A derived one is correct by construction.
//! * **No new failure mode.** A verdict cannot disagree with the rows it was
//!   read from, because there is only ever one reading.
//!
//! # What it deliberately does NOT do
//!
//! It does not populate a run's `error`, and it does not flip any
//! `nodes[].status`. A dropped report is not a broken graph: the nodes ran, the
//! work is valid, and the fix is a destination or a runtime wiring, not a node.
//! Marking the run failed would send the copilot's fix-from-run at a graph that
//! was fine, inflate the failure count, and collapse the three terminal
//! readings issue #383 kept apart. So `undelivered` is its **own** reading —
//! neither `failed` nor `ok` — and every existing consumer of `error`,
//! `cancelled`, `running` and `nodes[].status` sees exactly what it saw before.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::ports::workflow_runner::{
    DeliveryReason, DeliveryReport, DeliveryStatus, WorkflowBlockedNode,
};

/// The approvals queue as it is **right now**, in the two shapes a workflow run
/// can be joined against it by (issue #1189).
///
/// # Why two shapes and not one
///
/// A run has two ways to stop for a person, and they leave different traces.
///
/// * A **blocked node** — an agent node whose gated tool calls were parked by
///   `park_gated_calls` — records the ids those parks returned on
///   [`WorkflowBlockedNode::approval_ids`](crate::ports::WorkflowBlockedNode).
///   Those cards are ordinary tool-call effects (the policy stamps an `agent`
///   on them), so they carry no node id and can only be joined **by id**. That
///   is the join issue #1143 added.
/// * A **gate** — a `requires_approval` node the engine paused at — is parked by
///   `park_pending_gates` as a `workflow.approve` effect and records *nothing*:
///   no approval-row receipt, no blocked-node row, only the node id on the run's
///   `pendingApprovals`. Its card is the only thing that knows the pair, and it
///   knows it exactly: `Effect::run_id` is the run that paused and
///   `payload.node_id` is the gate. So that shape is joined **by
///   `(run_id, node_id)`**.
///
/// #1143's join is keyed on ids the second shape does not have, which is why it
/// could not reach it — and the gate shape is the larger half of the defect.
///
/// # Built where the raw effects are
///
/// Assembled by `CompanyRuntime::live_approvals`, off the journal's parked
/// effects, so no raw `Effect` has to reach the HTTP layer. The projected
/// [`ApprovalSummary`](crate::runtime::ApprovalSummary) is not a substitute: its
/// `payload` is `display_payload` — redacted and node-budget-bounded — so a
/// node id read back out of it would be reading a rendering, and would drift
/// the day the redaction rules change.
#[derive(Clone, Debug, Default)]
pub struct LiveApprovals {
    ids: HashSet<String>,
    gates: HashSet<(String, String)>,
}

impl LiveApprovals {
    /// Records a live approval id — every parked card, whatever shape it is.
    pub fn insert_id(&mut self, id: impl Into<String>) {
        self.ids.insert(id.into());
    }

    /// Records a live `(run, gate node)` pair — a parked `workflow.approve`
    /// card, which is the only shape that knows both halves.
    pub fn insert_gate(&mut self, run_id: impl Into<String>, node_id: impl Into<String>) {
        self.gates.insert((run_id.into(), node_id.into()));
    }

    /// Whether the queue still holds this approval id.
    pub fn holds_id(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// Whether the queue still holds a gate card for this run's node.
    ///
    /// Keyed on the pair rather than the node alone: two runs of the same
    /// workflow park the same node id, and answering "some run's `fetch_bbc` is
    /// still parked" about *this* run would keep a stranded run advertised as
    /// approvable for as long as any sibling run has a live card.
    pub fn holds_gate(&self, run_id: &str, node_id: &str) -> bool {
        // Borrowed lookup without allocating a `(String, String)`: the set is
        // small, and this is called once per pending node per returned run.
        self.gates
            .iter()
            .any(|(run, node)| run == run_id && node == node_id)
    }
}

/// What a workflow run adds up to, as a closed set (issue #981).
///
/// The order of the variants is the **precedence order** in which they are
/// tested, and the order is the whole content of the type — see
/// [`WorkflowRunVerdict::of`]. Every arm below the first exists because the
/// state it names had been scoring green on some surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowRunVerdict {
    /// Started and not yet settled. Neither succeeded nor failed, and painting
    /// it as either is a claim the host has not made.
    Running,
    /// The run carries an error — a genuine break, or the boot sweep's
    /// [`INTERRUPTED_BY_RESTART`](crate::runtime::INTERRUPTED_BY_RESTART).
    Failed,
    /// An operator stopped it (issue #383). Not a fault, and deliberately not
    /// grouped with `failed`: nothing about the graph went wrong.
    Stopped,
    /// Every person this run stopped for has **nothing left to answer**, and
    /// the run cannot go on (issue #1189).
    ///
    /// The reading that did not exist, and whose absence is the whole defect:
    /// a run whose gates have no card left in the queue went on scoring
    /// [`AwaitingApproval`](Self::AwaitingApproval) forever, so a third of one
    /// tenant's history claimed to be waiting on a person who had nothing to
    /// decide. There was no honest word for it, so it kept the dishonest one.
    ///
    /// Above [`Blocked`](Self::Blocked) and [`AwaitingApproval`](Self::AwaitingApproval)
    /// because it *contradicts* them rather than refining them: both tell an
    /// operator to go and decide something, and this is the state in which
    /// there is nothing to decide. The more specific fact wins, exactly as
    /// `failed` wins over `undelivered`.
    ///
    /// **It claims nothing about why.** Approving a gate does not continue the
    /// parent run — `resume_run` spawns a *new* run with a new id and records
    /// no link back — so a run whose gates were all approved and a run whose
    /// cards were lost are indistinguishable from here. What is observable, and
    /// all this word means, is that nothing in the queue is waiting on this run
    /// any more and no decision can move it.
    Stranded,
    /// A node stopped short because a tool call inside its turn is waiting on a
    /// person (issue #881). Not a failure and not a pause — the run will not
    /// continue on its own.
    Blocked,
    /// The run did its work and at least one report **did not reach its
    /// destination and will not without a change** (issue #981).
    ///
    /// The one this type was added for. It outranks
    /// [`AwaitingApproval`](Self::AwaitingApproval) because a report that needs
    /// a fix is worse news than one waiting on a human, and it ranks *below*
    /// [`Failed`](Self::Failed) and [`Blocked`](Self::Blocked) because those
    /// describe a run that did not finish its work at all.
    Undelivered,
    /// Something about this run is waiting on a person: a gate it paused at, or
    /// a report parked in Approvals (issue #846).
    AwaitingApproval,
    /// The run settled with no failure, no stop, no stranding, no block, no
    /// dropped report and nobody left to answer — but at least one node under
    /// `on_error: continue|route` errored and the graph kept going past it
    /// (issue #1865).
    ///
    /// Amber, not red: the run is not [`Failed`](Self::Failed) — the author
    /// asked for the branch to survive the error, and it did, so calling the
    /// whole run a failure would override a choice the graph's own config
    /// made. But it is not [`Ok`](Self::Ok) either — `WorkflowRunVerdict::of`
    /// never read `nodes[].status` before this, so a run with an errored node
    /// scored clean on every surface but the canvas overlay, which is the
    /// silent half of issue #1865.
    ///
    /// Ranked **last**, immediately above `Ok` — every other non-`Ok` verdict
    /// describes something more actionable than "a soft node error happened
    /// inside an otherwise-settled run", and none of them may be hidden behind
    /// this one. A run that is also `Failed`, `Stopped`, `Stranded`, `Blocked`,
    /// `Undelivered` or `AwaitingApproval` reports that instead.
    Degraded,
    /// Finished, delivered what it routed, and is waiting on nobody.
    Ok,
}

impl WorkflowRunVerdict {
    /// The wire token, matching this type's serde rendering exactly.
    ///
    /// Serde owns the wire, and this is here for the surfaces that are not
    /// JSON — the orchestrator's run summary, a log line, a test message — so
    /// they cannot drift into a second spelling of the same word.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Stranded => "stranded",
            Self::Blocked => "blocked",
            Self::Undelivered => "undelivered",
            Self::AwaitingApproval => "awaiting-approval",
            Self::Degraded => "degraded",
            Self::Ok => "ok",
        }
    }

    /// Reads a run's verdict off the facts, **in precedence order**.
    ///
    /// The order is the check, and each arm records a fall-through that used to
    /// score green:
    ///
    /// * `running` first — an unsettled run has no deliveries yet, no error and
    ///   no cancel, so without this arm it falls all the way to `ok` and the
    ///   host claims a run that has not finished succeeded.
    /// * `failed` next, so **a run that broke mid-graph and also dropped a
    ///   report reports the break**. The more serious fact wins; the delivery
    ///   rows are still on the response for whoever wants both.
    /// * `stopped` before the delivery reads (issue #383) — a stop somebody
    ///   asked for is not a fault, and a cancelled run has no deliveries to
    ///   weigh anyway.
    /// * `stranded` above both of them (issue #1189) — a run every one of whose
    ///   gates has lost its card is the one state in which "go and decide it"
    ///   is false, and `blocked` and `awaiting-approval` both say exactly that.
    ///   A run only **partly** stranded keeps its old verdict: something there
    ///   really is still decidable, and the per-node count carries the rest.
    /// * `blocked` before the delivery reads (issue #881) — a blocked run
    ///   carries no error, is not cancelled, is not running and routed no
    ///   report, which is precisely the shape that fell through every check.
    /// * `undelivered` before `awaiting-approval` (issue #981) — a report that
    ///   will not go out without a change outranks one waiting on a human.
    /// * `awaiting-approval` reads the **gates too**, not the delivery rows
    ///   alone (issue #846): a run that paused at a `requires_approval` node
    ///   never reached an `output` node, so a delivery-only read scored the
    ///   gated case — the common one — as clean.
    /// * `degraded` **last**, immediately before `Ok` (issue #1865) — a node
    ///   under `on_error: continue|route` errored and the run kept going past
    ///   it. Checked after everything above so a run that is ALSO failed,
    ///   stopped, stranded, blocked, undelivered or awaiting approval reports
    ///   that instead: a soft node error must never hide a hard failure or a
    ///   decidable gate.
    pub fn of(facts: RunVerdictFacts<'_>) -> Self {
        if facts.running {
            return Self::Running;
        }
        if facts.failed() {
            return Self::Failed;
        }
        if facts.cancelled {
            return Self::Stopped;
        }
        if facts.fully_stranded() {
            return Self::Stranded;
        }
        if facts.blocked_nodes > 0 {
            return Self::Blocked;
        }
        if undelivered_count(facts.deliveries) > 0 {
            return Self::Undelivered;
        }
        if awaiting_count(facts.deliveries, facts.pending_approvals) > 0 {
            return Self::AwaitingApproval;
        }
        if facts.errored_nodes > 0 {
            return Self::Degraded;
        }
        Self::Ok
    }
}

impl std::fmt::Display for WorkflowRunVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The facts a verdict is read from — **exactly** the fields both run DTOs
/// already serialize, and nothing else.
///
/// A named struct rather than six positional arguments, because five of them
/// are `bool`/`usize` and a transposed pair would compile silently into a wrong
/// verdict on one surface only.
#[derive(Clone, Copy, Debug)]
pub struct RunVerdictFacts<'a> {
    /// The run has started and not settled.
    pub running: bool,
    /// The run's `error`, when it has one.
    pub error: Option<&'a str>,
    /// An operator stopped the run (issue #383).
    pub cancelled: bool,
    /// How many nodes blocked on a human (issue #881).
    pub blocked_nodes: usize,
    /// One row per delivery attempt this run made.
    pub deliveries: &'a [DeliveryReport],
    /// How many nodes the run is waiting on a human for.
    pub pending_approvals: usize,
    /// How many of those nodes have **no live card left** (issue #1189) — the
    /// output of [`stranded_approvals`], which is where the join lives.
    ///
    /// Never greater than [`pending_approvals`](Self::pending_approvals), since
    /// it is a filter over the same list. Zero for every caller that cannot
    /// reconcile against the queue, which is the pre-#1189 reading.
    pub stranded_approvals: usize,
    /// How many of this run's nodes settled `Error` (issue #1865) — genuine
    /// engine errors only, read **after** [`reclassify_blocked`] (or the
    /// read-side `relabel_blocked` twin) has flipped a blocked node's row to
    /// [`Blocked`](crate::ports::types::WorkflowNodeStatus::Blocked), so a node
    /// waiting on a person is never counted as one that broke.
    ///
    /// [`reclassify_blocked`]: crate::workflows::runner
    ///
    /// Feeds [`Degraded`](WorkflowRunVerdict::Degraded) only, and only when
    /// checked last: a run under `on_error: continue|route` that also failed,
    /// stopped, stranded, blocked, dropped a report or is still awaiting an
    /// answer reports that instead — this count never overrides a more
    /// specific fact.
    pub errored_nodes: usize,
}

impl RunVerdictFacts<'_> {
    /// Whether the run carries a failure.
    ///
    /// An **empty** error string is not one. No producer writes one today, and
    /// the console's `if (run.error)` has always read it as falsy — so the host
    /// agreeing costs nothing and removes a way for the two to disagree about a
    /// run neither of them can explain.
    fn failed(&self) -> bool {
        self.error.is_some_and(|e| !e.is_empty())
    }

    /// Whether **every** person this run stopped for has nothing left to answer
    /// (issue #1189).
    ///
    /// Three conditions, and each excludes a run that is still actionable:
    ///
    /// * it stopped for somebody at all — a run with no gates was never
    ///   awaiting anything, and `stranded` is a correction to `awaiting`, not a
    ///   new way to score a clean run;
    /// * **all** of those gates lost their card. A partly stranded run keeps
    ///   its old verdict, because a decision that can still be made must still
    ///   be offered; the per-node count is what says the rest was lost;
    /// * no report is parked either. A `pending` delivery row is a *second*
    ///   thing waiting on a person, on its own queue, and it is untouched by
    ///   the gate join — so a run holding one is still genuinely awaiting.
    ///
    /// `==` rather than `>=` on purpose: the count is a filter over the same
    /// list and cannot exceed it, so a larger value means a caller built the
    /// facts by hand and got them wrong — in which case falling through to the
    /// old reading is the safe direction.
    fn fully_stranded(&self) -> bool {
        self.pending_approvals > 0
            && self.stranded_approvals == self.pending_approvals
            && !self
                .deliveries
                .iter()
                .any(|d| matches!(d.status, DeliveryStatus::Pending))
    }
}

/// Whether **this one report** did not reach a destination and will not without
/// a change (issue #981).
///
/// The single rung every surface stands on: the verdict below, the scheduler's
/// alert number, the sidecar's and the orchestrator's summaries, the console's
/// "N not delivered" badge, the SSE toast, and — since this exists — the
/// per-node delivery marker the console paints on the `output` node itself.
/// Written once here because a rung that only some readers honour is worse than
/// no rung at all.
///
/// # Status alone is not the reading
///
/// `sent` obviously did land and `pending` is a report parked for an operator's
/// approval — counting the latter here would score a working approvals queue as
/// a failure, so it is counted by [`awaiting_count`] instead.
///
/// The interesting half is [`Skipped`](DeliveryStatus::Skipped), which the
/// delivery path writes for three genuinely different situations. The axis that
/// separates them is **whether the report's fate is accounted for**, not whether
/// it "was owed to an address":
///
/// * [`AlreadyDelivered`](DeliveryReason::AlreadyDelivered) — an earlier run in
///   this approval lineage **sent it** (issue #438). Approving a gate re-runs the
///   graph from the trigger, so every upstream `output` node is reached a second
///   time; the report is at its destination and re-counting it as lost would
///   paint every resumed gate red.
/// * [`DryRun`](DeliveryReason::DryRun) — a test run (issue #542). Nothing was
///   attempted, on purpose, in a mode the operator chose. Counting it made the
///   *only* safe way to try a graph report a failure every single time.
/// * [`NoDestinationConfigured`](DeliveryReason::NoDestinationConfigured) — the
///   report was **produced and then lost**, with nothing accounting for it
///   (issue #925). This row exists precisely so that "the author routed nothing
///   on purpose" and "the author never configured a destination" stop being the
///   same observation; excusing it here would restore the silence issues #947
///   and #963 were filed about. **It counts.**
///
/// The match on [`DeliveryStatus`] is exhaustive and only the `Skipped` arm
/// reads a reason, so a new delivery status cannot be added without classifying
/// it, and a hypothetical `failed`/`dry-run` pair still counts.
///
/// A row carrying [`Unspecified`](DeliveryReason::Unspecified) — the only
/// reachable value on a `WorkflowRunFinished` journaled before issue #248 added
/// the field — counts, which is the safe direction: an unreadable reason must
/// not excuse a report from the number an operator acts on.
pub fn is_undelivered(report: &DeliveryReport) -> bool {
    match report.status {
        DeliveryStatus::Sent | DeliveryStatus::Pending => false,
        DeliveryStatus::Skipped => !matches!(
            report.reason,
            DeliveryReason::AlreadyDelivered | DeliveryReason::DryRun
        ),
        DeliveryStatus::Denied | DeliveryStatus::Failed => true,
    }
}

/// How many of a run's reports did **not** reach their destination and will not
/// without a change — the count worth acting on.
///
/// A fold of [`is_undelivered`], which is where the reasoning lives.
pub fn undelivered_count(deliveries: &[DeliveryReport]) -> usize {
    deliveries.iter().filter(|d| is_undelivered(d)).count()
}

/// Everything about a run that is waiting on a person: the gates it paused at
/// **and** the reports it parked (issue #846).
///
/// The two were never read together, which is what let a run report success
/// while a human had not answered it — a run that paused at a
/// `requires_approval` node never reaches an `output` node, so its deliveries
/// are empty and a delivery-only read scored it clean.
pub fn awaiting_count(deliveries: &[DeliveryReport], pending_approvals: usize) -> usize {
    pending_approvals
        + deliveries
            .iter()
            .filter(|d| matches!(d.status, DeliveryStatus::Pending))
            .count()
}

/// How many of a run's pending gate nodes have **no live card left** — the
/// question "is anything here still decidable?", answered against the queue as
/// it is now (issue #1189).
///
/// `awaiting_count` reads `pendingApprovals` raw, which is a receipt of where
/// the run stopped and cannot go stale — but the *question* each entry points at
/// can, and does. `ApprovalParked` is journaled at `Durability::Process` on the
/// reasoning that losing it is harmless because "the agent parks it again on its
/// next attempt"; that holds for a chat turn, which retries, and is false for a
/// workflow run, which halted at the gate and never re-enters it.
///
/// # A node is live if EITHER join finds it
///
/// * `(run_id, node)` is a parked `workflow.approve` card — the gate shape.
/// * any of the node's [`WorkflowBlockedNode::approval_ids`] is still parked —
///   the blocked-node shape, whose cards are tool-call effects carrying no node
///   id of their own.
///
/// **Any** live id on a blocked node makes the node live: while one of its
/// gated calls can still be decided, the node is still a question, and the
/// per-node `stranded` count that #1143 renders is what carries the nuance of a
/// partial loss.
///
/// # A run with no id answers 0
///
/// A row journaled before issue #371 carries no `run_id`, so the gate join has
/// no key — and calling every node of it stranded on the strength of a missing
/// field would retire live work an operator can still act on. Zero is the
/// pre-#1189 reading, which is the safe direction here for exactly the reason
/// `denied_in_input` is tolerant: inventing a dead end is worse than missing
/// one.
pub fn stranded_approvals(
    run_id: Option<&str>,
    pending_approvals: &[String],
    blocked: &[WorkflowBlockedNode],
    live: &LiveApprovals,
) -> usize {
    let Some(run_id) = run_id else {
        return 0;
    };
    pending_approvals
        .iter()
        .filter(|node| !node_still_decidable(run_id, node, blocked, live))
        .count()
}

/// Whether either join still finds a card for this run's gate node.
fn node_still_decidable(
    run_id: &str,
    node: &str,
    blocked: &[WorkflowBlockedNode],
    live: &LiveApprovals,
) -> bool {
    live.holds_gate(run_id, node)
        || blocked
            .iter()
            .filter(|b| b.node_id == node)
            .any(|b| b.approval_ids.iter().any(|id| live.holds_id(id)))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::DeliveryReason;

    fn row(status: DeliveryStatus, reason: DeliveryReason) -> DeliveryReport {
        DeliveryReport {
            node: "report".into(),
            kind: "channel".into(),
            target: Some("engineering".into()),
            status,
            detail: "detail".into(),
            reason,
        }
    }

    /// A run that finished cleanly and routed nothing — the base every case
    /// below varies by exactly one fact.
    fn clean() -> RunVerdictFacts<'static> {
        RunVerdictFacts {
            running: false,
            error: None,
            cancelled: false,
            blocked_nodes: 0,
            deliveries: &[],
            pending_approvals: 0,
            stranded_approvals: 0,
            errored_nodes: 0,
        }
    }

    #[test]
    fn a_clean_run_is_ok() {
        assert_eq!(WorkflowRunVerdict::of(clean()), WorkflowRunVerdict::Ok);
    }

    /// The defect issue #981 filed: every node `ok`, no error, nothing
    /// cancelled — and the report is gone.
    #[test]
    fn a_run_whose_only_failure_is_delivery_is_not_ok() {
        let dropped = [row(DeliveryStatus::Failed, DeliveryReason::ChannelNotWired)];
        let verdict = WorkflowRunVerdict::of(RunVerdictFacts {
            deliveries: &dropped,
            ..clean()
        });
        assert_eq!(verdict, WorkflowRunVerdict::Undelivered);
        assert_ne!(verdict, WorkflowRunVerdict::Ok);
        // …and it is not reported as a failure either. The nodes ran.
        assert_ne!(verdict, WorkflowRunVerdict::Failed);
    }

    /// The other two refusals issue #981 names reach the same verdict, and
    /// through their own `DeliveryStatus` rather than through a shared one — so
    /// this pins that the count is not accidentally reading only `Failed`.
    #[test]
    fn a_denied_or_skipped_report_is_undelivered_too() {
        for status in [DeliveryStatus::Denied, DeliveryStatus::Skipped] {
            let rows = [row(status, DeliveryReason::EmailNotGranted)];
            assert_eq!(
                WorkflowRunVerdict::of(RunVerdictFacts {
                    deliveries: &rows,
                    ..clean()
                }),
                WorkflowRunVerdict::Undelivered,
                "{status:?} is a report that did not go out"
            );
        }
    }

    #[test]
    fn a_run_that_delivered_everything_is_unchanged() {
        let sent = [row(DeliveryStatus::Sent, DeliveryReason::ChannelPosted)];
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                deliveries: &sent,
                ..clean()
            }),
            WorkflowRunVerdict::Ok
        );
    }

    /// The more serious fact first: a run that broke mid-graph AND dropped its
    /// report reports the break, not the drop.
    #[test]
    fn a_failed_run_that_also_dropped_a_report_reads_failed() {
        let dropped = [row(DeliveryStatus::Failed, DeliveryReason::ChannelNotWired)];
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                error: Some("node `draft` errored"),
                deliveries: &dropped,
                ..clean()
            }),
            WorkflowRunVerdict::Failed
        );
    }

    #[test]
    fn the_precedence_order_is_the_whole_check() {
        let dropped = [row(DeliveryStatus::Failed, DeliveryReason::ChannelNotWired)];
        // Every arm asserted against a fact set that ALSO satisfies every arm
        // below it, so a reordering breaks this rather than passing by luck.
        let everything = RunVerdictFacts {
            running: true,
            error: Some("boom"),
            cancelled: true,
            blocked_nodes: 1,
            deliveries: &dropped,
            pending_approvals: 1,
            // Issue #1189: the one gate this run stopped for has no card left,
            // so the facts satisfy `stranded` too.
            stranded_approvals: 1,
            // Issue #1865: also satisfies `degraded`, so this fact set proves
            // every arm outranks it too.
            errored_nodes: 1,
        };
        assert_eq!(
            WorkflowRunVerdict::of(everything),
            WorkflowRunVerdict::Running
        );
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                running: false,
                ..everything
            }),
            WorkflowRunVerdict::Failed
        );
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                running: false,
                error: None,
                ..everything
            }),
            WorkflowRunVerdict::Stopped
        );
        // Issue #1189. It sits ABOVE `blocked` and above `awaiting-approval`,
        // and these facts satisfy both — which is the whole claim: a run whose
        // every gate lost its card must not be told to go and decide it.
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                running: false,
                error: None,
                cancelled: false,
                ..everything
            }),
            WorkflowRunVerdict::Stranded
        );
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                running: false,
                error: None,
                cancelled: false,
                stranded_approvals: 0,
                ..everything
            }),
            WorkflowRunVerdict::Blocked
        );
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                running: false,
                error: None,
                cancelled: false,
                stranded_approvals: 0,
                blocked_nodes: 0,
                ..everything
            }),
            WorkflowRunVerdict::Undelivered
        );
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                running: false,
                error: None,
                cancelled: false,
                stranded_approvals: 0,
                blocked_nodes: 0,
                deliveries: &[],
                ..everything
            }),
            WorkflowRunVerdict::AwaitingApproval
        );
        // Issue #1865: with every fact above cleared and only the errored node
        // left, the run reads `degraded` — not `ok`, and not `failed`.
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                running: false,
                error: None,
                cancelled: false,
                stranded_approvals: 0,
                blocked_nodes: 0,
                deliveries: &[],
                pending_approvals: 0,
                ..everything
            }),
            WorkflowRunVerdict::Degraded
        );
        // …and clearing the errored-node count too is the only way back to
        // `ok`, which is the base case this whole ladder falls through to.
        // Every field of `everything` is overridden here, so this is written
        // out in full rather than spread — the base case earns no shortcut.
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                running: false,
                error: None,
                cancelled: false,
                stranded_approvals: 0,
                blocked_nodes: 0,
                deliveries: &[],
                pending_approvals: 0,
                errored_nodes: 0,
            }),
            WorkflowRunVerdict::Ok
        );
    }

    /// A parked report is waiting on a person, not on a fix — so it must never
    /// land in the undelivered count, which would badge a working approvals
    /// queue as a failure.
    #[test]
    fn a_parked_report_is_awaiting_not_undelivered() {
        let parked = [row(
            DeliveryStatus::Pending,
            DeliveryReason::ParkedForApproval,
        )];
        assert_eq!(undelivered_count(&parked), 0);
        assert_eq!(awaiting_count(&parked, 0), 1);
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                deliveries: &parked,
                ..clean()
            }),
            WorkflowRunVerdict::AwaitingApproval
        );
    }

    /// Issue #846: a gated run reaches no `output` node, so its verdict has to
    /// come off `pending_approvals` or it scores clean.
    #[test]
    fn a_gated_run_with_no_deliveries_is_awaiting() {
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                pending_approvals: 1,
                ..clean()
            }),
            WorkflowRunVerdict::AwaitingApproval
        );
    }

    /// An error string the host never writes, read the way the console reads
    /// it: empty is not a failure.
    #[test]
    fn an_empty_error_is_not_a_failure() {
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                error: Some(""),
                ..clean()
            }),
            WorkflowRunVerdict::Ok
        );
    }

    /// The wire tokens are the console's eight words, and `as_str` may not
    /// drift from them.
    #[test]
    fn the_wire_tokens_are_the_consoles_words() {
        for (verdict, token) in [
            (WorkflowRunVerdict::Running, "running"),
            (WorkflowRunVerdict::Failed, "failed"),
            (WorkflowRunVerdict::Stopped, "stopped"),
            (WorkflowRunVerdict::Stranded, "stranded"),
            (WorkflowRunVerdict::Blocked, "blocked"),
            (WorkflowRunVerdict::Undelivered, "undelivered"),
            (WorkflowRunVerdict::AwaitingApproval, "awaiting-approval"),
            (WorkflowRunVerdict::Degraded, "degraded"),
            (WorkflowRunVerdict::Ok, "ok"),
        ] {
            assert_eq!(
                serde_json::to_value(verdict).expect("serializes"),
                serde_json::Value::String(token.to_string())
            );
            assert_eq!(verdict.as_str(), token);
            assert_eq!(verdict.to_string(), token);
        }
    }

    /// Issue #981, the second half: a **test run** attempted nothing, on
    /// purpose, so its rows are not reports that went missing.
    ///
    /// This was a live false positive, not a theoretical one — `deliver_outputs_dry`
    /// writes one `skipped`/`dry-run` row per routed `output` node, so before
    /// this every single test run of a graph with a destination scored
    /// `undelivered` and the console badged the safest thing an operator can do
    /// as a failure.
    #[test]
    fn a_dry_run_is_not_undelivered() {
        let dry = [row(DeliveryStatus::Skipped, DeliveryReason::DryRun)];
        assert!(!is_undelivered(&dry[0]));
        assert_eq!(undelivered_count(&dry), 0);
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                deliveries: &dry,
                ..clean()
            }),
            WorkflowRunVerdict::Ok
        );
    }

    /// Issue #438: approving a gate re-runs the graph from the trigger, so an
    /// `output` node upstream of the gate is reached a second time and
    /// deliberately not sent again. The report is at its destination; the
    /// continuation is not a run that lost one.
    #[test]
    fn an_already_delivered_report_is_not_undelivered() {
        let again = [row(
            DeliveryStatus::Skipped,
            DeliveryReason::AlreadyDelivered,
        )];
        assert!(!is_undelivered(&again[0]));
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                deliveries: &again,
                ..clean()
            }),
            WorkflowRunVerdict::Ok
        );
    }

    /// The deliberate **non**-move, and the reason the other two could move at
    /// all: an `output` node with nowhere to send produced a report and lost it,
    /// with nothing accounting for it. Issue #925 added the row precisely so
    /// that case stops being indistinguishable from a graph that routed nothing
    /// on purpose; excusing it here restores the silence issues #947 and #963
    /// were filed about.
    #[test]
    fn an_output_node_with_no_destination_is_still_undelivered() {
        let nowhere = [row(
            DeliveryStatus::Skipped,
            DeliveryReason::NoDestinationConfigured,
        )];
        assert!(is_undelivered(&nowhere[0]));
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                deliveries: &nowhere,
                ..clean()
            }),
            WorkflowRunVerdict::Undelivered
        );
    }

    /// A row journaled before issue #248 added `reason` deserializes as
    /// `Unspecified`, and an unreadable reason must not excuse a report from the
    /// number an operator acts on.
    #[test]
    fn a_skipped_row_with_no_recorded_reason_still_counts() {
        let old = [row(DeliveryStatus::Skipped, DeliveryReason::Unspecified)];
        assert!(is_undelivered(&old[0]));
    }

    /// Only the `skipped` arm reads a reason. A `failed` row is a report that
    /// was attempted and did not work, whatever it claims about why — so the
    /// two exemptions cannot leak onto a status that means something broke.
    #[test]
    fn the_exemptions_are_scoped_to_skipped() {
        for status in [DeliveryStatus::Failed, DeliveryStatus::Denied] {
            for reason in [DeliveryReason::DryRun, DeliveryReason::AlreadyDelivered] {
                assert!(
                    is_undelivered(&row(status, reason)),
                    "{status:?}/{reason:?} is not a skip"
                );
            }
        }
    }

    // ── Issue #1189: a run nobody can act on any more ────────────────────────

    /// The marketing tenant's 34 runs: three gate nodes on `pendingApprovals`,
    /// no blocked-node rows at all, and an empty approvals queue.
    ///
    /// Before this arm they scored `awaiting-approval` forever — a third of the
    /// tenant's whole history claiming to wait on a person with nothing to
    /// answer, and #1143's reconciliation could not reach them because it joins
    /// on approval ids this shape never had.
    #[test]
    fn a_run_whose_every_gate_lost_its_card_is_stranded_not_awaiting() {
        let verdict = WorkflowRunVerdict::of(RunVerdictFacts {
            pending_approvals: 3,
            stranded_approvals: 3,
            ..clean()
        });
        assert_eq!(verdict, WorkflowRunVerdict::Stranded);
        assert_ne!(
            verdict,
            WorkflowRunVerdict::AwaitingApproval,
            "nothing in the queue is waiting on this run, so it must not say so"
        );
    }

    /// The negative that makes the test above mean anything.
    ///
    /// A rule that fired on *any* stranded gate would satisfy it and be worse
    /// than no rule: it would retire a run with a decision still sitting in the
    /// queue. Two of the three are gone; the third can still be made, so the
    /// verdict must go on saying so and the per-node count carries the loss.
    #[test]
    fn a_partly_stranded_run_is_still_awaiting() {
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                pending_approvals: 3,
                stranded_approvals: 1,
                ..clean()
            }),
            WorkflowRunVerdict::AwaitingApproval
        );
    }

    /// A parked **report** is a second thing waiting on a person, on its own
    /// queue, and the gate join does not look at it. So a run whose gates are
    /// all stranded but whose report is still parked is genuinely awaiting —
    /// there really is a card to decide.
    #[test]
    fn a_stranded_run_with_a_parked_report_is_still_awaiting() {
        let parked = [row(
            DeliveryStatus::Pending,
            DeliveryReason::ParkedForApproval,
        )];
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                deliveries: &parked,
                pending_approvals: 2,
                stranded_approvals: 2,
                ..clean()
            }),
            WorkflowRunVerdict::AwaitingApproval
        );
    }

    /// The `feature_pipeline` shape from the issue: a blocked node whose every
    /// card the queue has lost. #1143 already says so in the blocked-node list;
    /// this is the verdict finally agreeing with it instead of reading
    /// `blocked` — which, like `awaiting-approval`, tells the operator to go
    /// and decide something that is not there.
    #[test]
    fn a_fully_stranded_blocked_run_reads_stranded_not_blocked() {
        let verdict = WorkflowRunVerdict::of(RunVerdictFacts {
            blocked_nodes: 1,
            pending_approvals: 1,
            stranded_approvals: 1,
            ..clean()
        });
        assert_eq!(verdict, WorkflowRunVerdict::Stranded);
        assert_ne!(verdict, WorkflowRunVerdict::Blocked);
    }

    /// A run with no gates at all is not stranded, whatever else is true of it.
    /// `stranded` is a correction to `awaiting`, never a new way to score a run
    /// that was waiting on nobody.
    #[test]
    fn a_run_that_stopped_for_nobody_is_never_stranded() {
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                pending_approvals: 0,
                stranded_approvals: 0,
                ..clean()
            }),
            WorkflowRunVerdict::Ok
        );
    }

    // ── Issue #1189: the fold that reconciles a gate against the live queue ──

    /// A blocked node carrying `approval_ids`, for the id-keyed half of the
    /// join.
    fn blocked_node(node: &str, ids: &[&str]) -> WorkflowBlockedNode {
        WorkflowBlockedNode {
            node_id: node.to_string(),
            tools: vec!["shell".to_string()],
            approval_ids: ids.iter().map(|id| id.to_string()).collect(),
            unparkable: 0,
            stranded: 0,
            blockers: 0,
        }
    }

    fn nodes(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// The marketing tenant's shape: gate nodes on `pendingApprovals`, no
    /// blocked-node rows at all, and an empty queue.
    #[test]
    fn a_gate_with_no_card_left_is_stranded() {
        let live = LiveApprovals::default();
        assert_eq!(
            stranded_approvals(
                Some("run-1"),
                &nodes(&["fetch_bbc", "fetch_espn", "fetch_guardian"]),
                &[],
                &live
            ),
            3
        );
    }

    /// The negative that makes the one above mean anything: a fold that marked
    /// everything stranded would satisfy it and be worse than no fold at all.
    #[test]
    fn a_gate_whose_card_is_still_parked_is_not_stranded() {
        let mut live = LiveApprovals::default();
        live.insert_gate("run-1", "fetch_bbc");
        assert_eq!(
            stranded_approvals(Some("run-1"), &nodes(&["fetch_bbc"]), &[], &live),
            0
        );
    }

    /// Keyed on the **pair**. Two runs of one workflow park the same node id, so
    /// a node-only key would keep every historical run of `daily-sports-news`
    /// advertised as approvable for as long as any one of them has a live card.
    #[test]
    fn the_same_node_parked_under_a_different_run_does_not_count() {
        let mut live = LiveApprovals::default();
        live.insert_gate("run-2", "fetch_bbc");
        assert_eq!(
            stranded_approvals(Some("run-1"), &nodes(&["fetch_bbc"]), &[], &live),
            1
        );
    }

    /// The id-keyed half: a blocked node's cards carry no node id, so the node
    /// is reached through `approval_ids`.
    #[test]
    fn a_blocked_node_is_live_if_any_of_its_approvals_is() {
        let mut live = LiveApprovals::default();
        live.insert_id("appr-2");
        let blocked = [blocked_node("backend", &["appr-1", "appr-2", "appr-3"])];
        assert_eq!(
            stranded_approvals(Some("run-1"), &nodes(&["backend"]), &blocked, &live),
            0,
            "one decidable call still makes the node a question"
        );
    }

    /// …and the same node with every id gone is stranded, which is the
    /// `feature_pipeline` shape issue #1189 opens on.
    #[test]
    fn a_blocked_node_whose_every_approval_is_gone_is_stranded() {
        let live = LiveApprovals::default();
        let blocked = [blocked_node("backend", &["appr-1", "appr-2", "appr-3"])];
        assert_eq!(
            stranded_approvals(Some("run-1"), &nodes(&["backend"]), &blocked, &live),
            1
        );
    }

    /// A pre-#371 row has no run id, so the gate join has no key. Marking its
    /// nodes stranded on the strength of a missing field would retire work an
    /// operator can still act on.
    #[test]
    fn a_run_with_no_id_is_never_stranded() {
        let live = LiveApprovals::default();
        assert_eq!(
            stranded_approvals(None, &nodes(&["fetch_bbc"]), &[], &live),
            0
        );
    }

    // ── Issue #1865: a run whose node errored under `on_error: continue|route` ──

    /// The defect this issue filed: a node under `on_error: continue|route`
    /// errors, the graph keeps going, and the run reaches the end with no
    /// error, no cancel, nothing blocked, nothing undelivered and nobody
    /// awaited — which fell all the way through to `ok` before this arm
    /// existed.
    #[test]
    fn a_run_with_an_errored_continue_node_is_degraded_not_ok() {
        let verdict = WorkflowRunVerdict::of(RunVerdictFacts {
            errored_nodes: 1,
            ..clean()
        });
        assert_eq!(verdict, WorkflowRunVerdict::Degraded);
        assert_ne!(verdict, WorkflowRunVerdict::Ok);
        assert_ne!(
            verdict,
            WorkflowRunVerdict::Failed,
            "the author asked for the branch to survive the error"
        );
    }

    /// A run with no errored node is unaffected — the new arm changes nothing
    /// for the common case.
    #[test]
    fn a_clean_run_with_no_errored_nodes_is_still_ok() {
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                errored_nodes: 0,
                ..clean()
            }),
            WorkflowRunVerdict::Ok
        );
    }

    /// `degraded` is checked LAST — a run that is also genuinely `failed`
    /// (an error the host actually recorded, distinct from a per-node
    /// `on_error: continue` error) reports the failure, not the softer
    /// reading.
    #[test]
    fn an_errored_node_never_hides_a_real_failure() {
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                error: Some("node `draft` errored"),
                errored_nodes: 1,
                ..clean()
            }),
            WorkflowRunVerdict::Failed
        );
    }

    /// Nor does it hide a decidable gate — a run that is both degraded and
    /// still awaiting an answer reports the thing an operator can act on.
    #[test]
    fn an_errored_node_never_hides_awaiting_approval() {
        assert_eq!(
            WorkflowRunVerdict::of(RunVerdictFacts {
                pending_approvals: 1,
                errored_nodes: 1,
                ..clean()
            }),
            WorkflowRunVerdict::AwaitingApproval
        );
    }

    /// `sent` and `pending` are excused by **status**, so no reason can pull
    /// them into the count either.
    #[test]
    fn sent_and_pending_are_never_undelivered() {
        for status in [DeliveryStatus::Sent, DeliveryStatus::Pending] {
            for reason in [
                DeliveryReason::ChannelPosted,
                DeliveryReason::ParkedForApproval,
                DeliveryReason::NoDestinationConfigured,
            ] {
                assert!(!is_undelivered(&row(status, reason)));
            }
        }
    }
}
