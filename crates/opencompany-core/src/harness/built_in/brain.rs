//! [`HarnessBrain`]: the cognition [`Brain`] backed by the embedded OpenHuman
//! runtime.
//!
//! Where [`EchoBrain`](crate::brain::EchoBrain) turns every operator message
//! into `"You said: …"`, `HarnessBrain` routes it to a live openhuman
//! [`Agent`](openhuman_core::agent::Agent) through a
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
use crate::runtime::delegation::{self, ChatTarget, DelegationRunner, RunTurn};

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
///
/// # Who it names, and who it is *from*
///
/// It **names the responder**, exactly as [`spend_halt_notice`] names the
/// teammate whose budget ran out. That sibling's docs say naming the teammate
/// is what makes a stop "attributable in a way a bare cap is not", and the same
/// applies here: "this turn" told the operator a turn had capped without
/// saying whose, which on a delegating orchestrator is the one thing they need
/// to know. The objection recorded there is to quoting a *number* — one bubble
/// can cover a responder, a desk and a relay turn, so a single cap figure maps
/// back to nothing — and that objection is about the figure, not the name.
///
/// It is **from the system**, not from the agent it names. On the desk path
/// below this already carried [`SYSTEM_AUTHOR`](crate::ports::SYSTEM_AUTHOR),
/// because a journalled notice must have an author; the operator path passed
/// `agent: None` on the reasoning that no teammate said this and attributing it
/// to the responder would put the platform's words in their mouth. That
/// reasoning is right and the `None` still defeated it: an authorless reply
/// journals as `agent_id: "operator"`, which is not a roster member, so the
/// console fell through to the channel's own voice — the orchestrator's name —
/// and rendered the platform's words under **"Product Manager"** with the
/// company mark beside them. Naming the system author is what actually
/// delivers the intent: `chat.ts` maps it to `from: "system"`, which
/// `senderOf` renders as "System" and which the timeline's promotion guard
/// already refuses to lift into the channel.
pub(crate) fn iteration_cap_pause_notice(agent: &str) -> String {
    format!(
        "The reply above is a pause, not a finished answer: {agent}'s turn reached the maximum \
         number of steps it may take for a single reply, so it stopped and wrote up where it had \
         got to. Nothing errored — the work so far stands. Reply \"continue\" to ask it to pick up \
         from there."
    )
}

/// The system bubble emitted when a turn was halted by its in-turn spend brake
/// (issue #1032).
///
/// The sibling of [`iteration_cap_pause_notice`], and deliberately **not**
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
/// [`iteration_cap_pause_notice`] and [`spend_halt_notice`], and, like both,
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
use crate::ports::context::ContextStore;
use crate::ports::runs::{RunOutcome, RunStatus};
use crate::ports::tasks::{COLUMN_IN_REVIEW, TaskOutput, TaskOutputArtifact, TaskOutputSource};
use crate::ports::types::{
    CompanyEvent, CompanyId, CompanyRecord, CompressedTrace, ContextChunk, CycleRequest,
    CycleResult, Effect, EffectGroup, EventSeq, OutboundMessage, TokenUsage, TurnStep,
    TurnStepKind, TurnStepStatus, Verdict,
};
use crate::ports::{Cognition, TaskOrigin, TaskRecord, UsageMetering, generate_id, now_millis};

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
    /// The card-titling pass, built on first use. Lazy and `OnceLock` for
    /// exactly [`Self::triage`]'s reasons — it needs the company id, and once
    /// built it is immutable.
    titler: std::sync::OnceLock<crate::harness::title::MeteredTitler>,
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
        outputs: Vec::new(),
        channel: "operator".to_string(),
        agent: Some(crate::ports::SYSTEM_AUTHOR.to_string()),
        text,
        steps: Vec::new(),
        reply_to: None,
        mentions: Vec::new(),
    }
}

/// Who a dispatch relay should be authored by when it answers in the `origin`
/// thread.
///
/// A card whose origin is a teammate's **private DM** must be answered by that
/// teammate, never by the orchestrator — an orchestrator bubble in a private DM
/// intrudes a second voice into a one-to-one thread. The origin key is resolved
/// against the roster the same way an assignee is (the `dm:` prefix unwrapped as
/// a fallback), and only a resolved [`AssigneeResolution::Agent`] that is not the
/// orchestrator claims the voice. A desk, the General line, an empty/unknown key
/// — every shared surface — keeps the orchestrator as the single point of
/// contact.
///
/// The unwrapped `dm:` fallback checks the roster **by id first**, mirroring
/// [`chat_responder`](crate::runtime::delegation_tools::chat_responder)'s own
/// `dm:` arm (issue #1743): a desk whose id collides with a teammate's must
/// not swallow the prefixed address, because the prefix exists precisely to
/// reach that teammate. Falling straight into the bare, desk-first
/// [`assignee::resolve`] here would reopen #1743 in this resolver — a card
/// dispatched from that teammate's private DM would misattribute to the
/// orchestrator, exactly the "second voice" this function exists to prevent.
pub(crate) fn relay_speaker(record: &CompanyRecord, origin: &str, orchestrator: &str) -> String {
    let mut resolution = assignee::resolve(record, origin);
    if matches!(resolution, assignee::AssigneeResolution::Unknown(_))
        && let Some(key) = assignee::dm_key(origin)
    {
        resolution = match record.resolve_roster_agent_id(key) {
            Some(agent) => assignee::AssigneeResolution::Agent(agent),
            None => assignee::resolve(record, key),
        };
    }
    match resolution {
        assignee::AssigneeResolution::Agent(agent) if agent != orchestrator => agent,
        _ => orchestrator.to_string(),
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
        outputs: Vec::new(),
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
            titler: std::sync::OnceLock::new(),
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
        let output_claim = self.deps.pending_publishes.output_collector().claim();
        // Un-streamed, like a dispatched card: this turn is answered by the
        // bubble returned below, and its transient frames would otherwise
        // misattribute onto whichever chat thread the console is watching.
        let outcome = output_claim
            .scoped(run_turn.run_steered_background(
                &self.record().id,
                &grant.agent,
                &instruction,
                &control,
                // Issue #1890 I. Un-streamed, but **not** unaddressed: this
                // call was raised in a conversation, and the grant has recorded
                // which one — channel and thread — since #435. Without it the
                // re-issued call bound to nothing, so it ran against whatever
                // history the agent happened to be holding and then published
                // its answer into the origin thread regardless. The same pair
                // the delegation drain below is bound to.
                ChatTarget::in_thread(grant.origin_thread.as_deref(), grant.origin_parent),
                None,
            ))
            .await;
        drop(guard);
        let published = self.deps.pending_publishes.drain();
        if !published.is_empty()
            && publish_claim.is_some()
            && let Err(err) = output_claim
                .scoped(self.record_conversation_publishes(
                    &grant.agent,
                    // Both halves of the conversation the approval was raised
                    // in (#1890), the same pair the delegation drain below is
                    // bound to — a grant has recorded `origin_parent` beside
                    // `origin_thread` since A, and this site was reading only
                    // one of them.
                    ChatTarget::in_thread(grant.origin_thread.as_deref(), grant.origin_parent),
                    published,
                ))
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
        let drained = match output_claim
            .scoped(
                self.delegation_runner(run_turn.as_ref(), &record)
                    // The conversation the approval was raised in (#1890) — both halves
                    // of it. A grant records `origin_parent` beside `origin_thread` for
                    // exactly this reason, so a threaded approval resumes in its own
                    // thread rather than against the channel's unparented history.
                    .in_thread(grant.origin_parent)
                    .drain_and_execute(
                        grant.origin_thread.as_deref(),
                        delegation::MessageContext::default(),
                        delegation::HandOffs::Run,
                    ),
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
        let outputs = output_claim.drain();

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
            outputs,
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
        // Every id `responder` held before a reassignment overwrote it — a
        // hand-off's `[<old responder>] delegated to …` block stays on the
        // note under that old name, so `relay_text`'s `known_labels` needs it
        // too or the strip leaves that block's chrome in the relayed bubble
        // (issue #1949 review, CodeRabbit 3895599021).
        let mut prior_responders: Vec<String> = Vec::new();

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
                title: card.title.to_string(),
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
        // Issue #2150: this dispatch's trust window. Captured once, against
        // the responder frozen above — a redirect re-runs the same claim, and
        // a hand-off inside `handle_task_delegations` below inherits it
        // unchanged rather than re-deriving one for the delegate (see
        // `crate::harness::built_in::run_origin`).
        let dispatch_origin = crate::harness::built_in::run_origin::claim(
            crate::harness::built_in::run_origin::RunOrigin::Dispatched {
                agent: responder.clone(),
                source: crate::harness::built_in::run_origin::DispatchSource::Task,
                scope: None,
            },
        );
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
            let outcome = dispatch_origin
                .scoped(Box::pin(
                    run_turn
                        // A dispatched task card carries no chat bubble (its steps
                        // are discarded into the note), so its live turn frames
                        // must not leak onto the console timeline — run it
                        // un-streamed (#125 review).
                        .run_steered_background(
                            &self.record().id,
                            &responder,
                            &instruction,
                            &control,
                            // No conversation to bind to: a dispatched card's turn
                            // answers the board, not a thread (#1890 I). Unchanged
                            // behaviour — including that it does not clear
                            // history, since one task can span several turns.
                            ChatTarget::default(),
                            // Issue #242: un-streamed does not mean unrecorded. The
                            // trace this turn produces is written to the attempt
                            // row as it happens, which is what a redirect re-run
                            // appends to rather than restarting.
                            sink.clone(),
                        ),
                ))
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
                                // SPIKE: handed over, delegate not yet run.
                                // Settles `Delegated` — which
                                // `settled_landing_column` keeps in
                                // `in_progress` precisely because a hand-off is
                                // "not an ending" — and the runtime re-fires
                                // dispatch for the card's new owner.
                                Some(handoff) if handoff.pending => {
                                    let delegate = handoff.delegate.clone();
                                    // The DELEGATOR is who handed it over, so
                                    // the note is theirs. Reading `responder`
                                    // after the swap credits the delegate with
                                    // handing work to itself.
                                    let delegator =
                                        std::mem::replace(&mut responder, handoff.delegate);
                                    let result =
                                        format!("handed off to {delegate}; awaiting their run");
                                    settle(&mut card, TaskRunEnd::Delegated, &delegator, &result);
                                    prior_responders.push(delegator);
                                    (TaskRunEnd::Delegated, result)
                                }
                                Some(handoff) => {
                                    prior_responders
                                        .push(std::mem::replace(&mut responder, handoff.delegate));
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
                        lifecycle::OPERATOR_REDIRECT_ATTRIBUTION,
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
                    Some(
                        crate::harness::built_in::run_origin::RunOrigin::Dispatched {
                            agent: responder.to_string(),
                            source: crate::harness::built_in::run_origin::DispatchSource::Task,
                            scope: None,
                        },
                    ),
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
        // authoritative overwrite now that the parked count is known.
        //
        // SPIKE: through `settled_landing_column`, NOT `column_for_settled_run`
        // directly. The two agree on every ending except a hand-off, which the
        // former keeps in `in_progress` ("the work has changed hands, not
        // stopped") while the latter maps its `Paused` run status to the
        // `paused` column. The old comment here — "a hand-off never breaks the
        // loop" — was what made the difference unobservable; now that a
        // hand-off settles, the card was landing in `paused` and the delegate
        // was never dispatched.
        {
            let column = lifecycle::settled_landing_column(run_end, parked);
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
        let Some(origin) = card.origin_chat_id().map(str::to_string) else {
            return Ok(None);
        };
        let orchestrator = self.orchestrator();
        let speaker = relay_speaker(&self.record(), &origin, &orchestrator);
        let prior: Vec<&str> = prior_responders.iter().map(String::as_str).collect();
        let relay = lifecycle::relay_reply(&card, &responder, &speaker, origin, &prior);
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

        let Some(origin) = card.origin_chat_id().map(str::to_string) else {
            // A refusal has no relay, but its terminal outcome still belongs
            // on the task timeline.
            self.journal_task_outcome(&card, &orchestrator, text, Vec::new())
                .await;
            return Ok(None);
        };
        let speaker = relay_speaker(&self.record(), &origin, &orchestrator);
        // `speaker` goes in both slots: a refusal never claims anyone ran the
        // card, so `relay_text`'s `responder == orchestrator` check must
        // always hold here, regardless of who the relay speaks as (issue
        // #1949 review, CodeRabbit thread 3895107568). Passing `orchestrator`
        // in the responder slot instead used to fire the "ran it" credit
        // whenever `speaker` diverged from it — i.e. every private DM.
        // A refusal never ran a turn, so there is no reassignment history to
        // carry into the strip.
        let relay = lifecycle::relay_reply(&card, &speaker, &speaker, origin, &[]);
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
        dispatched: Option<crate::harness::built_in::run_origin::RunOrigin>,
    ) -> Option<String> {
        let instruction = publish::nudge_instruction(brief, reply, unpublished, scan_partial);
        let company = self.record().id.clone();
        let turn = Box::pin(run_turn.run_steered_background(
            &company,
            responder,
            &instruction,
            control,
            // A hand-off inside a dispatched card: the board is the
            // conversation, not a thread (#1890 I).
            ChatTarget::default(),
            sink,
        ));
        // Only the caller knows whether this nudge belongs to a dispatched
        // card. The cycle runs the operator's own turn, its delegated desk
        // turns, a dispatched card and a re-dispatch after an approval through
        // one path, so minting a `Dispatched` origin here would hand a turn
        // that followed an operator's message the trust a card earned.
        let outcome = match dispatched {
            Some(origin) => {
                crate::harness::built_in::run_origin::claim(origin)
                    .scoped(turn)
                    .await
            }
            None => turn.await,
        };
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
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    chat_id: card.id.clone(),
                    agent_id: responder.to_string(),
                    text: result_text.clone(),
                    steps: Vec::new(),
                    task_id: Some(card.id.clone()),
                    outputs: Vec::new(),
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
                    origin_chat_id: card.origin_chat_id().map(str::to_string),
                    // Issue #1890 B: the thread inside that channel, captured
                    // off the card on exactly the terms above. This is the
                    // whole of what B repairs — without it the terminal names a
                    // channel and no thread, so a card raised inside a thread
                    // settled flat in the channel and the thread that asked for
                    // the work never showed it finishing.
                    origin_parent: card.origin_parent(),
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
            self.deps.pending_publishes.output_collector().artifact(
                record.id.clone(),
                card.id.clone(),
                version,
                &record.title,
            );
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
        chat: ChatTarget<'_>,
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
                self.file_publishes_on_card(card_id, &publisher, chat, published)
                    .await
            }
            None => {
                self.record_conversation_publishes(&publisher, chat, published)
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
    /// `chat` is carried into that fallback so a minted replacement points
    /// back at the same conversation the no-card-in-scope path's card does;
    /// two minting paths must not differ in where their card posts back. One
    /// [`ChatTarget`] rather than a channel and a root side by side (#1890 B):
    /// the pair travels four frames down this chain, and two bare `Option`s
    /// beside each other is the mis-pairing hazard that type exists to remove.
    async fn file_publishes_on_card(
        &self,
        card_id: &str,
        agent: &str,
        chat: ChatTarget<'_>,
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
                .record_conversation_publishes(agent, chat, published)
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
        chat: ChatTarget<'_>,
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
            title: crate::ports::tasks::TaskTitle::system(&publish::conversation_card_title(
                &published,
            )),
            note: Some(publish::conversation_card_note(responder, &published)),
            // Finished agent work a person has not accepted yet — the same
            // landing `column_for_settled_run(Succeeded)` gives a dispatched run.
            column: COLUMN_IN_REVIEW.to_string(),
            priority: "medium".to_string(),
            assignee: responder.to_string(),
            updated_at_millis: now_millis(),
            // The conversation this came out of, so the card points back at the
            // thread that produced it (#151 §3.2's field, same meaning).
            // Issue #1890 B: and the thread inside it, so a file published
            // inside a thread leaves its card pointing at that thread rather
            // than at the channel around it. `None` for the thread is the
            // channel-level conversation, which is where every publish landed
            // before threads were part of the key.
            origin: TaskOrigin::new(chat.chat_id.map(str::to_string), chat.thread_root),
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
            origin_message_seq: None,
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
                crate::harness::built_in::blockers::connection_group_key(reason),
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
        group_key: Option<String>,
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
            group_key,
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
        .with_titler(self.title_pass(&record.id))
    }

    /// The company's card-titling pass, built once.
    fn title_pass(
        &self,
        company: &crate::ports::types::CompanyId,
    ) -> &crate::harness::title::MeteredTitler {
        self.titler.get_or_init(|| {
            crate::harness::title::MeteredTitler::from_deps(&self.deps, company.clone())
        })
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

    /// SPIKE: who currently oversees the thread rooted at `parent`.
    ///
    /// The last teammate to have replied under that root, read straight off the
    /// journal — so oversight follows the conversation without a field to keep
    /// in sync. `None` for an unparented message (the channel itself has no
    /// overseer), when no event log is wired, or when nobody has spoken yet.
    /// SPIKE: the card this thread already raised, if it raised one.
    ///
    /// One thread is one piece of work. Without this a follow-up inside a
    /// thread opens a SECOND card — "make it shorter" becomes its own task
    /// beside the draft it is about — because the carding decision is made per
    /// message and has no idea the conversation already has a card.
    ///
    /// Scoping the turn to it makes `open_work_card` decline on its existing
    /// `task.is_some()` guard, which is the same gate that stops a dispatched
    /// card's hand-off opening one.
    async fn thread_card(
        &self,
        chat: Option<&str>,
        parent: Option<crate::ports::types::EventSeq>,
    ) -> Option<String> {
        let root = parent?;
        let tasks = self.deps.tasks.as_ref()?;
        let record = self.record();
        tasks
            .list(&record.id)
            .await
            .ok()?
            .into_iter()
            .filter(|card| card.origin_parent() == Some(root))
            .filter(|card| {
                crate::server::chat_history::same_conversation(card.origin_chat_id(), chat)
            })
            .map(|card| card.id)
            .next_back()
    }

    async fn thread_overseer(
        &self,
        chat: Option<&str>,
        parent: Option<crate::ports::types::EventSeq>,
    ) -> Option<String> {
        /// How far back to look for the hand-off. A thread is one level deep
        /// and bounded in practice; past this the fallback answer is better
        /// than an unbounded scan on every message.
        const LOOKBACK: usize = 400;

        let root = parent?;
        let record = self.record();

        // **The card is the ownership record; the thread only reflects it.**
        //
        // Deriving oversight from the conversation alone gives a SECOND owner
        // that drifts from `assignee` — observed: card held by `design`, thread
        // overseen by `qa_engineer`, and a stray hop card under a third agent.
        // Two records for one question is the same class of split the board and
        // the run history already avoid by keeping one settle site.
        //
        // So when this thread raised a card, that card's assignee IS the
        // overseer: a hand-off moves it, a reassignment from the board moves
        // it, and the thread follows without a second rule. The mention scan
        // below is only for a thread with no card behind it — an ordinary chat
        // exchange, where there is nothing else to be authoritative.
        if let Some(tasks) = self.deps.tasks.as_ref()
            && let Ok(cards) = tasks.list(&record.id).await
        {
            let owner = cards
                .iter()
                .filter(|card| card.origin_parent() == Some(root))
                .filter(|card| {
                    crate::server::chat_history::same_conversation(card.origin_chat_id(), chat)
                })
                .filter_map(|card| {
                    crate::runtime::assignee::resolve(&record, &card.assignee)
                        .working_agent()
                        .map(str::to_string)
                })
                .next_back();
            if let Some(owner) = owner {
                return Some(owner);
            }
        }

        let events = self.deps.events.as_ref()?;
        // Backwards from the tail, bounded — NOT `read_from(0, MAX)`, which
        // turns every chat message into a scan of the whole company history.
        let page = events.read_before(&record.id, None, LOOKBACK).await.ok()?;

        let dir = crate::runtime::mentions::directory(&record, &[]);
        let mut last_speaker: Option<String> = None;

        for stored in page {
            // Nothing at or before the root can carry this thread's hand-off.
            if stored.seq <= root {
                break;
            }
            let crate::ports::types::CompanyEvent::AgentReply {
                parent: reply_parent,
                agent_id,
                chat_id,
                text,
                ..
            } = &stored.event
            else {
                continue;
            };
            if *reply_parent != Some(root)
                || !crate::server::chat_history::same_conversation(Some(chat_id), chat)
            {
                continue;
            }
            // **The hand-off, not the last utterance.** Oversight transfers to
            // whoever was HANDED the work, so the overseer is the agent the
            // most recent hand-off NAMED — not whoever spoke most recently.
            //
            // Keying on the last speaker instead is self-reinforcing: one
            // misrouted turn makes that agent the overseer of the thread
            // permanently, because answering is itself what confers oversight.
            // Naming somebody is a deliberate act; speaking is not.
            let named = crate::runtime::mentions::extract_with_known(text, &dir);
            if let Some(handed_to) =
                crate::runtime::mentions::mention_responder(&record, Some(chat_id), &named)
            {
                return Some(handed_to);
            }
            // Newest-first, so the first reply we see is the latest speaker.
            if last_speaker.is_none() && record.is_roster_agent(agent_id) {
                last_speaker = Some(agent_id.clone());
            }
        }
        // Nobody was handed anything: the thread has one participant, and they
        // hold it.
        last_speaker
    }

    /// TinyHiveMind's one-responder decision for an inbound chat message.
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
    async fn tinyhivemind_responder(
        &self,
        chat: Option<&str>,
        text: &str,
        mentions: &[crate::ports::types::Mention],
    ) -> Option<tinyhivemind::responder::ResponderDecision> {
        let (company, members, desks, candidates, request) = {
            let record = self.record();
            let company = record.id.clone();
            let members = crate::runtime::delegation_tools::tinyhivemind_roster(&record);
            let desks = crate::runtime::delegation_tools::tinyhivemind_desks(&record);
            let candidates: Vec<tinyhivemind::responder::SelectorCandidate> = record
                .effective_agents()
                .into_iter()
                .filter_map(|agent| {
                    selector_candidate(&record, &agent.id).map(|candidate| {
                        tinyhivemind::responder::SelectorCandidate {
                            id: candidate.id,
                            label: agent.name.clone().unwrap_or_else(|| agent.role.clone()),
                            role: candidate.role,
                            description: candidate.description,
                        }
                    })
                })
                .collect();
            let request = tinyhivemind::responder::ResponderRequest {
                message: text.to_string(),
                chat: chat.map(str::to_string),
                mentions: mentions.iter().map(tinyhivemind_mention).collect(),
                orchestrator_id: self.responder.clone(),
                selection_policy: tinyhivemind::responder::SelectionPolicy::Allowed,
                // ZERO preserves the behaviour this host had before the field
                // existed: the ladder had no confidence gate, so any selection
                // it made was taken. This host's selector is not a distribution
                // — `MeteredSelector` commits to one id and reports no spread —
                // so a threshold above zero would reject on a number
                // `TinyHiveSelector` synthesises rather than measures, which is
                // a gate on nothing. Raising it is a behaviour change and wants
                // a real distribution behind it first.
                minimum_selection_confidence: tinyhivemind::responder::Probability::ZERO,
            };
            (company, members, desks, candidates, request)
        };
        let people = Vec::new();
        let retired = Vec::new();
        let roster = tinyhivemind_core::roster::Roster::new(&members, &people, &retired);
        let mut request = request;
        if crate::harness::HarnessPool::total_ceiling_spent(&company, &self.deps).await {
            request.selection_policy = tinyhivemind::responder::SelectionPolicy::Disabled;
        }
        // **The semantic rung.**
        //
        // Jev where this instance has a routing credential, `MeteredSelector`
        // where it does not. A replacement rather than a layer, matching the
        // reference runner: one semantic router, no second model selector, the
        // deterministic destination underneath both.
        //
        // What the swap buys is the distribution. `MeteredSelector` commits to
        // one id and reports no spread, so `TinyHiveSelector` had to synthesise
        // `ONE`/`ZERO` — and every rule that reads a distribution is inert
        // against a synthetic one. A Jev evaluation carries the real spread.
        //
        // An `@mention` never reaches either: `choose_responder` resolves it on
        // a rung above this one, without a provider call.
        #[cfg(feature = "typesafe")]
        let jev = match crate::hivemind::broadcast::router_from_env() {
            Ok(router) => router,
            Err(error) => {
                tracing::warn!(
                    company = %company,
                    %error,
                    "[tinyhivemind] the routing credential is present but unusable;                      falling back to the metered selector"
                );
                None
            }
        };
        let metered = TinyHiveSelector(self.selector_pass(&company));
        #[cfg(feature = "typesafe")]
        let jev_selector =
            jev.map(|router| crate::hivemind::broadcast::JevSelector::new(router, None));
        #[cfg(feature = "typesafe")]
        let selector: &dyn tinyhivemind::responder::Selector = match jev_selector.as_ref() {
            Some(selector) => selector,
            None => &metered,
        };
        #[cfg(not(feature = "typesafe"))]
        let selector: &dyn tinyhivemind::responder::Selector = &metered;

        match tinyhivemind::responder::choose_responder(
            Some(selector),
            &request,
            &roster,
            &desks.set(),
            &candidates,
        )
        .await
        {
            Ok(decision) => {
                tracing::info!(
                    company = %company,
                    responder = %decision.responder_id,
                    rung = ?decision.rung,
                    disposition = ?decision.disposition,
                    "[tinyhivemind] routed one inbound message to one responder"
                );
                Some(decision)
            }
            Err(error) => {
                tracing::warn!(
                    company = %company,
                    %error,
                    "[tinyhivemind] responder ladder failed; using the host fallback"
                );
                None
            }
        }
    }

    #[cfg(test)]
    async fn auto_channel_responder(&self, chat: Option<&str>, text: &str) -> Option<String> {
        let chat = chat?;
        let record = self.record();
        let desk = record.resolve_desk_id(chat)?;
        if record.desk_responder_mode(&desk).is_lead() {
            return None;
        }
        drop(record);
        self.tinyhivemind_responder(Some(chat), text, &[])
            .await
            .and_then(|decision| {
                matches!(
                    decision.disposition,
                    tinyhivemind::responder::SelectionDisposition::Selected
                        | tinyhivemind::responder::SelectionDisposition::NotApplicable
                )
                .then_some(decision.responder_id)
            })
    }
}

struct TinyHiveSelector<'a>(&'a crate::harness::selector::MeteredSelector);

impl tinyhivemind::responder::Selector for TinyHiveSelector<'_> {
    fn select<'a>(
        &'a self,
        request: &'a tinyhivemind::responder::SelectionRequest,
    ) -> tinyhivemind::responder::SelectorFuture<'a> {
        Box::pin(async move {
            let candidates = request
                .candidates
                .iter()
                .map(|candidate| crate::harness::selector::SelectorCandidate {
                    id: candidate.id.clone(),
                    role: candidate.role.clone(),
                    description: candidate.description.clone(),
                    tools: Vec::new(),
                })
                .collect::<Vec<_>>();
            match self.0.select(&request.message, &candidates).await {
                // The port asks for a distribution; this host's selector
                // returns one id and nothing else. Rather than invent a spread
                // it did not compute, the chosen candidate takes the whole mass
                // and every other takes none — which is the literal truth about
                // what `MeteredSelector` decided. `confidence` is ONE for the
                // same reason: it is documented as "distribution concentration,
                // not correctness probability", and a one-point distribution is
                // maximally concentrated whatever its odds of being right.
                crate::harness::selector::SelectorVerdict::Member(id) => {
                    Ok(tinyhivemind::responder::SelectionEvaluation {
                        probabilities: request
                            .candidates
                            .iter()
                            .map(|candidate| tinyhivemind::responder::CandidateProbability {
                                probability: if candidate.id == id {
                                    tinyhivemind::responder::Probability::ONE
                                } else {
                                    tinyhivemind::responder::Probability::ZERO
                                },
                                candidate_id: candidate.id.clone(),
                            })
                            .collect(),
                        choice: id,
                        confidence: tinyhivemind::responder::Probability::ONE,
                    })
                }
                crate::harness::selector::SelectorVerdict::Unavailable => {
                    Err("the responder selector was unavailable".into())
                }
            }
        })
    }
}

fn tinyhivemind_mention(
    mention: &crate::ports::types::Mention,
) -> tinyhivemind_core::mention::Mention {
    use crate::ports::types::MentionTarget as HostTarget;
    use tinyhivemind_core::mention::MentionTarget;

    let target = match &mention.target {
        HostTarget::Agent { id } => MentionTarget::Agent { id: id.clone() },
        HostTarget::User { id } => MentionTarget::Person { id: id.clone() },
        HostTarget::Desk { id } => MentionTarget::Desk { id: id.clone() },
        HostTarget::Everyone => MentionTarget::Everyone,
    };
    tinyhivemind_core::mention::Mention {
        target,
        text: mention.text.clone(),
        offset: mention.offset,
        quiet: mention.quiet,
    }
}

/// One channel member as [`HarnessBrain::tinyhivemind_responder`] hands it to
/// the selection: the manifest half through
/// [`CompanyRecord::effective_agent`] (stored edits applied), the overlay half
/// from its row with any stored edit's role/description preferred — the same
/// two halves the Team page folds, so the selector and the members pane
/// describe a teammate identically.
fn selector_candidate(
    record: &CompanyRecord,
    id: &str,
) -> Option<crate::harness::selector::SelectorCandidate> {
    let allow = &record.manifest.tools.allow;
    if let Some(agent) = record.effective_agent(id) {
        return Some(crate::harness::selector::SelectorCandidate {
            id: agent.id.clone(),
            role: agent.role.clone(),
            description: agent.description.clone(),
            tools: crate::runtime::builder::agent_effective_grants(allow, agent.tools.as_deref()),
        });
    }
    let agent = record.overlay_agents.iter().find(|a| a.id == id)?;
    let edit = record.overlay_agent_edits.iter().find(|e| e.agent_id == id);
    // An edit that states `tools` replaces the teammate's own list; one that
    // says nothing leaves it, matching how role and description resolve above.
    let tools = edit
        .and_then(|e| e.tools.clone())
        .unwrap_or_else(|| agent.tools.clone());
    Some(crate::harness::selector::SelectorCandidate {
        id: agent.id.clone(),
        role: edit
            .and_then(|e| e.role.clone())
            .unwrap_or_else(|| agent.role.clone()),
        description: edit
            .and_then(|e| e.description.clone())
            .filter(|d| !d.is_empty())
            .or_else(|| agent.description.clone()),
        tools: crate::runtime::builder::agent_effective_grants(allow, tools.as_deref()),
    })
}

/// The turn instruction for a dispatched card: its title, plus its note when it
/// carries one, framed as a work item to act on.
fn task_instruction(card: &TaskRecord) -> String {
    match card.note.as_deref().filter(|n| !n.is_empty()) {
        Some(note) => {
            let (assignment, history) = task_note_sections(note);
            let research_hint = if public_research_assignment(&assignment) {
                " This is a public-source research assignment: begin with `web_search` now. Do \
                 not inspect the company workspace, ledgers, prior artifacts, or skill catalogue \
                 first. Open the strongest search results with `web_fetch` and cite what you \
                 actually read. If `web_search` reports an authentication, expired-session, \
                 credential, or provider-availability error, stop after that one call and report \
                 the exact blocker. Do not retry it with another query."
            } else {
                ""
            };
            if history.is_empty() {
                format!("Task: {}\n\n{}{}", card.title, assignment, research_hint)
            } else {
                format!(
                    "Task: {}\n\n## Prior attempt history\n\
                     Earlier agent/system output exists on the card but is omitted from this turn \
                     ({} bytes). It is history only. Do not summarize an earlier failure as the \
                     result of this run.\n\n## Current assignment\n{}\n\nPerform the current assignment now \
                     with the tools available in this turn. This card is already the authoritative \
                     task, so do not read the tasks ledger to rediscover it. If it asks for current \
                     public-source research and `web_search` is on your current tool belt, call \
                     `web_search`, then verify the strongest sources with `web_fetch`, before \
                     reporting that research is blocked.{}",
                    card.title,
                    history.len(),
                    assignment,
                    research_hint
                )
            }
        }
        None => format!("Task: {}", card.title),
    }
}

fn public_research_assignment(assignment: &str) -> bool {
    let lower = assignment.to_ascii_lowercase();
    lower.contains("research")
        && (lower.contains("source") || lower.contains("competitor") || lower.contains("landscape"))
}

/// Separate operator/reviewer instructions from result blocks accumulated on a
/// card by prior runs. `append_result` uses blank-line-separated `[label] `
/// blocks; continuation paragraphs inherit their preceding label.
fn task_note_sections(note: &str) -> (String, String) {
    let mut assignment = Vec::new();
    let mut history = Vec::new();
    let mut current_is_assignment = true;

    for paragraph in note.split("\n\n") {
        if let Some(label) = note_attribution(paragraph) {
            current_is_assignment = matches!(label, "operator" | "operator redirect" | "reviewer");
        }
        if current_is_assignment {
            assignment.push(paragraph);
        } else {
            history.push(paragraph);
        }
    }

    // A machine-authored-only note is unusual but still used by a few failure
    // paths. Never manufacture an empty assignment: the whole note remains the
    // work description in that case.
    if assignment.is_empty() {
        return (note.to_string(), String::new());
    }
    (assignment.join("\n\n"), history.join("\n\n"))
}

fn note_attribution(paragraph: &str) -> Option<&str> {
    let rest = paragraph.strip_prefix('[')?;
    let (label, _) = rest.split_once("] ")?;
    (!label.is_empty()
        && label.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_- ".contains(&byte)
        }))
    .then_some(label)
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
    /// The company's titling pass, so the card-opening paths that compile
    /// without the harness can still name what they open.
    fn titler(&self) -> Option<&dyn crate::ports::tasks::TitleSummariser> {
        Some(self.title_pass(&self.record().id))
    }

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
        // Set when a desk answered as a hive room. The episode journals every
        // turn and its own close directly (`hivemind::EpisodeDriver`), so it
        // hands nothing back through `channel_responses` — and the
        // "Acknowledged." fallback below must not then file a second, empty
        // system row under a conversation that was answered at length.
        let mut room_answered = false;
        for (index, event) in req.events.iter().enumerate() {
            // The durable log position of this event, which `CycleRequest`
            // carries positionally alongside the events themselves. Empty for a
            // caller that built the request without threading seqs, so the
            // lookup answers `None` rather than assuming an index is a seq.
            let event_seq = req.event_seqs.get(index).copied();
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
                    // Issue: hive-mind desks. A desk with somebody to
                    // deliberate WITH answers as a room rather than through one
                    // responder — `tinyhivemind_hive::step` decides who speaks
                    // next, that teammate runs an ordinary turn, its line is
                    // journaled on the desk, and the loop continues until the
                    // room converges, deadlocks, or spends its budget.
                    //
                    // It sits ABOVE the responder ladder below and BELOW the
                    // mention rung, which is the same explicit-beats-implicit
                    // ordering the rest of the ladder already applies: naming
                    // one teammate in a room addresses that teammate, not the
                    // room, so a `@mention` still runs exactly one turn.
                    // `desk_episode` declines everything else that must keep
                    // the single-turn path — an unaddressed message, a General
                    // spelling, a DM, a key that names no desk, a desk with
                    // fewer than two effective roster members, and a desk that
                    // opted out — so a company with one teammate per desk is
                    // unaffected byte-for-byte. A copilot thread never reaches
                    // here at all: it returned above.
                    //
                    // The episode journals its own turns, and deliberately
                    // pushes NO bubble onto `channel_responses`: the REST
                    // chat route journals every response it is handed as an
                    // `AgentReply`, so a bubble here would write a second row
                    // for a line the driver has already made durable. The
                    // console still sees each turn arrive live — the operator
                    // SSE feed projects journal rows — and a reload reads the
                    // same transcript the room itself folded.
                    //
                    // Skipped when the journal is not wired: the episode reads
                    // its own turns back out of it to fold the next step, so a
                    // driver with nowhere to append could not deliberate at
                    // all, and falling through answers exactly as before.
                    if crate::runtime::mentions::mention_responder(
                        &self.record(),
                        chat.as_deref(),
                        mentions,
                    )
                    .is_none()
                        && let Some(events) = self.deps.events.clone()
                        && let Some(desk) =
                            crate::hivemind::desk_episode(&self.record(), chat.as_deref())
                    {
                        // The operator message's own sequence: the episode's
                        // watermark, so the room folds what was said after it
                        // was asked and merely reads what came before. A
                        // request built without seqs falls back to the message
                        // itself being the whole of the room's history.
                        let trigger = event_seq.unwrap_or_else(|| EventSeq::new(0));
                        // The thread this episode's turns and closing report
                        // are parented to (issue: hive replies were never
                        // threaded to their trigger). Mirrors
                        // `server::operator::reply_thread`: already in a
                        // thread, the episode stays in it; otherwise the
                        // triggering operator message becomes the thread
                        // root, exactly as an ordinary single-responder reply
                        // threads itself. Without this, every hive turn for a
                        // top-level desk send is journaled with `parent: None`
                        // — unrelated top-level channel traffic detached from
                        // the question that asked it, and (worse) sharing the
                        // desk's channel-level projection with every other
                        // top-level hive send on the same desk, so a second
                        // operator message answered in the same cycle can
                        // fold the first episode's still-fresh turns as its
                        // own votes.
                        let thread_root = Some((*parent).unwrap_or(trigger));
                        let runner = HiveDeskRunner {
                            run_turn: self.run_turn(),
                            company: self.record().id.clone(),
                            chat_id: chat.clone(),
                            thread_root,
                            trigger_seq: Some(trigger),
                            brain: self,
                            host,
                        };
                        // The desk's own memory, over the same `ContextStore`
                        // the per-turn memory loop and the `memory_recall` belt
                        // read — so a hosted-memory overlay (CortexDB under
                        // `OPENCOMPANY_MEMORY=remote`) applies to what a room
                        // remembers exactly as it does to what a teammate does.
                        let memory = Arc::new(HiveDeskMemory {
                            context: Arc::clone(&self.deps.context),
                            company: self.record().id.clone(),
                            desk_id: desk.id.clone(),
                        });
                        // The company's other desks, when this one opted in to
                        // referral and there is a peer with somebody on it.
                        // `None` is the default and every desk that never wrote
                        // the block, so the driver below is byte-identical to
                        // the one that ran before referral existed.
                        let federation = crate::hivemind::desk_federation(&self.record(), &desk);
                        let mut driver = crate::hivemind::EpisodeDriver::new(
                            self.record().id.clone(),
                            desk,
                            events,
                            &runner,
                            composed.clone(),
                        )
                        .in_thread(thread_root)
                        .with_memory(memory)
                        // Every desk in the company, so a seat that also sits
                        // elsewhere reads its own other conversations. Ungated,
                        // unlike the federation above: this hands a member what
                        // it can already see, not permission to ask anyone
                        // anything.
                        .with_context_desks(crate::hivemind::company_desks(&self.record()))
                        // **An operator message to a desk runs completion-driven.**
                        //
                        // The room ends when every assigned member has reported
                        // `!complete`, not when a quorum can be named. That is
                        // what the reference runner in `tinyhivemind`'s OpenHuman
                        // example does, and it is the only form defined for a
                        // room of one — `deliberates()` floors a quorum room at
                        // two members and `desk_episode` additionally needs the
                        // configured quorum still reachable.
                        //
                        // `Quorum` stays reachable for the cross-desk crossing
                        // below, which asks another room to *decide* something
                        // rather than to do work.
                        .completing();
                        #[cfg(feature = "typesafe")]
                        {
                            // Handoffs route by meaning where a credential is
                            // configured; `None` leaves `!broadcast` falling to
                            // the desk's first other member, which is what this
                            // path did before routing existed.
                            driver = driver.with_router(
                                match crate::hivemind::broadcast::router_from_env() {
                                    Ok(router) => router.map(|router| {
                                        std::sync::Arc::new(router)
                                            as std::sync::Arc<
                                                dyn tinyhivemind_embed::routing::Router
                                                    + Send
                                                    + Sync,
                                            >
                                    }),
                                    Err(error) => {
                                        tracing::warn!(
                                            %error,
                                            "[hive] the routing credential is present but unusable; \
                                             handoffs fall back to the mechanical responder"
                                        );
                                        None
                                    }
                                },
                            );
                        }
                        if let Some(federation) = federation {
                            driver = driver.with_federation(federation, &runner);
                        }
                        let run_result = driver.run(trigger).await;
                        // Issue: an MCP tool-call failure inside a hive
                        // member's turn queues on `self.deps.mcp_failures`
                        // exactly as one inside an ordinary responder turn
                        // does, but nothing on this path ever drained it —
                        // the queue sat until a later, unrelated chat turn
                        // cleared it silently, and the failing call produced
                        // neither an error step nor a `McpCallFailed` journal
                        // row. Drained here, unconditionally, the same way
                        // the ordinary responder path drains it below: on
                        // success AND on error, since a failed episode may
                        // still have queued a real tool failure. The steps
                        // vec is discarded — a hive episode has no single
                        // bubble to attach them to, the transcript is already
                        // the record — but the drain's real effect, the
                        // journaled `McpCallFailed` row, does not depend on
                        // it. Best-effort: a failure surfacing its own
                        // failure must not cost the episode's real outcome.
                        let mut discarded_steps = Vec::new();
                        if let Err(err) =
                            self.surface_mcp_failures(&mut discarded_steps, None).await
                        {
                            tracing::warn!(
                                company = %self.record().id,
                                error = %err,
                                "[hive] failed to surface queued MCP failures after an episode"
                            );
                        }
                        let outcome = run_result?;
                        tracing::info!(
                            company = %self.record().id,
                            chat = %chat.as_deref().unwrap_or_default(),
                            ending = %outcome.ending.label(),
                            turns = outcome.turns,
                            failed_turns = outcome.failed_turns,
                            demoted = outcome.violations.len(),
                            asked = outcome.referrals.asked.len(),
                            "[hive] a desk answered as a room"
                        );
                        // Issue: a hive episode journals every turn and its
                        // own closing report directly (`EpisodeDriver`), so a
                        // synchronous chat-API caller and `emit_cycle_webhooks`
                        // — both of which read `CycleReport.responses`
                        // (`CycleResult.channel_responses` here) rather than
                        // the journal — saw an empty response collection and
                        // never fired `work.completed`, even though the desk
                        // had just answered at length. Pushing the closing
                        // report's own text back through here is NOT a second
                        // journal write: `outcome.report_seq` is the sequence
                        // the episode already journaled it under, so this
                        // response carries that durable id and
                        // `journal_chat_replies` — which otherwise journals
                        // every response it is handed — skips a response that
                        // already names one. When the report itself failed to
                        // journal (`report_seq` is `None`, logged where it
                        // happened), leaving `message_id` unset lets the
                        // ordinary path journal it now rather than losing it
                        // twice.
                        channel_responses.push(OutboundMessage {
                            message_id: outcome.report_seq.map(|seq| seq.value().to_string()),
                            task_id: None,
                            outputs: Vec::new(),
                            channel: "operator".to_string(),
                            agent: Some(crate::hivemind::HIVE_REPORT_AUTHOR.to_string()),
                            text: outcome.summary(),
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        });
                        room_answered = true;
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
                    // SPIKE: the thread-overseer rung.
                    //
                    // Oversight of a prompt transfers on hand-off: the operator
                    // opens the conversation, and whoever it delegates to owns
                    // the thread from there. So a reply *inside* a thread must
                    // reach whoever currently holds it — not the room's default
                    // answerer, which is what `responder_for` gives and which
                    // made a follow-up land on the desk lead while the actual
                    // overseer sat one message above.
                    //
                    // Derived, never stored: the overseer is the last teammate
                    // to have spoken under this root. That transfers for free —
                    // a delegate becomes the last speaker the moment it answers.
                    //
                    // Sits BELOW an @mention (naming somebody is still the
                    // strongest address) and ABOVE the channel default, which is
                    // the same explicit-beats-implicit ordering the ladder
                    // already applies.
                    let overseer = self.thread_overseer(chat.as_deref(), *parent).await;
                    // One thread, one card: a follow-up joins the work its
                    // thread already opened instead of opening another.
                    let thread_card = self.thread_card(chat.as_deref(), *parent).await;
                    let routed = self
                        .tinyhivemind_responder(chat.as_deref(), text, mentions)
                        .await;
                    // A direct address remains stronger than thread ownership.
                    // Otherwise the last agent holding the thread keeps it;
                    // TinyHiveMind supplies the channel/DM/orchestrator fallback.
                    let responder = match routed {
                        Some(decision)
                            if matches!(
                                decision.rung,
                                tinyhivemind::responder::ResponderRung::ExplicitMention
                                    | tinyhivemind::responder::ResponderRung::DirectAgent
                            ) =>
                        {
                            decision.responder_id
                        }
                        Some(decision) => overseer.unwrap_or(decision.responder_id),
                        None => overseer.unwrap_or_else(|| self.responder_for(chat.as_deref())),
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
                    let output_claim = self.deps.pending_publishes.output_collector().claim();
                    let turn = output_claim
                        .scoped(
                            self.delegation_runner(run_turn.as_ref(), &record)
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
                                // This message's own line in the journal, so the chat
                                // seed can tell it apart from a concurrently accepted
                                // sibling by identity instead of by text.
                                .answering(event_seq)
                                .maybe_for_task(thread_card.as_deref())
                                // Issue #1846 review (Codex #3864988176): the operator's
                                // own words, so a delegate's budget-pause marker re-parks
                                // with what the operator actually asked for rather than
                                // the hand-off instruction the model wrote.
                                .reissue_message(composed.clone())
                                .handle_operator_message(&responder, &composed, chat_id),
                        )
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
                    let mut published_card = output_claim
                        .scoped(self.file_conversation_batch(
                            &responder,
                            turn.spawned_task.as_deref(),
                            // Issue #1890 B: the same conversation this turn
                            // answers in, thread and all — `parent` IS the root.
                            ChatTarget::in_thread(chat_id, *parent),
                            publish_claim.is_some(),
                            published,
                            &mut operator_reply,
                        ))
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
                            let declined = output_claim
                                .scoped(self.nudge_for_unpublished(
                                    run_turn.as_ref(),
                                    &responder,
                                    text,
                                    &operator_reply,
                                    &unpublished,
                                    changed.partial,
                                    &nudge_control,
                                    None,
                                    None,
                                ))
                                .await;
                            let nudge_published = self.deps.pending_publishes.drain();
                            if let Some(card_id) = output_claim
                                .scoped(self.file_conversation_batch(
                                    &responder,
                                    turn.spawned_task.as_deref(),
                                    ChatTarget::in_thread(chat_id, *parent),
                                    publish_claim.is_some(),
                                    nudge_published,
                                    &mut operator_reply,
                                ))
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
                    let operator_outputs = output_claim.drain();
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
                        task_id: published_card.clone().or(turn.spawned_task.clone()),
                        channel: "operator".to_string(),
                        // Issue #885: who spoke, as distinct from where it goes.
                        // `responder_for` already picked this agent to answer the
                        // turn; before this the identity died here and the reply
                        // was journaled as `agent_id: "operator"` forever.
                        agent: Some(responder.clone()),
                        text: operator_reply.clone(),
                        reply_to: None,
                        mentions: Vec::new(),
                        steps: operator_steps,
                        outputs: operator_outputs,
                    });
                    // ── @ IS NOT AN EXECUTION CHANNEL ──────────────────────
                    //
                    // An earlier spike routed an agent's own @mention, on the
                    // theory that a hand-off is just a message naming the next
                    // teammate. It works, and it is unsafe, because a REFERENCE
                    // and a HAND-OFF are textually identical. Observed, with
                    // the routing on: asked "who just asked you this?",
                    // `qa_engineer` replied `@product_manager` — a bare mention
                    // at the head of the message, indistinguishable from a
                    // hand-off — and it dispatched a turn to product_manager,
                    // which mentioned back, which is the ping-pong only the hop
                    // cap stopped.
                    //
                    // No textual rule separates the two, because the model
                    // writes the text. So an agent's mention stays what
                    // `Mention::quiet` already describes — "draw the chip, but
                    // do not notify and do not route" — and handing work over
                    // goes through `delegate_to_desk`, which since the async
                    // hand-off actually transfers ownership, mints the
                    // delegate their own attempt, and rolls back if they cannot
                    // run.
                    //
                    // The convention the operator wants — plain names to refer,
                    // `@` to delegate — is then true by construction rather
                    // than by the model's cooperation: the only `@` that routes
                    // is one the hand-off tool produced.

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
                    // Authored by the **system** for the same reason: no
                    // teammate said this, and attributing it to the responder
                    // would put the platform's words in its mouth. This was
                    // `agent: None`, which meant the same thing and did not
                    // achieve it — an authorless reply journals as
                    // `agent_id: "operator"`, no roster member matches, and the
                    // console falls back to the channel's voice, so the
                    // platform's words appeared under the orchestrator's name.
                    // `SYSTEM_AUTHOR` is what the desk path below already used
                    // and what `chat.ts` maps to `from: "system"`. The notice
                    // names the responder in its text instead, as
                    // `spend_halt_notice` does — whose turn capped is the part
                    // the operator needs, and saying it is not the same as
                    // saying they said it. Empty steps — the turn's timeline is
                    // already on the bubble above, and repeating it would
                    // double every row in the console.
                    if turn.hit_iteration_cap {
                        channel_responses.push(OutboundMessage {
                            message_id: None,
                            task_id: None,
                            outputs: Vec::new(),
                            channel: "operator".to_string(),
                            agent: Some(crate::ports::SYSTEM_AUTHOR.to_string()),
                            text: iteration_cap_pause_notice(&responder),
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
                            outputs: Vec::new(),
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
                            outputs: Vec::new(),
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
                    let output_claim = self.deps.pending_publishes.output_collector().claim();
                    let turn = output_claim
                        .scoped(
                            self.delegation_runner(run_turn.as_ref(), &record)
                                .handle_operator_message(&responder, prompt, None),
                        )
                        .await?;
                    let mut responses = vec![OutboundMessage {
                        message_id: None,
                        task_id: turn.spawned_task,
                        outputs: Vec::new(),
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
                            outputs: Vec::new(),
                            channel: crate::server::ops::language::DEFAULT_DESK.to_string(),
                            agent: Some(crate::ports::SYSTEM_AUTHOR.to_string()),
                            text: iteration_cap_pause_notice(&responder),
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        });
                    }
                    if let Some(halt) = &turn.halted_for_spend {
                        responses.push(OutboundMessage {
                            message_id: None,
                            task_id: None,
                            outputs: Vec::new(),
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
                            outputs: Vec::new(),
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
                    let mut published_card = output_claim
                        .scoped(self.file_conversation_batch(
                            &responder,
                            spawned_task.as_deref(),
                            // Nothing threaded a scheduled turn: it posts into
                            // the General desk's channel-level conversation,
                            // which is what the bare id meant before #1890 B.
                            ChatTarget::channel(Some(crate::server::ops::language::DEFAULT_DESK)),
                            publish_claim.is_some(),
                            published,
                            &mut responses[0].text,
                        ))
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
                            let _declined = output_claim
                                .scoped(self.nudge_for_unpublished(
                                    run_turn.as_ref(),
                                    &responder,
                                    prompt,
                                    &responses[0].text,
                                    &unpublished,
                                    changed.partial,
                                    &nudge_control,
                                    None,
                                    None,
                                ))
                                .await;
                            let nudge_published = self.deps.pending_publishes.drain();
                            if let Some(card_id) = output_claim
                                .scoped(self.file_conversation_batch(
                                    &responder,
                                    spawned_task.as_deref(),
                                    ChatTarget::channel(Some(
                                        crate::server::ops::language::DEFAULT_DESK,
                                    )),
                                    publish_claim.is_some(),
                                    nudge_published,
                                    &mut responses[0].text,
                                ))
                                .await
                            {
                                published_card = published_card.or(Some(card_id));
                            }
                        }
                    }
                    drop(publish_claim);
                    responses[0].outputs = output_claim.drain();
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
                            outputs: Vec::new(),
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
                                        audience: Vec::new(),
                                        parent: None,
                                        task_id: response.task_id.clone(),
                                        outputs: response.outputs.clone(),
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

        // A cycle answers with at least one channel response, unless a hive
        // room already journaled its whole conversation itself.
        if channel_responses.is_empty() && !room_answered {
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

/// A hive episode's turn seam, wired to the harness.
///
/// The whole of what an episode needs from this host: an agent id and a prompt
/// in, one reply out. Everything that makes the answering teammate a teammate —
/// its tools, its memory retrieve/inject/store loop, its approval gate — comes
/// from `RunTurn::run` being the ordinary turn path, unchanged. A deliberating
/// turn differs from a single-responder turn in the prompt it is handed and in
/// nothing else.
///
/// The chat target is the desk the episode is deliberating on, so the live
/// turn-stream frames carry it and the console routes them to that thread
/// exactly as it does for a normal desk turn.
struct HiveDeskRunner<'a> {
    run_turn: Arc<dyn RunTurn>,
    company: CompanyId,
    chat_id: Option<String>,
    thread_root: Option<EventSeq>,
    /// The triggering operator message's own sequence — the same `trigger`
    /// the episode itself was started with. Carried onto every `ChatTarget`
    /// this runner builds (`.answering(...)`) so `TurnStreamCtx::message_seq`
    /// is populated for a hive turn exactly as it is for an ordinary one;
    /// without it, live tool frames from a desk started inside a thread carry
    /// no message identity and the frontend files them under the desk-level
    /// thread instead of the active `desk#root`/message bucket.
    trigger_seq: Option<EventSeq>,
    /// The brain and host this runner's episode is running under, so a
    /// gated tool call a member's turn hits can be parked for the operator
    /// **between turns**, not only once the whole episode has finished.
    ///
    /// Without this, `request_approval` — the tool `speak`/`refer` reach
    /// through `self.run_turn.run(...)` — enqueues onto
    /// `self.deps.approval_requests` and the turn completes normally with a
    /// "blocked, requires approval" refusal folded into the reply text
    /// (openhuman resolves `RequireApproval` inline; nothing downstream
    /// blocks on it). The episode loop therefore keeps running further
    /// turns without ever surfacing the pending request, and the *cycle's*
    /// `park_approval_requests` call only runs once
    /// [`driver.run(trigger)`](crate::hivemind::EpisodeDriver::run) has
    /// already returned — by then the room may have converged, deadlocked
    /// or exhausted its budget without the approval it was waiting on ever
    /// reaching `scripts/hive-euler.py`'s concurrent approval pump.
    ///
    /// Draining after every turn this runner completes closes that gap; the
    /// cycle-level call after `driver.run` stays in place as a safety net
    /// for anything queued by non-hive-turn work in the same cycle.
    brain: &'a HarnessBrain,
    host: &'a dyn CycleHost,
}

/// Drains and parks whatever a single hive turn just queued onto
/// `self.deps.approval_requests`, logging rather than propagating a failure:
/// there is no single "reply" a hive turn returns an operator-visible notice
/// on the way [`HarnessBrain::park_approval_requests`] does for an ordinary
/// cycle, and a parking failure must not fail the turn that already computed
/// a real answer.
async fn park_hive_turn_approvals(brain: &HarnessBrain, host: &dyn CycleHost, agent_id: &str) {
    match brain.park_approval_requests(host).await {
        Ok(None) => {}
        Ok(Some(notice)) => tracing::warn!(
            agent = %agent_id,
            %notice,
            "[hive] not every approval request from this turn could be parked for the operator"
        ),
        Err(err) => tracing::warn!(
            agent = %agent_id,
            error = %err,
            "[hive] failed to park approval requests queued during a member's turn"
        ),
    }
}

/// Turns a terminal outcome (`outcome.budget_paused` / `outcome.halted_for_spend`
/// / `outcome.abnormal_stop`) into the hard error a hive turn must surface, or
/// `None` when the turn actually answered.
///
/// Without this, `outcome.reply` on any of the three is host-authored
/// pause/halt/refusal copy, not the agent's answer — folding it as `Ok(reply)`
/// lets `EpisodeDriver` journal that copy as a genuine `CompanyEvent::AgentReply`
/// under the member's own identity, and the episode never counts the turn as
/// failed (`EpisodeOutcome::failed_turns`), so an operator reading the
/// transcript cannot tell a real answer from a budget wall or a pre-dispatch
/// refusal the room hit.
fn terminal_budget_error(
    agent_id: &str,
    outcome: &crate::harness::TurnOutcome,
) -> Option<crate::OpenCompanyError> {
    if let Some(pause) = &outcome.budget_paused {
        return Some(crate::OpenCompanyError::Harness(format!(
            "{agent_id} paused for lack of inference budget mid-deliberation: {}",
            pause.summary
        )));
    }
    if let Some(halt) = &outcome.halted_for_spend {
        return Some(crate::OpenCompanyError::Harness(format!(
            "{agent_id} halted for spend mid-deliberation: spent ${:.2} against a cap of ${:.2}",
            halt.spent_usd, halt.cap_usd
        )));
    }
    if let Some(reason) = &outcome.abnormal_stop {
        return Some(crate::OpenCompanyError::Harness(format!(
            "{agent_id} did not complete its turn: {reason}"
        )));
    }
    None
}

#[async_trait]
impl crate::hivemind::HiveTurnRunner for HiveDeskRunner<'_> {
    async fn speak(&self, agent_id: &str, prompt: &str) -> Result<String> {
        let outcome = self
            .run_turn
            .run(
                &self.company,
                agent_id,
                prompt,
                // `deliberating`, not `in_thread`: the episode prompt already
                // carries this desk's transcript, attributed and filtered to
                // what this turn is allowed to see. Seeding the desk's recent
                // history on top would hand the same lines back unattributed
                // and in the assistant role — a peer's opening position
                // reaching a *blind* turn, and every peer line reading as
                // something this member had itself said.
                //
                // `.answering(self.trigger_seq)`: this turn is still, at
                // bottom, this desk's response to the operator message that
                // opened the episode. Without it, `TurnStreamCtx::message_seq`
                // is `None` for every hive turn, and a desk started inside a
                // thread has its live tool frames fall back to the
                // desk-level thread bucket instead of the active
                // `desk#root`/message one.
                ChatTarget::deliberating(self.chat_id.as_deref(), self.thread_root)
                    .answering(self.trigger_seq),
            )
            .await?;
        park_hive_turn_approvals(self.brain, self.host, agent_id).await;
        if let Some(error) = terminal_budget_error(agent_id, &outcome) {
            return Err(error);
        }
        Ok(outcome.reply)
    }
}

impl HiveDeskRunner<'_> {
    /// An unconverged referred room's own turns, attributed and in order, as
    /// the answer to carry home.
    ///
    /// The members' lines rather than the closing report, for the reason
    /// [`deliberate`](crate::hivemind::HiveReferralRunner::deliberate) gives:
    /// a report only speaks for the desk once the desk has agreed on
    /// something.
    ///
    /// Each line is rendered through the same rewrite a person reading that
    /// desk gets. A room's move grammar — `!propose #topic ^N` — is addressed
    /// to the fold *of that desk*, and carrying it into another desk's
    /// transcript would hand the asking room markers naming topics and
    /// sequences that do not exist there.
    ///
    /// **The one agent-facing path that rewrites.** Everywhere else — the
    /// episode prompt, `elsewhere_for`, `referral_prompt`, `chat_seed` — reads
    /// the stored body, because that is how a seat cites `^16` against a row it
    /// can identify (see `readable_moves`). The exception earns itself on that
    /// same rule: this text *crosses desks*, so the rows its markers name are
    /// rows the reader cannot identify. What the asking room cites instead is
    /// the relayed line's own sequence on its own desk, which is what it does
    /// in practice.
    ///
    /// `None` when the room took no turn at all, which is a desk that did not
    /// answer rather than a desk that answered nothing.
    async fn turns_of(
        &self,
        events: &Arc<dyn crate::ports::events::EventLog>,
        company: &crate::ports::types::CompanyId,
        outcome: &crate::hivemind::EpisodeOutcome,
        desk_id: &str,
        // The question this room was convened on. Every turn of a referred
        // episode is parented to it, which is what separates them from whatever
        // else that desk was doing at the time.
        root: EventSeq,
    ) -> Option<String> {
        let (first, last) = (outcome.first_seq?, outcome.last_seq?);
        let span = last.value().saturating_sub(first.value()).saturating_add(1);
        let page = events
            .read_from(company, first, usize::try_from(span).unwrap_or(usize::MAX))
            .await
            .ok()?;
        let said: Vec<String> = page
            .iter()
            .filter(|stored| stored.seq <= last)
            .filter_map(|stored| match &stored.event {
                CompanyEvent::AgentReply {
                    chat_id,
                    agent_id,
                    text,
                    audience,
                    parent,
                    ..
                } if chat_id == desk_id
                    // **This room's turns, not the desk's other traffic.**
                    //
                    // A desk that was asked keeps working while the referred
                    // room runs — its own episode, an ordinary reply — and all
                    // of it lands on the same desk inside the same sequence
                    // span. Carried home, somebody else's words would arrive as
                    // this desk's answer and steer the asking room. The history
                    // projection scopes the same rows by this parent; this is
                    // the second builder and needed it too (Codex, #2332).
                    && *parent == Some(root)
                    && !crate::hivemind::is_hive_author(agent_id)
                    // **An aside is not a turn, and never leaves the desk.**
                    //
                    // `!aside @peer` is journaled as an ordinary `AgentReply`
                    // with the pair in `audience`, so it sits in this span like
                    // any other row and can even be the last one. Carried home
                    // it would publish a private exchange to a desk that was
                    // never in it — across a desk boundary, where nobody there
                    // can even see that it happened. Only desk-visible turns
                    // answer a crossing (CodeRabbit, #2332).
                    && audience.is_empty() =>
                {
                    Some(format!(
                        "{agent_id}: {}",
                        crate::server::chat_history::readable_moves(text.clone()).trim()
                    ))
                }
                _ => None,
            })
            .collect();
        (!said.is_empty()).then(|| said.join("\n"))
    }
}

#[async_trait]
impl crate::hivemind::HiveReferralRunner for HiveDeskRunner<'_> {
    async fn refer(&self, desk_id: &str, agent_id: &str, prompt: &str) -> Result<String> {
        let outcome = self
            .run_turn
            .run(
                &self.company,
                agent_id,
                prompt,
                // The *far* desk's channel, and never this episode's thread: a
                // thread root is a sequence in the conversation that owns it, so
                // carrying the asking desk's root across would parent the answer
                // to a message that does not exist over there.
                //
                // `deliberating` for the same reason the episode's own turns
                // are, and for one more: the referred teammate is not in this
                // room, so seeding it with the far desk's recent history would
                // put lines it has never read into its own assistant role while
                // it answers a question from somewhere else entirely.
                ChatTarget::deliberating(Some(desk_id), None),
            )
            .await?;
        park_hive_turn_approvals(self.brain, self.host, agent_id).await;
        if let Some(error) = terminal_budget_error(agent_id, &outcome) {
            return Err(error);
        }
        Ok(outcome.reply)
    }

    async fn deliberate(&self, desk_id: &str, asker: &str, prompt: &str) -> Result<Option<String>> {
        let record = self.brain.record();
        // Whether that desk can hold a room at all: it needs a `[hive]` block,
        // enough members, and a quorum still reachable with its *effective*
        // roster. `None` for any of those, and the seat answers — the one thing
        // this must not do is refuse a crossing a company is entitled to make.
        let Some(desk) = crate::hivemind::desk_episode(&record, Some(desk_id)) else {
            return Ok(None);
        };
        let Some(events) = self.brain.deps.events.clone() else {
            // The same guard the desk's own episode path carries: a room reads
            // its turns back out of the journal to fold the next step, so one
            // with nowhere to append could not deliberate at all.
            return Ok(None);
        };
        // **The question, written onto the desk being asked.**
        //
        // A single-seat crossing deliberately leaves nothing here — see
        // `referral::forward` — and for a single seat that is right: the seat
        // is handed the question in its prompt and its answer is the only row
        // the desk needs. A room cannot work that way. It decides what to say
        // next by folding this desk's transcript, so a question that is not in
        // the transcript is a question no seat after the first can read; and
        // every turn has to hang off something, or a second crossing into this
        // desk in the same cycle shares the channel-level projection with this
        // one and folds its turns as its own votes.
        //
        // So it is journaled, and its sequence is both the thread root and the
        // episode's watermark: the room folds what was said after it was asked
        // and merely reads what came before.
        //
        // As an `OperatorMessage` by the asking agent, which is not a new shape
        // — it is exactly what a chat-path crossing already lands on the desk
        // it goes to, and `attach_referral_origins` matches it as such ("it IS
        // that agent speaking there"). A reserved `hive-question` author would
        // have been the obvious alternative and is the wrong one twice over:
        // both projections would render it as a *teammate* nobody can spell,
        // which is the precise defect `HIVE_REPORT_AUTHOR` is dropped to avoid,
        // and the room folds an operator message as the request it is answering
        // — which is what this row is — where an agent line folds as a turn.
        let root = events
            .append(
                &record.id,
                CompanyEvent::OperatorMessage {
                    text: prompt.to_string(),
                    by: Some(crate::ports::types::Actor {
                        kind: crate::ports::types::ActorKind::Agent,
                        id: asker.to_string(),
                    }),
                    chat: Some(desk.id.clone()),
                    parent: None,
                    deliverable: None,
                    mentions: Vec::new(),
                    attachments: Vec::new(),
                },
            )
            .await?;
        // Kept before `EpisodeDriver::new` takes the desk by value.
        let far_desk = desk.id.clone();
        let runner = HiveDeskRunner {
            run_turn: Arc::clone(&self.run_turn),
            company: record.id.clone(),
            // This desk and this thread — never the asking episode's, whose
            // thread root is a sequence in a conversation that is not this one.
            chat_id: Some(desk.id.clone()),
            thread_root: Some(root),
            trigger_seq: Some(root),
            brain: self.brain,
            host: self.host,
        };
        let memory = Arc::new(HiveDeskMemory {
            context: Arc::clone(&self.brain.deps.context),
            company: record.id.clone(),
            desk_id: desk.id.clone(),
        });
        // **No `with_federation`, and that omission is the recursion bound.**
        //
        // `max_hops` bounds a chain of referrals *within* one episode's ledger.
        // A referred episode is a NEW episode with a fresh ledger starting at
        // hop 0, so a far desk allowed to refer onward could convene a third
        // desk, which could convene a fourth, and nothing in the hop budget
        // would ever see it. A crossing is therefore one room deep: this desk
        // answers with what it knows, or says it does not know.
        let outcome = crate::hivemind::EpisodeDriver::new(
            record.id.clone(),
            desk,
            Arc::clone(&events),
            &runner,
            prompt.to_string(),
        )
        .in_thread(Some(root))
        .with_memory(memory)
        .with_context_desks(crate::hivemind::company_desks(&record))
        .run(root)
        .await?;
        // **What to carry home depends on how the room ended.**
        //
        // A room that CONVERGED speaks for the desk in its closing report: the
        // report names the proposal that carried and who grounded it, which is
        // the one sentence no single seat is entitled to say.
        //
        // A room that did not converge does not. Its report is bookkeeping —
        // "Nobody on the desk had anything to add, so the room did not open" —
        // and relaying that as the desk's answer is worse than saying nothing,
        // because the asking room is told the desk had no view while the view
        // sits one row away in the transcript. A live crossing did exactly
        // that: both seats said plainly that refunds is their remit, the room
        // ended `idle` because neither line carried a move, and the answer that
        // came home said nobody had anything to add.
        //
        // So an unconverged room carries its members' own turns instead,
        // attributed, in the order they were said.
        let conclusion = match &outcome.ending {
            crate::hivemind::EpisodeEnding::Converged { .. } => {
                if let Some(report_seq) = outcome.report_seq {
                    let page = events.read_from(&record.id, report_seq, 1).await?;
                    page.into_iter().find_map(|stored| match stored.event {
                        CompanyEvent::AgentReply { text, .. } if stored.seq == report_seq => {
                            Some(text)
                        }
                        _ => None,
                    })
                } else {
                    tracing::warn!(
                        company = %record.id,
                        desk = %desk_id,
                        turns = outcome.turns,
                        "[hive] a referred desk converged but wrote no closing report"
                    );
                    // NOT `Ok(None)`: that means "this desk cannot hold a
                    // room", and `forward` acts on it by running one more model
                    // turn. The room has already run and spent its turns here —
                    // only the append of its closing row failed — so falling
                    // back would bill the room AND a seat, and credit the answer
                    // to `@<seat>` instead of the desk. Carry what the room
                    // said, exactly as the unconverged arm does (CodeRabbit,
                    // #2332).
                    self.turns_of(&events, &record.id, &outcome, &far_desk, root)
                        .await
                }
            }
            _ => {
                self.turns_of(&events, &record.id, &outcome, &far_desk, root)
                    .await
            }
        };
        tracing::info!(
            company = %record.id,
            desk = %desk_id,
            ending = %outcome.ending.label(),
            turns = outcome.turns,
            failed_turns = outcome.failed_turns,
            answered = conclusion.is_some(),
            "[hive] a referred desk answered as a room"
        );
        // **An empty room that RAN is a failure, not an absent room.**
        //
        // `Ok(None)` means "this desk cannot hold a room" and `forward` acts on
        // it by running a single seat instead. Past this point the room has been
        // stood up and has spent its budget — a one-turn desk whose only turn
        // failed comes back as a non-error `Exhausted` with no rows at all — so
        // returning `None` here would bill the room AND a seat, and credit the
        // answer to a seat that never spoke. An `Err` is what a crossing that
        // got nothing back already means, and `unanswered` now names the desk
        // rather than a seat for exactly this case (Codex, #2332).
        match conclusion {
            Some(answer) => Ok(Some(answer)),
            None => Err(crate::OpenCompanyError::Harness(format!(
                "the {desk_id} desk deliberated and produced no answer to carry back \
                 (ending {}, {} turn(s), {} of them failed)",
                outcome.ending.label(),
                outcome.turns,
                outcome.failed_turns,
            ))),
        }
    }
}

/// A deliberating desk's own memory, over the company's real context store.
///
/// Namespaced by desk — every note is labelled `hive/<desk id>/<slug>` — beside
/// the way an agent's private memories are labelled `agent-memory/<agent id>/…`
/// (`memory_tools.rs`). One desk's deliberations are therefore listable on their
/// own, never collide with another desk's, and are not mixed into a teammate's
/// private memories.
///
/// Both halves go through [`ContextStore`], which is the overlay seam: with
/// `OPENCOMPANY_MEMORY=remote` this is CortexDB, exactly as it is for the
/// per-turn retrieve→inject→store loop and the `memory_recall` tool.
struct HiveDeskMemory {
    context: Arc<dyn ContextStore>,
    company: CompanyId,
    desk_id: String,
}

/// How many search hits to ask for before narrowing them to this desk.
///
/// [`ContextStore::search`] ranks company-wide and returns no label, so the
/// desk scope is applied by intersecting its hits with the addresses actually
/// stored under this desk's prefix. Over-fetching is what makes that
/// intersection likely to be non-empty on a company whose memory is mostly
/// task outcomes and agent notes.
const HIVE_SEARCH_FANOUT: usize = 8;

#[async_trait]
impl crate::hivemind::HiveMemory for HiveDeskMemory {
    async fn recall(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::hivemind::HiveMemoryHit>> {
        let prefix = crate::hivemind::desk_prefix(&self.desk_id);
        let mine = self.context.list(&self.company, &prefix).await?;
        if mine.is_empty() {
            return Ok(Vec::new());
        }
        // Relevance first: the store's own ranking, narrowed to this desk.
        let hits = self
            .context
            .search(
                &self.company,
                query,
                limit.saturating_mul(HIVE_SEARCH_FANOUT),
            )
            .await?;
        let mut addrs: Vec<crate::ports::types::ChunkAddr> = hits
            .into_iter()
            .filter(|hit| mine.iter().any(|meta| meta.addr == hit.addr))
            .map(|hit| hit.addr)
            .take(limit)
            .collect();
        // Recency as the fallback, not as a supplement: a search that matched
        // nothing on this desk means the ranking has no opinion here, and the
        // most recent thing the desk concluded is a better answer than nothing.
        // Mixing the two would let a stale note outrank a relevant one.
        if addrs.is_empty() {
            let mut recent = mine.clone();
            recent.sort_by_key(|meta| std::cmp::Reverse(meta.stored_at_millis));
            addrs = recent
                .into_iter()
                .map(|meta| meta.addr)
                .take(limit)
                .collect();
        }
        let bodies = self.context.peek_many(&self.company, &addrs).await?;
        Ok(bodies
            .into_iter()
            .flatten()
            .map(|snippet| crate::hivemind::HiveMemoryHit { snippet })
            .collect())
    }

    async fn remember(&self, note: crate::hivemind::HiveMemoryNote) -> Result<()> {
        // Redacted on the way in, at the note's single construction point, for
        // the same reason `memory_loop::outcome_chunk` redacts: a deliberation
        // line can quote a tool's captured output, and this path bypasses
        // `OcMemory::store` entirely.
        let title = super::memory::redact_secrets(&note.title);
        self.context
            .put(
                &self.company,
                ContextChunk {
                    label: crate::hivemind::note_label(&note.desk_id, &title),
                    // Title on the first line, so the body is self-describing
                    // wherever it surfaces — a recall snippet, the Brain view,
                    // the next episode's "The desk remembers:" block.
                    body: format!("{title}\n\n{}", super::memory::redact_secrets(&note.body)),
                },
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "brain_tests.rs"]
mod tests;
