//! Per-tenant Bring-Your-Own-Key inference (issue #56): the inert data model
//! plus the async secret-resolution used to materialize a company's *effective*
//! inference configuration.
//!
//! A company's effective inference config is the highest-precedence of three
//! sources:
//!
//! 1. **Runtime** — a config the operator sets through the console, persisted as
//!    a single JSON blob in the [`SecretStore`](crate::ports::SecretStore) under
//!    [`RUNTIME_CONFIG_KEY`]. Highest precedence, so a console switch takes
//!    effect on the agents' next turn with no rebuild.
//! 2. **Manifest** — the `[inference]` section committed in `company.toml`
//!    ([`Inference`]). Declarative intent; never a credential.
//! 3. **Default** — the platform-injected managed brain
//!    (`TINYHUMANS_API_KEY` / `OPENCOMPANY_INFERENCE_*`), passed in as an
//!    [`EnvDefault`]. Lowest precedence.
//!
//! Credentials live apart from the declarations. The outbound key is written to
//! its own [`KEY_KEY`] secret (write-only via the console) — never inline in the
//! runtime config blob or the manifest — and is attached to the
//! [`InferenceDecl`] as a [`Credential`] by [`resolve_effective`], then read on
//! the request path by [`InferenceDecl::bearer`]. Deferring the read is what lets
//! the managed tier be a *rotating* platform token rather than a value captured
//! once at boot. Nothing here ever serializes a credential into an API response,
//! log line, or agent-visible output: [`InferenceDecl`] derives no `Serialize`
//! and its `Debug` redacts the credential.

pub mod catalogue;
pub mod probe;
pub mod resolve;
pub mod store;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::company::credentials::Credential;
use crate::company::types::{INFERENCE_PROVIDERS, Inference};
use crate::error::OpenCompanyError;
use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, SecretValue};

use self::store::provider_key_key;

/// The [`SecretStore`](crate::ports::SecretStore) key holding the JSON runtime
/// inference override (a [`RuntimeInference`] the console writes).
pub const RUNTIME_CONFIG_KEY: &str = "inference/config";

/// The canonical per-company inference credential key. The outbound token is
/// stored here (write-only via the console); the value is the raw token string.
pub const KEY_KEY: &str = "inference/key";

/// The [`SecretStore`](crate::ports::SecretStore) key holding the runtime
/// inference override for `harness_id`.
///
/// The **default** harness keeps the flat legacy key. That is not cosmetic: a
/// tenant's stored console override and credential already live at
/// [`RUNTIME_CONFIG_KEY`] / [`KEY_KEY`], and the store has no rename — so
/// namespacing every harness would silently orphan the config of every company
/// already running, which is the one migration this design cannot afford.
///
/// Non-default harnesses namespace under `harness/<id>/`, so two `built_in`
/// harnesses can hold two different OpenRouter accounts.
pub fn runtime_config_key(harness_id: &str, is_default: bool) -> String {
    if is_default {
        return RUNTIME_CONFIG_KEY.to_string();
    }
    format!("harness/{harness_id}/{RUNTIME_CONFIG_KEY}")
}

/// The credential key for `harness_id`. Same default-harness rule as
/// [`runtime_config_key`].
pub fn harness_key_key(harness_id: &str, is_default: bool) -> String {
    if is_default {
        return KEY_KEY.to_string();
    }
    format!("harness/{harness_id}/{KEY_KEY}")
}

/// Which harness's secrets a resolution reads, and whether that harness is the
/// company default (which keeps the flat legacy keys).
///
/// Passed as one value rather than two loose arguments because the pair is only
/// ever meaningful together — an id without the default flag cannot name a key.
#[derive(Clone, Debug)]
pub struct HarnessScope {
    /// The harness id.
    pub id: String,
    /// Whether it is the company's default harness.
    pub is_default: bool,
}

impl HarnessScope {
    /// The scope for a company's default harness — what every pre-existing
    /// caller means, and what keeps them reading the flat keys.
    pub fn default_harness(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            is_default: true,
        }
    }

    /// A named, non-default harness.
    pub fn named(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            is_default: false,
        }
    }

    /// This scope's runtime-config secret key.
    pub fn config_key(&self) -> String {
        runtime_config_key(&self.id, self.is_default)
    }

    /// This scope's credential secret key.
    pub fn key_key(&self) -> String {
        harness_key_key(&self.id, self.is_default)
    }
}

impl Default for HarnessScope {
    fn default() -> Self {
        Self::default_harness(crate::company::types::IMPLICIT_HARNESS_ID)
    }
}

/// The platform's OpenAI-compatible endpoint — the subscription proxy an
/// `openrouter` company with **no** key of its own resolves against.
///
/// The proxy fronts OpenRouter upstream and meters the spend against the
/// tenant's subscription, so from the workload's point of view this and
/// [`OPENROUTER_BASE_URL`] serve the same catalogue; only who pays differs.
pub const PLATFORM_BASE_URL: &str = "https://api.tinyhumans.ai/openai/v1";

/// The provider kind removed when OpenCompany stopped exposing its own model
/// SKUs. A manifest or stored runtime blob still naming it aliases to
/// [`DEFAULT_PROVIDER`] rather than failing: a runtime blob is data an operator
/// cannot hand-edit, so hard-failing on it would strand a tenant whose console
/// wrote a value that used to be valid.
pub const LEGACY_MANAGED: &str = "managed";

/// The provider a company gets when nothing names one.
pub const DEFAULT_PROVIDER: &str = "openrouter";

/// The concrete model id per abstract tier **in OpenRouter's vocabulary**.
///
/// Not a universal default, and the name is the only thing about it that ever
/// suggested otherwise. These four strings are OpenRouter catalog ids: they are
/// meaningful at an endpoint that publishes OpenRouter's catalog and meaningless
/// anywhere else, so applying them is only ever correct once
/// [`TierVocabulary::Concrete`] has been established for the endpoint the
/// request is about to travel to. See [`model_for_tier`].
///
/// The slugs mirror the platform's own OpenRouter bindings, so a proxied tier
/// and a direct substitution resolve to the same models by default.
pub const DEFAULT_TIER_MODELS: &[(&str, &str)] = &[
    ("chat-v1", "anthropic/claude-sonnet-5"),
    ("reasoning-v1", "openai/gpt-5.6-sol-pro"),
    ("agentic-v1", "anthropic/claude-opus-5"),
    ("vision-v1", "qwen/qwen3.8-max"),
];

/// Which model vocabulary an endpoint speaks — the question that has to be
/// answered before [`model_for_tier`] can decide whether to substitute anything.
///
/// **This is a property of the endpoint, not of who pays for it.** It used to be
/// read off [`InferenceDecl::is_proxied`], which is true only for the `openrouter`
/// kind with no tenant key; every other config was assumed to want OpenRouter
/// catalog ids. That conflated the payer with the vocabulary and broke the
/// moment the two came apart: a company pointing `provider = "openrouter"` at
/// `https://api.tinyhumans.ai/openai/v1` **with its own key** is not proxied, so
/// every tier was rewritten to `anthropic/claude-opus-5` and friends and the
/// endpoint answered `Model 'anthropic/claude-sonnet-5' is not available` — for
/// an endpoint whose catalog publishes `chat-v1` and `agentic-v1` directly.
///
/// Every OpenAI-compatible endpoint publishes its catalog at `GET
/// {base_url}/models`, so the vocabulary is *discoverable* rather than
/// guessable: see [`TierVocabulary::from_catalog_ids`]. Nothing here keys off a
/// hostname — a provider that publishes `agentic-v1` is telling us it resolves
/// tiers itself, whoever it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TierVocabulary {
    /// The endpoint publishes the tier names themselves, so it resolves a tier
    /// against its own registry. Send the tier verbatim — substituting a
    /// concrete id bypasses that routing and, on the platform proxy, is
    /// rejected outright unless passthrough is switched on.
    Tiers,
    /// The endpoint publishes ids [`DEFAULT_TIER_MODELS`] names, so a bare tier
    /// would 400 and the shipped mapping is the right default — **for the tiers
    /// it actually publishes**, which is what the payload records.
    Concrete(ConcreteTiers),
    /// The endpoint's catalog was read and publishes neither vocabulary. We know
    /// the shipped ids are *absent* from it, so applying them would be a guess
    /// already contradicted by evidence.
    Unknown,
}

/// Which of [`DEFAULT_TIER_MODELS`]' four ids an endpoint's catalog publishes,
/// one bit per tier in `DEFAULT_TIER_MODELS` order.
///
/// [`TierVocabulary::Concrete`] used to be a bare marker, and that lost the
/// distinction this type exists for. *Classification* is `any` — one shipped id
/// present is enough to say "this endpoint speaks OpenRouter's catalog" — but
/// *substitution* is per tier, and the two are not the same question. A partial
/// mirror publishing `anthropic/claude-sonnet-5` and nothing else was
/// classified `Concrete` and then handed `openai/gpt-5.6-sol-pro` for
/// `reasoning-v1`: an id that very catalog had just said it does not serve.
/// That is this PR's own defect one level down, and it has to fail the same
/// way — [`model_for_tier`] leaves an unpublished tier alone rather than
/// synthesizing an id against evidence we already hold.
///
/// A bitmask rather than a set, so the enum stays `Copy` and allocation-free:
/// it rides on [`InferenceDecl`], which is cloned on every turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConcreteTiers(u8);

impl ConcreteTiers {
    /// Every shipped tier — the pre-discovery assumption, and the truth for
    /// OpenRouter itself, whose registry is where these four ids come from.
    pub fn all() -> Self {
        Self((1u8 << DEFAULT_TIER_MODELS.len()) - 1)
    }

    /// Does this endpoint publish the shipped id for `tier`?
    pub fn publishes(self, tier: &str) -> bool {
        DEFAULT_TIER_MODELS
            .iter()
            .position(|(name, _)| *name == tier)
            .is_some_and(|index| self.0 & (1u8 << index) != 0)
    }

    fn with(self, index: usize) -> Self {
        Self(self.0 | (1u8 << index))
    }

    fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl TierVocabulary {
    /// Classify an endpoint from the ids it publishes at `{base_url}/models`.
    ///
    /// Tiers win when present: a catalog containing `agentic-v1` resolves tiers
    /// server-side, and that stays true even if it also lists concrete models.
    /// `any` rather than `all` on purpose — a provider that publishes three of
    /// the four tiers still speaks tiers, and demanding a complete set would
    /// silently fall back to substitution for it.
    ///
    /// [`Self::Concrete`] is reached on `any` too, for the same reason, but it
    /// carries **which** shipped ids were actually seen rather than implying all
    /// four: a partial mirror is still an OpenRouter-vocabulary endpoint, and
    /// the tiers it does not publish simply have no default to substitute. See
    /// [`ConcreteTiers`].
    pub fn from_catalog_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Self {
        let ids: std::collections::HashSet<&str> = ids.into_iter().collect();
        if crate::company::types::INFERENCE_TIERS
            .iter()
            .any(|tier| ids.contains(tier))
        {
            return Self::Tiers;
        }
        let published = DEFAULT_TIER_MODELS.iter().enumerate().fold(
            ConcreteTiers::default(),
            |acc, (index, (_, model))| {
                if ids.contains(model) {
                    acc.with(index)
                } else {
                    acc
                }
            },
        );
        if published.is_empty() {
            return Self::Unknown;
        }
        Self::Concrete(published)
    }

    /// The wire/console label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tiers => "tiers",
            Self::Concrete(_) => "concrete",
            Self::Unknown => "unknown",
        }
    }

    /// The tier → model mapping to offer as *this endpoint's* defaults.
    ///
    /// Empty for [`Self::Unknown`], and that emptiness is the point: an endpoint
    /// whose catalog publishes neither vocabulary has no default we can honestly
    /// supply, so the console must ask the operator for a model id per tier
    /// rather than prefill four ids the catalog has already told us are not
    /// there. Handing over a mapping we know is unusable is the failure this
    /// whole type exists to stop.
    ///
    /// For [`Self::Concrete`] the same rule applies **per tier**: a partial
    /// mirror gets defaults for the tiers it publishes and nothing for the rest,
    /// rather than four ids of which some are already known to be absent.
    pub fn tier_defaults(self) -> BTreeMap<String, String> {
        match self {
            Self::Tiers => crate::company::types::INFERENCE_TIERS
                .iter()
                .map(|tier| ((*tier).to_string(), (*tier).to_string()))
                .collect(),
            Self::Concrete(published) => DEFAULT_TIER_MODELS
                .iter()
                .filter(|(tier, _)| published.publishes(tier))
                .map(|(tier, model)| ((*tier).to_string(), (*model).to_string()))
                .collect(),
            Self::Unknown => BTreeMap::new(),
        }
    }
}

/// The concrete model id to put on the wire for `tier`, given what the endpoint
/// this request is about to reach actually publishes.
///
/// An operator's own `models` entry is honoured verbatim in every vocabulary:
/// they named a specific model and it is not this function's place to rewrite
/// it.
///
/// With no override, the vocabulary decides:
///
/// * [`TierVocabulary::Concrete`] — the endpoint publishes OpenRouter catalog
///   ids and has never heard of `chat-v1`, so the tier is resolved here or the
///   request 400s. Only for the tiers that endpoint's catalog actually
///   publishes: a partial mirror gets substitution where it has been seen to
///   work and the bare tier everywhere else, because sending an id the same
///   catalog omitted is the very guess-against-evidence this module exists to
///   stop (see [`ConcreteTiers`]).
/// * [`TierVocabulary::Tiers`] — the endpoint resolves the tier itself against
///   its own registry, which is exactly what it wants; substituting would bypass
///   its per-tier provider pinning.
/// * [`TierVocabulary::Unknown`] — the tier goes out unchanged. Both answers are
///   wrong at an endpoint that publishes neither vocabulary, but only one of
///   them is *honest*: the provider's 400 then names a string the operator
///   configured and can find, next to advice pointing at `GET
///   {base_url}/models`, instead of an `anthropic/…` id they never typed and
///   cannot locate in their own catalog.
pub fn model_for_tier(
    tier: &str,
    overrides: &BTreeMap<String, String>,
    vocabulary: TierVocabulary,
) -> String {
    if let Some(mapped) = overrides.get(tier) {
        return mapped.clone();
    }
    match vocabulary {
        TierVocabulary::Concrete(published) if published.publishes(tier) => DEFAULT_TIER_MODELS
            .iter()
            .find(|(name, _)| *name == tier)
            .map(|(_, model)| (*model).to_string())
            .unwrap_or_else(|| tier.to_string()),
        TierVocabulary::Concrete(_) | TierVocabulary::Tiers | TierVocabulary::Unknown => {
            tier.to_string()
        }
    }
}

/// Normalizes a provider kind: blank and the legacy `managed` both become
/// [`DEFAULT_PROVIDER`]; anything else passes through for validation to judge.
pub fn normalize_provider(provider: &str) -> &str {
    match provider.trim() {
        "" | LEGACY_MANAGED => DEFAULT_PROVIDER,
        other => other,
    }
}

/// The setup wizard's "TinyHumans" (managed) card, before [`normalize_provider`]
/// folds it into `openrouter`.
///
/// The managed choice must resolve to the platform endpoint and the injected
/// managed credential, never to a `base_url` the operator never typed — the card
/// has no URL field. Once normalized it is indistinguishable from a real
/// `openrouter`, so the managed probe branch keys on the raw kind instead. Only
/// [`decl_for_probe`] passes the raw kind here; [`resolve_effective_scoped`]
/// normalizes first, so runtime resolution of a legacy `managed` blob is
/// unaffected.
pub fn is_managed_choice(provider: &str) -> bool {
    matches!(provider.trim(), LEGACY_MANAGED | "tinyhumans")
}

/// The slug the managed/TinyHumans provider's credential is keyed on.
///
/// `tinyhumans`, not `openrouter`, even though [`normalize_provider`] folds the
/// managed kind onto `openrouter` for endpoint resolution. The two answer
/// different questions: the kind says *what shape of API this is*, the slug says
/// *whose account this is*. Keying the managed credential on `openrouter` would
/// put a TinyHumans key in the slot a real OpenRouter account belongs in, and a
/// company that had both would have one.
///
/// Not to be confused with [`company_key::KEY_KEY`](crate::company::company_key)
/// (`tinyhumans/key`), which is the company's **identity**. This is a slot for a
/// key pasted specifically for inference; that is the account the company signs
/// in as. They are consulted in that order and they are not the same thing.
pub const MANAGED_SLUG: &str = "tinyhumans";

/// Which `provider/<slug>/key` slot a provider kind's credential lives in.
///
/// One rule, used by the resolver and by the store's entry-zero reader, so the
/// address the turn path reads and the address the console writes cannot drift.
pub fn credential_slug(provider_raw: &str) -> &str {
    if is_managed_choice(provider_raw) {
        MANAGED_SLUG
    } else {
        normalize_provider(provider_raw)
    }
}

/// OpenRouter's OpenAI-compatible base URL — used when the `openrouter`
/// provider names no explicit `base_url`.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// A local Ollama server's OpenAI-compatible surface — the convenience default
/// for the `ollama` provider (validation still requires an explicit `base_url`
/// in the manifest; this only backstops an empty resolved value).
pub const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

/// Where an effective inference config came from — drives the console's source
/// badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceSource {
    /// The platform-injected managed default (`env`).
    Default,
    /// Declared in `company.toml`'s `[inference]`.
    Manifest,
    /// Set at runtime through the console.
    Runtime,
}

/// The platform-injected managed default (from `harness_inference_from_env`):
/// the base URL + credential the manager supplies. Passed to
/// [`resolve_effective`] as the lowest-precedence source.
///
/// The credential is a [`Credential`], not a `String`: on the hosted platform it
/// is a projected token that rotates in place, so it is resolved per request
/// rather than captured here.
#[derive(Clone, Debug)]
pub struct EnvDefault {
    /// Managed base URL (env `OPENCOMPANY_INFERENCE_URL` or the default).
    pub base_url: String,
    /// Managed credential — a projected platform token source, or the static
    /// `OPENCOMPANY_INFERENCE_KEY` / `TINYHUMANS_API_KEY` value.
    pub credential: Credential,
}

/// The on-disk runtime inference override stored under [`RUNTIME_CONFIG_KEY`].
/// Carries no credential — the token lives apart under [`KEY_KEY`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeInference {
    /// Provider kind — one of [`INFERENCE_PROVIDERS`].
    pub provider: String,
    /// Optional OpenAI-compatible base URL override.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Abstract-tier → concrete model id.
    #[serde(default)]
    pub models: BTreeMap<String, String>,
}

/// One company's *effective* inference configuration — the highest-precedence
/// of runtime / manifest / env, carrying the credential as a resolvable
/// [`Credential`] rather than a captured value.
///
/// Derives **no** `Serialize` (the key must never cross a wire) and its `Debug`
/// redacts the credential.
#[derive(Clone, Debug)]
pub struct InferenceDecl {
    /// Provider slug — one of [`INFERENCE_PROVIDERS`].
    pub provider: String,
    /// Resolved OpenAI-compatible base URL (never empty for a valid config).
    pub base_url: String,
    /// Abstract-tier → concrete model id. Empty means every tier passes
    /// through to the provider verbatim.
    pub models: BTreeMap<String, String>,
    /// Provenance badge for the console.
    pub source: InferenceSource,
    /// The outbound credential. Private — read only through
    /// [`bearer`](Self::bearer); never serialized.
    credential: Credential,
    /// Whether this rides the platform's subscription proxy. Read through
    /// [`is_proxied`](Self::is_proxied).
    proxied: bool,
    /// The endpoint's model vocabulary, **once discovered from its catalog**.
    ///
    /// `None` means nobody has read `{base_url}/models` for this decl yet, not
    /// that the endpoint speaks nothing — [`vocabulary`](Self::vocabulary)
    /// supplies the pre-discovery guess. Discovery is a network read, so it is
    /// attached by the callers that can afford one and can cache it
    /// (`crate::server::inference_models::discovered_vocabulary`) rather than
    /// performed inside this resolve, which runs on every turn.
    vocabulary: Option<TierVocabulary>,
}

impl InferenceDecl {
    /// The outbound credential, unresolved. Callers on the request path want
    /// [`bearer`](Self::bearer); this is for status and fingerprinting.
    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// The bearer to present on **this** request, or `None` to omit the header
    /// (the keyless Ollama case).
    ///
    /// Resolved per call rather than captured at build time: a hosted tenant's
    /// managed credential is a projected token the platform rotates in place, so
    /// a value captured once would go stale within minutes.
    pub async fn bearer(&self) -> Result<Option<String>> {
        self.credential.current().await
    }

    /// Whether an outbound credential is configured — the non-secret status the
    /// read APIs surface. Never returns the value, and never reads the token.
    pub fn key_configured(&self) -> bool {
        self.credential.configured()
    }

    /// Whether this config rides the platform's subscription proxy rather than
    /// a credential the tenant supplied.
    ///
    /// True only for `openrouter` with no tenant key — the default a company
    /// starts on. It is what separates "the subscription pays" from "the tenant
    /// pays", which is why it is recorded here rather than re-derived from the
    /// base URL by every caller that cares.
    pub fn is_proxied(&self) -> bool {
        self.proxied
    }

    /// The model vocabulary to resolve tiers against for this config.
    ///
    /// The discovered answer when one is attached; otherwise the pre-discovery
    /// guess, which is the rule this code used to apply unconditionally:
    /// proxied endpoints resolve tiers, everything else is assumed to publish
    /// OpenRouter catalog ids. That guess is wrong for a tier-native endpoint a
    /// tenant reaches with its own key, which is precisely why discovery
    /// exists — but keeping it as the fallback means an endpoint whose catalog
    /// cannot be read behaves exactly as it did before, rather than changing
    /// behaviour on a network failure.
    ///
    /// The un-discovered `Concrete` guess assumes **all four** shipped ids —
    /// `ConcreteTiers::all()` — which is exactly what the pre-discovery code
    /// did, and true of OpenRouter itself. Narrowing a tier only ever happens
    /// on evidence from a catalog that was actually read.
    pub fn vocabulary(&self) -> TierVocabulary {
        self.vocabulary.unwrap_or(if self.proxied {
            TierVocabulary::Tiers
        } else {
            TierVocabulary::Concrete(ConcreteTiers::all())
        })
    }

    /// Whether [`vocabulary`](Self::vocabulary) is the endpoint's published
    /// answer rather than the pre-discovery guess. The console needs the
    /// difference: "this provider publishes no mapping we recognise" and "we
    /// could not reach this provider's catalog" are different things to tell an
    /// operator.
    pub fn vocabulary_confirmed(&self) -> bool {
        self.vocabulary.is_some()
    }

    /// Attach a discovered vocabulary (or clear it back to the guess).
    #[must_use]
    pub fn with_vocabulary(mut self, vocabulary: Option<TierVocabulary>) -> Self {
        self.vocabulary = vocabulary;
        self
    }

    /// The stable telemetry slug for this config
    /// (`subscription` / `openrouter` / `byok` / `ollama`).
    ///
    /// Distinguishes proxied from direct OpenRouter, because those are two
    /// different payers and a Usage view that merged them would be telling the
    /// operator nothing.
    pub fn telemetry_slug(&self) -> &'static str {
        if self.proxied {
            return "subscription";
        }
        provider_slug(&self.provider)
    }
}

/// The stable telemetry slug for a provider kind.
///
/// An unknown kind reports `unknown` rather than being folded into a real
/// provider's attribution: [`resolve_effective`] rejects one outright, so
/// reaching here with one means something upstream is wrong, and quietly
/// billing it to a provider that was never called would hide that.
///
/// This answers on the *kind* alone. Proxied OpenRouter slugs as
/// `subscription`, which needs the credential too — see
/// [`InferenceDecl::telemetry_slug`].
pub fn provider_slug(provider: &str) -> &'static str {
    match normalize_provider(provider) {
        "openrouter" => "openrouter",
        "ollama" => "ollama",
        "openai_compatible" => "byok",
        _ => "unknown",
    }
}

/// Resolves the effective `(base_url, credential, proxied)` for a provider.
///
/// **`openrouter` is dual-mode**, and that is the whole shape of the product's
/// first-run story:
///
/// * **No tenant key** — the config inherits the platform endpoint and the
///   platform credential, and the subscription pays. This is where a company
///   starts, with nothing configured and nobody asked for a card.
/// * **A tenant `sk-or-…`** — the config goes direct to OpenRouter on the
///   tenant's own account.
///
/// The inheritance branch is the one `managed` used to own, and it moved here
/// rather than being deleted for the same reason it existed: a config that named
/// a provider but dropped the platform key would 401 rather than fall back.
///
/// The one exception is a keyless `openrouter` that also sets its own
/// `base_url`: that endpoint is not the platform's, so the platform credential
/// is withheld and the config goes direct (keyless) instead — sending the
/// platform token to an arbitrary override would leak it.
///
/// Every other kind uses its own configured base URL and key verbatim — those
/// are third-party endpoints we hold no credential for.
fn resolve_endpoint(
    provider: &str,
    base_url_override: Option<&str>,
    key: String,
    env_default: Option<&EnvDefault>,
) -> (String, Credential, bool) {
    let base_url_override = base_url_override.map(str::trim).filter(|s| !s.is_empty());
    let has_key = !key.trim().is_empty();

    if is_managed_choice(provider) {
        // The managed card carries no endpoint field, so a base URL left in the
        // form by a previously-picked provider is stale, not a chosen endpoint:
        // it never redirects the managed probe. The endpoint is always the
        // platform's, and the credential is the operator's own key when given,
        // else the injected managed one.
        let base_url = env_default
            .map(|e| e.base_url.clone())
            .unwrap_or_else(|| PLATFORM_BASE_URL.to_string());
        let credential = if has_key {
            Credential::from_value(key)
        } else {
            env_default
                .map(|e| e.credential.clone())
                .unwrap_or(Credential::None)
        };
        return (base_url, credential, true);
    }

    if normalize_provider(provider) == "openrouter" && !has_key {
        // The platform credential rides only the platform's own endpoint. A
        // tenant-supplied base URL override with no key is a direct (keyless)
        // config, not the inheritance branch: pairing the platform token with
        // an arbitrary endpoint would leak it to wherever the override points.
        if let Some(base_url) = base_url_override {
            return (base_url.to_string(), Credential::None, false);
        }
        let base_url = env_default
            .map(|e| e.base_url.clone())
            .unwrap_or_else(|| PLATFORM_BASE_URL.to_string());
        let credential = env_default
            .map(|e| e.credential.clone())
            .unwrap_or(Credential::None);
        return (base_url, credential, true);
    }

    (
        effective_base_url(provider, base_url_override),
        Credential::from_value(key),
        false,
    )
}

/// A declaration for the **first-run** connection test, before any company
/// exists to resolve one from.
///
/// [`resolve_effective`] cannot serve the wizard: it reads a company's secret
/// store and manifest, and during first-run setup there is neither. This builds
/// the same shape from what the operator has just typed, through the *same*
/// [`resolve_endpoint`] — so a probe defaults its URL and treats a blank key
/// exactly as the running system will, rather than by a second set of rules that
/// agree today and drift later. A test that passes under different rules than
/// the runtime uses is worse than no test.
///
/// `env_default` is what the host already has. Passing it is what makes "leave
/// the key blank and press Test" mean *test the credential this host was given*
/// — the hosted case, where the operator has no key of their own and the control
/// plane injected one. A blank key with no env default resolves to
/// [`Credential::None`]: correct for keyless Ollama, and honestly
/// unauthenticated everywhere else, which is what the probe should then report.
///
/// [`InferenceSource::Runtime`] because that is what this is — a value an
/// operator supplied, with nothing persisted anywhere yet.
pub fn decl_for_probe(
    provider: &str,
    base_url: Option<&str>,
    key: Option<&str>,
    env_default: Option<&EnvDefault>,
) -> InferenceDecl {
    let provider = provider.trim().to_string();
    let (base_url, credential, proxied) = resolve_endpoint(
        &provider,
        base_url,
        key.unwrap_or_default().trim().to_string(),
        env_default,
    );
    InferenceDecl {
        provider,
        base_url,
        models: BTreeMap::new(),
        source: InferenceSource::Runtime,
        credential,
        proxied,
        vocabulary: None,
    }
}

/// The effective base URL for a provider kind, given an optional override.
///
/// This is the **direct** endpoint for each kind. Proxied `openrouter` does not
/// come through here — it inherits the platform endpoint in
/// [`resolve_endpoint`], which is the only place that distinction is made.
///
/// `ollama` backstops to a local default; `openai_compatible` has no default
/// (validation requires an explicit URL).
pub fn effective_base_url(provider: &str, override_url: Option<&str>) -> String {
    let override_url = override_url.map(str::trim).filter(|s| !s.is_empty());
    match normalize_provider(provider) {
        "ollama" => override_url.unwrap_or(OLLAMA_DEFAULT_BASE_URL).to_string(),
        "openai_compatible" => override_url.unwrap_or_default().to_string(),
        // openrouter, and any unknown kind (which `resolve_effective` rejects).
        _ => override_url.unwrap_or(OPENROUTER_BASE_URL).to_string(),
    }
}

/// Normalizes an endpoint typed by a person during setup.
///
/// Local model applications commonly advertise themselves as `localhost:1234`
/// even though the OpenAI-compatible client needs
/// `http://localhost:1234/v1`. Accept that familiar spelling while preserving
/// explicit schemes and non-root paths.
pub fn normalize_setup_base_url(provider: &str, raw: Option<&str>) -> Option<String> {
    let raw = raw.map(str::trim).filter(|value| !value.is_empty())?;
    if !matches!(normalize_provider(provider), "ollama" | "openai_compatible") {
        return Some(raw.trim_end_matches('/').to_string());
    }

    let mut url = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    url = url.trim_end_matches('/').to_string();
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    if !after_scheme.contains('/') {
        url.push_str("/v1");
    }
    Some(url)
}

/// Loads the runtime inference override, or `None` when unset/blank. A malformed
/// blob is a store error (surfaced, not silently dropped).
pub async fn load_runtime_config(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<Option<RuntimeInference>> {
    load_runtime_config_scoped(company, secrets, &HarnessScope::default()).await
}

/// [`load_runtime_config`] for one harness's own slot.
pub async fn load_runtime_config_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    scope: &HarnessScope,
) -> Result<Option<RuntimeInference>> {
    let Some(SecretValue(raw)) = secrets.get(company, &scope.config_key()).await? else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let config: RuntimeInference = serde_json::from_str(&raw).map_err(|e| {
        OpenCompanyError::Store(format!("inference runtime config is not valid JSON: {e}"))
    })?;
    if config.provider.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(config))
}

/// Persists the runtime inference override (console `PUT`).
pub async fn save_runtime_config(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    config: &RuntimeInference,
) -> Result<()> {
    save_runtime_config_scoped(company, secrets, config, &HarnessScope::default()).await
}

/// [`save_runtime_config`] for one harness's own slot.
pub async fn save_runtime_config_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    config: &RuntimeInference,
    scope: &HarnessScope,
) -> Result<()> {
    let raw = serde_json::to_string(config)
        .map_err(|e| OpenCompanyError::Store(format!("serializing inference config: {e}")))?;
    secrets
        .set(company, &scope.config_key(), SecretValue(raw))
        .await
}

/// Clears the runtime inference override (console `DELETE` → revert to
/// manifest/managed). Best-effort — the store has no delete, so an empty value
/// reads back as unset.
pub async fn clear_runtime_config(company: &CompanyId, secrets: &dyn SecretStore) -> Result<()> {
    clear_runtime_config_scoped(company, secrets, &HarnessScope::default()).await
}

/// [`clear_runtime_config`] for one harness's own slot.
pub async fn clear_runtime_config_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    scope: &HarnessScope,
) -> Result<()> {
    secrets
        .set(company, &scope.config_key(), SecretValue(String::new()))
        .await
}

/// Reads the effective outbound credential.
///
/// The canonical [`KEY_KEY`] (`inference/key`) is tried first — the console
/// writes rotated tokens there. When it is empty/missing, `override_key` (a
/// manifest section's `api_key_secret`) is the fallback for a commit-time key.
/// Returns an empty string when neither holds a value.
pub async fn load_key(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    override_key: Option<&str>,
) -> Result<String> {
    load_key_scoped(company, secrets, override_key, &HarnessScope::default()).await
}

/// [`load_key`] for one harness's own credential slot.
pub async fn load_key_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    override_key: Option<&str>,
    scope: &HarnessScope,
) -> Result<String> {
    if let Some(SecretValue(raw)) = secrets.get(company, &scope.key_key()).await?
        && !raw.trim().is_empty()
    {
        return Ok(raw);
    }
    if let Some(key) = override_key.map(str::trim).filter(|s| !s.is_empty())
        && let Some(SecretValue(raw)) = secrets.get(company, key).await?
        && !raw.trim().is_empty()
    {
        return Ok(raw);
    }
    Ok(String::new())
}

/// Reads the outbound inference credential for one provider slug.
///
/// ```text
///   1. provider/<slug>/key    the address every provider's credential lives at
///   2. inference/key          the legacy flat slot, read-only
///   3. <manifest secret>      a commit-time key named by `[inference].api_key_secret`
/// ```
///
/// Steps 1 and 2 are **the same meaning at two addresses**. Nothing writes step 2
/// any more ([`store_provider_key`](super::inference::store::store_provider_key)
/// clears it on the next save of that provider), so the fallback retires itself
/// company by company and can be deleted outright once nothing reads it. That is
/// lazy convergence rather than a migration: no flag day, and no half-migrated
/// state on a store with no transaction.
pub async fn load_inference_key_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
    override_key: Option<&str>,
    scope: &HarnessScope,
) -> Result<String> {
    if let Some(SecretValue(raw)) = secrets.get(company, &provider_key_key(slug)).await?
        && !raw.trim().is_empty()
    {
        return Ok(raw);
    }
    load_key_scoped(company, secrets, override_key, scope).await
}

/// Which step of the managed chain a request would actually resolve at.
///
/// The managed row on the console has to say this, and it has to say it
/// honestly. The design this is ported from renders a permanent `Always on`
/// badge, which is true **there** — they run the managed backend — and is a lie
/// here: our managed tier needs a credential and can resolve to nothing. A row
/// claiming availability while agents cannot think is the failure
/// `CognitionState`'s five states exist to prevent.
///
/// Steps 3 and 4 are kept apart because they answer different questions for the
/// operator: one bills the company's own account, the other bills whoever runs
/// the server. Collapsing them into "on" hides the decision they would make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedSource {
    /// A key pasted for inference — `provider/tinyhumans/key`, or the legacy
    /// `inference/key`. These are two addresses for one meaning.
    ProviderKey,
    /// The company's own TinyHumans account.
    CompanyAccount,
    /// This instance's identity — so the server's account pays.
    Instance,
    /// Nothing resolves. The managed brain is **not set up**.
    None,
}

impl ManagedSource {
    /// The stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderKey => "provider_key",
            Self::CompanyAccount => "company_account",
            Self::Instance => "instance",
            Self::None => "none",
        }
    }

    /// Whether the managed brain can be reached at all.
    pub fn resolves(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// [`ManagedSource`] from the three facts that decide it.
///
/// Pure, because it is a decision with four branches and every one of them is a
/// different sentence on screen. The inputs are read where a store is available;
/// the reasoning is here, where it can be tested with three booleans.
pub fn managed_source(
    inference_key_set: bool,
    company_account: &Credential,
    env_default: Option<&EnvDefault>,
) -> ManagedSource {
    if inference_key_set {
        return ManagedSource::ProviderKey;
    }
    if matches!(company_account, Credential::Company(_)) {
        return ManagedSource::CompanyAccount;
    }
    match env_default {
        // `configured()` rather than presence: a projected-token source reports
        // itself configured while its file can still yield nothing, and what
        // decides availability is whether a value would reach the wire.
        Some(env) if env.credential.configured() => ManagedSource::Instance,
        _ => ManagedSource::None,
    }
}

/// Steps 3 and 4 of the managed chain: the company's account identity, then this
/// instance's.
///
/// **An identity flows to a surface only when the vendor at the other end is the
/// identity's own vendor.** That is the whole safety property, and `proxied` is
/// what enforces it: it is true exactly when the resolved endpoint is the
/// platform's own, and false for OpenRouter, Anthropic, a custom endpoint or any
/// other vendor. A `th_…` key presented as a bearer to `openrouter.ai` is a live
/// bug in the credential-link path today, and this is the line that stops it
/// being reproduced here.
///
/// `had_key` is the second gate: a key pasted for inference is a more specific
/// answer than an identity, so it wins and this is not consulted at all.
///
/// A store read error **propagates**. An unreadable store means we do not know
/// who this company is, and resolving that to the instance's identity would bill
/// the company's thinking to the server's account, invisibly.
async fn managed_identity(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    resolved: Credential,
    proxied: bool,
    had_key: bool,
) -> Result<Credential> {
    if !proxied || had_key {
        return Ok(resolved);
    }
    Ok(
        match crate::company::company_key::load(company, secrets).await? {
            // The company's own TinyHumans account. Setting it used to move only
            // the app connections and leave every agent turn on whoever runs the
            // server — the expensive half, with nothing on screen saying so.
            company_key @ Credential::Company(_) => company_key,
            // Nothing of the company's own: the instance identity that
            // `resolve_endpoint` already put here, or nothing at all.
            _ => resolved,
        },
    )
}

/// Writes the company's outbound inference credential (write-only intake).
pub async fn store_key(company: &CompanyId, secrets: &dyn SecretStore, key: &str) -> Result<()> {
    store_key_scoped(company, secrets, key, &HarnessScope::default()).await
}

/// [`store_key`] for one harness's own credential slot.
pub async fn store_key_scoped(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    key: &str,
    scope: &HarnessScope,
) -> Result<()> {
    secrets
        .set(company, &scope.key_key(), SecretValue(key.to_string()))
        .await
}

/// Clears the stored credential (best-effort — the store has no delete, so an
/// empty value reads back as "not configured").
pub async fn clear_key(company: &CompanyId, secrets: &dyn SecretStore) -> Result<()> {
    secrets
        .set(company, KEY_KEY, SecretValue(String::new()))
        .await
}

/// Whether the company currently has an outbound inference credential — the
/// non-secret status surfaced by the read APIs. Never returns the value.
pub async fn key_configured(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    override_key: Option<&str>,
) -> Result<bool> {
    Ok(!load_key(company, secrets, override_key)
        .await?
        .trim()
        .is_empty())
}

/// Resolves a company's *effective* inference configuration.
///
/// Precedence is **runtime > manifest > env-default**. Returns `None` when no
/// source configures inference at all — the caller then keeps the managed/echo
/// brain. The single seam the harness builder and the ops route both use so the
/// agent-facing resolution and the console's status view stay identical.
///
/// This re-reads the secret store on every call, which is what makes a console
/// switch take effect on the agents' next turn with no rebuild.
pub async fn resolve_effective(
    company: &CompanyId,
    manifest: &Inference,
    env_default: Option<&EnvDefault>,
    secrets: &dyn SecretStore,
) -> Result<Option<InferenceDecl>> {
    resolve_effective_scoped(
        company,
        manifest,
        env_default,
        secrets,
        &HarnessScope::default(),
    )
    .await
}

/// [`resolve_effective`] against one harness's own config and credential slots.
///
/// `manifest` is that harness's `[harness.inference]`, or the company-level
/// `[inference]` when it declares none — the caller picks, because only it knows
/// which fallback applies.
///
/// The precedence within a harness is unchanged (runtime > manifest > env
/// default); what differs is only *which* secret keys the runtime and credential
/// tiers read. Two `built_in` harnesses therefore resolve independently, which
/// is what lets one run on the subscription while the other runs on a key.
pub async fn resolve_effective_scoped(
    company: &CompanyId,
    manifest: &Inference,
    env_default: Option<&EnvDefault>,
    secrets: &dyn SecretStore,
    scope: &HarnessScope,
) -> Result<Option<InferenceDecl>> {
    // 0. The provider list — what the console's Connected rows actually hold.
    //
    // **This is the seam the whole feature hung off and nobody connected.** The
    // write routes populated `inference/providers`, the status route rendered
    // it, and the resolver began at `inference/config` — so a company that added
    // a provider through the console had configured its *display*, not itself,
    // and the chat pane's "no model configured" was telling the truth.
    //
    // Entry zero is why this sits ABOVE the legacy read rather than replacing
    // it: `inference/config` is the first element of this list, so a company
    // that predates the list resolves through the same branch it always did —
    // which is exactly what entry zero was designed to make true without a
    // migration. The `EntryZero` arm below therefore falls through to step 1
    // deliberately, so the legacy path keeps every rule it has (the proxy
    // inheritance, the managed chain, `reject_unknown_provider`) rather than a
    // reimplementation of them here.
    let providers = store::list_providers(company, secrets).await?;
    if let Some(decl) = decl_for_primary(company, secrets, &providers).await? {
        return Ok(Some(decl));
    }

    resolve_legacy_scoped(company, manifest, env_default, secrets, scope).await
}

/// The declaration the company's **primary** provider resolves to, if the list
/// settles the question at all.
///
/// `None` means it does not, and the caller falls through to
/// [`resolve_legacy_scoped`]: either the list is empty, or its primary is entry
/// zero — which is the legacy blob wearing a provider record's clothes and has
/// to resolve through the chain that owns it.
///
/// Takes the list rather than reading it, because both callers already hold one
/// and a second read per turn buys nothing but a round trip.
async fn decl_for_primary(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    providers: &[store::Provider],
) -> Result<Option<InferenceDecl>> {
    let marked = store::load_default_slug(company, secrets).await?;
    match resolve::primary(providers, marked.as_deref()) {
        Some(provider) if provider.origin == store::ProviderOrigin::Indexed => {
            Ok(Some(decl_for_indexed(company, secrets, provider).await?))
        }
        _ => Ok(None),
    }
}

/// The declaration one **indexed** provider record resolves to.
///
/// Extracted because two callers need it and must not drift: the unrouted path
/// above, which reaches it through the primary, and a routing row that names
/// this provider by slug. A second copy of these six lines is a second opinion
/// about which credential an added provider presents.
async fn decl_for_indexed(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    provider: &store::Provider,
) -> Result<InferenceDecl> {
    let key = store::load_provider_key(company, secrets, provider).await?;
    let had_key = !key.trim().is_empty();
    // A provider the operator added names its own endpoint. It is a vendor
    // account, never the platform proxy, so `proxied` is false — and that is
    // what denies it both the instance identity and the company's, which is the
    // safety property the credential chain is built on.
    let credential = Credential::from_value(key);
    let proxied = is_managed_choice(&provider.kind);
    let credential = managed_identity(company, secrets, credential, proxied, had_key).await?;
    Ok(InferenceDecl {
        provider: normalize_provider(&provider.kind).to_string(),
        base_url: provider.base_url.clone(),
        models: provider.models.clone(),
        source: InferenceSource::Runtime,
        credential,
        proxied,
        vocabulary: None,
    })
}

/// What a company resolves to **before** the provider list existed: the runtime
/// blob, then the manifest, then the platform default.
///
/// Split out of [`resolve_effective_scoped`] rather than inlined because a
/// routing row naming **entry zero** has to reach exactly this chain — entry
/// zero *is* the legacy blob wearing a provider record's clothes, so resolving
/// it through the record would drop the proxy inheritance, the managed chain and
/// `reject_unknown_provider` that only live here.
async fn resolve_legacy_scoped(
    company: &CompanyId,
    manifest: &Inference,
    env_default: Option<&EnvDefault>,
    secrets: &dyn SecretStore,
    scope: &HarnessScope,
) -> Result<Option<InferenceDecl>> {
    // 1. Runtime override (console) wins.
    if let Some(runtime) = load_runtime_config_scoped(company, secrets, scope).await? {
        let provider = normalize_provider(&runtime.provider).to_string();
        reject_unknown_provider(&provider, "the stored runtime inference config")?;
        let key = load_inference_key_scoped(
            company,
            secrets,
            credential_slug(&runtime.provider),
            None,
            scope,
        )
        .await?;
        let had_key = !key.trim().is_empty();
        // The **raw** kind, not the normalized one. `normalize_provider` folds
        // `managed` onto `openrouter`, and resolving through the normalized
        // value skipped both managed branches — so a company that declared
        // `managed` and stored a key had its requests sent to `openrouter.ai`
        // carrying a TinyHumans token. `resolve_endpoint` consults
        // `is_managed_choice` first and needs the word the operator chose.
        let (base_url, credential, proxied) = resolve_endpoint(
            &runtime.provider,
            runtime.base_url.as_deref(),
            key,
            env_default,
        );
        let credential = managed_identity(company, secrets, credential, proxied, had_key).await?;
        return Ok(Some(InferenceDecl {
            provider,
            base_url,
            models: runtime.models,
            source: InferenceSource::Runtime,
            credential,
            proxied,
            vocabulary: None,
        }));
    }

    // 2. Manifest `[inference]`.
    if manifest.is_set() {
        let provider =
            normalize_provider(manifest.provider.as_deref().unwrap_or_default()).to_string();
        reject_unknown_provider(&provider, "`[inference].provider`")?;
        let raw = manifest.provider.as_deref().unwrap_or_default();
        let key = load_inference_key_scoped(
            company,
            secrets,
            credential_slug(raw),
            manifest.api_key_secret.as_deref(),
            scope,
        )
        .await?;
        let had_key = !key.trim().is_empty();
        // The **normalized** kind here, unlike the runtime branch above, and the
        // difference is who wrote the value. A runtime blob comes from the
        // console, whose managed card has no URL field — so a `base_url` beside
        // `managed` there is a stale value a previously-picked provider left in
        // the form, and honouring it would send the managed probe somewhere the
        // operator never chose. A manifest is hand-authored and committed:
        // `provider = "managed"` with a `base_url` is a sentence somebody typed
        // on purpose, usually a gateway in front of the platform, and silently
        // redirecting it to the platform endpoint would be the same disregard in
        // the opposite direction.
        //
        // The credential chain is unaffected either way: an explicit endpoint
        // resolves `proxied = false`, which is exactly what denies it both the
        // platform credential and the company identity. A gateway is a vendor.
        let (base_url, credential, proxied) =
            resolve_endpoint(&provider, manifest.base_url.as_deref(), key, env_default);
        let credential = managed_identity(company, secrets, credential, proxied, had_key).await?;
        return Ok(Some(InferenceDecl {
            provider,
            base_url,
            models: manifest.models.clone(),
            source: InferenceSource::Manifest,
            credential,
            proxied,
            vocabulary: None,
        }));
    }

    // 3. The platform-injected default: OpenRouter, proxied on the subscription.
    //    A company that has configured nothing lands here, which is why it must
    //    be a working config and not a prompt for a credential.
    //
    //    It still runs through `resolve_endpoint` rather than assuming proxied,
    //    because a console-set key is a configuration act even when the operator
    //    never named a provider: the console's key field on a fresh company
    //    writes `inference/key` and nothing else. Assuming proxied here would
    //    take that key, store it, report it as configured — and then never send
    //    it anywhere.
    if let Some(env) = env_default {
        let key =
            load_inference_key_scoped(company, secrets, DEFAULT_PROVIDER, None, scope).await?;
        let had_key = !key.trim().is_empty();
        let (base_url, credential, proxied) =
            resolve_endpoint(DEFAULT_PROVIDER, None, key, Some(env));
        // A company that has configured nothing still lands on the platform's
        // own endpoint, so its account key is the right credential for it — and
        // this is the case where the silent billing split hurt most: an operator
        // set a company key, watched Composio move onto their account, and left
        // every agent turn on the server's.
        let credential = managed_identity(company, secrets, credential, proxied, had_key).await?;
        return Ok(Some(InferenceDecl {
            provider: DEFAULT_PROVIDER.to_string(),
            base_url,
            models: BTreeMap::new(),
            source: InferenceSource::Default,
            credential,
            proxied,
            vocabulary: None,
        }));
    }

    Ok(None)
}

/// [`resolve_effective_scoped`] for the workload one turn is actually for.
///
/// **The routing table's only caller on the turn path.** Without it the Routing
/// tab is a screen that persists choices and changes nothing: every row wrote
/// `inference/routes`, the status route read it back, and the turn resolved
/// through the primary regardless — so the value stuck across a reload while the
/// turn kept reaching the provider and the model the operator had just moved off.
/// A control that visibly fails is a bug; one that reports success and is inert
/// is worse, because nothing about it looks wrong.
///
/// `tier` is the abstract tier the turn carries (`chat-v1`, …). A tier with no
/// row of its own — anything outside
/// [`ROUTABLE_WORKLOADS`](resolve::ROUTABLE_WORKLOADS) — resolves exactly as it
/// did before routes existed, rather than acquiring a route by accident or
/// failing closed for want of one.
///
/// ## Why a route fails closed and an unset row does not
///
/// [`Resolution::Missing`](resolve::Resolution::Missing) and
/// [`Disabled`](resolve::Resolution::Disabled) become errors here. An unset
/// workload falls back to the primary because nobody chose anything for it; a
/// route is a choice with a workload attached, and silently spending it on a
/// different account is the failure the explicit default marker exists to
/// prevent, wearing a different hat.
pub async fn resolve_effective_for_tier(
    company: &CompanyId,
    manifest: &Inference,
    env_default: Option<&EnvDefault>,
    secrets: &dyn SecretStore,
    scope: &HarnessScope,
    tier: &str,
) -> Result<Option<InferenceDecl>> {
    let Some(workload) = resolve::Workload::from_tier(tier) else {
        return resolve_effective_scoped(company, manifest, env_default, secrets, scope).await;
    };
    let providers = store::list_providers(company, secrets).await?;
    let routes = store::load_routes(company, secrets).await?;

    match resolve::provider_for_workload(workload, &routes, &providers) {
        // Unset. The whole existing chain, unchanged — which is what keeps a
        // company that has never opened the Routing tab resolving exactly where
        // it always did.
        resolve::Resolution::Primary => {
            match decl_for_primary(company, secrets, &providers).await? {
                Some(decl) => Ok(Some(decl)),
                None => resolve_legacy_scoped(company, manifest, env_default, secrets, scope).await,
            }
        }
        // `managed` is a word in the route grammar, not a provider slug — it is
        // what the Managed mode button writes into every row. Read as a slug it
        // names nothing and the workload would fail closed against a provider
        // the operator never had.
        resolve::Resolution::Managed => Ok(Some(
            managed_decl(company, secrets, env_default, scope).await?,
        )),
        resolve::Resolution::Missing { workload, slug } => Err(OpenCompanyError::Config(format!(
            "the {} workload is routed to `{slug}`, which this company does not have. \
             Point it somewhere else in Settings → Inference → Routing.",
            workload.as_str()
        ))),
        resolve::Resolution::Disabled { workload, slug } => Err(OpenCompanyError::Config(format!(
            "the {} workload is routed to `{slug}`, which is switched off. \
             Switch it back on, or point that workload somewhere else in \
             Settings → Inference → Routing.",
            workload.as_str()
        ))),
        resolve::Resolution::Resolved { provider, model } => {
            let mut decl = match provider.origin {
                store::ProviderOrigin::Indexed => {
                    decl_for_indexed(company, secrets, provider).await?
                }
                // See [`resolve_legacy_scoped`]: entry zero is the legacy blob,
                // and resolving it through the synthesized record would drop the
                // rules only that chain holds.
                store::ProviderOrigin::EntryZero => {
                    match resolve_legacy_scoped(company, manifest, env_default, secrets, scope)
                        .await?
                    {
                        Some(decl) => decl,
                        None => return Ok(None),
                    }
                }
            };
            if let Some(model) = model {
                // The route's pinned model beats the provider's own tier map,
                // and deliberately: the map is that provider's default for every
                // workload, the route is this workload's choice. `model_for_tier`
                // reads `models` first, so writing it here is the whole of it.
                decl.models.insert(tier.to_string(), model);
            }
            Ok(Some(decl))
        }
    }
}

/// The declaration a route naming `managed` resolves to.
///
/// The platform endpoint and the managed credential chain — the company's own
/// TinyHumans key when it has one, else the instance identity — reached through
/// the same [`resolve_endpoint`] and [`managed_identity`] every other managed
/// path uses, rather than by assembling the endpoint here.
async fn managed_decl(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    env_default: Option<&EnvDefault>,
    scope: &HarnessScope,
) -> Result<InferenceDecl> {
    let key = load_inference_key_scoped(
        company,
        secrets,
        credential_slug(LEGACY_MANAGED),
        None,
        scope,
    )
    .await?;
    let had_key = !key.trim().is_empty();
    let (base_url, credential, proxied) = resolve_endpoint(LEGACY_MANAGED, None, key, env_default);
    let credential = managed_identity(company, secrets, credential, proxied, had_key).await?;
    Ok(InferenceDecl {
        provider: normalize_provider(LEGACY_MANAGED).to_string(),
        base_url,
        models: BTreeMap::new(),
        source: InferenceSource::Runtime,
        credential,
        proxied,
        vocabulary: None,
    })
}

/// Fails a provider kind that is not in [`INFERENCE_PROVIDERS`].
///
/// The manifest validator already rejects one, but a **stored runtime blob**
/// never passes through it: the console wrote it, possibly under an older build
/// whose vocabulary differed. Resolving one silently would attribute its spend to
/// whatever the fallback happened to be, so it fails loudly here instead — the
/// one place both sources converge.
fn reject_unknown_provider(provider: &str, whence: &str) -> Result<()> {
    if crate::company::types::INFERENCE_PROVIDERS.contains(&provider) {
        return Ok(());
    }
    Err(OpenCompanyError::Config(format!(
        "{whence} names an unknown inference provider `{provider}` — expected one of {}.",
        crate::company::types::INFERENCE_PROVIDERS.join(", ")
    )))
}

/// Validates the manifest `[inference]` section, returning every problem in
/// prosumer language. An absent section (`provider = None`) is inert. Shared by
/// manifest validation and the ops `PUT` route (via [`validate_runtime`]).
pub fn validate_inference(inference: &Inference) -> Vec<String> {
    let Some(provider_raw) = inference.provider.as_deref() else {
        return Vec::new();
    };
    let provider = provider_raw.trim();
    if provider.is_empty() {
        return Vec::new();
    }
    validate_parts(
        provider,
        inference.base_url.as_deref(),
        inference.api_key_secret.as_deref(),
    )
}

/// Validates a runtime override (console `PUT`) — same rules as the manifest,
/// but a runtime override never names a secret key (the console writes the
/// canonical `inference/key`), so `api_key_secret` is not part of the shape.
pub fn validate_runtime(config: &RuntimeInference) -> Vec<String> {
    validate_parts(config.provider.trim(), config.base_url.as_deref(), None)
}

/// The shared validation rules for an inference declaration.
fn validate_parts(
    provider: &str,
    base_url: Option<&str>,
    api_key_secret: Option<&str>,
) -> Vec<String> {
    let mut problems = Vec::new();

    // `managed` aliases rather than failing. It named a real thing until
    // OpenCompany stopped exposing its own SKUs, and a committed manifest that
    // still says it means "the platform's brain" — which is now proxied
    // OpenRouter. Rejecting it would break bundles that were valid when written,
    // to no purpose: the intent still resolves.
    let provider = normalize_provider(provider);

    if !INFERENCE_PROVIDERS.contains(&provider) {
        problems.push(format!(
            "`[inference].provider` must be one of {} — you wrote `{provider}`.",
            INFERENCE_PROVIDERS.join(", ")
        ));
    }

    let base_url = base_url.map(str::trim).filter(|s| !s.is_empty());
    // Every echo of the typed URL below is redacted. A `base_url` is quoted back
    // in a rejection the console renders, and a rejection is the one moment a
    // malformed URL — the kind most likely to have been typed by hand with a
    // password in it — is guaranteed to be shown to somebody.
    match provider {
        "ollama" | "openai_compatible" => match base_url {
            None => problems.push(format!(
                "`[inference].base_url` is required for provider `{provider}` — give the OpenAI-compatible endpoint URL."
            )),
            Some(url) if !is_http_url(url) => problems.push(format!(
                "`[inference].base_url` must be an `http://` or `https://` URL — you wrote `{}`.",
                catalogue::redact_endpoint(url)
            )),
            _ => {}
        },
        _ => {
            if let Some(url) = base_url
                && !is_http_url(url)
            {
                problems.push(format!(
                    "`[inference].base_url` must be an `http://` or `https://` URL — you wrote `{}`.",
                    catalogue::redact_endpoint(url)
                ));
            }
        }
    }

    // A credential in the endpoint, refused for the same reason
    // `api_key_secret` refuses a pasted token just below: a `base_url` is stored
    // as written, returned to every console reader on the company status read,
    // and interpolated into operator-facing failure text. The console's own
    // endpoint fields refuse this before anything is written
    // (`catalogue::normalize_local_endpoint`); this is the manifest and
    // console-`PUT` half of the same rule, so the two ways to set an endpoint
    // cannot disagree about it.
    if let Some(url) = base_url
        && catalogue::endpoint_has_credentials(url)
    {
        problems.push(format!(
            "`[inference].base_url` carries a username or password in the URL — you wrote `{}`. Remove them and store the credential in the key slot instead; an endpoint is readable by everyone who can see this company's settings.",
            catalogue::redact_endpoint(url)
        ));
    }

    // The credential must be a *key name*, not the token itself. Reject values
    // that look like a pasted credential so a secret never lands in the manifest.
    if let Some(secret) = api_key_secret.map(str::trim).filter(|s| !s.is_empty())
        && looks_like_inline_credential(secret)
    {
        problems.push(
            "`[inference].api_key_secret` names a secret-store key, not the secret itself — you appear to have pasted a credential. Set the token through the console instead.".to_string(),
        );
    }

    problems
}

/// True when `url` is an absolute `http://` or `https://` URL.
fn is_http_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Heuristic: does this string look like a pasted credential rather than a
/// secret-store *key name*? Catches the common provider token prefixes and any
/// long, opaque, single-token value.
fn looks_like_inline_credential(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "sk-", "sk_", "pk-", "pk_", "rk-", "or-v1-", "xai-", "gsk_", "bearer ",
    ];
    if PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    // A long, opaque token with no path separator or whitespace — key names are
    // short and structured (`inference/openrouter`), tokens are long and dense.
    value.len() >= 40 && !value.contains('/') && !value.contains(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;

    /// The resolved bearer for a decl, for the assertions below.
    async fn bearer(decl: &InferenceDecl) -> Option<String> {
        decl.bearer().await.expect("credential resolves")
    }

    fn inference(provider: &str) -> Inference {
        Inference {
            provider: Some(provider.to_string()),
            base_url: None,
            api_key_secret: None,
            models: BTreeMap::new(),
        }
    }

    #[derive(Default)]
    struct MemSecrets {
        map: Mutex<HashMap<String, String>>,
    }

    #[async_trait]
    impl SecretStore for MemSecrets {
        async fn get(&self, _c: &CompanyId, key: &str) -> Result<Option<SecretValue>> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(key)
                .map(|v| SecretValue(v.clone())))
        }
        async fn set(&self, _c: &CompanyId, key: &str, value: SecretValue) -> Result<()> {
            self.map.lock().unwrap().insert(key.to_string(), value.0);
            Ok(())
        }
    }

    // ---- precedence matrix -------------------------------------------------

    #[tokio::test]
    async fn runtime_beats_manifest_beats_env() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/v1".into(),
            credential: Credential::from_value("env-key"),
        };
        let mut manifest = inference("openai_compatible");
        manifest.base_url = Some("https://manifest.example/v1".into());

        // Env only.
        let decl = resolve_effective(&company, &Inference::default(), Some(&env), &secrets)
            .await
            .unwrap()
            .expect("env default resolves");
        assert_eq!(decl.source, InferenceSource::Default);
        assert_eq!(decl.provider, DEFAULT_PROVIDER);
        assert!(decl.is_proxied(), "the default rides the subscription");
        assert_eq!(decl.telemetry_slug(), "subscription");
        assert_eq!(bearer(&decl).await.as_deref(), Some("env-key"));

        // Manifest beats env.
        let decl = resolve_effective(&company, &manifest, Some(&env), &secrets)
            .await
            .unwrap()
            .expect("manifest resolves");
        assert_eq!(decl.source, InferenceSource::Manifest);
        assert_eq!(decl.provider, "openai_compatible");
        assert_eq!(decl.base_url, "https://manifest.example/v1");

        // Runtime beats manifest.
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "openrouter".into(),
                base_url: None,
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        store_key(&company, &secrets, "or-secret").await.unwrap();
        let decl = resolve_effective(&company, &manifest, Some(&env), &secrets)
            .await
            .unwrap()
            .expect("runtime resolves");
        assert_eq!(decl.source, InferenceSource::Runtime);
        assert_eq!(decl.provider, "openrouter");
        assert_eq!(decl.base_url, OPENROUTER_BASE_URL);
        assert!(!decl.is_proxied(), "a tenant key goes direct");
        assert_eq!(decl.telemetry_slug(), "openrouter");
        assert_eq!(bearer(&decl).await.as_deref(), Some("or-secret"));
        assert!(decl.key_configured());
    }

    /// A keyless `openrouter` inherits the platform endpoint and credential
    /// rather than dropping them — the branch `managed` used to own. Without it a
    /// company that names its provider but holds no key of its own would 401
    /// instead of riding the subscription.
    #[tokio::test]
    async fn keyless_openrouter_inherits_the_platform_endpoint_and_credential() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        let decl = resolve_effective(&company, &inference("openrouter"), Some(&env), &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.source, InferenceSource::Manifest);
        assert_eq!(decl.provider, "openrouter");
        assert_eq!(decl.base_url, "https://env.example/openai/v1");
        assert!(decl.is_proxied());
        assert_eq!(bearer(&decl).await.as_deref(), Some("platform-key"));
    }

    /// A keyless `openrouter` with a tenant-supplied `base_url` override goes
    /// direct with **no** credential — the platform token must not ride an
    /// arbitrary endpoint the operator pointed it at.
    #[tokio::test]
    async fn keyless_openrouter_never_sends_the_platform_credential_to_an_override() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        let mut manifest = inference("openrouter");
        manifest.base_url = Some("https://attacker.example/v1".into());
        let decl = resolve_effective(&company, &manifest, Some(&env), &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.base_url, "https://attacker.example/v1");
        assert!(
            !decl.is_proxied(),
            "an arbitrary endpoint is not the subscription"
        );
        assert!(
            !decl.key_configured(),
            "a keyless config holds no credential to send"
        );
        assert_eq!(decl.telemetry_slug(), "openrouter");
        assert_eq!(
            bearer(&decl).await,
            None,
            "the platform credential stays home"
        );
    }

    /// A committed manifest still saying `provider = "managed"` resolves as
    /// proxied OpenRouter rather than failing. It was valid when written, and the
    /// intent — "the platform's brain" — is exactly what proxied OpenRouter is.
    #[tokio::test]
    async fn a_legacy_managed_manifest_aliases_to_proxied_openrouter() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        let decl = resolve_effective(&company, &inference(LEGACY_MANAGED), Some(&env), &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.provider, DEFAULT_PROVIDER);
        assert!(decl.is_proxied());
        assert_eq!(decl.base_url, "https://env.example/openai/v1");
        assert_eq!(bearer(&decl).await.as_deref(), Some("platform-key"));
        assert!(
            validate_inference(&inference(LEGACY_MANAGED)).is_empty(),
            "and it still validates"
        );

        // The same alias applies to a stored runtime blob, which an operator
        // cannot hand-edit — the case that would otherwise strand a tenant.
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: LEGACY_MANAGED.into(),
                base_url: None,
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        let decl = resolve_effective(&company, &Inference::default(), Some(&env), &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.source, InferenceSource::Runtime);
        assert_eq!(decl.provider, DEFAULT_PROVIDER);
        assert!(decl.is_proxied());
    }

    /// A stored runtime blob naming a provider this build does not know fails
    /// loudly rather than resolving to whatever the fallback happened to be.
    #[tokio::test]
    async fn an_unknown_stored_provider_is_an_error_not_a_silent_fallback() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "telepathy".into(),
                base_url: None,
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        let err = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .expect_err("unknown provider must fail");
        let msg = err.to_string();
        assert!(msg.contains("telepathy"), "{msg}");
        assert!(msg.contains("openrouter"), "names what is valid: {msg}");
    }

    /// Issue #585: the company's own key is the admin's to set, and a key stored
    /// through the console wins over the deploy-time env credential — otherwise
    /// the only way to pay for a tenant is an environment variable the admin
    /// cannot reach.
    ///
    /// **What changed with `managed`'s removal.** Under `managed`, a console key
    /// kept the *platform* endpoint, so an admin could bill their own account
    /// through the proxy. `openrouter` is dual-mode instead: a key means an
    /// OpenRouter key, so it goes direct to OpenRouter — sending an `sk-or-…` to
    /// the platform proxy would simply be rejected. An admin who wants the
    /// platform endpoint with a credential of their own now names
    /// `openai_compatible` with that `base_url`.
    #[tokio::test]
    async fn a_console_key_wins_over_the_env_credential_and_goes_direct() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "openrouter".into(),
                base_url: None,
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        store_key(&company, &secrets, "company-key").await.unwrap();

        let decl = resolve_effective(&company, &Inference::default(), Some(&env), &secrets)
            .await
            .unwrap()
            .expect("runtime openrouter resolves");
        assert_eq!(decl.source, InferenceSource::Runtime);
        assert_eq!(decl.provider, "openrouter");
        assert_eq!(bearer(&decl).await.as_deref(), Some("company-key"));
        assert!(decl.key_configured());
        assert!(!decl.is_proxied(), "the tenant's own account pays");
        assert_eq!(decl.base_url, OPENROUTER_BASE_URL);

        // Clearing it falls back to the subscription rather than 401ing — the
        // property that makes a key genuinely optional in both directions.
        clear_key(&company, &secrets).await.unwrap();
        let decl = resolve_effective(&company, &Inference::default(), Some(&env), &secrets)
            .await
            .unwrap()
            .expect("still resolves with no key");
        assert!(decl.is_proxied());
        assert_eq!(decl.base_url, "https://env.example/openai/v1");
        assert_eq!(bearer(&decl).await.as_deref(), Some("platform-key"));
    }

    /// Issue #585 adds a second writer to `key_configured` — an admin setting the
    /// company's key from the console — alongside the platform default the
    /// manager injects. #636's `effective_status` split exists precisely to keep
    /// the console's `keyConfigured` reporting *tenant* config and never the
    /// platform token. Nothing asserted the two stay distinguishable, so this
    /// does: on one company, the same call answers differently depending on
    /// which source is in play.
    #[tokio::test]
    async fn a_console_key_and_the_platform_default_are_distinguishable() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let platform = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "managed".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Nothing tenant-scoped is stored yet. The platform-aware resolve is
        // credentialled — that is the value the console must NOT surface — while
        // the tenant-only resolve the read route uses reports "no key".
        let with_platform =
            resolve_effective(&company, &Inference::default(), Some(&platform), &secrets)
                .await
                .unwrap()
                .expect("platform default resolves");
        let tenant_only = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap()
            .expect("runtime config resolves");
        assert!(
            with_platform.key_configured(),
            "the platform token is a real credential"
        );
        assert!(
            !tenant_only.key_configured(),
            "an injected platform token must never light up the console's `keyConfigured`"
        );

        // An admin sets the company's key. Now both agree — and the bearer the
        // agents present is the tenant's, not the platform's.
        store_key(&company, &secrets, "company-key").await.unwrap();
        let with_platform =
            resolve_effective(&company, &Inference::default(), Some(&platform), &secrets)
                .await
                .unwrap()
                .expect("platform default resolves");
        let tenant_only = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap()
            .expect("runtime config resolves");
        assert!(
            tenant_only.key_configured(),
            "a console-set key is tenant config"
        );
        assert_eq!(bearer(&with_platform).await.as_deref(), Some("company-key"));
    }

    #[tokio::test]
    async fn no_source_resolves_to_none() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let decl = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap();
        assert!(
            decl.is_none(),
            "no source means the managed/echo brain stays"
        );
    }

    #[tokio::test]
    async fn clearing_runtime_reverts_to_manifest() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let manifest = inference("openrouter");
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "ollama".into(),
                base_url: Some("http://localhost:11434/v1".into()),
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            resolve_effective(&company, &manifest, None, &secrets)
                .await
                .unwrap()
                .unwrap()
                .provider,
            "ollama"
        );
        clear_runtime_config(&company, &secrets).await.unwrap();
        assert_eq!(
            resolve_effective(&company, &manifest, None, &secrets)
                .await
                .unwrap()
                .unwrap()
                .provider,
            "openrouter"
        );
    }

    // ---- write-only key ----------------------------------------------------

    #[tokio::test]
    async fn key_is_write_only_and_never_serialized() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        store_key(&company, &secrets, "sk-super-secret")
            .await
            .unwrap();
        let decl = resolve_effective(&company, &inference("openrouter"), None, &secrets)
            .await
            .unwrap()
            .unwrap();
        // The key resolves for request building…
        assert_eq!(bearer(&decl).await.as_deref(), Some("sk-super-secret"));
        // …but never appears in the Debug rendering.
        let debug = format!("{decl:?}");
        assert!(!debug.contains("sk-super-secret"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[tokio::test]
    async fn cleared_key_reads_back_unconfigured() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        store_key(&company, &secrets, "tok").await.unwrap();
        assert!(key_configured(&company, &secrets, None).await.unwrap());
        clear_key(&company, &secrets).await.unwrap();
        assert!(!key_configured(&company, &secrets, None).await.unwrap());
    }

    #[tokio::test]
    async fn manifest_api_key_secret_is_the_fallback_key() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        // Only the manifest-named key holds a token; canonical key is cold.
        secrets
            .set(
                &company,
                "byo/openrouter",
                SecretValue("named-secret".into()),
            )
            .await
            .unwrap();
        let mut manifest = inference("openrouter");
        manifest.api_key_secret = Some("byo/openrouter".into());
        let decl = resolve_effective(&company, &manifest, None, &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bearer(&decl).await.as_deref(), Some("named-secret"));
    }

    // ---- validation --------------------------------------------------------

    #[test]
    fn absent_section_is_inert() {
        assert!(validate_inference(&Inference::default()).is_empty());
    }

    #[test]
    fn valid_configs_pass() {
        assert!(validate_inference(&inference("managed")).is_empty());
        assert!(validate_inference(&inference("openrouter")).is_empty());
        let mut ollama = inference("ollama");
        ollama.base_url = Some("http://localhost:11434/v1".into());
        assert!(
            validate_inference(&ollama).is_empty(),
            "{:?}",
            validate_inference(&ollama)
        );
    }

    #[test]
    fn unknown_provider_is_rejected() {
        let problems = validate_inference(&inference("gpt5"));
        assert!(
            problems.iter().any(|p| p.contains("provider")),
            "{problems:?}"
        );
    }

    #[test]
    fn ollama_and_openai_compatible_require_base_url() {
        let ollama = validate_inference(&inference("ollama"));
        assert!(
            ollama
                .iter()
                .any(|p| p.contains("base_url") && p.contains("required"))
        );
        let compat = validate_inference(&inference("openai_compatible"));
        assert!(
            compat
                .iter()
                .any(|p| p.contains("base_url") && p.contains("required"))
        );
    }

    #[test]
    fn non_http_base_url_is_rejected() {
        let mut m = inference("openai_compatible");
        m.base_url = Some("ftp://x/v1".into());
        let problems = validate_inference(&m);
        assert!(problems.iter().any(|p| p.contains("http")), "{problems:?}");
    }

    #[test]
    fn a_base_url_carrying_a_credential_is_rejected_and_never_echoed() {
        // Same rule as `api_key_secret` below, one field over: a credential
        // belongs in the write-only key slot, and a `base_url` is stored as
        // written and read back by every console reader.
        let mut m = inference("openai_compatible");
        m.base_url = Some("http://alice:hunter2@127.0.0.1:8597/v1".into());
        let problems = validate_inference(&m);
        assert!(
            problems.iter().any(|p| p.contains("username or password")),
            "{problems:?}"
        );
        // The refusal is the one moment this value is guaranteed to be shown to
        // somebody, so it must not quote the credential back.
        for problem in &problems {
            assert!(
                !problem.contains("hunter2") && !problem.contains("alice"),
                "a rejection echoed the credential it was rejecting: {problem}"
            );
        }

        // A malformed URL is quoted back redacted too — and the malformed ones
        // are the likeliest to have been typed by hand with a password in them.
        let mut bad = inference("openai_compatible");
        bad.base_url = Some("ftp://alice:hunter2@127.0.0.1/v1".into());
        for problem in validate_inference(&bad) {
            assert!(
                !problem.contains("hunter2"),
                "a rejection echoed the credential it was rejecting: {problem}"
            );
        }
    }

    #[test]
    fn inline_credential_in_key_name_is_rejected() {
        let mut m = inference("openrouter");
        m.api_key_secret = Some("sk-or-v1-abcdef0123456789".into());
        let problems = validate_inference(&m);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("names a secret-store key")),
            "{problems:?}"
        );

        // A long opaque token with no separators is also caught.
        let mut m2 = inference("openrouter");
        m2.api_key_secret = Some("abcdefghijklmnopqrstuvwxyz0123456789ABCDEF".into());
        assert!(!validate_inference(&m2).is_empty());

        // A structured key name is accepted.
        let mut ok = inference("openrouter");
        ok.api_key_secret = Some("byo/openrouter".into());
        assert!(
            validate_inference(&ok).is_empty(),
            "{:?}",
            validate_inference(&ok)
        );
    }

    /// The isolation property named harnesses exist for: two `built_in`
    /// harnesses on one company resolve independently, so one can ride the
    /// subscription while the other runs on a key of its own.
    #[tokio::test]
    async fn two_harnesses_on_one_company_resolve_independently() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://env.example/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        let embedded = HarnessScope::default_harness("embedded");
        let deep = HarnessScope::named("deep");

        // Only `deep` gets a key.
        store_key_scoped(&company, &secrets, "sk-or-deep", &deep)
            .await
            .unwrap();

        let d = resolve_effective_scoped(
            &company,
            &inference("openrouter"),
            Some(&env),
            &secrets,
            &deep,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!d.is_proxied(), "deep pays its own way");
        assert_eq!(d.base_url, OPENROUTER_BASE_URL);
        assert_eq!(bearer(&d).await.as_deref(), Some("sk-or-deep"));

        let e = resolve_effective_scoped(
            &company,
            &inference("openrouter"),
            Some(&env),
            &secrets,
            &embedded,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(e.is_proxied(), "embedded is untouched by deep's key");
        assert_eq!(e.base_url, "https://env.example/v1");
        assert_eq!(bearer(&e).await.as_deref(), Some("platform-key"));
    }

    /// The default harness keeps the flat legacy keys, so a tenant whose console
    /// already wrote `inference/key` keeps working with no migration — the store
    /// has no rename, so getting this wrong would orphan every running company.
    #[tokio::test]
    async fn the_default_harness_reads_the_legacy_flat_keys() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();

        // Written the pre-harness way.
        store_key(&company, &secrets, "legacy-key").await.unwrap();
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "openrouter".into(),
                base_url: None,
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();

        // Read back through the scoped path, as the default harness.
        let scope = HarnessScope::default_harness("embedded");
        assert_eq!(scope.key_key(), KEY_KEY);
        assert_eq!(scope.config_key(), RUNTIME_CONFIG_KEY);

        let decl =
            resolve_effective_scoped(&company, &Inference::default(), None, &secrets, &scope)
                .await
                .unwrap()
                .expect("the legacy config resolves");
        assert_eq!(decl.source, InferenceSource::Runtime);
        assert_eq!(bearer(&decl).await.as_deref(), Some("legacy-key"));

        // A named harness namespaces instead, and sees none of it.
        let named = HarnessScope::named("deep");
        assert_eq!(named.key_key(), "harness/deep/inference/key");
        assert_eq!(named.config_key(), "harness/deep/inference/config");
        assert!(
            load_runtime_config_scoped(&company, &secrets, &named)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Every declared tier resolves to a concrete OpenRouter slug, and a tier
    /// left unmapped by the harness still does.
    ///
    /// This is what makes the DIRECT path work at all: OpenRouter has never
    /// heard of `chat-v1`, so a bare tier on a tenant's own key would 400.
    #[test]
    fn every_tier_resolves_to_a_concrete_model_id_on_the_direct_path() {
        let none = BTreeMap::new();
        for tier in crate::company::types::INFERENCE_TIERS {
            let resolved =
                model_for_tier(tier, &none, TierVocabulary::Concrete(ConcreteTiers::all()));
            assert_ne!(
                &resolved, tier,
                "`{tier}` must map to a concrete slug, not pass through"
            );
            assert!(
                resolved.contains('/'),
                "`{tier}` resolved to `{resolved}`, which is not an OpenRouter slug"
            );
        }
    }

    /// `every_tier_resolves_to_a_concrete_model_id_on_the_direct_path` above
    /// only proves each tier resolves to *some* concrete slug — it would still
    /// pass if `chat-v1` and `agentic-v1` were swapped. These expected ids are
    /// hardcoded rather than read back from [`DEFAULT_TIER_MODELS`]: asserting
    /// a table against itself would pass no matter what the table said, so an
    /// incorrect tier assignment needs a second, independent source of truth
    /// to fail against.
    #[test]
    fn every_tier_resolves_to_its_documented_default_model() {
        let none = BTreeMap::new();
        for (tier, expected) in [
            ("chat-v1", "anthropic/claude-sonnet-5"),
            ("reasoning-v1", "openai/gpt-5.6-sol-pro"),
            ("agentic-v1", "anthropic/claude-opus-5"),
            ("vision-v1", "qwen/qwen3.8-max"),
        ] {
            assert_eq!(
                model_for_tier(tier, &none, TierVocabulary::Concrete(ConcreteTiers::all())),
                expected,
                "`{tier}` must resolve to the documented default `{expected}`"
            );
        }
    }

    #[test]
    fn a_harness_override_beats_the_default_and_a_concrete_slug_passes_through() {
        let overrides =
            BTreeMap::from([("chat-v1".to_string(), "anthropic/claude-haiku".to_string())]);
        // An operator's own entry is honoured verbatim on BOTH paths — they
        // named a specific model, and rewriting it is not this function's call.
        for vocabulary in [
            TierVocabulary::Concrete(ConcreteTiers::all()),
            TierVocabulary::Tiers,
            TierVocabulary::Unknown,
        ] {
            assert_eq!(
                model_for_tier("chat-v1", &overrides, vocabulary),
                "anthropic/claude-haiku"
            );
        }
        // An unmapped tier still takes the shipped default on the direct path.
        assert_eq!(
            model_for_tier(
                "reasoning-v1",
                &overrides,
                TierVocabulary::Concrete(ConcreteTiers::all())
            ),
            "openai/gpt-5.6-sol-pro"
        );
        // A caller naming a concrete slug is not treated as an unknown tier.
        assert_eq!(
            model_for_tier(
                "anthropic/claude-sonnet-4.5",
                &BTreeMap::new(),
                TierVocabulary::Concrete(ConcreteTiers::all())
            ),
            "anthropic/claude-sonnet-4.5"
        );
    }

    /// A tier-native endpoint keeps the tier name. The platform's registry
    /// routes on it and pins each tier to a sub-provider; substituting a
    /// concrete slug would bypass that pinning, and its passthrough namespace
    /// is opt-in and off by default, so the slug would simply be rejected.
    #[test]
    fn a_tier_native_endpoint_keeps_the_tier_name() {
        let none = BTreeMap::new();
        for tier in crate::company::types::INFERENCE_TIERS {
            assert_eq!(&model_for_tier(tier, &none, TierVocabulary::Tiers), tier);
        }
    }

    /// The signal that answers "which of these is this provider?" — a catalog
    /// publishing `agentic-v1` is telling us it resolves tiers itself. Nothing
    /// here looks at a hostname: the same catalog served from anywhere
    /// classifies the same way.
    #[test]
    fn a_catalog_publishing_tier_names_classifies_as_tier_native() {
        assert_eq!(
            TierVocabulary::from_catalog_ids([
                "reasoning-v1",
                "vision-v1",
                "chat-v1",
                "burst-v1",
                "agentic-v1",
                "coding-v1",
                "whisper-v1",
                "embedding-v1",
            ]),
            TierVocabulary::Tiers
        );
        // Three of four is still a tier-native provider — demanding a complete
        // set would silently fall back to substitution for it.
        assert_eq!(
            TierVocabulary::from_catalog_ids(["chat-v1", "agentic-v1", "reasoning-v1"]),
            TierVocabulary::Tiers
        );
    }

    /// OpenRouter's catalog publishes the concrete ids and none of the tiers,
    /// so it keeps the shipped substitution.
    #[test]
    fn a_catalog_publishing_the_shipped_ids_classifies_as_concrete() {
        assert_eq!(
            TierVocabulary::from_catalog_ids([
                "anthropic/claude-opus-5",
                "anthropic/claude-sonnet-5",
                "openai/gpt-5.6-sol-pro",
                "qwen/qwen3.8-max",
                "meta-llama/llama-4",
            ]),
            TierVocabulary::Concrete(ConcreteTiers::all())
        );
    }

    /// A **partial** OpenRouter mirror is still `Concrete`, but only for the ids
    /// it actually publishes.
    ///
    /// Classification is `any` — one shipped id present is enough to say this
    /// endpoint speaks OpenRouter's catalog — and that was once taken as licence
    /// to substitute all four. A gateway mirroring only
    /// `anthropic/claude-sonnet-5` was therefore handed `openai/gpt-5.6-sol-pro`
    /// for `reasoning-v1`: an id that same catalog had just said it does not
    /// serve. That is this module's own defect one level down, so it now fails
    /// the same way — the unpublished tier goes out as the tier (Codex review
    /// on #2045).
    #[test]
    fn a_partial_mirror_substitutes_only_the_ids_its_catalog_publishes() {
        let vocabulary = TierVocabulary::from_catalog_ids([
            "anthropic/claude-sonnet-5",
            "some-gateway/unrelated-model",
        ]);
        assert_eq!(vocabulary.as_str(), "concrete");
        assert_ne!(
            vocabulary,
            TierVocabulary::Concrete(ConcreteTiers::all()),
            "one shipped id is not evidence for the other three"
        );

        let none = BTreeMap::new();
        assert_eq!(
            model_for_tier("chat-v1", &none, vocabulary),
            "anthropic/claude-sonnet-5",
            "the tier this catalog does publish is substituted exactly as before"
        );
        for absent in ["reasoning-v1", "agentic-v1", "vision-v1"] {
            assert_eq!(
                model_for_tier(absent, &none, vocabulary),
                absent,
                "`{absent}` is absent from this catalog, so synthesizing its shipped id would be \
                 a guess the catalog has already contradicted"
            );
        }

        assert_eq!(
            vocabulary.tier_defaults(),
            BTreeMap::from([(
                "chat-v1".to_string(),
                "anthropic/claude-sonnet-5".to_string()
            )]),
            "the console is offered defaults only for the tiers this endpoint publishes"
        );
    }

    /// A catalog that publishes neither is `Unknown`, and `Unknown` supplies no
    /// defaults at all. This is the half that broke the probe: the shipped ids
    /// used to be applied to *every* non-proxied endpoint, so an operator ended
    /// up with a four-entry mapping their provider had already told us it does
    /// not publish.
    #[test]
    fn a_catalog_publishing_neither_vocabulary_offers_no_defaults() {
        let vocabulary = TierVocabulary::from_catalog_ids(["llama3.1", "mistral-small"]);
        assert_eq!(vocabulary, TierVocabulary::Unknown);
        assert!(
            vocabulary.tier_defaults().is_empty(),
            "an endpoint whose catalog names none of our ids has no default we can honestly supply"
        );
        let none = BTreeMap::new();
        assert_eq!(
            model_for_tier("agentic-v1", &none, vocabulary),
            "agentic-v1",
            "the tier goes out unchanged, so the provider's 400 names a string the operator \
             configured rather than an `anthropic/…` id they never typed"
        );
    }

    /// The defaults each vocabulary implies, asserted against hardcoded values
    /// rather than read back from the tables they come from — asserting a table
    /// against itself would pass whatever the table said.
    #[test]
    fn tier_defaults_follow_the_vocabulary() {
        assert_eq!(
            TierVocabulary::Tiers.tier_defaults(),
            BTreeMap::from([
                ("chat-v1".to_string(), "chat-v1".to_string()),
                ("reasoning-v1".to_string(), "reasoning-v1".to_string()),
                ("agentic-v1".to_string(), "agentic-v1".to_string()),
                ("vision-v1".to_string(), "vision-v1".to_string()),
            ]),
            "a tier-native provider resolves the tier itself, so identity is the mapping"
        );
        assert_eq!(
            TierVocabulary::Concrete(ConcreteTiers::all())
                .tier_defaults()
                .get("agentic-v1")
                .map(String::as_str),
            Some("anthropic/claude-opus-5")
        );
    }

    /// The pre-discovery fallback, and the reason discovery has to exist: a
    /// tenant-keyed config is not proxied, so without a discovered answer it is
    /// still guessed as `Concrete` — which is exactly the guess that failed
    /// against a tier-native endpoint. Attaching the endpoint's own answer is
    /// what changes it, and `is_proxied()` is untouched by that, because who
    /// pays and what vocabulary is spoken are different facts.
    #[test]
    fn a_discovered_vocabulary_overrides_the_payer_derived_guess() {
        let decl = decl_for_probe(
            "openrouter",
            Some(PLATFORM_BASE_URL),
            Some("test-token"),
            None,
        );
        assert!(!decl.is_proxied(), "a tenant key means the tenant pays");
        assert_eq!(
            decl.vocabulary(),
            TierVocabulary::Concrete(ConcreteTiers::all()),
            "the pre-discovery guess"
        );
        assert!(!decl.vocabulary_confirmed());

        let decl = decl.with_vocabulary(Some(TierVocabulary::Tiers));
        assert!(decl.vocabulary_confirmed());
        assert_eq!(decl.vocabulary(), TierVocabulary::Tiers);
        assert!(
            !decl.is_proxied(),
            "discovering the vocabulary must not move who is billed"
        );
        assert_eq!(
            model_for_tier("agentic-v1", &decl.models, decl.vocabulary()),
            "agentic-v1",
            "the tier reaches a tier-native endpoint intact"
        );
    }

    #[test]
    fn provider_slugs_map_as_documented() {
        assert_eq!(provider_slug("openrouter"), "openrouter");
        assert_eq!(provider_slug("openai_compatible"), "byok");
        assert_eq!(provider_slug("ollama"), "ollama");
        // The legacy kind slugs as what it now is, so historical usage rows and
        // new ones aggregate together.
        assert_eq!(provider_slug(LEGACY_MANAGED), "openrouter");
        // An unknown kind is never folded into a real provider's attribution.
        assert_eq!(provider_slug("mystery"), "unknown");
    }

    #[test]
    fn effective_base_url_defaults_per_provider() {
        assert_eq!(
            effective_base_url(LEGACY_MANAGED, None),
            OPENROUTER_BASE_URL
        );
        assert_eq!(effective_base_url("openrouter", None), OPENROUTER_BASE_URL);
        assert_eq!(effective_base_url("ollama", None), OLLAMA_DEFAULT_BASE_URL);
        assert_eq!(
            effective_base_url("openrouter", Some("https://proxy/v1")),
            "https://proxy/v1"
        );
    }

    #[test]
    fn setup_accepts_the_localhost_spelling_local_model_apps_display() {
        assert_eq!(
            normalize_setup_base_url("ollama", Some("localhost:6969")),
            Some("http://localhost:6969/v1".to_string())
        );
        assert_eq!(
            normalize_setup_base_url("openai_compatible", Some("http://127.0.0.1:1234/v1/")),
            Some("http://127.0.0.1:1234/v1".to_string())
        );
        assert_eq!(
            normalize_setup_base_url("openai_compatible", Some("https://llm.test/api")),
            Some("https://llm.test/api".to_string())
        );
    }

    // ---- first-run probe (decl_for_probe) ----------------------------------

    fn managed_env() -> EnvDefault {
        EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::from_value("platform-key"),
        }
    }

    /// The managed card sends `provider = "managed"` and, because it has no URL
    /// field, whatever `base_url` a previously-picked provider left in the form —
    /// `openrouter.ai` here. The probe must ignore that stale endpoint and reach
    /// the managed endpoint with the managed credential. On the pre-fix code this
    /// went direct to `openrouter.ai` with no credential and 401'd.
    #[tokio::test]
    async fn managed_probe_ignores_stale_base_url_and_uses_managed_endpoint() {
        let env = managed_env();
        let decl = decl_for_probe(
            "managed",
            Some("https://openrouter.ai/api/v1"),
            None,
            Some(&env),
        );
        assert_eq!(decl.base_url, "https://env.example/openai/v1");
        assert!(decl.is_proxied());
        assert_eq!(bearer(&decl).await.as_deref(), Some("platform-key"));
    }

    /// A managed probe where the operator supplied their own TinyHumans key still
    /// reaches the managed endpoint — not `openrouter.ai` — carrying that key.
    #[tokio::test]
    async fn managed_probe_with_own_key_keeps_the_managed_endpoint() {
        let env = managed_env();
        let decl = decl_for_probe(
            "managed",
            Some("https://openrouter.ai/api/v1"),
            Some("th-key"),
            Some(&env),
        );
        assert_eq!(decl.base_url, "https://env.example/openai/v1");
        assert!(decl.is_proxied());
        assert_eq!(bearer(&decl).await.as_deref(), Some("th-key"));
    }

    /// A host holding no managed credential probes the managed endpoint honestly
    /// unauthenticated — so the failure names `api.tinyhumans.ai`, not the stale
    /// `openrouter.ai` the form carried over.
    #[tokio::test]
    async fn managed_probe_without_env_default_reports_the_platform_endpoint() {
        let decl = decl_for_probe("managed", Some("https://openrouter.ai/api/v1"), None, None);
        assert_eq!(decl.base_url, PLATFORM_BASE_URL);
        assert_eq!(bearer(&decl).await, None);
    }

    /// The real providers must keep honouring the form's `base_url` and `key` —
    /// the managed diversion must not over-correct them.
    #[tokio::test]
    async fn other_provider_probes_still_honour_the_form_endpoint_and_key() {
        let openrouter =
            decl_for_probe("openrouter", Some("https://proxy/v1"), Some("or-key"), None);
        assert_eq!(openrouter.base_url, "https://proxy/v1");
        assert!(!openrouter.is_proxied());
        assert_eq!(bearer(&openrouter).await.as_deref(), Some("or-key"));

        let compatible = decl_for_probe(
            "openai_compatible",
            Some("https://llm.test/v1"),
            Some("k"),
            None,
        );
        assert_eq!(compatible.base_url, "https://llm.test/v1");
        assert_eq!(bearer(&compatible).await.as_deref(), Some("k"));

        let ollama = decl_for_probe("ollama", None, None, None);
        assert_eq!(ollama.base_url, OLLAMA_DEFAULT_BASE_URL);
        assert_eq!(bearer(&ollama).await, None);
    }

    /// A keyless `openrouter` with its own `base_url` override still goes direct
    /// and keyless — the platform credential must never ride an arbitrary
    /// endpoint. Unchanged by the managed fix.
    #[tokio::test]
    async fn keyless_openrouter_override_probe_stays_direct_and_keyless() {
        let env = managed_env();
        let decl = decl_for_probe(
            "openrouter",
            Some("https://attacker.example/v1"),
            None,
            Some(&env),
        );
        assert_eq!(decl.base_url, "https://attacker.example/v1");
        assert!(!decl.is_proxied());
        assert_eq!(bearer(&decl).await, None);
    }

    // ---- the managed credential chain (issue #2266) -------------------------
    //
    // ```text
    //   1. provider/tinyhumans/key   a key pasted specifically for inference
    //   2. inference/key             the legacy address, read-only
    //   3. tinyhumans/key            the company's account identity
    //   4. instance identity         TINYHUMANS_TOKEN_FILE, else TINYHUMANS_API_KEY
    //   5. nothing                   fail closed
    // ```
    //
    // Steps 3 and 4 apply **only** when the vendor at the other end is the
    // identity's own vendor. The OpenRouter test below is the important one.

    async fn write(secrets: &MemSecrets, key: &str, value: &str) {
        secrets
            .set(&CompanyId::new("acme"), key, SecretValue(value.into()))
            .await
            .unwrap();
    }

    async fn resolve_managed(secrets: &MemSecrets) -> InferenceDecl {
        let company = CompanyId::new("acme");
        let config = RuntimeInference {
            provider: "managed".into(),
            base_url: None,
            models: BTreeMap::new(),
        };
        save_runtime_config(&company, secrets, &config)
            .await
            .unwrap();
        resolve_effective(
            &company,
            &Inference::default(),
            Some(&managed_env()),
            secrets,
        )
        .await
        .unwrap()
        .expect("a managed config resolves")
    }

    #[tokio::test]
    async fn managed_with_a_pasted_inference_key_uses_it() {
        let secrets = MemSecrets::default();
        write(
            &secrets,
            &store::provider_key_key(MANAGED_SLUG),
            "sk-not-a-real-key",
        )
        .await;
        // Present but outranked, so the ordering is actually exercised.
        write(&secrets, crate::company::company_key::KEY_KEY, "th-account").await;

        let decl = resolve_managed(&secrets).await;
        assert_eq!(bearer(&decl).await.as_deref(), Some("sk-not-a-real-key"));
        assert_eq!(decl.base_url, managed_env().base_url);
    }

    #[tokio::test]
    async fn managed_falls_back_to_the_company_account_key_and_keeps_the_platform_endpoint() {
        // The substance of #2266: a company key set in the console reached
        // Composio and never reached inference, so setting it moved the app
        // connections onto the company's account and left every agent turn —
        // the expensive half — on whoever runs the server.
        let secrets = MemSecrets::default();
        write(&secrets, crate::company::company_key::KEY_KEY, "th-account").await;

        let decl = resolve_managed(&secrets).await;
        assert_eq!(bearer(&decl).await.as_deref(), Some("th-account"));
        // **Assert the endpoint, not only the bearer.** Sending a `th_…` key to
        // openrouter.ai is the shipped bug this chain must not reproduce, and a
        // test that checked the bearer alone is exactly how it shipped.
        assert_eq!(decl.base_url, managed_env().base_url);
        assert!(
            !decl.base_url.contains("openrouter.ai"),
            "{}",
            decl.base_url
        );
    }

    #[tokio::test]
    async fn managed_with_neither_uses_the_instance_identity() {
        let secrets = MemSecrets::default();
        let decl = resolve_managed(&secrets).await;
        assert_eq!(bearer(&decl).await.as_deref(), Some("platform-key"));
        assert_eq!(decl.base_url, managed_env().base_url);
    }

    #[tokio::test]
    async fn openrouter_never_receives_the_company_identity_or_the_instance_one() {
        // THE test. An identity flows to a surface only when the vendor at the
        // other end is the identity's own vendor: a `th_…` key means nothing to
        // OpenRouter, and presenting it there is both a failed request and a
        // credential disclosed to a third party.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        write(&secrets, crate::company::company_key::KEY_KEY, "th-account").await;

        let config = RuntimeInference {
            provider: "openrouter".into(),
            // An explicit endpoint is what makes this unambiguously the tenant's
            // own OpenRouter rather than the platform proxy in front of it.
            base_url: Some(OPENROUTER_BASE_URL.into()),
            models: BTreeMap::new(),
        };
        save_runtime_config(&company, &secrets, &config)
            .await
            .unwrap();
        let decl = resolve_effective(
            &company,
            &Inference::default(),
            Some(&managed_env()),
            &secrets,
        )
        .await
        .unwrap()
        .expect("an openrouter config resolves");

        assert_eq!(decl.base_url, OPENROUTER_BASE_URL);
        assert!(!decl.is_proxied());
        let presented = bearer(&decl).await;
        assert_ne!(
            presented.as_deref(),
            Some("th-account"),
            "the company identity leaked to a vendor"
        );
        assert_ne!(
            presented.as_deref(),
            Some("platform-key"),
            "the instance identity leaked to a vendor"
        );
        assert_eq!(
            presented, None,
            "no credential at all is the correct answer here"
        );
    }

    #[tokio::test]
    async fn a_legacy_company_reads_the_flat_slot_and_one_save_moves_it() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        write(&secrets, KEY_KEY, "sk-not-a-real-key").await;

        let decl = resolve_managed(&secrets).await;
        assert_eq!(
            bearer(&decl).await.as_deref(),
            Some("sk-not-a-real-key"),
            "the legacy address is still read, so an untouched company keeps working"
        );

        // One save through the provider store converges the address.
        let zero = store::list_providers(&company, &secrets).await.unwrap()[0].clone();
        store::store_provider_key(&company, &secrets, &zero, "sk-not-a-real-key")
            .await
            .unwrap();
        assert_eq!(
            secrets.get(&company, KEY_KEY).await.unwrap(),
            Some(SecretValue(String::new())),
            "and clears the old one, so no secret is orphaned"
        );
        assert_eq!(
            secrets
                .get(&company, &store::provider_key_key(MANAGED_SLUG))
                .await
                .unwrap(),
            Some(SecretValue("sk-not-a-real-key".into()))
        );
    }

    // ---- the managed row's honest state -------------------------------------

    #[test]
    fn managed_reports_which_step_of_the_chain_answers() {
        // Not a boolean, and not "always on". The row that renders this used to
        // claim permanent availability, inherited from a design where the same
        // company runs the managed backend — here it needs a credential and can
        // resolve to nothing.
        let env = managed_env();
        let company = Credential::from_company_key("th-account");

        assert_eq!(
            managed_source(true, &company, Some(&env)),
            ManagedSource::ProviderKey,
            "a key pasted for inference outranks everything below it"
        );
        assert_eq!(
            managed_source(false, &company, Some(&env)),
            ManagedSource::CompanyAccount,
        );
        assert_eq!(
            managed_source(false, &Credential::None, Some(&env)),
            ManagedSource::Instance,
            "the server's account pays, and the row has to say so"
        );
        assert_eq!(
            managed_source(false, &Credential::None, None),
            ManagedSource::None,
            "nothing resolves — not set up, and not a green badge"
        );
    }

    #[test]
    fn the_two_paying_states_are_not_collapsed() {
        // An operator deciding whether to connect their account needs to know
        // which one they are on. "On" for both hides the decision.
        let env = managed_env();
        assert_ne!(
            managed_source(
                false,
                &Credential::from_company_key("th-account"),
                Some(&env)
            ),
            managed_source(false, &Credential::None, Some(&env)),
        );
    }

    #[test]
    fn an_env_default_that_would_yield_nothing_is_not_availability() {
        // `configured()` rather than presence: a projected-token source reports
        // itself present while its file can still yield nothing, and what
        // decides availability is whether a value would reach the wire.
        let empty = EnvDefault {
            base_url: "https://env.example/openai/v1".into(),
            credential: Credential::None,
        };
        assert_eq!(
            managed_source(false, &Credential::None, Some(&empty)),
            ManagedSource::None
        );
    }

    // ---- the provider list actually reaches the resolver ---------------------
    //
    // Every other test in this module seeds `inference/config` or exercises the
    // store in isolation, which is exactly how a feature comes to be fully built
    // on both sides and connected in neither: the write routes populated
    // `inference/providers`, the status route rendered it, and nothing on the
    // turn path ever read it. A company that added a provider through the
    // console had configured its *display*, not its company — the chat pane said
    // "no model configured" and was telling the truth.
    //
    // These write a provider through the store with NO legacy blob anywhere and
    // assert a turn resolves to it.

    async fn add_indexed(secrets: &MemSecrets, slug: &str, key: &str) {
        let company = CompanyId::new("acme");
        store::put_provider(
            &company,
            secrets,
            store::ProviderDraft {
                slug: slug.to_string(),
                label: slug.to_string(),
                kind: "openai_compatible".to_string(),
                base_url: format!("https://{slug}.example/v1"),
                models: BTreeMap::new(),
                enabled: true,
            },
        )
        .await
        .unwrap();
        secrets
            .set(
                &company,
                &store::provider_key_key(slug),
                SecretValue(key.to_string()),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_provider_added_through_the_console_resolves_for_a_turn() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        add_indexed(&secrets, "acme", "sk-not-a-real-key").await;
        // Deliberately no `inference/config`: this is what a company that only
        // ever used the provider list looks like on disk.
        assert!(
            load_runtime_config(&company, &secrets)
                .await
                .unwrap()
                .is_none(),
            "the legacy blob must be absent for this test to mean anything"
        );

        let decl = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap()
            .expect("a company with a provider resolves");
        assert_eq!(decl.base_url, "https://acme.example/v1");
        assert_eq!(bearer(&decl).await.as_deref(), Some("sk-not-a-real-key"));
        assert!(decl.key_configured());
    }

    #[tokio::test]
    async fn the_marked_default_is_the_one_a_turn_reaches() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        add_indexed(&secrets, "first", "sk-not-a-real-key-1").await;
        add_indexed(&secrets, "second", "sk-not-a-real-key-2").await;

        // No marker: list order, which is the behaviour that predates the marker.
        let decl = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.base_url, "https://first.example/v1");

        store::set_default_slug(&company, &secrets, "second")
            .await
            .unwrap();
        let decl = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            decl.base_url, "https://second.example/v1",
            "marking a default has to move where a turn actually goes, not just a badge"
        );
        assert_eq!(bearer(&decl).await.as_deref(), Some("sk-not-a-real-key-2"));
    }

    #[tokio::test]
    async fn a_disabled_provider_is_not_where_a_turn_goes() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        add_indexed(&secrets, "off", "sk-not-a-real-key-1").await;
        add_indexed(&secrets, "on", "sk-not-a-real-key-2").await;
        store::set_enabled(&company, &secrets, "off", false)
            .await
            .unwrap();

        let decl = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.base_url, "https://on.example/v1");
    }

    #[tokio::test]
    async fn the_legacy_blob_still_wins_when_it_is_the_only_thing_there() {
        // Entry zero sorts first in the list, so a company that had one provider
        // before any of this existed keeps resolving exactly where it did. The
        // whole entry-zero design exists to make that true without a migration.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "openai_compatible".into(),
                base_url: Some("https://legacy.example/v1".into()),
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        store_key(&company, &secrets, "sk-not-a-real-key")
            .await
            .unwrap();

        let decl = resolve_effective(&company, &Inference::default(), None, &secrets)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decl.base_url, "https://legacy.example/v1");
        assert_eq!(bearer(&decl).await.as_deref(), Some("sk-not-a-real-key"));
        assert_eq!(decl.source, InferenceSource::Runtime);
    }

    #[tokio::test]
    async fn nothing_configured_still_resolves_to_nothing() {
        // The list being empty must not become a way to resolve *something*.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        assert!(
            resolve_effective(&company, &Inference::default(), None, &secrets)
                .await
                .unwrap()
                .is_none()
        );
    }

    // ---- the routing table actually reaches the resolver ---------------------
    //
    // The same shape as the block above, one layer along, and found the same
    // way: the Routing tab wrote `inference/routes`, the status route rendered
    // it, `provider_for_workload` decided over it in isolation — and the turn
    // path never asked. Every row of that screen persisted, survived a reload,
    // and changed nothing about where a turn went. A control that visibly fails
    // is a bug; a control that reports success and is inert is a lie, and it is
    // the harder one to find because nothing looks wrong.

    /// The route a tier resolves to, as the wire model the plan would carry.
    fn wire_model(decl: &InferenceDecl, tier: &str) -> String {
        model_for_tier(tier, &decl.models, decl.vocabulary())
    }

    async fn route(secrets: &MemSecrets, tier: &str, raw: &str) {
        let company = CompanyId::new("acme");
        let mut routes = store::load_routes(&company, secrets).await.unwrap();
        routes.insert(tier.to_string(), resolve::ProviderRef::parse(raw));
        store::save_routes(&company, secrets, &routes)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_route_sends_its_workload_to_the_provider_and_model_it_names() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        add_indexed(&secrets, "first", "sk-not-a-real-key-1").await;
        add_indexed(&secrets, "second", "sk-not-a-real-key-2").await;
        route(&secrets, "chat-v1", "second:deepseek/deepseek-v4-flash").await;

        let decl = resolve_effective_for_tier(
            &company,
            &Inference::default(),
            None,
            &secrets,
            &HarnessScope::default(),
            "chat-v1",
        )
        .await
        .unwrap()
        .expect("a routed workload resolves");
        assert_eq!(
            decl.base_url, "https://second.example/v1",
            "the route names the provider, so the turn goes there and not to the primary"
        );
        assert_eq!(bearer(&decl).await.as_deref(), Some("sk-not-a-real-key-2"));
        assert_eq!(
            wire_model(&decl, "chat-v1"),
            "deepseek/deepseek-v4-flash",
            "the model the route pinned is the model that goes on the wire"
        );
    }

    #[tokio::test]
    async fn the_route_beats_the_default_providers_own_tier_map() {
        // The sharpest form of the bug, and the one an operator hit: Use Your
        // Own Models wrote `anthropic:claude-sonnet-5` into all four tiers, and
        // the next turn sent `anthropic/claude-opus-5` to OpenRouter — neither
        // their provider nor their model. The route was inert in both
        // dimensions, and the two failures hid each other: with a route naming
        // the provider that was already the default, "route honoured" and "route
        // ignored" look identical. This asserts both halves at once by pointing
        // the route at a provider that is *not* the marked default, and pinning
        // a model the default's own tier map would answer differently.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();

        let mut openrouter_tiers = BTreeMap::new();
        openrouter_tiers.insert("chat-v1".to_string(), "anthropic/claude-opus-5".to_string());
        store::put_provider(
            &company,
            &secrets,
            store::ProviderDraft {
                slug: "openrouter".into(),
                label: "OpenRouter".into(),
                kind: "openrouter".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                models: openrouter_tiers,
                enabled: true,
            },
        )
        .await
        .unwrap();
        secrets
            .set(
                &company,
                &store::provider_key_key("openrouter"),
                SecretValue("sk-not-a-real-key-or".into()),
            )
            .await
            .unwrap();
        store::put_provider(
            &company,
            &secrets,
            store::ProviderDraft {
                slug: "anthropic".into(),
                label: "Anthropic".into(),
                kind: "anthropic".into(),
                base_url: "https://api.anthropic.com/v1".into(),
                models: BTreeMap::new(),
                enabled: true,
            },
        )
        .await
        .unwrap();
        secrets
            .set(
                &company,
                &store::provider_key_key("anthropic"),
                SecretValue("sk-not-a-real-key-ant".into()),
            )
            .await
            .unwrap();
        store::set_default_slug(&company, &secrets, "openrouter")
            .await
            .unwrap();
        route(&secrets, "chat-v1", "anthropic:claude-sonnet-5").await;

        let decl = resolve_effective_for_tier(
            &company,
            &Inference::default(),
            None,
            &secrets,
            &HarnessScope::default(),
            "chat-v1",
        )
        .await
        .unwrap()
        .expect("the routed workload resolves");
        assert_eq!(
            decl.base_url, "https://api.anthropic.com/v1",
            "the route names anthropic, so the turn goes to anthropic and not to the default"
        );
        assert_eq!(
            bearer(&decl).await.as_deref(),
            Some("sk-not-a-real-key-ant")
        );
        assert_eq!(
            wire_model(&decl, "chat-v1"),
            "claude-sonnet-5",
            "the route's model beats the default provider's own tier map"
        );
    }

    #[tokio::test]
    async fn a_workload_with_no_route_still_falls_through_to_the_primary() {
        // The other half of the property: pinning one row must not move the
        // others. A fix that routed everything through the chat row would pass
        // the test above and be worse than the bug.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        add_indexed(&secrets, "first", "sk-not-a-real-key-1").await;
        add_indexed(&secrets, "second", "sk-not-a-real-key-2").await;
        route(&secrets, "chat-v1", "second:deepseek/deepseek-v4-flash").await;

        let decl = resolve_effective_for_tier(
            &company,
            &Inference::default(),
            None,
            &secrets,
            &HarnessScope::default(),
            "reasoning-v1",
        )
        .await
        .unwrap()
        .expect("an unrouted workload resolves");
        assert_eq!(decl.base_url, "https://first.example/v1");
        assert_eq!(bearer(&decl).await.as_deref(), Some("sk-not-a-real-key-1"));
        assert_ne!(
            wire_model(&decl, "reasoning-v1"),
            "deepseek/deepseek-v4-flash",
            "another row's pinned model must not leak onto this one"
        );
    }

    #[tokio::test]
    async fn a_route_naming_managed_resolves_through_the_managed_chain() {
        // `managed` is a word in the route grammar, not a provider slug. Read as
        // a slug it resolves to nothing and the workload fails closed — which is
        // what the Managed mode button writes into every row.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        let env = EnvDefault {
            base_url: "https://platform.example/v1".into(),
            credential: Credential::from_value("platform-key"),
        };
        add_indexed(&secrets, "first", "sk-not-a-real-key-1").await;
        route(&secrets, "agentic-v1", "managed").await;

        let decl = resolve_effective_for_tier(
            &company,
            &Inference::default(),
            Some(&env),
            &secrets,
            &HarnessScope::default(),
            "agentic-v1",
        )
        .await
        .unwrap()
        .expect("a managed route resolves");
        assert!(decl.is_proxied(), "the managed route rides the platform");
        assert_eq!(decl.base_url, "https://platform.example/v1");
        assert_eq!(bearer(&decl).await.as_deref(), Some("platform-key"));
    }

    #[tokio::test]
    async fn a_route_naming_a_provider_that_is_gone_fails_closed() {
        // An unset workload falls back, because nobody chose anything for it. A
        // route is a choice with a workload attached, so it fails rather than
        // quietly spending on an account the operator did not name.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        add_indexed(&secrets, "first", "sk-not-a-real-key-1").await;
        route(&secrets, "chat-v1", "ghost:gpt-5").await;

        let err = resolve_effective_for_tier(
            &company,
            &Inference::default(),
            None,
            &secrets,
            &HarnessScope::default(),
            "chat-v1",
        )
        .await
        .expect_err("a route naming nothing must not silently fall back");
        let message = err.to_string();
        assert!(message.contains("ghost"), "{message}");
        assert!(message.contains("chat"), "{message}");
    }

    #[tokio::test]
    async fn a_route_naming_a_switched_off_provider_fails_closed_too() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        add_indexed(&secrets, "first", "sk-not-a-real-key-1").await;
        add_indexed(&secrets, "parked", "sk-not-a-real-key-2").await;
        store::set_enabled(&company, &secrets, "parked", false)
            .await
            .unwrap();
        route(&secrets, "vision-v1", "parked").await;

        let err = resolve_effective_for_tier(
            &company,
            &Inference::default(),
            None,
            &secrets,
            &HarnessScope::default(),
            "vision-v1",
        )
        .await
        .expect_err("a parked route is a choice that no longer works");
        assert!(err.to_string().contains("parked"), "{err}");
    }

    #[tokio::test]
    async fn coding_reads_the_agentic_route_rather_than_one_of_its_own() {
        // The alias, asserted on the path that matters. A coding turn arrives
        // carrying `agentic-v1`, so this is really a statement about the tier
        // the route is keyed on — and it is the reason routes are keyed on tiers
        // rather than on workload names.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        add_indexed(&secrets, "first", "sk-not-a-real-key-1").await;
        add_indexed(&secrets, "second", "sk-not-a-real-key-2").await;
        route(&secrets, "agentic-v1", "second").await;

        let decl = resolve_effective_for_tier(
            &company,
            &Inference::default(),
            None,
            &secrets,
            &HarnessScope::default(),
            "agentic-v1",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(decl.base_url, "https://second.example/v1");
    }

    #[tokio::test]
    async fn a_route_naming_entry_zero_reaches_the_legacy_config() {
        // Entry zero is the legacy blob wearing a provider's clothes. A route
        // that names it has to reach the blob's own endpoint and credential —
        // not whichever indexed provider happens to be the primary.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        save_runtime_config(
            &company,
            &secrets,
            &RuntimeInference {
                provider: "openai_compatible".into(),
                base_url: Some("https://legacy.example/v1".into()),
                models: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        store_key(&company, &secrets, "sk-not-a-real-key-legacy")
            .await
            .unwrap();
        add_indexed(&secrets, "second", "sk-not-a-real-key-2").await;
        store::set_default_slug(&company, &secrets, "second")
            .await
            .unwrap();

        let zero = store::list_providers(&company, &secrets).await.unwrap()[0].clone();
        route(&secrets, "chat-v1", &format!("{}:gpt-5", zero.slug)).await;

        let decl = resolve_effective_for_tier(
            &company,
            &Inference::default(),
            None,
            &secrets,
            &HarnessScope::default(),
            "chat-v1",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(decl.base_url, "https://legacy.example/v1");
        assert_eq!(
            bearer(&decl).await.as_deref(),
            Some("sk-not-a-real-key-legacy")
        );
        assert_eq!(wire_model(&decl, "chat-v1"), "gpt-5");
    }

    #[tokio::test]
    async fn a_tier_nobody_routes_resolves_exactly_as_it_always_did() {
        // `embedding-v1` and friends have no row on the Routing tab. They must
        // not acquire one by accident, and they must not fail closed either.
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();
        add_indexed(&secrets, "first", "sk-not-a-real-key-1").await;

        let decl = resolve_effective_for_tier(
            &company,
            &Inference::default(),
            None,
            &secrets,
            &HarnessScope::default(),
            "embedding-v1",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(decl.base_url, "https://first.example/v1");
    }
}
