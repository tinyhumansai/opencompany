//! Inference model implementations for the embedded harness.
//!
//! openhuman's inference now runs on tinyinference [`ChatModel<()>`] (the old
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

use tinyinference::message::{AssistantMessage, ContentBlock, Message};
use tinyinference::model::{
    ChatModel, Modalities, ModelProfile, ModelRequest, ModelResponse, ProviderError, ToolChoice,
};
use tinyinference::tool::{ToolCall, ToolSchema};
use tinyinference::usage::Usage;
use tinyinference::{Error as InferenceError, Result as TaResult};

use crate::app::config::EnvSource;
use crate::company::Inference;
use crate::company::credentials::{Credential, TinyhumansTokenSource};
use crate::company::inference::{self, EnvDefault, InferenceDecl, InferenceSource};
use crate::ports::SecretStore;
use crate::ports::types::CompanyId;

/// Default hosted inference endpoint when only a bare `TINYHUMANS_API_KEY` is
/// supplied: the TinyHumans OpenRouter proxy, the same endpoint as
/// [`inference::PLATFORM_BASE_URL`]. Production value; fallbacks use
/// [`inference::platform_base_url`] so a configured `TINYHUMANS_API_URL` wins.
pub const DEFAULT_TINYHUMANS_INFERENCE_URL: &str = inference::PLATFORM_BASE_URL;

/// Default hosted model/tier when none is configured.
pub const DEFAULT_HOSTED_MODEL: &str = "chat-v1";

/// The OpenRouter attribution headers, re-exported from the catalogue so this
/// module's long-standing spelling keeps resolving.
///
/// They used to be defined here, and a *second* pair was written out inline in
/// [`roster_build`](crate::harness::roster_build) with a different `HTTP-Referer`
/// — so a company's roster-build traffic and its turn traffic were attributed to
/// two different apps in OpenRouter's dashboard. Nothing compared them, so
/// nothing noticed. One constant now, in
/// [`catalogue`](crate::company::inference::catalogue), with both callers
/// reading it.
pub use crate::company::inference::catalogue::{OPENROUTER_REFERER, OPENROUTER_TITLE};

/// The key under which the managed billing/context metadata is stashed on
/// [`ModelResponse::raw`] so openhuman's crate-native cost pipeline recovers the
/// backend-charged USD. Must match openhuman's `OPENHUMAN_USAGE_META_KEY`.
const OPENHUMAN_USAGE_META_KEY: &str = "openhuman_usage_meta";

/// A harness inference model: a tinyinference [`ChatModel<()>`] plus the telemetry
/// slug OpenCompany attributes per-turn cost to.
///
/// `ChatModel` carries no provider identity, so this thin supertrait re-adds the
/// one bit the WS5 cost hook needs. It is read **live** after each turn (see
/// [`HarnessPool::run`](crate::harness::HarnessPool)) so a console BYOK switch
/// re-attributes spend on the next turn. `Arc<dyn HarnessModel>` upcasts to
/// `Arc<dyn ChatModel<()>>` where the loopback `model_bridge` serves it to
/// the embedded runtime.
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

    /// A sibling of this model that resolves every turn against agent
    /// `agent_id`'s own `{provider, model}` pair (keys rework, issue #2306,
    /// slice 3a), or `None` when this implementation cannot pin (test
    /// doubles). `agent_name` is the display name (round-3a review P1-1) a
    /// turn-time refusal names — never the id, per X7.
    ///
    /// A **per-agent** auxiliary pass built inside a specific agent's own
    /// [`build_agent_with_model`](crate::harness::build::build_agent_with_model)
    /// call (today: payload extraction) resolves per X12 through
    /// `crate::harness::built_in::pass_model`: the company default first,
    /// else this pin (round-2 review comment 4012457329). A **company-wide**
    /// pass with no single agent to pin against (title, triage, planning,
    /// selector — each built once per company, before any agent is chosen)
    /// is not wired to a pin at all; its own "the default is unreachable ⇒
    /// skip" contract already covers a pinned-only company the same way a
    /// missing default always has.
    fn pinned(
        &self,
        _agent_id: &str,
        _agent_name: &str,
        _choice: &inference::store::ModelChoice,
    ) -> Option<Arc<dyn HarnessModel>> {
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
/// * url — `OPENCOMPANY_INFERENCE_URL`, else the TinyHumans proxy derived from
///   `TINYHUMANS_API_URL`, else [`DEFAULT_TINYHUMANS_INFERENCE_URL`].
/// * model — `OPENCOMPANY_INFERENCE_MODEL`, else [`DEFAULT_HOSTED_MODEL`].
///
/// `OPENCOMPANY_INFERENCE_KEY` is checked first because it is a *different*
/// credential — a per-tenant inference key an operator supplied — not the
/// platform's TinyHumans identity. Within the platform identity itself the
/// documented tier order (projected file over static key) applies.
pub fn harness_inference_from_env(
    env: &dyn EnvSource,
) -> Option<(HostedProviderConfig, Option<String>)> {
    harness_inference_from_env_at(env, None)
}

/// As [`harness_inference_from_env`], but uses the host's already-resolved API
/// URL when an explicit inference URL is absent.  This matters for a URL set in
/// `config.toml`: it is configuration, not a process environment variable.
pub fn harness_inference_from_env_at(
    env: &dyn EnvSource,
    api_url: Option<&str>,
) -> Option<(HostedProviderConfig, Option<String>)> {
    let (credential, base_url) = hosted_endpoint_from_env_at(env, api_url)?;
    // The model is a per-roster **override** now: only an explicit
    // `OPENCOMPANY_INFERENCE_MODEL` flattens every agent to one workload. When
    // unset, each agent keeps its tier-derived model, which the tenant
    // `[inference].models` table then maps. `None` = no override.
    let model_override = env
        .get("OPENCOMPANY_INFERENCE_MODEL")
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    // A tier name here is not a model (keys rework, issue #2306, slice 2d) —
    // it only ever selects a configured map entry, and never flattens the
    // roster the way a real id does. Warned rather than refused: this is an
    // env var an operator set at deploy time, not a request this process can
    // decline.
    if let Some(m) = model_override.as_deref()
        && inference::legacy_tiers::is_tier_name(m)
    {
        tracing::warn!(
            model = %m,
            "OPENCOMPANY_INFERENCE_MODEL names a tier, which is never sent as a model; \
             set a model id from the provider's catalog"
        );
    }
    Some((
        HostedProviderConfig {
            base_url,
            credential,
            extra_headers: Vec::new(),
        },
        model_override,
    ))
}

/// Resolves the managed endpoint with an optional, already-normalized host API
/// URL.  An environment endpoint remains the explicit highest-precedence
/// override; the host URL is only the fallback proxy origin.
pub(crate) fn hosted_endpoint_from_env_at(
    env: &dyn EnvSource,
    api_url: Option<&str>,
) -> Option<(Credential, String)> {
    let credential = match env
        .get("OPENCOMPANY_INFERENCE_KEY")
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
    {
        Some(key) => Credential::from_value(key),
        None => Credential::from_source(Arc::new(TinyhumansTokenSource::from_env(env)?)),
    };
    Some((credential, platform_inference_url_at(env, api_url)))
}

/// The managed inference endpoint this deployment talks to, credential or no
/// credential: `OPENCOMPANY_INFERENCE_URL` when set, else the TinyHumans
/// OpenRouter proxy on `api_url` — the host's resolved `TINYHUMANS_API_URL` /
/// `config.toml` `api_url` — else the same proxy on the `TINYHUMANS_API_URL`
/// variable, else production.
///
/// Split from [`hosted_endpoint_from_env_at`] because the endpoint and the
/// instance credential are two different questions, and every resolver that
/// conflated them fell back to **production** the moment the credential was
/// absent: a desktop with no `TINYHUMANS_API_KEY` in its environment — the
/// ordinary case, since its identity is the company's own account key — got
/// `EnvDefault = None` at boot, and `resolve_endpoint`'s managed arm then
/// used the built-in constant. A staging key was presented to production,
/// and the LLM page said so while nothing on the host could be pointed
/// anywhere else. The endpoint follows `api_url` whether or not a credential
/// does; what decides whether a company can *think* stays the credential.
pub fn platform_inference_url_at(env: &dyn EnvSource, api_url: Option<&str>) -> String {
    env.get("OPENCOMPANY_INFERENCE_URL").unwrap_or_else(|| {
        let platform_url = api_url
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                crate::company::composio::backend_url_or_default(
                    env.get(crate::company::composio::TINYHUMANS_API_URL_ENV),
                )
            });
        crate::company::inference::catalogue::tinyhumans_proxy_url(&platform_url)
    })
}

/// The platform managed default as the runtime builder wants it: **always** an
/// endpoint ([`platform_inference_url_at`]), and the instance credential when
/// the environment holds one, else [`Credential::None`].
///
/// Where [`harness_inference_from_env_at`] answers "can this deployment think
/// on its own identity?", this answers "which platform is this deployment
/// on?" — a question with an answer even when the first is no. A
/// [`EnvDefault`](crate::company::inference::EnvDefault) built from it carries
/// a credential that reports `configured() == false`, which is exactly what
/// every managed-source gate already tests (`managed_source`, the legacy
/// chain's step 3), so an endpoint without a credential still routes nowhere.
pub fn platform_inference_default_at(
    env: &dyn EnvSource,
    api_url: Option<&str>,
) -> (HostedProviderConfig, Option<String>) {
    match harness_inference_from_env_at(env, api_url) {
        Some(resolved) => resolved,
        None => (
            HostedProviderConfig {
                base_url: platform_inference_url_at(env, api_url),
                credential: Credential::None,
                extra_headers: Vec::new(),
            },
            None,
        ),
    }
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
/// This function still consults only the environment. The request-time Search
/// backend may prepend the company's own `search/managed/key`; that credential
/// is billed to the same company and therefore does not create the ambient-
/// credential problem the environment-only boundary was written to prevent.
pub fn search_backend_from_env(env: &dyn EnvSource) -> Option<super::search::SearchBackend> {
    let credential = Credential::from_source(Arc::new(TinyhumansTokenSource::from_env(env)?));
    Some(super::search::SearchBackend::new(
        search_backend_url_from_env(env),
        credential,
        crate::company::DEFAULT_SEARCH_DAILY_CALLS,
    ))
}

/// A process-wide managed-search handle, even when the deployment credential
/// is absent. The empty credential keeps requests fail-closed, while the shared
/// handle lets company credentials use one ledger across workflow and harness
/// lanes.
pub fn search_backend_handle_from_env(env: &dyn EnvSource) -> super::search::SearchBackend {
    let credential = TinyhumansTokenSource::from_env(env)
        .map(|source| Credential::from_source(Arc::new(source)))
        .unwrap_or(Credential::None);
    super::search::SearchBackend::new(
        search_backend_url_from_env(env),
        credential,
        crate::company::DEFAULT_SEARCH_DAILY_CALLS,
    )
}

/// The managed-search endpoint, independent of whether the deployment has a
/// credential. Company-scoped credentials use the same proxy and need this
/// answer even on a host with no platform identity.
pub fn search_backend_url_from_env(env: &dyn EnvSource) -> String {
    env.get("OPENCOMPANY_SEARCH_BACKEND_URL")
        .unwrap_or_else(|| DEFAULT_TINYHUMANS_SEARCH_BACKEND_URL.to_string())
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
    /// Managed chat inference resolved
    /// ([`hosted_endpoint_from_env_at`]).
    pub inference: bool,
    /// Managed web search resolved ([`search_backend_from_env`]).
    pub search: bool,
    /// Managed media generation resolved ([`media_backend_from_env`]).
    pub media: bool,
}

impl PlatformCredentialStatus {
    /// Resolves every managed surface against one environment read.
    pub fn resolve(env: &dyn EnvSource) -> Self {
        Self::resolve_at(env, None)
    }

    /// Resolves managed surfaces using the host configuration for inference's
    /// fallback endpoint.
    pub fn resolve_at(env: &dyn EnvSource, api_url: Option<&str>) -> Self {
        let source = TinyhumansTokenSource::from_env(env);
        let projected_tier = source
            .as_ref()
            .is_some_and(|s| s.tier() == crate::company::credentials::TokenTier::ProjectedFile);
        Self {
            platform_identity: source.is_some(),
            projected_tier,
            inference: hosted_endpoint_from_env_at(env, api_url).is_some(),
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
        // A host-defined message kind (tinyinference at the 1ecf1b0 pin). Its
        // display text is what an OpenAI endpoint can carry; sent as a user
        // turn so no provider rejects an unknown role.
        Message::Custom(custom) => serde_json::json!({
            "role": "user",
            "content": custom.display.clone().unwrap_or_else(|| message.text()),
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
/// tinyinference [`Usage`], or `None` when the payload carries no `usage` block.
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
        charged_amount: None,
        context_window_tokens: None,
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

/// Parse the OpenAI `choices[0].message.tool_calls[]` array into tinyinference
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

/// The observable facts about a turn that came back with nothing, appended to
/// the error so the next occurrence is diagnosable rather than merely reported.
///
/// Issue #2016: an empty inference on staging was persistent — the harness had
/// already retried it, and the only thing recorded was that it happened. These
/// are what separate the causes:
///
/// * **usage.** Zero prompt *and* completion tokens beside a 200 is the
///   signature of the silent provider failure this file already documents
///   (`~438,000 input tokens -> HTTP 200, finish_reason "failed", response ""`).
///   Nonzero prompt tokens mean the request was read and only the answer was
///   missing, which is a different fault entirely.
/// * **finish_reason.** `length` is truncation, `content_filter` a policy stop,
///   `failed` the documented silent failure, absent a non-conforming provider.
/// * **the `choices` shape.** No `choices` key, an empty array, and a present
///   choice carrying an empty message are three different provider bugs that
///   all reach this line identically.
/// * **whether a refusal was present**, since a refusal that failed to extract
///   would otherwise look like an ordinary empty turn.
///
/// Values only — no provider text is interpolated, so this cannot become a
/// second route for a payload to reach a log or an operator.
fn empty_turn_facts(payload: &serde_json::Value, finish_reason: Option<&str>) -> String {
    let choices = match payload.get("choices") {
        None => "absent".to_string(),
        Some(serde_json::Value::Array(items)) if items.is_empty() => "empty".to_string(),
        Some(serde_json::Value::Array(items)) => items.len().to_string(),
        Some(other) => format!("not-an-array({})", json_kind(other)),
    };
    let usage = match parse_usage(payload) {
        Some(usage) => format!(
            "in={} out={} total={}",
            usage.input_tokens, usage.output_tokens, usage.total_tokens
        ),
        None => "absent".to_string(),
    };
    let refusal = payload
        .pointer("/choices/0/message/refusal")
        .is_some_and(|value| !value.is_null());
    format!(
        " (finish_reason: {}; choices: {choices}; usage: {usage}; refusal_present: {refusal})",
        finish_reason.unwrap_or("absent")
    )
}

/// The JSON type of a value, for a diagnostic that must not print its contents.
fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Refuse a whole batch that pairs `request_approval` with any sibling call.
///
/// `request_approval` is the boundary an effectful call is supposed to stop at,
/// so a batch that requests approval *and* asks for something else in the same
/// breath is refused outright rather than partly honoured — the policy fold only
/// refuses calls sequenced after the approval, which would let a sibling ordered
/// before it run before the pending flag is even set.
///
/// Extracted so both paths that can produce a batch get it: the parsed
/// `message.tool_calls` array, and a batch recovered from message text.
fn refuse_approval_siblings(tool_calls: &[ToolCall]) -> TaResult<()> {
    if tool_calls.len() > 1
        && tool_calls
            .iter()
            .any(|call| call.name == crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND)
    {
        return Err(InferenceError::Model(
            "inference returned request_approval with sibling tool calls; the whole batch was \
             refused so the approval boundary cannot be crossed"
                .to_string(),
        ));
    }
    Ok(())
}

/// Whether the raw payload asks for an action at all, independent of whether
/// any of it parsed.
///
/// Checked on the *raw* payload and independent of `finish_reason`, so a
/// genuinely-requested-but-unparseable call can never be promoted as ordinary
/// prose (Codex review on #1779, comment 3862781739).
///
/// Shared with [`probe`] rather than re-derived there: the probe's
/// reasoning-only tolerance must not swallow a malformed-tool-call failure, and
/// answering "did this payload request an action?" two different ways is how
/// the probe drifted from the turn path before (Codex review on #2068).
fn raw_tool_call_requested(payload: &serde_json::Value) -> bool {
    payload
        .pointer("/choices/0/message/tool_calls")
        .is_some_and(|v| match v {
            serde_json::Value::Null => false,
            serde_json::Value::Array(arr) => !arr.is_empty(),
            // A present-but-non-array value (e.g. an object) is not a shape
            // `parse_tool_calls` or the legacy `function_call` check can
            // recognize, but it is not an absence either — fail closed
            // rather than let it read as "nothing requested".
            _ => true,
        })
        || payload
            .pointer("/choices/0/message/function_call")
            .is_some_and(|v| !v.is_null())
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
///
/// Offers no tools, so a caller with no live turn behind it — the connectivity
/// probe, and every payload-shape test — gets exactly the wire parse and no
/// text recovery.
fn model_response_from_payload(payload: serde_json::Value) -> TaResult<ModelResponse> {
    model_response_from_payload_offering(
        payload,
        &std::collections::BTreeSet::new(),
        &std::collections::BTreeMap::new(),
    )
}

/// The tool names a request's system prompt advertises on the `opencompany`
/// MCP server (plan hive-desks Phase 3), added to the salvage's `offered` set:
/// a model that writes `web_search` as prose meant the served tool, and the
/// brief is the only place the wire names it — the `tools` array carries
/// `mcp_call_tool`, not the catalogue behind it. Read from the same messages
/// that go on the wire, so it can never name a tool this turn did not offer.
///
/// User messages as well as the system prompt: a roster rebuilt under a
/// resumed session re-announces the catalogue on the turn text
/// (`build::opencompany_mcp_rebrief`), and that brief is the current one
/// where the prompt's is pinned. A name a person typed under that heading
/// buys nothing — a salvaged call is served only if the MCP host's
/// allowlist names it.
fn mcp_served_tools(messages: &[Message]) -> std::collections::BTreeSet<String> {
    messages
        .iter()
        .filter(|message| matches!(message, Message::System(_) | Message::User(_)))
        .flat_map(|message| crate::harness::build::tools_named_in_mcp_brief(&message.text()))
        .collect()
}

/// [`model_response_from_payload`], plus the tool names **this turn offered the
/// model**.
///
/// Those names are what let a tool call the model wrote as prose be recovered
/// instead of shown to the operator as raw JSON: a candidate is dispatched only
/// if it names a tool this turn actually advertised. An empty set disables the
/// recovery entirely. See [`native_salvage`](crate::harness::native_salvage).
fn model_response_from_payload_offering(
    payload: serde_json::Value,
    offered: &std::collections::BTreeSet<String>,
    schemas: &std::collections::BTreeMap<String, serde_json::Value>,
) -> TaResult<ModelResponse> {
    // Content may be a plain string OR an array of `{type:"text",text:…}`
    // parts; tolerate both.
    let raw_content = payload.pointer("/choices/0/message/content");
    let mut content = extract_content_text(raw_content);
    // Whether a fallback below replaced the model's own visible message. Read
    // by the text-tool-call recovery, which must act on what the model *said*
    // and never on what a fallback substituted for it.
    //
    // Recorded as provenance rather than inferred by comparing the text against
    // a snapshot: a gateway that echoes the same refusal string into both
    // `content` and `message.refusal` leaves the substituted value equal to the
    // original, so an equality check reads "untouched" for the one case that
    // most needs to block (Codex review on #2011).
    let mut content_substituted = false;
    let tool_calls = parse_tool_calls(&payload);
    refuse_approval_siblings(&tool_calls)?;
    let finish_reason = payload
        .pointer("/choices/0/finish_reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);

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
    let raw_tool_call_requested = raw_tool_call_requested(&payload);
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
        return Err(InferenceError::Model(format!(
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
            // A refusal is a *completed* decision, not a partial one, so its
            // precedence must not depend on `finish_reason`: a refusal that
            // ends with e.g. `finish_reason: "content_filter"` (arguably the
            // *more* likely finish reason for an actual content-policy
            // refusal) must not fall through untouched, leaving whatever text
            // or reasoning leaked alongside it to win instead and silently
            // discard the refusal (Codex review on #1779, comment
            // 3875167298). It always wins over leaked text/reasoning,
            // independent of how the turn finished.
            content = refusal;
            content_substituted = true;
        } else if finish_reason.as_deref() == Some("failed") && !content.is_empty() {
            // `finish_reason: "failed"` is the documented HTTP-200-empty-
            // response silent provider failure (docs/spec/runtime/providers.md).
            // It is a *completed* disclaimer that the turn did not succeed —
            // like a refusal, not a partial/unfinished state — so it must not
            // be overridden by whatever text leaked alongside it. `content`
            // itself is extracted unconditionally at the top of this function
            // (string OR array-shaped), so a provider that emits real text —
            // a leaked lead-in sentence, or a fuller partial reply — before
            // reporting `failed` had that text returned as a successful
            // answer with no finish_reason check at all. Discard it here so
            // the response falls through to the empty-turn error below,
            // naming `failed` for diagnosis (CodeRabbit review on #1779,
            // comment 3878355364).
            content.clear();
            content_substituted = true;
        }
        // Deliberately no `reasoning`/`reasoning_content` fallback here.
        //
        // A `content: null` turn with only a populated `reasoning` field is
        // not a misplaced answer — it is the model finishing (`stop`) having
        // spent its entire turn on hidden deliberation and writing nothing to
        // `content` at all. A prior version of this function copied
        // `reasoning` into `content` in that case, on the premise that
        // reasoning-only output was the model's real answer landing in the
        // wrong field. That premise was false: `reasoning` is chain-of-thought,
        // not a response, and promoting it put raw first-person deliberation —
        // often truncated mid-sentence — directly in front of operators,
        // labelled and persisted as the agent's genuine reply. Because a
        // promoted turn is persisted and replayed as real history
        // (`wire_message`), a later turn had no actual answer to build on and
        // fabricated one instead of surfacing the gap.
        //
        // Falling through to the empty-turn error below is not a regression:
        // that error is retryable (`classify_provider_failure` finds no
        // status/keyword match in `empty_turn_facts`'s text, so it defaults to
        // `ProviderFailureClass::Retryable`), so the harness retries the same
        // request automatically rather than requiring operator action. Given
        // whether a pass reasons at all is adaptive per-call, a retry has a
        // real chance at a genuine `content`-bearing response.
    }

    // The model wrote a tool call into its message body instead of emitting it
    // through the channel it was handed. Recover it here or not at all: the
    // agent loop takes a turn's calls from `ModelResponse::tool_calls` and never
    // parses model text, so this is the last point at which a text-shaped call
    // can still become a real one. See [`native_salvage`].
    //
    // Gated on the same condition as the fallbacks above — nothing parsed AND
    // nothing raw requested — so this can neither compete with the native
    // channel nor paper over a call the model really did make and whose body
    // failed to parse. That is a diagnosable error, not something to guess at.
    //
    // `!content_substituted` is the load-bearing one: it requires that `content`
    // is still the model's own visible message and not something a fallback
    // above put there. Each substitution must block a recovery for its own
    // reason (Codex review on #2011):
    //
    //   * a **refusal** is a completed decision *not* to act, so recovering an
    //     action out of one would invert it;
    //   * `finish_reason: "failed"` cleared `content` precisely because the turn
    //     did not succeed.
    //
    // `finished_or_unstated` covers the other half of the same idea. A stop the
    // model did not choose — `length`, `content_filter`, `failed` — leaves a
    // fragment, and a balanced object inside a fragment is not a completed
    // request.
    //
    // An **allow-list**, matching the principle
    // `unrecognized_finish_reason_reasoning_only_turn_errors` pins: a blocklist
    // only refuses the failures someone thought of, so a provider answering
    // `finish_reason: "error"` — a value this file's own tests already treat as
    // non-success — would have had a call recovered out of a failed turn
    // (CodeRabbit review on #2011).
    //
    // It admits one thing a plain `Some("stop")` check does not: an **absent**
    // finish_reason. Here the model's own visible
    // message is the evidence, and refusing without a finish_reason would
    // disable the recovery for exactly the non-conforming providers it exists
    // for. Silence is not a failure signal — every named failure still is.
    let finished_or_unstated = matches!(finish_reason.as_deref(), None | Some("stop"));
    let mut tool_calls = tool_calls;
    if tool_calls.is_empty()
        && !raw_tool_call_requested
        && !content_substituted
        && finished_or_unstated
        && !offered.is_empty()
        && let Some((cleaned, recovered)) =
            crate::harness::native_salvage::recover_text_tool_calls(&content, offered, schemas)
    {
        // The same fail-closed batch check the parsed path gets. Applied to the
        // recovered batch too, or a text response pairing `request_approval`
        // with an effectful sibling would cross the approval boundary here that
        // it cannot cross there (Codex review on #2011).
        refuse_approval_siblings(&recovered)?;
        content = cleaned;
        // A recovered call to a tool on the wire keeps its own name; one the
        // turn offers only through the `opencompany` MCP server (named in
        // `offered` by `mcp_served_tools`) is dispatched as `mcp_call_tool`.
        tool_calls = recovered
            .into_iter()
            .map(|call| {
                let (name, arguments) = if schemas.contains_key(&call.name) {
                    (call.name, call.arguments)
                } else {
                    crate::hive::tools::via_opencompany_mcp(&call.name, call.arguments)
                };
                ToolCall {
                    id: call.id,
                    name,
                    arguments,
                    invalid: None,
                }
            })
            .collect();
    }

    // Only a genuinely empty turn (no text anywhere, no tool call) is an error.
    // Fold what the payload actually said into the message so a truncation
    // (`length`), a `content_filter` stop, and a provider that answered 200
    // with nothing in it are each diagnosable rather than hidden behind one
    // generic "carried neither" string. See [`empty_turn_facts`].
    if content.is_empty() && tool_calls.is_empty() {
        return Err(InferenceError::Model(format!(
            "inference response carried neither choices[0].message.content nor tool_calls{}",
            empty_turn_facts(&payload, finish_reason.as_deref())
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
        origin: None,
    };
    let mut response = ModelResponse {
        message,
        usage,
        finish_reason,
        raw: None,
        resolved_model: None,
        continue_turn: None,
        served_from_cache: false,
        correlation: None,
        resolved_route: None,
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

/// Whether a payload the parser rejected as empty still proves the endpoint
/// **answered** — it deliberated and simply had no budget left to write a
/// visible reply.
///
/// Reachability and usability are different questions, and only [`probe`] asks
/// the first. A real turn that comes back with nothing but chain-of-thought is
/// a failed turn: there is no answer to show, and promoting the reasoning is
/// what this PR removed. A *probe* asking `ping` with `max_tokens: 16` is not
/// judging the answer at all — it is asking whether the credential, the base
/// URL and the model name reach a server that completes chat turns. A response
/// carrying reasoning tokens answers that with a yes.
///
/// Without this, the probe re-created the bug #1779 fixed: an endpoint that
/// every real turn reached fine was reported as a broken connection, and the
/// setup wizard would not proceed past it. 16 tokens is little enough room that
/// a well-behaved reasoning model burns it on deliberation routinely — for the
/// DeepSeek-style models this matters for, it is the *expected* shape, not an
/// edge case.
///
/// Deliberately a **predicate on the payload**, not a second content path. The
/// probe calls the same [`model_response_from_payload`] the turn path does and
/// only consults this when that parser has already refused; giving the probe
/// its own narrower parser is exactly how it drifted from the turn path before
/// (Codex review on #1779). Nothing here is ever surfaced — the reasoning text
/// is not read, only its presence.
fn probe_reachable_despite_empty_turn(payload: &serde_json::Value) -> bool {
    // Only the *empty-turn* refusal may be tolerated. A payload that asked for
    // an action and got it wrong — a malformed entry, a missing name, a
    // `finish_reason` declaring a call that never arrived — fails the parser
    // for a different reason, and returning `Ok(())` on that would sail past
    // the unoffered-tool-call guard below and pass an endpoint that cannot
    // complete a bare `ping` (Codex review on #2068). This probe offers no
    // tools, so a payload declaring an action is already wrong regardless of
    // what else it carries.
    if raw_tool_call_requested(payload)
        || matches!(
            payload
                .pointer("/choices/0/finish_reason")
                .and_then(serde_json::Value::as_str),
            Some("tool_calls") | Some("function_call")
        )
    {
        return false;
    }
    !extract_content_text(payload.pointer("/choices/0/message/reasoning")).is_empty()
        || !extract_content_text(payload.pointer("/choices/0/message/reasoning_content")).is_empty()
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

/// The per-request output cap actually sent, given the cap the harness asked
/// for.
///
/// The vendored harness stamps every request with a fixed
/// `AGENT_TURN_MAX_OUTPUT_TOKENS` (16384) sized for a model whose visible
/// answer is all it emits. A reasoning model routed through an OpenAI-shaped
/// endpoint counts its hidden reasoning stream against the same `max_tokens`,
/// so on a hard problem the model exhausts the cap before writing a single
/// visible token and the turn fails with `finish_reason: length` and an empty
/// message. `OPENCOMPANY_INFERENCE_MAX_TOKENS` raises the floor for such a
/// deployment: the larger of the harness's cap and the variable is sent, so the
/// variable can never *lower* a cap the harness relied on, and an unset or
/// unparsable value changes nothing.
fn output_cap(requested: Option<u32>) -> Option<u32> {
    let floor = std::env::var("OPENCOMPANY_INFERENCE_MAX_TOKENS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|value| *value > 0);
    match (requested, floor) {
        (Some(cap), Some(floor)) => Some(cap.max(floor)),
        (Some(cap), None) => Some(cap),
        (None, floor) => floor,
    }
}

/// Writes the caller's **intent** onto the body in the dialect `model` speaks,
/// and reports which field names went out.
///
/// The names are returned rather than recomputed because the retry needs to know
/// exactly what was sent: `inference::dialect::parameter_blamed_by` only accepts
/// a rejection that names a parameter **we actually sent**, and after renaming
/// (`max_tokens` → `max_completion_tokens`) the sent name is not the name the
/// caller asked with.
///
/// Every vendor-specific decision lives in `inference::dialect::RULES`. Nothing
/// here knows what a temperature is, which is the property that stops this
/// function growing a vendor name the next time a model behaves differently.
fn apply_sampling(
    body: &mut serde_json::Value,
    endpoint: &str,
    model: &str,
    sampling: inference::dialect::Sampling,
    max_tokens: Option<u32>,
) -> Vec<String> {
    let mut knobs = sampling.knobs();
    if let Some(cap) = output_cap(max_tokens) {
        knobs.push(inference::dialect::Knob {
            name: "max_tokens",
            value: serde_json::json!(cap),
        });
    }
    let mut sent = Vec::new();
    for (name, value) in inference::dialect::translate(endpoint, model, knobs) {
        body[&name] = value;
        sent.push(name);
    }
    sent
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
    /// [`Agent::turn`]: openhuman_core::agent::Agent
    async fn invoke(&self, _state: &(), request: ModelRequest) -> TaResult<ModelResponse> {
        let messages = wire_messages(&request.messages);
        let model = request
            .model
            .as_deref()
            .unwrap_or(DEFAULT_HOSTED_MODEL)
            .trim();
        // No decl on this path, so no configured per-tier map to fall back to
        // (keys rework, issue #2306, slice 2d): only a real requested id is
        // ever sent.
        if model.is_empty() || inference::legacy_tiers::is_tier_name(model) {
            return Err(InferenceError::Model(
                inference::NO_MODEL_CHOSEN.to_string(),
            ));
        }

        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
        });
        // Intent in, this model's dialect out — see `apply_sampling`.
        let _sent = apply_sampling(
            &mut body,
            &self.config.base_url,
            model,
            inference::dialect::Sampling::from_request(request.temperature),
            request.max_tokens,
        );
        // Native tool calling: expose the turn's tools so the model emits
        // structured `tool_calls` instead of hand-written `<tool_call>` XML.
        attach_tools(
            &mut body,
            wire_tools(&request.tools),
            &request.tool_choice,
            self.product_identity,
        );
        // Captured from the same list and the same choice that go on the wire,
        // so what the response is allowed to name can never drift from what the
        // request authorized.
        let mut offered = crate::harness::native_salvage::authorized_tool_names(
            &request.tools,
            &request.tool_choice,
        );
        if offered.contains("mcp_call_tool") {
            offered.extend(mcp_served_tools(&request.messages));
        }
        let schemas = crate::harness::native_salvage::authorized_tool_schemas(
            &request.tools,
            &request.tool_choice,
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
            InferenceError::Model(format!("resolving the TinyHumans credential: {e}"))
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
            .map_err(|e| InferenceError::Model(format!("hosted inference request failed: {e}")))?;
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
                return Err(InferenceError::Model(advice));
            }
            return Err(InferenceError::Model(error));
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
            InferenceError::Model(format!("hosted inference response was not JSON: {e}"))
        })?;
        model_response_from_payload_offering(payload, &offered, &schemas)
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
    /// The sampling/limit field names this body actually carries, **after**
    /// per-model translation.
    ///
    /// Carried rather than recomputed because a rule may rename a field, so the
    /// name the caller asked with is not always the name on the wire. The retry
    /// will only drop a parameter the model names *and* that appears here, which
    /// is what stops a rejection mentioning some field we never sent from
    /// talking us into removing one.
    pub tunable_fields: Vec<String>,
}

/// Builds the [`RequestPlan`] for one turn against a tenant provider.
///
/// * A decl with a chosen model (keys rework #2306, slice 2b) sends it;
///   otherwise the abstract tier (`chat-v1`, …) is mapped through the tenant
///   `[inference].models` table, and an unmapped tier passes through verbatim.
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
    requested_model: &str,
    messages: Vec<serde_json::Value>,
    sampling: inference::dialect::Sampling,
    max_tokens: Option<u32>,
    tools: Vec<serde_json::Value>,
    tool_choice: &ToolChoice,
) -> anyhow::Result<RequestPlan> {
    // Never a tier name (keys rework, issue #2306, slice 2d): the chosen
    // model, else a real requested id, else the operator's configured id for
    // the requested tier, else refuse before sending.
    let model =
        inference::model_on_the_wire(decl, requested_model).map_err(|e| anyhow::anyhow!("{e}"))?;
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
        "messages": messages,
    });
    // Intent in, this model's dialect out. `sent` is the field names that
    // actually went, which the retry needs — a rename means the caller's name
    // and the wire name differ. See `apply_sampling`.
    let sent = apply_sampling(&mut body, &url, &model, sampling, max_tokens);
    let supports_parallel_control =
        decl.is_proxied() || inference::normalize_provider(&decl.provider) == "openrouter";
    attach_tools(&mut body, tools, tool_choice, supports_parallel_control);
    Ok(RequestPlan {
        url,
        model,
        bearer,
        headers,
        body,
        tunable_fields: sent,
    })
}

/// Substrings that mark a provider 4xx as "you asked for a model that isn't
/// there", matched case-insensitively. The wording is the provider's and
/// varies: the managed backend says `Model '<id>' is not available`, an
/// OpenAI-compatible BYOK endpoint says `The model '<id>' does not exist`, and
/// OpenRouter says `<id> is not a valid model ID`.
///
/// **Anthropic is the one that matters most and matched none of them.** It
/// answers `404 {"type":"not_found_error","message":"model: agentic-v1"}` — a
/// typed code rather than a sentence — so the whole repair path returned `None`
/// for the provider an operator is most likely to connect first, and the
/// operator got the raw 404 with no pointer to Settings → Inference and no
/// `GET {base}/models` suggestion. Matching the type name rather than the
/// message is what makes it reachable; the message is only ever `model: <id>`,
/// which no generic substring could safely claim.
const MODEL_UNAVAILABLE_SIGNATURES: &[&str] = &[
    "is not available",
    "not a valid model",
    "model not found",
    "unknown model",
    "invalid model",
    "does not exist",
    "not_found_error",
];

const MODEL_UNAVAILABLE_FAILURE_PREFIX: &str =
    "the configured inference model is not available from the provider";

pub(crate) fn is_model_unavailable_failure(error: &ProviderError) -> bool {
    error.message.starts_with(MODEL_UNAVAILABLE_FAILURE_PREFIX)
}

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
    // Redacted here rather than at the two call sites, so a third one cannot
    // reintroduce the leak. An endpoint may carry userinfo, and this sentence is
    // operator-facing: it reaches the console and gets screenshotted into
    // tickets.
    let models_url = crate::company::inference::catalogue::redact_endpoint(models_url);
    Some(format!(
        "{MODEL_UNAVAILABLE_FAILURE_PREFIX} — {where_to_fix}, to \
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
    // **Bearer here, on purpose, for every provider including Anthropic — do not
    // "fix" this to match the catalogue's `auth_style`.**
    //
    // This is the OpenAI-shaped chat path (`POST {base}/chat/completions`), and
    // for `api.anthropic.com/v1` that reaches Anthropic's **OpenAI SDK
    // compatibility layer**, which authenticates with `Authorization: Bearer`
    // and takes no `anthropic-version`. Their *native* API is `POST
    // /v1/messages` with an entirely different body, and it is the native
    // endpoints — `GET /v1/models` among them — that want `x-api-key`.
    //
    // So `AuthStyle::Anthropic` means "this provider's NATIVE endpoints use
    // x-api-key", and the only native call this product makes is the catalog
    // listing (`inference_models::discover_models`). Applying it here would
    // break a path that currently works.
    //
    // Verified at `platform.claude.com/docs/en/cli-sdks-libraries/libraries/openai-sdk`.
    // Note their own caveat: the compatibility layer is "primarily intended to
    // test and compare model capabilities, and is not considered a long-term or
    // production-ready solution for most use cases" — it ignores `strict` and
    // `response_format`, supports no prompt caching, and hoists system messages.
    match send_body(client, plan, &plan.body, credential, harness, source).await {
        Ok(payload) => Ok(payload),
        Err(SendFailure::Rejected {
            parameter,
            error: _,
        }) => {
            // The model told us, by name, that a parameter we sent is one it
            // does not take. Drop that one parameter, remember it, and try once.
            //
            // **Bounded to a single retry, and only for this failure.** A 400 is
            // billed nothing, so the cost of being wrong about a model is one
            // wasted round-trip; the cost of not having this is a feature that
            // stays broken until someone ships a table row. It is what makes
            // `dialect::RULES` an optimisation rather than a dependency — a
            // vendor that changes silently corrects us without a release.
            inference::dialect::remember_omit(&plan.url, &plan.model, &parameter);
            let mut body = plan.body.clone();
            if let Some(object) = body.as_object_mut() {
                object.remove(&parameter);
            }
            send_body(client, plan, &body, credential, harness, source)
                .await
                .map_err(SendFailure::into_error)
        }
        Err(other) => Err(other.into_error()),
    }
}

/// Why a single attempt failed, keeping the one case the caller can act on
/// separate from the ones it cannot.
enum SendFailure {
    /// The model named a parameter we sent as one it does not accept. The only
    /// case worth a second attempt, because it is the only one where we know
    /// what to change.
    Rejected {
        parameter: String,
        error: anyhow::Error,
    },
    /// Everything else, already phrased for the operator.
    Other(anyhow::Error),
}

fn into_inference_error(error: anyhow::Error) -> InferenceError {
    match error.downcast::<InferenceError>() {
        Ok(error) => error,
        Err(error) => InferenceError::Model(error.to_string()),
    }
}

impl SendFailure {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Rejected { error, .. } | Self::Other(error) => error,
        }
    }
}

/// One attempt, with an explicit body so the retry can send a narrowed one.
async fn send_body(
    client: &reqwest::Client,
    plan: &RequestPlan,
    body: &serde_json::Value,
    credential: &Credential,
    harness: Option<&str>,
    source: Option<InferenceSource>,
) -> Result<serde_json::Value, SendFailure> {
    let mut request = client.post(&plan.url).json(body);
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
    let response = request.send().await.map_err(|e| {
        SendFailure::Other(anyhow::anyhow!(
            "inference request failed: {}",
            scrub(e.to_string())
        ))
    })?;
    let status = response.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            credential.invalidate();
        }
        let text = response.text().await.unwrap_or_default();
        let scrubbed = scrub(text.clone());
        let error = format!("inference returned {status}: {scrubbed}");
        let raw = serde_json::from_str::<serde_json::Value>(&scrubbed).ok();
        let error_object = raw.as_ref().and_then(|value| value.get("error"));
        let message = error_object
            .and_then(|value| value.get("message"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                raw.as_ref()
                    .and_then(|value| value.get("message"))
                    .and_then(serde_json::Value::as_str)
            })
            .filter(|message| !message.trim().is_empty())
            .unwrap_or(&scrubbed)
            .to_string();
        let code = error_object
            .and_then(|value| value.get("code").or_else(|| value.get("type")))
            .and_then(|value| match value {
                serde_json::Value::String(code) => Some(code.clone()),
                serde_json::Value::Number(code) => Some(code.to_string()),
                _ => None,
            });
        let provider_error = |message: String| {
            anyhow::Error::new(InferenceError::Provider(Box::new(ProviderError {
                provider: "inference".to_string(),
                model: Some(plan.model.clone()),
                status: Some(status.as_u16()),
                code: code.clone(),
                message,
                retryable: status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status.is_server_error(),
                retry_after_ms: None,
                partial_message: None,
                stop_reason: None,
                raw: None,
            })))
        };
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
            return Err(SendFailure::Other(provider_error(advice)));
        }
        // Only a 400 is a statement about the request's shape. A 5xx, a 429 or a
        // 401 is about the service or the credential, and narrowing the body in
        // response to one would drop a parameter over a problem it did not cause.
        if status == reqwest::StatusCode::BAD_REQUEST
            && let Some(parameter) =
                inference::dialect::parameter_blamed_by(&text, &plan.tunable_fields)
        {
            return Err(SendFailure::Rejected {
                parameter,
                error: provider_error(message),
            });
        }
        return Err(SendFailure::Other(provider_error(message)));
    }
    response.json().await.map_err(|e| {
        SendFailure::Other(anyhow::anyhow!(
            "inference response was not JSON: {}",
            scrub(e.to_string())
        ))
    })
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
    /// `Some` only on a sibling built by [`pinned`](HarnessModel::pinned) —
    /// every provider built by [`new`](Self::new) is unpinned, resolving
    /// against the harness's own config/default exactly as before slice 3a.
    pin: Option<AgentPin>,
}

/// One agent's `{provider, model}` pair, carried with the agent's id and
/// display name so a turn-time refusal can name it (keys rework, issue
/// #2306, slice 3a; the name added in round-3a review P1-1) —
/// `resolve_for_turn` itself sees only the [`inference::store::ModelChoice`]
/// and has no agent to name, so both travel beside it instead. `agent_name`
/// is `Agent.name`, else `Agent.role` (never the raw id): X7 requires every
/// user-facing sentence to name a display name, and `copy::pair_broken`
/// takes one, not an id.
#[derive(Clone, Debug)]
pub(crate) struct AgentPin {
    // Not yet read anywhere: round-3a review P1-1 (the pin-failure sentences
    // wired to `copy::pair_broken`, which is where the structured id this
    // travels with `agent_name` for gets consumed) is still open. Kept rather
    // than removed so that fix does not have to re-add it; `#[allow]` rather
    // than a leading underscore so it stays the field name that fix expects.
    #[allow(dead_code)]
    pub agent_id: String,
    pub agent_name: String,
    pub choice: inference::store::ModelChoice,
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
            pin: None,
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
    /// cached telemetry slug. Errors with [`inference::NO_MODEL_CHOSEN`] (or a
    /// fail-closed sentence for a broken pin/full default) when nothing
    /// resolves.
    ///
    /// `tier` is the abstract tier **this** turn carries. Keys rework (#2306,
    /// slice 2b): resolution no longer depends on it for a company with an
    /// agent pair or a full default — [`inference::resolve_for_turn`] decides
    /// the model directly — but the legacy chain (no pin, no full default)
    /// still routes per workload exactly as it always did, so `tier` still
    /// has to reach it.
    async fn resolve(&self, tier: &str) -> anyhow::Result<InferenceDecl> {
        // Checked **before** `resolve_for_turn`, and only here: that function
        // sees just the `ModelChoice`, so a refusal it raises for a gone pin
        // cannot name the agent. This pre-check can, because `AgentPin`
        // carries the id and display name alongside the choice (phase-3.md
        // use case 4). If the row vanishes between this read and
        // `resolve_for_turn`'s own, that function's generic "this agent is
        // set to …" still fires — fail closed either way, just with a less
        // specific sentence on the race.
        //
        // Round-3a review P1-1: both branches go through `copy::pair_broken`
        // now, not a hand-written sentence — the shared X9 table, in display
        // names, is what every other turn-time refusal in this module uses,
        // and this was the one holdout still naming a raw agent id and
        // provider slug in the sentence itself. `Removed` has no row to read
        // a label from, so it names the slug (`copy::pair_broken`'s own
        // "label, else nothing better on hand" contract); `TurnedOff` uses
        // the row's real label.
        //
        // KR-L2-03: `copy::with_agent_marker` attaches `agent_id` as a
        // hidden trailer `copy::classify` strips before display — this is
        // the one place in the whole resolution path that is mid-turn AND
        // still holds the agent's raw id, so it is the one place that can
        // close the `pairAgentId` gap `copy::classify`'s own doc names.
        if let Some(AgentPin {
            agent_id,
            agent_name,
            choice,
        }) = &self.pin
        {
            match inference::store::get_provider(
                &self.company,
                self.secrets.as_ref(),
                &choice.provider,
            )
            .await
            .map_err(|e| anyhow::anyhow!("resolving inference config: {e}"))?
            {
                None => anyhow::bail!(inference::copy::with_agent_marker(
                    inference::copy::pair_broken(
                        agent_name,
                        &choice.provider,
                        inference::copy::ProviderGone::Removed,
                    ),
                    agent_id,
                )),
                Some(row) if !row.enabled => anyhow::bail!(inference::copy::with_agent_marker(
                    inference::copy::pair_broken(
                        agent_name,
                        &row.label,
                        inference::copy::ProviderGone::TurnedOff,
                    ),
                    agent_id,
                )),
                Some(row) => {
                    // Round-3a review P1-1: a pin naming an enabled row that
                    // still has no credential used to go straight to the
                    // network with no bearer and come back as a generic
                    // rejection. Only for a kind that actually needs one —
                    // the local-runtime kinds `auth_style_for` classifies
                    // `None` (ollama, omlx) are keyless by design and must
                    // reach `resolve_for_turn` exactly as before.
                    if inference::catalogue::auth_style_for(&row.kind)
                        != inference::catalogue::AuthStyle::None
                        && !inference::store::provider_key_configured(
                            &self.company,
                            self.secrets.as_ref(),
                            &row,
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("resolving inference config: {e}"))?
                    {
                        anyhow::bail!(inference::copy::with_agent_marker(
                            inference::copy::provider_has_no_key(agent_name, &row.label),
                            agent_id,
                        ));
                    }
                }
            }
        }
        let decl = inference::resolve_for_turn(
            &self.company,
            &self.manifest,
            self.env_default.as_ref(),
            self.secrets.as_ref(),
            &self.scope,
            self.pin.as_ref().map(|p| p.choice.clone()),
            tier,
        )
        .await
        .map_err(|e| anyhow::anyhow!("resolving inference config: {e}"))?;
        *self.slug.write().unwrap() = decl.telemetry_slug();
        // No vocabulary discovery any more (keys rework, issue #2306, slice
        // 2d): the model this turn sends is decided once, in
        // `inference::model_on_the_wire` — a chosen model, else a real
        // requested id, else the legacy arm's own configured map entry for
        // the tier, else refuse. None of those needs this endpoint's catalog
        // read first.
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
    /// [`Agent::turn`]: openhuman_core::agent::Agent
    async fn invoke(&self, _state: &(), request: ModelRequest) -> TaResult<ModelResponse> {
        // The tier first, because resolution now depends on it: the routing
        // table decides per workload, so the decl cannot be resolved before the
        // workload is known.
        let model = request.model.as_deref().unwrap_or(DEFAULT_HOSTED_MODEL);
        let decl = self
            .resolve(model)
            .await
            .map_err(|e| InferenceError::Model(e.to_string()))?;
        let messages = wire_messages(&request.messages);
        let plan = request_plan(
            &decl,
            model,
            messages,
            // Intent recovered at the vendored boundary, which carries a float.
            inference::dialect::Sampling::from_request(request.temperature),
            request.max_tokens,
            wire_tools(&request.tools),
            &request.tool_choice,
        )
        .await
        .map_err(|e| InferenceError::Model(e.to_string()))?;
        // Captured from the same list and choice the plan puts on the wire —
        // see the matching lines in `HostedProvider::invoke`.
        let mut offered = crate::harness::native_salvage::authorized_tool_names(
            &request.tools,
            &request.tool_choice,
        );
        if offered.contains("mcp_call_tool") {
            offered.extend(mcp_served_tools(&request.messages));
        }
        let schemas = crate::harness::native_salvage::authorized_tool_schemas(
            &request.tools,
            &request.tool_choice,
        );
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
        .map_err(into_inference_error)?;
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
        model_response_from_payload_offering(payload, &offered, &schemas)
    }
}

impl HarnessModel for TenantProvider {
    fn telemetry_provider_id(&self) -> String {
        (*self.slug.read().unwrap()).to_string()
    }

    fn telemetry_model(&self) -> Option<crate::metering::ModelSlug> {
        *self.model.read().unwrap()
    }

    /// Builds a sibling that resolves every turn against `agent_id`'s own
    /// pair instead of this harness's own config/default (keys rework, issue
    /// #2306, slice 3a) — a fresh, independent `TenantProvider`, not a
    /// wrapper, so its own `slug`/`model` telemetry cells track that agent's
    /// turns rather than sharing this provider's (G8: a shared cell would
    /// attribute a pinned agent's usage to whichever of it and the default
    /// finished a turn last).
    fn pinned(
        &self,
        agent_id: &str,
        agent_name: &str,
        choice: &inference::store::ModelChoice,
    ) -> Option<Arc<dyn HarnessModel>> {
        Some(Arc::new(TenantProvider {
            company: self.company.clone(),
            secrets: self.secrets.clone(),
            manifest: self.manifest.clone(),
            env_default: self.env_default.clone(),
            client: self.client.clone(),
            slug: RwLock::new("subscription"),
            model: RwLock::new(None),
            scope: self.scope.clone(),
            pin: Some(AgentPin {
                agent_id: agent_id.to_string(),
                agent_name: agent_name.to_string(),
                choice: choice.clone(),
            }),
        }))
    }
}

/// A minimal live probe: one `ping` turn against the resolved config, used by
/// the console's "Test" button. The error is scrubbed of the credential by
/// [`send_plan`].
///
/// `model` is the concrete provider id the caller resolved. A tier name is
/// refused before a request can put it on the wire.
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
pub async fn probe(decl: &InferenceDecl, model: &str, harness: Option<&str>) -> anyhow::Result<()> {
    let model = model.trim();
    if model.is_empty() || inference::legacy_tiers::is_tier_name(model) {
        return Err(anyhow::Error::new(InferenceError::Model(
            "Could not test this connection because no concrete model was available.".to_string(),
        )));
    }
    let client = reqwest::Client::new();
    let messages = vec![serde_json::json!({ "role": "user", "content": "ping" })];
    // The connectivity probe exposes no tools — it only checks the endpoint
    // answers a bare chat turn.
    let plan = request_plan(
        decl,
        model,
        messages,
        // A reachability check has no opinion about sampling. The hardcoded
        // `0.0` here made the probe fail on exactly the providers it exists to
        // reassure the operator about.
        inference::dialect::Sampling::Default,
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
    //
    // Checked BEFORE the parser takes ownership: a reasoning-only reply fails
    // the parser's empty-turn check, and for a reachability probe that failure
    // is still a yes. See [`probe_reachable_despite_empty_turn`].
    let reachable_despite_empty = probe_reachable_despite_empty_turn(&payload);
    let response = match model_response_from_payload(payload) {
        Ok(response) => response,
        Err(_) if reachable_despite_empty => return Ok(()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "probe response carried no usable content: {e}"
            ));
        }
    };
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
#[path = "provider/provider_tenant_tests.rs"]
mod provider_tenant_tests;
#[cfg(test)]
#[path = "provider/provider_test_helpers_tests.rs"]
mod provider_test_helpers_tests;
#[cfg(test)]
#[path = "provider/provider_agent_pin_tests_1.rs"]
mod tests_agent_pin_1;
#[cfg(test)]
#[path = "provider/provider_agent_pin_tests_2.rs"]
mod tests_agent_pin_2;
#[cfg(test)]
#[path = "provider/provider_credential_tests_1.rs"]
mod tests_credential_1;
#[cfg(test)]
#[path = "provider/provider_credential_tests_2.rs"]
mod tests_credential_2;
#[cfg(test)]
#[path = "provider/provider_credential_tests_3.rs"]
mod tests_credential_3;
#[cfg(test)]
#[path = "provider/provider_early_tests.rs"]
mod tests_early;
