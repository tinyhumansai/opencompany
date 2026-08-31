//! [`TrackingUsageMeter`]: the seam that turns every metered sample into a
//! `turn_metered` event.
//!
//! # Why here and not at the cost hook
//!
//! The obvious place to read token counts is the harness cost hook
//! (`harness::built_in::cost::record_turn_cost`), which has the agent, the
//! provider, the run id and the totals in scope at once. It is also
//! `openhuman`-gated, and the *cycle*-level metering path deliberately reports
//! **zero** tokens on that build so the harness's per-turn accounting is not
//! double-counted (`ports::brain::UsageMetering`). So an event written at either
//! one of those two places is right for one build and blind on the other.
//!
//! [`UsageMeter::record`](crate::ports::UsageMeter::record) is where both paths
//! meet. Every `metering::record_*` function ends here, on every build, whether
//! the sample came from a per-turn harness hook, a per-cycle hosted brain, an
//! OAuth tool call or a search. Wrapping the port therefore instruments all of
//! them and changes not one call site — and the decorator shape is one this
//! tree already uses for exactly this reason (`EventingRunStore`,
//! `WorkspaceAnnouncer`, `QuotaEnforcedWorkspace`).
//!
//! # It never changes what is stored
//!
//! The inner meter's result is returned untouched, and the event is emitted
//! **only after** the inner write succeeds — a sample that failed to persist is
//! not usage that happened. A tracking wrapper that could fail a write, or
//! report one that did not land, would be worse than no instrumentation.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::analytics::{Event, Tracker};
use crate::ports::types::CompanyId;
use crate::ports::usage::{UsageMeter, UsageSample};

/// A [`UsageMeter`] that reports each recorded sample to a [`Tracker`].
pub struct TrackingUsageMeter {
    inner: Arc<dyn UsageMeter>,
    tracker: Arc<dyn Tracker>,
}

impl TrackingUsageMeter {
    /// Wraps `inner`.
    pub fn new(inner: Arc<dyn UsageMeter>, tracker: Arc<dyn Tracker>) -> Self {
        Self { inner, tracker }
    }
}

impl std::fmt::Debug for TrackingUsageMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TrackingUsageMeter")
    }
}

#[async_trait]
impl UsageMeter for TrackingUsageMeter {
    async fn record(&self, company: &CompanyId, sample: &UsageSample) -> Result<()> {
        let outcome = self.inner.record(company, sample).await;
        if outcome.is_ok() {
            // `Event::metered` is the only constructor for this event, and it
            // folds the sample's two free-form fields away: the agent name is
            // dropped entirely and the provider goes through the closed
            // vocabulary. The company id never appears — attribution is the
            // envelope's opaque instance id and nothing finer.
            self.tracker.track(Event::metered(sample));
        }
        outcome
    }

    async fn query(&self, company: &CompanyId, since_millis: u64) -> Result<Vec<UsageSample>> {
        self.inner.query(company, since_millis).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::analytics::RecordingTracker;
    use crate::ports::usage::SampleKind;
    use std::sync::Mutex;

    #[derive(Default)]
    struct InMemory {
        rows: Mutex<Vec<UsageSample>>,
    }

    #[async_trait]
    impl UsageMeter for InMemory {
        async fn record(&self, _company: &CompanyId, sample: &UsageSample) -> Result<()> {
            self.rows.lock().unwrap().push(sample.clone());
            Ok(())
        }
        async fn query(&self, _company: &CompanyId, _since: u64) -> Result<Vec<UsageSample>> {
            Ok(self.rows.lock().unwrap().clone())
        }
    }

    struct Failing;

    #[async_trait]
    impl UsageMeter for Failing {
        async fn record(&self, _company: &CompanyId, _sample: &UsageSample) -> Result<()> {
            Err(crate::OpenCompanyError::Store("nope".into()))
        }
        async fn query(&self, _company: &CompanyId, _since: u64) -> Result<Vec<UsageSample>> {
            Ok(Vec::new())
        }
    }

    fn sample() -> UsageSample {
        UsageSample {
            at_millis: 1,
            agent: "maya".into(),
            provider: "openrouter".into(),
            input_tokens: 10,
            output_tokens: 4,
            cached_input_tokens: 2,
            cost_usd: 0.5,
            kind: SampleKind::Inference,
            run_id: Some("run-1".into()),
            model: None,
        }
    }

    #[tokio::test]
    async fn a_recorded_sample_is_reported_and_still_stored() {
        let inner = Arc::new(InMemory::default());
        let tracker = Arc::new(RecordingTracker::new());
        let meter = TrackingUsageMeter::new(inner.clone(), tracker.clone());

        meter
            .record(&CompanyId::new("acme"), &sample())
            .await
            .unwrap();

        assert_eq!(inner.rows.lock().unwrap().len(), 1, "the write still lands");
        assert_eq!(
            tracker.events(),
            vec![Event::TurnMetered {
                kind: "inference",
                provider: "openrouter",
                model: None,
                input_tokens: 10,
                output_tokens: 4,
                cached_input_tokens: 2,
                cost_usd: 0.5,
                attributed_to_run: true,
            }]
        );
    }

    /// The model the sample was classified against reaches the event. This is
    /// the seam #1749 stops at: it puts a [`ModelSlug`] on every sample, and
    /// without this forwarding the fleet-wide "what is the spend going to?"
    /// question is answerable from a company's own meter and from nowhere else.
    ///
    /// [`ModelSlug`]: crate::metering::ModelSlug
    #[tokio::test]
    async fn a_sample_model_reaches_the_event() {
        let inner = Arc::new(InMemory::default());
        let tracker = Arc::new(RecordingTracker::new());
        let meter = TrackingUsageMeter::new(inner, tracker.clone());

        let mut with_model = sample();
        with_model.model = Some(crate::metering::ModelSlug::classify(
            "anthropic/claude-sonnet-4-6",
        ));
        meter
            .record(&CompanyId::new("acme"), &with_model)
            .await
            .unwrap();

        assert_eq!(
            tracker.events(),
            vec![Event::TurnMetered {
                kind: "inference",
                provider: "openrouter",
                model: Some("anthropic-sonnet"),
                input_tokens: 10,
                output_tokens: 4,
                cached_input_tokens: 2,
                cost_usd: 0.5,
                attributed_to_run: true,
            }]
        );
    }

    /// A sample that did not persist is not usage that happened, so it is not
    /// reported. Without this the metered counts would drift above the meter's
    /// own rows on any store fault.
    #[tokio::test]
    async fn a_failed_write_reports_nothing() {
        let tracker = Arc::new(RecordingTracker::new());
        let meter = TrackingUsageMeter::new(Arc::new(Failing), tracker.clone());

        assert!(
            meter
                .record(&CompanyId::new("acme"), &sample())
                .await
                .is_err()
        );
        assert!(tracker.events().is_empty());
    }

    /// The point of the wrapper, stated as a test: the agent name and the raw
    /// provider are in the sample and reach nothing.
    #[tokio::test]
    async fn the_agent_name_never_reaches_the_event() {
        let inner = Arc::new(InMemory::default());
        let tracker = Arc::new(RecordingTracker::new());
        let meter = TrackingUsageMeter::new(inner, tracker.clone());

        let mut hostile = sample();
        hostile.agent = "project-titan-ceo".into();
        hostile.provider = "mcp:acme-internal-crm".into();
        meter
            .record(&CompanyId::new("acme"), &hostile)
            .await
            .unwrap();

        let rendered = format!("{:?}", tracker.events());
        assert!(!rendered.contains("project-titan-ceo"), "{rendered}");
        assert!(!rendered.contains("acme-internal-crm"), "{rendered}");
        assert!(
            rendered.contains("\"mcp\""),
            "the shape survives: {rendered}"
        );
    }
}
