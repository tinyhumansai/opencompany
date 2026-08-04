//! Single-use grants: what an operator's "approve" actually buys for a tool
//! call an agent was blocked from making (issue #243).
//!
//! ## Why a grant, and not just "run the effect"
//!
//! Two different things park on the same approval queue, and they need opposite
//! treatment on approval:
//!
//! * A **native** effect — an email the runtime sends, a workflow delivery, a
//!   Medulla effect frame. The runtime built it and the runtime can perform it,
//!   so approving it means executing it, at most once, keyed by approval id.
//! * A **harness tool call** — `composio_execute`, `workspace_write`,
//!   `media_generate_image`. openhuman's `ToolPolicy` is fail-closed: it blocked
//!   the call inside the agent's turn and fed the model a refusal. The
//!   opencompany [`Effect`](crate::ports::types::Effect) projected from it is a
//!   *description* of a tool call, not something the runtime knows how to
//!   perform — its payload is the tool's arguments. Executing it would ledger a
//!   spend and route a `{channel, text}` payload if one happened to be shaped
//!   like that, and otherwise do nothing at all. The real work is the tool, and
//!   only that agent can run it.
//!
//! So approving a harness call mints a **grant**: a one-shot permission slip
//! that lets exactly one future call — same agent, same tool, byte-identical
//! arguments — through the policy that blocked it, after which it is gone. The
//! agent is then re-dispatched with an instruction to re-issue the call.
//!
//! ## Why every part of that sentence is load-bearing
//!
//! * **Single-use.** Consuming removes the grant under the same lock that
//!   matched it, so a model that re-tries the tool in a loop gets exactly one
//!   execution out of one approval. Approving once must not mean "this tool is
//!   open now".
//! * **Agent-scoped.** A grant minted for `finance` does not let `marketing`
//!   through, even for the identical call. The operator approved a specific
//!   agent's request.
//! * **Exact arguments.** Matching is `serde_json::Value` equality on the whole
//!   argument object. A model that re-issues the call with a different
//!   recipient, a larger amount, or an extra field does not match, falls
//!   through, and re-parks — which is the honest outcome, because the operator
//!   never saw those arguments. Approve-with-edit is handled by minting against
//!   the *amended* arguments, so the operator's edit is what the grant admits.
//! * **TTL.** A grant the agent never redeems expires
//!   ([`GRANT_TTL_MILLIS`]) rather than sitting live forever. An approval is
//!   consent to an action now, not a standing authorisation; without this, a
//!   grant minted today would still fire if the same call surfaced next month.
//!
//! ## Durability
//!
//! The live set is in-memory, but its lifecycle is journaled
//! (`ApprovalGranted` / `GrantConsumed` / `GrantExpired`) and replayed on boot
//! via [`RuntimeJournal::replayed_grants`](crate::runtime::journal::RuntimeJournal::replayed_grants),
//! so a restart between "operator approved" and "agent re-issued" does not
//! silently drop the approval. Consumed and expired grants are folded *out* on
//! replay, so a restart can never resurrect one that already fired.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::ports::types::ApprovalId;

/// How long an unredeemed grant stays live: 15 minutes.
///
/// Sized to the gap between an operator hitting approve and the granting agent
/// finishing its re-dispatched turn — generous for a model turn, far short of
/// "still valid tomorrow". Expiry is not silent: the sweep tells the operator
/// the agent did not act, so a re-approval is an informed choice rather than a
/// mystery.
pub const GRANT_TTL_MILLIS: u64 = 15 * 60 * 1000;

/// One approved-but-not-yet-redeemed tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantedCall {
    /// The approval the operator resolved to mint this grant.
    pub approval_id: ApprovalId,
    /// The roster agent allowed to redeem it. Nobody else matches.
    pub agent: String,
    /// The tool the grant admits — the parked effect's `kind`.
    pub tool: String,
    /// The exact arguments admitted. Matching is whole-value equality.
    pub args: serde_json::Value,
    /// Epoch-millis the grant was minted, for TTL expiry.
    pub at_millis: u64,
}

/// The live grant set: a cheap shared handle, the same pattern as
/// [`ApprovalRequestQueue`](crate::harness::policy::ApprovalRequestQueue).
///
/// Cloning shares the state, so the policy installed on every roster agent, the
/// cycle runner that mints, and the sweep that expires all see one set.
#[derive(Clone, Default)]
pub struct GrantSet {
    inner: Arc<Mutex<GrantState>>,
}

#[derive(Default)]
struct GrantState {
    live: HashMap<ApprovalId, GrantedCall>,
    /// Grants consumed since the last [`GrantSet::drain_consumed`].
    ///
    /// Consumption happens deep inside a `ToolPolicy::check`, which is sync and
    /// has no journal handle, so the record cannot be written there. The id is
    /// buffered instead and the cycle runner drains it after the cycle it
    /// belongs to. A crash between consuming and draining loses the
    /// `GrantConsumed` record, which on replay re-arms a grant whose tool
    /// already ran — the same at-most-once trade the effect journal makes, and
    /// in the safe direction: the operator is asked again rather than the tool
    /// firing again unasked.
    consumed: Vec<ApprovalId>,
}

impl GrantSet {
    /// Mints a grant.
    pub fn grant(&self, call: GrantedCall) {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .live
            .insert(call.approval_id.clone(), call);
    }

    /// Redeems a grant for `(agent, tool, args)`, removing it.
    ///
    /// The match and the removal happen under one lock, so two concurrent turns
    /// racing the same grant cannot both be admitted — exactly one gets the
    /// `Some`. Returns `None` when nothing matches, which is the fall-through
    /// that re-parks.
    pub fn consume(
        &self,
        agent: &str,
        tool: &str,
        args: &serde_json::Value,
    ) -> Option<GrantedCall> {
        let mut state = self.inner.lock().expect("grant set poisoned");
        let id = state
            .live
            .iter()
            .find(|(_, g)| g.agent == agent && g.tool == tool && &g.args == args)
            .map(|(id, _)| id.clone())?;
        let call = state.live.remove(&id)?;
        state.consumed.push(id);
        Some(call)
    }

    /// Reads a live grant by approval id without redeeming it.
    ///
    /// This is how the brain recovers the arguments to tell the agent to
    /// re-issue: the grant must still be live when the instruction is written,
    /// and must still be live when the agent's tool call reaches the policy.
    pub fn peek(&self, id: &ApprovalId) -> Option<GrantedCall> {
        self.inner
            .lock()
            .expect("grant set poisoned")
            .live
            .get(id)
            .cloned()
    }

    /// Removes every grant minted more than `ttl_millis` before `now_millis`,
    /// returning them so the caller can journal and announce each expiry.
    pub fn sweep(&self, now_millis: u64, ttl_millis: u64) -> Vec<GrantedCall> {
        let mut state = self.inner.lock().expect("grant set poisoned");
        let expired: Vec<ApprovalId> = state
            .live
            .iter()
            .filter(|(_, g)| now_millis.saturating_sub(g.at_millis) >= ttl_millis)
            .map(|(id, _)| id.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|id| state.live.remove(&id))
            .collect()
    }

    /// Seeds the live set from a journal replay (boot recovery).
    pub fn rehydrate(&self, calls: impl IntoIterator<Item = GrantedCall>) {
        let mut state = self.inner.lock().expect("grant set poisoned");
        for call in calls {
            state.live.insert(call.approval_id.clone(), call);
        }
    }

    /// Takes the ids consumed since the last drain, so they can be journaled.
    pub fn drain_consumed(&self) -> Vec<ApprovalId> {
        std::mem::take(&mut self.inner.lock().expect("grant set poisoned").consumed)
    }

    /// How many grants are live (tests / observability).
    pub fn live_count(&self) -> usize {
        self.inner.lock().expect("grant set poisoned").live.len()
    }
}

impl std::fmt::Debug for GrantSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrantSet")
            .field("live", &self.live_count())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn call(id: &str, agent: &str, tool: &str, args: serde_json::Value) -> GrantedCall {
        GrantedCall {
            approval_id: ApprovalId::new(id),
            agent: agent.to_string(),
            tool: tool.to_string(),
            args,
            at_millis: 1_000,
        }
    }

    #[test]
    fn a_grant_is_redeemed_exactly_once() {
        let set = GrantSet::default();
        let args = serde_json::json!({ "to": "a@b.test" });
        set.grant(call("a1", "finance", "composio_execute", args.clone()));

        assert!(set.consume("finance", "composio_execute", &args).is_some());
        assert!(
            set.consume("finance", "composio_execute", &args).is_none(),
            "one approval buys one call, not an open door"
        );
        assert_eq!(set.live_count(), 0);
        assert_eq!(set.drain_consumed().len(), 1);
        assert!(set.drain_consumed().is_empty(), "the drain is a take");
    }

    #[test]
    fn a_grant_is_scoped_to_its_agent_and_its_exact_arguments() {
        let set = GrantSet::default();
        let args = serde_json::json!({ "to": "a@b.test", "body": "hi" });
        set.grant(call("a1", "finance", "composio_execute", args.clone()));

        // Another agent making the identical call is not who was approved.
        assert!(
            set.consume("marketing", "composio_execute", &args)
                .is_none()
        );
        // A different tool is not what was approved.
        assert!(set.consume("finance", "workspace_write", &args).is_none());
        // Different arguments are not what the operator saw.
        assert!(
            set.consume(
                "finance",
                "composio_execute",
                &serde_json::json!({ "to": "someone@else.test", "body": "hi" })
            )
            .is_none()
        );
        // An extra key is a different call too — matching is whole-value.
        assert!(
            set.consume(
                "finance",
                "composio_execute",
                &serde_json::json!({ "to": "a@b.test", "body": "hi", "cc": "x@y.test" })
            )
            .is_none()
        );
        // ...and none of those near-misses burned the grant.
        assert!(set.consume("finance", "composio_execute", &args).is_some());
    }

    #[test]
    fn peek_reads_without_redeeming() {
        let set = GrantSet::default();
        let args = serde_json::json!({ "q": 1 });
        set.grant(call("a1", "finance", "web_fetch", args.clone()));

        let seen = set.peek(&ApprovalId::new("a1")).expect("grant is live");
        assert_eq!(seen.tool, "web_fetch");
        assert_eq!(set.live_count(), 1, "peeking must not consume");
        assert!(set.peek(&ApprovalId::new("nope")).is_none());
    }

    #[test]
    fn sweep_expires_only_grants_past_the_ttl() {
        let set = GrantSet::default();
        set.grant(call("old", "finance", "t", serde_json::json!({})));
        let mut fresh = call("new", "finance", "t2", serde_json::json!({}));
        fresh.at_millis = 900_000;
        set.grant(fresh);

        let expired = set.sweep(1_000 + GRANT_TTL_MILLIS, GRANT_TTL_MILLIS);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].approval_id, ApprovalId::new("old"));
        assert_eq!(set.live_count(), 1, "the fresh grant survives");
    }

    #[test]
    fn rehydrate_seeds_the_live_set() {
        let set = GrantSet::default();
        set.rehydrate([
            call("a1", "finance", "t", serde_json::json!({})),
            call("a2", "legal", "t", serde_json::json!({})),
        ]);
        assert_eq!(set.live_count(), 2);
        assert!(set.peek(&ApprovalId::new("a2")).is_some());
    }

    /// Concurrent redemption of one grant: exactly one caller wins.
    ///
    /// The match and the removal are one critical section precisely so this
    /// cannot double-fire. A read-then-remove would let both threads see the
    /// grant and both proceed, turning one approval into two executions of a
    /// tool the operator approved once.
    #[test]
    fn two_threads_racing_one_grant_yield_exactly_one_winner() {
        let set = GrantSet::default();
        let args = serde_json::json!({ "amount_usd": 40.0 });
        set.grant(call("a1", "finance", "pay_invoice", args.clone()));

        let barrier = Arc::new(std::sync::Barrier::new(8));
        let winners = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let set = set.clone();
                let args = args.clone();
                let barrier = Arc::clone(&barrier);
                let winners = Arc::clone(&winners);
                std::thread::spawn(move || {
                    barrier.wait();
                    if set.consume("finance", "pay_invoice", &args).is_some() {
                        winners.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread");
        }

        assert_eq!(winners.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(set.live_count(), 0);
        assert_eq!(set.drain_consumed().len(), 1, "one consumption journaled");
    }
}
