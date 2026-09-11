//! The provider catalogue: data, and nothing else.
//!
//! This is the one source for *which providers exist, where they live, and how
//! they authenticate*. It holds no behaviour beyond lookup by slug, because the
//! moment a table like this grows an opinion it stops being copyable and starts
//! being reimplemented — which is how the six divergent copies this module
//! replaces came to exist.
//!
//! ## Why a table at all
//!
//! Before this module, provider knowledge was written down in six places with no
//! shared source, and two had already drifted: the OpenRouter attribution
//! headers existed in both `harness::built_in::provider` and
//! `harness::roster_build` with *different values*, and the console's wire union
//! still carried a provider-source the host had dropped. Nothing noticed either.
//! A table plus a test that fails when the mirror disagrees is the minimum that
//! notices.
//!
//! ## Where the contents came from
//!
//! Ported verbatim from openhuman at `5e543a76b`
//! (`vendor/openhuman/vendor/tinymemory/crates/tinymemory-api/src/host/cloud_providers.rs`
//! and `vendor/openhuman/app/src/components/settings/panels/builtinCloudProviders.ts`),
//! with the differences listed in `docs/modules/inference/catalogue.md`. Verbatim
//! is deliberate: the endpoints, the auth styles and the copy are the result of
//! several rounds of real bugs, and reinterpreting them re-earns those bugs.
//!
//! Two entries carry a fix in their comment rather than only in their value —
//! [`CLOUD_PROVIDERS`]'s `minimax` and `anthropic`. Read them before editing
//! either.
//!
//! ## What is deliberately *not* ported
//!
//! openhuman's TypeScript mirror carries a `tone` field: a literal Tailwind
//! class string per provider, including raw hex. This repo's
//! `scripts/ci/assert-design-tokens.sh` rejects raw hex on purpose, so a brand
//! tone here is a design-token decision on the console side rather than a column
//! of this table. The table stays presentation-free.
//!
//! And the 27th Rust entry, `openhuman` — their managed first-party backend — is
//! not a row here. Our equivalent is the managed TinyHumans brain, which is
//! already modelled with its own auth path ([`super::PLATFORM_BASE_URL`]) and
//! must keep it. Porting `openhuman` as a row would give the managed brain a
//! second, bearer-shaped identity.

/// How a provider expects its credential presented.
///
/// `Anthropic` exists for exactly one entry in the whole catalogue, and that is
/// the point: a port that assumes one auth style across the list breaks
/// Anthropic specifically — which is the provider most people try first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>` — the OpenAI-compatible default.
    Bearer,
    /// `x-api-key: <key>` plus `anthropic-version: 2023-06-01`.
    Anthropic,
    /// No auth header at all — the local runtimes that take no credential.
    None,
}

impl AuthStyle {
    /// The wire spelling, which is also what the console mirror stores.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::Anthropic => "anthropic",
            Self::None => "none",
        }
    }
}

/// The `anthropic-version` header value that rides with [`AuthStyle::Anthropic`].
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// One hosted provider a company can be pointed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloudProvider {
    /// Stable routing key. What a routing entry names, and what a credential
    /// slot is keyed on.
    pub slug: &'static str,
    /// Display label. Never used for routing.
    pub label: &'static str,
    /// The OpenAI-compatible base URL, as a **preset**. See the note below on
    /// why this is never derived from the host.
    pub endpoint: &'static str,
    /// How the credential is presented.
    pub auth: AuthStyle,
    /// What a key for this provider tends to look like, for the input's
    /// placeholder. `None` where the vendor has no recognisable prefix —
    /// inventing one would teach the operator a shape that is not real.
    pub key_placeholder: Option<&'static str>,
}

/// The 26 hosted providers the add-dialog offers.
///
/// ## The endpoints are presets, not a pattern
///
/// Look at the paths: `/openai/v1`, `/inference/v1`, `/v1beta/openai`,
/// `/v1/openai`, `/v3/openai`, `/api/paas/v4`, `/api/gateway`, `/step_plan/v1`.
/// Any attempt to derive an endpoint as `https://{host}/v1` is wrong for
/// roughly a third of this list. That is why each row carries its own URL and
/// why the cloud category never asks the operator to type one.
pub const CLOUD_PROVIDERS: &[CloudProvider] = &[
    CloudProvider {
        slug: "openai",
        label: "OpenAI",
        endpoint: "https://api.openai.com/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("sk-..."),
    },
    CloudProvider {
        slug: "anthropic",
        label: "Anthropic",
        endpoint: "https://api.anthropic.com/v1",
        // The ONLY non-bearer entry in this table. A port that assumes one auth
        // style across the catalogue breaks exactly one provider, and it is the
        // one people reach for first. `AuthStyle::Anthropic` also carries
        // `anthropic-version: 2023-06-01` — see `ANTHROPIC_VERSION`.
        auth: AuthStyle::Anthropic,
        key_placeholder: Some("sk-ant-..."),
    },
    CloudProvider {
        slug: "openrouter",
        label: "OpenRouter",
        endpoint: "https://openrouter.ai/api/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("sk-or-..."),
    },
    CloudProvider {
        slug: "orcarouter",
        label: "OrcaRouter",
        endpoint: "https://api.orcarouter.ai/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("sk-orca-..."),
    },
    CloudProvider {
        slug: "gmi",
        label: "GMI",
        endpoint: "https://api.gmi-serving.com/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("eyJ...."),
    },
    CloudProvider {
        slug: "fireworks",
        label: "Fireworks",
        endpoint: "https://api.fireworks.ai/inference/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("fw-..."),
    },
    CloudProvider {
        slug: "moonshot",
        label: "Kimi (Moonshot)",
        endpoint: "https://api.moonshot.ai/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("sk-..."),
    },
    CloudProvider {
        slug: "groq",
        label: "Groq",
        endpoint: "https://api.groq.com/openai/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("gsk_..."),
    },
    CloudProvider {
        slug: "mistral",
        label: "Mistral",
        endpoint: "https://api.mistral.ai/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "deepseek",
        label: "DeepSeek",
        endpoint: "https://api.deepseek.com/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("sk-..."),
    },
    CloudProvider {
        slug: "together",
        label: "Together AI",
        endpoint: "https://api.together.xyz/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "google",
        label: "Google Gemini",
        endpoint: "https://generativelanguage.googleapis.com/v1beta/openai",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "cerebras",
        label: "Cerebras",
        endpoint: "https://api.cerebras.ai/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "xai",
        label: "xAI",
        endpoint: "https://api.x.ai/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "huggingface",
        label: "Hugging Face",
        endpoint: "https://router.huggingface.co/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("hf_..."),
    },
    CloudProvider {
        slug: "nvidia",
        label: "NVIDIA",
        endpoint: "https://integrate.api.nvidia.com/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "zai",
        label: "Z.AI",
        endpoint: "https://api.z.ai/api/paas/v4",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "minimax",
        label: "MiniMax",
        // This endpoint IS the bug fix; do not "restore" the older value.
        //
        // MiniMax was `https://api.minimax.io/anthropic` with Anthropic auth,
        // which points at MiniMax's Messages-protocol API. Nothing on this side
        // speaks that protocol — we build OpenAI-style `/chat/completions` and
        // `/models` — so both the chat call and the model listing 404'd
        // (`/anthropic/chat/completions`, `/anthropic/models`), and the listing
        // 404 became a live error-reporting issue upstream. MiniMax serves a
        // full OpenAI-compatible surface at `/v1`, so both paths resolve there.
        endpoint: "https://api.minimax.io/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "stepfun",
        label: "StepFun",
        endpoint: "https://api.stepfun.ai/step_plan/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "kilocode",
        label: "Kilo Code",
        endpoint: "https://api.kilo.ai/api/gateway",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "deepinfra",
        label: "DeepInfra",
        endpoint: "https://api.deepinfra.com/v1/openai",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "novita",
        label: "Novita",
        endpoint: "https://api.novita.ai/v3/openai",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "venice",
        label: "Venice",
        endpoint: "https://api.venice.ai/api/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "vercel-ai-gateway",
        label: "Vercel AI Gateway",
        endpoint: "https://ai-gateway.vercel.sh/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: None,
    },
    CloudProvider {
        slug: "sumopod",
        label: "SumoPod",
        endpoint: "https://ai.sumopod.com/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("sk-..."),
    },
    CloudProvider {
        slug: "modelscope",
        label: "ModelScope",
        endpoint: "https://api-inference.modelscope.cn/v1",
        auth: AuthStyle::Bearer,
        key_placeholder: Some("ms-..."),
    },
];

/// One model runtime reachable over an endpoint rather than a vendor account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalRuntime {
    /// Stable routing key.
    pub slug: &'static str,
    /// Display label.
    pub label: &'static str,
    /// A starting endpoint where one is conventional. The operator still types
    /// or confirms it — a local runtime's endpoint is the thing being chosen.
    pub default_endpoint: Option<&'static str>,
    /// Whether this runtime also wants a credential.
    pub needs_key: bool,
}

/// The three local runtimes.
///
/// ## These mean the *host's* localhost, not the operator's laptop
///
/// OpenCompany is a server-side product. openhuman is a desktop app, where
/// `http://localhost:11434` is the machine the person is sitting at. Here it is
/// the container. The category still ports — an operator may genuinely run a
/// runtime beside the host — but it is the reason loopback is an *explicit
/// allowance* in the connect flow's SSRF guard rather than an oversight. See
/// `docs/modules/inference/connect-flow.md`.
pub const LOCAL_RUNTIMES: &[LocalRuntime] = &[
    LocalRuntime {
        slug: "ollama",
        label: "Ollama",
        default_endpoint: Some("http://localhost:11434"),
        needs_key: false,
    },
    LocalRuntime {
        slug: "lmstudio",
        label: "LM Studio",
        default_endpoint: None,
        needs_key: false,
    },
    LocalRuntime {
        slug: "omlx",
        label: "OMLX",
        default_endpoint: None,
        // The only local runtime that wants both an endpoint and a key.
        needs_key: true,
    },
];

/// A credential another command-line tool already holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CliLogin {
    /// What the add-dialog's option is called.
    pub option_slug: &'static str,
    /// What the resulting credential is **stored** under. Not always the same
    /// as `option_slug` — read [`CLI_LOGINS`].
    pub stored_slug: &'static str,
    /// Display label.
    pub label: &'static str,
    /// Whether connecting this option runs the probe. A CLI login that hands
    /// over no key has nothing for a probe to present.
    pub probes: bool,
}

/// The two CLI logins.
///
/// ## Codex is the trap
///
/// The Codex CLI login *is* an OpenAI credential, so it is stored under the
/// `openai` slug and surfaces as the OpenAI row. Keying its "already connected"
/// check on the literal `codex` never matches, which means the dialog would
/// offer Codex forever, however many times it was connected. It also gets **no
/// row of its own** on purpose: a second row would imply a second connection the
/// operator could remove separately, and removing it would take OpenAI with it.
///
/// `claude-code` hands over no key at all, so it skips the probe entirely —
/// there is nothing to present.
///
/// ## On a server-side host this category is empty
///
/// Nothing on the host holds a desktop CLI's login. The console renders the
/// group saying so rather than hiding it: the shape is then already right if a
/// delegated credential ever becomes available, and an empty labelled group is
/// more honest than a missing one.
pub const CLI_LOGINS: &[CliLogin] = &[
    CliLogin {
        option_slug: "claude-code",
        stored_slug: "claude-code",
        label: "Claude Code",
        probes: false,
    },
    CliLogin {
        option_slug: "codex",
        stored_slug: "openai",
        label: "Codex",
        probes: true,
    },
];

/// The `HTTP-Referer` attribution header OpenRouter asks BYOK callers to send —
/// it identifies the app in OpenRouter's dashboard and rankings.
///
/// Here rather than beside either caller because it had **two** values before
/// this module existed: `harness::built_in::provider` sent
/// `https://opencompany.tinyhumans.ai` and `harness::roster_build` sent
/// `https://opencompany.ai`, so the same company attributed its roster-build
/// traffic and its turn traffic to two different apps. Nothing tested that they
/// agreed, so nothing noticed. One constant, two callers.
pub const OPENROUTER_REFERER: &str = "https://opencompany.tinyhumans.ai";

/// The `X-Title` attribution header OpenRouter asks BYOK callers to send.
pub const OPENROUTER_TITLE: &str = "OpenCompany";

/// Authority hosts that serve Azure AI Foundry / Azure OpenAI inference. A host
/// matches when it equals one of these or is a subdomain of it, which is how a
/// per-resource endpoint like `my-resource.openai.azure.com` is recognised.
///
/// ## Why by host and not by slug
///
/// Azure is reachable only through the generic custom-provider flow, so the
/// operator picks the slug (`azure`, `azure-foundry`, `my-azure`, …) and the
/// host is the one stable signal.
///
/// ## Why an Azure endpoint is treated differently at all
///
/// Azure separates the **base model id** a deployment was made from
/// (`gpt-5.6-terra-2026-07-09`) from the user-chosen **deployment name**
/// (`gpt-5.6-terra`) that actually routes the request, and the request body's
/// `model` field keys on the deployment name. A `/models` listing publishes base
/// model ids. So a dropdown sourced from the catalog makes the only correct
/// value unreachable, and an Azure endpoint forces free-text model entry.
///
/// ## What is deliberately absent
///
/// `inference.ai.azure.com` and `models.ai.azure.com` are **not** here. Those
/// are the Foundry *serverless* endpoints, which key `model` on the model name
/// rather than a deployment name. Classifying them would relabel a correct model
/// id as a "deployment name" and mislead the operator in the one place this
/// distinction exists to clarify. Do not add them for symmetry.
pub const AZURE_ENDPOINT_HOSTS: &[&str] = &[
    // The two hosts Microsoft documents for the OpenAI-compatible v1 base URL
    // (`https://<resource>.<host>/openai/v1/`), plus the resource host the older
    // `api-version` surface is served from.
    "openai.azure.com",
    "services.ai.azure.com",
    "cognitiveservices.azure.com",
    // Sovereign clouds — separate DNS parents, so the `.com` entries above do
    // not cover them. Without these a government or 21Vianet tenant falls
    // straight into the "model not found" path this list exists to prevent.
    "openai.azure.us",
    "openai.azure.cn",
];

/// The one provider whose endpoint serves the OpenAI **Responses API**
/// (`/v1/responses`), and may therefore fall back to it on a chat-completions
/// 404.
///
/// Every other preset in [`CLOUD_PROVIDERS`] is chat-completions-only. Enabling
/// the fallback for those guarantees a *second* 404 against a path that does not
/// exist, which floods error reporting with empty-bodied Responses-API events.
const RESPONSES_API_SLUG: &str = "openai";

/// Lowercased authority host of an endpoint URL — scheme, userinfo, port and
/// path dropped. `None` when no host can be parsed.
///
/// Tolerant of a missing scheme and of bracketed IPv6 literals, because the
/// values it reads include endpoints an operator typed. The console mirror
/// implements the same rules, so both sides classify a stored endpoint
/// identically.
pub fn endpoint_host(endpoint: &str) -> Option<String> {
    let trimmed = endpoint.trim();
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split_once(']').map(|(h, _)| h).unwrap_or(rest)
    } else {
        host_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_port)
    };
    let host = host.trim().to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// What [`redact_endpoint`] leaves where the userinfo was.
///
/// It replaces the userinfo only; the delimiting `@` survives, so the result
/// still reads as a URL and the operator can see *that* something was embedded
/// — which is the sentence they need in order to go and move it into the key
/// field.
pub const REDACTED_USERINFO: &str = "***";

/// The byte range of an endpoint's userinfo — everything between `://` (or the
/// start, for a scheme-less value) and the `@` that ends the credential.
///
/// `None` when there is none. The `@` must be inside the **authority**: a path
/// may legitimately contain one (`https://host/v1/@me`), and that is not a
/// credential.
fn endpoint_userinfo_range(endpoint: &str) -> Option<std::ops::Range<usize>> {
    let start = endpoint.find("://").map_or(0, |i| i + "://".len());
    let rest = endpoint.get(start..)?;
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    // `rfind`, not `find`: a password may itself contain an `@`, and the last
    // one in the authority is the delimiter per RFC 3986.
    let at = rest.get(..authority_len)?.rfind('@')?;
    Some(start..start + at)
}

/// Whether an endpoint URL carries a credential in its authority
/// (`http://user:password@host/v1`).
///
/// The check every place that **accepts** an endpoint makes, so that a
/// credential in a URL never reaches storage. It is the same class of rule as
/// the `[inference].api_key_secret` check in
/// [`validate_parts`](super::validate_inference): a secret belongs in the
/// credential slot, which is write-only to the console, and nowhere else. An
/// endpoint is read back by every console reader on every page load, echoed
/// into operator-facing failure text, and written to a plaintext store — so a
/// password in one is a password in all three.
pub fn endpoint_has_credentials(endpoint: &str) -> bool {
    endpoint_userinfo_range(endpoint.trim()).is_some()
}

/// The same endpoint with any embedded credential replaced by
/// [`REDACTED_USERINFO`].
///
/// The second, independent mechanism behind the same invariant as
/// [`endpoint_has_credentials`]. Rejection keeps userinfo out of anything
/// written from now on; this keeps it out of anything **said**, including about
/// values stored before the rejection existed and values that arrived from a
/// `company.toml` or an `OPENCOMPANY_INFERENCE_URL` this host does not own.
///
/// Every endpoint that reaches a response body, an operator-facing message or a
/// log goes through here. The endpoint used to actually *make* a request does
/// not — redacting there would break the request, which is the difference
/// between the two call sites and the reason this is a separate function rather
/// than something done at the point of storage.
pub fn redact_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim();
    match endpoint_userinfo_range(trimmed) {
        None => trimmed.to_string(),
        Some(range) => {
            let mut out = String::with_capacity(trimmed.len());
            out.push_str(&trimmed[..range.start]);
            out.push_str(REDACTED_USERINFO);
            out.push_str(&trimmed[range.end..]);
            out
        }
    }
}

/// The catalogue row for `slug`, if it is a built-in cloud provider.
pub fn cloud_provider(slug: &str) -> Option<&'static CloudProvider> {
    CLOUD_PROVIDERS.iter().find(|p| p.slug == slug)
}

/// The catalogue row for `slug`, if it is a built-in local runtime.
pub fn local_runtime(slug: &str) -> Option<&'static LocalRuntime> {
    LOCAL_RUNTIMES.iter().find(|r| r.slug == slug)
}

/// The catalogue row for `option_slug`, if it is a CLI login option.
pub fn cli_login(option_slug: &str) -> Option<&'static CliLogin> {
    CLI_LOGINS.iter().find(|c| c.option_slug == option_slug)
}

/// How a credential must be presented to a provider of this kind.
///
/// A decision rather than a lookup, because three of the four cases are not in
/// the table: a local runtime that takes no key sends no header, `omlx` takes a
/// bearer even though it is local, and an unknown kind is a custom
/// OpenAI-compatible endpoint, which by definition speaks bearer.
///
/// Getting this wrong is not cosmetic. A probe that presents a bearer to
/// Anthropic is rejected, the rejection classifies as `auth`, and the connect
/// flow deletes a key that was never wrong — on the one provider most people
/// try first.
pub fn auth_style_for(kind: &str) -> AuthStyle {
    let kind = kind.trim();
    if let Some(cloud) = cloud_provider(kind) {
        return cloud.auth;
    }
    if let Some(local) = local_runtime(kind) {
        return if local.needs_key {
            AuthStyle::Bearer
        } else {
            AuthStyle::None
        };
    }
    AuthStyle::Bearer
}

/// The endpoint an operator typed for a **local runtime**, normalised.
///
/// `None` when it is not usable: empty, or not `http`/`https`. The scheme check
/// is here rather than left to the probe because this is the one category whose
/// endpoint the operator types — a cloud provider's comes from the preset and
/// cannot be wrong, and rejecting before any write is what the connect flow's
/// ordering asks for.
///
/// `/v1` is appended when the path is empty or `/`, because that is where an
/// OpenAI-compatible surface lives and `http://localhost:11434` is what the
/// runtime's own documentation prints. Appending is not guessing: a path the
/// operator supplied is left exactly as typed.
///
/// An endpoint carrying userinfo (`http://user:password@host/v1`) is **not**
/// normalised — see [`endpoint_has_credentials`]. This is the point every
/// stored endpoint passes through, so refusing here is what makes "no
/// credential is ever stored in a `base_url`" a property of the store rather
/// than of whichever handler remembered to check. Callers that have a sentence
/// to give the operator ask [`endpoint_has_credentials`] first; this refusal is
/// the backstop for the ones that do not.
pub fn normalize_local_endpoint(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if endpoint_has_credentials(trimmed) {
        return None;
    }
    let (scheme, rest) = trimmed.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return None;
    }
    if rest.trim().is_empty() {
        return None;
    }
    // No path segment at all (the trailing slash is already gone), so the
    // operator gave a bare origin.
    if !rest.contains('/') {
        return Some(format!("{trimmed}/v1"));
    }
    Some(trimmed.to_string())
}

/// OpenRouter's own host. Named rather than compared against
/// [`OPENROUTER_BASE_URL`](super::OPENROUTER_BASE_URL) whole, so a trailing
/// slash or a `/api/v1/` spelling still matches.
const OPENROUTER_ENDPOINT_HOST: &str = "openrouter.ai";

/// Whether an endpoint is OpenRouter's own — **not** the platform proxy in front
/// of it, which is a different host with a different account behind it.
pub fn is_openrouter_endpoint(endpoint: &str) -> bool {
    endpoint_host(endpoint).is_some_and(|host| {
        host == OPENROUTER_ENDPOINT_HOST || host.ends_with(&format!(".{OPENROUTER_ENDPOINT_HOST}"))
    })
}

/// The **account-scoped** catalog path for an endpoint, when it has one.
///
/// `GET /models` on OpenRouter is a public, unauthenticated registry: the whole
/// ~450-model catalogue, whatever bearer is presented. An account whose Settings
/// → Privacy allowed-providers list permits only `novita, openai, baseten,
/// deepseek, deepinfra` is therefore offered every `anthropic/*` model in the
/// picker and gets a 404 at the first turn — a failure discovered after the
/// choice, with the reason buried in a thread reply. A picker that offers models
/// the account cannot use is worse than a short list.
///
/// `GET /models/user` is OpenRouter's documented answer: *"List models filtered
/// by user provider preferences, privacy settings, and guardrails"*, and it is
/// one of the two endpoint groups in their spec carrying a `bearer` security
/// block — the ordinary inference key, not a management key.
///
/// Two parameters are load-bearing:
///
/// * `output_modalities=all` — it **defaults to `text`**, so image, audio and
///   embedding models vanish silently otherwise. That is the trap most likely to
///   surface later as a confusing bug report.
/// * `limit=1000` — the maximum, and the whole permitted catalogue fits in one
///   response, so there is no paging to get wrong.
///
/// `None` for every other endpoint, and for OpenRouter with no credential:
/// account-scoping is a question about a key, and there is nothing to scope to.
/// This is deliberately host-specific rather than a general assumption, the same
/// way the Azure deployment-name rule is — every other provider has its own
/// account restrictions or none.
pub fn scoped_catalog_path(endpoint: &str, authenticated: bool) -> Option<&'static str> {
    (authenticated && is_openrouter_endpoint(endpoint))
        .then_some("/models/user?limit=1000&output_modalities=all")
}

/// Whether an endpoint points at an Azure Foundry / Azure OpenAI resource, i.e.
/// a provider whose `model` field must carry a deployment name.
pub fn is_azure_endpoint(endpoint: &str) -> bool {
    let Some(host) = endpoint_host(endpoint) else {
        return false;
    };
    AZURE_ENDPOINT_HOSTS
        .iter()
        .any(|known| host == *known || host.ends_with(&format!(".{known}")))
}

/// Whether an endpoint **host** is a known cloud host that does not serve the
/// Responses API, whatever slug points at it.
///
/// Derived from [`CLOUD_PROVIDERS`] rather than listed, so the set cannot drift
/// from the table. A host is chat-only when some preset uses it and no preset at
/// that host advertises the Responses API.
///
/// The host axis is what closes the custom-slug gap: a user-defined slug aimed
/// at `integrate.api.nvidia.com` must never attempt `/responses`, while a
/// genuinely unknown proxy host keeps the permissive fallback, because it may be
/// a real OpenAI proxy that does serve it.
pub fn endpoint_is_chat_completions_only(endpoint: &str) -> bool {
    let Some(host) = endpoint_host(endpoint) else {
        return false;
    };
    let mut matched = false;
    for provider in CLOUD_PROVIDERS {
        if endpoint_host(provider.endpoint).as_deref() == Some(host.as_str()) {
            if provider.slug == RESPONSES_API_SLUG {
                return false;
            }
            matched = true;
        }
    }
    matched
}

/// Whether `slug` names anything this catalogue ships — cloud, local, or the
/// slug a CLI login stores under.
///
/// This is the reserved-slug list, and it has exactly **one** meaning: a
/// custom provider may not take a name the catalogue already owns. openhuman
/// keeps two lists under that name meaning two different things, which is a trap
/// worth not porting.
///
/// `ollama` IS reserved here, unlike upstream. Their carve-out exists because
/// their settings panel registers a synthetic `ollama` entry so a model dropdown
/// can resolve a chosen base URL, and their factory's `ollama:` prefix branch
/// fires before the slug lookup. Neither mechanism exists here, so the carve-out
/// would only permit a custom provider to shadow the local runtime.
pub fn is_reserved_slug(slug: &str) -> bool {
    let slug = slug.trim();
    cloud_provider(slug).is_some()
        || local_runtime(slug).is_some()
        || CLI_LOGINS.iter().any(|c| c.stored_slug == slug)
}

/// Which of the three questions a provider answers.
///
/// A category is a fact about the provider, not a routing decision, so it lives
/// with the table rather than in [`resolve`](super::resolve). It is load-bearing
/// there: the rule for scrubbing a removed provider out of the routing map
/// differs per category, because only the cloud refs carry a slug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// A hosted account, addressed by slug.
    Cloud,
    /// A runtime reachable at an endpoint.
    Local,
    /// A credential another command-line tool holds.
    Cli,
}

/// The category a provider kind belongs to.
///
/// Unknown kinds are [`Category::Cloud`]: a custom provider is an
/// OpenAI-compatible endpoint someone pays for, addressed by the slug they
/// named, which is exactly how a cloud row behaves.
pub fn category_of(kind: &str) -> Category {
    let kind = kind.trim();
    if local_runtime(kind).is_some() {
        return Category::Local;
    }
    if CLI_LOGINS
        .iter()
        .any(|c| c.option_slug == kind || (c.stored_slug == kind && c.stored_slug != "openai"))
    {
        return Category::Cli;
    }
    Category::Cloud
}

/// Copy for the add-provider dialog, ported verbatim.
///
/// Strings live beside the table they describe because they *are* part of the
/// port: they say the distinction the three categories exist to make, and a
/// rewrite loses it. The categories are not three slices of one decision — a
/// cloud provider wants an API key, a local runtime wants an endpoint, a CLI
/// login wants nothing because another tool already holds the credential.
pub mod copy {
    /// Group heading, cloud.
    pub const GROUP_CLOUD: &str = "Cloud";
    /// Group heading, local runtimes.
    pub const GROUP_LOCAL: &str = "Local runtimes";
    /// Group heading, CLI logins.
    pub const GROUP_CLI: &str = "CLI logins";

    /// Select placeholder, cloud.
    pub const PLACEHOLDER_CLOUD: &str = "Choose a cloud provider…";
    /// Select placeholder, local runtimes.
    pub const PLACEHOLDER_LOCAL: &str = "Choose a local runtime…";
    /// Select placeholder, CLI logins.
    pub const PLACEHOLDER_CLI: &str = "Choose a CLI login…";

    /// Helper line, cloud.
    pub const HELPER_CLOUD: &str = "Hosted models. You supply an API key.";
    /// Helper line, local runtimes.
    pub const HELPER_LOCAL: &str = "Models running on this machine. You supply the endpoint.";
    /// Helper line, CLI logins.
    pub const HELPER_CLI: &str = "Reuses a login another command line tool already holds.";

    /// Row detail, local runtime. Cloud rows use the endpoint's host instead.
    pub const DETAIL_LOCAL: &str = "Runs on this machine";
    /// Row detail, CLI login.
    pub const DETAIL_CLI: &str = "Uses a login another CLI already holds";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_ships_the_counts_the_plan_names() {
        assert_eq!(CLOUD_PROVIDERS.len(), 26, "cloud providers");
        assert_eq!(LOCAL_RUNTIMES.len(), 3, "local runtimes");
        assert_eq!(CLI_LOGINS.len(), 2, "CLI logins");
    }

    #[test]
    fn every_entry_has_a_parseable_endpoint_and_a_known_auth_style() {
        for provider in CLOUD_PROVIDERS {
            assert!(!provider.slug.is_empty(), "empty slug");
            assert!(!provider.label.is_empty(), "{}: empty label", provider.slug);
            assert!(
                provider.endpoint.starts_with("https://"),
                "{}: cloud endpoints are https",
                provider.slug
            );
            assert!(
                endpoint_host(provider.endpoint).is_some(),
                "{}: endpoint has no parseable host",
                provider.slug
            );
            assert!(
                matches!(provider.auth, AuthStyle::Bearer | AuthStyle::Anthropic),
                "{}: a cloud provider authenticates",
                provider.slug
            );
        }
        for runtime in LOCAL_RUNTIMES {
            assert!(!runtime.slug.is_empty(), "empty slug");
            assert!(!runtime.label.is_empty(), "{}: empty label", runtime.slug);
            if let Some(endpoint) = runtime.default_endpoint {
                assert!(
                    endpoint_host(endpoint).is_some(),
                    "{}: default endpoint has no parseable host",
                    runtime.slug
                );
            }
        }
    }

    #[test]
    fn slugs_are_unique_across_the_whole_catalogue() {
        let mut seen: Vec<&str> = Vec::new();
        for slug in CLOUD_PROVIDERS
            .iter()
            .map(|p| p.slug)
            .chain(LOCAL_RUNTIMES.iter().map(|r| r.slug))
        {
            assert!(!seen.contains(&slug), "duplicate slug {slug}");
            seen.push(slug);
        }
    }

    #[test]
    fn anthropic_is_the_only_non_bearer_cloud_entry() {
        let anthropic: Vec<&str> = CLOUD_PROVIDERS
            .iter()
            .filter(|p| p.auth == AuthStyle::Anthropic)
            .map(|p| p.slug)
            .collect();
        assert_eq!(anthropic, vec!["anthropic"]);
    }

    #[test]
    fn minimax_keeps_the_openai_surface_that_is_the_fix() {
        // Reverting to `/anthropic` 404s both chat and the model listing. The
        // comment on the row says why; this makes reverting it fail loudly.
        let minimax = cloud_provider("minimax").expect("minimax is in the catalogue");
        assert_eq!(minimax.endpoint, "https://api.minimax.io/v1");
        assert_eq!(minimax.auth, AuthStyle::Bearer);
    }

    #[test]
    fn the_managed_first_party_backend_is_not_a_row() {
        // openhuman's 27th entry is their own managed backend. Ours is modelled
        // separately, with its own auth path; a row here would give it a second
        // bearer-shaped identity.
        assert!(cloud_provider("openhuman").is_none());
    }

    #[test]
    fn endpoint_host_drops_scheme_userinfo_port_and_path() {
        assert_eq!(
            endpoint_host("https://api.openai.com/v1").as_deref(),
            Some("api.openai.com")
        );
        assert_eq!(
            endpoint_host("api.groq.com/openai/v1").as_deref(),
            Some("api.groq.com")
        );
        assert_eq!(
            endpoint_host("http://user:pw@host.example:8080/v1").as_deref(),
            Some("host.example")
        );
        assert_eq!(
            endpoint_host("http://[::1]:11434/v1").as_deref(),
            Some("::1")
        );
        assert_eq!(
            endpoint_host("HTTPS://API.OpenAI.com/v1").as_deref(),
            Some("api.openai.com")
        );
        assert_eq!(endpoint_host("   "), None);
    }

    #[test]
    fn only_openai_serves_the_responses_api_fallback() {
        assert!(!endpoint_is_chat_completions_only(
            "https://api.openai.com/v1"
        ));
        // The custom-slug gap: a user-defined slug aimed at a known chat-only
        // host still must not try `/responses`.
        assert!(endpoint_is_chat_completions_only(
            "https://integrate.api.nvidia.com/v1"
        ));
        assert!(endpoint_is_chat_completions_only(
            "https://api.groq.com/openai/v1"
        ));
        // A genuinely unknown proxy keeps the permissive fallback.
        assert!(!endpoint_is_chat_completions_only(
            "https://proxy.acme.dev/v1"
        ));
    }

    #[test]
    fn azure_is_detected_by_host_including_subdomains_and_sovereign_clouds() {
        assert!(is_azure_endpoint(
            "https://my-resource.openai.azure.com/openai/v1"
        ));
        assert!(is_azure_endpoint(
            "https://r.services.ai.azure.com/openai/v1"
        ));
        assert!(is_azure_endpoint(
            "https://r.cognitiveservices.azure.com/openai/v1"
        ));
        assert!(is_azure_endpoint("https://r.openai.azure.us/openai/v1"));
        assert!(is_azure_endpoint("https://r.openai.azure.cn/openai/v1"));
        assert!(!is_azure_endpoint("https://api.openai.com/v1"));
    }

    #[test]
    fn the_foundry_serverless_hosts_are_deliberately_not_azure() {
        // Classifying these would relabel a correct model id as a deployment
        // name — the exact confusion the Azure rule exists to prevent. This test
        // is the guard against adding them back for symmetry.
        assert!(!is_azure_endpoint(
            "https://r.inference.ai.azure.com/models"
        ));
        assert!(!is_azure_endpoint("https://r.models.ai.azure.com/models"));
    }

    #[test]
    fn a_suffix_that_merely_ends_with_an_azure_host_is_not_azure() {
        // `notopenai.azure.com.evil.test` must not match, and neither must a
        // host that merely ends in the same characters without a dot boundary.
        assert!(!is_azure_endpoint("https://myopenai.azure.comx/v1"));
        assert!(!is_azure_endpoint("https://openai.azure.com.evil.test/v1"));
    }

    #[test]
    fn codex_stores_under_openai_so_its_connected_check_can_match() {
        let codex = cli_login("codex").expect("codex is a CLI login");
        assert_eq!(codex.stored_slug, "openai");
        assert!(codex.probes);
        let claude = cli_login("claude-code").expect("claude-code is a CLI login");
        assert_eq!(claude.stored_slug, "claude-code");
        // No key changes hands, so there is nothing for a probe to present.
        assert!(!claude.probes);
    }

    #[test]
    fn reserved_slugs_cover_cloud_local_and_stored_cli_names() {
        assert!(is_reserved_slug("openrouter"));
        assert!(is_reserved_slug("ollama"));
        assert!(is_reserved_slug("claude-code"));
        // Codex stores under `openai`, which is already reserved as a cloud row.
        assert!(is_reserved_slug("openai"));
        assert!(!is_reserved_slug("acme-gateway"));
    }

    #[test]
    fn a_kind_lands_in_the_category_its_scrub_rule_needs() {
        assert_eq!(category_of("openrouter"), Category::Cloud);
        assert_eq!(category_of("ollama"), Category::Local);
        assert_eq!(category_of("lmstudio"), Category::Local);
        assert_eq!(category_of("claude-code"), Category::Cli);
        // Codex stores under `openai`, and `openai` is a cloud row in its own
        // right — so the shared slug stays Cloud. Reading it as a CLI login
        // would give the OpenAI row the CLI's slug-less scrub rule and orphan
        // every route naming it.
        assert_eq!(category_of("openai"), Category::Cloud);
        assert_eq!(category_of("acme-gateway"), Category::Cloud);
    }

    #[test]
    fn omlx_is_the_only_local_runtime_that_wants_a_key() {
        let with_keys: Vec<&str> = LOCAL_RUNTIMES
            .iter()
            .filter(|r| r.needs_key)
            .map(|r| r.slug)
            .collect();
        assert_eq!(with_keys, vec!["omlx"]);
    }

    #[test]
    fn no_other_module_writes_its_own_openrouter_attribution_headers() {
        // The failure this guards is not a wrong value, it is a SECOND value.
        // `harness::built_in::provider` and `harness::roster_build` each spelled
        // the referer out, with different hosts, so one company's turn traffic
        // and its roster-build traffic reached OpenRouter's dashboard as two
        // apps. Both copies looked right in isolation, which is why reading
        // either one never found it.
        //
        // So the assertion is about shape rather than content: any file that
        // mentions the header must reach this constant for its value.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(source) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if path.file_name().and_then(|n| n.to_str()) == Some("catalogue.rs") {
                    continue;
                }
                if source.contains("\"HTTP-Referer\"") && !source.contains("OPENROUTER_REFERER") {
                    offenders.push(path.display().to_string());
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these files write an OpenRouter attribution header without reading \
             `catalogue::OPENROUTER_REFERER`, which is how the two copies \
             diverged the first time: {offenders:?}"
        );
    }

    // ── The cross-language check ────────────────────────────────────────────
    //
    // The console needs this table too, and TypeScript cannot read a Rust
    // `const`. So there are two copies, and the only thing that keeps two
    // hand-maintained copies honest is something that fails when they disagree.
    // openhuman has the same two copies and no such test, which is listed as a
    // defect for exactly that reason; this repo had six copies and two had
    // already silently drifted.
    //
    // The reader below is deliberately strict — one field per line, quoted
    // values, no comments inside an entry — and the mirror's header says so. A
    // forgiving parser would let the file drift into a shape the test quietly
    // stops checking, which is worse than no test because it reads as coverage.

    fn mirror_source() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("frontend/src/inference/catalogue.ts");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("console mirror at {} is unreadable: {e}", path.display()))
    }

    /// The text between `export const <name> … = [` and the closing `];`.
    fn array_body<'a>(src: &'a str, name: &str) -> &'a str {
        let decl = format!("export const {name}");
        let start = src
            .find(&decl)
            .unwrap_or_else(|| panic!("the mirror declares no {name}"));
        let open = src[start..]
            .find('[')
            .unwrap_or_else(|| panic!("{name} is not an array literal"))
            + start
            + 1;
        let close = src[open..]
            .find("\n];")
            .unwrap_or_else(|| panic!("{name} is not terminated by a bare `];`"))
            + open;
        &src[open..close]
    }

    /// Object literals in an array body, as ordered `(field, value)` lists.
    fn object_entries(body: &str) -> Vec<Vec<(String, String)>> {
        let mut entries = Vec::new();
        let mut current: Option<Vec<(String, String)>> = None;
        for line in body.lines() {
            let line = line.trim();
            if line == "{" {
                current = Some(Vec::new());
                continue;
            }
            if line == "}," || line == "}" {
                if let Some(fields) = current.take() {
                    entries.push(fields);
                }
                continue;
            }
            let Some(fields) = current.as_mut() else {
                continue;
            };
            let Some((key, value)) = line.split_once(':') else {
                panic!("mirror entry line is not `field: value` — {line:?}");
            };
            let value = value.trim().trim_end_matches(',').trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            fields.push((key.trim().to_string(), value.to_string()));
        }
        assert!(
            current.is_none(),
            "an unterminated object literal in the mirror"
        );
        entries
    }

    /// Bare quoted strings in an array body.
    fn string_entries(body: &str) -> Vec<String> {
        body.lines()
            .filter_map(|line| {
                let line = line.trim().trim_end_matches(',');
                line.strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .map(str::to_string)
            })
            .collect()
    }

    /// A field of a parsed entry, or `None` when the mirror omits it.
    fn field<'a>(entry: &'a [(String, String)], name: &str) -> Option<&'a str> {
        entry
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// A field the mirror must carry.
    fn required<'a>(entry: &'a [(String, String)], name: &str, row: usize) -> &'a str {
        field(entry, name).unwrap_or_else(|| panic!("mirror row {row} has no `{name}`: {entry:?}"))
    }

    #[test]
    fn the_console_mirror_lists_the_same_cloud_providers() {
        let src = mirror_source();
        let entries = object_entries(array_body(&src, "CLOUD_PROVIDERS"));
        assert_eq!(
            entries.len(),
            CLOUD_PROVIDERS.len(),
            "the mirror lists {} cloud providers, Rust lists {}",
            entries.len(),
            CLOUD_PROVIDERS.len()
        );
        for (row, (entry, provider)) in entries.iter().zip(CLOUD_PROVIDERS).enumerate() {
            assert_eq!(
                required(entry, "slug", row),
                provider.slug,
                "row {row} slug"
            );
            assert_eq!(
                required(entry, "label", row),
                provider.label,
                "{}: label",
                provider.slug
            );
            assert_eq!(
                required(entry, "endpoint", row),
                provider.endpoint,
                "{}: endpoint",
                provider.slug
            );
            assert_eq!(
                required(entry, "auth", row),
                provider.auth.as_str(),
                "{}: auth style",
                provider.slug
            );
            assert_eq!(
                field(entry, "keyPlaceholder"),
                provider.key_placeholder,
                "{}: key placeholder",
                provider.slug
            );
        }
    }

    #[test]
    fn the_console_mirror_lists_the_same_local_runtimes() {
        let src = mirror_source();
        let entries = object_entries(array_body(&src, "LOCAL_RUNTIMES"));
        assert_eq!(entries.len(), LOCAL_RUNTIMES.len(), "local runtime count");
        for (row, (entry, runtime)) in entries.iter().zip(LOCAL_RUNTIMES).enumerate() {
            assert_eq!(required(entry, "slug", row), runtime.slug, "row {row} slug");
            assert_eq!(
                required(entry, "label", row),
                runtime.label,
                "{}: label",
                runtime.slug
            );
            assert_eq!(
                field(entry, "defaultEndpoint"),
                runtime.default_endpoint,
                "{}: default endpoint",
                runtime.slug
            );
            assert_eq!(
                required(entry, "needsKey", row),
                if runtime.needs_key { "true" } else { "false" },
                "{}: needs a key",
                runtime.slug
            );
        }
    }

    #[test]
    fn the_console_mirror_lists_the_same_cli_logins() {
        let src = mirror_source();
        let entries = object_entries(array_body(&src, "CLI_LOGINS"));
        assert_eq!(entries.len(), CLI_LOGINS.len(), "CLI login count");
        for (row, (entry, login)) in entries.iter().zip(CLI_LOGINS).enumerate() {
            assert_eq!(
                required(entry, "optionSlug", row),
                login.option_slug,
                "row {row} option slug"
            );
            // The Codex trap: this is the assertion that keeps the console's
            // already-connected check able to match at all.
            assert_eq!(
                required(entry, "storedSlug", row),
                login.stored_slug,
                "{}: stored slug",
                login.option_slug
            );
            assert_eq!(
                required(entry, "label", row),
                login.label,
                "{}: label",
                login.option_slug
            );
            assert_eq!(
                required(entry, "probes", row),
                if login.probes { "true" } else { "false" },
                "{}: probes",
                login.option_slug
            );
        }
    }

    #[test]
    fn the_console_mirror_lists_the_same_azure_hosts() {
        let src = mirror_source();
        let hosts = string_entries(array_body(&src, "AZURE_ENDPOINT_HOSTS"));
        assert_eq!(
            hosts,
            AZURE_ENDPOINT_HOSTS
                .iter()
                .map(|h| h.to_string())
                .collect::<Vec<_>>(),
            "the Azure host lists have diverged — including, possibly, by adding \
             the Foundry serverless hosts to one side"
        );
    }

    #[test]
    fn the_console_mirror_uses_the_same_copy() {
        let src = mirror_source();
        let start = src
            .find("export const COPY = {")
            .expect("the mirror declares no COPY");
        let open = src[start..].find('{').expect("COPY is not an object") + start + 1;
        let close = src[open..]
            .find("\n} as const;")
            .expect("COPY is not terminated by `} as const;`")
            + open;
        let mut pairs = Vec::new();
        for line in src[open..close].lines() {
            let line = line.trim().trim_end_matches(',');
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            let Some(value) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
                continue;
            };
            pairs.push((key.trim().to_string(), value.to_string()));
        }
        let lookup = |name: &str| -> String {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .unwrap_or_else(|| panic!("the mirror's COPY has no `{name}`"))
                .1
                .clone()
        };
        assert_eq!(lookup("groupCloud"), copy::GROUP_CLOUD);
        assert_eq!(lookup("groupLocal"), copy::GROUP_LOCAL);
        assert_eq!(lookup("groupCli"), copy::GROUP_CLI);
        assert_eq!(lookup("placeholderCloud"), copy::PLACEHOLDER_CLOUD);
        assert_eq!(lookup("placeholderLocal"), copy::PLACEHOLDER_LOCAL);
        assert_eq!(lookup("placeholderCli"), copy::PLACEHOLDER_CLI);
        assert_eq!(lookup("helperCloud"), copy::HELPER_CLOUD);
        assert_eq!(lookup("helperLocal"), copy::HELPER_LOCAL);
        assert_eq!(lookup("helperCli"), copy::HELPER_CLI);
        assert_eq!(lookup("detailLocal"), copy::DETAIL_LOCAL);
        assert_eq!(lookup("detailCli"), copy::DETAIL_CLI);
        assert_eq!(pairs.len(), 11, "a copy string was added to one side only");
    }

    // ---- auth style and endpoint normalisation ------------------------------

    #[test]
    fn anthropic_is_the_only_non_bearer_entry_in_the_catalogue() {
        // A port that assumes one auth style breaks exactly one provider, and it
        // is the one people try first. Worse, the rejection classifies as `auth`
        // — the one destructive class — so the connect flow would delete a key
        // that was never wrong.
        assert_eq!(auth_style_for("anthropic"), AuthStyle::Anthropic);
        for provider in CLOUD_PROVIDERS.iter().filter(|p| p.slug != "anthropic") {
            assert_eq!(
                auth_style_for(provider.slug),
                AuthStyle::Bearer,
                "{} should be bearer",
                provider.slug
            );
        }
    }

    #[test]
    fn a_keyless_local_runtime_sends_no_auth_header_and_omlx_does() {
        assert_eq!(auth_style_for("ollama"), AuthStyle::None);
        assert_eq!(auth_style_for("lmstudio"), AuthStyle::None);
        // The only local runtime that wants both an endpoint and a key.
        assert_eq!(auth_style_for("omlx"), AuthStyle::Bearer);
    }

    #[test]
    fn an_unknown_kind_is_a_custom_openai_compatible_endpoint() {
        // Which by definition speaks bearer — that is what "OpenAI-compatible"
        // means in the field the operator typed it into.
        assert_eq!(auth_style_for("my-gateway"), AuthStyle::Bearer);
        assert_eq!(auth_style_for("custom"), AuthStyle::Bearer);
    }

    #[test]
    fn a_bare_origin_gains_the_v1_an_openai_surface_lives_at() {
        // `http://localhost:11434` is what Ollama's own documentation prints,
        // and it is not where the OpenAI-compatible surface is.
        assert_eq!(
            normalize_local_endpoint("http://localhost:11434").as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(
            normalize_local_endpoint("  http://localhost:11434/  ").as_deref(),
            Some("http://localhost:11434/v1")
        );
    }

    #[test]
    fn a_path_the_operator_supplied_is_left_exactly_as_typed() {
        // Appending is not guessing. Someone who typed a path meant it.
        assert_eq!(
            normalize_local_endpoint("https://acme.example/api/gateway").as_deref(),
            Some("https://acme.example/api/gateway")
        );
        assert_eq!(
            normalize_local_endpoint("http://127.0.0.1:1234/v1/").as_deref(),
            Some("http://127.0.0.1:1234/v1")
        );
    }

    #[test]
    fn an_endpoint_carrying_a_credential_is_not_an_endpoint() {
        // The security half of the same refusal. `normalize_local_endpoint` is
        // the funnel every stored endpoint passes through, so refusing here is
        // what makes "no credential is ever stored in a `base_url`" a property
        // of the store rather than of whichever handler remembered to check.
        for bad in [
            "http://alice:hunter2@127.0.0.1:8597/v1",
            "https://alice@api.acme.example/v1",
            "http://alice:hunter2@127.0.0.1:8597",
            // A password may itself contain an `@`; the authority still has one.
            "http://alice:hun@ter2@127.0.0.1:8597/v1",
        ] {
            assert!(endpoint_has_credentials(bad), "`{bad}` carries userinfo");
            assert!(
                normalize_local_endpoint(bad).is_none(),
                "`{bad}` must not normalise into something storable"
            );
        }
    }

    #[test]
    fn an_at_sign_in_the_path_is_not_a_credential() {
        // The `@` has to be inside the authority. A path may legitimately carry
        // one, and refusing those would reject perfectly good endpoints.
        for good in [
            "https://api.acme.example/v1/@me",
            "https://api.acme.example/v1?to=a@b",
            "https://api.acme.example/v1#a@b",
        ] {
            assert!(!endpoint_has_credentials(good), "`{good}` has no userinfo");
            assert_eq!(redact_endpoint(good), good);
        }
    }

    #[test]
    fn redacting_an_endpoint_removes_the_credential_and_nothing_else() {
        // Observed in the incident: reqwest masks userinfo in its own error
        // Display (`for url (http://127.0.0.1:8597/v1/models)`), and then the
        // handler's own `format!` put it back from the endpoint we hold.
        assert_eq!(
            redact_endpoint("http://alice:hunter2@127.0.0.1:8597/v1"),
            "http://***@127.0.0.1:8597/v1"
        );
        assert_eq!(
            redact_endpoint("https://alice@api.acme.example/v1"),
            "https://***@api.acme.example/v1"
        );
        // The last `@` in the authority is the delimiter, so a password
        // containing one is removed whole rather than half-left behind.
        assert_eq!(
            redact_endpoint("http://alice:hun@ter2@127.0.0.1:8597/v1"),
            "http://***@127.0.0.1:8597/v1"
        );
        // Scheme-less, as `normalize_setup_base_url` accepts.
        assert_eq!(
            redact_endpoint("alice:hunter2@localhost:1234/v1"),
            "***@localhost:1234/v1"
        );
        // Nothing to redact: byte-for-byte the same endpoint, trimmed.
        assert_eq!(
            redact_endpoint("  https://api.openai.com/v1  "),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn only_http_and_https_are_endpoints() {
        // Rejected here rather than at the probe, because this is the one
        // category whose endpoint the operator types — and the connect flow's
        // ordering says reject before any write.
        for bad in [
            "file:///etc/passwd",
            "ftp://acme.example/v1",
            "localhost:11434",
            "",
            "   ",
            "http://",
        ] {
            assert!(
                normalize_local_endpoint(bad).is_none(),
                "`{bad}` is not an endpoint"
            );
        }
    }
    #[test]
    fn only_openrouters_own_host_gets_the_account_scoped_catalogue() {
        assert!(is_openrouter_endpoint("https://openrouter.ai/api/v1"));
        assert!(is_openrouter_endpoint("https://openrouter.ai/api/v1/"));
        assert!(is_openrouter_endpoint("https://eu.openrouter.ai/api/v1"));
        // The platform proxy fronts OpenRouter and serves the same catalogue,
        // but the account behind it is the server's, not the tenant's — and it
        // is a different host, which is the whole point of matching on one.
        assert!(!is_openrouter_endpoint(
            "https://api.tinyhumans.ai/openai/v1"
        ));
        assert!(!is_openrouter_endpoint(
            "https://openrouter.ai.example.com/v1"
        ));
        assert!(!is_openrouter_endpoint("http://127.0.0.1:11434/v1"));
    }

    #[test]
    fn the_scoped_catalogue_needs_both_the_host_and_a_credential() {
        let path = scoped_catalog_path(super::super::OPENROUTER_BASE_URL, true)
            .expect("OpenRouter with a key reads the account-scoped list");
        assert!(path.starts_with("/models/user"));
        // `output_modalities` defaults to `text`, so leaving it off silently
        // drops every image, audio and embedding model.
        assert!(path.contains("output_modalities=all"), "{path}");
        assert!(path.contains("limit=1000"), "{path}");

        // Account-scoping is a question about a key. With none there is nothing
        // to scope to, and the public registry is the honest answer.
        assert!(scoped_catalog_path(super::super::OPENROUTER_BASE_URL, false).is_none());
        // Host-specific on purpose, the same way the Azure deployment-name rule
        // is. Every other provider has its own account restrictions or none.
        assert!(scoped_catalog_path("https://api.openai.com/v1", true).is_none());
        assert!(scoped_catalog_path("https://api.tinyhumans.ai/openai/v1", true).is_none());
    }
}
