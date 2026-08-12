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

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use openhuman_core::openhuman as oh;

use oh::agent::tool_policy::{ToolPolicy, ToolPolicyDecision, ToolPolicyRequest};

use crate::company::Policy;
use crate::metering::{usd_spent_by_agent, utc_day_start_millis};
use crate::policy::CallPath;
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
    /// Auto: the agent's own sandbox writes and outward reads run unattended;
    /// anything that leaves the company or spends money still parks.
    ///
    /// The tier line itself is
    /// [`Consequence::parks_under_auto`](crate::policy::Consequence::parks_under_auto),
    /// which reads the existing declaration table rather than adding a list.
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
/// the key, rather than in separate queues.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ApprovalScope {
    /// A request pushed outside any [`claim`](ApprovalRequestQueue::claim).
    ///
    /// The default, and deliberately **not** an error. Every production turn
    /// runs under a claim, so this bucket should stay empty there; making an
    /// unclaimed push panic or drop would trade a mis-attribution bug for a
    /// lost-approval bug, and #395 exists because a lost approval is the worse
    /// one. It is drained by the chat cycle alongside [`Cycle`](Self::Unscoped)
    /// — which is exactly the pre-#439 behaviour for anything unclaimed.
    #[default]
    Unscoped,
    /// The company's chat cycle, including every dispatched card and delegated
    /// turn that runs inside it.
    ///
    /// One at a time per company: `CycleRunner` holds a serial lock, so this
    /// bucket has a single writer. It is still concurrent with any number of
    /// workflow runs, which is the race #439 is about.
    Cycle,
    /// One workflow run, keyed by its run id.
    ///
    /// Workflow runs are `tokio::spawn`ed and are **not** under the cycle lock,
    /// so two of them genuinely overlap. That is the race a boundary index
    /// could never fix: both runs take a boundary against one shared vector and
    /// the later `split_off` swallows the earlier run's tail.
    Run(String),
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
    inner: Arc<Mutex<BTreeMap<ApprovalScope, Vec<ApprovalRequest>>>>,
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
    /// The whole turn goes inside. A push that escapes the future lands in
    /// [`ApprovalScope::Unscoped`] rather than in another turn's bucket — the
    /// conservative direction, since the cycle drains that too.
    pub async fn scoped<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        CURRENT_SCOPE.scope(self.scope.clone(), fut).await
    }
}

impl Drop for ApprovalClaim {
    fn drop(&mut self) {
        // The exit half, and the load-bearing one: a turn that returned early —
        // an error, a steer, a panic unwinding through here — must not leave
        // its entries for whoever claims this scope next. A workflow run id is
        // unique, so this is belt-and-braces there; for `Cycle` it is the
        // guarantee that replaced the explicit `clear()` at the top of
        // `run_cycle`.
        self.queue.discard(&self.scope);
    }
}

impl ApprovalRequestQueue {
    /// Records a gated call, ignoring one already queued for the same tool and
    /// arguments.
    ///
    /// openhuman blocks the call but lets the turn continue, so a model that
    /// re-tries the same tool would otherwise park the identical request several
    /// times over and show the operator a queue of duplicates.
    /// Records a gated call **in the surrounding claim's scope** (issue #439),
    /// ignoring one already queued in that same scope for the same tool and
    /// arguments.
    ///
    /// openhuman blocks the call but lets the turn continue, so a model that
    /// re-tries the same tool would otherwise park the identical request several
    /// times over and show the operator a queue of duplicates. De-duplication is
    /// per scope, which is the only reading that makes sense once buckets are
    /// separate: two different turns asking for the same tool are two requests,
    /// and collapsing them would hide one turn's ask behind another's.
    pub fn push(&self, request: ApprovalRequest) {
        let scope = Self::current_scope();
        let mut guard = self.inner.lock().expect("approval request queue");
        let bucket = guard.entry(scope).or_default();
        if bucket.iter().any(|q| {
            q.effect.kind == request.effect.kind && q.effect.payload == request.effect.payload
        }) {
            return;
        }
        bucket.push(request);
    }

    /// The scope pushes are currently filing into.
    ///
    /// Outside any claim — every test that pushes directly, and any turn entry
    /// point not yet under one — this is [`ApprovalScope::Unscoped`], which the
    /// chat cycle drains. That fallback is why adding a scope cannot lose a
    /// request.
    fn current_scope() -> ApprovalScope {
        CURRENT_SCOPE
            .try_with(ApprovalScope::clone)
            .unwrap_or_default()
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
            .remove(scope);
    }

    /// How many requests sit in `scope`, observed **without** claiming it.
    ///
    /// Test-only, and it exists because claiming is not a neutral observation:
    /// [`claim`](Self::claim) clears on entry, so asserting through a fresh
    /// claim cannot distinguish "`Drop` emptied this" from "my own claim just
    /// did". Reading the map directly is the only way to test the exit half.
    #[cfg(test)]
    fn len_in(&self, scope: &ApprovalScope) -> usize {
        self.inner
            .lock()
            .expect("approval request queue")
            .get(scope)
            .map_or(0, Vec::len)
    }

    /// Empties the current scope's bucket.
    ///
    /// Retained for the callers that clear without claiming. Under a claim this
    /// is redundant — [`claim`](Self::claim) already clears on entry and `Drop`
    /// clears on exit.
    pub fn clear(&self) {
        self.discard(&Self::current_scope());
    }

    /// Drains up to `cap` requests (FIFO) from the **current scope**, discarding
    /// that scope's remainder, so one turn can never flood the operator's queue.
    ///
    /// # Why this returns a struct rather than a `Vec` (issue #561)
    ///
    /// The discard is the whole point of the cap and it used to be invisible:
    /// this method dropped the overflow on the floor and handed back a `Vec`
    /// that looked exactly like a complete one, so the operator was shown eight
    /// cards and no indication that five more calls had been gated. `cap`
    /// travels into the result so the count and the number that produced it
    /// stay one value — see [`DrainedRequests`].
    ///
    /// # What #439 changed, and what it did not
    ///
    /// The shape is #561's; only *which* requests it can see is #439's. It used
    /// to drain one company-wide vector, which is why a concurrent turn's
    /// entries could be taken by whoever drained first. It now sees the calling
    /// turn's bucket and nothing else — **which also makes `discarded` mean
    /// something it could not mean before**. A count taken off a shared vector
    /// mixed in whatever a concurrent run had appended, so "this turn
    /// overflowed" was never reliably this turn's fact. Scoped, it is.
    ///
    /// From the chat cycle this also drains [`ApprovalScope::Unscoped`], so a
    /// push from any turn entry point not yet under a claim still reaches the
    /// operator exactly as it did before — the fallback that makes #439
    /// non-lossy. A workflow run drains only its own bucket and can no longer
    /// swallow anyone else's.
    pub fn drain(&self, cap: usize) -> DrainedRequests {
        let scope = Self::current_scope();
        let mut guard = self.inner.lock().expect("approval request queue");
        let mut queued: Vec<ApprovalRequest> = guard.remove(&scope).unwrap_or_default();
        // The cycle owns anything nobody claimed. A workflow run must not take
        // it: that would be the shared-queue theft this issue removes.
        if scope == ApprovalScope::Cycle {
            queued.extend(guard.remove(&ApprovalScope::Unscoped).unwrap_or_default());
        }
        let discarded = queued.len().saturating_sub(cap);
        queued.truncate(cap);
        DrainedRequests::new(queued, discarded, cap)
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
        self.inner
            .lock()
            .expect("approval request queue")
            .get(&Self::current_scope())
            .map_or(0, Vec::len)
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
        let scope = Self::current_scope();
        let mut guard = self.inner.lock().expect("approval request queue");
        let Some(bucket) = guard.get_mut(&scope) else {
            return 0;
        };
        let mut stamped = 0;
        for request in bucket.iter_mut().skip(from) {
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
            // The strict path by default — see `for_authored_workflow_nodes`.
            call_path: CallPath::Agent,
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

    /// The per-agent daily budget, if any.
    pub fn budget_usd_daily(&self) -> Option<f64> {
        self.budget_usd_daily
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
        let consequence = crate::policy::consequence_of(tool, &request.arguments);
        let reach = consequence.reach;
        let by_mode = match self.mode {
            PolicyMode::Full => ToolPolicyDecision::Allow,
            // `auto` sits between the two (issue #560): the agent's own sandbox
            // writes and its outward reads run unattended, and anything that
            // leaves the company or spends on submit still parks. The line is
            // drawn by `parks_under_auto`, which reads the same declaration
            // table this arm's neighbours read — see there for why it reuses
            // `Standing` rather than introducing a second list, and for the two
            // boundaries it deliberately does not draw.
            PolicyMode::Auto => {
                if consequence.parks_under_auto() {
                    self.require_approval(
                        tool,
                        &request.arguments,
                        format!(
                            "'{tool}' leaves the company or spends money, and this desk runs auto"
                        ),
                    )
                } else {
                    ToolPolicyDecision::Allow
                }
            }
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
mod tests {
    use super::*;
    use oh::agent::tool_policy::{ToolCallContext, ToolPolicyRequest};

    // Issue #470: the `composio_execute` fixtures are built here, from the same
    // key the classifier reads, so a call in a test reaches the same catalogue
    // lookup a call in production does.
    use crate::policy::test_support::{
        COMPOSIO_OTHER_SEND_SLUG, COMPOSIO_READ_SLUG, COMPOSIO_SEND_SLUG, composio_args,
        composio_read_args, composio_send_args, composio_unclassified_args,
        composio_unclassified_args_numbered,
    };

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

    /// Every tier is reachable from a manifest, parses to its own variant, and
    /// nothing else parses to any of them.
    ///
    /// This replaces `mode_maps_one_to_one_to_security_tiers`, which asserted
    /// the same thing through `PolicyMode::security_tier()` — a `&'static str`
    /// getter whose only caller in the tree was that test. It was deleted with
    /// issue #560 rather than given a fourth arm: its entire premise was a 1:1
    /// correspondence with OpenHuman's security-tier words, and `auto` has no
    /// such word. The two available arms were both wrong — `"auto"` names a tier
    /// upstream does not have, and `"supervised"` breaks the 1:1 the function
    /// documented. Keeping a dead accessor alive by making it lie is worse than
    /// the deletion.
    ///
    /// # Why it walks [`PolicyMode::ALL`] and not [`POLICY_MODES`]
    ///
    /// The first draft of this test walked `POLICY_MODES`, and a revert-and-check
    /// caught it passing vacuously: deleting `"auto"` from that list does not
    /// break a test that derives its cases from it — it just stops testing
    /// `auto`. The same shape hid the trap this whole change turns on, since
    /// `PolicyMode::parse` is never reached for a word the validator rejects.
    ///
    /// So the cases come from a third list, and the two under test are checked
    /// against it in both directions: `ALL` → `parse`, and `ALL` ↔
    /// `POLICY_MODES` by membership *and* by length, so a word in one and not
    /// the other cannot pass. Adding a variant without extending `ALL` is caught
    /// by the exhaustive `match` below, which the compiler will refuse.
    #[test]
    fn every_tier_is_reachable_from_a_manifest_and_parses_to_itself() {
        use crate::company::POLICY_MODES;

        // A new variant makes this match non-exhaustive — the compiler forces
        // whoever adds it to come here, and the length assertions below then
        // fail until `ALL` and `POLICY_MODES` are both extended.
        for (word, mode) in PolicyMode::ALL {
            let expected = match mode {
                PolicyMode::Readonly => "readonly",
                PolicyMode::Supervised => "supervised",
                PolicyMode::Auto => "auto",
                PolicyMode::Full => "full",
            };
            assert_eq!(word, expected, "ALL pairs `{word}` with the wrong variant");

            assert_eq!(
                PolicyMode::parse(word),
                mode,
                "`{word}` does not parse to {mode:?} — it is silently downgraded"
            );
            assert!(
                POLICY_MODES.contains(&word),
                "`{word}` is a tier the runtime knows but the manifest validator rejects — \
                 unreachable from a company.toml, and no policy-side test would notice"
            );
        }
        assert_eq!(
            POLICY_MODES.len(),
            PolicyMode::ALL.len(),
            "POLICY_MODES {POLICY_MODES:?} and PolicyMode::ALL disagree about how many tiers \
             exist"
        );

        // Unknown falls back to supervised — the safe default, unchanged.
        assert_eq!(PolicyMode::parse("bogus"), PolicyMode::Supervised);
    }

    #[tokio::test]
    async fn full_allows_but_always_approve_still_parks() {
        let p = policy("full", &["payment"], None);
        // `file_write`, not `write_file`. This asserted the undeclared
        // `write_file`, which the per-call judgement arm (issue #338) now stops
        // fail-closed — nobody has declared what it does. The point being made
        // here is that `full` allows a tool absent from `always_approve`, so it
        // wants a tool `full` genuinely allows: `file_write` is declared, and is
        // one of the low-consequence scratch writes #444 found safe enough to
        // grant standing.
        assert_eq!(
            p.check(&request("file_write", serde_json::json!({}))).await,
            ToolPolicyDecision::Allow
        );
        assert!(matches!(
            p.check(&request("payment.send", serde_json::json!({})))
                .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }

    /// The two approval paths must decide the same operator list the same way
    /// (issue #684).
    ///
    /// This is the assertion whose absence let the defect ship. Each path had
    /// its own matcher and its own tests, and each path's tests passed: the
    /// native gate matched dotted kinds exactly, this one matched tool names
    /// with a leading-segment rule, and nothing anywhere compared them. So
    /// `always_approve = ["payment"]` parked here and waved through there, and
    /// the shipped default — three dotted kinds, no tool names — was live on
    /// the gate and inert on the harness, which is the path a company using the
    /// openhuman toolbelt actually runs.
    ///
    /// It asserts agreement rather than a fixed verdict per path deliberately.
    /// Pinning "the harness parks `payment`" would go green again the moment
    /// the two implementations drifted apart in the other direction.
    #[tokio::test]
    async fn both_approval_paths_agree_on_the_same_always_approve_list() {
        use crate::policy::ManifestApprovalGate;
        use crate::ports::approvals::ApprovalGate;
        use crate::ports::types::PolicyDecision;

        // A leading segment, an exact dotted kind, a bare tool name, a
        // near-miss that must NOT be gated, and a case variant.
        //
        // Every name here is one the tier itself has no opinion about under
        // `full`, so the only thing that can make the two paths disagree is the
        // fence. A priced name like `web_search` would drag the harness's
        // metered-read and budget arms into a comparison that is not about
        // `always_approve`, so it is deliberately absent.
        let fence = &["payment", "filing.submit", "publish_artifact"];
        let names = [
            "payment.send",
            "payment",
            "filing.submit",
            "publish_artifact",
            "PUBLISH_ARTIFACT",
            "payroll.export",
        ];

        // `full` on both sides, so the tier decides nothing and any parking
        // observed is the override's doing.
        //
        // On the authored-workflow path so per-call judgement (#338) stays
        // silent: `ManifestApprovalGate` has no `judge` step, so comparing it
        // against the agent path — where an undeclared tool like
        // `payroll.export` stops on judge alone — would measure that Agent-only
        // rule, not the `always_approve` parity this test is about. Silencing
        // judge here isolates the one list both paths are supposed to share.
        let harness = policy("full", fence, None).for_authored_workflow_nodes();
        let gate = ManifestApprovalGate::new(Policy {
            mode: "full".to_string(),
            always_approve: fence.iter().map(|s| s.to_string()).collect(),
            auto_approve_under_usd: None,
        });

        let mut agreed = 0;
        for name in names {
            let harness_parks = matches!(
                harness.check(&request(name, serde_json::json!({}))).await,
                ToolPolicyDecision::RequireApproval { .. }
            );
            let effect = Effect {
                kind: name.to_string(),
                group: EffectGroup::Other,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::Value::Null,
                agent: None,
                run_id: None,
            };
            let gate_parks = matches!(
                gate.evaluate(&CompanyId::new("acme"), &effect)
                    .await
                    .unwrap(),
                PolicyDecision::RequireApproval
            );
            assert_eq!(
                harness_parks, gate_parks,
                "`{name}` parks on one approval path and not the other — \
                 one operator list, two answers (issue #684)"
            );
            agreed += 1;
        }
        assert_eq!(agreed, names.len(), "every name must have been compared");

        // Non-vacuity: the comparison above is only worth something if the
        // fence actually separates these names. Two paths that both allowed
        // everything would agree perfectly and prove nothing.
        assert!(matches!(
            harness
                .check(&request("payment.send", serde_json::json!({})))
                .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(
            harness
                .check(&request("payroll.export", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );
    }

    /// Issue #560's contract, stated as the operator reads it: the agent works
    /// without interrupting me, and stops before anything that leaves the
    /// building or spends money.
    ///
    /// Both halves are asserted in one test on purpose. A tier is a *line*, and
    /// a test that only checked the permissive half would pass just as happily
    /// against `full` — which is precisely the mistake `auto` exists to avoid.
    #[tokio::test]
    async fn auto_runs_sandbox_writes_and_outward_reads_but_parks_anything_that_leaves() {
        let p = policy("auto", &[], None);

        // Runs unattended: the agent's own scratch space, this company's own
        // memory, catalogue reads, and a read scoped to one connected account.
        for (tool, args) in [
            ("file_write", serde_json::json!({})),
            ("edit", serde_json::json!({})),
            ("apply_patch", serde_json::json!({})),
            ("csv_export", serde_json::json!({})),
            ("memory_store", serde_json::json!({})),
            ("file_read", serde_json::json!({})),
            ("mcp_list_tools", serde_json::json!({})),
            ("composio_list_tools", serde_json::json!({})),
            (
                "composio_execute",
                serde_json::json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" }),
            ),
        ] {
            assert_eq!(
                p.check(&request(tool, args)).await,
                ToolPolicyDecision::Allow,
                "{tool} should run unattended under auto — the tier is unusable if it interrupts \
                 the agent's own work"
            );
        }

        // Still parks: arbitrary code, arbitrary addresses, a configured remote,
        // operator-authored guidance, third-party effects, real money on submit,
        // and a workflow whose contents this layer cannot see.
        for (tool, args) in [
            ("shell", serde_json::json!({})),
            ("http_request", serde_json::json!({})),
            ("curl", serde_json::json!({})),
            ("web_fetch", serde_json::json!({})),
            ("git_operations", serde_json::json!({})),
            ("workspace_write", serde_json::json!({})),
            ("workspace_create", serde_json::json!({})),
            ("workspace_delete", serde_json::json!({})),
            ("workspace_rename", serde_json::json!({})),
            ("publish_artifact", serde_json::json!({})),
            ("media_generate_image", serde_json::json!({})),
            ("media_generate_video", serde_json::json!({})),
            ("mcp_call_tool", serde_json::json!({})),
            ("mcp_registry_tool_call", serde_json::json!({})),
            ("run_workflow", serde_json::json!({})),
            ("composio_authorize", serde_json::json!({})),
            // Issue #245: a checkout writes a tree of third-party source into a
            // sandbox this agent may also hold `shell` over, and both tools
            // reach the forge under the company's credential.
            ("repo_checkout", serde_json::json!({})),
            ("repo_pr", serde_json::json!({})),
            (
                "composio_execute",
                serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" }),
            ),
            // An action the provider catalogue does not name is a send, so the
            // cautious verdict survives into the new tier rather than being
            // re-decided by it.
            (
                "composio_execute",
                serde_json::json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" }),
            ),
            // A tool nobody has declared must not run unattended by omission.
            ("some_tool_nobody_declared", serde_json::json!({})),
        ] {
            assert!(
                matches!(
                    p.check(&request(tool, args)).await,
                    ToolPolicyDecision::RequireApproval { .. }
                ),
                "{tool} leaves the company or spends money and must still park under auto"
            );
        }
    }

    /// `always_approve` wins over `auto` exactly as it wins over `full`, and the
    /// two tiers below `auto` are untouched by its arrival.
    ///
    /// The `readonly`/`supervised` half is not ceremony: `auto` was added by
    /// widening a `match` on the mode and by adding a predicate next to the two
    /// the other arms read, so the way this change fails is by moving a
    /// neighbouring line, not by getting its own arm wrong.
    #[tokio::test]
    async fn auto_yields_to_always_approve_and_leaves_the_lower_tiers_alone() {
        let auto = policy("auto", &["file_write"], None);
        assert!(
            matches!(
                auto.check(&request("file_write", serde_json::json!({})))
                    .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "a tool on the always-approve list must park even though auto would otherwise run it"
        );

        // `readonly` still denies a sandbox write outright rather than parking
        // it, and still allows a pure read.
        let readonly = policy("readonly", &[], None);
        assert!(matches!(
            readonly
                .check(&request("file_write", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Deny { .. }
        ));
        assert_eq!(
            readonly
                .check(&request("file_read", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );

        // `supervised` still parks the sandbox write `auto` now runs — the one
        // difference between the tiers, asserted as a difference.
        let supervised = policy("supervised", &[], None);
        assert!(matches!(
            supervised
                .check(&request("file_write", serde_json::json!({})))
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

    /// What string a per-call **tool** gate actually puts in front of an
    /// operator (issue #701).
    ///
    /// The issue could not answer this from the frontend, and declined to
    /// invent labels without it — rightly: a card whose job is informed consent
    /// is worse off with a label naming the wrong action than with a vague one.
    /// The answer is that a tool gate parks under the tool's own raw name.
    /// [`ApprovalPolicy::require_approval`] is the only construction site for a
    /// `RequireApproval` decision, it builds its request through
    /// [`ApprovalPolicy::effect_for`], and that sets `kind` to `tool_name`
    /// verbatim; `CompanyRuntime::pending_approvals` — the only projection
    /// point for an `ApprovalSummary` — copies it through unchanged.
    ///
    /// Two of the seven invite the opposite guess, so both are pinned here
    /// rather than argued in prose:
    ///
    /// * `publish_artifact` does **not** park as `external.publish`. That kind
    ///   exists only as a native workflow-gate class and a `DEFAULT_ALWAYS_APPROVE`
    ///   entry; `harness::publish` builds no effect and touches no gate.
    /// * `run_workflow` does **not** park as `workflow.approve`. That kind is a
    ///   workflow *resuming* mid-run (issue #395, `WORKFLOW_APPROVE_KIND`),
    ///   which is a different event from an agent asking to start one.
    ///
    /// So all seven need entries in the console's tool-label table, and this
    /// test is what stops that answer decaying back into a guess.
    #[test]
    fn parked_kind_is_the_tool_name() {
        let p = policy("supervised", &[], None);
        for tool in [
            "curl",
            "git_operations",
            "http_request",
            "mcp_call_tool",
            "publish_artifact",
            "read_workspace_state",
            "run_workflow",
        ] {
            assert_eq!(
                p.effect_for(tool, &serde_json::json!({})).kind,
                tool,
                "`{tool}` parks under a kind the console's tool-label table does \
                 not key on; the label added for it in `language.ts` is now \
                 unreachable"
            );
        }
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

    /// The company workspace (issues #237, #551, #671): the two read tools
    /// reach only this company's own note tree and must be allowed in every
    /// mode, while the four mutations — `workspace_write` (overwrites shared
    /// guidance), `workspace_create` (adds to the tree everyone reads),
    /// `workspace_delete` and `workspace_rename` (remove and move what the
    /// agent put in its own folder) — must park under supervised / be denied
    /// under readonly.
    ///
    /// This is the ACTUAL gate on a workspace write. Issue #237 proposed that
    /// declaring `PermissionLevel::Write` would keep the `ApprovalPolicy` as
    /// the per-call gate; it would not — openhuman's `ToolPolicy` surface hands
    /// this bridge only the tool name and args, never the tool's permission
    /// level, so classification is by name alone. Pinning every one of the six
    /// names here is what stops a later rename (say `get_workspace_note`, which the
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
        for tool in [
            "workspace_write",
            "workspace_create",
            "workspace_delete",
            "workspace_rename",
        ] {
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
        for tool in ["workspace_list", "workspace_read"] {
            assert_eq!(
                readonly.check(&request(tool, serde_json::json!({}))).await,
                ToolPolicyDecision::Allow,
                "{tool} must stay available to a read-only desk"
            );
        }
        for tool in [
            "workspace_write",
            "workspace_create",
            "workspace_delete",
            "workspace_rename",
        ] {
            assert!(
                matches!(
                    readonly.check(&request(tool, serde_json::json!({}))).await,
                    ToolPolicyDecision::Deny { .. }
                ),
                "{tool} must be denied under readonly"
            );
        }

        // Under `full` these still run. There IS a per-call gate now (issue
        // #338), but writing the company's own note tree is not one of the acts
        // it stops: it is internal, and the gate is scoped to what leaves the
        // company or cannot be bounded. These tools keep their own safeguards:
        // writes and deletes require an `expected_updated_at` compare-and-swap
        // token, creates refuse paths that already resolve, deletes refuse
        // folders that still hold anything, and renames refuse occupied
        // destinations.
        //
        // This is the assertion that caught the first version of that gate,
        // which stopped both: publishing runs through them, and thirteen
        // `publish_turn_test` cases failed with "the model was never handed a
        // publish receipt".
        let full = policy("full", &[], None);
        for tool in [
            "workspace_write",
            "workspace_create",
            "workspace_delete",
            "workspace_rename",
        ] {
            assert_eq!(
                full.check(&request(tool, serde_json::json!({}))).await,
                ToolPolicyDecision::Allow,
                "{tool} under full mode"
            );
        }
    }

    /// The repository pair across all four tiers (issue #245), asserted as a
    /// line rather than as four independent facts.
    ///
    /// `readonly` **denies** rather than parks, and that is the one verdict here
    /// worth arguing: both names read like reads. `repo_checkout` writes
    /// thousands of files into the agent's sandbox, which a tier whose whole
    /// contract is "nothing changes" cannot admit; `repo_pr` reaches a third
    /// party under the company's credential, which is the other half of the same
    /// contract. Parking either under `readonly` would be worse than denying,
    /// because openhuman resolves a `RequireApproval` inline and never
    /// re-dispatches — the operator would approve a call that then does not run.
    ///
    /// The parked request's `kind` is checked too, because the console's plain
    /// language table is keyed on exactly that string: a `kind` that is not the
    /// tool name silently falls through to "Use one of its tools".
    #[tokio::test]
    async fn the_repository_pair_parks_under_supervision_and_is_denied_read_only() {
        let args = serde_json::json!({ "repo": "acme/widgets" });
        for mode in ["supervised", "auto"] {
            let p = policy(mode, &[], None);
            for tool in ["repo_checkout", "repo_pr"] {
                let decision = p.check(&request(tool, args.clone())).await;
                let ToolPolicyDecision::RequireApproval { .. } = decision else {
                    panic!("{tool} must park under {mode}, got {decision:?}");
                };
                assert_eq!(
                    p.effect_for(tool, &args).kind,
                    tool,
                    "the approval card's kind must be the tool name, or the console \
                     cannot label it"
                );
            }
        }

        let readonly = policy("readonly", &[], None);
        for tool in ["repo_checkout", "repo_pr"] {
            assert!(
                matches!(
                    readonly.check(&request(tool, args.clone())).await,
                    ToolPolicyDecision::Deny { .. }
                ),
                "{tool} must be denied under readonly, not parked"
            );
        }

        let full = policy("full", &[], None);
        for tool in ["repo_checkout", "repo_pr"] {
            assert_eq!(
                full.check(&request(tool, args.clone())).await,
                ToolPolicyDecision::Allow,
                "{tool} under full mode"
            );
        }
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
        let args = composio_send_args();
        assert!(matches!(
            p.check(&request("composio_execute", args.clone())).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));

        let queued = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN).requests;
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

    /// Issue #470, at the layer that projects a blocked call onto the effect
    /// the operator's card is built from.
    ///
    /// The fixtures above used to name their action under a key nothing reads,
    /// so every Composio test in this module classified through the
    /// unknown-is-a-send fallback and none of them ever reached the catalogue.
    /// Read and send came out identical, and a regression in the split would
    /// have failed nothing here. This asserts they come out different, and
    /// asserts the classification rather than the parking decision — so it
    /// stays honest across issue #559, which changes whether a read parks but
    /// not what it is.
    #[tokio::test]
    async fn a_composio_read_and_a_composio_send_are_classified_differently() {
        let read = composio_read_args();
        let send = composio_send_args();

        assert_eq!(
            classify_group("composio_execute", &read),
            EffectGroup::Other,
            "`{COMPOSIO_READ_SLUG}` is tagged `Read` in the vendored catalogue; \
             if this fails the lookup is not being reached"
        );
        assert!(
            grantable("composio_execute", &read),
            "a read scoped to one connected account is what a standing grant \
             can honestly describe"
        );

        assert_eq!(
            classify_group("composio_execute", &send),
            EffectGroup::Send,
            "`{COMPOSIO_SEND_SLUG}` is tagged `Write`"
        );
        assert!(!grantable("composio_execute", &send));

        // The cautious fallback still has its own coverage, and still says
        // send — but now because the catalogue was asked and had no answer,
        // not because the classifier never saw an action at all.
        let unknown = composio_unclassified_args();
        assert_eq!(
            classify_group("composio_execute", &unknown),
            EffectGroup::Send
        );
        assert!(!grantable("composio_execute", &unknown));

        // And the split survives the round trip through the park queue: the
        // group asserted above is the one the operator's card is built from.
        let (p, queue) = queued_policy("supervised", &[]);
        let _ = p.check(&request("composio_execute", send.clone())).await;
        let queued = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN).requests;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].effect.group, EffectGroup::Send);
        assert_eq!(
            queued[0].effect.payload, send,
            "the card shows the arguments the agent actually sent, action key \
             included"
        );
    }

    /// Issue #559, at the gate an agent actually hits: a Composio read runs
    /// under `supervised` instead of parking, and nothing is queued for a
    /// human — while a send on the same tool still parks.
    ///
    /// This is the behaviour the issue is about. `a_composio_read_and_a_
    /// composio_send_are_classified_differently` above pins what the two calls
    /// *are*; this pins what the desk *does* with them, which is the part an
    /// operator notices when every page of a mailbox raises a card.
    #[tokio::test]
    async fn a_composio_read_runs_under_supervision_without_parking() {
        let (p, queue) = queued_policy("supervised", &[]);

        assert_eq!(
            p.check(&request("composio_execute", composio_read_args()))
                .await,
            ToolPolicyDecision::Allow,
            "reading a connected account changes nothing and costs nothing"
        );
        assert_eq!(
            queue.queued(),
            0,
            "no card was raised, so no human was interrupted"
        );

        // Paging the same list is not a second decision, because there was
        // never a first one. This is the symptom the issue opens with.
        for _ in 0..5 {
            let _ = p
                .check(&request("composio_execute", composio_read_args()))
                .await;
        }
        assert_eq!(queue.queued(), 0);

        // The send half is untouched: same tool, same desk, still parks.
        assert!(matches!(
            p.check(&request("composio_execute", composio_send_args()))
                .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(queue.queued(), 1);
    }

    /// A `readonly` desk still denies the read. That tier's contract is that
    /// nothing outside the company is reached at all — #559 moves the read out
    /// of the parking bucket, not out of the reaching-outward one.
    #[tokio::test]
    async fn a_readonly_desk_still_denies_a_composio_read() {
        let p = policy("readonly", &[], None);
        assert!(matches!(
            p.check(&request("composio_execute", composio_read_args()))
                .await,
            ToolPolicyDecision::Deny { .. }
        ));
        assert!(matches!(
            p.check(&request("composio_execute", composio_send_args()))
                .await,
            ToolPolicyDecision::Deny { .. }
        ));
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
        let queued = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN).requests;
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
        let args = composio_send_args();
        for _ in 0..3 {
            let _ = p.check(&request("composio_execute", args.clone())).await;
        }
        assert_eq!(queue.queued(), 1, "the same call parks once");

        // A different call to the same tool is a distinct request. Another
        // catalogued send, so the second call is classified rather than merely
        // unrecognised.
        let _ = p
            .check(&request(
                "composio_execute",
                composio_args(COMPOSIO_OTHER_SEND_SLUG),
            ))
            .await;
        assert_eq!(queue.queued(), 2);
    }

    /// The drain is capped, so a runaway turn can't flood the operator's queue.
    ///
    /// The slugs here are deliberately uncatalogued (issue #470): this test
    /// wants many calls the queue treats as distinct and is indifferent to what
    /// any of them classify as, so naming real actions would only invite a
    /// reader to think the classification mattered. They do still land under
    /// the real action key, so each one reaches the catalogue lookup and misses
    /// it — the honest fallback, rather than a call carrying no action at all.
    #[tokio::test]
    async fn the_drain_is_capped_and_empties_the_queue() {
        let (p, queue) = queued_policy("supervised", &[]);
        for i in 0..(MAX_APPROVAL_REQUESTS_PER_TURN + 4) {
            let _ = p
                .check(&request(
                    "composio_execute",
                    composio_unclassified_args_numbered(i),
                ))
                .await;
        }
        let drained = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(drained.requests.len(), MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(queue.queued(), 0, "the overflow is discarded, not carried");

        // Issue #561: and the drain says how many it threw away, rather than
        // handing back a `Vec` indistinguishable from a complete one.
        assert_eq!(
            drained.discarded, 4,
            "12 gated calls, a cap of 8, so 4 were dropped"
        );
        let notice = drained
            .overflow_notice()
            .expect("an overflowing drain has something to tell the operator");
        assert!(
            notice.contains('4'),
            "the count is in the sentence: {notice}"
        );
        assert!(
            notice.contains("not** run") || notice.contains("not run"),
            "the operator must not read this as 'the calls happened, the records \
             were lost': {notice}"
        );
    }

    /// The ordinary path says nothing. A notice on every turn would train the
    /// operator to ignore the one that matters.
    #[tokio::test]
    async fn a_drain_under_the_cap_reports_no_overflow() {
        let (p, queue) = queued_policy("supervised", &[]);
        for i in 0..(MAX_APPROVAL_REQUESTS_PER_TURN - 1) {
            let _ = p
                .check(&request(
                    "composio_execute",
                    composio_unclassified_args_numbered(i),
                ))
                .await;
        }
        let drained = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(drained.requests.len(), MAX_APPROVAL_REQUESTS_PER_TURN - 1);
        assert_eq!(drained.discarded, 0);
        assert!(drained.overflow_notice().is_none());
    }

    /// Exactly at the cap is not an overflow. An off-by-one here would cry wolf
    /// on the commonest boundary case.
    #[tokio::test]
    async fn a_drain_exactly_at_the_cap_reports_no_overflow() {
        let (p, queue) = queued_policy("supervised", &[]);
        for i in 0..MAX_APPROVAL_REQUESTS_PER_TURN {
            let _ = p
                .check(&request(
                    "composio_execute",
                    composio_unclassified_args_numbered(i),
                ))
                .await;
        }
        let drained = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(drained.requests.len(), MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(drained.discarded, 0);
        assert!(drained.overflow_notice().is_none());
    }

    /// One dropped request reads as one, not as "1 calls".
    ///
    /// The nouns agreed from the start; the verbs and pronouns did not, so a
    /// single discard read "1 further gated tool call **were** not raised …
    /// **They were** not run and **they are** not on the Approvals page". The
    /// whole sentence has to agree, not the countable nouns in it — an operator
    /// reading a confidently-worded, ungrammatical notice has cause to wonder
    /// what else about it is stale.
    #[test]
    fn the_overflow_notice_is_singular_for_a_single_dropped_request() {
        let drained = DrainedRequests::new(Vec::new(), 1, 8);
        let notice = drained.overflow_notice().expect("one is still an overflow");
        assert!(notice.contains("1 further gated tool call "), "{notice}");
        assert!(!notice.contains("calls"), "{notice}");
        assert!(notice.contains("call was not raised"), "{notice}");
        assert!(notice.contains("It was **not** run"), "{notice}");
        assert!(notice.contains("it is **not** on the"), "{notice}");
        assert!(!notice.contains("were"), "{notice}");
        assert!(!notice.contains("they"), "{notice}");
        assert!(!notice.contains("They"), "{notice}");
    }

    /// …and the plural is untouched: the agreement fix must not singularise the
    /// case that was already right.
    #[test]
    fn the_overflow_notice_stays_plural_for_several_dropped_requests() {
        let drained = DrainedRequests::new(Vec::new(), 3, 8);
        let notice = drained.overflow_notice().expect("three is an overflow");
        assert!(
            notice.contains("3 further gated tool calls were not"),
            "{notice}"
        );
        assert!(notice.contains("They were **not** run"), "{notice}");
        assert!(notice.contains("they are **not** on the"), "{notice}");
        assert!(!notice.contains(" was "), "{notice}");
    }

    /// The notice names the cap the drain was actually taken against, not one a
    /// caller supplied later.
    ///
    /// `discarded` was always captured at drain time while `cap` arrived at the
    /// sentence, so `drain(8)` followed by `overflow_notice(20)` produced a
    /// confidently-worded, wrong number for the operator — the same class of
    /// defect as the invisible discard #561 fixes. Storing it makes that
    /// unrepresentable, and this pins that the stored value is the one used.
    #[tokio::test]
    async fn the_notice_quotes_the_cap_the_drain_was_taken_against() {
        let (p, queue) = queued_policy("supervised", &[]);
        for i in 0..5 {
            let _ = p
                .check(&request(
                    "composio_execute",
                    composio_unclassified_args_numbered(i),
                ))
                .await;
        }
        let drained = queue.drain(3);
        assert_eq!(drained.cap(), 3);
        assert_eq!(drained.discarded, 2);
        let notice = drained.overflow_notice().expect("2 were dropped");
        assert!(
            notice.contains("at most 3"),
            "the sentence must quote the cap that did the discarding: {notice}"
        );
        assert!(
            !notice.contains(&MAX_APPROVAL_REQUESTS_PER_TURN.to_string()),
            "and not the constant the call site happened to have in scope: {notice}"
        );
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
            origin_parent: None,
        }
    }

    /// The point of the whole feature: a call the operator approved actually
    /// runs, instead of parking a second time.
    #[tokio::test]
    async fn a_granted_call_is_allowed_once_and_then_parks_again() {
        let (p, grants) = granting_policy("supervised", &[], "finance");
        let args = composio_send_args();
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
        let args = composio_send_args();
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
        let args = composio_send_args();
        grants.grant(crate::runtime::grants::GrantedCall {
            approval_id: crate::ports::types::ApprovalId::new("appr-1"),
            agent: "finance".into(),
            tool: "composio_execute".into(),
            args: args.clone(),
            at_millis: 1_000,
            origin_thread: None,
            origin_parent: None,
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

        let drained = queue.drain(10).requests;
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

    // --- scopes: a turn's entries are its own, and nobody else's (#439) ------

    fn gated(kind: &str) -> ApprovalRequest {
        ApprovalRequest {
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
        }
    }

    /// The regression #395 narrowed and #439 removes: a workflow node parks its
    /// gated calls while a chat cycle is part-way through its own turn.
    ///
    /// #395 did this with a boundary index, which worked only because the queue
    /// was append-only — it encoded a *guess* about who wrote what. The scope
    /// encodes the fact, so the node cannot see the cycle's entry at all rather
    /// than merely declining to take it.
    #[tokio::test]
    async fn a_run_drains_its_own_entries_and_cannot_see_another_turns() {
        let queue = ApprovalRequestQueue::default();

        // A chat cycle's own turn parked this one and has not drained yet.
        let cycle = queue.claim(ApprovalScope::Cycle);
        cycle
            .scoped(async { queue.push(gated("chat.thing")) })
            .await;

        // The workflow node's turn parks two, concurrently.
        let run = queue.claim(ApprovalScope::Run("run-1".into()));
        let taken = run
            .scoped(async {
                queue.push(gated("node.thing"));
                queue.push(gated("node.other"));
                assert_eq!(queue.queued(), 2, "the run sees only its own");
                queue.drain(10)
            })
            .await;
        assert_eq!(
            taken
                .requests
                .iter()
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["node.thing", "node.other"],
            "only the node's own entries come back"
        );

        // The cycle's entry is untouched and still drains as its own.
        let drained = cycle.scoped(async { queue.drain(10) }).await;
        assert_eq!(
            drained
                .requests
                .iter()
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["chat.thing"],
            "the chat cycle's entry survived the run's drain, unmoved"
        );
    }

    /// The race a boundary index could never fix: **two concurrent workflow
    /// runs**. Both took a boundary against one shared vector, so the later
    /// `split_off` swallowed the earlier run's tail. Scopes make them disjoint.
    #[tokio::test]
    async fn two_concurrent_runs_cannot_take_each_others_entries() {
        let queue = ApprovalRequestQueue::default();
        let one = queue.claim(ApprovalScope::Run("run-1".into()));
        let two = queue.claim(ApprovalScope::Run("run-2".into()));

        // Interleaved exactly as two spawned runs would be.
        one.scoped(async { queue.push(gated("one.a")) }).await;
        two.scoped(async { queue.push(gated("two.a")) }).await;
        one.scoped(async { queue.push(gated("one.b")) }).await;

        let two_got = two.scoped(async { queue.drain(10) }).await;
        assert_eq!(
            two_got
                .requests
                .iter()
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["two.a"],
            "run-2 must not swallow run-1's tail",
        );
        let one_got = one.scoped(async { queue.drain(10) }).await;
        assert_eq!(
            one_got
                .requests
                .iter()
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["one.a", "one.b"],
            "and run-1 keeps both of its own, in order",
        );
    }

    /// The non-lossy fallback. A push outside any claim is not dropped and not
    /// an error — it lands in `Unscoped`, which the **chat cycle** drains and a
    /// workflow run does not.
    ///
    /// This is the direction that matters: a missed turn entry point degrades
    /// to today's behaviour (the operator is still asked) rather than to a
    /// silently discarded approval, which is the failure #395 existed to fix.
    #[tokio::test]
    async fn an_unclaimed_push_is_drained_by_the_cycle_never_by_a_run() {
        let queue = ApprovalRequestQueue::default();
        queue.push(gated("orphan"));

        let run = queue.claim(ApprovalScope::Run("run-1".into()));
        assert!(
            run.scoped(async { queue.drain(10) })
                .await
                .requests
                .is_empty(),
            "a run must not adopt an entry it did not raise",
        );

        let cycle = queue.claim(ApprovalScope::Cycle);
        let drained = cycle.scoped(async { queue.drain(10) }).await;
        assert_eq!(
            drained
                .requests
                .iter()
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>(),
            vec!["orphan"],
            "the cycle owns whatever nobody claimed — the pre-#439 behaviour",
        );
    }

    /// The claim's exit half. A turn that returns early — an error, a steer, an
    /// `?` — must not leave its entries for whoever claims that scope next.
    /// `clear()` at the top of a cycle never gave this; `Drop` does.
    #[tokio::test]
    async fn dropping_a_claim_discards_that_scopes_entries() {
        let queue = ApprovalRequestQueue::default();
        {
            let cycle = queue.claim(ApprovalScope::Cycle);
            cycle.scoped(async { queue.push(gated("abandoned")) }).await;
            assert_eq!(queue.len_in(&ApprovalScope::Cycle), 1, "parked mid-turn");
        }
        // Observed WITHOUT claiming. Asserting through a fresh claim would pass
        // even with `Drop` removed, because `claim` clears on entry too — this
        // assertion was vacuous until it read the bucket directly.
        assert_eq!(
            queue.len_in(&ApprovalScope::Cycle),
            0,
            "the abandoned entries must go when the claim does, not when the \
             next claim happens to clear them",
        );
    }

    /// De-duplication is per scope. Two turns asking for the same tool are two
    /// asks; collapsing them would hide one turn's request behind another's.
    #[tokio::test]
    async fn duplicate_suppression_is_per_scope_not_global() {
        let queue = ApprovalRequestQueue::default();
        let cycle = queue.claim(ApprovalScope::Cycle);
        let run = queue.claim(ApprovalScope::Run("run-1".into()));

        cycle
            .scoped(async {
                queue.push(gated("same"));
                queue.push(gated("same"));
                assert_eq!(queue.queued(), 1, "a retry within one turn is one ask");
            })
            .await;
        run.scoped(async {
            queue.push(gated("same"));
            assert_eq!(queue.queued(), 1, "the other turn's ask is its own");
        })
        .await;
    }

    /// Issue #439's half of the grant-lifetime guarantee, alongside
    /// `grants_survive_a_queue_clear`.
    ///
    /// A grant is minted by one turn's decision and redeemed by a different,
    /// later one, so it belongs to the company and never to a scope. If it had
    /// been folded into the per-scope map, dropping the claim would take it —
    /// and approvals would fail in exactly their own happy path.
    #[tokio::test]
    async fn grants_outlive_a_scope() {
        let grants = GrantSet::default();
        let queue = ApprovalRequestQueue::with_grants(grants.clone());
        let args = serde_json::json!({ "to": "a@b.test" });
        grants.grant(GrantedCall {
            approval_id: crate::ports::types::ApprovalId::new("a1"),
            agent: "finance".to_string(),
            tool: "composio_execute".to_string(),
            args: args.clone(),
            at_millis: 1_000,
            origin_thread: None,
            origin_parent: None,
        });

        {
            let cycle = queue.claim(ApprovalScope::Cycle);
            cycle.scoped(async { queue.push(gated("whatever")) }).await;
        }

        assert!(
            queue
                .grants()
                .consume("finance", "composio_execute", &args)
                .is_some(),
            "the scope went; the grant did not",
        );
    }

    /// The trap the derived `Default` used to hide: a queue built with
    /// `default()` has its **own** grant set, so a grant minted elsewhere can
    /// never be redeemed through it and every approval re-parks forever.
    ///
    /// Production uses `with_grants` and is safe; this pins the difference so
    /// the hazard is a stated property rather than a footgun — and so a future
    /// per-scope refactor cannot reach for `default()` and silently scope
    /// grants along with the requests.
    #[test]
    fn grants_are_not_shared_by_default() {
        let shared = GrantSet::default();
        let args = serde_json::json!({ "to": "a@b.test" });
        let call = GrantedCall {
            approval_id: crate::ports::types::ApprovalId::new("a1"),
            agent: "finance".to_string(),
            tool: "composio_execute".to_string(),
            args: args.clone(),
            at_millis: 1_000,
            origin_thread: None,
            origin_parent: None,
        };
        shared.grant(call.clone());

        assert!(
            ApprovalRequestQueue::default()
                .grants()
                .consume("finance", "composio_execute", &args)
                .is_none(),
            "a default queue cannot see a grant minted anywhere else",
        );
        assert!(
            ApprovalRequestQueue::with_grants(shared)
                .grants()
                .consume("finance", "composio_execute", &args)
                .is_some(),
            "…and `with_grants` is the constructor that can — the one production uses",
        );
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

        // `web_search`, not `pay_invoice`. Both are priced calls — `web_search`
        // is declared `EffectGroup::Spend`, which is what `is_priced_call`
        // reads — so this still exercises the cap arm. `pay_invoice` would now
        // also be stopped by the per-call judgement arm (issue #338), and a
        // test about the *meter* should not be able to fail for a reason that
        // has nothing to do with the meter.
        assert_eq!(
            uncapped
                .check(&request("web_search", serde_json::json!({})))
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

        // `web_search` for the same reason as `an_uncapped_agent_never_queries_
        // the_meter`: a priced call the per-call judgement arm is silent on, so
        // this keeps testing the cap and only the cap.
        assert_eq!(
            no_meter
                .check(&request("web_search", serde_json::json!({})))
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
            origin_parent: None,
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
            "workspace_create",
            "workspace_delete",
            "workspace_rename",
            // Anything a remote server chooses to advertise.
            "mcp_registry_tool_call",
            "mcp_call_tool",
            // Third-party source and diffs, fetched under the operator's
            // credential (issue #245).
            "repo_checkout",
            "repo_pr",
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
    ///
    /// `read_workspace_state` was in this list until issue #459 — see
    /// [`reading_workspace_state_parks_supervised_and_denies_readonly`].
    #[tokio::test]
    async fn a_workspace_read_runs_without_asking_whatever_its_name_begins_with() {
        let p = policy("supervised", &[], None);
        for tool in [
            "file_read",
            "glob",
            "grep",
            "image_info",
            "list",
            "memory_recall",
        ] {
            assert_eq!(
                p.check(&request(tool, serde_json::json!({}))).await,
                ToolPolicyDecision::Allow,
                "`{tool}` reads the agent's own workspace"
            );
        }
    }

    /// Issue #459: the sibling that turned out not to be a read at all. It
    /// shells out to `git status` in the agent's own workspace, and the
    /// vendored `run_git` lets that directory's `.git/config` — which
    /// `file_write` can author — decide what git executes. So it goes through
    /// the gate the way `shell` does, and this is the assertion an operator
    /// actually feels.
    #[tokio::test]
    async fn reading_workspace_state_parks_supervised_and_denies_readonly() {
        assert!(
            matches!(
                policy("supervised", &[], None)
                    .check(&request("read_workspace_state", serde_json::json!({})))
                    .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "running git under agent-authored config must reach an operator"
        );
        assert!(
            matches!(
                policy("readonly", &[], None)
                    .check(&request("read_workspace_state", serde_json::json!({})))
                    .await,
                ToolPolicyDecision::Deny { .. }
            ),
            "`readonly` promises nothing runs; a git config key can name a command"
        );
    }

    /// …and the `readonly` denial says **why**, because this is the one an
    /// operator ends up confused by.
    ///
    /// `read_workspace_state` used to run on a `readonly` desk, so a company
    /// that sat there gets a refusal on what was a normal first move — and a
    /// tier that promises reads still work refusing something called `read_*`
    /// reads as a bug in the tier unless the message names git. The
    /// `supervised` park explains itself by producing a card to approve; this
    /// one has to carry its reason.
    #[tokio::test]
    async fn the_readonly_denial_of_a_read_shaped_tool_says_why() {
        let ToolPolicyDecision::Deny { reason } = policy("readonly", &[], None)
            .check(&request("read_workspace_state", serde_json::json!({})))
            .await
        else {
            panic!("`readonly` denies it");
        };
        assert!(
            reason.contains("git"),
            "an operator must be able to tell this from a mis-classification: {reason}"
        );
        assert!(
            reason.contains("workspace"),
            "and where the config it obeys comes from: {reason}"
        );

        // A tool whose name already argues for the verdict carries no such
        // clause — every denial ending in an explanation is an explanation
        // nobody reads.
        let ToolPolicyDecision::Deny { reason } = policy("readonly", &[], None)
            .check(&request("shell", serde_json::json!({})))
            .await
        else {
            panic!("`readonly` denies shell");
        };
        assert!(!reason.contains("git"), "{reason}");
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

    /// Issue #457's scope check, pinned **directly** rather than through
    /// `check()`.
    ///
    /// It used to run through the real admission path, asserting that a grant
    /// scoped to `github` admitted a GitHub read and re-parked a Gmail one.
    /// Issue #559 made that unobservable from `check()`, and the honest thing
    /// is to say so rather than relax the assertion until it passes:
    ///
    /// * under `supervised` a catalogue read no longer parks at all, so it is
    ///   allowed by the tier long before the scope is consulted;
    /// * under `readonly` the brake at step 1 denies every external effect
    ///   *above* the grant checks, so the scope is not consulted there either;
    /// * under `full` everything is allowed.
    ///
    /// So `standing_grant_allows` is still correct and still worth pinning —
    /// this test does that — but no tier currently routes a Composio read to
    /// it. See the note in the PR for #559; the issue's claim that
    /// `Standing::Grantable` "still governs `readonly`" does not hold against
    /// the ordering in `check()`.
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
        // this is inside the sentence. Scoping by action slug instead would
        // have refused here and made the grant worthless.
        assert!(
            p.standing_grant_allows(
                "composio_execute",
                &composio_args("GITHUB_LIST_PULL_REQUESTS")
            ),
            "the operator consented to a provider, not to one action slug"
        );

        // A mailbox read. Also a catalogue read, also grantable, also `ops`,
        // also `composio_execute` — every check upstream of the scope says yes,
        // and the scope is the one thing that says no.
        assert!(
            !p.standing_grant_allows("composio_execute", &composio_args("GMAIL_FETCH_EMAILS")),
            "'read from GitHub' is not consent to read the company's mail"
        );

        // An action the catalogue cannot place carries no scope, so a scoped
        // grant refuses it — unknown is a send, here too.
        assert!(
            !p.standing_grant_allows("composio_execute", &composio_unclassified_args()),
            "an unplaceable action has no scope for a scoped grant to admit"
        );

        // Through the real gate, the unknown action still parks: it is a send,
        // so the tier does not wave it through and the scoped grant will not
        // admit it either. This half of the original test survives #559
        // unchanged, because only the *read* branch moved.
        assert!(matches!(
            p.check(&request("composio_execute", composio_unclassified_args()))
                .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));

        // Deliberately NOT asserting `check(read) == Allow` here. It would pass
        // whether or not #559 landed — this policy holds a standing grant that
        // admits a GitHub read at step 2b, so the tier never gets a say, and
        // the assertion would prove nothing while looking like it proved the
        // change. `a_composio_read_runs_under_supervision_without_parking` is
        // the test for that, and it uses a policy with no grant at all.

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

    /// The four workspace mutations keep their `Other` label on the card —
    /// there is no consequence word to name — while being refused a standing
    /// scope.
    /// That separation is the point of issue #444: the label and the permission
    /// are different questions.
    #[test]
    fn workspace_mutations_are_labelled_other_and_are_still_not_grantable() {
        let args = serde_json::json!({});
        for tool in [
            "workspace_write",
            "workspace_create",
            "workspace_delete",
            "workspace_rename",
        ] {
            assert_eq!(classify_group(tool, &args), EffectGroup::Other, "{tool}");
            assert!(classify_group(tool, &args).is_unclassified(), "{tool}");
            assert!(!grantable(tool, &args), "{tool}");
            assert!(is_external_effect(tool, &args), "{tool}");
        }
    }

    // ---- Per-call judgement (issue #338) ----------------------------------
    //
    // The unit tests for the verdict itself live in `crate::policy::judgement`,
    // which is pure and compiles in the default build. What is tested HERE is
    // the only thing that needs the harness: **where the arm sits in the
    // chain**. Every test below is an assertion that the arm did not move
    // something above it.

    fn decision_name(d: &ToolPolicyDecision) -> &'static str {
        match d {
            ToolPolicyDecision::Allow => "allow",
            ToolPolicyDecision::RequireApproval { .. } => "park",
            ToolPolicyDecision::Deny { .. } => "deny",
        }
    }

    /// The agent-path behaviour change after #658's ruling: sends, payments and
    /// undeclared publish-shaped calls stop regardless of mode. The declared
    /// `publish_artifact` exception has its own tests in `judgement.rs`.
    #[tokio::test]
    async fn full_autonomy_stops_for_an_irreversible_call() {
        let p = policy("full", &[], None);
        for tool in ["send_email", "publish_post", "pay_invoice"] {
            let d = p.check(&request(tool, serde_json::json!({}))).await;
            assert_eq!(decision_name(&d), "park", "`{tool}` must stop under full");
        }
    }

    /// The other half: `full` still means full for everything that does not
    /// warrant a human. A gate that stopped reads would simply be `supervised`
    /// with extra steps.
    #[tokio::test]
    async fn full_autonomy_still_allows_reads_and_drafts() {
        let p = policy("full", &[], None);
        for tool in ["file_read", "grep", "list", "memory_recall", "web_search"] {
            let d = p.check(&request(tool, serde_json::json!({}))).await;
            assert_eq!(decision_name(&d), "allow", "`{tool}` must still run");
        }
    }

    /// `readonly` DENIES an irreversible call; it does not park it.
    ///
    /// The failure this guards is subtle and would look like an improvement: if
    /// the judgement arm ran before the mode, a send on a read-only desk would
    /// come back as "ask the operator" instead of "no". That converts the
    /// emergency stop into a prompt, which is the one thing the brake exists to
    /// not be.
    #[tokio::test]
    async fn the_readonly_brake_still_denies_rather_than_parking() {
        let p = policy("readonly", &[], None);
        let d = p.check(&request("send_email", serde_json::json!({}))).await;
        assert_eq!(decision_name(&d), "deny");
    }

    /// `supervised` is untouched: the call already parked, and it still parks
    /// with the reason `supervised` gives rather than the judgement one.
    ///
    /// Reason text is asserted because it is the operator-visible half — a tier
    /// silently re-labelling its stops would be a change nobody asked for.
    #[tokio::test]
    async fn supervised_keeps_its_own_reason() {
        let p = policy("supervised", &[], None);
        let d = p.check(&request("send_email", serde_json::json!({}))).await;
        match d {
            ToolPolicyDecision::RequireApproval { reason } => {
                assert!(
                    reason.contains("supervised"),
                    "expected the supervised reason, got: {reason}"
                );
            }
            other => panic!("expected a park, got {}", decision_name(&other)),
        }
    }

    /// A pre-granted call still runs under `full` — "unless explicitly
    /// pre-granted", in the acceptance's words.
    ///
    /// This is the arm's placement doing the work: the grant check returns
    /// `Allow` long before the judgement arm is reached, so the operator's
    /// approval is not re-litigated by a classifier.
    #[tokio::test]
    async fn a_pre_granted_irreversible_call_still_runs() {
        let (p, grants) = granting_policy("full", &[], "finance");
        let args = serde_json::json!({ "to": "customer@example.com" });
        grants.grant(granted("finance", "send_email", args.clone()));
        let d = p.check(&request("send_email", args.clone())).await;
        assert_eq!(
            decision_name(&d),
            "allow",
            "the grant must still be honoured"
        );

        // ...and it was consumed, so the next identical call stops again
        // (#243 semantics, and #183 decision 3: a run may stop more than once).
        let again = p.check(&request("send_email", args)).await;
        assert_eq!(decision_name(&again), "park");
    }

    /// `always_approve` keeps its own reason under `full`.
    ///
    /// Both arms would park this call, so the decision alone cannot tell them
    /// apart — the reason can, and the operator's card shows the reason. If the
    /// judgement arm had been placed above `always_approve`, the operator would
    /// stop being told that this stop is one they themselves configured.
    #[tokio::test]
    async fn always_approve_keeps_its_own_reason_under_full() {
        let p = policy("full", &["send_email"], None);
        let d = p.check(&request("send_email", serde_json::json!({}))).await;
        match d {
            ToolPolicyDecision::RequireApproval { reason } => {
                assert!(
                    reason.contains("always-approve"),
                    "expected the always-approve reason, got: {reason}"
                );
            }
            other => panic!("expected a park, got {}", decision_name(&other)),
        }
    }

    /// `auto_approve_under_usd` is static configuration about spend, and it
    /// still speaks first.
    ///
    /// Worth stating as a test rather than leaving implicit: an operator who
    /// wrote "anything under $5 is fine" said something specific about money,
    /// and this arm does not get to overrule it. The daily cap above still
    /// does.
    #[tokio::test]
    async fn a_sub_threshold_spend_is_still_auto_approved() {
        let p = policy("full", &[], Some(5.0));
        let d = p
            .check(&request(
                "pay_invoice",
                serde_json::json!({ "amount_usd": 1.0 }),
            ))
            .await;
        assert_eq!(decision_name(&d), "allow");
    }

    /// Fail closed: a tool nobody declared stops under `full` rather than
    /// running because nothing recognised it.
    #[tokio::test]
    async fn an_undeclared_tool_stops_under_full() {
        let p = policy("full", &[], None);
        let d = p
            .check(&request("frobnicate_the_widget", serde_json::json!({})))
            .await;
        assert_eq!(decision_name(&d), "park");
    }

    /// Every stop reaches the operator. A `RequireApproval` that skipped
    /// `require_approval` would refuse the tool without ever queueing anything
    /// to park — the bug issue #172 closed — so the new arm is checked to go
    /// through the same door as every other.
    #[tokio::test]
    async fn a_judgement_stop_is_queued_for_the_operator() {
        let queue = ApprovalRequestQueue::default();
        let p = policy("full", &[], None).with_requests(queue.clone());
        let d = p.check(&request("send_email", serde_json::json!({}))).await;
        assert_eq!(decision_name(&d), "park");
        // `queued()`, deliberately, and neither `drain` nor `take_from`. Both of
        // those are being reshaped by PRs in flight — #625 changes `drain`'s
        // return type to `DrainedRequests`, and #439 removes `take_from`
        // entirely — and neither would conflict with this branch *textually*,
        // so both lanes would be green and whoever merged second would break
        // the tree. `queued()` is untouched by both and answers the question
        // this test is actually asking.
        assert_eq!(queue.queued(), 1, "the stop must be queued to park");
        // The reason reaching the operator is the same string the decision
        // carries — `require_approval` clones it onto the queued request — so
        // asserting it here needs no queue read at all.
        match d {
            ToolPolicyDecision::RequireApproval { reason } => assert!(
                !reason.is_empty(),
                "the operator needs a reason on the card"
            ),
            other => panic!("expected a park, got {}", decision_name(&other)),
        }
    }

    // ---- The path split (issue #674) --------------------------------------

    /// A policy nobody told about the path judges the strict way.
    ///
    /// The default is the safety property: a construction site added later,
    /// which has not thought about #674 at all, gets the agent rule rather than
    /// being silently exempted from it.
    #[tokio::test]
    async fn the_default_path_is_the_strict_one() {
        let p = policy("full", &[], None);
        assert_eq!(p.call_path, CallPath::Agent);
        let d = p
            .check(&request(
                "shell",
                serde_json::json!({ "command": "rm -rf ." }),
            ))
            .await;
        assert_eq!(decision_name(&d), "park");
    }

    /// ...and the workflow gate pass opts out of exactly that arm, because an
    /// operator authored the node past the manifest grant.
    #[tokio::test]
    async fn an_authored_node_is_not_stopped_by_the_judgement_arm() {
        let p = policy("full", &[], None).for_authored_workflow_nodes();
        let d = p
            .check(&request(
                "shell",
                serde_json::json!({ "command": "rm -rf ." }),
            ))
            .await;
        assert_eq!(decision_name(&d), "allow");
    }

    /// The opt-out scopes ONE arm. This is the assertion that keeps it from
    /// becoming a way to run a workflow node past the whole chain.
    ///
    /// Each case below is decided by an arm ABOVE the judgement one, so each
    /// must decide identically on both paths. A refactor that moved the path
    /// check any earlier — the obvious "simplification", since it would let
    /// `check` return before doing any work — fails here rather than shipping a
    /// bypass.
    #[tokio::test]
    async fn the_authored_path_changes_nothing_above_the_judgement_arm() {
        let cases: &[(&str, &[&str], &str, &str)] = &[
            // `readonly` denies an external effect; it does not park it, and it
            // certainly does not allow it because a workflow authored it.
            ("readonly", &[], "send_email", "deny"),
            // `always_approve` is the operator asking to be told. It outranks
            // the tier on both paths — and on the authored path it is now the
            // operator's whole control surface, so it had better work.
            ("full", &["shell"], "shell", "park"),
            // `supervised` parks a consequence on its own.
            ("supervised", &[], "shell", "park"),
        ];
        for (mode, always, tool, expected) in cases {
            let args = serde_json::json!({ "command": "ls" });
            let agent_path = policy(mode, always, None)
                .check(&request(tool, args.clone()))
                .await;
            let node_path = policy(mode, always, None)
                .for_authored_workflow_nodes()
                .check(&request(tool, args))
                .await;
            assert_eq!(
                decision_name(&agent_path),
                *expected,
                "{mode}/{tool}: the arm under test is not the one deciding here"
            );
            assert_eq!(
                decision_name(&node_path),
                *expected,
                "{mode}/{tool}: the authored path must only scope the judgement \
                 arm, and this decision is made above it"
            );
        }
    }

    /// The boundary condition, at the chain rather than in the pure module: the
    /// same node, the same tier, the same path — and templated arguments.
    #[tokio::test]
    async fn an_authored_node_templated_from_upstream_output_still_stops() {
        let p = policy("full", &[], None).for_authored_workflow_nodes();
        let d = p
            .check(&request(
                "shell",
                serde_json::json!({ "command": "=previous.output" }),
            ))
            .await;
        assert_eq!(
            decision_name(&d),
            "park",
            "the operator declared the shape, not the command"
        );
    }
}
