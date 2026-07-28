//! Steer: pause / cancel / redirect an in-flight board task or delegation from
//! the operator chat (issue #111, Epic #26).
//!
//! An operator watching a company work a task can now reach in mid-flight and
//! pause it, cancel it, or redirect it with a fresh instruction — without
//! waiting for the turn to finish or killing the whole company.
//!
//! ## Why this module is always compiled and openhuman-free
//!
//! Every type here is plain data + a `std::sync::Mutex`; nothing reaches into
//! `openhuman`. That keeps steering available to the **operator control plane**
//! (the REST routes + the runtime) in the default build, and — crucially — it
//! keeps the mechanism **structurally non-agent-injectable**: no agent tool
//! anywhere consumes a [`SteerControl`] or an [`InflightRegistry`], so an agent
//! can never steer (or cancel) another agent's work. Steering is honored only
//! between tool-loop iterations by the openhuman-gated
//! [`SteerStopHook`](crate::harness::steer::SteerStopHook), which polls the
//! shared control this module hands out.
//!
//! ## Shape
//!
//! * [`SteerControl`] — a one-shot, cheaply-cloned handle shared between the
//!   registry (operator side) and the stop hook (turn side). The hook polls
//!   [`requested`](SteerControl::requested) between iterations; the disposition
//!   site [`take`](SteerControl::take)s the pending action once the turn ends.
//!   Backed by a **std** `Mutex` held only for the length of a set/get — never
//!   across an `await`.
//! * [`InflightRegistry`] — `CompanyId → key → slot`, the live set of steerable
//!   runs. [`register`](InflightRegistry::register) returns a
//!   [`RegistrationGuard`] whose `Drop` deregisters the entry on **every** exit
//!   path (success, error, panic-unwind) — RAII so a crashed turn never leaves a
//!   ghost row in the operator strip.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::ports::types::CompanyId;

/// The maximum instruction length an operator redirect carries, in **chars**
/// (not bytes — truncation is codepoint-safe). A redirect longer than this is
/// truncated at the route boundary before it ever reaches an agent turn.
pub const MAX_REDIRECT_CHARS: usize = 2000;

/// What an operator asked us to do to an in-flight run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SteerAction {
    /// Stop the run and park the card in `paused`, preserving its partial work.
    Pause,
    /// Stop the run and drop its partial work, returning the card to `backlog`.
    Cancel,
    /// Stop the current attempt and re-run it with an appended operator
    /// instruction. Bounded per dispatch (see the harness disposition).
    Redirect {
        /// The operator's fresh instruction, already codepoint-capped to
        /// [`MAX_REDIRECT_CHARS`] at the route boundary.
        instruction: String,
    },
}

impl SteerAction {
    /// The stable lowercase wire word for this action (`pause` / `cancel` /
    /// `redirect`), used in the audit event, the pending-action badge, and the
    /// route body.
    pub fn as_str(&self) -> &'static str {
        match self {
            SteerAction::Pause => "pause",
            SteerAction::Cancel => "cancel",
            SteerAction::Redirect { .. } => "redirect",
        }
    }
}

/// A one-shot control handle shared between the operator-facing
/// [`InflightRegistry`] and the turn-side stop hook.
///
/// The registry [`request`](Self::request)s an action; the hook polls
/// [`requested`](Self::requested) between tool-loop iterations and stops the
/// turn when one pends; the disposition site [`take`](Self::take)s it once after
/// the turn ends. **Last request wins** — a second operator action before the
/// turn yields overwrites the first. Cheap to [`Clone`] (a shared handle).
#[derive(Clone, Default)]
pub struct SteerControl {
    inner: Arc<Mutex<Option<SteerAction>>>,
}

impl SteerControl {
    /// A fresh control with no action pending.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests a steer. Last write wins (a fresh action overwrites a pending
    /// one that the turn has not yet observed).
    pub fn request(&self, action: SteerAction) {
        *self.inner.lock().expect("steer control poisoned") = Some(action);
    }

    /// Whether an action is pending — the stop hook's fast-path poll.
    pub fn requested(&self) -> bool {
        self.inner.lock().expect("steer control poisoned").is_some()
    }

    /// Clones the pending action without clearing it.
    pub fn pending(&self) -> Option<SteerAction> {
        self.inner.lock().expect("steer control poisoned").clone()
    }

    /// Takes the pending action, clearing it. The disposition site's one-shot
    /// read after a turn ends.
    pub fn take(&self) -> Option<SteerAction> {
        self.inner.lock().expect("steer control poisoned").take()
    }
}

/// What kind of in-flight run an [`InflightEntry`] tracks. Drives which actions
/// the registry accepts: a `Task` supports all three; a `Delegation` is
/// **cancel-only** in v1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InflightKind {
    /// A dispatched board task (`in_progress`), steerable in full.
    Task,
    /// A desk delegation from the orchestrator's turn, cancel-only in v1.
    Delegation,
}

impl InflightKind {
    /// The stable lowercase wire word (`task` / `delegation`).
    pub fn as_str(&self) -> &'static str {
        match self {
            InflightKind::Task => "task",
            InflightKind::Delegation => "delegation",
        }
    }
}

/// One in-flight, steerable run surfaced to the operator's in-flight strip.
#[derive(Clone, Debug)]
pub struct InflightEntry {
    /// The steer key — the identifier the `POST …/tasks/{key}/steer` route
    /// addresses. For a board task it equals the task id; for a delegation it is
    /// a synthetic run id.
    pub key: String,
    /// The board task id, when this run is a dispatched card. `None` for a
    /// delegation (no card).
    pub task_id: Option<String>,
    /// Whether the run is a task or a delegation (gates the allowed actions).
    pub kind: InflightKind,
    /// A short human title for the strip (the card title, or the desk id).
    pub title: String,
    /// The roster agent running the turn.
    pub agent_id: String,
    /// When the run was registered (epoch millis), so the strip can show age.
    pub started_at_millis: u64,
    /// The last action requested against this run, as a wire word, so the strip
    /// can show a "pausing…/cancelling…/redirecting…" badge. `None` until an
    /// operator steers it.
    pub pending_action: Option<String>,
}

/// The reason a [`steer`](InflightRegistry::steer) call did not apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteerError {
    /// No run with that key is in flight for the company (the strip is stale, or
    /// the turn already finished).
    NotInFlight,
    /// The action is not supported for this run's kind (pause / redirect on a
    /// delegation, which is cancel-only in v1).
    Unsupported,
}

/// A live registry slot: the strip entry plus the control the turn polls.
struct Slot {
    entry: InflightEntry,
    control: SteerControl,
}

/// The live set of steerable runs, keyed `CompanyId → key → slot`.
///
/// Cheap to [`Clone`] (a shared handle) — the same underlying map is seen by the
/// [`HarnessBrain`](crate::harness::HarnessBrain) that registers runs, the
/// operator routes that steer them, and the runtime that lists them, because a
/// single registry is threaded through both [`HarnessDeps`] and
/// [`CompanyRuntime`] at build time.
///
/// [`HarnessDeps`]: crate::harness::HarnessDeps
/// [`CompanyRuntime`]: crate::company::runtime::CompanyRuntime
#[derive(Clone, Default)]
pub struct InflightRegistry {
    inner: Arc<Mutex<HashMap<CompanyId, HashMap<String, Slot>>>>,
}

impl InflightRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an in-flight run and returns its shared [`SteerControl`] plus a
    /// [`RegistrationGuard`] that deregisters the entry on drop.
    ///
    /// The caller runs the turn with the returned control's hook installed, then
    /// reads the control's pending action; when the guard drops (on **any** exit
    /// path) the strip row disappears.
    pub fn register(&self, company: &CompanyId, entry: InflightEntry) -> RegistrationGuard {
        let control = SteerControl::new();
        let key = entry.key.clone();
        {
            let mut guard = self.inner.lock().expect("inflight registry poisoned");
            guard.entry(company.clone()).or_default().insert(
                key.clone(),
                Slot {
                    entry,
                    control: control.clone(),
                },
            );
        }
        RegistrationGuard {
            registry: self.clone(),
            company: company.clone(),
            key,
            control,
        }
    }

    /// Removes a run's slot (called by [`RegistrationGuard`]'s `Drop`).
    fn deregister(&self, company: &CompanyId, key: &str) {
        let mut guard = self.inner.lock().expect("inflight registry poisoned");
        if let Some(runs) = guard.get_mut(company) {
            runs.remove(key);
            if runs.is_empty() {
                guard.remove(company);
            }
        }
    }

    /// The in-flight runs for a company, for the operator strip. Order is
    /// unspecified (the console sorts).
    pub fn list(&self, company: &CompanyId) -> Vec<InflightEntry> {
        self.inner
            .lock()
            .expect("inflight registry poisoned")
            .get(company)
            .map(|runs| runs.values().map(|slot| slot.entry.clone()).collect())
            .unwrap_or_default()
    }

    /// Applies an operator steer to an in-flight run.
    ///
    /// Errors:
    /// * [`SteerError::NotInFlight`] — no run with `key` is in flight.
    /// * [`SteerError::Unsupported`] — the action is not allowed for the run's
    ///   kind (pause / redirect on a delegation).
    ///
    /// On success the run's [`SteerControl`] is set (the turn stops at its next
    /// iteration boundary) and the strip's pending-action badge is updated.
    pub fn steer(
        &self,
        company: &CompanyId,
        key: &str,
        action: SteerAction,
    ) -> Result<(), SteerError> {
        let mut guard = self.inner.lock().expect("inflight registry poisoned");
        let slot = guard
            .get_mut(company)
            .and_then(|runs| runs.get_mut(key))
            .ok_or(SteerError::NotInFlight)?;
        if slot.entry.kind == InflightKind::Delegation && !matches!(action, SteerAction::Cancel) {
            return Err(SteerError::Unsupported);
        }
        slot.entry.pending_action = Some(action.as_str().to_string());
        slot.control.request(action);
        Ok(())
    }
}

/// An RAII guard that deregisters an in-flight run when dropped, and hands the
/// caller the run's shared [`SteerControl`].
///
/// Dropping this — whether the turn returned a reply, errored, or unwound —
/// removes the strip row, so the operator never sees a ghost run.
pub struct RegistrationGuard {
    registry: InflightRegistry,
    company: CompanyId,
    key: String,
    control: SteerControl,
}

impl RegistrationGuard {
    /// The shared control for this run: install its hook around the turn, then
    /// [`take`](SteerControl::take) the pending action after the turn ends.
    pub fn control(&self) -> &SteerControl {
        &self.control
    }
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        self.registry.deregister(&self.company, &self.key);
    }
}

/// Codepoint-safe truncation of an operator redirect instruction to
/// [`MAX_REDIRECT_CHARS`]. Never splits a multi-byte character (unlike a byte
/// slice), so a redirect ending in an emoji or accented letter can't panic.
pub fn cap_redirect(instruction: &str) -> String {
    instruction.chars().take(MAX_REDIRECT_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, kind: InflightKind) -> InflightEntry {
        InflightEntry {
            key: key.to_string(),
            task_id: matches!(kind, InflightKind::Task).then(|| key.to_string()),
            kind,
            title: "Ship the thing".to_string(),
            agent_id: "ceo".to_string(),
            started_at_millis: 0,
            pending_action: None,
        }
    }

    #[test]
    fn control_is_one_shot_and_last_write_wins() {
        let control = SteerControl::new();
        assert!(!control.requested());
        control.request(SteerAction::Pause);
        control.request(SteerAction::Cancel); // last write wins
        assert!(control.requested());
        assert_eq!(control.take(), Some(SteerAction::Cancel));
        assert!(!control.requested(), "take clears the action");
        assert_eq!(control.take(), None);
    }

    #[test]
    fn register_lists_then_guard_drop_deregisters() {
        let company = CompanyId::new("acme");
        let reg = InflightRegistry::new();
        {
            let _guard = reg.register(&company, entry("t1", InflightKind::Task));
            let listed = reg.list(&company);
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].key, "t1");
        }
        // Guard dropped → the row is gone, and the empty company map is pruned.
        assert!(reg.list(&company).is_empty());
    }

    #[test]
    fn steer_sets_the_control_and_pending_badge() {
        let company = CompanyId::new("acme");
        let reg = InflightRegistry::new();
        let guard = reg.register(&company, entry("t1", InflightKind::Task));
        reg.steer(&company, "t1", SteerAction::Pause)
            .expect("steers");
        assert!(guard.control().requested(), "the turn's control is set");
        assert_eq!(
            reg.list(&company)[0].pending_action.as_deref(),
            Some("pause"),
            "the strip shows a pending badge"
        );
    }

    #[test]
    fn steer_unknown_key_is_not_in_flight() {
        let company = CompanyId::new("acme");
        let reg = InflightRegistry::new();
        let err = reg
            .steer(&company, "ghost", SteerAction::Cancel)
            .expect_err("unknown key");
        assert_eq!(err, SteerError::NotInFlight);
    }

    #[test]
    fn delegation_is_cancel_only() {
        let company = CompanyId::new("acme");
        let reg = InflightRegistry::new();
        let _guard = reg.register(&company, entry("d1", InflightKind::Delegation));
        assert_eq!(
            reg.steer(&company, "d1", SteerAction::Pause),
            Err(SteerError::Unsupported)
        );
        assert_eq!(
            reg.steer(
                &company,
                "d1",
                SteerAction::Redirect {
                    instruction: "do X".into()
                }
            ),
            Err(SteerError::Unsupported)
        );
        // Cancel is allowed.
        reg.steer(&company, "d1", SteerAction::Cancel)
            .expect("cancel a delegation");
    }

    #[test]
    fn deregister_on_error_path_via_guard_drop() {
        // Simulate a turn that errors: the guard is created, then an early error
        // return drops it. The registry must be clean afterward (RAII).
        let company = CompanyId::new("acme");
        let reg = InflightRegistry::new();
        let run = || -> Result<(), &'static str> {
            let _guard = reg.register(&company, entry("t1", InflightKind::Task));
            Err("turn blew up")
        };
        assert!(run().is_err());
        assert!(
            reg.list(&company).is_empty(),
            "the guard deregistered even on the error path"
        );
    }

    #[test]
    fn cap_redirect_is_codepoint_safe() {
        let long: String = "é".repeat(MAX_REDIRECT_CHARS + 50);
        let capped = cap_redirect(&long);
        assert_eq!(capped.chars().count(), MAX_REDIRECT_CHARS);
        // Byte length is a clean multiple of 2 (é is 2 bytes) — no split codepoint.
        assert_eq!(capped.len(), MAX_REDIRECT_CHARS * 2);
    }
}
