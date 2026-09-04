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
//! It reaches sixteen tools, all wired only onto the orchestrator agent:
//!
//! * [`QueryCompanyTool`] — a read surface over the company's [`FactStore`],
//!   recent [`EventLog`] history, and (issue #1859) a `## Board` summary of
//!   open task cards.
//! * [`ListTasksTool`] / [`ReadTaskTool`] / [`ReadRunTool`] (issue #1859) —
//!   the execution-state read trio: `list_tasks` answers "what are you
//!   working on?" with real cards and statuses, `read_task` reads one card's
//!   full attempt history and output, and `read_run` reads one recorded
//!   run's outcome (an agent attempt, or a workflow run folded out of the
//!   journal). Where the board tools below are write-only, this trio is how
//!   an agent reads back what it — or the board — already did.
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

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::ports::store::company_write_lock;

use async_trait::async_trait;
// Issue #1865: `.catch_unwind()` on the `run_workflow` tool's runner call —
// see the call site in `RunWorkflowTool::execute` for why this path needs its
// own catch rather than routing through `WorkflowSpawn`.
use futures::future::FutureExt;
use serde_json::{Value, json};

use openhuman_core::openhuman as oh;

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};

use crate::company::{
    Agent as ManifestAgent, RawEdge, RawNode, RawWorkflow, WorkflowDestinationDef, WorkflowFile,
    WorkflowNodeKind, create_company_workflow, list_workflows_with_globals,
    load_workflow_with_globals,
};
use crate::error::OpenCompanyError;
use crate::harness::lifecycle::ReviewDecision;
use crate::harness::workflow_refs::WorkflowRefQueue;
use crate::ports::artifacts::ArtifactStore;
use crate::ports::events::EventLog;
use crate::ports::facts::FactStore;
use crate::ports::notifications::NotificationStore;
use crate::ports::runs::{RunFilter, RunRecord, RunStore};
use crate::ports::tasks::{
    BOARD_COLUMNS, COLUMN_DONE, TaskOutput, TaskOutputAction, TaskOutputWorkflow, TaskRecord,
    TaskStore, column_label, is_board_column,
};
use crate::ports::types::{
    CompanyEvent, CompanyId, EventSeq, OnboardingStep, OverlayAgent, WorkflowNodeStatus,
};
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

/// The depth argument passed by the delegations the chain bound does not apply
/// to (issue #176).
///
/// [`DelegationQueue::push_within_cap`] gates on depth only for a
/// [`Delegation::DelegateToDesk`] — the one delegation that runs another
/// synchronous turn and can therefore multiply. A board write passes a bound it
/// can never reach, rather than a plausible-looking real number that would
/// quietly start mattering if the gate were widened.
const NO_DEPTH_BOUND: usize = usize::MAX;

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
pub use crate::runtime::delegation_tools::{
    DELEGATE_TO_DESK_TOOL, DELEGATE_TO_TEAMMATE_TOOL, SPAWN_TASK_TOOL,
};
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
/// The `list_tasks` tool name (issue #1859 — execution-state read surface).
pub const LIST_TASKS_TOOL: &str = "list_tasks";
/// The `read_task` tool name (issue #1859).
pub const READ_TASK_TOOL: &str = "read_task";
/// The `read_run` tool name (issue #1859).
pub const READ_RUN_TOOL: &str = "read_run";
/// How many cards [`ListTasksTool`] renders before truncating with an honest
/// marker (issue #1859) — the same silent-cut discipline
/// [`QueryCompanyTool`]'s `FACT_LIMIT` already applies: a company with more
/// open cards than this must not read as though nothing is happening past
/// card N.
const LIST_TASKS_LIMIT: usize = 40;

/// `read_task`'s cap on rendered attempt rows (issue #1859's follow-up
/// review). A card retried many times — especially with failed attempts
/// carrying a 200-char error preview — can otherwise fill the whole
/// `TOOL_RESULT_BUDGET_BYTES` before the `## Output` section that answers
/// what the task actually produced ever renders. Bounded to the newest rows,
/// which are the ones a "why isn't this done" question is about, with an
/// honest count of what was cut.
const READ_TASK_ATTEMPTS_LIMIT: usize = 10;

/// `read_task`'s cap on the rendered card title (issue #1859's follow-up
/// review). The task-edit PATCH route persists an operator-pasted title
/// verbatim and without a length limit; an unusually long one can otherwise
/// consume `TOOL_RESULT_BUDGET_BYTES` before the `## Attempts` or `## Output`
/// sections are reached, and `read_task` has no paging mechanism to recover
/// them on a repeat call.
const READ_TASK_TITLE_LIMIT: usize = 200;

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
        // Issue #884: not optional. This predicate is what keeps a hand-off
        // classified as internal work rather than an external effect to park —
        // and, downstream, what keeps the new edge inside the loop checks
        // everything else on this seam already passes through.
        || tool == DELEGATE_TO_TEAMMATE_TOOL
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
with `delegate_to_desk`, naming the desk by an id `query_company` lists under Desks; when it names \
one PERSON, hand it to them with `delegate_to_teammate`, naming them by a roster id `query_company` \
lists under Team — a desk id is not a person and a person is not a desk, so pick the tool that \
matches the target; when it is yours to answer, answer it. (2) SHOULD THIS BE TRACKED: you do not have to decide this, and you must not pick a \
tool in order to influence it. Anything substantial handed to a desk is opened as a board card \
automatically, and so is anything substantial an operator asks a desk or teammate directly — the \
hand-off IS the card, so never call `spawn_task` alongside a `delegate_to_desk` for the same work, \
and never prefer one over the other to get something tracked. Reach for `spawn_task` only for work \
that belongs on the board but must NOT start in this turn: something for later, or for somebody \
else. Work that is waiting on a PERSON is not a card — a card notifies nobody and resumes \
nothing. When you cannot proceed without something only the operator can give you, call \
`escalate_to_human` with the question; the work parks and their answer restarts it. \
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
    /// Hand a turn to a **named teammate** rather than to whoever leads their
    /// desk (issue #884).
    ///
    /// Everything else about it is [`DelegateToDesk`](Self::DelegateToDesk): it
    /// runs one synchronous turn, opens the same hand-off card, folds the same
    /// [`DeskReply`](crate::runtime::delegation::DeskReply) back for the relay,
    /// and passes the same depth cap. Only the resolution differs — a roster id
    /// straight to that agent, instead of a desk key through
    /// [`desk_lead`](crate::runtime::delegation_tools::desk_lead) — which is the
    /// whole of what D1 was missing.
    DelegateToTeammate {
        /// The teammate's **canonical** roster id, resolved and validated at
        /// the tool boundary (#1162 — before it, this carried the key exactly
        /// as the model typed it, and the drain had to resolve it a second
        /// time). The one exception is the fail-open path, where the record
        /// could not be read at all: nothing was refused there and nothing was
        /// canonicalised, so the drain resolves it with the same resolver.
        teammate: String,
        /// The instruction handed to that teammate.
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
    ///
    /// [`DelegateToTeammate`](Self::DelegateToTeammate) is (issue #884), for
    /// exactly the reason `DelegateToDesk` is: "what did the SEO specialist find?"
    /// is unanswerable without running their turn.
    pub fn answers(&self) -> bool {
        matches!(
            self,
            Self::DelegateToDesk { .. } | Self::DelegateToTeammate { .. }
        )
    }

    /// Whether this delegation is one a **workflow run** may perform
    /// ([`DrainClaim::Board`], issue #661).
    ///
    /// [`SpawnTask`](Self::SpawnTask) and [`AssignTask`](Self::AssignTask) are:
    /// they open a card in To-do and set who owns one, and neither moves a card
    /// between columns nor needs anywhere to put a reply.
    ///
    /// [`ReviewTask`](Self::ReviewTask) and
    /// [`DelegateToDesk`](Self::DelegateToDesk) are not, for two unrelated
    /// reasons that [`no_drain`] states separately rather than collapsing:
    /// `review_task`'s `in_review → done` is the operator's accept lane, and a
    /// hand-off's only value is a synchronous reply that a run has nowhere to
    /// land.
    ///
    /// [`DelegateToTeammate`](Self::DelegateToTeammate) is not either, on the
    /// same ground as `DelegateToDesk`: a run has nowhere to put a synchronous
    /// reply (issue #884).
    ///
    /// This is [`answers`](Self::answers) inverted, and deliberately not
    /// written as `!self.answers()`: the two partitions agree today only by
    /// coincidence, and a further variant would have to be classified for each
    /// question on its own terms.
    pub fn writes_board_only(&self) -> bool {
        matches!(self, Self::SpawnTask { .. } | Self::AssignTask { .. })
    }
}

/// Which claimant a queued delegation belongs to (issue #661).
///
/// The queue handle is one per company and cannot be otherwise — see
/// [`DelegationQueue`] — so the separation between concurrent claimants lives
/// here, in the key, rather than in separate queues. Exactly the shape
/// [`ApprovalScope`](crate::harness::policy::ApprovalScope) took for the same
/// race one queue over (issue #439), and deliberately so: two identical
/// solutions to one problem are worth more than two clever ones.
///
/// # Not to be confused with the scope *chain*
///
/// [`DelegationQueue::scope_chain`] and [`ScopeGuard`] (issue #176) also say
/// "scope", and mean something else entirely: the stack of desk ids a hand-off
/// is currently nested through, whose length **is** the delegation depth. That
/// chain is per-claimant state like everything else here — each scope gets its
/// own — but a `DelegationScope` is *which claimant*, never *how deep*.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DelegationScope {
    /// A delegation staged outside any run-scoped claim.
    ///
    /// The default, and every chat and task path lands here: `run_cycle`, the
    /// operator-message turn and its relay, and a dispatched card all serialise
    /// under the cycle lock, so one bucket is all they have ever needed and
    /// their behaviour is unchanged by this scoping.
    ///
    /// Deliberately **not** an error, for the same reason
    /// [`ApprovalScope::Unscoped`](crate::harness::policy::ApprovalScope)
    /// is not: a claimant added later that forgets to name a scope degrades to
    /// today's behaviour rather than to a silently dropped delegation.
    #[default]
    Unscoped,
    /// One workflow run, keyed by its run id.
    ///
    /// Workflow runs are `tokio::spawn`ed and are **not** under the cycle lock —
    /// several genuinely overlap, bounded only by the #401 in-flight cap. That
    /// is the concurrency this scoping exists for.
    Run(String),
}

tokio::task_local! {
    /// The scope this task's delegation calls file into, installed for the
    /// duration of a claim by [`DelegationClaim::scoped`] (issue #661).
    ///
    /// # Why ambient, and why that is sound here
    ///
    /// The delegation tools are constructed with a queue handle and nothing
    /// else, and they are wired **statically**: belts are cached per roster by
    /// [`HarnessPool::ensure`](crate::harness::HarnessPool::ensure) and rebuilt
    /// rarely, so a tool cannot be handed a run id at call time — the same
    /// constraint that put the #176 depth chain on this queue rather than in a
    /// parameter.
    ///
    /// A task-local is sound on this path because a turn does not leave its
    /// task, and this is not a new dependency: the queue's neighbours
    /// ([`ApprovalRequestQueue`](crate::harness::policy::ApprovalRequestQueue))
    /// and `with_stop_hooks` are already task-local scopes over these same
    /// turns.
    static CURRENT_SCOPE: DelegationScope;
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
    inner: Arc<Mutex<BTreeMap<DelegationScope, Vec<Delegation>>>>,
    /// What the live claim on each scope's bucket permits (issues #453, #267,
    /// #661).
    ///
    /// A scope with no entry is [`DrainClaim::Unclaimed`] — nothing drains —
    /// which keeps the pre-#661 default and its fail-safe direction: a claimant
    /// that has not claimed stages nothing, and now cannot be *un*-claimed by a
    /// concurrent one either.
    committed: Arc<Mutex<BTreeMap<DelegationScope, DrainClaim>>>,
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
    ///
    /// Bucketed per [`DelegationScope`] since issue #661, and this field is why
    /// that issue is a **live** defect rather than a latent one:
    /// [`push_refusal`](Self::push_refusal) is called by `DelegateToDeskTool`
    /// *before* the claim is consulted, so an ungrounded hand-off from a
    /// concurrently-running workflow node already lands here today — and a chat
    /// turn's [`drain_refusals`](Self::drain_refusals) would take it, record it
    /// on its own card, and clear it.
    refused: Arc<Mutex<BTreeMap<DelegationScope, Vec<String>>>>,
    /// The **scope chain**: the resolved desk ids of the hand-offs currently
    /// being executed, outermost first (issue #176).
    ///
    /// Depth **is** `scope.len()` — there is no counter beside it to fall out of
    /// step. Empty while the orchestrator's own turn runs (depth 0); one entry
    /// while a desk lead the orchestrator handed work to runs (depth 1); two
    /// while that lead's own delegate runs (depth 2).
    ///
    /// It lives on the queue for the same reason [`refused`](Self::refused)
    /// does, and for one more. Belts are cached per roster
    /// ([`HarnessPool::ensure`](crate::harness::HarnessPool::ensure)) and rebuilt
    /// rarely, so a member's tools are wired **statically** — the queue handle
    /// they were constructed with is the only shared state they can reach at
    /// call time. Putting depth anywhere else (the message context, the task
    /// record, the runner) would put it somewhere the member's own tool cannot
    /// see it.
    ///
    /// Deliberately **not** touched by [`clear`](Self::clear): clearing runs
    /// between delegations *inside* a scope, and dropping the chain there would
    /// reset the depth of a chain that is still running.
    ///
    /// # Bucketed, and why depth is unaffected (issue #661)
    ///
    /// Renamed from `scope` to `chains` when it became a map, because "the
    /// scope of the scope" was about to mean two things: the key is a
    /// [`DelegationScope`] (*which claimant*), the value is that claimant's own
    /// #176 chain (*how deep it is nested*).
    ///
    /// Depth accounting is untouched by the bucketing. Depth still **is**
    /// `chain.len()`, still has no counter beside it, and is still read and
    /// written only within one claimant's own bucket — so a concurrent run can
    /// neither deepen nor shallow another's chain. Every existing caller is
    /// [`DelegationScope::Unscoped`], where this is one `Vec` under one key and
    /// therefore byte-for-byte the pre-#661 structure.
    chains: Arc<Mutex<BTreeMap<DelegationScope, Vec<String>>>>,
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
            .entry(Self::current_scope())
            .or_default()
            .push(delegation);
    }

    /// The [`DelegationScope`] the calling task is running under (issue #661).
    ///
    /// [`DelegationScope::Unscoped`] outside any
    /// [`DelegationClaim::scoped`] — which is every chat and task path, and the
    /// reason they are unaffected by the bucketing.
    fn current_scope() -> DelegationScope {
        CURRENT_SCOPE
            .try_with(Clone::clone)
            .unwrap_or(DelegationScope::Unscoped)
    }

    /// What the live claim on **this scope's** bucket permits (issues #453,
    /// #267, #661).
    ///
    /// A scope nobody has claimed reads [`DrainClaim::Unclaimed`], so the
    /// fail-safe default survives the move to a map.
    pub fn claim_state(&self) -> DrainClaim {
        self.committed
            .lock()
            .expect("delegation commitment")
            .get(&Self::current_scope())
            .copied()
            .unwrap_or_default()
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
    /// Since issue #661 this claims the calling task's **scope**, which for
    /// every existing caller is [`DelegationScope::Unscoped`] — the signature
    /// and the behaviour are both unchanged for them.
    #[must_use = "the claim releases on drop; dropping it immediately un-claims the queue"]
    pub fn claim(&self) -> DelegationClaim {
        self.claim_as(Self::current_scope(), DrainClaim::Full)
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
    /// Claims one workflow run's bucket, permitting only the board writes a run
    /// may perform (issue #661).
    ///
    /// The scope is the run id, so concurrent runs — several of which are live
    /// at once under the #401 in-flight cap — cannot see, take, clear, or be
    /// cleared by each other, nor by the chat cycle running beside them.
    ///
    /// The returned claim only routes calls once the run's turns are executed
    /// inside [`DelegationClaim::scoped`]; holding it alone claims the bucket
    /// but leaves the ambient scope unset.
    #[must_use = "the claim releases on drop; dropping it immediately un-claims the queue"]
    pub fn claim_board(&self, run_id: impl Into<String>) -> DelegationClaim {
        self.claim_as(DelegationScope::Run(run_id.into()), DrainClaim::Board)
    }

    #[must_use = "the claim releases on drop; dropping it immediately un-claims the queue"]
    pub fn claim_answering(&self) -> DelegationClaim {
        self.claim_as(Self::current_scope(), DrainClaim::Answering)
    }

    /// The shared body of the claim constructors.
    ///
    /// # Everything it touches is `scope`'s and only `scope`'s (issue #661)
    ///
    /// This function is where the defect lived. `clear()` and `reset_scope()`
    /// were global, so a claim taken by *any* claimant destroyed every other
    /// claimant's staged delegations and reset a running chain's depth — safe
    /// only for as long as every claimant serialised under the chat cycle lock,
    /// which workflow runs do not. Both are now bucketed, so the entry clear
    /// keeps its meaning (a claimant never inherits its own predecessor's
    /// leftovers) while losing its reach.
    fn claim_as(&self, scope: DelegationScope, state: DrainClaim) -> DelegationClaim {
        self.clear_scope(&scope);
        // Issue #176: a claim opens a fresh chain. The chain outlives an
        // ordinary `clear`, so it is reset on the two boundaries that really do
        // end a chain — the claim's acquire and its `Drop` — and nowhere else.
        // Both halves matter: a panic inside a nested turn unwinds past the
        // `ScopeGuard`s, and without the exit reset a leftover chain would make
        // the *next* operator message start at depth 2 and refuse its first
        // hand-off. Same every-exit-path discipline the claim already applies to
        // the queue itself.
        self.reset_chain(&scope);
        self.committed
            .lock()
            .expect("delegation commitment")
            .insert(scope.clone(), state);
        DelegationClaim {
            queue: self.clone(),
            scope,
        }
    }

    /// How deep the delegation chain currently running is: `0` inside the
    /// orchestrator's own turn, `1` inside a desk lead it handed work to, and so
    /// on (issue #176).
    ///
    /// Read from the calling scope's own chain since issue #661, so a
    /// concurrent workflow run's nesting cannot deepen a chat turn's depth (or
    /// vice versa). Depth is still exactly `chain.len()`.
    pub fn scope_depth(&self) -> usize {
        self.chains
            .lock()
            .expect("delegation scope")
            .get(&Self::current_scope())
            .map_or(0, Vec::len)
    }

    /// The resolved desk ids currently on the chain, outermost first (issue
    /// #176) — the set a hand-off target is checked against for a cycle.
    pub fn scope_chain(&self) -> Vec<String> {
        self.chains
            .lock()
            .expect("delegation scope")
            .get(&Self::current_scope())
            .cloned()
            .unwrap_or_default()
    }

    /// Enters the scope of a hand-off to `desk_id`, for as long as the returned
    /// [`ScopeGuard`] lives (issue #176).
    ///
    /// Pushes on the way in and pops on `Drop`, so every exit path from the
    /// delegate's turn — an early return, a `?`, a panic — leaves the chain
    /// exactly as deep as it found it. `desk_id` must be the **resolved** id
    /// rather than whatever key the model typed, so the cycle check compares
    /// identities rather than spellings.
    ///
    /// The guard records which [`DelegationScope`]'s chain it pushed onto
    /// (issue #661) and pops from that one, rather than from whatever scope
    /// happens to be ambient when it drops — so the pop cannot land in another
    /// claimant's chain and take a live level off it.
    #[must_use = "the scope pops on drop; dropping it immediately leaves the chain unchanged"]
    pub fn enter_scope(&self, desk_id: String) -> ScopeGuard {
        let scope = Self::current_scope();
        self.chains
            .lock()
            .expect("delegation scope")
            .entry(scope.clone())
            .or_default()
            .push(desk_id);
        ScopeGuard {
            queue: self.clone(),
            scope,
        }
    }

    /// Empties one scope's chain. Called only where a chain genuinely ends —
    /// the claim's acquire and release.
    fn reset_chain(&self, scope: &DelegationScope) {
        self.chains.lock().expect("delegation scope").remove(scope);
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
    ///
    /// # Why the depth gate is here too (issue #176)
    ///
    /// A desk member that may re-delegate is wired with `delegate_to_desk`
    /// **statically** — belts are cached per roster, so the tool cannot be
    /// withheld from the one turn that happens to be running too deep. The bound
    /// therefore has to be dynamic, and this is the one place every hand-off
    /// passes through. It applies only to
    /// [`DelegateToDesk`](Delegation::DelegateToDesk): that is the delegation
    /// that runs another synchronous turn, and so the only one that can
    /// multiply. A [`SpawnTask`](Delegation::SpawnTask) opens a To-do card and
    /// stops — refusing it at depth would push a member that has hit the bound
    /// into working silently instead of leaving the work tracked, which is the
    /// opposite of what the bound is for.
    #[must_use = "a refused delegation must be reported to the model, not dropped"]
    pub fn push_within_cap(&self, delegation: Delegation, cap: usize, max_depth: usize) -> Staged {
        match self.claim_state() {
            DrainClaim::Unclaimed => return Staged::NoDrain(NoDrainReason::Unwired),
            // Issue #267: the operator asked a question. A hand-off is how one
            // gets answered, so it stages; the pure board writes do not.
            DrainClaim::Answering if !delegation.answers() => {
                return Staged::NoDrain(NoDrainReason::Triage);
            }
            // Issue #661: a workflow run may open and assign cards, but may not
            // move one through its lifecycle or hand off for a reply it has
            // nowhere to put. Two refusals rather than one, because the causes
            // are unrelated and a model told the wrong one is being told
            // something false about what it may do next.
            DrainClaim::Board if !delegation.writes_board_only() => {
                return Staged::NoDrain(match delegation {
                    Delegation::DelegateToDesk { .. } | Delegation::DelegateToTeammate { .. } => {
                        NoDrainReason::WorkflowHandOff
                    }
                    _ => NoDrainReason::WorkflowLifecycle,
                });
            }
            DrainClaim::Answering | DrainClaim::Full | DrainClaim::Board => {}
        }
        // Issue #176: checked after the claim (a context that drains nothing is
        // still the only fact worth reporting) and before the queue lock, so the
        // two locks are never held at once.
        //
        // Issue #884: the teammate hand-off is gated here too, and that is the
        // load-bearing half of its loop safety. Its cycle guard refuses handing
        // work back to somebody already on the chain, but a *ring* of three or
        // more agents closes no immediate cycle — the depth cap is what bounds
        // that, exactly as it does for desks. Leaving the new edge out of this
        // condition would have given it no bound at all.
        if matches!(
            delegation,
            Delegation::DelegateToDesk { .. } | Delegation::DelegateToTeammate { .. }
        ) && self.scope_depth() >= max_depth
        {
            return Staged::NoDrain(NoDrainReason::Depth);
        }
        let mut guard = self.inner.lock().expect("delegation queue");
        let bucket = guard.entry(Self::current_scope()).or_default();
        if bucket.len() >= cap {
            return Staged::OverCap;
        }
        bucket.push(delegation);
        Staged::Queued
    }

    /// Records that a hand-off named `desk`, which the company cannot hand work
    /// to, so the drain can report the attempt (issue #272).
    ///
    /// Files into the calling scope's bucket (issue #661). This call needs no
    /// claim — `DelegateToDeskTool` reaches it on the ungrounded path *before*
    /// consulting [`claim_state`](Self::claim_state) — which is what made the
    /// shared vector reachable from a workflow node today, with no drain wired
    /// and nothing else changed.
    pub fn push_refusal(&self, desk: String) {
        self.refused
            .lock()
            .expect("delegation queue")
            .entry(Self::current_scope())
            .or_default()
            .push(desk);
    }

    /// How many refused desk keys are recorded right now (issue #176).
    ///
    /// Sampled either side of a delegate's turn so a **nested** refusal can be
    /// attributed to the member that made it, rather than swept up with the
    /// refusals its delegator left behind. Exactly the shape
    /// [`ApprovalRequestQueue::queued`](crate::harness::policy::ApprovalRequestQueue::queued)
    /// is used in for parked approvals, and for the same reason: a difference
    /// across a turn is the only honest way to say *this* turn did it.
    pub fn refusals_queued(&self) -> usize {
        self.refused
            .lock()
            .expect("delegation queue")
            .get(&Self::current_scope())
            .map_or(0, Vec::len)
    }

    /// Drains up to `cap` refused desk keys recorded **after** the first
    /// `from` (issue #176), leaving the earlier ones for whoever owns them.
    ///
    /// [`drain_refusals`](Self::drain_refusals) also clears the tail; this one
    /// deliberately does not, because the entries before `from` belong to an
    /// outer turn that has not read them yet.
    ///
    /// `from` indexes this scope's own bucket (issue #661), which is the same
    /// vector [`refusals_queued`](Self::refusals_queued) counted — so the
    /// sample-either-side-of-a-turn pattern keeps its meaning, and can no
    /// longer be thrown off by a concurrent claimant pushing between the two
    /// samples.
    pub fn drain_refusals_after(&self, from: usize, cap: usize) -> Vec<String> {
        let mut guard = self.refused.lock().expect("delegation queue");
        let Some(bucket) = guard.get_mut(&Self::current_scope()) else {
            return Vec::new();
        };
        if bucket.len() <= from {
            return Vec::new();
        }
        let take = (bucket.len() - from).min(cap);
        bucket.drain(from..from + take).collect()
    }

    /// Drains up to `cap` refused desk keys (FIFO) and discards the rest, so a
    /// turn that calls the tool repeatedly cannot grow an unbounded note.
    ///
    /// A discard is logged rather than silent (issue #419) — the note it would
    /// have grown is the operator's only record that a hand-off was attempted.
    pub fn drain_refusals(&self, cap: usize) -> Vec<String> {
        let mut guard = self.refused.lock().expect("delegation queue");
        let Some(bucket) = guard.get_mut(&Self::current_scope()) else {
            return Vec::new();
        };
        let take = bucket.len().min(cap);
        let dropped = bucket.len() - take;
        let drained: Vec<String> = bucket.drain(..take).collect();
        if dropped > 0 {
            tracing::warn!(
                dropped,
                cap,
                "[delegation] discarded refused hand-offs past the per-turn cap; they will not be \
                 recorded on the card"
            );
        }
        // Issue #661: this scope's tail only. It used to clear the whole shared
        // vector, which is what let one claimant's drain swallow another's
        // pending refusals.
        bucket.clear();
        drained
    }

    /// Empties the queue (called before an orchestrator turn so stale
    /// delegations from a prior turn never leak into this one).
    ///
    /// Empties **this scope's** staged delegations and refusals only (issue
    /// #661); a concurrent claimant's are untouched.
    pub fn clear(&self) {
        self.clear_scope(&Self::current_scope());
    }

    /// [`clear`](Self::clear) against an explicitly named scope, for the two
    /// callers that know their scope rather than inheriting it from the task:
    /// the claim's acquire and its `Drop` (issue #661).
    ///
    /// `Drop` in particular cannot read the ambient scope — a claim is very
    /// often dropped outside its own [`scoped`](DelegationClaim::scoped) future
    /// — so it must carry the scope it claimed.
    fn clear_scope(&self, scope: &DelegationScope) {
        self.inner.lock().expect("delegation queue").remove(scope);
        self.refused.lock().expect("delegation queue").remove(scope);
    }

    /// Releases a claim: discards everything the claim's scope staged and
    /// returns that scope to [`DrainClaim::Unclaimed`] (issue #661).
    ///
    /// A cancelled or panicking run's staged writes dying with the run is the
    /// intended semantics, matching the stance
    /// [`ApprovalClaim`](crate::harness::policy::ApprovalClaim) takes one queue
    /// over: staged work that nothing will now drain is work that must not
    /// survive to be executed under somebody else's turn.
    fn release(&self, scope: &DelegationScope) {
        self.committed
            .lock()
            .expect("delegation commitment")
            .remove(scope);
        self.clear_scope(scope);
        self.reset_chain(scope);
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
        let Some(bucket) = guard.get_mut(&Self::current_scope()) else {
            return Vec::new();
        };
        let take = bucket.len().min(cap);
        let dropped = bucket.len() - take;
        let drained: Vec<Delegation> = bucket.drain(..take).collect();
        if dropped > 0 {
            tracing::warn!(
                dropped,
                cap,
                "[delegation] discarding queued delegations past the per-turn cap — the tool \
                 boundary should have refused these before they were queued"
            );
        }
        // Issue #661: this scope's tail only — draining a chat turn must not
        // throw away what a concurrently-running workflow run has staged.
        bucket.clear();
        drained
    }

    /// The number of queued delegations in the calling scope
    /// (test/observability).
    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.inner
            .lock()
            .expect("delegation queue")
            .get(&Self::current_scope())
            .map_or(0, Vec::len)
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
    /// The delegation chain is already as deep as
    /// `[tools].max_delegation_depth` allows (issue #176), so a further
    /// **hand-off** is refused. Board writes are unaffected — a member at the
    /// bound may still open a card.
    ///
    /// Unlike the two above this is not a property of the context at all: the
    /// same member, on the same company, delegating from a shallower chain would
    /// have been staged. So the refusal must not tell the model its context
    /// cannot do board work (it can) nor that the message was a question (it was
    /// not) — it must say the chain has run as deep as the company allows.
    Depth,
    /// The queue is claimed by a workflow run ([`DrainClaim::Board`], issue
    /// #661) and the call would move a card through its lifecycle, which is the
    /// operator's lane rather than the run's.
    ///
    /// Distinct from [`WorkflowHandOff`](Self::WorkflowHandOff) because the
    /// causes are unrelated: this one is a deliberate authority boundary that no
    /// amount of wiring will move, and the model's recourse is to leave the card
    /// for a person. Collapsing the two would tell a model that `review_task`
    /// failed for want of somewhere to put a reply, which is untrue and points
    /// it at the wrong alternative.
    WorkflowLifecycle,
    /// The queue is claimed by a workflow run ([`DrainClaim::Board`], issue
    /// #661) and the call is a hand-off, whose only value is a synchronous reply
    /// that a run has nowhere to land.
    ///
    /// A run has no conversation behind it and nobody watching at 3am, so the
    /// reply would be composed and dropped. The recourse is real and worth
    /// naming: open a card for the desk instead, which persists and is exactly
    /// what a run *can* do.
    WorkflowHandOff,
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
            Self::Depth => "depth_capped",
            Self::WorkflowLifecycle => "workflow_lifecycle_operator_only",
            Self::WorkflowHandOff => "workflow_handoff_no_reply_target",
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
    /// A workflow run has claimed its own scope's bucket (issue #661). The
    /// drain runs exactly as under [`Full`](Self::Full); only delegations that
    /// [`write the board only`](Delegation::writes_board_only) may be staged.
    ///
    /// This is [`Answering`](Self::Answering)'s shape inverted, and inverted is
    /// the right word: that one permits the hand-off and refuses the board
    /// writes, this one permits the board writes and refuses the hand-off. Both
    /// exist because withholding the claim outright is too blunt — it says "no
    /// board work at all", which for a run is false and would leave the
    /// `→ task cards` seed unable to make a card.
    ///
    /// # The refusals are load-bearing for loop safety
    ///
    /// A run may open a card and set its owner; it may not move one between
    /// columns. `todo → planning` is only ever written by an operator drag and
    /// `planning → in_progress` is the dispatch gate, so run → card → dispatch
    /// → run cycles stay bounded precisely because every dispatch requires an
    /// operator act. Relaxing the column rule would take that bound with it.
    Board,
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
    /// The scope this claim owns (issue #661) — carried rather than read from
    /// the task on drop, because a claim is routinely dropped outside its own
    /// [`scoped`](Self::scoped) future, where the ambient scope is no longer
    /// its own.
    scope: DelegationScope,
}

impl DelegationClaim {
    /// The scope this claim owns.
    pub fn scope(&self) -> &DelegationScope {
        &self.scope
    }

    /// Runs `fut` with this claim's scope installed, so every delegation call
    /// inside it files into — and drains from — this claim's bucket (issue
    /// #661).
    ///
    /// The whole turn goes inside. A call that escapes the future lands in
    /// [`DelegationScope::Unscoped`] rather than in another claimant's bucket,
    /// which is the conservative direction: unclaimed there means an honest
    /// in-turn refusal, never somebody else's delegation being executed.
    ///
    /// Chat and task callers do not need this and do not use it — they claim
    /// `Unscoped`, which is what an un-installed task-local already reads as.
    pub async fn scoped<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        CURRENT_SCOPE.scope(self.scope.clone(), fut).await
    }
}

impl Drop for DelegationClaim {
    fn drop(&mut self) {
        // Issue #661: everything released here is keyed to this claim's own
        // scope. Before that this reset a single global commitment and cleared
        // one shared vector, so a claim ending anywhere un-claimed the queue
        // everywhere — the exit half of the same defect the acquire had.
        //
        // Issue #176: the chain ends with the claim. `ScopeGuard` pops its own
        // entry on every ordinary exit, so this is the belt to that braces — a
        // panic mid-nested-turn unwinds past the guards, and a chain left
        // standing would make the next message start at depth 2.
        self.queue.release(&self.scope);
    }
}

/// One level of the delegation scope chain, held for the span in which a
/// delegate's turn runs (issue #176).
///
/// Pushes its desk id when created and pops on `Drop`. Same reasoning as
/// [`DelegationClaim`]: the pop has to happen on **every** exit path, including
/// the ones nobody wrote by hand, or a chain that dies mid-turn leaves the queue
/// permanently one level deeper than it is.
///
/// Deliberately not [`Clone`] — two guards for one level would pop twice and
/// take a live outer level off the chain with them.
pub struct ScopeGuard {
    queue: DelegationQueue,
    /// Which claimant's chain this level was pushed onto (issue #661), so the
    /// pop lands in that same chain rather than in whichever one is ambient at
    /// drop time.
    scope: DelegationScope,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        if let Some(chain) = self
            .queue
            .chains
            .lock()
            .expect("delegation scope")
            .get_mut(&self.scope)
        {
            chain.pop();
        }
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
    /// The company's task board, so the insight document's `## Board` section
    /// (issue #1859) can summarize open work by column instead of the
    /// orchestrator having to reach for the separate `list_tasks` tool just to
    /// answer "is anything blocked?" as part of a broader question. `None`
    /// renders the section as unavailable rather than failing the whole tool.
    tasks: Option<Arc<dyn TaskStore>>,
}

impl QueryCompanyTool {
    /// Builds the tool over the company's read ports. Any handle may be `None`;
    /// the tool reports whatever surface is wired.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        company: CompanyId,
        facts: Option<Arc<dyn FactStore>>,
        events: Option<Arc<dyn EventLog>>,
        workflow_source_dir: Option<PathBuf>,
        store: Option<Arc<dyn CompanyStore>>,
        tasks: Option<Arc<dyn TaskStore>>,
    ) -> Self {
        Self {
            company,
            facts,
            events,
            workflow_source_dir,
            store,
            tasks,
        }
    }
}

#[async_trait]
impl Tool for QueryCompanyTool {
    fn name(&self) -> &str {
        QUERY_COMPANY_TOOL
    }

    fn description(&self) -> &str {
        "Read the company's durable facts, recent activity, saved workflows, team roster, desks, and a board summary to ground an answer in whole-company context — use this to answer \"what workflows do we have?\", \"who is on the team?\", \"which desks can take work?\", or \"what's in flight?\" instead of guessing, and to get the exact desk id `delegate_to_desk` needs. For a specific card's full attempt history and output, use `list_tasks` / `read_task` instead. Optionally pass a `query` to filter facts by a case-insensitive substring."
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
        let globals_disable = record
            .as_ref()
            .map(|r| r.manifest.globals.disable.clone())
            .unwrap_or_default();
        let mut workflows: Vec<(String, String)> = list_workflows_with_globals(
            self.workflow_source_dir.as_deref(),
            &overlay_workflows,
            &globals_disable,
        )
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
        //
        // **Every row leads with the id**, because this column is the one the
        // orchestrator's brief and `delegate_to_teammate`'s own description
        // send the model to for a hand-off target. An overlay teammate used to
        // be listed under `overlay.name` while a manifest agent was listed
        // under `agent.id` — two namespaces rendered identically, and since
        // `mint_agent_id` slugs the display name (`"Dana Designer"` →
        // `dana_designer`) the name was a token the delegation tools could not
        // ground. The model did exactly what it was told and was refused
        // (issue #1162). The display name follows as a label, in the shape
        // `workflow_build::roster_line` (#813) already uses for the roster it
        // shows the same model, so the two surfaces cannot drift apart.
        let mut roster: Vec<(String, Option<String>, String)> = Vec::new();
        if let Some(record) = &record {
            // Resolved through the record: a teammate the operator removed is not
            // a delegation target, and one they renamed is named as it is now.
            for agent in record.effective_agents() {
                // A manifest `[[agent]]` has no display name unless an operator
                // gave it one; otherwise its role is its label.
                roster.push((agent.id.clone(), agent.name.clone(), agent.role.clone()));
            }
            for overlay in record
                .overlay_agents
                .iter()
                .filter(|a| !record.is_retired(&a.id))
            {
                roster.push((
                    overlay.id.clone(),
                    Some(overlay.name.clone()).filter(|n| !n.trim().is_empty()),
                    overlay.role.clone(),
                ));
            }
        }
        md.push_str("\n## Team\n");
        if roster.is_empty() {
            md.push_str("_Roster unavailable._\n");
        } else {
            for (id, name, role) in &roster {
                md.push_str(&format!("- **{}** — {}", id, role.trim()));
                if let Some(name) = name {
                    md.push_str(&format!(" (known as {})", name.trim()));
                }
                md.push('\n');
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
                    // A leadless answer is two different facts (issue #1835):
                    // an `auto` channel has members but no lead by design —
                    // "cannot be handed work" would be a lie about a staffed
                    // channel — while a desk with nobody on the roster really
                    // cannot take anything.
                    None if record
                        .as_ref()
                        .is_some_and(|r| !r.desk_responder_mode(id).is_lead()) =>
                    {
                        md.push_str(&format!(
                            "- **{id}** — channel without a lead; who answers is picked per message. `delegate_to_desk` cannot target it — use `delegate_to_teammate` with one of its members\n"
                        ))
                    }
                    None => md.push_str(&format!(
                        "- **{id}** — no member on the roster, so it cannot be handed work\n"
                    )),
                }
            }
        }

        // Board summary (issue #1859): open cards grouped by column, so a
        // whole-company query surfaces execution state alongside facts and
        // roster instead of forcing a second `list_tasks` call for "what's in
        // flight?" as part of a broader question.
        //
        // **LAST section, deliberately.** Every section above it (Facts,
        // Recent activity, Saved workflows, Team, Desks) is inside the outer
        // tool-result byte budget the harness enforces
        // (`TOOL_RESULT_BUDGET_BYTES`), and a company with an unusually large
        // board must never be able to push that cut back far enough to drop
        // the Desks list `delegate_to_desk` depends on. Unlike Facts (which
        // has `query` to narrow with) this section has no narrowing argument
        // of its own — `list_tasks` is the fallback for a board too big to
        // fit here, exactly as its own truncation marker below says.
        let mut board_open_count = 0usize;
        md.push_str("\n## Board\n");
        match &self.tasks {
            Some(tasks) => match tasks.list(&self.company).await {
                Ok(cards) => {
                    let total_open = cards.iter().filter(|c| c.column != COLUMN_DONE).count();
                    if total_open == 0 {
                        md.push_str("_No open cards._\n");
                    } else {
                        let mut shown = 0usize;
                        for column in BOARD_COLUMNS {
                            if column == COLUMN_DONE {
                                continue;
                            }
                            let in_column: Vec<&TaskRecord> =
                                cards.iter().filter(|c| c.column == column).collect();
                            if in_column.is_empty() {
                                continue;
                            }
                            let mut titles: Vec<&str> = Vec::new();
                            for c in &in_column {
                                if shown >= LIST_TASKS_LIMIT {
                                    break;
                                }
                                titles.push(c.title.as_str());
                                shown += 1;
                            }
                            md.push_str(&format!(
                                "- **{}** ({}): {}\n",
                                column_label(column),
                                in_column.len(),
                                if titles.is_empty() {
                                    "…".to_string()
                                } else {
                                    titles.join("; ")
                                }
                            ));
                        }
                        if shown < total_open {
                            md.push_str(&format!(
                                "\n[TRUNCATED — {} more open card(s) not shown here. Use \
                                 `{LIST_TASKS_TOOL}` to page through the rest.]\n",
                                total_open - shown
                            ));
                        }
                    }
                    board_open_count = total_open;
                }
                Err(err) => {
                    tracing::debug!(company = %self.company, error = %err, "query_company: board read failed");
                    md.push_str("_Board unavailable._\n");
                }
            },
            None => md.push_str("_Board unavailable._\n"),
        }

        Ok(ToolResult::success_with_markdown(
            json!({
                "facts": facts.len(),
                "recent_events": recent.len(),
                "events_not_shown": older,
                "workflows": workflows.len(),
                "team": roster.len(),
                "desks": desks.len(),
                "board_open": board_open_count,
            }),
            md,
        ))
    }
}

/// Renders `attempt N status` for the newest row in `runs`, or `None` when
/// `runs` is empty — a card nobody has attempted yet gets no attempt clause
/// rather than a fabricated one (issue #1859, the same rule
/// [`inject_handed_task_awareness`](crate::runtime::cycle::CycleRunner) follows
/// for the chat-side briefing). `runs` is assumed newest-first, the
/// [`RunStore::list_runs`] ordering, so the first row is the latest attempt.
///
/// Deliberately never reads [`RunRecord::usage`] — no run's cost reaches any
/// of the three read tools this backs.
fn latest_attempt_label(runs: &[RunRecord]) -> Option<String> {
    let run = runs.first()?;
    Some(format!("attempt {} {}", run.attempt, run.status.as_str()))
}

/// Renders the `## Output` section's fallback line for a card with no
/// published artifact to show — either because no [`ArtifactStore`] is
/// wired at all (`store_wired = false`) or because one is wired but has
/// nothing for this task (`store_wired = true`, an empty list). Falls back to
/// the card's own recorded output stamp ([`TaskRecord::output`]) so a card
/// whose only trace is a `TaskOutput` (an operator reply, not a published
/// file) is not reported identically to a card that produced nothing.
fn output_stamp_markdown(output: Option<&TaskOutput>, store_wired: bool) -> String {
    let banner = if store_wired {
        "No artifacts published"
    } else {
        "No artifact store wired"
    };
    match output {
        Some(output) => match output.source.run_id() {
            Some(run_id) => {
                let attempt = output
                    .source
                    .attempt()
                    .map(|a| format!(" (attempt {a})"))
                    .unwrap_or_default();
                format!(
                    "_{banner}; this card's last successful attempt was run `{run_id}`{attempt}. \
                     Use `read_run` for that attempt's outcome._\n"
                )
            }
            None => format!(
                "_{banner}; this card's output came from an operator chat turn, not an \
                 attempt._\n"
            ),
        },
        None if store_wired => "_Nothing published yet._\n".to_string(),
        None => "_Nothing produced yet._\n".to_string(),
    }
}

/// A read-only surface over the company's task board (issue #1859): every
/// open card, grouped by column, with its assignee and latest attempt status
/// — the surface an agent queries directly to answer "what are you working
/// on?" or "what's in review?" truthfully, on the same terms
/// [`OPEN_WORK_ANNOTATION`](crate::runtime::cycle::OPEN_WORK_ANNOTATION)
/// already briefs into an addressed chat message, but reachable from any turn
/// rather than only one already addressed to a desk with open work.
///
/// Fail-closed by construction: this tool reads only [`TaskStore`] and
/// [`RunStore`], neither of which carries a run's USD cost, a raw tool-call
/// argument, or a step's full trace — so none of those can leak here no
/// matter what the rendering does with them.
pub struct ListTasksTool {
    company: CompanyId,
    tasks: Option<Arc<dyn TaskStore>>,
    runs: Option<Arc<dyn RunStore>>,
}

impl ListTasksTool {
    /// Builds the tool over the company's board ports. Either may be `None`;
    /// the tool reports the surface unavailable rather than failing.
    pub fn new(
        company: CompanyId,
        tasks: Option<Arc<dyn TaskStore>>,
        runs: Option<Arc<dyn RunStore>>,
    ) -> Self {
        Self {
            company,
            tasks,
            runs,
        }
    }
}

#[async_trait]
impl Tool for ListTasksTool {
    fn name(&self) -> &str {
        LIST_TASKS_TOOL
    }

    fn description(&self) -> &str {
        "List task cards on the company board, grouped by column, with each card's id, assignee, and latest attempt status — use this to answer \"what are you working on?\", \"what's in review?\", or \"is anything stuck?\" instead of guessing. Excludes Done cards unless `column` explicitly asks for them. Pass a card's id to `read_task` for its full history and output."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "column": {
                    "type": "string",
                    "description": "Only cards in this column (todo, planning, in_progress, paused, in_review, done). Omit to see every not-done column."
                },
                "assignee": {
                    "type": "string",
                    "description": "Only cards assigned to this desk/teammate id (case-insensitive exact match)."
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
        let Some(tasks) = &self.tasks else {
            return Ok(ToolResult::error(
                "No task board wired to this company build; `list_tasks` cannot answer.",
            ));
        };
        let column_filter = args
            .get("column")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(col) = column_filter
            && !is_board_column(col)
        {
            return Ok(ToolResult::error(format!(
                "Unknown column `{col}`. Valid columns: {}.",
                BOARD_COLUMNS.join(", ")
            )));
        }
        let assignee_filter = args
            .get("assignee")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let mut cards = match tasks.list(&self.company).await {
            Ok(cards) => cards,
            Err(err) => {
                tracing::debug!(company = %self.company, error = %err, "list_tasks: board read failed");
                return Ok(ToolResult::error(format!(
                    "Couldn't read the task board: {err}"
                )));
            }
        };
        cards.retain(|c| match column_filter {
            Some(col) => c.column == col,
            None => c.column != COLUMN_DONE,
        });
        if let Some(assignee) = assignee_filter {
            cards.retain(|c| c.assignee.eq_ignore_ascii_case(assignee));
        }
        let total = cards.len();

        let mut md = String::from("# Task board\n");
        if total == 0 {
            md.push_str("_No matching cards._\n");
        } else {
            let mut shown = 0usize;
            for column in BOARD_COLUMNS {
                let in_column: Vec<&TaskRecord> =
                    cards.iter().filter(|c| c.column == column).collect();
                if in_column.is_empty() {
                    continue;
                }
                md.push_str(&format!("\n## {}\n", column_label(column)));
                for c in in_column {
                    if shown >= LIST_TASKS_LIMIT {
                        continue;
                    }
                    let attempt = match &self.runs {
                        Some(runs) => match runs
                            .list_runs(
                                &self.company,
                                &RunFilter::for_task(c.id.as_str()).with_limit(1),
                            )
                            .await
                        {
                            Ok(rows) => latest_attempt_label(&rows),
                            Err(_) => Some("attempt status unavailable".to_string()),
                        },
                        None => None,
                    };
                    md.push_str(&format!(
                        "- `{}` {} — {}{}\n",
                        c.id,
                        c.title,
                        c.assignee,
                        attempt.map(|a| format!(" — {a}")).unwrap_or_default()
                    ));
                    shown += 1;
                }
            }
            if shown < total {
                md.push_str(&format!(
                    "\n[TRUNCATED — {} more card(s) not shown. Narrow with `column` or \
                     `assignee` before concluding a card is absent.]\n",
                    total - shown
                ));
            }
        }

        Ok(ToolResult::success_with_markdown(
            json!({ "shown": total.min(LIST_TASKS_LIMIT), "total": total }),
            md,
        ))
    }
}

/// A read-only surface over one task card's full record (issue #1859): its
/// header, every attempt's status, and what it produced — so an agent can
/// discuss a finished task, explain why one is stuck, or answer a follow-up
/// about its output instead of inventing an answer.
///
/// Deliberately **not** built over [`crate::server::ops::ScopedCompany`] or
/// the console's `assemble_detail` / task-export renderer (`task_export.rs`):
/// those exist to serve an operator's browser through a redaction pipeline
/// shaped for that surface, and fabricating a `ScopedCompany` outside a
/// request would be reaching for state this tool has no business holding.
/// This tool is fail-closed on its own, narrower terms instead — it holds
/// only [`TaskStore`], [`RunStore`] and [`ArtifactStore`], none of which
/// carries a run's USD cost, a raw tool-call argument, or a step's full
/// trace, so none of those can leak here no matter what the rendering does.
pub struct ReadTaskTool {
    company: CompanyId,
    tasks: Option<Arc<dyn TaskStore>>,
    runs: Option<Arc<dyn RunStore>>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
}

impl ReadTaskTool {
    /// Builds the tool over the company's board ports. Any may be `None`; the
    /// tool reports whatever surface is wired and falls back where it can
    /// (see the `## Output` section in [`Self::execute`]).
    pub fn new(
        company: CompanyId,
        tasks: Option<Arc<dyn TaskStore>>,
        runs: Option<Arc<dyn RunStore>>,
        artifacts: Option<Arc<dyn ArtifactStore>>,
    ) -> Self {
        Self {
            company,
            tasks,
            runs,
            artifacts,
        }
    }
}

#[async_trait]
impl Tool for ReadTaskTool {
    fn name(&self) -> &str {
        READ_TASK_TOOL
    }

    fn description(&self) -> &str {
        "Read one task card in full: its column, assignee, note, every attempt's status, and what it produced — use this to discuss a finished task, explain why one is stuck, or answer a follow-up about its output. Pass the card's `task_id`, from `list_tasks` or a board reference."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The card's id, from `list_tasks` or a board reference."
                }
            },
            "required": ["task_id"],
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
        let Some(tasks) = &self.tasks else {
            return Ok(ToolResult::error(
                "No task board wired to this company build; `read_task` cannot answer.",
            ));
        };
        let task_id = args
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(task_id) = task_id else {
            return Ok(ToolResult::error(
                "`task_id` is required: pass the card's id from `list_tasks`.",
            ));
        };

        let cards = match tasks.list(&self.company).await {
            Ok(cards) => cards,
            Err(err) => {
                tracing::debug!(company = %self.company, error = %err, "read_task: board read failed");
                return Ok(ToolResult::error(format!(
                    "Couldn't read the task board: {err}"
                )));
            }
        };
        let Some(card) = cards.into_iter().find(|c| c.id == task_id) else {
            return Ok(ToolResult::error(format!(
                "No card `{task_id}` on this board. Call `{LIST_TASKS_TOOL}` for the current ids."
            )));
        };

        let mut md = format!("# {}\n", truncate_chars(&card.title, READ_TASK_TITLE_LIMIT));
        md.push_str(&format!(
            "- **Column**: {}\n- **Priority**: {}\n- **Assignee**: {}\n",
            column_label(&card.column),
            card.priority,
            card.assignee
        ));
        if let Some(note) = card.note.as_deref().map(str::trim)
            && !note.is_empty()
        {
            md.push_str(&format!("- **Note**: {}\n", truncate_chars(note, 400)));
        }

        md.push_str("\n## Attempts\n");
        let mut attempt_count = 0usize;
        match &self.runs {
            Some(runs) => match runs
                .list_runs(&self.company, &RunFilter::for_task(task_id))
                .await
            {
                Ok(mut attempts) => {
                    attempt_count = attempts.len();
                    if attempts.is_empty() {
                        md.push_str("_No attempts yet._\n");
                    } else {
                        // `list_runs` is newest-first — keep the newest rows
                        // (the ones a "why isn't this done" question is
                        // about) before reversing the kept window to render
                        // the timeline oldest-first.
                        let omitted = attempts.len().saturating_sub(READ_TASK_ATTEMPTS_LIMIT);
                        attempts.truncate(READ_TASK_ATTEMPTS_LIMIT);
                        attempts.reverse();
                        if omitted > 0 {
                            md.push_str(&format!(
                                "_{omitted} earlier attempt(s) omitted — showing the \
                                 {READ_TASK_ATTEMPTS_LIMIT} most recent._\n"
                            ));
                        }
                        for run in &attempts {
                            md.push_str(&format!(
                                "- attempt {} — {} (run `{}`)",
                                run.attempt,
                                run.status.as_str(),
                                run.id
                            ));
                            if let Some(err) = &run.error {
                                md.push_str(&format!(" — {}", truncate_chars(err, 200)));
                            }
                            md.push('\n');
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(company = %self.company, task_id, error = %err, "read_task: run-history read failed");
                    md.push_str(
                        "_Run history unavailable — the run store couldn't be read. This is \
                         NOT the same as no attempts._\n",
                    );
                }
            },
            None => md.push_str("_Run history unavailable._\n"),
        }

        md.push_str("\n## Output\n");
        match &self.artifacts {
            Some(artifacts) => match artifacts.list(&self.company, Some(task_id)).await {
                Ok(published) => {
                    if published.is_empty() {
                        md.push_str(&output_stamp_markdown(card.output.as_ref(), true));
                    } else {
                        let pinned = card.output.as_ref().map(|o| o.artifacts.as_slice());
                        let has_stamp = pinned.is_some();
                        let mut rendered = 0usize;
                        for artifact in &published {
                            let pinned_entry = pinned.and_then(|entries| {
                                entries.iter().find(|a| a.artifact_id == artifact.id)
                            });
                            if has_stamp && pinned_entry.is_none() {
                                continue;
                            }
                            rendered += 1;
                            let preview = pinned_entry
                                .and_then(|entry| artifact.version(entry.version))
                                .or_else(|| artifact.latest())
                                .map(|v| truncate_chars(v.body.trim(), 400))
                                .unwrap_or_default();
                            md.push_str(&format!(
                                "- **{}** ({}): {}\n",
                                artifact.title,
                                artifact.kind.as_str(),
                                preview
                            ));
                        }
                        if has_stamp && rendered == 0 {
                            md.push_str(&output_stamp_markdown(card.output.as_ref(), true));
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(company = %self.company, task_id, error = %err, "read_task: artifact read failed");
                    md.push_str(
                        "_Output unavailable — the artifact store couldn't be read. This is \
                         NOT the same as nothing published._\n",
                    );
                }
            },
            None => md.push_str(&output_stamp_markdown(card.output.as_ref(), false)),
        }

        if let Some(output) = &card.output
            && !output.workflows.is_empty()
        {
            md.push_str("\n### Workflows\n");
            for wf in &output.workflows {
                let run_note = wf
                    .run_id
                    .as_deref()
                    .map(|id| format!(" — run `{id}`"))
                    .unwrap_or_default();
                md.push_str(&format!(
                    "- {} `{}`{run_note}\n",
                    wf.action.as_str(),
                    wf.workflow_id
                ));
            }
        }

        Ok(ToolResult::success_with_markdown(
            json!({
                "task_id": card.id,
                "column": card.column,
                "attempts": attempt_count,
            }),
            md,
        ))
    }
}

/// A read-only surface over one recorded run (issue #1859): an agent-attempt
/// row from [`RunStore`] when one exists, else a workflow run folded straight
/// out of the journal — so "why isn't X done?" or "what happened on run X?"
/// can be answered from the same two sources the console's Attempts list and
/// workflow history panel already read, instead of guessing.
///
/// **Summarizes, never dumps.** A run's [`RunStepRecord`](crate::ports::runs::RunStepRecord)
/// trace and a workflow node's raw output are deliberately never read here —
/// this tool answers with the run's status/verdict and, for a workflow run,
/// each node's terminal status and the pending-approval/blocked counts. No
/// step trace, no tool-call argument, no cost reaches the rendering, because
/// none of those are read off the ports this tool holds in the first place.
pub struct ReadRunTool {
    company: CompanyId,
    runs: Option<Arc<dyn RunStore>>,
    events: Option<Arc<dyn EventLog>>,
}

impl ReadRunTool {
    /// Builds the tool over the company's run ports. Either may be `None`;
    /// the tool reports whichever half of its dual-source lookup is missing
    /// rather than failing outright, as long as the other half can answer.
    pub fn new(
        company: CompanyId,
        runs: Option<Arc<dyn RunStore>>,
        events: Option<Arc<dyn EventLog>>,
    ) -> Self {
        Self {
            company,
            runs,
            events,
        }
    }
}

#[async_trait]
impl Tool for ReadRunTool {
    fn name(&self) -> &str {
        READ_RUN_TOOL
    }

    fn description(&self) -> &str {
        "Read one run's status and outcome — an agent-attempt run id (from `list_tasks` / `read_task`) or a workflow run id (from a `run_workflow` summary) — use this to explain why a run is stuck, failed, or blocked. Summarizes the verdict and node/approval state; does not dump the full step trace."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "The run id — an agent-attempt id or a workflow run id."
                }
            },
            "required": ["run_id"],
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
        let run_id = args
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(run_id) = run_id else {
            return Ok(ToolResult::error(
                "`run_id` is required: pass an agent-attempt id or a workflow run id.",
            ));
        };

        // Agent-attempt path first — the common case (`list_tasks` /
        // `read_task` both surface this id).
        if let Some(runs) = &self.runs {
            match runs.get_run(&self.company, run_id).await {
                Ok(Some(run)) => {
                    let mut md = format!("# Attempt {} — {}\n", run.attempt, run.status.as_str());
                    if let Some(task_id) = &run.task_id {
                        md.push_str(&format!("- **Task**: `{task_id}`\n"));
                    }
                    md.push_str(&format!("- **Agent**: {}\n", run.agent_id));
                    if let Some(err) = &run.error {
                        md.push_str(&format!("- **Error**: {}\n", truncate_chars(err, 300)));
                    }
                    return Ok(ToolResult::success_with_markdown(
                        json!({ "run_id": run.id, "kind": "attempt", "status": run.status.as_str() }),
                        md,
                    ));
                }
                // Not an attempt row — fall through to the workflow-run path.
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!(company = %self.company, run_id, error = %err, "read_run: run-store read failed");
                    return Ok(ToolResult::error(format!(
                        "Couldn't read the run store: {err}"
                    )));
                }
            }
        }

        // Workflow-run path: fold the journal, the same fold the console's
        // run-history route reads (`fold_run_events`) — reading the whole
        // company journal once, on the same terms `QueryCompanyTool` already
        // does for its recent-activity section, rather than the paged,
        // live-run-cross-checked walk `list_runs` does for an unbounded
        // history page (this tool answers about ONE run, not a page of them).
        let Some(events) = &self.events else {
            return Ok(ToolResult::error(format!(
                "No run `{run_id}` found, and no event log wired to check workflow runs."
            )));
        };
        let rows = match events
            .read_from(&self.company, EventSeq::new(0), usize::MAX)
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::debug!(company = %self.company, run_id, error = %err, "read_run: event log read failed");
                return Ok(ToolResult::error(format!(
                    "Couldn't read the event journal: {err}"
                )));
            }
        };
        let (folded, _read_through) = crate::server::ops::workflows::fold_run_events(rows, None);
        let Some(outcome) = folded
            .into_iter()
            .find(|o| o.run_id.as_deref() == Some(run_id))
        else {
            return Ok(ToolResult::error(format!(
                "No run `{run_id}` found — not an agent attempt and not a workflow run in this \
                 company's history."
            )));
        };

        // `outcome.verdict` is only ever the fold's placeholder (`Running`,
        // never resolved by `fold_run_events` itself — see
        // `WorkflowRunOutcome::derive_verdict`'s doc comment) unless something
        // calls `derive_verdict` after the fold, which the console's
        // `list_runs` route does and this tool must too.
        let verdict = outcome.derive_verdict();
        let mut md = format!("# Workflow run — {}\n", verdict.as_str());
        md.push_str(&format!("- **Workflow**: {}\n", outcome.workflow_id));
        md.push_str(&format!(
            "- **Status**: {}\n",
            if outcome.running {
                "still running"
            } else {
                "settled"
            }
        ));
        if let Some(err) = &outcome.error {
            md.push_str(&format!("- **Error**: {}\n", truncate_chars(err, 300)));
        }
        if !outcome.nodes.is_empty() {
            md.push_str("\n## Nodes\n");
            for node in &outcome.nodes {
                md.push_str(&format!("- {} — {:?}\n", node.node_id, node.status));
            }
        }
        if !outcome.pending_approvals.is_empty() {
            md.push_str(&format!(
                "\n{} pending approval(s).\n",
                outcome.pending_approvals.len()
            ));
        }
        if !outcome.blocked_nodes.is_empty() {
            md.push_str(&format!(
                "{} node(s) blocked on a person.\n",
                outcome.blocked_nodes.len()
            ));
        }

        Ok(ToolResult::success_with_markdown(
            json!({ "run_id": run_id, "kind": "workflow", "verdict": verdict.as_str() }),
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
        // Issue #983. Structural only, like every arm here: the turn id, which
        // is a minted identifier, and nothing else. Neither the desk nor the
        // failure reason is named — the desk is operator-authored free text on
        // an overlay-created chat, and the reason is our own prose about the
        // host, which is exactly what a non-sensitive one-liner should not
        // carry.
        CompanyEvent::TurnStarted { turn_id, .. } => format!("turn accepted: {turn_id}"),
        CompanyEvent::TurnFailed { turn_id, .. } => format!("turn unanswered: {turn_id}"),
        // Issue #1015. Structural only: the minted id and the status word, a
        // fixed vocabulary. The failure reason is our own prose about the host
        // and is tenant-scoped, so it stays off this surface exactly as
        // `TurnFailed`'s does.
        CompanyEvent::RunStatusChanged { run_id, to, .. } => {
            format!("attempt {run_id}: {to}")
        }
        CompanyEvent::AgentReply { agent_id, .. } => format!("reply from {agent_id}"),
        CompanyEvent::TaskDispatched { task_id, .. } => format!("task dispatched: {task_id}"),
        // Issue #464. Structural only, like every arm here: the id, the change
        // word (fixed vocabulary) and the stage (one of six). The card's title
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
        // Issue #1805. Structural only, on the same terms as the parked/resolved
        // arms: the id, and nothing else — `by` is a user id, dropped as every
        // arm here drops it.
        CompanyEvent::ApprovalExtended { approval_id, .. } => {
            format!("approval {approval_id} extended")
        }
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
                    // Issue #981: the shared rung, not a local one. As in the
                    // sidecar's projection, the filter this replaces counted
                    // `Pending` — a report waiting on a human read to the
                    // orchestrator as one that had been lost, and it would act
                    // on that.
                    let undelivered = crate::ports::undelivered_count(deliveries);
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
        // Issue #1843. Structural only, like every arm here: which step, from
        // a fixed vocabulary — no company or operator free text involved.
        CompanyEvent::OnboardingStepCompleted { step } => match step {
            OnboardingStep::NameConfirmed => "activation step: name confirmed".to_string(),
            OnboardingStep::IntegrationConnected => {
                "activation step: integration connected".to_string()
            }
            OnboardingStep::WorkflowRunSucceeded => {
                "activation step: workflow run succeeded".to_string()
            }
        },
        CompanyEvent::OnboardingCompleted { .. } => "activation completed".to_string(),
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
            NO_DEPTH_BOUND,
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
///
/// Since issue #176 the same tool is also wired onto a **desk member** the
/// manifest opted in with `delegates_to`. That copy carries a [`MemberScope`],
/// which adds two more target checks on top of the grounding above — the
/// member's allowlist and the cycle guard — and names who is delegating so a
/// hand-off back to the caller's own desk can be caught. The orchestrator's copy
/// carries `None` and is unrestricted, exactly as before.
pub struct DelegateToDeskTool {
    queue: DelegationQueue,
    company: CompanyId,
    /// The company store, read at call time so the desk set is the **current**
    /// one. Deliberately not a snapshot captured when the agent was built: an
    /// operator can create a desk mid-session (the desk-creation overlay), and a
    /// stale snapshot would refuse a desk that exists — a worse failure than the
    /// one this grounding fixes.
    store: Arc<dyn CompanyStore>,
    /// Set when this copy of the tool belongs to a desk member rather than the
    /// orchestrator (issue #176). `None` is the orchestrator: unrestricted
    /// target set, no cycle guard, and depth 0 by construction.
    member: Option<MemberScope>,
}

/// Who is delegating, when it is a desk member rather than the orchestrator
/// (issue #176).
///
/// Both fields exist for the same reason and travel together: a member's
/// hand-off has to be checked against something the orchestrator's does not
/// have — the desks its manifest entry permits, and its own identity, so it
/// cannot hand work back to the desk it leads.
#[derive(Clone, Debug)]
pub struct MemberScope {
    /// The roster id of the member this tool is wired onto.
    pub member: String,
    /// The desks it may hand work to — its manifest
    /// [`delegates_to`](crate::company::Agent::delegates_to), with `"*"` meaning
    /// every desk.
    pub delegates_to: Vec<String>,
}

/// What one read of the company record decided about a hand-off target: the
/// refusal to return, if any, the canonical id it resolved to, and the depth
/// bound in force.
struct Grounding {
    /// The refusal to hand back to the model, or `None` when the target is good.
    refusal: Option<String>,
    /// The **canonical roster id** the key resolved to, when it resolved to one
    /// (issue #1162).
    ///
    /// What makes grounding at the tool boundary mean anything: the queued
    /// delegation carries this rather than the string the model typed, so the
    /// drain has nothing left to decide. Without it the tool could accept a
    /// display name, answer "Handed to …", and then have the drain fail to
    /// resolve the very key the tool just approved — a refusal traded for a
    /// silent drop.
    ///
    /// `None` on the fail-open path (the record could not be read, so there was
    /// nothing to resolve against) and for [`DelegateToDeskTool`], whose target
    /// is a desk: `desk_lead` and `resolve_desk_id` already accept a desk by id
    /// **or** name at both ends, so that path has no namespace to close.
    target: Option<String>,
    /// `[tools].max_delegation_depth`, or its default. Read from the same record
    /// load as the refusal so a single store round-trip decides both.
    max_depth: usize,
}

impl DelegateToDeskTool {
    /// Builds the orchestrator's unrestricted copy of the tool over the shared
    /// delegation queue and the company store it grounds the target against.
    pub fn new(queue: DelegationQueue, company: CompanyId, store: Arc<dyn CompanyStore>) -> Self {
        Self {
            queue,
            company,
            store,
            member: None,
        }
    }

    /// Builds a **desk member's** copy (issue #176): the same tool, narrowed to
    /// the desks `scope` permits and guarded against handing work back up its
    /// own chain.
    pub fn for_member(
        queue: DelegationQueue,
        company: CompanyId,
        store: Arc<dyn CompanyStore>,
        scope: MemberScope,
    ) -> Self {
        Self {
            queue,
            company,
            store,
            member: Some(scope),
        }
    }

    /// Grounds `desk` against the live company record: the refusal for it, if
    /// any, plus the depth bound this company runs under.
    ///
    /// **Fails open for the orchestrator, closed for a member** — see
    /// [`ungrounded`](Self::ungrounded) for why the two halves differ.
    ///
    /// Order matters. Grounding (#272) runs first — "there is no such desk"
    /// outranks "you may not reach that desk", because a model told the latter
    /// about a desk it invented would go on inventing. Then the allowlist, which
    /// is retryable in the same turn with a desk from the list the message
    /// names, and only then the cycle guard, which is not.
    async fn ground(&self, desk: &str) -> Grounding {
        let record = match self.store.load(&self.company).await {
            Ok(Some(record)) => record,
            Ok(None) => return self.ungrounded(desk, "this company's record is not there"),
            Err(err) => {
                tracing::warn!(
                    company = %self.company,
                    error = %err,
                    member = self.member.as_ref().map(|s| s.member.as_str()).unwrap_or("-"),
                    "[delegate_to_desk] could not read the company record to ground the desk target"
                );
                return self.ungrounded(desk, "this company's record could not be read");
            }
        };
        let max_depth = usize::from(
            record
                .manifest
                .tools
                .max_delegation_depth
                .unwrap_or(crate::company::DEFAULT_MAX_DELEGATION_DEPTH),
        );
        let refusal = delegation_tools::reject_desk_target(&record, desk).or_else(|| {
            let scope = self.member.as_ref()?;
            delegation_tools::reject_out_of_allowlist_target(&record, &scope.delegates_to, desk)
                .or_else(|| {
                    delegation_tools::reject_cycle_target(
                        &record,
                        &self.queue.scope_chain(),
                        desk,
                        &scope.member,
                    )
                })
        });
        Grounding {
            refusal,
            // A desk key needs no canonicalising: `resolve_desk_id` and
            // `desk_lead` both accept a desk by id or name, at the tool
            // boundary and again at the drain.
            target: None,
            max_depth,
        }
    }

    /// What a hand-off grounds to when the record behind the grounding could
    /// not be read at all — the two callers above.
    ///
    /// The **orchestrator** fails open, exactly as it has since #272: it has
    /// nothing to authorise (its target set is every desk), so an unreadable
    /// record costs it only the "there is no such desk" courtesy, and a store
    /// hiccup must not take delegation offline.
    ///
    /// A **member** fails closed. Its allowlist and its cycle guard are checked
    /// here and nowhere else — `run_delegation` executes what the queue holds
    /// without re-deriving either — so queuing ungrounded would hand the member
    /// the orchestrator's reach for the duration of the hiccup, one level below
    /// where anyone is looking. Refusing costs a retry; queuing costs the bound.
    fn ungrounded(&self, desk: &str, why: &str) -> Grounding {
        let Some(scope) = self.member.as_ref() else {
            return Grounding::open();
        };
        Grounding {
            refusal: Some(format!(
                "Could not hand `{desk}` off: {why}, so the desks {member} is allowed to reach \
                 could not be checked. Nothing was queued — try again, or carry the work out \
                 yourself.",
                member = scope.member
            )),
            target: None,
            max_depth: usize::from(crate::company::DEFAULT_MAX_DELEGATION_DEPTH),
        }
    }
}

impl Grounding {
    /// The fail-open grounding: nothing refused, default depth.
    fn open() -> Self {
        Self {
            refusal: None,
            // Nothing was read, so there is nothing to canonicalise: the key
            // goes to the queue as the model wrote it, and the drain does the
            // resolve instead (#1162).
            target: None,
            max_depth: usize::from(crate::company::DEFAULT_MAX_DELEGATION_DEPTH),
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
        // For a desk member (issue #176) this also refuses a target outside its
        // allowlist and one that would close a loop.
        let grounding = self.ground(&desk).await;
        if let Some(refusal) = grounding.refusal {
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
            grounding.max_depth,
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
// delegate_to_teammate (issue #884 — D1: a lead can reach a peer on its desk)
// ---------------------------------------------------------------------------

/// A delegation tool that hands a turn to a **named teammate**. Enqueues a
/// [`Delegation::DelegateToTeammate`]; the harness brain runs that teammate's
/// turn on drain and folds their reply back exactly as a desk hand-off's is.
///
/// The sibling of [`DelegateToDeskTool`] and deliberately its twin — same
/// grounding-then-`push_within_cap` shape, same [`PermissionLevel::Write`], same
/// per-turn cap and same depth bound. Only the target namespace differs.
///
/// # Why it has to exist
///
/// `delegate_to_desk` resolves to
/// [`desk_lead`](crate::runtime::delegation_tools::desk_lead) and nothing else,
/// so a desk's own lead could reach every desk in the company **except the
/// people sitting on its own**: handing work back to its desk is self-delegation
/// and refused. A three-person desk asked for one member by name therefore got a
/// polite decline from the lead, which was the only move it had.
///
/// # The target comes from the tool call, never from the message
///
/// `teammate` is validated against a closed set derived from the company record
/// (see [`reject_teammate_target`](crate::runtime::delegation_tools::reject_teammate_target)).
/// Reading a `Name:` prefix out of the operator's prose was the rejected
/// alternative: it is ambiguous ("ask the SEO Specialist to…" is not an
/// address), spoofable — a pasted email opening "SEO Specialist:" would pick
/// whose grants and whose budget run — undefined for a message naming two
/// people, and wrong in every language the personas are not written in. Routing
/// stays deterministic; reading the prose stays with the model.
pub struct DelegateToTeammateTool {
    queue: DelegationQueue,
    company: CompanyId,
    /// Read at call time so the roster is the **current** one — an operator can
    /// add a teammate mid-session, and a snapshot would refuse somebody who
    /// exists. Same reasoning as [`DelegateToDeskTool::store`].
    store: Arc<dyn CompanyStore>,
    /// Set when this copy belongs to a desk member rather than the orchestrator.
    /// `None` is the orchestrator: the whole roster, no allowlist, no self-check.
    member: Option<MemberScope>,
}

impl DelegateToTeammateTool {
    /// Builds the orchestrator's unrestricted copy.
    pub fn new(queue: DelegationQueue, company: CompanyId, store: Arc<dyn CompanyStore>) -> Self {
        Self {
            queue,
            company,
            store,
            member: None,
        }
    }

    /// Builds a **desk member's** copy: narrowed to the teammates it shares a
    /// desk with, plus anybody on a desk its `delegates_to` allowlist permits.
    pub fn for_member(
        queue: DelegationQueue,
        company: CompanyId,
        store: Arc<dyn CompanyStore>,
        scope: MemberScope,
    ) -> Self {
        Self {
            queue,
            company,
            store,
            member: Some(scope),
        }
    }

    /// Grounds `teammate` against the live record: the refusal for it, if any,
    /// plus the depth bound this company runs under.
    ///
    /// Same fail-open-for-the-orchestrator / fail-closed-for-a-member split
    /// [`DelegateToDeskTool::ungrounded`] documents, and the same ordering
    /// rationale: grounding first ("there is no such teammate" outranks "you may
    /// not reach them", or a model told the latter about somebody it invented
    /// goes on inventing), then the cycle guard, which is not retryable.
    async fn ground(&self, teammate: &str) -> Grounding {
        let record = match self.store.load(&self.company).await {
            Ok(Some(record)) => record,
            Ok(None) => return self.ungrounded(teammate, "this company's record is not there"),
            Err(err) => {
                tracing::warn!(
                    company = %self.company,
                    error = %err,
                    member = self.member.as_ref().map(|s| s.member.as_str()).unwrap_or("-"),
                    "[delegate_to_teammate] could not read the company record to ground the target"
                );
                return self.ungrounded(teammate, "this company's record could not be read");
            }
        };
        let max_depth = usize::from(
            record
                .manifest
                .tools
                .max_delegation_depth
                .unwrap_or(crate::company::DEFAULT_MAX_DELEGATION_DEPTH),
        );
        let scope = self.member.as_ref();
        let allowed: &[String] = scope.map(|s| s.delegates_to.as_slice()).unwrap_or(&[]);
        let refusal = delegation_tools::reject_teammate_target(
            &record,
            scope.map(|s| s.member.as_str()),
            allowed,
            teammate,
        )
        .or_else(|| {
            delegation_tools::reject_teammate_cycle_target(
                &record,
                &self.queue.scope_chain(),
                teammate,
            )
        });
        // Resolved from the same record read that decided the refusal, so the
        // id queued below is the id the refusal was (or was not) written about.
        // Pure over `record`, so it agrees with `reject_teammate_target`'s own
        // resolve by construction.
        let target = record.resolve_teammate_key(teammate).agent();
        Grounding {
            refusal,
            target,
            max_depth,
        }
    }

    /// The member/orchestrator split for an unreadable record — see
    /// [`DelegateToDeskTool::ungrounded`], which this mirrors exactly.
    fn ungrounded(&self, teammate: &str, why: &str) -> Grounding {
        let Some(scope) = self.member.as_ref() else {
            return Grounding::open();
        };
        Grounding {
            refusal: Some(format!(
                "Could not hand this to `{teammate}`: {why}, so the teammates {member} is allowed \
                 to reach could not be checked. Nothing was queued — try again, or carry the work \
                 out yourself.",
                member = scope.member
            )),
            target: None,
            max_depth: usize::from(crate::company::DEFAULT_MAX_DELEGATION_DEPTH),
        }
    }
}

#[async_trait]
impl Tool for DelegateToTeammateTool {
    fn name(&self) -> &str {
        DELEGATE_TO_TEAMMATE_TOOL
    }

    fn description(&self) -> &str {
        "Hand a turn to one named teammate so the person who actually owns that specialism answers — including somebody on your own desk. Provide the `teammate` (their roster id, as `query_company` lists them under Team) and the `instruction` to carry out. Use this instead of `delegate_to_desk` whenever a specific person is wanted rather than whoever leads a desk. A substantial hand-off is opened as a tracked board card automatically, assigned to them — you do not need to call `spawn_task` as well."
    }

    fn parameters_schema(&self) -> Value {
        crate::runtime::delegation_tools::delegate_to_teammate_schema()
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let teammate = required_str(&args, "teammate")?;
        let instruction = required_str(&args, "instruction")?;

        let grounding = self.ground(&teammate).await;
        if let Some(refusal) = grounding.refusal {
            tracing::info!(
                company = %self.company,
                "[delegate_to_teammate] refused an ungrounded hand-off target"
            );
            // Recorded as well as returned, the same independence #272 gave the
            // desk refusals: the model is free to describe its own turn however
            // it likes, so the board must carry the attempt regardless.
            self.queue.push_refusal(teammate);
            return Ok(ToolResult::error(refusal));
        }

        // Queue the **canonical id**, not the key as typed (issue #1162). The
        // grounding above is the only place the roster is read before the turn
        // ends, so whatever goes into the queue is what the drain must be able
        // to deliver to. A display name accepted here and re-resolved there
        // would turn a refusal the model can retry into a hand-off that is
        // answered "Handed to …" and then quietly never runs.
        //
        // Falls back to the key on the fail-open path — the record could not be
        // read, so nothing was resolved and nothing was refused either; the
        // drain resolves it with the same resolver.
        let target = grounding.target.unwrap_or_else(|| teammate.clone());
        let effect = format!("nothing was handed to {target}");
        match self.queue.push_within_cap(
            Delegation::DelegateToTeammate {
                teammate: target.clone(),
                instruction,
            },
            MAX_DELEGATIONS_PER_TURN,
            grounding.max_depth,
        ) {
            Staged::Queued => {}
            Staged::OverCap => return Ok(ToolResult::error(over_cap(&effect))),
            Staged::NoDrain(why) => {
                return Ok(ToolResult::error(no_drain(
                    DELEGATE_TO_TEAMMATE_TOOL,
                    &effect,
                    why,
                )));
            }
        }
        // Both strings when they differ: the model asked for a person by the
        // name it read, and the id is the token it should write next time.
        Ok(ToolResult::success(
            if target.eq_ignore_ascii_case(&teammate) {
                format!("Handed to {target}. They will answer this turn.")
            } else {
                format!("Handed to {teammate} (`{target}`). They will answer this turn.")
            },
        ))
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
            NO_DEPTH_BOUND,
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
            NO_DEPTH_BOUND,
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
        NoDrainReason::Depth => format!(
            "Refused: this work has already been handed on as far as this company allows, so \
             {effect}. You are the last link in the chain — do the part you can do yourself and \
             say plainly what still needs another desk, or open a task card for it with \
             `spawn_task`, which still works. Do not retry this call; it will fail the same way, \
             and do NOT report the hand-off as done."
        ),
        NoDrainReason::WorkflowLifecycle => format!(
            "Refused: you are running inside a workflow, which can put work on the board but \
             cannot move it through review, so {effect}. Deciding a card is done is the \
             operator's call, not this run's. You CAN open a card with `spawn_task` and set who \
             owns it with `assign_task` — do that and leave the verdict to a person. Do not retry \
             this call; it will fail the same way, and do NOT report the card as reviewed, \
             approved or moved."
        ),
        NoDrainReason::WorkflowHandOff => format!(
            "Refused: you are running inside a workflow, which has no conversation for a desk's \
             reply to come back to, so {effect}. A hand-off is only worth making when somebody is \
             waiting on the answer, and here nobody is. Open a card for that desk instead with \
             `spawn_task` — naming the desk as its assignee — which persists and reaches them. Do \
             not retry this call; it will fail the same way, and do NOT report the work as handed \
             over or the desk as having replied."
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
        Box::new(DelegateToDeskTool::new(
            queue.clone(),
            company.clone(),
            store.clone(),
        )),
        // Issue #884: the orchestrator can now reach a named teammate directly
        // rather than only whoever leads their desk.
        Box::new(DelegateToTeammateTool::new(queue.clone(), company, store)),
        Box::new(AssignTaskTool::new(queue.clone())),
        Box::new(ReviewTaskTool::new(queue.clone())),
    ]
}

/// The delegation tools a desk member gets when its manifest entry names a
/// `delegates_to` allowlist (issue #176): `spawn_task`, a `delegate_to_desk`
/// narrowed to that allowlist, and — since #884 — a `delegate_to_teammate`
/// narrowed to its own desk-mates plus the members of the desks that allowlist
/// permits.
///
/// Deliberately a subset of [`delegation_tools`] rather than the same list.
/// `assign_task`, `review_task`, `query_company`, `run_workflow`,
/// `create_workflow` and `add_agent` are the orchestrator's *authority* over the
/// company — who owns a card, whether work passes review, who is on the roster —
/// and #176 is about a lead pulling in a specialist, not about every desk lead
/// becoming a second CEO. A member gets exactly what it needs to pass a slice
/// on and to leave the rest tracked.
///
/// All three names are already covered by
/// [`is_delegation_tool`], so
/// [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) classifies them as
/// internal here exactly as it does on the orchestrator — no policy change comes
/// with this wiring.
pub fn member_delegation_tools(
    queue: &DelegationQueue,
    company: CompanyId,
    store: Arc<dyn CompanyStore>,
    scope: MemberScope,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(SpawnTaskTool::new(queue.clone())),
        Box::new(DelegateToDeskTool::for_member(
            queue.clone(),
            company.clone(),
            store.clone(),
            scope.clone(),
        )),
        // Issue #884, D1: without this a desk lead can reach every desk its
        // allowlist names and nobody at all on its own — the one hand-off it is
        // best placed to make.
        Box::new(DelegateToTeammateTool::for_member(
            queue.clone(),
            company,
            store,
            scope,
        )),
    ]
}

/// The persona brief appended for a desk member that may re-delegate (issue
/// #176).
///
/// It exists because a refusal costs a whole turn. A model handed
/// `delegate_to_desk` with no idea that its reach is narrowed, or that the chain
/// it is running inside is nearly at its bound, spends turns discovering both
/// one refusal at a time — and the depth refusal in particular is not
/// retryable, so a model that has not been told will burn every remaining call
/// on it. Naming the allowlist and the shape of the bound up front is cheaper
/// than the refusals it avoids.
///
/// The bound is stated qualitatively rather than as a number. The number lives
/// on the live company record and is read at call time; baking a snapshot of it
/// into a persona that is cached with the belt would be a claim that goes stale
/// the moment an operator edits the manifest — and a *confidently wrong* bound
/// is worse guidance than an honest "there is one".
pub fn member_delegation_brief(desks: &[String]) -> String {
    let reach = match desks.iter().any(|d| d.trim() == "*") {
        true => "any desk in the company".to_string(),
        false => desks.join(", "),
    };
    format!(
        "\n\n## Handing work on\n\nYou can pass a slice of your work to another desk with \
`delegate_to_desk`, to one named person with `delegate_to_teammate` — including somebody on your \
own desk — and open a tracked card for anything that should be followed up later with \
`spawn_task`. The desks you may hand work to: {reach}. When a request names a specific teammate, \
hand it to THAT PERSON with `delegate_to_teammate` rather than declining it as not \
yours.\n\nHand on only the part somebody else is genuinely better placed to do, and do the rest \
yourself — every hand-off costs another turn. The chain is bounded: if you are told the work has \
already been handed on as far as this company allows, that is final, so do what you can and say \
plainly what is left rather than calling the tool again. You cannot hand work back to a desk it \
already came from, to a desk you lead yourself, or to somebody the work already passed \
through.\n"
    )
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
/// entry with the standard company-wide tool grant.
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
    /// The id of the agent this tool is wired onto — the minter. Named in the
    /// mint log so an operator can see who added a teammate, and with what.
    minter: String,
    /// The minter's own `tools` line, verbatim (issue #1804's three-state
    /// grant): `None` means the minter inherits the company's standard grant,
    /// in which case so does the teammate; `Some(globs)` is the minter's own
    /// explicit scope. `Some(vec![])` (deny-all) never mints anything reachable.
    minter_tools: Option<Vec<String>>,
    /// The minter's **effective** grant — its line already narrowed by the
    /// company `allow`. The ceiling an explicit `tools` argument is clamped to.
    minter_grants: Vec<String>,
}

impl AddAgentTool {
    /// Builds the tool over the company id and its store handle
    /// ([`HarnessDeps::store`](crate::harness::HarnessDeps::store)), plus the
    /// minting agent's identity and tool scope (issue #619).
    ///
    /// # Why the minter's scope is a constructor argument
    ///
    /// #661 gave a minted teammate a `tools` list clamped to the **company**
    /// grant. That leaves the defect #619 was filed about intact: omitting
    /// `tools` still yields the company's *whole* grant, so an agent scoped to
    /// a corner of the company can mint a teammate holding everything the
    /// company holds — and `add_agent` is [`Reach::Nothing`](crate::policy)
    /// and sits in `INTRINSIC_TOOLS`, so it is always present and never asks.
    ///
    /// The ceiling is therefore the **minter**, not the company: a minted
    /// teammate is never wider than the agent that minted it.
    pub fn new(
        company: CompanyId,
        store: Arc<dyn CompanyStore>,
        minter: String,
        minter_tools: Option<Vec<String>>,
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
/// minter ceiling existed still describes the same company through it. A test
/// that cares about narrowing constructs the tool directly with a scoped
/// minter instead.
#[cfg(test)]
pub(crate) fn unscoped_add_agent(company: CompanyId, store: Arc<dyn CompanyStore>) -> AddAgentTool {
    AddAgentTool::new(
        company,
        store,
        "ceo".to_string(),
        // No line of its own — the minter inherits the company grant…
        None,
        // …which for these fixtures is the catch-all, so the minter ceiling is
        // wide open and a test about *other* behaviour is not accidentally a
        // test about the #619 clamp. A test that cares about the clamp uses
        // `scoped_add_agent`.
        vec!["*".to_string()],
    )
}

#[async_trait]
impl Tool for AddAgentTool {
    fn name(&self) -> &str {
        ADD_AGENT_TOOL
    }

    fn description(&self) -> &str {
        "Add a new teammate to the company. Provide a `name`, a `role` (job title), an optional `description` of their mandate, and an optional `tools` grant. The teammate becomes a real, addressable member of the roster starting next turn."
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
                    "description": "Optional per-teammate tool grant, as a list of tool-namespace globs (e.g. \"docs.*\", \"email\"). These are INTERSECTED with the company's allowed tools: the grant can only NARROW this teammate below the company-wide allow-list, never widen or escalate it past what the company already permits. Omit or leave empty to give the standard company-wide grant."
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
        // Issue #661 / L5: an optional per-teammate tool grant. The globs are
        // INTERSECTED with the company's `[tools].allow` at roster-build time
        // (`agent_effective_grants`), so this can only narrow the new teammate
        // below the company grant — never widen or escalate it. A non-string
        // item is a clean argument error, the same shape as a missing
        // `name`/`role`.
        let requested: Option<Vec<String>> = match args.get("tools") {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => {
                let mut globs = Vec::with_capacity(items.len());
                for item in items {
                    let glob = item
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("`tools` must be an array of strings"))?
                        .trim();
                    if !glob.is_empty() {
                        globs.push(glob.to_string());
                    }
                }
                Some(globs)
            }
            Some(_) => return Err(anyhow::anyhow!("`tools` must be an array of strings")),
        };
        // Issue #619: the company grant is the wrong ceiling. Clamp to the
        // MINTER's own scope, resolved before the store is touched so a refused
        // scope never leaves a half-written roster.
        //
        // The boolean says whether the request LEFT the grant unstated — the
        // `None` and empty-list cases, which inherit the minter — rather than
        // stating one explicitly. Only an unstated grant is filtered for the BYO
        // real-money namespaces below; an explicitly requested billing namespace
        // survives, narrowed to what the minter holds.
        // `tools` is the teammate's own three-state grant line (issue #1804):
        // `None` inherits the standard grant, `Some(vec![])` is an explicit
        // deny-all, `Some(globs)` narrows. `unstated` marks the inherit path,
        // the only one the BYO-billing filter below runs on.
        let (mut tools, unstated): (Option<Vec<String>>, bool) = match requested {
            // Nothing asked for: copy the minter's own line verbatim. Copying the
            // *line* (which is itself `None` for an unscoped minter) rather than
            // its resolved grant is deliberate — an unscoped minter mints an
            // unscoped teammate that keeps tracking `[tools].allow`, instead of
            // freezing today's allow-list into the record as an explicit scope a
            // later company-wide narrowing would not reach.
            None => (self.minter_tools.clone(), true),
            // An explicitly empty list is a deliberate deny-all since #1804 — the
            // most restrictive scope an agent can hand a new teammate — NOT
            // "inherit". This is the contract inversion: `[]` no longer means
            // "give them what you have".
            Some(globs) if globs.is_empty() => (Some(Vec::new()), false),
            Some(globs) => {
                // Narrow against what the minter actually holds. An empty result
                // means nothing asked for was within reach, so refuse rather than
                // store `Some(vec![])` — an agent asking for tools it cannot
                // reach meant to scope, not to mint a powerless teammate.
                let narrowed = agent_effective_grants(&self.minter_grants, Some(&globs));
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
                (Some(narrowed), false)
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

        // The BYO real-money namespaces are not inherited by a minted teammate
        // (#788/#789). What an unstated grant inherits depends on the minter:
        // an empty minter line means the whole company allow-list (so an
        // unscoped-looking mint from, say, a desk-restricted creative director
        // would otherwise store an empty grant that reads back as billing it
        // does not hold), and a non-empty minter line that itself names billing
        // (the shipped bookkeeper) would hand it on. Both are filtered — the
        // company allow-list when the minter line is empty, the minter's own
        // line otherwise — before persistence.
        //
        // Same helper and same reasoning as the console `POST .../team` route,
        // deliberately rather than incidentally: two creation paths that answer
        // "what does an unstated grant mean" differently is how the first hole
        // got here. `CreationGrant::Standard` leaves the copied line untouched,
        // so nothing changes for the companies that grant none of these, and an
        // explicitly requested billing namespace is untouched too.
        if unstated {
            // `tools` here is the minter's own line, copied verbatim: `None`
            // for an unscoped minter (inherit the whole company allow-list),
            // `Some(line)` for a scoped one (inherit exactly that line). The
            // BYO-billing filter runs on whichever the teammate would inherit.
            let inherited: &[String] = match tools.as_deref() {
                None => &record.manifest.tools.allow,
                Some(line) => line,
            };
            match crate::company::creation_default_grants(inherited) {
                crate::company::CreationGrant::Standard => {}
                crate::company::CreationGrant::Narrowed(narrowed) => tools = Some(narrowed),
                crate::company::CreationGrant::NothingLeft => {
                    return Ok(ToolResult::error(format!(
                        "This company grants only billing namespaces, so \"{name}\" would inherit them. Pass an explicit `tools` list naming what they should hold."
                    )));
                }
            }
        }

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
            model: None,
            harness: None,
        };
        record.overlay_agents.push(agent);
        self.store.save(&record).await?;

        // Issue #619: the mint is observable — the minter, the teammate, and
        // the grant it was given. This was the condition attached to sanctioning
        // the narrowing at all: `add_agent` is `Reach::Nothing` and never asks,
        // so this log is the only place the decision is visible. A narrowing
        // that happens silently is the defect being fixed, one layer down.
        //
        // An **inherited** grant is the line an operator most needs to see,
        // because that is the teammate holding everything its minter holds.
        tracing::info!(
            company = %self.company,
            minter = %self.minter,
            teammate = %id,
            teammate_name = %name,
            scope = %match tools.as_deref() {
                None => "inherited: the minter's own standard grant".to_string(),
                Some([]) => "none: an explicit deny-all".to_string(),
                Some(globs) => globs.join(", "),
            },
            "[add_agent] minted an overlay teammate"
        );

        // The id is in the result because the orchestrator has to be able to
        // address the teammate it just created — delegating to it, or putting it
        // on a desk, takes the id, not the display name. The console gets the
        // same answer from `TeamMemberDto.id`; before this the agent-facing half
        // had no way to learn it at all.
        // The scope is in the result for the same reason it is in the log: the
        // minting agent should see what it handed over, and "the same tools you
        // hold" is a materially different answer from a named list.
        let scope = match tools.as_deref() {
            None => "They hold the same tools you do.".to_string(),
            Some([]) => "They hold no tools.".to_string(),
            Some(globs) => format!("Their tools are scoped to: {}.", globs.join(", ")),
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
/// tool writes through. `tasks` / `runs` / `artifacts` (issue #1859) back the
/// `list_tasks`, `read_task` and `read_run` execution-state read trio, plus
/// `query_company`'s `## Board` section — any of the three may be `None`, in
/// which case the surface it backs answers that it is unavailable rather than
/// failing the call.
// One more dependency than clippy's threshold, and each is a distinct wired
// port the orchestrator's tools need. Bundling them into a struct would only
// relocate the surface — the same call is made from exactly one place
// (`build_agent`), so there is nothing to deduplicate.
#[allow(clippy::too_many_arguments)]
pub fn orchestrator_tools(
    company: CompanyId,
    facts: Option<Arc<dyn FactStore>>,
    events: Option<Arc<dyn EventLog>>,
    // Issue #1859: the board + run-history read surface. See the doc comment
    // above for why any of the three may be `None`.
    tasks: Option<Arc<dyn TaskStore>>,
    runs: Option<Arc<dyn RunStore>>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
    queue: &DelegationQueue,
    workflow_source_dir: Option<PathBuf>,
    workflow_runner: WorkflowRunnerHandle,
    run_supervisor: crate::runtime::RunSupervisor,
    store: Arc<dyn CompanyStore>,
    // Issue #274's snapshot ring, for the #661 (M7) edit/delete tools. `None`
    // makes those two refuse rather than write with no undo — see
    // `HarnessDeps::workflow_revisions`.
    workflow_revisions: Option<Arc<dyn crate::ports::WorkflowRevisionStore>>,
    workflow_refs: WorkflowRefQueue,
    run_outputs: RunOutputCache,
    minter: String,
    minter_tools: Option<Vec<String>>,
    minter_grants: Vec<String>,
    // Issue #1865: where `run_workflow` files a `workflow_run_failed`
    // notification on a run the agent itself started — see
    // `RunWorkflowTool::notifications` for why this is optional.
    notifications: Option<Arc<dyn NotificationStore>>,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(QueryCompanyTool::new(
        company.clone(),
        facts,
        events.clone(),
        workflow_source_dir.clone(),
        Some(store.clone()),
        tasks.clone(),
    ))];
    // Issue #1859: the board/run-history read trio, grouped right after
    // `query_company` — all four answer "what does the company know?" rather
    // than acting on it. `list_tasks` and `read_task` share the board ports;
    // `read_run` additionally needs the event log to fold a workflow run's
    // outcome when the id names no agent-attempt row.
    tools.push(Box::new(ListTasksTool::new(
        company.clone(),
        tasks.clone(),
        runs.clone(),
    )));
    tools.push(Box::new(ReadTaskTool::new(
        company.clone(),
        tasks,
        runs.clone(),
        artifacts,
    )));
    tools.push(Box::new(ReadRunTool::new(
        company.clone(),
        runs,
        events.clone(),
    )));
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
        notifications,
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
        workflow_source_dir.clone(),
        store.clone(),
        events.clone(),
        workflow_refs,
    )));
    // Issue #661 (M7): the other three quarters of the workflow-authoring
    // surface — read the graph, replace it, remove it. Registered right after
    // `create_workflow` because they are its lifecycle: without them an agent
    // that got a graph wrong could only create a second one beside it forever.
    // All three share one handle, so the ports can never be wired to some of
    // them and not others. See [`crate::harness::workflow_admin`].
    let workflow_admin = crate::harness::workflow_admin::WorkflowAdmin::new(
        company.clone(),
        workflow_source_dir,
        store.clone(),
        workflow_revisions,
        events,
    );
    tools.push(Box::new(
        crate::harness::workflow_admin::ReadWorkflowTool::new(workflow_admin.clone()),
    ));
    tools.push(Box::new(
        crate::harness::workflow_admin::UpdateWorkflowTool::new(workflow_admin.clone()),
    ));
    tools.push(Box::new(
        crate::harness::workflow_admin::DeleteWorkflowTool::new(workflow_admin),
    ));
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
    /// Issue #1865 (PR #1883 review comment 3877185396): where a failed run
    /// files its `workflow_run_failed` notification, on the same terms as the
    /// console run route, the cron scheduler, and the approval-resume path —
    /// this tool is the one run-outcome chokepoint `WorkflowSpawn` does not
    /// cover (see [`crate::runtime::WorkflowSpawn`]'s own `notifications` doc
    /// comment), because an agent-started run stays inside the calling turn
    /// rather than routing through `WorkflowSpawn::spawn`.
    ///
    /// `None` (the default build, and most of the tool's own tests) simply
    /// skips the notification — the run itself still journals and answers the
    /// tool call either way, exactly as `events` degrades above; only the
    /// company-wide alert is lost.
    notifications: Option<Arc<dyn NotificationStore>>,
}

impl RunWorkflowTool {
    /// Builds the tool over the company id, its on-disk source directory
    /// (`companies/<name>`, whose `workflows/` subtree holds the seed graphs),
    /// the company store (holding the runtime-authored graph bodies), the
    /// shared runner handle, the company's journal, the shared queue a
    /// dispatched card's output link is staged on (issue #339), the run
    /// output cache the `read_run_output` companion reads back (issue #418),
    /// and the company's notification store a failed run alerts through
    /// (issue #1865).
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
        notifications: Option<Arc<dyn NotificationStore>>,
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
            notifications,
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
        let (overlays, globals_disable) = match self.store.load(&self.company).await {
            Ok(record) => record
                .map(|r| (r.overlay_workflows, r.manifest.globals.disable))
                .unwrap_or_default(),
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
        let file = match load_workflow_with_globals(
            self.source_dir.as_deref(),
            &overlays,
            &globals_disable,
            &wid,
        ) {
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
        // Issue #1865: this call sits inside an agent turn (see the #383
        // comment above — the run is deliberately NOT spawned onto its own
        // task, because spawning would reset the task-local `WORKFLOW_DEPTH`
        // re-entry guard mid-chain), so it cannot route through
        // `WorkflowSpawn`'s own `catch_unwind` the way the console run route
        // and the cron scheduler do. Left uncaught, a panic in the runner
        // future unwound straight past both journal-write arms below, so the
        // run's `WorkflowRunStarted` never got a matching finish and
        // `GET …/workflows/runs` read it `running: true` until the next boot
        // sweep — which does not run on rebuild, so a panicked agent-run could
        // zombie for the life of the process. This is its own catch rather
        // than a second call into `WorkflowSpawn`: that type owns a
        // `RunGuard`/supervisor registration this call already holds via
        // `_run_guard` above, and re-raising (as the spawned-task catch does,
        // so its `JoinHandle` still resolves to a `JoinError`) is wrong here —
        // there is no task boundary to preserve, only a tool call to answer,
        // so the payload is swallowed after the finish is journaled and this
        // returns an ordinary `ToolResult::error` instead.
        match std::panic::AssertUnwindSafe(runner.run(&self.company, &file, input, &ctx))
            .catch_unwind()
            .await
        {
            Err(_payload) => {
                tracing::error!(
                    company = %self.company,
                    workflow = %wid,
                    run_id = %ctx.run_id,
                    "run_workflow: the runner panicked; journaling a finish so the run does not \
                     read as in-flight forever"
                );
                if let Some(events) = self.events.as_ref() {
                    let journaled = crate::runtime::record_run_finished(
                        events,
                        &self.company,
                        &wid,
                        false,
                        &ctx.run_id,
                        Err(crate::runtime::workflow_spawn::PANICKED_BEFORE_FINISH.into()),
                    )
                    .await;
                    if !journaled {
                        tracing::error!(
                            company = %self.company,
                            workflow = %wid,
                            run_id = %ctx.run_id,
                            "run_workflow: a panicked run's finish could not be journaled; it \
                             will read as in-flight until the next boot sweep settles it"
                        );
                    }
                }
                // Issue #1865 (PR #1883 review comment 3877518535): a panic is
                // unambiguously the worst reading a run can settle with —
                // notify without needing a verdict computation, mirroring
                // `WorkflowSpawn::spawn_admitted`'s own panic arm. Fired
                // unconditionally like the journal write above, not gated on
                // it landing — the two are independent stores, and a journal
                // miss must not also cost the alert.
                if let Some(notifications) = self.notifications.as_ref() {
                    crate::runtime::file_run_unhealthy_notification(
                        notifications.as_ref(),
                        &self.company,
                        &wid,
                        &ctx.run_id,
                        "failed",
                        crate::runtime::PANICKED_BEFORE_FINISH,
                    )
                    .await;
                }
                // No re-raise: unlike `WorkflowSpawn`'s catch, there is no
                // `JoinHandle` here to preserve a `JoinError` on — this call is
                // itself the tool's execution, so the honest answer is an
                // ordinary tool failure the agent can read and act on.
                return Ok(ToolResult::error(format!(
                    "Workflow `{wid}` hit an internal error while running. Its completed steps, \
                     if any, are recorded in the run history. Don't retry it in a loop — check \
                     the run history or ask an operator."
                )));
            }
            Ok(Ok(run)) => {
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
                // Issue #1865 (PR #1883 review comment 3877518530): the same
                // unhealthy-run classification `WorkflowSpawn::spawn_admitted`
                // applies to its own settled runs — a stranded or blocked
                // agent-started run is otherwise silent to every operator not
                // watching this turn, especially a stranded run with no
                // approval card to surface.
                //
                // Issue #1865 (PR #1883 review comment 3878430677): gated on
                // `!run.cancelled`, matching `WorkflowSpawn::spawn_admitted`'s
                // own `Ok(run) if run.cancelled => {}` arm (added for the same
                // comment). The clean node-boundary cancel arm in
                // `run_workflow_inner` carries `blocked_nodes: blocks.take()`
                // forward, so a cancelled run reaches here with a non-empty
                // `blocked_nodes` exactly like a genuinely blocked one — this
                // must not tell an operator "a step is waiting on a person to
                // decide something" about a run somebody already stopped.
                //
                // Stranded checked before blocked, same as `WorkflowSpawn`:
                // `HarnessAgentRunner` pushes a `WorkflowBlockedNode` whenever
                // a turn gated anything at all, parked or not, so a fully
                // unparkable node lands in `blocked_nodes` exactly like one
                // with a live card — only `stranded_approvals` equalling the
                // full pending count, with no card still `Pending` delivery
                // either, tells the two apart.
                if let Some(notifications) = self.notifications.as_ref() {
                    if run.cancelled {
                        // Handled below by the `run.cancelled` arm, which
                        // returns a `ToolResult::error` — no unhealthy
                        // notification for a deliberate stop.
                    } else if !run.pending_approvals.is_empty()
                        && crate::ports::workflow_runner::stranded_approvals(
                            &run.pending_approvals,
                            &run.approvals,
                        ) == run.pending_approvals.len()
                        && !run
                            .deliveries
                            .iter()
                            .any(|d| matches!(d.status, crate::ports::DeliveryStatus::Pending))
                    {
                        crate::runtime::file_run_unhealthy_notification(
                            notifications.as_ref(),
                            &self.company,
                            &wid,
                            &ctx.run_id,
                            "stranded",
                            "This run tried to park an approval and could not — nothing is \
                             waiting on it any more, and nobody was asked.",
                        )
                        .await;
                    } else if !run.blocked_nodes.is_empty() {
                        crate::runtime::file_run_unhealthy_notification(
                            notifications.as_ref(),
                            &self.company,
                            &wid,
                            &ctx.run_id,
                            "blocked",
                            "This run stopped because a step is waiting on a person to decide \
                             something.",
                        )
                        .await;
                    }
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
                        // Issue #881: structural counts beside the prose, so a
                        // model reading only the JSON still learns the run
                        // delivered nothing.
                        "blocked_nodes": run.blocked_nodes.len(),
                        // Issue #900: `outcome == Parked` only, unlike
                        // `WorkflowRun::approvals`'s own receipt semantics
                        // (which deliberately count `ParkFailed` / `Discarded`
                        // too — see that field's doc comment). This key has no
                        // sibling field to carry the failure count the way the
                        // console's prose does with "N calls could not be
                        // queued", so a bare `approvals_parked` here has to
                        // mean what its name says: cards actually sitting on
                        // the Approvals page, not every receipt this run
                        // filed.
                        "approvals_parked": run
                            .approvals
                            .iter()
                            .filter(|a| a.outcome == crate::ports::WorkflowApprovalOutcome::Parked)
                            .count(),
                    }),
                    md,
                ))
            }
            Ok(Err(err)) => {
                tracing::debug!(company = %self.company, workflow = %wid, error = %err, "run_workflow: run failed");
                if let Some(events) = self.events.as_ref() {
                    let message = err.to_string();
                    crate::runtime::record_run_finished(
                        events,
                        &self.company,
                        &wid,
                        false,
                        &ctx.run_id,
                        // Issue #1008: an agent-started run journals what it did
                        // before it broke on exactly the same terms as a console
                        // or scheduled one — the three entry points share this
                        // helper so their history cannot drift.
                        Err(crate::runtime::FailedRun {
                            error: message.as_str(),
                            partial: err.partial_run(),
                        }),
                    )
                    .await;
                }
                // Issue #1865 (PR #1883 review comment 3877185396): this is
                // the second run-outcome chokepoint alongside `WorkflowSpawn`
                // — console, scheduled, and resumed failures already file a
                // `workflow_run_failed` notification through that type, but
                // an agent-started run never routed through it (see
                // `WorkflowSpawn`'s own `notifications` doc comment) and so
                // stayed silent to every operator not watching this turn.
                // Fired unconditionally like the journal write above, not
                // gated on it landing — the two are independent stores, and a
                // journal miss must not also cost the alert.
                if let Some(notifications) = self.notifications.as_ref() {
                    crate::runtime::file_run_unhealthy_notification(
                        notifications.as_ref(),
                        &self.company,
                        &wid,
                        &ctx.run_id,
                        "failed",
                        crate::runtime::RUN_FAILED_DETAIL,
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
    // A declined node is scrubbed from `output`, so without this it is
    // indistinguishable from one the run never reached.
    let declined: Vec<&str> = run
        .nodes
        .iter()
        .filter(|n| n.status == WorkflowNodeStatus::Declined)
        .map(|n| n.node_id.as_str())
        .collect();
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
                    None if declined.contains(&id) => md.push_str(&format!(
                        "- **{name}** (`{id}`, {kind}): not needed — the step stopped here on \
                         purpose\n"
                    )),
                    None => md.push_str(&format!("- **{name}** (`{id}`, {kind}): not reached\n")),
                }
            }
        }
        _ => md.push_str("_No per-node output was produced._\n"),
    }

    // Issue #881: a blocked node and a paused gate are BOTH in
    // `pending_approvals`, and they need different sentences. Approving a
    // paused gate continues the run; approving a blocked node's card does not —
    // an agent node is not re-enterable, so the only way forward is to run the
    // workflow again. Telling the model "resolve these for the run to continue"
    // about a blocked node would have it wait for a continuation that is never
    // coming.
    let blocked: Vec<&str> = run
        .blocked_nodes
        .iter()
        .map(|b| b.node_id.as_str())
        .collect();
    let paused: Vec<&str> = run
        .pending_approvals
        .iter()
        .map(String::as_str)
        .filter(|id| !blocked.contains(id))
        .collect();
    if !blocked.is_empty() {
        md.push_str(&format!(
            "\n**Blocked, waiting on a person** at: {}. {} produced no output and nothing after \
             {} ran, because a tool call in the step needed approval. This run parked {} \
             approval(s) and will NOT continue on its own — the approval has to be decided and \
             the workflow run again.\n",
            blocked.join(", "),
            if blocked.len() == 1 {
                "That step"
            } else {
                "Those steps"
            },
            if blocked.len() == 1 { "it" } else { "them" },
            run.approvals.len()
        ));
    }
    if !paused.is_empty() {
        md.push_str(&format!(
            "\n**Paused for approval** at: {}. Resolve these for the run to continue.\n",
            paused.join(", ")
        ));
    }
    if !declined.is_empty() {
        md.push_str(&format!(
            "\n**{} step(s) were declined as not needed:** {}. Each judged the work already done \
             or unnecessary and stopped its own branch deliberately — this is not a failure, but \
             nothing downstream of {} ran.\n",
            declined.len(),
            declined.join(", "),
            if declined.len() == 1 { "it" } else { "them" }
        ));
    }
    if blocked.is_empty() && paused.is_empty() && declined.is_empty() {
        md.push_str("\nThe run reached its terminal node(s) without pausing for approval.\n");
    }

    // Codex (PR #1883 review comment 3892522591): a node under `on_error =
    // "continue"`/`"route"`, or one truncated at the iteration cap, settles
    // this run as `Degraded` — `runner.rs` already turns that into a per-node
    // notice (`errored_node_notice`) the console reads, but nothing here ever
    // read it. An agent-started run only checked the blocked/paused and
    // delivery cases above, so a run that silently continued past a broken
    // step summarized as "reached its terminal node(s)" with no hint a step
    // was skipped over — the model then reports a clean run to whoever asked
    // for one, exactly the silence issue #981 closed for dropped deliveries
    // two blocks below, just for a different fact.
    //
    // Read `run.nodes` directly rather than `run.notices`: `notices` also
    // carries `blocked_notice` for every row already named above, and
    // rendering the whole vector here would print those a second time. By the
    // time a run settles, a row is still `Error` only when it is a genuine
    // continued/capped error — the host's own blocked-node reclassification
    // (mirroring `WorkflowRun::cancelled`'s) always leaves a blocked row
    // `Blocked`, never `Error`, so this filter can never double up with
    // `blocked` above. See `WorkflowNodeStatus::Blocked`'s doc.
    let errored: Vec<&str> = run
        .nodes
        .iter()
        .filter(|n| n.status == WorkflowNodeStatus::Error)
        .map(|n| n.node_id.as_str())
        .collect();
    if !errored.is_empty() {
        md.push_str(&format!(
            "\n**{} step(s) did not finish cleanly, and the run continued past {}:** {}. This is \
             NOT a clean run — check each step's own output for what went wrong before treating \
             its results as complete.\n",
            errored.len(),
            if errored.len() == 1 { "it" } else { "them" },
            errored.join(", ")
        ));
    }

    // Issue #981: what happened to the reports. Nothing here read `deliveries`,
    // so a run whose report was refused closed with the sentence above and
    // nothing else — and "reached its terminal node(s)" is true of exactly that
    // run, which is what made the silence so convincing. The model then reports
    // a clean run to whoever asked for one, and nobody learns the report is
    // gone until somebody notices it never arrived.
    //
    // **Reason, never `detail`.** `DeliveryReason` is the log-safe half of the
    // pair (issue #248): a mail transport's refusal quotes the mailbox it
    // refused, and `detail` is for the operator's own surfaces. This summary is
    // a model's tool result and goes wherever that turn goes, so it takes the
    // closed set — which says what class of thing failed and has no field that
    // could carry an address.
    let undelivered: Vec<&crate::ports::DeliveryReport> = run
        .deliveries
        .iter()
        .filter(|d| crate::ports::is_undelivered(d))
        .collect();
    if !undelivered.is_empty() {
        md.push_str(&format!(
            "\n**{} report(s) did NOT reach a destination.** The graph ran and its work stands — \
             delivery happens after the engine returns, so no node failed and none of the \
             per-node lines above is wrong. But the report did not go out, and it will not \
             without a change:\n",
            undelivered.len()
        ));
        for report in &undelivered {
            md.push_str(&format!(
                "- `{}` ({}): {}\n",
                report.node, report.kind, report.reason
            ));
        }
        md.push_str(
            "There is no retry: fix the destination or the runtime wiring and run the workflow \
             again.\n",
        );
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
pub(crate) struct CreateWorkflowArgs {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    owner_desk: Option<String>,
    #[serde(default)]
    nodes: Vec<CreateWorkflowArgNode>,
    #[serde(default)]
    edges: Vec<CreateWorkflowArgEdge>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateWorkflowArgNode {
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
pub(crate) struct CreateWorkflowArgEdge {
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
                // Same reason as the three above: a repeat guard (issue #850)
                // is a safety declaration about a call reaching a counterparty,
                // which is the operator's to make, not the agent's to author.
                repeatable: None,
                destination: n.destination,
                postcondition: None,
                verify: None,
            });
        }
        Ok(Self {
            id: args.id,
            name: args.name,
            description: args.description,
            owner_desk: RawWorkflow::normalize_owner_desk(args.owner_desk),
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
        create_workflow_parameters_schema()
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
        // `wired_channels: None` (issue #1191) — this tool holds a store and an
        // event log, not a `CompanyRuntime`, and the deliverable channel set can
        // only be read off a running runtime. `None` means "cannot see the
        // wiring", so the channel-destination rule is skipped rather than
        // guessed at; delivery's own `ChannelNotWired` refusal stays the
        // backstop for a graph authored this way. Deliberately unchanged by
        // #1191, which moved the rule into this core so the paths that CAN see
        // the wiring all run it; giving the agent tools a runtime handle is a
        // separate change.
        match create_company_workflow(
            &self.company,
            self.source_dir.as_deref(),
            &self.store,
            self.events.as_ref(),
            draft,
            None,
            // Issue #1843: an agent authoring a graph on its own initiative is
            // not the human activation signal `by` exists to capture — keep
            // this path unattributed, same as before this field existed.
            None,
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
                    // A structured workflow rejection (issue #1016) carries the
                    // joined problem text via `Display`, so the agent reads why.
                    OpenCompanyError::WorkflowInvalid { .. } => err.to_string(),
                    _ => "the company couldn't save it right now; try again.".to_string(),
                };
                Ok(ToolResult::error(format!(
                    "Couldn't create the workflow: {detail}"
                )))
            }
        }
    }
}

/// The `create_workflow` parameter schema, lifted out of the tool so
/// [`UpdateWorkflowTool`](crate::harness::workflow_admin::UpdateWorkflowTool)
/// advertises the SAME graph shape it deserializes (issue #661, M7).
///
/// Both tools parse [`CreateWorkflowArgs`]; a second hand-written copy of this
/// object would be free to drift from that struct — and from this one — with
/// nothing to notice. The update tool adds `expected_version` to the result and
/// rewrites `required`; everything else is shared by construction.
pub(crate) fn create_workflow_parameters_schema() -> Value {
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
                "ownerDesk": {
                    "type": ["string", "null"],
                    "description": "Optional owning desk id. Use the stable desk id, not its display name. On update_workflow, send `null` to explicitly unassign the desk; omit the key to leave the current desk untouched."
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::tasks::TaskTitle;
    use std::sync::Mutex as StdMutex;

    use crate::ports::runs::RunStatus;
    use crate::ports::types::{CompanyRecord, CompanySummary, LedgerEntry};

    fn agent(id: &str, tier: Option<&str>) -> ManifestAgent {
        ManifestAgent {
            global: false,
            id: id.to_string(),
            role: "Role".to_string(),
            name: None,
            description: None,
            tier: tier.map(str::to_string),
            harness: None,
            tools: None,
            delegates_to: Vec::new(),
            context: None,
            budget_usd_daily: None,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
                    NO_DEPTH_BOUND,
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
                NO_DEPTH_BOUND,
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
                NO_DEPTH_BOUND,
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
                    NO_DEPTH_BOUND,
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
            queue.push_within_cap(spawn(), MAX_DELEGATIONS_PER_TURN, NO_DEPTH_BOUND),
            Staged::NoDrain(NoDrainReason::Unwired)
        );
        let claim = queue.claim_answering();
        assert_eq!(
            queue.push_within_cap(spawn(), MAX_DELEGATIONS_PER_TURN, NO_DEPTH_BOUND),
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
                NO_DEPTH_BOUND,
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

    /// Issue #884: the teammate hand-off is internal work too. Left out, the
    /// approval policy would read it as an external effect and park every
    /// hand-off behind an operator approval — and the new edge would sit outside
    /// the loop checks every other delegation passes through.
    #[test]
    fn the_teammate_hand_off_is_an_internal_delegation_tool() {
        assert!(is_delegation_tool(DELEGATE_TO_TEAMMATE_TOOL));
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
        // Issue #884: and the teammate hand-off, exactly once — a duplicate name
        // on one belt is what the `else if` in `build` exists to prevent.
        assert_eq!(
            names
                .iter()
                .filter(|n| *n == DELEGATE_TO_TEAMMATE_TOOL)
                .count(),
            1,
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

    // --- Recursive desk delegation (issue #176) -----------------------------

    /// A three-desk record where two desks have roster leads, so a member of one
    /// can be given an allowlist that admits one desk and not another.
    fn nested_desks_record(id: &CompanyId) -> CompanyRecord {
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
delegates_to = ["research"]

[[agent]]
id = "analyst"
role = "Analyst"

[[group_chat]]
id = "strategy"
name = "Strategy desk"
members = ["writer"]

[[group_chat]]
id = "research"
name = "Research desk"
members = ["analyst"]

[[group_chat]]
id = "legal"
name = "Legal desk"
members = ["ceo"]
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            manifest,
            ..seeded_record(id)
        }
    }

    /// The `writer`'s copy of `delegate_to_desk`: allowed `research` only.
    fn member_desk_tool(record: CompanyRecord, queue: &DelegationQueue) -> DelegateToDeskTool {
        let company = record.id.clone();
        DelegateToDeskTool::for_member(
            queue.clone(),
            company,
            Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>,
            MemberScope {
                member: "writer".to_string(),
                delegates_to: vec!["research".to_string()],
            },
        )
    }

    /// Depth is the length of the scope chain, and it gates **hand-offs only**.
    ///
    /// At the bound a `delegate_to_desk` is refused with the new
    /// [`NoDrainReason::Depth`], while a `spawn_task` still stages — refusing
    /// that too would push a member that has hit the bound into working silently
    /// rather than leaving the work tracked.
    #[test]
    fn push_within_cap_refuses_a_hand_off_past_the_depth_bound() {
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let hand_off = || Delegation::DelegateToDesk {
            desk: "research".to_string(),
            instruction: "dig into it".to_string(),
        };
        let card = || Delegation::SpawnTask {
            title: "follow up".to_string(),
            note: None,
            assignee: None,
        };

        // Depth 0 (the orchestrator's own turn) under a bound of 1: allowed.
        assert_eq!(queue.scope_depth(), 0);
        assert_eq!(
            queue.push_within_cap(hand_off(), MAX_DELEGATIONS_PER_TURN, 1),
            Staged::Queued
        );
        queue.clear();

        // One level in, under a bound of 1: refused as depth-capped.
        let scope = queue.enter_scope("strategy".to_string());
        assert_eq!(queue.scope_depth(), 1);
        assert_eq!(
            queue.push_within_cap(hand_off(), MAX_DELEGATIONS_PER_TURN, 1),
            Staged::NoDrain(NoDrainReason::Depth)
        );
        // …while the board write at the same depth is untouched.
        assert_eq!(
            queue.push_within_cap(card(), MAX_DELEGATIONS_PER_TURN, 1),
            Staged::Queued
        );
        queue.clear();
        // …and the same hand-off under the default bound of 2 stages.
        assert_eq!(
            queue.push_within_cap(hand_off(), MAX_DELEGATIONS_PER_TURN, 2),
            Staged::Queued
        );
        queue.clear();

        // Two levels in, under a bound of 2: refused.
        let deeper = queue.enter_scope("research".to_string());
        assert_eq!(queue.scope_depth(), 2);
        assert_eq!(
            queue.push_within_cap(hand_off(), MAX_DELEGATIONS_PER_TURN, 2),
            Staged::NoDrain(NoDrainReason::Depth)
        );

        // The guards pop on drop, outermost last.
        drop(deeper);
        assert_eq!(queue.scope_depth(), 1);
        drop(scope);
        assert_eq!(queue.scope_depth(), 0);
    }

    /// The refusal has to be countable and distinguishable from the two that
    /// preceded it, and its text must not claim either of their causes — the
    /// same message would tell a fully capable company that its context cannot
    /// do board work.
    #[test]
    fn the_depth_refusal_is_its_own_reason_and_its_own_sentence() {
        assert_eq!(NoDrainReason::Depth.as_str(), "depth_capped");
        for other in [NoDrainReason::Unwired, NoDrainReason::Triage] {
            assert_ne!(NoDrainReason::Depth.as_str(), other.as_str());
        }
        let text = no_drain(
            DELEGATE_TO_DESK_TOOL,
            "nothing was handed to the research desk",
            NoDrainReason::Depth,
        );
        assert!(text.contains("as far as this company allows"), "{text}");
        assert!(
            text.contains("`spawn_task`"),
            "the model must be told what still works: {text}"
        );
        assert!(
            !text.contains("question"),
            "a depth refusal must not borrow the triage cause: {text}"
        );
        assert!(
            !text.contains("unavailable in this context"),
            "a depth refusal must not borrow the unwired cause: {text}"
        );
    }

    /// The chain ends with the claim, on **both** boundaries.
    ///
    /// The exit half is the load-bearing one: a `ScopeGuard` pops on every
    /// ordinary exit, but a panic inside a nested turn unwinds past it, and a
    /// chain left standing would make the next operator message start at depth 2
    /// and refuse its first hand-off. An ordinary `clear()` must NOT reset it —
    /// clearing happens between delegations inside a live chain.
    #[test]
    fn the_scope_chain_resets_with_the_claim_and_survives_a_clear() {
        let queue = DelegationQueue::default();
        {
            let _claim = queue.claim();
            std::mem::forget(queue.enter_scope("strategy".to_string()));
            std::mem::forget(queue.enter_scope("research".to_string()));
            assert_eq!(queue.scope_chain(), ["strategy", "research"]);
            queue.clear();
            assert_eq!(
                queue.scope_chain(),
                ["strategy", "research"],
                "clear() runs between delegations inside a live chain and must not reset depth"
            );
        }
        assert_eq!(
            queue.scope_depth(),
            0,
            "the claim's Drop must reset a chain leaked past its guards"
        );
        // …and the acquire resets too, for a claim taken after a leak.
        std::mem::forget(queue.enter_scope("strategy".to_string()));
        let _claim = queue.claim();
        assert_eq!(queue.scope_depth(), 0);
    }

    /// A member may not hand work back up its own chain (A→B→A), and the
    /// refusal is recorded for the card as well as returned to the model.
    #[tokio::test]
    async fn a_member_may_not_hand_work_back_to_a_desk_on_the_chain() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        // The chain the orchestrator's hand-off to `strategy` opened, with the
        // writer's own turn running inside it.
        let _scope = queue.enter_scope("strategy".to_string());
        // `writer` leads `strategy`, so it is BOTH on the chain and self-led;
        // give it a wildcard allowlist so the allowlist check cannot be what
        // refuses.
        let tool = DelegateToDeskTool::for_member(
            queue.clone(),
            company.clone(),
            Arc::new(MemStore::seeded(nested_desks_record(&company))) as Arc<dyn CompanyStore>,
            MemberScope {
                member: "writer".to_string(),
                delegates_to: vec!["*".to_string()],
            },
        );
        let result = tool
            .execute(json!({ "desk": "strategy", "instruction": "start over" }))
            .await
            .expect("execute");
        assert!(result.is_error, "a cycle must be refused");
        let text = result.output_for_llm(true);
        assert!(text.contains("strategy"), "{text}");
        assert_eq!(queue.queued(), 0, "nothing may be staged for a cycle");
        assert_eq!(
            queue.drain_refusals(MAX_DELEGATIONS_PER_TURN),
            vec!["strategy".to_string()],
            "the drain must be able to record the attempt on the card"
        );

        // A desk that is neither on the chain nor led by the caller goes
        // through.
        let ok = tool
            .execute(json!({ "desk": "research", "instruction": "dig into it" }))
            .await
            .expect("execute");
        assert!(!ok.is_error, "{}", ok.output_for_llm(true));
    }

    /// A member may only reach the desks its manifest entry names, and the
    /// refusal lists them — the model has no other way to learn its allowlist.
    #[tokio::test]
    async fn a_member_may_only_reach_the_desks_its_manifest_allows() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let tool = member_desk_tool(nested_desks_record(&company), &queue);

        let refused = tool
            .execute(json!({ "desk": "legal", "instruction": "review it" }))
            .await
            .expect("execute");
        assert!(refused.is_error, "an off-allowlist desk must be refused");
        let text = refused.output_for_llm(true);
        assert!(text.contains("legal"), "{text}");
        assert!(
            text.contains("research"),
            "the permitted set must be named so the model can retry in-turn: {text}"
        );
        assert_eq!(queue.queued(), 0);

        let allowed = tool
            .execute(json!({ "desk": "research", "instruction": "dig into it" }))
            .await
            .expect("execute");
        assert!(!allowed.is_error, "{}", allowed.output_for_llm(true));
        assert_eq!(queue.queued(), 1);
    }

    /// The **orchestrator's** copy is unrestricted: no allowlist, no cycle
    /// guard, and it reaches every desk exactly as it did before #176.
    #[tokio::test]
    async fn the_orchestrators_copy_is_unrestricted() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        // Even from inside a chain — which the orchestrator never is, but the
        // contrast is the point.
        let _scope = queue.enter_scope("legal".to_string());
        let tool = desk_tool(nested_desks_record(&company), &queue);
        for desk in ["strategy", "research", "legal"] {
            let result = tool
                .execute(json!({ "desk": desk, "instruction": "go" }))
                .await
                .expect("execute");
            assert!(
                !result.is_error,
                "the orchestrator must reach {desk}: {}",
                result.output_for_llm(true)
            );
            queue.clear();
        }
    }

    /// A store that cannot answer, so the grounding read has nothing to check
    /// the target against.
    struct BrokenStore;

    #[async_trait::async_trait]
    impl CompanyStore for BrokenStore {
        async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Err(crate::OpenCompanyError::Store("store is down".to_string()))
        }
        async fn save(&self, _record: &CompanyRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> crate::Result<()> {
            Ok(())
        }
    }

    /// A member's hand-off fails **closed** when the record cannot be read, and
    /// the orchestrator's still fails open.
    ///
    /// The asymmetry is the whole point. The allowlist and the cycle guard are
    /// enforced at this tool boundary and nowhere else — `run_delegation`
    /// executes whatever the queue holds without re-deriving either — so a
    /// member queued ungrounded reaches every desk in the company for as long
    /// as the store is unhappy. The orchestrator has no allowlist to lose, so
    /// an unreadable record leaves it exactly where #272 left it.
    #[tokio::test]
    async fn a_members_hand_off_is_refused_when_the_record_cannot_be_read() {
        let company = CompanyId::new("acme");
        let scope = || MemberScope {
            member: "writer".to_string(),
            delegates_to: vec!["research".to_string()],
        };

        // Ok(None) — no record under that id.
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let missing = DelegateToDeskTool::for_member(
            queue.clone(),
            company.clone(),
            Arc::new(MemStore::default()) as Arc<dyn CompanyStore>,
            scope(),
        );
        let refused = missing
            .execute(json!({ "desk": "research", "instruction": "dig into it" }))
            .await
            .expect("execute");
        assert!(
            refused.is_error,
            "a member may not be queued against a record nobody could read: {}",
            refused.output_for_llm(true)
        );
        let text = refused.output_for_llm(true);
        assert!(text.contains("research"), "{text}");
        assert!(
            text.contains("writer"),
            "the refusal must name whose allowlist went unchecked: {text}"
        );
        assert_eq!(queue.queued(), 0, "nothing may be staged ungrounded");
        assert_eq!(
            queue.drain_refusals(MAX_DELEGATIONS_PER_TURN),
            vec!["research".to_string()],
            "the drain must be able to record the attempt on the card"
        );

        // Err(..) — the store is there and unhappy. Same answer.
        let broken = DelegateToDeskTool::for_member(
            queue.clone(),
            company.clone(),
            Arc::new(BrokenStore) as Arc<dyn CompanyStore>,
            scope(),
        );
        let refused = broken
            .execute(json!({ "desk": "research", "instruction": "dig into it" }))
            .await
            .expect("execute");
        assert!(
            refused.is_error,
            "a store error must refuse too: {}",
            refused.output_for_llm(true)
        );
        assert_eq!(queue.queued(), 0);
        queue.clear();
        let _ = queue.drain_refusals(MAX_DELEGATIONS_PER_TURN);

        // …and the orchestrator's copy over the same broken store still queues.
        let orchestrator = DelegateToDeskTool::new(
            queue.clone(),
            company,
            Arc::new(BrokenStore) as Arc<dyn CompanyStore>,
        );
        let queued = orchestrator
            .execute(json!({ "desk": "research", "instruction": "dig into it" }))
            .await
            .expect("execute");
        assert!(
            !queued.is_error,
            "a store hiccup must not take the orchestrator's delegation offline: {}",
            queued.output_for_llm(true)
        );
        assert_eq!(queue.queued(), 1);
    }

    /// The depth bound comes off the **live company record**, not a build-time
    /// snapshot — an operator can edit `[tools].max_delegation_depth` without
    /// the cached belt being rebuilt.
    #[tokio::test]
    async fn the_depth_bound_is_read_from_the_manifest_at_call_time() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let mut record = nested_desks_record(&company);
        record.manifest.tools.max_delegation_depth = Some(1);
        let tool = member_desk_tool(record, &queue);
        // One level in, under the manifest's bound of 1.
        let _scope = queue.enter_scope("strategy".to_string());
        let result = tool
            .execute(json!({ "desk": "research", "instruction": "dig into it" }))
            .await
            .expect("execute");
        assert!(result.is_error, "depth 1 must stop a member re-delegating");
        assert!(
            result
                .output_for_llm(true)
                .contains("as far as this company allows"),
            "{}",
            result.output_for_llm(true)
        );
        assert_eq!(queue.queued(), 0);
    }

    /// The member's belt is exactly `spawn_task` + the two hand-off tools —
    /// never the orchestrator's authority tools.
    #[test]
    fn a_members_delegation_belt_is_the_two_hand_off_tools() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let store: Arc<dyn CompanyStore> =
            Arc::new(MemStore::seeded(nested_desks_record(&company)));
        let tools = member_delegation_tools(
            &queue,
            company,
            store,
            MemberScope {
                member: "writer".to_string(),
                delegates_to: vec!["research".to_string()],
            },
        );
        let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                DELEGATE_TO_DESK_TOOL,
                DELEGATE_TO_TEAMMATE_TOOL,
                SPAWN_TASK_TOOL
            ]
        );
    }

    // ── delegate_to_teammate at the tool boundary (issue #884) ──────────────

    /// A company whose `strategy` desk has THREE members, so its lead has peers
    /// to reach — the shape D1 was observed on — plus an `analyst` on a desk the
    /// lead's `delegates_to` permits and a `legal_counsel` on one it does not.
    fn peers_record(id: &CompanyId) -> CompanyRecord {
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
delegates_to = ["research"]

[[agent]]
id = "editor"
role = "Editor"

[[agent]]
id = "analyst"
role = "Analyst"

[[agent]]
id = "legal_counsel"
role = "Counsel"

[[group_chat]]
id = "strategy"
name = "Strategy desk"
members = ["writer", "editor"]

[[group_chat]]
id = "research"
name = "Research desk"
members = ["analyst"]

[[group_chat]]
id = "legal"
name = "Legal desk"
members = ["legal_counsel"]
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            manifest,
            ..seeded_record(id)
        }
    }

    /// `writer`'s copy of the teammate tool: a desk lead with one peer on its
    /// own desk and a `research` allowlist.
    fn member_teammate_tool(
        record: CompanyRecord,
        queue: &DelegationQueue,
    ) -> DelegateToTeammateTool {
        let company = record.id.clone();
        DelegateToTeammateTool::for_member(
            queue.clone(),
            company,
            Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>,
            MemberScope {
                member: "writer".to_string(),
                delegates_to: vec!["research".to_string()],
            },
        )
    }

    /// D1 at the boundary: the lead's hand-off to the peer beside it is
    /// **accepted**, and queues the delegation the drain runs that teammate's
    /// turn from.
    #[tokio::test]
    async fn a_lead_may_hand_work_to_a_peer_on_its_own_desk() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let tool = member_teammate_tool(peers_record(&company), &queue);
        let result = tool
            .execute(json!({ "teammate": "editor", "instruction": "tighten the copy" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{}", result.output_for_llm(true));
        assert_eq!(
            queue.drain(MAX_DELEGATIONS_PER_TURN),
            vec![Delegation::DelegateToTeammate {
                teammate: "editor".to_string(),
                instruction: "tighten the copy".to_string(),
            }]
        );
    }

    /// A key that is nobody is refused before anything is queued, and the
    /// attempt is recorded for the drain to report on the card — the same
    /// independence #272 gave the desk refusals.
    #[tokio::test]
    async fn a_teammate_that_is_not_on_the_roster_is_refused_and_recorded() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let tool = member_teammate_tool(peers_record(&company), &queue);
        let result = tool
            .execute(json!({ "teammate": "ghost", "instruction": "do it" }))
            .await
            .expect("execute");
        assert!(result.is_error, "{}", result.output_for_llm(true));
        assert!(
            result.output_for_llm(true).contains("editor"),
            "the refusal must name who CAN be reached: {}",
            result.output_for_llm(true)
        );
        assert_eq!(queue.queued(), 0);
        assert_eq!(
            queue.drain_refusals(MAX_DELEGATIONS_PER_TURN),
            vec!["ghost".to_string()]
        );
    }

    /// A real teammate on neither the caller's desk nor an allowlisted one is
    /// refused; one on an allowlisted desk is not. The allowlist is #176's, read
    /// at teammate granularity rather than duplicated.
    #[tokio::test]
    async fn the_allowlist_bounds_which_teammates_a_member_may_reach() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let tool = member_teammate_tool(peers_record(&company), &queue);

        let refused = tool
            .execute(json!({ "teammate": "legal_counsel", "instruction": "review it" }))
            .await
            .expect("execute");
        assert!(refused.is_error, "{}", refused.output_for_llm(true));
        assert_eq!(queue.queued(), 0);

        // `analyst` sits on `research`, which `writer`'s `delegates_to` names.
        let allowed = tool
            .execute(json!({ "teammate": "analyst", "instruction": "pull the numbers" }))
            .await
            .expect("execute");
        assert!(!allowed.is_error, "{}", allowed.output_for_llm(true));
        assert_eq!(queue.queued(), 1);
    }

    /// A hand-off back to somebody already on the chain is refused as a cycle —
    /// the A→B→A guard, at the boundary, in the model's own turn.
    #[tokio::test]
    async fn a_hand_off_back_up_the_teammate_chain_is_refused() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let record = peers_record(&company);
        let tool = DelegateToTeammateTool::for_member(
            queue.clone(),
            company,
            Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>,
            MemberScope {
                member: "editor".to_string(),
                delegates_to: Vec::new(),
            },
        );
        // `editor` is running inside a hand-off `writer` made.
        let _scope = queue.enter_scope(crate::runtime::delegation_tools::teammate_scope_key(
            "writer",
        ));
        let result = tool
            .execute(json!({ "teammate": "writer", "instruction": "you take it back" }))
            .await
            .expect("execute");
        assert!(result.is_error, "{}", result.output_for_llm(true));
        assert!(
            result.output_for_llm(true).contains("loop"),
            "{}",
            result.output_for_llm(true)
        );
        assert_eq!(queue.queued(), 0);
    }

    /// The depth bound applies to the teammate hand-off exactly as it does to
    /// the desk one — the guard a ring of three the cycle check cannot see still
    /// runs into.
    #[tokio::test]
    async fn the_depth_bound_stops_a_teammate_hand_off_too() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let mut record = peers_record(&company);
        record.manifest.tools.max_delegation_depth = Some(1);
        let tool = member_teammate_tool(record, &queue);
        let _scope = queue.enter_scope("strategy".to_string());
        let result = tool
            .execute(json!({ "teammate": "editor", "instruction": "tighten the copy" }))
            .await
            .expect("execute");
        assert!(result.is_error, "depth 1 must stop a further hand-off");
        assert!(
            result
                .output_for_llm(true)
                .contains("as far as this company allows"),
            "{}",
            result.output_for_llm(true)
        );
        assert_eq!(queue.queued(), 0);
    }

    /// The orchestrator's copy is unrestricted: it reaches a teammate that is
    /// not a desk lead, with no allowlist in the way. Grounding still applies.
    #[tokio::test]
    async fn the_orchestrators_teammate_tool_is_unrestricted_but_grounded() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let record = peers_record(&company);
        let store = Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>;
        let tool = DelegateToTeammateTool::new(queue.clone(), company, store);

        let ok = tool
            .execute(json!({ "teammate": "editor", "instruction": "tighten the copy" }))
            .await
            .expect("execute");
        assert!(!ok.is_error, "{}", ok.output_for_llm(true));

        let refused = tool
            .execute(json!({ "teammate": "ghost", "instruction": "do it" }))
            .await
            .expect("execute");
        assert!(refused.is_error, "{}", refused.output_for_llm(true));
    }

    /// Issue #1162, the other half of the fix: a hand-off written with a
    /// teammate's **display name** is accepted, and what reaches the queue is
    /// the **canonical id**.
    ///
    /// Queueing the key as typed is what would make a name-accepting refusal
    /// worse than the refusal it replaced — the tool would answer "Handed to
    /// …" and the drain, which resolves independently, would find nothing to
    /// deliver to. The reply names both strings so the model learns the id.
    #[tokio::test]
    async fn a_teammate_named_by_display_name_is_queued_under_its_id() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let mut record = peers_record(&company);
        record.overlay_agents.push(OverlayAgent {
            id: "dana_designer".to_string(),
            name: "Dana Designer".to_string(),
            role: "Designer".to_string(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        });
        let store = Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>;
        let tool = DelegateToTeammateTool::new(queue.clone(), company, store);

        let result = tool
            .execute(json!({ "teammate": "Dana Designer", "instruction": "draw it" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{}", result.output_for_llm(true));
        let reply = result.output_for_llm(true);
        assert!(
            reply.contains("Dana Designer") && reply.contains("dana_designer"),
            "the reply must name the person and teach the id: {reply}"
        );
        assert_eq!(
            queue.drain(MAX_DELEGATIONS_PER_TURN),
            vec![Delegation::DelegateToTeammate {
                teammate: "dana_designer".to_string(),
                instruction: "draw it".to_string(),
            }],
            "the queue must carry the canonical id, not the key as typed"
        );
    }

    /// A display name two teammates answer to is refused rather than routed to
    /// whichever was added first, and the refusal carries the ids to retry
    /// with — the collision is the operator's to resolve, and the model cannot
    /// do it without being told the alternatives (issue #1162).
    #[tokio::test]
    async fn a_display_name_two_teammates_share_is_refused_with_their_ids() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let mut record = peers_record(&company);
        for id in ["dana_designer", "dana_designer_2"] {
            record.overlay_agents.push(OverlayAgent {
                id: id.to_string(),
                name: "Dana Designer".to_string(),
                role: "Designer".to_string(),
                description: None,
                tools: None,
                model: None,
                harness: None,
            });
        }
        let store = Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>;
        let tool = DelegateToTeammateTool::new(queue.clone(), company, store);

        let result = tool
            .execute(json!({ "teammate": "Dana Designer", "instruction": "draw it" }))
            .await
            .expect("execute");
        assert!(result.is_error, "{}", result.output_for_llm(true));
        let refusal = result.output_for_llm(true);
        assert!(
            refusal.contains("dana_designer") && refusal.contains("dana_designer_2"),
            "the refusal must name both ids: {refusal}"
        );
        assert_eq!(queue.queued(), 0);
    }

    /// Both arguments are required, and neither may be blank — a hand-off with
    /// no instruction is a turn run on nothing.
    #[tokio::test]
    async fn the_teammate_tool_requires_both_arguments() {
        let company = CompanyId::new("acme");
        let queue = DelegationQueue::default();
        let _claim = queue.claim();
        let tool = member_teammate_tool(peers_record(&company), &queue);
        assert!(tool.execute(json!({ "teammate": "editor" })).await.is_err());
        assert!(
            tool.execute(json!({ "instruction": "do it" }))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({ "teammate": "  ", "instruction": "do it" }))
                .await
                .is_err()
        );
        assert_eq!(queue.queued(), 0);
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
            fn subscribe(
                &self,
                _id: &CompanyId,
            ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
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
                origin_chat_id: None,
                origin_parent: None,
            },
            at_millis: 30,
        });

        let log: Arc<dyn EventLog> = Arc::new(FixedLog(history));
        let tool = QueryCompanyTool::new(company, None, Some(log), None, None, None);
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
            fn subscribe(
                &self,
                _id: &CompanyId,
            ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
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
        let tool = QueryCompanyTool::new(company.clone(), None, Some(log), None, None, None);
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
        let result = QueryCompanyTool::new(company.clone(), None, Some(log), None, None, None)
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
        let result = QueryCompanyTool::new(company.clone(), None, Some(log), None, None, None)
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
        let out =
            QueryCompanyTool::new(CompanyId::new("acme"), Some(exact), None, None, None, None)
                .execute(json!({}))
                .await
                .expect("execute")
                .output_for_llm(true);
        assert!(!out.contains("TRUNCATED"), "nothing was cut: {out}");

        // Past the cap: the cut is announced, counted, and points at `query`.
        let many: Arc<dyn FactStore> = Arc::new(ManyFacts(FACT_LIMIT + 7));
        let out = QueryCompanyTool::new(CompanyId::new("acme"), Some(many), None, None, None, None)
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
            QueryCompanyTool::new(CompanyId::new("acme"), Some(store), None, None, None, None)
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
        let tool = QueryCompanyTool::new(CompanyId::new("acme"), None, None, None, None, None);
        let result = tool.execute(json!({})).await.expect("execute");
        // The insight surface lives in the markdown; `output()` is the summary.
        let out = result.output_for_llm(true);
        assert!(out.contains("No durable facts recorded"), "{out}");
        assert!(out.contains("No recent activity"), "{out}");
        // Not "no saved workflows" any more: the global baseline ships graphs
        // every company has, wired store or not.
        for workflow in crate::globals::workflows() {
            assert!(out.contains(&workflow.id), "{out}");
        }
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
            tools: None,
            model: None,
            harness: None,
        });
        let store: Arc<dyn CompanyStore> = Arc::new(MemStore::seeded(record));

        let tool = QueryCompanyTool::new(
            CompanyId::new("acme"),
            None,
            None,
            Some(dir.path().to_path_buf()),
            Some(store),
            None,
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

    /// Issue #1162: the Team column is the one the orchestrator is told to take
    /// a hand-off target from, so every row must lead with a token the
    /// delegation tools can ground. An overlay teammate was listed under its
    /// **display name** while a manifest agent was listed under its **id** —
    /// two namespaces rendered identically, and `mint_agent_id` guarantees the
    /// name is not the id.
    ///
    /// The two halves are pinned together deliberately: the assertion is not
    /// "the line contains `dana_designer`" but "the token the line prints
    /// resolves", so a render that drifts from the resolver fails here rather
    /// than in production.
    #[tokio::test]
    async fn query_company_lists_a_teammate_under_the_id_delegation_grounds() {
        let mut record = seeded_record(&CompanyId::new("acme"));
        let id = record.mint_agent_id("Dana Designer");
        assert_eq!(id, "dana_designer", "the id and the name must differ");
        record.overlay_agents.push(OverlayAgent {
            id: id.clone(),
            name: "Dana Designer".to_string(),
            role: "Designer".to_string(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        });
        let store: Arc<dyn CompanyStore> = Arc::new(MemStore::seeded(record.clone()));

        let out =
            QueryCompanyTool::new(CompanyId::new("acme"), None, None, None, Some(store), None)
                .execute(json!({}))
                .await
                .expect("execute")
                .output_for_llm(true);

        let line = out
            .lines()
            .find(|line| line.contains("Designer"))
            .unwrap_or_else(|| panic!("no teammate line: {out}"));
        assert!(
            line.contains(&id),
            "the row must lead with the groundable id: {line}"
        );
        assert!(
            line.contains("known as Dana Designer"),
            "the display name must survive as a label: {line}"
        );
        // The token the roster prints is the token delegation accepts.
        let printed = line
            .split("**")
            .nth(1)
            .unwrap_or_else(|| panic!("no bold token: {line}"));
        assert_eq!(
            record.resolve_teammate_key(printed),
            crate::ports::types::TeammateResolution::Agent(id),
            "the roster printed a token delegation cannot ground: {line}"
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
        let tool = QueryCompanyTool::new(company, None, None, None, Some(store), None);
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
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
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
        // No `tools` given → inherit the standard company-wide grant, which for
        // an unscoped minter is `None` (keeps tracking `[tools].allow`), NOT an
        // explicit empty list (which since #1804 is a deny-all).
        assert!(
            added.tools.is_none(),
            "an add with no `tools` inherits the standard grant (None), not an empty deny-all shelf"
        );
    }

    /// A minter scoped to part of the company grant, for the #619 tests below.
    /// `minter_tools` is the line it declares; `minter_grants` is that line
    /// already narrowed by the company `allow` — what `build_agent` hands the
    /// tool.
    fn scoped_add_agent(company: CompanyId, store: Arc<dyn CompanyStore>) -> AddAgentTool {
        AddAgentTool::new(
            company,
            store,
            "ceo".to_string(),
            Some(vec!["workspace".to_string()]),
            vec!["workspace".to_string()],
        )
    }

    /// Issue #619: a teammate minted by a **scoped** agent inherits that
    /// agent's line, not the company's whole grant.
    ///
    /// #661 clamped an explicit `tools` argument to the company grant, which
    /// leaves this open: omitting `tools` still yields the company's *entire*
    /// grant, so a narrowly scoped agent could mint a teammate holding
    /// everything the company holds. `add_agent` is `Reach::Nothing` and never
    /// asks, so nothing else in the path would catch it.
    #[tokio::test]
    async fn a_minted_teammate_is_bounded_by_its_minter_not_the_company() {
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
            Some(vec!["workspace".to_string()]),
            "the minted teammate must be bounded by the agent that minted it, \
             not by the company"
        );
    }

    /// An **unscoped** minter still mints an unscoped teammate — the pre-#619
    /// behaviour, kept deliberately. Copying the minter's *line* rather than
    /// its resolved grant is what keeps the teammate tracking `[tools].allow`
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
            record.overlay_agents[0].tools.is_none(),
            "an absent line (None) means the company's standard grant (#264/#1804), \
             and an unscoped minter hands on exactly it — None, not an empty deny-all"
        );
    }

    /// An explicit `tools` request is narrowed against what the **minter**
    /// holds, so the tool cannot hand out a grant its caller does not have.
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
            Some(vec!["workspace".to_string()]),
            "`composio` is outside the minter's own grant and must be dropped"
        );
    }

    /// A request that narrows to **nothing** is a refusal, not a stored empty
    /// list.
    ///
    /// This is the sharp edge: an empty `tools` list means "inherit the
    /// company's standard grant". Storing the empty result of a narrowing
    /// would turn the most deliberate narrowing an agent can ask for into the
    /// widest grant in the company — the exact inversion #619 exists to remove.
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

    /// Issue #661 / L5: `add_agent` carries a per-teammate tool grant onto the
    /// overlay record, trimming and dropping blank globs. The grant is narrowed
    /// against `[tools].allow` later (at roster build); persistence keeps the
    /// authored list verbatim so the Team tab and the roster read the same thing.
    #[tokio::test]
    async fn add_agent_tool_persists_a_tool_grant() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = unscoped_add_agent(company.clone(), store.clone());

        let result = tool
            .execute(json!({
                "name": "Ravi",
                "role": "Researcher",
                "tools": ["docs.*", "   ", "email"]
            }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{}", result.text());

        let record = store.load(&company).await.unwrap().expect("record");
        assert_eq!(
            record.overlay_agents[0].tools,
            Some(vec!["docs.*".to_string(), "email".to_string()]),
            "blanks are dropped and globs trimmed"
        );
    }

    /// Since issue #1804 an explicit empty `tools` array is a deliberate
    /// **deny-all**, NOT the standard grant — the contract inversion. Omitting
    /// the field entirely is what inherits the standard grant (`None`); passing
    /// `[]` deliberately hands the teammate no tools, stored as `Some(vec![])`.
    #[tokio::test]
    async fn add_agent_tool_empty_tools_is_an_explicit_deny_all() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = unscoped_add_agent(company.clone(), store.clone());

        let result = tool
            .execute(json!({ "name": "Ravi", "role": "Researcher", "tools": [] }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{}", result.text());
        assert!(
            result.text().contains("hold no tools"),
            "the mint result must state the deny-all plainly: {}",
            result.text()
        );

        let record = store.load(&company).await.unwrap().expect("record");
        assert_eq!(
            record.overlay_agents[0].tools,
            Some(Vec::new()),
            "an explicit empty array is a deny-all (Some(vec![])), not the standard grant (None)"
        );
    }

    /// A minter whose own line names `chargebee` (the shipped bookkeeper) hands
    /// that line on when `tools` is omitted — but an unstated grant never
    /// confers billing (#788/#789), so the copied line is filtered before it is
    /// stored. The #619 copy-the-line rule still holds for the non-BYO parts.
    #[tokio::test]
    async fn an_unstated_mint_from_a_billing_holding_minter_withholds_chargebee() {
        let company = CompanyId::new("acme");
        let mut record = seeded_record(&company);
        record.manifest = toml::from_str(
            "[company]\nname = \"Acme\"\n\
             [tools]\n\
             allow = [\"*\", \"workspace.*\", \"workspace.write\", \"media\", \"composio\", \
             \"search\", \"mcp:*\", \"chargebee\"]\n",
        )
        .expect("valid manifest");
        let store = Arc::new(MemStore::seeded(record));
        let belt = vec![
            "*".to_string(),
            "workspace.*".to_string(),
            "workspace.write".to_string(),
            "media".to_string(),
            "composio".to_string(),
            "search".to_string(),
            "mcp:*".to_string(),
            "chargebee".to_string(),
        ];
        let tool = AddAgentTool::new(
            company.clone(),
            store.clone(),
            "bookkeeper".to_string(),
            Some(belt.clone()),
            belt,
        );

        let result = tool
            .execute(json!({ "name": "Jamie", "role": "Data Entry" }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{}", result.text());

        let record = store.load(&company).await.unwrap().expect("persisted");
        let added = &record.overlay_agents[0];
        assert!(
            !added
                .tools
                .iter()
                .flatten()
                .any(|g| g == "chargebee" || g.starts_with("chargebee.")),
            "an unstated mint must not hand on billing: {:?}",
            added.tools
        );
        assert!(
            added.tools.iter().flatten().any(|g| g == "*"),
            "the rest of the minter's line is still copied verbatim (#619): {:?}",
            added.tools
        );
    }

    /// An EXPLICIT `tools` request naming `chargebee` survives — an unstated
    /// grant is withheld, a stated one is narrowed to what the minter holds.
    #[tokio::test]
    async fn an_explicit_chargebee_request_from_a_billing_minter_is_honored() {
        let company = CompanyId::new("acme");
        let mut record = seeded_record(&company);
        record.manifest = toml::from_str(
            "[company]\nname = \"Acme\"\n\
             [tools]\n\
             allow = [\"*\", \"workspace.*\", \"workspace.write\", \"media\", \"composio\", \
             \"search\", \"mcp:*\", \"chargebee\"]\n",
        )
        .expect("valid manifest");
        let store = Arc::new(MemStore::seeded(record));
        let belt = vec![
            "*".to_string(),
            "workspace.*".to_string(),
            "workspace.write".to_string(),
            "media".to_string(),
            "composio".to_string(),
            "search".to_string(),
            "mcp:*".to_string(),
            "chargebee".to_string(),
        ];
        let tool = AddAgentTool::new(
            company.clone(),
            store.clone(),
            "bookkeeper".to_string(),
            Some(belt.clone()),
            belt,
        );

        let result = tool
            .execute(json!({ "name": "Jamie", "role": "Data Entry", "tools": ["chargebee"] }))
            .await
            .expect("execute");
        assert!(!result.is_error, "{}", result.text());

        let record = store.load(&company).await.unwrap().expect("persisted");
        assert_eq!(
            record.overlay_agents[0].tools,
            Some(vec!["chargebee".to_string()]),
            "a stated billing namespace is narrowed to the minter's grant, not dropped"
        );
    }

    /// A non-string `tools` item is a clean argument error, the same shape as a
    /// missing `name`/`role` — a malformed grant must not persist a half-parsed
    /// teammate.
    #[tokio::test]
    async fn add_agent_tool_rejects_a_non_string_tool() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = unscoped_add_agent(company.clone(), store.clone());

        assert!(
            tool.execute(json!({ "name": "Ravi", "role": "Researcher", "tools": [123] }))
                .await
                .is_err(),
            "a non-string tool glob must be rejected"
        );
        // Also rejects a non-array `tools`.
        assert!(
            tool.execute(json!({ "name": "Ravi", "role": "Researcher", "tools": "docs.*" }))
                .await
                .is_err(),
            "a non-array `tools` must be rejected"
        );

        let record = store.load(&company).await.unwrap().expect("record");
        assert!(
            record.overlay_agents.is_empty(),
            "a rejected add must not persist a teammate"
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
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: Vec::new(),
                approvals: Vec::new(),
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

    /// A [`WorkflowRunner`] test double whose `run` always returns `Err` — the
    /// engine-failed shape issue #1865's review comment 3877185396 flagged as
    /// silent: `RunWorkflowTool`'s `Ok(Err(err))` arm journaled a finish but
    /// filed no `workflow_run_failed` notification, unlike the console run
    /// route, the cron scheduler, and the approval-resume path, which all
    /// file one through `WorkflowSpawn`.
    struct FailingRunner;

    #[async_trait::async_trait]
    impl WorkflowRunner for FailingRunner {
        async fn run(
            &self,
            _company: &CompanyId,
            _workflow: &WorkflowFile,
            _input: Value,
            _ctx: &crate::ports::WorkflowRunContext,
        ) -> crate::Result<WorkflowRun> {
            Err(crate::error::OpenCompanyError::Harness(
                "the engine blew up".to_string(),
            ))
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
    fn orchestrator_tools_includes_all_sixteen() {
        use crate::harness::workflow_admin::{
            DELETE_WORKFLOW_TOOL, READ_WORKFLOW_TOOL, UPDATE_WORKFLOW_TOOL,
        };
        let queue = DelegationQueue::default();
        let tools = orchestrator_tools(
            CompanyId::new("acme"),
            None,
            None,
            None,
            None,
            None,
            &queue,
            None,
            WorkflowRunnerHandle::default(),
            crate::runtime::RunSupervisor::default(),
            Arc::new(MemStore::default()),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
            "ceo".to_string(),
            None,
            vec!["fs:*".to_string()],
            None,
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        // Six before #186; `assign_task` + `review_task` made eight; #418's
        // `read_run_output` makes nine; #661's read/update/delete_workflow
        // trio makes twelve; #884's `delegate_to_teammate` makes thirteen;
        // #1859's `list_tasks` / `read_task` / `read_run` trio makes sixteen.
        assert_eq!(names.len(), 16, "got {names:?}");
        assert!(names.contains(&DELEGATE_TO_TEAMMATE_TOOL), "got {names:?}");
        assert!(names.contains(&RUN_WORKFLOW_TOOL), "got {names:?}");
        assert!(names.contains(&READ_RUN_OUTPUT_TOOL), "got {names:?}");
        assert!(names.contains(&CREATE_WORKFLOW_TOOL), "got {names:?}");
        assert!(names.contains(&READ_WORKFLOW_TOOL), "got {names:?}");
        assert!(names.contains(&UPDATE_WORKFLOW_TOOL), "got {names:?}");
        assert!(names.contains(&DELETE_WORKFLOW_TOOL), "got {names:?}");
        assert!(names.contains(&ADD_AGENT_TOOL), "got {names:?}");
        assert!(names.contains(&QUERY_COMPANY_TOOL), "got {names:?}");
        assert!(names.contains(&SPAWN_TASK_TOOL), "got {names:?}");
        assert!(names.contains(&DELEGATE_TO_DESK_TOOL), "got {names:?}");
        assert!(names.contains(&ASSIGN_TASK_TOOL), "got {names:?}");
        assert!(names.contains(&REVIEW_TASK_TOOL), "got {names:?}");
        assert!(names.contains(&LIST_TASKS_TOOL), "got {names:?}");
        assert!(names.contains(&READ_TASK_TOOL), "got {names:?}");
        assert!(names.contains(&READ_RUN_TOOL), "got {names:?}");
        // `read_run_output` sits immediately after `run_workflow`.
        let run_at = names.iter().position(|n| *n == RUN_WORKFLOW_TOOL).unwrap();
        assert_eq!(names[run_at + 1], READ_RUN_OUTPUT_TOOL, "got {names:?}");
        // The #661 trio sits immediately after `create_workflow`: they are its
        // lifecycle, and a model reads the belt in order.
        let created_at = names
            .iter()
            .position(|n| *n == CREATE_WORKFLOW_TOOL)
            .unwrap();
        assert_eq!(
            &names[created_at + 1..created_at + 4],
            &[
                READ_WORKFLOW_TOOL,
                UPDATE_WORKFLOW_TOOL,
                DELETE_WORKFLOW_TOOL
            ],
            "got {names:?}"
        );
        // #1859's read trio sits immediately after `query_company`: all four
        // answer "what does the company know?" rather than acting on it.
        let query_at = names.iter().position(|n| *n == QUERY_COMPANY_TOOL).unwrap();
        assert_eq!(
            &names[query_at + 1..query_at + 4],
            &[LIST_TASKS_TOOL, READ_TASK_TOOL, READ_RUN_TOOL],
            "got {names:?}"
        );
    }

    /// A runner panic is converted into an agent-visible error, and the RAII
    /// supervisor slot is gone when the tool returns. This covers both cleanup
    /// obligations without changing the runner architecture.
    #[tokio::test]
    async fn panicking_run_cleans_up_its_active_attempt() {
        struct PanickingRunner;

        #[async_trait::async_trait]
        impl WorkflowRunner for PanickingRunner {
            async fn run(
                &self,
                _company: &CompanyId,
                _workflow: &WorkflowFile,
                _input: Value,
                _ctx: &crate::ports::WorkflowRunContext,
            ) -> crate::Result<WorkflowRun> {
                panic!("test runner panic")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());
        let runner: Arc<dyn WorkflowRunner> = Arc::new(PanickingRunner);
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let supervisor = crate::runtime::RunSupervisor::default();
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            supervisor.clone(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
            None,
        );

        let result = tool
            .execute(json!({"id": "demo"}))
            .await
            .expect("panic is converted to a tool result");
        assert!(result.is_error, "panic must be agent-visible: {result:?}");
        assert!(
            result.output_for_llm(false).contains("internal error"),
            "the result should not leak panic payload: {result:?}"
        );
        assert_eq!(supervisor.len(), 0, "the active attempt must be cleaned up");
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
            None,
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
            None,
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
            None,
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
            None,
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
            None,
        );
        let result = tool
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute");
        assert!(result.is_error, "a cancelled run reports as a stop");
        assert_eq!(refs.queued(), 0);
    }

    /// Issue #1861: an agent-initiated run that ends blocked badges the
    /// operator, exactly as the console's and the scheduler's runs do.
    ///
    /// This is the one trigger nobody is watching a progress bar for, so the
    /// badge is the only way a run that stopped waiting on a person becomes
    /// visible without somebody thinking to open the run history.
    #[tokio::test]
    async fn a_blocked_agent_run_badges_the_operator() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": {} }),
            pending_approvals: vec!["worker".to_string()],
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: vec![crate::ports::workflow_runner::WorkflowBlockedNode {
                node_id: "worker".to_string(),
                tools: vec!["send_email".to_string()],
                approval_ids: vec!["ap-1".to_string()],
                unparkable: 0,
                stranded: 0,
                blockers: 0,
            }],
            approvals: Vec::new(),
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);

        let notifications: Arc<dyn crate::ports::notifications::NotificationStore> =
            Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
            Some(notifications.clone()),
        );
        tool.execute(json!({ "id": "demo" }))
            .await
            .expect("execute");

        let feed = notifications
            .list(&CompanyId::new("acme"), "ceo")
            .await
            .expect("list");
        assert_eq!(feed.len(), 1, "one badge for one unhealthy run: {feed:?}");
        assert_eq!(feed[0].notification.kind, "workflow_run_blocked");
    }

    /// The same contract for the other unhealthy end: the run could not park
    /// the approval at all, so nobody was asked and nothing is waiting.
    #[tokio::test]
    async fn a_stranded_agent_run_badges_the_operator() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": {} }),
            // Stranded is counted per pending *node* (`stranded_approvals`),
            // not per gated call: the node is pending, and every approval row
            // it owns failed to park, so there is nothing an operator can be
            // asked about. A fixture with no pending node at all is not a
            // stranded run under that reconciliation — it is an empty one.
            pending_approvals: vec!["worker".to_string()],
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: vec![crate::ports::workflow_runner::WorkflowRunApprovalRow {
                node_id: Some("worker".to_string()),
                tool: Some("send_email".to_string()),
                outcome: crate::ports::workflow_runner::WorkflowApprovalOutcome::ParkFailed,
                approval_id: None,
            }],
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);

        let notifications: Arc<dyn crate::ports::notifications::NotificationStore> =
            Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
            Some(notifications.clone()),
        );
        tool.execute(json!({ "id": "demo" }))
            .await
            .expect("execute");

        let feed = notifications
            .list(&CompanyId::new("acme"), "ceo")
            .await
            .expect("list");
        assert_eq!(feed.len(), 1, "{feed:?}");
        assert_eq!(feed[0].notification.kind, "workflow_run_stranded");
    }

    /// A run that finished cleanly badges nobody. The badge means "this needs
    /// you"; one per successful run would train the operator to ignore it.
    #[tokio::test]
    async fn a_healthy_agent_run_badges_nobody() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": [] } } }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        }));
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);

        let notifications: Arc<dyn crate::ports::notifications::NotificationStore> =
            Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));
        let tool = RunWorkflowTool::new(
            CompanyId::new("acme"),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
            Some(notifications.clone()),
        );
        tool.execute(json!({ "id": "demo" }))
            .await
            .expect("execute");

        let feed = notifications
            .list(&CompanyId::new("acme"), "ceo")
            .await
            .expect("list");
        assert!(feed.is_empty(), "{feed:?}");
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
            None,
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

    /// Issue #900 (tinysweeper `missing-test`): `summarize_run`'s blocked branch
    /// had no coverage at all, and the doc comment on `blocked` / `paused`
    /// (issue #881) — that a blocked node and a paused gate need separate
    /// sentences even though both ride `pending_approvals` — was untested along
    /// with it. One node blocks, a second is an ordinary paused gate: the
    /// summary must name the blocked node under "Blocked, waiting on a person"
    /// (never under "Paused for approval", which would tell the agent the run
    /// resumes on its own) and the paused node under "Paused for approval"
    /// only. The structural JSON counts (issue #881) must agree.
    #[tokio::test]
    async fn run_workflow_tool_separates_blocked_nodes_from_paused_gates() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());

        let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": [] } } }),
            // Issue #881: the union — the blocked node's id rides here too, and
            // `summarize_run` is what has to keep it out of the "Paused for
            // approval" line.
            pending_approvals: vec!["worker".to_string(), "gate".to_string()],
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
                node_id: "worker".to_string(),
                tools: vec!["publish_artifact".to_string()],
                approval_ids: vec!["appr-1".to_string()],
                unparkable: 0,
                stranded: 0,
                blockers: 0,
            }],
            approvals: vec![
                crate::ports::WorkflowRunApprovalRow {
                    node_id: Some("worker".to_string()),
                    tool: Some("publish_artifact".to_string()),
                    outcome: crate::ports::WorkflowApprovalOutcome::Parked,
                    approval_id: Some("appr-1".to_string()),
                },
                // Issue #900: a receipt for a call that did NOT land a card.
                // `run.approvals.len()` would count this as a second "parked"
                // approval; the JSON's `approvals_parked` must not.
                crate::ports::WorkflowRunApprovalRow {
                    node_id: Some("worker".to_string()),
                    tool: Some("publish_artifact".to_string()),
                    outcome: crate::ports::WorkflowApprovalOutcome::ParkFailed,
                    approval_id: None,
                },
            ],
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
            refs,
            RunOutputCache::default(),
            None,
        );
        let result = tool
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute");
        assert!(!result.is_error);
        let out = result.output_for_llm(true);
        assert!(
            out.contains("Blocked, waiting on a person") && out.contains("worker"),
            "{out}"
        );
        assert!(
            out.contains("Paused for approval") && out.contains("gate"),
            "{out}"
        );
        // The blocked node must not also read as an ordinary paused gate — that
        // sentence promises the run continues once it is decided, which is
        // false for a block (issue #881).
        let paused_line = out
            .lines()
            .find(|l| l.contains("Paused for approval"))
            .expect("a Paused for approval line");
        assert!(!paused_line.contains("worker"), "{out}");

        let payload = match &result.content[0] {
            openhuman_core::openhuman::skills::types::ToolContent::Json { data } => data.clone(),
            other => panic!("expected JSON payload, got {other:?}"),
        };
        assert_eq!(
            payload.get("blocked_nodes").and_then(Value::as_u64),
            Some(1)
        );
        // Issue #900: two receipts on this run (one parked, one that failed to
        // park), and the JSON count must name only the decidable one.
        assert_eq!(
            payload.get("approvals_parked").and_then(Value::as_u64),
            Some(1),
            "approvals_parked must exclude the ParkFailed receipt: {payload}"
        );
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
            None,
        );
        let result = tool
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute");
        assert!(result.is_error, "expected an error result");
        assert!(result.output_for_llm(false).contains("wired"), "{result:?}");
    }

    /// Issue #1865 (PR #1883 review comment 3877185396): an agent-started run
    /// that the engine returns `Err` on is the second run-outcome chokepoint
    /// `WorkflowSpawn` does not cover — console, scheduled, and resumed
    /// failures all file a `workflow_run_failed` notification through that
    /// type, but this tool's own `Ok(Err(err))` arm used to journal a finish
    /// and stop, leaving every agent-started failure invisible to an operator
    /// not watching this turn. Reused `crate::store::FsOps` as the
    /// notification-store double, the same one `WorkflowSpawn`'s own
    /// equivalent test (`a_failed_run_does_not_leak_the_raw_engine_error_into_its_notification`
    /// in `runtime::workflow_spawn`) uses.
    #[tokio::test]
    async fn run_workflow_tool_files_a_notification_when_the_engine_run_fails() {
        let dir = tempfile::tempdir().unwrap();
        seed_demo_workflow(dir.path());
        let runner: Arc<dyn WorkflowRunner> = Arc::new(FailingRunner);
        let handle = WorkflowRunnerHandle::default();
        handle.set(&runner);
        let company = CompanyId::new("acme");
        let notifications: Arc<dyn NotificationStore> =
            Arc::new(crate::store::FsOps::new(dir.path().to_path_buf()));

        let tool = RunWorkflowTool::new(
            company.clone(),
            Some(dir.path().to_path_buf()),
            Arc::new(MemStore::default()),
            handle,
            crate::runtime::RunSupervisor::default(),
            None,
            WorkflowRefQueue::default(),
            RunOutputCache::default(),
            Some(notifications.clone()),
        );
        let result = tool
            .execute(json!({ "id": "demo" }))
            .await
            .expect("execute");
        assert!(result.is_error, "the engine failed: {result:?}");

        let notes = notifications
            .list(&company, "anyone")
            .await
            .expect("list notifications");
        let failed = notes
            .iter()
            .find(|n| n.notification.kind == "workflow_run_failed")
            .expect(
                "an agent-started run that fails must file the same durable notification a \
                 console, scheduled, or resumed run does",
            );
        assert!(
            failed
                .notification
                .title
                .contains(crate::runtime::RUN_FAILED_DETAIL),
            "{:?}",
            failed.notification.title
        );
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
            None,
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
            None,
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
            None,
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
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
            None,
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
            None,
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
            None,
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
            None,
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

    /// Issue #1882 (tinysweeper): every other external boundary that turns a
    /// caller-supplied `ownerDesk` into a [`RawWorkflow`] runs it through
    /// [`RawWorkflow::normalize_owner_desk`] — the HTTP create route
    /// (`server::ops::workflows`) and the proposal-apply path
    /// (`workflow_create::raw_workflow_from_spec`) — so a blank/whitespace
    /// string is stored as `None`, not `Some("   ")`. The orchestrator's
    /// `create_workflow` tool passed `args.owner_desk` straight through
    /// instead, so a whitespace `ownerDesk` persisted verbatim in the graph's
    /// TOML and would defeat the `is_none()` fallback
    /// `apply_workflow_proposal` relies on later.
    #[tokio::test]
    async fn create_workflow_tool_normalizes_a_blank_owner_desk() {
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

        let mut body = greeter_body();
        body["ownerDesk"] = json!("   ");
        let result = tool.execute(body).await.expect("execute");
        assert!(!result.is_error, "{result:?}");

        let record = store.load(&company).await.unwrap().unwrap();
        assert_eq!(record.overlay_workflows.len(), 1);
        assert!(
            !record.overlay_workflows[0].toml.contains("owner_desk"),
            "a blank owner_desk must normalize to None and be omitted from the \
             persisted TOML, matching every other boundary that builds a \
             RawWorkflow: {}",
            record.overlay_workflows[0].toml
        );
    }

    /// PR #1882 review (bot finding on `orchestrator.rs:4788`).
    /// `UpdateWorkflowTool`'s description (built from this same schema via
    /// `create_graph_schema`) tells the agent to send `"ownerDesk": null` to
    /// unassign a desk, and `an_update_can_explicitly_clear_owner_desk_with_null`
    /// proves `execute` honors that. But `execute` is called directly in that
    /// test, bypassing the boundary a schema-constrained tool-calling client
    /// actually enforces: before this fix `ownerDesk` was declared bare
    /// `"type": "string"`, so such a client would reject the `null` argument
    /// before the call ever reached `execute`'s presence check, leaving the
    /// advertised clear operation reachable in tests but not in the field.
    #[test]
    fn owner_desk_schema_permits_null() {
        let schema = create_workflow_parameters_schema();
        let owner_desk_type = &schema["properties"]["ownerDesk"]["type"];
        let permits_null = owner_desk_type
            .as_array()
            .map(|types| types.iter().any(|t| t == "null"))
            .unwrap_or(false);
        assert!(
            permits_null,
            "ownerDesk schema type must include \"null\" so a schema-constrained \
             client can send the explicit-clear value the tool description \
             promises; got {owner_desk_type:?}"
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
            None,
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
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

    /// Issue #981 (part 2): the summary says a report did not go out.
    ///
    /// Before this, `summarize_run` never read `deliveries`, so a run whose
    /// report was refused closed with "The run reached its terminal node(s)
    /// without pausing for approval" and nothing else — a true sentence about a
    /// run that had just dropped its only output, which the model then reported
    /// upward as a clean run.
    #[test]
    fn the_summary_says_when_a_report_did_not_go_out() {
        let file = crate::company::parse_workflow(DEMO_WF).unwrap();
        let dropped = WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": ["the report"] } } }),
            pending_approvals: Vec::new(),
            deliveries: vec![crate::ports::DeliveryReport {
                node: "worker".into(),
                kind: "channel".into(),
                target: Some("operator".into()),
                status: crate::ports::DeliveryStatus::Failed,
                detail: "`operator` is not a workflow delivery channel — this runtime has:                          engineering"
                    .into(),
                reason: crate::ports::DeliveryReason::ChannelNotWired,
            }],
            cancelled: false,
            nodes: Vec::new(),
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        };
        let md = summarize_run(&file, &dropped, "run-drop", RunOutputStored::Stored);
        assert!(
            md.contains("1 report(s) did NOT reach a destination"),
            "{md}"
        );
        assert!(md.contains("`worker` (channel)"), "{md}");
        // The reason, from the closed set — never `detail`, which quotes what a
        // transport said and is for the operator's own surfaces (issue #248).
        assert!(
            md.contains(&crate::ports::DeliveryReason::ChannelNotWired.to_string()),
            "{md}"
        );
        assert!(
            !md.contains("this runtime has: engineering"),
            "the operator-only `detail` must not ride the summary: {md}"
        );
        // And it does not claim the graph broke: the per-node line still reports
        // what the node produced.
        assert!(md.contains("1 item(s) — the report"), "{md}");

        // A run that delivered fine says nothing about delivery at all, so an
        // ordinary summary is unchanged.
        let clean = WorkflowRun {
            deliveries: vec![crate::ports::DeliveryReport {
                node: "worker".into(),
                kind: "owner".into(),
                target: Some("ada@example.com".into()),
                status: crate::ports::DeliveryStatus::Sent,
                detail: "emailed the company's admin".into(),
                reason: crate::ports::DeliveryReason::OwnerEmailed,
            }],
            ..dropped.clone()
        };
        let md = summarize_run(&file, &clean, "run-ok", RunOutputStored::Stored);
        assert!(!md.contains("did NOT reach a destination"), "{md}");

        // A report parked for an operator's approval is waiting on a person,
        // not lost — counting it here would tell the model to go fix a queue
        // that is working.
        let parked = WorkflowRun {
            deliveries: vec![crate::ports::DeliveryReport {
                node: "worker".into(),
                kind: "email".into(),
                target: Some("new@example.com".into()),
                status: crate::ports::DeliveryStatus::Pending,
                detail: "waiting in Approvals".into(),
                reason: crate::ports::DeliveryReason::ParkedForApproval,
            }],
            ..dropped.clone()
        };
        let md = summarize_run(&file, &parked, "run-parked", RunOutputStored::Stored);
        assert!(!md.contains("did NOT reach a destination"), "{md}");

        // Issue #981, the second half. This paragraph's own prose is the
        // argument: it says the report "did not go out, and it will not without
        // a change". Neither is true of a test run, which attempted nothing on
        // purpose, nor of a continuation whose report an earlier run in the
        // lineage already sent — so telling the model to "fix the destination"
        // for either would send it at a graph that is behaving as designed.
        for reason in [
            crate::ports::DeliveryReason::DryRun,
            crate::ports::DeliveryReason::AlreadyDelivered,
        ] {
            let accounted = WorkflowRun {
                deliveries: vec![crate::ports::DeliveryReport {
                    node: "worker".into(),
                    kind: "channel".into(),
                    target: Some("engineering".into()),
                    status: crate::ports::DeliveryStatus::Skipped,
                    detail: "nothing was sent".into(),
                    reason,
                }],
                ..dropped.clone()
            };
            let md = summarize_run(&file, &accounted, "run-skip", RunOutputStored::Stored);
            assert!(
                !md.contains("did NOT reach a destination"),
                "{reason:?}: {md}"
            );
        }

        // The deliberate non-move: an `output` node with nowhere to send DID
        // lose its report, and the model is exactly the reader that should be
        // told (issues #925 / #947 / #963).
        let nowhere = WorkflowRun {
            deliveries: vec![crate::ports::DeliveryReport {
                node: "worker".into(),
                kind: "none".into(),
                target: None,
                status: crate::ports::DeliveryStatus::Skipped,
                detail: "this output node has no destination".into(),
                reason: crate::ports::DeliveryReason::NoDestinationConfigured,
            }],
            ..dropped.clone()
        };
        let md = summarize_run(&file, &nowhere, "run-nowhere", RunOutputStored::Stored);
        assert!(
            md.contains("1 report(s) did NOT reach a destination"),
            "{md}"
        );
    }

    /// Codex review on #1990 (#3905407434): a `halt_benign` judge verdict
    /// scrubs the declined node from `run.output`, so this summary — which
    /// derives its per-node lines from that map and separately inspects only
    /// `Error` rows — called the node "not reached" and still claimed the run
    /// reached its terminal nodes. The intentional stop was invisible to the
    /// agent that started the run.
    #[test]
    fn the_summary_reports_a_declined_node_as_not_needed() {
        let file = crate::company::parse_workflow(DEMO_WF).unwrap();
        let declined = WorkflowRun {
            output: json!({ "nodes": { "start": { "items": ["go"] } } }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: vec![crate::ports::WorkflowRunNodeRow {
                node_id: "worker".into(),
                status: WorkflowNodeStatus::Declined,
                elapsed_ms: 12,
                diagnostics: Vec::new(),
            }],
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        };
        let md = summarize_run(&file, &declined, "run-declined", RunOutputStored::Stored);
        assert!(
            md.contains("not needed"),
            "a declined node must read as an intentional stop: {md}"
        );
        assert!(
            !md.contains("**Worker** (`worker`, agent): not reached"),
            "a declined node is not an unreached one: {md}"
        );
        assert!(
            !md.contains("reached its terminal node(s) without pausing"),
            "the run stopped on purpose; the happy-path sentence is false: {md}"
        );
        assert!(
            !md.contains("NOT a clean run"),
            "a declined node is not an error: {md}"
        );
    }

    /// Codex (PR #1883 review comment 3892522591): a node under `on_error =
    /// "continue"`/`"route"` settles the run `Degraded`, and `runner.rs`
    /// already writes a per-node notice for it — but `summarize_run` never
    /// read `run.nodes`, so an agent-started run through this exact case
    /// summarized as "reached its terminal node(s) without pausing for
    /// approval" with no hint anything went wrong. This pins the fix: a row
    /// still `Error` after settle must show up in the tool result.
    #[test]
    fn the_summary_says_when_a_node_errored_and_the_run_continued() {
        let file = crate::company::parse_workflow(DEMO_WF).unwrap();
        let degraded = WorkflowRun {
            output: json!({ "nodes": { "worker": { "items": ["partial"] } } }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: vec![crate::ports::WorkflowRunNodeRow {
                node_id: "worker".into(),
                status: WorkflowNodeStatus::Error,
                elapsed_ms: 12,
                diagnostics: Vec::new(),
            }],
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        };
        let md = summarize_run(&file, &degraded, "run-degraded", RunOutputStored::Stored);
        assert!(
            md.contains("did not finish cleanly, and the run continued past it"),
            "{md}"
        );
        assert!(md.contains("worker"), "{md}");
        assert!(
            md.contains("NOT a clean run"),
            "an agent skimming for the happy-path sentence must not miss this: {md}"
        );

        // A node that finished clean says nothing about it — an ordinary
        // summary is unchanged.
        let clean = WorkflowRun {
            nodes: vec![crate::ports::WorkflowRunNodeRow {
                node_id: "worker".into(),
                status: WorkflowNodeStatus::Ok,
                elapsed_ms: 12,
                diagnostics: Vec::new(),
            }],
            ..degraded.clone()
        };
        let md = summarize_run(&file, &clean, "run-clean", RunOutputStored::Stored);
        assert!(!md.contains("did not finish cleanly"), "{md}");
        assert!(!md.contains("NOT a clean run"), "{md}");

        // A blocked node must not ALSO print here — it is already named by the
        // "Blocked, waiting on a person" paragraph above, sourced from
        // `blocked_nodes`, not from a node row's own status (the host never
        // leaves a blocked row `Error`; see `WorkflowNodeStatus::Blocked`'s doc).
        let blocked = WorkflowRun {
            nodes: vec![crate::ports::WorkflowRunNodeRow {
                node_id: "worker".into(),
                status: WorkflowNodeStatus::Blocked,
                elapsed_ms: 12,
                diagnostics: Vec::new(),
            }],
            blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
                node_id: "worker".into(),
                tools: vec!["send_email".into()],
                approval_ids: vec!["appr-1".into()],
                unparkable: 0,
                stranded: 0,
                blockers: 0,
            }],
            ..degraded.clone()
        };
        let md = summarize_run(&file, &blocked, "run-blocked", RunOutputStored::Stored);
        assert!(md.contains("Blocked, waiting on a person"), "{md}");
        assert!(
            !md.contains("did not finish cleanly"),
            "a blocked row must not double up with the degraded paragraph: {md}"
        );
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
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: Vec::new(),
                approvals: Vec::new(),
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
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: Vec::new(),
                approvals: Vec::new(),
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
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: Vec::new(),
                approvals: Vec::new(),
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
                notices: Vec::new(),
                board: Vec::new(),
                blocked_nodes: Vec::new(),
                approvals: Vec::new(),
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

    // -----------------------------------------------------------------------
    // Issue #661: the queue is scoped per claimant
    // -----------------------------------------------------------------------

    /// A card, titled so a drain can be identified by what it carried.
    fn card(title: &str) -> Delegation {
        Delegation::SpawnTask {
            title: title.to_string(),
            note: None,
            assignee: None,
        }
    }

    fn hand_off() -> Delegation {
        Delegation::DelegateToDesk {
            desk: "design".to_string(),
            instruction: "have a look".to_string(),
        }
    }

    fn titles(drained: Vec<Delegation>) -> Vec<String> {
        drained
            .into_iter()
            .map(|d| match d {
                Delegation::SpawnTask { title, .. } => title,
                other => panic!("expected a card, got {other:?}"),
            })
            .collect()
    }

    fn stage(queue: &DelegationQueue, d: Delegation) -> Staged {
        queue.push_within_cap(d, MAX_DELEGATIONS_PER_TURN, NO_DEPTH_BOUND)
    }

    /// **The regression this whole change exists for.**
    ///
    /// A workflow run taking a claim while the chat cycle has work staged must
    /// leave that work alone. Before the scoping, `claim_as` opened with a
    /// global `clear()`, so this exact interleaving destroyed a chat turn's
    /// staged card and its refusal — and the turn had already told the operator
    /// the card was opened.
    ///
    /// Runs are `tokio::spawn`ed and are not under the cycle lock (#401 allows
    /// several at once), so this interleaving is reachable rather than
    /// theoretical.
    #[tokio::test]
    async fn a_workflow_claim_leaves_a_concurrent_chat_turns_staged_work_intact() {
        let queue = DelegationQueue::default();

        // A chat turn is mid-flight with a card staged and a refusal recorded.
        let _chat = queue.claim();
        assert_eq!(stage(&queue, card("chat")), Staged::Queued);
        queue.push_refusal("nonexistent-desk".to_string());

        // A workflow run claims, concurrently. This is the moment that used to
        // wipe the chat's bucket.
        let run = queue.claim_board("run-1");
        run.scoped(async { assert_eq!(stage(&queue, card("run")), Staged::Queued) })
            .await;

        // The chat's staged card and refusal are both still there…
        assert_eq!(queue.queued(), 1);
        assert_eq!(queue.refusals_queued(), 1);
        assert_eq!(titles(queue.drain(MAX_DELEGATIONS_PER_TURN)), ["chat"]);
        assert_eq!(
            queue.drain_refusals(MAX_DELEGATIONS_PER_TURN),
            ["nonexistent-desk"]
        );

        // …and the run still has its own, drained separately.
        let run_drained = run
            .scoped(async { queue.drain(MAX_DELEGATIONS_PER_TURN) })
            .await;
        assert_eq!(titles(run_drained), ["run"]);
    }

    /// The half of the defect that is **live today**, with no drain wired and
    /// nothing else changed.
    ///
    /// `DelegateToDeskTool` calls [`DelegationQueue::push_refusal`] on the
    /// ungrounded path *before* it consults the claim, so an invented desk named
    /// by a workflow node already reaches the shared vector. A chat turn's
    /// `drain_refusals` would then take it, record it on that turn's card, and
    /// clear it — a hand-off nobody on that turn attempted.
    #[tokio::test]
    async fn a_runs_ungrounded_hand_off_is_not_recorded_on_a_chat_turns_card() {
        let queue = DelegationQueue::default();
        let _chat = queue.claim();

        let run = queue.claim_board("run-1");
        run.scoped(async { queue.push_refusal("marketing".to_string()) })
            .await;

        // Nothing to report on the chat turn's card: it attempted no hand-off.
        assert_eq!(queue.refusals_queued(), 0);
        assert!(queue.drain_refusals(MAX_DELEGATIONS_PER_TURN).is_empty());

        // The run's own refusal is intact and still its own to read.
        let seen = run
            .scoped(async { queue.drain_refusals(MAX_DELEGATIONS_PER_TURN) })
            .await;
        assert_eq!(seen, ["marketing"]);
    }

    /// Two runs and the chat cycle interleaved: each sees only its own, and
    /// neither draining nor claiming reaches across.
    #[tokio::test]
    async fn two_runs_and_the_chat_cycle_neither_drain_nor_clear_each_other() {
        let queue = DelegationQueue::default();

        let _chat = queue.claim();
        assert_eq!(stage(&queue, card("chat")), Staged::Queued);

        let run_a = queue.claim_board("run-a");
        run_a
            .scoped(async { assert_eq!(stage(&queue, card("a")), Staged::Queued) })
            .await;

        // B claims *after* A staged — the acquire-time clear must not reach A.
        let run_b = queue.claim_board("run-b");
        run_b
            .scoped(async { assert_eq!(stage(&queue, card("b")), Staged::Queued) })
            .await;

        assert_eq!(queue.queued(), 1, "the chat cycle sees only its own");
        assert_eq!(run_a.scoped(async { queue.queued() }).await, 1);
        assert_eq!(run_b.scoped(async { queue.queued() }).await, 1);

        // Draining A takes A's and only A's.
        let drained_a = run_a
            .scoped(async { queue.drain(MAX_DELEGATIONS_PER_TURN) })
            .await;
        assert_eq!(titles(drained_a), ["a"]);
        assert_eq!(queue.queued(), 1);
        assert_eq!(run_b.scoped(async { queue.queued() }).await, 1);

        assert_eq!(titles(queue.drain(MAX_DELEGATIONS_PER_TURN)), ["chat"]);
        let drained_b = run_b
            .scoped(async { queue.drain(MAX_DELEGATIONS_PER_TURN) })
            .await;
        assert_eq!(titles(drained_b), ["b"]);
    }

    /// A claim's `Drop` discards its own bucket and un-claims its own scope —
    /// and reaches nothing else. A cancelled run's staged writes dying with the
    /// run is the intended semantics; a chat turn's surviving it is the point.
    #[tokio::test]
    async fn dropping_a_claim_discards_only_its_own_bucket() {
        let queue = DelegationQueue::default();

        let _chat = queue.claim();
        assert_eq!(stage(&queue, card("chat")), Staged::Queued);

        {
            let run = queue.claim_board("run-1");
            run.scoped(async { assert_eq!(stage(&queue, card("run")), Staged::Queued) })
                .await;
            run.scoped(async { queue.push_refusal("ghost".to_string()) })
                .await;
        } // the run is cancelled here

        // Its bucket went with it, and its scope is claimable again from
        // scratch rather than left committed.
        let after = CURRENT_SCOPE
            .scope(DelegationScope::Run("run-1".to_string()), async {
                (queue.queued(), queue.refusals_queued(), queue.claim_state())
            })
            .await;
        assert_eq!(after, (0, 0, DrainClaim::Unclaimed));

        // The chat turn is untouched — still claimed, still holding its card.
        assert_eq!(queue.claim_state(), DrainClaim::Full);
        assert_eq!(titles(queue.drain(MAX_DELEGATIONS_PER_TURN)), ["chat"]);
    }

    /// The #176 scope chain is per claimant, and its depth accounting is
    /// unchanged by that.
    ///
    /// Depth is still exactly `chain.len()` and still gates a hand-off at the
    /// bound; what it no longer does is count another claimant's nesting.
    #[tokio::test]
    async fn a_scope_chain_is_per_claimant_and_depth_is_unchanged() {
        let queue = DelegationQueue::default();
        let _chat = queue.claim();

        let _outer = queue.enter_scope("design".to_string());
        assert_eq!(queue.scope_depth(), 1);
        assert_eq!(queue.scope_chain(), ["design"]);

        let run = queue.claim_board("run-1");
        run.scoped(async {
            // A run opens its own chain at depth 0 however deep the chat is.
            assert_eq!(queue.scope_depth(), 0);
            assert!(queue.scope_chain().is_empty());

            let _a = queue.enter_scope("eng".to_string());
            let _b = queue.enter_scope("qa".to_string());
            assert_eq!(queue.scope_depth(), 2);
            assert_eq!(queue.scope_chain(), ["eng", "qa"]);
        })
        .await;

        // The chat's chain is exactly as deep as it was left, and its guard
        // popped from its own chain rather than the run's.
        assert_eq!(queue.scope_depth(), 1);
        assert_eq!(queue.scope_chain(), ["design"]);

        // Depth still gates at the bound, counting this claimant's chain only:
        // one level deep against a bound of 1 refuses…
        assert_eq!(
            queue.push_within_cap(hand_off(), MAX_DELEGATIONS_PER_TURN, 1),
            Staged::NoDrain(NoDrainReason::Depth)
        );
        // …and against a bound of 2 it stages, which a run's two levels would
        // have blocked had they been counted here.
        assert_eq!(
            queue.push_within_cap(hand_off(), MAX_DELEGATIONS_PER_TURN, 2),
            Staged::Queued
        );
    }

    /// The [`DrainClaim::Board`] permit matrix: both kinds a run may perform
    /// stage, and both it may not are refused — each for its own reason.
    #[tokio::test]
    async fn a_board_claim_permits_cards_and_refuses_review_and_hand_off() {
        let queue = DelegationQueue::default();
        let run = queue.claim_board("run-1");

        run.scoped(async {
            assert_eq!(stage(&queue, card("open a card")), Staged::Queued);
            assert_eq!(
                stage(
                    &queue,
                    Delegation::AssignTask {
                        task_id: "t1".to_string(),
                        assignee: "design".to_string(),
                        note: None,
                    }
                ),
                Staged::Queued
            );

            // Lifecycle is the operator's lane.
            assert_eq!(
                stage(
                    &queue,
                    Delegation::ReviewTask {
                        task_id: "t1".to_string(),
                        decision: ReviewDecision::Approve,
                        note: None,
                    }
                ),
                Staged::NoDrain(NoDrainReason::WorkflowLifecycle)
            );
            // A hand-off has nowhere to put the reply it exists for.
            assert_eq!(
                stage(&queue, hand_off()),
                Staged::NoDrain(NoDrainReason::WorkflowHandOff)
            );
        })
        .await;
    }

    /// The refusal text is what a model reads and reacts to, so both wordings
    /// have to name the real cause and what the run *can* do instead — and must
    /// not be each other's.
    #[test]
    fn the_two_workflow_refusals_say_what_the_run_can_do_instead() {
        let lifecycle = no_drain(
            REVIEW_TASK_TOOL,
            "the card was NOT reviewed",
            NoDrainReason::WorkflowLifecycle,
        );
        assert!(
            lifecycle.contains("running inside a workflow"),
            "{lifecycle}"
        );
        assert!(lifecycle.contains("operator's call"), "{lifecycle}");
        assert!(
            lifecycle.contains("`spawn_task`") && lifecycle.contains("`assign_task`"),
            "it must name what the run can do instead: {lifecycle}"
        );
        assert!(
            !lifecycle.contains("no conversation"),
            "the lifecycle refusal must not borrow the hand-off's cause: {lifecycle}"
        );

        let hand_off = no_drain(
            DELEGATE_TO_DESK_TOOL,
            "nothing was handed to the design desk",
            NoDrainReason::WorkflowHandOff,
        );
        assert!(hand_off.contains("running inside a workflow"), "{hand_off}");
        assert!(hand_off.contains("no conversation"), "{hand_off}");
        assert!(
            hand_off.contains("`spawn_task`"),
            "it must name the durable alternative: {hand_off}"
        );
        assert!(
            !hand_off.contains("operator's call"),
            "the hand-off refusal must not borrow the lifecycle's cause: {hand_off}"
        );

        // Both keep the do-not-report-it-as-done half every refusal here needs.
        for text in [&lifecycle, &hand_off] {
            assert!(text.contains("Do not retry this call"), "{text}");
            assert!(text.contains("do NOT report"), "{text}");
        }

        // …and they stay countable apart in the logs, from each other and from
        // the three that came before.
        let labels = [
            NoDrainReason::Unwired,
            NoDrainReason::Triage,
            NoDrainReason::Depth,
            NoDrainReason::WorkflowLifecycle,
            NoDrainReason::WorkflowHandOff,
        ]
        .map(|r| r.as_str());
        let unique: std::collections::BTreeSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }

    // -----------------------------------------------------------------------
    // Issue #1859: the execution-state read trio (`list_tasks` / `read_task` /
    // `read_run`) and `query_company`'s `## Board` section.
    // -----------------------------------------------------------------------

    /// A minimal board card, for fixtures below. Named `task_card` rather than
    /// `card` — that name is already the `Delegation` fixture above.
    fn task_card(id: &str, title: &str, column: &str, assignee: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: TaskTitle::authored(title),
            note: None,
            column: column.to_string(),
            priority: "medium".to_string(),
            assignee: assignee.to_string(),
            updated_at_millis: 1,
            origin: crate::ports::TaskOrigin::new(None, None),
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            origin_message_seq: None,
            bounced: None,
        }
    }

    #[tokio::test]
    async fn list_tasks_filters_by_column_and_assignee_and_excludes_done_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(crate::store::FsOps::new(dir.path()));
        let tasks: Arc<dyn TaskStore> = fs;
        let company = CompanyId::new("acme");
        tasks
            .upsert(
                &company,
                &task_card(
                    "t-1",
                    "Draft the memo",
                    crate::ports::tasks::COLUMN_TODO,
                    "maya",
                ),
            )
            .await
            .unwrap();
        tasks
            .upsert(
                &company,
                &task_card(
                    "t-2",
                    "Fix the flaky test",
                    crate::ports::tasks::COLUMN_PAUSED,
                    "engineer",
                ),
            )
            .await
            .unwrap();
        tasks
            .upsert(
                &company,
                &task_card("t-3", "Ship the release", COLUMN_DONE, "maya"),
            )
            .await
            .unwrap();

        let tool = ListTasksTool::new(company, Some(tasks), None);

        let default_view = tool.execute(json!({})).await.unwrap().output_for_llm(true);
        assert!(default_view.contains("Draft the memo"), "{default_view}");
        assert!(
            default_view.contains("Fix the flaky test"),
            "{default_view}"
        );
        assert!(
            !default_view.contains("Ship the release"),
            "done cards must be excluded by default: {default_view}"
        );

        let by_column = tool
            .execute(json!({ "column": "paused" }))
            .await
            .unwrap()
            .output_for_llm(true);
        assert!(by_column.contains("Fix the flaky test"), "{by_column}");
        assert!(!by_column.contains("Draft the memo"), "{by_column}");

        let by_assignee = tool
            .execute(json!({ "assignee": "MAYA" }))
            .await
            .unwrap()
            .output_for_llm(true);
        assert!(
            by_assignee.contains("Draft the memo"),
            "case-insensitive assignee match: {by_assignee}"
        );
        assert!(!by_assignee.contains("Fix the flaky test"), "{by_assignee}");

        let done_explicit = tool
            .execute(json!({ "column": "done" }))
            .await
            .unwrap()
            .output_for_llm(true);
        assert!(
            done_explicit.contains("Ship the release"),
            "an explicit `column: done` must still answer: {done_explicit}"
        );
    }

    #[tokio::test]
    async fn list_tasks_truncates_with_an_honest_marker() {
        let dir = tempfile::tempdir().unwrap();
        let tasks: Arc<dyn TaskStore> = Arc::new(crate::store::FsOps::new(dir.path()));
        let company = CompanyId::new("acme");
        for n in 0..(LIST_TASKS_LIMIT + 5) {
            tasks
                .upsert(
                    &company,
                    &task_card(
                        &format!("t-{n}"),
                        &format!("Card {n}"),
                        crate::ports::tasks::COLUMN_TODO,
                        "maya",
                    ),
                )
                .await
                .unwrap();
        }

        let tool = ListTasksTool::new(company, Some(tasks), None);
        let out = tool.execute(json!({})).await.unwrap().output_for_llm(true);
        assert!(out.contains("TRUNCATED"), "{out}");
        assert!(out.contains("5 more card"), "{out}");
    }

    #[tokio::test]
    async fn list_tasks_reports_unavailable_when_the_board_is_unwired() {
        let tool = ListTasksTool::new(CompanyId::new("acme"), None, None);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(
            result.is_error,
            "no task board wired must be a refusal, not an empty board"
        );
        assert!(
            result.output_for_llm(true).contains("No task board wired"),
            "{:?}",
            result.output_for_llm(true)
        );
    }

    #[tokio::test]
    async fn read_task_renders_header_every_attempt_and_falls_back_to_the_cards_output_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(crate::store::FsOps::new(dir.path()));
        let tasks: Arc<dyn TaskStore> = fs.clone();
        let runs: Arc<dyn RunStore> = fs;
        let company = CompanyId::new("acme");

        let mut card = task_card(
            "t-1",
            "Investigate the outage",
            crate::ports::tasks::COLUMN_IN_REVIEW,
            "engineer",
        );
        card.note = Some("check the load balancer first".to_string());
        card.output = Some(crate::ports::tasks::TaskOutput {
            source: crate::ports::tasks::TaskOutputSource::Run {
                run_id: "r-2".to_string(),
                attempt: Some(2),
            },
            at_millis: 5,
            artifacts: Vec::new(),
            workflows: Vec::new(),
        });
        tasks.upsert(&company, &card).await.unwrap();

        let mut r1 = runs
            .create_run(
                &company,
                crate::ports::runs::NewRun::for_task("r-1", "t-1", "engineer"),
            )
            .await
            .unwrap();
        r1.status = RunStatus::Failed;
        r1.error = Some("timed out".to_string());
        runs.put_run(&company, &r1).await.unwrap();
        let mut r2 = runs
            .create_run(
                &company,
                crate::ports::runs::NewRun::for_task("r-2", "t-1", "engineer"),
            )
            .await
            .unwrap();
        r2.status = RunStatus::Succeeded;
        runs.put_run(&company, &r2).await.unwrap();

        let tool = ReadTaskTool::new(company, Some(tasks), Some(runs), None);
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(out.contains("Investigate the outage"), "{out}");
        assert!(out.contains("check the load balancer first"), "{out}");
        assert!(out.contains("attempt 1"), "{out}");
        assert!(out.contains("attempt 2"), "{out}");
        assert!(out.contains("timed out"), "{out}");
        // No artifact store wired: falls back to the card's own recorded
        // output stamp rather than fabricating anything.
        assert!(out.contains("run `r-2`"), "{out}");
        assert!(out.contains("attempt 2)"), "{out}");
    }

    #[tokio::test]
    async fn read_task_errors_on_an_unknown_id_instead_of_fabricating_a_card() {
        let dir = tempfile::tempdir().unwrap();
        let tasks: Arc<dyn TaskStore> = Arc::new(crate::store::FsOps::new(dir.path()));
        let tool = ReadTaskTool::new(CompanyId::new("acme"), Some(tasks), None, None);
        let result = tool.execute(json!({ "task_id": "nope" })).await.unwrap();
        assert!(result.is_error, "an unknown task_id must error");
        let text = result.output_for_llm(true);
        assert!(text.contains("nope"), "{text}");
        assert!(text.contains("list_tasks"), "{text}");
    }

    /// Fail-closed by construction (issue #1859's approved redaction posture):
    /// `read_task` never reads [`RunRecord::usage`], so a run's USD cost cannot
    /// reach its rendering no matter what that run cost.
    #[tokio::test]
    async fn read_task_never_renders_a_runs_usd_cost() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(crate::store::FsOps::new(dir.path()));
        let tasks: Arc<dyn TaskStore> = fs.clone();
        let runs: Arc<dyn RunStore> = fs;
        let company = CompanyId::new("acme");
        tasks
            .upsert(
                &company,
                &task_card(
                    "t-1",
                    "Send the invoice",
                    crate::ports::tasks::COLUMN_IN_PROGRESS,
                    "finance",
                ),
            )
            .await
            .unwrap();
        let mut run = runs
            .create_run(
                &company,
                crate::ports::runs::NewRun::for_task("r-1", "t-1", "finance"),
            )
            .await
            .unwrap();
        run.status = RunStatus::Succeeded;
        run.usage = crate::ports::types::TokenUsage {
            input: 500,
            output: 200,
            cached_input: 0,
            cost_usd: 4.20,
        };
        runs.put_run(&company, &run).await.unwrap();

        let tool = ReadTaskTool::new(company, Some(tasks), Some(runs), None);
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(
            !out.contains("4.2") && !out.to_lowercase().contains("cost") && !out.contains("usd"),
            "a run's USD cost must never reach read_task: {out}"
        );
    }

    #[tokio::test]
    async fn read_run_reads_an_agent_attempt_row() {
        let dir = tempfile::tempdir().unwrap();
        let runs: Arc<dyn RunStore> = Arc::new(crate::store::FsOps::new(dir.path()));
        let company = CompanyId::new("acme");
        let mut run = runs
            .create_run(
                &company,
                crate::ports::runs::NewRun::for_task("r-1", "t-1", "engineer"),
            )
            .await
            .unwrap();
        run.status = RunStatus::Failed;
        run.error = Some("connection refused".to_string());
        runs.put_run(&company, &run).await.unwrap();

        let tool = ReadRunTool::new(company, Some(runs), None);
        let out = tool
            .execute(json!({ "run_id": "r-1" }))
            .await
            .unwrap()
            .output_for_llm(true);
        assert!(out.contains("failed"), "{out}");
        assert!(out.contains("connection refused"), "{out}");
        assert!(out.contains("t-1"), "{out}");
    }

    /// The dual-source lookup's second half: no [`RunStore`] row named
    /// `run_id`, so `read_run` folds it out of the journal via
    /// [`crate::server::ops::workflows::fold_run_events`] instead — the same
    /// fold the console's run-history route reads.
    #[tokio::test]
    async fn read_run_folds_a_workflow_run_out_of_the_journal_when_no_attempt_row_exists() {
        use crate::ports::types::{StoredEvent, WorkflowNodeStatus};
        use futures::stream::{self, BoxStream};

        struct FixedLog(Vec<StoredEvent>);

        #[async_trait]
        impl EventLog for FixedLog {
            async fn append(
                &self,
                _id: &CompanyId,
                _event: CompanyEvent,
            ) -> crate::Result<EventSeq> {
                unreachable!("read_run only reads")
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
            fn subscribe(
                &self,
                _id: &CompanyId,
            ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
                Box::pin(stream::empty())
            }
        }

        let company = CompanyId::new("acme");
        let history = vec![
            StoredEvent {
                seq: EventSeq::new(0),
                company: company.clone(),
                event: CompanyEvent::WorkflowRunStarted {
                    workflow_id: "demo".to_string(),
                    run_id: "wf-run-1".to_string(),
                    scheduled: false,
                    started_by: None,
                    resume_semantic: None,
                },
                at_millis: 1,
            },
            StoredEvent {
                seq: EventSeq::new(1),
                company: company.clone(),
                event: CompanyEvent::WorkflowNodeFinished {
                    workflow_id: "demo".to_string(),
                    run_id: "wf-run-1".to_string(),
                    node_id: "fetch".to_string(),
                    status: WorkflowNodeStatus::Ok,
                    elapsed_ms: 10,
                    diagnostics: Vec::new(),
                    agent_run_id: None,
                },
                at_millis: 2,
            },
            StoredEvent {
                seq: EventSeq::new(2),
                company: company.clone(),
                event: CompanyEvent::WorkflowRunFinished {
                    workflow_id: "demo".to_string(),
                    scheduled: false,
                    run_id: Some("wf-run-1".to_string()),
                    deliveries: Vec::new(),
                    pending_approvals: vec!["gate-1".to_string()],
                    error: None,
                    cancelled: false,
                    notices: Vec::new(),
                    board: Vec::new(),
                    blocked_nodes: Vec::new(),
                    approvals: Vec::new(),
                },
                at_millis: 3,
            },
        ];
        let events: Arc<dyn EventLog> = Arc::new(FixedLog(history));

        let tool = ReadRunTool::new(company, None, Some(events));
        let out = tool
            .execute(json!({ "run_id": "wf-run-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(out.contains("demo"), "{out}");
        assert!(out.contains("fetch"), "{out}");
        assert!(out.contains("1 pending approval"), "{out}");
        // Summarized, never dumped: no step trace, no node output/argument text
        // rides this fold in the first place (see `WorkflowNodeFinished`'s own
        // doc comment), so there is nothing here to assert absent beyond what
        // the fixture itself never supplied.
    }

    #[tokio::test]
    async fn read_run_errors_on_an_id_that_is_neither_an_attempt_nor_a_workflow_run() {
        let dir = tempfile::tempdir().unwrap();
        let runs: Arc<dyn RunStore> = Arc::new(crate::store::FsOps::new(dir.path()));
        let tool = ReadRunTool::new(CompanyId::new("acme"), Some(runs), None);
        let result = tool.execute(json!({ "run_id": "nope" })).await.unwrap();
        assert!(result.is_error, "an unknown run_id must error");
        assert!(result.output_for_llm(true).contains("nope"));
    }

    #[tokio::test]
    async fn query_company_board_section_groups_open_cards_by_column_and_omits_done() {
        let dir = tempfile::tempdir().unwrap();
        let tasks: Arc<dyn TaskStore> = Arc::new(crate::store::FsOps::new(dir.path()));
        let company = CompanyId::new("acme");
        tasks
            .upsert(
                &company,
                &task_card(
                    "t-1",
                    "Draft the memo",
                    crate::ports::tasks::COLUMN_TODO,
                    "maya",
                ),
            )
            .await
            .unwrap();
        tasks
            .upsert(
                &company,
                &task_card("t-2", "Ship the release", COLUMN_DONE, "maya"),
            )
            .await
            .unwrap();

        let tool = QueryCompanyTool::new(company, None, None, None, None, Some(tasks));
        let out = tool.execute(json!({})).await.unwrap().output_for_llm(true);

        assert!(out.contains("## Board"), "{out}");
        assert!(out.contains("Draft the memo"), "{out}");
        assert!(
            !out.contains("Ship the release"),
            "the Board section must exclude Done, like `list_tasks`: {out}"
        );
        // Desks stays present AND after Board never gets to run — Board is the
        // LAST section, so this just pins Desks is still there at all.
        assert!(out.contains("## Desks"), "{out}");
    }

    #[tokio::test]
    async fn query_company_board_section_is_unavailable_when_the_board_is_unwired() {
        let tool = QueryCompanyTool::new(CompanyId::new("acme"), None, None, None, None, None);
        let out = tool.execute(json!({})).await.unwrap().output_for_llm(true);
        assert!(out.contains("## Board"), "{out}");
        assert!(out.contains("Board unavailable"), "{out}");
    }

    /// The ordering guarantee the byte-budget reasoning depends on: Board is
    /// the LAST section, so a company with an oversized board can never push
    /// the Desks list — which `delegate_to_desk` needs to ground a hand-off —
    /// out of the tool result ahead of it.
    #[tokio::test]
    async fn query_company_desks_section_still_renders_after_the_board_section() {
        let tool = QueryCompanyTool::new(CompanyId::new("acme"), None, None, None, None, None);
        let out = tool.execute(json!({})).await.unwrap().output_for_llm(true);
        let desks_at = out.find("## Desks").expect("Desks section present");
        let board_at = out.find("## Board").expect("Board section present");
        assert!(
            board_at > desks_at,
            "Board must render after Desks, never before: {out}"
        );
    }

    /// A task board that cannot answer, so a read failure never collapses
    /// into an empty or missing board.
    struct BrokenTaskStore;

    #[async_trait]
    impl TaskStore for BrokenTaskStore {
        async fn list(&self, _company: &CompanyId) -> crate::Result<Vec<TaskRecord>> {
            Err(OpenCompanyError::Store(
                "simulated board read failure".into(),
            ))
        }
        async fn upsert(&self, _company: &CompanyId, _task: &TaskRecord) -> crate::Result<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn delete(&self, _company: &CompanyId, _id: &str) -> crate::Result<bool> {
            unimplemented!("not exercised by these tests")
        }
    }

    #[tokio::test]
    async fn list_tasks_reports_a_read_failure_instead_of_an_empty_board() {
        let tasks: Arc<dyn TaskStore> = Arc::new(BrokenTaskStore);
        let tool = ListTasksTool::new(CompanyId::new("acme"), Some(tasks), None);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(
            result.is_error,
            "a board read failure must be a refusal, not a silently empty board"
        );
        let text = result.output_for_llm(true);
        assert!(
            !text.contains("No matching cards"),
            "must not claim the board is simply empty: {text}"
        );
        assert!(text.contains("Couldn't read the task board"), "{text}");
    }

    #[tokio::test]
    async fn read_task_reports_a_read_failure_instead_of_a_missing_card() {
        let tasks: Arc<dyn TaskStore> = Arc::new(BrokenTaskStore);
        let tool = ReadTaskTool::new(CompanyId::new("acme"), Some(tasks), None, None);
        let result = tool.execute(json!({ "task_id": "t-1" })).await.unwrap();
        assert!(
            result.is_error,
            "a board read failure must be a refusal, not a fabricated missing-card error"
        );
        let text = result.output_for_llm(true);
        assert!(
            !text.contains("No card `t-1`"),
            "must not claim the card doesn't exist when the board couldn't be read: {text}"
        );
        assert!(text.contains("Couldn't read the task board"), "{text}");
    }

    #[tokio::test]
    async fn query_company_board_section_reports_unavailable_on_a_read_failure_not_empty() {
        let tasks: Arc<dyn TaskStore> = Arc::new(BrokenTaskStore);
        let tool =
            QueryCompanyTool::new(CompanyId::new("acme"), None, None, None, None, Some(tasks));
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.is_error, "the whole tool must still answer");
        let text = result.output_for_llm(true);
        assert!(
            !text.contains("No open cards"),
            "must not claim the board is empty when it could not be read: {text}"
        );
        assert!(text.contains("Board unavailable"), "{text}");

        let payload = match &result.content[0] {
            openhuman_core::openhuman::skills::types::ToolContent::Json { data } => data.clone(),
            other => panic!("expected a JSON content block, got {other:?}"),
        };
        assert_eq!(
            payload["board_open"], 0,
            "board_open must stay at zero on a read failure, not report a fabricated count: \
             {payload}"
        );
    }

    #[tokio::test]
    async fn read_task_falls_back_to_the_output_stamp_when_the_artifact_store_is_wired_but_empty() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(crate::store::FsOps::new(dir.path()));
        let tasks: Arc<dyn TaskStore> = fs.clone();
        let artifacts: Arc<dyn ArtifactStore> = fs;
        let company = CompanyId::new("acme");

        let mut card = task_card(
            "t-1",
            "Reply to the customer",
            crate::ports::tasks::COLUMN_IN_REVIEW,
            "engineer",
        );
        card.output = Some(TaskOutput {
            source: crate::ports::tasks::TaskOutputSource::Run {
                run_id: "r-9".to_string(),
                attempt: Some(3),
            },
            at_millis: 5,
            artifacts: Vec::new(),
            workflows: Vec::new(),
        });
        tasks.upsert(&company, &card).await.unwrap();

        let tool = ReadTaskTool::new(company, Some(tasks), None, Some(artifacts));
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(
            out.contains("run `r-9`"),
            "an artifact store wired but empty must still surface the card's own output \
             stamp instead of claiming nothing published: {out}"
        );
        assert!(out.contains("attempt 3)"), "{out}");
        assert!(
            !out.contains("Nothing published yet"),
            "must not claim nothing happened when the card recorded an attempt: {out}"
        );
    }

    /// A run store that cannot answer, so a run-history read failure never
    /// collapses into "no attempts" or a missing run — the same distinction
    /// `list_tasks`/`read_task`'s board read already makes for [`TaskStore`].
    struct BrokenRunStore;

    #[async_trait]
    impl RunStore for BrokenRunStore {
        async fn create_run(
            &self,
            _company: &CompanyId,
            _spec: crate::ports::runs::NewRun,
        ) -> crate::Result<RunRecord> {
            unimplemented!("not exercised by these tests")
        }
        async fn get_run(
            &self,
            _company: &CompanyId,
            _id: &str,
        ) -> crate::Result<Option<RunRecord>> {
            Err(OpenCompanyError::Store(
                "simulated run-store read failure".into(),
            ))
        }
        async fn put_run(&self, _company: &CompanyId, _run: &RunRecord) -> crate::Result<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn list_runs(
            &self,
            _company: &CompanyId,
            _filter: &RunFilter,
        ) -> crate::Result<Vec<RunRecord>> {
            Err(OpenCompanyError::Store(
                "simulated run-history read failure".into(),
            ))
        }
        async fn append_run_step(
            &self,
            _company: &CompanyId,
            _step: &crate::ports::runs::RunStepRecord,
        ) -> crate::Result<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn list_run_steps(
            &self,
            _company: &CompanyId,
            _run_id: &str,
        ) -> crate::Result<Vec<crate::ports::runs::RunStepRecord>> {
            unimplemented!("not exercised by these tests")
        }
    }

    #[tokio::test]
    async fn read_task_reports_run_history_unavailable_instead_of_no_attempts_on_a_read_failure() {
        let dir = tempfile::tempdir().unwrap();
        let tasks: Arc<dyn TaskStore> = Arc::new(crate::store::FsOps::new(dir.path()));
        let company = CompanyId::new("acme");
        tasks
            .upsert(
                &company,
                &task_card(
                    "t-1",
                    "Investigate the outage",
                    crate::ports::tasks::COLUMN_IN_REVIEW,
                    "engineer",
                ),
            )
            .await
            .unwrap();

        let runs: Arc<dyn RunStore> = Arc::new(BrokenRunStore);
        let tool = ReadTaskTool::new(company, Some(tasks), Some(runs), None);
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(
            !out.contains("No attempts yet"),
            "a run-history read failure must not look like a card nobody attempted: {out}"
        );
        assert!(out.contains("Run history unavailable"), "{out}");
    }

    #[tokio::test]
    async fn list_tasks_reports_attempt_status_unavailable_on_a_run_history_read_failure() {
        let dir = tempfile::tempdir().unwrap();
        let tasks: Arc<dyn TaskStore> = Arc::new(crate::store::FsOps::new(dir.path()));
        let company = CompanyId::new("acme");
        tasks
            .upsert(
                &company,
                &task_card(
                    "t-1",
                    "Draft the memo",
                    crate::ports::tasks::COLUMN_TODO,
                    "maya",
                ),
            )
            .await
            .unwrap();

        let runs: Arc<dyn RunStore> = Arc::new(BrokenRunStore);
        let tool = ListTasksTool::new(company, Some(tasks), Some(runs));
        let out = tool.execute(json!({})).await.unwrap().output_for_llm(true);

        assert!(
            out.contains("attempt status unavailable"),
            "a per-card run-history read failure must not render identically to a card with \
             no attempt clause at all: {out}"
        );
    }

    /// A run store that answers `get_run` but never `list_runs`, to isolate
    /// [`ReadRunTool`]'s agent-attempt lookup from its journal fallback.
    struct FailingGetRun;

    #[async_trait]
    impl RunStore for FailingGetRun {
        async fn create_run(
            &self,
            _company: &CompanyId,
            _spec: crate::ports::runs::NewRun,
        ) -> crate::Result<RunRecord> {
            unimplemented!("not exercised by these tests")
        }
        async fn get_run(
            &self,
            _company: &CompanyId,
            _id: &str,
        ) -> crate::Result<Option<RunRecord>> {
            Err(OpenCompanyError::Store(
                "simulated run-store read failure".into(),
            ))
        }
        async fn put_run(&self, _company: &CompanyId, _run: &RunRecord) -> crate::Result<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn list_runs(
            &self,
            _company: &CompanyId,
            _filter: &RunFilter,
        ) -> crate::Result<Vec<RunRecord>> {
            unimplemented!("not exercised by these tests")
        }
        async fn append_run_step(
            &self,
            _company: &CompanyId,
            _step: &crate::ports::runs::RunStepRecord,
        ) -> crate::Result<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn list_run_steps(
            &self,
            _company: &CompanyId,
            _run_id: &str,
        ) -> crate::Result<Vec<crate::ports::runs::RunStepRecord>> {
            unimplemented!("not exercised by these tests")
        }
    }

    #[tokio::test]
    async fn read_run_reports_a_run_store_failure_instead_of_a_missing_run() {
        let runs: Arc<dyn RunStore> = Arc::new(FailingGetRun);
        let tool = ReadRunTool::new(CompanyId::new("acme"), Some(runs), None);
        let result = tool.execute(json!({ "run_id": "r-1" })).await.unwrap();
        assert!(
            result.is_error,
            "a run-store read failure must be a refusal, not a fabricated miss"
        );
        let text = result.output_for_llm(true);
        assert!(
            !text.contains("No run"),
            "must not claim the run doesn't exist when the run store couldn't be read: {text}"
        );
    }

    /// An event log that always fails `read_from`, to prove
    /// [`ReadRunTool`]'s workflow-run fallback distinguishes a journal read
    /// failure from a genuinely absent run.
    struct BrokenEventLog;

    #[async_trait]
    impl EventLog for BrokenEventLog {
        async fn append(&self, _id: &CompanyId, _event: CompanyEvent) -> crate::Result<EventSeq> {
            unreachable!("read_run only reads")
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> crate::Result<Vec<crate::ports::types::StoredEvent>> {
            Err(OpenCompanyError::Store(
                "simulated event-log read failure".into(),
            ))
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> futures::stream::BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(futures::stream::empty())
        }
    }

    #[tokio::test]
    async fn read_run_reports_an_event_log_failure_instead_of_a_missing_run() {
        let events: Arc<dyn EventLog> = Arc::new(BrokenEventLog);
        let tool = ReadRunTool::new(CompanyId::new("acme"), None, Some(events));
        let result = tool.execute(json!({ "run_id": "wf-1" })).await.unwrap();
        assert!(
            result.is_error,
            "an event-log read failure must be a refusal, not a fabricated miss"
        );
        let text = result.output_for_llm(true);
        assert!(
            !text.contains("not an agent attempt and not a workflow run"),
            "must not claim the run doesn't exist when the event log couldn't be read: {text}"
        );
    }

    /// An artifact store that cannot answer, so an output-surface read
    /// failure never collapses into "nothing published".
    struct BrokenArtifactStore;

    #[async_trait]
    impl ArtifactStore for BrokenArtifactStore {
        async fn list(
            &self,
            _company: &CompanyId,
            _task_id: Option<&str>,
        ) -> crate::Result<Vec<crate::ports::artifacts::ArtifactRecord>> {
            Err(OpenCompanyError::Store(
                "simulated artifact-store read failure".into(),
            ))
        }
        async fn get(
            &self,
            _company: &CompanyId,
            _id: &str,
        ) -> crate::Result<Option<crate::ports::artifacts::ArtifactRecord>> {
            unimplemented!("not exercised by these tests")
        }
        async fn upsert(
            &self,
            _company: &CompanyId,
            _artifact: &crate::ports::artifacts::ArtifactRecord,
        ) -> crate::Result<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn delete(&self, _company: &CompanyId, _id: &str) -> crate::Result<bool> {
            unimplemented!("not exercised by these tests")
        }
    }

    #[tokio::test]
    async fn read_task_reports_output_unavailable_on_an_artifact_read_failure() {
        let dir = tempfile::tempdir().unwrap();
        let tasks: Arc<dyn TaskStore> = Arc::new(crate::store::FsOps::new(dir.path()));
        let company = CompanyId::new("acme");
        tasks
            .upsert(
                &company,
                &task_card(
                    "t-1",
                    "Reply to the customer",
                    crate::ports::tasks::COLUMN_IN_REVIEW,
                    "engineer",
                ),
            )
            .await
            .unwrap();

        let artifacts: Arc<dyn ArtifactStore> = Arc::new(BrokenArtifactStore);
        let tool = ReadTaskTool::new(company, Some(tasks), None, Some(artifacts));
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(
            !out.contains("Nothing published yet"),
            "an artifact-store read failure must not look like a genuinely empty store: {out}"
        );
    }

    #[tokio::test]
    async fn read_task_includes_each_attempts_run_id_so_read_run_is_reachable() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(crate::store::FsOps::new(dir.path()));
        let tasks: Arc<dyn TaskStore> = fs.clone();
        let runs: Arc<dyn RunStore> = fs;
        let company = CompanyId::new("acme");
        tasks
            .upsert(
                &company,
                &task_card(
                    "t-1",
                    "Investigate the outage",
                    crate::ports::tasks::COLUMN_IN_REVIEW,
                    "engineer",
                ),
            )
            .await
            .unwrap();
        let mut run = runs
            .create_run(
                &company,
                crate::ports::runs::NewRun::for_task("r-1", "t-1", "engineer"),
            )
            .await
            .unwrap();
        run.status = RunStatus::Failed;
        run.error = Some("timed out".to_string());
        runs.put_run(&company, &run).await.unwrap();

        let tool = ReadTaskTool::new(company, Some(tasks), Some(runs), None);
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(
            out.contains("r-1"),
            "an attempt's run id must be discoverable from read_task, since read_run requires \
             it: {out}"
        );
    }

    #[tokio::test]
    async fn read_task_bounds_rendered_attempts_so_output_cannot_be_pushed_out_of_budget() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(crate::store::FsOps::new(dir.path()));
        let tasks: Arc<dyn TaskStore> = fs.clone();
        let runs: Arc<dyn RunStore> = fs;
        let company = CompanyId::new("acme");
        tasks
            .upsert(
                &company,
                &task_card(
                    "t-1",
                    "Flaky deploy",
                    crate::ports::tasks::COLUMN_IN_REVIEW,
                    "engineer",
                ),
            )
            .await
            .unwrap();
        for n in 1..=(READ_TASK_ATTEMPTS_LIMIT + 3) {
            let mut run = runs
                .create_run(
                    &company,
                    crate::ports::runs::NewRun::for_task(format!("r-{n}"), "t-1", "engineer"),
                )
                .await
                .unwrap();
            run.status = RunStatus::Failed;
            run.error = Some("boom".to_string());
            runs.put_run(&company, &run).await.unwrap();
        }

        let tool = ReadTaskTool::new(company, Some(tasks), Some(runs), None);
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(
            out.contains("3 earlier attempt(s) omitted"),
            "must report how many older attempts were cut: {out}"
        );
        let output_at = out.find("## Output").expect("Output section present");
        let attempts_at = out.find("## Attempts").expect("Attempts section present");
        assert!(
            output_at > attempts_at,
            "the Output section must still be reachable after a long attempt history: {out}"
        );
    }

    #[tokio::test]
    async fn read_task_bounds_the_rendered_title_so_attempts_and_output_stay_reachable() {
        let dir = tempfile::tempdir().unwrap();
        let tasks: Arc<dyn TaskStore> = Arc::new(crate::store::FsOps::new(dir.path()));
        let company = CompanyId::new("acme");
        let long_title = "x".repeat(5_000);
        tasks
            .upsert(
                &company,
                &task_card(
                    "t-1",
                    &long_title,
                    crate::ports::tasks::COLUMN_IN_REVIEW,
                    "engineer",
                ),
            )
            .await
            .unwrap();

        let tool = ReadTaskTool::new(company, Some(tasks), None, None);
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        let header_line = out.lines().next().expect("header line present");
        assert!(
            header_line.chars().count() <= READ_TASK_TITLE_LIMIT + 2,
            "an operator-pasted title must not render verbatim and unbounded, or it can \
             consume the whole tool-result budget before later sections: {} chars",
            header_line.chars().count()
        );
        let output_at = out.find("## Output").expect("Output section present");
        let attempts_at = out.find("## Attempts").expect("Attempts section present");
        assert!(
            output_at > attempts_at,
            "the Output section must stay reachable behind a very long card title: {} bytes total",
            out.len()
        );
    }

    #[tokio::test]
    async fn read_task_resolves_the_pinned_artifact_version_not_a_later_operator_edit() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(crate::store::FsOps::new(dir.path()));
        let tasks: Arc<dyn TaskStore> = fs.clone();
        let artifacts: Arc<dyn ArtifactStore> = fs;
        let company = CompanyId::new("acme");

        let mut card = task_card(
            "t-1",
            "Draft the memo",
            crate::ports::tasks::COLUMN_IN_REVIEW,
            "engineer",
        );
        card.output = Some(TaskOutput {
            source: crate::ports::tasks::TaskOutputSource::Run {
                run_id: "r-1".to_string(),
                attempt: Some(1),
            },
            at_millis: 5,
            artifacts: vec![crate::ports::tasks::TaskOutputArtifact {
                artifact_id: "art-1".to_string(),
                version: 1,
                title: "Memo".to_string(),
                kind: crate::ports::artifacts::ArtifactKind::Markdown,
            }],
            workflows: Vec::new(),
        });
        tasks.upsert(&company, &card).await.unwrap();

        let mut record = crate::ports::artifacts::ArtifactRecord::new(
            "art-1",
            "t-1",
            "Memo",
            crate::ports::artifacts::ArtifactKind::Markdown,
            "the agent's draft body",
            "engineer",
            5,
        );
        record.push_version(
            "an operator edited this after the attempt settled",
            crate::ports::artifacts::ArtifactAuthor::Operator,
            "operator",
            10,
            None,
        );
        artifacts.upsert(&company, &record).await.unwrap();

        let tool = ReadTaskTool::new(company, Some(tasks), None, Some(artifacts));
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(
            out.contains("the agent's draft body"),
            "must render the version the task's output pinned, not the latest: {out}"
        );
        assert!(
            !out.contains("an operator edited this"),
            "a later operator edit must not render as what the task produced: {out}"
        );
    }

    #[tokio::test]
    async fn read_task_only_renders_artifacts_pinned_by_the_current_output_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(crate::store::FsOps::new(dir.path()));
        let tasks: Arc<dyn TaskStore> = fs.clone();
        let artifacts: Arc<dyn ArtifactStore> = fs;
        let company = CompanyId::new("acme");

        let mut card = task_card(
            "t-1",
            "Draft the memo",
            crate::ports::tasks::COLUMN_IN_REVIEW,
            "engineer",
        );
        card.output = Some(TaskOutput {
            source: crate::ports::tasks::TaskOutputSource::Run {
                run_id: "r-2".to_string(),
                attempt: Some(2),
            },
            at_millis: 10,
            artifacts: vec![crate::ports::tasks::TaskOutputArtifact {
                artifact_id: "art-b".to_string(),
                version: 1,
                title: "Follow-up".to_string(),
                kind: crate::ports::artifacts::ArtifactKind::Markdown,
            }],
            workflows: Vec::new(),
        });
        tasks.upsert(&company, &card).await.unwrap();

        let record_a = crate::ports::artifacts::ArtifactRecord::new(
            "art-a",
            "t-1",
            "First draft",
            crate::ports::artifacts::ArtifactKind::Markdown,
            "attempt 1's body — superseded, no longer part of the latest output",
            "engineer",
            5,
        );
        artifacts.upsert(&company, &record_a).await.unwrap();
        let record_b = crate::ports::artifacts::ArtifactRecord::new(
            "art-b",
            "t-1",
            "Follow-up",
            crate::ports::artifacts::ArtifactKind::Markdown,
            "attempt 2's body",
            "engineer",
            10,
        );
        artifacts.upsert(&company, &record_b).await.unwrap();

        let tool = ReadTaskTool::new(company, Some(tasks), None, Some(artifacts));
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(
            out.contains("attempt 2's body"),
            "the artifact pinned by the current output stamp must render: {out}"
        );
        assert!(
            !out.contains("First draft") && !out.contains("superseded"),
            "an artifact from an earlier attempt that the current output stamp does not pin \
             must not render as part of the latest output: {out}"
        );
    }

    #[tokio::test]
    async fn read_task_treats_an_empty_output_stamp_as_the_latest_attempt_publishing_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(crate::store::FsOps::new(dir.path()));
        let tasks: Arc<dyn TaskStore> = fs.clone();
        let artifacts: Arc<dyn ArtifactStore> = fs;
        let company = CompanyId::new("acme");

        let mut card = task_card(
            "t-1",
            "Draft the memo",
            crate::ports::tasks::COLUMN_IN_REVIEW,
            "engineer",
        );
        card.output = Some(TaskOutput {
            source: crate::ports::tasks::TaskOutputSource::Run {
                run_id: "r-2".to_string(),
                attempt: Some(2),
            },
            at_millis: 10,
            artifacts: Vec::new(),
            workflows: Vec::new(),
        });
        tasks.upsert(&company, &card).await.unwrap();

        let record_a = crate::ports::artifacts::ArtifactRecord::new(
            "art-a",
            "t-1",
            "First draft",
            crate::ports::artifacts::ArtifactKind::Markdown,
            "attempt 1's body — attempt 2 published nothing",
            "engineer",
            5,
        );
        artifacts.upsert(&company, &record_a).await.unwrap();

        let tool = ReadTaskTool::new(company, Some(tasks), None, Some(artifacts));
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(
            !out.contains("First draft") && !out.contains("attempt 1's body"),
            "an earlier attempt's artifact must not render as the latest attempt's output when \
             the current output stamp pins an empty (non-absent) artifact list: {out}"
        );
        assert!(
            out.contains("No artifacts published"),
            "an empty-but-present output stamp must render as the latest attempt publishing \
             nothing, not fall through to the all-artifacts legacy fallback: {out}"
        );
    }

    #[tokio::test]
    async fn read_task_renders_workflows_recorded_in_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let tasks: Arc<dyn TaskStore> = Arc::new(crate::store::FsOps::new(dir.path()));
        let company = CompanyId::new("acme");

        let mut card = task_card(
            "t-1",
            "Automate the weekly report",
            crate::ports::tasks::COLUMN_IN_REVIEW,
            "orchestrator",
        );
        card.output = Some(TaskOutput {
            source: crate::ports::tasks::TaskOutputSource::Run {
                run_id: "r-1".to_string(),
                attempt: Some(1),
            },
            at_millis: 5,
            artifacts: Vec::new(),
            workflows: vec![TaskOutputWorkflow {
                workflow_id: "wf-weekly-report".to_string(),
                run_id: Some("wf-run-1".to_string()),
                action: TaskOutputAction::Ran,
            }],
        });
        tasks.upsert(&company, &card).await.unwrap();

        let tool = ReadTaskTool::new(company, Some(tasks), None, None);
        let out = tool
            .execute(json!({ "task_id": "t-1" }))
            .await
            .unwrap()
            .output_for_llm(true);

        assert!(out.contains("### Workflows"), "{out}");
        assert!(out.contains("wf-weekly-report"), "{out}");
        assert!(
            out.contains("wf-run-1"),
            "the workflow's run id must be surfaced for read_run: {out}"
        );
    }
}
