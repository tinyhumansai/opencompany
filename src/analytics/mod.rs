//! Product analytics: the [`Tracker`] port, its silent default, and the opt-in
//! Mixpanel transport.
//!
//! Issue #1739. The three decisions this module implements were taken before it
//! and are not re-litigated here; they are restated because every line below is
//! shaped by one of them.
//!
//! 1. **Hosted tenants only, by default.** This crate is GPL-3.0 and
//!    self-hostable. An open-source instance that phones home by default is a
//!    betrayal of that, so silence is the default and reporting is the
//!    exception. See [`Decision`].
//! 2. **Opaque identity only.** Events carry
//!    [`OpaqueId`] — the random instance id, or the digest of a tenant slug —
//!    never a company name.
//! 3. **Shape and outcome, never content.** Enforced by the type system: see
//!    [`types`], where [`PropValue`] has no `String` variant.
//!
//! # The shape of the module
//!
//! The repository's standing convention (`Cargo.toml`, `[features]`) is that a
//! port and its offline mock live in the **default** build and only the thin
//! real-network implementation is gated. That is followed exactly:
//!
//! | Always compiled | Behind `--features analytics` |
//! |---|---|
//! | [`Tracker`], [`Event`], [`Envelope`], [`NullTracker`], [`RecordingTracker`], [`TrackingUsageMeter`], the whole enable/disable decision, and the JSON body builder | `HttpMixpanelTracker` — one `reqwest` POST |
//!
//! So a default build **cannot** make an analytics request: the only type that
//! owns an HTTP client does not exist in it. That is not a policy the code
//! obeys, it is a type that is absent. It also keeps `tests/offline_e2e.rs`
//! honest — that lane runs inside a network namespace with no routes and
//! `docs/spec/runtime/offline.md` forbids widening it, so an analytics client
//! that fired at boot would turn the lane red and could not legitimately be
//! fixed by giving the namespace a route.
//!
//! # The desktop sends nothing, and needs no transport
//!
//! `src-tauri/tauri.conf.json` sets `connect-src 'self' ipc: http://ipc.localhost`,
//! so the desktop webview makes no outbound request at all — deliberately, and
//! documented in two places in the frontend. The desktop is
//! [`Deployment::Desktop`], which is silent, so nothing here asks for that CSP
//! to be widened and nothing here should ever be a reason to widen it.
//!
//! # Failure is silent
//!
//! Analytics never delays a turn, never surfaces an error to an operator, and
//! never prevents boot. [`Tracker::track`] is synchronous, infallible and
//! returns nothing: a call site cannot handle an analytics error because it is
//! not given one. A dead Mixpanel is a no-op.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

pub mod boot;
pub mod config;
pub mod meter;
pub mod mixpanel;
pub mod types;

pub use boot::install as install_analytics;
pub use config::{Decision, resolve};
pub use meter::TrackingUsageMeter;
pub use types::{
    BuildFlags, Envelope, Event, FailureCode, OpaqueId, Outcome, Prop, PropValue, Trigger,
};

#[cfg(doc)]
use crate::app::deployment::Deployment;

/// Where a batch of events is sent, and how they are named.
///
/// One `track` call records one event. It is **synchronous and infallible** on
/// purpose: the call sites are the cycle bracket and the usage meter, both on
/// the hot path of a turn, and neither may await a network or branch on a
/// telemetry failure.
#[async_trait]
pub trait Tracker: Send + Sync {
    /// Records one event. Never blocks, never fails, never surfaces anything.
    fn track(&self, event: Event);

    /// Sends anything buffered.
    ///
    /// Called at shutdown, after the server has drained. Also infallible: a
    /// flush that could fail would be a flush someone has to handle, at the one
    /// moment when there is nothing sensible to do about it.
    async fn flush(&self);

    /// Tells the tracker what cognition path this host turned out to be on, for
    /// the two cases where boot's answer stops being true: a hosted host
    /// provisioned into an empty registry (`server::provision`), and a company
    /// rebuilt in place after its first inference config (`runtime::rebuild`,
    /// issue #290), which moves it from `echo` to `harness`.
    ///
    /// A default no-op, so a tracker that carries no envelope — and every
    /// embedder that implements this port — is unaffected.
    fn observe_cognition(&self, _cognition: crate::ports::brain::Cognition) {}
}

/// The tracker that does nothing. **The default in every build**, and the only
/// tracker a desktop or self-hosted install has.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullTracker;

#[async_trait]
impl Tracker for NullTracker {
    fn track(&self, _event: Event) {}
    async fn flush(&self) {}
}

/// A shared [`NullTracker`], for a default field that wants no allocation
/// thought about at each site.
pub fn null_tracker() -> Arc<dyn Tracker> {
    Arc::new(NullTracker)
}

/// A tracker that keeps events in memory instead of sending them.
///
/// The offline mock the convention above asks for: it is what lets the whole
/// decision — which builds report, which stay silent, and what a payload may
/// contain — be tested at default features, with no network and no feature flag.
#[derive(Debug, Default)]
pub struct RecordingTracker {
    events: Mutex<Vec<Event>>,
    flushes: Mutex<usize>,
    cognition: Mutex<Option<crate::ports::brain::Cognition>>,
}

impl RecordingTracker {
    /// A fresh recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything recorded so far, in order.
    pub fn events(&self) -> Vec<Event> {
        self.events.lock().expect("recording tracker").clone()
    }

    /// How many times [`Tracker::flush`] has been called.
    pub fn flushes(&self) -> usize {
        *self.flushes.lock().expect("recording tracker")
    }

    /// The last cognition observed, if any.
    pub fn observed_cognition(&self) -> Option<crate::ports::brain::Cognition> {
        *self.cognition.lock().expect("recording tracker")
    }
}

#[async_trait]
impl Tracker for RecordingTracker {
    fn track(&self, event: Event) {
        self.events.lock().expect("recording tracker").push(event);
    }

    async fn flush(&self) {
        *self.flushes.lock().expect("recording tracker") += 1;
    }

    fn observe_cognition(&self, cognition: crate::ports::brain::Cognition) {
        *self.cognition.lock().expect("recording tracker") = Some(cognition);
    }
}

/// A tracker whose real destination is chosen after it has already been handed
/// out.
///
/// Boot has an ordering problem this solves and nothing else does. Every
/// company runtime needs its tracker at **build** time — the usage-meter
/// wrapper is baked into the ports struct there — but the context envelope
/// needs [`Cognition`](crate::ports::brain::Cognition), which is a property of
/// the brain a runtime *builds*. The alternatives were both worse: re-deriving
/// which brain the host will pick, from config, beside the code that actually
/// picks it (a second answer that drifts the first time either side changes),
/// or shipping an envelope that reports `custom`/`unknown` cognition, which is
/// not missing data but wrong data.
///
/// So the handle is handed out first and [`install`](Self::install)ed once, the
/// moment a runtime exists to ask.
///
/// # Events tracked before that are held, not dropped
///
/// They used to be dropped, on the reasoning that the pre-install window
/// contains nothing because the first company has not run a turn yet. **That
/// reasoning is wrong**, and PR #1751's review found the counterexample:
/// `CompanyScheduler::spawn` runs its restart catch-up (issue #241)
/// immediately, so a company with a cron occurrence missed during downtime
/// fires a real cycle the instant it is registered — before boot has a runtime
/// to read cognition from. Its `turn_finished` and `turn_metered` events fell
/// into the window, which undercounted exactly the scheduled work a restart
/// produces.
///
/// Fixing the boot ordering would have fixed that one call site. Holding the
/// events fixes the class: the mailbox poller, the workflow scheduler and
/// anything wired here later are all covered, and none of them has to know
/// where in `main` it sits. The buffer is bounded by [`MAX_PENDING`] because a
/// handle that is never installed must not grow without limit — the same
/// trade the transport's own buffer makes, and for the same reason.
///
/// `OnceLock` for the installed tracker, so the hot path stays lock-free:
/// `track` reads it first and takes the mutex **only** while nothing is
/// installed, which after boot is never. "Set exactly once, at boot" is
/// precisely what a `OnceLock` promises.
#[derive(Default)]
pub struct DeferredTracker {
    inner: std::sync::OnceLock<Arc<dyn Tracker>>,
    pending: Mutex<Vec<Event>>,
}

/// The most events held before installation before the oldest are dropped.
///
/// Generous for the window it covers — boot — and small enough that a handle
/// nobody ever installs costs nothing that matters.
const MAX_PENDING: usize = 256;

impl DeferredTracker {
    /// A handle with nothing behind it yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs the real tracker and replays anything tracked before now, in
    /// order. Returns `false` if one was already installed, in which case
    /// `tracker` is discarded — a second install would silently split a
    /// process's events across two destinations.
    pub fn install(&self, tracker: Arc<dyn Tracker>) -> bool {
        // The lock is held across the `set` **and** the drain so a `track`
        // racing this cannot read "not installed", be descheduled past the
        // drain, and then push onto a buffer nothing will ever read again.
        // `track` re-checks under the same lock; see below.
        let mut pending = self.pending.lock().expect("deferred tracker");
        if self.inner.set(tracker).is_err() {
            return false;
        }
        let inner = self.inner.get().expect("just installed");
        for event in std::mem::take(&mut *pending) {
            inner.track(event);
        }
        true
    }
}

impl std::fmt::Debug for DeferredTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.inner.get().is_some() {
            return f.write_str("DeferredTracker(installed)");
        }
        // The held count, because "why did boot report nothing?" is answered by
        // whether anything is waiting. Never the events themselves.
        let held = self.pending.lock().map(|p| p.len()).unwrap_or(0);
        write!(f, "DeferredTracker(pending, {held} held)")
    }
}

#[async_trait]
impl Tracker for DeferredTracker {
    fn track(&self, event: Event) {
        // The hot path: installed, no lock, one virtual call.
        if let Some(inner) = self.inner.get() {
            inner.track(event);
            return;
        }
        let mut pending = self.pending.lock().expect("deferred tracker");
        // Re-checked under the lock, which `install` also holds while it sets
        // and drains. Without this, an event could be pushed onto a buffer that
        // has already been drained and never be seen again.
        if let Some(inner) = self.inner.get() {
            drop(pending);
            inner.track(event);
            return;
        }
        if pending.len() >= MAX_PENDING {
            pending.remove(0);
        }
        pending.push(event);
    }

    async fn flush(&self) {
        if let Some(inner) = self.inner.get() {
            inner.flush().await;
        }
    }

    /// Forwarded, not held. An observation before installation is redundant:
    /// `boot::install` reads the registry itself at the moment it builds the
    /// envelope, so the tracker starts with the newest answer anyway.
    fn observe_cognition(&self, cognition: crate::ports::brain::Cognition) {
        if let Some(inner) = self.inner.get() {
            inner.observe_cognition(cognition);
        }
    }
}

/// Renders one event as the flat property map a JSON transport sends.
///
/// Un-gated deliberately. The body builder is where a leak would actually
/// happen, so it must be testable in the build that every lane runs, not only
/// in the one lane that compiles the network client.
///
/// The identity goes in as `distinct_id`; everything else comes from
/// [`Envelope::props`] and [`Event::props`], which yield [`PropValue`]s and can
/// therefore hold nothing but literals, counts, quantities and flags.
pub fn payload(envelope: &Envelope, event: &Event) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "distinct_id".to_string(),
        serde_json::Value::from(envelope.id.as_str()),
    );
    for (key, value) in envelope.props() {
        properties.insert(key.to_string(), value.to_json());
    }
    for (key, value) in event.props() {
        properties.insert(key.to_string(), value.to_json());
    }
    serde_json::json!({
        "event": event.name(),
        "properties": serde_json::Value::Object(properties),
    })
}

#[cfg(test)]
mod test;
