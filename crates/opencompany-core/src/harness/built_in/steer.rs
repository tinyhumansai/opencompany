//! [`SteerStopHook`]: the openhuman [`StopHook`] that honors an operator steer
//! between tool-loop iterations (issue #111).
//!
//! OpenHuman's stop hooks fire **between** iterations of an agent's tool-call
//! loop — never mid-tool-call — and a [`StopDecision::Stop`] pauses the run
//! gracefully before the next provider call
//! ([`oh::agent::stop_hooks`]). This hook wraps one
//! [`SteerControl`](crate::company::steer::SteerControl): while no operator
//! action pends it returns [`Continue`](StopDecision::Continue); the moment the
//! operator pauses / cancels / redirects the run, the shared control flips and
//! the next `check` stops the turn. The disposition site then reads the action
//! off the same control and moves the card.
//!
//! One control is registered per steerable turn (task-local, installed via
//! [`with_stop_hooks`](oh::agent::stop_hooks::with_stop_hooks) around the turn),
//! so two concurrent turns never see each other's steer.
//!
//! Compiled only under `feature = "openhuman"` (the whole `harness` module is).

use async_trait::async_trait;

use openhuman_core as oh;

use oh::agent::stop_hooks::{StopDecision, StopHook, TurnState};

use crate::company::steer::SteerControl;

/// A [`StopHook`] that stops the turn as soon as its [`SteerControl`] carries a
/// pending operator action.
pub struct SteerStopHook {
    control: SteerControl,
}

impl SteerStopHook {
    /// Builds a hook over a turn's shared steer control.
    pub fn new(control: SteerControl) -> Self {
        Self { control }
    }
}

#[async_trait]
impl StopHook for SteerStopHook {
    fn name(&self) -> &str {
        "steer"
    }

    async fn check(&self, _ctx: &TurnState<'_>) -> StopDecision {
        if self.control.requested() {
            StopDecision::Stop {
                reason: "operator steer requested".to_string(),
            }
        } else {
            StopDecision::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `TurnState.cost` names `openhuman::agent::cost::TurnCost`, whose module is
    // crate-private to openhuman, so a `TurnState` cannot be constructed from
    // here — the hook's Continue/Stop decision is instead proven end-to-end by
    // the scripted-provider probe + the disposition matrix (see
    // `harness::mod::tests` and `harness::brain::tests`), which drive a real turn
    // through `with_stop_hooks`. This test pins the stable hook name.
    #[test]
    fn hook_has_a_stable_name() {
        let hook = SteerStopHook::new(SteerControl::new());
        assert_eq!(hook.name(), "steer");
    }

    /// The hook holds a *clone* of the control, and the whole mechanism rests
    /// on that clone being a handle onto shared state rather than a snapshot
    /// of it. The hook is built before the turn starts; every steer the
    /// operator sends arrives afterwards. A control that copied its flag at
    /// construction would return `Continue` for the life of the turn and the
    /// cancel would land on a run that never stopped.
    #[test]
    fn a_steer_that_arrives_after_the_hook_is_built_is_visible_to_it() {
        let control = SteerControl::new();
        let hook = SteerStopHook::new(control.clone());
        assert!(
            !hook.control.requested(),
            "nothing pends before the operator acts"
        );

        control.request(crate::company::steer::SteerAction::Cancel);

        assert!(
            hook.control.requested(),
            "a steer sent mid-turn must reach the hook that was built before it"
        );
    }

    /// `check` is polled between every tool-loop iteration, and the same
    /// pending action has to stop the turn on whichever poll comes next.
    /// `requested` must therefore read without consuming — `take` is the
    /// disposition site's one-shot, and if the poll shared it the first check
    /// after a steer would stop, and every later one would wave the turn on
    /// while the operator's action sat consumed and unactioned.
    #[test]
    fn a_pending_steer_survives_repeated_polls() {
        let control = SteerControl::new();
        let hook = SteerStopHook::new(control.clone());
        control.request(crate::company::steer::SteerAction::Pause);

        for poll in 0..4 {
            assert!(
                hook.control.requested(),
                "poll {poll} lost the pending steer"
            );
        }
        assert!(
            control.pending().is_some(),
            "the disposition site still has an action to read after the hook has polled"
        );
    }

    /// Two turns run concurrently, each with its own control. A steer aimed at
    /// one must not stop the other: the hook reads only the control it was
    /// built over, so nothing shared may leak the flag across turns.
    #[test]
    fn one_turns_steer_does_not_stop_another_turn() {
        let mine = SteerControl::new();
        let theirs = SteerControl::new();
        let my_hook = SteerStopHook::new(mine.clone());
        let their_hook = SteerStopHook::new(theirs.clone());

        mine.request(crate::company::steer::SteerAction::Cancel);

        assert!(my_hook.control.requested());
        assert!(
            !their_hook.control.requested(),
            "a steer on one turn stopped an unrelated turn"
        );
    }
}
