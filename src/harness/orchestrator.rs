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

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::ports::store::company_write_lock;

use async_trait::async_trait;
use serde_json::{Value, json};

use openhuman_core::openhuman as oh;

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};

use crate::company::{
    Agent as ManifestAgent, RawEdge, RawNode, RawWorkflow, WorkflowFile, create_company_workflow,
    list_workflows_union, load_workflow_union,
};
use crate::error::OpenCompanyError;
use crate::harness::lifecycle::ReviewDecision;
use crate::harness::workflow_refs::WorkflowRefQueue;
use crate::ports::events::EventLog;
use crate::ports::facts::FactStore;
use crate::ports::tasks::{TaskOutputAction, TaskOutputWorkflow};
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq, OverlayAgent};
use crate::ports::{CompanyStore, WorkflowRun, WorkflowRunner, generate_id};

/// The manifest cognition-tier that marks the orchestrator agent.
pub const ORCHESTRATOR_TIER: &str = "orchestrator";

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

/// The `query_company` tool name.
pub const QUERY_COMPANY_TOOL: &str = "query_company";
// The `spawn_task` / `delegate_to_desk` names are the brain-agnostic canonical
// constants (issue #176) — re-exported here so the harness path and the hosted
// path share one definition and cannot drift.
use crate::runtime::delegation_tools;
pub use crate::runtime::delegation_tools::{DELEGATE_TO_DESK_TOOL, SPAWN_TASK_TOOL};
/// The `run_workflow` tool name (issue #67).
pub const RUN_WORKFLOW_TOOL: &str = "run_workflow";
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
pub fn orchestrator_id(agents: &[ManifestAgent]) -> Option<String> {
    agents
        .iter()
        .find(|a| a.tier.as_deref() == Some(ORCHESTRATOR_TIER))
        .or_else(|| agents.first())
        .map(|a| a.id.clone())
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
pub fn orchestrator_brief() -> String {
    " You are also this company's orchestrator: the single point of contact for the operator. \
Answer from whole-company context. Two decisions come up constantly and they are INDEPENDENT — do \
not collapse them into one. (1) WHO SHOULD DO THIS: when a request belongs to a specialist desk, \
hand it to that desk with `delegate_to_desk` rather than answering from your own guess; when it is \
yours to answer, answer it. (2) SHOULD THIS BE TRACKED: you do not have to decide this, and you \
must not pick a tool in order to influence it. Anything substantial handed to a desk is opened as a \
board card automatically, and so is anything substantial an operator asks a desk or teammate \
directly — the hand-off IS the card, so never call `spawn_task` alongside a `delegate_to_desk` for \
the same work, and never prefer one over the other to get something tracked. Reach for `spawn_task` \
only for work that belongs on the board but must NOT start in this turn: something for later, for \
somebody else, or waiting on a person. Use `query_company` to ground answers in the \
company's durable facts, recent activity, saved workflows, team roster, and desks — it is the \
source of truth for what workflows exist, who is on the team, and which desks can take work, so \
consult it before answering \"what workflows/teammates do we have?\" or before delegating, rather \
than guessing or naming a skill \
— `delegate_to_desk` to hand a turn to a desk's lead \
member, naming the desk by an id `query_company` lists under Desks (a desk is not a person: \
handing work to a teammate's name is not a delegation), \
`spawn_task` to open a card for work that should wait rather than start now, `run_workflow` to execute one of the \
company's saved workflows by id (for example to advance or finish a task that is waiting on a \
workflow run) — you can run workflows yourself; never claim the run_workflow tool is unavailable — \
`create_workflow` to author and save a brand-new workflow graph (a trigger plus agent / tool / \
condition / output steps) when a repeatable process is worth capturing — it's enabled immediately \
and runnable with run_workflow — and `add_agent` to bring on a new teammate (a name, role, and \
optional mandate) when the company genuinely needs one — it becomes a real, addressable member of \
the team starting next turn. \
You also own the board's lifecycle: `assign_task` to set or change who owns an existing card (this \
records ownership only — moving the card to In Progress is what starts the work), and \
`review_task` to record your verdict on a card awaiting review, either `approve` when the work is \
accepted or `revise` to send it back to To-do for another pass. \
Delegate, run or create a workflow, add a teammate, or act on the board only when it genuinely \
helps — otherwise answer directly and concisely."
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
    /// Whether some drain site has promised to drain what is staged here
    /// (issue #453). `false` — nothing drains — is the default and the
    /// fail-safe direction.
    committed: Arc<Mutex<bool>>,
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

    /// Whether a drain site has committed to draining this queue (issue #453).
    pub fn drain_committed(&self) -> bool {
        *self.committed.lock().expect("delegation commitment")
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
        self.clear();
        *self.committed.lock().expect("delegation commitment") = true;
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
        if !self.drain_committed() {
            return Staged::NoDrain;
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
    /// Nothing has claimed the queue, so nothing would ever execute it.
    NoDrain,
    /// This turn has already queued [`MAX_DELEGATIONS_PER_TURN`].
    OverCap,
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
        *self.queue.committed.lock().expect("delegation commitment") = false;
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
        for event in stored.iter().rev() {
            if recent.len() == RECENT_EVENTS {
                break;
            }
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
        recent.reverse(); // back to chronological order
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
            for fact in facts.iter().take(FACT_LIMIT) {
                md.push_str(&format!(
                    "- **{}**: {}\n",
                    fact.title.trim(),
                    fact.body.trim()
                ));
            }
            // Issue #410, the same silent-cut class one tool over: this list was
            // capped at FACT_LIMIT with no marker, so a company past twenty facts
            // handed the orchestrator a partial memory that read as complete —
            // and the narrowing argument that would have fixed it (`query`) was
            // never mentioned at the point the cut happened.
            if facts.len() > FACT_LIMIT {
                md.push_str(&format!(
                    "\n[TRUNCATED — {} more fact(s) not shown. This is NOT the whole record. \
                     Narrow it with `{QUERY_COMPANY_TOOL}({{\"query\": \"<substring>\"}})` before \
                     concluding a fact is absent.]\n",
                    facts.len() - FACT_LIMIT
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
                "workflows": workflows.len(),
                "team": roster.len(),
                "desks": desks.len(),
            }),
            md,
        ))
    }
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
        CompanyEvent::WorkflowNodeFinished {
            workflow_id,
            node_id,
            ..
        } => format!("workflow {workflow_id} finished node {node_id}"),
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
            Staged::NoDrain => return Ok(ToolResult::error(no_drain(SPAWN_TASK_TOOL, &effect))),
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
            Staged::NoDrain => {
                return Ok(ToolResult::error(no_drain(DELEGATE_TO_DESK_TOOL, &effect)));
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
            Staged::NoDrain => return Ok(ToolResult::error(no_drain(ASSIGN_TASK_TOOL, &effect))),
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
            Staged::NoDrain => return Ok(ToolResult::error(no_drain(REVIEW_TASK_TOOL, &effect))),
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
/// would queue (issue #453) — modelled on
/// [`cannot_publish_here`](crate::harness::publish) one module over.
///
/// It has to do two jobs, and they are not the two [`over_cap`] does. It must
/// not read as a transient condition worth retrying — every turn on an unclaimed
/// path fails identically — and it must tell the agent what to say next, because
/// the failure this replaces was one the agent could not detect: it was told the
/// card had moved, so it told the operator the card had moved, and the next
/// turn's `clear()` threw the delegation away.
fn no_drain(tool: &str, effect: &str) -> String {
    tracing::warn!(
        tool = %tool,
        "[delegation] a delegation tool was called from a turn with no claimed drain; refusing \
         rather than queuing into a queue nothing will drain"
    );
    format!(
        "Refused: nothing here can carry out board work, so {effect}. Board actions are \
         unavailable in this context. Do not retry — it will fail the same way — and do NOT report \
         the action as done or describe the card as moved. Say plainly that you could not do it."
    )
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
}

impl AddAgentTool {
    /// Builds the tool over the company id and its store handle
    /// ([`HarnessDeps::store`](crate::harness::HarnessDeps::store)).
    pub fn new(company: CompanyId, store: Arc<dyn CompanyStore>) -> Self {
        Self { company, store }
    }
}

#[async_trait]
impl Tool for AddAgentTool {
    fn name(&self) -> &str {
        ADD_AGENT_TOOL
    }

    fn description(&self) -> &str {
        "Add a new teammate to the company. Provide a `name`, a `role` (job title), and an optional `description` of their mandate. The teammate becomes a real, addressable member of the roster starting next turn."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The new teammate's display name." },
                "role": { "type": "string", "description": "The new teammate's job title." },
                "description": { "type": "string", "description": "An optional description of the teammate's mandate." }
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
        // collision with a manifest agent is handled by `build_roster`.
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
        let agent = OverlayAgent {
            id: generate_id(),
            name: name.clone(),
            role: role.clone(),
            description,
        };
        record.overlay_agents.push(agent);
        self.store.save(&record).await?;

        Ok(ToolResult::success(format!(
            "Added {name} as {role} to the team. They'll be reachable as a teammate starting next turn."
        )))
    }
}

// ---------------------------------------------------------------------------
// orchestrator_tools — the complete tool set (issues #53, #67, #71)
// ---------------------------------------------------------------------------

/// The complete tool set wired onto the company's orchestrator agent (issues
/// #53, #67, #71, and #112), in order: the `query_company` read surface, the
/// `spawn_task` and `delegate_to_desk` delegation tools, the `run_workflow`
/// execution tool, the `create_workflow` authoring tool, and the `add_agent`
/// roster-write tool.
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
    tools.push(Box::new(AddAgentTool::new(company, store)));
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
}

impl RunWorkflowTool {
    /// Builds the tool over the company id, its on-disk source directory
    /// (`companies/<name>`, whose `workflows/` subtree holds the seed graphs),
    /// the company store (holding the runtime-authored graph bodies), the
    /// shared runner handle, the company's journal, and the shared queue a
    /// dispatched card's output link is staged on (issue #339).
    pub fn new(
        company: CompanyId,
        source_dir: Option<PathBuf>,
        store: Arc<dyn CompanyStore>,
        runner: WorkflowRunnerHandle,
        run_supervisor: crate::runtime::RunSupervisor,
        events: Option<Arc<dyn EventLog>>,
        workflow_refs: WorkflowRefQueue,
    ) -> Self {
        Self {
            company,
            source_dir,
            store,
            runner,
            run_supervisor,
            events,
            workflow_refs,
        }
    }
}

#[async_trait]
impl Tool for RunWorkflowTool {
    fn name(&self) -> &str {
        RUN_WORKFLOW_TOOL
    }

    fn description(&self) -> &str {
        "Run one of the company's saved workflows by id to completion — use this to advance or finish work that is waiting on a workflow run. Provide the workflow `id` and an optional `input` trigger payload. Returns a summary of each node's outcome and any steps left pending approval."
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
        let (ctx, _run_guard) = self.run_supervisor.begin(&wid, false);
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
                let md = summarize_run(&file, &run);
                Ok(ToolResult::success_with_markdown(
                    json!({
                        "workflow": file.id,
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
fn summarize_run(file: &WorkflowFile, run: &WorkflowRun) -> String {
    let mut md = format!("Ran workflow **{}** (`{}`).\n\n", file.name.trim(), file.id);
    md.push_str("## Per-node outcome\n");
    let nodes = run.output.get("nodes").and_then(Value::as_object);
    match nodes {
        Some(nodes) if !file.nodes.is_empty() => {
            for node in &file.nodes {
                let name = node.name.trim();
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
                            Some(preview) => md.push_str(&format!(
                                "- **{name}** ({kind}): {count} item(s) — {preview}\n"
                            )),
                            None => {
                                md.push_str(&format!("- **{name}** ({kind}): {count} item(s)\n"))
                            }
                        }
                    }
                    None => md.push_str(&format!("- **{name}** ({kind}): not reached\n")),
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
    md
}

/// A short, single-line preview of one node output item: the raw string when the
/// item is a string, else its compact JSON — truncated on a char boundary to
/// [`ITEM_PREVIEW_CHARS`] so a large item can't blow up the summary.
fn preview_item(item: &Value) -> String {
    let raw = match item {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= ITEM_PREVIEW_CHARS {
        one_line
    } else {
        let cut: String = one_line.chars().take(ITEM_PREVIEW_CHARS).collect();
        format!("{cut}…")
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
/// **This is deliberately a narrower surface than the REST body**, and the node
/// shape below is where that shows: it accepts only `id`/`kind`/`name`/`summary`
/// /`agent`, omitting `config`, `onError`, `retry`, `requiresApproval` — and
/// `schedule` (issue #169). The omission is the policy, not an oversight:
///
/// * A field the model cannot set is a field it cannot get wrong. Each of these
///   carries real consequence — retry/error policy changes failure behavior, and
///   a `schedule` makes a workflow run *on its own, forever*, with no operator
///   in the loop at the moment it fires.
/// * So **agent-authored workflows are manual-run only**. Schedules are
///   operator-authored, through the console's creator or `POST …/workflows`,
///   where a human chose the cron. An agent can build the graph; a human decides
///   whether it runs unattended.
///
/// Whether agents should be able to schedule themselves is an open product
/// question. If the answer becomes yes, add the field here and to
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

impl From<CreateWorkflowArgs> for RawWorkflow {
    fn from(args: CreateWorkflowArgs) -> Self {
        Self {
            id: args.id,
            name: args.name,
            description: args.description,
            nodes: args
                .nodes
                .into_iter()
                .map(|n| RawNode {
                    id: n.id,
                    kind: n.kind,
                    name: n.name,
                    summary: n.summary,
                    agent: n.agent,
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
                })
                .collect(),
            edges: args
                .edges
                .into_iter()
                .map(|e| RawEdge {
                    from: e.from,
                    to: e.to,
                    label: e.label,
                })
                .collect(),
        }
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
        "Author and save a new workflow graph for the company, then enable it so it can be run with run_workflow. A workflow is a directed graph: exactly one `trigger` node (what starts it) plus any of `agent` (a roster teammate does a step — set `agent` to that teammate's id), `tool_call`, `http_request`, `condition`, and `output` nodes, joined by `edges` ({from, to, optional label}). Node ids must be unique; every `agent` node must name a real teammate. Use this to capture a repeatable process; then run it with run_workflow."
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
                            "agent": { "type": "string", "description": "On an `agent` node only: the roster teammate id that performs the step." }
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
        let draft: RawWorkflow = match serde_json::from_value::<CreateWorkflowArgs>(args) {
            Ok(args) => args.into(),
            Err(err) => {
                tracing::debug!(company = %self.company, error = %err, "create_workflow: unreadable args");
                return Ok(ToolResult::error(format!(
                    "Couldn't read the workflow definition: {err}. Provide `id`, `name`, and `nodes` (with an `edges` list)."
                )));
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
            Staged::NoDrain,
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
            template_provenance: None,
        }
    }

    #[tokio::test]
    async fn add_agent_tool_persists_an_overlay_teammate() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = AddAgentTool::new(company.clone(), store.clone());

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

    #[tokio::test]
    async fn add_agent_tool_requires_name_and_role() {
        let company = CompanyId::new("acme");
        let store = Arc::new(MemStore::seeded(seeded_record(&company)));
        let tool = AddAgentTool::new(company.clone(), store.clone());

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
        let tool = AddAgentTool::new(company, store);

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
    fn orchestrator_tools_includes_all_eight() {
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
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        // Six before #186; `assign_task` + `review_task` make eight.
        assert_eq!(names.len(), 8, "got {names:?}");
        assert!(names.contains(&RUN_WORKFLOW_TOOL), "got {names:?}");
        assert!(names.contains(&CREATE_WORKFLOW_TOOL), "got {names:?}");
        assert!(names.contains(&ADD_AGENT_TOOL), "got {names:?}");
        assert!(names.contains(&QUERY_COMPANY_TOOL), "got {names:?}");
        assert!(names.contains(&SPAWN_TASK_TOOL), "got {names:?}");
        assert!(names.contains(&DELEGATE_TO_DESK_TOOL), "got {names:?}");
        assert!(names.contains(&ASSIGN_TASK_TOOL), "got {names:?}");
        assert!(names.contains(&REVIEW_TASK_TOOL), "got {names:?}");
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
}
