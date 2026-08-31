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

use std::sync::{Arc, LazyLock, OnceLock, RwLock};

use async_trait::async_trait;

use tinyagents::harness::message::{AssistantMessage, ContentBlock, Message};
use tinyagents::harness::model::{
    ChatModel, Modalities, ModelProfile, ModelRequest, ModelResponse, ToolChoice,
};
use tinyagents::harness::tool::{ToolCall, ToolSchema};
use tinyagents::harness::usage::Usage;
use tinyagents::{Result as TaResult, TinyAgentsError};

use crate::app::config::EnvSource;
use crate::company::Inference;
use crate::company::credentials::{Credential, TinyhumansTokenSource};
use crate::company::inference::{self, EnvDefault, InferenceDecl, InferenceSource};
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

    /// The model the most recent turn resolved to, folded onto the closed
    /// [`ModelSlug`] vocabulary (issue #1749) — the model half of the same
    /// attribution [`telemetry_provider_id`](Self::telemetry_provider_id)
    /// answers the provider half of.
    ///
    /// A [`ModelSlug`] rather than a `String` because the model name on the
    /// wire is operator-authored free text on any BYOK or `openai_compatible`
    /// deployment; see the [`model`](crate::metering::model) module docs. The
    /// raw name is classified **inside the implementation**, at the same place
    /// it is put on the wire, and never leaves it.
    ///
    /// `None` before the first turn (nothing has been resolved yet) and for an
    /// implementation with no model identity to give — which is why this has a
    /// default: a test double that reports a provider has nothing useful to say
    /// here, and `None` is the honest answer rather than a fabricated one.
    ///
    /// ## Read live, and therefore approximate under concurrency
    ///
    /// Read *after* the turn, exactly as `telemetry_provider_id` is, so a
    /// console BYOK or model-table switch re-attributes the next turn without a
    /// rebuild. The cost of that shape is the same one the provider slug already
    /// pays and is worth stating plainly: one company's agents share one
    /// provider, so when two agents on **different workload tiers** have turns
    /// in flight at once, the sample recorded second can read the slug the first
    /// one resolved. It mis-sorts tokens between two of that company's own
    /// slugs; it never crosses a company boundary, never changes a total, and
    /// never invents a model the company did not run. Making it exact needs the
    /// turn's own requested tier carried from the roster to the cost hook, which
    /// is a change to the agent record rather than to this seam.
    ///
    /// That bound — *two models the company actually ran* — is the whole reason
    /// implementations publish only after their call succeeds. A cache written
    /// before the request went out would widen the window to include a model
    /// that produced no usage at all (a turn still in flight, or one rejected
    /// with a 401), and a concurrent turn that *did* run would then be recorded
    /// against it. That is a different and worse error than mis-sorting between
    /// two live models, so it is one every implementation must not make.
    fn telemetry_model(&self) -> Option<crate::metering::ModelSlug> {
        None
    }
}

/// Resolve a [`HostedProvider`] configuration (and its default model) from the
/// environment, or `None` when no credential can be obtained.
///
/// Precedence, most specific first:
///
/// * credential — `OPENCOMPANY_INFERENCE_KEY` if set, else the platform token
///   source ([`TinyhumansTokenSource::from_env`]: a projected `TINYHUMANS_TOKEN_FILE`
///   ahead of a static `TINYHUMANS_API_KEY`). **Nothing configured ⇒ `None`**, and
///   the runtime keeps its offline echo brain.
/// * url — `OPENCOMPANY_INFERENCE_URL`, else [`DEFAULT_TINYHUMANS_INFERENCE_URL`].
/// * model — `OPENCOMPANY_INFERENCE_MODEL`, else [`DEFAULT_HOSTED_MODEL`].
///
/// `OPENCOMPANY_INFERENCE_KEY` is checked first because it is a *different*
/// credential — a per-tenant inference key an operator supplied — not the
/// platform's TinyHumans identity. Within the platform identity itself the
/// documented tier order (projected file over static key) applies.
pub fn harness_inference_from_env(
    env: &dyn EnvSource,
) -> Option<(HostedProviderConfig, Option<String>)> {
    let (credential, base_url) = hosted_endpoint_from_env(env)?;
    // The model is a per-roster **override** now: only an explicit
    // `OPENCOMPANY_INFERENCE_MODEL` flattens every agent to one workload. When
    // unset, each agent keeps its tier-derived model, which the tenant
    // `[inference].models` table then maps. `None` = no override.
    let model_override = env.get("OPENCOMPANY_INFERENCE_MODEL");
    Some((
        HostedProviderConfig {
            base_url,
            credential,
            extra_headers: Vec::new(),
        },
        model_override,
    ))
}

/// Resolve the shared hosted-endpoint `(credential, base_url)` pair every hosted
/// TinyHumans surface addresses — the **one** credential path both chat
/// inference ([`harness_inference_from_env`]) and embeddings
/// ([`hosted_embeddings_from_env`](crate::harness::embeddings::hosted_embeddings_from_env))
/// resolve against, so a rotation or a per-tenant key reaches both without a
/// second, drifting resolution.
///
/// Precedence mirrors the documented inference order, most specific first:
///
/// * credential — `OPENCOMPANY_INFERENCE_KEY` if set, else the platform token
///   source ([`TinyhumansTokenSource::from_env`]: a projected `TINYHUMANS_TOKEN_FILE`
///   ahead of a static `TINYHUMANS_API_KEY`). **Nothing configured ⇒ `None`.**
/// * url — `OPENCOMPANY_INFERENCE_URL`, else [`DEFAULT_TINYHUMANS_INFERENCE_URL`].
///
/// The embeddings client POSTs to `{base_url}/embeddings`, the chat client to
/// `{base_url}/chat/completions` — the same OpenAI-compatible surface.
pub(crate) fn hosted_endpoint_from_env(env: &dyn EnvSource) -> Option<(Credential, String)> {
    let credential = match env
        .get("OPENCOMPANY_INFERENCE_KEY")
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
    {
        Some(key) => Credential::from_value(key),
        None => Credential::from_source(Arc::new(TinyhumansTokenSource::from_env(env)?)),
    };
    let base_url = env
        .get("OPENCOMPANY_INFERENCE_URL")
        .unwrap_or_else(|| DEFAULT_TINYHUMANS_INFERENCE_URL.to_string());
    Some((credential, base_url))
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

/// Default managed-search backend base URL — the same tinyhumans backend that
/// owns the search-provider keys, billing and rate limiting (issue #238).
pub const DEFAULT_TINYHUMANS_SEARCH_BACKEND_URL: &str = "https://api.tinyhumans.ai";

/// Resolve the MANAGED web-search backend (issue #238) from the environment, or
/// `None` when no platform credential is present (fail-closed — no credential ⇒
/// no `web_search` tool is ever wired).
///
/// Precedence:
///
/// * credential — the shared platform token source
///   ([`TinyhumansTokenSource::from_env`]: a projected `TINYHUMANS_TOKEN_FILE`
///   ahead of a static `TINYHUMANS_API_KEY`). **Nothing configured ⇒ `None`.**
/// * url — `OPENCOMPANY_SEARCH_BACKEND_URL`, else
///   [`DEFAULT_TINYHUMANS_SEARCH_BACKEND_URL`].
///
/// Two deliberate differences from [`media_backend_from_env`]:
///
/// 1. **No `OPENCOMPANY_SEARCH_KEY`.** The #188 sign-off is explicit that
///    managed search rides the platform identity the way managed inference does
///    rather than acquiring a credential of its own. A per-surface key override
///    would be a second thing to rotate for no gain — the URL override is kept
///    because pointing at staging is a real need and carries no secret.
/// 2. **A [`Credential`], not a `String`.** Search resolves its bearer on the
///    request path, so a projected token that rotates mid-day keeps working with
///    no roster rebuild. Media flattens to a `String` at build time; that is a
///    known rough edge there, not a pattern worth copying.
///
/// **Security**: consults ONLY the environment — never a tenant secret store —
/// so a company can never point search at a key it controls.
pub fn search_backend_from_env(env: &dyn EnvSource) -> Option<super::search::SearchBackend> {
    let credential = Credential::from_source(Arc::new(TinyhumansTokenSource::from_env(env)?));
    let backend_url = env
        .get("OPENCOMPANY_SEARCH_BACKEND_URL")
        .unwrap_or_else(|| DEFAULT_TINYHUMANS_SEARCH_BACKEND_URL.to_string());
    Some(super::search::SearchBackend::new(
        backend_url,
        credential,
        crate::company::DEFAULT_SEARCH_DAILY_CALLS,
    ))
}

/// Which managed-platform surfaces resolved a credential at boot (issue #879).
///
/// Every surface below fails **closed** and independently: a tenant with no
/// platform credential still boots, still serves, and still builds agents — it
/// simply never wires `web_search`, never wires the media tools, and falls back
/// to whatever brain its manifest names. That is the right runtime behaviour and
/// the wrong operator experience: the only trace today is one `tracing::warn`
/// per agent at roster build, which nobody reads until a workflow 500s.
///
/// This is the boot-time summary of the same resolvers the rest of the module
/// exposes, so there is one place that answers "did this deployment come up with
/// a platform identity" and it cannot drift from what actually got wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformCredentialStatus {
    /// A platform identity resolved at all — [`TinyhumansTokenSource::from_env`]
    /// found a projected file or a static key.
    pub platform_identity: bool,
    /// That identity is the **projected-file** tier rather than a static key.
    pub projected_tier: bool,
    /// Managed chat inference and embeddings resolved
    /// ([`hosted_endpoint_from_env`]).
    pub inference: bool,
    /// Managed web search resolved ([`search_backend_from_env`]).
    pub search: bool,
    /// Managed media generation resolved ([`media_backend_from_env`]).
    pub media: bool,
}

impl PlatformCredentialStatus {
    /// Resolves every managed surface against one environment read.
    pub fn resolve(env: &dyn EnvSource) -> Self {
        let source = TinyhumansTokenSource::from_env(env);
        let projected_tier = source
            .as_ref()
            .is_some_and(|s| s.tier() == crate::company::credentials::TokenTier::ProjectedFile);
        Self {
            platform_identity: source.is_some(),
            projected_tier,
            inference: hosted_endpoint_from_env(env).is_some(),
            search: search_backend_from_env(env).is_some(),
            media: media_backend_from_env(env).is_some(),
        }
    }

    /// Every managed surface has a credential.
    pub fn all_wired(&self) -> bool {
        self.inference && self.search && self.media
    }

    /// The one line an operator needs at boot, or `None` when nothing is
    /// missing.
    ///
    /// Two shapes, because they call for two different actions:
    ///
    /// * **No platform identity at all** — the hosted tenant was provisioned
    ///   without its projected token volume. Names both tiers, since the fix
    ///   differs between a cluster tenant and `docker compose`.
    /// * **A projected identity that media cannot use** — the deployment *does*
    ///   have a platform token and search/inference are live, but
    ///   [`media_backend_from_env`] reads only the static tier
    ///   ([`API_KEY_ENV`](crate::company::credentials::API_KEY_ENV)), never the
    ///   projected file. Without this arm an operator who has just fixed a
    ///   missing-token incident sees media still reporting "Awaiting credential"
    ///   with nothing anywhere saying why. The underlying asymmetry is not
    ///   fixable here — the upstream media client takes a `String` bearer for
    ///   the life of the process, so flattening a 600-second projected token
    ///   into it would trade "never works" for "works for ten minutes" — so the
    ///   deployment is told, precisely, what to set instead.
    pub fn boot_warning(&self) -> Option<String> {
        use crate::company::credentials::{API_KEY_ENV, TOKEN_FILE_ENV};

        if !self.platform_identity && !self.inference && !self.media {
            return Some(format!(
                "no platform credential resolved: managed inference, embeddings, web_search and \
                 media generation are ALL unwired for every company on this deployment \
                 (fail-closed). A hosted tenant expects the platform-projected token volume named \
                 by {TOKEN_FILE_ENV}; a local or self-hosted instance expects a static \
                 {API_KEY_ENV}."
            ));
        }

        if self.projected_tier && !self.media {
            return Some(format!(
                "the platform identity is a projected token file ({TOKEN_FILE_ENV}), which media \
                 generation does not read: media tools stay unwired even for a company that grants \
                 `media`. Set OPENCOMPANY_MEDIA_KEY (or a static {API_KEY_ENV}) to wire them."
            ));
        }

        let mut unwired: Vec<&str> = Vec::new();
        if !self.inference {
            unwired.push("managed inference/embeddings");
        }
        if !self.search {
            unwired.push("web_search");
        }
        if !self.media {
            unwired.push("media generation");
        }
        if unwired.is_empty() {
            return None;
        }
        Some(format!(
            "platform credential is only partly configured: {} unwired (fail-closed). See \
             {TOKEN_FILE_ENV} / {API_KEY_ENV}.",
            unwired.join(", ")
        ))
    }
}

/// Flatten a tinyagents request's messages into the OpenAI-compatible wire
/// `[{role, content, …}]` array.
///
/// This preserves the two fields native tool calling round-trips on (dropping
/// either strands a multi-turn tool loop): an assistant turn's `tool_calls`
/// (`{id, type:"function", function:{name, arguments}}`, `arguments` a JSON
/// **string** per the OpenAI contract) and a tool-result turn's `tool_call_id`.
/// A tool-call-only assistant turn carries `content: null` (OpenAI's shape).
/// Mirrors tinyagents' own `openai::convert::translate_message`.
fn wire_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    messages.iter().map(wire_message).collect()
}

/// Translate one message into its OpenAI wire object. Split out so the
/// assistant/tool arms stay readable.
fn wire_message(message: &Message) -> serde_json::Value {
    match message {
        Message::System(_) => serde_json::json!({ "role": "system", "content": message.text() }),
        Message::User(_) => serde_json::json!({ "role": "user", "content": message.text() }),
        Message::Assistant(assistant) => {
            let text = message.text();
            let mut obj = serde_json::Map::new();
            obj.insert("role".to_string(), serde_json::json!("assistant"));
            // OpenAI accepts (and expects) a null content on a tool-call-only turn.
            if text.is_empty() && !assistant.tool_calls.is_empty() {
                obj.insert("content".to_string(), serde_json::Value::Null);
            } else {
                obj.insert("content".to_string(), serde_json::json!(text));
            }
            if !assistant.tool_calls.is_empty() {
                obj.insert(
                    "tool_calls".to_string(),
                    serde_json::Value::Array(
                        assistant.tool_calls.iter().map(wire_tool_call).collect(),
                    ),
                );
            }
            serde_json::Value::Object(obj)
        }
        Message::Tool(tool) => serde_json::json!({
            "role": "tool",
            "tool_call_id": tool.tool_call_id,
            "content": message.text(),
        }),
    }
}

/// Render one assistant [`ToolCall`] as an OpenAI `tool_calls[]` entry. OpenAI
/// requires `function.arguments` to be a JSON **string**, not an object.
fn wire_tool_call(call: &ToolCall) -> serde_json::Value {
    serde_json::json!({
        "id": call.id,
        "type": "function",
        "function": {
            "name": call.name,
            "arguments": serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string()),
        },
    })
}

/// Render the exposed [`ToolSchema`] set into the OpenAI `tools[]` array.
/// Returns an empty vec when no tools are exposed, so the caller can omit the
/// `tools`/`tool_choice` keys entirely (a bare chat turn stays byte-identical).
fn wire_tools(tools: &[ToolSchema]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|schema| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": schema.name,
                    "description": schema.description,
                    "parameters": schema.parameters,
                },
            })
        })
        .collect()
}

/// Translate a tinyagents [`ToolChoice`] into the OpenAI `tool_choice` wire
/// value. Mirrors tinyagents' `openai::convert::translate_tool_choice`.
fn wire_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::json!("auto"),
        ToolChoice::None => serde_json::json!("none"),
        ToolChoice::Required => serde_json::json!("required"),
        ToolChoice::Tool(name) => serde_json::json!({
            "type": "function",
            "function": { "name": name },
        }),
    }
}

/// Attach `tools` + `tool_choice` to a chat-completion `body` when any tools are
/// exposed. `tool_choice` is only meaningful alongside a non-empty `tools`
/// array, so both are omitted together when the turn exposes no tools.
fn attach_tools(
    body: &mut serde_json::Value,
    tools: Vec<serde_json::Value>,
    tool_choice: &ToolChoice,
    supports_parallel_control: bool,
) {
    if tools.is_empty() {
        return;
    }
    body["tool_choice"] = wire_tool_choice(tool_choice);
    body["tools"] = serde_json::Value::Array(tools);
    // Profile metadata is local; put the turn-boundary promise on the actual
    // OpenAI-compatible request so the remote model cannot validly emit an
    // effectful sibling beside `request_approval`.
    if supports_parallel_control {
        body["parallel_tool_calls"] = serde_json::Value::Bool(false);
    }
}

// ## Guarding intra-turn history growth
//
// Turn limits bound how long a turn can run, but not how large its history can
// grow. Each model call includes the preceding history again, so a turn that
// runs many tool iterations can repeatedly resend an increasingly large input.
//
// openhuman already provides `ContextCompressionMiddleware` (summarization at
// 90% of the window) and `ImageAwareMessageTrimMiddleware` (deterministic
// trimming as a fallback), but installs them only behind this gate in
// `vendor/openhuman/.../tinyagents/mod.rs:2216`:
//
// ```text
// if let Some(window) = context_window.filter(|w| *w > 0) { … }
// ```
//
// On this `direct_model` path, `effective_context_window` obtains that value as
// `direct.profile().and_then(|p| p.max_input_tokens)`. `MANAGED_PROFILE` did not
// set the field, so it returned `None` and neither middleware was installed.
//
// ### Failure mode observed at the provider boundary
//
// A large-context provider was measured on 2026-08-15, using one request per
// measurement:
//
// ```text
//  18,281 input tokens  → HTTP 200, finish_reason "stop",   response "1"
// 245,781 input tokens  → HTTP 200, finish_reason "stop",   response "1"
// 280,781 input tokens  → HTTP 200, finish_reason "stop",   response "1"
// 350,781 input tokens  → HTTP 200, finish_reason "stop",   response "1"
// ~438,000 input tokens → HTTP 200, finish_reason "failed", response "",
//                         usage {prompt_tokens: 0, completion_tokens: 0}
// ```
//
// This is silent failure rather than a provider error: HTTP remains successful,
// the response is empty, and usage is zero. A generic empty-response path then
// handles the failure, while a token budget cannot observe the oversized call.
//
// ### Default derivation
//
// The 240,000-token default is intended for large-context models and remains
// configurable. A 272,000-token advertised window provides a representative
// lower bound for a 272k-class combined model, rather than relying on whichever
// backing model happens to accept a larger request.
//
// Two margins apply:
//
// 1. `estimate_text_tokens` estimates tokens as `bytes / 4`. In the measured
//    sample, 61,299 bytes represented 18,281 tokens, or 3.35 bytes per token.
//    `bytes / 4` estimates 15,325 tokens, 16% below the actual count; the actual
//    count is therefore approximately 1.19 times the estimate.
// 2. Compression and deterministic trimming activate at 90% of the configured
//    window (`SUMMARIZE_THRESHOLD_FRACTION` and `window - window / 10`).
//
// 272,000 / 1.19 ≈ 228,000 tokens of safe estimated budget; dividing by 0.9
// gives approximately 253,000. Rounding down to 240,000 starts compression at
// approximately 216,000 estimated tokens, or approximately 258,000 actual
// tokens under the measured ratio, about 5% below the advertised 272,000-token
// window.

/// Configurable context-window default, in tokens, for managed inference.
///
/// This default suits large-context models. Set `OPENCOMPANY_CONTEXT_WINDOW` to
/// the provider's advertised window with an appropriate estimation margin when
/// using a smaller model. Set it to `off` or `0` to restore the previous
/// unbounded behavior.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 240_000;

/// Read and trim an environment variable.
fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
}

/// Return the context window advertised by the managed model profile, or
/// `None` when context compression and trimming are disabled.
///
/// `MANAGED_PROFILE` is shared by `HostedProvider` and `TenantProvider`, so this
/// is currently one value for every configured model. Per-model values would
/// require `profile()` to know the asynchronously resolved `InferenceDecl` and
/// are outside the scope of this fix.
pub fn context_window() -> Option<u64> {
    static VALUE: OnceLock<Option<u64>> = OnceLock::new();
    *VALUE.get_or_init(|| {
        let selected = match env_string("OPENCOMPANY_CONTEXT_WINDOW") {
            None => Some(DEFAULT_CONTEXT_WINDOW),
            Some(raw) if raw.eq_ignore_ascii_case("off") || raw == "0" => None,
            Some(raw) => match raw.parse::<u64>() {
                Ok(value) if value > 0 => Some(value),
                _ => {
                    eprintln!(
                        "context window: OPENCOMPANY_CONTEXT_WINDOW='{raw}' is not a positive \
                         integer; using the default of {DEFAULT_CONTEXT_WINDOW} tokens"
                    );
                    Some(DEFAULT_CONTEXT_WINDOW)
                }
            },
        };
        match selected {
            // Compression starts at 90% of the advertised window. Reporting
            // both values distinguishes the activation threshold from the hard
            // model limit.
            Some(value) => eprintln!(
                "context window: {value} tokens; compression starts at approximately {} \
                 estimated tokens",
                value / 10 * 9
            ),
            None => eprintln!(
                "context window: disabled; no compression or trimming, so a long turn may \
                 grow until the provider rejects or silently fails it"
            ),
        }
        selected
    })
}

/// The capability profile the hosted / tenant managed inference surface
/// advertises. `tool_calling: true` is the load-bearing bit: openhuman's turn
/// loop derives `native_tools` from the injected model's profile
/// (`ProfileOverrideModel` → `native_tools = profile.tool_calling`), so without
/// this the harness falls back to prompt-guided XML tool calls and a model that
/// narrates prose instead of emitting the exact `<tool_call>` tag never runs a
/// tool. Mirrors the shape openhuman's own `OpenHumanBackendModel` uses against
/// the identical `/openai/v1` backend.
static MANAGED_PROFILE: LazyLock<ModelProfile> = LazyLock::new(|| ModelProfile {
    provider: Some("openrouter".to_string()),
    modalities: Modalities {
        image_in: true,
        ..Modalities::default()
    },
    tool_calling: true,
    // An explicit approval request is a turn boundary. Asking the provider for
    // at most one native tool call prevents a sibling effect from being emitted
    // in the same assistant message and running before the operator sees the
    // request. The policy queue adds a second serial-execution barrier for a
    // provider that violates this capability contract.
    parallel_tool_calls: false,
    // This field activates both `ContextCompressionMiddleware` and
    // `ImageAwareMessageTrimMiddleware`. `TurnModels::effective_context_window`
    // reads `direct.profile().and_then(|p| p.max_input_tokens)`, and the turn
    // harness installs those middlewares only for a positive value.
    //
    // Leaving this as `None` permits unbounded history growth during a turn that
    // runs many tool iterations. The observed failure at approximately 438k
    // input tokens was HTTP 200 with `finish_reason: "failed"`, an empty message,
    // and zero usage rather than a diagnosable provider error. The configurable
    // 240,000-token default and its 272,000/1.19/0.9 rationale are documented
    // above; `OPENCOMPANY_CONTEXT_WINDOW=off` restores the previous behavior.
    max_input_tokens: context_window(),
    ..ModelProfile::default()
});

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

/// Parse the OpenAI `choices[0].message.tool_calls[]` array into tinyagents
/// [`ToolCall`]s. `function.arguments` arrives as a JSON **string**, which is
/// parsed back into a value; an unparseable blob is preserved verbatim and the
/// call is flagged [`ToolCall::invalid`] (mirroring tinyagents' tolerance of
/// small-model defects) rather than dropped, so the loop can feed the error
/// back to the model instead of stalling on a never-resolving call. A missing
/// or empty `id` is back-filled with a stable `tool-{index}` slot id so the
/// tool result can still correlate.
fn parse_tool_calls(payload: &serde_json::Value) -> Vec<ToolCall> {
    let Some(raw_calls) = payload
        .pointer("/choices/0/message/tool_calls")
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    raw_calls
        .iter()
        .enumerate()
        .filter_map(|(index, call)| {
            let name = call.pointer("/function/name").and_then(|v| v.as_str())?;
            let id = call
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("tool-{index}"));
            let raw_args = call
                .pointer("/function/arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let (arguments, invalid) = match serde_json::from_str::<serde_json::Value>(raw_args) {
                Ok(value) => (value, None),
                // Empty arguments are the no-arg case, not a defect.
                Err(_) if raw_args.trim().is_empty() => (serde_json::json!({}), None),
                Err(e) => (
                    serde_json::Value::String(raw_args.to_string()),
                    Some(format!("unparseable tool-call arguments: {e}")),
                ),
            };
            Some(ToolCall {
                id,
                name: name.to_string(),
                arguments,
                invalid,
            })
        })
        .collect()
}

/// Extract the visible text from an OpenAI-compatible `content`-shaped field.
///
/// The field may be either a plain string (`"hi"`) or an array of content
/// parts (`[{"type":"text","text":"hi"},…]`) — some providers, and reasoning
/// models on their `reasoning` field, use the array form. Concatenates the
/// `text` of every text part; a part counts as text when its `type` is `"text"`
/// or absent (but a `text` field is present). Returns an empty string when the
/// value is `null`, absent, or carries no text.
fn extract_content_text(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                let is_text = part
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| t == "text")
                    .unwrap_or(true);
                if is_text && let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    out.push_str(text);
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// Find a refusal encoded as an array-of-parts `content` part
/// (`{"type":"refusal","refusal":"…"}`) rather than the scalar sibling
/// `message.refusal` field.
///
/// `extract_content_text` only concatenates `"text"`-typed parts, so a
/// refusal part in the same array is silently dropped and never reaches
/// visible `content` — it must be recovered separately so the
/// reasoning-fallback guard can still detect it and refuse to promote
/// leaked reasoning over it. Concatenates every nonempty refusal part in
/// order — mirroring how `extract_content_text` concatenates every
/// `"text"`-typed part rather than stopping at the first, since a provider
/// splitting a refusal across multiple parts is otherwise silently
/// truncated to just the first fragment. Returns `None` when the value
/// isn't an array or carries no refusal part.
fn extract_array_refusal_text(value: Option<&serde_json::Value>) -> Option<String> {
    let parts = value?.as_array()?;
    let mut out = String::new();
    for part in parts {
        let is_refusal = part.get("type").and_then(|t| t.as_str()) == Some("refusal");
        if !is_refusal {
            continue;
        }
        if let Some(refusal) = part.get("refusal").and_then(|r| r.as_str()) {
            out.push_str(refusal);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Parse an OpenAI-compatible chat-completion payload into a tinyagents
/// [`ModelResponse`], preserving token usage, native tool calls, AND the managed
/// billing envelope.
///
/// The full wire payload is kept on [`ModelResponse::raw`] (parity with the
/// crate `OpenAiModel`), and when the managed backend reports a charge the
/// `openhuman_usage_meta` key is injected so the host cost layer sees the USD
/// amount. `content` is **optional**: a tool-call-only turn carries `content:
/// null`. Errors only when the response carries neither text nor a tool call.
fn model_response_from_payload(payload: serde_json::Value) -> TaResult<ModelResponse> {
    // Content may be a plain string OR an array of `{type:"text",text:…}`
    // parts; tolerate both.
    let raw_content = payload.pointer("/choices/0/message/content");
    let content_is_null = raw_content.is_some_and(serde_json::Value::is_null);
    let mut content = extract_content_text(raw_content);
    let tool_calls = parse_tool_calls(&payload);
    if tool_calls.len() > 1
        && tool_calls
            .iter()
            .any(|call| call.name == crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND)
    {
        return Err(TinyAgentsError::Model(
            "inference returned request_approval with sibling tool calls; the whole batch was \
             refused so the approval boundary cannot be crossed"
                .to_string(),
        ));
    }
    let finish_reason = payload
        .pointer("/choices/0/finish_reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Reasoning-model fallback: a reasoning-only turn returns `content: null`
    // with the visible text under `reasoning` / `reasoning_content` (string or
    // array-of-parts). Recover it so the turn is not lost to a hard error —
    // but only when the model actually finished. Any finish reason other than
    // a true completion (`length` truncation, `content_filter`, `failed` —
    // the documented HTTP-200-empty-response silent failure, see
    // docs/spec/runtime/providers.md — or any other/unknown value) means the
    // chain of thought itself may be unfinished, so promoting it here would
    // hand downstream consumers a partial or incorrect thought as if it were
    // the final answer. Allow-list the known-good completions instead of
    // blocklisting the failures we happened to think of, so an unrecognized
    // failure reason fails closed. Fall through to the empty-response error
    // below otherwise.
    // Only `stop` means "finished, with prose, asking for nothing else".
    // `tool_calls` and `function_call` were in this list until PR #1779's
    // review: both assert the model requested an ACTION, so a response
    // carrying one of them has not produced a final answer at all — whether
    // or not the call body parses. Promoting a chain of thought over a
    // requested action is the same class of substitution the truncation
    // guard below prevents, so they are excluded here rather than handled by
    // a special case per payload shape.
    let genuinely_finished = matches!(finish_reason.as_deref(), Some("stop"));
    // `tool_calls` above is the *parsed* result: `parse_tool_calls` requires a
    // `/message/tool_calls` array AND drops any entry missing `function.name`,
    // and it never reads the legacy singular `message.function_call` field at
    // all. So a malformed tool-call entry, or a legacy `finish_reason:
    // "function_call"` response using `message.function_call`, leaves the
    // parsed `tool_calls` empty even though the model requested an action —
    // which would let this branch silently swap the requested action for
    // ordinary prose instead of surfacing the parse/empty error below. Check
    // the *raw* payload for either call shape, independent of finish_reason,
    // so a genuinely-requested-but-unparseable call can never be promoted
    // (Codex review on #1779, comment 3862781739).
    let raw_tool_call_requested = payload
        .pointer("/choices/0/message/tool_calls")
        .is_some_and(|v| match v {
            serde_json::Value::Null => false,
            serde_json::Value::Array(arr) => !arr.is_empty(),
            // A present-but-non-array value (e.g. an object) is not a shape
            // `parse_tool_calls` or the legacy `function_call` check can
            // recognize, but it is not an absence either — fail closed
            // rather than let it read as "nothing requested" and fall
            // through to the reasoning fallback below.
            _ => true,
        })
        || payload
            .pointer("/choices/0/message/function_call")
            .is_some_and(|v| !v.is_null());
    // How many entries the *raw* array actually carried, when it is an
    // array at all (legacy `function_call` and non-array shapes have no
    // raw count to compare against, and are already fully covered by the
    // `tool_calls.is_empty()` arm below since `parse_tool_calls` only reads
    // the array shape). Used to catch a *partial* parse: `parse_tool_calls`
    // silently drops any entry missing `function.name` (it is a
    // `filter_map`), so a raw array of one valid call and one malformed one
    // survives parsing as a single-element `tool_calls` — nonempty, so the
    // `tool_calls.is_empty()` check alone does not fire, and the malformed
    // entry (a genuinely requested action) is discarded without a trace
    // (CodeRabbit review on #1779, comment 3877118065).
    let raw_tool_call_count = payload
        .pointer("/choices/0/message/tool_calls")
        .and_then(|v| v.as_array())
        .map(std::vec::Vec::len);
    // `finish_reason` can assert an action was requested (`tool_calls`, or
    // the legacy `function_call` value) even when there is no raw call body
    // for `raw_tool_call_requested` above to find at all — it only reads
    // "requested" off a *present, nonempty* `tool_calls` array or a present
    // legacy `function_call` field, so a missing field or an explicit empty
    // `tool_calls: []` both read as "nothing requested" to it. When
    // `content` is also empty this self-corrects anyway, via the
    // content-and-tool_calls-both-empty catch-all below. But array-shaped
    // `content` can carry a genuinely nonempty text preamble on its own —
    // no `reasoning` fallback involved — which makes that catch-all a
    // no-op too, so the response would return successfully with just the
    // preamble and no tool call, silently dropping the action the finish
    // reason itself declared (CodeRabbit review on #1779, comment
    // 3877608728).
    let finish_reason_declares_action = matches!(
        finish_reason.as_deref(),
        Some("tool_calls") | Some("function_call")
    );
    // A tool call was genuinely requested — either the raw payload carries
    // one (whether or not `parse_tool_calls` accepted it), or the finish
    // reason alone asserts one — but not every entry survived parsing:
    // either none did, or some did and some were silently dropped. This
    // must error even when `content` is nonempty: array-shaped content can
    // carry a text preamble alongside the malformed (or entirely missing)
    // call, which would otherwise pass the empty-turn check below and let
    // the harness silently return the preamble as if it were the whole
    // answer — the same class of substitution the reasoning-fallback guard
    // above exists to prevent, just via the *content* channel instead of
    // `reasoning` (CodeRabbit review on #1779, comments 3872084060 and
    // 3877608728).
    if (raw_tool_call_requested || finish_reason_declares_action)
        && (tool_calls.is_empty() || raw_tool_call_count.is_some_and(|n| n != tool_calls.len()))
    {
        let detail = finish_reason
            .as_deref()
            .map(|r| format!(" (finish_reason: {r})"))
            .unwrap_or_default();
        return Err(TinyAgentsError::Model(format!(
            "inference response requested a tool call that failed to parse{detail}"
        )));
    }
    // Resolve the refusal, independent of `content`'s shape, ONCE: a
    // provider can express it as the scalar sibling `message.refusal` field,
    // or — some providers/gateways normalize a Responses-API-style refusal
    // this way — as a `{"type":"refusal","refusal":"…"}` part inside
    // array-shaped `content` itself. `extract_content_text` only
    // concatenates `"text"`-typed parts, so a refusal-typed part (alone or
    // alongside a text part) never reaches `content` and `content` can
    // already be nonempty (a leaked lead-in sentence) by the time this runs.
    // The scalar field wins when a payload somehow carries both; either one
    // alone is still the provider's own visible safety response and must be
    // detected regardless of what shape `content` took (Codex reviews on
    // #1779, comments 3874381270, 3875001349, 3875101974).
    let refusal_text = payload
        .pointer("/choices/0/message/refusal")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| extract_array_refusal_text(payload.pointer("/choices/0/message/content")));

    if tool_calls.is_empty() && !raw_tool_call_requested {
        if let Some(refusal) = refusal_text {
            // A refusal is a *completed* decision, not a partial one — unlike
            // the reasoning fallback below, its precedence must not depend on
            // `finish_reason`. Gating it on `genuinely_finished` let a
            // refusal that ends with e.g. `finish_reason: "content_filter"`
            // (arguably the *more* likely finish reason for an actual
            // content-policy refusal) fall through untouched, leaving
            // whatever text or reasoning leaked alongside it to win instead
            // and silently discard the refusal (Codex review on #1779,
            // comment 3875167298). It always wins over leaked text/reasoning,
            // independent of how the turn finished.
            content = refusal;
        } else if finish_reason.as_deref() == Some("failed") && !content.is_empty() {
            // `finish_reason: "failed"` is the documented HTTP-200-empty-
            // response silent provider failure (docs/spec/runtime/providers.md).
            // It is a *completed* disclaimer that the turn did not succeed —
            // like a refusal, not a partial/unfinished state — so it must not
            // be overridden by whatever text leaked alongside it, the same
            // way `genuinely_finished` already keeps a truncated/filtered/
            // failed *reasoning* stream from being promoted below. That gate
            // only covers the `reasoning` fallback though: `content` itself is
            // extracted unconditionally at the top of this function (string OR
            // array-shaped), so a provider that emits real text — a leaked
            // lead-in sentence, or a fuller partial reply — before reporting
            // `failed` had that text returned as a successful answer with no
            // finish_reason check at all. Discard it here so the response
            // falls through to the empty-turn error below, naming `failed` for
            // diagnosis (CodeRabbit review on #1779, comment 3878355364).
            content.clear();
        } else if genuinely_finished && content_is_null && content.is_empty() {
            // Reasoning-model fallback: a reasoning-only turn returns
            // `content: null` with the visible text under `reasoning` /
            // `reasoning_content` (string or array-of-parts). Only promote it
            // when the model actually finished — a truncated (`length`),
            // filtered (`content_filter`), failed, or otherwise-unfinished
            // chain of thought is not a final answer, and promoting it here
            // would hand downstream consumers a partial or incorrect thought
            // as if it were.
            //
            // `content.is_empty()` alone is not enough to detect the
            // reasoning-only shape: it is also true for an explicit
            // `content: ""` or a non-text content array (e.g. an image-only
            // part) — both a genuine, visible provider response that
            // `extract_content_text` simply can't render as text. Requiring
            // the *raw* field to be absent/null before promoting keeps that
            // response from being silently swapped for leaked
            // chain-of-thought (CodeRabbit review on #1779, comment
            // 3877224319).
            content = extract_content_text(payload.pointer("/choices/0/message/reasoning"));
            if content.is_empty() {
                content =
                    extract_content_text(payload.pointer("/choices/0/message/reasoning_content"));
            }
        }
    }

    // Only a genuinely empty turn (no text anywhere, no tool call) is an error.
    // Fold `finish_reason` into the message so a truncation (`length`) or
    // `content_filter` stop is diagnosable rather than hidden behind a generic
    // "carried neither" string.
    if content.is_empty() && tool_calls.is_empty() {
        let detail = finish_reason
            .as_deref()
            .map(|r| format!(" (finish_reason: {r})"))
            .unwrap_or_default();
        return Err(TinyAgentsError::Model(format!(
            "inference response carried neither choices[0].message.content nor tool_calls{detail}"
        )));
    }

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

    // Build the assistant message directly so a tool-call-only turn carries no
    // spurious empty text block alongside its `tool_calls`.
    let mut blocks = Vec::new();
    if !content.is_empty() {
        blocks.push(ContentBlock::Text(content));
    }
    let message = AssistantMessage {
        id: None,
        content: blocks,
        tool_calls,
        usage,
    };
    let mut response = ModelResponse {
        message,
        usage,
        finish_reason,
        raw: None,
        resolved_model: None,
        continue_turn: None,
        served_from_cache: false,
    };
    // `with_usage` mirrors usage onto both slots; call it only when present so
    // the billing-free path leaves `usage: None` intact.
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
#[derive(Clone, Debug, Default)]
pub struct HostedProviderConfig {
    /// Base URL of the OpenAI-compatible chat-completions API, e.g.
    /// `https://api.tinyhumans.ai/v1`. The provider POSTs to
    /// `{base_url}/chat/completions`.
    pub base_url: String,
    /// How the bearer for the hosted brain is obtained. Resolved on **every**
    /// request, so a platform token that rotates in place is picked up without a
    /// rebuild; [`Credential::None`] omits the header.
    pub credential: Credential,
    /// Extra request headers to attach on every call (e.g. OpenRouter's
    /// `HTTP-Referer` / `X-Title` attribution headers).
    pub extra_headers: Vec<(String, String)>,
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
    product_identity: bool,
    telemetry_provider: &'static str,
    /// The classified model of the most recent **successful** turn (issue
    /// #1749), so the synchronous
    /// [`telemetry_model`](HarnessModel::telemetry_model) reports what the last
    /// call that actually reached the backend ran on.
    ///
    /// Written only after the request returns 2xx: a turn that failed produced
    /// no usage, so publishing its model would name one that never ran for
    /// whichever concurrent turn reads the cache next.
    ///
    /// Behind an [`Arc`] because this type derives `Clone` and a clone is the
    /// same provider — a cloned handle must see the same last-turn model, not a
    /// private copy that never updates.
    telemetry_model: Arc<RwLock<Option<crate::metering::ModelSlug>>>,
}

impl HostedProvider {
    /// Builds a hosted provider from its endpoint configuration.
    pub fn new(config: HostedProviderConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            product_identity: true,
            telemetry_provider: "subscription",
            telemetry_model: Arc::new(RwLock::new(None)),
        }
    }

    /// Builds the short-lived direct provider used before a company exists in
    /// onboarding. It speaks the same wire format without attaching the
    /// TinyHumans product header to a third-party or local endpoint.
    pub fn new_direct(config: HostedProviderConfig, provider: &str) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            product_identity: false,
            telemetry_provider: inference::provider_slug(provider),
            telemetry_model: Arc::new(RwLock::new(None)),
        }
    }
}

#[async_trait]
impl ChatModel<()> for HostedProvider {
    /// Advertise native tool calling so openhuman's turn loop drives structured
    /// `tools`/`tool_calls` instead of prompt-guided XML. See [`MANAGED_PROFILE`].
    fn profile(&self) -> Option<&ModelProfile> {
        Some(&MANAGED_PROFILE)
    }

    /// Structured multi-turn chat — the path [`Agent::turn`] actually calls. The
    /// full history reaches the backend so multi-turn context survives, the bearer
    /// is resolved fresh for this request, and the response's token/cost usage is
    /// parsed back out (the WS5 metering signal).
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
        // Native tool calling: expose the turn's tools so the model emits
        // structured `tool_calls` instead of hand-written `<tool_call>` XML.
        attach_tools(
            &mut body,
            wire_tools(&request.tools),
            &request.tool_choice,
            self.product_identity,
        );

        let base_url = self.config.base_url.trim_end_matches('/');
        let url = format!("{base_url}/chat/completions");
        let mut http = self.client.post(&url).json(&body);
        if self.product_identity {
            // The normal constructor is the managed TinyHumans path. The
            // direct setup constructor deliberately skips this on local/BYOK
            // endpoints, matching `request_plan`'s privacy boundary.
            let (name, value) = crate::product::product_identity_header();
            http = http.header(name, value);
        }
        // Resolved per request, never captured: on the hosted platform this reads
        // a token file the cluster rewrites in place every few minutes.
        let bearer = self.config.credential.current().await.map_err(|e| {
            TinyAgentsError::Model(format!("resolving the TinyHumans credential: {e}"))
        })?;
        if let Some(bearer) = &bearer {
            http = http.bearer_auth(bearer);
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
            // A rejected bearer may mean the platform rotated the token early;
            // drop the cached read so the next turn goes back to the file rather
            // than re-presenting what was just refused.
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.config.credential.invalidate();
            }
            let text = response.text().await.unwrap_or_default();
            let error = format!("hosted inference returned {status}: {text}");
            let models_url = format!("{base_url}/models");
            // The hosted, TinyHumans-managed backend has no harness-scoped
            // inference config to point to — it always resolves the company's
            // default `[inference]`.
            if let Some(advice) = model_unavailable_advice(status, &error, &models_url, None, None)
            {
                return Err(TinyAgentsError::Model(advice));
            }
            return Err(TinyAgentsError::Model(error));
        }

        // Published only now, once *this* request has come back 2xx, and
        // classified from the same `model` that went on the wire — so the raw
        // string stops here and the cost hook reaches only a vocabulary member
        // (#1749). The cache is shared by every clone of this handle, so a turn
        // that never ran must not publish into it: a rejected request (a 401
        // from an early-rotated bearer, say) produces no usage of its own, and
        // writing before the call would let a *concurrent* agent's successful
        // turn read this model and attribute its tokens to one that never ran.
        // Leaving the last successful turn's model in place is the honest
        // reading, and it keeps the documented approximation to what it says on
        // the tin: two models that both actually ran.
        *self.telemetry_model.write().unwrap() = Some(crate::metering::ModelSlug::classify(model));

        let payload: serde_json::Value = response.json().await.map_err(|e| {
            TinyAgentsError::Model(format!("hosted inference response was not JSON: {e}"))
        })?;
        model_response_from_payload(payload)
    }
}

impl HarnessModel for HostedProvider {
    fn telemetry_provider_id(&self) -> String {
        // The normal constructor is the subscription path. First-run setup has
        // no tenant store to hang `TenantProvider` from, so its direct
        // constructor carries the selected provider's slug for that one
        // unmetered roster-design call.
        self.telemetry_provider.to_string()
    }

    fn telemetry_model(&self) -> Option<crate::metering::ModelSlug> {
        *self.telemetry_model.read().unwrap()
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
    /// The bearer credential for THIS request, resolved when the plan was built
    /// (never captured at roster-build time), or `None` to omit the header (e.g.
    /// Ollama).
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
/// * The bearer is resolved from the decl's [`Credential`] **here**, so every
///   plan carries a freshly-read token; no credential omits the header entirely
///   (the Ollama / keyless case).
/// * `tools` (already in OpenAI wire shape via [`wire_tools`]) and `tool_choice`
///   are attached only when the turn exposes tools, so a bare chat turn stays
///   byte-identical to the pre-tool-calling body.
pub async fn request_plan(
    decl: &InferenceDecl,
    abstract_model: &str,
    messages: Vec<serde_json::Value>,
    temperature: f64,
    max_tokens: Option<u32>,
    tools: Vec<serde_json::Value>,
    tool_choice: &ToolChoice,
) -> anyhow::Result<RequestPlan> {
    // Tier -> what this endpoint understands. The direct path talks to
    // OpenRouter, which has never heard of `chat-v1`, so the tier is resolved
    // here; the proxied path keeps the tier, which is what the platform's
    // registry routes on.
    let model = inference::model_for_tier(abstract_model, &decl.models, decl.is_proxied());
    let url = format!("{}/chat/completions", decl.base_url.trim_end_matches('/'));
    let bearer = decl
        .bearer()
        .await
        .map_err(|e| anyhow::anyhow!("resolving the outbound inference credential: {e}"))?;
    let mut headers = Vec::new();

    // OpenRouter's own attribution headers, on BOTH the proxied and the direct
    // path: they identify the app in OpenRouter's dashboard and rankings, which
    // is a feature we want either way and is unrelated to who is paying.
    if inference::normalize_provider(&decl.provider) == "openrouter" {
        headers.push(("HTTP-Referer", OPENROUTER_REFERER.to_string()));
        headers.push(("X-Title", OPENROUTER_TITLE.to_string()));
    }

    // The product-identity header goes ONLY to the platform's own endpoint —
    // i.e. proxied OpenRouter. Every other resolution reaches a THIRD-PARTY
    // endpoint (OpenRouter direct on the tenant's account, a self-hosted
    // OpenAI-compatible server, a local Ollama); sending them our `x-sdk-name`
    // would leak which product a tenant is running to an operator who has no
    // relationship with TinyHumans and gains nothing from knowing it.
    //
    // Keyed on `is_proxied()` rather than the provider kind: after `managed`'s
    // removal the kind no longer distinguishes our endpoint from OpenRouter's,
    // and it is the endpoint — not the vocabulary — that this rule is about.
    if decl.is_proxied() {
        let (name, value) = crate::product::product_identity_header();
        headers.push((name, value.to_string()));
    }
    let mut body = serde_json::json!({
        "model": model,
        "temperature": temperature,
        "messages": messages,
    });
    if let Some(cap) = max_tokens {
        body["max_tokens"] = serde_json::json!(cap);
    }
    let supports_parallel_control =
        decl.is_proxied() || inference::normalize_provider(&decl.provider) == "openrouter";
    attach_tools(&mut body, tools, tool_choice, supports_parallel_control);
    Ok(RequestPlan {
        url,
        model,
        bearer,
        headers,
        body,
    })
}

/// Substrings that mark a provider 4xx as "you asked for a model that isn't
/// there", matched case-insensitively. The wording is the provider's and
/// varies: the managed backend says `Model '<id>' is not available`, an
/// OpenAI-compatible BYOK endpoint says `The model '<id>' does not exist`, and
/// OpenRouter says `<id> is not a valid model ID`.
const MODEL_UNAVAILABLE_SIGNATURES: &[&str] = &[
    "is not available",
    "not a valid model",
    "model not found",
    "unknown model",
    "invalid model",
    "does not exist",
];

/// Rewrites a provider "unknown/unavailable model" refusal into an
/// operator-actionable message, or `None` for any other error (issue #1811).
///
/// A configured model id is company/operator data — not a repo default — so the
/// only fix is to change it. Raw, it reaches the operator as an unactionable
/// `inference returned 400 Bad Request: {"error":"Model '<id>' is not
/// available. Use GET /openai/v1/models to list available models."}` and the
/// task merely reads *Failed*. This says what to do and keeps the provider's own
/// words (which carry the bad id and the list-models hint) at the end for
/// support.
///
/// `models_url` is the catalog endpoint for the request that actually failed —
/// `{base_url}/models`, the same pattern [`discover_local_model`] already uses.
/// Callers derive it from the same `base_url` that built the chat-completions
/// URL rather than this function assuming TinyHumans' `/openai/v1/models`: for
/// a direct OpenRouter, Ollama, or arbitrary `openai_compatible` BYOK endpoint
/// (issue #1811 follow-up) that path 404s and points the operator at the wrong
/// catalog.
///
/// Gated two ways to stay quiet on everything else: a 4xx only (a 5xx is the
/// provider's fault and must not be reframed as a misconfiguration), and the
/// body must name a `model` (so a 4xx about something else — `user does not
/// exist` — is never mistaken for a model error). Deliberately not an allowlist
/// of model ids: that would rot as providers add models, so this recognises the
/// *refusal*, not the catalogue.
///
/// `harness` names the `built_in` harness this request ran as, when the
/// caller has one — `None` only for a caller with no harness concept at all
/// (e.g. the managed [`HostedProvider`]). Every caller with a harness passes
/// its *real*, declared-or-implicit id (`HarnessScope::id`) regardless of
/// whether that harness is the company's default: the default harness's own
/// `[harness.inference]` beats the company mapping exactly like a named
/// harness's does (`default_harness_inference`, `src/company/manifest.rs`),
/// so suppressing the name whenever a harness happened to be the default sent
/// its operator to a table its request never consulted (Codex review on
/// #1824's #1811 follow-up). `agent.model` only takes effect on an `acp`
/// harness (`Manifest::validate`, `src/company/manifest.rs`) — never a real
/// lever for any caller in this module — so this deliberately never suggests
/// it.
///
/// `source` is the resolved [`InferenceDecl::source`] for this request, when
/// known. A console-saved runtime override (`InferenceSource::Runtime`)
/// outranks *both* manifest tables (`resolve_effective_scoped`'s precedence),
/// so naming a `[harness.inference].models` or `[inference].models` mapping
/// while a runtime override is active sends the operator to edit a table that
/// is shadowed and will not change the outcome (Codex review on #1824).
fn model_unavailable_advice(
    status: reqwest::StatusCode,
    error: &str,
    models_url: &str,
    harness: Option<&str>,
    source: Option<InferenceSource>,
) -> Option<String> {
    if !status.is_client_error() {
        return None;
    }
    let haystack = error.to_ascii_lowercase();
    if !haystack.contains("model") {
        return None;
    }
    if !MODEL_UNAVAILABLE_SIGNATURES
        .iter()
        .any(|signature| haystack.contains(signature))
    {
        return None;
    }
    let where_to_fix = match (source, harness) {
        (Some(InferenceSource::Runtime), Some(id)) => format!(
            "update harness `{id}`'s saved runtime inference override (Settings → Inference) — \
             it takes precedence over any `[harness.inference].models` or `[inference].models` \
             mapping"
        ),
        (Some(InferenceSource::Runtime), None) => "update the saved runtime inference override \
             (Settings → Inference) — it takes precedence over the company's `[inference].models` \
             mapping"
            .to_string(),
        (_, Some(id)) => format!(
            "update harness `{id}`'s own `[harness.inference].models` mapping (or the company's \
             `[inference].models`, if `{id}` doesn't declare its own)"
        ),
        (_, None) => "update the company's `[inference].models` mapping".to_string(),
    };
    Some(format!(
        "the configured inference model is not available from the provider — {where_to_fix}, to \
         one the provider offers (list them with `GET {models_url}`). {error}"
    ))
}

/// Issues a prepared [`RequestPlan`] against `client`, returning the raw JSON
/// payload. Every error string is scrubbed of the bearer, so a credential can
/// never leak into a log line or an operator-visible message.
///
/// `credential` is the source the plan's bearer came from: a 401 invalidates it,
/// so a token the platform rotated early is re-read on the next attempt instead
/// of being presented again until its cache window closes.
async fn send_plan(
    client: &reqwest::Client,
    plan: &RequestPlan,
    credential: &Credential,
    harness: Option<&str>,
    source: Option<InferenceSource>,
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
        if status == reqwest::StatusCode::UNAUTHORIZED {
            credential.invalidate();
        }
        let text = response.text().await.unwrap_or_default();
        let error = format!("inference returned {status}: {}", scrub(text));
        // `plan.url` is always `{base_url}/chat/completions` (see
        // `RequestPlan::url`'s doc and `request_plan`'s construction of it), so
        // this recovers the same `base_url` the failed request actually used —
        // OpenRouter's, Ollama's, or an arbitrary `openai_compatible` endpoint's,
        // not a hard-coded TinyHumans path.
        let models_url = plan
            .url
            .strip_suffix("/chat/completions")
            .map(|base| format!("{base}/models"))
            .unwrap_or_else(|| plan.url.clone());
        if let Some(advice) = model_unavailable_advice(status, &error, &models_url, harness, source)
        {
            return Err(anyhow::anyhow!("{advice}"));
        }
        return Err(anyhow::anyhow!("{error}"));
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
    /// The classified model of the most recently **completed** turn (issue
    /// #1749), so the synchronous
    /// [`telemetry_model`](HarnessModel::telemetry_model) reports the model the
    /// last successful turn actually resolved to — which on this path means
    /// *after* the tenant `[inference].models` table has been applied, so a
    /// BYOK tenant's table switch re-attributes the next turn for the same
    /// reason `slug` does.
    ///
    /// Written only once the request has come back 2xx, so a rejected turn —
    /// which meters nothing — cannot name the model for a concurrent turn that
    /// did run.
    model: RwLock<Option<crate::metering::ModelSlug>>,
    /// Which harness's config and credential slots this provider resolves
    /// against. Two `built_in` harnesses on one company each get their own
    /// provider, differing only in this — which is what lets one ride the
    /// subscription while the other runs on a key of its own.
    scope: inference::HarnessScope,
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
            // Replaced by the resolved slug on the first turn; until then the
            // company is on the default it booted with.
            slug: RwLock::new("subscription"),
            // No turn has been issued yet, so there is no model to name.
            model: RwLock::new(None),
            scope: inference::HarnessScope::default(),
        }
    }

    /// Points this provider at one named harness's own config and credential
    /// slots. Without it, every provider reads the company's default harness.
    pub fn with_scope(mut self, scope: inference::HarnessScope) -> Self {
        self.scope = scope;
        self
    }

    /// The harness this provider resolves for.
    pub fn harness_id(&self) -> &str {
        &self.scope.id
    }

    /// Re-resolves the effective config from the secret store and updates the
    /// cached telemetry slug. Errors when no provider is configured at all.
    async fn resolve(&self) -> anyhow::Result<InferenceDecl> {
        let decl = inference::resolve_effective_scoped(
            &self.company,
            &self.manifest,
            self.env_default.as_ref(),
            self.secrets.as_ref(),
            &self.scope,
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
    /// Advertise native tool calling so the harness drives structured
    /// `tools`/`tool_calls`. Most OpenAI-compatible BYOK endpoints (OpenAI,
    /// OpenRouter, DeepSeek, recent Ollama) honour the `tools` param; the
    /// backend ignores an unused array, so this is safe to advertise uniformly.
    /// See [`MANAGED_PROFILE`].
    fn profile(&self) -> Option<&ModelProfile> {
        Some(&MANAGED_PROFILE)
    }

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
        let plan = request_plan(
            &decl,
            model,
            messages,
            temperature,
            request.max_tokens,
            wire_tools(&request.tools),
            &request.tool_choice,
        )
        .await
        .map_err(|e| TinyAgentsError::Model(e.to_string()))?;
        // Always this harness's real id — `self.scope.id` is meaningful
        // whether or not this is the company's *default* harness (the
        // default's own `[harness.inference]` beats the company mapping the
        // same way a named harness's does; `is_default` only routes which
        // secret keys get read, and must not also gate whether the advice
        // names the harness — see `model_unavailable_advice`'s doc).
        let harness = Some(self.scope.id.as_str());
        let payload = send_plan(
            &self.client,
            &plan,
            decl.credential(),
            harness,
            Some(decl.source),
        )
        .await
        .map_err(|e| TinyAgentsError::Model(e.to_string()))?;
        // Classified from `plan.model` — the exact string that goes on the wire,
        // *after* the tenant `[inference].models` table has been applied — so
        // the sample names what actually ran rather than the tier that was
        // asked for. `plan.model` is operator-authored text on a BYOK or
        // `openai_compatible` tenant and stops here: the only model identity
        // that leaves this method is the vocabulary member (issue #1749).
        //
        // Published *after* `send_plan` returns `Ok`, i.e. once this request has
        // actually come back 2xx. The cache is read by whichever turn finishes
        // next, and one provider is shared across concurrently running agents,
        // so publishing before the call would let a request that is still in
        // flight — or one that was rejected outright — name the model for
        // another agent's successful turn. A failed turn produces no usage of
        // its own, so keeping the last *successful* model is strictly more
        // accurate than advertising one that never ran.
        *self.model.write().unwrap() = Some(crate::metering::ModelSlug::classify(&plan.model));
        model_response_from_payload(payload)
    }
}

impl HarnessModel for TenantProvider {
    fn telemetry_provider_id(&self) -> String {
        (*self.slug.read().unwrap()).to_string()
    }

    fn telemetry_model(&self) -> Option<crate::metering::ModelSlug> {
        *self.model.read().unwrap()
    }
}

/// A minimal live probe: one `ping` turn against the resolved config, used by
/// the console's "Test" button. The error is scrubbed of the credential by
/// [`send_plan`].
///
/// `harness` is the real id of the harness whose config `decl` resolved from,
/// when the caller has one — `None` for the first-run wizard's
/// [`decl_for_probe`](crate::company::inference::decl_for_probe), which runs
/// before any company (and so any harness) exists. The console "Test" route
/// (`test_config`) always has a company and therefore a default harness id,
/// declared-or-implicit, and passes it: a missing-model repair hint otherwise
/// names the company's `[inference].models` even when the failing config came
/// from that harness's own `[harness.inference]`, sending the operator to a
/// table its request never consulted — the same gap already closed for live
/// turns in [`TenantProvider::invoke`] (Codex review on #1824's #1811
/// follow-up).
pub async fn probe(decl: &InferenceDecl, harness: Option<&str>) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let messages = vec![serde_json::json!({ "role": "user", "content": "ping" })];
    // The connectivity probe exposes no tools — it only checks the endpoint
    // answers a bare chat turn.
    let plan = request_plan(
        decl,
        DEFAULT_HOSTED_MODEL,
        messages,
        0.0,
        Some(16),
        Vec::new(),
        &ToolChoice::Auto,
    )
    .await?;
    let payload = send_plan(
        &client,
        &plan,
        decl.credential(),
        harness,
        Some(decl.source),
    )
    .await?;
    // Route through the exact same parser the turn path calls
    // (`model_response_from_payload`), not a hand-rolled subset of it. An
    // earlier revision called `extract_content_text` directly here, which
    // picked up the array-shaped-content case but not the
    // `reasoning`/`reasoning_content` fallback for reasoning-only turns
    // (`content: null`, `finish_reason: "stop"`) that lives inside
    // `model_response_from_payload` — so an endpoint answering with that shape
    // still passed every real turn while this probe reported the connection
    // broken (Codex review on #1779, comment 3864906472). Giving the probe a
    // second, narrower copy of the parsing logic is exactly how it drifted
    // from the turn path the first time; calling the shared function directly
    // means there is only one content path to keep in sync.
    let response = model_response_from_payload(payload)
        .map_err(|e| anyhow::anyhow!("probe response carried no usable content: {e}"))?;
    // `model_response_from_payload` accepts a tool-call-only reply — correct
    // for a real turn, where the model may have been offered tools and
    // legitimately chose to call one instead of answering in prose. This
    // probe offers none (`Vec::new()` above), so a tool call here can only
    // be the endpoint hallucinating or defaulting to an action it was never
    // given, not a valid response to `ping`. Letting it through would report
    // a broken endpoint as reachable, passing the setup wizard or console
    // Test action for a provider that cannot complete the bare chat turn it
    // exists to verify (CodeRabbit review on #1779, comment 3877827976).
    //
    // Checking `content.is_empty()` alone only catches a tool-call-*only*
    // reply. An endpoint can also emit a text preamble alongside a genuinely
    // parsed tool call (`content` nonempty AND `tool_calls` nonempty) —
    // `model_response_from_payload` accepts that combination for a real turn
    // too, so it clears this guard with content to spare even though a tool
    // call the probe never offered was still requested. Require the tool-call
    // list to be empty as well so any tool call at all — bare or alongside
    // text — fails the probe (CodeRabbit review on #1779, comment
    // 3878355375).
    if response.message.content.is_empty() || !response.message.tool_calls.is_empty() {
        return Err(anyhow::anyhow!(
            "probe response carried a tool call — endpoint requested an \
             action instead of (or alongside) answering a turn that offered \
             no tools"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::config::MapEnv;

    /// A three-segment JWT whose `exp` is `secs_from_now` in the future, so the
    /// projected-file cache window is wide open for the whole test.
    fn jwt_with_exp(secs_from_now: u64) -> String {
        const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + secs_from_now;
        let payload = serde_json::json!({ "exp": exp }).to_string();
        let mut encoded = String::new();
        for chunk in payload.as_bytes().chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..chunk.len() + 1 {
                encoded.push(ALPHA[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            }
        }
        format!("aGVhZGVy.{encoded}.c2ln")
    }

    /// The bearer a config would present right now.
    async fn bearer_of(config: &HostedProviderConfig) -> Option<String> {
        config.credential.current().await.expect("resolves")
    }

    /// Build a single-user-message request the way the harness turn does.
    fn user_request(message: &str) -> ModelRequest {
        ModelRequest {
            messages: vec![Message::user(message)],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn env_config_prefers_specific_key_and_fills_defaults() {
        let env = MapEnv::new([("OPENCOMPANY_INFERENCE_KEY", "sk-specific")]);
        let (cfg, model) = harness_inference_from_env(&env).expect("configured");
        assert_eq!(bearer_of(&cfg).await.as_deref(), Some("sk-specific"));
        assert_eq!(cfg.base_url, DEFAULT_TINYHUMANS_INFERENCE_URL);
        // No explicit model → no roster-wide override (each agent keeps its tier).
        assert_eq!(model, None);
    }

    #[tokio::test]
    async fn env_config_falls_back_to_tinyhumans_key_and_honors_overrides() {
        let env = MapEnv::new([
            ("TINYHUMANS_API_KEY", "sk-platform"),
            (
                "OPENCOMPANY_INFERENCE_URL",
                "https://staging-api.tinyhumans.ai/openai/v1",
            ),
            ("OPENCOMPANY_INFERENCE_MODEL", "reasoning-v1"),
        ]);
        let (cfg, model) = harness_inference_from_env(&env).expect("configured");
        assert_eq!(bearer_of(&cfg).await.as_deref(), Some("sk-platform"));
        assert_eq!(cfg.base_url, "https://staging-api.tinyhumans.ai/openai/v1");
        assert_eq!(model.as_deref(), Some("reasoning-v1"));
    }

    #[test]
    fn env_config_is_none_without_any_key() {
        let env = MapEnv::new([("OPENCOMPANY_INFERENCE_URL", "https://x/v1")]);
        assert!(harness_inference_from_env(&env).is_none());
    }

    /// The hosted path: no static key anywhere, just a projected token file. The
    /// harness must still resolve a managed brain, reading the file per request.
    #[tokio::test]
    async fn env_config_resolves_a_projected_token_file() {
        let dir = tempfile::Builder::new()
            .prefix("oc-prov-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "projected-token").unwrap();

        let env = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            path.display().to_string(),
        )]);
        let (cfg, _) = harness_inference_from_env(&env).expect("configured");
        assert_eq!(
            cfg.credential.source(),
            crate::company::CredentialSource::Attested
        );
        assert_eq!(bearer_of(&cfg).await.as_deref(), Some("projected-token"));

        // A projected file outranks a static key that is still lying around.
        let both = MapEnv::new([
            (
                crate::company::credentials::TOKEN_FILE_ENV,
                path.display().to_string(),
            ),
            (
                crate::company::credentials::API_KEY_ENV,
                "th-static".to_string(),
            ),
        ]);
        let (cfg, _) = harness_inference_from_env(&both).expect("configured");
        assert_eq!(bearer_of(&cfg).await.as_deref(), Some("projected-token"));
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

    /// Managed search (issue #238) rides the platform identity and accepts a URL
    /// override for staging, with the default daily cap applied.
    #[tokio::test]
    async fn search_backend_rides_the_platform_key_and_honors_the_url_override() {
        let env = MapEnv::new([
            ("TINYHUMANS_API_KEY", "platform-key"),
            (
                "OPENCOMPANY_SEARCH_BACKEND_URL",
                "https://staging-api.tinyhumans.ai",
            ),
        ]);
        let backend = search_backend_from_env(&env).expect("configured");
        assert_eq!(backend.backend_url, "https://staging-api.tinyhumans.ai");
        assert_eq!(
            backend.daily_call_cap,
            crate::company::DEFAULT_SEARCH_DAILY_CALLS
        );
        assert_eq!(
            backend.credential.current().await.unwrap().as_deref(),
            Some("platform-key")
        );

        // Default URL when only the platform key is present.
        let bare = search_backend_from_env(&MapEnv::new([("TINYHUMANS_API_KEY", "platform-key")]))
            .expect("configured");
        assert_eq!(bare.backend_url, DEFAULT_TINYHUMANS_SEARCH_BACKEND_URL);
    }

    /// There is deliberately **no** `OPENCOMPANY_SEARCH_KEY`: the #188 sign-off
    /// admitted search on the platform identity rather than a credential of its
    /// own. A per-tenant inference key must never stand in for it, and no
    /// credential at all means no search tool is ever wired (fail-closed).
    #[test]
    fn search_backend_has_no_credential_of_its_own_and_fails_closed() {
        let env = MapEnv::new([
            (
                "OPENCOMPANY_SEARCH_BACKEND_URL",
                "https://api.tinyhumans.ai",
            ),
            // A tenant BYOK inference key is NOT the platform identity.
            ("OPENCOMPANY_INFERENCE_KEY", "tenant-byok"),
            // And a hypothetical per-surface key is not consulted.
            ("OPENCOMPANY_SEARCH_KEY", "search-specific"),
        ]);
        assert!(search_backend_from_env(&env).is_none());
    }

    // ---- boot-time platform credential status (issue #879) -----------------

    /// Writes a projected-token file and returns `(dir, path-as-string)`. The
    /// `TempDir` must stay alive: `TinyhumansTokenSource` selects the projected
    /// tier only when the path **exists**.
    fn projected_token_file() -> (tempfile::TempDir, String) {
        let dir = tempfile::Builder::new()
            .prefix("oc-cred-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "projected-token").unwrap();
        let rendered = path.display().to_string();
        (dir, rendered)
    }

    /// The #879 tenant: nothing at all is set. One warning must name every
    /// surface that silently failed closed, and both tiers, because the fix
    /// differs between a cluster tenant and `docker compose`.
    #[test]
    fn no_platform_credential_warns_about_every_managed_surface() {
        let status = PlatformCredentialStatus::resolve(&MapEnv::default());
        assert!(!status.platform_identity);
        assert!(!status.all_wired());

        let warning = status.boot_warning().expect("a warning");
        assert!(warning.contains("no platform credential"), "{warning}");
        for expected in [
            "inference",
            "web_search",
            "media",
            crate::company::credentials::TOKEN_FILE_ENV,
            crate::company::credentials::API_KEY_ENV,
        ] {
            assert!(warning.contains(expected), "{expected} missing: {warning}");
        }
    }

    /// The trap this check exists for. A hosted tenant given the projected token
    /// volume — the documented hosted mechanism, and the fix for #879 — resolves
    /// inference and search but **not** media, because
    /// [`media_backend_from_env`] reads only the static tier. Staying silent
    /// here is what leaves an operator who has just supplied the credential
    /// staring at a console that still says "Awaiting credential".
    #[test]
    fn projected_token_alone_warns_that_media_cannot_use_it() {
        let (_dir, token_path) = projected_token_file();
        let env = MapEnv::new([(crate::company::credentials::TOKEN_FILE_ENV, token_path)]);

        let status = PlatformCredentialStatus::resolve(&env);
        assert!(status.platform_identity);
        assert!(status.projected_tier);
        assert!(status.inference, "inference reads the projected tier");
        assert!(status.search, "search reads the projected tier");
        assert!(!status.media, "media reads only the static tier");
        assert!(!status.all_wired());

        let warning = status.boot_warning().expect("a warning");
        assert!(warning.contains("media"), "{warning}");
        assert!(
            warning.contains("OPENCOMPANY_MEDIA_KEY"),
            "the warning must name the variable that fixes it: {warning}"
        );
    }

    /// A projected token plus a media key wires everything, so boot says
    /// nothing. Guards the check against crying wolf on a healthy deployment.
    #[test]
    fn projected_token_with_a_media_key_is_silent() {
        let (_dir, token_path) = projected_token_file();
        let env = MapEnv::new([
            (
                crate::company::credentials::TOKEN_FILE_ENV.to_string(),
                token_path,
            ),
            (
                "OPENCOMPANY_MEDIA_KEY".to_string(),
                "media-specific".to_string(),
            ),
        ]);

        let status = PlatformCredentialStatus::resolve(&env);
        assert!(status.all_wired());
        assert_eq!(status.boot_warning(), None);
    }

    /// The `docker compose` / self-host shape: one static key feeds all three
    /// surfaces, so boot is silent there too.
    #[test]
    fn a_static_key_wires_every_surface_and_is_silent() {
        let env = MapEnv::new([(crate::company::credentials::API_KEY_ENV, "th-static")]);
        let status = PlatformCredentialStatus::resolve(&env);

        assert!(status.platform_identity);
        assert!(!status.projected_tier);
        assert!(status.all_wired());
        assert_eq!(status.boot_warning(), None);
    }

    /// A media key on its own is not a platform identity: it wires media and
    /// leaves inference and search closed, which the partial arm must report by
    /// name rather than collapsing into "no credential".
    #[test]
    fn a_media_key_alone_warns_about_the_surfaces_it_does_not_cover() {
        let env = MapEnv::new([("OPENCOMPANY_MEDIA_KEY", "media-specific")]);
        let status = PlatformCredentialStatus::resolve(&env);

        assert!(!status.platform_identity);
        assert!(status.media);
        assert!(!status.inference);
        assert!(!status.search);

        let warning = status.boot_warning().expect("a warning");
        assert!(warning.contains("partly configured"), "{warning}");
        assert!(warning.contains("web_search"), "{warning}");
        assert!(
            !warning.contains("no platform credential"),
            "a partial deployment is not a bare one: {warning}"
        );
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
            credential: Credential::None,
            extra_headers: Vec::new(),
        });
        assert_eq!(provider.telemetry_provider_id(), "subscription");
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

    /// Wire responses are freshly served unless the provider explicitly
    /// reports otherwise. Keep the compatibility default introduced with the
    /// TinyAgents response field pinned at this parsing boundary.
    #[test]
    fn parsed_response_is_not_marked_as_cached() {
        let payload = serde_json::json!({
            "choices": [{ "message": { "content": "fresh" } }]
        });

        let response = model_response_from_payload(payload).expect("parses");

        assert!(!response.served_from_cache);
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
    fn empty_message_is_an_error_and_no_usage_is_none() {
        // Neither content nor tool_calls → genuinely empty, still an error.
        let empty = serde_json::json!({ "choices": [{ "message": {} }] });
        assert!(model_response_from_payload(empty).is_err());

        let no_usage = serde_json::json!({
            "choices": [{ "message": { "content": "hi" } }]
        });
        let resp = model_response_from_payload(no_usage).expect("parses");
        assert!(resp.usage.is_none());
    }

    /// Some OpenAI-compatible providers return `content` as an array of parts
    /// (`[{"type":"text","text":"…"}]`) rather than a bare string. The parser
    /// must concatenate the `text` of each text part instead of treating the
    /// non-string value as empty and hard-erroring. Regression for bug #1.
    #[test]
    fn parses_content_as_array_of_text_parts() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "Hello, " },
                        { "type": "text", "text": "world" }
                    ]
                }
            }]
        });
        let resp = model_response_from_payload(payload).expect("array content parses");
        assert_eq!(resp.text(), "Hello, world");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// A non-null but empty visible content field is not the documented
    /// reasoning-only shape. It must not cause internal reasoning to be
    /// promoted as the assistant answer.
    #[test]
    fn empty_string_content_does_not_fall_back_to_reasoning() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "",
                    "reasoning": "internal thought"
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("empty string content must not promote reasoning");
        assert!(err.to_string().contains("neither"));
    }

    /// An absent visible content field is distinct from an explicit null and
    /// must not activate the reasoning-only fallback.
    #[test]
    fn absent_content_does_not_fall_back_to_reasoning() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "reasoning": "internal thought"
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("absent content must not promote reasoning");
        assert!(err.to_string().contains("neither"));
    }

    /// An unsupported content shape is not equivalent to null content.
    #[test]
    fn unsupported_content_does_not_fall_back_to_reasoning() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "image_url", "image_url": {} }],
                    "reasoning": "internal thought"
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("unsupported content must not promote reasoning");
        assert!(err.to_string().contains("neither"));
    }

    /// A reasoning-only turn returns `content: null` with the visible text under
    /// a `reasoning` field and no tool calls. It must fall back to the reasoning
    /// text and parse rather than hard-erroring — the managed reasoning brain
    /// (deepseek/qwen via OpenRouter) is the exact source of the crash.
    #[test]
    fn reasoning_only_turn_falls_back_to_reasoning_text() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "The answer is 42."
                }
            }]
        });
        let resp = model_response_from_payload(payload).expect("reasoning-only turn parses");
        assert_eq!(resp.text(), "The answer is 42.");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// A refusal turn: `content: null`, `finish_reason: "stop"`, a nonempty
    /// `message.refusal`, and `reasoning` the model emitted before declining.
    /// The refusal is the provider's own visible safety response and must win
    /// over the internal reasoning — promoting the reasoning instead would
    /// expose exactly the content the model declined to return (CodeRabbit
    /// review on #1779, comment 3872084054).
    #[test]
    fn a_refusal_wins_over_leaked_reasoning() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "The user wants help with something I should decline.",
                    "refusal": "I can't help with that."
                }
            }]
        });
        let resp = model_response_from_payload(payload).expect("refusal turn parses");
        assert_eq!(resp.text(), "I can't help with that.");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// A refusal turn where the array-shaped `content` field itself encodes
    /// the refusal as a `{"type":"refusal","refusal":"…"}` part instead of
    /// the scalar sibling `message.refusal` field — some providers/gateways
    /// normalize a Responses-API-style refusal part into the Chat
    /// Completions `content` array. `extract_content_text` only concatenates
    /// `"text"`-typed parts, so the refusal part contributes nothing and
    /// `content` comes back empty; without an array-aware refusal check the
    /// scalar `message.refusal` lookup also finds nothing, and the reasoning
    /// fallback would promote the leaked pre-refusal reasoning as the
    /// visible answer. The refusal must still win (Codex review on #1779,
    /// comment 3874381270).
    #[test]
    fn a_refusal_wins_over_leaked_reasoning_when_refusal_is_an_array_content_part() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "refusal", "refusal": "I can't help with that." }
                    ],
                    "reasoning": "The user wants help with something I should decline."
                }
            }]
        });
        let resp = model_response_from_payload(payload).expect("refusal turn parses");
        assert_eq!(resp.text(), "I can't help with that.");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// Multiple `{"type":"refusal",…}` parts in the same array-shaped
    /// `content`. `extract_array_refusal_text`'s `find_map` stops at the
    /// first match, so only the first part's text is recovered — the
    /// analogous `extract_content_text` concatenates every `"text"`-typed
    /// part instead of stopping at the first, so the refusal path must do
    /// the same or it silently truncates the provider's own visible safety
    /// response (CodeRabbit review on #1779, comment 3878506287).
    #[test]
    fn a_refusal_wins_and_is_not_truncated_when_content_array_has_multiple_refusal_parts() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "refusal", "refusal": "I can't help with that. " },
                        { "type": "refusal", "refusal": "Here's why." }
                    ]
                }
            }]
        });
        let resp = model_response_from_payload(payload).expect("refusal turn parses");
        assert_eq!(resp.text(), "I can't help with that. Here's why.");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// A mixed array: a `"text"`-typed part alongside a `{"type":"refusal",…}`
    /// part in the same `content` array. `extract_content_text` concatenates
    /// only the text part, so `content` is already nonempty by the time the
    /// refusal-precedence block is reached — without checking for an array
    /// refusal independent of `content`'s emptiness, the block is skipped
    /// entirely and the turn "succeeds" with just the leaked text fragment,
    /// silently discarding the provider's actual safety response (Codex
    /// review on #1779, comment 3875001349).
    #[test]
    fn a_refusal_wins_over_leaked_text_when_content_array_mixes_text_and_refusal_parts() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "Sure, here's a start: " },
                        { "type": "refusal", "refusal": "I can't help with that." }
                    ]
                }
            }]
        });
        let resp = model_response_from_payload(payload).expect("refusal turn parses");
        assert_eq!(resp.text(), "I can't help with that.");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// An array-shaped `content` with only a `"text"`-typed part (no
    /// `{"type":"refusal",…}` part at all) alongside a nonempty *scalar*
    /// `message.refusal` sibling field. `extract_content_text` concatenates
    /// the text part, so `content` is nonempty; `extract_array_refusal_text`
    /// finds no refusal-typed part, so `array_refusal` is `None`. Gating the
    /// refusal-precedence block on `content.is_empty() || array_refusal.is_some()`
    /// alone therefore skips the block entirely and never even looks at the
    /// scalar `message.refusal` field, leaking the text fragment as the
    /// answer instead of surfacing the provider's actual safety response
    /// (Codex review on #1779, comment 3875101974).
    #[test]
    fn a_scalar_refusal_wins_over_leaked_text_in_array_shaped_content() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "Sure, here's a start: " }
                    ],
                    "refusal": "I can't help with that."
                }
            }]
        });
        let resp = model_response_from_payload(payload).expect("refusal turn parses");
        assert_eq!(resp.text(), "I can't help with that.");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// The mixed-array refusal case above (Codex review comment 3875001349)
    /// only reproduced with `finish_reason: "stop"`. The refusal-precedence
    /// block was gated on `genuinely_finished`, so the identical payload with
    /// `finish_reason: "content_filter"` — arguably the *more* likely finish
    /// reason a real content-policy refusal ends with — skipped the block
    /// entirely: `content` was already nonempty from the leaked text part,
    /// so the empty-response check at the bottom accepted it and returned
    /// the leaked lead-in as if it were the whole answer, silently
    /// discarding the refusal. A refusal is a completed decision, not a
    /// partial one, so its precedence must not depend on `finish_reason` the
    /// way the reasoning fallback's does (Codex review on #1779, comment
    /// 3875167298).
    #[test]
    fn a_refusal_wins_over_leaked_text_regardless_of_finish_reason() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "content_filter",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "Sure, here's a start: " },
                        { "type": "refusal", "refusal": "I can't help with that." }
                    ]
                }
            }]
        });
        let resp = model_response_from_payload(payload).expect("refusal turn parses");
        assert_eq!(resp.text(), "I can't help with that.");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// The sibling fallback field: some providers emit the reasoning-only
    /// text under `reasoning_content` (array-of-parts shape) instead of
    /// `reasoning`, with `reasoning` itself absent. `extract_content_text`
    /// handles the array shape and `model_response_from_payload` only tries
    /// `reasoning_content` once `reasoning` comes back empty — this test
    /// exercises that second fallback specifically, which the existing
    /// `reasoning`-field and `content_filter`-error tests do not cover
    /// (CodeRabbit nitpick on #1779, comment ed359cf20f434c7f7f83c058).
    #[test]
    fn reasoning_only_turn_falls_back_to_array_shaped_reasoning_content() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "",
                    "reasoning_content": [
                        { "type": "text", "text": "The answer is " },
                        { "type": "text", "text": "42." }
                    ]
                }
            }]
        });
        let resp =
            model_response_from_payload(payload).expect("reasoning_content-only turn parses");
        assert_eq!(resp.text(), "The answer is 42.");
        assert!(resp.message.tool_calls.is_empty());
    }

    /// An explicit `content: ""` (not `null`) is a *visible* empty response,
    /// not the documented reasoning-only shape — `extract_content_text`
    /// reduces both to the same empty string, so the old `content.is_empty()`
    /// check could not tell them apart and promoted `reasoning` anyway. That
    /// substitutes internal chain-of-thought for whatever unsupported/empty
    /// response the provider actually sent, the same class of bug the
    /// refusal-precedence guard above exists to prevent, just triggered by an
    /// empty string instead of a populated field (CodeRabbit review on
    /// #1779, comment 3877224319).
    #[test]
    fn explicit_empty_string_content_does_not_promote_reasoning() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "",
                    "reasoning": "The user wants help with something I should decline."
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("explicit empty-string content must not promote reasoning");
        assert!(
            !err.to_string().contains("decline"),
            "leaked reasoning must not appear in the error, got: {err}"
        );
    }

    /// Same gap as above, via the array-content path: a non-text content
    /// array (e.g. an image-only part) extracts to an empty string too, but
    /// the raw field is neither absent nor `null` — it is the provider's
    /// actual (just non-text) response, and must not be silently swapped for
    /// leaked reasoning (CodeRabbit review on #1779, comment 3877224319).
    #[test]
    fn non_text_array_content_does_not_promote_reasoning() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "image_url", "image_url": { "url": "https://example.com/x.png" } }
                    ],
                    "reasoning": "The user wants help with something I should decline."
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("non-text array content must not promote reasoning");
        assert!(
            !err.to_string().contains("decline"),
            "leaked reasoning must not appear in the error, got: {err}"
        );
    }

    /// A genuinely empty turn truncated by `finish_reason: "length"` (max_tokens
    /// hit) is still an error — but the message must name the finish reason so
    /// the truncation is diagnosable rather than hidden behind a generic string.
    #[test]
    fn truncated_empty_response_errors_with_finish_reason() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "length",
                "message": { "role": "assistant", "content": "" }
            }]
        });
        let err = model_response_from_payload(payload).expect_err("truncated empty turn errors");
        let msg = err.to_string();
        assert!(
            msg.contains("length"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// A reasoning-only turn truncated by `finish_reason: "length"` (max_tokens
    /// hit mid chain-of-thought) must still error, even though `reasoning`
    /// carries text — the reasoning-fallback exists to recover a *complete*
    /// answer that only landed under `reasoning`, not to promote a cut-off
    /// chain of thought into a fabricated final reply. Pre-fix, this payload
    /// parsed successfully with `resp.text() == "The answer is"`, silently
    /// handing a partial thought to downstream consumers as if it were the
    /// finished answer (Codex review on #1779).
    #[test]
    fn truncated_reasoning_only_turn_errors_instead_of_promoting_partial_thought() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "length",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "The answer is"
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("truncated reasoning-only turn must not parse as success");
        let msg = err.to_string();
        assert!(
            msg.contains("length"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// Same as above but for `content_filter` — a filtered reasoning stream is
    /// just as unfinished as a truncated one and must not be promoted either.
    #[test]
    fn content_filtered_reasoning_only_turn_errors() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "content_filter",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "Let's think about how to"
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("content-filtered reasoning-only turn must not parse as success");
        let msg = err.to_string();
        assert!(
            msg.contains("content_filter"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// `finish_reason: "failed"` is the documented HTTP-200-empty-response
    /// silent provider failure (see docs/spec/runtime/providers.md — observed
    /// on an oversized request, empty message, zero usage). The pre-fix guard
    /// blocklisted only `length`/`content_filter`, so a reasoning-only turn
    /// carrying `failed` still fell through and promoted whatever partial
    /// reasoning the provider emitted before failing — handing downstream
    /// consumers an unfinished thought as if it were the answer (Codex
    /// follow-up review on #1779, comment 3860281502). Must error instead.
    #[test]
    fn failed_finish_reason_reasoning_only_turn_errors() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "failed",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "The answer is"
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("failed reasoning-only turn must not parse as success");
        let msg = err.to_string();
        assert!(
            msg.contains("failed"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// Same as `failed_finish_reason_reasoning_only_turn_errors`, but the
    /// leaked text lives in the *primary* `content` field (array-shaped, the
    /// form round #8 of this PR taught `extract_content_text` to parse) rather
    /// than `reasoning`. `content` is extracted unconditionally at the top of
    /// `model_response_from_payload`, with no `finish_reason` check of its
    /// own — only the `reasoning` fallback is gated on `genuinely_finished`.
    /// Pre-fix, this payload parsed successfully with the leaked lead-in
    /// sentence returned as the answer, silently discarding the provider's own
    /// `failed` disclaimer (CodeRabbit review on #1779, comment 3878355364).
    #[test]
    fn failed_finish_reason_with_leaked_array_content_errors() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "failed",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "Let me look that up for you" }
                    ]
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("failed turn with leaked content must not parse as success");
        let msg = err.to_string();
        assert!(
            !msg.contains("look that up"),
            "leaked content must not appear in the error, got: {msg}"
        );
        assert!(
            msg.contains("failed"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// The legacy `finish_reason: "function_call"` shape carries the request
    /// under the singular `message.function_call` field, which
    /// `parse_tool_calls` never reads (it only parses the modern
    /// `message.tool_calls` array). Pre-fix, `finish_reason: "function_call"`
    /// sat in the `genuinely_finished` allow-list, so with `tool_calls` empty
    /// (nothing there to parse) and `content: null`, this fell straight into
    /// the reasoning fallback and silently swapped the requested action for
    /// prose — the caller never even sees a tool call was dropped. Must error
    /// instead (Codex follow-up review on #1779, comment 3862781739).
    #[test]
    fn legacy_function_call_with_reasoning_errors_instead_of_promoting() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "function_call",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "I should call the weather function",
                    "function_call": { "name": "get_weather", "arguments": "{}" }
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("a raw legacy function_call must not be dropped for promoted reasoning");
        let msg = err.to_string();
        assert!(
            msg.contains("function_call"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// A raw `tool_calls` array can be *partially* malformed: one entry
    /// parses (has `function.name`), one does not. `parse_tool_calls`'s
    /// `filter_map` drops the malformed entry and returns the single valid
    /// one — a nonempty `Vec`, so `tool_calls.is_empty()` alone never
    /// catches it. Pre-fix, the response is returned successfully with only
    /// the surviving call, silently discarding a genuinely requested action
    /// (CodeRabbit review on #1779, comment 3877118065). The guard must
    /// compare the raw array length against the parsed count, not just
    /// check for emptiness.
    #[test]
    fn partially_malformed_tool_calls_array_errors_instead_of_dropping_one_call() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        { "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "{}" } },
                        { "id": "call_2", "type": "function", "function": { "arguments": "{}" } }
                    ]
                }
            }]
        });
        let err = model_response_from_payload(payload).expect_err(
            "a partially malformed tool_calls array must not silently drop the malformed entry",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("tool call"),
            "error must name the dropped tool call for diagnosis, got: {msg}"
        );
    }

    /// A malformed modern `tool_calls` entry (missing `function.name`) is
    /// dropped by `parse_tool_calls`'s `filter_map`, leaving the *parsed*
    /// `tool_calls` empty even though the raw payload clearly requested one.
    /// The raw-payload guard must catch this too, not just the legacy
    /// `function_call` field, so a request that fails to parse surfaces as
    /// the empty-response error rather than a promoted reasoning answer.
    #[test]
    fn malformed_modern_tool_call_with_reasoning_errors_instead_of_promoting() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "I should call the weather function",
                    "tool_calls": [{ "id": "call_1", "type": "function", "function": { "arguments": "{}" } }]
                }
            }]
        });
        let err = model_response_from_payload(payload).expect_err(
            "a malformed raw tool_calls entry must not be dropped for promoted reasoning",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("tool_calls"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// Array-shaped `content` can carry a text preamble ("Let me check
    /// that…") alongside a `tool_calls` entry the model genuinely requested
    /// but that fails to parse (missing `function.name`). `content` reads
    /// nonempty via `extract_content_text` while `parse_tool_calls` drops the
    /// call, so a check that only looks at content-or-tool_calls emptiness
    /// passes and the harness would silently return just the preamble,
    /// dropping the requested action entirely. The raw-payload guard must
    /// catch this regardless of whether prose content is also present
    /// (CodeRabbit review on #1779, comment 3872084060).
    #[test]
    fn malformed_tool_call_beside_array_content_preamble_errors_instead_of_silently_dropping() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "Let me check that for you." }
                    ],
                    "tool_calls": [{ "id": "call_1", "type": "function", "function": { "arguments": "{}" } }]
                }
            }]
        });
        let err = model_response_from_payload(payload).expect_err(
            "a malformed raw tool_calls entry beside preamble content must not be silently dropped",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("tool_calls"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// A non-array `message.tool_calls` (e.g. an object instead of a list) is
    /// neither a legacy `function_call` nor something `.as_array()` accepts,
    /// so pre-fix the raw-payload guard silently read it as "no call
    /// present" and fell through to the reasoning fallback below — the exact
    /// class of substitution the array/legacy checks above exist to prevent,
    /// just for a shape neither one covers. Must error instead of promoting
    /// (CodeRabbit review on #1779, comment 3872083353).
    #[test]
    fn non_array_tool_calls_with_reasoning_errors_instead_of_promoting() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "I should call the weather function",
                    "tool_calls": {}
                }
            }]
        });
        let err = model_response_from_payload(payload).expect_err(
            "a non-array raw tool_calls value must not be dropped for promoted reasoning",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("stop"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// A `finish_reason: "tool_calls"` response that carries no call body at
    /// all (no `tool_calls` field, no legacy `function_call` field) must
    /// error rather than promote `reasoning` into the final answer. The
    /// finish reason itself asserts the model requested an action; treating
    /// it as "genuinely finished" let the raw-payload guard (which only
    /// checks for a *present* call) miss the case where there is no call
    /// field to find. Pre-fix, this silently swapped the requested action
    /// for prose (Codex review on #1779, comment 3864692178).
    #[test]
    fn tool_calls_finish_reason_with_missing_call_body_errors_instead_of_promoting() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "I should call the weather function"
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("a tool_calls finish reason with no call body must not promote reasoning");
        let msg = err.to_string();
        assert!(
            msg.contains("tool_calls"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// Same gap, but the provider sends an explicit empty `tool_calls: []`
    /// array instead of omitting the field — the raw-payload guard treats an
    /// empty array as "nothing requested" (correctly, for `parse_tool_calls`
    /// purposes) but that must not be read as license to promote reasoning
    /// when the finish reason itself claims an action was intended.
    #[test]
    fn tool_calls_finish_reason_with_empty_call_array_errors_instead_of_promoting() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "I should call the weather function",
                    "tool_calls": []
                }
            }]
        });
        let err = model_response_from_payload(payload).expect_err(
            "a tool_calls finish reason with an empty call array must not promote reasoning",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("tool_calls"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// Same gap again, but via the *content* channel instead of `reasoning`:
    /// array-shaped `content` carrying a nonempty text preamble ("Let me
    /// check that…") makes `content` nonempty on its own, with no reasoning
    /// fallback involved at all. `raw_tool_call_requested` reads a present
    /// but empty `tool_calls: []` array the same as an absent field (both
    /// "nothing requested"), so the explicit raw-payload guard never fires;
    /// and because `content` is already nonempty, the final
    /// content-and-tool_calls-both-empty catch-all below never fires either.
    /// The response would be returned successfully with the preamble as the
    /// full text and no tool call — silently dropping the action the
    /// `finish_reason` itself asserts was requested (CodeRabbit review on
    /// #1779, comment 3877608728).
    #[test]
    fn tool_calls_finish_reason_with_empty_array_beside_content_preamble_errors_instead_of_dropping_action()
     {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "Let me check that for you." }
                    ],
                    "tool_calls": []
                }
            }]
        });
        let err = model_response_from_payload(payload).expect_err(
            "a tool_calls finish reason with an empty call array must not let a content \
             preamble stand in for the requested action",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("tool_calls"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// Legacy sibling of the above: `finish_reason: "function_call"` with no
    /// `message.function_call` field at all, beside a nonempty array-shaped
    /// `content` preamble.
    #[test]
    fn function_call_finish_reason_with_missing_call_body_beside_content_preamble_errors_instead_of_dropping_action()
     {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "function_call",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "Let me check that for you." }
                    ]
                }
            }]
        });
        let err = model_response_from_payload(payload).expect_err(
            "a function_call finish reason with no call body must not let a content preamble \
             stand in for the requested action",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("function_call"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// The legacy sibling of the above: `finish_reason: "function_call"` with
    /// no `message.function_call` field present at all.
    #[test]
    fn function_call_finish_reason_with_missing_call_body_errors_instead_of_promoting() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "function_call",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "I should call the weather function"
                }
            }]
        });
        let err = model_response_from_payload(payload).expect_err(
            "a function_call finish reason with no call body must not promote reasoning",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("function_call"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// Any other non-success finish reason — including ones this module does
    /// not name explicitly — must fail closed rather than be assumed safe to
    /// promote. The guard is an allow-list of genuine textual completions
    /// (`stop` only — see [`model_response_from_payload`] for why
    /// `tool_calls`/`function_call` are excluded), not a blocklist of known
    /// failures, so an unrecognized value never silently promotes reasoning.
    #[test]
    fn unrecognized_finish_reason_reasoning_only_turn_errors() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "error",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "Working through it"
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("unrecognized finish_reason must not promote reasoning to an answer");
        let msg = err.to_string();
        assert!(
            msg.contains("error"),
            "error must name finish_reason for diagnosis, got: {msg}"
        );
    }

    /// A missing `finish_reason` altogether is unproven, not proven-complete —
    /// the allow-list requires an explicit good status, so this must also fail
    /// closed rather than assume the omission means success.
    #[test]
    fn missing_finish_reason_reasoning_only_turn_errors() {
        let payload = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "Working through it"
                }
            }]
        });
        let err = model_response_from_payload(payload)
            .expect_err("missing finish_reason must not promote reasoning to an answer");
        let msg = err.to_string();
        assert!(
            !msg.contains("finish_reason"),
            "no finish_reason detail should be appended when none was present, got: {msg}"
        );
    }

    /// A tool-call-only turn carries `content: null` and a `tool_calls` array.
    /// It must parse into a response whose message has no text block but the
    /// tool call intact (id, name, arguments parsed from the JSON string), so the
    /// harness's native tool loop can dispatch it. This is the core of bug #1:
    /// previously the null content hard-errored and the tool call was dropped.
    #[test]
    fn parses_tool_call_only_response_with_null_content() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "check_inventory",
                            "arguments": "{\"sku\":\"A-1\"}"
                        }
                    }]
                }
            }]
        });
        let resp = model_response_from_payload(payload).expect("parses tool-call-only turn");
        assert_eq!(resp.text(), "", "no visible text on a tool-call-only turn");
        let calls = resp.tool_calls();
        assert_eq!(calls.len(), 1, "the tool call survives parsing");
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].name, "check_inventory");
        assert_eq!(calls[0].arguments, serde_json::json!({ "sku": "A-1" }));
        assert!(calls[0].invalid.is_none());
        assert_eq!(resp.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn request_approval_with_siblings_refuses_the_whole_model_response() {
        let payload = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {"id":"c1","type":"function","function":{"name":"shell","arguments":"{}"}},
                        {"id":"c2","type":"function","function":{"name":"request_approval","arguments":"{\"title\":\"Run\",\"question\":\"Proceed?\"}"}}
                    ]
                }
            }]
        });
        let error = model_response_from_payload(payload)
            .expect_err("an approval boundary cannot share one tool-call batch");
        assert!(error.to_string().contains("sibling tool calls"));
    }

    /// A missing/empty tool-call `id` is back-filled with a stable `tool-{index}`
    /// slot id so the tool result can still correlate, and unparseable arguments
    /// are preserved + flagged `invalid` rather than dropping the call.
    #[test]
    fn tool_call_id_backfill_and_invalid_arguments_are_tolerated() {
        let payload = serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": { "name": "do_thing", "arguments": "{not json" }
                    }]
                }
            }]
        });
        let resp = model_response_from_payload(payload).expect("parses");
        let calls = resp.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "tool-0", "missing id back-fills to slot id");
        assert!(
            calls[0].invalid.is_some(),
            "unparseable arguments flag the call instead of dropping it"
        );
        assert_eq!(
            calls[0].arguments,
            serde_json::Value::String("{not json".to_string()),
            "raw arguments preserved for model retry"
        );
    }

    /// Exposed tools serialize into the OpenAI `tools[]`/`tool_choice` wire
    /// shape, and a multi-turn tool history (assistant `tool_calls` + a `tool`
    /// result) round-trips through the outbound message mapping. Together these
    /// are the outbound half of native tool calling.
    #[test]
    fn tools_and_tool_history_serialize_to_openai_wire() {
        use tinyagents::harness::message::Message;

        let tools = wire_tools(&[ToolSchema {
            name: "check_inventory".to_string(),
            description: "look up stock".to_string(),
            parameters: serde_json::json!({ "type": "object" }),
            format: tinyagents::harness::tool::ToolFormat::default(),
        }]);
        let mut body = serde_json::json!({ "model": "chat-v1" });
        attach_tools(&mut body, tools, &ToolChoice::Required, true);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "check_inventory");
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["parallel_tool_calls"], false);
        let mut unsupported = serde_json::json!({ "model": "local" });
        attach_tools(
            &mut unsupported,
            wire_tools(&[ToolSchema {
                name: "check_inventory".to_string(),
                description: "look up stock".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
                format: tinyagents::harness::tool::ToolFormat::default(),
            }]),
            &ToolChoice::Required,
            false,
        );
        assert!(unsupported.get("parallel_tool_calls").is_none());

        // An assistant tool-call turn → null content + wire tool_calls; the tool
        // result → a `tool` role message carrying its `tool_call_id`.
        let assistant = Message::Assistant(AssistantMessage {
            id: None,
            content: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "check_inventory".to_string(),
                arguments: serde_json::json!({ "sku": "A-1" }),
                invalid: None,
            }],
            usage: None,
        });
        let tool_result = Message::tool("call_1", "3 in stock");
        let wire = wire_messages(&[assistant, tool_result]);
        assert_eq!(wire[0]["role"], "assistant");
        assert!(
            wire[0]["content"].is_null(),
            "tool-call-only turn has null content"
        );
        assert_eq!(wire[0]["tool_calls"][0]["id"], "call_1");
        // OpenAI requires arguments as a JSON string, not an object.
        assert_eq!(
            wire[0]["tool_calls"][0]["function"]["arguments"],
            "{\"sku\":\"A-1\"}"
        );
        assert_eq!(wire[1]["role"], "tool");
        assert_eq!(wire[1]["tool_call_id"], "call_1");
        assert_eq!(wire[1]["content"], "3 in stock");
    }

    /// The hosted provider must advertise native tool calling, since openhuman's
    /// turn loop derives `native_tools` from `profile().tool_calling` — without it
    /// the harness silently falls back to prompt-guided XML (bug #1's mechanism).
    #[test]
    fn hosted_provider_advertises_native_tool_calling() {
        let provider = HostedProvider::new(HostedProviderConfig {
            base_url: "https://example.test/v1".to_string(),
            credential: Credential::None,
            extra_headers: Vec::new(),
        });
        let profile = provider.profile().expect("hosted profile is advertised");
        assert!(
            profile.tool_calling,
            "native tool calling must be advertised"
        );
        assert!(
            !profile.parallel_tool_calls,
            "one native call per assistant message keeps request_approval a turn boundary"
        );
    }

    /// The profile must advertise a context window because it activates
    /// `ContextCompressionMiddleware` and `ImageAwareMessageTrimMiddleware`.
    /// A missing value leaves intra-turn history unbounded and can end in the
    /// observed silent provider failure: HTTP 200, `finish_reason: "failed"`, an
    /// empty response, and zero usage.
    #[test]
    fn both_providers_advertise_the_same_context_window() {
        let expected = super::context_window();
        // `OPENCOMPANY_CONTEXT_WINDOW=off|0` is the documented escape hatch that
        // restores unbounded history, so `None` is legitimate only there; every
        // other environment must still advertise a window.
        let explicitly_disabled = std::env::var("OPENCOMPANY_CONTEXT_WINDOW")
            .map(|raw| {
                let raw = raw.trim();
                raw.eq_ignore_ascii_case("off") || raw == "0"
            })
            .unwrap_or(false);
        if explicitly_disabled {
            assert_eq!(
                expected, None,
                "OPENCOMPANY_CONTEXT_WINDOW=off|0 must disable the window"
            );
        } else {
            assert!(
                expected.is_some(),
                "the default profile must advertise a context window"
            );
        }
        let hosted = HostedProvider::new(HostedProviderConfig {
            base_url: "https://example.test/v1".to_string(),
            credential: Credential::None,
            extra_headers: Vec::new(),
        });
        assert_eq!(
            hosted
                .profile()
                .expect("hosted profile is advertised")
                .max_input_tokens,
            expected
        );
        // TenantProvider returns the same `MANAGED_PROFILE`, so tenant-provided
        // credentials receive the same history protection as the hosted route.
        assert_eq!(*MANAGED_PROFILE_WINDOW, expected);
    }

    /// Read the static profile directly to verify that both `profile()`
    /// implementations draw from the same source.
    static MANAGED_PROFILE_WINDOW: std::sync::LazyLock<Option<u64>> =
        std::sync::LazyLock::new(|| super::MANAGED_PROFILE.max_input_tokens);

    /// A stub that records the `Authorization` header of every request it
    /// answers, so a test can prove which bearer actually went out.
    async fn spawn_auth_recorder() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::http::HeaderMap;
        use axum::routing::post;
        use axum::{Json, Router};

        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let app = Router::new().route(
            "/chat/completions",
            post(move |headers: HeaderMap| {
                let log = Arc::clone(&log);
                async move {
                    log.lock().unwrap().push(
                        headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string(),
                    );
                    Json(serde_json::json!({
                        "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
                    }))
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

    /// The rotation contract at the transport: the SAME provider instance must
    /// present the token the projected file holds **now**, not the one it held
    /// when the provider was built. Without this, a hosted pod keeps sending a
    /// bearer the cluster rotated away from and every turn 401s.
    #[tokio::test]
    async fn hosted_provider_resolves_the_bearer_per_request() {
        let (url, seen) = spawn_auth_recorder().await;
        let dir = tempfile::Builder::new()
            .prefix("oc-prov-rot-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
        // No `exp` to read ⇒ never cached ⇒ every request re-reads the file.
        std::fs::write(&path, "token-before-rotation").unwrap();

        let provider = HostedProvider::new(HostedProviderConfig {
            base_url: url,
            credential: Credential::from_source(Arc::new(TinyhumansTokenSource::projected_file(
                &path,
            ))),
            extra_headers: Vec::new(),
        });

        provider.invoke(&(), user_request("one")).await.expect("t1");
        std::fs::write(&path, "token-after-rotation").unwrap();
        provider.invoke(&(), user_request("two")).await.expect("t2");

        let headers = seen.lock().unwrap().clone();
        assert_eq!(
            headers,
            vec![
                "Bearer token-before-rotation".to_string(),
                "Bearer token-after-rotation".to_string()
            ],
            "the bearer must be resolved per request, not captured at build time"
        );
    }

    /// With no credential at all the header is omitted rather than sent empty.
    #[tokio::test]
    async fn hosted_provider_omits_the_bearer_without_a_credential() {
        let (url, seen) = spawn_auth_recorder().await;
        let provider = HostedProvider::new(HostedProviderConfig {
            base_url: url,
            credential: Credential::None,
            extra_headers: Vec::new(),
        });
        provider
            .invoke(&(), user_request("hi"))
            .await
            .expect("turn");
        assert_eq!(seen.lock().unwrap().clone(), vec![String::new()]);
    }

    /// A 401 invalidates the cached read, so the next turn presents whatever the
    /// file holds now instead of re-sending a bearer the backend just refused —
    /// the recovery path for a token the platform rotated early.
    #[tokio::test]
    async fn a_rejected_bearer_forces_a_re_read_on_the_next_turn() {
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};

        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let app = Router::new().route(
            "/chat/completions",
            post(move |headers: HeaderMap| {
                let log = Arc::clone(&log);
                async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let first = {
                        let mut guard = log.lock().unwrap();
                        guard.push(auth);
                        guard.len() == 1
                    };
                    // Refuse the first bearer, accept the second.
                    if first {
                        (StatusCode::UNAUTHORIZED, Json(serde_json::json!({}))).into_response()
                    } else {
                        Json(serde_json::json!({
                            "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
                        }))
                        .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempfile::Builder::new()
            .prefix("oc-prov-401-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
        // A long-lived `exp` ⇒ the window would normally hold this read for the
        // full cap, so only invalidation can explain the second value going out.
        let long_lived = jwt_with_exp(60 * 60 * 24 * 365 * 100);
        std::fs::write(&path, format!("stale-{long_lived}")).unwrap();

        let provider = HostedProvider::new(HostedProviderConfig {
            base_url: format!("http://{addr}"),
            credential: Credential::from_source(Arc::new(TinyhumansTokenSource::projected_file(
                &path,
            ))),
            extra_headers: Vec::new(),
        });

        provider
            .invoke(&(), user_request("one"))
            .await
            .expect_err("first turn is refused");
        std::fs::write(&path, format!("rotated-{long_lived}")).unwrap();
        provider.invoke(&(), user_request("two")).await.expect("t2");

        let headers = seen.lock().unwrap().clone();
        assert_eq!(headers.len(), 2, "{headers:?}");
        assert!(headers[0].starts_with("Bearer stale-"), "{headers:?}");
        assert!(
            headers[1].starts_with("Bearer rotated-"),
            "a 401 must send the next turn back to the file: {headers:?}"
        );
    }

    /// An unreadable projected file fails the turn with a model error that names
    /// the problem — it must never silently send no bearer and get a confusing
    /// 401 from the backend instead.
    #[tokio::test]
    async fn hosted_provider_surfaces_an_unreadable_token_file() {
        let provider = HostedProvider::new(HostedProviderConfig {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            credential: Credential::from_source(Arc::new(TinyhumansTokenSource::projected_file(
                "/nonexistent/oc/token",
            ))),
            extra_headers: Vec::new(),
        });
        let err = provider
            .invoke(&(), user_request("hi"))
            .await
            .expect_err("unreadable credential");
        assert!(err.to_string().contains("credential"), "{err}");
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

        let plan = request_plan(
            &decl,
            "chat-v1",
            Vec::new(),
            0.2,
            None,
            Vec::new(),
            &ToolChoice::Auto,
        )
        .await
        .expect("plan");
        assert_eq!(
            plan.model, "deepseek/deepseek-chat",
            "tier maps through table"
        );
        // A toolless turn omits both `tools` and `tool_choice` entirely.
        assert!(
            plan.body.get("tools").is_none(),
            "no tools key when toolless"
        );
        assert!(
            plan.body.get("tool_choice").is_none(),
            "no tool_choice without tools"
        );
        assert!(
            plan.body.get("parallel_tool_calls").is_none(),
            "no parallel-tool setting without tools"
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

        // A tier the manifest does not map takes the shipped default rather than
        // passing through as a bare tier name. It used to pass through, which
        // worked only because the platform endpoint resolved tier names; this
        // decl is DIRECT, and OpenRouter has never heard of `reasoning-v1`.
        let defaulted = request_plan(
            &decl,
            "reasoning-v1",
            Vec::new(),
            0.2,
            None,
            Vec::new(),
            &ToolChoice::Auto,
        )
        .await
        .expect("plan");
        assert_eq!(defaulted.model, "openai/gpt-5.6-sol-pro");

        // A concrete slug is still forwarded untouched, so a caller can name any
        // model in OpenRouter's catalog.
        let explicit = request_plan(
            &decl,
            "anthropic/claude-sonnet-4.5",
            Vec::new(),
            0.2,
            None,
            Vec::new(),
            &ToolChoice::Auto,
        )
        .await
        .expect("plan");
        assert_eq!(explicit.model, "anthropic/claude-sonnet-4.5");
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
        let plan = request_plan(
            &decl,
            "chat-v1",
            Vec::new(),
            0.0,
            None,
            Vec::new(),
            &ToolChoice::Auto,
        )
        .await
        .expect("plan");
        assert!(plan.bearer.is_none(), "keyless Ollama sends no bearer");
        assert!(plan.headers.is_empty(), "no OpenRouter headers for Ollama");
    }

    /// The positive half of issue #376 (AC #1): a **proxied** config targets a
    /// TinyHumans-owned endpoint, so [`request_plan`] must attach our
    /// `x-sdk-name: opencompany` product header alongside the tier's other
    /// headers.
    ///
    /// After `managed`'s removal the provider *kind* no longer tells our
    /// endpoint from OpenRouter's — the same `openrouter` kind reaches both.
    /// `is_proxied()` is what distinguishes them, and this test and its negative
    /// twin below pin both sides of that one bit.
    #[tokio::test]
    async fn request_plan_attaches_the_product_header_when_proxied() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = crate::company::inference::EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        // A keyless `openrouter` resolves through the env default — this is the
        // shape a real company manifest produces, not a synthetic decl, and it
        // is the config a company that has configured nothing runs on.
        let decl = inference::resolve_effective(
            &company,
            &manifest_inference("openrouter"),
            Some(&env),
            &secrets,
        )
        .await
        .unwrap()
        .expect("keyless openrouter resolves via the env default");
        assert!(decl.is_proxied());

        let plan = request_plan(
            &decl,
            "chat-v1",
            Vec::new(),
            0.2,
            None,
            Vec::new(),
            &ToolChoice::Auto,
        )
        .await
        .expect("plan");
        assert!(
            plan.headers
                .contains(&("x-sdk-name", "opencompany".to_string())),
            "a proxied config must carry the product header: {:?}",
            plan.headers
        );
        assert!(
            plan.headers
                .contains(&("HTTP-Referer", OPENROUTER_REFERER.to_string())),
            "and OpenRouter's own attribution rides the proxied path too: {:?}",
            plan.headers
        );
    }

    /// The negative half of issue #376 (AC #1) — and the important one, per
    /// the task: `openrouter` and `openai_compatible` are bring-your-own-key
    /// THIRD-PARTY endpoints (OpenRouter's own API, and any OpenAI-compatible
    /// host an operator points at — OpenAI, DeepSeek, a self-hosted proxy,
    /// …). Sending them our product identity would tell a company we have no
    /// relationship with which product a tenant is running, for no benefit to
    /// anyone. Only a **proxied** config (see the test above) may ever carry
    /// the header — and note the first case here is the *same provider kind* as
    /// that test, differing only in holding a tenant key. That is precisely the
    /// distinction this rule now turns on.
    #[tokio::test]
    async fn request_plan_never_attaches_the_product_header_for_third_party_providers() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();

        // openrouter DIRECT (the tenant's own key): gets ITS OWN attribution
        // headers, never ours.
        let mut or_manifest = manifest_inference("openrouter");
        or_manifest.models =
            BTreeMap::from([("chat-v1".to_string(), "deepseek/deepseek-chat".to_string())]);
        inference::store_key(&company, &secrets, "or-key")
            .await
            .unwrap();
        let or_decl = inference::resolve_effective(&company, &or_manifest, None, &secrets)
            .await
            .unwrap()
            .unwrap();
        let or_plan = request_plan(
            &or_decl,
            "chat-v1",
            Vec::new(),
            0.2,
            None,
            Vec::new(),
            &ToolChoice::Auto,
        )
        .await
        .expect("plan");
        assert!(
            !or_plan
                .headers
                .iter()
                .any(|(name, _)| *name == "x-sdk-name"),
            "openrouter is third-party and must never see our product identity: {:?}",
            or_plan.headers
        );
        assert!(
            or_plan
                .headers
                .contains(&("HTTP-Referer", OPENROUTER_REFERER.to_string())),
            "openrouter's own attribution headers must be unaffected: {:?}",
            or_plan.headers
        );

        // openai_compatible: a bring-your-own-endpoint host — no headers at all.
        let mut compat_manifest = manifest_inference("openai_compatible");
        compat_manifest.base_url = Some("https://byok.example/v1".into());
        let compat_decl = inference::resolve_effective(&company, &compat_manifest, None, &secrets)
            .await
            .unwrap()
            .unwrap();
        let compat_plan = request_plan(
            &compat_decl,
            "chat-v1",
            Vec::new(),
            0.2,
            None,
            Vec::new(),
            &ToolChoice::Auto,
        )
        .await
        .expect("plan");
        assert!(
            compat_plan.headers.is_empty(),
            "openai_compatible is third-party and must carry no headers at all: {:?}",
            compat_plan.headers
        );
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

    /// Spawns an in-process OpenAI-compatible stub whose `message.content` is
    /// the given raw JSON value rather than a plain string — used to exercise
    /// the array-of-text-parts content shape end to end.
    async fn spawn_stub_content(content: serde_json::Value) -> String {
        use axum::routing::post;
        use axum::{Json, Router};

        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let content = content.clone();
                async move {
                    Json(serde_json::json!({
                        "choices": [{
                            "finish_reason": "stop",
                            "message": { "role": "assistant", "content": content }
                        }],
                        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// Spawns an in-process OpenAI-compatible stub whose full `message` object
    /// is the given raw JSON value — used to exercise shapes `spawn_stub_content`
    /// cannot, such as a reasoning-only turn (`content: null` with the visible
    /// text under `reasoning`/`reasoning_content` instead).
    async fn spawn_stub_message(message: serde_json::Value) -> String {
        use axum::routing::post;
        use axum::{Json, Router};

        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let message = message.clone();
                async move {
                    Json(serde_json::json!({
                        "choices": [{
                            "finish_reason": "stop",
                            "message": message
                        }],
                        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
                    }))
                }
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

    /// Issue #1749: the model half of the same live-attribution contract, and
    /// the BYOK containment it exists for.
    ///
    /// A tenant `[inference].models` entry is **operator free text** — this one
    /// is named after a customer, which is exactly the shape of the leak. The
    /// provider must report a vocabulary member for it, and the raw name must
    /// not appear anywhere in what the meter would persist.
    #[tokio::test]
    async fn a_tenant_model_is_reported_as_a_slug_and_never_as_the_operators_name() {
        let url = spawn_stub("ok").await;
        let company = CompanyId::new("acme");
        let secrets: Arc<dyn SecretStore> = Arc::new(MemSecrets::default());
        let mut manifest = manifest_inference("openai_compatible");
        manifest.base_url = Some(url.clone());
        let provider = TenantProvider::new(company.clone(), secrets.clone(), manifest, None);

        assert_eq!(
            provider.telemetry_model(),
            None,
            "no turn has run, so there is no model to name"
        );

        let save = |model: &str| {
            let mut models = BTreeMap::new();
            models.insert("chat-v1".to_string(), model.to_string());
            let secrets = Arc::clone(&secrets);
            let company = company.clone();
            let url = url.clone();
            async move {
                inference::save_runtime_config(
                    &company,
                    secrets.as_ref(),
                    &inference::RuntimeInference {
                        provider: "openai_compatible".into(),
                        base_url: Some(url),
                        models,
                    },
                )
                .await
                .unwrap();
            }
        };

        // A self-hosted model named after the customer it was built for.
        save("northwind-legal-review-v2").await;
        provider
            .invoke(&(), user_request("hi"))
            .await
            .expect("turn");
        assert_eq!(
            provider.telemetry_model(),
            Some(crate::metering::ModelSlug::OTHER),
            "a model this build cannot name reports the fallback"
        );
        let sample = crate::metering::inference_sample(
            &crate::ports::types::TokenUsage {
                input: 10,
                output: 5,
                cached_input: 0,
                cost_usd: 0.01,
            },
            "ceo",
            &provider.telemetry_provider_id(),
            provider.telemetry_model(),
        )
        .expect("a real turn meters");
        let persisted = serde_json::to_string(&sample).expect("serialize");
        assert!(
            !persisted.to_ascii_lowercase().contains("northwind"),
            "the operator's model name reached what the meter persists: {persisted}"
        );

        // …and a model the vocabulary does know, through the same path.
        save("anthropic/claude-sonnet-4-6").await;
        provider
            .invoke(&(), user_request("hi"))
            .await
            .expect("turn");
        assert_eq!(
            provider.telemetry_model().map(|m| m.as_str()),
            Some("anthropic-sonnet"),
            "a table switch re-attributes the next turn, exactly as the provider slug does"
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

    /// The product-identity contract at the transport: `HostedProvider::invoke`
    /// — the sole production inference path — must tag every chat-completions
    /// request with `x-sdk-name: opencompany`, mirroring the embeddings client
    /// and the openhuman-core call sites. This is the header the platform uses
    /// to attribute backend traffic to the `opencompany` SDK.
    #[tokio::test]
    async fn hosted_provider_invoke_carries_the_product_identity_header() {
        use axum::http::HeaderMap;
        use axum::routing::post;
        use axum::{Json, Router};

        let seen: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let capture = Arc::clone(&seen);
        let app = Router::new().route(
            "/chat/completions",
            post(move |headers: HeaderMap| {
                let capture = Arc::clone(&capture);
                async move {
                    *capture.lock().unwrap() = headers
                        .get(crate::product::PRODUCT_IDENTITY_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    Json(serde_json::json!({
                        "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let provider = HostedProvider::new(HostedProviderConfig {
            base_url: format!("http://{addr}"),
            credential: Credential::None,
            extra_headers: Vec::new(),
        });
        provider
            .invoke(&(), user_request("hi"))
            .await
            .expect("turn against the stub");

        assert_eq!(
            seen.lock().unwrap().as_deref(),
            Some(crate::product::PRODUCT_IDENTITY),
            "every hosted chat-completions request must attach the product identity header"
        );
    }

    /// A stub that rejects every chat-completion with `status`, the way an
    /// early-rotated bearer is rejected in production.
    async fn spawn_rejecting_stub(status: axum::http::StatusCode) -> String {
        use axum::Router;
        use axum::routing::post;

        let app = Router::new().route(
            "/chat/completions",
            post(move || async move { (status, "rejected") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// Issue #1749, the concurrency half: a turn that **failed** must not
    /// publish its model into the shared cache.
    ///
    /// One `TenantProvider` is shared by every agent on a company, and
    /// `telemetry_model()` is read after a turn finishes — by whichever turn
    /// finishes, not necessarily the one that wrote last. So publishing before
    /// the request is issued lets a rejected turn (or one still in flight) name
    /// the model for a *different* agent's successful turn, attributing real
    /// tokens to a model that produced none. That is strictly worse than the
    /// documented approximation, which is bounded to two models that both ran.
    ///
    /// A failed turn meters nothing of its own, so the honest state after one
    /// is the last **successful** turn's model, unchanged.
    #[tokio::test]
    async fn a_rejected_tenant_turn_leaves_the_last_successful_model_in_place() {
        let ok = spawn_stub("ok").await;
        let rejecting = spawn_rejecting_stub(axum::http::StatusCode::UNAUTHORIZED).await;

        let company = CompanyId::new("acme");
        let secrets: Arc<dyn SecretStore> = Arc::new(MemSecrets::default());
        let mut manifest = manifest_inference("openai_compatible");
        manifest.base_url = Some(ok.clone());
        let provider = TenantProvider::new(company.clone(), secrets.clone(), manifest, None);

        let point_at = |base_url: String, model: &str| {
            let mut models = BTreeMap::new();
            models.insert("chat-v1".to_string(), model.to_string());
            let secrets = Arc::clone(&secrets);
            let company = company.clone();
            async move {
                inference::save_runtime_config(
                    &company,
                    secrets.as_ref(),
                    &inference::RuntimeInference {
                        provider: "openai_compatible".into(),
                        base_url: Some(base_url),
                        models,
                    },
                )
                .await
                .unwrap();
            }
        };

        // A turn that runs, on a model the vocabulary names.
        point_at(ok.clone(), "anthropic/claude-sonnet-4-6").await;
        provider
            .invoke(&(), user_request("hi"))
            .await
            .expect("the successful turn");
        assert_eq!(
            provider.telemetry_model().map(|m| m.as_str()),
            Some("anthropic-sonnet"),
            "a completed turn names its model"
        );

        // …then a turn on a *differently* named model that the endpoint
        // rejects outright.
        point_at(rejecting.clone(), "openai/gpt-5.2").await;
        let err = provider
            .invoke(&(), user_request("hi"))
            .await
            .expect_err("the endpoint rejects this turn");
        assert!(err.to_string().contains("401"), "{err}");

        assert_eq!(
            provider.telemetry_model().map(|m| m.as_str()),
            Some("anthropic-sonnet"),
            "a rejected turn produced no usage, so it must not overwrite the \
             model of the turn that did run — a concurrent agent's cost hook \
             reads this value"
        );
    }

    /// The same contract on [`HostedProvider`], whose cache is behind an `Arc`
    /// precisely so every clone of the handle shares it — which is what makes a
    /// premature write observable by another agent's turn.
    #[tokio::test]
    async fn a_rejected_hosted_turn_leaves_the_last_successful_model_in_place() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Answers the first turn and rejects every one after it, so a single
        // endpoint gives us one success followed by one 401.
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let counter = Arc::clone(&counter);
                async move {
                    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                        Json(serde_json::json!({
                            "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
                        }))
                        .into_response()
                    } else {
                        (StatusCode::UNAUTHORIZED, "rotated").into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let provider = HostedProvider::new(HostedProviderConfig {
            base_url: format!("http://{addr}"),
            credential: Credential::None,
            extra_headers: Vec::new(),
        });

        let asking_for = |model: &str| ModelRequest {
            model: Some(model.to_string()),
            ..user_request("hi")
        };

        provider
            .invoke(&(), asking_for("anthropic/claude-sonnet-4-6"))
            .await
            .expect("the successful turn");
        assert_eq!(
            provider.telemetry_model().map(|m| m.as_str()),
            Some("anthropic-sonnet"),
            "a completed turn names its model"
        );

        let err = provider
            .invoke(&(), asking_for("openai/gpt-5.2"))
            .await
            .expect_err("the endpoint rejects this turn");
        assert!(err.to_string().contains("401"), "{err}");

        assert_eq!(
            provider.telemetry_model().map(|m| m.as_str()),
            Some("anthropic-sonnet"),
            "a rejected turn produced no usage, so it must not overwrite the \
             model of the turn that did run — every clone of this handle shares \
             the cache it would have overwritten"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "both turns reached the stub"
        );
    }

    /// Codex review on #1779 (comment 3864824480): `model_response_from_payload`
    /// learned to parse array-shaped `content` (`parses_content_as_array_of_text_parts`
    /// above), but `probe` — the setup wizard's and the console's "Test" button
    /// connectivity check — still read `content.as_str()` directly. An endpoint
    /// answering with array-shaped content therefore passed every real turn
    /// while its own connection probe reported the connection broken. `probe`
    /// must route through `model_response_from_payload` itself, the same
    /// parser the turn path calls, rather than any narrower stand-in for it.
    #[tokio::test]
    async fn probe_accepts_array_shaped_content() {
        let url = spawn_stub_content(serde_json::json!([
            { "type": "text", "text": "pong" }
        ]))
        .await;

        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let mut manifest = manifest_inference("openai_compatible");
        manifest.base_url = Some(url);
        let decl = inference::resolve_effective(&company, &manifest, None, &secrets)
            .await
            .unwrap()
            .unwrap();

        probe(&decl, None)
            .await
            .expect("array-shaped content must be recognized as a successful probe");
    }

    /// Codex review on #1779 (comment 3864906472): the array-content fix above
    /// made `probe` call `extract_content_text` directly instead of the shared
    /// `model_response_from_payload` — which picked up the array-shaped-content
    /// case but not the `reasoning`/`reasoning_content` fallback for a
    /// reasoning-only turn (`content: null`, `finish_reason: "stop"`, visible
    /// text under `reasoning`) that lives inside `model_response_from_payload`.
    /// A managed reasoning provider answering with that shape passed every
    /// real turn while its own connection probe reported the connection
    /// broken — blocking the setup wizard and the console's "Test" button for
    /// a valid provider. `probe` must route through the exact same parser the
    /// turn path calls so the two paths cannot diverge again.
    #[tokio::test]
    async fn probe_accepts_reasoning_only_content() {
        let url = spawn_stub_message(serde_json::json!({
            "role": "assistant",
            "content": null,
            "reasoning": "42 is the answer."
        }))
        .await;

        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let mut manifest = manifest_inference("openai_compatible");
        manifest.base_url = Some(url);
        let decl = inference::resolve_effective(&company, &manifest, None, &secrets)
            .await
            .unwrap()
            .unwrap();

        probe(&decl, None)
            .await
            .expect("reasoning-only content must be recognized as a successful probe");
    }

    /// CodeRabbit review on #1779 (comment 3877827976): `probe` routes
    /// through the shared `model_response_from_payload`, which is correct
    /// for a real turn but accepts a tool-call-only reply as a success —
    /// tool calls are a valid outcome when the caller offered tools. `probe`
    /// offers none (`Vec::new()`), so an endpoint answering `ping` with a
    /// tool call instead of prose never actually answered the bare chat turn
    /// the probe exists to verify. Without an explicit check for visible
    /// text, the setup wizard or console Test action would report such an
    /// endpoint as reachable.
    #[tokio::test]
    async fn probe_rejects_tool_call_only_reply() {
        let url = spawn_stub_message(serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "lookup_weather", "arguments": "{}" }
                }
            ]
        }))
        .await;

        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let mut manifest = manifest_inference("openai_compatible");
        manifest.base_url = Some(url);
        let decl = inference::resolve_effective(&company, &manifest, None, &secrets)
            .await
            .unwrap()
            .unwrap();

        let err = probe(&decl, None)
            .await
            .expect_err("a tool-call-only reply to a no-tools probe must not pass");
        assert!(
            err.to_string().contains("tool call"),
            "error should name why the probe failed: {err}"
        );
    }

    /// CodeRabbit review on #1779 (comment 3878355375): the tool-call guard
    /// above only checked `content.is_empty()`, which catches a tool-call-
    /// *only* reply but not a mixed one — a text preamble alongside a
    /// genuinely parsed tool call. `model_response_from_payload` accepts that
    /// combination for a real turn (the finish-reason-declares-an-action
    /// guard only fires when `tool_calls` fails to parse), so `content` comes
    /// back nonempty and the pre-fix check let it through even though the
    /// probe offered no tools and the endpoint still requested one. Must
    /// still fail the probe.
    #[tokio::test]
    async fn probe_rejects_tool_call_alongside_text_preamble() {
        let url = spawn_stub_message(serde_json::json!({
            "role": "assistant",
            "content": "Let me check that for you.",
            "tool_calls": [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "lookup_weather", "arguments": "{}" }
                }
            ]
        }))
        .await;

        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let mut manifest = manifest_inference("openai_compatible");
        manifest.base_url = Some(url);
        let decl = inference::resolve_effective(&company, &manifest, None, &secrets)
            .await
            .unwrap()
            .unwrap();

        let err = probe(&decl, None)
            .await
            .expect_err("a tool call alongside text in a no-tools probe must not pass");
        assert!(
            err.to_string().contains("tool call"),
            "error should name why the probe failed: {err}"
        );
    }

    /// Issue #1811: the managed backend's raw refusal for a model id that does
    /// not exist is rewritten into an actionable message that names the fix and
    /// keeps the provider's own words (the bad id + the list-models hint) at the
    /// end for support. No `harness` in play (the managed backend has no
    /// harness-scoped config), so the fix is named as the company mapping.
    #[test]
    fn a_missing_model_400_becomes_actionable() {
        let raw = concat!(
            "inference returned 400 Bad Request: ",
            r#"{"error":"Model 'deepseek/deepseek-v4-pro' is not available. "#,
            r#"Use GET /openai/v1/models to list available models.","errorCode":"BAD_REQUEST"}"#,
        );
        let advice = model_unavailable_advice(
            reqwest::StatusCode::BAD_REQUEST,
            raw,
            "https://api.tinyhumans.ai/openai/v1/models",
            None,
            None,
        )
        .expect("recognised as a missing model");
        assert!(
            !advice.contains("agent's model"),
            "a built_in harness never honours `agent.model` (`Manifest::validate`), so it must \
             not be suggested as a fix: {advice}"
        );
        assert!(
            advice.contains("the company's `[inference].models` mapping"),
            "with no harness scope, the company-level mapping is the only place to fix it: \
             {advice}"
        );
        assert!(
            advice.contains("deepseek/deepseek-v4-pro"),
            "the offending id survives for support: {advice}"
        );
        assert!(
            advice.contains("GET https://api.tinyhumans.ai/openai/v1/models"),
            "the caller-supplied catalog endpoint is used: {advice}"
        );
    }

    /// Codex review on #1824: a named `built_in` harness with its own
    /// `[harness.inference]` resolves independently of the company mapping
    /// (`resolve_effective_scoped`), so the advice must name *that* harness
    /// rather than blanket-pointing at `[inference].models` — the earlier wording
    /// sent its operator to a table the failing request never consulted, and
    /// separately suggested `agent.model`, which a `built_in` harness rejects
    /// outright. This assertion set does not compile against the pre-fix
    /// 3-argument `model_unavailable_advice`, i.e. it fails (to build) on the
    /// pre-fix code exactly as it must.
    #[test]
    fn scoped_harness_advice_names_its_own_harness() {
        let raw = "inference returned 400 Bad Request: openai/made-up is not a valid model ID";
        let advice = model_unavailable_advice(
            reqwest::StatusCode::BAD_REQUEST,
            raw,
            "https://openrouter.ai/api/v1/models",
            Some("research-harness"),
            Some(InferenceSource::Manifest),
        )
        .expect("recognised as a missing model");
        assert!(
            advice.contains("harness `research-harness`'s own `[harness.inference].models`"),
            "the failing harness is named, not just the company: {advice}"
        );
        assert!(
            advice.contains("the company's `[inference].models`"),
            "the company fallback (when the harness declares none of its own) is still \
             mentioned: {advice}"
        );
        assert!(
            !advice.contains("agent's model"),
            "still never suggests the non-lever `agent.model`: {advice}"
        );
    }

    /// Codex review on #1824 (round 2): a saved console runtime override
    /// outranks *both* manifest tables (`resolve_effective_scoped`'s
    /// precedence — runtime > manifest > env-default), so while one is active
    /// the earlier wording sent the operator to edit a `[harness.inference]` /
    /// `[inference]` table that is shadowed and would not change the outcome.
    /// This assertion set does not compile against the pre-fix 4-argument
    /// `model_unavailable_advice` (no `source` parameter), i.e. it fails to
    /// build on the pre-fix code exactly as it must.
    #[test]
    fn runtime_override_advice_names_the_override_not_the_shadowed_manifest() {
        let raw = "inference returned 400 Bad Request: openai/made-up is not a valid model ID";

        let scoped = model_unavailable_advice(
            reqwest::StatusCode::BAD_REQUEST,
            raw,
            "https://openrouter.ai/api/v1/models",
            Some("research-harness"),
            Some(InferenceSource::Runtime),
        )
        .expect("recognised as a missing model");
        assert!(
            scoped.contains("harness `research-harness`'s saved runtime inference override"),
            "the active override is named, not a shadowed manifest table: {scoped}"
        );
        assert!(
            !scoped.contains("update harness `research-harness`'s own `[harness.inference]"),
            "the manifest-table phrasing (the non-Runtime branch) must not be the suggested fix \
             while an override shadows it: {scoped}"
        );

        let default = model_unavailable_advice(
            reqwest::StatusCode::BAD_REQUEST,
            raw,
            "https://openrouter.ai/api/v1/models",
            None,
            Some(InferenceSource::Runtime),
        )
        .expect("recognised as a missing model");
        assert!(
            default.contains("update the saved runtime inference override"),
            "the company-scoped override is named: {default}"
        );
        assert!(
            !default.contains("update the company's `[inference].models` mapping"),
            "the manifest-table phrasing (the non-Runtime branch) must not be the suggested fix \
             while an override shadows it: {default}"
        );
    }

    /// Issue #1811 follow-up (Codex review on #1824): a direct OpenRouter,
    /// Ollama, or arbitrary `openai_compatible` BYOK endpoint must get *its own*
    /// catalog URL in the advice, not the TinyHumans-managed `/openai/v1/models`
    /// path. Before the fix this string was hard-coded regardless of
    /// `models_url`, so this assertion fails on the pre-fix code even though the
    /// raw provider error here (OpenRouter's own wording) never mentions
    /// `/openai/v1/models` at all.
    #[test]
    fn byok_provider_advice_points_at_its_own_catalog() {
        let raw = "inference returned 400 Bad Request: openai/made-up is not a valid model ID";
        let advice = model_unavailable_advice(
            reqwest::StatusCode::BAD_REQUEST,
            raw,
            "https://openrouter.ai/api/v1/models",
            None,
            None,
        )
        .expect("recognised as a missing model");
        assert!(
            advice.contains("GET https://openrouter.ai/api/v1/models"),
            "OpenRouter's own catalog endpoint is named: {advice}"
        );
        assert!(
            !advice.contains("openai/v1/models"),
            "the TinyHumans-managed path must not leak into a direct-provider hint: {advice}"
        );
    }

    /// The BYOK / OpenAI-compatible and OpenRouter phrasings for the same class
    /// are all recognised — the signature set is the provider's wording, not a
    /// catalogue of model ids.
    #[test]
    fn other_provider_phrasings_are_recognised() {
        for body in [
            "inference returned 404 Not Found: The model `gpt-9` does not exist",
            "inference returned 400 Bad Request: openai/made-up is not a valid model ID",
        ] {
            assert!(
                model_unavailable_advice(
                    reqwest::StatusCode::BAD_REQUEST,
                    body,
                    "https://example.com/v1/models",
                    None,
                    None,
                )
                .is_some(),
                "should be recognised as a missing model: {body}"
            );
        }
    }

    /// A valid model that fails for another reason must pass through untouched:
    /// a 401 (bad key), a 4xx about something other than a model, and any 5xx
    /// (the provider's own fault — reframing it as a config error would send the
    /// operator to change a model that is fine).
    #[test]
    fn unrelated_failures_are_left_alone() {
        assert_eq!(
            model_unavailable_advice(
                reqwest::StatusCode::UNAUTHORIZED,
                "inference returned 401 Unauthorized: invalid api key",
                "https://example.com/v1/models",
                None,
                None,
            ),
            None,
            "a bad key is not a missing model"
        );
        assert_eq!(
            model_unavailable_advice(
                reqwest::StatusCode::BAD_REQUEST,
                "inference returned 400 Bad Request: user does not exist",
                "https://example.com/v1/models",
                None,
                None,
            ),
            None,
            "a 4xx that never names a model is not a missing model"
        );
        assert_eq!(
            model_unavailable_advice(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                "inference returned 500: the model host crashed",
                "https://example.com/v1/models",
                None,
                None,
            ),
            None,
            "a 5xx is the provider's fault, not the operator's config"
        );
    }

    /// Spawns a stub that rejects every chat completion with a
    /// provider-flavoured "model not available" 400, the shape `probe`'s
    /// caller (the console "Test" button) hits when an operator has typed a
    /// model id the endpoint does not serve.
    async fn spawn_model_unavailable_stub() -> String {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;

        let app = Router::new().route(
            "/chat/completions",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({
                        "error": "Model 'gpt-5.9-ghost' is not available. Use GET /v1/models to \
                                  list available models."
                    })
                    .to_string(),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// `probe`'s repair hint must name the harness whose table the failing
    /// request actually read (Codex review on #1824's #1811 follow-up):
    /// `test_config` (src/server/ops/inference.rs) resolves against the
    /// company's default harness and now threads its real id through, the
    /// same distinction `TenantProvider::invoke` already makes for live
    /// turns. Before the fix `probe` hard-coded `None` for every caller, so a
    /// company whose default harness declares its own `[harness.inference]`
    /// got a repair hint pointing at the company-level `[inference].models`
    /// table — one its request never consulted.
    #[tokio::test]
    async fn probe_names_the_harness_that_owns_the_failing_config() {
        let base_url = spawn_model_unavailable_stub().await;
        let decl = inference::decl_for_probe("openai_compatible", Some(&base_url), None, None);

        let err = probe(&decl, Some("embedded"))
            .await
            .expect_err("the stub rejects every model");
        assert!(
            err.to_string().contains("harness `embedded`"),
            "the hint must name the owning harness: {err}"
        );

        let err = probe(&decl, None)
            .await
            .expect_err("the stub rejects every model");
        assert!(
            !err.to_string().contains("harness `"),
            "with no harness in play (the first-run wizard), the hint must not invent one: {err}"
        );
    }
}
