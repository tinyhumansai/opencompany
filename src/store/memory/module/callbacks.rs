//! Host-owned callback objects the loaded TinyMemory module calls back into.
//!
//! The module installs these during its own setup — before its store is
//! constructed — so all three must be served **and** named on the connection
//! before any `load_file_with_config`. Serving them late binds the module's
//! store to an inert provider with no error anywhere: the ordering is
//! load-bearing, and [`install`] is called from the host's `build_runtime`
//! for exactly that reason.
//!
//! The semantics here are deliberately narrower than openhuman's twin
//! (`vendor/openhuman/src/openhuman/modules/memory_host.rs`), because
//! opencompany brings no inference stack to the memory seam:
//!
//! - **EmbeddingHost** answers one empty vector per input text — behaviour-
//!   equivalent to the `NoopEmbedding` the incumbent `namespace` driver binds,
//!   keeping the module's recall on the same lexical footing that driver
//!   already documents. If the zero-dimension gate (#1524 Gate Q2) settles the
//!   other way, this is the one method that changes.
//! - **ChatHost** refuses with a stable wire name. The request parameter is
//!   deliberately `serde_json::Value`, not a typed model request: declaring
//!   the type would couple this seam to a `tinyagents` version for a callback
//!   whose answer is a refusal either way.
//! - **RuntimeHost** maps the module's events and error reports onto
//!   `tracing`, and answers NLP extraction with an empty response, logged once
//!   at INFO so the degradation is visible without being noisy.
//!
//! Every error string that leaves this file passes through [`scrub`]:
//! callback payloads carry user memory content, and an error that quotes its
//! input is an error that exfiltrates it into logs and wire messages.

use std::sync::OnceLock;

use tinybus::ObjectPath;
use tinymemory_api::host::{MemoryEvent, SpacyResponse};

const EMBEDDING_NAME: &str = "ai.tinyhumans.tinymemory.EmbeddingHost";
const EMBEDDING_PATH: &str = "/ai/tinyhumans/tinymemory/EmbeddingHost";
const CHAT_NAME: &str = "ai.tinyhumans.tinymemory.ChatHost";
const CHAT_PATH: &str = "/ai/tinyhumans/tinymemory/ChatHost";
const RUNTIME_NAME: &str = "ai.tinyhumans.tinymemory.RuntimeHost";
const RUNTIME_PATH: &str = "/ai/tinyhumans/tinymemory/RuntimeHost";

/// The most bytes of a scrubbed detail string that reach a log line or a
/// wire error. Same figure and same reasoning as the MCP probe's scrub seam:
/// enough for a status line and a summary, never a payload.
const SCRUB_MAX_BYTES: usize = 300;

/// Renders an error for a log line or wire message without carrying payload:
/// query strings dropped from anything URL-shaped, bearer-ish tokens redacted,
/// and the result truncated to [`SCRUB_MAX_BYTES`] on a char boundary.
fn scrub(detail: &str) -> String {
    // Bound the INPUT first: the word loop below copies a whole token before
    // the running-length check, so a huge whitespace-free input would be
    // copied in full before truncation. Four times the output cap leaves
    // room for redaction markers to shrink words while keeping the
    // allocation O(cap), not O(input).
    let detail = crate::store::text::slice_on_char_boundaries(detail, 0..SCRUB_MAX_BYTES * 4);
    let detail = detail.as_str();
    let mut out = String::with_capacity(detail.len().min(SCRUB_MAX_BYTES));
    let mut redact_next = false;
    for word in detail.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        let lowered = word.to_ascii_lowercase();
        // A scheme word marks the NEXT token as the credential ("Bearer
        // <token>"); inline shapes carry the secret in the same word.
        let credentialish = redact_next
            || lowered.starts_with("bearer")
            || lowered.contains("api_key")
            || lowered.contains("token=");
        redact_next = lowered.starts_with("bearer");
        if credentialish {
            out.push_str("[redacted]");
        } else if let Some(query) = word.find('?').filter(|_| word.contains("://")) {
            out.push_str(&word[..query]);
        } else {
            out.push_str(word);
        }
        if out.len() >= SCRUB_MAX_BYTES {
            break;
        }
    }
    crate::store::text::slice_on_char_boundaries(&out, 0..SCRUB_MAX_BYTES)
}

#[derive(Clone)]
struct EmbeddingCallbacks;

#[tinybus::interface(name = "ai.tinyhumans.tinymemory.EmbeddingHost")]
impl EmbeddingCallbacks {
    async fn embed(
        &self,
        _provider: String,
        _model: String,
        _dimensions: usize,
        texts: Vec<String>,
    ) -> tinybus::Result<Vec<Vec<f32>>> {
        // One EMPTY vector per input, exactly — a count mismatch is how an
        // embedder corrupts a store silently, so the shape contract holds
        // even though the vectors carry nothing.
        Ok(vec![Vec::new(); texts.len()])
    }
}

#[derive(Clone)]
struct ChatCallbacks;

#[tinybus::interface(name = "ai.tinyhumans.tinymemory.ChatHost")]
impl ChatCallbacks {
    async fn complete(
        &self,
        _role: String,
        _request: serde_json::Value,
    ) -> tinybus::Result<serde_json::Value> {
        Err(tinybus::Error::MethodFailed {
            name: "ai.tinyhumans.tinymemory.Error.ChatUnavailable".to_string(),
            message: "this host serves no chat model to the memory module; summarisation \
                      degrades to the module's non-LLM path"
                .to_string(),
        })
    }
}

#[derive(Clone)]
struct RuntimeCallbacks;

#[tinybus::interface(name = "ai.tinyhumans.tinymemory.RuntimeHost")]
impl RuntimeCallbacks {
    async fn publish_event(&self, event: MemoryEvent) -> tinybus::Result<()> {
        // opencompany has no consumer for the module's progress events yet;
        // visible beats silent, noisy loses to debug.
        tracing::debug!(?event, "[memory-module] event");
        Ok(())
    }

    async fn report_error(
        &self,
        classify_expected: bool,
        rendered: String,
        domain: String,
        operation: String,
        _tags: Vec<(String, String)>,
    ) -> tinybus::Result<()> {
        let detail = scrub(&rendered);
        if classify_expected {
            tracing::debug!(%domain, %operation, %detail, "[memory-module] expected error");
        } else {
            tracing::warn!(%domain, %operation, %detail, "[memory-module] error");
        }
        Ok(())
    }

    async fn extract_spacy(&self, _text: String) -> tinybus::Result<SpacyResponse> {
        static LOGGED: OnceLock<()> = OnceLock::new();
        LOGGED.get_or_init(|| {
            tracing::info!(
                "[memory-module] NLP extraction is not served by this host; entity and noun \
                 extraction degrade to empty (logged once)"
            );
        });
        Ok(SpacyResponse {
            entities: Vec::new(),
            nouns: Vec::new(),
        })
    }
}

/// Serves and names all three callback objects on the host connection, once.
///
/// # Errors
///
/// Returns an error when an object path is malformed (a bug in the constants
/// above, not the module) or when the connection refuses a registration —
/// which, for these fixed names, means something else in the process claimed
/// them first.
pub(super) async fn install(connection: &tinybus::Connection) -> tinybus::Result<()> {
    static INSTALLED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    INSTALLED
        .get_or_try_init(|| async move {
            connection
                .serve_at(ObjectPath::new(EMBEDDING_PATH)?, EmbeddingCallbacks)
                .await?;
            connection
                .serve_at(ObjectPath::new(CHAT_PATH)?, ChatCallbacks)
                .await?;
            connection
                .serve_at(ObjectPath::new(RUNTIME_PATH)?, RuntimeCallbacks)
                .await?;
            connection.request_name(EMBEDDING_NAME).await?;
            connection.request_name(CHAT_NAME).await?;
            connection.request_name(RUNTIME_NAME).await
        })
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::scrub;

    /// The scrub drops query strings, redacts credential-shaped words, and
    /// bounds the output — the three properties that make it safe to put an
    /// arbitrary error string on a log line.
    #[test]
    fn scrub_bounds_redacts_and_strips_queries() {
        let scrubbed = scrub("call to https://api.example.com/v1/embed?key=sekret failed");
        assert!(scrubbed.contains("https://api.example.com/v1/embed"));
        assert!(!scrubbed.contains("sekret"));

        let scrubbed = scrub("Bearer abc.def.ghi rejected");
        assert!(!scrubbed.contains("abc.def.ghi"));
        assert!(scrubbed.contains("[redacted]"));

        let long = "é".repeat(400);
        let scrubbed = scrub(&long);
        assert!(
            scrubbed.len() <= super::SCRUB_MAX_BYTES + 2,
            "{}",
            scrubbed.len()
        );
        assert!(scrubbed.chars().all(|c| c == 'é'), "no torn codepoint");
    }
}
