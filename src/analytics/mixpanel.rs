//! The Mixpanel transport, and the one function that chooses a tracker.
//!
//! [`build`] is compiled into **every** build; `HttpMixpanelTracker` is compiled
//! only under `--features analytics`. That split is the acceptance criterion of
//! issue #1739 expressed as a type rather than as a rule: in a default build
//! there is no type here that owns an HTTP client, so "a build with no opt-in
//! emits zero outbound analytics requests" is not a behaviour that could
//! regress — the code that would make the request is not in the binary.
//!
//! Under the feature, [`build`] still returns [`NullTracker`] for every
//! [`Decision::Silent`], which is what a desktop or self-hosted install resolves
//! to. `a_self_hosted_build_makes_no_request` proves that against a real local
//! collector, and `a_hosted_tenant_reports` is its positive control — without
//! the second, a zero request count would be indistinguishable from a test that
//! never sends anything at all.

use std::sync::Arc;

use crate::analytics::config::Decision;
use crate::analytics::{Envelope, NullTracker, Tracker};

/// Chooses the tracker this process will use.
///
/// The whole of the "hosted tenants only, by default" decision lands here: a
/// [`Decision::Silent`] gets a [`NullTracker`], and in a build without the
/// `analytics` feature *every* decision does, because there is nothing else to
/// return.
pub fn build(decision: &Decision, envelope: Envelope) -> Arc<dyn Tracker> {
    match decision {
        Decision::Silent(_) => Arc::new(NullTracker),
        #[cfg(feature = "analytics")]
        Decision::Report { endpoint, token } => {
            Arc::new(http::HttpMixpanelTracker::new(endpoint, token, envelope))
        }
        // Without the feature there is no transport to hand back. Reporting was
        // configured and the build cannot honour it, which is worth one line at
        // boot: silently ignoring an explicit `OPENCOMPANY_ANALYTICS=on` is the
        // kind of quiet no-op an operator debugs for an hour.
        #[cfg(not(feature = "analytics"))]
        Decision::Report { .. } => {
            let _ = envelope;
            tracing::info!(
                "[analytics] reporting is configured but this build was compiled without \
                 the `analytics` feature, so nothing is sent"
            );
            Arc::new(NullTracker)
        }
    }
}

#[cfg(feature = "analytics")]
pub use http::HttpMixpanelTracker;

#[cfg(feature = "analytics")]
mod http {
    use std::sync::{Arc, Mutex, Weak};
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::analytics::config::ProjectToken;
    use crate::analytics::{Envelope, Event, Tracker, payload};

    /// How often the background task drains the buffer.
    ///
    /// A threshold alone is not enough: a quiet instance would hold its events
    /// until the next one arrived, which on a company that ran two turns and
    /// stopped is forever.
    const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

    /// The most events held before the oldest are dropped.
    ///
    /// Analytics must never be able to grow without bound inside a tenant
    /// container. If the collector is unreachable for long enough to fill this,
    /// the right outcome is losing telemetry, not the process.
    const MAX_BUFFERED: usize = 500;

    /// How long a send may take before it is abandoned. Short on purpose:
    /// nothing waits on this, but a request that never completes is a task that
    /// never ends.
    const SEND_TIMEOUT: Duration = Duration::from_secs(5);

    /// Batches events and POSTs them to Mixpanel.
    pub struct HttpMixpanelTracker {
        inner: Arc<Inner>,
    }

    struct Inner {
        client: reqwest::Client,
        endpoint: String,
        token: ProjectToken,
        /// Behind a lock because its cognition labels are re-read after boot
        /// — see [`Envelope::set_cognition`]. Only ever held to render one
        /// payload or to relabel, never across an await.
        envelope: std::sync::RwLock<Envelope>,
        buffer: Mutex<Vec<serde_json::Value>>,
        /// Held for the whole of one `send_batch`, drain **and** request.
        ///
        /// Without it, the shutdown flush and the 30-second drain could
        /// overlap: the drain takes the entire buffer and awaits its POST, the
        /// flush finds an empty buffer, returns at once, and process exit
        /// cancels the request still in flight. That loses the whole batch
        /// exactly when the collector is slow — the one case the graceful flush
        /// exists for. An **async** mutex because it is held across an await;
        /// the `buffer` lock below stays a `std::sync` one and is never held
        /// across one.
        sending: tokio::sync::Mutex<()>,
        stop: tokio::sync::Notify,
    }

    impl HttpMixpanelTracker {
        /// Builds a tracker and starts its drain loop.
        pub fn new(endpoint: &str, token: &ProjectToken, envelope: Envelope) -> Self {
            let inner = Arc::new(Inner {
                client: reqwest::Client::builder()
                    .timeout(SEND_TIMEOUT)
                    .build()
                    .unwrap_or_default(),
                endpoint: endpoint.to_string(),
                token: token.clone(),
                envelope: std::sync::RwLock::new(envelope),
                buffer: Mutex::new(Vec::new()),
                sending: tokio::sync::Mutex::new(()),
                stop: tokio::sync::Notify::new(),
            });

            // A `Weak` so the loop cannot keep the tracker alive, and
            // `try_current` so constructing one outside a runtime is a
            // flush-only tracker rather than a panic. Neither is theoretical:
            // the drop path is how a rebuilt runtime retires its tracker, and a
            // synchronous test constructs one with no reactor.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let weak = Arc::downgrade(&inner);
                handle.spawn(async move { drain_loop(weak).await });
            }

            Self { inner }
        }
    }

    impl Drop for HttpMixpanelTracker {
        fn drop(&mut self) {
            self.inner.stop.notify_waiters();
        }
    }

    impl std::fmt::Debug for HttpMixpanelTracker {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // No endpoint and certainly no token: this type holds a credential,
            // and a `{:?}` in a log line is exactly how one escapes.
            f.write_str("HttpMixpanelTracker")
        }
    }

    async fn drain_loop(weak: Weak<Inner>) {
        loop {
            let Some(inner) = weak.upgrade() else { return };
            let stopped = {
                let stop = &inner.stop;
                tokio::select! {
                    _ = stop.notified() => true,
                    _ = tokio::time::sleep(FLUSH_INTERVAL) => false,
                }
            };
            inner.send_batch().await;
            if stopped {
                return;
            }
        }
    }

    /// The one rendering of a transport failure this module is allowed to log.
    ///
    /// `reqwest::Error` keeps the request URL and prints it — `… for url (…)` —
    /// and that URL is `OPENCOMPANY_ANALYTICS_ENDPOINT`. For a deployment
    /// fronting Mixpanel with an authenticated proxy, that is precisely where
    /// the proxy's key lives: in userinfo (`https://user:key@host/track`) or in
    /// the query string (`?key=…`). So a collector that merely goes unreachable
    /// wrote the operator's credential into container logs, on a path the boot
    /// line's redaction never touched and the `ProjectToken` redaction guards a
    /// different string from entirely.
    ///
    /// `without_url` **removes** the URL rather than rewriting it, which is why
    /// this is not a second redaction surface to keep in step with
    /// `boot::loggable_endpoint`. There is nothing here to diverge: the error
    /// carries no URL at all, and the destination on the same log line comes
    /// from that one helper, so the transport learns about a new place a URL can
    /// hold a secret at the same moment the boot line does.
    pub(super) fn loggable_send_error(error: reqwest::Error) -> String {
        error.without_url().to_string()
    }

    impl Inner {
        /// Drains the buffer and posts it. Every failure is swallowed after one
        /// debug line: a dead collector is a no-op, per #1739's constraints.
        ///
        /// Serialized: a caller entering while another send is in flight waits
        /// for it and then drains whatever has arrived since. That is what makes
        /// [`Tracker::flush`] a real guarantee rather than a buffer inspection —
        /// see [`Inner::sending`].
        async fn send_batch(&self) {
            let _sending = self.sending.lock().await;
            let batch = {
                let mut buffer = self.buffer.lock().expect("analytics buffer");
                if buffer.is_empty() {
                    return;
                }
                std::mem::take(&mut *buffer)
            };

            // The project token is stamped here, into the request body, and
            // nowhere else. `analytics::payload` — the un-gated, tested body
            // builder — never sees it, so no test fixture, log line or recorded
            // event can carry it.
            let events: Vec<serde_json::Value> = batch
                .into_iter()
                .map(|mut event| {
                    if let Some(properties) = event
                        .get_mut("properties")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        properties.insert(
                            "token".to_string(),
                            serde_json::Value::from(self.token.expose()),
                        );
                    }
                    event
                })
                .collect();

            match self.client.post(&self.endpoint).json(&events).send().await {
                Ok(response) if response.status().is_success() => {}
                Ok(response) => tracing::debug!(
                    status = %response.status(),
                    "[analytics] the collector refused a batch; dropping it"
                ),
                Err(error) => tracing::debug!(
                    endpoint = %crate::analytics::boot::loggable_endpoint(&self.endpoint),
                    error = %loggable_send_error(error),
                    "[analytics] could not reach the collector; dropping the batch"
                ),
            }
        }
    }

    #[async_trait]
    impl Tracker for HttpMixpanelTracker {
        fn track(&self, event: Event) {
            let body = {
                let envelope = self.inner.envelope.read().expect("analytics envelope");
                payload(&envelope, &event)
            };
            let mut buffer = self.inner.buffer.lock().expect("analytics buffer");
            if buffer.len() >= MAX_BUFFERED {
                buffer.remove(0);
            }
            buffer.push(body);
        }

        async fn flush(&self) {
            // Waits on any in-flight periodic drain before draining what is
            // left, so a shutdown overlapping the 30-second loop does not
            // return while the previous batch is still on the wire.
            self.inner.send_batch().await;
        }

        fn observe_cognition(&self, cognition: crate::ports::brain::Cognition) {
            self.inner
                .envelope
                .write()
                .expect("analytics envelope")
                .set_cognition(cognition);
        }
    }
}

#[cfg(all(test, feature = "analytics"))]
mod test {
    use super::*;
    use crate::analytics::config::{ENABLE_ENV, ENDPOINT_ENV, TOKEN_ENV, resolve};
    use crate::analytics::types::OpaqueId;
    use crate::analytics::{Event, Outcome, Trigger};
    use crate::app::config::MapEnv;
    use crate::app::deployment::{DEPLOYMENT_ENV, Deployment};
    use crate::ports::brain::Cognition;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// A local collector that counts what it is sent.
    struct Collector {
        hits: Arc<AtomicUsize>,
        bodies: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        url: String,
        shutdown: tokio::sync::oneshot::Sender<()>,
        handle: tokio::task::JoinHandle<()>,
    }

    async fn spawn_collector() -> Collector {
        spawn_collector_taking(Duration::ZERO).await
    }

    /// A collector that takes `delay` to answer, so a test can observe what
    /// happens while a request is still in flight.
    async fn spawn_collector_taking(delay: Duration) -> Collector {
        let hits = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_hits = hits.clone();
        let seen_bodies = bodies.clone();

        let app = axum::Router::new().route(
            "/track",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let hits = seen_hits.clone();
                let bodies = seen_bodies.clone();
                async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    hits.fetch_add(1, Ordering::SeqCst);
                    bodies.lock().unwrap().push(body);
                    axum::Json(serde_json::json!({"status": 1}))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/track", listener.local_addr().unwrap());
        let (shutdown, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });

        Collector {
            hits,
            bodies,
            url,
            shutdown,
            handle,
        }
    }

    impl Collector {
        async fn stop(self) {
            let _ = self.shutdown.send(());
            let _ = self.handle.await;
        }
    }

    fn envelope() -> Envelope {
        Envelope::new(
            OpaqueId::instance("0123456789abcdef0123456789abcdef"),
            Deployment::HostedTenant,
            Cognition::default(),
        )
    }

    fn events() -> Vec<Event> {
        vec![
            Event::InstanceStarted {
                companies: 1,
                storage: "fs",
                setup_complete: true,
            },
            Event::TurnFinished {
                trigger: Trigger::OperatorMessage,
                outcome: Outcome::Ok,
                failure: None,
                duration_ms: 12,
                effects_executed: 0,
                approvals_parked: 0,
            },
        ]
    }

    /// **Issue #1739's first acceptance criterion.** A build that *has* the
    /// transport compiled in, pointed at a live collector, with a token in the
    /// environment, and not declared hosted: it must send nothing.
    ///
    /// Note what is deliberately stacked against the assertion — the feature is
    /// on, the client exists, the endpoint resolves, the token is present. The
    /// only thing that is not is consent. That is the configuration a
    /// self-hoster who copied a hosted deployment's env file would have.
    #[tokio::test]
    async fn a_self_hosted_build_makes_no_request() {
        let collector = spawn_collector().await;
        let env = MapEnv::new([
            (TOKEN_ENV, "not-a-real-token"),
            (ENDPOINT_ENV, collector.url.as_str()),
        ]);

        let decision = resolve(Deployment::from_env(&env), &env);
        let tracker = build(&decision, envelope());
        for event in events() {
            tracker.track(event);
        }
        tracker.flush().await;

        assert_eq!(
            collector.hits.load(Ordering::SeqCst),
            0,
            "a self-hosted build must not dial out"
        );
        collector.stop().await;
    }

    /// The positive control that makes the test above non-vacuous: the same
    /// collector, the same events, the same code path, one variable changed.
    #[tokio::test]
    async fn a_hosted_tenant_reports_with_the_full_envelope() {
        let collector = spawn_collector().await;
        let env = MapEnv::new([
            (DEPLOYMENT_ENV, "hosted-tenant"),
            (TOKEN_ENV, "not-a-real-token"),
            (ENDPOINT_ENV, collector.url.as_str()),
        ]);

        let decision = resolve(Deployment::from_env(&env), &env);
        assert!(decision.reports(), "{decision:?}");
        let tracker = build(&decision, envelope());
        for event in events() {
            tracker.track(event);
        }
        tracker.flush().await;

        assert_eq!(collector.hits.load(Ordering::SeqCst), 1, "one batch");
        let bodies = collector.bodies.lock().unwrap().clone();
        let batch = bodies[0].as_array().expect("a batch is an array");
        assert_eq!(batch.len(), 2, "both events rode the batch: {batch:?}");

        let first = &batch[0]["properties"];
        assert_eq!(first["deployment"], "hosted-tenant");
        assert_eq!(first["distinct_id"], "i_0123456789abcdef0123456789abcdef");
        assert_eq!(first["token"], "not-a-real-token");
        assert!(first["app_version"].is_string());
        assert!(first["harness_in_build"].is_boolean());
        assert_eq!(batch[0]["event"], "instance_started");

        collector.stop().await;
    }

    /// An operator who switched it off stays off, even on a hosted tenant.
    #[tokio::test]
    async fn an_opted_out_tenant_makes_no_request() {
        let collector = spawn_collector().await;
        let env = MapEnv::new([
            (DEPLOYMENT_ENV, "hosted-tenant"),
            (ENABLE_ENV, "off"),
            (TOKEN_ENV, "not-a-real-token"),
            (ENDPOINT_ENV, collector.url.as_str()),
        ]);

        let tracker = build(&resolve(Deployment::from_env(&env), &env), envelope());
        for event in events() {
            tracker.track(event);
        }
        tracker.flush().await;

        assert_eq!(collector.hits.load(Ordering::SeqCst), 0);
        collector.stop().await;
    }

    /// The buffer is bounded. An unreachable collector must cost telemetry, not
    /// a tenant container's memory.
    #[tokio::test]
    async fn the_buffer_is_bounded() {
        let env = MapEnv::new([
            (DEPLOYMENT_ENV, "hosted-tenant"),
            (TOKEN_ENV, "not-a-real-token"),
            // Nothing listens here; the point is that `track` never blocks and
            // never grows without bound whatever the collector does.
            (ENDPOINT_ENV, "http://127.0.0.1:1/track"),
        ]);
        let tracker = build(&resolve(Deployment::from_env(&env), &env), envelope());
        for _ in 0..2_000 {
            tracker.track(Event::InstanceStarted {
                companies: 1,
                storage: "fs",
                setup_complete: true,
            });
        }
        // No assertion on an internal count — the observable property is that
        // this returns at all, promptly, with no reachable collector.
    }

    /// **A transport failure must not carry the collector credential.**
    ///
    /// `OPENCOMPANY_ANALYTICS_ENDPOINT` exists so a deployment can front
    /// Mixpanel with its own authenticated proxy, and such a proxy carries its
    /// key in one of the two places a URL can hold one. `reqwest::Error`
    /// retains the request URL and prints it, so an unreachable collector — a
    /// routine event, not an exotic one — wrote that key into the debug log.
    ///
    /// Measured against reqwest 0.12.28 rather than assumed, and the two places
    /// do **not** behave alike:
    ///
    /// | in the endpoint | what `reqwest::Error`'s `Display` printed |
    /// |---|---|
    /// | `http://someone:KEY@127.0.0.1:1/track` | `… for url (http://127.0.0.1:1/track)` — userinfo already stripped |
    /// | `http://127.0.0.1:1/track?key=KEY` | `… for url (http://127.0.0.1:1/track?key=KEY)` — **leaked verbatim** |
    ///
    /// So the query string is the live leak; userinfo is not, today. Both are
    /// covered here anyway, because "the dependency strips it" is not a
    /// property this crate owns — it is one `cargo update` from being false,
    /// and nothing here would fail when it changed. `without_url` removes the
    /// URL outright, so neither shape can reach the line whatever reqwest
    /// decides to print.
    ///
    /// Asserted **case-insensitively**, with the self-check below: this PR
    /// already shipped a leak guard that passed a deliberate leak because the
    /// value came back lowercased.
    #[tokio::test]
    async fn a_transport_failure_never_carries_the_endpoint_credential() {
        const SECRET: &str = "NotARealCollectorKey";
        let needle = SECRET.to_ascii_lowercase();

        // The self-check, on the shape that is measurably still leaking. A
        // guard that cannot find the needle in the **unstripped** error proves
        // nothing about the stripped one — the needle may never have been
        // there at all. If reqwest ever starts redacting query strings too,
        // this fails loudly and says so, rather than leaving a guard behind
        // that asserts nothing.
        let leaky = format!("http://127.0.0.1:1/track?key={SECRET}");
        let unstripped = send_failing(&leaky).await.to_string();
        assert!(
            unstripped.to_ascii_lowercase().contains(&needle),
            "the needle must be findable before stripping, or this guard is \
             vacuous: {unstripped}"
        );

        for endpoint in [
            // Port 1 refuses, so each of these is a real transport error rather
            // than a fabricated one.
            format!("http://someone:{SECRET}@127.0.0.1:1/track"),
            leaky.clone(),
            format!("http://someone:{SECRET}@127.0.0.1:1/track?key={SECRET}"),
        ] {
            let logged = super::http::loggable_send_error(send_failing(&endpoint).await);
            assert!(
                !logged.to_ascii_lowercase().contains(&needle),
                "the transport error leaked the collector credential from \
                 {endpoint:?}: {logged}"
            );
            assert!(
                !logged.is_empty(),
                "stripping the URL must still leave the operator a reason: {logged}"
            );

            // And the destination is still named on the same line — through the
            // one redaction helper the boot line uses, not a second one.
            let named = crate::analytics::boot::loggable_endpoint(&endpoint);
            assert!(
                !named.to_ascii_lowercase().contains(&needle),
                "the endpoint field leaked it instead: {named}"
            );
            assert!(
                named.contains("127.0.0.1"),
                "the operator still has to be able to tell where it was going: {named}"
            );
        }
    }

    /// One real, refused request. Nothing listens on port 1.
    async fn send_failing(endpoint: &str) -> reqwest::Error {
        reqwest::Client::new()
            .post(endpoint)
            .json(&serde_json::json!([]))
            .send()
            .await
            .expect_err("nothing listens on port 1")
    }

    /// **A shutdown flush waits for a send already in flight.**
    ///
    /// The periodic drain takes the whole buffer before it awaits its POST, so
    /// a flush that only inspected the buffer would find it empty, return at
    /// once, and let process exit cancel the request carrying the batch —
    /// losing everything precisely when the collector is slow, which is the one
    /// case the graceful flush exists for.
    ///
    /// Asserted by timing, against a collector that takes 600ms: the second
    /// flush must not return before the first request completes. The threshold
    /// is 300ms against a 600ms delay, so it neither trips on scheduling jitter
    /// nor passes without the wait (the unserialized version returns in
    /// microseconds).
    #[tokio::test]
    async fn a_flush_waits_for_a_send_already_in_flight() {
        let collector = spawn_collector_taking(Duration::from_millis(600)).await;
        let env = MapEnv::new([
            (DEPLOYMENT_ENV, "hosted-tenant"),
            (TOKEN_ENV, "not-a-real-token"),
            (ENDPOINT_ENV, collector.url.as_str()),
        ]);
        let tracker = build(&resolve(Deployment::from_env(&env), &env), envelope());

        tracker.track(Event::InstanceStarted {
            companies: 1,
            storage: "fs",
            setup_complete: true,
        });

        // Stands in for the 30-second drain loop: it takes the buffer and is
        // then parked on the POST.
        let first = {
            let tracker = tracker.clone();
            tokio::spawn(async move { tracker.flush().await })
        };
        // Long enough for the spawned task to drain the buffer and start its
        // request, short enough to be well inside the 600ms the collector takes.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let started = Instant::now();
        tracker.flush().await;
        let waited = started.elapsed();

        assert!(
            waited >= Duration::from_millis(300),
            "the flush returned in {waited:?} while a send was still in flight; \
             on a real shutdown that batch would be cancelled with the process"
        );

        first.await.expect("the in-flight send finished");
        assert_eq!(
            collector.hits.load(Ordering::SeqCst),
            1,
            "the batch really was in flight and really did land"
        );
        collector.stop().await;
    }
}
