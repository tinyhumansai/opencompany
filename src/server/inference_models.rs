//! OpenAI-compatible model catalog discovery and the OpenRouter registry cache.
//!
//! Both first-run setup and the inference settings picker consume the standard
//! `{ "data": [{ "id": ... }] }` model-list shape. Keeping the fetch and parser
//! here prevents setup from knowing only about the first entry while the picker
//! grows a second interpretation of the same provider response.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;

/// How long a successful OpenRouter catalog stays fresh in this process.
pub(crate) const MODEL_CATALOG_TTL: Duration = Duration::from_secs(60 * 60);

/// Maximum time a console page-load waits for the registry on a cache miss.
const MODEL_CATALOG_TIMEOUT: Duration = Duration::from_secs(10);

/// One model exposed to the operator console.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InferenceModel {
    /// Provider model id written unchanged into the tier mapping.
    pub(crate) id: String,
    /// Provider display name, when published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    /// Maximum context window, when published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context_length: Option<u64>,
}

/// Parsed leniently: `data` is decoded as raw JSON values first, so one
/// malformed entry (a non-string `id`, a string-valued `context_length`, …)
/// only drops that entry — or, for a malformed *optional* field, just that
/// field — in [`parse_models`] instead of failing the whole response and
/// hiding every valid model the endpoint actually returned.
#[derive(Deserialize)]
struct RegistryResponse {
    #[serde(default)]
    data: Vec<serde_json::Value>,
}

/// Parse every concrete model in a standard OpenAI-compatible catalog, in the
/// order the provider listed them.
///
/// Each `data` entry is read field-by-field rather than decoded in one shot
/// into a struct. `id` is the only field an entry cannot survive without —
/// everything downstream keys the tier mapping on it — so a missing or
/// non-string `id` drops the entry. `name` and `context_length` are read
/// leniently instead: a malformed optional field (a string-valued
/// `context_length`, say) is treated as absent rather than failing, so it
/// costs the entry that one field, not the id underneath it (issue #1838
/// follow-up — decoding the whole entry into a struct in one shot used to let
/// a bad *optional* field discard an otherwise-good `id` right along with it,
/// same shape as the whole-response failure `RegistryResponse` above already
/// guards against, one level deeper).
///
/// Order is preserved rather than sorted here: `src/server/setup.rs`'s probe
/// path takes `.next()` off this list to pick a local/custom endpoint's
/// leading model, the same thing the pre-catalog `discover_local_model` did
/// by taking the provider's first array entry. Sorting only matters for the
/// operator-facing OpenRouter catalog, so [`openrouter_models`] sorts its own
/// copy before caching it rather than this shared parser reordering every
/// caller's result.
fn parse_models(payload: RegistryResponse) -> Vec<InferenceModel> {
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();
    for entry in payload.data {
        let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let id = id.trim();
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        let context_length = entry
            .get("context_length")
            .and_then(serde_json::Value::as_u64);
        models.push(InferenceModel {
            id: id.to_string(),
            name,
            context_length,
        });
    }
    models
}

/// Fetch every model from an OpenAI-compatible `{base_url}/models` endpoint.
///
/// `bearer` is used by local/custom setup probes; OpenRouter's public registry
/// passes `None`.
pub(crate) async fn discover_models(
    base_url: &str,
    bearer: Option<&str>,
) -> Result<Vec<InferenceModel>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    // Bounded here, not left to each caller: reqwest's async client has no
    // default timeout, so an endpoint that accepts the connection but never
    // responds would otherwise hold this open indefinitely. `setup.rs`'s
    // local/custom probe calls this directly (no wrapping timeout of its
    // own), while `openrouter_models` below also wraps its call in
    // `tokio::time::timeout` for a friendlier, registry-specific message.
    let client = reqwest::Client::builder()
        .timeout(MODEL_CATALOG_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to build the model-discovery client: {error}"))?;
    let mut request = client.get(&url);
    if let Some(bearer) = bearer.filter(|bearer| !bearer.trim().is_empty()) {
        request = request.bearer_auth(bearer);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("request to {url} failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("request to {url} failed: {error}"))?;
    let payload = response
        .json::<RegistryResponse>()
        .await
        .map_err(|error| format!("model catalog from {url} was invalid: {error}"))?;
    Ok(parse_models(payload))
}

struct CacheEntry {
    at: Instant,
    models: Vec<InferenceModel>,
}

/// Process-wide OpenRouter catalog cache.
#[derive(Default)]
pub(crate) struct ModelCatalogCache {
    entry: Mutex<Option<CacheEntry>>,
    /// Serializes cache-miss fetches (issue #1838 follow-up). Held across the
    /// whole `discover_models` await, not just the cache write: without it,
    /// every console request that lands after startup or a TTL expiry sees
    /// the same empty/stale entry and fires its own upstream fetch, so a
    /// multi-tenant host can burst several identical OpenRouter registry
    /// calls at once — and any of them that gets rate-limited fails even
    /// though a sibling fetch is about to populate the cache. A `tokio`
    /// mutex, not `std`: the guard needs to survive the `.await` inside
    /// [`openrouter_models`].
    fetch_lock: TokioMutex<()>,
}

impl ModelCatalogCache {
    pub(crate) fn lookup(&self, now: Instant) -> Option<Vec<InferenceModel>> {
        let entry = self.entry.lock().ok()?;
        let entry = entry.as_ref()?;
        (now.saturating_duration_since(entry.at) < MODEL_CATALOG_TTL).then(|| entry.models.clone())
    }

    pub(crate) fn store(&self, models: Vec<InferenceModel>, at: Instant) {
        if let Ok(mut entry) = self.entry.lock() {
            *entry = Some(CacheEntry { at, models });
        }
    }
}

pub(crate) fn openrouter_cache() -> &'static ModelCatalogCache {
    static CACHE: OnceLock<ModelCatalogCache> = OnceLock::new();
    CACHE.get_or_init(ModelCatalogCache::default)
}

/// Return the cached OpenRouter catalog, fetching it on a miss.
///
/// Single-flight on a miss (issue #1838 follow-up): every caller queues on
/// [`ModelCatalogCache::fetch_lock`] rather than racing its own request to
/// OpenRouter, and re-checks the cache after acquiring it, so only the first
/// caller through actually fetches — everyone behind it reads what that
/// fetch just stored instead of duplicating the upstream call.
///
/// Bounded across the *whole* queue-wait-plus-fetch, not just the fetch
/// itself (issue #1838 follow-up): a failed fetch stores nothing, so during
/// a registry outage each queued caller would otherwise acquire the lock in
/// turn and run its own fresh `MODEL_CATALOG_TIMEOUT`-bounded attempt — the
/// Nth caller through the queue waiting roughly `N * MODEL_CATALOG_TIMEOUT`
/// before ever finding out, breaking the "a console page-load waits at most
/// [`MODEL_CATALOG_TIMEOUT`]" contract this module documents
/// (`docs/spec/runtime/providers.md`). Wrapping the lock acquisition and the
/// fetch in one `tokio::time::timeout` keeps every individual caller's own
/// wall-clock budget fixed at `MODEL_CATALOG_TIMEOUT`, however many callers
/// are already ahead of it in the queue.
pub(crate) async fn openrouter_models() -> Result<Vec<InferenceModel>, String> {
    let now = Instant::now();
    if let Some(models) = openrouter_cache().lookup(now) {
        return Ok(models);
    }

    let outcome = tokio::time::timeout(MODEL_CATALOG_TIMEOUT, async {
        let _fetch_guard = openrouter_cache().fetch_lock.lock().await;
        // Another caller may have already refilled the cache while we waited
        // for the lock — re-check before fetching again.
        let now = Instant::now();
        if let Some(models) = openrouter_cache().lookup(now) {
            return Ok(models);
        }

        let mut models = discover_models(crate::company::inference::OPENROUTER_BASE_URL, None)
            .await
            .map_err(FetchError::Failed)?;
        if models.is_empty() {
            return Err(FetchError::Failed(
                "OpenRouter's model registry returned no models".to_string(),
            ));
        }
        // Sorted here, not in `parse_models`: this is the operator-facing
        // catalog picker's own copy, while `parse_models` also serves
        // `setup.rs`'s local/custom probe, which relies on provider order.
        models.sort_by(|a, b| a.id.cmp(&b.id));
        openrouter_cache().store(models.clone(), now);
        Ok(models)
    })
    .await;

    match outcome {
        Ok(Ok(models)) => Ok(models),
        Ok(Err(FetchError::Failed(message))) => Err(message),
        Err(_elapsed) => Err(format!(
            "OpenRouter's model registry did not answer within {} seconds",
            MODEL_CATALOG_TIMEOUT.as_secs()
        )),
    }
}

/// Distinguishes "the fetch itself failed" from the outer
/// [`tokio::time::timeout`] elapsing in [`openrouter_models`], since both
/// have to report through the same `Result` and the outer timeout's own
/// message must win regardless of which inner step it interrupted.
enum FetchError {
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> InferenceModel {
        InferenceModel {
            id: id.to_string(),
            name: None,
            context_length: None,
        }
    }

    #[test]
    fn catalog_parser_returns_every_non_empty_unique_model_in_provider_order() {
        let parsed = parse_models(RegistryResponse {
            data: vec![
                serde_json::json!({"id": " vendor/zeta ", "name": " Zeta ", "context_length": 128_000}),
                serde_json::json!({"id": "vendor/alpha"}),
                serde_json::json!({"id": "vendor/zeta", "name": "duplicate"}),
                serde_json::json!({"id": "   "}),
            ],
        });

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "vendor/zeta");
        assert_eq!(parsed[0].name.as_deref(), Some("Zeta"));
        assert_eq!(parsed[0].context_length, Some(128_000));
        assert_eq!(parsed[1], model("vendor/alpha"));
    }

    /// Regression for a Minor review finding on #1838: the previous
    /// `BTreeMap`-keyed parser returned ids in lexicographic order, so
    /// `src/server/setup.rs` taking `.next()` off the result silently swapped
    /// from "the provider's first-listed model" (what the deleted
    /// `discover_local_model` returned) to "the alphabetically first model" —
    /// an arbitrary pick on any multi-model host whose ids don't already
    /// sort first-to-preferred.
    #[test]
    fn catalog_parser_does_not_alphabetize_a_local_hosts_leading_model() {
        let parsed = parse_models(RegistryResponse {
            data: vec![
                serde_json::json!({"id": "zephyr-preferred"}),
                serde_json::json!({"id": "alpaca-not-preferred"}),
            ],
        });

        assert_eq!(
            parsed.first().map(|m| m.id.as_str()),
            Some("zephyr-preferred"),
            "the provider's leading model must survive `.next()` in setup.rs, not lose to sort order"
        );
    }

    /// Regression for a P2 review finding on #1838: a single malformed
    /// record (here, a numeric `id`) used to fail `RegistryResponse`
    /// deserialization outright — `discover_models` never reached
    /// `parse_models` at all, so a valid model earlier or later in the same
    /// `data` array was lost with it. `data` is now decoded as raw JSON
    /// first, so only the bad entry drops out.
    #[test]
    fn catalog_parser_skips_malformed_entries_instead_of_rejecting_the_response() {
        let payload: RegistryResponse = serde_json::from_str(
            r#"{"data": [
                {"id": "vendor/good-one"},
                {"id": 12345},
                {"id": "vendor/good-three"}
            ]}"#,
        )
        .expect("RegistryResponse itself must still deserialize leniently");

        let parsed = parse_models(payload);

        let ids: Vec<&str> = parsed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["vendor/good-one", "vendor/good-three"]);
    }

    /// Regression for a P2 review finding on #1838's follow-up round: an
    /// entry whose `id` is fine but whose *optional* `context_length` is not
    /// used to lose the id right along with it. The old parser decoded each
    /// `data` entry into `RegistryModel` in one shot, and serde fails that
    /// whole decode on a type-mismatched field even when it is
    /// `Option<u64>` — a string-valued `context_length` isn't "field
    /// absent", it's "field present with the wrong type", and `Option`
    /// deserialization does not paper over that. So this id used to vanish
    /// from the catalog entirely instead of just losing its context length.
    #[test]
    fn catalog_parser_preserves_a_valid_id_with_malformed_optional_metadata() {
        let payload: RegistryResponse = serde_json::from_str(
            r#"{"data": [
                {"id": "vendor/good-one"},
                {"id": "vendor/good-two", "context_length": "not-a-number"},
                {"id": "vendor/good-three", "name": 42}
            ]}"#,
        )
        .expect("RegistryResponse itself must still deserialize leniently");

        let parsed = parse_models(payload);

        let ids: Vec<&str> = parsed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["vendor/good-one", "vendor/good-two", "vendor/good-three"],
            "a bad optional field must not discard the id sitting next to it"
        );
        let good_two = parsed
            .iter()
            .find(|m| m.id == "vendor/good-two")
            .expect("vendor/good-two must survive");
        assert_eq!(good_two.context_length, None);
        let good_three = parsed
            .iter()
            .find(|m| m.id == "vendor/good-three")
            .expect("vendor/good-three must survive");
        assert_eq!(good_three.name, None);
    }

    #[test]
    fn cache_serves_only_fresh_catalogs() {
        let cache = ModelCatalogCache::default();
        let stored_at = Instant::now();
        cache.store(vec![model("vendor/model")], stored_at);

        assert_eq!(
            cache.lookup(stored_at + MODEL_CATALOG_TTL - Duration::from_secs(1)),
            Some(vec![model("vendor/model")])
        );
        assert_eq!(cache.lookup(stored_at + MODEL_CATALOG_TTL), None);
    }

    /// Regression for a P2 review finding on #1838's follow-up round: without
    /// `fetch_lock`, every concurrent caller that observed the same
    /// empty/stale entry would independently "fetch" — a multi-tenant host
    /// bursting several identical upstream calls at once. This exercises the
    /// exact lock-then-recheck sequence [`openrouter_models`] runs (acquire
    /// `fetch_lock`, re-`lookup`, only then do the (here, simulated) fetch),
    /// against the real `ModelCatalogCache`, so a regression that drops the
    /// lock or the re-check fails this test rather than only showing up as
    /// upstream rate-limit noise in production.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_misses_coalesce_into_a_single_fetch() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache = Arc::new(ModelCatalogCache::default());
        let fetch_count = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let fetch_count = Arc::clone(&fetch_count);
                tokio::spawn(async move {
                    let now = Instant::now();
                    if let Some(models) = cache.lookup(now) {
                        return models;
                    }
                    let _fetch_guard = cache.fetch_lock.lock().await;
                    let now = Instant::now();
                    if let Some(models) = cache.lookup(now) {
                        return models;
                    }
                    fetch_count.fetch_add(1, Ordering::SeqCst);
                    // Hold the lock across a slow "upstream" call so every
                    // other task is still queued on `fetch_lock` — the same
                    // shape a real registry round-trip has.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let models = vec![model("vendor/single-flight")];
                    cache.store(models.clone(), Instant::now());
                    models
                })
            })
            .collect();

        for handle in handles {
            let models = handle.await.expect("fetch task must not panic");
            assert_eq!(models, vec![model("vendor/single-flight")]);
        }

        assert_eq!(
            fetch_count.load(Ordering::SeqCst),
            1,
            "only the first caller through fetch_lock should fetch; the rest must reuse its result"
        );
    }

    /// Regression for a P2 review finding on #1838's follow-up round, filed
    /// against the single-flight fix directly above: `fetch_lock` serializes
    /// misses, but a *failed* fetch stores nothing, so during a registry
    /// outage every queued caller would previously run its own fresh
    /// `bound`-length attempt after acquiring the lock — the Nth caller
    /// through the queue waiting roughly `N * bound` before ever finding out,
    /// which is exactly the docs/spec/runtime/providers.md "at most
    /// `MODEL_CATALOG_TIMEOUT` seconds" promise this test defends. Mirrors
    /// `openrouter_models`'s real composition (`tokio::time::timeout` wrapped
    /// around lock-acquire + recheck + fetch) against the real
    /// `ModelCatalogCache`, with a simulated fetch standing in for
    /// `discover_models` so the assertion is deterministic instead of racing
    /// a real HTTP timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_queued_caller_never_waits_longer_than_the_catalog_timeout() {
        use std::sync::Arc;

        let cache = Arc::new(ModelCatalogCache::default());
        // Short enough to keep the suite fast, long enough that three tasks
        // queuing on one real `tokio::sync::Mutex` stay well-ordered.
        let bound = Duration::from_millis(120);
        let fetch_delay = Duration::from_millis(80);

        async fn bounded_miss(
            cache: Arc<ModelCatalogCache>,
            bound: Duration,
            fetch_delay: Duration,
        ) {
            let _ = tokio::time::timeout(bound, async {
                let _fetch_guard = cache.fetch_lock.lock().await;
                let now = Instant::now();
                if cache.lookup(now).is_some() {
                    return;
                }
                // Simulates a registry that is down: takes real time, then
                // fails without storing anything — so the next caller through
                // the lock faces the same empty cache this one did.
                tokio::time::sleep(fetch_delay).await;
            })
            .await;
        }

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let cache = Arc::clone(&cache);
                tokio::spawn(async move {
                    let started = Instant::now();
                    bounded_miss(cache, bound, fetch_delay).await;
                    started.elapsed()
                })
            })
            .collect();

        for handle in handles {
            let elapsed = handle.await.expect("task must not panic");
            assert!(
                elapsed <= bound + Duration::from_millis(40),
                "a caller waited {elapsed:?}, which exceeds its own {bound:?} budget by more \
                 than scheduling slack — queue position must not multiply the wait"
            );
        }
    }
}
