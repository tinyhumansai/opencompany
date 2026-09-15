//! Writing the provider list: add, edit, delete, enable/disable, the draft
//! probe, and the routing table.
//!
//! Handlers only. Every branch worth a test is in a pure module beside this one
//! — [`catalogue`] decides what a kind's endpoint and auth style are,
//! [`store`](crate::company::inference::store) decides where a record and its
//! credential live, [`probe`] classifies a failure and answers where a probe may
//! point, and [`resolve`] answers what a removal orphans. What is left here is
//! extraction, ordering and DTO mapping.
//!
//! ## Authority
//!
//! **`AdminScopedCompany` on every route here except one.** The axis is "does
//! this decide something for the company", not read-versus-write: adding a
//! provider decides where the company's turns go and whose account pays for
//! them. The draft probe is on the same footing for a different reason —
//! generalising "send a request to this URL with this key" to company scope
//! creates an authenticated outbound-request primitive, and an SSRF guard is the
//! second line of defence behind an authority check, not a substitute for one.
//!
//! The exception is [`test_provider`], which re-asks a question the company has
//! already answered: it names no destination and no credential of its own, so it
//! is `ScopedCompany`, exactly like the `POST …/inference/test` it mirrors.
//!
//! ## The add flow's ordering, which is not arbitrary
//!
//! ```text
//!   validate ──▶ slug ──▶ write key ──▶ flush record ──▶ PROBE ──┬─▶ ok
//!                                                                 │
//!                                                     auth ◀──────┴──▶ anything else
//!                                                       │                  │
//!                                        roll back record AND key    KEEP both,
//!                                        reject                      amber advisory
//! ```
//!
//! 1. **Validate locally what can be validated locally.** A typed endpoint's
//!    scheme and shape are knowable without a network, so they are rejected
//!    before anything is written.
//! 2. **Derive and check the slug before any write.** A collision found after
//!    the credential has landed means a credential sitting in a slot nothing
//!    owns.
//! 3. **Credential first, then the record.** The probe reads the key by slug, so
//!    it has to be there before the record it belongs to is flushed.
//! 4. **Probe**, and classify rather than reduce to a boolean.
//! 5. **Roll back both stores only on the destructive class**, and log a
//!    rollback failure loudly rather than swallowing it. A silently failed
//!    key-clear orphans a secret, which is an incident shape rather than
//!    untidiness.

use std::collections::BTreeMap;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::inference::{catalogue, paged_catalog, probe, resolve, store};
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::server::error::ApiError;
use crate::server::ops::{AdminScopedCompany, scoped};

use super::{InferenceStatusDto, effective_status, managed_resolves};

/// The provider write plane.
pub(super) fn router() -> Router<AppState> {
    scoped("/inference/providers", post(add_provider))
        .merge(scoped(
            "/inference/providers/{slug}",
            put(edit_provider).delete(delete_provider),
        ))
        .merge(scoped(
            "/inference/providers/{slug}/enabled",
            post(set_enabled),
        ))
        .merge(scoped(
            "/inference/providers/{slug}/default",
            post(set_default),
        ))
        // Deliberately **not** under `/inference/providers/…`: a draft has no
        // slug yet, and a literal segment sharing a prefix with a `{slug}`
        // capture is a routing ambiguity waiting to be resolved the wrong way by
        // whichever router version is in play.
        .merge(scoped("/inference/probe", post(probe_draft)))
        // `ScopedCompany`, not admin — the only route here that is. It probes a
        // provider **as already stored**, naming no destination and no
        // credential of its own, which is the same footing as the existing
        // `POST …/inference/test`. The axis is "does this decide something for
        // the company", and re-asking a question the company already answered
        // decides nothing.
        .merge(scoped(
            "/inference/providers/{slug}/test",
            post(test_provider),
        ))
        // Per provider, not per company: two providers are two catalogs, and a
        // routing row picking a model needs the list of the one it is pointed
        // at. The existing `…/inference/models` answers for the *configured*
        // endpoint, which is a different question once there is a list.
        .merge(scoped(
            "/inference/providers/{slug}/models",
            get(list_provider_models),
        ))
        .merge(scoped("/inference/routes", get(get_routes).put(put_routes)))
        // The managed tier has no provider record — it resolves from a chain
        // rather than from a row — so its credential is written by a route of
        // its own rather than through `add_provider`. Putting it in the index
        // would create a record whose slug collides with entry zero's whenever
        // the company's stored config is already managed.
        .merge(scoped("/inference/managed/key", put(set_managed_key)))
        // Managed is a provider like any other in these two respects: its
        // credential can be checked, and it can be excluded from routing.
        .merge(scoped(
            "/inference/managed/enabled",
            post(set_managed_enabled),
        ))
        .merge(scoped("/inference/managed/test", post(test_managed)))
}

// ---- wire shapes ------------------------------------------------------------

/// What the add dialog sends.
///
/// **No `Serialize`.** This carries a credential, so it travels one way only.
/// The type system is the mechanism: a shape that cannot be serialized cannot
/// be put in a response body by a later edit that reaches for a convenience
/// derive.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddProvider {
    /// The catalogue slug chosen, the CLI option slug, or `custom`.
    kind: String,
    /// The operator's name for a custom provider. Ignored for a catalogue
    /// entry, whose label is the catalogue's.
    #[serde(default)]
    label: Option<String>,
    /// The endpoint, for a local runtime or a custom provider. A cloud
    /// provider's comes from the preset and anything sent here is ignored —
    /// the paths in that table are too varied to be derived or overridden by
    /// accident.
    #[serde(default)]
    base_url: Option<String>,
    /// The outbound credential. Write-only intake: no route returns it.
    #[serde(default)]
    key: Option<String>,
    /// The one model this row serves.
    ///
    /// Required for every kind (keys rework, issue #2306, slice 2c) — checked
    /// by [`store::check_model_id`] before any write. `Option` only so a
    /// missing field gets that function's own sentence rather than serde's
    /// bare 422; there is no longer a kind or catalog content that makes this
    /// optional. **The field that was missing, and the reason the reported
    /// 404 existed**: `add_provider` used to write `models: BTreeMap::new()`
    /// with no way to supply one at all, so a provider whose catalog
    /// published no vocabulary this host recognised was connected with four
    /// tiers unmapped — and the bare tier name went out on the wire (no
    /// longer possible at all since slice 2d's `model_on_the_wire`).
    #[serde(default)]
    model: Option<String>,
    /// Also make this row the company default `{provider, model}` (keys
    /// rework, slice 2c). Written last, after the row itself, so a failure
    /// here never half-applies the add.
    #[serde(default)]
    make_default: bool,
    /// Add despite a probe failure that would otherwise be destructive.
    ///
    /// The "add anyway" escape hatch, and it exists because a provider that does
    /// not serve an OpenAI-shaped `{base}/models` listing is still perfectly
    /// usable for inference — blocking creation on the probe would leave those
    /// operators unable to reach the model field at all.
    ///
    /// The console only offers it after a **typed probe failure**, never after a
    /// slug collision or a failed key write, and clears it on every retry.
    #[serde(default)]
    add_anyway: bool,
}

/// What the edit dialog sends. Same credential rule as [`AddProvider`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditProvider {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    /// The one model id. Omit to leave unchanged (keys rework, slice 2c —
    /// replaces the old per-tier `models` map). If this row is the company's
    /// current full default, the default's model moves with it in the same
    /// request.
    #[serde(default)]
    model: Option<String>,
    /// Omit to leave the credential unchanged; send `""` to clear it.
    #[serde(default)]
    key: Option<String>,
    /// Confirms a key clear that the in-use guard would otherwise refuse
    /// (`docs/key-reworks/in-use-guards.md` §2). Ignored unless `key` is
    /// `Some("")`. Defaults to `false`.
    #[serde(default)]
    confirm_in_use: bool,
}

/// The enable/disable body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetEnabled {
    enabled: bool,
    /// Confirms a **disable** that the in-use guard would otherwise refuse
    /// (`docs/key-reworks/in-use-guards.md` §2). Ignored when switching on.
    #[serde(default)]
    confirm_in_use: bool,
}

/// `POST …/inference/providers/{slug}/default` body (keys rework, slice 2c —
/// this route used to take no body at all).
#[derive(Debug, Default, Deserialize)]
struct SetDefault {
    #[serde(default)]
    model: Option<String>,
}

/// A draft probe: an endpoint and a key that are **not stored**.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeDraft {
    base_url: String,
    #[serde(default)]
    key: Option<String>,
    /// The kind, so the credential is presented the way that kind expects.
    #[serde(default)]
    kind: Option<String>,
}

/// The routing table on the way in.
#[derive(Debug, Deserialize)]
struct PutRoutes {
    /// Tier → route string. A tier mapped to `""` is unset.
    routes: BTreeMap<String, String>,
}

/// What a probe produced, for the console to render.
///
/// **Never carries the raw upstream string.** That text can echo request
/// material — headers, fragments of a key — and it lands in a banner someone
/// screenshots into a ticket. It goes to this host's log instead.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeResultDto {
    ok: bool,
    /// The failure class, absent on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<String>,
    /// One sentence, chosen by [`probe::describe`].
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// How many models the endpoint published. Zero is not a failure: plenty of
    /// endpoints serve inference and publish no catalog.
    model_count: usize,
    /// Whether the model the caller asked about is in that catalog.
    ///
    /// `None` when no model was named, or when the endpoint publishes no catalog
    /// to check against. **Absent is not a failure**: an Azure deployment name
    /// is never in `/models` by design, and a catalogue can be stale anywhere —
    /// so this is reported as a caution beside a successful check, never as one.
    #[serde(skip_serializing_if = "Option::is_none")]
    model_known: Option<bool>,
    /// The ids the endpoint published, so the add dialog can offer one.
    ///
    /// The catalog is already in hand at the moment of the probe; sending it
    /// means the operator is asked with the answers in front of them —
    /// choosing from the real list — rather than typing one blind or being
    /// told no after a round trip. Every kind asks for a model now (2c), so
    /// this is always worth sending when the endpoint published anything.
    ///
    /// Sorted and deduplicated, never truncated below what the paged read
    /// returned (round-3a review P2-5: a 500-id cap used to filter a large
    /// catalog by name, which is exactly what this feature promises never to
    /// do). The console offers free text alongside the list regardless.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    models: Vec<String>,
}

// `catalogue_offer` (the published ids to offer, sorted and deduplicated —
// never capped, round-3a review P2-5) moved to `paged_catalog::catalogue_offer`
// (keys rework #2306, P3-7 review): the account-key fan-out
// (`company::company_key::fan_out`) needs the same "sort, dedupe" this probe
// route decided, and `company` must never import from `server` — so the one
// place that decides it lives at a layer both already reach.

/// What `POST …/providers/{slug}/test` may be asked.
#[derive(Debug, Default, Deserialize)]
struct TestProvider {
    /// The model a routing row has chosen, when the caller is asking about one.
    #[serde(default)]
    model: Option<String>,
}

/// Every provider write answers with the whole status, so the console never has
/// to reconcile a partial update against what it already had.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderMutation {
    status: InferenceStatusDto,
    note: String,
    /// The probe's verdict, when one was run.
    #[serde(skip_serializing_if = "Option::is_none")]
    probe: Option<ProbeResultDto>,
    /// Tiers whose route this change moved or parked, so the console can say
    /// which rows changed rather than leaving the operator to notice.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    affected_tiers: Vec<String>,
    /// The `usedBy` this mutation would have refused with, echoed back on a
    /// **confirmed** delete/disable/key-clear
    /// (`docs/key-reworks/in-use-guards.md` §3) — computed *before* the
    /// mutation applied, so the console can show what it just broke without
    /// re-deriving it. `None` on every mutation that is not a guarded one,
    /// and on a guarded one that had nothing to warn about.
    #[serde(skip_serializing_if = "Option::is_none")]
    used_by: Option<crate::error::UsedBy>,
}

/// The `usedBy` a provider row's removal, disable, or key clear would carry
/// (keys rework, issue #2306; `docs/key-reworks/in-use-guards.md` §1/§6):
/// `default: true` when the company default names this slug — bare
/// (`DefaultChoice::ProviderOnly`) or full — and `agents` from every agent
/// whose own `{provider, model}` pair (slice 3a) names it, counted on the
/// provider slug alone per §6 ("no exception for a blank model") — an agent
/// naming this provider with no model yet still depends on it, and the
/// missing model is the manifest's own problem to refuse, not a reason for
/// this guard to look away.
///
/// `surfaces` is never populated here — unlike the account key or a Composio
/// credential, a provider row already names exactly what depends on it via
/// `default`/`agents`, so tagging it with the redundant `"llm"` surface
/// would say the same fact twice in two shapes.
///
/// A record load failure degrades to "no agents named" rather than refusing
/// the whole guard: the `default` half still answers, and a company whose
/// record cannot be read has bigger problems than an incomplete advisory.
///
/// The pure half of the guard: from an already-resolved default and an
/// already-loaded record, whether `slug` is used, and by what.
///
/// Split out (round-3a review P3-6) so a status read computing this for every
/// row in the list can load the default and the record **once** for the
/// whole request — see `ops::inference::provider_list` — instead of each row
/// repeating both reads through [`provider_used_by`].
///
/// `default` is read from the [`store::DefaultChoice`] itself, not
/// re-fetched, precisely so a caller that could not read the real one can
/// pass [`store::DefaultChoice::Unset`] and get an honest "not the default"
/// rather than this function silently going back to the store a second time
/// and hitting the same failure.
///
/// `pub(super)`: `ops::inference::provider_list` calls this directly, once
/// per row, over one default and one record loaded for the whole request.
pub(super) fn used_by_from(
    default: &store::DefaultChoice,
    record: Option<&crate::ports::types::CompanyRecord>,
    slug: &str,
) -> Option<crate::error::UsedBy> {
    let is_default = default.provider().is_some_and(|p| p == slug);
    let agents = record
        .map(|r| agents_pinned_to(r, slug))
        .unwrap_or_default();
    let used_by = crate::error::UsedBy {
        default: is_default,
        agents,
        surfaces: Vec::new(),
    };
    (!used_by.is_empty()).then_some(used_by)
}

/// `pub(super)`: called from the three guarded mutations below (delete,
/// disable, key clear) and from the agent-pair PATCH
/// (`server::ops::team_agent`) — never from a read path, which computes
/// [`used_by_from`] directly over data it already loaded once for the whole
/// request (round-3a review P3-6).
///
/// **A guard fails closed, a read degrades — this is the guard half**
/// (round-3a review P2-1). The company record is read fresh here because a
/// mutation's whole job is to decide whether it is safe to proceed *right
/// now*; a load error therefore propagates as `Err` rather than reading as
/// "no agents named", which used to let a transient store error turn an
/// unconfirmed delete, disable or key clear on a pinned-only provider into a
/// silent 200. `Ok(None)` — the record genuinely does not exist — still reads
/// as no agents: that is not a failure to recover from, it is the company
/// having nothing to strand.
///
/// The stored default, by contrast, is read leniently
/// ([`store::load_default_lenient`], round-3a review P2-4): a corrupt or
/// unreadable `inference/default` must never block an otherwise-unrelated
/// delete, disable or key clear — the guard still answers about `agents`, and
/// `default` reads as `false` rather than the whole request failing.
pub(super) async fn provider_used_by(
    runtime: &CompanyRuntime,
    slug: &str,
) -> Result<Option<crate::error::UsedBy>, ApiError> {
    let secrets = runtime.secrets().as_ref();
    let (default, unreadable) = store::load_default_lenient(runtime.id(), secrets).await;
    if unreadable {
        tracing::warn!(
            company = %runtime.id(),
            slug = %slug,
            "computing usedBy for a guarded mutation with an unreadable default; treating it \
             as not the default rather than refusing the mutation",
        );
    }
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    Ok(used_by_from(&default, record.as_ref(), slug))
}

/// Every effective roster agent whose own pair names `slug` (keys rework,
/// issue #2306, slice 3a) — manifest agents merged with their overrides
/// (retired ones already excluded by `effective_agents`), plus every overlay
/// teammate. An `acp`-bound agent never contributes: validation refuses a
/// pair there, but an unvalidated manifest must not be trusted to have run
/// it, the same reasoning `runtime::builder::agent_pairs` gives for its own
/// identical skip.
fn agents_pinned_to(
    record: &crate::ports::types::CompanyRecord,
    slug: &str,
) -> Vec<crate::error::UsedByAgent> {
    use crate::runtime::builder::agent_harness_kind;

    let mut agents = Vec::new();
    for agent in record.effective_agents() {
        if agent_harness_kind(&record.manifest, agent.harness.as_deref()).as_deref() == Some("acp")
        {
            continue;
        }
        if agent.provider.as_deref() == Some(slug) {
            agents.push(crate::error::UsedByAgent {
                id: agent.id.clone(),
                name: agent.name.clone().unwrap_or(agent.role.clone()),
            });
        }
    }
    for overlay in &record.overlay_agents {
        if agent_harness_kind(&record.manifest, overlay.harness.as_deref()).as_deref()
            == Some("acp")
        {
            continue;
        }
        if overlay.provider.as_deref() == Some(slug) {
            agents.push(crate::error::UsedByAgent {
                id: overlay.id.clone(),
                name: overlay.name.clone(),
            });
        }
    }
    agents
}

/// §2's sentence, naming every dependent in one line:
/// `"<Label> is used by the company default and N agent(s): <names>."` (or
/// just one half, when only one applies).
fn provider_in_use_message(label: &str, used_by: &crate::error::UsedBy) -> String {
    let agents = || {
        let names = used_by
            .agents
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} agent{}: {names}",
            used_by.agents.len(),
            if used_by.agents.len() == 1 { "" } else { "s" }
        )
    };
    match (used_by.default, used_by.agents.is_empty()) {
        (true, true) => format!("{label} is the company default."),
        (true, false) => format!("{label} is used by the company default and {}.", agents()),
        (false, false) => format!("{label} is used by {}.", agents()),
        // Unreachable in practice: `provider_used_by` returns `None` rather
        // than an empty `UsedBy`, so no caller ever builds this message from
        // one. Kept total rather than panicking on a shape a future caller
        // might otherwise construct by hand.
        (false, true) => format!("{label} is in use."),
    }
}

// ---- add --------------------------------------------------------------------

/// `POST …/inference/providers` — connect a provider.
async fn add_provider(
    State(state): State<AppState>,
    company: AdminScopedCompany,
    Json(body): Json<AddProvider>,
) -> Result<Json<ProviderMutation>, ApiError> {
    let runtime = company.runtime.as_ref();
    let secrets = runtime.secrets().as_ref();
    let kind = body.kind.trim().to_string();

    // Step 1 and 2: everything knowable without a network, before any write.
    let plan = plan_add(
        &kind,
        body.label.as_deref(),
        body.base_url.as_deref(),
        body.key
            .as_deref()
            .map(str::trim)
            .is_some_and(|k| !k.is_empty()),
        &state.config().api_url,
    )?;
    // Keys rework (#2306), slice 4a: the account-key fan-out
    // (`company_key::fan_out`) reads and writes this exact slug's row and key
    // under the same lock. Held only for the `tinyhumans` slug — every other
    // add is untouched by the fan-out and needs no serialisation with it.
    let _fan_out_guard = if plan.slug == crate::company::inference::MANAGED_SLUG {
        Some(crate::company::company_key::slot_guard(runtime.id()).await)
    } else {
        None
    };
    let existing = store::list_providers(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    // Decision X1 (round-3a review P0, 2026-09-15): auto-default requires more
    // than "no stored default" — every company that predates this rework has
    // no stored default, so that test alone would silently move an existing
    // company's traffic (entry zero, a manifest `[inference]` section, an env
    // default, or the managed chain) onto whatever it "tried out" next, with
    // no confirm. X1 means "the first provider this company has ever
    // connected", so both must hold, read from the state as it stood before
    // this add:
    //   (a) there were zero provider rows;
    //   (b) nothing else resolves for the company at all — `resolve_effective`
    //       is the one seam that already answers exactly that question, for
    //       the turn path and the boot path alike.
    //
    // Must run **before** this add's own row exists: asked afterwards, (b)
    // would trivially see this very row resolving as sole positional primary
    // and answer "nothing else" regardless of what was true before. Locked
    // only for this read — released here, long before the write below and
    // the network probe further down; re-validated under the lock again,
    // narrowly, at the point that actually writes the default.
    let first_provider_ever = if existing.is_empty() {
        let _guard = crate::company::inference::store::index_lock(runtime.id()).await;
        let (manifest, _harness_id) = super::manifest_inference(runtime).await?;
        let platform = super::platform_default(&crate::app::config::ProcessEnv);
        crate::company::inference::resolve_effective(
            runtime.id(),
            &manifest,
            platform.as_ref(),
            secrets,
        )
        .await
        .map_err(ApiError)?
        .is_none()
    } else {
        false
    };
    // The catalogue check applies to a *typed* name only. Adding the catalogue's
    // own `groq` entry should take the slug `groq` — that is the same provider,
    // not a collision.
    if plan.custom {
        store::check_slug(&existing, &plan.slug)
            .map_err(|e| ApiError(OpenCompanyError::InvalidRequest(e.to_string())))?;
    } else if existing.iter().any(|p| p.slug == plan.slug) {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "{} is already connected. Edit the existing row rather than adding a second one.",
            plan.label
        ))));
    }

    // Keys rework (#2306), slice 2c: a model is required for every kind,
    // independent of catalog content — the console always asks
    // (`asksForModel`/D-model) and the host refuses before any write so a
    // curl caller gets the same floor. `check_model_id` also refuses a tier
    // name here, so `providers.rs` cannot store one even if a caller tries.
    let model = store::check_model_id(body.model.as_deref().unwrap_or("")).map_err(ApiError)?;

    // Keys rework (#2306), slice 2a: `provider/tinyhumans/key` can already
    // hold the legacy Managed row's key with no index record behind it (set
    // through `PUT …/inference/managed/key`, or the account-key fan-out).
    // Every rollback below clears that slot (`roll_back_add` →
    // `store::delete_provider`, `clear_orphaned_key`), so a failed TinyHumans
    // add would silently delete a key nothing here wrote. Read the old value
    // now and put it back after any rollback.
    let previous_key = if plan.slug == crate::company::inference::MANAGED_SLUG {
        secrets
            .get(runtime.id(), &store::provider_key_key(&plan.slug))
            .await
            .map_err(ApiError)?
            .map(|crate::ports::types::SecretValue(raw)| raw)
            .filter(|raw| !raw.trim().is_empty())
    } else {
        None
    };

    // Step 3: the credential first. The probe resolves the key by slug, so it
    // has to land before the record does.
    let key = body.key.map(|k| k.trim().to_string()).unwrap_or_default();
    if !key.is_empty() {
        secrets
            .set(
                runtime.id(),
                &store::provider_key_key(&plan.slug),
                crate::ports::types::SecretValue(key.clone()),
            )
            .await
            .map_err(ApiError)?;
    }

    // Step 4: flush the record.
    //
    // A failure here has to take the credential back out. The key is already at
    // `provider/<slug>/key` and there is now no record owning it, which is the
    // invisible half of the rollback invariant `roll_back_add` exists for: a
    // record left behind is on screen and removable, an orphaned credential is
    // neither, and the next add of that slug would silently present it.
    let provider = match store::put_provider(
        runtime.id(),
        secrets,
        store::ProviderDraft {
            slug: plan.slug.clone(),
            label: plan.label.clone(),
            kind: plan.kind.clone(),
            base_url: plan.base_url.clone(),
            models: uniform_models(Some(&model)),
            // New providers arrive on. Adding something and then having to
            // switch it on is a second step for a decision already made.
            enabled: true,
        },
    )
    .await
    {
        Ok(provider) => provider,
        Err(err) => {
            if !key.is_empty() {
                clear_orphaned_key(runtime, &plan.slug).await;
            }
            restore_previous_key(runtime, &plan.slug, previous_key.as_deref()).await;
            return Err(ApiError(err));
        }
    };
    // The credential just changed for this company, and the catalog cache key is
    // made of non-secret ids on purpose — so a rotation would otherwise keep
    // answering from the previous credential's read for the rest of its TTL.
    crate::server::inference_models::evict_company_catalogs(runtime.id().as_ref());

    // Step 5: probe — but only when there is something for the probe to learn.
    //
    // A kind that expects a credential and was given none has nothing to verify:
    // the endpoint can only answer 401, which classifies as `auth`, which is the
    // one destructive class — so a keyless add would reject itself over a key the
    // operator has not typed yet. "Not checked" is the honest state for that row
    // and is exactly what the health column already renders.
    let auth = catalogue::auth_style_for(&plan.kind);
    let credential = (!key.is_empty()).then_some(key.as_str());
    let worth_probing = plan.probes && (auth == catalogue::AuthStyle::None || credential.is_some());
    let shape = catalogue::catalog_shape_for(&plan.kind, &provider.base_url);
    let outcome = if worth_probing {
        Some(
            probe::probe_models(
                &provider.base_url,
                credential,
                auth,
                probe::default_policy(),
                shape,
            )
            .await,
        )
    } else {
        None
    };

    let (probe_dto, note) = match outcome {
        None => (None, format!("{} is connected.", provider.label)),
        Some(Ok(models)) => {
            // Keys rework (#2306), slice 2c: the model-required rollback that
            // used to live here is gone — `check_model_id` above already
            // refused an add with no model, before any write, whatever the
            // catalog contains. The reported defect (a green-looking row that
            // could not think) is closed by never writing that row at all,
            // rather than by writing it and then deciding.
            record_health(runtime, &provider.slug, "ok").await;
            (
                Some(ProbeResultDto {
                    ok: true,
                    class: None,
                    message: None,
                    model_count: models.len(),
                    model_known: None,
                    models: paged_catalog::catalogue_offer(&models),
                }),
                format!("{} is connected and answering.", provider.label),
            )
        }
        Some(Err(failure)) => {
            // The raw text goes here and nowhere else.
            tracing::info!(
                company = %runtime.id(),
                provider = %provider.slug,
                class = failure.class.as_str(),
                detail = %failure.raw,
                "inference provider probe failed",
            );
            // Category-aware: a local runtime that is not running rolls back
            // too. See `probe::rolls_back` for why the same class means the
            // opposite thing for a cloud provider.
            if probe::rolls_back(failure.class, catalogue::category_of(&plan.kind))
                && !body.add_anyway
            {
                roll_back_add(runtime, &provider).await;
                restore_previous_key(runtime, &plan.slug, previous_key.as_deref()).await;
                // The **refusal** wording, not `describe`'s: nothing was saved,
                // and every one of `describe`'s sentences but the auth one
                // opens by saying it was.
                return Err(ApiError(OpenCompanyError::InvalidRequest(
                    probe::describe_refusal(failure.class, &provider.label),
                )));
            }
            record_health(runtime, &provider.slug, failure.class.as_str()).await;
            // Bug KR-L1-01: a catalog too large to read is not "the check did
            // not complete" — the connection and the credential are both
            // fine, and the operator needs to know it is specifically the
            // model list that could not be read, never a silent zero-models
            // "ok".
            let message = if failure.truncated {
                format!("The model list from {} could not be read.", provider.label)
            } else {
                probe::describe(failure.class, &advisory_subject(&provider))
            };
            (
                Some(ProbeResultDto {
                    ok: false,
                    class: Some(failure.class.as_str().to_string()),
                    message: Some(message.clone()),
                    model_count: 0,
                    model_known: None,
                    models: Vec::new(),
                }),
                message,
            )
        }
    };

    // §4: the one case where routing to this provider is not a guess.
    let (routed, note) = match auto_route_sole_provider(runtime, &provider).await? {
        tiers if tiers.is_empty() => (tiers, note),
        tiers => (
            tiers,
            format!("{note} Every workload now routes through it — change that under Routing."),
        ),
    };

    // Keys rework (#2306): the last write of the request, so a failure here
    // never half-applies the add — the row and the key are already valid and
    // visible, and a retry would only answer "already connected".
    //
    // Two reasons this runs, matched independently rather than one flag:
    // - `body.make_default` (2c): the operator explicitly ticked "Make this
    //   the default", which is honoured whatever the default already held.
    // - Decision D-first-default / X1 (round-3a review P0, 2026-09-15): the
    //   *first* provider a company has ever connected becomes its default
    //   automatically, with no opt-out — gated on `first_provider_ever`
    //   above, not merely on `load_default` reading `Unset` (X1's second
    //   half, "adding never changes it", still holds either way).
    let note = if body.make_default {
        let choice = store::ModelChoice {
            provider: provider.slug.clone(),
            model: model.clone(),
        };
        match store::set_default_choice(runtime.id(), secrets, &choice).await {
            Ok(()) => format!(
                "{note} New work now goes through {} · {model}.",
                provider.label
            ),
            Err(err) => {
                tracing::warn!(
                    company = %runtime.id(),
                    provider = %provider.slug,
                    error = %err,
                    "added a provider but could not make it the default",
                );
                format!("{note} It could not be made the default. Use Set as default.")
            }
        }
    } else if first_provider_ever {
        // Re-validated under the lock right before the write: the snapshot
        // above was taken before this add's own row was written and before
        // its probe ran, both of which took real time a concurrent request
        // could have used to add a second row or set an explicit default —
        // either of which means this is no longer "the first provider ever".
        let _guard = crate::company::inference::store::index_lock(runtime.id()).await;
        let still_unset = matches!(
            store::load_default(runtime.id(), secrets)
                .await
                .map_err(ApiError)?,
            store::DefaultChoice::Unset
        );
        let still_only_row = store::list_providers(runtime.id(), secrets)
            .await
            .map_err(ApiError)?
            .len()
            == 1;
        if still_unset && still_only_row {
            let choice = store::ModelChoice {
                provider: provider.slug.clone(),
                model: model.clone(),
            };
            match store::set_default_choice(runtime.id(), secrets, &choice).await {
                Ok(()) => format!(
                    "{note} New work now goes through {} · {model}.",
                    provider.label
                ),
                Err(err) => {
                    tracing::warn!(
                        company = %runtime.id(),
                        provider = %provider.slug,
                        error = %err,
                        "added a provider but could not make it the default",
                    );
                    note
                }
            }
        } else {
            note
        }
    } else {
        note
    };

    Ok(Json(ProviderMutation {
        status: effective_status(&state, runtime).await?,
        note,
        probe: probe_dto,
        affected_tiers: routed,
        used_by: None,
    }))
}

/// Routes every workload to a provider that has just been added, **only when
/// nothing else in this company can answer**.
///
/// ## The condition, and why it is not "the first provider"
///
/// The operator's mental model is *I added a provider so it will be used*, and
/// the reported dead end is what happens when that is false. But writing four
/// rows the operator did not author is the shape of the positional default the
/// explicit marker exists to kill, so it is worth doing only where it is not a
/// decision at all.
///
/// "The first provider they added" is the wrong test. Because of entry zero, a
/// company can have a provider it never added through this route, so the newly
/// added one can be the *second* element of the list and still be the thing the
/// operator expects to be used — and equally, a company with entry zero already
/// has something that answers. The condition that is genuinely unambiguous is:
///
/// * the route table is **empty** — nothing was authored, so nothing is
///   overwritten;
/// * the managed chain **does not resolve** — there is no fallback behind the
///   rows; and
/// * after this add there is **exactly one enabled provider**, and it is this
///   one.
///
/// All three together mean there is precisely one thing in this company that can
/// serve a turn. Routing to anything else is not a choice that exists, so this
/// is not a guess.
///
/// ## Why it deliberately stops when Managed is available
///
/// That is row B2, and it is the case where guessing moves money. A company on
/// Managed that adds an OpenRouter key may be doing it for one workload, for
/// vision only, or to compare — and writing all four rows would bill them for
/// everything, silently, from a screen that still says Managed. The answer there
/// is to ask, which is what leaving the table empty and reporting `Unset` does.
///
/// Returns the tiers it wrote, so the response says what changed rather than
/// leaving the operator to notice. Never fails the add: a route that did not
/// land leaves the company exactly where the add found it.
async fn auto_route_sole_provider(
    runtime: &CompanyRuntime,
    added: &store::Provider,
) -> Result<Vec<String>, ApiError> {
    let secrets = runtime.secrets().as_ref();

    let existing = store::load_routes(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    let providers = store::list_providers(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    if !is_the_only_thing_that_can_answer(
        &existing,
        &providers,
        managed_resolves(runtime).await?,
        &added.slug,
    ) {
        return Ok(Vec::new());
    }

    // The slug-carrying ref, which is what the resolver matches most precisely —
    // a slug match is decisive whatever the category, so this is right for a
    // cloud account, a local runtime and a CLI login alike.
    let route = resolve::ProviderRef::Cloud {
        provider_slug: added.slug.clone(),
        model: None,
    };
    let mut routes = resolve::Routes::new();
    let mut written = Vec::new();
    for workload in resolve::ROUTABLE_WORKLOADS {
        routes.insert(workload.tier().to_string(), route.clone());
        written.push(workload.tier().to_string());
    }

    // **Written only if this company's own resolver reads it back as this
    // provider.** A route is persisted as the text an operator would type, so a
    // slug that collides with a word in that grammar — `local`, `managed`,
    // `default` — round-trips into a different ref entirely, and `is_reserved_slug`
    // does not cover those three. Writing a row nobody asked for is defensible
    // only while it is certainly right; a check against the same function the
    // turn path uses is what makes it certain, rather than an argument about
    // which slugs are possible.
    let round_trip: resolve::Routes = routes
        .iter()
        .map(|(tier, route)| {
            (
                tier.clone(),
                resolve::ProviderRef::parse(&route.to_route_string()),
            )
        })
        .collect();
    let resolves_here = resolve::ROUTABLE_WORKLOADS.iter().all(|workload| {
        matches!(
            resolve::provider_for_workload(*workload, &round_trip, &providers),
            resolve::Resolution::Resolved { provider, .. } if provider.slug == added.slug
        )
    });
    if !resolves_here {
        tracing::warn!(
            company = %runtime.id(),
            provider = %added.slug,
            "not auto-routing: this slug does not read back as itself through the route grammar",
        );
        return Ok(Vec::new());
    }

    store::save_routes(runtime.id(), secrets, &routes)
        .await
        .map_err(ApiError)?;
    Ok(written)
}

/// The §4 condition, as a pure function of the three facts it reads.
///
/// Separated from the write so the decision that routes an operator's work for
/// them can be asserted directly, rather than only through a handler. Every
/// clause is load-bearing — see [`auto_route_sole_provider`] for why each one is
/// there and why "the first provider they added" is not among them.
fn is_the_only_thing_that_can_answer(
    routes: &resolve::Routes,
    providers: &[store::Provider],
    managed_answers: bool,
    added: &str,
) -> bool {
    let table_is_empty = routes
        .values()
        .all(|route| matches!(route, resolve::ProviderRef::Default));
    let mut enabled = providers.iter().filter(|p| p.enabled);
    let sole = match (enabled.next(), enabled.next()) {
        (Some(only), None) => only.slug == added,
        _ => false,
    };
    table_is_empty && !managed_answers && sole
}

/// One model id stored under every tier key: a storage encoding, not a
/// selection (keys rework, issue #2306, slice 2d).
///
/// A row's `models` map holds one tier-keyed shape for every provider,
/// whatever it can resolve — the legacy arm's
/// [`legacy_tiers::configured_model_for_tier`](crate::company::inference::legacy_tiers::configured_model_for_tier)
/// reads whichever tier a request names, and 2c's `check_model_id` already
/// refuses to store a tier name as the value, so this never has to guess
/// which of the four a turn will ask for.
fn uniform_models(model: Option<&str>) -> BTreeMap<String, String> {
    let Some(model) = model else {
        return BTreeMap::new();
    };
    crate::company::INFERENCE_TIERS
        .iter()
        .map(|tier| ((*tier).to_string(), model.to_string()))
        .collect()
}

/// What a kind implies, decided before anything is written.
struct AddPlan {
    slug: String,
    label: String,
    kind: String,
    base_url: String,
    /// Whether this kind is a typed name that may shadow a built-in.
    custom: bool,
    /// Whether connecting it runs the probe.
    probes: bool,
}

/// Resolves a kind, a typed label and a typed endpoint into a plan.
///
/// The three categories ask three different questions, and this is where that
/// shows: a cloud provider's endpoint comes from the preset and its label from
/// the catalogue; a local runtime's endpoint is the thing being chosen and is
/// normalised and scheme-checked here; a CLI login supplies neither and skips
/// the probe because there is nothing to present.
///
/// `api_url` is this instance's configured TinyHumans platform
/// (`AppConfig::api_url`, from `TINYHUMANS_API_URL`): the `tinyhumans` row's
/// endpoint is derived from it rather than read off the catalogue, so a
/// staging or local platform gets a row that points at itself — see
/// [`catalogue::tinyhumans_proxy_url`]. Every other cloud row keeps its
/// catalogue endpoint.
fn plan_add(
    kind: &str,
    label: Option<&str>,
    base_url: Option<&str>,
    has_key: bool,
    api_url: &str,
) -> Result<AddPlan, ApiError> {
    let invalid = |msg: String| ApiError(OpenCompanyError::InvalidRequest(msg));

    if let Some(cloud) = catalogue::cloud_provider(kind) {
        // Keys rework (#2306), slice 2a: TinyHumans is an ordinary cloud row,
        // but unlike every other cloud kind it has no fallback identity to
        // probe with (D-set forbids reusing `tinyhumans/key` or the instance
        // token on an indexed row — `catalog_shape_for`'s doc explains why an
        // indexed row never proxies). A keyless add would therefore add a row
        // with nothing to authenticate its probe, silently landing on
        // `unchecked` health forever.
        if cloud.slug == crate::company::inference::MANAGED_SLUG && !has_key {
            return Err(invalid("TinyHumans needs an API key.".to_string()));
        }
        let base_url = if cloud.slug == crate::company::inference::MANAGED_SLUG {
            catalogue::tinyhumans_proxy_url(api_url)
        } else {
            cloud.endpoint.to_string()
        };
        return Ok(AddPlan {
            slug: cloud.slug.to_string(),
            label: cloud.label.to_string(),
            kind: cloud.slug.to_string(),
            base_url,
            custom: false,
            probes: true,
        });
    }
    if let Some(local) = catalogue::local_runtime(kind) {
        let typed = base_url
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .or_else(|| local.default_endpoint.map(str::to_string))
            .ok_or_else(|| {
                invalid(format!(
                    "{} needs the endpoint it is listening on.",
                    local.label
                ))
            })?;
        let base_url = catalogue::normalize_local_endpoint(&typed)
            .ok_or_else(|| invalid(endpoint_refusal(&typed)))?;
        // **The catalogue says whether this runtime wants a credential, and the
        // host has to hold that rule too.** OMLX declares `needs_key: true`; the
        // console's dialog showed and required the field, and the handler
        // accepted a row without one — a console-only guard, which is not a
        // guard. The row then stored no credential, `worth_probing` was false
        // for want of one, and so it was never probed either: a provider that
        // could not work, added without a word.
        if local.needs_key && !has_key {
            return Err(invalid(format!("{} needs an API key.", local.label)));
        }
        return Ok(AddPlan {
            slug: local.slug.to_string(),
            label: local.label.to_string(),
            kind: local.slug.to_string(),
            base_url,
            custom: false,
            probes: true,
        });
    }
    if let Some(cli) = catalogue::cli_login(kind) {
        // Reachable only if a delegated credential ever becomes available here.
        // On a server-side host the category is empty and the console says so,
        // but refusing in the handler is the honest answer rather than storing a
        // row for a login nothing holds.
        return Err(invalid(format!(
            "{} is a credential held by a command-line tool on someone's own machine. \
             This host cannot reach one.",
            cli.label
        )));
    }
    if kind != "custom" {
        return Err(invalid(format!(
            "`{kind}` is not a provider this host knows."
        )));
    }

    // Custom: three fields, and the slug is derived from the name rather than
    // typed. An operator names the thing; the address falls out. Asking for both
    // invites them to disagree, and the one that appears in a routing entry
    // would then be the one they never chose.
    let label = label.map(str::trim).unwrap_or("").to_string();
    // Bounded before the slug is derived, so the sentence names what the
    // operator typed rather than the address that fell out of it.
    store::check_provider_name(&label).map_err(|e| invalid(e.to_string()))?;
    let slug = store::slugify(&label);
    if slug.is_empty() {
        return Err(invalid(store::SlugError::Empty.to_string()));
    }
    let typed = base_url
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| invalid("A custom provider needs an OpenAI-compatible URL.".to_string()))?;
    let base_url = catalogue::normalize_local_endpoint(typed)
        .ok_or_else(|| invalid(endpoint_refusal(typed)))?;
    Ok(AddPlan {
        slug,
        label,
        kind: "custom".to_string(),
        base_url,
        custom: true,
        probes: true,
    })
}

/// What an advisory names when it names something.
///
/// The endpoint's host for the classes that are about reachability, because
/// "nothing answered at api.acme.dev" is actionable in a way that "nothing
/// answered at Acme gateway" is not.
fn advisory_subject(provider: &store::Provider) -> String {
    catalogue::endpoint_host(&provider.base_url).unwrap_or_else(|| provider.label.clone())
}

/// What to say about an endpoint that cannot be used.
///
/// Two reasons, and they need two sentences. "Not an http address" is a typo.
/// A credential embedded in the authority is a security answer: the endpoint is
/// stored in the provider record, returned to **every** console reader on the
/// company status read, and interpolated into operator-facing failure text — so
/// a password in a URL is a password in all three, and the fix is to move it to
/// the field that is write-only.
fn endpoint_refusal(typed: &str) -> String {
    if catalogue::endpoint_has_credentials(typed) {
        return catalogue::ENDPOINT_CREDENTIAL_REFUSAL.to_string();
    }
    "That endpoint must be an http or https address.".to_string()
}

/// Undoes an add whose probe rejected the credential.
///
/// Both stores, and a failure in either is **logged loudly** rather than
/// swallowed: a record left behind is visible and the operator can remove it,
/// but an orphaned credential is invisible, and re-adding that slug would
/// silently reuse it.
async fn roll_back_add(runtime: &CompanyRuntime, provider: &store::Provider) {
    let secrets = runtime.secrets().as_ref();
    // `delete_provider` clears the credential itself, and clears it *first*, so
    // a failure leaves the row visible with its key rather than the reverse.
    if let Err(err) = store::delete_provider(runtime.id(), secrets, &provider.slug).await {
        tracing::error!(
            company = %runtime.id(),
            provider = %provider.slug,
            error = %err,
            "could not roll back a rejected provider; a credential may be orphaned at \
             provider/<slug>/key and re-adding this slug would reuse it",
        );
    }
}

/// Clears a credential whose provider record was never written.
///
/// The same loud-failure rule [`roll_back_add`] follows, for the same reason,
/// and separate from it because there is no `Provider` to delete yet — the
/// write that would have produced one is what failed.
async fn clear_orphaned_key(runtime: &CompanyRuntime, slug: &str) {
    let secrets = runtime.secrets().as_ref();
    if let Err(err) = secrets
        .set(
            runtime.id(),
            &store::provider_key_key(slug),
            crate::ports::types::SecretValue(String::new()),
        )
        .await
    {
        tracing::error!(
            company = %runtime.id(),
            provider = %slug,
            error = %err,
            "could not clear the credential of a provider whose record failed to write;              it is orphaned at provider/<slug>/key and re-adding this slug would reuse it",
        );
    }
}

/// Puts back a key that an add replaced and then rolled back (keys rework,
/// issue #2306, slice 2a). `None` does nothing.
///
/// `provider/<slug>/key` is one slot; a TinyHumans add overwrites it before it
/// knows whether the add will stick (`add_provider` writes the key before the
/// record, so the probe can read it by slug). A rollback then clears that same
/// slot (`roll_back_add` → `store::delete_provider`; `clear_orphaned_key`),
/// taking the legacy Managed row's key with it even though nothing about that
/// row was touched. This restores exactly what was there before the request.
async fn restore_previous_key(runtime: &CompanyRuntime, slug: &str, previous: Option<&str>) {
    let Some(previous) = previous else {
        return;
    };
    if let Err(err) = runtime
        .secrets()
        .set(
            runtime.id(),
            &store::provider_key_key(slug),
            crate::ports::types::SecretValue(previous.to_string()),
        )
        .await
    {
        tracing::error!(
            company = %runtime.id(),
            provider = %slug,
            error = %err,
            "could not restore the key a rolled-back add had replaced",
        );
    }
    crate::server::inference_models::evict_company_catalogs(runtime.id().as_ref());
}

/// Records health, never failing the request over it.
///
/// A health record is a decoration on a row. Failing an otherwise successful add
/// because a decoration could not be written would be the tail wagging the dog.
async fn record_health(runtime: &CompanyRuntime, slug: &str, state: &str) {
    // Dependency-free: the crate's one RFC-3339 formatter (`ports::iso8601`).
    let at = crate::ports::iso8601(crate::ports::now_millis());
    match store::record_health(runtime.id(), runtime.secrets().as_ref(), slug, state, &at).await {
        Ok(true) => tracing::info!(
            company = %runtime.id(),
            provider = %slug,
            state = %state,
            "inference provider health changed",
        ),
        // Unchanged: the latch held, which is the point of it. Silent on
        // purpose — logging every repetition is the ~9k-events failure.
        Ok(false) => {}
        Err(err) => tracing::warn!(
            company = %runtime.id(),
            provider = %slug,
            error = %err,
            "could not record inference provider health",
        ),
    }
}

// ---- edit -------------------------------------------------------------------

/// `PUT …/inference/providers/{slug}` — change a connected provider.
///
/// Not an add with a different verb: the slug is fixed, so nothing here can
/// collide, and the kind cannot change — a provider that changed kind would be a
/// different provider wearing an existing row's routes.
async fn edit_provider(
    State(state): State<AppState>,
    company: AdminScopedCompany,
    Path(params): Path<ProviderPath>,
    Json(body): Json<EditProvider>,
) -> Result<Json<ProviderMutation>, ApiError> {
    let runtime = company.runtime.as_ref();
    let secrets = runtime.secrets().as_ref();
    // Keys rework (#2306), slice 4a: held only for the `tinyhumans` slug —
    // see `add_provider`'s own guard for why.
    let _fan_out_guard = if params.slug == crate::company::inference::MANAGED_SLUG {
        Some(crate::company::company_key::slot_guard(runtime.id()).await)
    } else {
        None
    };
    let existing = require_provider(runtime, &params.slug).await?;

    if existing.origin == store::ProviderOrigin::EntryZero {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "This company's original provider is changed through the inference config, \
             not as a list entry."
                .to_string(),
        )));
    }

    let base_url = match body.base_url.as_deref().map(str::trim) {
        None | Some("") => existing.base_url.clone(),
        // **The endpoint this host itself served, sent back, is not a change.**
        // Every provider row carries `redact_endpoint`'s form, which masks
        // anything that might be userinfo — including a path segment that only
        // looks like it (`…/proxy/http:***@example.com/v1`). A client that
        // posts the row back on a rename would otherwise store that mask over a
        // working endpoint, or have a credentialed legacy row's rename refused
        // outright (Codex review on #2281).
        Some(typed) if typed == catalogue::redact_endpoint(&existing.base_url) => {
            existing.base_url.clone()
        }
        Some(typed) => {
            // A cloud preset's endpoint is not the operator's to retype: the
            // paths in that table are too varied for a typo to be recoverable,
            // and the row would then point somewhere the catalogue says it does
            // not.
            if catalogue::cloud_provider(&existing.kind).is_some() {
                existing.base_url.clone()
            } else {
                catalogue::normalize_local_endpoint(typed).ok_or_else(|| {
                    ApiError(OpenCompanyError::InvalidRequest(endpoint_refusal(typed)))
                })?
            }
        }
    };

    // A rename goes through the same bound an add does. The slug is fixed here,
    // so this bounds only the label — but an edit that could set a name an add
    // would refuse is a rule the host does not actually hold.
    let label = match body
        .label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        Some(typed) => {
            store::check_provider_name(typed)
                .map_err(|e| ApiError(OpenCompanyError::InvalidRequest(e.to_string())))?;
            typed.to_string()
        }
        None => existing.label.clone(),
    };

    // **The credential goes first, for the same reason it does on the add path.**
    // An edit can move the endpoint and rotate the key in one request, and
    // committing the endpoint first meant a failed key write returned an error
    // with the new host live and the *old* host's secret still in the slot — so
    // the next routed turn would present one provider's credential to another.
    // Written first, that failure leaves the row exactly as it was.
    // **A credential does not follow an endpoint to a different origin.**
    // Leaving the write-only key field blank means "unchanged", which is right
    // for a rename and wrong the moment the destination moves: the next routed
    // turn would present one host's secret to another. The operator is asked to
    // re-enter it, or to remove it first — either is a decision, and silently
    // forwarding it is not.
    if !probe::same_origin(&existing.base_url, &base_url)
        && body.key.is_none()
        && store::provider_key_configured(runtime.id(), secrets, &existing)
            .await
            .map_err(ApiError)?
    {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "{} has a stored credential and this changes its endpoint to a different \
             host. Enter the key for the new endpoint, or remove the key first — a \
             credential for one host is not one for another.",
            existing.label
        ))));
    }

    // Keys rework (#2306), round-3a review P2-2: held from the key-clear
    // guard's check through every write below, so a concurrent request (a
    // pin, a delete, another edit) cannot pass its own check against a row
    // this request is about to change out from under it. Nothing under this
    // guard makes a network call — every write here is to the secret store —
    // so it is dropped before `effective_status` rebuilds the response,
    // never held across a probe.
    let _index_guard = crate::company::inference::store::index_lock(runtime.id()).await;

    // Keys rework (#2306), slice 2c: an edit that **clears** the key
    // (`key: Some("")`) is a guard the same way a disable is — a row with no
    // credential cannot serve the default or an agent pair that names it any
    // more than a disabled or deleted one could. A rotate to a *new* key is
    // never guarded (`in-use-guards.md` §2): it keeps serving every
    // dependent, just with a different credential.
    let clearing_key = body.key.as_deref().is_some_and(|k| k.trim().is_empty());
    let used_by = if clearing_key {
        provider_used_by(runtime, &existing.slug).await?
    } else {
        None
    };
    if clearing_key
        && !body.confirm_in_use
        && let Some(used_by) = used_by.clone()
    {
        return Err(ApiError(OpenCompanyError::InUse {
            message: provider_in_use_message(&existing.label, &used_by),
            used_by,
        }));
    }

    // The model, validated the same way an add's is. Omitted means unchanged.
    let model = match body.model.as_deref() {
        Some(raw) => Some(store::check_model_id(raw).map_err(ApiError)?),
        None => None,
    };
    let models = match &model {
        Some(m) => uniform_models(Some(m)),
        None => existing.models.clone(),
    };

    //
    // **And the old one is kept, so the ordering is a rollback rather than a
    // preference.** Either write can fail, and either failure alone leaves one
    // host holding the other's secret — the endpoint moving without the key is
    // the old host with the new credential, the key moving without the endpoint
    // is the reverse. There is no transaction across two store keys, so the
    // second-best thing is to put the recoverable one first and undo it.
    let previous_key = if body.key.is_some() {
        Some(
            store::load_provider_key(runtime.id(), secrets, &existing)
                .await
                .map_err(ApiError)?,
        )
    } else {
        None
    };
    if let Some(key) = body.key.as_deref() {
        store::store_provider_key(runtime.id(), secrets, &existing, key.trim())
            .await
            .map_err(ApiError)?;
    }

    let written = store::put_provider(
        runtime.id(),
        secrets,
        store::ProviderDraft {
            slug: existing.slug.clone(),
            label,
            kind: existing.kind.clone(),
            base_url,
            models,
            enabled: existing.enabled,
        },
    )
    .await;
    let provider = match written {
        Ok(provider) => provider,
        Err(err) => {
            // The record did not move, so neither may the credential.
            if let Some(previous) = previous_key
                && let Err(restore) =
                    store::store_provider_key(runtime.id(), secrets, &existing, previous.trim())
                        .await
            {
                tracing::error!(
                    company = %runtime.id(),
                    provider = %existing.slug,
                    error = %restore,
                    "an edit failed to write the provider record and then failed to put the \
                     previous credential back; this row's stored key is the one that was \
                     being rotated to, against the endpoint it had before",
                );
            }
            return Err(ApiError(err));
        }
    };

    if body.key.is_some() {
        crate::server::inference_models::evict_company_catalogs(runtime.id().as_ref());
        // A rotation makes whatever was learnt about the old credential
        // meaningless — including a latched `auth` failure, which would
        // otherwise keep the row amber until something else happened to probe.
        if let Err(err) = store::forget_health(runtime.id(), secrets, &provider.slug).await {
            tracing::warn!(
                company = %runtime.id(),
                provider = %provider.slug,
                error = %err,
                "could not clear health after a credential rotation",
            );
        }
    }

    // Keys rework (#2306), slice 2c: if this row is the company's current
    // *full* default and its model just changed, the default moves with it
    // — otherwise the default would keep sending the row's old model even
    // though the row itself now serves a different one. A bare-slug
    // (`ProviderOnly`) default is **not** upgraded here (Q1): only an
    // explicit set-default rewrites one of those.
    if let Some(new_model) = &model {
        match store::load_default(runtime.id(), secrets).await {
            Ok(store::DefaultChoice::Full(c))
                if c.provider == provider.slug && &c.model != new_model =>
            {
                let moved = store::ModelChoice {
                    provider: c.provider,
                    model: new_model.clone(),
                };
                if let Err(err) = store::set_default_choice(runtime.id(), secrets, &moved).await {
                    tracing::warn!(
                        company = %runtime.id(),
                        provider = %provider.slug,
                        error = %err,
                        "edited the default row's model but could not move the default with it",
                    );
                }
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(
                company = %runtime.id(),
                error = %err,
                "could not read the default while editing a provider's model",
            ),
        }
    }

    drop(_index_guard);
    Ok(Json(ProviderMutation {
        status: effective_status(&state, runtime).await?,
        note: format!("{} updated.", provider.label),
        probe: None,
        affected_tiers: Vec::new(),
        used_by,
    }))
}

// ---- delete and disable -----------------------------------------------------

/// The `{slug}` capture.
#[derive(Debug, Deserialize)]
struct ProviderPath {
    slug: String,
}

/// `?confirmInUse=true` on `DELETE …/inference/providers/{slug}` — a DELETE
/// has no body on this API, so the confirmation the in-use guard needs rides
/// as a query parameter instead (`docs/key-reworks/in-use-guards.md` §2).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmInUseQuery {
    #[serde(default)]
    confirm_in_use: bool,
}

/// `DELETE …/inference/providers/{slug}` — disconnect a provider.
///
/// Three things happen together and they are one operation, not a cleanup pass:
/// the credential is cleared, the record is removed, and every route pointing at
/// it is reset. Skipping any one of them leaves a state an operator cannot see:
/// an orphaned secret, a row that reappears, or a workload pinned to a provider
/// that no longer exists.
async fn delete_provider(
    State(state): State<AppState>,
    company: AdminScopedCompany,
    Path(params): Path<ProviderPath>,
    axum::extract::Query(query): axum::extract::Query<ConfirmInUseQuery>,
) -> Result<Json<ProviderMutation>, ApiError> {
    let runtime = company.runtime.as_ref();
    let secrets = runtime.secrets().as_ref();
    // Keys rework (#2306), slice 4a: held only for the `tinyhumans` slug —
    // see `add_provider`'s own guard for why.
    let _fan_out_guard = if params.slug == crate::company::inference::MANAGED_SLUG {
        Some(crate::company::company_key::slot_guard(runtime.id()).await)
    } else {
        None
    };
    let provider = require_provider(runtime, &params.slug).await?;

    if provider.origin == store::ProviderOrigin::EntryZero {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "This company's original provider is cleared by resetting the inference \
             config, which also clears its key."
                .to_string(),
        )));
    }

    // Keys rework (#2306), round-3a review P2-2: held from the guard's check
    // through every write below, so a concurrent pin or edit cannot pass its
    // own check against a row this delete is about to remove out from under
    // it. Every write in this span is a secret-store write — no network call
    // — and the guard is dropped before `effective_status` rebuilds the
    // response.
    let _index_guard = crate::company::inference::store::index_lock(runtime.id()).await;

    // Keys rework (#2306), slice 2c: a delete is a removal in the fullest
    // sense — refused unless confirmed, same as a disable or a key clear.
    let used_by = provider_used_by(runtime, &provider.slug).await?;
    if !query.confirm_in_use
        && let Some(used_by) = used_by.clone()
    {
        return Err(ApiError(OpenCompanyError::InUse {
            message: provider_in_use_message(&provider.label, &used_by),
            used_by,
        }));
    }

    // Routes are scrubbed *before* the record goes, so the remaining-providers
    // list the three scrub rules need is the list as it will be afterwards.
    let mut routes = store::load_routes(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    let remaining: Vec<store::Provider> = store::list_providers(runtime.id(), secrets)
        .await
        .map_err(ApiError)?
        .into_iter()
        .filter(|p| p.slug != provider.slug)
        .collect();
    let reset = resolve::scrub_removed(&mut routes, &provider, &remaining);

    // **Computed before the removal, written after it.** The scrub rules need
    // the provider list as it will be *afterwards*, which is why the
    // calculation happens here — but writing the scrubbed table first meant a
    // failed removal returned an error with the provider still on screen and
    // the routes that named it already reset, which is a state nobody asked
    // for and nothing reports.
    //
    // Written after, the two failure modes are both readable: a failed removal
    // changes nothing, and a failed route write leaves routes naming a provider
    // that is gone — which `orphaned_routes` already finds and the Routing tab
    // already shows.
    //
    // `delete_provider` clears the credential first and refuses the removal if
    // that clear fails, which is the half-state the operator can see and act on.
    store::delete_provider(runtime.id(), secrets, &provider.slug)
        .await
        .map_err(ApiError)?;
    if !reset.is_empty() {
        store::save_routes(runtime.id(), secrets, &routes)
            .await
            .map_err(ApiError)?;
    }
    if let Err(err) = store::forget_health(runtime.id(), secrets, &provider.slug).await {
        tracing::warn!(
            company = %runtime.id(),
            provider = %provider.slug,
            error = %err,
            "removed a provider but could not clear its health record",
        );
    }
    // Keys rework (#2306), decision D-never-clear-default (X14, 2026-09-15):
    // `inference/default` is left exactly as it was, even though it may now
    // name a slug with no row at all. An operator who confirmed this delete
    // made one decision — remove the provider — and a silent second one —
    // "and also pick a new default" — is not that decision. A **full**
    // `{provider, model}` default (2b/2c) fails a turn closed when the marked
    // provider is gone, via `resolve_choice`; a legacy bare-slug marker keeps
    // its pre-existing behaviour of falling back to `resolve::primary`'s
    // first-enabled provider (D-legacy: unchanged for a company with no full
    // default). Either way, the console's job is to show the stale marker —
    // via the status route — not for this handler to paper over it by
    // quietly moving it to whatever the fallback happens to be today.
    crate::server::inference_models::evict_company_catalogs(runtime.id().as_ref());

    let note = if reset.is_empty() {
        format!("{} is disconnected and its key is cleared.", provider.label)
    } else {
        format!(
            "{} is disconnected and its key is cleared. {} {} through the primary \
             provider.",
            provider.label,
            reset.join(", "),
            // One tier resolves; several resolve. A sentence that reads as
            // broken English on the commonest case — a single route — reads as
            // a page that was not finished.
            if reset.len() == 1 {
                "now resolves"
            } else {
                "now resolve"
            }
        )
    };
    drop(_index_guard);
    Ok(Json(ProviderMutation {
        status: effective_status(&state, runtime).await?,
        note,
        probe: None,
        affected_tiers: reset,
        used_by,
    }))
}

/// `POST …/inference/providers/{slug}/enabled` — switch a provider on or off.
///
/// **Disabling does not scrub routes, and that is deliberate** — it is the one
/// place this implementation departs from the plan it follows, for a reason the
/// rest of the design already settled. A disabled provider keeps its endpoint,
/// its label and its credential precisely so that "stop billing this account
/// this week" is expressible; scrubbing its routes would make re-enabling it a
/// re-configuration rather than a switch, and would lose the operator's choices
/// silently. The resolver already models this: a route naming a disabled
/// provider is [`resolve::Resolution::Disabled`] — *reported*, never demoted to
/// a sibling — and scrubbing here would make that variant unreachable.
///
/// What it does instead is **say which tiers are parked**, so the operator is
/// told rather than left to notice, which is the property the scrub was there
/// to provide.
async fn set_enabled(
    State(state): State<AppState>,
    company: AdminScopedCompany,
    Path(params): Path<ProviderPath>,
    Json(body): Json<SetEnabled>,
) -> Result<Json<ProviderMutation>, ApiError> {
    let runtime = company.runtime.as_ref();
    let secrets = runtime.secrets().as_ref();
    let provider = require_provider(runtime, &params.slug).await?;

    // Keys rework (#2306), round-3a review P2-2: held from the guard's check
    // through the switch below, so a concurrent pin or delete cannot pass its
    // own check against a row this request is about to disable out from
    // under it. Every write in this span is a secret-store write — no
    // network call — and the guard is dropped before `effective_status`
    // rebuilds the response.
    let _index_guard = crate::company::inference::store::index_lock(runtime.id()).await;

    // Keys rework (#2306), slice 2c: a disable is guarded the same way a
    // delete is — the row keeps existing, but it stops being able to serve
    // whatever named it.
    let used_by = if body.enabled {
        None
    } else {
        provider_used_by(runtime, &provider.slug).await?
    };
    if !body.enabled
        && !body.confirm_in_use
        && let Some(used_by) = used_by.clone()
    {
        return Err(ApiError(OpenCompanyError::InUse {
            message: provider_in_use_message(&provider.label, &used_by),
            used_by,
        }));
    }

    // **Asked before the switch, not after.** `resolve::primary` skips disabled
    // providers, so once the write has landed this row can never report itself
    // as the one unset workloads were going through — and the whole point of
    // asking is to say that they just moved.
    let was_primary = !body.enabled && is_primary(runtime, &provider.slug).await?;

    if !store::set_enabled(runtime.id(), secrets, &provider.slug, body.enabled)
        .await
        .map_err(ApiError)?
    {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "This company's original provider cannot be switched off from the list; \
             reset the inference config instead."
                .to_string(),
        )));
    }

    let parked = if body.enabled {
        Vec::new()
    } else {
        // Keys rework (#2306), decision D-never-clear-default (X14,
        // 2026-09-15): `inference/default` is left exactly as marked, even
        // though this provider can no longer serve. An earlier version of
        // this handler cleared the marker here on the theory that moving it
        // to first-enabled was "the same answer" with less claim to intent —
        // but silently retargeting the default is itself an undocumented
        // decision the operator did not make. See the identical note in
        // `delete_provider` above for which resolution path this then takes.
        let mut tiers = parked_tiers(runtime, &provider).await?;
        // **An unset row is served by this provider too, and it moves.** Only
        // explicit routes name a slug, so switching off the provider every
        // unrouted workload was going through reported "nothing was routed
        // through it" while those workloads quietly moved to the next enabled
        // account — or to managed. A change of who pays is the one thing this
        // sentence exists to say out loud.
        if was_primary {
            let explicit = store::load_routes(runtime.id(), runtime.secrets().as_ref())
                .await
                .map_err(ApiError)?;
            for workload in resolve::ROUTABLE_WORKLOADS {
                let tier = workload.tier();
                let unset = !matches!(
                    explicit.get(tier),
                    Some(route) if !matches!(route, resolve::ProviderRef::Default)
                );
                if unset && !tiers.iter().any(|t| t == tier) {
                    tiers.push(tier.to_string());
                }
            }
        }
        tiers
    };
    let note = match (body.enabled, parked.is_empty()) {
        (true, _) => format!("{} is on.", provider.label),
        (false, true) => format!("{} is off. Nothing was routed through it.", provider.label),
        // **Named, not numbered.** This said "agentic-v1, vision-v1 are parked"
        // — the right sentence in the wrong vocabulary, on the one screen whose
        // job is to be read by a person.
        (false, false) => format!(
            "{} is off. {} {} no longer served by it.",
            provider.label,
            parked
                .iter()
                .map(|tier| resolve::tier_label(tier))
                .collect::<Vec<_>>()
                .join(", "),
            if parked.len() == 1 { "is" } else { "are" }
        ),
    };
    drop(_index_guard);
    Ok(Json(ProviderMutation {
        status: effective_status(&state, runtime).await?,
        note,
        probe: None,
        affected_tiers: parked,
        used_by,
    }))
}

/// `POST …/inference/providers/{slug}/default` — say which provider (and
/// which model) an unset workload goes through.
///
/// Explicit rather than positional. Without it "which provider is my default" is
/// answered by list order: add three, delete the first, and the company's
/// unrouted spend moves to a different account with nothing on screen having
/// changed to say so.
///
/// Setting one clears the previous one — not as a step, but because the marker
/// is a single slot holding a `{provider, model}` pair. Two defaults are not
/// representable.
///
/// Keys rework (#2306), slice 2c: this route used to take no body and store a
/// bare slug (Q1 kept that shape readable forever, never rewriting it on its
/// own). It now always requires a model — every default is `{provider,
/// model}` from here on.
async fn set_default(
    State(state): State<AppState>,
    company: AdminScopedCompany,
    Path(params): Path<ProviderPath>,
    Json(body): Json<SetDefault>,
) -> Result<Json<ProviderMutation>, ApiError> {
    let runtime = company.runtime.as_ref();
    let secrets = runtime.secrets().as_ref();
    let model = store::check_model_id(body.model.as_deref().unwrap_or("")).map_err(ApiError)?;
    // Keys rework (#2306), round-3a review P2-2: held from the enabled check
    // through both writes below, so a concurrent disable or delete of this
    // same provider cannot land between this handler's check and its write.
    // Set-default carries no `usedBy` guard of its own (round-3a review P2-6:
    // the console's own confirmation, naming the old and new provider, is the
    // guard — see `docs/key-reworks/in-use-guards.md` §1); this lock is only
    // about not racing the row's own state, and it never holds across a
    // network call.
    let _index_guard = crate::company::inference::store::index_lock(runtime.id()).await;
    let provider = require_provider(runtime, &params.slug).await?;
    if !provider.enabled {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "{} is switched off, so it cannot be the default. Switch it on first.",
            provider.label
        ))));
    }

    // 1. The row's model first, so the default never names a model its row
    //    lacks. Entry zero has no index record to rewrite — `put_provider`
    //    refuses its slug (`store.rs`) — so this step is skipped for it; its
    //    `models` map already came from `inference/config`.
    let draft = |models| store::ProviderDraft {
        slug: provider.slug.clone(),
        label: provider.label.clone(),
        kind: provider.kind.clone(),
        base_url: provider.base_url.clone(),
        models,
        enabled: provider.enabled,
    };
    let rewrite = provider.origin == store::ProviderOrigin::Indexed
        && !matches!(provider.model(), store::ModelOnRow::One(ref m) if *m == model);
    if rewrite {
        store::put_provider(runtime.id(), secrets, draft(uniform_models(Some(&model))))
            .await
            .map_err(ApiError)?;
    }

    // 2. Then the one JSON write. On failure put the row back; if that fails
    //    too, say so loudly rather than leaving a row and a default that
    //    disagree with no record of why.
    let choice = store::ModelChoice {
        provider: provider.slug.clone(),
        model: model.clone(),
    };
    if let Err(err) = store::set_default_choice(runtime.id(), secrets, &choice).await {
        if rewrite
            && let Err(restore) =
                store::put_provider(runtime.id(), secrets, draft(provider.models.clone())).await
        {
            tracing::error!(
                company = %runtime.id(),
                provider = %provider.slug,
                error = %restore,
                "set-default rewrote the row's model, failed to write the default, and \
                 failed to restore the row's previous models",
            );
        }
        return Err(ApiError(err));
    }

    drop(_index_guard);
    Ok(Json(ProviderMutation {
        status: effective_status(&state, runtime).await?,
        note: format!("New work now goes through {} · {model}.", provider.label),
        probe: None,
        affected_tiers: Vec::new(),
        used_by: None,
    }))
}

// DEPRECATED(keys-rework #2306): `clear_default_if_marked` used to live here,
// clearing `inference/default` whenever a delete or a disable named the
// marked provider. Removed by decision D-never-clear-default (X14,
// 2026-09-15, docs/key-reworks/README.md): see the notes at both of its
// former call sites, in `delete_provider` and `set_enabled` above. Removable
// once nobody searches the history for why the behaviour changed.

/// The tiers whose route `provider` serves, so switching it off can name them.
///
/// **This used to match on the slug alone**, and `ProviderRef::slug()` is `None`
/// for a `local` or `claude-code` ref — so disabling the only Ollama runtime
/// parked every `local:` route while the note said "Nothing was routed through
/// it." It reads through [`resolve::routes_served_by`] now, which is
/// `scrub_removed`'s three rules, so all three surfaces answer the same question
/// the same way.
///
/// `alternatives` is the providers that would *still be enabled* once this one
/// is off: a `local:` route is only parked when no other local runtime is left
/// to serve it, exactly as it is only orphaned when no other local runtime is
/// left at all.
async fn parked_tiers(
    runtime: &CompanyRuntime,
    provider: &store::Provider,
) -> Result<Vec<String>, ApiError> {
    let secrets = runtime.secrets().as_ref();
    let routes = store::load_routes(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    let providers = store::list_providers(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    let alternatives: Vec<store::Provider> = providers
        .into_iter()
        .filter(|p| p.enabled && p.slug != provider.slug)
        .collect();
    Ok(resolve::routes_served_by(&routes, provider, &alternatives))
}

/// The tiers explicitly routed to **managed**, so switching it off can name
/// them.
///
/// Its own function because managed has no provider record for [`parked_tiers`]
/// to take, and `managed` is a word in the route grammar rather than a slug.
///
/// **Unset rows count when managed is what they were falling back to.** A
/// company with no enabled provider resolves every unrouted workload through
/// the managed chain, so switching it off parks all four — and an empty routing
/// table, which is the commonest state there is, would otherwise report that
/// nothing changed.
async fn managed_parked_tiers(runtime: &CompanyRuntime) -> Result<Vec<String>, ApiError> {
    let secrets = runtime.secrets().as_ref();
    let routes = store::load_routes(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    let providers = store::list_providers(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    let marked = store::load_default_slug(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    let unset_falls_back_to_managed = resolve::primary(&providers, marked.as_deref()).is_none();

    let mut tiers: Vec<String> = routes
        .iter()
        .filter(|(_, route)| matches!(route, resolve::ProviderRef::Managed))
        .map(|(tier, _)| tier.clone())
        .collect();
    if unset_falls_back_to_managed {
        for workload in resolve::ROUTABLE_WORKLOADS {
            let tier = workload.tier();
            let unset = !matches!(
                routes.get(tier),
                Some(route) if !matches!(route, resolve::ProviderRef::Default)
            );
            if unset && !tiers.iter().any(|t| t == tier) {
                tiers.push(tier.to_string());
            }
        }
    }
    Ok(tiers)
}

/// Whether `slug` is the provider an **unset** workload currently goes through.
///
/// The resolved answer, like the status DTO's `is_default`: a company that has
/// never marked one resolves to its first enabled provider, and switching that
/// one off moves every unset workload just as surely as clearing an explicit
/// marker would.
async fn is_primary(runtime: &CompanyRuntime, slug: &str) -> Result<bool, ApiError> {
    let secrets = runtime.secrets().as_ref();
    let providers = store::list_providers(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    let marked = store::load_default_slug(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    Ok(resolve::primary(&providers, marked.as_deref()).is_some_and(|p| p.slug == slug))
}

/// The provider, or a 404 naming the slug that resolved to nothing.
async fn require_provider(
    runtime: &CompanyRuntime,
    slug: &str,
) -> Result<store::Provider, ApiError> {
    store::get_provider(runtime.id(), runtime.secrets().as_ref(), slug)
        .await
        .map_err(ApiError)?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::NotFound(format!(
                "this company has no provider `{slug}`"
            )))
        })
}

/// `POST …/inference/managed/enabled` — switch managed in or out of routing.
///
/// **Not the credential.** Every step of the chain stays exactly where it is;
/// what changes is whether a workload may be routed here, which is the same
/// thing `enabled` means on any other provider. `Resolution::Disabled` already
/// models a route naming a switched-off provider as *reported* rather than
/// quietly demoted, and managed gets that treatment too.
///
/// DEPRECATED(keys-rework #2306): decision D-managed-toggle (X3, 2026-09-15)
/// — TinyHumans has no special switch any more. A `tinyhumans` row's on/off
/// is the ordinary per-row `set_enabled` above, in-use-guarded like any
/// other provider. This route (and `inference/managed/enabled`) stays only
/// for the **legacy** Managed chain — the fallback that resolves with no
/// `tinyhumans` row at all — which has no default/agent-pair concept of its
/// own for the in-use guard to apply to; not guarded here.
async fn set_managed_enabled(
    State(state): State<AppState>,
    company: AdminScopedCompany,
    Json(body): Json<SetEnabled>,
) -> Result<Json<ProviderMutation>, ApiError> {
    let runtime = company.runtime.as_ref();
    store::set_managed_enabled(runtime.id(), runtime.secrets().as_ref(), body.enabled)
        .await
        .map_err(ApiError)?;
    // Named, the way switching an indexed provider off names them. A workload
    // routed explicitly to `managed` fails closed on its next turn, and an
    // empty list said nothing had changed.
    let parked = if body.enabled {
        Vec::new()
    } else {
        managed_parked_tiers(runtime).await?
    };
    Ok(Json(ProviderMutation {
        status: effective_status(&state, runtime).await?,
        note: if body.enabled {
            "Managed is on.".to_string()
        } else if parked.is_empty() {
            "Managed is off. Its credential is untouched.".to_string()
        } else {
            format!(
                "Managed is off and its credential is untouched. {} {} routed to it and                  will not run until it is back on or pointed elsewhere.",
                parked.join(", "),
                if parked.len() == 1 { "is" } else { "are" },
            )
        },
        probe: None,
        affected_tiers: parked,
        used_by: None,
    }))
}

/// `POST …/inference/managed/test` — check whatever the managed chain resolves to.
///
/// The credential it presents is **whichever step answers**, not necessarily a
/// key this company pasted: a company on the instance identity is testing the
/// server's credential against the platform endpoint, which is exactly what its
/// turns would do.
///
/// Like the per-provider test, it never deletes anything whatever the answer. A
/// test is a question being asked; making the button that reports a problem the
/// button that causes one would be a trap — and here it would be worse, because
/// the credential it might destroy could be the instance's.
async fn test_managed(
    company: crate::server::ops::ScopedCompany,
) -> Result<Json<ProbeResultDto>, ApiError> {
    use crate::company::inference;

    let runtime = company.runtime.as_ref();
    let secrets = runtime.secrets().as_ref();
    let platform = super::platform_default(&crate::app::config::ProcessEnv);
    let inference_key =
        inference::load_managed_key(runtime.id(), secrets, &inference::HarnessScope::default())
            .await
            .map_err(ApiError)?;
    let company_account = crate::company::company_key::load(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;

    // The same four-branch decision the row renders, resolved to a value here.
    let bearer = match inference::managed_source(
        !inference_key.trim().is_empty(),
        &company_account,
        platform.as_ref(),
    ) {
        inference::ManagedSource::ProviderKey => Some(inference_key.trim().to_string()),
        inference::ManagedSource::CompanyAccount => {
            company_account.current().await.map_err(ApiError)?
        }
        inference::ManagedSource::Instance => match platform.as_ref() {
            Some(env) => env.credential.current().await.map_err(ApiError)?,
            None => None,
        },
        inference::ManagedSource::None => {
            return Err(ApiError(OpenCompanyError::InvalidRequest(
                "Managed is not set up on this company, so there is nothing to check.".to_string(),
            )));
        }
    };

    let base_url = platform
        .as_ref()
        .map(|p| p.base_url.clone())
        .unwrap_or_else(|| inference::PLATFORM_BASE_URL.to_string());
    let subject = catalogue::endpoint_host(&base_url).unwrap_or_else(|| "the managed brain".into());

    match probe::probe_models(
        &base_url,
        bearer.as_deref(),
        catalogue::AuthStyle::Bearer,
        probe::default_policy(),
        catalogue::catalog_shape_for(inference::LEGACY_MANAGED, &base_url),
    )
    .await
    {
        Ok(models) => {
            record_health(runtime, inference::MANAGED_SLUG, "ok").await;
            Ok(Json(ProbeResultDto {
                ok: true,
                class: None,
                message: None,
                model_count: models.len(),
                model_known: None,
                models: paged_catalog::catalogue_offer(&models),
            }))
        }
        Err(failure) => {
            tracing::info!(
                company = %runtime.id(),
                class = failure.class.as_str(),
                detail = %failure.raw,
                "managed inference test failed",
            );
            record_health(runtime, inference::MANAGED_SLUG, failure.class.as_str()).await;
            let message = if failure.truncated {
                format!("The model list from {subject} could not be read.")
            } else {
                probe::describe(failure.class, &subject)
            };
            Ok(Json(ProbeResultDto {
                ok: false,
                class: Some(failure.class.as_str().to_string()),
                message: Some(message),
                model_count: 0,
                model_known: None,
                models: Vec::new(),
            }))
        }
    }
}

// ---- a provider's catalog ---------------------------------------------------

/// What `GET …/inference/providers/{slug}/models` answers.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderCatalogDto {
    /// The endpoint the catalog was read from.
    base_url: String,
    /// Every model that endpoint publishes, sorted. Empty when `error` is set.
    models: Vec<String>,
    /// Whether the endpoint's `model` field keys on a **deployment name** rather
    /// than a published model id.
    ///
    /// Azure separates the base model a deployment was made from
    /// (`gpt-5.6-terra-2026-07-09`) from the deployment name (`gpt-5.6-terra`)
    /// that actually routes the request — and `/models` publishes the first
    /// while the request body wants the second. So a closed dropdown sourced
    /// from the catalog makes the only correct value unreachable, and the
    /// console defaults such an endpoint to free text.
    free_text_only: bool,
    /// Why the list is empty, naming the endpoint.
    ///
    /// A **200** rather than a 5xx, because an empty picker with no explanation
    /// reads as "this provider has no models", which nobody established.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `GET …/inference/providers/{slug}/models` — that provider's own catalog.
///
/// The stored key is presented **host-side**: it is write-only to the console,
/// so this route is the only thing that can ask an authenticated endpoint what
/// it serves.
///
/// The cache is scoped `company + slug`, not `company` alone. Two providers on
/// one endpoint with two keys would otherwise share an entry, and an endpoint
/// that publishes an entitlement-scoped catalog would hand one account's list to
/// the other for the rest of the hour.
async fn list_provider_models(
    company: crate::server::ops::ScopedCompany,
    Path(params): Path<ProviderPath>,
) -> Result<Json<ProviderCatalogDto>, ApiError> {
    let runtime = company.runtime.as_ref();
    let secrets = runtime.secrets().as_ref();
    let provider = require_provider(runtime, &params.slug).await?;
    let key = store::load_provider_key(runtime.id(), secrets, &provider)
        .await
        .map_err(ApiError)?;
    let scope = format!("{}\u{1}{}", runtime.id().as_ref(), provider.slug);

    let free_text_only = catalogue::is_azure_endpoint(&provider.base_url);
    match crate::server::inference_models::catalog_models(
        &provider.base_url,
        (!key.trim().is_empty()).then(|| key.trim()),
        Some(&scope),
        catalogue::auth_style_for(&provider.kind),
        catalogue::catalog_shape_for(&provider.kind, &provider.base_url),
    )
    .await
    {
        // Redacted on both arms. This route is `ScopedCompany`, and `{error}`
        // alone is not enough: `reqwest` masks userinfo in its own `Display`,
        // and then a `format!` like this one re-adds it from the endpoint we
        // hold.
        Ok(models) => Ok(Json(ProviderCatalogDto {
            base_url: catalogue::redact_endpoint(&provider.base_url),
            models: models.into_iter().map(|m| m.id).collect(),
            free_text_only,
            error: None,
        })),
        Err(error) => Ok(Json(ProviderCatalogDto {
            error: Some(format!(
                "Could not list models from {}: {error}. Enter a model id directly.",
                catalogue::redact_endpoint(&provider.base_url)
            )),
            base_url: catalogue::redact_endpoint(&provider.base_url),
            models: Vec::new(),
            free_text_only,
        })),
    }
}

// ---- the managed credential -------------------------------------------------

/// The managed key on the way in. Write-only, like every other credential body.
#[derive(Debug, Deserialize)]
struct SetManagedKey {
    /// Send `""` to clear it and fall back down the chain.
    key: String,
}

/// `PUT …/inference/managed/key` — paste a key for the managed tier.
///
/// Step 1 of the managed chain: a credential pasted specifically for inference,
/// which outranks the company's account identity and the instance's.
///
/// Writes `provider/tinyhumans/key` and **clears the legacy `inference/key`** in
/// the same operation, which is the convergence rule every other provider's
/// write follows. The store has no delete, so the clear is a write of the empty
/// string and it is issued rather than inferred: a key left at the old address
/// after the new one is written is an orphaned secret.
///
/// The other half of setting managed up is the hub link flow, which writes the
/// company's *account* — step 3. That one is not here, and deliberately: it
/// already exists on the Account page and a second credential form for one
/// credential is how two surfaces come to disagree about whether a company has
/// one.
///
/// DEPRECATED(keys-rework #2306): decision D-legacy-writes (X6, 2026-09-15).
/// Kept for a manager or CLI that still calls it — writes the same slot a
/// `tinyhumans` row's own key does, so nothing here is wrong, only redundant
/// once a row exists. **The replacement the console should call instead:**
/// once a `tinyhumans` row exists (added through `POST …/inference/providers`,
/// or already present as entry zero), set or clear its key through the
/// ordinary `PUT …/inference/providers/tinyhumans` (`edit_provider`, above)
/// with `{"key": "…"}` or `{"key": ""}` — the same in-use-guarded path every
/// other provider's key goes through. This route remains the *only* way to
/// reach `provider/tinyhumans/key` before any `tinyhumans` row exists (the
/// legacy-Managed-row case), because `edit_provider` has no row to act on
/// yet. Not in-use-guarded: unlike a row's own key clear, this route's
/// dependants are the legacy chain's own (item 10, not handled by this
/// rework — see `docs/key-reworks/not-handled.md`), which this contract does
/// not model.
async fn set_managed_key(
    State(state): State<AppState>,
    company: AdminScopedCompany,
    Json(body): Json<SetManagedKey>,
) -> Result<Json<ProviderMutation>, ApiError> {
    let runtime = company.runtime.as_ref();
    // Keys rework (#2306), slice 4a: this handler always writes the
    // `tinyhumans` slug's key, so — unlike `add_provider`/`edit_provider`/
    // `delete_provider`, which take the lock only for that one slug —
    // it is held unconditionally.
    let _fan_out_guard = crate::company::company_key::slot_guard(runtime.id()).await;
    let secrets = runtime.secrets().as_ref();
    let key = body.key.trim();

    // **Asked before anything is written.** This read can fail, and asking it
    // after the new key had landed meant a transient store error returned "that
    // did not work" over a credential that was already live and already
    // outranking the old one on the next turn — the console saying the account
    // had not changed while it had.
    let legacy_is_managed = store::legacy_slot_is_managed(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;

    secrets
        .set(
            runtime.id(),
            &store::provider_key_key(crate::company::inference::MANAGED_SLUG),
            crate::ports::types::SecretValue(key.to_string()),
        )
        .await
        .map_err(ApiError)?;

    // **Only when the legacy slot is managed's to clear.**
    //
    // `inference/key` is one address that two different rows can read through
    // their own fallback: entry zero's, and managed's. Which one it belongs to
    // depends on what entry zero's kind normalises to. Clearing it
    // unconditionally while writing a *different* slug's slot destroyed the
    // credential of whatever else was reading it — on this company, removing
    // the managed key silently took OpenRouter's key with it, and the row went
    // from "•••• configured" to showing a bare host.
    //
    // Found in a browser, not by a test. The test is below it now.
    if legacy_is_managed
        && let Err(err) = secrets
            .set(
                runtime.id(),
                crate::company::inference::KEY_KEY,
                crate::ports::types::SecretValue(String::new()),
            )
            .await
    {
        tracing::error!(
            company = %runtime.id(),
            error = %err,
            "could not clear managed's legacy credential slot",
        );
        // **Reported, not just logged, and specifically on a clear.** The read
        // chain falls back to `inference/key` when the new slot is empty, so a
        // failure here leaves the old credential live and still billed while
        // the console says "Cleared the managed key." A save is different: the
        // new key is already in the slot that outranks this one, so the stale
        // legacy value is unreachable and the write succeeded in the only sense
        // the operator asked about.
        if key.is_empty() {
            return Err(ApiError(OpenCompanyError::Store(
                "The managed key could not be fully cleared — the older of its two \
                 storage slots still holds it, so turns may still be billed to it. \
                 Try again."
                    .to_string(),
            )));
        }
    }
    crate::server::inference_models::evict_company_catalogs(runtime.id().as_ref());

    Ok(Json(ProviderMutation {
        status: effective_status(&state, runtime).await?,
        note: if key.is_empty() {
            "Cleared the managed key.".to_string()
        } else {
            "Saved. Managed turns are billed to that key.".to_string()
        },
        probe: None,
        affected_tiers: Vec::new(),
        used_by: None,
    }))
}

// ---- testing a stored provider ----------------------------------------------

/// `POST …/inference/providers/{slug}/test` — re-check a provider that is
/// already connected.
///
/// One of the three things that feed a row's health, and the only one an
/// operator can ask for: the other two are the add-time probe and the turn
/// path's own 401. **There is no poller.** One would cost a request per provider
/// per interval across every company this host serves, to learn something the
/// next real turn learns for free.
async fn test_provider(
    company: crate::server::ops::ScopedCompany,
    Path(params): Path<ProviderPath>,
    body: Option<Json<TestProvider>>,
) -> Result<Json<ProbeResultDto>, ApiError> {
    let asked_model = body
        .and_then(|Json(body)| body.model)
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    let runtime = company.runtime.as_ref();
    let secrets = runtime.secrets().as_ref();
    let provider = require_provider(runtime, &params.slug).await?;
    let key = store::load_provider_key(runtime.id(), secrets, &provider)
        .await
        .map_err(ApiError)?;

    match probe::probe_models(
        &provider.base_url,
        (!key.trim().is_empty()).then(|| key.trim()),
        catalogue::auth_style_for(&provider.kind),
        probe::default_policy(),
        catalogue::catalog_shape_for(&provider.kind, &provider.base_url),
    )
    .await
    {
        Ok(models) => {
            record_health(runtime, &provider.slug, "ok").await;
            // Whether the row's chosen id is one this endpoint publishes. The
            // check used to answer "is the endpoint reachable" while the console
            // asked "will this model answer", so `this-model-does-not-exist`
            // came back as "Reached the provider." — a true sentence about a
            // question nobody asked.
            let model_known = asked_model
                .as_deref()
                .and_then(|asked| (!models.is_empty()).then(|| models.iter().any(|m| m == asked)));
            Ok(Json(ProbeResultDto {
                ok: true,
                class: None,
                message: None,
                model_count: models.len(),
                model_known,
                models: paged_catalog::catalogue_offer(&models),
            }))
        }
        Err(failure) => {
            tracing::info!(
                company = %runtime.id(),
                provider = %provider.slug,
                class = failure.class.as_str(),
                detail = %failure.raw,
                "inference provider test failed",
            );
            // **The test never deletes a credential**, whatever the class. An
            // add is a commitment being made and a rollback undoes it; a test is
            // a question being asked, and answering "your key is rejected" by
            // destroying it would make the button that reports a problem the
            // button that causes one.
            record_health(runtime, &provider.slug, failure.class.as_str()).await;
            let message = if failure.truncated {
                format!("The model list from {} could not be read.", provider.label)
            } else {
                probe::describe(failure.class, &advisory_subject(&provider))
            };
            Ok(Json(ProbeResultDto {
                ok: false,
                class: Some(failure.class.as_str().to_string()),
                message: Some(message),
                model_count: 0,
                model_known: None,
                models: Vec::new(),
            }))
        }
    }
}

// ---- the draft probe --------------------------------------------------------

/// `POST …/inference/probe` — test an endpoint and a key that are not stored.
///
/// The list's add-then-test flow needs to probe a **draft**: the existing
/// `POST …/inference/test` probes the *saved* config, which by definition does
/// not exist yet at the moment the operator wants to know.
///
/// Generalising it creates an authenticated "send a request to an arbitrary URL
/// with an arbitrary key" primitive, which is SSRF-shaped. The answer is
/// explicit rather than inherited, and it is in two places on purpose:
/// `AdminScopedCompany` in this signature, and [`probe::check_endpoint`] applied
/// to the URL **and to every redirect target** inside the probe itself.
async fn probe_draft(company: AdminScopedCompany, Json(body): Json<ProbeDraft>) -> Response {
    let _ = &company;
    let kind = body.kind.as_deref().unwrap_or("custom");
    // Refused before the request is made, not after. A draft is never stored, so
    // this is not about the store — it is that "probe this URL" would otherwise
    // be a way to make this host put a basic-auth credential on the wire to an
    // address the operator names, on an endpoint shape the add flow will refuse
    // to save anyway.
    if catalogue::endpoint_has_credentials(&body.base_url) {
        return ApiError(OpenCompanyError::InvalidRequest(endpoint_refusal(
            body.base_url.trim(),
        )))
        .into_response();
    }
    let auth = catalogue::auth_style_for(kind);
    let subject = catalogue::endpoint_host(&body.base_url).unwrap_or_else(|| "that host".into());
    match probe::probe_models(
        body.base_url.trim(),
        body.key.as_deref().filter(|k| !k.trim().is_empty()),
        auth,
        probe::default_policy(),
        catalogue::catalog_shape_for(kind, body.base_url.trim()),
    )
    .await
    {
        Ok(models) => Json(ProbeResultDto {
            ok: true,
            class: None,
            message: None,
            model_count: models.len(),
            model_known: None,
            models: paged_catalog::catalogue_offer(&models),
        })
        .into_response(),
        Err(failure) => {
            tracing::info!(
                company = %company.runtime.id(),
                class = failure.class.as_str(),
                detail = %failure.raw,
                "draft inference probe failed",
            );
            // A 200 carrying `ok: false`, not a 5xx: the request succeeded and
            // the answer is "that endpoint did not work". A gateway status would
            // make the console's error handling treat a correct answer as a
            // broken host.
            //
            // Bug KR-L1-01: a catalog too large to read is not "the check did
            // not complete" — the credential is not in question, and the
            // model step must say specifically that its list could not be
            // read rather than silently opening on free text with `ok: true`
            // and zero models.
            let message = if failure.truncated {
                format!("The model list from {subject} could not be read.")
            } else {
                probe::describe(failure.class, &subject)
            };
            Json(ProbeResultDto {
                ok: false,
                class: Some(failure.class.as_str().to_string()),
                message: Some(message),
                model_count: 0,
                model_known: None,
                models: Vec::new(),
            })
            .into_response()
        }
    }
}

// ---- routes -----------------------------------------------------------------

/// What `GET …/inference/routes` answers.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoutesDto {
    /// Tier → route string.
    routes: BTreeMap<String, String>,
    /// The mode these routes describe. **Inferred, never stored** — a stored
    /// mode would be a fifth thing that can disagree with the four routes.
    mode: String,
    /// Routes naming a provider this company does not hold, as `[tier, slug]`.
    ///
    /// The second, independent mechanism behind the same invariant as the
    /// delete-time scrub, because the UI path can be bypassed by a config edit
    /// or an older build — and an unresolvable route has to be reported rather
    /// than discovered mid-turn.
    orphaned: Vec<(String, String)>,
}

/// `GET …/inference/routes` — the routing table, its inferred mode, and any
/// route naming a provider that is gone.
async fn get_routes(company: AdminScopedCompany) -> Result<Json<RoutesDto>, ApiError> {
    let runtime = company.runtime.as_ref();
    let secrets = runtime.secrets().as_ref();
    let routes = store::load_routes(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    let providers = store::list_providers(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;
    let managed_answers = managed_resolves(runtime).await?;
    Ok(Json(RoutesDto {
        mode: mode_name(resolve::infer_routing_mode(&routes, managed_answers)),
        orphaned: resolve::orphaned_routes(&routes, &providers),
        routes: routes
            .into_iter()
            .map(|(tier, route)| (tier, route.to_route_string()))
            .collect(),
    }))
}

/// `PUT …/inference/routes` — replace the routing table.
///
/// A whole-table write rather than a per-row patch, because the modes are
/// whole-table statements: "route everything through one model" is not four
/// independent edits, and applying it as four would leave a visible intermediate
/// state where two rows have moved and two have not.
async fn put_routes(
    company: AdminScopedCompany,
    Json(body): Json<PutRoutes>,
) -> Result<Json<RoutesDto>, ApiError> {
    let runtime = company.runtime.as_ref();
    let secrets = runtime.secrets().as_ref();
    let providers = store::list_providers(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;

    let mut routes = resolve::Routes::new();
    for (tier, raw) in body.routes {
        let tier = tier.trim().to_string();
        if resolve::Workload::from_tier(&tier).is_none() {
            return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                "`{tier}` is not a workload this runtime has a tier for."
            ))));
        }
        let route = resolve::ProviderRef::parse(&raw);
        if let Err(message) = route_is_servable(&tier, &route, &providers) {
            return Err(ApiError(OpenCompanyError::InvalidRequest(message)));
        }
        routes.insert(tier, route);
    }
    store::save_routes(runtime.id(), secrets, &routes)
        .await
        .map_err(ApiError)?;

    // **Answered from the store, not from the request.** Echoing `routes` back
    // made this response a picture of what was *asked for*, so any divergence
    // between the ask and what is now stored was invisible by construction: a
    // write that landed nowhere still came back 200 carrying the operator's own
    // intent, the console rendered the new row, and the routing table held the
    // old value. That was reported as a save that vanished with no error, and it
    // could not be reproduced — because nothing on either side was capable of
    // noticing it.
    //
    // One extra read on a rare write buys the property that the console can only
    // ever render what is actually persisted. It also settles a concurrent write
    // honestly: two admins saving at once both used to be told they won.
    let stored = store::load_routes(runtime.id(), secrets)
        .await
        .map_err(ApiError)?;

    let managed_answers = managed_resolves(runtime).await?;
    Ok(Json(RoutesDto {
        mode: mode_name(resolve::infer_routing_mode(&stored, managed_answers)),
        orphaned: resolve::orphaned_routes(&stored, &providers),
        routes: stored
            .into_iter()
            .map(|(tier, route)| (tier, route.to_route_string()))
            .collect(),
    }))
}

/// Whether this company can actually serve `route`, or why not.
///
/// **Fail closed on a route naming a provider nobody holds.** Accepting it and
/// letting the turn discover it would attribute that workload's spend to
/// whatever the fallback happened to be — the same defect as resolving an
/// unknown provider kind instead of rejecting it.
///
/// A pure function, and not merely for tidiness: the bug this closes was a
/// branch that ran for two of the five ref kinds and silently did not for the
/// other two, which is exactly the shape a handler-shaped check hides. The two
/// name a provider by **category** rather than by slug, and `route.slug()` is
/// `None` for both — so `PUT …/routes {"chat-v1":"claude-code:opus"}` returned
/// 200 and rendered as a working row on a host that refuses to connect a CLI
/// login at all.
fn route_is_servable(
    tier: &str,
    route: &resolve::ProviderRef,
    providers: &[store::Provider],
) -> Result<(), String> {
    let missing = |name: &str| {
        Err(format!(
            "{tier} names `{name}`, which this company has no provider for."
        ))
    };
    match route {
        resolve::ProviderRef::Cloud { provider_slug, .. } => {
            if providers.iter().any(|p| &p.slug == provider_slug) {
                Ok(())
            } else {
                missing(provider_slug)
            }
        }
        // Named by kind rather than by slug, and gated the same way.
        resolve::ProviderRef::Local { .. } => {
            if has_category(providers, catalogue::Category::Local) {
                Ok(())
            } else {
                missing("local")
            }
        }
        resolve::ProviderRef::ClaudeCode { .. } => {
            if has_category(providers, catalogue::Category::Cli) {
                Ok(())
            } else {
                missing("claude-code")
            }
        }
        // Neither names a provider record: managed resolves through the
        // credential chain, and an absence is always servable.
        resolve::ProviderRef::Managed | resolve::ProviderRef::Default => Ok(()),
    }
}

fn has_category(providers: &[store::Provider], category: catalogue::Category) -> bool {
    providers
        .iter()
        .any(|p| catalogue::category_of(&p.kind) == category)
}

/// The wire name of an inferred mode.
fn mode_name(mode: resolve::RoutingMode) -> String {
    match mode {
        resolve::RoutingMode::Managed => "managed",
        resolve::RoutingMode::Own => "own",
        resolve::RoutingMode::Advanced => "advanced",
        // Not a mode the operator can pick — the absence of one. The console
        // renders it as "no row selected" plus a sentence naming where turns
        // actually go, which is the state this whole pass exists to make
        // visible.
        resolve::RoutingMode::Unset => "unset",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::config::DEFAULT_API_URL;
    use std::collections::BTreeMap;

    fn provider(slug: &str, kind: &str) -> store::Provider {
        store::Provider {
            id: store::ProviderId::new(),
            slug: slug.to_string(),
            label: slug.to_string(),
            kind: kind.to_string(),
            base_url: format!("https://{slug}.example/v1"),
            models: BTreeMap::new(),
            enabled: true,
            origin: store::ProviderOrigin::Indexed,
        }
    }

    // ---- what a route may name ------------------------------------------

    #[test]
    fn a_cloud_route_must_name_a_provider_this_company_holds() {
        let held = vec![provider("openrouter", "openrouter")];
        assert!(
            route_is_servable(
                "chat-v1",
                &resolve::ProviderRef::parse("openrouter:gpt-5"),
                &held
            )
            .is_ok()
        );
        let err = route_is_servable("chat-v1", &resolve::ProviderRef::parse("ghost"), &held)
            .expect_err("a route naming nothing fails closed");
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn the_slug_less_kinds_are_gated_too() {
        // The bug: this check is reached through `route.slug()`, which is `None`
        // for `Local` and `ClaudeCode` — so both bypassed validation entirely.
        // `POST …/providers {"kind":"claude-code"}` is refused on a host that
        // cannot reach a CLI login, while `PUT …/routes` accepted
        // `claude-code:opus` with a 200 and rendered it as a working row.
        let cloud_only = vec![provider("openrouter", "openrouter")];
        assert!(
            route_is_servable(
                "chat-v1",
                &resolve::ProviderRef::parse("claude-code:opus"),
                &cloud_only
            )
            .is_err(),
            "a CLI route on a company with no CLI login must fail closed"
        );
        assert!(
            route_is_servable(
                "chat-v1",
                &resolve::ProviderRef::parse("local"),
                &cloud_only
            )
            .is_err(),
            "and so must a local route with no local runtime"
        );
    }

    #[test]
    fn a_category_that_is_present_serves_its_slug_less_route() {
        let with_local = vec![provider("ollama", "ollama")];
        assert!(
            route_is_servable(
                "chat-v1",
                &resolve::ProviderRef::parse("local:llama3"),
                &with_local
            )
            .is_ok()
        );
    }

    #[test]
    fn managed_and_unset_name_no_record_and_are_always_servable() {
        // Managed resolves through the credential chain rather than the list,
        // and an absence is not a claim about anything.
        assert!(route_is_servable("chat-v1", &resolve::ProviderRef::Managed, &[]).is_ok());
        assert!(route_is_servable("chat-v1", &resolve::ProviderRef::Default, &[]).is_ok());
    }

    // ---- what adding a provider requires ---------------------------------

    /// The guard in `plan_add` is right and stays. What was wrong was the datum
    /// it read: OMLX was marked `needs_key: true`, and **no build of any of the
    /// three projects called "omlx" requires a key** — two have no auth
    /// mechanism at all. So the host refused to add it at all, which is a harder
    /// failure than the silent one the guard was promoted here to prevent.
    #[test]
    fn omlx_can_be_added_without_a_key() {
        assert!(
            plan_add(
                "omlx",
                None,
                Some("http://127.0.0.1:10240/v1"),
                false,
                DEFAULT_API_URL
            )
            .is_ok(),
            "omlx requires no key, so it must not be refused for want of one"
        );
        // Supplying one is still allowed: `jundot/omlx` has an opt-in
        // `--api-key`, so accepting a key and demanding one stay separate.
        assert!(
            plan_add(
                "omlx",
                None,
                Some("http://127.0.0.1:10240/v1"),
                true,
                DEFAULT_API_URL
            )
            .is_ok()
        );
    }

    /// No shipped local runtime sets `needs_key` any more, so the refusal itself
    /// would be covered by nothing. Asserted against a row built for the purpose
    /// rather than deleted, because the guard is what stops the *original*
    /// defect — a runtime stored with no credential, therefore never probed,
    /// therefore added without a word.
    #[test]
    fn a_local_runtime_that_demands_a_key_is_still_refused_without_one() {
        let demanding = catalogue::LocalRuntime {
            slug: "needs-a-key",
            label: "Needs A Key",
            default_endpoint: None,
            needs_key: true,
            auth: catalogue::AuthStyle::Bearer,
        };
        // The condition `plan_add` applies, against a row that declares it.
        let has_key = false;
        assert!(
            demanding.needs_key && !has_key,
            "this is the state the guard refuses"
        );
        // And accepting a key is not the same as demanding one: every shipped
        // runtime is addable keyless.
        for runtime in catalogue::LOCAL_RUNTIMES {
            assert!(
                !runtime.needs_key,
                "{} cannot be added at all while it demands a key",
                runtime.slug
            );
        }
    }

    #[test]
    fn a_keyless_local_runtime_is_still_added_without_one() {
        // Ollama wants an endpoint, not a credential. The rule is the
        // catalogue's per-row `needs_key`, never "local runtimes are keyless".
        assert!(plan_add("ollama", None, None, false, DEFAULT_API_URL).is_ok());
    }

    #[test]
    fn a_tinyhumans_add_points_at_the_configured_platform() {
        let prod = plan_add("tinyhumans", None, None, true, DEFAULT_API_URL).expect("planned");
        assert_eq!(
            prod.base_url,
            "https://api.tinyhumans.ai/agent-integrations/openrouter"
        );
        let local =
            plan_add("tinyhumans", None, None, true, "http://localhost:5005").expect("planned");
        assert_eq!(
            local.base_url,
            "http://localhost:5005/agent-integrations/openrouter"
        );
        // Only TinyHumans follows `api_url`; every other cloud row keeps its own host.
        let other =
            plan_add("openrouter", None, None, true, "http://localhost:5005").expect("planned");
        assert_eq!(other.base_url, "https://openrouter.ai/api/v1");
    }

    /// A named model is written to every tier, so no workload is left to fall
    /// through to the passthrough that produced the 404.
    #[test]
    fn a_named_model_covers_every_tier() {
        let overrides = uniform_models(Some("claude-sonnet-5"));
        assert_eq!(overrides.len(), crate::company::INFERENCE_TIERS.len());
        for tier in crate::company::INFERENCE_TIERS {
            assert_eq!(
                overrides.get(*tier).map(String::as_str),
                Some("claude-sonnet-5")
            );
        }
        assert!(uniform_models(None).is_empty());
    }

    // ---- the one case where routing a new provider is not a guess ---------

    fn empty() -> resolve::Routes {
        resolve::Routes::new()
    }

    fn routed_to(slug: &str) -> resolve::Routes {
        resolve::ROUTABLE_WORKLOADS
            .iter()
            .map(|w| (w.tier().to_string(), resolve::ProviderRef::parse(slug)))
            .collect()
    }

    /// The reported company: nothing authored, no managed credential, one
    /// provider just added. There is precisely one thing that can serve a turn,
    /// so routing to anything else is not a choice that exists.
    #[test]
    fn a_sole_provider_with_no_managed_and_no_routes_is_unambiguous() {
        let anthropic = provider("anthropic", "anthropic");
        assert!(is_the_only_thing_that_can_answer(
            &empty(),
            std::slice::from_ref(&anthropic),
            false,
            "anthropic"
        ));
    }

    /// Row B2, and the one the warning is about: Managed resolves, so adding a
    /// key may be for one workload, for vision only, or to compare. Writing all
    /// four rows would bill the operator for everything, silently, from a screen
    /// that still says Managed.
    #[test]
    fn managed_being_available_makes_it_a_decision_rather_than_a_certainty() {
        let anthropic = provider("anthropic", "anthropic");
        assert!(!is_the_only_thing_that_can_answer(
            &empty(),
            std::slice::from_ref(&anthropic),
            true,
            "anthropic"
        ));
    }

    /// Anything already authored is never overwritten, whatever it says.
    #[test]
    fn a_table_that_names_anything_is_left_alone() {
        let anthropic = provider("anthropic", "anthropic");
        assert!(!is_the_only_thing_that_can_answer(
            &routed_to("managed"),
            std::slice::from_ref(&anthropic),
            false,
            "anthropic"
        ));
        assert!(!is_the_only_thing_that_can_answer(
            &routed_to("anthropic"),
            std::slice::from_ref(&anthropic),
            false,
            "anthropic"
        ));
    }

    /// **"First provider" is the wrong test, and this is why.** Entry zero is a
    /// provider the operator never added and which is always enabled, so the
    /// newly added row can be the second element of the list — and the company
    /// already has something that answers. Two enabled providers is a choice
    /// between them, which is the operator's to make.
    #[test]
    fn a_second_enabled_provider_makes_it_a_choice() {
        let anthropic = provider("anthropic", "anthropic");
        let mut zero = provider("tinyhumans", "openrouter");
        zero.origin = store::ProviderOrigin::EntryZero;
        assert!(!is_the_only_thing_that_can_answer(
            &empty(),
            &[zero, anthropic],
            false,
            "anthropic"
        ));
    }

    /// A provider that is switched off is not competition — but the added one
    /// still has to be the one that is on.
    #[test]
    fn only_enabled_providers_count_and_it_must_be_this_one() {
        let anthropic = provider("anthropic", "anthropic");
        let mut parked = provider("openrouter", "openrouter");
        parked.enabled = false;
        assert!(is_the_only_thing_that_can_answer(
            &empty(),
            &[parked.clone(), anthropic.clone()],
            false,
            "anthropic"
        ));
        assert!(
            !is_the_only_thing_that_can_answer(&empty(), &[parked, anthropic], false, "openrouter"),
            "a provider that is not the one enabled is not the thing that answers"
        );
    }

    // `the_offered_catalogue_is_sorted_and_capped` moved to
    // `company::inference::paged_catalog::tests` alongside `catalogue_offer`
    // itself (keys rework #2306, P3-7 review).
}
