//! Capability-budget read surface (issue #108): the company's effective tier
//! plan and how much of each tier's token budget the current period has spent.
//!
//! A read-only companion to the harness gate — the console renders one row per
//! configured tier (budget, spend, remaining, whether its tools are disabled).
//! With no `[plan]` configured the response is `{ configured: false }` and the
//! console shows a "no token plan configured" note. The heavy lifting is the
//! pure math in [`crate::metering::capability`]; this handler only queries the
//! [`UsageMeter`](crate::ports::UsageMeter) for the period and projects it.

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde::Serialize;

use crate::AppState;
use crate::company::credentials::{CredentialSource, TinyhumansTokenSource};
use crate::company::runtime::CompanyRuntime;
use crate::metering::capability::{CapabilityPlan, tokens_in};
use crate::ports::now_millis;
use crate::server::cognition::{CognitionState, InferenceResolution, cognition_state};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the capability-budget route fragment.
pub fn router() -> Router<AppState> {
    scoped("/capabilities", get(get_status))
}

/// The company's capability-budget status as the console renders it.
///
/// When no `[plan]` is configured only `configured: false` is sent; the extra
/// fields are omitted so the console can branch on presence.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CapabilityStatusDto {
    /// Whether the company has a capability plan at all.
    configured: bool,
    /// The configured built-in tier name, if any (`null` for a bare
    /// `token_budgets` plan).
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    /// The budget window (`daily` / `monthly`).
    #[serde(skip_serializing_if = "Option::is_none")]
    period: Option<String>,
    /// Epoch-millis start of the current budget period (the spend window).
    #[serde(skip_serializing_if = "Option::is_none")]
    period_start_millis: Option<u64>,
    /// Total inference tokens spent by the company this period (the figure every
    /// tier threshold is compared against).
    #[serde(skip_serializing_if = "Option::is_none")]
    spent_tokens: Option<u64>,
    /// One row per configured tier, namespace-sorted. Omitted when unconfigured.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tiers: Vec<TierDto>,
    /// The plan-level **total token ceiling** (issue #188), when one is
    /// configured. Unlike the per-namespace `tiers` — a *soft* gate that only
    /// trims exec tools — crossing this is a *hard* stop: the harness refuses to
    /// dispatch further turns this period. Omitted when no ceiling is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<TotalDto>,
    /// Media generation (issue #109): whether this company **explicitly** grants
    /// the real-money `media` namespace (a `*` wildcard does NOT count). Sent
    /// regardless of whether a `[plan]` is configured, since media is opt-in per
    /// tool grant, not per plan.
    media_granted: bool,
    /// Whether the `media` feature is compiled into this build at all (the tools
    /// only exist under it). `false` lets the console show a "not in this build"
    /// state rather than implying a missing credential.
    media_in_build: bool,
    /// Whether a MANAGED media credential is resolvable from the environment on
    /// this build (feature on + env present). Never reflects a tenant secret.
    media_credential_configured: bool,
    /// Per-tenant Composio (issue #110): whether this company **explicitly**
    /// grants the `composio` namespace (a `*` wildcard does NOT count). Opt-in
    /// per tool grant, independent of a `[plan]`.
    composio_granted: bool,
    /// Whether the `composio` feature is compiled into this build at all.
    composio_in_build: bool,
    /// Chargebee billing (issue #788): whether this company **explicitly** grants
    /// the `chargebee` namespace (a `*` wildcard does NOT count). What the
    /// Settings UI reads to say whether billing tools would reach an agent even
    /// once credentials are saved.
    chargebee_granted: bool,
    /// Whether the `chargebee` feature is compiled into this build at all. The
    /// grant and the credentials can both be in place and still wire no tools if
    /// the running binary was not built with it.
    chargebee_in_build: bool,
    /// Whether a non-empty per-tenant Composio **BYO override** token is stored
    /// under `composio/token` — never the token itself. Unlike media's env
    /// credential, this is a tenant secret.
    ///
    /// Deliberately narrow, and **not** the answer to "can this company reach
    /// Composio" (issue #886): the BYO slot is the first of three tiers, and on
    /// a hosted tenant the third one answers, so this reads `false` for a
    /// company whose Composio tools are wired and working. Read
    /// [`Self::composio_credential_source`] for the resolution verdict; this
    /// field is retained with its original meaning for the console surface that
    /// asks whether *this company pasted a token*.
    composio_token_configured: bool,
    /// Which tier this company's Composio credential actually resolves from
    /// (issue #886) — `attested` (the instance's platform identity), `company`
    /// (the company's own TinyHumans key), `static` (a pasted BYO token or a
    /// static instance key), or `none` (nothing resolves, so no tools are
    /// wired).
    ///
    /// Sourced from
    /// [`resolve_credential`](crate::company::composio::resolve_credential) —
    /// the same derivation the toolbelt gates on — rather than a second copy of
    /// its precedence, so the console can never name a tier the agents are not
    /// on. Matches the `credentialSource` field
    /// [`ops::composio`](crate::server::ops::composio) already reports.
    ///
    /// A **resolution** verdict, not a liveness one: `attested` says a bearer
    /// can be obtained, not that Composio answered or that any account is
    /// connected. `GET …/connections` is the axis that answers those.
    ///
    /// Omitted entirely when the secret store could not be read — an unknown
    /// answer is not `none`, and reporting a confident "no credential" for a
    /// transient store hiccup is the same class of lie #886 is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    composio_credential_source: Option<CredentialSource>,
    /// Metered web search (issue #238): whether this company **explicitly**
    /// grants the `search` namespace (a `*` wildcard does NOT count).
    search_granted: bool,
    /// Whether the harness that carries `web_search` is compiled into this
    /// build. There is no `search` Cargo feature — the tool rides the plain
    /// `openhuman` harness feature deliberately, so CI's gated lane compiles and
    /// tests it rather than a real-money surface shipping untested.
    search_in_build: bool,
    /// Whether a MANAGED search credential is resolvable from the environment on
    /// this build. Never reflects a tenant secret: it answers "can this
    /// deployment search on the platform's account", which is a different
    /// question from [`search_provider`](Self::search_provider) below.
    search_credential_configured: bool,
    /// The provider this company's searches actually go to — `managed`, or the
    /// slug it configured in Settings → Search and finished configuring.
    ///
    /// Reported beside the managed flag rather than folded into it because the
    /// two can disagree in both directions: a deployment with no platform
    /// credential still searches for a company that brought its own key, and a
    /// company that selected Exa and pasted nothing is still on managed. Never
    /// the key — only the slug, which is not a secret.
    search_provider: String,
    /// The company's daily `web_search` call ceiling
    /// (`[tools].search_daily_calls`, else the built-in default). Reaching it
    /// makes the tool refuse loudly rather than return an empty result set.
    search_daily_call_cap: u32,
    /// Publishing (issue #244, panel half #1192): whether this company's grants
    /// confer `publish_artifact` — the only way a file an agent wrote becomes a
    /// deliverable.
    ///
    /// **Unlike every `*_granted` field above, a bare `*` DOES confer this.**
    /// Publishing spends nothing and reaches nothing outside the company's own
    /// board, so it rides the ordinary namespace rule rather than the
    /// opt-in-by-name rule the real-money surfaces use. Sourced from
    /// [`grants_files_or_docs`](crate::company::grants_files_or_docs), which is
    /// the same predicate `build_agent`'s `wants_files` gate calls — one
    /// derivation, so this panel cannot report a capability the toolbelt does
    /// not wire.
    ///
    /// # There is deliberately no third rung
    ///
    /// Media, Composio and search each carry a credential/config flag beside
    /// their grant, because each can be granted and still wire nothing.
    /// Publishing has neither a credential nor a store toggle: the artifact
    /// store is non-optional on the runtime ops bundle and the single
    /// production `HarnessDeps` literal always sets it, so a
    /// `artifactStoreConfigured` field could only ever serialize a hardcoded
    /// `true` for every company on every deployment — a fresh instance of
    /// exactly the always-reassuring flag issue #886 was filed about. If the
    /// store ever becomes genuinely optional in production, the burden is on
    /// adding the field back with a real derivation behind it.
    publish_granted: bool,
    /// Whether the harness that carries `publish_artifact` is compiled into this
    /// build. There is no `publish` Cargo feature — the tool rides the plain
    /// `openhuman` harness feature, exactly as
    /// [`search_in_build`](Self::search_in_build) does, so do not invent one.
    publish_in_build: bool,
    /// Whether the agent-side MCP bridge is compiled into this build (issue
    /// #567). Unlike media/composio/search this is **not** a grant question: the
    /// `/mcp/servers` management routes ship in every build, so an operator can
    /// add a server, store a token and watch it probe healthy on a build that
    /// hands agents no MCP tool at all — `registry_for_agent` is pushed onto the
    /// belt behind `#[cfg(feature = "mcp")]`. The most misleading case is a
    /// build with `openhuman` but without `mcp`: live tool discovery and health
    /// probes answer for real (they ride the harness feature), so every read in
    /// the console looks correct while no agent can call the server. `false`
    /// lets the MCP surfaces state that plainly instead of the operator finding
    /// out by asking an agent and watching nothing happen.
    mcp_in_build: bool,
    /// Whether this company's teammates can actually think, and why not when
    /// they cannot (issue #1735).
    ///
    /// The wire labels are [`CognitionState`]'s own — read them there rather
    /// than from a list here. This comment carried a hand-copied list of three
    /// and was stale within two commits of the states growing to five, which is
    /// exactly the second copy of a fact that this field exists to avoid
    /// (CodeRabbit review of PR #1740).
    ///
    /// The one capability on this response that is **not** a build fact alone.
    /// `media_in_build` and its neighbours answer "was this compiled in";
    /// cognition is that question *and* "is a harness actually attached" *and*
    /// "did a model resolve at boot", and only the last is something an
    /// operator can fix without a new binary. A fifth boolean would have
    /// collapsed them, which is the same mistake as the echo reply it exists to
    /// explain — an operator told "not available" goes looking for a rebuild
    /// when a provider was one settings page away.
    ///
    /// Derived on every read from the brain the runtime is holding, never
    /// stored. See [`crate::server::cognition`].
    cognition: CognitionState,
}

/// One tier's budget row.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TierDto {
    /// The exec tool namespace this tier gates.
    namespace: String,
    /// Tokens allowed this period.
    budget_tokens: u64,
    /// Tokens spent this period (company-wide — spend has no per-tier
    /// attribution, so this is the same across rows).
    spent_tokens: u64,
    /// `budget - spent`, saturating at zero.
    remaining_tokens: u64,
    /// Whether spend has reached the threshold — the tier's tools are disabled.
    exhausted: bool,
}

/// The plan-level total token ceiling row (issue #188).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TotalDto {
    /// Total tokens allowed this period before dispatch is refused.
    budget_tokens: u64,
    /// Tokens spent this period (the same company-wide figure the tiers compare
    /// against).
    spent_tokens: u64,
    /// `budget - spent`, saturating at zero.
    remaining_tokens: u64,
    /// Whether spend has reached the ceiling — the harness refuses to dispatch
    /// further turns until the period resets.
    exhausted: bool,
}

/// The opt-in-capability status flags carried on every response (media +
/// composio), independent of whether a `[plan]` is configured.
struct OptInFlags {
    media_granted: bool,
    chargebee_granted: bool,
    composio_granted: bool,
    composio_token_configured: bool,
    /// The resolved Composio credential tier (issue #886), or `None` when it
    /// could not be determined. Travels on the flags rather than being computed
    /// per DTO site because the DTO is built in two places, and a field wired
    /// into one of them alone reports honestly for a company with no plan and
    /// lies to every company that has one — the failure the issue #567 test
    /// below exists to catch.
    composio_credential_source: Option<CredentialSource>,
    search_granted: bool,
    search_daily_call_cap: u32,
    search_provider: String,
    /// Issue #1192. Carried on the flags rather than derived per DTO site for
    /// the reason the `composio_credential_source` note above already states:
    /// the DTO is built in two places, and a field wired into one of them alone
    /// reports honestly for a company with no plan and lies to every company
    /// that has one.
    publish_granted: bool,
    /// Issue #1735. Carried here for the reason the two notes above already
    /// give: the DTO is built in two places, and a field wired into one of them
    /// alone reports honestly for a company with no plan and lies to every
    /// company that has one — which for this field would mean chat rendering
    /// the echo brain's output as a teammate's reply on exactly the companies
    /// that have a budget configured.
    cognition: CognitionState,
}

impl OptInFlags {
    /// All-false — used when no company record is present.
    ///
    /// Takes the cognition state because that one is knowable without a record:
    /// the runtime is in hand either way, and which brain it holds does not
    /// depend on whether its company row loaded.
    fn none(cognition: CognitionState) -> Self {
        Self {
            cognition,
            media_granted: false,
            chargebee_granted: false,
            composio_granted: false,
            composio_token_configured: false,
            // `None` (undetermined), never `Some(CredentialSource::None)`:
            // there is no company record to resolve a credential for, which is
            // not the same answer as "no credential resolves".
            composio_credential_source: None,
            search_granted: false,
            search_daily_call_cap: crate::company::DEFAULT_SEARCH_DAILY_CALLS,
            search_provider: crate::company::search::MANAGED_PROVIDER.to_string(),
            publish_granted: false,
        }
    }
}

/// The unconfigured response: `{ configured: false }` plus the opt-in-capability
/// flags (media + composio are opt-in per tool grant, independent of a `[plan]`).
fn unconfigured(flags: OptInFlags) -> CapabilityStatusDto {
    CapabilityStatusDto {
        configured: false,
        plan: None,
        period: None,
        period_start_millis: None,
        spent_tokens: None,
        tiers: Vec::new(),
        total: None,
        media_granted: flags.media_granted,
        media_in_build: cfg!(feature = "media"),
        media_credential_configured: media_credential_configured(),
        composio_granted: flags.composio_granted,
        composio_in_build: cfg!(feature = "composio"),
        chargebee_granted: flags.chargebee_granted,
        chargebee_in_build: cfg!(feature = "chargebee"),
        composio_token_configured: flags.composio_token_configured,
        composio_credential_source: flags.composio_credential_source,
        search_granted: flags.search_granted,
        search_in_build: cfg!(feature = "openhuman"),
        search_credential_configured: search_credential_configured(),
        search_daily_call_cap: flags.search_daily_call_cap,
        search_provider: flags.search_provider.clone(),
        publish_granted: flags.publish_granted,
        publish_in_build: cfg!(feature = "openhuman"),
        mcp_in_build: cfg!(feature = "mcp"),
        cognition: flags.cognition,
    }
}

/// Which tier this company's Composio credential resolves from (issue #886), or
/// `None` when the secret store could not be read.
///
/// Asks
/// [`resolve_credential`](crate::company::composio::resolve_credential) rather
/// than restating its precedence. The three-tier resolution — BYO
/// `composio/token`, then the company's own TinyHumans key, then this instance's
/// platform identity — is the *same* one
/// [`TenantComposio::resolve`](crate::harness::composio::TenantComposio::resolve)
/// gates the toolbelt on, and the whole point of #886 is that this panel had a
/// second, one-tier copy of the question that disagreed with it. There must be
/// exactly one derivation, and this is not it — it is a caller of it.
///
/// Takes the instance identity **already resolved** rather than an `&dyn
/// EnvSource`, mirroring
/// [`ops::composio`](crate::server::ops::composio)'s `credential_source_for`: a
/// trait object with no `Send + Sync` bound held across the await below makes
/// the whole handler future non-`Send`, which axum rejects. Passing the resolved
/// value also keeps the tier matrix testable without mutating the process
/// environment.
///
/// A store error yields `None` and a warning, never `Some(CredentialSource::None)`.
/// The rest of `/capabilities` is budget and tier data with nothing to do with
/// Composio, so failing the whole response would be the wrong trade — but
/// answering "no credential" for a transient hiccup would send an operator to
/// paste a token they already have, which is the #886 failure in the other
/// direction. An omitted field is the only honest "we do not know".
async fn composio_credential_source(
    runtime: &CompanyRuntime,
    token_source: Option<std::sync::Arc<TinyhumansTokenSource>>,
) -> Option<CredentialSource> {
    match crate::company::composio::resolve_credential(
        runtime.id(),
        runtime.secrets().as_ref(),
        token_source,
    )
    .await
    {
        Ok(credential) => Some(credential.source()),
        Err(err) => {
            tracing::warn!(
                company = %runtime.id(),
                error = %err,
                "[capabilities] could not resolve the Composio credential tier; omitting \
                 `composioCredentialSource` rather than reporting a confident `none`"
            );
            None
        }
    }
}

/// Whether a MANAGED search credential (issue #238) is resolvable from the
/// environment on this build. Env-only, never a tenant secret, matching the
/// harness's fail-closed resolution. Off the harness feature this is always
/// `false` — there is no agent to search with.
fn search_credential_configured() -> bool {
    #[cfg(feature = "openhuman")]
    {
        use crate::app::config::ProcessEnv;
        crate::harness::provider::search_backend_from_env(&ProcessEnv).is_some()
    }
    #[cfg(not(feature = "openhuman"))]
    {
        false
    }
}

/// Whether a MANAGED media credential (issue #109) is resolvable from the
/// environment on this build. `true` only under the `media` feature with a
/// credential present — env-only, never a tenant secret, matching the harness's
/// fail-closed resolution. Off the feature this is always `false`.
fn media_credential_configured() -> bool {
    #[cfg(feature = "media")]
    {
        use crate::app::config::ProcessEnv;
        crate::harness::provider::media_backend_from_env(&ProcessEnv).is_some()
    }
    #[cfg(not(feature = "media"))]
    {
        false
    }
}

/// Reads this company's cognition state off the runtime it actually holds.
///
/// The third input is only consulted on the one degraded path, and that is
/// deliberate rather than incidental: `/capabilities` is a console read that
/// gets polled, and `inference_resolution` costs a manifest load plus a
/// secret-store resolve. A company that is thinking pays neither, because its
/// brain answers the question before either is needed. The placeholder passed
/// in the other arm is never read — `cognition_state` has already returned by
/// then, and its ordering is asserted.
async fn cognition_for(runtime: &CompanyRuntime) -> CognitionState {
    let path = runtime.cognition().path;
    let harness_reachable = crate::server::ops::inference::harness_reachable(runtime);
    // Short-circuit: with a real brain, or with no harness at all, the config
    // read cannot change the answer — see `cognition_state`'s own ordering.
    let resolution = if path == crate::ports::brain::ECHO_PATH && harness_reachable {
        crate::server::ops::inference::inference_resolution(runtime).await
    } else {
        InferenceResolution::Nothing
    };
    cognition_state(path, harness_reachable, resolution)
}

/// Resolves the capability-budget status DTO for a company.
async fn effective_status(runtime: &CompanyRuntime) -> Result<CapabilityStatusDto, ApiError> {
    // Issue #1735. Read off the brain this runtime is actually holding, before
    // anything else can fail: a company whose record will not load still has a
    // brain, and "can a teammate answer me" is the one question on this
    // response that must never degrade to a reassuring default.
    //
    // Reachability, not `cfg!(feature = "openhuman")`: the feature says the
    // harness was compiled in, not that this runtime was handed a pool, and an
    // embedder that skips `app::harness::attach` gets exactly that (the shipped
    // desktop-shell bug that module exists to end). Reporting `unconfigured`
    // there would point the operator at Settings → Inference, which cannot move
    // that runtime off the echo brain — the dead end `ops::inference`'s own
    // `restart_pending`/`runner_gap_for` already gate on this same predicate to
    // avoid (issues #266, #514). Borrowing that function rather than re-deriving
    // it is what keeps the two surfaces from disagreeing about one company.
    let cognition = cognition_for(runtime).await;
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let Some(record) = record else {
        return Ok(unconfigured(OptInFlags::none(cognition)));
    };
    // Media + composio are opt-in per tool grant (explicit namespace, never `*`)
    // and live on the manifest regardless of whether a `[plan]` is configured.
    let flags = OptInFlags {
        cognition,
        media_granted: crate::company::grants_media_explicit(&record.manifest.tools.allow),
        chargebee_granted: crate::company::grants_chargebee_explicit(&record.manifest.tools.allow),
        composio_granted: crate::company::grants_composio_explicit(&record.manifest.tools.allow),
        // Degrade to "unconfigured" on a transient secret-store error rather
        // than failing the whole /capabilities response (budget/tier data is
        // unrelated to Composio). Mirrors other opt-in-credential probes.
        composio_token_configured: crate::company::composio::token_configured(
            runtime.id(),
            runtime.secrets().as_ref(),
        )
        .await
        .unwrap_or(false),
        // Issue #886: the field above answers only whether a BYO token was
        // pasted, which on a hosted tenant is `false` for a company whose
        // Composio tools work. This one asks the resolver what the toolbelt
        // will actually present.
        //
        // The instance identity is read straight from the process environment
        // here, as `ops::composio` and `ops::company_key` already do. It would
        // be better held once on `CompanyRuntime` — this is the fourth
        // `from_env` call site on a console read path — but inventing that
        // accessor is a wider change than this fix, so it is left as a
        // follow-up rather than half-done here.
        composio_credential_source: composio_credential_source(
            runtime,
            TinyhumansTokenSource::from_env(&crate::app::config::ProcessEnv)
                .map(std::sync::Arc::new),
        )
        .await,
        // Issue #238: search is opt-in per tool grant like media/composio, and
        // its daily cap lives on `[tools]` rather than `[plan]` — a call
        // ceiling, not a token budget — so both travel with the plan-independent
        // flags.
        search_granted: crate::company::grants_search_explicit(&record.manifest.tools.allow),
        search_daily_call_cap: record
            .manifest
            .tools
            .search_daily_calls
            .unwrap_or(crate::company::DEFAULT_SEARCH_DAILY_CALLS),
        // Which provider the company's own settings point at. Degrades to the
        // managed answer on a transient secret-store error rather than failing
        // the whole /capabilities response, like the Composio probe above — the
        // panel's other cards are unrelated to search.
        search_provider: crate::company::search::resolve_effective_provider(
            runtime.id(),
            runtime.secrets().as_ref(),
        )
        .await
        .unwrap_or_else(|_| crate::company::search::MANAGED_PROVIDER.to_string()),
        // Issue #245: opt-in per tool grant like the three above, and read from
        // the same manifest field, so the repositories card can tell an operator
        // which half of the setup is missing.
        // Issue #1192: the same predicate `build_agent`'s `wants_files` gate
        // calls, so the panel's verdict and the wired toolbelt cannot disagree.
        // Note the shape difference from its four neighbours above — this one is
        // NOT `_explicit`, because a bare `*` confers publishing.
        publish_granted: crate::company::grants_files_or_docs(&record.manifest.tools.allow),
    };
    let manifest_plan = &record.manifest.plan;
    let Some(plan) = CapabilityPlan::from_manifest(manifest_plan) else {
        return Ok(unconfigured(flags));
    };

    let now = now_millis();
    let since = plan.period.period_start_millis(now);
    let samples = runtime
        .usage()
        .query(runtime.id(), since)
        .await
        .map_err(ApiError)?;
    let spent = tokens_in(&samples);

    let tiers = plan
        .status(spent)
        .into_iter()
        .map(|tier| TierDto {
            namespace: tier.namespace,
            budget_tokens: tier.budget,
            spent_tokens: tier.spent,
            remaining_tokens: tier.remaining,
            exhausted: tier.exhausted,
        })
        .collect();

    // The plan-level total ceiling (issue #188): present only when the manifest
    // set `[plan].total_tokens`. This is the hard gate the harness enforces by
    // refusing dispatch — the console renders it alongside the soft per-namespace
    // tiers.
    let total = plan.total_status(spent).map(|status| TotalDto {
        budget_tokens: status.budget,
        spent_tokens: status.spent,
        remaining_tokens: status.remaining,
        exhausted: status.exhausted,
    });

    Ok(CapabilityStatusDto {
        configured: true,
        plan: manifest_plan.name.clone(),
        period: Some(plan.period.as_str().to_string()),
        period_start_millis: Some(since),
        spent_tokens: Some(spent),
        tiers,
        total,
        media_granted: flags.media_granted,
        media_in_build: cfg!(feature = "media"),
        media_credential_configured: media_credential_configured(),
        composio_granted: flags.composio_granted,
        composio_in_build: cfg!(feature = "composio"),
        chargebee_granted: flags.chargebee_granted,
        chargebee_in_build: cfg!(feature = "chargebee"),
        composio_token_configured: flags.composio_token_configured,
        composio_credential_source: flags.composio_credential_source,
        search_granted: flags.search_granted,
        search_in_build: cfg!(feature = "openhuman"),
        search_credential_configured: search_credential_configured(),
        search_daily_call_cap: flags.search_daily_call_cap,
        search_provider: flags.search_provider.clone(),
        publish_granted: flags.publish_granted,
        publish_in_build: cfg!(feature = "openhuman"),
        mcp_in_build: cfg!(feature = "mcp"),
        cognition: flags.cognition,
    })
}

/// `GET …/capabilities` — the company's capability-budget status.
async fn get_status(company: ScopedCompany) -> Result<Json<CapabilityStatusDto>, ApiError> {
    Ok(Json(effective_status(company.runtime.as_ref()).await?))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::{CredentialSource, TinyhumansTokenSource};
    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::ports::usage::{SampleKind, UsageSample};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-capabilities-")
            .tempdir()
            .expect("tempdir")
    }

    async fn state_with_manifest(home: &std::path::Path, manifest_toml: &str) -> AppState {
        state_with(home, manifest_toml, None).await
    }

    /// [`state_with_manifest`], optionally over a caller-supplied
    /// [`SecretStore`](crate::ports::SecretStore) — the seam the issue #886
    /// store-error case needs, since an unreadable store is the one input the
    /// filesystem-backed default cannot produce.
    async fn state_with(
        home: &std::path::Path,
        manifest_toml: &str,
        secrets: Option<std::sync::Arc<dyn crate::ports::SecretStore>>,
    ) -> AppState {
        use crate::ports::CompanyStore;
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .unwrap();
        let mut builder = RuntimeBuilder::new(home.to_path_buf(), manifest).with_id(id.clone());
        if let Some(secrets) = secrets {
            builder = builder.with_secrets(secrets);
        }
        let runtime = builder.build().await.unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn get_capabilities(state: &AppState) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri("/api/v1/company/capabilities")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// Issue #1735: a build whose runtime is on the offline echo brain must
    /// never report itself as able to think — with or without a `[plan]`, since
    /// the DTO is built in two places.
    ///
    /// The runtimes `state_with_manifest` builds never call
    /// `app::harness::attach`, so **no pool is attached in either lane** and the
    /// honest answer is `unavailable` in both: nothing an operator saves in
    /// Settings → Inference reaches a harness that was never wired. This read
    /// `unconfigured` under `openhuman` while the state was derived from
    /// `cfg!` alone — a settings link offered on a runtime it could not help
    /// (codex review of PR #1740). The lane-independent expectation is the
    /// point: the answer turns on what this runtime holds, not on which lane
    /// compiled it.
    #[tokio::test]
    async fn a_company_on_the_echo_brain_never_reports_itself_configured() {
        let expected = "unavailable";

        // No `[plan]` — the `unconfigured()` construction site.
        let home_a_dir = home();
        let state = state_with_manifest(
            home_a_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;
        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["configured"], false, "{dto}");
        assert_eq!(
            dto["cognition"], expected,
            "a runtime built with no inference source runs the echo brain: {dto}"
        );

        // With a `[plan]` — the other construction site. A field wired into one
        // of them alone reports honestly here and lies to every company that
        // has a budget configured, which is the trap this file already warns
        // about twice.
        let home_b_dir = home();
        let state_b = state_with_manifest(
            home_b_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[plan]\nname = \"starter\"\nperiod = \"daily\"\ntotal_tokens = 1000\n",
        )
        .await;
        let (status_b, dto_b) = get_capabilities(&state_b).await;
        assert_eq!(status_b, StatusCode::OK);
        assert_eq!(dto_b["configured"], true, "{dto_b}");
        assert_eq!(
            dto_b["cognition"], expected,
            "the plan-configured construction site must carry the same answer: {dto_b}"
        );
    }

    /// A harness pool *is* attached and the company still resolved no inference
    /// source, so the echo brain won — the one state where Settings → Inference
    /// is a real remedy (issue #1735).
    ///
    /// The counterpart to the test above, and the pair is what pins the
    /// distinction codex's review turned on: same echo brain, same manifest,
    /// same lane, and the answer flips on whether a pool was attached. Gated on
    /// `openhuman` because `with_harness` — and the very idea of an attached
    /// pool — only exists under it; `feature-lanes.txt` records that lane as
    /// `tested`, so this runs rather than merely compiling.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn an_attached_harness_with_no_inference_is_a_settings_problem() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let manifest_toml = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n";
        // Seeds the record and a harness-less runtime, then replaces the
        // runtime with one holding a pool over the same record — so the only
        // difference from the test above is the attached harness.
        let state = state_with_manifest(&home, manifest_toml).await;
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let id = CompanyId::new("acme");
        let runtime = RuntimeBuilder::new(home, manifest)
            .with_id(id.clone())
            .with_harness(std::sync::Arc::new(crate::harness::HarnessPool::new()))
            .build()
            .await
            .unwrap();
        // The premise: no inference source resolved, so this is still the echo
        // brain. Asserted rather than assumed — if a future default put a brain
        // behind it, the `unconfigured` below would pass for the wrong reason.
        assert_eq!(
            runtime.cognition().path,
            crate::ports::brain::ECHO_PATH,
            "no inference configured, so the runtime must still be on the echo brain",
        );
        state.registry().insert(id, std::sync::Arc::new(runtime));

        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            dto["cognition"], "unconfigured",
            "a pool is attached, so a provider really is one settings page away: {dto}"
        );
    }

    /// A harness is attached and the config cannot be **read** — which is not
    /// the same as nothing being configured (codex review of PR #1740).
    ///
    /// `ops::inference` already refuses this promise from the other side: its
    /// `unreadable_inference_config_is_not_restartable` regression builds this
    /// exact runtime — reachable harness over a failing `SecretStore` — and
    /// asserts `RunnerGap::NotWired`, "not `InferenceRequired`", because saving
    /// cannot resolve a configuration the host cannot read. Chat pointing that
    /// same operator at Settings → Inference would make, on the same runtime,
    /// the promise the workflow-run route declines to make.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn an_unreadable_inference_config_is_not_reported_as_unconfigured() {
        use crate::ports::types::SecretValue;

        struct FailingSecrets;
        #[async_trait::async_trait]
        impl crate::ports::SecretStore for FailingSecrets {
            async fn get(&self, _c: &CompanyId, _key: &str) -> crate::Result<Option<SecretValue>> {
                Err(crate::error::OpenCompanyError::Store(
                    "secret store unreachable".into(),
                ))
            }
            async fn set(&self, _c: &CompanyId, _key: &str, _v: SecretValue) -> crate::Result<()> {
                Ok(())
            }
        }

        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let manifest_toml = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n";
        let state = state_with_manifest(&home, manifest_toml).await;
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let id = CompanyId::new("acme");
        let runtime = RuntimeBuilder::new(home, manifest)
            .with_id(id.clone())
            .with_harness(std::sync::Arc::new(crate::harness::HarnessPool::new()))
            .with_secrets(std::sync::Arc::new(FailingSecrets))
            .build()
            .await
            .unwrap();
        assert_eq!(
            runtime.cognition().path,
            crate::ports::brain::ECHO_PATH,
            "an unresolvable config leaves the runtime on the echo brain",
        );
        state.registry().insert(id, std::sync::Arc::new(runtime));

        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            dto["cognition"], "undetermined",
            "the host cannot read the config, so it must not name a remedy: {dto}"
        );
        assert_ne!(
            dto["cognition"], "unconfigured",
            "that would promise a settings page `runner_gap_for` refuses to promise: {dto}"
        );
    }

    /// A provider is saved and resolves, and the company is still on the echo
    /// brain because its runtime predates the save (codex review of PR #1740).
    ///
    /// The likeliest route to the echo brain in practice, and the one where
    /// getting it wrong is rudest: the operator followed this very banner's
    /// link, chose a provider, saved it — and `unconfigured` would send them
    /// back to that page to redo work they did correctly. `ops::inference`
    /// calls this same state `restartRequired` (issue #266).
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_saved_provider_awaiting_a_restart_reports_restart_required() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        // A manifest `[inference]` section resolves without a secret write, so
        // the config is set *before* the runtime is built and the runtime still
        // ends up on the echo brain — the same shape as a console save landing
        // after boot, without needing to rebuild anything mid-test.
        let manifest_toml = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[inference]\nprovider = \"ollama\"\n";
        let state = state_with_manifest(&home, manifest_toml).await;
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let id = CompanyId::new("acme");
        // Built with a pool but with the brain forced to echo, which is exactly
        // what a runtime that predates the save is holding.
        let runtime = RuntimeBuilder::new(home, manifest)
            .with_id(id.clone())
            .with_harness(std::sync::Arc::new(crate::harness::HarnessPool::new()))
            .with_brain(std::sync::Arc::new(crate::brain::EchoBrain))
            .build()
            .await
            .unwrap();
        assert_eq!(runtime.cognition().path, crate::ports::brain::ECHO_PATH);
        state.registry().insert(id, std::sync::Arc::new(runtime));

        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            dto["cognition"], "restart-required",
            "a provider resolves, so the remedy is a restart, not another choice: {dto}"
        );
        assert_ne!(
            dto["cognition"], "unconfigured",
            "that would send the operator back to the page they just came from: {dto}"
        );
    }

    /// The other direction, so the field is not a constant: a runtime holding a
    /// brain that is not the echo brain reports `configured`.
    #[tokio::test]
    async fn a_company_with_a_real_brain_reports_configured() {
        use crate::ports::brain::{Brain, CycleHost};
        use crate::ports::types::{CycleRequest, CycleResult};

        /// A brain that does nothing but exist. It reports the default
        /// `Cognition` (path `custom`), which is what any embedder-injected
        /// brain reports — cognition of a kind this crate cannot name, but
        /// cognition all the same.
        struct InjectedBrain;

        #[async_trait::async_trait]
        impl Brain for InjectedBrain {
            async fn run_cycle(
                &self,
                _req: CycleRequest,
                _host: &dyn CycleHost,
            ) -> crate::Result<CycleResult> {
                Ok(CycleResult {
                    channel_responses: Vec::new(),
                    new_traces: Vec::new(),
                    ledger_deltas: Vec::new(),
                    token_usage: Default::default(),
                })
            }
        }

        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let manifest_toml = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n";
        // Seeds the company record and an echo-brain runtime; the insert below
        // replaces that runtime with one holding a real brain, over the same
        // record — so the only thing that differs from the test above is the
        // brain, which is the point.
        let state = state_with_manifest(&home, manifest_toml).await;
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let id = CompanyId::new("acme");
        let runtime = RuntimeBuilder::new(home, manifest)
            .with_id(id.clone())
            .with_brain(std::sync::Arc::new(InjectedBrain))
            .build()
            .await
            .unwrap();
        state.registry().insert(id, std::sync::Arc::new(runtime));

        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["cognition"], "configured", "{dto}");
    }

    #[tokio::test]
    async fn reports_unconfigured_without_a_plan() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["configured"], false);
        assert!(
            dto.get("tiers").is_none(),
            "no tiers when unconfigured: {dto}"
        );
        assert!(dto.get("plan").is_none());
    }

    /// Media generation (issue #109): the route surfaces `mediaGranted` from the
    /// manifest tool grants (explicit `media`, never `*`), even with no `[plan]`.
    #[tokio::test]
    async fn reports_media_granted_from_explicit_grant_only() {
        // Explicit `media` grant, no `[plan]` → unconfigured but mediaGranted.
        let home_a_dir = home();
        let home_a = home_a_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home_a,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"media\"]\n",
        )
        .await;
        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["configured"], false);
        assert_eq!(dto["mediaGranted"], true, "{dto}");
        // The flags are always present so the console can render every state.
        assert!(dto.get("mediaInBuild").is_some(), "{dto}");
        assert!(dto.get("mediaCredentialConfigured").is_some(), "{dto}");

        // A `*` wildcard grant must NOT count as a media grant.
        let home_b_dir = home();
        let home_b = home_b_dir.path().to_path_buf();
        let state2 = state_with_manifest(
            &home_b,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"*\"]\n",
        )
        .await;
        let (_, dto2) = get_capabilities(&state2).await;
        assert_eq!(
            dto2["mediaGranted"], false,
            "the `*` wildcard must not grant the real-money media family: {dto2}"
        );
    }

    /// Per-tenant Composio (issue #110): the route surfaces `composioGranted`
    /// from the explicit grant (never `*`) and the trio flags, even with no
    /// `[plan]`.
    #[tokio::test]
    async fn reports_composio_flags_from_explicit_grant_only() {
        let home_a_dir = home();
        let home_a = home_a_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home_a,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n",
        )
        .await;
        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["composioGranted"], true, "{dto}");
        assert_eq!(dto["composioTokenConfigured"], false, "no token yet: {dto}");
        assert!(dto.get("composioInBuild").is_some(), "{dto}");

        // A `*` wildcard grant must NOT count as a composio grant.
        let home_b_dir = home();
        let home_b = home_b_dir.path().to_path_buf();
        let state2 = state_with_manifest(
            &home_b,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"*\"]\n",
        )
        .await;
        let (_, dto2) = get_capabilities(&state2).await;
        assert_eq!(
            dto2["composioGranted"], false,
            "the `*` wildcard must not grant composio: {dto2}"
        );
    }

    /// Metered web search (issue #238): the route surfaces `searchGranted` from
    /// the explicit grant (never `*`) and the company's daily call cap, even
    /// with no `[plan]` — the cap is a call ceiling on `[tools]`, not a token
    /// budget on `[plan]`.
    #[tokio::test]
    async fn reports_search_flags_from_explicit_grant_only() {
        let home_a_dir = home();
        let home_a = home_a_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home_a,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"search\"]\nsearch_daily_calls = 25\n",
        )
        .await;
        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["searchGranted"], true, "{dto}");
        assert_eq!(dto["searchDailyCallCap"], 25, "{dto}");
        assert!(dto.get("searchInBuild").is_some(), "{dto}");
        assert!(dto.get("searchCredentialConfigured").is_some(), "{dto}");

        // A `*` wildcard grant must NOT count as a search grant — every call is
        // a priced request, so it can never ride in on the wildcard a company
        // set for its file and shell tools.
        let home_b_dir = home();
        let home_b = home_b_dir.path().to_path_buf();
        let state2 = state_with_manifest(
            &home_b,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"*\"]\n",
        )
        .await;
        let (_, dto2) = get_capabilities(&state2).await;
        assert_eq!(
            dto2["searchGranted"], false,
            "the `*` wildcard must not grant metered search: {dto2}"
        );
        assert_eq!(
            dto2["searchDailyCallCap"],
            crate::company::DEFAULT_SEARCH_DAILY_CALLS,
            "an unset cap reports the built-in default: {dto2}"
        );
    }

    /// Issue #567: the MCP bridge's build state travels on every response, with
    /// a `[plan]` and without one. The `/mcp/servers` management routes ship in
    /// every build while the agent-side registry is pushed onto the belt behind
    /// `#[cfg(feature = "mcp")]`, so without this flag a console cannot tell a
    /// deployment that will honour a server from one that never can — the
    /// operator finds out by asking an agent and watching nothing happen.
    ///
    /// Asserted on **both** response paths deliberately: the DTO is built in two
    /// places (`unconfigured` and the configured branch), so a flag added to one
    /// alone would report honestly for a company with no plan and lie to every
    /// company that has one.
    #[tokio::test]
    async fn reports_whether_the_mcp_bridge_is_in_this_build() {
        let unplanned_dir = home();
        let unplanned = unplanned_dir.path().to_path_buf();
        let state = state_with_manifest(
            &unplanned,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"*\"]\n",
        )
        .await;
        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["configured"], false);
        assert_eq!(
            dto["mcpInBuild"],
            cfg!(feature = "mcp"),
            "the unconfigured response states the bridge's build state: {dto}"
        );

        let planned_dir = home();
        let planned = planned_dir.path().to_path_buf();
        let state2 = state_with_manifest(
            &planned,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[plan]\nname = \"starter\"\n",
        )
        .await;
        let (_, dto2) = get_capabilities(&state2).await;
        assert_eq!(dto2["configured"], true, "{dto2}");
        assert_eq!(
            dto2["mcpInBuild"],
            cfg!(feature = "mcp"),
            "a configured plan reports the same build state: {dto2}"
        );

        // The without-feature path is the one the console must not misreport:
        // pinned as a literal so the honest answer cannot regress into a
        // vacuously-true comparison against the same `cfg!`.
        #[cfg(not(feature = "mcp"))]
        {
            assert_eq!(
                dto["mcpInBuild"], false,
                "a build without the bridge must say so: {dto}"
            );
            assert_eq!(dto2["mcpInBuild"], false, "{dto2}");
        }
        #[cfg(feature = "mcp")]
        {
            assert_eq!(
                dto["mcpInBuild"], true,
                "a build with the bridge must say so: {dto}"
            );
            assert_eq!(dto2["mcpInBuild"], true, "{dto2}");
        }
    }

    #[tokio::test]
    async fn reports_tiers_and_exhaustion_for_a_configured_plan() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[plan]\nname = \"starter\"\n",
        )
        .await;

        // Seed 250k inference tokens into the company meter — past starter's 200k.
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        runtime
            .usage()
            .record(
                &id,
                &UsageSample {
                    at_millis: crate::ports::now_millis(),
                    agent: "ceo".into(),
                    provider: "managed".into(),
                    input_tokens: 200_000,
                    output_tokens: 50_000,
                    cached_input_tokens: 0,
                    cost_usd: 0.0,
                    kind: SampleKind::Inference,
                    run_id: None,
                    model: None,
                },
            )
            .await
            .unwrap();

        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["configured"], true);
        assert_eq!(dto["plan"], "starter");
        assert_eq!(dto["period"], "daily");
        assert_eq!(dto["spentTokens"], 250_000);

        let tiers = dto["tiers"].as_array().expect("tiers present");
        assert_eq!(tiers.len(), 2, "starter budgets shell + code: {dto}");
        for tier in tiers {
            assert_eq!(tier["budgetTokens"], 200_000);
            assert_eq!(tier["spentTokens"], 250_000);
            assert_eq!(tier["remainingTokens"], 0);
            assert_eq!(tier["exhausted"], true);
        }
        // No `[plan].total_tokens` → the hard-ceiling row is absent.
        assert!(
            dto.get("total").is_none(),
            "no total ceiling configured: {dto}"
        );
    }

    /// The plan-level total token ceiling (issue #188) surfaces its own `total`
    /// row — budget, spend, remaining, exhausted — alongside the per-namespace
    /// tiers, and reports `exhausted` once period spend crosses it.
    #[tokio::test]
    async fn reports_total_ceiling_row_when_configured() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[plan]\nname = \"starter\"\ntotal_tokens = 300000\n",
        )
        .await;

        // Seed 250k inference tokens — under the 300k total ceiling.
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        runtime
            .usage()
            .record(
                &id,
                &UsageSample {
                    at_millis: crate::ports::now_millis(),
                    agent: "ceo".into(),
                    provider: "managed".into(),
                    input_tokens: 200_000,
                    output_tokens: 50_000,
                    cached_input_tokens: 0,
                    cost_usd: 0.0,
                    kind: SampleKind::Inference,
                    run_id: None,
                    model: None,
                },
            )
            .await
            .unwrap();

        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        let total = &dto["total"];
        assert!(total.is_object(), "total ceiling row present: {dto}");
        assert_eq!(total["budgetTokens"], 300_000);
        assert_eq!(total["spentTokens"], 250_000);
        assert_eq!(total["remainingTokens"], 50_000);
        assert_eq!(total["exhausted"], false, "250k < 300k is under budget");
    }

    // ---- issue #886: the Composio verdict comes from the resolver -----------

    const GRANTS_COMPOSIO: &str =
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n";

    /// A store whose reads always fail — the transient-hiccup case, mirroring
    /// `company_key`'s own fixture.
    struct BrokenSecrets;

    #[async_trait::async_trait]
    impl crate::ports::SecretStore for BrokenSecrets {
        async fn get(
            &self,
            _c: &CompanyId,
            _key: &str,
        ) -> crate::Result<Option<crate::ports::types::SecretValue>> {
            Err(crate::error::OpenCompanyError::Store("boom".into()))
        }
        async fn set(
            &self,
            _c: &CompanyId,
            _key: &str,
            _value: crate::ports::types::SecretValue,
        ) -> crate::Result<()> {
            Err(crate::error::OpenCompanyError::Store("boom".into()))
        }
    }

    /// The instance identity the platform hands a hosted pod. Built directly
    /// rather than through `from_env` so the tier matrix never touches the
    /// process environment.
    fn platform_identity() -> std::sync::Arc<TinyhumansTokenSource> {
        std::sync::Arc::new(TinyhumansTokenSource::projected_file(
            "/var/run/secrets/tinyhumans.ai/token",
        ))
    }

    /// The whole of issue #886 in one test: the panel's Composio verdict must
    /// walk **all three** credential tiers, not just the BYO slot.
    ///
    /// The hosted case is the one that was wrong. Nobody pastes a
    /// `composio/token` on a hosted tenant — the pod's platform identity
    /// answers, the toolbelt wires up, the agents call `GITHUB_*` — and the
    /// old one-tier probe called that `false`, sending an operator looking for
    /// a missing credential that was never missing.
    #[tokio::test]
    async fn the_composio_verdict_walks_every_credential_tier() {
        use crate::company::{company_key, composio};

        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, GRANTS_COMPOSIO).await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let secrets = runtime.secrets().clone();

        // Nothing stored and no instance identity — fail closed, and say so.
        assert_eq!(
            super::composio_credential_source(runtime.as_ref(), None).await,
            Some(CredentialSource::None),
            "with no tier able to answer, `none` is the honest verdict"
        );

        // The hosted shape: nothing stored, the pod's projected identity
        // answers. This is the reported bug.
        assert_eq!(
            super::composio_credential_source(runtime.as_ref(), Some(platform_identity())).await,
            Some(CredentialSource::Attested),
        );
        assert!(
            !composio::token_configured(runtime.id(), secrets.as_ref())
                .await
                .unwrap(),
            "and the BYO slot is empty in exactly that case — the two fields \
             answer different questions, which is why the panel needs both"
        );

        // The company's own TinyHumans key outranks the instance identity.
        company_key::store_key(runtime.id(), secrets.as_ref(), "th_company")
            .await
            .unwrap();
        assert_eq!(
            super::composio_credential_source(runtime.as_ref(), Some(platform_identity())).await,
            Some(CredentialSource::Company),
        );

        // A pasted BYO token outranks everything.
        composio::store_token(runtime.id(), secrets.as_ref(), "cmp_byo")
            .await
            .unwrap();
        assert_eq!(
            super::composio_credential_source(runtime.as_ref(), Some(platform_identity())).await,
            Some(CredentialSource::Static),
        );
    }

    /// The gate. The DTO's verdict must **equal what the resolver says**, not a
    /// value this route computed for itself.
    ///
    /// Asserted as an equality against a live `resolve_credential` call rather
    /// than against a literal, deliberately: a literal would be satisfied by a
    /// second hardcoded copy of the precedence living in this file, and a second
    /// copy is the entire defect. Issue #586 removed one from the sibling status
    /// route; #886 is the one it missed here.
    ///
    /// Run across the tiers a store can produce on its own, so the equality is
    /// exercised with more than one answer.
    #[tokio::test]
    async fn the_dto_reports_exactly_what_the_resolver_resolves() {
        use crate::company::{company_key, composio};

        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(&home, GRANTS_COMPOSIO).await;
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let secrets = runtime.secrets().clone();

        // The route reads the instance identity from the process environment,
        // so the expectation must be derived from the same place — otherwise
        // this asserts against the test host's env rather than against the
        // resolver.
        let resolver_says = || async {
            composio::resolve_credential(
                runtime.id(),
                secrets.as_ref(),
                TinyhumansTokenSource::from_env(&crate::app::config::ProcessEnv)
                    .map(std::sync::Arc::new),
            )
            .await
            .unwrap()
            .source()
        };

        for label in ["nothing stored", "company key", "byo token"] {
            match label {
                "company key" => {
                    company_key::store_key(runtime.id(), secrets.as_ref(), "th_company")
                        .await
                        .unwrap()
                }
                "byo token" => composio::store_token(runtime.id(), secrets.as_ref(), "cmp_byo")
                    .await
                    .unwrap(),
                _ => {}
            }
            let (status, dto) = get_capabilities(&state).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                dto["composioCredentialSource"],
                resolver_says().await.as_str(),
                "the panel must never name a tier the toolbelt is not on ({label}): {dto}"
            );
        }
    }

    /// Both DTO construction sites carry the field — `unconfigured()` and the
    /// configured branch — per the issue #567 precedent above. A field wired
    /// into one alone reports honestly for a company with no plan and lies to
    /// every company that has one.
    ///
    /// The legacy `composioTokenConfigured` is pinned alongside it, keeping its
    /// original narrow meaning: `false` with no BYO token, `true` with one.
    /// Nothing about #886 changes what that field answers — only what the
    /// console reads for the question it was being misused for.
    #[tokio::test]
    async fn both_response_paths_carry_the_credential_tier() {
        use crate::company::composio;

        for manifest in [
            GRANTS_COMPOSIO,
            &format!("{GRANTS_COMPOSIO}[plan]\nname = \"starter\"\n"),
        ] {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let state = state_with_manifest(&home, manifest).await;
            let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

            let (_, dto) = get_capabilities(&state).await;
            assert!(
                dto.get("composioCredentialSource").is_some(),
                "every response states the resolved tier: {dto}"
            );
            assert_eq!(
                dto["composioTokenConfigured"], false,
                "no BYO token pasted yet: {dto}"
            );

            composio::store_token(runtime.id(), runtime.secrets().as_ref(), "cmp_byo")
                .await
                .unwrap();
            let (_, dto) = get_capabilities(&state).await;
            assert_eq!(
                dto["composioTokenConfigured"], true,
                "the legacy field keeps answering its own narrow question: {dto}"
            );
            assert_eq!(
                dto["composioCredentialSource"], "static",
                "and a pasted token is the `static` tier: {dto}"
            );
        }
    }

    /// Both DTO construction sites report which provider a company's searches
    /// actually reach, and it tracks what the Search settings page stored.
    ///
    /// Same #567 precedent as the two tests above: a field wired into one branch
    /// alone tells the truth to a company with no plan and lies to every company
    /// that has one.
    #[tokio::test]
    async fn both_response_paths_carry_the_effective_search_provider() {
        use crate::company::search::{API_KEY_SECRET, PROVIDER_SECRET};
        use crate::ports::types::SecretValue;

        let grants_search = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"search\"]\n";
        for manifest in [
            grants_search.to_string(),
            format!("{grants_search}[plan]\nname = \"starter\"\n"),
        ] {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let state = state_with_manifest(&home, &manifest).await;
            let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

            let (_, dto) = get_capabilities(&state).await;
            assert_eq!(
                dto["searchProvider"], "managed",
                "nothing configured, so the platform's account answers: {dto}"
            );

            // A provider selected with no key is NOT a connection — the panel
            // must not report one, because the agents are still on managed.
            runtime
                .secrets()
                .set(runtime.id(), PROVIDER_SECRET, SecretValue("exa".into()))
                .await
                .unwrap();
            let (_, dto) = get_capabilities(&state).await;
            assert_eq!(dto["searchProvider"], "managed", "{dto}");

            runtime
                .secrets()
                .set(runtime.id(), API_KEY_SECRET, SecretValue("exa_key".into()))
                .await
                .unwrap();
            let (_, dto) = get_capabilities(&state).await;
            assert_eq!(
                dto["searchProvider"], "exa",
                "a finished connection is what the company searches through: {dto}"
            );
            // And never the key itself, on either path.
            assert!(!dto.to_string().contains("exa_key"), "{dto}");
        }
    }

    /// An unreadable secret store **omits** the field rather than reporting
    /// `none`.
    ///
    /// `none` is a verdict — "no credential resolves, no tools are wired" — and
    /// claiming it on a transient hiccup would send an operator to paste a token
    /// they already have. That is issue #886 in the other direction, so the only
    /// honest wire shape for "we do not know" is absence. The rest of the
    /// response still serves: budgets and tiers have nothing to do with Composio.
    #[tokio::test]
    async fn an_unreadable_store_omits_the_tier_rather_than_claiming_none() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with(
            &home,
            GRANTS_COMPOSIO,
            Some(std::sync::Arc::new(BrokenSecrets)),
        )
        .await;

        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK, "the response still serves: {dto}");
        assert!(
            dto.get("composioCredentialSource").is_none(),
            "an unknown tier is omitted, never rendered as a confident `none`: {dto}"
        );
        assert_eq!(
            dto["composioGranted"], true,
            "the manifest-derived flags are unaffected by the store: {dto}"
        );
    }

    /// **Issue #1192, test 1.** Publishing is reported on **both** DTO paths.
    ///
    /// The DTO is built in two places — `unconfigured` and the configured tail
    /// of `effective_status` — and a field wired into one of them alone reports
    /// honestly for a company with no `[plan]` and is silently absent for every
    /// company that has one. That is the failure mode the `OptInFlags` note
    /// names, and it is worth a test rather than a convention because the two
    /// literals are 200 lines apart.
    #[tokio::test]
    async fn publish_capability_is_reported_on_both_dto_paths() {
        // No `[plan]` → the `unconfigured` literal.
        let home_a_dir = home();
        let state = state_with_manifest(
            home_a_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"files\"]\n",
        )
        .await;
        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["configured"], false, "{dto}");
        assert_eq!(dto["publishGranted"], true, "{dto}");
        assert!(
            dto.get("publishInBuild").is_some(),
            "the build flag is always present so the console can render every state: {dto}"
        );

        // A `[plan]` → the configured literal, the one a field is most easily
        // forgotten in.
        let home_b_dir = home();
        let state2 = state_with_manifest(
            home_b_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"files\"]\n\
             [plan]\nname = \"starter\"\n",
        )
        .await;
        let (status2, dto2) = get_capabilities(&state2).await;
        assert_eq!(status2, StatusCode::OK);
        assert_eq!(dto2["configured"], true, "{dto2}");
        assert_eq!(
            dto2["publishGranted"], true,
            "a company with a plan must get the same publishing verdict: {dto2}"
        );
        assert_eq!(
            dto2["publishInBuild"], dto["publishInBuild"],
            "the build flag cannot depend on whether a plan is configured: {dto2}"
        );
    }

    /// **Issue #1192, test 2.** A company whose grants confer no file family
    /// reads as ungranted — using the *shipped* `companies/e2e_harness` allow
    /// list rather than an invented one, so the negative is a real manifest
    /// somebody runs rather than a string chosen to make the assertion pass.
    #[tokio::test]
    async fn publish_is_ungranted_without_a_file_or_docs_grant() {
        let home_dir = home();
        let state = state_with_manifest(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\n\
             allow = [\"composio\", \"mcp:*\", \"workspace\", \"workspace.*\", \"web\"]\n",
        )
        .await;
        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            dto["publishGranted"], false,
            "no files/docs grant means no publish_artifact on the belt: {dto}"
        );

        // …and the wildcard the majority of manifests actually ship DOES grant
        // it. Asserted here, beside the negative, so the two shapes read
        // against each other.
        let wildcard_dir = home();
        let state2 = state_with_manifest(
            wildcard_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"*\"]\n",
        )
        .await;
        let (_, dto2) = get_capabilities(&state2).await;
        assert_eq!(
            dto2["publishGranted"], true,
            "a bare `*` confers publishing — this is the shape most manifests ship: {dto2}"
        );
    }
}
