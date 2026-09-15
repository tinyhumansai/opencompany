//! The company's own TinyHumans credential, over HTTP (issue #586): read whether
//! one is set and which identity brokered calls currently present, and set /
//! rotate / clear it.
//!
//! The credential itself is [`company_key`](crate::company::company_key); this
//! is its console plane. **Write-only**: the key goes in through the `key`
//! field, lands in the secret store, and is never echoed — the read shape
//! carries only a `configured` boolean plus the non-secret tier name.
//!
//! **Admin-only** on the write, for the same reason
//! [`ops::composio`](super::composio)'s token write is: this key is the identity
//! every one of the company's agents presents to every surface the platform
//! brokers, and it is the company's wallet — whoever sets it decides which
//! account those agents act through and which account pays. That is a decision
//! made *for* the company, not a member's own, and [`AdminScopedCompany`] is
//! what says so in the signature.
//!
//! A set / rotate / clear takes effect on the agents' **next cycle** with no
//! restart: every surface **wired to this credential** re-resolves through
//! [`company_key::resolve`](crate::company::company_key::resolve) and the roster
//! fingerprint moves with the key's value. That is Composio today; inference and
//! embeddings still resolve from the environment (#585), so "wired to it" is a
//! smaller set than "brokered" until that lands.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

use axum::extract::State;
use axum::http::HeaderMap;

use crate::AppState;
use crate::company::company_key::{self, account_key_used_by, key_configured, load, resolve};
use crate::company::credentials::CredentialSource;
use crate::company::inference::store as inference_store;
use crate::company::runtime::CompanyRuntime;
use crate::error::{OpenCompanyError, UsedBy};
use crate::ports::types::CompanyEvent;
use crate::server::cognition::InferenceResolution;
use crate::server::error::ApiError;
use crate::server::ops::{AdminScopedCompany, ScopedCompany, scoped};
use crate::server::users::token::OsTokens;

/// What an admin most needs to understand before pasting: this key is the
/// company's wallet, and membership in the company is what grants access to it.
///
/// The last sentence is load-bearing. The Connections screen renders this card
/// next to the Inference one, both read "configured / not configured", and
/// pasting a *provider's* key into this field is the exact mistake the
/// `tinyhumans/key`-vs-`inference/key` split exists to prevent. An admin will
/// not have read that reasoning in the module docs, so the card has to carry
/// it. See [`company_key`](crate::company::company_key).
///
/// The distinction is **kept** — an OpenRouter key pasted here is the mistake
/// the split exists to prevent — but narrowed to what is true. The wording this
/// replaces compressed it into "nothing to do with models", and [`finish_link`]
/// contradicts that directly: a managed turn resolves *through* this key, so
/// for most companies this credential is exactly what their agents think on.
/// Telling an admin otherwise sends them hunting for a second key they do not
/// need.
///
/// It also states the **billing move**, which neither string used to — but
/// states it conditionally, because it is conditional twice over. This notice
/// is returned by [`set_key`] *and* [`finish_link`] *and* [`get_status`], and
/// the two write paths do different amounts: a paste fans the key out to
/// Composio and the LLM TinyHumans slots
/// ([`company_key::fan_out`](crate::company::company_key::fan_out), keys
/// rework #2306, slice 4a) and stops there, while the grant runs the same
/// fan-out and declares no provider of its own (Q10). And the managed chain
/// has two rungs above the copy this fan-out makes — a key pasted for
/// TinyHumans on the LLM page directly, and the legacy `inference/key` —
/// either of which goes on answering after this one is set (#2266). A flat
/// "setting this moves every agent turn onto this account" would be false on
/// both counts, which is the same shape of overclaim the rest of this change
/// removes, pointing the other way.
const CONSEQUENCE: &str = "This is the company's TinyHumans account key — the identity the platform presents when it \
     connects providers like Gmail or Slack on your behalf. Every member's agents act and spend \
     through it, and a provider connected with it belongs to the company rather than to the \
     person who connected it. Spend arrives as one account, so it cannot be attributed per \
     member. Saving it also copies it to TinyHumans on the LLM page and to Composio wherever \
     those hold no key of their own, and makes TinyHumans the default only when no default is \
     set. It is not a model provider's own key: an OpenRouter key, or your own endpoint's, \
     belongs on the LLM page and will not serve as an identity here.";

/// Said instead when nothing is configured and the instance carries no identity
/// either — the honest degraded state, rather than a picker that will fail.
///
/// Scoped to what this credential actually governs. "Providers cannot be
/// connected or used" read as "nothing works", and a company whose LLM page
/// holds a key of its own goes on thinking perfectly well without this one —
/// that key outranks this credential in the managed chain, and a provider of
/// its own never consults it. Overstating the breakage sends that operator to
/// fix something that is not broken.
const DEGRADED: &str = "No credential is set for this company and this instance carries no \
     platform identity, so nothing the platform brokers on its behalf works: no provider can be \
     connected, and there is no TinyHumans account to bill thinking to. Set the company's \
     TinyHumans account key. A model provider's own key from the LLM page is a different \
     credential and will not do for this — though a company that has set one there can still \
     think while this is unset.";

/// Builds the company-credential route fragment.
pub fn router() -> Router<AppState> {
    scoped("/credential", get(get_status).put(set_key))
        .merge(scoped("/credential/link/start", post(start_link)))
        .merge(scoped("/credential/link/finish", post(finish_link)))
        .merge(scoped("/credential/billing", get(get_billing)))
}

/// The company's credential status as the console renders it. **Never** carries
/// the key — only whether one is stored and which tier a brokered call presents.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CredentialStatusDto {
    /// Whether this company has its **own** TinyHumans key stored. False does
    /// not mean no credential — see `source`.
    configured: bool,
    /// Which identity a brokered call presents right now: `company` (this
    /// company's own key), `attested` / `static` (this instance's platform
    /// identity), or `none`.
    source: CredentialSource,
    /// The consequence of setting this key, stated plainly, or the degraded
    /// state when nothing can be presented at all.
    notice: String,
    /// Where this person looks after the account behind the key: the hub
    /// dashboard's key list, and its top-up page.
    ///
    /// `None` on a host whose backend the naming convention does not describe
    /// (a self-hosted or loopback hub), where a derived link would point at an
    /// origin that need not exist. The console renders no link there rather
    /// than one that 404s — the same rule `hub_link` follows for the button.
    /// See [`hub_account`](crate::server::hub_account).
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<HubAccountLinks>,
    /// Whether this host can complete a one-click key grant against the hub.
    ///
    /// Reported alongside the status so the console can decide whether to offer
    /// the button without a second request. `false` on every host with no hub
    /// wired, which is where the paste field remains the only way in — so the
    /// console renders exactly what it renders today rather than a button that
    /// would 404.
    hub_link: bool,
    /// The LLM TinyHumans key slot holds a key that is not the account key.
    /// Saving leaves it alone (Q7). Never the key — see
    /// [`company_key::SlotFacts`] (keys rework #2306, slice 4b).
    inference_has_own_key: bool,
    /// The same for `composio/tinyhumans/key` (with 1a's legacy read).
    composio_has_own_key: bool,
    /// `inference/default` is set (`ProviderOnly` or `Full`).
    default_set: bool,
    /// Whether the `tinyhumans` row saving would fill already carries a
    /// model — so saving here would not leave anything for step two to ask
    /// (KR-ACCT-01, 2026-09-15). `true` only when the row exists (the fan-out's
    /// own `Slot::Provider` origin, `ProviderOrigin::Indexed`) **and**
    /// [`inference_store::Provider::model`] resolves to exactly one id —
    /// mirroring the same test [`company_key::fan_out`]'s own "picked" logic
    /// applies when it decides whether a save can complete the row without a
    /// `model` in the request body.
    inference_has_model: bool,
    /// What a **clear** of this key would strand right now — the same
    /// [`account_key_used_by`] computation [`company_key::fan_out`]'s own
    /// guard runs atomically under its lock (P3-6, keys rework #2306 review),
    /// exposed here so the Remove-key dialog can name dependents the moment
    /// it opens rather than only after a refused, uninformed attempt
    /// (`docs/key-reworks/in-use-guards.md` §1-2; keys rework #2306, KR-L3-01).
    /// `None` when the key is unset or nothing would be stranded.
    #[serde(skip_serializing_if = "Option::is_none")]
    used_by: Option<UsedBy>,
}

/// The two account pages the console links out to.
///
/// Both are the hub's, behind that person's own sign-in, and deliberately not
/// reimplemented here: one ends an instance's access and the other moves money.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HubAccountLinks {
    /// The dashboard's API-key list — where a key minted by the grant flow can
    /// be seen, named and revoked.
    manage_keys_url: String,
    /// The dashboard's balance and top-up page. What the company's agents spend
    /// comes off this, so an instance that stops thinking mid-week is usually
    /// one trip here from working again.
    top_up_url: String,
}

/// A mutating response: the resulting status plus the switch reminder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationResponse {
    status: CredentialStatusDto,
    note: String,
    /// What the fan-out (`company_key::fan_out`, keys rework #2306, slice 4a)
    /// did to each of the five slots it touches, always in order composio,
    /// inference, provider, default, health.
    slots: Vec<SlotReportDto>,
    /// Whether a `tinyhumans` row could not be created or defaulted for want
    /// of a model — the console's cue to ask for one.
    needs_model: bool,
    /// Whether a model sent on a follow-up request would also become the
    /// company default.
    sets_default: bool,
    /// Catalog ids to offer, only ever alongside `needsModel`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    models: Vec<String>,
    /// Echoes what a **confirmed** clear would have refused with
    /// (`docs/key-reworks/in-use-guards.md` §3) — the shape computed before
    /// the mutation applied, so the console can show what it just broke.
    /// `None` on every mutation that is not a guarded clear, and on a guarded
    /// one that had nothing to warn about.
    #[serde(skip_serializing_if = "Option::is_none")]
    used_by: Option<UsedBy>,
    /// Whether the config this write just landed needs a restart before
    /// agents actually run on it (KR-ACCT-01, 2026-09-15) — same wire name
    /// and meaning as `ops::inference`'s own
    /// `InferenceStatusDto.restart_required`, computed the identical way
    /// ([`restart_required_for`]) so the Account page and the LLM page can
    /// never disagree about the same company. The fan-out can create the
    /// first `tinyhumans` row and make it the default, but a company that
    /// booted with no inference configured is already running the
    /// echo/hosted brain and an unwired workflow runner
    /// (`RuntimeBuilder::build` picks the brain once, at boot) — so a save
    /// here can leave the company *reporting* a working default while turns
    /// keep running on the old brain until an operator restarts. Never
    /// serialized as `false` — the same "absent means nothing to report"
    /// convention [`UsedBy::default`](crate::error::UsedBy) uses — so an
    /// older console reads a missing field exactly as it always has.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    restart_required: bool,
}

/// Wire spelling of one [`company_key::SlotReport`] — moved to
/// [`super::slot_report`] (keys rework #2306, slice 4c) so the Composio route
/// serialises the same shape rather than a second copy of it.
use super::slot_report::SlotReportDto;

/// Whether a slot's outcome actually changed stored state — the same test a
/// catalog-cache eviction and a journal line both apply (keys rework #2306,
/// slice 4a): `Kept`, `Skipped`, `Failed` and the two health outcomes leave
/// the slot exactly as it was.
fn slot_changed(outcome: company_key::SlotOutcome) -> bool {
    use company_key::SlotOutcome;
    matches!(
        outcome,
        SlotOutcome::Filled | SlotOutcome::Rotated | SlotOutcome::Cleared | SlotOutcome::RolledBack
    )
}

/// Set-key body. `key` is write-only intake (never returned): a non-empty value
/// sets or rotates it, an explicit empty string clears it. `model` names the
/// model a `tinyhumans` row should carry if the fan-out creates one (keys
/// rework #2306, slice 4a) — ignored while clearing. `confirmInUse` proceeds
/// with a clear the in-use guard would otherwise refuse
/// (`docs/key-reworks/in-use-guards.md` §2); ignored on a set/rotate, which is
/// never guarded.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetKey {
    key: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    confirm_in_use: bool,
}

/// The real inference prober in production, or a per-company override in
/// tests (mirrors [`super::composio`]'s `probe_override`: keyed by company id
/// rather than held as one global, because the test binary runs many
/// companies' fan-outs concurrently in one process). `#[cfg(test)]`
/// throughout — there is no seam here in a shipped build.
fn prober_for(runtime: &CompanyRuntime) -> Box<dyn company_key::InferenceProber> {
    #[cfg(test)]
    {
        if let Some(forced) = prober_override::get(runtime.id().as_ref()) {
            return Box::new(prober_override::Forced(forced));
        }
    }
    #[cfg(not(test))]
    {
        let _ = runtime;
    }
    Box::new(company_key::LiveProber)
}

/// Test-only: force [`prober_for`]'s answer for one company, exactly as
/// [`super::composio`]'s `probe_override` does for the Composio draft probe.
///
/// KR review comment 4012285739: a company-keyed *global* map here would never
/// clear an entry between tests, so a later test reusing a company id would
/// silently inherit an earlier test's forced answer. `thread_local!` sidesteps
/// that rather than relying on every test to remember a teardown call: every
/// test in this file uses the default (single-threaded) `#[tokio::test]`
/// runtime, so a test's own body and every future it drives — including the
/// router call this override answers — run on that one OS thread, and libtest
/// gives each test function its own thread. A fresh, empty map per thread
/// means a fresh map per test, with no entry able to outlive the test that
/// wrote it.
#[cfg(test)]
mod prober_override {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use async_trait::async_trait;

    use crate::company::inference::probe::{ProbeClass, ProbeFailure};

    pub(super) type Outcome = std::result::Result<Vec<String>, ProbeClass>;

    thread_local! {
        static MAP: RefCell<HashMap<String, Outcome>> = RefCell::new(HashMap::new());
    }

    pub(super) fn set(company: &str, outcome: Outcome) {
        MAP.with(|map| {
            map.borrow_mut().insert(company.to_string(), outcome);
        });
    }

    pub(super) fn get(company: &str) -> Option<Outcome> {
        MAP.with(|map| map.borrow().get(company).cloned())
    }

    pub(super) struct Forced(pub(super) Outcome);

    #[async_trait]
    impl crate::company::company_key::InferenceProber for Forced {
        async fn probe(
            &self,
            _base_url: &str,
            _key: &str,
        ) -> std::result::Result<Vec<String>, ProbeFailure> {
            match &self.0 {
                Ok(ids) => Ok(ids.clone()),
                Err(class) => Err(ProbeFailure {
                    class: *class,
                    raw: "forced".to_string(),
                    truncated: false,
                }),
            }
        }
    }
}

/// Resolves the credential status DTO for a company.
///
/// `source` is the company's **TinyHumans identity**, straight from
/// [`company_key::resolve`](crate::company::company_key::resolve): its own key,
/// else this instance's platform identity, else nothing.
///
/// That is deliberately **not** the same answer `GET …/composio` gives. The
/// Composio route prepends its BYO `composio/tinyhumans/key` tier, so a company holding
/// both reads `company` here and `static` there — and both are correct, because
/// they answer different questions: this route reports whose identity the
/// company *has*, the Composio one reports what a Composio call *presents*.
/// Claiming parity between them would be wrong in exactly the case where the
/// distinction matters.
async fn effective_status(
    state: &AppState,
    runtime: &CompanyRuntime,
) -> Result<CredentialStatusDto, ApiError> {
    let secrets = runtime.secrets();
    let configured = key_configured(runtime.id(), secrets.as_ref())
        .await
        .map_err(ApiError)?;
    let env = crate::app::config::ProcessEnv;
    let source = resolve(
        runtime.id(),
        secrets.as_ref(),
        crate::company::TinyhumansTokenSource::from_env(&env).map(std::sync::Arc::new),
    )
    .await
    .map_err(ApiError)?
    .source();
    let facts = company_key::slot_facts(runtime.id(), secrets.as_ref())
        .await
        .map_err(ApiError)?;
    let used_by = account_key_used_by(runtime.id(), secrets.as_ref())
        .await
        .map_err(ApiError)?;
    let inference_has_model = tinyhumans_row_has_model(runtime).await?;
    Ok(CredentialStatusDto {
        configured,
        source,
        notice: if source == CredentialSource::None {
            DEGRADED.to_string()
        } else {
            CONSEQUENCE.to_string()
        },
        account: state.config().hub_site().map(|site| HubAccountLinks {
            manage_keys_url: crate::server::hub_account::manage_keys_url(&site),
            top_up_url: crate::server::hub_account::top_up_url(&site),
        }),
        hub_link: state.hub_identity().is_some(),
        inference_has_own_key: facts.inference_has_own_key,
        composio_has_own_key: facts.composio_has_own_key,
        default_set: facts.default_set,
        inference_has_model,
        used_by,
    })
}

/// Whether the `tinyhumans` row exists and already carries exactly one model
/// — see [`CredentialStatusDto::inference_has_model`]'s doc for why "exactly
/// one" is the bar rather than merely "non-empty": it mirrors the same test
/// [`company_key::fan_out`]'s own default-slot logic applies to a row's
/// `models` map (`ModelOnRow::One` only) when it decides whether a follow-up
/// save can complete the row without a `model` of its own.
async fn tinyhumans_row_has_model(runtime: &CompanyRuntime) -> Result<bool, ApiError> {
    let row = inference_store::list_providers(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?
        .into_iter()
        .find(|p| {
            p.origin == inference_store::ProviderOrigin::Indexed
                && p.slug == crate::company::inference::MANAGED_SLUG
        });
    Ok(row.is_some_and(|p| matches!(p.model(), inference_store::ModelOnRow::One(_))))
}

/// Whether this company needs a restart before a mutation here actually
/// reaches agent turns (KR-ACCT-01, 2026-09-15) — see
/// [`MutationResponse::restart_required`]'s doc for why.
///
/// Computed the identical way [`super::inference`]'s own
/// `InferenceStatusDto.restart_required` is: whether a tenant config resolves
/// *now* ([`super::inference::inference_resolution`], folding an unreadable
/// manifest into "not configured" exactly as that function already does for
/// the LLM page), fed through [`super::inference::restart_pending`]. One
/// reader shared by both surfaces, so they cannot drift apart on the same
/// company. Never fails — an unreadable manifest reads as "not configured",
/// which answers `false` here rather than surfacing a second error on top of
/// whatever the fan-out itself already reported.
async fn restart_required_for(runtime: &CompanyRuntime) -> bool {
    let configured = matches!(
        super::inference::inference_resolution(runtime).await,
        InferenceResolution::Resolved
    );
    super::inference::restart_pending(runtime, configured)
}

/// Puts a fan-out that just configured inference to work, the way the LLM
/// page's own `PUT …/inference` does (issue #290): when the company booted
/// with no inference source it is on the offline echo brain, and a
/// `tinyhumans` row plus default this write created cannot reach agents until
/// the runtime is rebuilt. Telling the operator "restart required" in a toast
/// was the whole of the previous behaviour, and the observed result was a
/// company whose account key, row and default all read as set while every
/// turn still answered `You said: …`.
///
/// Only rebuilds when the fan-out actually moved the provider row or the
/// default — a save that changed nothing about inference (a Composio-only
/// copy, a rotation of an already-live key) never quiesces a running
/// company. Returns the runtime the response status must be read off: the
/// successor after a rebuild, else the one the request came in on. A failed
/// rebuild is logged and falls back to the incoming runtime, whose status
/// still reports `restart_required` — the honest answer, exactly as
/// `set_config` handles the same failure.
async fn rebuild_if_pending(
    state: &AppState,
    company: &AdminScopedCompany,
    report: &company_key::FanOutReport,
) -> Arc<CompanyRuntime> {
    let runtime = company.runtime.clone();
    let inference_moved = report.slots.iter().any(|s| {
        matches!(
            s.slot,
            company_key::Slot::Provider | company_key::Slot::Default
        ) && slot_changed(s.outcome)
    });
    if !inference_moved || !restart_required_for(runtime.as_ref()).await {
        return runtime;
    }
    match crate::runtime::rebuild_company(state, runtime.id()).await {
        Ok(successor) => successor,
        Err(err) => {
            tracing::warn!(
                company = %runtime.id(),
                error = %err,
                "account key saved but the runtime could not be rebuilt; a restart is still required",
            );
            runtime
        }
    }
}

/// `GET …/credential` — whether this company has its own key, and which identity
/// its brokered calls present.
async fn get_status(
    State(state): State<AppState>,
    company: ScopedCompany,
) -> Result<Json<CredentialStatusDto>, ApiError> {
    Ok(Json(
        effective_status(&state, company.runtime.as_ref()).await?,
    ))
}

/// `PUT …/credential` — set / rotate / clear the company's write-only
/// TinyHumans credential, and fan it out to Composio and the LLM TinyHumans
/// slots (`company_key::fan_out`, keys rework #2306, slice 4a). **Admin-only**
/// — see the module docs.
///
/// Only a **clear** is guarded (`docs/key-reworks/in-use-guards.md` §2): a
/// set/rotate cannot strand a dependent, since the credential it presents only
/// gets more likely to resolve. The `usedBy` check now runs **inside**
/// `fan_out` itself (P3-6, keys rework #2306 review) — atomically, right after
/// it takes its own lock and before it writes anything — rather than here
/// beforehand, which used to leave an unlocked window between the check and
/// the write a concurrent save could land in. A refused clear still leaves
/// every slot exactly as it was; `fan_out`'s `Err` just now guarantees that
/// rather than this handler's own pre-check.
async fn set_key(
    State(state): State<AppState>,
    company: AdminScopedCompany,
    Json(body): Json<SetKey>,
) -> Result<Json<MutationResponse>, ApiError> {
    let runtime = company.runtime.as_ref();
    let clearing = body.key.trim().is_empty();

    let prober = prober_for(runtime);
    let report = company_key::fan_out(
        runtime.id(),
        runtime.secrets().as_ref(),
        company_key::FanOutRequest {
            key: &body.key,
            model: body.model.as_deref(),
            confirm_in_use: body.confirm_in_use,
            proxy_base_url: Some(&state.config().api_url),
        },
        prober.as_ref(),
    )
    .await
    .map_err(ApiError)?;
    // Echoes what a confirmed clear would have refused with
    // (`in-use-guards.md` §3) — computed by `fan_out` itself, atomically,
    // before it wrote anything; `None` on a set/rotate or a clear with
    // nothing to warn about.
    let used_by = report.used_by.clone();

    // The credential decides which account the backend resolves, so a change can
    // change which Composio catalog this company gets. Drop the cached one
    // rather than serving the previous account's answer for up to `CATALOG_TTL`.
    super::composio::evict_catalog_cache(runtime);
    // Same reasoning for the inference model-list cache, only when the LLM
    // copy itself actually moved — a `Kept`/`Skipped`/`Failed` inference slot
    // left `provider/tinyhumans/key` exactly as it was.
    if report
        .slots
        .iter()
        .any(|s| s.slot == company_key::Slot::Inference && slot_changed(s.outcome))
    {
        crate::server::inference_models::evict_company_catalogs(runtime.id().as_ref());
    }

    // Namespaced `company_key_*` rather than reusing `ops::composio`'s
    // `credential_set` / `credential_cleared`. Both routes append
    // `ToolAccessChanged` to the same log, and with one shared vocabulary a
    // reader could not tell "the admin rotated the company's identity" from
    // "the admin swapped the Composio token" — two changes with different blast
    // radii. An audit line that cannot name what changed is most of the way to
    // not having one.
    journal_fan_out(&company, clearing, &report).await?;

    // Read off whichever runtime is live after this write — the successor if
    // the fan-out configured inference for a company that booted without any.
    let live = rebuild_if_pending(&state, &company, &report).await;
    Ok(Json(MutationResponse {
        status: effective_status(&state, live.as_ref()).await?,
        note: company_key::fan_out_note(clearing, &report, body.model.as_deref()),
        slots: report.slots.iter().map(SlotReportDto::from).collect(),
        needs_model: report.needs_model,
        sets_default: report.sets_default,
        models: report.models.clone(),
        used_by,
        restart_required: restart_required_for(live.as_ref()).await,
    }))
}

/// The name the minted key carries in the person's TinyHumans account.
///
/// Names the company, so someone looking at a list of keys can tell which
/// instance each belongs to and revoke one without guessing.
fn key_name(company: &CompanyRuntime) -> String {
    format!("OpenCompany — {}", company.id())
}

/// What the console navigates to, and nothing else.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartLinkResponse {
    /// The hub URL to send the browser to, challenge already attached.
    authorize_url: String,
}

/// Finish body: the handle we minted, and the code the hub sent back.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FinishLink {
    state: String,
    code: String,
}

/// `POST …/credential/link/start` — begin a one-click key grant.
///
/// **Admin-only**, the same authority [`set_key`] needs and for the same reason:
/// what comes back is the company's identity and its wallet. That the key is
/// minted rather than pasted changes who types it, not what it does.
async fn start_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    company: AdminScopedCompany,
) -> Result<Json<StartLinkResponse>, ApiError> {
    let runtime = company.runtime.as_ref();
    // No exchange means nothing could redeem the code that came back, so the
    // flow cannot complete. Refusing here rather than at `finish` is the
    // difference between a console that never offers the button and one that
    // sends an admin through Google to fail on return.
    if state.hub_identity().is_none() {
        return Err(ApiError(OpenCompanyError::NotFound(
            "this host is not part of a TinyHumans ecosystem".to_string(),
        )));
    }

    let started = state.hub_links().start(&OsTokens, runtime.id().as_ref());

    // Where the hub returns to. `key=link` is this console's own marker, kept
    // distinct from the `key=auth` the hub appends on a sign-in so the two
    // return legs can never be mistaken for each other in `App.tsx`.
    let origin = callback_origin(&state, &headers);
    let callback_url = format!(
        "{}/?company={}&key=link&state={}",
        origin.trim_end_matches('/'),
        runtime.id(),
        started.state,
    );

    // Through the site's `/connect` where there is one, straight at the API
    // where there is not.
    //
    // `GET /auth/key` defaults to `provider=google` and redirects there at
    // once, so an admin who pressed a button in their own console landed on a
    // Google account picker that named nobody and offered no other account.
    // The site page names the instance asking, says what will be created, and
    // offers the same providers the sign-in screen does — then sends them to
    // this very endpoint with the provider they picked. The parameters are
    // built once either way, so the two paths cannot disagree about the
    // challenge.
    let query = crate::server::hub_identity::key_grant_query(
        &callback_url,
        &started.challenge,
        &key_name(runtime),
    );
    let authorize_url = match state.config().hub_site() {
        Some(site) => crate::server::hub_account::connect_url(&site, &query),
        None => format!(
            "{}/auth/key?{query}",
            state.config().api_url.trim_end_matches('/')
        ),
    };

    Ok(Json(StartLinkResponse { authorize_url }))
}

/// Where the hub sends the browser back to.
///
/// [`host_base_url`](crate::AppConfig::host_base_url) is the answer wherever a
/// deployment states one: a hosted tenant is `OPENCOMPANY_PUBLIC_URL`, and that
/// origin serves the console, so the return leg lands on the page that finishes
/// the exchange.
///
/// Its fallback — `http://{bind}` — is the wrong answer for local development,
/// and wrong in a way that only shows up at the end of the flow. The console in
/// dev is a Vite server on another port; the host on `127.0.0.1:8080` serves no
/// page unless somebody set `OPENCOMPANY_CONSOLE_DIR`. So an operator signed in,
/// approved, and landed on a 404 holding a spent code — with nothing on that
/// page able to say what had gone wrong, because there was no page.
///
/// So when nothing states an origin, the browser's own is used: whatever
/// pressed the button is where the answer should come back to, which is exactly
/// what a dev server on `:5173` needs and needs nobody to configure.
///
/// **Only a loopback origin.** A header is attacker-controllable in principle,
/// and while a stolen code redeems nothing without the verifier this host keeps
/// (`server::hub_link`), a callback is not somewhere to accept an arbitrary
/// address on a request's say-so. Anything else falls through to the bind
/// address, and a deployment that wants a real origin sets `OPENCOMPANY_PUBLIC_URL`
/// — which wins over this outright.
fn callback_origin(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(url) = state.config().public_url.as_deref() {
        let url = url.trim().trim_end_matches('/');
        if !url.is_empty() {
            return url.to_string();
        }
    }

    headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|origin| is_loopback_origin(origin))
        .map(|origin| origin.trim_end_matches('/').to_string())
        .unwrap_or_else(|| state.config().host_base_url())
}

/// Whether `origin` is an `http://` address on this machine.
///
/// Deliberately the same shape the hub's own gate admits without a tenant
/// registry lookup — `http` to `localhost` or a loopback literal — so an origin
/// accepted here cannot be one the hub will refuse a moment later.
fn is_loopback_origin(origin: &str) -> bool {
    let Some(rest) = origin.strip_prefix("http://") else {
        return false;
    };
    // Host only: a port is expected (that is the whole point), a path is not.
    if rest.contains('/') {
        return false;
    }
    // A bracketed IPv6 literal carries colons of its own, so the port split has
    // to start after the bracket or `[::1]:5173` parses as the host `[`.
    let host = match rest.strip_prefix('[') {
        Some(inside) => match inside.split_once(']') {
            Some((host, after)) if after.is_empty() || after.starts_with(':') => host,
            _ => return false,
        },
        None => rest.split_once(':').map_or(rest, |(host, _)| host),
    };
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// `POST …/credential/link/finish` — redeem the code, store what comes back,
/// and run it through the same fan-out `set_key` does.
///
/// The key lands in **one** place: `tinyhumans/key`, the company's identity for
/// everything the platform brokers — the fan-out's own copies (Composio, the
/// LLM TinyHumans slot) are derived from it, never a second source of truth.
/// It declares **no provider of its own** (Q10): a managed turn already
/// resolves through this key by *resolution* rather than by a copy, because
/// [`resolve_effective`](crate::company::inference::resolve_effective) reads
/// the company's account key for a managed provider.
///
/// This used to also write `inference/config = {provider: "managed"}`
/// directly, and that declaration was two bugs. It went **stale**: rotating
/// the account key through the ordinary route left the inference copy
/// presenting the old value until someone replaced it separately. And it
/// **misrouted**: it was stored with `provider: "managed"`, which
/// `normalize_provider` folded onto `openrouter` before the managed branch was
/// consulted, so a `th_…` key was presented as a bearer to `openrouter.ai`.
/// With one slot, one resolution seam, and the fan-out's own copy-if-empty
/// rule, neither is reachable (issue #2266).
async fn finish_link(
    State(state): State<AppState>,
    company: AdminScopedCompany,
    Json(body): Json<FinishLink>,
) -> Result<Json<MutationResponse>, ApiError> {
    let runtime = company.runtime.as_ref();

    let Some(exchange) = state.hub_identity().cloned() else {
        return Err(ApiError(OpenCompanyError::NotFound(
            "this host is not part of a TinyHumans ecosystem".to_string(),
        )));
    };

    // Single-use, and bound to the company it was started for. An expired or
    // replayed handle is indistinguishable from one that never existed, which
    // is the right amount to say: the remedy is the same either way.
    let Some(link) = state.hub_links().take(&body.state, runtime.id().as_ref()) else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "that connection attempt has expired — start it again".to_string(),
        )));
    };

    let key = exchange
        .redeem_key_grant(&body.code, &link.verifier)
        .await
        .map_err(ApiError)?;

    // The fan-out stores the account key before anything else can fail. The
    // hub emits the plaintext exactly once and cannot reissue it, so a key
    // dropped here is a key nobody can recover — the person would have to run
    // the whole flow again, and the one they just minted would linger in
    // their account doing nothing. A grant never names a model, so it never
    // creates a `tinyhumans` row or a default on its own (Q10) — only the key
    // itself, and its Composio/LLM copies.
    let prober = prober_for(runtime);
    let report = company_key::fan_out(
        runtime.id(),
        runtime.secrets().as_ref(),
        company_key::FanOutRequest {
            key: &key,
            model: None,
            // A grant never clears (Q10) — this flag never gates anything on
            // this path, so it is set unconditionally rather than threaded
            // from a request that has no such field.
            confirm_in_use: true,
            proxy_base_url: Some(&state.config().api_url),
        },
        prober.as_ref(),
    )
    .await
    .map_err(ApiError)?;

    super::composio::evict_catalog_cache(runtime);
    if report
        .slots
        .iter()
        .any(|s| s.slot == company_key::Slot::Inference && slot_changed(s.outcome))
    {
        crate::server::inference_models::evict_company_catalogs(runtime.id().as_ref());
    }
    journal_fan_out(&company, false, &report).await?;

    let live = rebuild_if_pending(&state, &company, &report).await;
    Ok(Json(MutationResponse {
        status: effective_status(&state, live.as_ref()).await?,
        note: company_key::fan_out_note(false, &report, None),
        slots: report.slots.iter().map(SlotReportDto::from).collect(),
        needs_model: report.needs_model,
        sets_default: report.sets_default,
        models: report.models.clone(),
        used_by: None,
        restart_required: restart_required_for(live.as_ref()).await,
    }))
}

/// Records who changed the company's credential (issue #403's discipline).
///
/// Propagates a journal failure rather than swallowing it, mirroring
/// [`ops::composio`](super::composio): the point of this record is that a change
/// to what the company's agents act through is never invisible, and an audit
/// line that quietly fails to be written is the one failure mode that would
/// defeat it.
async fn journal(company: &AdminScopedCompany, change: &str) -> Result<(), ApiError> {
    company
        .runtime
        .events()
        .append(
            company.id(),
            CompanyEvent::ToolAccessChanged {
                change: change.to_string(),
                toolkit: None,
                by: Some(company.actor()),
            },
        )
        .await
        .map_err(ApiError)?;
    Ok(())
}

/// Journals a fan-out (§3.5 of the plan): first the unchanged
/// `company_key_set` / `company_key_cleared` line, then one entry per slot
/// whose outcome actually changed stored state —
/// `company_key_{slot}_{filled|rotated|cleared|rolled_back}`, slot in
/// `composio|inference|provider|default` (the row is
/// `company_key_provider_filled`). No entry for `kept`, `skipped`, `failed` or
/// the health slot, which never changes anything this journal's vocabulary
/// describes.
async fn journal_fan_out(
    company: &AdminScopedCompany,
    clearing: bool,
    report: &company_key::FanOutReport,
) -> Result<(), ApiError> {
    journal(
        company,
        if clearing {
            "company_key_cleared"
        } else {
            "company_key_set"
        },
    )
    .await?;
    for slot_report in &report.slots {
        let slot_name = match slot_report.slot {
            company_key::Slot::Composio => "composio",
            company_key::Slot::Inference => "inference",
            company_key::Slot::Provider => "provider",
            company_key::Slot::Default => "default",
            company_key::Slot::Health => continue,
        };
        let suffix = match slot_report.outcome {
            company_key::SlotOutcome::Filled => "filled",
            company_key::SlotOutcome::Rotated => "rotated",
            company_key::SlotOutcome::Cleared => "cleared",
            company_key::SlotOutcome::RolledBack => "rolled_back",
            _ => continue,
        };
        journal(company, &format!("company_key_{slot_name}_{suffix}")).await?;
    }
    Ok(())
}

#[cfg(test)]
mod test;

/// `GET …/credential/billing` — what the account behind this company's key has
/// left to spend, and on which plan.
///
/// **Read-only, and only a read.** Topping up and changing a plan move money,
/// which is a decision a person makes signed in to their own account on the
/// hub — so this reports, and the console links out for the rest. A route here
/// that could raise a spend limit would make the limit advisory.
///
/// Not admin-gated, unlike the write above: a member whose agents stop working
/// mid-afternoon is the person who most needs to see "the balance is zero",
/// and telling them only an admin may look at a number they are already
/// feeling is how a company spends an afternoon guessing. Nothing here names
/// the credential, only what it can spend.
///
/// A company with no credential of its own is not an error: the console draws
/// the pitch for connecting one instead of a balance card, so this answers with
/// `configured: false` and no figures rather than a 404 the page has to
/// interpret.
async fn get_billing(
    State(state): State<AppState>,
    company: ScopedCompany,
) -> Result<Json<BillingDto>, ApiError> {
    let runtime = company.runtime.as_ref();
    // The company's own key only — never the instance's fallback platform
    // identity. `resolve` would report `configured: true` and query billing
    // for the shared host identity when the company has set nothing, exposing
    // that account's balance and plan to any member. `load` never falls
    // through.
    let credential = load(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;

    let Some(key) = credential.current().await.map_err(ApiError)? else {
        return Ok(Json(BillingDto {
            configured: false,
            summary: None,
            unavailable: None,
        }));
    };

    let Some(exchange) = state.hub_identity().cloned() else {
        // A build or deployment with no hub. There is an account somewhere that
        // this key belongs to, but nothing here can ask it anything.
        return Ok(Json(BillingDto {
            configured: true,
            summary: None,
            unavailable: Some("this host is not part of a TinyHumans ecosystem".to_string()),
        }));
    };

    match exchange.billing_summary(&key).await {
        Ok(summary) => Ok(Json(BillingDto {
            configured: true,
            summary: Some(summary),
            unavailable: None,
        })),
        // A hub that will not answer is reported as "not known right now", not
        // as a zero balance. The two look identical on a card and mean opposite
        // things: one is "top up", the other is "try again".
        Err(error) => Ok(Json(BillingDto {
            configured: true,
            summary: None,
            unavailable: Some(error.to_string()),
        })),
    }
}

/// The billing panel's whole state, including the two ways it can have no
/// figures to show.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BillingDto {
    /// Whether any credential could be resolved to ask with.
    configured: bool,
    /// The account's standing, when the hub answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<crate::server::hub_identity::BillingSummary>,
    /// Why there are no figures, when there are none and a credential exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable: Option<String>,
}
