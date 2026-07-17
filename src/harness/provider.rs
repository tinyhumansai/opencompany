//! Inference [`Provider`] implementations for the embedded harness.
//!
//! openhuman's [`Provider`](oh::inference::provider::Provider) is a trait, so
//! opencompany brings its own implementation. Consistent with the spec non-goal
//! "not a model host", only two providers ship:
//!
//! * [`HostedProvider`] — talks to the hosted TinyHumans / Medulla brain over an
//!   OpenAI-compatible chat-completions endpoint. This is the sole production
//!   inference path; there is no local-LLM or BYO-model seam.
//! * [`MockProvider`] — a deterministic, offline provider used by tests (and by
//!   any caller that wants the harness wired without a network).
//!
//! `BrainMode` continues to select hosted vs echo at the runtime layer; the old
//! out-of-process `sidecar` mode is subsumed by this in-process harness.

use async_trait::async_trait;
use openhuman_core::openhuman as oh;

use oh::inference::provider::{ChatRequest, ChatResponse, Provider, UsageInfo};

use crate::app::config::EnvSource;

/// Default hosted inference endpoint when only a bare `TINYHUMANS_API_KEY` is
/// supplied — the OpenAI-compatible surface a company agent's `chat-v1` /
/// `reasoning-v1` / … workloads resolve against.
pub const DEFAULT_TINYHUMANS_INFERENCE_URL: &str = "https://api.tinyhumans.ai/openai/v1";

/// Default hosted model/tier when none is configured.
pub const DEFAULT_HOSTED_MODEL: &str = "chat-v1";

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
pub fn harness_inference_from_env(env: &dyn EnvSource) -> Option<(HostedProviderConfig, String)> {
    let api_key = env
        .get("OPENCOMPANY_INFERENCE_KEY")
        .or_else(|| env.get("TINYHUMANS_API_KEY"))?;
    let base_url = env
        .get("OPENCOMPANY_INFERENCE_URL")
        .unwrap_or_else(|| DEFAULT_TINYHUMANS_INFERENCE_URL.to_string());
    let model = env
        .get("OPENCOMPANY_INFERENCE_MODEL")
        .unwrap_or_else(|| DEFAULT_HOSTED_MODEL.to_string());
    Some((HostedProviderConfig { base_url, api_key }, model))
}

/// Deterministic offline [`Provider`] for tests and offline harness wiring.
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
    /// Builds a mock provider whose replies are `{prefix}{user_message}`.
    pub fn new(reply_prefix: impl Into<String>) -> Self {
        Self {
            reply_prefix: reply_prefix.into(),
            provider_id: "mock".to_string(),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn telemetry_provider_id(&self) -> String {
        self.provider_id.clone()
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(format!("{}{}", self.reply_prefix, message))
    }
}

/// Configuration for the hosted inference [`Provider`].
#[derive(Debug, Clone)]
pub struct HostedProviderConfig {
    /// Base URL of the OpenAI-compatible chat-completions API, e.g.
    /// `https://api.tinyhumans.ai/v1`. The provider POSTs to
    /// `{base_url}/chat/completions`.
    pub base_url: String,
    /// Bearer credential for the hosted brain. Empty string omits the header.
    pub api_key: String,
}

/// Hosted TinyHumans / Medulla inference [`Provider`].
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
impl Provider for HostedProvider {
    fn telemetry_provider_id(&self) -> String {
        "managed".to_string()
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let mut messages: Vec<serde_json::Value> = Vec::with_capacity(2);
        if let Some(system) = system_prompt {
            messages.push(serde_json::json!({ "role": "system", "content": system }));
        }
        messages.push(serde_json::json!({ "role": "user", "content": message }));

        let body = serde_json::json!({
            "model": model,
            "temperature": temperature,
            "messages": messages,
        });

        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut request = self.client.post(&url).json(&body);
        if !self.config.api_key.is_empty() {
            request = request.bearer_auth(&self.config.api_key);
        }

        let response = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("hosted inference request failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "hosted inference returned {status}: {text}"
            ));
        }

        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("hosted inference response was not JSON: {e}"))?;
        let content = payload
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("hosted inference response missing choices[0].message.content")
            })?;
        Ok(content.to_string())
    }

    /// Structured multi-turn chat — the path [`Agent::turn`](oh::agent::Agent)
    /// actually calls.
    ///
    /// Overriding `chat()` (rather than inheriting the trait default, which
    /// collapses the conversation to system + last-user and reports no usage)
    /// buys two things the harness needs: the **full history** reaches the
    /// backend so multi-turn context survives, and the response's token/cost
    /// **usage** is parsed back out — the signal the WS5 metering hook records.
    /// The optional `request.stream` sink is ignored: the harness consumes the
    /// aggregated reply, not the SSE deltas.
    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
            .collect();

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

        let response = http
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("hosted inference request failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "hosted inference returned {status}: {text}"
            ));
        }

        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("hosted inference response was not JSON: {e}"))?;
        chat_response_from_payload(&payload)
    }
}

/// Parse an OpenAI-compatible chat-completion payload into a [`ChatResponse`],
/// carrying token usage when the backend reports it.
fn chat_response_from_payload(payload: &serde_json::Value) -> anyhow::Result<ChatResponse> {
    let message = payload.pointer("/choices/0/message");
    let content = message
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!("hosted inference response missing choices[0].message.content")
        })?;
    Ok(ChatResponse {
        text: Some(content.to_string()),
        tool_calls: Vec::new(),
        usage: parse_usage(payload),
        reasoning_content: message
            .and_then(|m| m.get("reasoning_content"))
            .and_then(|c| c.as_str())
            .map(str::to_string),
    })
}

/// Extract token/cost usage from a chat-completion payload.
///
/// Reads the standard OpenAI `usage` block, and — when the hosted backend wraps
/// its metered totals in an `openhuman.{usage,billing}` envelope (the managed
/// path, not the raw `/openai/v1` passthrough) — prefers those richer figures
/// for cached-input tokens and the charged USD amount. Returns `None` when the
/// payload carries no `usage` block at all.
fn parse_usage(payload: &serde_json::Value) -> Option<UsageInfo> {
    let usage = payload.get("usage")?;
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    // Cached-input tokens: the openhuman envelope wins, else the standard
    // `prompt_tokens_details.cached_tokens`.
    let cached_input_tokens = payload
        .pointer("/openhuman/usage/cached_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(0);
    // USD is only present on the managed envelope; the raw `/openai/v1`
    // passthrough bills backend-side and does not echo a charge.
    let charged_amount_usd = payload
        .pointer("/openhuman/billing/charged_amount_usd")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    Some(UsageInfo {
        input_tokens,
        output_tokens,
        cached_input_tokens,
        charged_amount_usd,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::config::MapEnv;

    #[test]
    fn env_config_prefers_specific_key_and_fills_defaults() {
        let env = MapEnv::new([("OPENCOMPANY_INFERENCE_KEY", "sk-specific")]);
        let (cfg, model) = harness_inference_from_env(&env).expect("configured");
        assert_eq!(cfg.api_key, "sk-specific");
        assert_eq!(cfg.base_url, DEFAULT_TINYHUMANS_INFERENCE_URL);
        assert_eq!(model, "chat-v1");
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
        assert_eq!(model, "reasoning-v1");
    }

    #[test]
    fn env_config_is_none_without_any_key() {
        let env = MapEnv::new([("OPENCOMPANY_INFERENCE_URL", "https://x/v1")]);
        assert!(harness_inference_from_env(&env).is_none());
    }

    #[tokio::test]
    async fn mock_provider_echoes_with_prefix() {
        let provider = MockProvider::new("reply: ");
        let out = provider
            .chat_with_system(None, "hello", "chat-v1", 0.5)
            .await
            .unwrap();
        assert_eq!(out, "reply: hello");
        assert_eq!(provider.telemetry_provider_id(), "mock");
    }

    #[tokio::test]
    async fn mock_provider_ignores_system_and_model() {
        let provider = MockProvider::default();
        let out = provider
            .chat_with_system(Some("be terse"), "ping", "any", 0.0)
            .await
            .unwrap();
        assert_eq!(out, "mock: ping");
    }

    #[test]
    fn hosted_provider_reports_managed_telemetry_id() {
        let provider = HostedProvider::new(HostedProviderConfig {
            base_url: "https://example.test/v1".to_string(),
            api_key: String::new(),
        });
        assert_eq!(provider.telemetry_provider_id(), "managed");
    }

    /// The exact `/openai/v1` staging response shape: reply text plus a standard
    /// `usage` block with a `prompt_tokens_details.cached_tokens` field and no
    /// openhuman billing envelope.
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
        let resp = chat_response_from_payload(&payload).expect("parses");
        assert_eq!(resp.text.as_deref(), Some("pong"));
        assert!(resp.tool_calls.is_empty());
        let usage = resp.usage.expect("usage present");
        assert_eq!(usage.input_tokens, 22);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cached_input_tokens, 5);
        assert_eq!(usage.charged_amount_usd, 0.0);
    }

    /// The managed envelope wins for cached tokens and carries the USD charge.
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
        let usage = chat_response_from_payload(&payload)
            .expect("parses")
            .usage
            .expect("usage present");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 40);
        assert_eq!(
            usage.cached_input_tokens, 64,
            "envelope beats prompt_tokens_details"
        );
        assert_eq!(usage.charged_amount_usd, 0.0123);
    }

    #[test]
    fn missing_content_is_an_error_and_no_usage_is_none() {
        let no_content = serde_json::json!({ "choices": [{ "message": {} }] });
        assert!(chat_response_from_payload(&no_content).is_err());

        let no_usage = serde_json::json!({
            "choices": [{ "message": { "content": "hi" } }]
        });
        let resp = chat_response_from_payload(&no_usage).expect("parses");
        assert!(resp.usage.is_none());
    }
}
