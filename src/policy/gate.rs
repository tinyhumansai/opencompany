//! The manifest-`[policy]`-driven [`ApprovalGate`] implementation.
//!
//! Evaluation follows the precedence in
//! [`docs/spec/company-brain/approvals.md`](../../docs/spec/company-brain/approvals.md):
//!
//! 1. `never_do` hard-deny (Phase 1: the delegation-rule compiler is stubbed,
//!    so this list is always empty).
//! 2. `[policy].always_approve` effect kinds always park for approval.
//! 3. mode dispatch: `readonly` gates everything, `full` allows everything,
//!    `supervised` applies the checkpoint taxonomy by [`EffectGroup`].
//!
//! `evaluate` returns a bare [`PolicyDecision`]; the [`ApprovalId`] for a
//! `RequireApproval` outcome is minted separately by [`park`](ManifestApprovalGate::park).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::Result;
use crate::company::Policy;
use crate::ports::approvals::ApprovalGate;
use crate::ports::now_millis;
use crate::ports::types::{
    Actor, ApprovalId, CompanyId, Effect, EffectGroup, PolicyDecision, Verdict,
};

/// Default time-to-live for a parked approval: 7 days in milliseconds.
pub const DEFAULT_TTL_MILLIS: u64 = 7 * 24 * 60 * 60 * 1000;

/// A parked effect awaiting operator resolution.
#[derive(Clone, Debug)]
struct ParkedEffect {
    effect: Effect,
    parked_at_millis: u64,
}

/// The default [`ApprovalGate`]: evaluates effects against a company's
/// `[policy]` and holds the in-memory approval queue.
pub struct ManifestApprovalGate {
    policy: Policy,
    ttl_millis: u64,
    parked: Mutex<HashMap<ApprovalId, ParkedEffect>>,
}

impl ManifestApprovalGate {
    /// Builds a gate from a company's manifest `[policy]` block.
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            ttl_millis: DEFAULT_TTL_MILLIS,
            parked: Mutex::new(HashMap::new()),
        }
    }

    /// Overrides the parked-approval TTL (default [`DEFAULT_TTL_MILLIS`]).
    pub fn with_ttl_millis(mut self, ttl_millis: u64) -> Self {
        self.ttl_millis = ttl_millis;
        self
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

    /// Removes every parked approval older than the TTL relative to `now`,
    /// returning the ids that expired (they resolve to deny).
    pub fn sweep_expired(&self, now_millis: u64) -> Vec<ApprovalId> {
        let mut map = self.parked.lock().expect("parked map poisoned");
        let expired: Vec<ApprovalId> = map
            .iter()
            .filter(|(_, pe)| now_millis.saturating_sub(pe.parked_at_millis) >= self.ttl_millis)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            map.remove(id);
        }
        expired
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
        if now_millis.saturating_sub(parked.parked_at_millis) >= self.ttl_millis {
            return None;
        }
        match verdict {
            Verdict::Approve => Some(parked.effect),
            Verdict::Deny => None,
        }
    }

    /// The supervised-mode checkpoint taxonomy.
    fn evaluate_supervised(&self, effect: &Effect) -> PolicyDecision {
        let cap = self.policy.auto_approve_under_usd;
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
        // 1. `never_do` hard-deny — the delegation-rule compiler is a Phase-1
        //    stub, so this list is currently always empty.

        // 2. `always_approve` effect kinds park regardless of mode or amount.
        if self
            .policy
            .always_approve
            .iter()
            .any(|kind| kind == effect.kind())
        {
            return Ok(PolicyDecision::RequireApproval);
        }

        // 3. mode dispatch.
        let decision = match self.policy.mode.as_str() {
            "full" => PolicyDecision::Allow,
            "readonly" => PolicyDecision::RequireApproval,
            "supervised" => self.evaluate_supervised(effect),
            // Unknown modes fail safe: require approval.
            _ => PolicyDecision::RequireApproval,
        };
        Ok(decision)
    }

    async fn park(&self, _company: &CompanyId, effect: Effect) -> Result<ApprovalId> {
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

    fn policy(mode: &str, cap: Option<f64>) -> Policy {
        Policy {
            mode: mode.to_string(),
            always_approve: crate::company::DEFAULT_ALWAYS_APPROVE
                .iter()
                .map(|s| s.to_string())
                .collect(),
            auto_approve_under_usd: cap,
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
        // `payment.send` is in the default always_approve list.
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
}
