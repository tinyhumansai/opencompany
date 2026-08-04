//! Hosted TinyHumans embeddings compute — the meaning-vector backend the in-pod
//! `tinycortex` memory engine plugs into for semantic recall (issue #188,
//! slice 188c2).
//!
//! OpenCompany ships no embeddings client of its own; this is the net-new one.
//! [`HostedEmbeddings`] mirrors [`HostedProvider`](crate::harness::provider::HostedProvider)'s
//! HTTP idiom — an OpenAI-compatible endpoint over the shared TinyHumans
//! credential — but POSTs to `{base_url}/embeddings` instead of
//! `/chat/completions`. It implements **only** the vendored engine's
//! [`EmbeddingBackend`] compute trait; it is NOT a retrieval-scorer `Embedder`
//! (the 768-dim summary-tree seal path), which is deferred to #198.
//!
//! ## Dimensions — native, no `dimensions` request param
//!
//! The managed embeddings model (`embedding-v1`) is **1024-dim** and rejects the
//! OpenAI `dimensions` request field (that param is honoured only by
//! `text-embedding-3-*`), so this client never sends it. The store+search path
//! it feeds ([`VectorStore`](tinycortex::memory::store::vectors::VectorStore)) is
//! dimension-agnostic — it keys off `backend.dimensions()` — so the whole slice
//! runs at the backend's native 1024. [`HostedEmbeddings::dimensions`] returns
//! the model's true dim (default 1024, overridable via
//! [`EMBEDDINGS_DIM_ENV`] for a future model); every returned vector is
//! validated against it and a wrong length is an error, never truncated.
//!
//! ## Signature stability
//!
//! [`EmbeddingBackend::signature`] is derived from `name + model_id + dims` and
//! is stable across instances built from the same config, so a restart never
//! silently splits the persisted embedding space (the #1574 drift trap).

use async_trait::async_trait;

use tinycortex::memory::store::vectors::embedding::EmbeddingBackend;

use crate::app::config::EnvSource;
use crate::company::credentials::Credential;
use crate::harness::provider::hosted_endpoint_from_env;

/// Default managed embeddings model — 1024-dim, `dimensions`-param-free.
pub const DEFAULT_EMBEDDINGS_MODEL: &str = "embedding-v1";

/// Default embedding dimensionality (`embedding-v1`'s only allowed size).
pub const DEFAULT_EMBEDDINGS_DIM: usize = 1024;

/// Overrides the embeddings model (defaults to [`DEFAULT_EMBEDDINGS_MODEL`]).
pub const EMBEDDINGS_MODEL_ENV: &str = "OPENCOMPANY_EMBEDDINGS_MODEL";

/// Overrides the embedding dimensionality (defaults to [`DEFAULT_EMBEDDINGS_DIM`]).
/// Only meaningful alongside a model whose native dim differs from 1024.
pub const EMBEDDINGS_DIM_ENV: &str = "OPENCOMPANY_EMBEDDINGS_DIM";

/// Stable backend name folded into the embedding-space [`signature`](EmbeddingBackend::signature).
const HOSTED_EMBEDDINGS_NAME: &str = "tinyhumans";

/// Max inputs per outbound `/embeddings` request; larger batches are split so a
/// single call never exceeds the managed backend's per-request budget.
const MAX_INPUTS_PER_REQUEST: usize = 64;

/// The outcome of one `/embeddings` POST, distinguishing a 401 (retry once after
/// invalidating the credential) from any other error (propagated as-is).
enum SendOutcome {
    /// A successful JSON payload.
    Ok(serde_json::Value),
    /// The backend rejected the bearer (401) — retryable after invalidation.
    Unauthorized,
    /// A hard, non-retryable error (already scrubbed of the bearer).
    Err(anyhow::Error),
}

/// Hosted TinyHumans embeddings backend.
///
/// Speaks the OpenAI-compatible `/embeddings` wire format over HTTPS against the
/// same managed endpoint + credential the chat provider uses. Implements the
/// vendored engine's [`EmbeddingBackend`] compute trait so a
/// [`VectorStore`](tinycortex::memory::store::vectors::VectorStore) can turn
/// chunk text into meaning vectors.
#[derive(Clone)]
pub struct HostedEmbeddings {
    /// Base URL of the OpenAI-compatible API; the client POSTs to
    /// `{base_url}/embeddings`.
    base_url: String,
    /// How the bearer is obtained, resolved on **every** request so a rotated
    /// platform token is picked up without a rebuild.
    credential: Credential,
    /// The embeddings model id (e.g. `embedding-v1`).
    model: String,
    /// The model's native embedding dimensionality (default 1024).
    dimensions: usize,
    /// Shared HTTP client.
    client: reqwest::Client,
}

impl HostedEmbeddings {
    /// Builds a hosted embeddings backend from its endpoint configuration.
    pub fn new(
        base_url: impl Into<String>,
        credential: Credential,
        model: impl Into<String>,
        dimensions: usize,
    ) -> Self {
        // A stalled hosted endpoint must degrade to lexical recall, never hang
        // the request path (put_chunk/search_chunks) indefinitely — and the 401
        // retry in `send_with_retry` would otherwise double the worst case.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base_url: base_url.into(),
            credential,
            model: model.into(),
            dimensions,
            client,
        }
    }

    /// Embeds one sub-batch (`≤ MAX_INPUTS_PER_REQUEST`) against the endpoint,
    /// returning vectors in **input order** regardless of the wire order.
    async fn embed_batch(&self, batch: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        // NOTE: no `dimensions` field — `embedding-v1` ignores/400s it.
        let body = serde_json::json!({ "model": self.model, "input": batch });
        let payload = self.send_with_retry(&body).await?;

        let data = payload
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| anyhow::anyhow!("embeddings response missing a `data` array"))?;
        if data.len() != batch.len() {
            anyhow::bail!(
                "embeddings response returned {} vectors for {} inputs",
                data.len(),
                batch.len()
            );
        }

        // Collect `(index, vector)` then sort by index so a backend that returns
        // `data` out of order still round-trips to input order.
        let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for (fallback, item) in data.iter().enumerate() {
            let index = item
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map(|i| i as usize)
                .unwrap_or(fallback);
            let arr = item
                .get("embedding")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow::anyhow!("embeddings item missing an `embedding` array"))?;
            let vector = arr
                .iter()
                .map(|n| n.as_f64().map(|f| f as f32))
                .collect::<Option<Vec<f32>>>()
                .ok_or_else(|| anyhow::anyhow!("embedding contained a non-numeric component"))?;
            // Validate against the backend's declared dim — never truncate.
            if vector.len() != self.dimensions {
                anyhow::bail!(
                    "embedding dimension mismatch: expected {}, got {}",
                    self.dimensions,
                    vector.len()
                );
            }
            indexed.push((index, vector));
        }
        indexed.sort_by_key(|(index, _)| *index);
        Ok(indexed.into_iter().map(|(_, vector)| vector).collect())
    }

    /// POSTs `body`, retrying **once** after a 401 (invalidating the credential
    /// first so the retry re-reads a possibly-rotated token).
    async fn send_with_retry(&self, body: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        match self.send_once(body).await {
            SendOutcome::Ok(value) => Ok(value),
            SendOutcome::Err(err) => Err(err),
            SendOutcome::Unauthorized => {
                // A rejected bearer may mean the platform rotated the token early;
                // drop the cached read so the retry goes back to the source.
                self.credential.invalidate();
                match self.send_once(body).await {
                    SendOutcome::Ok(value) => Ok(value),
                    SendOutcome::Err(err) => Err(err),
                    SendOutcome::Unauthorized => Err(anyhow::anyhow!(
                        "hosted embeddings returned 401 after retry"
                    )),
                }
            }
        }
    }

    /// Issues a single `/embeddings` POST. Every error string is scrubbed of the
    /// bearer so a credential can never leak into a log line.
    async fn send_once(&self, body: &serde_json::Value) -> SendOutcome {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let bearer = match self.credential.current().await {
            Ok(bearer) => bearer,
            Err(e) => {
                return SendOutcome::Err(anyhow::anyhow!(
                    "resolving the embeddings credential: {e}"
                ));
            }
        };
        let scrub = |text: String| match &bearer {
            Some(bearer) if !bearer.is_empty() => text.replace(bearer.as_str(), "<redacted>"),
            _ => text,
        };

        let mut http = self.client.post(&url).json(body);
        if let Some(bearer) = &bearer {
            http = http.bearer_auth(bearer);
        }

        let response = match http.send().await {
            Ok(response) => response,
            Err(e) => {
                return SendOutcome::Err(anyhow::anyhow!(
                    "hosted embeddings request failed: {}",
                    scrub(e.to_string())
                ));
            }
        };
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return SendOutcome::Unauthorized;
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return SendOutcome::Err(anyhow::anyhow!(
                "hosted embeddings returned {status}: {}",
                scrub(text)
            ));
        }
        match response.json().await {
            Ok(value) => SendOutcome::Ok(value),
            Err(e) => SendOutcome::Err(anyhow::anyhow!(
                "hosted embeddings response was not JSON: {}",
                scrub(e.to_string())
            )),
        }
    }
}

#[async_trait]
impl EmbeddingBackend for HostedEmbeddings {
    fn name(&self) -> &str {
        HOSTED_EMBEDDINGS_NAME
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    // `signature()` uses the trait default: `provider=…;model=…;dims=…` — stable
    // and provider-derived, so it never splits the embedding space (#1574).

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(MAX_INPUTS_PER_REQUEST) {
            out.extend(self.embed_batch(batch).await?);
        }
        Ok(out)
    }
}

/// Resolve a [`HostedEmbeddings`] backend from the environment, or `None` when no
/// credential can be obtained (the offline/no-embeddings path — recall degrades
/// to lexical).
///
/// Shares the **one** credential + base-url resolution
/// ([`hosted_endpoint_from_env`]) with the chat inference client, then layers the
/// embeddings model + dimensionality on top:
///
/// * model — [`EMBEDDINGS_MODEL_ENV`], else [`DEFAULT_EMBEDDINGS_MODEL`].
/// * dims — [`EMBEDDINGS_DIM_ENV`] (a positive integer), else
///   [`DEFAULT_EMBEDDINGS_DIM`].
pub fn hosted_embeddings_from_env(env: &dyn EnvSource) -> Option<HostedEmbeddings> {
    let (credential, base_url) = hosted_endpoint_from_env(env)?;
    let model = env
        .get(EMBEDDINGS_MODEL_ENV)
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_EMBEDDINGS_MODEL.to_string());
    let dimensions = env
        .get(EMBEDDINGS_DIM_ENV)
        .and_then(|d| d.trim().parse::<usize>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_EMBEDDINGS_DIM);
    Some(HostedEmbeddings::new(
        base_url, credential, model, dimensions,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use crate::app::config::MapEnv;

    /// How a spawned mock `/embeddings` server answers.
    #[derive(Clone, Copy)]
    enum Mode {
        /// Return, per input, a `dim`-length vector whose first component is the
        /// input's byte-sum (so a test can map output→input and prove order).
        ByteSum { dim: usize },
        /// First call answers 401, every later call succeeds (`dim`-length,
        /// byte-sum tagged) — proves the invalidate-and-retry path.
        UnauthorizedThenOk { dim: usize },
        /// Return vectors of the wrong length (`dim` when the client expects
        /// something else) — proves the length guard.
        WrongLength { dim: usize },
        /// Return `data` in reversed `index` order (each embedding's first
        /// component is its declared index) — proves index-sorting.
        ShuffledIndex { dim: usize },
    }

    /// Captured state of a spawned server: the request bodies it saw and a call
    /// counter (for the 401-then-ok mode).
    #[derive(Default)]
    struct Seen {
        bodies: Vec<serde_json::Value>,
        calls: usize,
    }

    /// Spawns an axum `/embeddings` stub in `mode`, returning its base URL and the
    /// shared capture handle.
    async fn spawn_server(mode: Mode) -> (String, Arc<StdMutex<Seen>>) {
        use axum::routing::post;
        use axum::{Json, Router};

        let seen: Arc<StdMutex<Seen>> = Arc::new(StdMutex::new(Seen::default()));
        let log = Arc::clone(&seen);
        let app = Router::new().route(
            "/embeddings",
            post(move |Json(body): Json<serde_json::Value>| {
                let log = Arc::clone(&log);
                async move {
                    let call = {
                        let mut guard = log.lock().unwrap();
                        guard.bodies.push(body.clone());
                        guard.calls += 1;
                        guard.calls
                    };
                    let inputs: Vec<String> = body
                        .get("input")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .map(|s| s.as_str().unwrap_or_default().to_string())
                                .collect()
                        })
                        .unwrap_or_default();

                    let byte_sum = |s: &str| s.bytes().map(u32::from).sum::<u32>() as f64;
                    let make = |first: f64, dim: usize| {
                        let mut v = vec![0.0_f64; dim];
                        if dim > 0 {
                            v[0] = first;
                        }
                        v
                    };

                    match mode {
                        Mode::ByteSum { dim } => {
                            let data: Vec<serde_json::Value> = inputs
                                .iter()
                                .enumerate()
                                .map(|(i, s)| {
                                    serde_json::json!({ "index": i, "embedding": make(byte_sum(s), dim) })
                                })
                                .collect();
                            Json(serde_json::json!({ "data": data })).into_response()
                        }
                        Mode::UnauthorizedThenOk { dim } => {
                            if call == 1 {
                                return (
                                    axum::http::StatusCode::UNAUTHORIZED,
                                    "bad token",
                                )
                                    .into_response();
                            }
                            let data: Vec<serde_json::Value> = inputs
                                .iter()
                                .enumerate()
                                .map(|(i, s)| {
                                    serde_json::json!({ "index": i, "embedding": make(byte_sum(s), dim) })
                                })
                                .collect();
                            Json(serde_json::json!({ "data": data })).into_response()
                        }
                        Mode::WrongLength { dim } => {
                            let data: Vec<serde_json::Value> = inputs
                                .iter()
                                .enumerate()
                                .map(|(i, _)| {
                                    serde_json::json!({ "index": i, "embedding": vec![0.5_f64; dim] })
                                })
                                .collect();
                            Json(serde_json::json!({ "data": data })).into_response()
                        }
                        Mode::ShuffledIndex { dim } => {
                            // Emit in reverse order, each tagged with its true index.
                            let mut data: Vec<serde_json::Value> = inputs
                                .iter()
                                .enumerate()
                                .map(|(i, _)| {
                                    serde_json::json!({ "index": i, "embedding": make(i as f64, dim) })
                                })
                                .collect();
                            data.reverse();
                            Json(serde_json::json!({ "data": data })).into_response()
                        }
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    use axum::response::IntoResponse;

    fn backend(url: String, dim: usize) -> HostedEmbeddings {
        HostedEmbeddings::new(
            url,
            Credential::from_value("test-token"),
            DEFAULT_EMBEDDINGS_MODEL,
            dim,
        )
    }

    #[tokio::test]
    async fn request_carries_input_array_and_never_a_dimensions_field() {
        let (url, seen) = spawn_server(Mode::ByteSum { dim: 8 }).await;
        let be = backend(url, 8);
        let out = be.embed(&["hello", "world"]).await.unwrap();
        assert_eq!(out.len(), 2);

        let body = &seen.lock().unwrap().bodies[0];
        assert_eq!(body["model"], DEFAULT_EMBEDDINGS_MODEL);
        assert!(body["input"].is_array(), "input must be an array");
        assert!(
            body.get("dimensions").is_none(),
            "must NOT send a `dimensions` field to embedding-v1"
        );
    }

    #[tokio::test]
    async fn retries_once_after_unauthorized() {
        let (url, seen) = spawn_server(Mode::UnauthorizedThenOk { dim: 4 }).await;
        let be = backend(url, 4);
        let out = be.embed_one("recover").await.unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(
            seen.lock().unwrap().calls,
            2,
            "a 401 must invalidate + retry exactly once"
        );
    }

    #[tokio::test]
    async fn wrong_length_vector_is_an_error() {
        // Client expects dim 8; server returns dim 3.
        let (url, _seen) = spawn_server(Mode::WrongLength { dim: 3 }).await;
        let be = backend(url, 8);
        let err = be.embed(&["x"]).await.unwrap_err();
        assert!(
            err.to_string().contains("dimension mismatch"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn sub_batches_large_input_and_preserves_order() {
        let dim = 4;
        let (url, seen) = spawn_server(Mode::ByteSum { dim }).await;
        let be = backend(url, dim);

        // 130 unique inputs → 3 requests at ≤64 each.
        let inputs: Vec<String> = (0..130).map(|i| format!("item-{i}")).collect();
        let refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
        let out = be.embed(&refs).await.unwrap();

        assert_eq!(out.len(), 130);
        assert_eq!(seen.lock().unwrap().calls, 3, "≤64/req ⇒ 3 sub-batches");
        // Each output's first component is the byte-sum of the matching input,
        // proving order survived the split + reassembly.
        for (i, input) in inputs.iter().enumerate() {
            let expected = input.bytes().map(u32::from).sum::<u32>() as f32;
            assert_eq!(out[i][0], expected, "output {i} out of order");
        }
    }

    #[tokio::test]
    async fn reorders_response_by_index() {
        let dim = 4;
        let (url, _seen) = spawn_server(Mode::ShuffledIndex { dim }).await;
        let be = backend(url, dim);
        let out = be.embed(&["a", "b", "c"]).await.unwrap();
        assert_eq!(out.len(), 3);
        // Server emitted reversed data tagged with true indices; the client must
        // restore input order (embedding[0] == its index).
        assert_eq!(out[0][0], 0.0);
        assert_eq!(out[1][0], 1.0);
        assert_eq!(out[2][0], 2.0);
    }

    #[test]
    fn signature_is_stable_across_instances() {
        let a = HostedEmbeddings::new(
            "https://a.test/v1",
            Credential::None,
            DEFAULT_EMBEDDINGS_MODEL,
            DEFAULT_EMBEDDINGS_DIM,
        );
        let b = HostedEmbeddings::new(
            "https://b.test/v1",
            Credential::from_value("other"),
            DEFAULT_EMBEDDINGS_MODEL,
            DEFAULT_EMBEDDINGS_DIM,
        );
        assert_eq!(
            a.signature(),
            b.signature(),
            "signature must be config-derived"
        );
        assert!(a.signature().contains("dims=1024"));
    }

    #[test]
    fn from_env_defaults_to_embedding_v1_1024() {
        let env = MapEnv::new([("TINYHUMANS_API_KEY", "sk-platform")]);
        let be = hosted_embeddings_from_env(&env).expect("configured");
        assert_eq!(be.model_id(), DEFAULT_EMBEDDINGS_MODEL);
        assert_eq!(be.dimensions(), DEFAULT_EMBEDDINGS_DIM);
    }

    #[test]
    fn from_env_honours_model_and_dim_overrides() {
        let env = MapEnv::new([
            ("OPENCOMPANY_INFERENCE_KEY", "sk-specific"),
            ("OPENCOMPANY_EMBEDDINGS_MODEL", "text-embedding-3-small"),
            ("OPENCOMPANY_EMBEDDINGS_DIM", "1536"),
        ]);
        let be = hosted_embeddings_from_env(&env).expect("configured");
        assert_eq!(be.model_id(), "text-embedding-3-small");
        assert_eq!(be.dimensions(), 1536);
    }

    #[test]
    fn from_env_is_none_without_any_credential() {
        let env = MapEnv::new([("OPENCOMPANY_INFERENCE_URL", "https://x/v1")]);
        assert!(hosted_embeddings_from_env(&env).is_none());
    }
}
