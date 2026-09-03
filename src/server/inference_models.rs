//! OpenAI-compatible model catalog discovery, cached **per endpoint**.
//!
//! Both first-run setup and the inference settings picker consume the standard
//! `{ "data": [{ "id": ... }] }` model-list shape. Keeping the fetch and parser
//! here prevents setup from knowing only about the first entry while the picker
//! grows a second interpretation of the same provider response.
//!
//! The cache used to be a single process-wide slot holding OpenRouter's public
//! registry, because the picker route asked for that registry unconditionally —
//! whatever endpoint the company had actually been pointed at. Discovery now
//! follows the configured base URL, so the cache is a registry keyed on it: one
//! entry per endpoint, each with its own single-flight lock, so two tenants on
//! two providers neither share a catalog nor queue behind each other.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;

use crate::company::inference::TierVocabulary;

/// How long a successful catalog stays fresh in this process.
pub(crate) const MODEL_CATALOG_TTL: Duration = Duration::from_secs(60 * 60);

/// How long a *failed* catalog read is remembered.
///
/// Much shorter than the success TTL, and it exists for a different reason: a
/// failure that stored nothing meant every caller retried, so an unreachable
/// provider cost a fresh [`MODEL_CATALOG_TIMEOUT`] on every status read and
/// every turn that consulted the vocabulary. Remembering "this endpoint did not
/// answer, a minute ago" turns that into one attempt a minute while staying
/// short enough that a provider coming back up is picked up promptly.
pub(crate) const MODEL_CATALOG_FAILURE_TTL: Duration = Duration::from_secs(60);

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
/// `bearer` is the credential the endpoint expects — the company's stored key
/// for a tenant catalog read, `None` for a public registry (OpenRouter's) or a
/// keyless local server.
pub(crate) async fn discover_models(
    base_url: &str,
    bearer: Option<&str>,
) -> Result<Vec<InferenceModel>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    // Bounded here, not left to each caller: reqwest's async client has no
    // default timeout, so an endpoint that accepts the connection but never
    // responds would otherwise hold this open indefinitely. `setup.rs`'s
    // local/custom probe calls this directly (no wrapping timeout of its
    // own), while `catalog_models` below also wraps its call in
    // `tokio::time::timeout` for a friendlier, endpoint-naming message.
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

/// One endpoint's catalog cache.
#[derive(Default)]
pub(crate) struct ModelCatalogCache {
    entry: Mutex<Option<CacheEntry>>,
    /// The last failure and when it happened — see [`MODEL_CATALOG_FAILURE_TTL`].
    failure: Mutex<Option<(Instant, String)>>,
    /// Serializes cache-miss fetches (issue #1838 follow-up). Held across the
    /// whole `discover_models` await, not just the cache write: without it,
    /// every console request that lands after startup or a TTL expiry sees
    /// the same empty/stale entry and fires its own upstream fetch, so a
    /// multi-tenant host can burst several identical registry calls at once —
    /// and any of them that gets rate-limited fails even though a sibling
    /// fetch is about to populate the cache. A `tokio` mutex, not `std`: the
    /// guard needs to survive the `.await` inside [`catalog_models`].
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
        // A success clears the failure memo: the endpoint is answering again,
        // and leaving a stale "unreachable" behind would keep reporting it.
        if let Ok(mut failure) = self.failure.lock() {
            *failure = None;
        }
    }

    /// The remembered failure, while it is still fresh.
    pub(crate) fn lookup_failure(&self, now: Instant) -> Option<String> {
        let failure = self.failure.lock().ok()?;
        let (at, message) = failure.as_ref()?;
        (now.saturating_duration_since(*at) < MODEL_CATALOG_FAILURE_TTL).then(|| message.clone())
    }

    pub(crate) fn store_failure(&self, message: String, at: Instant) {
        if let Ok(mut failure) = self.failure.lock() {
            *failure = Some((at, message));
        }
    }
}

/// The per-endpoint cache registry.
///
/// Keyed on the normalized base URL and **not** on the credential. A model
/// catalog is a public property of an endpoint rather than of the key used to
/// read it, and a credential must not become a map key — hashing one to key a
/// cache would put a derivative of it in process memory next to the data it
/// guards, for a partition nothing here needs. The cost is that two tenants
/// sharing one endpoint share its catalog; the benefit is that a credential
/// never leaves the paths that already handle it.
fn catalog_registry() -> &'static Mutex<HashMap<String, Arc<ModelCatalogCache>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<ModelCatalogCache>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Trailing slashes and surrounding space do not make a different endpoint.
fn cache_key(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// This endpoint's cache, created on first use.
pub(crate) fn catalog_cache(base_url: &str) -> Arc<ModelCatalogCache> {
    let key = cache_key(base_url);
    let mut registry = match catalog_registry().lock() {
        Ok(registry) => registry,
        // A poisoned registry must not take the catalog offline for the rest of
        // the process: hand back an unshared cache, which costs this caller a
        // fetch and nothing else.
        Err(_) => return Arc::new(ModelCatalogCache::default()),
    };
    Arc::clone(registry.entry(key).or_default())
}

/// Return the cached catalog for `base_url`, fetching it on a miss.
///
/// Single-flight per endpoint (issue #1838 follow-up): every caller for one
/// endpoint queues on its [`ModelCatalogCache::fetch_lock`] rather than racing
/// its own request, and re-checks the cache after acquiring it, so only the
/// first caller through actually fetches — everyone behind it reads what that
/// fetch just stored instead of duplicating the upstream call.
///
/// Bounded across the *whole* queue-wait-plus-fetch, not just the fetch
/// itself (issue #1838 follow-up): during an outage each queued caller would
/// otherwise acquire the lock in turn and run its own fresh
/// `MODEL_CATALOG_TIMEOUT`-bounded attempt — the Nth caller through the queue
/// waiting roughly `N * MODEL_CATALOG_TIMEOUT` before ever finding out,
/// breaking the "a console page-load waits at most [`MODEL_CATALOG_TIMEOUT`]"
/// contract this module documents (`docs/spec/runtime/providers.md`). Wrapping
/// the lock acquisition and the fetch in one `tokio::time::timeout` keeps every
/// individual caller's own wall-clock budget fixed, however many callers are
/// already ahead of it in the queue.
///
/// A failure is remembered for [`MODEL_CATALOG_FAILURE_TTL`] and replayed to
/// callers within it, so an unreachable provider costs one attempt a minute
/// rather than one per request.
pub(crate) async fn catalog_models(
    base_url: &str,
    bearer: Option<&str>,
) -> Result<Vec<InferenceModel>, String> {
    let cache = catalog_cache(base_url);
    let now = Instant::now();
    if let Some(models) = cache.lookup(now) {
        return Ok(models);
    }
    if let Some(failure) = cache.lookup_failure(now) {
        return Err(failure);
    }

    let endpoint = cache_key(base_url);
    let outcome = tokio::time::timeout(MODEL_CATALOG_TIMEOUT, async {
        let _fetch_guard = cache.fetch_lock.lock().await;
        // Another caller may have already refilled the cache while we waited
        // for the lock — re-check before fetching again.
        let now = Instant::now();
        if let Some(models) = cache.lookup(now) {
            return Ok(models);
        }
        if let Some(failure) = cache.lookup_failure(now) {
            return Err(FetchError::Failed(failure));
        }

        let mut models = discover_models(base_url, bearer)
            .await
            .map_err(FetchError::Failed)?;
        if models.is_empty() {
            return Err(FetchError::Failed(format!(
                "{endpoint} published an empty model catalog"
            )));
        }
        // Sorted here, not in `parse_models`: this is the operator-facing
        // catalog picker's own copy, while `parse_models` also serves
        // `setup.rs`'s local/custom probe, which relies on provider order.
        models.sort_by(|a, b| a.id.cmp(&b.id));
        cache.store(models.clone(), now);
        Ok(models)
    })
    .await;

    let result = match outcome {
        Ok(Ok(models)) => return Ok(models),
        Ok(Err(FetchError::Failed(message))) => message,
        Err(_elapsed) => format!(
            "{endpoint} did not answer within {} seconds",
            MODEL_CATALOG_TIMEOUT.as_secs()
        ),
    };
    cache.store_failure(result.clone(), Instant::now());
    Err(result)
}

/// What vocabulary `base_url` speaks, or `None` when its catalog cannot be read.
///
/// `None` is deliberately not [`TierVocabulary::Unknown`]: "the endpoint told us
/// it publishes neither vocabulary" and "we could not ask" are different facts
/// and lead to different operator advice, so the caller keeps its pre-discovery
/// fallback for the second rather than acting on an answer nobody gave.
// Both consumers — the turn path (`TenantProvider::resolve`) and the console
// probe (`test_config`) — live behind the `openhuman` feature, so a default
// build compiles this and calls it from nowhere. Gating the function itself
// would put a second `cfg` on a pure, feature-independent helper and make the
// two builds disagree about what this module offers.
#[cfg_attr(not(feature = "openhuman"), allow(dead_code))]
pub(crate) async fn discovered_vocabulary(
    base_url: &str,
    bearer: Option<&str>,
) -> Option<TierVocabulary> {
    let models = catalog_models(base_url, bearer).await.ok()?;
    Some(TierVocabulary::from_catalog_ids(
        models.iter().map(|model| model.id.as_str()),
    ))
}

/// Distinguishes "the fetch itself failed" from the outer
/// [`tokio::time::timeout`] elapsing in [`catalog_models`], since both
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

    /// A failure used to store nothing, so an unreachable provider cost a fresh
    /// `MODEL_CATALOG_TIMEOUT` on every request that consulted it — once per
    /// status read and, now that the turn path consults the vocabulary, once
    /// per turn. Remembering it briefly turns that into one attempt a minute.
    #[test]
    fn a_failure_is_remembered_briefly_and_cleared_by_the_next_success() {
        let cache = ModelCatalogCache::default();
        let failed_at = Instant::now();
        cache.store_failure("provider.example did not answer".to_string(), failed_at);

        assert_eq!(
            cache.lookup_failure(failed_at + MODEL_CATALOG_FAILURE_TTL - Duration::from_secs(1)),
            Some("provider.example did not answer".to_string()),
        );
        assert_eq!(
            cache.lookup_failure(failed_at + MODEL_CATALOG_FAILURE_TTL),
            None,
            "the memo expires far sooner than a success, so a provider coming back up is \
             picked up promptly"
        );

        cache.store_failure("still down".to_string(), Instant::now());
        let recovered_at = Instant::now();
        cache.store(vec![model("vendor/model")], recovered_at);
        assert_eq!(
            cache.lookup_failure(recovered_at),
            None,
            "a success clears the memo — leaving it would keep reporting an outage that ended"
        );
    }

    /// The seam the turn path and the probe both read: a cached catalog answers
    /// the vocabulary question with no request of its own.
    #[tokio::test]
    async fn a_cached_catalog_answers_the_vocabulary_question() {
        const ENDPOINT: &str = "https://vocabulary.example/v1";
        catalog_cache(ENDPOINT).store(vec![model("agentic-v1"), model("chat-v1")], Instant::now());
        assert_eq!(
            discovered_vocabulary(ENDPOINT, None).await,
            Some(TierVocabulary::Tiers)
        );
    }

    /// Two endpoints are two caches. A single process-wide slot is what let one
    /// company's catalog answer for another's endpoint in the first place.
    #[test]
    fn each_endpoint_gets_its_own_cache_and_trailing_slashes_do_not_split_one() {
        let a = catalog_cache("https://a.example/v1");
        let b = catalog_cache("https://b.example/v1");
        let now = Instant::now();
        a.store(vec![model("a-only")], now);

        assert_eq!(a.lookup(now), Some(vec![model("a-only")]));
        assert_eq!(b.lookup(now), None, "b must not inherit a's catalog");
        assert_eq!(
            catalog_cache("https://a.example/v1/").lookup(now),
            Some(vec![model("a-only")]),
            "a trailing slash is the same endpoint"
        );
    }

    /// Regression for a P2 review finding on #1838's follow-up round: without
    /// `fetch_lock`, every concurrent caller that observed the same
    /// empty/stale entry would independently "fetch" — a multi-tenant host
    /// bursting several identical upstream calls at once. This exercises the
    /// exact lock-then-recheck sequence [`catalog_models`] runs (acquire
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
    /// `catalog_models`'s real composition (`tokio::time::timeout` wrapped
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
