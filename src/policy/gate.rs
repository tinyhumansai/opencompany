//! The manifest-`[policy]`-driven [`ApprovalGate`] implementation.
//!
//! Evaluation follows the precedence in
//! [`docs/spec/company-brain/approvals.md`](../../docs/spec/company-brain/approvals.md):
//!
//! 1. `never_do` hard-deny (Phase 1: the delegation-rule compiler is stubbed,
//!    so this list is always empty).
//! 2. `[policy].always_approve` effect kinds always park for approval.
//! 3. mode dispatch: `readonly` gates everything, `full` allows everything,
//!    `supervised` applies the checkpoint taxonomy by [`EffectGroup`], and
//!    `auto` applies it too — see [`evaluate_auto`](ManifestApprovalGate::evaluate_auto)
//!    for why those two coincide on this path and differ sharply on the other.
//!
//! There are **four** policy modes, and only three of them
//! (`readonly`, `supervised`, `full`) take their names from OpenHuman's own
//! security tiers. `auto` is opencompany's, added by issue #560, and its
//! addition is what stopped the mapping being 1:1 — see
//! [`docs/spec/company-brain/grants.md`](../../docs/spec/company-brain/grants.md).
//!
//! This header said "three" for long enough that the gate below grew to match
//! it: `auto` fell into the `_` catch-all and parked every native effect,
//! making the tier every provisioned company boots on **stricter** than the one
//! below it on the ladder (issue #1454). The dispatch is now
//! [`mode_decision`](ManifestApprovalGate::mode_decision), which returns `None`
//! only for a word that is not a tier at all, so a test can tell "no arm" from
//! "an arm that decided to park".
//!
//! ## The ladder invariant
//!
//! [`POLICY_MODES`](crate::company::POLICY_MODES) is ordered by increasing
//! autonomy and the console renders it in that order, so the gate owes it one
//! property: **for any given effect, permissiveness must never decrease as you
//! move up the list.** An operator who moves a company one tier up to be
//! interrupted less must not be interrupted more. `the_tier_ladder_is_monotonic`
//! in this module's tests pins that across every tier and every branch of the
//! taxonomy, rather than spot-checking one arm — which is precisely what let
//! #1454 survive.
//!
//! `evaluate` returns a bare [`PolicyDecision`]; the [`ApprovalId`] for a
//! `RequireApproval` outcome is minted separately by [`park`](ManifestApprovalGate::park).
//!
//! Silence is a default-deny: a parked approval left unresolved past its TTL
//! (default [`DEFAULT_TTL_MILLIS`], overridable per company with
//! `[policy].approval_ttl_hours`) resolves to deny, whether swept by
//! [`sweep_expired`](ManifestApprovalGate::sweep_expired) or observed at
//! resolution time by [`resolve_at`](ManifestApprovalGate::resolve_at) /
//! [`resolve_amended`](ManifestApprovalGate::resolve_amended).
//!
//! **The TTL is only half of default-deny-on-silence.** The other half is
//! something actually running the sweep, which until issue #971 only a company
//! with a manifest `[[schedule]]` had — see
//! [`MaintenanceTicker`](crate::runtime::maintenance::MaintenanceTicker), which
//! now drives it for every registered company. A shorter deadline with nothing
//! sweeping is still a queue that never empties.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use async_trait::async_trait;

use crate::Result;
use crate::company::Policy;
use crate::ports::approvals::ApprovalGate;
use crate::ports::now_millis;
use crate::ports::types::{
    Actor, ApprovalId, CompanyId, Effect, EffectGroup, PolicyDecision, Verdict,
};

/// Default time-to-live for a parked approval: 24 hours in milliseconds.
///
/// **Was 7 days until issue #971.** A parked call is refused and re-dispatched
/// rather than suspended (#243/#469), so approving a three-day-old entry does
/// not usefully resume the turn that raised it — the turn is long gone. A
/// week-long deadline therefore did not buy an operator a week of useful
/// decisions; it bought a queue whose oldest entries were unactionable *and*
/// still counted toward the badge, which is how a badge stops describing
/// current state and starts being ignored.
///
/// 24 hours is one working day: an approval raised during a day the operator
/// works is still there when they next look, and one nobody looked at for a
/// full day is one the work behind it has already moved past.
///
/// A company that genuinely wants longer says so explicitly with
/// `[policy].approval_ttl_hours` rather than inheriting it from a constant.
pub const DEFAULT_TTL_MILLIS: u64 = 24 * 60 * 60 * 1000;

/// Replays a company's event log to decide whether it should boot with the
/// emergency stop engaged (issue #86).
///
/// **The event log is the durable state, not a mirror of it.** The kill switch
/// deliberately has no field on
/// [`CompanyRecord`](crate::ports::types::CompanyRecord): a second copy of a
/// safety flag is a second thing that can disagree with the first, and the
/// append-only log already answers "what did the last operator decide" exactly.
/// The last [`EmergencyPauseChanged`](crate::ports::types::CompanyEvent::EmergencyPauseChanged)
/// wins; a log with none was never stopped.
///
/// # Fail-safe
///
/// A read failure returns `Err`, and the caller
/// ([`hydrate_emergency`](crate::runtime::CompanyRuntime::hydrate_emergency))
/// turns that into **stopped**. This deliberately diverges from
/// [`sweep_interrupted_runs`](crate::runtime::sweep_interrupted_runs) beside it,
/// which swallows read failures because record-keeping must never stop a company
/// booting. Here the read *is* the safety decision: a company that cannot prove
/// it was running must not assume it was.
pub async fn replayed_emergency(
    events: &std::sync::Arc<dyn crate::ports::EventLog>,
    company: &CompanyId,
) -> Result<bool> {
    use crate::ports::types::{CompanyEvent, EventSeq};

    // A full scan, on the same terms as the interrupted-run sweep beside it:
    // boot already reads this log end to end, and the port exposes no reverse
    // or filtered read to do better with.
    let stored = events
        .read_from(company, EventSeq::new(0), usize::MAX)
        .await?;
    Ok(stored
        .iter()
        .rev()
        .find_map(|stored| match &stored.event {
            CompanyEvent::EmergencyPauseChanged { engaged, .. } => Some(*engaged),
            _ => None,
        })
        .unwrap_or(false))
}

/// A parked effect awaiting operator resolution.
#[derive(Clone, Debug)]
struct ParkedEffect {
    effect: Effect,
    parked_at_millis: u64,
}

/// What actually happened when a resolve reached the queue (issue #243).
///
/// The [`ApprovalGate`] port's `resolve` returns `Option<Effect>`, which
/// collapses four distinct situations into one `None`: the approval was denied,
/// it expired, it was never parked, or it was already resolved. That is enough
/// for "should I execute the effect?" and nowhere near enough for anything else
/// — most importantly it cannot tell "the operator denied this" from "this
/// approval is already gone", so a double-submit (a double-click, a retried
/// request, two operators on the same queue) looked exactly like a deny and got
/// the full treatment: a second `ApprovalResolved` journal line and a second
/// follow-up cycle, both describing an approval that no longer existed.
///
/// Returned by the concrete gate rather than widened onto the port, following
/// the amend path — [`resolve_amended`](ManifestApprovalGate::resolve_amended) —
/// which already reaches past the `dyn` boundary for the same reason.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolveOutcome {
    /// No such approval is parked: an unknown id, or one already resolved by an
    /// earlier call. The caller must treat this as a no-op, not a deny.
    NotParked,
    /// The approval was parked but is past its TTL, so it resolves to a
    /// default-deny whatever the operator asked for. It IS removed.
    Expired,
    /// The operator denied it. Removed, and here is the effect the card showed,
    /// retained so a standing denial can be minted against the same scoped
    /// arguments (issue #1458) rather than the payload-scrubbed copy the journal
    /// keeps (issue #351), which would read as an unscoped wildcard.
    Denied(Effect),
    /// The operator approved it in time. Removed, and here is the effect.
    Approved(Effect),
}

/// The default [`ApprovalGate`]: evaluates effects against a company's
/// `[policy]` and holds the in-memory approval queue.
pub struct ManifestApprovalGate {
    policy: RwLock<Policy>,
    policy_hitl_enabled: AtomicBool,
    ttl_millis: AtomicU64,
    parked: Mutex<HashMap<ApprovalId, ParkedEffect>>,
    /// Effects removed by TTL expiry, retained only until the runtime completes
    /// their retirement transaction.
    expired_effects: Mutex<HashMap<ApprovalId, Effect>>,
    /// The governance kill switch (issue #86).
    ///
    /// An `AtomicBool` rather than a lock because `evaluate` reads it on every
    /// effect and a kill switch that adds contention to the path it guards is a
    /// poor one. It is a *cache* of the event log, hydrated at boot by
    /// [`replayed_emergency`]; the log is the durable truth.
    ///
    /// Defaults to `false` because a freshly constructed gate has not been told
    /// anything yet, and the one caller that can distinguish "not in emergency"
    /// from "could not find out" — the boot path — engages it explicitly on a
    /// read failure. See
    /// [`hydrate_emergency`](crate::runtime::CompanyRuntime::hydrate_emergency).
    emergency: AtomicBool,
}

impl ManifestApprovalGate {
    /// Builds a gate from a company's manifest `[policy]` block.
    ///
    /// **The TTL default resolves HERE, not at parse** (issue #971).
    /// [`Policy::approval_ttl_hours`] is a plain `Option` with no serde
    /// default, so a manifest that never mentioned the knob deserializes to
    /// `None` — the same bytes and the same value it did before the field
    /// existed. That matters because
    /// [`carry_policy_override`](crate::runtime::builder) compares the previous
    /// boot's seed `[policy]` against this one's *as whole blocks* to decide
    /// whether an operator's console override survives a rebuild. A field that
    /// defaulted to `Some(24)` at parse would make the seed change under any
    /// company whose manifest is silent the moment this constant moves, and the
    /// rebuild would read that as version control having spoken and silently
    /// discard the override. Nobody would have edited anything. See the same
    /// trap spelled out on [`Policy::mode`](crate::company::Policy::mode).
    pub fn new(policy: Policy) -> Self {
        let ttl_millis = policy
            .approval_ttl_hours
            .map(|hours| hours.saturating_mul(60 * 60 * 1000))
            .unwrap_or(DEFAULT_TTL_MILLIS);
        Self {
            policy: RwLock::new(policy),
            policy_hitl_enabled: AtomicBool::new(true),
            ttl_millis: AtomicU64::new(ttl_millis),
            parked: Mutex::new(HashMap::new()),
            expired_effects: Mutex::new(HashMap::new()),
            emergency: AtomicBool::new(false),
        }
    }

    /// Disables policy-generated approval decisions while leaving explicit
    /// `park` calls and hard emergency/read-only denials available.
    pub fn with_policy_hitl_disabled(self) -> Self {
        self.policy_hitl_enabled.store(false, Ordering::Relaxed);
        self
    }

    /// Engages (`true`) or releases (`false`) the emergency stop.
    ///
    /// Returns the **previous** value. The atomic swap is what makes a
    /// transition race-safe: exactly one caller observes the old state, so
    /// [`CompanyRuntime::emergency_pause`] and
    /// [`CompanyRuntime::emergency_resume`] can journal an
    /// [`EmergencyPauseChanged`](crate::ports::types::CompanyEvent::EmergencyPauseChanged)
    /// event only for the caller that actually changed the switch, matching the
    /// event count to the number of real transitions under a double-press.
    ///
    /// `Ordering::SeqCst` on both ends: this is a safety flag read by request
    /// handlers on other threads, and the cost of the strongest ordering on a
    /// single boolean is irrelevant next to being sure a handler that starts
    /// after the switch was pulled observes it pulled.
    pub fn set_emergency(&self, engaged: bool) -> bool {
        self.emergency.swap(engaged, Ordering::SeqCst)
    }

    /// Whether the emergency stop is currently engaged.
    pub fn is_emergency(&self) -> bool {
        self.emergency.load(Ordering::SeqCst)
    }

    /// Overrides the parked-approval TTL (default [`DEFAULT_TTL_MILLIS`]).
    pub fn with_ttl_millis(mut self, ttl_millis: u64) -> Self {
        self.ttl_millis = AtomicU64::new(ttl_millis);
        self
    }

    /// How long a parked approval has before it default-denies.
    ///
    /// Read by [`CompanyRuntime::pending_approvals`](crate::CompanyRuntime::pending_approvals)
    /// to project each card's deadline (issue #971). Exposed rather than
    /// recomputed from `[policy]` at the projection because the gate is the one
    /// that resolves the default, and a second resolution of the same rule is a
    /// second thing that can disagree — the console would then show a deadline
    /// the gate does not enforce.
    pub fn ttl_millis(&self) -> u64 {
        self.ttl_millis.load(Ordering::Relaxed)
    }

    /// The policy snapshot the gate currently evaluates against.
    ///
    /// What [`apply_effective_policy`](Self::apply_effective_policy) last
    /// installed, or the `[policy]` block [`new`](Self::new) was built from when
    /// nothing has overridden it. The cycle reads this for a test-injected gate
    /// so the harness roster is pinned to the SAME policy the native gate keeps
    /// (issue #1455) — an injected gate carries its own policy on purpose, which
    /// may differ from the persisted record's effective one.
    pub fn policy(&self) -> Policy {
        self.policy.read().expect("policy lock poisoned").clone()
    }

    /// Updates the deadline used for new and already parked approvals.
    ///
    /// The policy overlay is an operator control, so waiting for a process
    /// restart would make the Settings panel report a deadline the live queue
    /// does not use. A parked card remains the same request, but its deadline
    /// is evaluated from the current company policy each time it is displayed
    /// or resolved.
    pub fn set_ttl_millis(&self, ttl_millis: u64) {
        self.ttl_millis.store(ttl_millis, Ordering::Relaxed);
    }

    /// Replaces the policy snapshot the gate evaluates against, keeping the
    /// parked queue and the emergency switch.
    ///
    /// Used at boot/rebuild time and at the start of every cycle (issue #1455):
    /// the gate is constructed from the seed's `[policy]` alone and the
    /// operator's console override resolves only after the persisted record is
    /// read — see
    /// [`CompanyRecord::effective_policy`](crate::ports::types::CompanyRecord::effective_policy).
    /// Applying the effective policy keeps native evaluation (mode,
    /// `always_approve`, spend cap) and the derived deadline enforcing what the
    /// console reports; without it a persisted override would still be returned
    /// by `GET` while the live gate silently reverted to the manifest snapshot,
    /// which is especially unsafe after an operator *shortened* a deadline.
    pub fn apply_effective_policy(&self, policy: Policy) {
        self.apply_effective_ttl(&policy);
        self.policy
            .write()
            .expect("policy lock poisoned")
            .clone_from(&policy);
    }

    /// Moves only the deadline derived from `policy`, leaving the evaluation
    /// snapshot (mode, `always_approve`, spend cap) untouched.
    ///
    /// The TTL is *immediate* by contract while the rest of the policy moves at
    /// the next safe turn boundary: a parked card remains the same request, but
    /// its deadline is re-evaluated against the current TTL each time it is
    /// displayed, swept or resolved, so delaying the deadline until the next
    /// cycle would let approvals parked under a longer TTL outlive the one the
    /// console just reported. The ops handler applies this right after a policy
    /// PUT/DELETE persists, and [`apply_effective_policy`](Self::apply_effective_policy)
    /// applies it alongside the snapshot at boot and per-cycle.
    pub fn apply_effective_ttl(&self, policy: &Policy) {
        let ttl_millis = policy
            .approval_ttl_hours
            .map(|hours| hours.saturating_mul(60 * 60 * 1000))
            .unwrap_or(DEFAULT_TTL_MILLIS);
        self.ttl_millis.store(ttl_millis, Ordering::Relaxed);
    }

    /// The ids of every currently-parked approval.
    pub fn parked_ids(&self) -> Vec<ApprovalId> {
        self.parked
            .lock()
            .expect("parked map poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Re-parks an effect under a known id (used by boot replay to rebuild the
    /// queue from the event log).
    pub fn rehydrate(&self, id: ApprovalId, effect: Effect, parked_at_millis: u64) {
        self.parked.lock().expect("parked map poisoned").insert(
            id,
            ParkedEffect {
                effect,
                parked_at_millis,
            },
        );
    }

    /// Re-anchors a parked approval's TTL window to `now`, giving the operator a
    /// fresh full deadline on it (issue #1805). Returns whether an entry was
    /// actually moved — `false` for an id that is not (or no longer) parked, so a
    /// caller can answer 404 rather than pretend it extended something.
    ///
    /// # Why a full fresh window, not "+N hours"
    ///
    /// The parked entry carries a single `parked_at_millis`, and both the sweeper
    /// and the console's deadline are `parked_at + ttl`. Moving that instant to
    /// `now` is therefore the *whole* of an extension: the sweep that would have
    /// retired it no longer sees it expired, and the projected deadline moves in
    /// lockstep, with no second knob that could disagree. An additive "+N" would
    /// need its own stored offset and a second place computing the deadline — the
    /// exact fork [`ttl_millis`](Self::ttl_millis) exists to avoid.
    ///
    /// The durable half lives in the journal (`record_extended`): this moves the
    /// **live** anchor the sweeper reads, and boot replay re-applies the move by
    /// rehydrating from the journal's extended anchor, so an extension survives a
    /// redeploy rather than reverting to the original park instant.
    pub fn extend(&self, id: &ApprovalId, now_millis: u64) -> bool {
        let mut map = self.parked.lock().expect("parked map poisoned");
        match map.get_mut(id) {
            Some(parked) => {
                parked.parked_at_millis = now_millis;
                true
            }
            None => false,
        }
    }

    /// Removes every parked approval older than the TTL relative to `now`,
    /// returning the ids that expired (they resolve to deny).
    pub fn sweep_expired(&self, now_millis: u64) -> Vec<ApprovalId> {
        self.sweep_expired_capped(now_millis, usize::MAX)
    }

    /// [`sweep_expired`](Self::sweep_expired), taking at most `limit` entries,
    /// **oldest first** (issue #971).
    ///
    /// The cap is what keeps a first sweep after a long silence from turning
    /// into one unbounded burst of retirement work. Each retirement is a
    /// journal append, a grant clear, an event append and possibly a released
    /// #469 continuation spawning a whole agent turn; a company that has been
    /// accumulating for days would do all of that for its entire backlog inside
    /// a single minute tick, on the tick shared by every other company in the
    /// process. Capped, the backlog drains over a few minutes and nothing else
    /// waits on it.
    ///
    /// **Oldest first, so the cap is not a lottery.** The map is a `HashMap`
    /// and its iteration order is randomized per process, so an uncapped-order
    /// cap would retire an arbitrary subset and leave an arbitrary one — the
    /// same entry could sit unswept across many ticks while newer ones went
    /// first. Sorting by park instant makes the drain deterministic and makes
    /// "the oldest, most unactionable entries go first" true rather than
    /// incidental.
    pub fn sweep_expired_capped(&self, now_millis: u64, limit: usize) -> Vec<ApprovalId> {
        let mut map = self.parked.lock().expect("parked map poisoned");
        let mut expired: Vec<(u64, ApprovalId)> = map
            .iter()
            .filter(|(_, pe)| now_millis.saturating_sub(pe.parked_at_millis) >= self.ttl_millis())
            .map(|(id, pe)| (pe.parked_at_millis, id.clone()))
            .collect();
        expired.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.as_ref().cmp(b.1.as_ref())));
        expired.truncate(limit);
        let expired: Vec<ApprovalId> = expired.into_iter().map(|(_, id)| id).collect();
        for id in &expired {
            if let Some(parked) = map.remove(id) {
                self.expired_effects
                    .lock()
                    .expect("expired effects poisoned")
                    .insert(id.clone(), parked.effect);
            }
        }
        expired
    }

    /// Takes the effect removed by an expiry, if its runtime retirement has not
    /// consumed it yet.
    pub fn take_expired_effect(&self, id: &ApprovalId) -> Option<Effect> {
        self.expired_effects
            .lock()
            .expect("expired effects poisoned")
            .remove(id)
    }

    /// A clone of a parked effect without resolving it.
    ///
    /// Used by the amend path to overlay an operator's payload edit onto the
    /// original before re-submitting it for execution.
    pub fn parked_effect(&self, id: &ApprovalId) -> Option<Effect> {
        self.parked
            .lock()
            .expect("parked map poisoned")
            .get(id)
            .map(|pe| pe.effect.clone())
    }

    /// Resolves a parked approval to an operator-amended effect
    /// (approve-with-edit), as of `now`.
    ///
    /// Removes the parked entry and returns the `amended` effect to execute, or
    /// `None` when the approval is unknown or has expired past its TTL — the
    /// same default-deny-on-silence that governs [`resolve_at`](Self::resolve_at).
    ///
    /// Prefer [`resolve_amended_outcome`](Self::resolve_amended_outcome) when
    /// the caller has to *report* what happened: this `None` collapses "there
    /// was nothing parked" into "the deadline had passed", and those are
    /// different things to write into an audit trail (issue #1449).
    pub fn resolve_amended(
        &self,
        id: &ApprovalId,
        amended: Effect,
        by: Actor,
        now_millis: u64,
    ) -> Option<Effect> {
        match self.resolve_amended_outcome(id, amended, by, now_millis) {
            ResolveOutcome::Approved(effect) => Some(effect),
            _ => None,
        }
    }

    /// The amend counterpart to
    /// [`resolve_outcome`](Self::resolve_outcome): resolves a parked approval to
    /// an operator-amended effect and says **which** outcome that was
    /// (issue #1449).
    ///
    /// An amend is an approve, so the outcomes it can produce are the same
    /// three the plain approve can: [`ResolveOutcome::NotParked`],
    /// [`ResolveOutcome::Expired`], and [`ResolveOutcome::Approved`] carrying
    /// the *amended* effect. It never denies — a deny cannot carry an
    /// amendment, and the route refuses the pairing.
    ///
    /// The removal and the outcome decision are one critical section, for the
    /// same reason they are on `resolve_outcome`: two concurrent resolves of
    /// one id must not both win.
    pub fn resolve_amended_outcome(
        &self,
        id: &ApprovalId,
        amended: Effect,
        _by: Actor,
        now_millis: u64,
    ) -> ResolveOutcome {
        let Some(parked) = self.parked.lock().expect("parked map poisoned").remove(id) else {
            return ResolveOutcome::NotParked;
        };
        if now_millis.saturating_sub(parked.parked_at_millis) >= self.ttl_millis() {
            self.expired_effects
                .lock()
                .expect("expired effects poisoned")
                .insert(id.clone(), parked.effect);
            return ResolveOutcome::Expired;
        }
        ResolveOutcome::Approved(amended)
    }

    /// Resolves a parked approval as of `now`, reporting **which** of the four
    /// outcomes occurred rather than collapsing them into an `Option`
    /// (issue #243).
    ///
    /// The `remove` and the outcome decision are one critical section, so two
    /// concurrent resolves of the same id cannot both win: whichever thread
    /// takes the lock first gets `Approved` / `Denied` / `Expired`, and every
    /// other thread finds the map empty and gets [`ResolveOutcome::NotParked`].
    /// That is what makes an approve idempotent at the source instead of
    /// depending on callers to check-then-act, which is racy by construction.
    pub fn resolve_outcome(
        &self,
        id: &ApprovalId,
        verdict: Verdict,
        _by: Actor,
        now_millis: u64,
    ) -> ResolveOutcome {
        let Some(parked) = self.parked.lock().expect("parked map poisoned").remove(id) else {
            return ResolveOutcome::NotParked;
        };
        if now_millis.saturating_sub(parked.parked_at_millis) >= self.ttl_millis() {
            self.expired_effects
                .lock()
                .expect("expired effects poisoned")
                .insert(id.clone(), parked.effect);
            return ResolveOutcome::Expired;
        }
        match verdict {
            Verdict::Approve => ResolveOutcome::Approved(parked.effect),
            // Carry the effect rather than re-reading the journal's scrubbed
            // copy (issue #351): a standing deny minted from that copy would
            // lose the scope the operator actually refused (issue #1458).
            Verdict::Deny => ResolveOutcome::Denied(parked.effect),
        }
    }

    /// Resolves a parked approval as of `now`, so expiry is testable.
    ///
    /// An expired approval resolves to deny (`None`) regardless of `verdict`.
    pub fn resolve_at(
        &self,
        id: &ApprovalId,
        verdict: Verdict,
        _by: Actor,
        now_millis: u64,
    ) -> Option<Effect> {
        let parked = self
            .parked
            .lock()
            .expect("parked map poisoned")
            .remove(id)?;
        if now_millis.saturating_sub(parked.parked_at_millis) >= self.ttl_millis() {
            return None;
        }
        match verdict {
            Verdict::Approve => Some(parked.effect),
            Verdict::Deny => None,
        }
    }

    /// Whether this effect is one a person would want to know had **already
    /// happened** before re-running the work that produced it (issue #351).
    ///
    /// It is the supervised checkpoint taxonomy read as a question about the
    /// past rather than the future: signing, publishing, touching identity,
    /// spending at or over the cap, first contact with a counterparty. Those
    /// are the effects `evaluate_supervised` refuses to wave through, and they
    /// are refused precisely because they cannot be taken back.
    ///
    /// Deliberately **mode-independent**. A `full`-mode company executes every
    /// one of these without ever parking it, which is exactly the case this
    /// warning exists for: the operator was never asked, so the retry dialog is
    /// the first and only place they learn a filing was already submitted.
    /// Asking the same taxonomy the same way for every mode also means a
    /// company that later tightens its policy does not retroactively change
    /// what its history says it did.
    ///
    /// Delegating to [`evaluate_supervised`](Self::evaluate_supervised) rather
    /// than restating the rules is the point: two copies of "which effects are
    /// irreversible" would drift, and the copy in the retry dialog would drift
    /// silently — nobody notices a warning that stopped naming something.
    pub fn is_irreversible(&self, effect: &Effect) -> bool {
        matches!(
            self.evaluate_supervised(effect),
            PolicyDecision::RequireApproval
        )
    }

    /// The tier dispatch, as an `Option` so "no arm for this word" is
    /// distinguishable from "an arm that decided to park" (issue #1454).
    ///
    /// `evaluate` turns `None` into [`PolicyDecision::RequireApproval`], which
    /// is the same fail-safe the `_` catch-all always applied. The difference is
    /// only visible from a test: `auto` sat in that catch-all for two releases
    /// and nothing could see it, because a tier that parks everything and a word
    /// nobody implemented produce identical decisions. Now
    /// `every_policy_mode_has_a_named_arm` walks
    /// [`POLICY_MODES`](crate::company::POLICY_MODES) and fails on the day a
    /// fifth tier is added to that list and forgotten here.
    ///
    /// Takes the evaluated `Policy` snapshot as an argument — not `&self` — so a
    /// caller already holding the policy read guard can dispatch without a
    /// recursive read of the non-reentrant `RwLock`, and `mode` as a separate
    /// argument so the test can ask about a word without building a whole
    /// [`Policy`] whose mode it is, keeping the fail-safe arm reachable.
    fn mode_decision(policy: &Policy, mode: &str, effect: &Effect) -> Option<PolicyDecision> {
        match mode {
            "full" => Some(PolicyDecision::Allow),
            "readonly" => Some(PolicyDecision::RequireApproval),
            "supervised" => Some(Self::evaluate_supervised_with_policy(policy, effect)),
            "auto" => Some(Self::evaluate_auto(policy, effect)),
            _ => None,
        }
    }

    /// The `auto` tier, for **native** effects (issue #1454).
    ///
    /// `auto`'s contract, in the operator's words and in the console's:
    /// *the agents work on their own and stop before anything that leaves the
    /// company or spends money.* On this path that is, exactly and completely,
    /// the supervised checkpoint taxonomy — so this delegates rather than
    /// restating it.
    ///
    /// # Why the two coincide here and not on the tool path
    ///
    /// The tier is real; it just does its work somewhere else. On the **tool**
    /// path `auto` is [`Consequence::parks_under_auto`](crate::policy::Consequence::parks_under_auto),
    /// which waves through the calls declared [`Standing::Grantable`](crate::policy::Standing::Grantable)
    /// — chiefly the agent's own sandbox writes (`file_write`, `edit`,
    /// `apply_patch`, `memory_store`) and reads scoped to one connected account.
    /// The declaration table in [`consequence`](crate::policy::consequence) is
    /// the list; this is a gloss on it, not a copy. Those are what `supervised`
    /// parks and `auto` does not, and they are the whole difference between the
    /// tiers.
    ///
    /// The **native** taxonomy has no such calls to wave through. Every group
    /// [`evaluate_supervised`](Self::evaluate_supervised) parks — spend at or
    /// over the cap, a message to a counterparty nobody has talked to, a
    /// signature, a publish, an identity change, an engagement over the cap — is
    /// by definition something that leaves the company or spends money, which is
    /// the exact line `auto` says it stops at. The only inside-the-company
    /// native bucket is [`EffectGroup::Other`], and `supervised` already allows
    /// it. So there is nothing for `auto` to loosen here, and the honest
    /// implementation is one that says so.
    ///
    /// # Why not the stricter reading
    ///
    /// The tempting alternative — park `Spend`/`Send`/`Hire` unconditionally,
    /// withholding the cap relief and the established-thread relief as
    /// "`supervised`'s concessions" — inverts the ladder a second time. It would
    /// park a $1 spend and a reply on a running email thread that `supervised`,
    /// the tier *below* it, waves through. A tier cannot be sold as more
    /// autonomy and deliver less; see the ladder invariant on this module.
    ///
    /// # If they ever diverge
    ///
    /// This is a named seam, not an alias, so a future native effect that
    /// genuinely belongs to `auto` and not to `supervised` gets its own arm
    /// here. The invariant that must survive that edit is the one direction:
    /// whatever this parks must stay a **subset** of what
    /// [`evaluate_supervised`](Self::evaluate_supervised) parks.
    fn evaluate_auto(policy: &Policy, effect: &Effect) -> PolicyDecision {
        Self::evaluate_supervised_with_policy(policy, effect)
    }

    /// Evaluates the supervised taxonomy using a captured policy snapshot.
    ///
    /// Keeping the cap as an argument prevents a caller that already holds the
    /// policy read guard from attempting a recursive read of the non-reentrant
    /// `RwLock`.
    fn evaluate_supervised_with_policy(policy: &Policy, effect: &Effect) -> PolicyDecision {
        Self::evaluate_supervised_with_cap(effect, policy.auto_approve_under_usd)
    }

    fn evaluate_supervised(&self, effect: &Effect) -> PolicyDecision {
        let policy = self.policy.read().expect("policy lock poisoned");
        Self::evaluate_supervised_with_policy(&policy, effect)
    }

    fn evaluate_supervised_with_cap(effect: &Effect, cap: Option<f64>) -> PolicyDecision {
        match effect.group() {
            // Spend under the cap (strict `<`) is auto-allowed; at/over the cap,
            // with no cap, or with an unknown amount, it parks.
            EffectGroup::Spend => match (effect.amount_usd(), cap) {
                (Some(amount), Some(cap)) if amount < cap => PolicyDecision::Allow,
                _ => PolicyDecision::RequireApproval,
            },
            // First message to a new counterparty parks; established threads pass.
            EffectGroup::Send => {
                if effect.is_established_thread() && !effect.is_first_time_counterparty() {
                    PolicyDecision::Allow
                } else {
                    PolicyDecision::RequireApproval
                }
            }
            // Irreversible / identity-touching effects always park.
            EffectGroup::Sign | EffectGroup::Publish | EffectGroup::Identity => {
                PolicyDecision::RequireApproval
            }
            // Hiring parks for a first-time counterparty or at/over the cap.
            EffectGroup::Hire => {
                let over_cap = matches!(
                    (effect.amount_usd(), cap),
                    (Some(amount), Some(cap)) if amount >= cap
                );
                if effect.is_first_time_counterparty() || over_cap {
                    PolicyDecision::RequireApproval
                } else {
                    PolicyDecision::Allow
                }
            }
            EffectGroup::Other => PolicyDecision::Allow,
        }
    }
}

#[async_trait]
impl ApprovalGate for ManifestApprovalGate {
    async fn evaluate(&self, _company: &CompanyId, effect: &Effect) -> Result<PolicyDecision> {
        // 0. The emergency stop (issue #86), ahead of every policy rule
        //    including `always_approve`.
        //
        //    `Deny`, not `RequireApproval`: parking would leave the queue as the
        //    escape hatch from the kill switch, so an operator who pulled it
        //    could re-authorise the very effects they just stopped without ever
        //    releasing it. Denial returns to the brain as a refusal it replans
        //    around, which is what "park all new work" has to mean.
        //
        //    `EffectGroup::Other` is exempt so chat survives — the operator has
        //    to be able to ask the company what it was doing. The gate does not
        //    police which tools `Other` covers, so "chat survives" is an
        //    observation, not a promise about every non-conversational effect.
        if self.is_emergency() && effect.group != EffectGroup::Other {
            return Ok(PolicyDecision::Deny);
        }

        if !self.policy_hitl_enabled.load(Ordering::Relaxed) {
            let policy = self.policy.read().expect("policy lock poisoned");
            if policy.mode.eq_ignore_ascii_case("readonly")
                && effect.kind != crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND
            {
                return Ok(PolicyDecision::Deny);
            }
            return Ok(PolicyDecision::Allow);
        }

        // 1. `never_do` hard-deny — the delegation-rule compiler is a Phase-1
        //    stub, so this list is currently always empty.

        // 2. `always_approve` effect kinds park regardless of mode or amount.
        //
        //    The match is `always_approve::matches`, shared with the harness
        //    tool policy. This arm used to compare exactly while the harness
        //    also honoured a leading segment, so one operator list meant two
        //    different things depending on which brain was running (issue
        //    #684). Adopting the harness rule here widens this arm: an entry
        //    like `payment` now parks `payment.send` natively, where before it
        //    parked nothing. That direction is the fail-safe one — an operator
        //    who named a family meant the family.
        let policy = self.policy.read().expect("policy lock poisoned");
        if crate::policy::always_approve::matches(&policy.always_approve, effect.kind()) {
            return Ok(PolicyDecision::RequireApproval);
        }

        // Mode dispatch against the held snapshot. A word with no arm is not a
        // tier — the manifest validator rejects anything outside `POLICY_MODES`
        // before a company loads — so `None` here means a path that reached a
        // `Policy` without validation. It fails safe: require approval.
        Ok(Self::mode_decision(&policy, &policy.mode, effect)
            .unwrap_or(PolicyDecision::RequireApproval))
    }

    async fn park(&self, _company: &CompanyId, effect: Effect) -> Result<ApprovalId> {
        // The emergency stop (issue #86) vetoes the park path too, not just
        // `evaluate`. The harness approval route — a tool call OpenHuman already
        // gated inline — parks without ever consulting `evaluate`, so without
        // this check a gated effect queued *after* the switch was pulled could
        // be released for execution by an approver. This is the same veto
        // `evaluate` applies, so an `EffectGroup::Other` effect (chat) still
        // parks and an approval parked *before* the stop stays resolvable.
        if self.is_emergency() && effect.group != EffectGroup::Other {
            return Err(crate::OpenCompanyError::EmergencyStop(format!(
                "refusing to park {} while stopped",
                effect.kind
            )));
        }

        let id = ApprovalId::generate();
        self.parked.lock().expect("parked map poisoned").insert(
            id.clone(),
            ParkedEffect {
                effect,
                parked_at_millis: now_millis(),
            },
        );
        Ok(id)
    }

    async fn resolve(
        &self,
        id: &ApprovalId,
        verdict: Verdict,
        by: Actor,
    ) -> Result<Option<Effect>> {
        Ok(self.resolve_at(id, verdict, by, now_millis()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::ActorKind;

    /// The fence these tests run under, written out rather than taken from
    /// [`DEFAULT_ALWAYS_APPROVE`](crate::company::DEFAULT_ALWAYS_APPROVE).
    ///
    /// It used to be the default, which made every assertion below depend on a
    /// constant these tests do not own — and when that constant turned out to
    /// gate nothing on the *other* approval path, nothing here noticed, because
    /// on this path it did match (issue #684). A test that borrows a shipped
    /// default tests the default, not the mechanism; the default is empty now
    /// and this list is the mechanism's own fixture.
    const FENCE: &[&str] = &["payment.send"];

    fn policy(mode: &str, cap: Option<f64>) -> Policy {
        Policy {
            mode: mode.to_string(),
            always_approve: FENCE.iter().map(|s| s.to_string()).collect(),
            auto_approve_under_usd: cap,
            approval_ttl_hours: None,
        }
    }

    fn effect(kind: &str, group: EffectGroup) -> Effect {
        Effect {
            kind: kind.to_string(),
            group,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        }
    }

    fn operator() -> Actor {
        Actor {
            kind: ActorKind::Operator,
            id: "owner".to_string(),
        }
    }

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    async fn decide(gate: &ManifestApprovalGate, effect: &Effect) -> PolicyDecision {
        gate.evaluate(&company(), effect).await.unwrap()
    }

    #[tokio::test]
    async fn disabled_policy_hitl_allows_legacy_parks_but_keeps_hard_denials() {
        let gate =
            ManifestApprovalGate::new(policy("supervised", None)).with_policy_hitl_disabled();
        assert_eq!(
            decide(&gate, &effect("payment.send", EffectGroup::Spend)).await,
            PolicyDecision::Allow
        );

        gate.set_emergency(true);
        assert_eq!(
            decide(&gate, &effect("payment.send", EffectGroup::Spend)).await,
            PolicyDecision::Deny
        );

        let readonly =
            ManifestApprovalGate::new(policy("readonly", None)).with_policy_hitl_disabled();
        assert_eq!(
            decide(&readonly, &effect("payment.send", EffectGroup::Spend)).await,
            PolicyDecision::Deny
        );
        assert_eq!(
            decide(&readonly, &effect("notification.post", EffectGroup::Other)).await,
            PolicyDecision::Deny
        );
        assert_eq!(
            decide(
                &readonly,
                &effect(
                    crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                    EffectGroup::Other
                )
            )
            .await,
            PolicyDecision::Allow
        );
    }

    #[tokio::test]
    async fn apply_effective_policy_updates_live_state_without_dropping_queue_or_emergency() {
        let gate = ManifestApprovalGate::new(policy("supervised", Some(5.0)));
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        gate.set_emergency(true);

        gate.apply_effective_policy(Policy {
            approval_ttl_hours: Some(2),
            ..policy("full", None)
        });

        assert_eq!(gate.parked_ids(), vec![id]);
        assert!(gate.is_emergency());
        assert_eq!(gate.ttl_millis(), 2 * 60 * 60 * 1000);
        assert_eq!(
            gate.policy(),
            Policy {
                approval_ttl_hours: Some(2),
                ..policy("full", None)
            }
        );
        assert_eq!(
            decide(&gate, &effect("misc.do", EffectGroup::Other)).await,
            PolicyDecision::Allow
        );
    }
    #[tokio::test]
    async fn readonly_gates_everything() {
        let gate = ManifestApprovalGate::new(policy("readonly", None));
        assert_eq!(
            decide(&gate, &effect("misc.read", EffectGroup::Other)).await,
            PolicyDecision::RequireApproval
        );
    }

    #[tokio::test]
    async fn full_allows_non_always_approve() {
        let gate = ManifestApprovalGate::new(policy("full", None));
        assert_eq!(
            decide(&gate, &effect("misc.do", EffectGroup::Other)).await,
            PolicyDecision::Allow
        );
    }

    #[tokio::test]
    async fn always_approve_overrides_full() {
        let gate = ManifestApprovalGate::new(policy("full", None));
        // `payment.send` is in FENCE, this module's always_approve fixture.
        assert_eq!(
            decide(&gate, &effect("payment.send", EffectGroup::Spend)).await,
            PolicyDecision::RequireApproval
        );
    }

    #[tokio::test]
    async fn supervised_spend_cap_is_strict() {
        let gate = ManifestApprovalGate::new(policy("supervised", Some(5.0)));
        let mut under = effect("x402.spend", EffectGroup::Spend);
        under.amount_usd = Some(4.99);
        assert_eq!(decide(&gate, &under).await, PolicyDecision::Allow);

        let mut at_cap = effect("x402.spend", EffectGroup::Spend);
        at_cap.amount_usd = Some(5.0);
        assert_eq!(
            decide(&gate, &at_cap).await,
            PolicyDecision::RequireApproval
        );

        // No cap configured → always parks.
        let gate_no_cap = ManifestApprovalGate::new(policy("supervised", None));
        assert_eq!(
            decide(&gate_no_cap, &under).await,
            PolicyDecision::RequireApproval
        );
    }

    #[tokio::test]
    async fn supervised_send_distinguishes_thread() {
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        let mut established = effect("email.send", EffectGroup::Send);
        established.established_thread = true;
        assert_eq!(decide(&gate, &established).await, PolicyDecision::Allow);

        let mut new_party = effect("email.send", EffectGroup::Send);
        new_party.first_time_counterparty = true;
        assert_eq!(
            decide(&gate, &new_party).await,
            PolicyDecision::RequireApproval
        );
    }

    #[tokio::test]
    async fn supervised_sign_publish_identity_always_park() {
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        for group in [
            EffectGroup::Sign,
            EffectGroup::Publish,
            EffectGroup::Identity,
        ] {
            assert_eq!(
                decide(&gate, &effect("some.effect", group)).await,
                PolicyDecision::RequireApproval
            );
        }
    }

    #[tokio::test]
    async fn supervised_hire_parks_first_time_or_over_cap() {
        let gate = ManifestApprovalGate::new(policy("supervised", Some(100.0)));
        let mut first = effect("a2a.engage", EffectGroup::Hire);
        first.first_time_counterparty = true;
        assert_eq!(decide(&gate, &first).await, PolicyDecision::RequireApproval);

        let mut over = effect("a2a.engage", EffectGroup::Hire);
        over.amount_usd = Some(150.0);
        assert_eq!(decide(&gate, &over).await, PolicyDecision::RequireApproval);

        let mut cheap = effect("a2a.engage", EffectGroup::Hire);
        cheap.amount_usd = Some(10.0);
        assert_eq!(decide(&gate, &cheap).await, PolicyDecision::Allow);
    }

    // -----------------------------------------------------------------------
    // The tier ladder (issue #1454)
    // -----------------------------------------------------------------------

    /// How permissive a decision is, so the ladder can be compared rather than
    /// spot-asserted.
    ///
    /// The order is the only thing here that is a judgement: a denied effect
    /// never happens, a parked one happens if a human says so, an allowed one
    /// happens. Nothing in between.
    fn permissiveness(decision: PolicyDecision) -> u8 {
        match decision {
            PolicyDecision::Deny => 0,
            PolicyDecision::RequireApproval => 1,
            PolicyDecision::Allow => 2,
        }
    }

    /// One effect per branch the checkpoint taxonomy actually takes, labelled in
    /// the operator's terms.
    ///
    /// Every kind is deliberately outside [`FENCE`], because `always_approve`
    /// is checked **above** the tier dispatch and wins over every tier — an
    /// entry in it would flatten the ladder to `RequireApproval` everywhere and
    /// make the monotonicity walk pass vacuously.
    ///
    /// Amounts are chosen against a $100 cap: `10.0` under it, `100.0` exactly
    /// at it (the strict-`<` boundary), `250.0` over it, and `None` for the
    /// unknown-amount branch.
    fn ladder_matrix() -> Vec<(&'static str, Effect)> {
        let mut cases: Vec<(&'static str, Effect)> = Vec::new();

        cases.push((
            "a consequence-free effect",
            effect("echo.noop", EffectGroup::Other),
        ));

        for (label, amount) in [
            ("a spend under the cap", Some(10.0)),
            ("a spend exactly at the cap", Some(100.0)),
            ("a spend over the cap", Some(250.0)),
            ("a spend of unknown amount", None),
        ] {
            let mut eff = effect("ladder.spend", EffectGroup::Spend);
            eff.amount_usd = amount;
            cases.push((label, eff));
        }

        for (label, established, first_time) in [
            ("a message on an established thread", true, false),
            ("a message to a first-time counterparty", false, true),
            ("a first message on an established thread", true, true),
            ("a message with no thread context", false, false),
        ] {
            let mut eff = effect("ladder.deliver", EffectGroup::Send);
            eff.established_thread = established;
            eff.first_time_counterparty = first_time;
            cases.push((label, eff));
        }

        for (label, group) in [
            ("a signature", EffectGroup::Sign),
            ("a publish", EffectGroup::Publish),
            ("an identity change", EffectGroup::Identity),
        ] {
            cases.push((label, effect("ladder.act", group)));
        }

        for (label, amount, first_time) in [
            (
                "an engagement of a known counterparty under the cap",
                Some(10.0),
                false,
            ),
            (
                "an engagement of a first-time counterparty",
                Some(10.0),
                true,
            ),
            ("an engagement exactly at the cap", Some(100.0), false),
            ("an engagement over the cap", Some(250.0), false),
            ("an engagement of unknown value", None, false),
        ] {
            let mut eff = effect("ladder.engage", EffectGroup::Hire);
            eff.amount_usd = amount;
            eff.first_time_counterparty = first_time;
            cases.push((label, eff));
        }

        cases
    }

    /// **The ladder itself, not one arm of it.**
    ///
    /// [`POLICY_MODES`](crate::company::POLICY_MODES) is ordered by increasing
    /// autonomy and the console renders it in that order under the promise that
    /// moving up interrupts you less. That promise is a property of the gate,
    /// and it is the property nothing checked: every existing test named a
    /// single mode, so `auto` could park strictly more than `supervised` for two
    /// releases with a green suite (issue #1454).
    ///
    /// Walked over `POLICY_MODES` rather than over a hard-coded list, so a fifth
    /// tier is covered the day it is added instead of the day someone remembers
    /// this file. Run under both a configured cap and no cap at all, because the
    /// no-cap branch of `Spend` and `Hire` is a different arm.
    #[tokio::test]
    async fn the_tier_ladder_is_monotonic() {
        let mut checked = 0;
        for cap in [None, Some(100.0)] {
            for (label, eff) in ladder_matrix() {
                let mut previous: Option<(&str, u8)> = None;
                for mode in crate::company::POLICY_MODES {
                    let gate = ManifestApprovalGate::new(policy(mode, cap));
                    let rank = permissiveness(decide(&gate, &eff).await);
                    if let Some((lower, lower_rank)) = previous {
                        assert!(
                            rank >= lower_rank,
                            "{label} (cap {cap:?}): `{mode}` is stricter than `{lower}`, \
                             which sits below it on the autonomy ladder — an operator \
                             moving up a tier to be interrupted less would be \
                             interrupted more"
                        );
                    }
                    previous = Some((mode, rank));
                    checked += 1;
                }
            }
        }
        assert_eq!(
            checked,
            2 * ladder_matrix().len() * crate::company::POLICY_MODES.len(),
            "the walk skipped a tier or a case"
        );
    }

    /// Every word in `POLICY_MODES` reaches a **named** arm.
    ///
    /// The failure this exists for is invisible to any decision-level assertion:
    /// a tier that fell into the fail-safe catch-all and a tier that genuinely
    /// decided to park return the same `PolicyDecision`. `auto` sat in that
    /// catch-all — it is in `POLICY_MODES`, it is `PROVISIONED_POLICY_MODE`, and
    /// the console offers it, so it was never an unknown mode; it just had no
    /// arm. Asking [`mode_decision`](ManifestApprovalGate::mode_decision) for
    /// `Some` is the only way to tell those apart.
    #[tokio::test]
    async fn every_policy_mode_has_a_named_arm() {
        let probe = effect("misc.do", EffectGroup::Other);

        let mut checked = 0;
        for mode in crate::company::POLICY_MODES {
            // A fresh snapshot whose mode is the probed word: `mode_decision`
            // takes the `Policy` explicitly, so the fail-safe arm stays
            // reachable without building a gate for every word.
            assert!(
                ManifestApprovalGate::mode_decision(&policy(mode, None), mode, &probe).is_some(),
                "`{mode}` is a selectable tier but falls into the fail-safe \
                 catch-all, so it silently behaves like `readonly`"
            );
            checked += 1;
        }
        assert_eq!(
            checked,
            crate::company::POLICY_MODES.len(),
            "the walk skipped a tier"
        );

        // The catch-all still exists, and still fails safe, for a word that is
        // not a tier at all.
        assert!(
            ManifestApprovalGate::mode_decision(&policy("moderately", None), "moderately", &probe)
                .is_none()
        );
        let unknown = ManifestApprovalGate::new(policy("moderately", None));
        assert_eq!(
            decide(&unknown, &probe).await,
            PolicyDecision::RequireApproval
        );
    }

    /// The reported bug: a company on `auto` parked `echo.noop`, an internal
    /// no-op that neither leaves the company nor spends money, while the tier
    /// below it did not.
    #[tokio::test]
    async fn auto_allows_a_consequence_free_effect() {
        let gate = ManifestApprovalGate::new(policy("auto", None));
        assert_eq!(
            decide(&gate, &effect("echo.noop", EffectGroup::Other)).await,
            PolicyDecision::Allow
        );
    }

    /// `auto` still stops at the line it advertises: anything that leaves the
    /// company or spends money.
    ///
    /// Asserted separately from the monotonicity walk on purpose — that walk
    /// would stay green if `auto` were widened all the way to `full`, since
    /// allowing more is monotonic. This is the other fence.
    #[tokio::test]
    async fn auto_still_parks_what_leaves_the_company() {
        let gate = ManifestApprovalGate::new(policy("auto", Some(100.0)));

        for group in [
            EffectGroup::Sign,
            EffectGroup::Publish,
            EffectGroup::Identity,
        ] {
            assert_eq!(
                decide(&gate, &effect("ladder.act", group)).await,
                PolicyDecision::RequireApproval,
                "{group:?} is irreversible and parks at every tier below `full`"
            );
        }

        let mut over_cap = effect("ladder.spend", EffectGroup::Spend);
        over_cap.amount_usd = Some(250.0);
        assert_eq!(
            decide(&gate, &over_cap).await,
            PolicyDecision::RequireApproval
        );

        let mut cold = effect("ladder.deliver", EffectGroup::Send);
        cold.first_time_counterparty = true;
        assert_eq!(decide(&gate, &cold).await, PolicyDecision::RequireApproval);
    }

    /// `auto` and `supervised` decide every native effect identically, and that
    /// is the *whole* of `auto`'s native behaviour rather than an accident of
    /// this fixture.
    ///
    /// The two tiers genuinely differ — but on the tool path, where `auto` waves
    /// through the `Standing::Grantable` sandbox writes `supervised` parks. The
    /// native taxonomy has no such bucket: everything it parks leaves the
    /// company or spends money, which is exactly `auto`'s stated stopping line.
    /// Pinned so a divergence has to be written down rather than drifted into,
    /// in either direction.
    #[tokio::test]
    async fn auto_matches_supervised_on_every_native_effect() {
        for cap in [None, Some(100.0)] {
            let supervised = ManifestApprovalGate::new(policy("supervised", cap));
            let auto = ManifestApprovalGate::new(policy("auto", cap));
            for (label, eff) in ladder_matrix() {
                assert_eq!(
                    decide(&auto, &eff).await,
                    decide(&supervised, &eff).await,
                    "{label} (cap {cap:?}) is decided differently by `auto` and \
                     `supervised`; if that is deliberate, say so on \
                     `evaluate_auto` and update this test"
                );
            }
        }
    }

    /// `always_approve` is checked above the tier dispatch, so it wins over
    /// `auto` exactly as it wins over `full`.
    #[tokio::test]
    async fn always_approve_overrides_auto() {
        let gate = ManifestApprovalGate::new(policy("auto", Some(100.0)));
        let mut small = effect("payment.send", EffectGroup::Spend);
        small.amount_usd = Some(1.0);
        assert_eq!(decide(&gate, &small).await, PolicyDecision::RequireApproval);
    }

    #[tokio::test]
    async fn park_then_approve_returns_effect() {
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        let eff = effect("filing.submit", EffectGroup::Sign);
        let id = gate.park(&company(), eff.clone()).await.unwrap();
        assert_eq!(gate.parked_ids().len(), 1);

        let resolved = gate
            .resolve(&id, Verdict::Approve, operator())
            .await
            .unwrap();
        assert_eq!(resolved, Some(eff));
        assert!(gate.parked_ids().is_empty());
    }

    #[tokio::test]
    async fn park_then_deny_returns_none() {
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        let resolved = gate.resolve(&id, Verdict::Deny, operator()).await.unwrap();
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn expired_approval_resolves_to_deny() {
        let gate = ManifestApprovalGate::new(policy("supervised", None)).with_ttl_millis(1000);
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        // Resolve far in the future: past the TTL → deny even for Approve.
        let future = now_millis() + 10_000;
        let resolved = gate.resolve_at(&id, Verdict::Approve, operator(), future);
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn resolve_amended_returns_amended_effect() {
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        let original = effect("filing.submit", EffectGroup::Sign);
        let id = gate.park(&company(), original.clone()).await.unwrap();

        // The operator peeks the original, edits its payload, and approves it.
        let parked = gate.parked_effect(&id).expect("parked effect readable");
        assert_eq!(parked, original);
        let mut amended = parked;
        amended.payload = serde_json::json!({ "edited": true });

        let resolved = gate.resolve_amended(&id, amended.clone(), operator(), now_millis());
        assert_eq!(resolved, Some(amended));
        // Resolving drains the queue.
        assert!(gate.parked_ids().is_empty());
    }

    #[tokio::test]
    async fn resolve_amended_expired_denies() {
        let gate = ManifestApprovalGate::new(policy("supervised", None)).with_ttl_millis(1000);
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        let mut amended = effect("filing.submit", EffectGroup::Sign);
        amended.payload = serde_json::json!({ "edited": true });

        // Past the TTL: the amend resolves to deny even though a payload was
        // supplied — default-deny-on-silence wins.
        let future = now_millis() + 10_000;
        let resolved = gate.resolve_amended(&id, amended, operator(), future);
        assert_eq!(resolved, None);
        assert!(gate.parked_ids().is_empty());
    }

    /// Issue #243: the four outcomes the port's `Option<Effect>` cannot tell
    /// apart. The important pair is `Denied` vs `NotParked` — the caller must
    /// journal and re-cycle the first and do nothing at all for the second.
    #[tokio::test]
    async fn resolve_outcome_distinguishes_all_four_results() {
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        let eff = effect("filing.submit", EffectGroup::Sign);

        // Unknown id.
        assert_eq!(
            gate.resolve_outcome(
                &ApprovalId::new("never-parked"),
                Verdict::Approve,
                operator(),
                now_millis()
            ),
            ResolveOutcome::NotParked
        );

        // Approved in time.
        let id = gate.park(&company(), eff.clone()).await.unwrap();
        assert_eq!(
            gate.resolve_outcome(&id, Verdict::Approve, operator(), now_millis()),
            ResolveOutcome::Approved(eff.clone())
        );
        // ...and resolving it a second time is NOT a deny, it is a no-op.
        assert_eq!(
            gate.resolve_outcome(&id, Verdict::Approve, operator(), now_millis()),
            ResolveOutcome::NotParked,
            "an already-resolved approval must not look like a fresh deny"
        );

        // Denied.
        let id = gate.park(&company(), eff.clone()).await.unwrap();
        assert_eq!(
            gate.resolve_outcome(&id, Verdict::Deny, operator(), now_millis()),
            ResolveOutcome::Denied(eff.clone()),
            "a deny must carry the parked effect so a standing denial can be \
             scoped to what the operator actually refused (issue #1458)"
        );

        // Expired: past the TTL, an approve still resolves to a default-deny,
        // and reports it as expiry rather than as the operator's choice.
        let short = ManifestApprovalGate::new(policy("supervised", None)).with_ttl_millis(1000);
        let id = short.park(&company(), eff).await.unwrap();
        assert_eq!(
            short.resolve_outcome(&id, Verdict::Approve, operator(), now_millis() + 10_000),
            ResolveOutcome::Expired
        );
        assert!(
            short.parked_ids().is_empty(),
            "expiry still drains the queue"
        );
    }

    /// Two operators (or one double-click, or a retried request) resolving the
    /// same approval concurrently: exactly one wins.
    ///
    /// The `remove` and the outcome decision share a critical section precisely
    /// so this cannot double-fire. A check-then-act caller — "is it parked? then
    /// resolve it" — would let both threads through and execute the approved
    /// effect twice; the at-most-once journal key would catch the *effect*, but
    /// the duplicate journal record and the duplicate follow-up cycle would
    /// still land.
    #[tokio::test]
    async fn concurrent_resolves_of_one_approval_yield_exactly_one_winner() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let gate = Arc::new(ManifestApprovalGate::new(policy("supervised", None)));
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();

        let approvals = Arc::new(AtomicUsize::new(0));
        let not_parked = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(8));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let gate = Arc::clone(&gate);
            let id = id.clone();
            let approvals = Arc::clone(&approvals);
            let not_parked = Arc::clone(&not_parked);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::task::spawn_blocking(move || {
                barrier.wait();
                match gate.resolve_outcome(&id, Verdict::Approve, operator(), now_millis()) {
                    ResolveOutcome::Approved(_) => approvals.fetch_add(1, Ordering::SeqCst),
                    ResolveOutcome::NotParked => not_parked.fetch_add(1, Ordering::SeqCst),
                    other => panic!("unexpected outcome {other:?}"),
                };
            }));
        }
        for t in tasks {
            t.await.expect("task");
        }

        assert_eq!(approvals.load(Ordering::SeqCst), 1, "exactly one winner");
        assert_eq!(not_parked.load(Ordering::SeqCst), 7, "the rest are no-ops");
        assert!(gate.parked_ids().is_empty());
    }

    /// Issue #351: the irreversibility question, answered by the same taxonomy
    /// that decides what parks — and answered the same way whatever mode the
    /// company runs in.
    ///
    /// The mode-independence is the part worth pinning. A `full`-mode company
    /// executes a filing without ever parking it, so the retry dialog is the
    /// only place anybody will ever be told it happened; a predicate that read
    /// the company's mode would say "nothing to warn about" in exactly the
    /// configuration that needs the warning most.
    #[test]
    fn irreversibility_follows_the_supervised_taxonomy_in_every_mode() {
        for mode in ["supervised", "full", "readonly"] {
            let gate = ManifestApprovalGate::new(policy(mode, Some(100.0)));

            for group in [
                EffectGroup::Sign,
                EffectGroup::Publish,
                EffectGroup::Identity,
            ] {
                assert!(
                    gate.is_irreversible(&effect("filing.submit", group)),
                    "{mode}: {group:?} is irreversible by construction",
                );
            }

            // Spend: the cap decides, strictly.
            let mut under = effect("x402.spend", EffectGroup::Spend);
            under.amount_usd = Some(99.0);
            assert!(!gate.is_irreversible(&under), "{mode}: under the cap");
            let mut at_cap = effect("x402.spend", EffectGroup::Spend);
            at_cap.amount_usd = Some(100.0);
            assert!(gate.is_irreversible(&at_cap), "{mode}: at the cap");

            // Send: first contact is the irreversible half.
            let mut established = effect("email.send", EffectGroup::Send);
            established.established_thread = true;
            assert!(!gate.is_irreversible(&established), "{mode}: a live thread");
            let mut cold = effect("email.send", EffectGroup::Send);
            cold.first_time_counterparty = true;
            assert!(gate.is_irreversible(&cold), "{mode}: first contact");

            // A read changes nothing and warns about nothing.
            assert!(
                !gate.is_irreversible(&effect("web.search", EffectGroup::Other)),
                "{mode}: an ordinary read must not raise a retry warning",
            );
        }
    }

    /// `always_approve` is a *parking* rule, not an irreversibility one, so it
    /// deliberately does not leak into the warning.
    ///
    /// Both live on the same policy block and it would be easy to fold them
    /// together. They answer different questions: `always_approve` is "ask me
    /// first", which an operator sets for anything they want a say in, while
    /// this dialog claims something cannot be taken back. Widening it would
    /// warn about routine work and teach people to click through.
    #[tokio::test]
    async fn always_approve_does_not_widen_what_counts_as_irreversible() {
        let gate = ManifestApprovalGate::new(policy("supervised", Some(100.0)));
        let mut cheap = effect("payment.send", EffectGroup::Spend);
        cheap.amount_usd = Some(1.0);
        // `payment.send` is in FENCE, this module's always_approve fixture, so it parks...
        assert_eq!(decide(&gate, &cheap).await, PolicyDecision::RequireApproval);
        // ...but a dollar under a hundred-dollar cap is not irreversible.
        assert!(!gate.is_irreversible(&cheap));
    }

    /// **T5 (issue #971).** The deadline binds even when nothing swept.
    ///
    /// The sweep is housekeeping — it retires an entry and tells the operator —
    /// but it is emphatically not the enforcement. `resolve_at` re-checks the
    /// TTL under the same lock that removes the entry, so a 25-hour-old
    /// approval default-denies on the operator's click whether or not any
    /// maintenance tick ever ran for that company. That property is what made
    /// the missing ticker a *visibility* bug rather than a safety one, and it
    /// has to survive the ticker landing: a future refactor that made the sweep
    /// the only expiry path would turn a paused host into one that honours
    /// week-old consent.
    #[tokio::test]
    async fn the_default_deadline_binds_at_resolve_with_no_sweep() {
        // No `with_ttl_millis`: the default the constant resolves to, so this
        // fails if the 24h default is quietly widened again.
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        assert_eq!(gate.ttl_millis(), 24 * 60 * 60 * 1000);
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        let twenty_five_hours = now_millis() + 25 * 60 * 60 * 1000;
        // Nothing swept: the entry is still parked right up to the click.
        assert_eq!(gate.parked_ids(), vec![id.clone()]);
        assert_eq!(
            gate.resolve_at(&id, Verdict::Approve, operator(), twenty_five_hours),
            None,
            "a 25h-old approval must default-deny even though no sweep ran"
        );
        // And an hour earlier it would still have been approvable, so the
        // assertion above is about the deadline and not about `resolve_at`
        // refusing everything.
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        let twenty_three_hours = now_millis() + 23 * 60 * 60 * 1000;
        assert!(
            gate.resolve_at(&id, Verdict::Approve, operator(), twenty_three_hours)
                .is_some()
        );
    }

    /// `[policy].approval_ttl_hours` is what the gate enforces, and an absent
    /// knob resolves to the default **here** rather than at parse (issue #971).
    #[tokio::test]
    async fn the_policy_knob_sets_the_deadline() {
        let configured = Policy {
            approval_ttl_hours: Some(2),
            ..policy("supervised", None)
        };
        let gate = ManifestApprovalGate::new(configured);
        assert_eq!(gate.ttl_millis(), 2 * 60 * 60 * 1000);

        let silent = policy("supervised", None);
        assert_eq!(
            silent.approval_ttl_hours, None,
            "a silent manifest must stay `None` through parse — see the field's note"
        );
        assert_eq!(
            ManifestApprovalGate::new(silent).ttl_millis(),
            DEFAULT_TTL_MILLIS
        );
    }

    /// The per-tick cap takes the **oldest** expired entries, not an arbitrary
    /// subset of them (issue #971).
    ///
    /// `parked` is a `HashMap`, so without the sort this passes or fails by
    /// process-random iteration order — and in production an entry could sit
    /// unretired across many ticks while newer ones drained ahead of it.
    #[tokio::test]
    async fn the_sweep_cap_drains_oldest_first() {
        let gate = ManifestApprovalGate::new(policy("supervised", None)).with_ttl_millis(0);
        let mut ids = Vec::new();
        for i in 0..5u64 {
            let id = gate
                .park(&company(), effect("filing.submit", EffectGroup::Sign))
                .await
                .unwrap();
            // Re-park under a known instant: `park` stamps `now`, and five
            // parks inside one millisecond would make "oldest" undecidable.
            gate.rehydrate(
                id.clone(),
                effect("filing.submit", EffectGroup::Sign),
                1_000 + i,
            );
            ids.push(id);
        }
        let first = gate.sweep_expired_capped(10_000, 2);
        assert_eq!(first, ids[..2].to_vec());
        let second = gate.sweep_expired_capped(10_000, 2);
        assert_eq!(second, ids[2..4].to_vec());
        // The uncapped form is the same sweep with no limit.
        assert_eq!(gate.sweep_expired(10_000), ids[4..].to_vec());
        assert!(gate.parked_ids().is_empty());
    }

    #[tokio::test]
    async fn sweep_expired_removes_stale_entries() {
        let gate = ManifestApprovalGate::new(policy("supervised", None)).with_ttl_millis(0);
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        // TTL 0 → everything is immediately expired at a strictly-later time.
        let expired = gate.sweep_expired(now_millis() + 1);
        assert_eq!(expired, vec![id]);
        assert!(gate.parked_ids().is_empty());
    }

    /// Issue #1805: extending re-anchors the TTL window, so an entry that was
    /// one tick from expiry survives the sweep that would have retired it and
    /// only expires on the fresh window.
    #[test]
    fn extend_keeps_a_near_expiry_entry_out_of_the_sweep() {
        let gate = ManifestApprovalGate::new(policy("supervised", None)).with_ttl_millis(1_000);
        let id = ApprovalId::from("appr-extend".to_string());
        gate.rehydrate(id.clone(), effect("filing.submit", EffectGroup::Sign), 0);
        // Just before the original deadline (0 + 1000) the operator extends it.
        assert!(gate.extend(&id, 900));
        // Past the ORIGINAL deadline the sweep now leaves it: its window runs
        // from 900, so 1500 - 900 = 600 < 1000.
        assert!(gate.sweep_expired(1_500).is_empty());
        assert_eq!(gate.parked_ids(), vec![id.clone()]);
        // It still expires, on the NEW window: 1901 - 900 = 1001 >= 1000.
        assert_eq!(gate.sweep_expired(1_901), vec![id]);
        // Extending an id that is not parked reports it rather than pretending.
        assert!(!gate.extend(&ApprovalId::from("ghost".to_string()), 0));
    }

    // -- Emergency stop (issue #86) -----------------------------------------

    /// Every side-effecting group is denied. Enumerated rather than sampled:
    /// this is a kill switch, and a group added to the taxonomy without being
    /// added here is a category of work that quietly keeps running.
    #[tokio::test]
    async fn emergency_denies_every_side_effecting_group() {
        // `full` mode — the most permissive policy there is. If the stop only
        // worked under `supervised` it would be useless exactly when it matters.
        let gate = ManifestApprovalGate::new(policy("full", None));
        gate.set_emergency(true);

        for group in [
            EffectGroup::Spend,
            EffectGroup::Send,
            EffectGroup::Sign,
            EffectGroup::Publish,
            EffectGroup::Hire,
            EffectGroup::Identity,
        ] {
            assert_eq!(
                decide(&gate, &effect("some.effect", group)).await,
                PolicyDecision::Deny,
                "{group:?} was not denied under emergency stop"
            );
        }
    }

    /// Chat survives, or the operator cannot ask what happened.
    #[tokio::test]
    async fn emergency_permits_other_so_chat_keeps_working() {
        let gate = ManifestApprovalGate::new(policy("full", None));
        gate.set_emergency(true);
        assert_eq!(
            decide(&gate, &effect("chat.reply", EffectGroup::Other)).await,
            PolicyDecision::Allow
        );
    }

    /// The stop outranks `always_approve`, which would otherwise park — and
    /// parking is a way *out* of the stop, since the operator could then approve
    /// the effect they just stopped without ever releasing the switch.
    #[tokio::test]
    async fn emergency_denies_rather_than_parking_an_always_approve_effect() {
        let gate = ManifestApprovalGate::new(policy("supervised", Some(100.0)));
        // Baseline: this kind parks under normal policy.
        assert_eq!(
            decide(&gate, &effect("payment.send", EffectGroup::Spend)).await,
            PolicyDecision::RequireApproval
        );
        gate.set_emergency(true);
        assert_eq!(
            decide(&gate, &effect("payment.send", EffectGroup::Spend)).await,
            PolicyDecision::Deny
        );
        // And nothing was parked as a side effect of being denied.
        assert!(gate.parked_ids().is_empty());
    }

    /// Parks *new* work without corrupting *in-flight* work: an approval already
    /// waiting on a person stays resolvable, and approving it still yields the
    /// effect to execute. Denying already-parked work instead would strand
    /// decisions the operator had already been asked for.
    #[tokio::test]
    async fn emergency_leaves_already_parked_approvals_resolvable() {
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        let parked = effect("filing.submit", EffectGroup::Sign);
        let id = gate.park(&company(), parked.clone()).await.unwrap();

        gate.set_emergency(true);

        // Still visible to the operator...
        assert_eq!(gate.parked_ids(), vec![id.clone()]);
        assert_eq!(gate.parked_effect(&id), Some(parked.clone()));
        // ...and still resolvable, yielding the effect to execute.
        let resolved = gate
            .resolve(&id, Verdict::Approve, operator())
            .await
            .unwrap();
        assert_eq!(resolved, Some(parked));
    }

    /// A denial for a parked approval is still a denial while stopped — the
    /// switch must not turn "deny" into "approve" by accident.
    #[tokio::test]
    async fn emergency_refuses_to_park_new_side_effecting_effects() {
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        gate.set_emergency(true);

        // A side-effecting effect cannot be queued for approval while stopped —
        // the harness approval route parks without consulting `evaluate`, so
        // without this veto a gated effect could be released after the stop.
        let err = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap_err();
        assert!(matches!(err, crate::OpenCompanyError::EmergencyStop(_)));
        assert!(gate.parked_ids().is_empty());

        // Chat still parks while stopped — the veto is the *same* one `evaluate`
        // applies, so an `EffectGroup::Other` effect is not caught by it.
        let id = gate
            .park(&company(), effect("chat.reply", EffectGroup::Other))
            .await
            .expect("an Other-group effect parks while stopped");
        assert_eq!(gate.parked_ids(), vec![id]);
    }

    /// A denial for a parked approval is still a denial while stopped — the
    /// switch must not turn "deny" into "approve" by accident.
    #[tokio::test]
    async fn emergency_does_not_change_a_denied_resolution() {
        let gate = ManifestApprovalGate::new(policy("supervised", None));
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        gate.set_emergency(true);
        let resolved = gate.resolve(&id, Verdict::Deny, operator()).await.unwrap();
        assert_eq!(resolved, None);
    }

    /// Releasing restores the *previous* policy exactly — the stop is a veto
    /// layered over evaluation, not a rewrite of it.
    #[tokio::test]
    async fn releasing_restores_normal_evaluation() {
        let gate = ManifestApprovalGate::new(policy("full", None));
        let spend = effect("payment.send", EffectGroup::Spend);
        // `payment.send` is in FENCE, this module's always_approve fixture, so `full` parks it.
        let before = decide(&gate, &spend).await;
        assert_eq!(before, PolicyDecision::RequireApproval);

        gate.set_emergency(true);
        assert_eq!(decide(&gate, &spend).await, PolicyDecision::Deny);

        gate.set_emergency(false);
        assert_eq!(decide(&gate, &spend).await, before);
    }

    /// **No implicit un-pause.** The TTL sweep expires parked *approvals*; it
    /// must never touch the switch. A kill switch with a timeout is a delay, and
    /// the failure mode is silent: work resumes at 3am with nobody watching.
    #[tokio::test]
    async fn emergency_does_not_decay_with_the_approval_ttl() {
        let gate = ManifestApprovalGate::new(policy("supervised", None)).with_ttl_millis(0);
        gate.park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        gate.set_emergency(true);

        // Sweep far past any TTL: every parked approval expires...
        let expired = gate.sweep_expired(now_millis() + DEFAULT_TTL_MILLIS * 1000);
        assert_eq!(expired.len(), 1);

        // ...and the stop is still engaged.
        assert!(gate.is_emergency());
        assert_eq!(
            decide(&gate, &effect("payment.send", EffectGroup::Spend)).await,
            PolicyDecision::Deny
        );
    }

    /// Boot replay: the *last* `EmergencyPauseChanged` decides, so a stop
    /// survives a restart and a release survives one too.
    ///
    /// This is the property the whole persistence design exists for — a kill
    /// switch that evaporates when the process restarts is not a kill switch,
    /// and a release that does not stick would strand a company nobody can
    /// restart.
    #[tokio::test]
    async fn replay_takes_the_last_emergency_event() {
        use crate::ports::EventLog;
        use crate::ports::types::CompanyEvent;
        use std::sync::Arc;

        let home = tempfile::Builder::new()
            .prefix("oc-emergency-replay-")
            .tempdir()
            .expect("tempdir");
        let events: Arc<dyn EventLog> = Arc::new(crate::store::FsEventLog::new(home.path()));
        let id = company();

        // A log with no such event was never stopped.
        assert!(!replayed_emergency(&events, &id).await.unwrap());

        let change = |engaged: bool| CompanyEvent::EmergencyPauseChanged {
            engaged,
            by: operator(),
            reason: None,
        };

        events.append(&id, change(true)).await.unwrap();
        assert!(replayed_emergency(&events, &id).await.unwrap());

        events.append(&id, change(false)).await.unwrap();
        assert!(!replayed_emergency(&events, &id).await.unwrap());

        // A second stop after a release wins again — the switch is not
        // one-shot, and "last write wins" must hold in both directions.
        events.append(&id, change(true)).await.unwrap();
        assert!(replayed_emergency(&events, &id).await.unwrap());
    }

    /// A fresh gate is not stopped. The boot path is the only caller that can
    /// tell "not stopped" from "could not find out", and it says so explicitly.
    #[tokio::test]
    async fn a_new_gate_is_not_stopped() {
        let gate = ManifestApprovalGate::new(policy("full", None));
        assert!(!gate.is_emergency());
        // A side-effecting group, so this would be `Deny` if the switch
        // defaulted engaged — but a kind outside `always_approve`, so under
        // `full` the undisturbed answer is `Allow` rather than a park.
        assert_eq!(
            decide(&gate, &effect("blog.post", EffectGroup::Publish)).await,
            PolicyDecision::Allow
        );
    }

    /// The live-policy update keeps the parked queue and the emergency switch
    /// while the evaluation snapshot and the derived deadline move (issue
    /// #1455). This is the property the boot and per-cycle refresh rely on: a
    /// console override lands on a gate that may already hold an approval the
    /// operator was asked about and a stop an operator pulled, and neither may
    /// be disturbed.
    #[tokio::test]
    async fn apply_effective_policy_moves_snapshot_and_ttl_but_not_parked_or_emergency() {
        let gate = ManifestApprovalGate::new(policy("full", None)).with_ttl_millis(1000);
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        gate.set_emergency(true);

        let stricter = Policy {
            mode: "supervised".to_string(),
            always_approve: FENCE.iter().map(|s| s.to_string()).collect(),
            auto_approve_under_usd: Some(5.0),
            approval_ttl_hours: Some(48),
        };
        gate.apply_effective_policy(stricter);

        // Parked queue and stop survive...
        assert_eq!(gate.parked_ids(), vec![id.clone()]);
        assert!(gate.is_emergency());
        // ...and the deadline moved.
        assert_eq!(gate.ttl_millis(), 48 * 60 * 60 * 1000);

        // With the stop released, evaluation reflects the new snapshot: `full`
        // waved every spend through, while the new `supervised` snapshot parks
        // one over the cap.
        gate.set_emergency(false);
        let mut over = effect("x402.spend", EffectGroup::Spend);
        over.amount_usd = Some(6.0);
        assert_eq!(decide(&gate, &over).await, PolicyDecision::RequireApproval);
    }

    /// The TTL-only update — what the ops handler applies immediately after a
    /// policy PUT/DELETE — moves the deadline without touching the snapshot, so
    /// an in-flight turn evaluating under the old tier is not disturbed.
    #[tokio::test]
    async fn apply_effective_ttl_moves_only_the_deadline() {
        let gate = ManifestApprovalGate::new(policy("full", None)).with_ttl_millis(1000);
        let id = gate
            .park(&company(), effect("filing.submit", EffectGroup::Sign))
            .await
            .unwrap();
        gate.set_emergency(true);

        let new_deadline = Policy {
            mode: "readonly".to_string(),
            always_approve: Vec::new(),
            auto_approve_under_usd: None,
            approval_ttl_hours: Some(1),
        };
        gate.apply_effective_ttl(&new_deadline);

        assert_eq!(gate.ttl_millis(), 60 * 60 * 1000);
        assert_eq!(gate.parked_ids(), vec![id]);
        assert!(gate.is_emergency());

        // Snapshot untouched: with the stop released, `blog.post` (Publish, not
        // in FENCE) still `Allow`s under `full`, which a `readonly` snapshot
        // would park.
        gate.set_emergency(false);
        assert_eq!(
            decide(&gate, &effect("blog.post", EffectGroup::Publish)).await,
            PolicyDecision::Allow
        );
    }
}
