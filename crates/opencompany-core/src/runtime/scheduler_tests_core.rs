pub(super) use super::*;
pub(super) use async_trait::async_trait;

pub(super) use crate::company::CompanyManifest;
pub(super) use crate::ports::brain::{Brain, CycleHost};
pub(super) use crate::ports::types::{
    CompressedTrace, CycleRequest, CycleResult, EventSeq, OutboundMessage, TokenUsage,
};
pub(super) use crate::runtime::RuntimeBuilder;
pub(super) use crate::runtime::cron::CivilTime;

pub(super) fn tmp_home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("opencompany-sched-")
        .tempdir()
        .expect("tempdir")
}

pub(super) fn manifest(policy_mode: &str) -> CompanyManifest {
    let toml_src = format!(
        r#"
        [company]
        name = "Acme"

        [[agent]]
        id = "ceo"
        role = "Chief"

        [policy]
        mode = "{policy_mode}"
        "#
    );
    toml::from_str(&toml_src).expect("parse manifest")
}

/// Unix millis for a UTC civil minute, reusing the cron module's math.
pub(super) fn millis_at(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> u64 {
    // Search forward from a coarse lower bound is overkill; instead binary
    // via the known conversion: rebuild through CivilTime round-trip.
    // Simpler: brute a direct computation using days-from-civil is private,
    // so derive from a probe. We reconstruct by scanning day starts.
    let mut probe = 0u64;
    // Jump in ~day steps to the target date, then add hour/minute.
    loop {
        let c = CivilTime::from_unix_millis(probe);
        if (c.year, c.month, c.day) == (year, month, day) {
            break;
        }
        probe += 86_400_000;
        if probe > 4_102_444_800_000 {
            panic!("date out of probe range");
        }
    }
    probe + (hour as u64) * 3_600_000 + (minute as u64) * MINUTE_MS
}

/// A brain that echoes ScheduleFired events into an operator response, so a
/// test can assert a scheduled cycle actually ran.
pub(super) struct ScheduleBrain;

#[async_trait]
impl Brain for ScheduleBrain {
    async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
        let mut responses = Vec::new();
        for event in &req.events {
            if let CompanyEvent::ScheduleFired { prompt, .. } = event {
                responses.push(OutboundMessage {
                    message_id: None,
                    task_id: None,
                    outputs: Vec::new(),
                    channel: "operator".into(),
                    agent: None,
                    text: format!("scheduled: {prompt}"),
                    steps: Vec::new(),
                    reply_to: None,
                    mentions: Vec::new(),
                });
            }
        }
        Ok(CycleResult {
            channel_responses: responses,
            new_traces: vec![CompressedTrace::now(&req.cycle_id, "scheduled")],
            ledger_deltas: Vec::new(),
            token_usage: TokenUsage::default(),
        })
    }
}

pub(super) fn scheduled_manifest() -> CompanyManifest {
    let toml_src = r#"
        [company]
        name = "Acme"

        [[agent]]
        id = "ceo"
        role = "Chief"

        [[schedule]]
        cron = "0 9 * * MON"
        prompt = "weekly standup"

        [policy]
        mode = "full"
    "#;
    toml::from_str(toml_src).expect("parse manifest")
}

/// A brain that parks inside its first cycle until released, so a test can
/// deliver a shutdown while a tick is provably in flight.
pub(super) struct BlockingBrain {
    pub(super) started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    pub(super) release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait]
impl Brain for BlockingBrain {
    async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
        if let Some(tx) = self.started.lock().expect("started lock").take() {
            let _ = tx.send(());
        }
        // Taken out of the mutex first so no guard is held across the await.
        let release = self.release.lock().expect("release lock").take();
        if let Some(rx) = release {
            let _ = rx.await;
        }
        Ok(CycleResult {
            channel_responses: Vec::new(),
            new_traces: vec![CompressedTrace::now(&req.cycle_id, "blocking")],
            ledger_deltas: Vec::new(),
            token_usage: TokenUsage::default(),
        })
    }
}

// --- issue #241: durable claims + restart catch-up ---------------------

pub(super) use crate::error::OpenCompanyError;
pub(super) use crate::ports::schedule_fires::ScheduleFireStore;
pub(super) use crate::ports::types::CompanyId;

/// ScheduleFired events actually written to a runtime's log. A claim loser,
/// and a fail-closed skip, must leave this at whatever it was — the "zero
/// side effects" invariant, read from the durable trail.
pub(super) async fn fired_count(rt: &CompanyRuntime) -> usize {
    rt.events
        .read_from(rt.id(), EventSeq::new(0), 1024)
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(e.event, CompanyEvent::ScheduleFired { .. }))
        .count()
}

/// A claim store whose every method errors, for the fail-closed path.
pub(super) struct ErroringFires;

#[async_trait]
impl ScheduleFireStore for ErroringFires {
    async fn claim_fire(&self, _c: &CompanyId, _s: &str, _m: u64) -> Result<bool> {
        Err(OpenCompanyError::Store("claim store is down".into()))
    }
    async fn latest_fire(&self, _c: &CompanyId, _s: &str) -> Result<Option<u64>> {
        Err(OpenCompanyError::Store("claim store is down".into()))
    }
    async fn prune_fires_before(&self, _c: &CompanyId, _m: u64) -> Result<usize> {
        Err(OpenCompanyError::Store("claim store is down".into()))
    }
    async fn delete_schedule_fires(&self, _c: &CompanyId, _s: &str) -> Result<usize> {
        Err(OpenCompanyError::Store("claim store is down".into()))
    }
}

// --- issue #661 (F1): re-arm the boot catch-up until one successful pass ----

pub(super) use std::collections::HashSet;
pub(super) use std::sync::Mutex;
pub(super) use std::sync::atomic::AtomicUsize;

/// Moves the company's durable lifecycle, so a test can pause the company
/// across a boot catch-up and later resume it.
pub(super) async fn set_lifecycle(rt: &CompanyRuntime, lifecycle: &str) {
    let store = rt.store().clone();
    let mut record = store
        .load(rt.id())
        .await
        .expect("loads")
        .expect("the builder materialized a record");
    record.lifecycle = lifecycle.to_string();
    store.save(&record).await.expect("saves");
}

/// An in-memory claim store whose first N `latest_fire` reads error, then
/// works — the flaky-once double for "a transient store error does NOT latch
/// the boot catch-up, so a later pass retries" (issue #661 F1). `seed` presets
/// an anchor without consuming the fail budget.
pub(super) struct FlakyOnceFires {
    claims: Mutex<HashMap<(String, String), HashSet<u64>>>,
    fail_latest: AtomicUsize,
}

impl FlakyOnceFires {
    pub(super) fn new(fail_latest: usize) -> Self {
        Self {
            claims: Mutex::new(HashMap::new()),
            fail_latest: AtomicUsize::new(fail_latest),
        }
    }
    pub(super) fn seed(&self, c: &CompanyId, s: &str, m: u64) {
        self.claims
            .lock()
            .unwrap()
            .entry((c.as_ref().to_string(), s.to_string()))
            .or_default()
            .insert(m);
    }
}

#[async_trait]
impl ScheduleFireStore for FlakyOnceFires {
    async fn claim_fire(&self, c: &CompanyId, s: &str, m: u64) -> Result<bool> {
        Ok(self
            .claims
            .lock()
            .unwrap()
            .entry((c.as_ref().to_string(), s.to_string()))
            .or_default()
            .insert(m))
    }
    async fn latest_fire(&self, c: &CompanyId, s: &str) -> Result<Option<u64>> {
        if self.fail_latest.load(Ordering::SeqCst) > 0 {
            self.fail_latest.fetch_sub(1, Ordering::SeqCst);
            return Err(OpenCompanyError::Store("flaky once claim store".into()));
        }
        Ok(self
            .claims
            .lock()
            .unwrap()
            .get(&(c.as_ref().to_string(), s.to_string()))
            .and_then(|set| set.iter().max().copied()))
    }
    async fn prune_fires_before(&self, _c: &CompanyId, _m: u64) -> Result<usize> {
        Ok(0)
    }
    async fn delete_schedule_fires(&self, c: &CompanyId, s: &str) -> Result<usize> {
        Ok(self
            .claims
            .lock()
            .unwrap()
            .remove(&(c.as_ref().to_string(), s.to_string()))
            .map_or(0, |set| set.len()))
    }
}
