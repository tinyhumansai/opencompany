//! The payload vocabulary: what an analytics event is allowed to say.
//!
//! # The structural rule
//!
//! Issue #1739, decision 3: an event may carry its name, timing, success or
//! failure, counts, and enum-valued fields. It may **never** carry message text,
//! prompts, file names, ledger values, tool arguments, addresses, or any
//! free-form string that came from a human or an agent.
//!
//! That is enforced by the type system rather than by review. [`PropValue`] has
//! four variants and **none of them holds a `String`**:
//!
//! ```text
//! Word(&'static str)   Count(u64)   Amount(f64)   Flag(bool)
//! ```
//!
//! A `&'static str` cannot be produced from runtime data without deliberately
//! leaking memory, so every textual property in every event is a literal that
//! was written in this repository and reviewed here. A call site that wants to
//! report a provider, an outcome or a failure must first pass its runtime string
//! through a classifier ([`provider_slug`], [`FailureCode::of`], [`Trigger::of`])
//! that maps it onto one of a fixed set of literals, and anything unrecognised
//! becomes `"other"`. Adding a leak means adding a `String` variant here, which
//! is a visible act rather than an oversight at a call site.
//!
//! The one place an event carries a runtime string is the identity, and it is
//! [`OpaqueId`] — a newtype whose only constructors are "the random instance id"
//! and "the SHA-256 of a tenant slug". It is not a property; it is the
//! `distinct_id`, and it is opaque by construction (decision 2).

use std::fmt;

use crate::app::deployment::Deployment;
use crate::error::OpenCompanyError;
use crate::ports::brain::{Cognition, UsageMetering};
use crate::ports::types::CompanyEvent;
use crate::ports::usage::{SampleKind, UsageSample};

// ---------------------------------------------------------------------------
// Property values
// ---------------------------------------------------------------------------

/// One property value on an analytics event.
///
/// **There is deliberately no `String` variant, and there must never be one.**
/// See the module docs: this enum is the whole of decision 3's enforcement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PropValue {
    /// A literal drawn from a vocabulary compiled into this binary.
    Word(&'static str),
    /// A count. Counts are shape, never content.
    Count(u64),
    /// A quantity — money, a ratio.
    Amount(f64),
    /// A yes/no fact.
    Flag(bool),
}

impl PropValue {
    /// The value as JSON, for a transport that speaks JSON.
    pub fn to_json(self) -> serde_json::Value {
        match self {
            Self::Word(word) => serde_json::Value::from(word),
            Self::Count(count) => serde_json::Value::from(count),
            Self::Amount(amount) => serde_json::Number::from_f64(amount)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Self::Flag(flag) => serde_json::Value::from(flag),
        }
    }
}

/// A property: a literal key and a [`PropValue`].
pub type Prop = (&'static str, PropValue);

// ---------------------------------------------------------------------------
// Closed vocabularies
// ---------------------------------------------------------------------------

/// The catch-all every classifier falls back to.
///
/// It exists so that an unrecognised input degrades to *less information*
/// rather than to *more*. The alternative — passing the unrecognised string
/// through — is exactly the leak this module is built to prevent, and it would
/// arrive by way of a value nobody anticipated, which is the only way such a
/// leak ever arrives.
pub const OTHER: &str = "other";

/// What ended a cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The cycle returned a report.
    Ok,
    /// The cycle returned an error.
    Failed,
}

impl Outcome {
    /// The stable slug.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

/// What triggered a cycle.
///
/// Derived from [`CompanyEvent`] — the vocabulary the runtime already has
/// (`runtime::cycle::cycle_trigger`) — rather than invented beside it. A third
/// parallel spelling of "what starts a turn" is how the two come to disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// An operator sent a message.
    OperatorMessage,
    /// A card was dispatched to a teammate.
    TaskDispatch,
    /// An approval was resolved and its turn continued.
    ApprovalContinuation,
    /// A teammate replied.
    AgentReply,
    /// Anything else, including an empty batch.
    Other,
}

impl Trigger {
    /// The stable slug.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OperatorMessage => "operator-message",
            Self::TaskDispatch => "task-dispatch",
            Self::ApprovalContinuation => "approval-continuation",
            Self::AgentReply => "agent-reply",
            Self::Other => OTHER,
        }
    }

    /// Classifies the first event of a cycle's batch.
    pub fn of(first: Option<&CompanyEvent>) -> Self {
        match first {
            Some(CompanyEvent::OperatorMessage { .. }) => Self::OperatorMessage,
            Some(CompanyEvent::TaskDispatched { .. }) => Self::TaskDispatch,
            Some(CompanyEvent::ApprovalResolved { .. }) => Self::ApprovalContinuation,
            Some(CompanyEvent::AgentReply { .. }) => Self::AgentReply,
            _ => Self::Other,
        }
    }
}

/// A coarse, closed classification of a failure.
///
/// **Not** [`OpenCompanyError::code`], and not `err.to_string()`, for two
/// different reasons.
///
/// `to_string()` is disqualified outright: `Display` on this error type embeds
/// absolute host paths, company ids, MCP server names, tool names, ledger slugs
/// and agent text across a dozen variants. It is the single richest source of
/// user content in the crate.
///
/// `code()` is much closer — it is a closed match onto literals — but it returns
/// an owned `String` because one variant (`Orchestration`) carries an `ORCH_*`
/// code that arrives *from the upstream runtime over the wire*. That is not a
/// vocabulary this repository controls, so it is not a vocabulary that can be
/// compiled in, and a property that can hold it is a property that can hold
/// whatever the upstream decides to send. This enum maps the same errors onto
/// a set that is fixed here.
///
/// The `_` arm is deliberate. A new error variant lands in [`Self::Other`],
/// which loses a little segmentation and leaks nothing — the correct direction
/// for a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureCode {
    /// A durable store or its filesystem refused.
    Store,
    /// A manifest or a company definition would not load.
    Manifest,
    /// The caller was refused: not granted, over budget, paused, conflicting.
    Refused,
    /// Something was not found.
    NotFound,
    /// Cognition failed — the harness, the hosted brain, or the provider.
    Cognition,
    /// A workflow was invalid.
    Workflow,
    /// Configuration was invalid or absent.
    Config,
    /// A named upstream integration returned an error.
    Upstream,
    /// Anything else, including an error variant added since this was written.
    Other,
}

impl FailureCode {
    /// The stable slug.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Manifest => "manifest",
            Self::Refused => "refused",
            Self::NotFound => "not-found",
            Self::Cognition => "cognition",
            Self::Workflow => "workflow",
            Self::Config => "config",
            Self::Upstream => "upstream",
            Self::Other => OTHER,
        }
    }

    /// Classifies an error, unwrapping a workflow wrapper first so the *cause*
    /// is reported rather than the wrapper
    /// ([`OpenCompanyError::unwrapped`](crate::error::OpenCompanyError::unwrapped)).
    ///
    /// Routed through `code()` rather than a fifty-arm match on variants:
    /// `code()` is already this crate's single classification of an error, it is
    /// exhaustive by construction, and re-deriving it here would produce a
    /// second answer that drifts from the HTTP envelope's. What this adds is the
    /// **closure** the property type needs — every code is folded onto one of
    /// nine literals owned by this file, so a code that arrived from
    /// off-process cannot reach a payload.
    pub fn of(err: &OpenCompanyError) -> Self {
        let code = err.unwrapped().code();
        // The prefixed families first: their suffix is an upstream's own code
        // and is exactly what must not be carried through.
        for family in ["tinyplace_", "tinyhumans_", "chargebee_", "paypal_"] {
            if code.starts_with(family) {
                return Self::Upstream;
            }
        }
        match code.as_str() {
            "store_error"
            | "store_io"
            | "data_read"
            | "data_parse"
            | "data_invalid"
            | "serialization_error" => Self::Store,
            "manifest_missing" | "manifest_read" | "manifest_parse" | "manifest_invalid" => {
                Self::Manifest
            }
            "tool_not_granted"
            | "budget_exceeded"
            | "workspace_quota_exceeded"
            | "workflow_run_limit"
            | "lifecycle_conflict"
            | "emergency_stop"
            | "conflict"
            | "quiescing"
            | "invalid_request" => Self::Refused,
            "not_found" | "company_not_found" | "mcp_server_not_found" => Self::NotFound,
            "openhuman_rpc" | "openhuman_process" | "openhuman_root_missing" | "harness_error" => {
                Self::Cognition
            }
            "workflow_invalid" => Self::Workflow,
            "config_error" => Self::Config,
            // `Orchestration` carries a code minted by the hosted runtime and
            // sent over the wire. It lands here, deliberately: an unrecognised
            // code costs a little segmentation and leaks nothing, which is the
            // correct direction for a default.
            _ => Self::Other,
        }
    }
}

/// The provider vocabulary an event may name.
///
/// [`UsageSample::provider`](crate::ports::usage::UsageSample::provider) is
/// **free-form**: it holds an inference kind on an inference sample, but on an
/// MCP sample it holds `mcp:<the name the operator gave their server>`, which is
/// operator-authored text and frequently names a customer or a project. So the
/// raw slug never reaches a payload. It is folded here onto a fixed list, and an
/// MCP sample reports the bare word `mcp` — that a remote tool server was used,
/// which is the segmentation anyone actually wants, without the name.
pub fn provider_slug(raw: &str) -> &'static str {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.starts_with(crate::metering::MCP_PROVIDER_PREFIX) {
        return "mcp";
    }
    match lower.as_str() {
        "openrouter" => "openrouter",
        "subscription" => "subscription",
        "managed" => "managed",
        "ollama" => "ollama",
        "byok" | "openai_compatible" => "byok",
        "echo" => "echo",
        "hosted" => "hosted",
        "github" => "github",
        "google" => "google",
        "slack" => "slack",
        "notion" => "notion",
        "composio" => "composio",
        "unknown" => "unknown",
        _ => OTHER,
    }
}

/// The stable slug for a [`SampleKind`], so a metered event can say what kind of
/// spend it was without the payload naming an agent.
pub fn sample_kind_slug(kind: SampleKind) -> &'static str {
    match kind {
        SampleKind::Inference => "inference",
        SampleKind::OauthCall => "oauth-call",
        SampleKind::SearchCall => "search-call",
        SampleKind::PlanningCall => "planning-call",
        SampleKind::TriageCall => "triage-call",
        SampleKind::SetupCall => "setup-call",
        SampleKind::AuthoringCall => "authoring-call",
        SampleKind::SelectorCall => "selector-call",
    }
}

/// The stable slug for where a cognition path's usage is metered.
pub fn metering_slug(metering: UsageMetering) -> &'static str {
    match metering {
        UsageMetering::PerTurn => "per-turn",
        UsageMetering::PerCycle => "per-cycle",
        UsageMetering::None => "none",
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// The opaque, stable id every event is attributed to (decision 2).
///
/// Two constructors and no others. There is no `From<String>`, no
/// `OpaqueId::new(&str)`, and the field is private, so a call site cannot
/// attribute an event to a company name however convenient that would be.
///
/// * [`Self::instance`] — the random 128-bit id from
///   [`crate::app::instance`], already this host's public name on `/spec`.
/// * [`Self::tenant`] — an HMAC-SHA256 of a tenant slug under a key the
///   collector does not have. Derived rather than passed through because a
///   tenant slug is usually the customer's own brand, and a customer's brand
///   does not belong in a third party's systems. A derived id keeps every
///   question analytics actually asks — uniques, funnels, segmentation,
///   retention — all of which need only that the same tenant maps to the same
///   value every time.
///
/// ## Why the tenant id is keyed, and what happens without a key
///
/// This used to be a plain `SHA-256(slug)`, and that did not deliver what the
/// paragraph above promises. A hash only hides an input that cannot be guessed,
/// and a tenant slug is close to the opposite: it is usually the customer's
/// brand, drawn from a small, public, enumerable set. Anyone holding the digests
/// — the collector itself, or anyone with access to the analytics project — can
/// hash a list of candidate brands and read `t_<digest>` straight back to the
/// customer. Truncating to 128 bits does not help; neither does a fixed salt
/// shipped in the binary, since it would be in every copy of a GPL-3.0 crate.
///
/// So the derivation is keyed with [`TenantIdKey`], which the platform injects
/// and the collector never sees. **When no key is configured there is no
/// fallback to an unkeyed digest** — the caller uses [`Self::instance`]
/// instead, whose 128 random bits are not enumerable by construction. That is
/// the safe direction: a host that cannot identify its tenant privately
/// identifies itself, rather than identifying its customer publicly. See
/// `crate::analytics::boot::install`.
#[derive(Clone, PartialEq, Eq)]
pub struct OpaqueId(String);

/// Prefixes so the two id spaces can never collide on one `distinct_id`.
const INSTANCE_PREFIX: &str = "i_";
const TENANT_PREFIX: &str = "t_";

/// How many hex characters of the tenant digest to keep. 32 characters is 128
/// bits — the same width as the instance id, and far past any collision concern
/// for a population of tenants.
const TENANT_DIGEST_HEX: usize = 32;

impl OpaqueId {
    /// Attributes events to this host's random instance id.
    pub fn instance(instance_id: &str) -> Self {
        Self(format!("{INSTANCE_PREFIX}{instance_id}"))
    }

    /// Attributes events to a hosted tenant, by keyed digest of its slug.
    ///
    /// HMAC rather than `Sha256(key || slug)`: the point of using a reviewed
    /// construction here is that nobody has to reason about whether a
    /// hand-assembled one is sound.
    pub fn tenant(tenant_slug: &str, key: &TenantIdKey) -> Self {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        // `new_from_slice` on HMAC accepts a key of any length, so this cannot
        // fail; the key is non-empty by `TenantIdKey`'s constructor anyway.
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.0.as_bytes())
            .expect("HMAC accepts a key of any length");
        mac.update(tenant_slug.as_bytes());
        let digest = mac.finalize().into_bytes();

        let mut hex = String::with_capacity(TENANT_DIGEST_HEX);
        for byte in digest.iter().take(TENANT_DIGEST_HEX / 2) {
            use fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        Self(format!("{TENANT_PREFIX}{hex}"))
    }

    /// The id as it goes on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OpaqueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Printed in full on purpose: it is opaque, it authenticates nothing,
        // and a redacted id in a log is one nobody can correlate with a support
        // conversation — which is most of what it is for.
        write!(f, "OpaqueId({})", self.0)
    }
}

/// The secret that makes a tenant's analytics id unguessable.
///
/// Held by the platform that provisions tenants and **never** by the collector,
/// which is the entire point: it is what stops whoever holds the digests from
/// enumerating candidate tenant slugs and reading customer identity back out.
///
/// A newtype rather than a bare `String`, for the same reason as
/// [`ProjectToken`](crate::analytics::config::ProjectToken): it derives neither
/// `Debug` nor `Serialize` — the hand-written `Debug` redacts — because
/// `serde_json::to_value(&some_config)` is exactly how a secret reaches a
/// payload. Nothing in this module serializes a config struct, and this value
/// is read out only inside [`OpaqueId::tenant`].
#[derive(Clone, PartialEq, Eq)]
pub struct TenantIdKey(String);

impl TenantIdKey {
    /// Wraps a key read from configuration. `None` for a blank one: a variable
    /// set to whitespace is a variable nobody meant to set, and a key that is
    /// effectively empty must read as *absent* — falling back to the random
    /// instance id — rather than as a key everyone can guess.
    pub fn new(raw: impl Into<String>) -> Option<Self> {
        let raw = raw.into();
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| Self(trimmed.to_string()))
    }
}

impl fmt::Debug for TenantIdKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TenantIdKey(<redacted>)")
    }
}

// ---------------------------------------------------------------------------
// The context envelope
// ---------------------------------------------------------------------------

/// The build capability flags an event carries, mirroring the `*_in_build`
/// booleans the setup DTO already publishes. They explain a great deal of
/// behavioural variance and cost nothing to attach.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildFlags {
    /// The embedded OpenHuman agent harness.
    pub harness: bool,
    /// MCP tool-server management.
    pub mcp: bool,
    /// The Agent Client Protocol surface.
    pub acp: bool,
    /// OAuth connection writes.
    pub oauth: bool,
    /// This very module's network transport.
    pub analytics: bool,
}

impl BuildFlags {
    /// Reads the flags off the compiled feature set.
    pub fn of_this_build() -> Self {
        Self {
            harness: cfg!(feature = "openhuman"),
            mcp: cfg!(feature = "mcp"),
            acp: cfg!(feature = "acp"),
            oauth: cfg!(feature = "oauth"),
            analytics: cfg!(feature = "analytics"),
        }
    }
}

/// The super-properties every event carries, set once at boot rather than
/// assembled at each call site.
///
/// Deliberately **not** `Serialize`: it is projected onto a transport's wire
/// shape through [`Self::props`], which yields [`PropValue`]s and therefore
/// cannot carry anything the module docs forbid. Deriving `Serialize` on a
/// struct that holds configuration is how a credential ends up in a payload —
/// see `SecretValue` (issue #1741) for the live example.
#[derive(Clone, Debug)]
pub struct Envelope {
    /// Who this instance is, opaquely.
    pub id: OpaqueId,
    /// Which kind of install it is.
    pub deployment: Deployment,
    /// The crate version.
    pub app_version: &'static str,
    /// The OS this build runs on.
    pub os: &'static str,
    /// The CPU architecture.
    pub arch: &'static str,
    /// The cognition path's stable label (`harness`, `hosted`, `echo`, …).
    pub cognition_path: &'static str,
    /// The provider that path meters under, folded onto the closed vocabulary.
    pub cognition_provider: &'static str,
    /// Where that path's usage is metered.
    pub cognition_metering: &'static str,
    /// Which optional surfaces are compiled in.
    pub build: BuildFlags,
}

impl Envelope {
    /// Builds an envelope for this process.
    pub fn new(id: OpaqueId, deployment: Deployment, cognition: Cognition) -> Self {
        Self {
            id,
            deployment,
            app_version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            cognition_path: cognition.path,
            cognition_provider: provider_slug(cognition.provider),
            cognition_metering: metering_slug(cognition.metering),
            build: BuildFlags::of_this_build(),
        }
    }

    /// Re-labels the cognition path this host is on.
    ///
    /// The envelope is built at boot, and at boot the answer can be wrong in two
    /// ways that both persist for the life of the process. A hosted host that
    /// starts with an empty registry and is provisioned through
    /// `server::provision` afterwards had no runtime to read, so it recorded the
    /// default descriptor — `custom`/`unknown` — and kept it. And a company that
    /// configures inference for the first time is *rebuilt in place* (issue
    /// #290), which moves it from `echo` to `harness` under an envelope that
    /// still says `echo`.
    ///
    /// Most recent observation wins rather than first: every company on a host
    /// shares its brain mode, so this is one process-wide fact, and the latest
    /// reading of it is the true one. Events already sent are not revised —
    /// they were right when they were sent.
    ///
    /// Still nothing but `&'static str`: [`Cognition`]'s labels are build facts
    /// and the provider goes through [`provider_slug`], so this cannot widen
    /// what a payload may carry.
    pub fn set_cognition(&mut self, cognition: Cognition) {
        self.cognition_path = cognition.path;
        self.cognition_provider = provider_slug(cognition.provider);
        self.cognition_metering = metering_slug(cognition.metering);
    }

    /// The envelope as super-properties.
    pub fn props(&self) -> Vec<Prop> {
        vec![
            ("deployment", PropValue::Word(self.deployment.as_str())),
            ("app_version", PropValue::Word(self.app_version)),
            ("os", PropValue::Word(self.os)),
            ("arch", PropValue::Word(self.arch)),
            ("cognition_path", PropValue::Word(self.cognition_path)),
            (
                "cognition_provider",
                PropValue::Word(self.cognition_provider),
            ),
            (
                "cognition_metering",
                PropValue::Word(self.cognition_metering),
            ),
            ("harness_in_build", PropValue::Flag(self.build.harness)),
            ("mcp_in_build", PropValue::Flag(self.build.mcp)),
            ("acp_in_build", PropValue::Flag(self.build.acp)),
            ("oauth_in_build", PropValue::Flag(self.build.oauth)),
            ("analytics_in_build", PropValue::Flag(self.build.analytics)),
        ]
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One analytics event.
///
/// Every field is a number, a bool, or a value from a closed vocabulary. There
/// is no variant carrying a `String`, and adding one would defeat the whole
/// module — see [`PropValue`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// The host finished booting and is serving.
    InstanceStarted {
        /// How many companies this host serves.
        companies: u64,
        /// The storage backend kind (`fs`, `sqlite`, `mongodb`).
        storage: &'static str,
        /// Whether first-run setup has been completed.
        setup_complete: bool,
    },
    /// One cycle finished — the product's unit of work.
    TurnFinished {
        /// What started it.
        trigger: Trigger,
        /// Whether it returned a report or an error.
        outcome: Outcome,
        /// The coarse failure class, `None` on success.
        failure: Option<FailureCode>,
        /// Wall-clock milliseconds from bracket open to bracket close. The only
        /// duration the runtime measures for a cycle; nothing else times one.
        duration_ms: u64,
        /// How many effects the cycle executed.
        effects_executed: u64,
        /// How many effects parked for an operator decision.
        approvals_parked: u64,
    },
    /// One usage sample was metered — tokens or a counted call.
    TurnMetered {
        /// What produced the sample.
        kind: &'static str,
        /// The provider, folded onto the closed vocabulary.
        provider: &'static str,
        /// The model, as the closed [`ModelSlug`](crate::metering::ModelSlug)
        /// vocabulary already folded it (issue #1749).
        ///
        /// A `&'static str` and not a `String` because a `ModelSlug`'s inner
        /// value is a compiled-in literal and `as_str` hands one back — so the
        /// raw model name a BYOK tenant configured cannot reach a payload even
        /// in principle, and this module needs no classifier of its own.
        ///
        /// `None` when the sample named no model: an OAuth or search call, a
        /// cognition path that cannot identify one, or a row written before
        /// [`UsageSample::model`] existed. Absent rather than folded onto
        /// [`OTHER`], so "no model ran" stays a different answer from "a model
        /// ran that this build cannot name" — the property is **omitted** from
        /// the payload rather than sent as `other` or as a null.
        model: Option<&'static str>,
        /// Prompt tokens.
        input_tokens: u64,
        /// Completion tokens.
        output_tokens: u64,
        /// Prompt tokens served from cache.
        cached_input_tokens: u64,
        /// USD attributed to the sample.
        cost_usd: f64,
        /// Whether the sample belongs to a task attempt. The attempt *id* is
        /// deliberately absent: it is a correlation key into this company's own
        /// data and buys no segmentation here.
        attributed_to_run: bool,
    },
}

impl Event {
    /// Builds a [`Self::TurnMetered`] from a sample, folding every free-form
    /// field away. This is the only constructor, so no call site can choose to
    /// pass the agent name or the raw provider through.
    pub fn metered(sample: &UsageSample) -> Self {
        Self::TurnMetered {
            kind: sample_kind_slug(sample.kind),
            provider: provider_slug(&sample.provider),
            // No `provider_slug`-style fold here on purpose: `ModelSlug` is
            // itself the closed vocabulary, classified once at the harness, and
            // `as_str` is already a `&'static str`. Re-classifying a folded
            // value would be a second place for the two lists to disagree.
            model: sample.model.map(|slug| slug.as_str()),
            input_tokens: sample.input_tokens,
            output_tokens: sample.output_tokens,
            cached_input_tokens: sample.cached_input_tokens,
            cost_usd: sample.cost_usd,
            attributed_to_run: sample.run_id.is_some(),
        }
    }

    /// The event's stable name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::InstanceStarted { .. } => "instance_started",
            Self::TurnFinished { .. } => "turn_finished",
            Self::TurnMetered { .. } => "turn_metered",
        }
    }

    /// The event's own properties, excluding the envelope's.
    pub fn props(&self) -> Vec<Prop> {
        match *self {
            Self::InstanceStarted {
                companies,
                storage,
                setup_complete,
            } => vec![
                ("companies", PropValue::Count(companies)),
                ("storage", PropValue::Word(storage)),
                ("setup_complete", PropValue::Flag(setup_complete)),
            ],
            Self::TurnFinished {
                trigger,
                outcome,
                failure,
                duration_ms,
                effects_executed,
                approvals_parked,
            } => vec![
                ("trigger", PropValue::Word(trigger.as_str())),
                ("outcome", PropValue::Word(outcome.as_str())),
                (
                    "failure",
                    PropValue::Word(failure.map_or("none", FailureCode::as_str)),
                ),
                ("duration_ms", PropValue::Count(duration_ms)),
                ("effects_executed", PropValue::Count(effects_executed)),
                ("approvals_parked", PropValue::Count(approvals_parked)),
            ],
            Self::TurnMetered {
                kind,
                provider,
                model,
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cost_usd,
                attributed_to_run,
            } => {
                let mut props = vec![
                    ("sample_kind", PropValue::Word(kind)),
                    ("provider", PropValue::Word(provider)),
                    ("input_tokens", PropValue::Count(input_tokens)),
                    ("output_tokens", PropValue::Count(output_tokens)),
                    ("cached_input_tokens", PropValue::Count(cached_input_tokens)),
                    ("cost_usd", PropValue::Amount(cost_usd)),
                    ("attributed_to_run", PropValue::Flag(attributed_to_run)),
                ];
                // Pushed only when the sample named a model. A `map_or("none",
                // …)` here would spend a vocabulary slot on the same fact the
                // property's absence already states, and an operator segmenting
                // spend by model would have to know that `none` and `other` are
                // different kinds of nothing.
                if let Some(model) = model {
                    props.push(("model", PropValue::Word(model)));
                }
                props
            }
        }
    }
}
