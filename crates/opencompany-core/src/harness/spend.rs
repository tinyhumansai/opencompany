//! [`SpendStopHook`]: the openhuman [`StopHook`] that makes an in-turn budget
//! halt **observable** to this crate (issue #1032).
//!
//! #988 armed the in-turn spend brake by pushing the vendored
//! [`BudgetStopHook`] onto the turn's hook list. It works — a turn that outruns
//! its teammate's declared `budget_usd_daily` stops before the next provider
//! call — but the fact that it stopped is destroyed at the boundary:
//!
//! * the hook's [`StopDecision::Stop { reason }`](StopDecision::Stop) is
//!   consumed inside openhuman's tool loop, which simply stops iterating and
//!   returns the run's text as an ordinary `Ok(reply)`;
//! * [`with_stop_hooks`](oh::agent::stop_hooks::with_stop_hooks) returns only
//!   the future's value — the hook list rides a `tokio::task_local` and nothing
//!   reads back out of it;
//! * and [`Agent::last_turn_hit_cap`](oh::agent::Agent::last_turn_hit_cap) is
//!   `false`, because a hook-driven halt pauses *below* `max_tool_iterations`
//!   so the iteration-cap predicate does not hold. #988 pins that distinction
//!   deliberately, which is exactly why the #926 flag cannot be reused here.
//!
//! So the halt has to be captured **in the hook itself**, on the way past. This
//! hook wraps the vendored one, delegates the decision to it unchanged, and
//! flips a shared [`AtomicBool`] when the delegate answers `Stop`.
//!
//! ## Why it wraps rather than reimplements
//!
//! The predicate stays upstream's — including its fail-closed handling of a
//! malformed cap, which turns a NaN or non-positive `max_usd` into an immediate
//! stop rather than a silently disabled guard. Reimplementing it here would
//! leave two copies of "when does a turn run out of money" free to disagree the
//! next time the vendored crate moves, and the copy this crate renders to the
//! operator would be the one that drifted.
//!
//! ## Why an `AtomicBool` and not the reason string
//!
//! The reason openhuman formats (`"turn cost $X reached cap $Y"`) is a
//! developer-facing trace line, not an operator-facing sentence, and its shape
//! is upstream's to change. The figures it quotes are already to hand outside
//! the hook — the cap from
//! [`turn_spend_cap_usd`](crate::harness::CompanyAgent::turn_spend_cap_usd) and
//! the spend from the [`TurnUsage`](crate::harness::cost::TurnUsage) totals the
//! turn already returns — so the flag carries the one bit that cannot be
//! recovered afterwards, and the notice is composed from figures this crate
//! owns.
//!
//! Compiled only under `feature = "openhuman"` (the whole `harness` module is).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use openhuman_core as oh;

use oh::agent::stop_hooks::{BudgetStopHook, StopDecision, StopHook, TurnState};

/// A [`StopHook`] that delegates to the vendored [`BudgetStopHook`] and records
/// that it fired.
///
/// The recording is one-way: the flag is only ever set, never cleared, because
/// one hook instance is built per turn and a turn that halted stays halted.
pub struct SpendStopHook {
    /// The vendored predicate, unchanged. `BudgetStopHook` is
    /// `#[derive(Debug, Clone, Copy)]`, so it composes by value.
    inner: BudgetStopHook,
    /// Set when `inner` answers [`StopDecision::Stop`]. Shared with the turn
    /// that installed the hook, which reads it once the turn has returned.
    halted: Arc<AtomicBool>,
}

impl SpendStopHook {
    /// Builds a hook that halts the turn at `cap_usd`, exactly as the vendored
    /// hook would on its own.
    pub fn new(cap_usd: f64) -> Self {
        Self {
            inner: BudgetStopHook::new(cap_usd),
            halted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The shared flag this hook sets when it halts a turn.
    ///
    /// Take this **before** the hook is boxed into the task-local list; once it
    /// is an `Arc<dyn StopHook>` there is no way back to the concrete type.
    pub fn halted(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.halted)
    }
}

#[async_trait]
impl StopHook for SpendStopHook {
    /// The delegate's name, not a new one: this hook changes nothing about when
    /// a turn stops, and openhuman's own trace lines name the hook that stopped
    /// it. A second spelling would make the same halt look like two.
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn check(&self, ctx: &TurnState<'_>) -> StopDecision {
        let decision = self.inner.check(ctx).await;
        if matches!(decision, StopDecision::Stop { .. }) {
            self.halted.store(true, Ordering::SeqCst);
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // As `SteerStopHook`'s own test records: `TurnState.cost` names
    // `openhuman::agent::cost::TurnCost`, whose module is crate-private to
    // openhuman, so a `TurnState` cannot be constructed from here and the
    // hook's Continue/Stop decision cannot be unit-tested. It is proven
    // end-to-end instead, by `harness::spend_halt_turn_test`, which drives a
    // real turn through `with_stop_hooks` against a scripted provider.
    //
    // What is testable here is the wiring either side of that decision: the
    // name the delegate lends it, and that the flag starts clear and is shared
    // rather than copied.
    #[test]
    fn the_hook_borrows_the_delegates_name() {
        let hook = SpendStopHook::new(1.0);
        assert_eq!(
            hook.name(),
            BudgetStopHook::new(1.0).name(),
            "a second spelling would make one halt look like two in the trace"
        );
    }

    #[test]
    fn the_flag_starts_clear_and_is_shared_not_copied() {
        let hook = SpendStopHook::new(1.0);
        let flag = hook.halted();
        assert!(
            !flag.load(Ordering::SeqCst),
            "a turn that has not run cannot have been halted"
        );
        // The handle the turn keeps must observe what the hook stores, or the
        // halt is recorded into a copy nobody reads — the exact failure this
        // module exists to prevent.
        hook.halted.store(true, Ordering::SeqCst);
        assert!(flag.load(Ordering::SeqCst));
    }
}
