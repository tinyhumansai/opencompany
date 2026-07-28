//! Inference model implementations for the embedded harness.
//!
//! openhuman's inference now runs on tinyagents [`ChatModel<()>`] (the old
//! `Provider` trait was deleted upstream), so opencompany brings its own
//! implementations. Consistent with the spec non-goal "not a model host", only
//! two production surfaces ship:
//!
//! * [`HostedProvider`] / [`TenantProvider`] — talk to the hosted TinyHumans /
//!   Medulla brain (and per-tenant BYOK endpoints) over an OpenAI-compatible
//!   chat-completions endpoint. This is the sole production inference path;
//!   there is no local-LLM or BYO-model seam beyond the tenant's own config.
//! * [`MockProvider`] — a deterministic, offline model used by tests (and by
//!   any caller that wants the harness wired without a network).
//!
//! Every model additionally implements [`HarnessModel`], a thin supertrait that
//! re-adds the one bit `ChatModel` drops: the telemetry provider slug the WS5
//! cost hook attributes per-turn spend to (read live after each turn so a BYOK
//! switch re-attributes cost on the next turn).
//!
//! ## Billing envelope (critical)
//!
//! The hosted managed backend wraps its metered totals in an
//! `openhuman.{usage,billing}` envelope. openhuman's crate-native cost pipeline
//! recovers the charged USD by reading an `openhuman_usage_meta` key off
//! [`ModelResponse::raw`] (via its internal `usage_info_from_response`). We
//! therefore mirror openhuman's own `OpenHumanBackendModel::project_managed_usage`
//! shape: [`model_response_from_payload`] sets `raw` to the full wire payload and
//! injects that meta key with the backend-charged USD + context window, so the
//! host cost layer (`agent/cost.rs`, `cost/global.rs`) still sees the charge.
//! (openhuman's own `merge_openhuman_usage_meta` helper is `pub(crate)`, hence
//! the local re-expression in [`inject_usage_meta`].)

use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use tinyagents::harness::message::Message;
use tinyagents::harness::model::{ChatModel, ModelRequest, ModelResponse};
use tinyagents::harness::usage::Usage;
use tinyagents::{Result as TaResult, TinyAgentsError};

use crate::app::config::EnvSource;
use crate::company::Inference;
use crate::company::inference::{self, EnvDefault, InferenceDecl};
use crate::ports::SecretStore;
use crate::ports::types::CompanyId;

/// Default hosted inference endpoint when only a bare `TINYHUMANS_API_KEY` is
/// supplied — the OpenAI-compatible surface a company agent's `chat-v1` /
/// `reasoning-v1` / … workloads resolve against.
pub const DEFAULT_TINYHUMANS_INFERENCE_URL: &str = "https://api.tinyhumans.ai/openai/v1";

/// Default hosted model/tier when none is configured.
pub const DEFAULT_HOSTED_MODEL: &str = "chat-v1";

/// The `HTTP-Referer` attribution header OpenRouter asks BYOK callers to send —
/// it identifies the app in OpenRouter's dashboard/rankings.
pub const OPENROUTER_REFERER: &str = "https://opencompany.tinyhumans.ai";

/// The `X-Title` attribution header OpenRouter asks BYOK callers to send.
pub const OPENROUTER_TITLE: &str = "OpenCompany";

/// The key under which the managed billing/context metadata is stashed on
/// [`ModelResponse::raw`] so openhuman's crate-native cost pipeline recovers the
/// backend-charged USD. Must match openhuman's `OPENHUMAN_USAGE_META_KEY`.
const OPENHUMAN_USAGE_META_KEY: &str = "openhuman_usage_meta";

/// A harness inference model: a tinyagents [`ChatModel<()>`] plus the telemetry
/// slug OpenCompany attributes per-turn cost to.
///
/// `ChatModel` carries no provider identity, so this thin supertrait re-adds the
/// one bit the WS5 cost hook needs. It is read **live** after each turn (see
/// [`HarnessPool::run`](crate::harness::HarnessPool)) so a console BYOK switch
/// re-attributes spend on the next turn. `Arc<dyn HarnessModel>` upcasts to
/// `Arc<dyn ChatModel<()>>` at the openhuman `AgentBuilder::chat_model` seam.
pub trait HarnessModel: ChatModel<()> {
    /// Stable provider slug attributed to usage samples (e.g. `managed`, `byok`).
    fn telemetry_provider_id(&self) -> String;
}

/// Resolve a [`HostedProvider`] configuration (and its default model) from the
/// environment, or `None` when no credential is present.
///
/// Precedence, most specific first:
///
/// * key — `OPENCOMPANY_INFERENCE_KEY`, else `TINYHUMANS_API_KEY`. **No key ⇒
///   `None`**, and the runtime keeps its offline echo brain.
/// * url — `OPENCOMPANY_INFERENCE_URL`, else [`DEFAULT_TINYHUMANS_INFERENCE_URL`].
/// * model — `OPENCOMPANY_INFERENCE_MODEL`, else [`DEFAULT_HOSTED_MODEL`].
///
/// The two-name key precedence keeps a per-tenant override
/// (`OPENCOMPANY_INFERENCE_KEY`) distinct from the platform-wide TinyHumans
/// credential the manager injects (`TINYHUMANS_API_KEY`).
pub fn harness_inference_from_env(
    env: &dyn EnvSource,
) -> Option<(HostedProviderConfig, Option<String>)> {
    let api_key = env
        .get("OPENCOMPANY_INFERENCE_KEY")
        .or_else(|| env.get("TINYHUMANS_API_KEY"))?;
    let base_url = env
        .get("OPENCOMPANY_INFERENCE_URL")
        .unwrap_or_else(|| DEFAULT_TINYHUMANS_INFERENCE_URL.to_string());
    // The model is a per-roster **override** now: only an explicit
    // `OPENCOMPANY_INFERENCE_MODEL` flattens every agent to one workload. When
    // unset, each agent keeps its tier-derived model, which the tenant
    // `[inference].models` table then maps. `None` = no override.
    let model_override = env.get("OPENCOMPANY_INFERENCE_MODEL");
    Some((
        HostedProviderConfig {
            base_url,
            api_key,
            extra_headers: Vec::new(),
        },
        model_override,
    ))
}

/// Default media-generation backend base URL when only a bare
/// `TINYHUMANS_API_KEY` is supplied — the OpenHuman backend that owns the GMI
/// provider keys, billing, and rate limiting for image/video generation.
pub const DEFAULT_TINYHUMANS_MEDIA_BACKEND_URL: &str = "https://api.tinyhumans.ai";

/// Resolve the MANAGED media-generation backend (issue #109) from the
/// environment, or `None` when no managed credential is present (fail-closed —
/// no credential ⇒ no media tools are ever wired).
///
/// Precedence, most specific first:
///
/// * token — `OPENCOMPANY_MEDIA_KEY`, else `TINYHUMANS_API_KEY`. **No token ⇒
///   `None`.** This is the platform's own managed credential; the tenant
///   identity the backend bills is derived server-side from it.
/// * url — `OPENCOMPANY_MEDIA_BACKEND_URL`, else
///   [`DEFAULT_TINYHUMANS_MEDIA_BACKEND_URL`].
///
/// **Security**: this deliberately consults ONLY the environment — never a
/// tenant secret store — so media generation can only ever run on the managed
/// platform credential, never a company-controlled BYOK key. Mirrors
/// [`harness_inference_from_env`]'s two-name precedence so a per-platform media
/// override (`OPENCOMPANY_MEDIA_KEY`) stays distinct from the shared
/// `TINYHUMANS_API_KEY`.
pub fn media_backend_from_env(env: &dyn EnvSource) -> Option<super::toolbelt::MediaBackend> {
    let auth_token = env
        .get("OPENCOMPANY_MEDIA_KEY")
        .or_else(|| env.get("TINYHUMANS_API_KEY"))?;
    let backend_url = env
        .get("OPENCOMPANY_MEDIA_BACKEND_URL")
        .unwrap_or_else(|| DEFAULT_TINYHUMANS_MEDIA_BACKEND_URL.to_string());
    Some(super::toolbelt::MediaBackend {
        backend_url,
        auth_token,
    })
}

/// Flatten a tinyagents request's messages into the OpenAI-compatible wire
/// `[{role, content}]` array. Text-only: each message's role is mapped by
/// variant and its concatenated text taken verbatim.
fn wire_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|m| {
            let role = match m {
                Message::System(_) => "system",
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::Tool(_) => "tool",
            };
            serde_json::json!({ "role": role, "content": m.text() })
        })
        .collect()
}

/// Extract token usage from an OpenAI-compatible chat-completion payload as a
/// tinyagents [`Usage`], or `None` when the payload carries no `usage` block.
///
/// Cached-input tokens follow the same precedence the legacy path used: the
/// `openhuman.usage.cached_input_tokens` envelope wins over the standard
/// `usage.prompt_tokens_details.cached_tokens`. They land on
/// [`Usage::cache_read_tokens`], which openhuman's `usage_info_from_response`
/// reads back as `cached_input_tokens`.
fn parse_usage(payload: &serde_json::Value) -> Option<Usage> {
    let usage = payload.get("usage")?;
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    // Total back-fills to input+output when the wire reports 0/absent.
    let total_tokens = usage
        .get("total_tokens")
        .and_then(serde_json::Value::as_u64)
        .filter(|&t| t != 0)
        .unwrap_or(input_tokens + output_tokens);
    let cache_read_tokens = payload
        .pointer("/openhuman/usage/cached_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(0);
    Some(Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_tokens,
        cache_creation_tokens: 0,
        reasoning_tokens: 0,
    })
}

/// Inject the `openhuman_usage_meta` billing/context key into `raw` so
/// openhuman's crate-native cost pipeline recovers the backend-charged USD, then
/// return the augmented value. A local re-expression of openhuman's
/// `pub(crate)` `merge_openhuman_usage_meta` — identical field names + no-op
/// rule (both values zero ⇒ raw untouched) so billing-free responses stay clean.
fn inject_usage_meta(
    raw: serde_json::Value,
    charged_amount_usd: f64,
    context_window: u64,
) -> serde_json::Value {
    if charged_amount_usd <= 0.0 && context_window == 0 {
        return raw;
    }
    let meta = serde_json::json!({
        "charged_amount_usd": charged_amount_usd,
        "context_window": context_window,
    });
    match raw {
        serde_json::Value::Object(mut obj) => {
            obj.insert(OPENHUMAN_USAGE_META_KEY.to_string(), meta);
            serde_json::Value::Object(obj)
        }
        // A non-object raw can't hold the key alongside wire fields — stash the
        // meta on its own so the reader still recovers it.
        _ => serde_json::json!({ OPENHUMAN_USAGE_META_KEY: meta }),
    }
}

/// Parse an OpenAI-compatible chat-completion payload into a tinyagents
/// [`ModelResponse`], preserving token usage AND the managed billing envelope.
///
/// The full wire payload is kept on [`ModelResponse::raw`] (parity with the
/// crate `OpenAiModel`), and when the managed backend reports a charge the
/// `openhuman_usage_meta` key is injected so the host cost layer sees the USD
/// amount. Errors when the response carries no assistant text.
fn model_response_from_payload(payload: serde_json::Value) -> TaResult<ModelResponse> {
    let content = payload
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            TinyAgentsError::Model(
                "inference response missing choices[0].message.content".to_string(),
            )
        })?
        .to_string();

    let usage = parse_usage(&payload);
    // USD is only present on the managed envelope; the raw `/openai/v1`
    // passthrough bills backend-side and does not echo a charge.
    let charged_amount_usd = payload
        .pointer("/openhuman/billing/charged_amount_usd")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    let context_window = payload
        .pointer("/openhuman/usage/context_window")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    let mut response = ModelResponse::assistant(content);
    if let Some(u) = usage {
        response = response.with_usage(u);
    }
    // Keep the full wire payload on `raw` and re-project the managed billing
    // envelope so the host cost layer still sees the charge.
    response.raw = Some(inject_usage_meta(
        payload,
        charged_amount_usd,
        context_window,
    ));
    Ok(response)
}

/// Deterministic offline model for tests and offline harness wiring.
///
/// Every call returns a canned reply built from a fixed prefix and the last
/// user message, so a full chat cycle can be exercised without a network or a
/// live model. It never issues tool calls.
#[derive(Debug, Clone)]
pub struct MockProvider {
    /// Prefix prepended to the echoed user message in every reply.
    reply_prefix: String,
    /// Stable provider id surfaced to telemetry.
    provider_id: String,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new("mock: ")
    }
}

impl MockProvider {
    /// Builds a mock provider whose replies are `{prefix}{last_user_message}`.
    pub fn new(reply_prefix: impl Into<String>) -> Self {
        Self {
            reply_prefix: reply_prefix.into(),
            provider_id: "mock".to_string(),
        }
    }
}

#[async_trait]
impl ChatModel<()> for MockProvider {
    async fn invoke(&self, _state: &(), request: ModelRequest) -> TaResult<ModelResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m, Message::User(_)))
            .map(|m| m.text())
            .unwrap_or_default();
        Ok(ModelResponse::assistant(format!(
            "{}{}",
            self.reply_prefix, last_user
        )))
    }
}

impl HarnessModel for MockProvider {
    fn telemetry_provider_id(&self) -> String {
        self.provider_id.clone()
    }
}

/// Configuration for the hosted inference model.
#[derive(Clone, Default)]
pub struct HostedProviderConfig {
    /// Base URL of the OpenAI-compatible chat-completions API, e.g.
    /// `https://api.tinyhumans.ai/v1`. The provider POSTs to
    /// `{base_url}/chat/completions`.
    pub base_url: String,
    /// Bearer credential for the hosted brain. Empty string omits the header.
    pub api_key: String,
    /// Extra request headers to attach on every call (e.g. OpenRouter's
    /// `HTTP-Referer` / `X-Title` attribution headers).
    pub extra_headers: Vec<(String, String)>,
}

impl std::fmt::Debug for HostedProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let the credential land in a trace.
        f.debug_struct("HostedProviderConfig")
            .field("base_url", &self.base_url)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .field("extra_headers", &self.extra_headers)
            .finish()
    }
}

/// Hosted TinyHumans / Medulla inference model.
///
/// Speaks the OpenAI-compatible chat-completions wire format over HTTPS. This is
/// the only production inference path the harness ships — there is no local or
/// bring-your-own-model provider by design (spec non-goal "not a model host").
#[derive(Debug, Clone)]
pub struct HostedProvider {
    config: HostedProviderConfig,
    client: reqwest::Client,
}

impl HostedProvider {
    /// Builds a hosted provider from its endpoint configuration.
    pub fn new(config: HostedProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl ChatModel<()> for HostedProvider {
    /// Structured multi-turn chat — the path [`Agent::turn`] actually calls. The
    /// full history reaches the backend so multi-turn context survives, and the
    /// response's token/cost usage is parsed back out (the WS5 metering signal).
    ///
    /// [`Agent::turn`]: openhuman_core::openhuman::agent::Agent
    async fn invoke(&self, _state: &(), request: ModelRequest) -> TaResult<ModelResponse> {
        let messages = wire_messages(&request.messages);
        let model = request.model.as_deref().unwrap_or(DEFAULT_HOSTED_MODEL);
        let temperature = request.temperature.unwrap_or(0.0);

        let mut body = serde_json::json!({
            "model": model,
            "temperature": temperature,
            "messages": messages,
        });
        if let Some(cap) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(cap);
        }

        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut http = self.client.post(&url).json(&body);
        if !self.config.api_key.is_empty() {
            http = http.bearer_auth(&self.config.api_key);
        }
        for (name, value) in &self.config.extra_headers {
            http = http.header(name, value);
        }

        let response = http
            .send()
            .await
            .map_err(|e| TinyAgentsError::Model(format!("hosted inference request failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(TinyAgentsError::Model(format!(
                "hosted inference returned {status}: {text}"
            )));
        }

        let payload: serde_json::Value = response.json().await.map_err(|e| {
            TinyAgentsError::Model(format!("hosted inference response was not JSON: {e}"))
        })?;
        model_response_from_payload(payload)
    }
}

impl HarnessModel for HostedProvider {
    fn telemetry_provider_id(&self) -> String {
        "managed".to_string()
    }
}

/// A pure request plan — everything needed to issue one chat-completion call,
/// derived from a resolved [`InferenceDecl`] with no I/O. Split out so the tier
/// mapping, header injection, and empty-key handling are unit-testable without
/// a live backend.
#[derive(Debug)]
pub struct RequestPlan {
    /// The full POST URL (`{base_url}/chat/completions`).
    pub url: String,
    /// The concrete provider model id after tier mapping.
    pub model: String,
    /// The bearer credential, or `None` to omit the header (e.g. Ollama).
    pub bearer: Option<String>,
    /// Extra request headers (OpenRouter attribution) to attach.
    pub headers: Vec<(&'static str, String)>,
    /// The JSON request body.
    pub body: serde_json::Value,
}

/// Builds the [`RequestPlan`] for one turn against a tenant provider.
///
/// * The abstract tier (`chat-v1`, …) is mapped through the tenant
///   `[inference].models` table; an unmapped tier passes through verbatim.
/// * OpenRouter gets its mandatory `HTTP-Referer` / `X-Title` attribution
///   headers; other providers get none.
/// * An empty resolved key omits the bearer (the Ollama / keyless case).
pub fn request_plan(
    decl: &InferenceDecl,
    abstract_model: &str,
    messages: Vec<serde_json::Value>,
    temperature: f64,
    max_tokens: Option<u32>,
) -> RequestPlan {
    let model = decl
        .models
        .get(abstract_model)
        .cloned()
        .unwrap_or_else(|| abstract_model.to_string());
    let url = format!("{}/chat/completions", decl.base_url.trim_end_matches('/'));
    let key = decl.api_key().trim();
    let bearer = if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    };
    let headers = if decl.provider == "openrouter" {
        vec![
            ("HTTP-Referer", OPENROUTER_REFERER.to_string()),
            ("X-Title", OPENROUTER_TITLE.to_string()),
        ]
    } else {
        Vec::new()
    };
    let mut body = serde_json::json!({
        "model": model,
        "temperature": temperature,
        "messages": messages,
    });
    if let Some(cap) = max_tokens {
        body["max_tokens"] = serde_json::json!(cap);
    }
    RequestPlan {
        url,
        model,
        bearer,
        headers,
        body,
    }
}

/// Issues a prepared [`RequestPlan`] against `client`, returning the raw JSON
/// payload. Every error string is scrubbed of the bearer, so a credential can
/// never leak into a log line or an operator-visible message.
async fn send_plan(
    client: &reqwest::Client,
    plan: &RequestPlan,
) -> anyhow::Result<serde_json::Value> {
    let mut request = client.post(&plan.url).json(&plan.body);
    if let Some(bearer) = &plan.bearer {
        request = request.bearer_auth(bearer);
    }
    for (name, value) in &plan.headers {
        request = request.header(*name, value);
    }
    let scrub = |text: String| match &plan.bearer {
        Some(bearer) if !bearer.is_empty() => text.replace(bearer.as_str(), "<redacted>"),
        _ => text,
    };
    let response = request
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("inference request failed: {}", scrub(e.to_string())))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "inference returned {status}: {}",
            scrub(text)
        ));
    }
    response
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("inference response was not JSON: {}", scrub(e.to_string())))
}

/// The per-tenant inference model (issue #56 — BYOK).
///
/// Holds no baked configuration: on **every** [`invoke`](ChatModel::invoke) it
/// re-resolves the company's effective [`InferenceDecl`] from the secret store
/// (runtime override > manifest `[inference]` > managed env default). That
/// re-resolution is what makes a console provider switch take effect on the
/// agents' *next turn* with **no rebuild** — the roster and history survive; only
/// the outbound endpoint/model/credential change. Each turn maps the incoming
/// abstract tier through the tenant model table, injects OpenRouter's
/// attribution headers, and omits the bearer when the key is empty (Ollama).
pub struct TenantProvider {
    company: CompanyId,
    secrets: Arc<dyn SecretStore>,
    manifest: Inference,
    env_default: Option<EnvDefault>,
    client: reqwest::Client,
    /// The slug of the most recently resolved provider, so the synchronous
    /// [`telemetry_provider_id`](HarnessModel::telemetry_provider_id) reflects
    /// the config the last turn actually used (cost attribution follows the
    /// switch).
    slug: RwLock<&'static str>,
}

impl TenantProvider {
    /// Builds a tenant provider over `secrets`, the manifest `[inference]`
    /// section, and the optional managed env default.
    pub fn new(
        company: CompanyId,
        secrets: Arc<dyn SecretStore>,
        manifest: Inference,
        env_default: Option<EnvDefault>,
    ) -> Self {
        Self {
            company,
            secrets,
            manifest,
            env_default,
            client: reqwest::Client::new(),
            slug: RwLock::new("managed"),
        }
    }

    /// Re-resolves the effective config from the secret store and updates the
    /// cached telemetry slug. Errors when no provider is configured at all.
    async fn resolve(&self) -> anyhow::Result<InferenceDecl> {
        let decl = inference::resolve_effective(
            &self.company,
            &self.manifest,
            self.env_default.as_ref(),
            self.secrets.as_ref(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("resolving inference config: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no inference provider is configured for this company"))?;
        *self.slug.write().unwrap() = decl.telemetry_slug();
        Ok(decl)
    }
}

#[async_trait]
impl ChatModel<()> for TenantProvider {
    /// Structured multi-turn chat — the path [`Agent::turn`] calls. Re-resolves
    /// the effective config, then mirrors [`HostedProvider`]: full history
    /// reaches the backend and token/cost usage is parsed back out.
    ///
    /// [`Agent::turn`]: openhuman_core::openhuman::agent::Agent
    async fn invoke(&self, _state: &(), request: ModelRequest) -> TaResult<ModelResponse> {
        let decl = self
            .resolve()
            .await
            .map_err(|e| TinyAgentsError::Model(e.to_string()))?;
        let messages = wire_messages(&request.messages);
        let model = request.model.as_deref().unwrap_or(DEFAULT_HOSTED_MODEL);
        let temperature = request.temperature.unwrap_or(0.0);
        let plan = request_plan(&decl, model, messages, temperature, request.max_tokens);
        let payload = send_plan(&self.client, &plan)
            .await
            .map_err(|e| TinyAgentsError::Model(e.to_string()))?;
        model_response_from_payload(payload)
    }
}

impl HarnessModel for TenantProvider {
    fn telemetry_provider_id(&self) -> String {
        (*self.slug.read().unwrap()).to_string()
    }
}

/// A minimal live probe: one `ping` turn against the resolved config, used by
/// the console's "Test" button. The error is scrubbed of the credential by
/// [`send_plan`].
pub async fn probe(decl: &InferenceDecl) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let messages = vec![serde_json::json!({ "role": "user", "content": "ping" })];
    let plan = request_plan(decl, DEFAULT_HOSTED_MODEL, messages, 0.0, Some(16));
    let payload = send_plan(&client, &plan).await?;
    payload
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("probe response missing choices[0].message.content"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::config::MapEnv;

    /// Build a single-user-message request the way the harness turn does.
    fn user_request(message: &str) -> ModelRequest {
        ModelRequest {
            messages: vec![Message::user(message)],
            ..Default::default()
        }
    }

    #[test]
    fn env_config_prefers_specific_key_and_fills_defaults() {
        let env = MapEnv::new([("OPENCOMPANY_INFERENCE_KEY", "sk-specific")]);
        let (cfg, model) = harness_inference_from_env(&env).expect("configured");
        assert_eq!(cfg.api_key, "sk-specific");
        assert_eq!(cfg.base_url, DEFAULT_TINYHUMANS_INFERENCE_URL);
        // No explicit model → no roster-wide override (each agent keeps its tier).
        assert_eq!(model, None);
    }

    #[test]
    fn env_config_falls_back_to_tinyhumans_key_and_honors_overrides() {
        let env = MapEnv::new([
            ("TINYHUMANS_API_KEY", "sk-platform"),
            (
                "OPENCOMPANY_INFERENCE_URL",
                "https://staging-api.tinyhumans.ai/openai/v1",
            ),
            ("OPENCOMPANY_INFERENCE_MODEL", "reasoning-v1"),
        ]);
        let (cfg, model) = harness_inference_from_env(&env).expect("configured");
        assert_eq!(cfg.api_key, "sk-platform");
        assert_eq!(cfg.base_url, "https://staging-api.tinyhumans.ai/openai/v1");
        assert_eq!(model.as_deref(), Some("reasoning-v1"));
    }

    #[test]
    fn env_config_is_none_without_any_key() {
        let env = MapEnv::new([("OPENCOMPANY_INFERENCE_URL", "https://x/v1")]);
        assert!(harness_inference_from_env(&env).is_none());
    }

    // ---- media backend (issue #109) ---------------------------------------

    #[test]
    fn media_backend_prefers_specific_key_and_defaults_url() {
        let env = MapEnv::new([("OPENCOMPANY_MEDIA_KEY", "media-specific")]);
        let backend = media_backend_from_env(&env).expect("configured");
        assert_eq!(backend.auth_token, "media-specific");
        assert_eq!(backend.backend_url, DEFAULT_TINYHUMANS_MEDIA_BACKEND_URL);
    }

    #[test]
    fn media_backend_falls_back_to_tinyhumans_key_and_honors_url_override() {
        let env = MapEnv::new([
            ("TINYHUMANS_API_KEY", "platform-key"),
            (
                "OPENCOMPANY_MEDIA_BACKEND_URL",
                "https://staging-api.tinyhumans.ai",
            ),
        ]);
        let backend = media_backend_from_env(&env).expect("configured");
        assert_eq!(backend.auth_token, "platform-key");
        assert_eq!(backend.backend_url, "https://staging-api.tinyhumans.ai");
    }

    /// Fail-closed: no managed credential ⇒ no media backend, even when a URL is
    /// set. A tenant BYOK inference key must never stand in for the media token.
    #[test]
    fn media_backend_is_none_without_managed_key() {
        let env = MapEnv::new([("OPENCOMPANY_MEDIA_BACKEND_URL", "https://api.tinyhumans.ai")]);
        assert!(media_backend_from_env(&env).is_none());
    }

    #[tokio::test]
    async fn mock_provider_echoes_last_user_message_with_prefix() {
        let provider = MockProvider::new("reply: ");
        let out = provider.invoke(&(), user_request("hello")).await.unwrap();
        assert_eq!(out.text(), "reply: hello");
        assert_eq!(provider.telemetry_provider_id(), "mock");
    }

    #[tokio::test]
    async fn mock_provider_ignores_system_and_echoes_last_user() {
        let provider = MockProvider::default();
        let req = ModelRequest {
            messages: vec![Message::system("be terse"), Message::user("ping")],
            model: Some("any".to_string()),
            ..Default::default()
        };
        let out = provider.invoke(&(), req).await.unwrap();
        assert_eq!(out.text(), "mock: ping");
    }

    #[test]
    fn hosted_provider_reports_managed_telemetry_id() {
        let provider = HostedProvider::new(HostedProviderConfig {
            base_url: "https://example.test/v1".to_string(),
            api_key: String::new(),
            extra_headers: Vec::new(),
        });
        assert_eq!(provider.telemetry_provider_id(), "managed");
    }

    /// The exact `/openai/v1` staging response shape: reply text plus a standard
    /// `usage` block with a `prompt_tokens_details.cached_tokens` field and no
    /// openhuman billing envelope. No charge ⇒ no `openhuman_usage_meta` key.
    #[test]
    fn parses_openai_v1_completion_with_usage() {
        let payload = serde_json::json!({
            "model": "chat-v1",
            "choices": [{ "message": { "role": "assistant", "content": "pong" } }],
            "usage": {
                "prompt_tokens": 22,
                "completion_tokens": 2,
                "total_tokens": 24,
                "prompt_tokens_details": { "cached_tokens": 5 }
            }
        });
        let resp = model_response_from_payload(payload).expect("parses");
        assert_eq!(resp.text(), "pong");
        assert!(resp.message.tool_calls.is_empty());
        let usage = resp.usage.expect("usage present");
        assert_eq!(usage.input_tokens, 22);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cache_read_tokens, 5);
        // No billing envelope → raw carries the wire payload but no meta key.
        assert!(
            resp.raw
                .as_ref()
                .unwrap()
                .get(OPENHUMAN_USAGE_META_KEY)
                .is_none(),
            "billing-free response must not fabricate a charge"
        );
    }

    /// The managed envelope wins for cached tokens and carries the USD charge,
    /// which must survive onto `raw.openhuman_usage_meta.charged_amount_usd` so
    /// the host cost layer bills it. This is the #1 billing-preservation contract.
    #[test]
    fn managed_envelope_supplies_cost_and_cached_tokens() {
        let payload = serde_json::json!({
            "choices": [{ "message": { "content": "ok" } }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 40,
                "prompt_tokens_details": { "cached_tokens": 1 }
            },
            "openhuman": {
                "usage": { "cached_input_tokens": 64 },
                "billing": { "charged_amount_usd": 0.0123 }
            }
        });
        let resp = model_response_from_payload(payload).expect("parses");
        let usage = resp.usage.expect("usage present");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 40);
        assert_eq!(
            usage.cache_read_tokens, 64,
            "envelope beats prompt_tokens_details"
        );
        // The charged USD is re-projected onto raw for openhuman's cost pipeline.
        let charged = resp
            .raw
            .as_ref()
            .and_then(|raw| raw.get(OPENHUMAN_USAGE_META_KEY))
            .and_then(|meta| meta.get("charged_amount_usd"))
            .and_then(serde_json::Value::as_f64)
            .expect("charged_amount_usd survives onto raw");
        assert!(
            (charged - 0.0123).abs() < 1e-9,
            "charged_amount_usd must survive the billing envelope: {charged}"
        );
    }

    #[test]
    fn missing_content_is_an_error_and_no_usage_is_none() {
        let no_content = serde_json::json!({ "choices": [{ "message": {} }] });
        assert!(model_response_from_payload(no_content).is_err());

        let no_usage = serde_json::json!({
            "choices": [{ "message": { "content": "hi" } }]
        });
        let resp = model_response_from_payload(no_usage).expect("parses");
        assert!(resp.usage.is_none());
    }

    // ---- TenantProvider (issue #56 — BYOK) --------------------------------

    use std::collections::BTreeMap;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::company::Inference;
    use crate::ports::types::SecretValue;

    #[derive(Default)]
    struct MemSecrets {
        map: Mutex<HashMap<String, String>>,
    }

    #[async_trait]
    impl SecretStore for MemSecrets {
        async fn get(&self, _c: &CompanyId, key: &str) -> crate::Result<Option<SecretValue>> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(key)
                .map(|v| SecretValue(v.clone())))
        }
        async fn set(&self, _c: &CompanyId, key: &str, value: SecretValue) -> crate::Result<()> {
            self.map.lock().unwrap().insert(key.to_string(), value.0);
            Ok(())
        }
    }

    fn manifest_inference(provider: &str) -> Inference {
        Inference {
            provider: Some(provider.to_string()),
            base_url: None,
            api_key_secret: None,
            models: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn request_plan_maps_tier_and_injects_openrouter_headers() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let mut manifest = manifest_inference("openrouter");
        manifest.models =
            BTreeMap::from([("chat-v1".to_string(), "deepseek/deepseek-chat".to_string())]);
        inference::store_key(&company, &secrets, "or-key")
            .await
            .unwrap();
        let decl = inference::resolve_effective(&company, &manifest, None, &secrets)
            .await
            .unwrap()
            .unwrap();

        let plan = request_plan(&decl, "chat-v1", Vec::new(), 0.2, None);
        assert_eq!(
            plan.model, "deepseek/deepseek-chat",
            "tier maps through table"
        );
        assert_eq!(plan.bearer.as_deref(), Some("or-key"));
        assert!(plan.url.ends_with("/chat/completions"), "{}", plan.url);
        assert!(
            plan.headers
                .contains(&("HTTP-Referer", OPENROUTER_REFERER.to_string()))
        );
        assert!(
            plan.headers
                .contains(&("X-Title", OPENROUTER_TITLE.to_string()))
        );

        // An unmapped tier passes through unchanged.
        let passthrough = request_plan(&decl, "reasoning-v1", Vec::new(), 0.2, None);
        assert_eq!(passthrough.model, "reasoning-v1");
    }

    #[tokio::test]
    async fn request_plan_omits_bearer_for_keyless_ollama() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let mut manifest = manifest_inference("ollama");
        manifest.base_url = Some("http://localhost:11434/v1".into());
        let decl = inference::resolve_effective(&company, &manifest, None, &secrets)
            .await
            .unwrap()
            .unwrap();
        let plan = request_plan(&decl, "chat-v1", Vec::new(), 0.0, None);
        assert!(plan.bearer.is_none(), "keyless Ollama sends no bearer");
        assert!(plan.headers.is_empty(), "no OpenRouter headers for Ollama");
    }

    /// Spawns an in-process OpenAI-compatible stub that echoes `marker` as the
    /// completion content. The listener is bound before the task spawns, so the
    /// OS accepts connections into the backlog immediately.
    async fn spawn_stub(marker: &'static str) -> String {
        use axum::routing::post;
        use axum::{Json, Router};

        let app = Router::new().route(
            "/chat/completions",
            post(move || async move {
                Json(serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": marker } }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// The live-switch contract: the same `TenantProvider` instance routes turn
    /// 1 to stub A, then — after the operator flips the runtime override in the
    /// secret store — routes turn 2 to stub B, with **no rebuild** of the
    /// provider or the agent. This is what makes a console switch take effect on
    /// the next turn.
    #[tokio::test]
    async fn tenant_provider_live_switches_between_turns_without_rebuild() {
        let url_a = spawn_stub("reply-from-A").await;
        let url_b = spawn_stub("reply-from-B").await;

        let company = CompanyId::new("acme");
        let secrets: Arc<dyn SecretStore> = Arc::new(MemSecrets::default());
        let mut manifest = manifest_inference("openai_compatible");
        manifest.base_url = Some(url_a.clone());
        let provider = TenantProvider::new(company.clone(), secrets.clone(), manifest, None);

        // Turn 1 → stub A.
        let first = provider
            .invoke(&(), user_request("hi"))
            .await
            .expect("turn 1");
        assert_eq!(first.text(), "reply-from-A");
        assert_eq!(provider.telemetry_provider_id(), "byok");

        // Operator flips the provider to stub B via a runtime override — no
        // rebuild, just a secret-store write.
        inference::save_runtime_config(
            &company,
            secrets.as_ref(),
            &inference::RuntimeInference {
                provider: "openai_compatible".into(),
                base_url: Some(url_b.clone()),
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();

        // Turn 2 → stub B, same provider instance.
        let second = provider
            .invoke(&(), user_request("hi"))
            .await
            .expect("turn 2");
        assert_eq!(
            second.text(),
            "reply-from-B",
            "the switch took effect next turn"
        );
    }

    #[tokio::test]
    async fn tenant_provider_errors_when_nothing_is_configured() {
        let company = CompanyId::new("acme");
        let secrets: Arc<dyn SecretStore> = Arc::new(MemSecrets::default());
        let provider = TenantProvider::new(company, secrets, Inference::default(), None);
        let err = provider
            .invoke(&(), user_request("hi"))
            .await
            .expect_err("no provider configured");
        assert!(err.to_string().contains("no inference provider"), "{err}");
    }
}
