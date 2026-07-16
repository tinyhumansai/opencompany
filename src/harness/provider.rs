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

use oh::inference::provider::Provider;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
