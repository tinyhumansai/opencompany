//! [`ApprovalPolicy`] — a manifest `[policy]` → openhuman [`ToolPolicy`] bridge.
//!
//! Manifest `[policy].mode` deliberately uses OpenHuman's own security-tier
//! words — `readonly` / `supervised` / `full` — so the mapping to
//! [`PolicyMode`] is 1:1. On top of the tier the bridge honours the manifest's
//! `always_approve` effect kinds and the per-agent `budget_usd_daily` /
//! `auto_approve_under_usd` thresholds.
//!
//! ## Where approvals actually park (issue #172)
//!
//! openhuman's [`ToolPolicy`] returns
//! [`ToolPolicyDecision::RequireApproval`](oh::agent::tool_policy::ToolPolicyDecision::RequireApproval),
//! which the session turn loop treats **fail-closed** — it blocks the tool call
//! and feeds the model a refusal rather than suspending and resuming it inline.
//! That refusal was for a long time the *only* trace a gated call left: nothing
//! was ever written to opencompany's [`ApprovalGate`] port or its journal, so
//! the operator's Approvals page stayed empty however many tools an agent
//! parked, and the work silently dead-ended.
//!
//! The bridge is now closed. Every `RequireApproval` this policy returns also
//! projects the flagged call onto an opencompany [`Effect`]
//! ([`ApprovalPolicy::effect_for`]) and pushes it onto the shared
//! [`ApprovalRequestQueue`] carried on
//! [`HarnessDeps`](crate::harness::HarnessDeps). The
//! [`HarnessBrain`](crate::harness::HarnessBrain) drains that queue after the
//! turn and parks each request through
//! [`CycleHost::park_effect`](crate::ports::brain::CycleHost::park_effect), so
//! the request lands in the journal the Approvals page reads and survives a
//! restart. Same cheap-shared-handle pattern as the delegation and MCP-failure
//! queues.
//!
//! ## Resume-after-approval, via grants (issue #243)
//!
//! Closing the park bridge left approval still not *meaning* anything: the
//! verdict was recorded, the queue drained, and the tool never ran. The operator
//! had to go back and ask for the same thing again — with the work having
//! silently dead-ended in between.
//!
//! openhuman genuinely cannot be resumed here: it resolves a `RequireApproval`
//! inline and the blocked call is gone by the time the operator sees it. So the
//! call is not resumed, it is **re-issued**. Approving a parked harness effect
//! mints a single-use [`GrantedCall`](crate::runtime::grants::GrantedCall)
//! scoped to that agent, that tool and those exact arguments, and the
//! [`HarnessBrain`](crate::harness::HarnessBrain) re-dispatches the granting
//! agent with an instruction to make the call again unchanged. The grant is
//! consumed at the top of [`check`](ToolPolicy::check) — above
//! `always_approve`, because a tool that always parks must still *run* once the
//! operator has approved that specific call — and it is gone afterwards, so the
//! next call to the same tool parks like any other.
//!
//! A grant that goes unredeemed expires
//! ([`GRANT_TTL_MILLIS`](crate::runtime::grants::GRANT_TTL_MILLIS)) and the
//! operator is told the agent did not act, rather than the permission sitting
//! live indefinitely.
//!
//! ## The per-agent daily spend cap (issue #304)
//!
//! The manifest's per-agent `budget_usd_daily` was validated, persisted and
//! passed all the way down to a field on this struct whose getter had no call
//! sites. It was documentation. The company-wide `[budget].monthly_usd` **is**
//! enforced on the economy path, so the two knobs presented identically while
//! only one of them was real.
//!
//! Spend arrives through two doors, so enforcement is two layers. Inference —
//! the dominant stream — is gated at dispatch in
//! [`HarnessPool::run`](crate::harness::HarnessPool::run), before any model
//! call. Priced *tool* calls are gated here, by the arm below `always_approve`.
//! See [`ApprovalPolicy::daily_budget_verdict`] for why the arm sits exactly
//! where it does, why an at-cap call **parks** rather than being denied, and
//! what the cap does not see.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use openhuman_core::openhuman as oh;

use oh::agent::tool_policy::{ToolPolicy, ToolPolicyDecision, ToolPolicyRequest};

use crate::company::Policy;
use crate::metering::{usd_spent_by_agent, utc_day_start_millis};
use crate::ports::UsageMeter;
use crate::ports::types::{CompanyId, Effect, EffectGroup};
use crate::runtime::grants::{GrantSet, GrantedCall};

/// Most approval requests parked out of a single turn. A model that keeps
/// re-trying a blocked tool (openhuman feeds it a refusal and lets it continue)
/// must not be able to flood the operator's queue, so the drain is bounded the
/// same way delegation is.
pub const MAX_APPROVAL_REQUESTS_PER_TURN: usize = 8;

/// The three approval tiers, mirroring OpenHuman's security tiers 1:1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyMode {
    /// Read-only: mutating / external-effect tools are denied outright.
    Readonly,
    /// Supervised: external-effect tools require operator approval.
    Supervised,
    /// Full autonomy: tools run without approval (except `always_approve`).
    Full,
}

impl PolicyMode {
    /// Parses a manifest `[policy].mode` string; unknown values fall back to the
    /// safe `Supervised` default.
    pub fn parse(mode: &str) -> Self {
        match mode.trim().to_ascii_lowercase().as_str() {
            "readonly" => Self::Readonly,
            "full" => Self::Full,
            _ => Self::Supervised,
        }
    }

    /// The openhuman security-tier word this mode maps to (1:1).
    pub fn security_tier(self) -> &'static str {
        match self {
            Self::Readonly => "readonly",
            Self::Supervised => "supervised",
            Self::Full => "full",
        }
    }
}

/// One approval-gated tool call observed during an agent turn: the projected
/// [`Effect`] the operator will see, plus the tool and the policy's own reason
/// for logging.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalRequest {
    /// The tool the agent tried to call.
    pub tool: String,
    /// Why the policy flagged it (the same wording openhuman feeds the model).
    pub reason: String,
    /// The projected effect to park on the gate.
    pub effect: Effect,
}

/// A shared, in-memory queue of approval-gated tool calls — the exact
/// [`DelegationQueue`](crate::harness::orchestrator::DelegationQueue) /
/// [`McpFailureQueue`](crate::harness::mcp_probe::McpFailureQueue) pattern.
/// Cheap to [`Clone`] (a shared handle); the [`ApprovalPolicy`] installed on
/// every roster agent and the [`HarnessBrain`](crate::harness::HarnessBrain)
/// that drains it see the same queue because
/// [`HarnessDeps`](crate::harness::HarnessDeps) clones share this handle.
#[derive(Clone, Default)]
pub struct ApprovalRequestQueue {
    inner: Arc<Mutex<Vec<ApprovalRequest>>>,
    /// The live single-use grants (issue #243), riding along so the whole
    /// approval round-trip travels on one handle.
    ///
    /// It lives here rather than as a new `HarnessDeps` field because every one
    /// of the ~28 `HarnessDeps` literals in this crate (tests, examples, the
    /// builder) would otherwise have to be widened to carry it — for a value
    /// only the approval path reads.
    ///
    /// **Its own `Arc<Mutex<..>>` is load-bearing**, not incidental
    /// encapsulation. [`clear`](Self::clear) runs at the top of every cycle and
    /// empties `inner`; a grant folded into that same allocation would be wiped
    /// by the very cycle that was dispatched to redeem it, so the feature would
    /// fail in exactly its own happy path. The test
    /// `grants_survive_a_queue_clear` pins this.
    grants: GrantSet,
}

impl ApprovalRequestQueue {
    /// Records a gated call, ignoring one already queued for the same tool and
    /// arguments.
    ///
    /// openhuman blocks the call but lets the turn continue, so a model that
    /// re-tries the same tool would otherwise park the identical request several
    /// times over and show the operator a queue of duplicates.
    pub fn push(&self, request: ApprovalRequest) {
        let mut guard = self.inner.lock().expect("approval request queue");
        if guard.iter().any(|q| {
            q.effect.kind == request.effect.kind && q.effect.payload == request.effect.payload
        }) {
            return;
        }
        guard.push(request);
    }

    /// Empties the queue (called before a turn so a request from a prior turn —
    /// or from a workflow run that shares these deps — never leaks into it).
    pub fn clear(&self) {
        self.inner.lock().expect("approval request queue").clear();
    }

    /// Drains up to `cap` queued requests (FIFO) and discards the rest, so one
    /// turn can never flood the operator's queue.
    pub fn drain(&self, cap: usize) -> Vec<ApprovalRequest> {
        let mut guard = self.inner.lock().expect("approval request queue");
        let take = guard.len().min(cap);
        let drained: Vec<ApprovalRequest> = guard.drain(..take).collect();
        guard.clear();
        drained
    }

    /// Builds a queue whose grant set is one the caller already holds.
    ///
    /// The runtime mints and sweeps grants and the policy redeems them, so both
    /// sides must share one set. The builder creates the [`GrantSet`] first
    /// (it is feature-independent, unlike this queue) and hands it in here.
    pub fn with_grants(grants: GrantSet) -> Self {
        Self {
            inner: Arc::default(),
            grants,
        }
    }

    /// The live single-use grant set carried alongside this queue (issue #243).
    pub fn grants(&self) -> GrantSet {
        self.grants.clone()
    }

    /// The number of queued requests.
    ///
    /// Read by a dispatched card **before** its turns run, so it can tell which
    /// of the queue's entries are its own (issue #242) — the queue is shared
    /// with the cycle's chat turns, and [`push`](Self::push) only ever appends,
    /// so a position taken now stays a valid boundary until the cycle-end drain.
    pub fn queued(&self) -> usize {
        self.inner.lock().expect("approval request queue").len()
    }

    /// Stamps `run_id` onto every request queued at or after `from`, returning
    /// how many were stamped (issue #242).
    ///
    /// This is where an approval learns which task attempt is waiting on it. It
    /// happens at the **dispatch** boundary rather than in
    /// [`ApprovalPolicy::effect_for`] because that is the only place the run is
    /// unambiguous: the policy is per-agent and outlives every run, whereas a
    /// dispatched card knows exactly which of the queue's entries its own turns
    /// added. Requests below `from` belong to a chat turn earlier in the same
    /// cycle and are deliberately left `None`.
    pub fn stamp_run(&self, from: usize, run_id: &str) -> usize {
        let mut guard = self.inner.lock().expect("approval request queue");
        let mut stamped = 0;
        for request in guard.iter_mut().skip(from) {
            request.effect.run_id = Some(run_id.to_string());
            stamped += 1;
        }
        stamped
    }
}

/// openhuman [`ToolPolicy`] derived from a company's manifest `[policy]` and a
/// single agent's per-agent budget.
pub struct ApprovalPolicy {
    mode: PolicyMode,
    always_approve: Vec<String>,
    auto_approve_under_usd: Option<f64>,
    /// Per-agent daily spend cap (issue #304). `None` leaves budget enforcement
    /// to the company-wide `[budget]` ceiling.
    ///
    /// Enforced by [`daily_budget_verdict`](Self::daily_budget_verdict) for
    /// priced tool calls, and by the dispatch gate in
    /// [`HarnessPool::run`](crate::harness::HarnessPool::run) for inference. The
    /// cap is only *readable* when a [`spend`](Self::with_spend) reader is
    /// chained on, which only `build_roster` does.
    budget_usd_daily: Option<f64>,
    /// Where "what has this agent spent since UTC midnight" is read from
    /// (issue #304): the company's [`UsageMeter`] plus the company the cap is
    /// scoped to.
    ///
    /// `None` at every non-harness construction site — and, deliberately, on a
    /// host with no meter wired — which makes the budget arm **inert**, so every
    /// existing construction site and test decides exactly as it did before.
    /// Chained by `build_roster` alongside [`with_requests`](Self::with_requests)
    /// / [`with_agent`](Self::with_agent) rather than widening
    /// [`new`](Self::new), for the same reason those are.
    spend: Option<SpendReader>,
    /// Whether the "cap set but no meter to read it with" warning has already
    /// been emitted by this policy instance.
    ///
    /// That condition is a permanent deployment fact, not a transient one, so
    /// warning per priced call would emit a line per tool call for the life of
    /// the process and bury everything else. Once per policy is the useful
    /// signal.
    no_meter_warned: AtomicBool,
    /// Where a `RequireApproval` decision is recorded so the runtime can park it
    /// (issue #172). The default is a private queue nobody drains, which keeps
    /// every non-harness construction site (and every test) behaving exactly as
    /// before; `build_roster` installs the shared one off
    /// [`HarnessDeps`](crate::harness::HarnessDeps).
    requests: ApprovalRequestQueue,
    /// Which roster agent this policy instance is installed on, stamped onto
    /// every projected [`Effect`] so an approval can be re-dispatched to the
    /// agent that asked for it (issue #243).
    ///
    /// `None` for every non-harness construction site, which is what keeps a
    /// policy built outside `build_roster` projecting exactly the effect it
    /// projected before: no agent, so no grant, so the runtime executes it
    /// natively. Only `build_roster` sets it.
    agent: Option<String>,
}

/// Where the per-agent daily spend cap reads today's spend from (issue #304):
/// the company's durable [`UsageMeter`] and the company id to scope the query
/// to.
///
/// Durable on purpose — spend is re-read from the meter (jsonl / sqlite /
/// mongo) on every priced call rather than accumulated in memory, so a restart
/// mid-day resumes against the real figure instead of resetting the cap to
/// zero. A per-turn snapshot cell was considered and rejected: it is stale
/// within the turn it is taken for, and it would share the
/// [`ApprovalRequestQueue::clear`] lifecycle that
/// `grants_survive_a_queue_clear` exists to warn about.
struct SpendReader {
    meter: Arc<dyn UsageMeter>,
    company: CompanyId,
}

impl ApprovalPolicy {
    /// Builds a policy from the manifest `[policy]` block and an agent's
    /// `budget_usd_daily`.
    ///
    /// The signature deliberately does **not** take the agent id: this is the
    /// constructor `build.rs`-generated tests and every non-harness caller use,
    /// and widening it would churn them all to pass a `None` they have no
    /// meaning for. The harness chains [`with_agent`](Self::with_agent) instead,
    /// the same way it already chains [`with_requests`](Self::with_requests).
    pub fn new(policy: &Policy, budget_usd_daily: Option<f64>) -> Self {
        Self {
            mode: PolicyMode::parse(&policy.mode),
            always_approve: policy.always_approve.clone(),
            auto_approve_under_usd: policy.auto_approve_under_usd,
            budget_usd_daily,
            requests: ApprovalRequestQueue::default(),
            agent: None,
            spend: None,
            no_meter_warned: AtomicBool::new(false),
        }
    }

    /// Installs the shared queue every `RequireApproval` decision is recorded on,
    /// so the brain can park the request after the turn (issue #172).
    pub fn with_requests(mut self, requests: ApprovalRequestQueue) -> Self {
        self.requests = requests;
        self
    }

    /// Installs the meter the per-agent daily spend cap is measured against
    /// (issue #304).
    ///
    /// Without this the cap is inert — which is exactly what every non-harness
    /// construction site wants, and what a host with no meter gets.
    pub fn with_spend(mut self, meter: Arc<dyn UsageMeter>, company: CompanyId) -> Self {
        self.spend = Some(SpendReader { meter, company });
        self
    }

    /// Binds this policy to the roster agent it is installed on, so a parked
    /// effect knows whose tool call it came from (issue #243).
    pub fn with_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    /// The resolved tier.
    pub fn mode(&self) -> PolicyMode {
        self.mode
    }

    /// The per-agent daily budget, if any.
    pub fn budget_usd_daily(&self) -> Option<f64> {
        self.budget_usd_daily
    }

    /// Whether `kind` is in the manifest's `always_approve` list. Matches either
    /// the exact dotted kind or a leading segment (so `payment` matches
    /// `payment.send`).
    fn always_requires_approval(&self, kind: &str) -> bool {
        self.always_approve
            .iter()
            .any(|entry| entry == kind || kind.starts_with(&format!("{entry}.")))
    }

    /// Best-effort USD amount carried by a tool call's arguments, from either an
    /// `amount_usd` or `amount` field.
    fn amount_usd(args: &serde_json::Value) -> Option<f64> {
        args.get("amount_usd")
            .or_else(|| args.get("amount"))
            .and_then(|v| v.as_f64())
    }

    /// Project a flagged tool call onto an opencompany [`Effect`] so the runtime
    /// can park it on the [`ApprovalGate`](crate::ports::ApprovalGate). The tool
    /// name becomes the dotted effect `kind`; the group and amount are inferred
    /// best-effort.
    ///
    /// This is the **only** place [`Effect::agent`] is ever stamped, which is
    /// what makes `agent.is_some()` mean precisely "projected from a harness
    /// tool call" everywhere downstream (issue #243). A native effect the
    /// runtime performs itself is built elsewhere and keeps `None`.
    pub fn effect_for(&self, tool_name: &str, args: &serde_json::Value) -> Effect {
        Effect {
            kind: tool_name.to_string(),
            group: classify_group(tool_name),
            amount_usd: Self::amount_usd(args),
            established_thread: false,
            first_time_counterparty: false,
            payload: args.clone(),
            agent: self.agent.clone(),
            run_id: None,
        }
    }

    /// Redeems a live grant for this agent, this tool and these exact arguments
    /// (issue #243), consuming it.
    ///
    /// A policy with no agent bound — every non-harness construction site — can
    /// never match, so it short-circuits before touching the lock and behaves
    /// exactly as it did before grants existed.
    ///
    /// On a near-miss the differing top-level keys are logged. That is the one
    /// diagnostic that matters here: the visible symptom of a mismatch is "I
    /// approved it and the agent asked again", and without this line the operator
    /// and the developer have no way to tell a re-worded argument from a bug in
    /// the grant machinery.
    fn consume_grant(&self, tool: &str, args: &serde_json::Value) -> Option<GrantedCall> {
        let agent = self.agent.as_deref()?;
        let grants = self.requests.grants();
        if let Some(grant) = grants.consume(agent, tool, args) {
            return Some(grant);
        }
        // No exact match. If a grant for this agent+tool exists at all, the
        // arguments drifted — say which keys, without dumping values (arguments
        // carry recipients, bodies and amounts).
        log::debug!(
            "[approval] no grant matched tool '{tool}' for agent '{agent}'; \
             argument keys offered: {:?}",
            top_level_keys(args)
        );
        None
    }

    /// Does this tool call **spend money**? The predicate the daily budget arm
    /// gates on (issue #304).
    ///
    /// Three signals, any of which is enough:
    ///
    /// * the call **declares** an amount (`amount_usd` / `amount`) — the only
    ///   pre-flight signal there is for an x402 payment;
    /// * it is a [metered read](is_metered_read) — `web_search` changes nothing
    ///   but the backend charges per request;
    /// * it projects onto [`EffectGroup::Spend`] — `media_generate_*`,
    ///   `pay_*`/`transfer_*`, and anything else the group classifier already
    ///   calls spend.
    ///
    /// Everything else — a read, a send, a publish, a workspace write — is
    /// **untouched at cap**. A spend cap caps spend; making a teammate unable to
    /// answer a question because it spent its budget this morning would be a
    /// different feature, and a worse one.
    fn is_priced_call(tool: &str, declared_amount: Option<f64>) -> bool {
        declared_amount.is_some()
            || is_metered_read(tool)
            || classify_group(tool) == EffectGroup::Spend
    }

    /// The per-agent daily spend cap (issue #304): `Some(decision)` when this
    /// priced call must not proceed on today's budget, `None` to fall through.
    ///
    /// ## Where this sits, and why
    ///
    /// Below the reserved `never_do` slot, below the `readonly` brake, **below
    /// grant consumption**, below `always_approve` — and **above**
    /// `auto_approve_under_usd` and the mode dispatch.
    ///
    /// Below the grant is the one placement that is not obvious, and it is the
    /// same argument #243 made for putting grants above `always_approve`. A
    /// budget park exists *to ask the operator a question*; the grant is the
    /// operator's answer. Ranking the budget above the grant would mean
    /// approving an at-cap call re-parks it, forever — approval would authorise
    /// nothing for precisely the calls the operator most wants to authorise
    /// deliberately. `readonly` is different and stays on top: it is the
    /// emergency brake, not a question, and consent does not survive it.
    ///
    /// Above `auto_approve_under_usd` because that threshold is a
    /// *per-call* convenience ("don't bother me about anything under $5") and
    /// this is a *per-day* ceiling. Below it, an agent with a $5 cap and a $5
    /// auto-approve threshold could spend $4.99 at a time without limit — the
    /// cap would be unreachable by construction. Above `Full` for the same
    /// reason: full autonomy means "no per-call gate", not "no budget".
    ///
    /// ## At cap it PARKS — never denies, never downgrades
    ///
    /// A hard deny recreates the pre-#172 silent dead-end: openhuman resolves
    /// the refusal inline, the model is told no, and the operator never learns
    /// the company stopped working because one teammate hit its cap. A silent
    /// downgrade to a cheaper path would be worse still — the suppression-shaped
    /// "fix" where monitoring goes quiet and the defect keeps running. Parking
    /// puts the decision in front of the operator, who can approve it (minting a
    /// #243 single-use grant that re-dispatches the call) or leave it.
    ///
    /// ## Failure semantics
    ///
    /// * **No meter wired** — inert, with a one-shot warning. This is a
    ///   permanent deployment fact, not a transient read failure; parking every
    ///   priced call forever would brick every spend tool on a host that simply
    ///   has no meter, and no operator approval would ever clear it.
    /// * **Meter query errored** — **park**, naming the uncertainty. Transient
    ///   uncertainty about money reads as *ask*, not *allow*. This is the
    ///   deliberate opposite of the dispatch gate's fail-open, and the asymmetry
    ///   is the point: there the alternative is bricking the company's cognition
    ///   with no recourse, here the alternative is one call waiting on a human
    ///   who can wave it through.
    ///
    /// ## What the cap does not see
    ///
    /// An **executed** x402 payment. The ledger carries no agent, so there is
    /// nothing to attribute; the pre-flight `declared_amount` check below covers
    /// the call *before* the money moves, and that is the whole of the coverage.
    /// Closing the gap is a store-shape change across three persistence
    /// backends. Documented in `docs/spec/runtime/manifest.md` rather than
    /// papered over here.
    ///
    /// There is also a turn-boundary TOCTOU window: a call that starts under the
    /// cap can finish over it, bounded by one call's cost. The same documented
    /// window `capability_budget` carries; v1 has no reservation ledger.
    async fn daily_budget_verdict(
        &self,
        tool: &str,
        args: &serde_json::Value,
        declared_amount: Option<f64>,
    ) -> Option<ToolPolicyDecision> {
        let cap = self.budget_usd_daily?;
        let agent = self.agent.as_deref()?;
        if !Self::is_priced_call(tool, declared_amount) {
            return None;
        }
        let Some(spend) = self.spend.as_ref() else {
            if !self.no_meter_warned.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "[approval] agent '{agent}' has a daily budget of ${cap:.2} but no usage \
                     meter is wired; the per-agent spend cap cannot be enforced on this host"
                );
            }
            return None;
        };

        let since = utc_day_start_millis(crate::ports::now_millis());
        let samples = match spend.meter.query(&spend.company, since).await {
            Ok(samples) => samples,
            Err(error) => {
                log::warn!(
                    "[approval] could not read agent '{agent}' spend for the daily budget \
                     ({error}); parking '{tool}' rather than spending against an unknown balance"
                );
                return Some(self.require_approval(
                    tool,
                    args,
                    format!(
                        "'{tool}' spends money and {agent}'s daily budget could not be verified \
                         right now"
                    ),
                ));
            }
        };

        let spent = usd_spent_by_agent(&samples, agent);
        let amount = declared_amount.unwrap_or(0.0);
        // Two boundaries: already at/over the cap (`>=`, matching every other
        // budget gate in the crate), or a declared amount that would carry this
        // agent past it. A call with no declared amount only trips the first —
        // its cost is unknowable until it runs.
        if spent < cap && spent + amount <= cap {
            return None;
        }

        let reason = if spent >= cap {
            format!(
                "'{tool}' spends money and {agent} has used ${spent:.2} of its ${cap:.2} daily \
                 budget; approve to let this one call through"
            )
        } else {
            format!(
                "'{tool}' would spend ${amount:.2}, carrying {agent} past its ${cap:.2} daily \
                 budget (${spent:.2} used so far); approve to let this one call through"
            )
        };
        Some(self.require_approval(tool, args, reason))
    }

    /// The one construction site for a `RequireApproval` decision (issue #172):
    /// record the projected effect on the shared queue so the brain can park it
    /// after the turn, then return the decision openhuman blocks the call with.
    ///
    /// Every `RequireApproval` arm of [`check`](ToolPolicy::check) goes through
    /// here — a decision that skipped it would refuse the tool without ever
    /// reaching the operator, which is exactly the bug this closes.
    fn require_approval(
        &self,
        tool: &str,
        args: &serde_json::Value,
        reason: String,
    ) -> ToolPolicyDecision {
        self.requests.push(ApprovalRequest {
            tool: tool.to_string(),
            reason: reason.clone(),
            effect: self.effect_for(tool, args),
        });
        log::debug!(
            "[approval] tool '{tool}' requires operator approval — queued to park ({reason})"
        );
        ToolPolicyDecision::require_approval(reason)
    }
}

#[async_trait]
impl ToolPolicy for ApprovalPolicy {
    fn name(&self) -> &str {
        "opencompany-approval"
    }

    async fn check(&self, request: &ToolPolicyRequest) -> ToolPolicyDecision {
        let tool = request.tool_name.as_str();

        // 0. `never_do` hard-deny — RESERVED SLOT, deliberately empty.
        //
        // The manifest's `never_do` list is compiled by the delegation-rule
        // compiler, which is still a Phase-1 stub, and today it is only consulted
        // by `ManifestApprovalGate::evaluate` (`src/policy/gate.rs`). That gate
        // never sees a harness tool call: the harness path parks via
        // `CycleHostImpl::park` → `gate.park()`, which bypasses `evaluate`
        // entirely, so the two gates sit on disjoint paths. When the compiler
        // lands it must emit a tool-level arm HERE, ABOVE the grant check —
        // a never-do tool must be refused even holding a grant, because a grant
        // is an operator saying "yes to this call" and `never_do` is the company
        // saying "not this, ever". Precedence between them is not a detail: an
        // operator can be socially engineered, and the standing rule is the thing
        // that is supposed to survive that.
        //
        // Adding the arm below the grant check would silently invert that.

        // 1. `readonly` outranks a grant — the brake wins (issue #243).
        //
        // A grant can be up to `GRANT_TTL_MILLIS` old when it is redeemed, so a
        // company can be switched to `readonly` in the window between the
        // operator approving a call and the agent re-issuing it. Switching to
        // `readonly` is the emergency stop, and that window is exactly when
        // someone means it: the tier's contract is that nothing mutates and
        // nothing reaches outside, and an approval given under a laxer mode is
        // the older instruction. Consent does not survive the brake.
        //
        // Scoped deliberately to `readonly` and to external effects — the same
        // condition the mode arm below denies on. `supervised` and `full` still
        // fall through to the grant, because bypassing `supervised`'s re-park is
        // the entire point of a grant.
        //
        // The grant is left UNCONSUMED here: this call never ran, so the
        // operator's approval stays redeemable if the brake is released inside
        // the TTL. It expires on its own otherwise.
        if self.mode == PolicyMode::Readonly && is_external_effect(tool) {
            return ToolPolicyDecision::deny(format!(
                "'{tool}' mutates or reaches outside; this desk is read-only, \
                 so an earlier approval does not apply"
            ));
        }

        // 2. A live single-use grant: the operator already approved exactly this
        //    call, so let it through — once (issue #243).
        //
        // ABOVE `always_requires_approval` on purpose. A tool on the
        // `always_approve` list still parks the FIRST time, which is what that
        // list is for; but once the operator has said yes to that specific call,
        // re-parking it would mean approval never actually authorises anything.
        // The blast radius stays small because a grant is agent-scoped,
        // argument-exact and single-use: redeeming it consumes it, so the very
        // next call to the same tool parks again.
        if let Some(grant) = self.consume_grant(tool, &request.arguments) {
            log::debug!(
                "[approval] tool '{tool}' allowed by single-use grant {} for agent '{}'",
                grant.approval_id,
                grant.agent
            );
            return ToolPolicyDecision::Allow;
        }

        // 2. `always_approve` wins over everything else, including Full autonomy.
        if self.always_requires_approval(tool) {
            return self.require_approval(
                tool,
                &request.arguments,
                format!("'{tool}' is in the company's always-approve list"),
            );
        }

        let declared_amount = Self::amount_usd(&request.arguments);

        // 3. The per-agent daily spend cap (issue #304). ABOVE
        //    `auto_approve_under_usd` and the mode dispatch, so neither a
        //    sub-threshold trickle nor `full` autonomy can spend past a cap the
        //    manifest set. See `daily_budget_verdict` for the full ordering
        //    argument and the park-don't-deny reasoning.
        if let Some(decision) = self
            .daily_budget_verdict(tool, &request.arguments, declared_amount)
            .await
        {
            return decision;
        }

        // Auto-approve small spends under the configured threshold.
        if let (Some(threshold), Some(amount)) = (self.auto_approve_under_usd, declared_amount)
            && amount < threshold
        {
            return ToolPolicyDecision::Allow;
        }

        let external = is_external_effect(tool);
        // A *metered read* (issue #238) is external — it reaches a third party
        // and spends real money — but it changes nothing anywhere, so under
        // `supervised` the consent already given at grant time is enough and
        // the daily call cap is the boundary. Under `readonly` it is still
        // denied: that tier's contract is that nothing is spent.
        let metered_read = is_metered_read(tool);
        match self.mode {
            PolicyMode::Full => ToolPolicyDecision::Allow,
            PolicyMode::Supervised => {
                if external && !metered_read {
                    self.require_approval(
                        tool,
                        &request.arguments,
                        format!("'{tool}' has an external effect and this desk runs supervised"),
                    )
                } else {
                    ToolPolicyDecision::Allow
                }
            }
            PolicyMode::Readonly => {
                if external {
                    ToolPolicyDecision::deny(format!(
                        "'{tool}' mutates or reaches outside; this desk is read-only"
                    ))
                } else {
                    ToolPolicyDecision::Allow
                }
            }
        }
    }
}

/// Does this tool reach outside and spend money, while changing nothing?
///
/// A third bucket the original binary could not express, added for `web_search`
/// (issue #238). The existing classifier conflates two questions —
/// *does it change anything / reach a counterparty* and *does it cost money* —
/// which is fine while every tool answers both the same way. `media_list_models`
/// is free, so waving it through in every mode is safe.
/// [`crate::harness::search`]'s `web_search` is not: the backend charges per
/// request.
///
/// The two modes therefore want different answers, and this is what lets them
/// have them:
///
/// * **`supervised`** — allowed. The issue's #188 sign-off is explicit that
///   individual searches must not park: consent happens once, at grant time (an
///   explicit `search` grant a `*` cannot confer), and the per-company daily
///   cap is the boundary. Parking each call would also be *useless* here rather
///   than merely annoying — openhuman resolves a `RequireApproval` inline and
///   never re-dispatches the call (see the module docs), so a parked search is
///   a search that never happens, and an agent with no search does the exact
///   thing this feature exists to stop: it invents citations.
/// * **`readonly`** — still denied, via the ordinary external-effect path.
///   A read-only desk is one that spends nothing, and this spends. That is a
///   deliberate divergence from the issue text, which would have made a paid
///   call unstoppable in the one tier whose whole promise is that nothing
///   moves.
///
/// An operator who *does* want a per-call gate has one: `[policy].always_approve
/// = ["web_search"]` wins over every tier, including `full`.
fn is_metered_read(tool_name: &str) -> bool {
    tool_name.eq_ignore_ascii_case(crate::harness::search::WEB_SEARCH_TOOL)
}

/// Heuristic: does this tool mutate state or reach an external counterparty?
///
/// Best-effort classification by name — openhuman's [`ToolPolicy`] surface hands
/// the bridge only the tool name and arguments, not the tool's own
/// external-effect flag. Unknown tools are treated as external (fail-safe).
fn is_external_effect(tool_name: &str) -> bool {
    // The orchestrator's in-cycle delegation tools (`spawn_task`,
    // `delegate_to_desk`) enqueue internal work the harness brain drains this
    // turn — a task card or a hand-off to a desk's lead — never an external
    // effect. Without this, the default `supervised` policy would park them and
    // `readonly` would deny them, breaking in-cycle delegation. (Issue #53.)
    if crate::harness::orchestrator::is_delegation_tool(tool_name) {
        return false;
    }
    // An MCP tool call can perform any effect advertised by a third-party
    // server. Treat it as external even if future prefix rules become broader.
    if tool_name.eq_ignore_ascii_case("mcp_registry_tool_call") {
        return true;
    }
    // The media catalog is a read-only GET (issue #109): listing models spends
    // nothing and must never park for approval, even though its name does not
    // start with a read-only prefix. The `media_generate_*` tools are NOT listed
    // here — they spend real money and fall through to the external-effect
    // default, so they park under supervised / deny under readonly.
    if tool_name.eq_ignore_ascii_case("media_list_models") {
        return false;
    }
    // The Composio read tools (issue #110) are read-only GETs: listing toolkits,
    // connections, or action schemas reaches no third party and must never park
    // for approval, even though the `composio_*` name has no read-only prefix.
    // `composio_authorize` / `composio_execute` are NOT listed here — they begin
    // an OAuth handoff / run a real action, so they fall through to the external-
    // effect default (park under supervised, deny under readonly).
    if matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "composio_list_toolkits" | "composio_list_connections" | "composio_list_tools"
    ) {
        return false;
    }
    // The company-workspace read tools (issue #237) only read this company's
    // own note tree — no state changes and no counterparty is reached — but
    // their names begin with the namespace rather than a read-only prefix, so
    // without this arm the fail-safe default would park them under the DEFAULT
    // `supervised` mode and deny them outright under `readonly`. That would
    // make the workspace unreadable in exactly the mode whose point is that
    // reads are fine.
    //
    // `workspace_write` is deliberately NOT listed: it overwrites
    // operator-owned guidance, so it falls through to the external-effect
    // default and parks under supervised / is denied under readonly. That —
    // not the tool's declared `PermissionLevel::Write` — is what gates the
    // write; openhuman's `ToolPolicy` surface never sees a permission level.
    // The tests below pin all three classifications so renaming a tool cannot
    // silently move it across the gate.
    if matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "workspace_list" | "workspace_read"
    ) {
        return false;
    }
    // `web_search` (issue #238) is deliberately NOT carved out here. It reaches
    // a third party and the managed backend charges for it, so it must stay on
    // the external side and be DENIED under `readonly`. What keeps it from
    // parking under `supervised` is `is_metered_read`, which is a narrower claim
    // ("costs money but changes nothing") than the one this function answers.
    // Note also that it would not have matched a read-only prefix by accident:
    // the list below starts `search`, but the tool is `web_search`.
    const READ_ONLY_PREFIXES: &[&str] = &[
        "read",
        "list",
        "get",
        "search",
        "recall",
        "query",
        "peek",
        "inspect",
        "view",
        "memory_recall",
        "memory_search",
    ];
    let name = tool_name.to_ascii_lowercase();
    !READ_ONLY_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The top-level keys of a tool-argument object, for a mismatch diagnostic.
/// Keys only — values carry recipients, bodies and amounts.
fn top_level_keys(args: &serde_json::Value) -> Vec<&str> {
    match args {
        serde_json::Value::Object(map) => map.keys().map(String::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Map a tool name onto the supervised [`EffectGroup`] taxonomy.
fn classify_group(tool_name: &str) -> EffectGroup {
    let name = tool_name.to_ascii_lowercase();
    if name == "mcp_registry_tool_call" {
        EffectGroup::Other
    } else if name == "composio_authorize" {
        // Beginning an OAuth handoff establishes an account identity for the
        // company (issue #110) — an identity effect, parked before it lands.
        EffectGroup::Identity
    } else if name == "composio_execute" {
        // Running a Composio action reaches a third-party account (send an
        // email, post a message, open a PR) — a send effect. Placed before the
        // generic `contains` heuristics so the slug can't be misclassified.
        EffectGroup::Send
    } else if name == crate::harness::search::WEB_SEARCH_TOOL {
        // A metered search is billed by the backend per request (issue #238), so
        // when it *does* park — an operator listing it in `always_approve` — the
        // Approvals page must say "spend", not the catch-all "other". An
        // operator approving a paid call deserves to be told it is one.
        EffectGroup::Spend
    } else if name.starts_with("media_generate") {
        // Image/video generation is billed by the backend on submit (issue
        // #109), so it is a spend effect — parked for approval before money
        // moves. (`media_list_models` is read-only and never reaches here.)
        EffectGroup::Spend
    } else if name.contains("pay") || name.contains("transfer") || name.starts_with("spend") {
        EffectGroup::Spend
    } else if name.contains("email") || name.contains("send") || name.contains("message") {
        EffectGroup::Send
    } else if name.contains("sign") || name.contains("file") {
        EffectGroup::Sign
    } else if name.contains("publish") || name.contains("post") {
        EffectGroup::Publish
    } else if name.contains("hire") || name.contains("contract") {
        EffectGroup::Hire
    } else if name.contains("identity") || name.contains("handle") {
        EffectGroup::Identity
    } else {
        EffectGroup::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oh::agent::tool_policy::{ToolCallContext, ToolPolicyRequest};

    fn policy(mode: &str, always: &[&str], auto_under: Option<f64>) -> ApprovalPolicy {
        let p = Policy {
            mode: mode.to_string(),
            always_approve: always.iter().map(|s| s.to_string()).collect(),
            auto_approve_under_usd: auto_under,
        };
        ApprovalPolicy::new(&p, Some(25.0))
    }

    fn request(tool: &str, args: serde_json::Value) -> ToolPolicyRequest {
        let ctx = ToolCallContext::session("s", "chat", "ceo", "call-1", 0);
        ToolPolicyRequest::new(tool, args, ctx)
    }

    #[test]
    fn mode_maps_one_to_one_to_security_tiers() {
        assert_eq!(PolicyMode::parse("readonly").security_tier(), "readonly");
        assert_eq!(
            PolicyMode::parse("supervised").security_tier(),
            "supervised"
        );
        assert_eq!(PolicyMode::parse("full").security_tier(), "full");
        // Unknown falls back to supervised.
        assert_eq!(PolicyMode::parse("bogus"), PolicyMode::Supervised);
    }

    #[tokio::test]
    async fn full_allows_but_always_approve_still_parks() {
        let p = policy("full", &["payment"], None);
        assert_eq!(
            p.check(&request("write_file", serde_json::json!({}))).await,
            ToolPolicyDecision::Allow
        );
        assert!(matches!(
            p.check(&request("payment.send", serde_json::json!({})))
                .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }

    #[tokio::test]
    async fn supervised_requires_approval_for_external_effects() {
        let p = policy("supervised", &[], None);
        assert!(matches!(
            p.check(&request("send_email", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(
            p.check(&request("read_file", serde_json::json!({}))).await,
            ToolPolicyDecision::Allow
        );
    }

    #[tokio::test]
    async fn supervised_parks_mcp_tool_calls_as_external_other_effects() {
        let p = policy("supervised", &[], None);
        let args = serde_json::json!({
            "server_id": "server-1",
            "tool_name": "echo",
            "arguments": {"text": "hello"}
        });
        assert!(matches!(
            p.check(&request("mcp_registry_tool_call", args.clone()))
                .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(
            p.effect_for("mcp_registry_tool_call", &args).group,
            EffectGroup::Other
        );
    }

    #[tokio::test]
    async fn readonly_denies_mutations_allows_reads() {
        let p = policy("readonly", &[], None);
        assert!(matches!(
            p.check(&request("publish_post", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Deny { .. }
        ));
        assert_eq!(
            p.check(&request("list_files", serde_json::json!({}))).await,
            ToolPolicyDecision::Allow
        );
    }

    #[tokio::test]
    async fn auto_approve_under_threshold_allows_small_spends() {
        let p = policy("supervised", &[], Some(5.0));
        // $3 spend is under the $5 threshold → allowed even though it's external.
        assert_eq!(
            p.check(&request(
                "pay_invoice",
                serde_json::json!({ "amount_usd": 3.0 })
            ))
            .await,
            ToolPolicyDecision::Allow
        );
        // $9 spend exceeds the threshold → requires approval.
        assert!(matches!(
            p.check(&request(
                "pay_invoice",
                serde_json::json!({ "amount_usd": 9.0 })
            ))
            .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }

    /// Media generation (issue #109): the paid `media_generate_*` tools park
    /// under supervised and deny under readonly (external spend effect), while
    /// the read-only `media_list_models` catalog GET is always allowed.
    #[tokio::test]
    async fn media_generate_parks_supervised_and_denies_readonly_but_list_is_read_only() {
        let supervised = policy("supervised", &[], None);
        for tool in ["media_generate_image", "media_generate_video"] {
            assert!(
                matches!(
                    supervised
                        .check(&request(tool, serde_json::json!({})))
                        .await,
                    ToolPolicyDecision::RequireApproval { .. }
                ),
                "{tool} must park under supervised"
            );
        }
        // The catalog GET is read-only — allowed even under supervised.
        assert_eq!(
            supervised
                .check(&request("media_list_models", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );

        let readonly = policy("readonly", &[], None);
        assert!(
            matches!(
                readonly
                    .check(&request("media_generate_image", serde_json::json!({})))
                    .await,
                ToolPolicyDecision::Deny { .. }
            ),
            "media_generate must be denied under readonly"
        );
        // Even a read-only desk can list the model catalog.
        assert_eq!(
            readonly
                .check(&request("media_list_models", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );
    }

    /// Paid generation classifies as a spend effect (issue #109).
    #[test]
    fn media_generate_classifies_as_spend() {
        let p = policy("supervised", &[], None);
        assert_eq!(
            p.effect_for("media_generate_image", &serde_json::json!({}))
                .group,
            EffectGroup::Spend
        );
        assert_eq!(
            p.effect_for("media_generate_video", &serde_json::json!({}))
                .group,
            EffectGroup::Spend
        );
    }

    /// Per-tenant Composio (issue #110): the read tools are read-only (allowed
    /// even under supervised/readonly), while `composio_authorize` /
    /// `composio_execute` are external — parked under supervised, denied under
    /// readonly.
    #[tokio::test]
    async fn composio_reads_allowed_but_authorize_execute_park_or_deny() {
        let supervised = policy("supervised", &[], None);
        for tool in [
            "composio_list_toolkits",
            "composio_list_connections",
            "composio_list_tools",
        ] {
            assert_eq!(
                supervised
                    .check(&request(tool, serde_json::json!({})))
                    .await,
                ToolPolicyDecision::Allow,
                "{tool} is read-only and must be allowed"
            );
        }
        for tool in ["composio_authorize", "composio_execute"] {
            assert!(
                matches!(
                    supervised
                        .check(&request(tool, serde_json::json!({})))
                        .await,
                    ToolPolicyDecision::RequireApproval { .. }
                ),
                "{tool} must park under supervised"
            );
        }

        let readonly = policy("readonly", &[], None);
        // A read-only desk may still browse the Composio surface.
        assert_eq!(
            readonly
                .check(&request("composio_list_connections", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );
        for tool in ["composio_authorize", "composio_execute"] {
            assert!(
                matches!(
                    readonly.check(&request(tool, serde_json::json!({}))).await,
                    ToolPolicyDecision::Deny { .. }
                ),
                "{tool} must be denied under readonly"
            );
        }
    }

    /// Composio effect groups (issue #110): authorize is an Identity effect,
    /// execute is a Send effect — pinned before the generic `contains`
    /// heuristics could misclassify the slug.
    #[test]
    fn composio_classifies_authorize_identity_and_execute_send() {
        let p = policy("supervised", &[], None);
        assert_eq!(
            p.effect_for("composio_authorize", &serde_json::json!({}))
                .group,
            EffectGroup::Identity
        );
        assert_eq!(
            p.effect_for("composio_execute", &serde_json::json!({}))
                .group,
            EffectGroup::Send
        );
    }

    /// Metered web search (issue #238): allowed under `supervised`, DENIED
    /// under `readonly`, allowed under `full`.
    ///
    /// This is the classification decision the issue got wrong in both
    /// directions, so it is pinned here rather than left to the heuristic:
    ///
    /// * It must **not park** under `supervised`. openhuman resolves a
    ///   `RequireApproval` inline and never re-dispatches the call (module
    ///   docs), so parking a search is not "the operator approves it later" —
    ///   it is "the search never happens", leaving the agent in exactly the
    ///   no-discovery state that makes it invent citations. Consent is the
    ///   explicit `search` grant; the boundary is the daily cap.
    /// * It must **still be denied** under `readonly`. The issue proposed a
    ///   flat carve-out, which would have made a paid outbound call
    ///   unstoppable in the one tier whose entire promise is that nothing is
    ///   spent. `web_fetch`, its sibling in the same research loop, is denied
    ///   there; a *priced* discovery call has no business being more permissive
    ///   than the free retrieval call it feeds.
    #[tokio::test]
    async fn web_search_never_parks_under_supervised_but_is_denied_read_only() {
        let supervised = policy("supervised", &[], None);
        assert_eq!(
            supervised
                .check(&request(
                    "web_search",
                    serde_json::json!({ "query": "acme pricing" })
                ))
                .await,
            ToolPolicyDecision::Allow,
            "a search must not park: a parked search is a search that never runs"
        );
        assert_eq!(
            supervised.requests.queued(),
            0,
            "an allowed search must not queue an approval request"
        );

        // Its sibling in the same research loop still parks — searching is
        // carved out because it is a *read*, not because `web` was relaxed.
        assert!(
            matches!(
                supervised
                    .check(&request(
                        "web_fetch",
                        serde_json::json!({ "url": "https://a.test/" })
                    ))
                    .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "web_fetch must still park under supervised"
        );

        let readonly = policy("readonly", &[], None);
        assert!(
            matches!(
                readonly
                    .check(&request(
                        "web_search",
                        serde_json::json!({ "query": "acme" })
                    ))
                    .await,
                ToolPolicyDecision::Deny { .. }
            ),
            "a read-only desk spends nothing, and a search spends"
        );

        let full = policy("full", &[], None);
        assert_eq!(
            full.check(&request(
                "web_search",
                serde_json::json!({ "query": "acme" })
            ))
            .await,
            ToolPolicyDecision::Allow
        );
    }

    /// The company workspace (issue #237): the two read tools reach only this
    /// company's own note tree and must be allowed in every mode, while
    /// `workspace_write` overwrites operator-owned guidance and must park under
    /// supervised / be denied under readonly.
    ///
    /// This is the ACTUAL gate on a workspace write. Issue #237 proposed that
    /// declaring `PermissionLevel::Write` would keep the `ApprovalPolicy` as
    /// the per-call gate; it would not — openhuman's `ToolPolicy` surface hands
    /// this bridge only the tool name and args, never the tool's permission
    /// level, so classification is by name alone. Pinning all three names here
    /// is what stops a later rename (say `get_workspace_note`, which the
    /// read-only prefix list would silently wave through) from moving a tool
    /// across the gate unnoticed.
    #[tokio::test]
    async fn workspace_reads_are_allowed_but_writes_park_or_deny() {
        let supervised = policy("supervised", &[], None);
        for tool in ["workspace_list", "workspace_read"] {
            assert_eq!(
                supervised
                    .check(&request(tool, serde_json::json!({})))
                    .await,
                ToolPolicyDecision::Allow,
                "{tool} only reads this company's own workspace and must be allowed"
            );
        }
        assert!(
            matches!(
                supervised
                    .check(&request("workspace_write", serde_json::json!({})))
                    .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "workspace_write must park under supervised"
        );

        let readonly = policy("readonly", &[], None);
        for tool in ["workspace_list", "workspace_read"] {
            assert_eq!(
                readonly.check(&request(tool, serde_json::json!({}))).await,
                ToolPolicyDecision::Allow,
                "{tool} must stay available to a read-only desk"
            );
        }
        assert!(
            matches!(
                readonly
                    .check(&request("workspace_write", serde_json::json!({})))
                    .await,
                ToolPolicyDecision::Deny { .. }
            ),
            "workspace_write must be denied under readonly"
        );

        // Under `full` there is no per-call gate at all — which is precisely
        // why `workspace_write` carries a required `expected_updated_at`
        // compare-and-swap token of its own.
        let full = policy("full", &[], None);
        assert_eq!(
            full.check(&request("workspace_write", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );
    }

    /// The operator's escape hatch: `always_approve` wins over every tier, so a
    /// company that *does* want to eyeball each paid search can have that —
    /// and the parked request is projected as a **spend**, not the catch-all
    /// "other", so the Approvals page says what is being approved.
    #[tokio::test]
    async fn an_operator_can_still_force_approval_on_each_search() {
        let policy = policy("supervised", &["web_search"], None);
        assert!(
            matches!(
                policy
                    .check(&request(
                        "web_search",
                        serde_json::json!({ "query": "acme" })
                    ))
                    .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "`always_approve` must override the metered-read carve-out"
        );
        assert_eq!(
            policy
                .effect_for("web_search", &serde_json::json!({}))
                .group,
            EffectGroup::Spend,
            "a paid call must not park as an unlabelled `Other`"
        );
    }

    #[test]
    fn effect_projection_infers_group_and_amount() {
        let p = policy("supervised", &[], None);
        let effect = p.effect_for("pay_invoice", &serde_json::json!({ "amount_usd": 12.5 }));
        assert_eq!(effect.kind, "pay_invoice");
        assert_eq!(effect.group, EffectGroup::Spend);
        assert_eq!(effect.amount_usd, Some(12.5));
    }

    // --- The park queue (issue #172) ----------------------------------------

    fn queued_policy(mode: &str, always: &[&str]) -> (ApprovalPolicy, ApprovalRequestQueue) {
        let queue = ApprovalRequestQueue::default();
        (
            policy(mode, always, None).with_requests(queue.clone()),
            queue,
        )
    }

    /// The core of #172: a `RequireApproval` decision no longer evaporates into
    /// the model's transcript — it is recorded, with the call projected onto the
    /// effect the operator will see, so the runtime can park it.
    #[tokio::test]
    async fn require_approval_records_the_request_to_park() {
        let (p, queue) = queued_policy("supervised", &[]);
        let args = serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" });
        assert!(matches!(
            p.check(&request("composio_execute", args.clone())).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));

        let queued = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(queued.len(), 1, "the gated call was recorded");
        assert_eq!(queued[0].tool, "composio_execute");
        assert_eq!(queued[0].effect.kind, "composio_execute");
        assert_eq!(queued[0].effect.group, EffectGroup::Send);
        assert_eq!(queued[0].effect.payload, args);
        assert!(
            queued[0].reason.contains("supervised"),
            "the operator-facing reason rides along: {}",
            queued[0].reason
        );
    }

    /// `always_approve` parks regardless of tier — including under `full` — so
    /// that arm has to record its request too.
    #[tokio::test]
    async fn always_approve_records_the_request_even_under_full_autonomy() {
        let (p, queue) = queued_policy("full", &["payment"]);
        assert!(matches!(
            p.check(&request(
                "payment.send",
                serde_json::json!({ "amount_usd": 40.0 })
            ))
            .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        let queued = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].effect.kind, "payment.send");
        assert_eq!(queued[0].effect.amount_usd, Some(40.0));
    }

    /// Allowed and denied calls leave the queue alone: only a call actually
    /// waiting on the operator may reach the Approvals page.
    #[tokio::test]
    async fn allow_and_deny_record_nothing() {
        let (supervised, allow_queue) = queued_policy("supervised", &[]);
        assert_eq!(
            supervised
                .check(&request("read_file", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );
        assert_eq!(allow_queue.queued(), 0, "an allowed call parks nothing");

        let (readonly, deny_queue) = queued_policy("readonly", &[]);
        assert!(matches!(
            readonly
                .check(&request("publish_post", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Deny { .. }
        ));
        assert_eq!(
            deny_queue.queued(),
            0,
            "a denied call is refused outright, never parked"
        );
    }

    /// openhuman blocks a gated call but lets the turn continue, so a model that
    /// keeps re-trying the same tool must not stack up duplicate approvals.
    #[tokio::test]
    async fn a_retried_call_is_recorded_once() {
        let (p, queue) = queued_policy("supervised", &[]);
        let args = serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" });
        for _ in 0..3 {
            let _ = p.check(&request("composio_execute", args.clone())).await;
        }
        assert_eq!(queue.queued(), 1, "the same call parks once");

        // A different call to the same tool is a distinct request.
        let _ = p
            .check(&request(
                "composio_execute",
                serde_json::json!({ "tool_slug": "SLACK_POST" }),
            ))
            .await;
        assert_eq!(queue.queued(), 2);
    }

    /// The drain is capped, so a runaway turn can't flood the operator's queue.
    #[tokio::test]
    async fn the_drain_is_capped_and_empties_the_queue() {
        let (p, queue) = queued_policy("supervised", &[]);
        for i in 0..(MAX_APPROVAL_REQUESTS_PER_TURN + 4) {
            let _ = p
                .check(&request(
                    "composio_execute",
                    serde_json::json!({ "tool_slug": format!("TOOL_{i}") }),
                ))
                .await;
        }
        let drained = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(drained.len(), MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(queue.queued(), 0, "the overflow is discarded, not carried");
    }

    // --- Redeeming a grant (issue #243) --------------------------------------

    /// A policy bound to `agent`, plus the grant set its queue carries.
    fn granting_policy(
        mode: &str,
        always: &[&str],
        agent: &str,
    ) -> (ApprovalPolicy, crate::runtime::grants::GrantSet) {
        let queue = ApprovalRequestQueue::default();
        let grants = queue.grants();
        (
            policy(mode, always, None)
                .with_requests(queue)
                .with_agent(agent),
            grants,
        )
    }

    fn granted(
        agent: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> crate::runtime::grants::GrantedCall {
        crate::runtime::grants::GrantedCall {
            approval_id: crate::ports::types::ApprovalId::new("appr-1"),
            agent: agent.to_string(),
            tool: tool.to_string(),
            args,
            at_millis: 1_000,
        }
    }

    /// The point of the whole feature: a call the operator approved actually
    /// runs, instead of parking a second time.
    #[tokio::test]
    async fn a_granted_call_is_allowed_once_and_then_parks_again() {
        let (p, grants) = granting_policy("supervised", &[], "finance");
        let args = serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" });
        grants.grant(granted("finance", "composio_execute", args.clone()));

        assert_eq!(
            p.check(&request("composio_execute", args.clone())).await,
            ToolPolicyDecision::Allow,
            "the operator approved this exact call; it must run"
        );
        // Single-use: the next identical call has no grant left and parks.
        assert!(
            matches!(
                p.check(&request("composio_execute", args)).await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "one approval buys one call, not standing permission"
        );
    }

    /// A grant is consumed **above** `always_approve`.
    ///
    /// This ordering is the one real judgement call in the change. Leaving
    /// `always_approve` on top reads safer, and is in fact incoherent: a tool on
    /// that list would park, the operator would approve it, and it would park
    /// again forever. Approval would authorise nothing at all for precisely the
    /// tools the operator most wants to authorise deliberately. Single-use +
    /// exact-args + agent-scope is what keeps the widened path narrow.
    #[tokio::test]
    async fn a_grant_beats_always_approve_but_only_for_that_one_call() {
        let (p, grants) = granting_policy("full", &["payment"], "finance");
        let args = serde_json::json!({ "amount_usd": 40.0 });

        // Without a grant, `always_approve` parks it even under full autonomy.
        assert!(matches!(
            p.check(&request("payment.send", args.clone())).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));

        grants.grant(granted("finance", "payment.send", args.clone()));
        assert_eq!(
            p.check(&request("payment.send", args.clone())).await,
            ToolPolicyDecision::Allow
        );
        // And the list reasserts itself immediately afterwards.
        assert!(matches!(
            p.check(&request("payment.send", args)).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }

    /// A grant minted for one agent does not admit another agent's identical
    /// call. The operator approved a specific desk's request, not the action in
    /// the abstract.
    #[tokio::test]
    async fn a_grant_does_not_travel_to_another_agent() {
        let (marketing, grants) = granting_policy("supervised", &[], "marketing");
        let args = serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" });
        // The grant belongs to `finance`.
        grants.grant(granted("finance", "composio_execute", args.clone()));

        assert!(
            matches!(
                marketing
                    .check(&request("composio_execute", args.clone()))
                    .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "another agent's grant must not admit this call"
        );
        // ...and the near-miss did not burn finance's grant.
        assert_eq!(grants.live_count(), 1);
    }

    /// Re-issuing with different arguments re-parks rather than riding the
    /// grant.
    ///
    /// This is the security boundary of the feature. If matching were on the
    /// tool name alone, a model that came back with a larger amount or a
    /// different recipient would execute it under an approval the operator gave
    /// for something else entirely — the operator would have authorised a $40
    /// payment and funded a $4,000 one.
    #[tokio::test]
    async fn drifted_arguments_re_park_instead_of_riding_the_grant() {
        let (p, grants) = granting_policy("supervised", &[], "finance");
        grants.grant(granted(
            "finance",
            "pay_invoice",
            serde_json::json!({ "amount_usd": 40.0, "to": "acme" }),
        ));

        for drifted in [
            serde_json::json!({ "amount_usd": 4000.0, "to": "acme" }),
            serde_json::json!({ "amount_usd": 40.0, "to": "someone-else" }),
            serde_json::json!({ "amount_usd": 40.0 }),
            serde_json::json!({ "amount_usd": 40.0, "to": "acme", "memo": "extra" }),
        ] {
            assert!(
                matches!(
                    p.check(&request("pay_invoice", drifted.clone())).await,
                    ToolPolicyDecision::RequireApproval { .. }
                ),
                "arguments the operator never saw must re-park: {drifted}"
            );
        }
        // Every near-miss left the grant intact for the genuine call.
        assert_eq!(grants.live_count(), 1);
        assert_eq!(
            p.check(&request(
                "pay_invoice",
                serde_json::json!({ "amount_usd": 40.0, "to": "acme" })
            ))
            .await,
            ToolPolicyDecision::Allow
        );
    }

    /// A grant cannot rescue a tool the tier denies outright.
    ///
    /// Deliberate: this arm is reachable only if an approval was parked under a
    /// permissive tier and the company was moved to `readonly` before it was
    /// resolved. `readonly` promises nothing is spent and nothing moves, and a
    /// stale grant must not be a hole in that promise.
    #[tokio::test]
    async fn a_grant_does_not_override_a_readonly_desk() {
        let (p, grants) = granting_policy("readonly", &[], "finance");
        let args = serde_json::json!({ "to": "a@b.test" });
        grants.grant(granted("finance", "publish_post", args.clone()));

        // `readonly` outranks the grant. A grant can be up to its TTL old, so the
        // company may have been switched to `readonly` between the operator
        // approving this call and the agent re-issuing it — and that switch is
        // the emergency stop. The tier's contract wins over the older consent.
        assert!(
            matches!(
                p.check(&request("publish_post", args.clone())).await,
                ToolPolicyDecision::Deny { .. }
            ),
            "a live grant must not survive the readonly brake"
        );

        // And the grant was NOT consumed by that denial — the call never ran, so
        // the operator's approval is still redeemable if the brake comes off
        // inside the TTL.
        assert!(
            grants
                .peek(&crate::ports::types::ApprovalId::new("appr-1"))
                .is_some(),
            "a denied call must not burn the grant it never used"
        );

        // Anything the operator did NOT approve is still denied outright.
        assert!(matches!(
            p.check(&request("publish_post", serde_json::json!({ "other": 1 })))
                .await,
            ToolPolicyDecision::Deny { .. }
        ));
    }

    /// A policy with no agent bound — every non-harness construction site —
    /// never consults the grant set at all.
    #[tokio::test]
    async fn an_unbound_policy_ignores_grants_entirely() {
        let queue = ApprovalRequestQueue::default();
        let grants = queue.grants();
        let p = policy("supervised", &[], None).with_requests(queue);
        let args = serde_json::json!({ "to": "a@b.test" });
        // A grant naming *some* agent exists, but this policy is bound to none.
        grants.grant(granted("finance", "send_email", args.clone()));

        assert!(matches!(
            p.check(&request("send_email", args)).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(grants.live_count(), 1, "the grant was never touched");
    }

    /// Issue #243, and the single most fragile thing about riding the grant set
    /// inside this queue: `HarnessBrain::run_cycle` calls
    /// [`ApprovalRequestQueue::clear`] at the top of **every** cycle.
    ///
    /// A grant is minted by the approve, and redeemed during the follow-up cycle
    /// that approve kicks off — so if `clear()` reached the grants, the feature
    /// would be destroyed by its own happy path: the cycle dispatched to redeem
    /// the grant would wipe it microseconds before the agent's tool call arrived,
    /// and every approval would fall through and re-park. Separate inner locks
    /// are what prevent that, and this pins it.
    #[test]
    fn grants_survive_a_queue_clear() {
        let queue = ApprovalRequestQueue::default();
        let grants = queue.grants();
        let args = serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" });
        grants.grant(crate::runtime::grants::GrantedCall {
            approval_id: crate::ports::types::ApprovalId::new("appr-1"),
            agent: "finance".into(),
            tool: "composio_execute".into(),
            args: args.clone(),
            at_millis: 1_000,
        });

        queue.clear();

        assert_eq!(
            grants.live_count(),
            1,
            "clearing the request queue must not clear the grants it rides with"
        );
        assert!(
            queue
                .grants()
                .consume("finance", "composio_execute", &args)
                .is_some(),
            "and the grant is still redeemable through a fresh handle"
        );
    }

    /// Issue #242: a dispatched card claims only the requests **its own** turns
    /// added. The queue is shared with any chat turn earlier in the same cycle,
    /// so stamping from position zero would tag somebody else's approval with
    /// this run and make the card read as waiting on an approval it never
    /// triggered.
    #[test]
    fn stamping_a_run_claims_only_the_requests_that_came_after_the_boundary() {
        let queue = ApprovalRequestQueue::default();
        let queued = |kind: &str| ApprovalRequest {
            tool: kind.to_string(),
            reason: "gated".to_string(),
            effect: Effect {
                kind: kind.to_string(),
                group: EffectGroup::Other,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::json!({ "kind": kind }),
                agent: Some("ceo".to_string()),
                run_id: None,
            },
        };

        // A chat turn earlier in this cycle parked one…
        queue.push(queued("chat.thing"));
        // …and the dispatch takes its boundary here.
        let boundary = queue.queued();
        assert_eq!(boundary, 1);
        queue.push(queued("dispatch.thing"));
        queue.push(queued("dispatch.other"));

        assert_eq!(queue.stamp_run(boundary, "run-1"), 2);

        let drained = queue.drain(10);
        assert_eq!(drained.len(), 3);
        assert_eq!(
            drained[0].effect.run_id, None,
            "the chat turn's approval belongs to no attempt"
        );
        assert_eq!(drained[1].effect.run_id.as_deref(), Some("run-1"));
        assert_eq!(drained[2].effect.run_id.as_deref(), Some("run-1"));
    }

    /// A dispatch that parked nothing stamps nothing — which is what keeps a
    /// clean run reading as `Succeeded` rather than as waiting on a person.
    #[test]
    fn a_dispatch_that_parked_nothing_claims_nothing() {
        let queue = ApprovalRequestQueue::default();
        assert_eq!(queue.stamp_run(queue.queued(), "run-1"), 0);
    }

    /// A queue nobody installed stays inert — the default policy behaves exactly
    /// as it did before #172 for every non-harness construction site.
    #[tokio::test]
    async fn a_policy_without_a_shared_queue_still_decides_normally() {
        let p = policy("supervised", &[], None);
        assert!(matches!(
            p.check(&request("send_email", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }

    // --- The per-agent daily spend cap (issue #304) ---------------------------

    use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
    use std::sync::atomic::AtomicUsize;

    /// A meter over a fixed sample set that **respects `since_millis`**.
    ///
    /// The respecting is the whole point of the double. A meter that ignored
    /// `since` — which the crate's other test meters do, harmlessly, because
    /// nothing they back reads a window — would make the day-boundary tests
    /// pass no matter what boundary the code computed, including no boundary at
    /// all. It also counts queries, so "a policy with no cap never asks the
    /// meter" is an assertion rather than an assumption.
    #[derive(Default)]
    struct FixedMeter {
        samples: Vec<UsageSample>,
        queries: AtomicUsize,
    }

    impl FixedMeter {
        fn with(samples: Vec<UsageSample>) -> Arc<Self> {
            Arc::new(Self {
                samples,
                queries: AtomicUsize::new(0),
            })
        }

        fn query_count(&self) -> usize {
            self.queries.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl UsageMeter for FixedMeter {
        async fn record(&self, _company: &CompanyId, _sample: &UsageSample) -> crate::Result<()> {
            Ok(())
        }
        async fn query(&self, _company: &CompanyId, since: u64) -> crate::Result<Vec<UsageSample>> {
            self.queries.fetch_add(1, Ordering::Relaxed);
            Ok(self
                .samples
                .iter()
                .filter(|sample| sample.at_millis >= since)
                .cloned()
                .collect())
        }
    }

    /// A meter whose reads fail — the transient-uncertainty case.
    struct FailingMeter;

    #[async_trait]
    impl UsageMeter for FailingMeter {
        async fn record(&self, _company: &CompanyId, _sample: &UsageSample) -> crate::Result<()> {
            Ok(())
        }
        async fn query(
            &self,
            _company: &CompanyId,
            _since: u64,
        ) -> crate::Result<Vec<UsageSample>> {
            Err(crate::error::OpenCompanyError::Store(
                "meter unavailable".into(),
            ))
        }
    }

    /// An inference sample costing `usd`, stamped at `at_millis`.
    fn spend_sample(agent: &str, usd: f64, at_millis: u64) -> UsageSample {
        UsageSample {
            at_millis,
            agent: agent.into(),
            provider: "managed".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cost_usd: usd,
            kind: SampleKind::Inference,
            run_id: None,
        }
    }

    /// Some instant today, comfortably after UTC midnight.
    fn today() -> u64 {
        crate::ports::now_millis()
    }

    /// The last millisecond of yesterday, UTC.
    fn yesterday() -> u64 {
        crate::metering::utc_day_start_millis(today()).saturating_sub(1)
    }

    /// A cap-bearing policy bound to `agent`, reading spend from `meter`, plus
    /// the grant set its queue carries.
    fn capped_policy(
        mode: &str,
        auto_under: Option<f64>,
        cap: f64,
        agent: &str,
        meter: Arc<dyn UsageMeter>,
    ) -> (ApprovalPolicy, crate::runtime::grants::GrantSet) {
        let p = Policy {
            mode: mode.to_string(),
            always_approve: Vec::new(),
            auto_approve_under_usd: auto_under,
        };
        let queue = ApprovalRequestQueue::default();
        let grants = queue.grants();
        (
            ApprovalPolicy::new(&p, Some(cap))
                .with_requests(queue)
                .with_agent(agent)
                .with_spend(meter, CompanyId::new("acme")),
            grants,
        )
    }

    /// The core of #304: at cap, a **priced** call parks — and it parks through
    /// the two carve-outs that would otherwise wave it straight through.
    ///
    /// * `web_search` under `supervised` is the #238 metered-read carve-out: it
    ///   never parks, precisely *because* it spends money and the daily call cap
    ///   was the boundary. A per-agent spend cap is a second, tighter boundary,
    ///   and it has to outrank the carve-out or the tightest limit in the
    ///   manifest would be the one that does nothing.
    /// * Under `full` there is no per-call gate at all. "Full autonomy" means
    ///   the operator is not asked about each action; it does not mean the
    ///   budget they wrote down is advisory.
    #[tokio::test]
    async fn at_cap_a_priced_call_parks_through_the_metered_read_and_full_carve_outs() {
        let meter = FixedMeter::with(vec![spend_sample("analyst", 5.00, today())]);

        let (supervised, _) = capped_policy(
            "supervised",
            None,
            5.0,
            "analyst",
            meter.clone() as Arc<dyn UsageMeter>,
        );
        let decision = supervised
            .check(&request(
                "web_search",
                serde_json::json!({ "query": "acme pricing" }),
            ))
            .await;
        assert!(
            matches!(decision, ToolPolicyDecision::RequireApproval { .. }),
            "a metered read must park once the agent is out of budget: {decision:?}"
        );

        let (full, _) = capped_policy(
            "full",
            None,
            5.0,
            "analyst",
            meter.clone() as Arc<dyn UsageMeter>,
        );
        assert!(
            matches!(
                full.check(&request("media_generate_image", serde_json::json!({})))
                    .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "full autonomy is not a budget exemption"
        );
        assert!(
            matches!(
                full.check(&request(
                    "pay_invoice",
                    serde_json::json!({ "amount_usd": 1.0 })
                ))
                .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "an amount-bearing call must park at cap under full autonomy too"
        );
    }

    /// The remaining-budget boundary, and why the arm sits **above**
    /// `auto_approve_under_usd`: a declared amount that would carry the agent
    /// past its cap parks even though it is under the auto-approve threshold,
    /// while an amount that still fits is waved through as before.
    ///
    /// Below the threshold instead, an agent with a $5 cap and a $5 auto-approve
    /// threshold could spend $4.99 at a time forever and the cap would be
    /// unreachable by construction.
    #[tokio::test]
    async fn a_declared_amount_that_breaches_the_remaining_budget_parks() {
        let meter = FixedMeter::with(vec![spend_sample("analyst", 4.20, today())]);
        let (p, _) = capped_policy(
            "supervised",
            Some(5.0),
            5.0,
            "analyst",
            meter.clone() as Arc<dyn UsageMeter>,
        );

        // $4.20 spent + $3.00 = $7.20 > $5.00 cap — parks despite being under
        // the $5 auto-approve threshold.
        assert!(
            matches!(
                p.check(&request(
                    "pay_invoice",
                    serde_json::json!({ "amount_usd": 3.0 })
                ))
                .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "a sub-threshold spend that breaches the day's remaining budget must park"
        );

        // $4.20 + $0.50 = $4.70 <= $5.00 — still fits, so auto-approve applies.
        assert_eq!(
            p.check(&request(
                "pay_invoice",
                serde_json::json!({ "amount_usd": 0.5 })
            ))
            .await,
            ToolPolicyDecision::Allow,
            "a spend that fits inside the remaining budget is unaffected"
        );
    }

    /// A spend cap caps **spend**. At cap a teammate can still read and can
    /// still park a send for the ordinary supervised reason — it has not been
    /// muted, it has been defunded.
    ///
    /// The send assertion checks the *reason text*, not just the decision:
    /// `send_email` parks under supervised either way, so only the wording
    /// distinguishes "parked because it reaches outside" from "parked because
    /// the budget arm swallowed a free call".
    #[tokio::test]
    async fn free_reads_and_sends_are_untouched_at_cap() {
        let meter = FixedMeter::with(vec![spend_sample("analyst", 9.99, today())]);
        let (p, _) = capped_policy(
            "supervised",
            None,
            5.0,
            "analyst",
            meter.clone() as Arc<dyn UsageMeter>,
        );

        assert_eq!(
            p.check(&request("read_file", serde_json::json!({}))).await,
            ToolPolicyDecision::Allow,
            "a free read costs nothing and must survive the cap"
        );

        let decision = p
            .check(&request(
                "send_email",
                serde_json::json!({ "to": "a@b.test" }),
            ))
            .await;
        match decision {
            ToolPolicyDecision::RequireApproval { reason, .. } => assert!(
                reason.contains("supervised"),
                "a free send parks for the ordinary tier reason, not the budget: {reason}"
            ),
            other => panic!("send_email must still park under supervised: {other:?}"),
        }
    }

    /// The ordering pin, mirroring `a_grant_beats_always_approve`: the operator's
    /// approval of a budget-parked call actually **runs** it, once.
    ///
    /// If the budget arm sat above grant consumption, approving an at-cap call
    /// would re-park it forever — the park exists to ask the operator a
    /// question, and the grant is their answer. Ranking the question above the
    /// answer makes approval mean nothing.
    #[tokio::test]
    async fn a_grant_releases_a_budget_parked_call_once_then_it_re_parks() {
        let meter = FixedMeter::with(vec![spend_sample("analyst", 5.00, today())]);
        let (p, grants) = capped_policy(
            "supervised",
            None,
            5.0,
            "analyst",
            meter.clone() as Arc<dyn UsageMeter>,
        );
        let args = serde_json::json!({ "query": "acme pricing" });

        // Out of budget: parks.
        assert!(matches!(
            p.check(&request("web_search", args.clone())).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));

        // The operator approves that exact call.
        grants.grant(granted("analyst", "web_search", args.clone()));
        assert_eq!(
            p.check(&request("web_search", args.clone())).await,
            ToolPolicyDecision::Allow,
            "an approved at-cap call must run; otherwise approval authorises nothing"
        );

        // Single-use: the budget is still exhausted, so the next one re-parks.
        assert!(
            matches!(
                p.check(&request("web_search", args)).await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "one approval buys one over-budget call, not a raised cap"
        );
    }

    /// `readonly` outranks the budget arm, as it outranks the grant: the brake
    /// denies outright rather than offering the operator something to approve.
    #[tokio::test]
    async fn the_readonly_brake_still_denies_before_the_budget_arm() {
        let meter = FixedMeter::with(vec![spend_sample("analyst", 9.99, today())]);
        let (p, _) = capped_policy(
            "readonly",
            None,
            5.0,
            "analyst",
            meter.clone() as Arc<dyn UsageMeter>,
        );
        assert!(
            matches!(
                p.check(&request(
                    "pay_invoice",
                    serde_json::json!({ "amount_usd": 1.0 })
                ))
                .await,
                ToolPolicyDecision::Deny { .. }
            ),
            "a read-only desk denies a spend; it does not offer it for approval"
        );
    }

    /// "Daily" is the UTC calendar day: yesterday's $9 does not hold today's
    /// budget hostage. This is what the `since`-respecting double buys.
    #[tokio::test]
    async fn yesterdays_spend_does_not_count_against_todays_cap() {
        let meter = FixedMeter::with(vec![spend_sample("analyst", 9.00, yesterday())]);
        let (p, _) = capped_policy(
            "supervised",
            None,
            5.0,
            "analyst",
            meter.clone() as Arc<dyn UsageMeter>,
        );
        assert_eq!(
            p.check(&request(
                "web_search",
                serde_json::json!({ "query": "acme" })
            ))
            .await,
            ToolPolicyDecision::Allow,
            "the cap resets at 00:00Z; yesterday's spend is spent"
        );
    }

    /// An uncapped agent never pays for a meter round-trip — the arm
    /// short-circuits on the cap before anything else.
    #[tokio::test]
    async fn an_uncapped_agent_never_queries_the_meter() {
        let meter = FixedMeter::with(vec![spend_sample("analyst", 99.0, today())]);
        let p = Policy {
            mode: "full".to_string(),
            always_approve: Vec::new(),
            auto_approve_under_usd: None,
        };
        let uncapped = ApprovalPolicy::new(&p, None)
            .with_requests(ApprovalRequestQueue::default())
            .with_agent("analyst")
            .with_spend(meter.clone() as Arc<dyn UsageMeter>, CompanyId::new("acme"));

        assert_eq!(
            uncapped
                .check(&request(
                    "pay_invoice",
                    serde_json::json!({ "amount_usd": 400.0 })
                ))
                .await,
            ToolPolicyDecision::Allow
        );
        assert_eq!(
            meter.query_count(),
            0,
            "no cap means no question to ask the meter"
        );
    }

    /// No meter wired — every non-harness construction site, and a host without
    /// one — leaves the cap inert rather than parking every priced call forever.
    /// A permanent deployment fact must not brick spend tools with a park no
    /// approval can clear.
    #[tokio::test]
    async fn a_cap_with_no_meter_is_inert() {
        let p = Policy {
            mode: "full".to_string(),
            always_approve: Vec::new(),
            auto_approve_under_usd: None,
        };
        let no_meter = ApprovalPolicy::new(&p, Some(5.0))
            .with_requests(ApprovalRequestQueue::default())
            .with_agent("analyst");

        assert_eq!(
            no_meter
                .check(&request(
                    "pay_invoice",
                    serde_json::json!({ "amount_usd": 400.0 })
                ))
                .await,
            ToolPolicyDecision::Allow,
            "an unenforceable cap must not park what it can never release"
        );
    }

    /// A meter read that **errors** is transient uncertainty about money, and
    /// reads as *ask*, not *allow*: the priced call parks, naming the
    /// uncertainty. A free call is untouched — the arm never saw it.
    ///
    /// The deliberate opposite of the dispatch gate's fail-open. There the
    /// alternative is bricking the company's cognition with no recourse; here it
    /// is one call waiting on a human who can wave it through.
    #[tokio::test]
    async fn a_failing_meter_parks_priced_calls_and_leaves_free_ones_alone() {
        let (p, _) = capped_policy(
            "full",
            None,
            5.0,
            "analyst",
            Arc::new(FailingMeter) as Arc<dyn UsageMeter>,
        );

        let decision = p
            .check(&request(
                "pay_invoice",
                serde_json::json!({ "amount_usd": 1.0 }),
            ))
            .await;
        match decision {
            ToolPolicyDecision::RequireApproval { reason, .. } => assert!(
                reason.contains("could not be verified"),
                "the park must say the budget is unknown, not that it is exceeded: {reason}"
            ),
            other => panic!("an unreadable budget must park a spend: {other:?}"),
        }

        assert_eq!(
            p.check(&request("read_file", serde_json::json!({}))).await,
            ToolPolicyDecision::Allow,
            "a free call never reaches the budget arm, so a meter outage cannot gate it"
        );
    }
}
