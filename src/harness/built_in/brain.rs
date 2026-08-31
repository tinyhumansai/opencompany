//! [`HarnessBrain`]: the cognition [`Brain`] backed by the embedded OpenHuman
//! runtime.
//!
//! Where [`EchoBrain`](crate::brain::EchoBrain) turns every operator message
//! into `"You said: …"`, `HarnessBrain` routes it to a live openhuman
//! [`Agent`](openhuman_core::openhuman::agent::Agent) through a
//! [`HarnessPool`], so the reply comes from the hosted brain and the turn's
//! token/cost usage is metered into the company ledger.
//!
//! The default chat responder is the company **orchestrator** (issue #53): the
//! roster agent tagged `tier = "orchestrator"`, or the first agent when none is
//! (so a company without an orchestrator behaves exactly as before). An operator
//! message addressed to a desk (its `chat` field) is answered by that desk's
//! lead member; an unaddressed message goes to the orchestrator, which may
//! delegate — the queue its tools fill is drained here after its turn (v1:
//! synchronous, in-cycle, capped, no sub-agent re-delegation).
//!
//! Compiled only under `feature = "openhuman"`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::company::artifact_mirror;
use crate::company::steer::{InflightEntry, InflightKind, SteerAction, SteerControl, cap_redirect};
use crate::harness::build::agent_workspace;
use crate::harness::confine;
// The note shape is shared with the ungated system paths (issue #337): the
// backstop, the quiescing-runtime settle and the boot reaper all append their
// reason to a card, and a second copy of it here is how one card ends up with
// two note formats depending on which path touched it last.
use crate::harness::lifecycle::{self, TaskRunEnd};
use crate::harness::orchestrator;
use crate::harness::policy::ApprovalScope;
use crate::harness::publish::{self, WorkspaceSnapshot};
use crate::runtime::advance::append_result;
// `Delegation` is only named by the test-only `run_delegation` wrapper and the
// delegation tests (via `use super::*`); the cycle path drives the runner's
// `handle_operator_message` and never spells the type out.
#[cfg(test)]
use crate::harness::orchestrator::Delegation;
use crate::harness::run_turn::HarnessRunTurn;
use crate::harness::{HarnessDeps, HarnessPool};
use crate::runtime::assignee;
use crate::runtime::delegation::{self, DelegationRunner, RunTurn};

/// The most operator redirects honored within a single task dispatch (issue
/// #111). A redirect re-runs the turn in-loop with the fresh instruction
/// appended; past this cap the run is finalized to its terminal column (see
/// [`lifecycle::success_terminal_column`]) so a redirect storm can't loop
/// forever.
const MAX_REDIRECTS_PER_DISPATCH: u32 = 3;

/// The `error` a dispatched attempt settles with when the company has no task
/// board wired at all — the card cannot even be read, so nothing was tried.
const NO_TASK_STORE: &str = "this company has no task board wired, so the card could not be run";

/// The `error` a dispatched attempt settles with when its card is gone by the
/// time the cycle reaches it (deleted, or never persisted).
const CARD_VANISHED: &str = "the card was gone by the time its dispatch ran";

/// The system bubble appended when a turn paused at its tool-iteration cap
/// (issue #926).
///
/// Three things it must say, and one it must not:
///
/// - **It paused** — the reply above is a checkpoint, so it reads like a plan
///   the agent simply chose not to carry out. QA reported this as agents
///   "getting permanently stuck mid-task", which is what an unexplained pause
///   looks like from the outside.
/// - **Nothing failed** — a cap is a budget, not an error. Without saying so,
///   the notice reads as a crash report for a turn that worked fine.
/// - **How to carry on** — the operator's move is to reply.
///
/// What it must NOT do is promise the agent resumes where it left off. The
/// pooled `Agent` keeps its history in memory, and `HarnessPool::ensure`
/// rebuilds the agent whenever the roster / skill / MCP fingerprint moves — so
/// "continue" is an instruction to the operator, phrased as a request to the
/// agent, never a durability guarantee this layer cannot make.
pub(crate) const ITERATION_CAP_PAUSE_NOTICE: &str = "\
The reply above is a pause, not a finished answer: this turn reached the maximum number of steps \
it may take for a single reply, so it stopped and wrote up where it had got to. Nothing errored — \
the work so far stands. Reply \"continue\" to ask it to pick up from there.";

/// The system bubble emitted when a turn was halted by its in-turn spend brake
/// (issue #1032).
///
/// The sibling of [`ITERATION_CAP_PAUSE_NOTICE`], and deliberately **not**
/// interchangeable with it. Both say a turn stopped short, but the operator's
/// next move is opposite:
///
/// - a step pause is resumable — the work fits, the turn just ran out of room,
///   so `"continue"` finishes it;
/// - a spend halt is not — the work costs more than the budget allows, and
///   asking again only spends more against the same cap. So this notice must
///   never tell the operator to reply `"continue"`; that would invite them to
///   burn the rest of a budget that had already run out.
///
/// It names the teammate and quotes both figures. The iteration-cap notice
/// deliberately quotes no number, because one bubble can cover a responder, a
/// desk and a relay turn and naming one of *their* caps would be a number the
/// operator cannot map back to anything. Naming the teammate is what removes
/// that objection here: `$X of $Y, by this teammate` is attributable in a way a
/// bare cap is not.
///
/// `spent` can exceed `cap` and the wording allows for it: the brake fires
/// between tool iterations, so the call that crossed the line was already paid
/// for. Reporting the real figure is the honest answer — rounding it down to the
/// cap would hide exactly the overshoot an operator setting a budget wants to
/// see.
pub(crate) fn spend_halt_notice(halt: &crate::harness::SpendHalt) -> String {
    format!(
        "The reply above is where this turn stopped, not a finished answer: {agent} reached its \
         spend cap partway through, so the work was halted before it was done. This turn spent \
         ${spent:.2} against a cap of ${cap:.2}. Nothing errored — the work so far stands, but \
         asking again runs a new turn against the same cap. Raising {agent}'s budget, or narrowing \
         what it was asked to do, is what lets the work finish.",
        agent = halt.agent,
        spent = halt.spent_usd,
        cap = halt.cap_usd,
    )
}

/// The authored bubble's placeholder text when a turn paused for lack of
/// inference budget/credits (issue #1846) — see the override in
/// [`HarnessBrain::handle_operator_message`]'s caller for why this exists
/// instead of the teammate's own words: the model call itself failed, so
/// there is no partial answer to attribute, unlike a step-cap pause or a spend
/// halt.
pub(crate) const BUDGET_PAUSED_PLACEHOLDER_REPLY: &str = "(no reply — see the notice below)";

/// The stable prefix every budget-pause system notice starts with (issue
/// #1846). Kept as a named constant, not just embedded in
/// [`budget_pause_notice`]'s format string, because the console frontend
/// pattern-matches on it (`text.startsWith(...)`) to render this notice
/// distinctly from an ordinary system bubble and offer an "Add credits" CTA —
/// there is no structured wire field for a notice "kind" here (unlike
/// `TurnStepFailure` for a tool result), so the prefix IS the contract. A
/// drift-coupling test on the frontend side should assert against this exact
/// string; keep the two in sync by hand until a structured field exists.
pub(crate) const BUDGET_PAUSE_NOTICE_PREFIX: &str = "⏸ Paused — out of credits:";

/// The system bubble emitted when a turn paused for lack of inference
/// budget/credits (issue #1846) — the sibling of
/// [`ITERATION_CAP_PAUSE_NOTICE`] and [`spend_halt_notice`], and, like both,
/// deliberately unauthored: no teammate said this, the account ran out of
/// money before the model ever replied.
///
/// Unlike either sibling, the operator's only lever here is adding credits —
/// not "continue" (there is no checkpoint to resume) and not "raise the
/// budget / narrow the ask" (the account itself, not a company-declared cap,
/// is exhausted). The prefix is load-bearing: see
/// [`BUDGET_PAUSE_NOTICE_PREFIX`].
pub(crate) fn budget_pause_notice(pause: &crate::harness::BudgetPause) -> String {
    format!("{BUDGET_PAUSE_NOTICE_PREFIX} {}", pause.summary)
}

/// The non-redeemable sibling of [`BUDGET_PAUSE_NOTICE_PREFIX`] (issue #1846
/// review, Codex #3870562586 / #3870562590).
///
/// The console renders its "Add credits & resend" CTA off
/// [`BUDGET_PAUSE_NOTICE_PREFIX`] alone — there is no structured wire field to
/// key on yet, as that constant's own doc says. Two paths pause WITHOUT a
/// marker the generic chat redeem can honour:
///
/// * the **confined workflow copilot** — `run_confined` bypasses `run_inner`,
///   the only place that parks a marker, and `CONFINED_AGENT_ID` names no
///   addressable teammate, so there is nothing to redeem and nothing safe to
///   replay; and
/// * an **approval continuation** — it runs through `run_steered_background`,
///   so `run_inner` parks its marker with `background: true`, a shape
///   `redeem_budget_pause` refuses outright
///   (`src/server/ops/budget_pause.rs`).
///
/// Emitting the redeemable prefix on either put a button on screen that could
/// only ever fail — a 404 for the first (no marker exists), a 400 for the
/// second (the marker exists and is refused). This prefix carries the SAME
/// information and deliberately does not match the console's
/// `isBudgetPauseNotice`, so the notice renders as an ordinary system bubble
/// with no unusable action on it.
///
/// Each arm is pinned where it chooses:
/// `a_confined_copilot_pause_offers_no_redeem_cta` calls `confined_turn_bubble`,
/// and
/// `a_budget_paused_approval_continuation_surfaces_the_notice_and_parks_a_marker`
/// drives a real continuation and reads the bubble it emits. The builder alone
/// is pinned by `the_no_resend_notice_builder_uses_the_non_redeemable_prefix`,
/// which is all it ever pinned — issue #1906 renamed it from
/// `an_approval_continuation_pause_offers_no_redeem_cta`, a name that promised
/// the arm above's coverage for a test that runs no continuation.
pub(crate) const BUDGET_PAUSE_NOTICE_NO_RESEND_PREFIX: &str =
    "⏸ Paused — out of credits (add credits, then start this again):";

/// The unauthored bubble for a budget pause that carries no redeemable marker.
/// See [`BUDGET_PAUSE_NOTICE_NO_RESEND_PREFIX`].
pub(crate) fn budget_pause_notice_no_resend(pause: &crate::harness::BudgetPause) -> String {
    format!("{BUDGET_PAUSE_NOTICE_NO_RESEND_PREFIX} {}", pause.summary)
}

use crate::harness::run_trace::RunTraceSink;
use crate::ports::artifacts::{ArtifactAuthor, ArtifactRecord};
use crate::ports::blockers::{BlockerPayload, BlockerStep};
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::runs::{RunOutcome, RunStatus};
use crate::ports::tasks::{COLUMN_IN_REVIEW, TaskOutput, TaskOutputArtifact, TaskOutputSource};
use crate::ports::types::{
    CompanyEvent, CompanyRecord, CompressedTrace, CycleRequest, CycleResult, Effect, EffectGroup,
    OutboundMessage, TokenUsage, TurnStep, TurnStepKind, TurnStepStatus, Verdict,
};
use crate::ports::{Cognition, TaskRecord, UsageMetering, generate_id, now_millis};

/// A [`Brain`] that answers with a live openhuman agent turn.
pub struct HarnessBrain {
    pool: Arc<HarnessPool>,
    deps: Arc<HarnessDeps>,
    /// Every harness lane beyond the default, by id, and which agents are bound
    /// to which. Empty for a company that declares no `[[harness]]` block —
    /// which is every company that has not asked for one — and in that case
    /// [`run_turn`](Self::run_turn) hands back the default lane directly, so the
    /// single-harness path stays exactly what it was.
    lanes: Vec<(String, Arc<dyn RunTurn>)>,
    /// Agent id -> harness id, for agents bound to a named harness.
    bindings: std::collections::HashMap<String, String>,
    /// Declared harnesses this host cannot run, and why.
    unavailable: Vec<(String, String)>,
    /// The harness id agents naming none run on.
    default_harness: String,
    /// Override for the default harness's engine, from
    /// [`lanes::build`](crate::harness::lanes::build)'s resolution
    /// (issue #1244).
    ///
    /// `None` — before [`Self::with_default_engine`] is ever called — means
    /// "no override yet": [`Self::run_turn`] falls back to building the
    /// embedded `built_in` engine from `pool`/`deps`, lazily, exactly as it
    /// always did before named harnesses existed. That laziness matters: doing
    /// it eagerly here in [`Self::new`] would hold a second `Arc` on `deps` for
    /// the brain's whole lifetime, breaking every test (and any real caller)
    /// that assumes the brain is `deps`'s sole holder before the first turn.
    ///
    /// Once set, `Some(engine)` is authoritative even when `engine` is itself
    /// `None` — "this host cannot run the default harness", with the reason in
    /// `unavailable` — which must win over the lazy built-in fallback, or the
    /// exact silent-fallback bug #1244 fixed comes back.
    default_engine: Option<Option<Arc<dyn RunTurn>>>,
    /// The LLM triage escalation, built on first use (issue #678).
    ///
    /// Lazy because it needs the company id, and a brain outlives any one
    /// record read; `OnceLock` because it is immutable once built and the cost
    /// is a clone of two `Arc`s, not a model call.
    triage: std::sync::OnceLock<crate::harness::triage::MeteredTriage>,
    /// The per-message responder selection for `auto` channels, built on first
    /// use (issue #1835). Lazy and `OnceLock` for exactly [`Self::triage`]'s
    /// reasons — it needs the company id, and once built it is immutable.
    selector: std::sync::OnceLock<crate::harness::selector::MeteredSelector>,
    /// The company's record, **re-read from the store at the top of every
    /// cycle** (issue #707).
    ///
    /// # Why this is not a build-time snapshot any more
    ///
    /// It used to be a plain `CompanyRecord`, assigned once in [`Self::new`] and
    /// never again. Desk chat routing reads it — [`Self::responder_for`] →
    /// [`Self::desk_lead`] → `effective_desk_members` → `overlay_desk_order` —
    /// so an operator who reordered a desk, added a desk member, or created a
    /// desk in the console kept reaching the *old* lead until the process
    /// restarted. Nothing rebuilt the brain in between: the only caller of
    /// [`rebuild_company`](crate::runtime::rebuild_company) is an
    /// inference-settings change.
    ///
    /// It was also a **divergence**, not merely a lag. Every other consumer of
    /// this state already loads per call — `delegate_to_desk` re-reads the
    /// record on each tool call, and the REST desk surfaces re-read per request
    /// — so the console and a delegation card would name the new lead while a
    /// desk chat still routed to the old one. Refreshing here makes chat routing
    /// do what the correct consumers already do, which removes the divergence
    /// rather than adding a second mechanism to paper over it.
    ///
    /// Behind an `RwLock<Arc<…>>` so a reader is a lock-free-ish clone of a
    /// handle rather than a copy of the manifest, and so the guard is never held
    /// across an `await`. Private, and reached only through [`Self::record`]:
    /// there is deliberately no way to read a stale one.
    record: std::sync::RwLock<Arc<CompanyRecord>>,
    responder: String,
    /// The attempt records a dispatched card writes into (issue #242).
    ///
    /// Held here rather than on [`HarnessDeps`] on purpose: every one of the
    /// ~28 `HarnessDeps` literals in this crate would otherwise have to be
    /// widened for a handle only the dispatch path reads — the same argument
    /// that put the grant set on `ApprovalRequestQueue` instead.
    ///
    /// `None` **fails silent, not closed**: the card still runs, its outcome
    /// still lands on the board and in the journal, and only the run record is
    /// missing. That is the right direction for a purely observational store —
    /// and it is why every test construction can leave it unset.
    runs: Option<Arc<dyn crate::ports::RunStore>>,
}

/// A bubble the **runtime** wrote, not an agent (issue #966).
///
/// Two sites emit these on the operator channel: the approval-overflow notice
/// and the cycle's `"Acknowledged."` fallback. Both used to leave the author
/// unset, so the journal writer's `channel` fallback stamped `"operator"` — and
/// that made a *correct* system row byte-identical on disk to a reply whose
/// author the pre-#885 defect had overwritten. No reader could tell them apart,
/// which is the finding recorded on #966.
///
/// Named rather than inlined for the same reason [`confined_bubble`] is: the
/// author is the load-bearing field, and a free function is what lets it be
/// asserted without standing up a cycle.
fn system_notice(text: String) -> OutboundMessage {
    OutboundMessage {
        message_id: None,
        task_id: None,
        channel: "operator".to_string(),
        agent: Some(crate::ports::SYSTEM_AUTHOR.to_string()),
        text,
        steps: Vec::new(),
        reply_to: None,
        mentions: Vec::new(),
    }
}

/// The single bubble a workflow-copilot turn returns (issues #416, #966).
///
/// Named rather than inlined because its **author** is the load-bearing field
/// and it was wrong. #885 taught the main operator bubble to carry its
/// responder and left this branch on `None`, so a genuine copilot reply kept
/// journaling as `agent_id: "operator"` — the #885 defect still happening, not
/// history needing a label. A function is what lets that be asserted without
/// standing up a scripted model endpoint and a whole harness pool.
///
/// `CONFINED_AGENT_ID` is deliberately not a roster id (see [`confine`]): it
/// names no teammate and cannot be addressed. That makes it a **truthful**
/// author rather than a resolvable one, which is why
/// `chat_history::is_known_author` has to know it — otherwise this row trades
/// one wrong answer for a permanent false positive in the attribution audit.
///
/// No card, by construction: a confined turn has no `spawn_task` to call, and
/// the chat handler does not open one from a copilot message either.
fn confined_bubble(outcome: crate::harness::TurnOutcome) -> OutboundMessage {
    OutboundMessage {
        message_id: None,
        task_id: None,
        channel: "operator".to_string(),
        agent: Some(confine::CONFINED_AGENT_ID.to_string()),
        text: outcome.reply,
        reply_to: None,
        mentions: Vec::new(),
        steps: outcome.steps,
    }
}

/// The bubble a confined workflow-copilot turn's outcome becomes on the
/// operator channel — the boundary [`HarnessBrain`]'s copilot arm calls
/// (issue #1846 review, Codex #3869277640).
///
/// A budget pause is a RUNTIME terminal state (issue #1846), the same as the
/// iteration-cap pause and the spend halt the interactive operator-turn path
/// already reports via [`system_notice`] rather than folding into a reply —
/// never something the confined copilot itself said. `run_confined`
/// deliberately bypasses `run_inner` (the only place that parks a redeemable
/// re-issue marker): `CONFINED_AGENT_ID` "names no teammate and cannot be
/// addressed" (see [`confined_bubble`]'s doc), so there is no safe way to
/// replay ITS confined workflow context through the generic chat-message
/// redeem path a marker would offer. Before this, `confined_bubble` folded
/// the placeholder pause text straight into `outcome.reply` and attributed
/// it to the copilot as an ordinary answer — exactly the #966 misattribution
/// class `system_notice` exists to prevent, just one call site short of it.
///
/// Emits the unauthored notice through
/// [`budget_pause_notice_no_resend`] — the honest middle ground between a
/// fabricated copilot reply and a redemption this agent id can never honour.
///
/// Issue #1846 review (Codex #3870562586): this used to emit
/// [`budget_pause_notice`], whose prefix is exactly what the console keys its
/// "Add credits & resend" button off. The doc above already claimed "with no
/// CTA" — but nothing enforced it, so the button rendered anyway and, with no
/// marker ever parked for `CONFINED_AGENT_ID`, every click did a GET that
/// returned `null` followed by a POST that 404'd. The no-resend prefix makes
/// the claim true.
fn confined_turn_bubble(outcome: crate::harness::TurnOutcome) -> OutboundMessage {
    match &outcome.budget_paused {
        Some(pause) => system_notice(budget_pause_notice_no_resend(pause)),
        None => confined_bubble(outcome),
    }
}

impl HarnessBrain {
    /// Builds a harness brain for `record`, answering unaddressed operator
    /// messages with the company orchestrator (the `tier = "orchestrator"` agent,
    /// else the first roster agent). The pool is shared so the roster is built
    /// once and reused across cycles.
    pub fn new(pool: Arc<HarnessPool>, deps: HarnessDeps, record: CompanyRecord) -> Self {
        // Resolved over the roster as it effectively stands: a company whose
        // first declared agent has since been removed still has an orchestrator,
        // and answering as a teammate the harness no longer builds would leave
        // the operator's message unanswered.
        let responder =
            orchestrator::orchestrator_id(&record.effective_agents()).unwrap_or_default();
        let default_harness = record.manifest.default_harness_id();
        // Effective agents plus the overlay roster — the same two halves
        // `lanes::agents_on` folds together. Built from the raw manifest, this
        // map saw neither a console-created teammate nor an admin's harness
        // edit to a blueprint one, so the lane excluded such a teammate from
        // the default pool while this router still dispatched it there. The
        // binding was saved, survived a restart, and did nothing.
        let bindings = record
            .effective_agents()
            .into_iter()
            .filter_map(|a| a.harness.clone().map(|h| (a.id, h)))
            .chain(
                record
                    .overlay_agents
                    .iter()
                    .filter_map(|a| a.harness.clone().map(|h| (a.id.clone(), h))),
            )
            .collect();
        Self {
            pool,
            deps: Arc::new(deps),
            lanes: Vec::new(),
            bindings,
            unavailable: Vec::new(),
            default_harness,
            default_engine: None,
            record: std::sync::RwLock::new(Arc::new(record)),
            responder,
            runs: None,
            triage: std::sync::OnceLock::new(),
            selector: std::sync::OnceLock::new(),
        }
    }

    /// Attaches the harness lanes beyond the default.
    ///
    /// Each entry is a declared harness id and the engine serving it — another
    /// `built_in` pool on its own provider, or an ACP agent. Without this a
    /// brain routes every turn to the default lane, which is correct for a
    /// company that declares no `[[harness]]`.
    pub fn with_lanes(mut self, lanes: Vec<(String, Arc<dyn RunTurn>)>) -> Self {
        self.lanes = lanes;
        self
    }

    /// Records that a declared harness has no engine on this host, and why, so
    /// a turn bound to it fails with something actionable instead of silently
    /// running somewhere nobody chose.
    pub fn with_unavailable_lanes(mut self, unavailable: Vec<(String, String)>) -> Self {
        self.unavailable = unavailable;
        self
    }

    /// Overrides the default harness's engine with
    /// [`lanes::build`](crate::harness::lanes::build)'s actual resolution —
    /// `None` when this host cannot run it, which must win over the lazy
    /// built-in fallback [`Self::run_turn`] otherwise takes (issue #1244).
    /// Pass `lanes.default_engine` straight through; its accompanying
    /// `unavailable` entry, if any, goes through
    /// [`Self::with_unavailable_lanes`].
    pub fn with_default_engine(mut self, default_engine: Option<Arc<dyn RunTurn>>) -> Self {
        self.default_engine = Some(default_engine);
        self
    }

    /// The [`RunTurn`] this brain's turns go through.
    ///
    /// With no extra lanes and a runnable default this is the default engine
    /// alone — byte-identical behaviour to before named harnesses existed, and
    /// no routing table is consulted. Otherwise it is a [`HarnessRouter`] that
    /// sends each agent's turn to the harness it is bound to; a `None` default
    /// engine (from [`Self::with_default_engine`]) routes through it exactly
    /// like any other unavailable harness, rather than falling back to the
    /// lazily-built embedded engine (issue #1244).
    fn run_turn(&self) -> Arc<dyn RunTurn> {
        // No override set — every test, and any pre-#1244 caller — falls back
        // to building the embedded engine fresh, exactly as this always did.
        // Lazy on purpose: building it eagerly in `new` would hold a second
        // `Arc` on `deps` for the brain's whole lifetime.
        let default_engine = self.default_engine.clone().unwrap_or_else(|| {
            Some(
                Arc::new(HarnessRunTurn::new(self.pool.clone(), self.deps.clone()))
                    as Arc<dyn RunTurn>,
            )
        });

        if self.lanes.is_empty()
            && self.unavailable.is_empty()
            && let Some(engine) = &default_engine
        {
            return engine.clone();
        }
        Arc::new(crate::harness::router::HarnessRouter::from_lanes(
            &self.default_harness,
            default_engine,
            &self.lanes,
            &self.unavailable,
            &self.bindings,
        ))
    }

    /// This company's record as of the current cycle's refresh.
    ///
    /// Returns a handle rather than a borrow so no lock is held across the
    /// `await` points every caller here has. Within one cycle every call sees
    /// the same record: the refresh happens once, at the top of
    /// [`run_cycle`](Brain::run_cycle), so a turn cannot observe the operator
    /// changing a desk halfway through its own routing.
    fn record(&self) -> Arc<CompanyRecord> {
        self.record
            .read()
            .expect("harness brain record poisoned")
            .clone()
    }

    /// Edits the record in place, for tests that set a company up after the
    /// brain exists. Production code changes this only through
    /// [`Self::refresh_record`], which is why this is test-only.
    #[cfg(test)]
    fn mutate_record(&self, edit: impl FnOnce(&mut CompanyRecord)) {
        let mut guard = self.record.write().expect("harness brain record poisoned");
        let mut next = (**guard).clone();
        edit(&mut next);
        *guard = Arc::new(next);
    }

    /// Re-reads the record from the store, so this cycle routes on what the
    /// operator has actually saved (issue #707).
    ///
    /// # The error path is loud, and never silently stale
    ///
    /// A failed load **propagates and fails the cycle**. Falling back to the
    /// previous record would reintroduce exactly the defect this exists to fix,
    /// and would do it invisibly — the operator would see a turn that appeared
    /// to succeed while routing on state they had already changed. It is also
    /// not a new failure mode: the cycle path already loads this same record
    /// with `?` (`runtime::cycle`), so a store this broken fails the turn
    /// either way.
    ///
    /// `Ok(None)` — no persisted record — **keeps the current one** rather than
    /// clearing it. That is the same choice [`RuntimeBuilder`] makes when it
    /// seeds a brain (an absent record contributes no overlays rather than
    /// erasing the manifest), and the alternative would turn a company whose
    /// record has not been written yet into one with no roster at all.
    async fn refresh_record(&self) -> Result<()> {
        let id = self.record().id.clone();
        match self.deps.store.load(&id).await? {
            Some(fresh) => {
                *self.record.write().expect("harness brain record poisoned") = Arc::new(fresh);
            }
            None => {
                tracing::warn!(
                    company = %id,
                    "no persisted record to refresh from; the cycle routes on the record this \
                     brain was built with"
                );
            }
        }
        Ok(())
    }

    /// Overrides which roster agent answers operator messages.
    pub fn with_responder(mut self, agent_id: impl Into<String>) -> Self {
        self.responder = agent_id.into();
        self
    }

    /// Wires the run store a dispatched card records its attempt into (#242).
    pub fn with_runs(mut self, runs: Arc<dyn crate::ports::RunStore>) -> Self {
        self.runs = Some(runs);
        self
    }

    /// Runs a dispatched card to completion and, when the card remembers the
    /// conversation it was spawned from, returns the reply to post back there
    /// (issue #151 §3.2).
    ///
    /// Loads the card, routes it to its assignee (or the default responder) for
    /// a single turn, and writes the outcome back onto the board — moved to its
    /// success terminal column on success (see
    /// [`lifecycle::success_terminal_column`]),
    /// back to `todo` with the error noted on failure. A missing task store
    /// or a card that has since vanished is a silent no-op.
    ///
    /// Before this the answer only ever reached `card.note`: the card runs
    /// asynchronously, long after the turn that spawned it has answered, so the
    /// operator had to know to go and look. The note is still written — it stays
    /// the durable record — and the post-back is additive.
    /// Re-dispatches the agent that owns `approval_id`. Legacy policy approvals
    /// re-issue the exact granted call; an explicit `request_approval` receives
    /// the operator's approve/deny decision and continues without asking again.
    ///
    /// Returns the agent's reply as a bubble on its own channel, or `None` when
    /// there is no grant to redeem — which is the common case and must stay a
    /// silent no-op:
    ///
    /// * a **denied** approval (this arm only runs on `Approve`, but a deny
    ///   reaching here must not turn into a turn either);
    /// * a **native** effect the runtime already executed, which mints nothing;
    /// * a **legacy** parked effect from before `Effect::agent` existed, which
    ///   replays as `None` and so mints nothing;
    /// * the **system expiry** `ApprovalResolved { verdict: Deny }` that #309's
    ///   sweep appends;
    /// * a grant already consumed or swept.
    ///
    /// The turn is registered on the steer registry with the same RAII guard
    /// `run_task` uses, so an operator can cancel a re-issue mid-flight and a
    /// crashed turn never strands a ghost row in the in-flight strip.
    async fn redispatch_granted_call(
        &self,
        approval_id: &crate::ports::types::ApprovalId,
        verdict: Verdict,
    ) -> Result<Option<OutboundMessage>> {
        // What the resolution actually minted, whichever scope it was (#374).
        //
        // Both scopes have to be looked up here or the broader one is inert: it
        // arms a permission and then never re-dispatches the agent that was
        // waiting on it, so the parked call the operator just approved is one
        // the agent never re-issues. Peeking only the single-use set — the
        // pre-#374 behaviour — silently no-ops on a miss, which is right for
        // every legitimate miss and catastrophic for this one.
        struct Redispatch {
            agent: String,
            tool: String,
            instruction: String,
            explicit_request: bool,
            origin_thread: Option<String>,
            /// The thread within `origin_thread` the approval was raised in
            /// (#1890). Both grant kinds already record it; dropping it here
            /// would resume a threaded approval against unparented channel
            /// history instead of the conversation that prompted it.
            origin_parent: Option<crate::ports::types::EventSeq>,
        }

        let grants = self.deps.approval_requests.grants();
        let grant = if let Some(continuation) = grants.peek_continuation(approval_id) {
            let grant = continuation.call;
            debug_assert_eq!(continuation.verdict, verdict);
            let title = grant
                .args
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("your request");
            let decision = match continuation.verdict {
                Verdict::Approve => "APPROVED. Continue based on that decision",
                Verdict::Deny => {
                    "DENIED. Respect that decision and continue safely or stop the proposed work"
                }
            };
            Redispatch {
                instruction: format!(
                    "The operator {decision} for your explicit approval request: {title}. Do \
                         not call `request_approval` again for the same action unless circumstances \
                         materially change."
                ),
                tool: grant.tool,
                agent: grant.agent,
                explicit_request: true,
                origin_thread: grant.origin_thread,
                origin_parent: grant.origin_parent,
            }
        } else if let Some(grant) = grants.peek(approval_id) {
            let args = serde_json::to_string(&grant.args).unwrap_or_else(|_| "{}".to_string());
            Redispatch {
                instruction: format!(
                    "Operator approved your `{tool}` call. Re-issue it now with EXACTLY these \
                     arguments: {args}. Do not modify them.",
                    tool = grant.tool,
                ),
                tool: grant.tool,
                agent: grant.agent,
                explicit_request: false,
                origin_thread: grant.origin_thread,
                origin_parent: grant.origin_parent,
            }
        } else if let Some(standing) = grants
            .peek_standing_by_approval(approval_id)
            .filter(|standing| standing.verdict == Verdict::Approve)
        {
            // No exact-arguments pin, and deliberately so: a standing grant
            // admits any arguments, which is precisely what the operator
            // consented to by choosing this scope. Pinning them anyway would
            // make the broader scope behave like the narrow one on its first
            // call and confuse the model about what it is allowed to do next.
            Redispatch {
                instruction: format!(
                    "Operator granted your use of `{tool}` until further notice. Re-issue your \
                     call now.",
                    tool = standing.tool,
                ),
                tool: standing.tool,
                agent: standing.agent,
                explicit_request: false,
                origin_thread: standing.origin_thread,
                origin_parent: standing.origin_parent,
            }
        } else {
            return Ok(None);
        };
        let instruction = grant.instruction.clone();
        let guard = self.deps.steer.register(
            &self.record().id,
            InflightEntry {
                key: format!("approval:{approval_id}"),
                task_id: None,
                kind: InflightKind::Delegation,
                title: format!("re-issue {}", grant.tool),
                agent_id: grant.agent.clone(),
                started_at_millis: now_millis(),
                pending_action: None,
            },
        );
        let control = guard.control().clone();

        let run_turn = self.run_turn();
        // Bound for the runner's whole lifetime (issue #707): one turn, one record.
        let record = self.record();
        // Issue #453: the same argument as the publish claim below, one queue
        // over. This is a full agent turn with the whole toolbelt, so it can
        // reach `review_task` / `assign_task` / `spawn_task` — and nothing here
        // drained them. A `review_task` from a re-issued call was staged, the
        // tool said the card had moved, and the next turn's `clear()` destroyed
        // it.
        //
        // It **drains** rather than refusing, deliberately. `review_task` is a
        // gateable Write effect, so an operator can be asked to approve one; if
        // this path refused, approving it would make the approval unspendable —
        // approve, refuse, re-park, forever. The claim is taken **per
        // re-dispatch turn**, not per continuation cycle: one cycle may run
        // several of these (issue #476 batches resolutions), and each turn owns
        // its own drain window so one re-dispatch's queue can never leak into
        // the next one's.
        let delegation_claim = self.deps.delegations.claim();
        // Issue #445: this is a full agent turn with the whole toolbelt, so it
        // can publish — and before this nothing drained it, exactly as on the
        // chat path. It is a conversation continuation (it answers into the
        // thread the approval was raised in), so it files the same way a chat
        // turn does.
        let publish_claim =
            (self.deps.tasks.is_some() && self.deps.artifacts.is_some()).then(|| {
                self.deps
                    .pending_publishes
                    .claim(publish::PublishDestination::Conversation)
            });
        // Un-streamed, like a dispatched card: this turn is answered by the
        // bubble returned below, and its transient frames would otherwise
        // misattribute onto whichever chat thread the console is watching.
        let outcome = run_turn
            .run_steered_background(
                &self.record().id,
                &grant.agent,
                &instruction,
                &control,
                None,
            )
            .await;
        drop(guard);
        let published = self.deps.pending_publishes.drain();
        if !published.is_empty()
            && publish_claim.is_some()
            && let Err(err) = self
                .record_conversation_publishes(
                    &grant.agent,
                    grant.origin_thread.as_deref(),
                    published,
                )
                .await
        {
            tracing::error!(
                approval_id = %approval_id,
                agent = %grant.agent,
                error = %err,
                "[publish] the re-issued call published files that could not be recorded"
            );
        }
        drop(publish_claim);

        // Then the delegations (issue #453), in that order: publishes first so a
        // file the turn offered is recorded before a card write can fail, and
        // delegations before the bubble is built so the bubble can carry the id
        // of a card this drain opened.
        //
        // Never propagated with `?`. The turn has already run and the operator is
        // owed its answer; unwinding here would swallow the reply over a board
        // write. A hand-off drained from here runs the delegate and settles their
        // card, but has nowhere to relay their reply to — there is no relay turn
        // on this path. That is a strict improvement on dropping it unrun, and it
        // is recorded on the card the hand-off opens.
        let drained = match self
            .delegation_runner(run_turn.as_ref(), &record)
            // The conversation the approval was raised in (#1890) — both halves
            // of it. A grant records `origin_parent` beside `origin_thread` for
            // exactly this reason, so a threaded approval resumes in its own
            // thread rather than against the channel's unparented history.
            .in_thread(grant.origin_parent)
            .drain_and_execute(
                grant.origin_thread.as_deref(),
                delegation::MessageContext::default(),
                delegation::HandOffs::Run,
            )
            .await
        {
            Ok(drained) => drained,
            Err(err) => {
                tracing::error!(
                    approval_id = %approval_id,
                    agent = %grant.agent,
                    error = %err,
                    "[delegation] the re-issued call queued board work that could not be executed"
                );
                delegation::Drained::default()
            }
        };
        drop(delegation_claim);

        let text = match outcome {
            // Issue #1846 review (Codex #3869725683): `run_steered_background`
            // runs through the SAME `run_inner` the interactive chat path
            // does, so a provider that is out of credits parks a re-issue
            // marker for `grant.agent` here exactly as it would for an
            // ordinary message — but `outcome.reply` is just the
            // budget-paused placeholder text (`classify_turn`'s
            // `AttemptOutcome::BudgetPaused` handling), which does NOT start
            // with `BUDGET_PAUSE_NOTICE_PREFIX`. Sent straight through as an
            // ordinary reply, the console's `isBudgetPauseNotice` check never
            // fires, so the bubble rendered as a normal (if oddly-worded)
            // answer with no "Add credits & resend" CTA — even though a
            // marker was sitting there, parked and redeemable, the whole time.
            // Swap in the unauthored pause notice so the bubble reads as the
            // runtime terminal state it is rather than as an answer.
            //
            // Issue #1846 review (Codex #3870562590): the NO-RESEND prefix,
            // not the redeemable one. `run_steered_background` means
            // `run_inner` parks this marker with `background: true`, and
            // `redeem_budget_pause` refuses exactly that shape
            // (`src/server/ops/budget_pause.rs`) — so offering the CTA here
            // reserved the marker, restored it, and returned 400 on every
            // click. Resuming an approval continuation needs the grant's own
            // identity, which the generic chat-message redeem path does not
            // carry; until it does, the honest surface is a notice with no
            // button rather than a button that cannot work.
            Ok(outcome) => match &outcome.budget_paused {
                Some(pause) => budget_pause_notice_no_resend(pause),
                None => {
                    if grant.explicit_request && grants.consume_continuation(approval_id).is_none()
                    {
                        tracing::warn!(
                            approval_id = %approval_id,
                            "explicit approval continuation was already consumed or expired; \
                             the agent's turn still ran"
                        );
                    }
                    outcome.reply
                }
            },
            Err(err) => {
                // The grant stays live: the call did not go through, so the
                // operator's approval has not been spent and the TTL sweep will
                // tell them if it never does. Reporting the failure is what stops
                // this looking like a silent success.
                tracing::warn!(
                    approval_id = %approval_id,
                    tool = %grant.tool,
                    error = %err,
                    "[approval] re-issuing an approved tool call failed"
                );
                format!(
                    "re-issuing the approved `{}` call failed: {err}",
                    grant.tool
                )
            }
        };

        // The reply is **not** journaled here (issue #469).
        //
        // It used to be, because nothing else did: the resolve route dropped a
        // continuation's replies on the floor, so this arm hand-wrote its own
        // `AgentReply` to get the answer onto the event stream. That covered
        // exactly one shape — a re-dispatch that found a grant to redeem — and
        // left every other continuation reply invisible, including the ones this
        // function returns `None` for and everything the default build produces.
        //
        // Journaling now happens once, for every continuation reply, in
        // `CompanyRuntime::publish_continuation`, against the same thread this
        // used (`journal.approval_thread`, issue #379's key) with the same
        // fallback to the answering agent. Writing it here as well would post the
        // agent's answer into the conversation twice.
        Ok(Some(OutboundMessage {
            message_id: None,
            // The card this turn's board work opened, when it opened one (issue
            // #453) — the same first-wins id an operator turn's bubble carries,
            // so a continuation that spawned or handed off work links to it
            // instead of pointing at nothing.
            task_id: drained.spawned_task,
            channel: grant.agent.clone(),
            agent: None,
            text,
            steps: Vec::new(),
            reply_to: None,
            mentions: Vec::new(),
        }))
    }

    async fn run_task(
        &self,
        task_id: &str,
        run_id: Option<&str>,
    ) -> Result<Option<OutboundMessage>> {
        // Issue #242: the attempt this dispatch is recorded under. `None`
        // whenever the run store is unwired or the choke point could not mint a
        // row — the card runs either way, untracked.
        let sink = self.open_trace(run_id);

        let Some(tasks) = self.deps.tasks.as_ref() else {
            self.settle_run(sink.as_deref(), RunStatus::Failed, Some(NO_TASK_STORE))
                .await;
            return Ok(None);
        };
        let Some(mut card) = tasks
            .list(&self.record().id)
            .await?
            .into_iter()
            .find(|t| t.id == task_id)
        else {
            self.settle_run(sink.as_deref(), RunStatus::Failed, Some(CARD_VANISHED))
                .await;
            return Ok(None);
        };

        // Issue #205: who works this card, resolved against the FULL roster —
        // teammates, operator-overlay teammates and desks alike.
        let resolution = assignee::resolve(&self.record(), &card.assignee);
        if let Some(reason) = resolution.rejection() {
            // The card names somebody this company does not have. Before this
            // it dispatched to the orchestrator anyway and the board kept the
            // invalid name, so the only trace was a timeline that read "reply
            // from ceo" on a card assigned to somebody else. Refuse instead: no
            // turn is run (nothing was asked of the orchestrator), the card
            // returns to `todo` carrying the reason, and the operator is
            // told — on the board, on the timeline, and in the thread the card
            // came from.
            tracing::warn!(
                task_id = %card.id,
                assignee = %card.assignee,
                "[task] refusing dispatch: {reason}"
            );
            return self
                .refuse_dispatch(tasks, card, &reason, sink.as_deref())
                .await;
        }
        // A blank assignee is the one legitimate miss: nobody was named, so the
        // orchestrator picks it up.
        //
        // Not final: a turn that hands the work off (issue #204) reassigns the
        // card to the delegate, and from that point the delegate is the
        // responder every downstream write credits — the note, the artifact,
        // the journal, and the relay.
        let mut responder = resolution
            .working_agent()
            .unwrap_or(&self.responder)
            .to_string();

        // Link the working agent to the card, and persist it BEFORE the turn
        // runs (#205). A card the CEO picked up used to keep `assignee = ""`
        // for the whole run and forever after, so the board never named who was
        // doing the work. Writing it up front is what makes the board show the
        // card "working" under a real agent while the turn is in flight — the
        // store `upsert` here is the plain persistence path, not
        // `CompanyRuntime::upsert_task`, so it cannot re-fire the dispatch edge.
        //
        // Only for an assignee that names a teammate or nobody, though. A desk
        // assignment is ownership and stays one — `AssigneeResolution::canonical`
        // deliberately stores the **desk** id and the REST boundary honours it;
        // dispatch only picks which member runs *this* turn. Writing the lead
        // back would silently turn a card assigned to `eng` into one assigned to
        // `engineer` the first time it ran, erasing the desk from the board and
        // breaking the very invariant `canonical()` documents (#214 review).
        if resolution.links_working_agent() && card.assignee != responder {
            card.assignee = responder.clone();
            card.updated_at_millis = now_millis();
            tasks.upsert(&self.record().id, &card).await?;
        }

        // Issue #244: the dispatch-start baseline for "did this agent write
        // anything it did not publish?".
        //
        // Taken **after** responder resolution, because the workspace is the
        // responder's — a snapshot taken before we knew who was running would
        // be of the wrong directory. `dispatched_responder` remembers who that
        // was, so a hand-off (which reassigns the card mid-run) can be detected
        // and the whole detection path skipped rather than diffed against a
        // workspace nobody touched.
        let dispatched_responder = responder.clone();
        let workspace = agent_workspace(&self.deps.workspace_root, &self.record().id, &responder);
        let workspace_at_dispatch = WorkspaceSnapshot::take(&workspace);
        // Claim the publish queue for this dispatch (#445). The claim clears on
        // the way in for the reason the bare `clear()` here always did — a chat
        // turn earlier in this cycle shares these deps, and its staged file must
        // never be attributed to this card — and now also tells the tool which
        // destination to name in its receipt.
        //
        // Only when there is an artifact store to record into. Without one the
        // drain below has nowhere to write and would log a warning while the
        // agent was told its file was safe, so leaving the queue unclaimed
        // turns that into an in-turn refusal the agent can actually report.
        // `build_agent` already declines to wire the tool at all in that case;
        // this makes the invariant local rather than borrowed from the builder.
        let _publish_claim = self.deps.artifacts.as_ref().map(|_| {
            self.deps
                .pending_publishes
                .claim(publish::PublishDestination::Task)
        });
        // Issue #453: and the delegation queue, for the same span. A dispatched
        // card's responder is the orchestrator, which carries the delegation
        // tools, and `handle_task_delegations` below is the drain — so this path
        // has always been entitled to delegate and now says so. Unconditional:
        // the drain runs whatever else is wired (a card with no task store
        // simply executes to no effect, which every task path on this seam does).
        //
        // Spans the whole steer loop through the drain, not one iteration of it.
        // The per-iteration `clear()` inside the loop stays — it abandons a
        // redirected turn's work, which is a different decision from who is
        // entitled to queue.
        let _delegation_claim = self.deps.delegations.claim();
        // Issue #339, same argument for staged workflow references: an operator
        // chat turn earlier in this cycle may have run a workflow through the
        // orchestrator's tool, and that run belongs to the conversation, not to
        // this card.
        self.deps.workflow_refs.clear();

        // Register the run so an operator can steer it mid-flight. The guard's
        // RAII `Drop` deregisters on every exit path (success, error, redirect
        // exhaustion), so a crashed turn never leaves a ghost row in the strip.
        let guard = self.deps.steer.register(
            &self.record().id,
            InflightEntry {
                key: card.id.clone(),
                task_id: Some(card.id.clone()),
                kind: InflightKind::Task,
                title: card.title.clone(),
                agent_id: responder.clone(),
                started_at_millis: now_millis(),
                pending_action: None,
            },
        );
        let control = guard.control().clone();

        // The base turn instruction is frozen at dispatch (the card's note keeps
        // accumulating operator/agent blocks, but a redirect always re-runs from
        // the original brief plus the fresh instruction — last redirect wins).
        let base_instruction = task_instruction(&card);
        let mut instruction = base_instruction.clone();
        let mut redirects: u32 = 0;
        // Route the background turn through the brain-agnostic `RunTurn` seam
        // (issue #176), re-attaching `HarnessDeps` behind `HarnessRunTurn`.
        let run_turn = self.run_turn();
        // Bound for the runner's whole lifetime (issue #707): one turn, one record.
        let record = self.record();
        // Issue #242: where this attempt's own approval requests begin. The
        // queue is shared with any chat turn earlier in the same cycle and is
        // append-only until the cycle-end drain, so a position taken here stays
        // the boundary between "somebody else parked that" and "this run did".
        let approvals_before = self.deps.approval_requests.queued();

        // The loop yields how the run ended plus its operator-facing result on
        // whichever path ends it, so the artifact (#187), the completion event
        // (#185) and the attempt row (#242) all record exactly what the note
        // does rather than three divergent renderings of one run. The ending
        // rides along because the run's status cannot be re-derived from the
        // card's landing column — `Failed` and `Cancelled` share one column and
        // are not the same outcome (see [`lifecycle::run_status_for`]).
        let (run_end, result_text) = loop {
            // Start each turn from an empty queue so nothing a prior turn (this
            // cycle's operator message, or an earlier redirect rerun) left
            // behind can hijack this card — the same guard
            // `handle_operator_message` opens with.
            self.deps.delegations.clear();
            // Issue #244, same argument for staged publishes: a redirect
            // re-runs from the original brief and *abandons* the previous
            // turn's work, so a file that turn offered must be abandoned with
            // it. This is inside the loop deliberately; the nudge below is not
            // part of the loop and never clears, so a nudge cannot discard what
            // the turn it is asking about published.
            self.deps.pending_publishes.clear();
            // Issue #339: an abandoned redirect's workflow run is abandoned with
            // it, for the same reason — the card's link must name what the turn
            // that actually settled produced, not what a discarded one did.
            self.deps.workflow_refs.clear();
            let outcome = run_turn
                // A dispatched task card carries no chat bubble (its steps are
                // discarded into the note), so its live turn frames must not leak
                // onto the console timeline — run it un-streamed (#125 review).
                .run_steered_background(
                    &self.record().id,
                    &responder,
                    &instruction,
                    &control,
                    // Issue #242: un-streamed does not mean unrecorded. The
                    // trace this turn produces is written to the attempt row as
                    // it happens, which is what a redirect re-run appends to
                    // rather than restarting.
                    sink.clone(),
                )
                .await;
            // One-shot read of what (if anything) the operator asked for. `None`
            // is the ordinary, unsteered path.
            match control.take() {
                None => {
                    // A dispatched task discards its steps — the note is text-only.
                    match outcome {
                        Ok(outcome) => {
                            // Issue #1846 review (Codex #3864988168): a budget
                            // pause is a terminal state, exactly like a spend
                            // halt or an iteration-cap pause — the model call
                            // itself failed, so `outcome.reply` is not a
                            // completed answer, it is the placeholder/notice
                            // text `classify_turn` substitutes (mirrors the
                            // operator-chat path's own
                            // `BUDGET_PAUSED_PLACEHOLDER_REPLY` treatment
                            // below). Settling this as `Completed` let a
                            // background dispatch's exhausted-budget failure
                            // read on the board as a finished, reviewable
                            // result — the same asymmetry this issue's
                            // headline fix closes for the top-level
                            // orchestrator's own call, just on the dispatched-
                            // card path instead.
                            //
                            // Checked, and returned on, BEFORE the delegation
                            // drain: per `classify_turn`, the budget-paused arm
                            // only fires when the model call itself errored,
                            // which cannot also have queued a hand-off on the
                            // same attempt — so there is nothing below worth
                            // draining, and doing so unconditionally would risk
                            // running a stale hand-off from an earlier retry
                            // still sitting in the queue.
                            if let Some(pause) = &outcome.budget_paused {
                                let result = budget_pause_notice(pause);
                                settle(&mut card, TaskRunEnd::Paused, &responder, &result);
                                break (TaskRunEnd::Paused, result);
                            }
                            // Issue #204: the turn may have DELEGATED rather
                            // than done the work. The dispatched responder is
                            // the orchestrator, which carries `delegate_to_desk`
                            // / `spawn_task`, and nothing here used to drain
                            // what those queued — so the hand-off was dropped,
                            // the turn still read as a clean completion, and the
                            // card landed in `in_review` under the delegator
                            // with the delegate never having run. Draining here
                            // runs the delegate, reassigns the card to them, and
                            // settles it from THEIR output.
                            //
                            // An errored hand-off lands exactly like an errored
                            // turn (the `Err` arm below), and must NOT propagate
                            // with `?`. By the time `run_delegation` can fail,
                            // `hand_card_over` has already persisted the card as
                            // `in_progress` reassigned to the delegate — so
                            // unwinding here would skip both the settle and the
                            // final `upsert` and leave the card sitting in
                            // `in_progress` under a delegate that produced
                            // nothing, with no result and nothing to re-dispatch
                            // it: `task_enters_in_progress` only edge-fires on
                            // the *transition* into that column, which already
                            // happened. That is precisely the stranded state
                            // this fix exists to eliminate.
                            //
                            // The card keeps the delegate as its assignee on the
                            // way to `todo` — the hand-off did happen, and a
                            // re-dispatch should start from who it was given to.
                            let handoff = match self
                                .delegation_runner(run_turn.as_ref(), &record)
                                .for_task(&card.id)
                                // The delegate's turn is part of THIS attempt —
                                // its steps and its spend belong to the card's
                                // run, not to nothing (#242).
                                .for_run(sink.clone())
                                // Issue #1846 review (Codex #3864988176): the
                                // card's own (possibly redirect-augmented)
                                // instruction — the closest thing a dispatched
                                // task has to "the operator's own words" — so a
                                // delegate's budget-pause marker re-parks with
                                // the brief this attempt is actually running,
                                // not the hand-off instruction the model wrote.
                                .reissue_message(instruction.clone())
                                .handle_task_delegations(&mut card, &responder)
                                .await
                            {
                                Ok(handoff) => handoff,
                                Err(err) => {
                                    let result = format!("hand-off failed: {err}");
                                    // Issue #1861: a hand-off that failed on a
                                    // rejected model id or a dead integration
                                    // is as answerable as a direct dispatch
                                    // that did — the delegate hit the same
                                    // wall, so it asks the same question.
                                    let end = self.settle_as_blocker_or_failure(
                                        &card.id,
                                        &result,
                                        sink.as_ref().map(|s| s.run_id()),
                                    );
                                    settle(&mut card, end, &responder, &result);
                                    break (end, result);
                                }
                            };
                            // `settle` writes the note (attributed to whoever
                            // actually produced the text) and the landing column
                            // via the #186 lifecycle seam; the loop still yields
                            // the reply so the #185/#190 completion events
                            // report the same text that landed in the note.
                            let (end, result) = match handoff {
                                // The delegate answered: they own the card, and
                                // every downstream write credits them.
                                Some(handoff) => {
                                    responder = handoff.delegate;
                                    let budget_paused = handoff.budget_paused;
                                    match handoff.reply {
                                        Some(reply) => {
                                            // Issue #1846 review (Codex
                                            // #3865395868): `TaskHandoff` now
                                            // carries the delegate's own
                                            // budget pause through from
                                            // `DeskReply` — this is the other
                                            // half of the asymmetry the
                                            // top-level orchestrator's own
                                            // dispatched call already closed
                                            // above (`outcome.budget_paused`).
                                            // Without it a delegate that ran
                                            // out of credits still settled
                                            // `Completed`, landing the pause
                                            // notice in In Review as though it
                                            // were a finished answer.
                                            let end = if budget_paused.is_some() {
                                                TaskRunEnd::Paused
                                            } else {
                                                TaskRunEnd::Completed
                                            };
                                            settle(&mut card, end, &responder, &reply);
                                            (end, reply)
                                        }
                                        // The hand-off ran and an operator
                                        // CANCELLED it in flight, so it produced
                                        // nothing. Naming the cancellation here
                                        // is safe because `TaskHandoff` only
                                        // carries `reply: None` for a run
                                        // `run_delegation` reported as cancelled
                                        // — a hand-off that ends empty for any
                                        // other reason reports no hand-off at
                                        // all and never reaches this arm (issue
                                        // #213 review).
                                        //
                                        // Partial work is discarded and the card
                                        // returns to To-do, exactly as a
                                        // cancelled dispatch does — it must not
                                        // read as finished, and it must not
                                        // strand in `in_progress` either.
                                        None => {
                                            let reply =
                                                "the delegated run was cancelled before it \
                                                 produced anything"
                                                    .to_string();
                                            settle(
                                                &mut card,
                                                TaskRunEnd::Cancelled,
                                                &responder,
                                                &reply,
                                            );
                                            (TaskRunEnd::Cancelled, reply)
                                        }
                                    }
                                }
                                // Nothing was handed off — the responder did the
                                // work itself, as before.
                                None => {
                                    let result = outcome.reply;
                                    settle(&mut card, TaskRunEnd::Completed, &responder, &result);
                                    (TaskRunEnd::Completed, result)
                                }
                            };
                            break (end, result);
                        }
                        Err(err) => {
                            // Issue #1861: the main settle site. A stop the
                            // classifier recognises as answerable parks a
                            // blocker and lands the card `paused` with the
                            // question on it; everything else settles `Failed`
                            // exactly as before.
                            let result = format!("dispatch failed: {err}");
                            let end = self.settle_as_blocker_or_failure(
                                &card.id,
                                &result,
                                sink.as_ref().map(|s| s.run_id()),
                            );
                            settle(&mut card, end, &responder, &result);
                            break (end, result);
                        }
                    }
                }
                Some(SteerAction::Cancel) => {
                    // Partial work is DISCARDED — only a cancellation note lands,
                    // and the card returns to `todo`. The note is attributed to
                    // the operator, not the assignee (the lifecycle seam decides
                    // that). The loop still yields the text for #185/#190.
                    let result = "cancelled while in flight".to_string();
                    settle(&mut card, TaskRunEnd::Cancelled, &responder, &result);
                    break (TaskRunEnd::Cancelled, result);
                }
                Some(SteerAction::Pause) => {
                    // Partial work is PRESERVED in the note; the card parks in the
                    // `paused` column. The cycle ends normally, so the per-tenant
                    // serial lock releases while parked — resume is a plain
                    // `column → in_progress` PATCH that re-triggers dispatch.
                    let partial = match &outcome {
                        Ok(outcome) => format!("[paused] {}", outcome.reply),
                        Err(err) => format!("[paused] dispatch failed: {err}"),
                    };
                    settle(&mut card, TaskRunEnd::Paused, &responder, &partial);
                    break (TaskRunEnd::Paused, partial);
                }
                Some(SteerAction::Redirect { instruction: fresh }) => {
                    redirects += 1;
                    card.note = Some(append_result(
                        card.note.as_deref(),
                        "operator redirect",
                        &fresh,
                    ));
                    if redirects > MAX_REDIRECTS_PER_DISPATCH {
                        // Exhausted the redirect budget — finalize the last run's
                        // reply to the card's terminal column rather than looping
                        // forever.
                        let last = match &outcome {
                            Ok(outcome) => outcome.reply.clone(),
                            Err(err) => format!("dispatch failed: {err}"),
                        };
                        settle(&mut card, TaskRunEnd::RedirectsExhausted, &responder, &last);
                        break (TaskRunEnd::RedirectsExhausted, last);
                    }
                    // Re-run from the original brief plus the (codepoint-capped)
                    // operator instruction.
                    instruction = format!(
                        "{base_instruction}\n\nOperator redirect: {}",
                        cap_redirect(&fresh)
                    );
                    continue;
                }
            }
        };

        // ── Issue #244: the deliverable gate, and the one nudge ─────────────
        //
        // This sits between the primary turn and the completion bookkeeping —
        // after the run ended and delegation settled, before the card is
        // persisted and the queue drained — because a nudge that ran after the
        // card landed would be asking about a task the board already calls
        // finished.
        //
        // It cannot be mid-turn: OpenHuman's session loop is not injectable. But
        // `run_task` already runs several turns per run (the redirect loop and
        // the delegation runner both do exactly this), so a follow-up turn is
        // the acting surface available.
        //
        // Gated on `Completed` alone. A failure, a cancellation, a pause or a
        // spent redirect budget is not a moment to ask an agent about its files.
        let mut declined: Option<String> = None;
        let mut unpublished_before_nudge: Vec<String> = Vec::new();
        // Issue #420 item 3: whether the scan below saw the whole sandbox. A
        // partial scan can only under-report, but the nudge must say so rather
        // than present a DFS prefix as the complete list of what changed.
        let mut scan_partial = false;
        if run_end == TaskRunEnd::Completed {
            if responder == dispatched_responder {
                let changed = workspace_at_dispatch.changed_since(&workspace);
                scan_partial = changed.partial;
                unpublished_before_nudge =
                    publish::unpublished(&changed.files, &self.deps.pending_publishes.sources());
            } else {
                // A hand-off reassigned the card, so the snapshot above is of
                // the delegator's workspace and the work happened in the
                // delegate's. Diffing them would name files nobody wrote this
                // run. Degrade silently: no nudge, no warning, and whatever the
                // delegate published is still drained and recorded below.
                tracing::debug!(
                    task_id = %card.id,
                    from = %dispatched_responder,
                    to = %responder,
                    "[publish] responder was reassigned mid-run; skipping the unpublished scan"
                );
            }
        }

        // **Exactly one nudge, by construction.** Straight-line code guarded by
        // a local — not a loop, not a counter, not inside the redirect loop. A
        // second nudge is not merely absent, there is nowhere to write one.
        if !unpublished_before_nudge.is_empty() {
            declined = self
                .nudge_for_unpublished(
                    run_turn.as_ref(),
                    &responder,
                    &base_instruction,
                    &result_text,
                    &unpublished_before_nudge,
                    scan_partial,
                    &control,
                    sink.clone(),
                )
                .await;
        }

        // The fallback's file list is the **pre-nudge** diff minus whatever is
        // staged now. Deliberately not a fresh scan: a scratch file the agent
        // wrote *while answering the nudge* is an artifact of being asked, and
        // naming it in the warning would make the nudge generate its own noise.
        let still_unpublished = publish::unpublished(
            &unpublished_before_nudge,
            &self.deps.pending_publishes.sources(),
        );
        if !still_unpublished.is_empty() {
            // A decline is a clean outcome, not an error: the reason goes on the
            // card where it is addressable, the warning names the files for
            // whoever is watching logs, and nothing else happens. No retry, no
            // failure, no artifact.
            if let Some(reply) = declined.as_deref() {
                card.note = Some(append_result(
                    card.note.as_deref(),
                    &responder,
                    &publish::declined_note(&still_unpublished, reply),
                ));
            }
            tracing::warn!(
                task_id = %card.id,
                agent = %responder,
                files = %publish::name_files(&still_unpublished),
                declined = declined.is_some(),
                // Issue #420 item 3: whoever reads this needs to know the file
                // list is a floor, not an inventory — the agent may have
                // declined about files this scan never reached.
                partial_scan = scan_partial,
                "[publish] the run changed sandbox files and published none of them; no \
                 artifact was recorded"
            );
        }

        // Issue #337: the attempt's **settled status** decides where the card
        // lands, so it has to be known before the card is written.
        //
        // This ordering is the fix. The parked-approval count used to be taken
        // *after* the upsert above, which made `waiting_approval → in_review`
        // literally inexpressible: by the time the brain knew a person had been
        // left something to act on, the card was already persisted wherever the
        // turn's ending had put it. A run that parked an approval on a delegated
        // card therefore landed in **Done** — filed as finished while a human
        // still had to authorise the call it was blocked on.
        //
        // So: stamp and count first (#333's unblocking move — tag every
        // approval this attempt's own turns parked with the run that produced
        // it), fold that into the settled status, then land the card from the
        // one mapping, then persist.
        let parked = match sink.as_ref() {
            Some(sink) => self
                .deps
                .approval_requests
                .stamp_run(approvals_before, sink.run_id()),
            None => 0,
        };
        // Issue #1861: and how many of them were *questions* rather than
        // decisions. Counted from the same boundary `stamp_run` stamps from, so
        // the two describe one set — and unconditionally, because a card with
        // no attempt row still parks its blocker and still must not land in
        // review with an unanswered question on it.
        let blockers = self.deps.approval_requests.blockers_since(approvals_before);
        let settled = lifecycle::settled_run_status_with_blockers(run_end, parked, blockers);

        // ── Issues #244 + #339: record what the run produced, then say so on
        //    the card — both **before** the one card write ────────────────────
        //
        // The queues are drained **unconditionally** so nothing an abandoned
        // turn staged can leak into the next run, and the drained sets are
        // recorded only when the run reached its success terminal.
        //
        // "Success terminal" is read off the *ending* —
        // `run_status_for(run_end) == Succeeded`, i.e. the turn finished or
        // spent its redirect budget — not off `settled`. Parking an approval
        // relabels who acts next, not whether the agent produced a deliverable,
        // so a run that wrote a spec and also asked for an authorisation must
        // not lose the spec.
        //
        // **Why this moved above the upsert (issue #339).** The card now
        // carries a link to what it produced, and that link names artifact
        // versions that do not exist until they are written — so the artifacts
        // have to land before the card is persisted, or the single card write
        // would have to become two.
        //
        // **And why recording became best-effort with it.** #244 made a failed
        // artifact write propagate, on the argument that an explicit publish
        // that could not be stored is a real failure of the run. That argument
        // held while this ran *after* the card was persisted. Above the upsert
        // a `?` would skip the card write entirely and strand the card in
        // `in_progress` with nothing to re-dispatch it — the exact stranded
        // state the hand-off arm above exists to prevent, and a far worse
        // outcome than a missing deliverable record. So the failure is now
        // logged at `error` (loudly: an operator whose published file did not
        // store needs to know) and the settle continues.
        let published = self.deps.pending_publishes.drain();
        let staged_workflows = self.deps.workflow_refs.drain();
        let succeeded = lifecycle::run_status_for(run_end) == RunStatus::Succeeded;
        let recorded: Vec<TaskOutputArtifact> = if succeeded {
            match self
                .record_published_artifacts(
                    &card,
                    &responder,
                    published,
                    sink.as_ref().map(|s| s.run_id()),
                )
                .await
            {
                Ok(recorded) => recorded,
                Err(err) => {
                    tracing::error!(
                        task_id = %card.id,
                        agent = %responder,
                        error = %err,
                        "[publish] could not record what this run published; the dispatch itself \
                         still lands"
                    );
                    Vec::new()
                }
            }
        } else {
            if !published.is_empty() || !staged_workflows.is_empty() {
                tracing::info!(
                    task_id = %card.id,
                    staged = published.len(),
                    workflows = staged_workflows.len(),
                    ending = ?run_end,
                    "[publish] the run did not reach its success terminal; staged output is \
                     discarded rather than recorded"
                );
            }
            Vec::new()
        };
        // The completion event (#185) carries the ids, the card (#339) carries
        // the pinned versions — one recording, two readers, so they cannot
        // disagree about what the run produced.
        let artifact_ids: Vec<String> = recorded
            .iter()
            .map(|artifact| artifact.artifact_id.clone())
            .collect();

        // Issue #339: the card's link to its deliverable.
        //
        // Only on success, and only when there is an attempt to name: without a
        // trace sink the run store is unwired or the dispatch minted no row, so
        // there is no addressable attempt and a stamp would point at nothing.
        // That degrades to exactly the pre-#339 card, which is the same
        // fail-silent-not-closed direction `runs: None` already takes.
        //
        // Written **wholesale**, overwriting any earlier stamp: this is what
        // makes "the latest successful attempt" true by construction rather
        // than by a read-time query. A later failure never reaches here, so a
        // failed retry cannot erase the link to the success before it.
        if succeeded && let Some(sink) = sink.as_ref() {
            card.output = Some(TaskOutput {
                source: TaskOutputSource::Run {
                    run_id: sink.run_id().to_string(),
                    attempt: self.attempt_ordinal(sink.run_id()).await,
                },
                at_millis: now_millis(),
                artifacts: recorded,
                workflows: staged_workflows,
            });
        }

        // `settle()` already wrote a landing at the break point; this is the
        // authoritative overwrite now that the parked count is known. `None`
        // only for a status that is not settled at all, which no ending
        // produces — a hand-off never breaks the loop.
        if let Some(column) = crate::ports::tasks::column_for_settled_run(settled) {
            card.column = column.to_string();
            // Issue #1865: the board's bounce chip — same rule the system
            // mover applies in `crate::runtime::advance::advance_settled_card`,
            // so a card cannot read differently depending on which of the two
            // settle paths landed it. `result_text` is this attempt's own
            // account of what happened, the same text `settle_run` below
            // stamps as the failure reason.
            card.bounced = crate::runtime::advance::bounced_reason(column, settled, &result_text);
        }
        card.updated_at_millis = now_millis();
        tasks.upsert(&self.record().id, &card).await?;
        // Issue #1883 (CodeRabbit review, PR #1883): the durable notification
        // every other failed-dispatch path already files — `refuse_dispatch`
        // below, the cycle's terminality backstop, and the workflow-builder
        // failure path (`workflow_build.rs`) all call `notify_dispatch_failed`
        // when a card bounces to To-do. This rich settle — the ordinary
        // "an assigned card's turn failed" ending — stamped the bounce chip
        // above but never filed the row, so a card with no `origin_chat_id`
        // (nothing dispatched straight from a chat thread, so no relay to
        // answer in) got neither a chat reply nor a durable notification: the
        // failure was visible only to someone who happened to look at the
        // board. The backstop cannot pick this up later either — it skips
        // any run that is no longer active, and `settle_run` below is what
        // terminalizes this one.
        //
        // `card.bounced.is_some()` is exactly `column_for_settled_run` having
        // landed on `COLUMN_TODO` with a failure/cancellation status (the
        // check `bounced_reason` above already made); the `settled ==
        // RunStatus::Failed` guard narrows it to an actual failure — a card
        // the responder or an operator deliberately cancelled is not a
        // dispatch failure and must not page anyone.
        if card.bounced.is_some()
            && matches!(settled, RunStatus::Failed)
            && let Some(notifications) = self.deps.notifications.as_deref()
        {
            crate::runtime::advance::notify_dispatch_failed(
                notifications,
                &self.record().id,
                &card.id,
                &result_text,
            )
            .await;
        }
        // `guard` drops here → the run leaves the in-flight strip.
        drop(guard);

        // Issue #242: settle the attempt row from the same status the card was
        // just landed from, so the run record and the board cannot disagree.
        //
        // Here, right after the card is persisted and before any of the
        // best-effort journal writes below, so both are readable together. It
        // also means the cycle's terminality backstop finds this row already
        // settled and no-ops — the rich settle always wins the race, because
        // `run_task` returns before `run_locked` reaches the backstop.
        self.settle_run(
            sink.as_deref(),
            settled,
            // A failure or blocked attempt carries a reason: `error` is "why this went
            // wrong", not "what the agent said". Stamping a success's reply
            // here would put the deliverable in a field every reader renders
            // as a fault.
            matches!(settled, RunStatus::Failed | RunStatus::Blocked)
                .then_some(result_text.as_str()),
        )
        .await;

        // Issue #185: correlate this dispatch's journal trail to its card.
        //
        // Ordering matters. Any MCP failures the turn queued are drained FIRST,
        // tagged with this task, so they land on the task's own timeline. Before
        // this they were left in the queue for whichever operator turn drained
        // next — which both mis-attributed them to an unrelated chat bubble and
        // left the dispatch's timeline silent about the very calls that broke.
        //
        // The steps the drain produces are discarded, matching the rest of
        // `run_task`: a dispatched card has no chat bubble to render them on
        // (they are journaled as `McpCallFailed` events instead).
        //
        // Every write below is **best-effort**: the card was already persisted
        // above, so propagating a journal failure with `?` would abandon the
        // terminal anchor *and* the #151 post-back for a dispatch that has in
        // fact landed — leaving a timeline stuck "still running" for a card the
        // board already shows in its terminal column, and failing the whole cycle over
        // a bookkeeping write. Matches the existing journal-after-persist sites
        // (`chat_and_emit`, `WorkflowCreated`, `TaskSteered`).
        let mut discarded_steps = Vec::new();
        if let Err(err) = self
            .surface_mcp_failures(&mut discarded_steps, Some(&card.id))
            .await
        {
            tracing::warn!(
                task_id = %card.id,
                error = %err,
                "[task] failed to journal dispatch MCP failures; continuing"
            );
        }

        // The terminal timeline is written before the relay is returned so the
        // event journal preserves the causal order: task outcome, then the
        // origin-thread relay. The dispatch-cycle wrapper persists the returned
        // relay after this function completes.
        //
        // Issue #151 §3.2: answer in the conversation the card was spawned
        // from. Only a card that remembers an origin posts back — one created
        // straight on the board, or written before `origin_chat_id` existed,
        // has no thread to answer in and behaves exactly as before.
        //
        // Issue #186: the **orchestrator** relays the result, not the assignee.
        //
        // The bubble used to be attributed to the responder, so a desk member
        // spoke straight to the operator — which bypasses the orchestrator's
        // role as the single point of contact that `run_delegation` already
        // honours. It is now the orchestrator's bubble, and the assignee is
        // credited inside the text, so the operator still knows who did the
        // work without a second voice in the thread.
        //
        // It still carries the card's landing column, so the operator reads one
        // line and knows both what came back and where the card went. Steps are
        // deliberately empty: a dispatched card discards them into the note.
        self.journal_task_outcome(&card, &responder, result_text, artifact_ids)
            .await;
        let Some(origin) = card.origin_chat_id.clone() else {
            return Ok(None);
        };
        let relay = lifecycle::relay_reply(&card, &responder, &self.orchestrator(), origin);
        Ok(Some(relay))
    }

    /// Ends a dispatch that never ran because its `assignee` names nobody this
    /// company has (issue #205).
    ///
    /// Takes the same three exits a finished run does — the card is settled and
    /// persisted, the outcome is journaled onto the task's timeline, and a card
    /// that remembers an origin thread is answered there — so the refusal is
    /// visible everywhere a result would have been, instead of being the silence
    /// this issue is about. It deliberately skips the three things that belong
    /// to a run that happened: no in-flight registration (nothing is in flight),
    /// no MCP drain (no turn queued anything), and no artifact (`Failed` lands
    /// in `todo`, never a success terminal).
    ///
    /// Attributed to the **orchestrator**: it is the company answering for its
    /// own roster, and the named assignee does not exist to speak.
    async fn refuse_dispatch(
        &self,
        tasks: &Arc<dyn crate::ports::TaskStore>,
        mut card: TaskRecord,
        reason: &str,
        sink: Option<&RunTraceSink>,
    ) -> Result<Option<OutboundMessage>> {
        let orchestrator = self.orchestrator();
        let text = format!("dispatch refused: {reason}");
        settle(&mut card, TaskRunEnd::Failed, &orchestrator, &text);
        // Issue #1865 (CodeRabbit review, PR #1883): the same bounce-chip rule
        // `run_task`'s rich settle and `advance::advance_settled_card` already
        // apply. Without this, a refusal — an invalid `assignee` — lands the
        // card in `todo` exactly like any other failed dispatch but skips the
        // amber chip that failure is supposed to carry, because this is the
        // one settle path that never computed it.
        card.bounced =
            crate::runtime::advance::bounced_reason(&card.column, RunStatus::Failed, &text);
        card.updated_at_millis = now_millis();
        tasks.upsert(&self.record().id, &card).await?;
        if let Some(notifications) = self.deps.notifications.as_deref() {
            crate::runtime::advance::notify_dispatch_failed(
                notifications,
                &self.record().id,
                &card.id,
                &text,
            )
            .await;
        }
        // A refusal is a real, terminal attempt — one that spent nothing. It
        // settles like any other ending (#242), so the card's run history shows
        // "this was tried and refused, and why" rather than a gap.
        self.settle_run_end(sink, TaskRunEnd::Failed, &text, 0)
            .await;

        let Some(origin) = card.origin_chat_id.clone() else {
            // A refusal has no relay, but its terminal outcome still belongs
            // on the task timeline.
            self.journal_task_outcome(&card, &orchestrator, text, Vec::new())
                .await;
            return Ok(None);
        };
        let relay = lifecycle::relay_reply(&card, &orchestrator, &orchestrator, origin);
        self.journal_task_outcome(&card, &orchestrator, text, Vec::new())
            .await;
        Ok(Some(relay))
    }

    /// Opens the trace sink for this dispatch's attempt row (issue #242), or
    /// `None` when there is nothing to record into.
    ///
    /// Two independent reasons for `None`, and both are ordinary: the run store
    /// is unwired (every test construction, and any embedder that never called
    /// [`with_runs`](Self::with_runs)), or the dispatch choke point could not
    /// mint a row and sent `run_id: None`. In both cases the card runs exactly
    /// as it did before this issue.
    fn open_trace(&self, run_id: Option<&str>) -> Option<Arc<RunTraceSink>> {
        let run_id = run_id?;
        let runs = self.runs.as_ref()?;
        Some(Arc::new(RunTraceSink::new(
            self.record().id.clone(),
            run_id,
            Arc::clone(runs),
        )))
    }

    /// Settles the attempt row from how its run ended, folding in the trace's
    /// step count and cost (issue #242).
    ///
    /// `parked_approvals` is how many approval requests **this attempt's own
    /// turns** left for a person to act on. A run that otherwise succeeded while
    /// parking at least one finishes [`RunStatus::WaitingApproval`] rather than
    /// [`RunStatus::Succeeded`] — epic #183 decision 2: a person must act, so
    /// the attempt is in review, not done. A run that failed, was cancelled or
    /// was paused keeps its own status; the operator has a bigger problem than a
    /// pending approval, and overwriting the reason it stopped would hide it.
    ///
    /// `WaitingApproval` is terminal-in-v1 (resuming an approved attempt is its
    /// own issue) and deliberately **re-enterable across attempts**: the
    /// re-dispatch after an approval is a new run that can wait again, which is
    /// what keeps #243's single-use, argument-exact grants coherent instead of
    /// forcing an operator to batch several approvals into one.
    async fn settle_run_end(
        &self,
        sink: Option<&RunTraceSink>,
        end: TaskRunEnd,
        result: &str,
        parked_approvals: usize,
    ) {
        let status = lifecycle::settled_run_status(end, parked_approvals);
        // Only a failure carries a reason: `error` is "why this went wrong", not
        // "what the agent said". Stamping a success's reply here would put the
        // deliverable in a field every reader renders as a fault.
        //
        // Issue #1861: a blocker carries one too. It is the same kind of
        // sentence — why the attempt stopped — and it is the only copy of the
        // question on the attempt row, so omitting it would leave the run
        // history saying an attempt stopped and refusing to say what for.
        let error = matches!(status, RunStatus::Failed | RunStatus::Blocked).then_some(result);
        self.settle_run(sink, status, error).await;
    }

    /// Writes one settle through to the run store, best-effort.
    ///
    /// Never propagates: the card is already persisted and the operator can
    /// already see the outcome by the time this runs, so a store fault must not
    /// fail the cycle — the same journal-after-persist rule
    /// [`journal_task_outcome`](Self::journal_task_outcome) follows. A run left
    /// unsettled by a failure here is still caught by the cycle's terminality
    /// backstop, and failing that by the boot reaper.
    async fn settle_run(
        &self,
        sink: Option<&RunTraceSink>,
        status: RunStatus,
        error: Option<&str>,
    ) {
        let (Some(sink), Some(runs)) = (sink, self.runs.as_ref()) else {
            return;
        };
        let outcome = RunOutcome {
            status,
            error: error.map(str::to_string),
            usage: sink.usage(),
            step_count: sink.step_count(),
        };
        if let Err(err) = runs
            .finish_run(&self.record().id, sink.run_id(), outcome)
            .await
        {
            tracing::warn!(
                company = %self.record().id,
                run = %sink.run_id(),
                error = %err,
                "[runs] could not settle an attempt row; the dispatch itself landed"
            );
        }
    }

    /// Runs the **one** follow-up turn that asks about unpublished files
    /// (issue #244), returning the agent's reply when it still published
    /// nothing.
    ///
    /// # Why it is a method with no loop in it
    ///
    /// The bound on nudges is structural. This runs exactly one turn, there is
    /// no iteration anywhere in it, and its single call site is straight-line
    /// code guarded by a local. A second nudge is not prevented by a counter
    /// that could be miscounted — there is nowhere to write one.
    ///
    /// # What it costs, and where that shows up
    ///
    /// One extra model turn on the same path, so its spend lands in the same
    /// usage ledger and counts against this agent's daily budget like any other
    /// turn. It is **distinguishable in the run trace** — its steps append to
    /// the same attempt after the primary reply — but it is **not separately
    /// labelled in the usage ledger**. Adding a provenance field to
    /// `UsageSample` is a real change to the metering surface and is out of
    /// scope here; saying so is better than half-doing it.
    ///
    /// # Failure is contained
    ///
    /// A provider fault logs a warning and returns `None`, falling through to
    /// the fallback warning. The run already completed its work and its reply
    /// has already been decided; a bookkeeping turn must never fail or delay it.
    ///
    /// The **per-agent daily cap** takes a different shape and needs saying:
    /// `HarnessPool::run` refuses an over-cap turn with `Ok(notice)` rather than
    /// an error, so a budget-refused nudge comes back as an ordinary reply and
    /// is recorded verbatim on the card — `unpublished: <files> — agent: <agent>
    /// has reached its daily spend cap of …`. That is left as-is on purpose:
    /// the line is *true*, it explains why the files were never reviewed, and
    /// the alternative is prose-matching the notice to special-case it, which is
    /// exactly the content-classifier this issue rules out. No model call is
    /// made either way, so the refusal costs nothing.
    ///
    /// An operator steer during the nudge discards the nudge's *reply* (no note
    /// line, no decline recorded) but keeps anything it published — losing a
    /// real deliverable to a cancelled bookkeeping turn would be the worse
    /// failure by a wide margin.
    #[allow(clippy::too_many_arguments)]
    async fn nudge_for_unpublished(
        &self,
        run_turn: &dyn RunTurn,
        responder: &str,
        brief: &str,
        reply: &str,
        unpublished: &[String],
        scan_partial: bool,
        control: &crate::company::steer::SteerControl,
        sink: Option<Arc<RunTraceSink>>,
    ) -> Option<String> {
        let instruction = publish::nudge_instruction(brief, reply, unpublished, scan_partial);
        let outcome = run_turn
            .run_steered_background(&self.record().id, responder, &instruction, control, sink)
            .await;
        // A steer that landed during the nudge is consumed here so it cannot
        // leak into a later `control.take()` and be mistaken for a steer of the
        // primary run, which has already ended.
        if let Some(action) = control.take() {
            tracing::info!(
                agent = %responder,
                action = ?action,
                "[publish] the operator steered during the publish nudge; its reply is discarded"
            );
            return None;
        }
        match outcome {
            Ok(outcome) => Some(outcome.reply),
            Err(err) => {
                tracing::warn!(
                    agent = %responder,
                    error = %err,
                    "[publish] the nudge turn did not run; falling through to the warning"
                );
                None
            }
        }
    }

    /// Journals a finished dispatch onto its card's timeline (issue #185): the
    /// run's reply, then the terminal anchor that closes the timeline.
    ///
    /// Every write is **best-effort** and the errors are logged, never
    /// propagated. The card is already persisted by the time this runs, so
    /// failing the cycle over a bookkeeping write would abandon the terminal
    /// anchor *and* the #151 post-back for a dispatch that has in fact landed —
    /// leaving a timeline stuck "still running" for a card the board already
    /// shows in its terminal column. Matches the existing journal-after-persist
    /// sites (`chat_and_emit`, `WorkflowCreated`, `TaskSteered`).
    async fn journal_task_outcome(
        &self,
        card: &TaskRecord,
        responder: &str,
        result_text: String,
        artifact_ids: Vec<String>,
    ) {
        let Some(events) = self.deps.events.as_ref() else {
            return;
        };
        // The run's reply, tagged so the per-task timeline can filter it out of
        // the company-scoped journal.
        //
        // `chat_id` is the **card id**, deliberately, not the card's origin
        // thread. `chat_history::owns` routes a reply into a desk's history by
        // matching `chat_id` against the desk id/name, so using the origin here
        // would inject this record into that desk's chat — a behaviour change
        // well outside a read foundation, and a duplicate of the live post-back
        // bubble the caller returns. A card id matches no desk, so the record
        // stays exactly what it is: timeline material, reachable only through
        // `task_id`. An empty string would be worse still — it folds into the
        // General desk.
        if let Err(err) = events
            .append(
                &self.record().id,
                CompanyEvent::AgentReply {
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    chat_id: card.id.clone(),
                    agent_id: responder.to_string(),
                    text: result_text.clone(),
                    steps: Vec::new(),
                    task_id: Some(card.id.clone()),
                },
            )
            .await
        {
            tracing::warn!(
                task_id = %card.id,
                error = %err,
                "[task] failed to journal dispatch reply; continuing"
            );
        }
        // The terminal anchor, journaled after the card's landing column is
        // persisted so it always records a completed run. Attempted even if the
        // reply above failed — the anchor is what closes a timeline, so dropping
        // it is strictly worse than dropping the reply.
        if let Err(err) = events
            .append(
                &self.record().id,
                CompanyEvent::DeskTaskCompleted {
                    task_id: card.id.clone(),
                    desk: responder.to_string(),
                    output: result_text,
                    column: card.column.clone(),
                    artifact_ids,
                    // Issue #377: the conversation this card was raised from,
                    // **captured** off the card rather than derived at
                    // completion. `responder` above is an agent id and a
                    // channel is a desk id, so the origin cannot be recovered
                    // from any other field on this event — and re-deriving it
                    // would put a second rule beside `chat_history`'s, which is
                    // the drift issue #435 exists to have removed.
                    //
                    // This is the one emission point every dispatch ending
                    // passes through (`run_task`, `refuse_dispatch`), which is
                    // why capturing it here cannot miss a path. A board-created
                    // card carries `None` and gets no channel marker: no
                    // conversation raised it.
                    origin_chat_id: card.origin_chat_id.clone(),
                },
            )
            .await
        {
            tracing::warn!(
                task_id = %card.id,
                error = %err,
                "[task] failed to journal task completion; continuing"
            );
        }
    }

    /// The company orchestrator's agent id — the single voice that answers the
    /// operator (issue #186).
    ///
    /// Resolved from the roster rather than read off [`Self::responder`],
    /// because `with_responder` can point that at any agent for a test or a
    /// single-desk company; the relay must still be attributed to the real
    /// orchestrator. Falls back to `responder` only when the roster has no
    /// orchestrator to name at all, which is the same empty-roster case
    /// [`orchestrator::orchestrator_id`] already tolerates.
    fn orchestrator(&self) -> String {
        orchestrator::orchestrator_id(&self.record().effective_agents())
            .unwrap_or_else(|| self.responder.clone())
    }

    /// This attempt's 1-based ordinal, for the card's operator-facing link
    /// label (issue #339) — *"attempt 2"*.
    ///
    /// One extra read on a successful settle, and deliberately not free-ridden
    /// off the `finish_run` that settles the attempt row: that runs *after* the
    /// card is written, and the card is what needs this. Reordering the settle
    /// to harvest it would put the run row's terminal status ahead of the
    /// board's, contradicting the ordering #242 chose on purpose.
    ///
    /// Best-effort: `None` on an unwired store, a missing row or a read fault.
    /// The ordinal is a **label**, never an identity —
    /// [`TaskOutput::run_id`](crate::ports::tasks::TaskOutput::run_id) still
    /// addresses the attempt — so a failure here costs a nicety in the link
    /// text and never the link.
    async fn attempt_ordinal(&self, run_id: &str) -> Option<u32> {
        let runs = self.runs.as_ref()?;
        match runs.get_run(&self.record().id, run_id).await {
            Ok(run) => run.map(|run| run.attempt),
            Err(err) => {
                tracing::warn!(
                    company = %self.record().id,
                    run = %run_id,
                    error = %err,
                    "[runs] could not read an attempt's ordinal for the card's link; the link \
                     still names the run"
                );
                None
            }
        }
    }

    /// Records everything the run published as versioned artifacts, returning
    /// one reference per artifact **pinned at the version this run wrote**
    /// (issues #244, #339).
    ///
    /// # Why the version comes back
    ///
    /// The caller stamps these onto the card, and a card link that named only
    /// the artifact would re-point at whatever a human last edited — silently
    /// turning "what this task produced" into "what the artifact says now".
    /// `push_version` already computes the number; before #339 it was
    /// discarded.
    ///
    /// # Extend by identity, never by recency
    ///
    /// The record to extend is the one on this card whose `source` equals the
    /// published path. That is the correction at the heart of this issue.
    ///
    /// The old rule was `max_by_key(updated_at_millis)` — extend whichever
    /// artifact on the card was touched most recently. An **operator edit**
    /// bumps `updated_at_millis`, so editing the invoice made the invoice the
    /// target for the next agent write to the spec: the spec's v3 landed as the
    /// invoice's v4, and `human_edit_diff` then reported an operator rewriting
    /// a document they had never seen. Since that diff is the entire purpose of
    /// the artifact port, recency did not merely mis-file records — it
    /// fabricated the one number the product exists to measure.
    ///
    /// A path that has never been published opens a new record; a rename starts
    /// a new lineage, which is a limitation named on
    /// [`ArtifactRecord::source`](crate::ports::artifacts::ArtifactRecord::source)
    /// rather than papered over with a guess.
    ///
    /// # Errors propagate — to the caller, which now contains them
    ///
    /// Deliberately, and this was a change in #244. The pre-#244 path returned
    /// a silent `Ok(())` when the store was missing and swallowed nothing else,
    /// which meant a failed write to a deliverable an agent had explicitly
    /// published was indistinguishable from success. An explicit publish that
    /// could not be stored is a real failure of the run and the operator needs
    /// to see it, so this still surfaces one.
    ///
    /// What changed in #339 is where that failure stops. This now runs
    /// **before** the card's single write, so `run_task` logs the error at
    /// `error` and settles the card anyway rather than propagating — a
    /// bookkeeping fault must not strand a finished card in `in_progress`. The
    /// error is still raised here; it is simply no longer fatal there.
    ///
    /// A **missing store** is different: `publish_artifact` is not wired at all
    /// without one (see `build.rs`), so a non-empty queue here means something
    /// upstream is misconfigured. It warns loudly rather than failing the cycle,
    /// because the turn's actual work is already done and persisted.
    ///
    /// `run_id` stamps the revision this call writes (#242) so a run row can
    /// point at what it actually produced. An earlier attempt's version keeps
    /// the attempt that wrote *it*.
    ///
    /// # Authorship is per file, not per call (issue #463)
    ///
    /// Each revision records the agent that published **that file**, read from
    /// [`PendingPublish::agent`]. `responder` is only the fallback, for a value
    /// built by hand rather than by the tool.
    ///
    /// One drain can hold publishes from more than one agent — the desk lead's
    /// turn and the orchestrator's own turn both run with the full toolbelt
    /// under a single `Conversation` claim — so a single author applied to the
    /// batch stamps one agent's name on another's file. The card above still
    /// takes one owner, because a card has one; a revision is a different
    /// question with a different answer.
    async fn record_published_artifacts(
        &self,
        card: &TaskRecord,
        responder: &str,
        published: Vec<publish::PendingPublish>,
        run_id: Option<&str>,
    ) -> Result<Vec<TaskOutputArtifact>> {
        if published.is_empty() {
            // The honest, common case: this run produced no file. There is no
            // artifact, and the run trace is the addressable record of what
            // happened.
            return Ok(Vec::new());
        }
        let Some(artifacts) = self.deps.artifacts.as_ref() else {
            tracing::warn!(
                task_id = %card.id,
                staged = published.len(),
                "[publish] files were published but no artifact store is configured; the tool \
                 should not have been wired — nothing was recorded"
            );
            return Ok(Vec::new());
        };

        let mut on_card = artifacts.list(&self.record().id, Some(&card.id)).await?;
        let mut written = Vec::with_capacity(published.len());
        for pending in published {
            let at = now_millis();
            // Issue #463: whoever published THIS file. `responder` is the
            // fallback for a `PendingPublish` not built by the tool — the tool
            // always stamps its own agent.
            let author = match pending.agent.trim() {
                "" => responder,
                agent => agent,
            };
            // Identity, not recency: the record whose `source` is this exact
            // path, or a new one.
            let existing = on_card
                .iter()
                .position(|a| a.source.as_deref() == Some(pending.source.as_str()));
            // The revision THIS run wrote (#339). A fresh record is always its
            // own v1; an extended one takes whatever `push_version` numbered.
            let mut version = 1;
            // The node the PREVIOUS version was mirrored into, read before the
            // push below appends a version whose own node is not chosen yet.
            let mut prior_node = None;
            let mut record = match existing {
                Some(index) => {
                    let mut found = on_card.remove(index);
                    prior_node = found.workspace_node_id().map(str::to_string);
                    version = found.push_version(
                        pending.payload.artifact_body(),
                        ArtifactAuthor::Agent,
                        author,
                        at,
                        pending.note.clone(),
                    );
                    // A republished file may have changed shape — a markdown
                    // draft exported as a PDF, a small file grown past the
                    // inline cap. The record follows what was actually
                    // captured, or the console renders the new version with the
                    // old version's renderer.
                    found.kind = pending.kind;
                    found
                }
                None => {
                    let mut fresh = ArtifactRecord::new(
                        generate_id(),
                        &card.id,
                        &pending.title,
                        pending.kind,
                        pending.payload.artifact_body(),
                        author,
                        at,
                    )
                    .with_source(pending.source.clone());
                    if let Some(note) = pending.note.clone()
                        && let Some(first) = fresh.versions.first_mut()
                    {
                        first.note = Some(note);
                    }
                    fresh
                }
            };
            if let Some(run_id) = run_id {
                record.stamp_run(run_id);
            }
            // Issue #552: the deliverable also goes into the shared workspace
            // tree, which is the one surface the operator browses and every
            // other agent can read. The artifact chain here stays the
            // authoritative version history; the node holds the current body.
            //
            // # Chain first, without exception
            //
            // A re-publish inherits the node the previous version named, so the
            // version can be written *before* the tree is touched. That
            // ordering is the load-bearing half of keeping the chain
            // authoritative, not a preference: a node one version ahead of the
            // chain is the tree showing content the version history has no
            // record of, which makes `human_edit_diff` quietly wrong rather
            // than loudly broken — the same #187 rot arriving by a different
            // door. Requiring the store to half-fail bounds how *often* that
            // happens and not at all how bad it is, and on a data path a silent
            // wrong answer outlives the incident that caused it.
            //
            // A *fresh* publish has no node id to inherit, so its v1 is stored
            // unlinked and the link is stamped by the second upsert below. Note
            // what that buys beyond ordering: because the record is written
            // first, a node is only ever created for a deliverable that is
            // already recorded, so this path can no longer leave a node in the
            // tree with no artifact behind it at all.
            if let Some(node_id) = prior_node.as_deref() {
                // Inherit before storing, so a failure anywhere below leaves
                // the version pointing at the node that currently holds it.
                record.stamp_workspace_node(node_id);
            }
            artifacts.upsert(&self.record().id, &record).await?;

            // **A failed mirror does not lose the deliverable.** An explicit
            // publish that could not be filed into the tree is still recorded
            // as an artifact — dropping a produced file over tree bookkeeping
            // would be far worse than a deliverable the operator has to reach
            // through the Artifacts tab. So this logs at `error` (loudly: the
            // tree is where people look) and leaves the version unlinked, which
            // is exactly what a pre-#552 record carries. The next publish of
            // the same source retries and heals it.
            if let Some(workspace) = self.deps.workspace.as_ref() {
                let target = artifact_mirror::PublishTarget {
                    agent_id: author,
                    task_id: &card.id,
                    // Issue #1687: the folder the deliverable lands in is
                    // named for the work, not only keyed by it. The card is
                    // right here and its title is the one string that says
                    // what an operator is looking at.
                    task_title: Some(card.title.as_str()),
                    source: &pending.source,
                    payload: match &pending.payload {
                        crate::harness::publish::PublishPayload::Text(text) => {
                            artifact_mirror::MirrorPayload::Text(text)
                        }
                        crate::harness::publish::PublishPayload::Bytes { bytes, mime } => {
                            artifact_mirror::MirrorPayload::Bytes { bytes, mime }
                        }
                    },
                    existing_node_id: prior_node.as_deref(),
                };
                match artifact_mirror::materialize(workspace.as_ref(), &self.record().id, target)
                    .await
                {
                    Ok(mirrored) => {
                        let node_id = mirrored.node_id;
                        // Issue #663/#668: the version body was composed before
                        // the store was asked, so it describes an outcome that
                        // had not happened. Now it has — say what it was, and
                        // record the digest the STORE computed so two versions
                        // of one binary can be told apart.
                        //
                        // Always re-composed, never conditional on the link
                        // having changed: an ordinary re-publish reuses its node
                        // and would otherwise keep the previous version's
                        // digest, which is precisely the "identical string"
                        // failure #668 describes.
                        let stored = pending.payload.artifact_body_for(
                            crate::harness::publish::PayloadStorage::Stored {
                                sha256: mirrored.sha256.as_deref(),
                            },
                        );
                        // Only when it actually says something new. Prose is its
                        // own body, so a text re-publish composes the identical
                        // string and still stores once — the contract
                        // `an_ordinary_republish_writes_the_artifact_once`
                        // pins. A binary's body gains the store's digest, so it
                        // differs and is worth the second write: without it the
                        // version would keep the PREVIOUS digest, which is the
                        // indistinguishable-versions defect (#668) with an extra
                        // step.
                        let body_changed =
                            record.latest().is_some_and(|latest| latest.body != stored);
                        if body_changed {
                            record.amend_latest_body(stored);
                        }
                        let relinked = record.workspace_node_id() != Some(node_id.as_str());
                        if relinked {
                            record.stamp_workspace_node(&node_id);
                        }
                        // A second write only when the record actually changed:
                        // a fresh publish, a re-publish whose node the operator
                        // deleted, or a body that now carries an outcome it did
                        // not before. Warn rather than `?` for the unchanged
                        // reason — BOTH surfaces already hold this body and only
                        // the record's copy is stale, so failing the batch would
                        // discard the remaining publishes' records to report
                        // something the next publish repairs.
                        if (body_changed || relinked)
                            && let Err(err) = artifacts.upsert(&self.record().id, &record).await
                        {
                            tracing::warn!(
                                task_id = %card.id,
                                source = %pending.source,
                                node = %node_id,
                                error = %err,
                                "[publish] the deliverable and its note are both stored but the \
                                 record could not be updated; the next publish of this source \
                                 re-adopts the note and repairs it"
                            );
                        }
                    }
                    Err(err) => {
                        // Issue #663. The record already claimed this file was
                        // filed into the workspace. It was not, so the claim is
                        // withdrawn rather than left standing — an operator who
                        // opens the artifact and reads "open it there" and finds
                        // nothing is the dangling-record failure #553 set out to
                        // remove, arriving through the error path.
                        //
                        // The store's error is logged and NOT written to the
                        // record: a version body is permanent and a backend
                        // error can name host paths.
                        tracing::error!(
                            task_id = %card.id,
                            agent = %author,
                            source = %pending.source,
                            error = %err,
                            "[publish] could not put the published file into the company \
                             workspace; the artifact record says so rather than promising a \
                             file that is not there"
                        );
                        record.amend_latest_body(
                            pending.payload.artifact_body_for(
                                crate::harness::publish::PayloadStorage::Refused,
                            ),
                        );
                        if let Err(err) = artifacts.upsert(&self.record().id, &record).await {
                            tracing::error!(
                                task_id = %card.id,
                                source = %pending.source,
                                error = %err,
                                "[publish] the workspace refused the file AND the record could \
                                 not be corrected; it still claims the file is stored"
                            );
                        }
                    }
                }
            }
            written.push(TaskOutputArtifact {
                artifact_id: record.id.clone(),
                version,
                title: record.title.clone(),
                kind: record.kind,
            });
            // Keep the working set current so two publishes of the same path in
            // one run extend one record rather than opening two.
            on_card.push(record);
        }
        Ok(written)
    }

    /// Files one drained batch of conversation publishes onto the right card —
    /// same destination rule (`spawned_task` vs a fresh card), same
    /// publisher-attribution fallback, same failure escalation into the
    /// operator's own reply (issue #445) — and returns the card it landed on.
    ///
    /// `claimed` mirrors the belt-and-suspenders guard every call site already
    /// used inline (`!published.is_empty() && publish_claim.is_some()`): an
    /// unclaimed queue can only ever drain empty, so this is defensive rather
    /// than load-bearing, but it keeps both conditions visible together instead
    /// of only at the call site.
    ///
    /// Extracted for issue #989: a capped chat turn's unpublished-work nudge
    /// (below) is a **second** turn that can ALSO publish, staged into the same
    /// queue while the same claim is still live. Filing its batch through this
    /// exact path — rather than a re-derived one — is what keeps a publish made
    /// during the nudge from being the one publish in this whole module that
    /// silently drops on the floor.
    async fn file_conversation_batch(
        &self,
        responder: &str,
        spawned_task: Option<&str>,
        chat_id: Option<&str>,
        claimed: bool,
        published: Vec<publish::PendingPublish>,
        operator_reply: &mut String,
    ) -> Option<String> {
        if published.is_empty() || !claimed {
            return None;
        }
        let count = published.len();
        // Issue #463: the agent that actually called the tool, not the turn's
        // responder. A desk lead publishing inside a hand-off used to be filed
        // under the orchestrator that relayed for it — the card named the wrong
        // person for work it did not do.
        //
        // The **card** takes one owner, so one agent is picked; each artifact
        // keeps its own author further down, in `record_published_artifacts`,
        // because a turn can stage publishes from more than one agent.
        let publisher = published
            .first()
            .map(|p| p.agent.clone())
            .filter(|agent| !agent.is_empty())
            .unwrap_or_else(|| responder.to_string());
        // Issue #463: file onto the card THIS message already opened when
        // there is one. Minting is the no-card-in-scope case, and the fallback
        // inside `file_publishes_on_card` for a card deleted mid-turn.
        let filed = match spawned_task {
            Some(card_id) => {
                self.file_publishes_on_card(card_id, &publisher, chat_id, published)
                    .await
            }
            None => {
                self.record_conversation_publishes(&publisher, chat_id, published)
                    .await
            }
        };
        match filed {
            Ok(card_id) => Some(card_id),
            Err(err) => {
                // The agent has already been told the file was published, and
                // that receipt is now wrong. This is the one remaining way that
                // can happen — a store write failing under a claim that was
                // honestly made — so it is said out loud in the conversation
                // rather than left in a log the operator will never read.
                // Saying nothing here would reproduce #445 exactly: a
                // confident delivery claim over nothing recorded.
                tracing::error!(
                    agent = %responder,
                    staged = count,
                    error = %err,
                    "[publish] a conversation published files but they could not be recorded; \
                     telling the operator in the reply"
                );
                operator_reply.push_str(&publish::recording_failed_notice(count));
                None
            }
        }
    }

    /// Files what a conversation turn published onto the card that turn already
    /// opened (issue #463). Returns that card's id.
    ///
    /// # Why this exists at all
    ///
    /// #445 made a chat publish mint a card, which was right for a publish with
    /// nothing else in scope and wrong the moment #442 started opening a card
    /// for the work itself: one substantial ask that ended in a published file
    /// produced two cards, and the reply linked to the one with no artifacts on
    /// it. Both fixes were correct alone. Together they doubled, and the
    /// deliverable ended up on the card nothing pointed at.
    ///
    /// So a publish files onto the card in scope instead of opening a rival to
    /// it. The card already carries the request and the answer; this adds the
    /// artifact and says who delivered it.
    ///
    /// # What it changes on the card, and what it leaves alone
    ///
    /// The note gains a line naming the published files. The column moves to
    /// [`COLUMN_IN_REVIEW`] — a deliverable was produced and a person has not
    /// accepted it yet, the same landing `record_conversation_publishes` gives
    /// its minted card and the same one a settled run gets. A card with **no
    /// assignee** — the To-do card the REST chat handler opens, which has never
    /// belonged to anybody — is assigned to the publisher; a card that already
    /// has an owner keeps them, because filing a file must not quietly take
    /// somebody's work away from them.
    ///
    /// A card that has since been deleted falls back to minting, so the
    /// artifact stays reachable rather than being dropped for the sake of the
    /// rule. **The returned id is the card the deliverable actually landed
    /// on** — the replacement, on that path, not `card_id` — because the caller
    /// links the operator's reply to it and sending them to an id that no
    /// longer resolves is the bug this whole change is about.
    ///
    /// `chat_id` is carried into that fallback so a minted replacement points
    /// back at the same conversation the no-card-in-scope path's card does;
    /// two minting paths must not differ in where their card posts back.
    async fn file_publishes_on_card(
        &self,
        card_id: &str,
        agent: &str,
        chat_id: Option<&str>,
        published: Vec<publish::PendingPublish>,
    ) -> Result<String> {
        let Some(tasks) = self.deps.tasks.as_ref() else {
            return Err(crate::OpenCompanyError::Harness(
                "a conversation published a file but no task board is wired".to_string(),
            ));
        };
        let Some(mut card) = tasks
            .list(&self.record().id)
            .await?
            .into_iter()
            .find(|card| card.id == card_id)
        else {
            tracing::warn!(
                task_id = %card_id,
                agent = %agent,
                "[publish] the card this turn opened is gone; minting one for the deliverable \
                 instead of dropping it"
            );
            return self
                .record_conversation_publishes(agent, chat_id, published)
                .await;
        };

        let recorded = self
            .record_published_artifacts(&card, agent, published.clone(), None)
            .await?;
        card.note = Some(append_result(
            card.note.as_deref(),
            agent,
            &publish::filed_on_card_note(&published),
        ));
        card.column = COLUMN_IN_REVIEW.to_string();
        if card.assignee.is_empty() {
            card.assignee = agent.to_string();
        }
        card.updated_at_millis = now_millis();
        tasks.upsert(&self.record().id, &card).await?;
        tracing::info!(
            task_id = %card.id,
            agent = %agent,
            artifacts = recorded.len(),
            "[publish] a conversation published files onto the card this message already opened"
        );
        Ok(card.id)
    }

    /// Records what a **conversation** turn published, minting the card that
    /// carries it (issue #445). Returns that card's id.
    ///
    /// # Why a card, rather than a company-level artifact
    ///
    /// The issue allows either: a chat deliverable becomes an artifact attached
    /// to no card, or the act of publishing mints the card. This path takes the
    /// second, and the deciding argument is *reachability* — which is, after
    /// all, the entire bug.
    ///
    /// An [`ArtifactRecord`] carries a non-optional `task_id`, `(task_id,
    /// source)` **is** its identity, the only route that lists artifacts is
    /// `GET /tasks/{task_id}/artifacts`, and the only console surface that
    /// renders one is the per-task Artifacts tab. A card-less artifact would
    /// therefore need an optional `task_id` (breaking the identity contract), a
    /// new company-scoped route, and a new console view — and until that last
    /// piece shipped, the artifact would be recorded and still unreachable,
    /// which is precisely the failure being fixed, merely moved one layer down.
    /// Minting the card reuses a path the operator can already open today.
    ///
    /// It is also honest about what happened rather than a workaround: an agent
    /// that produced a deliverable did a unit of work, and a board that shows it
    /// is more accurate than one that does not. The card lands in
    /// [`COLUMN_IN_REVIEW`] because that is where the lifecycle already puts
    /// finished agent work awaiting a person — `COLUMN_DONE` is reached only by
    /// a human accepting it, and this fix does not get to decide that on their
    /// behalf.
    ///
    /// # What it deliberately does not do
    ///
    /// No `output` stamp. That field pins a `run_id` and an attempt ordinal, and
    /// a chat turn has neither — inventing one would put a fabricated attempt on
    /// a card to make a field look populated. The artifacts are reachable
    /// through the tab regardless; an invented run id would not be true.
    async fn record_conversation_publishes(
        &self,
        responder: &str,
        chat_id: Option<&str>,
        published: Vec<publish::PendingPublish>,
    ) -> Result<String> {
        let Some(tasks) = self.deps.tasks.as_ref() else {
            // Unreachable while the claim is only taken with both stores wired,
            // and an error rather than a silent `Ok` so it stays unreachable:
            // the caller surfaces this to the operator instead of dropping the
            // deliverable the way #445 did.
            return Err(crate::OpenCompanyError::Harness(
                "a conversation published a file but no task board is wired".to_string(),
            ));
        };

        let card = TaskRecord {
            id: generate_id(),
            title: publish::conversation_card_title(&published),
            note: Some(publish::conversation_card_note(responder, &published)),
            // Finished agent work a person has not accepted yet — the same
            // landing `column_for_settled_run(Succeeded)` gives a dispatched run.
            column: COLUMN_IN_REVIEW.to_string(),
            priority: "medium".to_string(),
            assignee: responder.to_string(),
            updated_at_millis: now_millis(),
            // The conversation this came out of, so the card points back at the
            // thread that produced it (#151 §3.2's field, same meaning).
            origin_chat_id: chat_id.map(str::to_string),
            // A chat turn has no card in scope, so this is a lineage root —
            // the same `None` a `spawn_task` from an ordinary chat turn writes.
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        };
        // The card is written **first**: an artifact's `task_id` must name a
        // card that exists. If the artifact writes then fail, the failure
        // direction is a visible card whose note explains what it was for —
        // recoverable, and the operator is told below. The reverse order would
        // leave artifacts pointing at a card that was never created, which is
        // unreachable by every route and indistinguishable from the original
        // bug.
        tasks.upsert(&self.record().id, &card).await?;

        // No run id: there is no attempt row behind a chat turn, and
        // `stamp_run` is skipped rather than given something invented.
        let recorded = self
            .record_published_artifacts(&card, responder, published, None)
            .await?;
        tracing::info!(
            task_id = %card.id,
            agent = %responder,
            artifacts = recorded.len(),
            "[publish] a conversation published files; minted a card to carry them"
        );
        Ok(card.id)
    }

    /// Resolves which agent answers an operator message.
    ///
    /// Resolution order, and the order matters:
    ///
    /// 1. a **desk** (the `chat` field naming a group chat with a lead member) is
    ///    answered by that desk's lead — unchanged;
    /// 2. a **roster teammate id** is answered by that teammate directly, which
    ///    is what makes a per-agent DM thread possible (issue #151 §3.3);
    /// 3. everything else — the "General" desk, an unknown id, an unaddressed
    ///    message — goes to the orchestrator, as before.
    ///
    /// Desks are tried first so a desk whose id happens to match an agent id
    /// keeps routing as a desk; the DM case only ever claims ids that resolve to
    /// no desk at all. The console's `dm:<teammate-id>` channel key is resolved
    /// last, after both (issue #982) — see the comment on that arm. Without step 2 a DM thread would silently reach the
    /// orchestrator instead of the teammate the operator opened — the console
    /// would look like it were addressing an agent while talking to someone
    /// else.
    ///
    /// # Step 2 resolves the key rather than matching it exactly (issue #884)
    ///
    /// `chat` is a **human-and-console-typed** key: the console mints it from a
    /// `TeamMember.id`, an operator can type one into a URL, and an audit script
    /// can post one straight to the API. Matching it with the exact,
    /// case-sensitive [`CompanyRecord::is_roster_agent`] meant any drift at all —
    /// a capital letter, a console id that differs from the manifest id — read as
    /// *unaddressed* and fell to arm 3, where an agent nobody asked answered
    /// confidently and nothing said so.
    /// [`CompanyRecord::resolve_roster_agent_id`] is documented as the resolver
    /// for exactly this case, and returning its **canonical** id (rather than the
    /// key as typed) is what stops the persona lookup one layer up in
    /// [`HarnessBrain::agent_for`](crate::harness::HarnessBrain) missing on the
    /// same difference. `is_roster_agent` keeps its exact-match contract for the
    /// machine-written desk overlay.
    ///
    /// Case-folding can only claim keys that resolve to nothing today, so no
    /// existing thread moves. The one behaviour it introduces: two roster ids
    /// differing **only** by case make a mixed-case key order-dependent —
    /// `resolve_roster_agent_id` returns the first match, manifest agents before
    /// overlay ones. That roster is already ambiguous for every other typed key
    /// (a card assignee resolves the same way), so this does not add a namespace
    /// problem, it inherits one.
    ///
    /// # The fall-through warns (issue #884)
    ///
    /// Arm 3 answers as the orchestrator whether the message was unaddressed or
    /// addressed to something that does not exist, and those are very different
    /// facts. The log line is what makes the second one a greppable event in a
    /// tenant's log instead of a silent wrong-agent answer.
    fn responder_for(&self, chat: Option<&str>) -> String {
        let Some(chat) = chat else {
            return self.responder.clone();
        };
        // The four arms — desk lead, bare roster id, then the console's
        // `dm:<teammate-id>` key tried both ways (issue #982, step 3) — live on
        // the brain-agnostic seam since issue #1725, so the cycle's small-talk
        // fast path attributes its reply to the same teammate a turn would have.
        // The DM arm stays LAST there for the reason it was last here: it can
        // only claim a key that resolves to nothing today, so no existing
        // thread moves, and a company that really does have a desk or teammate
        // called `dm:x` keeps it.
        if let Some(responder) =
            crate::runtime::delegation_tools::chat_responder(&self.record(), chat)
        {
            return responder;
        }
        // The built-in `#general` channel (issue #1743) — the one key that
        // resolves to nobody *on purpose*. `chat_responder` declines it so both
        // callers answer as their own orchestrator, which is what this host has
        // always done for the company's main line. It is not the #884 case the
        // warning below exists for: nothing was misaddressed, so logging it
        // would bury the real misroutes under the console's most-used channel.
        if crate::server::chat_history::is_general_chat(Some(chat)) {
            return self.responder.clone();
        }
        tracing::warn!(
            company = %self.record().id,
            chat = %chat,
            responder = %self.responder,
            "[chat] addressed thread key matches no desk and no roster teammate; the \
             orchestrator is answering a message that may not have been meant for it"
        );
        self.responder.clone()
    }

    /// The desk key `@everyone` expands against for a message addressed to
    /// `chat`. Folds the General-desk spellings [`is_general_chat`] admits
    /// (`None`, `""`, `"main"`, `"General"`) to [`DEFAULT_DESK`], so a
    /// broadcast from the console's default thread — which sends
    /// `chat: "main"`, an alias `resolve_desk_id` does not know — expands
    /// against the General desk rather than no desk at all.
    ///
    /// **A real desk answering to that key wins, whatever it is spelled like.**
    /// A blueprint may declare `[[group_chat]] id = "main"` (or `"general"`),
    /// which this host grandfathers — `is_general_channel` is guarded on
    /// `!desk_exists`, so the desk keeps its members and `responder_for` routes
    /// to its lead. Folding that key to `General` asks `resolve_desk_id` for a
    /// name no such desk has, which misses, and `@everyone` then expands to the
    /// **whole roster** instead of the desk that was actually addressed — a
    /// broadcast escaping the scope of the one case the fold exists to keep
    /// working. Asking the record first costs one lookup and cannot be wrong.
    fn everyone_desk(record: &CompanyRecord, chat: Option<&str>) -> String {
        match chat {
            Some(chat)
                if record.resolve_desk_id(chat).is_some()
                    || !crate::server::chat_history::is_general_chat(Some(chat)) =>
            {
                chat.to_string()
            }
            // A General alias resolves to whichever desk claims the line, not
            // to the literal `DEFAULT_DESK`. A blueprint desk declared
            // `id = "main", name = "Front office"` claims it by id, so
            // `resolve_desk_id("General")` misses and the guard above falls
            // through — and expanding `@everyone` against a desk called
            // `General` that does not exist scoped the broadcast to the entire
            // roster, while the channel it was posted in is that desk and its
            // lead answers there. The alias and the raw key have to name the
            // same membership or `@everyone` means two different things in one
            // channel (issue #1743).
            _ => crate::runtime::delegation_tools::general_claimant(record)
                .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string()),
        }
    }

    /// Drains the MCP failure queue **onto the operator bubble's step timeline**
    /// as error steps (the Activity-trace re-skin of the error-hardening cell's
    /// original fallback bubble), and journals a scrubbed
    /// [`CompanyEvent::McpCallFailed`] audit event per failure when the event log
    /// is wired.
    ///
    /// One surface, one renderer, one scrub discipline: a silently-failed MCP
    /// call shows up as a red step in the same timeline as every other tool call
    /// instead of a separate warning bubble. Every string was already scrubbed at
    /// the source (`OcMcpCallTool`), so `scrubbed_message` is safe to show and to
    /// persist.
    ///
    /// `task_id` is the dispatched card the failing turn belonged to, when the
    /// drain runs inside a [`CompanyEvent::TaskDispatched`] cycle (issue #185).
    /// It is stamped onto each journaled failure so a task's broken tool calls
    /// can be filtered out of the company-scoped journal onto its own timeline;
    /// a chat turn passes `None` and journals exactly as before.
    async fn surface_mcp_failures(
        &self,
        steps: &mut Vec<TurnStep>,
        task_id: Option<&str>,
    ) -> Result<()> {
        for failure in self.deps.mcp_failures.drain() {
            steps.push(TurnStep {
                kind: TurnStepKind::Note,
                status: TurnStepStatus::Error,
                label: format!("MCP: {} unavailable", failure.server),
                detail: Some(failure.scrubbed_message.clone()),
                elapsed_ms: None,
                ..TurnStep::default()
            });
            if let Some(events) = self.deps.events.as_ref() {
                // Best-effort **per failure**. `drain` is a `mem::take`, so the
                // queue is already empty by the time this loop runs and the
                // batch exists only in this iterator. Propagating with `?` here
                // would discard every failure after the first journal error —
                // permanently, since nothing remains to retry from. A failed
                // audit write must not cost us the rest of the audit.
                let server = failure.server.clone();
                if let Err(err) = events
                    .append(
                        &self.record().id,
                        CompanyEvent::McpCallFailed {
                            task_id: task_id.map(str::to_string),
                            server: failure.server,
                            tool: failure.tool,
                            status: failure.status,
                            message: failure.scrubbed_message,
                        },
                    )
                    .await
                {
                    tracing::warn!(
                        server = %server,
                        task_id = task_id.unwrap_or("-"),
                        error = %err,
                        "[task] failed to journal an MCP failure; draining the rest"
                    );
                }
            }
        }
        Ok(())
    }

    /// Decides how a failed dispatch should settle (issue #1861): as a blocker
    /// the operator can answer, or as the plain failure it always was.
    ///
    /// The one place the two settle sites ask the question, so `run_task`
    /// cannot classify a hand-off failure by one rule and a dispatch failure by
    /// another.
    ///
    /// A [`Transient`](crate::ports::blockers::BlockerKind::Transient)
    /// classification returns [`TaskRunEnd::Failed`] like an unrecognised one:
    /// recognising a rate limit tells us **not** to ask anybody about it.
    fn settle_as_blocker_or_failure(
        &self,
        task_id: &str,
        reason: &str,
        run_id: Option<&str>,
    ) -> TaskRunEnd {
        match crate::harness::built_in::blockers::classify_blocker_message(reason) {
            Some(class) => self.queue_blocker(
                class,
                BlockerStep::Task {
                    task_id: task_id.to_string(),
                },
                reason,
                run_id,
            ),
            None => TaskRunEnd::Failed,
        }
    }

    /// Queues a blocker for the operator, or reports that this failure is not
    /// one (issue #1861).
    ///
    /// Returns the ending the caller should settle with:
    /// [`TaskRunEnd::Blocked`] when the stop was recognised as answerable by a
    /// person, and [`TaskRunEnd::Failed`] — today's behaviour, unchanged — for
    /// everything else.
    ///
    /// # Why this rides the approval-request queue
    ///
    /// A blocker needs exactly what a gated tool call needs: a durable park
    /// that survives a restart, a continuation armed against this cycle, and an
    /// entry on the operator's queue. All three already happen, once, in
    /// [`park_approval_requests`](Self::park_approval_requests) →
    /// [`CycleHost::park_effect`]. Pushing onto the same queue inherits them
    /// instead of standing up a second park path that would have to be kept in
    /// step with the first.
    ///
    /// The ordering that makes it work: `run_task` runs inside the cycle's
    /// event loop, and the drain is after it, so a blocker queued here is
    /// parked before this cycle ends.
    ///
    /// # Approving one does nothing, on purpose
    ///
    /// The effect carries no `amount_usd`, no `channel`/`text` pair and a kind
    /// no executor matches, so `perform_effect` falls through it — and
    /// [`agent`](Effect::agent) is `None`, so no single-use grant is minted and
    /// no re-dispatch is attempted. That is the intended v1 boundary: #1861
    /// makes the stop durable, visible and expirable; carrying the operator's
    /// *answer* back into the stopped turn is #1863. Stamping `agent` here
    /// instead would re-dispatch the agent to call `escalate_to_human` again,
    /// which would park again.
    fn queue_blocker(
        &self,
        class: crate::harness::built_in::blockers::BlockerClass,
        step: BlockerStep,
        reason: &str,
        run_id: Option<&str>,
    ) -> TaskRunEnd {
        if !class.kind.parks() {
            return TaskRunEnd::Failed;
        }
        let payload = BlockerPayload {
            kind: class.kind,
            source: class.source,
            step: Some(step),
            reason: reason.to_string(),
            needed: class.needed.to_string(),
        };
        let effect = Effect {
            kind: payload.effect_kind(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            // Serialization cannot fail for this shape; an empty payload would
            // still park correctly (the kind carries the gap class), so a
            // fallback beats refusing to ask.
            payload: serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null),
            agent: None,
            run_id: run_id.map(str::to_string),
        };
        self.deps
            .approval_requests
            .push(crate::harness::built_in::policy::ApprovalRequest {
                tool: payload.kind.effect_kind(),
                reason: reason.to_string(),
                effect,
            });
        TaskRunEnd::Blocked
    }

    /// Drains the approval-request queue and parks each request on the host's
    /// approval gate, so an approval-gated tool call the agent hit during this
    /// cycle reaches the operator's Approvals page (issue #172).
    ///
    /// The missing half of the approval path. openhuman resolves a
    /// `RequireApproval` **inline** — it blocks the tool and narrates the
    /// refusal to the model — so nothing downstream of the turn ever learned a
    /// request existed and `journal.pending()` stayed empty. The
    /// [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) now records
    /// each blocked call on the shared queue; this drains it once per cycle and
    /// parks it through
    /// [`CycleHost::park_effect`](crate::ports::brain::CycleHost::park_effect).
    ///
    /// Parked, not re-evaluated:
    /// [`emit_effect`](crate::ports::brain::CycleHost::emit_effect) would
    /// re-decide the request against the runtime
    /// [`ApprovalGate`](crate::ports::ApprovalGate), which allows (and therefore
    /// "executes") anything it classifies as
    /// [`EffectGroup::Other`](crate::ports::types::EffectGroup::Other) — most
    /// gated tool calls — and the request would disappear again. The verdict was
    /// already reached inside the turn; the runtime's job here is only to hold
    /// it for the operator.
    ///
    /// Bounded by
    /// [`MAX_APPROVAL_REQUESTS_PER_TURN`](crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN);
    /// anything past the cap is discarded rather than flooding the queue.
    ///
    /// **A failed park never takes the batch or the turn down with it, but it
    /// is never silent.**
    /// [`ApprovalRequestQueue::drain`](crate::harness::policy::ApprovalRequestQueue::drain)
    /// empties the shared queue up front, so propagating the first
    /// [`CycleHost::park_effect`](crate::ports::brain::CycleHost::park_effect)
    /// error with `?` would lose every *later* request in the batch — already out
    /// of the queue and never retried — and would discard the turn's
    /// already-computed operator reply along with it. The drain therefore
    /// continues, then returns an operator-visible notice naming how many
    /// requests were not saved and how to retry them.
    async fn park_approval_requests(&self, host: &dyn CycleHost) -> Result<Option<String>> {
        let cap = crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN;
        let drained = self.deps.approval_requests.drain(cap);

        // Issue #561: the overflow used to end here, silently. The queue entries
        // are already gone and the turn is over, so a log line is the only trace
        // — and a log line is not something an operator reads. Kept loud for the
        // operator's sake *and* returned, so the cycle can say it out loud.
        if drained.discarded > 0 {
            log::warn!(
                "[harness::brain] {} gated tool call(s) past the per-turn cap of {cap} were \
                 discarded and will not reach the operator",
                drained.discarded
            );
        }

        // No `cap` argument: the drain carries the one it was taken against, so
        // the sentence cannot name a limit this turn was not held to.
        let mut notices: Vec<String> = drained.overflow_notice().into_iter().collect();
        let mut failed = 0usize;
        for request in drained.requests {
            match host.park_effect(request.effect).await {
                Ok(approval_id) => log::info!(
                    "[harness::brain] parked '{}' for operator approval (id={approval_id}): {}",
                    request.tool,
                    request.reason
                ),
                // Loud, and the only trace of a request the operator will never
                // see — the queue entry is already gone.
                Err(err) => {
                    failed += 1;
                    log::error!(
                        "[harness::brain] failed to park '{}' for operator approval ({}): {err}",
                        request.tool,
                        request.reason
                    );
                }
            }
        }
        if failed > 0 {
            let requests = if failed == 1 { "request" } else { "requests" };
            notices.push(format!(
                "{failed} approval {requests} could not be saved, so no decision is pending for \
                 that work and it was not run. Ask the agent to request approval again."
            ));
        }
        Ok((!notices.is_empty()).then(|| notices.join("\n\n")))
    }

    /// Executes one drained delegation from the orchestrator's turn.
    ///
    /// `spawn_task` opens a To-do card through the same
    /// [`TaskStore::upsert`](crate::ports::TaskStore) path the console uses and
    /// reports the card's id (issue #246); it surfaces no bubble of its own. A
    /// missing task store is a silent no-op.
    /// `delegate_to_desk` runs a single turn on the desk's lead member and
    /// **returns its reply for the orchestrator to relay** (a [`DeskReply`]) —
    /// the CEO-relay hand-back: instead of a disconnected sibling bubble the
    /// teammate's answer feeds a second orchestrator turn so the CEO comes back
    /// with it in one coherent conversation. An unknown desk (no roster-backed
    /// lead) or a cancelled run yields nothing to relay. No sub-agent
    /// re-delegation in v1: desk members carry no delegation tools, so their
    /// turns queue nothing.
    ///
    /// The orchestration lives on the brain-agnostic seam (issue #176); this is
    /// a thin wrapper that re-attaches `HarnessDeps` behind a
    /// [`HarnessRunTurn`] and drives a [`DelegationRunner`]. It exists only to
    /// keep the delegation tests exercising the same code path the cycle drives
    /// through [`DelegationRunner::handle_operator_message`], so it is
    /// test-only — the cycle never calls it directly.
    #[cfg(test)]
    async fn run_delegation(
        &self,
        delegation: Delegation,
        chat_id: Option<&str>,
    ) -> Result<delegation::DelegationOutcome> {
        let run_turn = self.run_turn();
        let record = self.record();
        self.delegation_runner(run_turn.as_ref(), &record)
            .run_delegation(delegation, chat_id, delegation::MessageContext::default())
            .await
    }

    /// Builds a [`DelegationRunner`] over `run_turn`, threading the brain-agnostic
    /// handles it needs — the record (desk-lead resolution), the task store, the
    /// steer registry, the company id, and the shared delegation queue the turn
    /// pushes onto. `HarnessDeps` never crosses the seam; it stays behind
    /// `run_turn`.
    ///
    /// The approval queue rides along **read-only** (issue #465): a card the
    /// runner opens by construction is settled from the turn that filled it, and
    /// a turn that stopped at an unauthorised call produced nothing to review.
    /// Wired here, at the one factory, so every runner the brain builds settles
    /// by the same rule rather than each call site remembering to.
    /// `record` is passed in rather than read here because the runner borrows it
    /// for its whole lifetime, and since issue #707 the brain's record lives
    /// behind a lock — [`Self::record`] hands back a handle, and a handle
    /// created inside this factory would die at the end of it. Every caller
    /// binds one for the duration of the turn, which is also what keeps a single
    /// turn on a single consistent record.
    fn delegation_runner<'a>(
        &'a self,
        run_turn: &'a dyn RunTurn,
        record: &'a CompanyRecord,
    ) -> DelegationRunner<'a> {
        DelegationRunner::new(
            run_turn,
            record,
            self.deps.tasks.as_ref(),
            &self.deps.steer,
            &record.id,
            &self.deps.delegations,
            orchestrator::MAX_DELEGATIONS_PER_TURN,
        )
        .with_approvals(&self.deps.approval_requests)
        .with_workflow_refs(&self.deps.workflow_refs)
        .with_triage(self.triage_escalation(&record.id))
    }

    /// The company's triage escalation, built once (issue #678).
    fn triage_escalation(
        &self,
        company: &crate::ports::types::CompanyId,
    ) -> &crate::harness::triage::MeteredTriage {
        self.triage.get_or_init(|| {
            crate::harness::triage::MeteredTriage::from_deps(&self.deps, company.clone())
        })
    }

    /// The company's responder selection, built once (issue #1835).
    fn selector_pass(
        &self,
        company: &crate::ports::types::CompanyId,
    ) -> &crate::harness::selector::MeteredSelector {
        self.selector.get_or_init(|| {
            crate::harness::selector::MeteredSelector::from_deps(&self.deps, company.clone())
        })
    }

    /// The per-message pick for a message addressed to an `auto` channel — or
    /// `None` wherever the deterministic answer should stand (issue #1835).
    ///
    /// `None` covers every case, deliberately in one place: the chat key names
    /// no desk, the desk is lead-routed, the channel has fewer than two roster
    /// members (a pick over one candidate is the fallback with extra latency),
    /// or the selection failed — unreachable, slow, unparseable, or an id
    /// outside the membership. The caller falls back to
    /// [`responder_for`](Self::responder_for), whose desk arm answers the
    /// channel's first roster member; **the worst case of this rung is the old
    /// rung**.
    ///
    /// Takes the operator's raw `text`, not the attachment-composed wire body:
    /// routing is judged on what was said, and a 200k-char extracted-file block
    /// would drown the one line the selection is about.
    ///
    /// A member's role and description come from the same halves the Team page
    /// renders — [`CompanyRecord::effective_agent`] for a manifest teammate
    /// (edits applied), the overlay row plus its stored edit for a
    /// console-created one — so the selector judges fit by what an operator
    /// reads on the members pane.
    async fn auto_channel_responder(&self, chat: Option<&str>, text: &str) -> Option<String> {
        let chat = chat?;
        let (company, desk_id, candidates) = {
            let record = self.record();
            let desk_id = record.resolve_desk_id(chat)?;
            if record.desk_responder_mode(&desk_id).is_lead() {
                return None;
            }
            let candidates: Vec<crate::harness::selector::SelectorCandidate> = record
                .effective_desk_members(&desk_id)
                .into_iter()
                .filter(|m| record.is_roster_agent(m))
                .filter_map(|id| selector_candidate(&record, &id))
                .collect();
            (record.id.clone(), desk_id, candidates)
        };
        match candidates.len() {
            // Every member has left the roster since the channel was created —
            // `POST …/desks` refuses an empty auto channel, but `DELETE
            // …/team/{id}` can empty one later (codex on #1872). There is
            // nobody to pick, so this defers to the caller's fallback ladder
            // and the orchestrator answers, exactly as it does for any desk
            // whose members have all gone. Refusing the deletion instead would
            // be worse — a teammate you cannot remove because a channel names
            // them — so the gap is closed by saying so rather than by
            // pretending the channel still routes.
            0 => {
                tracing::warn!(
                    company = %company,
                    chat = %desk_id,
                    "[selector] this channel has no roster members left, so there is nobody to \
                     pick; the orchestrator is answering a message addressed to the channel"
                );
                None
            }
            1 => Some(candidates[0].id.clone()),
            // The plan-level total-token ceiling gates the selection too, not
            // only the responder turn it precedes (codex on #1872). Selection
            // runs *before* a responder exists, so `total_ceiling_refusal` has
            // no agent to refuse as — but it is a real model call, and without
            // this a tenant past its hard ceiling could keep paying to route
            // by posting into an auto channel, one selector call per message,
            // after the ceiling that is supposed to permit no model calls at
            // all. Falling through to the deterministic first member costs
            // nothing and is the same answer a lead desk would give.
            _ if crate::harness::HarnessPool::total_ceiling_spent(&company, &self.deps).await => {
                tracing::info!(
                    company = %company,
                    chat = %desk_id,
                    "[selector] total token ceiling reached; routing to the channel's first \
                     member without a selection call"
                );
                None
            }
            _ => match self.selector_pass(&company).select(text, &candidates).await {
                crate::harness::selector::SelectorVerdict::Member(id) => {
                    tracing::info!(
                        company = %company,
                        chat = %desk_id,
                        picked = %id,
                        "[selector] routed an unmentioned channel message to its best-fit member"
                    );
                    Some(id)
                }
                crate::harness::selector::SelectorVerdict::Unavailable => None,
            },
        }
    }
}

/// One channel member as [`HarnessBrain::auto_channel_responder`] hands it to
/// the selection: the manifest half through
/// [`CompanyRecord::effective_agent`] (stored edits applied), the overlay half
/// from its row with any stored edit's role/description preferred — the same
/// two halves the Team page folds, so the selector and the members pane
/// describe a teammate identically.
fn selector_candidate(
    record: &CompanyRecord,
    id: &str,
) -> Option<crate::harness::selector::SelectorCandidate> {
    if let Some(agent) = record.effective_agent(id) {
        return Some(crate::harness::selector::SelectorCandidate {
            id: agent.id.clone(),
            role: agent.role.clone(),
            description: agent.description.clone(),
        });
    }
    let agent = record.overlay_agents.iter().find(|a| a.id == id)?;
    let edit = record.overlay_agent_edits.iter().find(|e| e.agent_id == id);
    Some(crate::harness::selector::SelectorCandidate {
        id: agent.id.clone(),
        role: edit
            .and_then(|e| e.role.clone())
            .unwrap_or_else(|| agent.role.clone()),
        description: edit
            .and_then(|e| e.description.clone())
            .filter(|d| !d.is_empty())
            .or_else(|| agent.description.clone()),
    })
}

/// The turn instruction for a dispatched card: its title, plus its note when it
/// carries one, framed as a work item to act on.
fn task_instruction(card: &TaskRecord) -> String {
    match card.note.as_deref().filter(|n| !n.is_empty()) {
        Some(note) => format!("Task: {}\n\n{}", card.title, note),
        None => format!("Task: {}", card.title),
    }
}

/// Records one run ending on the card: the result block on its note, and the
/// board column it lands in.
///
/// Both decisions are the orchestrator's (issue #186), so both are read from
/// [`crate::harness::lifecycle`] rather than written as literals here. Every
/// break point in `run_task`'s steer loop goes through this one function, which
/// is what stops a sixth exit inventing a sixth column string — and gives #171
/// (the `in_review → done` write, PR #179, now folded into
/// [`lifecycle::landing_column`]) and #190's `DeskTaskCompleted { column, .. }`
/// a single decision to consume. #187's artifact guard reads the same seam via
/// [`lifecycle::success_terminal_column`], so "the run succeeded" stays one
/// decision rather than a literal column compared in two places.
fn settle(card: &mut TaskRecord, end: TaskRunEnd, responder: &str, body: &str) {
    card.note = Some(append_result(
        card.note.as_deref(),
        &lifecycle::note_attribution(end, responder),
        body,
    ));
    card.column = lifecycle::landing_column(end).to_string();
}

#[async_trait]
impl Brain for HarnessBrain {
    async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
        // Issue #707: re-read the record before anything routes on it, so a desk
        // reorder / new desk / added desk member saved through the console
        // reaches this turn. Once per cycle rather than per lookup, so one turn
        // sees one consistent company. See `refresh_record` for why a failure
        // fails the cycle instead of falling back to the previous record.
        self.refresh_record().await?;
        // Issue #439: everything this cycle does runs inside its own approval
        // scope, so a workflow run executing concurrently cannot see, take, or
        // be taken by it.
        //
        // The claim replaces the `clear()` that used to open this function. It
        // clears on the way in exactly as that did, and additionally on the way
        // out via `Drop` — which is the half `clear()` never had. A cycle that
        // returned early used to leave its entries for the *next* cycle to
        // park; now the window is the claim's lifetime and nothing outlives it.
        let claim = self.deps.approval_requests.claim(ApprovalScope::Cycle);
        let company_id = req.company_id.clone();
        // Issue #1455: the cycle's policy pin must not outlive the cycle even
        // if the cycle body is cancelled or unwinds through a panic after
        // `ensure_with_policy` installed it — the `await` that would have
        // released it is exactly where a dropped future stops. The guard holds
        // the same `run_turn` the body warms through and releases every lane's
        // pin synchronously from `Drop`, so the release covers success, error,
        // cancellation and panic alike. The explicit `end_cycle` below keeps
        // the happy path visible; both are idempotent map removals.
        let _pin_guard = PolicyPinGuard::new(self.run_turn(), company_id.clone());
        let result = claim.scoped(self.run_cycle_scoped(req, host)).await;
        // Issue #1455: release the cycle's policy pin now that the cycle body is
        // over — success or error. The pin's whole job was to keep the in-flight
        // roster on the snapshot the native gate was re-applied from for the
        // cycle's own turns; a standalone workflow turn between cycles must
        // instead rebuild against the live store overlay, and a pin left behind
        // would keep the roster on a snapshot that only an unrelated cycle could
        // refresh. Dispatched through `run_turn` so a router releases every
        // lane's pool, not just the default one.
        self.run_turn().end_cycle(&company_id).await;
        result
    }

    /// The harness meters itself per turn in [`HarnessPool::run`], against the
    /// live provider slug the turn resolved to — which is why `run_cycle` reports
    /// zero `token_usage` and the runtime's cycle-level metering is a no-op here.
    fn cognition(&self) -> Cognition {
        Cognition {
            path: crate::ports::brain::HARNESS_PATH,
            provider: "per-turn",
            // Named per turn, beside the provider slug, for the same reason:
            // this path meters itself and reports zero cycle usage, so a model
            // named here would never reach a sample (issue #1749).
            model: None,
            metering: UsageMetering::PerTurn,
        }
    }
}

/// RAII release for a cycle's policy pins, the analogue of
/// [`ApprovalClaim`](crate::harness::policy::ApprovalClaim)'s `Drop` half.
///
/// A cycle pins its policy snapshot to every lane's pool through
/// [`RunTurn::ensure_with_policy`]; the pin must be released when the cycle is
/// over so a standalone workflow turn between cycles rebuilds against the live
/// store overlay. The async [`RunTurn::end_cycle`] covers the normal end, but
/// a cycle whose future is cancelled or unwinds through a panic after the pin
/// was installed never reaches it — the `await` that would have called it is
/// exactly where the future is dropped. This guard releases from `Drop`, so
/// the pin cannot outlive the cycle no matter how it ends (issue #1455).
struct PolicyPinGuard {
    run_turn: Arc<dyn crate::runtime::delegation::RunTurn>,
    company_id: crate::ports::types::CompanyId,
}

impl PolicyPinGuard {
    fn new(
        run_turn: Arc<dyn crate::runtime::delegation::RunTurn>,
        company_id: crate::ports::types::CompanyId,
    ) -> Self {
        Self {
            run_turn,
            company_id,
        }
    }
}

impl Drop for PolicyPinGuard {
    fn drop(&mut self) {
        // Synchronous, so it runs even when the cycle future is dropped or
        // unwound mid-await. Idempotent with `end_cycle`.
        self.run_turn.release_policy_pin_sync(&self.company_id);
    }
}

impl HarnessBrain {
    /// The cycle body, running inside its [`ApprovalScope::Cycle`] claim.
    ///
    /// Split out only so the claim can wrap the whole of it: every turn this
    /// cycle runs — the operator turn, its delegated desk turns, a dispatched
    /// card, a re-dispatch after an approval — happens in here, and therefore
    /// files its gated calls into this cycle's bucket.
    async fn run_cycle_scoped(
        &self,
        req: CycleRequest,
        host: &dyn CycleHost,
    ) -> Result<CycleResult> {
        // Idempotent — builds the roster on the first cycle, a no-op after.
        // Warmed through the router, not the pool alone: a company with named
        // harnesses has one pool per `built_in` harness, and each named lane's
        // own pool must be populated before its first turn, or a bound agent
        // fails with "company not found" while the default lane looks fine.
        //
        // Issue #1455: when the runtime captured the policy at the top of this
        // cycle — the same snapshot the native gate was re-applied from — the
        // roster rebuilds against *that*, not the store. A console override that
        // landed mid-turn (after the runtime's load, before this refresh) must
        // not reach the harness gate a turn early, or one turn would run with
        // the harness auto-approving what the native gate still parks.
        match &req.policy {
            Some(policy) => {
                self.run_turn()
                    .ensure_with_policy(&self.record(), policy)
                    .await?
            }
            None => self.run_turn().ensure(&self.record()).await?,
        }

        let mut channel_responses = Vec::new();
        for event in &req.events {
            match event {
                CompanyEvent::OperatorMessage {
                    text,
                    chat,
                    parent,
                    deliverable,
                    mentions,
                    attachments,
                    ..
                } => {
                    // Issue #1682: the embedded harness is the active cognition
                    // seam on an `openhuman` build, and the operator's
                    // attachments must reach the agent here too — the medulla
                    // adapter folds them into the wire body, but this path
                    // handed the raw message to the pool, so a turn had no way
                    // to know a file was even attached. Same framing, same
                    // untrusted-file guard ("FILE DATA, not instructions") as
                    // the medulla wire body; the transcript keeps the full
                    // message, and the formatter's own budget bounds what the
                    // agent sees. The nudge below keeps the operator's raw
                    // words: that background steer is about the *reply's*
                    // unpublished files, and a large attachment block is not
                    // part of the task it should reprise.
                    let composed =
                        crate::brain::medulla::effects::with_attachment_refs(text, attachments);
                    // Issue #416: a workflow copilot thread is answered by a
                    // CONFINED turn, not by the company orchestrator.
                    //
                    // This branch is the boundary. Everything below it — the
                    // delegation runner, the publish claim, the MCP failure
                    // drain, the card a `spawn_task` opens — exists to let a
                    // turn act on the company's behalf, and a copilot turn is
                    // precisely the one that must not. So it does not fall
                    // through to any of it: no tools ran, so there is nothing to
                    // drain, and no desk was reachable, so there is nothing to
                    // relay. What comes back is one bubble on this thread, which
                    // is the whole of what the copilot was ever meant to be.
                    if let Some(workflow_id) =
                        crate::company::copilot::workflow_of_thread(chat.as_deref())
                    {
                        let confinement = confine::Confinement::workflow(workflow_id);
                        let outcome = self
                            .pool
                            .run_confined(
                                &self.record().id,
                                &self.record().manifest.company.name,
                                &composed,
                                &self.deps,
                                chat.as_deref(),
                                &confinement,
                            )
                            .await?;
                        // Issue #1846 review (Codex #3869277640): see
                        // `confined_turn_bubble`'s doc for why a budget pause
                        // is handled separately from an ordinary copilot
                        // reply here.
                        channel_responses.push(confined_turn_bubble(outcome));
                        continue;
                    }
                    // Route to the teammate the message named, else to the
                    // addressed desk's lead, else the orchestrator.
                    //
                    // Naming somebody in a room is a stronger address than the
                    // room's default answerer, so a mention outranks the desk
                    // lead. That is the same explicit-beats-implicit ordering
                    // `responder_for` already applies between an addressed desk
                    // and the orchestrator (issue #884) — one more rung at the
                    // top of the existing ladder, not a second competing notion
                    // of who a message is for.
                    //
                    // Resolves nothing on a message that mentions no teammate,
                    // which is every message journaled before mentions existed,
                    // so routing is unchanged byte-for-byte for them.
                    let responder =
                        match crate::runtime::mentions::mention_responder(&self.record(), mentions)
                        {
                            Some(responder) => responder,
                            // Issue #1835: below a mention, above the deterministic
                            // answer, an `auto` channel picks its best-fit member
                            // for this message. Every way the pick cannot happen —
                            // not an auto channel, one member, selection failed —
                            // is `None`, and the ladder continues exactly where it
                            // always stood.
                            None => {
                                match self.auto_channel_responder(chat.as_deref(), text).await {
                                    Some(responder) => responder,
                                    None => self.responder_for(chat.as_deref()),
                                }
                            }
                        };
                    // Everyone else the message named, for the answering turn's
                    // context. A list, not a fan-out: one operator message still
                    // spawns exactly one turn, and this teammate spreads the
                    // work — if it should — through the existing gated
                    // delegation seam rather than through a new uncontrolled
                    // one. `@everyone` expands here, against the addressed
                    // desk's membership.
                    //
                    // The addressed desk is the raw chat key unless it is one of
                    // the General-desk spellings `is_general_chat` folds — the
                    // console's default thread sends `chat: "main"`, and
                    // `resolve_desk_id` does not recognise that console-only
                    // alias, so a broadcast from the main thread would otherwise
                    // expand against no desk at all.
                    let addressed_desk = Self::everyone_desk(&self.record(), chat.as_deref());
                    let also_mentioned = crate::runtime::mentions::mentioned_agents(
                        &self.record(),
                        &addressed_desk,
                        mentions,
                        Some(&responder),
                    );
                    // The chat/desk thread this turn answers — the same id the
                    // reply is journaled under (`AgentReply.chat_id`). Passed into
                    // the pool so the live turn-stream frames carry it and the
                    // console routes them to this thread; a delegated desk reply
                    // in this cycle rides the same operator thread, so it gets the
                    // same id.
                    //
                    // Passed through AS THE OPERATOR SENT IT — `None` when they
                    // addressed no desk — and deliberately NOT normalized to
                    // `DEFAULT_DESK` (#1890). A codex review on #1896 read
                    // `run_with_steer`'s `if let Some(incoming) = turn_chat_id`
                    // guard and concluded an unaddressed threaded message loses
                    // its root; it does not. `turn_chat_id` comes from the
                    // turn-stream route, which already falls back to
                    // `DEFAULT_DESK` (see `LiveRoute::Chat`'s construction), so
                    // an unaddressed chat turn binds to General and keeps its
                    // thread like any other.
                    //
                    // Normalizing here would be actively wrong: this same
                    // `chat_id` reaches card creation, a card's
                    // `origin_chat_id`, and `is_copilot_thread` — and a `None`
                    // origin means "no conversation raised this card", which
                    // `chat_history::owns` routes to no desk on purpose.
                    // Turning it into `Some("General")` posts board-marker lines
                    // into the operator's main line, the exact bug that arm is
                    // documented to prevent.
                    let chat_id = chat.as_deref();
                    // Issue #989: the dispatch-start baseline for "did this
                    // responder write anything it did not publish?" — taken
                    // before the turn runs, for the same reason run_task's own
                    // `workspace_at_dispatch` is (a snapshot taken after the
                    // turn would already include whatever it wrote).
                    //
                    // Only consulted below when the turn reports
                    // `hit_iteration_cap`: an ordinary chat reply that finishes
                    // on its own is not this issue's scope. #244's scan already
                    // covers the task-dispatch path unconditionally; widening
                    // it to every chat turn is a separate, unscoped change.
                    // Taken unconditionally anyway (cap status is not known
                    // until the turn returns), the same trade-off run_task
                    // already makes for every dispatch.
                    let cap_scan_workspace =
                        agent_workspace(&self.deps.workspace_root, &self.record().id, &responder);
                    let cap_scan_baseline = WorkspaceSnapshot::take(&cap_scan_workspace);
                    // Clear stale MCP failures so nothing leaks from a prior turn
                    // (the delegation queue is cleared inside the runner, right
                    // before the orchestrator turn).
                    self.deps.mcp_failures.clear();
                    // Issue #445: claim the publish queue for this conversation,
                    // so a file published in chat is drained below instead of
                    // being staged into a queue nothing reaches. Claimed only
                    // when both stores the drain needs are wired — the claim is
                    // a promise to record, and one that cannot be kept must not
                    // be made, or the tool goes back to issuing receipts nothing
                    // honours.
                    let publish_claim =
                        (self.deps.tasks.is_some() && self.deps.artifacts.is_some()).then(|| {
                            self.deps
                                .pending_publishes
                                .claim(publish::PublishDestination::Conversation)
                        });
                    // Drive the brain-agnostic delegation seam (issue #176): the
                    // orchestrator turn, its queued delegations, and the CEO-relay
                    // hand-back all run behind the `RunTurn` impl. `HarnessDeps` is
                    // re-attached behind `HarnessRunTurn`.
                    let run_turn = self.run_turn();
                    // Bound for the runner's whole lifetime (issue #707): one turn, one record.
                    let record = self.record();
                    let turn = self
                        .delegation_runner(run_turn.as_ref(), &record)
                        // Issues #1035 / #1152: the operator's own statement of
                        // what this message is for. The REST handler already
                        // acts on it; until #1035 the runtime never saw it, so
                        // it could not tell a message the handler had carded
                        // from one it had not — and since #1152 it also carries
                        // "this is not work", which the runtime has to honour or
                        // the console's promise holds on one surface only.
                        .requested(*deliverable)
                        // Who else this message named (issue: mentions). Context
                        // for the turn, never a second dispatch.
                        .also_mentioned(also_mentioned)
                        // The thread this message belongs to (#1890). Its own
                        // `parent` IS the root — a reply is parented to its
                        // question's parent, never to the question — so an
                        // unparented message carries `None` and lands on the
                        // channel-level conversation.
                        .in_thread(*parent)
                        // Issue #1846 review (Codex #3864988176): the operator's
                        // own words, so a delegate's budget-pause marker re-parks
                        // with what the operator actually asked for rather than
                        // the hand-off instruction the model wrote.
                        .reissue_message(composed.clone())
                        .handle_operator_message(&responder, &composed, chat_id)
                        .await?;
                    let mut operator_steps = turn.steps;
                    let mut operator_reply = turn.reply;
                    // Issue #1846: unlike a step-cap pause or a spend halt —
                    // both of which stop a turn that had already produced SOME
                    // text — a budget pause fires on the model call itself
                    // failing, so `turn.reply` is not a partial answer, it is
                    // the SAME actionable "add credits" copy the sibling
                    // notice below carries. Left as-is it would double: the
                    // operator reads it once attributed to the teammate (who
                    // said nothing — the call never returned) and again,
                    // correctly, as the unauthored system notice. Overwritten
                    // here with a short, honest placeholder so the authored
                    // bubble never claims words the teammate did not produce,
                    // and the full explanation lives in exactly one place.
                    //
                    // Issue #1906: this override is WHOLESALE, and that is the
                    // fact the delegation layer has to be written against. It
                    // discards the CEO relay's reply, and it discarded #1886's
                    // fold of the delegates' text — anything appended to
                    // `OperatorTurn::reply` upstream is unreachable from here
                    // on any paused turn. If a delegate's own words should ever
                    // reach the operator through a pause, they need a channel
                    // of their own (a sibling bubble), not more text on a
                    // string this line replaces.
                    if turn.budget_paused.is_some() {
                        operator_reply = BUDGET_PAUSED_PLACEHOLDER_REPLY.to_string();
                    }

                    // Drain what the conversation published (#445). Unconditional
                    // so nothing survives into the next turn, and only *recorded*
                    // when the claim was actually taken — an unclaimed queue can
                    // only be empty here, because the tool refuses without one.
                    let published = self.deps.pending_publishes.drain();
                    // Issue #989: the paths this turn actually offered, captured
                    // before `file_conversation_batch` below moves `published` —
                    // the cap-pause scan's "staged" side of `publish::unpublished`
                    // needs this list and cannot re-read the queue for it: by the
                    // time that scan runs the queue has already been drained here.
                    let published_sources: Vec<String> =
                        published.iter().map(|p| p.source.clone()).collect();
                    let mut published_card = self
                        .file_conversation_batch(
                            &responder,
                            turn.spawned_task.as_deref(),
                            chat_id,
                            publish_claim.is_some(),
                            published,
                            &mut operator_reply,
                        )
                        .await;

                    // Issue #989: on a turn that paused at its iteration cap, run
                    // the same unpublished-work scan the task-dispatch path
                    // (`run_task`) already runs, and — if it wrote something it
                    // never offered — the same one follow-up nudge turn (issue
                    // #244's `nudge_for_unpublished`, unchanged). A capped turn
                    // returns `Ok` with a checkpoint reply, so nothing above
                    // treats it as interrupted; without this, a file the agent
                    // had already written sits in its sandbox with nothing
                    // anywhere saying so.
                    //
                    // The publish claim is still live here — `drop(publish_claim)`
                    // is below this block, not above it — so a publish the nudge
                    // turn itself makes stages exactly like any other and is
                    // filed through the same `file_conversation_batch` path
                    // rather than being silently discarded when the claim
                    // releases.
                    //
                    // Issue #1032 deliberately does NOT extend this to a spend
                    // halt, and the omission is the decision rather than an
                    // oversight. `nudge_for_unpublished` runs ANOTHER model
                    // turn — that is what makes it a nudge — and the teammate
                    // this would fire for has just been stopped for running out
                    // of money. Spending more of a budget that had already run
                    // out, to tidy up after the brake that enforced it, defeats
                    // the brake. The spend notice tells the operator the work
                    // stopped short; deciding whether it is worth more money is
                    // theirs to make, not this layer's to make for them.
                    if turn.hit_iteration_cap {
                        let changed = cap_scan_baseline.changed_since(&cap_scan_workspace);
                        let unpublished = publish::unpublished(&changed.files, &published_sources);
                        if !unpublished.is_empty() {
                            let nudge_control = SteerControl::new();
                            let declined = self
                                .nudge_for_unpublished(
                                    run_turn.as_ref(),
                                    &responder,
                                    text,
                                    &operator_reply,
                                    &unpublished,
                                    changed.partial,
                                    &nudge_control,
                                    None,
                                )
                                .await;
                            let nudge_published = self.deps.pending_publishes.drain();
                            if let Some(card_id) = self
                                .file_conversation_batch(
                                    &responder,
                                    turn.spawned_task.as_deref(),
                                    chat_id,
                                    publish_claim.is_some(),
                                    nudge_published,
                                    &mut operator_reply,
                                )
                                .await
                            {
                                published_card = published_card.or(Some(card_id));
                            }
                            // The fallback's file list is the pre-nudge diff
                            // minus whatever is staged now — the same "not a
                            // fresh scan" argument `run_task`'s own fallback
                            // documents: a scratch file written *while
                            // answering the nudge* is an artifact of being
                            // asked, and naming it here would make the nudge
                            // generate its own noise.
                            let still_unpublished = publish::unpublished(
                                &unpublished,
                                &self.deps.pending_publishes.sources(),
                            );
                            if !still_unpublished.is_empty() {
                                // A plain chat turn has no card to note a
                                // decline on unless one happened to be opened
                                // (`turn.spawned_task`) — unlike a dispatched
                                // task, which always has one. The warning is
                                // therefore the whole of what happens here,
                                // exactly as it is in `run_task` whenever the
                                // nudge itself produced no reply to note.
                                tracing::warn!(
                                    company = %self.record().id,
                                    agent = %responder,
                                    files = %publish::name_files(&still_unpublished),
                                    declined = declined.is_some(),
                                    partial_scan = changed.partial,
                                    "[publish] a capped chat turn changed sandbox files and \
                                     published none of them; no artifact was recorded"
                                );
                            }
                        }
                    }
                    drop(publish_claim);
                    // Re-skin any MCP tool-call failures (from the orchestrator
                    // turn, a delegated desk turn, or the relay turn) as error
                    // steps on the operator bubble — one surface, one renderer.
                    self.surface_mcp_failures(&mut operator_steps, None).await?;
                    channel_responses.push(OutboundMessage {
                        message_id: None,
                        // Issue #246: when the turn opened a board card, say so
                        // on the bubble it opened it from. Before this a
                        // `spawn_task` was invisible in chat — the card landed
                        // on the board and the reply carried nothing tying the
                        // two together.
                        //
                        // Issue #445 reuses that link for the card a publish
                        // minted, which is how the operator gets from "here is
                        // your deliverable" to the thing itself in one click.
                        //
                        // **`published_card` wins** (#463). It is `Some` only
                        // when a publish landed, and it always names the card
                        // the deliverable landed ON — usually `spawned_task`
                        // itself, but the freshly minted replacement when that
                        // card was deleted mid-turn. Preferring `spawned_task`
                        // there would link the reply to an id that no longer
                        // resolves while the file sits elsewhere, which is this
                        // issue's own failure reached through the fallback added
                        // to prevent it. With no publish this is `None` and the
                        // turn's own card takes the slot exactly as before.
                        task_id: published_card.or(turn.spawned_task),
                        channel: "operator".to_string(),
                        // Issue #885: who spoke, as distinct from where it goes.
                        // `responder_for` already picked this agent to answer the
                        // turn; before this the identity died here and the reply
                        // was journaled as `agent_id: "operator"` forever.
                        agent: Some(responder.clone()),
                        text: operator_reply,
                        reply_to: None,
                        mentions: Vec::new(),
                        steps: operator_steps,
                    });
                    // Issue #926: a turn that paused at its step cap says so,
                    // in its own bubble.
                    //
                    // A SIBLING bubble rather than text appended to the reply,
                    // for the reason the approval-overflow notice below gives:
                    // the reply is the agent's answer and this is the system
                    // saying the agent was cut off. Here that separation is
                    // load-bearing rather than tidy — `HarnessPool::run`
                    // persists `outcome.reply` to the context store, so
                    // appending would write "you hit the step limit" into
                    // memory and recall it as something the agent said in a
                    // later turn.
                    //
                    // Unauthored (`agent: None`) for the same reason: no
                    // teammate said this, and attributing it to the responder
                    // would put the platform's words in its mouth. Empty steps
                    // — the turn's timeline is already on the bubble above, and
                    // repeating it would double every row in the console.
                    if turn.hit_iteration_cap {
                        channel_responses.push(OutboundMessage {
                            message_id: None,
                            task_id: None,
                            channel: "operator".to_string(),
                            agent: None,
                            text: ITERATION_CAP_PAUSE_NOTICE.to_string(),
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        });
                    }
                    // Issue #1032: and a turn halted for spend says so, in its
                    // own bubble, for every reason the block above gives —
                    // sibling not appended (`HarnessPool::run` persists
                    // `outcome.reply`, so appending would file "you ran out of
                    // budget" as something the teammate said and recall it
                    // later), unauthored, no steps.
                    //
                    // A separate `if`, not an `else`: one operator message can
                    // run several turns, so a responder that paused at its step
                    // cap and a delegate that ran out of money are both true of
                    // the same bubble, and the operator is owed both facts. They
                    // cannot both come from ONE turn — #988 pins that a spend
                    // halt reads `hit_iteration_cap == false` — so this is only
                    // ever two notices for two different turns.
                    if let Some(halt) = &turn.halted_for_spend {
                        channel_responses.push(OutboundMessage {
                            message_id: None,
                            task_id: None,
                            channel: "operator".to_string(),
                            agent: None,
                            text: spend_halt_notice(halt),
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        });
                    }
                    // Issue #1846: and a turn paused for lack of inference
                    // budget/credits says so, in its own bubble, for every
                    // reason the two blocks above give — sibling not appended,
                    // unauthored, no steps. Mutually exclusive with a spend
                    // halt (opposite lever: add money vs. don't ask for more
                    // work), but not with an iteration-cap pause in principle —
                    // in practice `classify_turn` only ever reaches the
                    // budget-paused arm when the model call itself errored,
                    // which cannot also have hit the iteration cap on the same
                    // attempt.
                    if let Some(pause) = &turn.budget_paused {
                        channel_responses.push(OutboundMessage {
                            message_id: None,
                            task_id: None,
                            channel: "operator".to_string(),
                            agent: None,
                            text: budget_pause_notice(pause),
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        });
                    }
                    channel_responses.extend(turn.bubbles);
                }
                CompanyEvent::TaskDispatched { task_id, run_id } => {
                    if let Some(message) = self.run_task(task_id, run_id.as_deref()).await? {
                        channel_responses.push(message);
                    }
                }
                // Approval resolutions return to the asking agent. Legacy
                // approved calls redeem their grant; explicit requests resume
                // on either verdict with the decision itself.
                //
                // This arm is the reason the feature was invisible before. The
                // match had exactly two arms and everything else fell into
                // `_ => {}`, so an `ApprovalResolved` produced no turn, no
                // response, and the cycle ended on the "Acknowledged." fallback
                // below. The operator approved, saw "Acknowledged.", and nothing
                // happened — indistinguishable from the tool having run.
                CompanyEvent::ApprovalResolved {
                    approval_id,
                    verdict,
                    ..
                } => {
                    if let Some(message) =
                        self.redispatch_granted_call(approval_id, *verdict).await?
                    {
                        channel_responses.push(message);
                    }
                }
                CompanyEvent::ScheduleFired { prompt, .. } => {
                    let responder = self.responder_for(None);
                    // A scheduled tick drives a real turn, so it needs the same
                    // scaffolding an operator message gets: stale MCP failures
                    // cleared before it (nothing leaks from a prior turn), a
                    // live publish claim while it runs, and its own MCP
                    // failures re-skinned onto the reply afterwards.
                    self.deps.mcp_failures.clear();
                    // Issue #989: capture the workspace before the scheduled
                    // turn, so a capped turn can recover files it wrote but
                    // never offered to publish.
                    let cap_scan_workspace =
                        agent_workspace(&self.deps.workspace_root, &self.record().id, &responder);
                    let cap_scan_baseline = WorkspaceSnapshot::take(&cap_scan_workspace);
                    // Issue #445: claim the publish queue for this conversation,
                    // so a file the scheduled turn publishes is drained below
                    // instead of being staged into a queue nothing reaches.
                    // Claimed only when both stores the drain needs are wired —
                    // the claim is a promise to record, and one that cannot be
                    // kept must not be made, or the tool goes back to issuing
                    // receipts nothing honours.
                    let publish_claim =
                        (self.deps.tasks.is_some() && self.deps.artifacts.is_some()).then(|| {
                            self.deps
                                .pending_publishes
                                .claim(publish::PublishDestination::Conversation)
                        });
                    // Drive the same routed turn an operator message gets, so a
                    // responder bound to a named harness runs there and an
                    // unavailable default fails loudly instead of silently
                    // falling back to the embedded engine — the router's job.
                    let run_turn = self.run_turn();
                    let record = self.record();
                    let turn = self
                        .delegation_runner(run_turn.as_ref(), &record)
                        .handle_operator_message(&responder, prompt, None)
                        .await?;
                    let mut responses = vec![OutboundMessage {
                        message_id: None,
                        task_id: turn.spawned_task,
                        // The reply lands on the General desk — the destination
                        // — and the responder is its author. Fusing the two into
                        // `channel` made reload history route the same reply to
                        // General while journaling the wrong author; they are
                        // separate facts (issue #885).
                        channel: crate::server::ops::language::DEFAULT_DESK.to_string(),
                        agent: Some(responder.clone()),
                        text: if turn.budget_paused.is_some() {
                            BUDGET_PAUSED_PLACEHOLDER_REPLY.to_string()
                        } else {
                            turn.reply
                        },
                        reply_to: None,
                        steps: turn.steps,
                        mentions: Vec::new(),
                    }];
                    responses.extend(turn.bubbles);
                    // Issue #926/#1032/#1846, scheduled edition: a turn that
                    // paused at its step cap, halted for spend, or paused for
                    // lack of budget says so in its own sibling bubble, exactly
                    // as the operator path does. Here the journal is the turn's
                    // only durable record — the operator channel is in-memory
                    // for a cron tick — so omitting them would present
                    // interrupted scheduled work as a completed answer.
                    // Unauthored on the operator path; here they must carry an
                    // author to be journaled at all, so they take the same
                    // system author `system_notice` uses. Separate `if`s, not
                    // `else`s, mirroring the operator path: the flags are
                    // sticky across the turns one tick runs, and each notice is
                    // owed even when another fires.
                    if turn.hit_iteration_cap {
                        responses.push(OutboundMessage {
                            message_id: None,
                            task_id: None,
                            channel: crate::server::ops::language::DEFAULT_DESK.to_string(),
                            agent: Some(crate::ports::SYSTEM_AUTHOR.to_string()),
                            text: ITERATION_CAP_PAUSE_NOTICE.to_string(),
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        });
                    }
                    if let Some(halt) = &turn.halted_for_spend {
                        responses.push(OutboundMessage {
                            message_id: None,
                            task_id: None,
                            channel: crate::server::ops::language::DEFAULT_DESK.to_string(),
                            agent: Some(crate::ports::SYSTEM_AUTHOR.to_string()),
                            text: spend_halt_notice(halt),
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        });
                    }
                    if let Some(pause) = &turn.budget_paused {
                        responses.push(OutboundMessage {
                            message_id: None,
                            task_id: None,
                            channel: crate::server::ops::language::DEFAULT_DESK.to_string(),
                            agent: Some(crate::ports::SYSTEM_AUTHOR.to_string()),
                            text: budget_pause_notice(pause),
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        });
                    }
                    // The pause placeholder and notices are journaled here — a
                    // scheduled turn's journal is its only durable record, and
                    // no live operator is reading the in-memory channel.
                    // Drain what the scheduled turn published (#445) and file it
                    // onto the card the turn opened — or a freshly minted one —
                    // exactly as an operator turn's publish is filed.
                    let spawned_task = responses[0].task_id.clone();
                    let published = self.deps.pending_publishes.drain();
                    let published_sources: Vec<String> = published
                        .iter()
                        .map(|publish| publish.source.clone())
                        .collect();
                    let mut published_card = self
                        .file_conversation_batch(
                            &responder,
                            spawned_task.as_deref(),
                            Some(crate::server::ops::language::DEFAULT_DESK),
                            publish_claim.is_some(),
                            published,
                            &mut responses[0].text,
                        )
                        .await;
                    // Issue #989: a capped scheduled turn gets the same
                    // unpublished-file recovery as an operator turn. The nudge
                    // is deliberately limited to iteration caps, not spend
                    // halts: spending another model turn after a budget brake
                    // would defeat that brake. The scheduled path has no
                    // operator present to notice a stranded workspace file, so
                    // silently leaving it behind would lose a deliverable.
                    if turn.hit_iteration_cap {
                        let changed = cap_scan_baseline.changed_since(&cap_scan_workspace);
                        let unpublished = publish::unpublished(&changed.files, &published_sources);
                        if !unpublished.is_empty() {
                            let nudge_control = SteerControl::new();
                            let _declined = self
                                .nudge_for_unpublished(
                                    run_turn.as_ref(),
                                    &responder,
                                    prompt,
                                    &responses[0].text,
                                    &unpublished,
                                    changed.partial,
                                    &nudge_control,
                                    None,
                                )
                                .await;
                            let nudge_published = self.deps.pending_publishes.drain();
                            if let Some(card_id) = self
                                .file_conversation_batch(
                                    &responder,
                                    spawned_task.as_deref(),
                                    Some(crate::server::ops::language::DEFAULT_DESK),
                                    publish_claim.is_some(),
                                    nudge_published,
                                    &mut responses[0].text,
                                )
                                .await
                            {
                                published_card = published_card.or(Some(card_id));
                            }
                        }
                    }
                    drop(publish_claim);
                    // `published_card` wins over the turn's own card, the same
                    // rule the operator path applies (#463): it names the card
                    // the deliverable actually landed on, which is usually the
                    // turn's card but the freshly minted replacement when that
                    // card was deleted mid-turn.
                    if let Some(card_id) = published_card {
                        responses[0].task_id = Some(card_id);
                    }
                    // Re-skin any MCP tool-call failures from the scheduled turn
                    // as error steps on the reply's timeline — one surface, one
                    // renderer. Runs before the reply is journaled, so the
                    // journal order reads failure-then-reply.
                    self.surface_mcp_failures(&mut responses[0].steps, None)
                        .await?;
                    // Issue #561, scheduled edition: if this scheduled turn
                    // gated more calls than one turn may raise, park the batch
                    // and journal the overflow notice here. The after-loop
                    // `park_approval_requests` below routes its notice to the
                    // in-memory operator adapter, which a cron tick has no live
                    // operator reading, so the warning that some approvals were
                    // permanently discarded would otherwise vanish from the only
                    // surface that records a scheduled turn. Draining here
                    // leaves the after-loop call with an empty queue, so the
                    // notice is never raised twice.
                    if let Some(notice) = self.park_approval_requests(host).await? {
                        responses.push(OutboundMessage {
                            message_id: None,
                            task_id: None,
                            channel: crate::server::ops::language::DEFAULT_DESK.to_string(),
                            agent: Some(crate::ports::SYSTEM_AUTHOR.to_string()),
                            text: notice,
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        });
                    }
                    if let Some(events) = self.deps.events.as_ref() {
                        for response in &mut responses {
                            // Per-bubble authorship: a delegation bubble names
                            // its own speaker; the primary bubble is the
                            // responder's, and the responder is the fallback a
                            // bubble without an author needs.
                            let agent_id =
                                response.agent.clone().unwrap_or_else(|| responder.clone());
                            match events
                                .append(
                                    &record.id,
                                    CompanyEvent::AgentReply {
                                        parent: None,
                                        task_id: response.task_id.clone(),
                                        chat_id: crate::server::ops::language::DEFAULT_DESK
                                            .to_string(),
                                        agent_id,
                                        text: response.text.clone(),
                                        steps: response.steps.clone(),
                                        mentions: Vec::new(),
                                        mention_depth: 0,
                                    },
                                )
                                .await
                            {
                                Ok(seq) => response.message_id = Some(seq.value().to_string()),
                                Err(err) => tracing::warn!(
                                    error = %err,
                                    "failed to journal a scheduled reply; the bubble has no durable id"
                                ),
                            }
                        }
                    }
                    channel_responses.extend(responses);
                }
                _ => {}
            }
        }

        // Issue #172: every approval-gated tool call this cycle's turns hit is
        // parked on the host's gate now, so it shows up on the operator's
        // Approvals page instead of only being narrated away in chat.
        //
        // Issue #561: and if the turn gated more calls than one turn may raise,
        // the operator is told so here rather than discovering it as silence.
        // Pushed as its own bubble instead of appended to the reply above,
        // because the reply is the agent's answer and this is the system saying
        // the agent was cut off — and because a turn whose only outcome was
        // overflow has no reply to append to.
        if let Some(notice) = self.park_approval_requests(host).await? {
            channel_responses.push(system_notice(notice));
        }

        // The runtime requires at least one channel response per cycle.
        if channel_responses.is_empty() {
            channel_responses.push(system_notice("Acknowledged.".to_string()));
        }

        let trace = CompressedTrace::now(
            req.cycle_id.clone(),
            format!("harness cycle handled {} event(s)", req.events.len()),
        );

        // No `ledger_deltas` / `token_usage` here on purpose: `HarnessPool::run`
        // is the single cost-accounting site (it writes the ledger entry and the
        // usage sample through `deps`), so surfacing the same spend again would
        // double-count it — the runtime meters a non-zero `token_usage` for every
        // brain (issue #174), and `cognition()` below declares that this path has
        // already done it.
        Ok(CycleResult {
            channel_responses,
            new_traces: vec![trace],
            ledger_deltas: Vec::new(),
            token_usage: TokenUsage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tinyagents::harness::message::Message;
    use tinyagents::harness::model::{ChatModel, ModelRequest, ModelResponse};

    use crate::company::CompanyManifest;
    use crate::harness::provider::{HarnessModel, MockProvider};
    use crate::ports::brain::CycleHost;
    // Issue #301: every lifecycle return now lands in To-do (the `backlog` pool
    // is gone), so these assertions read the const rather than a literal.
    use crate::ports::tasks::{COLUMN_IN_REVIEW, COLUMN_PAUSED, COLUMN_TODO};
    use crate::ports::types::{
        ApprovalId, CompanyId, ContextOp, ContextOpResult, Effect, EffectDisposition, OverlayAgent,
        ToolCall, ToolResult,
    };
    use crate::store::{FsCompanyStore, FsContextStore, FsOps};

    /// A `CycleHost` that auto-executes anything the brain asks for and swallows
    /// anything it parks; used by every test that isn't about approvals.
    #[derive(Default)]
    struct NoopHost;

    #[async_trait]
    impl CycleHost for NoopHost {
        async fn call_tool(&self, _call: ToolCall) -> Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                output: serde_json::Value::Null,
            })
        }
        async fn context_op(&self, _op: ContextOp) -> Result<ContextOpResult> {
            Ok(ContextOpResult::Text(String::new()))
        }
        async fn emit_effect(&self, _effect: Effect) -> Result<EffectDisposition> {
            Ok(EffectDisposition::Executed)
        }
        async fn park_effect(&self, _effect: Effect) -> Result<ApprovalId> {
            Ok(ApprovalId::new("appr-parked"))
        }
    }

    /// A `CycleHost` that records every effect parked for approval, so the
    /// approval drain can be asserted on (issue #172). Anything else it does is
    /// inert.
    #[derive(Default)]
    struct ParkingHost {
        parked: std::sync::Mutex<Vec<Effect>>,
    }

    impl ParkingHost {
        /// The effects parked through `park_effect`, in order.
        fn parked(&self) -> Vec<Effect> {
            self.parked.lock().expect("parked").clone()
        }
    }

    #[async_trait]
    impl CycleHost for ParkingHost {
        async fn call_tool(&self, _call: ToolCall) -> Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                output: serde_json::Value::Null,
            })
        }
        async fn context_op(&self, _op: ContextOp) -> Result<ContextOpResult> {
            Ok(ContextOpResult::Text(String::new()))
        }
        async fn emit_effect(&self, _effect: Effect) -> Result<EffectDisposition> {
            panic!("an approval request must be parked, never re-evaluated as an effect");
        }
        async fn park_effect(&self, effect: Effect) -> Result<ApprovalId> {
            let mut parked = self.parked.lock().expect("parked");
            parked.push(effect);
            Ok(ApprovalId::new(format!("appr-{}", parked.len())))
        }
    }

    fn record() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
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

    fn brain_over_mock(dir: &std::path::Path) -> HarnessBrain {
        brain_over_mock_with(dir, record())
    }

    /// [`brain_over_mock`] over a chosen record, so a test can vary the roster
    /// (and its `[[harness]]` block) without restating the whole deps literal.
    fn brain_over_mock_with(dir: &std::path::Path, record: CompanyRecord) -> HarnessBrain {
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record)
    }

    fn request(events: Vec<CompanyEvent>) -> CycleRequest {
        CycleRequest {
            cycle_id: "cycle-1".to_string(),
            company_id: CompanyId::new("acme"),
            events,
            event_seqs: Vec::new(),
            policy: None,
        }
    }

    #[tokio::test]
    async fn operator_message_gets_an_agent_reply() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "status?".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
        // The mock provider prefixes the routed message, proving the turn ran
        // through the openhuman agent rather than an echo.
        assert!(
            result.channel_responses[0].text.contains("status?"),
            "{:?}",
            result.channel_responses[0].text
        );
        // The offline mock runs no tools and emits no progress, so the operator
        // bubble carries zero steps — the tell that distinguishes a tool-less
        // (here, memory/echo-style) answer from a tool-backed one.
        assert!(
            result.channel_responses[0].steps.is_empty(),
            "a tool-less turn carries no steps: {:?}",
            result.channel_responses[0].steps
        );
        assert_eq!(result.new_traces.len(), 1);
        // Single cost-accounting site: the cycle result carries no ledger delta.
        assert!(result.ledger_deltas.is_empty());
    }

    #[tokio::test]
    async fn schedule_fired_gets_an_agent_reply() {
        // A cron tick (`ScheduleFired`) must drive a real turn and surface its
        // reply, not fall through the match and vanish — the same guarantee an
        // operator message gets. Without this arm a scheduled prompt ran to
        // nowhere: the turn produced an answer that was never journaled, so the
        // desk history had no record it fired.
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::ScheduleFired {
                    cron: "0 9 * * *".into(),
                    prompt: "daily standup".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 1);
        // The mock provider prefixes the routed prompt, proving the turn ran
        // through the agent rather than falling through the match.
        assert!(
            result.channel_responses[0].text.contains("daily standup"),
            "{:?}",
            result.channel_responses[0].text
        );
        assert_eq!(result.new_traces.len(), 1);
    }

    /// A cron tick's reply is journaled onto the General desk under the
    /// responder's name, and the returned bubble is stamped with the journaled
    /// event's sequence — the same contract an operator reply's bubble carries
    /// (issue #885: destination and author are separate facts).
    #[tokio::test]
    async fn schedule_fired_journals_an_agent_reply_on_the_general_desk() {
        use crate::ports::EventLog;
        use crate::ports::types::EventSeq;
        use crate::store::FsEventLog;

        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let brain = brain_with_queue_and_events(dir.path(), Default::default(), log.clone());
        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::ScheduleFired {
                    cron: "0 9 * * *".into(),
                    prompt: "daily standup".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        // The reply lands on the General desk, authored by the responder —
        // destination and author stay separate.
        assert_eq!(result.channel_responses.len(), 1);
        let bubble = &result.channel_responses[0];
        assert_eq!(bubble.channel, crate::server::ops::language::DEFAULT_DESK);
        assert_eq!(bubble.agent.as_deref(), Some("ceo"));

        // The journal holds one AgentReply, on the General desk, attributed to
        // the responder — not to the channel the reply was routed over.
        let events = log
            .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
            .await
            .expect("read events");
        let reply = events
            .iter()
            .find(|e| matches!(&e.event, CompanyEvent::AgentReply { .. }))
            .expect("a scheduled reply was journaled");
        match &reply.event {
            CompanyEvent::AgentReply {
                chat_id,
                agent_id,
                text,
                ..
            } => {
                assert_eq!(chat_id, crate::server::ops::language::DEFAULT_DESK);
                assert_eq!(agent_id, "ceo");
                assert!(text.contains("daily standup"), "{text}");
            }
            _ => unreachable!(),
        }

        // The returned bubble carries the appended event's sequence as its
        // durable id.
        assert_eq!(bubble.message_id, Some(reply.seq.value().to_string()));
    }

    #[tokio::test]
    async fn schedule_fired_journals_halt_notices() {
        use crate::ports::EventLog;
        use crate::ports::types::EventSeq;
        use crate::store::FsEventLog;

        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let outcome = crate::harness::built_in::TurnOutcome {
            reply: "checkpoint".to_string(),
            steps: Vec::new(),
            hit_iteration_cap: true,
            // Test fixture, not the ACP fold (PR #1880 review).
            abnormal_stop: None,
            halted_for_spend: Some(crate::harness::SpendHalt {
                agent: "ceo".to_string(),
                spent_usd: 1.25,
                cap_usd: 1.0,
            }),
            // This fixture scripts a SPEND halt; a budget pause is the separate
            // signal added in issue #1846 and is not what it exercises.
            budget_paused: None,
        };
        let brain = brain_with_queue_and_events(dir.path(), Default::default(), log.clone())
            .with_default_engine(Some(Arc::new(FixedOutcomeTurn {
                outcome,
                approval_requests: None,
            })));
        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::ScheduleFired {
                    cron: "0 9 * * *".into(),
                    prompt: "daily standup".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 3);
        assert!(
            result.channel_responses[1]
                .text
                .contains("maximum number of steps")
        );
        assert!(result.channel_responses[2].text.contains("spend cap"));
        assert!(
            result
                .channel_responses
                .iter()
                .skip(1)
                .all(|response| response.agent.as_deref() == Some(crate::ports::SYSTEM_AUTHOR))
        );
        let events = log
            .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
            .await
            .expect("read events");
        let replies: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.event, CompanyEvent::AgentReply { .. }))
            .collect();
        assert_eq!(replies.len(), 3, "all scheduled notices are durable");
    }

    #[tokio::test]
    async fn schedule_fired_journals_a_budget_pause_notice() {
        use crate::ports::EventLog;
        use crate::ports::types::EventSeq;
        use crate::store::FsEventLog;

        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let outcome = crate::harness::built_in::TurnOutcome {
            reply: "checkpoint".to_string(),
            steps: Vec::new(),
            hit_iteration_cap: false,
            // Test fixture, not the ACP fold (PR #1880 review).
            abnormal_stop: None,
            // Issue #1906: this fixtures a BUDGET pause, not a spend halt — the
            // halt sibling is pinned by `schedule_fired_journals_halt_notices`.
            halted_for_spend: None,
            budget_paused: Some(crate::harness::BudgetPause {
                agent: "ceo".to_string(),
                summary: "the provider is exhausted".to_string(),
            }),
        };
        let brain = brain_with_queue_and_events(dir.path(), Default::default(), log.clone())
            .with_default_engine(Some(Arc::new(FixedOutcomeTurn {
                outcome,
                approval_requests: None,
            })));
        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::ScheduleFired {
                    cron: "0 9 * * *".into(),
                    prompt: "daily standup".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        // Issue #1906: a scheduled tick that pauses for lack of credits must
        // not present the interrupted turn as a completed answer — the primary
        // bubble carries the pause placeholder and a system notice follows it.
        assert_eq!(result.channel_responses.len(), 2);
        assert_eq!(
            result.channel_responses[0].text,
            BUDGET_PAUSED_PLACEHOLDER_REPLY
        );
        assert!(
            result.channel_responses[1]
                .text
                .starts_with(BUDGET_PAUSE_NOTICE_PREFIX)
        );
        assert!(
            result
                .channel_responses
                .iter()
                .skip(1)
                .all(|response| response.agent.as_deref() == Some(crate::ports::SYSTEM_AUTHOR))
        );
        let events = log
            .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
            .await
            .expect("read events");
        let replies: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.event, CompanyEvent::AgentReply { .. }))
            .collect();
        assert_eq!(
            replies.len(),
            2,
            "the pause placeholder and notice are durable"
        );
    }

    #[tokio::test]
    async fn schedule_fired_journals_approval_overflow_notice() {
        use crate::ports::EventLog;
        use crate::ports::types::EventSeq;
        use crate::store::FsEventLog;

        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        let brain = brain_with_queue_and_events(dir.path(), requests.clone(), log.clone())
            .with_default_engine(Some(Arc::new(FixedOutcomeTurn {
                outcome: crate::harness::built_in::TurnOutcome {
                    reply: "checkpoint".to_string(),
                    steps: Vec::new(),
                    hit_iteration_cap: false,
                    // Test fixture, not the ACP fold (PR #1880 review).
                    abnormal_stop: None,
                    halted_for_spend: None,
                    // Added by #1846 after these fixtures were written.
                    budget_paused: None,
                },
                approval_requests: Some(requests.clone()),
            })));
        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::ScheduleFired {
                    cron: "0 9 * * *".into(),
                    prompt: "daily standup".into(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 2);
        let notice = &result.channel_responses[1];
        assert!(
            notice.text.contains("further gated tool call"),
            "{}",
            notice.text
        );
        assert_eq!(notice.agent.as_deref(), Some(crate::ports::SYSTEM_AUTHOR));
        assert_eq!(requests.queued(), 0);
        let events = log
            .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
            .await
            .expect("read events");
        assert!(events.iter().any(|event| {
            matches!(&event.event, CompanyEvent::AgentReply { text, .. } if text.contains("further gated tool call"))
        }));
    }

    #[tokio::test]
    async fn no_events_still_acknowledges() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        let result = brain
            .run_cycle(request(Vec::new()), &NoopHost)
            .await
            .expect("cycle runs");
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].text, "Acknowledged.");
        // Issue #966, asserted here rather than only on `system_notice`: this
        // drives the real cycle, so it pins that the fallback *calls* the
        // constructor. Asserting the constructor alone leaves the call site free
        // to go back to an inline bubble with no author, which is the shape that
        // caused the defect.
        assert_eq!(
            result.channel_responses[0].agent.as_deref(),
            Some(crate::ports::SYSTEM_AUTHOR),
            "the runtime's own fallback is authored by the runtime, not by its destination"
        );
    }

    #[test]
    fn responder_defaults_to_first_roster_agent() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        assert_eq!(brain.responder, "ceo");
        let brain = brain.with_responder("cfo");
        assert_eq!(brain.responder, "cfo");
    }

    // --- Task dispatch ------------------------------------------------------

    use crate::ports::TaskStore;

    /// A two-agent record so assignee routing has somewhere to route.
    fn record_two() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds it."
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
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

    /// A brain wired to a real task store (shared handle returned for seeding /
    /// asserting), over the offline mock provider.
    fn brain_with_tasks(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        brain_with_tasks_notified(dir, false)
    }

    /// Same as [`brain_with_tasks`], but also wires the task store as the
    /// notification store (issue #1865, PR #1883 review comment 3878668326):
    /// [`FsOps`] implements both, so a test can seed a card, drive a cycle,
    /// and then read back any `dispatch_failed` row a refusal filed.
    fn brain_with_tasks_notified(
        dir: &std::path::Path,
        notify: bool,
    ) -> (HarnessBrain, Arc<FsOps>) {
        let tasks = Arc::new(FsOps::new(dir));
        let deps = HarnessDeps {
            notifications: if notify { Some(tasks.clone()) } else { None },
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks.clone()),
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_two()),
            tasks,
        )
    }

    /// A model whose every call fails with the exact wire shape
    /// `is_top_level_budget_exhausted` recognises (issue #1846 review, Codex
    /// #3864988168) — the same body `a_top_level_budget_exhaustion_pauses_
    /// gracefully_and_parks_a_reissue_marker` in `mod.rs` scripts, reused here
    /// to prove the DISPATCHED-CARD path settles on the pause rather than
    /// completing.
    struct BudgetExhaustedProvider;

    #[async_trait]
    impl ChatModel<()> for BudgetExhaustedProvider {
        async fn invoke(
            &self,
            _state: &(),
            _request: ModelRequest,
        ) -> tinyagents::Result<ModelResponse> {
            Err(tinyagents::TinyAgentsError::Model(
                "USER_INSUFFICIENT_CREDITS: insufficient budget for this account — add credits \
                 to continue"
                    .to_string(),
            ))
        }
    }

    impl HarnessModel for BudgetExhaustedProvider {
        fn telemetry_provider_id(&self) -> String {
            "scripted".to_string()
        }
    }

    /// As [`brain_with_tasks`], but every model call fails with a
    /// budget-exhausted body (issue #1846 review, Codex #3864988168) —
    /// otherwise byte-identical, so the only variable a test built on this
    /// exercises is how the dispatch path reacts to that one failure shape.
    fn brain_with_tasks_and_budget_exhausted_provider(
        dir: &std::path::Path,
    ) -> (HarnessBrain, Arc<FsOps>) {
        let tasks = Arc::new(FsOps::new(dir));
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(BudgetExhaustedProvider),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks.clone()),
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_two()),
            tasks,
        )
    }

    /// **The regression.** Issue #1846 review (Codex #3864988168): `run_task`
    /// never inspected `outcome.budget_paused` before this fix — a dispatched
    /// card whose model call ran out of credits fell straight into the
    /// `None => { ... None => settle(Completed) }` arm, since a budget pause
    /// carries an `Ok(TurnOutcome)` with no delegation queued, and landed in
    /// `in_review` looking like a finished, reviewable result instead of the
    /// graceful pause the operator-chat path already gave the same failure.
    #[tokio::test]
    async fn a_dispatched_tasks_budget_exhaustion_pauses_rather_than_completes() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks_and_budget_exhausted_provider(dir.path());
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-1", "engineer"))
            .await
            .expect("seed");
        // Driven directly, not through `run_cycle`, so the roster has to be
        // built explicitly — see
        // `dispatched_card_with_an_origin_stops_in_review_and_still_posts_back`.
        brain
            .pool
            .ensure(&brain.record(), &brain.deps)
            .await
            .expect("roster");

        brain.run_task("t-1", None).await.expect("run");

        let settled = only_card(&tasks).await;
        assert_eq!(
            settled.column, COLUMN_PAUSED,
            "a budget-exhausted model call is a graceful pause, not a completed result — \
             it must not read on the board as a finished, reviewable card"
        );
        let note = settled.note.expect("note");
        assert!(
            note.contains("add credits") || note.contains("Add credits"),
            "the note must carry the actionable ask a genuine budget pause gives, not just \
             an opaque dispatch failure: {note}"
        );
    }

    /// As [`brain_with_tasks`], but the roster also carries an `eng` desk led by
    /// the engineer — the shape `delegate_to_desk` writes into a card's
    /// `assignee` (issue #205).
    fn brain_with_desk_tasks(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        let (brain, tasks) = brain_with_tasks(dir);
        let group_chats = toml::from_str::<CompanyManifest>(
            r#"
[company]
name = "Acme"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "eng"
name = "Engineering desk"
members = ["engineer"]
"#,
        )
        .expect("valid manifest")
        .group_chats;
        brain.mutate_record(|r| r.manifest.group_chats = group_chats);
        (brain, tasks)
    }

    /// As [`brain_with_tasks`], but with the artifact store wired to the same
    /// [`FsOps`] handle (it implements both), so a dispatch's versioned output
    /// is observable.
    fn brain_with_artifacts(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        brain_with_stores(dir, false)
    }

    /// As [`brain_with_artifacts`], but with the workspace store wired to the
    /// same [`FsOps`] handle too (it implements all three), so issue #552's
    /// dual write into the shared tree is observable.
    ///
    /// A separate constructor rather than a change to the one above: leaving
    /// `brain_with_artifacts` workspace-less is what keeps every pre-existing
    /// publish test on the artifact-only path, which is the guarantee that an
    /// unwired workspace behaves exactly as it did before this cell.
    fn brain_with_artifacts_and_workspace(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        brain_with_stores(dir, true)
    }

    fn brain_with_stores(
        dir: &std::path::Path,
        with_workspace: bool,
    ) -> (HarnessBrain, Arc<FsOps>) {
        let ops = Arc::new(FsOps::new(dir));
        let artifacts = ops.clone() as Arc<dyn crate::ports::artifacts::ArtifactStore>;
        brain_with_injected_artifacts(dir, ops, artifacts, with_workspace)
    }

    /// As [`brain_with_stores`], but with the artifact store supplied by the
    /// caller — so a test can make `upsert` refuse and observe what the publish
    /// drain did to the *tree* before it got there.
    ///
    /// That is the only way to pin issue #552's write ordering. An ordering
    /// described in a comment is not an ordering: the next refactor reorders it
    /// and nothing objects.
    fn brain_with_injected_artifacts(
        dir: &std::path::Path,
        ops: Arc<FsOps>,
        artifacts: Arc<dyn crate::ports::artifacts::ArtifactStore>,
        with_workspace: bool,
    ) -> (HarnessBrain, Arc<FsOps>) {
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(ops.clone()),
            artifacts: Some(artifacts),
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: with_workspace.then(|| ops.clone() as Arc<dyn crate::ports::WorkspaceStore>),
            workflow_runs: None,
            deep_trace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_two()),
            ops,
        )
    }

    // -- issue #552: the write ordering, proven by failure injection ---------

    /// An [`ArtifactStore`](crate::ports::artifacts::ArtifactStore) that refuses
    /// `upsert` from the Nth call onward, delegating everything else.
    ///
    /// The instrument the ordering tests need: with the artifact write made to
    /// fail at a chosen point, what the *tree* holds afterwards says
    /// unambiguously which surface was written first.
    struct FailingArtifacts {
        inner: Arc<FsOps>,
        /// How many `upsert` calls succeed before the rest refuse.
        allowed: std::sync::atomic::AtomicUsize,
        seen: std::sync::atomic::AtomicUsize,
    }

    impl FailingArtifacts {
        fn new(inner: Arc<FsOps>, allowed: usize) -> Arc<Self> {
            Arc::new(Self {
                inner,
                allowed: std::sync::atomic::AtomicUsize::new(allowed),
                seen: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        /// Let every later `upsert` through again, so a test can publish
        /// normally after the injected failure and watch the repair.
        fn heal(&self) {
            self.allowed
                .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl crate::ports::artifacts::ArtifactStore for FailingArtifacts {
        async fn list(
            &self,
            company: &CompanyId,
            task_id: Option<&str>,
        ) -> crate::Result<Vec<ArtifactRecord>> {
            crate::ports::artifacts::ArtifactStore::list(&*self.inner, company, task_id).await
        }
        async fn get(
            &self,
            company: &CompanyId,
            id: &str,
        ) -> crate::Result<Option<ArtifactRecord>> {
            crate::ports::artifacts::ArtifactStore::get(&*self.inner, company, id).await
        }
        async fn upsert(
            &self,
            company: &CompanyId,
            artifact: &ArtifactRecord,
        ) -> crate::Result<()> {
            let n = self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= self.allowed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::error::OpenCompanyError::Store(
                    "artifact store is down".to_string(),
                ));
            }
            crate::ports::artifacts::ArtifactStore::upsert(&*self.inner, company, artifact).await
        }
        async fn delete(&self, company: &CompanyId, id: &str) -> crate::Result<bool> {
            crate::ports::artifacts::ArtifactStore::delete(&*self.inner, company, id).await
        }
    }

    fn publish_of(source: &str, body: &str) -> crate::harness::publish::PendingPublish {
        crate::harness::publish::PendingPublish {
            agent: "maya".to_string(),
            source: source.to_string(),
            title: "Launch spec".to_string(),
            kind: crate::ports::artifacts::ArtifactKind::Markdown,
            note: None,
            payload: crate::harness::publish::PublishPayload::Text(body.to_string()),
        }
    }

    /// The named node under `agents/maya/t-1/`, with its body — the tree's own
    /// answer, read without going through the artifact chain at all.
    async fn note_in_tree(
        ops: &FsOps,
        company: &CompanyId,
        name: &str,
    ) -> Option<(String, String)> {
        use crate::ports::workspace::WorkspaceStore;
        let nodes = WorkspaceStore::tree(ops, company).await.unwrap();
        let found = nodes.iter().find(|n| n.name == name)?;
        let (_, body) = WorkspaceStore::read(ops, company, &found.id)
            .await
            .unwrap()?;
        Some((found.id.clone(), body))
    }

    /// **Chain first, proven.** A re-publish whose artifact write fails must
    /// leave the note holding the PREVIOUS body — the version was stored before
    /// the tree was touched, so a refused version means an untouched tree.
    ///
    /// The opposite ordering is what this rules out, and it is not a stylistic
    /// difference: a note one version ahead of the chain shows the operator
    /// content the version history has no record of, which makes
    /// `human_edit_diff` quietly wrong rather than loudly broken — the same rot
    /// the artifact port exists to prevent, arriving through the tree instead.
    #[tokio::test]
    async fn a_refused_republish_leaves_the_note_on_the_previous_body() {
        use crate::ports::artifacts::ArtifactStore;

        let dir = tempfile::tempdir().unwrap();
        let ops = Arc::new(FsOps::new(dir.path()));
        // v1 costs two upserts: the record, then the link once the node exists.
        let artifacts = FailingArtifacts::new(ops.clone(), 2);
        let (brain, _) =
            brain_with_injected_artifacts(dir.path(), ops.clone(), artifacts.clone(), true);
        let company = CompanyId::new("acme");
        let c = card("t-1", "maya");

        brain
            .record_published_artifacts(&c, "maya", vec![publish_of("launch.md", "v1")], None)
            .await
            .expect("the first publish lands");
        let (node_id, body) = note_in_tree(&ops, &company, "launch.md")
            .await
            .expect("v1 is in the tree");
        assert_eq!(body, "v1");

        // Now the artifact store refuses. The re-publish must fail *before*
        // reaching the tree.
        brain
            .record_published_artifacts(&c, "maya", vec![publish_of("launch.md", "v2")], None)
            .await
            .expect_err("a refused artifact write fails the publish");

        let (still, body) = note_in_tree(&ops, &company, "launch.md")
            .await
            .expect("the note is still there");
        assert_eq!(still, node_id, "no rival note was minted");
        assert_eq!(
            body, "v1",
            "the tree must not hold a body the version history never recorded"
        );
        // And the chain is unchanged too — one version, not a half-written two.
        let stored = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .unwrap();
        assert_eq!(stored[0].versions.len(), 1);
        assert_eq!(stored[0].latest().unwrap().body, "v1");
    }

    /// The same ordering on a **fresh** publish: an artifact write that fails
    /// creates nothing in the tree at all.
    ///
    /// This is what makes the fresh path's residual an *orphan note* rather
    /// than a lost deliverable — a node is only ever created for a deliverable
    /// that is already recorded, so this path cannot leave a file in the tree
    /// with no artifact behind it.
    #[tokio::test]
    async fn a_refused_first_publish_creates_nothing_in_the_tree() {
        use crate::ports::workspace::WorkspaceStore;

        let dir = tempfile::tempdir().unwrap();
        let ops = Arc::new(FsOps::new(dir.path()));
        let artifacts = FailingArtifacts::new(ops.clone(), 0);
        let (brain, _) = brain_with_injected_artifacts(dir.path(), ops.clone(), artifacts, true);
        let company = CompanyId::new("acme");

        brain
            .record_published_artifacts(
                &card("t-1", "maya"),
                "maya",
                vec![publish_of("launch.md", "v1")],
                None,
            )
            .await
            .expect_err("a refused artifact write fails the publish");

        assert!(
            note_in_tree(&ops, &company, "launch.md").await.is_none(),
            "no note may exist for a deliverable that was never recorded"
        );
        assert!(
            WorkspaceStore::tree(&*ops, &company)
                .await
                .unwrap()
                .is_empty(),
            "not even the agent's folder is minted for a publish that failed"
        );
    }

    /// The fresh path's one residual, and its repair.
    ///
    /// A fresh publish has no node id to inherit, so v1 is stored unlinked and
    /// a *second* artifact write stamps the link. If that second write fails,
    /// both surfaces hold the body and only the pointer between them is
    /// missing. That is deliberately warned-and-tolerated rather than fatal:
    /// failing would discard the rest of the batch to report a link that the
    /// next publish repairs.
    ///
    /// The repair is the half worth proving. `materialize` find-or-creates by
    /// path, so the next publish of the same source **re-adopts the very same
    /// note** rather than duplicating it — which is what makes the orphan
    /// self-healing rather than permanent.
    #[tokio::test]
    async fn an_unlinked_first_publish_is_repaired_by_the_next_one() {
        use crate::ports::artifacts::ArtifactStore;
        use crate::ports::workspace::WorkspaceStore;

        let dir = tempfile::tempdir().unwrap();
        let ops = Arc::new(FsOps::new(dir.path()));
        // Exactly one upsert succeeds: the record lands, the link does not.
        let artifacts = FailingArtifacts::new(ops.clone(), 1);
        let (brain, _) =
            brain_with_injected_artifacts(dir.path(), ops.clone(), artifacts.clone(), true);
        let company = CompanyId::new("acme");
        let c = card("t-1", "maya");

        brain
            .record_published_artifacts(&c, "maya", vec![publish_of("launch.md", "v1")], None)
            .await
            .expect("a missing link must not fail the publish");

        // Both surfaces hold the body; only the pointer is absent.
        let (orphan, body) = note_in_tree(&ops, &company, "launch.md")
            .await
            .expect("the note was still written");
        assert_eq!(body, "v1");
        let stored = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .unwrap();
        assert_eq!(stored[0].latest().unwrap().body, "v1");
        assert_eq!(
            stored[0].workspace_node_id(),
            None,
            "this is the orphan: recorded and written, but not linked"
        );

        // The next publish of the same source repairs it.
        artifacts.heal();
        let nodes_before = WorkspaceStore::tree(&*ops, &company).await.unwrap().len();
        brain
            .record_published_artifacts(&c, "maya", vec![publish_of("launch.md", "v2")], None)
            .await
            .expect("the repairing publish lands");

        let stored = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .unwrap();
        assert_eq!(stored[0].versions.len(), 2, "one record, extended");
        assert_eq!(
            stored[0].workspace_node_id(),
            Some(orphan.as_str()),
            "the very same note is re-adopted, which is what makes the orphan self-healing"
        );
        assert_eq!(
            WorkspaceStore::tree(&*ops, &company).await.unwrap().len(),
            nodes_before,
            "re-adoption, not duplication: no rival note beside the orphan"
        );
        assert_eq!(
            WorkspaceStore::read(&*ops, &company, &orphan)
                .await
                .unwrap()
                .unwrap()
                .1,
            "v2"
        );
    }

    /// The ordinary re-publish stores **once**, not twice. The second artifact
    /// write exists only for a link that actually changed — a fresh publish, or
    /// a note the operator deleted — and a re-publish that reuses its note has
    /// nothing to restate.
    #[tokio::test]
    async fn an_ordinary_republish_writes_the_artifact_once() {
        let dir = tempfile::tempdir().unwrap();
        let ops = Arc::new(FsOps::new(dir.path()));
        let artifacts = FailingArtifacts::new(ops.clone(), usize::MAX);
        let (brain, _) =
            brain_with_injected_artifacts(dir.path(), ops.clone(), artifacts.clone(), true);
        let c = card("t-1", "maya");

        brain
            .record_published_artifacts(&c, "maya", vec![publish_of("launch.md", "v1")], None)
            .await
            .unwrap();
        // v1: the record, then the link once the node id exists.
        assert_eq!(
            artifacts.seen.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a fresh publish stores the record, then stamps the link"
        );

        brain
            .record_published_artifacts(&c, "maya", vec![publish_of("launch.md", "v2")], None)
            .await
            .unwrap();
        assert_eq!(
            artifacts.seen.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "a re-publish inherits its note, so one store is enough"
        );
    }
    // -- issue #552: a published deliverable reaches the shared workspace -----

    /// The headline of #552. A published file used to reach the artifact store
    /// and stop, which left it visible only in the Artifacts tab of one card.
    /// It must now also land in the shared tree, under the publishing agent's
    /// own folder, attributed to that agent — and the version that wrote it
    /// must carry the node id, which is the link the console's cross-link and
    /// every later mirror depend on.
    #[tokio::test]
    async fn a_publish_lands_in_the_shared_workspace_and_the_version_names_the_node() {
        use crate::harness::publish::PendingPublish;
        use crate::ports::artifacts::ArtifactStore;
        use crate::ports::workspace::{WorkspaceOrigin, WorkspaceStore};

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts_and_workspace(dir.path());
        let company = CompanyId::new("acme");

        brain
            .record_published_artifacts(
                &card("t-1", "maya"),
                "maya",
                vec![PendingPublish {
                    agent: "maya".to_string(),
                    source: "specs/launch.md".to_string(),
                    title: "Launch spec".to_string(),
                    kind: crate::ports::artifacts::ArtifactKind::Markdown,
                    note: None,
                    payload: crate::harness::publish::PublishPayload::Text(
                        "the spec body".to_string(),
                    ),
                }],
                Some("run-1"),
            )
            .await
            .expect("records");

        let listed = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .unwrap();
        let node_id = listed[0]
            .workspace_node_id()
            .expect("the version must name the node its body was mirrored into");

        let (node, body) = WorkspaceStore::read(&*ops, &company, node_id)
            .await
            .unwrap()
            .expect("the node exists in the shared tree");
        assert_eq!(body, "the spec body");
        assert_eq!(node.name, "launch.md");
        assert_eq!(
            node.created_by,
            WorkspaceOrigin::Agent {
                id: "maya".to_string()
            },
            "the tree must say which teammate produced this"
        );
    }

    /// The "zero tool work" claim in #552, proven rather than asserted: a
    /// second agent reads the first agent's deliverable through the ordinary
    /// `workspace_read` path, with nothing published-specific involved.
    ///
    /// The read goes through the *same* index-and-resolve the tool uses (a
    /// company-scoped `tree()` then a `read()` by id), so what this pins is
    /// that the node is reachable by path from the shared tree — which is
    /// exactly what makes it readable by every teammate.
    #[tokio::test]
    async fn a_second_agent_can_read_what_the_first_published() {
        use crate::harness::publish::PendingPublish;
        use crate::ports::workspace::WorkspaceStore;

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts_and_workspace(dir.path());
        let company = CompanyId::new("acme");

        brain
            .record_published_artifacts(
                &card("t-1", "maya"),
                "maya",
                vec![PendingPublish {
                    agent: "maya".to_string(),
                    source: "launch.md".to_string(),
                    title: "Launch spec".to_string(),
                    kind: crate::ports::artifacts::ArtifactKind::Markdown,
                    note: None,
                    payload: crate::harness::publish::PublishPayload::Text(
                        "what maya produced".to_string(),
                    ),
                }],
                None,
            )
            .await
            .expect("records");

        // Agent B knows nothing about the artifact. It walks the shared tree by
        // path, exactly as `workspace_list` / `workspace_read` do.
        let nodes = WorkspaceStore::tree(&*ops, &company).await.unwrap();
        let name_of = |id: &str| nodes.iter().find(|n| n.id == id).map(|n| n.name.clone());
        let found = nodes
            .iter()
            .find(|n| {
                n.name == "launch.md"
                    && n.parent_id
                        .as_deref()
                        .and_then(name_of)
                        // Issue #1687: the task folder is named for the work
                        // and keyed by the card id — `<title>.<id>`, not the
                        // bare id — so browsing by path lands on that name.
                        .is_some_and(|parent| parent == "ship-the-thing.t-1")
            })
            .expect("agent B finds the deliverable by browsing the shared tree");

        let (_, body) = WorkspaceStore::read(&*ops, &company, &found.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(body, "what maya produced");
    }

    /// A re-publish revises the SAME node rather than opening a rival beside
    /// it, so the operator's open tab and any link to it keep working — and
    /// the new version carries the same node id, which is what lets the next
    /// re-publish find it again.
    #[tokio::test]
    async fn a_republish_updates_the_same_node() {
        use crate::harness::publish::PendingPublish;
        use crate::ports::artifacts::ArtifactStore;
        use crate::ports::workspace::WorkspaceStore;

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts_and_workspace(dir.path());
        let company = CompanyId::new("acme");
        let c = card("t-1", "maya");
        let publish = |body: &str| PendingPublish {
            agent: "maya".to_string(),
            source: "specs/launch.md".to_string(),
            title: "Launch spec".to_string(),
            kind: crate::ports::artifacts::ArtifactKind::Markdown,
            note: None,
            payload: crate::harness::publish::PublishPayload::Text(body.to_string()),
        };

        brain
            .record_published_artifacts(&c, "maya", vec![publish("v1")], Some("run-1"))
            .await
            .unwrap();
        let first_node = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .unwrap()[0]
            .workspace_node_id()
            .expect("v1 named a node")
            .to_string();
        let tree_before = WorkspaceStore::tree(&*ops, &company).await.unwrap().len();

        brain
            .record_published_artifacts(&c, "maya", vec![publish("v2")], Some("run-2"))
            .await
            .unwrap();

        let record = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .unwrap()[0]
            .clone();
        assert_eq!(record.versions.len(), 2, "one record, two versions");
        assert_eq!(
            record.workspace_node_id(),
            Some(first_node.as_str()),
            "the second version must name the node the first one already had"
        );
        assert_eq!(
            WorkspaceStore::tree(&*ops, &company).await.unwrap().len(),
            tree_before,
            "a re-publish must create no new nodes"
        );
        assert_eq!(
            WorkspaceStore::read(&*ops, &company, &first_node)
                .await
                .unwrap()
                .unwrap()
                .1,
            "v2",
            "the node holds the current body"
        );
    }

    /// An unwired workspace must behave **exactly** as before this cell: the
    /// artifact is recorded, nothing is attempted against a tree that does not
    /// exist, and no version claims a node.
    ///
    /// This is what keeps every pre-#552 publish test honest, since they all
    /// run on this path.
    #[tokio::test]
    async fn without_a_workspace_store_the_publish_path_is_unchanged() {
        use crate::harness::publish::PendingPublish;
        use crate::ports::artifacts::ArtifactStore;

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts(dir.path());
        let company = CompanyId::new("acme");

        brain
            .record_published_artifacts(
                &card("t-1", "maya"),
                "maya",
                vec![PendingPublish {
                    agent: "maya".to_string(),
                    source: "specs/launch.md".to_string(),
                    title: "Launch spec".to_string(),
                    kind: crate::ports::artifacts::ArtifactKind::Markdown,
                    note: None,
                    payload: crate::harness::publish::PublishPayload::Text("body".to_string()),
                }],
                None,
            )
            .await
            .expect("the artifact is still recorded");

        let listed = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].workspace_node_id(),
            None,
            "with no tree to mirror into, a version names no node"
        );
    }

    /// A deliverable is never dropped for tree bookkeeping. When the node
    /// cannot be written — here an operator's *file* squatting the `Artifacts`
    /// root name, which the fail-closed resolver refuses rather than guesses —
    /// the artifact is still recorded, just without a node id.
    ///
    /// The opposite behaviour (propagating the error) would lose an explicitly
    /// published file because a folder could not be made, which is the worse
    /// of the two failures by a wide margin.
    #[tokio::test]
    async fn a_failed_node_write_still_records_the_artifact() {
        use crate::company::workspace_scaffold::ARTIFACTS_ROOT;
        use crate::harness::publish::PendingPublish;
        use crate::ports::artifacts::ArtifactStore;
        use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts_and_workspace(dir.path());
        let company = CompanyId::new("acme");

        // A *file* named `Artifacts` at the workspace root — the root a publish
        // now resolves through. The minter refuses to resolve a folder through
        // it rather than clobbering an operator's note.
        WorkspaceStore::create(
            &*ops,
            &company,
            &WorkspaceNode {
                id: crate::ports::generate_id(),
                name: ARTIFACTS_ROOT.to_string(),
                kind: NodeKind::File,
                parent_id: None,
                updated_at_millis: now_millis(),
                created_by: WorkspaceOrigin::Operator,
                updated_by: WorkspaceOrigin::Operator,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
            },
            Some("an operator's note, in the way"),
        )
        .await
        .unwrap();

        let written = brain
            .record_published_artifacts(
                &card("t-1", "maya"),
                "maya",
                vec![PendingPublish {
                    agent: "maya".to_string(),
                    source: "launch.md".to_string(),
                    title: "Launch spec".to_string(),
                    kind: crate::ports::artifacts::ArtifactKind::Markdown,
                    note: None,
                    payload: crate::harness::publish::PublishPayload::Text(
                        "the deliverable".to_string(),
                    ),
                }],
                None,
            )
            .await
            .expect("a tree that refuses must not fail the publish");

        assert_eq!(written.len(), 1, "the deliverable is still recorded");
        let listed = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .unwrap();
        assert_eq!(listed[0].latest().unwrap().body, "the deliverable");
        assert_eq!(
            listed[0].workspace_node_id(),
            None,
            "no node was written, so no version may claim one"
        );
    }
    fn card(id: &str, assignee: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: "Ship the thing".to_string(),
            note: None,
            column: "in_progress".to_string(),
            priority: "high".to_string(),
            assignee: assignee.to_string(),
            updated_at_millis: 0,
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        }
    }

    // ── Issue #151 §3.2: a finished card answers where it was asked ──────

    // The post-back's *text* rules — title, landing status, note folding,
    // whitespace-only notes — moved with the renderer to
    // `crate::harness::lifecycle` (issue #186), which owns them now and covers
    // each case plus the new assignee-credit rule. What stays here is the
    // wiring: that `run_task` reaches the relay at all, and attributes it to
    // the orchestrator.

    /// The compatibility guarantee: a card with no remembered origin — one made
    /// straight on the board, or written before `origin_chat_id` existed —
    /// posts back nowhere and behaves exactly as it did before.
    #[tokio::test]
    async fn a_card_with_no_origin_posts_back_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        // A roster assignee: since #205 an off-roster one is refused outright,
        // which would satisfy the no-post-back assertion below without ever
        // running the dispatch this test is about.
        let mut c = card("t-no-origin", "engineer");
        c.origin_chat_id = None;
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");

        let posted = brain.run_task("t-no-origin", None).await.expect("run");
        assert!(
            posted.is_none(),
            "a card with no originating thread must not post back"
        );
        // The note is still the durable record.
        assert!(only_card(&tasks).await.note.is_some());
    }

    /// …and one that does remember its origin answers there, threaded with
    /// `reply_to` and — since issue #186 — attributed to the **orchestrator**
    /// rather than to the assignee that did the work.
    #[tokio::test]
    async fn a_card_with_an_origin_posts_back_to_that_thread() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        // A roster assignee, deliberately: an off-roster one falls back to the
        // default responder (`task_responder`), which in this fixture *is* the
        // orchestrator — so the credit would be correctly suppressed and this
        // test would prove nothing about the one-voice relay.
        let mut c = card("t-origin", "engineer");
        c.origin_chat_id = Some("strategy".to_string());
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");

        let posted = brain
            .run_task("t-origin", None)
            .await
            .expect("run")
            .expect("a card with an origin must post back");
        assert_eq!(
            posted.reply_to.as_ref().map(|r| r.chat_id.as_str()),
            Some("strategy")
        );
        // Issue #186: one voice. The bubble belongs to the orchestrator, and
        // the assignee that ran the card is credited in the text instead of
        // speaking to the operator directly.
        assert_eq!(
            posted.channel,
            brain.orchestrator(),
            "the orchestrator relays a finished card, not the assignee"
        );
        assert_ne!(
            posted.channel, "engineer",
            "the assignee must not address the operator directly"
        );
        assert!(posted.text.contains("Ship the thing"), "{}", posted.text);
        assert!(
            posted.text.contains("engineer"),
            "the relay must still credit who did the work: {}",
            posted.text
        );
        // A dispatched card discards its steps into the note.
        assert!(posted.steps.is_empty());
    }

    async fn only_card(tasks: &Arc<FsOps>) -> TaskRecord {
        tasks
            .list(&CompanyId::new("acme"))
            .await
            .expect("list")
            .into_iter()
            .next()
            .expect("one card")
    }

    /// A dispatched **board-created** card (no `origin_chat_id`) runs a turn and
    /// moves to `in_review` — the operator who made it is the reviewer — with
    /// its result folded into the note under the responder that ran it.
    #[tokio::test]
    async fn task_dispatch_runs_and_moves_to_in_review() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        assert_eq!(moved.column, "in_review");
        let note = moved.note.expect("result written to note");
        // Default responder (first roster agent) ran it, and the mock provider
        // echoes the instruction (the card title) back into the reply.
        assert!(note.contains("[ceo]"), "{note:?}");
        assert!(note.contains("Ship the thing"), "{note:?}");
    }

    // ── Issue #337: every finished card stops for a person ────────────────

    /// **Rewritten by #337.** This used to pin the opposite: a card spawned by
    /// a delegating turn (so it carries an `origin_chat_id`) completed straight
    /// to `done`, on #171's argument that nobody was watching the board for it.
    ///
    /// The operator decision of 2026-08-05 removed every automatic route to
    /// Done, so it now stops in `in_review` like any other card. The thing #171
    /// actually cared about — that the originating conversation gets its answer
    /// rather than waiting on a board nobody is reading — is unaffected and is
    /// asserted here: the post-back still fires, and it now says the card is
    /// ready for review instead of claiming it is finished.
    #[tokio::test]
    async fn dispatched_card_with_an_origin_stops_in_review_and_still_posts_back() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        // A roster assignee: since #205 an off-roster one never runs a turn, so
        // it would settle to `todo` and prove nothing about the terminal.
        let mut c = card("t-origin", "engineer");
        c.origin_chat_id = Some("strategy".to_string());
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");
        // `run_task` is driven directly here rather than through `run_cycle`,
        // so the roster the turn runs on has to be built explicitly. Without it
        // every dispatch fails with "company not found" and settles to
        // `todo` — which still satisfies this test's post-back assertions
        // while proving nothing about the terminal column.
        brain
            .pool
            .ensure(&brain.record(), &brain.deps)
            .await
            .expect("roster");

        let posted = brain
            .run_task("t-origin", None)
            .await
            .expect("run")
            .expect("a card with an origin posts back");

        let moved = only_card(&tasks).await;
        assert_eq!(
            moved.column, COLUMN_IN_REVIEW,
            "Done is a person's decision; no dispatch may reach it on its own"
        );
        // The note stays the durable record of what came back.
        assert!(moved.note.expect("note").contains("Ship the thing"));
        // …and the bubble answers in the originating thread either way, which
        // is what the handoff was actually waiting on.
        assert!(posted.text.contains("ready for review"), "{}", posted.text);
        assert!(!posted.text.contains("is done"), "{}", posted.text);
    }

    /// **The headline of #244, stated as its own test.** This used to assert
    /// the opposite — that a completed dispatch always mints an artifact from
    /// its chat reply.
    ///
    /// It does not any more. A run that published nothing yields **no
    /// artifact**, and that is a first-class outcome rather than a gap. The old
    /// behaviour is exactly what made the Artifacts tab present refusals and
    /// blocker messages as deliverables: capture was gated on run disposition
    /// and never on whether anything had been produced.
    ///
    /// Nothing is lost. The reply still reaches the card note, the timeline, the
    /// completion event and the run trace — five records, none of which claims
    /// to be a deliverable.
    #[tokio::test]
    async fn a_completed_run_that_published_nothing_records_no_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts(dir.path());
        // Empty assignee → the default responder, so the turn actually runs.
        let mut c = card("t-origin", "");
        c.origin_chat_id = Some("strategy".to_string());
        ops.upsert(&CompanyId::new("acme"), &c).await.expect("seed");

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t-origin".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&ops).await;
        assert_eq!(
            moved.column, COLUMN_IN_REVIEW,
            "since #337 every finished card stops for a person"
        );
        let artifacts = crate::ports::artifacts::ArtifactStore::list(
            &*ops,
            &CompanyId::new("acme"),
            Some("t-origin"),
        )
        .await
        .expect("list");
        assert!(
            artifacts.is_empty(),
            "a run that published nothing has no deliverable: {artifacts:?}"
        );
        // …and the reply is still recorded where it belongs: on the card.
        assert!(
            moved.note.expect("note").contains("Ship the thing"),
            "the reply must survive even though it is not an artifact"
        );
    }

    /// **The identity-vs-recency regression** — the second defect #244 names,
    /// and the one with teeth.
    ///
    /// The old extend target was `max_by_key(updated_at_millis)`: whichever
    /// artifact on the card had been touched most recently. An **operator edit**
    /// bumps that timestamp. So an operator who tidied the invoice made the
    /// invoice the target for the agent's next write to the spec — the spec's v2
    /// landed as the invoice's v3, and `human_edit_diff` then reported a human
    /// rewriting a document they had never opened.
    ///
    /// The setup here reproduces exactly that: publish two files, operator-edit
    /// the **second** so it is unambiguously the most recent, then republish the
    /// **first**. Under recency this appends to the invoice. Under identity it
    /// extends the spec, and the invoice is untouched.
    #[tokio::test]
    async fn a_republish_extends_by_identity_not_by_whatever_was_edited_last() {
        use crate::harness::publish::PendingPublish;
        use crate::ports::artifacts::{ArtifactAuthor, ArtifactStore};

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts(dir.path());
        let company = CompanyId::new("acme");
        let c = card("t-1", "maya");
        let publish = |source: &str, body: &str| PendingPublish {
            agent: "maya".to_string(),
            source: source.to_string(),
            title: source.to_string(),
            kind: crate::ports::artifacts::ArtifactKind::Markdown,
            note: None,
            payload: crate::harness::publish::PublishPayload::Text(body.to_string()),
        };

        // Run 1 publishes both files.
        let ids = brain
            .record_published_artifacts(
                &c,
                "maya",
                vec![
                    publish("specs/launch.md", "# Spec v1"),
                    publish("billing/invoice.md", "# Invoice v1"),
                ],
                Some("run-1"),
            )
            .await
            .expect("records");
        assert_eq!(ids.len(), 2, "two files, two records");

        let by_source = |list: &[crate::ports::artifacts::ArtifactRecord], source: &str| {
            list.iter()
                .find(|a| a.source.as_deref() == Some(source))
                .expect("record for source")
                .clone()
        };
        let listed = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .unwrap();
        let mut invoice = by_source(&listed, "billing/invoice.md");

        // The operator edits the INVOICE, making it the most recently updated
        // artifact on the card. This is the trap.
        invoice.push_version(
            "# Invoice v1, corrected",
            ArtifactAuthor::Operator,
            "operator",
            now_millis() + 1_000,
            Some("operator edit before approval".to_string()),
        );
        ArtifactStore::upsert(&*ops, &company, &invoice)
            .await
            .unwrap();

        // Run 2 republishes the SPEC only.
        brain
            .record_published_artifacts(
                &c,
                "maya",
                vec![publish("specs/launch.md", "# Spec v2")],
                Some("run-2"),
            )
            .await
            .expect("records");

        let after = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .unwrap();
        assert_eq!(after.len(), 2, "no duplicate record was opened");

        let spec = by_source(&after, "specs/launch.md");
        assert_eq!(
            spec.versions.len(),
            2,
            "the republished path must extend its OWN record"
        );
        assert_eq!(spec.latest().unwrap().body, "# Spec v2");
        assert_eq!(spec.latest().unwrap().run_id.as_deref(), Some("run-2"));
        assert_eq!(
            spec.versions[0].run_id.as_deref(),
            Some("run-1"),
            "an earlier attempt keeps the attempt that wrote it"
        );

        let invoice = by_source(&after, "billing/invoice.md");
        assert_eq!(
            invoice.versions.len(),
            2,
            "the agent's spec must not have landed on the invoice"
        );
        assert_eq!(invoice.latest().unwrap().body, "# Invoice v1, corrected");
        assert_eq!(
            invoice.latest().unwrap().author,
            ArtifactAuthor::Operator,
            "the invoice's newest version is still the human's"
        );
        // And the reason all of this matters: the human-edit diff still says
        // what a human actually did, on each document separately.
        let diff = invoice.human_edit_diff().expect("the operator edited it");
        assert_eq!((diff.from_version, diff.to_version), (1, 2));
        assert!(
            spec.human_edit_diff().is_none(),
            "nobody edited the spec, so it must report no human edit"
        );
    }

    /// Two publishes of the same path within one run extend one record rather
    /// than opening two — the working set the loop keeps has to stay current.
    #[tokio::test]
    async fn republishing_the_same_path_twice_in_one_run_extends_once() {
        use crate::harness::publish::PendingPublish;
        use crate::ports::artifacts::ArtifactStore;

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts(dir.path());
        let c = card("t-1", "maya");
        let publish = |body: &str| PendingPublish {
            agent: "maya".to_string(),
            source: "spec.md".to_string(),
            title: "spec.md".to_string(),
            kind: crate::ports::artifacts::ArtifactKind::Markdown,
            note: None,
            payload: crate::harness::publish::PublishPayload::Text(body.to_string()),
        };

        brain
            .record_published_artifacts(&c, "maya", vec![publish("draft"), publish("final")], None)
            .await
            .expect("records");

        let listed = ArtifactStore::list(&*ops, &CompanyId::new("acme"), Some("t-1"))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].versions.len(), 2);
        assert_eq!(listed[0].latest().unwrap().body, "final");
    }

    /// A store fault on an **explicit** publish propagates. The old path
    /// returned a silent `Ok(())`, which made a lost deliverable
    /// indistinguishable from a successful one.
    #[tokio::test]
    async fn a_store_error_on_a_published_file_propagates() {
        use crate::harness::publish::PendingPublish;
        use crate::ports::artifacts::{ArtifactRecord, ArtifactStore};

        /// Reads fine, refuses every write.
        struct BrokenArtifacts;
        #[async_trait]
        impl ArtifactStore for BrokenArtifacts {
            async fn list(
                &self,
                _: &CompanyId,
                _: Option<&str>,
            ) -> crate::Result<Vec<ArtifactRecord>> {
                Ok(Vec::new())
            }
            async fn get(&self, _: &CompanyId, _: &str) -> crate::Result<Option<ArtifactRecord>> {
                Ok(None)
            }
            async fn upsert(&self, _: &CompanyId, _: &ArtifactRecord) -> crate::Result<()> {
                Err(crate::error::OpenCompanyError::Store(
                    "the disk is full".to_string(),
                ))
            }
            async fn delete(&self, _: &CompanyId, _: &str) -> crate::Result<bool> {
                Ok(false)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let (mut brain, _ops) = brain_with_artifacts(dir.path());
        // Sole owner at this point — no turn has run, so nothing has cloned the
        // deps into a lane yet.
        Arc::get_mut(&mut brain.deps)
            .expect("the brain is the only holder of its deps before any turn")
            .artifacts = Some(Arc::new(BrokenArtifacts));

        let err = brain
            .record_published_artifacts(
                &card("t-1", "maya"),
                "maya",
                vec![PendingPublish {
                    agent: "maya".to_string(),
                    source: "spec.md".to_string(),
                    title: "spec.md".to_string(),
                    kind: crate::ports::artifacts::ArtifactKind::Markdown,
                    note: None,
                    payload: crate::harness::publish::PublishPayload::Text("# Spec".to_string()),
                }],
                None,
            )
            .await
            .expect_err("a lost deliverable must not read as a success");
        assert!(err.to_string().contains("the disk is full"), "{err}");
    }

    /// Issue #463, the headline. A publish made when this message already has a
    /// card files **onto that card** rather than minting a second one beside it.
    ///
    /// #445 minted unconditionally, which was right on its own and wrong beside
    /// #442's card-by-construction: one substantial ask that ended in a
    /// published file left two cards, and the reply bubble linked to the empty
    /// one because that is the card the turn opened.
    #[tokio::test]
    async fn a_publish_with_a_card_in_scope_files_onto_it_instead_of_minting() {
        use crate::harness::publish::PendingPublish;
        use crate::ports::artifacts::ArtifactStore;

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts(dir.path());
        let company = CompanyId::new("acme");
        // The card #442 opens for the work — no owner yet, exactly like the
        // To-do card the REST chat handler writes.
        let mut open = card("t-open", "");
        open.column = COLUMN_TODO.to_string();
        TaskStore::upsert(&*ops, &company, &open)
            .await
            .expect("seed");

        let filed = brain
            .file_publishes_on_card(
                "t-open",
                "writer",
                None,
                vec![PendingPublish {
                    agent: "writer".to_string(),
                    source: "memo.md".to_string(),
                    title: "Q3 board memo".to_string(),
                    kind: crate::ports::artifacts::ArtifactKind::Markdown,
                    note: None,
                    payload: crate::harness::publish::PublishPayload::Text("# Memo".to_string()),
                }],
            )
            .await
            .expect("files onto the card");

        assert_eq!(filed, "t-open", "no second card was minted");
        let cards = TaskStore::list(&*ops, &company).await.expect("list");
        assert_eq!(cards.len(), 1, "{cards:?}");
        assert_eq!(
            cards[0].column, COLUMN_IN_REVIEW,
            "a delivered file lands for a person to accept"
        );
        assert_eq!(
            cards[0].assignee, "writer",
            "an unowned card becomes the publisher's"
        );
        assert!(
            cards[0]
                .note
                .as_deref()
                .unwrap_or_default()
                .contains("memo.md"),
            "the note names what landed: {:?}",
            cards[0].note
        );
        let artifacts = ArtifactStore::list(&*ops, &company, Some("t-open"))
            .await
            .expect("list artifacts");
        assert_eq!(artifacts.len(), 1, "the deliverable is ON the card");
        assert_eq!(artifacts[0].title, "Q3 board memo");
    }

    /// …but filing a file never takes somebody's card away from them. Only an
    /// **unowned** card is claimed by the publisher.
    #[tokio::test]
    async fn filing_a_publish_leaves_an_owned_card_with_its_owner() {
        use crate::harness::publish::PendingPublish;

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts(dir.path());
        let company = CompanyId::new("acme");
        ops.upsert(&company, &card("t-owned", "maya"))
            .await
            .expect("seed");

        brain
            .file_publishes_on_card(
                "t-owned",
                "writer",
                None,
                vec![PendingPublish {
                    agent: "writer".to_string(),
                    source: "memo.md".to_string(),
                    title: "Memo".to_string(),
                    kind: crate::ports::artifacts::ArtifactKind::Markdown,
                    note: None,
                    payload: crate::harness::publish::PublishPayload::Text("# Memo".to_string()),
                }],
            )
            .await
            .expect("files onto the card");

        assert_eq!(only_card(&ops).await.assignee, "maya");
    }

    /// A card that vanished between the turn and the drain falls back to
    /// minting. The rule exists to stop a second card, not to lose a
    /// deliverable — dropping the artifact would be #445 all over again.
    #[tokio::test]
    async fn a_publish_onto_a_card_that_vanished_mints_one_rather_than_dropping_it() {
        use crate::harness::publish::PendingPublish;

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts(dir.path());
        let company = CompanyId::new("acme");

        let filed = brain
            .file_publishes_on_card(
                "t-gone",
                "writer",
                Some("strategy"),
                vec![PendingPublish {
                    agent: "writer".to_string(),
                    source: "memo.md".to_string(),
                    title: "Memo".to_string(),
                    kind: crate::ports::artifacts::ArtifactKind::Markdown,
                    note: None,
                    payload: crate::harness::publish::PublishPayload::Text("# Memo".to_string()),
                }],
            )
            .await
            .expect("mints a card instead");

        let cards = TaskStore::list(&*ops, &company).await.expect("list");
        assert_eq!(cards.len(), 1, "the deliverable still has a card");
        assert_eq!(cards[0].assignee, "writer");
        // The returned id must be the REPLACEMENT, not the id that is gone: the
        // caller links the operator's reply to it (#463 review).
        assert_eq!(
            filed, cards[0].id,
            "the returned id must name the card the deliverable landed on"
        );
        assert_ne!(filed, "t-gone");
        // …and it belongs to the same conversation, like the card the
        // no-card-in-scope path mints. Two minting paths must not disagree
        // about where their card posts back.
        assert_eq!(cards[0].origin_chat_id.as_deref(), Some("strategy"));
    }

    /// Each artifact records the agent that published **it** (#463 review).
    ///
    /// One drain can hold publishes from more than one agent — the desk lead's
    /// turn and the orchestrator's own turn both run with the full toolbelt
    /// under a single `Conversation` claim. Collapsing the batch to one author
    /// stamps the writer's name on the orchestrator's file and the reverse.
    #[tokio::test]
    async fn each_published_artifact_records_its_own_author() {
        use crate::harness::publish::PendingPublish;
        use crate::ports::artifacts::ArtifactStore;

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts(dir.path());
        let company = CompanyId::new("acme");
        let publish = |agent: &str, source: &str| PendingPublish {
            agent: agent.to_string(),
            source: source.to_string(),
            title: source.to_string(),
            kind: crate::ports::artifacts::ArtifactKind::Markdown,
            note: None,
            payload: crate::harness::publish::PublishPayload::Text(format!("# {source}")),
        };

        brain
            .record_published_artifacts(
                &card("t-1", "maya"),
                // The batch-level fallback, which must NOT win over the
                // per-item agents below.
                "maya",
                vec![publish("writer", "memo.md"), publish("ceo", "notes.md")],
                None,
            )
            .await
            .expect("records");

        let mut authors: Vec<(String, String)> = ArtifactStore::list(&*ops, &company, Some("t-1"))
            .await
            .expect("list")
            .into_iter()
            .map(|a| {
                (
                    a.source.clone().unwrap_or_default(),
                    a.versions[0].author_id.clone(),
                )
            })
            .collect();
        authors.sort();
        assert_eq!(
            authors,
            vec![
                ("memo.md".to_string(), "writer".to_string()),
                ("notes.md".to_string(), "ceo".to_string()),
            ]
        );
    }

    /// …and a `PendingPublish` built by hand — not by the tool, which always
    /// stamps its agent — still falls back to the caller's responder rather
    /// than recording a blank author.
    #[tokio::test]
    async fn a_publish_with_no_agent_falls_back_to_the_responder() {
        use crate::harness::publish::PendingPublish;
        use crate::ports::artifacts::ArtifactStore;

        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts(dir.path());

        brain
            .record_published_artifacts(
                &card("t-1", "maya"),
                "maya",
                vec![PendingPublish {
                    agent: String::new(),
                    source: "memo.md".to_string(),
                    title: "Memo".to_string(),
                    kind: crate::ports::artifacts::ArtifactKind::Markdown,
                    note: None,
                    payload: crate::harness::publish::PublishPayload::Text("# Memo".to_string()),
                }],
                None,
            )
            .await
            .expect("records");

        let listed = ArtifactStore::list(&*ops, &CompanyId::new("acme"), Some("t-1"))
            .await
            .expect("list");
        assert_eq!(listed[0].versions[0].author_id, "maya");
    }

    /// The other half: a run that did NOT succeed records nothing either, and
    /// its note still says what happened.
    #[tokio::test]
    async fn a_cancelled_delegated_card_records_no_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, ops, _provider) =
            brain_that_steers_itself(dir.path(), "t-cancel", vec![SteerAction::Cancel]);
        let mut c = card("t-cancel", "");
        c.origin_chat_id = Some("strategy".to_string());
        ops.upsert(&CompanyId::new("acme"), &c).await.expect("seed");

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t-cancel".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(only_card(&ops).await.column, COLUMN_TODO);
        let artifacts = crate::ports::artifacts::ArtifactStore::list(
            &*ops,
            &CompanyId::new("acme"),
            Some("t-cancel"),
        )
        .await
        .expect("list");
        assert!(
            artifacts.is_empty(),
            "a cancelled run has no deliverable to version"
        );
    }

    /// **Rewritten by #337.** The landing no longer depends on the card at
    /// all: a card's origin used to pick between two success terminals, and now
    /// there is one. The decision itself lives in
    /// [`crate::ports::tasks::column_for_settled_run`] (and is unit-tested
    /// there); this pins that `settle` — every run-ending path in this file —
    /// actually consults it, for both card shapes.
    #[test]
    fn settle_lands_every_finished_card_in_review_whatever_its_origin() {
        let mut board_card = card("t1", "maya");
        settle(&mut board_card, TaskRunEnd::Completed, "maya", "shipped");
        assert_eq!(board_card.column, COLUMN_IN_REVIEW);

        let mut delegated = card("t2", "maya");
        delegated.origin_chat_id = Some("strategy".to_string());
        settle(&mut delegated, TaskRunEnd::Completed, "maya", "shipped");
        assert_eq!(
            delegated.column, COLUMN_IN_REVIEW,
            "an origin no longer buys a card its own terminal"
        );
    }

    /// The redirect-cap finalize branch is the other success ending, so it has
    /// to make the same choice — otherwise a steered handoff diverges from an
    /// unsteered one.
    #[tokio::test]
    async fn redirect_cap_finalizes_a_card_with_an_origin_to_review() {
        let dir = tempfile::tempdir().unwrap();
        let redirect = || SteerAction::Redirect {
            instruction: "focus on the API".to_string(),
        };
        let (brain, tasks, _provider) = brain_that_steers_itself(
            dir.path(),
            "t1",
            vec![redirect(), redirect(), redirect(), redirect()],
        );
        let mut c = card("t1", "");
        c.origin_chat_id = Some("strategy".to_string());
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(only_card(&tasks).await.column, COLUMN_IN_REVIEW);
    }

    /// The relay has to have wording for the `done` landing column — without it
    /// the fallback arm renders the raw column id into the sentence.
    #[test]
    fn postback_reads_naturally_for_a_done_card() {
        let mut finished = card("t1", "maya");
        finished.column = "done".to_string();
        finished.note = None;
        assert_eq!(
            lifecycle::relay_text(&finished, "maya", "ceo"),
            "\"Ship the thing\" is done (maya ran it)."
        );
    }

    /// An `assignee` that names a roster member routes the turn to that member.
    #[tokio::test]
    async fn task_dispatch_routes_to_assignee() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "engineer"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let note = only_card(&tasks).await.note.expect("note");
        assert!(note.contains("[engineer]"), "{note:?}");
    }

    // ── Issue #242: the attempt row records what the dispatch actually did ──

    /// Wires a run store onto a task-capable brain, mints the `Pending` row the
    /// dispatch choke point would have minted, and returns both.
    async fn brain_with_a_pending_run(
        dir: &std::path::Path,
        assignee: &str,
    ) -> (HarnessBrain, Arc<FsOps>, Arc<dyn crate::ports::RunStore>) {
        use crate::ports::runs::NewRun;

        let (brain, tasks) = brain_with_tasks(dir);
        let runs: Arc<dyn crate::ports::RunStore> = Arc::new(FsOps::new(dir));
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-1", assignee))
            .await
            .expect("seed");
        runs.create_run(&company, NewRun::for_task("run-1", "t-1", assignee))
            .await
            .expect("mint");
        (brain.with_runs(Arc::clone(&runs)), tasks, runs)
    }

    /// The settle. This fixture's pool holds no roster, so the turn errors —
    /// which is exactly the `TaskRunEnd::Failed` path — and the row must end
    /// **terminal**, carrying the reason the card's note carries, rather than
    /// sitting `Pending` for the boot reaper to find.
    #[tokio::test]
    async fn a_dispatch_settles_its_attempt_row_from_how_the_run_ended() {
        use crate::ports::runs::RunStatus;

        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks, runs) = brain_with_a_pending_run(dir.path(), "engineer").await;
        let company = CompanyId::new("acme");

        brain.run_task("t-1", Some("run-1")).await.expect("run");

        let settled = runs
            .get_run(&company, "run-1")
            .await
            .expect("read")
            .expect("the row survives");
        assert_eq!(settled.status, RunStatus::Failed);
        assert!(
            settled.finished_at_millis.is_some(),
            "a terminal settle stamps when the attempt ended"
        );
        let reason = settled.error.expect("a failure carries its reason");
        assert!(reason.contains("dispatch failed"), "{reason}");
        // …and the row agrees with the card, rather than telling a second story.
        let note = only_card(&tasks).await.note.expect("note");
        assert!(note.contains("dispatch failed"), "{note}");
        // No turn ran on this offline fixture, so there is nothing to charge.
        assert_eq!(settled.step_count, 0);
        assert_eq!(settled.usage, TokenUsage::default());
    }

    /// Issue #1865 (CodeRabbit review, PR #1883 review comment 3892338104): an
    /// ordinary assigned board card — no `origin_chat_id`, so no relay target
    /// — whose turn genuinely fails (not a refusal) reaches this same
    /// rich-settle tail with a bounce chip but, before this fix, filed no
    /// `dispatch_failed` notification. `refuse_dispatch` files this
    /// notification for an off-roster assignee, and the cycle's terminality
    /// backstop files it for a crash-recovered dispatch — but the backstop
    /// explicitly skips any run no longer active, and `settle_run` just above
    /// this test's call site already terminalizes the attempt, so the
    /// backstop never sees it either. That left an ordinary failed dispatch
    /// with no origin chat completely silent: no chat reply, no badge,
    /// nothing but the board itself.
    #[tokio::test]
    async fn an_ordinary_failed_dispatch_with_no_origin_chat_files_a_dispatch_failed_notification()
    {
        use crate::ports::runs::NewRun;

        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks_notified(dir.path(), true);
        let runs: Arc<dyn crate::ports::RunStore> = Arc::new(FsOps::new(dir.path()));
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-1", "engineer"))
            .await
            .expect("seed");
        runs.create_run(&company, NewRun::for_task("run-1", "t-1", "engineer"))
            .await
            .expect("mint");
        let brain = brain.with_runs(Arc::clone(&runs));

        brain.run_task("t-1", Some("run-1")).await.expect("run");

        let settled = only_card(&tasks).await;
        assert_eq!(settled.column, COLUMN_TODO);
        assert!(
            settled.bounced.is_some(),
            "an ordinary turn failure must carry the bounce chip: {settled:?}"
        );
        assert!(
            settled.origin_chat_id.is_none(),
            "this is exactly the board-created shape with no relay target: {settled:?}"
        );

        let notes = crate::ports::notifications::NotificationStore::list(
            tasks.as_ref(),
            &company,
            "anyone",
        )
        .await
        .expect("list notifications");
        assert!(
            notes
                .iter()
                .any(|n| n.notification.kind == "dispatch_failed"
                    && n.notification.subject.id == "t-1"),
            "an ordinary failed dispatch with no origin chat must still file a \
             dispatch_failed notification, got {notes:?}"
        );
    }

    /// A refusal is an attempt too. It spends nothing and runs no turn, but it
    /// is a real, terminal outcome — the card's history must show "this was
    /// tried and refused, and why" rather than a gap where an attempt was.
    #[tokio::test]
    async fn a_refused_dispatch_settles_its_attempt_rather_than_leaving_a_gap() {
        use crate::ports::runs::RunStatus;

        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks, runs) = brain_with_a_pending_run(dir.path(), "Shane").await;
        let company = CompanyId::new("acme");

        brain.run_task("t-1", Some("run-1")).await.expect("run");

        let settled = runs
            .get_run(&company, "run-1")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(settled.status, RunStatus::Failed);
        let reason = settled.error.expect("reason");
        assert!(reason.contains("dispatch refused"), "{reason}");
        assert!(
            reason.contains("Shane"),
            "the row must name what was wrong, like the note does: {reason}"
        );
    }

    /// The degraded path stays degraded, not broken: a dispatch carrying no run
    /// id runs the card exactly as before and invents no row for it.
    #[tokio::test]
    async fn an_untracked_dispatch_runs_the_card_and_records_no_attempt() {
        use crate::ports::runs::RunFilter;

        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let runs: Arc<dyn crate::ports::RunStore> = Arc::new(FsOps::new(dir.path()));
        let brain = brain.with_runs(Arc::clone(&runs));
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-1", "engineer"))
            .await
            .expect("seed");

        brain.run_task("t-1", None).await.expect("run");

        assert!(
            only_card(&tasks).await.note.is_some(),
            "the card still ran and still recorded its outcome"
        );
        assert!(
            runs.list_runs(&company, &RunFilter::default())
                .await
                .expect("list")
                .is_empty(),
            "no row was minted for this dispatch, so none may be invented"
        );
    }

    /// A card that vanished between the dispatch write and the cycle still
    /// closes its attempt. Otherwise the row would sit `Pending` until a
    /// restart reaped it with a misleading "the host restarted" reason.
    #[tokio::test]
    async fn a_dispatch_whose_card_is_gone_still_closes_its_attempt() {
        use crate::ports::runs::{NewRun, RunStatus};

        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_tasks(dir.path());
        let runs: Arc<dyn crate::ports::RunStore> = Arc::new(FsOps::new(dir.path()));
        let brain = brain.with_runs(Arc::clone(&runs));
        let company = CompanyId::new("acme");
        runs.create_run(&company, NewRun::for_task("run-1", "t-gone", "engineer"))
            .await
            .expect("mint");

        assert!(
            brain
                .run_task("t-gone", Some("run-1"))
                .await
                .expect("run")
                .is_none(),
            "a missing card posts nothing back"
        );

        let settled = runs
            .get_run(&company, "run-1")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(settled.status, RunStatus::Failed);
        assert_eq!(settled.error.as_deref(), Some(CARD_VANISHED));
    }

    // ── Issue #205: the working agent is linked, and a bad assignee is refused ──

    /// The reported bug. A card assigned to "Shane" — nobody this company has —
    /// used to dispatch to the orchestrator anyway, keeping `assignee = "Shane"`
    /// while the timeline read "reply from ceo" and nothing said the name was
    /// invalid. It must now be **refused**: the card goes back to `todo`
    /// carrying the reason, and the orchestrator runs no turn on its behalf.
    #[tokio::test]
    async fn task_dispatch_off_roster_assignee_is_refused_not_silently_reassigned() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "Shane"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let refused = only_card(&tasks).await;
        assert_eq!(
            refused.column, COLUMN_TODO,
            "a card nobody can work must not sit in in_progress"
        );
        assert_eq!(
            refused.assignee, "Shane",
            "the invalid name is left as typed for the operator to correct"
        );
        // Issue #1865 (CodeRabbit review, PR #1883): a refusal is a failed
        // dispatch landing on `todo` exactly like any other, so it must carry
        // the same bounce chip `run_task`'s rich settle and the system mover
        // apply — the board must not read this card any differently just
        // because nobody ever ran.
        assert!(
            refused.bounced.is_some(),
            "an off-roster refusal must set the bounce chip like any other failed dispatch: {refused:?}"
        );
        let note = refused.note.expect("the refusal is written to the note");
        assert!(
            note.contains("Shane"),
            "the operator must be told which name is not a teammate: {note:?}"
        );
        assert!(
            note.contains("dispatch refused"),
            "the note must read as a refusal, not as work the CEO did: {note:?}"
        );
        assert!(
            !note.contains("mock: "),
            "no turn may run for an assignee nobody answers to: {note:?}"
        );
    }

    /// Issue #1865 (CodeRabbit review, PR #1883 review comment 3878668326): a
    /// board-created card (no `origin_chat_id`, exactly [`card`]'s shape) with
    /// an off-roster assignee bounces to `todo` and gets the bounce chip
    /// (c6c3a3083), but before this fix filed no `dispatch_failed`
    /// notification — the relay `refuse_dispatch` falls back to only fires
    /// when an `origin_chat_id` exists, and `settle_run_end` makes the
    /// attempt terminal before the cycle's own backstop notifier ever sees
    /// it. That left the refusal visible only to someone already looking at
    /// the board, unlike every other bounced-dispatch path
    /// (`CompanyRuntime::abandon_run`, the cycle's terminality backstop, the
    /// boot reaper's card sweep, and `workflow_build`'s `settle_to_todo`),
    /// which all raise this same notification.
    #[tokio::test]
    async fn a_refused_dispatch_with_no_origin_chat_files_a_dispatch_failed_notification() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks_notified(dir.path(), true);
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "Shane"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let refused = only_card(&tasks).await;
        assert_eq!(refused.column, COLUMN_TODO);
        assert!(
            refused.origin_chat_id.is_none(),
            "this is exactly the board-created shape with no relay target: {refused:?}"
        );

        let notes = crate::ports::notifications::NotificationStore::list(
            tasks.as_ref(),
            &CompanyId::new("acme"),
            "anyone",
        )
        .await
        .expect("list notifications");
        assert!(
            notes
                .iter()
                .any(|n| n.notification.kind == "dispatch_failed"
                    && n.notification.subject.id == "t1"),
            "a board card refused with no origin chat must still file a \
             dispatch_failed notification, got {notes:?}"
        );
    }

    /// The other half of #205: a card the orchestrator picks up because nobody
    /// was named gets that orchestrator written onto it, so the board names who
    /// is actually doing the work instead of showing a blank assignee.
    #[tokio::test]
    async fn task_dispatch_links_the_working_agent_to_an_unassigned_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(
            only_card(&tasks).await.assignee,
            "ceo",
            "the orchestrator that worked the card must be linked to it"
        );
    }

    /// A card assigned to a **desk** is worked by that desk's lead, but the card
    /// stays the desk's. `delegate_to_desk` writes a desk id into `assignee`, so
    /// this is the shape a hand-off actually produces. Dispatch picks who runs
    /// this turn, not who owns the card, so only the note names the member that
    /// ran it — relinking the lead onto `assignee` would erase the desk from the
    /// board the first time the card ran (#214 review).
    #[tokio::test]
    async fn task_dispatch_routes_a_desk_assignee_to_its_lead_but_keeps_the_desk() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_desk_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "eng"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let worked = only_card(&tasks).await;
        assert_eq!(
            worked.assignee, "eng",
            "a desk assignment is ownership: the card stays the desk's"
        );
        let note = worked.note.expect("note");
        assert!(
            note.contains("[engineer]"),
            "the desk's lead member still did the work, and the note names them: {note:?}"
        );
    }

    /// An **operator-overlay** teammate is a roster teammate. The narrow
    /// `manifest.agents`-only lookup used to drop these onto the orchestrator.
    #[tokio::test]
    async fn task_dispatch_routes_to_an_overlay_teammate() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        brain.mutate_record(|r| {
            r.overlay_agents.push(OverlayAgent {
                id: "nova".into(),
                name: "Nova".into(),
                role: "Growth".into(),
                description: None,
                tools: None,
                model: None,
                harness: None,
            })
        });
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "nova"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let note = only_card(&tasks).await.note.expect("note");
        assert!(
            note.contains("[nova]"),
            "an overlay teammate must work their own card: {note:?}"
        );
    }

    /// A refused dispatch still answers in the thread the card came from —
    /// otherwise a delegated hand-off to a bad assignee is silent twice over.
    #[tokio::test]
    async fn a_refused_dispatch_posts_the_reason_back_to_its_origin() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-origin", "Shane");
        c.origin_chat_id = Some("strategy".to_string());
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");

        let posted = brain
            .run_task("t-origin", None)
            .await
            .expect("run")
            .expect("a refused card with an origin must still post back");
        assert_eq!(
            posted.reply_to.as_ref().map(|r| r.chat_id.as_str()),
            Some("strategy")
        );
        assert_eq!(
            posted.channel,
            brain.orchestrator(),
            "the orchestrator answers for its own roster"
        );
        assert!(posted.text.contains("Shane"), "{}", posted.text);
        // Issue #1852: `refuse_dispatch` relays through the same
        // `relay_reply` as a settled run, so it carries the card id too — but
        // `CompanyRuntime::journal_dispatch_replies` strips it back to `None`
        // before journaling, since the settle already left a
        // `DeskTaskCompleted` link and this would only duplicate it.
        assert_eq!(posted.task_id.as_deref(), Some("t-origin"));
    }

    /// A dispatch for a card that no longer exists is a silent no-op, not an
    /// error.
    #[tokio::test]
    async fn task_dispatch_missing_card_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "nope".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs without a card");
        assert!(
            tasks
                .list(&CompanyId::new("acme"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    // --- Orchestrator routing + delegation ----------------------------------

    /// A roster with an `orchestrator`-tier agent (not first) and a desk.
    fn record_with_desk() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"
description = "Coordinates the company."

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds it."

[[group_chat]]
id = "eng_desk"
name = "Engineering"
members = ["engineer"]
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
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

    /// A brain over `record`, wired to a real task store.
    fn brain_over(dir: &std::path::Path, record: CompanyRecord) -> (HarnessBrain, Arc<FsOps>) {
        let tasks = Arc::new(FsOps::new(dir));
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks.clone()),
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record),
            tasks,
        )
    }

    /// A brain over the desk-bearing record, wired to a real task store.
    fn brain_with_desk(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        brain_over(dir, record_with_desk())
    }

    /// Issue #707, the retention half: a store with **no persisted record**
    /// leaves the brain's record exactly as it was.
    ///
    /// `Ok(None)` is not a failure and must not be treated as one — an absent
    /// record is what a company whose bundle has not been written yet looks
    /// like, and clearing on it would leave that company with no roster and no
    /// desks at all. Uses the real `FsCompanyStore` over a directory nothing was
    /// saved to, which is precisely the shape that returns `Ok(None)`.
    #[tokio::test]
    async fn a_refresh_with_no_persisted_record_keeps_the_one_it_has() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        let before = brain.record();

        brain
            .refresh_record()
            .await
            .expect("an absent record is not an error");

        let after = brain.record();
        assert_eq!(
            after.manifest.company.name, before.manifest.company.name,
            "the brain kept its record"
        );
        assert_eq!(
            after.manifest.agents.len(),
            before.manifest.agents.len(),
            "an absent record must not empty the roster"
        );
        assert_eq!(
            delegation::desk_lead(&brain.record(), "eng_desk"),
            Some("engineer".to_string()),
            "nor cost the company its desks"
        );
    }

    /// Issue #707, the loud half: a store that **cannot be read** fails the
    /// refresh rather than falling back to the record already in hand.
    ///
    /// Falling back is the defect this whole change removes, and it would come
    /// back invisibly — a turn that looked successful while routing on state the
    /// operator had already replaced. So the error propagates and the cycle
    /// fails. A corrupt `company.toml` is a real way to reach that arm, and
    /// reaching it through the real store rather than a double is what keeps
    /// this test honest about the failure it claims to cover.
    #[tokio::test]
    async fn a_refresh_that_cannot_read_the_store_fails_rather_than_going_stale() {
        use crate::ports::store::CompanyStore;

        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        let store = FsCompanyStore::new(dir.path());
        store.save(&brain.record()).await.expect("seed the record");

        // A bundle whose manifest no longer parses: the store reports a failure
        // rather than an absence, which is the arm under test.
        let toml_path =
            crate::store::Bundle::new(dir.path().to_path_buf(), &brain.record().id).company_toml();
        tokio::fs::write(&toml_path, b"this is not = valid toml [[[")
            .await
            .expect("corrupt the manifest");

        let err = brain
            .refresh_record()
            .await
            .expect_err("an unreadable record must fail the refresh, not be ignored");
        assert!(
            err.to_string().contains("company.toml"),
            "the failure names what could not be read: {err}"
        );

        // And through `run_cycle`, which is the level that actually protects
        // the promise. Asserting only on `refresh_record` would leave the call
        // site free to become `let _ = self.refresh_record().await;` — the
        // refresh would still run, the error would be dropped, the turn would
        // report success while routing on the record it already held, and every
        // other test here would stay green. That is issue #707 returning by a
        // different door, so the propagation is pinned where it is relied on.
        let cycle = brain.run_cycle(request(Vec::new()), &NoopHost).await;
        assert!(
            cycle.is_err(),
            "a cycle must fail when the record cannot be read, rather than \
             quietly routing on a stale one"
        );
    }

    // -----------------------------------------------------------------------
    // Mention routing: naming somebody outranks the desk lead
    // -----------------------------------------------------------------------

    fn mention_of(id: &str) -> crate::ports::types::Mention {
        crate::ports::types::Mention {
            target: crate::ports::types::MentionTarget::Agent { id: id.to_string() },
            text: format!("@{id}"),
            offset: 0,
            quiet: false,
        }
    }

    /// The whole point of the feature: `@engineer` in the main line is answered
    /// by the engineer, not by the orchestrator that would otherwise take it.
    #[test]
    fn a_mentioned_teammate_answers_instead_of_the_default_responder() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        // Without a mention, the orchestrator answers an unaddressed message.
        assert_eq!(brain.responder_for(None), "chief");
        // With one, the named teammate does.
        assert_eq!(
            crate::runtime::mentions::mention_responder(&brain.record(), &[mention_of("engineer")]),
            Some("engineer".to_string()),
        );
    }

    /// And it outranks the *desk lead*, which is the stronger claim: a message
    /// addressed to a desk still goes to the teammate it names.
    #[test]
    fn a_mention_outranks_the_addressed_desks_lead() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
        assert_eq!(
            crate::runtime::mentions::mention_responder(&brain.record(), &[mention_of("ceo")]),
            Some("ceo".to_string()),
            "the named teammate answers even on a desk with its own lead",
        );
    }

    /// A message that mentions nobody routes exactly as it did before mentions
    /// existed — which is every message already in every journal.
    #[test]
    fn a_message_with_no_mentions_routes_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(
            crate::runtime::mentions::mention_responder(&brain.record(), &[]),
            None,
            "so the caller falls through to responder_for",
        );
        assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
        assert_eq!(brain.responder_for(None), "chief");
    }

    /// `@everyone` names the addressed desk's teammates for the turn's context
    /// — and still leaves exactly one responder, because it is a list and not a
    /// fan-out.
    #[test]
    fn everyone_names_the_desk_without_choosing_a_responder() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        let mentions = [crate::ports::types::Mention {
            target: crate::ports::types::MentionTarget::Everyone,
            text: "@everyone".to_string(),
            offset: 0,
            quiet: false,
        }];
        assert_eq!(
            crate::runtime::mentions::mention_responder(&brain.record(), &mentions),
            None,
            "a broadcast names no single teammate, so the desk lead still answers",
        );
        assert_eq!(
            crate::runtime::mentions::mentioned_agents(
                &brain.record(),
                "eng_desk",
                &mentions,
                Some("engineer"),
            ),
            Vec::<String>::new(),
            "the only member is the responder, and it is not told it was mentioned",
        );
    }

    /// `@everyone` from the console's default thread (`chat: "main"`) expands
    /// against the General desk, not no desk at all. The console-only alias is
    /// not a desk key `resolve_desk_id` knows, so the brain folds it — and the
    /// other General-desk spellings — to the General desk id before expanding.
    #[test]
    fn everyone_desk_folds_the_main_thread_alias_to_general() {
        let record = record_with_desk();
        assert_eq!(HarnessBrain::everyone_desk(&record, None), "General");
        assert_eq!(HarnessBrain::everyone_desk(&record, Some("")), "General");
        assert_eq!(
            HarnessBrain::everyone_desk(&record, Some("main")),
            "General"
        );
        assert_eq!(
            HarnessBrain::everyone_desk(&record, Some("General")),
            "General"
        );
        assert_eq!(
            HarnessBrain::everyone_desk(&record, Some("eng_desk")),
            "eng_desk"
        );
    }

    /// A blueprint that declares a desk under one of the General spellings is
    /// grandfathered by this host — `is_general_channel` is guarded on
    /// `!desk_exists`, the desk keeps its members, and `responder_for` routes
    /// to its lead. The fold must not run over it: asking `resolve_desk_id`
    /// for the *name* `General` misses a desk called anything else, and
    /// `@everyone` would then expand to the whole roster instead of the two
    /// people actually on the line — a broadcast escaping the scope of the one
    /// case the fold exists to preserve.
    #[test]
    fn a_grandfathered_general_desk_keeps_its_own_membership() {
        let manifest: CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "main"
name = "Front office"
members = ["ceo", "engineer"]
"#,
        )
        .expect("valid manifest");
        let mut record = record_with_desk();
        record.manifest.group_chats = manifest.group_chats;

        // The raw key, not the General fold: this desk answers to it.
        assert_eq!(HarnessBrain::everyone_desk(&record, Some("main")), "main");

        // Every folded alias names the same membership as the raw key. A desk
        // that claims the line by *id* is missed by `resolve_desk_id("General")`,
        // so the alias used to fall through to a `General` desk that does not
        // exist — scoping `@everyone` to the whole roster in a channel whose
        // own lead answers (issue #1743).
        for alias in ["", "General", "general", "MAIN"] {
            assert_eq!(
                HarnessBrain::everyone_desk(&record, Some(alias)),
                "main",
                "the alias {alias:?} must scope @everyone to the claiming desk"
            );
        }
        // And with no such desk, the fold still applies as before.
        assert_eq!(
            HarnessBrain::everyone_desk(&record_with_desk(), Some("main")),
            "General"
        );

        let mentions = [crate::ports::types::Mention {
            target: crate::ports::types::MentionTarget::Everyone,
            text: "@everyone".to_string(),
            offset: 0,
            quiet: false,
        }];
        let expanded = crate::runtime::mentions::mentioned_agents(
            &record,
            &HarnessBrain::everyone_desk(&record, Some("main")),
            &mentions,
            None,
        );
        assert_eq!(
            expanded,
            vec!["ceo".to_string(), "engineer".to_string()],
            "a broadcast stays inside the desk that was addressed"
        );
        assert!(
            !expanded.contains(&"chief".to_string()),
            "and does not reach a teammate who is not on it: {expanded:?}"
        );
    }

    /// The default responder is the `orchestrator`-tier agent, even when it is
    /// not first on the roster.
    #[test]
    fn default_responder_is_the_orchestrator() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(brain.responder, "chief");
    }

    /// An addressed desk routes to its lead member (by id or name); anything else
    /// — the "General" desk, an unknown id, or no address — falls to the
    /// orchestrator.
    #[test]
    fn responder_for_routes_desk_to_lead_else_orchestrator() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
        assert_eq!(brain.responder_for(Some("Engineering")), "engineer");
        assert_eq!(brain.responder_for(Some("General")), "chief");
        assert_eq!(brain.responder_for(Some("nope")), "chief");
        assert_eq!(brain.responder_for(None), "chief");
    }

    // ── Issue #151 §3.3: a DM thread reaches the teammate it names ──

    /// A chat id naming a roster teammate answers as that teammate, which is
    /// what a per-agent DM thread is. Before this it fell through to the
    /// orchestrator, so the console would show an agent's thread while someone
    /// else answered in it.
    #[test]
    fn responder_for_routes_a_roster_agent_id_to_that_agent() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(brain.responder_for(Some("engineer")), "engineer");
        assert_eq!(brain.responder_for(Some("chief")), "chief");
    }

    // ── Issue #1743: who answers the built-in `#general` channel ──

    /// An **overlay** desk that took a General spelling before those were
    /// reserved must not answer the company-wide line.
    ///
    /// `create_desk` accepted `main` until issue #1743, so this is persisted
    /// state rather than a hypothesis. Such a desk is already hidden from
    /// `GET .../desks` and refused every mutation, but hiding a desk does not
    /// stop it routing: `desk_lead` resolves through
    /// `CompanyRecord::resolve_desk_id`, which used to match it, so the console
    /// rendered `#general` and named the orchestrator as who answers while this
    /// desk's lead answered instead. The resolver declines the key now, and the
    /// arm below it hands the line back to the orchestrator.
    #[test]
    fn responder_for_does_not_let_a_hidden_overlay_desk_answer_the_general_line() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        brain.mutate_record(|r| {
            r.overlay_desks.push(crate::ports::types::OverlayDesk {
                id: "main".into(),
                name: "Front office".into(),
                description: None,
                responder: Default::default(),
                members: vec!["engineer".into()],
            })
        });
        for spelling in ["", "main", "Main", "general", "General"] {
            assert_eq!(
                brain.responder_for(Some(spelling)),
                "chief",
                "the orchestrator answers the company-wide line as {spelling:?}"
            );
        }
        // The desk is not retired — it simply has no General key any more, and
        // the desk it is still routes to its own lead.
        assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
    }

    /// A **teammate** whose id is a General spelling keeps its DM, and does not
    /// take the company-wide line with it (issue #1743).
    ///
    /// `mint_agent_id` reserves `main` and `General`, but a manifest can still
    /// declare one, and a manifest is not something this host overrules. Before
    /// this, `resolve_roster_agent_id` matched the bare key and that teammate
    /// answered every unaddressed message — while `GET chat/history?desk=main`
    /// returned the *folded General conversation* (`is_general_chat` has folded
    /// `""`, `main`, `General` and `general` into one since issue #65). The
    /// responder and the transcript disagreed about whose conversation it was.
    /// The bare key is the line; `dm:<id>` is the teammate.
    #[test]
    fn responder_for_gives_the_general_line_to_the_orchestrator_not_a_teammate_called_main() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        brain.mutate_record(|r| {
            r.overlay_agents.push(OverlayAgent {
                id: "main".into(),
                name: "Mainard".into(),
                role: "Analyst".into(),
                description: None,
                tools: None,
                model: None,
                harness: None,
            })
        });
        assert!(
            brain.record().is_roster_agent("main"),
            "the teammate really is on the roster, so the old arm would have matched"
        );
        assert_eq!(
            brain.responder_for(Some("main")),
            "chief",
            "the bare key is the company's line, whatever a teammate is called"
        );
        assert_eq!(
            brain.responder_for(Some("dm:main")),
            "main",
            "and the teammate keeps its own DM, addressed the way the console addresses one"
        );
    }

    /// The grandfathered case the two tests above must not break: a
    /// `[[group_chat]]` the **blueprint** declares under a General spelling is
    /// the company's own General desk, and its lead still answers it.
    #[test]
    fn responder_for_still_routes_a_blueprint_general_desk_to_its_lead() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        let declared = toml::from_str::<CompanyManifest>(
            r#"
[company]
name = "Acme"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "main"
name = "Front office"
members = ["engineer"]
"#,
        )
        .expect("valid manifest")
        .group_chats;
        brain.mutate_record(|r| r.manifest.group_chats.extend(declared));
        assert_eq!(
            brain.responder_for(Some("main")),
            "engineer",
            "a blueprint desk keeps the line and its lead keeps answering it"
        );
    }

    // ── Issue #884 D2: an unresolvable chat key is no longer silent ──

    /// Captures everything logged on **this thread** while `body` runs.
    ///
    /// Thread-local (`with_default`) rather than a global default on purpose:
    /// `workflow_scheduler`'s capture already claims the process-wide slot in
    /// this same test binary and asserts it wins that race, so installing a
    /// second global here would turn its test red for an unrelated reason.
    fn logs_from(body: impl FnOnce()) -> String {
        use std::io::Write;
        use std::sync::Mutex;

        #[derive(Clone)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl Write for Writer {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("log sink").extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Writer;
            fn make_writer(&'a self) -> Self::Writer {
                Writer(self.0.clone())
            }
        }

        let sink = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Sink(sink.clone()))
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        let bytes = sink.lock().expect("log sink").clone();
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// A key that resolves to no desk and no teammate still answers as the
    /// orchestrator — the fallback is deliberate — but it now says so.
    ///
    /// This is the whole of D2: before it, "nobody addressed anybody" and
    /// "somebody addressed a teammate that does not exist" produced the same
    /// confident answer from an agent nobody asked, and the tenant log carried
    /// nothing to tell them apart.
    #[test]
    fn responder_for_warns_before_falling_back_to_the_orchestrator() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        // `dm:engineer` was this fixture until issue #982 made it resolve; the
        // key here has to be one that names nothing at all, or the test would
        // pass by asserting the wrong fact and #884's coverage would be gone.
        let logs = logs_from(|| {
            assert_eq!(
                brain.responder_for(Some("dm:nobody_by_that_name")),
                "chief",
                "the fallback itself is unchanged"
            );
        });
        assert!(
            logs.contains("dm:nobody_by_that_name"),
            "the unresolved key must be named so the fall-through is greppable: {logs}"
        );
        assert!(logs.contains("WARN"), "{logs}");

        // …and a key that DOES resolve stays silent, or the line is noise
        // rather than a signal.
        let quiet = logs_from(|| {
            assert_eq!(brain.responder_for(Some("engineer")), "engineer");
            assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
        });
        assert!(quiet.is_empty(), "a resolved key must not warn: {quiet}");
    }

    /// Issue #982: the console mints a DM channel id as `dm:<teammate-id>`, and
    /// a sibling route documents that form as a valid channel key — so a thread
    /// keyed on it has to be answered by the teammate it names, not by the
    /// orchestrator.
    ///
    /// The prefix is stripped **after** the desk and roster attempts, so this
    /// can only ever claim a key that resolved to nothing: `engineer` and
    /// `eng_desk` still route exactly as they did, which the test above pins.
    #[test]
    fn responder_for_answers_a_console_dm_channel_key_as_the_teammate() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(
            brain.responder_for(Some("dm:engineer")),
            "engineer",
            "a DM channel key addresses the teammate it names"
        );
        assert_eq!(
            brain.responder_for(Some("dm:")),
            "chief",
            "a prefix with nothing after it names nobody"
        );
    }

    /// A human- or console-typed teammate key resolves case-insensitively to the
    /// **canonical** roster id, so a capital letter no longer reads as "nobody"
    /// and hands the turn to the orchestrator.
    ///
    /// Returning the canonical id rather than the key as typed is the load-bearing
    /// half: the persona lookup downstream matches on the roster id, so echoing
    /// `"Engineer"` back would move the miss one layer along instead of fixing it.
    #[test]
    fn responder_for_resolves_a_teammate_key_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        for typed in ["Engineer", "ENGINEER", "engineer"] {
            assert_eq!(
                brain.responder_for(Some(typed)),
                "engineer",
                "`{typed}` must reach the engineer under its canonical id"
            );
        }
        // A key that resolves to nothing is still the orchestrator's — folding
        // the case may only claim keys that reached nobody before.
        assert_eq!(brain.responder_for(Some("engineeer")), "chief");
    }

    /// Desks still win. A desk id is resolved as a desk even if an agent shares
    /// the name, so no existing thread changes where it lands.
    #[test]
    fn a_desk_still_outranks_an_agent_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        // `eng_desk` is a desk led by `engineer`; it must resolve through the
        // desk path, not the DM path.
        assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
        // And an id that is neither still reaches the orchestrator.
        assert_eq!(brain.responder_for(Some("not-a-teammate")), "chief");
    }

    /// An operator-added overlay member is resolved as a desk's lead (issue #72):
    /// on a desk the manifest left empty, the overlay addition becomes the lead,
    /// and an addressed message routes to it. Proves `desk_lead`/`responder_for`
    /// read the effective (manifest ∪ overlay) membership.
    #[test]
    fn overlay_member_resolves_as_desk_lead() {
        let dir = tempfile::tempdir().unwrap();
        // `design` is a manifest desk with no declared members; the operator adds
        // `engineer` to it through the overlay.
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "design"
name = "Design"
"#,
        )
        .expect("valid manifest");
        let record = CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: vec![crate::ports::types::OverlayDeskMember {
                desk_id: "design".to_string(),
                agent_id: "engineer".to_string(),
            }],
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
        };
        let (brain, _tasks) = brain_over(dir.path(), record);
        assert_eq!(
            delegation::desk_lead(&brain.record(), "design"),
            Some("engineer".to_string())
        );
        assert_eq!(brain.responder_for(Some("design")), "engineer");
    }

    /// The operator's desk hierarchy drives the desk lead: a desk with manifest
    /// members `[eng1, eng2]` plus an overlay `cto`, ordered `[cto, eng1, eng2]`,
    /// resolves its lead to `cto` — `desk_lead` reads `effective_desk_members`,
    /// so the reorder flows through with no change to the resolver (issue #131).
    #[test]
    fn desk_order_drives_the_desk_lead() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "eng1"
role = "Engineer One"

[[agent]]
id = "eng2"
role = "Engineer Two"

[[group_chat]]
id = "eng"
name = "Engineering"
members = ["eng1", "eng2"]
"#,
        )
        .expect("valid manifest");
        let record = CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: vec![crate::ports::types::OverlayAgent {
                id: "cto".to_string(),
                name: "Cto".to_string(),
                role: "CTO".to_string(),
                description: None,
                tools: None,
                model: None,
                harness: None,
            }],
            overlay_desk_members: vec![crate::ports::types::OverlayDeskMember {
                desk_id: "eng".to_string(),
                agent_id: "cto".to_string(),
            }],
            overlay_desk_order: vec![crate::ports::types::OverlayDeskOrder {
                desk_id: "eng".to_string(),
                ordered: vec!["cto".to_string(), "eng1".to_string(), "eng2".to_string()],
            }],
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
        };
        let (brain, _tasks) = brain_over(dir.path(), record);
        assert_eq!(
            delegation::desk_lead(&brain.record(), "eng"),
            Some("cto".to_string())
        );
    }

    /// Regression for the builder seeding path (#133): a desk-order change written
    /// to the store must take effect on routing once the brain is rebuilt from the
    /// persisted record. The builder used to construct the brain with an empty
    /// `overlay_desk_order`, so desk chats kept routing to the pre-reorder lead.
    /// Here we persist a record, build a brain from the loaded record (blueprint
    /// lead), then write a new order and rebuild the brain from the reloaded record
    /// — the lead must update, not stay stale.
    #[tokio::test]
    async fn desk_order_change_updates_routing_after_rebuild() {
        use crate::ports::store::CompanyStore;

        let dir = tempfile::tempdir().unwrap();
        let store = FsCompanyStore::new(dir.path());
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "eng1"
role = "Engineer One"

[[agent]]
id = "eng2"
role = "Engineer Two"

[[group_chat]]
id = "eng"
name = "Engineering"
members = ["eng1", "eng2"]
"#,
        )
        .expect("valid manifest");
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
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
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .unwrap();

        // Brain built from the persisted record before any reorder: blueprint lead.
        let loaded = store.load(&id).await.unwrap().unwrap();
        let (brain, _tasks) = brain_over(dir.path(), loaded);
        assert_eq!(
            delegation::desk_lead(&brain.record(), "eng"),
            Some("eng1".to_string()),
            "blueprint lead before reorder"
        );

        // Operator reorders the desk (as `set_desk_order` does), promoting eng2.
        let mut record = store.load(&id).await.unwrap().unwrap();
        record
            .overlay_desk_order
            .push(crate::ports::types::OverlayDeskOrder {
                desk_id: "eng".to_string(),
                ordered: vec!["eng2".to_string(), "eng1".to_string()],
            });
        store.save(&record).await.unwrap();

        // Rebuild the brain from the reloaded record: routing follows the reorder,
        // no stale lead.
        let reloaded = store.load(&id).await.unwrap().unwrap();
        let (rebuilt, _tasks2) = brain_over(dir.path(), reloaded);
        assert_eq!(
            delegation::desk_lead(&rebuilt.record(), "eng"),
            Some("eng2".to_string()),
            "reorder did not take effect on routing after rebuild"
        );
    }

    /// A `spawn_task` delegation opens a To-do card and surfaces no bubble.
    #[tokio::test]
    async fn spawn_task_delegation_opens_a_todo_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_desk(dir.path());
        let out = brain
            .run_delegation(
                Delegation::SpawnTask {
                    title: "Draft the plan".to_string(),
                    note: Some("by friday".to_string()),
                    assignee: Some("engineer".to_string()),
                },
                None,
            )
            .await
            .expect("delegation runs");
        assert!(
            out.bubble.is_none() && out.desk_reply.is_none(),
            "spawn_task surfaces nothing to relay or bubble"
        );

        let cards = tasks.list(&CompanyId::new("acme")).await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].title, "Draft the plan");
        assert_eq!(cards[0].column, COLUMN_TODO);
        assert_eq!(cards[0].assignee, "engineer");
        // Issue #246: it surfaces no *bubble*, but it no longer surfaces
        // *nothing* — the card it opened is reported, which is what lets the
        // caller tell the operator a card exists instead of leaving them to
        // notice it on the board.
        assert_eq!(
            out.spawned_task.as_deref(),
            Some(cards[0].id.as_str()),
            "the opened card must be reported, and be the one actually written"
        );
    }

    /// Issue #246: a chat turn that opened a card says so on the bubble it
    /// answered from. Before this the card appeared on the board and the reply
    /// carried nothing tying the two together, so an operator had no way to
    /// tell a turn that opened work from one that only talked about it.
    #[tokio::test]
    async fn a_turn_that_opens_a_card_reports_it_on_the_operator_bubble() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::SpawnTask {
                title: "Draft the announcement".to_string(),
                note: None,
                assignee: None,
            })],
        );

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "we should announce this".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 1);
        let bubble = &result.channel_responses[0];
        let reported = bubble.task_id.as_deref().expect("the bubble names a card");
        let cards = brain
            .deps
            .tasks
            .as_ref()
            .unwrap()
            .list(&brain.record().id)
            .await
            .unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(
            reported, cards[0].id,
            "the reported card must be the one on the board"
        );
    }

    /// Issue #246, the documented limitation stated as a test rather than only
    /// as prose: a turn that opens several cards reports the **first**. The
    /// journal field this feeds is a single optional id, so widening it would
    /// break the byte-identical round-trip every already-stored reply relies
    /// on. Pinned to *first* — not "whichever won" — because a later spawn
    /// silently overwriting an earlier one would make the reported card depend
    /// on queue order, which is the model's choice, not a contract.
    #[tokio::test]
    async fn a_turn_that_opens_several_cards_reports_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _provider) = brain_that_delegates_with(
            dir.path(),
            vec![vec![
                Delegation::SpawnTask {
                    title: "First".to_string(),
                    note: None,
                    assignee: None,
                },
                Delegation::SpawnTask {
                    title: "Second".to_string(),
                    note: None,
                    assignee: None,
                },
            ]],
            TurnFaults::default(),
        );

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "two things".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let reported = result.channel_responses[0]
            .task_id
            .as_deref()
            .expect("the bubble names a card");
        let cards = brain
            .deps
            .tasks
            .as_ref()
            .unwrap()
            .list(&brain.record().id)
            .await
            .unwrap();
        assert_eq!(cards.len(), 2, "both cards are opened either way");
        let first = cards
            .iter()
            .find(|c| c.title == "First")
            .expect("the first card exists");
        assert_eq!(
            reported, first.id,
            "the bubble reports the first card opened, not the last"
        );
    }

    /// The other side of the same contract: a turn that opened no card must
    /// leave the field empty, so no bubble grows a "card opened" chip it has
    /// not earned.
    #[tokio::test]
    async fn a_turn_that_opens_no_card_reports_none() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _provider) = brain_that_delegates(dir.path(), Vec::new());

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "status?".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert!(
            result.channel_responses[0].task_id.is_none(),
            "an ordinary chat turn must not claim a card"
        );
    }

    // ── Issue #186 part b: orchestrator lifecycle authority ────────────────

    /// `assign_task` changes who owns an existing card, records the change in
    /// the orchestrator's voice, and — deliberately — does **not** touch the
    /// column: dispatch fires from `CompanyRuntime::upsert_task`, which the
    /// `TaskStore` port this drain writes through cannot reach.
    #[tokio::test]
    async fn assign_task_reassigns_the_card_without_dispatching_it() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-assign", "engineer");
        c.column = COLUMN_TODO.to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        let out = brain
            .run_delegation(
                Delegation::AssignTask {
                    task_id: "t-assign".to_string(),
                    assignee: "ceo".to_string(),
                    note: Some("closer to the customer".to_string()),
                },
                None,
            )
            .await
            .expect("delegation runs");
        assert!(
            out.bubble.is_none() && out.desk_reply.is_none(),
            "the orchestrator is mid-turn; a second voice here would be it talking to itself"
        );

        let after = only_card(&tasks).await;
        assert_eq!(after.assignee, "ceo");
        assert_eq!(
            after.column, COLUMN_TODO,
            "assignment records ownership; it must not start the work"
        );
        let note = after.note.expect("note");
        assert!(note.contains("assigned to ceo"), "{note}");
        assert!(note.contains("closer to the customer"), "{note}");
        assert!(
            note.contains(&format!("[{}]", brain.orchestrator())),
            "the assignment is recorded in the orchestrator's voice: {note}"
        );
    }

    /// #205: `assign_task` takes its `assignee` from an LLM tool call, so it can
    /// name somebody the company does not have just as easily as the operator's
    /// free-text field can. The bad name must not reach the card — the previous
    /// owner stays, and the refusal is recorded on the note.
    #[tokio::test]
    async fn assign_task_refuses_an_off_roster_assignee_and_keeps_the_current_owner() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-assign", "engineer");
        c.column = COLUMN_TODO.to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_delegation(
                Delegation::AssignTask {
                    task_id: "t-assign".to_string(),
                    assignee: "Shane".to_string(),
                    note: None,
                },
                None,
            )
            .await
            .expect("delegation runs");

        let after = only_card(&tasks).await;
        assert_eq!(
            after.assignee, "engineer",
            "a name nobody answers to must not displace the real owner"
        );
        let note = after.note.expect("note");
        assert!(note.contains("could not assign to Shane"), "{note}");
    }

    /// #214 review: a blank `assignee` resolves to `Unassigned`, whose canonical
    /// form is `""`. Clearing the owner is correct — unassigning is a real
    /// request — but the note must say so. It used to fall through the named
    /// arm and record `assigned to ` with nothing after it.
    #[tokio::test]
    async fn assign_task_with_a_blank_assignee_clears_the_owner_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-assign", "engineer");
        c.column = COLUMN_TODO.to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_delegation(
                Delegation::AssignTask {
                    task_id: "t-assign".to_string(),
                    assignee: "   ".to_string(),
                    note: None,
                },
                None,
            )
            .await
            .expect("delegation runs");

        let after = only_card(&tasks).await;
        assert_eq!(
            after.assignee, "",
            "a blank assignee unassigns the card, which is a legitimate write"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("cleared the assignee"),
            "the note names the effect rather than trailing off: {note}"
        );
        assert!(
            !note.contains("assigned to "),
            "the truncated 'assigned to <nothing>' note must not come back: {note}"
        );
    }

    /// Approving finishes a board-created card: this is #171's `in_review →
    /// done` write (PR #179) for the card shape #179's own origin rule cannot
    /// reach, with the verdict recorded on the note.
    #[tokio::test]
    async fn review_approve_records_the_verdict_and_completes_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-review", "engineer");
        c.column = "in_review".to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_delegation(
                Delegation::ReviewTask {
                    task_id: "t-review".to_string(),
                    decision: lifecycle::ReviewDecision::Approve,
                    note: Some("ships as-is".to_string()),
                },
                None,
            )
            .await
            .expect("delegation runs");

        let after = only_card(&tasks).await;
        assert_eq!(
            after.column, "done",
            "an approving verdict is the in_review -> done transition (#171)"
        );
        let note = after.note.expect("note");
        assert!(note.contains("reviewed: approved"), "{note}");
        assert!(note.contains("ships as-is"), "{note}");
    }

    /// `revise` is a transition #186 does own: the card goes back to the
    /// To-do so it can be picked up and re-dispatched.
    #[tokio::test]
    async fn review_revise_sends_the_card_back_to_todo() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-revise", "engineer");
        c.column = "in_review".to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_delegation(
                Delegation::ReviewTask {
                    task_id: "t-revise".to_string(),
                    decision: lifecycle::ReviewDecision::Revise,
                    note: None,
                },
                None,
            )
            .await
            .expect("delegation runs");

        let after = only_card(&tasks).await;
        assert_eq!(after.column, COLUMN_TODO);
        assert!(
            after.note.expect("note").contains("needs another pass"),
            "the verdict must be recorded even without a reviewer comment"
        );
    }

    /// A card that has since been deleted is a silent no-op, matching every
    /// other task path in this file — never an error that kills the turn.
    #[tokio::test]
    async fn a_lifecycle_delegation_for_a_missing_card_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());

        for delegation in [
            Delegation::AssignTask {
                task_id: "ghost".to_string(),
                assignee: "ceo".to_string(),
                note: None,
            },
            Delegation::ReviewTask {
                task_id: "ghost".to_string(),
                decision: lifecycle::ReviewDecision::Approve,
                note: None,
            },
        ] {
            let out = brain
                .run_delegation(delegation, None)
                .await
                .expect("a missing card must not error");
            assert!(out.bubble.is_none() && out.desk_reply.is_none());
        }
        assert!(
            tasks
                .list(&CompanyId::new("acme"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A `delegate_to_desk` delegation runs the desk lead and hands its reply
    /// back to relay (a `DeskReply` attributed to the lead, no standalone
    /// bubble); an unknown desk yields nothing.
    #[tokio::test]
    async fn delegate_to_desk_delegation_answers_as_the_desk_lead() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        // The pool must have the roster before a member turn can run.
        brain
            .pool
            .ensure(&brain.record(), &brain.deps)
            .await
            .expect("roster");

        let out = brain
            .run_delegation(
                Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "ship-marker".to_string(),
                },
                None,
            )
            .await
            .expect("delegation runs");
        // The answer comes back as a DeskReply to relay — not a standalone
        // bubble — attributed to the desk lead, and the mock provider echoes the
        // instruction, proving the member's turn ran.
        assert!(
            out.bubble.is_none(),
            "the desk reply is relayed, not bubbled"
        );
        let desk = out.desk_reply.expect("desk lead replies");
        assert_eq!(desk.member, "engineer");
        assert!(desk.reply.contains("ship-marker"), "{:?}", desk.reply);

        // An unknown desk delegates to nobody.
        let none = brain
            .run_delegation(
                Delegation::DelegateToDesk {
                    desk: "ghost".to_string(),
                    instruction: "hello".to_string(),
                },
                None,
            )
            .await
            .expect("delegation runs");
        assert!(
            none.bubble.is_none() && none.desk_reply.is_none(),
            "an unknown desk yields nothing"
        );
    }

    // --- MCP failure drain --------------------------------------------------

    /// A recorded MCP failure re-skins into an **error step** on the operator
    /// bubble's timeline AND a scrubbed `McpCallFailed` audit event when the
    /// event log is wired (the Activity-trace re-skin of the old warning bubble).
    #[tokio::test]
    async fn mcp_failures_surface_as_error_steps_and_event() {
        use crate::harness::mcp_probe::McpFailure;
        use crate::ports::EventLog;
        use crate::ports::types::EventSeq;
        use crate::store::FsEventLog;

        let dir = tempfile::tempdir().unwrap();
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let failures = crate::harness::mcp_probe::McpFailureQueue::default();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir.path())),
            store: Arc::new(FsCompanyStore::new(dir.path())),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: Some(events.clone()),
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: failures.clone(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record());

        // A failure recorded during the turn (its message already scrubbed).
        failures.push(McpFailure {
            server: "browserbase".into(),
            tool: "browse".into(),
            status: "tool_call_rejected".into(),
            hint: None,
            scrubbed_message: "server rejected the call".into(),
        });

        let mut steps: Vec<TurnStep> = Vec::new();
        // `None` — this is the chat-turn drain, which journals no `task_id`
        // (#185). The dispatch drain passes the card id; see `run_task`.
        brain
            .surface_mcp_failures(&mut steps, None)
            .await
            .expect("drain surfaces failures");

        assert_eq!(steps.len(), 1, "one error step");
        assert_eq!(steps[0].kind, TurnStepKind::Note);
        assert_eq!(steps[0].status, TurnStepStatus::Error);
        assert!(
            steps[0].label.contains("browserbase"),
            "{:?}",
            steps[0].label
        );
        assert_eq!(steps[0].detail.as_deref(), Some("server rejected the call"));

        let logged = events
            .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
            .await
            .expect("read events");
        assert!(
            logged.iter().any(|e| matches!(
                &e.event,
                CompanyEvent::McpCallFailed { server, status, .. }
                    if server == "browserbase" && status == "tool_call_rejected"
            )),
            "an McpCallFailed audit event was journaled"
        );
    }

    /// #185 review follow-up: one bad journal write must not swallow the rest of
    /// the batch.
    ///
    /// `McpFailureQueue::drain` is a `mem::take` — by the time the loop runs the
    /// queue is empty and the batch exists only in that iterator. Propagating
    /// the first append error with `?` therefore did not merely skip one audit
    /// event, it discarded every failure behind it with nothing left to retry
    /// from. Journaling is per-item best-effort so the drain always completes.
    #[tokio::test]
    async fn a_failed_journal_write_does_not_swallow_the_rest_of_the_drain() {
        use crate::harness::mcp_probe::McpFailure;
        use crate::ports::EventLog;
        use crate::ports::types::{EventSeq, StoredEvent};
        use futures::stream::{self, BoxStream};

        /// An event log whose FIRST append fails and whose later appends
        /// succeed, recording what got through.
        #[derive(Default)]
        struct FailFirstLog {
            seen: StdMutex<Vec<CompanyEvent>>,
            appends: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl EventLog for FailFirstLog {
            async fn append(&self, _id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
                let nth = self
                    .appends
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if nth == 0 {
                    return Err(crate::error::OpenCompanyError::Store(
                        "journal unavailable".to_string(),
                    ));
                }
                let mut guard = self.seen.lock().unwrap();
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

        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(FailFirstLog::default());
        let failures = crate::harness::mcp_probe::McpFailureQueue::default();
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir.path())),
            store: Arc::new(FsCompanyStore::new(dir.path())),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: Some(log.clone()),
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: failures.clone(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record());

        for server in ["first", "second", "third"] {
            failures.push(McpFailure {
                server: server.into(),
                tool: "browse".into(),
                status: "tool_call_rejected".into(),
                hint: None,
                scrubbed_message: "server rejected the call".into(),
            });
        }

        let mut steps: Vec<TurnStep> = Vec::new();
        brain
            .surface_mcp_failures(&mut steps, Some("t1"))
            .await
            .expect("a journal error is best-effort, not fatal");

        // Every failure is re-skinned onto the timeline regardless…
        assert_eq!(steps.len(), 3, "all three failures surfaced as steps");
        // …and the two after the failed write still reached the journal. Before
        // this fix `seen` was empty: the `?` returned on `first` and `second` /
        // `third` were dropped with the drained batch.
        let seen = log.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "the drain continued past the failed append");
        assert!(
            seen.iter().any(|e| matches!(
                e,
                CompanyEvent::McpCallFailed { server, .. } if server == "third"
            )),
            "the last failure in the batch was still journaled"
        );
    }

    // --- Approval parking (issue #172) --------------------------------------

    /// A brain over `dir` whose deps carry `requests` as the shared
    /// approval-request queue — the same handle every roster agent's
    /// `ApprovalPolicy` pushes onto.
    fn brain_with_approval_queue(
        dir: &std::path::Path,
        requests: crate::harness::policy::ApprovalRequestQueue,
    ) -> HarnessBrain {
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: requests,
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record())
    }

    /// The regression for #172: a `RequireApproval` recorded during a turn is
    /// **parked** on the host, so it lands in the journal the Approvals page
    /// reads instead of being narrated away in chat and lost.
    ///
    /// `ParkingHost` panics on `emit_effect`, which pins the other half of the
    /// fix: the request must NOT be re-evaluated by the runtime gate (which
    /// allows — and so silently "executes" — the `Other` group most gated tool
    /// calls classify into).
    #[tokio::test]
    async fn approval_requests_are_parked_for_the_operator() {
        use crate::harness::policy::{ApprovalPolicy, ApprovalRequestQueue};
        use openhuman_core::openhuman::agent::tool_policy::{
            ToolCallContext, ToolPolicy, ToolPolicyDecision, ToolPolicyRequest,
        };

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());

        // Exactly what a supervised policy records when the agent reaches for a
        // gated tool mid-turn.
        let policy = ApprovalPolicy::new(
            &crate::company::Policy {
                mode: "supervised".to_string(),
                always_approve: Vec::new(),
                auto_approve_under_usd: None,
                approval_ttl_hours: None,
            },
            None,
        )
        .with_requests(requests.clone());
        let args = crate::policy::test_support::composio_send_args();
        let request = ToolPolicyRequest::new(
            "composio_execute",
            args.clone(),
            ToolCallContext::session("s", "chat", "ceo", "call-1", 0),
        );
        assert!(
            matches!(
                policy.check(&request).await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "the fixture must reproduce a gated call"
        );
        assert_eq!(requests.queued(), 1, "the decision was recorded to park");

        let host = ParkingHost::default();
        brain
            .park_approval_requests(&host)
            .await
            .expect("the drain parks");

        let parked = host.parked();
        assert_eq!(parked.len(), 1, "one approval reached the operator");
        assert_eq!(parked[0].kind, "composio_execute");
        assert_eq!(
            parked[0].payload, args,
            "the call's arguments are preserved"
        );
        assert_eq!(requests.queued(), 0, "the queue is drained");
    }

    /// A second drain parks nothing: the queue is emptied, so a later cycle
    /// can't re-park a request the operator has already been shown.
    #[tokio::test]
    async fn draining_twice_parks_nothing_the_second_time() {
        use crate::harness::policy::{ApprovalRequest, ApprovalRequestQueue};
        use crate::ports::types::EffectGroup;

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());
        requests.push(ApprovalRequest {
            tool: "media_generate_image".to_string(),
            reason: "supervised".to_string(),
            effect: Effect {
                kind: "media_generate_image".to_string(),
                group: EffectGroup::Spend,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::json!({ "prompt": "a logo" }),
                agent: None,
                run_id: None,
            },
        });

        let host = ParkingHost::default();
        brain.park_approval_requests(&host).await.expect("drain");
        brain
            .park_approval_requests(&host)
            .await
            .expect("second drain");
        assert_eq!(host.parked().len(), 1, "parked once, not twice");
    }

    /// Issue #561: a turn that gates more calls than one turn may raise tells
    /// the operator so, with the count.
    ///
    /// The cap itself is not the bug and is not touched here. The bug is that
    /// exceeding it was **silent**: the operator saw eight cards and had no way
    /// to learn that five more gated calls had happened, been refused, and been
    /// dropped. Eight cards and no notice is indistinguishable from "eight is
    /// all there was".
    #[tokio::test]
    async fn a_turn_that_overflows_the_cap_tells_the_operator_how_many_were_dropped() {
        use crate::harness::policy::{ApprovalRequest, ApprovalRequestQueue};
        use crate::ports::types::EffectGroup;

        let cap = crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN;
        let over = 5;

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());
        for i in 0..(cap + over) {
            requests.push(ApprovalRequest {
                tool: "composio_execute".to_string(),
                reason: "supervised".to_string(),
                effect: Effect {
                    kind: "composio_execute".to_string(),
                    group: EffectGroup::Send,
                    amount_usd: None,
                    established_thread: false,
                    first_time_counterparty: false,
                    // Distinct payloads, or `push` would dedupe them and the
                    // queue would never reach the cap in the first place.
                    payload: crate::policy::test_support::composio_unclassified_args_numbered(i),
                    agent: None,
                    run_id: None,
                },
            });
        }

        let host = ParkingHost::default();
        let notice = brain
            .park_approval_requests(&host)
            .await
            .expect("drain")
            .expect("an overflowing turn has something to tell the operator");

        assert_eq!(host.parked().len(), cap, "the cap still holds");
        assert!(
            notice.contains(&over.to_string()),
            "the operator is told HOW MANY were dropped, not just that some were: {notice}"
        );
        assert!(
            notice.contains(&cap.to_string()),
            "…and what the limit was, so the number means something: {notice}"
        );
    }

    /// The ordinary turn stays quiet. A notice on every cycle would train the
    /// operator to scroll past the one that matters.
    #[tokio::test]
    async fn a_turn_within_the_cap_raises_no_notice() {
        use crate::harness::policy::{ApprovalRequest, ApprovalRequestQueue};
        use crate::ports::types::EffectGroup;

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());
        requests.push(ApprovalRequest {
            tool: "composio_execute".to_string(),
            reason: "supervised".to_string(),
            effect: Effect {
                kind: "composio_execute".to_string(),
                group: EffectGroup::Send,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: crate::policy::test_support::composio_send_args(),
                agent: None,
                run_id: None,
            },
        });

        let host = ParkingHost::default();
        assert!(
            brain
                .park_approval_requests(&host)
                .await
                .expect("drain")
                .is_none(),
            "one request, a cap of 8: nothing was dropped and nothing is said"
        );
        assert_eq!(host.parked().len(), 1, "and the request itself still parks");
    }

    /// A host that fails to park the *first* effect it is handed, then behaves.
    /// Models a transient journal/IO fault mid-batch.
    #[derive(Default)]
    struct FlakyParkingHost {
        parked: std::sync::Mutex<Vec<Effect>>,
        seen: std::sync::atomic::AtomicUsize,
    }

    impl FlakyParkingHost {
        fn parked(&self) -> Vec<Effect> {
            self.parked.lock().expect("parked").clone()
        }
    }

    #[async_trait]
    impl CycleHost for FlakyParkingHost {
        async fn call_tool(&self, _call: ToolCall) -> Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                output: serde_json::Value::Null,
            })
        }
        async fn context_op(&self, _op: ContextOp) -> Result<ContextOpResult> {
            Ok(ContextOpResult::Text(String::new()))
        }
        async fn emit_effect(&self, _effect: Effect) -> Result<EffectDisposition> {
            panic!("an approval request must be parked, never re-evaluated as an effect");
        }
        async fn park_effect(&self, effect: Effect) -> Result<ApprovalId> {
            if self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Err(crate::OpenCompanyError::Store(
                    "journal on fire".to_string(),
                ));
            }
            let mut parked = self.parked.lock().expect("parked");
            parked.push(effect);
            Ok(ApprovalId::new(format!("appr-{}", parked.len())))
        }
    }

    /// One failed park must not take the rest of the batch — or the turn's reply
    /// — down with it. `drain` has already emptied the shared queue, so a `?`
    /// here would lose every later request forever and abort `run_cycle`,
    /// reproducing for the remainder of the batch exactly the silent
    /// disappearance this issue fixes.
    #[tokio::test]
    async fn a_failed_park_does_not_drop_the_rest_of_the_batch() {
        use crate::harness::policy::{ApprovalRequest, ApprovalRequestQueue};
        use crate::ports::types::EffectGroup;

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());
        for tool in ["first_tool", "second_tool", "third_tool"] {
            requests.push(ApprovalRequest {
                tool: tool.to_string(),
                reason: "supervised".to_string(),
                effect: Effect {
                    kind: tool.to_string(),
                    group: EffectGroup::Other,
                    amount_usd: None,
                    established_thread: false,
                    first_time_counterparty: false,
                    payload: serde_json::json!({ "tool": tool }),
                    agent: None,
                    run_id: None,
                },
            });
        }

        let host = FlakyParkingHost::default();
        let notice = brain
            .park_approval_requests(&host)
            .await
            .expect("a park failure is surfaced without aborting the batch")
            .expect("the operator is told a request was not saved");

        // The first park failed; the two after it still reached the operator.
        let parked = host.parked();
        assert_eq!(parked.len(), 2, "the batch continued past the failure");
        assert_eq!(parked[0].kind, "second_tool");
        assert_eq!(parked[1].kind, "third_tool");
        assert!(notice.contains("1 approval request could not be saved"));
        assert!(notice.contains("Ask the agent to request approval again"));
    }

    // --- Re-dispatching a granted call (issue #243) --------------------------

    /// A brain over the offline mock provider, wired to a real event log and a
    /// shared approval queue (whose grant set the runtime would mint into).
    fn brain_with_queue_and_events(
        dir: &std::path::Path,
        requests: crate::harness::policy::ApprovalRequestQueue,
        events: Arc<dyn crate::ports::EventLog>,
    ) -> HarnessBrain {
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: Some(events),
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: requests,
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record())
    }

    /// As [`brain_with_queue_and_events`], but every model call fails with a
    /// budget-exhausted body via [`BudgetExhaustedProvider`] (issue #1846
    /// review, Codex #3869725683) — otherwise byte-identical, so the only
    /// variable a test built on this exercises is how the approval-
    /// continuation redispatch path reacts to that one failure shape.
    fn brain_with_queue_and_events_and_budget_exhausted_provider(
        dir: &std::path::Path,
        requests: crate::harness::policy::ApprovalRequestQueue,
        events: Arc<dyn crate::ports::EventLog>,
    ) -> HarnessBrain {
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(BudgetExhaustedProvider),
            provider_slug: "scripted".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: Some(events),
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: requests,
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record())
    }

    fn approval_resolved(id: &str, verdict: Verdict) -> CompanyEvent {
        CompanyEvent::ApprovalResolved {
            approval_id: ApprovalId::new(id),
            verdict,
            by: crate::ports::types::Actor {
                kind: crate::ports::types::ActorKind::Operator,
                id: "owner".into(),
            },
        }
    }

    fn cycle_over(events: Vec<CompanyEvent>) -> CycleRequest {
        CycleRequest {
            cycle_id: "cyc-1".to_string(),
            company_id: CompanyId::new("acme"),
            events,
            event_seqs: Vec::new(),
            policy: None,
        }
    }

    /// The arm that made #243 visible: an approved grant re-dispatches its agent
    /// with the exact arguments, answers on that agent's channel, and journals
    /// the reply.
    ///
    /// Before this arm existed, `ApprovalResolved` fell into `_ => {}`: no turn,
    /// no response, and the cycle ended on the "Acknowledged." fallback. The
    /// operator approved, read "Acknowledged.", and nothing ran — which looks
    /// exactly like success.
    ///
    /// `MockProvider` echoes the user message back, so the reply text IS the
    /// instruction the agent received — which is what makes argument fidelity
    /// assertable offline.
    #[tokio::test]
    async fn an_approved_grant_redispatches_its_agent_with_the_exact_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path().to_path_buf()));
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        // Issue #470: a real catalogued send, with the action's own parameters
        // under `arguments` where the tool's schema puts them — so the
        // re-dispatch path this test covers carries a call the classifier can
        // actually read.
        let args = crate::policy::test_support::composio_args_with(
            crate::policy::test_support::COMPOSIO_SEND_SLUG,
            serde_json::json!({ "to": "a@b.test" }),
        );
        requests
            .grants()
            .grant(crate::runtime::grants::GrantedCall {
                approval_id: ApprovalId::new("appr-1"),
                agent: "ceo".into(),
                tool: "composio_execute".into(),
                args: args.clone(),
                at_millis: now_millis(),
                origin_thread: None,
                origin_parent: None,
                origin_task: None,
            });
        let brain = brain_with_queue_and_events(dir.path(), requests, log.clone());

        let result = brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-1", Verdict::Approve)]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        // A real bubble, on the GRANTING agent's channel — not the generic
        // "Acknowledged." fallback and not the operator channel.
        assert_eq!(result.channel_responses.len(), 1);
        let bubble = &result.channel_responses[0];
        assert_eq!(bubble.channel, "ceo");
        assert_ne!(bubble.text, "Acknowledged.");

        // The instruction carried the tool and the arguments VERBATIM. A model
        // that re-issues with drifted arguments re-parks (see the policy tests),
        // so the fidelity of this string is what makes the round-trip land.
        assert!(bubble.text.contains("composio_execute"), "{}", bubble.text);
        assert!(
            bubble.text.contains(&serde_json::to_string(&args).unwrap()),
            "the exact approved arguments must reach the agent: {}",
            bubble.text
        );
        assert!(
            bubble.text.contains("Do not modify them"),
            "{}",
            bubble.text
        );

        // Journaling the reply is no longer this function's job (issue #469):
        // the runtime journals every continuation reply once, in
        // `CompanyRuntime::publish_continuation`, so that the answers of
        // continuations this arm produces nothing for are not lost either. The
        // round trip — reply journaled into the thread the sign-off was raised
        // in, reaching the console's event stream — is covered end to end over
        // the real router by
        // `server::operator::test::a_continuation_answers_in_the_thread_the_sign_off_was_raised_in`.
        assert!(
            no_replies_journaled(&log).await,
            "the brain must not journal the reply a second time; the runtime owns it"
        );
    }

    async fn assert_explicit_decision_continues(verdict: Verdict, expected: &str) {
        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path().to_path_buf()));
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        requests
            .grants()
            .continue_approval(crate::runtime::grants::ApprovalContinuation {
                call: crate::runtime::grants::GrantedCall {
                    approval_id: ApprovalId::new("appr-explicit"),
                    agent: "ceo".into(),
                    tool: crate::harness::approval_tool::REQUEST_APPROVAL_TOOL.into(),
                    args: serde_json::json!({
                        "title": "Publish the announcement",
                        "question": "May I publish it?"
                    }),
                    at_millis: now_millis(),
                    origin_thread: None,
                    origin_parent: None,
                    origin_task: None,
                },
                verdict,
                by: crate::ports::types::Actor {
                    kind: crate::ports::types::ActorKind::User,
                    id: "operator".into(),
                },
            });
        let grants = requests.grants();
        let brain = brain_with_queue_and_events(dir.path(), requests, log);

        let result = brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-explicit", verdict)]),
                &NoopHost,
            )
            .await
            .unwrap();

        assert_eq!(result.channel_responses.len(), 1);
        let text = &result.channel_responses[0].text;
        assert!(text.contains(expected), "{text}");
        assert!(text.contains("Publish the announcement"), "{text}");
        assert!(!text.contains("Re-issue it"), "{text}");
        assert!(
            grants
                .peek_continuation(&ApprovalId::new("appr-explicit"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_explicit_approval_continues_without_reissuing_the_request_tool() {
        assert_explicit_decision_continues(Verdict::Approve, "APPROVED").await;
    }

    #[tokio::test]
    async fn an_explicit_denial_also_returns_to_the_requesting_agent() {
        assert_explicit_decision_continues(Verdict::Deny, "DENIED").await;
    }

    /// A threaded approval continuation must preserve the approval's thread root
    /// when it re-dispatches the granted call. The bound agent's observable
    /// context proves the target is threaded rather than channel-only.
    #[tokio::test]
    async fn an_approved_threaded_grant_redispatches_in_its_origin_thread() {
        let dir = tempfile::tempdir().unwrap();
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        let root = crate::ports::types::EventSeq::new(7);
        requests
            .grants()
            .grant(crate::runtime::grants::GrantedCall {
                approval_id: ApprovalId::new("appr-threaded"),
                agent: "ceo".into(),
                tool: "workspace_write".into(),
                args: serde_json::json!({}),
                at_millis: now_millis(),
                origin_thread: Some("general".into()),
                origin_parent: Some(root),
                origin_task: None,
            });
        let base = brain_with_queue_and_events(
            dir.path(),
            requests,
            Arc::new(crate::store::FsEventLog::new(dir.path().to_path_buf())),
        );
        let pool = Arc::new(HarnessPool::new());
        let brain = HarnessBrain::new(pool.clone(), (*base.deps).clone(), record());
        pool.ensure(&record(), &brain.deps).await.expect("ensure");

        brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-threaded", Verdict::Approve)]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let agent = pool
            .agents
            .read()
            .await
            .get(&CompanyId::new("acme"))
            .and_then(|roster| roster.iter().find(|agent| agent.agent_id == "ceo"))
            .cloned()
            .expect("the approved turn keeps the agent resident");
        assert_eq!(
            *agent.bound_chat.lock().await,
            None,
            "approval continuation runs unstreamed; binding is covered by the delegated target"
        );
    }

    /// No continuation reply was journaled by the brain itself (issue #469).
    async fn no_replies_journaled(log: &Arc<dyn crate::ports::EventLog>) -> bool {
        log.read_from(&CompanyId::new("acme"), crate::ports::EventSeq::new(0), 100)
            .await
            .unwrap()
            .iter()
            .all(|e| !matches!(e.event, CompanyEvent::AgentReply { .. }))
    }

    /// Issue #1846 review (Codex #3869725683) — **the regression.** Same
    /// fixture as `an_approved_grant_redispatches_its_agent_with_the_exact_arguments`
    /// above, but the re-issued call's provider is now out of credits.
    ///
    /// `run_steered_background` runs through the SAME `run_inner` the
    /// interactive chat path does, so it parks a re-issue marker for the
    /// granting agent exactly as an ordinary paused message would — proven
    /// below by reading it straight off `BudgetPauseSet`. Before this fix, the
    /// bubble `redispatch_granted_call` built from that outcome carried
    /// `outcome.reply` (the budget-paused placeholder text) verbatim, so the
    /// operator saw an ordinary-looking reply rather than the runtime's own
    /// pause notice.
    ///
    /// Issue #1846 review (Codex #3870562590): the notice it now carries is the
    /// NO-RESEND one. The marker asserted below is real but not redeemable —
    /// `run_steered_background` parks it with `background: true`, the one shape
    /// `redeem_budget_pause` refuses (`src/server/ops/budget_pause.rs`) — so
    /// the redeemable prefix would have drawn a CTA that returned 400 on every
    /// click. Both prefixes are asserted: matching the new one is only half the
    /// contract, since the console branches on the old one.
    #[tokio::test]
    async fn a_budget_paused_approval_continuation_surfaces_the_notice_and_parks_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path().to_path_buf()));
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        let args = crate::policy::test_support::composio_args_with(
            crate::policy::test_support::COMPOSIO_SEND_SLUG,
            serde_json::json!({ "to": "a@b.test" }),
        );
        requests
            .grants()
            .grant(crate::runtime::grants::GrantedCall {
                approval_id: ApprovalId::new("appr-1"),
                agent: "ceo".into(),
                tool: "composio_execute".into(),
                args: args.clone(),
                at_millis: now_millis(),
                origin_thread: None,
                origin_parent: None,
                origin_task: None,
            });
        let brain = brain_with_queue_and_events_and_budget_exhausted_provider(
            dir.path(),
            requests,
            log.clone(),
        );

        let result = brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-1", Verdict::Approve)]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 1);
        let bubble = &result.channel_responses[0];
        assert_eq!(bubble.channel, "ceo");
        assert!(
            bubble
                .text
                .starts_with(BUDGET_PAUSE_NOTICE_NO_RESEND_PREFIX),
            "an approval continuation parks a background marker the redeem route refuses, so \
             its notice must carry the non-redeemable prefix — got: {}",
            bubble.text
        );
        assert!(
            !bubble.text.starts_with(BUDGET_PAUSE_NOTICE_PREFIX),
            "the pre-fix defect: this prefix is what the console keys its \"Add credits & \
             resend\" CTA off, and this marker's redeem returns 400: {}",
            bubble.text
        );
        assert!(
            bubble.text.to_ascii_lowercase().contains("add credits"),
            "the actionable ask survives into the notice: {}",
            bubble.text
        );

        // And a re-issue marker really was parked for the granting agent: the
        // notice is non-redeemable because of HOW it was parked (background),
        // not because nothing was parked at all.
        let marker = crate::runtime::grants::budget_pauses_for(&CompanyId::new("acme"))
            .peek("ceo")
            .expect("run_steered_background parks a marker on the same terms run_inner does");
        assert_eq!(marker.agent, "ceo");
    }

    /// Issue #374: a resolution that minted only a STANDING grant must still
    /// re-dispatch the agent.
    ///
    /// This is the feature's happy path, and it was the one real gap in the
    /// plan. `redispatch_granted_call` peeked only the single-use set and
    /// no-ops silently on a miss — correct for every legitimate miss (a deny, a
    /// native effect, a legacy park) and catastrophic here: the operator picks
    /// the broader scope, the permission is armed, and the call they were
    /// looking at never runs. It would have looked exactly like #243's original
    /// bug, one scope over.
    #[tokio::test]
    async fn a_standing_grant_also_redispatches_its_agent() {
        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path().to_path_buf()));
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        requests
            .grants()
            .grant_standing(crate::runtime::grants::StandingGrant {
                id: crate::runtime::grants::GrantId::new("g1"),
                agent: "ceo".into(),
                workflow: None,
                tool: "workspace_write".into(),
                verdict: Verdict::Approve,
                granted_by: crate::ports::types::Actor {
                    kind: crate::ports::types::ActorKind::User,
                    id: "user-1".into(),
                },
                approval_id: ApprovalId::new("appr-1"),
                at_millis: now_millis(),
                expires_at_millis: now_millis() + 60 * 60 * 1000,
                origin_thread: None,
                origin_parent: None,
                origin_task: None,
                scope: None,
            });
        let brain = brain_with_queue_and_events(dir.path(), requests, log.clone());

        let result = brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-1", Verdict::Approve)]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 1);
        let bubble = &result.channel_responses[0];
        assert_eq!(bubble.channel, "ceo");
        assert_ne!(
            bubble.text, "Acknowledged.",
            "a standing grant must re-dispatch, not fall through to the no-op"
        );
        assert!(bubble.text.contains("workspace_write"), "{}", bubble.text);
        // No exact-arguments pin: a standing grant admits any arguments, which
        // is what the operator consented to by choosing this scope. Telling the
        // model to reproduce a specific argument object would make the broad
        // scope behave like the narrow one.
        assert!(
            !bubble.text.contains("Do not modify them"),
            "a standing grant must not pin arguments: {}",
            bubble.text
        );

        // Journaling the reply belongs to the runtime now (issue #469), so the
        // brain must not write a second copy. See
        // `server::operator::test::a_continuation_answers_in_the_thread_the_sign_off_was_raised_in`
        // for the round trip.
        assert!(no_replies_journaled(&log).await);
    }

    /// A DENIED approval runs no turn. "No" must never re-dispatch anything.
    #[tokio::test]
    async fn a_denied_approval_redispatches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path().to_path_buf()));
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        // A grant for a DIFFERENT approval is live, to prove the arm keys on the
        // resolved id rather than reaching for whatever is lying around.
        requests
            .grants()
            .grant(crate::runtime::grants::GrantedCall {
                approval_id: ApprovalId::new("appr-other"),
                agent: "ceo".into(),
                tool: "composio_execute".into(),
                args: serde_json::json!({}),
                at_millis: now_millis(),
                origin_thread: None,
                origin_parent: None,
                origin_task: None,
            });
        requests
            .grants()
            .grant_standing(crate::runtime::grants::StandingGrant {
                id: crate::runtime::grants::GrantId::new("deny-1"),
                agent: "ceo".into(),
                workflow: None,
                tool: "workspace_write".into(),
                verdict: Verdict::Deny,
                granted_by: crate::ports::types::Actor {
                    kind: crate::ports::types::ActorKind::User,
                    id: "user-1".into(),
                },
                approval_id: ApprovalId::new("appr-1"),
                at_millis: now_millis(),
                expires_at_millis: now_millis() + 60_000,
                origin_thread: None,
                origin_parent: None,
                origin_task: None,
                scope: None,
            });
        let brain = brain_with_queue_and_events(dir.path(), requests, log.clone());

        let result = brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-1", Verdict::Deny)]),
                &NoopHost,
            )
            .await
            .unwrap();

        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(
            result.channel_responses[0].text, "Acknowledged.",
            "a deny falls through to the fallback, exactly as before #243"
        );
        assert!(
            log.read_from(&CompanyId::new("acme"), crate::ports::EventSeq::new(0), 100)
                .await
                .unwrap()
                .is_empty(),
            "nothing is journaled for a deny"
        );
    }

    // --- Issue #453: a re-dispatch drains what its turn queued ---------------

    /// What the scripted model does on each successive `/chat/completions` call.
    #[derive(Clone, Debug)]
    enum ScriptTurn {
        /// Emit a native tool call.
        Call {
            tool: &'static str,
            args: serde_json::Value,
        },
        /// Finish with plain assistant text.
        Say(&'static str),
    }

    /// Serves a scripted OpenAI-compatible endpoint on loopback and returns its
    /// base URL.
    ///
    /// `MockProvider` cannot express a tool call, and a tool call is the whole
    /// point here: the defect is that a `review_task` made by a re-issued turn
    /// was staged and never drained. Same shape `workspace_turn_test` and
    /// `gated_tool_turn_test` established — stub exactly one boundary, the
    /// model's choices, and run everything else for real.
    async fn spawn_model_script(turns: Vec<ScriptTurn>) -> String {
        use axum::Json;
        use axum::routing::post;

        let script = Arc::new(std::sync::Mutex::new(turns));
        let app = axum::Router::new().route(
            "/chat/completions",
            post(move |Json(_body): Json<serde_json::Value>| {
                let script = Arc::clone(&script);
                async move {
                    let next = {
                        let mut turns = script.lock().unwrap();
                        if turns.is_empty() {
                            None
                        } else {
                            Some(turns.remove(0))
                        }
                    };
                    // Running off the end means the loop went round more times
                    // than expected; end the turn rather than hang.
                    let message = match next.unwrap_or(ScriptTurn::Say("done")) {
                        ScriptTurn::Say(text) => {
                            serde_json::json!({ "role": "assistant", "content": text })
                        }
                        ScriptTurn::Call { tool, args } => serde_json::json!({
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": format!("call-{tool}"),
                                "type": "function",
                                "function": { "name": tool, "arguments": args.to_string() }
                            }]
                        }),
                    };
                    Json(serde_json::json!({
                        "choices": [{ "index": 0, "message": message }],
                        "usage": { "prompt_tokens": 12, "completion_tokens": 4 }
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// A brain over the scripted model with a **real task store**, so a
    /// `review_task` the re-dispatched turn makes can actually move a card.
    fn brain_over_script(
        dir: &std::path::Path,
        requests: crate::harness::policy::ApprovalRequestQueue,
        base_url: String,
    ) -> HarnessBrain {
        use crate::company::credentials::Credential;
        use crate::harness::provider::{HostedProvider, HostedProviderConfig};

        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(HostedProvider::new(HostedProviderConfig {
                base_url,
                credential: Credential::from_value("stub-key"),
                extra_headers: Vec::new(),
            })),
            provider_slug: "managed".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: Some("stub-model".to_string()),
            tasks: Some(Arc::new(FsOps::new(dir))),
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: requests,
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record())
    }

    /// A card sitting in review, waiting on the verdict the operator approved.
    fn card_in_review(id: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: format!("Work item {id}"),
            note: None,
            column: COLUMN_IN_REVIEW.to_string(),
            priority: "medium".to_string(),
            assignee: "ceo".to_string(),
            updated_at_millis: now_millis(),
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        }
    }

    fn granted(approval: &str, tool: &str) -> crate::runtime::grants::GrantedCall {
        crate::runtime::grants::GrantedCall {
            approval_id: ApprovalId::new(approval),
            agent: "ceo".into(),
            tool: tool.into(),
            args: serde_json::json!({}),
            at_millis: now_millis(),
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
        }
    }

    /// **The reachability assertion.** A test that the drain works when called
    /// is not coverage that the drain is reached — and on this path it was not.
    ///
    /// `redispatch_granted_call` runs a full toolbelt turn and claimed publishes
    /// only, so a `review_task` the re-issued call made was staged, answered
    /// with "the card has moved to done", and destroyed by the next turn's
    /// `clear()`. It **drains** rather than refusing, deliberately: `review_task`
    /// is a gateable Write effect, so refusing here would make an operator's own
    /// approval unspendable — approve, refuse, re-park.
    #[tokio::test]
    async fn a_granted_redispatch_drains_the_board_work_its_turn_queued() {
        let dir = tempfile::tempdir().unwrap();
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        requests.grants().grant(granted("appr-1", "review_task"));
        let base_url = spawn_model_script(vec![
            ScriptTurn::Call {
                tool: "review_task",
                args: serde_json::json!({ "task_id": "card-1", "decision": "approve" }),
            },
            ScriptTurn::Say("Approved."),
        ])
        .await;
        let brain = brain_over_script(dir.path(), requests, base_url);
        let tasks = brain.deps.tasks.clone().expect("task store");
        tasks
            .upsert(&CompanyId::new("acme"), &card_in_review("card-1"))
            .await
            .expect("seed the card");

        let result = brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-1", Verdict::Approve)]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");
        assert_eq!(result.channel_responses.len(), 1);

        let cards = tasks.list(&CompanyId::new("acme")).await.expect("list");
        assert_eq!(
            cards[0].column,
            crate::ports::tasks::COLUMN_DONE,
            "the approved card must actually move — staging it and returning was the defect"
        );
        assert_eq!(
            brain.deps.delegations.queued(),
            0,
            "and nothing may be left for a later turn's clear() to destroy"
        );
        assert!(
            !brain.deps.delegations.drain_committed(),
            "the claim releases with the re-dispatch turn"
        );
    }

    /// The #476 nuance: one continuation cycle can run **several** re-dispatch
    /// turns, one per batched resolution. The claim is therefore per turn, not
    /// per cycle — each re-dispatch owns its own drain window.
    ///
    /// A per-cycle claim would pass a single-approval test and fail here in the
    /// worst way: the second turn's staged verdict would ride on the first
    /// turn's already-spent window, or the second acquire would clear work the
    /// first had not drained yet.
    #[tokio::test]
    async fn batched_resolutions_each_get_their_own_drain_window() {
        let dir = tempfile::tempdir().unwrap();
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        requests.grants().grant(granted("appr-1", "review_task"));
        requests.grants().grant(granted("appr-2", "review_task"));
        let base_url = spawn_model_script(vec![
            ScriptTurn::Call {
                tool: "review_task",
                args: serde_json::json!({ "task_id": "card-1", "decision": "approve" }),
            },
            ScriptTurn::Say("Approved card-1."),
            ScriptTurn::Call {
                tool: "review_task",
                args: serde_json::json!({ "task_id": "card-2", "decision": "revise" }),
            },
            ScriptTurn::Say("Sent card-2 back."),
        ])
        .await;
        let brain = brain_over_script(dir.path(), requests, base_url);
        let tasks = brain.deps.tasks.clone().expect("task store");
        let company = CompanyId::new("acme");
        for id in ["card-1", "card-2"] {
            tasks
                .upsert(&company, &card_in_review(id))
                .await
                .expect("seed the card");
        }

        let result = brain
            .run_cycle(
                cycle_over(vec![
                    approval_resolved("appr-1", Verdict::Approve),
                    approval_resolved("appr-2", Verdict::Approve),
                ]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");
        assert_eq!(
            result.channel_responses.len(),
            2,
            "both resolutions re-dispatch"
        );

        let cards = tasks.list(&company).await.expect("list");
        let column = |id: &str| {
            cards
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.column.clone())
                .unwrap_or_else(|| panic!("{id} is on the board"))
        };
        assert_eq!(
            column("card-1"),
            crate::ports::tasks::COLUMN_DONE,
            "the first re-dispatch's verdict must survive the second re-dispatch's claim"
        );
        assert_eq!(
            column("card-2"),
            crate::ports::tasks::COLUMN_TODO,
            "and the second's own verdict lands too"
        );
        assert_eq!(brain.deps.delegations.queued(), 0);
        assert!(!brain.deps.delegations.drain_committed());
    }

    /// An approved resolution with NO grant behind it is a silent no-op.
    ///
    /// This is the common case, not an edge: a native effect the runtime already
    /// executed, a legacy parked effect from before `Effect::agent` existed (it
    /// replays as `None` and mints nothing), a grant already consumed, and a
    /// grant already swept all land here. Every one of them must keep the exact
    /// pre-#243 behaviour rather than manufacturing a turn.
    #[tokio::test]
    async fn an_approval_with_no_grant_is_a_silent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path().to_path_buf()));
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        let brain = brain_with_queue_and_events(dir.path(), requests, log.clone());

        let result = brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-native", Verdict::Approve)]),
                &NoopHost,
            )
            .await
            .unwrap();

        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].text, "Acknowledged.");
        assert!(
            log.read_from(&CompanyId::new("acme"), crate::ports::EventSeq::new(0), 100)
                .await
                .unwrap()
                .is_empty()
        );
    }

    // --- Steer disposition (issue #111) -------------------------------------

    use crate::company::steer::{InflightKind, InflightRegistry};
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    /// A model that steers its OWN in-flight run on selected turns (via the
    /// shared registry), so the disposition matrix can be driven deterministically
    /// over an offline turn. It pops one queued action per [`invoke`](ChatModel::invoke)
    /// call and applies it against `key`, then echoes the last user message.
    struct SteeringProvider {
        steer: InflightRegistry,
        company: CompanyId,
        key: String,
        actions: StdMutex<VecDeque<SteerAction>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ChatModel<()> for SteeringProvider {
        async fn invoke(
            &self,
            _state: &(),
            request: ModelRequest,
        ) -> tinyagents::Result<ModelResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(action) = self.actions.lock().unwrap().pop_front() {
                let key = if self.key.is_empty() {
                    self.steer
                        .list(&self.company)
                        .into_iter()
                        .next()
                        .map(|entry| entry.key)
                        .unwrap_or_default()
                } else {
                    self.key.clone()
                };
                let _ = self.steer.steer(&self.company, &key, action);
            }
            let message = request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::User(_)))
                .map(|m| m.text())
                .unwrap_or_default();
            Ok(ModelResponse::assistant(format!("did: {message}")))
        }
    }

    impl HarnessModel for SteeringProvider {
        fn telemetry_provider_id(&self) -> String {
            "steering".to_string()
        }
    }

    /// A deterministic turn result for scheduled-cycle edge-case tests.
    struct FixedOutcomeTurn {
        outcome: crate::harness::built_in::TurnOutcome,
        approval_requests: Option<crate::harness::policy::ApprovalRequestQueue>,
    }

    #[async_trait]
    impl crate::runtime::delegation::RunTurn for FixedOutcomeTurn {
        async fn run(
            &self,
            _company: &CompanyId,
            _agent_id: &str,
            _message: &str,
            _chat: crate::runtime::delegation::ChatTarget<'_>,
        ) -> Result<crate::harness::built_in::TurnOutcome> {
            if let Some(requests) = &self.approval_requests {
                for index in 0..(crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN + 1) {
                    requests.push(crate::harness::policy::ApprovalRequest {
                        tool: format!("test_tool_{index}"),
                        reason: "test approval".to_string(),
                        effect: Effect {
                            kind: format!("test_tool_{index}"),
                            group: crate::ports::types::EffectGroup::Other,
                            amount_usd: None,
                            established_thread: false,
                            first_time_counterparty: false,
                            payload: serde_json::json!({ "index": index }),
                            agent: None,
                            run_id: None,
                        },
                    });
                }
            }
            Ok(self.outcome.clone())
        }

        async fn run_steered(
            &self,
            company: &CompanyId,
            agent_id: &str,
            message: &str,
            _control: &crate::company::steer::SteerControl,
            chat: crate::runtime::delegation::ChatTarget<'_>,
            _run_sink: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
        ) -> Result<crate::harness::built_in::TurnOutcome> {
            self.run(company, agent_id, message, chat).await
        }

        async fn run_steered_background(
            &self,
            company: &CompanyId,
            agent_id: &str,
            message: &str,
            _control: &crate::company::steer::SteerControl,
            _run_sink: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
        ) -> Result<crate::harness::built_in::TurnOutcome> {
            self.run(
                company,
                agent_id,
                message,
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
        }
    }

    /// A brain whose provider steers the dispatched card `key` with `actions`
    /// (one per turn). Returns the brain + its task store so a test can seed the
    /// card and read the disposition back.
    fn brain_that_steers_itself(
        dir: &std::path::Path,
        key: &str,
        actions: Vec<SteerAction>,
    ) -> (HarnessBrain, Arc<FsOps>, Arc<SteeringProvider>) {
        let steer = InflightRegistry::new();
        let tasks = Arc::new(FsOps::new(dir));
        let provider = Arc::new(SteeringProvider {
            steer: steer.clone(),
            company: CompanyId::new("acme"),
            key: key.to_string(),
            actions: StdMutex::new(actions.into_iter().collect()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: provider.clone(),
            provider_slug: "steering".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks.clone()),
            // Same handle as `tasks` (FsOps is both stores), so a steered run's
            // artifact side effect — or the absence of one — is observable.
            artifacts: Some(tasks.clone()),
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer,
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()),
            tasks,
            provider,
        )
    }

    /// Cancel mid-flight → the card returns to `todo`, the partial reply is
    /// DISCARDED, and only the operator cancellation note lands.
    #[tokio::test]
    async fn steer_cancel_returns_to_todo_and_discards_partial() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks, _) =
            brain_that_steers_itself(dir.path(), "t1", vec![SteerAction::Cancel]);
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        assert_eq!(moved.column, COLUMN_TODO);
        let note = moved.note.expect("note");
        assert!(note.contains("cancelled while in flight"), "{note:?}");
        // The agent's partial reply must NOT be preserved on a cancel.
        assert!(
            !note.contains("did: "),
            "cancel discards the partial: {note:?}"
        );
    }

    /// Pause mid-flight → the card parks in the new `paused` column and the
    /// partial reply is PRESERVED in the note.
    #[tokio::test]
    async fn steer_pause_parks_in_paused_and_preserves_partial() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks, _) =
            brain_that_steers_itself(dir.path(), "t1", vec![SteerAction::Pause]);
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        assert_eq!(moved.column, "paused");
        let note = moved.note.expect("note");
        assert!(note.contains("[paused]"), "{note:?}");
        assert!(
            note.contains("did: "),
            "pause preserves the partial: {note:?}"
        );
    }

    /// Redirect on every turn → the run re-runs in-loop carrying the operator
    /// instruction, and the per-dispatch redirect cap (3) finalizes it to
    /// `in_review` instead of looping forever.
    #[tokio::test]
    async fn steer_redirect_reruns_and_the_cap_finalizes_to_in_review() {
        let dir = tempfile::tempdir().unwrap();
        let redirect = || SteerAction::Redirect {
            instruction: "focus on the API".to_string(),
        };
        // Steer a redirect on the first several turns; the cap should stop it.
        let (brain, tasks, provider) = brain_that_steers_itself(
            dir.path(),
            "t1",
            vec![redirect(), redirect(), redirect(), redirect()],
        );
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        // Redirect budget exhausted → finalized, not looping.
        assert_eq!(moved.column, "in_review");
        let note = moved.note.expect("note");
        // The operator instruction was carried into the rerun, and the reruns
        // echoed it back through the "Operator redirect:" preamble.
        assert!(note.contains("focus on the API"), "{note:?}");
        assert!(
            note.contains("Operator redirect:"),
            "the rerun carried the operator instruction: {note:?}"
        );
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "one initial turn plus three reruns"
        );
    }

    #[tokio::test]
    async fn steer_cancelled_delegation_returns_no_bubble() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _, _) = brain_that_steers_itself(dir.path(), "", vec![SteerAction::Cancel]);

        let result = brain
            .run_delegation(
                Delegation::DelegateToDesk {
                    desk: "engineering".to_string(),
                    instruction: "investigate".to_string(),
                },
                None,
            )
            .await
            .expect("cancellation is handled");

        assert!(
            result.bubble.is_none() && result.desk_reply.is_none(),
            "cancelled delegation must not bubble or relay"
        );
    }

    // --- CEO-relay hand-back (delegate_to_desk second turn) ------------------

    /// A provider that simulates the orchestrator queuing a `delegate_to_desk`
    /// on its turns: on each invoke it pops the next scripted delegation (if any)
    /// onto the shared queue — exactly what the real tool call does — then echoes
    /// the last user message so a test can read the turn's reply. Sharing the
    /// queue handle with [`HarnessDeps::delegations`] is what lets the brain
    /// drain it after the turn.
    /// Whether this request is a triage escalation rather than an agent turn
    /// (issue #678).
    ///
    /// Keyed on the system prompt's opening sentence, which
    /// `harness::triage::system_prompt` owns. Coupling a fixture to prose is
    /// ordinarily a smell; here the alternative is worse, because the only
    /// other thing distinguishing the two is "carries no tools", and a turn
    /// whose agent happens to have an empty belt would be misread as a
    /// classification. Pinned by `a_triage_request_is_recognised_as_one`.
    fn is_triage_request(request: &ModelRequest) -> bool {
        request
            .messages
            .first()
            .map(|m| m.text().contains("You classify one message"))
            .unwrap_or(false)
    }

    #[test]
    fn a_triage_request_is_recognised_as_one() {
        let triage = ModelRequest {
            messages: vec![
                tinyagents::harness::message::Message::system(
                    crate::harness::triage::system_prompt_for_test(),
                ),
                tinyagents::harness::message::Message::user("hello".to_string()),
            ],
            ..ModelRequest::default()
        };
        assert!(
            is_triage_request(&triage),
            "the fixture must recognise the real prompt, or it silently starts \
             eating scripted turns again"
        );
        let turn = ModelRequest {
            messages: vec![
                tinyagents::harness::message::Message::system(
                    "You are the CEO of Acme.".to_string(),
                ),
                tinyagents::harness::message::Message::user("ship it".to_string()),
            ],
            ..ModelRequest::default()
        };
        assert!(
            !is_triage_request(&turn),
            "an agent turn is not a classification"
        );
    }

    /// A provider for the selection rung (issue #1835): a request opening with
    /// the selector's own system prompt gets the scripted reply; anything else
    /// echoes. Keyed on the prompt's opening sentence for the reason
    /// [`is_triage_request`] documents, and pinned the same way below.
    struct SelectingProvider {
        reply: String,
        selector_calls: std::sync::atomic::AtomicUsize,
    }

    fn is_selection_request(request: &ModelRequest) -> bool {
        request
            .messages
            .first()
            .map(|m| {
                m.text()
                    .contains("You route one message in a group channel")
            })
            .unwrap_or(false)
    }

    #[async_trait]
    impl ChatModel<()> for SelectingProvider {
        async fn invoke(
            &self,
            _state: &(),
            request: ModelRequest,
        ) -> tinyagents::Result<ModelResponse> {
            if is_selection_request(&request) {
                self.selector_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Ok(ModelResponse::assistant(self.reply.clone()));
            }
            let message = request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::User(_)))
                .map(|m| m.text())
                .unwrap_or_default();
            Ok(ModelResponse::assistant(format!("mock: {message}")))
        }
    }

    impl HarnessModel for SelectingProvider {
        fn telemetry_provider_id(&self) -> String {
            "selecting".to_string()
        }
    }

    #[test]
    fn a_selection_request_is_recognised_as_one() {
        let selection = ModelRequest {
            messages: vec![
                tinyagents::harness::message::Message::system(
                    crate::harness::selector::system_prompt_for_test(),
                ),
                tinyagents::harness::message::Message::user("who owns login?".to_string()),
            ],
            ..ModelRequest::default()
        };
        assert!(
            is_selection_request(&selection),
            "the fixture must recognise the real prompt, or it silently starts \
             eating scripted turns"
        );
    }

    /// A brain whose provider answers every selection request with `reply`.
    /// The record is [`record_with_desk`] — `engineer` + `chief`, a lead
    /// `eng_desk` — plus an `auto` overlay channel `launch` holding both.
    fn brain_that_selects(
        dir: &std::path::Path,
        reply: &str,
    ) -> (HarnessBrain, Arc<SelectingProvider>) {
        brain_that_selects_with(dir, reply, None, None)
    }

    /// [`brain_that_selects`], plus an optional plan and usage meter, so a
    /// test can place the company past its plan-level total-token ceiling.
    fn brain_that_selects_with(
        dir: &std::path::Path,
        reply: &str,
        plan: Option<crate::harness::capability_budget::CapabilityPlan>,
        meter: Option<Arc<dyn crate::ports::usage::UsageMeter>>,
    ) -> (HarnessBrain, Arc<SelectingProvider>) {
        let provider = Arc::new(SelectingProvider {
            reply: reply.to_string(),
            selector_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: provider.clone(),
            provider_slug: "selecting".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter,
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: InflightRegistry::new(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        let mut record = record_with_desk();
        record.overlay_desks.push(crate::ports::types::OverlayDesk {
            id: "launch".to_string(),
            name: "Launch week".to_string(),
            description: None,
            members: vec!["engineer".to_string(), "chief".to_string()],
            responder: crate::ports::types::ResponderMode::Auto,
        });
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record),
            provider,
        )
    }

    /// Issue #1835, the rung itself: an unmentioned message addressed to an
    /// `auto` channel is routed to the selector's pick — a member the
    /// deterministic fallback (`engineer`, the first member) would never have
    /// chosen — and the pick is clamped to the channel.
    #[tokio::test]
    async fn an_auto_channel_routes_by_the_selectors_pick() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_selects(dir.path(), "chief");
        assert_eq!(
            brain
                .auto_channel_responder(Some("launch"), "which strategy are we running?")
                .await
                .as_deref(),
            Some("chief"),
            "the selection overrides the first-member fallback"
        );
        assert_eq!(
            provider
                .selector_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    /// The worst case of the new rung is the old rung: a pick outside the
    /// channel's membership answers `None`, and the caller keeps the
    /// deterministic fallback. Revert the clamp in `SelectorVerdict::parse`
    /// and this routes a turn to a teammate the channel does not contain.
    #[tokio::test]
    async fn a_failed_selection_keeps_the_deterministic_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _provider) = brain_that_selects(dir.path(), "somebody_else");
        assert_eq!(
            brain
                .auto_channel_responder(Some("launch"), "which strategy are we running?")
                .await,
            None,
            "an out-of-membership pick must fall back, never route"
        );
    }

    /// Issue #1872 (codex P1): the plan-level total-token ceiling gates the
    /// **selection**, not only the responder turn it precedes.
    ///
    /// Selection runs before a responder exists, so `total_ceiling_refusal`
    /// has no agent to refuse as and never fired for it — meaning a tenant
    /// past its hard ceiling could keep paying to route, one selector call per
    /// message, after the ceiling that is supposed to permit no model calls at
    /// all. Remove the `total_ceiling_spent` arm in `auto_channel_responder`
    /// and this spends a call and answers `chief`.
    #[tokio::test]
    async fn an_exhausted_total_ceiling_routes_without_paying_for_a_selection() {
        let dir = tempfile::tempdir().unwrap();
        let meter = Arc::new(SpentMeter);
        let plan = crate::harness::capability_budget::CapabilityPlan {
            period: crate::harness::capability_budget::BudgetPeriod::Daily,
            budgets: Default::default(),
            total_budget: Some(10),
        };
        let (brain, provider) =
            brain_that_selects_with(dir.path(), "chief", Some(plan), Some(meter));
        assert_eq!(
            brain
                .auto_channel_responder(Some("launch"), "which strategy are we running?")
                .await,
            None,
            "past the ceiling the deterministic fallback answers, not a selection"
        );
        assert_eq!(
            provider
                .selector_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a company past its hard ceiling must not pay to route"
        );
    }

    /// Issue #1872 (codex P2): a channel emptied *after* creation.
    ///
    /// `POST …/desks` refuses an empty auto channel, but `DELETE …/team/{id}`
    /// can retire its last roster-backed member later. There is then nobody to
    /// pick, so this defers to the caller's ladder — the orchestrator answers,
    /// as it does for any desk whose members have all gone — and spends
    /// nothing doing it. Refusing the deletion instead would mean a teammate
    /// you cannot remove because a channel names them.
    #[tokio::test]
    async fn a_channel_emptied_by_deletion_falls_back_without_paying() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_selects(dir.path(), "chief");
        brain.mutate_record(|r| {
            r.overlay_retired_agents = vec!["engineer".to_string(), "chief".to_string()];
        });
        assert_eq!(
            brain
                .auto_channel_responder(Some("launch"), "who owns the retry logic?")
                .await,
            None,
            "no candidates left: fall back rather than route to a retired teammate"
        );
        assert_eq!(
            provider
                .selector_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    /// A meter whose every query reports spend past any ceiling a test sets.
    struct SpentMeter;

    #[async_trait]
    impl crate::ports::usage::UsageMeter for SpentMeter {
        async fn record(
            &self,
            _company: &CompanyId,
            _sample: &crate::ports::usage::UsageSample,
        ) -> crate::Result<()> {
            Ok(())
        }
        async fn query(
            &self,
            _company: &CompanyId,
            _since: u64,
        ) -> crate::Result<Vec<crate::ports::usage::UsageSample>> {
            Ok(vec![crate::ports::usage::UsageSample {
                at_millis: 0,
                agent: "someone".to_string(),
                provider: "test".to_string(),
                input_tokens: 10_000,
                output_tokens: 0,
                cached_input_tokens: 0,
                cost_usd: 0.0,
                kind: crate::ports::usage::SampleKind::Inference,
                run_id: None,
                model: None,
            }])
        }
    }

    /// The short-circuits spend nothing: a lead desk never reaches the
    /// selector at all, and a single-member channel is its member without a
    /// model call — a pick over one candidate is the fallback with latency.
    #[tokio::test]
    async fn lead_desks_and_single_member_channels_never_pay_for_selection() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_selects(dir.path(), "chief");
        // The lead desk from `record_with_desk` is not an auto channel.
        assert_eq!(
            brain
                .auto_channel_responder(Some("eng_desk"), "hello")
                .await,
            None
        );
        // Shrink the channel to one member: it answers without the model.
        brain.mutate_record(|r| {
            r.overlay_desks[0].members = vec!["chief".to_string()];
        });
        assert_eq!(
            brain
                .auto_channel_responder(Some("launch"), "hello")
                .await
                .as_deref(),
            Some("chief")
        );
        assert_eq!(
            provider
                .selector_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "neither path may spend a selection call"
        );
    }

    struct DelegatingProvider {
        queue: orchestrator::DelegationQueue,
        pushes: StdMutex<VecDeque<Vec<Delegation>>>,
        calls: std::sync::atomic::AtomicUsize,
        /// The same task store the brain writes through, so each invoke can
        /// snapshot the board **as that turn sees it** — the only way a test can
        /// observe a dispatched card mid-run (issue #204).
        tasks: Arc<FsOps>,
        /// `(column, assignee)` of the company's card at each invoke, in order.
        board: StdMutex<Vec<(String, String)>>,
        /// How this provider misbehaves, by invoke number.
        faults: TurnFaults,
        /// The same registry wired into [`HarnessDeps::steer`], so a scripted
        /// invoke can cancel its own in-flight delegation.
        steer: InflightRegistry,
    }

    /// How a [`DelegatingProvider`] misbehaves, keyed by 1-based invoke number
    /// (issue #213 review).
    #[derive(Default)]
    struct TurnFaults {
        /// Every invoke from here on ERRORS instead of answering, so a test can
        /// make a delegate's own run fail. A *from*, not an *on*: openhuman's
        /// agent loop retries a failed provider call within the same turn, so
        /// failing a single invoke only makes the turn succeed on its retry.
        fail_from: Option<usize>,
        /// Invokes that CANCEL their own in-flight delegation mid-run, so the
        /// delegated reply is discarded exactly as an operator cancel does.
        cancel_on: Vec<usize>,
        /// Desk keys the first turn's `delegate_to_desk` calls named and the
        /// tool REFUSED (issue #272). A refusal never becomes a `Delegation`,
        /// so this is how a test reproduces one without standing up the tool.
        refused_on_first: Vec<String>,
    }

    impl DelegatingProvider {
        /// The board snapshot each turn ran against, in invoke order.
        fn board(&self) -> Vec<(String, String)> {
            self.board.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ChatModel<()> for DelegatingProvider {
        async fn invoke(
            &self,
            _state: &(),
            request: ModelRequest,
        ) -> tinyagents::Result<ModelResponse> {
            // Issue #678: a triage escalation is a classification, not a turn.
            // It rides the same `HarnessModel` handle the roster runs on, so
            // without this it would consume a scripted push and shift every
            // turn's script by one — the delegation the test wrote for turn 1
            // would be staged by a call that is not turn 1.
            //
            // Answering `chatter` rather than declining keeps these fixtures on
            // the ungated path they were written for: only an `answer` verdict
            // narrows the claim, so `chatter` leaves the gate exactly where the
            // abstention left it. A test that wants the narrowing drives it
            // through `DelegationRunner::with_triage` directly, where the
            // verdict is scripted.
            if is_triage_request(&request) {
                return Ok(ModelResponse::assistant("chatter".to_string()));
            }
            let invoke = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if self.faults.fail_from.is_some_and(|from| invoke >= from) {
                return Err(tinyagents::TinyAgentsError::Model(
                    "the delegate's provider fell over".to_string(),
                ));
            }
            if self.faults.cancel_on.contains(&invoke) {
                // The delegation's own in-flight entry, not the dispatched
                // card's — cancelling the card would end the whole run.
                let company = CompanyId::new("acme");
                if let Some(entry) = self
                    .steer
                    .list(&company)
                    .into_iter()
                    .find(|e| e.kind == InflightKind::Delegation)
                {
                    let _ = self.steer.steer(&company, &entry.key, SteerAction::Cancel);
                }
            }
            let snapshot = self
                .tasks
                .list(&CompanyId::new("acme"))
                .await
                .ok()
                .and_then(|cards| cards.into_iter().next())
                .map(|card| (card.column, card.assignee))
                .unwrap_or_default();
            self.board.lock().unwrap().push(snapshot);
            for delegation in self.pushes.lock().unwrap().pop_front().unwrap_or_default() {
                self.queue.push(delegation);
            }
            if invoke == 1 {
                for desk in &self.faults.refused_on_first {
                    self.queue.push_refusal(desk.clone());
                }
            }
            let message = request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::User(_)))
                .map(|m| m.text())
                .unwrap_or_default();
            Ok(ModelResponse::assistant(format!("did: {message}")))
        }
    }

    impl HarnessModel for DelegatingProvider {
        fn telemetry_provider_id(&self) -> String {
            "delegating".to_string()
        }
    }

    /// A brain over the desk-bearing record whose provider is a
    /// [`DelegatingProvider`] scripted to push `pushes[i]` on invoke `i + 1`.
    /// Returns the brain plus the shared provider so a test can read the invoke
    /// count.
    fn brain_that_delegates(
        dir: &std::path::Path,
        pushes: Vec<Option<Delegation>>,
    ) -> (HarnessBrain, Arc<DelegatingProvider>) {
        brain_that_delegates_with(
            dir,
            pushes.into_iter().map(Vec::from_iter).collect(),
            TurnFaults::default(),
        )
    }

    /// [`brain_that_delegates`], but each invoke pushes a whole *set* of
    /// delegations (a single turn can queue several), and per-invoke faults can
    /// make a delegate's run fail or be cancelled mid-flight.
    fn brain_that_delegates_with(
        dir: &std::path::Path,
        pushes: Vec<Vec<Delegation>>,
        faults: TurnFaults,
    ) -> (HarnessBrain, Arc<DelegatingProvider>) {
        let queue = orchestrator::DelegationQueue::default();
        let tasks = Arc::new(FsOps::new(dir));
        let steer = InflightRegistry::new();
        let provider = Arc::new(DelegatingProvider {
            queue: queue.clone(),
            pushes: StdMutex::new(pushes.into_iter().collect()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            tasks: tasks.clone(),
            board: StdMutex::new(Vec::new()),
            faults,
            steer: steer.clone(),
        });
        let deps = HarnessDeps {
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: provider.clone(),
            provider_slug: "delegating".to_string(),
            serves: None,
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks),
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            artifacts: None,
            delegations: queue,
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_revisions: None,
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer,
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
            workflow_runs: None,
            deep_trace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_with_desk()),
            provider,
        )
    }

    /// (a) After a `delegate_to_desk`, the operator-facing reply is a SECOND
    /// orchestrator turn that relays the teammate's answer — one coherent
    /// bubble, not a disconnected sibling.
    #[tokio::test]
    async fn delegate_to_desk_relays_the_answer_in_a_second_orchestrator_turn() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 (orchestrator) queues a delegate_to_desk; invoke 2 is the desk
        // lead's turn; invoke 3 is the relay turn (queues nothing).
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "diagnose the outage".to_string(),
            })],
        );

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "why is the site down?".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        // The operator sees ONE bubble — the CEO's relay, not a separate teammate
        // sibling bubble.
        assert_eq!(result.channel_responses.len(), 1);
        let bubble = &result.channel_responses[0];
        assert_eq!(bubble.channel, "operator");
        // Three turns ran: orchestrator → desk lead → exactly one relay turn.
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "orchestrator, desk lead, then exactly one relay turn"
        );
        // The relayed bubble carries the teammate's answer (the desk lead echoed
        // its instruction, and the relay prompt embeds that reply under an
        // `engineer replied:` frame) — proving the operator reply is the SECOND
        // turn relaying the teammate, not the pre-delegation first reply.
        assert!(
            bubble.text.contains("engineer replied:")
                && bubble.text.contains("diagnose the outage"),
            "the relay carries the teammate's answer: {:?}",
            bubble.text
        );
        // …and it is the relay turn, whose prompt framed the hand-back.
        assert!(
            bubble.text.contains("Relay their answer"),
            "the operator bubble is the relay turn: {:?}",
            bubble.text
        );
    }

    /// (b) The relay turn cannot re-delegate: a delegation it queues is
    /// discarded, so no further desk turn or relay runs (cost stays bounded to
    /// one extra turn).
    #[tokio::test]
    async fn the_relay_turn_cannot_re_delegate() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 queues a delegation; invoke 3 (the relay) ALSO tries to queue
        // one — which must be discarded, so no fourth/fifth turn runs.
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![
                Some(Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "first".to_string(),
                }),
                None, // the desk lead's turn queues nothing
                Some(Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "second".to_string(),
                }),
            ],
        );

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "handle it".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        // Exactly three turns: orchestrator, desk lead, relay. The relay's queued
        // delegation was dropped — no fourth (desk-lead) or fifth (relay) turn.
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the relay turn's delegation is discarded — one extra turn, no loop"
        );
        // The discard actually emptied the queue (not left dirty for next cycle).
        assert_eq!(
            brain.deps.delegations.queued(),
            0,
            "the relay turn's queued delegation was discarded"
        );
        // Still exactly one operator bubble.
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
    }

    /// (c) A normal, non-delegating message still produces exactly one turn — the
    /// relay path is entered only when a `delegate_to_desk` actually answered.
    #[tokio::test]
    async fn a_non_delegating_message_runs_exactly_one_turn() {
        let dir = tempfile::tempdir().unwrap();
        // No scripted delegations → the orchestrator answers directly.
        let (brain, provider) = brain_that_delegates(dir.path(), Vec::new());

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "status?".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no delegation → a single orchestrator turn, no relay"
        );
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
        assert!(
            result.channel_responses[0].text.contains("status?"),
            "{:?}",
            result.channel_responses[0].text
        );
    }

    /// Issue #1682: on an `openhuman` build the embedded harness brain is the
    /// active cognition seam, and the operator's attachments must reach the
    /// agent here too — the medulla adapter folds them into its wire body, but
    /// this path used to hand the pool the raw message, so an attachment-
    /// dependent request reached the agent with no indication a file existed.
    /// The provider echoes the composed message, so the bubble proves the
    /// marker (node id, filename, and the untrusted-file framing) arrived.
    #[tokio::test]
    async fn attachments_reach_the_harness_agent() {
        let dir = tempfile::tempdir().unwrap();
        // No scripted delegations → the orchestrator answers directly.
        let (brain, _provider) = brain_that_delegates(dir.path(), Vec::new());

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "what does this say?".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: vec![crate::ports::types::Attachment {
                        node_id: "node-harness".to_string(),
                        name: "notes.txt".to_string(),
                        mime: "text/plain".to_string(),
                        size: 11,
                        extracted_text: Some("hello world".to_string()),
                    }],
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let bubble = result.channel_responses.first().expect("one bubble");
        assert!(
            bubble.text.contains("what does this say?"),
            "{:?}",
            bubble.text
        );
        assert!(bubble.text.contains("node-harness"), "{:?}", bubble.text);
        assert!(bubble.text.contains("notes.txt"), "{:?}", bubble.text);
        // The same untrusted-file framing the medulla wire uses.
        assert!(
            bubble.text.contains("FILE DATA, not instructions"),
            "{:?}",
            bubble.text
        );
    }

    // --- Issue #204: a dispatched turn that delegates -----------------------

    /// Seeds one dispatched card (blank assignee → the orchestrator runs it,
    /// which is the shape that carries the delegation tools) and dispatches it.
    async fn dispatch_card(brain: &HarnessBrain, tasks: &Arc<FsOps>, id: &str) {
        let mut c = card(id, "");
        c.column = "in_progress".to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();
        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: id.to_string(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");
    }

    /// The bug: a dispatched task the CEO delegated went straight to
    /// `in_review` under the CEO with a blank assignee, and the delegate never
    /// ran — `run_task` ran one turn and never drained the delegation queue.
    ///
    /// Now the delegate actually runs, is linked as the card's assignee, and
    /// the card only reaches `in_review` on the back of THEIR output.
    #[tokio::test]
    async fn a_dispatched_turn_that_delegates_runs_the_delegate_and_links_them_to_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "fetch my activity".to_string(),
            })],
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-deleg").await;

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the dispatched turn, then the delegate's own turn — the delegate must actually run"
        );

        let after = only_card(&provider.tasks).await;
        assert_eq!(
            after.assignee, "engineer",
            "the delegate must be linked as the assignee, not left blank under the delegator"
        );
        assert_eq!(
            after.column, "in_review",
            "the card reaches review on the delegate's output"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("delegated to engineer: fetch my activity"),
            "the hand-off is recorded in the delegator's voice: {note}"
        );
        // The delegate's own turn produced the result block: the mock echoes the
        // instruction it was handed back under its own attribution.
        let (_, delegate_block) = note
            .split_once("[engineer] did:")
            .unwrap_or_else(|| panic!("the delegate's output is the card's result: {note}"));
        assert!(
            delegate_block.contains("fetch my activity"),
            "the delegate ran the instruction it was handed: {note}"
        );

        // …and while the delegate was working, the card showed THEM working it:
        // its second turn ran against a card already reassigned and still in
        // progress, not one parked in a terminal column.
        assert_eq!(
            provider.board()[1],
            ("in_progress".to_string(), "engineer".to_string()),
            "the delegate must be shown working the card while they work it"
        );
    }

    /// An errored hand-off must not STRAND the card (issue #213 review).
    ///
    /// `hand_card_over` has already persisted the card as `in_progress`
    /// reassigned to the delegate before their turn starts. If that turn then
    /// errors and the failure is propagated out of `run_task`, the settle and
    /// the final `upsert` are both skipped and the card sits in `in_progress`
    /// under the delegate with no result — and nothing re-dispatches it, because
    /// `task_enters_in_progress` only edge-fires on the *transition* into that
    /// column and the card is already there. Exactly the state issue #204 exists
    /// to eliminate, reintroduced through the error path.
    ///
    /// So an errored hand-off takes the same arm an errored turn does: settle
    /// `Failed` → `todo`, with the reason on the note.
    #[tokio::test]
    async fn a_hand_off_whose_delegate_errors_lands_in_todo_not_stranded_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 is the dispatched orchestrator turn (queues the hand-off);
        // invoke 2 is the delegate's own turn, which errors.
        let (brain, provider) = brain_that_delegates_with(
            dir.path(),
            vec![vec![Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "fetch my activity".to_string(),
            }]],
            TurnFaults {
                fail_from: Some(2),
                ..TurnFaults::default()
            },
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-boom").await;

        let after = only_card(&provider.tasks).await;
        assert_eq!(
            after.column, COLUMN_TODO,
            "an errored hand-off must return the card to To-do, never leave it stranded in \
             progress where nothing will re-dispatch it"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("hand-off failed:"),
            "the failure reason lands on the note: {note}"
        );
        assert!(
            note.contains("delegated to engineer: fetch my activity"),
            "the hand-off that was attempted is still recorded: {note}"
        );
        // The failure is the assignee's, not the operator's — a cancellation is
        // the only ending attributed to `operator`.
        assert!(
            !note.contains("[operator]"),
            "an errored run is not an operator cancellation: {note}"
        );
    }

    /// A hand-off an operator cancels mid-flight produced nothing, so the card
    /// returns to `todo` reported as the cancellation it actually was.
    ///
    /// The claim is only made because `run_delegation` reports the cancellation
    /// as a fact (`DelegationOutcome::cancelled`); a hand-off that ends empty
    /// for any other reason no longer reaches this arm at all (issue #213
    /// review finding 2).
    #[tokio::test]
    async fn a_cancelled_hand_off_returns_the_card_to_todo_as_a_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 is the dispatched orchestrator turn (queues the hand-off);
        // invoke 2 is the delegate's turn, which cancels itself mid-run.
        let (brain, provider) = brain_that_delegates_with(
            dir.path(),
            vec![vec![Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "fetch my activity".to_string(),
            }]],
            TurnFaults {
                cancel_on: vec![2],
                ..TurnFaults::default()
            },
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-cancel").await;

        let after = only_card(&provider.tasks).await;
        assert_eq!(
            after.column, COLUMN_TODO,
            "a cancelled hand-off must not read as finished, and must not strand in progress"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("the delegated run was cancelled before it produced anything"),
            "the cancellation is reported as the cause: {note}"
        );
        // A cancellation is the operator's act, so the block is theirs — not the
        // delegate's, who never said it.
        assert!(
            note.contains("[operator] the delegated run was cancelled"),
            "a cancellation is attributed to the operator: {note}"
        );
    }

    /// A later hand-off that ANSWERS must not be discarded by an earlier one
    /// that produced nothing (issue #213 review finding 3).
    ///
    /// Before this, the first hand-off owned the card unconditionally. A first
    /// hand-off cancelled mid-flight still produced a `TaskHandoff`, so a second
    /// hand-off in the same drain took the "does not own the card" arm: its
    /// answer was appended to the note, but the card still settled `Cancelled`
    /// -> `todo` off the first. Work that ran and produced output ended up
    /// filed under a card marked cancelled.
    #[tokio::test]
    async fn a_later_answering_hand_off_takes_the_card_over_from_an_earlier_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 is the dispatched orchestrator turn, queuing BOTH hand-offs.
        // Invoke 2 is the first delegate's run, cancelled mid-flight; invoke 3
        // is the second delegate's run, which answers.
        let (brain, provider) = brain_that_delegates_with(
            dir.path(),
            vec![vec![
                Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "first attempt".to_string(),
                },
                Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "second attempt".to_string(),
                },
            ]],
            TurnFaults {
                cancel_on: vec![2],
                ..TurnFaults::default()
            },
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-two-handoffs").await;

        let after = only_card(&provider.tasks).await;
        assert_eq!(
            after.column, "in_review",
            "the card settles from the hand-off that actually produced work, not from the \
             cancelled one that preceded it"
        );
        assert_eq!(
            after.assignee, "engineer",
            "the delegate that produced the work owns the card"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("second attempt"),
            "the answering hand-off's output is the card's result: {note}"
        );
        assert!(
            !note.contains("the delegated run was cancelled before it produced anything"),
            "an earlier cancelled hand-off must not settle a card a later one completed: {note}"
        );
    }

    /// The compatibility half: a dispatched turn that delegates nothing still
    /// runs exactly one turn and settles under the agent that ran it.
    ///
    /// (Deliberately no assertion on `assignee` — linking the *non-delegating*
    /// working agent to the card is issue #205's fix, and this test must not
    /// pin the blank it leaves behind today.)
    #[tokio::test]
    async fn a_dispatched_turn_that_delegates_nothing_settles_exactly_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(dir.path(), Vec::new());
        dispatch_card(&brain, &provider.tasks.clone(), "t-plain").await;

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no delegation → one turn, no hand-off"
        );
        let after = only_card(&provider.tasks).await;
        assert_eq!(after.column, "in_review");
        assert!(
            after.note.expect("note").contains("[chief]"),
            "the agent that ran it owns the result"
        );
    }

    /// A hand-off to a desk nobody leads has nowhere to go: rather than
    /// stranding the card in `in_progress` waiting on a delegate that will
    /// never run, the delegator's own reply settles it exactly as before.
    #[tokio::test]
    async fn a_hand_off_to_an_unknown_desk_settles_under_the_delegator() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::DelegateToDesk {
                desk: "ghost".to_string(),
                instruction: "look into it".to_string(),
            })],
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-ghost").await;

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an unknown desk has no lead to run a second turn"
        );
        let after = only_card(&provider.tasks).await;
        assert_eq!(
            after.column, "in_review",
            "the card must not strand in progress waiting on a delegate that cannot run"
        );
        assert_ne!(after.assignee, "ghost");
    }

    /// Issue #272: settling under the delegator is the right *behaviour* (#213
    /// chose it so a card is never stranded), but it used to be silent — the
    /// board showed a card whose note claimed a hand-off and whose owner was the
    /// delegator, with nothing connecting the two. The undeliverable hand-off is
    /// now recorded on the card, so an operator reads the fact instead of
    /// inferring it from an absence.
    #[tokio::test]
    async fn a_hand_off_that_cannot_be_delivered_says_so_on_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::DelegateToDesk {
                desk: "ghost".to_string(),
                instruction: "look into it".to_string(),
            })],
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-loud").await;

        let note = only_card(&provider.tasks).await.note.expect("note");
        assert!(
            note.contains("hand-off to \"ghost\" was not delivered"),
            "the card must name the hand-off that did not happen: {note}"
        );
        assert!(
            note.contains("this card is still with chief"),
            "the card must say who still owns it: {note}"
        );
    }

    /// Issue #272, the grounded half: the tool refused the invented target, so
    /// no `Delegation` was ever queued. The turn is still free to *say* it
    /// handed the work off — that is exactly what happened on the live company
    /// — so the board records the refusal independently of the turn's account
    /// of it. Without this the card settles under the delegator with a note
    /// that claims a hand-off and nothing anywhere contradicting it.
    #[tokio::test]
    async fn a_refused_hand_off_is_recorded_on_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates_with(
            dir.path(),
            vec![Vec::new()],
            TurnFaults {
                refused_on_first: vec!["writer".to_string()],
                ..TurnFaults::default()
            },
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-refused").await;

        let after = only_card(&provider.tasks).await;
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a refused hand-off runs no delegate"
        );
        assert_eq!(
            after.column, "in_review",
            "the card still settles under the delegator (#213); it is only no longer silent"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("hand-off to \"writer\" was not delivered"),
            "the refused target must be named on the card: {note}"
        );
        assert!(
            note.contains("not somewhere this company can hand work to"),
            "the cause must be on the card: {note}"
        );
    }

    /// The other half of #272's note: a delegation that never had a desk target
    /// (a `spawn_task`) must not pick up an undeliverable-hand-off line.
    #[tokio::test]
    async fn a_spawn_task_never_records_an_undeliverable_hand_off() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::SpawnTask {
                title: "Follow up".to_string(),
                note: None,
                assignee: None,
            })],
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-quiet").await;

        let cards = provider.tasks.list(&CompanyId::new("acme")).await.unwrap();
        let parent = cards.iter().find(|c| c.id == "t-quiet").expect("parent");
        assert!(
            !parent
                .note
                .as_deref()
                .unwrap_or_default()
                .contains("was not delivered"),
            "a spawn_task has no desk target to fail: {:?}",
            parent.note
        );
    }

    /// A `spawn_task` queued by a *dispatched* turn now opens its card with the
    /// dispatched card as its parent — the lineage `run_delegation` could never
    /// stamp while the task path did not drain the queue (issue #185's
    /// `parent_task_id`).
    #[tokio::test]
    async fn a_task_spawned_by_a_dispatched_turn_records_its_parent_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::SpawnTask {
                title: "Follow up next week".to_string(),
                note: None,
                assignee: None,
            })],
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-parent").await;

        let cards = provider.tasks.list(&CompanyId::new("acme")).await.unwrap();
        let spawned = cards
            .iter()
            .find(|c| c.title == "Follow up next week")
            .expect("the spawned card must actually be opened");
        assert_eq!(spawned.column, COLUMN_TODO);
        assert_eq!(
            spawned.parent_task_id.as_deref(),
            Some("t-parent"),
            "a card spawned inside a dispatch remembers the card it came from"
        );
        // The parent still settles on its own turn's reply — spawning follow-up
        // work is not a hand-off.
        let parent = cards.iter().find(|c| c.id == "t-parent").expect("parent");
        assert_eq!(parent.column, "in_review");
    }

    // ---- named harnesses: does the wiring actually route? ------------------

    /// Records which agents it ran, so a test can assert *which* lane served a
    /// turn rather than only that one did.
    struct SpyLane {
        label: String,
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl crate::runtime::delegation::RunTurn for SpyLane {
        async fn run(
            &self,
            _company: &CompanyId,
            agent_id: &str,
            _message: &str,
            _chat: crate::runtime::delegation::ChatTarget<'_>,
        ) -> Result<crate::harness::built_in::TurnOutcome> {
            self.seen.lock().unwrap().push(agent_id.to_string());
            Ok(crate::harness::built_in::TurnOutcome {
                reply: self.label.clone(),
                steps: Vec::new(),
                hit_iteration_cap: false,
                // Test fixture, not the ACP fold (PR #1880 review).
                abnormal_stop: None,
                halted_for_spend: None,
                budget_paused: None,
            })
        }
        async fn run_steered(
            &self,
            c: &CompanyId,
            a: &str,
            m: &str,
            _: &crate::company::steer::SteerControl,
            chat: crate::runtime::delegation::ChatTarget<'_>,
            _: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
        ) -> Result<crate::harness::built_in::TurnOutcome> {
            self.run(c, a, m, chat).await
        }
        async fn run_steered_background(
            &self,
            c: &CompanyId,
            a: &str,
            m: &str,
            _: &crate::company::steer::SteerControl,
            _: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
        ) -> Result<crate::harness::built_in::TurnOutcome> {
            self.run(c, a, m, crate::runtime::delegation::ChatTarget::default())
                .await
        }
    }

    /// A roster spanning two declared harnesses.
    fn two_harness_record() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "researcher"
role = "Researcher"
harness = "deep"

[[harness]]
id = "embedded"
kind = "built_in"
default = true

[[harness]]
id = "deep"
kind = "built_in"
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            manifest,
            ..record()
        }
    }

    /// The wiring's whole point: an agent bound to a named harness runs on that
    /// harness's engine, and an unbound one stays on the default.
    #[tokio::test]
    async fn a_bound_agent_runs_on_its_own_lane() {
        let dir = tempfile::tempdir().unwrap();
        let deep = Arc::new(SpyLane {
            label: "deep".to_string(),
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let brain = brain_over_mock_with(dir.path(), two_harness_record()).with_lanes(vec![(
            "deep".to_string(),
            deep.clone() as Arc<dyn crate::runtime::delegation::RunTurn>,
        )]);

        let company = CompanyId::new("acme");
        let out = brain
            .run_turn()
            .run(
                &company,
                "researcher",
                "hi",
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("routes to the deep lane");
        assert_eq!(out.reply, "deep");
        assert_eq!(&*deep.seen.lock().unwrap(), &["researcher".to_string()]);

        // The unbound agent must not reach it — it belongs to the default pool.
        let _ = brain
            .run_turn()
            .run(
                &company,
                "ceo",
                "hi",
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await;
        assert_eq!(
            &*deep.seen.lock().unwrap(),
            &["researcher".to_string()],
            "the default agent must not land on the named lane"
        );
    }

    /// The named lane is a **real** pool, not a spy: it must build its own
    /// roster at boot, or a bound agent's first turn fails `CompanyNotFound`
    /// on an empty pool. This is the path the `SpyLane` coverage above cannot
    /// reach — a spy forwards every turn, so it would pass whether or not the
    /// lane's pool was ever warmed.
    #[tokio::test]
    async fn a_named_lane_builds_its_roster_at_boot() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock_with(dir.path(), two_harness_record());
        // The lane `lanes::build` produces: its own pool over deps narrowed to
        // the agents it serves.
        let mut deep_deps = (*brain.deps).clone();
        deep_deps.serves = Some(std::collections::HashSet::from(["researcher".to_string()]));
        let deep: Arc<dyn crate::runtime::delegation::RunTurn> = Arc::new(HarnessRunTurn::new(
            Arc::new(HarnessPool::new()),
            Arc::new(deep_deps),
        ));
        let brain = brain.with_lanes(vec![("deep".to_string(), deep.clone())]);

        // Boot warm-up: the router warms every lane's engine, each against its
        // own narrowed deps.
        brain
            .run_turn()
            .ensure(&brain.record())
            .await
            .expect("every lane's roster builds");

        // A bound agent's turn now reaches its lane's engine instead of dying
        // with "company not found".
        let out = brain
            .run_turn()
            .run(
                &CompanyId::new("acme"),
                "researcher",
                "hi",
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect("the deep lane's roster is built at boot");
        assert!(out.reply.contains("hi"), "{}", out.reply);
    }

    /// A declared harness this host cannot run fails the turn, naming the
    /// harness and the reason. It must never quietly borrow the default lane:
    /// that turn would succeed on a model and a credential nobody chose, and
    /// the only evidence would be a billing line.
    #[tokio::test]
    async fn an_unrunnable_harness_fails_rather_than_falling_back() {
        let dir = tempfile::tempdir().unwrap();
        let brain =
            brain_over_mock_with(dir.path(), two_harness_record()).with_unavailable_lanes(vec![(
                "deep".to_string(),
                "this build has no ACP transport wired".to_string(),
            )]);

        let err = brain
            .run_turn()
            .run(
                &CompanyId::new("acme"),
                "researcher",
                "hi",
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect_err("must not fall back to the default lane");
        let msg = err.to_string();
        assert!(msg.contains("researcher"), "{msg}");
        assert!(msg.contains("deep"), "{msg}");
        assert!(msg.contains("ACP transport"), "names the fix: {msg}");
    }

    /// A company declaring no `[[harness]]` keeps exactly the single-lane path:
    /// no lanes, no bindings, nothing to consult.
    #[tokio::test]
    async fn a_company_with_no_harness_block_is_unrouted() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        assert!(brain.lanes.is_empty());
        assert!(brain.unavailable.is_empty());
        assert!(brain.bindings.is_empty());
        assert_eq!(brain.default_harness, "default");
    }

    /// Issue #1244: a company whose *only* declared harness is `kind = "acp"`
    /// must not silently run turns on the embedded engine.
    ///
    /// Before the fix, `lanes::build`'s early return for a single declared
    /// harness meant nobody ever asked what *kind* that lone harness was — the
    /// caller (here, and identically in `RuntimeBuilder`) unconditionally built
    /// a `HarnessRunTurn` from the shared pool regardless. This exercises the
    /// real `lanes::build` output rather than a hand-simulated one, so a
    /// regression in either `lanes::build` or `HarnessBrain::run_turn` fails it.
    #[tokio::test]
    async fn a_lone_acp_default_harness_does_not_silently_run_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let manifest: CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[harness]]
id = "laptop"
kind = "acp"
default = true

[harness.acp]
transport = "local"
agent = "claude"
"#,
        )
        .expect("valid manifest");
        let record = CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
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
        };

        let brain = brain_over_mock_with(dir.path(), record);
        let secrets: Arc<dyn crate::ports::SecretStore> =
            Arc::new(crate::store::FsSecretStore::new(dir.path()));
        let lanes = crate::harness::lanes::build(
            &brain.record(),
            Arc::new(HarnessPool::new()),
            &brain.deps,
            secrets,
            None,
            None,
        );

        assert!(
            lanes.default_engine.is_none(),
            "an acp default harness has no built-in engine to fall back to"
        );
        assert!(
            lanes.unavailable.iter().any(|(id, _)| id == "laptop"),
            "the default harness's own id must carry the unavailable reason: {:?}",
            lanes.unavailable
        );

        // Wire the brain exactly as `RuntimeBuilder` would, and confirm the
        // turn actually fails instead of quietly answering from the embedded
        // `MockProvider`.
        let brain = brain
            .with_lanes(lanes.lanes)
            .with_unavailable_lanes(lanes.unavailable)
            .with_default_engine(lanes.default_engine);

        let err = brain
            .run_turn()
            .run(
                &CompanyId::new("acme"),
                "ceo",
                "hi",
                crate::runtime::delegation::ChatTarget::default(),
            )
            .await
            .expect_err("must not fall back to the embedded engine");
        let msg = err.to_string();
        assert!(msg.contains("ceo"), "{msg}");
        assert!(msg.contains("laptop"), "{msg}");
        assert!(msg.contains("ACP transport"), "names the fix: {msg}");
    }

    /// Issue #966: a workflow-copilot reply is authored by the copilot.
    ///
    /// This is the assertion the #885 fix was missing on this branch. The bubble
    /// is emitted on the **operator** channel, and before this the author field
    /// was left `None` — so the journal writer's `channel` fallback stamped
    /// `agent_id: "operator"` on a reply an agent had genuinely produced.
    ///
    /// Asserts the author is *not* the channel, rather than only that it equals
    /// the constant: the defect's whole shape is the two being conflated, and a
    /// test that checked equality alone would still pass if `CONFINED_AGENT_ID`
    /// were ever redefined to `"operator"`.
    #[test]
    fn a_copilot_turn_is_authored_by_the_copilot_not_the_operator_channel() {
        let bubble = confined_bubble(crate::harness::TurnOutcome {
            reply: "here is what that node does".to_string(),
            steps: Vec::new(),
            hit_iteration_cap: false,
            // Test fixture, not the ACP fold (PR #1880 review).
            abnormal_stop: None,
            halted_for_spend: None,
            budget_paused: None,
        });
        assert_eq!(bubble.channel, "operator", "the destination is unchanged");
        assert_eq!(
            bubble.agent.as_deref(),
            Some(crate::ports::CONFINED_AGENT_ID)
        );
        assert_ne!(
            bubble.agent.as_deref(),
            Some(bubble.channel.as_str()),
            "author and destination must not be the same value — that conflation is issue #885"
        );
    }

    /// Issue #1846 review (Codex #3869277640) — **the regression.** A budget
    /// pause from a confined workflow-copilot turn must NOT read as the
    /// copilot's own answer.
    ///
    /// Before this fix, `confined_bubble` folded `outcome.reply` — the
    /// budget-paused placeholder text, per `classify_turn`'s
    /// `AttemptOutcome::BudgetPaused` handling — straight into an ordinary
    /// bubble authored by `CONFINED_AGENT_ID`, exactly the #885/#966
    /// author-vs-channel conflation this file exists to prevent, just for a
    /// pause instead of an authored reply. `confined_turn_bubble` is the
    /// fixed boundary: this asserts it routes a paused outcome to
    /// `system_notice` (unauthored, `SYSTEM_AUTHOR`) instead.
    #[test]
    fn a_confined_turns_budget_pause_is_a_system_notice_not_a_copilot_reply() {
        let outcome = crate::harness::TurnOutcome {
            // What `classify_turn`'s `AttemptOutcome::BudgetPaused` arm
            // actually leaves in `reply` — irrelevant to the notice text,
            // which is built fresh from `budget_paused` below, but present
            // here so this fixture matches what `run_confined` really
            // returns rather than an idealised one.
            reply: "Paused — copilot's turn ran out of inference budget/credits.".to_string(),
            steps: Vec::new(),
            hit_iteration_cap: false,
            abnormal_stop: None,
            halted_for_spend: None,
            budget_paused: Some(crate::harness::BudgetPause {
                agent: confine::CONFINED_AGENT_ID.to_string(),
                summary: "Add credits to your account, then resend your message.".to_string(),
            }),
        };

        let bubble = confined_turn_bubble(outcome);

        assert_eq!(
            bubble.agent.as_deref(),
            Some(crate::ports::SYSTEM_AUTHOR),
            "a budget pause is never something the copilot said — it must be unauthored, not \
             attributed to CONFINED_AGENT_ID like an ordinary reply: {:?}",
            bubble.agent
        );
        assert_ne!(
            bubble.agent.as_deref(),
            Some(confine::CONFINED_AGENT_ID),
            "the pre-fix defect: falling through to confined_bubble would attribute the pause \
             notice to the copilot itself"
        );
        // Issue #1846 review (Codex #3870562586): the NO-RESEND prefix. This
        // assertion used to require `BUDGET_PAUSE_NOTICE_PREFIX`, which is
        // precisely what the console keys its "Add credits & resend" button
        // off — and `run_confined` never parks a marker, so that button could
        // only ever 404. Asserting the negative too: the whole defect is the
        // two prefixes being conflated, and a test that only checked the new
        // one would still pass if the redeemable prefix were ever made a
        // prefix of it.
        assert!(
            bubble
                .text
                .starts_with(BUDGET_PAUSE_NOTICE_NO_RESEND_PREFIX),
            "a confined pause parks no marker, so it must carry the non-redeemable prefix: {}",
            bubble.text
        );
        assert!(
            !bubble.text.starts_with(BUDGET_PAUSE_NOTICE_PREFIX),
            "the pre-fix defect: this prefix renders an Add-Credits CTA whose GET returns null \
             and whose POST 404s, because CONFINED_AGENT_ID never has a marker: {}",
            bubble.text
        );
        assert!(
            bubble.text.to_ascii_lowercase().contains("add credits"),
            "the actionable ask survives into the notice: {}",
            bubble.text
        );
    }

    /// Issue #1846 review (Codex #3870562590) — **the regression.** An approval
    /// continuation that pauses for credits must not advertise a redeem the
    /// server refuses.
    ///
    /// The continuation runs through `run_steered_background`, so `run_inner`
    /// parks its marker with `background: true`, and `redeem_budget_pause`
    /// rejects exactly that shape with a 400 (`src/server/ops/budget_pause.rs`).
    /// Emitting `BUDGET_PAUSE_NOTICE_PREFIX` therefore put a button on screen
    /// that reserved the marker, restored it, and failed — every single click.
    ///
    /// Issue #1906: this pins the notice BUILDER only, and its name now says
    /// so. It calls `budget_pause_notice_no_resend` directly and asserts the
    /// result starts with the constant that function formats with — a
    /// tautology over `format!`. Revert the continuation arm at
    /// `run_steered_background`'s tail to `budget_pause_notice` and this test
    /// still passes, so the name it used to carry — "an approval continuation
    /// pause offers no redeem CTA" — promised coverage it does not provide.
    /// That coverage is real and lives in
    /// `a_budget_paused_approval_continuation_surfaces_the_notice_and_parks_a_marker`,
    /// which drives the continuation and reads the bubble it emits. Kept under
    /// the honest name anyway: it is the cheap guard on the builder itself,
    /// which is what the console branches on.
    #[test]
    fn the_no_resend_notice_builder_uses_the_non_redeemable_prefix() {
        let pause = crate::harness::BudgetPause {
            agent: "maya".to_string(),
            summary: "Add credits to your account, then start this again.".to_string(),
        };

        let notice = budget_pause_notice_no_resend(&pause);

        assert!(
            notice.starts_with(BUDGET_PAUSE_NOTICE_NO_RESEND_PREFIX),
            "{notice}"
        );
        assert!(
            !notice.starts_with(BUDGET_PAUSE_NOTICE_PREFIX),
            "the pre-fix defect: a background-parked marker is refused by the redeem route, so \
             this prefix's CTA can only ever return 400: {notice}"
        );
        assert!(
            notice.to_ascii_lowercase().contains("add credits"),
            "the operator still has to be told the lever: {notice}"
        );
        assert!(
            notice.contains(&pause.summary),
            "the provider's own summary survives into the notice: {notice}"
        );
    }

    /// The two prefixes must stay genuinely distinct: the console decides
    /// whether to render an actionable button by `startsWith`, so if the
    /// redeemable prefix were ever edited to become a prefix of the
    /// non-redeemable one, every no-resend notice would silently regain the
    /// broken CTA. Cheap coupling test, mirrored on the frontend by
    /// `budget-pause-notice.test.ts`'s "does not match the NO-RESEND sibling
    /// prefix" fixture — which asserts the negative against the real
    /// `BUDGET_PAUSE_NOTICE_NO_RESEND_PREFIX` string rather than an invented
    /// near-miss (issue #1906: the claim was made here before that fixture
    /// existed).
    #[test]
    fn the_redeemable_and_no_resend_prefixes_are_not_prefixes_of_each_other() {
        assert!(
            !BUDGET_PAUSE_NOTICE_NO_RESEND_PREFIX.starts_with(BUDGET_PAUSE_NOTICE_PREFIX),
            "a no-resend notice would match `isBudgetPauseNotice` and regain the CTA"
        );
        assert!(
            !BUDGET_PAUSE_NOTICE_PREFIX.starts_with(BUDGET_PAUSE_NOTICE_NO_RESEND_PREFIX),
            "a redeemable notice would stop matching and lose its working CTA"
        );
    }

    /// Issue #966: a bubble the runtime wrote names a non-agent author.
    ///
    /// Asserts it is not `"operator"` specifically, rather than only that it
    /// equals the constant. The whole defect is that a host-authored notice and
    /// a reply whose author was overwritten stored the *same* value, so a test
    /// that checked equality alone would still pass if `SYSTEM_AUTHOR` were ever
    /// redefined to the channel name.
    #[test]
    fn a_host_authored_notice_is_not_authored_by_the_operator_channel() {
        let bubble = system_notice("Acknowledged.".to_string());
        assert_eq!(bubble.channel, "operator", "the destination is unchanged");
        assert_eq!(bubble.agent.as_deref(), Some(crate::ports::SYSTEM_AUTHOR));
        assert_ne!(
            bubble.agent.as_deref(),
            Some("operator"),
            "a notice must not store the author a destination-overwrite produces"
        );
    }

    // ── Issue #1861: blockers park instead of settling Failed ───────────────

    /// The acceptance case: a dispatch that died on a model id the provider
    /// rejects is answerable — somebody can set a real one — so it parks and
    /// the card lands `paused` carrying the question, instead of dropping back
    /// into To-do indistinguishable from work nobody started.
    #[tokio::test]
    async fn a_rejected_model_id_parks_a_blocker_rather_than_settling_failed() {
        use crate::harness::policy::ApprovalRequestQueue;
        use crate::ports::blockers::{BlockerKind, BlockerPayload, BlockerSource, BlockerStep};

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());

        let reason = "dispatch failed: the model `gpt-nonexistent` does not exist or you do not \
                      have access to it";
        let end = brain.settle_as_blocker_or_failure("t-1", reason, Some("run-1"));

        assert_eq!(end, TaskRunEnd::Blocked);
        assert_eq!(
            lifecycle::landing_column(end),
            crate::ports::tasks::COLUMN_PAUSED,
            "a card with an open question on it has not failed — it is waiting"
        );

        let drained = requests.drain(8);
        assert_eq!(drained.requests.len(), 1, "exactly one question is asked");
        let effect = &drained.requests[0].effect;
        assert_eq!(effect.kind, "blocker.infrastructure");
        assert_eq!(effect.run_id.as_deref(), Some("run-1"));

        let payload: BlockerPayload =
            serde_json::from_value(effect.payload.clone()).expect("the payload round-trips");
        assert_eq!(payload.kind, BlockerKind::Infrastructure);
        assert_eq!(payload.source, BlockerSource::Provider);
        assert_eq!(
            payload.step,
            Some(BlockerStep::Task {
                task_id: "t-1".to_string()
            })
        );
        assert!(
            !payload.needed.trim().is_empty(),
            "a question that does not say what would answer it wastes the asking"
        );
    }

    /// The conservative default, pinned: a failure the classifier does not
    /// recognise keeps today's behaviour exactly and asks nobody. Being wrong
    /// in this direction costs a `Failed` that #1865 already surfaces; being
    /// wrong the other way spends an operator's attention on a question they
    /// cannot answer.
    #[tokio::test]
    async fn an_unrecognised_failure_still_fails_and_asks_nobody() {
        use crate::harness::policy::ApprovalRequestQueue;

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());

        let end =
            brain.settle_as_blocker_or_failure("t-1", "dispatch failed: index out of bounds", None);

        assert_eq!(end, TaskRunEnd::Failed);
        assert_eq!(
            lifecycle::landing_column(end),
            crate::ports::tasks::COLUMN_TODO
        );
        assert!(
            requests.drain(8).requests.is_empty(),
            "an unrecognised failure must not reach the operator as a question"
        );
    }

    /// Recognising a transient stop is how we know **not** to ask: a rate limit
    /// resolves itself, so it settles like any other failure and nothing is
    /// parked.
    #[tokio::test]
    async fn a_rate_limit_settles_without_asking_anybody() {
        use crate::harness::policy::ApprovalRequestQueue;

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());

        let end = brain.settle_as_blocker_or_failure(
            "t-1",
            "dispatch failed: hosted inference returned 429: rate limit exceeded",
            None,
        );

        assert_eq!(end, TaskRunEnd::Failed);
        assert!(requests.drain(8).requests.is_empty());
    }

    /// Approving a blocker must do **nothing** in this issue — the answer is
    /// carried back into the stopped turn by #1863, and until then an approve
    /// that half-executed something would be worse than one that does not.
    ///
    /// `perform_effect` acts on three things: an `amount_usd` (writes a ledger
    /// entry), a `channel`+`text` pair in the payload (sends a message), and
    /// the email kind. This pins that a blocker effect carries none of them, so
    /// the no-op is a property of the shape rather than a coincidence somebody
    /// could break by adding a field.
    #[tokio::test]
    async fn a_parked_blocker_carries_nothing_an_executor_would_act_on() {
        use crate::harness::policy::ApprovalRequestQueue;

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());

        brain.settle_as_blocker_or_failure(
            "t-1",
            "tool call failed: could not connect to mcp server `slack`",
            None,
        );

        let drained = requests.drain(8);
        let effect = &drained.requests[0].effect;
        assert!(effect.amount_usd.is_none(), "a question costs nothing");
        assert!(
            effect.payload.get("channel").is_none() && effect.payload.get("text").is_none(),
            "a `channel`+`text` payload would make approving a blocker post a message"
        );
        assert!(
            effect.agent.is_none(),
            "stamping an agent would mint a grant and re-dispatch the turn, which would \
             call the escalation again and park a second time"
        );
    }
}
