//! The company **orchestrator**: the operator↔company chat as a first-class
//! delegating agent.
//!
//! Where the harness brain's default chat responder is just the first roster
//! agent, the orchestrator is the one place the operator asks anything and it
//! answers from whole-company context — grounding replies in the company's
//! durable facts and recent activity, and delegating work it should not do
//! itself. It is the roster agent whose manifest `tier = "orchestrator"`, or the
//! first agent when none is tagged (so a company without an orchestrator behaves
//! exactly as before).
//!
//! It reaches eight tools, all wired only onto the orchestrator agent:
//!
//! * [`QueryCompanyTool`] — a read surface over the company's [`FactStore`] and
//!   recent [`EventLog`] history.
//! * [`SpawnTaskTool`] / [`DelegateToDeskTool`] — delegation tools that push a
//!   [`Delegation`] onto a shared [`DelegationQueue`]. They perform no work
//!   themselves; the [`HarnessBrain`](crate::harness::HarnessBrain) drains the
//!   queue after the orchestrator's turn (v1: synchronous, in-cycle, capped at
//!   [`MAX_DELEGATIONS_PER_TURN`], no sub-agent re-delegation). Since issue #453
//!   they push only when a drain site has [claimed](DelegationQueue::claim) the
//!   queue; a turn run from a path that drains nothing gets an in-turn refusal
//!   instead of a receipt for work that will never happen.
//! * [`RunWorkflowTool`] — executes one of the company's saved workflow graphs
//!   by id via the [`WorkflowRunner`] port (issue #67). It loads the graph from
//!   the company source directory (the same loader the REST run route uses) and
//!   invokes the runner reached through a shared [`WorkflowRunnerHandle`], so a
//!   task waiting on a workflow can actually be run to completion. Unlike the
//!   delegation tools it runs the graph inline and returns a concise summary of
//!   the run rather than enqueuing deferred work.
//! * [`ReadRunOutputTool`] (issue #418) — the `run_workflow` companion. The run
//!   summary only previews each node's *last* item, clipped — so
//!   `read_run_output` pages a named node's full, unclipped output out of a
//!   bounded in-process [`RunOutputCache`] the run tool populates. No journal or
//!   workspace write is involved: the durable human record already exists in the
//!   console run drawer; this exists only so the same-process orchestrator agent
//!   can read what its own summary clipped.
//! * [`CreateWorkflowTool`] (issue #112) — authors and saves a brand-new
//!   workflow graph through the same validated-persist core the console
//!   `POST .../workflows` route runs, so the orchestrator can capture a
//!   repeatable process mid-chat; it lands enabled and runnable by
//!   [`RunWorkflowTool`] the same turn.
//! * [`AssignTaskTool`] / [`ReviewTaskTool`] (issue #186) — the board's
//!   lifecycle. `assign_task` sets or changes who owns an existing card;
//!   `review_task` records the orchestrator's verdict on one awaiting review
//!   (`approve` completes it to `done` — #171's transition, PR #179 — and
//!   `revise` returns it to To-do). Both enqueue a [`Delegation`] drained
//!   by the brain, like the other delegation tools.
//! * [`AddAgentTool`] (issue #71) — writes a new [`OverlayAgent`] through the
//!   same store path the console `POST .../team` route uses, so the
//!   orchestrator can bring on a teammate mid-chat.
//!
//! Compiled only under `feature = "openhuman"` (the whole `harness` module is).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::ports::store::company_write_lock;

use async_trait::async_trait;
use serde_json::{Value, json};

use openhuman_core::openhuman as oh;

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};

use crate::company::{
    Agent as ManifestAgent, RawEdge, RawNode, RawWorkflow, WorkflowDestinationDef, WorkflowFile,
    WorkflowNodeKind, create_company_workflow, list_workflows_union, load_workflow_union,
};
use crate::error::OpenCompanyError;
use crate::harness::lifecycle::ReviewDecision;
use crate::harness::workflow_refs::WorkflowRefQueue;
use crate::ports::events::EventLog;
use crate::ports::facts::FactStore;
use crate::ports::tasks::{TaskOutputAction, TaskOutputWorkflow};
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq, OverlayAgent};
use crate::ports::{CompanyStore, WorkflowRun, WorkflowRunner};

/// The manifest cognition-tier that marks the orchestrator agent.
///
/// Re-exported from [`crate::company`] rather than declared here (issue #264):
/// the console's agent detail route has to name the same tier in the default
/// build, where this module does not compile.
pub use crate::company::ORCHESTRATOR_TIER;

/// Max delegations one orchestrator turn may make (v1 cap) — delegation is
/// bounded so a runaway turn can't fan out unboundedly.
///
/// Enforced **at the tool boundary** since issue #419
/// ([`DelegationQueue::push_within_cap`]): a call past the cap is refused in the
/// model's own turn, naming the bound. It used to be enforced only in the drain,
/// which took `min(cap)` and destroyed the rest — after the tool had already
/// told the model the card would be opened this turn. Ask for five cards, get
/// told five were opened, find two.
pub const MAX_DELEGATIONS_PER_TURN: usize = 3;

/// How many recent events [`QueryCompanyTool`] surfaces.
const RECENT_EVENTS: usize = 10;
/// How many facts [`QueryCompanyTool`] surfaces.
const FACT_LIMIT: usize = 20;
/// Longest a single fact body may render in the insight document before it is
/// cut with an ellipsis. One verbose fact must not be able to crowd the whole
/// budget on its own; the full body is still reachable through the fact store.
const MAX_FACT_BODY_CHARS: usize = 400;
/// Byte ceiling for the whole Facts section of the insight document.
///
/// The insight document is handed to the model through the harness tool-result
/// path, which hard-cuts anything past
/// [`TOOL_RESULT_BUDGET_BYTES`](crate::harness::build::TOOL_RESULT_BUDGET_BYTES)
/// — and that outer cut is blind, so a facts list long enough to blow the
/// budget would take the facts `[TRUNCATED …]` marker AND every section below
/// it (Recent activity, Saved workflows, Team, Desks) over the edge with it,
/// including the Desks list `delegate_to_desk` depends on. Bounding the facts
/// section here — the one section with a `query` narrowing argument to fall
/// back on — keeps the announcement and the delegation-grounding sections
/// inside the outer budget. Half the budget leaves the other half for
/// everything below. Sized against the real ceiling per issue #417.
const FACTS_SECTION_BUDGET_BYTES: usize = crate::harness::build::TOOL_RESULT_BUDGET_BYTES / 2;

/// The `query_company` tool name.
pub const QUERY_COMPANY_TOOL: &str = "query_company";
// The `spawn_task` / `delegate_to_desk` names are the brain-agnostic canonical
// constants (issue #176) — re-exported here so the harness path and the hosted
// path share one definition and cannot drift.
use crate::runtime::builder::agent_effective_grants;
use crate::runtime::delegation_tools;
pub use crate::runtime::delegation_tools::{DELEGATE_TO_DESK_TOOL, SPAWN_TASK_TOOL};
/// The `run_workflow` tool name (issue #67).
pub const RUN_WORKFLOW_TOOL: &str = "run_workflow";
/// The `read_run_output` tool name (issue #418 — the `run_workflow` companion
/// that reads a run node's full, unclipped output out of the in-process cache).
pub const READ_RUN_OUTPUT_TOOL: &str = "read_run_output";
/// The `add_agent` tool name (issue #71 — Active Runtime Teammates).
pub const ADD_AGENT_TOOL: &str = "add_agent";
/// The `create_workflow` tool name (issue #112 — author a saved workflow graph).
pub const CREATE_WORKFLOW_TOOL: &str = "create_workflow";
/// The `assign_task` tool name (issue #186 — orchestrator lifecycle authority).
pub const ASSIGN_TASK_TOOL: &str = "assign_task";
/// The `review_task` tool name (issue #186 — orchestrator lifecycle authority).
pub const REVIEW_TASK_TOOL: &str = "review_task";

/// The id of the orchestrator agent for a roster: the first agent tagged
/// `tier = "orchestrator"`, else the first roster agent, else `None` (empty
/// roster). The fallback is what keeps a company with no tagged orchestrator
/// answering exactly as it did before this cell.
///
/// Delegates to [`crate::company::orchestrator_id`] (issue #264) so the
/// harness and the console's agent detail route answer "who is the
/// orchestrator?" from one rule. A second copy of the fallback here would let
/// the console label a teammate a worker while the harness handed it the
/// orchestrator's tools.
pub fn orchestrator_id(agents: &[ManifestAgent]) -> Option<String> {
    crate::company::orchestrator_id(agents).map(str::to_string)
}

/// Whether `tool` is one of the orchestrator's in-cycle delegation / roster-write
/// tools.
///
/// These enqueue internal work drained by the harness brain (a task card, or a
/// hand-off to a desk's lead member) or write to the company's own store (adding
/// a teammate), rather than reaching an external counterparty, so the
/// [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) classifies them as
/// internal — never an external effect to park or deny.
pub fn is_delegation_tool(tool: &str) -> bool {
    tool == SPAWN_TASK_TOOL
        || tool == DELEGATE_TO_DESK_TOOL
        || tool == ADD_AGENT_TOOL
        || tool == CREATE_WORKFLOW_TOOL
        || tool == ASSIGN_TASK_TOOL
        || tool == REVIEW_TASK_TOOL
}

/// The orchestrator persona brief, appended to the orchestrator agent's persona.
///
/// # The two decisions are named separately (issue #442)
///
/// This brief used to say "delegate when a request belongs to a specialist desk
/// **or** should be tracked as work" — one verb covering two independent
/// questions. A request that was *both* (a specialist should do it **and** it
/// should be on the board) had no stated answer, and of the two tools the model
/// could reach for, the one described as actually doing the work
/// (`delegate_to_desk`) was the one that touched no board. So the work got done,
/// well, and nothing tracked it.
///
/// The brief now separates them, and — because a brief is guidance and guidance
/// is not a guarantee — the runtime no longer depends on the model getting the
/// second one right: a substantial hand-off opens its card in
/// [`DelegationRunner::run_delegation`](crate::runtime::delegation::DelegationRunner::run_delegation)
/// whichever tool was chosen. What the brief has to do now is stop the model
/// *double-tracking* (a `spawn_task` beside every hand-off), which is why it
/// tells it what `spawn_task` is still for.
///
/// # Answering leads, and the tool list stopped being the shape (issue #267)
///
/// The discrimination rule was already here — *"act on the board only when it
/// genuinely helps — otherwise answer directly and concisely"* — as the closing
/// clause of a brief that spent its length enumerating seven action tools and
/// how to use each. **The structure read as an invitation to act with a caveat
/// attached, and behaviour followed the structure rather than the caveat.** Six
/// cards sat unworked in `backlog` on a live company, and four of them were
/// asks to *create a workflow* that the orchestrator could have authored in the
/// turn it was asked.
///
/// So the default moved to the front and the enumeration was trimmed to fit
/// underneath it. Two things changed in substance rather than order:
///
/// * *A question about state is never a card* is now stated, not implied.
/// * `create_workflow` is framed as **something to do this turn**, not a
///   capability to mention. That is what un-deadens the four workflow cards'
///   class: Layer A still opens the `Track` card for "create a workflow named
///   X" — it *is* an instruction — but the orchestrator now completes it
///   instead of leaving it to rot.
///
/// This is guidance, and the two deterministic layers in
/// [`triage_message`](crate::company::task_intent::triage_message) and
/// `DelegationRunner::handle_operator_message` are what make the outcome not
/// depend on it.
///
/// # Budget
///
/// Persona-appended, so it sits OUTSIDE the issue-#417
/// [`TOOL_RESULT_BUDGET_BYTES`](crate::harness::build::TOOL_RESULT_BUDGET_BYTES)
/// insight-document budget and cannot crowd out the Desks section
/// `delegate_to_desk` depends on. Kept no longer than it already was regardless
/// — see `the_brief_leads_with_answering_and_did_not_grow`.
pub fn orchestrator_brief() -> String {
    " You are also this company's orchestrator: the single point of contact for the operator. \
MOST MESSAGES ARE QUESTIONS OR QUICK READS. Answer them from whole-company context and touch \
nothing else. A question about state — what is on the board, what workflows exist, who is on the \
team, what happened — is NEVER a card. Use `query_company`: it is the source of truth for the \
company's durable facts, recent activity, saved workflows, team roster and desks, so consult it \
before answering rather than guessing, then answer directly and concisely. A board write is the \
exception and needs a reason. \
When there IS work, two decisions come up and they are INDEPENDENT — do not collapse them into \
one. (1) WHO SHOULD DO THIS: when a request belongs to a specialist desk, hand it to that desk \
with `delegate_to_desk`, naming the desk by an id `query_company` lists under Desks (a desk is not \
a person: handing work to a teammate's name is not a delegation); when it is yours to answer, \
answer it. (2) SHOULD THIS BE TRACKED: you do not have to decide this, and you must not pick a \
tool in order to influence it. Anything substantial handed to a desk is opened as a board card \
automatically, and so is anything substantial an operator asks a desk or teammate directly — the \
hand-off IS the card, so never call `spawn_task` alongside a `delegate_to_desk` for the same work, \
and never prefer one over the other to get something tracked. Reach for `spawn_task` only for work \
that belongs on the board but must NOT start in this turn: something for later, for somebody else, \
or waiting on a person. \
WHEN YOU CAN DO THE WORK IN THIS TURN, DO IT — do not park it as a card for later. Asked to \
capture a repeatable process (\"create a workflow that…\"), author it NOW with `create_workflow` — \
a trigger plus agent / tool / condition / output steps — and say it is ready; it is enabled \
immediately and runnable. `run_workflow` executes a saved workflow by id, including to advance a \
task waiting on a run; you can run workflows yourself, so never claim that tool is unavailable. \
`add_agent` brings on a new teammate when the company genuinely needs one. \
You also own the board's lifecycle: `assign_task` sets who owns an existing card (ownership only — \
moving it to In Progress is what starts the work), and `review_task` records `approve` or `revise` \
on a card awaiting review."
        .to_string()
}

// ---------------------------------------------------------------------------
// Delegation queue
// ---------------------------------------------------------------------------

/// One unit of work the orchestrator hands off during a turn, drained by the
/// harness brain after the turn completes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delegation {
    /// Open a tracked task card on the company's board.
    SpawnTask {
        /// The task title.
        title: String,
        /// An optional longer note / brief.
        note: Option<String>,
        /// An optional assignee (a roster/desk id); empty when unassigned.
        assignee: Option<String>,
    },
    /// Hand a turn to a desk's lead member.
    DelegateToDesk {
        /// The desk id or name to delegate to.
        desk: String,
        /// The instruction handed to the desk's lead member.
        instruction: String,
    },
    /// Set (or change) who owns an existing board card (issue #186 part b).
    AssignTask {
        /// The card's id.
        task_id: String,
        /// The roster/desk id taking it on.
        assignee: String,
        /// An optional line recorded on the card explaining the assignment.
        note: Option<String>,
    },
    /// Record the orchestrator's verdict on a card in `in_review` (issue #186
    /// part b).
    ReviewTask {
        /// The card's id.
        task_id: String,
        /// The verdict.
        decision: ReviewDecision,
        /// An optional reviewer comment recorded on the card.
        note: Option<String>,
    },
}

impl Delegation {
    /// Whether this delegation is a way of **answering** the operator, rather
    /// than only a write to the board (issue #267).
    ///
    /// Only [`DelegateToDesk`](Self::DelegateToDesk) is. It runs a teammate's
    /// turn and hands their reply back for the orchestrator to relay, so it is
    /// how a question the orchestrator cannot answer alone reaches somebody who
    /// can — "what did the design desk ship this week?" is unanswerable without
    /// it. [`SpawnTask`](Self::SpawnTask), [`AssignTask`](Self::AssignTask) and
    /// [`ReviewTask`](Self::ReviewTask) change the board and return nothing to
    /// say, so they have no answering role and stay refused on a question turn.
    ///
    /// This is what [`DrainClaim::Answering`] filters on.
    pub fn answers(&self) -> bool {
        matches!(self, Self::DelegateToDesk { .. })
    }
}

/// A shared, in-memory queue the delegation tools push onto and the harness
/// brain drains. Cheap to [`Clone`] (a shared handle); the same underlying
/// queue is seen by the tools captured into the orchestrator agent and by the
/// brain that drains it, because [`HarnessDeps`](crate::harness::HarnessDeps)
/// clones share this handle.
///
/// # Why staging must be *claimed* (issue #453)
///
/// Staging has one failure mode and it is the worst kind: a caller for whom
/// **nothing drains**. `review_task` returned *"Approved card X; it is complete
/// and has moved to done"* the instant it pushed, and two production paths ran
/// a full toolbelt turn and never drained — the approval re-dispatch and the
/// workflow agent node. On those the sentence was false every time, and the
/// next turn's [`clear`](Self::clear) destroyed the work the operator had just
/// been told was done. A tool that cannot fail launders the failure through the
/// agent into a confident falsehood.
///
/// So the queue carries a **drain commitment** alongside the staged items, and
/// a drain site [`claim`](Self::claim)s it for the span in which it promises to
/// drain. The default is *uncommitted*, and that direction is the whole
/// guarantee: a turn run from a path that has not claimed — including one
/// written later, by someone who never read this — gets an honest in-turn
/// refusal instead of a receipt nothing will honour. Enforced by construction
/// rather than by remembering: *no claim, no delegation*.
///
/// This is [`PendingPublishQueue`](crate::harness::publish::PendingPublishQueue)'s
/// #445 shape, deliberately. The one difference is that there is nothing to name:
/// every drain site executes the same delegations the same way, so the
/// commitment is a boolean rather than a destination.
#[derive(Clone, Default)]
pub struct DelegationQueue {
    inner: Arc<Mutex<Vec<Delegation>>>,
    /// What the live claim on this queue permits (issues #453, #267).
    /// [`DrainClaim::Unclaimed`] — nothing drains — is the default and the
    /// fail-safe direction.
    committed: Arc<Mutex<DrainClaim>>,
    /// Desk keys a `delegate_to_desk` call named that the company does not have
    /// (issue #272).
    ///
    /// A refused hand-off never becomes a [`Delegation`], so without this the
    /// drain has no way to know one was attempted — and a dispatched card would
    /// settle under the delegator with only whatever the turn chose to say about
    /// it. Carried on the queue because it shares the queue's exact lifetime:
    /// filled by the tool during a turn, read by the drain right after, and
    /// wiped by the same [`clear`](Self::clear) that keeps a prior turn from
    /// leaking into this one.
    refused: Arc<Mutex<Vec<String>>>,
}

impl DelegationQueue {
    /// Enqueues a delegation **without regard for the per-turn cap**.
    ///
    /// Production callers want [`push_within_cap`](Self::push_within_cap)
    /// instead: this one can queue work the drain will later throw away, which
    /// is exactly the failure issue #419 is about. It bypasses the #453 drain
    /// commitment for the same reason it bypasses the cap — it is the escape
    /// hatch the tests that stand in for a turn use, and it is not reachable
    /// from any tool. Kept for the tests that deliberately over-fill the queue
    /// to prove the cap holds.
    pub fn push(&self, delegation: Delegation) {
        self.inner
            .lock()
            .expect("delegation queue")
            .push(delegation);
    }

    /// What the live claim on this queue permits (issues #453, #267).
    pub fn claim_state(&self) -> DrainClaim {
        *self.committed.lock().expect("delegation commitment")
    }

    /// Whether a drain site has committed to draining this queue (issue #453).
    ///
    /// True for **both** claim kinds: an answering claim drains exactly like a
    /// full one, it merely narrows what may be staged. Callers that need the
    /// distinction want [`claim_state`](Self::claim_state).
    pub fn drain_committed(&self) -> bool {
        self.claim_state() != DrainClaim::Unclaimed
    }

    /// Claims this queue for a drain site that promises to drain it, for as long
    /// as the returned [`DelegationClaim`] lives (issue #453).
    ///
    /// Clears on the way in for the reason the drain sites already cleared by
    /// hand — a prior turn's staged delegation must never be executed for this
    /// caller — and, via [`DelegationClaim`]'s `Drop`, on the way out too. The
    /// exit half is the one that is new and load-bearing: an early return, a
    /// `?`, or a panic mid-turn used to leave items staged for the next caller
    /// to clear, so correctness depended on every future path remembering. Now
    /// the claim's scope *is* the window in which delegating works.
    #[must_use = "the claim releases on drop; dropping it immediately un-claims the queue"]
    pub fn claim(&self) -> DelegationClaim {
        self.claim_as(DrainClaim::Full)
    }

    /// Claims this queue for a turn whose operator message triaged as a
    /// question (issue #267).
    ///
    /// Identical to [`claim`](Self::claim) in every way that matters to the
    /// drain — it runs, and it runs the same code — but only delegations that
    /// [`answer`](Delegation::answers) may be staged under it. The three pure
    /// board writes are refused at the tool boundary in the model's own turn.
    ///
    /// This exists because withholding the claim outright was too blunt: it
    /// took `delegate_to_desk` away too, and that tool is how a question the
    /// orchestrator cannot answer alone gets routed to a desk that can.
    #[must_use = "the claim releases on drop; dropping it immediately un-claims the queue"]
    pub fn claim_answering(&self) -> DelegationClaim {
        self.claim_as(DrainClaim::Answering)
    }

    /// The shared body of the two claim constructors.
    fn claim_as(&self, state: DrainClaim) -> DelegationClaim {
        self.clear();
        *self.committed.lock().expect("delegation commitment") = state;
        DelegationClaim {
            queue: self.clone(),
        }
    }

    /// Enqueues a delegation unless nothing will drain it, or `cap` are already
    /// queued for this turn (issues #419 and #453).
    ///
    /// # Why the cap is enforced *here*
    ///
    /// [`drain`](Self::drain) takes `min(cap)` and clears the rest, so anything
    /// past the cap was destroyed — while the tool that queued it had already
    /// told the model "it will be opened on the board this turn". Ask for five
    /// cards and the turn reports five; two exist. The bound was real and
    /// nothing announced it.
    ///
    /// Refusing at the boundary is the fix that keeps the model's own account
    /// of the turn honest: the tool call fails, in the model's turn, with the
    /// cap named — so it can tell the operator which items it did not get to
    /// instead of confidently reporting work that never happened.
    ///
    /// Refusal rather than carry-over is deliberate. Carrying overflow into the
    /// next turn would mean the queue outliving the turn that filled it, and the
    /// queue is [`clear`](Self::clear)ed before every turn precisely so a stale
    /// delegation cannot leak into one — including into a CEO-relay turn, which
    /// is forbidden to delegate at all.
    ///
    /// # Why the commitment is checked *first*
    ///
    /// When nothing will drain, that is the only fact that matters: reporting
    /// the cap instead would tell the model to try again next turn, and the next
    /// turn on that path drains no better than this one. The two refusals are
    /// therefore distinct [`Staged`] variants and never collapsed.
    #[must_use = "a refused delegation must be reported to the model, not dropped"]
    pub fn push_within_cap(&self, delegation: Delegation, cap: usize) -> Staged {
        match self.claim_state() {
            DrainClaim::Unclaimed => return Staged::NoDrain(NoDrainReason::Unwired),
            // Issue #267: the operator asked a question. A hand-off is how one
            // gets answered, so it stages; the pure board writes do not.
            DrainClaim::Answering if !delegation.answers() => {
                return Staged::NoDrain(NoDrainReason::Triage);
            }
            DrainClaim::Answering | DrainClaim::Full => {}
        }
        let mut guard = self.inner.lock().expect("delegation queue");
        if guard.len() >= cap {
            return Staged::OverCap;
        }
        guard.push(delegation);
        Staged::Queued
    }

    /// Records that a hand-off named `desk`, which the company cannot hand work
    /// to, so the drain can report the attempt (issue #272).
    pub fn push_refusal(&self, desk: String) {
        self.refused.lock().expect("delegation queue").push(desk);
    }

    /// Drains up to `cap` refused desk keys (FIFO) and discards the rest, so a
    /// turn that calls the tool repeatedly cannot grow an unbounded note.
    ///
    /// A discard is logged rather than silent (issue #419) — the note it would
    /// have grown is the operator's only record that a hand-off was attempted.
    pub fn drain_refusals(&self, cap: usize) -> Vec<String> {
        let mut guard = self.refused.lock().expect("delegation queue");
        let take = guard.len().min(cap);
        let dropped = guard.len() - take;
        let drained: Vec<String> = guard.drain(..take).collect();
        if dropped > 0 {
            tracing::warn!(
                dropped,
                cap,
                "[delegation] discarded refused hand-offs past the per-turn cap; they will not be \
                 recorded on the card"
            );
        }
        guard.clear();
        drained
    }

    /// Empties the queue (called before an orchestrator turn so stale
    /// delegations from a prior turn never leak into this one).
    pub fn clear(&self) {
        self.inner.lock().expect("delegation queue").clear();
        self.refused.lock().expect("delegation queue").clear();
    }

    /// Drains up to `cap` queued delegations (FIFO) and discards the rest, so a
    /// single turn can never fan out past the cap.
    ///
    /// Since issue #419 the discard should be unreachable from the tool path —
    /// [`push_within_cap`](Self::push_within_cap) refuses at the boundary, so
    /// the queue never grows past `cap`. It is still counted and **logged**
    /// rather than dropped in silence, because a queue that arrives here over
    /// the cap means some caller bypassed that boundary and is quietly losing
    /// work the model already claimed it had done.
    pub fn drain(&self, cap: usize) -> Vec<Delegation> {
        let mut guard = self.inner.lock().expect("delegation queue");
        let take = guard.len().min(cap);
        let dropped = guard.len() - take;
        let drained: Vec<Delegation> = guard.drain(..take).collect();
        if dropped > 0 {
            tracing::warn!(
                dropped,
                cap,
                "[delegation] discarding queued delegations past the per-turn cap — the tool \
                 boundary should have refused these before they were queued"
            );
        }
        guard.clear();
        drained
    }

    /// The number of queued delegations (test/observability).
    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.inner.lock().expect("delegation queue").len()
    }
}

/// What happened when a tool offered a delegation to the queue (issues #419,
/// #453).
///
/// Three outcomes rather than a `bool`, because the two refusals need different
/// sentences: one says *this turn is full, raise it next turn*, and the other
/// says *this context cannot do board work at all, do not retry*. A model told
/// the wrong one either burns its next turn on a call that will fail identically
/// or gives up on work it could still have queued.
#[must_use = "a delegation that was not queued must be reported to the model, not dropped"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staged {
    /// Queued; some drain site will execute it as this turn completes.
    Queued,
    /// Nothing that would execute *this* delegation has claimed the queue. The
    /// [`NoDrainReason`] says which of the two very different causes it was.
    NoDrain(NoDrainReason),
    /// This turn has already queued [`MAX_DELEGATIONS_PER_TURN`].
    OverCap,
}

/// Why a delegation found nothing that would drain it (issues #453, #267).
///
/// One refusal used to speak for both of these, and they are not the same
/// condition: one is a context that can never do board work, the other is a
/// fully capable company reading *this message* as a question. Sharing a
/// sentence made the refusal wrong for the second case — "board actions are
/// unavailable in this context" is false when they would have worked on a
/// differently-phrased message — and, worse, made the two indistinguishable in
/// the logs, so the rate at which the triage gate fires could not be measured
/// after shipping a keyword classifier with teeth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoDrainReason {
    /// No drain site claimed the queue at all ([`DrainClaim::Unclaimed`]):
    /// nothing in this context can carry out board work, for any message.
    Unwired,
    /// The queue is claimed for answering ([`DrainClaim::Answering`]): the
    /// operator's message triaged as a question, so board writes are held back
    /// for **this message only** (issue #267).
    Triage,
}

impl NoDrainReason {
    /// The value the `reason` log field carries, so the two causes can be
    /// counted apart in production.
    ///
    /// `drain_unwired` rather than the `no_drain_wired` this shipped with: the
    /// old value parsed just as readily as "a no-drain **was** wired", the
    /// opposite of what it records, and this label is the field the whole
    /// countability argument rests on (issue #267 review).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unwired => "drain_unwired",
            Self::Triage => "triaged_as_question",
        }
    }
}

impl std::fmt::Display for NoDrainReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the live claim on a [`DelegationQueue`] permits (issues #453, #267).
///
/// The gate on a question turn is a *narrowing* rather than a withdrawal, and
/// this is where the difference lives. Before #267's review the answering case
/// was expressed by simply not claiming, which could only say "no board work at
/// all" — and that took `delegate_to_desk` with it, leaving the orchestrator
/// unable to consult a desk about the very question it was asked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DrainClaim {
    /// No drain site has claimed the queue: nothing may be staged, because
    /// nothing would ever execute it. The default and the fail-safe direction.
    #[default]
    Unclaimed,
    /// A drain site has claimed the queue and will execute anything staged.
    Full,
    /// A drain site has claimed the queue for a turn whose operator message
    /// triaged as [`MessageTriage::Answer`](crate::company::task_intent::MessageTriage)
    /// (issue #267). The drain runs exactly as under [`Full`](Self::Full); only
    /// delegations that [`answer`](Delegation::answers) may be staged.
    Answering,
}

/// The live claim on a [`DelegationQueue`] — proof that some drain site is
/// listening (issue #453).
///
/// Held for the span in which a caller promises to drain; on `Drop` the queue is
/// emptied and returns to uncommitted, so delegating is off again the moment
/// that promise ends. Mirrors
/// [`PublishClaim`](crate::harness::publish::PublishClaim), and the in-flight
/// steer guard before it, for the same reason: the cleanup has to happen on
/// **every** exit path, including the ones nobody wrote by hand.
///
/// Deliberately not [`Clone`] — two live claims would mean two owners of one
/// promise, and the second to drop would un-claim the queue underneath the
/// first.
pub struct DelegationClaim {
    queue: DelegationQueue,
}

impl Drop for DelegationClaim {
    fn drop(&mut self) {
        *self.queue.committed.lock().expect("delegation commitment") = DrainClaim::Unclaimed;
        self.queue.clear();
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// A read surface over the company's durable facts and recent event history, so
/// the orchestrator can ground its answers in whole-company context.
pub struct QueryCompanyTool {
    company: CompanyId,
    facts: Option<Arc<dyn FactStore>>,
    events: Option<Arc<dyn EventLog>>,
    /// The company's source directory, so the tool can enumerate saved
    /// `workflows/*.toml` graphs — the same on-disk list the REST picker reads.
    /// `None` in platform-provisioned mode (nothing on disk to scan).
    workflow_source_dir: Option<PathBuf>,
    /// The company store, so the tool can read the persisted roster (manifest
    /// agents + operator-added overlay teammates) and the manifest's enabled
    /// workflow ids. `None` on builds with no store wired.
    store: Option<Arc<dyn CompanyStore>>,
}

impl QueryCompanyTool {
    /// Builds the tool over the company's read ports. Any handle may be `None`;
    /// the tool reports whatever surface is wired.
    pub fn new(
        company: CompanyId,
        facts: Option<Arc<dyn FactStore>>,
        events: Option<Arc<dyn EventLog>>,
        workflow_source_dir: Option<PathBuf>,
        store: Option<Arc<dyn CompanyStore>>,
    ) -> Self {
        Self {
            company,
            facts,
            events,
            workflow_source_dir,
            store,
        }
    }
}

#[async_trait]
impl Tool for QueryCompanyTool {
    fn name(&self) -> &str {
        QUERY_COMPANY_TOOL
    }

    fn description(&self) -> &str {
        "Read the company's durable facts, recent activity, saved workflows, team roster, and desks to ground an answer in whole-company context — use this to answer \"what workflows do we have?\", \"who is on the team?\", or \"which desks can take work?\" instead of guessing, and to get the exact desk id `delegate_to_desk` needs. Optionally pass a `query` to filter facts by a case-insensitive substring."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional case-insensitive substring to filter facts by."
                }
            },
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn supports_markdown(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let query = args.get("query").and_then(Value::as_str).map(str::trim);
        let query = query.filter(|q| !q.is_empty());

        let facts = match &self.facts {
            Some(store) => store
                .list(&self.company, query, None)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };

        // Recent events: read the log and keep the tail. Mirrors the GraphQL
        // history resolver's read-then-tail pattern (`read_from(0, MAX)`).
        //
        // Discussion posts (#335) do not take a slot each. The tail is ten
        // events wide and a card's discussion is an operator-driven,
        // high-frequency writer into this same log, so a row per post would let
        // one afternoon's thread evict every dispatch, reply and approval from
        // the orchestrator's whole view of the company — replacing rows it can
        // act on with ten it cannot ("agents do not participate" holds for the
        // text, and must hold for the slot too). They fold into a single count
        // line instead: the orchestrator learns that people are talking on the
        // cards without losing what the company *did*.
        let stored = match &self.events {
            Some(log) => log
                .read_from(&self.company, EventSeq::new(0), usize::MAX)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let mut recent: Vec<String> = Vec::new();
        let mut discussion_posts = 0usize;
        // Events actually visited before the tail filled. Anything past this is
        // strictly older than everything shown (we walk newest-first), so it is
        // the exact count of activity dropped off the far end.
        let mut consumed = 0usize;
        for event in stored.iter().rev() {
            if recent.len() == RECENT_EVENTS {
                break;
            }
            consumed += 1;
            if matches!(event.event, CompanyEvent::TaskDiscussionPosted { .. }) {
                // Counted over the same span the tail covers, not over all of
                // history: this line reads as "recent activity" like the rows
                // beside it.
                discussion_posts += 1;
                continue;
            }
            recent.push(format!(
                "- #{} {}",
                event.seq,
                summarize_event(&event.event)
            ));
        }
        // Older events the tail could not reach. The facts section one block
        // down already announces its own cut (issue #410); this is the same
        // silent-cut class on the activity tail — a full log handed the
        // orchestrator only its last ten rows and read as complete. `stored`
        // holds the whole log, so the drop is exactly countable. No remediation
        // clause: `query_company` has no pagination argument to point at.
        let older = stored.len() - consumed;
        recent.reverse(); // back to chronological order
        if older > 0 {
            // Dropped rows are older than everything below; the list renders
            // oldest→newest, so the notice belongs at the top.
            recent.insert(0, format!("- […{older} earlier event(s) not shown]"));
        }
        if discussion_posts > 0 {
            let plural = if discussion_posts == 1 { "" } else { "s" };
            recent.push(format!(
                "- {discussion_posts} discussion post{plural} on task cards (text not shown)"
            ));
        }

        let mut md = String::from("# Company insight\n");
        md.push_str("\n## Facts\n");
        if facts.is_empty() {
            md.push_str("_No durable facts recorded._\n");
        } else {
            // Two bounds, so the facts section can never be the thing that
            // pushes the outer tool-result cut into the sections below it
            // (issue #420): each body is capped at MAX_FACT_BODY_CHARS, and the
            // section as a whole stops once its rendered bytes reach
            // FACTS_SECTION_BUDGET_BYTES. The budget is charged in bytes because
            // the outer cut is bytes; the body cut counts characters so it can
            // never split a codepoint.
            let mut shown = 0usize;
            let mut section_bytes = 0usize;
            for fact in facts.iter().take(FACT_LIMIT) {
                let line = format!(
                    "- **{}**: {}\n",
                    fact.title.trim(),
                    truncate_chars(fact.body.trim(), MAX_FACT_BODY_CHARS)
                );
                // Always render at least one fact; past that, stop before a line
                // would carry the section over its byte budget.
                if shown > 0 && section_bytes + line.len() > FACTS_SECTION_BUDGET_BYTES {
                    break;
                }
                section_bytes += line.len();
                md.push_str(&line);
                shown += 1;
            }
            // Issue #410, the same silent-cut class one tool over: this list was
            // capped at FACT_LIMIT with no marker, so a company past twenty facts
            // handed the orchestrator a partial memory that read as complete —
            // and the narrowing argument that would have fixed it (`query`) was
            // never mentioned at the point the cut happened. The count now covers
            // both the FACT_LIMIT cap and the byte-budget cut (issue #420); the
            // marker line itself is charged outside the budget so the
            // announcement can never be the fact that gets squeezed out.
            if shown < facts.len() {
                md.push_str(&format!(
                    "\n[TRUNCATED — {} more fact(s) not shown. This is NOT the whole record. \
                     Narrow it with `{QUERY_COMPANY_TOOL}({{\"query\": \"<substring>\"}})` before \
                     concluding a fact is absent.]\n",
                    facts.len() - shown
                ));
            }
        }
        md.push_str("\n## Recent activity\n");
        if recent.is_empty() {
            md.push_str("_No recent activity._\n");
        } else {
            md.push_str(&recent.join("\n"));
            md.push('\n');
        }

        // Load the persisted record once: it carries both the roster and the
        // manifest's enabled workflow ids (the seed workflows that have no file
        // under `workflows/`). `None`/error → those sections read empty rather
        // than failing the whole surface.
        let record = match &self.store {
            Some(store) => store.load(&self.company).await.ok().flatten(),
            None => None,
        };

        // Saved workflows: the seed `workflows/*.toml` graphs unioned with the
        // record's runtime-authored bodies (what `create_workflow` persists and
        // the REST picker lists), then with the manifest's enabled ids so
        // provisioned-but-bodiless workflows show too. This is the section
        // that makes "what workflows do we have?" answerable — before it, the
        // orchestrator had no way to enumerate saved workflows and would fall
        // back to its skills catalog.
        let overlay_workflows = record
            .as_ref()
            .map(|r| r.overlay_workflows.clone())
            .unwrap_or_default();
        let mut workflows: Vec<(String, String)> =
            list_workflows_union(self.workflow_source_dir.as_deref(), &overlay_workflows)
                .into_iter()
                .map(|f| (f.id, f.name))
                .collect();
        let mut seen: std::collections::HashSet<String> =
            workflows.iter().map(|(id, _)| id.clone()).collect();
        if let Some(record) = &record {
            for id in &record.manifest.workflows.enabled {
                if seen.insert(id.clone()) {
                    workflows.push((id.clone(), id.clone()));
                }
            }
        }
        workflows.sort_by(|a, b| a.0.cmp(&b.0));
        md.push_str("\n## Saved workflows\n");
        if workflows.is_empty() {
            md.push_str("_No saved workflows. Author one with `create_workflow`._\n");
        } else {
            for (id, name) in &workflows {
                md.push_str(&format!(
                    "- **{}** (`{}`) — run with `run_workflow`\n",
                    name.trim(),
                    id
                ));
            }
        }

        // Team roster: manifest agents plus operator-added overlay teammates
        // (the ones `add_agent` persists), so a freshly added teammate is
        // visible on the next query instead of looking unpersisted.
        let mut roster: Vec<(String, String)> = Vec::new();
        if let Some(record) = &record {
            for agent in &record.manifest.agents {
                roster.push((agent.id.clone(), agent.role.clone()));
            }
            for overlay in &record.overlay_agents {
                roster.push((overlay.name.clone(), overlay.role.clone()));
            }
        }
        md.push_str("\n## Team\n");
        if roster.is_empty() {
            md.push_str("_Roster unavailable._\n");
        } else {
            for (id, role) in &roster {
                md.push_str(&format!("- **{}** — {}\n", id, role.trim()));
            }
        }

        // Desks (issue #272). The roster was already here, but the *desks* were
        // not — so an orchestrator asked to hand work to a desk had nothing
        // authoritative to read and reached for a teammate's id instead. These
        // are exactly the ids `delegate_to_desk` accepts, with each desk's lead
        // named so the two are never confused for one another again.
        let desks: Vec<(String, Option<String>)> = record
            .as_ref()
            .map(|record| {
                delegation_tools::desk_ids(record)
                    .into_iter()
                    .map(|id| {
                        let lead = delegation_tools::desk_lead(record, &id);
                        (id, lead)
                    })
                    .collect()
            })
            .unwrap_or_default();
        md.push_str("\n## Desks\n");
        if desks.is_empty() {
            md.push_str("_No desks. Answer directly rather than delegating._\n");
        } else {
            for (id, lead) in &desks {
                match lead {
                    Some(lead) => md.push_str(&format!(
                        "- **{id}** — lead: {lead} (delegate with `delegate_to_desk` desk=`{id}`)\n"
                    )),
                    None => md.push_str(&format!(
                        "- **{id}** — no member on the roster, so it cannot be handed work\n"
                    )),
                }
            }
        }

        Ok(ToolResult::success_with_markdown(
            json!({
                "facts": facts.len(),
                "recent_events": recent.len(),
                "events_not_shown": older,
                "workflows": workflows.len(),
                "team": roster.len(),
                "desks": desks.len(),
            }),
            md,
        ))
    }
}

/// Cut `s` to at most `max` characters, marking a cut with a trailing ellipsis.
///
/// Sibling of [`memory_loop::truncate_chars`](crate::harness::memory_loop) —
/// kept a private copy here rather than coupled to it, since the two callers
/// share only the shape, not a contract. The ellipsis is budgeted *inside*
/// `max`: taking `max` characters and then appending would return `max + 1` and
/// quietly exceed the cap it advertises. Cutting counts characters, never
/// bytes, so a multibyte body can never be split mid-codepoint.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let head: String = s.chars().take(max - 1).collect();
    format!("{head}…")
}

/// A short, non-sensitive one-line summary of an event for the insight surface.
fn summarize_event(event: &CompanyEvent) -> String {
    match event {
        CompanyEvent::OperatorMessage { .. } => "operator message".to_string(),
        CompanyEvent::AgentReply { agent_id, .. } => format!("reply from {agent_id}"),
        CompanyEvent::TaskDispatched { task_id, .. } => format!("task dispatched: {task_id}"),
        // Issue #464. Structural only, like every arm here: the id, the change
        // word (fixed vocabulary) and the column (one of six). The card's title
        // is deliberately not named — it is operator- or agent-authored free
        // text, and this string is a non-sensitive one-liner for the insight
        // surface, which is exactly where free text does not belong.
        CompanyEvent::TaskCardChanged {
            task_id,
            change,
            column,
        } => match column {
            Some(column) => format!("card {change}: {task_id} → {column}"),
            None => format!("card {change}: {task_id}"),
        },
        // Issue #327. Structural only, like the board arm above: the id and the
        // change word, from a fixed vocabulary. The node's NAME is deliberately
        // absent — it is operator-authored free text, and this string is a
        // non-sensitive one-liner for the insight surface, which is exactly
        // where free text does not belong.
        CompanyEvent::WorkspaceChanged { node_id, change } => {
            format!("workspace {change}: {node_id}")
        }
        CompanyEvent::ScheduleFired { cron, .. } => format!("schedule fired: {cron}"),
        CompanyEvent::WebhookReceived { channel, .. } => format!("webhook on {channel}"),
        CompanyEvent::A2aTaskReceived { from, .. } => format!("A2A task from {from}"),
        // The tool this resolved is deliberately NOT named here, though it would
        // read better: `ApprovalResolved` carries only the id, the verdict and
        // the actor, so naming the tool would mean threading a journal lookup
        // into what is a pure function over one event — real coupling for a
        // non-load-bearing insight string. The id is carried instead, which is
        // enough to correlate against the approvals surface and costs nothing.
        // Issue #379. Same reasoning as the resolution arm below: the id, and
        // only the id, plus the effect's dotted kind — which is a type name, not
        // a payload. The thread it was raised in is deliberately not named: a
        // thread id is a desk or a roster agent, and this string is a
        // non-sensitive one-liner, not a routing surface.
        CompanyEvent::ApprovalParked {
            approval_id,
            effect_kind,
            ..
        } => format!("approval {approval_id} parked ({effect_kind})"),
        CompanyEvent::ApprovalResolved {
            approval_id,
            verdict,
            ..
        } => format!("approval {approval_id} {verdict:?}"),
        CompanyEvent::FeedbackFiled { .. } => "feedback filed".to_string(),
        CompanyEvent::PaymentReceived { amount_usd, .. } => format!("payment ${amount_usd:.2}"),
        CompanyEvent::LifecycleChanged { from, to, .. } => format!("lifecycle {from} → {to}"),
        // Issue #86. Structural only, on exactly the terms the arms around it
        // set: the engaged/released word is fixed vocabulary, and BOTH of the
        // event's other fields are dropped. `reason` is the operator's free-text
        // incident note — the single most sensitive thing on this event and the
        // clearest example of what an insight one-liner must not quote — and
        // `by` is a user id, dropped for the same reason every arm here drops
        // it. That the company was stopped is the insight; who typed what about
        // it is not.
        CompanyEvent::EmergencyPauseChanged { engaged, .. } => format!(
            "emergency stop {}",
            if *engaged { "engaged" } else { "released" }
        ),
        CompanyEvent::MemoryFactDeleted { .. } => "memory fact deleted".to_string(),
        // Issue #364. The emoji is carried — a reaction with the emoji taken out
        // says nothing at all — and it is the one part of a reaction that cannot
        // be free text: the route bounds it and refuses control characters. The
        // message it is about is named by sequence position, which is an
        // ordinal, and `by` is dropped for the same reason every arm here drops
        // it: a user id is neither one-line-worthy nor insight.
        CompanyEvent::ReactionToggled {
            message_seq,
            emoji,
            on,
            ..
        } => {
            let verb = if *on { "reacted" } else { "un-reacted" };
            format!("{verb} {emoji} on message {message_seq}")
        }
        // Issue #403. The change word and the toolkit slug are both fixed
        // vocabulary, so neither can carry anything a company typed. `by` is
        // dropped: this is a non-sensitive one-liner for the insight surface,
        // and a user id is neither one-line-worthy nor insight.
        CompanyEvent::ToolAccessChanged {
            change, toolkit, ..
        } => match toolkit {
            Some(toolkit) => format!("tool access changed: {change} ({toolkit})"),
            None => format!("tool access changed: {change}"),
        },
        CompanyEvent::McpCallFailed { server, tool, .. } => {
            format!("MCP call failed: {server}/{tool}")
        }
        CompanyEvent::WorkflowCreated {
            workflow_id, name, ..
        } => format!("workflow created: {name} ({workflow_id})"),
        CompanyEvent::WorkflowUpdated {
            workflow_id, name, ..
        } => format!("workflow updated: {name} ({workflow_id})"),
        CompanyEvent::WorkflowDeleted {
            workflow_id, name, ..
        } => format!("workflow deleted: {name} ({workflow_id})"),
        // Issue #276. This one-liner is folded into the orchestrator's
        // recent-activity context, so it is read by a model — and the arms
        // around it drop free text and actor ids for that reason. Name and id
        // only, exactly like the create/update/delete arms above.
        //
        // `by` and `reason` are both dropped. `by` is an actor id, per the rule
        // this whole function follows. `reason` is dropped on a different
        // ground: whether the host's disarm rule or a person flipped the switch
        // is a fact about our write path, and an orchestrator reasoning about
        // "the host refused to arm this" would be reasoning about plumbing. The
        // operator-facing answer to that question is the journal and the SSE
        // frame, which do carry it.
        CompanyEvent::WorkflowEnabledChanged {
            workflow_id,
            name,
            enabled,
            ..
        } => format!(
            "workflow {}: {name} ({workflow_id})",
            if *enabled {
                "switched on"
            } else {
                "switched off"
            }
        ),
        CompanyEvent::TaskSteered {
            task_id, action, ..
        } => format!("task steered ({action}): {task_id}"),
        CompanyEvent::DeskTaskCompleted {
            task_id, column, ..
        } => format!("task completed ({column}): {task_id}"),
        // A human posted on a card (#335). The card is named; the message text
        // is not — a discussion post is operator free text that no agent
        // consumes in v1, and quoting it here would route it into a turn through
        // the back door. The insight tail folds these into a single count line
        // rather than calling this arm (they must not each hold one of ten
        // slots); the arm stays because the match is exhaustive and because a
        // future caller must inherit the no-quoting rule, not re-decide it.
        CompanyEvent::TaskDiscussionPosted { task_id, .. } => {
            format!("discussion post on task {task_id}")
        }
        // Issue #358. Structural, like its neighbour and for a sharper reason:
        // the operator has just said that message should stop being readable,
        // so an insight line quoting anything about it would undo the act it
        // reports. The event carries no text to quote in any case.
        CompanyEvent::TaskDiscussionRedacted { task_id, .. } => {
            format!("discussion message removed on task {task_id}")
        }
        // A finished workflow run (#228). Counts only — never a delivery row's
        // `target` (a recipient's email address) or its `detail`, which can
        // quote one. This string is a non-sensitive one-liner for the insight
        // surface; the operator reads the full rows in the console.
        CompanyEvent::WorkflowRunFinished {
            workflow_id,
            scheduled,
            deliveries,
            error,
            cancelled,
            ..
        } => {
            let how = if *scheduled { "scheduled" } else { "manual" };
            match error {
                Some(_) => format!("{how} workflow run failed: {workflow_id}"),
                // Issue #383, same reasoning as the sidecar's projection in
                // `brain::medulla::effects`: a cancelled run has no error, so it
                // would otherwise read to the orchestrator as a clean finish and
                // invite it to act on work an operator deliberately stopped.
                None if *cancelled => format!("{how} workflow run stopped: {workflow_id}"),
                None => {
                    let undelivered = deliveries
                        .iter()
                        .filter(|d| !matches!(d.status, crate::ports::DeliveryStatus::Sent))
                        .count();
                    format!(
                        "{how} workflow run finished: {workflow_id} ({undelivered} not delivered)"
                    )
                }
            }
        }
        // Issue #371's progress trail. Summarized, not surfaced: the insight
        // tail has ten slots and a six-node run would take eight of them to say
        // things the run's own finished line already says better. The arms exist
        // because the match is exhaustive, and they hold the same no-payload
        // rule as their neighbours — node ids and durations only, which is all
        // these events carry.
        CompanyEvent::WorkflowRunStarted { workflow_id, .. } => {
            format!("workflow run started: {workflow_id}")
        }
        // Issue #382: the per-node start bracket. Same summarized, no-payload
        // rule as its sibling arms — a node id only, never any input.
        CompanyEvent::WorkflowNodeStarted {
            workflow_id,
            node_id,
            ..
        } => format!("workflow {workflow_id} started node {node_id}"),
        CompanyEvent::WorkflowNodeFinished {
            workflow_id,
            node_id,
            ..
        } => format!("workflow {workflow_id} finished node {node_id}"),
        // Issue #529: a delivered-report record. Same no-payload rule as its
        // neighbours — the node and the destination kind, never the target
        // address, which is operator-only. The run's own finished line already
        // folds the delivery counts, so this is summarized, not surfaced.
        CompanyEvent::WorkflowReportDelivered {
            workflow_id,
            node,
            kind,
            ..
        } => format!("workflow {workflow_id} delivered {kind} report from node {node}"),
        // Issue #617. Structural only, and without the policy's `reason` for
        // the same rule the arms above follow.
        CompanyEvent::WorkflowChildCallNotOffered {
            child_workflow_id,
            node,
            tool,
            ..
        } => format!("workflow child {child_workflow_id} ran {tool} at node {node} unapproved"),
    }
}

/// A delegation tool that opens a tracked task card. Enqueues a
/// [`Delegation::SpawnTask`]; the harness brain writes the card on drain.
pub struct SpawnTaskTool {
    queue: DelegationQueue,
}

impl SpawnTaskTool {
    /// Builds the tool over the shared delegation queue.
    pub fn new(queue: DelegationQueue) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl Tool for SpawnTaskTool {
    fn name(&self) -> &str {
        SPAWN_TASK_TOOL
    }

    fn description(&self) -> &str {
        "Open a task card on the company's board for work that should NOT start in this turn — something for later, for somebody else, or waiting on a person. Provide a `title`, an optional `note` brief, and an optional `assignee` (a desk or teammate id). Do NOT use this to get a hand-off tracked: work you hand to a desk with `delegate_to_desk` already opens its own card, and calling both for the same work opens two."
    }

    fn parameters_schema(&self) -> Value {
        crate::runtime::delegation_tools::spawn_task_schema()
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`title` is required"))?
            .to_string();
        let note = args
            .get("note")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        let assignee = args
            .get("assignee")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string);

        let effect = format!("the card \"{title}\" was NOT opened");
        match self.queue.push_within_cap(
            Delegation::SpawnTask {
                title: title.clone(),
                note,
                assignee,
            },
            MAX_DELEGATIONS_PER_TURN,
        ) {
            Staged::Queued => {}
            Staged::OverCap => return Ok(ToolResult::error(over_cap(&effect))),
            Staged::NoDrain(why) => {
                return Ok(ToolResult::error(no_drain(SPAWN_TASK_TOOL, &effect, why)));
            }
        }
        Ok(ToolResult::success(format!(
            "Queued a task card: \"{title}\". It will be opened on the board this turn."
        )))
    }
}

/// A delegation tool that hands a turn to a desk's lead member. Enqueues a
/// [`Delegation::DelegateToDesk`]; the harness brain runs the desk turn on
/// drain and surfaces its reply as its own chat bubble.
///
/// The target is **grounded against the company's real desks** before anything
/// is queued (issue #272): a `desk` that matches no desk — or a desk nobody on
/// the roster leads, which no turn could ever run for — is refused here, with
/// the valid desk ids in the refusal, instead of being queued and quietly
/// dropped at drain time. `delegate_to_desk` is an
/// [intrinsic tool](crate::harness::steps), so the refusal reaches both the
/// model (as a failed tool result it can retry from in the same turn) and the
/// operator's run trail verbatim.
pub struct DelegateToDeskTool {
    queue: DelegationQueue,
    company: CompanyId,
    /// The company store, read at call time so the desk set is the **current**
    /// one. Deliberately not a snapshot captured when the agent was built: an
    /// operator can create a desk mid-session (the desk-creation overlay), and a
    /// stale snapshot would refuse a desk that exists — a worse failure than the
    /// one this grounding fixes.
    store: Arc<dyn CompanyStore>,
}

impl DelegateToDeskTool {
    /// Builds the tool over the shared delegation queue and the company store it
    /// grounds the target against.
    pub fn new(queue: DelegationQueue, company: CompanyId, store: Arc<dyn CompanyStore>) -> Self {
        Self {
            queue,
            company,
            store,
        }
    }

    /// The refusal for `desk`, or `None` when it names a desk that can take
    /// work.
    ///
    /// **Fails open**: if the record cannot be read the delegation is queued
    /// exactly as it was before this grounding existed. A store hiccup must not
    /// take delegation offline; the drain-time fall-through still records what
    /// happened.
    async fn refusal(&self, desk: &str) -> Option<String> {
        match self.store.load(&self.company).await {
            Ok(Some(record)) => delegation_tools::reject_desk_target(&record, desk),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(
                    company = %self.company,
                    error = %err,
                    "[delegate_to_desk] could not read the company record to ground the desk target; \
                     queuing the hand-off ungrounded"
                );
                None
            }
        }
    }
}

#[async_trait]
impl Tool for DelegateToDeskTool {
    fn name(&self) -> &str {
        DELEGATE_TO_DESK_TOOL
    }

    fn description(&self) -> &str {
        "Hand a turn to a desk's lead member so a specialist answers. Provide the `desk` (its id or name) and the `instruction` to carry out. A substantial hand-off is opened as a tracked board card automatically, assigned to that lead — you do not need to call `spawn_task` as well."
    }

    fn parameters_schema(&self) -> Value {
        crate::runtime::delegation_tools::delegate_to_desk_schema()
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let desk = args
            .get("desk")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`desk` is required"))?
            .to_string();
        let instruction = args
            .get("instruction")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`instruction` is required"))?
            .to_string();

        // Ground the target before queuing anything: an invented desk is
        // refused here, in the model's own turn, rather than surviving as a
        // queued hand-off that the drain silently cannot deliver (issue #272).
        if let Some(refusal) = self.refusal(&desk).await {
            tracing::info!(
                company = %self.company,
                "[delegate_to_desk] refused an ungrounded delegation target"
            );
            // Recorded as well as returned: the model can still describe this
            // turn however it likes, so the *board* must carry the fact
            // independently of what the turn says about it.
            self.queue.push_refusal(desk);
            return Ok(ToolResult::error(refusal));
        }

        let effect = format!("nothing was handed to the {desk} desk");
        match self.queue.push_within_cap(
            Delegation::DelegateToDesk {
                desk: desk.clone(),
                instruction,
            },
            MAX_DELEGATIONS_PER_TURN,
        ) {
            Staged::Queued => {}
            Staged::OverCap => return Ok(ToolResult::error(over_cap(&effect))),
            Staged::NoDrain(why) => {
                return Ok(ToolResult::error(no_drain(
                    DELEGATE_TO_DESK_TOOL,
                    &effect,
                    why,
                )));
            }
        }
        Ok(ToolResult::success(format!(
            "Delegated to the {desk} desk. Its lead will answer this turn."
        )))
    }
}

// ---------------------------------------------------------------------------
// assign_task / review_task (issue #186 part b — orchestrator lifecycle authority)
// ---------------------------------------------------------------------------

/// A lifecycle tool that (re)assigns an existing board card.
///
/// Part (a) of #186 gave the orchestrator the *reply* and put the column
/// decisions behind a seam; this is the half that makes the authority real —
/// the orchestrator can now decide who owns a card, rather than assignment
/// being fixed at the moment the card was opened.
///
/// **It does not (re)dispatch.** Dispatch fires from
/// `CompanyRuntime::upsert_task`, which is reached by the console's
/// `column → in_progress` PATCH; the delegation queue drains through the
/// [`TaskStore`](crate::ports::TaskStore) port instead, which deliberately has
/// no runtime handle. Assigning a card therefore sets its owner and leaves
/// dispatch to that existing path — reaching the runtime from a tool would be a
/// layering change well outside this issue.
pub struct AssignTaskTool {
    queue: DelegationQueue,
}

impl AssignTaskTool {
    /// Builds the tool over the shared delegation queue.
    pub fn new(queue: DelegationQueue) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl Tool for AssignTaskTool {
    fn name(&self) -> &str {
        ASSIGN_TASK_TOOL
    }

    fn description(&self) -> &str {
        "Set or change who owns an existing task card on the company's board. Provide the card's `task_id`, the `assignee` (a desk or teammate id), and an optional `note` explaining the assignment. This records ownership; it does not start the work — move the card to In Progress to dispatch it."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The id of the card to assign." },
                "assignee": { "type": "string", "description": "The desk/teammate id taking it on." },
                "note": { "type": "string", "description": "An optional line explaining the assignment." }
            },
            "required": ["task_id", "assignee"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let task_id = required_str(&args, "task_id")?;
        let assignee = required_str(&args, "assignee")?;
        let note = optional_str(&args, "note");

        let effect = format!("card {task_id} was NOT assigned");
        match self.queue.push_within_cap(
            Delegation::AssignTask {
                task_id: task_id.clone(),
                assignee: assignee.clone(),
                note,
            },
            MAX_DELEGATIONS_PER_TURN,
        ) {
            Staged::Queued => {}
            Staged::OverCap => return Ok(ToolResult::error(over_cap(&effect))),
            Staged::NoDrain(why) => {
                return Ok(ToolResult::error(no_drain(ASSIGN_TASK_TOOL, &effect, why)));
            }
        }
        // Staged truth, not the past tense (issue #453). Nothing has been
        // written yet; the drain this turn's claim promises is what writes it.
        Ok(ToolResult::success(format!(
            "Recorded the assignment of card {task_id} to {assignee}; it takes effect as this turn \
             completes."
        )))
    }
}

/// A lifecycle tool that records the orchestrator's verdict on a card sitting
/// in `in_review`.
///
/// **Approving completes the card to `done`.** That is issue #171's
/// `in_review → done` transition (PR #179), which this tool supplies for the
/// one card shape #179's own rule cannot reach: a board-created card, which has
/// no `origin_chat_id` and so never completes on its own. The verdict is
/// recorded on the card's note either way; `revise` returns the card to
/// `todo` so it can be picked up again, with the verdict readable on it —
/// issue #301 collapsed the old `backlog` pool into To-do. See
/// [`crate::harness::lifecycle::review_landing_column`].
pub struct ReviewTaskTool {
    queue: DelegationQueue,
}

impl ReviewTaskTool {
    /// Builds the tool over the shared delegation queue.
    pub fn new(queue: DelegationQueue) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl Tool for ReviewTaskTool {
    fn name(&self) -> &str {
        REVIEW_TASK_TOOL
    }

    fn description(&self) -> &str {
        "Record your review of a task card that is awaiting review. Provide the card's `task_id` and a `decision` of `approve` (the work is accepted) or `revise` (it needs another pass, which returns the card to To-do), plus an optional `note` with your feedback."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The id of the card being reviewed." },
                "decision": {
                    "type": "string",
                    "enum": ["approve", "revise"],
                    "description": "`approve` to accept the work, `revise` to send it back."
                },
                "note": { "type": "string", "description": "Optional reviewer feedback." }
            },
            "required": ["task_id", "decision"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let task_id = required_str(&args, "task_id")?;
        let raw = required_str(&args, "decision")?;
        // An unrecognised verdict is an error, never a silent approval: a card
        // must not pass review on a typo.
        let decision = ReviewDecision::parse(&raw).ok_or_else(|| {
            anyhow::anyhow!("`decision` must be `approve` or `revise`, got `{raw}`")
        })?;
        let note = optional_str(&args, "note");

        let effect = format!("card {task_id} was NOT reviewed");
        match self.queue.push_within_cap(
            Delegation::ReviewTask {
                task_id: task_id.clone(),
                decision,
                note,
            },
            MAX_DELEGATIONS_PER_TURN,
        ) {
            Staged::Queued => {}
            Staged::OverCap => return Ok(ToolResult::error(over_cap(&effect))),
            Staged::NoDrain(why) => {
                return Ok(ToolResult::error(no_drain(REVIEW_TASK_TOOL, &effect, why)));
            }
        }
        // Issue #453: the card has NOT moved yet. It moves when the drain runs,
        // and the drain runs because this turn is claimed — which is what makes
        // "as this turn completes" a commitment rather than a hope. The old
        // wording ("it is complete and has moved to done") was the past tense
        // for something that had not happened, and on an unclaimed path it never
        // would.
        Ok(match decision {
            ReviewDecision::Approve => ToolResult::success(format!(
                "Recorded your approval of card {task_id}; it moves to done as this turn completes."
            )),
            ReviewDecision::Revise => ToolResult::success(format!(
                "Recorded your revision request; card {task_id} returns to To-do as this turn \
                 completes."
            )),
        })
    }
}

/// The refusal a delegation tool returns once this turn has already queued
/// [`MAX_DELEGATIONS_PER_TURN`] of them (issue #419).
///
/// `effect` names, in the tool's own terms, the thing that did **not** happen —
/// because the failure mode being fixed is a model that reports work it never
/// did. The refusal has to be unmistakably a refusal, name the bound, and say
/// what to do next, or the model will paper over it in its summary exactly as
/// it papered over the silent discard.
fn over_cap(effect: &str) -> String {
    format!(
        "Refused: this turn has already used all {MAX_DELEGATIONS_PER_TURN} of its delegations, so \
{effect}. Nothing was queued and nothing will happen. Do not report this as done — tell the \
operator which items you got to and which you did not, and raise the rest in your next turn."
    )
}

/// The refusal a delegation tool returns when **nothing will drain** what it
/// would queue (issues #453, #267) — modelled on
/// [`cannot_publish_here`](crate::harness::publish) one module over.
///
/// It has to do two jobs, and they are not the two [`over_cap`] does. It must
/// not read as a transient condition worth retrying — every turn on an unclaimed
/// path fails identically — and it must tell the agent what to say next, because
/// the failure this replaces was one the agent could not detect: it was told the
/// card had moved, so it told the operator the card had moved, and the next
/// turn's `clear()` threw the delegation away.
///
/// # One sentence could not do both causes
///
/// It was written for a genuinely inert context and then inherited, unchanged,
/// by [`NoDrainReason::Triage`] — a **fully capable** company whose triage read
/// this message as a question. There, "nothing here can carry out board work"
/// and "board actions are unavailable in this context" are both false as the
/// operator will hear them: board actions work fine, and would have worked on a
/// differently-phrased message. Paired with a triage miss the experience was
/// *ask for a landing page → "I could not do it; board actions are
/// unavailable"*, with no hint that rephrasing would work.
///
/// So the triage case gets its own text, which says what was actually read,
/// keeps the do-not-report-it-as-done half that both causes need, and gives the
/// model something recoverable to offer: restate it as a request.
///
/// # The log field is the measurement
///
/// `reason` is on the warn as well as in the message. The triage gate ships a
/// keyword classifier with teeth, and its residual miss rate is exactly the
/// number worth having afterwards — with both causes emitting identical text
/// there was no way to count one without the other. Kept at `warn` rather than
/// demoted to `info`: a refusal here means either the model over-reached on a
/// question or the triage misread a request, and both are worth seeing.
fn no_drain(tool: &str, effect: &str, reason: NoDrainReason) -> String {
    tracing::warn!(
        tool = %tool,
        reason = %reason,
        "[delegation] a delegation tool found no drain that would execute it; refusing in the \
         model's own turn rather than queuing into a queue nothing will drain"
    );
    match reason {
        NoDrainReason::Unwired => format!(
            "Refused: nothing here can carry out board work, so {effect}. Board actions are \
             unavailable in this context. Do not retry — it will fail the same way — and do NOT \
             report the action as done or describe the card as moved. Say plainly that you could \
             not do it."
        ),
        NoDrainReason::Triage => format!(
            "Refused: this message was read as a question rather than a request to do work, so \
             {effect}. Board writes are held back for this message only — answer it from what you \
             can read, and hand it to a desk if somebody else knows better. Do not retry this \
             call; it will fail the same way. Do NOT report the action as done or describe the \
             card as moved. If the operator did mean it as work, say so plainly and ask them to \
             restate it as a direct request."
        ),
    }
}

/// Reads a required non-empty string argument, trimmed.
fn required_str(args: &Value, key: &str) -> anyhow::Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("`{key}` is required"))
}

/// Reads an optional non-empty string argument, trimmed. A blank string is
/// treated as absent so a note never renders as an empty block.
fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The orchestrator's delegation and lifecycle tools over a shared queue:
/// `spawn_task`, `delegate_to_desk`, and — since #186 part b — `assign_task`
/// and `review_task`. `query_company` is built separately because it needs the
/// read ports, not the queue.
///
/// `delegate_to_desk` additionally takes the company id + store, which it reads
/// at call time to ground the delegation target against the company's real
/// desks (issue #272).
pub fn delegation_tools(
    queue: &DelegationQueue,
    company: CompanyId,
    store: Arc<dyn CompanyStore>,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(SpawnTaskTool::new(queue.clone())),
        Box::new(DelegateToDeskTool::new(queue.clone(), company, store)),
        Box::new(AssignTaskTool::new(queue.clone())),
        Box::new(ReviewTaskTool::new(queue.clone())),
    ]
}

// ---------------------------------------------------------------------------
// add_agent (issue #71)
// ---------------------------------------------------------------------------

/// A tool that lets the orchestrator bring on a new teammate mid-chat (issue
/// #71 — Active Runtime Teammates, the minimal slice): it writes an
/// [`OverlayAgent`] through the exact same load → push → save path the console
/// `POST .../team` route uses (`crate::server::ops::team::add_member`), so a
/// teammate added from chat is persisted identically to one added from the
/// operator's Team tab. The teammate becomes a real, addressable roster agent
/// on the company's next [`HarnessPool::ensure`](crate::harness::HarnessPool::ensure)
/// call (the overlay-agent freshness gate) — no restart needed.
///
/// No lifecycle states, budgets, or workspace/memory namespaces here — those
/// stay future work per the design doc; this tool only ever appends a roster
/// entry.
///
/// # The minted teammate's tool scope (issue #619)
///
/// `add_agent` is [`Reach::Nothing`](crate::policy::consequence) and sits in
/// `INTRINSIC_TOOLS`, so it is always present and never asks. Before #619 the
/// teammate it minted had no tools field at all and therefore held the
/// company's **whole** `[tools].allow` — a model could mint a teammate holding
/// the widest grant in the company with no approval anywhere in the path.
///
/// The rule now: **a minted teammate is never wider than the agent that minted
/// it.**
///
/// * With no `tools` argument, the teammate copies the minter's own `tools`
///   line verbatim. Copying the *line* rather than the resolved grant matters:
///   an unscoped minter (empty line) mints an unscoped teammate that keeps
///   tracking `[tools].allow`, instead of freezing today's allow-list into the
///   record as an explicit scope that a later company-wide narrowing would not
///   reach.
/// * With a `tools` argument, the request is narrowed against the minter's own
///   **effective** grant, so the teammate cannot be handed something the minter
///   does not itself hold. A request that survives that narrowing empty is a
///   clean error rather than a stored empty list — an empty list means "inherit
///   everything", so silently storing one would turn a deliberate narrowing
///   into the widest possible grant. That inversion is the whole defect #619
///   was filed about, and it must not be reachable through the fix for it.
///
/// Every mint is logged with the minter, the teammate, and the resolved grant.
/// A narrowing nobody can observe is the same defect wearing a different hat.
///
/// Writes are serialised per-company through a shared static mutex map, so the
/// orchestrator's `add_agent` and the console `POST .../team` route can never
/// clobber each other's `overlay_agents` list with concurrent load→push→save
/// cycles (the CodeRabbit concurrency finding).
///
/// The tool also guards against accidental duplicates: calling `add_agent` with
/// a `name` that already exists in the overlay set is a clean error, not a
/// silent duplicate that would surface two indistinguishable teammates in the
/// roster and Team view (the Greptile deduplication finding).
pub struct AddAgentTool {
    company: CompanyId,
    store: Arc<dyn CompanyStore>,
    /// The id of the agent this tool is wired onto — the minter, named in the
    /// mint log so an operator can see who added a teammate and with what.
    minter: String,
    /// The minter's own `tools` line, verbatim. Empty means the minter itself
    /// inherits the company grant; the teammate then does too.
    minter_tools: Vec<String>,
    /// The minter's **effective** grant — its line already narrowed by the
    /// company `allow`. The ceiling an explicit `tools` argument is clamped to.
    minter_grants: Vec<String>,
}

impl AddAgentTool {
    /// Builds the tool over the company id and its store handle
    /// ([`HarnessDeps::store`](crate::harness::HarnessDeps::store)), plus the
    /// minting agent's identity and tool scope (issue #619) — see the type docs
    /// for why a minted teammate is bounded by its minter.
    pub fn new(
        company: CompanyId,
        store: Arc<dyn CompanyStore>,
        minter: String,
        minter_tools: Vec<String>,
        minter_grants: Vec<String>,
    ) -> Self {
        Self {
            company,
            store,
            minter,
            minter_tools,
            minter_grants,
        }
    }
}

/// An `add_agent` tool wired onto an **unscoped** minter: an agent whose own
/// `tools` line is empty and which therefore holds the whole company grant.
///
/// This is the pre-#619 shape of every minter, so a test written before the
/// scoping seam existed still describes the same company through it. A test
/// that cares about narrowing constructs the tool directly with a scoped
/// minter instead.
#[cfg(test)]
pub(crate) fn unscoped_add_agent(company: CompanyId, store: Arc<dyn CompanyStore>) -> AddAgentTool {
    AddAgentTool::new(
        company,
        store,
        "ceo".to_string(),
        Vec::new(),
        vec!["fs:*".to_string(), "web:*".to_string()],
    )
}

#[async_trait]
impl Tool for AddAgentTool {
    fn name(&self) -> &str {
        ADD_AGENT_TOOL
    }

    fn description(&self) -> &str {
        "Add a new teammate to the company. Provide a `name`, a `role` (job title), an optional `description` of their mandate, and an optional `tools` scope. The teammate becomes a real, addressable member of the roster starting next turn."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The new teammate's display name." },
                "role": { "type": "string", "description": "The new teammate's job title." },
                "description": { "type": "string", "description": "An optional description of the teammate's mandate." },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tool globs to scope the teammate to, e.g. [\"fs:read\"] for a read-only teammate. Omit to give them the same tools you hold. You cannot grant more than you hold yourself."
                }
            },
            "required": ["name", "role"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`name` is required"))?
            .to_string();
        let role = args
            .get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| anyhow::anyhow!("`role` is required"))?
            .to_string();
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);

        // Issue #619: resolve the teammate's scope before touching the store, so
        // a refused scope is not a half-written roster.
        let requested_tools: Option<Vec<String>> =
            args.get("tools").and_then(Value::as_array).map(|globs| {
                globs
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                    .map(str::to_string)
                    .collect()
            });
        let tools = match requested_tools {
            // No scope asked for: the teammate copies the minter's own line, so
            // an unscoped minter keeps minting teammates that track the company
            // allow-list rather than freezing today's copy of it.
            None => self.minter_tools.clone(),
            // An explicitly empty list is the same request as none at all —
            // "give them what you have" — not "grant everything".
            Some(globs) if globs.is_empty() => self.minter_tools.clone(),
            Some(globs) => {
                // Narrow against what the minter actually holds. An empty
                // result means nothing asked for was within reach, and storing
                // that would read back as "inherit the whole company grant" —
                // the exact inversion #619 exists to remove.
                let narrowed = agent_effective_grants(&self.minter_grants, &globs);
                if narrowed.is_empty() {
                    return Ok(ToolResult::error(format!(
                        "None of the requested tools ({}) are within your own tool grant ({}), so \"{name}\" was not added. Ask for a subset of what you hold, or omit `tools` to give them the same grant you have.",
                        globs.join(", "),
                        if self.minter_grants.is_empty() {
                            "nothing".to_string()
                        } else {
                            self.minter_grants.join(", ")
                        },
                    )));
                }
                narrowed
            }
        };

        // Serialize per-company writes so the orchestrator's add_agent and the
        // console `POST .../team` route can never clobber each other's
        // `overlay_agents` list with concurrent load→push→save cycles.
        let write_lock = company_write_lock(&self.company);
        let _lock = write_lock.lock().await;

        // Same write path as the console `POST .../team` route: load, push the
        // overlay entry, save. Never rewrites the version-controlled manifest.
        let mut record = self
            .store
            .load(&self.company)
            .await?
            .ok_or_else(|| OpenCompanyError::CompanyNotFound(self.company.to_string()))?;

        // Deduplication guard: reject a call whose `name` already names an
        // existing overlay teammate, so a trigger-happy orchestrator can't
        // accumulate indistinguishable duplicates. Matching on name alone is
        // intentional — the orchestrator supplies display names, and an id
        // collision with a manifest agent is handled by `mint_agent_id` below.
        //
        // It does not subsume that check: this compares overlay *names*, so a
        // call naming "Backend Engineer" on a company whose manifest declares
        // `backend_engineer` passes here and still needs a suffixed id.
        let name_lower = name.to_ascii_lowercase();
        if record
            .overlay_agents
            .iter()
            .any(|a| a.name.to_ascii_lowercase() == name_lower)
        {
            return Ok(ToolResult::error(format!(
                "A teammate named \"{name}\" already exists. Pick a different name, or remove the existing one first."
            )));
        }
        // Same readable-id rule as the console route (issue #686), under the
        // same per-company write lock, so the two minting sites cannot hand out
        // one id twice.
        let id = record.mint_agent_id(&name);
        let agent = OverlayAgent {
            id: id.clone(),
            name: name.clone(),
            role: role.clone(),
            description,
            tools: tools.clone(),
        };
        record.overlay_agents.push(agent);
        self.store.save(&record).await?;

        // Issue #619: the mint is observable, naming the minter, the teammate,
        // and the grant it was given. A narrowing that happens silently is the
        // defect this fixes, repeated one layer down — and an *inherited* grant
        // is the line an operator most needs to see, because that is the
        // teammate holding everything the company holds.
        tracing::info!(
            company = %self.company,
            minter = %self.minter,
            teammate = %id,
            teammate_name = %name,
            scope = %if tools.is_empty() {
                "inherited: the company's standard grant".to_string()
            } else {
                tools.join(", ")
            },
            "[add_agent] minted an overlay teammate"
        );

        // The id is in the result because the orchestrator has to be able to
        // address the teammate it just created — delegating to it, or putting it
        // on a desk, takes the id, not the display name. The console gets the
        // same answer from `TeamMemberDto.id`; before this the agent-facing half
        // had no way to learn it at all.
        // The scope is in the result for the same reason it is in the log: the
        // agent that minted the teammate should be able to see what it handed
        // over, and "the same tools you hold" is a materially different answer
        // from a named list.
        let scope = if tools.is_empty() {
            "They hold the company's standard tool grant, the same as you.".to_string()
        } else {
            format!("Their tools are scoped to: {}.", tools.join(", "))
        };
        Ok(ToolResult::success(format!(
            "Added {name} (id `{id}`) as {role} to the team. {scope} They'll be reachable as a teammate starting next turn."
        )))
    }
}

// ---------------------------------------------------------------------------
// orchestrator_tools — the complete tool set (issues #53, #67, #71)
// ---------------------------------------------------------------------------

/// The complete tool set wired onto the company's orchestrator agent (issues
/// #53, #67, #71, and #112), in order: the `query_company` read surface, the
/// `spawn_task` and `delegate_to_desk` delegation tools, the `run_workflow`
/// execution tool, the `read_run_output` companion (issue #418), the
/// `create_workflow` authoring tool, and the `add_agent` roster-write tool.
///
/// [`build_agent`](crate::harness::build::build_agent) extends the orchestrator
/// agent's tools with exactly this vector, so a test over this function is the
/// registration check for the orchestrator's tool list. `workflow_source_dir` is
/// the company source directory (`companies/<name>`) whose `workflows/` subtree
/// holds the graphs; `workflow_runner` is the shared handle the runtime builder
/// fills once the runner is built. `store` is the company store the `add_agent`
/// tool writes through.
// One more dependency than clippy's threshold, and each is a distinct wired
// port the orchestrator's tools need. Bundling them into a struct would only
// relocate the surface — the same call is made from exactly one place
// (`build_agent`), so there is nothing to deduplicate.
#[allow(clippy::too_many_arguments)]
pub fn orchestrator_tools(
    company: CompanyId,
    facts: Option<Arc<dyn FactStore>>,
    events: Option<Arc<dyn EventLog>>,
    queue: &DelegationQueue,
    workflow_source_dir: Option<PathBuf>,
    workflow_runner: WorkflowRunnerHandle,
    run_supervisor: crate::runtime::RunSupervisor,
    store: Arc<dyn CompanyStore>,
    workflow_refs: WorkflowRefQueue,
    run_outputs: RunOutputCache,
    minter: String,
    minter_tools: Vec<String>,
    minter_grants: Vec<String>,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(QueryCompanyTool::new(
        company.clone(),
        facts,
        events.clone(),
        workflow_source_dir.clone(),
        Some(store.clone()),
    ))];
    tools.extend(delegation_tools(queue, company.clone(), store.clone()));
    tools.push(Box::new(RunWorkflowTool::new(
        company.clone(),
        workflow_source_dir.clone(),
        store.clone(),
        workflow_runner,
        run_supervisor,
        events.clone(),
        workflow_refs.clone(),
        run_outputs.clone(),
    )));
    // `read_run_output` (issue #418) is the run tool's companion: it reads full
    // node output out of the same bounded cache the run tool populates, so a
    // preview the summary clipped is reachable. Pushed right after the run tool.
    tools.push(Box::new(ReadRunOutputTool::new(
        company.clone(),
        run_outputs,
    )));
    // `create_workflow` (issue #112) shares the same source dir the run tool
    // reads graphs from, plus the store it enables the new id on and the event
    // log it journals the audit event to.
    tools.push(Box::new(CreateWorkflowTool::new(
        company.clone(),
        workflow_source_dir,
        store.clone(),
        events,
        workflow_refs,
    )));
    tools.push(Box::new(AddAgentTool::new(
        company,
        store,
        minter,
        minter_tools,
        minter_grants,
    )));
    tools
}

// ---------------------------------------------------------------------------
// run_workflow (issue #67)
// ---------------------------------------------------------------------------

/// A shared, fillable handle to the company's [`WorkflowRunner`].
///
/// The `run_workflow` tool must reach the runner, but the runner
/// ([`HarnessWorkflowRunner`](crate::workflows::HarnessWorkflowRunner)) is built
/// *from* [`HarnessDeps`](crate::harness::HarnessDeps) — so it cannot be a plain
/// field on deps without a construction cycle. Instead the runtime builder puts
/// an empty handle on deps, builds the runner from a deps clone (which shares
/// this one cell), then fills the handle. Every clone — the deps the brain later
/// builds the orchestrator agent from, and the tool captured into that agent —
/// sees the same cell, so the fill is visible at turn time.
///
/// The cell holds a [`Weak`], so the deps→handle→runner→deps reference is **not**
/// a strong cycle: the one strong reference lives on the
/// [`CompanyRuntime`](crate::company::CompanyRuntime), and the tool upgrades the
/// weak on demand. Empty until filled (and always empty on a build with no
/// runner), in which case the tool reports that workflow execution is not wired.
#[derive(Clone, Default)]
pub struct WorkflowRunnerHandle {
    inner: Arc<OnceLock<Weak<dyn WorkflowRunner>>>,
}

impl WorkflowRunnerHandle {
    /// Fills the handle with a weak reference to the built runner. Idempotent —
    /// a second fill is ignored (the runner is built once per company boot).
    pub fn set(&self, runner: &Arc<dyn WorkflowRunner>) {
        let _ = self.inner.set(Arc::downgrade(runner));
    }

    /// The wired runner, upgraded from the weak cell, or `None` when no runner
    /// was attached (or the owning runtime has been dropped).
    pub fn get(&self) -> Option<Arc<dyn WorkflowRunner>> {
        self.inner.get().and_then(Weak::upgrade)
    }
}

/// How many characters of a node item preview the run summary keeps.
const ITEM_PREVIEW_CHARS: usize = 120;

/// How many recent runs the in-process [`RunOutputCache`] keeps before evicting
/// the oldest.
const RUN_OUTPUT_CACHE_RUNS: usize = 8;

/// Total serialized-bytes ceiling across every cached run's node map (~4 MiB).
/// Reached before [`RUN_OUTPUT_CACHE_RUNS`] only by unusually large runs; the
/// oldest entries are evicted until the cache is back under it.
const RUN_OUTPUT_CACHE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// A single run whose node map serializes past this hard ceiling (16 MiB) is
/// **refused** rather than cached — caching it would blow the whole budget on
/// one run. The refusal is announced in the run summary footer (pointing at the
/// console run drawer), never silently dropped.
const RUN_OUTPUT_ENTRY_MAX_BYTES: usize = 16 * 1024 * 1024;

/// One cached run's node output: the run id it is keyed on, the workflow that
/// produced it, and the engine's `run.output["nodes"]` map (`{ "<node id>": {
/// "items": [ … ] } }`). `bytes` is the map's serialized size, held so the
/// byte-ceiling eviction does not re-serialize on every insert.
#[derive(Clone)]
struct CachedRunOutput {
    run_id: String,
    workflow_id: String,
    nodes: Value,
    bytes: usize,
    /// Display-name → node-id pairs from the workflow file, captured at store
    /// time so `read_run_output` can resolve a `node` argument that is the
    /// display name the run summary prints. The engine's `nodes` map is keyed by
    /// id (a slug); the summary shows the name (prose) — in real graphs the two
    /// differ, so a name-only lookup would miss. Resolved case-insensitively at
    /// read time, so keys are kept as authored.
    name_to_id: Vec<(String, String)>,
}

/// What [`RunOutputCache::store`] did with a run's node map — the run summary
/// footer is worded from this so a refused (oversized) run points the agent at
/// the console instead of at a `read_run_output` call that would 404.
#[derive(Clone, Copy)]
enum RunOutputStored {
    /// Cached and reachable via `read_run_output`.
    Stored,
    /// Over the hard per-run ceiling, so not cached. `bytes` is the size that
    /// tripped it, named in the footer.
    Oversized { bytes: usize },
}

/// A bounded, in-process cache of recent workflow-run node output, so the
/// `read_run_output` tool can hand back the items the run summary clipped.
///
/// # Why a bounded cache and not the journal
///
/// The run summary is the sole surface the orchestrator agent sees, and it only
/// previews each node's *last* item, clipped — items `1..n-1` and everything
/// past [`ITEM_PREVIEW_CHARS`] are unreachable from the turn. The obvious
/// "persist the output" answer is wrong here: the journal deliberately scrubs
/// node output (it feeds the SSE stream and the inference sidecar — issue's
/// tested no-output invariant), and the tenant workspace is for files on disk,
/// not a run's in-memory items. Durability is not the requirement either — the
/// consumer is the *same process* that produced the run, and the durable human
/// record already exists: the console run drawer renders a live run's output
/// from the POST run route, run history covers the settled trail, and since #596
/// a **durable per-node snapshot** ([`WorkflowRunOutputStore`](crate::ports::run_output::WorkflowRunOutputStore))
/// lets the console reopen any *past* run and read what each node produced. That
/// store reads the same `output["nodes"]` capture this cache does but persists it
/// on a sibling surface, so the two never share storage. So this is a plain
/// in-memory cache, bounded two ways ([`RUN_OUTPUT_CACHE_RUNS`] runs and
/// [`RUN_OUTPUT_CACHE_MAX_BYTES`] total), evicting oldest-first.
///
/// Cheap to [`Clone`] (a shared handle) exactly like [`WorkflowRefQueue`]: the
/// run tool that fills it and the read tool that drains it are both built in one
/// `build_agent` pass off the same [`HarnessDeps`] clone, so they see one cache.
#[derive(Clone, Default)]
pub struct RunOutputCache {
    inner: Arc<Mutex<VecDeque<CachedRunOutput>>>,
}

impl RunOutputCache {
    /// Caches a run's node map under its run id, enforcing both bounds and
    /// evicting oldest-first. A map that serializes past
    /// [`RUN_OUTPUT_ENTRY_MAX_BYTES`] is refused (returned as
    /// [`RunOutputStored::Oversized`]) rather than cached.
    fn store(
        &self,
        run_id: &str,
        workflow_id: &str,
        nodes: Value,
        name_to_id: Vec<(String, String)>,
    ) -> RunOutputStored {
        // An unserializable map can't be sized, so treat it as over the hard
        // ceiling: refuse it rather than cache it at a false zero that never
        // counts toward the byte ceiling and so never gets evicted.
        let bytes = serde_json::to_string(&nodes)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        if bytes > RUN_OUTPUT_ENTRY_MAX_BYTES {
            tracing::debug!(
                run_id = %run_id,
                workflow = %workflow_id,
                bytes,
                "read_run_output: run output over the hard ceiling, refusing to cache"
            );
            return RunOutputStored::Oversized { bytes };
        }
        let entry = CachedRunOutput {
            run_id: run_id.to_string(),
            workflow_id: workflow_id.to_string(),
            nodes,
            bytes,
            name_to_id,
        };
        let mut q = self.inner.lock().expect("run output cache");
        q.push_back(entry);
        // Run-count bound first.
        while q.len() > RUN_OUTPUT_CACHE_RUNS {
            q.pop_front();
        }
        // Then the byte ceiling — but never evict the run just stored, even when
        // it alone is over the ceiling (it is still under the hard per-run cap,
        // and refusing to keep the one thing a follow-up would read is worse
        // than briefly overshooting the total).
        let mut total: usize = q.iter().map(|e| e.bytes).sum();
        while total > RUN_OUTPUT_CACHE_MAX_BYTES && q.len() > 1 {
            if let Some(evicted) = q.pop_front() {
                total -= evicted.bytes;
            }
        }
        tracing::debug!(
            run_id = %run_id,
            workflow = %workflow_id,
            bytes,
            runs = q.len(),
            "read_run_output: cached run output"
        );
        RunOutputStored::Stored
    }

    /// The cached entry for `run_id`, if it is still held (recent and
    /// un-evicted). Cloned out so the lock is not held across rendering.
    fn get(&self, run_id: &str) -> Option<CachedRunOutput> {
        self.inner
            .lock()
            .expect("run output cache")
            .iter()
            .find(|e| e.run_id == run_id)
            .cloned()
    }

    /// How many runs are cached. Test/introspection helper.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().expect("run output cache").len()
    }
}

/// A tool that runs one of the company's saved workflows by id.
///
/// It mirrors the REST run route (`POST /workflows/{wid}/run`): load the graph
/// from the company source directory, then invoke the [`WorkflowRunner`] reached
/// through a shared [`WorkflowRunnerHandle`]. On success it returns a concise,
/// natural-language summary of the run (per-node outcome + any nodes left
/// pending approval) rather than the engine's raw JSON. Every failure mode — no
/// runner wired, no source directory, an unknown id, a load or run error — is an
/// agent-actionable [`ToolResult::error`], never a panic or a silent empty
/// result, so the orchestrator can reason about and report what went wrong.
pub struct RunWorkflowTool {
    company: CompanyId,
    source_dir: Option<PathBuf>,
    /// The company store, so the tool reads the record's runtime-authored graph
    /// bodies — the only place a hosted tenant's workflows live (issue #168).
    store: Arc<dyn CompanyStore>,
    runner: WorkflowRunnerHandle,
    /// The company's live set of cancellable runs (issue #383), so an
    /// agent-initiated run is registered exactly like an operator's.
    ///
    /// It matters more here than anywhere else: this is the one entry point
    /// nobody is watching a progress bar for, so a run an agent started on a
    /// wedged node had no observer AND no way to be stopped. Registering it puts
    /// it in the console's run history as `running` with a Cancel button.
    run_supervisor: crate::runtime::RunSupervisor,
    /// The company's journal, so an agent-initiated run records an outcome like
    /// every other entry point (issues #228, #371).
    ///
    /// This tool journaled **nothing** before #371 — a gap #228 left open when
    /// it made the console's and the scheduler's runs durable. That was merely
    /// an inconsistency then; it stops being harmless once the runner emits a
    /// `WorkflowRunStarted`, because an unjournaled finish would leave every
    /// agent-initiated run reading as interrupted, and the boot sweep would
    /// dutifully stamp a failure on runs that succeeded.
    ///
    /// `None` (the default build, and the tool's own tests) simply skips the
    /// record, exactly as the runner skips the progress events — the two degrade
    /// together, so the pair can never be half-present.
    events: Option<Arc<dyn EventLog>>,
    /// Issue #339: where a run this tool started is staged, so the dispatched
    /// card that started it can link to the workflow.
    ///
    /// The tool cannot write the card itself — it is built once per agent while
    /// the card varies per dispatch — so it stages and the brain drains. Pushed
    /// only after the runner actually returned a run: a refused, unknown or
    /// failed invocation produced nothing to point at.
    workflow_refs: WorkflowRefQueue,
    /// Issue #418: the bounded cache a successful run's node output is stored
    /// into, so the `read_run_output` companion can hand back the items this
    /// tool's summary previewed only the last of (and clipped). Shared handle,
    /// so the read tool built in the same `build_agent` pass sees what this one
    /// stores. A cancelled or failed run stores nothing.
    run_outputs: RunOutputCache,
}

impl RunWorkflowTool {
    /// Builds the tool over the company id, its on-disk source directory
    /// (`companies/<name>`, whose `workflows/` subtree holds the seed graphs),
    /// the company store (holding the runtime-authored graph bodies), the
    /// shared runner handle, the company's journal, the shared queue a
    /// dispatched card's output link is staged on (issue #339), and the run
    /// output cache the `read_run_output` companion reads back (issue #418).
    // Each argument is a distinct wired dependency; the tool is built from
    // exactly one place (`orchestrator_tools`), so there is nothing a parameter
    // struct would deduplicate — same rationale as `orchestrator_tools` above.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        company: CompanyId,
        source_dir: Option<PathBuf>,
        store: Arc<dyn CompanyStore>,
        runner: WorkflowRunnerHandle,
        run_supervisor: crate::runtime::RunSupervisor,
        events: Option<Arc<dyn EventLog>>,
        workflow_refs: WorkflowRefQueue,
        run_outputs: RunOutputCache,
    ) -> Self {
        Self {
            company,
            source_dir,
            store,
            runner,
            run_supervisor,
            events,
            workflow_refs,
            run_outputs,
        }
    }
}

#[async_trait]
impl Tool for RunWorkflowTool {
    fn name(&self) -> &str {
        RUN_WORKFLOW_TOOL
    }

    fn description(&self) -> &str {
        "Run one of the company's saved workflows by id to completion — use this to advance or finish work that is waiting on a workflow run. Provide the workflow `id` and an optional `input` trigger payload. Returns a summary of each node's outcome and any steps left pending approval; the summary previews only each node's last item, clipped, so call `read_run_output` with the returned `run_id` and a node name to read any node's full output."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The id of the workflow to run (its `workflows/<id>.toml` stem; see the workflows list)."
                },
                "input": {
                    "description": "An optional trigger payload seeded as the workflow's trigger item. Any JSON value; omit to run with no input."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn supports_markdown(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        // Accept `id` (canonical) or `workflow` (a natural alias) for the id.
        let wid = args
            .get("id")
            .or_else(|| args.get("workflow"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|w| !w.is_empty());
        let Some(wid) = wid.map(str::to_string) else {
            return Ok(ToolResult::error(
                "`id` is required: pass the id of the workflow to run (see the workflows list).",
            ));
        };
        let input = args.get("input").cloned().unwrap_or(Value::Null);

        // `wid` becomes a filename — reject anything that could escape the
        // `workflows/` directory (mirrors the REST route's guard).
        if !is_safe_workflow_id(&wid) {
            tracing::debug!(company = %self.company, workflow = %wid, "run_workflow: rejected unsafe id");
            return Ok(ToolResult::error(format!(
                "No workflow with id `{wid}` exists."
            )));
        }

        // The runner is reached through the shared handle; an empty handle means
        // no runner is wired (default build / no harness).
        let Some(runner) = self.runner.get() else {
            tracing::debug!(company = %self.company, workflow = %wid, runner = false, "run_workflow: no runner wired");
            return Ok(ToolResult::error(
                "Workflow execution isn't available on this deployment (no workflow runner is wired).",
            ));
        };

        // Load the saved graph from the seed ∪ overlay union, so a workflow the
        // console (or this agent) created on a hosted tenant runs the same as a
        // committed one.
        let overlays = match self.store.load(&self.company).await {
            Ok(record) => record.map(|r| r.overlay_workflows).unwrap_or_default(),
            Err(err) => {
                tracing::debug!(company = %self.company, workflow = %wid, error = %err, "run_workflow: record load failed");
                return Ok(ToolResult::error(format!(
                    "Couldn't read this company's saved workflows: {err}"
                )));
            }
        };
        // Mirror the REST run route: an id neither source has is a clean
        // "unknown id" rather than a raw read error (which would also leak the
        // on-disk path into agent-visible text).
        let file = match load_workflow_union(self.source_dir.as_deref(), &overlays, &wid) {
            Ok(file) => file,
            Err(err) => {
                tracing::debug!(company = %self.company, workflow = %wid, error = %err, "run_workflow: load failed");
                return Ok(ToolResult::error(format!(
                    "Couldn't load workflow `{wid}`: {err}"
                )));
            }
        };
        let Some(file) = file else {
            tracing::debug!(company = %self.company, workflow = %wid, "run_workflow: unknown id");
            return Ok(ToolResult::error(format!(
                "No workflow with id `{wid}` exists. Check the workflows list for valid ids."
            )));
        };

        tracing::debug!(company = %self.company, workflow = %wid, runner = true, "run_workflow: invoking runner");
        // Issue #371: an agent-initiated run is a run like any other, so it
        // mints a context and journals an outcome on BOTH arms. Not scheduled —
        // an agent asking for a run is closer to an operator pressing Run than
        // to a cron fire, and the flag drives exactly that distinction in the
        // console.
        //
        // Issue #383: minted through the supervisor so this run is cancellable
        // too. Deliberately NOT spawned onto its own task, unlike the HTTP run
        // route: this call sits inside an agent turn, and the `WORKFLOW_DEPTH`
        // re-entry guard that bounds a workflow reaching back into itself is
        // task-local. Spawning here would reset the depth mid-chain and turn the
        // guard off exactly where it is load-bearing.
        // Issue #401: `begin` refuses when the company is already at its
        // in-flight run ceiling. Return a `ToolResult::error` with retry-later
        // guidance — the same "stop, don't retry blindly" convention as the
        // cancelled-run arm below — rather than an `Err`, so the agent treats it
        // as a reason to wait instead of a tool failure to surface as a crash.
        // Nothing was started: no context, no run id, nothing journaled.
        let (ctx, _run_guard) = match self.run_supervisor.begin(&wid, false) {
            Ok(started) => started,
            Err(err) => {
                tracing::info!(
                    company = %self.company,
                    workflow = %wid,
                    %err,
                    "run_workflow: refused — company is at its in-flight run cap"
                );
                return Ok(ToolResult::error(format!(
                    "Workflow `{wid}` wasn't started: {err}. Wait for a running workflow to finish \
                     (or ask an operator to stop one from the runs view), then try again — don't \
                     retry in a loop."
                )));
            }
        };
        match runner.run(&self.company, &file, input, &ctx).await {
            Ok(run) => {
                tracing::debug!(
                    company = %self.company,
                    workflow = %wid,
                    pending = run.pending_approvals.len(),
                    "run_workflow: run succeeded"
                );
                if let Some(events) = self.events.as_ref() {
                    crate::runtime::record_run_finished(
                        events,
                        &self.company,
                        &wid,
                        false,
                        &ctx.run_id,
                        Ok(&run),
                    )
                    .await;
                }
                // Issue #383: a cancelled run is `Ok`, so without this arm the
                // agent would read the empty node summary as "the workflow did
                // nothing" and quite reasonably try again — against a run an
                // operator just deliberately stopped. Say what happened in
                // words, as a `ToolResult::error` so the agent treats it as a
                // reason to stop rather than a result to act on. It is still not
                // a *failure* of the graph, which is why the journal records no
                // error for it.
                if run.cancelled {
                    tracing::info!(
                        company = %self.company,
                        workflow = %wid,
                        run_id = %ctx.run_id,
                        "run_workflow: an operator stopped this run"
                    );
                    return Ok(ToolResult::error(format!(
                        "Workflow `{wid}` was stopped by an operator before it finished. Its \
                         completed steps are recorded in the run history. Don't retry it unless \
                         you're asked to — someone chose to stop it."
                    )));
                }
                // Issue #339: this run is something the turn produced, so a
                // dispatched card that reached here can link to it. Staged
                // here — *after* the cancelled arm above — because a run an
                // operator stopped is not a deliverable to advertise on a
                // card; its partial steps are in the run history either way.
                // Off a dispatch (an ordinary operator chat turn) nothing ever
                // drains this and the brain clears it before the next card, so
                // staging unconditionally cannot mis-attribute.
                self.workflow_refs.push(TaskOutputWorkflow {
                    workflow_id: file.id.clone(),
                    run_id: Some(ctx.run_id.clone()),
                    action: TaskOutputAction::Ran,
                });
                // Issue #418: stash the run's node output so `read_run_output`
                // can hand back the items this summary only previews the last
                // of. Cloned (not moved out of `run`) because the summary below
                // still reads `run.output`. The outcome shapes the footer — a
                // refused oversized run points at the console, not at a
                // `read_run_output` call that would find nothing.
                let nodes_map = run.output.get("nodes").cloned().unwrap_or(Value::Null);
                // Capture display-name → id so `read_run_output` resolves the
                // name the summary prints back to the id the cache is keyed on.
                let name_to_id: Vec<(String, String)> = file
                    .nodes
                    .iter()
                    .map(|n| (n.name.trim().to_string(), n.id.clone()))
                    .collect();
                let cache_outcome =
                    self.run_outputs
                        .store(&ctx.run_id, &file.id, nodes_map, name_to_id);
                let md = summarize_run(&file, &run, &ctx.run_id, cache_outcome);
                Ok(ToolResult::success_with_markdown(
                    json!({
                        "workflow": file.id,
                        "run_id": ctx.run_id,
                        "pending_approvals": run.pending_approvals.len(),
                    }),
                    md,
                ))
            }
            Err(err) => {
                tracing::debug!(company = %self.company, workflow = %wid, error = %err, "run_workflow: run failed");
                if let Some(events) = self.events.as_ref() {
                    crate::runtime::record_run_finished(
                        events,
                        &self.company,
                        &wid,
                        false,
                        &ctx.run_id,
                        Err(err.to_string().as_str()),
                    )
                    .await;
                }
                Ok(ToolResult::error(format!(
                    "Workflow `{wid}` failed to run: {err}"
                )))
            }
        }
    }
}

/// Whether `wid` is a single safe on-disk filename stem — no path separators, no
/// `..`, not empty — so it can't escape the `workflows/` directory.
fn is_safe_workflow_id(wid: &str) -> bool {
    use std::path::{Component, Path};
    let mut comps = Path::new(wid).components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

/// A concise, natural-language summary of a completed workflow run: a per-node
/// outcome line (in graph order) plus any nodes left pending approval. This is
/// what the tool hands back to the turn — never the engine's raw
/// `{ run, nodes }` JSON dumped verbatim.
/// `run_id` is the run's correlation id, so the footer can name the exact
/// `read_run_output` call that reads a node's full output; `cache` is what the
/// output cache did with this run's node map, so the footer points at the right
/// follow-up (a `read_run_output` call, or the console when the run was too
/// large to cache).
fn summarize_run(
    file: &WorkflowFile,
    run: &WorkflowRun,
    run_id: &str,
    cache: RunOutputStored,
) -> String {
    let mut md = format!("Ran workflow **{}** (`{}`).\n\n", file.name.trim(), file.id);
    md.push_str("## Per-node outcome\n");
    let nodes = run.output.get("nodes").and_then(Value::as_object);
    // Whether any per-node line carried output — drives the footer, which only
    // makes sense when there is something to read the full of.
    let mut rendered_output = false;
    match nodes {
        Some(nodes) if !file.nodes.is_empty() => {
            for node in &file.nodes {
                let name = node.name.trim();
                // Surface the node id alongside the display name: `read_run_output`
                // keys the cache by id, but the summary is the only place the agent
                // sees a node, and in real graphs ids are slugs while names are
                // prose. Printing `(`id`, kind)` makes the footer's `node: <id>`
                // instruction true without a wasted round trip.
                let id = node.id.as_str();
                let kind = node.kind.as_str();
                match nodes.get(&node.id) {
                    Some(state) => {
                        let items = state.get("items").and_then(Value::as_array);
                        let count = items.map(Vec::len).unwrap_or(0);
                        let preview = items
                            .and_then(|items| items.last())
                            .map(preview_item)
                            .filter(|p| !p.is_empty());
                        match preview {
                            // With >1 item the preview is the *last* of them, so
                            // say so — the earlier items are not shown here and
                            // reachable only through `read_run_output`.
                            Some(preview) if count > 1 => {
                                rendered_output = true;
                                md.push_str(&format!(
                                    "- **{name}** (`{id}`, {kind}): last of {count} items — {preview}\n"
                                ))
                            }
                            Some(preview) => {
                                rendered_output = true;
                                md.push_str(&format!(
                                    "- **{name}** (`{id}`, {kind}): {count} item(s) — {preview}\n"
                                ))
                            }
                            None => md.push_str(&format!(
                                "- **{name}** (`{id}`, {kind}): {count} item(s)\n"
                            )),
                        }
                    }
                    None => md.push_str(&format!("- **{name}** (`{id}`, {kind}): not reached\n")),
                }
            }
        }
        _ => md.push_str("_No per-node output was produced._\n"),
    }

    if run.pending_approvals.is_empty() {
        md.push_str("\nThe run reached its terminal node(s) without pausing for approval.\n");
    } else {
        md.push_str(&format!(
            "\n**Paused for approval** at: {}. Resolve these for the run to continue.\n",
            run.pending_approvals.join(", ")
        ));
    }

    // Footer: the previews above are the *last* item of each node, clipped, so
    // name the follow-up that reads the rest. Only when there was node output to
    // read, and worded off the tool-name const so it can never drift from the
    // registered tool. An oversized (refused) run cannot be read this way, so it
    // is sent to the console run drawer instead of to a dead `read_run_output`.
    if rendered_output {
        match cache {
            RunOutputStored::Stored => md.push_str(&format!(
                "\n_Previews are clipped. Read any node's full output with `{READ_RUN_OUTPUT_TOOL}` (run_id: `{run_id}`, node: <id>) — the `id` in each line above._\n"
            )),
            RunOutputStored::Oversized { bytes } => md.push_str(&format!(
                "\n_This run's output ({bytes} bytes) was too large to keep in memory, so `{READ_RUN_OUTPUT_TOOL}` can't reach it — open run `{run_id}` in the console's run drawer to read it in full._\n"
            )),
        }
    }
    md
}

/// A short, single-line preview of one node output item: the raw string when the
/// item is a string, else its compact JSON — truncated on a char boundary to
/// [`ITEM_PREVIEW_CHARS`]. A clipped preview ends `… (+N chars)` naming exactly
/// how many characters were dropped, so the omission is stated rather than left
/// to a bare `…`. Codepoint-safe (`chars()`-iterated, never byte-indexed).
fn preview_item(item: &Value) -> String {
    let raw = match item {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let total = one_line.chars().count();
    if total <= ITEM_PREVIEW_CHARS {
        one_line
    } else {
        let cut: String = one_line.chars().take(ITEM_PREVIEW_CHARS).collect();
        let dropped = total - ITEM_PREVIEW_CHARS;
        format!("{cut}… (+{dropped} chars)")
    }
}

// ---------------------------------------------------------------------------
// read_run_output (issue #418) — the run_workflow companion
// ---------------------------------------------------------------------------

/// Bytes reserved out of [`TOOL_RESULT_BUDGET_BYTES`](crate::harness::build::TOOL_RESULT_BUDGET_BYTES) for a `read_run_output`
/// page's own framing — the header line and the trailing "Showing chars …
/// Continue with offset=…" notice — so the whole rendered result stays under the
/// harness's tool-result budget and is never silently re-clipped downstream.
const READ_PAGE_HEADROOM_BYTES: usize = 1024;

/// Renders every item of a node's output into one string, each under an `Item i
/// of n:` header — strings verbatim, any other JSON value pretty-printed.
/// Returns the joined text and the item count.
fn render_run_items(items: &[Value]) -> (String, usize) {
    let n = items.len();
    let mut full = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            full.push_str("\n\n");
        }
        full.push_str(&format!("Item {} of {n}:\n", i + 1));
        match item {
            Value::String(s) => full.push_str(s),
            other => full.push_str(
                &serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
            ),
        }
    }
    (full, n)
}

/// Pages `full` starting at char `offset`, accumulating **whole** chars until the
/// next one would push the page past `budget` bytes. Returns the page text and,
/// when output remains, the char offset to resume from (`None` at the end).
///
/// Never splits a codepoint — chars are pushed whole, so a page boundary that
/// lands mid-multibyte-character keeps that character with the next page. An
/// empty page always takes at least one char, so a caller looping on the
/// returned offset always makes forward progress even against a pathological
/// budget.
fn page_run_output(full: &str, offset: usize, budget: usize) -> (String, Option<usize>) {
    // Resolve the char `offset` to a byte index once, then walk from there — so
    // reading a long output to its end is linear in the output, not quadratic in
    // the page count (each call would otherwise rescan from index 0, and the
    // caller re-renders the whole joined string per page).
    let start_byte = full
        .char_indices()
        .nth(offset)
        .map(|(b, _)| b)
        .unwrap_or(full.len());
    let mut page = String::new();
    for c in full[start_byte..].chars() {
        if !page.is_empty() && page.len() + c.len_utf8() > budget {
            // `page` holds exactly the chars taken since `offset`, so the resume
            // point is `offset` plus that count — and `c` starts the next page.
            // Count before the move into the returned tuple.
            let next = offset + page.chars().count();
            return (page, Some(next));
        }
        page.push(c);
    }
    (page, None)
}

/// The `read_run_output` tool (issue #418): reads a workflow run node's full,
/// unclipped output out of the bounded [`RunOutputCache`] the `run_workflow`
/// tool fills.
///
/// The run summary previews only each node's *last* item, clipped to
/// [`ITEM_PREVIEW_CHARS`] — so items `1..n-1`, and everything a preview dropped,
/// are otherwise unreachable from the turn. This tool renders **every** item and
/// pages the result under [`TOOL_RESULT_BUDGET_BYTES`](crate::harness::build::TOOL_RESULT_BUDGET_BYTES) so nothing is silently
/// re-clipped. Every failure — an unknown run id (evicted or from before a
/// restart), an unknown node, an empty node — is an agent-actionable
/// [`ToolResult`] naming the console fallback or the valid node ids, never a
/// panic or a bare empty result.
pub struct ReadRunOutputTool {
    /// The owning company — used only for `tracing` context, never in the
    /// lookup (which is by `run_id` alone). That is safe **because the cache is
    /// per-company by construction**: `HarnessDeps` (and the `RunOutputCache`
    /// handle it carries) is built once per tenant in `build_agent`
    /// (`src/runtime/builder.rs`), so a run id from another company can never be
    /// in this cache to collide with. Kept as a field so this invariant is
    /// explicit — a later refactor toward a shared cache must re-derive scoping
    /// from the id rather than assume this one is already isolated.
    company: CompanyId,
    run_outputs: RunOutputCache,
}

impl ReadRunOutputTool {
    /// Builds the tool over the company id and the shared run-output cache the
    /// `run_workflow` tool fills.
    pub fn new(company: CompanyId, run_outputs: RunOutputCache) -> Self {
        Self {
            company,
            run_outputs,
        }
    }
}

#[async_trait]
impl Tool for ReadRunOutputTool {
    fn name(&self) -> &str {
        READ_RUN_OUTPUT_TOOL
    }

    fn description(&self) -> &str {
        "Read the full, unclipped output of one node from a recent workflow run — use this when a `run_workflow` summary previewed only a node's last item (clipped). Provide the `run_id` from that summary and the node's id (the `id` shown in each summary line; the node's display name also resolves); pass `offset` to continue reading a long output where a previous page stopped. Only recent runs of this running company are cached; older runs are in the console's run drawer."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "The run id from the `run_workflow` summary footer (its `run_id`)."
                },
                "node": {
                    "type": "string",
                    "description": "The node whose full output to read — its `id` from the run summary line (the node's display name also resolves)."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Character offset to resume from; omit to start at the beginning. Use the `offset=` value a previous page ended with to read the next page."
                }
            },
            "required": ["run_id", "node"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let run_id = args
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(run_id) = run_id else {
            return Ok(ToolResult::error(
                "`run_id` is required: pass the `run_id` from the `run_workflow` summary.",
            ));
        };
        let node = args
            .get("node")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(node) = node else {
            return Ok(ToolResult::error(
                "`node` is required: pass the name of the node whose output to read (see the run summary).",
            ));
        };
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;

        let Some(entry) = self.run_outputs.get(run_id) else {
            tracing::debug!(company = %self.company, run_id = %run_id, "read_run_output: run not cached");
            return Ok(ToolResult::error(format!(
                "No cached output for run `{run_id}`. The run-output cache holds only the most \
                 recent workflow runs of this running company, so a run from before the last \
                 restart — or one pushed out by newer runs — isn't here. Open the run in the \
                 console's run drawer to read its full output."
            )));
        };

        let Some(nodes) = entry.nodes.as_object() else {
            tracing::debug!(company = %self.company, run_id = %run_id, "read_run_output: run recorded no node map");
            return Ok(ToolResult::error(format!(
                "Run `{run_id}` (workflow `{}`) recorded no per-node output to read.",
                entry.workflow_id
            )));
        };

        // Resolve the `node` argument three ways, so passing either the id or the
        // display name the run summary prints both land: an exact id match, a
        // case-insensitive id match, then — since ids are slugs but the summary
        // shows prose names — the display name resolved to its id through the
        // name→id map captured at store time.
        let state = nodes
            .get(node)
            .or_else(|| {
                nodes
                    .iter()
                    .find(|(id, _)| id.eq_ignore_ascii_case(node))
                    .map(|(_, st)| st)
            })
            .or_else(|| {
                entry
                    .name_to_id
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(node))
                    .and_then(|(_, id)| nodes.get(id))
            });
        let Some(state) = state else {
            // Name the valid ids + their item counts, so the agent can retry
            // with a real node rather than guess.
            let mut valid: Vec<String> = nodes
                .iter()
                .map(|(id, st)| {
                    let count = st
                        .get("items")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0);
                    format!("`{id}` ({count} item(s))")
                })
                .collect();
            valid.sort();
            let list = if valid.is_empty() {
                "(this run reached no nodes)".to_string()
            } else {
                valid.join(", ")
            };
            tracing::debug!(company = %self.company, run_id = %run_id, node = %node, "read_run_output: unknown node");
            return Ok(ToolResult::error(format!(
                "Run `{run_id}` has no node named `{node}`. Nodes in this run: {list}."
            )));
        };

        let items = state
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            return Ok(ToolResult::success(format!(
                "Node `{node}` of run `{run_id}` produced no items."
            )));
        }

        let (full, n) = render_run_items(&items);
        let total = full.chars().count();
        let start = offset.min(total);
        let budget = crate::harness::build::TOOL_RESULT_BUDGET_BYTES
            .saturating_sub(READ_PAGE_HEADROOM_BYTES);
        let (page, next) = page_run_output(&full, start, budget);
        let end = next.unwrap_or(total);

        let mut out = format!(
            "Full output of node `{node}` from run `{run_id}` (workflow `{}`) — {n} item(s), \
             {total} chars total.\n\n{page}",
            entry.workflow_id
        );
        if let Some(next_off) = next {
            out.push_str(&format!(
                "\n\nShowing chars {start}–{end} of {total}. Continue with offset={next_off}."
            ));
        } else if start > 0 {
            out.push_str(&format!(
                "\n\nShowing chars {start}–{end} of {total} (end)."
            ));
        }
        tracing::debug!(
            company = %self.company,
            run_id = %run_id,
            node = %node,
            start,
            end,
            total,
            "read_run_output: served a page"
        );
        Ok(ToolResult::success(out))
    }
}

// ---------------------------------------------------------------------------
// create_workflow (issue #112)
// ---------------------------------------------------------------------------

/// The camelCase request shape the `create_workflow` tool accepts — the same
/// graph shape the console's creator posts to `POST …/workflows` and the read
/// routes return (`id`/`name`/`description?`/`nodes`/`edges`), so a graph the
/// orchestrator authors is indistinguishable from one authored in the console.
///
/// **This is deliberately a narrower surface than the REST body, but the
/// narrowing is POLICY fields only** — the node shape below omits `schedule`
/// (issue #169), `onError`, `retry`, and `requiresApproval`, and nothing else.
/// Those four are unattended-run policy: a field the model cannot set is a field
/// it cannot get wrong, and each carries real consequence — retry/error policy
/// changes failure behavior, and a `schedule` makes a workflow run *on its own,
/// forever*, with no operator in the loop at the moment it fires. So
/// **agent-authored workflows stay manual-run only**: schedules are
/// operator-authored, through the console's creator or `POST …/workflows`, where
/// a human chose the cron. An agent can build the graph; a human decides whether
/// it runs unattended.
///
/// The FUNCTIONAL fields `config` and `destination` are accepted (issue #661,
/// H1): four of the six node kinds this tool advertises are inert without them.
/// A `tool_call` names the tool to run in `config.slug`; an `http_request` puts
/// its method/url in `config`; a `condition` branches on a `config` expression;
/// an `output` may route its report via `destination`. Omitting these did not
/// make the tool safer — it made the tool advertise `tool_call`/`http_request`/
/// `condition`/`output` while being unable to author a working one (on current
/// main `validate_draft_against_record` now *rejects* every config-less
/// `tool_call` as "names no `slug`"). Both fields flow into the same validated
/// `create_company_workflow` core the REST route and the builder use, so they
/// inherit that core's validation rather than adding any of their own.
///
/// **`tool_call` args are LITERAL only — no templated `=`-expressions (issue
/// #674).** At runtime a workflow `tool_call` node has *saved-node* position:
/// #614's more-permissive rule (not #338's unbounded-reach agent rule) governs
/// it, justified because a saved node passed TWO operator gates — a manifest
/// `[tools].allow` grant AND an operator authoring the node. But a node this
/// tool authors was authored by the *agent*, not an operator, so it reaches
/// runtime with only ONE operator gate (the grant) while still being treated as
/// a saved node. `config.args` passes through verbatim and `=`-expressions in
/// args are a live runtime feature (see `workflows::gate`'s
/// `every_reachable_workflow_tool_is_classified_by_name_alone`), so an agent
/// could author `tool_call{slug:"shell", args:{command:"=<expr over upstream
/// output>"}}` — clearing every author-time gate, taking saved-node position,
/// with model-chosen templated args: exactly #674's carve-out for
/// templated-from-upstream args, which are not pre-declared and must follow the
/// stricter agent rule. To keep the two-operator-gate model intact, the
/// [`TryFrom`] below **rejects any `tool_call` node whose `config` carries a
/// string beginning with `=`** (tinyflows' `is_expression` convention). Literal
/// args only here; templated wiring stays with the console + `POST …/workflows`,
/// where an operator picks the args. NOTE: #674's templated-args carve-out is
/// framed for saved (operator-authored) nodes; that it needs revisiting for
/// agent-authored nodes *generally* — not only this tool — is flagged, not fixed
/// here.
///
/// Whether agents should be able to schedule themselves is an open product
/// question. If the answer becomes yes, add the policy field here and to
/// [`RawWorkflow`] construction below — the model and validation already support
/// it, so nothing else has to change.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkflowArgs {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    nodes: Vec<CreateWorkflowArgNode>,
    #[serde(default)]
    edges: Vec<CreateWorkflowArgEdge>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkflowArgNode {
    #[serde(default)]
    id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    /// Free-form, kind-specific node config carried as JSON on the wire and
    /// converted to a `toml::Value` on the way into [`RawNode`] (issue #661): a
    /// `tool_call`'s `slug` (+ `args`), an `http_request`'s `method`/`url`, a
    /// `condition`'s `field` expression. A JSON `null` anywhere inside is a
    /// caller error — TOML has no null — refused in [`TryFrom`] below.
    #[serde(default)]
    config: Option<serde_json::Value>,
    /// Where an `output` node's report goes (`owner`/`email`/`channel` + an
    /// optional `target`). Reuses the REST route's [`WorkflowDestinationDef`];
    /// the shared create core enforces each kind's target contract.
    #[serde(default)]
    destination: Option<WorkflowDestinationDef>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkflowArgEdge {
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    label: Option<String>,
}

/// The dotted location of the first `=`-expression string found anywhere in
/// `value`, or `None` when it holds only literal values.
///
/// Matches tinyflows' `expr::is_expression` convention exactly — a string that
/// *starts with* `=` (no whitespace trim) is an expression the engine would
/// resolve at run time. Walks nested objects and arrays; array elements are
/// numeric segments, so a hit reads like `args.cc.0` — the same shape
/// tinyflows' `NullResolution.location` uses.
fn first_expression_location(value: &serde_json::Value, path: &str) -> Option<String> {
    match value {
        serde_json::Value::String(s) if s.starts_with('=') => Some(path.to_string()),
        serde_json::Value::Object(map) => map.iter().find_map(|(key, child)| {
            let next = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            first_expression_location(child, &next)
        }),
        serde_json::Value::Array(items) => items.iter().enumerate().find_map(|(index, child)| {
            let next = if path.is_empty() {
                index.to_string()
            } else {
                format!("{path}.{index}")
            };
            first_expression_location(child, &next)
        }),
        _ => None,
    }
}

/// Whether a JSON `null` appears anywhere inside `value` (recursively).
///
/// The JSON→TOML conversion in [`TryFrom`] fails for more than one reason — a
/// `null` (TOML has no null) but also, e.g., an integer outside `i64` range — so
/// the caller uses this to only append the "drop null-valued keys" remedy when a
/// null is actually the cause, and otherwise lets the raw converter error speak.
fn json_contains_null(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => map.values().any(json_contains_null),
        serde_json::Value::Array(items) => items.iter().any(json_contains_null),
        _ => false,
    }
}

impl TryFrom<CreateWorkflowArgs> for RawWorkflow {
    /// A prosumer-language conversion error. Every fallible step is a node's JSON
    /// `config`: a non-object shape, a templated `=`-expression on a `tool_call`
    /// (issue #674 — see the struct doc), or a value TOML cannot store (e.g. a
    /// `null`). The caller maps each straight onto a [`ToolResult::error`].
    type Error = String;

    fn try_from(args: CreateWorkflowArgs) -> Result<Self, String> {
        let mut nodes = Vec::with_capacity(args.nodes.len());
        for n in args.nodes {
            let config = match n.config {
                Some(json) => {
                    // The schema types `config` as an object; a scalar or array
                    // (e.g. `"config": "web_fetch"`) would otherwise persist as an
                    // inert TOML value — silently on an `http_request`/`condition`
                    // node. Refuse it here as an agent-actionable error.
                    if !json.is_object() {
                        return Err(format!(
                            "node `{}` has a non-object `config` — `config` must be a JSON object \
                             (e.g. `{{\"slug\": \"web_fetch\"}}`), not a bare string, number, or \
                             list.",
                            n.id
                        ));
                    }
                    // Issue #674 boundary: an agent-authored `tool_call` carries
                    // saved-node runtime position, so templated `=`-expression
                    // args would clear every author-time gate with model-chosen
                    // values (see the struct doc). Literal args only — reject any
                    // `=`-prefixed string anywhere in the config.
                    if n.kind == WorkflowNodeKind::ToolCall.as_str()
                        && let Some(location) = first_expression_location(&json, "")
                    {
                        return Err(format!(
                            "node `{}` puts a templated `=`-expression at `config.{location}` — an \
                             agent-authored `tool_call` accepts LITERAL args only, not \
                             `=`-expressions over upstream output. Use the console (or `POST \
                             …/workflows`) for templated wiring, where an operator picks the args.",
                            n.id
                        ));
                    }
                    // JSON config → TOML value. TOML has no `null`, so a `null`
                    // anywhere in the config is a caller error, not a 500 on
                    // write — the same rule the REST create route and the workflow
                    // builder apply. Other conversion failures (e.g. an integer
                    // outside `i64` range) get the converter's own message, with
                    // the null hint appended only when a null is the cause.
                    Some(toml::Value::try_from(&json).map_err(|err| {
                        if json_contains_null(&json) {
                            format!(
                                "node `{}` has config that can't be stored ({err}) — TOML has no \
                                 null; drop null-valued keys.",
                                n.id
                            )
                        } else {
                            format!("node `{}` has config that can't be stored ({err}).", n.id)
                        }
                    })?)
                }
                None => None,
            };
            nodes.push(RawNode {
                id: n.id,
                kind: n.kind,
                name: n.name,
                summary: n.summary,
                agent: n.agent,
                // Policy fields stay omitted — agent-authored graphs are
                // manual-run only (see the struct doc above).
                schedule: None,
                config,
                on_error: None,
                retry: None,
                requires_approval: None,
                destination: n.destination,
            });
        }
        Ok(Self {
            id: args.id,
            name: args.name,
            description: args.description,
            nodes,
            edges: args
                .edges
                .into_iter()
                .map(|e| RawEdge {
                    from: e.from,
                    to: e.to,
                    label: e.label,
                })
                .collect(),
        })
    }
}

/// A tool that lets the orchestrator author and save a brand-new workflow graph
/// mid-chat (issue #112).
///
/// It runs the exact same validated-persist core the console's
/// `POST …/workflows` route runs
/// ([`create_company_workflow`](crate::company::create_company_workflow)): safe
/// id + size caps, exactly one trigger, roster cross-check, case-insensitive
/// name uniqueness, [`parse_workflow`](crate::company::parse_workflow)
/// revalidation, atomic write, enable-on-record, best-effort audit event. So a
/// workflow the orchestrator creates is byte-identical to one created in the
/// console, immediately enabled, and runnable via `run_workflow` the same turn.
///
/// Every failure mode — no source directory, an invalid graph, a duplicate id
/// or name, a store write error — is an agent-actionable [`ToolResult::error`]
/// (the [`RunWorkflowTool`] convention), never a panic, so the orchestrator can
/// reason about and report exactly what to fix.
pub struct CreateWorkflowTool {
    company: CompanyId,
    source_dir: Option<PathBuf>,
    store: Arc<dyn CompanyStore>,
    events: Option<Arc<dyn EventLog>>,
    /// Issue #339: where a graph this tool authored is staged, so a dispatched
    /// card whose whole job was *"build us a process for this"* links to the
    /// process rather than to the sentence describing it.
    ///
    /// Carries no run id — nothing has executed yet. If the same turn goes on
    /// to run it, the queue collapses the pair to the run
    /// ([`WorkflowRefQueue::drain`](crate::harness::workflow_refs::WorkflowRefQueue::drain)).
    workflow_refs: WorkflowRefQueue,
}

impl CreateWorkflowTool {
    /// Builds the tool over the company id, its on-disk source directory
    /// (`companies/<name>`, whose `workflows/` subtree the graph lands in), the
    /// company store (to enable the new id), the event log (to journal the
    /// audit event), and the shared queue a dispatched card's output link is
    /// staged on (issue #339).
    pub fn new(
        company: CompanyId,
        source_dir: Option<PathBuf>,
        store: Arc<dyn CompanyStore>,
        events: Option<Arc<dyn EventLog>>,
        workflow_refs: WorkflowRefQueue,
    ) -> Self {
        Self {
            company,
            source_dir,
            store,
            events,
            workflow_refs,
        }
    }
}

#[async_trait]
impl Tool for CreateWorkflowTool {
    fn name(&self) -> &str {
        CREATE_WORKFLOW_TOOL
    }

    fn description(&self) -> &str {
        "Author and save a new workflow graph for the company, then enable it so it can be run with run_workflow. A workflow is a directed graph: exactly one `trigger` node (what starts it) plus any of `agent` (a roster teammate does a step — set `agent` to that teammate's id), `tool_call`, `http_request`, `condition`, and `output` nodes, joined by `edges` ({from, to, optional label}). Node ids must be unique; every `agent` node must name a real teammate. Per-kind config: a `tool_call` node REQUIRES `config.slug` and runs ONLY a wired shell/code/web/search tool (e.g. `shell`, `apply_patch`, `web_fetch`, `web_search`), with LITERAL `config.args` only (no `=`-expressions — use the console for templated wiring) — for Composio, GitHub, or media/image/video actions use an `agent` node instead, NOT a `tool_call` (those are agent-turn tool families; a non-wired slug is refused when the node runs). An `http_request` node needs `config.method` and `config.url`. A `condition` node needs a `config.field` boolean expression, with its outgoing edges labeled `yes`/`no`. An `output` node may carry a `destination` ({kind: `owner`/`email`/`channel`, and a `target` for email/channel). Never put a null value inside `config` — it can't be stored. Workflows authored here are manual-run only (no schedule). Use this to capture a repeatable process; then run it with run_workflow."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "A short unique id (the on-disk filename stem): no spaces, slashes, or `..`."
                },
                "name": {
                    "type": "string",
                    "description": "A human-readable name, unique among the company's workflows (case-insensitive)."
                },
                "description": {
                    "type": "string",
                    "description": "An optional one-line description of what the workflow does."
                },
                "nodes": {
                    "type": "array",
                    "description": "The graph's nodes. Exactly one must be a `trigger`.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Node id, unique within the graph." },
                            "kind": {
                                "type": "string",
                                "enum": ["trigger", "agent", "tool_call", "http_request", "condition", "output"],
                                "description": "One of the six node kinds."
                            },
                            "name": { "type": "string", "description": "Human-readable node name." },
                            "summary": { "type": "string", "description": "Optional short description of the step." },
                            "agent": { "type": "string", "description": "On an `agent` node only: the roster teammate id that performs the step." },
                            "config": {
                                "type": "object",
                                "description": "Kind-specific settings, a JSON object. `tool_call`: `{ \"slug\": \"<wired shell/code/web/search tool>\", \"args\": {…} }` (slug required; `args` must be LITERAL values, not `=`-expressions; Composio/GitHub/media are agent-turn families — use an `agent` node instead). `http_request`: `{ \"method\": \"GET\", \"url\": \"https://…\" }`. `condition`: `{ \"field\": \"<boolean expression>\" }` with `yes`/`no` edge labels. Never include null values — they can't be stored."
                            },
                            "destination": {
                                "type": "object",
                                "description": "On an `output` node only: where the report goes.",
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "enum": ["owner", "email", "channel"],
                                        "description": "`owner` (company admins; no target), `email` (target is an address), or `channel` (target is a wired channel id)."
                                    },
                                    "target": { "type": "string", "description": "The recipient: an email address (`email`) or channel id (`channel`). Absent for `owner`." }
                                },
                                "required": ["kind"],
                                "additionalProperties": false
                            }
                        },
                        "required": ["id", "kind", "name"],
                        "additionalProperties": false
                    }
                },
                "edges": {
                    "type": "array",
                    "description": "Directed edges between node ids.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "from": { "type": "string", "description": "Source node id." },
                            "to": { "type": "string", "description": "Destination node id." },
                            "label": { "type": "string", "description": "Optional branch label (e.g. `yes`/`no` off a condition)." }
                        },
                        "required": ["from", "to"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["id", "name", "nodes"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn supports_markdown(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let parsed = match serde_json::from_value::<CreateWorkflowArgs>(args) {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::debug!(company = %self.company, error = %err, "create_workflow: unreadable args");
                return Ok(ToolResult::error(format!(
                    "Couldn't read the workflow definition: {err}. Provide `id`, `name`, and `nodes` (with an `edges` list)."
                )));
            }
        };
        // The only fallible conversion step is a node's JSON `config` (TOML has
        // no null). Surface it as an agent-actionable error, never a panic.
        let draft: RawWorkflow = match RawWorkflow::try_from(parsed) {
            Ok(draft) => draft,
            Err(msg) => {
                tracing::debug!(company = %self.company, error = %msg, "create_workflow: unstorable config");
                return Ok(ToolResult::error(msg));
            }
        };

        // No source directory is needed: the graph body is persisted on the
        // company record, which is the only writable surface a hosted tenant has
        // (issue #168).
        tracing::debug!(company = %self.company, workflow = %draft.id, "create_workflow: authoring");
        match create_company_workflow(
            &self.company,
            self.source_dir.as_deref(),
            &self.store,
            self.events.as_ref(),
            draft,
        )
        .await
        {
            Ok(file) => {
                tracing::debug!(company = %self.company, workflow = %file.id, "create_workflow: created");
                // Issue #339: the graph is saved, so it is a real thing this
                // turn produced and a dispatched card can point at it. Only on
                // the `Ok` arm — a rejected draft persisted nothing.
                self.workflow_refs.push(TaskOutputWorkflow {
                    workflow_id: file.id.clone(),
                    run_id: None,
                    action: TaskOutputAction::Created,
                });
                let md = format!(
                    "Created workflow **{}** (`{}`). It's enabled and ready to run — use `run_workflow` with id `{}` to execute it.",
                    file.name.trim(),
                    file.id,
                    file.id
                );
                Ok(ToolResult::success_with_markdown(
                    json!({ "workflow": file.id }),
                    md,
                ))
            }
            Err(err) => {
                tracing::debug!(company = %self.company, error = %err, "create_workflow: rejected");
                let detail = match &err {
                    OpenCompanyError::InvalidRequest(message)
                    | OpenCompanyError::Conflict(message) => message.clone(),
                    _ => "the company couldn't save it right now; try again.".to_string(),
                };
                Ok(ToolResult::error(format!(
                    "Couldn't create the workflow: {detail}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use crate::ports::types::{CompanyRecord, CompanySummary, LedgerEntry};

    fn agent(id: &str, tier: Option<&str>) -> ManifestAgent {
        ManifestAgent {
            id: id.to_string(),
            role: "Role".to_string(),
            description: None,
            tier: tier.map(str::to_string),
            tools: Vec::new(),
            budget_usd_daily: None,
        }
    }

    /// Issue #267: the brief's **shape** is the thing under test, because the
    /// shape is what the model followed. Answering has to lead, the
    /// never-a-card rule has to be stated rather than implied, authoring a
    /// workflow has to read as something done in this turn, and the whole thing
    /// has to be no longer than the version it replaced — a "rebalance" that
    /// grew the brief would just be more prose competing with the lead.
    #[test]
    fn the_brief_leads_with_answering_and_did_not_grow() {
        let brief = orchestrator_brief();

        // The default leads. Measured by position, not by presence: the old
        // brief contained the same rule as its closing clause and behaviour
        // followed the enumeration instead.
        let answer_first = brief
            .find("MOST MESSAGES ARE QUESTIONS OR QUICK READS")
            .expect("the answering default is stated");
        for later in [
            "delegate_to_desk",
            "spawn_task",
            "create_workflow",
            "add_agent",
            "assign_task",
            "review_task",
        ] {
            let at = brief.find(later).unwrap_or_else(|| panic!("names {later}"));
            assert!(
                answer_first < at,
                "`{later}` is introduced before the answering default"
            );
        }

        assert!(
            brief.contains("is NEVER a card"),
            "the never-a-card rule must be stated, not implied: {brief}"
        );
        // The #442 two-decisions block survives the restructure.
        assert!(brief.contains("they are INDEPENDENT"), "{brief}");
        assert!(brief.contains("the hand-off IS the card"), "{brief}");
        // A "create a workflow" ask is authored now, not parked.
        assert!(
            brief.contains("author it NOW with `create_workflow`"),
            "the automate path must read as this-turn work: {brief}"
        );

        // The length of the brief this replaced. A ceiling, not a target.
        const PREVIOUS_LEN: usize = 2784;
        assert!(
            brief.len() <= PREVIOUS_LEN,
            "the brief grew to {} (was {PREVIOUS_LEN})",
            brief.len()
        );
    }

    /// Issue #276: both directions of the arming summary, including the name
    /// and id, and neither the actor nor the reason.
    #[test]
    fn an_arming_change_summarizes_in_both_directions_without_the_actor_or_the_reason() {
        let event = |enabled, reason| CompanyEvent::WorkflowEnabledChanged {
            workflow_id: "digest".to_string(),
            name: "Daily digest".to_string(),
            enabled,
            reason,
            by: Some(crate::ports::types::Actor {
                kind: crate::ports::types::ActorKind::User,
                id: "u_secret".to_string(),
            }),
        };

        let off = summarize_event(&event(
            false,
            crate::ports::types::WorkflowEnabledReason::Disarmed,
        ));
        assert!(off.contains("switched off"), "{off}");
        assert!(off.contains("Daily digest"), "{off}");
        assert!(off.contains("digest"), "{off}");

        let on = summarize_event(&event(
            true,
            crate::ports::types::WorkflowEnabledReason::Operator,
        ));
        assert!(on.contains("switched on"), "{on}");
        assert!(on.contains("Daily digest"), "{on}");

        // The actor id never reaches the insight surface, and neither does the
        // rule-vs-person distinction — see the arm's comment.
        for summary in [&off, &on] {
            assert!(!summary.contains("u_secret"), "{summary}");
            assert!(!summary.contains("disarm"), "{summary}");
            assert!(!summary.contains("operator"), "{summary}");
        }
    }

    /// **Issue #248, the insight-surface twin of the sidecar guard.** This
    /// one-liner is folded into the orchestrator's recent-activity context, so
    /// it is read by a model rather than by the tenant. A delivery row's
    /// `target` is a recipient's address and its `detail` quotes one when the
    /// transport refuses, so neither may appear here. The exclusion was written
    /// this way by #228; this pins it.
    #[test]
    fn a_finished_run_summarizes_to_counts_without_the_recipient_or_transport_text() {
        // `.invalid` is reserved by RFC 2606, so this fixture names nobody.
        const RECIPIENT: &str = "recipient@example.invalid";

        let summary = summarize_event(&CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".to_string(),
            scheduled: true,
            run_id: None,
            deliveries: vec![crate::ports::DeliveryReport {
                node: "owner_summary".to_string(),
                kind: "email".to_string(),
                target: Some(RECIPIENT.to_string()),
                status: crate::ports::DeliveryStatus::Failed,
                detail: format!(
                    "the mail transport refused the message: 550 5.1.1 <{RECIPIENT}>: Recipient \
                     address rejected"
                ),
                reason: crate::ports::DeliveryReason::MailTransportRefused,
            }],
            pending_approvals: Vec::new(),
            error: None,
            cancelled: false,
        });

        assert!(!summary.contains(RECIPIENT), "{summary}");
        assert!(!summary.contains("recipient@"), "{summary}");
        assert!(!summary.contains("Recipient address rejected"), "{summary}");
        assert!(!summary.contains("550"), "{summary}");
        // Still useful: which workflow, and that something did not go out.
        assert!(summary.contains("digest"), "{summary}");
        assert!(summary.contains("1 not delivered"), "{summary}");
    }

    /// **Issue #383, the twin of the sidecar's pin.** The insight tail is the
    /// other non-tenant reader of a finished run, and it had the same hole: a
    /// cancelled run carries no error, so it summarized as a clean finish and
    /// invited the orchestrator to reason about — or redo — work an operator had
    /// just stopped.
    #[test]
    fn a_cancelled_run_summarizes_as_stopped_rather_than_finished() {
        let summary = summarize_event(&CompanyEvent::WorkflowRunFinished {
            workflow_id: "digest".to_string(),
            scheduled: true,
            run_id: Some("run-1".to_string()),
            deliveries: Vec::new(),
            pending_approvals: Vec::new(),
            error: None,
            cancelled: true,
        });

        assert!(summary.contains("stopped"), "{summary}");
        assert!(
            !summary.contains("finished"),
            "a stopped run must not read as a finished one: {summary}"
        );
    }

    /// **Issue #327.** A workspace write summarizes structurally — the change
    /// word and the node id, nothing else.
    ///
    /// The node's *name* is the exclusion with teeth. It is operator-authored
    /// free text that routinely carries the substance of the note ("Q3 layoffs
    /// shortlist"), and this string is a non-sensitive one-liner for the
    /// insight surface, which is precisely where free text does not belong.
    /// Same reasoning as the recipient exclusion two tests up; the arm was
    /// written this way, and this is what pins it.
    #[test]
    fn a_workspace_write_summarizes_to_the_change_and_node_without_the_notes_name() {
        let summary = summarize_event(&CompanyEvent::WorkspaceChanged {
            node_id: "n-42".to_string(),
            change: "updated".to_string(),
        });

        // Exact, not `contains`: the whole claim is that nothing *else* is in
        // here. A future arm that looked the node up to add its name would keep
        // passing every `contains` assertion and fail this one.
        assert_eq!(summary, "workspace updated: n-42");
    }

    #[test]
    fn orchestrator_id_prefers_the_tagged_agent() {
        let roster = vec![
            agent("ceo", None),
            agent("chief", Some("orchestrator")),
            agent("eng", Some("reasoning")),
        ];
        assert_eq!(orchestrator_id(&roster).as_deref(), Some("chief"));
    }

    #[test]
    fn orchestrator_id_falls_back_to_first_agent() {
        let roster = vec![agent("ceo", None), agent("eng", None)];
        assert_eq!(orchestrator_id(&roster).as_deref(), Some("ceo"));
    }

    #[test]
    fn orchestrator_id_is_none_for_an_empty_roster() {
        assert_eq!(orchestrator_id(&[]), None);
    }

    #[test]
    fn delegation_tool_names_are_classified_internal() {
        assert!(is_delegation_tool(SPAWN_TASK_TOOL));
        assert!(is_delegation_tool(DELEGATE_TO_DESK_TOOL));
        assert!(is_delegation_tool(ADD_AGENT_TOOL));
        assert!(is_delegation_tool(CREATE_WORKFLOW_TOOL));
        // The read tool is NOT a delegation tool.
        assert!(!is_delegation_tool(QUERY_COMPANY_TOOL));
        assert!(!is_delegation_tool("send_email"));
    }

    #[test]
    fn queue_drains_fifo_up_to_cap_and_discards_the_rest() {
        let queue = DelegationQueue::default();
        for i in 0..5 {
            queue.push(Delegation::SpawnTask {
                title: format!("t{i}"),
                note: None,
                assignee: None,
            });
        }
        assert_eq!(queue.queued(), 5);
        let drained = queue.drain(MAX_DELEGATIONS_PER_TURN);
        assert_eq!(drained.len(), 3);
        // The first three (FIFO) survive; the queue is emptied.
        assert_eq!(
            drained[0],
            Delegation::SpawnTask {
                title: "t0".to_string(),
                note: None,
                assignee: None,
            }
        );
        assert_eq!(queue.queued(), 0);
    }

    #[test]
    fn clear_empties_the_queue() {
        let queue = DelegationQueue::default();
        queue.push(Delegation::DelegateToDesk {
            desk: "strategy".to_string(),
            instruction: "plan".to_string(),
        });
        queue.clear();
        assert_eq!(queue.queued(), 0);
    }

    // ── Issue #419: the cap is announced, not silently applied ─────────────

    /// The queue itself refuses past the cap rather than accepting work the
    /// drain will destroy.
    #[test]
    fn push_within_cap_refuses_once_the_turn_is_full() {
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        for i in 0..MAX_DELEGATIONS_PER_TURN {
            assert_eq!(
                queue.push_within_cap(
                    Delegation::SpawnTask {
                        title: format!("t{i}"),
                        note: None,
                        assignee: None,
                    },
                    MAX_DELEGATIONS_PER_TURN,
                ),
                Staged::Queued
            );
        }
        assert_eq!(
            queue.push_within_cap(
                Delegation::SpawnTask {
                    title: "one too many".to_string(),
                    note: None,
                    assignee: None,
                },
                MAX_DELEGATIONS_PER_TURN,
            ),
            Staged::OverCap
        );
        assert_eq!(queue.queued(), MAX_DELEGATIONS_PER_TURN);
    }

    // ── Issue #453: no claim, no delegation ────────────────────────────────

    /// The commitment is checked before the cap, and it is the answer an
    /// unclaimed queue gives however empty it is. A model told "this turn is
    /// full" would try again next turn; on an unclaimed path the next turn fails
    /// identically, so the two refusals must stay distinguishable.
    #[test]
    fn an_unclaimed_queue_refuses_before_the_cap_is_even_consulted() {
        let queue = DelegationQueue::default();
        assert!(!queue.drain_committed(), "uncommitted is the default");
        assert_eq!(
            queue.push_within_cap(
                Delegation::SpawnTask {
                    title: "first and only".to_string(),
                    note: None,
                    assignee: None,
                },
                MAX_DELEGATIONS_PER_TURN,
            ),
            Staged::NoDrain(NoDrainReason::Unwired),
            "an EMPTY unclaimed queue is still a queue nothing drains"
        );
        assert_eq!(queue.queued(), 0);
    }

    /// The RAII half, which is the one that was missing everywhere before #453:
    /// an early exit — a `?`, a panic, a `return` from the middle of a turn —
    /// must leave the queue empty and uncommitted, so the *next* caller inherits
    /// a refusal rather than this one's abandoned work.
    #[test]
    fn a_claim_that_exits_early_un_commits_and_clears() {
        let queue = DelegationQueue::default();

        // A turn that queues work and then bails before draining.
        fn bail(queue: &DelegationQueue) -> Result<(), &'static str> {
            let _claim = queue.claim();
            assert_eq!(
                queue.push_within_cap(
                    Delegation::ReviewTask {
                        task_id: "t1".to_string(),
                        decision: ReviewDecision::Approve,
                        note: None,
                    },
                    MAX_DELEGATIONS_PER_TURN,
                ),
                Staged::Queued
            );
            assert_eq!(queue.queued(), 1, "staged while the claim is live");
            Err("the turn failed after queuing")
        }

        assert!(bail(&queue).is_err());
        assert_eq!(
            queue.queued(),
            0,
            "the abandoned delegation must not survive the claim that staged it"
        );
        assert!(
            !queue.drain_committed(),
            "and the next caller must inherit a refusal, not this one's promise"
        );

        // Acquiring also clears, so a prior turn's leftovers can never be
        // executed for the caller that comes next.
        queue.push(Delegation::SpawnTask {
            title: "left behind".to_string(),
            note: None,
            assignee: None,
        });
        let _claim = queue.claim();
        assert_eq!(queue.queued(), 0);
    }

    /// The headline refusal, per tool, with the sentence each one owes the
    /// model. The effect clause is the tool's own — a generic "refused" would
    /// leave the model guessing which of its calls did not happen.
    #[tokio::test]
    async fn every_delegation_tool_refuses_when_nothing_will_drain() {
        let queue = DelegationQueue::default();
        let store = Arc::new(MemStore::seeded(desks_record(&CompanyId::new("acme"))))
            as Arc<dyn CompanyStore>;

        let cases: Vec<(Box<dyn Tool>, Value, &str)> = vec![
            (
                Box::new(SpawnTaskTool::new(queue.clone())),
                json!({ "title": "Ship it" }),
                "the card \"Ship it\" was NOT opened",
            ),
            (
                Box::new(DelegateToDeskTool::new(
                    queue.clone(),
                    CompanyId::new("acme"),
                    store,
                )),
                json!({ "desk": "strategy", "instruction": "draft a plan" }),
                "nothing was handed to the strategy desk",
            ),
            (
                Box::new(AssignTaskTool::new(queue.clone())),
                json!({ "task_id": "t1", "assignee": "writer" }),
                "card t1 was NOT assigned",
            ),
            (
                Box::new(ReviewTaskTool::new(queue.clone())),
                json!({ "task_id": "t1", "decision": "approve" }),
                "card t1 was NOT reviewed",
            ),
        ];

        for (tool, args, effect) in cases {
            let name = tool.name().to_string();
            let result = tool.execute(args).await.expect("execute");
            assert!(result.is_error, "{name} must refuse: {}", result.text());
            let text = result.text();
            assert!(text.contains(effect), "{name}: {text}");
            // Not retryable — the next turn on this path drains no better.
            assert!(text.contains("Do not retry"), "{name}: {text}");
            // And the model must not narrate it as done, which is the whole
            // failure this replaces.
            assert!(text.contains("report the action as done"), "{name}: {text}");
            // Deliberately NOT the cap sentence: this is a different problem
            // with a different remedy.
            assert!(!text.contains("delegations"), "{name}: {text}");
        }
        assert_eq!(queue.queued(), 0, "nothing may be staged by a refusal");
    }

    /// **Issue #267 review, finding 3.** The two no-drain causes stop sharing a
    /// sentence.
    ///
    /// Written for a genuinely inert context, the refusal was then inherited by
    /// a fully capable company whose triage read the message as a question —
    /// where "board actions are unavailable in this context" is simply false as
    /// the operator will hear it. Paired with a triage miss the experience was
    /// *ask for a landing page → "I could not do it; board actions are
    /// unavailable"*, with nothing to suggest that rephrasing would work.
    ///
    /// The halves both causes need stay on both; what differs is what the model
    /// is told happened, and what it can offer next.
    #[tokio::test]
    async fn the_triage_refusal_says_it_read_a_question_and_offers_a_way_forward() {
        let queue = DelegationQueue::default();
        let _claim = queue.claim_answering();
        let refused = SpawnTaskTool::new(queue.clone())
            .execute(json!({ "title": "Build the landing page" }))
            .await
            .expect("execute");
        assert!(refused.is_error, "{}", refused.text());
        let text = refused.text();

        assert!(
            text.contains("read as a question"),
            "it must name what actually happened: {text}"
        );
        assert!(
            text.contains("this message only"),
            "…and scope it to this message, not to the whole context: {text}"
        );
        assert!(
            text.contains("restate it"),
            "…and leave the model something recoverable to offer: {text}"
        );
        // The two claims that are false here, and were the whole complaint.
        assert!(
            !text.contains("nothing here can carry out board work"),
            "a capable company must not claim it cannot do board work: {text}"
        );
        assert!(
            !text.contains("unavailable in this context"),
            "the context is fine; the message was a question: {text}"
        );
        // …while everything both causes owe the model survives.
        assert!(text.contains("Do not retry"), "{text}");
        assert!(text.contains("report the action as done"), "{text}");
        assert!(
            text.contains("the card \"Build the landing page\" was NOT opened"),
            "the tool's own effect clause is untouched: {text}"
        );
        assert_eq!(queue.queued(), 0);
    }

    /// …and the inert-context refusal keeps saying the thing that is true only
    /// of it, so the split is a split rather than a rename.
    #[tokio::test]
    async fn the_unwired_refusal_still_says_the_context_cannot_do_board_work() {
        let queue = DelegationQueue::default();
        let refused = SpawnTaskTool::new(queue.clone())
            .execute(json!({ "title": "Ship it" }))
            .await
            .expect("execute");
        let text = refused.text();
        assert!(refused.is_error, "{text}");
        assert!(
            text.contains("nothing here can carry out board work"),
            "{text}"
        );
        assert!(!text.contains("read as a question"), "{text}");
    }

    /// The measurement finding 3 asks for: the two causes are distinguishable
    /// as data, not only as prose. Without this the rate at which the triage
    /// gate fires — the residual miss rate of a keyword classifier with teeth —
    /// could not be counted apart from a genuinely unwired context.
    #[test]
    fn the_two_no_drain_causes_are_countable_apart() {
        let queue = DelegationQueue::default();
        let spawn = || Delegation::SpawnTask {
            title: "Build the landing page".to_string(),
            note: None,
            assignee: None,
        };
        assert_eq!(
            queue.push_within_cap(spawn(), MAX_DELEGATIONS_PER_TURN),
            Staged::NoDrain(NoDrainReason::Unwired)
        );
        let claim = queue.claim_answering();
        assert_eq!(
            queue.push_within_cap(spawn(), MAX_DELEGATIONS_PER_TURN),
            Staged::NoDrain(NoDrainReason::Triage)
        );
        // …and a hand-off is not refused at all under the same claim, because it
        // is how the question gets answered (finding 2).
        assert_eq!(
            queue.push_within_cap(
                Delegation::DelegateToDesk {
                    desk: "eng".to_string(),
                    instruction: "what did you ship?".to_string(),
                },
                MAX_DELEGATIONS_PER_TURN,
            ),
            Staged::Queued
        );
        drop(claim);
        assert_ne!(
            NoDrainReason::Unwired.as_str(),
            NoDrainReason::Triage.as_str(),
            "the log field must separate them"
        );
    }

    /// The defect #419 names: the tool told the model "it will be opened on the
    /// board this turn" for a card the drain then threw away, so a turn asked
    /// for five cards, reported five, and left two. The call past the cap is now
    /// an **error** naming the bound, and nothing is queued.
    #[tokio::test]
    async fn spawn_task_refuses_past_the_cap_instead_of_promising_a_discarded_card() {
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let tool = SpawnTaskTool::new(queue.clone());
        for i in 0..MAX_DELEGATIONS_PER_TURN {
            let ok = tool
                .execute(json!({ "title": format!("item {i}") }))
                .await
                .expect("execute");
            assert!(!ok.is_error, "within the cap: {}", ok.text());
        }
        let refused = tool
            .execute(json!({ "title": "the fourth item" }))
            .await
            .expect("execute");
        assert!(refused.is_error, "{}", refused.text());
        let text = refused.text();
        assert!(text.contains("the fourth item"), "{text}");
        assert!(text.contains("NOT opened"), "{text}");
        assert!(
            text.contains(&MAX_DELEGATIONS_PER_TURN.to_string()),
            "the refusal names the bound: {text}"
        );
        // The queue is exactly full — the refusal queued nothing, so the drain
        // has nothing left over to destroy.
        assert_eq!(queue.queued(), MAX_DELEGATIONS_PER_TURN);
        assert_eq!(queue.drain(MAX_DELEGATIONS_PER_TURN).len(), 3);
    }

    /// Same for the hand-off tool, whose success line ("Its lead will answer
    /// this turn") was the more misleading of the two: it claimed a teammate had
    /// been given work nobody would ever run.
    #[tokio::test]
    async fn delegate_to_desk_refuses_past_the_cap() {
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        // An empty store loads no record, so desk grounding fails open and the
        // hand-off is queued exactly as it was before #272 — which isolates this
        // test to the cap.
        let store = Arc::new(MemStore::default()) as Arc<dyn CompanyStore>;
        let tool = DelegateToDeskTool::new(queue.clone(), CompanyId::new("acme"), store);
        for i in 0..MAX_DELEGATIONS_PER_TURN {
            let ok = tool
                .execute(json!({ "desk": "eng", "instruction": format!("item {i}") }))
                .await
                .expect("execute");
            assert!(!ok.is_error, "within the cap: {}", ok.text());
        }
        let refused = tool
            .execute(json!({ "desk": "eng", "instruction": "one more" }))
            .await
            .expect("execute");
        assert!(refused.is_error, "{}", refused.text());
        assert!(
            refused.text().contains("nothing was handed"),
            "{}",
            refused.text()
        );
        assert_eq!(queue.queued(), MAX_DELEGATIONS_PER_TURN);
    }

    /// The two board-lifecycle tools share the queue and therefore the cap, so
    /// they share the refusal — an `assign_task` that silently did not assign is
    /// the same defect wearing a different hat.
    #[tokio::test]
    async fn the_lifecycle_tools_refuse_past_the_cap_too() {
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let assign = AssignTaskTool::new(queue.clone());
        let review = ReviewTaskTool::new(queue.clone());
        for i in 0..MAX_DELEGATIONS_PER_TURN {
            assign
                .execute(json!({ "task_id": format!("t{i}"), "assignee": "eng" }))
                .await
                .expect("execute");
        }
        let refused_assign = assign
            .execute(json!({ "task_id": "t9", "assignee": "eng" }))
            .await
            .expect("execute");
        assert!(refused_assign.is_error, "{}", refused_assign.text());
        assert!(
            refused_assign.text().contains("NOT assigned"),
            "{}",
            refused_assign.text()
        );
        let refused_review = review
            .execute(json!({ "task_id": "t9", "decision": "approve" }))
            .await
            .expect("execute");
        assert!(refused_review.is_error, "{}", refused_review.text());
        assert!(
            refused_review.text().contains("NOT reviewed"),
            "{}",
            refused_review.text()
        );
        assert_eq!(queue.queued(), MAX_DELEGATIONS_PER_TURN);
    }

    #[tokio::test]
    async fn spawn_task_tool_enqueues_a_task() {
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let tool = SpawnTaskTool::new(queue.clone());
        tool.execute(json!({ "title": "Ship it", "note": "soon", "assignee": "eng" }))
            .await
            .expect("execute");
        let drained = queue.drain(MAX_DELEGATIONS_PER_TURN);
        assert_eq!(
            drained,
            vec![Delegation::SpawnTask {
                title: "Ship it".to_string(),
                note: Some("soon".to_string()),
                assignee: Some("eng".to_string()),
            }]
        );
    }

    #[tokio::test]
    async fn spawn_task_tool_requires_a_title() {
        let queue = DelegationQueue::default();
        let tool = SpawnTaskTool::new(queue.clone());
        assert!(tool.execute(json!({ "note": "no title" })).await.is_err());
        assert_eq!(queue.queued(), 0);
    }

    // ── Issue #186 part b: the lifecycle tools ─────────────────────────────

    #[tokio::test]
    async fn assign_task_tool_enqueues_an_assignment() {
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let tool = AssignTaskTool::new(queue.clone());
        tool.execute(json!({ "task_id": "t1", "assignee": "eng", "note": "closer to it" }))
            .await
            .expect("execute");
        assert_eq!(
            queue.drain(MAX_DELEGATIONS_PER_TURN),
            vec![Delegation::AssignTask {
                task_id: "t1".to_string(),
                assignee: "eng".to_string(),
                note: Some("closer to it".to_string()),
            }]
        );
    }

    #[tokio::test]
    async fn assign_task_tool_requires_a_card_and_an_assignee() {
        let queue = DelegationQueue::default();
        let tool = AssignTaskTool::new(queue.clone());
        assert!(tool.execute(json!({ "assignee": "eng" })).await.is_err());
        assert!(tool.execute(json!({ "task_id": "t1" })).await.is_err());
        // A blank string is not an assignee.
        assert!(
            tool.execute(json!({ "task_id": "t1", "assignee": "  " }))
                .await
                .is_err()
        );
        assert_eq!(queue.queued(), 0);
    }

    #[tokio::test]
    async fn review_task_tool_enqueues_both_verdicts() {
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let tool = ReviewTaskTool::new(queue.clone());
        let approved = tool
            .execute(json!({ "task_id": "t1", "decision": "approve", "note": "good" }))
            .await
            .expect("approve");
        let revised = tool
            .execute(json!({ "task_id": "t2", "decision": "revise" }))
            .await
            .expect("revise");

        // Issue #453: staged truth, not the past tense. The card has not moved
        // when this sentence is written — the drain the claim promises is what
        // moves it — and saying otherwise is what made an undrained turn a lie
        // told through the agent.
        assert!(!approved.is_error);
        let text = approved.text();
        assert!(text.contains("Recorded your approval of card t1"), "{text}");
        assert!(text.contains("as this turn completes"), "{text}");
        assert!(!text.contains("has moved"), "nothing has moved yet: {text}");
        let text = revised.text();
        assert!(text.contains("card t2 returns to To-do"), "{text}");
        assert!(text.contains("as this turn completes"), "{text}");

        assert_eq!(
            queue.drain(MAX_DELEGATIONS_PER_TURN),
            vec![
                Delegation::ReviewTask {
                    task_id: "t1".to_string(),
                    decision: ReviewDecision::Approve,
                    note: Some("good".to_string()),
                },
                Delegation::ReviewTask {
                    task_id: "t2".to_string(),
                    decision: ReviewDecision::Revise,
                    note: None,
                },
            ]
        );
    }

    /// An unrecognised verdict is an error, never a silent approval — a card
    /// must not pass review because the model typed something unexpected.
    #[tokio::test]
    async fn review_task_tool_rejects_an_unknown_verdict_rather_than_approving() {
        let queue = DelegationQueue::default();
        let tool = ReviewTaskTool::new(queue.clone());
        assert!(
            tool.execute(json!({ "task_id": "t1", "decision": "maybe" }))
                .await
                .is_err()
        );
        assert!(tool.execute(json!({ "task_id": "t1" })).await.is_err());
        assert_eq!(queue.queued(), 0, "nothing may be queued on a bad verdict");
    }

    /// Both lifecycle tools are internal delegation work, so the approval
    /// policy must classify them as such — never as an external effect to park.
    #[test]
    fn the_lifecycle_tools_are_internal_delegation_tools() {
        assert!(is_delegation_tool(ASSIGN_TASK_TOOL));
        assert!(is_delegation_tool(REVIEW_TASK_TOOL));
    }

    /// The orchestrator is actually handed the new tools.
    #[test]
    fn delegation_tools_include_the_lifecycle_tools() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let names: Vec<String> = delegation_tools(&DelegationQueue::default(), company, store)
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&ASSIGN_TASK_TOOL.to_string()), "{names:?}");
        assert!(names.contains(&REVIEW_TASK_TOOL.to_string()), "{names:?}");
        // …without dropping the ones that were already there.
        assert!(names.contains(&SPAWN_TASK_TOOL.to_string()), "{names:?}");
        assert!(
            names.contains(&DELEGATE_TO_DESK_TOOL.to_string()),
            "{names:?}"
        );
    }

    /// A company with a `strategy` desk led by a roster teammate, an
    /// `archive` desk nobody on the roster sits on, and a `writer` teammate who
    /// is *not* a desk — the exact shape issue #272 was observed on.
    fn desks_record(id: &CompanyId) -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"

[[agent]]
id = "writer"
role = "Writer"

[[group_chat]]
id = "strategy"
name = "Strategy desk"
members = ["writer"]

[[group_chat]]
id = "archive"
name = "Archive desk"
members = ["nobody"]
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            manifest,
            ..seeded_record(id)
        }
    }

    fn desk_tool(record: CompanyRecord, queue: &DelegationQueue) -> DelegateToDeskTool {
        let company = record.id.clone();
        DelegateToDeskTool::new(
            queue.clone(),
            company,
            Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>,
        )
    }

    #[tokio::test]
    async fn delegate_to_desk_tool_enqueues_a_hand_off() {
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let tool = desk_tool(desks_record(&CompanyId::new("acme")), &queue);
        let result = tool
            .execute(json!({ "desk": "strategy", "instruction": "draft a plan" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "a real desk with a lead is delegatable");
        let drained = queue.drain(MAX_DELEGATIONS_PER_TURN);
        assert_eq!(
            drained,
            vec![Delegation::DelegateToDesk {
                desk: "strategy".to_string(),
                instruction: "draft a plan".to_string(),
            }]
        );
    }

    /// Issue #272: the observed failure — the orchestrator handed work to
    /// `writer`, which is a teammate rather than a desk. Nothing may be queued,
    /// and the refusal must carry the real desk ids so the model can correct
    /// itself in the same turn.
    #[tokio::test]
    async fn delegate_to_desk_tool_refuses_a_desk_that_does_not_exist() {
        let queue = DelegationQueue::default();
        let tool = desk_tool(desks_record(&CompanyId::new("acme")), &queue);
        let result = tool
            .execute(json!({ "desk": "writer", "instruction": "draft the release note" }))
            .await
            .expect("execute");
        assert!(result.is_error, "an invented desk must be refused");
        let text = result.output_for_llm(true);
        assert!(text.contains("strategy"), "valid ids must be named: {text}");
        assert!(
            text.contains("teammate"),
            "a teammate-as-desk target must be named as such: {text}"
        );
        assert_eq!(
            queue.queued(),
            0,
            "a refused target must not survive as a queued hand-off"
        );
        assert_eq!(
            queue.drain_refusals(MAX_DELEGATIONS_PER_TURN),
            vec!["writer".to_string()],
            "the drain must be able to report the attempt on the card"
        );
    }

    /// A desk that exists but has nobody on the roster can never run a turn, so
    /// the hand-off is refused rather than queued into a drain that cannot
    /// deliver it.
    #[tokio::test]
    async fn delegate_to_desk_tool_refuses_a_desk_with_no_roster_lead() {
        let queue = DelegationQueue::default();
        let tool = desk_tool(desks_record(&CompanyId::new("acme")), &queue);
        let result = tool
            .execute(json!({ "desk": "archive", "instruction": "file it" }))
            .await
            .expect("execute");
        assert!(result.is_error, "a leadless desk must be refused");
        let text = result.output_for_llm(true);
        assert!(
            text.contains("no member on the roster"),
            "the refusal must name the cause: {text}"
        );
        assert!(
            text.contains("strategy"),
            "a desk that CAN take work must be offered: {text}"
        );
        assert_eq!(queue.queued(), 0);
    }

    /// Fail-open: with no record to read, delegation behaves exactly as it did
    /// before grounding existed. A store gap must not take delegation offline.
    #[tokio::test]
    async fn delegate_to_desk_tool_queues_ungrounded_when_no_record_is_readable() {
        let queue = DelegationQueue::default();
        // Claimed (issue #453): "fail open" is about the *desk grounding*, and
        // this pins that an unreadable record still queues. Whether anything
        // drains is a separate question with its own refusal.
        let _claim = queue.claim();
        let tool = DelegateToDeskTool::new(
            queue.clone(),
            CompanyId::new("acme"),
            Arc::new(MemStore::default()) as Arc<dyn CompanyStore>,
        );
        let result = tool
            .execute(json!({ "desk": "whatever", "instruction": "do it" }))
            .await
            .expect("execute");
        assert!(!result.is_error);
        assert_eq!(queue.queued(), 1);
    }

    /// **Issue #348 review.** The recent-activity tail is ten slots wide, and a
    /// discussion (#335) is an operator-driven writer into the same journal the
    /// tail reads. A row per post would let one afternoon's thread on one card
    /// push every dispatch, reply and approval out of the orchestrator's only
    /// view of what the company has been doing — and replace them with rows it
    /// cannot act on, since no agent participates in a discussion.
    ///
    /// So: posts never hold a slot, the run events survive a thread that
    /// outnumbers them, and the fact that people are talking is still reported —
    /// as one folded count, with no message text (the same no-quoting rule
    /// `summarize_event`'s arm carries).
    #[tokio::test]
    async fn discussion_posts_fold_to_one_line_instead_of_evicting_the_activity_tail() {
        use crate::ports::types::StoredEvent;
        use futures::stream::{self, BoxStream};

        /// A log that replays a fixed history.
        struct FixedLog(Vec<StoredEvent>);

        #[async_trait]
        impl EventLog for FixedLog {
            async fn append(
                &self,
                _id: &CompanyId,
                _event: CompanyEvent,
            ) -> crate::Result<EventSeq> {
                unreachable!("the insight surface only reads")
            }
            async fn read_from(
                &self,
                _id: &CompanyId,
                seq: EventSeq,
                limit: usize,
            ) -> crate::Result<Vec<StoredEvent>> {
                Ok(self
                    .0
                    .iter()
                    .filter(|e| e.seq.value() >= seq.value())
                    .take(limit)
                    .cloned()
                    .collect())
            }
            fn subscribe(&self, _id: &CompanyId) -> BoxStream<'static, StoredEvent> {
                Box::pin(stream::empty())
            }
        }

        let company = CompanyId::new("acme");
        let mut history = vec![StoredEvent {
            seq: EventSeq::new(0),
            company: company.clone(),
            event: CompanyEvent::TaskDispatched {
                task_id: "t-1".to_string(),
                run_id: None,
            },
            at_millis: 1,
        }];
        // Twenty posts — twice the tail — on the one card, as an afternoon of
        // back-and-forth actually looks.
        for n in 0..20u64 {
            history.push(StoredEvent {
                seq: EventSeq::new(n + 1),
                company: company.clone(),
                event: CompanyEvent::TaskDiscussionPosted {
                    task_id: "t-1".to_string(),
                    text: format!("ping the vendor again ({n})"),
                    by: None,
                },
                at_millis: 2 + n,
            });
        }
        history.push(StoredEvent {
            seq: EventSeq::new(21),
            company: company.clone(),
            event: CompanyEvent::DeskTaskCompleted {
                task_id: "t-1".to_string(),
                desk: "eng".to_string(),
                output: "shipped".to_string(),
                column: "done".to_string(),
                artifact_ids: Vec::new(),
            },
            at_millis: 30,
        });

        let log: Arc<dyn EventLog> = Arc::new(FixedLog(history));
        let tool = QueryCompanyTool::new(company, None, Some(log), None, None);
        let out = tool
            .execute(json!({}))
            .await
            .expect("execute")
            .output_for_llm(true);

        // Both run events survive the thread that buried them.
        assert!(out.contains("task dispatched"), "dispatch evicted: {out}");
        assert!(out.contains("task completed"), "completion evicted: {out}");
        // One folded line, not twenty rows — and no message text anywhere.
        assert!(out.contains("20 discussion posts"), "{out}");
        assert!(!out.contains("ping the vendor"), "post text quoted: {out}");
        assert_eq!(
            out.matches("discussion post").count(),
            1,
            "a post must not hold a slot of its own: {out}"
        );
    }

    /// Issue #420: the recent-activity tail keeps only [`RECENT_EVENTS`] rows,
    /// and it used to drop everything older in silence — a full log read as
    /// complete, the same silent-cut class the facts section one block down
    /// already announces. The tail now names how many rows fell off the far end
    /// (in the markdown) and reports the count (in the JSON summary). Discussion
    /// posts pushed past the tail are dropped rows too, so they count toward it
    /// rather than folding into their own line.
    #[tokio::test]
    async fn query_company_announces_the_dropped_event_tail() {
        use crate::ports::types::StoredEvent;
        use futures::stream::{self, BoxStream};

        /// A log that replays a fixed history.
        struct FixedLog(Vec<StoredEvent>);

        #[async_trait]
        impl EventLog for FixedLog {
            async fn append(
                &self,
                _id: &CompanyId,
                _event: CompanyEvent,
            ) -> crate::Result<EventSeq> {
                unreachable!("the insight surface only reads")
            }
            async fn read_from(
                &self,
                _id: &CompanyId,
                seq: EventSeq,
                limit: usize,
            ) -> crate::Result<Vec<StoredEvent>> {
                Ok(self
                    .0
                    .iter()
                    .filter(|e| e.seq.value() >= seq.value())
                    .take(limit)
                    .cloned()
                    .collect())
            }
            fn subscribe(&self, _id: &CompanyId) -> BoxStream<'static, StoredEvent> {
                Box::pin(stream::empty())
            }
        }

        let company = CompanyId::new("acme");

        // A distinct, non-discussion event so every row occupies a tail slot.
        let dispatch = |seq: u64| StoredEvent {
            seq: EventSeq::new(seq),
            company: company.clone(),
            event: CompanyEvent::TaskDispatched {
                task_id: format!("t-{seq}"),
                run_id: None,
            },
            at_millis: seq + 1,
        };

        // (a) Five more row-events than the tail is wide: the five oldest fall
        // off, the notice sits at the top, and the JSON summary counts them.
        let over: Vec<StoredEvent> = (0..(RECENT_EVENTS as u64 + 5)).map(dispatch).collect();
        let log: Arc<dyn EventLog> = Arc::new(FixedLog(over));
        let tool = QueryCompanyTool::new(company.clone(), None, Some(log), None, None);
        let result = tool.execute(json!({})).await.expect("execute");
        let md = result.output_for_llm(true);
        let activity = md
            .split("## Recent activity\n")
            .nth(1)
            .expect("recent activity section");
        assert!(
            activity.starts_with("- […5 earlier event(s) not shown]"),
            "the dropped tail must be announced at the top: {md}"
        );
        assert!(
            result
                .output_for_llm(false)
                .contains("\"events_not_shown\": 5"),
            "the JSON summary must count the drop: {}",
            result.output_for_llm(false)
        );

        // (b) Exactly the tail width: nothing was dropped, so nothing is said.
        let exact: Vec<StoredEvent> = (0..RECENT_EVENTS as u64).map(dispatch).collect();
        let log: Arc<dyn EventLog> = Arc::new(FixedLog(exact));
        let result = QueryCompanyTool::new(company.clone(), None, Some(log), None, None)
            .execute(json!({}))
            .await
            .expect("execute");
        assert!(
            !result
                .output_for_llm(true)
                .contains("earlier event(s) not shown"),
            "a complete tail must stay silent: {}",
            result.output_for_llm(true)
        );
        assert!(
            result
                .output_for_llm(false)
                .contains("\"events_not_shown\": 0"),
            "a complete tail reports zero dropped: {}",
            result.output_for_llm(false)
        );

        // (c) Discussion posts older than the tail are dropped rows: they count
        // toward the drop, not toward the fold line. Three posts (oldest) then
        // enough dispatches to fill the tail — the posts never get visited.
        let mut mixed: Vec<StoredEvent> = Vec::new();
        for seq in 0..3u64 {
            mixed.push(StoredEvent {
                seq: EventSeq::new(seq),
                company: company.clone(),
                event: CompanyEvent::TaskDiscussionPosted {
                    task_id: "t-1".to_string(),
                    text: format!("older chatter {seq}"),
                    by: None,
                },
                at_millis: seq + 1,
            });
        }
        for seq in 3..(RECENT_EVENTS as u64 + 5) {
            mixed.push(dispatch(seq));
        }
        // total = 3 posts + (RECENT_EVENTS + 2) dispatches; the tail holds
        // RECENT_EVENTS dispatches, so 2 dispatches + 3 posts = 5 fall off.
        let log: Arc<dyn EventLog> = Arc::new(FixedLog(mixed));
        let result = QueryCompanyTool::new(company.clone(), None, Some(log), None, None)
            .execute(json!({}))
            .await
            .expect("execute");
        let md = result.output_for_llm(true);
        assert!(
            md.contains("- […5 earlier event(s) not shown]"),
            "dropped discussion posts must count toward the tail drop: {md}"
        );
        assert!(
            !md.contains("discussion post"),
            "an unvisited post must not also fold into its own line: {md}"
        );
        assert!(
            result
                .output_for_llm(false)
                .contains("\"events_not_shown\": 5"),
            "{}",
            result.output_for_llm(false)
        );
    }

    /// Issue #410, point 4 (audit the same silent-cut class elsewhere): the
    /// fact list is capped at [`FACT_LIMIT`], and it used to be capped in
    /// silence. A company past twenty facts handed the orchestrator a partial
    /// memory that read as complete, so "we have no record of that" was a
    /// conclusion it could reach from a truncated list. The cut now says it
    /// happened and names the argument that narrows it.
    #[tokio::test]
    async fn query_company_says_when_the_fact_list_was_cut() {
        use crate::ports::FactStore;
        use crate::ports::facts::{FactKind, FactRecord};

        struct ManyFacts(usize);
        #[async_trait]
        impl FactStore for ManyFacts {
            async fn list(
                &self,
                _company: &CompanyId,
                _query: Option<&str>,
                _kind: Option<FactKind>,
            ) -> crate::Result<Vec<FactRecord>> {
                Ok((0..self.0)
                    .map(|i| FactRecord {
                        id: format!("f-{i}"),
                        kind: FactKind::Fact,
                        title: format!("Fact {i}"),
                        body: format!("Body {i}"),
                        source: "ceo".to_string(),
                        updated_at_millis: i as u64,
                    })
                    .collect())
            }
            async fn upsert(&self, _c: &CompanyId, _f: &FactRecord) -> crate::Result<()> {
                Ok(())
            }
            async fn delete(&self, _c: &CompanyId, _id: &str) -> crate::Result<bool> {
                Ok(false)
            }
        }

        // Exactly at the cap: complete, so no notice.
        let exact: Arc<dyn FactStore> = Arc::new(ManyFacts(FACT_LIMIT));
        let out = QueryCompanyTool::new(CompanyId::new("acme"), Some(exact), None, None, None)
            .execute(json!({}))
            .await
            .expect("execute")
            .output_for_llm(true);
        assert!(!out.contains("TRUNCATED"), "nothing was cut: {out}");

        // Past the cap: the cut is announced, counted, and points at `query`.
        let many: Arc<dyn FactStore> = Arc::new(ManyFacts(FACT_LIMIT + 7));
        let out = QueryCompanyTool::new(CompanyId::new("acme"), Some(many), None, None, None)
            .execute(json!({}))
            .await
            .expect("execute")
            .output_for_llm(true);
        assert!(
            out.contains("TRUNCATED"),
            "the cut must be announced: {out}"
        );
        assert!(out.contains("7 more fact(s) not shown"), "{out}");
        assert!(out.contains("query_company"), "{out}");
    }

    /// Issue #420, the residual: the whole insight document is handed to the
    /// model through the harness tool-result path, which hard-cuts anything past
    /// its byte budget — blindly. A facts list long enough would carry that cut
    /// into the sections below it, dropping the facts `[TRUNCATED]` marker and
    /// the Desks list `delegate_to_desk` reads. So each fact body is capped and
    /// the facts section is bounded in bytes; the marker and every later section
    /// stay inside the outer budget. Cutting a body counts characters, never
    /// bytes, so a multibyte body cannot panic mid-codepoint.
    #[tokio::test]
    async fn query_company_bounds_the_insight_document_size() {
        use crate::ports::FactStore;
        use crate::ports::facts::{FactKind, FactRecord};

        struct Facts(Vec<FactRecord>);
        #[async_trait]
        impl FactStore for Facts {
            async fn list(
                &self,
                _c: &CompanyId,
                _q: Option<&str>,
                _k: Option<FactKind>,
            ) -> crate::Result<Vec<FactRecord>> {
                Ok(self.0.clone())
            }
            async fn upsert(&self, _c: &CompanyId, _f: &FactRecord) -> crate::Result<()> {
                Ok(())
            }
            async fn delete(&self, _c: &CompanyId, _id: &str) -> crate::Result<bool> {
                Ok(false)
            }
        }

        let mk = |i: usize, body: String| FactRecord {
            id: format!("f-{i}"),
            kind: FactKind::Fact,
            title: format!("Fact {i}"),
            body,
            source: "ceo".to_string(),
            updated_at_millis: i as u64,
        };
        let render = |facts: Vec<FactRecord>| async move {
            let store: Arc<dyn FactStore> = Arc::new(Facts(facts));
            QueryCompanyTool::new(CompanyId::new("acme"), Some(store), None, None, None)
                .execute(json!({}))
                .await
                .expect("execute")
                .output_for_llm(true)
        };

        // (e) A single multi-KB multibyte body: cut on a char boundary, marked
        // with an ellipsis, exactly the cap wide, and no panic.
        let out = render(vec![mk(0, "é".repeat(5_000))]).await;
        let line = out
            .lines()
            .find(|l| l.starts_with("- **Fact 0**: "))
            .expect("fact line");
        let body = line.strip_prefix("- **Fact 0**: ").unwrap();
        assert!(body.ends_with('…'), "a cut body is marked: {body:?}");
        assert_eq!(
            body.chars().count(),
            MAX_FACT_BODY_CHARS,
            "the body is cut to exactly the cap"
        );
        assert!(
            body.chars().take(MAX_FACT_BODY_CHARS - 1).all(|c| c == 'é'),
            "the cut landed on a codepoint boundary, not inside one"
        );

        // (f) Enough capped bodies to blow the section byte budget. The count
        // reflects the budget cut, not merely FACT_LIMIT, and the marker plus
        // every section below Facts survives the outer tool-result cut.
        let heavy: Vec<FactRecord> = (0..FACT_LIMIT)
            .map(|i| mk(i, "é".repeat(MAX_FACT_BODY_CHARS)))
            .collect();
        let out = render(heavy).await;
        let shown = out.matches("- **Fact ").count();
        assert!(
            (1..FACT_LIMIT).contains(&shown),
            "the byte budget must cut before FACT_LIMIT yet keep at least one: shown={shown}"
        );
        assert!(
            out.contains(&format!("{} more fact(s) not shown", FACT_LIMIT - shown)),
            "the marker counts the budget cut: {out}"
        );
        for header in [
            "[TRUNCATED",
            "## Recent activity",
            "## Saved workflows",
            "## Team",
            "## Desks",
        ] {
            assert!(
                out.contains(header),
                "the facts cut must not carry the outer budget into `{header}`: {out}"
            );
        }

        // (g) A small document is byte-for-byte the pre-guard behavior: bodies
        // under the cap render verbatim and nothing is announced.
        let out = render(vec![
            mk(0, "Body 0".to_string()),
            mk(1, "Body 1".to_string()),
        ])
        .await;
        assert!(
            out.contains("## Facts\n- **Fact 0**: Body 0\n- **Fact 1**: Body 1\n"),
            "the small-document path is unchanged: {out}"
        );
        assert!(!out.contains("TRUNCATED"), "nothing was cut: {out}");
        assert!(!out.contains('…'), "nothing was truncated: {out}");
    }

    #[tokio::test]
    async fn query_company_tool_reports_no_data_when_unwired() {
        let tool = QueryCompanyTool::new(CompanyId::new("acme"), None, None, None, None);
        let result = tool.execute(json!({})).await.expect("execute");
        // The insight surface lives in the markdown; `output()` is the summary.
        let out = result.output_for_llm(true);
        assert!(out.contains("No durable facts recorded"), "{out}");
        assert!(out.contains("No recent activity"), "{out}");
        assert!(out.contains("No saved workflows"), "{out}");
    }

    /// Regression: a saved workflow (on disk) and an operator-added overlay
    /// teammate both show up in `query_company`. Before this the orchestrator
    /// had no way to enumerate either, so a freshly created workflow / added
    /// teammate looked unpersisted when the operator asked about it.
    #[tokio::test]
    async fn query_company_tool_lists_saved_workflows_and_roster() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("workflows")).unwrap();
        std::fs::write(
            dir.path().join("workflows").join("daily-standup.toml"),
            r#"
id = "daily-standup"
name = "Daily Standup"
description = "Morning summary."
[[node]]
id = "start"
kind = "trigger"
name = "Morning"
"#,
        )
        .unwrap();

        // A record whose overlay adds a teammate — the `add_agent`
        // persistence shape.
        let mut record = seeded_record(&CompanyId::new("acme"));
        record.overlay_agents.push(OverlayAgent {
            id: "fact-fetcher".to_string(),
            name: "Fact Fetcher".to_string(),
            role: "Researcher".to_string(),
            description: None,
            tools: Vec::new(),
        });
        let store: Arc<dyn CompanyStore> = Arc::new(MemStore::seeded(record));

        let tool = QueryCompanyTool::new(
            CompanyId::new("acme"),
            None,
            None,
            Some(dir.path().to_path_buf()),
            Some(store),
        );
        let out = tool
            .execute(json!({}))
            .await
            .expect("execute")
            .output_for_llm(true);

        assert!(out.contains("Daily Standup"), "workflow missing: {out}");
        assert!(out.contains("daily-standup"), "workflow id missing: {out}");
        assert!(
            out.contains("Fact Fetcher"),
            "overlay teammate name missing: {out}"
        );
    }

    /// Issue #272: `query_company` is the grounding surface the orchestrator is
    /// told to consult, but it listed the roster and not the **desks** — so an
    /// orchestrator about to delegate had no authoritative id to read and
    /// reached for a teammate's name instead. Every desk is listed by the id
    /// `delegate_to_desk` takes, with its lead, and a desk nobody leads says so.
    #[tokio::test]
    async fn query_company_tool_lists_the_desks_delegation_accepts() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> = Arc::new(MemStore::seeded(desks_record(&company)));
        let tool = QueryCompanyTool::new(company, None, None, None, Some(store));
        let out = tool
            .execute(json!({}))
            .await
            .expect("execute")
            .output_for_llm(true);

        assert!(out.contains("## Desks"), "{out}");
        assert!(
            out.contains("**strategy** — lead: writer"),
            "a delegatable desk must name its id and lead: {out}"
        );
        assert!(
            out.contains("**archive** — no member on the roster"),
            "a leadless desk must say it cannot be handed work: {out}"
        );
    }

    // --- add_agent (issue #71) ----------------------------------------------

    /// An in-memory `CompanyStore` so `AddAgentTool` can be exercised without a
    /// filesystem, mirroring `crate::server::ops::team`'s `add_member` write
    /// path (load → push overlay → save).
    #[derive(Default)]
    struct MemStore {
        record: StdMutex<Option<CompanyRecord>>,
    }

    impl MemStore {
        fn seeded(record: CompanyRecord) -> Self {
            Self {
                record: StdMutex::new(Some(record)),
            }
        }
    }

    #[async_trait::async_trait]
    impl CompanyStore for MemStore {
        async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Ok(self.record.lock().unwrap().clone())
        }
        async fn save(&self, record: &CompanyRecord) -> crate::Result<()> {
            *self.record.lock().unwrap() = Some(record.clone());
            Ok(())
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> crate::Result<()> {
            Ok(())
        }
    }

    fn empty_manifest() -> crate::company::CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n").expect("valid manifest")
    }

    fn seeded_record(id: &CompanyId) -> CompanyRecord {
        CompanyRecord {
            id: id.clone(),
            manifest: empty_manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            disabled_workflows: Vec::new(),
            template_provenance: None,
        }
    }

    #[tokio::test]
    async fn add_agent_tool_persists_an_overlay_teammate() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = unscoped_add_agent(company.clone(), store.clone());

        let result = tool
            .execute(json!({
                "name": "Jamie",
                "role": "Growth Lead",
                "description": "Owns acquisition experiments."
            }))
            .await
            .expect("execute");
        assert!(!result.is_error, "add_agent should succeed");

        let record = store
            .load(&company)
            .await
            .unwrap()
            .expect("record persisted");
        assert_eq!(record.overlay_agents.len(), 1);
        let added = &record.overlay_agents[0];
        assert_eq!(added.name, "Jamie");
        assert_eq!(added.role, "Growth Lead");
        assert_eq!(
            added.description.as_deref(),
            Some("Owns acquisition experiments.")
        );
        assert!(!added.id.is_empty(), "a stable id must be minted");
    }

    /// A minter scoped to part of the company grant, for the #619 tests below.
    /// `minter_tools` is the line it declares; `minter_grants` is that line
    /// already narrowed by the company `allow` — which is what `build_agent`
    /// hands the tool.
    fn scoped_add_agent(company: CompanyId, store: Arc<dyn CompanyStore>) -> AddAgentTool {
        AddAgentTool::new(
            company,
            store,
            "ceo".to_string(),
            vec!["workspace".to_string()],
            vec!["workspace".to_string()],
        )
    }

    /// Issue #619: a teammate minted by a **scoped** agent inherits that agent's
    /// line, not the company's whole grant.
    ///
    /// Before this, `add_agent` — which is `Reach::Nothing` and never asks —
    /// could mint a teammate holding everything the company held, from an agent
    /// that held far less.
    #[tokio::test]
    async fn a_minted_teammate_copies_the_minters_scope() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = scoped_add_agent(company.clone(), store.clone());

        let result = tool
            .execute(json!({ "name": "Jamie", "role": "Growth Lead" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "got {:?}", result.text());

        let record = store.load(&company).await.unwrap().expect("persisted");
        assert_eq!(
            record.overlay_agents[0].tools,
            vec!["workspace".to_string()],
            "the minted teammate must be bounded by the agent that minted it"
        );
    }

    /// An **unscoped** minter still mints an unscoped teammate — the pre-#619
    /// behaviour, kept deliberately. Copying the minter's *line* rather than its
    /// resolved grant is what keeps the teammate tracking `[tools].allow`
    /// instead of freezing today's copy of it into the record.
    #[tokio::test]
    async fn an_unscoped_minter_mints_an_unscoped_teammate() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = unscoped_add_agent(company.clone(), store.clone());

        let result = tool
            .execute(json!({ "name": "Jamie", "role": "Growth Lead" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "got {:?}", result.text());

        let record = store.load(&company).await.unwrap().expect("persisted");
        assert!(
            record.overlay_agents[0].tools.is_empty(),
            "an empty line means the company's standard grant (#264), and a \
             minter holding that grant hands on exactly it"
        );
    }

    /// An explicit `tools` request is narrowed against what the minter holds, so
    /// the tool cannot be used to hand out a grant its caller does not have.
    #[tokio::test]
    async fn an_explicit_scope_is_narrowed_to_what_the_minter_holds() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = scoped_add_agent(company.clone(), store.clone());

        let result = tool
            .execute(json!({
                "name": "Jamie",
                "role": "Growth Lead",
                "tools": ["workspace", "composio"]
            }))
            .await
            .expect("execute");
        assert!(!result.is_error, "got {:?}", result.text());

        let record = store.load(&company).await.unwrap().expect("persisted");
        assert_eq!(
            record.overlay_agents[0].tools,
            vec!["workspace".to_string()],
            "`composio` is outside the minter's own grant and must be dropped"
        );
    }

    /// A request that narrows to **nothing** is a refusal, not a stored empty
    /// list.
    ///
    /// This is the sharp edge of the fix: an empty `tools` list means "inherit
    /// the company's standard grant". Storing the empty result of a narrowing
    /// would therefore turn the single most deliberate narrowing an agent can
    /// ask for into the widest grant in the company — the exact inversion #619
    /// exists to remove.
    #[tokio::test]
    async fn a_scope_entirely_outside_the_minters_grant_is_refused() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = scoped_add_agent(company.clone(), store.clone());

        let result = tool
            .execute(json!({
                "name": "Jamie",
                "role": "Growth Lead",
                "tools": ["composio"]
            }))
            .await
            .expect("execute");
        assert!(result.is_error, "got {:?}", result.text());

        let record = store.load(&company).await.unwrap().expect("persisted");
        assert!(
            record.overlay_agents.is_empty(),
            "and no teammate was written at all, scoped or otherwise"
        );
    }

    /// Issue #686 — the tool mints the same readable, name-derived id the
    /// console route does, and hands it back in the result so the orchestrator
    /// can delegate to the teammate it just created.
    #[tokio::test]
    async fn add_agent_tool_mints_a_readable_id_and_reports_it() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = unscoped_add_agent(company.clone(), store.clone());

        let result = tool
            .execute(json!({ "name": "Dana Designer", "role": "Designer" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{}", result.text());
        assert!(
            result.text().contains("`dana_designer`"),
            "the id must be in the result, not only in the record: {}",
            result.text()
        );

        let record = store.load(&company).await.unwrap().expect("record");
        assert_eq!(record.overlay_agents[0].id, "dana_designer");
    }

    /// The name guard still fires, and it fires *before* minting — so a
    /// duplicate display name is refused rather than quietly given a `_2` id.
    /// Two teammates the orchestrator cannot tell apart is the thing that guard
    /// exists to stop, and readable ids do not make it less true.
    #[tokio::test]
    async fn add_agent_tool_still_refuses_a_duplicate_display_name() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = unscoped_add_agent(company.clone(), store.clone());

        for _ in 0..1 {
            let first = tool
                .execute(json!({ "name": "Dana Designer", "role": "Designer" }))
                .await
                .expect("execute");
            assert!(!first.is_error, "{}", first.text());
        }

        let second = tool
            .execute(json!({ "name": "dana designer", "role": "Illustrator" }))
            .await
            .expect("execute");
        assert!(second.is_error, "{}", second.text());
        assert!(
            second.text().contains("already exists"),
            "{}",
            second.text()
        );

        let record = store.load(&company).await.unwrap().expect("record");
        assert_eq!(
            record.overlay_agents.len(),
            1,
            "the refusal must not have persisted a `dana_designer_2`"
        );
    }

    /// A name colliding with a **manifest** agent's id passes the name guard —
    /// it compares overlay names — and is caught by the minter instead. The
    /// roster-level consequence is pinned in `harness::tests`.
    #[tokio::test]
    async fn add_agent_tool_suffixes_past_a_manifest_agent_id() {
        let company = CompanyId::new("acme");
        let mut record = seeded_record(&company);
        record.manifest = toml::from_str(
            "[company]\nname = \"Acme\"\n\
             [[agent]]\nid = \"backend_engineer\"\nrole = \"Backend Engineer\"\n",
        )
        .expect("valid manifest");
        let store = Arc::new(MemStore::seeded(record));
        let tool = unscoped_add_agent(company.clone(), store.clone());

        let result = tool
            .execute(json!({ "name": "Backend Engineer", "role": "Platform" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{}", result.text());

        let record = store.load(&company).await.unwrap().expect("record");
        assert_eq!(record.overlay_agents[0].id, "backend_engineer_2");
    }

    #[tokio::test]
    async fn add_agent_tool_requires_name_and_role() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = unscoped_add_agent(company.clone(), store.clone());

        assert!(
            tool.execute(json!({ "role": "Growth Lead" }))
                .await
                .is_err(),
            "missing `name` must be rejected"
        );
        assert!(
            tool.execute(json!({ "name": "Jamie" })).await.is_err(),
            "missing `role` must be rejected"
        );
        let record = store.load(&company).await.unwrap().expect("record");
        assert!(
            record.overlay_agents.is_empty(),
            "a rejected call must not persist a half-formed teammate"
        );
    }

    #[tokio::test]
    async fn add_agent_tool_reports_company_not_found() {
        let company = CompanyId::new("ghost");
        let store: Arc<dyn CompanyStore> = Arc::new(MemStore::default());
        let tool = unscoped_add_agent(company, store);

        let err = tool
            .execute(json!({ "name": "Jamie", "role": "Growth Lead" }))
            .await
            .expect_err("no record for this company id");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    // ---- run_workflow (issue #67) ----

    /// A valid trigger → agent → output graph, mirroring the REST route's fixture.
    const DEMO_WF: &str = r#"
        id = "demo"
        name = "Demo flow"
        description = "A tiny trigger → agent → output graph."
        [[node]]
        id = "start"
        kind = "trigger"
        name = "Start"
        [[node]]
        id = "worker"
        kind = "agent"
        name = "Worker"
        agent = "assistant"
        [[node]]
        id = "done"
        kind = "output"
        name = "Report"
        [[edge]]
        from = "start"
        to = "worker"
        [[edge]]
        from = "worker"
        to = "done"
    "#;

    /// A [`WorkflowRunner`] test double: records the ids it was asked to run and
    /// returns a canned [`WorkflowRun`].
    struct StubRunner {
        calls: Arc<Mutex<Vec<String>>>,
        run: WorkflowRun,
    }

    impl StubRunner {
        fn new(run: WorkflowRun) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                run,
            }
        }

        fn empty() -> Self {
            Self::new(WorkflowRun {
                output: Value::Null,
                pending_approvals: Vec::new(),
                deliveries: Vec::new(),
                cancelled: false,
                nodes: Vec::new(),
            })
        }
    }

    #[async_trait::async_trait]
    impl WorkflowRunner for StubRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            workflow: &WorkflowFile,
            _input: Value,
            _ctx: &crate::ports::WorkflowRunContext,
        ) -> crate::Result<WorkflowRun> {
            self.calls.lock().unwrap().push(workflow.id.clone());
            Ok(self.run.clone())
        }
    }

    /// Writes `DEMO_WF` to `<dir>/workflows/demo.toml`.
    fn seed_demo_workflow(dir: &std::path::Path) {
        let wf = dir.join("workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("demo.toml"), DEMO_WF).unwrap();
    }

    #[test]
    fn workflow_runner_handle_is_empty_until_filled() {
        let handle = WorkflowRunnerHandle::default();
        assert!(handle.get().is_none());
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
        handle.set(&runner);
        assert!(handle.get().is_some());
    }

    #[test]
    fn workflow_runner_handle_holds_only_a_weak_reference() {
        // Proves the deps↔runner cell is not a strong cycle: once the sole strong
        // owner drops, the handle can no longer upgrade.
        let handle = WorkflowRunnerHandle::default();
        {
            let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
            handle.set(&runner);
            assert!(handle.get().is_some());
        }
        assert!(
            handle.get().is_none(),
            "the handle must not keep the runner alive"
        );
    }

    #[test]
    fn orchestrator_tools_includes_all_nine() {
        let queue = DelegationQueue::default();
        let tools = orchestrator_tools(
            CompanyId::new("acme"),
            None,
            None,
            &queue,
            None,
            WorkflowRunnerHandle::default(),
            crate::runtime::RunSupervisor::default(),
            Arc::new(MemStore::default()),
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
            "ceo".to_string(),
            Vec::new(),
            vec!["fs:*".to_string()],
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        // Six before #186; `assign_task` + `review_task` made eight; #418's
        // `read_run_output` makes nine.
        assert_eq!(names.len(), 9, "got {names:?}");
        assert!(names.contains(&RUN_WORKFLOW_TOOL), "got {names:?}");
        assert!(names.contains(&READ_RUN_OUTPUT_TOOL), "got {names:?}");
        assert!(names.contains(&CREATE_WORKFLOW_TOOL), "got {names:?}");
        assert!(names.contains(&ADD_AGENT_TOOL), "got {names:?}");
        assert!(names.contains(&QUERY_COMPANY_TOOL), "got {names:?}");
        assert!(names.contains(&SPAWN_TASK_TOOL), "got {names:?}");
        assert!(names.contains(&DELEGATE_TO_DESK_TOOL), "got {names:?}");
        assert!(names.contains(&ASSIGN_TASK_TOOL), "got {names:?}");
        assert!(names.contains(&REVIEW_TASK_TOOL), "got {names:?}");
        // `read_run_output` sits immediately after `run_workflow`.
        let run_at = names.iter().position(|n| *n == RUN_WORKFLOW_TOOL).unwrap();
        assert_eq!(names[run_at + 1], READ_RUN_OUTPUT_TOOL, "got {names:?}");
    }

    #[tokio::test]
    async fn run_workflow_tool_loads_and_invokes_the_runner() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let runner_impl = StubRunner::new(WorkflowRun {
            output: json!({
                "run": {},
                "nodes": { "worker": { "items": ["did the thing"] }, "done": { "items": [] } }
            }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
        });
        let calls = runner_impl.calls.clone();
        let runner: Arc<dyn WorkflowRunner> = Arc::new(runner_impl);
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);

        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
        );
        let result = tool
            .execute(json!({ "id": "demo", "input": { "seed": 1 } }))
            .await
            .expect("execute");

        assert!(!result.is_error, "expected success, got {result:?}");
        assert_eq!(calls.lock().unwrap().as_slice(), ["demo"]);
        let out = result.output_for_llm(true);
        assert!(out.contains("Demo flow"), "{out}");
        assert!(out.contains("did the thing"), "{out}");
        assert!(out.contains("without pausing for approval"), "{out}");
    }

    /// Issue #339: a run this tool started is staged for the dispatched card
    /// that started it, carrying the run id so the card's link can open the
    /// overlay showing what actually executed.
    #[tokio::test]
    async fn a_successful_run_stages_a_workflow_reference_for_the_card() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": ["did the thing"] } } }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let refs = WorkflowRefQueue::default();

        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            refs.clone(),
            RunOutputCache::default(),
        );
        let result = tool
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{result:?}");

        let staged = refs.drain();
        assert_eq!(staged.len(), 1, "got {staged:?}");
        assert_eq!(staged[0].workflow_id, "demo");
        assert_eq!(staged[0].action, TaskOutputAction::Ran);
        assert!(
            staged[0].run_id.is_some(),
            "a run the card links to must name the run that happened"
        );
    }

    /// The other half, and the one worth pinning: a run that never happened
    /// stages nothing. An unknown id, an unwired runner and a failed run all
    /// produced no deliverable, so a card must not advertise one.
    #[tokio::test]
    async fn a_run_that_did_not_happen_stages_nothing() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());
        let refs = WorkflowRefQueue::default();

        // No runner wired.
        let unwired = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            WorkflowRunnerHandle::default(),
            crate::runtime::RunSupervisor::default(),
            None,
            refs.clone(),
            RunOutputCache::default(),
        );
        assert!(
            unwired
                .execute(json!({ "id": "demo" }))
                .await
                .expect("execute")
                .is_error
        );

        // A wired runner, but an id neither source has.
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let unknown = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            refs.clone(),
            RunOutputCache::default(),
        );
        assert!(
            unknown
                .execute(json!({ "id": "nope" }))
                .await
                .expect("execute")
                .is_error
        );

        assert_eq!(refs.queued(), 0, "nothing ran, so nothing may be linked");
    }

    /// A run an operator stopped is not a deliverable to put on a card. Its
    /// partial steps stay in the run history either way, so nothing is lost —
    /// what is avoided is a card advertising work somebody deliberately halted.
    #[tokio::test]
    async fn a_cancelled_run_stages_nothing() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": {} }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: true,
            nodes: Vec::new(),
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let refs = WorkflowRefQueue::default();

        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            refs.clone(),
            RunOutputCache::default(),
        );
        let result = tool
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute");
        assert!(result.is_error, "a cancelled run reports as a stop");
        assert_eq!(refs.queued(), 0);
    }

    #[tokio::test]
    async fn run_workflow_tool_surfaces_pending_approvals() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": [] } } }),
            pending_approvals: vec!["worker".to_string()],
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);

        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
        );
        let result = tool
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute");
        assert!(!result.is_error);
        let out = result.output_for_llm(true);
        assert!(out.contains("Paused for approval"), "{out}");
        assert!(out.contains("worker"), "{out}");
    }

    #[tokio::test]
    async fn run_workflow_tool_errors_when_no_runner_is_wired() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());
        // A valid workflow on disk, but an empty handle → not wired.
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            WorkflowRunnerHandle::default(),
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
        );
        let result = tool
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute");
        assert!(result.is_error, "expected an error result");
        assert!(result.output_for_llm(false).contains("wired"), "{result:?}");
    }

    #[tokio::test]
    async fn run_workflow_tool_errors_on_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);

        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
        );
        let result = tool
            .execute(json!({ "id": "nope" }))
            .await
            .expect("execute");
        assert!(result.is_error);
        assert!(
            result.output_for_llm(false).contains("No workflow with id"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn run_workflow_tool_requires_an_id() {
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            None,
            Arc::new(MemStore::default()),
            WorkflowRunnerHandle::default(),
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
        );
        let result = tool.execute(json!({})).await.expect("execute");
        assert!(result.is_error);
        assert!(
            result.output_for_llm(false).contains("`id` is required"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn run_workflow_tool_rejects_traversal_ids() {
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::empty());
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(std::path::PathBuf::from("/tmp")),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
        );
        let result = tool
            .execute(json!({ "id": "../secrets" }))
            .await
            .expect("execute");
        assert!(result.is_error);
    }

    // ---- create_workflow (issue #112) ----

    /// A record with an `assistant` roster agent so an `agent`-node graph passes
    /// the roster cross-check inside the create core.
    fn record_with_assistant(company: &CompanyId) -> CompanyRecord {
        let manifest: crate::company::CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"assistant\"\nrole = \"Assistant\"\n",
        )
        .expect("valid manifest");
        CompanyRecord {
            id: company.clone(),
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
            disabled_workflows: Vec::new(),
            template_provenance: None,
        }
    }

    /// The canonical happy graph the create tool accepts (camelCase body).
    fn greeter_body() -> Value {
        json!({
            "id": "greeter",
            "name": "Greeter",
            "description": "Says hi.",
            "nodes": [
                { "id": "start", "kind": "trigger", "name": "Start" },
                { "id": "worker", "kind": "agent", "name": "Worker", "agent": "assistant" },
                { "id": "done", "kind": "output", "name": "Report" }
            ],
            "edges": [
                { "from": "start", "to": "worker" },
                { "from": "worker", "to": "done", "label": "ok" }
            ]
        })
    }

    #[tokio::test]
    async fn create_workflow_tool_then_run_workflow_tool() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));

        // Author the graph.
        let create = CreateWorkflowTool::new(
            company.clone(),
            Some(dir.path().to_path_buf()),
            store.clone(),
            None,
            WorkflowRefQueue::default(),
        );
        let created = create.execute(greeter_body()).await.expect("execute");
        assert!(!created.is_error, "create should succeed: {created:?}");
        assert!(
            created.output_for_llm(true).contains("run_workflow"),
            "{created:?}"
        );

        // It's enabled on the record.
        let record = store.load(&company).await.unwrap().unwrap();
        assert!(
            record
                .manifest
                .workflows
                .enabled
                .contains(&"greeter".to_string())
        );

        // And immediately runnable via the run tool over the same source dir.
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": ["hi"] } } }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let run = RunWorkflowTool::new(
            company.clone(),
            Some(dir.path().to_path_buf()),
            store.clone(),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
        );
        let result = run
            .execute(json!({ "id": "greeter" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "run should succeed: {result:?}");
        assert!(
            result.output_for_llm(true).contains("Greeter"),
            "{result:?}"
        );
    }

    /// Issue #401: the orchestrator's run tool refuses when the company is
    /// already at its in-flight run ceiling. The refusal is a
    /// `ToolResult::error` the agent should treat as "wait / stop one", NOT an
    /// `Err`, and it registers nothing — a held guard stands in for the
    /// in-flight run, so no wall-clock and no real second run is needed.
    #[tokio::test]
    async fn run_workflow_tool_refuses_at_the_in_flight_cap() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));

        // Author a runnable graph on disk so `execute` reaches the cap check.
        let create = CreateWorkflowTool::new(
            company.clone(),
            Some(dir.path().to_path_buf()),
            store.clone(),
            None,
            WorkflowRefQueue::default(),
        );
        assert!(
            !create
                .execute(greeter_body())
                .await
                .expect("execute")
                .is_error
        );

        // A supervisor with room for one run, whose only slot is already taken
        // by a (simulated) in-flight run held for the length of the test.
        let supervisor = crate::runtime::RunSupervisor::with_limit(1);
        let (_ctx, _held) = supervisor
            .begin("greeter", false)
            .expect("the held run fills the cap of 1");

        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({}),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let run = RunWorkflowTool::new(
            company.clone(),
            Some(dir.path().to_path_buf()),
            store.clone(),
            handle,
            supervisor.clone(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
        );

        let result = run
            .execute(json!({ "id": "greeter" }))
            .await
            .expect("execute");
        assert!(result.is_error, "a run over the cap is refused: {result:?}");
        let text = result.output_for_llm(false);
        assert!(
            text.contains("wasn't started") && text.contains("maximum"),
            "the refusal names the cap and is actionable: {text}"
        );
        assert_eq!(
            supervisor.len(),
            1,
            "the refused run registered nothing — only the held run remains"
        );
    }

    /// Issue #339: the *"build us a process for this"* card. The graph is the
    /// deliverable, so authoring it stages a link even though nothing has run —
    /// and when the same turn goes on to run it, the pair collapses to the run,
    /// which is the stronger link because it can show what executed.
    #[tokio::test]
    async fn authoring_a_workflow_stages_a_link_and_running_it_upgrades_it() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let refs = WorkflowRefQueue::default();

        let create = CreateWorkflowTool::new(
            company.clone(),
            Some(dir.path().to_path_buf()),
            store.clone(),
            None,
            refs.clone(),
        );
        assert!(
            !create
                .execute(greeter_body())
                .await
                .expect("execute")
                .is_error
        );
        assert_eq!(refs.queued(), 1, "the saved graph is a deliverable");

        // A rejected draft persists nothing, so it must stage nothing either.
        assert!(
            create
                .execute(json!({ "id": "greeter", "name": "Greeter", "nodes": [] }))
                .await
                .expect("execute")
                .is_error
        );
        assert_eq!(refs.queued(), 1, "a rejected draft is not a deliverable");

        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": ["hi"] } } }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let run = RunWorkflowTool::new(
            company.clone(),
            Some(dir.path().to_path_buf()),
            store.clone(),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            refs.clone(),
            RunOutputCache::default(),
        );
        assert!(
            !run.execute(json!({ "id": "greeter" }))
                .await
                .expect("execute")
                .is_error
        );

        let staged = refs.drain();
        assert_eq!(staged.len(), 1, "one workflow, one link: {staged:?}");
        assert_eq!(staged[0].workflow_id, "greeter");
        assert_eq!(staged[0].action, TaskOutputAction::Ran);
        assert!(staged[0].run_id.is_some());
    }

    #[tokio::test]
    async fn create_workflow_tool_guardrail_failure_is_error_result() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let tool = CreateWorkflowTool::new(
            company,
            Some(dir.path().to_path_buf()),
            store,
            None,
            WorkflowRefQueue::default(),
        );
        // Zero triggers — a guardrail failure must be an is_error ToolResult, not
        // a raised anyhow error.
        let result = tool
            .execute(json!({
                "id": "bad",
                "name": "Bad",
                "nodes": [ { "id": "a", "kind": "output", "name": "A" } ],
                "edges": []
            }))
            .await
            .expect("execute returns a result, not an error");
        assert!(result.is_error, "{result:?}");
        assert!(
            result.output_for_llm(false).contains("trigger"),
            "{result:?}"
        );
    }

    /// Issue #168: a hosted tenant has no source directory, and the tool must
    /// still create — the graph body is persisted on the record. It used to
    /// refuse outright with "nowhere to save".
    #[tokio::test]
    async fn create_workflow_tool_creates_without_source_dir() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let tool = CreateWorkflowTool::new(
            company.clone(),
            None,
            store.clone(),
            None,
            WorkflowRefQueue::default(),
        );
        let result = tool
            .execute(json!({
                "id": "hosted",
                "name": "Hosted",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    { "id": "done", "kind": "output", "name": "Done" }
                ],
                "edges": [ { "from": "start", "to": "done" } ]
            }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{result:?}");

        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1);
        assert_eq!(record.overlay_workflows[0].id, "hosted");

        // And it runs, with no source directory anywhere in the picture.
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": { "done": { "items": ["ok"] } } }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let run = RunWorkflowTool::new(
            company,
            None,
            store,
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
        );
        let result = run
            .execute(json!({ "id": "hosted" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "run should succeed: {result:?}");
        assert!(result.output_for_llm(true).contains("Hosted"), "{result:?}");
    }

    #[tokio::test]
    async fn create_workflow_tool_errors_on_unreadable_args() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn CompanyStore> = Arc::new(MemStore::default());
        let tool = CreateWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            store,
            None,
            WorkflowRefQueue::default(),
        );
        // A non-object payload can't deserialize into the create body.
        let result = tool.execute(json!(42)).await.expect("execute");
        assert!(result.is_error);
        assert!(
            result.output_for_llm(false).contains("Couldn't read"),
            "{result:?}"
        );
    }

    /// Like [`record_with_assistant`], but the company `[tools].allow` grants the
    /// `web` namespace so a `web_fetch` `tool_call` clears the author-time grant
    /// gate under the `openhuman` build (issue #661).
    fn record_granting_web(company: &CompanyId) -> CompanyRecord {
        let mut record = record_with_assistant(company);
        record.manifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[tools]\nallow = [\"web\"]\n[[agent]]\nid = \"assistant\"\nrole = \"Assistant\"\n",
        )
        .expect("valid manifest");
        record
    }

    /// Issue #661 (H1): a `tool_call` node authored with `config.slug` persists
    /// the slug into the saved graph — the tool advertises `tool_call` and can
    /// now actually author a working one. Round-trip proof: the rendered TOML on
    /// the record carries `slug = "web_fetch"`.
    #[tokio::test]
    async fn create_workflow_tool_persists_tool_call_config_slug() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_granting_web(&company)));
        let tool = CreateWorkflowTool::new(
            company.clone(),
            None,
            store.clone(),
            None,
            WorkflowRefQueue::default(),
        );
        let result = tool
            .execute(json!({
                "id": "fetcher",
                "name": "Fetcher",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    {
                        "id": "grab",
                        "kind": "tool_call",
                        "name": "Grab",
                        "config": { "slug": "web_fetch", "args": { "url": "https://example.com" } }
                    },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [
                    { "from": "start", "to": "grab" },
                    { "from": "grab", "to": "done" }
                ]
            }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{result:?}");

        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1);
        assert!(
            record.overlay_workflows[0]
                .toml
                .contains("slug = \"web_fetch\""),
            "the persisted graph carries the tool slug: {}",
            record.overlay_workflows[0].toml
        );
    }

    /// Issue #661 (H1): an `output` node's `destination` flows through into the
    /// saved graph — the persisted TOML carries the routed address.
    #[tokio::test]
    async fn create_workflow_tool_persists_output_destination() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let tool = CreateWorkflowTool::new(
            company.clone(),
            None,
            store.clone(),
            None,
            WorkflowRefQueue::default(),
        );
        let result = tool
            .execute(json!({
                "id": "reporter",
                "name": "Reporter",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    {
                        "id": "done",
                        "kind": "output",
                        "name": "Report",
                        "destination": { "kind": "email", "target": "ada@example.com" }
                    }
                ],
                "edges": [ { "from": "start", "to": "done" } ]
            }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{result:?}");

        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1);
        let saved = &record.overlay_workflows[0].toml;
        assert!(
            saved.contains("target = \"ada@example.com\""),
            "the persisted graph routes to the destination address: {saved}"
        );
    }

    /// Issue #661 (H1): a `tool_call` with no `slug` is still rejected — the
    /// inherited author-time gate, now reachable with a useful message instead of
    /// the tool being unable to author a `tool_call` at all.
    #[tokio::test]
    async fn create_workflow_tool_rejects_tool_call_without_slug() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let tool = CreateWorkflowTool::new(company, None, store, None, WorkflowRefQueue::default());
        let result = tool
            .execute(json!({
                "id": "bad",
                "name": "Bad",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    { "id": "grab", "kind": "tool_call", "name": "Grab" },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [
                    { "from": "start", "to": "grab" },
                    { "from": "grab", "to": "done" }
                ]
            }))
            .await
            .expect("execute");
        assert!(result.is_error, "{result:?}");
        assert!(
            result.output_for_llm(false).contains("slug"),
            "the refusal names the missing slug: {result:?}"
        );
    }

    /// Issue #661 (H1): the exact GitHub/Composio failure mode — a `tool_call`
    /// naming an agent-turn tool family (`composio_execute`) can never run on a
    /// workflow `tool_call` node, so it is refused at save. Gated on `openhuman`
    /// because the namespace resolution (`namespace_of`) lives behind it.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn create_workflow_tool_rejects_agent_turn_tool_call() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let tool = CreateWorkflowTool::new(company, None, store, None, WorkflowRefQueue::default());
        let result = tool
            .execute(json!({
                "id": "gh",
                "name": "GitHub",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    {
                        "id": "call",
                        "kind": "tool_call",
                        "name": "Call",
                        "config": { "slug": "composio_execute" }
                    },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [
                    { "from": "start", "to": "call" },
                    { "from": "call", "to": "done" }
                ]
            }))
            .await
            .expect("execute");
        assert!(result.is_error, "{result:?}");
        assert!(
            result.output_for_llm(false).contains("agent-turn"),
            "the refusal explains it is an agent-turn family, not a workflow tool: {result:?}"
        );
    }

    /// Issue #661 (H1): a JSON `null` inside a node's `config` can't be stored —
    /// TOML has no null — so the fallible `TryFrom` conversion refuses it as an
    /// agent-actionable error, never a panic or a silently-dropped key.
    #[tokio::test]
    async fn create_workflow_tool_rejects_null_config_value() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let tool = CreateWorkflowTool::new(company, None, store, None, WorkflowRefQueue::default());
        let result = tool
            .execute(json!({
                "id": "nullish",
                "name": "Nullish",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    {
                        "id": "grab",
                        "kind": "tool_call",
                        "name": "Grab",
                        "config": { "slug": "web_fetch", "args": { "url": null } }
                    },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [
                    { "from": "start", "to": "grab" },
                    { "from": "grab", "to": "done" }
                ]
            }))
            .await
            .expect("execute");
        assert!(result.is_error, "{result:?}");
        assert!(
            result.output_for_llm(false).contains("TOML has no null"),
            "the refusal explains why the config can't be stored: {result:?}"
        );
    }

    /// Issue #674 boundary: an agent-authored `tool_call` whose `config.args`
    /// carries a templated `=`-expression is rejected — that node would take
    /// saved-node runtime position with model-chosen templated args, collapsing
    /// the two-operator-gate model. The refusal names the node and points at the
    /// console for templated wiring. Feature-independent: the check runs in the
    /// `TryFrom`, before any namespace/grant gate.
    #[tokio::test]
    async fn create_workflow_tool_rejects_tool_call_expression_args() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_granting_web(&company)));
        let tool = CreateWorkflowTool::new(
            company.clone(),
            None,
            store.clone(),
            None,
            WorkflowRefQueue::default(),
        );
        let result = tool
            .execute(json!({
                "id": "templated",
                "name": "Templated",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    {
                        "id": "grab",
                        "kind": "tool_call",
                        "name": "Grab",
                        "config": { "slug": "web_fetch", "args": { "url": "=item.url" } }
                    },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [
                    { "from": "start", "to": "grab" },
                    { "from": "grab", "to": "done" }
                ]
            }))
            .await
            .expect("execute");
        assert!(result.is_error, "{result:?}");
        let msg = result.output_for_llm(false);
        assert!(
            msg.contains("=`-expression") && msg.contains("config.args.url"),
            "the refusal names the templated expression and its location: {result:?}"
        );
        // Nothing was persisted — the reject happens before the store write.
        let record = store.load(&company).await.unwrap().unwrap();
        assert!(
            record.overlay_workflows.is_empty(),
            "a rejected draft persists nothing"
        );
    }

    /// Issue #674 boundary, positive half: the same `tool_call` with a LITERAL
    /// arg (no `=` prefix) persists — the restriction is on templated
    /// `=`-expressions, not on args as such.
    #[tokio::test]
    async fn create_workflow_tool_persists_tool_call_literal_args() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_granting_web(&company)));
        let tool = CreateWorkflowTool::new(
            company.clone(),
            None,
            store.clone(),
            None,
            WorkflowRefQueue::default(),
        );
        let result = tool
            .execute(json!({
                "id": "literal",
                "name": "Literal",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    {
                        "id": "grab",
                        "kind": "tool_call",
                        "name": "Grab",
                        "config": { "slug": "web_fetch", "args": { "url": "https://example.com" } }
                    },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [
                    { "from": "start", "to": "grab" },
                    { "from": "grab", "to": "done" }
                ]
            }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{result:?}");
        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1);
        assert!(
            record.overlay_workflows[0]
                .toml
                .contains("url = \"https://example.com\""),
            "the persisted graph carries the literal arg: {}",
            record.overlay_workflows[0].toml
        );
    }

    /// Issue #661 (H1): the `=`-expression restriction is scoped to `tool_call`.
    /// A `condition` node legitimately branches on a `config.field` expression,
    /// so a `=`-prefixed field must NOT be rejected — proving the guard doesn't
    /// over-reach into the kinds that resolve expressions by design.
    #[tokio::test]
    async fn create_workflow_tool_allows_condition_expression_field() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let tool = CreateWorkflowTool::new(
            company.clone(),
            None,
            store.clone(),
            None,
            WorkflowRefQueue::default(),
        );
        let result = tool
            .execute(json!({
                "id": "brancher",
                "name": "Brancher",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    {
                        "id": "check",
                        "kind": "condition",
                        "name": "Check",
                        "config": { "field": "=item.ok" }
                    },
                    { "id": "yes", "kind": "output", "name": "Yes" },
                    { "id": "no", "kind": "output", "name": "No" }
                ],
                "edges": [
                    { "from": "start", "to": "check" },
                    { "from": "check", "to": "yes", "label": "yes" },
                    { "from": "check", "to": "no", "label": "no" }
                ]
            }))
            .await
            .expect("execute");
        assert!(
            !result.is_error,
            "a condition's `=`-expression field is allowed: {result:?}"
        );
    }

    /// Issue #661 (H1): a non-object `config` (here a bare string on an
    /// `http_request` node — the path that would otherwise persist silently) is
    /// refused with an agent-actionable message, not saved as an inert TOML
    /// scalar.
    #[tokio::test]
    async fn create_workflow_tool_rejects_non_object_config() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let tool = CreateWorkflowTool::new(company, None, store, None, WorkflowRefQueue::default());
        let result = tool
            .execute(json!({
                "id": "scalar",
                "name": "Scalar",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    {
                        "id": "call",
                        "kind": "http_request",
                        "name": "Call",
                        "config": "GET https://example.com"
                    },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [
                    { "from": "start", "to": "call" },
                    { "from": "call", "to": "done" }
                ]
            }))
            .await
            .expect("execute");
        assert!(result.is_error, "{result:?}");
        assert!(
            result.output_for_llm(false).contains("non-object `config`"),
            "the refusal explains config must be a JSON object: {result:?}"
        );
    }

    /// Issue #661 (H1) — item #2: a `destination` on a non-`output` node is
    /// already rejected end-to-end by the shared `validate` (`render_workflow` →
    /// `parse_workflow` inside `create_company_workflow`), so the create_workflow
    /// tool inherits the catch with no duplicated validation of its own. This
    /// pins that end-to-end behaviour; the shared-validator hardening is #682's.
    #[tokio::test]
    async fn create_workflow_tool_rejects_destination_on_non_output() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let tool = CreateWorkflowTool::new(company, None, store, None, WorkflowRefQueue::default());
        let result = tool
            .execute(json!({
                "id": "misrouted",
                "name": "Misrouted",
                "nodes": [
                    {
                        "id": "start",
                        "kind": "trigger",
                        "name": "Start",
                        "destination": { "kind": "owner" }
                    },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [ { "from": "start", "to": "done" } ]
            }))
            .await
            .expect("execute");
        assert!(result.is_error, "{result:?}");
        assert!(
            result
                .output_for_llm(false)
                .contains("only `output` nodes route a report"),
            "the shared validator's destination-placement message surfaces: {result:?}"
        );
    }

    /// Issue #661 (H1) — item #3: the JSON→TOML conversion remedy is conditional.
    /// A failure that is NOT about a null (here a `u64` beyond `i64` range) must
    /// get the converter's own message WITHOUT the misleading "TOML has no null"
    /// hint — the null case keeps that hint (`create_workflow_tool_rejects_null_config_value`).
    #[tokio::test]
    async fn create_workflow_tool_non_null_conversion_error_omits_null_hint() {
        let company = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(record_with_assistant(&company)));
        let tool = CreateWorkflowTool::new(company, None, store, None, WorkflowRefQueue::default());
        let result = tool
            .execute(json!({
                "id": "toobig",
                "name": "TooBig",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    {
                        "id": "grab",
                        "kind": "tool_call",
                        "name": "Grab",
                        "config": { "slug": "web_fetch", "args": { "n": 18446744073709551615u64 } }
                    },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [
                    { "from": "start", "to": "grab" },
                    { "from": "grab", "to": "done" }
                ]
            }))
            .await
            .expect("execute");
        assert!(result.is_error, "{result:?}");
        let msg = result.output_for_llm(false);
        assert!(
            msg.contains("can't be stored"),
            "names the failure: {result:?}"
        );
        assert!(
            !msg.contains("TOML has no null"),
            "a non-null conversion failure must not misdirect to the null remedy: {result:?}"
        );
    }

    #[test]
    fn first_expression_location_walks_nested_config() {
        // Matches tinyflows' `is_expression`: a leading `=` (no trim).
        assert_eq!(
            first_expression_location(&json!({ "args": { "command": "=item.x" } }), ""),
            Some("args.command".to_string())
        );
        // Array elements become numeric segments.
        assert_eq!(
            first_expression_location(&json!({ "args": { "cc": ["a", "=item.y"] } }), ""),
            Some("args.cc.1".to_string())
        );
        // Literals — including a `=` in the MIDDLE — are not expressions.
        assert_eq!(
            first_expression_location(&json!({ "args": { "q": "a=b", "s": "ls -la" } }), ""),
            None
        );
    }

    #[test]
    fn json_contains_null_is_recursive() {
        assert!(json_contains_null(&json!({ "args": { "url": null } })));
        assert!(json_contains_null(&json!(["ok", [null]])));
        assert!(!json_contains_null(
            &json!({ "args": { "url": "https://x" } })
        ));
    }

    // ---- read_run_output (issue #418) ----

    /// Builds a `RunWorkflowTool` over the demo graph in `dir`, a stub runner
    /// returning `run`, and the given caches — the shared setup the round-trip
    /// tests need.
    /// Returns the tool **and** the runner `Arc` — the handle keeps only a weak
    /// reference, so the caller must hold the returned runner alive for the
    /// duration of the test or the run tool reports "no runner wired".
    fn run_tool_over(
        dir: &std::path::Path,
        run: WorkflowRun,
        refs: WorkflowRefQueue,
        cache: RunOutputCache,
    ) -> (RunWorkflowTool, Arc<dyn WorkflowRunner>) {
        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(run));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            refs,
            cache,
        );
        (tool, runner)
    }

    /// T1: a clipped preview names exactly how many characters it dropped, and
    /// counts them in `chars()` — so a multibyte string past the boundary
    /// reports codepoints dropped, never bytes, and never panics on a byte
    /// index that lands mid-character.
    #[test]
    fn preview_marks_the_exact_dropped_char_count_including_multibyte() {
        // 130 ASCII chars → 120 kept, 10 dropped.
        let ascii = "a".repeat(130);
        let preview = preview_item(&json!(ascii));
        assert!(preview.ends_with("… (+10 chars)"), "{preview}");
        assert!(preview.starts_with(&"a".repeat(120)), "{preview}");
        // 120 kept chars, then the '…' and the marker — the kept body is exactly
        // the cap, not one over.
        assert_eq!(preview.chars().take_while(|c| *c == 'a').count(), 120);

        // A multibyte fill: 130 'é' (2 bytes each). The marker must count the 10
        // dropped *characters*, not their 20 bytes, and the boundary must not
        // split a codepoint.
        let multibyte = "é".repeat(130);
        let preview = preview_item(&json!(multibyte));
        assert!(preview.ends_with("… (+10 chars)"), "{preview}");
        assert_eq!(preview.chars().take_while(|c| *c == 'é').count(), 120);

        // At or below the cap there is no marker at all.
        let short = "x".repeat(ITEM_PREVIEW_CHARS);
        assert_eq!(preview_item(&json!(short)), short);
    }

    /// T2: a node with more than one item is labelled `last of N items`, and the
    /// summary footer names the companion tool via the `READ_RUN_OUTPUT_TOOL`
    /// const (so wording can't drift) and embeds the run id.
    #[test]
    fn summary_labels_multi_item_nodes_and_footers_the_companion() {
        let file = crate::company::parse_workflow(DEMO_WF).unwrap();
        let run = WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": ["first", "second", "third"] } } }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
        };
        let md = summarize_run(&file, &run, "run-xyz", RunOutputStored::Stored);
        assert!(md.contains("last of 3 items — third"), "{md}");
        assert!(md.contains(READ_RUN_OUTPUT_TOOL), "{md}");
        assert!(md.contains("run-xyz"), "{md}");

        // A single-item node keeps the plain "1 item(s)" phrasing.
        let run_one = WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": ["only"] } } }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
        };
        let md = summarize_run(&file, &run_one, "run-1", RunOutputStored::Stored);
        assert!(md.contains("1 item(s) — only"), "{md}");
        assert!(!md.contains("last of"), "{md}");

        // The oversized footer sends the agent to the console run drawer instead
        // of to a `read_run_output` call that would find nothing cached.
        let md = summarize_run(
            &file,
            &run,
            "run-big",
            RunOutputStored::Oversized { bytes: 999 },
        );
        assert!(md.contains("console"), "{md}");
        assert!(md.contains("run drawer"), "{md}");
        assert!(md.contains("999 bytes"), "{md}");
        assert!(!md.contains("Read any node's full output"), "{md}");
    }

    /// T3: a successful run populates the cache and the tool's JSON payload
    /// carries the run id; a cancelled or failed run stores nothing.
    #[tokio::test]
    async fn success_populates_cache_and_payload_carries_run_id() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let cache = RunOutputCache::default();
        let (ok, _runner) = run_tool_over(
            dir.path(),
            WorkflowRun {
                output: json!({ "nodes": { "worker": { "items": ["did the thing"] } } }),
                pending_approvals: Vec::new(),
                deliveries: Vec::new(),
                cancelled: false,
                nodes: Vec::new(),
            },
            WorkflowRefQueue::default(),
            cache.clone(),
        );
        let result = ok.execute(json!({ "id": "demo" })).await.expect("execute");
        assert!(!result.is_error, "{result:?}");
        assert_eq!(cache.len(), 1, "a successful run must be cached");
        // The payload the brain sees carries a run id string.
        let payload = match &result.content[0] {
            openhuman_core::openhuman::skills::types::ToolContent::Json { data } => data.clone(),
            other => panic!("expected JSON payload, got {other:?}"),
        };
        assert!(
            payload.get("run_id").and_then(Value::as_str).is_some(),
            "{payload}"
        );

        // A cancelled run caches nothing.
        let cancel_cache = RunOutputCache::default();
        let (cancelled, _cancel_runner) = run_tool_over(
            dir.path(),
            WorkflowRun {
                output: json!({ "nodes": {} }),
                pending_approvals: Vec::new(),
                deliveries: Vec::new(),
                cancelled: true,
                nodes: Vec::new(),
            },
            WorkflowRefQueue::default(),
            cancel_cache.clone(),
        );
        assert!(
            cancelled
                .execute(json!({ "id": "demo" }))
                .await
                .expect("execute")
                .is_error
        );
        assert_eq!(cancel_cache.len(), 0, "a cancelled run stores nothing");
    }

    /// T4: the full agent round-trip. A run whose node holds multiple >120-char
    /// items is summarised (clipping the preview), then `read_run_output` over
    /// the shared cache returns every item — including every character the
    /// preview dropped — verbatim.
    #[tokio::test]
    async fn read_run_output_returns_every_dropped_char_after_a_run() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let long_a = "A".repeat(400);
        let long_b = format!("B{}", "b".repeat(500));
        let cache = RunOutputCache::default();
        let (run, _runner) = run_tool_over(
            dir.path(),
            WorkflowRun {
                output: json!({ "nodes": { "worker": { "items": [long_a, long_b] } } }),
                pending_approvals: Vec::new(),
                deliveries: Vec::new(),
                cancelled: false,
                nodes: Vec::new(),
            },
            WorkflowRefQueue::default(),
            cache.clone(),
        );
        let summary = run.execute(json!({ "id": "demo" })).await.expect("execute");
        // The summary only previews the last item, clipped.
        assert!(
            summary.output_for_llm(true).contains("last of 2 items"),
            "{summary:?}"
        );

        let reader = ReadRunOutputTool::new(CompanyId::new("acme"), cache);
        // Pass the display name "Worker" — the case-insensitive fallback resolves
        // it to id `worker`.
        let read = reader
            .execute(json!({ "run_id": "", "node": "Worker" }))
            .await
            .expect("execute");
        // Empty run_id is rejected before lookup.
        assert!(read.is_error, "empty run_id must be rejected");

        // Read with the real run id (recover it from the payload).
        let payload = match &summary.content[0] {
            openhuman_core::openhuman::skills::types::ToolContent::Json { data } => data.clone(),
            other => panic!("{other:?}"),
        };
        let run_id = payload.get("run_id").and_then(Value::as_str).unwrap();
        let read = reader
            .execute(json!({ "run_id": run_id, "node": "Worker" }))
            .await
            .expect("execute");
        assert!(!read.is_error, "{read:?}");
        let text = read.output_for_llm(false);
        assert!(text.contains("Item 1 of 2:"), "{text}");
        assert!(text.contains("Item 2 of 2:"), "{text}");
        assert!(text.contains(&"A".repeat(400)), "item 1 must be verbatim");
        assert!(text.contains(&"b".repeat(500)), "item 2 must be verbatim");
    }

    /// T4b: the run summary lists a node by its display name but the cache is
    /// keyed by id, and in `DEMO_WF` the two genuinely differ — `id = "done"`,
    /// `name = "Report"`. Both the display name the summary prints ("Report")
    /// and the raw id ("done") must resolve through `read_run_output`. This is
    /// the non-degenerate name/id pair the case-only fallback never covered.
    #[tokio::test]
    async fn read_run_output_resolves_display_name_and_id() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let cache = RunOutputCache::default();
        let (run, _runner) = run_tool_over(
            dir.path(),
            WorkflowRun {
                // The terminal node's id is `done`; the summary shows its name,
                // "Report".
                output: json!({ "nodes": { "done": { "items": ["the report body"] } } }),
                pending_approvals: Vec::new(),
                deliveries: Vec::new(),
                cancelled: false,
                nodes: Vec::new(),
            },
            WorkflowRefQueue::default(),
            cache.clone(),
        );
        let summary = run.execute(json!({ "id": "demo" })).await.expect("execute");
        // The summary prints the display name and now the id alongside it.
        let md = summary.output_for_llm(true);
        assert!(md.contains("**Report**"), "{md}");
        assert!(md.contains("`done`"), "{md}");
        let run_id = match &summary.content[0] {
            openhuman_core::openhuman::skills::types::ToolContent::Json { data } => data
                .get("run_id")
                .and_then(Value::as_str)
                .unwrap()
                .to_string(),
            other => panic!("{other:?}"),
        };
        let reader = ReadRunOutputTool::new(CompanyId::new("acme"), cache);

        // The display name from the summary resolves to id `done`.
        let by_name = reader
            .execute(json!({ "run_id": run_id, "node": "Report" }))
            .await
            .expect("execute");
        assert!(!by_name.is_error, "display name must resolve: {by_name:?}");
        assert!(
            by_name.output_for_llm(false).contains("the report body"),
            "{by_name:?}"
        );

        // The raw id resolves too, so both paths are live.
        let by_id = reader
            .execute(json!({ "run_id": run_id, "node": "done" }))
            .await
            .expect("execute");
        assert!(!by_id.is_error, "id must resolve: {by_id:?}");
        assert!(
            by_id.output_for_llm(false).contains("the report body"),
            "{by_id:?}"
        );
    }

    /// T5: paging. Two windows concatenate to the original, each page stays
    /// within budget, and a boundary that lands on a multibyte char never
    /// splits a codepoint.
    #[test]
    fn paging_reassembles_and_never_splits_a_codepoint() {
        // 300 'é' (2 bytes each) = 600 bytes. A 401-byte budget forces a break
        // right where the next 'é' would cross it — proving whole-char taking.
        let full: String = "é".repeat(300);
        let budget = 401;
        let (p1, next) = page_run_output(&full, 0, budget);
        let n1 = next.expect("more remains");
        assert!(p1.len() <= budget, "page 1 is {} bytes", p1.len());
        // A page of whole 'é' has even byte length — never an odd split.
        assert_eq!(p1.len() % 2, 0, "a split codepoint would make this odd");
        let (p2, next2) = page_run_output(&full, n1, budget);
        assert!(p2.len() <= budget, "page 2 is {} bytes", p2.len());
        // Continue to the end and prove the concatenation reconstructs the whole.
        let mut assembled = p1.clone();
        assembled.push_str(&p2);
        let mut off = next2;
        while let Some(o) = off {
            let (p, nxt) = page_run_output(&full, o, budget);
            assert!(p.len() <= budget);
            assembled.push_str(&p);
            off = nxt;
        }
        assert_eq!(assembled, full, "the pages must reassemble the original");

        // Every char is valid UTF-8 by construction (String), so decoding the
        // reassembly back is lossless.
        assert_eq!(assembled.chars().count(), 300);
    }

    /// T5b: the tool's own paging clips a huge single item under the budget and
    /// hands back an offset that reads the remainder.
    #[tokio::test]
    async fn read_run_output_pages_a_huge_item_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        // One item far larger than the 16 KiB tool-result budget.
        let huge = "z".repeat(40_000);
        let cache = RunOutputCache::default();
        cache.store(
            "run-huge",
            "demo",
            json!({ "worker": { "items": [huge] } }),
            Vec::new(),
        );
        let _ = dir;
        let reader = ReadRunOutputTool::new(CompanyId::new("acme"), cache);

        let first = reader
            .execute(json!({ "run_id": "run-huge", "node": "worker" }))
            .await
            .expect("execute");
        assert!(!first.is_error, "{first:?}");
        let text = first.output_for_llm(false);
        assert!(
            text.len() <= crate::harness::build::TOOL_RESULT_BUDGET_BYTES,
            "page too big"
        );
        assert!(text.contains("Continue with offset="), "{text}");
        // Pull the offset out and read the next page.
        let off: usize = text
            .rsplit("offset=")
            .next()
            .and_then(|t| t.trim_end_matches('.').parse().ok())
            .expect("an offset to continue from");
        let second = reader
            .execute(json!({ "run_id": "run-huge", "node": "worker", "offset": off }))
            .await
            .expect("execute");
        assert!(!second.is_error, "{second:?}");
        assert!(
            off > 0 && off < 40_100,
            "offset {off} advances into the item"
        );
    }

    /// T6: the error arms are actionable — unknown run names the console
    /// fallback, unknown node lists the valid ids with item counts, and an empty
    /// node says so rather than returning nothing.
    #[tokio::test]
    async fn read_run_output_error_arms_are_actionable() {
        let cache = RunOutputCache::default();
        cache.store(
            "run-1",
            "demo",
            json!({
                "worker": { "items": ["one", "two"] },
                "done": { "items": [] }
            }),
            Vec::new(),
        );
        let reader = ReadRunOutputTool::new(CompanyId::new("acme"), cache);

        // Unknown run → names the cache scope + the console fallback.
        let unknown_run = reader
            .execute(json!({ "run_id": "ghost", "node": "worker" }))
            .await
            .expect("execute");
        assert!(unknown_run.is_error);
        let t = unknown_run.output_for_llm(false);
        assert!(t.contains("console"), "{t}");

        // Unknown node → lists valid ids + counts.
        let unknown_node = reader
            .execute(json!({ "run_id": "run-1", "node": "nope" }))
            .await
            .expect("execute");
        assert!(unknown_node.is_error);
        let t = unknown_node.output_for_llm(false);
        assert!(t.contains("`worker` (2 item(s))"), "{t}");
        assert!(t.contains("`done` (0 item(s))"), "{t}");

        // Empty node → a success that says it is empty, not an error, not silence.
        let empty = reader
            .execute(json!({ "run_id": "run-1", "node": "done" }))
            .await
            .expect("execute");
        assert!(!empty.is_error, "{empty:?}");
        assert!(
            empty.output_for_llm(false).contains("no items"),
            "{empty:?}"
        );
    }

    /// T7: eviction (oldest run drops past the run-count bound) and the
    /// oversized-run announce (a run over the hard per-run ceiling is refused,
    /// reported as `Oversized`, and never cached).
    #[test]
    fn cache_evicts_oldest_and_refuses_an_oversized_run() {
        let cache = RunOutputCache::default();
        for i in 0..(RUN_OUTPUT_CACHE_RUNS + 3) {
            let outcome = cache.store(
                &format!("run-{i}"),
                "demo",
                json!({ "worker": { "items": [format!("item-{i}")] } }),
                Vec::new(),
            );
            assert!(matches!(outcome, RunOutputStored::Stored));
        }
        assert_eq!(cache.len(), RUN_OUTPUT_CACHE_RUNS, "bounded to the run cap");
        // The three oldest runs were evicted; the newest survive.
        assert!(cache.get("run-0").is_none(), "oldest must be evicted");
        assert!(
            cache
                .get(&format!("run-{}", RUN_OUTPUT_CACHE_RUNS + 2))
                .is_some(),
            "newest must survive"
        );

        // A run whose node map serializes past the hard per-run ceiling is
        // refused, announced (not silently dropped), and never cached.
        let fresh = RunOutputCache::default();
        let giant = "g".repeat(RUN_OUTPUT_ENTRY_MAX_BYTES + 1);
        let outcome = fresh.store(
            "run-giant",
            "demo",
            json!({ "worker": { "items": [giant] } }),
            Vec::new(),
        );
        assert!(
            matches!(outcome, RunOutputStored::Oversized { .. }),
            "must refuse"
        );
        assert_eq!(fresh.len(), 0, "an oversized run must not be cached");
        assert!(fresh.get("run-giant").is_none());
    }
}
