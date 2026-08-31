//! The autonomy-tier write plane: `GET`/`PUT`/`DELETE {scope}/policy`
//! (issue #562).
//!
//! An operator drowning in approval cards had no way to stop it. The tier is
//! read from `[policy].mode` in the company manifest, and nothing in the console
//! read or wrote it — so the only ways to change it were editing a
//! version-controlled file and redeploying, or, on a hosted tenant where the
//! manifest is a read-only boot snapshot baked into the image, nothing at all.
//!
//! This is the company-scoped twin of the per-teammate budget surface in
//! [`team`](crate::server::ops::team), and it keeps the same three rules for the
//! same reasons:
//!
//! - **Admin-only, and attributed.** Loosening the approval gate is a privilege
//!   boundary at least as sharp as raising a spend cap, so both writes go through
//!   [`require_admin`](crate::server::users::admin::require_admin) and stamp who
//!   did it and when. The console renders that attribution: a tier that can be
//!   loosened anonymously is not much of a gate.
//! - **Absent is not empty.** An omitted key leaves that field on the manifest's
//!   value; `"alwaysApprove": []` is an operator deliberately clearing the
//!   always-ask list. Those are different stored states and different
//!   behaviours, so the body uses a double option and an empty `{}` is a `422`
//!   rather than a silent no-op.
//! - **Reset is its own verb.** `DELETE` drops the override so the manifest's
//!   `[policy]` applies again, which no `PUT` body can express — writing the
//!   manifest's *current* values would pin them, and the manifest can change
//!   under a rebuild.
//!
//! ## Why this writes an overlay and not the manifest
//!
//! A rebuild re-persists `record.manifest` **from the seed**, merging only
//! `[workflows].enabled`; every other field is seed-authoritative, *"and for
//! `[tools]` / `[policy]` that is a security property"* (`runtime::builder`). A
//! manifest write would be wiped by the next rebuild **and** would contradict
//! that invariant. So this route writes
//! [`PolicyOverride`](crate::ports::types::PolicyOverride), which
//! [`CompanyRecord::effective_policy`](crate::ports::types::CompanyRecord::effective_policy)
//! resolves *ahead* of the manifest at read time.
//!
//! ## When a change takes effect, stated precisely
//!
//! On the company's **next turn**, not on the turn already running.
//! `ApprovalPolicy` is built once per roster build, and
//! `HarnessPool::ensure` rebuilds the roster when the policy fingerprint moves
//! (issue #562's `effective_policy_fingerprint`). So the write lands, the next `ensure`
//! rebuilds, and the following turn runs on the new tier — an in-flight turn
//! finishes under the old one. Since "stop the flood **now**" is the motivating
//! complaint, the response says so rather than leaving an operator to discover
//! it: see [`PolicyDto::takes_effect`].

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::{POLICY_MODES, Policy};
use crate::error::OpenCompanyError;
use crate::policy::DEFAULT_TTL_MILLIS;
use crate::ports::now_millis;
use crate::ports::store::company_write_lock;
use crate::ports::types::{Actor, ActorKind, CompanyRecord, PolicyOverride};
use crate::server::error::ApiError;
use crate::server::ops::team::double_option;
use crate::server::ops::{ScopedCompany, scoped};
use crate::server::users::admin::require_admin;

/// What the console tells an operator about when a tier change bites.
///
/// A constant rather than prose invented at the call site, so the two write
/// routes and the read route cannot describe the timing differently.
///
/// `pub(crate)` for the same reason [`PolicyDto`] is: the GraphQL suite asserts
/// the field against **this** string rather than a copy of it, so a test cannot
/// keep passing while the two surfaces quote different timings at an operator.
pub(crate) const TAKES_EFFECT: &str =
    "on the next turn — a turn already running finishes under the previous tier";

/// Builds the policy route fragment.
pub fn router() -> Router<AppState> {
    scoped(
        "/policy",
        get(read_policy).put(set_policy).delete(clear_policy),
    )
}

/// One tier as the console renders it: the value, and what it means in the
/// operator's own words rather than in tier names.
///
/// The descriptions live here rather than in the frontend because they describe
/// *this* runtime's behaviour — `auto`'s line in particular is the contract
/// `Consequence::parks_under_auto` implements — and a copy in TypeScript would
/// drift from the gate it claims to describe.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TierDto {
    /// The `[policy].mode` word.
    pub(crate) value: &'static str,
    /// The operator-facing label.
    pub(crate) label: &'static str,
    /// What choosing it means, in consequences rather than in tier vocabulary.
    pub(crate) description: &'static str,
}

/// The operator-facing text for every tier this console knows how to describe,
/// in increasing order of autonomy.
///
/// **Not** what gets offered — [`selectable_tiers`] filters this by
/// [`POLICY_MODES`], so the console offers exactly what the runtime accepts.
/// The two are separate because they move independently: `auto` is issue #560,
/// landing in its own PR, and hard-coding it here would either offer a tier the
/// gate would silently downgrade to `supervised` (if #562 landed first) or need
/// a cross-PR edit to appear (if #560 did).
///
/// With the filter, neither happens: an entry ahead of the runtime is inert, and
/// the day `auto` joins `POLICY_MODES` it appears in the console with no further
/// change. An entry *behind* the runtime is the real error, and
/// `every_runtime_tier_has_console_text` fails on it.
const TIER_TEXT: &[TierDto] = &[
    TierDto {
        value: "readonly",
        label: "Read-only",
        description: "The agents can look at things but change nothing and spend nothing.",
    },
    TierDto {
        value: "supervised",
        label: "Supervised",
        description: "Conservative execution restrictions. Approval prompts are explicit through \
                      request_approval while policy HITL is disabled.",
    },
    TierDto {
        value: "auto",
        label: "Auto",
        description: "Balanced execution autonomy. Approval prompts are explicit through \
                      request_approval while policy HITL is disabled.",
    },
    TierDto {
        value: "full",
        label: "Full",
        description: "Broadest execution autonomy. Approval prompts are explicit through \
                      request_approval while policy HITL is disabled.",
    },
];

/// The tiers this company can actually be set to: [`TIER_TEXT`] narrowed to the
/// modes [`POLICY_MODES`] accepts, in `POLICY_MODES` order.
///
/// Driving the order from `POLICY_MODES` rather than from `TIER_TEXT` keeps one
/// list authoritative about what exists; `TIER_TEXT` is only authoritative about
/// how it reads.
fn selectable_tiers() -> Vec<&'static TierDto> {
    tiers_for(POLICY_MODES)
}

/// [`selectable_tiers`] over an explicit mode list.
///
/// Split out so the filter can be tested against a list that is not
/// `POLICY_MODES`. That is not ceremony: `TIER_TEXT` and `POLICY_MODES` now hold
/// the same four tiers (issue #560 landed `auto`), so a test driven off
/// `POLICY_MODES` alone can no longer tell a filtered list from an unfiltered
/// one — deleting the filter would leave every such assertion passing. A test
/// that has stopped discriminating looks exactly like coverage, which is worse
/// than no test at all.
fn tiers_for(modes: &[&str]) -> Vec<&'static TierDto> {
    modes
        .iter()
        .filter_map(|mode| TIER_TEXT.iter().find(|tier| tier.value == *mode))
        .collect()
}

/// The policy surface as the console reads it.
///
/// `pub(crate)` so the GraphQL layer can answer from **this** derivation rather
/// than recomputing the tier from the record (issue #1070). Two surfaces
/// resolving one value independently is how they come to disagree — a company
/// with a console override would report the overridden tier on one and the
/// manifest's on the other, and a caller has no way to tell which it got.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PolicyDto {
    /// The tier actually in force.
    pub(crate) mode: String,
    /// The always-ask list actually in force. The operator's real lever: it
    /// wins over every tier, `full` included.
    pub(crate) always_approve: Vec<String>,
    /// The spend threshold actually in force. `None` means every spend parks.
    pub(crate) auto_approve_under_usd: Option<f64>,
    /// The deadline actually in force, including the runtime default.
    pub(crate) approval_ttl_hours: u64,
    /// The manifest's tier, so the console can show what "reset" would restore
    /// rather than describing it abstractly.
    pub(crate) manifest_mode: String,
    /// The manifest's always-ask list, for the same reason.
    pub(crate) manifest_always_approve: Vec<String>,
    /// The manifest's spend threshold, before any console override.
    pub(crate) manifest_auto_approve_under_usd: Option<f64>,
    /// The manifest's deadline, if it explicitly names one.
    pub(crate) manifest_approval_ttl_hours: Option<u64>,
    /// Whether an operator override is in force. Distinct from comparing the
    /// values: an override that happens to match the manifest is still an
    /// override, and still what `DELETE` would remove.
    pub(crate) overridden: bool,
    /// Who set the override, if one is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) set_by: Option<String>,
    /// When it was set (epoch millis), if one is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) set_at_millis: Option<u64>,
    /// The selectable tiers with their operator-facing consequences.
    pub(crate) tiers: Vec<&'static TierDto>,
    /// When a change bites. Stated because "stop the flood now" is what an
    /// operator comes here to do, and this is not quite that.
    pub(crate) takes_effect: &'static str,
    /// Every tool name this build's approval gate can match, for the console's
    /// "is this a real tool?" note (issue #1423).
    ///
    /// The complete registry, not the granted-and-wired subset
    /// (`/workflows/tool-slugs`): the gate matches a tool call by name, so an
    /// entry naming a wired agent tool (`hosting_launch_site`,
    /// `publish_artifact`) is a legitimate fence and must not be called a
    /// mistake just because it cannot be a workflow node. Sourced from
    /// `consequence::declared_tools`, which
    /// `every_registered_tool_is_declared` proves covers every tool a live
    /// agent can call.
    pub(crate) known_tools: Vec<String>,
}

impl PolicyDto {
    pub(crate) fn build(record: &CompanyRecord) -> Self {
        let effective: Policy = record.effective_policy();
        let manifest = &record.manifest.policy;
        Self {
            mode: effective.mode,
            always_approve: effective.always_approve,
            auto_approve_under_usd: effective.auto_approve_under_usd,
            approval_ttl_hours: effective
                .approval_ttl_hours
                .unwrap_or(DEFAULT_TTL_MILLIS / (60 * 60 * 1000)),
            manifest_mode: manifest.mode.clone(),
            manifest_always_approve: manifest.always_approve.clone(),
            manifest_auto_approve_under_usd: manifest.auto_approve_under_usd,
            manifest_approval_ttl_hours: manifest.approval_ttl_hours,
            overridden: record.overlay_policy.is_some(),
            set_by: record.overlay_policy.as_ref().map(|o| o.set_by.id.clone()),
            set_at_millis: record.overlay_policy.as_ref().map(|o| o.at_millis),
            tiers: selectable_tiers(),
            takes_effect: TAKES_EFFECT,
            known_tools: {
                let mut tools: Vec<String> = crate::policy::consequence::declared_tools()
                    .map(str::to_owned)
                    .collect();
                // Deterministic over the wire; the console compares membership,
                // not order, but a stable list is easier to read and to test.
                tools.sort();
                tools
            },
        }
    }
}

/// The set-policy body.
///
/// Both fields are **double options** so "leave this alone" and "set it to this"
/// stay apart on the wire:
///
/// | body | parses as | means |
/// |---|---|---|
/// | `{"mode": "auto"}` | `Some(Some("auto"))` | run `auto`; leave the list alone |
/// | `{"alwaysApprove": []}` | `Some(Some([]))` | clear the always-ask list |
/// | `{"mode": null}` | `Some(None)` | stop overriding the tier |
/// | `{}` | *rejected* | — |
///
/// `#[serde(default)]` on each field makes an omitted key legal individually —
/// otherwise setting the tier would force the caller to restate the whole list —
/// but a body that sets *neither* is refused by [`set_policy`] rather than
/// stored, because a row with both fields `None` says nothing while still
/// rendering in the console as "overridden".
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetPolicy {
    #[serde(default, deserialize_with = "double_option")]
    mode: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    always_approve: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "double_option")]
    auto_approve_under_usd: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    approval_ttl_hours: Option<Option<u64>>,
}

/// `GET {scope}/policy` — the tier in force, what the manifest would restore,
/// and the selectable tiers with their consequences.
async fn read_policy(company: ScopedCompany) -> Result<Json<PolicyDto>, crate::server::Rejection> {
    let record = load_record(&company).await?;
    Ok(Json(PolicyDto::build(&record)))
}

/// `PUT {scope}/policy` — set the tier and/or the always-ask list. Admin-only,
/// attributed, in force on the next turn.
async fn set_policy(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(body): Json<SetPolicy>,
) -> Result<Json<PolicyDto>, crate::server::Rejection> {
    let admin = require_admin(&headers, &state, &company.runtime, peer).await?;

    if body.mode.is_none()
        && body.always_approve.is_none()
        && body.auto_approve_under_usd.is_none()
        && body.approval_ttl_hours.is_none()
    {
        return Err(refusal(
            "Nothing to set. Send a policy field — or `DELETE` \
             this endpoint to go back to the manifest's policy.",
        )
        .into());
    }

    // Validate against the same list `company.toml` is validated against, so a
    // tier the console accepts is one the manifest would have accepted too. An
    // A stored unknown mode falls back to the manifest for version-skew safety,
    // but a fresh write must still be refused: accepting it would leave the
    // console showing a tier the gate was not running.
    if let Some(Some(mode)) = &body.mode
        && !POLICY_MODES.contains(&mode.as_str())
    {
        return Err(refusal(&format!(
            "`mode` must be one of {} — you sent `{mode}`.",
            POLICY_MODES.join(", ")
        ))
        .into());
    }
    if let Some(Some(cap)) = body.auto_approve_under_usd
        && (!cap.is_finite() || cap < 0.0)
    {
        return Err(refusal("`autoApproveUnderUsd` must be a non-negative number.").into());
    }
    if let Some(Some(hours)) = body.approval_ttl_hours
        && !(1..=8_760).contains(&hours)
    {
        return Err(refusal("`approvalTtlHours` must be between 1 hour and 1 year.").into());
    }

    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(&company).await?;

    // Merge onto whatever is already stored, so setting the tier does not
    // silently discard an always-ask list a previous call set (and vice versa).
    // The outer `Option` is "did the caller mention this field"; the inner one is
    // the value or an explicit "stop overriding it".
    let held = record.overlay_policy.clone();
    let mode = match body.mode {
        Some(value) => value,
        None => held.as_ref().and_then(|o| o.mode.clone()),
    };
    let always_approve = match body.always_approve {
        Some(value) => value,
        None => held.as_ref().and_then(|o| o.always_approve.clone()),
    };
    let auto_approve_under_usd = match body.auto_approve_under_usd {
        Some(value) => Some(value),
        None => held.as_ref().and_then(|o| o.auto_approve_under_usd),
    };
    let approval_ttl_hours = match body.approval_ttl_hours {
        Some(Some(value)) => Some(value),
        // An explicit `null` clears this one override — restoring the
        // manifest/default deadline — while an omitted field keeps the held
        // value. `mode: null` and `alwaysApprove` already work this way.
        Some(None) => None,
        None => held.as_ref().and_then(|o| o.approval_ttl_hours),
    };

    let entry = PolicyOverride {
        mode,
        always_approve,
        auto_approve_under_usd,
        approval_ttl_hours,
        set_by: Actor {
            kind: ActorKind::User,
            id: admin.user_id,
        },
        at_millis: now_millis(),
    };
    // A merge that cleared both fields is a reset, and storing an empty row
    // would leave the console showing "overridden" over the manifest's own
    // values. `DELETE`'s outcome is the honest one.
    record.overlay_policy = (!entry.is_empty()).then_some(entry);

    save(&company, &record).await?;
    // The deadline is immediate, not next-turn: a parked card stays the same
    // request, but its deadline is re-evaluated against the current TTL each
    // time it is displayed, swept or resolved, so waiting for the next cycle
    // would let approvals parked under the old (longer) TTL outlive the deadline
    // the console just reported. The tier/cap/always-ask half is the safe-turn-
    // boundary one and is applied by `run_locked` at the start of the next
    // cycle — an in-flight turn must finish under the policy snapshot it started
    // with (issue #1455). A test-injected gate is exempt.
    if !company.runtime.gate_injected {
        company
            .runtime
            .approval_gate
            .apply_effective_ttl(&record.effective_policy());
    }
    Ok(Json(PolicyDto::build(&record)))
}

/// `DELETE {scope}/policy` — drop the override so the manifest's `[policy]`
/// applies again.
///
/// Not expressible as a `PUT`: writing the manifest's current values would pin
/// them, and a later `company.toml` edit would then be silently overridden by
/// the values it replaced. Deleting when nothing is stored is a no-op rather
/// than a `404` — the caller's intent ("this company should follow the
/// manifest") is already satisfied.
async fn clear_policy(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
) -> Result<Json<PolicyDto>, crate::server::Rejection> {
    require_admin(&headers, &state, &company.runtime, peer).await?;

    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(&company).await?;
    record.overlay_policy = None;
    save(&company, &record).await?;
    // Same split as `set_policy` (issue #1455): the manifest/default deadline
    // applies immediately — a parked card's deadline is re-evaluated against the
    // current TTL — while the manifest tier/cap/always-ask returns at the start
    // of the next cycle via `run_locked`.
    if !company.runtime.gate_injected {
        company
            .runtime
            .approval_gate
            .apply_effective_ttl(&record.effective_policy());
    }
    Ok(Json(PolicyDto::build(&record)))
}

fn refusal(message: &str) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, message.to_string()).into_response()
}

async fn load_record(company: &ScopedCompany) -> Result<CompanyRecord, crate::server::Rejection> {
    company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::CompanyNotFound(company.id().to_string()))
                .into_response()
                .into()
        })
}

async fn save(
    company: &ScopedCompany,
    record: &CompanyRecord,
) -> Result<(), crate::server::Rejection> {
    company
        .runtime
        .store()
        .save(record)
        .await
        .map_err(|e| ApiError(e).into_response().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::CompanyStore;
    use crate::ports::types::CompanyId;
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    const MANIFEST: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [policy]\nmode = \"supervised\"\n\
         always_approve = [\"payment.send\", \"filing.submit\"]\n\
         auto_approve_under_usd = 5.0\n\
         approval_ttl_hours = 24\n";

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-policy-")
            .tempdir()
            .expect("tempdir")
    }

    async fn state(home: &std::path::Path) -> AppState {
        let manifest: CompanyManifest = toml::from_str(MANIFEST).unwrap();
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
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn call(state: &AppState, method: &str, body: Option<Value>) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri("/api/v1/company/policy")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .header("content-type", "application/json");
        let request = match body {
            Some(value) => request.body(Body::from(value.to_string())).unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// `GET` reports the manifest's policy when nothing is overridden, and says
    /// so — `overridden` is what the console keys the reset control off.
    #[tokio::test]
    async fn get_reports_the_manifest_policy_when_nothing_is_overridden() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) = call(&state, "GET", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["mode"], "supervised");
        assert_eq!(body["manifestMode"], "supervised");
        assert_eq!(body["autoApproveUnderUsd"], 5.0);
        assert_eq!(body["manifestAutoApproveUnderUsd"], 5.0);
        assert_eq!(body["approvalTtlHours"], 24);
        assert_eq!(body["manifestApprovalTtlHours"], 24);
        assert_eq!(body["overridden"], false);
        assert!(body["setBy"].is_null());
        assert!(!body["tiers"].as_array().unwrap().is_empty());
    }

    /// `GET` also answers the console's "is this a real tool?" note: the
    /// complete gateable registry, not the workflow-authorable subset. A wired
    /// agent tool that cannot be a workflow node (`publish_artifact`,
    /// `hosting_launch_site`) is still a fence the gate matches, so it must be
    /// present — otherwise the console would call a working entry a mistake.
    #[tokio::test]
    async fn get_serves_the_complete_gateable_tool_registry() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) = call(&state, "GET", None).await;
        assert_eq!(status, StatusCode::OK);
        let known_tools: &Vec<Value> = body["knownTools"]
            .as_array()
            .expect("knownTools is an array");
        assert!(
            known_tools.iter().any(|tool| tool == "publish_artifact"),
            "a wired agent tool the workflow catalog does not carry must be known"
        );
        assert!(
            known_tools.iter().any(|tool| tool == "shell"),
            "the registry still carries the workflow tools"
        );
    }

    /// A tier `PUT` moves the tier and leaves the always-ask list on the
    /// manifest's value — the independence the console's two controls rely on.
    #[tokio::test]
    async fn putting_a_tier_leaves_the_always_ask_list_alone() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) = call(&state, "PUT", Some(json!({ "mode": "full" }))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["mode"], "full");
        assert_eq!(body["overridden"], true);
        assert_eq!(
            body["alwaysApprove"],
            json!(["payment.send", "filing.submit"]),
            "setting the tier must not discard the manifest's always-ask list"
        );
        // And it persisted, rather than only being reflected in the response.
        let (_, reread) = call(&state, "GET", None).await;
        assert_eq!(reread["mode"], "full");
    }

    /// An emptied always-ask list is stored as empty, not resolved back to the
    /// manifest's three defaults.
    #[tokio::test]
    async fn an_emptied_always_ask_list_is_stored_as_empty() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) = call(&state, "PUT", Some(json!({ "alwaysApprove": [] }))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["alwaysApprove"], json!([]));
        assert_eq!(body["mode"], "supervised", "the tier must not have moved");
    }

    /// The spend cap and deadline use the same field-wise write behaviour as
    /// the tier: changing either leaves every other policy setting alone.
    #[tokio::test]
    async fn putting_a_cap_or_deadline_overrides_only_that_field() {
        let dir = home();
        let state = state(dir.path()).await;

        let (status, body) = call(
            &state,
            "PUT",
            Some(json!({ "autoApproveUnderUsd": null, "approvalTtlHours": 72 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["autoApproveUnderUsd"].is_null());
        assert_eq!(body["approvalTtlHours"], 72);
        assert_eq!(body["mode"], "supervised");
        assert_eq!(
            body["alwaysApprove"],
            json!(["payment.send", "filing.submit"])
        );

        let (_, reread) = call(&state, "GET", None).await;
        assert!(reread["autoApproveUnderUsd"].is_null());
        assert_eq!(reread["approvalTtlHours"], 72);
    }

    /// The saved override reaches the live gate on the documented schedule
    /// (issue #1455): the deadline immediately — a parked card is re-checked
    /// against the current TTL each time it is displayed or swept — and the
    /// tier/cap/always-ask half at the next turn boundary, applied by
    /// `run_locked` so an in-flight turn finishes under the snapshot it started
    /// with.
    #[tokio::test]
    async fn a_policy_put_applies_the_deadline_immediately_and_the_cap_next_turn() {
        use crate::ports::types::CompanyEvent;

        let dir = home();
        let state = state(dir.path()).await;
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).expect("registered").clone();
        assert_eq!(runtime.approval_gate.ttl_millis(), 24 * 60 * 60 * 1000);

        // Deadline-only PUT: the live gate's TTL moves without waiting for a turn.
        let (status, body) = call(&state, "PUT", Some(json!({ "approvalTtlHours": 72 }))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["approvalTtlHours"], 72);
        assert_eq!(runtime.approval_gate.ttl_millis(), 72 * 60 * 60 * 1000);

        // Cap PUT: the snapshot must NOT move mid-turn...
        let (status, _) = call(&state, "PUT", Some(json!({ "autoApproveUnderUsd": 50 }))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            runtime.approval_gate.policy().auto_approve_under_usd,
            Some(5.0),
            "the evaluation snapshot must not move mid-turn"
        );

        // ...and the next turn applies the effective policy snapshot. Policy
        // HITL is disabled, but the stored cap still moves on its documented
        // schedule for reporting and any future opt-in policy mode.
        runtime
            .run_cycle(vec![CompanyEvent::ScheduleFired {
                cron: "* * * * *".to_string(),
                prompt: "status".to_string(),
            }])
            .await
            .expect("the next turn runs");
        assert_eq!(
            runtime.approval_gate.policy().auto_approve_under_usd,
            Some(50.0)
        );
    }

    /// A deadline `null` releases that one override while preserving the cap,
    /// just as `mode: null` releases only the tier override.
    #[tokio::test]
    async fn null_deadline_stops_overriding_the_deadline() {
        let dir = home();
        let state = state(dir.path()).await;
        call(
            &state,
            "PUT",
            Some(json!({ "autoApproveUnderUsd": 10, "approvalTtlHours": 72 })),
        )
        .await;

        let (status, body) = call(&state, "PUT", Some(json!({ "approvalTtlHours": null }))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["autoApproveUnderUsd"], 10.0);
        assert_eq!(body["approvalTtlHours"], 24);
    }

    /// A body that sets nothing is refused rather than stored, and an unknown
    /// tier is refused rather than silently downgraded to `supervised`.
    #[tokio::test]
    async fn an_empty_body_and_an_unknown_tier_are_both_refused() {
        let dir = home();
        let state = state(dir.path()).await;

        let (status, _) = call(&state, "PUT", Some(json!({}))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (status, _) = call(&state, "PUT", Some(json!({ "mode": "supervized" }))).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "an unknown tier must be refused — accepting it would leave the console \
             showing a tier the gate was not running"
        );

        for hours in [0, 8_761, u64::MAX] {
            let (status, _) = call(&state, "PUT", Some(json!({ "approvalTtlHours": hours }))).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "an invalid approval deadline must be refused before persistence"
            );
        }

        // Neither refusal stored anything.
        let (_, body) = call(&state, "GET", None).await;
        assert_eq!(body["overridden"], false);
    }

    /// `DELETE` restores the manifest's policy and is a no-op when nothing is
    /// stored.
    #[tokio::test]
    async fn delete_restores_the_manifest_policy() {
        let dir = home();
        let state = state(dir.path()).await;

        call(&state, "PUT", Some(json!({ "mode": "full" }))).await;
        let (status, body) = call(&state, "DELETE", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["mode"], "supervised");
        assert_eq!(body["overridden"], false);

        // Deleting again is a no-op, not a 404: the caller's intent is already
        // satisfied.
        let (status, _) = call(&state, "DELETE", None).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// Every tier the runtime accepts has console text, so none is
    /// unselectable (issue #562).
    ///
    /// **This assertion can no longer fail on its own, and that is recorded
    /// rather than hidden.** It compares `selectable_tiers()` against
    /// `POLICY_MODES`, and since #560 landed `auto` the text table holds exactly
    /// those four tiers — so deleting the `POLICY_MODES` filter entirely leaves
    /// this green. A revert-and-check confirmed it.
    ///
    /// It is kept because the invariant it states is real and will bite the next
    /// time a tier is added to `POLICY_MODES` without text. The filter itself is
    /// pinned by `a_tier_the_host_does_not_accept_is_not_offered`, which drives
    /// `tiers_for` from synthetic lists and does fail against that revert.
    ///
    /// Asserted in **one** direction on purpose. A `POLICY_MODES` entry with no
    /// text here is a tier an operator cannot pick — the same class of gap as a
    /// mode the manifest validator rejects, and equally invisible, since every
    /// other test would pass. The converse is harmless and deliberate: text for
    /// a tier the runtime has not gained yet (`auto`, issue #560, landing in its
    /// own PR) is filtered out by `selectable_tiers` and simply does not appear.
    /// Asserting that direction too would force the two PRs to be stacked.
    #[test]
    fn every_runtime_tier_has_console_text() {
        let offered: Vec<&str> = selectable_tiers().iter().map(|t| t.value).collect();
        assert_eq!(
            offered,
            POLICY_MODES.to_vec(),
            "a tier the runtime accepts has no console text, so an operator cannot select it"
        );
        for tier in selectable_tiers() {
            assert!(
                !tier.description.is_empty() && !tier.label.is_empty(),
                "tier `{}` has no operator-facing text, which is the whole point \
                 of showing tiers rather than mode names",
                tier.value
            );
        }
    }

    /// The text table stays a superset of the runtime's tiers, and every entry
    /// in it is a real mode name rather than a typo that would silently never
    /// render.
    #[test]
    fn console_text_names_only_plausible_tiers() {
        for tier in TIER_TEXT {
            assert!(
                POLICY_MODES.contains(&tier.value),
                "`{}` is not an accepted mode — a typo here silently drops the \
                 tier from the console",
                tier.value
            );
        }
    }

    /// The host's mode list decides what is offered, not the text table.
    ///
    /// Driven off synthetic lists rather than `POLICY_MODES`, and that is the
    /// whole point. `TIER_TEXT` and `POLICY_MODES` now hold the same four tiers,
    /// so an assertion built on `POLICY_MODES` cannot distinguish a filtered
    /// list from an unfiltered one, and deleting the filter would leave it
    /// green. This one fails.
    #[test]
    fn a_tier_the_host_does_not_accept_is_not_offered() {
        // A host without `auto` (every release before #560) must not be offered
        // it, even though the text for it is compiled in.
        let older_host = tiers_for(&["readonly", "supervised", "full"]);
        assert_eq!(
            older_host.iter().map(|t| t.value).collect::<Vec<_>>(),
            vec!["readonly", "supervised", "full"],
            "text for a tier the host does not accept must not be offered — the \
             gate would silently downgrade it to `supervised`"
        );

        // Order follows the host's list, not the text table's.
        let reordered = tiers_for(&["full", "readonly"]);
        assert_eq!(
            reordered.iter().map(|t| t.value).collect::<Vec<_>>(),
            vec!["full", "readonly"]
        );

        // A mode with no text is skipped rather than panicking or rendering
        // blank; `every_runtime_tier_has_console_text` is what stops that state
        // reaching a release.
        assert!(tiers_for(&["not_a_tier"]).is_empty());
    }
}
