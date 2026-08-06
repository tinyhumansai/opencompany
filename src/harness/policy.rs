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

/// The name openhuman knows this policy by, and the name it stamps into the
/// refusal it hands the model when a call is gated
/// (`"…requires approval under policy 'opencompany-approval'"`).
///
/// A constant rather than a literal because issue #411's step classifier keys
/// on it: that is how a parked call is told apart from every other blocked one
/// and rendered as *waiting on you* rather than as a crash. Naming it here
/// means the producer and the reader share one definition instead of two
/// copies of a string that can silently drift.
pub const POLICY_NAME: &str = "opencompany-approval";

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

    /// Splits off every request queued **at or after** `from`, leaving the ones
    /// below it in place (issue #395).
    ///
    /// # Why this exists next to [`drain`](Self::drain)
    ///
    /// `drain` is a *cycle-end* verb: it takes the front of the queue and
    /// **clears whatever is left**, which is correct when the cycle owns the
    /// whole queue and is about to finish with it. A workflow agent node owns
    /// no such thing. It runs on the same shared
    /// [`HarnessDeps`](crate::harness::HarnessDeps) as the chat cycles, and a
    /// cycle may be part-way through its own turn when the node's turn parks a
    /// request. Calling `drain` there would steal that cycle's entries and then
    /// wipe the rest — one workflow node silently swallowing another
    /// conversation's pending approvals.
    ///
    /// So this takes only the tail the caller is entitled to. The boundary is
    /// valid for exactly the reason [`queued`](Self::queued) is documented to
    /// be one: [`push`](Self::push) only ever appends, so a position taken
    /// before the turn stays the line between "somebody else parked that" and
    /// "this turn did".
    ///
    /// A `from` past the end yields nothing rather than panicking, so a caller
    /// that raced a [`clear`](Self::clear) degrades to parking nothing instead
    /// of aborting the run.
    pub fn take_from(&self, from: usize) -> Vec<ApprovalRequest> {
        let mut guard = self.inner.lock().expect("approval request queue");
        if from >= guard.len() {
            return Vec::new();
        }
        guard.split_off(from)
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
            group: classify_group(tool_name, args),
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

    /// Matches a live **standing** grant for this agent and tool (issue #374),
    /// leaving it in place — a standing grant is not spent by being used.
    ///
    /// Two conditions beyond the match itself, and both are the arm's safety
    /// rather than decoration:
    ///
    /// * **A policy with no agent bound can never match**, the same
    ///   short-circuit [`consume_grant`](Self::consume_grant) takes, so every
    ///   non-harness construction site behaves exactly as it did before.
    /// * **A priced call is refused outright**, even holding a grant. The mint
    ///   side already refuses to grant anything that is not
    ///   [`Standing::Grantable`](crate::policy::Standing::Grantable), so a
    ///   `Spend`-group tool can never reach here; this covers the *other* way a
    ///   call becomes priced, which the tool name cannot predict — a grantable
    ///   tool invoked with a declared `amount_usd`. Refusing here means the call
    ///   falls through to the budget and mode arms below and parks, so a
    ///   standing grant cannot admit money **by placement**, not by promise.
    /// * **The call being made must itself be grantable**, re-checked here
    ///   rather than trusted from mint time (issue #441). A standing grant
    ///   matches on `(agent, tool, unexpired)` and admits *any* arguments,
    ///   which was a fair summary of a tool's consequence while consequence was
    ///   a property of the tool name. It is not one for `composio_execute`:
    ///   every Composio action arrives under that name, so a grant minted on a
    ///   repository read would otherwise admit an outgoing email on the same
    ///   handle. Re-classifying the live arguments keeps the grant to the shape
    ///   the operator was shown. It also means a grant replayed from a journal
    ///   line written before this change cannot admit a send.
    /// * **The call must fall in the same slice of the tool the grant was minted
    ///   on** (issue #457). Re-classifying keeps a send out of a read's grant,
    ///   but every Composio read across every connected toolkit is the same
    ///   verdict under the same tool name — so a grant minted from "read from
    ///   GitHub" admitted a read of the company's mailbox too. The live call's
    ///   scope is computed here and matched against what the grant recorded; an
    ///   action the catalogue cannot place has no scope and a scoped grant
    ///   refuses it. A grant replayed from a line written before the field
    ///   existed is unscoped and behaves exactly as it did.
    fn standing_grant_allows(&self, tool: &str, args: &serde_json::Value) -> bool {
        let Some(agent) = self.agent.as_deref() else {
            return false;
        };
        if Self::is_priced_call(tool, args, Self::amount_usd(args)) {
            return false;
        }
        if !crate::policy::consequence_of(tool, args)
            .standing
            .is_grantable()
        {
            log::debug!(
                "[approval] tool '{tool}' holds a standing grant for agent '{agent}' but this \
                 call is not grantable on its own arguments; parking it"
            );
            return false;
        }
        // Issue #457: which slice of the tool this *live* call falls in,
        // computed by the same function the mint side used on the parked
        // effect's payload — one function, so the two answers cannot drift into
        // a grant that never matches its own tool.
        let scope = crate::policy::consequence::standing_scope_of(tool, args);
        let Some(grant) = self.requests.grants().match_standing(
            agent,
            tool,
            scope.as_deref(),
            crate::ports::now_millis(),
        ) else {
            return false;
        };
        log::debug!(
            "[approval] tool '{tool}' allowed by standing grant {} for agent '{agent}' \
             (expires at {})",
            grant.id,
            grant.expires_at_millis
        );
        true
    }

    /// Does this tool call **spend money**? The predicate the daily budget arm
    /// gates on (issue #304).
    ///
    /// Three signals, any of which is enough:
    ///
    /// * the call **declares** an amount (`amount_usd` / `amount`) — the only
    ///   pre-flight signal there is for an x402 payment;
    /// * it projects onto [`EffectGroup::Spend`] — `web_search`, whose backend
    ///   charges per request, plus `media_generate_*`, `pay_*`/`transfer_*`,
    ///   and anything else the group classifier already calls spend.
    ///
    /// A separate "is it a metered read" arm used to sit between those two.
    /// It was subsumed the moment the declaration named a group and a reach
    /// together: the only `Reach::Money` tool is `web_search`, and its group is
    /// `Spend`, so the arm could only ever fire where the one below already
    /// had. `web_search_is_still_a_priced_call` pins that, so a future
    /// `Reach::Money` tool that is *not* `Spend` fails a test rather than
    /// silently escaping the cap.
    ///
    /// Everything else — a read, a send, a publish, a workspace write — is
    /// **untouched at cap**. A spend cap caps spend; making a teammate unable to
    /// answer a question because it spent its budget this morning would be a
    /// different feature, and a worse one.
    fn is_priced_call(tool: &str, args: &serde_json::Value, declared_amount: Option<f64>) -> bool {
        declared_amount.is_some() || classify_group(tool, args) == EffectGroup::Spend
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
        if !Self::is_priced_call(tool, args, declared_amount) {
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
        POLICY_NAME
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
        if self.mode == PolicyMode::Readonly && is_external_effect(tool, &request.arguments) {
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

        // 2b. A live STANDING grant: the operator opened this tool up for this
        //     teammate until a deadline (issue #374). Any arguments, unlimited
        //     calls, until it expires or is revoked.
        //
        // IMMEDIATELY BELOW the single-use check and nowhere else. Above it, a
        // standing grant would mask consumption: the operator's one-off approval
        // would sit unredeemed until its TTL and then be announced as "the agent
        // didn't act" — a lie about a call that ran. The single-use grant must
        // burn if it matches, so it is asked first.
        //
        // Everything ABOVE stays above, and for unchanged reasons: `never_do`'s
        // reserved slot outranks a standing grant exactly as it outranks a
        // single-use one — more so, since this one admits many calls — and the
        // `readonly` brake denies before either is consulted, leaving the grant
        // intact for when the brake is released.
        //
        // What keeps this narrow is decided at MINT time and re-checked here.
        // The mint side refuses to grant anything the declaration does not call
        // grantable, so no Spend / Send / Sign / Publish / Hire / Identity tool
        // can have a standing grant to match. Two things the tool NAME cannot
        // predict are refused inside `standing_grant_allows`: a grantable tool
        // carrying a declared amount, so this arm can never admit money; and a
        // `composio_execute` call whose action is a send, so a grant minted on
        // a repository read cannot admit an outgoing email (issue #441).
        if self.standing_grant_allows(tool, &request.arguments) {
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

        // One value, two questions. `readonly`'s contract is that nothing
        // changes and nothing is spent, so it denies anything but `Nothing`.
        // `supervised` parks only a consequence — which leaves the `Money`
        // bucket (`web_search`, issue #238) allowed here and denied there,
        // the split the old boolean could not express.
        //
        // Parking a metered read would also be *useless* rather than merely
        // annoying: openhuman resolves a `RequireApproval` inline and never
        // re-dispatches the call, so a parked search is a search that never
        // happens, and an agent with no search invents citations. Consent for
        // it happens once, at grant time — an explicit `search` grant a `*`
        // cannot confer — and the per-company daily cap is the boundary. An
        // operator who does want a per-call gate has
        // `[policy].always_approve = ["web_search"]`, which wins over every
        // tier including `full`.
        let reach = crate::policy::consequence_of(tool, &request.arguments).reach;
        match self.mode {
            PolicyMode::Full => ToolPolicyDecision::Allow,
            PolicyMode::Supervised => {
                if reach.parks_under_supervision() {
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
                if reach.denied_under_readonly() {
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

/// Does this tool call mutate state or reach an external counterparty?
///
/// A thin reader of [`crate::policy::consequence_of`], which is where the
/// answer is actually declared. It used to be a hand-maintained carve-out list
/// bolted onto a read-only-*prefix* heuristic, and it drifted the way such
/// lists do: three separate families needed the same exemption for the same
/// reason, each added after somebody hit it, and the ones nobody hit stayed
/// broken. `mcp_list_servers` — which the agent persona *instructs* every agent
/// to call rather than answer a capability question from memory — parked for
/// operator approval (issue #443), and so did `file_read`, `glob` and `grep`,
/// because the prefix rule keys on the start of a name and none of them begins
/// with a read-only word.
///
/// `args` are consulted because one tool name does not always mean one
/// consequence: every Composio action arrives as `composio_execute`.
fn is_external_effect(tool_name: &str, args: &serde_json::Value) -> bool {
    crate::policy::consequence_of(tool_name, args)
        .reach
        .denied_under_readonly()
}

/// The top-level keys of a tool-argument object, for a mismatch diagnostic.
/// Keys only — values carry recipients, bodies and amounts.
fn top_level_keys(args: &serde_json::Value) -> Vec<&str> {
    match args {
        serde_json::Value::Object(map) => map.keys().map(String::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Map a tool call onto the supervised [`EffectGroup`] taxonomy — the
/// consequence class the operator's approval card names.
///
/// A thin reader of [`crate::policy::consequence_of`]. Since issue #444 this
/// classification decides *only* what the card says: whether the call may be
/// granted standing is a separate answer from the same declaration
/// ([`Effect::may_be_granted_standing`](crate::ports::types::Effect::may_be_granted_standing)),
/// because one enum answering both questions is how the residual `Other` bucket
/// came to mean "nothing in particular to name" and "safe for a week" at the
/// same time.
///
/// `args` matter: `composio_execute` carries every Composio action under one
/// name, so the group is read from the action rather than from the tool that
/// delivers it (issue #441).
fn classify_group(tool_name: &str, args: &serde_json::Value) -> EffectGroup {
    crate::policy::consequence_of(tool_name, args).group
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

    /// May this call be granted standing? The rule as the mint path asks it,
    /// so a test here and the enforcement in the default build cannot answer
    /// differently.
    fn grantable(tool: &str, args: &serde_json::Value) -> bool {
        crate::policy::consequence_of(tool, args)
            .standing
            .is_grantable()
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
            origin_thread: None,
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
            origin_thread: None,
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

    // --- `take_from`: a workflow node's own tail, and nobody else's (#395) ----

    /// The regression this method exists for. A workflow agent node parks its
    /// gated calls while a chat cycle is part-way through its own turn; taking
    /// the tail must leave that cycle's entries exactly where they were, and
    /// `drain` would not — it takes from the front and clears the rest.
    #[test]
    fn taking_from_a_boundary_leaves_everything_below_it_untouched() {
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

        // A chat cycle's own turn parked this one and has not drained yet.
        queue.push(queued("chat.thing"));
        let boundary = queue.queued();
        // The workflow node's turn parks two.
        queue.push(queued("node.thing"));
        queue.push(queued("node.other"));

        let taken = queue.take_from(boundary);
        assert_eq!(
            taken.iter().map(|r| r.tool.as_str()).collect::<Vec<_>>(),
            vec!["node.thing", "node.other"],
            "only the node's own tail comes back"
        );
        assert_eq!(
            queue.queued(),
            1,
            "the chat cycle's entry is still queued for its own drain"
        );
        assert_eq!(
            queue.drain(10)[0].tool,
            "chat.thing",
            "…and it is the same entry, not a survivor of a clear-and-rebuild"
        );
    }

    /// A node whose turn parked nothing takes nothing — and a boundary past the
    /// end (a `clear` raced in between) yields nothing rather than panicking.
    #[test]
    fn taking_from_the_end_or_past_it_yields_nothing() {
        let queue = ApprovalRequestQueue::default();
        assert!(queue.take_from(queue.queued()).is_empty());
        assert!(queue.take_from(0).is_empty());
        assert!(
            queue.take_from(9_000).is_empty(),
            "a boundary past the end must not panic — a raced clear degrades to \
             parking nothing"
        );
    }

    /// The append-only property the boundary rests on: a push while the node's
    /// turn is running lands *after* the boundary, never before it, so the
    /// entitlement split can never mis-attribute an entry downward.
    #[test]
    fn pushes_only_ever_append_so_a_boundary_stays_valid() {
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
                agent: None,
                run_id: None,
            },
        };
        queue.push(queued("a"));
        let boundary = queue.queued();
        for kind in ["b", "c", "d"] {
            queue.push(queued(kind));
        }
        assert_eq!(queue.take_from(boundary).len(), 3);
        assert_eq!(queue.queued(), 1);
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

    // -----------------------------------------------------------------------
    // Standing grants (issue #374)
    // -----------------------------------------------------------------------
    //
    // These tests granted a standing scope on `workspace_write` until issue
    // #444. That tool was never a fair fixture: it is grantable only under the
    // rule that read grantability off a name's vocabulary, and the parking side
    // of this same gate has always refused to exempt it because it overwrites
    // guidance the operator wrote. The two halves of one gate disagreed about
    // one tool, and the tests pinned the half that was wrong. `file_write` is
    // the honest stand-in — it mutates, so it still parks the first time, but
    // what it mutates is the agent's own sandboxed workspace.

    fn standing(
        agent: &str,
        tool: &str,
        expires_at_millis: u64,
    ) -> crate::runtime::grants::StandingGrant {
        crate::runtime::grants::StandingGrant {
            id: crate::runtime::grants::GrantId::new("g1"),
            agent: agent.to_string(),
            tool: tool.to_string(),
            granted_by: crate::ports::types::Actor {
                kind: crate::ports::types::ActorKind::User,
                id: "user-1".to_string(),
            },
            approval_id: crate::ports::types::ApprovalId::new("appr-1"),
            at_millis: 1_000,
            expires_at_millis,
            origin_thread: None,
            scope: None,
        }
    }

    /// The same fixture, confined to one Composio toolkit (issue #457).
    fn scoped_standing(
        agent: &str,
        tool: &str,
        scope: &str,
        expires_at_millis: u64,
    ) -> crate::runtime::grants::StandingGrant {
        crate::runtime::grants::StandingGrant {
            scope: Some(scope.to_string()),
            ..standing(agent, tool, expires_at_millis)
        }
    }

    /// Far enough ahead that wall-clock drift during a test run cannot reach it.
    fn far_future() -> u64 {
        crate::ports::now_millis() + 60 * 60 * 1000
    }

    /// The issue in one test: the same tool, called repeatedly with different
    /// arguments, stops asking.
    #[tokio::test]
    async fn a_standing_grant_admits_repeat_calls_with_any_arguments() {
        let (p, grants) = granting_policy("supervised", &[], "ops");
        grants.grant_standing(standing("ops", "file_write", far_future()));

        for args in [
            serde_json::json!({ "path": "notes/a.md", "body": "one" }),
            serde_json::json!({ "path": "notes/b.md", "body": "two" }),
            serde_json::json!({}),
        ] {
            assert_eq!(
                p.check(&request("file_write", args.clone())).await,
                ToolPolicyDecision::Allow,
                "a standing grant admits any arguments: {args}"
            );
        }
        assert_eq!(
            grants.standing_count(),
            1,
            "using a standing grant must not spend it"
        );
    }

    /// An expired standing grant is refused at redemption, not merely swept.
    ///
    /// The sweep runs on the scheduler's maintenance tick; between two ticks a
    /// lapsed grant would otherwise keep admitting calls, and "for one hour" has
    /// to mean one hour.
    #[tokio::test]
    async fn an_expired_standing_grant_re_parks() {
        let (p, grants) = granting_policy("supervised", &[], "ops");
        // Already past — and deliberately left in the set, so this proves the
        // redemption check rather than the sweep.
        grants.grant_standing(standing("ops", "file_write", 1));

        assert!(matches!(
            p.check(&request("file_write", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(grants.standing_count(), 1, "the sweep did not run here");
    }

    #[tokio::test]
    async fn a_standing_grant_is_scoped_to_its_agent_and_tool() {
        let (p, grants) = granting_policy("supervised", &[], "marketing");
        // Granted to a different teammate.
        grants.grant_standing(standing("ops", "file_write", far_future()));
        assert!(matches!(
            p.check(&request("file_write", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));

        let (p, grants) = granting_policy("supervised", &[], "ops");
        grants.grant_standing(standing("ops", "file_write", far_future()));
        // A different tool for the right teammate.
        assert!(matches!(
            p.check(&request("send_email", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }

    /// The single-use grant burns first, even when a standing grant would also
    /// have admitted the call.
    ///
    /// Ordering, not coincidence. If the standing arm ran first the operator's
    /// one-off approval would sit unredeemed until its TTL and then be announced
    /// as "the agent didn't act within 15 minutes" — a notice about work that
    /// had already happened, which is worse than no notice at all.
    #[tokio::test]
    async fn the_single_use_grant_is_consumed_first() {
        let (p, grants) = granting_policy("supervised", &[], "ops");
        let args = serde_json::json!({ "path": "notes/a.md" });
        grants.grant(granted("ops", "file_write", args.clone()));
        grants.grant_standing(standing("ops", "file_write", far_future()));

        assert_eq!(
            p.check(&request("file_write", args)).await,
            ToolPolicyDecision::Allow
        );
        assert_eq!(grants.live_count(), 0, "the single-use grant burned");
        assert_eq!(grants.standing_count(), 1);
        assert_eq!(
            grants.drain_consumed().len(),
            1,
            "the consumption is journaled, so no phantom expiry notice follows"
        );
    }

    /// A standing grant can never admit money — by placement, not by promise.
    ///
    /// The mint side refuses to grant anything the declaration does not call
    /// grantable, so no Spend-group tool can have one. This covers the other way
    /// a call becomes priced, which the tool name cannot predict: a grantable
    /// tool invoked with a declared amount. It must fall through to the budget
    /// and mode arms and park.
    #[tokio::test]
    async fn a_standing_grant_refuses_a_priced_call() {
        let (p, grants) = granting_policy("supervised", &[], "ops");
        grants.grant_standing(standing("ops", "file_write", far_future()));

        // Same tool, same grant — the only difference is a declared amount.
        assert_eq!(
            p.check(&request("file_write", serde_json::json!({ "path": "a" })))
                .await,
            ToolPolicyDecision::Allow
        );
        assert!(
            matches!(
                p.check(&request(
                    "file_write",
                    serde_json::json!({ "path": "a", "amount_usd": 25.0 })
                ))
                .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "a declared amount must park even under a standing grant"
        );

        // And the metered read, which is priced without declaring anything.
        let (p, grants) = granting_policy("supervised", &[], "ops");
        grants.grant_standing(standing(
            "ops",
            crate::harness::search::WEB_SEARCH_TOOL,
            far_future(),
        ));
        assert_ne!(
            p.check(&request(
                crate::harness::search::WEB_SEARCH_TOOL,
                serde_json::json!({ "query": "x" })
            ))
            .await,
            ToolPolicyDecision::Deny {
                reason: String::new()
            },
            "sanity: this asserts the arm was not reached, not the tier's answer"
        );
    }

    /// `readonly` outranks a standing grant, and leaves it intact.
    ///
    /// Same argument as the single-use case: the brake is the emergency stop,
    /// not a question, and consent does not survive it. But the grant is not
    /// destroyed — the call never ran, so the operator's permission is still
    /// there when the brake comes off.
    #[tokio::test]
    async fn readonly_outranks_a_standing_grant_and_leaves_it_intact() {
        let (p, grants) = granting_policy("readonly", &[], "ops");
        grants.grant_standing(standing("ops", "file_write", far_future()));

        assert!(matches!(
            p.check(&request("file_write", serde_json::json!({}))).await,
            ToolPolicyDecision::Deny { .. }
        ));
        assert_eq!(
            grants.standing_count(),
            1,
            "a denied call must not destroy the permission it never used"
        );
    }

    #[tokio::test]
    async fn an_unbound_policy_ignores_standing_grants_entirely() {
        let queue = ApprovalRequestQueue::default();
        let grants = queue.grants();
        let p = policy("supervised", &[], None).with_requests(queue);
        grants.grant_standing(standing("ops", "send_email", far_future()));

        assert!(matches!(
            p.check(&request("send_email", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }

    /// What may be granted standing is decided by what a tool can **reach**,
    /// never by the vocabulary of its name (issue #444).
    ///
    /// The list below is the before/after of the boundary. Every tool in the
    /// first group used to be grantable — `shell` and `web_fetch` and
    /// `mcp_registry_tool_call` because their names carry no consequence word,
    /// and `workspace_write` in flat contradiction of the parking side of this
    /// same gate, which has always refused to exempt it on the grounds that it
    /// overwrites operator-owned guidance. An operator on staging really did
    /// get a standing grant on running arbitrary terminal commands while being
    /// refused one on reading a repository.
    #[test]
    fn what_may_be_granted_standing_is_what_a_tool_can_reach() {
        let args = serde_json::json!({});
        for tool in [
            // Arbitrary code, arbitrary address, operator-owned guidance.
            "shell",
            "http_request",
            "curl",
            "web_fetch",
            "workspace_write",
            // Anything a remote server chooses to advertise.
            "mcp_registry_tool_call",
            "mcp_call_tool",
            // Named consequences, unchanged.
            "composio_authorize",
            "pay_invoice",
            "transfer_funds",
            "send_email",
            "media_generate_image",
            crate::harness::search::WEB_SEARCH_TOOL,
            "publish_post",
            "contract_accept",
            "handle_register",
            "filing_submit",
            "deploy_site",
            // A tool nobody has classified. Landing in the residual bucket no
            // longer confers the longest permission available.
            "some_tool_nobody_declared",
        ] {
            assert!(
                !grantable(tool, &args),
                "{tool} can reach further than a standing grant can describe"
            );
        }
        // The feature keeps its point: writes confined to the agent's own
        // sandbox stay grantable, so a stretch of unattended autonomy is still
        // worth granting.
        for tool in ["file_write", "edit", "apply_patch", "memory_store"] {
            assert!(grantable(tool, &args), "{tool} stays grantable");
        }
    }

    /// Issue #441's headline, at the gate rather than at the card: the same
    /// tool, two verdicts, decided by the action in the arguments.
    #[test]
    fn a_composio_read_may_be_granted_standing_and_a_send_may_not() {
        assert!(grantable(
            "composio_execute",
            &serde_json::json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" })
        ));
        assert!(!grantable(
            "composio_execute",
            &serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" })
        ));
        // The cautious fallback: an action the provider catalogue does not name
        // is a send, so it neither loses its per-call decision nor its `Send`
        // label on the card.
        assert!(!grantable(
            "composio_execute",
            &serde_json::json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" })
        ));
        assert_eq!(
            classify_group(
                "composio_execute",
                &serde_json::json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" })
            ),
            EffectGroup::Send
        );
    }

    /// The grantability answer and the parking answer are read from one
    /// declaration, so an effect the policy projects and the effect the mint
    /// path inspects can never disagree — for every tool, and for both shapes
    /// of a Composio call.
    #[test]
    fn the_projected_effect_and_the_mint_rule_agree_about_every_tool() {
        let p = policy("supervised", &[], None).with_agent("ops");
        let cases: Vec<(&str, serde_json::Value)> = crate::policy::consequence::declared_tools()
            .map(|t| (t, serde_json::json!({})))
            .chain([
                (
                    "composio_execute",
                    serde_json::json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" }),
                ),
                (
                    "composio_execute",
                    serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" }),
                ),
                ("some_tool_nobody_declared", serde_json::json!({})),
            ])
            .collect();
        for (tool, args) in cases {
            let effect = p.effect_for(tool, &args);
            assert_eq!(
                effect.may_be_granted_standing(),
                grantable(tool, &args),
                "the parked effect for `{tool}` disagrees with the declaration it came from"
            );
            assert_eq!(
                effect.group,
                classify_group(tool, &args),
                "the parked effect's group for `{tool}` is not the one the classifier gave"
            );
        }
    }

    /// The arm `is_priced_call` dropped, pinned from the other side.
    ///
    /// Removing the metered-read arm was safe only because the one
    /// `Reach::Money` tool is also `EffectGroup::Spend`. If a future tool is
    /// billed without being classified as spend, it would slip the daily cap
    /// silently — so this fails instead.
    #[test]
    fn web_search_is_still_a_priced_call() {
        let args = serde_json::json!({});
        assert!(ApprovalPolicy::is_priced_call(
            crate::harness::search::WEB_SEARCH_TOOL,
            &args,
            None
        ));
        for tool in crate::policy::consequence::declared_tools() {
            let verdict = crate::policy::consequence_of(tool, &args);
            if verdict.reach.costs_money() {
                assert_eq!(
                    verdict.group,
                    EffectGroup::Spend,
                    "`{tool}` is billed but is not classified as spend, so the daily \
                     budget arm would never see it"
                );
            }
        }
    }

    /// Issue #443: the persona instructs every agent to call these rather than
    /// answer a capability question from memory, and under the DEFAULT
    /// supervised mode that instruction used to cost an operator approval to
    /// follow. Calling *through* a server still parks.
    #[tokio::test]
    async fn listing_mcp_servers_and_tools_runs_without_asking() {
        let p = policy("supervised", &[], None);
        for tool in [
            "mcp_list_servers",
            "mcp_list_tools",
            "mcp_registry_list_tools",
        ] {
            assert_eq!(
                p.check(&request(tool, serde_json::json!({}))).await,
                ToolPolicyDecision::Allow,
                "`{tool}` reads local registration state and reaches nothing"
            );
        }
        for tool in ["mcp_call_tool", "mcp_registry_tool_call"] {
            assert!(
                matches!(
                    p.check(&request(tool, serde_json::json!({}))).await,
                    ToolPolicyDecision::RequireApproval { .. }
                ),
                "`{tool}` can perform any effect the remote server advertises"
            );
        }
    }

    /// The sibling defects the same sweep turned up. Four pure reads of the
    /// agent's own workspace parked under the default mode, because the
    /// read-only rule matched a *prefix* and none of these names begins with a
    /// read-only word. Nobody had reported them; they were found by asking the
    /// same question of every registered tool.
    #[tokio::test]
    async fn a_workspace_read_runs_without_asking_whatever_its_name_begins_with() {
        let p = policy("supervised", &[], None);
        for tool in [
            "file_read",
            "glob",
            "grep",
            "image_info",
            "list",
            "read_workspace_state",
            "memory_recall",
        ] {
            assert_eq!(
                p.check(&request(tool, serde_json::json!({}))).await,
                ToolPolicyDecision::Allow,
                "`{tool}` reads the agent's own workspace"
            );
        }
    }

    /// A standing grant admits any arguments, which was a fair summary of a
    /// tool's consequence while consequence was a property of the tool name.
    /// It is not one for `composio_execute`, so the grant is re-checked against
    /// the live call: a scope granted on a repository read must not admit an
    /// outgoing email on the same handle.
    #[tokio::test]
    async fn a_standing_grant_on_a_composio_read_does_not_admit_a_send() {
        let queue = ApprovalRequestQueue::default();
        let grants = queue.grants();
        let p = policy("supervised", &[], None)
            .with_requests(queue)
            .with_agent("ops");
        grants.grant_standing(standing("ops", "composio_execute", far_future()));

        assert_eq!(
            p.check(&request(
                "composio_execute",
                serde_json::json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" })
            ))
            .await,
            ToolPolicyDecision::Allow,
            "the read the operator granted keeps running"
        );
        assert!(
            matches!(
                p.check(&request(
                    "composio_execute",
                    serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" })
                ))
                .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "a send on the same tool name parks despite the grant"
        );
    }

    /// Issue #457, through the real admission path. Re-classifying the live
    /// action (the test above) separates a read from a send; it cannot separate
    /// one provider's read from another's, because both are reads under the same
    /// tool name for the same teammate. The card said "read from GitHub" and the
    /// grant has to mean that.
    #[tokio::test]
    async fn a_grant_scoped_to_one_provider_does_not_admit_another_providers_read() {
        let queue = ApprovalRequestQueue::default();
        let grants = queue.grants();
        let p = policy("supervised", &[], None)
            .with_requests(queue)
            .with_agent("ops");
        grants.grant_standing(scoped_standing(
            "ops",
            "composio_execute",
            "github",
            far_future(),
        ));

        // A *different* GitHub read: the operator consented to the provider, so
        // this is inside the sentence and must keep running. Scoping by action
        // slug instead would have re-parked here and made the grant worthless.
        assert_eq!(
            p.check(&request(
                "composio_execute",
                serde_json::json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" })
            ))
            .await,
            ToolPolicyDecision::Allow
        );

        // A mailbox read. Also a catalogue read, also grantable, also `ops`,
        // also `composio_execute` — every check upstream of the scope says yes.
        assert!(
            matches!(
                p.check(&request(
                    "composio_execute",
                    serde_json::json!({ "tool": "GMAIL_FETCH_EMAILS" })
                ))
                .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "'read from GitHub' is not consent to read the company's mail"
        );

        // An action the catalogue cannot place has no scope to compare, so the
        // scoped grant refuses it and it parks — unknown is a send, here too.
        assert!(matches!(
            p.check(&request(
                "composio_execute",
                serde_json::json!({ "tool": "NOTAREALTOOLKIT_LIST_THINGS" })
            ))
            .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));

        assert_eq!(
            grants.standing_count(),
            1,
            "none of those refusals spent the permission"
        );
    }

    /// **Replay compatibility (issue #457).** A grant journaled before the scope
    /// field existed comes back unscoped, and an unscoped grant admits the tool
    /// exactly as it did before — otherwise this change would silently void
    /// every permission an operator had already granted.
    #[tokio::test]
    async fn a_grant_from_before_scopes_existed_still_admits_its_tool() {
        let queue = ApprovalRequestQueue::default();
        let grants = queue.grants();
        let p = policy("supervised", &[], None)
            .with_requests(queue)
            .with_agent("ops");

        // Deserialized from the pre-#457 wire shape rather than constructed, so
        // this fails if the field ever stops defaulting.
        let replayed: crate::runtime::grants::StandingGrant =
            serde_json::from_value(serde_json::json!({
                "id": "g-old",
                "agent": "ops",
                "tool": "composio_execute",
                "granted_by": { "kind": "user", "id": "user-1" },
                "approval_id": "appr-old",
                "at_millis": 1_000,
                "expires_at_millis": far_future(),
            }))
            .expect("an old journal line still replays");
        assert_eq!(replayed.scope, None);
        grants.grant_standing(replayed);

        for slug in ["GITHUB_LIST_PULL_REQUESTS", "GMAIL_FETCH_EMAILS"] {
            assert_eq!(
                p.check(&request(
                    "composio_execute",
                    serde_json::json!({ "tool": slug })
                ))
                .await,
                ToolPolicyDecision::Allow,
                "an unscoped grant behaves exactly as it did: {slug}"
            );
        }
        // …and the boundary that was always there is untouched: a send still
        // parks, because the live re-classification runs first.
        assert!(matches!(
            p.check(&request(
                "composio_execute",
                serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" })
            ))
            .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }

    /// Issue #374 added the `deploy` arm. It still applies — to tools with no
    /// declaration, which is now the only place the name heuristics run.
    #[test]
    fn an_undeclared_deploy_still_classifies_as_publish() {
        let args = serde_json::json!({});
        assert_eq!(classify_group("deploy_site", &args), EffectGroup::Publish);
        assert_eq!(
            classify_group("website_deploy", &args),
            EffectGroup::Publish
        );
        assert_eq!(classify_group("publish_post", &args), EffectGroup::Publish);
        // …but it no longer decides grantability, so a deploy tool nobody has
        // declared is refused a standing scope by the undeclared rule as well
        // as by its group.
        assert!(!grantable("deploy_site", &args));
    }

    /// `workspace_write` keeps its `Other` label on the card — there is no
    /// consequence word to name — while being refused a standing scope. That
    /// separation is the point of issue #444: the label and the permission are
    /// different questions.
    #[test]
    fn workspace_write_is_labelled_other_and_is_still_not_grantable() {
        let args = serde_json::json!({});
        assert_eq!(classify_group("workspace_write", &args), EffectGroup::Other);
        assert!(classify_group("workspace_write", &args).is_unclassified());
        assert!(!grantable("workspace_write", &args));
        assert!(is_external_effect("workspace_write", &args));
    }
}
