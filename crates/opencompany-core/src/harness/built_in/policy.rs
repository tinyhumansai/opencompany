//! [`ApprovalPolicy`] — a manifest `[policy]` → openhuman [`ToolPolicy`] bridge.
//!
//! Manifest `[policy].mode` deliberately uses OpenHuman's own security-tier
//! words, so the mapping to [`PolicyMode`] is 1:1 — and that enum is where the
//! tiers are listed, deliberately the only place. Spelling them out here as
//! well went stale the moment `auto` landed (issue #560), which is the whole
//! reason this file stopped enumerating them. On top of the tier the bridge
//! honours the manifest's
//! `always_approve` effect kinds and the per-agent `budget_usd_daily` /
//! `auto_approve_under_usd` thresholds.
//!
//! ## Current production mode: policy HITL disabled
//!
//! Roster construction applies [`ApprovalPolicy::with_policy_hitl_disabled`].
//! In that mode this policy preserves the `readonly` hard denial and redeems
//! grants for approvals already in flight, but returns `Allow` instead of
//! manufacturing a new `RequireApproval`. Agents create new approvals only by
//! calling [`request_approval`](crate::harness::approval_tool). The machinery
//! below is retained for migration compatibility and focused policy tests.
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
//! The call is not resumed, it is **re-issued**, and the reason is narrower than
//! this comment used to claim. It asserted openhuman "genuinely cannot be
//! resumed"; that is false as a blanket statement and it is why nobody looked
//! for years. openhuman never claimed impossibility — its own
//! `ToolPolicyDecision::RequireApproval` doc says session execution
//! *"currently"* treats the variant as fail-closed and that "callers that can
//! prompt for approval may branch on this variant and retry". It even ships
//! durable approval interrupt-and-resume already, for delegation, on tinyagents'
//! graph executor. Two separate things are true (issue #561):
//!
//! * **Resolving inline is incidental.** openhuman blocks the call in one
//!   `wrap_tool` middleware, and that hook is `async` and may call `next.run`
//!   zero or more times — tinyagents' own `wrap_tool_retries_next_until_success`
//!   pins that. Awaiting a verdict there and *then* dispatching is representable
//!   today, with no upstream change.
//! * **Durably suspending is structural.** The agent turn runs on
//!   `AgentHarness`, which has no checkpointer; its `AgentRun` is not
//!   `Serialize`; and the turn is a live async task holding the model context,
//!   bounded by the run's wall-clock deadline.
//!
//! Which settles the design: an in-memory await survives neither a long approval
//! nor a process restart, so against this crate's seven-day standing-grant
//! ceiling it is a leak rather than a mechanism. Re-issuing is the honest answer
//! here until the turn is checkpointable — see issue #561 for the full analysis,
//! including why even the graph executor's `resume` *re-runs* its interrupted
//! node rather than continuing a suspended call.
//!
//! Approving a parked harness effect
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
//! enforced on the ledger path, so the two knobs presented identically while
//! only one of them was real.
//!
//! Spend arrives through two doors, so enforcement is two layers. Inference —
//! the dominant stream — is gated at dispatch in
//! [`HarnessPool::run`](crate::harness::HarnessPool::run), before any model
//! call. Priced *tool* calls are gated here, by the arm below `always_approve`.
//! See [`ApprovalPolicy::daily_budget_verdict`] for why the arm sits exactly
//! where it does, why an at-cap call **parks** rather than being denied, and
//! what the cap does not see.
//!
//! ## Per-call judgement (issue #338)
//!
//! Everything above is decided before the run starts, by an operator writing a
//! manifest; nothing in it looks at what the run is about to do. The last arm
//! of [`check`](ToolPolicy::check) asks that question — see
//! [`crate::policy::judgement`], where the verdict itself lives as a pure,
//! separately tested function.
//!
//! It is **last**, and consulted only where the mode already said allow. That
//! placement is the whole safety argument: it can turn an allow into a stop and
//! can do nothing else, so every arm above keeps its authority unchanged. The
//! invariant, phrased so it survives the next tier: **this arm only ever speaks
//! where the mode allowed.**
//!
//! It is also scoped by **which path the call arrived on** (issue #674). A
//! policy built by [`ApprovalPolicy::new`] judges an agent turn, where the model
//! picked the tool and the arguments and nobody saw the call first. The workflow
//! gate pass opts into
//! [`for_authored_workflow_nodes`](ApprovalPolicy::for_authored_workflow_nodes),
//! where an operator authored the node past the manifest grant and the authoring
//! refusal — unless its arguments are templated from an upstream node's output,
//! which un-declares it. Only this last arm reads the path; every arm above
//! decides identically on both.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use openhuman_core as oh;

use oh::agent::tool_policy::{ToolPolicy, ToolPolicyDecision, ToolPolicyRequest};

use crate::company::Policy;
use crate::metering::{usd_spent_by_agent, utc_day_start_millis};
use crate::policy::{CallPath, McpReadSet, Standing};
use crate::ports::UsageMeter;
use crate::ports::types::{CompanyId, Effect, EffectGroup, Verdict};
use crate::runtime::grants::{GrantSet, GrantSubject, GrantedCall};

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

/// The four approval tiers, in increasing order of autonomy.
///
/// Three of them mirror OpenHuman's own security tiers by name;
/// [`Auto`](Self::Auto) (issue #560) does not, and is the reason this enum is no
/// longer 1:1 with anything upstream. See
/// [`autonomy_for`](crate::harness::toolbelt) for what that costs at the
/// boundary and how it is paid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyMode {
    /// Read-only: mutating / external-effect tools are denied outright.
    Readonly,
    /// Supervised: external-effect tools require operator approval.
    Supervised,
    /// Auto: the agent's own sandbox writes, outward reads, and workspace
    /// mutations confined to nodes the calling agent created and last wrote run
    /// unattended; anything that leaves the company or spends money still parks.
    ///
    /// The tier line itself is
    /// [`Consequence::parks_under_auto`](crate::policy::Consequence::parks_under_auto),
    /// which reads the existing declaration table rather than adding a list; the
    /// workspace exception is graded at the policy seam before the table verdict
    /// is taken (issue #877).
    Auto,
    /// Full autonomy: tools run without approval (except `always_approve`).
    Full,
}

impl PolicyMode {
    /// Every tier, paired with the manifest word that selects it.
    ///
    /// Exists so the two halves of "a tier is reachable" can be checked against
    /// something that is neither of them. The enum, [`parse`](Self::parse) and
    /// [`POLICY_MODES`](crate::company::POLICY_MODES) are three lists that must
    /// agree; a test that walks any one of them to check the others passes
    /// vacuously when a tier is missing from the list it walked.
    pub const ALL: [(&'static str, PolicyMode); 4] = [
        ("readonly", Self::Readonly),
        ("supervised", Self::Supervised),
        ("auto", Self::Auto),
        ("full", Self::Full),
    ];

    /// Parses a manifest `[policy].mode` string; unknown values fall back to the
    /// safe `Supervised` default.
    ///
    /// The fallback is the *second* line of defence, not the first:
    /// [`Manifest::validate`](crate::company::Manifest) rejects a mode outside
    /// [`POLICY_MODES`](crate::company::POLICY_MODES) before a company ever
    /// loads. This arm catches the paths that reach a `Policy` without going
    /// through validation, and a new tier has to be added in **both** places —
    /// parsing `"auto"` here while the validator still refuses it would make the
    /// tier unreachable from a manifest, which is the only way anyone sets it.
    pub fn parse(mode: &str) -> Self {
        match mode.trim().to_ascii_lowercase().as_str() {
            "readonly" => Self::Readonly,
            "auto" => Self::Auto,
            "full" => Self::Full,
            _ => Self::Supervised,
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

/// Which turn a queued approval request belongs to (issue #439).
///
/// The queue handle is one per company and cannot be otherwise — see
/// [`ApprovalRequestQueue`] — so the separation between turns lives here, in
/// the key, rather than in separate queues. There is no unclaimed bucket: a
/// push outside every claim is refused, because nothing would park it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ApprovalScope {
    /// The company's chat cycle, including every dispatched card and delegated
    /// turn that runs inside it. `CycleRunner` holds a serial lock, so this
    /// bucket has a single writer.
    Cycle,
    /// One workflow run, keyed by its run id. Runs are spawned outside the
    /// cycle lock, so two of them genuinely overlap.
    Run(String),
    /// One hive episode seat turn, keyed by the seat.
    Seat(String),
}

/// What [`ApprovalRequestQueue::push`] did with a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalPush {
    /// Queued in the current claim's bucket, or already there.
    Queued,
    /// Refused: the claim's batch is already at its cap.
    OverCap,
    /// Refused: no claim is active on this task, so nothing would park it.
    Unclaimed,
}

impl ApprovalPush {
    /// Whether the request will reach the operator.
    pub fn is_queued(self) -> bool {
        self == Self::Queued
    }
}

/// A shared, in-memory queue of approval-gated tool calls — the exact
/// [`DelegationQueue`](crate::harness::orchestrator::DelegationQueue) /
/// [`McpFailureQueue`](crate::harness::mcp_probe::McpFailureQueue) pattern.
/// Cheap to [`Clone`] (a shared handle); the [`ApprovalPolicy`] installed on
/// every roster agent and the [`HarnessBrain`](crate::harness::HarnessBrain)
/// that drains it see the same queue because
/// [`HarnessDeps`](crate::harness::HarnessDeps) clones share this handle.
///
/// # One handle, many turns (issue #439)
///
/// The handle is shared and **has to be**. `ApprovalPolicy` is installed once
/// per roster agent inside [`build_roster`](crate::harness::build_roster),
/// which runs in a fingerprint-cached, per-company `HarnessPool::ensure` with
/// no run id in scope, and the policy is then sealed into the vendored agent
/// with no setter. So "one queue per run" cannot be built by handing each run
/// its own queue — there is nowhere to hand it to.
///
/// The separation is therefore in the **key**, not the handle: entries are
/// bucketed by [`ApprovalScope`], a turn declares its scope by taking a
/// [`claim`](Self::claim), and [`push`](Self::push) files into whichever bucket
/// the surrounding claim named. A turn can then only ever see its own entries,
/// which is the property the issue asks for.
#[derive(Clone)]
pub struct ApprovalRequestQueue {
    inner: Arc<Mutex<ApprovalQueueState>>,
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
    ///
    /// **Issue #439 made this sharper, not looser.** Buckets come and go with
    /// the turns that claim them; grants outlive every one of them. A grant is
    /// minted by one turn's decision, redeemed by a *different, later* turn,
    /// swept by a periodic pass, and rehydrated from the journal on boot — so
    /// it belongs to the company, never to a scope. Folding it into the
    /// per-scope map would break redemption in exactly its own happy path, the
    /// same way folding it into `inner` would have. `grants_outlive_a_scope`
    /// pins the #439 half of that alongside `grants_survive_a_queue_clear`.
    grants: GrantSet,
}

#[derive(Default)]
struct ApprovalQueueState {
    buckets: BTreeMap<ApprovalScope, Vec<ApprovalRequest>>,
}

/// What one cycle-end drain took, and what it threw away (issue #561).
///
/// Two numbers rather than one, because the second one is the one an operator
/// needs and never got: `requests` is what they will be asked about, and
/// `discarded` is how many gated calls this turn made that they will **not** be
/// asked about and cannot discover any other way — the queue entries are gone
/// and the turn that produced them is over.
#[derive(Debug, Default)]
pub struct DrainedRequests {
    /// The requests to park, oldest first, at most `cap` of them.
    pub requests: Vec<ApprovalRequest>,
    /// How many were dropped for exceeding the cap. Zero on the ordinary path.
    pub discarded: usize,
    /// The cap this drain was taken against.
    ///
    /// Kept rather than asked for again, so the notice cannot be rendered
    /// against a different number than the one that did the discarding. A
    /// caller holding a `DrainedRequests` from `drain(8)` could otherwise write
    /// `overflow_notice(20)` and hand the operator a confidently-worded, wrong
    /// sentence — the same shape of defect as the invisible discard this type
    /// exists to fix, one level up. Private because the only honest value is
    /// the one [`ApprovalRequestQueue::drain`] already had in hand.
    cap: usize,
}

impl DrainedRequests {
    /// A drain result, for callers that build one directly (tests, and any
    /// future producer that is not the queue).
    ///
    /// Takes `cap` because rendering the notice needs it, and takes it *here*
    /// so it arrives with the count it belongs to rather than at the sentence.
    pub fn new(requests: Vec<ApprovalRequest>, discarded: usize, cap: usize) -> Self {
        Self {
            requests,
            discarded,
            cap,
        }
    }

    /// The cap this drain was taken against.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The operator-facing sentence for a turn that overflowed the cap, or
    /// `None` when nothing was dropped.
    ///
    /// Lives here rather than at the call site so the chat drain and any future
    /// consumer word it the same way, and so the count and the sentence cannot
    /// drift apart.
    ///
    /// It deliberately says what the operator must do about it. "Requests were
    /// discarded" alone invites the reading that the calls happened and only the
    /// records were lost; they did not happen, they were refused, and the only
    /// way to get them is to ask the agent again.
    ///
    /// # "this turn" (issue #439)
    ///
    /// The original wording said "from this turn" and "a single turn", which
    /// was true while the cap only ever bounded a chat cycle. A drain is now
    /// taken per [`ApprovalScope`], and a workflow run's scope spans that run
    /// rather than a turn — so for [`ApprovalScope::Run`] the sentence named
    /// the wrong unit of work.
    ///
    /// It says "one batch" instead: the set of requests this drain took, which
    /// is true of a cycle and of a run alike. The alternative — branching the
    /// sentence on the scope — would put the notice back in the business of
    /// being worded twice, which is precisely what putting it on this type
    /// avoided.
    ///
    /// "at most {cap}" stays lowercase and mid-sentence deliberately:
    /// `the_notice_quotes_the_cap_the_drain_was_taken_against` matches on it,
    /// and that assertion is #561's real guarantee — the cap quoted is the one
    /// the drain was taken against, never a constant the call site had lying
    /// around. Rewording around it beat loosening it.
    ///
    /// # Agreement
    ///
    /// Every countable word in the sentence branches on `n`, verbs and pronouns
    /// included — a single discard reads "1 further gated tool call was not
    /// raised … It was not run". Only the nouns branched at first, so the one
    /// case an operator is most likely to hit read as "1 … call **were** not
    /// raised". The sentence exists to be believed; ungrammatical is a reason
    /// not to believe it.
    pub fn overflow_notice(&self) -> Option<String> {
        (self.discarded > 0).then(|| {
            let n = self.discarded;
            let cap = self.cap;
            let calls = if n == 1 { "call" } else { "calls" };
            let them = if n == 1 { "it" } else { "them" };
            let were = if n == 1 { "was" } else { "were" };
            let they = if n == 1 { "It" } else { "They" };
            let they_are = if n == 1 { "it is" } else { "they are" };
            format!(
                "Heads up: {n} further gated tool {calls} {were} not raised for approval. One \
                 batch can raise at most {cap}, and {n} more needed your sign-off than that. \
                 {they} {were} **not** run and {they_are} **not** on the Approvals page — ask \
                 the agent again to get {them} back."
            )
        })
    }
}

/// A queue with **its own, unshared** [`GrantSet`] — an isolated fixture, never
/// the company's queue.
///
/// Spelled out by hand rather than derived, because the derive made a real trap
/// look like a default. `GrantSet` is a shared handle whose whole purpose is
/// that the runtime that *mints* a grant and the policy that *redeems* it see
/// one set; a queue built here sees neither. Redemption through it silently
/// never matches, so every approval re-parks forever — the feature failing in
/// exactly its own happy path, with no error anywhere.
///
/// Production must use [`with_grants`](ApprovalRequestQueue::with_grants), and
/// does: `RuntimeBuilder` is the single construction site and hands in the
/// runtime's own set. `grants_are_not_shared_by_default` pins the difference so
/// the trap is a stated property rather than a footgun.
///
/// This matters more after issue #439, not less. The obvious way to build a
/// per-run queue is one `default()` per run — which would have scoped grants
/// per run and broken every approval. The chosen design keeps one handle for
/// exactly this reason; see [`ApprovalRequestQueue`].
impl Default for ApprovalRequestQueue {
    fn default() -> Self {
        Self {
            inner: Arc::default(),
            grants: GrantSet::default(),
        }
    }
}

tokio::task_local! {
    /// The scope [`ApprovalRequestQueue::push`] files into, set for the
    /// duration of a turn by [`ApprovalRequestQueue::claim`] (issue #439).
    ///
    /// # Why ambient, and why that is safe here
    ///
    /// `push` happens deep inside `ApprovalPolicy::require_approval`, which
    /// openhuman calls synchronously from the tool loop. The policy is
    /// per-agent, cached, and outlives every run, so it cannot hold a run id —
    /// and there is no parameter to thread one through, because the call comes
    /// from the vendored engine rather than from us.
    ///
    /// A task-local is sound on this path because the turn does not leave its
    /// task: `run_background` → `run_inner` is awaited directly (the one
    /// `tokio::spawn` nearby collects progress events, not the turn), and the
    /// turn body runs inside `with_stop_hooks` on that same task. This is also
    /// not a new dependency — `with_stop_hooks` is itself a task-local scope on
    /// this exact path.
    static CURRENT_SCOPE: ApprovalScope;
    /// Whether this agent turn has asked the operator for approval or an answer.
    static EXPLICIT_REQUEST_PENDING: Cell<bool>;
}

/// A turn's exclusive claim on one [`ApprovalScope`]'s bucket (issue #439).
///
/// Modelled on [`PublishClaim`](crate::harness::publish::PublishClaim) and
/// [`DelegationClaim`](crate::harness::orchestrator::DelegationClaim), which
/// solve the same problem one queue over: the claim's scope **is** the window
/// in which the queue means anything, and `Drop` closes it on the way out so a
/// turn that returned early cannot leave entries behind for the next one to
/// find.
///
/// Obtained from [`ApprovalRequestQueue::claim`]. The turn must run inside
/// [`ApprovalClaim::scoped`] for pushes to be routed to it.
pub struct ApprovalClaim {
    queue: ApprovalRequestQueue,
    scope: ApprovalScope,
}

impl ApprovalClaim {
    /// The scope this claim owns.
    pub fn scope(&self) -> &ApprovalScope {
        &self.scope
    }

    /// Runs `fut` with this claim's scope installed, so every
    /// [`push`](ApprovalRequestQueue::push) inside it files into this bucket.
    ///
    /// The whole turn goes inside. A push that escapes the future is refused
    /// as [`ApprovalPush::Unclaimed`].
    pub async fn scoped<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        CURRENT_SCOPE.scope(self.scope.clone(), fut).await
    }

    /// Drains this claim's own bucket in enqueue order, whatever scope the
    /// calling task has installed.
    pub fn drain(&self, cap: usize) -> DrainedRequests {
        self.queue.drain_scope(&self.scope, cap)
    }
}

impl Drop for ApprovalClaim {
    fn drop(&mut self) {
        self.queue.discard(&self.scope);
    }
}

impl ApprovalRequestQueue {
    /// Enqueues a gated call in the current claim's scope, deduplicated by
    /// effect. Refused as [`ApprovalPush::Unclaimed`] outside every claim.
    pub fn push(&self, request: ApprovalRequest) -> ApprovalPush {
        self.push_with_cap(request, usize::MAX)
    }

    /// Accepts a blocker only within the first eight entries of its scope.
    pub(super) fn push_blocker(&self, request: ApprovalRequest) -> ApprovalPush {
        self.push_with_cap(request, MAX_APPROVAL_REQUESTS_PER_TURN)
    }

    fn push_with_cap(&self, request: ApprovalRequest, cap: usize) -> ApprovalPush {
        let Some(scope) = Self::current_scope() else {
            log::warn!(
                "[approval] '{}' was raised outside any approval claim; refusing it rather than \
                 queueing a request nothing will park",
                request.tool
            );
            return ApprovalPush::Unclaimed;
        };
        let explicit = request.tool == crate::harness::approval_tool::REQUEST_APPROVAL_TOOL
            || request.tool == super::blockers::ESCALATE_TO_HUMAN_TOOL;
        if explicit {
            let _ = EXPLICIT_REQUEST_PENDING.try_with(|pending| pending.set(true));
        }
        let mut guard = self.inner.lock().expect("approval request queue");
        let bucket = guard.buckets.entry(scope).or_default();
        if let Some(position) = bucket.iter().position(|queued| {
            queued.effect.kind == request.effect.kind
                && queued.effect.payload == request.effect.payload
                && queued.effect.agent == request.effect.agent
        }) {
            return if position < cap {
                ApprovalPush::Queued
            } else {
                ApprovalPush::OverCap
            };
        }
        if bucket.len() >= cap {
            return ApprovalPush::OverCap;
        }
        bucket.push(request);
        ApprovalPush::Queued
    }

    /// The scope pushes are currently filing into, or `None` outside every
    /// claim.
    fn current_scope() -> Option<ApprovalScope> {
        CURRENT_SCOPE.try_with(ApprovalScope::clone).ok()
    }

    /// Whether the current turn has asked the operator for approval or an answer.
    fn explicit_request_pending(&self) -> bool {
        EXPLICIT_REQUEST_PENDING
            .try_with(Cell::get)
            .unwrap_or(false)
    }

    /// Runs one real agent turn with a fresh explicit-approval boundary.
    pub async fn turn_scoped<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        EXPLICIT_REQUEST_PENDING.scope(Cell::new(false), fut).await
    }

    /// Takes exclusive ownership of `scope`'s bucket for the life of the
    /// returned claim (issue #439).
    ///
    /// Clears on the way in, and via [`Drop`] on the way out too, so the
    /// claim's lifetime *is* the window in which the scope holds anything. Run
    /// the turn inside [`ApprovalClaim::scoped`] to route its pushes here.
    pub fn claim(&self, scope: ApprovalScope) -> ApprovalClaim {
        self.discard(&scope);
        ApprovalClaim {
            queue: self.clone(),
            scope,
        }
    }

    /// Drops `scope`'s bucket entirely, so an empty scope costs no memory.
    fn discard(&self, scope: &ApprovalScope) {
        self.inner
            .lock()
            .expect("approval request queue")
            .buckets
            .remove(scope);
    }

    /// How many requests sit in `scope`, observed **without** claiming it.
    ///
    /// Test-only: [`claim`](Self::claim) clears on entry, so asserting through
    /// a fresh claim cannot distinguish "`Drop` emptied this" from "my own
    /// claim just did".
    #[cfg(test)]
    fn len_in(&self, scope: &ApprovalScope) -> usize {
        self.inner
            .lock()
            .expect("approval request queue")
            .buckets
            .get(scope)
            .map_or(0, Vec::len)
    }

    /// Empties the current claim's bucket. A no-op outside every claim.
    pub fn clear(&self) {
        if let Some(scope) = Self::current_scope() {
            self.discard(&scope);
        }
    }

    /// Drains the current claim's bucket in enqueue order, counting discarded
    /// overflow. Empty outside every claim.
    ///
    /// A cap below [`MAX_APPROVAL_REQUESTS_PER_TURN`] imposes a smaller limit
    /// than blocker admission; production drains use that constant.
    pub fn drain(&self, cap: usize) -> DrainedRequests {
        match Self::current_scope() {
            Some(scope) => self.drain_scope(&scope, cap),
            None => DrainedRequests::new(Vec::new(), 0, cap),
        }
    }

    fn drain_scope(&self, scope: &ApprovalScope, cap: usize) -> DrainedRequests {
        let mut queued = self
            .inner
            .lock()
            .expect("approval request queue")
            .buckets
            .remove(scope)
            .unwrap_or_default();
        let discarded = queued.len().saturating_sub(cap);
        queued.truncate(cap);
        DrainedRequests::new(queued, discarded, cap)
    }

    /// Builds a queue whose grant set is one the caller already holds.
    ///
    /// The runtime mints and sweeps grants and the policy redeems them, so both
    /// sides must share one set. The builder creates the [`GrantSet`] first
    /// (it is feature-independent, unlike this queue) and hands it in here.
    ///
    /// The workflow gate pass uses it for a second reason (issue #1098): it
    /// needs the company's real grants but must **not** share `inner`, because
    /// `ApprovalPolicy::check` pushes its projected effect there and the shared
    /// queue is drained by `park_gated_calls` and the chat cycle — either of
    /// which would park a second, tool-call-shaped card for a decision the gate
    /// card already covers. A fresh `inner` with the real `grants` is exactly
    /// what this gives, and is why the two halves are separately owned.
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

    /// The number of requests queued **in the current scope**.
    ///
    /// Read by a dispatched card **before** its turns run, so it can tell which
    /// of the entries are its own (issue #242) — [`push`](Self::push) only ever
    /// appends, so a position taken now stays a valid boundary until the
    /// cycle-end drain.
    ///
    /// Issue #439 narrowed what this counts, which is what finally makes the
    /// boundary sound: it used to index into a vector a concurrent workflow run
    /// could append to between the read and the turn, so the position meant
    /// "everything so far, from anyone". Within a scope there is one writer, so
    /// it now means what #242 always assumed it did.
    pub fn queued(&self) -> usize {
        let Some(scope) = Self::current_scope() else {
            return 0;
        };
        self.inner
            .lock()
            .expect("approval request queue")
            .buckets
            .get(&scope)
            .map_or(0, Vec::len)
    }

    /// How many requests queued **since** `from` are blockers (issue #1861).
    ///
    /// Counted the same way [`stamp_run`](Self::stamp_run) stamps — over this
    /// scope's bucket from the same boundary — so the two describe one set of
    /// requests rather than two overlapping ones.
    ///
    /// `run_task` needs it to settle honestly. A turn that raised a question
    /// has not succeeded, and it has not failed either; without this the
    /// ending would come from the turn's own outcome and a turn that escalated
    /// and then produced prose would land `in_review` with an unanswered
    /// question attached to it.
    ///
    /// Keyed on the effect kind's [`BLOCKER_EFFECT_PREFIX`] rather than on a
    /// flag: the kind is what the journal, the console and the approvals feed
    /// all read, so a request that says `blocker.*` to them and something else
    /// here could not happen.
    ///
    /// [`BLOCKER_EFFECT_PREFIX`]: crate::ports::blockers::BLOCKER_EFFECT_PREFIX
    pub fn blockers_since(&self, from: usize) -> usize {
        let Some(scope) = Self::current_scope() else {
            return 0;
        };
        let guard = self.inner.lock().expect("approval request queue");
        let Some(bucket) = guard.buckets.get(&scope) else {
            return 0;
        };
        let prefix = format!("{}.", crate::ports::blockers::BLOCKER_EFFECT_PREFIX);
        bucket
            .iter()
            .skip(from)
            .filter(|entry| entry.effect.kind.starts_with(&prefix))
            .count()
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
    ///
    /// Scoped to the current bucket since #439, so "the entries its own turns
    /// added" is now true by construction rather than by a boundary that a
    /// concurrent run could invalidate.
    pub fn stamp_run(&self, from: usize, run_id: &str) -> usize {
        let Some(scope) = Self::current_scope() else {
            return 0;
        };
        let mut guard = self.inner.lock().expect("approval request queue");
        let Some(bucket) = guard.buckets.get_mut(&scope) else {
            return 0;
        };
        let mut stamped = 0;
        for entry in bucket.iter_mut().skip(from) {
            entry.effect.run_id = Some(run_id.to_string());
            stamped += 1;
        }
        stamped
    }
}

/// openhuman [`ToolPolicy`] derived from a company's manifest `[policy]` and a
/// single agent's per-agent budget.
pub struct ApprovalPolicy {
    /// Whether policy may manufacture HITL requests from ordinary tool calls.
    policy_hitl_enabled: bool,
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
    unenforced_cap_warned: AtomicBool,
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
    /// Which authored workflow this policy instance is gating (issue #1098),
    /// read by the standing-permission arm and by nothing else.
    ///
    /// `None` everywhere except the workflow gate pass, so no other construction
    /// site changes behaviour. Set alongside — never instead of — the absent
    /// [`agent`](Self::agent): a gate still projects an effect with no teammate
    /// on it, because there is none. This names the subject a *standing*
    /// permission may be matched against, and nothing else; see
    /// [`with_workflow`](Self::with_workflow) for why the single-use arm stays
    /// shut.
    workflow: Option<String>,
    /// Which of the two paths the calls this instance judges arrive on (issue
    /// #674), read by the per-call judgement arm and by nothing else.
    ///
    /// [`CallPath::Agent`] — the strict one — at every construction site except
    /// the workflow gate pass, which opts in via
    /// [`for_authored_workflow_nodes`](Self::for_authored_workflow_nodes). A
    /// field rather than a `check` parameter because the trait's signature is
    /// openhuman's, and because the path is a property of the instance: the
    /// gate pass builds its own policy for its own pass, and nothing else shares
    /// it.
    call_path: CallPath,
    /// The operator's per-server declaration of which remote MCP tools only read
    /// (issue #1124), consulted by the tier dispatch to downgrade a
    /// server-declared read-only bridge call so it does not park under `auto`.
    ///
    /// **Empty at every non-harness construction site** — the default — which
    /// keeps every one of them (and the workflow gate, and every test that sets
    /// no declaration) gating every `mcp_call_tool` / `mcp_registry_tool_call`
    /// exactly as before. Only `build_roster` chains
    /// [`with_mcp_reads`](Self::with_mcp_reads), from the company's effective MCP
    /// servers. `consequence_of` cannot read this because it is a pure,
    /// company-blind free function; the declaration is company context, so it
    /// arrives here and the gate applies it through
    /// [`mcp_call_reach`](crate::policy::consequence::mcp_call_reach).
    mcp_reads: McpReadSet,
    /// The shared workspace, when this harness has one. It is queried only for
    /// the four mutation tools' authorship-aware auto-tier exception.
    workspace: Option<WorkspaceReader>,
    /// The company's CONNECTED Composio toolkits (issue #1759, slice S2), used to
    /// deflect a raw web call aimed at one of their API hosts.
    ///
    /// **Empty at every non-harness construction site** — the default — which
    /// keeps every one of them (and every test that sets nothing) letting web
    /// calls through exactly as before. Only `build_roster` chains
    /// [`with_connected_composio_toolkits`](Self::with_connected_composio_toolkits),
    /// from `deps.composio` (the same allowlist S1's `composio_brief` names), and
    /// only when the company has Composio wired. The set is the S1 brief's twin:
    /// S1 tells the agent to route these providers through Composio, this refuses
    /// the raw `http_request` / `curl` / `web_fetch` that ignores it.
    connected_composio_toolkits: Vec<String>,
    /// The company's emergency-stop flag, consulted ahead of the mode dispatch
    /// so a consequential call still refuses under `full` autonomy — the one
    /// tier with no per-call gate to reach `ManifestApprovalGate::evaluate` or
    /// `park` at all.
    ///
    /// `None` at every non-harness construction site and every test with no
    /// company gate to ask, which keeps them dispatching exactly as before.
    /// Only `build_roster` chains
    /// [`with_emergency_gate`](Self::with_emergency_gate), from
    /// `deps.emergency_gate`.
    emergency_gate: Option<Arc<crate::policy::gate::ManifestApprovalGate>>,
}

#[derive(Clone)]
struct WorkspaceReader {
    store: Arc<dyn crate::ports::WorkspaceStore>,
    company: CompanyId,
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
            policy_hitl_enabled: true,
            mode: PolicyMode::parse(&policy.mode),
            always_approve: policy.always_approve.clone(),
            auto_approve_under_usd: policy.auto_approve_under_usd,
            budget_usd_daily,
            requests: ApprovalRequestQueue::default(),
            agent: None,
            workflow: None,
            spend: None,
            no_meter_warned: AtomicBool::new(false),
            unenforced_cap_warned: AtomicBool::new(false),
            // The strict path by default — see `for_authored_workflow_nodes`.
            call_path: CallPath::Agent,
            // No read declaration by default, so every MCP bridge call gates
            // exactly as before — see `with_mcp_reads`.
            mcp_reads: McpReadSet::default(),
            workspace: None,
            // No connected toolkits by default, so the S2 web-deflection arm is
            // inert — see `with_connected_composio_toolkits`.
            connected_composio_toolkits: Vec::new(),
            // No gate by default, so every non-harness construction site and
            // every test with no company to ask dispatches exactly as before —
            // see `with_emergency_gate`.
            emergency_gate: None,
        }
    }

    /// Disables policy-generated approvals while preserving hard denials and
    /// redemption of approvals that were already in flight during migration.
    pub fn with_policy_hitl_disabled(mut self) -> Self {
        self.policy_hitl_enabled = false;
        self
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

    /// Installs the company's emergency-stop flag, so `check` refuses a
    /// consequential call under `full` autonomy the same way `evaluate` and
    /// `park` already refuse one on every other tier.
    pub fn with_emergency_gate(
        mut self,
        gate: Arc<crate::policy::gate::ManifestApprovalGate>,
    ) -> Self {
        self.emergency_gate = Some(gate);
        self
    }

    /// Installs the operator's per-server declaration of which remote MCP tools
    /// only read (issue #1124), so a server-declared read-only bridge call does
    /// not park under `auto`.
    ///
    /// Without this the declaration is empty and every `mcp_call_tool` /
    /// `mcp_registry_tool_call` gates — which is exactly what every non-harness
    /// construction site wants. Chained by `build_roster` alongside
    /// [`with_requests`](Self::with_requests) / [`with_agent`](Self::with_agent),
    /// the same way those are, because the declaration is company context the
    /// pure `consequence_of` cannot see.
    pub fn with_mcp_reads(mut self, reads: McpReadSet) -> Self {
        self.mcp_reads = reads;
        self
    }

    /// The MCP read set bridge calls are graded against.
    #[cfg(test)]
    pub(crate) fn mcp_reads(&self) -> &McpReadSet {
        &self.mcp_reads
    }

    /// Installs the company workspace for authorship-aware mutation grading.
    /// Without it, or without an agent identity, every workspace mutation keeps
    /// the conservative per-call verdict.
    pub fn with_workspace(
        mut self,
        store: Arc<dyn crate::ports::WorkspaceStore>,
        company: CompanyId,
    ) -> Self {
        self.workspace = Some(WorkspaceReader { store, company });
        self
    }

    /// Installs the company's connected Composio toolkits so the S2 guardrail can
    /// deflect a raw web call aimed at one of their API hosts (issue #1759).
    ///
    /// Without this the set is empty and the web-deflection arm is inert — which
    /// is exactly what every non-harness construction site and every test wants:
    /// a build with no Composio configured has no connected provider to route
    /// around, so `http_request` / `curl` / `web_fetch` gate exactly as before.
    /// Chained by `build_roster` from `deps.composio` (the same allowlist S1's
    /// [`composio_brief`](crate::harness::composio_catalog::composio_brief)
    /// names), and only when a toolkit is actually connected.
    pub fn with_connected_composio_toolkits(mut self, toolkits: Vec<String>) -> Self {
        self.connected_composio_toolkits = toolkits;
        self
    }

    /// Binds this policy to the authored workflow whose nodes it is gating, so a
    /// standing permission the operator gave that workflow can be matched (issue
    /// #1098).
    ///
    /// **This opens exactly one arm.** `standing_grant_allows` gains a subject
    /// to match on; [`consume_grant`](Self::consume_grant) is untouched and
    /// still short-circuits on the absent agent, so no single-use grant can ever
    /// be minted or redeemed on this path. That asymmetry is the whole safety
    /// argument and it is not an oversight: a `GrantedCall` is redeemed by
    /// re-dispatching *the agent that asked*, and on a workflow path nobody
    /// asked — such a grant could never be redeemed, would expire on its TTL,
    /// and would tell the operator "the agent did not act" about a call no agent
    /// made. See [`crate::workflows::gate`], which records that reasoning and
    /// whose `agent: None` this deliberately does **not** undo.
    ///
    /// A standing permission has the opposite lifecycle and so is not covered by
    /// that argument: nothing redeems it, it is matched at the gate before the
    /// call and the call then proceeds in place.
    pub fn with_workflow(mut self, workflow_id: impl Into<String>) -> Self {
        self.workflow = Some(workflow_id.into());
        self
    }

    /// Declares that this policy instance judges calls an operator **authored**
    /// into a saved workflow, not calls a model chose during a turn (issue
    /// #674).
    ///
    /// It changes one arm and one arm only: the per-call judgement at the end of
    /// [`check`](ToolPolicy::check). Every arm above — the reserved `never_do`
    /// slot, the `readonly` brake, both grant arms, `always_approve`, the daily
    /// cap, `auto_approve_under_usd` and the tier — decides exactly as it does
    /// on the agent path. An operator who wants a node gated still names it in
    /// `always_approve` and still gets it, on either path.
    ///
    /// **Opt-in, and deliberately so.** [`new`](Self::new) yields the agent
    /// path, which is the strict one, so a construction site that has not
    /// thought about this gets judged in full rather than silently exempted.
    /// Only [`apply_policy_gates`](crate::workflows::gate) calls this, on the
    /// private instance it builds for exactly that pass.
    pub fn for_authored_workflow_nodes(mut self) -> Self {
        self.call_path = CallPath::AuthoredWorkflowNode;
        self
    }

    /// The resolved tier.
    pub fn mode(&self) -> PolicyMode {
        self.mode
    }

    /// Whether this policy turns a classification into an approval request, as
    /// opposed to allowing everything the hard denials do not already refuse.
    ///
    /// The read side of the bypass in [`check`](Self::check): a `needs_approval`
    /// MCP tool mode parks only where this is `true`. Read by
    /// [`roster_approvals_park`](crate::harness::built_in::roster_approvals_park)
    /// so the console states the fact rather than restating the assumption.
    pub fn policy_hitl_enabled(&self) -> bool {
        self.policy_hitl_enabled
    }

    /// The mode handed to OpenHuman's built-in tool security. With policy HITL
    /// disabled, non-readonly tiers use `Full` there too; otherwise an advisory
    /// medium-risk check could recreate an approval prompt below this policy.
    pub fn toolbelt_mode(&self) -> PolicyMode {
        if !self.policy_hitl_enabled && self.mode != PolicyMode::Readonly {
            PolicyMode::Full
        } else {
            self.mode
        }
    }

    /// The per-agent daily budget, if any.
    pub fn budget_usd_daily(&self) -> Option<f64> {
        self.budget_usd_daily
    }

    /// The declared daily cap this call will not be judged against: a priced
    /// call, a cap in the manifest, and policy-generated approvals off, which
    /// leaves `daily_budget_verdict` below the `Allow` that `check` returns
    /// first. `None` when the cap is absent, inapplicable, or in force.
    fn unenforced_daily_cap(&self, tool: &str, args: &serde_json::Value) -> Option<f64> {
        if self.policy_hitl_enabled {
            return None;
        }
        let cap = self.budget_usd_daily?;
        Self::is_priced_call(tool, args, Self::amount_usd(args)).then_some(cap)
    }

    /// Whether `kind` is in the manifest's `always_approve` list.
    ///
    /// Delegates to [`always_approve::matches`](crate::policy::always_approve::matches)
    /// so this path and the native-effect gate
    /// ([`ManifestApprovalGate`](crate::policy::ManifestApprovalGate)) read one
    /// rule. They used to hold two: this one matched the exact kind or a
    /// leading segment, the gate matched exactly — so `always_approve =
    /// ["payment"]` gated a tool call here and silently missed the
    /// identically-named native effect there (issue #684).
    ///
    /// `kind` is the tool name on this path, which is not a coincidence to
    /// paper over: [`effect_for`](Self::effect_for) below projects a flagged
    /// call onto an [`Effect`] by making the tool name the effect kind
    /// verbatim, so the two namespaces the issue describes are one namespace
    /// read twice.
    fn always_requires_approval(&self, kind: &str) -> bool {
        crate::policy::always_approve::matches(&self.always_approve, kind)
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
        // Agent-only, permanently. Do NOT extend this to the workflow subject
        // #1098 added to `standing_grant_allows` — the asymmetry is deliberate.
        // A `GrantedCall` is redeemed by re-dispatching the agent that asked;
        // on a workflow path nobody asked, so one minted here could never be
        // redeemed, would expire on its TTL, and would tell the operator "the
        // agent did not act" about a call no agent made. `crate::workflows::gate`
        // records the full reasoning.
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
        // Issue #1098: an agent when one is installed, else the authored
        // workflow this instance is gating. Neither means no subject to match a
        // permission against, which is every construction site that has set
        // up neither and is why they are unaffected.
        let subject = match (self.agent.as_deref(), self.workflow.as_deref()) {
            (Some(agent), _) => GrantSubject::Agent(agent.to_string()),
            (None, Some(workflow)) => GrantSubject::Workflow(workflow.to_string()),
            (None, None) => return false,
        };
        // Built only where a log line actually needs it — this runs on every
        // gated tool call, and the two refusal paths below are the cold ones.
        let holder = || match &subject {
            GrantSubject::Agent(agent) => format!("agent '{agent}'"),
            GrantSubject::Workflow(workflow) => format!("workflow '{workflow}'"),
        };
        if Self::is_priced_call(tool, args, Self::amount_usd(args)) {
            return false;
        }
        if !crate::policy::consequence_of(tool, args)
            .standing
            .is_grantable()
        {
            log::debug!(
                "[approval] tool '{tool}' holds a standing grant for {} but this \
                 call is not grantable on its own arguments; parking it",
                holder()
            );
            return false;
        }
        // Issue #457: which slice of the tool this *live* call falls in,
        // computed by the same function the mint side used on the parked
        // effect's payload — one function, so the two answers cannot drift into
        // a grant that never matches its own tool.
        let scope = crate::policy::consequence::standing_scope_of(tool, args);
        let Some(grant) = self.requests.grants().match_standing_with_verdict(
            &subject,
            tool,
            scope.as_deref(),
            Verdict::Approve,
            crate::ports::now_millis(),
        ) else {
            return false;
        };
        log::debug!(
            "[approval] tool '{tool}' allowed by standing grant {} for {} \
             (expires at {})",
            grant.id,
            holder(),
            grant.expires_at_millis
        );
        true
    }

    fn standing_deny_applies(&self, tool: &str, args: &serde_json::Value) -> bool {
        // Agents only, on purpose: a standing denial is only *enforced* on the
        // agent turn path, where openhuman treats a `Deny` verdict as
        // fail-closed. The workflow gate deliberately does not honour `Deny`
        // (see `src/workflows/gate.rs`), so minting a standing denial for a
        // workflow would advertise a refusal nothing ever enforces. The mint
        // side refuses those too; this keeps the check from ever *advertising*
        // one even if a stale deny were already in the set.
        let Some(agent) = self.agent.as_deref() else {
            return false;
        };
        let scope = crate::policy::consequence::standing_scope_of(tool, args);
        self.requests
            .grants()
            .match_standing_with_verdict(
                &GrantSubject::Agent(agent.to_string()),
                tool,
                scope.as_deref(),
                Verdict::Deny,
                crate::ports::now_millis(),
            )
            .is_some()
    }

    /// Does this tool call **spend money**? The predicate the daily budget arm
    /// gates on (issue #304).
    ///
    /// Three signals, any of which is enough:
    ///
    /// * the call **declares** an amount (`amount_usd` / `amount`) — the only
    ///   pre-flight signal there is for a payment;
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
    /// Record what a consequence floor would have done with this call, and
    /// change nothing (issue #2147).
    ///
    /// Epic #1817 asks for outward, irreversible acts — publish, send, sign,
    /// hire, identity, spend that leaves — to reach a person whatever tier the
    /// company runs. Since #1925 nothing does: the bypass immediately below
    /// this call site allows every classification-derived decision. Whether to
    /// re-introduce a stop is a product question, and the honest input to it is
    /// how often such a stop would actually fire on live traffic — a
    /// taxonomically narrow floor can still be practically wide.
    ///
    /// So this emits one line per call the floor would have stopped, tagged
    /// with the reason, the tier, and whether HITL was on at the time. Silent
    /// calls emit nothing: the denominator is the tool-call trace the runs
    /// already record, and logging every allowed call to count it would bury
    /// the signal it exists to produce.
    ///
    /// [`floor::deferred_group`] is reported on its own axis so #658's
    /// `publish_artifact` carve-out can be revisited with a number instead of
    /// two opinions — it is counted, never folded into the floor's own count.
    ///
    /// Emitted through `tracing`, at the `policy::shadow_floor` target
    /// `DEFAULT_LOG_FILTER` (`src/bin/opencompany.rs`) names explicitly. The
    /// binary's default filter is bare `error`, and no container image,
    /// compose file or deploy workflow sets `RUST_LOG` — the same shape issue
    /// #450 already found once for the durable-append worker's `warn!` lines.
    /// An `info!` with no matching exception is exactly as silent as those
    /// were: a week of staging traffic would produce this measurement's
    /// entire denominator and record none of it.
    fn record_shadow_floor(&self, tool: &str, args: &serde_json::Value) {
        // The per-call allowance, which is the cap a single call is measured
        // against. The daily budget is a different question and has its own arm.
        let verdict = crate::policy::floor::evaluate(tool, args, self.auto_approve_under_usd);
        if verdict.requires_human() {
            tracing::info!(
                target: "policy::shadow_floor",
                "[policy:shadow-floor] agent={} tool='{}' would_stop={} mode={:?} hitl={} issue=2147",
                self.agent.as_deref().unwrap_or("-"),
                tool,
                verdict.reason_word(),
                self.mode,
                self.policy_hitl_enabled,
            );
        }
        if let Some(group) = crate::policy::floor::deferred_group(tool, args) {
            tracing::info!(
                target: "policy::shadow_floor",
                "[policy:shadow-floor] agent={} tool='{}' deferred_group={:?} mode={:?} hitl={} \
                 issue=2147",
                self.agent.as_deref().unwrap_or("-"),
                tool,
                group,
                self.mode,
                self.policy_hitl_enabled,
            );
        }
    }

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
    /// An **executed** payment. The ledger carries no agent, so there is
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
    /// record the projected effect on the approval queue so the brain can park
    /// it after the turn, then return the decision openhuman blocks the call
    /// with.
    ///
    /// The entry lands in the surrounding turn's
    /// [`ApprovalScope`] (issue #439). This function is the reason the scope is
    /// ambient rather than a parameter: it is called synchronously by the
    /// vendored tool loop, through a per-agent policy that outlives every run,
    /// so there is no argument here that could carry a run id.
    ///
    /// Every `RequireApproval` arm of [`check`](ToolPolicy::check) goes through
    /// here — a decision that skipped it would refuse the tool without ever
    /// reaching the operator, which is exactly the bug this closes.
    ///
    /// Outside every approval claim nothing would park the request, so the
    /// call is denied outright and the reason says so.
    fn require_approval(
        &self,
        tool: &str,
        args: &serde_json::Value,
        reason: String,
    ) -> ToolPolicyDecision {
        let pushed = self.requests.push(ApprovalRequest {
            tool: tool.to_string(),
            reason: reason.clone(),
            effect: self.effect_for(tool, args),
        });
        if pushed == ApprovalPush::Unclaimed {
            return ToolPolicyDecision::deny(unrecorded_approval_reason(tool, &reason));
        }
        log::debug!(
            "[approval] tool '{tool}' requires operator approval — queued to park ({reason})"
        );
        ToolPolicyDecision::require_approval(reason)
    }

    /// What this call reaches, with the one company-context downgrade the pure
    /// [`consequence_of`](crate::policy::consequence_of) cannot make: an MCP
    /// bridge call whose remote tool the operator has declared read-only on this
    /// server (issue #1124).
    ///
    /// Every non-bridge tool takes the plain classifier, so nothing else changes.
    /// A bridge call is graded by [`mcp_call_reach`](crate::policy::consequence::mcp_call_reach),
    /// which downgrades to [`Reach::ExternalRead`](crate::policy::Reach::ExternalRead)
    /// only on affirmative membership of this policy's declaration and returns the
    /// gated base otherwise — so an undeclared server, an unreadable argument, or
    /// a policy with no declaration all keep the parking verdict this arm read
    /// before.
    async fn consequence_for(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> crate::policy::Consequence {
        if crate::policy::consequence::is_mcp_bridge_tool(tool) {
            return crate::policy::consequence::mcp_call_reach(tool, args, &self.mcp_reads);
        }
        let consequence = crate::policy::consequence_of(tool, args);
        // Only `auto` consumes the ownership downgrade: `full` ignores the
        // consequence entirely and `supervised`/`readonly` read only `reach`.
        // Reading the whole workspace tree to grade authorship in those modes
        // is pure cost, so the lookup is gated on the one mode that uses it
        // (issue #877).
        if self.mode != PolicyMode::Auto {
            return consequence;
        }
        let Some(workspace) = self.workspace.as_ref() else {
            return consequence;
        };
        let Some(agent) = self.agent.as_deref() else {
            return consequence;
        };
        if !matches!(
            tool.to_ascii_lowercase().as_str(),
            "workspace_create" | "workspace_write" | "workspace_delete" | "workspace_rename"
        ) {
            return consequence;
        }
        if crate::harness::built_in::workspace_tools::mutation_is_owned_by_agent(
            &workspace.store,
            &workspace.company,
            agent,
            tool,
            args,
        )
        .await
        {
            return crate::policy::Consequence {
                standing: crate::policy::Standing::Grantable,
                ..consequence
            };
        }
        consequence
    }

    /// Does the run this call is executing inside already carry an operator's
    /// consent for it (issue #2150, Rung 3 of epic #1817)?
    ///
    /// Consulted only from inside the `auto` / `supervised` mode arms of
    /// [`check`](ToolPolicy::check), which is what keeps this a narrowing: it
    /// can turn a park those two tiers would otherwise raise into an allow,
    /// and it is never reached for `readonly` (denies before the mode
    /// dispatch) or `full` (never parks here at all). Everything above the
    /// mode dispatch — the reserved `never_do` slot, the S2 web-deflection
    /// deny, the `readonly` brake, both grant arms, `always_approve`, the
    /// daily cap — has already had its say and none of it reads this.
    ///
    /// [`crate::harness::built_in::run_origin::current`] is fail-closed by
    /// construction, so every early `false` below is this predicate refusing
    /// to admit rather than the origin failing to supply an answer.
    fn trusted_dispatch_admits(
        &self,
        tool: &str,
        args: &serde_json::Value,
        consequence: crate::policy::Consequence,
    ) -> bool {
        // Issue #674's split, reasserted here rather than merely documented:
        // `judge` is silent on an authored workflow node, so admitting one
        // through trust as well would remove the ceiling `always_approve`
        // still leaves on that path. `for_authored_workflow_nodes` is the only
        // constructor that sets this, and this arm must never fire for it.
        if self.call_path != CallPath::Agent {
            return false;
        }
        // `Standing::PerCall` is exactly "every call is its own decision" —
        // the set an operator could never have handed over ahead of time, so
        // there is nothing here for a dispatched run to have inherited.
        if consequence.standing == Standing::PerCall {
            return false;
        }
        // The consequence floor outranks every trust this module hands out.
        // `None` for the cap matches `judge`'s own reading — any declared
        // amount stops.
        //
        // It cannot change an outcome today, and that is worth saying rather
        // than discovering. Two reasons, and both could stop being true:
        // every tool the floor names is `PerCall`, which the arm above already
        // refused (the one `Grantable` tool with a consequence group,
        // `publish_artifact`, is deferred by #658 so the floor is silent on
        // it); and the money arm is caught anyway by `judge` at the tail of
        // `check`, which re-judges whatever the tier allowed.
        //
        // Removing it therefore keeps the suite green — verified, not assumed.
        // It stays because the second reason is positional: if epic #1817's
        // floor becomes an enforced arm ABOVE the `policy_hitl_enabled`
        // bypass, `judge`'s tail placement no longer covers a call this
        // function admitted, and this line is what keeps trust from outranking
        // the floor on that day. The first reason is one `DECLARED` edit away
        // from changing on its own.
        if crate::policy::floor::evaluate_consequence(tool, consequence, args, None)
            .requires_human()
        {
            return false;
        }
        let crate::harness::built_in::run_origin::RunOrigin::Dispatched { agent, scope, .. } =
            crate::harness::built_in::run_origin::current()
        else {
            return false;
        };
        // The dispatched agent, not this call's arguments, is what earned the
        // trust — and the checking policy's own `self.agent` is the identity
        // actually making this call. A delegate running under an origin its
        // dispatch never named is exactly the escalation `run_origin`'s own
        // docs warn against; the mismatch here is what stops it.
        if self.agent.as_deref() != Some(agent.as_str()) {
            return false;
        }
        match consequence.standing {
            Standing::Grantable => true,
            Standing::ScopedGrantable => {
                // A scope that cannot be derived refuses rather than admits —
                // the same direction `StandingGrant::admits_scope` takes for a
                // live call whose scope reads `None` against a scoped grant.
                let Some(call_scope) = crate::policy::consequence::standing_scope_of(tool, args)
                else {
                    return false;
                };
                scope.as_deref() == Some(call_scope.as_str())
            }
            Standing::PerCall => unreachable!("returned above"),
        }
    }
}

#[async_trait]
impl ToolPolicy for ApprovalPolicy {
    fn name(&self) -> &str {
        POLICY_NAME
    }

    async fn check(&self, request: &ToolPolicyRequest) -> ToolPolicyDecision {
        let tool = request.tool_name.as_str();

        if self.requests.explicit_request_pending() {
            return ToolPolicyDecision::deny(format!(
                "'{tool}' was not run because this turn already asked the operator for approval or an answer; \
                 stop and wait for the decision"
            ));
        }

        // Asking the operator is never itself an effect to approve or deny.
        // The first call queues a question; a second call in the same turn was
        // refused by the boundary above.
        if tool == crate::harness::approval_tool::REQUEST_APPROVAL_TOOL {
            return ToolPolicyDecision::Allow;
        }

        // The emergency stop (issue #86), computed and enforced HERE — above
        // every unconditional `Allow` this function can still reach below: a
        // redeemed single-use grant, a standing grant, `policy_hitl_enabled ==
        // false`, and `auto_approve_under_usd`. `ManifestApprovalGate` enforces
        // the same veto at `evaluate`/`park`, but a harness tool call never
        // reaches either — `check` decides `Allow` on its own, so any one of
        // those branches returning first is a live bypass, not just `full`
        // autonomy's blanket allow. Without this, an in-flight turn that
        // survives the stop (by design — see
        // `CompanyRuntime::ensure_not_emergency_stopped`) could still dispatch
        // a consequential tool the containment story assumes the gate denies.
        // `EffectGroup::Other` stays exempt, matching `evaluate`/`park`.
        //
        // `consequence_for` is computed here rather than at its original site
        // near the mode dispatch, and reused there (see `let reach =
        // consequence.reach;` below) — it depends only on `tool`/`args` and
        // `self.mode`/`self.workspace`/`self.agent`, none of which the branches
        // between here and there can change.
        let consequence = self.consequence_for(tool, &request.arguments).await;
        if consequence.group != EffectGroup::Other
            && self
                .emergency_gate
                .as_deref()
                .is_some_and(|gate| gate.is_emergency())
        {
            return ToolPolicyDecision::deny(format!(
                "'{tool}' was not run because the company is stopped and will run no work \
                 until an operator releases it"
            ));
        }

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

        // 0.5. S2 web-deflection (issue #1759): a raw web call aimed at a
        //      CONNECTED Composio provider's API host is refused, with the
        //      Composio route named.
        //
        // Defense-in-depth behind S1's routing brief: even when the agent ignores
        // the prompt, `http_request` / `curl` / `web_fetch` to `api.github.com`
        // and its kin are blocked, because unauthenticated they only ever 401/403
        // — the connection credential lives in Composio, not the web tools. The
        // observed failure was exactly this: a raw `http_request` to
        // `api.github.com` → 403.
        //
        // ABOVE the grant arms, and for the same reason the reserved `never_do`
        // slot is: a grant is an operator approving one specific call, but this
        // call cannot succeed by this route for anyone, so no grant should smuggle
        // it past the block. A hard deny, never a rewrite — the guardrail points
        // at the right door rather than silently walking the agent through it.
        //
        // Scoped strictly to hosts of toolkits this company actually connected
        // (the field is empty unless `build_roster` wired a live Composio set),
        // so a provider host whose toolkit is NOT connected — and every
        // non-provider host — passes through untouched (requirement #2). The
        // decision and the host table live in `composio_catalog`; the web-tool
        // family lives in `toolbelt`; this arm only joins them.
        if !self.connected_composio_toolkits.is_empty()
            && crate::harness::toolbelt::is_web_request_tool(tool)
        {
            // Every tool this arm recognises declares `url` as a REQUIRED
            // string in its own schema, so a call that does not carry one that
            // way is not a legitimate call this guardrail failed to reach — it
            // is malformed relative to the tool's own contract. Falling
            // through silently would let exactly that malformed shape walk
            // past the one thing standing between `full` autonomy and a
            // connected provider's API host, so this arm fails CLOSED on it
            // instead of treating "could not read a url" as "nothing to
            // check".
            match request.arguments.get("url").and_then(|v| v.as_str()) {
                Some(url) => {
                    if let Some(reason) = crate::harness::composio_catalog::web_call_deflection(
                        &self.connected_composio_toolkits,
                        url,
                    ) {
                        return ToolPolicyDecision::deny(reason);
                    }
                }
                None => {
                    return ToolPolicyDecision::deny(format!(
                        "'{tool}' must be called with `url` as a plain string so it can be \
                         checked against this company's connected toolkits; retry with a \
                         string `url`"
                    ));
                }
            }
        }

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
                 so an earlier approval does not apply{}",
                readonly_denial_suffix(tool)
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

        if self.standing_deny_applies(tool, &request.arguments) {
            return ToolPolicyDecision::deny(format!(
                "'{tool}' is denied by a standing permission; it will ask again when that refusal expires or is revoked"
            ));
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

        // Paid media tools are explicitly approval-producing tool calls: their
        // own invocation stages the concrete generation request before the
        // backend can bill it. This is tool behavior, not risk-classifier HITL.
        // An exact one-shot grant was consumed above on the approved re-issue.
        if matches!(tool, "media_generate_image" | "media_generate_video") {
            return self.require_approval(
                tool,
                &request.arguments,
                format!("'{tool}' explicitly stages a paid media generation for approval"),
            );
        }

        // Shadow measurement for issue #2147 — reads, records, decides nothing.
        //
        // Immediately ABOVE the bypass because that is the position the
        // consequence floor epic #1817 proposes for a real arm, and a
        // measurement taken anywhere else would be measuring a different
        // question. Everything above has already returned for the calls it owns
        // — a hard deny, a redeemed grant, a paid-media card — so what reaches
        // here is exactly the population a floor would decide.
        //
        // It must stay incapable of changing an outcome: no early return, no
        // mutation, no queue write. The only observable is a log line.
        self.record_shadow_floor(tool, &request.arguments);

        // General approvals come only from `request_approval`; specialized
        // approval-producing tools such as paid media stage themselves above.
        // Keep the readonly brake and old-grant redemption above this point,
        // but bypass every arm below that would turn classification into HITL.
        if !self.policy_hitl_enabled {
            if let Some(cap) = self.unenforced_daily_cap(tool, &request.arguments)
                && !self.unenforced_cap_warned.swap(true, Ordering::Relaxed)
            {
                let agent = self.agent.as_deref().unwrap_or("<unnamed>");
                log::warn!(
                    "[approval] agent '{agent}' declares a daily budget of ${cap:.2}, but \
                     policy-generated approvals are disabled on this host, so the cap does not \
                     gate priced tool calls such as '{tool}'"
                );
            }
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
        // `consequence` was computed, and the emergency stop already enforced
        // against it, above — see the comment there for why this moved.
        let reach = consequence.reach;
        let by_mode = match self.mode {
            PolicyMode::Full => ToolPolicyDecision::Allow,
            // `auto` sits between the two (issue #560): the agent's own sandbox
            // writes, its outward reads, and workspace mutations confined to its
            // own nodes run unattended, and anything that leaves the company or
            // spends on submit still parks. The line is drawn by
            // `parks_under_auto`, which reads the same declaration table this
            // arm's neighbours read — see there for why it reuses `Standing`
            // rather than introducing a second list, and for the two boundaries
            // it deliberately does not draw. The workspace exception is the one
            // company-context downgrade, graded by `consequence_for` before the
            // table verdict is taken (issue #877).
            PolicyMode::Auto => {
                if !consequence.parks_under_auto()
                    || self.trusted_dispatch_admits(tool, &request.arguments, consequence)
                {
                    ToolPolicyDecision::Allow
                } else {
                    self.require_approval(
                        tool,
                        &request.arguments,
                        format!(
                            "'{tool}' leaves the company or spends money, and this desk runs auto"
                        ),
                    )
                }
            }
            PolicyMode::Supervised => {
                if !reach.parks_under_supervision()
                    || self.trusted_dispatch_admits(tool, &request.arguments, consequence)
                {
                    ToolPolicyDecision::Allow
                } else {
                    self.require_approval(
                        tool,
                        &request.arguments,
                        format!("'{tool}' has an external effect and this desk runs supervised"),
                    )
                }
            }
            PolicyMode::Readonly => {
                if reach.denied_under_readonly() {
                    ToolPolicyDecision::deny(format!(
                        "'{tool}' mutates or reaches outside; this desk is read-only{}",
                        readonly_denial_suffix(tool)
                    ))
                } else {
                    ToolPolicyDecision::Allow
                }
            }
        };

        // 5. Per-call judgement (issue #338): the last word, and only ever a
        //    stricter one.
        //
        // LAST, and gated on the mode having already said Allow, because that
        // placement *is* the safety argument. Static configuration stays
        // authoritative everywhere it speaks: the reserved `never_do` slot, the
        // `readonly` brake, a redeemed grant, `always_approve`, the daily cap
        // and `auto_approve_under_usd` have all had their say above and every
        // one of them returned before reaching here. This can turn an Allow
        // into a stop; it can never turn a deny into an allow, skip
        // `always_approve`, or spend a grant.
        //
        // The invariant, phrased to survive the next tier: this arm only ever
        // speaks where the mode allowed. A tier that already parks or denies a
        // call keeps its own answer — deliberately, so a tier an operator has
        // already reasoned about does not shift under them.
        //
        // Today that leaves `full` alone. `supervised` parks a consequence,
        // `readonly` denies it, and `auto` (issue #560) parks everything that
        // is not `Grantable` — which covers every rule `judge` applies, so it
        // adds nothing there. That last one holds by a property of the
        // declaration table rather than by construction, so it is pinned by
        // `the_arm_adds_nothing_under_auto` instead of asserted here.
        //
        // `full`'s contract is "act without asking, EXCEPT the few things on
        // the always-ask list", and this is what makes that exception mean
        // something without requiring an operator to have anticipated each one
        // by name.
        if matches!(by_mode, ToolPolicyDecision::Allow)
            && let Some(stop) =
                crate::policy::judge(tool, &request.arguments, self.call_path).stop_reason()
        {
            return self.require_approval(
                tool,
                &request.arguments,
                format!("'{tool}' {}", stop.describe()),
            );
        }

        by_mode
    }
}

/// The ", because …" a `readonly` denial carries when the tool's name argues
/// against its classification (issue #459), or an empty string.
///
/// `readonly` denying `read_workspace_state` is the case this exists for: the
/// operator sees a tier that promises reads still work refuse something called
/// `read_*`, and "mutates or reaches outside" alone reads as a bug in the tier.
/// The reason lives in [`crate::policy::consequence::denial_reason`], next to
/// the classification it explains, so the two cannot drift.
/// The deny reason for a gated call raised where no approval can be recorded.
pub(crate) fn unrecorded_approval_reason(tool: &str, reason: &str) -> String {
    format!(
        "'{tool}' needs operator approval ({reason}), but this turn cannot record an approval \
         request, so the call was refused and nobody was asked. Do not tell anyone you asked \
         for approval."
    )
}

fn readonly_denial_suffix(tool: &str) -> String {
    crate::policy::consequence::denial_reason(tool)
        .map(|why| format!(" — {why}"))
        .unwrap_or_default()
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
#[path = "policy/policy_spend_cap_tests.rs"]
mod policy_spend_cap_tests;
#[cfg(test)]
#[cfg(test)]
#[path = "policy/policy_test_helpers_tests.rs"]
pub(crate) mod policy_test_helpers_tests;
#[cfg(test)]
#[path = "policy/policy_autonomy_tests.rs"]
mod tests_autonomy;
#[cfg(test)]
#[path = "policy/policy_call_judgement_tests.rs"]
mod tests_call_judgement;
#[cfg(test)]
#[path = "policy/policy_effect_classification_tests.rs"]
mod tests_effect_classification;
#[cfg(test)]
#[path = "policy/policy_escalation_tests.rs"]
mod tests_escalation;
#[cfg(test)]
#[path = "policy/policy_grant_redemption_tests.rs"]
mod tests_grant_redemption;
#[cfg(test)]
#[path = "policy/policy_hitl_tests.rs"]
mod tests_hitl;
#[cfg(test)]
#[path = "policy/policy_park_queue_tests.rs"]
mod tests_park_queue;
#[cfg(test)]
#[path = "policy/policy_path_split_tests.rs"]
mod tests_path_split;
#[cfg(test)]
#[path = "policy/policy_scopes_tests.rs"]
mod tests_scopes;
#[cfg(test)]
#[path = "policy/policy_ssrf_guardrail_tests.rs"]
mod tests_ssrf_guardrail;
#[cfg(test)]
#[path = "policy/policy_standing_grants_tests.rs"]
mod tests_standing_grants;
#[cfg(test)]
#[path = "policy/policy_standing_grants_more_tests.rs"]
mod tests_standing_grants_more;
